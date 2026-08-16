//! The content-addressed parse cache must be invisible to callers.
//!
//! `CodeParserPlugin::extract_entities` goes through the cache;
//! `extract_entities_with_tree` does not. Any divergence between them is a bug
//! in the cache, so these tests pin them together over real fixture files.

use sem_core::parser::cache;
use sem_core::parser::plugin::SemanticParserPlugin;
use sem_core::parser::plugins::code::CodeParserPlugin;
use sem_core::parser::plugins::create_default_registry;

/// A handful of real files spanning the languages weave actually merges.
fn fixtures() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        "tests/fixtures/python/api.py",
        "tests/fixtures/python/auth.py",
        "tests/fixtures/python/db.py",
        "tests/fixtures/python/handlers.py",
        "tests/fixtures/scope_test/typescript/models.ts",
        "tests/fixtures/scope_test/typescript/service.ts",
        "src/parser/plugin.rs",
        "src/utils/hash.rs",
        "src/parser/registry.rs",
    ];

    let loaded: Vec<(String, String)> = candidates
        .iter()
        .filter_map(|rel| {
            let content = std::fs::read_to_string(root.join(rel)).ok()?;
            Some((rel.to_string(), content))
        })
        .collect();

    assert!(
        loaded.len() >= 5,
        "expected the fixture corpus to still exist, found {}",
        loaded.len()
    );
    loaded
}

fn assert_same(
    expected: &[sem_core::model::entity::SemanticEntity],
    actual: &[sem_core::model::entity::SemanticEntity],
    what: &str,
) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "entity count differs for {what}"
    );
    for (a, b) in expected.iter().zip(actual) {
        assert_eq!(a.id, b.id, "id differs for {what}");
        assert_eq!(a.file_path, b.file_path, "file_path differs for {what}");
        assert_eq!(
            a.entity_type, b.entity_type,
            "entity_type differs for {what}"
        );
        assert_eq!(a.name, b.name, "name differs for {what}");
        assert_eq!(a.parent_id, b.parent_id, "parent_id differs for {what}");
        assert_eq!(a.content, b.content, "content differs for {what}");
        assert_eq!(
            a.content_hash, b.content_hash,
            "content_hash differs for {what}"
        );
        assert_eq!(
            a.structural_hash, b.structural_hash,
            "structural_hash differs for {what}"
        );
        assert_eq!(a.start_line, b.start_line, "start_line differs for {what}");
        assert_eq!(a.end_line, b.end_line, "end_line differs for {what}");
        assert_eq!(a.start_byte, b.start_byte, "start_byte differs for {what}");
        assert_eq!(a.end_byte, b.end_byte, "end_byte differs for {what}");
        assert_eq!(a.metadata, b.metadata, "metadata differs for {what}");
    }
}

#[test]
fn cached_extraction_matches_the_uncached_path() {
    let plugin = CodeParserPlugin;
    for (path, content) in fixtures() {
        // The uncached reference: `extract_entities_with_tree` never consults
        // the cache.
        let reference = plugin.extract_entities_with_tree(&content, &path).0;

        let first = plugin.extract_entities(&content, &path);
        assert_same(&reference, &first, &format!("{path} (first call)"));

        let second = plugin.extract_entities(&content, &path);
        assert_same(&reference, &second, &format!("{path} (cached call)"));
    }
}

#[test]
fn a_cache_hit_is_recorded_for_a_repeated_blob() {
    if !cache::is_enabled() {
        return; // SEM_PARSE_CACHE=0 in the environment; nothing to assert.
    }
    let plugin = CodeParserPlugin;
    let path = "hit_counter_probe.py";
    let content = "def probe(x):\n    return x + 1\n";

    // Prime, then measure only the repeat so a concurrent test cannot inflate
    // the delta beyond the >= assertion.
    plugin.extract_entities(content, path);
    let before = cache::stats().hits;
    plugin.extract_entities(content, path);
    let after = cache::stats().hits;

    assert!(
        after > before,
        "expected a hit on the repeated blob ({before} -> {after})"
    );
}

#[test]
fn an_edit_to_the_content_is_a_different_key() {
    let plugin = CodeParserPlugin;
    let path = "edit_probe.py";
    let before = plugin.extract_entities("def alpha():\n    pass\n", path);
    let after = plugin.extract_entities("def beta():\n    pass\n", path);

    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_eq!(before[0].name, "alpha");
    assert_eq!(after[0].name, "beta");
}

#[test]
fn the_same_bytes_at_a_different_path_get_their_own_entities() {
    let plugin = CodeParserPlugin;
    let content = "def shared():\n    pass\n";
    let a = plugin.extract_entities(content, "pkg/a.py");
    let b = plugin.extract_entities(content, "pkg/b.py");

    assert_eq!(a[0].file_path, "pkg/a.py");
    assert_eq!(b[0].file_path, "pkg/b.py");
    assert_ne!(a[0].id, b[0].id, "entity ids embed the path");
}

#[test]
fn structural_hash_is_stable_across_repeats() {
    let plugin = CodeParserPlugin;
    let path = "structural_probe.rs";
    let content = "pub fn probe(x: u8) -> u8 { x + 1 }\n";
    let first = plugin.structural_hash_content(content, path);
    let second = plugin.structural_hash_content(content, path);
    assert_eq!(first, second);
    assert!(first.is_some());

    // Comments and whitespace must still wash out — the cache must not be
    // keyed in a way that changes this.
    let commented = "pub fn probe(x: u8) -> u8 {\n    // note\n    x + 1\n}\n";
    assert_eq!(
        plugin.structural_hash_content(commented, path),
        first,
        "structural hash should ignore comments and formatting"
    );
}

#[test]
fn the_registry_path_is_cached_and_still_correct() {
    let registry = create_default_registry();
    for (path, content) in fixtures() {
        let first = registry.extract_entities(&path, &content);
        let second = registry.extract_entities(&path, &content);
        assert_same(&first, &second, &format!("registry {path}"));

        // The brief variant strips payloads off the *same* underlying result.
        let brief = registry.extract_entities_brief(&path, &content);
        assert_eq!(brief.len(), first.len(), "brief count differs for {path}");
        for (b, f) in brief.iter().zip(&first) {
            assert_eq!(b.id, f.id);
            assert!(b.content.is_empty(), "brief entities must drop content");
        }
    }
}

#[test]
fn clearing_the_cache_does_not_change_results() {
    let plugin = CodeParserPlugin;
    let path = "clear_probe.ts";
    let content = "export function probe(n: number): number {\n  return n * 2;\n}\n";
    let warm = plugin.extract_entities(content, path);
    cache::clear();
    let cold = plugin.extract_entities(content, path);
    assert_same(&warm, &cold, "after clear");
}
