use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_REPO_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let counter = TEMP_REPO_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sem-cwd-resolution-test-{}-{id}-{counter}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp repo");
        run_git(&path, &["init", "-q"]);
        run_git(&path, &["config", "user.name", "Test"]);
        run_git(&path, &["config", "user.email", "test@example.com"]);
        Self { path }
    }

    fn subdir(&self) -> PathBuf {
        self.path.join("sub")
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_sem(args: &[&str], input: &[u8], cwd: &Path) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd)
        .spawn()
        .expect("spawn sem");

    child
        .stdin
        .take()
        .expect("open stdin")
        .write_all(input)
        .expect("write stdin");

    child.wait_with_output().expect("wait for sem")
}

fn write_sub_file(repo: &TempRepo, contents: &str) {
    std::fs::create_dir_all(repo.subdir()).expect("create subdir");
    std::fs::write(repo.subdir().join("foo.py"), contents).expect("write foo.py");
}

fn commit_sub_file(repo: &TempRepo) {
    run_git(&repo.path, &["add", "sub/foo.py"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
}

#[test]
fn verify_diff_finds_broken_callers_from_subdirectory() {
    let repo = TempRepo::new();
    write_sub_file(
        &repo,
        "def f(x, y):\n    return x + y\n\ndef g():\n    return f(1, 2)\n",
    );
    commit_sub_file(&repo);
    write_sub_file(
        &repo,
        "def f(x):\n    return x\n\ndef g():\n    return f(1, 2)\n",
    );

    let output = run_sem(&["verify", "--diff", "--json"], &[], &repo.subdir());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    let mismatches: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(mismatches[0]["callee"], "f");
    assert_eq!(mismatches[0]["caller"], "g");
    assert_eq!(mismatches[0]["file"], "sub/foo.py");
}

#[test]
fn diff_patch_reads_worktree_contents_from_repo_root() {
    let repo = TempRepo::new();
    write_sub_file(&repo, "def foo():\n    return 1\n");
    commit_sub_file(&repo);
    write_sub_file(&repo, "def foo():\n    return 2\n");

    let patch = run_git(&repo.subdir(), &["diff"]).stdout;
    let output = run_sem(&["diff", "--patch", "--json"], &patch, &repo.subdir());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stdout}");
    let diff: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(diff["summary"]["modified"], 1);
    assert_eq!(diff["summary"]["deleted"], 0);
}

#[test]
fn diff_patch_pathspecs_are_relative_to_invocation_directory() {
    let repo = TempRepo::new();
    write_sub_file(&repo, "def foo():\n    return 1\n");
    commit_sub_file(&repo);
    write_sub_file(&repo, "def foo():\n    return 2\n");

    let patch = run_git(&repo.subdir(), &["diff"]).stdout;
    let output = run_sem(
        &["diff", "--patch", "--json", "--", "foo.py"],
        &patch,
        &repo.subdir(),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stdout}");
    let diff: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(diff["summary"]["fileCount"], 1);
    assert_eq!(diff["changes"][0]["filePath"], "sub/foo.py");
}

#[test]
fn impact_and_context_file_hints_are_relative_to_invocation_directory() {
    let repo = TempRepo::new();
    write_sub_file(
        &repo,
        "def f():\n    return 1\n\ndef g():\n    return f()\n",
    );
    commit_sub_file(&repo);

    let impact = run_sem(
        &["impact", "f", "--file", "foo.py", "--json", "--no-cache"],
        &[],
        &repo.subdir(),
    );
    let impact_stdout = String::from_utf8_lossy(&impact.stdout);
    assert!(impact.status.success(), "{impact_stdout}");
    let impact_json: serde_json::Value = serde_json::from_str(&impact_stdout).expect("valid json");
    assert_eq!(impact_json["entity"]["file"], "sub/foo.py");

    let context = run_sem(
        &["context", "f", "--file", "foo.py", "--json", "--no-cache"],
        &[],
        &repo.subdir(),
    );
    let context_stdout = String::from_utf8_lossy(&context.stdout);
    assert!(context.status.success(), "{context_stdout}");
    let context_json: serde_json::Value =
        serde_json::from_str(&context_stdout).expect("valid json");
    assert_eq!(context_json["entries"][0]["file"], "sub/foo.py");
}
