use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::parser::graph::EntityGraph;

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

pub enum ImpactMode {
    All,
    Deps,
    Dependents,
    Tests,
}

pub fn impact_command(opts: ImpactOptions) {
    let mut timings = Timings::from_env("impact");
    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());

    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let file_paths = super::graph::find_supported_files_with_options(
        root,
        &registry,
        &ext_filter,
        opts.no_default_excludes,
    );
    timings.mark("file_discovery");

    let file_hint = opts
        .file_hint
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), root, file));

    match opts.mode {
        ImpactMode::Deps | ImpactMode::Dependents => {
            let graph = super::graph::get_or_build_graph_topology_with_timings(
                root,
                &file_paths,
                &registry,
                opts.no_cache,
                &mut timings,
            );
            let entity = find_entity(
                &graph,
                opts.entity_name.as_deref(),
                opts.entity_id.as_deref(),
                file_hint.as_deref(),
            );
            timings.mark("entity_lookup");
            match opts.mode {
                ImpactMode::Deps => print_deps(&graph, entity, opts.json),
                ImpactMode::Dependents => print_dependents(&graph, entity, opts.json),
                _ => unreachable!(),
            }
            timings.mark("cli_output_serialization");
        }
        ImpactMode::Tests | ImpactMode::All => {
            let (graph, all_entities) = super::graph::get_or_build_graph_with_timings(
                root,
                &file_paths,
                &registry,
                opts.no_cache,
                &mut timings,
            );
            let entity = find_entity(
                &graph,
                opts.entity_name.as_deref(),
                opts.entity_id.as_deref(),
                file_hint.as_deref(),
            );
            timings.mark("entity_lookup");
            match opts.mode {
                ImpactMode::Tests => print_tests(&graph, entity, &all_entities, opts.json),
                ImpactMode::All => print_all(&graph, entity, &all_entities, opts.json, opts.depth),
                _ => unreachable!(),
            }
            timings.mark("cli_output_serialization");
        }
    }
    timings.finish();
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
        .filter(|e| super::entity_matches_query(e, name))
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
    entity: &sem_core::parser::graph::EntityInfo,
    all_entities: &[sem_core::model::entity::SemanticEntity],
    json: bool,
) {
    let tests = graph.test_impact(&entity.id, all_entities);

    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "tests": entity_list_json(&tests),
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
            for t in &tests {
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
    entity: &sem_core::parser::graph::EntityInfo,
    all_entities: &[sem_core::model::entity::SemanticEntity],
    json: bool,
    depth: usize,
) {
    let deps = graph.get_dependencies(&entity.id);
    let dependents = graph.get_dependents(&entity.id);
    let impact_bounded = graph.impact_analysis_bounded(&entity.id, depth);
    let tests = graph.test_impact(&entity.id, all_entities);

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
            "tests": entity_list_json(&tests),
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
            for t in &tests {
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
