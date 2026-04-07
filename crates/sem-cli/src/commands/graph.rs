use std::path::Path;

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::registry::ParserRegistry;

use crate::cache::DiskCache;

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

/// Extract all entities from the given files in parallel.
pub fn extract_all_entities(root: &Path, file_paths: &[String], registry: &ParserRegistry) -> Vec<SemanticEntity> {
    file_paths
        .iter()
        .filter_map(|fp| {
            let full = root.join(fp);
            let content = std::fs::read_to_string(&full).ok()?;
            let plugin = registry.get_plugin(fp)?;
            Some(plugin.extract_entities(&content, fp))
        })
        .flatten()
        .collect()
}

/// Build the entity graph + entities, using the disk cache when possible.
pub fn get_or_build_graph(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
) -> (EntityGraph, Vec<SemanticEntity>) {
    if !no_cache {
        // Try loading from cache
        if let Ok(disk) = DiskCache::open(root) {
            if let Some(cached) = disk.load(root, file_paths) {
                return cached;
            }
        }
    }

    // Cache miss (or --no-cache): build from scratch
    let graph = EntityGraph::build(root, file_paths, registry);
    let entities = extract_all_entities(root, file_paths, registry);

    if !no_cache {
        // Best-effort save
        if let Ok(disk) = DiskCache::open(root) {
            let _ = disk.save(root, file_paths, &graph, &entities);
        }
    }

    (graph, entities)
}
