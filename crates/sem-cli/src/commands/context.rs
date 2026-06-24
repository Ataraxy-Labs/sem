use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::parser::context::build_context_result_bounded;
use sem_core::parser::graph::EntityGraph;

pub struct ContextOptions {
    pub cwd: String,
    pub entity_name: Option<String>,
    pub entity_id: Option<String>,
    pub file_path: Option<String>,
    pub budget: usize,
    /// Bound transitive related entities to this many graph hops (0 = unbounded).
    pub hops: usize,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

pub fn context_command(opts: ContextOptions) {
    if super::cloud::try_cloud_context(&opts).is_some() {
        return;
    }

    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(root, &ext_filter, opts.no_default_excludes);

    let file_paths = super::graph::find_supported_files_with_options(
        root,
        &registry,
        &ext_filter,
        opts.no_default_excludes,
    );
    let prog = crate::progress::Progress::start("Building entity graph");
    let (graph, all_entities) =
        super::graph::get_or_build_graph(root, &file_paths, &registry, opts.no_cache, source_scope);
    prog.done(&format!(
        "{} entities, {} files",
        super::graph::fmt_count(graph.entities.len()),
        super::graph::fmt_count(file_paths.len())
    ));

    let file_path = opts
        .file_path
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), root, file));
    let entity = find_entity(
        &graph,
        opts.entity_name.as_deref(),
        opts.entity_id.as_deref(),
        file_path.as_deref(),
    );
    let context_result =
        build_context_result_bounded(&graph, &entity.id, &all_entities, opts.budget, opts.hops);

    if opts.json {
        let output = serde_json::json!({
            "entity": entity.name,
            "entityId": entity.id,
            "budget": opts.budget,
            "total_tokens": context_result.total_tokens,
            "truncated": context_result.truncated,
            "target_omitted": context_result.target_omitted,
            "entries": context_result.entries.iter().map(|e| serde_json::json!({
                "entityId": e.entity_id,
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
            context_result.total_tokens,
        );

        if context_result.target_omitted {
            println!(
                "  {}",
                "target omitted: signature exceeds token budget".dimmed()
            );
        }

        let mut current_role = String::new();
        for entry in &context_result.entries {
            if entry.role != current_role {
                current_role = entry.role.clone();
                let role_label = match current_role.as_str() {
                    "target" => "target".green().bold(),
                    "direct_dependency" => "direct dependencies".cyan().bold(),
                    "direct_dependent" => "direct dependents".yellow().bold(),
                    "transitive_dependency" => "transitive dependencies".blue().bold(),
                    "transitive_dependent" => "transitive dependents".dimmed().bold(),
                    _ => current_role.normal().bold(),
                };
                println!("  {}:", role_label);
            }

            println!(
                "    {} {} ({}, ~{} tokens)",
                entry.entity_type.dimmed(),
                entry.entity_name.bold(),
                entry.file_path.dimmed(),
                entry.estimated_tokens,
            );
            // The target is what you asked to read: print its full body. Related
            // entities stay a one-line signature so the context map stays scannable.
            if entry.role == "target" {
                for line in entry.content.lines() {
                    println!("      {line}");
                }
            } else {
                let snippet = entry.content.lines().next().unwrap_or("");
                if !snippet.is_empty() {
                    println!("      {}", snippet.dimmed());
                }
            }
        }
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
        matching = filtered;
    }

    if matching.len() == 1 {
        return matching[0];
    }

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
