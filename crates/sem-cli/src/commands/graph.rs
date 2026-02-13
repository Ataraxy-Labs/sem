use std::path::Path;

use colored::Colorize;
use sem_core::parser::graph::{EntityGraph, RefType};
use sem_core::parser::plugins::create_default_registry;

pub struct GraphOptions {
    pub cwd: String,
    pub file_paths: Vec<String>,
    pub entity: Option<String>,
    pub format: GraphFormat,
    pub file_exts: Vec<String>,
}

pub enum GraphFormat {
    Terminal,
    Json,
}

pub fn graph_command(opts: GraphOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    let ext_filter = normalize_exts(&opts.file_exts);

    // If no files specified, find all supported files in the repo
    let file_paths = if opts.file_paths.is_empty() {
        find_supported_files(root, &registry, &ext_filter)
    } else if ext_filter.is_empty() {
        opts.file_paths
    } else {
        opts.file_paths.into_iter().filter(|f| ext_filter.iter().any(|ext| f.ends_with(ext.as_str()))).collect()
    };

    let graph = EntityGraph::build(root, &file_paths, &registry);

    match opts.format {
        GraphFormat::Json => print_json(&graph, opts.entity.as_deref()),
        GraphFormat::Terminal => print_terminal(&graph, opts.entity.as_deref()),
    }
}

fn print_terminal(graph: &EntityGraph, entity_filter: Option<&str>) {
    if let Some(entity_name) = entity_filter {
        // Find entity by name
        let matching: Vec<_> = graph
            .entities
            .values()
            .filter(|e| e.name == entity_name)
            .collect();

        if matching.is_empty() {
            eprintln!("{} Entity '{}' not found", "error:".red().bold(), entity_name);
            return;
        }

        for entity in &matching {
            println!(
                "{} {} {} ({}:{}–{})",
                "⊕".green(),
                entity.entity_type.dimmed(),
                entity.name.bold(),
                entity.file_path.dimmed(),
                entity.start_line,
                entity.end_line,
            );

            // Dependencies (what it calls)
            let deps = graph.get_dependencies(&entity.id);
            if !deps.is_empty() {
                println!("  {} {}", "→".blue(), "depends on:".dimmed());
                for dep in &deps {
                    println!(
                        "    {} {} {}",
                        ref_symbol(&RefType::Calls),
                        dep.entity_type.dimmed(),
                        dep.name.bold()
                    );
                }
            }

            // Dependents (who calls it)
            let dependents = graph.get_dependents(&entity.id);
            if !dependents.is_empty() {
                println!("  {} {}", "←".yellow(), "depended on by:".dimmed());
                for dep in &dependents {
                    println!(
                        "    {} {} {}",
                        "←".yellow(),
                        dep.entity_type.dimmed(),
                        dep.name.bold()
                    );
                }
            }

            // Impact analysis
            let impact = graph.impact_analysis(&entity.id);
            if !impact.is_empty() {
                println!(
                    "  {} {} {}",
                    "!".red().bold(),
                    "impact:".red(),
                    format!("{} entities transitively affected", impact.len()).dimmed()
                );
                for imp in &impact {
                    println!(
                        "    {} {} {} ({})",
                        "!".red(),
                        imp.entity_type.dimmed(),
                        imp.name.bold(),
                        imp.file_path.dimmed()
                    );
                }
            }

            println!();
        }
    } else {
        // Print full graph summary
        println!(
            "{} {} entities, {} references\n",
            "graph:".green().bold(),
            graph.entities.len(),
            graph.edges.len(),
        );

        // Group by file
        let mut by_file: std::collections::HashMap<&str, Vec<_>> = std::collections::HashMap::new();
        for entity in graph.entities.values() {
            by_file
                .entry(entity.file_path.as_str())
                .or_default()
                .push(entity);
        }

        let mut files: Vec<_> = by_file.keys().copied().collect();
        files.sort();

        for file in files {
            println!("  {}", file.bold());
            let mut entities = by_file[file].clone();
            entities.sort_by_key(|e| e.start_line);

            for entity in entities {
                let dep_count = graph
                    .dependencies
                    .get(&entity.id)
                    .map(|d| d.len())
                    .unwrap_or(0);
                let dependent_count = graph
                    .dependents
                    .get(&entity.id)
                    .map(|d| d.len())
                    .unwrap_or(0);

                let refs_str = if dep_count > 0 || dependent_count > 0 {
                    format!(" (→{} ←{})", dep_count, dependent_count)
                        .dimmed()
                        .to_string()
                } else {
                    String::new()
                };

                println!(
                    "    {} {} {}{}",
                    entity.entity_type.dimmed(),
                    entity.name.bold(),
                    format!("L{}-{}", entity.start_line, entity.end_line).dimmed(),
                    refs_str,
                );
            }
            println!();
        }
    }
}

fn print_json(graph: &EntityGraph, entity_filter: Option<&str>) {
    let output = if let Some(entity_name) = entity_filter {
        let matching: Vec<_> = graph
            .entities
            .values()
            .filter(|e| e.name == entity_name)
            .collect();

        let results: Vec<_> = matching
            .iter()
            .map(|entity| {
                let deps = graph.get_dependencies(&entity.id);
                let dependents = graph.get_dependents(&entity.id);
                let impact = graph.impact_analysis(&entity.id);

                serde_json::json!({
                    "id": entity.id,
                    "name": entity.name,
                    "type": entity.entity_type,
                    "file": entity.file_path,
                    "lines": [entity.start_line, entity.end_line],
                    "dependencies": deps.iter().map(|d| serde_json::json!({
                        "id": d.id, "name": d.name, "type": d.entity_type, "file": d.file_path
                    })).collect::<Vec<_>>(),
                    "dependents": dependents.iter().map(|d| serde_json::json!({
                        "id": d.id, "name": d.name, "type": d.entity_type, "file": d.file_path
                    })).collect::<Vec<_>>(),
                    "impact": impact.iter().map(|d| serde_json::json!({
                        "id": d.id, "name": d.name, "type": d.entity_type, "file": d.file_path
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();

        serde_json::json!({ "results": results })
    } else {
        serde_json::json!({
            "entities": graph.entities.len(),
            "edges": graph.edges.len(),
            "graph": graph.edges.iter().map(|e| serde_json::json!({
                "from": e.from_entity,
                "to": e.to_entity,
                "type": format!("{:?}", e.ref_type),
            })).collect::<Vec<_>>(),
        })
    };

    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}

fn ref_symbol(ref_type: &RefType) -> colored::ColoredString {
    match ref_type {
        RefType::Calls => "→".blue(),
        RefType::TypeRef => "⊳".cyan(),
        RefType::Imports => "↓".green(),
    }
}

/// Normalize extension strings: ensure each starts with '.'
pub fn normalize_exts(exts: &[String]) -> Vec<String> {
    exts.iter().map(|e| {
        if e.starts_with('.') { e.clone() } else { format!(".{}", e) }
    }).collect()
}

/// Find all supported files in the repo (public for use by other commands).
pub fn find_supported_files_public(root: &Path, registry: &sem_core::parser::registry::ParserRegistry, ext_filter: &[String]) -> Vec<String> {
    find_supported_files(root, registry, ext_filter)
}

fn find_supported_files(root: &Path, registry: &sem_core::parser::registry::ParserRegistry, ext_filter: &[String]) -> Vec<String> {
    let mut files = Vec::new();
    walk_dir(root, root, registry, ext_filter, &mut files);
    files.sort();
    files
}

fn walk_dir(
    dir: &Path,
    root: &Path,
    registry: &sem_core::parser::registry::ParserRegistry,
    ext_filter: &[String],
    files: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Skip hidden dirs and common non-code dirs
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') || name == "node_modules" || name == "target" || name == "__pycache__" || name == "venv" {
                continue;
            }
        }

        if path.is_dir() {
            walk_dir(&path, root, registry, ext_filter, files);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().to_string();
            // If ext filter is set, only include matching extensions
            if !ext_filter.is_empty() && !ext_filter.iter().any(|ext| rel_str.ends_with(ext.as_str())) {
                continue;
            }
            if registry.get_plugin(&rel_str).is_some() {
                files.push(rel_str);
            }
        }
    }
}
