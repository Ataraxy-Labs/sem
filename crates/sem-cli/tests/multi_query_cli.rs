//! Multi-query forms of `sem find` / `sem grep` / `sem context`: several
//! names, patterns, or entities in one invocation, each resolved through the
//! exact same machinery as the single form — and the single form's output
//! staying byte-identical to what it always produced.

use std::{fs, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn fixture_repo() -> TempDir {
    let repo = TempDir::new().expect("tempdir");
    fs::write(
        repo.path().join("lib.rs"),
        r#"pub fn alpha() -> usize {
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
        r#"def gamma():
    return "needle-one"


def delta():
    return "needle-two"
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

// ── find ──

#[test]
fn find_single_query_output_is_unchanged() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["find", "alpha", "--json"]);
    assert!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 1, "single-query stays a bare match array");
    assert_eq!(rows[0]["name"], "alpha");
    assert_eq!(rows[0]["file"], "lib.rs");
    // The single form's shape has no batch wrapper.
    assert!(rows[0].get("query").is_none());
}

#[test]
fn find_multi_query_resolves_each_name_independently() {
    let repo = fixture_repo();
    let output = run_sem(
        &repo,
        &["find", "alpha", "no_such_entity", "gamma", "--json"],
    );
    assert!(
        output.status.success(),
        "a miss on one name must not fail the batch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 3, "one entry per query, in order");
    assert_eq!(rows[0]["query"], "alpha");
    assert_eq!(rows[0]["matches"][0]["file"], "lib.rs");
    assert_eq!(rows[1]["query"], "no_such_entity");
    assert_eq!(rows[1]["matches"].as_array().unwrap().len(), 0);
    assert_eq!(rows[2]["query"], "gamma");
    assert_eq!(rows[2]["matches"][0]["file"], "util.py");
}

#[test]
fn find_multi_query_fails_only_when_every_name_misses() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["find", "nope_a", "nope_b", "--json"]);
    assert!(
        !output.status.success(),
        "all-miss batch should exit non-zero"
    );
}

// ── grep ──

#[test]
fn grep_single_pattern_output_is_unchanged() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["grep", "needle-one", "--json"]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("json");
    // The single form's shape: one object, not an array of per-pattern results.
    assert!(report.get("hits").is_some());
    assert!(report.get("pattern").is_none());
    assert_eq!(report["hits"][0]["file"], "util.py");
}

#[test]
fn grep_repeated_e_keeps_each_patterns_hits_separate() {
    let repo = fixture_repo();
    let output = run_sem(
        &repo,
        &["grep", "-e", "needle-one", "-e", "needle-two", "--json"],
    );
    assert!(output.status.success());
    let results: Value = serde_json::from_slice(&output.stdout).expect("json");
    let results = results.as_array().expect("array");
    assert_eq!(results.len(), 2, "one entry per pattern, never merged");
    assert_eq!(results[0]["pattern"], "needle-one");
    assert_eq!(results[0]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(results[1]["pattern"], "needle-two");
    assert_eq!(results[1]["hits"].as_array().unwrap().len(), 1);
}

#[test]
fn grep_single_e_behaves_like_the_positional_form() {
    let repo = fixture_repo();
    let positional = run_sem(&repo, &["grep", "needle-one", "--json"]);
    let flagged = run_sem(&repo, &["grep", "-e", "needle-one", "--json"]);
    assert!(positional.status.success());
    assert!(flagged.status.success());
    assert_eq!(
        positional.stdout, flagged.stdout,
        "one -e is the single form, byte-identical"
    );
}

// ── context ──

#[test]
fn context_multiple_entities_pack_one_block_each() {
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
        ],
    );
    assert!(
        output.status.success(),
        "batch context failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // One JSON object per line, one per entity, in order.
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "one packed context per entity:\n{stdout}");
    let first: Value = serde_json::from_str(lines[0]).expect("first entity json");
    let second: Value = serde_json::from_str(lines[1]).expect("second entity json");
    assert_eq!(first["entity"], "alpha");
    assert_eq!(second["entity"], "beta");
}

#[test]
fn context_batch_refuses_unresolved_names_like_the_single_form() {
    let repo = fixture_repo();
    let output = run_sem(
        &repo,
        &[
            "context",
            "--entity",
            "alpha",
            "--entity",
            "no_such_entity",
            "--json",
            "--no-cache",
        ],
    );
    assert!(
        !output.status.success(),
        "an unresolved name refuses exactly like the single-entity form"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found"),
        "refusal names the problem: {stderr}"
    );
}
