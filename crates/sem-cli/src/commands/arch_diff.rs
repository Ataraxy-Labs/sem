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
pub(crate) fn analyze_tree(dir: &Path, files: &[String], entities: &[sem_core::model::entity::SemanticEntity], models: &Models) -> Analysis {
    let specs = super::topology::spec_targets(&dir.to_string_lossy());
    let resolve = move |from: &str, spec: &str| specs.get(&(from.to_string(), spec.to_string())).cloned();
    dataflow::analyze(dir, files, entities, &resolve, models)
}

fn is_test_file(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    let leaf = l.rsplit('/').next().unwrap_or(&l);
    l.contains("/test/") || l.contains("/tests/") || l.contains("__tests__") || l.starts_with("test/") || l.starts_with("tests/")
        || l.contains("/testdata/") || l.contains("/fixtures/") || l.contains(".test.") || l.contains(".spec.")
        || leaf.starts_with("test_") || leaf.ends_with("_test.py") || leaf.ends_with("_test.go") || leaf == "conftest.py"
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
}

/// File-level import edges for Python / Go / Rust, from each file's
/// imports: a Python dotted module, a Go import path under the repo's
/// module path, a Rust `crate::` path — resolved to repo files by path
/// only when the answer is unambiguous; otherwise no edge.
fn import_edges(dir: &Path, a: &Analysis) -> Vec<(String, String)> {
    let files: BTreeSet<&str> = a.files.iter().map(|f| f.path.as_str()).collect();
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
    fn build(t: &Tree, a: &Analysis) -> Deps {
        let g: &EntityGraph = &t.graph;
        let mut edges = BTreeSet::new();
        for (x, y) in import_edges(t.dir.path(), a) {
            if !is_test_file(&x) && !is_test_file(&y) {
                edges.insert((x, y));
            }
        }
        // JS/TS entity edges come from a name-matching resolver: their file
        // dependencies are taken from resolved imports (topology) instead
        let js = |p: &str| dataflow::ir::Lang::for_path(p) == Some(dataflow::ir::Lang::Ts);
        for e in &g.edges {
            let (Some(a), Some(b)) = (g.entities.get(e.from_entity.as_str()), g.entities.get(e.to_entity.as_str())) else { continue };
            if js(&a.file_path) || js(&b.file_path) {
                continue;
            }
            if a.file_path != b.file_path && !is_test_file(&a.file_path) && !is_test_file(&b.file_path) {
                edges.insert((a.file_path.clone(), b.file_path.clone()));
            }
        }
        let (nodes, adj) = super::topology::module_value_graph(&t.dir.path().to_string_lossy());
        for (u, vs) in adj.iter().enumerate() {
            for &v in vs {
                if !is_test_file(&nodes[u]) && !is_test_file(&nodes[v]) {
                    edges.insert((nodes[u].clone(), nodes[v].clone()));
                }
            }
        }
        // a Go package is its directory: references between its files are
        // not dependencies (and cycles among them mean nothing)
        edges.retain(|(a, b)| !(a.ends_with(".go") && b.ends_with(".go") && package_of(a) == package_of(b)));
        let mut files: BTreeSet<String> = t.files.iter().filter(|f| !is_test_file(f)).cloned().collect();
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
        Deps { files: files.into_iter().collect(), edges, pkg_edges }
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
    let btw = if pkgs.len() <= 4000 { algo::betweenness(&padj) } else { vec![0.0; pkgs.len()] };
    let n = pkgs.len() as f64;
    let norm = if n > 2.0 { (n - 1.0) * (n - 2.0) } else { 1.0 };
    Measures {
        file_cycles: Deps::cycles(&d.files, &fadj),
        pkg_cycles: Deps::cycles(&pkgs, &padj),
        file_pc: Deps::propagation_cost(&fadj),
        pkg_pc: Deps::propagation_cost(&padj),
        centrality: pkgs.iter().cloned().zip(btw.into_iter().map(|b| b / norm)).collect(),
    }
}

const UNTRUSTED: &[&str] = &["http-input", "net-input", "cli-input", "db-read", "file-read"];
const DANGEROUS: &[&str] = &["exec", "db", "template", "http-response", "file-write"];
const EFFECTS: &[&str] = &["exec", "db", "net-send", "file-write", "template", "http-response"];

fn flow_severity(src: &str, sink: &str) -> Severity {
    match (src, sink) {
        (s, k) if ["http-input", "net-input"].contains(&s) && DANGEROUS.contains(&k) => Severity::High,
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

pub fn arch_diff_command(opts: ArchDiffOptions) -> Result<(), Box<dyn std::error::Error>> {
    let root = super::repo_root_or_cwd(&opts.cwd);
    let (base, head) = certify::resolve_range(&root, &opts.range)?;
    let t0 = std::time::Instant::now();
    let (bt, ht) = certify::build_trees(&root, &base, &head)?;
    let cert = certify::certificate(&root, &base, &head, &bt, &ht, &opts.laws)?;
    let models = load_models(&[ht.dir.path()], &opts.models)?;
    // data flow of production code only: test code feeding shared state
    // would otherwise manufacture paths no deployment has
    let prod = |files: &[String]| -> Vec<String> { files.iter().filter(|f| !is_test_file(f)).cloned().collect() };
    let (bprod, hprod) = (prod(&bt.files), prod(&ht.files));
    let (ba, ha) = std::thread::scope(|sc| {
        let b = sc.spawn(|| analyze_tree(bt.dir.path(), &bprod, &bt.entities, &models));
        let h = sc.spawn(|| analyze_tree(ht.dir.path(), &hprod, &ht.entities, &models));
        (b.join().expect("base dataflow"), h.join().expect("head dataflow"))
    });
    let (bj, hj) = (ba.to_json(), ha.to_json());
    let (bd, hd) = (Deps::build(&bt, &ba), Deps::build(&ht, &ha));
    let (bm, hm) = (measure(&bd), measure(&hd));
    let report = compose(&cert, &bj, &hj, &bd, &hd, &bm, &hm, &bt, &ht, opts.max_items, t0.elapsed());
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
    max: usize,
    elapsed: std::time::Duration,
) -> Value {
    let mut findings: Vec<Finding> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();

    // -- data paths ------------------------------------------------------------
    let (bf, hf) = (keyed(bj, "flows"), keyed(hj, "flows"));
    let new_flows: Vec<&Value> = hf.iter().filter(|(k, _)| !bf.contains_key(*k)).map(|(_, v)| v).collect();
    let gone_flows: Vec<&Value> = bf.iter().filter(|(k, _)| !hf.contains_key(*k)).map(|(_, v)| v).collect();
    for f in &new_flows {
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
                "new data path{}: {}",
                if object_state { " (via object state; may conflate instances)" } else { "" },
                flow_line(f)
            ),
            details,
            data: (*f).clone(),
        });
    }
    for f in &gone_flows {
        findings.push(Finding { severity: Severity::Info, kind: "removed-data-path", title: format!("removed data path: {}", flow_line(f)), details: Vec::new(), data: (*f).clone() });
    }
    if new_flows.is_empty() && gone_flows.is_empty() {
        unchanged.push(format!("No source->sink data path was added or removed ({} path(s) at both base and head, within the modeled sources/sinks).", hf.len()));
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
        if is_test_file(&s(&he_["file"])) {
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
    if effect_changes == 0 {
        unchanged.push(format!("Side effects (env/file/DB/network/subprocess/log/template reads and writes, module state, fields) are unchanged for all {common} function(s) present in both trees."));
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
    for ((a, b), (fa, fb)) in &new_pkg {
        findings.push(Finding {
            severity: Severity::Medium,
            kind: "new-package-dependency",
            title: format!("new package dependency {a}/ -> {b}/ (e.g. {fa} -> {fb})"),
            details: Vec::new(),
            data: json!({ "from": a, "to": b, "witness": [fa, fb] }),
        });
    }
    for (a, b) in &gone_pkg {
        findings.push(Finding { severity: Severity::Info, kind: "removed-package-dependency", title: format!("removed package dependency {a}/ -> {b}/"), details: Vec::new(), data: json!({ "from": a, "to": b }) });
    }
    if new_pkg.is_empty() && gone_pkg.is_empty() {
        unchanged.push(format!("No package (directory) dependency was added or removed ({} package edge(s)).", hd.pkg_edges.len()));
    }
    let new_file_edges: Vec<&(String, String)> = hd.edges.difference(&bd.edges).collect();
    let gone_file_edges: Vec<&(String, String)> = bd.edges.difference(&hd.edges).collect();
    for (cyc, level, sev) in [(&hm.pkg_cycles, "package", Severity::High), (&hm.file_cycles, "file", Severity::Medium)] {
        let before = if level == "package" { &bm.pkg_cycles } else { &bm.file_cycles };
        let before_members: BTreeSet<&String> = before.iter().flatten().collect();
        for c in cyc.iter().filter(|c| !before.contains(*c)) {
            let newly: Vec<&String> = c.iter().filter(|m| !before_members.contains(m)).collect();
            let grown = newly.len() < c.len();
            findings.push(Finding {
                severity: if grown && level == "file" { Severity::Low } else { sev },
                kind: "new-cycle",
                title: format!(
                    "{} {level} cycle of {}: {}{}",
                    if grown { "changed" } else { "new" },
                    c.len(),
                    c.iter().take(6).cloned().collect::<Vec<_>>().join(", "),
                    if c.len() > 6 { ", …" } else { "" }
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
        unchanged.push(format!("Cycles unchanged: {} package-level and {} file-level strongly connected component(s), identical members.", hm.pkg_cycles.len(), hm.file_cycles.len()));
    }
    let pc_delta = (hm.file_pc - bm.file_pc) * 100.0;
    if pc_delta.abs() >= 0.05 {
        findings.push(Finding {
            severity: if pc_delta >= 2.0 { Severity::Medium } else { Severity::Info },
            kind: "propagation-cost",
            title: format!("propagation cost (file graph) {:.1}% -> {:.1}% ({:+.1} pts); package graph {:.1}% -> {:.1}%", bm.file_pc * 100.0, hm.file_pc * 100.0, pc_delta, bm.pkg_pc * 100.0, hm.pkg_pc * 100.0),
            details: Vec::new(),
            data: json!({ "file": [bm.file_pc, hm.file_pc], "package": [bm.pkg_pc, hm.pkg_pc] }),
        });
    } else {
        unchanged.push(format!("Propagation cost unchanged: {:.1}% of file pairs reachable ({:.1}% of package pairs).", hm.file_pc * 100.0, hm.pkg_pc * 100.0));
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
            title: format!("package {p}/ betweenness centrality {b:.3} -> {h:.3}"),
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
    for g in &sigs {
        let callers = arr(&g["callers"]);
        let stale: Vec<&Value> = callers.iter().filter(|c| c["touchedByChange"] != true).collect();
        let stale_old = arr(&g["staleCallersOfOldName"]);
        let shape = param_shape_change(&s(&g["before"]), &s(&g["after"]));
        let breaking = !matches!(shape, "annotations-only" | "compatible-extension");
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
        findings.push(Finding {
            severity: sev,
            kind: "signature-change",
            title: format!("public contract of `{}` ({}:{}) changed; {} static caller(s), {} not modified by this change", s(&g["entity"]), s(&g["file"]), g["line"], callers.len(), stale.len()),
            details,
            data: g.clone(),
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
    let (bs, hs) = (schema_files(bt.dir.path(), &bt.files), schema_files(ht.dir.path(), &ht.files));
    let changed_files: BTreeSet<String> = arr(&cert["entities"]).iter().map(|e| s(&e["file"])).collect();
    let touched_schema: Vec<&String> = hs.iter().filter(|f| !bs.contains(*f) || changed_files.contains(*f)).collect();
    // relative imports that resolve to no file (JS/TS)
    let (bb, hb) = (
        super::topology::broken_relative_imports(&bt.dir.path().to_string_lossy()),
        super::topology::broken_relative_imports(&ht.dir.path().to_string_lossy()),
    );
    for (file, spec) in hb.difference(&bb) {
        findings.push(Finding {
            severity: if is_test_file(file) { Severity::Medium } else { Severity::High },
            kind: "broken-import",
            title: format!("{file} imports `{spec}`, which resolves to no file"),
            details: Vec::new(),
            data: json!({ "file": file, "specifier": spec }),
        });
    }
    if hb.difference(&bb).next().is_none() {
        unchanged.push(format!("No new unresolvable relative import ({} at head).", hb.len()));
    }
    for f in &touched_schema {
        findings.push(Finding { severity: Severity::Medium, kind: "service-boundary", title: format!("service schema added or changed: {f} (its consumers are not modeled as edges)"), details: Vec::new(), data: json!({ "file": f }) });
    }

    findings.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.kind.cmp(b.kind)).then(a.title.cmp(&b.title)));
    let count = |sev: Severity| findings.iter().filter(|f| f.severity == sev).count();
    json!({
        "base": cert["base"], "head": cert["head"],
        "summary": cert["summary"],
        "severity": { "high": count(Severity::High), "medium": count(Severity::Medium), "low": count(Severity::Low), "info": count(Severity::Info) },
        "findings": findings.iter().map(|f| json!({ "severity": f.severity.as_str(), "kind": f.kind, "title": f.title, "details": f.details, "data": f.data })).collect::<Vec<_>>(),
        "unchanged": unchanged,
        "dataPaths": { "base": bf.len(), "head": hf.len(), "added": new_flows.len(), "removed": gone_flows.len(),
            "possibleHead": hp.len(), "unknownPathsHead": hu.len(), "newUnknownPaths": new_uflows,
            "escapesHead": he.len(), "newEscapes": new_esc.len() },
        "dependencies": { "fileEdges": [bd.edges.len(), hd.edges.len()], "fileEdgesAdded": new_file_edges.len(), "fileEdgesRemoved": gone_file_edges.len(),
            "fileEdgesAddedList": new_file_edges.iter().take(200).map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "packageEdges": [bd.pkg_edges.len(), hd.pkg_edges.len()],
            "packageCycles": [bm.pkg_cycles.len(), hm.pkg_cycles.len()], "fileCycles": [bm.file_cycles.len(), hm.file_cycles.len()],
            "propagationCost": { "file": [bm.file_pc, hm.file_pc], "package": [bm.pkg_pc, hm.pkg_pc] },
            "derivedFrom": "resolved references between entities (calls, type references, imports) plus JS/TS module imports; test files excluded" },
        "complexity": complexity,
        "coverage": { "base": bc, "head": hc },
        "serviceBoundaries": { "schemaFiles": hs, "note": if hs.is_empty() { "no OpenAPI/protobuf/queue schema files found; any service boundary is an unmodeled unknown" } else { "schema files present; routes/messages are not yet linked to handlers (unknown boundary)" } },
        "precision": dataflow::PRECISION,
        "incomplete": bj["incomplete"].as_bool().unwrap_or(false) || hj["incomplete"].as_bool().unwrap_or(false),
        "elapsedMs": elapsed.as_millis() as u64,
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

pub fn render_text(r: &Value, max: usize) -> String {
    let mut o = String::new();
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
            for d in arr(&f["details"]).iter().take(6) {
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
    let a = analyze_tree(&root, &files, &entities, &models);
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
