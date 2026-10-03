//! `sem arch-diff --budget N` bounds the command's wall time.
//!
//! On sympy, arch-diff never finished (over 600 s); with the
//! later 45 s budget it still took 65-80 s, because the budget started after
//! the trees were built, the data-flow engine's final pass and escape
//! resolution ran past it (the deadline was checked every 64 functions, then
//! a quarter of the budget again), and compose re-parsed every Python file of
//! both trees with no deadline at all. A 90 s caller timeout killed it.
//!
//! The fixture is a class hierarchy whose data-flow fixpoint takes minutes
//! (every `self.step()` may run any of 1200 overrides). `SEM_BIN=<path>`
//! runs another binary on the same repo.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::Value;

const MODULES: usize = 60;
const CLASSES: usize = 20;
const BUDGET_SECS: u64 = 10;
/// Process start/exit and temp-dir cleanup are outside the command's clock.
const SLACK_SECS: f64 = 2.5;

/// The wall-time limit: the budget plus slack for a release binary; an
/// unoptimized build spends several times longer serializing the partial
/// result after the deadline, so it gets twice the budget.
fn limit_secs() -> f64 {
    let release = std::env::var("SEM_BIN").is_ok() || !cfg!(debug_assertions);
    BUDGET_SECS as f64 * if release { 1.0 } else { 2.0 } + SLACK_SECS
}

fn git(repo: &Path, args: &[&str]) {
    let o = Command::new("git")
        .current_dir(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("run git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn slow_repo(r: &Path) {
    let app = r.join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(app.join("__init__.py"), "").unwrap();
    fs::write(
        app.join("base.py"),
        "import os\nimport subprocess\n\n\nclass Node(object):\n    def step(self, x):\n        return x\n\n    def run(self, x):\n        y = self.step(x)\n        return self.emit(y)\n\n    def emit(self, y):\n        subprocess.run(y, shell=True)\n        return y\n",
    )
    .unwrap();
    for m in 0..MODULES {
        let nxt = (m + 1) % MODULES;
        let mut s = format!("import os\nfrom app.base import Node\nfrom app import mod_{nxt}\n");
        for c in 0..CLASSES {
            let c2 = (c + 1) % CLASSES;
            write!(
                s,
                "\n\nclass N{m}_{c}(Node):\n    def step(self, x):\n        a = os.environ.get('K{c}', x)\n        b = mod_{nxt}.f{c}(a, self)\n        return self.run(b) if b else self.emit(a)\n\n\ndef f{c}(v, node):\n    w = node.step(v)\n    return mod_{nxt}.f{c2}(w, node) if w else v\n"
            )
            .unwrap();
        }
        fs::write(app.join(format!("mod_{m}.py")), s).unwrap();
    }
    git(r, &["init", "-q"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-qm", "base"]);
    let p = app.join("mod_0.py");
    let mut s = fs::read_to_string(&p).unwrap();
    s += "\n\ndef added(v):\n    return os.environ['X'] + v\n";
    fs::write(&p, s).unwrap();
    git(r, &["commit", "-qam", "head"]);
}

#[test]
fn the_budget_bounds_the_whole_command() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path().join("repo");
    fs::create_dir_all(&r).unwrap();
    slow_repo(&r);
    let bin = std::env::var("SEM_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_sem").to_string());
    let t0 = Instant::now();
    let out = Command::new(bin)
        .args(["arch-diff", "HEAD~1..HEAD", "--json", "--budget", &BUDGET_SECS.to_string()])
        .current_dir(&r)
        .env("SEM_CACHE_DIR", tmp.path().join("cache"))
        .stderr(Stdio::null())
        .output()
        .expect("run sem");
    let secs = t0.elapsed().as_secs_f64();
    assert!(out.status.success());
    eprintln!("arch-diff --budget {BUDGET_SECS}: {secs:.1}s");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    // the fixpoint cannot finish in the budget: the report says it is partial
    assert!(v["budgetExhausted"].is_object(), "{}", v["budgetExhausted"]);
    assert_eq!(v["budgetExhausted"]["why"], "time budget");
    assert!(secs <= limit_secs(), "--budget {BUDGET_SECS} took {secs:.1}s (limit {:.1}s)", limit_secs());
}
