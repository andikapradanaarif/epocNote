//! Fresh-database DDL (plan.md section 10.3) and the bundled-engine checks the
//! CI matrix must pass: FTS5 availability, the `porter unicode61` tokenizer, and
//! the actual bm25 ranking query used by search.

/// Schema for a new database. Applied once inside a transaction when a database
/// is created or recreated (plan.md sections 6.2 and 10.3); never run
/// unconditionally at startup.
// Used only by tests until the index lifecycle lands.
#[allow(dead_code)]
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
