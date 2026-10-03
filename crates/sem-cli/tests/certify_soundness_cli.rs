//! `sem certify` and `sem impact` state what they could not see.
//!
//! SWE-bench sympy-12489: an agent changed `_af_new(perm)` to
//! `_af_new(cls, perm)`. The certificate said "static callers at head: 2
//! (... 0 NOT modified)" and, for callers outside the change, "none found in
//! the static graph", while fifteen methods called it through the alias
//! `_af_new = Permutation._af_new`. `sem impact _af_new --tests` printed a
//! green "✓ No tests found." An untested change is unverified, and a caller
//! set the resolver could not close is not a proof of none.

use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

const BASE: &str = r#"class Permutation(object):
    def __new__(cls, *args):
        return _af_new(list(args))

    @staticmethod
    def _af_new(perm):
        p = object.__new__(Permutation)
        p._array_form = perm
        return p

    def rmul(self):
        return _af_new([2])


_af_new = Permutation._af_new
"#;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn fixture_repo() -> TempDir {
    let repo = TempDir::new().expect("tempdir");
    let root = repo.path();
    git(root, &["init", "-q"]);
    write(root, "pkg/__init__.py", "");
    write(root, "pkg/permutations.py", BASE);
    write(
        root,
        "pkg/named_groups.py",
        "from pkg.permutations import _af_new\n\n\ndef SymmetricGroup(n):\n    return _af_new(list(range(n)))\n",
    );
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    // the agent's change: a classmethod taking cls
    let head = BASE
        .replace("    @staticmethod\n    def _af_new(perm):\n        p = object.__new__(Permutation)", "    @classmethod\n    def _af_new(cls, perm):\n        p = object.__new__(cls)")
        .replace("        return _af_new(list(args))", "        return cls._af_new(list(args))");
    write(root, "pkg/permutations.py", &head);
    git(root, &["commit", "-qam", "head"]);
    repo
}

fn run_sem(repo: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .env("SEM_CACHE_DIR", repo.path().join(".git/test-cache"))
        .args(args)
        .output()
        .expect("run sem")
}

#[test]
fn certify_signature_change_lists_alias_callers_it_cannot_resolve() {
    let repo = fixture_repo();
    let out = run_sem(&repo, &["certify", "HEAD~1..HEAD", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let cert: Value = serde_json::from_slice(&out.stdout).expect("json");
    let sig = cert["signatureChanges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["entity"] == "_af_new")
        .expect("the signature change of _af_new")
        .clone();
    assert_eq!(sig["callersComplete"], false, "{sig}");
    let possible: Vec<String> = sig["possibleCallersNotModified"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["entity"].as_str().unwrap().to_string())
        .collect();
    assert!(possible.iter().any(|p| p == "Permutation.rmul"), "{possible:?}");
    assert!(possible.iter().any(|p| p == "SymmetricGroup"), "{possible:?}");
    assert!(!possible.iter().any(|p| p == "Permutation.__new__"), "__new__ was updated by the change: {possible:?}");

    let out = run_sem(&repo, &["certify", "HEAD~1..HEAD"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("none found in the static graph"), "{text}");
    assert!(text.contains("INCOMPLETE caller set"), "{text}");
    assert!(text.contains("SymmetricGroup"), "{text}");
}

#[test]
fn certify_says_loudly_that_no_test_reaches_the_change() {
    let repo = fixture_repo();
    let out = run_sem(&repo, &["certify", "HEAD~1..HEAD", "--json"]);
    let cert: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(cert["affectedTests"].as_array().unwrap().len(), 0);
    assert_eq!(cert["noTestReaches"], true);
    let out = run_sem(&repo, &["certify", "HEAD~1..HEAD"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("NO TEST REACHES THIS CHANGE: it is unverified"), "{text}");
}

#[test]
fn impact_tests_never_prints_a_green_no_tests() {
    let repo = fixture_repo();
    let out = run_sem(&repo, &["impact", "Permutation._af_new", "--tests"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("No tests found"), "{text}");
    assert!(text.contains("NO TEST REACHES `_af_new`"), "{text}");
    assert!(text.contains("INCOMPLETE"), "{text}");

    let out = run_sem(&repo, &["impact", "Permutation._af_new", "--tests", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["noTestReaches"], true, "{v}");
    assert_eq!(v["callersComplete"], false, "{v}");
}

#[test]
fn impact_dependents_is_not_a_confident_none() {
    let repo = fixture_repo();
    let out = run_sem(&repo, &["impact", "Permutation._af_new", "--dependents"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("✓ No dependents"), "{text}");
    assert!(text.contains("SymmetricGroup"), "{text}");
}
