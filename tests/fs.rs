//! Filesystem-level tests for root discovery, containment, scanning, and
//! advisory impact inspection (plan.md sections 3, 6.3, 10.4; issue #2).

use epocnote::advisory::{ImpactTarget, inspect_impact};
use epocnote::canon::{PathSource, canonicalize_with_host};
use epocnote::error::Error;
#[cfg(unix)]
use epocnote::root::validate_db_files;
use epocnote::root::{ContextStatus, context_dir, db_path, discover_root, validate_context_dir};
use epocnote::scan::{
    EntryKind, ExclusionReason, MAX_SOURCE_BYTES, ReadOutcome, SkipReason, scan_context,
    stable_read,
};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn canon(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap()
}

fn mk_context(root: &Path) -> PathBuf {
    let dir = context_dir(root);
    fs::create_dir_all(dir.join("notes")).unwrap();
    dir
}

fn kinds(scan: &[epocnote::scan::ScanEntry]) -> Vec<(String, EntryKind)> {
    scan.iter().map(|e| (e.rel_path.clone(), e.kind)).collect()
}

// ---------- root discovery (section 3.1) ----------

#[test]
fn explicit_root_used_directly() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    fs::create_dir_all(&root).unwrap();
    // An explicit --root is used as-is even without markers elsewhere.
    let found = discover_root(Some(&root), &tmp.path().join("elsewhere"), false).unwrap();
    assert_eq!(found, canon(&root));
}

#[test]
fn explicit_root_must_be_existing_directory() {
    let tmp = TempDir::new().unwrap();
    assert!(matches!(
        discover_root(Some(&tmp.path().join("missing")), tmp.path(), false),
        Err(Error::InvalidArgument(_))
    ));
    let file = tmp.path().join("file.txt");
    fs::write(&file, "x").unwrap();
    assert!(matches!(
        discover_root(Some(&file), tmp.path(), false),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn nearest_boundary_wins() {
    let tmp = TempDir::new().unwrap();
    let outer = tmp.path();
    fs::create_dir(outer.join(".git")).unwrap();
    let inner = outer.join("pkg");
    fs::create_dir_all(inner.join(".context")).unwrap();
    let deeper = inner.join("src/deep");
    fs::create_dir_all(&deeper).unwrap();
    // The nearer .context boundary beats the enclosing repository.
    let found = discover_root(None, &deeper, false).unwrap();
    assert_eq!(found, canon(&inner));
}

#[test]
fn git_file_worktree_marker() {
    let tmp = TempDir::new().unwrap();
    let worktree = tmp.path().join("wt");
    fs::create_dir(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: /somewhere/.git/worktrees/wt",
    )
    .unwrap();
    let found = discover_root(None, &worktree, false).unwrap();
    assert_eq!(found, canon(&worktree));
}

#[test]
fn missing_root_errors_unless_init() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare");
    fs::create_dir(&bare).unwrap();
    assert!(matches!(
        discover_root(None, &bare, false),
        Err(Error::RootNotFound)
    ));
    assert_eq!(discover_root(None, &bare, true).unwrap(), canon(&bare));
}

#[cfg(unix)]
#[test]
fn unsafe_context_entry_is_error_not_skip() {
    let tmp = TempDir::new().unwrap();
    let proj = tmp.path().join("proj");
    fs::create_dir(&proj).unwrap();
    fs::create_dir(proj.join(".git")).unwrap();
    std::os::unix::fs::symlink("/tmp", proj.join(".context")).unwrap();
    // A symlinked .context stops discovery even though .git also marks it.
    assert!(matches!(
        discover_root(None, &proj, false),
        Err(Error::UnsafePath(_))
    ));
}

#[test]
fn nondirectory_context_is_unsafe() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join(".context"), "not a dir").unwrap();
    assert!(matches!(
        discover_root(None, tmp.path(), false),
        Err(Error::UnsafePath(_))
    ));
}

// ---------- context validation (section 3.2) ----------

#[test]
fn validate_context_dir_states() {
    let tmp = TempDir::new().unwrap();
    assert!(matches!(
        validate_context_dir(tmp.path()).unwrap(),
        ContextStatus::NotInitialized
    ));
    mk_context(tmp.path());
    assert!(matches!(
        validate_context_dir(tmp.path()).unwrap(),
        ContextStatus::Ready
    ));
}

#[cfg(unix)]
#[test]
fn validate_db_rejects_symlinks() {
    let tmp = TempDir::new().unwrap();
    mk_context(tmp.path());
    fs::write(db_path(tmp.path()), b"x").unwrap();
    assert!(validate_db_files(tmp.path()).is_ok());
    fs::remove_file(db_path(tmp.path())).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", db_path(tmp.path())).unwrap();
    assert!(matches!(
        validate_db_files(tmp.path()),
        Err(Error::UnsafePath(_))
    ));
}

#[test]
fn worktree_indexes_are_per_worktree() {
    // The database always lives under the selected tree's .context, never in a
    // shared git directory (section 3.1).
    let tmp = TempDir::new().unwrap();
    let (w1, w2) = (tmp.path().join("w1"), tmp.path().join("w2"));
    fs::create_dir_all(&w1).unwrap();
    fs::create_dir_all(&w2).unwrap();
    assert!(db_path(&w1).starts_with(&w1));
    assert!(db_path(&w2).starts_with(&w2));
    assert!(db_path(&w1).ends_with(".context/.epocnote.sqlite"));
    assert_ne!(db_path(&w1), db_path(&w2));
}

// ---------- scanning (sections 3.2, 6.3, 10.4) ----------

#[test]
fn scan_collects_candidates_in_order() {
    let tmp = TempDir::new().unwrap();
    let ctx = mk_context(tmp.path());
    fs::write(ctx.join("architecture.md"), "# arch").unwrap();
    fs::write(ctx.join("notes/b.md"), "b").unwrap();
    fs::create_dir(ctx.join("notes/deep")).unwrap();
    fs::write(ctx.join("notes/deep/a.md"), "a").unwrap();
    fs::write(ctx.join("notes/readme.txt"), "not markdown").unwrap();
    fs::write(ctx.join("notes/up.MD"), "wrong case").unwrap();

    let scan = scan_context(tmp.path()).unwrap();
    let paths: Vec<_> = scan.iter().map(|e| e.rel_path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            ".context/architecture.md",
            ".context/notes/b.md",
            ".context/notes/deep/a.md",
            ".context/notes/readme.txt",
            ".context/notes/up.MD",
        ]
    );
    let map = kinds(&scan);
    assert!(matches!(map[0].1, EntryKind::Candidate { .. }));
    assert!(matches!(map[1].1, EntryKind::Candidate { .. }));
    assert!(matches!(map[2].1, EntryKind::Candidate { .. }));
    assert_eq!(map[3].1, EntryKind::Skip(SkipReason::NotMarkdown));
    assert_eq!(map[4].1, EntryKind::Skip(SkipReason::NotMarkdown));
}

#[test]
fn scan_ignores_db_and_tmp_files() {
    let tmp = TempDir::new().unwrap();
    let ctx = mk_context(tmp.path());
    fs::write(ctx.join(".epocnote.sqlite"), b"db").unwrap();
    fs::write(ctx.join(".epocnote.sqlite-wal"), b"wal").unwrap();
    fs::write(ctx.join("notes/.epocnote-tmp-a1b2"), b"partial").unwrap();
    let scan = scan_context(tmp.path()).unwrap();
    assert!(
        scan.iter()
            .all(|e| e.kind == EntryKind::Skip(SkipReason::Internal))
    );
}

#[test]
fn scan_exclusions() {
    let tmp = TempDir::new().unwrap();
    let ctx = mk_context(tmp.path());
    fs::write(
        ctx.join("notes/big.md"),
        vec![b'x'; MAX_SOURCE_BYTES as usize + 1],
    )
    .unwrap();
    let scan = scan_context(tmp.path()).unwrap();
    let map = kinds(&scan);
    assert_eq!(
        map.iter().find(|(p, _)| p.ends_with("big.md")).unwrap().1,
        EntryKind::Exclusion(ExclusionReason::Oversize)
    );
}

// A path over 1,024 bytes needs nested components: filesystems cap each
// component near 255 bytes, and macOS/Windows cap the full path below 1,024
// so only Linux can create this case at all.
#[cfg(target_os = "linux")]
#[test]
fn scan_deep_path_is_unsupported() {
    let tmp = TempDir::new().unwrap();
    let ctx = mk_context(tmp.path());
    let mut dir = ctx.join("notes");
    for _ in 0..5 {
        dir = dir.join("d".repeat(240));
    }
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("x.md"), "x").unwrap();
    let scan = scan_context(tmp.path()).unwrap();
    let map = kinds(&scan);
    assert_eq!(
        map.iter().find(|(p, _)| p.ends_with("x.md")).unwrap().1,
        EntryKind::Exclusion(ExclusionReason::UnsupportedPath)
    );
}

#[cfg(unix)]
#[test]
fn scan_skips_symlinks_and_specials() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    let ctx = mk_context(tmp.path());
    symlink("/tmp", ctx.join("notes/linkdir")).unwrap();
    symlink("/etc/hostname", ctx.join("notes/link.md")).unwrap();
    // A FIFO is a special file: skipped without opening it.
    let fifo = ctx.join("notes/pipe.md");
    std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    let scan = scan_context(tmp.path()).unwrap();
    let map = kinds(&scan);
    assert_eq!(
        map.iter().find(|(p, _)| p.ends_with("linkdir")).unwrap().1,
        EntryKind::Skip(SkipReason::Symlink)
    );
    assert_eq!(
        map.iter().find(|(p, _)| p.ends_with("link.md")).unwrap().1,
        EntryKind::Skip(SkipReason::Symlink)
    );
    assert_eq!(
        map.iter().find(|(p, _)| p.ends_with("pipe.md")).unwrap().1,
        EntryKind::Skip(SkipReason::Special)
    );
}

// Only Linux permits non-UTF-8 filenames; macOS and Windows reject them at
// creation, which the scan's lossy label never sees in practice.
#[cfg(target_os = "linux")]
#[test]
fn scan_non_utf8_name_is_unsupported_path() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let tmp = TempDir::new().unwrap();
    let ctx = mk_context(tmp.path());
    fs::write(
        ctx.join("notes")
            .join(OsStr::from_bytes(b"bad-\xFF-name.md")),
        "x",
    )
    .unwrap();
    let scan = scan_context(tmp.path()).unwrap();
    assert_eq!(scan.len(), 1);
    assert_eq!(
        scan[0].kind,
        EntryKind::Exclusion(ExclusionReason::UnsupportedPath)
    );
}

// ---------- stable reads (section 6.3 read rules) ----------

#[test]
fn stable_read_roundtrip_and_cap() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("note.md");
    fs::write(&f, "hello epocnote").unwrap();
    match stable_read(&f, 1024).unwrap() {
        ReadOutcome::Content { bytes, size, .. } => {
            assert_eq!(bytes, b"hello epocnote");
            assert_eq!(size, 14);
        }
        other => panic!("expected content, got {other:?}"),
    }
    assert!(matches!(stable_read(&f, 4).unwrap(), ReadOutcome::Oversize));
    assert!(matches!(
        stable_read(&tmp.path().join("missing.md"), 1024).unwrap(),
        ReadOutcome::Failed
    ));
}

#[cfg(unix)]
#[test]
fn stable_read_rejects_symlink_and_special() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("real.md");
    fs::write(&target, "x").unwrap();
    let link = tmp.path().join("link.md");
    symlink(&target, &link).unwrap();
    assert!(matches!(
        stable_read(&link, 1024).unwrap(),
        ReadOutcome::NotRegular
    ));
    let fifo = tmp.path().join("pipe");
    std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(matches!(
        stable_read(&fifo, 1024).unwrap(),
        ReadOutcome::NotRegular
    ));
}

// ---------- advisory impact inspection (section 3.3) ----------

#[test]
fn advisory_impact_inspection() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("src/auth")).unwrap();
    fs::write(tmp.path().join("src/auth/x.rs"), "fn x() {}").unwrap();
    let arg = |p: &str| canonicalize_with_host(p, PathSource::ToolArg, false).unwrap();
    assert_eq!(
        inspect_impact(tmp.path(), &arg("src/auth/x.rs")),
        ImpactTarget::Exists
    );
    assert_eq!(
        inspect_impact(tmp.path(), &arg("src")),
        ImpactTarget::Exists
    );
    assert_eq!(inspect_impact(tmp.path(), &arg("./")), ImpactTarget::Exists);
    assert_eq!(
        inspect_impact(tmp.path(), &arg("src/missing.rs")),
        ImpactTarget::Missing
    );
    assert_eq!(
        inspect_impact(tmp.path(), &arg("no/such/dir")),
        ImpactTarget::Missing
    );
}

#[cfg(unix)]
#[test]
fn advisory_impact_detects_symlink_component() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("real/dir")).unwrap();
    fs::write(tmp.path().join("real/dir/x.rs"), "x").unwrap();
    symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
    let arg = |p: &str| canonicalize_with_host(p, PathSource::ToolArg, false).unwrap();
    assert_eq!(
        inspect_impact(tmp.path(), &arg("link/dir/x.rs")),
        ImpactTarget::SymlinkComponent
    );
}
