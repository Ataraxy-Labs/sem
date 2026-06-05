use std::collections::BTreeSet;
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

fn run_sem_with_path(
    args: &[&str],
    input: &[u8],
    cwd: Option<&Path>,
    path: Option<&Path>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sem"));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd.unwrap_or_else(|| Path::new(".")));
    if let Some(path) = path {
        command.env("PATH", path);
    }

    let mut child = command.spawn().expect("spawn sem");

    child
        .stdin
        .take()
        .expect("open stdin")
        .write_all(input)
        .expect("write stdin");

    child.wait_with_output().expect("wait for sem")
}

fn run_sem(args: &[&str], input: &[u8], cwd: Option<&Path>) -> Output {
    run_sem_with_path(args, input, cwd, None)
}

fn run_diff_patch(input: &str) -> Output {
    run_sem(&["diff", "--patch"], input.as_bytes(), None)
}

fn json_file_paths(output: &Output) -> BTreeSet<String> {
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    json["changes"]
        .as_array()
        .expect("changes should be an array")
        .iter()
        .filter_map(|change| change["filePath"].as_str().map(str::to_owned))
        .collect()
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
    std::fs::write(
        repo.path.join("app.py"),
        "def foo():\n    return 1\n\n\ndef bar():\n    return 3\n",
    )
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
fn patch_mode_resolves_blobs_and_worktree_hash_without_git_executable() {
    let repo = TempRepo::new();
    std::fs::write(repo.path.join("app.py"), "def foo():\n    return 1\n")
        .expect("write initial file");
    run_git(&repo.path, &["add", "app.py"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
    let old_sha = run_git(&repo.path, &["rev-parse", "HEAD:app.py"]).stdout;
    let old_sha = String::from_utf8(old_sha).expect("old sha should be utf8");

    std::fs::write(repo.path.join("app.py"), "def foo():\n    return 2\n")
        .expect("write changed file");
    let new_sha = run_git(&repo.path, &["hash-object", "--", "app.py"]).stdout;
    let new_sha = String::from_utf8(new_sha).expect("new sha should be utf8");
    let patch = format!(
        concat!(
            "diff --git a/app.py b/app.py\n",
            "old mode 100644\n",
            "new mode 100755\n",
            "index {}..{} 100644\n",
        ),
        old_sha.trim(),
        new_sha.trim()
    );

    let empty_path = repo.path.join("empty-path");
    std::fs::create_dir(&empty_path).expect("create empty PATH directory");
    let output = run_sem_with_path(
        &["diff", "--patch", "--json"],
        patch.as_bytes(),
        Some(&repo.path),
        Some(&empty_path),
    );
    assert!(
        output.status.success(),
        "sem diff --patch --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("could not resolve contents"), "{stderr}");

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["filePath"].as_str(), Some("app.py"));
    assert_eq!(changes[0]["entityName"].as_str(), Some("foo"));
    assert_eq!(changes[0]["changeType"].as_str(), Some("modified"));
    assert_eq!(json["binaryChanges"].as_array().unwrap().len(), 0);
}

#[test]
fn single_colon_commit_message_rev_is_not_treated_as_pathspec() {
    let repo = TempRepo::new();
    std::fs::write(repo.path.join("app.py"), "def foo():\n    return 1\n")
        .expect("write initial file");
    run_git(&repo.path, &["add", "app.py"]);
    run_git(&repo.path, &["commit", "-qm", "init"]);
    std::fs::write(repo.path.join("app.py"), "def foo():\n    return 2\n")
        .expect("write changed file");

    let output = run_sem(&["diff", ":/init", "--json"], &[], Some(&repo.path));
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["filePath"].as_str(), Some("app.py"));
    assert_eq!(changes[0]["entityName"].as_str(), Some("foo"));
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

#[test]
fn patch_mode_directory_pathspec_does_not_match_sibling_prefixes() {
    let repo = TempRepo::new();
    std::fs::create_dir(repo.path.join("src")).expect("create src");
    std::fs::create_dir(repo.path.join("src2")).expect("create src2");
    std::fs::write(repo.path.join("src/a.py"), "def a():\n    return 1\n").expect("write src file");
    std::fs::write(repo.path.join("src2/a.py"), "def b():\n    return 1\n")
        .expect("write src2 file");
    run_git(&repo.path, &["add", "."]);
    run_git(&repo.path, &["commit", "-qm", "init"]);

    std::fs::write(repo.path.join("src/a.py"), "def a():\n    return 2\n")
        .expect("change src file");
    std::fs::write(repo.path.join("src2/a.py"), "def b():\n    return 2\n")
        .expect("change src2 file");

    let patch = run_git(&repo.path, &["diff"]).stdout;
    let output = run_sem(
        &["diff", "--patch", "--json", "--", "src/"],
        &patch,
        Some(&repo.path),
    );

    let paths = json_file_paths(&output);
    assert_eq!(paths, BTreeSet::from(["src/a.py".to_string()]));
}

#[test]
fn patch_mode_pathspec_matches_bracket_character_classes() {
    let repo = TempRepo::new();
    std::fs::write(repo.path.join("a1.py"), "def f():\n    return 1\n").expect("write a1");
    std::fs::write(repo.path.join("a2.py"), "def g():\n    return 1\n").expect("write a2");
    std::fs::write(repo.path.join("a3.py"), "def h():\n    return 1\n").expect("write a3");
    std::fs::write(repo.path.join("a[12].py"), "def literal():\n    return 1\n")
        .expect("write literal bracket path");
    run_git(&repo.path, &["add", "."]);
    run_git(&repo.path, &["commit", "-qm", "init"]);

    std::fs::write(repo.path.join("a1.py"), "def f():\n    return 2\n").expect("change a1");
    std::fs::write(repo.path.join("a2.py"), "def g():\n    return 2\n").expect("change a2");
    std::fs::write(repo.path.join("a3.py"), "def h():\n    return 2\n").expect("change a3");
    std::fs::write(repo.path.join("a[12].py"), "def literal():\n    return 2\n")
        .expect("change literal bracket path");
    run_git(&repo.path, &["add", "."]);

    let patch = run_git(&repo.path, &["diff", "--cached"]).stdout;
    let output = run_sem(
        &["diff", "--patch", "--json", "--", "a[12].py"],
        &patch,
        Some(&repo.path),
    );

    let paths = json_file_paths(&output);
    assert_eq!(
        paths,
        BTreeSet::from([
            "a1.py".to_string(),
            "a2.py".to_string(),
            "a[12].py".to_string(),
        ])
    );
}

#[test]
fn patch_mode_pathspec_matches_posix_bracket_classes() {
    let repo = TempRepo::new();
    std::fs::write(repo.path.join("a1.py"), "def f():\n    return 1\n").expect("write a1");
    std::fs::write(repo.path.join("a2.py"), "def g():\n    return 1\n").expect("write a2");
    std::fs::write(repo.path.join("aa.py"), "def h():\n    return 1\n").expect("write aa");
    run_git(&repo.path, &["add", "."]);
    run_git(&repo.path, &["commit", "-qm", "init"]);

    std::fs::write(repo.path.join("a1.py"), "def f():\n    return 2\n").expect("change a1");
    std::fs::write(repo.path.join("a2.py"), "def g():\n    return 2\n").expect("change a2");
    std::fs::write(repo.path.join("aa.py"), "def h():\n    return 2\n").expect("change aa");
    run_git(&repo.path, &["add", "."]);

    let patch = run_git(&repo.path, &["diff", "--cached"]).stdout;
    let output = run_sem(
        &["diff", "--patch", "--json", "--", "a[[:digit:]].py"],
        &patch,
        Some(&repo.path),
    );

    let paths = json_file_paths(&output);
    assert_eq!(
        paths,
        BTreeSet::from(["a1.py".to_string(), "a2.py".to_string()])
    );
}

#[test]
fn patch_mode_pathspec_wildcards_match_nested_paths_like_git() {
    let repo = TempRepo::new();
    std::fs::create_dir_all(repo.path.join("src/nested")).expect("create nested dir");
    std::fs::write(repo.path.join("root.py"), "def root():\n    return 1\n").expect("write root");
    std::fs::write(
        repo.path.join("src/nested/child.py"),
        "def child():\n    return 1\n",
    )
    .expect("write child");
    run_git(&repo.path, &["add", "."]);
    run_git(&repo.path, &["commit", "-qm", "init"]);

    std::fs::write(repo.path.join("root.py"), "def root():\n    return 2\n").expect("change root");
    std::fs::write(
        repo.path.join("src/nested/child.py"),
        "def child():\n    return 2\n",
    )
    .expect("change child");
    run_git(&repo.path, &["add", "."]);

    let patch = run_git(&repo.path, &["diff", "--cached"]).stdout;
    let output = run_sem(
        &["diff", "--patch", "--json", "--", "*.py"],
        &patch,
        Some(&repo.path),
    );

    let paths = json_file_paths(&output);
    assert_eq!(
        paths,
        BTreeSet::from(["root.py".to_string(), "src/nested/child.py".to_string(),])
    );
}
