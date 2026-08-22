use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
