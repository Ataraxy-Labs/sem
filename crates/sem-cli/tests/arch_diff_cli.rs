//! `sem arch-diff` end to end on a synthetic two-commit repo. The head
//! commit plants four structural changes a line-diff reviewer can miss:
//! request input now reaching a SQL string and a shell, a module import
//! cycle, a breaking signature change with a caller left behind, and an
//! annotation-only signature change that must NOT be ranked high. It also
//! checks the "unchanged" statements of an empty change.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn sem(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sem")).current_dir(repo).args(args).output().expect("run sem")
}

fn git(repo: &Path, args: &[&str]) {
    let o = Command::new("git").current_dir(repo).args(args).output().expect("run git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn commit(r: &Path, msg: &str) {
    git(r, &["add", "-A"]);
    git(r, &["-c", "commit.gpgsign=false", "commit", "-qm", msg]);
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    fs::create_dir_all(r.join("app")).unwrap();
    fs::write(r.join("app/__init__.py"), "").unwrap();
    fs::write(
        r.join("app/web.py"),
        "from flask import request\nfrom app.store import save\nfrom app.util import fmt\n\ndef create():\n    name = request.args.get(\"name\")\n    fmt(name, 2)\n    return save(name)\n",
    )
    .unwrap();
    fs::write(
        r.join("app/store.py"),
        "import sqlite3\n\ndef save(name):\n    conn = sqlite3.connect(\"x.db\")\n    conn.execute(\"insert into t values (?)\", (name,))\n    return True\n",
    )
    .unwrap();
    fs::write(r.join("app/util.py"), "def fmt(x: str, n: int) -> str:\n    return str(x)\n").unwrap();
    for args in [&["init", "-q"][..], &["config", "user.email", "t@example.com"], &["config", "user.name", "T"]] {
        git(r, args);
    }
    commit(r, "base");
    fs::write(
        r.join("app/store.py"),
        "import sqlite3\nimport subprocess\nfrom app.util import fmt\n\ndef save(name, backup):\n    conn = sqlite3.connect(\"x.db\")\n    conn.execute(\"insert into t values ('\" + name + \"')\")\n    if backup:\n        subprocess.run(\"cp x.db \" + fmt(name, 1), shell=True)\n    return True\n",
    )
    .unwrap();
    fs::write(
        r.join("app/util.py"),
        "from app import store\n\ndef fmt(x: bytes, n: float) -> str:\n    return str(x) + store.__name__\n",
    )
    .unwrap();
    commit(r, "head");
    dir
}

fn findings(v: &Value) -> Vec<(String, String, String)> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["severity"].as_str().unwrap().to_string(), f["kind"].as_str().unwrap().to_string(), f["title"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn planted_structural_changes_are_reported_and_ranked() {
    let dir = fixture();
    let o = sem(dir.path(), &["arch-diff", "HEAD~1..HEAD", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let f = findings(&v);
    let has = |sev: &str, kind: &str, needle: &str| f.iter().any(|(s, k, t)| s == sev && k == kind && t.contains(needle));
    assert!(has("high", "new-data-path", "http-input app/web.py:6 `create` -> db app/store.py:7 `save`"), "{f:#?}");
    assert!(has("high", "new-data-path", "-> exec app/store.py:9 `save`"), "{f:#?}");
    assert!(has("medium", "new-cycle", "app/store.py, app/util.py"), "{f:#?}");
    assert!(has("high", "signature-change", "`save`"), "{f:#?}");
    // fmt(x: str, n: int) -> fmt(x: bytes, n: float): annotations only
    assert!(has("low", "signature-change", "`fmt`"), "{f:#?}");
    assert!(has("medium", "side-effect-change", "now writes exec"), "{f:#?}");
    // ranked: every high before every medium before every low
    let rank = |s: &str| ["high", "medium", "low", "info"].iter().position(|x| *x == s).unwrap();
    assert!(f.windows(2).all(|w| rank(&w[0].0) <= rank(&w[1].0)));
    assert_eq!(v["dataPaths"]["base"], 0);
    assert_eq!(v["dataPaths"]["added"], 2);

    let md = String::from_utf8_lossy(&sem(dir.path(), &["arch-diff", "HEAD~1..HEAD", "--md"]).stdout).into_owned();
    assert!(md.starts_with("## Architecture delta"), "{md}");
    assert!(md.contains("### High") && md.contains("sink exec via subprocess.run"), "{md}");
    let text = String::from_utf8_lossy(&sem(dir.path(), &["arch-diff", "HEAD~1..HEAD"]).stdout).into_owned();
    assert!(text.lines().count() < 30, "{text}");
}

#[test]
fn an_empty_change_states_what_did_not_change() {
    let dir = fixture();
    let o = sem(dir.path(), &["arch-diff", "HEAD..HEAD", "--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["findings"].as_array().unwrap().len(), 0, "{v:#}");
    let u: Vec<&str> = v["unchanged"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    for needle in ["No source->sink data path", "Side effects", "No package", "Cycles unchanged", "Propagation cost unchanged", "No function signature changed"] {
        assert!(u.iter().any(|x| x.contains(needle)), "missing {needle}: {u:#?}");
    }
}
