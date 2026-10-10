//! `sem check`: an exact incremental verifier.
//!
//! Each checker (TypeScript, lint, JS tests, Python, pytest, Go, Cargo,
//! Gradle or Maven, dotnet, SwiftPM, C and C++, any command) returns the
//! verdict the real tool would return on the whole project. A checker works
//! incrementally — rechecking only what a change can affect, with every other
//! result carried over from a state an earlier run recorded — only when that
//! is provably the same verdict; otherwise it runs the full tool and says why.
//! It never reports a sliced pass that the full tool might not.
//!
//! Two layers make that work for every language. The shared one (`carry`)
//! skips a checker outright when no changed path is one of its inputs since
//! its base passed. The per-language one scopes the real tool to what a change
//! can reach: TypeScript's own program graph, the JS module graph for tests,
//! Go packages, Python imports for pyright and pytest, and C/C++ includes over
//! the compile database.
//!
//! States live in a content-addressed store under sem's cache root, keyed by
//! the git tree id they were computed at (plus the checker's configuration),
//! so every checkout and clone of a project shares them: the tree a landing
//! publishes was checked, and its state is the base of the next landing. The
//! store is bounded (bytes and entries) and evicts least recently used.
//!
//! Exit code: 0 pass, 1 fail, 2 could not decide. `--json` prints one object
//! carrying a verification certificate: the input trees, tool versions, the
//! mode of every checker and digests of the states it read and wrote.

mod carry;
mod cpp;
mod deps;
mod generic;
mod lint;
mod python;
mod store;
mod tests;
mod tree;
mod ts;
mod util;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Args;
use serde_json::{json, Value};

use store::Store;
use tree::{Change, Rev};

#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    /// Compare against this revision (default: HEAD). The working tree, with
    /// uncommitted and untracked files, is what is checked.
    #[arg(long)]
    pub base: Option<String>,
    /// Print one JSON object (verdict, per-checker results, certificate)
    #[arg(long)]
    pub json: bool,
    /// Checkers to run, comma-separated: ts, lint, tests, python, pytest, go,
    /// cargo, jvm, dotnet, swift, cpp, cmd
    /// (default: `.sem/check.json`'s "checkers", else every one detected)
    #[arg(long, value_delimiter = ',')]
    pub checkers: Vec<String>,
    /// Run every checker in full mode (states are still recorded)
    #[arg(long)]
    pub full: bool,
    /// TypeScript backend: auto (tsgo when the project uses it, else the
    /// project's own `typescript` through its API), tsc, or tsgo
    #[arg(long, default_value = "auto")]
    pub ts_backend: String,
    /// tsconfig to check (default: `.sem/check.json` ts.project, else tsconfig.json)
    #[arg(long)]
    pub project: Option<String>,
    /// Neither read nor write check states
    #[arg(long)]
    pub no_cache: bool,
    /// Give up on any one tool run after this many seconds (verdict: could not decide)
    #[arg(long, default_value = "1800")]
    pub timeout: u64,
    /// Run as if started in this directory
    #[arg(short = 'C', long = "cwd")]
    pub directory: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pass,
    Fail,
    Undecided,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Undecided => "undecided",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Incremental,
    Full,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Incremental => "incremental",
            Mode::Full => "full",
        }
    }
}

/// One checker's result.
#[derive(Debug, Clone)]
pub(crate) struct Outcome {
    pub name: &'static str,
    pub tool: String,
    pub tool_version: Option<String>,
    pub verdict: Verdict,
    pub mode: Mode,
    pub reasons: Vec<String>,
    pub rechecked: Vec<String>,
    pub diagnostics: Vec<String>,
    pub errors: usize,
    pub state_in: Option<String>,
    pub state_from: Option<String>,
    pub state_out: Option<String>,
    pub extra: Value,
    pub duration: Duration,
}

impl Outcome {
    pub fn new(name: &'static str, tool: &str) -> Outcome {
        Outcome {
            name,
            tool: tool.to_string(),
            tool_version: None,
            verdict: Verdict::Undecided,
            mode: Mode::Full,
            reasons: Vec::new(),
            rechecked: Vec::new(),
            diagnostics: Vec::new(),
            errors: 0,
            state_in: None,
            state_from: None,
            state_out: None,
            extra: json!({}),
            duration: Duration::ZERO,
        }
    }

    pub fn undecided(name: &'static str, tool: &str, why: impl Into<String>) -> Outcome {
        let mut o = Outcome::new(name, tool);
        o.reasons.push(why.into());
        o
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "tool": self.tool,
            "toolVersion": self.tool_version,
            "verdict": self.verdict.as_str(),
            "mode": self.mode.as_str(),
            "reasons": self.reasons,
            "filesRechecked": self.rechecked,
            "filesRecheckedCount": self.rechecked.len(),
            "errors": self.errors,
            "diagnostics": self.diagnostics,
            "state": { "in": self.state_in, "from": self.state_from, "out": self.state_out },
            "details": self.extra,
            "durationMs": self.duration.as_millis() as u64,
        })
    }
}

/// What every checker sees.
pub(crate) struct Ctx {
    pub root: PathBuf,
    /// The base revision, when there is a git repository and the rev resolves.
    pub base: Option<Rev>,
    /// The tree being checked: the working tree, as a git tree id.
    pub head: Option<tree::Head>,
    /// Paths that differ between the base tree and the checked tree.
    pub changed: Option<Vec<Change>>,
    pub store: Option<Store>,
    pub config: Value,
    pub full: bool,
    pub timeout: Duration,
    pub args: CheckArgs,
    /// Identity shared by every clone of the repository (its root commit).
    pub project_id: String,
}

impl Ctx {
    /// The store key of a state computed at `tree` for a checker whose
    /// configuration fingerprint is `fp`.
    pub fn key(tree: &str, fp: &str) -> String {
        format!("{tree}-{fp}")
    }

    /// Candidate states, best first: the one recorded at the base tree, the
    /// one at the checked tree itself, and — for checkers whose states are
    /// self-validating — the most recent one of this project.
    pub fn state_candidates(&self, checker: &str, fp: &str, self_validating: bool) -> Vec<(String, PathBuf)> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(b) = &self.base {
            if let Some(p) = store.get(checker, &Ctx::key(&b.tree, fp)) {
                out.push(("base".to_string(), p));
            }
        }
        if self_validating {
            if let Some(h) = &self.head {
                if let Some(p) = store.get(checker, &Ctx::key(&h.tree, fp)) {
                    out.push(("head".to_string(), p));
                }
            }
            if let Some(p) = store.latest(checker, &format!("{}-{fp}", self.project_id)) {
                if !out.iter().any(|(_, q)| q == &p) {
                    out.push(("recent".to_string(), p));
                }
            }
        }
        out
    }

    /// Record `file` as the state of the checked tree.
    pub fn save_state(&self, checker: &str, fp: &str, file: &Path) -> Option<()> {
        let store = self.store.as_ref()?;
        let head = self.head.as_ref()?;
        let key = Ctx::key(&head.tree, fp);
        store.put(checker, &key, file).ok()?;
        store.set_latest(checker, &format!("{}-{fp}", self.project_id), &key);
        Some(())
    }

    /// A scratch directory for one run, removed by the caller's guard.
    pub fn scratch(&self) -> std::io::Result<util::Scratch> {
        util::Scratch::new()
    }

    pub fn cfg(&self, section: &str) -> Value {
        self.config.get(section).cloned().unwrap_or(Value::Null)
    }
}

fn load_config(root: &Path) -> Result<Value, String> {
    let p = root.join(".sem/check.json");
    match std::fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", p.display())),
        Err(_) => Ok(json!({})),
    }
}

/// Checkers detected in `root`, in run order.
fn detect(root: &Path, config: &Value) -> Vec<String> {
    let mut v = Vec::new();
    if root.join("tsconfig.json").exists() || config.pointer("/ts/project").is_some() {
        v.push("ts".to_string());
    }
    if lint::detect(root) {
        v.push("lint".to_string());
    }
    if tests::detect(root, config).is_some() {
        v.push("tests".to_string());
    }
    if python::detect(root) || config.get("python").is_some() {
        v.push("python".to_string());
    }
    if python::detect_pytest(root) || config.get("pytest").is_some() {
        v.push("pytest".to_string());
    }
    if root.join("go.mod").exists() {
        v.push("go".to_string());
    }
    if root.join("Cargo.toml").exists() {
        v.push("cargo".to_string());
    }
    if generic::detect_jvm(root) {
        v.push("jvm".to_string());
    }
    if generic::detect_dotnet(root) {
        v.push("dotnet".to_string());
    }
    if generic::detect_swift(root) {
        v.push("swift".to_string());
    }
    if cpp::detect(root, config) {
        v.push("cpp".to_string());
    }
    if config.get("commands").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        v.push("cmd".to_string());
    }
    v
}

pub fn run(args: CheckArgs) -> i32 {
    let t0 = Instant::now();
    let start = args
        .directory
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let (doc, code) = match evaluate(&start, args.clone()) {
        Ok((mut doc, code)) => {
            doc["durationMs"] = json!(t0.elapsed().as_millis() as u64);
            (doc, code)
        }
        Err(e) => (
            json!({ "verdict": "undecided", "exitCode": 2, "error": e, "checkers": [] }),
            2,
        ),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        print_human(&doc);
    }
    code
}

fn evaluate(start: &Path, args: CheckArgs) -> Result<(Value, i32), String> {
    let git_root = tree::toplevel(start);
    let root = git_root.clone().unwrap_or_else(|| start.to_path_buf());
    let config = load_config(&root)?;
    let mut base = None;
    let mut head = None;
    let mut changed = None;
    let mut notes: Vec<String> = Vec::new();
    let mut project_id = String::from("nogit");
    if git_root.is_some() {
        let spec = args.base.clone().unwrap_or_else(|| "HEAD".to_string());
        match tree::rev(&root, &spec) {
            Some(r) => base = Some(r),
            None if args.base.is_some() => {
                return Err(format!("--base {spec}: not a commit in this repository"));
            }
            None => notes.push("no commits yet: nothing to compare against".into()),
        }
        match tree::head(&root) {
            Ok(h) => head = Some(h),
            Err(e) => notes.push(format!("the working tree has no tree id ({e}); states are neither read by tree nor saved")),
        }
        if let (Some(b), Some(h)) = (&base, &head) {
            changed = tree::diff(&root, &b.tree, &h.tree).ok();
        }
        project_id = tree::root_commit(&root).unwrap_or_else(|| "norootcommit".into());
    } else {
        notes.push("not a git repository: every checker runs in full mode".into());
    }
    let store = if args.no_cache { None } else { Store::open(&root) };

    let mut wanted: Vec<String> = if !args.checkers.is_empty() {
        args.checkers.clone()
    } else if let Some(a) = config.get("checkers").and_then(Value::as_array) {
        a.iter().filter_map(|x| x.as_str().map(String::from)).collect()
    } else {
        detect(&root, &config)
    };
    wanted.retain(|c| !c.trim().is_empty());
    let ctx = Ctx {
        root: root.clone(),
        base,
        head,
        changed,
        store,
        config,
        full: args.full,
        timeout: Duration::from_secs(args.timeout.max(1)),
        args,
        project_id,
    };

    let mut outcomes = Vec::new();
    for c in &wanted {
        let t = Instant::now();
        let mut o = match c.trim() {
            "ts" | "typescript" => ts::run(&ctx),
            "lint" | "eslint" | "biome" => lint::run(&ctx),
            "tests" | "test" => tests::run(&ctx),
            "python" | "pyright" | "mypy" => python::run(&ctx),
            "pytest" => python::pytest(&ctx),
            "go" => generic::go(&ctx),
            "cargo" | "rust" => generic::cargo(&ctx),
            "jvm" | "java" | "kotlin" | "gradle" | "maven" => generic::jvm(&ctx),
            "dotnet" | "csharp" => generic::dotnet(&ctx),
            "swift" => generic::swift(&ctx),
            "cpp" | "c" | "c++" => cpp::run(&ctx),
            "cmd" | "commands" => generic::commands(&ctx),
            other => Outcome::undecided(
                "unknown",
                other,
                format!("unknown checker `{other}` (known: ts, lint, tests, python, pytest, go, cargo, jvm, dotnet, swift, cpp, cmd)"),
            ),
        };
        o.duration = t.elapsed();
        outcomes.push(o);
    }

    let verdict = if outcomes.iter().any(|o| o.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if outcomes.is_empty() {
        notes.push("no checker applies to this project (none detected and none requested)".into());
        Verdict::Undecided
    } else if outcomes.iter().any(|o| o.verdict == Verdict::Undecided) {
        Verdict::Undecided
    } else {
        Verdict::Pass
    };
    let code = match verdict {
        Verdict::Pass => 0,
        Verdict::Fail => 1,
        Verdict::Undecided => 2,
    };
    let cert = certificate(&ctx, &outcomes, verdict);
    let doc = json!({
        "verdict": verdict.as_str(),
        "exitCode": code,
        "root": ctx.root.to_string_lossy(),
        "base": ctx.base.as_ref().map(|b| json!({"rev": b.spec, "commit": b.commit, "tree": b.tree})),
        "head": ctx.head.as_ref().map(|h| json!({"commit": h.commit, "tree": h.tree, "dirty": h.dirty})),
        "changedPaths": ctx.changed.as_ref().map(|c| c.iter().map(|c| format!("{} {}", c.status, c.path)).collect::<Vec<_>>()),
        "notes": notes,
        "checkers": outcomes.iter().map(Outcome::to_json).collect::<Vec<_>>(),
        "certificate": cert,
    });
    Ok((doc, code))
}

/// The verification certificate: what was checked (input trees), with what
/// (tool versions), how (mode and reasons per checker), the states read and
/// written, and a digest of the verdict's diagnostics. `digest` is the git
/// blob id of the certificate's canonical JSON without it.
fn certificate(ctx: &Ctx, outcomes: &[Outcome], verdict: Verdict) -> Value {
    let checkers: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            json!({
                "name": o.name,
                "tool": o.tool,
                "toolVersion": o.tool_version,
                "mode": o.mode.as_str(),
                "reasons": o.reasons,
                "verdict": o.verdict.as_str(),
                "stateIn": o.state_in,
                "stateFrom": o.state_from,
                "stateOut": o.state_out,
                "filesRechecked": o.rechecked.len(),
                "diagnosticsDigest": util::digest(&o.diagnostics.join("\n")),
            })
        })
        .collect();
    let mut cert = json!({
        "schema": "sem-check-certificate/1",
        "sem": env!("CARGO_PKG_VERSION"),
        "inputs": {
            "baseCommit": ctx.base.as_ref().map(|b| b.commit.clone()),
            "baseTree": ctx.base.as_ref().map(|b| b.tree.clone()),
            "headCommit": ctx.head.as_ref().and_then(|h| h.commit.clone()),
            "tree": ctx.head.as_ref().map(|h| h.tree.clone()),
            "dirty": ctx.head.as_ref().map(|h| h.dirty),
            "changedPaths": ctx.changed.as_ref().map(|c| c.len()),
        },
        "checkers": checkers,
        "verdict": verdict.as_str(),
    });
    let canonical = serde_json::to_string(&cert).unwrap_or_default();
    cert["digest"] = json!(util::digest(&canonical));
    cert
}

fn print_human(doc: &Value) {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    if let Some(e) = doc.get("error").and_then(Value::as_str) {
        println!("sem check: could not decide: {e}");
        return;
    }
    for n in doc["notes"].as_array().into_iter().flatten() {
        println!("note: {}", s(n));
    }
    for c in doc["checkers"].as_array().into_iter().flatten() {
        println!(
            "{:<6} {:<9} {:<11} {} ({} rechecked, {} ms)",
            s(&c["name"]),
            s(&c["verdict"]).to_uppercase(),
            s(&c["mode"]),
            s(&c["tool"]),
            c["filesRecheckedCount"],
            c["durationMs"]
        );
        for r in c["reasons"].as_array().into_iter().flatten() {
            println!("       because: {}", s(r));
        }
        let diags = c["diagnostics"].as_array().cloned().unwrap_or_default();
        for d in diags.iter().take(50) {
            for (i, line) in s(d).lines().enumerate() {
                println!("{}{line}", if i == 0 { "  " } else { "    " });
            }
        }
        if diags.len() > 50 {
            println!("  ... {} more (use --json)", diags.len() - 50);
        }
    }
    println!(
        "sem check: {} (exit {})",
        s(&doc["verdict"]).to_uppercase(),
        doc["exitCode"]
    );
}
