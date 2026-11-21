use epocnote::db::Index;
use epocnote::root::db_path;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

fn run(root: &Path, command: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_epocnote"))
        .args([command, "--root"])
        .arg(root)
        .args(extra)
        .output()
        .unwrap()
}

fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn title(conn: &rusqlite::Connection, path: &str) -> String {
    conn.query_row("SELECT title FROM files WHERE path = ?1", [path], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn rebuild_rehashes_same_stat_edits_and_repairs_the_index() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let context = root.join(".context");
    fs::create_dir(&context).unwrap();
    let edited = context.join("edited.md");
    let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::write(&edited, "# Old title\n").unwrap();
    File::options()
        .write(true)
        .open(&edited)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    fs::write(context.join("orphan.md"), "# Orphan\n").unwrap();
    fs::write(context.join("repair.md"), b"\xff").unwrap();

    let first = run(root, "rebuild", &[]);
    assert_success(&first);
    assert!(String::from_utf8_lossy(&first.stdout).contains("excluded_count: 1"));

    let mut index = Index::open(root).unwrap();
    let reader = index.conn_mut().transaction().unwrap();
    assert_eq!(title(&reader, ".context/edited.md"), "Old title");
    fs::write(&edited, "# New title\n").unwrap();
    File::options()
        .write(true)
        .open(&edited)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    fs::remove_file(context.join("orphan.md")).unwrap();
    fs::write(context.join("repair.md"), "# Repaired\n").unwrap();

    let second = run(root, "rebuild", &[]);
    assert_success(&second);
    let summary = String::from_utf8(second.stdout).unwrap();
    assert_eq!(
        summary,
        "indexed_count: 2\nexcluded_count: 0\nfailed_count: 0\n"
    );
    assert_eq!(title(&reader, ".context/edited.md"), "Old title");
    reader.commit().unwrap();
    assert_eq!(title(index.conn(), ".context/edited.md"), "New title");
    assert_eq!(title(index.conn(), ".context/repair.md"), "Repaired");
    let paths: String = index
        .conn()
        .query_row(
            "SELECT group_concat(path ORDER BY path) FROM files",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(paths, ".context/edited.md,.context/repair.md");
}

#[test]
fn check_is_read_only_and_reports_sorted_findings_and_cycles() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let context = root.join(".context");
    fs::create_dir(&context).unwrap();
    let notes = [
        (
            "a.md",
            "---\nimpacts: [z.rs, a.rs]\nsupersedes: [.context/b.md]\n---\n",
        ),
        (
            "b.md",
            "---\nsupersedes: [.context/c.md, .context/invalid.md]\n---\n",
        ),
        ("c.md", "---\nsupersedes: [.context/a.md]\n---\n"),
        ("d.md", "---\nsupersedes: [.context/a.md]\n---\n"),
        ("e.md", "---\nkind: global\n---\n"),
        ("f.md", "---\n[bad YAML\n---\n"),
    ];
    for (name, text) in notes {
        fs::write(context.join(name), text).unwrap();
    }
    fs::write(context.join("invalid.md"), b"\xff").unwrap();

    let normal = run(root, "check", &[]);
    assert_success(&normal);
    assert!(!db_path(root).exists());
    let diagnostics = String::from_utf8(normal.stdout).unwrap();
    let prefixes: Vec<_> = diagnostics
        .lines()
        .map(|line| line.splitn(3, ": ").take(2).collect::<Vec<_>>().join(": "))
        .collect();
    assert_eq!(
        prefixes,
        [
            ".context/a.md: STALE_IMPACT",
            ".context/a.md: STALE_IMPACT",
            ".context/a.md: SUPERSEDES_CYCLE",
            ".context/b.md: SUPERSEDES_CYCLE",
            ".context/b.md: UNRESOLVED_SUPERSEDES",
            ".context/c.md: SUPERSEDES_CYCLE",
            ".context/e.md: INVALID_FIELD",
            ".context/f.md: MALFORMED_FRONTMATTER",
        ]
    );
    assert!(diagnostics.lines().next().unwrap().contains("\"a.rs\""));

    fs::write(db_path(root), b"unused index").unwrap();
    let strict = run(root, "check", &["--strict"]);
    assert_eq!(strict.status.code(), Some(1));
    assert_eq!(strict.stdout, diagnostics.as_bytes());
    assert!(strict.stderr.is_empty());
    assert_eq!(fs::read(db_path(root)).unwrap(), b"unused index");
    for (name, text) in notes {
        assert_eq!(fs::read_to_string(context.join(name)).unwrap(), text);
    }
}

#[cfg(unix)]
#[test]
fn rebuild_preserves_unreadable_rows_and_check_fails_on_incomplete_inspection() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let context = root.join(".context");
    fs::create_dir(&context).unwrap();
    let unreadable = context.join("locked.md");
    fs::write(&unreadable, "# Before\n").unwrap();
    assert_success(&run(root, "rebuild", &[]));

    fs::write(&unreadable, "# Different\n").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    if File::open(&unreadable).is_ok() {
        return; // Privileged users may still be able to read the file.
    }
    fs::write(context.join("new.md"), "# New document\n").unwrap();

    let rebuilt = run(root, "rebuild", &[]);
    assert_eq!(rebuilt.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rebuilt.stdout).contains("failed_count: 1"));
    assert!(String::from_utf8_lossy(&rebuilt.stderr).contains("READ_FAILED"));
    let index = Index::open(root).unwrap();
    assert_eq!(title(index.conn(), ".context/locked.md"), "Before");
    assert_eq!(title(index.conn(), ".context/new.md"), "New document");

    for args in [&[] as &[&str], &["--strict"]] {
        let checked = run(root, "check", args);
        assert_eq!(checked.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&checked.stderr).contains("IO_ERROR"));
    }
}

#[cfg(unix)]
#[test]
fn incomplete_traversal_never_reconciles_removals() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let context = root.join(".context");
    let blocked = context.join("blocked");
    fs::create_dir_all(&blocked).unwrap();
    let removed = context.join("removed.md");
    fs::write(&removed, "# Still indexed\n").unwrap();
    assert_success(&run(root, "rebuild", &[]));

    fs::remove_file(removed).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&blocked).is_ok() {
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let results = [
        run(root, "rebuild", &[]),
        run(root, "check", &[]),
        run(root, "check", &["--strict"]),
    ];
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();

    for result in results {
        assert_eq!(result.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&result.stderr).contains("IO_ERROR"));
    }
    let index = Index::open(root).unwrap();
    assert_eq!(title(index.conn(), ".context/removed.md"), "Still indexed");
}
