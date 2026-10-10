//! Python: type checking (pyright scoped to the files a change can reach, or
//! mypy in full with its own cache) and pytest scoped to the affected tests.
//!
//! pyright's diagnostics for a file depend only on that file, the files its
//! imports reach, the configuration and the installed environment. So the
//! incremental verdict rechecks the reverse import closure of the changed
//! files and carries every other file's errors over from the state recorded
//! at the base tree. It runs in full when there is no such state, pyright's
//! version or the environment changed, or a configuration file changed.
//! Early cutoff: a modified module whose interface is unchanged (every edit
//! is inside the body of a module-level function that declares its return
//! type) is rechecked itself, but reaches no other file.
//!
//! pytest reruns only the test files that can load a changed file at run
//! time (any mention counts, not just imports) and carries every other test
//! file's failures over from the base, so a base that already had failures
//! still checks incrementally. Any change to pytest's configuration, a
//! `conftest.py`, or a path that is neither Python, inert, nor another
//! language's source (a fixture, a data file) runs every test, as does a base
//! failure no test file owns.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::carry;
use super::deps;
use super::tree;
use super::util;
use super::{Ctx, Mode, Outcome, Verdict};

fn is_py(p: &str) -> bool {
    p.ends_with(".py") || p.ends_with(".pyi")
}

fn leaf(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn read(root: &Path, f: &str) -> String {
    std::fs::read_to_string(root.join(f)).unwrap_or_default()
}

/// The body of `[section]` in a TOML file (up to the next table header).
fn toml_section(text: &str, section: &str) -> Option<String> {
    let head = format!("[{section}]");
    let start = text.lines().position(|l| l.trim() == head)?;
    let body: Vec<&str> = text.lines().skip(start + 1).take_while(|l| !l.trim_start().starts_with('[')).collect();
    Some(body.join("\n"))
}

/// The strings of a TOML array `key = [...]` in `body`.
fn toml_strings(body: &str, key: &str) -> Option<Vec<String>> {
    let re = regex::Regex::new(&format!(r#"(?s)(?m)^\s*{}\s*=\s*\[(.*?)\]"#, regex::escape(key))).ok()?;
    let inner = re.captures(body)?.get(1)?.as_str().to_string();
    let s = regex::Regex::new(r#""([^"]*)"|'([^']*)'"#).ok()?;
    Some(s.captures_iter(&inner).filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string())).collect())
}

/// A tool-config section from pyproject.toml.
fn pyproject(root: &Path, section: &str) -> Option<String> {
    toml_section(&read(root, "pyproject.toml"), section)
}

fn which(root: &Path, bin: &str) -> Option<PathBuf> {
    for d in [".venv/bin", "venv/bin"] {
        let p = root.join(d).join(bin);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(p) = util::node_bin(root, bin) {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file()))
}

fn version(bin: &Path, root: &Path) -> Option<String> {
    let o = std::process::Command::new(bin).arg("--version").current_dir(root).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if s.is_empty() {
        Some(String::from_utf8_lossy(&o.stderr).trim().to_string())
    } else {
        Some(s)
    }
}

/// What is installed: the interpreter's version and the distributions in the
/// project's virtualenv (or the active one).
fn environment(root: &Path) -> String {
    let mut parts = Vec::new();
    let py = which(root, "python3").or_else(|| which(root, "python"));
    if let Some(p) = &py {
        parts.push(version(p, root).unwrap_or_default());
    }
    let mut envs: Vec<PathBuf> = [".venv", "venv"].iter().map(|d| root.join(d)).collect();
    if let Some(v) = std::env::var_os("VIRTUAL_ENV") {
        envs.push(PathBuf::from(v));
    }
    for env in envs {
        let Ok(libs) = std::fs::read_dir(env.join("lib")) else { continue };
        for lib in libs.flatten() {
            let Ok(sp) = std::fs::read_dir(lib.path().join("site-packages")) else { continue };
            let mut names: Vec<String> = sp.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            names.sort();
            parts.push(names.join(","));
        }
    }
    util::fingerprint(&parts.iter().map(String::as_str).collect::<Vec<_>>())
}

fn changes(ctx: &Ctx) -> Option<Vec<(char, String)>> {
    ctx.changed.as_ref().map(|c| c.iter().map(|c| (c.status, c.path.clone())).collect())
}

fn head_files(ctx: &Ctx) -> Result<Vec<String>, String> {
    let h = ctx.head.as_ref().ok_or("no tree id for the working tree")?;
    tree::files(&ctx.root, &h.tree)
}

// ── type checking ──────────────────────────────────────────────────────────

pub(crate) fn detect(root: &Path) -> bool {
    root.join("pyrightconfig.json").exists()
        || pyproject(root, "tool.pyright").is_some()
        || pyproject(root, "tool.basedpyright").is_some()
        || root.join("mypy.ini").exists()
        || root.join(".mypy.ini").exists()
        || pyproject(root, "tool.mypy").is_some()
}

fn wants_mypy(root: &Path, cfg: &Value) -> bool {
    match cfg["tool"].as_str() {
        Some(t) => t == "mypy",
        None => {
            !(root.join("pyrightconfig.json").exists() || pyproject(root, "tool.pyright").is_some() || pyproject(root, "tool.basedpyright").is_some())
        }
    }
}

pub(crate) fn run(ctx: &Ctx) -> Outcome {
    let cfg = ctx.cfg("python");
    if wants_mypy(&ctx.root, &cfg) {
        mypy(ctx, &cfg)
    } else {
        pyright(ctx, &cfg)
    }
}

fn mypy(ctx: &Ctx, cfg: &Value) -> Outcome {
    let mut o = Outcome::new("python", "mypy");
    let Some(bin) = which(&ctx.root, "mypy") else {
        return Outcome::undecided("python", "mypy", "mypy is not installed (not in .venv, venv or PATH)");
    };
    o.tool_version = version(&bin, &ctx.root).map(|v| format!("{v} env:{}", environment(&ctx.root)));
    // mypy checks its configured `files` when it has them, else the project
    let has_files = pyproject(&ctx.root, "tool.mypy").is_some_and(|b| b.lines().any(|l| l.trim_start().starts_with("files")))
        || ["mypy.ini", ".mypy.ini", "setup.cfg"].iter().any(|f| toml_section(&read(&ctx.root, f), "mypy").is_some_and(|b| b.lines().any(|l| l.trim_start().starts_with("files"))));
    let command = cfg["command"].as_str().map(String::from).unwrap_or_else(|| format!("'{}'{}", bin.display(), if has_files { "" } else { " ." }));
    let fp = util::fingerprint(&["python-mypy", &command]);
    let inputs = carry::default_inputs(cfg, &["python"]);
    match carry::try_carry(ctx, &mut o, &fp, &inputs) {
        Ok(()) => {}
        Err(reasons) => {
            o.mode = Mode::Full;
            o.reasons = reasons;
            o.reasons.push("mypy runs in full; its own cache rechecks only modules a change reaches".into());
            match util::run(util::sh(&ctx.root, &command), ctx.timeout) {
                Ok(r) => {
                    o.diagnostics = r.stdout.lines().filter(|l| l.contains(": error:")).map(String::from).collect();
                    o.errors = o.diagnostics.len();
                    o.verdict = match r.status.code() {
                        Some(0) => Verdict::Pass,
                        Some(1) => Verdict::Fail,
                        _ => {
                            o.diagnostics.extend(r.tail(20));
                            Verdict::Undecided
                        }
                    };
                }
                Err(e) => o.reasons.push(e),
            }
        }
    }
    carry::record(ctx, &mut o, &fp);
    o
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PyrightState {
    schema: String,
    version: Option<String>,
    env: String,
    /// file -> its errors
    errors: BTreeMap<String, Vec<String>>,
}

/// The files pyright checks in full: Python files in the tree, filtered by the
/// configuration's include, exclude and ignore lists.
fn pyright_scope(root: &Path, files: &[String]) -> Vec<String> {
    let (mut include, mut exclude, mut ignore) = (Vec::new(), Vec::new(), Vec::new());
    let json_cfg = std::fs::read_to_string(root.join("pyrightconfig.json")).ok().and_then(|t| {
        let no_comments: String = t.lines().map(|l| if l.trim_start().starts_with("//") { "" } else { l }).collect::<Vec<_>>().join("\n");
        serde_json::from_str::<Value>(&no_comments).ok()
    });
    let strs = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
    if let Some(c) = &json_cfg {
        include = strs(&c["include"]);
        exclude = strs(&c["exclude"]);
        ignore = strs(&c["ignore"]);
    } else if let Some(body) = pyproject(root, "tool.pyright").or_else(|| pyproject(root, "tool.basedpyright")) {
        include = toml_strings(&body, "include").unwrap_or_default();
        exclude = toml_strings(&body, "exclude").unwrap_or_default();
        ignore = toml_strings(&body, "ignore").unwrap_or_default();
    }
    if exclude.is_empty() {
        exclude = vec!["**/node_modules".into(), "**/__pycache__".into(), "**/.*".into()];
    }
    let norm = |e: &String| e.trim_start_matches("./").trim_end_matches('/').to_string();
    let hit = |entries: &[String], f: &str| {
        entries.iter().map(norm).any(|e| e.is_empty() || e == "." || f == e || f.starts_with(&format!("{e}/")) || util::glob(&e, f) || util::glob(&format!("{e}/**"), f))
    };
    files
        .iter()
        .filter(|f| is_py(f))
        .filter(|f| include.is_empty() || hit(&include, f))
        .filter(|f| !hit(&exclude, f) && !hit(&ignore, f))
        .cloned()
        .collect()
}

const PYRIGHT_GLOBAL: &[&str] = &[
    "pyrightconfig.json",
    "pyproject.toml",
    "setup.cfg",
    "**/py.typed",
    "**/*.pth",
    "requirements*.txt",
    "**/requirements*.txt",
    "uv.lock",
    "poetry.lock",
    "Pipfile",
    "Pipfile.lock",
    ".python-version",
];

fn run_pyright(ctx: &Ctx, bin: &Path, files: Option<&[String]>) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut c = std::process::Command::new(bin);
    c.arg("--outputjson").current_dir(&ctx.root);
    if let Some(f) = files {
        c.args(f);
    }
    let r = util::run(c, ctx.timeout)?;
    if !matches!(r.status.code(), Some(0) | Some(1)) {
        return Err(format!("pyright exited {:?}: {}", r.status.code(), r.tail(10).join(" ")));
    }
    let v: Value = serde_json::from_str(&r.stdout).map_err(|e| format!("pyright output is not JSON: {e}"))?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in v["generalDiagnostics"].as_array().into_iter().flatten() {
        if d["severity"].as_str() != Some("error") {
            continue;
        }
        let abs = d["file"].as_str().unwrap_or("");
        let rel = Path::new(abs)
            .strip_prefix(&ctx.root)
            .or_else(|_| Path::new(abs).strip_prefix(ctx.root.canonicalize().unwrap_or_default()))
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| abs.to_string());
        let line = d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
        let col = d["range"]["start"]["character"].as_u64().unwrap_or(0) + 1;
        let rule = d["rule"].as_str().map(|r| format!(" ({r})")).unwrap_or_default();
        out.entry(rel.clone()).or_default().push(format!("{rel}:{line}:{col} - error: {}{rule}", d["message"].as_str().unwrap_or("")));
    }
    Ok(out)
}

fn pyright(ctx: &Ctx, _cfg: &Value) -> Outcome {
    let name = if pyproject(&ctx.root, "tool.basedpyright").is_some() { "basedpyright" } else { "pyright" };
    let Some(bin) = which(&ctx.root, name).or_else(|| which(&ctx.root, "pyright")) else {
        return Outcome::undecided("python", name, format!("{name} is not installed (not in .venv, venv, node_modules/.bin or PATH)"));
    };
    let mut o = Outcome::new("python", name);
    o.tool_version = version(&bin, &ctx.root);
    let env = environment(&ctx.root);
    let fp = util::fingerprint(&["python-pyright"]);
    let files = match head_files(ctx) {
        Ok(f) => f,
        Err(e) => return Outcome::undecided("python", name, e),
    };
    let scope = pyright_scope(&ctx.root, &files);

    let mut reasons = Vec::new();
    let mut base: Option<(PyrightState, String, String)> = None;
    if ctx.full {
        reasons.push("full: requested".to_string());
    } else {
        match ctx.state_candidates("python", &fp, false).into_iter().next() {
            Some((from, p)) => match std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<PyrightState>(&t).ok().map(|s| (s, util::digest(&t)))) {
                Some((s, d)) => base = Some((s, from, d)),
                None => reasons.push("no-state: the base's python state could not be read".into()),
            },
            None => reasons.push("no-state: no python verdict recorded at the base tree".into()),
        }
    }
    let mut affected: BTreeSet<String> = BTreeSet::new();
    let mut cutoff = 0usize;
    if let Some((st, _, _)) = &base {
        if st.version != o.tool_version {
            reasons.push(format!("toolchain: {:?} -> {:?}", st.version, o.tool_version));
        }
        if st.env != env {
            reasons.push("environment: the interpreter or installed packages changed since the base".into());
        }
        match changes(ctx) {
            None => reasons.push("changes unknown: no base tree to compare with".into()),
            Some(ch) => {
                let mut py = Vec::new();
                for (s, p) in &ch {
                    if PYRIGHT_GLOBAL.iter().any(|g| util::glob(g, p)) {
                        reasons.push(format!("configuration: {p}"));
                    } else if is_py(p) {
                        py.push((*s, p.clone()));
                    }
                    // any other file is invisible to the type checker
                }
                if reasons.is_empty() && !py.is_empty() {
                    let in_scope: BTreeSet<&String> = scope.iter().collect();
                    // early cutoff: a module whose interface did not change reaches no other file
                    let (quiet, loud): (Vec<_>, Vec<_>) = py.into_iter().partition(|(s, p)| *s == 'M' && p.ends_with(".py") && interface_unchanged(ctx, p));
                    cutoff = quiet.len();
                    let quiet: Vec<String> = quiet.into_iter().map(|(_, p)| p).collect();
                    affected = deps::closure_with_cutoff(&ctx.root, &files, &loud, &quiet, &deps::PYTHON).into_iter().filter(|f| in_scope.contains(f)).collect();
                    if affected.len() * 10 >= scope.len() * 7 && scope.len() > 20 {
                        reasons.push(format!("{} of {} files can see the change: one full run is faster", affected.len(), scope.len()));
                    }
                }
            }
        }
    }

    let result = if reasons.is_empty() {
        let (st, from, digest) = base.as_ref().expect("incremental implies a base state");
        o.mode = Mode::Incremental;
        o.state_from = Some(from.clone());
        o.state_in = Some(digest.clone());
        o.rechecked = affected.iter().cloned().collect();
        o.reasons.push(if affected.is_empty() {
            "no checked file can import what changed: the base's errors carry".into()
        } else {
            "rechecked: changed files and every file whose imports can reach them; other files' errors carry from the base".into()
        });
        let fresh = if affected.is_empty() { Ok(BTreeMap::new()) } else { run_pyright(ctx, &bin, Some(&o.rechecked)) };
        fresh.map(|fresh| {
            let live: BTreeSet<&String> = scope.iter().collect();
            let mut all: BTreeMap<String, Vec<String>> =
                st.errors.iter().filter(|(f, _)| live.contains(f) && !affected.contains(*f)).map(|(f, e)| (f.clone(), e.clone())).collect();
            all.extend(fresh.into_iter().filter(|(f, _)| affected.contains(f)));
            all
        })
    } else {
        o.mode = Mode::Full;
        o.reasons = reasons;
        o.rechecked = scope.clone();
        run_pyright(ctx, &bin, None).map(|all| {
            let live: BTreeSet<&String> = scope.iter().collect();
            all.into_iter().filter(|(f, _)| live.contains(f) || !is_py(f)).collect()
        })
    };
    match result {
        Err(e) => {
            o.verdict = Verdict::Undecided;
            o.diagnostics.push(e);
        }
        Ok(errors) => {
            o.diagnostics = errors.values().flatten().cloned().collect();
            o.errors = o.diagnostics.len();
            o.verdict = if o.errors == 0 { Verdict::Pass } else { Verdict::Fail };
            if !ctx.args.no_cache {
                let st = PyrightState { schema: "sem-check-pyright/1".into(), version: o.tool_version.clone(), env, errors };
                if let (Ok(scratch), Ok(text)) = (ctx.scratch(), serde_json::to_string(&st)) {
                    let p = scratch.path("state.json");
                    if std::fs::write(&p, &text).is_ok() && ctx.save_state("python", &fp, &p).is_some() {
                        o.state_out = Some(util::digest(&text));
                    }
                }
            }
        }
    }
    o.extra = json!({ "filesInScope": scope.len(), "interfaceUnchanged": cutoff });
    o
}

// ── early cutoff ───────────────────────────────────────────────────────────

/// For each line: does it start inside a string literal (a triple-quoted
/// string, or a bracket or backslash continuation of the previous line)?
fn continued_lines(text: &str) -> Vec<bool> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<(u8, bool)> = None; // (quote char, triple)
    let mut depth = 0i32;
    out.push(false);
    while i < b.len() {
        let c = b[i];
        if let Some((q, triple)) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q && (!triple || b[i..].starts_with(&[q, q, q])) {
                i += if triple { 3 } else { 1 };
                quote = None;
                continue;
            }
            if c == b'\n' {
                if !triple {
                    quote = None;
                }
                out.push(triple);
            }
            i += 1;
            continue;
        }
        match c {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' | b'\'' => {
                let triple = b[i..].starts_with(&[c, c, c]);
                quote = Some((c, triple));
                i += if triple { 3 } else { 1 };
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'\\' if b.get(i + 1) == Some(&b'\n') => {
                out.push(true);
                i += 2;
                continue;
            }
            b'\n' => {
                out.push(depth > 0);
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The module's interface as pyright's other files see it: the text with the
/// body of every module-level function that declares its return type replaced
/// by a placeholder. `None` when a body could reach module state (`global`,
/// `nonlocal`), so no cutoff is safe.
fn python_interface(text: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let cont = continued_lines(text);
    let top = |i: usize| {
        let l = lines[i];
        !cont.get(i).copied().unwrap_or(false) && !l.is_empty() && !l.starts_with([' ', '\t', '#'])
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if top(i) && (l.starts_with("def ") || l.starts_with("async def ")) {
            // the signature runs to the line ending its header
            let mut j = i;
            let mut header = String::from(l);
            while j + 1 < lines.len() && cont.get(j + 1).copied().unwrap_or(false) {
                j += 1;
                header.push('\n');
                header.push_str(lines[j]);
            }
            let mut k = j + 1;
            while k < lines.len() && !top(k) {
                k += 1;
            }
            let body = &lines[j + 1..k];
            let annotated = header.contains("->");
            if body.iter().any(|b| {
                let t = b.trim_start();
                t.starts_with("global ") || t.starts_with("nonlocal ")
            }) {
                return None;
            }
            out.push(header);
            if annotated {
                out.push("<body>".into());
            } else {
                out.extend(body.iter().map(|s| s.to_string()));
            }
            i = k;
            continue;
        }
        out.push(l.to_string());
        i += 1;
    }
    // trailing blank lines and comments between functions do not matter
    Some(out.into_iter().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).collect())
}

/// Did the interface of a modified Python file stay the same?
fn interface_unchanged(ctx: &Ctx, path: &str) -> bool {
    let Some(base) = &ctx.base else { return false };
    let (Ok(before), Ok(after)) = (tree::blob(&ctx.root, &base.tree, path), std::fs::read_to_string(ctx.root.join(path))) else {
        return false;
    };
    matches!((python_interface(&before), python_interface(&after)), (Some(a), Some(b)) if a == b)
}

// ── pytest ─────────────────────────────────────────────────────────────────

fn pytest_config(root: &Path) -> Option<String> {
    if let Ok(t) = std::fs::read_to_string(root.join("pytest.ini")) {
        return Some(t);
    }
    if let Some(b) = pyproject(root, "tool.pytest.ini_options") {
        return Some(b);
    }
    for (f, s) in [("tox.ini", "pytest"), ("setup.cfg", "tool:pytest")] {
        if let Some(b) = toml_section(&read(root, f), s) {
            return Some(b);
        }
    }
    None
}

pub(crate) fn detect_pytest(root: &Path) -> bool {
    (pytest_config(root).is_some() || root.join("conftest.py").exists()) && which(root, "pytest").is_some()
}

fn is_test_file(p: &str) -> bool {
    let l = leaf(p);
    l.ends_with(".py") && (l.starts_with("test_") || l.ends_with("_test.py"))
}

const NORECURSE: &[&str] = &["**/.*/**", "**/build/**", "**/dist/**", "**/node_modules/**", "**/venv/**", "**/*.egg/**", "**/_darcs/**", "**/CVS/**", "**/{arch}/**"];

const PYTEST_GLOBAL: &[&str] = &[
    "pytest.ini",
    "pyproject.toml",
    "setup.cfg",
    "tox.ini",
    "setup.py",
    "**/conftest.py",
    "requirements*.txt",
    "**/requirements*.txt",
    "uv.lock",
    "poetry.lock",
    "Pipfile",
    "Pipfile.lock",
    ".python-version",
    "**/.env*",
];

#[derive(serde::Serialize, serde::Deserialize)]
struct PytestState {
    schema: String,
    version: Option<String>,
    /// test file -> its failure lines (`FAILED path::test - why`)
    failed: BTreeMap<String, Vec<String>>,
    /// a failure no test file owns (a conftest, a plugin, the session)
    global: bool,
}

struct PytestRun {
    failed: BTreeMap<String, Vec<String>>,
    global: Vec<String>,
    exit: Option<i32>,
    command: String,
}

/// Run pytest on `files` (all tests when `None`) and attribute every failure
/// in its short summary to the test file it names.
fn run_pytest(ctx: &Ctx, bin: &Path, files: Option<&[String]>) -> Result<PytestRun, String> {
    let mut command = format!("'{}' -q -rfE", bin.display());
    for f in files.into_iter().flatten() {
        command.push_str(&format!(" '{f}'"));
    }
    let r = util::run(util::sh(&ctx.root, &command), ctx.timeout)?;
    let exit = r.status.code();
    let mut failed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut global = Vec::new();
    for l in r.stdout.lines() {
        let Some(rest) = l.strip_prefix("FAILED ").or_else(|| l.strip_prefix("ERROR ")) else { continue };
        let path = rest.split("::").next().unwrap_or(rest).split(" - ").next().unwrap_or(rest).trim();
        if is_test_file(path) {
            failed.entry(path.to_string()).or_default().push(l.to_string());
        } else {
            global.push(l.to_string());
        }
    }
    match exit {
        Some(0) | Some(5) => {}
        Some(1) | Some(2) if !failed.is_empty() || !global.is_empty() => {}
        Some(1) | Some(2) => global.extend(r.tail(30)),
        _ => return Err(format!("pytest exited {exit:?}: {}", r.tail(10).join(" "))),
    }
    Ok(PytestRun { failed, global, exit, command })
}

pub(crate) fn pytest(ctx: &Ctx) -> Outcome {
    let cfg = ctx.cfg("pytest");
    let Some(bin) = which(&ctx.root, "pytest") else {
        return Outcome::undecided("pytest", "pytest", "pytest is not installed (not in .venv, venv or PATH)");
    };
    let mut o = Outcome::new("pytest", "pytest");
    o.tool_version = version(&bin, &ctx.root).map(|v| format!("{v} env:{}", environment(&ctx.root)));
    let fp = util::fingerprint(&["pytest/2"]);
    let conf = pytest_config(&ctx.root).unwrap_or_default();
    let extra_inert: Vec<String> = cfg["inert"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();

    let mut reasons = Vec::new();
    let mut base: Option<(PytestState, String, String)> = None;
    if ctx.full {
        reasons.push("full: requested".to_string());
    } else {
        match ctx.state_candidates("pytest", &fp, false).into_iter().next() {
            Some((from, p)) => match std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<PytestState>(&t).ok().map(|s| (s, util::digest(&t)))) {
                Some((s, d)) => base = Some((s, from, d)),
                None => reasons.push("no-state: the base's pytest state could not be read".into()),
            },
            None => reasons.push("no-state: no pytest verdict recorded at the base tree".into()),
        }
    }
    for k in ["python_files", "python_classes", "python_functions", "--ignore", "--deselect", "collect_ignore", "rootdir"] {
        if conf.contains(k) {
            reasons.push(format!("pytest configuration sets {k}: test discovery is not the default"));
        }
    }
    let testpaths = toml_strings(&conf, "testpaths").or_else(|| {
        conf.lines().find_map(|l| l.trim().strip_prefix("testpaths").map(|r| r.trim_start_matches([' ', '=']).split_whitespace().map(String::from).collect()))
    });
    let collected = |f: &String| {
        is_test_file(f)
            && !NORECURSE.iter().any(|g| util::glob(g, f))
            && testpaths.as_ref().is_none_or(|tp| tp.iter().any(|t| f.starts_with(&format!("{}/", t.trim_end_matches('/'))) || f == t))
    };
    let files = head_files(ctx);
    let mut selected: BTreeSet<String> = BTreeSet::new();
    if let Some((st, _, _)) = &base {
        if st.global {
            reasons.push("the base had a failure outside any test file: it may affect every test".into());
        }
        if st.version != o.tool_version {
            reasons.push("toolchain or environment changed since the base".into());
        }
        match (changes(ctx), &files) {
            (None, _) => reasons.push("changes unknown: no base tree to compare with".into()),
            (_, Err(e)) => reasons.push(e.clone()),
            (Some(ch), Ok(files)) => {
                let mut py = Vec::new();
                for (s, p) in &ch {
                    if PYTEST_GLOBAL.iter().any(|g| util::glob(g, p)) {
                        reasons.push(format!("runner input: {p}"));
                    } else if is_py(p) {
                        py.push((*s, p.clone()));
                    } else if !(carry::inert(p, &extra_inert) || carry::foreign(p, &["python"])) {
                        reasons.push(format!("unmatched: {p} is not Python, and tests may read it"));
                    }
                }
                if reasons.is_empty() {
                    let pyfiles: Vec<String> = files.iter().filter(|f| is_py(f)).cloned().collect();
                    selected = deps::closure(&ctx.root, &pyfiles, &py, &deps::PYTHON_RUNTIME).into_iter().filter(collected).collect();
                }
            }
        }
    }
    let all_tests: BTreeSet<String> = files.as_ref().map(|f| f.iter().filter(|f| collected(f)).cloned().collect()).unwrap_or_default();
    let result = if reasons.is_empty() {
        let (st, from, digest) = base.as_ref().expect("incremental implies a base state");
        o.mode = Mode::Incremental;
        o.state_from = Some(from.clone());
        o.state_in = Some(digest.clone());
        o.rechecked = selected.iter().cloned().collect();
        o.reasons.push(if selected.is_empty() {
            "no test file can load what changed: every test file's result carries from the base".into()
        } else {
            "affected tests: test files that can load a changed file; every other test file's result carries from the base".into()
        });
        let fresh = if selected.is_empty() { Ok(None) } else { run_pytest(ctx, &bin, Some(&o.rechecked)).map(Some) };
        fresh.map(|fresh| {
            let mut failed: BTreeMap<String, Vec<String>> =
                st.failed.iter().filter(|(f, _)| all_tests.contains(*f) && !selected.contains(*f)).map(|(f, e)| (f.clone(), e.clone())).collect();
            let mut global = Vec::new();
            if let Some(r) = fresh {
                failed.extend(r.failed);
                global = r.global;
                o.extra = json!({ "command": r.command, "exitCode": r.exit });
            }
            (failed, global)
        })
    } else {
        o.mode = Mode::Full;
        o.reasons = reasons;
        o.rechecked = all_tests.iter().cloned().collect();
        run_pytest(ctx, &bin, None).map(|r| {
            o.extra = json!({ "command": r.command, "exitCode": r.exit });
            (r.failed, r.global)
        })
    };
    match result {
        Err(e) => {
            o.verdict = Verdict::Undecided;
            o.diagnostics.push(e);
        }
        Ok((failed, global)) => {
            o.diagnostics = failed.values().flatten().chain(global.iter()).cloned().collect();
            o.errors = o.diagnostics.len();
            o.verdict = if o.errors == 0 { Verdict::Pass } else { Verdict::Fail };
            if !ctx.args.no_cache {
                let st = PytestState { schema: "sem-check-pytest/2".into(), version: o.tool_version.clone(), failed, global: !global.is_empty() };
                if let (Ok(scratch), Ok(text)) = (ctx.scratch(), serde_json::to_string(&st)) {
                    let p = scratch.path("state.json");
                    if std::fs::write(&p, &text).is_ok() && ctx.save_state("pytest", &fp, &p).is_some() {
                        o.state_out = Some(util::digest(&text));
                    }
                }
            }
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_arrays() {
        let t = "[tool.pyright]\ninclude = [\"src\", 'lib']\nexclude = [\n  \"src/gen\",\n]\n[tool.other]\ninclude = [\"x\"]\n";
        let b = toml_section(t, "tool.pyright").unwrap();
        assert_eq!(toml_strings(&b, "include").unwrap(), vec!["src", "lib"]);
        assert_eq!(toml_strings(&b, "exclude").unwrap(), vec!["src/gen"]);
    }

    #[test]
    fn annotated_bodies_are_not_interface() {
        let a = "import os\n\ndef f(x: int) -> int:\n    return x\n\ndef g(x):\n    return x\n";
        let body = "import os\n\ndef f(x: int) -> int:\n    y = x + 1  # c\n    return y\n\ndef g(x):\n    return x\n";
        let unannotated = "import os\n\ndef f(x: int) -> int:\n    return x\n\ndef g(x):\n    return str(x)\n";
        let sig = "import os\n\ndef f(x: str) -> int:\n    return 1\n\ndef g(x):\n    return x\n";
        let multi = "def f(\n    x: int,\n) -> int:\n    return x\n";
        let multi2 = "def f(\n    x: int,\n) -> int:\n    return x + 1\n";
        assert_eq!(python_interface(a), python_interface(body));
        assert_ne!(python_interface(a), python_interface(unannotated));
        assert_ne!(python_interface(a), python_interface(sig));
        assert_eq!(python_interface(multi), python_interface(multi2));
        assert!(python_interface("def f() -> None:\n    global X\n    X = 1\n").is_none());
        // a "def" inside a module-level string is not a function
        let s1 = "DOC = \"\"\"\ndef f() -> int:\n    old\n\"\"\"\n";
        let s2 = "DOC = \"\"\"\ndef f() -> int:\n    new\n\"\"\"\n";
        assert_ne!(python_interface(s1), python_interface(s2));
    }

    #[test]
    fn test_files_follow_pytest_defaults() {
        assert!(is_test_file("tests/test_api.py"));
        assert!(is_test_file("pkg/api_test.py"));
        assert!(!is_test_file("pkg/testing.py"));
        assert!(!is_test_file("tests/conftest.py"));
    }
}
