//! Single-shot memory probe (semx-4w1).
//!
//! `perf_probe` deliberately runs several *separate* full-corpus passes in
//! one process (WALK, IO, PARSE_EXTRACT, PASS1_ONLY, BUILD_TOTAL, then a
//! LANG_RATE re-parse per extension) to answer *timing* questions phase by
//! phase. That is fine for wall-clock attribution — each phase's timer is
//! independent — but it is the wrong harness for *peak RSS*: `/usr/bin/time
//! -l`'s "maximum resident set size" is a high-water mark over the whole
//! process lifetime, so a `perf_probe` RSS figure is the peak across *all*
//! of those passes, not the peak of any single one. A real caller (sem-cli,
//! the facts layer) calls `EntityGraph::build` exactly once per cold build.
//!
//! This probe does exactly that — walk the files, then one
//! `EntityGraph::build` call, nothing else — so `/usr/bin/time -l` on *this*
//! process measures what a real cold build actually costs.
//!
//! Usage:
//!   SEM_PROFILE_MEM=1 /usr/bin/time -l \
//!     cargo run --release --example mem_single_probe -- <repo_root>

use std::path::{Path, PathBuf};
use std::time::Instant;

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
    let root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: mem_single_probe <repo_root>");
    let label = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());

    let registry = make_registry(&root);
    let file_paths = walk_files(&root, &registry);

    let t0 = Instant::now();
    let (graph, entities) = EntityGraph::build(&root, &file_paths, &registry);
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!(
        "SINGLE_BUILD label={label} files={} entities={} edges={} build_ms={build_ms:.2}",
        file_paths.len(),
        entities.len(),
        graph.edges.len()
    );
    // Keep both alive through the print above so the compiler can't drop
    // them before `/usr/bin/time -l` samples the still-live peak.
    std::hint::black_box((&graph, &entities));
}
