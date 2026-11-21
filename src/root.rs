//! Project root discovery and context directory validation (plan.md sections
//! 3.1 and 3.2).

use crate::error::Error;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const CONTEXT_DIR: &str = ".context";
pub const NOTES_DIR: &str = "notes";
pub const DB_NAME: &str = ".epocnote.sqlite";
const DB_SIDECARS: [&str; 2] = [".epocnote.sqlite-wal", ".epocnote.sqlite-shm"];

pub fn context_dir(root: &Path) -> PathBuf {
    root.join(CONTEXT_DIR)
}

pub fn notes_dir(root: &Path) -> PathBuf {
    context_dir(root).join(NOTES_DIR)
}

pub fn db_path(root: &Path) -> PathBuf {
    context_dir(root).join(DB_NAME)
}

/// Resolve the project root (section 3.1). `explicit` is the `--root` value,
/// `cwd` the process working directory, and `init` permits falling back to the
/// cwd when no marker exists.
pub fn discover_root(explicit: Option<&Path>, cwd: &Path, init: bool) -> Result<PathBuf, Error> {
    if let Some(dir) = explicit {
        // Resolve to a canonical absolute path; no parent search.
        let resolved = fs::canonicalize(dir)
            .map_err(|e| Error::InvalidArgument(format!("--root {}: {e}", dir.display())))?;
        if !resolved.is_dir() {
            return Err(Error::InvalidArgument(format!(
                "--root {} is not a directory",
                dir.display()
            )));
        }
        return Ok(resolved);
    }

    let start = fs::canonicalize(cwd)?;
    for dir in start.ancestors() {
        let context = dir.join(CONTEXT_DIR);
        match fs::symlink_metadata(&context) {
            Ok(md) => {
                // An unsafe entry is an error, not a reason to keep walking.
                return validate_context_entry(dir, &context, &md);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // `.git` as a file (linked worktree, submodule) or directory marks a
        // project boundary even when .context is absent.
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
    }

    if init {
        Ok(start)
    } else {
        Err(Error::RootNotFound)
    }
}

fn validate_context_entry(dir: &Path, context: &Path, md: &fs::Metadata) -> Result<PathBuf, Error> {
    if md.file_type().is_symlink() {
        return Err(Error::UnsafePath(format!(
            "{} is a symlink",
            context.display()
        )));
    }
    if !md.is_dir() {
        return Err(Error::UnsafePath(format!(
            "{} is not a directory",
            context.display()
        )));
    }
    Ok(dir.to_path_buf())
}

pub enum ContextStatus {
    Ready,
    NotInitialized,
}

/// Validate `.context/` directly under the root (section 3.2). Absent is
/// `NotInitialized`; a symlink or non-directory is `UNSAFE_PATH`.
pub fn validate_context_dir(root: &Path) -> Result<ContextStatus, Error> {
    let context = context_dir(root);
    match fs::symlink_metadata(&context) {
        Ok(md) if md.file_type().is_symlink() => Err(Error::UnsafePath(format!(
            "{} is a symlink",
            context.display()
        ))),
        Ok(md) if md.is_dir() => Ok(ContextStatus::Ready),
        Ok(_) => Err(Error::UnsafePath(format!(
            "{} is not a directory",
            context.display()
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(ContextStatus::NotInitialized),
        Err(e) => Err(e.into()),
    }
}

/// The database and its sidecars must be regular files, never symlinks or
/// other redirections (section 3.2).
pub fn validate_db_files(root: &Path) -> Result<(), Error> {
    for name in [DB_NAME, DB_SIDECARS[0], DB_SIDECARS[1]] {
        let path = context_dir(root).join(name);
        match fs::symlink_metadata(&path) {
            Ok(md) if md.file_type().is_symlink() || !md.is_file() => {
                return Err(Error::UnsafePath(format!(
                    "{} is not a regular file",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
