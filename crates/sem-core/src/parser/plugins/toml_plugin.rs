use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

pub struct TomlParserPlugin;

impl SemanticParserPlugin for TomlParserPlugin {
    fn id(&self) -> &str {
        "toml"
    }

    fn extensions(&self) -> &[&str] {
        &[".toml"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let parsed: toml::Value = match content.parse() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let table = match parsed.as_table() {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut entities = Vec::new();
        walk_toml(table, "", file_path, &mut entities, content, 0);
        entities
    }
}

fn walk_toml(
    table: &toml::map::Map<String, toml::Value>,
    path: &str,
    file_path: &str,
    entities: &mut Vec<SemanticEntity>,
    full_content: &str,
    depth: usize,
) {
    for (key, value) in table {
        let dot_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };

        let value_str = if value.is_table() {
            serde_json::to_string_pretty(value).unwrap_or_default()
        } else {
            toml_value_to_string(value)
        };

        let entity_type = if value.is_table() {
            "section"
        } else {
            "property"
        };

        let line_match = find_key_line(full_content, key);
        let parent_id = if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        };

        entities.push(SemanticEntity {
            id: build_entity_id(file_path, entity_type, &dot_path, None),
            file_path: file_path.to_string(),
            entity_type: entity_type.to_string(),
            name: dot_path.clone(),
            parent_id,
            content_hash: content_hash(&value_str),
            content: value_str,
            start_line: line_match,
            end_line: line_match,
            metadata: None,
        });

        if let Some(inner) = value.as_table() {
            if depth < 4 {
                walk_toml(inner, &dot_path, file_path, entities, full_content, depth + 1);
            }
        }
    }
}

fn toml_value_to_string(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Array(arr) => serde_json::to_string_pretty(arr).unwrap_or_default(),
        _ => format!("{value}"),
    }
}

fn find_key_line(content: &str, key: &str) -> usize {
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&format!("{key} ="))
            || trimmed.starts_with(&format!("{key}="))
            || trimmed == format!("[{key}]")
        {
            return i + 1;
        }
    }
    0
}
