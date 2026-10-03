//! `sem arch-diff` on synthetic two-commit repos, one per gap a reviewer
//! study on large agent PRs found: what the report must say, and what it
//! must not claim.

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

fn write(r: &Path, files: &[(&str, &str)]) {
    for (p, src) in files {
        let path = r.join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, src).unwrap();
    }
}

fn remove(r: &Path, files: &[&str]) {
    for p in files {
        fs::remove_file(r.join(p)).unwrap();
    }
}

fn commit(r: &Path, msg: &str) {
    git(r, &["add", "-A"]);
    git(r, &["-c", "commit.gpgsign=false", "commit", "-qm", msg]);
}

/// A repo with `base` committed, then `head` applied (files written,
/// `gone` removed) and committed.
fn two_commits(base: &[(&str, &str)], head: &[(&str, &str)], gone: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    for args in [&["init", "-q"][..], &["config", "user.email", "t@example.com"], &["config", "user.name", "T"]] {
        git(r, args);
    }
    write(r, base);
    commit(r, "base");
    write(r, head);
    remove(r, gone);
    commit(r, "head");
    dir
}

fn report(dir: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["arch-diff", "HEAD~1..HEAD", "--json"];
    args.extend_from_slice(extra);
    let o = sem(dir, &args);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn md(dir: &Path, extra: &[&str]) -> String {
    let mut args = vec!["arch-diff", "HEAD~1..HEAD", "--md"];
    args.extend_from_slice(extra);
    let o = sem(dir, &args);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn findings(v: &Value) -> Vec<(String, String, String)> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["severity"].as_str().unwrap().to_string(), f["kind"].as_str().unwrap().to_string(), f["title"].as_str().unwrap().to_string()))
        .collect()
}

const WEB_BASE: &str = "import subprocess\nfrom flask import request\n\ndef run():\n    return 'ok'\n";
const WEB_HEAD: &str = "import subprocess\nfrom flask import request\n\ndef run():\n    subprocess.run(request.args['cmd'], shell=True)\n    return 'ok'\n";

#[test]
fn a_memory_budget_degrades_to_a_partial_report() {
    let dir = two_commits(&[("app/web.py", WEB_BASE)], &[("app/web.py", WEB_HEAD)], &[]);
    // 1 MB: the limit trips at once; the report is partial and says so
    let v = report(dir.path(), &["--max-memory", "1", "--scope", "full"]);
    assert_eq!(v["incomplete"], true, "{v:#}");
    assert_eq!(v["budgetExhausted"]["why"], "memory budget", "{v:#}");
    let u: Vec<&str> = v["unchanged"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(!u.iter().any(|x| x.contains("No source->sink data path")), "a partial analysis proves no absence: {u:#?}");
    let m = md(dir.path(), &["--max-memory", "1", "--scope", "full"]);
    assert!(m.contains("Budget exhausted (memory budget"), "{m}");
    // without the limit the same change is complete, with its data path
    let v = report(dir.path(), &[]);
    assert_eq!(v["incomplete"], false);
    assert!(findings(&v).iter().any(|(s, k, _)| s == "high" && k == "new-data-path"), "{v:#}");
}

#[test]
fn a_calls_own_result_reaching_its_argument_is_not_a_data_path() {
    let base = "package main\n\nfunc handle(q string) string {\n\treturn q\n}\n";
    let head = "package main\n\nimport (\n\t\"os\"\n\t\"strings\"\n)\n\nfunc resolve(q string) (string, error) {\n\tq = strings.TrimSpace(q)\n\tc, err := os.ReadFile(q)\n\tif err != nil {\n\t\treturn \"\", err\n\t}\n\treturn string(c), nil\n}\n\nfunc handle(q string) string {\n\tq, _ = resolve(q)\n\treturn q\n}\n";
    let dir = two_commits(&[("go.mod", "module example.com/app\n\ngo 1.22\n"), ("main.go", base)], &[("main.go", head)], &[]);
    let v = report(dir.path(), &[]);
    let f = findings(&v);
    // flow-insensitively the read's result comes back through `handle` into
    // its own path argument: labeled, not ranked as a new data path
    assert!(!f.iter().any(|(_, k, t)| k == "new-data-path" && t.contains("file-read")), "{f:#?}");
    assert!(f.iter().any(|(s, k, t)| s == "info" && k == "self-data-path" && t.contains("resolve")), "{f:#?}");
}

const WS: &str = "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"2\"\n";
const ALPHA_TOML: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nbeta = { path = \"../beta\" }\n";
const BETA_TOML: &str = "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dev-dependencies]\nalpha = { path = \"../alpha\" }\n";

#[test]
fn cycles_are_per_crate_and_skip_examples() {
    let base: &[(&str, &str)] = &[
        ("Cargo.toml", WS),
        ("alpha/Cargo.toml", ALPHA_TOML),
        ("alpha/src/lib.rs", "pub mod onn;\npub mod tts;\n\npub fn run() -> u32 {\n    beta::helper()\n}\n"),
        ("alpha/src/onn/mod.rs", "pub fn infer() -> u32 {\n    1\n}\n"),
        ("alpha/src/tts/mod.rs", "use crate::onn::infer;\n\npub fn speak() -> u32 {\n    infer() + beta::helper()\n}\n"),
        ("beta/Cargo.toml", BETA_TOML),
        ("beta/src/lib.rs", "pub fn helper() -> u32 {\n    2\n}\n"),
        ("beta/examples/demo.rs", "fn main() {\n    println!(\"{}\", alpha::run());\n}\n"),
    ];
    let head: &[(&str, &str)] = &[
        // a module cycle inside one crate: real, must be reported
        ("alpha/src/onn/mod.rs", "use crate::tts::speak;\n\npub fn infer() -> u32 {\n    1\n}\n\npub fn both() -> u32 {\n    speak() + infer()\n}\n"),
        // crate beta naming crate alpha (a dev-dependency): Cargo forbids a
        // real cycle between crates, so this is not one
        ("beta/src/lib.rs", "pub fn helper() -> u32 {\n    2\n}\n\n#[cfg(test)]\npub fn check() -> u32 {\n    alpha::tts::speak()\n}\n"),
        ("beta/examples/demo.rs", "fn main() {\n    println!(\"{} {}\", alpha::run(), alpha::tts::speak());\n}\n"),
    ];
    let dir = two_commits(base, head, &[]);
    let v = report(dir.path(), &[]);
    let f = findings(&v);
    let cycles: Vec<&(String, String, String)> = f.iter().filter(|(_, k, _)| k == "new-cycle").collect();
    assert!(cycles.iter().any(|(s, _, t)| s == "high" && t.contains("package cycle") && t.contains("alpha/src/onn") && t.contains("alpha/src/tts")), "{f:#?}");
    assert!(!cycles.iter().any(|(_, _, t)| t.contains("beta/")), "a cycle across crates is not a cycle: {f:#?}");
    let edges = v["dependencies"]["fileEdgesAddedList"].to_string();
    assert!(!edges.contains("examples/"), "examples are not architecture: {edges}");
    // with --include-examples the example's dependency is counted
    let v = report(dir.path(), &["--include-examples"]);
    assert!(v["dependencies"]["fileEdgesAddedList"].to_string().contains("examples/"), "{v:#}");
}

#[test]
fn regrouping_a_go_var_is_not_a_contract_change() {
    let base = "package watch\n\nimport \"log\"\n\nvar (\n\t// the component logger\n\tlogger = log.New(nil, \"watch\", 0)\n)\n\nfunc Start() {\n\tlogger.Println(\"start\")\n}\n\nfunc Stop() {\n\tlogger.Println(\"stop\")\n}\n";
    let head = "package watch\n\nimport \"log\"\n\nvar logger = log.New(nil, \"watch\", 0)\n\nfunc Start() {\n\tlogger.Println(\"started\")\n}\n\nfunc Stop() {\n\tlogger.Println(\"stop\")\n}\n";
    let dir = two_commits(&[("go.mod", "module example.com/w\n\ngo 1.22\n"), ("pkg/watch/watch.go", base)], &[("pkg/watch/watch.go", head)], &[]);
    let f = findings(&report(dir.path(), &[]));
    assert!(!f.iter().any(|(_, k, t)| k == "signature-change" && t.contains("`logger`")), "{f:#?}");
}

#[test]
fn a_python_import_of_a_missing_repo_module_is_reported() {
    let base: &[(&str, &str)] = &[
        ("app/__init__.py", ""),
        ("app/main.py", "from .routes import register\n\ndef create():\n    return register()\n"),
        ("app/routes.py", "def register():\n    return []\n"),
    ];
    let head: &[(&str, &str)] = &[(
        "app/middleware.py",
        "from .constants import CORRELATION_HEADER\nfrom app.settings import TIMEOUT\nfrom .routes import register\n\ntry:\n    from .speedups import fast\nexcept ImportError:\n    fast = None\n\ndef wrap(h):\n    return (CORRELATION_HEADER, TIMEOUT, register, h)\n",
    )];
    let dir = two_commits(base, head, &[]);
    let f = findings(&report(dir.path(), &[]));
    let broken: Vec<&str> = f.iter().filter(|(s, k, _)| s == "high" && k == "broken-import").map(|(_, _, t)| t.as_str()).collect();
    assert!(broken.iter().any(|t| t.contains("app/middleware.py imports `.constants`")), "{f:#?}");
    assert!(broken.iter().any(|t| t.contains("`app.settings`")), "{f:#?}");
    // an ImportError-guarded import is optional by construction
    assert!(!f.iter().any(|(_, _, t)| t.contains("speedups")), "{f:#?}");
    assert_eq!(broken.len(), 2, "{f:#?}");
}

const LOGGER_BASE: &str = "class Logger:\n    def __init__(self, path='app.log'):\n        self.path = path\n\n    def log(self, message, level='INFO'):\n        print(level, message)\n";
const LOGGER_HEAD: &str = "class Logger:\n    def __init__(self, path='app.log'):\n        self.path = path\n\n    def log(self, component, message, level='INFO'):\n        print(level, component, message)\n";

#[test]
fn callers_through_attributes_resolve_or_count_as_unknown() {
    let base: &[(&str, &str)] = &[
        ("utils/__init__.py", ""),
        ("utils/logger.py", LOGGER_BASE),
        ("svc/__init__.py", ""),
        // typed by its constructor in __init__: resolved
        ("svc/scan.py", "from utils.logger import Logger\n\nclass Scanner:\n    def __init__(self):\n        self.logger = Logger()\n\n    def run(self):\n        self.logger.log('scan started')\n"),
        // a module-level instance: resolved
        ("svc/jobs.py", "from utils.logger import Logger\n\nlogger = Logger()\n\ndef nightly():\n    logger.log('nightly')\n"),
        // injected or defaulted: the attribute's type is not known
        ("svc/steal.py", "from utils.logger import Logger\n\nclass Stealer:\n    def __init__(self, logger=None):\n        self.logger = logger if logger else Logger()\n\n    def run(self):\n        self.logger.log('stealing')\n"),
    ];
    let dir = two_commits(base, &[("utils/logger.py", LOGGER_HEAD)], &[]);
    let v = report(dir.path(), &[]);
    let sig = v["findings"].as_array().unwrap().iter().find(|f| f["kind"] == "signature-change" && f["title"].as_str().unwrap().contains("`log`")).unwrap_or_else(|| panic!("{v:#}"));
    let title = sig["title"].as_str().unwrap();
    let details = sig["details"].to_string();
    assert!(details.contains("svc/scan.py") && details.contains("svc/jobs.py"), "resolved through the attribute / module value: {title} {details}");
    // never "0 not modified" while a possible caller is unresolved
    assert!(title.contains("unresolved"), "{title}");
    assert!(details.contains("svc/steal.py"), "the unresolved call is listed: {details}");
    assert_eq!(sig["severity"], "high", "{sig:#}");
}

#[test]
fn every_markdown_finding_says_why_it_matters() {
    let base: &[(&str, &str)] = &[
        ("app/__init__.py", ""),
        ("app/web.py", WEB_BASE),
        ("app/store.py", "def save(x):\n    return x\n"),
        ("app/util.py", "def fmt(x):\n    return str(x)\n"),
    ];
    let head: &[(&str, &str)] = &[
        ("app/web.py", WEB_HEAD),
        ("app/store.py", "from app.util import fmt\n\ndef save(x):\n    return fmt(x)\n"),
        ("app/util.py", "from app import store\n\ndef fmt(x):\n    return str(x) + store.__name__\n"),
    ];
    let dir = two_commits(base, head, &[]);
    let m = md(dir.path(), &[]);
    // every finding line under High/Medium/Low is followed by its reason
    let lines: Vec<&str> = m.lines().collect();
    let mut findings = 0;
    let mut section = "";
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with("### ") {
            section = l;
        }
        if ["### High", "### Medium", "### Low"].contains(&section) && l.starts_with("- ") && !l.starts_with("- … +") {
            findings += 1;
            assert!(lines.get(i + 1).is_some_and(|n| n.starts_with("  - why it matters: ")), "no reason after {l:?}\n{m}");
        }
    }
    assert!(findings >= 2, "{m}");
    assert!(m.contains("why it matters: untrusted request input now flows into a command that is executed"), "{m}");
    assert!(m.contains("why it matters: changes now ripple both ways"), "{m}");
}

/// A repo where `save` is called from another package, and `cfg.py` is
/// unrelated to the change.
fn scoped_fixture() -> tempfile::TempDir {
    two_commits(
        &[
            ("store/__init__.py", ""),
            ("store/db.py", "def save(name):\n    return name\n"),
            ("app/__init__.py", ""),
            ("app/main.py", "from store.db import save\n\ndef run():\n    return save('x')\n"),
            ("cfg/__init__.py", ""),
            ("cfg/cfg.py", "def load():\n    return 1\n"),
        ],
        &[("store/db.py", "def save(name, backup):\n    return name\n")],
        &[],
    )
}

#[test]
fn a_diff_scoped_report_finds_the_same_callers_as_a_whole_tree_one() {
    let d = scoped_fixture();
    let (full, scoped) = (report(d.path(), &["--scope", "full"]), report(d.path(), &["--scope", "diff"]));
    assert_eq!(full["scope"]["mode"], "full");
    assert_eq!(scoped["scope"]["mode"], "diff");
    // the unrelated package is not read; the caller is
    assert!(scoped["scope"]["filesAnalyzed"].as_u64().unwrap() < scoped["scope"]["filesInRepo"].as_u64().unwrap());
    let sig = |v: &Value| -> Value {
        v["findings"].as_array().unwrap().iter().find(|f| f["kind"] == "signature-change").cloned().unwrap_or_else(|| panic!("{v:#}"))
    };
    let (a, b) = (sig(&full), sig(&scoped));
    assert_eq!(a["severity"], "high");
    assert_eq!(a["title"], b["title"]);
    assert_eq!(a["data"]["callers"], b["data"]["callers"]);
    assert!(md(d.path(), &["--scope", "diff"]).contains("Diff-scoped analysis"));
}

#[test]
fn callers_a_diff_scoped_report_could_not_search_are_unknown_not_absent() {
    let d = scoped_fixture();
    // a region budget that holds only the changed file
    let v = report(d.path(), &["--scope", "diff", "--region-mb", "0"]);
    let sig = v["findings"].as_array().unwrap().iter().find(|f| f["kind"] == "signature-change").unwrap_or_else(|| panic!("{v:#}"));
    assert_eq!(sig["data"]["callersNotSearched"], 1, "{sig:#}");
    assert!(sig["title"].as_str().unwrap().contains("callers not searched: 1 file(s) outside the analyzed region mention `save`"), "{sig:#}");
    assert!(v["scope"]["unexplored"].as_array().unwrap().iter().any(|u| u["name"] == "save" && u["role"] == "callers"), "{:#}", v["scope"]);
    // no claim that nothing changed outside what was read
    assert!(v["unchanged"].as_array().unwrap().iter().all(|u| !u.as_str().unwrap().starts_with("Cycles unchanged:")), "{:#}", v["unchanged"]);
}
