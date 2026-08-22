use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticEntity {
    pub id: String,
    pub file_path: String,
    pub entity_type: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub content: String,
    pub content_hash: String,
    /// AST-based hash that strips comments and normalizes whitespace.
    /// Two entities with the same structural_hash are logically identical
    /// even if formatting/comments differ. Inspired by Unison's content-addressed model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structural_hash: Option<String>,
    /// Semantic identity hash ("kappa"): a canonical hash over named AST node
    /// kinds and semantically meaningful leaf tokens (identifiers, literals,
    /// operators), excluding comments and pure CST punctuation/delimiters
    /// (braces, parens, semicolons, commas). Computed additively alongside
    /// `structural_hash` and does not change its meaning or use.
    ///
    /// Unlike `structural_hash` (which strips the name so renames don't
    /// affect it, for `differ.rs` rename detection), kappa includes the name
    /// and is rename-*sensitive* — it is meant as a parser-independent
    /// semantic identity, not a rename-detection signal. Formatting-only
    /// changes (whitespace, comments, trailing commas, brace style) leave it
    /// unchanged. See `crates/sem-core/ for the full spec,
    /// measurements, and collision analysis.
    ///
    /// `None` for entities from parsers that don't expose byte spans (most
    /// non-tree-sitter plugins) and for a couple of tree-sitter-code
    /// error-recovery fallback paths that don't operate over a single clean
    /// AST subtree (see compat/coverage section).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kappa: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    /// Byte offset of the entity's first byte in the source file (inclusive).
    /// `None` for entities from parsers that don't expose byte spans (most
    /// non-tree-sitter plugins). Set for code entities, where it equals the
    /// underlying tree-sitter node's `start_byte()`. Lets a consumer slice the
    /// exact original bytes out of the file given only `file_path` + this span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_byte: Option<usize>,
    /// Byte offset just past the entity's last byte in the source file
    /// (exclusive), matching tree-sitter's `end_byte()`. `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_byte: Option<usize>,
    /// A `BTreeMap`, not a `HashMap`: this is serialized directly
    /// — into the factpack's CBOR and into `cache.db`'s `metadata_json`
    /// column via `serde_json` — and `std::HashMap`'s per-instance random
    /// hasher key makes that serialization nondeterministic even across two
    /// maps built the same way in the same process, let alone across
    /// separate builds. Sorted-by-key iteration is exactly the canonical
    /// order those write boundaries need, and this map is small (a handful
    /// of entries per entity), so there is no hot-path cost being traded
    /// away for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
}

pub fn build_entity_id(
    file_path: &str,
    entity_type: &str,
    name: &str,
    parent_id: Option<&str>,
) -> String {
    match parent_id {
        Some(pid) => format!("{pid}::{name}"),
        None => format!("{file_path}::{entity_type}::{name}"),
    }
}

/// Build an entity ID with a line-number disambiguator for overloads.
pub fn build_entity_id_disambiguated(
    file_path: &str,
    entity_type: &str,
    name: &str,
    parent_id: Option<&str>,
    line: usize,
) -> String {
    let base = build_entity_id(file_path, entity_type, name, parent_id);
    format!("{base}@L{line}")
}

/// Build an entity ID with a line-number and same-line ordinal disambiguator.
pub fn build_entity_id_disambiguated_with_ordinal(
    file_path: &str,
    entity_type: &str,
    name: &str,
    parent_id: Option<&str>,
    line: usize,
    ordinal: usize,
) -> String {
    let base = build_entity_id_disambiguated(file_path, entity_type, name, parent_id, line);
    format!("{base}#{ordinal}")
}

/// Build an entity ID with a document-index disambiguator, for formats that
/// pack independent documents into one file (multi-document YAML, `---`-
/// separated). Two same-named top-level entities in *different* documents of
/// one file otherwise collapse onto the same [`build_entity_id`] output —
/// this appends the zero-based document index so they stay distinct.
///
/// Deliberately its own tag (`@D`), not a reuse of
/// [`build_entity_id_disambiguated`]'s `@L` line marker: a document index
/// only changes when a document boundary is inserted or removed *before*
/// this entity, whereas a line number changes on every edit anywhere above
/// it in the file — far churnier, and the wrong stability story for a
/// content-addressed diff. Labeling a document index "L" would also just be
/// wrong, not merely imprecise.
pub fn build_entity_id_disambiguated_by_document(
    file_path: &str,
    entity_type: &str,
    name: &str,
    parent_id: Option<&str>,
    doc_index: usize,
) -> String {
    let base = build_entity_id(file_path, entity_type, name, parent_id);
    format!("{base}@D{doc_index}")
}

type IdRewrites = HashMap<String, Vec<(usize, String)>>;

pub(crate) fn disambiguate_colliding_entity_ids(entities: &mut [SemanticEntity]) {
    if !has_duplicate_entity_ids(entities) {
        return;
    }

    // Each pass can expose child ID collisions at the next descendant level.
    // The entity count bounds the maximum number of parent-child propagation steps.
    for _ in 0..=entities.len() {
        let rewrites = disambiguate_current_entity_ids(entities);
        if rewrites.is_empty() {
            assert_unique_entity_ids(entities);
            return;
        }

        propagate_parent_id_rewrites(entities, rewrites);
    }

    assert_unique_entity_ids(entities);
}

fn has_duplicate_entity_ids(entities: &[SemanticEntity]) -> bool {
    let mut seen = HashSet::with_capacity(entities.len());
    for entity in entities {
        if !seen.insert(entity.id.as_str()) {
            return true;
        }
    }
    false
}

fn disambiguate_current_entity_ids(entities: &mut [SemanticEntity]) -> IdRewrites {
    let mut id_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, entity) in entities.iter().enumerate() {
        id_indices.entry(entity.id.clone()).or_default().push(i);
    }

    let mut rewrites: IdRewrites = HashMap::new();
    for (_id, indices) in &id_indices {
        if indices.len() > 1 {
            let mut indices = indices.clone();
            indices.sort_unstable();

            let mut line_counts: HashMap<usize, usize> = HashMap::new();
            for &idx in &indices {
                *line_counts.entry(entities[idx].start_line).or_default() += 1;
            }

            let mut line_ordinals: HashMap<usize, usize> = HashMap::new();
            for &idx in &indices {
                let e = &entities[idx];
                let new_id = if line_counts[&e.start_line] > 1 {
                    let ordinal = line_ordinals.entry(e.start_line).or_default();
                    *ordinal += 1;
                    build_entity_id_disambiguated_with_ordinal(
                        &e.file_path,
                        &e.entity_type,
                        &e.name,
                        e.parent_id.as_deref(),
                        e.start_line,
                        *ordinal,
                    )
                } else {
                    build_entity_id_disambiguated(
                        &e.file_path,
                        &e.entity_type,
                        &e.name,
                        e.parent_id.as_deref(),
                        e.start_line,
                    )
                };
                let old_id = std::mem::replace(&mut entities[idx].id, new_id.clone());
                if old_id != new_id {
                    rewrites.entry(old_id).or_default().push((idx, new_id));
                }
            }
        }
    }

    rewrites
}

fn propagate_parent_id_rewrites(entities: &mut [SemanticEntity], mut rewrites: IdRewrites) {
    while !rewrites.is_empty() {
        let mut child_rewrites: IdRewrites = HashMap::new();

        for child_idx in 0..entities.len() {
            let Some(parent_id) = entities[child_idx].parent_id.clone() else {
                continue;
            };
            let Some(candidates) = rewrites.get(&parent_id) else {
                continue;
            };
            let Some(new_parent_id) = select_rewritten_parent_id(entities, child_idx, candidates)
            else {
                continue;
            };

            entities[child_idx].parent_id = Some(new_parent_id.clone());
            let new_child_id = build_entity_id(
                &entities[child_idx].file_path,
                &entities[child_idx].entity_type,
                &entities[child_idx].name,
                Some(&new_parent_id),
            );
            let old_child_id = std::mem::replace(&mut entities[child_idx].id, new_child_id.clone());
            if old_child_id != new_child_id {
                child_rewrites
                    .entry(old_child_id)
                    .or_default()
                    .push((child_idx, new_child_id));
            }
        }

        rewrites = child_rewrites;
    }
}

fn select_rewritten_parent_id(
    entities: &[SemanticEntity],
    child_idx: usize,
    candidates: &[(usize, String)],
) -> Option<String> {
    let child = &entities[child_idx];
    let mut best: Option<((u8, u8, u8, usize, usize), String)> = None;

    for (parent_idx, parent_id) in candidates {
        if *parent_idx == child_idx {
            continue;
        }
        let parent = &entities[*parent_idx];
        let same_file_rank = if parent.file_path == child.file_path {
            0
        } else {
            1
        };
        let before_rank = if *parent_idx < child_idx { 0 } else { 1 };
        let line_span_contains_child =
            parent.start_line <= child.start_line && child.end_line <= parent.end_line;
        let line_span_differs =
            (parent.start_line, parent.end_line) != (child.start_line, child.end_line);
        let contains_rank = if line_span_contains_child && line_span_differs {
            0
        } else {
            1
        };
        let distance = parent_idx.abs_diff(child_idx);
        let span = parent.end_line.saturating_sub(parent.start_line);
        let key = (same_file_rank, contains_rank, before_rank, distance, span);

        if match best.as_ref() {
            Some((best_key, _)) => key < *best_key,
            None => true,
        } {
            best = Some((key, parent_id.clone()));
        }
    }

    best.map(|(_, parent_id)| parent_id)
}

pub(crate) fn assert_unique_entity_ids(entities: &[SemanticEntity]) {
    let mut seen = HashSet::with_capacity(entities.len());
    for entity in entities {
        assert!(
            seen.insert(entity.id.as_str()),
            "duplicate semantic entity id generated: {}",
            entity.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `metadata`'s JSON serialization (what `sem-mcp`'s and
    /// `sem-cli`'s `cache.db` writers store as `metadata_json`) must not
    /// depend on which order the map happens to iterate in. The change's
    /// evidence is 78 differing rows between two runs of the *same binary at
    /// the same HEAD* — this reproduces the mechanism directly: build the
    /// same key/value pairs, in the same insertion order, into two separate
    /// map instances (standing in for two producer builds of the same
    /// entity), and require identical JSON bytes.
    ///
    /// Fails on HEAD because `metadata`'s `HashMap` uses `std`'s default
    /// `RandomState`, which draws a fresh random hasher key per `HashMap`
    /// instance (not just per process) — so even two maps built back-to-back
    /// in one test, from the identical insertion sequence, routinely iterate
    /// (and therefore serialize) in different orders.
    #[test]
    fn metadata_json_is_independent_of_map_instance() {
        fn build() -> super::SemanticEntity {
            let mut metadata = std::collections::BTreeMap::new();
            metadata.insert("alpha".to_string(), "1".to_string());
            metadata.insert("bravo".to_string(), "2".to_string());
            metadata.insert("charlie".to_string(), "3".to_string());
            metadata.insert("delta".to_string(), "4".to_string());
            metadata.insert("echo".to_string(), "5".to_string());
            metadata.insert("foxtrot".to_string(), "6".to_string());
            metadata.insert("golf".to_string(), "7".to_string());
            metadata.insert("hotel".to_string(), "8".to_string());
            SemanticEntity {
                id: "f.ts::function::f".to_string(),
                file_path: "f.ts".to_string(),
                entity_type: "function".to_string(),
                name: "f".to_string(),
                parent_id: None,
                content: "fn f() {}".to_string(),
                content_hash: "deadbeef".to_string(),
                structural_hash: None,
                kappa: None,
                start_line: 1,
                end_line: 1,
                start_byte: None,
                end_byte: None,
                metadata: Some(metadata),
            }
        }

        let a = build();
        let b = build();
        let a_json = serde_json::to_string(a.metadata.as_ref().unwrap()).expect("encode a");
        let b_json = serde_json::to_string(b.metadata.as_ref().unwrap()).expect("encode b");

        assert_eq!(
            a_json, b_json,
            "identical metadata content serialized to different \
             metadata_json bytes across two map instances built the same way — \
             this is what makes cache.db table-dump gates non-reproducible"
        );
    }

    #[test]
    fn test_build_entity_id_no_parent() {
        assert_eq!(
            build_entity_id("src/main.ts", "function", "hello", None),
            "src/main.ts::function::hello"
        );
    }

    #[test]
    fn test_build_entity_id_with_parent() {
        let id = build_entity_id(
            "src/main.ts",
            "method",
            "greet",
            Some("src/main.ts::class::MyClass"),
        );
        assert_eq!(id, "src/main.ts::class::MyClass::greet");
    }

    #[test]
    fn test_build_entity_id_disambiguated_by_document() {
        let id = build_entity_id_disambiguated_by_document(
            "fixtures/multidoc.yaml",
            "section",
            "Args",
            None,
            2,
        );
        assert_eq!(id, "fixtures/multidoc.yaml::section::Args@D2");
    }

    #[test]
    fn test_build_entity_id_disambiguated_by_document_distinct_per_index() {
        let a = build_entity_id_disambiguated_by_document(
            "f.yaml", "section", "Args", None, 0,
        );
        let b = build_entity_id_disambiguated_by_document(
            "f.yaml", "section", "Args", None, 1,
        );
        assert_ne!(a, b);
    }
}
