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
        // Extract top-level properties from JSON objects, plus depth-2 children
        // for "object" entities (e.g. scripts, dependencies in package.json).
        // We scan the source text directly to get accurate line positions,
        // which weave needs for entity-level merge reconstruction.
        let trimmed = content.trim();
        if !trimmed.starts_with('{') {
            return Vec::new();
        }

        let lines: Vec<&str> = content.lines().collect();
        let entries = find_top_level_entries(content);
        let closing = find_closing_brace_line(&lines);

        let mut entities = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let end_line = if i + 1 < entries.len() {
                let next_start = entries[i + 1].start_line;
                trim_trailing_blanks(&lines, entry.start_line, next_start)
            } else {
                trim_trailing_blanks(&lines, entry.start_line, closing)
            };

            let entity_content = lines[entry.start_line - 1..end_line]
                .join("\n");

            let value_content = extract_value_content(&entity_content);
            let structural_hash = Some(content_hash(value_content));

            let parent_id = build_entity_id(file_path, &entry.entity_type, &entry.pointer, None);

            entities.push(SemanticEntity {
                id: parent_id.clone(),
                file_path: file_path.to_string(),
                entity_type: entry.entity_type.clone(),
                name: entry.key.clone(),
                parent_id: None,
                content_hash: content_hash(&entity_content),
                structural_hash,
                content: entity_content.clone(),
                start_line: entry.start_line,
                end_line,
                metadata: None,
            });

            // Extract depth-2 children from "object" entities
            if entry.entity_type == "object" {
                let nested = find_nested_object_entries(&entity_content, entry.start_line);
                for (j, nentry) in nested.iter().enumerate() {
                    let child_end = if j + 1 < nested.len() {
                        trim_trailing_blanks(&lines, nentry.start_line, nested[j + 1].start_line)
                    } else {
                        trim_trailing_blanks(&lines, nentry.start_line, end_line)
                    };

                    let child_content = lines[nentry.start_line - 1..child_end].join("\n");
                    let child_value = extract_value_content(&child_content);

                    entities.push(SemanticEntity {
                        id: build_entity_id(file_path, &nentry.entity_type, &nentry.key, Some(&parent_id)),
                        file_path: file_path.to_string(),
                        entity_type: nentry.entity_type.clone(),
                        name: nentry.key.clone(),
                        parent_id: Some(parent_id.clone()),
                        content_hash: content_hash(&child_content),
                        structural_hash: Some(content_hash(child_value)),
                        content: child_content,
                        start_line: nentry.start_line,
                        end_line: child_end,
                        metadata: None,
                    });
                }
            }
        }

        entities
    }
}

struct JsonEntry {
    key: String,
    pointer: String,
    entity_type: String,
    start_line: usize, // 1-based
}

/// Scan the source text to find each top-level key in the root JSON object.
/// Returns entries with accurate start_line positions.
fn find_top_level_entries(content: &str) -> Vec<JsonEntry> {
    let mut entries = Vec::new();
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;
    let mut line_num: usize = 1;

    // State for tracking when we find a key at depth 1
    let mut current_key: Option<String> = None;
    let mut key_start = false;
    let mut key_buf = String::new();
    let mut reading_key = false;

    for ch in content.chars() {
        if ch == '\n' {
            line_num += 1;
            continue;
        }

        if escape_next {
            if reading_key {
                key_buf.push(ch);
            }
            escape_next = false;
            continue;
        }

        if ch == '\\' && in_string {
            if reading_key {
                key_buf.push(ch);
            }
            escape_next = true;
            continue;
        }

        if in_string {
            if ch == '"' {
                in_string = false;
                if reading_key {
                    reading_key = false;
                    current_key = Some(key_buf.clone());
                    key_buf.clear();
                }
            } else if reading_key {
                key_buf.push(ch);
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                // At depth 1, a string could be a key (before ':') or value (after ':')
                if depth == 1 && current_key.is_none() && !key_start {
                    reading_key = true;
                    key_buf.clear();
                }
            }
            ':' => {
                if depth == 1 {
                    if let Some(ref key) = current_key {
                        // Found a key: value pair at depth 1
                        let escaped_key = key.replace('~', "~0").replace('/', "~1");
                        let pointer = format!("/{escaped_key}");
                        entries.push(JsonEntry {
                            key: key.clone(),
                            pointer,
                            entity_type: String::new(), // filled in below
                            start_line: line_num,
                        });
                        key_start = true;
                    }
                }
            }
            '{' | '[' => {
                depth += 1;
                if depth == 2 && key_start {
                    // The value for this key is an object/array
                    if let Some(entry) = entries.last_mut() {
                        entry.entity_type = "object".to_string();
                    }
                }
            }
            '}' | ']' => {
                depth -= 1;
            }
            ',' => {
                if depth == 1 {
                    // End of a top-level entry
                    if let Some(entry) = entries.last_mut() {
                        if entry.entity_type.is_empty() {
                            entry.entity_type = "property".to_string();
                        }
                    }
                    current_key = None;
                    key_start = false;
                }
            }
            _ => {}
        }
    }

    // Handle last entry (no trailing comma)
    if let Some(entry) = entries.last_mut() {
        if entry.entity_type.is_empty() {
            entry.entity_type = "property".to_string();
        }
    }

    entries
}

/// Find keys inside a depth-1 object value within an entity's content.
/// Returns entries with absolute line numbers computed from `base_line`.
fn find_nested_object_entries(entity_content: &str, base_line: usize) -> Vec<JsonEntry> {
    let mut entries = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;
    let mut line_num: usize = 0; // 0-based offset from base_line
    let mut found_outer_colon = false;
    let mut found_value_start = false;
    let mut value_depth: usize = 0;
    let mut current_key: Option<String> = None;
    let mut reading_key = false;
    let mut key_buf = String::new();
    let mut key_start = false;

    for ch in entity_content.chars() {
        if ch == '\n' {
            line_num += 1;
            continue;
        }

        if escape_next {
            if reading_key {
                key_buf.push(ch);
            }
            escape_next = false;
            continue;
        }

        if ch == '\\' && in_string {
            if reading_key {
                key_buf.push(ch);
            }
            escape_next = true;
            continue;
        }

        if in_string {
            if ch == '"' {
                in_string = false;
                if reading_key {
                    reading_key = false;
                    current_key = Some(key_buf.clone());
                    key_buf.clear();
                }
            } else if reading_key {
                key_buf.push(ch);
            }
            continue;
        }

        if !found_value_start {
            match ch {
                '"' => {
                    in_string = true;
                }
                ':' => {
                    found_outer_colon = true;
                }
                '{' if found_outer_colon => {
                    found_value_start = true;
                    value_depth = 1;
                }
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                if value_depth == 1 && current_key.is_none() && !key_start {
                    reading_key = true;
                    key_buf.clear();
                }
            }
            ':' => {
                if value_depth == 1 {
                    if let Some(ref key) = current_key {
                        entries.push(JsonEntry {
                            key: key.clone(),
                            pointer: String::new(),
                            entity_type: "property".to_string(),
                            start_line: base_line + line_num,
                        });
                        key_start = true;
                    }
                }
            }
            '{' | '[' => {
                value_depth += 1;
            }
            '}' | ']' => {
                value_depth -= 1;
                if value_depth == 0 {
                    break;
                }
            }
            ',' => {
                if value_depth == 1 {
                    current_key = None;
                    key_start = false;
                }
            }
            _ => {}
        }
    }

    entries
}

/// Extract just the value portion of a `"key": value` entity content string,
/// stripping the key name so that renamed keys with identical values share the
/// same structural_hash and are detected as renames rather than delete + add.
fn extract_value_content(content: &str) -> &str {
    let mut in_string = false;
    let mut escape_next = false;
    for (i, ch) in content.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
        }
        if ch == ':' && !in_string {
            let rest = content[i + 1..].trim();
            return rest.trim_end_matches(',').trim();
        }
    }
    content
}

/// Find the line number (1-based) of the closing `}` of the root object.
fn find_closing_brace_line(lines: &[&str]) -> usize {
    for (i, line) in lines.iter().enumerate().rev() {
        if line.trim() == "}" {
            return i + 1;
        }
    }
    lines.len()
}

/// Walk backwards from next_start to skip trailing blank lines and commas,
/// returning the end_line (1-based, inclusive) for the current entry.
fn trim_trailing_blanks(lines: &[&str], start: usize, next_start: usize) -> usize {
    let mut end = next_start - 1;
    while end > start {
        let trimmed = lines[end - 1].trim();
        if trimmed.is_empty() || trimmed == "," {
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
    use crate::model::change::ChangeType;
    use crate::model::identity::match_entities;

    #[test]
    fn test_json_line_positions() {
        let content = r#"{
  "name": "my-app",
  "version": "1.0.0",
  "scripts": {
    "build": "tsc",
    "test": "jest"
  },
  "description": "a test app"
}
"#;
        let plugin = JsonParserPlugin;
        let entities = plugin.extract_entities(content, "package.json");

        assert_eq!(entities.len(), 6);

        assert_eq!(entities[0].name, "name");
        assert_eq!(entities[0].start_line, 2);
        assert_eq!(entities[0].end_line, 2);
        assert!(entities[0].parent_id.is_none());

        assert_eq!(entities[1].name, "version");
        assert_eq!(entities[1].start_line, 3);
        assert_eq!(entities[1].end_line, 3);

        assert_eq!(entities[2].name, "scripts");
        assert_eq!(entities[2].entity_type, "object");
        assert_eq!(entities[2].start_line, 4);
        assert_eq!(entities[2].end_line, 7);

        // Depth-2 children of "scripts"
        assert_eq!(entities[3].name, "build");
        assert_eq!(entities[3].start_line, 5);
        assert_eq!(entities[3].end_line, 5);
        assert_eq!(entities[3].parent_id.as_deref(), Some(&entities[2].id as &str));

        assert_eq!(entities[4].name, "test");
        assert_eq!(entities[4].start_line, 6);
        assert_eq!(entities[4].end_line, 6);
        assert_eq!(entities[4].parent_id.as_deref(), Some(&entities[2].id as &str));

        assert_eq!(entities[5].name, "description");
        assert_eq!(entities[5].start_line, 8);
        assert_eq!(entities[5].end_line, 8);
    }

    #[test]
    fn test_rename_detected_end_to_end() {
        let before_content = "{\n  \"timeout\": 30\n}\n";
        let after_content = "{\n  \"request_timeout\": 30\n}\n";
        let plugin = JsonParserPlugin;
        let before = plugin.extract_entities(before_content, "config.json");
        let after = plugin.extract_entities(after_content, "config.json");
        let result = match_entities(&before, &after, "config.json", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Renamed);
        assert_eq!(result.changes[0].entity_name, "request_timeout");
    }

    #[test]
    fn test_renamed_scalar_property_shares_structural_hash() {
        let before_content = "{\n  \"timeout\": 30\n}\n";
        let after_content = "{\n  \"request_timeout\": 30\n}\n";
        let plugin = JsonParserPlugin;
        let before = plugin.extract_entities(before_content, "config.json");
        let after = plugin.extract_entities(after_content, "config.json");
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        // content_hash differs (key name is part of content)
        assert_ne!(before[0].content_hash, after[0].content_hash);
        // structural_hash matches (same value)
        assert_eq!(before[0].structural_hash, after[0].structural_hash);
    }

    #[test]
    fn test_renamed_object_property_shares_structural_hash() {
        let before_content = "{\n  \"config\": {\n    \"port\": 8080\n  }\n}\n";
        let after_content = "{\n  \"settings\": {\n    \"port\": 8080\n  }\n}\n";
        let plugin = JsonParserPlugin;
        let before = plugin.extract_entities(before_content, "config.json");
        let after = plugin.extract_entities(after_content, "config.json");
        // 1 parent + 1 child ("port")
        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 2);
        assert_ne!(before[0].content_hash, after[0].content_hash);
        assert_eq!(before[0].structural_hash, after[0].structural_hash);
    }
}
