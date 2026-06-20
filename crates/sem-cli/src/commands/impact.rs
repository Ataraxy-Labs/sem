use std::{collections::HashSet, path::Path};

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::parser::graph::{EntityGraph, EntityInfo};
use sem_mcp::cache::CacheSourceScope;

use crate::cache::{CachedImpactError, CachedImpactMode, CachedImpactResult, DiskCache};
use crate::timings::Timings;

pub struct ImpactOptions {
    pub cwd: String,
    pub entity_name: Option<String>,
    pub entity_id: Option<String>,
    pub file_hint: Option<String>,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub mode: ImpactMode,
    pub depth: usize,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

#[derive(Clone, Copy)]
pub enum ImpactMode {
    All,
    Deps,
    Dependents,
    Tests,
}

const LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD: usize = 20_000;

/// Run a graph build behind a uv-style spinner, then clear it and print a
/// summary before the result is used (so the spinner never interleaves with
/// command output). `count` extracts the entity count for the summary line.
fn build_with_spinner<T>(
    file_count: usize,
    build: impl FnOnce() -> T,
    count: impl FnOnce(&T) -> usize,
) -> T {
    let prog = crate::progress::Progress::start("Building entity graph");
    let result = build();
    prog.done(&format!(
        "{} entities, {} files",
        super::graph::fmt_count(count(&result)),
        super::graph::fmt_count(file_count)
    ));
    result
}

pub fn impact_command(opts: ImpactOptions) {
    if super::cloud::try_cloud_impact(&opts).is_some() {
        return;
    }

    let mut timings = Timings::from_env("impact");
    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());

    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(root, &ext_filter, opts.no_default_excludes);
    let file_hint = opts
        .file_hint
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), root, file));
    let cache_first_entity_scope = opts.entity_id.is_some() || file_hint.is_some();

    if !opts.no_cache
        && matches!(opts.mode, ImpactMode::Deps)
        && matches!(source_scope, CacheSourceScope::Default)
        && cache_first_entity_scope
    {
        match DiskCache::open(root) {
            Ok(disk) => {
                timings.mark("cache_open");
                if try_cached_impact_query(
                    &disk,
                    root,
                    &[],
                    &opts,
                    file_hint.as_deref(),
                    source_scope,
                    true,
                    &mut timings,
                ) {
                    return;
                }
            }
            Err(_) => {
                timings.mark("cache_open_failed");
            }
        }
    }

    let file_paths = super::graph::find_supported_files_with_options(
        root,
        &registry,
        &ext_filter,
        opts.no_default_excludes,
    );
    timings.mark("file_discovery");

    if !opts.no_cache {
        match DiskCache::open(root) {
            Ok(disk) => {
                timings.mark("cache_open");
                if try_cached_impact_query(
                    &disk,
                    root,
                    &file_paths,
                    &opts,
                    file_hint.as_deref(),
                    source_scope,
                    false,
                    &mut timings,
                ) {
                    return;
                }
            }
            Err(_) => {
                timings.mark("cache_open_failed");
            }
        }
    }

    match opts.mode {
        ImpactMode::Deps => {
            let graph = build_with_spinner(
                file_paths.len(),
                || {
                    if opts.no_cache || file_paths.len() > LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD {
                        let entity_name = opts.entity_name.clone();
                        let entity_id = opts.entity_id.clone();
                        let file_hint_for_match = file_hint.clone();
                        super::graph::get_or_build_direct_dependency_graph_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                            move |entity| {
                                if let Some(id) = entity_id.as_deref() {
                                    return entity.id == id;
                                }
                                let Some(name) = entity_name.as_deref() else {
                                    return false;
                                };
                                if file_hint_for_match
                                    .as_deref()
                                    .is_some_and(|file| entity.file_path != file)
                                {
                                    return false;
                                }
                                super::entity_matches_query(entity, name)
                            },
                        )
                    } else {
                        super::graph::get_or_build_graph_topology_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    }
                },
                |g| g.entities.len(),
            );
            let entity = find_entity(
                &graph,
                opts.entity_name.as_deref(),
                opts.entity_id.as_deref(),
                file_hint.as_deref(),
            );
            timings.mark("entity_lookup");
            print_deps(&graph, entity, opts.json);
            timings.mark("cli_output_serialization");
        }
        ImpactMode::Dependents => {
            let graph = build_with_spinner(
                file_paths.len(),
                || {
                    if file_paths.len() > LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD {
                        super::graph::get_or_build_graph_topology_with_topology_save_on_miss_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    } else {
                        super::graph::get_or_build_graph_topology_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    }
                },
                |g| g.entities.len(),
            );
            let entity = find_entity(
                &graph,
                opts.entity_name.as_deref(),
                opts.entity_id.as_deref(),
                file_hint.as_deref(),
            );
            timings.mark("entity_lookup");
            print_dependents(&graph, entity, opts.json);
            timings.mark("cli_output_serialization");
        }
        ImpactMode::Tests | ImpactMode::All => {
            if file_paths.len() > LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD {
                let graph_data = build_with_spinner(
                    file_paths.len(),
                    || {
                        super::graph::get_or_build_graph_with_test_data_and_topology_save_on_miss_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    },
                    |gd| match gd {
                        super::graph::GraphWithTestData::Full(g, _) => g.entities.len(),
                        super::graph::GraphWithTestData::Topology { graph, .. } => {
                            graph.entities.len()
                        }
                    },
                );
                match graph_data {
                    super::graph::GraphWithTestData::Full(graph, all_entities) => {
                        let entity = find_entity(
                            &graph,
                            opts.entity_name.as_deref(),
                            opts.entity_id.as_deref(),
                            file_hint.as_deref(),
                        );
                        timings.mark("entity_lookup");
                        match opts.mode {
                            ImpactMode::Tests => print_tests(
                                &graph,
                                entity,
                                &all_entities,
                                opts.json,
                                &registry.custom_test_dirs,
                            ),
                            ImpactMode::All => print_all(
                                &graph,
                                entity,
                                &all_entities,
                                opts.json,
                                opts.depth,
                                &registry.custom_test_dirs,
                            ),
                            _ => unreachable!(),
                        }
                    }
                    super::graph::GraphWithTestData::Topology {
                        graph,
                        test_entity_ids,
                    } => {
                        let entity = find_entity(
                            &graph,
                            opts.entity_name.as_deref(),
                            opts.entity_id.as_deref(),
                            file_hint.as_deref(),
                        );
                        timings.mark("entity_lookup");
                        match opts.mode {
                            ImpactMode::Tests => {
                                print_tests_with_ids(&graph, entity, &test_entity_ids, opts.json)
                            }
                            ImpactMode::All => print_all_with_ids(
                                &graph,
                                entity,
                                &test_entity_ids,
                                opts.json,
                                opts.depth,
                            ),
                            _ => unreachable!(),
                        }
                    }
                }
            } else {
                let (graph, all_entities) = build_with_spinner(
                    file_paths.len(),
                    || {
                        super::graph::get_or_build_graph_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    },
                    |(g, _)| g.entities.len(),
                );
                let entity = find_entity(
                    &graph,
                    opts.entity_name.as_deref(),
                    opts.entity_id.as_deref(),
                    file_hint.as_deref(),
                );
                timings.mark("entity_lookup");
                match opts.mode {
                    ImpactMode::Tests => print_tests(
                        &graph,
                        entity,
                        &all_entities,
                        opts.json,
                        &registry.custom_test_dirs,
                    ),
                    ImpactMode::All => print_all(
                        &graph,
                        entity,
                        &all_entities,
                        opts.json,
                        opts.depth,
                        &registry.custom_test_dirs,
                    ),
                    _ => unreachable!(),
                }
            }
            timings.mark("cli_output_serialization");
        }
    }
    timings.finish();
}

fn try_cached_impact_query(
    disk: &DiskCache,
    root: &Path,
    file_paths: &[String],
    opts: &ImpactOptions,
    file_hint: Option<&str>,
    source_scope: CacheSourceScope,
    cache_first: bool,
    timings: &mut Timings,
) -> bool {
    match disk.query_impact_topology(
        root,
        file_paths,
        source_scope,
        cache_first,
        opts.entity_name.as_deref(),
        opts.entity_id.as_deref(),
        file_hint,
        cached_mode_for(opts.mode),
        opts.depth,
    ) {
        Ok(Some(result)) => {
            timings.mark("cache_topology_impact_query");
            print_cached_result(&result, opts.mode, opts.json, opts.depth);
            timings.mark("cli_output_serialization");
            timings.finish();
            true
        }
        Ok(None) => {
            timings.mark("cache_topology_impact_miss");
            false
        }
        Err(CachedImpactError::CacheReadFailed) => {
            timings.mark("cache_topology_impact_query_failed");
            false
        }
        Err(err) => print_cached_error(err),
    }
}

fn cached_mode_for(mode: ImpactMode) -> CachedImpactMode {
    match mode {
        ImpactMode::All => CachedImpactMode::All,
        ImpactMode::Deps => CachedImpactMode::Deps,
        ImpactMode::Dependents => CachedImpactMode::Dependents,
        ImpactMode::Tests => CachedImpactMode::Tests,
    }
}

fn find_entity<'a>(
    graph: &'a EntityGraph,
    name: Option<&str>,
    entity_id: Option<&str>,
    file_hint: Option<&str>,
) -> &'a sem_core::parser::graph::EntityInfo {
    // Direct lookup by entity ID
    if let Some(id) = entity_id {
        if let Some(e) = graph.entities.get(id) {
            return e;
        }
        eprintln!("{} Entity ID '{}' not found", "error:".red().bold(), id);
        std::process::exit(1);
    }

    let name = name.unwrap_or_else(|| {
        eprintln!(
            "{} Either entity name or --entity-id is required",
            "error:".red().bold()
        );
        std::process::exit(1);
    });

    let mut matching: Vec<_> = graph
        .entities
        .values()
        .filter(|e| super::entity_matches_qualified(graph, e, name))
        .collect();

    if matching.is_empty() {
        eprintln!("{} Entity '{}' not found", "error:".red().bold(), name);
        std::process::exit(1);
    }

    if let Some(file) = file_hint {
        let filtered: Vec<_> = matching
            .iter()
            .filter(|e| e.file_path == file)
            .copied()
            .collect();
        if filtered.len() == 1 {
            return filtered[0];
        }
        if filtered.is_empty() {
            eprintln!(
                "{} Entity '{}' not found in file '{}'",
                "error:".red().bold(),
                name,
                file
            );
            std::process::exit(1);
        }
        // Multiple matches even within the file — fall through to ambiguity error
        matching = filtered;
    }

    if matching.len() == 1 {
        return matching[0];
    }

    // Multiple matches — report ambiguity
    matching.sort_by_key(|e| (&e.file_path, e.start_line));
    eprintln!(
        "{} Entity name '{}' is ambiguous ({} matches). Specify --file or --entity-id:",
        "error:".red().bold(),
        name,
        matching.len()
    );
    for m in &matching {
        eprintln!(
            "  {} {} ({}:L{})",
            m.entity_type, m.id, m.file_path, m.start_line
        );
    }
    std::process::exit(1);
}

fn entity_json(e: &sem_core::parser::graph::EntityInfo) -> serde_json::Value {
    serde_json::json!({
        "entityId": e.id, "name": e.name, "type": e.entity_type,
        "file": e.file_path, "lines": [e.start_line, e.end_line],
    })
}

fn entity_list_json(entities: &[&sem_core::parser::graph::EntityInfo]) -> Vec<serde_json::Value> {
    entities.iter().map(|e| entity_json(*e)).collect()
}

fn owned_entity_list_json(entities: &[EntityInfo]) -> Vec<serde_json::Value> {
    entities.iter().map(entity_json).collect()
}

fn print_entity_header(e: &sem_core::parser::graph::EntityInfo) {
    println!(
        "{} {} {} ({}:{}–{})",
        "⊕".green(),
        e.entity_type.dimmed(),
        e.name.bold(),
        e.file_path.dimmed(),
        e.start_line,
        e.end_line,
    );
}

fn print_cached_result(result: &CachedImpactResult, mode: ImpactMode, json: bool, depth: usize) {
    match mode {
        ImpactMode::Deps => {
            print_cached_deps(&result.entity, &result.dependencies, json);
        }
        ImpactMode::Dependents => {
            print_cached_dependents(&result.entity, &result.dependents, json);
        }
        ImpactMode::Tests => {
            print_cached_tests(&result.entity, &result.tests, result.tests_truncated, json);
        }
        ImpactMode::All => {
            print_cached_all(result, json, depth);
        }
    }
}

fn print_cached_deps(entity: &EntityInfo, deps: &[EntityInfo], json: bool) {
    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependencies": owned_entity_list_json(deps),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if deps.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No dependencies.".dimmed());
        } else {
            println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
            for dep in deps {
                println!(
                    "    {} {} {} ({})",
                    "→".blue(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
        println!();
    }
}

fn print_cached_dependents(entity: &EntityInfo, dependents: &[EntityInfo], json: bool) {
    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependents": owned_entity_list_json(dependents),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if dependents.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No dependents.".dimmed());
        } else {
            println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
            for dep in dependents {
                println!(
                    "    {} {} {} ({})",
                    "←".yellow(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
        println!();
    }
}

fn print_cached_tests(entity: &EntityInfo, tests: &[EntityInfo], truncated: bool, json: bool) {
    if json {
        let mut output = serde_json::json!({
            "entity": entity_json(entity),
            "tests": owned_entity_list_json(tests),
        });
        if truncated {
            output
                .as_object_mut()
                .unwrap()
                .insert("testsTruncated".to_string(), serde_json::json!(true));
        }
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if tests.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No tests found.".dimmed());
        } else {
            println!(
                "\n  {} {}",
                "⚡".yellow(),
                format!("{} tests affected:", tests.len()).bold()
            );
            let mut by_file: std::collections::HashMap<&str, Vec<_>> =
                std::collections::HashMap::new();
            for test in tests {
                by_file
                    .entry(test.file_path.as_str())
                    .or_default()
                    .push(test);
            }
            let mut files: Vec<_> = by_file.keys().copied().collect();
            files.sort();
            for file in files {
                println!("    {}", file.bold());
                let mut entities = by_file[file].clone();
                entities.sort_by_key(|test| test.start_line);
                for test in entities {
                    println!(
                        "      {} {} (L{}–{})",
                        test.entity_type.dimmed(),
                        test.name.bold(),
                        test.start_line,
                        test.end_line,
                    );
                }
            }
        }
        print_cached_tests_truncation_warning(truncated);
        println!();
    }
}

fn print_cached_tests_truncation_warning(truncated: bool) {
    if truncated {
        println!(
            "\n  {} {}",
            "warning:".yellow().bold(),
            "Cached test impact reached its traversal limit; results may be incomplete.".yellow()
        );
    }
}

fn print_cached_all(result: &CachedImpactResult, json: bool, depth: usize) {
    if json {
        let impact_entities: Vec<serde_json::Value> = result
            .impact
            .iter()
            .map(|(entity, depth)| {
                let mut value = entity_json(entity);
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("depth".to_string(), serde_json::json!(depth));
                value
            })
            .collect();
        let mut output = serde_json::json!({
            "entity": entity_json(&result.entity),
            "dependencies": owned_entity_list_json(&result.dependencies),
            "dependents": owned_entity_list_json(&result.dependents),
            "impact": {
                "depth": depth,
                "total": result.impact.len(),
                "entities": impact_entities,
            },
            "tests": owned_entity_list_json(&result.tests),
        });
        if result.tests_truncated {
            output
                .as_object_mut()
                .unwrap()
                .insert("testsTruncated".to_string(), serde_json::json!(true));
        }
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    print_entity_header(&result.entity);

    if !result.dependencies.is_empty() {
        println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
        for dep in &result.dependencies {
            println!(
                "    {} {} {} ({})",
                "→".blue(),
                dep.entity_type.dimmed(),
                dep.name.bold(),
                dep.file_path.dimmed(),
            );
        }
    }

    if !result.dependents.is_empty() {
        println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
        for dep in &result.dependents {
            println!(
                "    {} {} {} ({})",
                "←".yellow(),
                dep.entity_type.dimmed(),
                dep.name.bold(),
                dep.file_path.dimmed(),
            );
        }
    }

    if result.impact.is_empty() {
        println!(
            "\n  {} {}",
            "✓".green().bold(),
            "No other entities are affected by changes to this entity.".dimmed()
        );
    } else {
        let max_depth_seen = result
            .impact
            .iter()
            .map(|(_, depth)| *depth)
            .max()
            .unwrap_or(0);
        let depth_label = if depth == 0 {
            "unlimited".to_string()
        } else {
            format!("depth {}", depth)
        };
        println!(
            "\n  {} {}",
            "!".red().bold(),
            format!(
                "{} entities transitively affected ({}):",
                result.impact.len(),
                depth_label
            )
            .red(),
        );

        for current_depth in 1..=max_depth_seen {
            let at_depth: Vec<_> = result
                .impact
                .iter()
                .filter(|(_, depth)| *depth == current_depth)
                .map(|(entity, _)| entity)
                .collect();
            if at_depth.is_empty() {
                continue;
            }

            let label = if current_depth == 1 {
                "Direct dependents".to_string()
            } else {
                format!("Depth {}", current_depth)
            };
            println!("\n    {} ({})", label.bold(), at_depth.len());
            for entity in at_depth {
                println!(
                    "      {} {} {} ({}:L{})",
                    "→".red(),
                    entity.entity_type.dimmed(),
                    entity.name.bold(),
                    entity.file_path.dimmed(),
                    entity.start_line,
                );
            }
        }
    }

    if !result.tests.is_empty() {
        println!(
            "\n  {} {}",
            "⚡".yellow(),
            format!("{} tests affected:", result.tests.len()).bold()
        );
        for test in &result.tests {
            println!(
                "    {} {} ({})",
                test.entity_type.dimmed(),
                test.name.bold(),
                test.file_path.dimmed(),
            );
        }
    }
    print_cached_tests_truncation_warning(result.tests_truncated);

    println!();
}

fn print_cached_error(error: CachedImpactError) -> ! {
    match error {
        CachedImpactError::CacheReadFailed => {
            eprintln!(
                "{} Failed to read the cached impact graph",
                "error:".red().bold()
            );
        }
        CachedImpactError::MissingEntityQuery => {
            eprintln!(
                "{} Either entity name or --entity-id is required",
                "error:".red().bold()
            );
        }
        CachedImpactError::EntityIdNotFound(id) => {
            eprintln!("{} Entity ID '{}' not found", "error:".red().bold(), id);
        }
        CachedImpactError::EntityNotFound(name) => {
            eprintln!("{} Entity '{}' not found", "error:".red().bold(), name);
        }
        CachedImpactError::EntityNotFoundInFile { name, file } => {
            eprintln!(
                "{} Entity '{}' not found in file '{}'",
                "error:".red().bold(),
                name,
                file
            );
        }
        CachedImpactError::AmbiguousEntity { name, mut matches } => {
            matches.sort_by_key(|entity| {
                (
                    entity.file_path.clone(),
                    entity.start_line,
                    entity.id.clone(),
                )
            });
            eprintln!(
                "{} Entity name '{}' is ambiguous ({} matches). Specify --file or --entity-id:",
                "error:".red().bold(),
                name,
                matches.len()
            );
            for entity in &matches {
                eprintln!(
                    "  {} {} ({}:L{})",
                    entity.entity_type, entity.id, entity.file_path, entity.start_line
                );
            }
        }
    }
    std::process::exit(1);
}

fn print_deps(graph: &EntityGraph, entity: &sem_core::parser::graph::EntityInfo, json: bool) {
    let deps = graph.get_dependencies(&entity.id);

    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependencies": entity_list_json(&deps),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if deps.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No dependencies.".dimmed());
        } else {
            println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
            for dep in &deps {
                println!(
                    "    {} {} {} ({})",
                    "→".blue(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
        println!();
    }
}

fn print_dependents(graph: &EntityGraph, entity: &sem_core::parser::graph::EntityInfo, json: bool) {
    let dependents = graph.get_dependents(&entity.id);

    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependents": entity_list_json(&dependents),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if dependents.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No dependents.".dimmed());
        } else {
            println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
            for dep in &dependents {
                println!(
                    "    {} {} {} ({})",
                    "←".yellow(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
        println!();
    }
}

fn print_tests(
    graph: &EntityGraph,
    entity: &EntityInfo,
    all_entities: &[sem_core::model::entity::SemanticEntity],
    json: bool,
    custom_test_dirs: &[String],
) {
    let tests = graph.test_impact_with_custom_dirs(&entity.id, all_entities, custom_test_dirs);
    print_tests_result(entity, &tests, json);
}

fn print_tests_with_ids(
    graph: &EntityGraph,
    entity: &EntityInfo,
    test_entity_ids: &HashSet<String>,
    json: bool,
) {
    let tests = test_impact_from_ids(graph, &entity.id, test_entity_ids);
    print_tests_result(entity, &tests, json);
}

fn print_tests_result(entity: &EntityInfo, tests: &[&EntityInfo], json: bool) {
    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "tests": entity_list_json(tests),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if tests.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No tests found.".dimmed());
        } else {
            println!(
                "\n  {} {}",
                "⚡".yellow(),
                format!("{} tests affected:", tests.len()).bold()
            );
            let mut by_file: std::collections::HashMap<&str, Vec<_>> =
                std::collections::HashMap::new();
            for t in tests {
                by_file.entry(t.file_path.as_str()).or_default().push(t);
            }
            let mut files: Vec<_> = by_file.keys().copied().collect();
            files.sort();
            for file in files {
                println!("    {}", file.bold());
                let mut entities = by_file[file].clone();
                entities.sort_by_key(|e| e.start_line);
                for t in entities {
                    println!(
                        "      {} {} (L{}–{})",
                        t.entity_type.dimmed(),
                        t.name.bold(),
                        t.start_line,
                        t.end_line,
                    );
                }
            }
        }
        println!();
    }
}

fn print_all(
    graph: &EntityGraph,
    entity: &EntityInfo,
    all_entities: &[sem_core::model::entity::SemanticEntity],
    json: bool,
    depth: usize,
    custom_test_dirs: &[String],
) {
    let tests = graph.test_impact_with_custom_dirs(&entity.id, all_entities, custom_test_dirs);
    print_all_with_tests(graph, entity, &tests, json, depth);
}

fn print_all_with_ids(
    graph: &EntityGraph,
    entity: &EntityInfo,
    test_entity_ids: &HashSet<String>,
    json: bool,
    depth: usize,
) {
    let tests = test_impact_from_ids(graph, &entity.id, test_entity_ids);
    print_all_with_tests(graph, entity, &tests, json, depth);
}

fn test_impact_from_ids<'a>(
    graph: &'a EntityGraph,
    entity_id: &str,
    test_entity_ids: &HashSet<String>,
) -> Vec<&'a EntityInfo> {
    graph
        .impact_analysis(entity_id)
        .into_iter()
        .filter(|info| test_entity_ids.contains(&info.id))
        .collect()
}

fn print_all_with_tests(
    graph: &EntityGraph,
    entity: &EntityInfo,
    tests: &[&EntityInfo],
    json: bool,
    depth: usize,
) {
    let deps = graph.get_dependencies(&entity.id);
    let dependents = graph.get_dependents(&entity.id);
    let impact_bounded = graph.impact_analysis_bounded(&entity.id, depth);

    if json {
        let impact_entities: Vec<serde_json::Value> = impact_bounded
            .iter()
            .map(|(e, d)| {
                let mut v = entity_json(e);
                v.as_object_mut()
                    .unwrap()
                    .insert("depth".to_string(), serde_json::json!(d));
                v
            })
            .collect();
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependencies": entity_list_json(&deps),
            "dependents": entity_list_json(&dependents),
            "impact": {
                "depth": depth,
                "total": impact_bounded.len(),
                "entities": impact_entities,
            },
            "tests": entity_list_json(tests),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);

        // Dependencies
        if !deps.is_empty() {
            println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
            for dep in &deps {
                println!(
                    "    {} {} {} ({})",
                    "→".blue(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }

        // Dependents
        if !dependents.is_empty() {
            println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
            for dep in &dependents {
                println!(
                    "    {} {} {} ({})",
                    "←".yellow(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }

        // Transitive impact grouped by depth
        if impact_bounded.is_empty() {
            println!(
                "\n  {} {}",
                "✓".green().bold(),
                "No other entities are affected by changes to this entity.".dimmed()
            );
        } else {
            let max_depth_seen = impact_bounded.iter().map(|(_, d)| *d).max().unwrap_or(0);
            let depth_label = if depth == 0 {
                "unlimited".to_string()
            } else {
                format!("depth {}", depth)
            };
            println!(
                "\n  {} {}",
                "!".red().bold(),
                format!(
                    "{} entities transitively affected ({}):",
                    impact_bounded.len(),
                    depth_label
                )
                .red(),
            );

            for d in 1..=max_depth_seen {
                let at_depth: Vec<_> = impact_bounded
                    .iter()
                    .filter(|(_, dd)| *dd == d)
                    .map(|(e, _)| *e)
                    .collect();
                if at_depth.is_empty() {
                    continue;
                }

                let label = if d == 1 {
                    "Direct dependents".to_string()
                } else {
                    format!("Depth {}", d)
                };
                println!("\n    {} ({})", label.bold(), at_depth.len());
                for imp in &at_depth {
                    println!(
                        "      {} {} {} ({}:L{})",
                        "→".red(),
                        imp.entity_type.dimmed(),
                        imp.name.bold(),
                        imp.file_path.dimmed(),
                        imp.start_line,
                    );
                }
            }
        }

        // Tests
        if !tests.is_empty() {
            println!(
                "\n  {} {}",
                "⚡".yellow(),
                format!("{} tests affected:", tests.len()).bold()
            );
            for t in tests {
                println!(
                    "    {} {} ({})",
                    t.entity_type.dimmed(),
                    t.name.bold(),
                    t.file_path.dimmed(),
                );
            }
        }

        println!();
    }
}
