use rayon::prelude::*;
use serde::Serialize;

use crate::git::types::FileChange;
use crate::model::change::{ChangeType, SemanticChange};
use crate::model::entity::SemanticEntity;
use crate::model::identity::match_entities;
use crate::parser::registry::ParserRegistry;
use std::collections::HashSet;

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
            let all_entities: Vec<&SemanticEntity> =
                before_entities.iter().chain(after_entities.iter()).collect();
            suppress_redundant_parents(&mut result.changes, &all_entities);

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

/// Remove "Modified" parent entities from the change list when at least one
/// child entity also appears as a change.  This avoids showing e.g. an impl
/// block as modified when the real change is in a method inside it.
/// Only suppresses container entity types (impl, trait, module) where the
/// parent is just a wrapper. Functions, structs, etc. are never suppressed
/// because they have independent meaningful content.
fn suppress_redundant_parents(
    changes: &mut Vec<SemanticChange>,
    entities: &[&SemanticEntity],
) {
    if changes.len() < 2 {
        return;
    }

    // Container types whose only purpose is grouping child entities.
    // Functions, structs, enums etc. are NOT containers because they have
    // independent meaningful content (body logic, fields, variants).
    const CONTAINER_TYPES: &[&str] = &[
        "impl", "trait", "module", "class", "interface", "mixin",
        "extension", "namespace", "export", "package",
        "svelte_instance_script", "svelte_module_script",
    ];

    // Build set of entity IDs that have changes
    let changed_ids: HashSet<&str> = changes.iter().map(|c| c.entity_id.as_str()).collect();

    // Find parent entity IDs that should be suppressed: a parent is redundant
    // when at least one of its children also has a change and the parent is a
    // container type (impl, trait, module).
    let mut suppress: HashSet<String> = HashSet::new();
    for entity in entities {
        if let Some(ref pid) = entity.parent_id {
            if changed_ids.contains(entity.id.as_str()) && changed_ids.contains(pid.as_str()) {
                suppress.insert(pid.clone());
            }
        }
    }

    if !suppress.is_empty() {
        changes.retain(|c| {
            !(matches!(c.change_type, ChangeType::Modified | ChangeType::Added | ChangeType::Deleted)
                && suppress.contains(&c.entity_id)
                && CONTAINER_TYPES.contains(&c.entity_type.as_str()))
        });
    }
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
        file_path: file.file_path.clone(),
        old_entity_name: None,
        old_file_path: None,
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
