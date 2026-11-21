//! `epocnote init` scaffolding, idempotency, ignore rules, and skill targets
//! (plan.md sections 5.1, 5.1.1, 13).

use epocnote::error::Error;
use epocnote::init::{InitReport, Target, run_init, skill_content};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn init(root: &Path, target: Target) -> InitReport {
    run_init(Some(root), root, target)
        .map(|(_, r)| r)
        .unwrap_or_else(|f| panic!("init failed: {} (actions: {:?})", f.error, f.report.actions))
}

fn labels(report: &InitReport) -> Vec<String> {
    report.actions.iter().map(ToString::to_string).collect()
}

fn gitignore_lines(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join(".gitignore"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn init_creates_context_layout() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let report = init(root, Target::None);

    assert!(root.join(".context/notes").is_dir());
    let arch = fs::read_to_string(root.join(".context/architecture.md")).unwrap();
    assert!(arch.starts_with("---\n"));
    assert!(arch.contains("kind: architecture"));
    assert!(arch.contains("scope: global"));
    assert!(arch.contains("impacts: []"));
    // Server-generated RFC 3339 second-precision timestamp.
    assert!(arch.contains("created_at: \"20"));
    // Placeholders are marked and no project facts are invented.
    assert!(arch.contains("TODO"));
    assert!(arch.contains("## Boundaries"));
    assert!(arch.contains("## Dependencies"));
    assert!(arch.contains("## Constraints"));

    let lines = gitignore_lines(root);
    assert!(lines.contains(&"/.context/.epocnote.sqlite*".to_string()));
    assert!(lines.contains(&"/.context/notes/.epocnote-tmp-*".to_string()));

    // --target none installs no skill directories.
    for d in [".agents", ".claude", ".cursor"] {
        assert!(!root.join(d).exists(), "{d} should not exist");
    }
    assert!(
        labels(&report)
            .iter()
            .any(|a| a.contains("no repository skill"))
    );
}

#[test]
fn init_is_idempotent_and_preserves_edits() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    init(root, Target::Codex);
    let arch = root.join(".context/architecture.md");
    let skill = root.join(".agents/skills/epocnote/SKILL.md");
    fs::write(&arch, "custom architecture\n").unwrap();
    fs::write(&skill, "custom skill\n").unwrap();
    fs::write(root.join(".gitignore"), "target/\nnode_modules/\n").unwrap();

    let report = init(root, Target::Codex);
    assert_eq!(fs::read_to_string(&arch).unwrap(), "custom architecture\n");
    assert_eq!(fs::read_to_string(&skill).unwrap(), "custom skill\n");
    let lines = gitignore_lines(root);
    assert!(lines.contains(&"node_modules/".to_string()));
    assert_eq!(
        lines
            .iter()
            .filter(|l| *l == "/.context/.epocnote.sqlite*")
            .count(),
        1
    );
    let text = labels(&report).join("\n");
    assert!(text.contains("preserved .context/architecture.md"));
    assert!(text.contains("preserved .agents/skills/epocnote/SKILL.md"));
}

#[test]
fn init_installs_each_harness_skill() {
    for (target, rel) in [
        (Target::Codex, ".agents/skills/epocnote/SKILL.md"),
        (Target::ClaudeCode, ".claude/skills/epocnote/SKILL.md"),
        (Target::Cursor, ".cursor/skills/epocnote/SKILL.md"),
    ] {
        let tmp = TempDir::new().unwrap();
        init(tmp.path(), target);
        let content = fs::read_to_string(tmp.path().join(rel)).unwrap();
        assert!(
            content.starts_with("---\nname: epocnote\ndescription: Retrieve project architecture"),
            "{rel} manifest header wrong"
        );
        assert!(content.contains("epocnote_search"), "{rel} missing policy");
        // One skill only: no cross-harness directories.
        let others = [".agents", ".claude", ".cursor"]
            .iter()
            .filter(|d| !rel.starts_with(**d))
            .count();
        assert_eq!(others, 2, "{rel}: expected only its own skill dir");
        for d in [".agents", ".claude", ".cursor"] {
            if !rel.starts_with(d) {
                assert!(!tmp.path().join(d).exists());
            }
        }
    }
}

#[test]
fn init_cursor_reuses_identical_compatible_copy() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let compat = root.join(".agents/skills/epocnote");
    fs::create_dir_all(&compat).unwrap();
    fs::write(compat.join("SKILL.md"), skill_content()).unwrap();

    let report = init(root, Target::Cursor);
    assert!(!root.join(".cursor").exists());
    assert!(labels(&report).iter().any(|a| a.contains("cursor reuses")));
    assert!(report.warnings.is_empty());
}

#[test]
fn init_cursor_reports_conflict_on_differing_copy() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let compat = root.join(".claude/skills/epocnote");
    fs::create_dir_all(&compat).unwrap();
    fs::write(compat.join("SKILL.md"), "a different skill\n").unwrap();

    let report = init(root, Target::Cursor);
    assert!(!root.join(".cursor").exists());
    // The differing copy is preserved.
    assert_eq!(
        fs::read_to_string(compat.join("SKILL.md")).unwrap(),
        "a different skill\n"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.code == "SKILL_POLICY_CONFLICT")
    );
}

#[test]
fn init_cursor_creates_when_no_compatible_copy() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // A same-named skill elsewhere is not ours: still create the cursor copy.
    let other = root.join("unrelated/skills/epocnote");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("SKILL.md"), skill_content()).unwrap();
    init(root, Target::Cursor);
    assert!(root.join(".cursor/skills/epocnote/SKILL.md").is_file());
}

#[test]
fn init_gitignore_appends_with_separator() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(root.join(".gitignore"), "target/").unwrap(); // no trailing newline
    init(root, Target::None);
    let content = fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(content.starts_with("target/\n/.context/.epocnote.sqlite*\n"));
    assert!(content.contains("/.context/notes/.epocnote-tmp-*\n"));
}

#[test]
fn init_errors_on_unsafe_scaffold_targets() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // A directory named .gitignore is a non-regular scaffold target.
    fs::create_dir(root.join(".gitignore")).unwrap();
    let err = run_init(Some(root), root, Target::None).unwrap_err();
    assert!(matches!(err.error, Error::UnsafePath(_)));
    // Validation runs before any writes.
    assert!(!root.join(".context").exists());
}

#[cfg(unix)]
#[test]
fn init_rejects_symlinked_targets() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let c1 = root.join("c1");
    fs::create_dir(&c1).unwrap();
    symlink("/tmp", c1.join(".context")).unwrap();
    let err = run_init(Some(&c1), &c1, Target::None).unwrap_err();
    assert!(matches!(err.error, Error::UnsafePath(_)));

    let c2 = root.join("c2");
    fs::create_dir(&c2).unwrap();
    symlink("/tmp", c2.join(".gitignore")).unwrap();
    let err = run_init(Some(&c2), &c2, Target::None).unwrap_err();
    assert!(matches!(err.error, Error::UnsafePath(_)));
    assert!(!c2.join(".context").exists());

    let c3 = root.join("c3");
    fs::create_dir(&c3).unwrap();
    symlink("/tmp", c3.join(".agents")).unwrap();
    let err = run_init(Some(&c3), &c3, Target::Codex).unwrap_err();
    assert!(matches!(err.error, Error::UnsafePath(_)));
}

#[test]
fn init_explicit_root_resolves_directly() {
    let tmp = TempDir::new().unwrap();
    let nested = tmp.path().join("a/b/c");
    fs::create_dir_all(&nested).unwrap();
    let (root, _) = run_init(Some(&nested), tmp.path(), Target::None).unwrap();
    assert_eq!(root, nested.canonicalize().unwrap());
}

#[test]
fn concurrent_init_preserves_bytes() {
    let tmp = TempDir::new().unwrap();
    let root: PathBuf = tmp.path().to_path_buf();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let r = root.clone();
        handles.push(std::thread::spawn(move || {
            run_init(Some(&r), &r, Target::Codex)
        }));
    }
    for h in handles {
        h.join().unwrap().unwrap();
    }
    let arch = fs::read_to_string(root.join(".context/architecture.md")).unwrap();
    assert!(arch.starts_with("---\ntitle: \"Project architecture\""));
    assert!(arch.ends_with("-->\n"));
    // Every pre-existing gitignore byte survived; duplicate lines are allowed.
    let lines = gitignore_lines(&root);
    assert!(
        lines
            .iter()
            .all(|l| l.starts_with("/.context/") || l.is_empty())
    );
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn init_verifies_git_ignores_database() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    assert!(git(root, &["init", "-q"]));
    let report = init(root, Target::None);
    for probe in [
        ".context/.epocnote.sqlite",
        ".context/.epocnote.sqlite-wal",
        ".context/.epocnote.sqlite-shm",
        ".context/notes/.epocnote-tmp-x",
    ] {
        assert!(git(root, &["check-ignore", "-q", "--", probe]), "{probe}");
    }
    assert!(labels(&report).iter().any(|a| a.contains("git verifies")));

    // A later overriding rule is reported as a conflict, not success.
    let mut ignore = fs::read_to_string(root.join(".gitignore")).unwrap();
    ignore.push_str("!.context/.epocnote.sqlite*\n");
    fs::write(root.join(".gitignore"), ignore).unwrap();
    let report = init(root, Target::None);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.code == "IGNORE_RULE_CONFLICT")
    );
}

#[test]
fn oversized_gitignore_tail_handling() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    // 8 MiB of comment lines, no trailing newline.
    let mut g = String::new();
    while g.len() < 8 * 1024 * 1024 {
        g.push_str("# filler comment line\n");
    }
    g.push_str("# last line without newline");
    std::fs::write(root.join(".gitignore"), &g).unwrap();
    run_init(Some(root), root, Target::None).unwrap();
    let c1 = std::fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(c1.contains("# last line without newline\n/.context/.epocnote.sqlite*\n"));
    run_init(Some(root), root, Target::None).unwrap();
    let c2 = std::fs::read_to_string(root.join(".gitignore")).unwrap();
    assert_eq!(c1, c2, "second init must not duplicate entries");
}
