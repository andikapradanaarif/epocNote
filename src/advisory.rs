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
    if canonical.is_root() {
        return ImpactTarget::Exists;
    }
    let path = canonical.as_str();
    for end in path
        .match_indices('/')
        .map(|(end, _)| end)
        .chain([path.len()])
    {
        match fs::symlink_metadata(root.join(&path[..end])) {
            Ok(md) if crate::root::is_redirected(&md) => return ImpactTarget::SymlinkComponent,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                return ImpactTarget::Missing;
            }
            Err(_) => return ImpactTarget::InspectFailed,
        }
    }
    ImpactTarget::Exists
}
