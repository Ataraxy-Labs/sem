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
        // Extract top-level keys and [sections] with proper line ranges.
        // TOML has two kinds of top-level entries:
        //   1. Key-value pairs before any section header
        //   2. Section headers like [package] or [dependencies]
        let lines: Vec<&str> = content.lines().collect();
        let sections = find_toml_sections(&lines);

        if sections.is_empty() {
            return Vec::new();
        }

        // Parse for content hashing
        let parsed: toml::Value = match content.parse() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let table = match parsed.as_table() {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut entities = Vec::new();
        for (i, section) in sections.iter().enumerate() {
            let end_line = if i + 1 < sections.len() {
                let next_start = sections[i + 1].line;
                trim_trailing_blanks_toml(&lines, section.line, next_start)
            } else {
                trim_trailing_blanks_toml(&lines, section.line, lines.len() + 1)
            };

            let entity_content = lines[section.line - 1..end_line].join("\n");

            // Look up in parsed table for content hash
            let (value_str, entity_type) = if let Some(val) = table.get(&section.key) {
                let is_table = val.is_table();
                let vs = if is_table {
                    serde_json::to_string_pretty(val).unwrap_or_default()
                } else {
                    toml_value_to_string(val)
                };
                (vs, if is_table { "section" } else { "property" })
            } else {
                (entity_content.clone(), "property")
            };

            entities.push(SemanticEntity {
                id: build_entity_id(file_path, entity_type, &section.key, None),
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: section.key.clone(),
                parent_id: None,
                content_hash: content_hash(&value_str),
                structural_hash: None,
                content: entity_content,
                start_line: section.line,
                end_line,
                metadata: None,
            });
        }

        entities
    }
}

struct TomlSection {
    key: String,
    line: usize, // 1-based
}

/// Find top-level entries in TOML: section headers ([name]) and root key-value pairs.
fn find_toml_sections(lines: &[&str]) -> Vec<TomlSection> {
    let mut sections = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Section header: [package] or [[bin]]
        if trimmed.starts_with('[') {
            let key = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            if !key.is_empty() {
                sections.push(TomlSection {
                    key,
                    line: i + 1,
                });
            }
            continue;
        }

        // Root key-value pair (only if no section header seen yet, or it's before the first [section])
        // Actually in TOML, root keys can appear before any section header.
        // After a [section], keys belong to that section.
        if sections.is_empty() || !has_section_before(lines, i) {
            if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim().to_string();
                if !key.is_empty() {
                    sections.push(TomlSection {
                        key,
                        line: i + 1,
                    });
                }
            }
        }
    }

    sections
}

/// Check if there's a [section] header before line index `idx`.
fn has_section_before(lines: &[&str], idx: usize) -> bool {
    for line in &lines[..idx] {
        if line.trim().starts_with('[') {
            return true;
        }
    }
    false
}

fn trim_trailing_blanks_toml(lines: &[&str], start: usize, next_start: usize) -> usize {
    let mut end = next_start - 1;
    while end > start {
        let trimmed = lines[end - 1].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            end -= 1;
        } else {
            break;
        }
    }
    end
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toml_line_positions() {
        let content = r#"[package]
name = "my-app"
version = "1.0.0"

[dependencies]
serde = "1.0"
tokio = { version = "1", features = ["full"] }
"#;
        let plugin = TomlParserPlugin;
        let entities = plugin.extract_entities(content, "Cargo.toml");

        assert_eq!(entities.len(), 2);

        assert_eq!(entities[0].name, "package");
        assert_eq!(entities[0].start_line, 1);
        assert_eq!(entities[0].end_line, 3);

        assert_eq!(entities[1].name, "dependencies");
        assert_eq!(entities[1].start_line, 5);
        assert_eq!(entities[1].end_line, 7);
    }
}
