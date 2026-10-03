//! Assemble the whole-system graph layer by layer and measure it.
//!
//! Layers are cumulative: `base` (repo code) → `deps` (locked dependencies
//! and the standard library) → `db` (schema) → `config` (routes, env,
//! dispatch tables, registries, manifests) → `contracts` (OpenAPI / proto /
//! GraphQL) → `dynamic` (a runtime trace). Every site — a call site from a
//! language front-end, or a boundary site from the models — gets one
//! outcome per layer: resolved (`R`), external (`X`), unknown (`U:<why>`) or
//! observed at runtime (`O`). The site set and the source/sink sets are
//! fixed from the final world, so every layer is measured on the same
//! denominator.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use super::config::{self, ConfigFacts};
use super::contracts::{self, path_match, path_segments, Contracts};
use super::locate::{self, Located, RootKind};
use super::lockfiles;
use super::models::{self, BoundarySite, Kind, Models};
use super::scan::{self, Arg, FileScan, Lang};
use super::sql::{self, Schema};
use crate::parser::graph::{EntityGraph, RefType};
use crate::parser::registry::ParserRegistry;
use crate::parser::test_detect::is_test_path;

pub const LAYERS: [&str; 6] = ["base", "deps", "db", "config", "contracts", "dynamic"];
const BASE: usize = 0;
const DEPS: usize = 1;
const DB: usize = 2;
const CONFIG: usize = 3;
const CONTRACTS: usize = 4;
const DYNAMIC: usize = 5;
pub const PATH_DEPTH: usize = 12;

pub struct Input {
    pub root: PathBuf,
    pub dep_roots: Vec<(RootKind, PathBuf)>,
    /// TypeScript front-end site files: (without deps, with deps).
    pub ts_sites: Option<(PathBuf, PathBuf)>,
    pub schema_dumps: Vec<PathBuf>,
    pub trace: Option<PathBuf>,
    pub models: Models,
    pub out: PathBuf,
    /// The data-flow engine, run over a layer's file set and entities
    /// (base, then deps); returns its JSON (`dataflow::Analysis::to_json`).
    pub dataflow: Option<DataflowFn>,
}

pub type DataflowFn = std::sync::Arc<dyn Fn(&[String], &[crate::model::entity::SemanticEntity]) -> Value + Send + Sync>;

/// Wall-clock budget for one data-flow run (`SEM_SYSTEM_DATAFLOW_SECS`,
/// default 300): past it the layer is reported as timed out.
pub fn dataflow_budget() -> std::time::Duration {
    let s = std::env::var("SEM_SYSTEM_DATAFLOW_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300u64);
    std::time::Duration::from_secs(s)
}

/// Outcome of a site at one layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Outcome {
    R,
    X,
    U(String),
    O,
}

impl Outcome {
    fn code(&self) -> String {
        match self {
            Outcome::R => "R".into(),
            Outcome::X => "X".into(),
            Outcome::U(w) => format!("U:{w}"),
            Outcome::O => "O".into(),
        }
    }
    fn known(&self) -> bool {
        matches!(self, Outcome::R | Outcome::O)
    }
}

/// A node in the whole-system graph: an entity id, or a synthetic node
/// (`table:x`, `route:GET /x`, `decl:file:line`, `env:X`).
type Node = String;

#[derive(Clone, Debug, Serialize)]
pub struct SiteRow {
    pub file: String,
    pub line: usize,
    pub lang: &'static str,
    /// "call" or a boundary kind.
    pub kind: String,
    pub text: String,
    pub caller: Option<String>,
    pub outcomes: Vec<String>,
    /// Targets at the last layer that resolved it.
    pub targets: Vec<String>,
    /// Layer index at which the targets were added.
    pub resolved_at: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

struct Ent {
    id: String,
    name: String,
    ty: String,
    start: usize,
    end: usize,
}

/// Innermost-entity lookup per file.
#[derive(Default)]
struct EntIndex {
    by_file: HashMap<String, Vec<Ent>>,
    by_name: HashMap<String, Vec<(String, String)>>, // name -> (id, file)
}

impl EntIndex {
    fn build(graph: &EntityGraph) -> Self {
        let mut ix = EntIndex::default();
        for e in graph.entities.values() {
            ix.by_file.entry(e.file_path.clone()).or_default().push(Ent {
                id: e.id.as_str().to_string(),
                name: e.name.clone(),
                ty: e.entity_type.clone(),
                start: e.start_line,
                end: e.end_line,
            });
            ix.by_name
                .entry(e.name.clone())
                .or_default()
                .push((e.id.as_str().to_string(), e.file_path.clone()));
        }
        for v in ix.by_file.values_mut() {
            v.sort_by_key(|e| (e.start, std::cmp::Reverse(e.end)));
        }
        ix
    }
    fn at(&self, file: &str, line: usize) -> Option<&Ent> {
        self.by_file
            .get(file)?
            .iter()
            .filter(|e| e.start <= line && line <= e.end)
            .min_by_key(|e| e.end - e.start)
    }
    /// The entity a line belongs to; top-level code belongs to its module
    /// (`module:<file>`), so module-level calls and inline route handlers
    /// still have an owner.
    fn owner(&self, file: &str, line: usize) -> String {
        self.at(file, line).map(|e| e.id.clone()).unwrap_or_else(|| format!("module:{file}"))
    }

    fn fn_at(&self, file: &str, line: usize, name: Option<&str>) -> Option<&Ent> {
        let v = self.by_file.get(file)?;
        if let Some(n) = name.map(|n| n.rsplit(['.', ':']).next().unwrap_or(n)).filter(|n| !n.is_empty()) {
            if let Some(e) = v
                .iter()
                .filter(|e| e.name == n && e.start <= line + 3 && line <= e.end)
                .min_by_key(|e| e.end - e.start)
            {
                return Some(e);
            }
            // transpiled or sampled positions: a unique same-named entity in the file
            let same: Vec<&Ent> = v.iter().filter(|e| e.name == n).collect();
            if same.len() == 1 {
                return Some(same[0]);
            }
        }
        if line == 0 {
            return None;
        }
        self.at(file, line)
    }
    /// A repo entity named `name` (last dotted/scoped segment), preferring `file`.
    fn named(&self, name: &str, file: &str, repo: &dyn Fn(&str) -> bool) -> Vec<String> {
        let n = name.rsplit(['.', ':']).next().unwrap_or(name);
        let Some(c) = self.by_name.get(n) else { return Vec::new() };
        let c: Vec<&(String, String)> = c.iter().filter(|(_, f)| repo(f)).collect();
        let same: Vec<String> = c.iter().filter(|(_, f)| f == file).map(|(i, _)| i.clone()).collect();
        if !same.is_empty() {
            return same;
        }
        c.iter().map(|(i, _)| i.clone()).collect()
    }
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

fn line_of(starts: &[usize], byte: usize) -> usize {
    match starts.binary_search(&byte) {
        Ok(i) => i + 1,
        Err(i) => i,
    }
}

const SKIP_WALK: &[&str] = &[
    ".git", "node_modules", "vendor", "target", "dist", "build", ".sem-system", ".venv", "venv",
    "__pycache__", ".tox", ".mypy_cache", ".next", "coverage", ".cache",
];

fn walk_repo(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !SKIP_WALK.contains(&name.as_str()) {
                    stack.push(e.path());
                }
            } else if ft.is_file() {
                if let Ok(rel) = e.path().strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    out.sort();
    out
}

fn is_generated(f: &str) -> bool {
    f.ends_with(".pb.go")
        || f.contains("_pb2")
        || f.ends_with(".gen.go")
        || f.ends_with(".gen.ts")
        || f.contains("/genproto/")
        || f.contains("_grpc.pb")
}

/// Parse `SEM_CALLS_SITES` lines for repo files into (file, at) → (call, outcome, defs).
fn read_sites(path: &Path) -> Vec<(String, usize, bool, Outcome, Vec<String>)> {
    read_sites_split(path, &|_| true).0
}

/// Stream the site dump: sites of `keep` files in full, the rest only
/// counted by outcome (a dependency world's dump runs to gigabytes).
fn read_sites_split(
    path: &Path,
    keep: &dyn Fn(&str) -> bool,
) -> (Vec<(String, usize, bool, Outcome, Vec<String>)>, BTreeMap<String, usize>) {
    use std::io::BufRead;
    let mut kept = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let Ok(f) = std::fs::File::open(path) else { return (kept, counts) };
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        // cheap pre-filter on the file field before parsing
        let file_of = line
            .split("\"file\":\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or("");
        if !keep(file_of) {
            if !line.contains("\"call\":true") {
                continue;
            }
            let k = if line.contains("\"defs\":[") {
                "resolved".to_string()
            } else if let Some(u) = line.split("\"unknown\":\"").nth(1).and_then(|r| r.split('"').next()) {
                format!("unknown:{u}")
            } else {
                "external".to_string()
            };
            *counts.entry(k).or_default() += 1;
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let file = v["file"].as_str().unwrap_or("").to_string();
        let at = v["at"].as_u64().unwrap_or(0) as usize;
        let call = v["call"].as_bool().unwrap_or(false);
        let defs: Vec<String> = v["defs"]
            .as_array()
            .map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let outcome = if v["defs"].is_array() {
            Outcome::R
        } else if let Some(u) = v["unknown"].as_str() {
            Outcome::U(u.to_string())
        } else {
            Outcome::X
        };
        kept.push((file, at, call, outcome, defs));
    }
    (kept, counts)
}

/// One graph build with the call pipeline's per-site dump turned on.
fn build_with_sites(
    root: &Path,
    files: &[String],
    registry: &ParserRegistry,
    scratch: &Path,
    tag: &str,
    keep: &dyn Fn(&str) -> bool,
) -> (
    EntityGraph,
    Vec<crate::model::entity::SemanticEntity>,
    (Vec<(String, usize, bool, Outcome, Vec<String>)>, BTreeMap<String, usize>),
    f64,
    Option<usize>,
) {
    let dump = scratch.join(format!("sites-{tag}.jsonl"));
    let _ = std::fs::remove_file(&dump);
    // The pipeline reads these at resolve time; set only around this build.
    std::env::set_var("SEM_CALLS_STATS", "1");
    std::env::set_var("SEM_CALLS_SITES", &dump);
    let t = Instant::now();
    let (graph, entities) = EntityGraph::build(root, files, registry);
    let secs = t.elapsed().as_secs_f64();
    let rss = crate::parser::mem_profile::current_rss_bytes();
    std::env::remove_var("SEM_CALLS_STATS");
    std::env::remove_var("SEM_CALLS_SITES");
    let sites = read_sites_split(&dump, keep);
    let _ = std::fs::remove_file(&dump);
    (graph, entities, sites, secs, rss)
}

/// A TypeScript front-end site: `{"file","at","line","call","defs":[{"file","line","name"}]|null,"unknown","callee"}`.
struct TsSite {
    file: String,
    at: usize,
    line: usize,
    outcome: Outcome,
    defs: Vec<(String, usize, String)>,
    callee: String,
}

fn read_ts_sites(path: &Path) -> Vec<TsSite> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["call"].as_bool() == Some(true))
        .map(|v| {
            let defs: Vec<(String, usize, String)> = v["defs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|d| {
                            (
                                d["file"].as_str().unwrap_or("").to_string(),
                                d["line"].as_u64().unwrap_or(0) as usize,
                                d["name"].as_str().unwrap_or("").to_string(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let outcome = if v["defs"].is_array() {
                Outcome::R
            } else if let Some(u) = v["unknown"].as_str() {
                Outcome::U(u.to_string())
            } else {
                Outcome::X
            };
            TsSite {
                file: v["file"].as_str().unwrap_or("").to_string(),
                at: v["at"].as_u64().unwrap_or(0) as usize,
                line: v["line"].as_u64().unwrap_or(0) as usize,
                outcome,
                defs,
                callee: v["callee"].as_str().unwrap_or("").to_string(),
            }
        })
        .collect()
}

/// One observed runtime edge (world-relative paths).
#[derive(Clone, Debug)]
struct Obs {
    caller_file: String,
    /// 0 when the tracer samples stacks and knows no call-site line.
    site_line: usize,
    caller_line: usize,
    caller: String,
    callee_file: String,
    callee_line: usize,
    callee: String,
}

fn read_trace(path: &Path) -> (Vec<Obs>, HashSet<(String, usize)>) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut edges = Vec::new();
    let mut lines = HashSet::new();
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        if let Some(f) = v["executed_file"].as_str() {
            // coverage record: {"executed_file": f, "lines": [..]}
            for x in v["lines"].as_array().into_iter().flatten() {
                if let Some(n) = x.as_u64() {
                    lines.insert((f.to_string(), n as usize));
                }
            }
            continue;
        }
        let o = Obs {
            caller_file: v["caller_file"].as_str().unwrap_or("").to_string(),
            site_line: v["site_line"].as_u64().unwrap_or(0) as usize,
            caller_line: v["caller_line"].as_u64().unwrap_or(0) as usize,
            caller: v["caller"].as_str().unwrap_or("").to_string(),
            callee_file: v["callee_file"].as_str().unwrap_or("").to_string(),
            callee_line: v["callee_line"].as_u64().unwrap_or(0) as usize,
            callee: v["callee"].as_str().unwrap_or("").to_string(),
        };
        if o.site_line > 0 {
            lines.insert((o.caller_file.clone(), o.site_line));
        }
        edges.push(o);
    }
    (edges, lines)
}

struct CallSite {
    file: String,
    _at: usize,
    line: usize,
    lang: Lang,
    caller: Option<String>,
    base: Outcome,
    deps: Outcome,
    targets_base: Vec<Node>,
    targets_deps: Vec<Node>,
    /// Callee text from the scan (for table dispatch and trace matching).
    callee: String,
    /// Added by config / dynamic.
    later: Vec<(usize, Outcome, Vec<Node>)>,
}

impl CallSite {
    fn outcome(&self, layer: usize) -> Outcome {
        let mut o = if layer >= DEPS { self.deps.clone() } else { self.base.clone() };
        for (l, oc, _) in &self.later {
            if *l <= layer && !o.known() {
                o = oc.clone();
            }
        }
        o
    }
    fn targets(&self, layer: usize) -> Vec<Node> {
        let mut t = if layer >= DEPS { self.targets_deps.clone() } else { self.targets_base.clone() };
        for (l, _, tg) in &self.later {
            if *l <= layer {
                t.extend(tg.iter().cloned());
            }
        }
        t
    }
}

struct BSite {
    s: BoundarySite,
    caller: Option<String>,
    /// First layer at which it resolves, and its targets (+ edge direction).
    resolved: Option<(usize, Vec<Node>)>,
    why: String,
    /// sql: true = writes the tables (entity → table); false = reads.
    writes: bool,
}

/// Everything `run` measures, for `summary.json`.
#[derive(Default, Serialize)]
pub struct Summary {
    pub repo: String,
    pub files: BTreeMap<String, usize>,
    pub deps: Value,
    pub db: Value,
    pub config: Value,
    pub contracts: Value,
    pub layers: Vec<Value>,
    pub closed_world_sites: Value,
    pub costs: Value,
    pub sources: usize,
    pub sinks: usize,
    pub dynamic: Value,
    /// The data-flow engine's value-level flows per code layer.
    pub dataflow: Value,
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    for r in rows {
        serde_json::to_writer(&mut f, r)?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// Link each dependency root under `<root>/.sem-system/links/<i>/<name>`,
/// so the resolvers see dependency sources as part of the input.
fn link_roots(root: &Path, dep_roots: &[(RootKind, PathBuf)]) -> std::io::Result<Vec<(RootKind, String, PathBuf)>> {
    let base = root.join(".sem-system").join("links");
    std::fs::create_dir_all(&base)?;
    let mut out = Vec::new();
    for (i, (kind, path)) in dep_roots.iter().enumerate() {
        let dir = base.join(i.to_string());
        std::fs::create_dir_all(&dir)?;
        let link = dir.join(kind.link_name());
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link)?;
        }
        let target = std::fs::canonicalize(path)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link)?;
        out.push((*kind, format!(".sem-system/links/{i}/{}", kind.link_name()), target));
    }
    Ok(out)
}

/// Run every layer and write `summary.json`, `sites.jsonl`, `edges.jsonl`,
/// `world.json` into `input.out`.
pub fn run(input: &Input, registry: &ParserRegistry) -> Result<Summary, String> {
    let root = input.root.as_path();
    let t_all = Instant::now();
    let mut costs = serde_json::Map::new();
    std::fs::create_dir_all(&input.out).map_err(|e| e.to_string())?;
    let scratch = input.out.clone();

    // ------------------------------------------------------------ files
    let all_files = walk_repo(root);
    let nontest: Vec<&String> = all_files.iter().filter(|f| !is_test_path(f)).collect();
    // bundled / minified third-party assets are not the repo's code
    let minified = |f: &str| -> bool {
        if !matches!(Lang::of(f), Some(Lang::Js)) {
            return false;
        }
        std::fs::read_to_string(root.join(f)).is_ok_and(|s| s.lines().any(|l| l.len() > 1000))
    };
    let code: Vec<String> = nontest
        .iter()
        .filter(|f| Lang::of(f).is_some() && !f.ends_with(".min.js") && !minified(f))
        .map(|f| f.to_string())
        .collect();
    let pipeline_code: Vec<String> = code
        .iter()
        .filter(|f| crate::parser::calls::language_for(f).is_some())
        .cloned()
        .collect();
    let mut files_count = BTreeMap::new();
    for f in &code {
        *files_count.entry(Lang::of(f).unwrap().as_str().to_string()).or_insert(0usize) += 1;
    }
    let is_repo = |f: &str| {
        !f.starts_with(".sem-system/") && !f.starts_with("node_modules/") && !f.contains("/node_modules/") && !f.starts_with('<')
    };

    // ------------------------------------------------------------ deps (locate)
    let t = Instant::now();
    let linked = link_roots(root, &input.dep_roots).map_err(|e| format!("linking dependency roots: {e}"))?;
    let locked = lockfiles::locked_deps(root);
    let plain_roots: Vec<(RootKind, PathBuf)> = linked.iter().map(|(k, _, p)| (*k, p.clone())).collect();
    let located: Vec<Located> = locate::locate(&locked, &plain_roots);
    let mut dep_files: Vec<String> = Vec::new();
    let mut by_eco: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for l in &located {
        let e = by_eco.entry(l.dep.ecosystem.as_str().to_string()).or_default();
        e.0 += 1;
        if l.dir.is_some() {
            e.1 += 1;
        }
        e.2 += l.files.len();
        let Some(dir) = &l.dir else { continue };
        // the root this dep was found under
        let prefix = linked.iter().find(|(k, _, p)| match l.dep.ecosystem {
            lockfiles::Ecosystem::Pypi => *k == RootKind::SitePackages,
            lockfiles::Ecosystem::Go => *k == RootKind::GoModCache && p.join(dir).is_dir(),
            lockfiles::Ecosystem::Cargo => *k == RootKind::CargoSrc && p.join(dir).is_dir(),
            lockfiles::Ecosystem::Npm => false,
        });
        if let Some((_, link, _)) = prefix {
            for f in &l.files {
                dep_files.push(format!("{link}/{f}"));
            }
        }
    }
    let mut std_files = 0usize;
    for (k, link, p) in &linked {
        if k.is_stdlib() {
            let fs = locate::stdlib_files(*k, p);
            std_files += fs.len();
            dep_files.extend(fs.into_iter().map(|f| format!("{link}/{f}")));
        }
    }
    dep_files.sort();
    dep_files.dedup();
    // Go: a go.sum module is a whole tree, most of which the build never
    // compiles; keep the package import closure from the repo's Go files.
    {
        let mut modules: Vec<(String, String)> = Vec::new();
        for l in &located {
            if l.dep.ecosystem != lockfiles::Ecosystem::Go {
                continue;
            }
            let Some(dir) = &l.dir else { continue };
            if let Some((_, link, _)) = linked.iter().find(|(k, _, p)| *k == RootKind::GoModCache && p.join(dir).is_dir()) {
                modules.push((l.dep.name.clone(), format!("{link}/{dir}")));
            }
        }
        let gostd = linked.iter().find(|(k, _, _)| *k == RootKind::GoStdlib).map(|(_, l, _)| l.clone());
        let repo_go: Vec<&String> = code.iter().filter(|f| f.ends_with(".go")).collect();
        if !repo_go.is_empty() && (!modules.is_empty() || gostd.is_some()) {
            let before = dep_files.iter().filter(|f| f.ends_with(".go")).count();
            let keep = go_import_closure(root, &repo_go, &dep_files, &modules, gostd.as_deref());
            dep_files.retain(|f| !f.ends_with(".go") || keep.contains(f));
            let after = dep_files.iter().filter(|f| f.ends_with(".go")).count();
            costs.insert("go_dep_files_import_closure".into(), json!({"before": before, "after": after}));
        }
    }
    costs.insert("locate_s".into(), json!(t.elapsed().as_secs_f64()));
    eprintln!("sem system: located {} locked deps, {} dependency files, in {:.1}s", locked.len(), dep_files.len(), t.elapsed().as_secs_f64());
    let dep_bytes: u64 = dep_files
        .iter()
        .filter_map(|f| std::fs::metadata(root.join(f)).ok())
        .map(|m| m.len())
        .sum();

    // ------------------------------------------------------------ base + deps builds
    eprintln!("sem system: base build over {} files", pipeline_code.len() + (code.len() - pipeline_code.len()));
    let (base_graph, mut base_entities, (base_sites, _), base_s, base_rss) =
        build_with_sites(root, &code, registry, &scratch, "base", &|_| true);
    costs.insert("base_build_s".into(), json!(base_s));
    costs.insert("base_rss_bytes".into(), json!(base_rss));
    let mut deps_input = code.clone();
    deps_input.extend(dep_files.iter().cloned());
    eprintln!("sem system: deps build over {} files ({} dependency)", deps_input.len(), dep_files.len());
    let (deps_graph, mut deps_entities, (deps_sites, closed), deps_s, deps_rss) =
        build_with_sites(root, &deps_input, registry, &scratch, "deps", &|f: &str| is_repo(f));
    // value-level data flow (the data-flow engine), per code layer
    let mut dataflow_layers: Vec<Value> = Vec::new();
    if let Some(df) = &input.dataflow {
        let is_repo_file = |f: &str| !f.starts_with(".sem-system/") && !f.contains("node_modules/");
        // the deps layer's engine input: repo code plus the dependency files
        // holding code reachable from it over resolved calls (a summary-based
        // engine never needs the rest)
        let reach_files: Vec<String> = {
            let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
            for e in &deps_graph.edges {
                if matches!(e.ref_type, RefType::Calls | RefType::Dispatch) {
                    adj.entry(e.from_entity.as_str()).or_default().push(e.to_entity.as_str());
                }
            }
            let mut seen: HashSet<&str> = HashSet::new();
            let mut q: VecDeque<&str> = deps_graph
                .entities
                .values()
                .filter(|e| is_repo(&e.file_path))
                .map(|e| e.id.as_str())
                .collect();
            while let Some(n) = q.pop_front() {
                if !seen.insert(n) {
                    continue;
                }
                for &m in adj.get(n).into_iter().flatten() {
                    if !seen.contains(m) {
                        q.push_back(m);
                    }
                }
            }
            let files: BTreeSet<&str> = seen
                .iter()
                .filter_map(|id| deps_graph.entities.get(*id).map(|e| e.file_path.as_str()))
                .collect();
            let mut v: Vec<String> = code.clone();
            v.extend(files.into_iter().filter(|f| !is_repo(f)).map(String::from));
            v
        };
        let reach_set: HashSet<&str> = reach_files.iter().map(|s| s.as_str()).collect();
        let deps_ents: Vec<crate::model::entity::SemanticEntity> =
            std::mem::take(&mut deps_entities).into_iter().filter(|e| reach_set.contains(e.file_path.as_str())).collect();
        eprintln!("sem system: data flow deps input: {} of {} files reachable from repo code", reach_files.len(), deps_input.len());
        let runs = [
            ("base", code.clone(), std::mem::take(&mut base_entities)),
            ("deps", reach_files.clone(), deps_ents),
        ];
        for (tag, files, ents) in runs {
            eprintln!("sem system: data flow over the {tag} layer ({} files)", files.len());
            let t = Instant::now();
            // on a worker thread with a budget: the engine can be slow on
            // large dependency worlds; a timed-out layer is reported, not waited on
            let (tx, rx) = std::sync::mpsc::channel();
            let f = std::sync::Arc::clone(df);
            std::thread::spawn(move || {
                let _ = tx.send(f(&files, &ents));
            });
            let Ok(j) = rx.recv_timeout(dataflow_budget()) else {
                eprintln!("sem system: data flow over the {tag} layer timed out");
                dataflow_layers.push(json!({"layer": tag, "timed_out": true, "seconds": t.elapsed().as_secs_f64()}));
                continue;
            };
            let secs = t.elapsed().as_secs_f64();
            // keep what starts in the repo: flows whose source is repo code
            let arr = |k: &str| j[k].as_array().cloned().unwrap_or_default();
            let repo_src = |v: &Value| v["source"]["file"].as_str().is_some_and(is_repo_file);
            let flows: Vec<Value> = arr("flows").into_iter().filter(|v| repo_src(v)).collect();
            let escapes: Vec<Value> = arr("escapes").into_iter().filter(|v| repo_src(v)).collect();
            let possible: Vec<Value> = arr("possibleFlows").into_iter().filter(|v| repo_src(v)).collect();
            let unknown_flows: Vec<Value> = arr("unknownFlows")
                .into_iter()
                .filter(|v| v["sink"]["file"].as_str().is_some_and(is_repo_file))
                .collect();
            let cross_boundary = flows
                .iter()
                .filter(|v| !v["sink"]["file"].as_str().is_some_and(is_repo_file))
                .count();
            let _ = std::fs::write(
                input.out.join(format!("dataflow-{tag}.json")),
                serde_json::to_string_pretty(&json!({"flows": flows, "escapes": escapes, "possibleFlows": possible, "unknownFlows": unknown_flows, "coverage": j["coverage"], "incomplete": j["incomplete"]})).unwrap_or_default(),
            );
            let known = flows.len();
            let unk = escapes.len();
            // per source site: does its data reach a sink, reach unknown code, or both
            let src_key = |v: &Value| format!("{}:{}:{}", v["source"]["file"], v["source"]["line"], v["source"]["via"]);
            let with_flow: BTreeSet<String> = flows.iter().map(src_key).collect();
            let escaping: BTreeSet<String> = escapes.iter().map(src_key).collect();
            let all_src: BTreeSet<&String> = with_flow.iter().chain(escaping.iter()).collect();
            dataflow_layers.push(json!({
                "layer": tag,
                "sources_reaching_sink_or_unknown": all_src.len(),
                "sources_with_flow": with_flow.len(),
                "sources_escaping": escaping.len(),
                "sources_fully_bounded": all_src.len() - escaping.len(),
                "flows": known,
                "flows_with_sink_in_dependency": cross_boundary,
                "escapes_to_unknown": unk,
                "flow_unknown_rate": if known + unk > 0 { json!(unk as f64 / (known + unk) as f64) } else { Value::Null },
                "possible_flows": possible.len(),
                "unknown_flows": unknown_flows.len(),
                "incomplete": j["incomplete"],
                "coverage": j["coverage"],
                "seconds": secs,
            }));
        }
    }
    drop(base_entities);
    drop(deps_entities);
    costs.insert("deps_build_s".into(), json!(deps_s));
    costs.insert("deps_rss_bytes".into(), json!(deps_rss));
    costs.insert("dep_source_bytes".into(), json!(dep_bytes));
    let ix = EntIndex::build(&deps_graph);
    let base_ids: HashSet<&str> = base_graph.entities.keys().map(|k| k.as_str()).collect();

    // ------------------------------------------------------------ scan repo code
    let t = Instant::now();
    let mut scans: BTreeMap<String, (Lang, FileScan, Vec<usize>)> = BTreeMap::new();
    for f in &code {
        let Ok(src) = std::fs::read_to_string(root.join(f)) else { continue };
        if src.len() > 2_000_000 {
            continue;
        }
        if let Some((lang, s)) = scan::scan_file(f, &src) {
            scans.insert(f.clone(), (lang, s, line_starts(&src)));
        }
    }
    costs.insert("scan_s".into(), json!(t.elapsed().as_secs_f64()));
    eprintln!("sem system: scanned {} files in {:.1}s", scans.len(), t.elapsed().as_secs_f64());

    // ------------------------------------------------------------ call sites
    let mut calls: Vec<CallSite> = Vec::new();
    let mut deps_by_key: HashMap<(String, usize), (Outcome, Vec<String>)> = HashMap::new();
    // `closed`: outcome counts of the sites inside dependency code
    for (f, at, call, o, defs) in &deps_sites {
        if *call && is_repo(f) {
            deps_by_key.insert((f.clone(), *at), (o.clone(), defs.clone()));
        }
    }
    let callee_at = |f: &str, at: usize| -> String {
        scans
            .get(f)
            .and_then(|(_, s, _)| {
                s.calls
                    .iter()
                    .filter(|c| c.fn_span.0 <= at && at < c.fn_span.1.max(c.fn_span.0 + 1))
                    .min_by_key(|c| c.fn_span.1 - c.fn_span.0)
                    .map(|c| c.callee.clone())
            })
            .unwrap_or_default()
    };
    for (f, at, call, o, defs) in &base_sites {
        if !call || !is_repo(f) {
            continue;
        }
        let Some((_, _, starts)) = scans.get(f) else { continue };
        let line = line_of(starts, *at);
        let (dout, ddefs) = deps_by_key.get(&(f.clone(), *at)).cloned().unwrap_or((o.clone(), defs.clone()));
        calls.push(CallSite {
            file: f.clone(),
            _at: *at,
            line,
            lang: Lang::of(f).unwrap_or(Lang::Py),
            caller: Some(ix.owner(f, line)),
            base: o.clone(),
            deps: dout,
            targets_base: defs.clone(),
            targets_deps: ddefs,
            callee: callee_at(f, *at),
            later: Vec::new(),
        });
    }
    // TypeScript/JavaScript front-end
    let mut ts_note = "no TypeScript front-end sites given".to_string();
    if let Some((tb, td)) = &input.ts_sites {
        let base_ts = read_ts_sites(tb);
        let deps_ts: HashMap<(String, usize, String), TsSite> =
            read_ts_sites(td).into_iter().map(|s| ((s.file.clone(), s.at, s.callee.clone()), s)).collect();
        let decl = |d: &(String, usize, String)| -> Node {
            if is_repo(&d.0) && !d.0.contains("node_modules/") {
                if let Some(e) = ix.fn_at(&d.0, d.1, Some(&d.2)) {
                    return e.id.clone();
                }
            }
            format!("decl:{}:{}:{}", d.0, d.1, d.2)
        };
        let n = base_ts.len();
        for s in base_ts {
            if is_test_path(&s.file) || !code.contains(&s.file) {
                continue;
            }
            // a call chain `a().b().c()` starts every member at one offset: key by callee too
            let d = deps_ts.get(&(s.file.clone(), s.at, s.callee.clone()));
            let (dout, ddefs) = match d {
                Some(d) => (d.outcome.clone(), d.defs.iter().map(decl).collect()),
                None => (s.outcome.clone(), s.defs.iter().map(decl).collect()),
            };
            calls.push(CallSite {
                file: s.file.clone(),
                _at: s.at,
                line: s.line,
                lang: Lang::of(&s.file).unwrap_or(Lang::Ts),
                caller: Some(ix.owner(&s.file, s.line)),
                base: s.outcome.clone(),
                deps: dout,
                targets_base: s.defs.iter().map(decl).collect(),
                targets_deps: ddefs,
                callee: s.callee.clone(),
                later: Vec::new(),
            });
        }
        ts_note = format!("{n} TypeScript/JavaScript front-end sites read");
    }

    // tests inside source files (Rust `mod tests`) are test code too
    calls.retain(|c| !c.caller.as_deref().is_some_and(is_test_entity));

    // ------------------------------------------------------------ db
    let t = Instant::now();
    let mut schema = Schema::default();
    let mut sql_files = Vec::new();
    for f in nontest.iter().filter(|f| f.ends_with(".sql")) {
        if let Ok(text) = std::fs::read_to_string(root.join(f)) {
            schema.apply_sql(f, &text);
            sql_files.push(f.to_string());
        }
    }
    for d in &input.schema_dumps {
        if let Ok(text) = std::fs::read_to_string(d) {
            schema.apply_sql(&d.to_string_lossy(), &text);
            sql_files.push(d.to_string_lossy().to_string());
        }
    }
    // Prisma models
    let mut model_tables: BTreeMap<String, String> = BTreeMap::new(); // model/class name -> table
    for f in nontest.iter().filter(|f| f.ends_with(".prisma")) {
        let text = std::fs::read_to_string(root.join(f)).unwrap_or_default();
        let re = regex::Regex::new(r"(?s)\bmodel\s+(\w+)\s*\{(.*?)\n\}").unwrap();
        for c in re.captures_iter(&text) {
            let name = c[1].to_string();
            let table = regex::Regex::new(r#"@@map\("([^"]+)"\)"#)
                .unwrap()
                .captures(&c[2])
                .map(|m| m[1].to_string())
                .unwrap_or_else(|| name.clone());
            schema.add_table(&table, f);
            model_tables.insert(name, sql::norm(&table));
        }
    }
    // ORM classes and migration ops from code
    for (f, (lang, s, _)) in &scans {
        for c in &s.classes {
            let attr = |k: &str| c.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
            let table = match lang {
                Lang::Py => attr("__tablename__").or_else(|| attr("Meta.db_table")).or_else(|| {
                    let sqlmodel = c.header_kw.iter().any(|(k, v)| k == "table" && v == "True");
                    let django = c.bases.iter().any(|b| b.ends_with("models.Model"));
                    // Flask-SQLAlchemy: `class User(db.Model)` → snake_case name
                    let flask_sqla = c.bases.iter().any(|b| b == "db.Model" || b.ends_with(".db.Model"));
                    if sqlmodel {
                        Some(c.name.to_ascii_lowercase())
                    } else if flask_sqla {
                        Some(snake(&c.name))
                    } else if django {
                        let app = f.split('/').rev().nth(1).unwrap_or("app");
                        Some(format!("{app}_{}", c.name.to_ascii_lowercase()))
                    } else {
                        None
                    }
                }),
                Lang::Go => attr("TableName").or_else(|| {
                    c.bases.iter().any(|b| b == "gorm.Model").then(|| snake_plural(&c.name))
                }),
                Lang::Ts | Lang::Js => c.decorators.iter().find(|(d, _)| d == "Entity").map(|(_, n)| {
                    n.clone().unwrap_or_else(|| c.name.to_ascii_lowercase())
                }),
                Lang::Rs => None,
            };
            if let Some(t) = table {
                schema.add_table(&t, f);
                model_tables.insert(c.name.clone(), sql::norm(&t));
            }
        }
        // alembic / django migrations: op.create_table("x", ...)
        for call in &s.calls {
            if call.callee.ends_with("op.create_table") || call.callee.ends_with("migrations.CreateModel") {
                if let Some(n) = call.args.first().and_then(|a| a.literal()) {
                    schema.add_table(n, f);
                }
            }
            // GORM AutoMigrate(&X{}) without TableName: snake plural
            if call.callee.ends_with(".AutoMigrate") {
                for a in &call.args {
                    if let Arg::Expr(e) = a {
                        let ty = e.trim_start_matches('&').split('{').next().unwrap_or("");
                        let ty = ty.rsplit('.').next().unwrap_or(ty);
                        if !ty.is_empty() && !model_tables.contains_key(ty) {
                            let t = snake_plural(ty);
                            schema.add_table(&t, f);
                            model_tables.insert(ty.to_string(), t);
                        }
                    }
                }
            }
        }
    }
    costs.insert("db_s".into(), json!(t.elapsed().as_secs_f64()));

    // ------------------------------------------------------------ config + contracts
    let t = Instant::now();
    let nontest_owned: Vec<String> = nontest.iter().map(|s| s.to_string()).collect();
    let cfg: ConfigFacts = config::collect(root, &nontest_owned);
    costs.insert("config_s".into(), json!(t.elapsed().as_secs_f64()));
    let t = Instant::now();
    let ctr: Contracts = contracts::collect(root, &nontest_owned);
    costs.insert("contracts_s".into(), json!(t.elapsed().as_secs_f64()));

    // ------------------------------------------------------------ boundary sites
    let rpc_names: HashMap<String, Vec<&contracts::Rpc>> = {
        let mut m: HashMap<String, Vec<&contracts::Rpc>> = HashMap::new();
        for r in &ctr.rpc {
            m.entry(r.method.to_ascii_lowercase()).or_default().push(r);
        }
        m
    };
    let op_ids: HashMap<String, &contracts::HttpOp> = ctr
        .http
        .iter()
        .filter_map(|o| {
            o.operation_id.as_ref().map(|id| {
                // drop a `tag-` prefix and separators: `users-delete_user` -> `deleteuser`
                let id = id.rsplit_once('-').map(|(_, r)| r).unwrap_or(id);
                (id.replace('_', "").to_ascii_lowercase(), o)
            })
        })
        .collect();
    let mut bsites: Vec<BSite> = Vec::new();
    for (f, (lang, s, _)) in &scans {
        let mut found = models::match_file(&input.models, f, *lang, s);
        // http_out via an object argument `{ url: '/x' }` (generated TS clients)
        for b in found.iter_mut().filter(|b| b.kind == Kind::HttpOut && b.key.is_none() && b.fragments.is_none()) {
            if let Some(c) = s.calls.iter().find(|c| c.byte == b.byte) {
                for a in &c.args {
                    if let Arg::Expr(e) = a {
                        if let Some(u) = regex::Regex::new(r#"url:['"`]([^'"`]+)['"`]"#).unwrap().captures(e) {
                            b.key = Some(u[1].to_string());
                        }
                    }
                }
            }
        }
        let seen: HashSet<usize> = found.iter().map(|b| b.byte).collect();
        // contract-driven detection: rpc stubs and generated OpenAPI clients
        if !is_generated(f) {
            for c in &s.calls {
                if seen.contains(&c.byte) || c.decorates.is_some() {
                    continue;
                }
                let seg = c.callee.trim_end_matches("()").rsplit(['.', ':']).next().unwrap_or("");
                let recv = c.callee.to_ascii_lowercase();
                // a generated client/stub; `service`-named receivers are too often the app's own ports
                let recv_ok = recv.contains("client") || recv.contains("stub");
                let seg_l = seg.to_ascii_lowercase();
                if recv_ok && rpc_names.contains_key(&seg_l) && c.callee.contains('.') {
                    found.push(BoundarySite {
                        file: f.clone(),
                        byte: c.byte,
                        line: c.line,
                        lang: *lang,
                        kind: Kind::RpcOut,
                        text: c.callee.clone(),
                        key: Some(seg.to_string()),
                        fragments: None,
                        method: None,
                        handler: None,
                        orm: false,
                        orm_text: None,
                        prefix: None,
                    });
                    continue;
                }
                let stripped = seg_l
                    .trim_end_matches("withresponse")
                    .trim_end_matches("withbody")
                    .to_string();
                // generated clients: `UsersService.deleteUser` for operationId
                // `users-delete_user`, `client.GetTrainerCalendar` for `getTrainerCalendar`
                let generated_recv = recv.contains("client") || recv.contains("service") || recv.contains("api");
                if c.callee.contains('.') && op_ids.contains_key(&stripped) && generated_recv {
                    found.push(BoundarySite {
                        file: f.clone(),
                        byte: c.byte,
                        line: c.line,
                        lang: *lang,
                        kind: Kind::HttpOut,
                        text: c.callee.clone(),
                        key: Some(format!("operationId:{stripped}")),
                        fragments: None,
                        method: None,
                        handler: None,
                        orm: false,
                        orm_text: None,
                        prefix: None,
                    });
                }
            }
        }
        for b in found {
            let caller = Some(ix.owner(f, b.line));
            bsites.push(BSite { s: b, caller, resolved: None, why: String::new(), writes: false });
        }
    }
    // SQL literals not passed to a matched call (query strings in constants)
    let matched_bytes: HashSet<(String, usize)> = bsites.iter().map(|b| (b.s.file.clone(), b.s.byte)).collect();
    for (f, (lang, s, _)) in &scans {
        let covered: Vec<(usize, usize)> = s
            .calls
            .iter()
            .filter(|c| matched_bytes.contains(&(f.clone(), c.byte)))
            .map(|c| (c.byte, c.end))
            .collect();
        for lit in &s.strings {
            if !sql::is_sql(&lit.text) || covered.iter().any(|(a, b)| *a <= lit.byte && lit.byte < *b) {
                continue;
            }
            let caller = Some(ix.owner(f, lit.line));
            bsites.push(BSite {
                s: BoundarySite {
                    file: f.clone(),
                    byte: lit.byte,
                    line: lit.line,
                    lang: *lang,
                    kind: Kind::Sql,
                    text: "<sql literal>".into(),
                    key: (!lit.interpolated).then(|| lit.text.clone()),
                    fragments: lit.interpolated.then(|| lit.text.clone()),
                    method: None,
                    handler: None,
                    orm: false,
                    orm_text: None,
                    prefix: None,
                },
                caller,
                resolved: None,
                why: String::new(),
                writes: false,
            });
        }
    }

    bsites.retain(|b| !b.caller.as_deref().is_some_and(is_test_entity));

    // ------------------------------------------------------------ routes (config layer)
    struct Route {
        method: String,
        path: String,
        segs: Vec<String>,
        handler: Vec<String>,
    }
    // Router prefixes: `router = APIRouter(prefix="/items")`, `v1 := r.Group("/api")`,
    // and mounts `include_router(x.router, prefix="/p")`, `app.use("/p", router)`.
    const ROUTER_CTORS: &[&str] = &["APIRouter", "Blueprint", "Group", "Router", "Scope", "scope", "PathPrefix", "nest"];
    const MOUNTS: &[&str] = &["include_router", "register_blueprint", "use", "Mount", "mount", "nest", "Route", "service"];
    let kw = |c: &scan::CallFact, keys: &[&str]| {
        c.args.iter().find_map(|a| match a {
            Arg::Kw(k, v) if keys.contains(&k.as_str()) => v.literal().map(String::from),
            _ => None,
        })
    };
    let first_str = |c: &scan::CallFact| c.args.iter().find_map(|a| a.literal().map(String::from));
    let mut ctor_prefix: HashMap<(String, String), String> = HashMap::new();
    let mut mounts: Vec<(String, Option<String>, String)> = Vec::new(); // (target var, qualifier, prefix)
    for (f, (_, s, _)) in &scans {
        for c in &s.calls {
            let seg = c.callee.trim_end_matches("()").rsplit(['.', ':']).next().unwrap_or("");
            if let Some(v) = &c.assign {
                if ROUTER_CTORS.contains(&seg) {
                    let p = kw(c, &["prefix", "url_prefix"]).or_else(|| {
                        matches!(seg, "Group" | "Scope" | "scope" | "PathPrefix").then(|| first_str(c)).flatten()
                    });
                    if let Some(p) = p {
                        ctor_prefix.insert((f.clone(), v.clone()), p);
                    }
                }
            }
            if MOUNTS.contains(&seg) {
                let p = kw(c, &["prefix", "url_prefix"]).or_else(|| first_str(c).filter(|p| p.starts_with('/')));
                let target = c.args.iter().find_map(|a| match a {
                    Arg::Name(n) => Some(n.clone()),
                    _ => None,
                });
                if let (Some(p), Some(t)) = (p, target) {
                    let (qual, last) = match t.rsplit_once(['.', ':']) {
                        Some((q, l)) => (Some(q.rsplit(['.', ':']).next().unwrap_or(q).to_string()), l.to_string()),
                        None => (None, t.clone()),
                    };
                    mounts.push((last, qual, p));
                }
            }
        }
    }
    let route_prefix = |file: &str, callee: &str| -> String {
        let recv = callee.trim_end_matches("()").rsplit_once(['.', ':']).map(|(r, _)| r).unwrap_or("");
        let var = recv.rsplit(['.', ':']).next().unwrap_or(recv).to_string();
        if var.is_empty() {
            return String::new();
        }
        let stem = file.rsplit('/').next().unwrap_or(file).split('.').next().unwrap_or("");
        let dir = file.rsplit('/').nth(1).unwrap_or("");
        let mount = mounts
            .iter()
            .find(|(t, q, _)| *t == var && q.as_deref().is_none_or(|q| q == stem || q == dir))
            .map(|(_, _, p)| p.clone())
            .unwrap_or_default();
        let own = ctor_prefix.get(&(file.to_string(), var)).cloned().unwrap_or_default();
        format!("{}{}", mount.trim_end_matches('/'), own.trim_end_matches('/'))
    };
    let mut routes: Vec<Route> = Vec::new();
    for b in bsites.iter_mut().filter(|b| b.s.kind == Kind::RouteIn) {
        let path = format!(
            "{}{}{}",
            route_prefix(&b.s.file, &b.s.text),
            b.s.prefix.as_deref().map(|p| format!("/{}", p.trim_matches('/'))).unwrap_or_default(),
            b.s.key.as_deref().map(|k| if k.starts_with('/') || k.is_empty() { k.to_string() } else { format!("/{k}") }).unwrap_or_default()
        );
        let handler: Vec<String> = match &b.s.handler {
            Some(h) => {
                // decorated: the entity at the decorated line
                let named = ix.named(h, &b.s.file, &is_repo);
                if named.len() > 8 {
                    Vec::new()
                } else {
                    named
                }
            }
            None => Vec::new(),
        };
        if handler.is_empty() {
            // an inline handler (closure / arrow): its body belongs to the
            // registering entity (or module), which then serves the route
            if let (Some(c), true) = (&b.caller, b.s.handler.is_none()) {
                b.resolved = Some((CONFIG, vec![c.clone()]));
                routes.push(Route {
                    method: b.s.method.clone().unwrap_or_else(|| "ANY".into()),
                    segs: path_segments(&path),
                    path: path.clone(),
                    handler: vec![c.clone()],
                });
            } else {
                b.why = "handler not found".into();
            }
            continue;
        }
        b.resolved = Some((CONFIG, handler.iter().map(|h| h.to_string()).collect()));
        routes.push(Route {
            method: b.s.method.clone().unwrap_or_else(|| "ANY".into()),
            segs: path_segments(&path),
            path,
            handler,
        });
    }

    // ------------------------------------------------------------ hookimpls / registries / tables
    let mut hookimpls: HashMap<String, Vec<String>> = HashMap::new();
    let mut registrations: Vec<(String, Option<String>, Vec<String>)> = Vec::new(); // (registry segment, key, fn ids)
    for (f, (lang, s, _)) in &scans {
        for c in &s.calls {
            if let Some((name, line)) = &c.decorates {
                if c.callee.contains("hookimpl") {
                    if let Some(e) = ix.fn_at(f, *line, Some(name)) {
                        hookimpls.entry(name.clone()).or_default().push(e.id.clone());
                    }
                }
            }
            for r in input.models.registry.iter().filter(|r| r.langs.iter().any(|l| l == lang.as_str())) {
                if !r.register.iter().any(|p| models::glob(p, &c.callee)) {
                    continue;
                }
                let key = c.args.get(r.key_arg).and_then(|a| a.literal()).map(String::from);
                let val = match &r.value_arg {
                    models::ArgSel::Index(i) => c.args.get(*i),
                    models::ArgSel::Named(_) => c.args.last(),
                };
                let fns: Vec<String> = match val {
                    // a registered name must be unambiguous: one definition, preferring this file
                    Some(Arg::Name(n)) => {
                        let v = ix.named(n, f, &is_repo);
                        let callable: Vec<String> = v
                            .into_iter()
                            .filter(|id| ["::function::", "::method::", "::class::"].iter().any(|k| id.contains(k)))
                            .collect();
                        if callable.len() == 1 { callable } else { Vec::new() }
                    }
                    // `register('x', require('./y'))`: the module's top-level functions
                    Some(Arg::Expr(e)) if e.starts_with("require(") => {
                        let spec: String = e.trim_start_matches("require(").trim_matches(|c| c == '\'' || c == '"' || c == ')').to_string();
                        let dir = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                        let target = normalize_rel(dir, &spec);
                        let cand = ["", ".ts", ".js", "/index.ts", "/index.js"]
                            .iter()
                            .map(|x| format!("{target}{x}"))
                            .find(|p| ix.by_file.contains_key(p));
                        match cand {
                            Some(p) => {
                                let tops: Vec<String> = ix.by_file[&p]
                                    .iter()
                                    .filter(|e| e.ty == "function" && !ix.by_file[&p].iter().any(|o| o.start < e.start && e.end <= o.end && o.id != e.id))
                                    .map(|e| e.id.clone())
                                    .collect();
                                if tops.is_empty() || tops.len() > 3 { vec![format!("module:{p}")] } else { tops }
                            }
                            None => Vec::new(),
                        }
                    }
                    // an inline function: the registering entity holds its body
                    Some(Arg::Expr(e)) if e.contains("=>") || e.starts_with("function") || e.starts_with("func") => {
                        vec![ix.owner(f, c.line)]
                    }
                    _ => Vec::new(),
                };
                if fns.is_empty() || fns.len() > 8 {
                    continue;
                }
                let recv = c.callee.trim_end_matches("()");
                let recv = recv.rsplit_once(['.', ':']).map(|(a, _)| a).unwrap_or("");
                let seg = recv.rsplit(['.', ':']).next().unwrap_or(recv).to_string();
                registrations.push((seg, key, fns));
            }
        }
    }
    let manifest_names: HashSet<String> = cfg
        .manifests
        .iter()
        .flat_map(|m| m.entries.iter().map(|(n, _)| n.split(':').next_back().unwrap_or(n).to_string()))
        .collect();
    // tables: name -> function ids
    let mut tables: HashMap<(String, String), Vec<String>> = HashMap::new(); // (file, name) -> ids
    let mut tables_by_name: HashMap<String, Vec<Vec<String>>> = HashMap::new();
    for (f, (_, s, _)) in &scans {
        for t in &s.tables {
            let ids: Vec<String> = t
                .entries
                .iter()
                .filter_map(|(_, v)| v.as_ref())
                .flat_map(|v| {
                    let n = ix.named(v, f, &is_repo);
                    let callable = n.len() == 1
                        && ["::function::", "::method::", "::class::", "::struct::"].iter().any(|k| n[0].contains(k))
                        && !n[0].contains("::property::");
                    if callable { n } else { Vec::new() }
                })
                .collect();
            if ids.is_empty() {
                continue;
            }
            tables.insert((f.clone(), t.name.clone()), ids.clone());
            tables_by_name.entry(t.name.rsplit('.').next().unwrap_or(&t.name).to_string()).or_default().push(ids);
        }
    }
    let table_candidates = |file: &str, callee: &str| -> Option<Vec<String>> {
        // `NAME[k]`, `NAME.get(k)`, `self.NAME[k]`
        // the call must be the lookup itself: `NAME[k](..)` / `NAME.get(k)(..)`
        if !(callee.ends_with(']') || callee.ends_with(".get()")) {
            return None;
        }
        let base = callee.split('[').next().unwrap_or(callee);
        let base = base.trim_end_matches(".get()");
        let name = base.rsplit('.').next().unwrap_or(base);
        if let Some(ids) = tables.get(&(file.to_string(), name.to_string())) {
            return Some(ids.clone());
        }
        match tables_by_name.get(name) {
            Some(v) if v.len() == 1 => Some(v[0].clone()),
            _ => None,
        }
    };
    // config layer: call sites through dispatch tables
    let mut table_resolved = 0usize;
    for c in calls.iter_mut() {
        if c.outcome(CONTRACTS).known() {
            continue;
        }
        if let Some(ids) = table_candidates(&c.file, &c.callee) {
            c.later.push((CONFIG, Outcome::R, ids));
            table_resolved += 1;
        }
    }

    // ------------------------------------------------------------ boundary outcomes
    let env = &cfg.env;
    let entity_mentions = |id: &Option<String>, names: &BTreeMap<String, String>| -> Vec<String> {
        let Some(id) = id else { return Vec::new() };
        let Some(e) = deps_graph.entities.get(id.as_str()) else { return Vec::new() };
        let Ok(src) = std::fs::read_to_string(root.join(&e.file_path)) else { return Vec::new() };
        let lines: Vec<&str> = src.lines().collect();
        let body = lines
            .get(e.start_line.saturating_sub(1)..e.end_line.min(lines.len()))
            .map(|l| l.join("\n"))
            .unwrap_or_default();
        names
            .iter()
            .filter(|(n, _)| word_in(&body, n))
            .map(|(_, t)| t.clone())
            .collect()
    };
    let mut rpc_impls: HashMap<String, Vec<String>> = HashMap::new();
    for r in &ctr.rpc {
        let key = r.method.to_ascii_lowercase();
        if rpc_impls.contains_key(&key) {
            continue;
        }
        // server implementations: methods named like the rpc outside generated code
        let mut ids: Vec<String> = Vec::new();
        for cand in [r.method.clone(), lower_camel(&r.method)] {
            if let Some(v) = ix.by_name.get(&cand) {
                ids.extend(v.iter().filter(|(_, f)| is_repo(f) && !is_generated(f) && !is_test_path(f)).map(|(i, _)| i.clone()));
            }
        }
        // functions/methods only, not mocks or build-tagged fuzz harnesses
        ids.retain(|id| {
            let l = id.to_ascii_lowercase();
            let file = l.split("::").next().unwrap_or("");
            (l.contains("::function::") || l.contains("::method::") || l.contains("::type::") || l.contains("::class::") || l.contains("::impl::"))
                && !file.contains("mock")
                && !file.contains("fuzz")
        });
        // prefer the server side when there is one: an owner named like a server
        let server: Vec<String> = ids
            .iter()
            .filter(|id| {
                let owner = id.rsplit("::").nth(1).unwrap_or("").to_ascii_lowercase();
                ["server", "servicer", "impl", "handler"].iter().any(|k| owner.contains(k))
            })
            .cloned()
            .collect();
        if !server.is_empty() {
            ids = server;
        }
        ids.sort();
        ids.dedup();
        rpc_impls.insert(key, ids);
    }
    for b in bsites.iter_mut() {
        if b.s.kind == Kind::RouteIn {
            continue;
        }
        let key = b.s.key.clone();
        match b.s.kind {
            Kind::Sql => {
                let text = key.clone().or_else(|| b.s.fragments.clone());
                let lower = text.as_deref().unwrap_or("").trim_start().to_ascii_lowercase();
                b.writes = ["insert", "update", "delete", "replace", "create", "alter", "drop", "merge", "upsert"]
                    .iter()
                    .any(|k| lower.starts_with(k));
                if b.s.orm {
                    let t = b.s.orm_text.clone().unwrap_or_default();
                    let seg = b.s.text.rsplit(['.', ':']).next().unwrap_or("").to_ascii_lowercase();
                    b.writes = ["add", "add_all", "delete", "merge", "create", "save", "update", "updates", "insert", "remove", "upsert", "firstorcreate", "automigrate"]
                        .iter()
                        .any(|k| seg.starts_with(k));
                    let mut tabs: Vec<String> = model_tables
                        .iter()
                        .filter(|(n, _)| word_in(&t, n) || t.contains(&format!("prisma.{}.", lower_camel(n))))
                        .map(|(_, t)| t.clone())
                        .collect();
                    if tabs.is_empty() {
                        tabs = entity_mentions(&b.caller, &model_tables);
                    }
                    tabs.sort();
                    tabs.dedup();
                    if tabs.is_empty() {
                        b.why = "ORM model not identified".into();
                    } else {
                        b.resolved = Some((DB, tabs.into_iter().map(|t| format!("table:{t}")).collect()));
                    }
                } else if let Some(sqltext) = key.as_deref() {
                    let refs = sql::table_refs(sqltext);
                    let routines = sql::routine_calls(sqltext, &schema);
                    let known: Vec<String> = refs.iter().filter(|t| schema.has_relation(t)).cloned().collect();
                    if refs.is_empty() && routines.is_empty() {
                        b.why = "no table in SQL".into();
                    } else if known.len() < refs.len() {
                        b.why = "table not in schema".into();
                    } else {
                        let mut tg: Vec<String> = known.into_iter().map(|t| format!("table:{t}")).collect();
                        tg.extend(routines.into_iter().map(|r| format!("routine:{r}")));
                        b.resolved = Some((DB, tg));
                    }
                } else if b.s.fragments.is_some() {
                    b.why = "SQL built at runtime".into();
                } else {
                    b.why = "SQL not a literal".into();
                }
            }
            Kind::Env => match key {
                Some(k) if env.defined(&k) => b.resolved = Some((CONFIG, vec![format!("env:{k}")])),
                Some(_) => b.why = "env key not defined in repo config".into(),
                None => b.why = "env key not a literal".into(),
            },
            Kind::Dispatch => {
                let seg = b.s.text.rsplit(['.', ':']).next().unwrap_or("").trim_end_matches("()").to_string();
                if b.s.text.contains(".hook.") {
                    match hookimpls.get(&seg) {
                        Some(ids) if !ids.is_empty() => b.resolved = Some((CONFIG, ids.clone())),
                        _ => b.why = "no hookimpl in repo".into(),
                    }
                } else if let Some(k) = &key {
                    // a literal module / attribute name
                    let ids = ix.named(k, &b.s.file, &is_repo);
                    if !ids.is_empty() && ids.len() <= 8 {
                        b.resolved = Some((CONFIG, ids));
                    } else if manifest_names.contains(k) {
                        b.resolved = Some((CONFIG, vec![format!("plugin:{k}")]));
                    } else {
                        b.why = "name not in repo".into();
                    }
                } else {
                    b.why = "name computed at runtime".into();
                }
            }
            Kind::HttpOut => {
                let url = key.clone().or_else(|| b.s.fragments.clone());
                let op = url.as_deref().and_then(|u| u.strip_prefix("operationId:")).and_then(|id| op_ids.get(id));
                let segs = match (op, url.as_deref()) {
                    (Some(o), _) => path_segments(&o.path),
                    (None, Some(u)) => path_segments(&u.replace("${", "{").replace(' ', "")),
                    _ => Vec::new(),
                };
                if segs.is_empty() {
                    b.why = if url.is_none() { "URL not a literal".into() } else { "URL has no path".into() };
                } else {
                    let method = op.map(|o| o.method.clone()).or_else(|| b.s.method.clone());
                    let mut best: Vec<(usize, &Route)> = routes
                        .iter()
                        .filter(|r| method.as_deref().is_none_or(|m| r.method == "ANY" || m == "REQUEST" || r.method.contains(m)))
                        .filter_map(|r| path_match(&segs, &r.segs).map(|n| (n, r)))
                        .collect();
                    let top = best.iter().map(|(n, _)| *n).max().unwrap_or(0);
                    best.retain(|(n, _)| *n == top);
                    if !best.is_empty() && best.len() <= 4 {
                        b.resolved = Some((CONTRACTS, best.iter().flat_map(|(_, r)| r.handler.clone()).collect()));
                    } else if op.is_some() || ctr.http.iter().any(|o| path_match(&segs, &path_segments(&o.path)).is_some()) {
                        b.why = "contract op without a handler in the repo".into();
                    } else if url.as_deref().is_some_and(|u| u.starts_with("http")) {
                        b.why = "third-party host".into();
                    } else {
                        b.why = "no matching route or contract".into();
                    }
                }
            }
            Kind::RpcOut => {
                let k = key.clone().unwrap_or_default().to_ascii_lowercase();
                // the calling wrapper is not its own server
                let ids: Vec<String> = rpc_impls
                    .get(&k)
                    .map(|v| v.iter().filter(|i| Some(*i) != b.caller.as_ref()).cloned().collect())
                    .unwrap_or_default();
                match (rpc_impls.contains_key(&k), ids.len()) {
                    (false, _) => b.why = "not a known rpc".into(),
                    (true, 0) => b.why = "rpc served outside the repo's front-ends".into(),
                    (true, n) if n <= 6 => b.resolved = Some((CONTRACTS, ids)),
                    (true, _) => b.why = "rpc implementation ambiguous by name".into(),
                }
            }
            Kind::Queue => b.why = "no topic contract".into(),
            Kind::Subprocess => {
                let prog = key.as_deref().unwrap_or("").split_whitespace().next().unwrap_or("").to_string();
                let in_repo = !prog.is_empty() && all_files.iter().any(|f| f.ends_with(prog.trim_start_matches("./")));
                if in_repo {
                    b.resolved = Some((CONFIG, vec![format!("file:{prog}")]));
                } else {
                    b.why = if key.is_some() { "external program".into() } else { "command not a literal".into() };
                }
            }
            Kind::Fs => match key {
                Some(p) => b.resolved = Some((CONFIG, vec![format!("path:{p}")])),
                None => b.why = "path not a literal".into(),
            },
            Kind::RouteIn => {}
        }
    }
    // registries: lookups with a key, registered in the repo
    let mut registry_resolved = 0usize;
    let mut reg_calls_at: HashMap<(String, usize), Vec<usize>> = HashMap::new();
    for (i, c) in calls.iter().enumerate() {
        reg_calls_at.entry((c.file.clone(), c.line)).or_default().push(i);
    }
    for (f, (lang, s, starts)) in &scans {
        for c in &s.calls {
            for r in input.models.registry.iter().filter(|r| r.langs.iter().any(|l| l == lang.as_str())) {
                if !r.lookup.iter().any(|p| models::glob(p, &c.callee)) {
                    continue;
                }
                let key = c.args.get(r.key_arg).and_then(|a| a.literal());
                let recv = c.callee.trim_end_matches("()");
                let recv = recv.rsplit_once(['.', ':']).map(|(a, _)| a).unwrap_or("");
                let seg = recv.rsplit(['.', ':']).next().unwrap_or(recv);
                // a keyless lookup names the whole registry: only for a
                // registry named by a real word, not a one-letter local
                if key.is_none() && seg.len() < 3 {
                    continue;
                }
                let cands: Vec<String> = registrations
                    .iter()
                    .filter(|(s2, k2, _)| s2 == seg && (key.is_none() || k2.as_deref() == key))
                    .flat_map(|(_, _, ids)| ids.clone())
                    .collect();
                if cands.is_empty() {
                    continue;
                }
                // the call pipeline's site at this call, if it was unknown
                let line = line_of(starts, c.byte);
                for &i in reg_calls_at.get(&(f.clone(), line)).into_iter().flatten() {
                    let cs = &mut calls[i];
                    if !cs.outcome(CONTRACTS).known() {
                        cs.later.push((CONFIG, Outcome::R, cands.clone()));
                    }
                }
                bsites.push(BSite {
                    s: BoundarySite {
                        file: f.clone(),
                        byte: c.byte,
                        line,
                        lang: *lang,
                        kind: Kind::Dispatch,
                        text: c.callee.clone(),
                        key: key.map(String::from),
                        fragments: None,
                        method: None,
                        handler: None,
                        orm: false,
                        orm_text: None,
                        prefix: None,
                    },
                    caller: Some(ix.owner(f, line)),
                    resolved: Some((CONFIG, cands)),
                    why: String::new(),
                    writes: false,
                });
                registry_resolved += 1;
                break;
            }
        }
    }

    eprintln!("sem system: {} call sites, {} boundary sites (t={:.1}s)", calls.len(), bsites.len(), t_all.elapsed().as_secs_f64());
    // ------------------------------------------------------------ dynamic
    let mut dyn_summary = json!({"trace": null});
    let mut observed_pairs: HashSet<(String, String)> = HashSet::new();
    let mut executed: HashSet<(String, usize)> = HashSet::new();
    // (caller, callee, callee_in_repo, explicit: a static call site is on the observed site line)
    let mut obs_edges_mapped: Vec<(String, String, bool, bool)> = Vec::new();
    let call_lines: HashSet<(String, usize)> = calls.iter().map(|c| (c.file.clone(), c.line)).collect();
    let mut calls_at: HashMap<(String, usize), Vec<usize>> = HashMap::new();
    let mut calls_by_caller: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in calls.iter().enumerate() {
        calls_at.entry((c.file.clone(), c.line)).or_default().push(i);
        if let Some(cl) = &c.caller {
            calls_by_caller.entry(cl.clone()).or_default().push(i);
        }
    }
    if let Some(tp) = &input.trace {
        let (obs, lines) = read_trace(tp);
        executed = lines;
        let mut unmapped = 0usize;
        for o in &obs {
            if !is_repo(&o.caller_file) || is_test_path(&o.caller_file) {
                continue;
            }
            let caller = if o.site_line > 0 {
                Some(ix.owner(&o.caller_file, o.site_line))
            } else {
                ix.fn_at(&o.caller_file, o.caller_line, Some(&o.caller)).map(|e| e.id.clone())
            };
            let callee = ix.fn_at(&o.callee_file, o.callee_line, Some(&o.callee)).map(|e| e.id.clone());
            match (caller, callee) {
                // test callers, and calls within one entity (closures, recursion
                // at entity granularity), are not edges of the system graph
                (Some(a), Some(_)) if is_test_entity(&a) => {}
                (Some(a), Some(b)) if a == b => {}
                (Some(a), Some(b)) => {
                    observed_pairs.insert((a.clone(), b.clone()));
                    let explicit = o.site_line > 0 && call_lines.contains(&(o.caller_file.clone(), o.site_line));
                    obs_edges_mapped.push((a, b, is_repo(&o.callee_file), explicit));
                }
                _ => unmapped += 1,
            }
            // unresolved static sites on this line calling this name become observed
            let callee_seg = o.callee.rsplit(['.', ':']).next().unwrap_or(&o.callee).to_string();
            let target = ix.fn_at(&o.callee_file, o.callee_line, Some(&o.callee)).map(|e| e.id.clone());
            // sampled traces have no site line: match the caller entity's sites by callee name
            let caller_id = if o.site_line == 0 {
                ix.fn_at(&o.caller_file, o.caller_line, Some(&o.caller)).map(|e| e.id.clone())
            } else {
                None
            };
            let on_line: Vec<usize> = if o.site_line > 0 {
                calls_at.get(&(o.caller_file.clone(), o.site_line)).cloned().unwrap_or_default()
            } else {
                match &caller_id {
                    Some(cid) => calls_by_caller.get(cid.as_str()).cloned().unwrap_or_default(),
                    None => Vec::new(),
                }
            };
            let single = on_line.len() == 1 && o.site_line > 0;
            for i in on_line {
                let c = &mut calls[i];
                if c.outcome(CONTRACTS).known() {
                    continue;
                }
                let seg = c.callee.trim_end_matches("()").rsplit(['.', ':', '[']).next().unwrap_or("").to_string();
                if single || seg == callee_seg {
                    c.later.push((DYNAMIC, Outcome::O, target.clone().into_iter().collect()));
                }
            }
        }
        obs_edges_mapped.sort();
        obs_edges_mapped.dedup();
        dyn_summary = json!({
            "trace": tp.to_string_lossy(),
            "observed_edges": obs.len(),
            "mapped_entity_pairs": observed_pairs.len(),
            "unmapped": unmapped,
            "executed_lines": executed.len(),
        });
    }
    for b in bsites.iter_mut() {
        if b.resolved.is_none() && executed.contains(&(b.s.file.clone(), b.s.line)) {
            b.why = format!("{} (executed at runtime)", b.why);
        }
    }

    // ------------------------------------------------------------ edges per layer
    let lang_of_entity = |id: &str| -> Option<Lang> {
        deps_graph.entities.get(id).and_then(|e| Lang::of(&e.file_path))
    };
    // code edges from the call pipeline languages, per build; TS from its front-end
    let pipeline_edges = |g: &EntityGraph| -> Vec<(String, String)> {
        g.edges
            .iter()
            .filter(|e| matches!(e.ref_type, RefType::Calls | RefType::Dispatch))
            .filter(|e| {
                g.entities
                    .get(e.from_entity.as_str())
                    .is_some_and(|x| crate::parser::calls::language_for(&x.file_path).is_some())
            })
            .map(|e| (e.from_entity.as_str().to_string(), e.to_entity.as_str().to_string()))
            .collect()
    };
    let mut layer_edges: Vec<Vec<(String, String)>> = vec![Vec::new(); LAYERS.len()];
    layer_edges[BASE] = pipeline_edges(&base_graph);
    layer_edges[DEPS] = pipeline_edges(&deps_graph);
    for c in &calls {
        // TS/JS edges come from its front-end; module-level callers have no
        // sem entity, so the call pipeline recorded no edge for them either
        let module_caller = c.caller.as_deref().is_some_and(|x| x.starts_with("module:"));
        if matches!(c.lang, Lang::Ts | Lang::Js) || module_caller {
            if let Some(from) = &c.caller {
                for t in &c.targets_base {
                    layer_edges[BASE].push((from.clone(), t.clone()));
                }
                for t in &c.targets_deps {
                    layer_edges[DEPS].push((from.clone(), t.clone()));
                }
            }
        }
    }
    for l in DB..LAYERS.len() {
        let prev = layer_edges[l - 1].clone();
        layer_edges[l] = prev;
    }
    // layer additions: db (entity <-> table, triggers), config (routes, tables, registries, env), contracts, dynamic
    let mut added: Vec<Vec<(String, String)>> = vec![Vec::new(); LAYERS.len()];
    for b in &bsites {
        let (Some((l, targets)), Some(from)) = (&b.resolved, &b.caller) else { continue };
        for t in targets {
            if b.s.kind == Kind::Sql && !b.writes {
                added[*l].push((t.clone(), from.clone())); // table -> reader
            } else if b.s.kind == Kind::RouteIn {
                let route = format!("route:{} {}", b.s.method.clone().unwrap_or_else(|| "ANY".into()), b.s.key.clone().unwrap_or_default());
                added[*l].push((route, t.clone()));
            } else {
                added[*l].push((from.clone(), t.clone()));
            }
        }
    }
    for (name, o) in &schema.objects {
        if let (Some(on), Some(ex)) = (&o.on_table, &o.executes) {
            added[DB].push((format!("table:{on}"), format!("routine:{ex}")));
        }
        for t in &o.touches {
            let from = match o.kind {
                sql::DbKind::Trigger => o.on_table.clone().map(|x| format!("table:{x}")).unwrap_or_else(|| format!("trigger:{name}")),
                _ => format!("routine:{name}"),
            };
            added[DB].push((from, format!("table:{t}")));
        }
    }
    for c in &calls {
        for (l, _, tg) in &c.later {
            if let Some(from) = &c.caller {
                for t in tg {
                    added[*l].push((from.clone(), t.clone()));
                }
            }
        }
    }
    for (a, b) in &observed_pairs {
        added[DYNAMIC].push((a.clone(), b.clone()));
    }
    for l in DB..LAYERS.len() {
        for m in DB..=l {
            let add = added[m].clone();
            layer_edges[l].extend(add);
        }
        layer_edges[l].sort();
        layer_edges[l].dedup();
    }
    layer_edges[BASE].sort();
    layer_edges[BASE].dedup();
    layer_edges[DEPS].sort();
    layer_edges[DEPS].dedup();

    // ------------------------------------------------------------ sources and sinks (fixed from the final world)
    let mut sources: BTreeSet<String> = BTreeSet::new();
    for b in &bsites {
        if b.s.kind == Kind::RouteIn {
            if let Some((_, t)) = &b.resolved {
                sources.extend(t.iter().cloned());
            }
        }
        if b.s.kind == Kind::RpcOut {
            if let Some((_, t)) = &b.resolved {
                sources.extend(t.iter().cloned());
            }
        }
    }
    for r in &routes {
        sources.extend(r.handler.iter().cloned());
    }
    for ids in rpc_impls.values() {
        sources.extend(ids.iter().cloned());
    }
    for (f, v) in &ix.by_file {
        if !is_repo(f) || is_test_path(f) {
            continue;
        }
        for e in v {
            if e.name == "main" && matches!(e.ty.as_str(), "function" | "method") {
                sources.insert(e.id.clone());
            }
        }
    }
    let sinks: Vec<(usize, String)> = bsites
        .iter()
        .enumerate()
        .filter(|(_, b)| b.s.kind.is_sink())
        .filter_map(|(i, b)| b.caller.clone().map(|c| (i, c)))
        .collect();
    let mut sink_by_entity: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in &sinks {
        sink_by_entity.entry(c.clone()).or_default().push(*i);
    }

    eprintln!("sem system: edges ready (t={:.1}s)", t_all.elapsed().as_secs_f64());
    // ------------------------------------------------------------ per-layer metrics
    let mut layers_out = Vec::new();
    let unresolved_by_entity = |layer: usize| -> HashMap<String, usize> {
        let mut m: HashMap<String, usize> = HashMap::new();
        for c in &calls {
            if !c.outcome(layer).known() {
                if let Some(e) = &c.caller {
                    *m.entry(e.clone()).or_default() += 1;
                }
            }
        }
        for b in &bsites {
            let known = b.resolved.as_ref().is_some_and(|(l, _)| *l <= layer);
            if !known {
                if let Some(e) = &b.caller {
                    *m.entry(e.clone()).or_default() += 1;
                }
            }
        }
        m
    };
    for (li, lname) in LAYERS.iter().enumerate() {
        let t = Instant::now();
        // call-site outcomes
        let mut call_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        let mut call_by_lang: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for c in &calls {
            let o = c.outcome(li);
            let k = match &o {
                Outcome::R => "resolved",
                Outcome::O => "observed",
                Outcome::X => "external",
                Outcome::U(w) => {
                    *reasons.entry(w.clone()).or_default() += 1;
                    "unknown"
                }
            };
            *call_counts.entry(k.into()).or_default() += 1;
            let e = call_by_lang.entry(c.lang.as_str().into()).or_default();
            e.0 += 1;
            if !o.known() {
                e.1 += 1;
            }
        }
        let call_total = calls.len();
        let call_unres = calls.iter().filter(|c| !c.outcome(li).known()).count();
        let mut bkind: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut bwhy: BTreeMap<String, usize> = BTreeMap::new();
        for b in &bsites {
            let e = bkind.entry(b.s.kind.as_str().into()).or_default();
            e.0 += 1;
            let known = b.resolved.as_ref().is_some_and(|(l, _)| *l <= li);
            if !known {
                e.1 += 1;
                let w = if b.resolved.is_some() {
                    format!("{}: needs a later layer", b.s.kind.as_str())
                } else {
                    format!("{}: {}", b.s.kind.as_str(), b.why)
                };
                *bwhy.entry(w).or_default() += 1;
            }
        }
        let b_total = bsites.len();
        let b_unres: usize = bkind.values().map(|v| v.1).sum();
        // paths
        let edges = &layer_edges[li];
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for (a, b) in edges {
            adj.entry(a.as_str()).or_default().push(b.as_str());
        }
        let unres = unresolved_by_entity(li);
        let mut pairs = 0usize;
        let mut observed_paths = 0usize;
        let mut frontier: HashSet<String> = HashSet::new();
        let mut reached_sources = 0usize;
        // interned adjacency (all edges, and the observed subgraph)
        let mut ids: HashMap<&str, u32> = HashMap::new();
        let mut names: Vec<&str> = Vec::new();
        for (a, b) in edges {
            for n in [a.as_str(), b.as_str()] {
                if !ids.contains_key(n) {
                    ids.insert(n, names.len() as u32);
                    names.push(n);
                }
            }
        }
        let mut g: Vec<Vec<u32>> = vec![Vec::new(); names.len()];
        let mut go: Vec<Vec<u32>> = vec![Vec::new(); names.len()];
        for (a, b) in edges {
            let (x, y) = (ids[a.as_str()], ids[b.as_str()]);
            g[x as usize].push(y);
            if observed_pairs.contains(&(a.clone(), b.clone())) {
                go[x as usize].push(y);
            }
        }
        let bfs = |adj: &Vec<Vec<u32>>, s: u32| -> Vec<u32> {
            let mut dist: HashMap<u32, usize> = HashMap::new();
            let mut q = VecDeque::new();
            dist.insert(s, 0);
            q.push_back(s);
            let mut out = vec![s];
            while let Some(n) = q.pop_front() {
                let d = dist[&n];
                if d >= PATH_DEPTH {
                    continue;
                }
                for &m in &adj[n as usize] {
                    if let std::collections::hash_map::Entry::Vacant(v) = dist.entry(m) {
                        v.insert(d + 1);
                        q.push_back(m);
                        out.push(m);
                    }
                }
            }
            out
        };
        for s in &sources {
            let Some(&sid) = ids.get(s.as_str()) else {
                // a source with no edges still reaches its own sinks
                if let Some(sk) = sink_by_entity.get(s) {
                    pairs += sk.len();
                    reached_sources += 1;
                }
                if unres.contains_key(s) {
                    frontier.insert(s.to_string());
                }
                continue;
            };
            let reach: Vec<&str> = bfs(&g, sid).into_iter().map(|i| names[i as usize]).collect();
            let reach_obs: HashSet<&str> = if observed_pairs.is_empty() {
                HashSet::new()
            } else {
                bfs(&go, sid).into_iter().map(|i| names[i as usize]).collect()
            };
            let mut any = false;
            for &n in &reach {
                if let Some(sk) = sink_by_entity.get(n) {
                    any = true;
                    pairs += sk.len();
                    // OBSERVED: some witness path ran edge by edge, and so did the sink site
                    if reach_obs.contains(n) {
                        observed_paths += sk
                            .iter()
                            .filter(|&&i| executed.contains(&(bsites[i].s.file.clone(), bsites[i].s.line)))
                            .count();
                    }
                }
                if unres.contains_key(n) {
                    frontier.insert(n.to_string());
                }
            }
            if any {
                reached_sources += 1;
            }
        }
        let frontier_sites: usize = frontier.iter().map(|e| unres.get(e).copied().unwrap_or(0)).sum();
        // recall against observed edges
        let edge_set: HashSet<(&str, &str)> = edges.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (mut rr_hit, mut rr_all, mut rd_hit, mut rd_all) = (0, 0, 0, 0);
        // explicit = the observed call has a syntactic call site (not a property,
        // operator, decorator application or other implicit call)
        let (mut ex_hit, mut ex_all) = (0, 0);
        for (a, b, in_repo, explicit) in &obs_edges_mapped {
            // the edge must exist in this layer's world
            let in_world = *in_repo || li >= DEPS;
            if !in_world {
                continue;
            }
            // a constructor call is statically an edge to the class
            let class_of = deps_graph.entities.get(b.as_str()).filter(|e| {
                matches!(e.name.as_str(), "__init__" | "__new__" | "constructor" | "new" | "init")
            });
            let hit = edge_set.contains(&(a.as_str(), b.as_str()))
                || class_of
                    .and_then(|e| e.parent_id.as_ref())
                    .is_some_and(|p| edge_set.contains(&(a.as_str(), p.as_str())));
            if *explicit {
                ex_all += 1;
                if hit {
                    ex_hit += 1;
                }
            }
            if *in_repo {
                rr_all += 1;
                if hit {
                    rr_hit += 1;
                }
            } else {
                rd_all += 1;
                if hit {
                    rd_hit += 1;
                }
            }
        }
        let mut top_reasons: Vec<(String, usize)> = reasons.into_iter().collect();
        top_reasons.sort_by(|a, b| b.1.cmp(&a.1));
        top_reasons.truncate(12);
        let mut top_bwhy: Vec<(String, usize)> = bwhy.into_iter().collect();
        top_bwhy.sort_by(|a, b| b.1.cmp(&a.1));
        top_bwhy.truncate(16);
        let rate = |u: usize, t: usize| if t == 0 { Value::Null } else { json!(u as f64 / t as f64) };
        layers_out.push(json!({
            "layer": lname,
            "call_sites": call_total,
            "call_unresolved": call_unres,
            "call_unknown_rate": rate(call_unres, call_total),
            "call_outcomes": call_counts,
            "call_by_lang": call_by_lang.iter().map(|(k, (t, u))| (k.clone(), json!({"total": t, "unresolved": u, "rate": rate(*u, *t)}))).collect::<serde_json::Map<_, _>>(),
            "call_unknown_reasons": top_reasons,
            "boundary_sites": b_total,
            "boundary_unresolved": b_unres,
            "boundary_unknown_rate": rate(b_unres, b_total),
            "boundary_by_kind": bkind.iter().map(|(k, (t, u))| (k.clone(), json!({"total": t, "unresolved": u}))).collect::<serde_json::Map<_, _>>(),
            "boundary_unresolved_reasons": top_bwhy,
            "combined_unknown_rate": rate(call_unres + b_unres, call_total + b_total),
            "edges": edges.len(),
            "paths": {"pairs": pairs, "observed": observed_paths, "possible": pairs - observed_paths.min(pairs), "sources_reaching_a_sink": reached_sources, "unknown_frontier_sites": frontier_sites},
            "recall": {
                "repo_repo": rate(rr_hit, rr_all), "repo_repo_n": rr_all,
                "repo_dep": if li >= DEPS { rate(rd_hit, rd_all) } else { Value::Null }, "repo_dep_n": rd_all,
                "explicit_sites": rate(ex_hit, ex_all), "explicit_sites_n": ex_all,
            },
            "metrics_s": t.elapsed().as_secs_f64(),
        }));
    }

    // ------------------------------------------------------------ outputs
    let mut rows: Vec<SiteRow> = Vec::new();
    for c in &calls {
        let outs: Vec<String> = (0..LAYERS.len()).map(|l| c.outcome(l).code()).collect();
        let resolved_at = (0..LAYERS.len()).find(|l| c.outcome(*l).known());
        rows.push(SiteRow {
            file: c.file.clone(),
            line: c.line,
            lang: c.lang.as_str(),
            kind: "call".into(),
            text: c.callee.clone(),
            caller: c.caller.clone(),
            outcomes: outs,
            targets: resolved_at.map(|l| c.targets(l)).unwrap_or_default(),
            resolved_at,
            key: None,
        });
    }
    for b in &bsites {
        let outs: Vec<String> = (0..LAYERS.len())
            .map(|l| match &b.resolved {
                Some((rl, _)) if *rl <= l => "R".to_string(),
                _ => format!("U:{}", if b.resolved.is_some() { "needs a later layer".to_string() } else { b.why.clone() }),
            })
            .collect();
        rows.push(SiteRow {
            file: b.s.file.clone(),
            line: b.s.line,
            lang: b.s.lang.as_str(),
            kind: b.s.kind.as_str().into(),
            text: b.s.text.clone(),
            caller: b.caller.clone(),
            outcomes: outs,
            targets: b.resolved.as_ref().map(|(_, t)| t.clone()).unwrap_or_default(),
            resolved_at: b.resolved.as_ref().map(|(l, _)| *l),
            key: b.s.key.clone().map(|k| k.chars().take(300).collect()),
        });
    }
    write_jsonl(&input.out.join("sites.jsonl"), &rows).map_err(|e| e.to_string())?;
    // static edges with the layer they first appear in, for precision sampling
    let mut first_layer: HashMap<(String, String), usize> = HashMap::new();
    for (l, es) in layer_edges.iter().enumerate() {
        for e in es {
            first_layer.entry(e.clone()).or_insert(l);
        }
    }
    // site locations for code edges
    let mut edge_site: HashMap<(String, String), (String, usize)> = HashMap::new();
    for c in &calls {
        if let Some(from) = &c.caller {
            for l in 0..LAYERS.len() {
                for t in c.targets(l) {
                    edge_site.entry((from.clone(), t)).or_insert((c.file.clone(), c.line));
                }
            }
        }
    }
    for b in &bsites {
        if let (Some((_, ts)), Some(from)) = (&b.resolved, &b.caller) {
            for t in ts {
                edge_site.entry((from.clone(), t.clone())).or_insert((b.s.file.clone(), b.s.line));
                edge_site.entry((t.clone(), from.clone())).or_insert((b.s.file.clone(), b.s.line));
            }
        }
    }
    let mut erows: Vec<Value> = first_layer
        .iter()
        .map(|((a, b), l)| {
            let site = edge_site.get(&(a.clone(), b.clone()));
            json!({
                "from": a, "to": b, "layer": LAYERS[*l],
                "observed": observed_pairs.contains(&(a.clone(), b.clone())),
                "site": site.map(|(f, n)| format!("{f}:{n}")),
                "site_executed": site.map(|s| executed.contains(s)),
                "from_lang": lang_of_entity(a).map(|l| l.as_str()),
            })
        })
        .collect();
    erows.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    write_jsonl(&input.out.join("edges.jsonl"), &erows).map_err(|e| e.to_string())?;
    // line spans of every entity an edge touches (for rendering samples)
    let mut spans: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (a, b) in first_layer.keys() {
        for n in [a, b] {
            if let Some(e) = deps_graph.entities.get(n.as_str()) {
                spans.insert(n.as_str(), (e.start_line, e.end_line));
            }
        }
    }
    std::fs::write(input.out.join("spans.json"), serde_json::to_string(&spans).unwrap()).map_err(|e| e.to_string())?;
    // observed edges with the first layer whose static graph holds them
    let layer_sets: Vec<HashSet<(&str, &str)>> = layer_edges
        .iter()
        .map(|es| es.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect())
        .collect();
    let orows: Vec<Value> = obs_edges_mapped
        .iter()
        .map(|(a, b, in_repo, explicit)| {
            let parent = deps_graph
                .entities
                .get(b.as_str())
                .filter(|e| matches!(e.name.as_str(), "__init__" | "__new__" | "constructor" | "new" | "init"))
                .and_then(|e| e.parent_id.as_ref().map(|p| p.as_str().to_string()));
            let first = (0..DYNAMIC).find(|l| {
                layer_sets[*l].contains(&(a.as_str(), b.as_str()))
                    || parent.as_deref().is_some_and(|p| layer_sets[*l].contains(&(a.as_str(), p)))
            });
            json!({"from": a, "to": b, "to_repo": in_repo, "explicit": explicit, "static_layer": first.map(|l| LAYERS[l])})
        })
        .collect();
    write_jsonl(&input.out.join("observed.jsonl"), &orows).map_err(|e| e.to_string())?;
    let world = json!({
        "schema": schema,
        "orm_models": model_tables,
        "routes": routes.iter().map(|r| json!({"method": r.method, "path": r.path, "handler": r.handler})).collect::<Vec<_>>(),
        "config": cfg,
        "contracts": ctr,
        "located_deps": located.iter().map(|l| json!({"eco": l.dep.ecosystem.as_str(), "name": l.dep.name, "version": l.dep.version, "found": l.dir.is_some(), "files": l.files.len()})).collect::<Vec<_>>(),
        "sources": sources,
        "hookimpls": hookimpls,
        "registrations": registrations.len(),
    });
    std::fs::write(input.out.join("world.json"), serde_json::to_string_pretty(&world).unwrap()).map_err(|e| e.to_string())?;

    costs.insert("total_s".into(), json!(t_all.elapsed().as_secs_f64()));
    let summary = Summary {
        repo: root.to_string_lossy().to_string(),
        files: files_count,
        deps: json!({
            "locked": locked.len(),
            "by_ecosystem": by_eco.iter().map(|(k, (a, b, c))| (k.clone(), json!({"locked": a, "located": b, "files": c}))).collect::<serde_json::Map<_, _>>(),
            "stdlib_files": std_files,
            "dep_files_in_world": dep_files.len(),
            "ts_front_end": ts_note,
            "base_entities": base_ids.len(),
            "world_entities": deps_graph.entities.len(),
        }),
        db: json!({
            "sql_files": sql_files.len(),
            "objects": schema.objects.len(),
            "tables": schema.objects.values().filter(|o| o.kind == sql::DbKind::Table).count(),
            "views": schema.objects.values().filter(|o| o.kind == sql::DbKind::View).count(),
            "routines": schema.objects.values().filter(|o| matches!(o.kind, sql::DbKind::Function | sql::DbKind::Procedure)).count(),
            "triggers": schema.objects.values().filter(|o| o.kind == sql::DbKind::Trigger).count(),
            "orm_models": model_tables.len(),
        }),
        config: json!({
            "env_keys": cfg.env.keys.len(),
            "config_files": cfg.files.len(),
            "manifests": cfg.manifests.iter().map(|m| json!({"file": m.file, "kind": m.kind, "entries": m.entries.len()})).collect::<Vec<_>>(),
            "routes": routes.len(),
            "dispatch_tables": tables.len(),
            "table_dispatch_calls_resolved": table_resolved,
            "registrations": registrations.len(),
            "registry_lookups_resolved": registry_resolved,
            "hookimpls": hookimpls.values().map(|v| v.len()).sum::<usize>(),
        }),
        contracts: json!({
            "files": ctr.files,
            "http_ops": ctr.http.len(),
            "rpcs": ctr.rpc.len(),
            "graphql_fields": ctr.graphql.len(),
        }),
        layers: layers_out,
        closed_world_sites: json!(closed),
        costs: Value::Object(costs),
        sources: sources.len(),
        sinks: sinks.len(),
        dynamic: dyn_summary,
        dataflow: json!(dataflow_layers),
    };
    std::fs::write(input.out.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap()).map_err(|e| e.to_string())?;
    Ok(summary)
}

/// `dir` + a relative module specifier, `..` and `.` folded.
fn normalize_rel(dir: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in spec.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Import paths of a Go source file.
fn go_imports(src: &str) -> Vec<String> {
    static R: std::sync::OnceLock<(regex::Regex, regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    let (block, single, spec) = R.get_or_init(|| {
        (
            regex::Regex::new(r"(?s)\bimport\s*\((.*?)\)").unwrap(),
            regex::Regex::new(r#"(?m)^\s*import\s+(?:[\w.]+\s+)?"([^"]+)""#).unwrap(),
            regex::Regex::new(r#""([^"]+)""#).unwrap(),
        )
    });
    // only the import section: up to the first top-level declaration
    let head = src
        .find("\nfunc ")
        .or_else(|| src.find("\ntype "))
        .or_else(|| src.find("\nvar "))
        .map(|i| &src[..i])
        .unwrap_or(src);
    let mut out: Vec<String> = Vec::new();
    for b in block.captures_iter(head) {
        out.extend(spec.captures_iter(&b[1]).map(|c| c[1].to_string()));
    }
    out.extend(single.captures_iter(head).map(|c| c[1].to_string()));
    out
}

/// The Go dependency files the build compiles: the package import closure
/// from `repo` over the located modules (`(module path, world dir)`) and
/// GOROOT (`gostd`, whose vendored golang.org/x packages it also serves).
fn go_import_closure(
    root: &Path,
    repo: &[&String],
    dep_files: &[String],
    modules: &[(String, String)],
    gostd: Option<&str>,
) -> HashSet<String> {
    let mut by_dir: HashMap<&str, Vec<&String>> = HashMap::new();
    for f in dep_files.iter().filter(|f| f.ends_with(".go")) {
        let d = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        by_dir.entry(d).or_default().push(f);
    }
    let dir_of = |imp: &str| -> Option<String> {
        let first = imp.split('/').next().unwrap_or("");
        if !first.contains('.') {
            return gostd.map(|g| format!("{g}/{imp}"));
        }
        let best = modules
            .iter()
            .filter(|(m, _)| imp == m || imp.starts_with(&format!("{m}/")))
            .max_by_key(|(m, _)| m.len());
        match best {
            Some((m, dir)) => Some(format!("{dir}{}", &imp[m.len()..])),
            None => gostd.map(|g| format!("{g}/vendor/{imp}")),
        }
    };
    let mut keep: HashSet<String> = HashSet::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for f in repo {
        if let Ok(src) = std::fs::read_to_string(root.join(f.as_str())) {
            queue.extend(go_imports(&src));
        }
    }
    while let Some(imp) = queue.pop_front() {
        if !seen.insert(imp.clone()) {
            continue;
        }
        let Some(dir) = dir_of(&imp) else { continue };
        let Some(files) = by_dir.get(dir.as_str()) else { continue };
        for f in files {
            keep.insert((*f).clone());
            if let Ok(src) = std::fs::read_to_string(root.join(f.as_str())) {
                for i in go_imports(&src) {
                    if !seen.contains(&i) {
                        queue.push_back(i);
                    }
                }
            }
        }
    }
    keep
}

/// An entity inside a test module of a source file (`mod tests`).
fn is_test_entity(id: &str) -> bool {
    id.contains("::module::tests") || id.contains("::module::test::") || id.ends_with("::module::test")
}

fn word_in(hay: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let b = hay.as_bytes();
    let mut start = 0;
    while let Some(i) = hay[start..].find(word) {
        let s = start + i;
        let e = s + word.len();
        let before = s == 0 || !(b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_');
        let after = e >= b.len() || !(b[e].is_ascii_alphanumeric() || b[e] == b'_');
        if before && after {
            return true;
        }
        start = s + 1;
    }
    false
}

fn lower_camel(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_lowercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// GORM's default table name: snake_case plural of the struct name.
fn snake_plural(name: &str) -> String {
    let mut out = snake(name);
    if out.ends_with('y') && !out.ends_with("ey") {
        out.pop();
        out.push_str("ies");
    } else if out.ends_with('s') || out.ends_with("ch") || out.ends_with('x') {
        out.push_str("es");
    } else {
        out.push('s');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(snake_plural("ArticleModel"), "article_models");
        assert_eq!(snake_plural("Category"), "categories");
        assert!(word_in("select(User).where", "User"));
        assert!(!word_in("UserModel", "User"));
        assert_eq!(line_of(&line_starts("a\nb\nc"), 2), 2);
        assert_eq!(line_of(&line_starts("a\nb\nc"), 0), 1);
    }
}
