use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

pub struct JsonParserPlugin;

impl SemanticParserPlugin for JsonParserPlugin {
    fn id(&self) -> &str {
        "json"
    }

    fn extensions(&self) -> &[&str] {
        &[".json"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let parsed: serde_json::Value = match serde_json::from_str(content) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut entities = Vec::new();
        walk(&parsed, "", file_path, &mut entities, 0);
        entities
    }
}

fn walk(
    value: &serde_json::Value,
    pointer: &str,
    file_path: &str,
    entities: &mut Vec<SemanticEntity>,
    depth: usize,
) {
    match value {
        serde_json::Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let item_pointer = format!("{pointer}/{i}");

                // Only create entities for non-primitive array items
                if item.is_object() || item.is_array() {
                    let item_content = serde_json::to_string_pretty(item).unwrap_or_default();
                    let name = format!("[{i}]");
                    let parent_id = if pointer.is_empty() {
                        None
                    } else {
                        Some(pointer)
                    };

                    entities.push(SemanticEntity {
                        id: build_entity_id(file_path, "element", &item_pointer, None),
                        file_path: file_path.to_string(),
                        entity_type: "element".to_string(),
                        name,
                        parent_id: parent_id.map(String::from),
                        content_hash: content_hash(&item_content),
                        structural_hash: None,
                        content: item_content,
                        start_line: 0,
                        end_line: 0,
                        metadata: None,
                    });

                    if depth < 3 {
                        walk(item, &item_pointer, file_path, entities, depth + 1);
                    }
                }
            }
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                let escaped_key = key.replace('~', "~0").replace('/', "~1");
                let prop_pointer = format!("{pointer}/{escaped_key}");
                let prop_content = serde_json::to_string_pretty(val).unwrap_or_default();
                let entity_type = if val.is_object() || val.is_array() {
                    "object"
                } else {
                    "property"
                };
                let parent_id = if pointer.is_empty() {
                    None
                } else {
                    Some(pointer)
                };

                entities.push(SemanticEntity {
                    id: build_entity_id(file_path, entity_type, &prop_pointer, None),
                    file_path: file_path.to_string(),
                    entity_type: entity_type.to_string(),
                    name: key.clone(),
                    parent_id: parent_id.map(String::from),
                    content_hash: content_hash(&prop_content),
                    structural_hash: None,
                    content: prop_content,
                    start_line: 0,
                    end_line: 0,
                    metadata: None,
                });

                if (val.is_object() || val.is_array()) && depth < 3 {
                    walk(val, &prop_pointer, file_path, entities, depth + 1);
                }
            }
        }
        _ => {}
    }
}
