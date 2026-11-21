//! `epocnote_note`: input validation, server-generated frontmatter, atomic
//! publication, and direct indexing of the published note (plan.md 9).

use crate::advisory::{ImpactTarget, inspect_impact};
use crate::canon::{self, CanonPath, PathSource};
use crate::db::{Index, map_error};
use crate::doc;
use crate::error::{Error, Warning};
use crate::scan::{self, MAX_SOURCE_BYTES, ReadOutcome};
use crate::sync;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_BYTES: usize = 10_240;
const MAX_IMPACTS: usize = 32;
const MAX_SUPERSEDES: usize = 8;
const MAX_PUBLISH_ATTEMPTS: usize = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteInput {
    pub title: String,
    pub body: String,
    pub impacts: Vec<String>,
    #[serde(default, deserialize_with = "crate::error::present")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "crate::error::present")]
    pub supersedes: Option<Vec<String>>,
}

#[derive(Debug, Serialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoteOutput {
    pub path: String,
    pub saved: bool,
    pub indexed: bool,
    pub warnings: Vec<Warning>,
    pub warnings_truncated: bool,
}

pub(crate) struct Parsed {
    title: String,
    body: String,
    impacts: Vec<CanonPath>,
    kind: &'static str,
    supersedes: Vec<CanonPath>,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidArgument(msg.into())
}

pub(crate) fn validate(input: NoteInput) -> Result<Parsed, Error> {
    // Newlines and control characters are rejected anywhere in the raw
    // title, including a trailing newline that trimming would erase.
    if input
        .title
        .chars()
        .any(|c| c == '\n' || c == '\r' || c.is_control())
    {
        return Err(invalid(
            "title must not contain newlines or control characters",
        ));
    }
    let title = input.title.trim();
    if title.is_empty() || input.title.chars().count() > MAX_TITLE_CHARS {
        return Err(invalid("title must be nonempty and at most 200 characters"));
    }
    // Normalize CRLF and lone CR before the size check (9.1).
    let body = input.body.replace("\r\n", "\n").replace('\r', "\n");
    if body.trim().is_empty() || body.len() > MAX_BODY_BYTES {
        return Err(invalid(format!(
            "body must be nonempty and at most {MAX_BODY_BYTES} UTF-8 bytes"
        )));
    }
    let kind = match input.kind.as_deref() {
        None | Some("note") => "note",
        Some("adr") => "adr",
        Some(_) => return Err(invalid("kind must be \"note\" or \"adr\"")),
    };
    if input.impacts.len() > MAX_IMPACTS {
        return Err(invalid(format!(
            "impacts accepts at most {MAX_IMPACTS} entries"
        )));
    }
    let mut impacts = Vec::new();
    for raw in &input.impacts {
        let c = canon::canonicalize(raw, PathSource::ToolArg)?;
        if !impacts.contains(&c) {
            impacts.push(c);
        }
    }
    let mut supersedes = Vec::new();
    if let Some(list) = &input.supersedes {
        if list.len() > MAX_SUPERSEDES {
            return Err(invalid(format!(
                "supersedes accepts at most {MAX_SUPERSEDES} entries"
            )));
        }
        for raw in list {
            let c = canon::canonicalize(raw, PathSource::ToolArg)?;
            if !supersedes.contains(&c) {
                supersedes.push(c);
            }
        }
    }
    Ok(Parsed {
        title: title.to_string(),
        body,
        impacts,
        kind,
        supersedes,
    })
}

/// Supersedes targets must canonicalize beneath `.context/`, name an
/// existing regular Markdown file, and have no symlinked ancestor (9.1,
/// 3.2) — a rejection, not a warning.
fn check_supersedes(root: &Path, parsed: &Parsed) -> Result<(), Error> {
    for c in &parsed.supersedes {
        let s = c.as_str();
        if !(s.starts_with(".context/") && s.ends_with(".md")) {
            return Err(invalid(format!(
                "supersedes target {s:?} is not a context document"
            )));
        }
        if inspect_impact(root, c) != ImpactTarget::Exists
            || !fs::symlink_metadata(root.join(s)).is_ok_and(|md| md.is_file())
        {
            return Err(invalid(format!(
                "supersedes target {s:?} is not an existing regular context document"
            )));
        }
    }
    Ok(())
}

fn yaml_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' | '\\' => out.push_str(&format!("\\{c}")),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Serialize the complete note document; frontmatter is produced by the
/// quoting rules above rather than by concatenating raw metadata (9.2).
fn document(parsed: &Parsed, created_at: &str) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("title: {}\n", yaml_string(&parsed.title)));
    out.push_str(&format!("kind: {}\n", parsed.kind));
    out.push_str("scope: paths\n");
    out.push_str(&format!("created_at: {}\n", yaml_string(created_at)));
    if parsed.impacts.is_empty() {
        out.push_str("impacts: []\n");
    } else {
        out.push_str("impacts:\n");
        for c in &parsed.impacts {
            out.push_str(&format!("  - {}\n", yaml_string(c.as_str())));
        }
    }
    if !parsed.supersedes.is_empty() {
        out.push_str("supersedes:\n");
        for c in &parsed.supersedes {
            out.push_str(&format!("  - {}\n", yaml_string(c.as_str())));
        }
    }
    out.push_str("---\n");
    out.push_str(&parsed.body);
    if !parsed.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in title.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = !out.is_empty();
        }
    }
    // Cap at 60 characters after building, so a separator never spills over.
    let out: String = out.chars().take(60).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "note".into() } else { out }
}

fn random_hex() -> Result<String, Error> {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| Error::Io(std::io::Error::other(e)))?;
    Ok(format!("{:016x}", u64::from_be_bytes(buf)))
}

fn notes_dir(root: &Path) -> PathBuf {
    root.join(".context").join("notes")
}

/// Write to a unique `.epocnote-tmp-*` file, then publish by hard link so the
/// destination never receives a partially written name (9.3). The link step
/// fails when the destination already exists. Once the link succeeds the note
/// exists: durability and cleanup failures return as warnings, never errors
/// that could misreport the note as unsaved or trigger a duplicate retry.
fn publish(
    root: &Path,
    slug: &str,
    bytes: &[u8],
    date: &str,
    cancelled: impl Fn() -> bool,
    mut random_suffix: impl FnMut() -> Result<String, Error>,
) -> Result<(String, Vec<Warning>), Error> {
    let dir = notes_dir(root);
    // The publication directory must be a real directory beneath a real
    // .context — a symlink here would publish outside the project (3.2).
    for d in [&root.join(".context"), &dir] {
        match fs::symlink_metadata(d) {
            Ok(md) if md.is_dir() && !crate::root::is_redirected(&md) => {}
            Ok(_) => {
                return Err(Error::UnsafePath(format!(
                    "{} is not a real directory",
                    d.display()
                )));
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let suffix = random_suffix()?;
        let tmp = dir.join(format!(".epocnote-tmp-{}", random_suffix()?));
        let dest_name = format!("{date}-{slug}-{suffix}.md");
        let dest = dir.join(&dest_name);
        {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&tmp).map_err(Error::Io)?;
            let r = f
                .write_all(bytes)
                .and_then(|()| f.sync_all())
                .map_err(Error::Io);
            if let Err(e) = r {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
        if cancelled() {
            let _ = fs::remove_file(&tmp);
            return Err(Error::Cancelled);
        }
        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                let path = format!(".context/notes/{dest_name}");
                let mut warnings = Vec::new();
                if let Err(e) = fs::remove_file(&tmp) {
                    warnings.push(Warning::new(
                        "CLEANUP_FAILED",
                        Some(path.clone()),
                        format!("note saved; could not remove its temporary file: {e}"),
                    ));
                }
                if let Err(e) = sync_dir(&dir) {
                    warnings.insert(
                        0,
                        Warning::new(
                            "DURABILITY",
                            Some(path.clone()),
                            format!("note saved but the directory could not be synchronized: {e}"),
                        ),
                    );
                }
                return Ok((path, warnings));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                let _ = fs::remove_file(&tmp);
                return Err(Error::AtomicPublishUnavailable);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(Error::Io(e));
            }
        }
    }
    Err(Error::NameCollision)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), Error> {
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(Error::Io)
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), Error> {
    Ok(())
}

/// Step 7 of 9.3: index the published path directly in one write
/// transaction. `indexed: true` only when a committed row carries the
/// publication hash — never substitute content that arrived after
/// publication.
pub(crate) fn index_published(
    index: &mut Index,
    root: &Path,
    path: &str,
    publication_hash: &str,
) -> Result<bool, Error> {
    {
        let tx = index
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_error)?;
        // A peer may already have indexed the same bytes.
        let stored: Option<String> = tx
            .query_row(
                "SELECT hash FROM files WHERE path = ?1",
                params![path],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_error)?;
        let outcome = scan::stable_read(&root.join(path), MAX_SOURCE_BYTES)?;
        let ReadOutcome::Content {
            bytes,
            size,
            mtime_ns,
        } = outcome
        else {
            return Ok(false);
        };
        if sync::hex_sha256(&bytes) != publication_hash {
            return Ok(false);
        }
        if stored.as_deref() != Some(publication_hash) {
            let text = std::str::from_utf8(&bytes).map_err(|_| Error::IndexCorrupt)?;
            let analyzed = doc::analyze(text, path);
            sync::replace_document(&tx, path, &analyzed, publication_hash, size, mtime_ns)
                .map_err(map_error)?;
        }
        tx.commit().map_err(map_error)?;
    }
    let committed: Option<String> = index
        .conn()
        .query_row(
            "SELECT hash FROM files WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_error)?;
    Ok(committed.as_deref() == Some(publication_hash))
}

/// Create, publish, and index a note (plan.md section 9).
pub fn create_note(root: &Path, index: &mut Index, input: NoteInput) -> Result<NoteOutput, Error> {
    create_prepared(
        root,
        validate(input)?,
        || false,
        |path, hash| index_published(index, root, path, hash),
    )
}

pub(crate) fn create_prepared(
    root: &Path,
    parsed: Parsed,
    cancelled: impl Fn() -> bool,
    index: impl FnOnce(&str, &str) -> Result<bool, Error>,
) -> Result<NoteOutput, Error> {
    check_supersedes(root, &parsed)?;
    let now = chrono::Utc::now();
    let text = document(
        &parsed,
        &now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    let publication_hash = sync::hex_sha256(text.as_bytes());
    let slug = slug(&parsed.title);
    let date = now.format("%Y-%m-%d").to_string();
    let (path, mut warnings) = publish(root, &slug, text.as_bytes(), &date, cancelled, random_hex)?;

    // Publication succeeded: every later failure is a warning, saved stays
    // true (9.4). Durability and indexing warnings precede advisory ones.
    let indexed = match index(&path, &publication_hash) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(Warning::new(
                "INDEXING_FAILED",
                Some(path.clone()),
                format!("note was saved but could not be indexed: {e}"),
            ));
            false
        }
    };
    if !indexed
        && !warnings
            .iter()
            .any(|warning| warning.code == "INDEXING_FAILED")
    {
        warnings.push(Warning::new(
            "INDEXING_FAILED",
            Some(path.clone()),
            "note was saved but its content changed before indexing",
        ));
    }
    let mut impacts = parsed.impacts;
    impacts.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for c in &impacts {
        let w = match inspect_impact(root, c) {
            ImpactTarget::Exists => None,
            ImpactTarget::Missing => Some("impact target does not currently exist"),
            ImpactTarget::SymlinkComponent => Some("impact target path crosses a symlink"),
            ImpactTarget::InspectFailed => Some("impact target could not be inspected"),
        };
        if let Some(m) = w {
            warnings.push(Warning::new("IMPACT_TARGET", Some(c.to_string()), m));
        }
    }
    let (warnings, warnings_truncated) = cap_warnings(warnings);
    Ok(NoteOutput {
        path,
        saved: true,
        indexed,
        warnings,
        warnings_truncated,
    })
}

/// Warning caps from 9.4/8.5, preserving the note's priority ordering:
/// durability and indexing warnings already precede advisory ones.
fn cap_warnings(mut ws: Vec<Warning>) -> (Vec<Warning>, bool) {
    for w in &mut ws {
        if w.message.chars().count() > 160 {
            w.message = w.message.chars().take(160).collect();
        }
    }
    let mut truncated = false;
    while ws.len() > 8 {
        ws.pop();
        truncated = true;
    }
    while !ws.is_empty()
        && serde_json::to_string(&ws)
            .map(|s| s.len())
            .unwrap_or(usize::MAX)
            > 4096
    {
        ws.pop();
        truncated = true;
    }
    (ws, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn cancellation_at_the_publication_boundary_removes_only_its_tempfile() {
        let root = tempfile::tempdir().unwrap();
        let dir = notes_dir(root.path());
        fs::create_dir_all(&dir).unwrap();
        let peer = dir.join(".epocnote-tmp-peer");
        fs::write(&peer, "peer data").unwrap();
        let checks = Cell::new(0);
        let result = publish(
            root.path(),
            "abort",
            b"staged data",
            "2026-01-01",
            || {
                checks.set(checks.get() + 1);
                if checks.get() == 2 {
                    assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
                    true
                } else {
                    false
                }
            },
            random_hex,
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(fs::read_to_string(peer).unwrap(), "peer data");
    }

    #[test]
    fn publication_retries_collisions_without_replacing_existing_notes() {
        let root = tempfile::tempdir().unwrap();
        let dir = notes_dir(root.path());
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("2026-01-01-collision-0000000000000000.md");
        fs::write(&first, "existing").unwrap();
        let mut suffixes = (0..).map(|n| format!("{n:016x}"));
        let (note, _) = publish(
            root.path(),
            "collision",
            b"new content",
            "2026-01-01",
            || false,
            || Ok(suffixes.next().unwrap()),
        )
        .unwrap();
        assert!(note.ends_with("-0000000000000002.md"));
        assert_eq!(fs::read_to_string(first).unwrap(), "existing");
        assert_eq!(
            fs::read_to_string(root.path().join(note)).unwrap(),
            "new content"
        );
        assert_eq!(fs::read_dir(dir).unwrap().count(), 2);
    }

    #[test]
    fn peer_hash_certifies_publication_until_the_file_is_edited() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(notes_dir(root.path())).unwrap();
        let mut index = Index::open(root.path()).unwrap();
        let path = ".context/notes/sample.md";
        let absolute = root.path().join(path);
        let original = b"Original content\n";
        fs::write(&absolute, original).unwrap();
        let hash = sync::hex_sha256(original);
        assert!(index_published(&mut index, root.path(), path, &hash).unwrap());
        assert!(index_published(&mut index, root.path(), path, &hash).unwrap());
        fs::write(&absolute, "Later content\n").unwrap();
        assert!(!index_published(&mut index, root.path(), path, &hash).unwrap());
        assert_eq!(fs::read_to_string(absolute).unwrap(), "Later content\n");
    }
}
