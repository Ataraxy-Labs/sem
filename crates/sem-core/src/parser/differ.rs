#[cfg(feature = "parallel")]
use rayon::prelude::*;
use serde::Serialize;

use crate::git::types::FileChange;

macro_rules! maybe_par_iter {
    ($slice:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $slice.par_iter()
        }
        #[cfg(not(feature = "parallel"))]
        {
            $slice.iter()
        }
    }};
}
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
    pub total_entities_before: usize,
    pub total_entities_after: usize,
}

pub fn compute_semantic_diff(
    file_changes: &[FileChange],
    registry: &ParserRegistry,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> DiffResult {
    // Process files in parallel: each file's entity extraction and matching is independent
    let per_file_changes: Vec<(String, Vec<SemanticChange>, usize, usize)> =
        maybe_par_iter!(file_changes)
            .filter_map(|file| {
                let content_hint = file
                    .after_content
                    .as_deref()
                    .or(file.before_content.as_deref())
                    .unwrap_or("");
                let resolved = registry.resolve_file_path(&file.file_path);
                let detection_path = resolved.as_deref().unwrap_or(&file.file_path);
                let plugin = registry.get_plugin_with_content(detection_path, content_hint)?;

                let before_entities = if let Some(ref content) = file.before_content {
                    let before_path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                    let before_resolved = registry.resolve_file_path(before_path);
                    let before_detection = before_resolved.as_deref().unwrap_or(before_path);
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        plugin.extract_entities(content, before_detection)
                    })) {
                        Ok(entities) => entities,
                        Err(_) => Vec::new(),
                    }
                } else {
                    Vec::new()
                };

                let after_entities = if let Some(ref content) = file.after_content {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        plugin.extract_entities(content, detection_path)
                    })) {
                        Ok(entities) => entities,
                        Err(_) => Vec::new(),
                    }
                } else {
                    Vec::new()
                };

                let before_count = before_entities.len();
                let after_count = after_entities.len();

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
                    Some((
                        file.file_path.clone(),
                        result.changes,
                        before_count,
                        after_count,
                    ))
                }
            })
            .collect();

    let mut all_changes: Vec<SemanticChange> = Vec::new();
    let mut files_with_changes: HashSet<String> = HashSet::new();
    let mut total_entities_before: usize = 0;
    let mut total_entities_after: usize = 0;
    for (file_path, changes, before_count, after_count) in per_file_changes {
        files_with_changes.insert(file_path);
        all_changes.extend(changes);
        total_entities_before += before_count;
        total_entities_after += after_count;
    }

    // Single-pass counting. Orphans are first-class changes for the
    // change-type buckets, and orphan_count is cross-cutting metadata.
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
        total_entities_before,
        total_entities_after,
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
        "impl",
        "trait",
        "module",
        "class",
        "interface",
        "mixin",
        "extension",
        "namespace",
        "export",
        "package",
        "field",
        "svelte_instance_script",
        "svelte_module_script",
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
        if !matches!(
            change.change_type,
            ChangeType::Modified | ChangeType::Added | ChangeType::Deleted
        ) {
            continue;
        }
        if !CONTAINER_TYPES.contains(&change.entity_type.as_str()) {
            continue;
        }
        let eid = change.entity_id.as_str();
        let b_children = before_children
            .get(eid)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let a_children = after_children.get(eid).map(|v| v.as_slice()).unwrap_or(&[]);

        let has_changed_child = b_children
            .iter()
            .any(|c| changed_ids.contains(c.id.as_str()))
            || a_children
                .iter()
                .any(|c| changed_ids.contains(c.id.as_str()));
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
            before
                .iter()
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
                && c.old_parent_id
                    .as_deref()
                    .map_or(false, |pid| renamed_before_ids.contains(pid)))
        });
    }
}

fn strip_children_content(
    content: &str,
    parent_start_line: usize,
    children: &[&SemanticEntity],
) -> String {
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
    lines
        .iter()
        .enumerate()
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

    let before_orphans = orphan_segments(before_text, &before_covered);
    let after_orphans = orphan_segments(after_text, &after_covered);
    let mut changes = Vec::new();

    for (before_idx, after_idx) in orphan_segment_change_pairs(&before_orphans, &after_orphans) {
        let before_orphan = before_idx.and_then(|idx| before_orphans.get(idx));
        let after_orphan = after_idx.and_then(|idx| after_orphans.get(idx));
        let before_content = orphan_content(before_orphan);
        let after_content = orphan_content(after_orphan);

        // Skip if orphan content is unchanged, including blank-only segments.
        if before_content == after_content {
            continue;
        }

        let change_type = if before_content.is_none() {
            ChangeType::Added
        } else if after_content.is_none() {
            ChangeType::Deleted
        } else {
            ChangeType::Modified
        };

        let current_orphan = match change_type {
            ChangeType::Deleted => before_orphan,
            _ => after_orphan.or(before_orphan),
        };
        let Some(current_orphan) = current_orphan else {
            continue;
        };
        let span_label = if change_type == ChangeType::Deleted {
            "oldL"
        } else {
            "L"
        };
        let orphan_id = format!(
            "{}::orphan::{}@{}{}-{}",
            file.file_path,
            change_type,
            span_label,
            current_orphan.start_line,
            current_orphan.end_line
        );

        changes.push(SemanticChange {
            id: format!("change::{orphan_id}"),
            entity_id: orphan_id,
            change_type,
            entity_type: "orphan".to_string(),
            entity_name: "module-level".to_string(),
            entity_line: current_orphan.start_line,
            start_line: current_orphan.start_line,
            end_line: current_orphan.end_line,
            old_start_line: before_orphan.map(|orphan| orphan.start_line),
            old_end_line: before_orphan.map(|orphan| orphan.end_line),
            parent_name: None,
            file_path: file.file_path.clone(),
            old_entity_name: None,
            old_file_path: None,
            old_parent_id: None,
            before_content: before_content.map(str::to_string),
            after_content: after_content.map(str::to_string),
            commit_sha: commit_sha.map(String::from),
            author: author.map(String::from),
            timestamp: None,
            structural_change: Some(true),
        });
    }

    changes
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanSegment {
    start_line: usize,
    end_line: usize,
    content: String,
}

fn orphan_segments(text: &str, covered_lines: &HashSet<usize>) -> Vec<OrphanSegment> {
    let mut segments = Vec::new();
    let mut current_start: Option<usize> = None;
    let mut current_lines: Vec<&str> = Vec::new();
    let mut last_line_number = 0;

    for (i, line) in text.lines().enumerate() {
        let line_number = i + 1;
        last_line_number = line_number;
        if covered_lines.contains(&line_number) {
            if let Some(start_line) = current_start.take() {
                segments.push(OrphanSegment {
                    start_line,
                    end_line: line_number - 1,
                    content: current_lines.join("\n"),
                });
                current_lines.clear();
            }
            continue;
        }

        current_start.get_or_insert(line_number);
        current_lines.push(line);
    }

    if let Some(start_line) = current_start {
        segments.push(OrphanSegment {
            start_line,
            end_line: last_line_number.max(start_line),
            content: current_lines.join("\n"),
        });
    }

    segments
}

fn orphan_content(segment: Option<&OrphanSegment>) -> Option<&str> {
    segment
        .map(|segment| segment.content.as_str())
        .filter(|content| !content.trim().is_empty())
}

fn orphan_segment_change_pairs(
    before: &[OrphanSegment],
    after: &[OrphanSegment],
) -> Vec<(Option<usize>, Option<usize>)> {
    let anchors = orphan_segment_lcs(before, after);
    let mut pairs = Vec::new();
    let mut before_start = 0;
    let mut after_start = 0;

    for (before_anchor, after_anchor) in anchors {
        append_orphan_gap_pairs(
            &mut pairs,
            before_start,
            before_anchor,
            after_start,
            after_anchor,
        );
        before_start = before_anchor + 1;
        after_start = after_anchor + 1;
    }

    append_orphan_gap_pairs(
        &mut pairs,
        before_start,
        before.len(),
        after_start,
        after.len(),
    );

    pairs
}

fn append_orphan_gap_pairs(
    pairs: &mut Vec<(Option<usize>, Option<usize>)>,
    before_start: usize,
    before_end: usize,
    after_start: usize,
    after_end: usize,
) {
    let before_len = before_end.saturating_sub(before_start);
    let after_len = after_end.saturating_sub(after_start);

    if before_len == after_len {
        for i in 0..before_len {
            pairs.push((Some(before_start + i), Some(after_start + i)));
        }
        return;
    }

    for i in 0..before_len {
        pairs.push((Some(before_start + i), None));
    }
    for i in 0..after_len {
        pairs.push((None, Some(after_start + i)));
    }
}

fn orphan_segment_lcs(before: &[OrphanSegment], after: &[OrphanSegment]) -> Vec<(usize, usize)> {
    let mut dp = vec![vec![0; after.len() + 1]; before.len() + 1];

    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            dp[i][j] = if orphan_segments_equal(&before[i], &after[j]) {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut anchors = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < before.len() && j < after.len() {
        if orphan_segments_equal(&before[i], &after[j]) {
            anchors.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }

    anchors
}

fn orphan_segments_equal(before: &OrphanSegment, after: &OrphanSegment) -> bool {
    match (orphan_content(Some(before)), orphan_content(Some(after))) {
        (Some(before), Some(after)) => before == after,
        _ => false,
    }
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

    fn renamed_file(old_path: &str, new_path: &str, before: &str, after: &str) -> FileChange {
        FileChange {
            file_path: new_path.to_string(),
            status: FileStatus::Renamed,
            old_file_path: Some(old_path.to_string()),
            before_content: Some(before.to_string()),
            after_content: Some(after.to_string()),
        }
    }

    fn entity_span(id: &str, start_line: usize, end_line: usize) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: "a.rs".to_string(),
            entity_type: "function".to_string(),
            name: id.to_string(),
            parent_id: None,
            content: String::new(),
            content_hash: String::new(),
            structural_hash: None,
            start_line,
            end_line,
            metadata: None,
        }
    }

    #[test]
    fn orphan_only_change_counts_file_and_orphan() {
        let before = "# old module comment\n\ndef value():\n    return 1\n";
        let after = "# new module comment\n\ndef value():\n    return 1\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("app.py", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.file_count, 1);
        assert_eq!(result.orphan_count, 1);
        assert_eq!(result.modified_count, 1);
        assert_eq!(result.changes[0].entity_type, "orphan");
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
    }

    #[test]
    fn test_parent_suppressed_when_only_child_modified() {
        let before = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id)\n";
        let after  = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id, include_deleted=False)\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("svc.py", before, after)],
            &registry,
            None,
            None,
        );

        let names: Vec<&str> = result
            .changes
            .iter()
            .map(|c| c.entity_name.as_str())
            .collect();
        assert!(
            result.changes.iter().any(|c| c.entity_name == "get_user"),
            "expected method get_user in changes, got: {names:?}"
        );
        assert!(
            !result
                .changes
                .iter()
                .any(|c| c.entity_name == "UserService" && c.change_type == ChangeType::Modified),
            "class should be suppressed when only the method body changed, got: {names:?}"
        );
    }

    #[test]
    fn test_parent_not_suppressed_when_own_declaration_changes() {
        let before = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id)\n";
        let after  = "class UserService(BaseService):\n    def get_user(self, user_id):\n        return db.find(user_id, include_deleted=False)\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("svc.py", before, after)],
            &registry,
            None,
            None,
        );

        let names: Vec<&str> = result
            .changes
            .iter()
            .map(|c| c.entity_name.as_str())
            .collect();
        assert!(
            result.changes.iter().any(|c| c.entity_name == "get_user"),
            "expected method get_user in changes, got: {names:?}"
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_name == "UserService" && c.change_type == ChangeType::Modified),
            "class should remain Modified when its own declaration changed, got: {names:?}"
        );
    }

    #[test]
    fn test_nested_typescript_class_field_diff_reports_leaf_method() {
        let before = r#"class L1 {
  L2 = class {
    L3 = class {
      L4 = class {
        method() { return 1; }
      };
    };
  };
}
"#;
        let after = r#"class L1 {
  L2 = class {
    L3 = class {
      L4 = class {
        method() { return 999; }
      };
    };
  };
}
"#;

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("a.ts", before, after)],
            &registry,
            None,
            None,
        );

        let changes: Vec<_> = result
            .changes
            .iter()
            .map(|c| (c.entity_name.as_str(), c.entity_type.as_str()))
            .collect();
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_id == "a.ts::class::L1::L2::L3::L4::method"),
            "expected method leaf change, got: {changes:?}"
        );
        assert!(
            !result.changes.iter().any(|c| c.entity_type == "field"),
            "field containers should be suppressed when only a nested method changed, got: {changes:?}"
        );
    }

    #[test]
    fn renamed_file_with_edited_entity_reports_move_not_add_delete() {
        let before = "def foo():\n    return alpha + beta + gamma\n";
        let after = "def foo():\n    return one + two + three\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[renamed_file("old.py", "new.py", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.added_count, 0);
        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.moved_count, 1);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].entity_name, "foo");
        assert_eq!(result.changes[0].old_file_path.as_deref(), Some("old.py"));
    }

    #[test]
    fn duplicate_markdown_heading_reports_first_section_modification() {
        let before = "# Same Title\n\noriginal content of section A\n\n# Same Title\n\ncontent of section B\n";
        let after = "# Same Title\n\nMODIFIED content of section A\n\n# Same Title\n\ncontent of section B\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("doc.md", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.modified_count, 1, "{:?}", result.changes);
        assert_eq!(result.changes.len(), 1, "{:?}", result.changes);

        let change = &result.changes[0];
        assert_eq!(change.change_type, ChangeType::Modified);
        assert_eq!(change.entity_name, "Same Title");
        assert_eq!(change.entity_line, 1);
        assert!(change
            .before_content
            .as_deref()
            .unwrap_or_default()
            .contains("original content of section A"));
        assert!(change
            .after_content
            .as_deref()
            .unwrap_or_default()
            .contains("MODIFIED content of section A"));
    }

    #[test]
    fn orphan_changes_count_toward_change_type_buckets() {
        let before = "def foo():\n    return 1\n\ndef bar():\n    return 2\n";
        let after = "# just a comment\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("svc.py", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.added_count, 1);
        assert_eq!(result.deleted_count, 2);
        assert_eq!(result.modified_count, 0);
        assert_eq!(result.orphan_count, 1);
        assert!(result
            .changes
            .iter()
            .any(|c| c.entity_type == "orphan" && c.change_type == ChangeType::Added));

        let named_bucket_total = result.added_count
            + result.modified_count
            + result.deleted_count
            + result.moved_count
            + result.renamed_count
            + result.reordered_count;
        assert_eq!(named_bucket_total, result.changes.len());
    }

    #[test]
    fn orphan_changes_use_contiguous_line_spans() {
        let file = modified_file(
            "a.rs",
            "use alpha;\nfn foo() {}\nuse beta;\nfn bar() {}\n",
            "use gamma;\nfn foo() {}\nuse delta;\nfn bar() {}\n",
        );
        let entities = vec![entity_span("foo", 2, 2), entity_span("bar", 4, 4)];

        let changes = detect_orphan_changes(&file, &entities, &entities, None, None);

        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].start_line, 1);
        assert_eq!(changes[0].end_line, 1);
        assert_eq!(changes[0].old_start_line, Some(1));
        assert_eq!(changes[0].old_end_line, Some(1));
        assert_eq!(changes[0].before_content.as_deref(), Some("use alpha;"));
        assert_eq!(changes[0].after_content.as_deref(), Some("use gamma;"));
        assert_eq!(changes[1].start_line, 3);
        assert_eq!(changes[1].end_line, 3);
        assert_eq!(changes[1].old_start_line, Some(3));
        assert_eq!(changes[1].old_end_line, Some(3));
        assert_eq!(changes[1].before_content.as_deref(), Some("use beta;"));
        assert_eq!(changes[1].after_content.as_deref(), Some("use delta;"));
    }

    #[test]
    fn blank_only_orphan_segments_are_ignored() {
        let file = modified_file("a.rs", "fn foo() {}\n", "\nfn foo() {}\n");
        let before_entities = vec![entity_span("foo", 1, 1)];
        let after_entities = vec![entity_span("foo", 2, 2)];

        let changes =
            detect_orphan_changes(&file, &before_entities, &after_entities, None, None);

        assert!(changes.is_empty());
    }

    #[test]
    fn inserted_orphan_segment_does_not_modify_unchanged_later_segment() {
        let file = modified_file(
            "a.rs",
            "fn foo() {}\nuse a;\nfn bar() {}\n",
            "use x;\nfn foo() {}\nuse a;\nfn bar() {}\n",
        );
        let before_entities = vec![entity_span("foo", 1, 1), entity_span("bar", 3, 3)];
        let after_entities = vec![entity_span("foo", 2, 2), entity_span("bar", 4, 4)];

        let changes =
            detect_orphan_changes(&file, &before_entities, &after_entities, None, None);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Added);
        assert_eq!(changes[0].start_line, 1);
        assert_eq!(changes[0].end_line, 1);
        assert!(changes[0].old_start_line.is_none());
        assert_eq!(changes[0].before_content, None);
        assert_eq!(changes[0].after_content.as_deref(), Some("use x;"));
    }

    #[test]
    fn uneven_orphan_gaps_are_not_forced_into_modifications() {
        let file = modified_file(
            "a.rs",
            "use a;\nfn foo() {}\nuse old;\nfn mid() {}\nuse c;\nfn bar() {}\n",
            "use a;\nfn foo() {}\nuse new1;\nfn mid() {}\nuse new2;\nfn baz() {}\nuse c;\nfn bar() {}\n",
        );
        let before_entities = vec![
            entity_span("foo", 2, 2),
            entity_span("mid", 4, 4),
            entity_span("bar", 6, 6),
        ];
        let after_entities = vec![
            entity_span("foo", 2, 2),
            entity_span("mid", 4, 4),
            entity_span("baz", 6, 6),
            entity_span("bar", 8, 8),
        ];

        let changes =
            detect_orphan_changes(&file, &before_entities, &after_entities, None, None);

        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].change_type, ChangeType::Deleted);
        assert!(changes[0].entity_id.contains("::deleted@oldL3-3"));
        assert_eq!(changes[0].before_content.as_deref(), Some("use old;"));
        assert_eq!(changes[1].change_type, ChangeType::Added);
        assert_eq!(changes[1].after_content.as_deref(), Some("use new1;"));
        assert_eq!(changes[2].change_type, ChangeType::Added);
        assert_eq!(changes[2].after_content.as_deref(), Some("use new2;"));
    }
}
