use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use colored::Colorize;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::registry::ParserRegistry;

pub struct EntitiesOptions {
    pub cwd: String,
    pub path: Option<String>,
    pub json: bool,
    pub no_default_excludes: bool,
}

pub fn entities_command(opts: EntitiesOptions) {
    let root = Path::new(&opts.cwd);
    let registry = super::create_registry(&opts.cwd);
    let path_arg = opts.path.as_deref().filter(|p| !p.is_empty()).unwrap_or(".");
    let (path_label, full_path) = resolve_path(root, path_arg);

    let (entities, include_file) = if full_path.is_file() {
        (
            extract_file_entities(&full_path, &registry, &path_label).unwrap_or_else(|e| {
                eprintln!(
                    "{} Cannot read '{}': {}",
                    "error:".red().bold(),
                    path_label,
                    e
                );
                std::process::exit(1);
            }),
            false,
        )
    } else if full_path.is_dir() {
        let file_paths = super::files::find_supported_files_in_path(
            root,
            &full_path,
            &registry,
            &[],
            opts.no_default_excludes,
        );
        (extract_files_entities(root, &file_paths, &registry), true)
    } else {
        eprintln!("{} Path not found '{}'", "error:".red().bold(), path_arg);
        std::process::exit(1);
    };

    if opts.json {
        let output: Vec<_> = entities
            .iter()
            .map(|e| entity_json(e, include_file))
            .collect();
        println!("{}", serde_json::to_string(&output).unwrap());
    } else if should_group_by_file(&entities) {
        print_grouped_entities(&path_label, &entities);
    } else if let Some(file_path) = entities.first().map(|e| e.file_path.as_str()) {
        print_file_entities(file_path, &entities);
    } else {
        println!("{} {}\n", "entities:".green().bold(), path_label.bold());
    }
}

fn resolve_path(root: &Path, path_arg: &str) -> (String, PathBuf) {
    let path = Path::new(path_arg);
    let full_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    let label = if path.is_absolute() {
        file_path_for_entity(root, &full_path)
    } else {
        path_arg.to_string()
    };

    (label, full_path)
}

fn extract_files_entities(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
) -> Vec<SemanticEntity> {
    let mut entities = registry.extract_all_entities(root, file_paths);
    entities.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.entity_type.cmp(&b.entity_type))
            .then(a.name.cmp(&b.name))
    });
    entities
}

fn file_path_for_entity(root: &Path, path: &Path) -> String {
    super::files::file_path_for_entity(root, path)
}

fn extract_file_entities(
    full_path: &Path,
    registry: &ParserRegistry,
    file_path: &str,
) -> Result<Vec<SemanticEntity>, std::io::Error> {
    let content = std::fs::read_to_string(&full_path)?;
    Ok(registry.extract_entities(file_path, &content))
}

fn entity_json(entity: &SemanticEntity, include_file: bool) -> serde_json::Value {
    let mut value = serde_json::json!({
        "name": entity.name,
        "type": entity.entity_type,
        "start_line": entity.start_line,
        "end_line": entity.end_line,
        "parent_id": entity.parent_id,
    });

    if include_file {
        value["file"] = serde_json::json!(entity.file_path);
    }

    value
}

fn print_file_entities(file_path: &str, entities: &[SemanticEntity]) {
    println!("{} {}\n", "entities:".green().bold(), file_path.bold());
    print_entity_rows(entities, "  ");
}

fn should_group_by_file(entities: &[SemanticEntity]) -> bool {
    let files: BTreeSet<&str> = entities.iter().map(|e| e.file_path.as_str()).collect();
    files.len() > 1
}

fn print_grouped_entities(path_label: &str, entities: &[SemanticEntity]) {
    println!("{} {}\n", "entities:".green().bold(), path_label.bold());

    let mut current_file: Option<&str> = None;
    for entity in entities {
        if current_file != Some(entity.file_path.as_str()) {
            current_file = Some(entity.file_path.as_str());
            println!("  {}", entity.file_path.bold());
        }

        let indent = if entity.parent_id.is_some() {
            "      "
        } else {
            "    "
        };
        print_entity_row(entity, indent);
    }
}

fn print_entity_rows(entities: &[SemanticEntity], base_indent: &str) {
    for entity in entities {
        let indent = if entity.parent_id.is_some() {
            format!("{base_indent}  ")
        } else {
            base_indent.to_string()
        };
        print_entity_row(entity, &indent);
    }
}

fn print_entity_row(entity: &SemanticEntity, indent: &str) {
    println!(
        "{}{} {} (L{}:{})",
        indent,
        entity.entity_type.dimmed(),
        entity.name.bold(),
        entity.start_line,
        entity.end_line,
    );
}
