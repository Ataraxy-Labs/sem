//! Identical to `mem_single_probe`, except for `#[global_allocator]` (semx-4w1).
//!
//! Exists to test one specific hypothesis about the giant-corpus RSS
//! blowout: that most of the gap between "bytes we can attribute to a named
//! structure" (`SEM_PROFILE_MEM=1`'s checkpoints) and actual measured RSS is
//! system-allocator fragmentation, not a leak or a genuinely-retained
//! structure — a cold build does tens of millions of small, short-lived
//! allocations (tree-sitter AST nodes, per-file temporary maps) across a
//! highly parallel `rayon` workload, and macOS's default allocator does not
//! always return freed arena pages to the OS promptly under that pattern.
//! If peak RSS drops substantially with no source change other than the
//! allocator, that confirms fragmentation as the dominant term. See
//! RESOLUTION-PROFILE.md, "Memory attribution (semx-4w1)".
//!
//! Usage:
//!   SEM_PROFILE_MEM=1 /usr/bin/time -l \
//!     cargo run --release --example mem_single_probe_mimalloc -- <repo_root>

use std::path::{Path, PathBuf};
use std::time::Instant;

use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
        .expect("usage: mem_single_probe_mimalloc <repo_root>");
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
    std::hint::black_box((&graph, &entities));
}
