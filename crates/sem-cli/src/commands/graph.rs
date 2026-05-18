use std::path::Path;

use colored::Colorize;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::registry::ParserRegistry;

use crate::cache::DiskCache;

pub struct GraphOptions {
    pub cwd: String,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
}

pub fn graph_command(opts: GraphOptions) {
    let root = Path::new(&opts.cwd);
    let registry = super::create_registry(&opts.cwd);
    let ext_filter = normalize_exts(&opts.file_exts);
    let file_paths = find_supported_files_public(root, &registry, &ext_filter);
    let (graph, _entities) = get_or_build_graph(root, &file_paths, &registry, opts.no_cache);

    if opts.json {
        let output = serde_json::json!({
            "entities": graph.entities.values().collect::<Vec<_>>(),
            "edges": &graph.edges,
            "stats": {
                "entityCount": graph.entities.len(),
                "edgeCount": graph.edges.len()
            }
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        println!(
            "{} {} entities, {} edges",
            "⊕".green(),
            graph.entities.len().to_string().bold(),
            graph.edges.len().to_string().bold(),
        );
    }
}

/// Normalize extension strings: ensure each starts with '.'
pub fn normalize_exts(exts: &[String]) -> Vec<String> {
    exts.iter().map(|e| {
        if e.starts_with('.') { e.clone() } else { format!(".{}", e) }
    }).collect()
}

/// Find all supported files in the repo (public for use by other commands).
pub fn find_supported_files_public(root: &Path, registry: &ParserRegistry, ext_filter: &[String]) -> Vec<String> {
    find_supported_files(root, registry, ext_filter)
}

fn find_supported_files(root: &Path, registry: &ParserRegistry, ext_filter: &[String]) -> Vec<String> {
    let mut files = Vec::new();

    // Use the `ignore` crate to walk the filesystem respecting .gitignore
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)       // skip hidden files/dirs
        .git_ignore(true)   // respect .gitignore
        .git_global(true)   // respect global gitignore
        .git_exclude(true)  // respect .git/info/exclude
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().to_string();
            if !ext_filter.is_empty() && !ext_filter.iter().any(|ext| rel_str.ends_with(ext.as_str())) {
                continue;
            }
            if registry.get_plugin(&rel_str).is_some() {
                files.push(rel_str);
            }
        }
    }

    files.sort();
    files
}

/// Build the entity graph + entities, using the disk cache when possible.
/// Tries: full cache hit → incremental rebuild (stale files only) → full rebuild.
pub fn get_or_build_graph(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
) -> (EntityGraph, Vec<SemanticEntity>) {
    if !no_cache {
        if let Ok(disk) = DiskCache::open(root) {
            // Try full cache hit
            if let Some(cached) = disk.load(root, file_paths) {
                return cached;
            }

            // Try incremental: load clean cached data, rebuild only stale files
            if let Some(partial) = disk.load_partial(root, file_paths) {
                let (graph, entities) = EntityGraph::build_incremental(
                    root,
                    &partial.stale_files,
                    file_paths,
                    partial.cached_entities,
                    partial.cached_edges,
                    partial.stale_file_entities,
                    registry,
                );
                let _ = disk.save_incremental(
                    root,
                    file_paths,
                    &partial.stale_files,
                    &graph,
                    &entities,
                );
                return (graph, entities);
            }
        }
    }

    // Full rebuild
    let (graph, entities) = EntityGraph::build(root, file_paths, registry);

    if !no_cache {
        if let Ok(disk) = DiskCache::open(root) {
            let _ = disk.save(root, file_paths, &graph, &entities);
        }
    }

    (graph, entities)
}
