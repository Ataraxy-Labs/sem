//! WASI component plugin for semantic entity-level change detection.
//!
//! Compiles to wasm32-wasip2 and exports the Lix plugin interface:
//! - `detect-changes`: extracts entities from before/after file content, diffs at entity level
//! - `apply-changes`: reconstructs file bytes from entity snapshots

wit_bindgen::generate!({
    path: "wit/lix-plugin.wit",
    world: "plugin",
});

use exports::lix::plugin::api::{
    DetectStateContext, EntityChange, File, Guest, PluginError,
};

use std::sync::OnceLock;

use sem_core::git::types::{FileChange, FileStatus};
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;

/// Cached registry — initialized once, reused across all detect_changes calls.
fn registry() -> &'static ParserRegistry {
    static REGISTRY: OnceLock<ParserRegistry> = OnceLock::new();
    REGISTRY.get_or_init(create_default_registry)
}

struct SemPlugin;

export!(SemPlugin);

impl Guest for SemPlugin {
    fn detect_changes(
        before: Option<File>,
        after: File,
        _state_context: Option<DetectStateContext>,
    ) -> Result<Vec<EntityChange>, PluginError> {
        let after_str = String::from_utf8(after.data)
            .map_err(|e| PluginError::InvalidInput(format!("invalid UTF-8 in after: {e}")))?;

        let before_str = match &before {
            Some(f) => Some(
                String::from_utf8(f.data.clone())
                    .map_err(|e| PluginError::InvalidInput(format!("invalid UTF-8 in before: {e}")))?,
            ),
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
                Some(after_str)
            },
        };

        let result = compute_semantic_diff(&[file_change], registry(), None, None);

        let changes = result
            .changes
            .into_iter()
            .map(|c| {
                let snapshot = match c.change_type {
                    ChangeType::Deleted => None,
                    _ => {
                        let content = serde_json::json!({
                            "id": c.entity_id,
                            "entity_type": c.entity_type,
                            "entity_name": c.entity_name,
                            "file_path": c.file_path,
                            "line": c.entity_line,
                            "content": c.after_content,
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

        Ok(changes)
    }

    fn apply_changes(file: File, _changes: Vec<EntityChange>) -> Result<Vec<u8>, PluginError> {
        // Source code cannot be trivially reconstructed from entity snapshots.
        // Return the file data as-is — sem is a read-only analysis plugin.
        Ok(file.data)
    }
}
