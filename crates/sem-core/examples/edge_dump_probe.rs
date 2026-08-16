//! One-off diagnostic (semx-jo1 follow-up): dump every resolved edge,
//! sorted, so two runs (e.g. at different `SCOPE_RESOLVE_FILE_CHUNK_SIZE`
//! values) can be diffed to find exactly which edge(s) differ. Not part of
//! the public example surface this bead ships.
//!
//! Usage: cargo run --release --example edge_dump_probe -- <repo_root> <out_file>

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
    let (graph, _entities) = EntityGraph::build(&root, &file_paths, &registry);

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
}
