//! Tests: affected-test selection over sem's runtime module graph.
//!
//! The full verdict is the test runner's exit status on every test. The
//! incremental verdict runs only the test files that can load a changed file
//! (the reverse value-import closure, snapshot owners, changed test files,
//! tests that loaded a deleted file at the base) and carries every other test
//! file's result over from the state recorded at the base tree — exactly the
//! base tree: test outcomes are not self-validating the way compiler inputs
//! are, so a state from any other tree is never used.
//!
//! All tests run instead (and the reason is reported) when there is no state
//! at the base, the runner's version changed, the base itself had a failure
//! outside any test file, a changed path is a runner/build/environment input
//! (package.json, lockfiles, tsconfig, .env, runner and babel configs, setup
//! files and anything they load), a changed path is not in the module graph
//! and not inert, an unresolved relative import may name a changed path, or a
//! file was added while the runner's config narrows which files are tests.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde_json::{json, Value};

use super::util;
use super::{Ctx, Mode, Outcome, Verdict};

const GLOBAL: &[&str] = &[
    "**/package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "bun.lock",
    "bun.lockb",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
    "**/tsconfig*.json",
    "**/.env*",
    "**/vitest.config.*",
    "**/vitest.workspace.*",
    "**/vitest.setup.*",
    "**/vite.config.*",
    "**/jest.config.*",
    "**/jest.setup.*",
    "**/setupTests.*",
    "**/babel.config.*",
    "**/.babelrc*",
    "**/.swcrc",
    "**/postcss.config.*",
    "**/tailwind.config.*",
];

const INERT: &[&str] = &[
    "**/*.md",
    "**/*.mdx",
    "**/LICENSE*",
    "**/CHANGELOG*",
    ".github/**",
    ".husky/**",
    ".vscode/**",
    ".idea/**",
    "docs/**",
    "dev-docs/**",
    ".gitignore",
    ".gitattributes",
    ".prettierignore",
    ".prettierrc*",
    ".eslintignore",
    "**/.eslintrc*",
    "eslint.config.*",
    ".editorconfig",
    ".dockerignore",
    "Dockerfile",
    "docker-compose.yml",
    ".sem/**",
];

const TEST_FILE: &str = r"(^|/)(__tests__/.*\.[cm]?[jt]sx?|[^/]*\.(test|spec)\.[cm]?[jt]sx?)$";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Runner {
    Vitest,
    Jest,
}

impl Runner {
    fn name(self) -> &'static str {
        match self {
            Runner::Vitest => "vitest",
            Runner::Jest => "jest",
        }
    }
}

fn package_json(root: &Path) -> Value {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn scripts_mention(root: &Path, word: &str) -> bool {
    package_json(root)["scripts"]
        .as_object()
        .is_some_and(|s| s.values().any(|c| c.as_str().is_some_and(|c| c.split(|ch: char| !(ch.is_alphanumeric() || ch == '-')).any(|w| w == word))))
}

fn config_files(root: &Path, stem: &str) -> Vec<String> {
    let mut v = Vec::new();
    for ext in ["ts", "mts", "cts", "js", "mjs", "cjs", "json"] {
        let f = format!("{stem}.{ext}");
        if root.join(&f).exists() {
            v.push(f);
        }
    }
    v
}

pub(crate) fn detect(root: &Path, config: &Value) -> Option<String> {
    if let Some(r) = config.pointer("/tests/runner").and_then(Value::as_str) {
        return Some(r.to_string());
    }
    if util::node_bin(root, "vitest").is_some()
        && (!config_files(root, "vitest.config").is_empty() || scripts_mention(root, "vitest"))
    {
        return Some("vitest".into());
    }
    if util::node_bin(root, "jest").is_some() && (!config_files(root, "jest.config").is_empty() || scripts_mention(root, "jest")) {
        return Some("jest".into());
    }
    None
}

struct Run {
    files: BTreeMap<String, FileResult>,
    exit_ok: bool,
    tail: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct FileResult {
    ok: bool,
    failed: Vec<String>,
}

fn run_tests(ctx: &Ctx, runner: Runner, files: Option<&[String]>) -> Result<Run, String> {
    let bin = util::node_bin(&ctx.root, runner.name()).ok_or_else(|| format!("{} is not installed", runner.name()))?;
    let scratch = ctx.scratch().map_err(|e| e.to_string())?;
    let out = scratch.path("report.json");
    let mut cmd = std::process::Command::new(bin);
    cmd.current_dir(&ctx.root).env("CI", "true").env("FORCE_COLOR", "0").env("NO_COLOR", "1");
    match runner {
        Runner::Vitest => {
            cmd.arg("run").arg("--reporter=json").arg(format!("--outputFile={}", out.display()));
        }
        Runner::Jest => {
            cmd.arg("--ci").arg("--json").arg(format!("--outputFile={}", out.display()));
        }
    }
    for a in ctx.cfg("tests").get("args").and_then(Value::as_array).into_iter().flatten() {
        if let Some(a) = a.as_str() {
            cmd.arg(a);
        }
    }
    if let Some(fs) = files {
        cmd.arg("--passWithNoTests");
        if runner == Runner::Jest {
            cmd.arg("--runTestsByPath");
        }
        cmd.args(fs);
    }
    let ran = util::run(cmd, ctx.timeout)?;
    let report: Value = std::fs::read_to_string(&out)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let mut map = BTreeMap::new();
    for tr in report["testResults"].as_array().into_iter().flatten() {
        let name = tr["name"].as_str().unwrap_or("");
        let rel = Path::new(name)
            .strip_prefix(&ctx.root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| name.to_string());
        let mut failed: Vec<String> = tr["assertionResults"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["status"] == "failed")
            .map(|a| {
                let msg = a["failureMessages"].as_array().and_then(|m| m.first()).and_then(Value::as_str).unwrap_or("");
                format!("{rel} :: {}: {}", a["fullName"].as_str().unwrap_or("?"), msg.lines().next().unwrap_or(""))
            })
            .collect();
        let status_failed = tr["status"] == "failed";
        if status_failed && failed.is_empty() {
            let msg = tr["message"].as_str().unwrap_or("the file failed to run");
            failed.push(format!("{rel} :: <file>: {}", msg.lines().next().unwrap_or("")));
        }
        map.insert(rel, FileResult { ok: !status_failed && failed.is_empty(), failed });
    }
    if report.is_null() && !ran.ok() {
        return Ok(Run { files: map, exit_ok: false, tail: ran.tail(60) });
    }
    Ok(Run { files: map, exit_ok: ran.ok(), tail: ran.tail(60) })
}

/// Setup files named in the runner configs (`setupFiles`, `setupFilesAfterEnv`,
/// `globalSetup`), repo-relative.
fn setup_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let re = regex::Regex::new(r#"(?s)(setupFiles|setupFilesAfterEnv|globalSetup)\s*:\s*(\[[^\]]*\]|["'][^"']+["'])"#).unwrap();
    let lit = regex::Regex::new(r#"["']([^"']+)["']"#).unwrap();
    let mut cfgs = config_files(root, "vitest.config");
    cfgs.extend(config_files(root, "vite.config"));
    cfgs.extend(config_files(root, "jest.config"));
    for c in cfgs {
        let Ok(t) = std::fs::read_to_string(root.join(&c)) else { continue };
        for m in re.captures_iter(&t) {
            for l in lit.captures_iter(&m[2]) {
                let p = l[1].trim_start_matches("<rootDir>/").trim_start_matches("./").to_string();
                out.push(p);
            }
        }
    }
    out
}

fn custom_include(root: &Path) -> bool {
    let mut cfgs = config_files(root, "vitest.config");
    cfgs.extend(config_files(root, "vite.config"));
    cfgs.extend(config_files(root, "jest.config"));
    cfgs.iter().any(|c| {
        std::fs::read_to_string(root.join(c))
            .is_ok_and(|t| t.contains("include:") || t.contains("testMatch") || t.contains("testRegex"))
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    schema: String,
    runner: String,
    version: Option<String>,
    files: BTreeMap<String, FileResult>,
    exit_ok: bool,
    /// value edges of the base's runtime graph, as paths
    edges: Vec<(String, String)>,
}

fn read_state(p: &Path) -> Option<(State, String)> {
    let t = std::fs::read_to_string(p).ok()?;
    let s: State = serde_json::from_str(&t).ok()?;
    Some((s, util::digest(&t)))
}

pub(crate) fn run(ctx: &Ctx) -> Outcome {
    let runner = match detect(&ctx.root, &ctx.config).as_deref() {
        Some("vitest") => Runner::Vitest,
        Some("jest") => Runner::Jest,
        Some(r) => return Outcome::undecided("tests", r, format!("unsupported test runner `{r}` (vitest, jest; or use a \"commands\" entry)")),
        None => return Outcome::undecided("tests", "?", "no test runner found (vitest or jest in node_modules/.bin, named by a script or a config file)"),
    };
    let mut o = Outcome::new("tests", runner.name());
    o.tool_version = util::node_package(&ctx.root, runner.name()).and_then(|p| util::package_version(&p));
    let fp = util::fingerprint(&["tests", runner.name(), &ctx.cfg("tests").to_string()]);
    let graph = super::super::topology::runtime_graph(&ctx.root.to_string_lossy());
    let edges: Vec<(String, String)> =
        graph.edges.iter().map(|(a, b)| (graph.nodes[*a].0.clone(), graph.nodes[*b].0.clone())).collect();

    let mut reasons: Vec<String> = Vec::new();
    let mut base_state: Option<(State, String)> = None;
    if ctx.full {
        reasons.push("full: requested".into());
    } else {
        match ctx.state_candidates("tests", &fp, false).into_iter().next() {
            Some((from, p)) => match read_state(&p) {
                Some(s) => {
                    o.state_from = Some(from);
                    base_state = Some(s);
                }
                None => reasons.push("no-state: the base's test state could not be read".into()),
            },
            None => reasons.push("no-state: no test results recorded at the base tree".into()),
        }
    }
    let changed = ctx.changed.clone();
    if base_state.is_some() && changed.is_none() {
        reasons.push("changes unknown: no base tree to compare with".into());
    }
    let mut selected: BTreeSet<String> = BTreeSet::new();
    if let (Some((st, _)), Some(changed)) = (&base_state, &changed) {
        if st.version != o.tool_version {
            reasons.push(format!("version: {} {:?} -> {:?}", runner.name(), st.version, o.tool_version));
        }
        if !st.exit_ok && st.files.values().all(|f| f.ok) {
            reasons.push("the base run failed outside any test file".into());
        }
        let extra_global: Vec<String> = ctx.cfg("tests").get("fallback").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
        let extra_inert: Vec<String> = ctx.cfg("tests").get("inert").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
        let global: Vec<String> = GLOBAL.iter().map(|s| s.to_string()).chain(extra_global).collect();
        let inert: Vec<String> = INERT.iter().map(|s| s.to_string()).chain(extra_inert).collect();
        let idx: HashMap<&str, usize> = graph.nodes.iter().enumerate().map(|(i, n)| (n.0.as_str(), i)).collect();
        let mut rev: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
        let mut fwd: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
        for (a, b) in &graph.edges {
            rev[*b].push(*a);
            fwd[*a].push(*b);
        }
        // setup files and everything they load
        let mut setup: BTreeSet<usize> = BTreeSet::new();
        let mut stack: Vec<usize> = setup_files(&ctx.root).iter().filter_map(|f| idx.get(f.as_str()).copied()).collect();
        for (i, n) in graph.nodes.iter().enumerate() {
            if util::glob_any(&["**/setupTests.*".to_string(), "**/vitest.setup.*".into(), "**/jest.setup.*".into()], &n.0) {
                stack.push(i);
            }
        }
        while let Some(u) = stack.pop() {
            if setup.insert(u) {
                stack.extend(fwd[u].iter().copied());
            }
        }
        let old_rev: HashMap<&str, Vec<&str>> = {
            let mut m: HashMap<&str, Vec<&str>> = HashMap::new();
            for (a, b) in &st.edges {
                m.entry(b.as_str()).or_default().push(a.as_str());
            }
            m
        };
        let test_re = regex::Regex::new(TEST_FILE).unwrap();
        let is_test = |p: &str| test_re.is_match(p);
        let custom = custom_include(&ctx.root);
        let mut seeds: Vec<usize> = Vec::new();
        for c in changed {
            let p = c.path.as_str();
            if let Some(owner) = super::super::topology::snapshot_owner(p) {
                if c.status != 'D' && ctx.root.join(&owner).exists() {
                    selected.insert(owner);
                }
                continue;
            }
            if util::glob_any(&global, p) {
                reasons.push(format!("runner input: {p}"));
                continue;
            }
            if c.status == 'A' && custom {
                reasons.push(format!("added {p} while the runner config narrows which files are tests (include/testMatch)"));
                continue;
            }
            if let Some(&u) = idx.get(p) {
                if setup.contains(&u) {
                    reasons.push(format!("setup closure: {p} is loaded by the test setup"));
                    continue;
                }
                seeds.push(u);
                if is_test(p) {
                    selected.insert(p.to_string());
                }
            } else if c.status == 'D' && (old_rev.contains_key(p) || st.files.contains_key(p)) {
                // tests that loaded it at the base
                let mut seen: BTreeSet<&str> = BTreeSet::new();
                let mut stack = vec![p];
                while let Some(x) = stack.pop() {
                    if !seen.insert(x) {
                        continue;
                    }
                    if test_re.is_match(x) && x != p && ctx.root.join(x).exists() {
                        selected.insert(x.to_string());
                    }
                    for y in old_rev.get(x).into_iter().flatten() {
                        stack.push(y);
                    }
                }
            } else if !util::glob_any(&inert, p) {
                reasons.push(format!("unmatched: {p} is not in the module graph and not known to be inert"));
            }
        }
        for (src, spec) in &graph.unresolved {
            if !spec.starts_with('.') {
                continue;
            }
            let dir = Path::new(src).parent().unwrap_or(Path::new(""));
            let base = normalize(&dir.join(spec).to_string_lossy());
            for c in changed {
                let p = &c.path;
                if *p == base || p.starts_with(&format!("{base}.")) || p.starts_with(&format!("{base}/index.")) {
                    reasons.push(format!("unresolved-import: {src} imports {spec}, which may be {p}"));
                }
            }
        }
        // reverse closure from the changed nodes
        let mut seen = vec![false; graph.nodes.len()];
        let mut stack = seeds.clone();
        for &s in &seeds {
            seen[s] = true;
        }
        while let Some(u) = stack.pop() {
            let id = &graph.nodes[u].0;
            if test_re.is_match(id) {
                selected.insert(id.clone());
            }
            for &p in &rev[u] {
                if !seen[p] {
                    seen[p] = true;
                    stack.push(p);
                }
            }
        }
    }

    if reasons.is_empty() {
        let (st, digest) = base_state.take().unwrap();
        o.state_in = Some(digest);
        o.mode = Mode::Incremental;
        let sel: Vec<String> = selected.into_iter().filter(|f| ctx.root.join(f).exists()).collect();
        let mut files = st.files.clone();
        let deleted: Vec<String> = changed.iter().flatten().filter(|c| c.status == 'D').map(|c| c.path.clone()).collect();
        for d in &deleted {
            files.remove(d);
        }
        let mut exit_ok = true;
        let mut tail = Vec::new();
        if !sel.is_empty() {
            match run_tests(ctx, runner, Some(&sel)) {
                Ok(r) => {
                    for f in &sel {
                        files.remove(f);
                    }
                    for (k, v) in r.files {
                        files.insert(k, v);
                    }
                    exit_ok = r.exit_ok;
                    tail = r.tail;
                }
                Err(e) => return Outcome::undecided("tests", runner.name(), e),
            }
        }
        o.reasons.push(if sel.is_empty() {
            "no test file can load a changed file: every result carried from the base".into()
        } else {
            "affected tests: only test files that can load a changed file ran".into()
        });
        o.rechecked = sel;
        finish(ctx, &mut o, files, exit_ok, tail, edges, &fp);
    } else {
        o.reasons = reasons;
        o.mode = Mode::Full;
        match run_tests(ctx, runner, None) {
            Ok(r) => {
                o.rechecked = r.files.keys().cloned().collect();
                finish(ctx, &mut o, r.files, r.exit_ok, r.tail, edges, &fp);
            }
            Err(e) => return Outcome::undecided("tests", runner.name(), e),
        }
    }
    o
}

fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

fn finish(
    ctx: &Ctx,
    o: &mut Outcome,
    files: BTreeMap<String, FileResult>,
    exit_ok: bool,
    tail: Vec<String>,
    edges: Vec<(String, String)>,
    fp: &str,
) {
    let failed: Vec<String> = files.values().flat_map(|f| f.failed.clone()).collect();
    let all_ok = files.values().all(|f| f.ok);
    o.errors = failed.len();
    o.diagnostics = failed;
    o.verdict = if all_ok && exit_ok { Verdict::Pass } else { Verdict::Fail };
    if all_ok && !exit_ok {
        o.diagnostics.push(format!("{} exited non-zero outside any test file:", o.tool));
        o.diagnostics.extend(tail);
        o.errors += 1;
    }
    o.extra = json!({ "testFiles": files.len(), "failedFiles": files.values().filter(|f| !f.ok).count() });
    let state = State {
        schema: "sem-check-tests/1".into(),
        runner: o.tool.clone(),
        version: o.tool_version.clone(),
        files,
        exit_ok,
        edges,
    };
    if ctx.args.no_cache {
        return;
    }
    if let Ok(scratch) = ctx.scratch() {
        let p = scratch.path("state.json");
        if let Ok(text) = serde_json::to_string(&state) {
            if std::fs::write(&p, &text).is_ok() && ctx.save_state("tests", fp, &p).is_some() {
                o.state_out = Some(util::digest(&text));
            }
        }
    }
}
