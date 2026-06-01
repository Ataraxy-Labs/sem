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
            "sem-diff-patch-test-{}-{id}-{counter}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp repo");
        run_git(&path, &["init", "-q"]);
        run_git(&path, &["config", "user.name", "Test"]);
        run_git(&path, &["config", "user.email", "test@example.com"]);
        Self { path }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_sem(args: &[&str], input: &[u8], cwd: Option<&Path>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd.unwrap_or_else(|| Path::new(".")))
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

fn run_diff_patch(input: &str) -> Output {
    run_sem(&["diff", "--patch"], input.as_bytes(), None)
}

fn diff_patch_json(input: &[u8], cwd: &Path) -> serde_json::Value {
    let output = run_sem(&["diff", "--patch", "--json"], input, Some(cwd));
    assert!(
        output.status.success(),
        "sem diff --patch --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout should be json")
}

fn changed_app_patch() -> (TempRepo, Vec<u8>) {
    let repo = TempRepo::new();
    std::fs::write(
        repo.path.join("app.js"),
        "function greet() {\n  return \"hello\";\n}\n",
    )
    .expect("write initial file");
    run_git(&repo.path, &["add", "app.js"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
    std::fs::write(
        repo.path.join("app.js"),
        "function greet() {\n  return \"hello world\";\n}\n",
    )
    .expect("write changed file");

    let patch = run_git(&repo.path, &["diff", "--", "app.js"]).stdout;
    (repo, patch)
}

#[test]
fn patch_mode_rejects_empty_stdin() {
    let output = run_diff_patch("");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("error: no input on stdin (use --patch < file.diff)"));
}

#[test]
fn patch_mode_rejects_non_diff_stdin() {
    let output = run_diff_patch("this is not a diff\n");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains(
        "error: no recognizable diff hunks in stdin (expected 'diff --git' headers and '@@ ... @@' hunk markers)"
    ));
}

#[test]
fn patch_mode_rejects_truncated_metadata_patch() {
    let output = run_diff_patch("diff --git a/a.ts b/a.ts\nnew file mode 100644\n");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("error: no recognizable diff hunks in stdin"));
}

#[test]
fn patch_mode_rejects_truncated_git_binary_patch() {
    let output = run_diff_patch(
        "diff --git a/blob.bin b/blob.bin\n\
         index 1111111..2222222 100644\n\
         GIT binary patch\n\
         literal 1\n",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("error: no recognizable diff hunks in stdin"));
}

#[test]
fn patch_mode_warns_for_malformed_hunk_without_content_resolution_warning() {
    let output = run_diff_patch(
        "diff --git a/a.ts b/a.ts\n\
         --- a/a.ts\n\
         +++ b/a.ts\n\
         @@ NOTAHUNK @@\n\
         -foo\n\
         +bar\n",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success());
    assert!(stderr.contains(
        "warning: malformed hunk header in a.ts: '@@ NOTAHUNK @@' (expected '@@ -N,M +N,M @@')"
    ));
    assert!(!stderr.contains("could not resolve contents"));
    assert!(stdout.contains("No semantic changes detected."));
}

#[test]
fn patch_mode_accepts_hunkless_git_metadata_patches() {
    let output = run_diff_patch(
        "diff --git a/old.py b/new.py\n\
         similarity index 100%\n\
         rename from old.py\n\
         rename to new.py\n",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success());
    assert!(!stderr.contains("no recognizable diff hunks"));
}

#[test]
fn patch_mode_accepts_default_quoted_non_ascii_paths() {
    let repo = TempRepo::new();
    std::fs::write(repo.path.join("ünïcödé.py"), "def foo():\n    return 1\n")
        .expect("write initial file");
    run_git(&repo.path, &["add", "ünïcödé.py"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
    std::fs::write(repo.path.join("ünïcödé.py"), "def foo():\n    return 2\n")
        .expect("write changed file");
    run_git(&repo.path, &["add", "ünïcödé.py"]);

    let patch = run_git(
        &repo.path,
        &["-c", "core.quotepath=true", "diff", "--cached"],
    )
    .stdout;
    let patch_text = String::from_utf8_lossy(&patch);
    assert!(patch_text.contains("\"a/\\303\\274n\\303\\257c\\303\\266d\\303\\251.py\""));

    let json = diff_patch_json(&patch, &repo.path);
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["filePath"].as_str(), Some("ünïcödé.py"));
    assert_eq!(changes[0]["entityName"].as_str(), Some("foo"));
    assert_eq!(changes[0]["changeType"].as_str(), Some("modified"));
}

#[test]
fn patch_mode_preserves_paths_containing_b_prefix() {
    let repo = TempRepo::new();
    std::fs::create_dir(repo.path.join("my b")).expect("create directory");
    std::fs::write(repo.path.join("my b/app.py"), "def foo():\n    return 1\n")
        .expect("write initial file");
    run_git(&repo.path, &["add", "my b/app.py"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
    std::fs::write(repo.path.join("my b/app.py"), "def foo():\n    return 2\n")
        .expect("write changed file");
    run_git(&repo.path, &["add", "my b/app.py"]);

    let patch = run_git(
        &repo.path,
        &["-c", "core.quotepath=false", "diff", "--cached"],
    )
    .stdout;

    let json = diff_patch_json(&patch, &repo.path);
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["filePath"].as_str(), Some("my b/app.py"));
    assert_eq!(changes[0]["entityName"].as_str(), Some("foo"));
}

#[test]
fn patch_mode_reconstructs_indexless_hunk_content() {
    let repo = TempRepo::new();
    let patch = concat!(
        "diff --git a/app.py b/app.py\n",
        "--- a/app.py\n",
        "+++ b/app.py\n",
        "@@ -1,2 +1,2 @@\n",
        " def foo():\n",
        "-    return 1\n",
        "+    return 2\n",
    )
    .as_bytes();

    let json = diff_patch_json(patch, &repo.path);
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["filePath"].as_str(), Some("app.py"));
    assert_eq!(changes[0]["entityName"].as_str(), Some("foo"));
    assert_eq!(changes[0]["changeType"].as_str(), Some("modified"));
}

#[test]
fn patch_mode_uses_hunks_when_new_blob_is_absent() {
    let repo = TempRepo::new();
    std::fs::write(repo.path.join("app.py"), "def foo():\n    return 1\n")
        .expect("write initial file");
    run_git(&repo.path, &["add", "app.py"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
    let old_sha = run_git(&repo.path, &["rev-parse", "HEAD:app.py"]).stdout;
    let old_sha = String::from_utf8(old_sha).expect("old sha should be utf8");
    let patch = format!(
        concat!(
            "diff --git a/app.py b/app.py\n",
            "index {}..deadbee 100644\n",
            "--- a/app.py\n",
            "+++ b/app.py\n",
            "@@ -1,2 +1,2 @@\n",
            " def foo():\n",
            "-    return 1\n",
            "+    return 2\n",
        ),
        old_sha.trim()
    );

    let json = diff_patch_json(patch.as_bytes(), &repo.path);
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["filePath"].as_str(), Some("app.py"));
    assert_eq!(changes[0]["entityName"].as_str(), Some("foo"));
    assert_eq!(changes[0]["changeType"].as_str(), Some("modified"));
}

#[test]
fn patch_mode_filters_unmatched_literal_pathspec_after_separator() {
    let (repo, patch) = changed_app_patch();
    let output = run_sem(
        &["diff", "--patch", "--", "no/such/path"],
        &patch,
        Some(&repo.path),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(stdout.contains("No semantic changes detected."));
    assert!(!stdout.contains("greet"));
}

#[test]
fn patch_mode_treats_two_existing_raw_args_as_pathspecs() {
    let (repo, patch) = changed_app_patch();
    std::fs::write(repo.path.join("left.js"), "function left() {}\n").expect("write left");
    std::fs::write(repo.path.join("right.js"), "function right() {}\n").expect("write right");

    let output = run_sem(
        &["diff", "--patch", "left.js", "right.js"],
        &patch,
        Some(&repo.path),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(stdout.contains("No semantic changes detected."));
    assert!(!stdout.contains("left"));
    assert!(!stdout.contains("right"));
    assert!(!stdout.contains("greet"));
}

#[test]
fn patch_mode_accepts_more_than_two_raw_pathspecs() {
    let (repo, patch) = changed_app_patch();

    let output = run_sem(
        &["diff", "--patch", "left.js", "right.js", "third.js"],
        &patch,
        Some(&repo.path),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success());
    assert!(stdout.contains("No semantic changes detected."));
    assert!(!stderr.contains("too many positional arguments"));
}

#[test]
fn patch_mode_pathspec_matches_renamed_old_path() {
    let repo = TempRepo::new();
    std::fs::write(
        repo.path.join("old.js"),
        "function greet() {\n  return \"hello\";\n}\n",
    )
    .expect("write initial file");
    run_git(&repo.path, &["add", "old.js"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
    run_git(&repo.path, &["mv", "old.js", "new.js"]);
    std::fs::write(
        repo.path.join("new.js"),
        "function greet() {\n  return \"hello world\";\n}\n",
    )
    .expect("write renamed file");
    run_git(&repo.path, &["add", "-A"]);

    let patch = run_git(&repo.path, &["diff", "--cached", "-M", "--find-renames"]).stdout;
    let output = run_sem(&["diff", "--patch", "old.js"], &patch, Some(&repo.path));
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(stdout.contains("greet"));
    assert!(!stdout.contains("No semantic changes detected."));
}
