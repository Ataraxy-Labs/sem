//! One-off diagnostic (follow-up): dump every resolved edge,
//! sorted, so two runs (e.g. at different `SCOPE_RESOLVE_FILE_CHUNK_SIZE`
//! values) can be diffed to find exactly which edge(s) differ. Not part of
//! the public example surface this change ships.
//!
//! Usage: cargo run --release --example edge_dump_probe -- <repo_root> <out_file>
//!
//! DANGLING_EDGE_ORACLE (a follow-up to the Go memory-check work): every edge's
//! `to_entity`/`from_entity` must name a real id in this same build's
//! entity set — the invariant `PrecomputedFileFacts::rekey_entity_ids`'
//! `Scope::defs`/`Scope::owner_id` gap violated (a stale pre-Go-rewrite id
//! left in a precomputed scope's `.defs` survived into an edge whose
//! target no entity held any more, only detected by hand via
//! `entity_probe`). This oracle makes that bug class self-detecting:
//! printed once as `DANGLING_EDGE name=... from=... to=... ref_type=...`
//! per offender, then a gating `DANGLING_EDGE_ORACLE edges=... checked=...
//! dangling=... verdict=ok|MISMATCH` line, matching this crate's other
//! probes' `ORACLE ... verdict=` convention (see `index_probe.rs`,
//! `incr_probe.rs`). Runs unconditionally — the check is O(edges) against
//! an already-built `HashSet<&str>`, not a second graph build.
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

fn make_registry(root: &Path) -> ParserRegistry {
    let mut registry = create_default_registry();
    registry.load_semrc(root);
    registry.load_gitattributes(root);
    registry
}

fn walk_files(root: &Path, _registry: &ParserRegistry) -> Vec<String> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_default_excluded(&rel_str) {
            continue;
        }
        if is_probably_binary_path(&rel_str) {
            continue;
        }
        files.push(rel_str);
    }
    files.sort();
    files
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root: PathBuf = args
        .next()
        .expect("usage: edge_dump_probe <repo_root> <out_file>")
        .into();
    let out_path: PathBuf = args
        .next()
        .expect("usage: edge_dump_probe <repo_root> <out_file>")
        .into();

    let registry = make_registry(&root);
    let file_paths = walk_files(&root, &registry);
    let (graph, entities) = EntityGraph::build(&root, &file_paths, &registry);

    let mut lines: Vec<String> = graph
        .edges
        .iter()
        .map(|e| format!("{}\t{:?}\t{}", e.from_entity, e.ref_type, e.to_entity))
        .collect();
    lines.sort();

    let mut f = std::fs::File::create(&out_path).expect("create out file");
    for line in &lines {
        writeln!(f, "{line}").expect("write line");
    }
    eprintln!("wrote {} edges to {}", lines.len(), out_path.display());

    dangling_edge_oracle(&graph.edges, &entities);
}

/// DANGLING_EDGE_ORACLE: `∀ e ∈ edges. e.from_entity ∈ ids ∧ e.to_entity ∈
/// ids`, where `ids` is this same build's own entity set. A build that
/// produces an edge naming an id nothing declared is always a bug (a
/// resolver-internal id going stale between when it was captured and when
/// it was read, e.g. a rewrite this crate ran without rekeying every place
/// that id could be cached) — never a legitimate "the target doesn't exist
/// yet" case, since `resolve_ref` only ever returns ids it read out of this
/// same build's own tables.
fn dangling_edge_oracle(
    edges: &[sem_core::parser::graph::EntityRef],
    entities: &[sem_core::model::entity::SemanticEntity],
) {
    let ids: HashSet<&str> = entities.iter().map(|e| e.id.as_str()).collect();
    let mut dangling = 0usize;
    for e in edges {
        let from_ok = ids.contains(e.from_entity.as_str());
        let to_ok = ids.contains(e.to_entity.as_str());
        if !from_ok || !to_ok {
            dangling += 1;
            if !from_ok {
                eprintln!(
                    "DANGLING_EDGE endpoint=from from={} ref_type={:?} to={}",
                    e.from_entity, e.ref_type, e.to_entity
                );
            }
            if !to_ok {
                eprintln!(
                    "DANGLING_EDGE endpoint=to from={} ref_type={:?} to={}",
                    e.from_entity, e.ref_type, e.to_entity
                );
            }
        }
    }
    let verdict = if dangling == 0 { "ok" } else { "MISMATCH" };
    eprintln!(
        "DANGLING_EDGE_ORACLE edges={} entities={} dangling={dangling} verdict={verdict}",
        edges.len(),
        entities.len()
    );
}
