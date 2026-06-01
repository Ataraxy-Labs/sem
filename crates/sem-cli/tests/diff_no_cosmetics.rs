use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct TestRepo {
    path: PathBuf,
    home: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after UNIX epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("sem-cli-{name}-{}-{nonce}", std::process::id()));
        let home = std::env::temp_dir().join(format!(
            "sem-cli-{name}-home-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temporary repo");
        fs::create_dir_all(&home).expect("create temporary home");

        git(&path, &["init", "-q"]);
        git(&path, &["config", "user.email", "test@example.com"]);
        git(&path, &["config", "user.name", "Test User"]);
        git(&path, &["config", "commit.gpgsign", "false"]);

        Self { path, home }
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");

    assert!(
        output.status.success(),
        "git {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sem(repo: &TestRepo, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(args)
        .current_dir(&repo.path)
        .env("HOME", &repo.home)
        .output()
        .expect("run sem")
}

#[test]
fn no_cosmetics_hides_comment_only_orphan_change() {
    let repo = TestRepo::new("diff-no-cosmetics-comment-orphan");
    fs::write(
        repo.path.join("app.py"),
        "# original comment\ndef foo():\n    return 1\n",
    )
    .expect("write initial source");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-q", "-m", "initial"]);

    fs::write(
        repo.path.join("app.py"),
        "# changed comment text\ndef foo():\n    return 1\n",
    )
    .expect("write changed source");

    let output = sem(&repo, &["diff", "--no-cosmetics", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["changes"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["summary"]["fileCount"], 0);
    assert_eq!(json["summary"]["total"], 0);
    assert_eq!(json["summary"]["orphan"], 0);
}

#[test]
fn no_cosmetics_hides_added_comment_only_orphan() {
    let repo = TestRepo::new("diff-no-cosmetics-added-comment-orphan");
    fs::write(repo.path.join("app.py"), "def foo():\n    return 1\n")
        .expect("write initial source");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-q", "-m", "initial"]);

    fs::write(
        repo.path.join("app.py"),
        "# added comment\ndef foo():\n    return 1\n",
    )
    .expect("write changed source");

    let output = sem(&repo, &["diff", "--no-cosmetics", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["changes"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["summary"]["total"], 0);
    assert_eq!(json["summary"]["orphan"], 0);
}

#[test]
fn no_cosmetics_keeps_orphan_code_addition_in_summary_bucket() {
    let repo = TestRepo::new("diff-no-cosmetics-code-orphan");
    fs::write(repo.path.join("app.py"), "def foo():\n    return 1\n")
        .expect("write initial source");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-q", "-m", "initial"]);

    fs::write(
        repo.path.join("app.py"),
        "import sys\n\ndef foo():\n    return 1\n",
    )
    .expect("write changed source");

    let output = sem(&repo, &["diff", "--no-cosmetics", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["changes"][0]["entityType"], "orphan");
    assert_eq!(json["changes"][0]["changeType"], "added");
    assert_eq!(json["changes"][0]["structuralChange"], true);
    assert_eq!(json["summary"]["added"], 1);
    assert_eq!(json["summary"]["total"], 1);
    assert_eq!(json["summary"]["orphan"], 1);
}

#[test]
fn no_cosmetics_keeps_shebang_change() {
    let repo = TestRepo::new("diff-no-cosmetics-shebang");
    fs::write(
        repo.path.join("script"),
        "#!/usr/bin/env python3\ndef foo():\n    return 1\n",
    )
    .expect("write initial source");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-q", "-m", "initial"]);

    fs::write(
        repo.path.join("script"),
        "#!/usr/bin/env python\ndef foo():\n    return 1\n",
    )
    .expect("write changed source");

    let output = sem(&repo, &["diff", "--no-cosmetics", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["changes"][0]["entityType"], "orphan");
    assert_eq!(json["changes"][0]["changeType"], "modified");
    assert_eq!(json["changes"][0]["structuralChange"], true);
    assert_eq!(json["summary"]["modified"], 1);
    assert_eq!(json["summary"]["total"], 1);
    assert_eq!(json["summary"]["orphan"], 1);
}
