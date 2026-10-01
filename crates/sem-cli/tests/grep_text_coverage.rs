use serde_json::Value;
use std::{fs, process::Command};
use tempfile::TempDir;

fn search(repo: &TempDir) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .args(["grep", "coverage_marker", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn grep_finds_unparsed_text_before_and_after_structural_index() {
    let repo = TempDir::new().unwrap();
    fs::write(repo.path().join("lib.rs"), "fn coverage_marker() {}\n").unwrap();
    for file in ["OSXScreen.mm", "notes.unknown", "BUILD_CUSTOM"] {
        fs::write(repo.path().join(file), "coverage_marker\n").unwrap();
    }
    fs::write(repo.path().join("binary.unknown"), b"coverage_marker\0\n").unwrap();
    fs::write(repo.path().join(".hidden"), "coverage_marker\n").unwrap();
    fs::write(repo.path().join("ignored.mm"), "coverage_marker\n").unwrap();
    fs::write(repo.path().join(".semignore"), "ignored.mm\n").unwrap();
    let cold = search(&repo);
    let files: Vec<_> = cold["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["file"].as_str().unwrap())
        .collect();
    assert_eq!(
        files,
        ["BUILD_CUSTOM", "OSXScreen.mm", "lib.rs", "notes.unknown"]
    );
    assert!(cold["coverage"]
        .as_str()
        .unwrap()
        .contains("exclusions_apply"));
    let build = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .args(["find", "coverage_marker", "--json"])
        .output()
        .unwrap();
    assert!(build.status.success());
    assert_eq!(search(&repo)["hits"], cold["hits"]);
    fs::write(repo.path().join("new.mm"), "coverage_marker\n").unwrap();
    assert_eq!(search(&repo)["hits"].as_array().unwrap().len(), 5);
}
