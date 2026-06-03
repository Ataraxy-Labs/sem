use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run git");

    assert!(
        output.status.success(),
        "git {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", message]);
}

fn init_repo() -> TempDir {
    let repo = TempDir::new().expect("create temporary repo");
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test User"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    repo
}

fn sem_log_json(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo)
        .args(["log"])
        .args(args)
        .args(["--json"])
        .output()
        .expect("run sem log")
}

#[test]
fn log_limit_bounds_history_scan_for_deleted_entities() {
    let repo = init_repo();

    fs::write(repo.path().join("a.py"), "def foo():\n    return 1\n").expect("write a.py");
    commit_all(repo.path(), "add foo");

    fs::write(repo.path().join("a.py"), "def foo():\n    return 2\n").expect("modify a.py");
    commit_all(repo.path(), "modify foo");

    fs::write(repo.path().join("a.py"), "").expect("delete foo");
    commit_all(repo.path(), "delete foo");

    fs::write(repo.path().join("b.py"), "def bar():\n    return 1\n").expect("write b.py");
    commit_all(repo.path(), "add bar");

    fs::write(repo.path().join("c.py"), "def baz():\n    return 1\n").expect("write c.py");
    commit_all(repo.path(), "add baz");

    let output = sem_log_json(repo.path(), &["foo", "--file", "a.py", "--limit", "2"]);
    assert!(
        output.status.success(),
        "sem log failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("parse json");
    let changes = json["changes"].as_array().expect("changes array");
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["change_type"], "modified (logic)");
    assert_eq!(changes[0]["commit"]["message"], "modify foo");
    assert_eq!(changes[1]["change_type"], "deleted");
    assert_eq!(changes[1]["commit"]["message"], "delete foo");
}

#[test]
fn log_limit_is_not_applied_as_result_entry_truncation() {
    let repo = init_repo();

    fs::write(repo.path().join("a.py"), "def foo():\n    return 1\n").expect("write a.py");
    commit_all(repo.path(), "v1");

    fs::write(repo.path().join("a.py"), "def foo():\n    return 2\n").expect("modify a.py");
    commit_all(repo.path(), "v2");

    fs::write(repo.path().join("a.py"), "def foo():\n    return 3\n").expect("modify a.py");
    commit_all(repo.path(), "v3");

    fs::write(repo.path().join("a.py"), "def foo():\n    return 4\n").expect("modify a.py");
    commit_all(repo.path(), "v4");

    let output = sem_log_json(repo.path(), &["foo", "--file", "a.py", "--limit", "2"]);
    assert!(
        output.status.success(),
        "sem log failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("parse json");
    let changes = json["changes"].as_array().expect("changes array");
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["change_type"], "modified (logic)");
    assert_eq!(changes[0]["commit"]["message"], "v3");
    assert_eq!(changes[1]["change_type"], "modified (logic)");
    assert_eq!(changes[1]["commit"]["message"], "v4");
}
