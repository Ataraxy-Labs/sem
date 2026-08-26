//! `sem callers` answers about exactly one entity: an ambiguous name is
//! refused with the full candidate list, `--file` (or a "type name" query)
//! picks one, and `--limit` caps the caller rows shown.

use std::{fs, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn fixture_repo() -> TempDir {
    let repo = TempDir::new().expect("tempdir");
    fs::write(
        repo.path().join("app.py"),
        r#"def target_fn():
    return 0


def caller_a():
    return target_fn()


def caller_b():
    return target_fn() + caller_a()
"#,
    )
    .expect("write app.py");
    fs::write(
        repo.path().join("dup.py"),
        "def dup_name():\n    return 1\n",
    )
    .expect("write dup.py");
    fs::write(
        repo.path().join("dup2.py"),
        "def dup_name():\n    return 2\n",
    )
    .expect("write dup2.py");
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

#[test]
fn callers_unique_name_lists_direct_callers() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["callers", "target_fn", "--json"]);
    assert!(
        output.status.success(),
        "unique-name callers failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 1, "one resolved definition");
    assert_eq!(rows[0]["entity"]["name"], "target_fn");
    assert_eq!(
        rows[0]["related"].as_array().unwrap().len(),
        2,
        "both direct callers listed"
    );
}

#[test]
fn callers_limit_caps_the_rows_shown() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["callers", "target_fn", "--limit", "1", "--json"]);
    assert!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(
        rows[0]["related"].as_array().unwrap().len(),
        1,
        "--limit 1 shows exactly one caller"
    );
}

#[test]
fn callers_ambiguous_name_is_refused_with_every_candidate() {
    let repo = fixture_repo();
    let output = run_sem(&repo, &["callers", "dup_name", "--json"]);
    assert!(
        !output.status.success(),
        "ambiguous name must be refused, not answered many times over"
    );
    let refusal: Value = serde_json::from_slice(&output.stdout).expect("refusal json");
    assert_eq!(refusal["resolved"], false);
    let candidates = refusal["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2, "every candidate definition listed");

    let text = run_sem(&repo, &["callers", "dup_name"]);
    assert!(!text.status.success());
    let stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        stderr.contains("dup.py") && stderr.contains("dup2.py"),
        "text refusal lists every candidate on stderr: {stderr}"
    );
}

#[test]
fn callers_file_flag_disambiguates() {
    let repo = fixture_repo();
    let output = run_sem(
        &repo,
        &["callers", "dup_name", "--file", "dup.py", "--json"],
    );
    assert!(
        output.status.success(),
        "--file picks one definition: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(rows[0]["entity"]["file"], "dup.py");
}
