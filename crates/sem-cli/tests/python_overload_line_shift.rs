use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("sem-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    dir
}

fn run_sem_json(dir: &PathBuf, home: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .output()
        .expect("sem should run")
}

fn run_git(dir: &PathBuf, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git {} failed\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(dir: &PathBuf) {
    run_git(dir, &["init", "-q"]);
    run_git(dir, &["config", "user.email", "a@b.co"]);
    run_git(dir, &["config", "user.name", "a"]);
}

fn commit_all(dir: &PathBuf, message: &str) {
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-qm", message]);
}

const MOD_PY_INITIAL: &str = r"from typing import overload

def helper():
    return 1

@overload
def polyval(x: int) -> int: ...
@overload
def polyval(x: float) -> float: ...
@overload
def polyval(x: str) -> str: ...
def polyval(x):
    return x
";

const MOD_PY_EDITED: &str = r"from typing import overload

def helper():
    y = 2
    z = 3
    return y + z

@overload
def polyval(x: int) -> int: ...
@overload
def polyval(x: float) -> float: ...
@overload
def polyval(x: str) -> str: ...
def polyval(x):
    return x
";

/// Regression for issue #455: editing an unrelated function above Python @overload
/// stubs must not cascade spurious diff entries for the unchanged overload group.
#[test]
fn python_overload_stubs_unchanged_when_unrelated_function_edited() {
    let dir = temp_dir("python-overload-line-shift");
    let home = temp_dir("python-overload-line-shift-home");
    init_repo(&dir);
    fs::write(dir.join("mod.py"), MOD_PY_INITIAL).expect("write initial mod.py");
    commit_all(&dir, "init");

    fs::write(dir.join("mod.py"), MOD_PY_EDITED).expect("write edited mod.py");

    let output = run_sem_json(&dir, &home, &["diff", "mod.py", "--json"]);
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

    let polyval_changes: Vec<_> = changes
        .iter()
        .filter(|change| change["entityName"].as_str() == Some("polyval"))
        .collect();

    assert_eq!(
        changes.len(),
        1,
        "expected exactly one change (helper modified); got: {changes:?}"
    );
    assert_eq!(
        changes[0]["entityName"].as_str(),
        Some("helper"),
        "expected helper to be the only changed entity; got: {changes:?}"
    );
    assert_eq!(
        changes[0]["changeType"].as_str(),
        Some("modified"),
        "expected helper to be modified; got: {changes:?}"
    );
    assert!(
        polyval_changes.is_empty(),
        "polyval overload stubs must not appear in diff; got: {polyval_changes:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}
