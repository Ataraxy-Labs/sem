use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::EntityGraph;
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
    let file_paths = find_supported_files_inner(root, &registry, &ext_filter, opts.no_default_excludes);
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
    find_supported_files_inner(root, registry, ext_filter, false)
}

/// File names that are always excluded from graph/index (lockfiles, generated content).
const DEFAULT_EXCLUDED_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Gemfile.lock",
    "Pipfile.lock",
    "poetry.lock",
    "composer.lock",
    "go.sum",
    "flake.lock",
];

/// Directory names that are always excluded from graph/index (fixtures, benchmarks, vendor).
const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    "fixtures",
    "fixture",
    "benchmarks",
    "vendor",
    "node_modules",
    "test-harness",
];

fn is_default_excluded(rel_path: &str) -> bool {
    // Check file name
    if let Some(file_name) = rel_path.rsplit('/').next() {
        if DEFAULT_EXCLUDED_FILES.contains(&file_name) {
            return true;
        }
    }
    // Check directory components
    for component in rel_path.split('/') {
        if DEFAULT_EXCLUDED_DIRS.contains(&component) {
            return true;
        }
    }
    false
}

fn find_supported_files_inner(root: &Path, registry: &ParserRegistry, ext_filter: &[String], no_default_excludes: bool) -> Vec<String> {
    let mut files = Vec::new();

    // Use the `ignore` crate to walk the filesystem respecting .gitignore and .semignore
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)       // skip hidden files/dirs
        .git_ignore(true)   // respect .gitignore
        .git_global(true)   // respect global gitignore
        .git_exclude(true); // respect .git/info/exclude

    // Respect .semignore if present
    let semignore = root.join(".semignore");
    if semignore.exists() {
        builder.add_ignore(semignore);
    }

    let walker = builder.build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().to_string();
            if !no_default_excludes && is_default_excluded(&rel_str) {
                continue;
            }
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
