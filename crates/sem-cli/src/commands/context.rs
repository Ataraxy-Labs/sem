use std::path::Path;

use colored::Colorize;
use sem_core::parser::context::build_context;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;

pub struct ContextOptions {
    pub cwd: String,
    pub entity_name: String,
    pub file_path: Option<String>,
    pub budget: usize,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
}

pub fn context_command(opts: ContextOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);

    let file_paths = super::graph::find_supported_files_public(root, &registry, &ext_filter);
    let (graph, all_entities) = super::graph::get_or_build_graph(root, &file_paths, &registry, opts.no_cache);

    let entity = find_entity(&graph, &opts.entity_name, opts.file_path.as_deref());
    let entries = build_context(&graph, &entity.id, &all_entities, opts.budget);

    let total_tokens: usize = entries.iter().map(|e| e.estimated_tokens).sum();

    if opts.json {
        let output = serde_json::json!({
            "entity": opts.entity_name,
            "budget": opts.budget,
            "total_tokens": total_tokens,
            "entries": entries.iter().map(|e| serde_json::json!({
                "name": e.entity_name,
                "type": e.entity_type,
                "file": e.file_path,
                "role": e.role,
                "tokens": e.estimated_tokens,
                "content": e.content,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        println!(
            "{} {} {} (budget: {}, used: {})\n",
            "context for".green().bold(),
            entity.entity_type.dimmed(),
            entity.name.bold(),
            opts.budget,
            total_tokens,
        );

        let mut current_role = String::new();
        for entry in &entries {
            if entry.role != current_role {
                current_role = entry.role.clone();
                let role_label = match current_role.as_str() {
                    "target" => "target".green().bold(),
                    "direct_dependent" => "direct dependents".yellow().bold(),
                    "transitive_dependent" => "transitive dependents".dimmed().bold(),
                    _ => current_role.normal().bold(),
                };
                println!("  {}:", role_label);
            }

            let snippet: String = entry.content.lines().next().unwrap_or("").to_string();
            println!(
                "    {} {} ({}, ~{} tokens)",
                entry.entity_type.dimmed(),
                entry.entity_name.bold(),
                entry.file_path.dimmed(),
                entry.estimated_tokens,
            );
            if !snippet.is_empty() {
                println!("      {}", snippet.dimmed());
            }
        }
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
