//! Ignored container data must not stop source discovery (#535).
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

struct LockedDir(std::path::PathBuf);
impl Drop for LockedDir {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(root)
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .env("XDG_CACHE_HOME", root.join("cache"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn ignored_unreadable_directory_does_not_break_discovery() {
    for with_git in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        if with_git {
            assert!(Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()
                .unwrap()
                .success());
        }
        fs::write(root.join(".gitignore"), "postgres/\ncache/\n").unwrap();
        fs::write(
            root.join("app.py"),
            "def target_fn():\n    return 1\n\ndef caller():\n    return target_fn()\n",
        )
        .unwrap();
        let locked = LockedDir(root.join("postgres"));
        fs::create_dir(&locked.0).unwrap();
        fs::write(locked.0.join("ignored.py"), "def ignored(): pass\n").unwrap();
        fs::set_permissions(&locked.0, fs::Permissions::from_mode(0)).unwrap();
        for args in [
            vec!["graph", "--no-cache", "--json"],
            vec!["find", "target_fn", "--json"],
            vec!["impact", "target_fn", "--no-cache", "--json"],
        ] {
            let out = run(root, &args);
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let body = String::from_utf8_lossy(&out.stdout);
            assert!(body.contains("target_fn"), "{args:?}: {body}");
            assert!(!body.contains("ignored.py"), "{args:?}: {body}");
        }
    }
}

#[test]
fn unignored_permission_error_explains_directory_exclusion() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join(".gitignore"), "postgres/**\n").unwrap();
    let locked = LockedDir(root.join("postgres"));
    fs::create_dir(&locked.0).unwrap();
    fs::set_permissions(&locked.0, fs::Permissions::from_mode(0)).unwrap();
    if fs::read_dir(&locked.0).is_ok() {
        // Root can bypass mode bits; run this test as an unprivileged user.
        return;
    }
    let out = run(root, &["graph", "--no-cache"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(".semignore") && err.contains("postgres/ rather than postgres/**"),
        "{err}"
    );
    fs::write(root.join(".semignore"), "postgres/\n").unwrap();
    assert!(run(root, &["graph", "--no-cache"]).status.success());
}
