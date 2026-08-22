//! Oracle + timing probe for red-green incremental resolution.
//!
//! Not part of the public API and not wired into any product code path.
//!
//! For a real checkout it: builds cold into a [`GraphSession`], mutates a chosen
//! set of files, rebuilds warm, then builds cold *again* from the mutated tree
//! and asserts the two agree bit-for-bit (entity set, edge set, sorted-edge
//! hash). Originals are restored before exit, including on panic.
//!
//! Usage:
//!   cargo run --release --example incr_probe -- <repo_root> <scenario> [label]
//!
//! Scenarios: `none`, `leaf`, `mixed50`, `hub`, `tests` (the duplicate-name
//! fixture directories), `all`.
//!
//! Prints tagged, greppable lines:
//!   COLD    — cold build wall time, entity/edge counts, edge hash
//!   WARM    — warm rebuild wall time and the red-green counters
//!   ORACLE  — `ok` when warm == cold at the mutated state, `MISMATCH` otherwise

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, RefType};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::parser::session::GraphSession;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

/// Appended to a mutated file. Two functions, one calling the other, so the
/// mutation adds an *edge* and not merely an entity — an append that only grows
/// the entity set would leave the edge set untouched and make the oracle a much
/// weaker check than it looks.
const PROBE_SUFFIX: &str = "\n// incr probe\nfunction __incrHelper() { return 1; }\nexport function __incrProbe() { return __incrHelper(); }\n";

fn make_registry(root: &Path) -> ParserRegistry {
    let mut registry = create_default_registry();
    registry.load_semrc(root);
    registry.load_gitattributes(root);
    registry
}

fn walk_files(root: &Path, registry: &ParserRegistry) -> Vec<String> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    for entry in builder.build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if is_default_excluded(&rel) || is_probably_binary_path(&rel) {
            continue;
        }
        if registry.get_explicit_plugin(&rel).is_none() {
            continue;
        }
        files.push(rel);
    }
    files.sort();
    files
}

/// Restores every file it touched, whatever happens — a probe that leaves a
/// developer's checkout mutated is worse than no probe.
struct Restore {
    root: PathBuf,
    originals: HashMap<String, String>,
}

impl Restore {
    /// Rename the first exported function in `rel`, breaking every importer of
    /// it. Unlike an append, this changes what *other* files resolve, which is
    /// what makes the blast-radius numbers meaningful.
    fn rename_export(&mut self, rel: &str) -> bool {
        let full = self.root.join(rel);
        let Ok(original) = std::fs::read_to_string(&full) else {
            return false;
        };
        let Some(at) = original.find("export function ") else {
            return false;
        };
        let name_start = at + "export function ".len();
        let name_end = name_start
            + original[name_start..]
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(0);
        if name_end == name_start {
            return false;
        }
        let mut mutated = String::with_capacity(original.len() + 16);
        mutated.push_str(&original[..name_end]);
        mutated.push_str("__incrRenamed");
        mutated.push_str(&original[name_end..]);
        if std::fs::write(&full, mutated).is_err() {
            return false;
        }
        self.originals.insert(rel.to_string(), original);
        true
    }

    fn mutate(&mut self, rel: &str) -> bool {
        let full = self.root.join(rel);
        let Ok(original) = std::fs::read_to_string(&full) else {
            return false;
        };
        let mutated = format!("{original}{PROBE_SUFFIX}");
        if std::fs::write(&full, mutated).is_err() {
            return false;
        }
        self.originals.insert(rel.to_string(), original);
        true
    }

    /// give `rel` a brand-new named export. Paired with
    /// `add_import_and_use` to build the "importchurn" scenario — unlike
    /// every other mutation here, this one actually touches the import
    /// table (the others only add entities/edges within a single file).
    fn add_export(&mut self, rel: &str, export_name: &str) -> bool {
        let full = self.root.join(rel);
        let Ok(original) = std::fs::read_to_string(&full) else {
            return false;
        };
        let mutated = format!(
            "{original}\n// import-churn probe\nexport function {export_name}() {{ return 7; }}\n"
        );
        if std::fs::write(&full, mutated).is_err() {
            return false;
        }
        self.originals.entry(rel.to_string()).or_insert(original);
        true
    }

    /// give `rel` a brand-new import of `export_name` from
    /// `import_path`, plus a function that calls it — a new
    /// `(rel, export_name) -> target` entry the import table did not have
    /// before this mutation.
    fn add_import_and_use(
        &mut self,
        rel: &str,
        import_path: &str,
        export_name: &str,
        local_fn_name: &str,
    ) -> bool {
        let full = self.root.join(rel);
        let Ok(original) = std::fs::read_to_string(&full) else {
            return false;
        };
        let mutated = format!(
            "{original}\n// import-churn probe\nimport {{ {export_name} }} from '{import_path}';\nexport function {local_fn_name}() {{ return {export_name}(); }}\n"
        );
        if std::fs::write(&full, mutated).is_err() {
            return false;
        }
        self.originals.entry(rel.to_string()).or_insert(original);
        true
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        for (rel, original) in &self.originals {
            let _ = std::fs::write(self.root.join(rel), original);
        }
    }
}

struct Fingerprint {
    entities: usize,
    edges: usize,
    edge_hash: u64,
    entity_hash: u64,
}

fn fingerprint(graph: &EntityGraph, entities: &[SemanticEntity]) -> Fingerprint {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut edges: Vec<String> = graph
        .edges
        .iter()
        .map(|e| {
            let kind = match e.ref_type {
                RefType::Calls => "calls",
                RefType::TypeRef => "typeref",
                RefType::Imports => "imports",
            };
            format!("{}\u{1f}{}\u{1f}{}", e.from_entity, e.to_entity, kind)
        })
        .collect();
    edges.sort();
    let mut h = DefaultHasher::new();
    for edge in &edges {
        edge.hash(&mut h);
    }
    let edge_hash = h.finish();

    let mut ids: Vec<&str> = entities.iter().map(|e| e.id.as_str()).collect();
    ids.sort_unstable();
    let mut h = DefaultHasher::new();
    for id in &ids {
        id.hash(&mut h);
    }
    Fingerprint {
        entities: entities.len(),
        edges: graph.edges.len(),
        edge_hash,
        entity_hash: h.finish(),
    }
}

/// The file whose entities the most *other* files depend on — the worst case for
/// blast radius, picked from the graph rather than guessed.
fn highest_fan_in(graph: &EntityGraph, files: &[String]) -> Option<String> {
    let mut dependents: HashMap<&str, std::collections::HashSet<&str>> = HashMap::new();
    for edge in &graph.edges {
        let Some(to_file) = edge.to_entity.split("::").next() else {
            continue;
        };
        let Some(from_file) = edge.from_entity.split("::").next() else {
            continue;
        };
        if to_file == from_file {
            continue;
        }
        dependents.entry(to_file).or_default().insert(from_file);
    }
    let known: std::collections::HashSet<&str> = files.iter().map(String::as_str).collect();
    dependents
        .into_iter()
        .filter(|(f, _)| known.contains(f))
        .max_by_key(|(_, deps)| deps.len())
        .map(|(f, deps)| {
            eprintln!(
                "  (highest fan-in: {f} with {} dependent files)",
                deps.len()
            );
            f.to_string()
        })
}

/// A file no other file has an edge into — the best case for blast radius.
fn a_leaf(graph: &EntityGraph, files: &[String]) -> Option<String> {
    let mut targeted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for edge in &graph.edges {
        if let Some(to_file) = edge.to_entity.split("::").next() {
            targeted.insert(to_file);
        }
    }
    files
        .iter()
        .find(|f| !targeted.contains(f.as_str()) && is_js_ts(f))
        .cloned()
}

fn is_js_ts(path: &str) -> bool {
    [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// two distinct JS/TS files for the "importchurn" scenario — a
/// "provider" that will gain a new export, and a "consumer" that will start
/// importing it. Picked generically (first two JS/TS files) so this works on
/// any real corpus, not just the synthetic fixture in `session.rs`'s tests.
fn import_churn_pair(files: &[String]) -> Option<(String, String)> {
    let mut js_ts = files.iter().filter(|f| is_js_ts(f));
    let provider = js_ts.next()?.clone();
    let consumer = js_ts.next()?.clone();
    Some((provider, consumer))
}

/// A repo-relative import specifier from `from_file` to `to_file`
/// (`./sibling`, `../dir/other`, …) — enough for `scan_import_file`'s
/// relative-path resolution to find it, computed from the path strings alone
/// so this scenario needs no corpus-specific knowledge.
fn repo_relative_import(from_file: &str, to_file: &str) -> String {
    let from_dir: Vec<&str> = match from_file.rsplit_once('/') {
        Some((dir, _)) => dir.split('/').collect(),
        None => Vec::new(),
    };
    let to_no_ext = to_file.rsplit_once('.').map(|(p, _)| p).unwrap_or(to_file);
    let to_parts: Vec<&str> = to_no_ext.split('/').collect();
    let mut i = 0;
    while i < from_dir.len() && i + 1 < to_parts.len() && from_dir[i] == to_parts[i] {
        i += 1;
    }
    let ups = from_dir.len() - i;
    let mut rel = String::new();
    if ups == 0 {
        rel.push_str("./");
    } else {
        for _ in 0..ups {
            rel.push_str("../");
        }
    }
    rel.push_str(&to_parts[i..].join("/"));
    rel
}

fn pick(scenario: &str, graph: &EntityGraph, files: &[String]) -> Vec<String> {
    match scenario {
        "none" => Vec::new(),
        "leaf" => a_leaf(graph, files).into_iter().collect(),
        "hub" | "hubrename" => highest_fan_in(graph, files).into_iter().collect(),
        "tests" => files
            .iter()
            .filter(|f| (f.contains("/tests/") || f.starts_with("tests/")) && is_js_ts(f))
            .take(50)
            .cloned()
            .collect(),
        // `mixedN`: N files spread evenly across the corpus. Any
        // other unrecognized scenario name (including the original
        // `mixed50`) falls back to N=50, unchanged from before this change.
        _ => {
            let n: usize = scenario
                .strip_prefix("mixed")
                .and_then(|rest| rest.parse().ok())
                .unwrap_or(50);
            let js_ts: Vec<&String> = files.iter().filter(|f| is_js_ts(f)).collect();
            if js_ts.is_empty() {
                return Vec::new();
            }
            let step = (js_ts.len() / n.max(1)).max(1);
            js_ts
                .iter()
                .step_by(step)
                .take(n)
                .map(|f| (*f).clone())
                .collect()
        }
    }
}

/// Apply one scenario's mutation(s) via `restore`, returning the paths that
/// actually changed. `importchurn` is handled separately from
/// every other scenario: it needs a *pair* of files (a provider that gains
/// an export, a consumer that starts importing it), not one mutation applied
/// uniformly to a picked target list.
fn apply_scenario(
    scenario: &str,
    graph: &EntityGraph,
    files: &[String],
    restore: &mut Restore,
) -> Vec<String> {
    if scenario == "importchurn" {
        let mut changed = Vec::new();
        if let Some((provider, consumer)) = import_churn_pair(files) {
            let rel_path = repo_relative_import(&consumer, &provider);
            if restore.add_export(&provider, "__importChurnProvided") {
                changed.push(provider.clone());
            }
            if restore.add_import_and_use(
                &consumer,
                &rel_path,
                "__importChurnProvided",
                "__importChurnConsumed",
            ) {
                changed.push(consumer.clone());
            }
        }
        return changed;
    }
    let targets = pick(scenario, graph, files);
    let mut changed = Vec::new();
    for target in &targets {
        let applied = if scenario == "hubrename" {
            restore.rename_export(target)
        } else {
            restore.mutate(target)
        };
        if applied {
            changed.push(target.clone());
        }
    }
    changed
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: incr_probe <repo_root> <none|leaf|mixed50|hub|tests|importchurn|all> [label]"
        );
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[1]).canonicalize().expect("bad root");
    let scenarios: Vec<&str> = if args[2] == "all" {
        vec![
            "none",
            "leaf",
            "mixed50",
            "hub",
            "hubrename",
            "tests",
            "importchurn",
        ]
    } else {
        vec![args[2].as_str()]
    };
    let label = args.get(3).cloned().unwrap_or_else(|| {
        root.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let registry = make_registry(&root);
    let files = walk_files(&root, &registry);

    let t0 = Instant::now();
    let mut session = GraphSession::build(&root, &files, &registry);
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let base = fingerprint(session.graph(), session.entities());
    println!(
        "COLD label={label} files={} entities={} edges={} edge_hash={:016x} cold_ms={cold_ms:.2}",
        files.len(),
        base.entities,
        base.edges,
        base.edge_hash
    );

    // `SEM_INCR_PROBE_SESSION_ONLY=1` skips both oracle cross-builds so a run
    // under `/usr/bin/time -l` measures the session's own footprint rather than
    // the session plus two full graphs held live beside it.
    let session_only = std::env::var("SEM_INCR_PROBE_SESSION_ONLY").is_ok();
    if session_only {
        for scenario in scenarios {
            let mut restore = Restore {
                root: root.clone(),
                originals: HashMap::new(),
            };
            let changed = apply_scenario(scenario, session.graph(), &files, &mut restore);
            let t0 = Instant::now();
            let stats = session.rebuild(&files, &changed, &registry);
            println!(
                "WARM label={label} scenario={scenario} changed={} warm_ms={:.2} files_red={} files_green={}",
                changed.len(),
                t0.elapsed().as_secs_f64() * 1000.0,
                stats.files_red,
                stats.files_green
            );
            drop(restore);
            session.rebuild(&files, &changed, &registry);
        }
        return;
    }

    // Cross-check: the session's cold build must equal a plain EntityGraph::build.
    let (plain_graph, plain_entities) = EntityGraph::build(&root, &files, &registry);
    let plain = fingerprint(&plain_graph, &plain_entities);
    println!(
        "ORACLE label={label} scenario=cold-vs-build {}",
        if plain.edge_hash == base.edge_hash && plain.entity_hash == base.entity_hash {
            "ok".to_string()
        } else {
            format!(
                "MISMATCH session({:016x}/{}) vs build({:016x}/{})",
                base.edge_hash, base.edges, plain.edge_hash, plain.edges
            )
        }
    );
    drop(plain_graph);
    drop(plain_entities);

    for scenario in scenarios {
        let mut restore = Restore {
            root: root.clone(),
            originals: HashMap::new(),
        };
        let changed = apply_scenario(scenario, session.graph(), &files, &mut restore);

        let t0 = Instant::now();
        let stats = session.rebuild(&files, &changed, &registry);
        let warm_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let warm = fingerprint(session.graph(), session.entities());

        let (cold_graph, cold_entities) = EntityGraph::build(&root, &files, &registry);
        let cold_b = fingerprint(&cold_graph, &cold_entities);

        println!(
            "WARM label={label} scenario={scenario} changed={} warm_ms={warm_ms:.2} \
             files_red={} files_green={} files_green_bow={} edges_reused={} edges_rederived={} \
             changed_keys={} entities={} edges={} edge_hash={:016x}",
            changed.len(),
            stats.files_red,
            stats.files_green,
            stats.files_green_bow,
            stats.edges_reused,
            stats.edges_rederived,
            stats.changed_keys,
            warm.entities,
            warm.edges,
            warm.edge_hash,
        );
        println!(
            "ORACLE label={label} scenario={scenario} {}",
            if warm.edge_hash == cold_b.edge_hash
                && warm.entity_hash == cold_b.entity_hash
                && warm.edges == cold_b.edges
                && warm.entities == cold_b.entities
            {
                "ok".to_string()
            } else {
                format!(
                    "MISMATCH warm(entities={} edges={} hash={:016x}) vs cold(entities={} edges={} hash={:016x})",
                    warm.entities,
                    warm.edges,
                    warm.edge_hash,
                    cold_b.entities,
                    cold_b.edges,
                    cold_b.edge_hash
                )
            }
        );
        drop(cold_graph);
        drop(cold_entities);

        // Put the tree back and re-sync the session so the next scenario starts
        // from the pristine state, exactly like the first one did.
        drop(restore);
        session.rebuild(&files, &changed, &registry);
    }
}
