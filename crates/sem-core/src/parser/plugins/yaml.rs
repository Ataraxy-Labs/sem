use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

pub struct YamlParserPlugin;

impl SemanticParserPlugin for YamlParserPlugin {
    fn id(&self) -> &str {
        "yaml"
    }

    fn extensions(&self) -> &[&str] {
        &[".yml", ".yaml"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let parsed: serde_yaml::Value = match serde_yaml::from_str(content) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let mapping = match parsed.as_mapping() {
            Some(m) => m,
            None => return Vec::new(),
        };

        let mut entities = Vec::new();
        walk_yaml(mapping, "", file_path, &mut entities, content, 0);
        entities
    }
}

fn walk_yaml(
    mapping: &serde_yaml::Mapping,
    path: &str,
    file_path: &str,
    entities: &mut Vec<SemanticEntity>,
    full_content: &str,
    depth: usize,
) {
    for (key, value) in mapping {
        let key_str = match key.as_str() {
            Some(s) => s.to_string(),
            None => format!("{:?}", key),
        };

        let dot_path = if path.is_empty() {
            key_str.clone()
        } else {
            format!("{path}.{key_str}")
        };

        let value_str = if value.is_mapping() || value.is_sequence() {
            serde_yaml::to_string(value)
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            yaml_value_to_string(value)
        };

        let entity_type = if value.is_mapping() {
            "section"
        } else {
            "property"
        };

        let line_match = find_key_line(full_content, &key_str);
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
            structural_hash: None,
            content: value_str,
            start_line: line_match,
            end_line: line_match,
            metadata: None,
        });

        if let Some(inner) = value.as_mapping() {
            if depth < 4 {
                walk_yaml(inner, &dot_path, file_path, entities, full_content, depth + 1);
            }
        }
    }
}

fn yaml_value_to_string(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Null => "null".to_string(),
        _ => format!("{:?}", value),
    }
}

fn find_key_line(content: &str, key: &str) -> usize {
    for (i, line) in content.lines().enumerate() {
        if line.trim_start().starts_with(&format!("{key}:")) {
            return i + 1;
        }
    }
    0
}
