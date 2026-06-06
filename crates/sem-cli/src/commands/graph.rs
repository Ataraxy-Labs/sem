use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityRef, RefType};
use sem_core::parser::registry::ParserRegistry;
use serde::ser::{SerializeMap, Serializer};

use crate::cache::DiskCache;
use crate::timings::Timings;

pub struct GraphOptions {
    pub cwd: String,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

pub fn graph_command(opts: GraphOptions) {
    let mut timings = Timings::from_env("graph");
    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());
    let ext_filter = normalize_exts(&opts.file_exts);
    let file_paths =
        find_supported_files_inner(root, &registry, &ext_filter, opts.no_default_excludes);
    timings.mark("file_discovery");
    if opts.json && !opts.no_cache {
        if let Ok(disk) = DiskCache::open(root) {
            timings.mark("cache_open");
            let stdout = std::io::stdout();
            match disk.write_graph_json_topology(root, &file_paths, stdout.lock()) {
                Ok(true) => {
                    timings.mark("cache_topology_json_stream");
                    timings.finish();
                    return;
                }
                Ok(false) => {}
                Err(err) => {
                    eprintln!(
                        "{} failed to stream cached graph JSON: {}",
                        "error:".red().bold(),
                        err
                    );
                    std::process::exit(1);
                }
            }
        }
    }

    let graph = get_or_build_graph_topology_with_timings(
        root,
        &file_paths,
        &registry,
        opts.no_cache,
        &mut timings,
    );

    if opts.json {
        write_graph_json(&graph).unwrap();
        timings.mark("cli_output_serialization");
    } else {
        timings.mark("cli_output_serialization");
        println!(
            "{} {} entities, {} edges",
            "⊕".green(),
            graph.entities.len().to_string().bold(),
            graph.edges.len().to_string().bold(),
        );
    }
    timings.finish();
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphStats {
    entity_count: usize,
    edge_count: usize,
}

fn write_graph_json(graph: &EntityGraph) -> serde_json::Result<()> {
    let mut entities = graph.entities.values().collect::<Vec<_>>();
    entities.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges = graph.edges.iter().collect::<Vec<_>>();
    edges.sort_by(compare_entity_refs);

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut serializer = serde_json::Serializer::new(&mut stdout);
    let mut map = (&mut serializer).serialize_map(Some(3))?;
    map.serialize_entry("entities", &entities)?;
    map.serialize_entry("edges", &edges)?;
    map.serialize_entry(
        "stats",
        &GraphStats {
            entity_count: graph.entities.len(),
            edge_count: graph.edges.len(),
        },
    )?;
    map.end()?;
    use std::io::Write;
    stdout.write_all(b"\n").map_err(serde_json::Error::io)
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
    let mut timings = Timings::disabled("graph");
    get_or_build_graph_with_timings(root, file_paths, registry, no_cache, &mut timings)
}

pub fn get_or_build_graph_with_timings(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    timings: &mut Timings,
) -> (EntityGraph, Vec<SemanticEntity>) {
    if !no_cache {
        if let Ok(disk) = DiskCache::open(root) {
            timings.mark("cache_open");
            // Try full cache hit
            if let Some(cached) = disk.load(root, file_paths) {
                timings.mark("cache_full_load");
                return cached;
            }

            // Try incremental: load clean cached data, rebuild only stale files
            if let Some(partial) = disk.load_partial(root, file_paths) {
                timings.mark("cache_partial_load");
                let (graph, entities, metadata) =
                    EntityGraph::build_incremental_with_metadata_and_import_candidates(
                        root,
                        &partial.stale_files,
                        file_paths,
                        partial.cached_entities,
                        partial.cached_edges,
                        partial.stale_file_entities,
                        Some(&partial.cached_importing_stale_files),
                        registry,
                    );
                timings.mark("incremental_graph_rebuild");
                let _ = disk.save_incremental_with_repair_metadata(
                    root,
                    file_paths,
                    &partial.stale_files,
                    &graph,
                    &entities,
                    metadata.repaired_clean_entity_ids,
                    &metadata.recomputed_edge_source_ids,
                    &metadata.deleted_entity_ids,
                );
                timings.mark("cache_incremental_save");
                return (graph, entities);
            }
        }
    }

    // Full rebuild
    let (graph, entities) = EntityGraph::build(root, file_paths, registry);
    timings.mark("full_graph_build");

    if !no_cache {
        if let Ok(disk) = DiskCache::open(root) {
            let _ = disk.save(root, file_paths, &graph, &entities);
            timings.mark("cache_full_save");
        }
    }

    (graph, entities)
}

pub fn get_or_build_graph_topology_with_timings(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    timings: &mut Timings,
) -> EntityGraph {
    if !no_cache {
        if let Ok(disk) = DiskCache::open(root) {
            timings.mark("cache_open");
            if let Some(graph) = disk.load_graph_topology(root, file_paths) {
                timings.mark("cache_topology_load");
                return graph;
            }
        }
    }

    let (graph, _entities) =
        get_or_build_graph_with_timings(root, file_paths, registry, no_cache, timings);
    graph
}
