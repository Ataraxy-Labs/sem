//! `sem callers` never answers a confident "none" it cannot back.
//!
//! SWE-bench sympy-12489: `sem callers _af_new` said "(callers: none)"
//! while fifteen sibling methods called it through the module-level alias
//! `_af_new = Permutation._af_new`. sympy-20438: `is_subset_sets` is a
//! multipledispatch function with fifteen `@dispatch` registrations in one
//! file, reached only through the dispatcher. sympy's `getattr(self,
//! '_eval_rewrite_as_' + name)` reaches methods with no mention at all.
//!
//! Every caller answer carries `complete` (text and json) and, when it is
//! false, why, plus the possible callers the static graph did not resolve.

use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn fixture_repo() -> TempDir {
    let repo = TempDir::new().expect("tempdir");
    let root = repo.path();
    write(root, "pkg/__init__.py", "");
    // sympy-12489: module-level alias of a staticmethod, called bare
    write(
        root,
        "pkg/permutations.py",
        r#"class Permutation(object):
    def __new__(cls, *args):
        return _af_new(list(args))

    @staticmethod
    def _af_new(perm):
        """Build a permutation. Internally `_af_new` is used; _af_new(x)."""
        p = object.__new__(Permutation)
        p._array_form = perm
        return p

    def __mul__(self, other):
        return _af_new([1, 0])

    def rmul(self):
        # _af_new is the fast path
        return _af_new([2])


_af_new = Permutation._af_new
"#,
    );
    write(
        root,
        "pkg/named_groups.py",
        r#"from pkg.permutations import _af_new


def SymmetricGroup(n):
    return _af_new(list(range(n)))
"#,
    );
    // sympy-20438: multipledispatch registrations in one file
    write(
        root,
        "pkg/handlers.py",
        r#"from multipledispatch import dispatch


@dispatch(object, object)
def is_subset_sets(a, b):
    return None


@dispatch(int, object)
def is_subset_sets(a, b):
    return True
"#,
    );
    write(
        root,
        "pkg/sets.py",
        r#"from pkg.handlers import is_subset_sets


class Set(object):
    def is_subset(self, other):
        ret = is_subset_sets(self, other)
        if ret is not None:
            return ret
        return self.rewrite('Interval')

    def rewrite(self, name):
        return getattr(self, '_eval_rewrite_as_' + name)()

    def _eval_rewrite_as_Interval(self):
        return self
"#,
    );
    // a fully resolved function, mentioned in prose elsewhere
    write(
        root,
        "pkg/plain.py",
        r#"def target_fn():
    return 0


def caller_a():
    return target_fn()


def doc_only():
    """target_fn() is documented here but not called."""
    # target_fn in a comment
    return 1
"#,
    );
    repo
}

fn run_sem(repo: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .env("SEM_CACHE_DIR", repo.path().join(".test-cache"))
        .args(args)
        .output()
        .expect("run sem")
}

fn callers_json(repo: &TempDir, args: &[&str]) -> Value {
    let mut all = vec!["callers"];
    all.extend_from_slice(args);
    all.push("--json");
    let out = run_sem(repo, &all);
    assert!(
        out.status.success(),
        "sem {all:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    v.as_array().expect("rows")[0].clone()
}

fn possible_entities(row: &Value) -> Vec<String> {
    row["possible_callers"]
        .as_array()
        .expect("possible_callers")
        .iter()
        .map(|p| p["entity"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn alias_callers_make_the_answer_incomplete_and_are_listed() {
    let repo = fixture_repo();
    let row = callers_json(&repo, &["_af_new"]);
    assert_eq!(row["complete"], false, "{row}");
    let codes: Vec<&str> = row["incomplete_because"].as_array().unwrap().iter().map(|r| r["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"alias"), "{codes:?}");
    let got = possible_entities(&row);
    for want in ["Permutation.__new__", "Permutation.__mul__", "Permutation.rmul", "SymmetricGroup"] {
        assert!(got.iter().any(|g| g == want), "missing possible caller {want}: {got:?}");
    }
    // the docstring and the comment are not callers
    assert!(!got.iter().any(|g| g == "Permutation._af_new"), "{got:?}");
}

#[test]
fn text_mode_never_prints_a_bare_none_when_incomplete() {
    let repo = fixture_repo();
    let out = run_sem(&repo, &["callers", "Permutation._af_new"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("(callers: none)\n"), "{text}");
    assert!(text.contains("INCOMPLETE"), "{text}");
    assert!(text.contains("Permutation.rmul") && text.contains("SymmetricGroup"), "{text}");
}

#[test]
fn dispatch_registrations_answer_as_one_dispatcher() {
    let repo = fixture_repo();
    // two registrations in one file: answered as a group, not refused
    let row = callers_json(&repo, &["is_subset_sets"]);
    assert_eq!(row["complete"], false);
    let codes: Vec<&str> = row["incomplete_because"].as_array().unwrap().iter().map(|r| r["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"dispatch"), "{codes:?}");
    assert_eq!(row["dispatch_registrations"].as_array().unwrap().len(), 2);
    let callers: Vec<String> = row["related"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_string()).collect();
    let possible = possible_entities(&row);
    assert!(
        callers.iter().any(|c| c == "is_subset") || possible.iter().any(|p| p == "Set.is_subset"),
        "the dispatcher's caller is listed: {callers:?} {possible:?}"
    );
    // one registration, picked by line, is still flagged as dispatch-reached
    let row = callers_json(&repo, &["is_subset_sets@10"]);
    assert_eq!(row["complete"], false);
}

#[test]
fn computed_getattr_prefix_makes_the_answer_incomplete() {
    let repo = fixture_repo();
    let row = callers_json(&repo, &["_eval_rewrite_as_Interval"]);
    assert_eq!(row["complete"], false, "{row}");
    let reasons = row["incomplete_because"].to_string();
    assert!(reasons.contains("dynamic_getattr") && reasons.contains("_eval_rewrite_as_"), "{reasons}");
}

#[test]
fn a_fully_resolved_caller_set_is_complete_despite_prose_mentions() {
    let repo = fixture_repo();
    let row = callers_json(&repo, &["target_fn"]);
    assert_eq!(row["related"].as_array().unwrap().len(), 1, "{row}");
    assert_eq!(row["complete"], true, "{row}");
    let out = run_sem(&repo, &["callers", "target_fn"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("complete:"), "{text}");
}

#[test]
fn a_caller_added_to_an_unrelated_file_after_indexing_is_seen() {
    let repo = fixture_repo();
    // first call builds and writes the index
    let row = callers_json(&repo, &["target_fn"]);
    assert_eq!(row["complete"], true, "{row}");
    // an agent edit: a new caller in a file that had no edge to target_fn
    let p = repo.path().join("pkg/sets.py");
    let mut s = fs::read_to_string(&p).unwrap();
    s = format!("from pkg.plain import target_fn\n{s}\n\ndef later():\n    return target_fn()\n");
    fs::write(&p, s).unwrap();
    let row = callers_json(&repo, &["target_fn"]);
    let resolved: Vec<String> = row["related"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_string()).collect();
    let possible = possible_entities(&row);
    assert!(
        resolved.iter().any(|r| r == "later") || possible.iter().any(|p| p == "later"),
        "the new caller is listed: {resolved:?} {possible:?}"
    );
    if !resolved.iter().any(|r| r == "later") {
        assert_eq!(row["complete"], false, "{row}");
    }
    // impact shares the verdict path
    let out = run_sem(&repo, &["impact", "target_fn", "--dependents"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("later"), "{text}");
}

#[test]
fn registrations_of_different_dispatchers_are_not_merged() {
    let repo = fixture_repo();
    write(
        repo.path(),
        "pkg/single.py",
        r#"from functools import singledispatch


@singledispatch
def show(x):
    return str(x)


@show.register(int)
def _(x):
    return "int"


@singledispatch
def size(x):
    return 0


@size.register(list)
def _(x):
    return len(x)
"#,
    );
    // two `_` of two dispatchers: not one callable, so refused as ambiguous
    let out = run_sem(&repo, &["callers", "_", "--file", "pkg/single.py"]);
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stderr).contains("matches 2 definitions"));
    // one of them, by line, is answered and flagged as registered
    let row = callers_json(&repo, &["_@10", "--file", "pkg/single.py"]);
    assert_eq!(row["complete"], false, "{row}");
}
