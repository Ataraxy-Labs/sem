//! `sem arch-diff <base>..<head>`: the architecture delta of a change —
//! what a reviewer reads instead of the line diff.
//!
//! Computed from the two committed trees, each section a set difference of
//! facts sem derives statically:
//!
//! - data paths (source -> sink), with witnesses, from [`sem_core::dataflow`];
//! - side effects per entity (what it reads / writes: env, files, DB,
//!   network, subprocess, logs, module state, fields);
//! - file and package dependencies (from resolved references, plus JS/TS
//!   module imports), cycles (SCCs), propagation cost and centrality;
//! - per-entity complexity (cyclomatic, cognitive);
//! - public signature changes and the callers they leave behind, and laws
//!   (layers, forbidden imports, ...), both from `sem certify`;
//! - what did *not* change, stated explicitly;
//! - how much of the code the analysis could not resolve (unknowns).
//!
//! Every finding carries a severity; output is ranked by it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use sem_core::dataflow::{self, models::Models, Analysis};
use sem_core::parser::graph::EntityGraph;
use sem_core::topology::algo;

use super::certify::{self, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    Markdown,
}

pub struct ArchDiffOptions {
    pub cwd: String,
    pub range: String,
    pub laws: Vec<PathBuf>,
    pub models: Vec<PathBuf>,
    pub format: Format,
    pub max_items: usize,
    /// Time the data-flow analysis of each tree may take.
    pub budget: Option<std::time::Duration>,
    /// Resident memory the process may reach during data flow, in bytes.
    pub max_memory: Option<usize>,
    /// Keep examples/, benches/ and tests in the dependency graph.
    pub include_examples: bool,
    /// Whole trees, or the diff's region (see `region`).
    pub scope: certify::Scope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    High,
    Medium,
    Low,
    Info,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Info => "info",
        }
    }
}

struct Finding {
    severity: Severity,
    kind: &'static str,
    title: String,
    details: Vec<String>,
    data: Value,
}

/// Peak resident bytes a whole-tree arch-diff takes per byte of head code,
/// both trees and data flow included. Measured: TypeScript 26x (Azure JS
/// SDK), Go 41x (Azure Go SDK, data flow cut short) to 121x (evergreen),
/// Rust 64x (risingwave), Python 137x (a generated 50k-file repo).
const FULL_BYTES_PER_SOURCE_BYTE: u64 = 128;

/// The `--scope` choice. `auto` analyzes whole trees when their estimated
/// peak fits in `max_memory_mb` (always, when it is 0).
pub fn scope_of(scope: &str, max_memory_mb: u64, region_mb: u64) -> certify::Scope {
    let budget = region_mb * 1024 * 1024;
    match scope {
        "full" => certify::Scope::Full,
        "diff" => certify::Scope::Diff { budget },
        _ if max_memory_mb == 0 => certify::Scope::Full,
        _ => certify::Scope::Auto { full_max: max_memory_mb * 1024 * 1024 / FULL_BYTES_PER_SOURCE_BYTE, budget },
    }
}

/// Models: built-ins, then `.sem/models/*.json` at head, then `--models`.
pub(crate) fn load_models(extra_dirs: &[&Path], files: &[PathBuf]) -> Result<Models, Box<dyn std::error::Error>> {
    let mut m = Models::builtin();
    let mut paths: Vec<PathBuf> = Vec::new();
    for d in extra_dirs {
        let dir = d.join(".sem").join("models");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
            v.sort();
            paths.extend(v);
        }
    }
    paths.extend(files.iter().cloned());
    for p in paths {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
        let errs = dataflow::models::validate(&v);
        if !errs.is_empty() {
            return Err(format!("{}: invalid models: {}", p.display(), errs.join("; ")).into());
        }
        m.add(&v, &p.to_string_lossy());
    }
    Ok(m)
}

/// Data flow over one materialized tree.
pub(crate) fn analyze_tree(dir: &Path, files: &[String], entities: &[sem_core::model::entity::SemanticEntity], models: &Models, limits: dataflow::Limits, scope: Option<&std::collections::HashSet<String>>) -> Analysis {
    let specs = super::topology::spec_targets(&dir.to_string_lossy(), scope);
    let resolve = move |from: &str, spec: &str| specs.get(&(from.to_string(), spec.to_string())).cloned();
    dataflow::analyze_until(dir, files, entities, &resolve, models, limits)
}

fn is_test_file(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    let leaf = l.rsplit('/').next().unwrap_or(&l);
    l.contains("/test/") || l.contains("/tests/") || l.contains("__tests__") || l.starts_with("test/") || l.starts_with("tests/")
        || l.contains("/testdata/") || l.contains("/fixtures/") || l.contains(".test.") || l.contains(".spec.")
        || leaf.starts_with("test_") || leaf.ends_with("_test.py") || leaf.ends_with("_test.go") || leaf == "conftest.py"
}

/// Example programs and benchmarks: they depend on the library, not the
/// other way round, and are not shipped with it.
fn is_example_or_bench(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.split('/').rev().skip(1).any(|d| matches!(d, "examples" | "example" | "benches" | "bench" | "benchmarks"))
}

/// Code outside the shipped architecture: tests, examples, benchmarks
/// (all of it counts with `include`).
fn non_prod(p: &str, include: bool) -> bool {
    !include && (is_test_file(p) || is_example_or_bench(p))
}

/// The Cargo crate a Rust file belongs to: the nearest directory above it
/// holding a `Cargo.toml` (`None` for other files).
fn crate_of(dir: &Path, file: &str) -> Option<String> {
    if !file.ends_with(".rs") {
        return None;
    }
    let mut d = Path::new(file).parent();
    while let Some(x) = d {
        if dir.join(x).join("Cargo.toml").is_file() {
            return Some(x.to_string_lossy().to_string());
        }
        d = x.parent();
    }
    Some(String::new())
}

fn package_of(file: &str) -> String {
    match file.rsplit_once('/') {
        Some((d, _)) => d.to_string(),
        None => ".".to_string(),
    }
}

/// File- and package-level dependency graph of production code: resolved
/// entity references between files, plus JS/TS module imports.
struct Deps {
    files: Vec<String>,
    edges: BTreeSet<(String, String)>,
    pkg_edges: BTreeMap<(String, String), (String, String)>,
    /// Rust file -> its crate. Cargo forbids dependency cycles between
    /// crates, so a cycle crossing crates is an artifact (a dev-dependency,
    /// a mis-resolved name): cycles are computed within a crate only.
    krate: HashMap<String, String>,
}

impl Deps {
    /// An edge that can take part in a cycle: not between two crates.
    fn cyclic(&self, a: &str, b: &str) -> bool {
        match (self.krate.get(a), self.krate.get(b)) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        }
    }
}

/// File-level import edges for Python / Go / Rust, from each file's
/// imports: a Python dotted module, a Go import path under the repo's
/// module path, a Rust `crate::` path — resolved to repo files by path
/// only when the answer is unambiguous; otherwise no edge.
fn import_edges(dir: &Path, a: &Analysis, all_files: &[String]) -> Vec<(String, String)> {
    // targets: every file of the tree (a diff-scoped analysis reads fewer)
    let files: BTreeSet<&str> = all_files.iter().map(String::as_str).chain(a.files.iter().map(|f| f.path.as_str())).collect();
    let mut by_dir: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in &files {
        if f.ends_with(".go") {
            by_dir.entry(f.rsplit_once('/').map_or("", |x| x.0)).or_default().push(f);
        }
    }
    // go.mod module paths, by the directory holding go.mod
    let mut gomods: Vec<(String, String)> = Vec::new();
    for e in ignore::WalkBuilder::new(dir).build().flatten() {
        if e.file_name() == "go.mod" {
            if let Ok(t) = std::fs::read_to_string(e.path()) {
                if let Some(m) = t.lines().find_map(|l| l.trim().strip_prefix("module ")) {
                    let rel = e.path().parent().and_then(|p| p.strip_prefix(dir).ok()).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                    gomods.push((m.trim().to_string(), rel));
                }
            }
        }
    }
    // Python files by dotted module name, from every import root: the repo
    // root, or a directory that is not itself a package (no __init__.py)
    let inits: BTreeSet<&str> = files.iter().filter(|f| f.ends_with("__init__.py")).map(|f| f.rsplit_once('/').map_or("", |x| x.0)).collect();
    let mut py_mod: HashMap<String, Vec<&str>> = HashMap::new();
    for f in files.iter().filter(|f| f.ends_with(".py")) {
        let m = f.trim_end_matches(".py").trim_end_matches("/__init__");
        let parts: Vec<&str> = m.split('/').collect();
        for i in 0..parts.len() {
            let root = parts[..i].join("/");
            if i > 0 && inits.contains(root.as_str()) {
                continue; // inside a package: not an import root
            }
            py_mod.entry(parts[i..].join(".")).or_default().push(f);
        }
    }
    let mut out = Vec::new();
    for f in &a.files {
        for imp in &f.imports {
            if imp.spec.is_some() {
                continue; // JS/TS: module graph from topology
            }
            let targets: Vec<&str> = if f.path.ends_with(".py") {
                // `a.b.c`: module a.b.c, or member c of module a.b
                let p = imp.path.trim_start_matches('.');
                let mut t = py_mod.get(p).cloned().unwrap_or_default();
                if t.is_empty() {
                    if let Some((m, _)) = p.rsplit_once('.') {
                        t = py_mod.get(m).cloned().unwrap_or_default();
                    }
                }
                if t.len() == 1 { t } else { Vec::new() }
            } else if f.path.ends_with(".go") {
                gomods
                    .iter()
                    .filter_map(|(m, root)| {
                        let sub = imp.path.strip_prefix(m.as_str())?.trim_start_matches('/');
                        let d = [root.as_str(), sub].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join("/");
                        by_dir.get(d.as_str()).cloned()
                    })
                    .next()
                    .unwrap_or_default()
            } else if f.path.ends_with(".rs") {
                let Some(rest) = imp.path.strip_prefix("crate::") else { continue };
                let Some(src) = f.path.find("src/").map(|i| &f.path[..i + 4]) else { continue };
                let segs: Vec<&str> = rest.split("::").collect();
                let mut hit = Vec::new();
                for n in (1..=segs.len()).rev() {
                    let base = format!("{src}{}", segs[..n].join("/"));
                    for c in [format!("{base}.rs"), format!("{base}/mod.rs")] {
                        if let Some(x) = files.get(c.as_str()) {
                            hit.push(*x);
                        }
                    }
                    if !hit.is_empty() {
                        break;
                    }
                }
                hit
            } else {
                Vec::new()
            };
            for t in targets {
                if t != f.path {
                    out.push((f.path.clone(), t.to_string()));
                }
            }
        }
    }
    out
}

impl Deps {
    fn build(t: &Tree, a: &Analysis, include: bool) -> Deps {
        let g: &EntityGraph = &t.graph;
        let mut edges = BTreeSet::new();
        // the same production files the data flow read, all of them when diff-scoped
        let prod: Vec<String> = t.all_files.iter().filter(|f| !non_prod(f, include)).cloned().collect();
        for (x, y) in import_edges(t.dir.path(), a, &prod) {
            if !non_prod(&x, include) && !non_prod(&y, include) {
                edges.insert((x, y));
            }
        }
        // JS/TS entity edges come from a name-matching resolver: their file
        // dependencies are taken from resolved imports (topology) instead
        // Go: package imports are the dependency relation (the compiler
        // forbids their cycles); entity edges include interface dispatch
        let js = |p: &str| matches!(dataflow::ir::Lang::for_path(p), Some(dataflow::ir::Lang::Ts | dataflow::ir::Lang::Go));
        for e in &g.edges {
            let (Some(a), Some(b)) = (g.entities.get(e.from_entity.as_str()), g.entities.get(e.to_entity.as_str())) else { continue };
            if js(&a.file_path) || js(&b.file_path) {
                continue;
            }
            if a.file_path != b.file_path && !non_prod(&a.file_path, include) && !non_prod(&b.file_path, include) {
                edges.insert((a.file_path.clone(), b.file_path.clone()));
            }
        }
        let (nodes, adj) = super::topology::module_value_graph(&t.dir.path().to_string_lossy(), t.scope.as_ref());
        // repo files only: topology also has nodes for npm packages
        let in_repo = |p: &str| t.dir.path().join(p).is_file();
        for (u, vs) in adj.iter().enumerate() {
            for &v in vs {
                if !non_prod(&nodes[u], include) && !non_prod(&nodes[v], include) && in_repo(&nodes[u]) && in_repo(&nodes[v]) {
                    edges.insert((nodes[u].clone(), nodes[v].clone()));
                }
            }
        }
        // a Go package is its directory: references between its files are
        // not dependencies (and cycles among them mean nothing)
        edges.retain(|(a, b)| !(a.ends_with(".go") && b.ends_with(".go") && package_of(a) == package_of(b)));
        let mut files: BTreeSet<String> = t.files.iter().filter(|f| !non_prod(f, include)).cloned().collect();
        for (a, b) in &edges {
            files.insert(a.clone());
            files.insert(b.clone());
        }
        let mut pkg_edges = BTreeMap::new();
        for (a, b) in &edges {
            let (pa, pb) = (package_of(a), package_of(b));
            if pa != pb {
                pkg_edges.entry((pa, pb)).or_insert((a.clone(), b.clone()));
            }
        }
        let krate = files.iter().filter_map(|f| Some((f.clone(), crate_of(t.dir.path(), f)?))).collect();
        Deps { files: files.into_iter().collect(), edges, pkg_edges, krate }
    }

    fn adj(nodes: &[String], edges: impl Iterator<Item = (String, String)>) -> algo::Adj {
        let idx: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
        let mut adj = vec![Vec::new(); nodes.len()];
        for (a, b) in edges {
            if let (Some(&u), Some(&v)) = (idx.get(a.as_str()), idx.get(b.as_str())) {
                adj[u].push(v);
            }
        }
        for vs in &mut adj {
            vs.sort_unstable();
            vs.dedup();
        }
        adj
    }

    fn packages(&self) -> Vec<String> {
        let s: BTreeSet<String> = self.files.iter().map(|f| package_of(f)).collect();
        s.into_iter().collect()
    }

    /// Non-trivial strongly connected components, as sorted member lists.
    fn cycles(nodes: &[String], adj: &algo::Adj) -> BTreeSet<Vec<String>> {
        let (_, comps) = algo::scc(adj);
        comps
            .into_iter()
            .filter(|c| c.len() > 1)
            .map(|c| {
                let mut m: Vec<String> = c.into_iter().map(|i| nodes[i].clone()).collect();
                m.sort();
                m
            })
            .collect()
    }

    fn propagation_cost(adj: &algo::Adj) -> f64 {
        let n = adj.len();
        if n < 2 {
            return 0.0;
        }
        algo::reach_counts(adj).iter().sum::<usize>() as f64 / (n as f64 * (n as f64 - 1.0))
    }
}

struct Measures {
    file_cycles: BTreeSet<Vec<String>>,
    pkg_cycles: BTreeSet<Vec<String>>,
    file_pc: f64,
    pkg_pc: f64,
    centrality: BTreeMap<String, f64>,
}

fn measure(d: &Deps) -> Measures {
    let fadj = Deps::adj(&d.files, d.edges.iter().cloned());
    let pkgs = d.packages();
    let padj = Deps::adj(&pkgs, d.pkg_edges.keys().cloned());
    // cycles: within a Rust crate only (see `Deps::krate`)
    let fcyc = Deps::adj(&d.files, d.edges.iter().filter(|(a, b)| d.cyclic(a, b)).cloned());
    let pcyc = Deps::adj(&pkgs, d.pkg_edges.iter().filter(|(_, (fa, fb))| d.cyclic(fa, fb)).map(|(k, _)| k.clone()));
    let btw = if pkgs.len() <= 4000 { algo::betweenness(&padj) } else { vec![0.0; pkgs.len()] };
    let n = pkgs.len() as f64;
    let norm = if n > 2.0 { (n - 1.0) * (n - 2.0) } else { 1.0 };
    Measures {
        file_cycles: Deps::cycles(&d.files, &fcyc),
        pkg_cycles: Deps::cycles(&pkgs, &pcyc),
        file_pc: Deps::propagation_cost(&fadj),
        pkg_pc: Deps::propagation_cost(&padj),
        centrality: pkgs.iter().cloned().zip(btw.into_iter().map(|b| b / norm)).collect(),
    }
}

const UNTRUSTED: &[&str] = &["http-input", "tool-input", "net-input", "cli-input", "db-read", "file-read"];
const DANGEROUS: &[&str] = &["exec", "db", "template", "http-response", "file-write", "file-path"];
const EFFECTS: &[&str] = &["exec", "db", "net-send", "file-write", "file-path", "template", "http-response"];
/// Input a remote party controls: a request, a tool call, a peer's reply.
const REMOTE: &[&str] = &["http-input", "tool-input", "net-input"];

fn flow_severity(src: &str, sink: &str) -> Severity {
    match (src, sink) {
        (s, k) if REMOTE.contains(&s) && DANGEROUS.contains(&k) => Severity::High,
        // request-controlled data in an outgoing request (SSRF-shaped;
        // also what every proxy does, so not ranked high)
        (s, "net-send") if ["http-input", "tool-input"].contains(&s) => Severity::Medium,
        (s, k) if UNTRUSTED.contains(&s) && DANGEROUS.contains(&k) => Severity::Medium,
        ("env", k) if ["log", "net-send", "http-response", "template"].contains(&k) => Severity::Medium,
        _ => Severity::Low,
    }
}

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn s(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn keyed(v: &Value, list: &str) -> BTreeMap<String, Value> {
    arr(&v[list]).into_iter().map(|f| (s(&f["key"]), f)).collect()
}

fn flow_line(f: &Value) -> String {
    format!(
        "{} {}:{} `{}` -> {} {}:{} `{}` (via {})",
        s(&f["source"]["class"]),
        s(&f["source"]["file"]),
        f["source"]["line"],
        s(&f["source"]["entity"]),
        s(&f["sink"]["class"]),
        s(&f["sink"]["file"]),
        f["sink"]["line"],
        s(&f["sink"]["entity"]),
        s(&f["sink"]["via"])
    )
}

/// A phase boundary: its time, and with `SEM_TIMINGS` set, the process's
/// resident memory at that point (stderr).
fn mark(t: &mut crate::timings::Timings, name: &'static str) {
    t.mark(name);
    if t.is_enabled() {
        let mb = |b: Option<usize>| b.map(|b| (b / (1024 * 1024)).to_string()).unwrap_or_else(|| "?".into());
        let (rss, peak) = (sem_core::parser::mem_profile::current_rss_bytes(), sem_core::parser::mem_profile::peak_rss_bytes());
        eprintln!("phase {name} done; rss {} MB; peak so far {} MB", mb(rss), mb(peak));
    }
}

/// The data-flow deadline inside a whole-command budget: propagation gets
/// half of what is left after the trees are built, the engine's final pass
/// up to a quarter of that again (at least 1 s), and the rest is held for
/// the phases after it (escape resolution, JSON, dependencies, compose), so
/// `--budget` bounds the command's wall time, not one phase of it.
pub(crate) fn dataflow_deadline(now: std::time::Instant, end: std::time::Instant) -> std::time::Instant {
    now + end.saturating_duration_since(now).mul_f64(0.5)
}

pub fn arch_diff_command(opts: ArchDiffOptions) -> Result<(), Box<dyn std::error::Error>> {
    let t0 = std::time::Instant::now();
    let end = opts.budget.map(|b| t0 + b);
    let root = super::repo_root_or_cwd(&opts.cwd);
    let (base, head) = certify::resolve_range(&root, &opts.range)?;
    let mut timings = crate::timings::Timings::from_env("arch-diff");
    let (bt, ht, region) = certify::build_trees_in(&root, &base, &head, opts.scope)?;
    mark(&mut timings, "build_trees");
    let cert = certify::certificate(&root, &base, &head, &bt, &ht, &opts.laws)?;
    mark(&mut timings, "certificate");
    let models = load_models(&[ht.dir.path()], &opts.models)?;
    // data flow of production code only: test code feeding shared state
    // would otherwise manufacture paths no deployment has
    let include = opts.include_examples;
    let prod = |files: &[String]| -> Vec<String> { files.iter().filter(|f| !non_prod(f, include)).cloned().collect() };
    let (bprod, hprod) = (prod(&bt.files), prod(&ht.files));
    let limits = dataflow::Limits { deadline: end.map(|e| dataflow_deadline(std::time::Instant::now(), e)), max_rss_bytes: opts.max_memory };
    let (ba, ha) = std::thread::scope(|sc| {
        let b = sc.spawn(|| analyze_tree(bt.dir.path(), &bprod, &bt.entities, &models, limits, bt.scope.as_ref()));
        let h = sc.spawn(|| analyze_tree(ht.dir.path(), &hprod, &ht.entities, &models, limits, ht.scope.as_ref()));
        (b.join().expect("base dataflow"), h.join().expect("head dataflow"))
    });
    mark(&mut timings, "dataflow");
    // compact: escapes compared by key hash; only the new ones rendered
    let (bj, mut hj) = (ba.to_json_with(dataflow::JsonDetail::Compact), ha.to_json_with(dataflow::JsonDetail::Compact));
    hj["escapes"] = json!(ha.escapes_not_in(&ba.escape_keys()));
    mark(&mut timings, "dataflow_json");
    let (bd, hd) = (Deps::build(&bt, &ba, include), Deps::build(&ht, &ha, include));
    drop((ba, ha));
    mark(&mut timings, "deps");
    let (bm, hm) = (measure(&bd), measure(&hd));
    mark(&mut timings, "measure");
    let report = compose(&cert, &bj, &hj, &bd, &hd, &bm, &hm, &bt, &ht, region.as_ref(), opts.max_items, t0, end);
    mark(&mut timings, "compose");
    timings.finish();
    match opts.format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        Format::Markdown => print!("{}", render_md(&report, opts.max_items)),
        Format::Text => print!("{}", render_text(&report, opts.max_items)),
    }
    Ok(())
}

fn schema_files(dir: &Path, files: &[String]) -> BTreeSet<String> {
    let _ = files;
    let mut out = BTreeSet::new();
    let walker = ignore::WalkBuilder::new(dir).hidden(false).build();
    for e in walker.flatten() {
        let p = e.path();
        let Ok(rel) = p.strip_prefix(dir) else { continue };
        let rel = rel.to_string_lossy().to_string();
        let leaf = rel.rsplit('/').next().unwrap_or(&rel).to_ascii_lowercase();
        let schema = leaf.ends_with(".proto")
            || leaf.ends_with(".avsc")
            || leaf.ends_with(".graphql")
            || leaf.ends_with(".gql")
            || ((leaf.starts_with("openapi") || leaf.starts_with("swagger") || leaf.starts_with("asyncapi"))
                && (leaf.ends_with(".yaml") || leaf.ends_with(".yml") || leaf.ends_with(".json")));
        if schema && !rel.contains("node_modules/") {
            out.insert(rel);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn compose(
    cert: &Value,
    bj: &Value,
    hj: &Value,
    bd: &Deps,
    hd: &Deps,
    bm: &Measures,
    hm: &Measures,
    bt: &Tree,
    ht: &Tree,
    region: Option<&super::region::Region>,
    max: usize,
    t0: std::time::Instant,
    end: Option<std::time::Instant>,
) -> Value {
    let late = || end.is_some_and(|e| std::time::Instant::now() >= e);
    let mut skipped: Vec<&str> = Vec::new();
    let mut findings: Vec<Finding> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();
    // diff-scoped: repo-wide facts hold within the analyzed region only
    let within = if region.is_some() { " (within the analyzed region)" } else { "" };

    // -- data paths ------------------------------------------------------------
    // A tree whose data-flow analysis hit the time budget has partial
    // facts: absence proves nothing, and a "new" path may exist at base.
    let partial = bj["incomplete"] == true || hj["incomplete"] == true;
    let (bf, hf) = (keyed(bj, "flows"), keyed(hj, "flows"));
    let new_flows: Vec<&Value> = hf.iter().filter(|(k, _)| !bf.contains_key(*k)).map(|(_, v)| v).collect();
    let gone_flows: Vec<&Value> = bf.iter().filter(|(k, _)| !hf.contains_key(*k)).map(|(_, v)| v).collect();
    for f in &new_flows {
        if f["selfPath"] == true {
            findings.push(Finding {
                severity: Severity::Info,
                kind: "self-data-path",
                title: format!("intra-call self path (the call's own result reaching its own argument; an artifact of flow-insensitivity): {}", flow_line(f)),
                details: Vec::new(),
                data: (*f).clone(),
            });
            continue;
        }
        let mut sev = flow_severity(&s(&f["source"]["class"]), &s(&f["sink"]["class"]));
        let mut details: Vec<String> = arr(&f["path"]).iter().map(s).collect();
        let object_state = f["throughState"].as_str().is_some_and(|st| st.ends_with(".*"));
        if let Some(st) = f["throughState"].as_str() {
            details.push(format!("through shared state {st}"));
        }
        if object_state {
            // allocation-insensitive: any instance's fields stand for all
            sev = sev.max(Severity::Low);
        }
        findings.push(Finding {
            severity: sev,
            kind: "new-data-path",
            title: format!(
                "new data path{}{}: {}",
                if object_state { " (via object state; may conflate instances)" } else { "" },
                if partial { " (partial analysis: may exist at base too)" } else { "" },
                flow_line(f)
            ),
            details,
            data: (*f).clone(),
        });
    }
    for f in gone_flows.iter().filter(|_| !partial) {
        findings.push(Finding { severity: Severity::Info, kind: "removed-data-path", title: format!("removed data path: {}", flow_line(f)), details: Vec::new(), data: (*f).clone() });
    }
    if new_flows.is_empty() && gone_flows.is_empty() && !partial {
        unchanged.push(format!("No source->sink data path was added or removed{within} ({} path(s) at both base and head, within the modeled sources/sinks).", hf.len()));
    }
    // possible (name-only) paths and unknown boundaries
    let (bp, hp) = (keyed(bj, "possibleFlows"), keyed(hj, "possibleFlows"));
    let new_possible: Vec<&Value> = hp.iter().filter(|(k, _)| !bp.contains_key(*k)).map(|(_, v)| v).collect();
    for f in &new_possible {
        findings.push(Finding {
            severity: Severity::Low,
            kind: "new-possible-path",
            title: format!("possible new data path (receiver type unknown, method name matches a sink): {}", flow_line(f)),
            details: arr(&f["path"]).iter().map(s).collect(),
            data: (*f).clone(),
        });
    }
    let (be, he) = (keyed(bj, "escapes"), keyed(hj, "escapes"));
    let new_esc: Vec<&Value> = he.iter().filter(|(k, _)| !be.contains_key(*k)).map(|(_, v)| v).collect();
    for e in &new_esc {
        let class = s(&e["source"]["class"]);
        // a "may" fact about unresolved code: worth listing, not ranking high
        let _ = class.as_str();
        let sev = Severity::Low;
        findings.push(Finding {
            severity: sev,
            kind: "new-unknown-path",
            title: format!(
                "{} data from {}:{} `{}` reaches code sem cannot resolve at {}:{} `{}` ({}) — may reach any sink",
                class,
                s(&e["source"]["file"]),
                e["source"]["line"],
                s(&e["source"]["entity"]),
                s(&e["unknown"]["file"]),
                e["unknown"]["line"],
                s(&e["unknown"]["entity"]),
                s(&e["unknown"]["why"])
            ),
            details: arr(&e["path"]).iter().map(s).collect(),
            data: (*e).clone(),
        });
    }
    let (bu, hu) = (keyed(bj, "unknownFlows"), keyed(hj, "unknownFlows"));
    let new_uflows = hu.keys().filter(|k| !bu.contains_key(*k)).count();

    // -- side effects per entity -----------------------------------------------
    let ents = |j: &Value| -> BTreeMap<String, Value> { arr(&j["entities"]).into_iter().map(|e| (s(&e["key"]), e)).collect() };
    let (bents, hents) = (ents(bj), ents(hj));
    let set_of = |e: &Value, k: &str| -> BTreeSet<String> { arr(&e[k]).iter().map(s).collect() };
    let mut effect_changes = 0;
    let mut complexity: Vec<Value> = Vec::new();
    for (k, he_) in &hents {
        // tests are not architecture: their effects and complexity are not findings
        if non_prod(&s(&he_["file"]), false) {
            continue;
        }
        match bents.get(k) {
            Some(be_) => {
                for dir in ["reads", "writes"] {
                    let (b, h) = (set_of(be_, dir), set_of(he_, dir));
                    let added: Vec<&String> = h.difference(&b).collect();
                    let removed: Vec<&String> = b.difference(&h).collect();
                    if added.is_empty() && removed.is_empty() {
                        continue;
                    }
                    effect_changes += 1;
                    let sev = if added.iter().any(|a| EFFECTS.contains(&a.as_str())) {
                        Severity::Medium
                    } else if added.is_empty() {
                        Severity::Info
                    } else {
                        Severity::Low
                    };
                    let mut parts = Vec::new();
                    if !added.is_empty() {
                        parts.push(format!("now {dir} {}", added.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")));
                    }
                    if !removed.is_empty() {
                        parts.push(format!("no longer {dir} {}", removed.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")));
                    }
                    findings.push(Finding {
                        severity: sev,
                        kind: "side-effect-change",
                        title: format!("`{}` ({}:{}) {}", s(&he_["name"]), s(&he_["file"]), he_["line"], parts.join("; ")),
                        details: Vec::new(),
                        data: json!({ "entity": k, "direction": dir, "added": added, "removed": removed }),
                    });
                }
                let (bc, hc) = (be_["cyclomatic"].as_i64().unwrap_or(0), he_["cyclomatic"].as_i64().unwrap_or(0));
                let (bg, hg) = (be_["cognitive"].as_i64().unwrap_or(0), he_["cognitive"].as_i64().unwrap_or(0));
                if (bc, bg) != (hc, hg) && !he_["module"].as_bool().unwrap_or(false) {
                    complexity.push(json!({ "entity": k, "name": he_["name"], "file": he_["file"], "line": he_["line"],
                        "cyclomatic": [bc, hc], "cognitive": [bg, hg], "delta": [hc - bc, hg - bg] }));
                }
            }
            None => {
                let w: Vec<String> = set_of(he_, "writes").into_iter().filter(|x| EFFECTS.contains(&x.as_str())).collect();
                if !w.is_empty() && !he_["module"].as_bool().unwrap_or(false) {
                    findings.push(Finding {
                        severity: Severity::Low,
                        kind: "new-entity-effects",
                        title: format!("new `{}` ({}:{}) performs {}", s(&he_["name"]), s(&he_["file"]), he_["line"], w.join(", ")),
                        details: Vec::new(),
                        data: json!({ "entity": k, "writes": w }),
                    });
                }
                let (hc, hg) = (he_["cyclomatic"].as_i64().unwrap_or(0), he_["cognitive"].as_i64().unwrap_or(0));
                if !he_["module"].as_bool().unwrap_or(false) {
                    complexity.push(json!({ "entity": k, "name": he_["name"], "file": he_["file"], "line": he_["line"],
                        "cyclomatic": [Value::Null, hc], "cognitive": [Value::Null, hg], "delta": [hc, hg], "new": true }));
                }
            }
        }
    }
    let common = hents.keys().filter(|k| bents.contains_key(*k)).count();
    if effect_changes == 0 && !partial {
        unchanged.push(format!("Side effects (env/file/DB/network/subprocess/log/template reads and writes, module state, fields) are unchanged for all {common} function(s) present in both trees{within}."));
    }
    complexity.sort_by_key(|c| -(c["delta"][0].as_i64().unwrap_or(0).max(0) * 2 + c["delta"][1].as_i64().unwrap_or(0).max(0)));
    for c in &complexity {
        let (dc, dg) = (c["delta"][0].as_i64().unwrap_or(0), c["delta"][1].as_i64().unwrap_or(0));
        let new = c["new"] == true;
        let sev = if new {
            if dc >= 15 || dg >= 25 { Severity::Medium } else if dc >= 8 || dg >= 12 { Severity::Low } else { continue }
        } else if dc >= 10 || dg >= 15 {
            Severity::Medium
        } else if dc >= 4 || dg >= 6 {
            Severity::Low
        } else {
            continue;
        };
        let title = if new {
            format!("new `{}` ({}:{}) has cyclomatic {} / cognitive {}", s(&c["name"]), s(&c["file"]), c["line"], dc, dg)
        } else {
            format!("`{}` ({}:{}) complexity cyclomatic {} -> {}, cognitive {} -> {}", s(&c["name"]), s(&c["file"]), c["line"], c["cyclomatic"][0], c["cyclomatic"][1], c["cognitive"][0], c["cognitive"][1])
        };
        findings.push(Finding { severity: sev, kind: "complexity", title, details: Vec::new(), data: c.clone() });
    }

    // -- dependencies, cycles, propagation cost, centrality ----------------------
    let new_pkg: Vec<(&(String, String), &(String, String))> = hd.pkg_edges.iter().filter(|(k, _)| !bd.pkg_edges.contains_key(*k)).collect();
    let gone_pkg: Vec<&(String, String)> = bd.pkg_edges.keys().filter(|k| !hd.pkg_edges.contains_key(*k)).collect();
    // Edges touching a package that is itself new are the new package's
    // wiring, not a decision between existing parts: one finding per new
    // package. An edge between two packages that both existed is ranked.
    // packages of every production file, not only the analyzed ones
    let mut base_pkgs: BTreeSet<String> = bd.packages().into_iter().collect();
    base_pkgs.extend(bt.all_files.iter().filter(|f| !non_prod(f, false)).map(|f| package_of(f)));
    // Diff-scoped: a package's dependencies are known only when all its
    // files were read; otherwise a "new" edge may exist through another file.
    let mut pkg_read: HashMap<String, bool> = HashMap::new();
    if let Some(r) = region {
        for f in bt.all_files.iter().chain(&ht.all_files).filter(|f| !non_prod(f, false)) {
            *pkg_read.entry(package_of(f)).or_insert(true) &= r.files.contains(f.as_str());
        }
    }
    let read_whole = |p: &str| region.is_none() || pkg_read.get(p).copied().unwrap_or(true);
    let mut wiring: BTreeMap<&String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for ((a, b), (fa, fb)) in &new_pkg {
        let (a_new, b_new) = (!base_pkgs.contains(a), !base_pkgs.contains(b));
        if a_new || b_new {
            if a_new {
                wiring.entry(a).or_default().1.push(b.clone());
            }
            if b_new {
                wiring.entry(b).or_default().0.push(a.clone());
            }
            continue;
        }
        let exact = read_whole(a);
        findings.push(Finding {
            severity: if exact { Severity::Medium } else { Severity::Low },
            kind: "new-package-dependency",
            title: format!(
                "new package dependency {a}/ -> {b}/ (e.g. {fa} -> {fb}){}",
                if exact { "" } else { " (partial: files of this package outside the analyzed region were not read, so it may already exist)" }
            ),
            details: Vec::new(),
            data: json!({ "from": a, "to": b, "witness": [fa, fb] }),
        });
    }
    for (p, (users, uses)) in &wiring {
        let list = |v: &Vec<String>| if v.is_empty() { "nothing".to_string() } else { v.iter().map(|x| format!("{x}/")).collect::<Vec<_>>().join(", ") };
        findings.push(Finding {
            severity: Severity::Low,
            kind: "new-package",
            title: format!("new package {p}/: depends on {}; depended on by {}", list(uses), list(users)),
            details: Vec::new(),
            data: json!({ "package": p, "dependsOn": uses, "dependedOnBy": users }),
        });
    }
    for (a, b) in &gone_pkg {
        findings.push(Finding { severity: Severity::Info, kind: "removed-package-dependency", title: format!("removed package dependency {a}/ -> {b}/"), details: Vec::new(), data: json!({ "from": a, "to": b }) });
    }
    if new_pkg.is_empty() && gone_pkg.is_empty() {
        unchanged.push(format!("No package (directory) dependency was added or removed{within} ({} package edge(s)).", hd.pkg_edges.len()));
    }
    let new_file_edges: Vec<&(String, String)> = hd.edges.difference(&bd.edges).collect();
    let gone_file_edges: Vec<&(String, String)> = bd.edges.difference(&hd.edges).collect();
    for (cyc, level, sev) in [(&hm.pkg_cycles, "package", Severity::High), (&hm.file_cycles, "file", Severity::Medium)] {
        let before = if level == "package" { &bm.pkg_cycles } else { &bm.file_cycles };
        let before_members: BTreeSet<&String> = before.iter().flatten().collect();
        for c in cyc.iter().filter(|c| !before.contains(*c)) {
            let newly: Vec<&String> = c.iter().filter(|m| !before_members.contains(m)).collect();
            let grown = newly.len() < c.len();
            // diff-scoped: a member package with unread files may close the
            // same cycle at base through them
            let exact = level == "file" || c.iter().all(|m| read_whole(m));
            findings.push(Finding {
                severity: if grown && level == "file" { Severity::Low } else if exact { sev } else { Severity::Medium },
                kind: "new-cycle",
                title: format!(
                    "{} {level} cycle of {}{within}: {}{}{}",
                    if grown { "changed" } else { "new" },
                    c.len(),
                    c.iter().take(6).cloned().collect::<Vec<_>>().join(", "),
                    if c.len() > 6 { ", …" } else { "" },
                    if exact { "" } else { " (partial: some member packages were not read in full, so it may exist at base too)" }
                ),
                details: if grown { vec![format!("newly in a cycle: {}", newly.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", "))] } else { Vec::new() },
                data: json!({ "level": level, "members": c, "newMembers": newly }),
            });
        }
        for c in before.iter().filter(|c| !cyc.contains(*c)) {
            findings.push(Finding { severity: Severity::Info, kind: "removed-cycle", title: format!("{level} cycle of {} no longer exists: {}", c.len(), c.iter().take(6).cloned().collect::<Vec<_>>().join(", ")), details: Vec::new(), data: json!({ "level": level, "members": c }) });
        }
    }
    if hm.pkg_cycles == bm.pkg_cycles && hm.file_cycles == bm.file_cycles {
        unchanged.push(format!("Cycles unchanged{within}: {} package-level and {} file-level strongly connected component(s), identical members.", hm.pkg_cycles.len(), hm.file_cycles.len()));
    }
    let pc_delta = (hm.file_pc - bm.file_pc) * 100.0;
    if pc_delta.abs() >= 0.05 {
        findings.push(Finding {
            severity: if pc_delta >= 2.0 { Severity::Medium } else { Severity::Info },
            kind: "propagation-cost",
            title: format!("propagation cost (file graph{within}) {:.1}% -> {:.1}% ({:+.1} pts); package graph {:.1}% -> {:.1}%", bm.file_pc * 100.0, hm.file_pc * 100.0, pc_delta, bm.pkg_pc * 100.0, hm.pkg_pc * 100.0),
            details: Vec::new(),
            data: json!({ "file": [bm.file_pc, hm.file_pc], "package": [bm.pkg_pc, hm.pkg_pc] }),
        });
    } else {
        unchanged.push(format!("Propagation cost unchanged{within}: {:.1}% of file pairs reachable ({:.1}% of package pairs).", hm.file_pc * 100.0, hm.pkg_pc * 100.0));
    }
    let mut cen: Vec<(String, f64, f64)> = hm
        .centrality
        .iter()
        .map(|(p, h)| (p.clone(), bm.centrality.get(p).copied().unwrap_or(0.0), *h))
        .filter(|(_, b, h)| (h - b).abs() >= 0.01)
        .collect();
    cen.sort_by(|a, b| (b.2 - b.1).abs().partial_cmp(&(a.2 - a.1).abs()).unwrap_or(std::cmp::Ordering::Equal));
    for (p, b, h) in cen.iter().take(max) {
        findings.push(Finding {
            severity: if h - b >= 0.1 { Severity::Low } else { Severity::Info },
            kind: "centrality",
            title: format!("package {p}/ betweenness centrality{within} {b:.3} -> {h:.3}"),
            details: Vec::new(),
            data: json!({ "package": p, "before": b, "after": h }),
        });
    }

    // -- signatures and laws (from the certificate) --------------------------------
    // callable code only: a stylesheet chunk has no callers to break
    let sigs: Vec<Value> = arr(&cert["signatureChanges"])
        .into_iter()
        .filter(|g| {
            let file = s(&g["file"]);
            let supported = dataflow::ir::Lang::for_path(&file).is_some() || [".java", ".kt", ".cs", ".rb", ".php", ".swift", ".c", ".cpp", ".h"].iter().any(|e| file.ends_with(e));
            supported && !matches!(g["type"].as_str(), Some("chunk" | "orphan"))
        })
        .collect();
    // Unresolved call sites by called name (receiver type unknown): each
    // may be a caller the static caller list cannot show.
    let edited: BTreeSet<String> = arr(&cert["entities"]).iter().map(|e| s(&e["file"])).collect();
    let mut unresolved_by_name: HashMap<String, Vec<(String, i64, String)>> = HashMap::new();
    for e in arr(&hj["entities"]) {
        for u in arr(&e["unknownCallees"]) {
            let name = s(&u[1]);
            unresolved_by_name.entry(name).or_default().push((s(&e["file"]), u[0].as_i64().unwrap_or(0), s(&e["name"])));
        }
    }
    for g in &sigs {
        let callers = arr(&g["callers"]);
        let stale: Vec<&Value> = callers.iter().filter(|c| c["touchedByChange"] != true).collect();
        let stale_old = arr(&g["staleCallersOfOldName"]);
        let shape = param_shape_change(&s(&g["before"]), &s(&g["after"]));
        let breaking = !matches!(shape, "annotations-only" | "compatible-extension");
        // unresolved calls of the same name, outside the definition and the
        // static callers already listed
        let known: BTreeSet<(String, String)> = callers.iter().map(|c| (s(&c["file"]), s(&c["entity"]))).collect();
        let entity = s(&g["entity"]);
        let unknown: Vec<&(String, i64, String)> = unresolved_by_name
            .get(&entity)
            .into_iter()
            .flatten()
            .filter(|(f, _, e)| !known.contains(&(f.clone(), e.rsplit('.').next().unwrap_or(e).to_string())) && !known.contains(&(f.clone(), e.clone())))
            .collect();
        // one per file first, so the listed few show how far the change reaches
        let mut unknown_untouched: Vec<&&(String, i64, String)> = unknown.iter().filter(|(f, _, _)| !edited.contains(f)).collect();
        {
            let mut seen = BTreeSet::new();
            let (first, rest): (Vec<_>, Vec<_>) = unknown_untouched.iter().partition(|(f, _, _)| seen.insert(f.clone()));
            unknown_untouched = first.into_iter().chain(rest).collect();
        }
        let sev = if breaking && (!stale.is_empty() || !stale_old.is_empty()) {
            Severity::High
        } else if breaking {
            Severity::Medium
        } else {
            Severity::Low
        };
        let mut details = vec![format!("before: {}", s(&g["before"])), format!("after:  {}", s(&g["after"])), format!("parameter change: {shape}")];
        for c in stale.iter().take(max) {
            details.push(format!("caller not modified by this change: {}:{} `{}`", s(&c["file"]), c["line"], s(&c["entity"])));
        }
        if breaking {
            for (f, line, e) in unknown_untouched.iter().take(max) {
                details.push(format!("possible caller (receiver type unknown), not modified by this change: {f}:{line} `{e}` calls `.{entity}()`"));
            }
        }
        let mut unknown_note = if unknown.is_empty() {
            String::new()
        } else {
            format!("; {} unresolved call(s) named `{entity}()` may also be callers ({} not modified by this change)", unknown.len(), unknown_untouched.len())
        };
        // diff-scoped, and the name was too widely mentioned to search: its
        // callers outside the region are unknown, not absent
        let unsearched = region.and_then(|r| r.unexplored_callers(entity.rsplit(['.', ':']).next().unwrap_or(&entity)));
        if let Some(n) = unsearched {
            unknown_note += &format!("; callers not searched: {n} file(s) outside the analyzed region mention `{entity}`");
            details.push(format!("callers outside the analyzed region are unknown: {n} file(s) mention `{entity}` (diff-scoped analysis; `--scope full` or a larger `--region-mb` searches them)"));
        }
        findings.push(Finding {
            severity: sev,
            kind: "signature-change",
            title: format!(
                "public contract of `{}` ({}:{}) changed; {} static caller(s), {} not modified by this change{unknown_note}",
                entity,
                s(&g["file"]),
                g["line"],
                callers.len(),
                stale.len()
            ),
            details,
            data: {
                let mut d = g.clone();
                d["callersNotSearched"] = json!(unsearched);
                d["unresolvedCallsSameName"] = json!(unknown.iter().map(|(f, l, e)| json!({ "file": f, "line": l, "entity": e, "touchedByChange": edited.contains(f) })).collect::<Vec<_>>());
                d
            },
        });
    }
    if sigs.is_empty() {
        unchanged.push("No function signature changed.".to_string());
    }
    for l in arr(&cert["laws"]["results"]) {
        if l["status"] == "newly-broken" {
            let nv = arr(&l["newViolations"]);
            findings.push(Finding {
                severity: Severity::High,
                kind: "law-broken",
                title: format!("law `{}` newly broken ({} new violation(s)){}", s(&l["id"]), nv.len(), l["promise"].as_str().map(|p| format!(" — {p}")).unwrap_or_default()),
                details: nv.iter().take(3).map(|d| d["path"].as_array().map(|p| p.iter().map(s).collect::<Vec<_>>().join(" -> ")).unwrap_or_else(|| d.to_string())).collect(),
                data: l.clone(),
            });
        }
    }

    // -- unknowns and service boundaries -------------------------------------------
    let (bc, hc) = (&bj["coverage"], &hj["coverage"]);
    let (br, hr) = (bc["unknownRate"].as_f64().unwrap_or(0.0), hc["unknownRate"].as_f64().unwrap_or(0.0));
    if (hr - br) * 100.0 >= 5.0 {
        findings.push(Finding { severity: Severity::Low, kind: "unknown-coverage", title: format!("unresolved call rate {:.1}% -> {:.1}%", br * 100.0, hr * 100.0), details: Vec::new(), data: json!({ "before": br, "after": hr }) });
    }
    let (bs, hs) = if late() {
        skipped.push("service schemas");
        (BTreeSet::new(), BTreeSet::new())
    } else {
        (schema_files(bt.dir.path(), &bt.files), schema_files(ht.dir.path(), &ht.files))
    };
    let changed_files: BTreeSet<String> = arr(&cert["entities"]).iter().map(|e| s(&e["file"])).collect();
    let touched_schema: Vec<&String> = hs.iter().filter(|f| !bs.contains(*f) || changed_files.contains(*f)).collect();
    // imports of repo modules that resolve to no file: JS/TS relative
    // imports, Python relative and repo-absolute imports. Python: when no
    // module file was added or removed, an unchanged file's imports resolve
    // the same in both trees, so only the changed files are parsed (the
    // whole-tree parse, twice, serially, was 3-14 s of sympy's compose).
    let py_check = python_files_to_check(bt, ht);
    let broken = |t: &Tree| -> BTreeSet<(String, String)> {
        let mut b = super::topology::broken_relative_imports(&t.dir.path().to_string_lossy(), t.scope.as_ref());
        let check: Option<std::collections::HashSet<String>> = match (&py_check, t.scope.as_ref()) {
            (Some(c), Some(sc)) => Some(c.intersection(sc).cloned().collect()),
            (Some(c), None) => Some(c.clone()),
            (None, sc) => sc.cloned(),
        };
        b.extend(sem_core::topology::pyimports::broken_imports_in(t.dir.path(), &t.all_files, check.as_ref()));
        b
    };
    let (bb, hb) = if late() {
        skipped.push("broken imports");
        (BTreeSet::new(), BTreeSet::new())
    } else {
        (broken(bt), broken(ht))
    };
    for (file, spec) in hb.difference(&bb) {
        // generated at build time, often gitignored (protobuf, version stamps)
        let leaf = spec.rsplit(['.', '/']).next().unwrap_or(spec);
        let generated = leaf.ends_with("_pb2") || leaf.ends_with("_pb2_grpc") || leaf.ends_with("_pb") || matches!(leaf, "_version" | "version" | "__version__");
        findings.push(Finding {
            severity: if is_test_file(file) || generated { Severity::Medium } else { Severity::High },
            kind: "broken-import",
            title: format!("{file} imports `{spec}`, which resolves to no file{}", if generated { " (it may be generated at build time)" } else { "" }),
            details: Vec::new(),
            data: json!({ "file": file, "specifier": spec }),
        });
    }
    if hb.difference(&bb).next().is_none() && !skipped.contains(&"broken imports") {
        let at_head = if py_check.is_some() { format!("{} in the changed files at head", hb.len()) } else { format!("{} at head", hb.len()) };
        unchanged.push(format!("No new import of a repo module that resolves to no file{within} (JS/TS relative, Python relative and repo-absolute; {at_head})."));
    }
    for f in &touched_schema {
        findings.push(Finding { severity: Severity::Medium, kind: "service-boundary", title: format!("service schema added or changed: {f} (its consumers are not modeled as edges)"), details: Vec::new(), data: json!({ "file": f }) });
    }

    // within a severity, what a reviewer must act on first (the markdown
    // lists a bounded number per severity)
    let rank = |k: &str| {
        ["law-broken", "new-data-path", "broken-import", "signature-change", "new-cycle", "new-package-dependency", "new-possible-path", "side-effect-change", "service-boundary", "new-package", "new-entity-effects", "new-unknown-path"]
            .iter()
            .position(|x| *x == k)
            .unwrap_or(99)
    };
    findings.sort_by(|a, b| a.severity.cmp(&b.severity).then(rank(a.kind).cmp(&rank(b.kind))).then(a.kind.cmp(b.kind)).then(a.title.cmp(&b.title)));
    let count = |sev: Severity| findings.iter().filter(|f| f.severity == sev).count();
    json!({
        "base": cert["base"], "head": cert["head"],
        "summary": cert["summary"],
        "severity": { "high": count(Severity::High), "medium": count(Severity::Medium), "low": count(Severity::Low), "info": count(Severity::Info) },
        "findings": findings.iter().map(|f| json!({ "severity": f.severity.as_str(), "kind": f.kind, "title": f.title, "why": why(f), "details": f.details, "data": f.data })).collect::<Vec<_>>(),
        "unchanged": unchanged,
        "dataPaths": { "base": bf.len(), "head": hf.len(), "added": new_flows.len(), "removed": gone_flows.len(),
            "possibleHead": hp.len(), "unknownPathsHead": hu.len(), "newUnknownPaths": new_uflows,
            "escapesHead": hj["escapeCount"].as_u64().unwrap_or(he.len() as u64), "newEscapes": new_esc.len() },
        "dependencies": { "fileEdges": [bd.edges.len(), hd.edges.len()], "fileEdgesAdded": new_file_edges.len(), "fileEdgesRemoved": gone_file_edges.len(),
            "fileEdgesAddedList": new_file_edges.iter().take(200).map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "packageEdges": [bd.pkg_edges.len(), hd.pkg_edges.len()],
            "packageCycles": [bm.pkg_cycles.len(), hm.pkg_cycles.len()], "fileCycles": [bm.file_cycles.len(), hm.file_cycles.len()],
            "packageCyclesAtHead": hm.pkg_cycles.iter().take(20).collect::<Vec<_>>(),
            "fileCyclesAtHead": hm.file_cycles.iter().take(20).collect::<Vec<_>>(),
            "propagationCost": { "file": [bm.file_pc, hm.file_pc], "package": [bm.pkg_pc, hm.pkg_pc] },
            "derivedFrom": "resolved references between entities (calls, type references, imports) plus JS/TS module imports; tests, examples and benches excluded unless --include-examples; cycles within a Rust crate only (Cargo forbids crate cycles)" },
        "complexity": complexity,
        "coverage": { "base": bc, "head": hc },
        "serviceBoundaries": { "schemaFiles": hs, "note": if hs.is_empty() { "no OpenAPI/protobuf/queue schema files found; any service boundary is an unmodeled unknown" } else { "schema files present; routes/messages are not yet linked to handlers (unknown boundary)" } },
        "precision": dataflow::PRECISION,
        "scope": scope_json(region, bt, ht),
        "incomplete": partial,
        "budgetExhausted": if partial {
            json!({ "base": bj["incomplete"] == true, "head": hj["incomplete"] == true,
                "why": hj["incompleteWhy"].as_str().or(bj["incompleteWhy"].as_str()) })
        } else {
            Value::Null
        },
        "elapsedMs": t0.elapsed().as_millis() as u64,
        "skippedForBudget": skipped,
    })
}

/// The Python files whose imports can differ between the trees: the changed
/// ones, when the set of Python files is the same in both; `None` (all of
/// them) when a module was added or removed, since that can break or mend an
/// import in a file this change did not touch.
fn python_files_to_check(bt: &Tree, ht: &Tree) -> Option<std::collections::HashSet<String>> {
    fn py(t: &Tree) -> BTreeSet<&String> {
        t.all_files.iter().filter(|f| f.ends_with(".py")).collect()
    }
    let (bp, hp) = (py(bt), py(ht));
    if bp != hp {
        return None;
    }
    Some(
        hp.into_iter()
            .filter(|f| std::fs::read(bt.dir.path().join(f.as_str())).ok() != std::fs::read(ht.dir.path().join(f.as_str())).ok())
            .cloned()
            .collect(),
    )
}

/// What was analyzed: whole trees, or a diff-scoped region with the names
/// whose mentions were not followed.
fn scope_json(region: Option<&super::region::Region>, bt: &Tree, ht: &Tree) -> Value {
    let Some(r) = region else {
        return json!({ "mode": "full", "filesInRepo": ht.all_files.len().max(bt.all_files.len()) });
    };
    let mut un: Vec<&(String, super::region::Role, usize)> = r.unexplored.iter().collect();
    un.sort_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
    json!({
        "mode": "diff",
        "filesAnalyzed": r.files.len(), "filesInRepo": r.repo_files,
        "bytesAnalyzed": r.bytes, "bytesInRepo": r.repo_bytes, "budgetBytes": r.budget,
        "unexploredCount": un.len(),
        "unexplored": un.iter().take(200).map(|(n, role, files)| json!({ "name": n, "role": role.as_str(), "files": files })).collect::<Vec<_>>(),
        "note": "diff-scoped: the changed files, every file mentioning a changed entity's name or a changed file's stem, \
                 and the definitions of rarely mentioned names the change uses. Callers resolved through scope and imports are complete unless \
                 the name is listed as unexplored; a caller attributed by name alone (receiver type unknown) may differ from a whole-tree run. Cycles, propagation cost, centrality, the dependent cone and data paths are computed within the region: \
                 a path or cycle through files outside it is unknown, and calls into them count as unresolved.",
    })
}

/// The notice a diff-scoped report carries.
fn scope_note(r: &Value) -> Option<String> {
    let sc = &r["scope"];
    if sc["mode"] != "diff" {
        return None;
    }
    let mb = |v: &Value| v.as_f64().unwrap_or(0.0) / (1024.0 * 1024.0);
    let un = arr(&sc["unexplored"]);
    let callers: Vec<String> = un.iter().filter(|u| u["role"] == "callers").take(6).map(|u| format!("`{}` ({} files)", s(&u["name"]), u["files"])).collect();
    let mut t = format!(
        "Diff-scoped analysis: {} of {} files ({:.1} of {:.1} MB): the change, its callers and importers, and what it calls. Cycles, propagation cost, dependents and data paths hold within that region; paths through other files are unknown.",
        sc["filesAnalyzed"], sc["filesInRepo"], mb(&sc["bytesAnalyzed"]), mb(&sc["bytesInRepo"])
    );
    if !callers.is_empty() {
        t += &format!(" Callers not searched (name too widely used to fit): {}.", callers.join(", "));
    }
    let n = sc["unexploredCount"].as_u64().unwrap_or(0);
    if n > 0 {
        t += &format!(" {n} name(s) in all were not followed (see `scope.unexplored` in --json).");
    }
    Some(t)
}

fn source_words(class: &str) -> &'static str {
    match class {
        "http-input" => "request input",
        "tool-input" => "tool-call arguments from an agent or client",
        "net-input" => "network replies",
        "cli-input" => "command-line input",
        "env" => "environment configuration",
        "file-read" => "file contents",
        "db-read" => "database rows",
        _ => "source data",
    }
}

fn sink_words(class: &str) -> &'static str {
    match class {
        "exec" => "a command that is executed",
        "db" => "a database query",
        "template" => "a rendered template",
        "http-response" => "an HTTP response",
        "file-path" => "a file path (path traversal)",
        "file-write" => "a file's contents",
        "net-send" => "an outgoing request (SSRF, or data leaving)",
        "log" => "the logs",
        _ => "a sink",
    }
}

/// One line on why a finding matters, for a reviewer who does not know
/// what the analysis is for.
fn why(f: &Finding) -> Option<String> {
    let d = &f.data;
    Some(match f.kind {
        "new-data-path" | "new-possible-path" => {
            let (src, sink) = (s(&d["source"]["class"]), s(&d["sink"]["class"]));
            let who = if UNTRUSTED.contains(&src.as_str()) { "untrusted " } else { "" };
            format!("{who}{} now flows into {}{}", source_words(&src), sink_words(&sink), if f.kind == "new-possible-path" { ", if the receiver is what its method name suggests" } else { "" })
        }
        "new-unknown-path" => "this data enters code sem cannot see; where it ends up is not checked".into(),
        "new-cycle" => "changes now ripple both ways between these, and they cannot be tested or released independently".into(),
        "signature-change" if f.severity == Severity::Low => "callers still fit (annotations only, or optional parameters added)".into(),
        "signature-change" => "callers that were not updated will break, at compile time or at run time".into(),
        "broken-import" => "loading this file fails: the module it imports does not exist".into(),
        "law-broken" => "the change breaks a rule this repo declared for its structure".into(),
        "new-package-dependency" => "a new coupling: changes in the target can now break the source package".into(),
        "new-package" => "a new module boundary; check it depends only on what it should".into(),
        "side-effect-change" | "new-entity-effects" => "callers now get effects they did not have (I/O, state)".into(),
        "complexity" => "more paths to review and test".into(),
        "propagation-cost" => "a change to one file now reaches more of the codebase".into(),
        "centrality" => "more dependency paths now run through this package".into(),
        "service-boundary" => "other services that use this schema can break, and they are not visible here".into(),
        "unknown-coverage" => "more of the code is invisible to this analysis".into(),
        _ => return None,
    })
}

/// The parameters of a signature's first parameter list: `(name, optional)`.
fn params_of(sig: &str) -> Option<Vec<(String, bool)>> {
    let open = sig.find('(')?;
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut parts = Vec::new();
    let mut prev = ' ';
    for c in sig[open..].chars() {
        let arrow = c == '>' && (prev == '=' || prev == '-');
        prev = c;
        if arrow {
            cur.push(c);
            continue;
        }
        match c {
            '(' | '[' | '{' | '<' => {
                depth += 1;
                if depth > 1 {
                    cur.push(c);
                }
            }
            ')' | ']' | '}' | '>' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                cur.push(c);
            }
            ',' if depth == 1 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    Some(
        parts
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(|p| {
                let optional = p.contains('=') || p.split(':').next().is_some_and(|n| n.trim_end().ends_with('?'));
                let head = p.split([':', '=']).next().unwrap_or(&p).trim();
                let head = head.trim_start_matches("...").trim_start_matches('*').trim_start_matches('&').trim_start_matches("mut ").trim();
                let name = head.split_whitespace().next().unwrap_or(head).trim_end_matches('?').to_string();
                (name, optional)
            })
            .collect(),
    )
}

/// How a signature's parameters changed: `annotations-only` (same names,
/// same required-ness), `compatible-extension` (only optional parameters
/// added), or `breaking` (anything else).
fn param_shape_change(before: &str, after: &str) -> &'static str {
    let (Some(b), Some(a)) = (params_of(before), params_of(after)) else { return "breaking" };
    if a == b {
        return "annotations-only";
    }
    if a.len() > b.len() && a[..b.len()] == b[..] && a[b.len()..].iter().all(|(_, opt)| *opt) {
        return "compatible-extension";
    }
    "breaking"
}

fn short(sha: &Value) -> String {
    let x = s(sha);
    x[..12.min(x.len())].to_string()
}

/// Checks the budget ran out before.
fn skipped_note(r: &Value) -> Option<String> {
    let k: Vec<String> = arr(&r["skippedForBudget"]).iter().map(s).collect();
    (!k.is_empty()).then(|| format!("Not checked, the time budget ran out first: {}. Raise --budget (or 0) to include them.", k.join(", ")))
}

/// The notice a partial (budget-exhausted) report carries.
fn budget_note(r: &Value) -> Option<String> {
    let b = &r["budgetExhausted"];
    if !b.is_object() {
        return None;
    }
    let which = match (b["base"] == true, b["head"] == true) {
        (true, true) => "base and head",
        (true, false) => "base",
        _ => "head",
    };
    Some(format!(
        "Budget exhausted ({}, {which}): data paths, side effects and escapes are partial. A path missing here may exist; a \"new\" path may exist at base too. `--budget 0` / `--max-memory 0` lift the limits.",
        s(&b["why"])
    ))
}

pub fn render_text(r: &Value, max: usize) -> String {
    let mut o = String::new();
    if let Some(n) = budget_note(r) {
        o += &format!("{n}\n");
    }
    if let Some(n) = skipped_note(r) {
        o += &format!("{n}\n");
    }
    if let Some(n) = scope_note(r) {
        o += &format!("{n}\n");
    }
    let sv = &r["severity"];
    let sm = &r["summary"];
    o += &format!(
        "arch-diff {}..{}: {} files, {} entities changed | {} high, {} medium, {} low\n",
        short(&r["base"]),
        short(&r["head"]),
        sm["filesChanged"],
        sm["entitiesChanged"],
        sv["high"],
        sv["medium"],
        sv["low"]
    );
    let fs = arr(&r["findings"]);
    let shown: Vec<&Value> = fs.iter().filter(|f| f["severity"] != "info").take(max * 2).collect();
    for f in &shown {
        o += &format!("  [{}] {}\n", s(&f["severity"]).to_uppercase(), s(&f["title"]));
    }
    let rest = fs.iter().filter(|f| f["severity"] != "info").count().saturating_sub(shown.len());
    if rest > 0 {
        o += &format!("  … +{rest} more (see --json / --md)\n");
    }
    for u in arr(&r["unchanged"]) {
        o += &format!("  unchanged: {}\n", s(&u));
    }
    let c = &r["coverage"]["head"];
    let dp = &r["dataPaths"];
    o += &format!(
        "  data paths {} -> {}; unknown: {} of {} call sites unresolved ({:.1}%), {} path(s) may reach via unknown code\n",
        dp["base"],
        dp["head"],
        c["unknown"],
        c["callSites"],
        c["unknownRate"].as_f64().unwrap_or(0.0) * 100.0,
        dp["escapesHead"]
    );
    o
}

pub fn render_md(r: &Value, max: usize) -> String {
    let mut o = String::new();
    let sv = &r["severity"];
    let sm = &r["summary"];
    o += &format!("## Architecture delta `{}..{}`\n\n", short(&r["base"]), short(&r["head"]));
    if let Some(n) = budget_note(r) {
        o += &format!("> **{n}**\n\n");
    }
    if let Some(n) = skipped_note(r) {
        o += &format!("> **{n}**\n\n");
    }
    if let Some(n) = scope_note(r) {
        o += &format!("> {n}\n\n");
    }
    o += &format!(
        "{} files, {} entities changed. **{} high**, {} medium, {} low.\n\n",
        sm["filesChanged"], sm["entitiesChanged"], sv["high"], sv["medium"], sv["low"]
    );
    let fs = arr(&r["findings"]);
    for (sev, head) in [("high", "High"), ("medium", "Medium"), ("low", "Low")] {
        let v: Vec<&Value> = fs.iter().filter(|f| f["severity"] == sev).collect();
        if v.is_empty() {
            continue;
        }
        o += &format!("### {head}\n");
        for f in v.iter().take(max * 2) {
            o += &format!("- {}\n", s(&f["title"]));
            if let Some(w) = f["why"].as_str() {
                o += &format!("  - why it matters: {w}\n");
            }
            let shown = if f["kind"] == "signature-change" { 10 } else { 6 };
            for d in arr(&f["details"]).iter().take(shown) {
                o += &format!("  - {}\n", s(d));
            }
        }
        if v.len() > max * 2 {
            o += &format!("- … +{} more\n", v.len() - max * 2);
        }
        o += "\n";
    }
    o += "### Unchanged\n";
    for u in arr(&r["unchanged"]) {
        o += &format!("- {}\n", s(&u));
    }
    let d = &r["dependencies"];
    let dp = &r["dataPaths"];
    let c = &r["coverage"]["head"];
    o += &format!(
        "\n### Numbers\n- data paths {} -> {} (+{} / -{}); possible (name-only) {}; paths into unresolved code {}\n- file edges {} -> {}; package edges {} -> {}; package cycles {} -> {}; file cycles {} -> {}\n- propagation cost (files) {:.1}% -> {:.1}%\n- unresolved calls {} of {} ({:.1}%)\n",
        dp["base"], dp["head"], dp["added"], dp["removed"], dp["possibleHead"], dp["escapesHead"],
        d["fileEdges"][0], d["fileEdges"][1], d["packageEdges"][0], d["packageEdges"][1], d["packageCycles"][0], d["packageCycles"][1], d["fileCycles"][0], d["fileCycles"][1],
        d["propagationCost"]["file"][0].as_f64().unwrap_or(0.0) * 100.0, d["propagationCost"]["file"][1].as_f64().unwrap_or(0.0) * 100.0,
        c["unknown"], c["callSites"], c["unknownRate"].as_f64().unwrap_or(0.0) * 100.0,
    );
    o += &format!("\n<sub>{}</sub>\n", s(&r["precision"]));
    o
}

/// `sem dataflow [path]`: facts and flows of the working tree.
pub fn dataflow_command(path: &str, json_out: bool, models: &[PathBuf], max: usize) -> Result<(), Box<dyn std::error::Error>> {
    let root = super::repo_root_or_cwd(path);
    let registry = super::create_registry(&root.to_string_lossy());
    let files = super::graph::find_supported_files_public(&root, &registry, &[]);
    let (_, entities) = EntityGraph::build(&root, &files, &registry);
    let models = load_models(&[&root], models)?;
    let a = analyze_tree(&root, &files, &entities, &models, dataflow::Limits::default(), None);
    let j = a.to_json();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&j)?);
        return Ok(());
    }
    let c = &j["coverage"];
    println!(
        "{} files, {} functions; {} call sites: {} repo, {} modeled external, {} unmodeled external, {} unresolved ({:.1}%)",
        c["files"], c["functions"], c["callSites"], c["resolvedToRepo"], c["externalModeled"], c["externalUnmodeled"], c["unknown"],
        c["unknownRate"].as_f64().unwrap_or(0.0) * 100.0
    );
    let flows = arr(&j["flows"]);
    println!("{} data path(s):", flows.len());
    for f in flows.iter().take(max) {
        println!("  {}", flow_line(f));
        for p in arr(&f["path"]) {
            println!("      {}", s(&p));
        }
    }
    let esc = arr(&j["escapes"]);
    println!("{} source(s) reach unresolved code; {} possible (name-only) path(s)", esc.len(), arr(&j["possibleFlows"]).len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::param_shape_change as shape;
    use super::{flow_severity, Severity};

    #[test]
    fn tool_arguments_rank_like_request_input() {
        assert_eq!(flow_severity("tool-input", "file-path"), Severity::High);
        assert_eq!(flow_severity("tool-input", "exec"), Severity::High);
        assert_eq!(flow_severity("http-input", "net-send"), Severity::Medium);
        assert_eq!(flow_severity("tool-input", "net-send"), Severity::Medium);
        assert_eq!(flow_severity("env", "exec"), Severity::Low);
    }

    #[test]
    fn annotation_only_changes_are_not_breaking() {
        assert_eq!(shape("def f(a: dict[str, any], b: int) -> X", "def f(a: dict[str, Any], b: int) -> X"), "annotations-only");
        assert_eq!(shape("function f(a: number, cb: (x: T) => void)", "function f(a: bigint, cb: (x: U) => void)"), "annotations-only");
        assert_eq!(shape("fn f(&self, a: Vec<u8>)", "fn f(&self, a: &[u8])"), "annotations-only");
    }

    #[test]
    fn optional_additions_are_compatible() {
        assert_eq!(shape("def f(a)", "def f(a, b=None)"), "compatible-extension");
        assert_eq!(shape("function f(a: string)", "function f(a: string, b?: number)"), "compatible-extension");
    }

    #[test]
    fn required_or_renamed_parameters_break() {
        assert_eq!(shape("def save(name)", "def save(name, backup)"), "breaking");
        assert_eq!(shape("def f(a, b)", "def f(b, a)"), "breaking");
        assert_eq!(shape("func Get(ctx context.Context, id string)", "func Get(id string)"), "breaking");
    }
}
