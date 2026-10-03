//! `sem arch-diff --view` / `--html` end to end on a synthetic repo whose
//! head commit makes three architecture decisions: request input now opens
//! a file by path, a new import closes a cycle between two modules, and a
//! module starts depending on one it never reached before.

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

fn write(r: &Path, p: &str, s: &str) {
    let f = r.join(p);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(f, s).unwrap();
}

fn commit(r: &Path, msg: &str) {
    git(r, &["add", "-A"]);
    git(r, &["-c", "commit.gpgsign=false", "commit", "-qm", msg]);
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    for p in ["app/__init__.py", "app/core/__init__.py", "app/api/__init__.py", "app/metrics/__init__.py", "app/paths/__init__.py"] {
        write(r, p, "");
    }
    write(r, "app/core/model.py", "def make(x):\n    return x\n");
    write(r, "app/api/routes.py", "from flask import request\nfrom app.core.model import make\n\ndef get():\n    return make(request.args.get(\"id\"))\n");
    write(r, "app/metrics/count.py", "def inc(n):\n    return n + 1\n");
    write(r, "app/paths/roots.py", "def root():\n    return \"/srv/data\"\n");
    for args in [&["init", "-q"][..], &["config", "user.email", "t@example.com"], &["config", "user.name", "T"]] {
        git(r, args);
    }
    commit(r, "base");
    write(
        r,
        "app/api/routes.py",
        "from flask import request\nfrom app.core.model import make\n\ndef get():\n    return make(request.args.get(\"id\"))\n\ndef download():\n    name = request.args.get(\"name\")\n    with open(\"/srv/data/\" + name) as f:\n        return f.read()\n",
    );
    write(r, "app/core/model.py", "from app.api.routes import get\n\ndef make(x):\n    return x if x else get()\n");
    write(r, "app/metrics/count.py", "from app.paths.roots import root\n\ndef inc(n):\n    return len(root()) + n\n");
    commit(r, "head");
    dir
}

#[test]
fn the_view_ranks_explains_and_states_what_did_not_change() {
    let dir = fixture();
    let r = dir.path();
    let o = sem(r, &["arch-diff", "HEAD~1..HEAD", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let report: Value = serde_json::from_slice(&o.stdout).unwrap();
    let classes: Vec<(String, String, String)> = report["moduleGraph"]["newEdges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["from"].as_str().unwrap().into(), e["to"].as_str().unwrap().into(), e["class"].as_str().unwrap().into()))
        .collect();
    assert!(classes.contains(&("app/core".into(), "app/api".into(), "closes-cycle".into())), "{classes:?}");
    assert!(classes.contains(&("app/metrics".into(), "app/paths".into(), "couples-independent".into())), "{classes:?}");
    assert!(report["changed"].as_array().is_some_and(|c| !c.is_empty()));

    let o = sem(r, &["arch-diff", "HEAD~1..HEAD", "--view", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let items = v["items"].as_array().unwrap();
    assert!(!items.is_empty() && items.len() <= 10, "{v:#}");
    for it in items {
        assert!(it["why"].as_str().unwrap().starts_with("Why it matters: "), "{it:#}");
    }
    let tags: Vec<&str> = items.iter().map(|i| i["tag"].as_str().unwrap()).collect();
    let pos = |t: &str| tags.iter().position(|x| *x == t).unwrap_or_else(|| panic!("no {t} in {tags:?}"));
    assert!(pos("NEW UNTRUSTED FLOW") < pos("NEW CYCLE") && pos("NEW CYCLE") < pos("NEW COUPLING"), "{tags:?}");
    assert!(items[pos("NEW UNTRUSTED FLOW")]["why"].as_str().unwrap().contains("path traversal"));
    assert!(items[pos("NEW CYCLE")]["what"].as_str().unwrap().contains("closes it"), "{v:#}");

    let text = String::from_utf8_lossy(&sem(r, &["arch-diff", "HEAD~1..HEAD", "--view"]).stdout).into_owned();
    assert!(text.contains("needs a human decision") && text.contains("Unchanged (as far as sem can see):"), "{text}");
    assert!(text.contains("subprocesses") && text.contains("database access"), "{text}");
    assert!(text.lines().count() < 40, "{text}");

    // a saved report renders the same view
    let saved = r.join("report.json");
    fs::write(&saved, serde_json::to_string(&report).unwrap()).unwrap();
    let again = String::from_utf8_lossy(&sem(r, &["arch-diff", "--from-json", saved.to_str().unwrap(), "--view"]).stdout).into_owned();
    let body = |t: &str| t.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert_eq!(body(&again), body(&text));

    let html = String::from_utf8_lossy(&sem(r, &["arch-diff", "--from-json", saved.to_str().unwrap(), "--html"]).stdout).into_owned();
    assert!(html.starts_with("<!doctype html>"), "{}", &html[..80.min(html.len())]);
    assert!(html.contains("id=\"view-data\"") && html.contains("app/metrics"));
    for bad in ["src=\"http", "href=\"http", "@import", "url(http", "url(//"] {
        assert!(!html.contains(bad), "external asset: {bad}");
    }
}

#[test]
fn an_empty_change_has_nothing_to_decide() {
    let dir = fixture();
    let o = sem(dir.path(), &["arch-diff", "HEAD..HEAD", "--view"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("No architectural change that needs a decision."), "{text}");
    assert!(text.contains("network calls") && text.contains("cycles"), "{text}");
}
