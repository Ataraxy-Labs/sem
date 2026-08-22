//! semx-vlg / semx-kkk: a multi-document YAML file (documents separated by
//! `---`) with same-named top-level keys in different documents used to
//! collapse onto one `build_entity_id` output — a genuine oracle failure on
//! llvm (11 "Args" entities sharing one id, TESTS_ORACLE semx-kkk), making
//! which document's data (including `is_test`) a corpus-wide id collision
//! kept depend on processing order.
//!
//! `crates/sem-core/src/parser/plugins/yaml.rs`'s own `#[cfg(test)]` module
//! already proves the id fix at the plugin level (ids no longer collide,
//! single-document files are unchanged). This file is the whole-graph
//! determinism witness: build the graph over a multi-document fixture twice,
//! under two differently-sized rayon thread pools, and assert the result —
//! entity ids, content hashes, and `is_test` — is bit-identical either way.
//!
//! A single process can't just set `RAYON_NUM_THREADS` twice (rayon's
//! global pool reads it once, at first use, and can't be reconfigured
//! afterward), so this scopes two *explicit* pools with
//! `rayon::ThreadPoolBuilder` and runs the same cold build inside each via
//! `.install(...)` — the standard way to vary parallelism within one test.

use sem_core::parser::graph::{is_test_entity, RefType};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::session::GraphSession;
use std::path::Path;

/// Same shape as the multi-document fixture in
/// `yaml.rs::tests::test_yaml_multidoc_same_named_keys_get_distinct_ids`,
/// generalized to three documents and placed under a test-detected
/// directory (`fixtures/`, per `test_detect.rs`'s `EXACT_DIR_NAMES`) with
/// one document's `Args` content carrying a test marker (`it(`) — so a
/// wrongly-surviving collision would show up as the *wrong* document's
/// `is_test` value, not just a missing entity.
const MULTIDOC_YAML: &str = "\
---
name: alpha
Args:
  x: 1
---
name: beta
Args:
  note: \"it(marks this document's Args as test-like)\"
---
name: gamma
Args:
  y: 3
";

struct GraphSnapshot {
    /// `(id, content_hash, is_test)` per entity, sorted by id so build order
    /// (which can vary with thread count) doesn't matter for comparison.
    entities: Vec<(String, String, bool)>,
    /// `(from, to, kind)` per edge, sorted the same way.
    edges: Vec<(String, String, &'static str)>,
    /// Count of `graph.entities` (`EntityInfoMap`, keyed by entity id) —
    /// where a real id collision actually bites: `EntityGraph::build`
    /// populates this map with one `HashMap::insert` per entity, so two
    /// entities sharing an id silently collapse into whichever one is
    /// inserted last. Comparing this count against the raw entity count
    /// below is the collision detector; comparing it across thread counts
    /// is the determinism witness.
    id_keyed_entity_count: usize,
}

fn build_snapshot(root: &Path, files: &[String], num_threads: usize) -> GraphSnapshot {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .expect("build scoped rayon pool");

    pool.install(|| {
        let registry = create_default_registry();
        let session = GraphSession::build(root, files, &registry);

        let mut entities: Vec<(String, String, bool)> = session
            .entities()
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    e.content_hash.clone(),
                    is_test_entity(e, &[]),
                )
            })
            .collect();
        entities.sort();

        let mut edges: Vec<(String, String, &'static str)> = session
            .graph()
            .edges
            .iter()
            .map(|e| {
                let kind = match e.ref_type {
                    RefType::Calls => "calls",
                    RefType::TypeRef => "typeref",
                    RefType::Imports => "imports",
                };
                (e.from_entity.clone(), e.to_entity.clone(), kind)
            })
            .collect();
        edges.sort();

        let id_keyed_entity_count = session.graph().entities.len();

        GraphSnapshot {
            entities,
            edges,
            id_keyed_entity_count,
        }
    })
}

#[test]
fn multidoc_yaml_graph_build_is_deterministic_across_thread_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("fixtures")).unwrap();
    std::fs::write(root.join("fixtures/multidoc.yaml"), MULTIDOC_YAML).unwrap();

    let files = vec!["fixtures/multidoc.yaml".to_string()];

    let single_threaded = build_snapshot(root, &files, 1);
    let multi_threaded = build_snapshot(root, &files, 4);

    assert_eq!(
        single_threaded.entities, multi_threaded.entities,
        "entity set (id, content_hash, is_test) differs between a 1-thread \
         and a 4-thread cold build of the same multi-document YAML fixture \
         — the corpus-wide id collision this fixture exercises must not \
         make a build's result depend on processing order"
    );
    assert_eq!(
        single_threaded.edges, multi_threaded.edges,
        "edge set differs between thread counts"
    );
    assert_eq!(
        single_threaded.id_keyed_entity_count, multi_threaded.id_keyed_entity_count,
        "graph.entities (id-keyed) count differs between thread counts"
    );

    // The real collision detector: `graph.entities` is keyed by entity id,
    // so two entities sharing an id collapse into one `HashMap` slot —
    // silently dropping one of them from every id-keyed lookup (parent
    // resolution, impact analysis, the persisted index) even though the raw
    // `session.entities()` list above still (harmlessly) contains both. This
    // fixture's 5 raw entities (2 "name" + 3 "Args") must all survive as 5
    // distinct `graph.entities` rows once ids are unique.
    assert_eq!(
        single_threaded.id_keyed_entity_count,
        single_threaded.entities.len(),
        "graph.entities lost entries to an id collision: {} raw entities but \
         only {} distinct ids in the id-keyed map — a colliding id silently \
         drops every entity but the last-inserted one from parent \
         resolution, impact analysis, and the persisted index",
        single_threaded.entities.len(),
        single_threaded.id_keyed_entity_count,
    );

    // Direct regression check for the reported symptom: the "Args" entity
    // whose content carries the test marker must be classified is_test, and
    // the other two "Args" entities (same key, different documents) must
    // not be — proving the id fix keeps each document's own entity, and its
    // own is_test answer, independently addressable rather than collapsed
    // onto one winner.
    let args_entities: Vec<&(String, String, bool)> = single_threaded
        .entities
        .iter()
        .filter(|(id, _, _)| id.contains("Args"))
        .collect();
    assert_eq!(
        args_entities.len(),
        3,
        "expected 3 distinct Args entities (one per document): {args_entities:?}"
    );
    let test_like_count = args_entities.iter().filter(|(_, _, is_test)| *is_test).count();
    assert_eq!(
        test_like_count, 1,
        "expected exactly one Args entity (beta's, carrying the `it(` marker) \
         to be classified is_test: {args_entities:?}"
    );
}
