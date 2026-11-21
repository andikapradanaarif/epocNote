//! Index connection setup and schema version lifecycle
//! (plan.md sections 6.1, 6.2, 10.2, 10.3).

use epocnote::db::{Index, SCHEMA_VERSION};
use epocnote::error::Error;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn mk_context(root: &Path) {
    fs::create_dir_all(root.join(".context/notes")).unwrap();
}

fn db_file(root: &Path) -> std::path::PathBuf {
    root.join(".context/.epocnote.sqlite")
}

fn version(root: &Path) -> i64 {
    Connection::open(db_file(root))
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

#[test]
fn open_creates_schema_and_pragmas() {
    let tmp = TempDir::new().unwrap();
    mk_context(tmp.path());
    let index = Index::open(tmp.path()).unwrap();
    let conn = index.conn();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        conn.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    assert_eq!(
        conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.pragma_query_value(None, "busy_timeout", |r| r.get::<_, i64>(0))
            .unwrap(),
        5_000
    );
    for t in ["files", "chunks", "impacts", "supersedes", "chunks_fts"] {
        assert!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [t],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
                == 1,
            "missing {t}"
        );
    }
}

#[test]
fn reopen_preserves_data_and_does_not_recreate() {
    let tmp = TempDir::new().unwrap();
    mk_context(tmp.path());
    {
        let index = Index::open(tmp.path()).unwrap();
        index
            .conn()
            .execute(
                "INSERT INTO files(path, hash, size, mtime_ns, title, indexed_at)
                 VALUES ('.context/architecture.md','h',1,1,'arch',0)",
                [],
            )
            .unwrap();
    }
    let index = Index::open(tmp.path()).unwrap();
    assert_eq!(
        index
            .conn()
            .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn newer_schema_is_too_new() {
    let tmp = TempDir::new().unwrap();
    mk_context(tmp.path());
    Index::open(tmp.path()).unwrap();
    Connection::open(db_file(tmp.path()))
        .unwrap()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    assert!(matches!(Index::open(tmp.path()), Err(Error::SchemaTooNew)));
    // The newer version is never downgraded or overwritten.
    assert_eq!(version(tmp.path()), SCHEMA_VERSION + 1);
}

#[test]
fn incomplete_schema_is_recreated() {
    let tmp = TempDir::new().unwrap();
    mk_context(tmp.path());
    Index::open(tmp.path()).unwrap();
    let c = Connection::open(db_file(tmp.path())).unwrap();
    c.execute("DROP TABLE chunks", []).unwrap();
    drop(c);
    // user_version still 1 but a table is missing → recreate.
    let index = Index::open(tmp.path()).unwrap();
    assert_eq!(
        index
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='chunks'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn uninitialized_context_is_not_initialized() {
    let tmp = TempDir::new().unwrap();
    assert!(matches!(
        Index::open(tmp.path()),
        Err(Error::NotInitialized)
    ));
}

// Windows cannot remove or replace a database file while this process holds
// it open, so these scenarios are only reachable on platforms that allow
// unlinking open files.
#[cfg(unix)]
#[test]
fn ensure_current_reopens_replaced_db() {
    let tmp = TempDir::new().unwrap();
    let ctx = tmp.path().join(".context");
    mk_context(tmp.path());
    let mut index = Index::open(tmp.path()).unwrap();
    index
        .conn()
        .execute(
            "INSERT INTO files(path, hash, size, mtime_ns, title, indexed_at)
             VALUES ('.context/old.md','h',1,1,'old',0)",
            [],
        )
        .unwrap();
    assert!(!index.ensure_current().unwrap());

    // Replace .context entirely; the old file handle is stale.
    fs::remove_dir_all(&ctx).unwrap();
    mk_context(tmp.path());
    assert!(index.ensure_current().unwrap());
    assert_eq!(
        index
            .conn()
            .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0,
        "reopened index must not see the stale file's rows"
    );
}

#[cfg(unix)]
#[test]
fn ensure_current_reports_missing_context() {
    let tmp = TempDir::new().unwrap();
    let ctx = tmp.path().join(".context");
    mk_context(tmp.path());
    let mut index = Index::open(tmp.path()).unwrap();
    fs::remove_dir_all(&ctx).unwrap();
    assert!(matches!(index.ensure_current(), Err(Error::NotInitialized)));
}

#[test]
fn sidecar_validation_blocks_symlinked_db() {
    let tmp = TempDir::new().unwrap();
    mk_context(tmp.path());
    Index::open(tmp.path()).unwrap();
    // A symlinked database file is rejected before opening (unix check).
    #[cfg(unix)]
    {
        fs::remove_file(db_file(tmp.path())).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", db_file(tmp.path())).unwrap();
        assert!(matches!(Index::open(tmp.path()), Err(Error::UnsafePath(_))));
    }
}

#[test]
fn missing_trigger_recreates_the_disposable_schema() {
    let root = tempfile::tempdir().unwrap();
    mk_context(root.path());
    let index = Index::open(root.path()).unwrap();
    index
        .conn()
        .execute_batch("DROP TRIGGER chunks_ai")
        .unwrap();
    drop(index);

    let reopened = Index::open(root.path()).unwrap();
    let count: i64 = reopened
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name = 'chunks_ai'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
