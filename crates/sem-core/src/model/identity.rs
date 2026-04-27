use std::collections::{HashMap, HashSet};

use super::change::{ChangeType, SemanticChange};
use super::entity::SemanticEntity;

pub struct MatchResult {
    pub changes: Vec<SemanticChange>,
}

fn classify_match(before: &SemanticEntity, after: &SemanticEntity) -> ChangeType {
    if before.file_path != after.file_path {
        ChangeType::Moved
    } else if before.parent_id != after.parent_id {
        ChangeType::Moved // intra-file scope move (e.g. method moved between classes)
    } else {
        ChangeType::Renamed
    }
}

fn make_change(
    after_entity: &SemanticEntity,
    change_type: ChangeType,
    before_entity: Option<&SemanticEntity>,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> SemanticChange {
    let prefix = match change_type {
        ChangeType::Added => "added::",
        ChangeType::Deleted => "deleted::",
        ChangeType::Reordered => "reordered::",
        _ => "",
    };
    // For deleted entities, use the before entity as the primary source
    let primary = if change_type == ChangeType::Deleted {
        before_entity.unwrap_or(after_entity)
    } else {
        after_entity
    };
    SemanticChange {
        id: format!("change::{prefix}{}", primary.id),
        entity_id: primary.id.clone(),
        change_type,
        entity_type: primary.entity_type.clone(),
        entity_name: primary.name.clone(),
        entity_line: primary.start_line,
        file_path: primary.file_path.clone(),
        old_entity_name: before_entity.and_then(|b| {
            (b.name != after_entity.name).then(|| b.name.clone())
        }),
        old_file_path: before_entity.and_then(|b| {
            (b.file_path != after_entity.file_path).then(|| b.file_path.clone())
        }),
        old_parent_id: before_entity.and_then(|b| {
            (b.parent_id != after_entity.parent_id).then(|| b.parent_id.clone()).flatten()
        }),
        before_content: before_entity.map(|b| b.content.clone()),
        after_content: if change_type == ChangeType::Deleted || change_type == ChangeType::Reordered {
            None
        } else {
            Some(after_entity.content.clone())
        },
        commit_sha: commit_sha.map(String::from),
        author: author.map(String::from),
        timestamp: None,
        structural_change: None,
    }
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
                let mut change = make_change(after_entity, ChangeType::Modified, Some(before_entity), commit_sha, author);
                change.structural_change = match (&before_entity.structural_hash, &after_entity.structural_hash) {
                    (Some(before_sh), Some(after_sh)) => Some(before_sh != after_sh),
                    _ => None,
                };
                changes.push(change);
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

            // If name, file, and parent are the same, only the parent qualifier in the ID changed
            // (e.g. parent class was renamed). Skip — the entity itself is unchanged.
            // But if parent_id differs, this is an intra-file move (e.g. method moved between classes).
            if before_entity.name == after_entity.name
                && before_entity.file_path == after_entity.file_path
                && before_entity.content_hash == after_entity.content_hash
                && before_entity.parent_id == after_entity.parent_id
            {
                continue;
            }

            changes.push(make_change(after_entity, classify_match(before_entity, after_entity), Some(before_entity), commit_sha, author));
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

                // If name, file, and parent are the same, only the parent qualifier changed.
                if matched.name == after_entity.name
                    && matched.file_path == after_entity.file_path
                    && matched.content_hash == after_entity.content_hash
                    && matched.parent_id == after_entity.parent_id
                {
                    continue;
                }

                changes.push(make_change(after_entity, classify_match(matched, after_entity), Some(matched), commit_sha, author));
            }
        }
    }

    // Phase 4: Intra-file reorder detection
    // For entities that matched by exact ID with identical content (unchanged),
    // check if their relative ordering changed within the file.
    detect_reorders(before, after, &matched_before, &matched_after, &mut changes, commit_sha, author);

    // Remaining unmatched before = deleted
    for entity in before.iter().filter(|e| !matched_before.contains(e.id.as_str())) {
        changes.push(make_change(entity, ChangeType::Deleted, Some(entity), commit_sha, author));
    }

    // Remaining unmatched after = added
    for entity in after.iter().filter(|e| !matched_after.contains(e.id.as_str())) {
        changes.push(make_change(entity, ChangeType::Added, None, commit_sha, author));
    }

    // Deduplicate: when a parent (class) is Modified and one or more of its
    // children (methods) are also Modified, drop the parent. The child diffs
    // are more specific and the parent body overlaps with them.
    // Only applies to Modified; Added/Deleted should still show all entities.
    let modified_ids: HashSet<&str> = changes
        .iter()
        .filter(|c| c.change_type == ChangeType::Modified)
        .map(|c| c.entity_id.as_str())
        .collect();

    if modified_ids.len() > 1 {
        let mut parents_to_remove: HashSet<&str> = HashSet::new();
        for entity in after.iter().chain(before.iter()) {
            if let Some(ref pid) = entity.parent_id {
                if modified_ids.contains(entity.id.as_str())
                    && modified_ids.contains(pid.as_str())
                {
                    parents_to_remove.insert(pid.as_str());
                }
            }
        }

        if !parents_to_remove.is_empty() {
            changes.retain(|c| {
                !(c.change_type == ChangeType::Modified
                    && parents_to_remove.contains(c.entity_id.as_str()))
            });
        }
    }

    MatchResult { changes }
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

/// Detect intra-file reordering of unchanged entities.
///
/// Takes entities that matched by exact ID with identical content and checks
/// if their relative ordering changed. Uses longest increasing subsequence
/// (LIS) on the "after" positions to find the minimum set of moved entities.
fn detect_reorders(
    before: &[SemanticEntity],
    after: &[SemanticEntity],
    matched_before: &HashSet<&str>,
    matched_after: &HashSet<&str>,
    changes: &mut Vec<SemanticChange>,
    commit_sha: Option<&str>,
    author: Option<&str>,
) {
    // Collect unchanged entities: matched by ID with same content_hash
    let before_by_id: HashMap<&str, &SemanticEntity> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();

    // Group by file. For each file, collect unchanged entities in their
    // before-order, then look up their after-positions.
    let mut by_file: HashMap<&str, Vec<(&SemanticEntity, &SemanticEntity)>> = HashMap::new();
    for after_entity in after {
        if !matched_after.contains(after_entity.id.as_str()) {
            continue;
        }
        if let Some(before_entity) = before_by_id.get(after_entity.id.as_str()) {
            if !matched_before.contains(before_entity.id.as_str()) {
                continue;
            }
            // Only consider truly unchanged entities (same content)
            if before_entity.content_hash != after_entity.content_hash {
                continue;
            }
            // Only intra-file
            if before_entity.file_path != after_entity.file_path {
                continue;
            }
            by_file
                .entry(after_entity.file_path.as_str())
                .or_default()
                .push((before_entity, after_entity));
        }
    }

    for (_file, pairs) in &mut by_file {
        if pairs.len() < 2 {
            continue;
        }

        // Sort by before start_line to get the "before" ordering
        pairs.sort_by_key(|(b, _)| b.start_line);

        // Map to after start_lines in before-order
        let after_lines: Vec<usize> = pairs.iter().map(|(_, a)| a.start_line).collect();

        // Find LIS indices (entities that stayed in relative order)
        let lis_set = longest_increasing_subsequence_indices(&after_lines);

        // Entities NOT in LIS were reordered
        for (i, (_before_entity, after_entity)) in pairs.iter().enumerate() {
            if lis_set.contains(&i) {
                continue;
            }
            changes.push(make_change(after_entity, ChangeType::Reordered, None, commit_sha, author));
        }
    }
}

/// Find indices that form the longest increasing subsequence.
/// Returns a HashSet of indices in the original array that are part of the LIS.
fn longest_increasing_subsequence_indices(seq: &[usize]) -> HashSet<usize> {
    let n = seq.len();
    if n == 0 {
        return HashSet::new();
    }

    // tails[i] = index in seq of the smallest tail element for IS of length i+1
    let mut tails: Vec<usize> = Vec::new();
    // parent[i] = index of previous element in the LIS ending at seq[i]
    let mut parent: Vec<Option<usize>> = vec![None; n];
    // tail_idx[i] = index in seq that tails[i] points to
    let mut tail_idx: Vec<usize> = Vec::new();

    for i in 0..n {
        let pos = tails.partition_point(|&t| t < seq[i]);
        if pos == tails.len() {
            tails.push(seq[i]);
            tail_idx.push(i);
        } else {
            tails[pos] = seq[i];
            tail_idx[pos] = i;
        }
        parent[i] = if pos > 0 { Some(tail_idx[pos - 1]) } else { None };
    }

    // Trace back to find actual LIS indices
    let mut result = HashSet::new();
    let mut idx = *tail_idx.last().unwrap();
    result.insert(idx);
    while let Some(p) = parent[idx] {
        result.insert(p);
        idx = p;
    }
    result
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
        // Class entity contains the method body in its content.
        // parent_id stores the full entity ID of the parent.
        let class_before = SemanticEntity {
            id: "a.ts::class::DataStack".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "class".to_string(),
            name: "DataStack".to_string(),
            parent_id: None,
            content: "class DataStack { constructor() {} genPg() { old } }".to_string(),
            content_hash: content_hash("class DataStack { constructor() {} genPg() { old } }"),
            structural_hash: None,
            start_line: 1,
            end_line: 10,
            metadata: None,
        };
        let method_before = SemanticEntity {
            id: "a.ts::a.ts::class::DataStack::genPg".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "method".to_string(),
            name: "genPg".to_string(),
            parent_id: Some("a.ts::class::DataStack".to_string()),
            content: "genPg() { old }".to_string(),
            content_hash: content_hash("genPg() { old }"),
            structural_hash: None,
            start_line: 5,
            end_line: 8,
            metadata: None,
        };

        let class_after = SemanticEntity {
            id: "a.ts::class::DataStack".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "class".to_string(),
            name: "DataStack".to_string(),
            parent_id: None,
            content: "class DataStack { constructor() {} genPg() { new } }".to_string(),
            content_hash: content_hash("class DataStack { constructor() {} genPg() { new } }"),
            structural_hash: None,
            start_line: 1,
            end_line: 10,
            metadata: None,
        };
        let method_after = SemanticEntity {
            id: "a.ts::a.ts::class::DataStack::genPg".to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "method".to_string(),
            name: "genPg".to_string(),
            parent_id: Some("a.ts::class::DataStack".to_string()),
            content: "genPg() { new }".to_string(),
            content_hash: content_hash("genPg() { new }"),
            structural_hash: None,
            start_line: 5,
            end_line: 8,
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

    fn make_entity_with_parent(id: &str, name: &str, content: &str, file_path: &str, parent_id: Option<&str>) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: file_path.to_string(),
            entity_type: "method".to_string(),
            name: name.to_string(),
            parent_id: parent_id.map(String::from),
            content: content.to_string(),
            content_hash: content_hash(content),
            structural_hash: None,
            start_line: 1,
            end_line: 1,
            metadata: None,
        }
    }

    #[test]
    fn test_intra_file_move_between_classes() {
        // Method moves from ClassA to ClassB in the same file
        let before = vec![make_entity_with_parent(
            "a.rs::class::ClassA::foo", "foo", "fn foo() { do_thing() }",
            "a.rs", Some("a.rs::class::ClassA"),
        )];
        let after = vec![make_entity_with_parent(
            "a.rs::class::ClassB::foo", "foo", "fn foo() { do_thing() }",
            "a.rs", Some("a.rs::class::ClassB"),
        )];
        let result = match_entities(&before, &after, "a.rs", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Moved);
        assert_eq!(result.changes[0].old_parent_id, Some("a.rs::class::ClassA".to_string()));
    }

    #[test]
    fn test_same_parent_is_rename_not_move() {
        // Same parent, different name = rename (not move)
        // Content must be identical (same hash) so Phase 2 catches it
        let body = "fn method(&self) { let x = self.compute(); self.validate(x); self.store(x) }";
        let before = vec![make_entity_with_parent(
            "a.rs::class::Foo::old_method", "old_method", body,
            "a.rs", Some("a.rs::class::Foo"),
        )];
        let after = vec![make_entity_with_parent(
            "a.rs::class::Foo::new_method", "new_method", body,
            "a.rs", Some("a.rs::class::Foo"),
        )];
        let result = match_entities(&before, &after, "a.rs", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Renamed);
        assert!(result.changes[0].old_parent_id.is_none());
    }

    fn make_entity_at(id: &str, name: &str, content: &str, file_path: &str, line: usize) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: file_path.to_string(),
            entity_type: "function".to_string(),
            name: name.to_string(),
            parent_id: None,
            content: content.to_string(),
            content_hash: content_hash(content),
            structural_hash: None,
            start_line: line,
            end_line: line + 2,
            metadata: None,
        }
    }

    #[test]
    fn test_reorder_detection() {
        let before = vec![
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 5),
            make_entity_at("a::f::gamma", "gamma", "fn gamma() {}", "a.rs", 9),
        ];
        let after = vec![
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::gamma", "gamma", "fn gamma() {}", "a.rs", 5),
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 9),
        ];
        let result = match_entities(&before, &after, "a.rs", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Reordered);
        // Either beta or gamma is marked, LIS picks the minimum
        assert!(result.changes[0].entity_name == "beta" || result.changes[0].entity_name == "gamma");
    }

    #[test]
    fn test_no_reorder_when_order_preserved() {
        let before = vec![
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 5),
        ];
        let after = vec![
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 10),
        ];
        let result = match_entities(&before, &after, "a.rs", None, None, None);
        // Lines shifted but relative order is same, no reorder
        assert_eq!(result.changes.len(), 0);
    }

    #[test]
    fn test_default_similarity() {
        let a = make_entity("a", "a", "the quick brown fox", "a.ts");
        let b = make_entity("b", "b", "the quick brown dog", "a.ts");
        let score = default_similarity(&a, &b);
        assert!(score > 0.5);
        assert!(score < 1.0);
    }
}
