#![cfg(unix)]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "sem-cli-{name}-{}-{nanos}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct IsolatedEnv {
    _tmp: TempDir,
    home: PathBuf,
    gitconfig: PathBuf,
    path: OsString,
}

impl IsolatedEnv {
    fn new(name: &str) -> Self {
        let tmp = TempDir::new(name);
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let sem_dir = sem_bin().parent().unwrap().to_path_buf();
        let path =
            env::join_paths([sem_dir, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]).unwrap();

        Self {
            gitconfig: tmp.path().join("gitconfig"),
            _tmp: tmp,
            home,
            path,
        }
    }

    fn apply(&self, command: &mut Command) {
        command
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig)
            .env("PATH", &self.path)
            .env("NO_COLOR", "1")
            .env_remove("GIT_EXTERNAL_DIFF");
    }

    fn wrapper_path(&self) -> PathBuf {
        self.home.join(".local/bin/sem-diff-wrapper")
    }
}

fn sem_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sem"))
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: Output, context: &str) -> Output {
    assert!(
        output.status.success(),
        "{context} failed with status {:?}\n{}",
        output.status.code(),
        output_text(&output)
    );
    output
}

fn assert_failure(output: Output, context: &str) -> Output {
    assert!(
        !output.status.success(),
        "{context} unexpectedly succeeded\n{}",
        output_text(&output)
    );
    output
}

fn run_git(repo: &Path, env: &IsolatedEnv, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    command.args(args).current_dir(repo);
    env.apply(&mut command);
    assert_success(
        command.output().unwrap(),
        &format!("git {}", args.join(" ")),
    )
}

fn run_sem(repo: &Path, env: &IsolatedEnv, args: &[&str]) -> Output {
    let mut command = Command::new(sem_bin());
    command.args(args).current_dir(repo);
    env.apply(&mut command);
    assert_success(
        command.output().unwrap(),
        &format!("sem {}", args.join(" ")),
    )
}

fn init_repo(root: &Path, env: &IsolatedEnv) {
    fs::create_dir_all(root).unwrap();
    run_git(root, env, &["init", "-q"]);
    run_git(root, env, &["config", "user.email", "t@t.com"]);
    run_git(root, env, &["config", "user.name", "test"]);
}

fn commit_python_file(repo: &Path, env: &IsolatedEnv) {
    fs::write(repo.join("a.py"), "def foo():\n    return 1\n").unwrap();
    run_git(repo, env, &["add", "a.py"]);
    run_git(repo, env, &["commit", "-q", "-m", "init"]);
}

fn path_from_git(repo: &Path, env: &IsolatedEnv, args: &[&str]) -> PathBuf {
    let output = run_git(repo, env, args);
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

#[test]
fn setup_writes_absolute_external_and_labels_cached_diff() {
    let tmp = TempDir::new("setup-path");
    let env = IsolatedEnv::new("setup-path-env");
    let repo = tmp.path().join("repo");
    init_repo(&repo, &env);
    commit_python_file(&repo, &env);

    fs::write(repo.join("a.py"), "def foo():\n    return 2\n").unwrap();
    run_git(&repo, &env, &["add", "a.py"]);

    run_sem(&repo, &env, &["setup"]);

    let config = run_git(
        &repo,
        &env,
        &["config", "--global", "--get", "diff.external"],
    );
    let configured = String::from_utf8_lossy(&config.stdout).trim().to_string();
    assert_eq!(configured, env.wrapper_path().display().to_string());
    assert!(Path::new(&configured).is_absolute());
    assert_ne!(configured, "sem-diff-wrapper");

    let wrapper = fs::read_to_string(env.wrapper_path()).unwrap();
    assert!(wrapper.contains("sem diff --label \"$1\" \"$2\" \"$5\""));
    assert!(!wrapper.contains("\"$@\""));

    let diff = run_git(&repo, &env, &["diff", "--cached"]);
    let stdout = String::from_utf8_lossy(&diff.stdout);
    assert!(
        stdout.contains("a.py"),
        "cached diff should use repo path\n{stdout}"
    );
    assert!(
        !stdout.contains("git-blob"),
        "cached diff should not expose temporary blob paths\n{stdout}"
    );
}

#[test]
fn unsetup_keeps_foreign_wrapper_when_external_is_unset() {
    let tmp = TempDir::new("unsetup-foreign");
    let env = IsolatedEnv::new("unsetup-foreign-env");
    fs::create_dir_all(env.wrapper_path().parent().unwrap()).unwrap();
    let foreign = "#!/bin/sh\n# hand edited wrapper\nexec sem diff \"$2\" \"$5\"\n";
    fs::write(env.wrapper_path(), foreign).unwrap();

    run_sem(tmp.path(), &env, &["unsetup"]);

    assert_eq!(fs::read_to_string(env.wrapper_path()).unwrap(), foreign);
}

#[test]
fn unsetup_unsets_external_but_keeps_modified_wrapper() {
    let tmp = TempDir::new("unsetup-modified");
    let env = IsolatedEnv::new("unsetup-modified-env");
    fs::create_dir_all(env.wrapper_path().parent().unwrap()).unwrap();
    let modified = "#!/bin/sh\n# locally modified wrapper\nexec sem diff \"$@\"\n";
    fs::write(env.wrapper_path(), modified).unwrap();
    run_git(
        tmp.path(),
        &env,
        &[
            "config",
            "--global",
            "diff.external",
            env.wrapper_path().to_str().unwrap(),
        ],
    );

    run_sem(tmp.path(), &env, &["unsetup"]);

    assert_eq!(fs::read_to_string(env.wrapper_path()).unwrap(), modified);
    let mut command = Command::new("git");
    command
        .args(["config", "--global", "--get", "diff.external"])
        .current_dir(tmp.path());
    env.apply(&mut command);
    assert_failure(command.output().unwrap(), "git config --get diff.external");
}

#[test]
fn unsetup_removes_owned_wrapper_when_external_points_to_it() {
    let tmp = TempDir::new("unsetup-owned");
    let env = IsolatedEnv::new("unsetup-owned-env");
    let repo = tmp.path().join("repo");
    init_repo(&repo, &env);

    run_sem(&repo, &env, &["setup"]);
    assert!(env.wrapper_path().exists());

    run_sem(&repo, &env, &["unsetup"]);
    assert!(!env.wrapper_path().exists());

    let mut command = Command::new("git");
    command
        .args(["config", "--global", "--get", "diff.external"])
        .current_dir(&repo);
    env.apply(&mut command);
    assert_failure(command.output().unwrap(), "git config --get diff.external");
}

#[test]
fn setup_and_unsetup_use_git_path_for_linked_worktree_hooks() {
    let tmp = TempDir::new("linked-hooks");
    let env = IsolatedEnv::new("linked-hooks-env");
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("repo-linked");
    init_repo(&repo, &env);
    commit_python_file(&repo, &env);
    run_git(
        &repo,
        &env,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );

    run_sem(&linked, &env, &["setup"]);

    let hook_path = path_from_git(
        &linked,
        &env,
        &["rev-parse", "--git-path", "hooks/pre-commit"],
    );
    let wrong_hook_path =
        path_from_git(&linked, &env, &["rev-parse", "--git-dir"]).join("hooks/pre-commit");

    assert!(
        hook_path.exists(),
        "hook should be installed at {:?}",
        hook_path
    );
    assert_ne!(hook_path, wrong_hook_path);
    assert!(
        !wrong_hook_path.exists(),
        "hook should not be installed in linked worktree private git dir"
    );
    assert!(fs::read_to_string(&hook_path)
        .unwrap()
        .contains("# === sem pre-commit hook ==="));

    run_sem(&linked, &env, &["unsetup"]);
    assert!(
        !hook_path.exists(),
        "unsetup from linked worktree should remove the real hook"
    );
}

/// `sem setup` changes global git config first, then installs hooks. If a hook
/// step fails, the machine is half-configured — so setup must say so and exit
/// non-zero instead of printing "sem is wired in".
#[test]
fn setup_reports_incomplete_when_claude_settings_cannot_be_parsed() {
    let env = IsolatedEnv::new("setup-incomplete");
    let repo = env.home.join("repo");
    init_repo(&repo, &env);

    let claude = env.home.join(".claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(claude.join("settings.json"), "{ not json").unwrap();

    let mut command = Command::new(sem_bin());
    command.arg("setup").current_dir(&repo);
    env.apply(&mut command);
    let output = assert_failure(
        command.output().unwrap(),
        "sem setup with an unparseable settings.json",
    );

    let text = output_text(&output);
    assert!(text.contains("setup incomplete"), "{text}");
    assert!(!text.contains("sem is wired in"), "{text}");
    // What did apply is still reported, so nobody has to guess what changed.
    assert!(text.contains("entity-level git diff"), "{text}");
    assert!(
        env.wrapper_path().exists(),
        "the git diff wrapper should still be installed\n{text}"
    );
}
