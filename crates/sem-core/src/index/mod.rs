//! The query index: a zero-copy, mmap-able materialized view that answers
//! structural questions from a cold process without a daemon, a filesystem
//! walk, or a corpus-wide freshness proof.
//!
//! `QUERY-INDEX.md` is the contract — the measured attribution that justifies
//! this design (§1), the invariants it satisfies (§2), the byte layout (§3), and
//! the list of layers it obsoletes (§7). Read that before changing anything
//! here; several of the choices below look arbitrary and are not.
//!
//! Scope today: the entity tier (entity-by-name, entities-by-file), `REFS`
//! (callers/refs postings, semx-gis), `TRIGRAM` (the text tier, `sem grep`,
//! semx-az9 — see `grep`), and `DIRS`/`Complete` freshness (the membership
//! sweep, semx-ykf — see `complete`). An image built via the dirs-less
//! writer entry points still writes `DIRS` zero-length, which reads as "tier
//! absent" exactly like an unbuilt `REFS`/`TRIGRAM`, so old images and old
//! callers keep working without a format version bump.

pub mod complete;
pub mod format;
pub mod grep;
pub mod reader;
pub mod writer;

pub use complete::{complete_check, CompleteReport};
pub use reader::{Entity, QueryIndex, TrigramPosting};
pub use writer::{
    build, build_with_content_and_dirs, build_with_content_and_dirs_and_tests,
    build_with_content_and_dirs_and_tests_and_spans,
    build_with_trigrams_and_dirs_and_tests_and_spans, write_atomic, DirFingerprint,
    FileFingerprint, FileTrigrams, TrigramBuildStats,
};

/// The index sits beside `cache.db` and `facts/` in the per-repo cache dir.
pub const INDEX_FILE_NAME: &str = "index.sem";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::graph::{EntityGraph, EntityInfo, EntityInfoMap, EntityRef, RefType};

    const SALT: u64 = 0x5EEDu64;

    fn entity(id: &str, name: &str, kind: &str, file: &str, line: usize) -> EntityInfo {
        EntityInfo {
            id: id.to_string(),
            name: name.to_string(),
            entity_type: kind.to_string(),
            file_path: file.to_string(),
            parent_id: None,
            start_line: line,
            end_line: line + 5,
        }
    }

    fn graph_of(entities: Vec<EntityInfo>) -> EntityGraph {
        graph_of_with_edges(entities, Vec::new())
    }

    fn graph_of_with_edges(entities: Vec<EntityInfo>, edges: Vec<EntityRef>) -> EntityGraph {
        let map: EntityInfoMap = entities.into_iter().map(|e| (e.id.clone(), e)).collect();
        EntityGraph::from_parts(map, edges)
    }

    fn edge(from: &str, to: &str) -> EntityRef {
        typed_edge(from, to, RefType::Calls)
    }

    fn typed_edge(from: &str, to: &str, ref_type: RefType) -> EntityRef {
        EntityRef {
            from_entity: from.to_string(),
            to_entity: to.to_string(),
            ref_type,
        }
    }

    fn round_trip(graph: &EntityGraph) -> QueryIndex {
        let bytes = build_salt(graph, &[], SALT);
        QueryIndex::from_bytes_with_salt(bytes, SALT).expect("valid image")
    }

    /// The one salt-override convenience this test module needs
    /// (`build_image` is `pub(crate)` for exactly this): every test below
    /// wants a fixed salt for byte-identical assertions but has no
    /// content/dirs/test-classification to supply.
    fn build_salt(graph: &EntityGraph, files: &[FileFingerprint], salt: u64) -> Vec<u8> {
        writer::build_image(
            graph,
            files,
            &[],
            writer::TrigramSource::Contents(&std::collections::HashMap::new()),
            None,
            None,
            salt,
        )
        .0
    }

    fn sample() -> EntityGraph {
        graph_of(vec![
            entity("a.ts::function::alpha", "alpha", "function", "a.ts", 1),
            entity("a.ts::function::shared", "shared", "function", "a.ts", 20),
            entity("b.ts::class::shared", "shared", "class", "b.ts", 3),
            entity("b.ts::function::beta", "beta", "function", "b.ts", 40),
        ])
    }

    #[test]
    fn test_flags_are_absent_unless_the_writer_was_given_a_classification() {
        // `round_trip` hands the writer `None`: not "no entity is a
        // test", but "this caller could not classify". The distinction is the
        // whole point of the header flag — a reader that ignored it would
        // read an uncomputed field as an authoritative empty answer, which is
        // exactly the bug `test_flags_computed` guards on the SQLite side.
        let index = round_trip(&sample());
        assert!(!index.has_test_flags());
        assert!((0..index.entity_count()).all(|at| !index.entity(at).is_test()));
    }

    #[test]
    fn test_flags_round_trip_per_entity_when_supplied() {
        let graph = sample();
        let mut test_ids = std::collections::HashSet::new();
        test_ids.insert("b.ts::function::beta");
        let (bytes, _) = writer::build_image(
            &graph,
            &[],
            &[],
            writer::TrigramSource::Contents(&std::collections::HashMap::new()),
            Some(&test_ids),
            None,
            SALT,
        );
        let index = QueryIndex::from_bytes_with_salt(bytes, SALT).expect("valid image");
        assert!(index.has_test_flags());
        let flagged: Vec<String> = (0..index.entity_count())
            .map(|at| index.entity(at))
            .filter(|e| e.is_test())
            .map(|e| e.id())
            .collect();
        assert_eq!(flagged, vec!["b.ts::function::beta".to_string()]);
    }

    #[test]
    fn test_flags_do_not_disturb_any_other_entity_field() {
        // The bits live in `EntityRec`'s formerly-reserved `_pad` u16, so the
        // record width and every other accessor must be unchanged whether or
        // not the bit is set — this is what makes the change safe without a
        // `FORMAT_VERSION` bump.
        let graph = sample();
        let mut test_ids = std::collections::HashSet::new();
        for id in [
            "a.ts::function::alpha",
            "a.ts::function::shared",
            "b.ts::class::shared",
            "b.ts::function::beta",
        ] {
            test_ids.insert(id);
        }
        let (with_flags, _) = writer::build_image(
            &graph,
            &[],
            &[],
            writer::TrigramSource::Contents(&std::collections::HashMap::new()),
            Some(&test_ids),
            None,
            SALT,
        );
        let flagged = QueryIndex::from_bytes_with_salt(with_flags, SALT).expect("valid image");
        let plain = round_trip(&graph);
        assert_eq!(flagged.entity_count(), plain.entity_count());
        for at in 0..plain.entity_count() {
            let (l, r) = (flagged.entity(at), plain.entity(at));
            assert_eq!(l.id(), r.id());
            assert_eq!(l.name(), r.name());
            assert_eq!(l.entity_type(), r.entity_type());
            assert_eq!(l.file_path(), r.file_path());
            assert_eq!(l.start_line(), r.start_line());
            assert_eq!(l.end_line(), r.end_line());
            assert_eq!(l.parent_id(), r.parent_id());
            assert!(l.is_test() && !r.is_test());
        }
    }

    #[test]
    fn lookup_returns_every_entity_with_that_name() {
        let index = round_trip(&sample());
        let hits = index.lookup("shared");
        assert_eq!(hits.len(), 2);
        let mut ids: Vec<String> = hits.iter().map(|e| e.id()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a.ts::function::shared", "b.ts::class::shared"]);
    }

    #[test]
    fn lookup_round_trips_every_field() {
        let index = round_trip(&sample());
        let hits = index.lookup("beta");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.id(), "b.ts::function::beta");
        assert_eq!(hit.name(), "beta");
        assert_eq!(hit.entity_type(), "function");
        assert_eq!(hit.file_path(), "b.ts");
        assert_eq!(hit.start_line(), 40);
        assert_eq!(hit.end_line(), 45);
        assert_eq!(hit.parent_id(), None);
    }

    #[test]
    fn lookup_misses_are_empty_not_errors() {
        let index = round_trip(&sample());
        assert!(index.lookup("nonexistent").is_empty());
        assert!(index.lookup("").is_empty());
    }

    #[test]
    fn lookup_agrees_with_the_graph_for_every_name() {
        // Property 1 of the consistency oracle (QUERY-INDEX.md §6), in
        // miniature: the index's answer for every name in the graph is the
        // graph's own answer.
        let graph = sample();
        let index = round_trip(&graph);
        let names: std::collections::BTreeSet<&str> =
            graph.entities.values().map(|e| e.name.as_str()).collect();
        for name in names {
            let mut expected: Vec<&str> = graph
                .entities
                .values()
                .filter(|e| e.name == name)
                .map(|e| e.id.as_str())
                .collect();
            expected.sort_unstable();
            let mut actual: Vec<String> = index.lookup(name).iter().map(|e| e.id()).collect();
            actual.sort();
            assert_eq!(actual, expected, "name {name}");
        }
    }

    #[test]
    fn parent_ids_survive_the_index_relative_encoding() {
        let mut child = entity(
            "a.ts::class::Outer::method::inner",
            "inner",
            "method",
            "a.ts",
            5,
        );
        child.parent_id = Some("a.ts::class::Outer".to_string());
        let graph = graph_of(vec![
            entity("a.ts::class::Outer", "Outer", "class", "a.ts", 1),
            child,
        ]);
        let index = round_trip(&graph);
        let hits = index.lookup("inner");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].parent_id().as_deref(), Some("a.ts::class::Outer"));
    }

    #[test]
    fn entities_in_file_is_grouped_and_ordered() {
        let index = round_trip(&sample());
        let rows = index.entities_in_file("a.ts");
        let names: Vec<&str> = rows.iter().map(|e| e.name()).collect();
        assert_eq!(names, vec!["alpha", "shared"]);
        assert!(index.entities_in_file("missing.ts").is_empty());
    }

    #[test]
    fn files_under_matches_a_directory_prefix_not_a_string_prefix() {
        // `QUERY-INDEX.md` §7 item 1's oracle: index-backed listing must
        // agree with a directory walk on a fresh index, and must not
        // false-positive on a sibling whose name merely starts with the
        // same characters (`src` vs `src2`).
        let graph = graph_of(vec![
            entity(
                "src/a.ts::function::alpha",
                "alpha",
                "function",
                "src/a.ts",
                1,
            ),
            entity(
                "src/sub/b.ts::function::beta",
                "beta",
                "function",
                "src/sub/b.ts",
                1,
            ),
            entity(
                "src2/c.ts::function::gamma",
                "gamma",
                "function",
                "src2/c.ts",
                1,
            ),
            entity(
                "root.ts::function::delta",
                "delta",
                "function",
                "root.ts",
                1,
            ),
        ]);
        let index = round_trip(&graph);

        let mut under_src: Vec<&str> = index.files_under("src/");
        under_src.sort_unstable();
        assert_eq!(under_src, vec!["src/a.ts", "src/sub/b.ts"]);

        let mut under_sub: Vec<&str> = index.files_under("src/sub/");
        under_sub.sort_unstable();
        assert_eq!(under_sub, vec!["src/sub/b.ts"]);

        assert!(index.files_under("nowhere/").is_empty());

        let mut whole_repo: Vec<&str> = index.files_under("");
        whole_repo.sort_unstable();
        assert_eq!(
            whole_repo,
            vec!["root.ts", "src/a.ts", "src/sub/b.ts", "src2/c.ts"]
        );
    }

    #[test]
    fn files_under_reflects_only_what_the_index_built_from() {
        // Stale-membership behavior (§2's Verified/Complete split): a file
        // that appears on disk after the index was built is invisible to
        // `files_under` — Verified proves membership w.r.t. the index's own
        // `FILES` table, not the corpus's current file set. This is the
        // documented gap the doc comment names; the caller (not this tier)
        // owns deciding whether that gap is safe for its query shape.
        let graph = graph_of(vec![entity(
            "src/a.ts::function::alpha",
            "alpha",
            "function",
            "src/a.ts",
            1,
        )]);
        let index = round_trip(&graph);
        // Simulate a file added under src/ after the index was built: the
        // index was never told, so it must not appear.
        assert_eq!(index.files_under("src/"), vec!["src/a.ts"]);
    }

    #[test]
    fn ids_that_break_the_prefix_rule_fall_back_to_whole_ids() {
        // The JSON plugin emits `package.json::/name`, which does start with
        // the file path; a synthetic violation proves the fallback rather
        // than the happy path.
        let graph = graph_of(vec![
            entity("::detached::thing", "thing", "field", "a.json", 1),
            entity("a.json::/name", "name", "field", "a.json", 2),
        ]);
        let index = round_trip(&graph);
        let mut ids: Vec<String> = index
            .lookup("thing")
            .iter()
            .chain(index.lookup("name").iter())
            .map(|e| e.id())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["::detached::thing", "a.json::/name"]);
    }

    #[test]
    fn json_pointer_ids_round_trip_under_elision() {
        let graph = graph_of(vec![
            entity("package.json::/name", "name", "field", "package.json", 1),
            entity(
                "package.json::/version",
                "version",
                "field",
                "package.json",
                2,
            ),
        ]);
        let index = round_trip(&graph);
        assert_eq!(index.lookup("version")[0].id(), "package.json::/version");
    }

    #[test]
    fn a_stale_salt_reads_as_absent() {
        let bytes = build_salt(&sample(), &[], SALT);
        assert!(QueryIndex::from_bytes_with_salt(bytes, SALT + 1).is_none());
    }

    #[test]
    fn every_truncation_is_a_clean_miss_not_a_panic() {
        let bytes = build_salt(&sample(), &[], SALT);
        for cut in 0..bytes.len() {
            assert!(
                QueryIndex::from_bytes_with_salt(bytes[..cut].to_vec(), SALT).is_none(),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn an_empty_graph_produces_a_valid_empty_index() {
        let index = round_trip(&graph_of(Vec::new()));
        assert_eq!(index.entity_count(), 0);
        assert_eq!(index.file_count(), 0);
        assert!(index.lookup("anything").is_empty());
    }

    #[test]
    fn file_fingerprints_are_carried_for_files_with_no_entities() {
        // The file list is what lets the read path skip discovery, so a file
        // the graph never produced an entity for must still appear.
        let bytes = build_salt(
            &sample(),
            &[FileFingerprint {
                path: "empty.ts".to_string(),
                mtime_secs: 7,
                mtime_nanos: 8,
                content_hash: 9,
            }],
            SALT,
        );
        let index = QueryIndex::from_bytes_with_salt(bytes, SALT).expect("valid image");
        assert_eq!(index.file_count(), 3);
        assert!(index.entities_in_file("empty.ts").is_empty());
    }

    #[test]
    fn build_is_deterministic() {
        // Byte equality of two builds from the same graph is what the
        // consistency oracle's patch-equivalence property rests on.
        let graph = sample();
        assert_eq!(build_salt(&graph, &[], SALT), build_salt(&graph, &[], SALT));
    }

    /// `alpha` calls `beta` and `shared` (both directions); `beta` calls
    /// nothing; `shared` (b.ts's class) is called by both `alpha` and a
    /// second caller in a third file, so its callers row has 2 entries and
    /// exercises cross-file targets.
    fn graph_with_edges() -> EntityGraph {
        graph_of_with_edges(
            vec![
                entity("a.ts::function::alpha", "alpha", "function", "a.ts", 1),
                entity("b.ts::function::beta", "beta", "function", "b.ts", 1),
                entity("b.ts::class::shared", "shared", "class", "b.ts", 10),
                entity("c.ts::function::gamma", "gamma", "function", "c.ts", 1),
            ],
            vec![
                edge("a.ts::function::alpha", "b.ts::function::beta"),
                edge("a.ts::function::alpha", "b.ts::class::shared"),
                edge("c.ts::function::gamma", "b.ts::class::shared"),
            ],
        )
    }

    #[test]
    fn refs_of_returns_direct_dependencies() {
        let graph = graph_with_edges();
        let index = round_trip(&graph);
        let alpha = index.lookup("alpha");
        assert_eq!(alpha.len(), 1);
        let mut refs: Vec<String> = index
            .refs_of(alpha[0].index())
            .iter()
            .map(|e| e.id())
            .collect();
        refs.sort();
        assert_eq!(refs, vec!["b.ts::class::shared", "b.ts::function::beta"]);
    }

    #[test]
    fn callers_of_returns_direct_dependents_across_files() {
        let graph = graph_with_edges();
        let index = round_trip(&graph);
        let shared = index.lookup("shared");
        assert_eq!(shared.len(), 1);
        let mut callers: Vec<String> = index
            .callers_of(shared[0].index())
            .iter()
            .map(|e| e.id())
            .collect();
        callers.sort();
        assert_eq!(
            callers,
            vec!["a.ts::function::alpha", "c.ts::function::gamma"]
        );
    }

    #[test]
    fn refs_and_callers_are_empty_for_leaf_and_unreferenced_entities() {
        let graph = graph_with_edges();
        let index = round_trip(&graph);
        let beta = index.lookup("beta");
        assert!(index.refs_of(beta[0].index()).is_empty()); // beta calls nothing
        let gamma = index.lookup("gamma");
        assert!(index.callers_of(gamma[0].index()).is_empty()); // nobody calls gamma
    }

    #[test]
    fn refs_tier_is_present_once_s2_writes_it() {
        // `reserved_sections_read_as_absent` (format::tests) covers the
        // pre-S2 skeleton contract (`SectionRef::EMPTY`); this covers the
        // other side, now that the writer always emits CSR for a non-empty
        // graph — S3's TRIGRAM is the section still legitimately absent.
        assert!(round_trip(&graph_with_edges()).has_refs());
    }

    /// Property 1's refs analogue, in miniature (`QUERY-INDEX.md` §6, S2's
    /// obligation): for every entity in the graph, the index's callers/refs
    /// answer equals the graph's own `dependents`/`dependencies` maps.
    #[test]
    fn refs_and_callers_agree_with_the_graph_for_every_entity() {
        let graph = graph_with_edges();
        let index = round_trip(&graph);
        for entity in graph.entities.values() {
            let hits = index.lookup(&entity.name);
            let hit = hits
                .iter()
                .find(|e| e.id() == entity.id)
                .expect("entity present in index");

            let mut expected_deps = graph
                .dependencies()
                .get(entity.id.as_str())
                .cloned()
                .unwrap_or_default();
            expected_deps.sort();
            let mut got_deps: Vec<String> =
                index.refs_of(hit.index()).iter().map(|e| e.id()).collect();
            got_deps.sort();
            assert_eq!(got_deps, expected_deps, "refs of {}", entity.id);

            let mut expected_callers = graph
                .dependents()
                .get(entity.id.as_str())
                .cloned()
                .unwrap_or_default();
            expected_callers.sort();
            let mut got_callers: Vec<String> = index
                .callers_of(hit.index())
                .iter()
                .map(|e| e.id())
                .collect();
            got_callers.sort();
            assert_eq!(got_callers, expected_callers, "callers of {}", entity.id);
        }
    }

    /// semx-zvq: a graph whose edges span all three `RefType` variants
    /// between the same small set of entities, so packing/unpacking is
    /// exercised on every kind, not just the `Calls`-only fixture above.
    fn graph_with_typed_edges() -> EntityGraph {
        graph_of_with_edges(
            vec![
                entity("a.ts::function::alpha", "alpha", "function", "a.ts", 1),
                entity("b.ts::function::beta", "beta", "function", "b.ts", 1),
                entity("b.ts::class::Shared", "Shared", "class", "b.ts", 10),
                entity("c.ts::function::gamma", "gamma", "function", "c.ts", 1),
            ],
            vec![
                typed_edge(
                    "a.ts::function::alpha",
                    "b.ts::function::beta",
                    RefType::Calls,
                ),
                typed_edge(
                    "a.ts::function::alpha",
                    "b.ts::class::Shared",
                    RefType::TypeRef,
                ),
                typed_edge(
                    "c.ts::function::gamma",
                    "b.ts::class::Shared",
                    RefType::Imports,
                ),
            ],
        )
    }

    #[test]
    fn refs_are_typed_is_true_for_a_normal_round_trip() {
        assert!(round_trip(&graph_with_typed_edges()).refs_are_typed());
    }

    /// semx-zvq: `refs_of_typed`/`callers_of_typed` recover each edge's
    /// `ref_type` after the pack/unpack round trip through the image.
    #[test]
    fn refs_of_typed_recovers_each_edges_kind() {
        let graph = graph_with_typed_edges();
        let index = round_trip(&graph);
        let alpha = index.lookup("alpha");
        assert_eq!(alpha.len(), 1);

        let mut got: Vec<(String, RefType)> = index
            .refs_of_typed(alpha[0].index())
            .into_iter()
            .map(|(e, k)| (e.id(), k))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                ("b.ts::class::Shared".to_string(), RefType::TypeRef),
                ("b.ts::function::beta".to_string(), RefType::Calls),
            ]
        );
    }

    /// semx-zvq: the reverse (`callers_of_typed`) direction, and a target
    /// with callers of two different kinds (`TypeRef` from `alpha`,
    /// `Imports` from `gamma`) — proves the kind travels with the *edge*,
    /// not with the target entity.
    #[test]
    fn callers_of_typed_recovers_each_edges_kind() {
        let graph = graph_with_typed_edges();
        let index = round_trip(&graph);
        let shared = index.lookup("Shared");
        assert_eq!(shared.len(), 1);

        let mut got: Vec<(String, RefType)> = index
            .callers_of_typed(shared[0].index())
            .into_iter()
            .map(|(e, k)| (e.id(), k))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                ("a.ts::function::alpha".to_string(), RefType::TypeRef),
                ("c.ts::function::gamma".to_string(), RefType::Imports),
            ]
        );
    }

    /// semx-zvq: the untyped accessors must still decode the correct target
    /// entity id once the high nibble carries a non-zero kind — this is the
    /// regression `csr_row`'s `target_of` mask exists to prevent (an
    /// unmasked read would corrupt the entity index for any `TypeRef`/
    /// `Imports` edge, since those pack a non-zero high nibble).
    #[test]
    fn untyped_refs_and_callers_still_resolve_the_right_entity_when_kind_is_nonzero() {
        let graph = graph_with_typed_edges();
        let index = round_trip(&graph);
        let alpha = index.lookup("alpha");
        let mut refs: Vec<String> = index
            .refs_of(alpha[0].index())
            .iter()
            .map(|e| e.id())
            .collect();
        refs.sort();
        assert_eq!(refs, vec!["b.ts::class::Shared", "b.ts::function::beta"]);

        let shared = index.lookup("Shared");
        let mut callers: Vec<String> = index
            .callers_of(shared[0].index())
            .iter()
            .map(|e| e.id())
            .collect();
        callers.sort();
        assert_eq!(
            callers,
            vec!["a.ts::function::alpha", "c.ts::function::gamma"]
        );
    }

    #[test]
    fn a_torn_refs_section_is_a_clean_miss() {
        // A REFS section whose declared edge counts don't account for the
        // section's actual length must fail to open, not panic the first
        // time a caller reaches for `callers_of`/`refs_of`.
        let bytes = build_salt(&graph_with_edges(), &[], SALT);
        let refs = format::Header::read(&bytes, SALT).unwrap().sections[format::SEC_REFS];
        let mut corrupt = bytes.clone();
        // Truncate the REFS section by one byte and shrink the file to match,
        // which desyncs the declared edge counts from the section's length.
        corrupt.remove(refs.offset as usize + refs.len as usize - 1);
        assert!(QueryIndex::from_bytes_with_salt(corrupt, SALT).is_none());
    }

    #[test]
    fn a_torn_trigram_section_is_a_clean_miss() {
        // Same discipline as `a_torn_refs_section_is_a_clean_miss`: a
        // TRIGRAM section whose declared `posting_total` doesn't account for
        // the section's actual length must fail to open, not panic the
        // first time `grep` reaches for a posting. This is also the bead's
        // mutation test: corrupt a posting, the oracle (here, `open`) must
        // catch it.
        let mut contents = std::collections::HashMap::new();
        contents.insert("a.ts".to_string(), b"needle haystack".to_vec());
        let files = vec![writer::FileFingerprint {
            path: "a.ts".to_string(),
            ..Default::default()
        }];
        let (bytes, _stats) = writer::build_image(
            &graph_of(Vec::new()),
            &files,
            &[],
            writer::TrigramSource::Contents(&contents),
            None,
            None,
            SALT,
        );
        let trigram = format::Header::read(&bytes, SALT).unwrap().sections[format::SEC_TRIGRAM];
        assert!(
            trigram.len > 0,
            "test corpus must produce a real trigram section"
        );
        let mut corrupt = bytes.clone();
        // Truncate the TRIGRAM section by one byte and shrink the file to
        // match, which desyncs the declared posting_total from the
        // section's actual length — exactly what a corrupted/truncated
        // posting looks like on disk.
        corrupt.remove(trigram.offset as usize + trigram.len as usize - 1);
        assert!(QueryIndex::from_bytes_with_salt(corrupt, SALT).is_none());
    }

    #[test]
    fn grep_search_finds_matches_via_trigram_postings_and_verifies_against_live_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.ts"),
            "const needle = 1;\nconst other = 2;\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.ts"), "no match here\n").unwrap();

        let mut contents = std::collections::HashMap::new();
        contents.insert(
            "a.ts".to_string(),
            std::fs::read(dir.path().join("a.ts")).unwrap(),
        );
        contents.insert(
            "b.ts".to_string(),
            std::fs::read(dir.path().join("b.ts")).unwrap(),
        );
        let files: Vec<writer::FileFingerprint> = contents
            .keys()
            .map(|p| writer::FileFingerprint {
                path: p.clone(),
                ..Default::default()
            })
            .collect();
        let (bytes, _stats) = writer::build_image(
            &graph_of(Vec::new()),
            &files,
            &[],
            writer::TrigramSource::Contents(&contents),
            None,
            None,
            SALT,
        );
        let index = QueryIndex::from_bytes_with_salt(bytes, SALT).expect("valid image");

        let report = grep::search(
            &index,
            dir.path(),
            "needle",
            &grep::GrepOptions::default(),
            |_| Vec::new(),
        )
        .expect("valid pattern");
        assert_eq!(report.origin, grep::CandidateOrigin::Trigram);
        assert_eq!(report.candidate_files, 1);
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].file, "a.ts");
        assert_eq!(report.hits[0].line, 1);

        // A pattern whose trigram is provably absent from every file must
        // resolve with zero candidates and zero hits, without ever touching
        // the filesystem.
        let report = grep::search(
            &index,
            dir.path(),
            "zzzzz",
            &grep::GrepOptions::default(),
            |_| Vec::new(),
        )
        .expect("valid pattern");
        assert_eq!(report.origin, grep::CandidateOrigin::NoCandidates);
        assert!(report.hits.is_empty());
    }

    #[test]
    fn grep_search_reflects_live_content_not_stale_stored_bytes() {
        // The index stores no content at all — only trigram membership at
        // build time. A file edited after the index was built must be
        // matched (or not) against its CURRENT bytes, never anything cached.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ts"), "const needle = 1;\n").unwrap();
        let mut contents = std::collections::HashMap::new();
        contents.insert(
            "a.ts".to_string(),
            std::fs::read(dir.path().join("a.ts")).unwrap(),
        );
        let files = vec![writer::FileFingerprint {
            path: "a.ts".to_string(),
            ..Default::default()
        }];
        let (bytes, _stats) = writer::build_image(
            &graph_of(Vec::new()),
            &files,
            &[],
            writer::TrigramSource::Contents(&contents),
            None,
            None,
            SALT,
        );
        let index = QueryIndex::from_bytes_with_salt(bytes, SALT).expect("valid image");

        // Edit the file after the index was built, without touching the index.
        std::fs::write(dir.path().join("a.ts"), "needle is gone now\n").unwrap();
        let report = grep::search(
            &index,
            dir.path(),
            "needle",
            &grep::GrepOptions::default(),
            |_| Vec::new(),
        )
        .expect("valid pattern");
        assert_eq!(report.hits[0].text, "needle is gone now");
    }

    #[test]
    fn write_atomic_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INDEX_FILE_NAME);
        let bytes = build_salt(&sample(), &[], SALT);
        writer::write_atomic(&path, &bytes).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != std::ffi::OsStr::new(INDEX_FILE_NAME))
            .collect();
        assert!(leftovers.is_empty(), "temp files survived: {leftovers:?}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
