use std::path::Path;

use colored::Colorize;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;

pub struct ImpactOptions {
    pub cwd: String,
    pub entity_name: String,
    pub file_hint: Option<String>,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub mode: ImpactMode,
    pub no_cache: bool,
}

pub enum ImpactMode {
    All,
    Deps,
    Dependents,
    Tests,
}

pub fn impact_command(opts: ImpactOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let file_paths = super::graph::find_supported_files_public(root, &registry, &ext_filter);
    let (graph, all_entities) = super::graph::get_or_build_graph(root, &file_paths, &registry, opts.no_cache);

    let entity = find_entity(&graph, &opts.entity_name, opts.file_hint.as_deref());

    match opts.mode {
        ImpactMode::Deps => print_deps(&graph, entity, opts.json),
        ImpactMode::Dependents => print_dependents(&graph, entity, opts.json),
        ImpactMode::Tests => print_tests(&graph, entity, &all_entities, opts.json),
        ImpactMode::All => print_all(&graph, entity, &all_entities, opts.json),
    }
}

fn find_entity<'a>(
    graph: &'a EntityGraph,
    name: &str,
    file_hint: Option<&str>,
) -> &'a sem_core::parser::graph::EntityInfo {
    let mut matching: Vec<_> = graph.entities.values().filter(|e| e.name == name).collect();

    if matching.is_empty() {
        eprintln!("{} Entity '{}' not found", "error:".red().bold(), name);
        std::process::exit(1);
    }

    if let Some(file) = file_hint {
        if let Some(e) = matching.iter().find(|e| e.file_path == file) {
            return e;
        }
    }

    matching.sort_by_key(|e| (&e.file_path, e.start_line));
    matching[0]
}

fn entity_json(e: &sem_core::parser::graph::EntityInfo) -> serde_json::Value {
    serde_json::json!({
        "name": e.name, "type": e.entity_type,
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
) {
    let deps = graph.get_dependencies(&entity.id);
    let dependents = graph.get_dependents(&entity.id);
    let impact = graph.impact_analysis(&entity.id);
    let tests = graph.test_impact(&entity.id, all_entities);

    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependencies": entity_list_json(&deps),
            "dependents": entity_list_json(&dependents),
            "impact": {
                "total": impact.len(),
                "entities": entity_list_json(&impact),
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

        // Transitive impact
        if impact.is_empty() {
            println!(
                "\n  {} {}",
                "✓".green().bold(),
                "No other entities are affected by changes to this entity."
                    .dimmed()
            );
        } else {
            println!(
                "\n  {} {}",
                "!".red().bold(),
                format!("{} entities transitively affected:", impact.len()).red(),
            );
            let mut by_file: std::collections::HashMap<&str, Vec<_>> =
                std::collections::HashMap::new();
            for imp in &impact {
                by_file
                    .entry(imp.file_path.as_str())
                    .or_default()
                    .push(imp);
            }
            let mut files: Vec<_> = by_file.keys().copied().collect();
            files.sort();
            for file in files {
                println!("    {}", file.bold());
                let mut entities = by_file[file].clone();
                entities.sort_by_key(|e| e.start_line);
                for imp in entities {
                    println!(
                        "      {} {} {} (L{}–{})",
                        "!".red(),
                        imp.entity_type.dimmed(),
                        imp.name.bold(),
                        imp.start_line,
                        imp.end_line,
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
