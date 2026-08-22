//! The build plane's warm-cache tier, and `index.sem`'s writer (semx-gpu
//! census, QUERY-INDEX.md §15; the SQL `DiskCache` itself moved to
//! `sem_core::persist::disk_cache` under semx-r94 — see that module's doc for
//! why `sem-core`, and for the divergences the unification reconciled).
//!
//! This crate is now a thin consumer: `DiskCache`/`PartialCache`/
//! `FileCacheColumns`/`CacheSourceScope` are re-exported from `sem-core`
//! unchanged, and this file keeps only what is genuinely `sem-cli`'s own —
//! `write_query_index`/`write_index_only` (`index.sem` is never written by
//! `sem-mcp`, per QUERY-INDEX.md §15.3's superseding note), the profiling
//! marks gated by `SEM_PROFILE_CACHE=1`, and three small orchestration
//! wrappers (`save_full_with_index`/`save_topology_with_index`/
//! `save_incremental_with_index`) that call the unified `DiskCache`'s SQL-only
//! save methods and then write `index.sem` from the same corpus read,
//! preserving the single-read fusion (semx-3tb) the old, since-deleted
//! `sem-cli`-local `DiskCache::save_with_test_dirs`/`save_topology` used to
//! perform internally — `DiskCache` itself couldn't keep doing that once it
//! also had to serve `sem-mcp`, which never writes `index.sem` at all
//! (`Needs(DiskCache) ⊋ declared(DiskCache)` if it had kept that side effect).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rayon::prelude::*;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::EntityGraph;
use sem_core::persist::disk_cache as shared_cache;

// Test-module support only (`use super::*`); production code above needs
// nothing but `EntityGraph`/`SemanticEntity`.
#[cfg(test)]
#[allow(unused_imports)]
use rusqlite::{params, Connection};
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::parser::graph::{EntityInfo, EntityInfoMap, EntityRef, RefType};

// `PartialCache` is used only structurally by callers (`partial.stale_files`
// etc., never named), so this bin crate's unused-import lint flags it even
// though it's part of the re-exported surface — same shape `pub struct
// PartialCache` had here before semx-r94 moved its definition to `sem-core`.
#[allow(unused_imports)]
pub use shared_cache::{CacheSourceScope, DiskCache, FileCacheColumns, PartialCache};

use crate::corpus_columns::CorpusColumns;

/// Sub-phase timing for the save path, gated by `SEM_PROFILE_CACHE=1` — the
/// same pattern `resolve_profile::enabled()` uses (`OnceLock<bool>`, one env
/// read ever, then a relaxed bool check). Zero-cost when unset beyond the
/// single cached bool load; never changes what is written, only what is
/// printed to stderr.
///
/// Coarser than before semx-r94 for the SQL phase specifically: the unified
/// `DiskCache::save_with_test_dirs_precomputed`'s own sub-phase marks
/// (`corpus_columns_single_read`'s SQL-side siblings — file insert, manifest
/// refresh, import refresh, entity/edge insert, test-flag write, commit) no
/// longer print individually now that method lives in `sem-core` rather than
/// this crate; this wrapper marks the whole SQL phase as one span
/// (`sqlite_save_total`) instead. `insert_entities_with_content_store`'s own
/// marks (`file_reads_parallel`/`entity_inserts_serial`/`compress_parallel`/
/// `file_contents_insert_serial`) are unaffected — they were already living
/// in the shared substrate before this bead.
fn cache_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SEM_PROFILE_CACHE").as_deref() == Ok("1"))
}

pub(crate) fn cache_profile_mark(phase: &str, t0: std::time::Instant) {
    if cache_profile_enabled() {
        eprintln!(
            "CACHE_SAVE_PHASE phase={phase} ms={:.2}",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// Byte spans for every entity that has one (semx-a3w), keyed by entity id —
/// the shape `index::build_with_trigrams_and_dirs_and_tests_and_spans` wants
/// for its `entity_byte_spans` parameter. The datum lives on `SemanticEntity`
/// (`start_byte`/`end_byte`), which the index writer never sees (it only
/// ever sees topology-only `EntityInfo`), so whoever *does* have the bodies
/// on hand derives it here and hands the map down. An entity whose
/// `start_byte`/`end_byte` are `None` (several extractors never set them —
/// see `format::entity`'s doc) is simply absent from the map, which is
/// exactly [`sem_core::index::format::NONE_U32`]'s "absent" contract on the
/// write side.
fn entity_byte_spans(entities: &[SemanticEntity]) -> HashMap<&str, (u32, u32)> {
    entities
        .iter()
        .filter_map(|e| {
            let start = u32::try_from(e.start_byte?).ok()?;
            let end = u32::try_from(e.end_byte?).ok()?;
            Some((e.id.as_str(), (start, end)))
        })
        .collect()
}

/// Project a `CorpusColumns` read (which also carries `index.sem`'s
/// trigrams) down to the subset `DiskCache`'s SQL save needs — no second
/// read, just a cheap in-memory reshape.
fn to_file_cache_columns(columns: &CorpusColumns) -> Vec<FileCacheColumns> {
    columns
        .rows
        .iter()
        .map(|r| FileCacheColumns {
            path: r.path.clone(),
            mtime_secs: r.mtime_secs,
            mtime_nanos: r.mtime_nanos,
            content_hash: r.hash_hex(),
            imports: r.imports.clone(),
        })
        .collect()
}

/// Write *only* the on-disk query index for a finished build — no SQLite
/// connection, no schema, no `cache.db` (semx-4ex, RESOLUTION-PROFILE.md
/// W4.5).
///
/// This is what `sem graph`'s cold/dirty build path does now. It is the whole
/// save plane that path's own answers need: `sem graph` is served forever
/// after from `index.sem` (`try_index_graph`), and W4's cache.db-deleted
/// experiment measured every other verb byte-identical and equally fast from
/// the index too. Producing the SQL mirror there was a write with no reader on
/// the verb that paid for it — the biggest single item in the cold build
/// (linux 24.3 s pre-W4, ~9 s post-W4).
///
/// The three inputs `write_query_index` cannot derive for itself are computed
/// here exactly as `save_full_with_index` computes them, and by the same
/// functions, so the image this writes is byte-identical to the one a full
/// save would have written beside its tables:
///
/// * the one post-graph corpus read (`CorpusColumns::read`, semx-3tb),
/// * the test classification (`filter_test_entities_with_custom_dirs` — the
///   same call `DiskCache`'s save methods make before inserting
///   `entity_flags`),
/// * the entity byte spans (semx-a3w).
///
/// Callers that *do* need the SQL mirror (the content-hydrating verbs, and
/// anyone who opts back in with `SEM_BUILD_CACHE=1`) keep calling
/// `save_full_with_index`/`save_topology_with_index`, which end with this same
/// `write_query_index` call.
pub(crate) fn write_index_only(
    root: &Path,
    files: &[String],
    graph: &EntityGraph,
    entities: &[SemanticEntity],
    custom_test_dirs: &[String],
) {
    // `write_query_index`'s own two O(1) gates, hoisted here — same shape
    // `impact.rs`'s `try_index_impact_deps` uses for its `SEM_NO_INDEX`
    // check (cheapest possible decline first). Without this, the
    // `SEM_NO_INDEX=1` escape hatch and an unresolvable cache dir still pay
    // for the corpus read + test-entity filter + byte-span pass below,
    // whose output `write_query_index` would otherwise discard the instant
    // it reached these same two checks itself.
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return;
    }
    if shared_cache::cache_dir_for_repo(root).is_none() {
        return;
    }

    let __columns_t0 = std::time::Instant::now();
    let columns = CorpusColumns::read(root, files, files);
    cache_profile_mark("corpus_columns_single_read", __columns_t0);
    let test_entity_ids = graph.filter_test_entities_with_custom_dirs(entities, custom_test_dirs);
    let byte_spans = entity_byte_spans(entities);
    let __index_t0 = std::time::Instant::now();
    write_query_index(
        root,
        files,
        graph,
        Some(&test_entity_ids),
        Some(&byte_spans),
        Some(columns),
    );
    cache_profile_mark("write_query_index_total", __index_t0);
}

pub(crate) fn write_query_index(
    root: &Path,
    files: &[String],
    graph: &EntityGraph,
    test_entity_ids: Option<&HashSet<&str>>,
    entity_byte_spans: Option<&HashMap<&str, (u32, u32)>>,
    columns: Option<CorpusColumns>,
) {
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return;
    }
    let Some(cache_dir) = shared_cache::cache_dir_for_repo(root) else {
        return;
    };
    if shared_cache::create_cache_dir(&cache_dir).is_err() {
        return;
    }

    // semx-3tb: the index's two inputs — the fingerprint's `content_hash`
    // (`parser::incremental::content_hash`, xxh3-64; QUERY-INDEX.md §3.5
    // pins the index to it so Verified freshness can compare against what a
    // re-extraction would produce) and the TRIGRAM tier's per-file trigram
    // sets (S3, semx-az9) — are two columns of the build's *one* corpus
    // read, which the save path performed before calling this. Only a caller
    // with no save behind it (`commands::query`'s index-only cold path)
    // passes `None` and reads here; then this *is* that one read, not a
    // second one.
    let __reread_t0 = std::time::Instant::now();
    let columns = columns.unwrap_or_else(|| CorpusColumns::read(root, files, files));
    cache_profile_mark("write_query_index_columns", __reread_t0);

    let fingerprints = columns.fingerprints();

    // DIRS (semx-ykf, `Complete` freshness — QUERY-INDEX.md §2/§10.6): every
    // distinct ancestor directory of every fingerprinted file, all levels
    // (`writer::DirFingerprint`'s doc explains why leaf parents alone aren't
    // enough), plus the repo root itself (`""`) so an empty corpus still has
    // one directory whose mtime bumps the moment a first file lands.
    let __dirs_t0 = std::time::Instant::now();
    let dirs = build_dir_fingerprints(root, &fingerprints);
    cache_profile_mark("write_query_index_dir_fingerprints", __dirs_t0);

    let trigrams = columns.into_trigrams();
    let __build_t0 = std::time::Instant::now();
    let (bytes, _trigram_stats) = sem_core::index::build_with_trigrams_and_dirs_and_tests_and_spans(
        graph,
        &fingerprints,
        &dirs,
        &trigrams,
        test_entity_ids,
        entity_byte_spans,
    );
    cache_profile_mark("write_query_index_build_image", __build_t0);
    let __write_t0 = std::time::Instant::now();
    let index_path = cache_dir.join(sem_core::index::INDEX_FILE_NAME);
    let _ = sem_core::index::write_atomic(&index_path, &bytes);
    cache_profile_mark("write_query_index_atomic_write", __write_t0);
}

/// Every ancestor directory of `path` (all levels), root-relative, `""` for
/// the repo root — `"src/compiler/program.ts"` yields `["", "src",
/// "src/compiler"]`. The file's own name is never itself a directory.
fn ancestors_of(path: &str) -> impl Iterator<Item = String> + '_ {
    let segs: Vec<&str> = path.split('/').collect();
    let depth = segs.len().saturating_sub(1);
    (0..=depth).map(move |n| segs[..n].join("/"))
}

/// Stat every distinct ancestor directory of `fingerprints`' paths, in
/// parallel — the same `stat` primitive `rows` above already used for files,
/// just over a much smaller set (directories, not files: §1.6's measured
/// ratio on the monster is 40,877 files to a few thousand directories).
/// Directories whose mtime can't be read (raced away between the file walk
/// and here) are silently dropped: a `DirFingerprint` this build never
/// recorded degrades to "always re-check" on the read side
/// (`complete::complete_check` treats a missing fingerprint as drifted), the
/// same "missing degrades to re-verify, never to false-fresh" discipline
/// `FileFingerprint`'s own doc already commits to.
fn build_dir_fingerprints(
    root: &Path,
    fingerprints: &[sem_core::index::FileFingerprint],
) -> Vec<sem_core::index::DirFingerprint> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    set.insert(String::new());
    for fp in fingerprints {
        set.extend(ancestors_of(&fp.path));
    }
    let paths: Vec<String> = set.into_iter().collect();
    paths
        .par_iter()
        .filter_map(|path| {
            let full = if path.is_empty() {
                root.to_path_buf()
            } else {
                root.join(path)
            };
            let (secs, nanos) = shared_cache::file_mtime_parts(&full)?;
            Some(sem_core::index::DirFingerprint {
                path: path.clone(),
                mtime_secs: secs,
                mtime_nanos: nanos as u32,
            })
        })
        .collect()
}

/// Full save (`cache.db`'s content tables) plus `index.sem`, from one corpus
/// read shared between both writes (semx-3tb, preserved across semx-r94's
/// move of the SQL half to `sem-core` — see this module's doc).
pub(crate) fn save_full_with_index(
    disk: &DiskCache,
    root: &Path,
    files: &[String],
    graph: &EntityGraph,
    entities: &[SemanticEntity],
    custom_test_dirs: &[String],
    source_scope: CacheSourceScope,
) -> Result<(), rusqlite::Error> {
    let __columns_t0 = std::time::Instant::now();
    let columns = CorpusColumns::read(root, files, files);
    cache_profile_mark("corpus_columns_single_read", __columns_t0);
    let precomputed = to_file_cache_columns(&columns);

    let __save_t0 = std::time::Instant::now();
    let test_entity_ids = disk.save_with_test_dirs_precomputed(
        root,
        files,
        graph,
        entities,
        custom_test_dirs,
        source_scope,
        Some(&precomputed),
    )?;
    cache_profile_mark("sqlite_save_total", __save_t0);

    let test_entity_id_refs: HashSet<&str> = test_entity_ids.iter().map(String::as_str).collect();
    let byte_spans = entity_byte_spans(entities);
    let __index_t0 = std::time::Instant::now();
    write_query_index(
        root,
        files,
        graph,
        Some(&test_entity_id_refs),
        Some(&byte_spans),
        Some(columns),
    );
    cache_profile_mark("write_query_index_total", __index_t0);
    Ok(())
}

/// Topology-only save plus `index.sem`, mirroring [`save_full_with_index`].
pub(crate) fn save_topology_with_index(
    disk: &DiskCache,
    root: &Path,
    files: &[String],
    graph: &EntityGraph,
    entities: &[SemanticEntity],
    custom_test_dirs: &[String],
    source_scope: CacheSourceScope,
) -> Result<(), rusqlite::Error> {
    let columns = CorpusColumns::read(root, files, files);
    let precomputed = to_file_cache_columns(&columns);

    let test_entity_ids = disk.save_topology_precomputed(
        root,
        files,
        graph,
        entities,
        custom_test_dirs,
        source_scope,
        Some(&precomputed),
    )?;

    let test_entity_id_refs: HashSet<&str> = test_entity_ids.iter().map(String::as_str).collect();
    let byte_spans = entity_byte_spans(entities);
    write_query_index(
        root,
        files,
        graph,
        Some(&test_entity_id_refs),
        Some(&byte_spans),
        Some(columns),
    );
    Ok(())
}

/// Incremental save plus `index.sem`. Unlike the two saves above, the
/// pre-unification `sem-cli` copy of this method never fused its `index.sem`
/// write with the SQL save's own read (it always passed `None, None, None`,
/// letting `write_query_index` read the corpus itself) — so there is nothing
/// to precompute here; this wrapper only relocates the trailing
/// `write_query_index` call that used to live inside `DiskCache`'s method
/// body, unchanged in every other respect.
#[allow(clippy::too_many_arguments)]
pub(crate) fn save_incremental_with_index(
    disk: &DiskCache,
    root: &Path,
    all_files: &[String],
    stale_files: &[String],
    graph: &EntityGraph,
    entities: &[SemanticEntity],
    repair_changed_clean_entity_ids: bool,
    recomputed_edge_source_ids: &[String],
    deleted_entity_ids: &[String],
    source_scope: CacheSourceScope,
) -> Result<(), rusqlite::Error> {
    disk.save_incremental_with_repair_metadata(
        root,
        all_files,
        stale_files,
        graph,
        entities,
        repair_changed_clean_entity_ids,
        recomputed_edge_source_ids,
        deleted_entity_ids,
        source_scope,
    )?;
    // No classification available here: the incremental path never
    // recomputes `entity_flags` either, so handing `None` keeps the two
    // stores honest about the same gap rather than stamping a stale
    // classification into the image (semx-zvq).
    write_query_index(root, all_files, graph, None, None, None);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_cache_root() -> &'static Path {
        static CACHE_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

        CACHE_ROOT
            .get_or_init(|| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let root = std::env::temp_dir()
                    .join(format!("sem-cli-test-cache-{}-{nanos}", std::process::id()));
                std::fs::create_dir_all(&root).unwrap();
                root
            })
            .as_path()
    }

    fn configure_test_cache_root() {
        std::env::set_var("SEM_CACHE_DIR", test_cache_root());
    }

    fn temp_repo_root(test_name: &str) -> std::path::PathBuf {
        configure_test_cache_root();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sem-cli-cache-{test_name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    fn empty_graph() -> EntityGraph {
        EntityGraph::from_parts(EntityInfoMap::default(), Vec::new())
    }

    fn entity(id: &str, file_path: &str, name: &str, content: &str) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: file_path.to_string(),
            entity_type: "function".to_string(),
            name: name.to_string(),
            parent_id: None,
            content: content.to_string(),
            content_hash: format!("hash:{content}"),
            structural_hash: None,

            kappa: None,
            start_line: 1,
            end_line: 1,
            start_byte: None,
            end_byte: None,
            metadata: None,
        }
    }

    fn entity_content(cache: &DiskCache, id: &str) -> Option<String> {
        let mut stmt = cache
            .connection()
            .prepare("SELECT content FROM entities WHERE id = ?1")
            .unwrap();
        let mut rows = stmt.query(rusqlite::params![id]).unwrap();
        rows.next().unwrap().map(|row| row.get(0).unwrap())
    }

    fn entity_info(id: &str, file_path: &str, name: &str) -> EntityInfo {
        EntityInfo {
            id: id.to_string(),
            file_path: file_path.to_string(),
            entity_type: "function".to_string(),
            name: name.to_string(),
            parent_id: None,
            start_line: 1,
            end_line: 1,
        }
    }

    fn graph_with_edges(entities: &[SemanticEntity], edges: Vec<EntityRef>) -> EntityGraph {
        let entity_map: EntityInfoMap = entities
            .iter()
            .map(|entity| {
                (
                    entity.id.clone(),
                    entity_info(&entity.id, &entity.file_path, &entity.name),
                )
            })
            .collect();
        EntityGraph::from_parts(entity_map, edges)
    }

    fn edge(from_entity: &str, to_entity: &str) -> EntityRef {
        EntityRef {
            from_entity: from_entity.to_string(),
            to_entity: to_entity.to_string(),
            ref_type: RefType::Calls,
        }
    }

    fn edge_rowid(cache: &DiskCache, from_entity: &str, to_entity: &str) -> Option<i64> {
        cache
            .connection()
            .query_row(
                "SELECT rowid FROM edges WHERE from_entity = ?1 AND to_entity = ?2",
                rusqlite::params![from_entity, to_entity],
                |row| row.get(0),
            )
            .ok()
    }

    fn edge_count(cache: &DiskCache, from_entity: &str, to_entity: &str) -> i64 {
        cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE from_entity = ?1 AND to_entity = ?2",
                rusqlite::params![from_entity, to_entity],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn file_import_count(cache: &DiskCache, importing_file: &str, imported_file: &str) -> i64 {
        cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE importing_file = ?1 AND imported_file = ?2",
                rusqlite::params![importing_file, imported_file],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn cached_file_mtime(cache: &DiskCache, file: &str) -> (i64, i64) {
        cache
            .connection()
            .query_row(
                "SELECT mtime_secs, mtime_nanos FROM files WHERE path = ?1",
                rusqlite::params![file],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    fn sample_files(root: &Path) -> Vec<String> {
        write_file(&root.join("sample.foo"), "export const alpha = () => 1;\n");
        vec!["sample.foo".to_string()]
    }

    fn cleanup(root: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(&root);
        if let Some(cache_dir) = shared_cache::cache_dir_for_repo(&root) {
            let _ = std::fs::remove_dir_all(cache_dir);
        }
    }

    fn save_empty_cache(root: &Path, files: &[String]) -> DiskCache {
        let cache = DiskCache::open(root).unwrap();
        cache
            .save(
                root,
                files,
                &empty_graph(),
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();
        assert!(cache.load(root, files).is_some());
        cache
    }

    #[test]
    fn topology_cache_loads_only_topology_readers() {
        let root = temp_repo_root("topology-only-cache");
        write_file(&root.join("a.rs"), "fn a() {}\n");
        write_file(&root.join("b.rs"), "fn b() { a(); }\n");
        write_file(&root.join("a_test.rs"), "#[test]\nfn test_a() { a(); }\n");
        let files = vec![
            "b.rs".to_string(),
            "a.rs".to_string(),
            "a_test.rs".to_string(),
        ];
        let entities = vec![
            entity("b-id", "b.rs", "b", "fn b() { a(); }"),
            entity("a-id", "a.rs", "a", "fn a() {}"),
            entity(
                "test-id",
                "a_test.rs",
                "test_a",
                "#[test]\nfn test_a() { a(); }",
            ),
        ];
        let graph = graph_with_edges(
            &entities,
            vec![edge("b-id", "a-id"), edge("test-id", "a-id")],
        );
        let cache = DiskCache::open(&root).unwrap();
        cache
            .save_topology(
                &root,
                &files,
                &graph,
                &entities,
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        assert!(cache.load(&root, &files).is_none());
        let topology = cache.load_graph_topology(&root, &files).unwrap();
        assert_eq!(topology.entities.len(), 3);
        assert_eq!(topology.edges.len(), 2);
        let (_, test_entity_ids) = cache
            .load_graph_topology_with_test_ids(&root, &files)
            .unwrap();
        assert!(test_entity_ids.contains("test-id"));
        assert!(!test_entity_ids.contains("a-id"));

        rewrite_after_mtime_tick(&root.join("a.rs"), "fn a() { let _x = 1; }\n");
        assert!(cache.load_partial(&root, &files).is_none());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn save_topology_records_file_imports() {
        let root = temp_repo_root("topology-file-imports");
        write_file(
            &root.join("a.ts"),
            "export function target() { return 1; }\n",
        );
        write_file(
            &root.join("b.ts"),
            "import { target } from './a';\nexport function useIt() { return target(); }\n",
        );
        let files = vec!["a.ts".to_string(), "b.ts".to_string()];
        let entities = vec![
            entity(
                "a-id",
                "a.ts",
                "target",
                "export function target() { return 1; }",
            ),
            entity(
                "b-id",
                "b.ts",
                "useIt",
                "export function useIt() { return target(); }",
            ),
        ];
        let graph = graph_with_edges(&entities, vec![edge("b-id", "a-id")]);
        let cache = DiskCache::open(&root).unwrap();

        cache
            .save_topology(
                &root,
                &files,
                &graph,
                &entities,
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        assert_eq!(file_import_count(&cache, "b.ts", "a.ts"), 1);

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn cache_reuse_requires_matching_source_scope_and_incremental_preserves_it() {
        let root = temp_repo_root("source-scope-cache-reuse");
        write_file(&root.join("a.ts"), "export function a() { return 1; }\n");
        write_file(&root.join("b.ts"), "export function b() { return a(); }\n");
        let files = vec!["a.ts".to_string(), "b.ts".to_string()];
        let entities = vec![
            entity("a-id", "a.ts", "a", "export function a() { return 1; }"),
            entity("b-id", "b.ts", "b", "export function b() { return a(); }"),
        ];
        let graph = graph_with_edges(&entities, vec![edge("b-id", "a-id")]);
        let cache = DiskCache::open(&root).unwrap();

        cache
            .save(
                &root,
                &files,
                &graph,
                &entities,
                shared_cache::CacheSourceScope::Custom,
            )
            .unwrap();

        assert!(cache
            .load_with_source_scope(&root, &files, shared_cache::CacheSourceScope::Default)
            .is_none());
        assert!(cache
            .load_with_source_scope(&root, &files, shared_cache::CacheSourceScope::Custom)
            .is_some());
        assert!(cache
            .load_partial_with_source_scope(&root, &files, shared_cache::CacheSourceScope::Default)
            .is_none());

        rewrite_after_mtime_tick(&root.join("b.ts"), "export function b() { return 2; }\n");
        let partial = cache
            .load_partial_with_source_scope(&root, &files, shared_cache::CacheSourceScope::Custom)
            .unwrap();
        assert_eq!(partial.stale_files, vec!["b.ts"]);

        let updated_entities = vec![
            entity("a-id", "a.ts", "a", "export function a() { return 1; }"),
            entity("b-id", "b.ts", "b", "export function b() { return 2; }"),
        ];
        let updated_graph = graph_with_edges(&updated_entities, vec![]);
        cache
            .save_incremental_with_repair_metadata(
                &root,
                &files,
                &partial.stale_files,
                &updated_graph,
                &updated_entities,
                false,
                &["b-id".to_string()],
                &[],
                shared_cache::CacheSourceScope::Custom,
            )
            .unwrap();

        assert!(cache
            .load_with_source_scope(&root, &files, shared_cache::CacheSourceScope::Default)
            .is_none());
        assert!(cache
            .load_with_source_scope(&root, &files, shared_cache::CacheSourceScope::Custom)
            .is_some());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn load_refreshes_mtime_when_file_content_is_unchanged() {
        let root = temp_repo_root("mtime-only-refresh");
        let file_contents = [
            ("same_a.rs", "fn same_a() {}\n"),
            ("same_b.rs", "fn same_b() {}\n"),
            ("same_c.rs", "fn same_c() {}\n"),
        ];
        for (file, content) in &file_contents {
            write_file(&root.join(*file), content);
        }
        let files: Vec<String> = file_contents
            .iter()
            .map(|(file, _)| (*file).to_string())
            .collect();
        let cache = save_empty_cache(&root, &files);
        let before: Vec<(i64, i64)> = files
            .iter()
            .map(|file| cached_file_mtime(&cache, file))
            .collect();

        let rewrite_all = || -> Vec<(i64, i64)> {
            for (file, content) in &file_contents {
                rewrite_after_mtime_tick(&root.join(*file), content);
            }
            file_contents
                .iter()
                .map(|(file, _)| shared_cache::file_mtime_parts(&root.join(*file)).unwrap())
                .collect()
        };
        let assert_cached_mtimes = |expected: &[(i64, i64)]| {
            for (file, expected) in files.iter().zip(expected) {
                assert_eq!(cached_file_mtime(&cache, file), *expected);
            }
        };

        let full_current = rewrite_all();
        assert!(before
            .iter()
            .zip(&full_current)
            .all(|(before, current)| before != current));
        assert!(cache.load(&root, &files).is_some());
        assert_cached_mtimes(&full_current);

        let topology_current = rewrite_all();
        assert!(cache.load_graph_topology(&root, &files).is_some());
        assert_cached_mtimes(&topology_current);

        let partial_current = rewrite_all();
        assert!(cache.load_partial(&root, &files).is_none());
        assert_cached_mtimes(&partial_current);

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn cache_loads_ignore_fingerprint_refresh_failure() {
        let root = temp_repo_root("refresh-failure-cache-hit");
        write_file(&root.join("same.rs"), "fn same() {}\n");
        write_file(&root.join("stale.rs"), "fn stale() {}\n");
        let files = vec!["same.rs".to_string(), "stale.rs".to_string()];
        let cache = save_empty_cache(&root, &files);
        let before_same = cached_file_mtime(&cache, "same.rs");

        cache
            .connection()
            .execute_batch(
                "CREATE TRIGGER fail_fingerprint_refresh
                 BEFORE UPDATE ON files
                 BEGIN
                     SELECT RAISE(FAIL, 'stop refresh');
                 END;",
            )
            .unwrap();

        rewrite_after_mtime_tick(&root.join("same.rs"), "fn same() {}\n");
        assert!(cache.load(&root, &files).is_some());
        assert!(cache.load_graph_topology(&root, &files).is_some());
        assert_eq!(cached_file_mtime(&cache, "same.rs"), before_same);

        rewrite_after_mtime_tick(&root.join("stale.rs"), "fn stale() { 1; }\n");
        let partial = cache.load_partial(&root, &files).unwrap();
        assert_eq!(partial.stale_files, vec!["stale.rs"]);
        assert_eq!(cached_file_mtime(&cache, "same.rs"), before_same);

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn partial_cache_reports_clean_files_that_import_stale_js_ts_files() {
        let root = temp_repo_root("incremental-import-metadata");
        write_file(
            &root.join("a.ts"),
            "import { target } from './b';\nexport function useIt() { return target(); }\n",
        );
        write_file(
            &root.join("b.ts"),
            "export function target() { return 1; }\n",
        );
        write_file(
            &root.join("c.ts"),
            "export function other() { return 2; }\n",
        );
        let files = vec!["a.ts".to_string(), "b.ts".to_string(), "c.ts".to_string()];
        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(
                &root,
                &files,
                &empty_graph(),
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        assert_eq!(file_import_count(&cache, "a.ts", "b.ts"), 1);

        rewrite_after_mtime_tick(
            &root.join("b.ts"),
            "export function target() { return 3; }\n",
        );
        rewrite_after_mtime_tick(
            &root.join("c.ts"),
            "export function other() { return 2; }\n",
        );
        let current_c = shared_cache::file_mtime_parts(&root.join("c.ts")).unwrap();
        let partial = cache.load_partial(&root, &files).unwrap();
        assert_eq!(partial.stale_files, vec!["b.ts"]);
        assert_eq!(partial.cached_importing_stale_files, vec!["a.ts"]);
        assert_eq!(cached_file_mtime(&cache, "c.ts"), current_c);

        rewrite_after_mtime_tick(
            &root.join("a.ts"),
            "import { other } from './c';\nexport function useIt() { return other(); }\n",
        );
        cache
            .save_incremental_with_repair_metadata(
                &root,
                &files,
                &["a.ts".to_string()],
                &empty_graph(),
                &[],
                false,
                &[],
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();
        assert_eq!(file_import_count(&cache, "a.ts", "b.ts"), 0);
        assert_eq!(file_import_count(&cache, "a.ts", "c.ts"), 1);

        drop(cache);
        cleanup(root);
    }

    fn write_gitattributes(root: &Path) {
        write_file(
            &root.join(".gitattributes"),
            "*.foo linguist-language=javascript\n",
        );
    }

    fn rewrite_after_mtime_tick(path: &Path, content: &str) {
        let before = shared_cache::file_mtime_parts(path).unwrap();

        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            write_file(path, content);
            if shared_cache::file_mtime_parts(path).unwrap() != before {
                return;
            }
        }

        panic!("mtime did not change for {}", path.display());
    }

    fn read_user_version(cache: &DiskCache) -> i32 {
        cache
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn assert_lookup_indexes(cache: &DiskCache) {
        let mut stmt = cache
            .connection()
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index' AND name NOT LIKE 'sqlite_autoindex%'
                 ORDER BY name",
            )
            .unwrap();
        let indexes: HashSet<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|result| result.unwrap())
            .collect();

        for (expected, _, _) in shared_cache::CACHE_INDEXES {
            assert!(indexes.contains(*expected), "missing index {expected}");
        }
    }

    fn assert_table_empty(cache: &DiskCache, table: &str) {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let count: i64 = cache.connection().query_row(&sql, [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "{table} should be empty after schema rebuild");
    }

    fn seed_unsupported_cache(root: &Path, version: i32) {
        let cache_dir = shared_cache::cache_dir_for_repo(root).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA user_version = {version};
             CREATE TABLE files (
                 path TEXT PRIMARY KEY,
                 mtime_secs INTEGER NOT NULL,
                 mtime_nanos INTEGER NOT NULL
             );
             CREATE TABLE entities (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 entity_type TEXT NOT NULL,
                 file_path TEXT NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 structural_hash TEXT,
                 parent_id TEXT,
                 metadata_json TEXT
             );
             CREATE TABLE edges (
                 from_entity TEXT NOT NULL,
                 to_entity TEXT NOT NULL,
                 ref_type TEXT NOT NULL
             );
             INSERT INTO files (path, mtime_secs, mtime_nanos)
             VALUES ('stale.rs', 1, 2);
             INSERT INTO entities (
                 id, name, entity_type, file_path, start_line, end_line,
                 content, content_hash, structural_hash, parent_id, metadata_json
             )
             VALUES (
                 'stale-id', 'stale', 'function', 'stale.rs', 1, 1,
                 'fn stale() {{}}', 'old-content', NULL, NULL, NULL
             );
             INSERT INTO edges (from_entity, to_entity, ref_type)
             VALUES ('stale-id', 'other-id', 'calls');"
        ))
        .unwrap();
    }

    #[test]
    fn load_invalidates_when_gitattributes_is_added() {
        let root = temp_repo_root("gitattributes-added");
        let files = sample_files(&root);
        let cache = save_empty_cache(&root, &files);

        write_file(
            &root.join(".gitattributes"),
            "*.foo linguist-language=javascript\n",
        );

        assert!(cache.load(&root, &files).is_none());
        assert!(cache.load_partial(&root, &files).is_none());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn load_invalidates_when_gitattributes_is_modified() {
        let root = temp_repo_root("gitattributes-modified");
        let files = sample_files(&root);
        let gitattributes = root.join(".gitattributes");
        write_file(&gitattributes, "*.foo linguist-language=javascript\n");
        let cache = save_empty_cache(&root, &files);

        rewrite_after_mtime_tick(&gitattributes, "*.foo linguist-language=typescript\n");

        assert!(cache.load(&root, &files).is_none());
        assert!(cache.load_partial(&root, &files).is_none());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn load_refreshes_gitattributes_mtime_when_content_is_unchanged() {
        let root = temp_repo_root("gitattributes-mtime-only-refresh");
        let files = sample_files(&root);
        let gitattributes = root.join(".gitattributes");
        let content = "*.foo linguist-language=javascript\n";
        write_file(&gitattributes, content);
        let cache = save_empty_cache(&root, &files);
        let cache_key = shared_cache::CACHE_MANIFEST_FILES
            .iter()
            .find_map(|(file_name, cache_key)| {
                (*file_name == ".gitattributes").then_some(*cache_key)
            })
            .unwrap();
        let before = cached_file_mtime(&cache, cache_key);

        rewrite_after_mtime_tick(&gitattributes, content);
        let current = shared_cache::file_mtime_parts(&gitattributes).unwrap();

        assert_ne!(before, current);
        assert!(cache.load(&root, &files).is_some());
        assert_eq!(cached_file_mtime(&cache, cache_key), current);

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn load_invalidates_when_gitattributes_is_removed() {
        let root = temp_repo_root("gitattributes-removed");
        let files = sample_files(&root);
        let gitattributes = root.join(".gitattributes");
        write_file(&gitattributes, "*.foo linguist-language=javascript\n");
        let cache = save_empty_cache(&root, &files);

        std::fs::remove_file(&gitattributes).unwrap();

        assert!(cache.load(&root, &files).is_none());
        assert!(cache.load_partial(&root, &files).is_none());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn save_incremental_keeps_clean_entity_rows_without_clean_id_repair() {
        let root = temp_repo_root("incremental-entities");
        write_file(&root.join("stale.rs"), "fn stale() {}\n");
        write_file(&root.join("clean.rs"), "fn clean() {}\n");
        let files = vec!["stale.rs".to_string(), "clean.rs".to_string()];
        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(
                &root,
                &files,
                &empty_graph(),
                &[
                    entity("stale-id", "stale.rs", "stale", "stale old"),
                    entity("clean-id", "clean.rs", "clean", "clean old"),
                ],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        let entities = vec![
            entity("stale-id", "stale.rs", "stale", "stale new"),
            entity("clean-id", "clean.rs", "clean", "clean should stay cached"),
        ];
        cache
            .save_incremental_with_repair_metadata(
                &root,
                &files,
                &["stale.rs".to_string()],
                &empty_graph(),
                &entities,
                false,
                &["stale-id".to_string()],
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        assert_eq!(
            entity_content(&cache, "stale-id"),
            Some("stale new".to_string())
        );
        assert_eq!(
            entity_content(&cache, "clean-id"),
            Some("clean old".to_string())
        );

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn save_incremental_rewrites_entities_after_clean_id_repair() {
        let root = temp_repo_root("incremental-clean-repair");
        write_file(&root.join("stale.rs"), "fn stale() {}\n");
        write_file(&root.join("clean.rs"), "fn clean() {}\n");
        let files = vec!["stale.rs".to_string(), "clean.rs".to_string()];
        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(
                &root,
                &files,
                &empty_graph(),
                &[
                    entity("stale-id", "stale.rs", "stale", "stale old"),
                    entity("clean-old-id", "clean.rs", "clean", "clean old"),
                ],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        let entities = vec![
            entity("stale-id", "stale.rs", "stale", "stale new"),
            entity("clean-new-id", "clean.rs", "clean", "clean repaired"),
        ];
        cache
            .save_incremental_with_repair_metadata(
                &root,
                &files,
                &["stale.rs".to_string()],
                &empty_graph(),
                &entities,
                true,
                &[],
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        assert_eq!(entity_content(&cache, "clean-old-id"), None);
        assert_eq!(
            entity_content(&cache, "clean-new-id"),
            Some("clean repaired".to_string())
        );
        assert_eq!(
            entity_content(&cache, "stale-id"),
            Some("stale new".to_string())
        );

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn save_incremental_rewrites_only_recomputed_edge_sources() {
        let root = temp_repo_root("incremental-edge-sources");
        write_file(&root.join("stale.rs"), "fn stale() {}\n");
        write_file(&root.join("clean.rs"), "fn clean() {}\n");
        write_file(&root.join("other.rs"), "fn other() {}\n");
        write_file(&root.join("target.rs"), "fn target() {}\n");
        let files = vec![
            "stale.rs".to_string(),
            "clean.rs".to_string(),
            "other.rs".to_string(),
            "target.rs".to_string(),
        ];
        let cache = DiskCache::open(&root).unwrap();
        let entities = vec![
            entity("stale-id", "stale.rs", "stale", "stale old"),
            entity("clean-id", "clean.rs", "clean", "clean old"),
            entity("other-id", "other.rs", "other", "other"),
            entity("old-target-id", "target.rs", "oldTarget", "old target"),
            entity("new-target-id", "target.rs", "newTarget", "new target"),
        ];
        let initial_graph = graph_with_edges(
            &entities,
            vec![
                edge("stale-id", "old-target-id"),
                edge("clean-id", "other-id"),
            ],
        );
        cache
            .save(
                &root,
                &files,
                &initial_graph,
                &entities,
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();
        let clean_edge_rowid = edge_rowid(&cache, "clean-id", "other-id").unwrap();

        let updated_graph = graph_with_edges(
            &entities,
            vec![
                edge("stale-id", "new-target-id"),
                edge("clean-id", "other-id"),
            ],
        );
        cache
            .save_incremental_with_repair_metadata(
                &root,
                &files,
                &["stale.rs".to_string()],
                &updated_graph,
                &entities,
                false,
                &["stale-id".to_string()],
                &["old-target-id".to_string()],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();

        assert_eq!(edge_count(&cache, "stale-id", "old-target-id"), 0);
        assert_eq!(edge_count(&cache, "stale-id", "new-target-id"), 1);
        assert_eq!(
            edge_rowid(&cache, "clean-id", "other-id"),
            Some(clean_edge_rowid)
        );

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn cli_and_mcp_caches_share_manifest_entries() {
        let cli_to_mcp = temp_repo_root("cli-to-mcp");
        let cli_to_mcp_files = sample_files(&cli_to_mcp);
        write_gitattributes(&cli_to_mcp);
        let cli_cache = DiskCache::open(&cli_to_mcp).unwrap();
        cli_cache
            .save(
                &cli_to_mcp,
                &cli_to_mcp_files,
                &empty_graph(),
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();
        let mcp_cache = shared_cache::DiskCache::open(&cli_to_mcp).unwrap();
        assert!(mcp_cache.load(&cli_to_mcp, &cli_to_mcp_files).is_some());
        drop(mcp_cache);
        drop(cli_cache);
        cleanup(cli_to_mcp);

        let mcp_to_cli = temp_repo_root("mcp-to-cli");
        let mcp_to_cli_files = sample_files(&mcp_to_cli);
        write_gitattributes(&mcp_to_cli);
        let mcp_cache = shared_cache::DiskCache::open(&mcp_to_cli).unwrap();
        mcp_cache
            .save(
                &mcp_to_cli,
                &mcp_to_cli_files,
                &empty_graph(),
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();
        let cli_cache = DiskCache::open(&mcp_to_cli).unwrap();
        assert!(cli_cache.load(&mcp_to_cli, &mcp_to_cli_files).is_some());
        drop(cli_cache);
        drop(mcp_cache);
        cleanup(mcp_to_cli);

        let cli_topology_to_mcp = temp_repo_root("cli-topology-to-mcp");
        let cli_topology_to_mcp_files = sample_files(&cli_topology_to_mcp);
        let cli_cache = DiskCache::open(&cli_topology_to_mcp).unwrap();
        cli_cache
            .save_topology(
                &cli_topology_to_mcp,
                &cli_topology_to_mcp_files,
                &empty_graph(),
                &[],
                &[],
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();
        let mcp_cache = shared_cache::DiskCache::open(&cli_topology_to_mcp).unwrap();
        assert!(mcp_cache
            .load(&cli_topology_to_mcp, &cli_topology_to_mcp_files)
            .is_none());
        assert!(mcp_cache
            .load_graph_topology(&cli_topology_to_mcp, &cli_topology_to_mcp_files)
            .is_some());
        drop(mcp_cache);
        drop(cli_cache);
        cleanup(cli_topology_to_mcp);
    }

    #[test]
    fn open_creates_schema_version_and_lookup_indexes() {
        let root = temp_repo_root("schema");
        let cache = DiskCache::open(&root).unwrap();

        assert_eq!(
            read_user_version(&cache),
            shared_cache::CACHE_SCHEMA_VERSION
        );
        assert_lookup_indexes(&cache);
        assert!(shared_cache::cache_db_path(&root).unwrap().exists());
        assert!(!root.join(".sem").exists());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn open_uses_shared_external_cache_path() {
        let root = temp_repo_root("external-path");
        let cache = DiskCache::open(&root).unwrap();

        assert!(shared_cache::cache_db_path(&root).unwrap().exists());
        assert!(!root.join(".sem").exists());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn open_rebuilds_cache_when_schema_version_is_unsupported() {
        for version in [
            0,
            shared_cache::CACHE_SCHEMA_VERSION - 1,
            shared_cache::CACHE_SCHEMA_VERSION + 1,
        ] {
            let root = temp_repo_root(&format!("unsupported-{version}"));
            seed_unsupported_cache(&root, version);

            let cache = DiskCache::open(&root).unwrap();

            assert_eq!(
                read_user_version(&cache),
                shared_cache::CACHE_SCHEMA_VERSION
            );
            assert_lookup_indexes(&cache);
            for table in ["files", "entities", "edges"] {
                assert_table_empty(&cache, table);
            }

            drop(cache);
            cleanup(root);
        }
    }

    /// v1.1 (semx-2i2): kappa must round-trip through the on-disk SQLite
    /// entity cache, which previously always dropped it (KAPPA.md's
    /// "coverage gaps" section, closed here by the `entities.kappa` column
    /// and the `CACHE_SCHEMA_VERSION` bump). Uses REAL extraction (the
    /// actual `CodeParserPlugin`, not a hand-built entity) so this proves
    /// kappa is both computed AND preserved through a genuine
    /// save/reopen/load cycle -- not just that the column exists.
    #[test]
    fn kappa_round_trips_through_disk_cache() {
        use sem_core::parser::plugin::SemanticParserPlugin;
        use sem_core::parser::plugins::code::CodeParserPlugin;

        let root = temp_repo_root("kappa-round-trip");
        let source = "let mutableCounter = 1;\nconst frozenCounter = 2;\n\nfunction add(a: number, b: number): number {\n    return a + b;\n}\n";
        write_file(&root.join("decls.ts"), source);
        let files = vec!["decls.ts".to_string()];

        let entities = CodeParserPlugin.extract_entities(source, "decls.ts");
        assert_eq!(entities.len(), 3, "expected 3 entities: {entities:#?}");
        for e in &entities {
            assert!(
                e.kappa.is_some(),
                "entity `{}` should have kappa computed pre-cache",
                e.name
            );
        }
        let original_kappa: std::collections::HashMap<String, String> = entities
            .iter()
            .map(|e| (e.name.clone(), e.kappa.clone().unwrap()))
            .collect();
        // Sanity: this fixture only proves something if let/const differ --
        // re-assert the v1.1 fix at the point of use, not just in kappa.rs.
        assert_ne!(
            original_kappa["mutableCounter"], original_kappa["frozenCounter"],
            "sanity: let vs const must differ before the cache round-trip \
             is even exercised"
        );

        let graph = graph_with_edges(&entities, vec![]);
        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(
                &root,
                &files,
                &graph,
                &entities,
                shared_cache::CacheSourceScope::Default,
            )
            .unwrap();
        drop(cache);

        // Reopen fresh -- a genuinely new connection/process-equivalent load,
        // not reusing any in-memory state from the save above.
        let reopened = DiskCache::open(&root).unwrap();
        let (_graph, loaded_entities) = reopened
            .load(&root, &files)
            .expect("cache should be fresh and complete");
        assert_eq!(loaded_entities.len(), 3);

        for loaded in &loaded_entities {
            let expected = original_kappa
                .get(&loaded.name)
                .unwrap_or_else(|| panic!("no original kappa recorded for `{}`", loaded.name));
            assert_eq!(
                loaded.kappa.as_deref(),
                Some(expected.as_str()),
                "entity `{}` must keep its kappa through a save/reopen/load \
                 cycle -- got {:?}, expected Some({expected:?})",
                loaded.name,
                loaded.kappa
            );
        }
        // And the differentiator that matters survives the round trip too.
        let kappa_of = |name: &str| {
            loaded_entities
                .iter()
                .find(|e| e.name == name)
                .and_then(|e| e.kappa.clone())
                .unwrap()
        };
        assert_ne!(
            kappa_of("mutableCounter"),
            kappa_of("frozenCounter"),
            "let vs const must still differ after a cache round-trip"
        );

        drop(reopened);
        cleanup(root);
    }
}
