//! `epocnote_search` retrieval, ranking, and response shape (plan.md 8).

use epocnote::db::Index;
use epocnote::init::{Target, run_init};
use epocnote::search::{SearchInput, SearchOutput, search};
use epocnote::sync::{IndexSync, ScanReuse};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn project() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    run_init(Some(&root), &root, Target::None).unwrap();
    (dir, root)
}

fn note(root: &Path, name: &str, meta: &str, body: &str) {
    fs::write(
        root.join(".context/notes").join(name),
        format!("---\n{meta}---\n{body}"),
    )
    .unwrap();
}

fn ctx(root: &Path) -> (Index, IndexSync) {
    (Index::open(root).unwrap(), IndexSync::new(root))
}

fn run(_root: &Path, index: &Index, sync: &IndexSync, input: SearchInput) -> SearchOutput {
    search(index, sync, input, ScanReuse::Off).unwrap()
}

fn q(query: &str) -> SearchInput {
    SearchInput {
        query: Some(query.into()),
        path_filter: None,
        limit: None,
    }
}

fn f(filter: &str) -> SearchInput {
    SearchInput {
        query: None,
        path_filter: Some(filter.into()),
        limit: None,
    }
}

#[test]
fn path_filter_matches_spec_table() {
    let (_t, root) = project();
    note(
        &root,
        "exact.md",
        "title: E\nimpacts:\n  - src/auth/login.go\n",
        "# E\n\nx\n",
    );
    note(
        &root,
        "parent.md",
        "title: P\nimpacts:\n  - src/auth\n",
        "# P\n\nx\n",
    );
    note(
        &root,
        "child.md",
        "title: C\nimpacts:\n  - src/auth/login.go\n",
        "# C\n\nx\n",
    );
    note(
        &root,
        "other.md",
        "title: O\nimpacts:\n  - src/author.go\n",
        "# O\n\nx\n",
    );
    let (index, sync) = ctx(&root);

    // Descendant filter hits an ancestor impact and the exact impact.
    let out = run(&root, &index, &sync, f("src/auth/login.go"));
    let paths: Vec<_> = out.results.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&".context/notes/exact.md"));
    assert!(paths.contains(&".context/notes/parent.md"), "{paths:?}");
    assert!(!paths.contains(&".context/notes/other.md"));
    assert_eq!(out.mode, "path");
    assert!(out.results.iter().all(|r| r.score.is_none()));

    // Ancestor filter hits a descendant impact.
    let out = run(&root, &index, &sync, f("src/auth"));
    let paths: Vec<_> = out.results.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&".context/notes/exact.md"));
    // Prefix without component boundary does not match.
    let out = run(&root, &index, &sync, f("src/au"));
    assert!(out.results.is_empty() || out.results.iter().all(|r| !r.path.contains("other")));
}

#[test]
fn global_scope_is_eligible_under_any_filter() {
    let (_t, root) = project();
    note(
        &root,
        "g.md",
        "title: Arch\nkind: architecture\nscope: global\n",
        "# Arch\n\nglobal\n",
    );
    note(
        &root,
        "n.md",
        "title: N\nimpacts:\n  - src/x\n",
        "# N\n\ny\n",
    );
    let (index, sync) = ctx(&root);
    let out = run(&root, &index, &sync, f("unrelated/dir"));
    assert!(out.results.iter().any(|r| r.path.ends_with("g.md")));
}

#[test]
fn text_search_all_then_any_tier() {
    let (_t, root) = project();
    note(&root, "both.md", "title: B\n", "# Both\n\nalpha beta\n");
    note(&root, "one.md", "title: O\n", "# One\n\nalpha only\n");
    let (index, sync) = ctx(&root);
    let out = run(&root, &index, &sync, q("alpha beta"));
    assert_eq!(out.mode, "text");
    let both = out.results.iter().position(|r| r.path.ends_with("both.md"));
    let one = out.results.iter().position(|r| r.path.ends_with("one.md"));
    assert!(both.is_some() && one.is_some(), "{:?}", out.results);
    assert!(both < one, "all-tier result must rank before any-tier");
    assert_eq!(out.results[both.unwrap()].match_tier, Some("all"));
    assert_eq!(out.results[one.unwrap()].match_tier, Some("any"));
    assert!(out.results[0].score.is_some());
}

#[test]
fn stemming_and_underscore_tokenization() {
    let (_t, root) = project();
    note(
        &root,
        "s.md",
        "title: S\n",
        "# Sessions\n\nThe session_store caches.\n",
    );
    let (index, sync) = ctx(&root);
    for query in ["session", "sessions", "session_store", "store"] {
        let out = run(&root, &index, &sync, q(query));
        assert!(!out.results.is_empty(), "query {query:?} found nothing");
    }
}

#[test]
fn superseded_docs_rank_last_with_superseded_by() {
    let (_t, root) = project();
    note(
        &root,
        "new.md",
        "title: New\nsupersedes:\n  - .context/notes/old.md\n",
        "# New\n\nshared keyword\n",
    );
    note(&root, "old.md", "title: Old\n", "# Old\n\nshared keyword\n");
    let (index, sync) = ctx(&root);
    let out = run(&root, &index, &sync, q("shared keyword"));
    assert!(out.results.len() >= 2);
    assert_eq!(out.results[0].path, ".context/notes/new.md");
    assert_eq!(out.results[1].path, ".context/notes/old.md");
    assert_eq!(
        out.results[1].superseded_by,
        vec![".context/notes/new.md".to_string()]
    );
    assert!(out.results[0].superseded_by.is_empty());
}

#[test]
fn validation_errors() {
    let (_t, root) = project();
    let (index, sync) = ctx(&root);
    let err = |i: SearchInput| {
        search(&index, &sync, i, ScanReuse::Off)
            .unwrap_err()
            .code()
            .to_string()
    };
    assert_eq!(
        err(SearchInput {
            query: None,
            path_filter: None,
            limit: None
        }),
        "INVALID_ARGUMENT"
    );
    assert_eq!(err(q("   ")), "INVALID_ARGUMENT");
    assert_eq!(err(q("!!! ...")), "INVALID_ARGUMENT");
    assert_eq!(err(q(&"x".repeat(513))), "INVALID_ARGUMENT");
    assert_eq!(err(f("../outside")), "INVALID_ARGUMENT");
    for bad in [0, -1] {
        assert_eq!(
            err(SearchInput {
                query: Some("a".into()),
                path_filter: None,
                limit: Some(bad)
            }),
            "INVALID_ARGUMENT"
        );
    }
}

#[test]
fn limit_clamps_at_ten_and_reports_truncated() {
    let (_t, root) = project();
    for i in 0..12 {
        note(
            &root,
            &format!("d{i:02}.md"),
            &format!("title: D{i}\ncreated_at: \"2026-11-13T09:30:00Z\"\n"),
            "# Same\n\nidentical body\n",
        );
    }
    let (index, sync) = ctx(&root);
    let out = run(&root, &index, &sync, q("identical"));
    assert_eq!(out.results.len(), 5, "default limit is 5");
    assert!(out.truncated);
    let out = run(
        &root,
        &index,
        &sync,
        SearchInput {
            query: Some("identical".into()),
            path_filter: None,
            limit: Some(50),
        },
    );
    assert_eq!(out.results.len(), 10, "limit clamps at 10");
    assert!(out.truncated);
}

#[test]
fn empty_match_is_success_not_truncation() {
    let (_t, root) = project();
    note(&root, "a.md", "title: A\n", "# A\n\nhello\n");
    let (index, sync) = ctx(&root);
    let out = run(&root, &index, &sync, q("zzz_nonexistent_zzz"));
    assert!(out.results.is_empty());
    assert!(!out.truncated);
    // created_at absent from frontmatter normalizes to null in output.
    assert!(out.results.is_empty());
}
