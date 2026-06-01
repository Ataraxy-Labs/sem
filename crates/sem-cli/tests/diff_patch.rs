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
fn json_empty_result_shape_matches_when_clean_or_filtered_empty() {
    let repo = TempRepo::new();
    std::fs::write(repo.path.join("app.py"), "def foo():\n    return 1\n").expect("write app");
    run_git(&repo.path, &["add", "app.py"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);

    let clean = run_sem(&["diff", "--json"], &[], Some(&repo.path));
    assert!(clean.status.success());

    std::fs::write(repo.path.join("app.py"), "def foo():\n        return 1\n").expect("edit app");

    let filtered = run_sem(&["diff", "--json", "--no-cosmetics"], &[], Some(&repo.path));
    assert!(filtered.status.success());

    assert_eq!(clean.stdout, filtered.stdout);
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
