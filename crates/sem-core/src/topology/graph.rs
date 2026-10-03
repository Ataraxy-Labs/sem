//! The module-reference graph: extraction once, projected to package or
//! module (file) granularity, scoped to a root, filtered by edge kind and
//! reference kind.
//!
//! The graph is a multigraph: one edge per (source, target, file kind,
//! reference kind). Counting measures (density, cyclomatic number) see the
//! parallel edges; path measures (reach, betweenness, depth) deduplicate.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use rayon::prelude::*;

use super::imports::{module_refs, Form, RefKind};
use super::resolve::{Resolver, Target};
use super::workspace::{bare_package_name_len, Workspace};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Prod,
    Test,
    Example,
    Build,
}

/// Conventional file roles, from the package-relative path.
pub fn classify(rel: &str) -> FileKind {
    let leaf = rel.rsplit('/').next().unwrap_or(rel);
    let segs: Vec<&str> = rel.split('/').collect();
    let dirs = &segs[..segs.len().saturating_sub(1)];
    let has = |names: &[&str]| dirs.iter().any(|s| names.contains(s));
    let tsx = |mid: &str| {
        ["ts", "tsx", "mts", "cts", "mtsx", "ctsx", "js", "jsx", "mjs", "cjs"].iter().any(|e| leaf.ends_with(&format!("{mid}{e}")))
    };
    if tsx(".test.") || tsx(".spec.") || has(&["__tests__", "__mocks__", "test", "testing"]) {
        FileKind::Test
    } else if has(&["examples", "example"]) {
        FileKind::Example
    } else if ["ts", "tsx", "js", "jsx", "mts", "mjs", "cts", "cjs"].iter().any(|e| leaf.ends_with(&format!(".config.{e}"))) {
        FileKind::Build
    } else {
        FileKind::Prod
    }
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub target: Target,
    pub kind: RefKind,
    pub form: Form,
    pub line: u32,
    pub specifier: String,
}

#[derive(Debug)]
pub struct SourceFile {
    pub path: String,
    pub package: Option<usize>,
    pub file_kind: FileKind,
    pub refs: Vec<Reference>,
    pub parse_errors: usize,
}

/// Everything read from disk: workspace + every source file's resolved references.
#[derive(Debug)]
pub struct Extraction {
    pub ws: Workspace,
    pub files: Vec<SourceFile>,
    by_path: HashMap<String, usize>,
    /// Every source path, when only some were scanned (`run_in`): a scanned
    /// file's import of an unscanned one is still an edge.
    all_paths: Option<HashSet<String>>,
}

impl Extraction {
    /// `paths` are repo-relative source files (the caller decides the scan set).
    pub fn run(root: &Path, ws: Workspace, paths: &[String]) -> Extraction {
        Self::run_in(root, ws, paths, None)
    }

    /// [`Self::run`], reading only the files in `scan` (all of `paths` when
    /// `None`). Specifiers still resolve against every path, so a scanned
    /// file's references are the same as in a full run.
    pub fn run_in(root: &Path, ws: Workspace, paths: &[String], scan: Option<&HashSet<String>>) -> Extraction {
        let file_set: HashSet<String> = paths.iter().cloned().collect();
        let resolver = Resolver { ws: &ws, files: &file_set };
        let scanned: Vec<&String> = paths.iter().filter(|p| scan.is_none_or(|s| s.contains(p.as_str()))).collect();
        let files: Vec<SourceFile> = scanned
            .par_iter()
            .map(|&p| {
                let content = std::fs::read_to_string(root.join(p)).unwrap_or_default();
                let (refs, errs) = module_refs(p, &content);
                let package = ws.owner_of(p);
                let rel_in_pkg = package
                    .and_then(|i| p.strip_prefix(&format!("{}/", ws.packages[i].dir)))
                    .unwrap_or(p);
                let refs = refs
                    .into_iter()
                    .flat_map(|r| {
                        let targets = match r.form {
                            Form::DynamicPattern => resolver.expand_pattern(p, &r.specifier),
                            _ => vec![resolver.resolve(p, &r.specifier)],
                        };
                        targets.into_iter().map(move |target| Reference {
                            target,
                            kind: r.kind,
                            form: r.form,
                            line: r.line,
                            specifier: r.specifier.clone(),
                        })
                    })
                    .collect();
                SourceFile { path: p.clone(), package, file_kind: classify(rel_in_pkg), refs, parse_errors: errs }
            })
            .collect();
        let by_path = files.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
        let all_paths = scan.map(|_| file_set);
        Extraction { ws, files, by_path, all_paths }
    }

    /// A source file of the repo that was not scanned (`run_in`).
    pub fn unscanned(&self, path: &str) -> bool {
        self.all_paths.as_ref().is_some_and(|a| a.contains(path)) && !self.by_path.contains_key(path)
    }

    pub fn file(&self, path: &str) -> Option<&SourceFile> {
        self.by_path.get(path).map(|&i| &self.files[i])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    Package,
    Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Selector {
    Both,
    Value,
    Type,
}

impl Selector {
    pub fn admits(self, k: RefKind) -> bool {
        matches!((self, k), (Selector::Both, _) | (Selector::Value, RefKind::Value) | (Selector::Type, RefKind::Type))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Package,
    Module,
    /// Outside the root: an opaque sink with no out-edges.
    World,
    /// A repo file a module references that is not a scanned source file
    /// (json, stylesheet, font, image): a sink. Only with `Options::assets`.
    Asset,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    /// Owning workspace package name (module nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_kind: Option<FileKind>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    /// Role of the source file(s) that realize the edge.
    pub kind: FileKind,
    pub reference: RefKind,
    /// The target is a dependency the source package's manifest declares.
    pub declared: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Unresolved {
    pub source: String,
    pub specifier: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Graph {
    pub granularity: Granularity,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub unresolved: Vec<Unresolved>,
}

pub struct Options<'a> {
    pub granularity: Granularity,
    /// Repo-relative directory; packages outside it collapse to world sinks.
    pub root: &'a str,
    pub kinds: &'a [FileKind],
    pub reference: Selector,
    /// Drop world nodes and every edge to them.
    pub no_world: bool,
    /// Module granularity: a reference to an existing repo file that is not a
    /// source node (json, css, font, image) becomes an edge to an asset node
    /// instead of an unresolved import.
    pub assets: bool,
}

const NODE_BUILTINS: &[&str] = &[
    "assert", "async_hooks", "buffer", "child_process", "cluster", "console", "constants", "crypto", "dgram",
    "diagnostics_channel", "dns", "domain", "events", "fs", "http", "http2", "https", "inspector", "module", "net",
    "os", "path", "perf_hooks", "process", "punycode", "querystring", "readline", "repl", "stream", "string_decoder",
    "sys", "timers", "tls", "trace_events", "tty", "url", "util", "v8", "vm", "wasi", "worker_threads", "zlib",
];

/// World-node name for a specifier that leaves the repo (`None` for relative/absolute/`#`).
/// Runtime builtins group under one node per runtime (`node`, `bun`).
pub fn outside_name(spec: &str) -> Option<String> {
    if spec.starts_with("node:") {
        return Some("node".into());
    }
    if spec.starts_with("bun:") || spec == "bun" {
        return Some("bun".into());
    }
    let n = bare_package_name_len(spec)?;
    let name = &spec[..n];
    if NODE_BUILTINS.contains(&name) {
        return Some("node".into());
    }
    Some(name.to_string())
}

/// Manifest entries that declare a dependency on `target`: a runtime builtin
/// is declared through its type package.
fn declaring_names(target: &str) -> Vec<&str> {
    match target {
        "node" => vec!["@types/node"],
        "bun" => vec!["@types/bun", "bun-types"],
        t => vec![t],
    }
}

impl Graph {
    pub fn build(ex: &Extraction, opt: &Options) -> Graph {
        let root = opt.root.trim_end_matches('/');
        let in_root = |dir: &str| root.is_empty() || root == "." || dir == root || dir.starts_with(&format!("{root}/"));
        let admitted: Vec<bool> = ex.ws.packages.iter().map(|p| in_root(&p.dir)).collect();
        let admitted_names: HashSet<&str> =
            ex.ws.packages.iter().enumerate().filter(|(i, _)| admitted[*i]).map(|(_, p)| p.name.as_str()).collect();
        let kind_ok = |k: FileKind| opt.kinds.contains(&k);

        let mut nodes: Vec<Node> = Vec::new();
        let mut index: HashMap<(NodeKind, String), usize> = HashMap::new();
        let mut add = |nodes: &mut Vec<Node>, kind: NodeKind, id: String, package: Option<String>, fk: Option<FileKind>| -> usize {
            *index.entry((kind, id.clone())).or_insert_with(|| {
                nodes.push(Node { id, kind, package, file_kind: fk });
                nodes.len() - 1
            })
        };

        let mut files: Vec<&SourceFile> = ex.files.iter().filter(|f| f.package.is_some_and(|p| admitted[p])).collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let mut pkg_node: HashMap<&str, usize> = HashMap::new();
        let mut file_node: HashMap<&str, usize> = HashMap::new();
        match opt.granularity {
            Granularity::Package => {
                let mut names: Vec<&str> = admitted_names.iter().copied().collect();
                names.sort();
                for n in names {
                    let id = add(&mut nodes, NodeKind::Package, n.to_string(), None, None);
                    pkg_node.insert(n, id);
                }
            }
            Granularity::Module => {
                for f in &files {
                    if kind_ok(f.file_kind) {
                        let pkg = f.package.map(|p| ex.ws.packages[p].name.clone());
                        let id = add(&mut nodes, NodeKind::Module, f.path.clone(), pkg, Some(f.file_kind));
                        file_node.insert(f.path.as_str(), id);
                    }
                }
                // files a scanned file imports but that were not scanned
                // themselves: nodes too (their own imports are not known)
                let mut extra: BTreeSet<&str> = BTreeSet::new();
                for f in &files {
                    for r in &f.refs {
                        if let Target::File(p) = &r.target {
                            if !file_node.contains_key(p.as_str()) && ex.unscanned(p) {
                                extra.insert(p.as_str());
                            }
                        }
                    }
                }
                for p in extra {
                    let Some(o) = ex.ws.owner_of(p).filter(|&o| admitted[o]) else { continue };
                    let rel = p.strip_prefix(&format!("{}/", ex.ws.packages[o].dir)).unwrap_or(p);
                    let fk = classify(rel);
                    if kind_ok(fk) {
                        let id = add(&mut nodes, NodeKind::Module, p.to_string(), Some(ex.ws.packages[o].name.clone()), Some(fk));
                        file_node.insert(p, id);
                    }
                }
            }
        }
        let internal_count = nodes.len();

        let mut acc: BTreeSet<(usize, usize, FileKind, RefKind)> = BTreeSet::new();
        let mut unresolved = Vec::new();
        let mut miss = |f: &SourceFile, r: &Reference, v: &mut Vec<Unresolved>| {
            v.push(Unresolved { source: f.path.clone(), specifier: r.specifier.clone() })
        };
        for f in &files {
            if !kind_ok(f.file_kind) {
                continue;
            }
            let own_name = ex.ws.packages[f.package.unwrap()].name.as_str();
            let from = match opt.granularity {
                Granularity::Package => pkg_node[own_name],
                Granularity::Module => file_node[f.path.as_str()],
            };
            for r in &f.refs {
                if !opt.reference.admits(r.kind) {
                    continue;
                }
                let to = match opt.granularity {
                    // a package depends on another by *name*: the specifier's package, if it names one
                    Granularity::Package => {
                        let Some(name) = outside_name(&r.specifier) else { continue };
                        if name == own_name {
                            continue;
                        }
                        match pkg_node.get(name.as_str()) {
                            Some(&n) => n,
                            None if opt.no_world => continue,
                            None => add(&mut nodes, NodeKind::World, name, None, None),
                        }
                    }
                    Granularity::Module => match &r.target {
                        Target::File(p) if file_node.contains_key(p.as_str()) => file_node[p.as_str()],
                        Target::File(p) | Target::Path(p)
                            if opt.assets && ex.file(p).is_none() && ex.ws.root.join(p).is_file() =>
                        {
                            add(&mut nodes, NodeKind::Asset, p.clone(), ex.ws.owner_of(p).map(|o| ex.ws.packages[o].name.clone()), None)
                        }
                        Target::File(p) | Target::Path(p) => match ex.ws.owner_of(p) {
                            // in-root but not a node (excluded file kind) -> no edge; otherwise unresolved
                            Some(o) if admitted[o] => {
                                if ex.file(p).is_none() {
                                    miss(f, r, &mut unresolved);
                                }
                                continue;
                            }
                            Some(_) if opt.no_world => continue,
                            Some(o) => add(&mut nodes, NodeKind::World, ex.ws.packages[o].name.clone(), None, None),
                            None => {
                                miss(f, r, &mut unresolved);
                                continue;
                            }
                        },
                        Target::Package(i) if !admitted[*i] => {
                            if opt.no_world {
                                continue;
                            }
                            add(&mut nodes, NodeKind::World, ex.ws.packages[*i].name.clone(), None, None)
                        }
                        Target::Package(_) => {
                            miss(f, r, &mut unresolved);
                            continue;
                        }
                        Target::External(_) => match outside_name(&r.specifier) {
                            Some(_) if opt.no_world => continue,
                            Some(n) => add(&mut nodes, NodeKind::World, n, None, None),
                            None => {
                                miss(f, r, &mut unresolved);
                                continue;
                            }
                        },
                    },
                };
                if to != from {
                    acc.insert((from, to, f.file_kind, r.kind));
                }
            }
        }

        let pkg_of = |u: usize| -> &str {
            match nodes[u].kind {
                NodeKind::Module => nodes[u].package.as_deref().unwrap_or(""),
                _ => nodes[u].id.as_str(),
            }
        };
        let edges = acc
            .into_iter()
            .map(|(from, to, kind, reference)| {
                let (sp, tp) = (pkg_of(from), pkg_of(to));
                let declared = sp == tp
                    || ex.ws.by_name.get(sp).is_some_and(|&i| {
                        let p = &ex.ws.packages[i];
                        declaring_names(tp).iter().any(|t| {
                            p.deps.contains(*t) || p.peer_deps.contains(*t) || (kind != FileKind::Prod && p.dev_deps.contains(*t))
                        })
                    });
                Edge { from, to, kind, reference, declared }
            })
            .collect();

        let mut g = Graph { granularity: opt.granularity, nodes, edges, unresolved };
        g.sort_world(internal_count);
        g
    }

    /// World nodes after internal ones, sorted by name; edges re-sorted.
    fn sort_world(&mut self, internal: usize) {
        let mut world: Vec<usize> = (internal..self.nodes.len()).collect();
        world.sort_by(|&a, &b| self.nodes[a].id.cmp(&self.nodes[b].id));
        let mut remap: Vec<usize> = (0..self.nodes.len()).collect();
        for (i, &old) in world.iter().enumerate() {
            remap[old] = internal + i;
        }
        let mut nodes = self.nodes.clone();
        for (old, n) in self.nodes.iter().enumerate() {
            nodes[remap[old]] = n.clone();
        }
        self.nodes = nodes;
        for e in &mut self.edges {
            e.from = remap[e.from];
            e.to = remap[e.to];
        }
        self.edges.sort_by(|a, b| (a.from, a.to, a.kind, a.reference).cmp(&(b.from, b.to, b.kind, b.reference)));
    }

    /// The declared runtime dependency graph between workspace packages
    /// (`dependencies` ∪ `peerDependencies` ∪ `optionalDependencies`; dev excluded).
    pub fn manifest(ws: &Workspace) -> Graph {
        let mut names: Vec<&str> = ws.packages.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        let at: HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
        let nodes = names
            .iter()
            .map(|n| Node { id: n.to_string(), kind: NodeKind::Package, package: None, file_kind: None })
            .collect();
        let mut edges = Vec::new();
        for p in &ws.packages {
            let deps: BTreeSet<&String> = p.deps.iter().chain(&p.peer_deps).chain(&p.optional_deps).collect();
            for d in deps {
                if let Some(&to) = at.get(d.as_str()) {
                    let from = at[p.name.as_str()];
                    if from != to {
                        edges.push(Edge { from, to, kind: FileKind::Prod, reference: RefKind::Value, declared: true });
                    }
                }
            }
        }
        edges.sort_by_key(|e| (e.from, e.to));
        Graph { granularity: Granularity::Package, nodes, edges, unresolved: Vec::new() }
    }

    /// Distinct successors (parallel edges collapsed), in edge order.
    pub fn adjacency(&self) -> Vec<Vec<usize>> {
        self.adjacency_where(|_| true)
    }

    pub fn adjacency_where(&self, keep: impl Fn(&Edge) -> bool) -> Vec<Vec<usize>> {
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for e in self.edges.iter().filter(|e| keep(e)) {
            if !adj[e.from].contains(&e.to) {
                adj[e.from].push(e.to);
            }
        }
        adj
    }

    pub fn find(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    pub fn is_world(&self, u: usize) -> bool {
        self.nodes[u].kind == NodeKind::World
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, outside_name, FileKind};

    #[test]
    fn file_roles() {
        assert_eq!(classify("src/a.ts"), FileKind::Prod);
        assert_eq!(classify("src/a.test.ts"), FileKind::Test);
        assert_eq!(classify("__tests__/x.ts"), FileKind::Test);
        assert_eq!(classify("testing/src/x.ts"), FileKind::Test);
        assert_eq!(classify("examples/demo/x.ts"), FileKind::Example);
        assert_eq!(classify("vite.config.ts"), FileKind::Build);
        assert_eq!(classify("src/test.ts"), FileKind::Prod);
        assert_eq!(classify("src/a.spec.jsx"), FileKind::Test);
        assert_eq!(classify("src/a.test.mjs"), FileKind::Test);
    }

    #[test]
    fn outside_names() {
        assert_eq!(outside_name("@a/b/c").as_deref(), Some("@a/b"));
        assert_eq!(outside_name("effect/Schema").as_deref(), Some("effect"));
        assert_eq!(outside_name("node:fs").as_deref(), Some("node"));
        assert_eq!(outside_name("fs").as_deref(), Some("node"));
        assert_eq!(outside_name("./x"), None);
        assert_eq!(outside_name("#src/x"), None);
    }
}
