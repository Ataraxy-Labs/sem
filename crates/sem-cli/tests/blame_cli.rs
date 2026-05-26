use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn git(repo: &TempDir, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo.path())
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

#[test]
fn blame_json_marks_entity_with_uncommitted_line() {
    let repo = TempDir::new().unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t.com"]);
    git(&repo, &["config", "user.name", "test"]);

    fs::write(repo.path().join("a.py"), "def foo():\n    return 1\n").unwrap();
    git(&repo, &["add", "a.py"]);
    let status = Command::new("git")
        .current_dir(repo.path())
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .args(["commit", "-q", "-m", "init"])
        .status()
        .unwrap();
    assert!(status.success(), "git commit failed");

    fs::write(repo.path().join("a.py"), "def foo():\n    return 2\n").unwrap();

    let sem = env!("CARGO_BIN_EXE_sem");
    let output = Command::new(sem)
        .current_dir(repo.path())
        .args(["blame", "a.py", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json[0]["name"], "foo");
    assert_eq!(json[0]["author"], "Not Committed Yet");
    assert!(json[0]["commit"].is_null());
}
