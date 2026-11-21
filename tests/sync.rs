//! Search-time synchronization: scan → diff → batched reconcile → reuse
//! (plan.md section 6.3).

use epocnote::db::Index;
use epocnote::init::{Target, run_init};
use epocnote::sync::{IndexSync, ScanReuse};
use rusqlite::params;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const BUDGET: Duration = Duration::from_secs(30);

fn project() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    run_init(Some(&root), &root, Target::None).unwrap();
    (dir, root)
}

fn write_note(root: &Path, name: &str, text: &str) {
    fs::write(root.join(".context/notes").join(name), text).unwrap();
}

fn note_md(title: &str, impacts: &str, body: &str) -> String {
    format!("---\ntitle: {title}\nkind: note\nscope: paths\nimpacts:\n{impacts}---\n{body}")
}

fn prepare(root: &Path, index: &Index) -> epocnote::sync::SyncOutcome {
    IndexSync::new(root)
        .prepare_for_search(index, ScanReuse::Auto, BUDGET)
        .unwrap()
}

fn file_row(index: &Index, path: &str) -> Option<(i64, i64, String, String)> {
    index
        .conn()
        .query_row(
            "SELECT size, mtime_ns, title, hash FROM files WHERE path = ?1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
}

fn chunk_count(index: &Index, path: &str) -> i64 {
    index
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE file_path = ?1",
            params![path],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn indexes_new_documents() {
    let (_t, root) = project();
    write_note(
        &root,
        "a.md",
        &note_md(
            "Auth boundary",
            "  - src/auth/\n",
            "# Auth\n\nTokens live in src/auth.\n",
        ),
    );
    write_note(&root, "b.md", "# Loose note\n\nNo frontmatter body.\n");
    let mut index = Index::open(&root).unwrap();

    let out = prepare(&root, &index);
    assert!(!out.partial_index, "warnings: {:?}", out.warnings);

    let a = file_row(&index, ".context/notes/a.md").expect("a indexed");
    assert_eq!(a.2, "Auth boundary");
    assert_eq!(a.3.len(), 64);
    assert_eq!(chunk_count(&index, ".context/notes/a.md"), 1);

    let impacts: i64 = index
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM impacts WHERE context_path = '.context/notes/a.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(impacts, 1);

    // Second prepare with an unchanged corpus reuses nothing yet but must
    // not take the write lock or churn rows (6.3).
    let out2 = IndexSync::new(&root)
        .prepare_for_search(&index, ScanReuse::Off, BUDGET)
        .unwrap();
    assert!(!out2.partial_index);
    index.ensure_current().unwrap();
}

#[test]
fn reconcile_applies_changes_and_removals() {
    let (_t, root) = project();
    write_note(&root, "keep.md", "# Keep\n\nv1\n");
    write_note(&root, "gone.md", "# Gone\n\nto be deleted\n");
    let index = Index::open(&root).unwrap();
    prepare(&root, &index);
    assert!(file_row(&index, ".context/notes/gone.md").is_some());

    write_note(&root, "keep.md", "# Keep\n\nv2 changed text\n");
    fs::remove_file(root.join(".context/notes/gone.md")).unwrap();

    let out = IndexSync::new(&root)
        .prepare_for_search(&index, ScanReuse::Auto, BUDGET)
        .unwrap();
    assert!(!out.partial_index);
    assert!(file_row(&index, ".context/notes/gone.md").is_none());
    let body: String = index
        .conn()
        .query_row(
            "SELECT body FROM chunks WHERE file_path = '.context/notes/keep.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(body.contains("v2"));
}

#[test]
fn same_content_updates_only_stats() {
    let (_t, root) = project();
    write_note(&root, "a.md", "# A\n\nsame bytes\n");
    let index = Index::open(&root).unwrap();
    prepare(&root, &index);
    let indexed_at: i64 = index
        .conn()
        .query_row(
            "SELECT indexed_at FROM files WHERE path = '.context/notes/a.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Rewrite identical bytes so only mtime changes.
    std::thread::sleep(Duration::from_millis(20));
    write_note(&root, "a.md", "# A\n\nsame bytes\n");
    let out = IndexSync::new(&root)
        .prepare_for_search(&index, ScanReuse::Auto, BUDGET)
        .unwrap();
    assert!(!out.partial_index);
    let indexed_at2: i64 = index
        .conn()
        .query_row(
            "SELECT indexed_at FROM files WHERE path = '.context/notes/a.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed_at, indexed_at2, "content-equal doc re-indexed");
}

#[test]
fn oversized_and_non_utf8_files_are_excluded() {
    let (_t, root) = project();
    write_note(&root, "ok.md", "# Ok\n\nfine\n");
    fs::write(root.join(".context/notes/big.md"), vec![b'x'; 1_048_577]).unwrap();
    fs::write(root.join(".context/notes/bad.md"), [0xff, 0xfe, 0xfd, 0x3c]).unwrap();
    let index = Index::open(&root).unwrap();

    let out = prepare(&root, &index);
    assert_eq!(out.excluded_count, 2, "outcome: {out:?}");
    assert!(out.warnings.iter().any(|w| w.code == "EXCLUDED"));
    assert!(file_row(&index, ".context/notes/big.md").is_none());
    assert!(file_row(&index, ".context/notes/bad.md").is_none());
    // The oversized file never becomes a candidate, so the scan alone
    // reports it; UTF-8 failure is discovered at read time.
    assert!(
        out.warnings
            .iter()
            .any(|w| w.path.as_deref() == Some(".context/notes/bad.md"))
    );
}

#[test]
fn exclusion_removes_previously_indexed_content() {
    let (_t, root) = project();
    write_note(&root, "grow.md", "# Grow\n\nsmall\n");
    let index = Index::open(&root).unwrap();
    prepare(&root, &index);
    assert!(file_row(&index, ".context/notes/grow.md").is_some());

    fs::write(root.join(".context/notes/grow.md"), vec![b'y'; 1_048_577]).unwrap();
    let out = IndexSync::new(&root)
        .prepare_for_search(&index, ScanReuse::Auto, BUDGET)
        .unwrap();
    assert!(out.excluded_count >= 1);
    assert!(file_row(&index, ".context/notes/grow.md").is_none());
    assert_eq!(chunk_count(&index, ".context/notes/grow.md"), 0);
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_a_failure_not_an_exclusion() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, root) = project();
    write_note(&root, "locked.md", "# Locked\n\nno read\n");
    fs::set_permissions(
        root.join(".context/notes/locked.md"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let index = Index::open(&root).unwrap();

    let out = IndexSync::new(&root)
        .prepare_for_search(&index, ScanReuse::Auto, BUDGET)
        .unwrap();
    assert!(out.partial_index, "failures must report partial_index");
    assert!(out.warnings.iter().any(|w| w.code == "READ_FAILED"));
    assert!(file_row(&index, ".context/notes/locked.md").is_none());

    fs::set_permissions(
        root.join(".context/notes/locked.md"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

#[test]
fn scan_reuse_skips_second_scan() {
    let (_t, root) = project();
    write_note(&root, "a.md", "# A\n\none\n");
    let index = Index::open(&root).unwrap();
    let sync = IndexSync::with_reuse_window(&root, Duration::from_secs(600));

    let out = sync
        .prepare_for_search(&index, ScanReuse::Auto, BUDGET)
        .unwrap();
    assert!(!out.scan_reused);
    assert!(file_row(&index, ".context/notes/a.md").is_some());

    // A filesystem change made during the reuse window is not seen.
    write_note(&root, "b.md", "# B\n\ntwo\n");
    let out = sync
        .prepare_for_search(&index, ScanReuse::Auto, BUDGET)
        .unwrap();
    assert!(out.scan_reused);
    assert!(file_row(&index, ".context/notes/b.md").is_none());

    // --scan-reuse off always scans.
    let out = sync
        .prepare_for_search(&index, ScanReuse::Off, BUDGET)
        .unwrap();
    assert!(!out.scan_reused);
    assert!(file_row(&index, ".context/notes/b.md").is_some());
}

#[test]
fn batching_indexes_large_sets() {
    let (_t, root) = project();
    for i in 0..150 {
        write_note(
            &root,
            &format!("n{i:03}.md"),
            &format!("# N{i}\n\ndoc {i}\n"),
        );
    }
    let index = Index::open(&root).unwrap();
    let out = prepare(&root, &index);
    assert!(!out.partial_index);
    let count: i64 = index
        .conn()
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    // 150 notes + architecture.md
    assert_eq!(count, 151);
}
