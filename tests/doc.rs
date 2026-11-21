//! Markdown contract tests: frontmatter, malformed fallback, limits, and
//! chunking (plan.md sections 4, 7, 10.4).

use epocnote::doc::{DocKind, Scope, analyze};

const P: &str = ".context/notes/2026-11-13-x-a1b2c3d4e5f60718.md";

fn fm(body: &str) -> String {
    format!("---\ntitle: T\nkind: note\nscope: paths\n---\n{body}")
}

#[test]
fn parses_full_frontmatter() {
    let doc = analyze(&fm("## First\nbody text\n\n## Second\nmore\n"), P);
    assert!(!doc.malformed_frontmatter);
    assert_eq!(doc.title, "T");
    assert_eq!(doc.kind, DocKind::Note);
    assert_eq!(doc.scope, Scope::Paths);
    assert_eq!(doc.chunks.len(), 2);
    assert_eq!(doc.chunks[0].heading, "First");
    // The heading line remains part of the chunk body.
    assert!(doc.chunks[0].text.starts_with("## First"));
    // Frontmatter lines count toward positions: lines 1-5 are fm, body
    // starts at line 6.
    assert_eq!(doc.chunks[0].start_line, 6);
    assert_eq!(doc.chunks[0].end_line, 7);
    assert_eq!(doc.chunks[1].start_line, 9);
}

#[test]
fn fields_extracted() {
    let doc = analyze(
        "---\ntitle: Use Redis\nkind: adr\nscope: global\ncreated_at: \"2026-11-13T09:30:00Z\"\nimpacts:\n  - src/auth/\n  - src/session/store.go\nsupersedes:\n  - .context/notes/old-a1b2c3d4e5f60718.md\n---\nbody\n",
        P,
    );
    assert_eq!(doc.title, "Use Redis");
    assert_eq!(doc.kind, DocKind::Adr);
    assert_eq!(doc.scope, Scope::Global);
    assert_eq!(
        doc.created_at,
        chrono::DateTime::parse_from_rfc3339("2026-11-13T09:30:00Z")
            .unwrap()
            .timestamp()
    );
    let impacts: Vec<_> = doc.impacts.iter().map(|c| c.as_str()).collect();
    assert_eq!(impacts, ["src/auth", "src/session/store.go"]);
    assert_eq!(
        doc.supersedes[0].as_str(),
        ".context/notes/old-a1b2c3d4e5f60718.md"
    );
    assert!(doc.warnings.is_empty(), "{:?}", doc.warnings);
}

#[test]
fn title_fallback_heading_then_stem() {
    // No frontmatter: first heading wins.
    let doc = analyze("# Real Title\nbody\n", P);
    assert_eq!(doc.title, "Real Title");
    // No headings either: filename stem.
    let doc = analyze("just body\n", P);
    assert_eq!(doc.title, "2026-11-13-x-a1b2c3d4e5f60718");
    // Missing title field falls back to heading.
    let doc = analyze("---\nkind: note\n---\n# From Heading\n", P);
    assert_eq!(doc.title, "From Heading");
}

#[test]
fn malformed_missing_closure_indexes_raw() {
    let raw = "---\ntitle: never parsed\nbody after\n";
    let doc = analyze(raw, P);
    assert!(doc.malformed_frontmatter);
    assert_eq!(doc.kind, DocKind::Note);
    assert_eq!(doc.scope, Scope::Paths);
    assert_eq!(doc.created_at, 0);
    assert!(doc.impacts.is_empty());
    // The whole raw text is indexed, including the delimiter lines.
    let all: String = doc.chunks.iter().map(|c| c.text.as_str()).collect();
    assert!(all.contains("title: never parsed"));
    assert!(
        doc.warnings
            .iter()
            .any(|w| w.code == "MALFORMED_FRONTMATTER")
    );
}

#[test]
fn malformed_variants() {
    for raw in [
        "---\n[unclosed\n---\nbody\n",            // invalid YAML
        "---\ntitle: a\ntitle: b\n---\nbody\n",   // duplicate key
        "---\nx: &a 1\ny: *a\n---\nbody\n",       // alias
        "---\nbase: &b {v: 1}\n<<: *b\n---\nb\n", // merge key
        "---\n- a\n- b\n---\nbody\n",             // non-mapping root
        "---\njust a scalar\n---\nbody\n",        // scalar root
    ] {
        let doc = analyze(raw, P);
        assert!(
            doc.malformed_frontmatter,
            "{raw:?} should be malformed frontmatter"
        );
    }
}

#[test]
fn dots_does_not_close_frontmatter() {
    let doc = analyze("---\ntitle: T\n...\n---\nbody\n", P);
    // `...` sits inside the block; the real `---` still closes it.
    assert!(!doc.malformed_frontmatter);
    assert_eq!(doc.title, "T");
    assert_eq!(doc.chunks[0].text, "body");
}

#[test]
fn field_validation_rules() {
    let doc = analyze(
        "---\ntitle: 123\nkind: decision\nscope: everywhere\ncreated_at: 2026-11-13\nimpacts: src/x\n---\n# H\n",
        P,
    );
    assert!(!doc.malformed_frontmatter);
    // title: 123 is a number, not a string → fallback to heading.
    assert_eq!(doc.title, "H");
    assert_eq!(doc.kind, DocKind::Note); // invalid explicit kind → note+warn
    assert_eq!(doc.scope, Scope::Paths); // invalid explicit scope → paths+warn
    assert_eq!(doc.created_at, 0); // bare date lacks timezone
    assert!(doc.impacts.is_empty()); // scalar, not an array
    let codes: Vec<_> = doc.warnings.iter().map(|w| w.code.as_str()).collect();
    assert!(codes.contains(&"INVALID_FIELD"));
}

#[test]
fn valid_created_at_variants() {
    for (ts, want_zero) in [
        ("\"2026-11-13T09:30:00Z\"", false),
        ("\"2026-11-13T09:30:00+02:00\"", false),
        ("\"2026-11-13 09:30:00\"", true),  // not RFC 3339
        ("\"1960-01-01T00:00:00Z\"", true), // before epoch
        ("12345", true),                    // not a string
    ] {
        let doc = analyze(&format!("---\ncreated_at: {ts}\n---\nbody\n"), P);
        assert_eq!(doc.created_at == 0, want_zero, "created_at {ts}");
    }
}

#[test]
fn impact_and_supersedes_entries_validated() {
    let doc = analyze(
        "---\nimpacts:\n  - src/ok.rs\n  - ../escape\n  - /abs\n  - a\\\\b\n  - src/ok.rs\n  - 42\nsupersedes:\n  - .context/notes/ok-a1b2c3d4e5f60718.md\n  - src/not-context.rs\n  - .context/notes/2026-11-13-x-a1b2c3d4e5f60718.md\n---\nb\n",
        P,
    );
    let impacts: Vec<_> = doc.impacts.iter().map(|c| c.as_str()).collect();
    // Only src/ok.rs survives, deduplicated.
    assert_eq!(impacts, ["src/ok.rs"]);
    let sup: Vec<_> = doc.supersedes.iter().map(|c| c.as_str()).collect();
    // Outside .context and self-supersession are omitted.
    assert_eq!(sup, [".context/notes/ok-a1b2c3d4e5f60718.md"]);
    assert!(doc.warnings.len() >= 4);
}

#[test]
fn excess_list_entries_capped() {
    let many: String = (0..40).map(|i| format!("  - src/f{i}.rs\n")).collect();
    let doc = analyze(&format!("---\nimpacts:\n{many}---\nb\n"), P);
    assert_eq!(doc.impacts.len(), 32);
    assert!(doc.warnings.iter().any(|w| w.code == "EXCESS_ENTRIES"));
}

#[test]
fn title_truncated_at_200_scalars() {
    let long = "t".repeat(250);
    let doc = analyze(&format!("---\ntitle: {long}\n---\nb\n"), P);
    assert_eq!(doc.title.chars().count(), 200);
    assert!(doc.warnings.iter().any(|w| w.code == "TRUNCATED_METADATA"));
}

#[test]
fn heading_like_text_in_code_is_not_a_heading() {
    let body = "para\n\n```\n# fenced\n```\n\n~~~~\n# tilde fenced\n~~~~\n\n`````\n# long fence\n`````\n\n    # indented\n\nafter\n";
    let doc = analyze(&fm(body), P);
    assert!(doc.chunks.iter().all(|c| c.heading.is_empty()));
    let all: String = doc.chunks.iter().map(|c| c.text.as_str()).collect();
    assert!(all.contains("# fenced") && all.contains("# indented"));
}

#[test]
fn setext_heading_is_a_section() {
    let doc = analyze(&fm("Setext Title\n============\nbody\nunder\n"), P);
    assert_eq!(doc.chunks.len(), 1);
    assert_eq!(doc.chunks[0].heading, "Setext Title");
    assert!(doc.chunks[0].text.starts_with("Setext Title\n============"));
}

#[test]
fn whitespace_only_body_yields_no_chunks() {
    let doc = analyze(&fm("   \n\n\t\n"), P);
    assert!(doc.chunks.is_empty());
}

#[test]
fn long_section_splits_at_paragraphs() {
    let para = |n: usize| format!("{}\n", "x".repeat(n));
    // Three 800-scalar paragraphs: first two pack, third overflows.
    let body = format!("## S\n{}{}\n{}", para(800), para(800), para(800));
    let doc = analyze(&fm(&body), P);
    assert!(doc.chunks.len() >= 3);
    assert!(doc.chunks.iter().all(|c| c.heading == "S"));
    assert!(doc.chunks.iter().all(|c| c.text.chars().count() <= 1_500));
}

#[test]
fn long_paragraph_splits_at_lines() {
    let line = "y".repeat(400);
    // One section: heading + 10 lines of 400 scalars, no blank lines → one
    // oversized paragraph → split at line boundaries in packs of ~3.
    let body = format!(
        "## S\n{}",
        (0..10)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let doc = analyze(&fm(&body), P);
    assert!(doc.chunks.len() >= 3);
    assert!(doc.chunks.iter().all(|c| c.text.chars().count() <= 1_500));
}

#[test]
fn single_long_line_splits_at_scalars() {
    // Multi-byte characters must split at scalar boundaries, not bytes.
    let body = format!("## S\n{}", "\u{4e2d}".repeat(2_600));
    let doc = analyze(&fm(&body), P);
    let long_chunks: Vec<_> = doc
        .chunks
        .iter()
        .filter(|c| c.text.contains('\u{4e2d}'))
        .collect();
    assert_eq!(long_chunks.len(), 2);
    assert_eq!(long_chunks[0].text.chars().count(), 1_500);
    assert_eq!(long_chunks[1].text.chars().count(), 1_100);
    // Pieces of one source line share the line number.
    assert_eq!(long_chunks[0].start_line, long_chunks[1].start_line);
    assert_eq!(long_chunks[0].end_line, long_chunks[0].start_line);
}

#[test]
fn crlf_counts_as_one_line_break() {
    let doc = analyze("---\ntitle: T\r\n---\r\nline one\r\nline two\r\n", P);
    assert_eq!(doc.chunks.len(), 1);
    assert_eq!(doc.chunks[0].start_line, 4);
    assert_eq!(doc.chunks[0].end_line, 5);
}

#[test]
fn heading_truncated_at_300_scalars() {
    let doc = analyze(&fm(&format!("## {}\nx\n", "h".repeat(400))), P);
    assert_eq!(doc.chunks[0].heading.chars().count(), 300);
    assert!(doc.warnings.iter().any(|w| w.code == "TRUNCATED_METADATA"));
}

#[test]
fn bom_accepted_then_frontmatter() {
    let doc = analyze("\u{feff}---\ntitle: T\n---\nbody\n", P);
    assert!(!doc.malformed_frontmatter);
    assert_eq!(doc.title, "T");
}

#[test]
fn unknown_fields_ignored() {
    let doc = analyze("---\ntitle: T\nmystery: [1, {a: b}]\n---\nb\n", P);
    assert!(!doc.malformed_frontmatter);
    assert_eq!(doc.title, "T");
}

#[test]
fn deep_yaml_nesting_is_malformed() {
    let nested = format!("---\na: {}{}\n---\nb\n", "[".repeat(40), "]".repeat(40));
    let doc = analyze(&nested, P);
    assert!(doc.malformed_frontmatter);
}

#[test]
fn trailing_content_after_mapping_is_malformed() {
    let src = "---\ntitle: x\nkind: note\nscope: global\n---\njunk after\n";
    // Trailing content after a complete mapping makes the block malformed.
    let src2 = "---\ntitle: x\nkind: note\nscope: global\ntrailing: [unclosed\n---\nbody\n";
    assert!(analyze(src, ".context/notes/t.md").malformed_frontmatter == false);
    assert!(analyze(src2, ".context/notes/t.md").malformed_frontmatter == true);
}
