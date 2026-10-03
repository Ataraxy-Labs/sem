//! `sem arch-diff --view` and `--html`: the architecture view of a change,
//! a presentation layer over the arch-diff report (`--json`).
//!
//! The report lists every fact sem derived; a large change yields hundreds.
//! The view is what a reviewer reads in a minute:
//!
//! - **collapse**: findings that state one fact become one item (data paths
//!   by sink module, sink class and the changed function they pass through;
//!   a new dependency that closes a cycle folds into the cycle; side effects
//!   by module and effect), the individual findings kept as witnesses;
//! - **zoom**: entities are named by module (`module.Function`), modules by
//!   role where the path says so (config, request handling, database, ...);
//! - **rank** by whether a human has to decide something: untrusted input
//!   reaching a dangerous sink, contracts broken with callers left behind,
//!   imports of missing modules, new cycles, layer breaks, new couplings,
//!   then complexity jumps; at most [`VIEW_ITEMS`] items, the rest counted;
//! - **explain**: every item has a "what changed" and a "why it matters"
//!   line, from templates (deterministic, no model);
//! - **unchanged** concerns and modules, and the **uncertainty** (calls the
//!   analysis could not resolve in the changed code), each in one line.
//!
//! [`build_view`] reads the report JSON only, so a saved report re-renders
//! (`sem arch-diff --from-json report.json --view`). Reports without the
//! `changed` and `moduleGraph` keys (older sem) degrade: dependency classes
//! and the untouched-module count are then omitted.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{json, Value};

use sem_core::topology::algo;

use super::arch_diff::{is_test_file, package_of, Deps};

/// Items shown in the ranked list.
pub const VIEW_ITEMS: usize = 10;
/// Score at or above which an item is marked "needs a human decision".
const DECISION: i64 = 55;
/// Score below which an item is only counted, never listed.
const LISTED: i64 = 30;
/// Nodes drawn in the HTML module graph.
const GRAPH_NODES: usize = 40;

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// -- module graph (computed where the dependency graphs exist) ------------------

/// Longest path (in edges) from each node to a sink of the condensation.
fn depths(adj: &algo::Adj) -> Vec<usize> {
    let (comp, comps) = algo::scc(adj);
    let mut d = vec![0usize; comps.len()];
    for (c, members) in comps.iter().enumerate() {
        let mut x = 0;
        for &u in members {
            for &v in &adj[u] {
                if comp[v] != c {
                    x = x.max(d[comp[v]] + 1);
                }
            }
        }
        d[c] = x;
    }
    (0..adj.len()).map(|u| d[comp[u]]).collect()
}

/// The package graph around a change, for the view: the packages the change
/// touches, the endpoints of added/removed package edges and their nearest
/// neighbours (at most [`GRAPH_NODES`]), each edge tagged `new`, `removed`
/// or `kept`; and every new edge between two packages that already existed,
/// classified against the base graph:
///
/// - `couples-distant`: the source reached the target only through more
///   than two other packages (in a dense graph almost everything reaches
///   everything; a new direct edge across that distance is still a new
///   coupling);
/// - `inside-cycle`: both were already in one cycle (another link in it);
/// - `closes-cycle`: the target already reached the source;
/// - `already-indirect`: the source already reached the target (a shortcut
///   along the existing direction: fits the layers);
/// - `inverts-layers`: neither reached the other, and the target sits higher
///   (longer dependency chain below it) than the source;
/// - `couples-independent`: neither reached the other.
pub(crate) fn module_graph(bd: &Deps, hd: &Deps, changed_files: &BTreeSet<String>) -> Value {
    let (bp, hp) = (bd.packages(), hd.packages());
    let be: BTreeSet<(String, String)> = bd.pkg_edges.keys().cloned().collect();
    let he: BTreeSet<(String, String)> = hd.pkg_edges.keys().cloned().collect();
    let badj = Deps::adj(&bp, be.iter().cloned());
    let hadj = Deps::adj(&hp, he.iter().cloned());
    let bidx: HashMap<&str, usize> = bp.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let hidx: HashMap<&str, usize> = hp.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let (bdep, hdep) = (depths(&badj), depths(&hadj));
    let touched: BTreeSet<String> = changed_files
        .iter()
        .filter(|f| !is_test_file(f))
        .map(|f| package_of(f))
        .filter(|p| bidx.contains_key(p.as_str()) || hidx.contains_key(p.as_str()))
        .collect();
    let mut new_edges = Vec::new();
    for (a, b) in he.difference(&be) {
        let (Some(&ia), Some(&ib)) = (bidx.get(a.as_str()), bidx.get(b.as_str())) else { continue };
        let (b_reaches_a, a_reaches_b) = (algo::reach(&badj, ib).binary_search(&ia).is_ok(), algo::reach(&badj, ia).binary_search(&ib).is_ok());
        // hops from source to target at base: a long way round is not a layer the new edge "fits"
        let hops = if a_reaches_b { algo::shortest_path(&badj, ia, ib).map(|p| p.len().saturating_sub(1)) } else { None };
        let class = if a_reaches_b && hops.is_some_and(|h| h > 3) {
            "couples-distant"
        } else if b_reaches_a && a_reaches_b {
            "inside-cycle"
        } else if b_reaches_a {
            "closes-cycle"
        } else if a_reaches_b {
            "already-indirect"
        } else if bdep[ib] > bdep[ia] {
            "inverts-layers"
        } else {
            "couples-independent"
        };
        let w = &hd.pkg_edges[&(a.clone(), b.clone())];
        new_edges.push(json!({ "from": a, "to": b, "class": class, "witness": [w.0, w.1], "depth": [bdep[ia], bdep[ib]], "hops": hops }));
    }
    // nodes: touched and changed-edge endpoints first, then neighbours by degree
    let changed_edges: Vec<&(String, String)> = he.symmetric_difference(&be).collect();
    let mut first: Vec<String> = changed_edges.iter().flat_map(|(a, b)| [a.clone(), b.clone()]).collect();
    first.extend(touched.iter().cloned());
    let mut seen = BTreeSet::new();
    first.retain(|p| seen.insert(p.clone()));
    first.truncate(GRAPH_NODES);
    let core: BTreeSet<String> = first.iter().cloned().collect();
    let mut deg: BTreeMap<String, usize> = BTreeMap::new();
    for (a, b) in be.union(&he) {
        match (core.contains(a), core.contains(b)) {
            (true, false) => *deg.entry(b.clone()).or_default() += 1,
            (false, true) => *deg.entry(a.clone()).or_default() += 1,
            _ => {}
        }
    }
    let mut cand: Vec<(String, usize)> = deg.into_iter().collect();
    cand.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
    let room = GRAPH_NODES.saturating_sub(first.len());
    let hidden = cand.len().saturating_sub(room);
    let mut shown = first.clone();
    shown.extend(cand.into_iter().take(room).map(|(p, _)| p));
    let set: BTreeSet<&str> = shown.iter().map(String::as_str).collect();
    let nodes: Vec<Value> = shown
        .iter()
        .map(|p| {
            let depth = hidx.get(p.as_str()).map(|&i| hdep[i]).or_else(|| bidx.get(p.as_str()).map(|&i| bdep[i]));
            json!({ "id": p, "depth": depth, "touched": touched.contains(p),
                "added": !bidx.contains_key(p.as_str()), "removed": !hidx.contains_key(p.as_str()) })
        })
        .collect();
    let edges: Vec<Value> = be
        .union(&he)
        .filter(|(a, b)| set.contains(a.as_str()) && set.contains(b.as_str()))
        .map(|(a, b)| {
            let status = match (be.contains(&(a.clone(), b.clone())), he.contains(&(a.clone(), b.clone()))) {
                (false, true) => "new",
                (true, false) => "removed",
                _ => "kept",
            };
            json!({ "from": a, "to": b, "status": status })
        })
        .collect();
    json!({ "packages": hp.len(), "packagesBase": bp.len(), "touched": touched, "nodes": nodes, "edges": edges,
        "newEdges": new_edges, "hiddenNeighbors": hidden })
}

// -- naming ---------------------------------------------------------------------

const GENERIC_SEGS: &[&str] = &["src", "lib", "internal", "pkg", "source", "sources", "main", "java", "kotlin", "python", "go", "rust", "ts", "js"];
const VAGUE_SEGS: &[&str] = &[
    "utils", "util", "common", "helpers", "helper", "types", "core", "components", "models", "shared", "base", "api", "handlers", "services", "service", "impl", "server", "client", "config", "tools", "misc",
];

/// Short name of a package directory: its last meaningful segment,
/// prefixed by the parent when that segment says little on its own.
pub(crate) fn mod_label(pkg: &str) -> String {
    if pkg == "." || pkg.is_empty() {
        return "(root)".to_string();
    }
    let segs: Vec<&str> = pkg.split('/').filter(|x| !x.is_empty()).collect();
    let mut i = segs.len();
    while i > 1 && GENERIC_SEGS.contains(&segs[i - 1].to_ascii_lowercase().as_str()) {
        i -= 1;
    }
    let last = segs[i - 1];
    if i > 1 && VAGUE_SEGS.contains(&last.to_ascii_lowercase().as_str()) {
        format!("{}/{}", segs[i - 2], last)
    } else {
        last.to_string()
    }
}

fn ent_label(file: &str, entity: &str) -> String {
    format!("{}.{}", mod_label(&package_of(file)), entity)
}

fn tokens(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
            cur.push(c.to_ascii_lowercase());
        } else {
            prev_lower = false;
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

const ROLES: &[(&str, &[&str])] = &[
    ("auth", &["auth", "authn", "authz", "login", "logout", "session", "sessions", "jwt", "oauth", "oidc", "permission", "permissions", "acl", "rbac", "credential", "credentials", "password", "passwords", "crypto", "security"]),
    ("config", &["config", "configs", "conf", "settings", "cfg", "configuration", "dotenv"]),
    ("request handling", &["handler", "handlers", "router", "routers", "route", "routes", "controller", "controllers", "endpoint", "endpoints", "mcp", "middleware", "middlewares", "webhook", "webhooks", "views", "trpc", "rpc", "server", "api", "http"]),
    ("database", &["db", "database", "databases", "store", "storage", "repository", "repositories", "dao", "sql", "sqlite", "postgres", "mysql", "migrations", "persistence", "orm"]),
    ("network", &["net", "network", "networking", "transport", "transports", "grpc", "p2p", "socket", "sockets", "fetch", "download", "upload"]),
    ("disk", &["fs", "filesystem", "file", "files", "archive", "archives", "zip", "tar", "disk", "io", "cache"]),
    ("logging", &["log", "logs", "logger", "logging", "telemetry", "tracing", "metrics"]),
    ("subprocess", &["exec", "process", "subprocess", "shell", "spawn"]),
];

/// The role a path (package or file) names, if any.
pub(crate) fn path_role(path: &str) -> Option<&'static str> {
    let t = tokens(path);
    ROLES.iter().find(|(_, words)| t.iter().any(|x| words.contains(&x.as_str()))).map(|(r, _)| *r)
}

fn is_handler(file: &str) -> bool {
    path_role(file) == Some("request handling")
}

fn is_auth(path: &str) -> bool {
    path_role(path) == Some("auth")
}

// -- trust of sources, danger of sinks -------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Trust {
    /// request / network input: anyone who can reach the service
    Untrusted,
    /// config files, environment, command line: whoever deploys or runs it
    Operator,
    /// file contents, database rows: whoever can write them
    Stored,
}

fn source_role(class: &str, file: &str, entity: &str) -> (&'static str, Trust) {
    match class {
        "http-input" => ("HTTP request input", Trust::Untrusted),
        "net-input" => ("network input", Trust::Untrusted),
        "cli-input" => ("command-line arguments", Trust::Operator),
        "env" => ("environment variables", Trust::Operator),
        "db-read" => ("database rows", Trust::Stored),
        "file-read" if path_role(&package_of(file)) == Some("config") || path_role(file) == Some("config") || path_role(entity) == Some("config") => ("config", Trust::Operator),
        "file-read" => ("file contents", Trust::Stored),
        _ => ("data", Trust::Stored),
    }
}

fn sink_phrase(class: &str) -> &'static str {
    match class {
        "file-path" => "opens a file path",
        "file-write" => "writes files to disk",
        "exec" => "runs a subprocess",
        "db" => "builds a database query",
        "template" => "renders an HTML template",
        "http-response" => "writes an HTTP response",
        "net-send" => "sends a network request",
        "log" => "writes logs",
        _ => "reaches a sink",
    }
}

fn flow_score(t: Trust, env: bool, sink: &str) -> i64 {
    let dangerous = ["exec", "db", "template", "file-path", "file-write"].contains(&sink);
    match t {
        Trust::Untrusted if dangerous || ["http-response", "net-send"].contains(&sink) => 85,
        Trust::Untrusted => 25,
        Trust::Operator if env && dangerous => 50,
        Trust::Operator if env && ["log", "net-send", "http-response"].contains(&sink) => 45,
        Trust::Operator if dangerous => 60,
        Trust::Operator => 15,
        Trust::Stored if ["exec", "db", "template"].contains(&sink) => 58,
        Trust::Stored if ["file-path", "file-write"].contains(&sink) => 55,
        Trust::Stored if sink == "http-response" => 45,
        Trust::Stored if sink == "net-send" => 28,
        Trust::Stored => 12,
    }
}

fn flow_why(t: Trust, src: &str, sink: &str) -> String {
    let who = match t {
        Trust::Untrusted => "a crafted request value".to_string(),
        Trust::Operator if src == "environment variables" => "a changed environment".to_string(),
        Trust::Operator => format!("tampered {src}"),
        Trust::Stored => format!("anyone who can edit those {}", if src == "database rows" { "rows" } else { "files" }),
    };
    match sink {
        "file-path" | "file-write" => format!("{who} could read or write files outside the intended folder (path traversal)."),
        "exec" => format!("{who} could run arbitrary commands (command injection)."),
        "db" => format!("{who} could change what the query does (SQL injection)."),
        "template" | "http-response" if t == Trust::Operator && src == "environment variables" => "secrets in environment variables could be shown to users.".to_string(),
        "template" | "http-response" => format!("{who} could inject script into pages (XSS)."),
        "net-send" if t == Trust::Untrusted => "a caller could make the server contact hosts they choose (SSRF).".to_string(),
        "net-send" if src == "environment variables" => "secrets in environment variables could leave the process.".to_string(),
        "net-send" => "local data now leaves the process; check nothing sensitive is sent.".to_string(),
        "log" if src == "environment variables" => "secrets in environment variables may end up in logs.".to_string(),
        "log" => "sensitive values may end up in logs.".to_string(),
        _ => "data now crosses a boundary it did not cross before.".to_string(),
    }
}

const EFFECTS: &[&str] = &["exec", "db", "net-send", "file-write", "file-path", "template", "http-response"];

fn effect_phrase(e: &str) -> &'static str {
    match e {
        "file-path" => "open files by path",
        "file-write" => "write files",
        "exec" => "run subprocesses",
        "db" => "query the database",
        "net-send" => "make network calls",
        "template" => "render HTML templates",
        "http-response" => "write HTTP responses",
        _ => "have new side effects",
    }
}

fn handler_why(e: &str) -> &'static str {
    match e {
        "file-path" | "file-write" => "this is request-handling code: if any part of the path comes from the caller, it must be confined to the intended folder (path traversal). sem does not model this handler's inputs, so check by hand.",
        "exec" => "this is request-handling code: if any part of the command comes from the caller, it could run arbitrary commands. sem does not model this handler's inputs, so check by hand.",
        "db" => "this is request-handling code: if any part of the query comes from the caller, it must be parameterised (SQL injection). sem does not model this handler's inputs, so check by hand.",
        "net-send" => "this is request-handling code: if any part of the URL or host comes from the caller, the server can be pointed at internal hosts (SSRF). sem does not model this handler's inputs, so check by hand.",
        _ => "this is request-handling code: if its input reaches this effect unchecked, callers control it. sem does not model this handler's inputs, so check by hand.",
    }
}

fn compiled(file: &str) -> bool {
    [".go", ".rs", ".ts", ".tsx", ".mts", ".cts", ".java", ".kt", ".cs", ".swift", ".c", ".cpp", ".h", ".scala"].iter().any(|e| file.ends_with(e))
}

// -- items -------------------------------------------------------------------------

#[derive(Default)]
struct Item {
    score: i64,
    cat: &'static str,
    tag: String,
    chain: Vec<String>,
    what: String,
    why: String,
    loc: Option<String>,
    modules: BTreeSet<String>,
    files: BTreeSet<String>,
    entities: BTreeSet<String>,
    witnesses: Vec<Value>,
    merged: usize,
    /// module-graph edges this item is about: (from package, to package, kind)
    edges: Vec<(String, String, &'static str)>,
    /// the module the item is anchored in (a flow's sink module)
    anchor: String,
}

impl Item {
    fn to_json(&self, rank: usize) -> Value {
        json!({ "rank": rank, "score": self.score, "category": self.cat, "tag": self.tag, "decision": self.score >= DECISION,
            "chain": self.chain, "what": self.what, "why": self.why, "location": self.loc,
            "modules": self.modules, "files": self.files, "entities": self.entities,
            "merged": self.merged, "witnesses": self.witnesses,
            "edges": self.edges.iter().map(|(a, b, k)| json!({ "from": a, "to": b, "kind": k })).collect::<Vec<_>>() })
    }
}

struct Step {
    file: String,
    entity: String,
}

fn parse_step(t: &str) -> Option<Step> {
    let (loc, rest) = t.split_once(' ')?;
    let (file, line) = loc.rsplit_once(':')?;
    line.parse::<i64>().ok()?;
    let entity = rest.split_once(": ").map_or(rest, |x| x.0).to_string();
    Some(Step { file: file.to_string(), entity })
}

fn last_seg(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

/// What the change touched: files and (file, name) entities.
struct Changed {
    files: BTreeSet<String>,
    ents: BTreeSet<(String, String)>,
    exact: bool,
}

impl Changed {
    fn of(r: &Value) -> Changed {
        let mut files = BTreeSet::new();
        let mut ents = BTreeSet::new();
        let exact = r["changed"].is_array();
        if exact {
            for e in arr(&r["changed"]) {
                files.insert(s(&e["file"]));
                ents.insert((s(&e["file"]), last_seg(&s(&e["name"])).to_string()));
            }
        } else {
            // older reports: the entities the report itself names as changed
            for c in arr(&r["complexity"]) {
                files.insert(s(&c["file"]));
                ents.insert((s(&c["file"]), last_seg(&s(&c["name"])).to_string()));
            }
            for f in arr(&r["findings"]) {
                let d = &f["data"];
                match f["kind"].as_str() {
                    Some("signature-change") => {
                        files.insert(s(&d["file"]));
                        ents.insert((s(&d["file"]), last_seg(&s(&d["entity"])).to_string()));
                    }
                    Some("side-effect-change" | "new-entity-effects") => {
                        let k = s(&d["entity"]);
                        if let Some((file, name)) = k.split_once("::") {
                            files.insert(file.to_string());
                            ents.insert((file.to_string(), last_seg(name).to_string()));
                        }
                    }
                    Some("broken-import") => {
                        files.insert(s(&d["file"]));
                    }
                    _ => {}
                }
            }
        }
        Changed { files, ents, exact }
    }

    fn has(&self, file: &str, entity: &str) -> bool {
        self.ents.contains(&(file.to_string(), last_seg(entity).to_string()))
    }
}

fn entity_of_key(k: &str) -> (String, String) {
    match k.split_once("::") {
        Some((f, rest)) => (f.to_string(), rest.rsplit("::").next().unwrap_or(rest).to_string()),
        None => (String::new(), k.to_string()),
    }
}

fn flow_items(fs: &[Value], ch: &Changed, dropped_same_call: &mut usize) -> Vec<Item> {
    #[derive(Default)]
    struct G {
        score: i64,
        trust: Option<Trust>,
        srcs: BTreeSet<(Trust, &'static str)>,
        sink_class: String,
        sink_mod: String,
        sinks: BTreeMap<String, usize>,
        pivots: BTreeMap<String, usize>,
        loc: String,
        vias: BTreeSet<String>,
        members: Vec<Value>,
        files: BTreeSet<String>,
        ents: BTreeSet<String>,
        mods: Vec<String>,
        edges: BTreeSet<(String, String)>,
        weak: &'static str,
        weak_merged: usize,
    }
    // one fact per (sink class, sink module, confidence): the sources and
    // the changed functions it passes through are listed, not split on
    let mut groups: BTreeMap<(String, String, &'static str), G> = BTreeMap::new();
    for f in fs {
        let kind = f["kind"].as_str().unwrap_or("");
        if kind != "new-data-path" && kind != "new-possible-path" {
            continue;
        }
        let d = &f["data"];
        let (src, snk) = (&d["source"], &d["sink"]);
        // a read and a path sink on the same call (open(path)) is one operation, not a flow
        if src["file"] == snk["file"] && src["line"] == snk["line"] && src["entity"] == snk["entity"] && src["via"] == snk["via"] {
            *dropped_same_call += 1;
            continue;
        }
        let (sclass, kclass) = (s(&src["class"]), s(&snk["class"]));
        let (sfile, kfile) = (s(&src["file"]), s(&snk["file"]));
        let (srole, trust) = source_role(&sclass, &sfile, &s(&src["entity"]));
        let possible = kind == "new-possible-path";
        let object_state = f["title"].as_str().is_some_and(|t| t.contains("via object state")) || d["throughState"].as_str().is_some_and(|x| x.ends_with(".*"));
        let weak = if possible { "possible" } else if object_state { "object-state" } else { "" };
        let mut score = flow_score(trust, sclass == "env", &kclass);
        if possible {
            score -= 30;
        } else if object_state {
            score -= 35;
        }
        let steps: Vec<Step> = arr(&d["path"]).iter().filter_map(|x| parse_step(&s(x))).collect();
        let sink_ent = s(&snk["entity"]);
        let pivot = steps.iter().skip(1).find(|st| ch.has(&st.file, &st.entity) && !(st.file == kfile && last_seg(&st.entity) == last_seg(&sink_ent)));
        let sink_mod = package_of(&kfile);
        let g = groups.entry((kclass.clone(), sink_mod.clone(), weak)).or_default();
        if g.members.is_empty() || score > g.score {
            g.score = score;
            g.loc = format!("{}:{}", kfile, snk["line"]);
        }
        g.trust = Some(g.trust.map_or(trust, |t| t.min(trust)));
        g.sink_class = kclass.clone();
        g.sink_mod = sink_mod.clone();
        g.weak = weak;
        *g.sinks.entry(ent_label(&kfile, &sink_ent)).or_default() += 1;
        if let Some(p) = pivot {
            *g.pivots.entry(ent_label(&p.file, &p.entity)).or_default() += 1;
        }
        g.srcs.insert((trust, srole));
        g.vias.insert(s(&snk["via"]));
        for st in &steps {
            g.files.insert(st.file.clone());
            g.ents.insert(format!("{}::{}", st.file, st.entity));
        }
        g.files.insert(sfile.clone());
        g.files.insert(kfile.clone());
        let mut chain_mods = vec![package_of(&sfile)];
        if let Some(p) = pivot {
            chain_mods.push(package_of(&p.file));
        }
        chain_mods.push(sink_mod.clone());
        chain_mods.dedup();
        for w in chain_mods.windows(2) {
            g.edges.insert((w[0].clone(), w[1].clone()));
        }
        for m in chain_mods {
            if !g.mods.contains(&m) {
                g.mods.push(m);
            }
        }
        g.members.push(json!({ "text": s(&f["title"]), "steps": d["path"], "source": format!("{}:{}", sfile, src["line"]), "sink": format!("{}:{}", kfile, snk["line"]), "sinkEntity": format!("{kfile}::{sink_ent}") }));
    }
    let most = |m: &BTreeMap<String, usize>| -> Vec<String> {
        let mut v: Vec<(&String, &usize)> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.into_iter().map(|(k, _)| k.clone()).collect()
    };
    // a weak group (possible, or via object state) whose sink class and
    // module also have a definite group is the same fact: its paths become
    // extra witnesses there
    let keys: Vec<(String, String, &'static str)> = groups.keys().cloned().collect();
    for k in keys {
        if k.2.is_empty() || !groups.contains_key(&(k.0.clone(), k.1.clone(), "")) {
            continue;
        }
        let weak = groups.remove(&k).unwrap_or_default();
        let g = groups.get_mut(&(k.0.clone(), k.1.clone(), "")).expect("definite group");
        g.weak_merged += weak.members.len();
        g.members.extend(weak.members);
        g.files.extend(weak.files);
        g.ents.extend(weak.ents);
    }
    groups
        .into_values()
        .map(|g| {
            let trust = g.trust.unwrap_or(Trust::Stored);
            let mut v: Vec<(Trust, &str)> = g.srcs.iter().cloned().collect();
            v.sort();
            let mut srcs: Vec<&str> = Vec::new();
            for (_, r) in v {
                if !srcs.contains(&r) {
                    srcs.push(r);
                }
            }
            let src_text = match srcs.len() {
                1 => srcs[0].to_string(),
                2 => format!("{} and {}", srcs[0], srcs[1]),
                n => format!("{}, {} and {}", srcs[0], srcs[1], plural(n - 2, "other source", "other sources")),
            };
            let src_short = if srcs.len() <= 2 { srcs.join(" + ") } else { format!("{} + {} + {} more", srcs[0], srcs[1], srcs.len() - 2) };
            let sinks = most(&g.sinks);
            let pivots = most(&g.pivots);
            let sink_text = if sinks.len() == 1 { sinks[0].clone() } else { format!("{} (+{} more in {})", sinks[0], sinks.len() - 1, mod_label(&g.sink_mod)) };
            let mut chain = vec![src_short];
            if let Some(p) = pivots.first() {
                chain.push(p.clone());
            }
            chain.push(sinks[0].clone());
            chain.push(sink_phrase(&g.sink_class).to_string());
            let n = g.members.len();
            let vias: Vec<String> = g.vias.iter().filter(|x| !x.is_empty()).cloned().collect();
            let mut what = format!(
                "{} now flow{} into {}, which {}{}",
                capitalize(&src_text),
                if srcs.len() == 1 && !src_text.ends_with('s') { "s" } else { "" },
                sink_text,
                sink_phrase(&g.sink_class),
                if vias.is_empty() { String::new() } else { format!(" ({})", names(&vias, 3)) }
            );
            if !pivots.is_empty() {
                what += &format!(", via {} (changed here)", names(&pivots, 2));
            }
            what += ".";
            if n > 1 {
                what += &format!(" [{} paths{}]", n, if g.weak_merged > 0 { format!(", {} of them weaker (shared state or name-only)", g.weak_merged) } else { String::new() });
            }
            let mut why = format!("Why it matters: {}", flow_why(trust, srcs[0], &g.sink_class));
            match g.weak {
                "possible" => why += " (Possible path: the receiver's type is unknown; the method name matches a sink.)",
                "object-state" => why += " (Through shared object state; may conflate instances.)",
                _ => {}
            }
            let tag = if trust == Trust::Untrusted && g.weak.is_empty() { "NEW UNTRUSTED FLOW" } else { "NEW DATA FLOW" };
            Item {
                score: g.score,
                cat: "flow",
                tag: tag.to_string(),
                chain,
                what,
                why,
                loc: Some(g.loc),
                modules: g.mods.iter().cloned().collect(),
                files: g.files,
                entities: g.ents,
                witnesses: g.members.into_iter().take(25).collect(),
                merged: n,
                edges: g.edges.into_iter().map(|(a, b)| (a, b, "flow")).collect(),
                anchor: g.sink_mod.clone(),
            }
        })
        .collect()
}

/// Flows that do not need a decision (e.g. environment -> logs) are one
/// item per (source, sink class) across modules, not one per module.
fn merge_minor_flows(items: Vec<Item>) -> Vec<Item> {
    let mut out = Vec::new();
    let mut minor: BTreeMap<(String, String, String), Vec<Item>> = BTreeMap::new();
    for it in items {
        if it.score >= DECISION || it.chain.len() < 2 {
            out.push(it);
        } else {
            let key = (it.chain[0].clone(), it.chain.last().cloned().unwrap_or_default(), it.why.clone());
            minor.entry(key).or_default().push(it);
        }
    }
    for ((src, phrase, why), mut v) in minor {
        if v.len() == 1 {
            out.extend(v);
            continue;
        }
        v.sort_by(|a, b| b.score.cmp(&a.score).then(b.merged.cmp(&a.merged)));
        let mods: Vec<String> = v.iter().map(|i| mod_label(&i.anchor)).fold(Vec::new(), |mut acc, m| {
            if !acc.contains(&m) {
                acc.push(m);
            }
            acc
        });
        let paths: usize = v.iter().map(|i| i.merged).sum();
        let mut m = Item {
            score: v[0].score,
            cat: "flow",
            tag: v[0].tag.clone(),
            chain: vec![src.clone(), format!("{} modules", mods.len()), phrase.clone()],
            what: format!("{} now flow{} into code that {} in {} modules: {}. [{paths} paths]", capitalize(&src.replace(" + ", " and ")), if src.ends_with('s') || src.contains(" + ") { "" } else { "s" }, phrase, mods.len(), names(&mods, 4)),
            why,
            loc: v[0].loc.clone(),
            merged: paths,
            ..Default::default()
        };
        for i in v {
            m.modules.extend(i.modules);
            m.files.extend(i.files);
            m.entities.extend(i.entities);
            m.edges.extend(i.edges);
            m.witnesses.extend(i.witnesses);
        }
        m.witnesses.truncate(25);
        m.edges.sort();
        m.edges.dedup();
        out.push(m);
    }
    out
}

fn capitalize(x: &str) -> String {
    let mut c = x.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn names(v: &[String], max: usize) -> String {
    let mut out = v.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if v.len() > max {
        out += &format!(" and {} more", v.len() - max);
    }
    out
}

/// Builds the view model from an arch-diff report.
pub fn build_view(r: &Value) -> Value {
    let fs = arr(&r["findings"]);
    let ch = Changed::of(r);
    let mg = &r["moduleGraph"];
    let dep_class: HashMap<(String, String), String> = arr(&mg["newEdges"]).iter().map(|e| ((s(&e["from"]), s(&e["to"])), s(&e["class"]))).collect();
    let dep_hops: HashMap<(String, String), usize> = arr(&mg["newEdges"]).iter().filter_map(|e| Some(((s(&e["from"]), s(&e["to"])), e["hops"].as_u64()? as usize))).collect();
    let mut items: Vec<Item> = Vec::new();
    let mut dropped_same_call = 0usize;
    items.extend(merge_minor_flows(flow_items(&fs, &ch, &mut dropped_same_call)));
    // a function already shown as a flow's sink needs no separate side-effect item
    let flow_sink_ents: BTreeSet<String> = items.iter().filter(|i| i.score >= LISTED).flat_map(|i| i.witnesses.iter().map(|w| s(&w["sinkEntity"]))).collect();

    // -- cycles, with the new dependencies that close them --------------------------
    let new_deps: Vec<&Value> = fs.iter().filter(|f| f["kind"] == "new-package-dependency").collect();
    let mut folded: BTreeSet<(String, String)> = BTreeSet::new();
    for f in fs.iter().filter(|f| f["kind"] == "new-cycle") {
        let d = &f["data"];
        let members: Vec<String> = arr(&d["members"]).iter().map(s).collect();
        let newly: Vec<String> = arr(&d["newMembers"]).iter().map(s).collect();
        let level = s(&d["level"]);
        let grown = newly.len() < members.len();
        let mset: BTreeSet<&str> = members.iter().map(String::as_str).collect();
        let closers: Vec<(String, String, Value)> = if level == "package" {
            new_deps
                .iter()
                .filter(|x| mset.contains(s(&x["data"]["from"]).as_str()) && mset.contains(s(&x["data"]["to"]).as_str()))
                .map(|x| (s(&x["data"]["from"]), s(&x["data"]["to"]), x["data"]["witness"].clone()))
                .collect()
        } else {
            Vec::new()
        };
        for (a, b, _) in &closers {
            folded.insert((a.clone(), b.clone()));
        }
        let label = |m: &str| if level == "package" { mod_label(m) } else { m.rsplit('/').next().unwrap_or(m).to_string() };
        let labels: Vec<String> = members.iter().map(|m| label(m)).collect();
        let score = match (level.as_str(), grown) {
            ("package", false) => 80,
            ("package", true) => 60,
            (_, false) => 45,
            _ => 20,
        };
        let closer_text = closers
            .first()
            .map(|(a, b, _)| format!("New dependency {} → {} closes it", mod_label(a), mod_label(b)))
            .unwrap_or_default();
        let (tag, what) = if !grown {
            let body = if members.len() <= 4 { format!("{} now depend on each other", labels.join(" ⇄ ")) } else { format!("{} {}s now form a dependency cycle: {}", members.len(), if level == "package" { "module" } else { "file" }, names(&labels, 5)) };
            ("NEW CYCLE", if closer_text.is_empty() { format!("{body}.") } else { format!("{body}. {closer_text}.") })
        } else {
            let nl: Vec<String> = newly.iter().map(|m| label(m)).collect();
            let body = format!("{} joined an existing cycle of {} {}s", names(&nl, 3), members.len(), if level == "package" { "module" } else { "file" });
            ("CYCLE GROWS", if closer_text.is_empty() { format!("{body}.") } else { format!("{body}. {closer_text}.") })
        };
        let why = if !grown {
            "Why it matters: the parts now depend on each other: a change in either ripples both ways, and neither can be built, tested or reused alone.".to_string()
        } else {
            format!("Why it matters: everything in a cycle changes together; {} now ripples both ways with {} other {}s.", names(&newly.iter().map(|m| label(m)).collect::<Vec<_>>(), 2), members.len() - newly.len(), if level == "package" { "module" } else { "file" })
        };
        let mut witnesses: Vec<Value> = closers.iter().map(|(a, b, w)| json!({ "text": format!("new dependency {a}/ -> {b}/"), "steps": w })).collect();
        witnesses.push(json!({ "text": format!("{} members: {}", members.len(), members.join(", ")), "steps": [] }));
        let mods: BTreeSet<String> = if level == "package" { members.iter().cloned().collect() } else { members.iter().map(|m| package_of(m)).collect() };
        items.push(Item {
            score,
            cat: "cycle",
            tag: tag.to_string(),
            chain: Vec::new(),
            what,
            why,
            loc: closers.first().and_then(|(_, _, w)| w.get(0).map(s)),
            modules: mods,
            files: if level == "file" { members.iter().cloned().collect() } else { BTreeSet::new() },
            entities: BTreeSet::new(),
            witnesses,
            merged: 1 + closers.len(),
            edges: closers.iter().map(|(a, b, _)| (a.clone(), b.clone(), "new")).collect(),
            anchor: String::new(),
        });
    }

    // -- new dependencies between existing modules ---------------------------------
    for f in &new_deps {
        let d = &f["data"];
        let (a, b) = (s(&d["from"]), s(&d["to"]));
        if folded.contains(&(a.clone(), b.clone())) {
            continue;
        }
        let (la, lb) = (mod_label(&a), mod_label(&b));
        let w: Vec<String> = arr(&d["witness"]).iter().map(s).collect();
        let eg = if w.len() == 2 { format!(" (e.g. {} → {})", w[0], w[1]) } else { String::new() };
        let class = dep_class.get(&(a.clone(), b.clone())).map(String::as_str).unwrap_or("");
        let (score, tag, what, why) = match class {
            "closes-cycle" => (80, "NEW CYCLE", format!("{la} now depends on {lb}, which already depended on {la}{eg}."), "Why it matters: the two now depend on each other: a change in either ripples both ways.".to_string()),
            "inverts-layers" => (
                70,
                "LAYER BREAK",
                format!("Lower-level {la} now depends on higher-level {lb}{eg}."),
                format!("Why it matters: {la} never reached {lb} before and sits below it; code reaching up the layers makes cycles likely and lets changes in {lb} break {la}."),
            ),
            "couples-independent" => (
                60,
                "NEW COUPLING",
                format!("{la} now depends on {lb}; neither reached the other before{eg}."),
                format!("Why it matters: two independent parts are now coupled: changes to {lb} can break {la}, and {la} can no longer be used without {lb}."),
            ),
            "couples-distant" => (
                55,
                "NEW COUPLING",
                format!("{la} now depends on {lb} directly; before, {la} reached {lb} only through {} other modules{eg}.", dep_hops.get(&(a.clone(), b.clone())).copied().unwrap_or(0).saturating_sub(1)),
                format!("Why it matters: a new shortcut across the module structure: changes to {lb} now reach {la} in one step; check this direction fits the intended layering."),
            ),
            "inside-cycle" => (
                30,
                "NEW DEPENDENCY",
                format!("{la} → {lb}, inside a dependency cycle that already contained both{eg}."),
                "Why it matters: little new; one more link in an existing cycle.".to_string(),
            ),
            "already-indirect" => (
                30,
                "NEW DEPENDENCY",
                format!("{la} → {lb} (fits existing layers: {la} already reached {lb} indirectly){eg}."),
                "Why it matters: little; a shortcut along an existing dependency direction.".to_string(),
            ),
            _ => (
                50,
                "NEW DEPENDENCY",
                format!("{la} now depends on {lb}{eg}."),
                format!("Why it matters: changes to {lb} can now affect {la}; check this direction fits the intended layering."),
            ),
        };
        items.push(Item {
            score,
            cat: "dependency",
            tag: tag.to_string(),
            chain: vec![la.clone(), lb.clone()],
            what,
            why,
            loc: w.first().cloned(),
            modules: [a.clone(), b.clone()].into_iter().collect(),
            files: w.iter().cloned().collect(),
            entities: BTreeSet::new(),
            witnesses: vec![json!({ "text": format!("{a}/ -> {b}/ ({})", if class.is_empty() { "unclassified" } else { class }), "steps": w })],
            merged: 1,
            edges: vec![(a, b, "new")],
            anchor: String::new(),
        });
    }

    // -- contracts, imports, laws, schemas ---------------------------------------------
    for f in &fs {
        let d = &f["data"];
        match f["kind"].as_str().unwrap_or("") {
            "signature-change" => {
                let file = s(&d["file"]);
                let ent = s(&d["entity"]);
                let label = ent_label(&file, &ent);
                let shape = arr(&f["details"]).iter().map(s).find_map(|x| x.strip_prefix("parameter change: ").map(str::to_string)).unwrap_or_else(|| "breaking".into());
                let callers = arr(&d["callers"]);
                let stale: Vec<&Value> = callers.iter().filter(|c| c["touchedByChange"] != true).collect();
                let stale_old = arr(&d["staleCallersOfOldName"]);
                let prod: Vec<&Value> = stale.iter().copied().filter(|c| c["test"] != true && !is_test_file(&s(&c["file"]))).collect();
                let not_callable = matches!(d["type"].as_str(), Some("variable" | "constant" | "const" | "var" | "static" | "field" | "property" | "type" | "type_alias" | "class" | "struct" | "interface" | "enum" | "trait" | "impl" | "test"));
                let breaking = shape == "breaking";
                let when = if compiled(&file) { "will fail to compile" } else { "will break at runtime" };
                let eg = |v: &[&Value]| v.first().map(|c| format!(" (e.g. {}:{} `{}`)", s(&c["file"]), c["line"], s(&c["entity"]))).unwrap_or_default();
                let (score, tag, what, why) = if not_callable {
                    (10, "DECLARATION CHANGED", format!("Declaration of `{ent}` ({label}) changed."), "Why it matters: little; its header is not a parameter list, so callers are not checked.".to_string())
                } else if breaking && (!prod.is_empty() || !stale_old.is_empty()) {
                    let k = prod.len() + stale_old.len();
                    (
                        90,
                        "BREAKS CALLERS",
                        format!("`{ent}` ({label}) changed its parameters; {} this change did not touch still use{} the old form{}.", plural(k, "caller", "callers"), if k == 1 { "s" } else { "" }, eg(&prod)),
                        format!("Why it matters: those callers {when}."),
                    )
                } else if breaking && !stale.is_empty() {
                    (60, "BREAKS TESTS", format!("`{ent}` ({label}) changed its parameters; {} not touched by this change still use the old form{}.", plural(stale.len(), "test caller", "test callers"), eg(&stale)), format!("Why it matters: those tests {when}."))
                } else if breaking {
                    (
                        if callers.is_empty() { 20 } else { 40 },
                        "CONTRACT CHANGED",
                        match callers.len() {
                            0 => format!("`{ent}` ({label}) changed its parameters; nothing in this repository calls it."),
                            1 => format!("`{ent}` ({label}) changed its parameters; its one static caller was updated in this change."),
                            n => format!("`{ent}` ({label}) changed its parameters; all {n} static callers were updated in this change."),
                        },
                        "Why it matters: callers outside this repository (if it is a public API) would break.".to_string(),
                    )
                } else {
                    (10, "SIGNATURE TWEAK", format!("`{ent}` ({label}) signature changed ({shape})."), "Why it matters: little; existing calls still fit.".to_string())
                };
                let mut witnesses = vec![json!({ "text": format!("before: {}", s(&d["before"])), "steps": [] }), json!({ "text": format!("after:  {}", s(&d["after"])), "steps": [] })];
                for c in stale.iter().chain(stale_old.iter().collect::<Vec<_>>().iter()).take(20) {
                    witnesses.push(json!({ "text": format!("caller not modified by this change: {}:{} `{}`", s(&c["file"]), c["line"], s(&c["entity"])), "steps": [] }));
                }
                let mut files: BTreeSet<String> = [file.clone()].into_iter().collect();
                files.extend(stale.iter().map(|c| s(&c["file"])));
                items.push(Item {
                    score,
                    cat: if score >= 60 { "contract" } else if score >= LISTED { "contract-ok" } else { "minor-contract" },
                    tag: tag.to_string(),
                    chain: Vec::new(),
                    what,
                    why,
                    loc: Some(format!("{}:{}", file, d["line"])),
                    modules: files.iter().map(|x| package_of(x)).collect(),
                    files,
                    entities: [format!("{file}::{ent}")].into_iter().collect(),
                    witnesses,
                    merged: 1,
                    edges: Vec::new(),
                    anchor: String::new(),
                });
            }
            "broken-import" => {
                let file = s(&d["file"]);
                let test = is_test_file(&file);
                items.push(Item {
                    score: if test { 40 } else { 100 },
                    cat: "import",
                    tag: "WILL NOT LOAD".into(),
                    chain: Vec::new(),
                    what: format!("{} imports `{}`, which does not exist.", file, s(&d["specifier"])),
                    why: format!("Why it matters: {} as soon as anything imports it.", if compiled(&file) { "the build fails" } else { "the module fails to load" }),
                    loc: Some(file.clone()),
                    modules: [package_of(&file)].into_iter().collect(),
                    files: [file.clone()].into_iter().collect(),
                    entities: BTreeSet::new(),
                    witnesses: vec![json!({ "text": s(&f["title"]), "steps": [] })],
                    merged: 1,
                    edges: Vec::new(),
                    anchor: String::new(),
                });
            }
            "law-broken" => {
                items.push(Item {
                    score: 95,
                    cat: "law",
                    tag: "RULE BROKEN".into(),
                    what: format!("{}.", s(&f["title"])),
                    why: "Why it matters: a declared architecture rule (from .sem/promises or --laws) is now violated.".into(),
                    witnesses: arr(&f["details"]).iter().map(|x| json!({ "text": s(x), "steps": [] })).collect(),
                    merged: 1,
                    ..Default::default()
                });
            }
            "service-boundary" => {
                let file = s(&d["file"]);
                items.push(Item {
                    score: 30,
                    cat: "schema",
                    tag: "API SCHEMA".into(),
                    what: format!("Service schema {file} was added or changed."),
                    why: "Why it matters: clients built against the old schema may break; sem does not link schemas to their handlers, so check consumers by hand.".into(),
                    loc: Some(file.clone()),
                    modules: [package_of(&file)].into_iter().collect(),
                    files: [file].into_iter().collect(),
                    merged: 1,
                    ..Default::default()
                });
            }
            "new-package" => {
                let p = s(&d["package"]);
                let uses: Vec<String> = arr(&d["dependsOn"]).iter().map(|x| mod_label(&s(x))).collect();
                let users: Vec<String> = arr(&d["dependedOnBy"]).iter().map(|x| mod_label(&s(x))).collect();
                items.push(Item {
                    score: 30,
                    cat: "module",
                    tag: "NEW MODULE".into(),
                    what: format!(
                        "New module {}: uses {}; used by {}.",
                        mod_label(&p),
                        if uses.is_empty() { "nothing in the repo".to_string() } else { names(&uses, 4) },
                        if users.is_empty() { "nothing yet".to_string() } else { names(&users, 4) }
                    ),
                    why: "Why it matters: a new building block; check it sits in the right layer.".into(),
                    loc: Some(format!("{p}/")),
                    modules: [p.clone()].into_iter().collect(),
                    merged: 1,
                    witnesses: vec![json!({ "text": s(&f["title"]), "steps": [] })],
                    edges: arr(&d["dependsOn"]).iter().map(|x| (p.clone(), s(x), "new")).chain(arr(&d["dependedOnBy"]).iter().map(|x| (s(x), p.clone(), "new"))).collect(),
                    ..Default::default()
                });
            }
            "propagation-cost" if f["severity"] == "medium" => {
                items.push(Item {
                    score: 30,
                    cat: "coupling",
                    tag: "MORE COUPLED".into(),
                    what: format!("{}.", capitalize(&s(&f["title"]))),
                    why: "Why it matters: more of the code base can now be affected by a change in one file.".into(),
                    merged: 1,
                    ..Default::default()
                });
            }
            _ => {}
        }
    }

    // -- side effects, by module and effect -------------------------------------------
    let mut eff: BTreeMap<(String, String), (bool, Vec<String>, BTreeSet<String>, Vec<Value>)> = BTreeMap::new();
    for f in &fs {
        let d = &f["data"];
        let (added, new_ent): (Vec<String>, bool) = match f["kind"].as_str() {
            Some("side-effect-change") => (arr(&d["added"]).iter().map(s).collect(), false),
            Some("new-entity-effects") => (arr(&d["writes"]).iter().map(s).collect(), true),
            _ => continue,
        };
        let key = s(&d["entity"]);
        let (file, name) = entity_of_key(&key);
        if flow_sink_ents.iter().any(|k| k.rsplit_once("::").is_some_and(|(f, n)| f == file && last_seg(n) == last_seg(&name))) {
            continue;
        }
        for e in added.into_iter().filter(|e| EFFECTS.contains(&e.as_str())) {
            let pkg = package_of(&file);
            let g = eff.entry((pkg, e)).or_insert((true, Vec::new(), BTreeSet::new(), Vec::new()));
            g.0 &= new_ent;
            if !g.1.contains(&name) {
                g.1.push(name.clone());
            }
            g.2.insert(file.clone());
            g.3.push(json!({ "text": s(&f["title"]), "steps": [] }));
        }
    }
    for ((pkg, e), (all_new, ents, files, witnesses)) in eff {
        let handler = files.iter().any(|f| is_handler(f));
        let score = if handler { 55 } else if all_new { 25 } else { 30 };
        let who = if ents.len() == 1 { format!("{}.{}", mod_label(&pkg), ents[0]) } else { format!("{} functions in {} ({})", ents.len(), mod_label(&pkg), names(&ents, 3)) };
        let verb = effect_phrase(&e);
        let what = if ents.len() == 1 {
            format!("{}{who} {}.", if all_new { "New " } else { "" }, verb.replacen("open", "opens", 1).replacen("write", "writes", 1).replacen("run", "runs", 1).replacen("query", "queries", 1).replacen("make", "makes", 1).replacen("render", "renders", 1))
        } else {
            format!("{who} now {verb}.")
        };
        let why = if handler { format!("Why it matters: {}", handler_why(&e)) } else { format!("Why it matters: a new side effect in {}; check it belongs in this layer.", mod_label(&pkg)) };
        let n = witnesses.len();
        items.push(Item {
            score,
            cat: "effect",
            tag: "NEW EFFECT".into(),
            what,
            why,
            loc: files.iter().next().cloned(),
            modules: [pkg.clone()].into_iter().collect(),
            entities: files.iter().flat_map(|f| ents.iter().map(move |x| format!("{f}::{x}"))).collect(),
            files,
            witnesses,
            merged: n,
            ..Default::default()
        });
    }

    // -- complexity -------------------------------------------------------------------
    for f in fs.iter().filter(|f| f["kind"] == "complexity") {
        let d = &f["data"];
        let (file, name) = (s(&d["file"]), s(&d["name"]));
        let label = ent_label(&file, &name);
        let new = d["new"] == true;
        let (bc, hc) = (d["cyclomatic"][0].as_i64(), d["cyclomatic"][1].as_i64().unwrap_or(0));
        let (bg, hg) = (d["cognitive"][0].as_i64(), d["cognitive"][1].as_i64().unwrap_or(0));
        let (dc, dg) = (d["delta"][0].as_i64().unwrap_or(0), d["delta"][1].as_i64().unwrap_or(0));
        let score = if new {
            if hc >= 15 || hg >= 25 { 42 } else { 15 }
        } else if dc >= 10 || dg >= 20 {
            50
        } else if dc >= 5 || dg >= 10 {
            32
        } else {
            5
        };
        let what = if new {
            format!("New {label}: cyclomatic {hc}, cognitive {hg}.")
        } else {
            format!("{label} cyclomatic {} → {hc}, cognitive {} → {hg}.", bc.unwrap_or(0), bg.unwrap_or(0))
        };
        items.push(Item {
            score,
            cat: "complexity",
            tag: if new { "NEW COMPLEX CODE".into() } else { "MORE COMPLEX".into() },
            what,
            why: "Why it matters: more branches mean more cases to test and review; bugs hide in the paths no test takes.".into(),
            loc: Some(format!("{}:{}", file, d["line"])),
            modules: [package_of(&file)].into_iter().collect(),
            files: [file.clone()].into_iter().collect(),
            entities: [format!("{file}::{name}")].into_iter().collect(),
            merged: 1,
            ..Default::default()
        });
    }

    // -- rank, cap, count the rest ------------------------------------------------------
    items.sort_by(|a, b| b.score.cmp(&a.score).then(a.cat.cmp(b.cat)).then(a.what.cmp(&b.what)));
    let caps: HashMap<&str, usize> = [("dependency", 4), ("complexity", 3), ("effect", 3), ("module", 2), ("schema", 2), ("coupling", 1), ("cycle", 3), ("contract", 5), ("contract-ok", 2)].into_iter().collect();
    let mut shown: Vec<Item> = Vec::new();
    let mut rest: Vec<Item> = Vec::new();
    let mut per: HashMap<&str, usize> = HashMap::new();
    for it in items {
        let n = per.get(it.cat).copied().unwrap_or(0);
        if it.score >= LISTED && shown.len() < VIEW_ITEMS && caps.get(it.cat).is_none_or(|&c| n < c) {
            *per.entry(it.cat).or_default() += 1;
            shown.push(it);
        } else {
            rest.push(it);
        }
    }

    let also = also_line(&rest, &fs, dropped_same_call);
    let unchanged = unchanged_line(r, &fs, &ch, &shown);
    let uncertainty = uncertainty_line(r, &fs, &ch);
    let graph = view_graph(r, &shown);
    let decisions = shown.iter().filter(|i| i.score >= DECISION).count();
    let sm = &r["summary"];
    let touched_mods = arr(&mg["touched"]).len();
    let raw = fs.iter().filter(|f| f["severity"] != "info").count();
    json!({
        "base": r["base"], "head": r["head"],
        "summary": { "files": sm["filesChanged"], "entities": sm["entitiesChanged"], "modulesTouched": if mg.is_object() { json!(touched_mods) } else { Value::Null },
            "findings": raw, "facts": shown.len() + rest.len(), "decisions": decisions },
        "items": shown.iter().enumerate().map(|(i, it)| it.to_json(i + 1)).collect::<Vec<_>>(),
        "also": also,
        "unchanged": unchanged,
        "uncertainty": uncertainty,
        "graph": graph,
        "precision": r["precision"],
    })
}

fn also_line(rest: &[Item], fs: &[Value], dropped: usize) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let flows: Vec<&Item> = rest.iter().filter(|i| i.cat == "flow").collect();
    if !flows.is_empty() {
        let paths: usize = flows.iter().map(|i| i.merged).sum();
        let mut pairs: BTreeMap<String, usize> = BTreeMap::new();
        for i in &flows {
            *pairs.entry(format!("{} → {}", i.chain.first().cloned().unwrap_or_default(), i.chain.last().cloned().unwrap_or_default())).or_default() += i.merged;
        }
        let top = pairs.into_iter().max_by_key(|x| x.1).map(|x| x.0).unwrap_or_default();
        parts.push(format!("{} ({}; mostly {top})", plural(flows.len(), "minor data-flow group", "minor data-flow groups"), plural(paths, "path", "paths")));
    }
    let count = |cat: &str| rest.iter().filter(|i| i.cat == cat).count();
    for (cat, one, many) in [
        ("contract", "more broken contract", "more broken contracts"),
        ("contract-ok", "more contract change with callers updated", "more contract changes with callers updated"),
        ("minor-contract", "compatible signature change", "compatible signature changes"),
        ("dependency", "more new dependency", "more new dependencies"),
        ("cycle", "more cycle change", "more cycle changes"),
        ("effect", "side-effect change", "side-effect changes"),
        ("complexity", "smaller complexity change", "smaller complexity changes"),
        ("module", "more new module", "more new modules"),
        ("schema", "more schema change", "more schema changes"),
    ] {
        let n = count(cat);
        if n > 0 {
            parts.push(plural(n, one, many));
        }
    }
    let removed = |k: &str| fs.iter().filter(|f| f["kind"] == k).count();
    let (rp, rd, rc) = (removed("removed-data-path"), removed("removed-package-dependency"), removed("removed-cycle"));
    let mut gone = Vec::new();
    if rp > 0 {
        gone.push(plural(rp, "data path", "data paths"));
    }
    if rd > 0 {
        gone.push(plural(rd, "dependency", "dependencies"));
    }
    if rc > 0 {
        gone.push(plural(rc, "cycle", "cycles"));
    }
    if !gone.is_empty() {
        parts.push(format!("removed: {}", gone.join(", ")));
    }
    if dropped > 0 {
        parts.push(format!("{} dropped (a read and a path on one call)", plural(dropped, "same-call path", "same-call paths")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("Also: {}.", parts.join("; ")))
    }
}

fn classes_touched(fs: &[Value]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in fs {
        let d = &f["data"];
        match f["kind"].as_str().unwrap_or("") {
            "new-data-path" | "removed-data-path" | "new-possible-path" => {
                out.insert(s(&d["source"]["class"]));
                out.insert(s(&d["sink"]["class"]));
            }
            "side-effect-change" => {
                out.extend(arr(&d["added"]).iter().map(s));
                out.extend(arr(&d["removed"]).iter().map(s));
            }
            "new-entity-effects" => out.extend(arr(&d["writes"]).iter().map(s)),
            _ => {}
        }
    }
    out
}

fn unchanged_line(r: &Value, fs: &[Value], ch: &Changed, shown: &[Item]) -> Option<String> {
    let cls = classes_touched(fs);
    let has = |k: &str| fs.iter().any(|f| f["kind"] == k);
    let mut parts: Vec<String> = Vec::new();
    // auth: provable only from paths: no changed file and no listed item in an auth-named module
    if !ch.files.is_empty() && !ch.files.iter().any(|f| is_auth(f)) && !shown.iter().flat_map(|i| i.modules.iter()).any(|m| is_auth(m)) {
        parts.push("auth".into());
    }
    for (name, keys) in [
        ("network calls", &["net-send", "net-input", "http-input"][..]),
        ("subprocesses", &["exec"][..]),
        ("database access", &["db", "db-read"][..]),
        ("file writes", &["file-write", "file-path"][..]),
        ("environment reads", &["env"][..]),
        ("HTML/HTTP output", &["template", "http-response"][..]),
    ] {
        if !keys.iter().any(|k| cls.contains(*k)) {
            parts.push(name.into());
        }
    }
    let breaking_sig = fs.iter().any(|f| f["kind"] == "signature-change" && arr(&f["details"]).iter().any(|d| s(d) == "parameter change: breaking"));
    if !breaking_sig {
        parts.push("public APIs".into());
    }
    if !has("new-package-dependency") && !has("removed-package-dependency") && !has("new-package") {
        parts.push("module dependencies".into());
    }
    if !has("new-cycle") && !has("removed-cycle") {
        parts.push("cycles".into());
    }
    let mg = &r["moduleGraph"];
    if let Some(total) = mg["packages"].as_u64() {
        let mut involved: BTreeSet<String> = arr(&mg["touched"]).iter().map(s).collect();
        for i in shown {
            involved.extend(i.modules.iter().cloned());
        }
        let other = (total as usize).saturating_sub(involved.len());
        if other > 0 {
            parts.push(plural(other, "other module", "other modules"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("Unchanged (as far as sem can see): {}.", parts.join(", ")))
    }
}

fn uncertainty_line(r: &Value, fs: &[Value], ch: &Changed) -> Option<String> {
    let mut sites: BTreeSet<(String, String)> = BTreeSet::new();
    let mut why: BTreeMap<String, usize> = BTreeMap::new();
    for f in fs.iter().filter(|f| f["kind"] == "new-unknown-path") {
        let u = &f["data"]["unknown"];
        let file = s(&u["file"]);
        if ch.files.is_empty() || ch.files.contains(&file) {
            if sites.insert((file, s(&u["line"]))) {
                *why.entry(s(&u["why"])).or_default() += 1;
            }
        }
    }
    let mut out = Vec::new();
    if !sites.is_empty() {
        let top = why.into_iter().filter(|(w, _)| !w.is_empty()).max_by_key(|x| x.1).map(|(w, _)| format!(", mostly {w}")).unwrap_or_default();
        out.push(format!(
            "{} {} couldn't be resolved{top}; data reaching {} may hide more paths.",
            plural(sites.len(), "call", "calls"),
            if ch.exact { "in the changed code" } else { "on new data paths" },
            if sites.len() == 1 { "it" } else { "them" }
        ));
    }
    for f in fs.iter().filter(|f| f["kind"] == "unknown-coverage") {
        out.push(format!("The share of unresolved calls rose ({}).", s(&f["title"]).trim_start_matches("unresolved call rate ")));
    }
    if r["incomplete"] == true {
        out.push("The analysis hit a size or time limit: findings may be missing.".into());
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
}

/// The graph drawn in the HTML view: the report's module graph when present,
/// plus the modules and edges the listed items name.
fn view_graph(r: &Value, shown: &[Item]) -> Value {
    let mg = &r["moduleGraph"];
    let mut nodes: BTreeMap<String, Value> = BTreeMap::new();
    for n in arr(&mg["nodes"]) {
        nodes.insert(s(&n["id"]), n.clone());
    }
    let mut edges: BTreeMap<(String, String, String), Vec<usize>> = BTreeMap::new();
    for e in arr(&mg["edges"]) {
        edges.entry((s(&e["from"]), s(&e["to"]), s(&e["status"]))).or_default();
    }
    for (i, it) in shown.iter().enumerate() {
        for m in &it.modules {
            nodes.entry(m.clone()).or_insert_with(|| json!({ "id": m, "depth": null, "touched": false, "added": false, "removed": false }));
        }
        for (a, b, k) in &it.edges {
            for m in [a, b] {
                nodes.entry(m.clone()).or_insert_with(|| json!({ "id": m, "depth": null, "touched": false, "added": false, "removed": false }));
            }
            let kind = if *k == "new" && edges.contains_key(&(a.clone(), b.clone(), "new".into())) { "new".to_string() } else { k.to_string() };
            edges.entry((a.clone(), b.clone(), kind)).or_default().push(i + 1);
        }
    }
    let mut item_of: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, it) in shown.iter().enumerate() {
        for m in &it.modules {
            item_of.entry(m.clone()).or_default().push(i + 1);
        }
    }
    let nodes: Vec<Value> = nodes
        .into_iter()
        .map(|(id, mut n)| {
            n["label"] = json!(mod_label(&id));
            n["role"] = json!(path_role(&id));
            n["items"] = json!(item_of.get(&id).cloned().unwrap_or_default());
            n
        })
        .collect();
    json!({
        "nodes": nodes,
        "edges": edges.into_iter().map(|((a, b, k), items)| json!({ "from": a, "to": b, "status": k, "items": items })).collect::<Vec<_>>(),
        "hiddenNeighbors": mg["hiddenNeighbors"],
        "totalModules": mg["packages"],
    })
}

// -- rendering ----------------------------------------------------------------------

fn short(sha: &Value) -> String {
    let x = s(sha);
    x[..12.min(x.len())].to_string()
}

/// Plain-text view: one screen.
pub fn render_view_text(v: &Value) -> String {
    let mut o = String::new();
    let sm = &v["summary"];
    let mods = sm["modulesTouched"].as_u64().map(|m| format!(", {} touched", plural(m as usize, "module", "modules"))).unwrap_or_default();
    o += &format!("Architecture view {}..{}: {} files, {} entities changed{mods}\n", short(&v["base"]), short(&v["head"]), sm["files"], sm["entities"]);
    let items = arr(&v["items"]);
    let n = sm["decisions"].as_u64().unwrap_or(0);
    o += &format!(
        "{} findings collapsed to {} facts; {} listed, {} {} a human decision.\n\n",
        sm["findings"],
        sm["facts"],
        items.len(),
        n,
        if n == 1 { "needs" } else { "need" }
    );
    if items.is_empty() {
        o += "No architectural change that needs a decision.\n";
    }
    for it in &items {
        let chain = arr(&it["chain"]).iter().map(s).collect::<Vec<_>>();
        let head = if chain.len() >= 2 { chain.join(" ─▶ ") } else { s(&it["what"]) };
        o += &format!("{:>2}. {:<18} {}\n", it["rank"], s(&it["tag"]), head);
        let pad = " ".repeat(23);
        if chain.len() >= 2 {
            o += &format!("{pad}{}\n", s(&it["what"]));
        }
        o += &format!("{pad}{}{}\n", s(&it["why"]), if it["decision"] == true { "  → needs a human decision" } else { "" });
        if let Some(l) = it["location"].as_str() {
            o += &format!("{pad}at {l}\n");
        }
    }
    o += "\n";
    for k in ["also", "unchanged", "uncertainty"] {
        if let Some(x) = v[k].as_str() {
            o += &format!("{x}\n");
        }
    }
    o
}

const HTML_TEMPLATE: &str = include_str!("arch_view.html");

/// Self-contained HTML: the ranked list beside the module graph, with the
/// view embedded as JSON. No network assets.
pub fn render_view_html(v: &Value) -> String {
    let data = serde_json::to_string(v).unwrap_or_else(|_| "{}".into()).replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026");
    let title = format!("Architecture view {}..{}", short(&v["base"]), short(&v["head"]));
    HTML_TEMPLATE.replace("{{TITLE}}", &title).replace("{{DATA}}", &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(src_file: &str, src_ent: &str, src_class: &str, sink_file: &str, sink_ent: &str, sink_class: &str, path: &[&str]) -> Value {
        json!({ "kind": "new-data-path", "severity": "medium",
            "title": format!("new data path: {src_class} {src_file}:1 `{src_ent}` -> {sink_class} {sink_file}:9 `{sink_ent}` (via os.Open)"),
            "details": path,
            "data": { "source": { "class": src_class, "file": src_file, "line": 1, "entity": src_ent, "via": "read" },
                "sink": { "class": sink_class, "file": sink_file, "line": 9, "entity": sink_ent, "via": "os.Open" },
                "path": path } })
    }

    fn report(findings: Vec<Value>) -> Value {
        json!({ "base": "aaaaaaaaaaaaaaaa", "head": "bbbbbbbbbbbbbbbb", "summary": { "filesChanged": 3, "entitiesChanged": 5 },
            "findings": findings, "complexity": [],
            "changed": [ { "file": "pkg/plugins/manager.go", "name": "Manager.Install", "change": "modified" },
                         { "file": "pkg/unpack/unpack.go", "name": "Extract", "change": "added" } ],
            "moduleGraph": { "packages": 40, "touched": ["pkg/plugins", "pkg/unpack"], "nodes": [], "edges": [], "newEdges": [] } })
    }

    fn one_fact_fixture() -> Vec<Value> {
        let mut v = Vec::new();
        for src in ["pkg/settings/loader.go", "pkg/settings/cache.go", "pkg/plugins/source.go"] {
            for line in [9, 30] {
                let mut f = flow(
                    src,
                    "Load",
                    "file-read",
                    "pkg/unpack/unpack.go",
                    "Extract",
                    "file-path",
                    &[&format!("{src}:1 Load: source file-read via os.ReadFile"), "pkg/plugins/manager.go:40 Manager.Install: returned from Load", "pkg/plugins/manager.go:50 Manager.Install: calls Extract", &format!("pkg/unpack/unpack.go:{line} Extract: sink file-path via os.Open")],
                );
                f["data"]["sink"]["line"] = json!(line);
                v.push(f);
            }
        }
        // same-call artifact: open(path) is a read and a path sink at once
        let mut same = flow("pkg/tool/helpers.go", "Pack", "file-read", "pkg/tool/helpers.go", "Pack", "file-path", &[]);
        same["data"]["source"]["line"] = json!(9);
        same["data"]["source"]["via"] = json!("os.Open");
        v.push(same);
        v
    }

    #[test]
    fn one_fact_many_paths_is_one_item() {
        let v = build_view(&report(one_fact_fixture()));
        let items = arr(&v["items"]);
        assert_eq!(items.len(), 1, "{v:#}");
        let it = &items[0];
        assert_eq!(it["merged"], 6);
        assert_eq!(it["decision"], true);
        let chain: Vec<String> = arr(&it["chain"]).iter().map(s).collect();
        assert_eq!(chain, ["config + file contents", "plugins.Manager.Install", "unpack.Extract", "opens a file path"]);
        assert!(s(&it["why"]).contains("path traversal"), "{it:#}");
        assert!(s(&v["also"]).contains("1 same-call path dropped"), "{v:#}");
    }

    #[test]
    fn every_listed_item_says_what_and_why() {
        let mut fs = one_fact_fixture();
        fs.push(json!({ "kind": "broken-import", "severity": "high", "title": "x", "data": { "file": "web/page.tsx", "specifier": "./consts" } }));
        fs.push(json!({ "kind": "complexity", "severity": "medium", "title": "c", "data": { "name": "Manager.Install", "file": "pkg/plugins/manager.go", "line": 30, "cyclomatic": [9, 17], "cognitive": [12, 30], "delta": [8, 18] } }));
        fs.push(json!({ "kind": "new-package-dependency", "severity": "medium", "title": "d", "data": { "from": "pkg/plugins", "to": "pkg/unpack", "witness": ["pkg/plugins/manager.go", "pkg/unpack/unpack.go"] } }));
        let v = build_view(&report(fs));
        let items = arr(&v["items"]);
        assert!(items.len() >= 4 && items.len() <= VIEW_ITEMS);
        for it in &items {
            assert!(s(&it["what"]).len() > 10, "{it:#}");
            assert!(s(&it["why"]).starts_with("Why it matters: ") && s(&it["why"]).len() > 25, "{it:#}");
        }
        // a missing module outranks a data path, which outranks complexity
        assert_eq!(items[0]["tag"], "WILL NOT LOAD");
        let pos = |tag: &str| items.iter().position(|i| i["tag"] == tag).unwrap();
        assert!(pos("NEW DATA FLOW") < pos("MORE COMPLEX"));
        assert!(s(&v["unchanged"]).contains("auth") && s(&v["unchanged"]).contains("network calls") && s(&v["unchanged"]).contains("public APIs"), "{v:#}");
        assert!(s(&v["unchanged"]).contains("other modules"), "{v:#}");
        let text = render_view_text(&v);
        assert!(text.contains("needs a human decision") && text.contains("Unchanged"), "{text}");
    }

    #[test]
    fn the_list_is_capped_and_the_rest_counted() {
        let mut fs = Vec::new();
        for i in 0..30 {
            fs.push(json!({ "kind": "complexity", "severity": "medium", "title": "c", "data": { "name": format!("f{i}"), "file": "a/b.py", "line": i, "cyclomatic": [1, 20], "cognitive": [1, 30], "delta": [19, 29] } }));
        }
        for i in 0..20 {
            fs.push(json!({ "kind": "new-package-dependency", "severity": "medium", "title": "d", "data": { "from": format!("m{i}"), "to": "core", "witness": [] } }));
        }
        let v = build_view(&report(fs));
        let items = arr(&v["items"]);
        assert!(items.len() <= VIEW_ITEMS);
        assert_eq!(items.iter().filter(|i| i["category"] == "complexity").count(), 3);
        assert!(s(&v["also"]).contains("27 smaller complexity changes"), "{v:#}");
    }

    #[test]
    fn dependencies_are_classified_against_the_base_layers() {
        let dep = |a: &str, b: &str| json!({ "kind": "new-package-dependency", "severity": "medium", "title": "d", "data": { "from": a, "to": b, "witness": [format!("{a}/x.rs"), format!("{b}/y.rs")] } });
        let mut r = report(vec![dep("net/codec", "ui/panels"), dep("app/cli", "net/codec"), dep("fs/cache", "net/wire"), dep("ui/forms", "db/rows")]);
        r["moduleGraph"]["newEdges"] = json!([
            { "from": "net/codec", "to": "ui/panels", "class": "inverts-layers" },
            { "from": "app/cli", "to": "net/codec", "class": "already-indirect", "hops": 2 },
            { "from": "fs/cache", "to": "net/wire", "class": "couples-independent" },
            { "from": "ui/forms", "to": "db/rows", "class": "couples-distant", "hops": 5 } ]);
        let v = build_view(&r);
        let tags: Vec<String> = arr(&v["items"]).iter().map(|i| s(&i["tag"])).collect();
        assert_eq!(tags, ["LAYER BREAK", "NEW COUPLING", "NEW COUPLING", "NEW DEPENDENCY"], "{v:#}");
        assert!(s(&v["items"][2]["what"]).contains("only through 4 other modules"), "{v:#}");
        assert!(s(&v["items"][3]["what"]).contains("fits existing layers"));
    }

    #[test]
    fn a_dependency_that_closes_a_cycle_is_part_of_the_cycle() {
        let r = report(vec![
            json!({ "kind": "new-package-dependency", "severity": "medium", "title": "d", "data": { "from": "src/low", "to": "src/high", "witness": ["src/low/a.rs", "src/high/b.rs"] } }),
            json!({ "kind": "new-cycle", "severity": "high", "title": "new package cycle of 2", "data": { "level": "package", "members": ["src/high", "src/low"], "newMembers": ["src/high", "src/low"] } }),
        ]);
        let v = build_view(&r);
        let items = arr(&v["items"]);
        assert_eq!(items.len(), 1, "{v:#}");
        assert_eq!(items[0]["tag"], "NEW CYCLE");
        assert!(s(&items[0]["what"]).contains("low → high closes it"), "{v:#}");
    }

    #[test]
    fn stale_callers_break_and_updated_ones_do_not() {
        let sig = |ent: &str, touched: bool| json!({ "kind": "signature-change", "severity": "high", "title": "s",
            "details": ["before: x", "after: y", "parameter change: breaking"],
            "data": { "entity": ent, "file": "core/api.py", "line": 3, "before": "def f(a)", "after": "def f(a, b)",
                "callers": [ { "entity": "use", "file": "cli/main.py", "line": 7, "touchedByChange": touched, "test": false } ], "staleCallersOfOldName": [] } });
        let v = build_view(&report(vec![sig("save", false), sig("load", true)]));
        let items = arr(&v["items"]);
        assert_eq!(items[0]["tag"], "BREAKS CALLERS");
        assert!(s(&items[0]["why"]).contains("will break at runtime"), "{v:#}");
        assert_eq!(items[1]["tag"], "CONTRACT CHANGED");
        assert!(!s(&v["unchanged"]).contains("public APIs"));
    }

    #[test]
    fn unresolved_calls_in_changed_code_are_stated_once() {
        let unk = |line: i64| json!({ "kind": "new-unknown-path", "severity": "low", "title": "u",
            "data": { "unknown": { "file": "pkg/plugins/manager.go", "line": line, "entity": "Manager.Install", "why": "unknown receiver type" }, "source": { "class": "env" } } });
        let v = build_view(&report(vec![unk(3), unk(3), unk(8)]));
        assert_eq!(s(&v["uncertainty"]), "2 calls in the changed code couldn't be resolved, mostly unknown receiver type; data reaching them may hide more paths.");
        let quiet = build_view(&report(vec![]));
        assert!(quiet["uncertainty"].is_null());
    }

    #[test]
    fn handler_effects_stand_in_for_unmodelled_request_input() {
        let r = report(vec![json!({ "kind": "new-entity-effects", "severity": "low", "title": "new `readSteps` performs file-path",
            "data": { "entity": "svc/mcp/handlers.rs::readSteps", "writes": ["file-path"] } })]);
        let v = build_view(&r);
        let it = &v["items"][0];
        assert_eq!(it["decision"], true, "{v:#}");
        assert!(s(&it["why"]).contains("path traversal"));
    }

    #[test]
    fn labels_and_roles() {
        assert_eq!(mod_label("svc/devices/bus/bus_client/src"), "bus_client");
        assert_eq!(mod_label("src/pages/inbox/utils"), "inbox/utils");
        assert_eq!(mod_label("."), "(root)");
        assert_eq!(path_role("pkg/config/loader.go"), Some("config"));
        assert_eq!(path_role("internal/handlers/billingHandlers.go"), Some("request handling"));
        assert_eq!(path_role("server/auth/session.ts"), Some("auth"));
        assert_eq!(path_role("pkg/unpack"), None);
    }

    #[test]
    fn html_is_self_contained() {
        let mut v = build_view(&report(one_fact_fixture()));
        v["items"][0]["what"] = json!("</script><script>alert(1)</script>");
        let h = render_view_html(&v);
        assert!(h.contains("<svg") || h.contains("createElementNS"));
        for bad in ["src=\"http", "href=\"http", "@import", "url(http", "url(//", "</script><script>alert"] {
            assert!(!h.contains(bad), "html contains {bad}");
        }
        assert_eq!(h.matches("<script").count(), 2, "one data block, one program");
    }
}
