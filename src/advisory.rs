//! Advisory inspection of impact targets during note creation (plan.md section
//! 3.3). Results are warnings only; they never change the recorded path and an
//! impact is never opened for contents.

use crate::canon::CanonPath;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactTarget {
    Exists,
    /// The target (or an ancestor component) does not exist.
    Missing,
    /// A path component on the way to the target is a symlink.
    SymlinkComponent,
    /// Inspection itself failed (permissions, I/O errors).
    InspectFailed,
}

/// Inspect the canonical impact under `root` component by component without
/// dereferencing symlinks.
pub fn inspect_impact(root: &Path, canonical: &CanonPath) -> ImpactTarget {
    let mut current = root.to_path_buf();
    for comp in canonical.components() {
        current.push(comp);
        match fs::symlink_metadata(&current) {
            Ok(md) if md.file_type().is_symlink() => return ImpactTarget::SymlinkComponent,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                return ImpactTarget::Missing;
            }
            Err(_) => return ImpactTarget::InspectFailed,
        }
    }
    ImpactTarget::Exists
}
