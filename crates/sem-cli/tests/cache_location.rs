use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

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

fn git(repo: &Path, args: &[&str]) -> Output {
    assert_success(
        Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap(),
        &format!("git {}", args.join(" ")),
    )
}

/// Did a build land its per-repo artifacts *here* (and not in the working
/// tree)? Either on-disk artifact answers that: `index.sem`, which every
/// build writes, or `cache.db`, which only the content-hydrating verbs write
/// now that `sem graph`'s cold miss is `CacheMissSavePolicy::IndexOnly`
/// (semx-4ex, RESOLUTION-PROFILE.md W4.5). These tests are about *where* the
/// cache directory is, so they must not also pin *which* artifacts a
/// particular verb chooses to write into it.
fn contains_cache_artifact(path: &Path) -> bool {
    fs::read_dir(path).unwrap().any(|entry| {
        let entry = entry.unwrap();
        let path = entry.path();
        path.file_name()
            .is_some_and(|name| name == "cache.db" || name == "index.sem")
            || (path.is_dir() && contains_cache_artifact(&path))
    })
}

fn init_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);

    fs::write(repo.join("a.py"), "def f(a, b):\n    return a + b\n").unwrap();
    git(repo, &["add", "a.py"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

#[test]
fn graph_cache_does_not_dirty_the_working_tree() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();

    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["graph", "--format", "json"])
            .output()
            .unwrap(),
        "sem graph",
    );

    let status = git(repo.path(), &["status", "--porcelain"]);
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
    assert!(!repo.path().join(".sem").exists());
    assert!(contains_cache_artifact(cache.path()));
}

#[test]
fn graph_ignores_repo_local_cache_override() {
    let repo = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", ".sem")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("XDG_CACHE_HOME")
            .env_remove("LOCALAPPDATA")
            .env_remove("APPDATA")
            .args(["graph", "--format", "json"])
            .output()
            .unwrap(),
        "sem graph",
    );

    let status = git(repo.path(), &["status", "--porcelain"]);
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
    assert!(!repo.path().join(".sem").exists());
    assert!(contains_cache_artifact(home.path()));
}

#[test]
fn graph_ignores_repo_local_xdg_cache_home() {
    let repo = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("XDG_CACHE_HOME", ".sem")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("SEM_CACHE_DIR")
            .env_remove("LOCALAPPDATA")
            .env_remove("APPDATA")
            .args(["graph", "--format", "json"])
            .output()
            .unwrap(),
        "sem graph",
    );

    let status = git(repo.path(), &["status", "--porcelain"]);
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
    assert!(!repo.path().join(".sem").exists());
    assert!(contains_cache_artifact(home.path()));
}
