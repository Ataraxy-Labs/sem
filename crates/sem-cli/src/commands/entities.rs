use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use colored::Colorize;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::registry::{resolve_go_method_parent_ids, ParserRegistry};

pub struct EntitiesOptions {
    pub cwd: String,
    pub paths: Vec<String>,
    pub json: bool,
    pub no_default_excludes: bool,
}

pub fn entities_command(opts: EntitiesOptions) {
    // Normalize to a non-empty list of path args, defaulting to ".".
    let path_args: Vec<String> = {
        let cleaned: Vec<String> = opts
            .paths
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if cleaned.is_empty() {
            vec![".".to_string()]
        } else {
            cleaned
        }
    };

    // The cloud fast-path only helps the whole-repo single listing.
    if path_args.len() == 1 && super::cloud::try_cloud_entities(&opts).is_some() {
        return;
    }

    let root = Path::new(&opts.cwd);
    let registry = super::create_registry(&opts.cwd);

    let mut entities: Vec<SemanticEntity> = Vec::new();
    let mut dir_count = 0usize;
    for path_arg in &path_args {
        let (path_label, full_path) = resolve_path(root, path_arg);
        if full_path.is_file() {
            let file_entities = extract_file_entities(&full_path, &registry, &path_label)
                .unwrap_or_else(|e| {
                    eprintln!(
                        "{} Cannot read '{}': {}",
                        "error:".red().bold(),
                        path_label,
                        e
                    );
                    std::process::exit(1);
                });
            entities.extend(file_entities);
        } else if full_path.is_dir() {
            dir_count += 1;
            let file_paths = super::files::find_supported_files_in_path(
                root,
                &full_path,
                &registry,
                &[],
                opts.no_default_excludes,
            );
            entities.extend(extract_files_entities(root, &file_paths, &registry));
        } else {
            eprintln!("{} Path not found '{}'", "error:".red().bold(), path_arg);
            std::process::exit(1);
        }
    }

    // Overlapping paths (e.g. a directory and a file inside it) can surface the
    // same entity twice; sort and drop exact duplicates by id.
    entities.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.entity_type.cmp(&b.entity_type))
            .then(a.name.cmp(&b.name))
    });
    entities.dedup_by(|a, b| a.id == b.id);

    // Show the file column whenever results span more than one file. This keeps
    // the prior single-file vs directory behavior and covers multi-path input.
    let distinct_files = {
        let mut files: Vec<&str> = entities.iter().map(|e| e.file_path.as_str()).collect();
        files.sort_unstable();
        files.dedup();
        files.len()
    };
    let include_file = dir_count > 0 || distinct_files > 1;
    let display_label = path_args.join(" ");

    if opts.json {
        let output: Vec<_> = entities
            .iter()
            .map(|e| entity_json(e, include_file))
            .collect();
        println!("{}", serde_json::to_string(&output).unwrap());
    } else if should_group_by_file(&entities) {
        print_grouped_entities(&display_label, &entities);
    } else if let Some(file_path) = entities.first().map(|e| e.file_path.as_str()) {
        print_file_entities(file_path, &entities);
    } else {
        println!("{} {}\n", "entities:".green().bold(), display_label.bold());
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
    resolve_go_method_parent_ids(&mut entities);
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
    let entities_by_id = entities_by_id(entities);
    for entity in entities {
        if current_file != Some(entity.file_path.as_str()) {
            current_file = Some(entity.file_path.as_str());
            println!("  {}", entity.file_path.bold());
        }

        let indent = entity_indent(entity, &entities_by_id, "    ");
        print_entity_row(entity, &indent);
    }
}

fn print_entity_rows(entities: &[SemanticEntity], base_indent: &str) {
    let entities_by_id = entities_by_id(entities);
    for entity in entities {
        let indent = entity_indent(entity, &entities_by_id, base_indent);
        print_entity_row(entity, &indent);
    }
}

fn entities_by_id(entities: &[SemanticEntity]) -> HashMap<&str, &SemanticEntity> {
    entities
        .iter()
        .map(|entity| (entity.id.as_str(), entity))
        .collect()
}

fn entity_indent(
    entity: &SemanticEntity,
    entities_by_id: &HashMap<&str, &SemanticEntity>,
    base_indent: &str,
) -> String {
    format!(
        "{base_indent}{}",
        "  ".repeat(entity_depth(entity, entities_by_id))
    )
}

fn entity_depth(entity: &SemanticEntity, entities_by_id: &HashMap<&str, &SemanticEntity>) -> usize {
    let mut depth = 0;
    let mut current_parent = entity.parent_id.as_deref();
    let mut seen = HashSet::new();

    while let Some(parent_id) = current_parent {
        if !seen.insert(parent_id) {
            break;
        }
        depth += 1;
        current_parent = entities_by_id
            .get(parent_id)
            .and_then(|parent| parent.parent_id.as_deref());
    }

    depth
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: &str, parent_id: Option<&str>) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "field".to_string(),
            name: id.rsplit("::").next().unwrap_or(id).to_string(),
            parent_id: parent_id.map(String::from),
            content: String::new(),
            content_hash: String::new(),
            structural_hash: None,
            start_line: 1,
            end_line: 1,
            metadata: None,
        }
    }

    #[test]
    fn entity_depth_follows_parent_chain() {
        let root = entity("a.ts::class::L1", None);
        let child = entity("a.ts::class::L1::L2", Some("a.ts::class::L1"));
        let grandchild = entity("a.ts::class::L1::L2::L3", Some("a.ts::class::L1::L2"));
        let entities = vec![root, child, grandchild];
        let entities_by_id = entities_by_id(&entities);

        assert_eq!(entity_depth(&entities[0], &entities_by_id), 0);
        assert_eq!(entity_depth(&entities[1], &entities_by_id), 1);
        assert_eq!(entity_depth(&entities[2], &entities_by_id), 2);
        assert_eq!(entity_indent(&entities[2], &entities_by_id, "  "), "      ");
    }

    #[test]
    fn entity_depth_handles_missing_or_cyclic_parents() {
        let missing = entity("a.ts::field::missing", Some("a.ts::field::unknown"));
        let cyclic = entity("a.ts::field::cyclic", Some("a.ts::field::cyclic"));
        let entities = vec![missing, cyclic];
        let entities_by_id = entities_by_id(&entities);

        assert_eq!(entity_depth(&entities[0], &entities_by_id), 1);
        assert_eq!(entity_depth(&entities[1], &entities_by_id), 1);
    }
}
