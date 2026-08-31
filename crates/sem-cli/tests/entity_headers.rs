//! The middle zoom: `sem entities --signatures` and `sem context --headers`.
//! A header is an entity's signature lines up to where its body starts plus
//! the first line of the doc comment immediately above it
//! (`sem_core::parser::header`). Both flags are strictly additive: with the
//! flag off, output is byte-identical to what the command always produced —
//! no `header` key in JSON, no extra text lines.

use std::{fs, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn fixture_repo() -> TempDir {
    let repo = TempDir::new().expect("tempdir");
    fs::write(
        repo.path().join("lib.rs"),
        r#"/// Adds one to the answer.
pub fn alpha() -> usize {
    beta() + 1
}

pub fn beta() -> usize {
    41
}
"#,
    )
    .expect("write lib.rs");
    fs::write(
        repo.path().join("util.py"),
        r#"# Greets nobody in particular.
def gamma():
    return "hello"
"#,
    )
    .expect("write util.py");
    repo
}

fn run_sem(repo: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .args(args)
        .output()
        .expect("run sem")
}

// ── entities --signatures ──

#[test]
fn entities_signatures_adds_header_lines_to_json() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["entities", "lib.rs", "--json", "--signatures"]);
    assert!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    let alpha = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "alpha")
        .expect("alpha row");
    let header: Vec<&str> = alpha["header"]
        .as_array()
        .expect("header array")
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(
        header,
        vec!["/// Adds one to the answer.", "pub fn alpha() -> usize {"],
        "doc first line + signature up to the body"
    );
}

#[test]
fn entities_without_signatures_flag_is_unchanged() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["entities", "lib.rs", "--json"]);
    assert!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    for row in rows.as_array().unwrap() {
        assert!(
            row.get("header").is_none(),
            "no header key without the flag: {row}"
        );
    }

    let with_flag = run_sem(&repo, &["entities", "lib.rs"]);
    let without_flag = run_sem(&repo, &["entities", "lib.rs"]);
    assert_eq!(
        with_flag.stdout, without_flag.stdout,
        "text output is deterministic and unchanged"
    );
}

#[test]
fn entities_signatures_text_mode_prints_header_under_each_row() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["entities", "util.py", "--signatures"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("def gamma():"),
        "signature line printed under the entity row: {stdout}"
    );
    assert!(
        stdout.contains("# Greets nobody in particular."),
        "doc line printed too: {stdout}"
    );
}

#[test]
fn entities_signatures_python_headers_stop_at_the_colon() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["entities", "util.py", "--json", "--signatures"]);
    assert!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    let gamma = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "gamma")
        .expect("gamma row");
    let header = gamma["header"].as_array().expect("header array");
    assert!(
        header
            .iter()
            .all(|l| !l.as_str().unwrap().contains("return")),
        "the body never leaks into a header: {header:?}"
    );
}

// ── context --headers ──

#[test]
fn context_headers_replaces_content_with_header_in_json() {
    let repo = fixture_repo();
    let output = run_sem(
        &repo,
        &["context", "alpha", "--json", "--no-cache", "--headers"],
    );
    assert!(
        output.status.success(),
        "context --headers failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out: Value = serde_json::from_slice(&output.stdout).expect("json");
    let entries = out["entries"].as_array().expect("entries");
    assert!(!entries.is_empty());
    for entry in entries {
        assert!(
            entry.get("content").is_none(),
            "headers mode carries no bodies: {entry}"
        );
        assert!(entry.get("header").is_some(), "header key present: {entry}");
    }
    let target = entries
        .iter()
        .find(|e| e["role"] == "target")
        .expect("target entry");
    assert_eq!(
        target["header"][0], "/// Adds one to the answer.",
        "target's header starts with its doc line"
    );
}

#[test]
fn context_without_headers_flag_is_unchanged() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["context", "alpha", "--json", "--no-cache"]);
    assert!(output.status.success());
    let out: Value = serde_json::from_slice(&output.stdout).expect("json");
    for entry in out["entries"].as_array().expect("entries") {
        assert!(
            entry.get("header").is_none(),
            "no header key without the flag: {entry}"
        );
        assert!(entry.get("content").is_some(), "content stays: {entry}");
    }
}

#[test]
fn context_headers_text_mode_prints_headers_not_bodies() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["context", "alpha", "--no-cache", "--headers"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pub fn alpha() -> usize {"),
        "target header shown: {stdout}"
    );
    assert!(
        !stdout.contains("beta() + 1"),
        "target body not shown in headers mode: {stdout}"
    );
}

#[test]
fn context_headers_applies_to_the_entity_batch_form() {
    let repo = fixture_repo();
    let output = run_sem(
        &repo,
        &[
            "context",
            "--entity",
            "alpha",
            "--entity",
            "beta",
            "--json",
            "--no-cache",
            "--headers",
        ],
    );
    assert!(
        output.status.success(),
        "batch headers failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "one object per entity");
    for line in lines {
        let obj: Value = serde_json::from_str(line).expect("entity json");
        for entry in obj["entries"].as_array().expect("entries") {
            assert!(entry.get("content").is_none(), "no bodies in batch headers");
        }
    }
}
