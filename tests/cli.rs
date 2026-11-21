//! CLI surface checks: valid argument shapes are accepted and invalid ones are
//! rejected with a nonzero exit before any work happens (plan.md sections 5
//! and 11).

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_epocnote"))
        .args(args)
        .output()
        .unwrap()
}

fn assert_rejected(args: &[&str]) {
    let out = run(args);
    assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "{args:?} produced no stderr diagnostic"
    );
}

#[test]
fn help_and_version_succeed() {
    assert!(run(&["--help"]).status.success());
    assert!(run(&["--version"]).status.success());
    for cmd in ["init", "serve", "rebuild", "check"] {
        assert!(
            run(&[cmd, "--help"]).status.success(),
            "{cmd} --help failed"
        );
    }
}

#[test]
fn valid_argument_shapes_are_accepted() {
    // init is implemented and creates real scaffolds: it runs in a temp dir.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_str().unwrap();
    let out = run(&["init", "--root", root, "--target", "none"]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tmp.path().join(".context/architecture.md").is_file());
    for target in ["codex", "claude-code", "cursor"] {
        let dir = tempfile::TempDir::new().unwrap();
        let out = run(&[
            "init",
            "--root",
            dir.path().to_str().unwrap(),
            "--target",
            target,
        ]);
        assert!(
            out.status.success(),
            "init --target {target}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(run(&["rebuild", "--root", root]).status.success());
    assert!(run(&["check", "--root", root, "--strict"]).status.success());

    // The server stub still accepts valid options.
    for args in [
        ["serve", "--root", root, "--scan-reuse", "auto"].as_slice(),
        ["serve", "--scan-reuse", "off"].as_slice(),
    ] {
        let out = run(args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn invalid_values_are_rejected_before_work() {
    for args in [
        ["init", "--target", "emacs"].as_slice(), // unknown harness target
        ["init", "--target"].as_slice(),          // missing value
        ["serve", "--scan-reuse", "sometimes"].as_slice(),
        ["serve", "--scan-reuse", "on"].as_slice(),
        ["check", "--strict", "yes"].as_slice(), // flag takes no value
        ["rebuild", "--target", "codex"].as_slice(), // option on wrong command
        ["init", "--unknown"].as_slice(),
        ["explode"].as_slice(),       // unknown subcommand
        ["init", "extra"].as_slice(), // positional argument
    ] {
        assert_rejected(args);
    }
}
