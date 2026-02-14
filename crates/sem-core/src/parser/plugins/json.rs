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
        // Only extract top-level properties from JSON objects.
        // We scan the source text directly to get accurate line positions,
        // which weave needs for entity-level merge reconstruction.
        let trimmed = content.trim();
        if !trimmed.starts_with('{') {
            return Vec::new();
        }

        let lines: Vec<&str> = content.lines().collect();
        let entries = find_top_level_entries(content);

        let mut entities = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let end_line = if i + 1 < entries.len() {
                // End just before the next entry starts (minus trailing blank/comma lines)
                let next_start = entries[i + 1].start_line;
                trim_trailing_blanks(&lines, entry.start_line, next_start)
            } else {
                // Last entry: end before the closing brace
                let closing = find_closing_brace_line(&lines);
                trim_trailing_blanks(&lines, entry.start_line, closing)
            };

            let entity_content = lines[entry.start_line - 1..end_line]
                .join("\n");

            entities.push(SemanticEntity {
                id: build_entity_id(file_path, &entry.entity_type, &entry.pointer, None),
                file_path: file_path.to_string(),
                entity_type: entry.entity_type.clone(),
                name: entry.key.clone(),
                parent_id: None,
                content_hash: content_hash(&entity_content),
                structural_hash: None,
                content: entity_content,
                start_line: entry.start_line,
                end_line,
                metadata: None,
            });
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

        assert_eq!(entities.len(), 4);

        assert_eq!(entities[0].name, "name");
        assert_eq!(entities[0].start_line, 2);
        assert_eq!(entities[0].end_line, 2);

        assert_eq!(entities[1].name, "version");
        assert_eq!(entities[1].start_line, 3);
        assert_eq!(entities[1].end_line, 3);

        assert_eq!(entities[2].name, "scripts");
        assert_eq!(entities[2].entity_type, "object");
        assert_eq!(entities[2].start_line, 4);
        assert_eq!(entities[2].end_line, 7);

        assert_eq!(entities[3].name, "description");
        assert_eq!(entities[3].start_line, 8);
        assert_eq!(entities[3].end_line, 8);
    }
}
