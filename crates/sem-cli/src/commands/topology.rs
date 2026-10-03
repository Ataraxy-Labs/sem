//! `sem topology`: the module-reference graph of a JS/TS workspace, the
//! graph math over it, and laws checked against it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Value};

use sem_core::topology::algo;
use sem_core::topology::glob;
use sem_core::topology::graph::{Extraction, FileKind, Granularity, Graph, NodeKind, Options, Selector};
use sem_core::topology::metrics;
use sem_core::topology::workspace::{Discovery, Workspace};

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum GranularityArg {
    Package,
    Module,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum KindArg {
    Both,
    Value,
    Type,
}

#[derive(Args, Debug, Clone)]
pub struct Common {
    /// Repository root (workspace discovery starts at its package.json)
    #[arg(long, default_value = ".")]
    repo_root: String,
    /// Only packages under this directory are nodes; everything else is an opaque world sink
    #[arg(long, default_value = "")]
    root: String,
    #[arg(long, value_enum, default_value = "package")]
    granularity: GranularityArg,
    /// Reference relation: runtime (value), compile-time only (type), or both
    #[arg(long, value_enum, default_value = "both")]
    reference_kind: KindArg,
    /// Also include edges from test / example / build files (default: production only)
    #[arg(long)]
    include_test: bool,
    #[arg(long)]
    include_example: bool,
    #[arg(long)]
    include_build: bool,
    /// Drop world (outside-root) nodes and edges to them
    #[arg(long)]
    no_world: bool,
    /// Module granularity: imports of existing non-source repo files (json,
    /// css, fonts, images) become edges to asset nodes instead of unresolved
    #[arg(long)]
    assets: bool,
    /// The repository root is a package too: files outside every workspace
    /// package (root config, scripts, test setup) become nodes owned by it
    #[arg(long)]
    root_package: bool,
    /// Source extensions to scan
    #[arg(long = "ext", num_args = 1.., default_values = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"])]
    exts: Vec<String>,
    /// Directory names never descended into
    #[arg(long = "skip-dir", num_args = 1.., default_values = ["node_modules", "dist", "dist-types"])]
    skip_dirs: Vec<String>,
    /// Globs over workspace directories to leave out
    #[arg(long = "exclude-dir", num_args = 1..)]
    exclude_dirs: Vec<String>,
    /// Print timings to stderr
    #[arg(long)]
    timings: bool,
}

#[derive(Subcommand, Debug)]
pub enum TopologyCmd {
    /// Nodes, edges (source, target, kind, reference, declared) and unresolved imports
    Graph(Common),
    /// Measurements: density, propagation cost, cycles, depth, centrality, cut points
    Metrics(Common),
    /// Strongly connected components of runtime edges (every directed cycle)
    Cycles(Common),
    /// Articulation points and the domains each one separates
    Domains(Common),
    /// Who transitively depends on <node> (who breaks if it changes)
    BlastRadius {
        #[command(flatten)]
        common: Common,
        node: String,
    },
    /// What <node> transitively depends on
    Ancestors {
        #[command(flatten)]
        common: Common,
        node: String,
    },
    /// Intersection of the ancestor sets of two or more nodes
    CommonAncestors {
        #[command(flatten)]
        common: Common,
        #[arg(num_args = 2..)]
        nodes: Vec<String>,
    },
    /// Shortest reference path <from> -> <to>
    Path {
        #[command(flatten)]
        common: Common,
        from: String,
        to: String,
    },
    /// Test files that transitively reach any of the changed paths (module graph, tests included)
    AffectedTests {
        #[command(flatten)]
        common: Common,
        #[arg(num_args = 1..)]
        changed: Vec<String>,
    },
    /// Check laws (JSON file) against the graph; exit 1 on any violation
    Check {
        #[command(flatten)]
        common: Common,
        #[arg(long)]
        laws: PathBuf,
    },
}

impl Common {
    /// Every option at its default, rooted at `repo_root`.
    pub fn at(repo_root: &str) -> Common {
        use clap::FromArgMatches;
        let cmd = Common::augment_args(clap::Command::new("sem"));
        Common::from_arg_matches(&cmd.get_matches_from(["sem", "--repo-root", repo_root])).expect("defaults parse")
    }

    /// Defaults, with the repository root as a package too: single-package
    /// repos and apps nested without a workspaces entry get nodes.
    pub fn whole_repo(repo_root: &str) -> Common {
        use clap::FromArgMatches;
        let cmd = Common::augment_args(clap::Command::new("sem"));
        Common::from_arg_matches(&cmd.get_matches_from(["sem", "--repo-root", repo_root, "--root-package"])).expect("defaults parse")
    }
}

/// What laws are checked against: the workspace extraction is loaded only
/// when a graph or reference law asks for it (code-shape laws read files).
pub struct Ctx {
    common: Common,
    loaded: std::cell::OnceCell<Loaded>,
    misfits: std::cell::OnceCell<Vec<sem_core::parser::calls::fit::Misfit>>,
}

impl Ctx {
    pub fn new(common: Common) -> Ctx {
        Ctx { common, loaded: std::cell::OnceCell::new(), misfits: std::cell::OnceCell::new() }
    }

    fn l(&self) -> &Loaded {
        self.loaded.get_or_init(|| Loaded::load(self.common.clone()))
    }
}

struct Loaded {
    ex: Extraction,
    common: Common,
    t_extract: std::time::Duration,
}

impl Loaded {
    fn load(common: Common) -> Loaded {
        Self::load_in(common, None)
    }

    /// Reads only the files in `scope` (see `Extraction::run_in`).
    fn load_in(common: Common, scope: Option<&std::collections::HashSet<String>>) -> Loaded {
        let t0 = Instant::now();
        let root = PathBuf::from(&common.repo_root);
        let disc = Discovery {
            exclude_dirs: &common.exclude_dirs,
            skip_segments: &common.skip_dirs,
            extensions: &common.exts,
            root_package: common.root_package,
        };
        let ws = Workspace::discover(&root, &disc);
        let paths = ws.source_files(&disc);
        let ex = Extraction::run_in(&root, ws, &paths, scope);
        Loaded { ex, common, t_extract: t0.elapsed() }
    }

    fn kinds(&self, force_test: bool) -> Vec<FileKind> {
        let c = &self.common;
        let mut k = vec![FileKind::Prod];
        if c.include_test || force_test {
            k.push(FileKind::Test);
        }
        if c.include_example {
            k.push(FileKind::Example);
        }
        if c.include_build {
            k.push(FileKind::Build);
        }
        k
    }

    fn graph(&self, granularity: Option<Granularity>, reference: Option<Selector>, kinds: &[FileKind], no_world: bool) -> Graph {
        self.graph_with(granularity, reference, kinds, no_world, self.common.assets)
    }

    fn graph_with(
        &self,
        granularity: Option<Granularity>,
        reference: Option<Selector>,
        kinds: &[FileKind],
        no_world: bool,
        assets: bool,
    ) -> Graph {
        let c = &self.common;
        Graph::build(
            &self.ex,
            &Options {
                granularity: granularity.unwrap_or(match c.granularity {
                    GranularityArg::Package => Granularity::Package,
                    GranularityArg::Module => Granularity::Module,
                }),
                root: c.root.trim_end_matches('/'),
                kinds,
                reference: reference.unwrap_or(match c.reference_kind {
                    KindArg::Both => Selector::Both,
                    KindArg::Value => Selector::Value,
                    KindArg::Type => Selector::Type,
                }),
                no_world,
                assets,
            },
        )
    }

    fn default_graph(&self) -> Graph {
        self.graph(None, None, &self.kinds(false), self.common.no_world)
    }
}

/// The module-granularity runtime (value) reference graph of the JS/TS
/// workspace at `repo_root`, production files only: node ids and adjacency.
/// Empty when the root holds no JS/TS workspace.
/// JS/TS import resolution of the tree at `repo_root`: `(importing file,
/// specifier) -> repo file`, for every specifier that lands on a source file.
/// `scope`: only those importing files (all when `None`).
pub(crate) fn spec_targets(repo_root: &str, scope: Option<&std::collections::HashSet<String>>) -> std::collections::HashMap<(String, String), String> {
    let l = Loaded::load_in(Common::whole_repo(repo_root), scope);
    let mut out = std::collections::HashMap::new();
    for f in &l.ex.files {
        for r in &f.refs {
            if let sem_core::topology::resolve::Target::File(t) = &r.target {
                out.insert((f.path.clone(), r.specifier.clone()), t.clone());
            }
        }
    }
    out
}

/// Relative JS/TS imports that land on no file at all: `(importing file,
/// specifier)`.
pub(crate) fn broken_relative_imports(repo_root: &str, scope: Option<&std::collections::HashSet<String>>) -> BTreeSet<(String, String)> {
    use sem_core::topology::resolve::Target;
    let l = Loaded::load_in(Common::whole_repo(repo_root), scope);
    let root = std::path::Path::new(repo_root);
    let mut out = BTreeSet::new();
    for f in &l.ex.files {
        for r in &f.refs {
            if !r.specifier.starts_with('.') {
                continue;
            }
            if let Target::Path(p) = &r.target {
                let exists = root.join(p).exists()
                    || [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".json", ".d.ts", "/index.ts", "/index.js"].iter().any(|e| root.join(format!("{p}{e}")).exists());
                if !exists {
                    out.insert((f.path.clone(), r.specifier.clone()));
                }
            }
        }
    }
    out
}

/// With `scope`, the edges out of those files only.
pub(crate) fn module_value_graph(repo_root: &str, scope: Option<&std::collections::HashSet<String>>) -> (Vec<String>, algo::Adj) {
    let l = Loaded::load_in(Common::whole_repo(repo_root), scope);
    let g = l.graph(Some(Granularity::Module), Some(Selector::Value), &[FileKind::Prod], false);
    let adj = g.adjacency();
    (g.nodes.iter().map(|n| n.id.clone()).collect(), adj)
}

fn common_of(cmd: &TopologyCmd) -> Common {
    match cmd {
        TopologyCmd::Graph(c) | TopologyCmd::Metrics(c) | TopologyCmd::Cycles(c) | TopologyCmd::Domains(c) => c,
        TopologyCmd::BlastRadius { common, .. }
        | TopologyCmd::Ancestors { common, .. }
        | TopologyCmd::CommonAncestors { common, .. }
        | TopologyCmd::Path { common, .. }
        | TopologyCmd::AffectedTests { common, .. }
        | TopologyCmd::Check { common, .. } => common,
    }
    .clone()
}

pub fn run(cmd: TopologyCmd) -> Result<(), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    if let TopologyCmd::Check { common, laws } = cmd {
        let spec: Value = serde_json::from_str(&std::fs::read_to_string(laws)?)?;
        let ctx = Ctx::new(common);
        let results = check(&ctx, spec["laws"].as_array().map(Vec::as_slice).unwrap_or_default(), None)?;
        let total: u64 = results.iter().filter_map(|r| r["violations"].as_u64()).sum();
        println!("{}", serde_json::to_string_pretty(&json!({ "laws": results, "violations": total }))?);
        if ctx.common.timings {
            eprintln!("total {:?}", t0.elapsed());
        }
        if total > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }
    let l = Loaded::load(common_of(&cmd));
    let out = match cmd {
        TopologyCmd::Graph(_) => graph_json(&l.default_graph()),
        TopologyCmd::Metrics(_) => {
            let g = l.default_graph();
            serde_json::to_value(metrics::measure(&g, &|_| false))?
        }
        TopologyCmd::Cycles(_) => {
            let g = l.default_graph();
            cycles(&g, &g.adjacency_where(|e| e.reference == sem_core::topology::imports::RefKind::Value))
        }
        TopologyCmd::Domains(_) => domains(&l.default_graph()),
        TopologyCmd::BlastRadius { node, .. } => {
            let g = l.default_graph();
            closure(&g, &algo::reverse(&g.adjacency()), &node, "blast-radius")?
        }
        TopologyCmd::Ancestors { node, .. } => {
            let g = l.default_graph();
            closure(&g, &g.adjacency(), &node, "ancestors")?
        }
        TopologyCmd::CommonAncestors { nodes, .. } => common_ancestors(&l.default_graph(), &nodes)?,
        TopologyCmd::Path { from, to, .. } => path(&l.default_graph(), &from, &to)?,
        TopologyCmd::AffectedTests { changed, .. } => {
            // assets always: a changed json/css/font a module imports reaches the tests that load it
            let g = l.graph_with(Some(Granularity::Module), None, &[FileKind::Prod, FileKind::Test], true, true);
            affected_tests(&l, &g, &changed)
        }
        TopologyCmd::Check { .. } => unreachable!("handled above"),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    if l.common.timings {
        eprintln!("files {}  extract {:?}  total {:?}", l.ex.files.len(), l.t_extract, t0.elapsed());
    }
    Ok(())
}

fn graph_json(g: &Graph) -> Value {
    json!({
        "granularity": g.granularity,
        "nodes": g.nodes,
        "edges": g.edges.iter().map(|e| json!({
            "source": g.nodes[e.from].id, "target": g.nodes[e.to].id,
            "kind": e.kind, "reference": e.reference, "declared": e.declared,
        })).collect::<Vec<_>>(),
        "unresolved": g.unresolved,
    })
}

fn cycles(g: &Graph, adj: &algo::Adj) -> Value {
    let (_, comps) = algo::scc(adj);
    let mut cs: Vec<Vec<String>> = comps
        .into_iter()
        .filter(|c| c.len() > 1)
        .map(|c| {
            let mut v: Vec<String> = c.iter().map(|&u| g.nodes[u].id.clone()).collect();
            v.sort();
            v
        })
        .collect();
    cs.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    json!({ "components": cs })
}

/// Cut vertices of the undirected projection, each with the components of its own
/// component once it is removed.
fn domains(g: &Graph) -> Value {
    let adj = g.adjacency();
    let n = adj.len();
    let mut und: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (u, vs) in adj.iter().enumerate() {
        for &v in vs {
            if u != v {
                und[u].insert(v);
                und[v].insert(u);
            }
        }
    }
    let mut cuts: Vec<Value> = algo::articulation_points(&adj)
        .into_iter()
        .map(|a| {
            let mut seen = vec![false; n];
            seen[a] = true;
            let mut doms: Vec<Vec<String>> = Vec::new();
            for &s in &und[a] {
                if seen[s] {
                    continue;
                }
                seen[s] = true;
                let (mut stack, mut comp) = (vec![s], vec![]);
                while let Some(u) = stack.pop() {
                    comp.push(g.nodes[u].id.clone());
                    for &v in &und[u] {
                        if !seen[v] {
                            seen[v] = true;
                            stack.push(v);
                        }
                    }
                }
                comp.sort();
                doms.push(comp);
            }
            doms.sort();
            let isolates_world = doms.iter().any(|d| d.len() == 1 && g.find(&d[0]).is_some_and(|u| g.is_world(u)));
            json!({ "name": g.nodes[a].id, "domains": doms, "isolatesWorld": isolates_world })
        })
        .collect();
    cuts.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({ "cutVertices": cuts })
}

fn lookup(g: &Graph, id: &str) -> Result<usize, String> {
    g.find(id).ok_or_else(|| format!("no node `{id}` at {:?} granularity", g.granularity))
}

fn closure(g: &Graph, adj: &algo::Adj, node: &str, query: &str) -> Result<Value, String> {
    let s = lookup(g, node)?;
    let mut r: Vec<&str> = algo::reach(adj, s).into_iter().filter(|&v| v != s).map(|v| g.nodes[v].id.as_str()).collect();
    r.sort();
    Ok(json!({ "query": query, "subject": node, "results": r }))
}

fn common_ancestors(g: &Graph, nodes: &[String]) -> Result<Value, String> {
    let adj = g.adjacency();
    let mut acc: Option<BTreeSet<usize>> = None;
    for n in nodes {
        let s = lookup(g, n)?;
        let set: BTreeSet<usize> = algo::reach(&adj, s).into_iter().filter(|&v| v != s).collect();
        acc = Some(match acc {
            None => set,
            Some(a) => a.intersection(&set).copied().collect(),
        });
    }
    let mut r: Vec<&str> = acc.unwrap_or_default().into_iter().map(|v| g.nodes[v].id.as_str()).collect();
    r.sort();
    Ok(json!({ "query": "common-ancestors", "subject": nodes.join(" & "), "results": r }))
}

fn path(g: &Graph, from: &str, to: &str) -> Result<Value, String> {
    let (s, t) = (lookup(g, from)?, lookup(g, to)?);
    if s == t {
        return Ok(json!({ "query": "path", "from": from, "to": to, "found": true, "steps": [] }));
    }
    Ok(match algo::shortest_path(&g.adjacency(), s, t) {
        Some(p) => json!({ "query": "path", "from": from, "to": to, "found": true,
            "steps": p.windows(2).map(|w| {
                // witness: prefer a prod edge, then a value edge
                let e = g.edges.iter().filter(|e| e.from == w[0] && e.to == w[1])
                    .min_by_key(|e| (e.kind != FileKind::Prod, e.reference != sem_core::topology::imports::RefKind::Value)).unwrap();
                json!({ "from": g.nodes[w[0]].id, "to": g.nodes[w[1]].id, "kind": e.kind, "reference": e.reference })
            }).collect::<Vec<_>>() }),
        None => json!({ "query": "path", "from": from, "to": to, "found": false, "steps": [] }),
    })
}

fn affected_tests(l: &Loaded, g: &Graph, changed: &[String]) -> Value {
    let rev = algo::reverse(&g.adjacency());
    let root = l.common.root.trim_end_matches('/');
    let in_root = |p: &str| root.is_empty() || p.starts_with(&format!("{root}/"));
    let (mut matched, mut unmatched, mut out_of_scope) = (Vec::new(), Vec::new(), Vec::new());
    let mut paths: Vec<&String> = changed.iter().filter(|c| !c.is_empty()).collect();
    paths.sort();
    paths.dedup();
    for p in paths {
        // a snapshot file belongs to the test beside it: `dir/__snapshots__/x.test.ts.snap` -> `dir/x.test.ts`
        if let Some(t) = snapshot_owner(p).and_then(|t| g.find(&t)) {
            matched.push(t);
            continue;
        }
        if !in_root(p) {
            out_of_scope.push(p.clone());
        } else if let Some(u) = g.find(p) {
            matched.push(u);
        } else if l.ex.file(p).is_some() {
            out_of_scope.push(p.clone());
        } else {
            unmatched.push(p.clone());
        }
    }
    let is_test = |u: usize| g.nodes[u].file_kind == Some(FileKind::Test);
    let mut seen = vec![false; g.nodes.len()];
    let mut stack: Vec<usize> = matched.clone();
    for &m in &matched {
        seen[m] = true;
    }
    let (mut tests, mut direct) = (BTreeSet::new(), BTreeSet::new());
    for &m in &matched {
        if is_test(m) {
            tests.insert(g.nodes[m].id.clone());
            direct.insert(g.nodes[m].id.clone());
        }
        for &p in &rev[m] {
            if is_test(p) {
                direct.insert(g.nodes[p].id.clone());
            }
        }
    }
    while let Some(u) = stack.pop() {
        if is_test(u) {
            tests.insert(g.nodes[u].id.clone());
        }
        for &p in &rev[u] {
            if !seen[p] {
                seen[p] = true;
                stack.push(p);
            }
        }
    }
    json!({ "query": "affected-tests", "changedPaths": changed, "affectedTests": tests, "directlyAffectedTests": direct,
            "unmatchedChangedPaths": unmatched, "outOfScopeChangedPaths": out_of_scope })
}

/// The test file a jest/vitest snapshot belongs to (`a/__snapshots__/b.test.tsx.snap` -> `a/b.test.tsx`).
fn snapshot_owner(p: &str) -> Option<String> {
    let stem = p.strip_suffix(".snap")?;
    let (dir, leaf) = stem.rsplit_once('/').unwrap_or(("", stem));
    let owner_dir = if dir == "__snapshots__" { "" } else { dir.strip_suffix("/__snapshots__")? };
    Some(if owner_dir.is_empty() { leaf.to_string() } else { format!("{owner_dir}/{leaf}") })
}

// ---- laws ------------------------------------------------------------------

/// A glob or list of globs.
fn globs(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn any_match(pats: &[String], text: &str) -> bool {
    pats.iter().any(|p| glob::matches(p, text))
}

/// Laws file (JSON), `{ "laws": [ law, .. ] }`. Graph laws (`at`: package | module | manifest;
/// `kind`: value | type | both; `include`: file kinds, default prod) — node globs match package
/// names, or repo-relative file paths at module granularity:
///   { "forbid": { "from", "to", "transitive": true, "except" } }   no path/edge from -> to
///   { "only": { "to", "from": [..] } }                              only `from` may depend on `to`
///   { "acyclic": { "scope" } }                                      no cycle inside scope
///   { "layers": [lowest, .., highest] }                             no edge from a lower to a higher layer
/// Reference laws (over every import in scanned files; globs or lists of globs; optional
/// `kind`: value | type | both, default both — `value` skips `import type` / all-`type` specifiers):
///   { "forbidImport": { "from", "except", "specifier", "to", "forms" } }  matching files must not import it
///   { "allowImports": { "from", "except", "specifiers", "forms" } }       matching files import only these
///   { "noCrossPackageRelative": { "from", "except" } }                    relative imports stay in their package
/// Code-shape laws (a tree-sitter query; every non-`_` capture is a violation):
///   { "forbidPattern": { "from", "except", "query", "within" } }   within: node kinds the hit must be inside
/// Call-fit laws (Python): every call the resolver pins to one repo function fits its signature:
///   { "callsFit": { "from", "except" } }                           from/except: caller files (default **/*.py)
/// Any law may carry "promise": the human statement it verifies; results report "kept".
/// With `scope` (repo-relative paths), only violations involving those files are
/// reported: code-shape laws read only them; other laws run globally, then filter.
pub fn check(ctx: &Ctx, laws: &[Value], scope: Option<&BTreeSet<String>>) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    use sem_core::topology::resolve::Target;
    let mut cache: BTreeMap<String, Graph> = BTreeMap::new();
    let mut shapes = code_shapes(ctx, laws, scope)?;
    let mut results = Vec::new();
    let form_name = |f: sem_core::topology::imports::Form| serde_json::to_value(f).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    for (idx, law) in laws.iter().enumerate() {
        let mut v = shapes.remove(&idx).unwrap_or_default();
        // -- reference laws: evaluated on raw imports ------------------------
        if let Some(f) = law.get("forbidImport").or_else(|| law.get("allowImports")) {
            let l = ctx.l();
            let target_path = |t: &Target| -> Option<String> {
                match t {
                    Target::File(p) | Target::Path(p) => Some(p.clone()),
                    Target::Package(i) => Some(l.ex.ws.packages[*i].name.clone()),
                    Target::External(n) => Some(n.clone()),
                }
            };
            let allow = law.get("allowImports").is_some();
            let (from, except) = (globs(&f["from"]), globs(&f["except"]));
            let (specs, to) = (globs(if allow { &f["specifiers"] } else { &f["specifier"] }), globs(&f["to"]));
            let forms = globs(&f["forms"]);
            // optional reference kind filter, as for graph laws: value | type | both (default both)
            let sel = match law["kind"].as_str().unwrap_or("both") {
                "value" => Selector::Value,
                "type" => Selector::Type,
                _ => Selector::Both,
            };
            for file in l.ex.files.iter().filter(|x| any_match(&from, &x.path) && !any_match(&except, &x.path)) {
                for r in &file.refs {
                    if (!forms.is_empty() && !forms.contains(&form_name(r.form))) || !sel.admits(r.kind) {
                        continue;
                    }
                    let hit_spec = any_match(&specs, &r.specifier);
                    let hit_to = target_path(&r.target).is_some_and(|t| any_match(&to, &t));
                    let bad = if allow { !hit_spec } else { hit_spec || hit_to };
                    if bad {
                        v.push(json!({ "file": file.path, "line": r.line, "specifier": r.specifier }));
                    }
                }
            }
        }
        if let Some(f) = law.get("noCrossPackageRelative") {
            let l = ctx.l();
            let (from, except) = (globs(&f["from"]), globs(&f["except"]));
            for file in l.ex.files.iter().filter(|x| (from.is_empty() || any_match(&from, &x.path)) && !any_match(&except, &x.path)) {
                for r in file.refs.iter().filter(|r| r.specifier.starts_with('.')) {
                    let owner = match &r.target {
                        Target::File(p) | Target::Path(p) => l.ex.ws.owner_of(p),
                        _ => None,
                    };
                    if owner.is_some() && owner != file.package {
                        v.push(json!({ "file": file.path, "line": r.line, "specifier": r.specifier,
                            "into": l.ex.ws.packages[owner.unwrap()].name }));
                    }
                }
            }
        }
        if let Some(f) = law.get("callsFit") {
            let (from, except) = (globs(&f["from"]), globs(&f["except"]));
            let from = if from.is_empty() { vec!["**/*.py".to_string()] } else { from };
            for m in python_misfits(ctx)? {
                if any_match(&from, &m.file) && !any_match(&except, &m.file) {
                    v.push(json!({ "file": m.file, "line": m.line, "col": m.col, "text": m.text,
                        "problem": m.problem, "target": m.target, "targetFile": m.target_file }));
                }
            }
        }
        // -- graph laws -------------------------------------------------------
        let is_graph_law = ["forbid", "only", "acyclic", "layers"].iter().any(|k| law.get(*k).is_some());
        if is_graph_law {
            let l = ctx.l();
            let at = law["at"].as_str().unwrap_or("package");
            let sel = match law["kind"].as_str().unwrap_or("value") {
                "type" => Selector::Type,
                "both" => Selector::Both,
                _ => Selector::Value,
            };
            let kinds: Vec<FileKind> = law["include"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|k| match k.as_str()? {
                            "prod" => Some(FileKind::Prod),
                            "test" => Some(FileKind::Test),
                            "example" => Some(FileKind::Example),
                            "build" => Some(FileKind::Build),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_else(|| vec![FileKind::Prod]);
            let key = format!("{at}/{sel:?}/{kinds:?}");
            let g = cache.entry(key).or_insert_with(|| match at {
                "manifest" => Graph::manifest(&l.ex.ws),
                "module" => l.graph(Some(Granularity::Module), Some(sel), &kinds, false),
                _ => l.graph(Some(Granularity::Package), Some(sel), &kinds, false),
            });
            let adj = g.adjacency();
            let m = |pats: &[String], u: usize| any_match(pats, &g.nodes[u].id);
            if let Some(f) = law.get("forbid") {
                let (from, to, except) = (globs(&f["from"]), globs(&f["to"]), globs(&f["except"]));
                let bad = |t: usize| m(&to, t) && !m(&except, t);
                for s in (0..g.nodes.len()).filter(|&u| g.nodes[u].kind != NodeKind::World && m(&from, u)) {
                    if f["transitive"].as_bool().unwrap_or(true) {
                        for t in algo::reach(&adj, s).into_iter().filter(|&t| t != s && bad(t)) {
                            let p = algo::shortest_path(&adj, s, t).unwrap_or_default();
                            v.push(json!({ "from": g.nodes[s].id, "to": g.nodes[t].id,
                                "path": p.iter().map(|&u| g.nodes[u].id.as_str()).collect::<Vec<_>>() }));
                        }
                    } else {
                        for &t in adj[s].iter().filter(|&&t| bad(t)) {
                            v.push(json!({ "from": g.nodes[s].id, "to": g.nodes[t].id }));
                        }
                    }
                }
            }
            if let Some(o) = law.get("only") {
                let (to, allowed) = (globs(&o["to"]), globs(&o["from"]));
                for (u, vs) in adj.iter().enumerate() {
                    for &t in vs {
                        if m(&to, t) && !m(&to, u) && !m(&allowed, u) {
                            v.push(json!({ "from": g.nodes[u].id, "to": g.nodes[t].id }));
                        }
                    }
                }
            }
            if let Some(a) = law.get("acyclic") {
                let scope = { let s = globs(&a["scope"]); if s.is_empty() { vec!["**".to_string()] } else { s } };
                let keep: HashSet<usize> = (0..g.nodes.len()).filter(|&u| m(&scope, u)).collect();
                let sub: algo::Adj = (0..g.nodes.len())
                    .map(|u| if keep.contains(&u) { adj[u].iter().copied().filter(|x| keep.contains(x)).collect() } else { vec![] })
                    .collect();
                for c in algo::scc(&sub).1.into_iter().filter(|c| c.len() > 1) {
                    v.push(json!({ "cycle": c.iter().map(|&u| g.nodes[u].id.as_str()).collect::<Vec<_>>() }));
                }
            }
            if let Some(tiers) = law.get("layers").and_then(|x| x.as_array()) {
                let tiers: Vec<Vec<String>> = tiers.iter().map(globs).collect();
                let tier = |u: usize| tiers.iter().position(|p| m(p, u));
                for (u, vs) in adj.iter().enumerate() {
                    for &t in vs {
                        if let (Some(a), Some(b)) = (tier(u), tier(t)) {
                            if a < b {
                                v.push(json!({ "from": g.nodes[u].id, "to": g.nodes[t].id, "fromLayer": a, "toLayer": b }));
                            }
                        }
                    }
                }
            }
        }
        if let Some(scope) = scope {
            let mut involved = scope.clone();
            if let Some(l) = ctx.loaded.get() {
                involved.extend(scope.iter().filter_map(|p| l.ex.ws.owner_of(p)).map(|i| l.ex.ws.packages[i].name.clone()));
            }
            v.retain(|d| involves(d, &involved));
        }
        let mut r = json!({ "id": law["id"], "kept": v.is_empty(), "violations": v.len(), "details": v });
        if let Some(p) = law.get("promise") {
            r["promise"] = p.clone();
        }
        results.push(r);
    }
    Ok(results)
}

/// Does a violation name one of `set`? One with a `file` is judged by that file
/// (and, for a call that no longer fits, the file defining its target);
/// graph violations by any node they mention (file or package).
pub(crate) fn involves(v: &Value, set: &BTreeSet<String>) -> bool {
    match v {
        Value::String(s) => set.contains(s),
        Value::Array(a) => a.iter().any(|x| involves(x, set)),
        Value::Object(o) => o.get("file").map_or_else(
            || o.values().any(|x| involves(x, set)),
            |f| involves(f, set) || o.get("targetFile").is_some_and(|t| involves(t, set)),
        ),
        _ => false,
    }
}

/// Python call-fit misfits over every `.py` file under the repo root (not
/// git-ignored, not under a skipped directory), computed once per check.
fn python_misfits(ctx: &Ctx) -> Result<Vec<sem_core::parser::calls::fit::Misfit>, String> {
    if let Some(m) = ctx.misfits.get() {
        return Ok(m.clone());
    }
    let root = PathBuf::from(&ctx.common.repo_root);
    let skip = ctx.common.skip_dirs.clone();
    let walk = ignore::WalkBuilder::new(&root).filter_entry(move |e| !skip.iter().any(|s| e.file_name() == s.as_str())).build();
    let mut files: Vec<String> = walk
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && e.path().extension().is_some_and(|x| x == "py"))
        .filter_map(|e| e.path().strip_prefix(&root).ok().map(|p| p.to_string_lossy().replace('\\', "/")))
        .collect();
    files.sort();
    let m = sem_core::parser::calls::fit::python_misfits(&root, &files);
    let _ = ctx.misfits.set(m.clone());
    Ok(m)
}

/// Violations of every `forbidPattern` law (by law index), from one pass that
/// parses each candidate file once. Candidates: files under the repo root not
/// ignored by git, not under a skipped directory, and (with `scope`) in scope.
fn code_shapes(ctx: &Ctx, laws: &[Value], scope: Option<&BTreeSet<String>>) -> Result<BTreeMap<usize, Vec<Value>>, String> {
    use sem_core::topology::pattern::{scan, Pattern};
    let idx: Vec<usize> = (0..laws.len()).filter(|&i| laws[i].get("forbidPattern").is_some()).collect();
    if idx.is_empty() {
        return Ok(BTreeMap::new());
    }
    let spec = |i: usize| &laws[i]["forbidPattern"];
    let mut patterns: Vec<Pattern> = idx.iter().map(|&i| Pattern::new(spec(i)["query"].as_str().unwrap_or("")).within(globs(&spec(i)["within"]))).collect();
    let sel: Vec<(Vec<String>, Vec<String>)> = idx.iter().map(|&i| (globs(&spec(i)["from"]), globs(&spec(i)["except"]))).collect();
    let root = PathBuf::from(&ctx.common.repo_root);
    let skip = ctx.common.skip_dirs.clone();
    let walk = ignore::WalkBuilder::new(&root)
        .filter_entry(move |e| !skip.iter().any(|s| e.file_name() == s.as_str()))
        .build();
    let mut files = Vec::new();
    for e in walk.flatten().filter(|e| e.file_type().is_some_and(|t| t.is_file())) {
        let Ok(rel) = e.path().strip_prefix(&root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if scope.is_some_and(|s| !s.contains(&rel)) {
            continue;
        }
        let which: Vec<usize> = (0..sel.len()).filter(|&k| any_match(&sel[k].0, &rel) && !any_match(&sel[k].1, &rel)).collect();
        if !which.is_empty() {
            files.push((rel, which));
        }
    }
    files.sort();
    let hits = scan(&root, &files, &mut patterns)?;
    Ok(idx
        .into_iter()
        .zip(hits)
        .map(|(i, hs)| (i, hs.into_iter().map(|(file, h)| json!({ "file": file, "line": h.line, "col": h.col, "capture": h.capture, "text": h.text })).collect()))
        .collect())
}

#[cfg(test)]
mod snapshot_tests {
    use super::snapshot_owner;

    #[test]
    fn snapshot_belongs_to_the_test_beside_its_directory() {
        assert_eq!(snapshot_owner("pkg/tests/__snapshots__/a.test.tsx.snap").as_deref(), Some("pkg/tests/a.test.tsx"));
        assert_eq!(snapshot_owner("__snapshots__/b.test.ts.snap").as_deref(), Some("b.test.ts"));
        assert_eq!(snapshot_owner("pkg/a.test.tsx.snap"), None);
        assert_eq!(snapshot_owner("pkg/__snapshots__/a.test.tsx"), None);
    }
}
