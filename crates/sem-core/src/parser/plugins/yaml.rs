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
        // Extract top-level keys with proper line ranges by scanning the source text.
        // A top-level key starts a line with no indentation (e.g. "key:" or "key: value").
        // Its range extends until the next top-level key or end of file.
        let lines: Vec<&str> = content.lines().collect();
        let top_level_keys = find_top_level_keys(&lines);

        if top_level_keys.is_empty() {
            return Vec::new();
        }

        // Parse with serde_yaml for content hashing
        let parsed: serde_yaml::Value = match serde_yaml::from_str(content) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mapping = match parsed.as_mapping() {
            Some(m) => m,
            None => return Vec::new(),
        };

        // Build a lookup from key name to serialized value
        let mut value_map: std::collections::HashMap<String, (String, bool)> =
            std::collections::HashMap::new();
        for (key, value) in mapping {
            let key_str = match key.as_str() {
                Some(s) => s.to_string(),
                None => format!("{:?}", key),
            };
            let is_section = value.is_mapping() || value.is_sequence();
            let value_str = if is_section {
                serde_yaml::to_string(value)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            } else {
                yaml_value_to_string(value)
            };
            value_map.insert(key_str, (value_str, is_section));
        }

        let mut entities = Vec::new();
        for (i, tk) in top_level_keys.iter().enumerate() {
            let end_line = if i + 1 < top_level_keys.len() {
                // End just before the next top-level key (skip trailing blanks)
                let next_start = top_level_keys[i + 1].line;
                trim_trailing_blanks_yaml(&lines, tk.line, next_start)
            } else {
                // Last key: extend to end of file (skip trailing blanks)
                trim_trailing_blanks_yaml(&lines, tk.line, lines.len() + 1)
            };

            let entity_content = lines[tk.line - 1..end_line].join("\n");
            let (value_str, is_section) = value_map
                .get(&tk.key)
                .cloned()
                .unwrap_or_else(|| (entity_content.clone(), false));

            let entity_type = if is_section { "section" } else { "property" };

            entities.push(SemanticEntity {
                id: build_entity_id(file_path, entity_type, &tk.key, None),
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: tk.key.clone(),
                parent_id: None,
                content_hash: content_hash(&value_str),
                structural_hash: None,
                content: entity_content,
                start_line: tk.line,
                end_line,
                metadata: None,
            });
        }

        entities
    }
}

struct TopLevelKey {
    key: String,
    line: usize, // 1-based
}

/// Find all top-level keys in the YAML source. A top-level key is a line
/// that starts with a non-space, non-comment character and contains a colon.
fn find_top_level_keys(lines: &[&str]) -> Vec<TopLevelKey> {
    let mut keys = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        // Skip comments and document markers
        if line.starts_with('#') || line.starts_with("---") || line.starts_with("...") {
            continue;
        }
        // Extract the key (everything before the first ':')
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_string();
            if !key.is_empty() {
                keys.push(TopLevelKey {
                    key,
                    line: i + 1,
                });
            }
        }
    }
    keys
}

fn trim_trailing_blanks_yaml(lines: &[&str], start: usize, next_start: usize) -> usize {
    let mut end = next_start - 1;
    while end > start {
        let trimmed = lines[end - 1].trim();
        if trimmed.is_empty() {
            end -= 1;
        } else {
            break;
        }
    }
    end
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yaml_line_positions() {
        let content = "name: my-app\nversion: 1.0.0\nscripts:\n  build: tsc\n  test: jest\ndescription: a test app\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "config.yaml");

        assert_eq!(entities.len(), 4);

        assert_eq!(entities[0].name, "name");
        assert_eq!(entities[0].start_line, 1);
        assert_eq!(entities[0].end_line, 1);

        assert_eq!(entities[1].name, "version");
        assert_eq!(entities[1].start_line, 2);
        assert_eq!(entities[1].end_line, 2);

        assert_eq!(entities[2].name, "scripts");
        assert_eq!(entities[2].entity_type, "section");
        assert_eq!(entities[2].start_line, 3);
        assert_eq!(entities[2].end_line, 5);

        assert_eq!(entities[3].name, "description");
        assert_eq!(entities[3].start_line, 6);
        assert_eq!(entities[3].end_line, 6);
    }
}
