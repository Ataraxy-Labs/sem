//! WASI component plugin for semantic entity-level change detection.
//!
//! Compiles to wasm32-wasip2 and exports the Lix plugin interface:
//! - `detect-changes`: extracts entities from before/after file content, diffs at entity level
//! - `apply-changes`: reconstructs file bytes from entity snapshots

#[cfg(any(target_arch = "wasm32", test))]
wit_bindgen::generate!({
    path: "wit/lix-plugin.wit",
    world: "plugin",
});

#[cfg(any(target_arch = "wasm32", test))]
use exports::lix::plugin::api::{DetectStateContext, EntityChange, File, Guest, PluginError};

#[cfg(any(target_arch = "wasm32", test))]
use std::collections::HashMap;
#[cfg(any(target_arch = "wasm32", test))]
use std::sync::OnceLock;

#[cfg(any(target_arch = "wasm32", test))]
use sem_core::git::types::{FileChange, FileStatus};
#[cfg(any(target_arch = "wasm32", test))]
use sem_core::model::change::ChangeType;
#[cfg(any(target_arch = "wasm32", test))]
use sem_core::parser::differ::compute_semantic_diff;
#[cfg(any(target_arch = "wasm32", test))]
use sem_core::parser::plugins::create_default_registry;
#[cfg(any(target_arch = "wasm32", test))]
use sem_core::parser::registry::ParserRegistry;

/// Cached registry — initialized once, reused across all detect_changes calls.
#[cfg(any(target_arch = "wasm32", test))]
fn registry() -> &'static ParserRegistry {
    static REGISTRY: OnceLock<ParserRegistry> = OnceLock::new();
    REGISTRY.get_or_init(create_default_registry)
}

#[cfg(any(target_arch = "wasm32", test))]
struct SemPlugin;

#[cfg(target_arch = "wasm32")]
export!(SemPlugin);

#[cfg(any(target_arch = "wasm32", test))]
impl Guest for SemPlugin {
    fn detect_changes(
        before: Option<File>,
        after: File,
        _state_context: Option<DetectStateContext>,
    ) -> Result<Vec<EntityChange>, PluginError> {
        let after_str = String::from_utf8(after.data)
            .map_err(|e| PluginError::InvalidInput(format!("invalid UTF-8 in after: {e}")))?;

        let before_str =
            match &before {
                Some(f) => Some(String::from_utf8(f.data.clone()).map_err(|e| {
                    PluginError::InvalidInput(format!("invalid UTF-8 in before: {e}"))
                })?),
                None => None,
            };

        let status = if before_str.is_none() {
            FileStatus::Added
        } else if after_str.is_empty() {
            FileStatus::Deleted
        } else {
            FileStatus::Modified
        };

        let file_change = FileChange {
            file_path: after.path.clone(),
            status,
            old_file_path: None,
            before_content: before_str,
            after_content: if after_str.is_empty() {
                None
            } else {
                Some(after_str.clone())
            },
        };

        let result = compute_semantic_diff(&[file_change], registry(), None, None);

        // Extract entities from the after file to get line ranges (start_line, end_line)
        let after_entities = if !after_str.is_empty() {
            registry().extract_entities(&after.path, &after_str)
        } else {
            Vec::new()
        };

        // Build map: start_line → max end_line
        let mut line_to_end: HashMap<usize, usize> = HashMap::new();
        for e in &after_entities {
            line_to_end
                .entry(e.start_line)
                .and_modify(|existing| *existing = (*existing).max(e.end_line))
                .or_insert(e.end_line);
        }

        let after_lines: Vec<&str> = after_str.lines().collect();
        let total_lines = after_lines.len();
        let path = &after.path;

        // Filter out orphan changes, add end_line, use line-based content for roundtripping
        let mut changes: Vec<EntityChange> = result
            .changes
            .into_iter()
            .filter(|c| c.entity_type != "orphan")
            .map(|c| {
                let snapshot = match c.change_type {
                    ChangeType::Deleted => None,
                    _ => {
                        let end_line = line_to_end
                            .get(&c.entity_line)
                            .copied()
                            .unwrap_or(c.entity_line);

                        // Extract content from file lines for correct roundtripping
                        // (entity content from parser may strip leading indentation)
                        let start = c.entity_line.max(1);
                        let end = end_line.min(total_lines);
                        let line_content = if start <= end && start <= total_lines {
                            (start..=end)
                                .map(|l| after_lines[l - 1])
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            c.after_content.unwrap_or_default()
                        };

                        let content = serde_json::json!({
                            "id": c.entity_id,
                            "entity_type": c.entity_type,
                            "entity_name": c.entity_name,
                            "file_path": c.file_path,
                            "line": c.entity_line,
                            "end_line": end_line,
                            "content": line_content,
                        });
                        Some(serde_json::to_string(&content).unwrap_or_default())
                    }
                };

                EntityChange {
                    entity_id: c.entity_id,
                    schema_key: String::from("sem_entity"),
                    snapshot_content: snapshot,
                }
            })
            .collect();

        // Compute gap entities for lines not covered by emitted entity changes
        if !after_str.is_empty() && total_lines > 0 {
            let mut covered = vec![false; total_lines + 1]; // 1-indexed
            for change in &changes {
                if let Some(ref snapshot) = change.snapshot_content {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(snapshot) {
                        let start = parsed["line"].as_u64().unwrap_or(0) as usize;
                        let end = parsed["end_line"].as_u64().unwrap_or(start as u64) as usize;
                        for line in start..=end.min(total_lines) {
                            if line > 0 {
                                covered[line] = true;
                            }
                        }
                    }
                }
            }

            let mut gap_index = 0usize;
            let mut i = 1usize;
            while i <= total_lines {
                if !covered[i] {
                    let start = i;
                    while i <= total_lines && !covered[i] {
                        i += 1;
                    }
                    let end = i - 1;

                    let gap_content: String = (start..=end)
                        .map(|l| after_lines[l - 1])
                        .collect::<Vec<_>>()
                        .join("\n");

                    let gap_id = format!("{path}::gap::{gap_index}");
                    let content = serde_json::json!({
                        "id": gap_id,
                        "entity_type": "gap",
                        "entity_name": format!("gap-{gap_index}"),
                        "file_path": path,
                        "line": start,
                        "end_line": end,
                        "content": gap_content,
                    });

                    changes.push(EntityChange {
                        entity_id: gap_id,
                        schema_key: String::from("sem_entity"),
                        snapshot_content: Some(serde_json::to_string(&content).unwrap_or_default()),
                    });

                    gap_index += 1;
                } else {
                    i += 1;
                }
            }
        }

        Ok(changes)
    }

    fn apply_changes(file: File, changes: Vec<EntityChange>) -> Result<Vec<u8>, PluginError> {
        if changes.is_empty() {
            return Ok(file.data);
        }

        // Parse all snapshot_content JSON, collect (line, end_line, content)
        let mut snapshots: Vec<(usize, usize, String)> = Vec::new();
        for change in &changes {
            if let Some(ref snapshot_json) = change.snapshot_content {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(snapshot_json) {
                    let line = parsed["line"].as_u64().unwrap_or(0) as usize;
                    let end_line = parsed["end_line"].as_u64().unwrap_or(line as u64) as usize;
                    if let Some(content) = parsed["content"].as_str() {
                        snapshots.push((line, end_line, content.to_string()));
                    }
                }
            }
        }

        if snapshots.is_empty() {
            return Ok(Vec::new());
        }

        // Sort by line ascending, then by range size descending (larger ranges first)
        // so that when ranges overlap the outermost snapshot comes first and subsumes children
        snapshots.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));

        // Reconstruct file by concatenating non-overlapping content in line order
        let mut parts: Vec<&str> = Vec::new();
        let mut current_end: usize = 0;
        for (line, end_line, content) in &snapshots {
            if *line > current_end {
                parts.push(content);
                current_end = *end_line;
            }
        }

        let mut result = parts.join("\n");
        if !result.is_empty() {
            result.push('\n');
        }
        Ok(result.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(path: &str, source: &str) {
        let after = File {
            id: String::new(),
            path: path.to_string(),
            data: source.as_bytes().to_vec(),
        };

        let changes = SemPlugin::detect_changes(None, after, None).unwrap();

        // Verify we have both entity and gap changes
        let has_entities = changes
            .iter()
            .filter_map(|c| c.snapshot_content.as_ref())
            .any(|s| {
                serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .and_then(|v| v["entity_type"].as_str().map(|t| t != "gap"))
                    .unwrap_or(false)
            });
        assert!(has_entities, "expected entity changes for {path}");

        let empty_file = File {
            id: String::new(),
            path: path.to_string(),
            data: vec![],
        };

        let reconstructed = SemPlugin::apply_changes(empty_file, changes).unwrap();
        let reconstructed_str = String::from_utf8(reconstructed).unwrap();

        assert_eq!(reconstructed_str, source, "roundtrip failed for {path}");
    }

    #[test]
    fn test_roundtrip_typescript() {
        let source = "import { foo } from 'bar';\n\nexport function hello(name: string): string {\n    return `Hello, ${name}!`;\n}\n\nexport class Greeter {\n    greet(name: string): string {\n        return hello(name);\n    }\n}\n";
        roundtrip("test.ts", source);
    }

    #[test]
    fn test_roundtrip_python() {
        let source = "import os\nfrom pathlib import Path\n\ndef hello(name: str) -> str:\n    return f\"Hello, {name}!\"\n\nclass Greeter:\n    def greet(self, name: str) -> str:\n        return hello(name)\n";
        roundtrip("test.py", source);
    }

    #[test]
    fn test_roundtrip_rust() {
        let source = "use std::fmt;\n\nfn hello(name: &str) -> String {\n    format!(\"Hello, {}!\", name)\n}\n\nstruct Greeter;\n\nimpl Greeter {\n    fn greet(&self, name: &str) -> String {\n        hello(name)\n    }\n}\n";
        roundtrip("test.rs", source);
    }

    #[test]
    fn test_roundtrip_with_comments_and_whitespace() {
        let source = "// Module-level comment\n\nimport { something } from 'somewhere';\n\n// Function documentation\n// Multi-line comment\nexport function process(data: string): string {\n    // inline comment\n    return data.trim();\n}\n\n// Another comment block\nexport const VALUE = 42;\n";
        roundtrip("test_comments.ts", source);
    }
}
