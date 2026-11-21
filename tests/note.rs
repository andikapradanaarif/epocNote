//! `epocnote_note`: validation, server frontmatter, atomic publication, and
//! direct indexing (plan.md section 9).

use epocnote::db::Index;
use epocnote::doc;
use epocnote::init::{Target, run_init};
use epocnote::note::{NoteInput, create_note};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn project() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    run_init(Some(&root), &root, Target::None).unwrap();
    (dir, root)
}

fn input(title: &str, body: &str) -> NoteInput {
    NoteInput {
        title: title.into(),
        body: body.into(),
        impacts: vec![],
        kind: None,
        supersedes: None,
    }
}

#[test]
fn creates_publishes_and_indexes_a_note() {
    let (_t, root) = project();
    let mut index = Index::open(&root).unwrap();
    let mut i = input("Auth boundary decision", "# Use JWT\n\nTokens in src/auth.");
    i.impacts = vec!["src/auth".into()];
    let out = create_note(&root, &mut index, i).unwrap();
    assert!(out.saved);
    assert!(out.indexed, "warnings: {:?}", out.warnings);

    // Filename: YYYY-MM-DD-<slug>-<16 lowercase hex>.md under .context/notes.
    let name = out.path.rsplit('/').next().unwrap();
    assert!(name.starts_with(&chrono::Utc::now().format("%Y-%m-%d").to_string()));
    assert!(name.contains("-auth-boundary-decision-"));
    let suffix = name.trim_end_matches(".md").rsplit('-').next().unwrap();
    assert_eq!(suffix.len(), 16);
    assert!(
        suffix
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    // The published bytes parse back with the recorded metadata.
    let text = fs::read_to_string(root.join(&out.path)).unwrap();
    let analyzed = doc::analyze(&text, &out.path);
    assert_eq!(analyzed.title, "Auth boundary decision");
    assert_eq!(analyzed.kind.as_str(), "note");
    assert_eq!(analyzed.impacts.len(), 1);
    assert!(analyzed.created_at > 0);
    assert!(text.contains("# Use JWT"));

    // And it is searchable without a corpus scan.
    let hash: String = index
        .conn()
        .query_row(
            "SELECT hash FROM files WHERE path = ?1",
            rusqlite::params![out.path],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hash.len(), 64);
}

#[test]
fn validation_rejections() {
    let (_t, root) = project();
    let mut index = Index::open(&root).unwrap();
    let mut err = |i: NoteInput| create_note(&root, &mut index, i).unwrap_err().code();
    assert_eq!(err(input("  ", "x")), "INVALID_ARGUMENT");
    assert_eq!(err(input("line\nbreak", "x")), "INVALID_ARGUMENT");
    assert_eq!(err(input(&"t".repeat(201), "x")), "INVALID_ARGUMENT");
    assert_eq!(err(input("t", "   ")), "INVALID_ARGUMENT");
    assert_eq!(err(input("t", &"x".repeat(10_241))), "INVALID_ARGUMENT");
    let mut i = input("t", "x");
    i.impacts = vec!["a".into(); 33];
    assert_eq!(err(i), "INVALID_ARGUMENT");
    let mut i = input("t", "x");
    i.impacts = vec!["../escape".into()];
    assert_eq!(err(i), "INVALID_ARGUMENT");
    let mut i = input("t", "x");
    i.kind = Some("decision".into());
    assert_eq!(err(i), "INVALID_ARGUMENT");
    let mut i = input("t", "x");
    i.supersedes = Some(vec!["src/not-context.md".into()]);
    assert_eq!(err(i), "INVALID_ARGUMENT");
    let mut i = input("t", "x");
    i.supersedes = Some(vec![".context/notes/missing.md".into()]);
    assert_eq!(err(i), "INVALID_ARGUMENT");
    let published = fs::read_dir(root.join(".context/notes"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .count();
    assert_eq!(published, 0, "rejected inputs must not publish files");
}

#[test]
fn supersedes_requires_existing_context_doc() {
    let (_t, root) = project();
    let mut index = Index::open(&root).unwrap();
    let old = create_note(&root, &mut index, input("Old note", "body")).unwrap();
    assert!(old.indexed);
    let mut i = input("New note", "replacement");
    i.supersedes = Some(vec![old.path.clone()]);
    i.kind = Some("adr".into());
    let new = create_note(&root, &mut index, i).unwrap();
    assert!(new.indexed);
    let n: i64 = index
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM supersedes WHERE context_path = ?1 AND target = ?2",
            rusqlite::params![new.path, old.path],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    let kind: String = index
        .conn()
        .query_row(
            "SELECT kind FROM files WHERE path = ?1",
            rusqlite::params![new.path],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "adr");
}

#[test]
fn frontmatter_escapes_specials() {
    let (_t, root) = project();
    let mut index = Index::open(&root).unwrap();
    let out = create_note(
        &root,
        &mut index,
        input("Title: with \"quotes\" & colon", "body"),
    )
    .unwrap();
    let text = fs::read_to_string(root.join(&out.path)).unwrap();
    let analyzed = doc::analyze(&text, &out.path);
    assert_eq!(analyzed.title, "Title: with \"quotes\" & colon");
}

#[test]
fn missing_impact_target_is_a_soft_warning() {
    let (_t, root) = project();
    let mut index = Index::open(&root).unwrap();
    let mut i = input("T", "body");
    i.impacts = vec!["no/such/dir".into()];
    let out = create_note(&root, &mut index, i).unwrap();
    assert!(out.saved && out.indexed);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.code == "IMPACT_TARGET" && w.path.as_deref() == Some("no/such/dir"))
    );
}

#[test]
fn slug_fallback_and_normalization() {
    let (_t, root) = project();
    let mut index = Index::open(&root).unwrap();
    let out = create_note(&root, &mut index, input("!!! ???", "body")).unwrap();
    assert!(out.path.contains("-note-"));
    let out = create_note(&root, &mut index, input("CRLF body", "a\r\nb\rc")).unwrap();
    let text = fs::read_to_string(root.join(&out.path)).unwrap();
    assert!(!text.contains('\r'));
}
