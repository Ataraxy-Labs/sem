//! Qualified entity names on `sem find` / `sem callers` / `sem refs`.
//!
//! In agent sessions on real repositories, about one in five `sem find`/`sem callers`
//! calls failed with "no entity named" because agents address entities the
//! way they read them: `Class.method`, `module.func`,
//! `pkg.mod.Class.method`, `name@line`. The fixture is modelled on the real
//! misses: `Permutation._af_new` / `Permutation.__new__` (sympy-12489),
//! `RelatedManager.create` defined in a class nested inside a factory
//! function (django-16256), and `Set.is_subset` where the bare name has two
//! definitions in the same file (sympy-20438), so `--file` cannot pick one.

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
    write(root, "pkg/combinatorics/__init__.py", "");
    write(
        root,
        "pkg/combinatorics/permutations.py",
        r#"class Permutation(object):
    def __new__(cls, *args):
        return _af_new(list(args))

    @staticmethod
    def _af_new(perm):
        p = object.__new__(Permutation)
        p._array_form = perm
        return p


def bad_name_rgxs():
    return []
"#,
    );
    write(
        root,
        "pkg/sets/sets.py",
        r#"class Set(object):
    def is_subset(self, other):
        return other is self


class Interval(Set):
    def is_subset(self, other):
        return False
"#,
    );
    write(
        root,
        "pkg/fields/related_descriptors.py",
        r#"def create_reverse_many_to_one_manager(superclass):
    class RelatedManager(superclass):
        def create(self, **kwargs):
            return kwargs

    return RelatedManager


def create_forward_many_to_many_manager(superclass):
    class ManyRelatedManager(superclass):
        def create(self, **kwargs):
            return kwargs

    return ManyRelatedManager
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

fn resolved_one(repo: &TempDir, verb: &str, query: &str, extra: &[&str]) -> Value {
    let mut args = vec![verb, query];
    args.extend_from_slice(extra);
    args.push("--json");
    let out = run_sem(repo, &args);
    assert!(
        out.status.success(),
        "`sem {verb} {query}` failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Value = serde_json::from_slice(&out.stdout).expect("json");
    let rows = rows.as_array().expect("array of rows").clone();
    assert_eq!(rows.len(), 1, "`{query}` names exactly one entity: {rows:?}");
    rows[0].clone()
}

#[test]
fn class_dot_method_resolves_for_callers_find_and_refs() {
    let repo = fixture_repo();
    let row = resolved_one(&repo, "callers", "Permutation._af_new", &[]);
    assert_eq!(row["entity"]["name"], "_af_new");
    assert_eq!(row["entity"]["file"], "pkg/combinatorics/permutations.py");

    let row = resolved_one(&repo, "callers", "Permutation.__new__", &["--file", "pkg/combinatorics/permutations.py"]);
    assert_eq!(row["entity"]["name"], "__new__");

    let row = resolved_one(&repo, "refs", "Permutation.__new__", &[]);
    assert_eq!(row["entity"]["name"], "__new__");

    let row = resolved_one(&repo, "find", "Permutation._af_new", &[]);
    assert_eq!(row["name"], "_af_new");
}

#[test]
fn module_qualified_names_resolve() {
    let repo = fixture_repo();
    for q in [
        "pkg.combinatorics.permutations.Permutation._af_new",
        "permutations.Permutation._af_new",
        "combinatorics.permutations.Permutation._af_new",
        "pkg/combinatorics/permutations.py::Permutation::_af_new",
    ] {
        let row = resolved_one(&repo, "find", q, &[]);
        assert_eq!(row["name"], "_af_new", "{q}");
    }
    let row = resolved_one(&repo, "find", "permutations.bad_name_rgxs", &[]);
    assert_eq!(row["name"], "bad_name_rgxs");
    // a wrong module qualifier is a miss, not a match on the bare name
    let out = run_sem(&repo, &["find", "sets.Permutation._af_new"]);
    assert!(!out.status.success());
}

#[test]
fn owner_qualifier_disambiguates_same_file_definitions() {
    let repo = fixture_repo();
    // the bare name is ambiguous inside one file: --file cannot pick
    let out = run_sem(&repo, &["callers", "is_subset", "--file", "pkg/sets/sets.py"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Set.is_subset") && err.contains("Interval.is_subset"), "refusal names the qualified retry forms: {err}");

    let row = resolved_one(&repo, "callers", "Set.is_subset", &[]);
    assert_eq!(row["entity"]["start_line"], 2);
    let row = resolved_one(&repo, "callers", "Interval.is_subset", &["--file", "pkg/sets/sets.py"]);
    assert_eq!(row["entity"]["start_line"], 7);
}

#[test]
fn line_selector_picks_one_definition() {
    let repo = fixture_repo();
    let row = resolved_one(&repo, "callers", "is_subset@7", &[]);
    assert_eq!(row["entity"]["start_line"], 7);
    let row = resolved_one(&repo, "callers", "is_subset@L2", &[]);
    assert_eq!(row["entity"]["start_line"], 2);
}

#[test]
fn class_nested_in_function_resolves_by_its_own_name() {
    let repo = fixture_repo();
    let row = resolved_one(
        &repo,
        "callers",
        "RelatedManager.create",
        &["--file", "pkg/fields/related_descriptors.py"],
    );
    assert_eq!(row["entity"]["start_line"], 3);
    let row = resolved_one(&repo, "callers", "ManyRelatedManager.create", &[]);
    assert_eq!(row["entity"]["start_line"], 11);
    let row = resolved_one(
        &repo,
        "find",
        "create_forward_many_to_many_manager.ManyRelatedManager.create",
        &[],
    );
    assert_eq!(row["start_line"], 11);
}

#[test]
fn a_miss_returns_near_matches() {
    let repo = fixture_repo();
    // wrong owner: the member exists under another class
    let out = run_sem(&repo, &["callers", "Interval._af_new"]);
    assert!(!out.status.success());
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("no entity named 'Interval._af_new'"), "{text}");
    assert!(text.contains("Permutation._af_new"), "near match by member name: {text}");

    // typo in the owner
    let out = run_sem(&repo, &["find", "Permutaton._af_new"]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("Permutation._af_new"), "{text}");

    // dashes vs underscores, as in pylint option names
    let out = run_sem(&repo, &["find", "bad-name-rgxs"]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("bad_name_rgxs"), "{text}");

    // json carries the near matches too
    let out = run_sem(&repo, &["callers", "Interval._af_new", "--json"]);
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("json on a miss");
    assert_eq!(v["resolved"], false);
    let near = v["near_matches"].as_array().expect("near_matches");
    assert!(near.iter().any(|m| m["qualified_name"] == "Permutation._af_new"), "{v}");
}

#[test]
fn file_flag_accepts_a_directory() {
    let repo = fixture_repo();
    // django-11149: `--file django/contrib/admin` for a method in options.py
    let row = resolved_one(&repo, "callers", "Set.is_subset", &["--file", "pkg/sets"]);
    assert_eq!(row["entity"]["file"], "pkg/sets/sets.py");
    let row = resolved_one(&repo, "find", "_af_new", &["--file", "pkg/combinatorics/"]);
    assert_eq!(row["file"], "pkg/combinatorics/permutations.py");
    // a sibling directory sharing a prefix is not inside it
    let out = run_sem(&repo, &["find", "_af_new", "--file", "pkg/combinatoric", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v.as_array().map(|a| a.len()), Some(0), "{v}");
}
