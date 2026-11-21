//! Search-time synchronization: scan, compare, batched reconciliation, and
//! scan reuse (plan.md section 6.3).

use crate::db::{FileIdentity, Index, map_error};
use crate::doc::{self, Analyzed};
use crate::error::{Error, Warning, fit_warnings};
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

#[doc(hidden)]
pub struct SyncTimings {
    pub scan_duration: Duration,
    pub max_write_lock: Duration,
    pub worker_running: bool,
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
    had_failure: bool,
    /// Stable exclusions discovered at read time (section 6.5).
    extra_excluded: usize,
    warnings: Vec<Warning>,
    max_write_lock: Duration,
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

#[derive(Debug)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReconcileMode {
    Changed,
    Verify,
}

/// Per-document reconciliation result. A file read failure leaves its row intact.
#[derive(Debug, PartialEq, Eq)]
enum ApplyOutcome {
    /// Content applied, or the row already matched.
    Applied,
    /// Stable exclusion discovered at read time: counted, warned, no partial.
    Excluded,
    /// Read-level failure: old content retained, warned, partial_index.
    Failed,
}

pub struct RebuildReport {
    pub indexed_count: i64,
    pub excluded_count: usize,
    pub failed_count: usize,
    pub warnings: Vec<Warning>,
    pub warnings_truncated: bool,
}

/// Verify the index in place, including files whose size and mtime agree.
pub fn rebuild(root: &Path) -> Result<RebuildReport, Error> {
    let mut index = Index::open(root)?;
    // A complete scan is required before making any reconciliation changes.
    let started = Instant::now();
    let entries = scan::scan_context(root)?;
    let cached = cache_scan(entries, started.elapsed());
    let rows = load_file_stats(index.conn())?;
    let diffs = compute_diffs(&cached, &rows, ReconcileMode::Verify);

    let mut excluded_count = cached.excluded_count;
    let mut failed_count = 0;
    let (mut warnings, mut warnings_truncated) = fit_warnings(cached.warnings);
    let mut offset = 0;
    while offset < diffs.len() {
        let end = (offset + BATCH_DOCS).min(diffs.len());
        let (summary, applied, _) =
            apply_batch(&mut index, root, &diffs[offset..end], ReconcileMode::Verify)?;
        excluded_count += summary.excluded;
        failed_count += summary.failed;
        warnings.extend(summary.warnings);
        let (bounded, truncated) = fit_warnings(warnings);
        warnings = bounded;
        warnings_truncated |= truncated;
        offset += applied;
    }
    let indexed_count = index
        .conn()
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        .map_err(map_error)?;
    Ok(RebuildReport {
        indexed_count,
        excluded_count,
        failed_count,
        warnings,
        warnings_truncated,
    })
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
    pub fn timings(&self) -> SyncTimings {
        let state = self.inner.state.lock().unwrap();
        SyncTimings {
            scan_duration: state
                .cached
                .as_ref()
                .map_or(Duration::ZERO, |scan| scan.duration),
            max_write_lock: state.max_write_lock,
            worker_running: state.worker_running,
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
        self.prepare_until(index, reuse, Instant::now() + budget)
    }

    pub(crate) fn prepare_until(
        &self,
        index: &Index,
        reuse: ScanReuse,
        deadline: Instant,
    ) -> Result<SyncOutcome, Error> {
        let inner = &self.inner;
        let mut st = inner.state.lock().unwrap();
        if reuse == ScanReuse::Auto
            && let Some(c) = &st.cached
            && Instant::now() < c.finished + c.reuse_window(st.reuse_window)
        {
            return Ok(self.outcome(&st, true));
        }
        if st.worker_running {
            st = self.wait(st, deadline);
            return Ok(self.outcome(&st, false));
        }
        drop(st);

        // Step 1: scan, outside any transaction (section 6.3).
        let started = Instant::now();
        let entries = scan::scan_context(&inner.root)?;
        let mut cached = cache_scan(entries, started.elapsed());

        // Step 2: compare against the files table on this connection.
        let rows = load_file_stats(index.conn())?;
        let diffs = compute_diffs(&cached, &rows, ReconcileMode::Changed);

        let mut st = inner.state.lock().unwrap();
        if st.worker_running {
            st = self.wait(st, deadline);
            return Ok(self.outcome(&st, false));
        }
        st.warnings = std::mem::take(&mut cached.warnings);
        st.cached = Some(cached);
        st.extra_excluded = 0;
        st.had_failure = false;

        if diffs.is_empty() {
            // An unchanged corpus never takes the write lock.
            st.pending.clear();
            return Ok(self.outcome(&st, false));
        }
        st.pending = diffs.into();
        st.worker_running = true;
        let inner = Arc::clone(inner);
        let identity = index.identity();
        std::thread::spawn(move || worker(inner, identity));
        st = self.wait(st, deadline);
        Ok(self.outcome(&st, false))
    }

    /// Wait until the worker drains or the search's original deadline expires.
    fn wait<'a>(
        &self,
        mut st: MutexGuard<'a, Shared>,
        deadline: Instant,
    ) -> MutexGuard<'a, Shared> {
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
fn compute_diffs(
    cached: &CachedScan,
    rows: &[(String, i64, i64)],
    mode: ReconcileMode,
) -> Vec<Diff> {
    let candidates: HashMap<&str, (u64, i64)> = cached
        .candidates
        .iter()
        .map(|(p, s, m)| (p.as_str(), (*s, *m)))
        .collect();
    let indexed: HashSet<&str> = rows.iter().map(|(p, _, _)| p.as_str()).collect();
    let mut diffs = Vec::new();
    for (path, size, mtime) in rows {
        match candidates.get(path.as_str()) {
            Some((s, m))
                if mode == ReconcileMode::Changed && *s == *size as u64 && *m == *mtime => {}
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

fn worker(inner: Arc<Inner>, identity: Option<FileIdentity>) {
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
                inner.condvar.notify_all();
                return;
            }
            let n = st.pending.len().min(BATCH_DOCS);
            let batch: Vec<Diff> = st.pending.drain(..n).collect();
            st.inflight += batch.len();
            batch
        };

        let committed =
            if identity.is_some() && (index.identity() != identity || !index.still_current()) {
                Err(Error::Io(std::io::Error::other(
                    "index replaced during reconciliation",
                )))
            } else {
                apply_batch(&mut index, &inner.root, &batch, ReconcileMode::Changed)
            };
        let mut st = inner.state.lock().unwrap();
        st.inflight = st.inflight.saturating_sub(batch.len());
        match committed {
            Ok((summary, applied, lock_duration)) => {
                st.max_write_lock = st.max_write_lock.max(lock_duration);
                st.warnings.extend(summary.warnings);
                st.extra_excluded += summary.excluded;
                st.had_failure |= summary.failed > 0;
                // Requeue the batch tail that did not fit in this window.
                for d in batch.into_iter().skip(applied).rev() {
                    st.pending.push_front(d);
                }
            }
            Err(error) => {
                // The whole batch rolled back. Leave its items for the next scan.
                let code = match error {
                    Error::IndexBusy => "INDEX_BUSY",
                    _ => "RECONCILE_FAILED",
                };
                st.warnings.push(Warning::new(
                    code,
                    None,
                    format!("reconciliation stopped: {error}"),
                ));
                for d in batch.into_iter().rev() {
                    st.pending.push_front(d);
                }
                st.worker_running = false;
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
    failed: usize,
}

/// One batch transaction: at most BATCH_DOCS documents, closed at the first
/// document boundary after BATCH_MS. On a database error the batch rolls back.
fn apply_batch(
    index: &mut Index,
    root: &Path,
    batch: &[Diff],
    mode: ReconcileMode,
) -> Result<(BatchSummary, usize, Duration), Error> {
    let mut out = BatchSummary::default();
    let mut applied = 0usize;
    let tx = index
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let opened = Instant::now();
    for d in batch {
        match apply_one(&tx, root, d, mode, &mut out.warnings)? {
            ApplyOutcome::Applied => {}
            ApplyOutcome::Excluded => out.excluded += 1,
            ApplyOutcome::Failed => out.failed += 1,
        }
        applied += 1;
        if opened.elapsed() >= Duration::from_millis(BATCH_MS) {
            break;
        }
    }
    tx.commit().map_err(map_error)?;
    Ok((out, applied, opened.elapsed()))
}

/// One document inside a batch transaction (section 6.3 steps 3.2–3.4).
fn apply_one(
    tx: &Transaction<'_>,
    root: &Path,
    d: &Diff,
    mode: ReconcileMode,
    warns: &mut Vec<Warning>,
) -> Result<ApplyOutcome, Error> {
    match d {
        Diff::Apply {
            path,
            size,
            mtime_ns,
        } => {
            let scanned_stats = (mode == ReconcileMode::Changed).then_some((*size, *mtime_ns));
            apply_candidate(tx, root, path, scanned_stats, warns)
        }
        Diff::Remove { path } => apply_removal(tx, root, path, warns),
    }
}

fn apply_candidate(
    tx: &Transaction<'_>,
    root: &Path,
    path: &str,
    scanned_stats: Option<(u64, i64)>,
    warns: &mut Vec<Warning>,
) -> Result<ApplyOutcome, Error> {
    // Recheck under the lock: another process may have reconciled it (3.2).
    let row: Option<(i64, i64, String)> = tx
        .query_row(
            "SELECT size, mtime_ns, hash FROM files WHERE path = ?1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(map_error)?;
    if let Some((s, m, _)) = &row
        && scanned_stats == Some((*s as u64, *m))
    {
        return Ok(ApplyOutcome::Applied);
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
            return Ok(ApplyOutcome::Failed);
        }
    };
    match outcome {
        ReadOutcome::Content {
            bytes,
            size,
            mtime_ns,
        } => {
            let hash = hex_sha256(&bytes);
            if let Some((old_size, old_mtime, old_hash)) = &row
                && *old_hash == hash
            {
                // Same bytes: update only the observed stats (6.3).
                if (*old_size, *old_mtime) != (size as i64, mtime_ns) {
                    tx.execute(
                        "UPDATE files SET size = ?2, mtime_ns = ?3 WHERE path = ?1",
                        params![path, size as i64, mtime_ns],
                    )
                    .map_err(map_error)?;
                }
                return Ok(ApplyOutcome::Applied);
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return stable_exclusion(tx, path, warns, "file is not valid UTF-8");
            };
            let analyzed = doc::analyze(text, path);
            replace_document(tx, path, &analyzed, &hash, size, mtime_ns).map_err(map_error)?;
            warns.extend(analyzed.warnings);
            Ok(ApplyOutcome::Applied)
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
            Ok(ApplyOutcome::Failed)
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
) -> Result<ApplyOutcome, Error> {
    delete_document(tx, path).map_err(map_error)?;
    warns.push(Warning::new("EXCLUDED", Some(path.to_string()), message));
    Ok(ApplyOutcome::Excluded)
}

fn apply_removal(
    tx: &Transaction<'_>,
    root: &Path,
    path: &str,
    warns: &mut Vec<Warning>,
) -> Result<ApplyOutcome, Error> {
    // Confirm the path is still ineligible before deleting (section 6.3).
    match scan::is_candidate(root, path) {
        Ok(true) => {} // A file reappeared; the next scan handles it.
        Ok(false) => delete_document(tx, path).map_err(map_error)?,
        Err(e) => {
            warns.push(Warning::new(
                "READ_FAILED",
                Some(path.to_string()),
                format!("could not confirm document removal: {e}"),
            ));
            return Ok(ApplyOutcome::Failed);
        }
    }
    Ok(ApplyOutcome::Applied)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn exhausted_search_deadline_does_not_wait_for_reconciliation() {
        let root = tempfile::tempdir().unwrap();
        let context = root.path().join(".context");
        fs::create_dir(&context).unwrap();
        fs::write(context.join("new.md"), "Ready to index\n").unwrap();

        let index = Index::open(root.path()).unwrap();
        let mut writer = Index::open(root.path()).unwrap();
        let lock = writer
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let sync = IndexSync::new(root.path());

        let deadline = Instant::now();
        let outcome = sync
            .prepare_until(&index, ScanReuse::Off, deadline)
            .unwrap();
        assert!(outcome.partial_index);
        assert_eq!(outcome.remaining, 1);
        assert!(deadline.elapsed() < Duration::from_secs(1));

        lock.rollback().unwrap();
        let finished = sync
            .prepare_for_search(&index, ScanReuse::Off, Duration::from_secs(5))
            .unwrap();
        assert!(!finished.partial_index);
    }
}
