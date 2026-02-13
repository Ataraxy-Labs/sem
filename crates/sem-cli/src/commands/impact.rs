use std::path::Path;

use colored::Colorize;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;

pub struct ImpactOptions {
    pub cwd: String,
    pub entity_name: String,
    pub file_paths: Vec<String>,
    pub json: bool,
    pub file_exts: Vec<String>,
}

pub fn impact_command(opts: ImpactOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    let ext_filter = super::graph::normalize_exts(&opts.file_exts);

    // If no files specified, find all supported files in the repo
    let file_paths = if opts.file_paths.is_empty() {
        super::graph::find_supported_files_public(root, &registry, &ext_filter)
    } else if ext_filter.is_empty() {
        opts.file_paths
    } else {
        opts.file_paths.into_iter().filter(|f| ext_filter.iter().any(|ext| f.ends_with(ext.as_str()))).collect()
    };

    let graph = EntityGraph::build(root, &file_paths, &registry);

    // Find entity by name
    let matching: Vec<_> = graph
        .entities
        .values()
        .filter(|e| e.name == opts.entity_name)
        .collect();

    if matching.is_empty() {
        eprintln!(
            "{} Entity '{}' not found",
            "error:".red().bold(),
            opts.entity_name
        );
        std::process::exit(1);
    }

    for entity in &matching {
        let impact = graph.impact_analysis(&entity.id);
        let deps = graph.get_dependencies(&entity.id);

        if opts.json {
            let output = serde_json::json!({
                "entity": {
                    "name": entity.name,
                    "type": entity.entity_type,
                    "file": entity.file_path,
                    "lines": [entity.start_line, entity.end_line],
                },
                "dependencies": deps.iter().map(|d| serde_json::json!({
                    "name": d.name, "type": d.entity_type,
                    "file": d.file_path, "lines": [d.start_line, d.end_line],
                })).collect::<Vec<_>>(),
                "impact": {
                    "total": impact.len(),
                    "entities": impact.iter().map(|d| serde_json::json!({
                        "name": d.name, "type": d.entity_type,
                        "file": d.file_path, "lines": [d.start_line, d.end_line],
                    })).collect::<Vec<_>>(),
                },
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        } else {
            println!(
                "{} {} {} ({}:{}–{})",
                "⊕".green(),
                entity.entity_type.dimmed(),
                entity.name.bold(),
                entity.file_path.dimmed(),
                entity.start_line,
                entity.end_line,
            );

            if !deps.is_empty() {
                println!(
                    "\n  {} {}",
                    "→".blue(),
                    "depends on:".dimmed()
                );
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

            if impact.is_empty() {
                println!(
                    "\n  {} {}",
                    "✓".green().bold(),
                    "No other entities are affected by changes to this entity."
                        .dimmed()
                );
            } else {
                println!(
                    "\n  {} {} {}",
                    "!".red().bold(),
                    format!("{} entities transitively affected:", impact.len())
                        .red(),
                    "".dimmed()
                );
                // Group by file
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
            println!();
        }
    }
}
