//! `sem promises` end to end on a synthetic repo: one kept promise (Rust), one
//! broken (TSX), change scoping, `--only`, `status --json`, the edit hook, and
//! `topology check --laws` sharing the same evaluator.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const PROMISES: &str = r#"{ "laws": [
  { "id": "jsx/conditionals", "promise": "JSX contains no conditional rendering",
    "forbidPattern": { "from": "**/*.tsx", "within": ["jsx_expression"], "query": "(ternary_expression) @conditional" } },
  { "id": "rust/no-unwrap", "promise": "Library code never unwraps",
    "forbidPattern": { "from": "src/**/*.rs", "query": "((call_expression function: (field_expression field: (field_identifier) @_m)) @unwrap (#eq? @_m \"unwrap\"))" } }
] }"#;

fn sem(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sem")).current_dir(repo).args(args).output().expect("run sem")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn git(repo: &Path, args: &[&str]) {
    let o = Command::new("git").current_dir(repo).args(args).output().expect("run git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    fs::create_dir_all(r.join(".sem/promises")).unwrap();
    fs::create_dir_all(r.join("src/lib")).unwrap();
    fs::write(r.join(".sem/promises/app.json"), PROMISES).unwrap();
    fs::write(r.join("src/View.tsx"), "export const V = ({ a }) => <div>{a ? 1 : 2}</div>;\n").unwrap();
    fs::write(r.join("src/Clean.tsx"), "export const C = ({ name }) => <div>{name}</div>;\n").unwrap();
    fs::write(r.join("src/lib/mod.rs"), "pub fn f(x: Option<u8>) -> u8 { x.unwrap_or(0) }\n").unwrap();
    for args in [&["init", "-q"][..], &["config", "user.email", "t@example.com"], &["config", "user.name", "T"], &["add", "-A"], &["-c", "commit.gpgsign=false", "commit", "-qm", "init"]] {
        git(r, args);
    }
    dir
}

#[test]
fn check_reports_kept_and_broken_promises() {
    let dir = fixture();
    let o = sem(dir.path(), &["promises", "check"]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert!(out.contains("BROKEN 1 jsx/conditionals  JSX contains no conditional rendering"), "{out}");
    assert!(out.contains("  src/View.tsx:1:35  conditional  a ? 1 : 2"), "{out}");
    assert!(out.contains("KEPT rust/no-unwrap"), "{out}");

    // scoped to a clean file, or to the kept promise: done
    assert_eq!(sem(dir.path(), &["promises", "check", "--changed", "src/Clean.tsx"]).status.code(), Some(0));
    assert_eq!(sem(dir.path(), &["promises", "check", "--only", "rust/*"]).status.code(), Some(0));
    assert_eq!(sem(dir.path(), &["promises", "check", "--only", "typo"]).status.code(), Some(2));

    // status: counts only, exit 0
    let o = sem(dir.path(), &["promises", "status", "--json"]);
    assert_eq!(o.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["broken"], 1);
    assert_eq!(v["promises"][0]["violations"], 1);
    assert!(v["promises"][0].get("details").is_none());
    assert_eq!(v["promises"][1]["kept"], true);
}

#[test]
fn since_scopes_to_changed_and_untracked_files() {
    let dir = fixture();
    let r = dir.path();
    fs::write(r.join("src/View.tsx"), "export const V = () => <div />;\n").unwrap();
    assert_eq!(sem(r, &["promises", "check", "--since", "HEAD"]).status.code(), Some(0));
    fs::write(r.join("src/lib/new.rs"), "pub fn g(x: Option<u8>) -> u8 { x.unwrap() }\n").unwrap();
    let o = sem(r, &["promises", "check", "--since", "HEAD", "--json"]);
    assert_eq!(o.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["promises"][1]["details"][0]["file"], "src/lib/new.rs");
}

// The hook is a POSIX shell script and PATH is colon-separated.
#[cfg(unix)]
#[test]
fn edit_hook_blocks_only_on_a_broken_promise() {
    let dir = fixture();
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/promises-hook.sh");
    let bin = Path::new(env!("CARGO_BIN_EXE_sem")).parent().unwrap().to_path_buf();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |file: &Path| {
        let mut c = Command::new("sh").arg(&hook).env("PATH", &path).stdin(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let payload = format!(r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#, file.display());
        c.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        c.wait_with_output().unwrap()
    };
    assert_eq!(run(&dir.path().join("src/Clean.tsx")).status.code(), Some(0));
    let o = run(&dir.path().join("src/View.tsx"));
    assert_eq!(o.status.code(), Some(2));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("src/View.tsx:1:35") && !err.contains("KEPT"), "{err}");
}

#[test]
fn topology_check_shares_the_evaluator() {
    let dir = fixture();
    let o = sem(dir.path(), &["topology", "check", "--laws", ".sem/promises/app.json"]);
    assert_eq!(o.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["violations"], 1);
    assert_eq!(v["laws"][0]["details"][0]["file"], "src/View.tsx");
}

#[test]
fn verify_requires_a_mutation_that_breaks_each_promise() {
    let dir = fixture();
    let r = dir.path();
    let laws = r#"{ "laws": [
  { "id": "rust/no-unwrap",
    "forbidPattern": { "from": "src/**/*.rs", "query": "((call_expression function: (field_expression field: (field_identifier) @_m)) @unwrap (#eq? @_m \"unwrap\"))" },
    "mutation": { "file": "src/lib/mod.rs", "replace": ["unwrap_or(0)", "unwrap()"] } },
  { "id": "rust/new-file",
    "forbidPattern": { "from": "src/**/*.rs", "query": "((call_expression function: (field_expression field: (field_identifier) @_m)) @unwrap (#eq? @_m \"unwrap\"))" },
    "mutation": { "file": "src/lib/extra.rs", "create": "fn g(x: Option<u8>) -> u8 { x.unwrap() }\n" } },
  { "id": "rust/vacuous",
    "forbidPattern": { "from": "src/**/*.rsx", "query": "(call_expression) @call" },
    "mutation": { "file": "src/lib/mod.rs", "append": "fn h() { k() }\n" } },
  { "id": "rust/no-mutation",
    "forbidPattern": { "from": "src/**/*.rs", "query": "(macro_invocation) @m" } }
] }"#;
    fs::remove_file(r.join(".sem/promises/app.json")).unwrap();
    fs::write(r.join(".sem/promises/v.json"), laws).unwrap();
    let before = fs::read(r.join("src/lib/mod.rs")).unwrap();
    let o = sem(r, &["promises", "verify"]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.contains("FALSIFIABLE rust/no-unwrap"), "{out}");
    assert!(out.contains("FALSIFIABLE rust/new-file"), "{out}");
    assert!(out.contains("UNVERIFIED rust/vacuous  stays kept under its mutation"), "{out}");
    assert!(out.contains("UNVERIFIED rust/no-mutation  no mutation"), "{out}");
    assert_eq!(fs::read(r.join("src/lib/mod.rs")).unwrap(), before, "mutated file restored");
    assert!(!r.join("src/lib/extra.rs").exists(), "created file removed");
    assert_eq!(sem(r, &["promises", "verify", "--only", "rust/no-unwrap", "rust/new-file"]).status.code(), Some(0));
}
