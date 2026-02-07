use std::collections::{HashMap, HashSet};

use super::change::{ChangeType, SemanticChange};
use super::entity::SemanticEntity;

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
    similarity_fn: Option<&dyn Fn(&SemanticEntity, &SemanticEntity) -> f64>,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> MatchResult {
    let mut changes: Vec<SemanticChange> = Vec::new();
    let mut matched_before: HashSet<String> = HashSet::new();
    let mut matched_after: HashSet<String> = HashSet::new();

    let before_by_id: HashMap<&str, &SemanticEntity> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();
    let after_by_id: HashMap<&str, &SemanticEntity> =
        after.iter().map(|e| (e.id.as_str(), e)).collect();

    // Phase 1: Exact ID match
    for (id, after_entity) in &after_by_id {
        if let Some(before_entity) = before_by_id.get(id) {
            matched_before.insert(id.to_string());
            matched_after.insert(id.to_string());

            if before_entity.content_hash != after_entity.content_hash {
                changes.push(SemanticChange {
                    id: format!("change::{id}"),
                    entity_id: id.to_string(),
                    change_type: ChangeType::Modified,
                    entity_type: after_entity.entity_type.clone(),
                    entity_name: after_entity.name.clone(),
                    file_path: after_entity.file_path.clone(),
                    old_file_path: None,
                    before_content: Some(before_entity.content.clone()),
                    after_content: Some(after_entity.content.clone()),
                    commit_sha: commit_sha.map(String::from),
                    author: author.map(String::from),
                    timestamp: None,
                });
            }
        }
    }

    // Collect unmatched
    let unmatched_before: Vec<&SemanticEntity> = before
        .iter()
        .filter(|e| !matched_before.contains(&e.id))
        .collect();
    let unmatched_after: Vec<&SemanticEntity> = after
        .iter()
        .filter(|e| !matched_after.contains(&e.id))
        .collect();

    // Phase 2: Content hash match (rename/move detection)
    // Also uses structural_hash — if formatting/comments changed but logic is same,
    // structural_hash still matches (inspired by Unison content-addressed model).
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
        if matched_after.contains(&after_entity.id) {
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
                        .position(|e| !matched_before.contains(&e.id))
                        .map(|i| c.remove(i))
                })
            })
        });

        if let Some(before_entity) = found {
            matched_before.insert(before_entity.id.clone());
            matched_after.insert(after_entity.id.clone());

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

            changes.push(SemanticChange {
                id: format!("change::{}", after_entity.id),
                entity_id: after_entity.id.clone(),
                change_type,
                entity_type: after_entity.entity_type.clone(),
                entity_name: after_entity.name.clone(),
                file_path: after_entity.file_path.clone(),
                old_file_path,
                before_content: Some(before_entity.content.clone()),
                after_content: Some(after_entity.content.clone()),
                commit_sha: commit_sha.map(String::from),
                author: author.map(String::from),
                timestamp: None,
            });
        }
    }

    // Phase 3: Fuzzy similarity (>80% threshold)
    let still_unmatched_before: Vec<&SemanticEntity> = unmatched_before
        .iter()
        .filter(|e| !matched_before.contains(&e.id))
        .copied()
        .collect();
    let still_unmatched_after: Vec<&SemanticEntity> = unmatched_after
        .iter()
        .filter(|e| !matched_after.contains(&e.id))
        .copied()
        .collect();

    if let Some(sim_fn) = similarity_fn {
        if !still_unmatched_before.is_empty() && !still_unmatched_after.is_empty() {
            const THRESHOLD: f64 = 0.8;

            for after_entity in &still_unmatched_after {
                let mut best_match: Option<&SemanticEntity> = None;
                let mut best_score: f64 = 0.0;

                for before_entity in &still_unmatched_before {
                    if matched_before.contains(&before_entity.id) {
                        continue;
                    }
                    if before_entity.entity_type != after_entity.entity_type {
                        continue;
                    }

                    let score = sim_fn(before_entity, after_entity);
                    if score > best_score && score >= THRESHOLD {
                        best_score = score;
                        best_match = Some(before_entity);
                    }
                }

                if let Some(matched) = best_match {
                    matched_before.insert(matched.id.clone());
                    matched_after.insert(after_entity.id.clone());

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

                    changes.push(SemanticChange {
                        id: format!("change::{}", after_entity.id),
                        entity_id: after_entity.id.clone(),
                        change_type,
                        entity_type: after_entity.entity_type.clone(),
                        entity_name: after_entity.name.clone(),
                        file_path: after_entity.file_path.clone(),
                        old_file_path,
                        before_content: Some(matched.content.clone()),
                        after_content: Some(after_entity.content.clone()),
                        commit_sha: commit_sha.map(String::from),
                        author: author.map(String::from),
                        timestamp: None,
                    });
                }
            }
        }
    }

    // Remaining unmatched before = deleted
    for entity in before.iter().filter(|e| !matched_before.contains(&e.id)) {
        changes.push(SemanticChange {
            id: format!("change::deleted::{}", entity.id),
            entity_id: entity.id.clone(),
            change_type: ChangeType::Deleted,
            entity_type: entity.entity_type.clone(),
            entity_name: entity.name.clone(),
            file_path: entity.file_path.clone(),
            old_file_path: None,
            before_content: Some(entity.content.clone()),
            after_content: None,
            commit_sha: commit_sha.map(String::from),
            author: author.map(String::from),
            timestamp: None,
        });
    }

    // Remaining unmatched after = added
    for entity in after.iter().filter(|e| !matched_after.contains(&e.id)) {
        changes.push(SemanticChange {
            id: format!("change::added::{}", entity.id),
            entity_id: entity.id.clone(),
            change_type: ChangeType::Added,
            entity_type: entity.entity_type.clone(),
            entity_name: entity.name.clone(),
            file_path: entity.file_path.clone(),
            old_file_path: None,
            before_content: None,
            after_content: Some(entity.content.clone()),
            commit_sha: commit_sha.map(String::from),
            author: author.map(String::from),
            timestamp: None,
        });
    }

    MatchResult { changes }
}

/// Default content similarity using Jaccard index on whitespace-split tokens
pub fn default_similarity(a: &SemanticEntity, b: &SemanticEntity) -> f64 {
    let tokens_a: HashSet<&str> = a.content.split_whitespace().collect();
    let tokens_b: HashSet<&str> = b.content.split_whitespace().collect();

    let intersection_size = tokens_a.intersection(&tokens_b).count();
    let union_size = tokens_a.union(&tokens_b).count();

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
    fn test_default_similarity() {
        let a = make_entity("a", "a", "the quick brown fox", "a.ts");
        let b = make_entity("b", "b", "the quick brown dog", "a.ts");
        let score = default_similarity(&a, &b);
        assert!(score > 0.5);
        assert!(score < 1.0);
    }
}
