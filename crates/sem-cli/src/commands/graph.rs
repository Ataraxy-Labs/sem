use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityRef, RefType};
use sem_core::parser::registry::ParserRegistry;

use crate::cache::DiskCache;

pub struct GraphOptions {
    pub cwd: String,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

pub fn graph_command(opts: GraphOptions) {
    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());
    let ext_filter = normalize_exts(&opts.file_exts);
    let file_paths =
        find_supported_files_inner(root, &registry, &ext_filter, opts.no_default_excludes);
    let (graph, _entities) = get_or_build_graph(root, &file_paths, &registry, opts.no_cache);

    if opts.json {
        let mut entities = graph.entities.values().collect::<Vec<_>>();
        entities.sort_by(|a, b| a.id.cmp(&b.id));

        let mut edges = graph.edges.iter().collect::<Vec<_>>();
        edges.sort_by(compare_entity_refs);

        let output = serde_json::json!({
            "entities": entities,
            "edges": edges,
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

fn compare_entity_refs(a: &&EntityRef, b: &&EntityRef) -> std::cmp::Ordering {
    a.from_entity
        .cmp(&b.from_entity)
        .then_with(|| a.to_entity.cmp(&b.to_entity))
        .then_with(|| ref_type_sort_key(&a.ref_type).cmp(&ref_type_sort_key(&b.ref_type)))
}

fn ref_type_sort_key(ref_type: &RefType) -> u8 {
    match ref_type {
        RefType::Calls => 0,
        RefType::Imports => 1,
        RefType::TypeRef => 2,
    }
}

/// Normalize extension strings: ensure each starts with '.'
pub fn normalize_exts(exts: &[String]) -> Vec<String> {
    exts.iter()
        .map(|e| {
            if e.starts_with('.') {
                e.clone()
            } else {
                format!(".{}", e)
            }
        })
        .collect()
}

/// Find all supported files in the repo (public for use by other commands).
pub fn find_supported_files_public(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
) -> Vec<String> {
    find_supported_files_with_options(root, registry, ext_filter, false)
}

pub fn find_supported_files_with_options(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Vec<String> {
    super::files::find_supported_files_in_path(
        root,
        root,
        registry,
        ext_filter,
        no_default_excludes,
    )
}

fn find_supported_files_inner(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Vec<String> {
    find_supported_files_with_options(root, registry, ext_filter, no_default_excludes)
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
