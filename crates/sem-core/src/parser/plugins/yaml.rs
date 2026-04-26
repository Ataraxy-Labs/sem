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
            // No top-level keys: treat the whole file as a single chunk so
            // changes to comment-only or marker-only YAML files are detected.
            if !content.trim().is_empty() {
                return vec![SemanticEntity {
                    id: build_entity_id(file_path, "chunk", "(document)", None),
                    file_path: file_path.to_string(),
                    entity_type: "chunk".to_string(),
                    name: "(document)".to_string(),
                    parent_id: None,
                    content_hash: content_hash(content),
                    structural_hash: None,
                    content: content.to_string(),
                    start_line: 1,
                    end_line: lines.len(),
                    metadata: None,
                }];
            }
            return Vec::new();
        }

        // Determine entity types using serde_yaml for section vs property.
        let section_keys: std::collections::HashSet<String> =
            if let Ok(serde_yaml::Value::Mapping(mapping)) = serde_yaml::from_str(content) {
                mapping
                    .iter()
                    .filter(|(_, v)| v.is_mapping() || v.is_sequence())
                    .filter_map(|(k, _)| k.as_str().map(String::from))
                    .collect()
            } else {
                std::collections::HashSet::new()
            };

        let mut entities = Vec::new();

        // Capture preamble (comments, document markers) before the first key
        if top_level_keys[0].line > 1 {
            let preamble_end =
                trim_trailing_blanks_yaml(&lines, 1, top_level_keys[0].line);
            if preamble_end >= 1 {
                let preamble_content = lines[..preamble_end].join("\n");
                if !preamble_content.trim().is_empty() {
                    entities.push(SemanticEntity {
                        id: build_entity_id(file_path, "chunk", "(preamble)", None),
                        file_path: file_path.to_string(),
                        entity_type: "chunk".to_string(),
                        name: "(preamble)".to_string(),
                        parent_id: None,
                        content_hash: content_hash(&preamble_content),
                        structural_hash: None,
                        content: preamble_content,
                        start_line: 1,
                        end_line: preamble_end,
                        metadata: None,
                    });
                }
            }
        }

        for (i, tk) in top_level_keys.iter().enumerate() {
            let end_line = if i + 1 < top_level_keys.len() {
                let next_start = top_level_keys[i + 1].line;
                trim_trailing_blanks_yaml(&lines, tk.line, next_start)
            } else {
                trim_trailing_blanks_yaml(&lines, tk.line, lines.len() + 1)
            };

            let entity_content = lines[tk.line - 1..end_line].join("\n");
            let is_section = section_keys.contains(&tk.key);
            let entity_type = if is_section { "section" } else { "property" };

            // Hash raw text so comment changes within a section are detected.
            entities.push(SemanticEntity {
                id: build_entity_id(file_path, entity_type, &tk.key, None),
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: tk.key.clone(),
                parent_id: None,
                content_hash: content_hash(&entity_content),
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

    #[test]
    fn test_yaml_preamble() {
        let content = "# Config file\n---\nname: my-app\nversion: 1.0.0\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "config.yaml");

        assert_eq!(entities[0].name, "(preamble)");
        assert_eq!(entities[0].entity_type, "chunk");
        assert_eq!(entities[0].start_line, 1);

        assert_eq!(entities[1].name, "name");
        assert_eq!(entities[2].name, "version");
    }

    #[test]
    fn test_yaml_comment_only_file() {
        let content = "# Just a comment\n# Another line\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "notes.yaml");

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "(document)");
        assert_eq!(entities[0].entity_type, "chunk");
    }

    #[test]
    fn test_yaml_comment_changes_detected() {
        let content_a = "name: my-app\n# old comment\nversion: 1.0.0\n";
        let content_b = "name: my-app\n# new comment\nversion: 1.0.0\n";
        let plugin = YamlParserPlugin;
        let entities_a = plugin.extract_entities(content_a, "config.yaml");
        let entities_b = plugin.extract_entities(content_b, "config.yaml");

        // The "name" entity includes the comment line in its range, so
        // its content_hash should differ between versions.
        assert_ne!(entities_a[0].content_hash, entities_b[0].content_hash);
    }
}
