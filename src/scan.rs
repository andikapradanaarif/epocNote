//! The no-follow `.context/` walk and stable file reads used by reconciliation
//! (plan.md sections 3.2, 6.3, and 10.4).

use crate::error::Error;
use crate::root::{DB_NAME, context_dir};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Maximum source Markdown size (section 10.4).
pub const MAX_SOURCE_BYTES: u64 = 1_048_576; // 1 MiB
/// Maximum project-relative document path length (section 10.4).
pub const MAX_SOURCE_PATH_BYTES: usize = 1_024;
const TMP_PREFIX: &str = ".epocnote-tmp-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// A symlinked directory or file; warned, never traversed.
    Symlink,
    /// Device, socket, pipe, or another non-regular entry.
    Special,
    /// A regular file without the exact `.md` suffix.
    NotMarkdown,
    /// Temporary publication files and the database/sidecars; ignored.
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    Oversize,
    /// Not valid UTF-8 or longer than `MAX_SOURCE_PATH_BYTES`.
    UnsupportedPath,
    /// Bytes did not decode as UTF-8 (decided at read time).
    InvalidUtf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Candidate { size: u64, mtime_ns: i64 },
    Skip(SkipReason),
    Exclusion(ExclusionReason),
}

#[derive(Debug, Clone)]
pub struct ScanEntry {
    /// Path relative to the project root (`-`-joined from `.context/`); for a
    /// non-UTF-8 name this is a lossy label used only for warnings, never as a
    /// database key.
    pub rel_path: String,
    pub kind: EntryKind,
}

/// Walk `.context/` without following symlinks, in binary lexical path order
/// (section 6.3 step 1). An incomplete traversal is an error, never evidence
/// of deletion.
pub fn scan_context(root: &Path) -> Result<Vec<ScanEntry>, Error> {
    let base = context_dir(root);
    let mut out = Vec::new();
    visit(&base, ".context", &mut out)?;
    Ok(out)
}

fn visit(dir: &Path, rel_dir: &str, out: &mut Vec<ScanEntry>) -> Result<(), Error> {
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let name = child.file_name();
        let name_str = name.to_string_lossy();
        let rel = format!("{rel_dir}/{name_str}");
        let md = fs::symlink_metadata(child.path())?;

        if md.file_type().is_symlink() {
            out.push(entry(rel, EntryKind::Skip(SkipReason::Symlink)));
            continue;
        }
        if md.is_dir() {
            visit(&child.path(), &rel, out)?;
            continue;
        }
        if !md.is_file() {
            out.push(entry(rel, EntryKind::Skip(SkipReason::Special)));
            continue;
        }

        let name_bytes = name_bytes(&name);
        if name_str.starts_with(TMP_PREFIX) || name_str.starts_with(DB_NAME) {
            out.push(entry(rel, EntryKind::Skip(SkipReason::Internal)));
            continue;
        }
        if !name_bytes.ends_with(b".md") {
            out.push(entry(rel, EntryKind::Skip(SkipReason::NotMarkdown)));
            continue;
        }
        // A document path must be lossless UTF-8 within the byte limit.
        if name.to_str().is_none() || rel.len() > MAX_SOURCE_PATH_BYTES {
            out.push(entry(
                rel,
                EntryKind::Exclusion(ExclusionReason::UnsupportedPath),
            ));
            continue;
        }
        if md.len() > MAX_SOURCE_BYTES {
            out.push(entry(rel, EntryKind::Exclusion(ExclusionReason::Oversize)));
            continue;
        }
        out.push(entry(
            rel,
            EntryKind::Candidate {
                size: md.len(),
                mtime_ns: mtime_ns(&md),
            },
        ));
    }
    Ok(())
}

fn entry(rel_path: String, kind: EntryKind) -> ScanEntry {
    ScanEntry { rel_path, kind }
}

#[cfg(unix)]
fn name_bytes(name: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes()
}

#[cfg(not(unix))]
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    name.to_string_lossy().as_bytes().to_vec()
}

fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Result of a bounded, stability-checked read (section 6.3 read rules).
#[derive(Debug)]
pub enum ReadOutcome {
    /// The file's bytes together with the stats that describe them.
    Content {
        bytes: Vec<u8>,
        size: u64,
        mtime_ns: i64,
    },
    /// The open path is a symlink or a non-regular file.
    NotRegular,
    /// Stat observations changed across the read; retry next scan.
    Unstable,
    /// The first stat already exceeds the cap; a stable exclusion.
    Oversize,
    /// The file has no stable content for this attempt (vanished mid-open).
    Failed,
}

/// Open without following symlinks where the platform supports it, confirm a
/// regular file from the handle, read at most `cap + 1` bytes, and re-stat. The
/// returned stats always describe the bytes that were read.
pub fn stable_read(path: &Path, cap: u64) -> io::Result<ReadOutcome> {
    let file = match open_nofollow(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ReadOutcome::Failed),
        Err(e) => {
            if is_eloop(&e) {
                return Ok(ReadOutcome::NotRegular);
            }
            return Err(e);
        }
    };

    let before = file.metadata()?;
    if !before.is_file() {
        return Ok(ReadOutcome::NotRegular);
    }
    if before.len() > cap {
        return Ok(ReadOutcome::Oversize);
    }
    let (size_before, mtime_before) = (before.len(), mtime_ns(&before));

    let mut bytes = Vec::new();
    (&file).take(cap + 1).read_to_end(&mut bytes)?;

    let after = file.metadata()?;
    if !after.is_file()
        || after.len() != size_before
        || mtime_ns(&after) != mtime_before
        || after.len() > cap
        || bytes.len() as u64 > cap
    {
        return Ok(ReadOutcome::Unstable);
    }
    Ok(ReadOutcome::Content {
        bytes,
        size: size_before,
        mtime_ns: mtime_before,
    })
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(windows)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    // Windows has no O_NOFOLLOW; junction-type redirections are already
    // excluded at the symlink_metadata stage before this open.
    File::open(path)
}

#[cfg(all(not(unix), not(windows)))]
fn open_nofollow(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn is_eloop(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_eloop(_: &io::Error) -> bool {
    false
}
