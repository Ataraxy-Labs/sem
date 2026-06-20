use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::change::{ChangeType, SemanticChange};
use super::entity::SemanticEntity;

fn parent_name(entity: &SemanticEntity, by_id: &HashMap<&str, &SemanticEntity>) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut pid = entity.parent_id.as_deref()?;
    loop {
        if !visited.insert(pid) {
            break;
        }
        match by_id.get(pid) {
            Some(parent) => {
                // Skip ancestors with empty names (e.g. JSON's empty-string
                // root-package key in package-lock.json). The full path is
                // still recoverable from entity_id; the displayed chain is
                // for human readability.
                if !parent.name.is_empty() {
                    parts.push(parent.name.as_str());
                }
                match parent.parent_id.as_deref() {
                    Some(next) => pid = next,
                    None => break,
                }
            }
            None => break,
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(parts.join("::"))
}

pub struct MatchResult {
    pub changes: Vec<SemanticChange>,
}

type SameFileSignatureKey<'a> = (&'a str, &'a str, &'a str, Option<&'a str>);
type RenameSignatureKey<'a> = (&'a str, &'a str, Option<&'a str>);
const SAME_FILE_SIGNATURE_MIN_SIMILARITY: f64 = 0.3;

struct ContentTokens<'a> {
    token_count: usize,
    unique_tokens: HashSet<&'a str>,
}

struct TokenCache<'a> {
    tokens: Vec<Option<ContentTokens<'a>>>,
}

impl<'a> TokenCache<'a> {
    fn new(len: usize) -> Self {
        Self {
            tokens: std::iter::repeat_with(|| None).take(len).collect(),
        }
    }

    fn get(&mut self, entities: &[&'a SemanticEntity], idx: usize) -> &ContentTokens<'a> {
        if self.tokens[idx].is_none() {
            let content: &'a str = entities[idx].content.as_str();
            self.tokens[idx] = Some(tokenize_content(content));
        }
        self.tokens[idx].as_ref().unwrap()
    }
}

fn tokenize_content(content: &str) -> ContentTokens<'_> {
    let mut token_count = 0;
    let mut unique_tokens = HashSet::new();
    for token in content.split_whitespace() {
        token_count += 1;
        unique_tokens.insert(token);
    }
    ContentTokens {
        token_count,
        unique_tokens,
    }
}

fn jaccard_similarity(a: &HashSet<&str>, b: &HashSet<&str>) -> f64 {
    let intersection_size = a.intersection(b).count();
    let union_size = a.len() + b.len() - intersection_size;
    if union_size == 0 {
        return 0.0;
    }
    intersection_size as f64 / union_size as f64
}

fn default_similarity_from_tokens(a: &ContentTokens<'_>, b: &ContentTokens<'_>) -> f64 {
    let (min_c, max_c) = if a.token_count < b.token_count {
        (a.token_count, b.token_count)
    } else {
        (b.token_count, a.token_count)
    };
    if max_c > 0 && (min_c as f64 / max_c as f64) < 0.6 {
        return 0.0;
    }
    jaccard_similarity(&a.unique_tokens, &b.unique_tokens)
}

fn classify_match(before: &SemanticEntity, after: &SemanticEntity) -> ChangeType {
    if before.file_path != after.file_path {
        ChangeType::Moved
    } else if before.parent_id != after.parent_id {
        ChangeType::Moved // intra-file scope move (e.g. method moved between classes)
    } else if before.entity_type != after.entity_type || before.name != after.name {
        ChangeType::Renamed
    } else {
        ChangeType::Modified
    }
}

fn structural_change_between(before: &SemanticEntity, after: &SemanticEntity) -> Option<bool> {
    if before.content_hash == after.content_hash {
        return None;
    }

    match (&before.structural_hash, &after.structural_hash) {
        (Some(before_hash), Some(after_hash)) => Some(before_hash != after_hash),
        _ => None,
    }
}

fn make_change(
    after_entity: &SemanticEntity,
    change_type: ChangeType,
    before_entity: Option<&SemanticEntity>,
    commit_sha: Option<&str>,
    author: Option<&str>,
    by_id: &HashMap<&str, &SemanticEntity>,
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
    let structural_change = before_entity.and_then(|before| {
        if matches!(change_type, ChangeType::Deleted | ChangeType::Reordered) {
            None
        } else {
            structural_change_between(before, after_entity)
        }
    });
    SemanticChange {
        id: format!("change::{prefix}{}", primary.id),
        entity_id: primary.id.clone(),
        change_type,
        entity_type: primary.entity_type.clone(),
        entity_name: primary.name.clone(),
        entity_line: primary.start_line,
        start_line: primary.start_line,
        end_line: primary.end_line,
        old_start_line: before_entity.map(|b| b.start_line),
        old_end_line: before_entity.map(|b| b.end_line),
        parent_name: parent_name(primary, by_id),
        file_path: primary.file_path.clone(),
        old_entity_name: before_entity
            .and_then(|b| (b.name != after_entity.name).then(|| b.name.clone())),
        old_file_path: before_entity
            .and_then(|b| (b.file_path != after_entity.file_path).then(|| b.file_path.clone())),
        old_parent_id: before_entity.and_then(|b| {
            (b.parent_id != after_entity.parent_id)
                .then(|| b.parent_id.clone())
                .flatten()
        }),
        before_content: if change_type == ChangeType::Reordered {
            None
        } else {
            before_entity.map(|b| b.content.clone())
        },
        after_content: if change_type == ChangeType::Deleted || change_type == ChangeType::Reordered
        {
            None
        } else {
            Some(after_entity.content.clone())
        },
        commit_sha: commit_sha.map(String::from),
        author: author.map(String::from),
        timestamp: None,
        structural_change,
    }
}

/// Entity matching algorithm:
/// 1. Exact ID match — same entity ID in before/after → modified or unchanged
/// 2. Content hash match — same hash, different ID → modified, renamed, or moved
/// 3. Same signature across file rename → moved, even if content changed
/// 4. Fuzzy similarity — >80% content similarity → modified, renamed, or moved
pub fn match_entities(
    before: &[SemanticEntity],
    after: &[SemanticEntity],
    _file_path: &str,
    similarity_fn: Option<&dyn Fn(&SemanticEntity, &SemanticEntity) -> f64>,
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

    // Combined map for ancestor-chain lookup: after takes precedence so the
    // displayed path reflects the post-change tree for non-deleted entities.
    let combined_by_id: HashMap<&str, &SemanticEntity> = before
        .iter()
        .map(|e| (e.id.as_str(), e))
        .chain(after.iter().map(|e| (e.id.as_str(), e)))
        .collect();

    // Phase 1: Exact ID match
    for (&id, after_entity) in &after_by_id {
        if let Some(before_entity) = before_by_id.get(id) {
            matched_before.insert(id);
            matched_after.insert(id);

            if before_entity.content_hash != after_entity.content_hash {
                changes.push(make_change(
                    after_entity,
                    ChangeType::Modified,
                    Some(before_entity),
                    commit_sha,
                    author,
                    &combined_by_id,
                ));
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
    let mut unmatched_before_tokens = TokenCache::new(unmatched_before.len());
    let mut unmatched_after_tokens = TokenCache::new(unmatched_after.len());

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

            changes.push(make_change(
                after_entity,
                classify_match(before_entity, after_entity),
                Some(before_entity),
                commit_sha,
                author,
                &combined_by_id,
            ));
        }
    }

    // Phase 3: Same logical signature within a file.
    // Collision groups can shrink or grow, changing only the disambiguator
    // portion of an ID. Match those entities before the generic fuzzy pass.
    let unmatched_before_parent_names: Vec<Option<String>> = unmatched_before
        .iter()
        .map(|entity| parent_name(entity, &before_by_id))
        .collect();
    let unmatched_after_parent_names: Vec<Option<String>> = unmatched_after
        .iter()
        .map(|entity| parent_name(entity, &after_by_id))
        .collect();

    let mut before_by_same_file_signature: HashMap<SameFileSignatureKey<'_>, Vec<usize>> =
        HashMap::new();
    for (before_idx, before_entity) in unmatched_before.iter().enumerate() {
        if matched_before.contains(before_entity.id.as_str()) {
            continue;
        }
        let key = (
            before_entity.file_path.as_str(),
            before_entity.entity_type.as_str(),
            before_entity.name.as_str(),
            unmatched_before_parent_names[before_idx].as_deref(),
        );
        before_by_same_file_signature
            .entry(key)
            .or_default()
            .push(before_idx);
    }

    let mut after_by_same_file_signature: HashMap<SameFileSignatureKey<'_>, Vec<usize>> =
        HashMap::new();
    for (after_idx, after_entity) in unmatched_after.iter().enumerate() {
        if matched_after.contains(after_entity.id.as_str()) {
            continue;
        }
        let key = (
            after_entity.file_path.as_str(),
            after_entity.entity_type.as_str(),
            after_entity.name.as_str(),
            unmatched_after_parent_names[after_idx].as_deref(),
        );
        after_by_same_file_signature
            .entry(key)
            .or_default()
            .push(after_idx);
    }

    let mut same_file_keys: Vec<SameFileSignatureKey<'_>> = after_by_same_file_signature
        .keys()
        .copied()
        .filter(|key| before_by_same_file_signature.contains_key(key))
        .collect();
    same_file_keys.sort_unstable();

    for key in same_file_keys {
        let before_indices = &before_by_same_file_signature[&key];
        let after_indices = &after_by_same_file_signature[&key];
        let mut same_file_candidates: Vec<(f64, usize, usize, usize)> = Vec::new();

        for &after_idx in after_indices {
            let after_entity = unmatched_after[after_idx];
            if matched_after.contains(after_entity.id.as_str()) {
                continue;
            }
            for &before_idx in before_indices {
                let before_entity = unmatched_before[before_idx];
                if matched_before.contains(before_entity.id.as_str()) {
                    continue;
                }

                let score = match similarity_fn {
                    Some(f) => f(before_entity, after_entity),
                    None => default_similarity_from_tokens(
                        unmatched_before_tokens.get(&unmatched_before, before_idx),
                        unmatched_after_tokens.get(&unmatched_after, after_idx),
                    ),
                };
                same_file_candidates.push((
                    score,
                    before_entity.start_line.abs_diff(after_entity.start_line),
                    before_idx,
                    after_idx,
                ));
            }
        }

        same_file_candidates.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.cmp(&b.3))
        });

        for (score, _line_distance, before_idx, after_idx) in same_file_candidates {
            if !score.is_finite() || score < SAME_FILE_SIGNATURE_MIN_SIMILARITY {
                continue;
            }
            let before_entity = unmatched_before[before_idx];
            let after_entity = unmatched_after[after_idx];
            if matched_before.contains(before_entity.id.as_str())
                || matched_after.contains(after_entity.id.as_str())
            {
                continue;
            }

            matched_before.insert(before_entity.id.as_str());
            matched_after.insert(after_entity.id.as_str());

            if before_entity.content_hash == after_entity.content_hash {
                continue;
            }

            changes.push(make_change(
                after_entity,
                classify_match(before_entity, after_entity),
                Some(before_entity),
                commit_sha,
                author,
                &combined_by_id,
            ));
        }
    }

    // Phase 4: Same logical signature across a file rename.
    // A file path change changes entity IDs, so renamed files with edited
    // entities need a signature fallback to avoid add/delete pairs.
    let mut before_by_rename_signature: HashMap<RenameSignatureKey<'_>, Vec<usize>> =
        HashMap::new();
    for (before_idx, before_entity) in unmatched_before.iter().enumerate() {
        if matched_before.contains(before_entity.id.as_str()) {
            continue;
        }
        let key = (
            before_entity.entity_type.as_str(),
            before_entity.name.as_str(),
            unmatched_before_parent_names[before_idx].as_deref(),
        );
        before_by_rename_signature
            .entry(key)
            .or_default()
            .push(before_idx);
    }

    for (after_idx, after_entity) in unmatched_after.iter().enumerate() {
        if matched_after.contains(after_entity.id.as_str()) {
            continue;
        }

        let key = (
            after_entity.entity_type.as_str(),
            after_entity.name.as_str(),
            unmatched_after_parent_names[after_idx].as_deref(),
        );
        let Some(before_indices) = before_by_rename_signature.get(&key) else {
            continue;
        };

        let mut best_match: Option<&SemanticEntity> = None;
        let mut best_score = f64::NEG_INFINITY;

        for &before_idx in before_indices {
            let before_entity = unmatched_before[before_idx];
            if matched_before.contains(before_entity.id.as_str()) {
                continue;
            }
            if before_entity.file_path == after_entity.file_path {
                continue;
            }

            let score = match similarity_fn {
                Some(f) => f(before_entity, after_entity),
                None => default_similarity_from_tokens(
                    unmatched_before_tokens.get(&unmatched_before, before_idx),
                    unmatched_after_tokens.get(&unmatched_after, after_idx),
                ),
            };
            if score > best_score {
                best_score = score;
                best_match = Some(before_entity);
            }
        }

        if let Some(before_entity) = best_match {
            matched_before.insert(before_entity.id.as_str());
            matched_after.insert(after_entity.id.as_str());
            changes.push(make_change(
                after_entity,
                classify_match(before_entity, after_entity),
                Some(before_entity),
                commit_sha,
                author,
                &combined_by_id,
            ));
        }
    }

    // Phase 5: Fuzzy similarity (>80% threshold)
    // Cache token sets on demand and group by type.
    let still_unmatched_before: Vec<(usize, &SemanticEntity)> = unmatched_before
        .iter()
        .enumerate()
        .filter(|(_, e)| !matched_before.contains(e.id.as_str()))
        .map(|(i, e)| (i, *e))
        .collect();
    let still_unmatched_after: Vec<(usize, &SemanticEntity)> = unmatched_after
        .iter()
        .enumerate()
        .filter(|(_, e)| !matched_after.contains(e.id.as_str()))
        .map(|(i, e)| (i, *e))
        .collect();

    if !still_unmatched_before.is_empty() && !still_unmatched_after.is_empty() {
        const THRESHOLD: f64 = 0.8;
        const SIZE_RATIO_CUTOFF: f64 = 0.5;

        // Group before entities by type: O(sum(n_t × m_t)) instead of O(N×M)
        let mut before_by_type: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, (_, e)) in still_unmatched_before.iter().enumerate() {
            before_by_type
                .entry(e.entity_type.as_str())
                .or_default()
                .push(i);
        }

        for &(after_unmatched_idx, after_entity) in &still_unmatched_after {
            let candidates = match before_by_type.get(after_entity.entity_type.as_str()) {
                Some(indices) => indices,
                None => continue,
            };

            let a_len = unmatched_after_tokens
                .get(&unmatched_after, after_unmatched_idx)
                .unique_tokens
                .len();
            let mut best_idx: Option<usize> = None;
            let mut best_score: f64 = 0.0;

            for &bi in candidates {
                let (before_unmatched_idx, before_entity) = still_unmatched_before[bi];
                if matched_before.contains(before_entity.id.as_str()) {
                    continue;
                }

                let b_len = unmatched_before_tokens
                    .get(&unmatched_before, before_unmatched_idx)
                    .unique_tokens
                    .len();

                // Size ratio filter using pre-computed set lengths
                let (min_l, max_l) = if a_len < b_len {
                    (a_len, b_len)
                } else {
                    (b_len, a_len)
                };
                if max_l > 0 && (min_l as f64 / max_l as f64) < SIZE_RATIO_CUTOFF {
                    continue;
                }

                // Jaccard on pre-computed sets
                let score = jaccard_similarity(
                    &unmatched_after_tokens
                        .get(&unmatched_after, after_unmatched_idx)
                        .unique_tokens,
                    &unmatched_before_tokens
                        .get(&unmatched_before, before_unmatched_idx)
                        .unique_tokens,
                );

                if score >= THRESHOLD && score > best_score {
                    best_score = score;
                    best_idx = Some(bi);
                }
            }

            if let Some(bi) = best_idx {
                let matched = still_unmatched_before[bi].1;
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

                changes.push(make_change(
                    after_entity,
                    classify_match(matched, after_entity),
                    Some(matched),
                    commit_sha,
                    author,
                    &combined_by_id,
                ));
            }
        }
    }

    // Phase 6: Intra-file reorder detection
    // For entities that matched by exact ID with identical content (unchanged),
    // check if their relative ordering changed within the file.
    detect_reorders(
        before,
        after,
        &matched_before,
        &matched_after,
        &mut changes,
        commit_sha,
        author,
        &combined_by_id,
    );

    // Remaining unmatched before = deleted
    for entity in before
        .iter()
        .filter(|e| !matched_before.contains(e.id.as_str()))
    {
        changes.push(make_change(
            entity,
            ChangeType::Deleted,
            Some(entity),
            commit_sha,
            author,
            &combined_by_id,
        ));
    }

    // Remaining unmatched after = added
    for entity in after
        .iter()
        .filter(|e| !matched_after.contains(e.id.as_str()))
    {
        changes.push(make_change(
            entity,
            ChangeType::Added,
            None,
            commit_sha,
            author,
            &combined_by_id,
        ));
    }

    MatchResult { changes }
}

/// Default content similarity using Jaccard index on whitespace-split tokens
pub fn default_similarity(a: &SemanticEntity, b: &SemanticEntity) -> f64 {
    let tokens_a = tokenize_content(&a.content);
    let tokens_b = tokenize_content(&b.content);
    default_similarity_from_tokens(&tokens_a, &tokens_b)
}

/// Detect intra-file reordering of unchanged entities.
///
/// Takes entities that matched by exact ID with identical content and checks
/// if their relative ordering changed. Uses a longest non-decreasing
/// subsequence on the "after" positions to find the minimum set of moved entities.
fn detect_reorders(
    before: &[SemanticEntity],
    after: &[SemanticEntity],
    matched_before: &HashSet<&str>,
    matched_after: &HashSet<&str>,
    changes: &mut Vec<SemanticChange>,
    commit_sha: Option<&str>,
    author: Option<&str>,
    by_id: &HashMap<&str, &SemanticEntity>,
) {
    // Collect unchanged entities: matched by ID with same content_hash
    let before_by_id: HashMap<&str, &SemanticEntity> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();
    let before_index_by_id: HashMap<&str, usize> = before
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    let after_index_by_id: HashMap<&str, usize> = after
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();

    // Group by file. For each file, collect unchanged entities in their
    // before-order, then look up their after-positions.
    let mut by_file: HashMap<&str, Vec<(&SemanticEntity, &SemanticEntity, usize, usize)>> =
        HashMap::new();
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
            let (Some(&before_index), Some(&after_index)) = (
                before_index_by_id.get(before_entity.id.as_str()),
                after_index_by_id.get(after_entity.id.as_str()),
            ) else {
                continue;
            };
            by_file
                .entry(after_entity.file_path.as_str())
                .or_default()
                .push((before_entity, after_entity, before_index, after_index));
        }
    }

    for (_file, pairs) in &mut by_file {
        if pairs.len() < 2 {
            continue;
        }

        // Sort by before position to get the "before" ordering.
        pairs.sort_by_key(|(b, _, before_index, _)| (b.start_line, *before_index));

        // Map to after positions in before-order. The extraction index gives
        // same-line entities a stable secondary ordering.
        let after_positions: Vec<(usize, usize)> = pairs
            .iter()
            .map(|(_, a, _, after_index)| (a.start_line, *after_index))
            .collect();

        // Find LNDS indices (entities that stayed in relative order).
        let lnds_set = longest_non_decreasing_subsequence_indices(&after_positions);

        // Entities outside the LNDS were reordered.
        for (i, (before_entity, after_entity, _, _)) in pairs.iter().enumerate() {
            if lnds_set.contains(&i) {
                continue;
            }
            changes.push(make_change(
                after_entity,
                ChangeType::Reordered,
                Some(before_entity),
                commit_sha,
                author,
                by_id,
            ));
        }
    }
}

/// Find indices that form the longest non-decreasing subsequence.
/// Returns a HashSet of indices in the original array that are part of the subsequence.
fn longest_non_decreasing_subsequence_indices(seq: &[(usize, usize)]) -> HashSet<usize> {
    let n = seq.len();
    if n == 0 {
        return HashSet::new();
    }

    // tails[i] = smallest tail position for a non-decreasing subsequence of length i+1
    let mut tails: Vec<(usize, usize)> = Vec::new();
    // parent[i] = index of previous element in the subsequence ending at seq[i]
    let mut parent: Vec<Option<usize>> = vec![None; n];
    // tail_idx[i] = index in seq that tails[i] points to
    let mut tail_idx: Vec<usize> = Vec::new();

    for i in 0..n {
        // Non-decreasing subsequences use the first tail greater than the
        // current position, allowing equal positions to extend the sequence.
        let pos = tails.partition_point(|&t| t <= seq[i]);
        if pos == tails.len() {
            tails.push(seq[i]);
            tail_idx.push(i);
        } else {
            tails[pos] = seq[i];
            tail_idx[pos] = i;
        }
        parent[i] = if pos > 0 {
            Some(tail_idx[pos - 1])
        } else {
            None
        };
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
    fn test_change_line_spans_track_current_and_previous_entities() {
        let before = vec![make_entity_at(
            "a::f::foo",
            "foo",
            "fn foo() { old }",
            "a.rs",
            3,
        )];
        let after = vec![make_entity_at(
            "a::f::foo",
            "foo",
            "fn foo() { new }",
            "a.rs",
            7,
        )];

        let result = match_entities(&before, &after, "a.rs", None, None, None);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].start_line, 7);
        assert_eq!(result.changes[0].end_line, 9);
        assert_eq!(result.changes[0].old_start_line, Some(3));
        assert_eq!(result.changes[0].old_end_line, Some(5));
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
    fn test_same_name_fuzzy_match_is_modified() {
        let before = vec![make_entity(
            "a.ts::function::foo@L1",
            "foo",
            "function foo() { const value = input + 1; return process(value); }",
            "a.ts",
        )];
        let after = vec![make_entity(
            "a.ts::function::foo@L2",
            "foo",
            "function foo() { const value = input + 2; return process(value); }",
            "a.ts",
        )];

        let result = match_entities(&before, &after, "a.ts", None, None, None);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
        assert_eq!(result.changes[0].entity_name, "foo");
        assert!(result.changes[0].old_entity_name.is_none());
    }

    #[test]
    fn test_different_name_fuzzy_match_is_renamed() {
        let before = vec![make_entity(
            "a.ts::function::old_name@L1",
            "old_name",
            "function old_name(input: number) { const first = input + 1; const second = first * 2; const third = second - 3; return compute(third, first, second); }",
            "a.ts",
        )];
        let after = vec![make_entity(
            "a.ts::function::new_name@L2",
            "new_name",
            "function new_name(input: number) { const first = input + 1; const second = first * 2; const third = second - 3; return compute(third, first, second); }",
            "a.ts",
        )];

        let result = match_entities(&before, &after, "a.ts", None, None, None);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Renamed);
        assert_eq!(result.changes[0].entity_name, "new_name");
        assert_eq!(
            result.changes[0].old_entity_name.as_deref(),
            Some("old_name")
        );
    }

    #[test]
    fn test_same_signature_file_rename_with_content_change_is_moved() {
        let mut before_entity = make_entity(
            "old.ts::function::foo",
            "foo",
            "export function foo() { return alpha + beta + gamma; }",
            "old.ts",
        );
        before_entity.structural_hash = Some("before-structure".to_string());
        let mut after_entity = make_entity(
            "new.ts::function::foo",
            "foo",
            "export function foo() { return one + two + three; }",
            "new.ts",
        );
        after_entity.structural_hash = Some("after-structure".to_string());
        let before = vec![before_entity];
        let after = vec![after_entity];

        let result = match_entities(&before, &after, "new.ts", None, None, None);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Moved);
        assert_eq!(result.changes[0].old_file_path.as_deref(), Some("old.ts"));
        assert_eq!(result.changes[0].structural_change, Some(true));
    }

    #[test]
    fn test_same_signature_file_rename_keeps_duplicate_names_with_parents() {
        let mut before_alpha_class =
            make_entity("old.ts::class::Alpha", "Alpha", "class Alpha {}", "old.ts");
        before_alpha_class.entity_type = "class".to_string();
        let mut before_beta_class =
            make_entity("old.ts::class::Beta", "Beta", "class Beta {}", "old.ts");
        before_beta_class.entity_type = "class".to_string();

        let mut after_alpha_class =
            make_entity("new.ts::class::Alpha", "Alpha", "class Alpha {}", "new.ts");
        after_alpha_class.entity_type = "class".to_string();
        let mut after_beta_class =
            make_entity("new.ts::class::Beta", "Beta", "class Beta {}", "new.ts");
        after_beta_class.entity_type = "class".to_string();

        let before = vec![
            before_alpha_class,
            before_beta_class,
            make_entity_with_parent(
                "old.ts::class::Alpha::run",
                "run",
                "run() { return alpha_original_value; }",
                "old.ts",
                Some("old.ts::class::Alpha"),
            ),
            make_entity_with_parent(
                "old.ts::class::Beta::run",
                "run",
                "run() { return beta_original_value; }",
                "old.ts",
                Some("old.ts::class::Beta"),
            ),
        ];
        let after = vec![
            after_alpha_class,
            after_beta_class,
            make_entity_with_parent(
                "new.ts::class::Alpha::run",
                "run",
                "run() { return alpha_changed_value; }",
                "new.ts",
                Some("new.ts::class::Alpha"),
            ),
            make_entity_with_parent(
                "new.ts::class::Beta::run",
                "run",
                "run() { return beta_changed_value; }",
                "new.ts",
                Some("new.ts::class::Beta"),
            ),
        ];

        let result = match_entities(&before, &after, "new.ts", None, None, None);
        let method_added_or_deleted = result
            .changes
            .iter()
            .filter(|change| {
                change.entity_type == "method"
                    && matches!(change.change_type, ChangeType::Added | ChangeType::Deleted)
            })
            .count();
        let alpha = result
            .changes
            .iter()
            .find(|change| change.entity_id == "new.ts::class::Alpha::run")
            .expect("alpha method should be matched across the file rename");
        let beta = result
            .changes
            .iter()
            .find(|change| change.entity_id == "new.ts::class::Beta::run")
            .expect("beta method should be matched across the file rename");

        assert_eq!(method_added_or_deleted, 0, "{:?}", result.changes);
        assert_eq!(alpha.change_type, ChangeType::Moved);
        assert_eq!(alpha.old_parent_id.as_deref(), Some("old.ts::class::Alpha"));
        assert_eq!(beta.change_type, ChangeType::Moved);
        assert_eq!(beta.old_parent_id.as_deref(), Some("old.ts::class::Beta"));
    }

    #[test]
    fn test_moved_content_change_without_structural_hash_is_unknown_structurally() {
        let before = vec![make_entity(
            "old.ts::function::foo",
            "foo",
            "export function foo() { return alpha + beta + gamma; }",
            "old.ts",
        )];
        let after = vec![make_entity(
            "new.ts::function::foo",
            "foo",
            "export function foo() { return one + two + three; }",
            "new.ts",
        )];

        let result = match_entities(&before, &after, "new.ts", None, None, None);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Moved);
        assert_eq!(result.changes[0].old_file_path.as_deref(), Some("old.ts"));
        assert_eq!(result.changes[0].structural_change, None);
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

        // match_entities no longer deduplicates — suppression happens in differ.rs.
        // Both the class and the method are Modified here.
        assert_eq!(result.changes.len(), 2);
        let types: Vec<ChangeType> = result.changes.iter().map(|c| c.change_type).collect();
        assert!(types.iter().all(|t| *t == ChangeType::Modified));
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

    fn make_entity_with_parent(
        id: &str,
        name: &str,
        content: &str,
        file_path: &str,
        parent_id: Option<&str>,
    ) -> SemanticEntity {
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
            "a.rs::class::ClassA::foo",
            "foo",
            "fn foo() { do_thing() }",
            "a.rs",
            Some("a.rs::class::ClassA"),
        )];
        let after = vec![make_entity_with_parent(
            "a.rs::class::ClassB::foo",
            "foo",
            "fn foo() { do_thing() }",
            "a.rs",
            Some("a.rs::class::ClassB"),
        )];
        let result = match_entities(&before, &after, "a.rs", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Moved);
        assert_eq!(
            result.changes[0].old_parent_id,
            Some("a.rs::class::ClassA".to_string())
        );
    }

    #[test]
    fn test_same_parent_is_rename_not_move() {
        // Same parent, different name = rename (not move)
        // Content must be identical (same hash) so Phase 2 catches it
        let body = "fn method(&self) { let x = self.compute(); self.validate(x); self.store(x) }";
        let before = vec![make_entity_with_parent(
            "a.rs::class::Foo::old_method",
            "old_method",
            body,
            "a.rs",
            Some("a.rs::class::Foo"),
        )];
        let after = vec![make_entity_with_parent(
            "a.rs::class::Foo::new_method",
            "new_method",
            body,
            "a.rs",
            Some("a.rs::class::Foo"),
        )];
        let result = match_entities(&before, &after, "a.rs", None, None, None);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Renamed);
        assert!(result.changes[0].old_parent_id.is_none());
    }

    fn make_entity_at(
        id: &str,
        name: &str,
        content: &str,
        file_path: &str,
        line: usize,
    ) -> SemanticEntity {
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
        assert!(result.changes[0].before_content.is_none());
        assert!(result.changes[0].old_start_line.is_some());
        assert!(result.changes[0].old_end_line.is_some());
        assert_ne!(
            result.changes[0].old_start_line,
            Some(result.changes[0].start_line)
        );
        // Either beta or gamma is marked, LIS picks the minimum
        assert!(
            result.changes[0].entity_name == "beta" || result.changes[0].entity_name == "gamma"
        );
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
    fn test_no_reorder_for_unchanged_entities_on_same_line() {
        let before = vec![
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 1),
            make_entity_at("a::f::gamma", "gamma", "fn gamma() {}", "a.rs", 1),
            make_entity_at("a::f::delta", "delta", "fn delta() {}", "a.rs", 1),
        ];
        let after = vec![
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 1),
            make_entity_at("a::f::gamma", "gamma", "fn gamma() { 999 }", "a.rs", 1),
            make_entity_at("a::f::delta", "delta", "fn delta() {}", "a.rs", 1),
        ];
        let result = match_entities(&before, &after, "a.rs", None, None, None);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
        assert_eq!(result.changes[0].entity_name, "gamma");
    }

    #[test]
    fn test_collision_group_shrink_with_survivor_edit() {
        let before = vec![
            make_entity_at(
                "a::f::f@L1#1",
                "f",
                "function f(a: number): void {}",
                "a.ts",
                1,
            ),
            make_entity_at(
                "a::f::f@L1#2",
                "f",
                "function f(a: string): void {}",
                "a.ts",
                1,
            ),
        ];
        let after = vec![make_entity_at(
            "a::f::f",
            "f",
            "function f(a: number): void { console.log(a) }",
            "a.ts",
            1,
        )];
        let result = match_entities(&before, &after, "a.ts", None, None, None);
        let modified = result
            .changes
            .iter()
            .filter(|change| change.change_type == ChangeType::Modified)
            .count();
        let deleted = result
            .changes
            .iter()
            .filter(|change| change.change_type == ChangeType::Deleted)
            .count();

        assert_eq!(modified, 1, "{:?}", result.changes);
        assert_eq!(deleted, 1, "{:?}", result.changes);
    }

    #[test]
    fn test_collision_group_growth_matches_edited_survivor() {
        let before = vec![make_entity_at(
            "a::f::f",
            "f",
            "function f(): void { return oldValue + stableThing; }",
            "a.ts",
            1,
        )];
        let after = vec![
            make_entity_at(
                "a::f::f@L1#1",
                "f",
                "function f(): void { totallyDifferentAlphaBetaGamma(); }",
                "a.ts",
                1,
            ),
            make_entity_at(
                "a::f::f@L1#2",
                "f",
                "function f(): void { return oldValue + stableThing + changedThing; }",
                "a.ts",
                1,
            ),
        ];
        let result = match_entities(&before, &after, "a.ts", None, None, None);
        let modified = result
            .changes
            .iter()
            .find(|change| change.change_type == ChangeType::Modified)
            .expect("edited survivor should be modified");
        let added = result
            .changes
            .iter()
            .find(|change| change.change_type == ChangeType::Added)
            .expect("new duplicate should be added");

        assert_eq!(result.changes.len(), 2, "{:?}", result.changes);
        assert_eq!(modified.entity_id, "a::f::f@L1#2");
        assert_eq!(added.entity_id, "a::f::f@L1#1");
    }

    #[test]
    fn test_same_file_signature_rejects_unrelated_content() {
        let before = vec![
            make_entity_at(
                "a.ts::function::process@L1",
                "process",
                "function process(req: Request) { return validateInput(req.body); const result = processData(req.params); return formatResponse(result, req.headers); }",
                "a.ts",
                1,
            ),
            make_entity_at(
                "a.ts::function::process@L7",
                "process",
                "function process(socket: WebSocket): void { const conn = establishConnection(socket.url); conn.onMessage(data => parseProtobuf(data)); conn.onClose(() => cleanupResources(conn.id)); }",
                "a.ts",
                7,
            ),
        ];
        let after = vec![
            make_entity_at(
                "a.ts::function::process@L1",
                "process",
                "function process(req: Request) { return validateInput(req.body); const result = processData(req.params); return sendJSON(result); }",
                "a.ts",
                1,
            ),
            make_entity_at(
                "a.ts::function::process@L9",
                "process",
                "function process(file: File): Promise<string> { const buffer = await readFileAsBuffer(file); const hash = computeSHA256(buffer); await uploadToS3(hash, buffer); return generateCDNUrl(hash); }",
                "a.ts",
                9,
            ),
        ];

        let result = match_entities(&before, &after, "a.ts", None, None, None);
        let modified = result
            .changes
            .iter()
            .filter(|change| change.change_type == ChangeType::Modified)
            .count();
        let added = result
            .changes
            .iter()
            .find(|change| change.change_type == ChangeType::Added)
            .expect("unrelated upload handler should be added");
        let deleted = result
            .changes
            .iter()
            .find(|change| change.change_type == ChangeType::Deleted)
            .expect("unrelated websocket handler should be deleted");

        assert_eq!(modified, 1, "{:?}", result.changes);
        assert_eq!(added.entity_id, "a.ts::function::process@L9");
        assert_eq!(deleted.entity_id, "a.ts::function::process@L7");
    }

    #[test]
    fn test_reorder_detection_uses_same_line_extraction_order() {
        let before = vec![
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 1),
            make_entity_at("a::f::gamma", "gamma", "fn gamma() {}", "a.rs", 1),
        ];
        let after = vec![
            make_entity_at("a::f::beta", "beta", "fn beta() {}", "a.rs", 1),
            make_entity_at("a::f::alpha", "alpha", "fn alpha() {}", "a.rs", 1),
            make_entity_at("a::f::gamma", "gamma", "fn gamma() {}", "a.rs", 1),
        ];
        let result = match_entities(&before, &after, "a.rs", None, None, None);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Reordered);
        assert!(
            result.changes[0].entity_name == "alpha" || result.changes[0].entity_name == "beta"
        );
    }

    #[test]
    fn test_default_similarity() {
        let a = make_entity("a", "a", "the quick brown fox", "a.ts");
        let b = make_entity("b", "b", "the quick brown dog", "a.ts");
        let score = default_similarity(&a, &b);
        assert!(score > 0.5);
        assert!(score < 1.0);
    }

    #[test]
    fn parent_name_terminates_on_cyclic_parent_id() {
        // Two entities whose parent_id chains form a cycle. parent_name
        // would loop forever without the visited-set guard.
        let a = make_entity_with_parent("A", "A", "", "f", Some("B"));
        let b = make_entity_with_parent("B", "B", "", "f", Some("A"));
        let mut by_id: HashMap<&str, &SemanticEntity> = HashMap::new();
        by_id.insert("A", &a);
        by_id.insert("B", &b);
        // Synthesize a leaf whose parent_id enters the cycle via A.
        let leaf = make_entity_with_parent("L", "L", "", "f", Some("A"));
        let chain = parent_name(&leaf, &by_id);
        // Must terminate. We don't assert exact contents — order/composition
        // depends on which side of the cycle is reached first; the safety
        // property is "this returns at all."
        assert!(chain.is_some());
    }
}
