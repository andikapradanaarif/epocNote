//! Search-time synchronization: scan, compare, batched reconciliation, and
//! scan reuse (plan.md section 6.3).

use crate::db::{Index, map_error};
use crate::doc::{self, Analyzed};
use crate::error::{Error, Warning};
use crate::scan::{self, EntryKind, ExclusionReason, MAX_SOURCE_BYTES, ReadOutcome, SkipReason};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const BATCH_DOCS: usize = 100;
const BATCH_MS: u64 = 250;
const REUSE_CAP: Duration = Duration::from_secs(30);
const REUSE_FACTOR: u32 = 20;

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ScanReuse {
    /// Reuse a recent scan for 20× its duration, capped at 30 s.
    Auto,
    /// Scan on every call.
    Off,
}

/// What a caller needs after preparing the index (section 6.3 step 4).
#[derive(Debug)]
pub struct SyncOutcome {
    pub partial_index: bool,
    /// Documents still waiting for reconciliation, for INDEX_CATCHING_UP.
    pub remaining: usize,
    pub excluded_count: usize,
    pub warnings: Vec<Warning>,
    pub scan_reused: bool,
}

/// Process-local scan cache and reconciliation worker state.
pub struct IndexSync {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    state: Mutex<Shared>,
    condvar: Condvar,
}

#[derive(Default)]
struct Shared {
    cached: Option<CachedScan>,
    /// Diffs not yet drained by the worker.
    pending: VecDeque<Diff>,
    /// Items popped into an in-flight batch.
    inflight: usize,
    worker_running: bool,
    /// The last cycle's reconcile finished without failures or leftovers.
    complete: bool,
    had_failure: bool,
    /// Stable exclusions discovered at read time (section 6.5).
    extra_excluded: usize,
    warnings: Vec<Warning>,
    /// Test hook overriding the reuse window calculation.
    reuse_window: Option<Duration>,
}

struct CachedScan {
    finished: Instant,
    duration: Duration,
    /// Candidates only: `(rel path, size, mtime_ns)`.
    candidates: Vec<(String, u64, i64)>,
    excluded_count: usize,
    warnings: Vec<Warning>,
}

#[derive(Debug, Clone)]
enum Diff {
    Apply {
        path: String,
        size: u64,
        mtime_ns: i64,
    },
    Remove {
        path: String,
    },
}

/// Per-document reconciliation result; mutations always roll back together
/// through a savepoint.
#[derive(Debug, PartialEq, Eq)]
enum ApplyOutcome {
    /// Content applied, or the row already matched.
    Applied,
    /// Stable exclusion discovered at read time: counted, warned, no partial.
    Excluded,
    /// Read-level failure: old content retained, warned, partial_index.
    Failed,
}

impl IndexSync {
    pub fn new(root: &Path) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: root.to_path_buf(),
                state: Mutex::new(Shared::default()),
                condvar: Condvar::new(),
            }),
        }
    }

    #[doc(hidden)]
    pub fn with_reuse_window(root: &Path, window: Duration) -> Self {
        let s = Self::new(root);
        s.inner.state.lock().unwrap().reuse_window = Some(window);
        s
    }

    /// Step through section 6.3: attach to a running worker, reuse a fresh
    /// scan, or scan → diff → ensure a worker → wait at most `budget`.
    /// The caller then runs its query on `index`'s own connection.
    pub fn prepare_for_search(
        &self,
        index: &Index,
        reuse: ScanReuse,
        budget: Duration,
    ) -> Result<SyncOutcome, Error> {
        let inner = &self.inner;
        let mut st = inner.state.lock().unwrap();
        if st.worker_running {
            st = self.wait(st, budget);
            return Ok(self.outcome(&st, false));
        }
        if reuse == ScanReuse::Auto
            && let Some(c) = &st.cached
            && Instant::now() < c.finished + c.reuse_window(st.reuse_window)
        {
            return Ok(self.outcome(&st, true));
        }
        drop(st);

        // Step 1: scan, outside any transaction (section 6.3).
        let started = Instant::now();
        let entries = scan::scan_context(&inner.root)?;
        let mut cached = cache_scan(entries, started.elapsed());

        // Step 2: compare against the files table on this connection.
        let rows = load_file_stats(index.conn())?;
        let diffs = compute_diffs(&cached, &rows);

        let mut st = inner.state.lock().unwrap();
        st.warnings = std::mem::take(&mut cached.warnings);
        st.cached = Some(cached);
        st.extra_excluded = 0;
        st.had_failure = false;

        if diffs.is_empty() {
            // An unchanged corpus never takes the write lock.
            st.pending.clear();
            st.complete = true;
            return Ok(self.outcome(&st, false));
        }
        st.pending = diffs.into();
        st.complete = false;
        if !st.worker_running {
            st.worker_running = true;
            let inner = Arc::clone(inner);
            std::thread::spawn(move || worker(inner));
        }
        st = self.wait(st, budget);
        Ok(self.outcome(&st, false))
    }

    /// Wait until the worker drains or `budget` elapses.
    fn wait<'a>(&self, mut st: MutexGuard<'a, Shared>, budget: Duration) -> MutexGuard<'a, Shared> {
        let deadline = Instant::now() + budget;
        while st.worker_running {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let (guard, _) = self.inner.condvar.wait_timeout(st, left).unwrap();
            st = guard;
        }
        st
    }

    fn outcome(&self, st: &Shared, scan_reused: bool) -> SyncOutcome {
        let remaining = st.pending.len() + st.inflight;
        SyncOutcome {
            partial_index: remaining > 0 || st.worker_running || st.had_failure,
            remaining,
            excluded_count: st.cached.as_ref().map(|c| c.excluded_count).unwrap_or(0)
                + st.extra_excluded,
            warnings: st.warnings.clone(),
            scan_reused,
        }
    }
}

impl CachedScan {
    fn reuse_window(&self, override_window: Option<Duration>) -> Duration {
        override_window.unwrap_or_else(|| (self.duration * REUSE_FACTOR).min(REUSE_CAP))
    }
}

fn cache_scan(entries: Vec<scan::ScanEntry>, duration: Duration) -> CachedScan {
    let mut candidates = Vec::new();
    let mut excluded_count = 0usize;
    let mut warnings = Vec::new();
    for e in entries {
        match e.kind {
            EntryKind::Candidate { size, mtime_ns } => {
                candidates.push((e.rel_path, size, mtime_ns))
            }
            EntryKind::Exclusion(reason) => {
                excluded_count += 1;
                warnings.push(Warning::new(
                    "EXCLUDED",
                    Some(e.rel_path),
                    match reason {
                        ExclusionReason::Oversize => "file exceeds the 1 MiB source limit",
                        ExclusionReason::UnsupportedPath => "unsupported document path",
                        ExclusionReason::InvalidUtf8 => "file is not valid UTF-8",
                    },
                ));
            }
            EntryKind::Skip(SkipReason::Symlink) => warnings.push(Warning::new(
                "SYMLINK_SKIPPED",
                Some(e.rel_path),
                "symlink skipped without traversal",
            )),
            EntryKind::Skip(_) => {}
        }
    }
    CachedScan {
        finished: Instant::now(),
        duration,
        candidates,
        excluded_count,
        warnings,
    }
}

fn load_file_stats(conn: &Connection) -> Result<Vec<(String, i64, i64)>, Error> {
    let mut stmt = conn
        .prepare("SELECT path, size, mtime_ns FROM files ORDER BY path")
        .map_err(map_error)?;
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)
}

/// Merge the sorted candidate list with the sorted file rows into ordered
/// diffs (binary lexical path order, section 6.3 step 2).
fn compute_diffs(cached: &CachedScan, rows: &[(String, i64, i64)]) -> Vec<Diff> {
    let candidates: HashMap<&str, (u64, i64)> = cached
        .candidates
        .iter()
        .map(|(p, s, m)| (p.as_str(), (*s, *m)))
        .collect();
    let indexed: HashSet<&str> = rows.iter().map(|(p, _, _)| p.as_str()).collect();
    let mut diffs = Vec::new();
    for (path, size, mtime) in rows {
        match candidates.get(path.as_str()) {
            Some((s, m)) if *s == *size as u64 && *m == *mtime => {}
            Some((s, m)) => diffs.push(Diff::Apply {
                path: path.clone(),
                size: *s,
                mtime_ns: *m,
            }),
            None => diffs.push(Diff::Remove { path: path.clone() }),
        }
    }
    for (path, size, mtime) in &cached.candidates {
        if !indexed.contains(path.as_str()) {
            diffs.push(Diff::Apply {
                path: path.clone(),
                size: *size,
                mtime_ns: *mtime,
            });
        }
    }
    diffs.sort_by(|a, b| path_of(a).cmp(path_of(b)));
    diffs
}

fn path_of(d: &Diff) -> &str {
    match d {
        Diff::Apply { path, .. } | Diff::Remove { path } => path,
    }
}

// ---------- reconciliation worker (section 6.3 step 3) ----------

fn worker(inner: Arc<Inner>) {
    let mut index = match Index::open(&inner.root) {
        Ok(i) => i,
        Err(e) => {
            let mut st = inner.state.lock().unwrap();
            st.warnings.push(Warning::new(
                "RECONCILE_FAILED",
                None,
                format!("reconciliation could not open the index: {e}"),
            ));
            st.worker_running = false;
            inner.condvar.notify_all();
            return;
        }
    };
    loop {
        let batch: Vec<Diff> = {
            let mut st = inner.state.lock().unwrap();
            if st.pending.is_empty() {
                st.worker_running = false;
                st.complete = !st.had_failure;
                inner.condvar.notify_all();
                return;
            }
            let n = st.pending.len().min(BATCH_DOCS);
            let batch: Vec<Diff> = st.pending.drain(..n).collect();
            st.inflight += batch.len();
            batch
        };

        let committed = apply_batch(&mut index, &inner.root, &batch);
        let mut st = inner.state.lock().unwrap();
        st.inflight = st.inflight.saturating_sub(batch.len());
        match committed {
            Ok((summary, applied)) => {
                st.warnings.extend(summary.warnings);
                st.extra_excluded += summary.excluded;
                st.had_failure |= summary.failed;
                // Requeue the batch tail that did not fit in this window.
                for d in batch[applied..].iter().rev() {
                    st.pending.push_front(d.clone());
                }
            }
            Err((warnings, applied)) => {
                // Write-lock contention: committed batches remain; return the
                // unapplied items and stop, leaving them for the next scan.
                st.warnings.extend(warnings);
                for d in batch[applied..].iter().rev() {
                    st.pending.push_front(d.clone());
                }
                st.worker_running = false;
                st.complete = false;
                inner.condvar.notify_all();
                return;
            }
        }
        drop(st);
        inner.condvar.notify_all();
    }
}

#[derive(Default)]
struct BatchSummary {
    warnings: Vec<Warning>,
    excluded: usize,
    failed: bool,
}

/// One batch transaction: at most BATCH_DOCS documents, closed at the first
/// document boundary after BATCH_MS. `Ok` = committed; `Err` = stopped early,
/// leftovers go back to the queue for the next scan (section 6.3).
fn apply_batch(
    index: &mut Index,
    root: &Path,
    batch: &[Diff],
) -> Result<(BatchSummary, usize), (Vec<Warning>, usize)> {
    let mut out = BatchSummary::default();
    let mut applied = 0usize;
    let opened = Instant::now();
    let tx = match index
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
    {
        Ok(tx) => tx,
        Err(e) => {
            out.warnings.push(Warning::new(
                "INDEX_BUSY",
                None,
                format!(
                    "reconciliation could not take the write lock: {}",
                    map_error(e)
                ),
            ));
            return Err((out.warnings, applied));
        }
    };
    for d in batch {
        match apply_one(&tx, root, d, &mut out.warnings) {
            ApplyOutcome::Applied => {}
            ApplyOutcome::Excluded => out.excluded += 1,
            ApplyOutcome::Failed => out.failed = true,
        }
        applied += 1;
        if opened.elapsed() > Duration::from_millis(BATCH_MS) {
            break;
        }
    }
    match tx.commit() {
        Ok(()) => Ok((out, applied)),
        Err(e) => {
            out.warnings.push(Warning::new(
                "RECONCILE_FAILED",
                None,
                format!("reconciliation batch failed to commit: {}", map_error(e)),
            ));
            // The whole transaction rolled back: every popped item must go
            // back so it is retried and partial_index stays honest.
            Err((out.warnings, 0))
        }
    }
}

/// One document inside a batch transaction (section 6.3 steps 3.2–3.4).
fn apply_one(
    tx: &Transaction<'_>,
    root: &Path,
    d: &Diff,
    warns: &mut Vec<Warning>,
) -> ApplyOutcome {
    match d {
        Diff::Apply {
            path,
            size,
            mtime_ns,
        } => apply_candidate(tx, root, path, *size, *mtime_ns, warns),
        Diff::Remove { path } => apply_removal(tx, root, path, warns),
    }
}

fn apply_candidate(
    tx: &Transaction<'_>,
    root: &Path,
    path: &str,
    size: u64,
    mtime_ns: i64,
    warns: &mut Vec<Warning>,
) -> ApplyOutcome {
    // Recheck under the lock: another process may have reconciled it (3.2).
    let row: Option<(i64, i64, String)> = match tx
        .query_row(
            "SELECT size, mtime_ns, hash FROM files WHERE path = ?1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
    {
        Ok(row) => row,
        Err(e) => {
            warns.push(Warning::new(
                "READ_FAILED",
                Some(path.to_string()),
                format!("files lookup failed: {e}"),
            ));
            return ApplyOutcome::Failed;
        }
    };
    if let Some((s, m, _)) = &row
        && *s == size as i64
        && *m == mtime_ns
    {
        return ApplyOutcome::Applied;
    }

    let abs = root.join(path);
    let outcome = match scan::stable_read(&abs, MAX_SOURCE_BYTES) {
        Ok(o) => o,
        Err(e) => {
            warns.push(Warning::new(
                "READ_FAILED",
                Some(path.to_string()),
                format!("could not read document: {e}"),
            ));
            return ApplyOutcome::Failed;
        }
    };
    match outcome {
        ReadOutcome::Content {
            bytes,
            size,
            mtime_ns,
        } => {
            let hash = hex_sha256(&bytes);
            if let Some((_, _, old_hash)) = &row
                && *old_hash == hash
            {
                // Same bytes: update only the observed stats (6.3).
                return match tx.execute(
                    "UPDATE files SET size = ?2, mtime_ns = ?3 WHERE path = ?1",
                    params![path, size as i64, mtime_ns],
                ) {
                    Ok(_) => ApplyOutcome::Applied,
                    Err(e) => {
                        warns.push(Warning::new(
                            "WRITE_FAILED",
                            Some(path.to_string()),
                            format!("stats update failed: {e}"),
                        ));
                        ApplyOutcome::Failed
                    }
                };
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return stable_exclusion(tx, path, warns, "file is not valid UTF-8");
            };
            let analyzed = doc::analyze(text, path);
            match replace_document(tx, path, &analyzed, &hash, size, mtime_ns) {
                Ok(()) => {
                    warns.extend(analyzed.warnings.iter().cloned());
                    ApplyOutcome::Applied
                }
                Err(e) => {
                    warns.push(Warning::new(
                        "WRITE_FAILED",
                        Some(path.to_string()),
                        format!("document replace failed: {e}"),
                    ));
                    ApplyOutcome::Failed
                }
            }
        }
        ReadOutcome::Oversize => {
            stable_exclusion(tx, path, warns, "file exceeds the 1 MiB source limit")
        }
        ReadOutcome::NotRegular | ReadOutcome::Unstable | ReadOutcome::Failed => {
            // Retain prior content and stats; the next scan retries (6.3).
            warns.push(Warning::new(
                "READ_FAILED",
                Some(path.to_string()),
                "document could not be read stably",
            ));
            ApplyOutcome::Failed
        }
    }
}

/// Delete a document's indexed content and count it as a stable exclusion
/// (section 6.5).
fn stable_exclusion(
    tx: &Transaction<'_>,
    path: &str,
    warns: &mut Vec<Warning>,
    message: &'static str,
) -> ApplyOutcome {
    match delete_document(tx, path) {
        Ok(()) => {
            warns.push(Warning::new("EXCLUDED", Some(path.to_string()), message));
            ApplyOutcome::Excluded
        }
        Err(e) => {
            warns.push(Warning::new(
                "WRITE_FAILED",
                Some(path.to_string()),
                format!("document removal failed: {e}"),
            ));
            ApplyOutcome::Failed
        }
    }
}

fn apply_removal(
    tx: &Transaction<'_>,
    root: &Path,
    path: &str,
    warns: &mut Vec<Warning>,
) -> ApplyOutcome {
    // Confirm the path is still not an eligible candidate with one
    // no-follow stat before deleting (section 6.3 step 3.4).
    let abs = root.join(path);
    if let Ok(md) = std::fs::symlink_metadata(&abs)
        && md.is_file()
        && md.len() <= MAX_SOURCE_BYTES
        && path.ends_with(".md")
    {
        // An eligible regular file reappeared; the next scan handles it.
        return ApplyOutcome::Applied;
    }
    match delete_document(tx, path) {
        Ok(()) => ApplyOutcome::Applied,
        Err(e) => {
            warns.push(Warning::new(
                "WRITE_FAILED",
                Some(path.to_string()),
                format!("document removal failed: {e}"),
            ));
            ApplyOutcome::Failed
        }
    }
}

pub(crate) fn delete_document(tx: &Transaction<'_>, path: &str) -> Result<(), rusqlite::Error> {
    // Explicit chunk delete first; impacts/supersedes cascade (6.4).
    tx.execute("DELETE FROM chunks WHERE file_path = ?1", params![path])?;
    tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
    Ok(())
}

pub(crate) fn replace_document(
    tx: &Transaction<'_>,
    path: &str,
    a: &Analyzed,
    hash: &str,
    size: u64,
    mtime_ns: i64,
) -> Result<(), rusqlite::Error> {
    tx.execute("DELETE FROM chunks WHERE file_path = ?1", params![path])?;
    tx.execute("DELETE FROM impacts WHERE context_path = ?1", params![path])?;
    tx.execute(
        "DELETE FROM supersedes WHERE context_path = ?1",
        params![path],
    )?;
    tx.execute(
        "INSERT INTO files(path, hash, size, mtime_ns, title, kind, scope, created_at, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(path) DO UPDATE SET
           hash = excluded.hash, size = excluded.size, mtime_ns = excluded.mtime_ns,
           title = excluded.title, kind = excluded.kind, scope = excluded.scope,
           created_at = excluded.created_at, indexed_at = excluded.indexed_at",
        params![
            path,
            hash,
            size as i64,
            mtime_ns,
            a.title,
            a.kind.as_str(),
            a.scope.as_str(),
            a.created_at,
            chrono::Utc::now().timestamp()
        ],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO chunks(file_path, heading, start_line, end_line, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for c in &a.chunks {
            stmt.execute(params![
                path,
                c.heading,
                c.start_line as i64,
                c.end_line as i64,
                c.text
            ])?;
        }
    }
    {
        let mut stmt = tx.prepare("INSERT INTO impacts(context_path, target) VALUES (?1, ?2)")?;
        for c in &a.impacts {
            stmt.execute(params![path, c.as_str()])?;
        }
    }
    {
        let mut stmt =
            tx.prepare("INSERT INTO supersedes(context_path, target) VALUES (?1, ?2)")?;
        for c in &a.supersedes {
            stmt.execute(params![path, c.as_str()])?;
        }
    }
    Ok(())
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
