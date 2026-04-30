use crate::model::entity::SemanticEntity;
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
        let trimmed = content.trim();
        if !trimmed.starts_with('{') {
            return Vec::new();
        }

        let mut entities = Vec::new();
        extract_entries_recursive(content, file_path, 1, None, None, &mut entities);
        entities
    }
}

/// Recursively extract entities from a JSON object string.
///
/// - `content`: the full text of the object (including surrounding `{` `}`)
/// - `file_path`: original file path, threaded through for entity IDs
/// - `line_offset`: 1-based absolute line number of the first line of `content`
/// - `parent_pointer`: JSON Pointer prefix for children, e.g. `Some("/scripts")`
/// - `parent_entity_id`: the entity id of the enclosing entity (for `parent_id` field)
/// - `out`: collected entities, appended in-place (DFS pre-order)
fn extract_entries_recursive(
    content: &str,
    file_path: &str,
    line_offset: usize,
    parent_pointer: Option<&str>,
    parent_entity_id: Option<&str>,
    out: &mut Vec<SemanticEntity>,
) {
    let lines: Vec<&str> = content.lines().collect();
    let entries = find_top_level_entries(content);

    for (i, entry) in entries.iter().enumerate() {
        let end_line = if i + 1 < entries.len() {
            let next_start = entries[i + 1].start_line;
            trim_trailing_blanks(&lines, entry.start_line, next_start)
        } else {
            let closing = find_closing_brace_line(&lines);
            trim_trailing_blanks(&lines, entry.start_line, closing)
        };

        let entity_content = lines[entry.start_line - 1..end_line].join("\n");

        let value_content = extract_value_content(&entity_content);
        let structural_hash = Some(content_hash(value_content));

        // Build JSON Pointer path: parent_pointer + "/" + escaped_key
        let pointer = match parent_pointer {
            Some(pp) => format!("{pp}{}", entry.pointer),
            None => entry.pointer.clone(),
        };

        let abs_start = line_offset + entry.start_line - 1;
        let abs_end = line_offset + end_line - 1;

        // JSON entity IDs are file::pointer — entity_type is intentionally not
        // part of the ID so that scalar↔object value-type changes match as Modified.
        let entity_id = format!("{}::{}", file_path, pointer);

        out.push(SemanticEntity {
            id: entity_id.clone(),
            file_path: file_path.to_string(),
            entity_type: entry.entity_type.clone(),
            name: entry.key.clone(),
            parent_id: parent_entity_id.map(str::to_string),
            content_hash: content_hash(&entity_content),
            structural_hash,
            content: entity_content.clone(),
            start_line: abs_start,
            end_line: abs_end,
            metadata: None,
        });

        // If this entry is an object, recurse into its value
        if entry.entity_type == "object" {
            if let Some(obj_str) = extract_object_value(&entity_content) {
                // The object value starts at the line with the opening `{`.
                // We need to find the absolute line of that `{` inside entity_content.
                let obj_line_in_entity = find_value_start_line(&entity_content);
                let obj_abs_line = abs_start + obj_line_in_entity - 1;
                extract_entries_recursive(
                    obj_str,
                    file_path,
                    obj_abs_line,
                    Some(&pointer),
                    Some(&entity_id),
                    out,
                );
            }
        }
    }
}

/// Given an entity content string like `  "scripts": {\n    "build": "tsc"\n  }`,
/// return a slice that starts at the opening `{` of the value and ends at (and
/// including) the matching closing `}`.
fn extract_object_value(content: &str) -> Option<&str> {
    // Skip past the first `:` (outside strings) to find the value
    let mut in_string = false;
    let mut escape_next = false;
    let mut colon_pos: Option<usize> = None;

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
            colon_pos = Some(i);
            break;
        }
    }

    let after_colon = &content[colon_pos? + 1..];
    // Find the opening `{`
    let brace_offset = after_colon.find('{')?;
    let obj_start = colon_pos? + 1 + brace_offset;

    // Find the matching `}`
    let mut depth = 0usize;
    in_string = false;
    escape_next = false;

    for (i, ch) in content[obj_start..].char_indices() {
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
            continue;
        }
        if !in_string {
            match ch {
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&content[obj_start..obj_start + i + 1]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Return the 1-based line number (relative to the entity content) where the
/// object value's `{` appears.
fn find_value_start_line(content: &str) -> usize {
    let mut in_string = false;
    let mut escape_next = false;
    let mut past_colon = false;
    let mut line = 1usize;

    for ch in content.chars() {
        if ch == '\n' {
            line += 1;
            continue;
        }
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
            continue;
        }
        if ch == ':' && !in_string {
            past_colon = true;
            continue;
        }
        if past_colon && ch == '{' {
            return line;
        }
    }
    1
}

struct JsonEntry {
    key: String,
    pointer: String,
    entity_type: String,
    start_line: usize, // 1-based, relative to the content passed in
}

/// Scan the source text to find each top-level key in the root JSON object.
/// Returns entries with accurate start_line positions (1-based, relative to `content`).
fn find_top_level_entries(content: &str) -> Vec<JsonEntry> {
    let mut entries = Vec::new();
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;
    let mut line_num: usize = 1;

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
                if depth == 1 && current_key.is_none() && !key_start {
                    reading_key = true;
                    key_buf.clear();
                }
            }
            ':' => {
                if depth == 1 {
                    if let Some(ref key) = current_key {
                        let escaped_key = key.replace('~', "~0").replace('/', "~1");
                        let pointer = format!("/{escaped_key}");
                        entries.push(JsonEntry {
                            key: key.clone(),
                            pointer,
                            entity_type: String::new(),
                            start_line: line_num,
                        });
                        key_start = true;
                    }
                }
            }
            '{' | '[' => {
                depth += 1;
                if depth == 2 && key_start {
                    if let Some(entry) = entries.last_mut() {
                        entry.entity_type = if ch == '{' { "object" } else { "array" }.to_string();
                    }
                }
            }
            '}' | ']' => {
                depth -= 1;
            }
            ',' => {
                if depth == 1 {
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

    if let Some(entry) = entries.last_mut() {
        if entry.entity_type.is_empty() {
            entry.entity_type = "property".to_string();
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
    use crate::git::types::{FileChange, FileStatus};
    use crate::model::change::{ChangeType, SemanticChange};
    use crate::parser::differ::compute_semantic_diff;
    use crate::parser::registry::ParserRegistry;

    /// Run the full pipeline and drop orphan changes (which represent line-level
    /// noise outside entity spans like the root `{` `}` brackets).
    fn json_diff(before: &str, after: &str) -> Vec<SemanticChange> {
        let mut registry = ParserRegistry::new();
        registry.register(Box::new(JsonParserPlugin));
        let changes = vec![FileChange {
            file_path: "test.json".to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(before.to_string()),
            after_content: Some(after.to_string()),
        }];
        compute_semantic_diff(&changes, &registry, None, None)
            .changes
            .into_iter()
            .filter(|c| c.entity_type != "orphan")
            .collect()
    }

    fn names(changes: &[SemanticChange]) -> Vec<(String, ChangeType)> {
        changes.iter().map(|c| (c.entity_name.clone(), c.change_type)).collect()
    }

    fn find_change<'a>(changes: &'a [SemanticChange], name: &str, kind: ChangeType) -> &'a SemanticChange {
        changes.iter().find(|c| c.entity_name == name && c.change_type == kind)
            .unwrap_or_else(|| panic!("expected {:?} {} in changes; got: {:?}", kind, name, names(changes)))
    }

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

        // Top-level entities
        let top: Vec<_> = entities.iter().filter(|e| e.parent_id.is_none()).collect();
        assert_eq!(top.len(), 4);

        assert_eq!(top[0].name, "name");
        assert_eq!(top[0].start_line, 2);
        assert_eq!(top[0].end_line, 2);

        assert_eq!(top[1].name, "version");
        assert_eq!(top[1].start_line, 3);
        assert_eq!(top[1].end_line, 3);

        assert_eq!(top[2].name, "scripts");
        assert_eq!(top[2].entity_type, "object");
        assert_eq!(top[2].start_line, 4);
        assert_eq!(top[2].end_line, 7);

        assert_eq!(top[3].name, "description");
        assert_eq!(top[3].start_line, 8);
        assert_eq!(top[3].end_line, 8);
    }

    #[test]
    fn test_nested_entities_extracted() {
        let content = r#"{
  "scripts": {
    "build": "tsc",
    "test": "jest"
  }
}
"#;
        let plugin = JsonParserPlugin;
        let entities = plugin.extract_entities(content, "package.json");

        // Should have "scripts" (top-level) + "build" and "test" (nested)
        assert_eq!(entities.len(), 3);

        let scripts = entities.iter().find(|e| e.name == "scripts").unwrap();
        assert!(scripts.parent_id.is_none());

        let build = entities.iter().find(|e| e.name == "build").unwrap();
        assert_eq!(build.parent_id, Some(scripts.id.clone()));
        assert_eq!(build.start_line, 3);

        let test = entities.iter().find(|e| e.name == "test").unwrap();
        assert_eq!(test.parent_id, Some(scripts.id.clone()));
        assert_eq!(test.start_line, 4);
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Top-level scalars
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn scalar_value_change_reports_modified() {
        let changes = json_diff(
            "{\n  \"name\": \"foo\"\n}",
            "{\n  \"name\": \"bar\"\n}",
        );
        assert_eq!(names(&changes), vec![("name".into(), ChangeType::Modified)]);
        assert_eq!(changes[0].parent_name, None);
    }

    #[test]
    fn scalar_added_to_empty_object_reports_only_the_scalar() {
        let changes = json_diff("{}", "{\n  \"name\": \"foo\"\n}");
        assert_eq!(names(&changes), vec![("name".into(), ChangeType::Added)]);
    }

    #[test]
    fn scalar_deleted_from_object_reports_only_the_scalar() {
        let changes = json_diff("{\n  \"name\": \"foo\"\n}", "{}");
        assert_eq!(names(&changes), vec![("name".into(), ChangeType::Deleted)]);
    }

    #[test]
    fn scalar_key_renamed_with_unchanged_value_reports_renamed() {
        let changes = json_diff(
            "{\n  \"timeout\": 30\n}",
            "{\n  \"testTimeout\": 30\n}",
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Renamed);
        assert_eq!(changes[0].entity_name, "testTimeout");
        assert_eq!(changes[0].old_entity_name.as_deref(), Some("timeout"));
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Parent suppression — object containers don't surface when children change
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn child_modified_inside_object_only_child_reported() {
        let changes = json_diff(
            "{\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}",
            "{\n  \"scripts\": {\n    \"build\": \"webpack\"\n  }\n}",
        );
        assert!(!changes.iter().any(|c| c.entity_name == "scripts"),
            "scripts should be suppressed; got: {:?}", names(&changes));
        let build = find_change(&changes, "build", ChangeType::Modified);
        assert_eq!(build.parent_name.as_deref(), Some("scripts"));
    }

    #[test]
    fn child_added_inside_object_only_child_reported() {
        let changes = json_diff(
            "{\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}",
            "{\n  \"scripts\": {\n    \"build\": \"tsc\",\n    \"test\": \"jest\"\n  }\n}",
        );
        assert!(!changes.iter().any(|c| c.entity_name == "scripts" && c.change_type == ChangeType::Modified),
            "scripts should be suppressed; got: {:?}", names(&changes));
        let test = find_change(&changes, "test", ChangeType::Added);
        assert_eq!(test.parent_name.as_deref(), Some("scripts"));
    }

    #[test]
    fn child_deleted_inside_object_only_child_reported() {
        let changes = json_diff(
            "{\n  \"scripts\": {\n    \"build\": \"tsc\",\n    \"test\": \"jest\"\n  }\n}",
            "{\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}",
        );
        assert!(!changes.iter().any(|c| c.entity_name == "scripts" && c.change_type == ChangeType::Modified),
            "scripts should be suppressed; got: {:?}", names(&changes));
        let test = find_change(&changes, "test", ChangeType::Deleted);
        assert_eq!(test.parent_name.as_deref(), Some("scripts"));
    }

    #[test]
    fn whole_object_added_only_leaf_children_reported() {
        let changes = json_diff(
            "{}",
            "{\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}",
        );
        assert!(!changes.iter().any(|c| c.entity_name == "scripts"),
            "scripts (container) should be suppressed; got: {:?}", names(&changes));
        let build = find_change(&changes, "build", ChangeType::Added);
        assert_eq!(build.parent_name.as_deref(), Some("scripts"));
    }

    #[test]
    fn whole_object_deleted_only_leaf_children_reported() {
        let changes = json_diff(
            "{\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}",
            "{}",
        );
        assert!(!changes.iter().any(|c| c.entity_name == "scripts"),
            "scripts (container) should be suppressed; got: {:?}", names(&changes));
        find_change(&changes, "build", ChangeType::Deleted);
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Deep nesting — full ancestor chain in parent_name
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn deep_nested_value_change_reports_only_the_leaf_with_full_chain() {
        let before = r#"{
  "jest": {
    "config": {
      "testTimeout": 5000
    }
  }
}"#;
        let after = r#"{
  "jest": {
    "config": {
      "testTimeout": 10000
    }
  }
}"#;
        let changes = json_diff(before, after);
        assert_eq!(names(&changes), vec![("testTimeout".into(), ChangeType::Modified)]);
        assert_eq!(changes[0].parent_name.as_deref(), Some("jest::config"));
    }

    #[test]
    fn empty_string_key_ancestor_is_skipped_in_parent_name() {
        // package-lock.json uses "" as a key for the root project.
        // Walking the parent chain for a deeply-nested change must not emit
        // the empty name (would render as "::::") in the displayed path.
        let before = r#"{
  "packages": {
    "": {
      "dependencies": {
        "jose": "^6.1.3"
      }
    }
  }
}"#;
        let after = r#"{
  "packages": {
    "": {
      "dependencies": {
        "jose": "^6.1.4"
      }
    }
  }
}"#;
        let changes = json_diff(before, after);
        let jose = find_change(&changes, "jose", ChangeType::Modified);
        // The empty-string key ancestor is dropped from the displayed chain.
        assert_eq!(jose.parent_name.as_deref(), Some("packages::dependencies"));
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Renames at the object level
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn nested_scalar_rename_with_unchanged_value() {
        // Same value → structural_hash matches → Renamed.
        let before = r#"{
  "scripts": {
    "run": "node .",
    "test": "jest"
  }
}"#;
        let after = r#"{
  "scripts": {
    "start": "node .",
    "test": "jest"
  }
}"#;
        let changes = json_diff(before, after);
        let renames: Vec<_> = changes.iter().filter(|c| c.change_type == ChangeType::Renamed).collect();
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].entity_name, "start");
        assert_eq!(renames[0].old_entity_name.as_deref(), Some("run"));
        assert_eq!(renames[0].parent_name.as_deref(), Some("scripts"));
    }

    #[test]
    fn parent_object_renamed_unchanged_child_move_suppressed() {
        // scripts → tasks, dev unchanged: only the parent rename is reported.
        let before = "{\n  \"scripts\": {\n    \"dev\": \"vite\"\n  }\n}\n";
        let after = "{\n  \"tasks\": {\n    \"dev\": \"vite\"\n  }\n}\n";
        let changes = json_diff(before, after);
        let tasks = find_change(&changes, "tasks", ChangeType::Renamed);
        assert_eq!(tasks.old_entity_name.as_deref(), Some("scripts"));
        assert!(!changes.iter().any(|c| c.entity_name == "dev"),
            "child 'dev' should be suppressed (only moved due to parent rename); got: {:?}", names(&changes));
    }

    #[test]
    fn parent_object_renamed_and_child_renamed_only_child_surfaces() {
        // scripts → tasks AND dev → develop. Parent rename cannot be detected
        // because the renamed child key changes the parent's structural_hash.
        // The child move alone conveys the move + rename via:
        //   parent_name="tasks", old_entity_name="dev", old_parent_id=<scripts>
        let before = "{\n  \"scripts\": {\n    \"dev\": \"vite\"\n  }\n}\n";
        let after = "{\n  \"tasks\": {\n    \"develop\": \"vite\"\n  }\n}\n";
        let changes = json_diff(before, after);
        assert_eq!(names(&changes), vec![("develop".into(), ChangeType::Moved)]);
        let develop = &changes[0];
        assert_eq!(develop.old_entity_name.as_deref(), Some("dev"));
        assert_eq!(develop.parent_name.as_deref(), Some("tasks"));
        assert!(develop.old_parent_id.is_some(), "child Moved should carry old_parent_id");
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Type transitions — scalar ↔ object
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn scalar_to_object_transition_reports_modified_plus_new_children_added() {
        let changes = json_diff(
            "{\n  \"build\": \"tsc\"\n}",
            "{\n  \"build\": {\n    \"command\": \"tsc\"\n  }\n}",
        );
        let build = find_change(&changes, "build", ChangeType::Modified);
        assert_eq!(build.entity_type, "object", "after type should reflect new value");
        let command = find_change(&changes, "command", ChangeType::Added);
        assert_eq!(command.parent_name.as_deref(), Some("build"));
    }

    #[test]
    fn object_to_scalar_transition_reports_modified_plus_old_children_deleted() {
        let changes = json_diff(
            "{\n  \"config\": {\n    \"watch\": true\n  }\n}",
            "{\n  \"config\": \"auto\"\n}",
        );
        let config = find_change(&changes, "config", ChangeType::Modified);
        assert_eq!(config.entity_type, "property", "after type should reflect new value");
        find_change(&changes, "watch", ChangeType::Deleted);
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Arrays — opaque (no recursion into elements)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn array_modified_reports_only_the_array_key() {
        let changes = json_diff(
            "{\n  \"deps\": [\"react\", \"vue\"]\n}",
            "{\n  \"deps\": [\"react\", \"vue\", \"lodash\"]\n}",
        );
        assert_eq!(names(&changes), vec![("deps".into(), ChangeType::Modified)]);
    }

    #[test]
    fn array_renamed_when_contents_unchanged() {
        let changes = json_diff(
            "{\n  \"deps\": [\"react\", \"vue\"]\n}",
            "{\n  \"dependencies\": [\"react\", \"vue\"]\n}",
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Renamed);
        assert_eq!(changes[0].entity_name, "dependencies");
    }

    #[test]
    fn array_element_keys_are_not_tracked_as_entities() {
        let before = r#"{
  "deps": [
    {"name": "react"},
    {"name": "vue"}
  ]
}"#;
        let after = r#"{
  "deps": [
    {"package": "react"},
    {"name": "vue"}
  ]
}"#;
        let changes = json_diff(before, after);
        assert_eq!(names(&changes), vec![("deps".into(), ChangeType::Modified)],
            "array elements have no stable identity; only the array key should change");
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Null and empty values
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn null_to_string_value_reports_modified() {
        let changes = json_diff(
            "{\n  \"key\": null\n}",
            "{\n  \"key\": \"value\"\n}",
        );
        assert_eq!(names(&changes), vec![("key".into(), ChangeType::Modified)]);
    }

    #[test]
    fn empty_object_gains_child_reports_both_parent_and_child() {
        // The precision guard keeps `key` Modified — its declaration shape
        // changed from `{}` to `{...}`.
        let changes = json_diff(
            "{\n  \"key\": {}\n}",
            "{\n  \"key\": {\n    \"build\": \"tsc\"\n  }\n}",
        );
        let key = find_change(&changes, "key", ChangeType::Modified);
        assert_eq!(key.parent_name, None);
        let build = find_change(&changes, "build", ChangeType::Added);
        assert_eq!(build.parent_name.as_deref(), Some("key"));
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Entity ID format — file::pointer (no entity_type)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn entity_id_for_nested_property_uses_full_pointer_only() {
        let changes = json_diff(
            "{\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}",
            "{\n  \"scripts\": {\n    \"build\": \"webpack\"\n  }\n}",
        );
        let build = find_change(&changes, "build", ChangeType::Modified);
        assert_eq!(build.entity_id, "test.json::/scripts/build");
    }

    #[test]
    fn key_with_slash_is_pointer_escaped_in_entity_id() {
        let changes = json_diff(
            "{\n  \"a/b\": 1\n}",
            "{\n  \"a/b\": 2\n}",
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].entity_id, "test.json::/a~1b");
    }

    #[test]
    fn key_with_tilde_is_pointer_escaped_in_entity_id() {
        let changes = json_diff(
            "{\n  \"a~b\": 1\n}",
            "{\n  \"a~b\": 2\n}",
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].entity_id, "test.json::/a~0b");
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Phase 3 fuzzy matching
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn fuzzy_rename_detected_when_value_mostly_unchanged() {
        // config → settings: key rename (Phase 1 & 2 miss).
        // testTimeout 30 → 60: small value change rules out structural_hash.
        // Many siblings unchanged → Jaccard > 0.8 → Phase 3 catches it.
        let before = r#"{
  "config": {
    "host": "localhost",
    "protocol": "https",
    "retries": 3,
    "testTimeout": 30,
    "keepalive": true,
    "compression": true,
    "logging": "verbose",
    "maxConnections": 100
  }
}"#;
        let after = r#"{
  "settings": {
    "host": "localhost",
    "protocol": "https",
    "retries": 3,
    "testTimeout": 60,
    "keepalive": true,
    "compression": true,
    "logging": "verbose",
    "maxConnections": 100
  }
}"#;
        let changes = json_diff(before, after);
        assert!(changes.iter().any(|c| c.entity_name == "settings" && c.change_type == ChangeType::Renamed),
            "expected fuzzy rename of config → settings; got: {:?}", names(&changes));
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Known limitations (documented in spec)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn parent_rename_with_sibling_added_surfaces_leaf_moves() {
        // Parent renamed AND a new sibling appears: structural_hash diverges,
        // Phase 2 misses the parent rename. The unchanged child still matches
        // by structural_hash and surfaces as Moved; the parent Deleted/Added
        // entries are container-suppressed.
        let before = r#"{
  "scripts": {
    "build": "tsc"
  }
}"#;
        let after = r#"{
  "tasks": {
    "build": "tsc",
    "test": "jest"
  }
}"#;
        let changes = json_diff(before, after);
        let build = find_change(&changes, "build", ChangeType::Moved);
        assert_eq!(build.parent_name.as_deref(), Some("tasks"));
        assert!(build.old_parent_id.is_some());
        find_change(&changes, "test", ChangeType::Added);
        assert!(!changes.iter().any(|c| c.entity_name == "scripts" || c.entity_name == "tasks"),
            "parent Deleted/Added should be suppressed; got: {:?}", names(&changes));
    }

    #[test]
    fn scalar_to_array_transition_reports_modified_only() {
        // Arrays are opaque so no children are produced on either side.
        let changes = json_diff(
            "{\n  \"deps\": \"react\"\n}",
            "{\n  \"deps\": [\"react\", \"vue\"]\n}",
        );
        assert_eq!(names(&changes), vec![("deps".into(), ChangeType::Modified)]);
        assert_eq!(changes[0].entity_type, "array");
    }

    #[test]
    fn array_to_scalar_transition_reports_modified_only() {
        let changes = json_diff(
            "{\n  \"deps\": [\"react\", \"vue\"]\n}",
            "{\n  \"deps\": \"react\"\n}",
        );
        assert_eq!(names(&changes), vec![("deps".into(), ChangeType::Modified)]);
        assert_eq!(changes[0].entity_type, "property");
    }

    #[test]
    fn object_to_array_transition_reports_modified_plus_old_children_deleted() {
        let changes = json_diff(
            "{\n  \"deps\": {\n    \"react\": \"18\"\n  }\n}",
            "{\n  \"deps\": [\"react\"]\n}",
        );
        let deps = find_change(&changes, "deps", ChangeType::Modified);
        assert_eq!(deps.entity_type, "array");
        find_change(&changes, "react", ChangeType::Deleted);
    }

    #[test]
    fn array_to_object_transition_reports_modified_plus_new_children_added() {
        let changes = json_diff(
            "{\n  \"deps\": [\"react\"]\n}",
            "{\n  \"deps\": {\n    \"react\": \"18\"\n  }\n}",
        );
        let deps = find_change(&changes, "deps", ChangeType::Modified);
        assert_eq!(deps.entity_type, "object");
        let react = find_change(&changes, "react", ChangeType::Added);
        assert_eq!(react.parent_name.as_deref(), Some("deps"));
    }

    #[test]
    fn deep_whole_section_deleted_only_leaf_reported() {
        let changes = json_diff(
            "{\n  \"jest\": {\n    \"config\": {\n      \"testTimeout\": 5000\n    }\n  }\n}",
            "{}",
        );
        let timeout = find_change(&changes, "testTimeout", ChangeType::Deleted);
        assert_eq!(timeout.parent_name.as_deref(), Some("jest::config"));
        assert!(!changes.iter().any(|c| c.entity_name == "jest" || c.entity_name == "config"),
            "intermediate containers should be suppressed; got: {:?}", names(&changes));
    }

    #[test]
    fn key_with_both_tilde_and_slash_is_pointer_escaped_in_correct_order() {
        // Per RFC 6901, '~' must be escaped before '/' so 'a~/b' becomes
        // 'a~0~1b' — not 'a~01b' which would happen if '/' were escaped first.
        let changes = json_diff(
            "{\n  \"a~/b\": 1\n}",
            "{\n  \"a~/b\": 2\n}",
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].entity_id, "test.json::/a~0~1b");
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Document-level edge cases
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn root_array_document_produces_no_entities() {
        let plugin = JsonParserPlugin;
        let entities = plugin.extract_entities("[1, 2, 3]", "test.json");
        assert!(entities.is_empty());
    }

    #[test]
    fn root_scalar_document_produces_no_entities() {
        let plugin = JsonParserPlugin;
        assert!(plugin.extract_entities("\"hello\"", "test.json").is_empty());
        assert!(plugin.extract_entities("42", "test.json").is_empty());
        assert!(plugin.extract_entities("null", "test.json").is_empty());
    }

    #[test]
    fn empty_root_object_produces_no_entities() {
        let plugin = JsonParserPlugin;
        assert!(plugin.extract_entities("{}", "test.json").is_empty());
    }

    #[test]
    fn parent_rename_with_child_value_change_falls_back_to_leaf_delete_add() {
        let changes = json_diff(
            "{\n  \"scripts\": {\n    \"dev\": \"vite\"\n  }\n}\n",
            "{\n  \"tasks\": {\n    \"dev\": \"rollup\"\n  }\n}\n",
        );
        find_change(&changes, "dev", ChangeType::Deleted);
        find_change(&changes, "dev", ChangeType::Added);
        assert!(!changes.iter().any(|c| c.change_type == ChangeType::Renamed),
            "rename should not be detectable; got: {:?}", names(&changes));
    }
}
