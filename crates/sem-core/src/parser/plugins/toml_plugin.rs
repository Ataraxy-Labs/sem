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

            // Resolve the display name, entity type, and the value to hash.
            let (name, entity_type, value_str) = if let Some(idx) = section.array_index {
                // Array-of-tables entry: give it an index-based identity (key/0,
                // key/1, ...) and hash only its own element so appending a new
                // entry reads as an addition, not a modification of the last one.
                let value_str = table
                    .get(&section.key)
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.get(idx))
                    .map(|el| serde_json::to_string_pretty(el).unwrap_or_default())
                    .unwrap_or_else(|| entity_content.clone());
                (
                    format!("{}/{}", section.key, idx),
                    "array_table".to_string(),
                    value_str,
                )
            } else if let Some(val) = table.get(&section.key) {
                let is_table = val.is_table();
                let vs = if is_table {
                    serde_json::to_string_pretty(val).unwrap_or_default()
                } else {
                    toml_value_to_string(val)
                };
                (
                    section.key.clone(),
                    if is_table { "section" } else { "property" }.to_string(),
                    vs,
                )
            } else {
                (
                    section.key.clone(),
                    "property".to_string(),
                    entity_content.clone(),
                )
            };

            let id = build_entity_id(file_path, &entity_type, &name, None);
            entities.push(SemanticEntity {
                id,
                file_path: file_path.to_string(),
                entity_type,
                name,
                parent_id: None,
                content_hash: content_hash(&value_str),
                structural_hash: None,
                content: entity_content,
                start_line: section.line,
                end_line,
                start_byte: None,
                end_byte: None,
                metadata: None,
            });
        }

        entities
    }
}

struct TomlSection {
    key: String,
    line: usize, // 1-based
    /// `Some(n)` for the nth entry of an array-of-tables (`[[key]]`); `None` for
    /// a regular table (`[key]`) or a root key-value pair.
    array_index: Option<usize>,
}

/// Find top-level entries in TOML: section headers ([name]), array-of-tables
/// (\[\[name\]\]), and root key-value pairs.
fn find_toml_sections(lines: &[&str]) -> Vec<TomlSection> {
    let mut sections = Vec::new();
    // Per-key occurrence counter so repeated `[[key]]` entries get distinct
    // indexed identities (key/0, key/1, ...) instead of collapsing to one.
    let mut array_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Array-of-tables header: [[bin]]. Must be checked before [table] since
        // it also starts with '['. Each occurrence is a distinct element.
        if trimmed.starts_with("[[") {
            let key = trimmed
                .trim_start_matches("[[")
                .trim_end_matches("]]")
                .trim()
                .to_string();
            if !key.is_empty() {
                let idx = array_counts.entry(key.clone()).or_insert(0);
                sections.push(TomlSection {
                    key: key.clone(),
                    line: i + 1,
                    array_index: Some(*idx),
                });
                *idx += 1;
            }
            continue;
        }

        // Section header: [package]
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
                    array_index: None,
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
                        array_index: None,
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

    #[test]
    fn test_array_of_tables_get_indexed_identities() {
        // Repeated `[[array]]` entries must get distinct, index-based identities
        // so appending one reads as an addition rather than a modification of the
        // previous entry (#362).
        let content = "[[array]]\nitem = 1\n[[array]]\nitem = 2\n[[array]]\nitem = 3\n";
        let plugin = TomlParserPlugin;
        let entities = plugin.extract_entities(content, "a.toml");

        assert_eq!(entities.len(), 3);
        for (i, e) in entities.iter().enumerate() {
            assert_eq!(e.name, format!("array/{i}"));
            assert_eq!(e.entity_type, "array_table");
        }
        // Each element hashes independently, so the first two are stable when a
        // third is appended.
        let two = plugin.extract_entities("[[array]]\nitem = 1\n[[array]]\nitem = 2\n", "a.toml");
        assert_eq!(two[0].content_hash, entities[0].content_hash);
        assert_eq!(two[1].content_hash, entities[1].content_hash);
    }

    #[test]
    fn test_table_and_array_of_tables_same_name_do_not_collide() {
        // `[server]` (table) and `[[server]]` (array-of-tables) previously both
        // collapsed to id `...::server`; they must now be distinct entities.
        let content = "[server]\nhost = \"a\"\n[[worker]]\nid = 1\n";
        let entities = TomlParserPlugin.extract_entities(content, "c.toml");
        let ids: Vec<&str> = entities.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.iter().any(|id| id.contains("section::server")));
        assert!(ids.iter().any(|id| id.contains("array_table::worker/0")));
    }
}
