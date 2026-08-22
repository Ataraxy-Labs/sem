//! Cross-process oracle + timing probe for the on-disk facts store.
//!
//! Not part of the public API and not wired into any product code path.
//!
//! Unlike `incr_probe`'s in-process oracle, this probe is meant to be run as
//! **two separate OS processes** so the only channel between them is the disk
//! — the same guarantee `sem-cli` gets from `FactsStore` across two `sem`
//! invocations. `save` does a cold build, exports the facts layer, and writes
//! it to `<store_dir>`. `load` starts a brand-new process, loads that
//! snapshot, warm-starts a session from it, optionally mutates a scenario's
//! files first, and asserts entity/edge counts + a sorted-edge hash against a
//! from-scratch cold build of the same on-disk state.
//!
//! Usage:
//!   cargo run --release --example facts_probe -- save <repo_root> <store_dir> [label]
//!   cargo run --release --example facts_probe -- load <repo_root> <store_dir> <none|leaf|mixed50|hub> [label]
//!
//! Prints tagged, greppable lines:
//!   COLD    — cold build + save wall time, entity/edge counts, edge hash, store size
//!   WARM    — cross-process warm-start wall time (load + rebuild), red-green counters
//!   ORACLE  — `ok` when warm == cold at the (possibly mutated) state, `MISMATCH` otherwise

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::facts_store::FactsStore;
use sem_core::parser::graph::{EntityGraph, RefType};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::parser::session::GraphSession;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

const PROBE_SUFFIX_JS_TS: &str = "\n// facts probe\nfunction __factsHelper() { return 1; }\nexport function __factsProbe() { return __factsHelper(); }\n";
const PROBE_SUFFIX_PY: &str =
    "\n\ndef __facts_helper():\n    return 1\n\n\ndef __facts_probe():\n    return __facts_helper()\n";
const PROBE_SUFFIX_GO: &str =
    "\n\nfunc factsHelper() int { return 1 }\n\nfunc factsProbe() int { return factsHelper() }\n";
const PROBE_SUFFIX_RUST: &str =
    "\n\nfn facts_helper() -> i32 { 1 }\n\npub fn facts_probe() -> i32 { facts_helper() }\n";

/// A same-file, syntactically-valid append for whichever language `path`
/// detects as (: generalized past the JS/TS-only original so this
/// probe can exercise the newly-attributed languages too). Falls back to the
/// JS/TS snippet for anything else — callers only ever invoke this on a path
/// [`is_reuse_eligible`] already accepted, so the fallback is unreachable in
/// practice, not a silent wrong-language mutation.
fn probe_suffix_for(path: &str) -> &'static str {
    if path.ends_with(".py") {
        PROBE_SUFFIX_PY
    } else if path.ends_with(".go") {
        PROBE_SUFFIX_GO
    } else if path.ends_with(".rs") {
        PROBE_SUFFIX_RUST
    } else {
        PROBE_SUFFIX_JS_TS
    }
}

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

fn is_js_ts(path: &str) -> bool {
    [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Mirrors `sem_core::parser::import_resolution::is_reuse_eligible_file`
/// Duplicated here because that function is `pub(crate)` and this
/// probe links against the crate as an ordinary dependent, not from inside
/// it. Extensions must be kept in sync by hand; both lists are small and
/// change rarely.
fn is_reuse_eligible(path: &str) -> bool {
    is_js_ts(path) || path.ends_with(".py") || path.ends_with(".go") || path.ends_with(".rs")
}

struct Fingerprint {
    entities: usize,
    edges: usize,
    edge_hash: u64,
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
    Fingerprint {
        entities: entities.len(),
        edges: graph.edges.len(),
        edge_hash: h.finish(),
    }
}

/// Restores every mutated file on drop, including on panic — a probe that
/// leaves a real checkout mutated is worse than no probe.
struct Restore {
    root: PathBuf,
    originals: HashMap<String, String>,
}

impl Restore {
    fn mutate(&mut self, rel: &str) -> bool {
        let full = self.root.join(rel);
        let Ok(original) = std::fs::read_to_string(&full) else {
            return false;
        };
        let mutated = format!("{original}{}", probe_suffix_for(rel));
        if std::fs::write(&full, mutated).is_err() {
            return false;
        }
        self.originals.insert(rel.to_string(), original);
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

fn pick(scenario: &str, graph: &EntityGraph, files: &[String]) -> Vec<String> {
    match scenario {
        "none" => Vec::new(),
        "leaf" => {
            let mut targeted: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for edge in &graph.edges {
                if let Some(to_file) = edge.to_entity.split("::").next() {
                    targeted.insert(to_file);
                }
            }
            files
                .iter()
                .find(|f| !targeted.contains(f.as_str()) && is_reuse_eligible(f))
                .cloned()
                .into_iter()
                .collect()
        }
        "hub" => {
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
                .map(|(f, _)| f.to_string())
                .into_iter()
                .collect()
        }
        _ => {
            // mixed50: 50 files spread evenly across the corpus.
            let eligible: Vec<&String> = files.iter().filter(|f| is_reuse_eligible(f)).collect();
            if eligible.is_empty() {
                return Vec::new();
            }
            let step = (eligible.len() / 50).max(1);
            eligible
                .iter()
                .step_by(step)
                .take(50)
                .map(|f| (*f).clone())
                .collect()
        }
    }
}

fn cmd_save(root: &Path, store_dir: &Path, label: &str) {
    let registry = make_registry(root);
    let files = walk_files(root, &registry);

    let build_t0 = Instant::now();
    let session = GraphSession::build(root, &files, &registry);
    let build_ms = build_t0.elapsed().as_secs_f64() * 1000.0;
    let fp = fingerprint(session.graph(), session.entities());

    let export_t0 = Instant::now();
    let exported = session.export_persisted();
    let export_ms = export_t0.elapsed().as_secs_f64() * 1000.0;

    let store = FactsStore::open(store_dir);
    let save_t0 = Instant::now();
    store.save(root, &exported).expect("save");
    let save_ms = save_t0.elapsed().as_secs_f64() * 1000.0;

    let store_bytes: u64 = std::fs::read_dir(store_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);

    println!(
        "COLD label={label} files={} entities={} edges={} edge_hash={:016x} build_ms={build_ms:.2} export_ms={export_ms:.2} save_ms={save_ms:.2} store_bytes={store_bytes}",
        files.len(),
        fp.entities,
        fp.edges,
        fp.edge_hash,
    );
}

fn cmd_load(root: &Path, store_dir: &Path, scenario: &str, label: &str) {
    let registry = make_registry(root);
    let files = walk_files(root, &registry);

    // Apply the scenario's mutation *before* the warm process ever looks at
    // the tree, exactly like an editor session handing `sem` a dirty
    // checkout — the store's stale entries for touched files must miss.
    //
    // Picking mutation targets needs a graph, so build one cheaply from the
    // pre-mutation `EntityGraph::build` only when a scenario other than
    // "none" is requested; this build is not timed (it substitutes for "the
    // developer already knows which files they edited").
    let mut restore = Restore {
        root: root.to_path_buf(),
        originals: HashMap::new(),
    };
    let changed: Vec<String> = if scenario == "none" {
        Vec::new()
    } else {
        let (probe_graph, _probe_entities) = EntityGraph::build(root, &files, &registry);
        let targets = pick(scenario, &probe_graph, &files);
        targets
            .iter()
            .filter(|t| restore.mutate(t))
            .cloned()
            .collect()
    };

    let store = FactsStore::open(store_dir);

    let load_t0 = Instant::now();
    let loaded = store.load(root);
    let load_ms = load_t0.elapsed().as_secs_f64() * 1000.0;
    let Some(loaded) = loaded else {
        println!("WARM label={label} scenario={scenario} MISS store had nothing for this root");
        std::process::exit(1);
    };
    let stored_files = loaded.file_count();
    let stored_fingerprints = loaded.fingerprint_count();
    let stored_resolved = loaded.resolved_file_count();
    eprintln!(
        "DEBUG label={label} scenario={scenario} stored_files={stored_files} stored_fingerprints={stored_fingerprints} stored_resolved={stored_resolved}"
    );

    let warm_t0 = Instant::now();
    let (session, stats) = GraphSession::warm_start(root, &files, &registry, loaded);
    let warm_ms = warm_t0.elapsed().as_secs_f64() * 1000.0;
    let warm_total_ms = load_ms + warm_ms;
    let warm = fingerprint(session.graph(), session.entities());

    println!(
        "WARM label={label} scenario={scenario} stored_files={stored_files} changed={} load_ms={load_ms:.2} warm_start_ms={warm_ms:.2} warm_total_ms={warm_total_ms:.2} files_seed_red={} files_red={} files_green={} files_green_bow={} changed_keys={} entities={} edges={} edge_hash={:016x}",
        changed.len(),
        stats.files_seed_red,
        stats.files_red,
        stats.files_green,
        stats.files_green_bow,
        stats.changed_keys,
        warm.entities,
        warm.edges,
        warm.edge_hash,
    );

    // Oracle: fresh-process warm-start must equal a from-scratch cold build
    // of the exact same (possibly mutated) on-disk state.
    let (cold_graph, cold_entities) = EntityGraph::build(root, &files, &registry);
    let cold = fingerprint(&cold_graph, &cold_entities);

    println!(
        "ORACLE label={label} scenario={scenario} {}",
        if warm.edge_hash == cold.edge_hash
            && warm.edges == cold.edges
            && warm.entities == cold.entities
        {
            "ok".to_string()
        } else {
            format!(
                "MISMATCH warm(entities={} edges={} hash={:016x}) vs cold(entities={} edges={} hash={:016x})",
                warm.entities, warm.edges, warm.edge_hash, cold.entities, cold.edges, cold.edge_hash
            )
        }
    );

    drop(restore);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage:\n  facts_probe save <repo_root> <store_dir> [label]\n  facts_probe load <repo_root> <store_dir> <none|leaf|mixed50|hub> [label]"
        );
        std::process::exit(1);
    }
    let mode = args[1].as_str();
    let root = PathBuf::from(&args[2]).canonicalize().expect("bad root");
    let store_dir = PathBuf::from(&args[3]);

    match mode {
        "save" => {
            let label = args.get(4).cloned().unwrap_or_else(|| {
                root.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
            cmd_save(&root, &store_dir, &label);
        }
        "load" => {
            if args.len() < 5 {
                eprintln!("load needs a scenario: none|leaf|mixed50|hub");
                std::process::exit(1);
            }
            let scenario = args[4].as_str();
            let label = args.get(5).cloned().unwrap_or_else(|| {
                root.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
            cmd_load(&root, &store_dir, scenario, &label);
        }
        other => {
            eprintln!("unknown mode {other:?}, expected save|load");
            std::process::exit(1);
        }
    }
}
