use std::collections::{HashMap, HashSet};

use super::change::{ChangeType, SemanticChange};
use super::entity::SemanticEntity;

/// Extracts the leaf name from a parent_id string.
/// parent_id format: "{file_path}::{entity_type}::{name}" (for top-level parents)
/// The name is always the last "::" segment.
fn parent_name(entity: &SemanticEntity) -> Option<String> {
    let pid = entity.parent_id.as_ref()?;
    pid.rsplit("::").next().map(String::from)
}

pub struct MatchResult {
    pub changes: Vec<SemanticChange>,
}

/// 3-phase entity matching algorithm:
/// 1. Exact ID match — same entity ID in before/after → modified or unchanged
/// 2. Content hash match — same hash, different ID → renamed or moved
/// 3. Fuzzy similarity — >80% content similarity → probable rename
pub fn match_entities(
    before: &[SemanticEntity],
    after: &[SemanticEntity],
    _file_path: &str,
    _similarity_fn: Option<&dyn Fn(&SemanticEntity, &SemanticEntity) -> f64>,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> MatchResult {
    let mut changes: Vec<SemanticChange> = Vec::new();
    let mut matched_before: HashSet<&str> = HashSet::new();
    let mut matched_after: HashSet<&str> = HashSet::new();

    let before_by_id: HashMap<&str, &SemanticEntity> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();
    let after_by_id: HashMap<&str, &SemanticEntity> =
        after.iter().map(|e| (e.id.as_str(), e)).collect();

    // Phase 1: Exact ID match
    for (&id, after_entity) in &after_by_id {
        if let Some(before_entity) = before_by_id.get(id) {
            matched_before.insert(id);
            matched_after.insert(id);

            if before_entity.content_hash != after_entity.content_hash {
                let structural_change = match (&before_entity.structural_hash, &after_entity.structural_hash) {
                    (Some(before_sh), Some(after_sh)) => Some(before_sh != after_sh),
                    _ => None,
                };
                changes.push(SemanticChange {
                    id: format!("change::{id}"),
                    entity_id: id.to_string(),
                    change_type: ChangeType::Modified,
                    entity_type: after_entity.entity_type.clone(),
                    entity_name: after_entity.name.clone(),
                    entity_line: after_entity.start_line,
                    parent_name: parent_name(after_entity),
                    file_path: after_entity.file_path.clone(),
                    old_entity_name: None,
                    old_file_path: None,
                    before_content: Some(before_entity.content.clone()),
                    after_content: Some(after_entity.content.clone()),
                    commit_sha: commit_sha.map(String::from),
                    author: author.map(String::from),
                    timestamp: None,
                    structural_change,
                });
            }
        }
    }

    // Collect unmatched
    let unmatched_before: Vec<&SemanticEntity> = before
        .iter()
        .filter(|e| !matched_before.contains(e.id.as_str()))
        .collect();
    let unmatched_after: Vec<&SemanticEntity> = after
        .iter()
        .filter(|e| !matched_after.contains(e.id.as_str()))
        .collect();

    // Phase 2: Content hash match (rename/move detection)
    let mut before_by_hash: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    let mut before_by_structural: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    for entity in &unmatched_before {
        before_by_hash
            .entry(entity.content_hash.as_str())
            .or_default()
            .push(entity);
        if let Some(ref sh) = entity.structural_hash {
            before_by_structural
                .entry(sh.as_str())
                .or_default()
                .push(entity);
        }
    }

    for after_entity in &unmatched_after {
        if matched_after.contains(after_entity.id.as_str()) {
            continue;
        }
        // Try exact content_hash first
        let found = before_by_hash
            .get_mut(after_entity.content_hash.as_str())
            .and_then(|c| c.pop());
        // Fall back to structural_hash (formatting/comment changes don't matter)
        let found = found.or_else(|| {
            after_entity.structural_hash.as_ref().and_then(|sh| {
                before_by_structural.get_mut(sh.as_str()).and_then(|c| {
                    c.iter()
                        .position(|e| !matched_before.contains(e.id.as_str()))
                        .map(|i| c.remove(i))
                })
            })
        });

        if let Some(before_entity) = found {
            matched_before.insert(&before_entity.id);
            matched_after.insert(&after_entity.id);

            // If name and file are the same, only the parent qualifier in the ID changed
            // (e.g. parent class was renamed). Skip — the entity itself is unchanged.
            if before_entity.name == after_entity.name
                && before_entity.file_path == after_entity.file_path
                && before_entity.content_hash == after_entity.content_hash
            {
                continue;
            }

            let change_type = if before_entity.file_path != after_entity.file_path {
                ChangeType::Moved
            } else {
                ChangeType::Renamed
            };

            let old_file_path = if before_entity.file_path != after_entity.file_path {
                Some(before_entity.file_path.clone())
            } else {
                None
            };

            let old_entity_name = if before_entity.name != after_entity.name {
                Some(before_entity.name.clone())
            } else {
                None
            };

            changes.push(SemanticChange {
                id: format!("change::{}", after_entity.id),
                entity_id: after_entity.id.clone(),
                change_type,
                entity_type: after_entity.entity_type.clone(),
                entity_name: after_entity.name.clone(),
                entity_line: after_entity.start_line,
                parent_name: parent_name(after_entity),
                file_path: after_entity.file_path.clone(),
                old_entity_name,
                old_file_path,
                before_content: Some(before_entity.content.clone()),
                after_content: Some(after_entity.content.clone()),
                commit_sha: commit_sha.map(String::from),
                author: author.map(String::from),
                timestamp: None,
                structural_change: None,
            });
        }
    }

    // Phase 3: Fuzzy similarity (>80% threshold)
    // Optimized: pre-compute token sets once per entity, group by type
    let still_unmatched_before: Vec<&SemanticEntity> = unmatched_before
        .iter()
        .filter(|e| !matched_before.contains(e.id.as_str()))
        .copied()
        .collect();
    let still_unmatched_after: Vec<&SemanticEntity> = unmatched_after
        .iter()
        .filter(|e| !matched_after.contains(e.id.as_str()))
        .copied()
        .collect();

    if !still_unmatched_before.is_empty() && !still_unmatched_after.is_empty() {
        const THRESHOLD: f64 = 0.8;
        const SIZE_RATIO_CUTOFF: f64 = 0.5;

        // Pre-compute token sets once per entity (N+M instead of N×M allocations)
        let before_sets: Vec<HashSet<&str>> = still_unmatched_before
            .iter()
            .map(|e| e.content.split_whitespace().collect())
            .collect();
        let after_sets: Vec<HashSet<&str>> = still_unmatched_after
            .iter()
            .map(|e| e.content.split_whitespace().collect())
            .collect();

        // Group before entities by type: O(sum(n_t × m_t)) instead of O(N×M)
        let mut before_by_type: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, e) in still_unmatched_before.iter().enumerate() {
            before_by_type
                .entry(e.entity_type.as_str())
                .or_default()
                .push(i);
        }

        for (ai, after_entity) in still_unmatched_after.iter().enumerate() {
            let candidates = match before_by_type.get(after_entity.entity_type.as_str()) {
                Some(indices) => indices,
                None => continue,
            };

            let a_set = &after_sets[ai];
            let a_len = a_set.len();
            let mut best_idx: Option<usize> = None;
            let mut best_score: f64 = 0.0;

            for &bi in candidates {
                if matched_before.contains(still_unmatched_before[bi].id.as_str()) {
                    continue;
                }

                let b_set = &before_sets[bi];
                let b_len = b_set.len();

                // Size ratio filter using pre-computed set lengths
                let (min_l, max_l) = if a_len < b_len {
                    (a_len, b_len)
                } else {
                    (b_len, a_len)
                };
                if max_l > 0 && (min_l as f64 / max_l as f64) < SIZE_RATIO_CUTOFF {
                    continue;
                }

                // Inline Jaccard on pre-computed sets
                let intersection = a_set.intersection(b_set).count();
                let union = a_len + b_len - intersection;
                let score = if union == 0 {
                    0.0
                } else {
                    intersection as f64 / union as f64
                };

                if score >= THRESHOLD && score > best_score {
                    best_score = score;
                    best_idx = Some(bi);
                }
            }

            if let Some(bi) = best_idx {
                let matched = still_unmatched_before[bi];
                matched_before.insert(&matched.id);
                matched_after.insert(&after_entity.id);

                // If name and file are the same, only the parent qualifier changed.
                if matched.name == after_entity.name
                    && matched.file_path == after_entity.file_path
                    && matched.content_hash == after_entity.content_hash
                {
                    continue;
                }

                let change_type = if matched.file_path != after_entity.file_path {
                    ChangeType::Moved
                } else {
                    ChangeType::Renamed
                };

                let old_file_path = if matched.file_path != after_entity.file_path {
                    Some(matched.file_path.clone())
                } else {
                    None
                };

                let old_entity_name = if matched.name != after_entity.name {
                    Some(matched.name.clone())
                } else {
                    None
                };

                changes.push(SemanticChange {
                    id: format!("change::{}", after_entity.id),
                    entity_id: after_entity.id.clone(),
                    change_type,
                    entity_type: after_entity.entity_type.clone(),
                    entity_name: after_entity.name.clone(),
                    entity_line: after_entity.start_line,
                    parent_name: parent_name(after_entity),
                    file_path: after_entity.file_path.clone(),
                    old_entity_name,
                    old_file_path,
                    before_content: Some(matched.content.clone()),
                    after_content: Some(after_entity.content.clone()),
                    commit_sha: commit_sha.map(String::from),
                    author: author.map(String::from),
                    timestamp: None,
                    structural_change: None,
                });
            }
        }
    }

    // Remaining unmatched before = deleted
    for entity in before.iter().filter(|e| !matched_before.contains(e.id.as_str())) {
        changes.push(SemanticChange {
            id: format!("change::deleted::{}", entity.id),
            entity_id: entity.id.clone(),
            change_type: ChangeType::Deleted,
            entity_type: entity.entity_type.clone(),
            entity_name: entity.name.clone(),
            entity_line: entity.start_line,
            parent_name: parent_name(entity),
            file_path: entity.file_path.clone(),
            old_entity_name: None,
            old_file_path: None,
            before_content: Some(entity.content.clone()),
            after_content: None,
            commit_sha: commit_sha.map(String::from),
            author: author.map(String::from),
            timestamp: None,
            structural_change: None,
        });
    }

    // Remaining unmatched after = added
    for entity in after.iter().filter(|e| !matched_after.contains(e.id.as_str())) {
        changes.push(SemanticChange {
            id: format!("change::added::{}", entity.id),
            entity_id: entity.id.clone(),
            change_type: ChangeType::Added,
            entity_type: entity.entity_type.clone(),
            entity_name: entity.name.clone(),
            entity_line: entity.start_line,
            parent_name: parent_name(entity),
            file_path: entity.file_path.clone(),
            old_entity_name: None,
            old_file_path: None,
            before_content: None,
            after_content: Some(entity.content.clone()),
            commit_sha: commit_sha.map(String::from),
            author: author.map(String::from),
            timestamp: None,
            structural_change: None,
        });
    }

    suppress_redundant_parent_modified(&mut changes, before, after);

    MatchResult { changes }
}

/// Strips child entity content from a parent entity's content using line numbers,
/// then normalizes whitespace. Returns a string representing only the parent's
/// "own" content (its declaration, signature, etc.) without any child body content.
fn strip_children_content(
    content: &str,
    parent_start_line: usize,
    children: &[&SemanticEntity],
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut excluded: HashSet<usize> = HashSet::new();
    for child in children {
        debug_assert!(
            child.start_line >= parent_start_line,
            "child start_line ({}) < parent start_line ({}): extraction bug",
            child.start_line,
            parent_start_line
        );
        // Convert absolute 1-based line numbers to 0-based indices within this entity's content
        let start_idx = child.start_line.saturating_sub(parent_start_line);
        let end_idx = child.end_line.saturating_sub(parent_start_line);
        for i in start_idx..=end_idx {
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

/// Post-processing pass: remove `Modified` changes for parent entities whose
/// modification is entirely a side-effect of their children changing.
///
/// Example: renaming a method inside a class causes the class `content_hash` to
/// change (because class content includes method bodies), but the class's own
/// declaration line didn't change — only a child did. This function suppresses
/// that spurious `Modified` entry so the output shows only the child's change.
fn suppress_redundant_parent_modified(
    changes: &mut Vec<SemanticChange>,
    before: &[SemanticEntity],
    after: &[SemanticEntity],
) {
    let before_by_id: HashMap<&str, &SemanticEntity> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();
    let after_by_id: HashMap<&str, &SemanticEntity> =
        after.iter().map(|e| (e.id.as_str(), e)).collect();

    // Map parent entity ID → its direct children in before/after
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

    // All entity IDs that appear in any change (across before and after)
    let changed_ids: HashSet<&str> = changes.iter().map(|c| c.entity_id.as_str()).collect();

    let mut to_suppress: HashSet<String> = HashSet::new();

    for change in changes.iter() {
        if change.change_type != ChangeType::Modified {
            continue;
        }
        let eid = change.entity_id.as_str();

        let b_children = before_children.get(eid).map(|v| v.as_slice()).unwrap_or(&[]);
        let a_children = after_children.get(eid).map(|v| v.as_slice()).unwrap_or(&[]);

        // Only consider container entities (those that have children)
        if b_children.is_empty() && a_children.is_empty() {
            continue;
        }

        // At least one child must have a change to justify suppression
        let has_changed_child = b_children.iter().any(|c| changed_ids.contains(c.id.as_str()))
            || a_children.iter().any(|c| changed_ids.contains(c.id.as_str()));
        if !has_changed_child {
            continue;
        }

        let before_parent = match before_by_id.get(eid) {
            Some(e) => e,
            None => continue,
        };
        let after_parent = match after_by_id.get(eid) {
            Some(e) => e,
            None => continue,
        };

        // Strip child content from both sides and compare what remains.
        // If the parent's own content (declaration, fields, etc.) is unchanged,
        // the Modified is purely a consequence of child changes — suppress it.
        let before_own = strip_children_content(
            &before_parent.content,
            before_parent.start_line,
            b_children,
        );
        let after_own = strip_children_content(
            &after_parent.content,
            after_parent.start_line,
            a_children,
        );

        if before_own == after_own {
            to_suppress.insert(change.entity_id.clone());
        }
    }

    changes.retain(|c| {
        !(c.change_type == ChangeType::Modified && to_suppress.contains(&c.entity_id))
    });
}

/// Default content similarity using Jaccard index on whitespace-split tokens
pub fn default_similarity(a: &SemanticEntity, b: &SemanticEntity) -> f64 {
    let tokens_a: Vec<&str> = a.content.split_whitespace().collect();
    let tokens_b: Vec<&str> = b.content.split_whitespace().collect();

    // Early rejection: if token counts differ too much, Jaccard can't reach 0.8
    let (min_c, max_c) = if tokens_a.len() < tokens_b.len() {
        (tokens_a.len(), tokens_b.len())
    } else {
        (tokens_b.len(), tokens_a.len())
    };
    if max_c > 0 && (min_c as f64 / max_c as f64) < 0.6 {
        return 0.0;
    }

    let set_a: HashSet<&str> = tokens_a.into_iter().collect();
    let set_b: HashSet<&str> = tokens_b.into_iter().collect();

    let intersection_size = set_a.intersection(&set_b).count();
    let union_size = set_a.union(&set_b).count();

    if union_size == 0 {
        return 0.0;
    }

    intersection_size as f64 / union_size as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::hash::content_hash;

    fn make_entity(id: &str, name: &str, content: &str, file_path: &str) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: file_path.to_string(),
            entity_type: "function".to_string(),
            name: name.to_string(),
            parent_id: None,
            content: content.to_string(),
            content_hash: content_hash(content),
            structural_hash: None,
            start_line: 1,
            end_line: 1,
            metadata: None,
        }
    }

    #[test]
    fn test_exact_match_modified() {
        let before = vec![make_entity("a::f::foo", "foo", "old content", "a.ts")];
        let after = vec![make_entity("a::f::foo", "foo", "new content", "a.ts")];
        let result = match_entities(&before, &after, "a.ts", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
    }

    #[test]
    fn test_exact_match_unchanged() {
        let before = vec![make_entity("a::f::foo", "foo", "same", "a.ts")];
        let after = vec![make_entity("a::f::foo", "foo", "same", "a.ts")];
        let result = match_entities(&before, &after, "a.ts", None, None, None);
        assert_eq!(result.changes.len(), 0);
    }

    #[test]
    fn test_added_deleted() {
        let before = vec![make_entity("a::f::old", "old", "content", "a.ts")];
        let after = vec![make_entity("a::f::new", "new", "different", "a.ts")];
        let result = match_entities(&before, &after, "a.ts", None, None, None);
        assert_eq!(result.changes.len(), 2);
        let types: Vec<ChangeType> = result.changes.iter().map(|c| c.change_type).collect();
        assert!(types.contains(&ChangeType::Deleted));
        assert!(types.contains(&ChangeType::Added));
    }

    #[test]
    fn test_content_hash_rename() {
        let before = vec![make_entity("a::f::old", "old", "same content", "a.ts")];
        let after = vec![make_entity("a::f::new", "new", "same content", "a.ts")];
        let result = match_entities(&before, &after, "a.ts", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Renamed);
    }

    #[test]
    fn test_parent_child_dedup_class_method() {
        // Use realistic multi-line content so line-number-based child stripping works.
        // Line 1: class header, lines 2-3: constructor, lines 4-6: genPg, line 7: closing brace.
        let class_before = SemanticEntity {
            id: "a.ts::class::DataStack".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "class".to_string(),
            name: "DataStack".to_string(),
            parent_id: None,
            content: "class DataStack {\n  constructor() {}\n  genPg() {\n    old\n  }\n}".to_string(),
            content_hash: content_hash("class DataStack {\n  constructor() {}\n  genPg() {\n    old\n  }\n}"),
            structural_hash: None,
            start_line: 1,
            end_line: 6,
            metadata: None,
        };
        let method_before = SemanticEntity {
            id: "a.ts::a.ts::class::DataStack::genPg".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "method".to_string(),
            name: "genPg".to_string(),
            parent_id: Some("a.ts::class::DataStack".to_string()),
            content: "genPg() {\n    old\n  }".to_string(),
            content_hash: content_hash("genPg() {\n    old\n  }"),
            structural_hash: None,
            start_line: 3,
            end_line: 5,
            metadata: None,
        };

        let class_after = SemanticEntity {
            id: "a.ts::class::DataStack".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "class".to_string(),
            name: "DataStack".to_string(),
            parent_id: None,
            content: "class DataStack {\n  constructor() {}\n  genPg() {\n    new\n  }\n}".to_string(),
            content_hash: content_hash("class DataStack {\n  constructor() {}\n  genPg() {\n    new\n  }\n}"),
            structural_hash: None,
            start_line: 1,
            end_line: 6,
            metadata: None,
        };
        let method_after = SemanticEntity {
            id: "a.ts::a.ts::class::DataStack::genPg".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "method".to_string(),
            name: "genPg".to_string(),
            parent_id: Some("a.ts::class::DataStack".to_string()),
            content: "genPg() {\n    new\n  }".to_string(),
            content_hash: content_hash("genPg() {\n    new\n  }"),
            structural_hash: None,
            start_line: 3,
            end_line: 5,
            metadata: None,
        };

        let before = vec![class_before, method_before];
        let after = vec![class_after, method_after];
        let result = match_entities(&before, &after, "a.ts", None, None, None);

        // Should only report the method change, not the class
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].entity_name, "genPg");
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
    }

    #[test]
    fn test_parent_not_deduped_when_no_child_changes() {
        // Only the class-level content changes (e.g. a field added), no method changes
        let class_before = SemanticEntity {
            id: "a.ts::class::Foo".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "class".to_string(),
            name: "Foo".to_string(),
            parent_id: None,
            content: "class Foo { bar() {} }".to_string(),
            content_hash: content_hash("class Foo { bar() {} }"),
            structural_hash: None,
            start_line: 1,
            end_line: 5,
            metadata: None,
        };
        let method_before = SemanticEntity {
            id: "a.ts::a.ts::class::Foo::bar".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "method".to_string(),
            name: "bar".to_string(),
            parent_id: Some("a.ts::class::Foo".to_string()),
            content: "bar() {}".to_string(),
            content_hash: content_hash("bar() {}"),
            structural_hash: None,
            start_line: 2,
            end_line: 4,
            metadata: None,
        };

        let class_after = SemanticEntity {
            id: "a.ts::class::Foo".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "class".to_string(),
            name: "Foo".to_string(),
            parent_id: None,
            content: "class Foo { x = 1; bar() {} }".to_string(),
            content_hash: content_hash("class Foo { x = 1; bar() {} }"),
            structural_hash: None,
            start_line: 1,
            end_line: 6,
            metadata: None,
        };
        let method_after = SemanticEntity {
            id: "a.ts::a.ts::class::Foo::bar".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "method".to_string(),
            name: "bar".to_string(),
            parent_id: Some("a.ts::class::Foo".to_string()),
            content: "bar() {}".to_string(),
            content_hash: content_hash("bar() {}"),
            structural_hash: None,
            start_line: 3,
            end_line: 5,
            metadata: None,
        };

        let before = vec![class_before, method_before];
        let after = vec![class_after, method_after];
        let result = match_entities(&before, &after, "a.ts", None, None, None);

        // Class changed but method didn't, so class should still appear
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].entity_name, "Foo");
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
    }

    #[test]
    fn test_default_similarity() {
        let a = make_entity("a", "a", "the quick brown fox", "a.ts");
        let b = make_entity("b", "b", "the quick brown dog", "a.ts");
        let score = default_similarity(&a, &b);
        assert!(score > 0.5);
        assert!(score < 1.0);
    }

    // ---- suppress_redundant_parent_modified tests ----

    fn make_entity_with_parent(
        id: &str,
        name: &str,
        content: &str,
        file_path: &str,
        parent_id: Option<&str>,
        start_line: usize,
        end_line: usize,
    ) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: file_path.to_string(),
            entity_type: "function".to_string(),
            name: name.to_string(),
            parent_id: parent_id.map(String::from),
            content: content.to_string(),
            content_hash: content_hash(content),
            structural_hash: None,
            start_line,
            end_line,
            metadata: None,
        }
    }

    // A realistic method body with enough tokens that Jaccard similarity stays >0.8
    // even when only the method name differs (one token changes out of ~15 unique).
    const METHOD_BODY: &str =
        "x = 1\n    y = 2\n    z = 3\n    w = x + y\n    return w + z";

    /// When a child is renamed, the parent should NOT appear as Modified.
    #[test]
    fn test_parent_not_modified_when_child_renamed() {
        let before_method_content =
            format!("def old_method(self):\n    {METHOD_BODY}");
        let after_method_content =
            format!("def new_method(self):\n    {METHOD_BODY}");
        let before_class_content =
            format!("class Svc:\n    {before_method_content}");
        let after_class_content =
            format!("class Svc:\n    {after_method_content}");

        let before_class = make_entity_with_parent(
            "a.py::class::Svc", "Svc", &before_class_content, "a.py", None, 1, 6,
        );
        let before_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::old_method",
            "old_method",
            &before_method_content,
            "a.py",
            Some("a.py::class::Svc"),
            2,
            6,
        );
        let after_class = make_entity_with_parent(
            "a.py::class::Svc", "Svc", &after_class_content, "a.py", None, 1, 6,
        );
        let after_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::new_method",
            "new_method",
            &after_method_content,
            "a.py",
            Some("a.py::class::Svc"),
            2,
            6,
        );

        let before = vec![before_class, before_method];
        let after = vec![after_class, after_method];

        let result = match_entities(
            &before,
            &after,
            "a.py",
            Some(&default_similarity),
            None,
            None,
        );

        let types: Vec<ChangeType> = result.changes.iter().map(|c| c.change_type).collect();
        assert!(types.contains(&ChangeType::Renamed), "expected method Renamed");
        assert!(
            !types.contains(&ChangeType::Modified),
            "parent class should not appear as Modified when only child renamed"
        );
    }

    /// When a method is added to a class, the class should NOT appear as Modified.
    #[test]
    fn test_parent_not_modified_when_child_added() {
        let method_content = format!("def bar(self):\n    {METHOD_BODY}");
        let new_method_content = format!("def baz(self):\n    {METHOD_BODY}");

        let before_class = make_entity_with_parent(
            "a.py::class::Svc",
            "Svc",
            &format!("class Svc:\n    {method_content}"),
            "a.py",
            None,
            1,
            6,
        );
        let before_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::bar",
            "bar",
            &method_content,
            "a.py",
            Some("a.py::class::Svc"),
            2,
            6,
        );
        let after_class = make_entity_with_parent(
            "a.py::class::Svc",
            "Svc",
            &format!("class Svc:\n    {method_content}\n    {new_method_content}"),
            "a.py",
            None,
            1,
            12,
        );
        let after_method_bar = make_entity_with_parent(
            "a.py::a.py::class::Svc::bar",
            "bar",
            &method_content,
            "a.py",
            Some("a.py::class::Svc"),
            2,
            6,
        );
        let after_method_baz = make_entity_with_parent(
            "a.py::a.py::class::Svc::baz",
            "baz",
            &new_method_content,
            "a.py",
            Some("a.py::class::Svc"),
            7,
            12,
        );

        let before = vec![before_class, before_method];
        let after = vec![after_class, after_method_bar, after_method_baz];

        let result = match_entities(&before, &after, "a.py", None, None, None);

        let types: Vec<ChangeType> = result.changes.iter().map(|c| c.change_type).collect();
        assert!(types.contains(&ChangeType::Added), "expected new method Added");
        assert!(
            !types.contains(&ChangeType::Modified),
            "parent class should not appear as Modified when only child added"
        );
    }

    /// When the class's own declaration changes (e.g. base class added) in addition
    /// to a child rename, the parent SHOULD remain as Modified.
    #[test]
    fn test_parent_still_modified_when_own_content_changes() {
        let before_method_content =
            format!("def old_method(self):\n    {METHOD_BODY}");
        let after_method_content =
            format!("def new_method(self):\n    {METHOD_BODY}");

        // Before: "class Svc:" — After: "class Svc(Base):" — declaration changed
        let before_class = make_entity_with_parent(
            "a.py::class::Svc",
            "Svc",
            &format!("class Svc:\n    {before_method_content}"),
            "a.py",
            None,
            1,
            6,
        );
        let before_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::old_method",
            "old_method",
            &before_method_content,
            "a.py",
            Some("a.py::class::Svc"),
            2,
            6,
        );
        let after_class = make_entity_with_parent(
            "a.py::class::Svc",
            "Svc",
            &format!("class Svc(Base):\n    {after_method_content}"),
            "a.py",
            None,
            1,
            6,
        );
        let after_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::new_method",
            "new_method",
            &after_method_content,
            "a.py",
            Some("a.py::class::Svc"),
            2,
            6,
        );

        let before = vec![before_class, before_method];
        let after = vec![after_class, after_method];

        let result = match_entities(
            &before,
            &after,
            "a.py",
            Some(&default_similarity),
            None,
            None,
        );

        let types: Vec<ChangeType> = result.changes.iter().map(|c| c.change_type).collect();
        assert!(types.contains(&ChangeType::Renamed), "expected method Renamed");
        assert!(
            types.contains(&ChangeType::Modified),
            "parent class should still be Modified when its own declaration changed"
        );
    }

    /// `parent_name` is None for top-level entities and Some("ClassName") for nested ones.
    #[test]
    fn test_parent_name_populated_on_changes() {
        let method_content = format!("def bar(self):\n    {METHOD_BODY}");

        let before_class = make_entity_with_parent(
            "a.py::class::Svc", "Svc",
            &format!("class Svc:\n    {method_content}"),
            "a.py", None, 1, 6,
        );
        let before_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::bar", "bar", &method_content,
            "a.py", Some("a.py::class::Svc"), 2, 6,
        );

        // Modify the method body so it shows as Modified
        let after_method_content = format!("def bar(self):\n    {METHOD_BODY}\n    return 0");
        let after_class = make_entity_with_parent(
            "a.py::class::Svc", "Svc",
            &format!("class Svc:\n    {after_method_content}"),
            "a.py", None, 1, 7,
        );
        let after_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::bar", "bar", &after_method_content,
            "a.py", Some("a.py::class::Svc"), 2, 7,
        );

        let before = vec![before_class, before_method];
        let after = vec![after_class, after_method];
        let result = match_entities(&before, &after, "a.py", None, None, None);

        let method_change = result.changes.iter()
            .find(|c| c.entity_name == "bar")
            .expect("expected change for bar");

        assert_eq!(method_change.change_type, ChangeType::Modified);
        assert_eq!(
            method_change.parent_name.as_deref(),
            Some("Svc"),
            "nested method should carry parent class name"
        );

        // Top-level entity (the class itself) should not appear due to suppression,
        // but if we check a top-level entity directly it should have no parent_name.
        let top_level = make_entity("a.py::function::standalone", "standalone", "def standalone(): pass", "a.py");
        let top_level_after = make_entity("a.py::function::standalone", "standalone", "def standalone(): return 1", "a.py");
        let top_result = match_entities(&[top_level], &[top_level_after], "a.py", None, None, None);
        assert_eq!(top_result.changes.len(), 1);
        assert_eq!(
            top_result.changes[0].parent_name,
            None,
            "top-level entity should have no parent_name"
        );
    }

    /// When all children are deleted the parent body changes structurally,
    /// so the parent should remain visible as Modified.
    #[test]
    fn test_parent_still_modified_when_all_children_deleted() {
        let method_content = format!("def bar(self):\n    {METHOD_BODY}");

        let before_class = make_entity_with_parent(
            "a.py::class::Svc", "Svc",
            &format!("class Svc:\n    {method_content}"),
            "a.py", None, 1, 6,
        );
        let before_method = make_entity_with_parent(
            "a.py::a.py::class::Svc::bar", "bar", &method_content,
            "a.py", Some("a.py::class::Svc"), 2, 6,
        );

        // After: class body is now just `pass` — completely different from before
        let after_class = make_entity_with_parent(
            "a.py::class::Svc", "Svc",
            "class Svc:\n    pass",
            "a.py", None, 1, 2,
        );

        let before = vec![before_class, before_method];
        let after = vec![after_class];
        let result = match_entities(&before, &after, "a.py", None, None, None);

        let types: Vec<ChangeType> = result.changes.iter().map(|c| c.change_type).collect();
        assert!(types.contains(&ChangeType::Deleted), "method should be Deleted");
        assert!(
            types.contains(&ChangeType::Modified),
            "parent class should remain Modified when all children are deleted and body changes"
        );
    }
}
