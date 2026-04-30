use rayon::prelude::*;
use serde::Serialize;

use crate::git::types::FileChange;
use crate::model::change::{ChangeType, SemanticChange};
use crate::model::entity::SemanticEntity;
use crate::model::identity::match_entities;
use crate::parser::registry::ParserRegistry;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub changes: Vec<SemanticChange>,
    pub file_count: usize,
    pub added_count: usize,
    pub modified_count: usize,
    pub deleted_count: usize,
    pub moved_count: usize,
    pub renamed_count: usize,
    pub reordered_count: usize,
    pub orphan_count: usize,
}

pub fn compute_semantic_diff(
    file_changes: &[FileChange],
    registry: &ParserRegistry,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> DiffResult {
    // Process files in parallel: each file's entity extraction and matching is independent
    let per_file_changes: Vec<(String, Vec<SemanticChange>)> = file_changes
        .par_iter()
        .filter_map(|file| {
            let content_hint = file.after_content.as_deref()
                .or(file.before_content.as_deref())
                .unwrap_or("");
            let plugin = registry.get_plugin_with_content(&file.file_path, content_hint)?;

            let before_entities = if let Some(ref content) = file.before_content {
                let before_path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    plugin.extract_entities(content, before_path)
                })) {
                    Ok(entities) => entities,
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            };

            let after_entities = if let Some(ref content) = file.after_content {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    plugin.extract_entities(content, &file.file_path)
                })) {
                    Ok(entities) => entities,
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            };

            let sim_fn = |a: &crate::model::entity::SemanticEntity,
                          b: &crate::model::entity::SemanticEntity|
             -> f64 { plugin.compute_similarity(a, b) };

            let mut result = match_entities(
                &before_entities,
                &after_entities,
                &file.file_path,
                Some(&sim_fn),
                commit_sha,
                author,
            );

            // Suppress parent entities whose modification is already explained
            // by child entity changes (e.g. impl blocks when methods changed).
            suppress_redundant_parents(&mut result.changes, &before_entities, &after_entities);

            // Detect orphan changes (lines that changed outside any entity span).
            let orphans = detect_orphan_changes(
                file,
                &before_entities,
                &after_entities,
                commit_sha,
                author,
            );
            result.changes.extend(orphans);

            result.changes.sort_by_key(|change| change.entity_line);

            if result.changes.is_empty() {
                None
            } else {
                Some((file.file_path.clone(), result.changes))
            }
        })
        .collect();

    let mut all_changes: Vec<SemanticChange> = Vec::new();
    let mut files_with_changes: HashSet<String> = HashSet::new();
    for (file_path, changes) in per_file_changes {
        files_with_changes.insert(file_path);
        all_changes.extend(changes);
    }

    // Single-pass counting (exclude orphan changes from entity counts)
    let mut added_count = 0;
    let mut modified_count = 0;
    let mut deleted_count = 0;
    let mut moved_count = 0;
    let mut renamed_count = 0;
    let mut reordered_count = 0;
    let mut orphan_count = 0;

    for c in &all_changes {
        if c.entity_type == "orphan" {
            orphan_count += 1;
            continue;
        }
        match c.change_type {
            ChangeType::Added => added_count += 1,
            ChangeType::Modified => modified_count += 1,
            ChangeType::Deleted => deleted_count += 1,
            ChangeType::Moved => moved_count += 1,
            ChangeType::Renamed => renamed_count += 1,
            ChangeType::Reordered => reordered_count += 1,
        }
    }

    DiffResult {
        changes: all_changes,
        file_count: files_with_changes.len(),
        added_count,
        modified_count,
        deleted_count,
        moved_count,
        renamed_count,
        reordered_count,
        orphan_count,
    }
}

fn suppress_redundant_parents(
    changes: &mut Vec<SemanticChange>,
    before: &[SemanticEntity],
    after: &[SemanticEntity],
) {
    if changes.len() < 2 {
        return;
    }

    const CONTAINER_TYPES: &[&str] = &[
        "impl", "trait", "module", "class", "interface", "mixin",
        "extension", "namespace", "export", "package",
        "svelte_instance_script", "svelte_module_script",
        "object",
    ];

    let before_by_id: HashMap<&str, &SemanticEntity> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();
    let after_by_id: HashMap<&str, &SemanticEntity> =
        after.iter().map(|e| (e.id.as_str(), e)).collect();

    let mut before_children: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    for e in before {
        if let Some(ref pid) = e.parent_id {
            before_children.entry(pid.as_str()).or_default().push(e);
        }
    }
    let mut after_children: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    for e in after {
        if let Some(ref pid) = e.parent_id {
            after_children.entry(pid.as_str()).or_default().push(e);
        }
    }

    let changed_ids: HashSet<&str> = changes.iter().map(|c| c.entity_id.as_str()).collect();

    let mut suppress: HashSet<String> = HashSet::new();
    for change in changes.iter() {
        if !matches!(change.change_type, ChangeType::Modified | ChangeType::Added | ChangeType::Deleted) {
            continue;
        }
        if !CONTAINER_TYPES.contains(&change.entity_type.as_str()) {
            continue;
        }
        let eid = change.entity_id.as_str();
        let b_children = before_children.get(eid).map(|v| v.as_slice()).unwrap_or(&[]);
        let a_children = after_children.get(eid).map(|v| v.as_slice()).unwrap_or(&[]);

        let has_changed_child = b_children.iter().any(|c| changed_ids.contains(c.id.as_str()))
            || a_children.iter().any(|c| changed_ids.contains(c.id.as_str()));
        if !has_changed_child {
            continue;
        }

        // Added/Deleted: suppress unconditionally; the children carry the detail.
        // Modified: only suppress if the container's own declaration is unchanged
        // and the value type didn't transition.
        let should_suppress = if change.change_type == ChangeType::Modified {
            match (before_by_id.get(eid), after_by_id.get(eid)) {
                (Some(bp), Some(ap)) if bp.entity_type == ap.entity_type => {
                    let before_own = strip_children_content(&bp.content, bp.start_line, b_children);
                    let after_own = strip_children_content(&ap.content, ap.start_line, a_children);
                    before_own == after_own
                }
                _ => false,
            }
        } else {
            true
        };

        if should_suppress {
            suppress.insert(change.entity_id.clone());
        }
    }

    // Suppress an old parent that a Moved child left behind when the old
    // parent itself appears as a change — handles the parent-rename case
    // where the parent itself failed to match.
    for change in changes.iter() {
        if change.change_type == ChangeType::Moved {
            if let Some(ref old_pid) = change.old_parent_id {
                if changed_ids.contains(old_pid.as_str()) {
                    suppress.insert(old_pid.clone());
                }
            }
        }
    }

    if !suppress.is_empty() {
        changes.retain(|c| !suppress.contains(&c.entity_id));
    }

    // Drop a Moved child whose key is unchanged and whose old parent matches
    // a Renamed entity — the child only "moved" because the parent renamed.
    let renamed_before_ids: HashSet<&str> = changes
        .iter()
        .filter(|c| c.change_type == ChangeType::Renamed)
        .filter_map(|c| {
            let old_name = c.old_entity_name.as_deref()?;
            let after_entity = after_by_id.get(c.entity_id.as_str())?;
            before.iter()
                .find(|e| {
                    e.name == old_name
                        && e.entity_type == after_entity.entity_type
                        && e.parent_id == after_entity.parent_id
                })
                .map(|e| e.id.as_str())
        })
        .collect();

    if !renamed_before_ids.is_empty() {
        changes.retain(|c| {
            !(c.change_type == ChangeType::Moved
                && c.old_entity_name.is_none()
                && c.old_parent_id.as_deref()
                    .map_or(false, |pid| renamed_before_ids.contains(pid)))
        });
    }
}

fn strip_children_content(content: &str, parent_start_line: usize, children: &[&SemanticEntity]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut excluded: HashSet<usize> = HashSet::new();
    for child in children {
        let start_idx = child.start_line.saturating_sub(parent_start_line);
        let end_idx = child.end_line.saturating_sub(parent_start_line);
        for i in start_idx..=end_idx.max(start_idx) {
            if i < lines.len() {
                excluded.insert(i);
            }
        }
    }
    lines.iter().enumerate()
        .filter(|(i, _)| !excluded.contains(i))
        .map(|(_, l)| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Detect changes in lines that fall outside any entity span.
/// These are things like use statements, crate-level attributes, standalone
/// comments, and macro invocations that aren't tracked as entities.
fn detect_orphan_changes(
    file: &FileChange,
    before_entities: &[SemanticEntity],
    after_entities: &[SemanticEntity],
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> Vec<SemanticChange> {
    let before_text = file.before_content.as_deref().unwrap_or("");
    let after_text = file.after_content.as_deref().unwrap_or("");

    // Build covered line sets from entity spans
    let before_covered: HashSet<usize> = before_entities
        .iter()
        .flat_map(|e| e.start_line..=e.end_line)
        .collect();
    let after_covered: HashSet<usize> = after_entities
        .iter()
        .flat_map(|e| e.start_line..=e.end_line)
        .collect();

    // Extract uncovered lines, preserving line numbers for context
    let before_orphan: String = before_text
        .lines()
        .enumerate()
        .filter(|(i, _)| !before_covered.contains(&(i + 1)))
        .map(|(_, l)| l)
        .collect::<Vec<_>>()
        .join("\n");
    let after_orphan: String = after_text
        .lines()
        .enumerate()
        .filter(|(i, _)| !after_covered.contains(&(i + 1)))
        .map(|(_, l)| l)
        .collect::<Vec<_>>()
        .join("\n");

    // Skip if orphan content is unchanged
    if before_orphan == after_orphan {
        return Vec::new();
    }

    let change_type = if before_orphan.trim().is_empty() {
        ChangeType::Added
    } else if after_orphan.trim().is_empty() {
        ChangeType::Deleted
    } else {
        ChangeType::Modified
    };

    vec![SemanticChange {
        id: format!("{}::orphan", file.file_path),
        entity_id: format!("{}::orphan", file.file_path),
        change_type,
        entity_type: "orphan".to_string(),
        entity_name: "module-level".to_string(),
        entity_line: 0,
        parent_name: None,
        file_path: file.file_path.clone(),
        old_entity_name: None,
        old_file_path: None,
        old_parent_id: None,
        before_content: if before_orphan.is_empty() {
            None
        } else {
            Some(before_orphan)
        },
        after_content: if after_orphan.is_empty() {
            None
        } else {
            Some(after_orphan)
        },
        commit_sha: commit_sha.map(String::from),
        author: author.map(String::from),
        timestamp: None,
        structural_change: Some(true),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{FileChange, FileStatus};
    use crate::parser::plugins::create_default_registry;

    fn modified_file(path: &str, before: &str, after: &str) -> FileChange {
        FileChange {
            file_path: path.to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(before.to_string()),
            after_content: Some(after.to_string()),
        }
    }

    #[test]
    fn test_parent_suppressed_when_only_child_modified() {
        let before = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id)\n";
        let after  = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id, include_deleted=False)\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(&[modified_file("svc.py", before, after)], &registry, None, None);

        let names: Vec<&str> = result.changes.iter().map(|c| c.entity_name.as_str()).collect();
        assert!(
            result.changes.iter().any(|c| c.entity_name == "get_user"),
            "expected method get_user in changes, got: {names:?}"
        );
        assert!(
            !result.changes.iter().any(|c| c.entity_name == "UserService" && c.change_type == ChangeType::Modified),
            "class should be suppressed when only the method body changed, got: {names:?}"
        );
    }

    #[test]
    fn test_parent_not_suppressed_when_own_declaration_changes() {
        let before = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id)\n";
        let after  = "class UserService(BaseService):\n    def get_user(self, user_id):\n        return db.find(user_id, include_deleted=False)\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(&[modified_file("svc.py", before, after)], &registry, None, None);

        let names: Vec<&str> = result.changes.iter().map(|c| c.entity_name.as_str()).collect();
        assert!(
            result.changes.iter().any(|c| c.entity_name == "get_user"),
            "expected method get_user in changes, got: {names:?}"
        );
        assert!(
            result.changes.iter().any(|c| c.entity_name == "UserService" && c.change_type == ChangeType::Modified),
            "class should remain Modified when its own declaration changed, got: {names:?}"
        );
    }
}
