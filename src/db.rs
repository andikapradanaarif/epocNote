//! Index connection setup, schema version lifecycle, and the fresh-database
//! DDL (plan.md sections 6.1, 6.2, 10.2, 10.3).

use crate::error::Error;
use crate::root::{ContextStatus, db_path, validate_context_dir, validate_db_files};
use rusqlite::{Connection, TransactionBehavior};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT_MS: i64 = 5_000;

/// One open index connection. Each process serializes use of its own
/// connections; there is no process-wide lock (section 6.1).
pub struct Index {
    conn: Connection,
    path: PathBuf,
    /// Device + file identity captured when the connection was opened, used
    /// to notice the file being replaced underneath us (section 3.2).
    identity: Option<(u64, u64)>,
}

impl Index {
    /// Open (creating if needed) the context database under `root/.context`,
    /// apply per-connection pragmas, verify FTS5, and reconcile the schema
    /// version per section 6.2.
    pub fn open(root: &Path) -> Result<Self, Error> {
        match validate_context_dir(root)? {
            ContextStatus::NotInitialized => return Err(Error::NotInitialized),
            ContextStatus::Ready => {}
        }
        let mut index = Self::open_file(&db_path(root))?;
        index.identity = file_identity(&index.path);
        Ok(index)
    }

    fn open_file(path: &Path) -> Result<Self, Error> {
        validate_db_files_for(path)?;
        let mut conn = Connection::open(path).map_err(map_error)?;
        setup_connection(&conn)?;
        ensure_schema(&mut conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            identity: None,
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Confirm the database path still names the file this connection opened
    /// (device + file identity). When the file was replaced — for example a
    /// recreated `.context/` — close the stale connection and reopen against
    /// the validated path. Returns whether the connection was reopened.
    pub fn ensure_current(&mut self) -> Result<bool, Error> {
        let current = file_identity(&self.path);
        if current.is_some() && current == self.identity {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent()
            && !parent.is_dir()
        {
            return Err(Error::NotInitialized);
        }
        // Assigning *self drops the stale connection and stores the new one.
        let path = self.path.clone();
        *self = Self::open_file(&path)?;
        self.identity = file_identity(&self.path);
        Ok(true)
    }
}

/// Validate the database and sidecar files beneath a `.context` directory
/// (reuses the rules in root.rs via the db file's ancestors).
fn validate_db_files_for(path: &Path) -> Result<(), Error> {
    let Some(root) = path.parent().and_then(|c| c.parent()) else {
        return Err(Error::UnsafePath(
            "database path is not under a project root".into(),
        ));
    };
    validate_db_files(root)
}

/// Per-connection pragmas and the defensive FTS5 probe (sections 6.1, 10.2).
fn setup_connection(conn: &Connection) -> Result<(), Error> {
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(map_error)?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)
        .map_err(map_error)?;
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .map_err(map_error)?;
    if !mode.eq_ignore_ascii_case("wal") {
        // Change it only when needed, outside a transaction, and verify the
        // returned mode (section 6.1).
        let new_mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(map_error)?;
        if !new_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Io(std::io::Error::other(format!(
                "journal_mode is {new_mode}, expected wal"
            ))));
        }
    }
    // Defensive check for builds without FTS5 (section 6.1); bundled builds
    // always pass. Uses a temp table so no catalog objects remain.
    if let Err(e) = conn.execute_batch(
        "CREATE VIRTUAL TABLE temp._epocnote_fts5_probe USING fts5(x);
         DROP TABLE temp._epocnote_fts5_probe;",
    ) {
        let _ = e;
        return Err(Error::Fts5Unavailable);
    }
    Ok(())
}

/// Schema version lifecycle (section 6.2).
fn ensure_schema(conn: &mut Connection) -> Result<(), Error> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(map_error)?;
    if version > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew);
    }
    if version == SCHEMA_VERSION && schema_complete(conn)? {
        return Ok(());
    }
    // Missing or older schema: recreate inside one immediate transaction,
    // re-reading the version because another process may have recreated it
    // while this one waited.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let version: i64 = tx
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(map_error)?;
    if version > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew);
    }
    if version == SCHEMA_VERSION && schema_complete(&tx)? {
        tx.commit().map_err(map_error)?;
        return Ok(());
    }
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS chunks_ai;
         DROP TRIGGER IF EXISTS chunks_ad;
         DROP TRIGGER IF EXISTS chunks_bu;
         DROP TABLE IF EXISTS chunks_fts;
         DROP TABLE IF EXISTS chunks;
         DROP TABLE IF EXISTS impacts;
         DROP TABLE IF EXISTS supersedes;
         DROP TABLE IF EXISTS files;",
    )
    .map_err(map_error)?;
    tx.execute_batch(SCHEMA_DDL).map_err(map_error)?;
    tx.commit().map_err(map_error)?;
    Ok(())
}

fn schema_complete(conn: &Connection) -> Result<bool, Error> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'table'
               AND name IN ('files','chunks','impacts','supersedes','chunks_fts')",
            [],
            |r| r.get(0),
        )
        .map_err(map_error)?;
    Ok(n == 5)
}

/// Device + file identity of `path` (`None` if it cannot be stated).
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    let md = std::fs::metadata(path).ok()?;
    identity_of(&md)
}

#[cfg(unix)]
fn identity_of(md: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((md.dev(), md.ino()))
}

#[cfg(windows)]
fn identity_of(md: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::windows::fs::MetadataExt;
    md.file_index()
        .map(|i| (md.volume_serial_number().unwrap_or(0), i))
}

/// Map engine errors onto the application codes (section 11).
pub fn map_error(e: rusqlite::Error) -> Error {
    use rusqlite::ErrorCode;
    match e {
        rusqlite::Error::SqliteFailure(code, _) => match code.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Error::IndexBusy,
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => Error::IndexCorrupt,
            _ => Error::Io(std::io::Error::other(e)),
        },
        other => Error::Io(std::io::Error::other(other)),
    }
}

/// Schema for a new database. Applied once inside a transaction when a database
/// is created or recreated (plan.md sections 6.2 and 10.3); never run
/// unconditionally at startup.
pub const SCHEMA_DDL: &str = r#"
CREATE TABLE files (
    path          TEXT PRIMARY KEY NOT NULL,
    hash          TEXT NOT NULL,
    size          INTEGER NOT NULL CHECK (size >= 0),
    mtime_ns      INTEGER NOT NULL,
    title         TEXT NOT NULL,
    kind          TEXT NOT NULL DEFAULT 'note'
                  CHECK (kind IN ('note', 'adr', 'architecture')),
    scope         TEXT NOT NULL DEFAULT 'paths'
                  CHECK (scope IN ('paths', 'global')),
    created_at    INTEGER NOT NULL DEFAULT 0 CHECK (created_at >= 0),
    indexed_at    INTEGER NOT NULL
);

CREATE TABLE chunks (
    id            INTEGER PRIMARY KEY,
    file_path     TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    heading       TEXT NOT NULL DEFAULT '',
    start_line    INTEGER NOT NULL CHECK (start_line >= 1),
    end_line      INTEGER NOT NULL CHECK (end_line >= start_line),
    body          TEXT NOT NULL
);

CREATE INDEX idx_chunks_file ON chunks(file_path);

CREATE TABLE impacts (
    context_path  TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    target        TEXT NOT NULL,
    PRIMARY KEY (context_path, target)
);

CREATE INDEX idx_impacts_target ON impacts(target);

CREATE TABLE supersedes (
    context_path  TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    target        TEXT NOT NULL,
    PRIMARY KEY (context_path, target)
);

CREATE INDEX idx_supersedes_target ON supersedes(target);

CREATE VIRTUAL TABLE chunks_fts USING fts5(
    title,
    heading,
    body,
    tokenize = "porter unicode61"
);

CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, title, heading, body)
    SELECT new.id, f.title, new.heading, new.body
    FROM files AS f
    WHERE f.path = new.file_path;
END;

CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.id;
END;

CREATE TRIGGER chunks_bu BEFORE UPDATE ON chunks BEGIN
    SELECT RAISE(ABORT, 'chunks are immutable; delete and reinsert');
END;

PRAGMA user_version = 1;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        c.execute_batch(SCHEMA_DDL).unwrap();
        c
    }

    fn add_doc(c: &Connection, path: &str, title: &str, body: &str) {
        c.execute(
            "INSERT INTO files(path, hash, size, mtime_ns, title, indexed_at)
             VALUES (?1, 'h', ?2, 1, ?3, 0)",
            (path, body.len() as i64, title),
        )
        .unwrap();
        c.execute(
            "INSERT INTO chunks(file_path, heading, start_line, end_line, body)
             VALUES (?1, '', 1, 2, ?2)",
            (path, body),
        )
        .unwrap();
    }

    /// The bundled engine must be at least the WAL-reset-fixed version and must
    /// provide FTS5; plan.md sections 1.3 and 6.1 require both on every target.
    #[test]
    fn bundled_engine_version_and_options() {
        let version = rusqlite::version();
        let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        let v = (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        assert!(
            v >= (3, 51, 3),
            "bundled SQLite {version} predates the WAL-reset fix"
        );

        let c = conn();
        let options: Vec<String> = c
            .prepare("PRAGMA compile_options")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("bundled SQLite {version}, compile options: {options:?}");
        // Creating chunks_fts already proves FTS5 is compiled in.
    }

    /// The actual ranking query used by text search (plan.md section 8.4):
    /// quoted tokens, bm25, join through chunks for metadata.
    #[test]
    fn fts5_ranking_query() {
        let c = conn();
        add_doc(
            &c,
            "a.md",
            "Use Redis",
            "the session_store holds cached sessions",
        );
        add_doc(&c, "b.md", "Unrelated", "nothing here");

        // `session_store` tokenizes as `session` + `store`; a quoted query token
        // matches it as a phrase (plan.md section 10.1).
        let hits: Vec<(String, f64)> = c
            .prepare(
                "SELECT c.file_path, bm25(chunks_fts) FROM chunks_fts
                 JOIN chunks c ON c.id = chunks_fts.rowid
                 WHERE chunks_fts MATCH ?1 ORDER BY 2 LIMIT 10",
            )
            .unwrap()
            .query_map(["\"session\""], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a.md");

        // Porter stemming: `sessions` matches `session`.
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '\"sessions\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn triggers_keep_fts_aligned() {
        let c = conn();
        add_doc(&c, "a.md", "t", "body one");
        assert_eq!(
            c.query_row("SELECT count(*) FROM chunks_fts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(c.execute("UPDATE chunks SET body = 'x'", []).is_err());
        c.execute("DELETE FROM chunks WHERE file_path = 'a.md'", [])
            .unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM chunks_fts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
