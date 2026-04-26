use std::path::Path;

use colored::Colorize;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::verify::{find_arity_mismatches, find_broken_callers};

pub struct VerifyOptions {
    pub cwd: String,
    pub json: bool,
    pub diff: bool,
    pub file_exts: Vec<String>,
}

pub fn verify_command(opts: VerifyOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let file_paths = super::graph::find_supported_files_public(root, &registry, &ext_filter);
    let (graph, all_entities) =
        super::graph::get_or_build_graph(root, &file_paths, &registry, false);

    if opts.diff {
        verify_diff(root, &graph, &all_entities, &registry, &ext_filter, opts.json);
    } else {
        verify_full(&graph, &all_entities, opts.json);
    }
}

fn verify_full(
    graph: &sem_core::parser::graph::EntityGraph,
    all_entities: &[sem_core::model::entity::SemanticEntity],
    json: bool,
) {
    let mismatches = find_arity_mismatches(graph, all_entities);

    if json {
        let items: Vec<serde_json::Value> = mismatches
            .iter()
            .map(|m| {
                serde_json::json!({
                    "caller": m.caller_entity,
                    "callee": m.callee_entity,
                    "expected_min": m.expected_min,
                    "expected_max": m.expected_max,
                    "actual_args": m.actual_args,
                    "file": m.file_path,
                    "line": m.line,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap_or_default());
    } else if mismatches.is_empty() {
        println!("{} No arity mismatches found", "ok:".green().bold());
    } else {
        println!(
            "{} {} arity mismatch{} found\n",
            "warning:".yellow().bold(),
            mismatches.len(),
            if mismatches.len() == 1 { "" } else { "es" }
        );
        for m in &mismatches {
            let expected = if m.expected_min == m.expected_max {
                format!("{}", m.expected_min)
            } else {
                format!("{}-{}", m.expected_min, m.expected_max)
            };
            println!(
                "  {} {}:{} {} calls {}({} args) but {} expects {} params",
                "x".red(),
                m.file_path.dimmed(),
                m.line,
                m.caller_entity.bold(),
                m.callee_entity.cyan(),
                m.actual_args,
                m.callee_entity.cyan(),
                expected,
            );
        }
    }

    if !mismatches.is_empty() {
        std::process::exit(1);
    }
}

fn verify_diff(
    root: &Path,
    new_graph: &sem_core::parser::graph::EntityGraph,
    new_entities: &[sem_core::model::entity::SemanticEntity],
    registry: &sem_core::parser::registry::ParserRegistry,
    ext_filter: &[String],
    json: bool,
) {
    // Get HEAD entities for comparison
    let old_entities = match get_head_entities(root, registry, ext_filter) {
        Some(entities) => entities,
        None => {
            if json {
                println!("[]");
            } else {
                println!(
                    "{} Could not read HEAD for comparison (not a git repo or no commits)",
                    "note:".dimmed()
                );
            }
            return;
        }
    };

    let broken = find_broken_callers(&old_entities, new_graph, new_entities);

    if json {
        let items: Vec<serde_json::Value> = broken
            .iter()
            .map(|m| {
                serde_json::json!({
                    "caller": m.caller_entity,
                    "callee": m.callee_entity,
                    "expected_min": m.expected_min,
                    "expected_max": m.expected_max,
                    "actual_args": m.actual_args,
                    "file": m.file_path,
                    "line": m.line,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap_or_default());
    } else if broken.is_empty() {
        println!(
            "{} No broken callers from signature changes",
            "ok:".green().bold()
        );
    } else {
        println!(
            "{} {} broken caller{} from signature changes\n",
            "warning:".yellow().bold(),
            broken.len(),
            if broken.len() == 1 { "" } else { "s" }
        );
        for m in &broken {
            let expected = if m.expected_min == m.expected_max {
                format!("{}", m.expected_min)
            } else {
                format!("{}-{}", m.expected_min, m.expected_max)
            };
            println!(
                "  {} {}:{} {} calls {}({} args) but signature now expects {} params",
                "x".red(),
                m.file_path.dimmed(),
                m.line,
                m.caller_entity.bold(),
                m.callee_entity.cyan(),
                m.actual_args,
                expected,
            );
        }
    }

    if !broken.is_empty() {
        std::process::exit(1);
    }
}

/// Extract entities from HEAD using git show.
fn get_head_entities(
    root: &Path,
    registry: &sem_core::parser::registry::ParserRegistry,
    ext_filter: &[String],
) -> Option<Vec<sem_core::model::entity::SemanticEntity>> {
    let file_paths = super::graph::find_supported_files_public(root, registry, ext_filter);
    let mut all_entities = Vec::new();

    for fp in &file_paths {
        // Read file content from HEAD
        let output = std::process::Command::new("git")
            .args(["show", &format!("HEAD:{}", fp)])
            .current_dir(root)
            .output()
            .ok()?;

        if !output.status.success() {
            continue;
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        if let Some(plugin) = registry.get_plugin_with_content(fp, &content) {
            let entities = plugin.extract_entities(&content, fp);
            all_entities.extend(entities);
        }
    }

    if all_entities.is_empty() {
        None
    } else {
        Some(all_entities)
    }
}
