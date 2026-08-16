//! `DiskCache`: the build plane's warm-cache tier (semx-gpu census, QUERY-
//! INDEX.md §15). Persists the *finished* `EntityGraph` (+ `SemanticEntity`
//! bodies) to `cache.db` so a later `sem` invocation can reload it in one
//! SQLite read instead of paying parse+resolve again — the tier `graph.rs`'s
//! `get_or_build_graph*` family tries before falling back to
//! `build_graph_with_facts_store` (semx-9en's pre-resolution facts store,
//! which only ever saves a *reparse*, not a *re-resolve*). Renamed from
//! `cache.rs`: every symbol that remained after semx-zvq's deletion cascade
//! (QUERY-INDEX.md §12.3) turned out to be this one tier — saves, loads,
//! their freshness gates, and the index write this tier's own saves also
//! trigger — so the module name now says what the file has actually been
//! since that bead, rather than the generic name it inherited from when it
//! also held the SQL query fast paths §12.3 deleted.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use rayon::prelude::*;
use rusqlite::{params, Connection};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityInfoMap, EntityRef, RefType};
use sem_mcp::cache as shared_cache;

use crate::corpus_columns::CorpusColumns;

/// Sub-phase timing for the save path, gated by `SEM_PROFILE_CACHE=1` — the
/// same pattern `resolve_profile::enabled()` uses (`OnceLock<bool>`, one env
/// read ever, then a relaxed bool check). Added for semx-8lf's physics-budget
/// census: `cache_full_save` was previously one opaque `SEM_TIMINGS` mark
/// (graph.rs) covering a serial per-file re-read+rehash, the SQLite writes,
/// and a *second* parallel per-file re-read+rehash inside `write_query_index`
/// — three internally-undifferentiated passes bundled into one number.
/// Zero-cost when unset beyond the single cached bool load; never changes
/// what is written, only what is printed to stderr.
fn cache_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
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

/// Set once a cache has computed test flags, so All/Tests-mode impact can trust
/// the cached `entity_flags` (an empty set then means "no tests", not "unknown").
const META_TEST_FLAGS: &str = "test_flags_computed";

/// Stamp the origin repo so `sem repos` can label local storage: the cache
/// directory's name is a hash of the repo path, so this row is the only thing
/// that maps it back. Best-effort.
///
/// All that survives of `store_freshness_epoch` (semx-zvq). The git HEAD oid,
/// clean-tree flag and oracle-eligibility columns it also wrote existed
/// solely to feed `git_oracle_says_fresh`, deleted with the rest of the git
/// freshness oracle — so every cache save also stops paying for a `git
/// status` shell-out and a full `git2` index walk, on the *write* path, for a
/// read-path shortcut nothing takes any more.
fn store_repo_origin(tx: &rusqlite::Transaction<'_>, root: &Path) {
    let _ = shared_cache::set_cache_repo_root(tx, root);
}

/// Compute and persist which entities are tests, plus a marker so a later
/// point query can trust an EMPTY result as "no tests" rather than "not
/// computed". Shared by the full and topology saves — this is what lets
/// All/Tests-mode `sem impact` answer from the indexed cache (the "covered by
/// N tests" field) instead of hydrating the whole graph.
/// Returns the classification it persisted, so the same value can be handed
/// to `write_query_index` (semx-zvq) instead of being recomputed — the two
/// stores must agree on what a test is or the index-backed `sem impact
/// --tests`/`--all` reroute would answer differently from the SQL path it
/// displaces.
fn write_test_flags<'a>(
    tx: &rusqlite::Transaction<'_>,
    graph: &EntityGraph,
    entities: &'a [SemanticEntity],
    custom_test_dirs: &[String],
) -> Result<HashSet<&'a str>, rusqlite::Error> {
    let test_entity_ids = graph.filter_test_entities_with_custom_dirs(entities, custom_test_dirs);
    {
        let mut stmt =
            tx.prepare("INSERT INTO entity_flags (entity_id, is_test) VALUES (?1, 1)")?;
        for entity_id in &test_entity_ids {
            stmt.execute(params![entity_id])?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO cache_metadata (key, value) VALUES (?1, '1')",
        params![META_TEST_FLAGS],
    )?;
    Ok(test_entity_ids)
}

/// Byte spans for every entity that has one (semx-a3w), keyed by entity id —
/// the shape `index::build_with_trigrams_and_dirs_and_tests_and_spans` wants
/// for its `entity_byte_spans` parameter. Mirrors `write_test_flags`'s
/// reason for existing: the datum lives on `SemanticEntity`
/// (`start_byte`/`end_byte`), which the index writer never sees (it only
/// ever sees topology-only `EntityInfo`), so whoever *does* have the bodies
/// on hand — every caller of `write_query_index` that also has `entities`
/// — must derive it here and hand the map down rather than the writer
/// re-deriving it (it has nothing to re-derive it from). An entity whose
/// `start_byte`/`end_byte` are `None` (several extractors never set them —
/// see `format::entity`'s doc) is simply absent from the map, which is
/// exactly [`format::NONE_U32`]'s "absent" contract on the write side.
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

/// Result of a partial cache load: stale files that need reparsing, plus cached clean data.
pub struct PartialCache {
    pub stale_files: Vec<String>,
    pub cached_entities: Vec<SemanticEntity>,
    pub cached_edges: Vec<EntityRef>,
    pub cached_importing_stale_files: Vec<String>,
    /// Cached entities from stale files (for entity-level content_hash comparison)
    pub stale_file_entities: Vec<SemanticEntity>,
}

pub struct DiskCache {
    conn: Connection,
}

// `EntityListingJsonRow` (+ `query_entities_listing`/`write_entities_listing_json`
// below it, and their sole gate `has_fresh_topology_cache_for_files`) deleted —
// QUERY-INDEX.md §7 item 4 (partial), §12 update (semx-woe S4 second half).
// `entities.rs`'s directory-listing branch now answers from
// `QueryIndex::files_under` (§7 item 1's enabler) with the same fallback
// discipline S2 established; these SQL-listing functions had exactly one
// caller each (grep-verified before deletion) and neither is reachable
// once that caller stops calling them.

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
/// here exactly as `save_with_test_dirs` computes them, and by the same
/// functions, so the image this writes is byte-identical to the one the SQL
/// save would have written beside its tables:
///
/// * the one post-graph corpus read (`CorpusColumns::read`, semx-3tb),
/// * the test classification (`filter_test_entities_with_custom_dirs` — the
///   same call `write_test_flags` makes before inserting `entity_flags`),
/// * the entity byte spans (semx-a3w).
///
/// Callers that *do* need the SQL mirror (the content-hydrating verbs, and
/// anyone who opts back in with `SEM_BUILD_CACHE=1`) keep calling
/// `save_with_test_dirs`/`save_topology`, which end with this same
/// `write_query_index` call.
pub(crate) fn write_index_only(
    root: &Path,
    files: &[String],
    graph: &EntityGraph,
    entities: &[SemanticEntity],
    custom_test_dirs: &[String],
) {
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
    // second one. Either way `CorpusColumns::read` extracts trigrams inside
    // its own closure and drops the bytes, so the whole-corpus
    // `HashMap<String, Vec<u8>>` that used to live from here through the
    // image build (1.5 GB on linux, alongside the per-file sets the writer
    // derived from it anyway) no longer exists.
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

impl DiskCache {
    pub fn open(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let cache_dir = shared_cache::cache_dir_for_repo(repo_root)
            .ok_or_else(|| rusqlite::Error::InvalidPath(repo_root.to_path_buf()))?;
        shared_cache::create_cache_dir(&cache_dir)?;
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path)?;

        shared_cache::initialize_schema(&conn)?;

        Ok(Self { conn })
    }

    /// [`Self::open`] for the *read* side: declines instead of creating an
    /// empty `cache.db` (semx-4ex).
    ///
    /// `Connection::open` creates the file, so every cache-tier probe used to
    /// leave a schema-only SQLite database behind even on a path that never
    /// wrote a row — which was invisible while every cold build ended in a
    /// `save*`, and is a lie on disk now that
    /// [`CacheMissSavePolicy::IndexOnly`](super::commands::graph) exists.
    /// Behaviour is unchanged for every caller: a `cache.db` that does not
    /// exist has no rows, so every `load*`/`has_fresh_*` below would have
    /// missed anyway. Callers that intend to *write* still use `open`.
    pub fn open_existing(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let cache_dir = shared_cache::cache_dir_for_repo(repo_root)
            .ok_or_else(|| rusqlite::Error::InvalidPath(repo_root.to_path_buf()))?;
        let db_path = cache_dir.join("cache.db");
        if !db_path.exists() {
            return Err(rusqlite::Error::InvalidPath(db_path));
        }
        Self::open(repo_root)
    }

    /// Test-only convenience: no production caller ever saves without a
    /// `custom_test_dirs` list (`graph.rs`'s `get_or_build_graph*` family
    /// always goes through `save_with_test_dirs`/`save_topology`) — verified
    /// by a release-build `dead_code` warning before this bead's census
    /// (semx-gpu, QUERY-INDEX.md §15) gated it here rather than deleting it
    /// outright and rewriting nine test call sites for no behavioral gain.
    #[cfg(test)]
    pub fn save(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        self.save_with_test_dirs(root, files, graph, entities, &[], source_scope)
    }

    /// Full save that also records test flags, so `sem impact` in the default
    /// (All) mode can be answered from the indexed point-query instead of
    /// hydrating the whole graph. Previously only `save_topology` wrote test
    /// flags, so a full cache forced All/Tests-mode impact down the slow path.
    pub fn save_with_test_dirs(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        custom_test_dirs: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch(
            "DELETE FROM files; DELETE FROM entities; DELETE FROM edges; DELETE FROM file_imports; DELETE FROM entity_flags;",
        )?;

        // semx-3tb (SINGLE-PASS.md §2): the build's one post-graph corpus
        // read. Census passes H (`file_fingerprint`), I
        // (`refresh_file_import_entries`'s own read) and L
        // (`write_query_index`'s re-read) all consumed the same bytes to
        // derive columns that are folds over them — fold-fusion says those
        // are one walk, so this is that walk. What used to be three reads and
        // two xxh3 passes is one read and one hash; `files.content_hash` is
        // its hex rendering (`FileColumns::hash_hex`), the index's
        // `FileFingerprint.content_hash` is the same number unrendered.
        let __columns_t0 = std::time::Instant::now();
        let columns = CorpusColumns::read(root, files, files);
        cache_profile_mark("corpus_columns_single_read", __columns_t0);

        {
            let __insert_t0 = std::time::Instant::now();
            let mut stmt = tx.prepare(
                "INSERT INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for row in &columns.rows {
                stmt.execute(params![
                    row.path,
                    row.mtime_secs,
                    row.mtime_nanos,
                    row.hash_hex()
                ])?;
            }
            cache_profile_mark("file_fingerprint_insert", __insert_t0);
        }

        {
            let __t0 = std::time::Instant::now();
            shared_cache::refresh_manifest_entries(&tx, root)?;
            cache_profile_mark("refresh_manifest_entries", __t0);
        }
        {
            let __t0 = std::time::Instant::now();
            shared_cache::refresh_file_import_entries_precomputed(
                &tx,
                files,
                &columns.imports_by_file(),
            )?;
            cache_profile_mark("refresh_file_import_entries", __t0);
        }

        {
            let __t0 = std::time::Instant::now();
            shared_cache::insert_entities_with_content_store(
                &tx,
                root,
                &entities.iter().collect::<Vec<_>>(),
                true,
            )?;
            cache_profile_mark("insert_entities_with_content", __t0);
        }

        // Insert edges with prepared statement
        {
            let __t0 = std::time::Instant::now();
            let mut stmt = tx.prepare(
                "INSERT INTO edges (from_entity, to_entity, ref_type) VALUES (?1, ?2, ?3)",
            )?;
            for edge in &graph.edges {
                let rt = match edge.ref_type {
                    RefType::Calls => "calls",
                    RefType::TypeRef => "typeref",
                    RefType::Imports => "imports",
                };
                stmt.execute(params![edge.from_entity, edge.to_entity, rt])?;
            }
            cache_profile_mark("insert_edges", __t0);
        }

        let __testflags_t0 = std::time::Instant::now();
        let test_entity_ids = write_test_flags(&tx, graph, entities, custom_test_dirs)?;
        cache_profile_mark("write_test_flags", __testflags_t0);
        let __spans_t0 = std::time::Instant::now();
        let byte_spans = entity_byte_spans(entities);
        cache_profile_mark("entity_byte_spans", __spans_t0);

        shared_cache::set_cache_kind(&tx, shared_cache::CACHE_KIND_FULL)?;
        shared_cache::set_cache_source_scope(&tx, source_scope)?;
        store_repo_origin(&tx, root);
        let __commit_t0 = std::time::Instant::now();
        tx.commit()?;
        cache_profile_mark("sqlite_commit", __commit_t0);
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
        Ok(())
    }

    pub fn save_topology(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        custom_test_dirs: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch(
            "DELETE FROM files; DELETE FROM entities; DELETE FROM edges; DELETE FROM file_imports; DELETE FROM entity_flags;",
        )?;

        // The same one corpus read as `save_with_test_dirs` (semx-3tb).
        let columns = CorpusColumns::read(root, files, files);

        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for row in &columns.rows {
                stmt.execute(params![
                    row.path,
                    row.mtime_secs,
                    row.mtime_nanos,
                    row.hash_hex()
                ])?;
            }
        }

        shared_cache::refresh_manifest_entries(&tx, root)?;
        shared_cache::refresh_file_import_entries_precomputed(
            &tx,
            files,
            &columns.imports_by_file(),
        )?;

        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO entities (id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', '', NULL, ?7, NULL)",
            )?;
            for e in graph.entities.values() {
                stmt.execute(params![
                    e.id,
                    e.name,
                    e.entity_type,
                    e.file_path,
                    e.start_line as i64,
                    e.end_line as i64,
                    e.parent_id,
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO edges (from_entity, to_entity, ref_type) VALUES (?1, ?2, ?3)",
            )?;
            for edge in &graph.edges {
                let rt = match edge.ref_type {
                    RefType::Calls => "calls",
                    RefType::TypeRef => "typeref",
                    RefType::Imports => "imports",
                };
                stmt.execute(params![edge.from_entity, edge.to_entity, rt])?;
            }
        }

        let test_entity_ids = write_test_flags(&tx, graph, entities, custom_test_dirs)?;
        let byte_spans = entity_byte_spans(entities);

        shared_cache::set_cache_kind(&tx, shared_cache::CACHE_KIND_TOPOLOGY)?;
        shared_cache::set_cache_source_scope(&tx, source_scope)?;
        store_repo_origin(&tx, root);
        tx.commit()?;
        write_query_index(
            root,
            files,
            graph,
            Some(&test_entity_ids),
            Some(&byte_spans),
            Some(columns),
        );
        Ok(())
    }

    #[cfg(test)]
    pub fn load(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<(EntityGraph, Vec<SemanticEntity>)> {
        self.load_with_source_scope(root, files, shared_cache::CacheSourceScope::Default)
    }

    pub fn load_with_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Option<(EntityGraph, Vec<SemanticEntity>)> {
        if !self.has_fresh_complete_cache(root, files, source_scope) {
            return None;
        }

        let mut entity_stmt = self
            .conn
            .prepare("SELECT id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json, start_byte, end_byte, kappa FROM entities")
            .ok()?;
        let entities: Vec<SemanticEntity> = entity_stmt
            .query_map([], |row| {
                let metadata_json: Option<String> = row.get(10)?;
                let metadata = metadata_json.and_then(|j| serde_json::from_str(&j).ok());
                Ok(SemanticEntity {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    entity_type: row.get(2)?,
                    file_path: row.get(3)?,
                    start_line: row.get::<_, i64>(4)? as usize,
                    end_line: row.get::<_, i64>(5)? as usize,
                    start_byte: row.get::<_, Option<i64>>(11)?.map(|v| v as usize),
                    end_byte: row.get::<_, Option<i64>>(12)?.map(|v| v as usize),
                    content: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    content_hash: row.get(7)?,
                    structural_hash: row.get(8)?,

                    kappa: row.get(13)?,
                    parent_id: row.get(9)?,
                    metadata,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        // Reconstruct span-sliced entity bodies from the file-content store.
        let entities: Vec<SemanticEntity> = {
            let mut recon = shared_cache::ContentReconstructor::new(&self.conn);
            entities
                .into_iter()
                .map(|mut e| {
                    if e.content.is_empty() {
                        e.content = recon.content(&e.file_path, None, e.start_byte, e.end_byte);
                    }
                    e
                })
                .collect()
        };

        let mut edge_stmt = self
            .conn
            .prepare("SELECT from_entity, to_entity, ref_type FROM edges")
            .ok()?;
        let edges: Vec<EntityRef> = edge_stmt
            .query_map([], |row| {
                let rt: String = row.get(2)?;
                let ref_type = match rt.as_str() {
                    "calls" => RefType::Calls,
                    "imports" => RefType::Imports,
                    _ => RefType::TypeRef,
                };
                Ok(EntityRef {
                    from_entity: row.get(0)?,
                    to_entity: row.get(1)?,
                    ref_type,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        let entity_map: EntityInfoMap = entities
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    EntityInfo {
                        id: e.id.clone(),
                        name: e.name.clone(),
                        entity_type: e.entity_type.clone(),
                        file_path: e.file_path.clone(),
                        start_line: e.start_line,
                        end_line: e.end_line,
                        parent_id: e.parent_id.clone(),
                    },
                )
            })
            .collect();

        let graph = EntityGraph::from_parts(entity_map, edges);
        Some((graph, entities))
    }

    /// Load only graph topology from a fresh cache.
    #[cfg(test)]
    pub fn load_graph_topology(&self, root: &Path, files: &[String]) -> Option<EntityGraph> {
        self.load_graph_topology_with_source_scope(
            root,
            files,
            shared_cache::CacheSourceScope::Default,
        )
    }

    pub fn load_graph_topology_with_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Option<EntityGraph> {
        if !self.has_fresh_topology_cache(root, files, source_scope) {
            return None;
        }

        self.load_graph_topology_rows()
    }

    #[cfg(test)]
    pub fn load_graph_topology_with_test_ids(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<(EntityGraph, HashSet<String>)> {
        self.load_graph_topology_with_test_ids_and_source_scope(
            root,
            files,
            shared_cache::CacheSourceScope::Default,
        )
    }

    pub fn load_graph_topology_with_test_ids_and_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Option<(EntityGraph, HashSet<String>)> {
        if !self.has_fresh_topology_only_cache(root, files, source_scope) {
            return None;
        }

        let graph = self.load_graph_topology_rows()?;
        let test_entity_ids = self.load_test_entity_ids()?;
        Some((graph, test_entity_ids))
    }

    /// Query a fresh topology cache directly for impact data without hydrating
    /// the complete in-memory graph.
    fn load_graph_topology_rows(&self) -> Option<EntityGraph> {
        let mut entity_stmt = self
            .conn
            .prepare(
                "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id FROM entities",
            )
            .ok()?;
        let entity_map: EntityInfoMap = entity_stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                Ok((
                    id.clone(),
                    EntityInfo {
                        id,
                        name: row.get(1)?,
                        entity_type: row.get(2)?,
                        file_path: row.get(3)?,
                        start_line: row.get::<_, i64>(4)? as usize,
                        end_line: row.get::<_, i64>(5)? as usize,
                        parent_id: row.get(6)?,
                    },
                ))
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        let edges = self.load_edges()?;
        Some(EntityGraph::from_parts(entity_map, edges))
    }

    fn load_test_entity_ids(&self) -> Option<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT entity_id FROM entity_flags WHERE is_test != 0")
            .ok()?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        Some(ids)
    }

    fn has_fresh_complete_cache(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> bool {
        if !shared_cache::cache_has_kind(&self.conn, &[shared_cache::CACHE_KIND_FULL]) {
            return false;
        }

        self.has_fresh_cache(root, files, source_scope)
    }

    fn has_fresh_topology_cache(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> bool {
        if !shared_cache::cache_has_kind(
            &self.conn,
            &[
                shared_cache::CACHE_KIND_FULL,
                shared_cache::CACHE_KIND_TOPOLOGY,
            ],
        ) {
            return false;
        }

        self.has_fresh_cache(root, files, source_scope)
    }

    fn has_fresh_topology_only_cache(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> bool {
        if !shared_cache::cache_has_kind(&self.conn, &[shared_cache::CACHE_KIND_TOPOLOGY]) {
            return false;
        }

        self.has_fresh_cache(root, files, source_scope)
    }

    fn has_fresh_cache(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> bool {
        if !shared_cache::cache_has_source_scope(&self.conn, source_scope) {
            return false;
        }

        if shared_cache::is_manifest_stale(&self.conn, root) {
            return false;
        }

        let cached_count: i64 = match self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        {
            Ok(count) => count,
            Err(_) => return false,
        };
        if (cached_count - shared_cache::manifest_entry_count(&self.conn)) as usize
            != shared_cache::source_file_count(files)
        {
            return false;
        }

        let cached_mtimes: HashMap<String, (i64, i64, Option<String>)> = {
            let Ok(mut stmt) = self
                .conn
                .prepare("SELECT path, mtime_secs, mtime_nanos, content_hash FROM files")
            else {
                return false;
            };
            let cached_mtimes = match stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ),
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => return false,
            };
            cached_mtimes
        };

        let mut fingerprint_refreshes = Vec::new();
        for file in files {
            if shared_cache::is_manifest_file_name(file) {
                continue;
            }
            let Some((secs, nanos, content_hash)) = cached_mtimes.get(file.as_str()) else {
                return false;
            };
            let full = root.join(file);
            match shared_cache::file_freshness(&full, *secs, *nanos, content_hash.as_deref()) {
                Some(shared_cache::FileFreshness::Fresh) => {}
                Some(shared_cache::FileFreshness::FreshWithUpdatedFingerprint {
                    secs,
                    nanos,
                    content_hash,
                }) => {
                    fingerprint_refreshes.push(shared_cache::FileFingerprintRefresh {
                        path: file.clone(),
                        mtime_secs: secs,
                        mtime_nanos: nanos,
                        content_hash,
                    });
                }
                Some(shared_cache::FileFreshness::Stale) | None => return false,
            }
        }

        shared_cache::refresh_file_fingerprints_best_effort(&self.conn, &fingerprint_refreshes);
        true
    }

    fn load_edges(&self) -> Option<Vec<EntityRef>> {
        let mut edge_stmt = self
            .conn
            .prepare("SELECT from_entity, to_entity, ref_type FROM edges")
            .ok()?;
        let edges: Vec<EntityRef> = edge_stmt
            .query_map([], |row| {
                let rt: String = row.get(2)?;
                let ref_type = match rt.as_str() {
                    "calls" => RefType::Calls,
                    "imports" => RefType::Imports,
                    _ => RefType::TypeRef,
                };
                Ok(EntityRef {
                    from_entity: row.get(0)?,
                    to_entity: row.get(1)?,
                    ref_type,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        Some(edges)
    }

    /// Load a partial cache: identify stale files and return clean cached data.
    /// Returns None if cache is empty or ALL files are stale (full rebuild is better).
    #[cfg(test)]
    pub fn load_partial(&self, root: &Path, files: &[String]) -> Option<PartialCache> {
        self.load_partial_with_source_scope(root, files, shared_cache::CacheSourceScope::Default)
    }

    pub fn load_partial_with_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Option<PartialCache> {
        if !shared_cache::cache_has_kind(&self.conn, &[shared_cache::CACHE_KIND_FULL]) {
            return None;
        }

        if !shared_cache::cache_has_source_scope(&self.conn, source_scope) {
            return None;
        }

        if shared_cache::is_manifest_stale(&self.conn, root) {
            return None;
        }

        // Load all cached file paths + mtimes
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime_secs, mtime_nanos, content_hash FROM files")
            .ok()?;
        let cached_files: HashMap<String, (i64, i64, Option<String>)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ),
                ))
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        if cached_files.is_empty() {
            return None;
        }

        let source_files: Vec<&String> = files
            .iter()
            .filter(|file| !shared_cache::is_manifest_file_name(file))
            .collect();
        let source_file_count = source_files.len();
        let current_set: HashSet<&str> = source_files.iter().map(|file| file.as_str()).collect();

        // Find stale source files: mtime differs or not in cache
        let mut stale_source_files: Vec<String> = Vec::new();
        let mut stale_current_file_count = 0;
        let mut fingerprint_refreshes = Vec::new();
        for file in &source_files {
            match cached_files.get(file.as_str()) {
                Some((secs, nanos, content_hash)) => {
                    let full = root.join(file.as_str());
                    match shared_cache::file_freshness(
                        &full,
                        *secs,
                        *nanos,
                        content_hash.as_deref(),
                    ) {
                        Some(shared_cache::FileFreshness::Fresh) => {}
                        Some(shared_cache::FileFreshness::FreshWithUpdatedFingerprint {
                            secs,
                            nanos,
                            content_hash,
                        }) => {
                            fingerprint_refreshes.push(shared_cache::FileFingerprintRefresh {
                                path: (*file).clone(),
                                mtime_secs: secs,
                                mtime_nanos: nanos,
                                content_hash,
                            });
                        }
                        Some(shared_cache::FileFreshness::Stale) | None => {
                            stale_current_file_count += 1;
                            stale_source_files.push((*file).clone());
                        }
                    }
                }
                None => {
                    stale_current_file_count += 1;
                    stale_source_files.push((*file).clone());
                }
            }
        }

        // Files in cache but not on disk anymore count as stale/deleted
        let mut deleted_cached_files: Vec<String> = Vec::new();
        for cached_path in cached_files.keys() {
            if !shared_cache::is_cache_manifest_key(cached_path)
                && !shared_cache::is_manifest_file_name(cached_path)
                && !current_set.contains(cached_path.as_str())
            {
                deleted_cached_files.push(cached_path.clone());
            }
        }

        shared_cache::refresh_file_fingerprints_best_effort(&self.conn, &fingerprint_refreshes);

        // If nothing stale, full load would have worked
        if stale_source_files.is_empty() && deleted_cached_files.is_empty() {
            return None;
        }

        // If everything is stale, skip incremental
        if stale_current_file_count >= source_file_count {
            return None;
        }

        let stale_set: HashSet<&str> = stale_source_files
            .iter()
            .chain(deleted_cached_files.iter())
            .map(|s| s.as_str())
            .collect();
        let mut import_stale_files = stale_source_files.clone();
        import_stale_files.extend(deleted_cached_files.iter().cloned());
        let cached_importing_stale_files = shared_cache::cached_importing_files_for_stale_files(
            &self.conn,
            &import_stale_files,
            &source_files,
        );

        // Load ALL entities, split into clean vs stale-file
        let mut entity_stmt = self
            .conn
            .prepare("SELECT id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json, start_byte, end_byte, kappa FROM entities")
            .ok()?;
        let mut cached_entities = Vec::new();
        let mut stale_file_entities = Vec::new();
        let mut recon = shared_cache::ContentReconstructor::new(&self.conn);
        let mut entity_rows = entity_stmt.query([]).ok()?;
        while let Some(row) = entity_rows.next().ok()? {
            let metadata_json: Option<String> = row.get(10).ok()?;
            let entity = SemanticEntity {
                id: row.get(0).ok()?,
                name: row.get(1).ok()?,
                entity_type: row.get(2).ok()?,
                file_path: row.get(3).ok()?,
                start_line: row.get::<_, i64>(4).ok()? as usize,
                end_line: row.get::<_, i64>(5).ok()? as usize,
                start_byte: row.get::<_, Option<i64>>(11).ok()?.map(|v| v as usize),
                end_byte: row.get::<_, Option<i64>>(12).ok()?.map(|v| v as usize),
                content: row.get::<_, Option<String>>(6).ok()?.unwrap_or_default(),
                content_hash: row.get(7).ok()?,
                structural_hash: row.get(8).ok()?,

                kappa: row.get(13).ok()?,
                parent_id: row.get(9).ok()?,
                metadata: metadata_json.and_then(|j| serde_json::from_str(&j).ok()),
            };
            let mut entity = entity;
            if entity.content.is_empty() {
                entity.content =
                    recon.content(&entity.file_path, None, entity.start_byte, entity.end_byte);
            }
            if stale_set.contains(entity.file_path.as_str()) {
                stale_file_entities.push(entity);
            } else {
                cached_entities.push(entity);
            }
        }

        // Load ALL cached edges (build_incremental decides which to keep)
        let mut edge_stmt = self
            .conn
            .prepare("SELECT from_entity, to_entity, ref_type FROM edges")
            .ok()?;
        let cached_edges: Vec<EntityRef> = edge_stmt
            .query_map([], |row| {
                let rt: String = row.get(2)?;
                let ref_type = match rt.as_str() {
                    "calls" => RefType::Calls,
                    "imports" => RefType::Imports,
                    _ => RefType::TypeRef,
                };
                Ok(EntityRef {
                    from_entity: row.get(0)?,
                    to_entity: row.get(1)?,
                    ref_type,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        Some(PartialCache {
            stale_files: stale_source_files,
            cached_entities,
            cached_edges,
            cached_importing_stale_files,
            stale_file_entities,
        })
    }

    /// Incrementally update the cache with graph-repair metadata.
    pub fn save_incremental_with_repair_metadata(
        &self,
        root: &Path,
        all_files: &[String],
        stale_files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        repair_changed_clean_entity_ids: bool,
        recomputed_edge_source_ids: &[String],
        deleted_entity_ids: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        let source_stale_files: Vec<&String> = stale_files
            .iter()
            .filter(|file| !shared_cache::is_manifest_file_name(file))
            .collect();
        let source_stale_set: HashSet<&str> = source_stale_files
            .iter()
            .map(|file| file.as_str())
            .collect();

        let tx = self.conn.unchecked_transaction()?;

        // Delete stale file entries
        {
            let mut del_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for f in &source_stale_files {
                del_files.execute(params![f])?;
            }
        }

        let current_set: HashSet<&str> = all_files
            .iter()
            .map(|s| s.as_str())
            .filter(|path| !shared_cache::is_manifest_file_name(path))
            .collect();
        let cached_paths: Vec<String> = {
            let mut cached_stmt = tx.prepare("SELECT path FROM files")?;
            cached_stmt
                .query_map([], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        };
        let deleted_cached_files: Vec<String> = cached_paths
            .into_iter()
            .filter(|path| {
                !shared_cache::is_cache_manifest_key(path)
                    && !shared_cache::is_manifest_file_name(path)
                    && !current_set.contains(path.as_str())
            })
            .collect();
        let use_legacy_edge_fallback = !repair_changed_clean_entity_ids
            && recomputed_edge_source_ids.is_empty()
            && deleted_entity_ids.is_empty();
        let cached_rewritten_entity_ids: HashSet<String> = if use_legacy_edge_fallback {
            let rewritten_file_paths: HashSet<&str> = source_stale_files
                .iter()
                .map(|file| file.as_str())
                .chain(deleted_cached_files.iter().map(String::as_str))
                .collect();
            let mut stmt = tx.prepare("SELECT id, file_path FROM entities")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.filter_map(|row| row.ok())
                .filter_map(|(id, file_path)| {
                    rewritten_file_paths
                        .contains(file_path.as_str())
                        .then_some(id)
                })
                .collect()
        } else {
            HashSet::new()
        };

        // Delete files that are no longer in the file list (deleted from disk)
        {
            let mut del_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for path in &deleted_cached_files {
                del_files.execute(params![path])?;
            }
        }

        // Insert new mtimes for stale files
        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for file in &source_stale_files {
                let full = root.join(file);
                if let Some((secs, nanos, content_hash)) = shared_cache::file_fingerprint(&full) {
                    ins.execute(params![file, secs, nanos, content_hash])?;
                }
            }
        }

        shared_cache::refresh_manifest_entries(&tx, root)?;
        let mut import_files_to_refresh: Vec<String> = source_stale_files
            .iter()
            .map(|file| (*file).clone())
            .collect();
        import_files_to_refresh.extend(deleted_cached_files.iter().cloned());
        shared_cache::refresh_file_import_entries(&tx, root, &import_files_to_refresh, all_files)?;

        if repair_changed_clean_entity_ids {
            tx.execute("DELETE FROM entities", [])?;
            tx.execute("DELETE FROM file_contents", [])?;
        } else {
            let mut del = tx.prepare("DELETE FROM entities WHERE file_path = ?1")?;
            let mut del_fc = tx.prepare("DELETE FROM file_contents WHERE path = ?1")?;
            for f in &source_stale_files {
                del.execute(params![f])?;
                del_fc.execute(params![f])?;
            }
            for f in &deleted_cached_files {
                del.execute(params![f])?;
                del_fc.execute(params![f])?;
            }
        }

        {
            let to_insert: Vec<&SemanticEntity> = entities
                .iter()
                .filter(|e| {
                    repair_changed_clean_entity_ids
                        || source_stale_set.contains(e.file_path.as_str())
                })
                .collect();
            shared_cache::insert_entities_with_content_store(&tx, root, &to_insert, true)?;
        }

        if repair_changed_clean_entity_ids {
            tx.execute("DELETE FROM edges", [])?;
            let mut ins = tx.prepare(
                "INSERT INTO edges (from_entity, to_entity, ref_type) VALUES (?1, ?2, ?3)",
            )?;
            for edge in &graph.edges {
                let rt = match edge.ref_type {
                    RefType::Calls => "calls",
                    RefType::TypeRef => "typeref",
                    RefType::Imports => "imports",
                };
                ins.execute(params![edge.from_entity, edge.to_entity, rt])?;
            }
        } else {
            let mut affected_sources: HashSet<String> =
                recomputed_edge_source_ids.iter().cloned().collect();
            let mut deleted_ids: HashSet<String> = deleted_entity_ids.iter().cloned().collect();
            if use_legacy_edge_fallback {
                let current_rewritten_entity_ids: HashSet<&str> = entities
                    .iter()
                    .filter(|entity| source_stale_set.contains(entity.file_path.as_str()))
                    .map(|entity| entity.id.as_str())
                    .collect();
                affected_sources.extend(cached_rewritten_entity_ids.iter().cloned());
                affected_sources.extend(
                    current_rewritten_entity_ids
                        .iter()
                        .map(|entity_id| (*entity_id).to_string()),
                );
                deleted_ids.extend(
                    cached_rewritten_entity_ids
                        .iter()
                        .filter(|entity_id| {
                            !current_rewritten_entity_ids.contains(entity_id.as_str())
                        })
                        .cloned(),
                );
            }
            affected_sources.extend(deleted_ids.iter().cloned());

            {
                let mut del_from = tx.prepare("DELETE FROM edges WHERE from_entity = ?1")?;
                for entity_id in &affected_sources {
                    del_from.execute(params![entity_id])?;
                }
            }
            {
                let mut del_to = tx.prepare("DELETE FROM edges WHERE to_entity = ?1")?;
                for entity_id in &deleted_ids {
                    del_to.execute(params![entity_id])?;
                }
            }

            let mut ins = tx.prepare(
                "INSERT INTO edges (from_entity, to_entity, ref_type) VALUES (?1, ?2, ?3)",
            )?;
            for edge in &graph.edges {
                if !affected_sources.contains(&edge.from_entity)
                    || deleted_ids.contains(&edge.from_entity)
                    || deleted_ids.contains(&edge.to_entity)
                {
                    continue;
                }
                let rt = match edge.ref_type {
                    RefType::Calls => "calls",
                    RefType::TypeRef => "typeref",
                    RefType::Imports => "imports",
                };
                ins.execute(params![edge.from_entity, edge.to_entity, rt])?;
            }
        }

        shared_cache::set_cache_kind(&tx, shared_cache::CACHE_KIND_FULL)?;
        shared_cache::set_cache_source_scope(&tx, source_scope)?;
        // An incremental write leaves the cache reflecting the current working
        // tree, which may differ from HEAD (uncommitted edits). Refresh the
        // epoch so `built_clean` reflects reality now — otherwise a later
        // revert-to-HEAD would let the oracle serve this incremental content as
        // if it were the committed state. Reflecting the current state keeps
        // the oracle correct (it disables itself when built dirty).
        store_repo_origin(&tx, root);
        tx.commit()?;
        // No classification available here: the incremental path never
        // recomputes `entity_flags` either, so handing `None` keeps the two
        // stores honest about the same gap rather than stamping a stale
        // classification into the image (semx-zvq).
        write_query_index(root, all_files, graph, None, None, None);
        Ok(())
    }
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
            .conn
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
            .conn
            .query_row(
                "SELECT rowid FROM edges WHERE from_entity = ?1 AND to_entity = ?2",
                rusqlite::params![from_entity, to_entity],
                |row| row.get(0),
            )
            .ok()
    }

    fn edge_count(cache: &DiskCache, from_entity: &str, to_entity: &str) -> i64 {
        cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE from_entity = ?1 AND to_entity = ?2",
                rusqlite::params![from_entity, to_entity],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn file_import_count(cache: &DiskCache, importing_file: &str, imported_file: &str) -> i64 {
        cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE importing_file = ?1 AND imported_file = ?2",
                rusqlite::params![importing_file, imported_file],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn cached_file_mtime(cache: &DiskCache, file: &str) -> (i64, i64) {
        cache
            .conn
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
            .conn
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
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn assert_lookup_indexes(cache: &DiskCache) {
        let mut stmt = cache
            .conn
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
        let count: i64 = cache.conn.query_row(&sql, [], |row| row.get(0)).unwrap();
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
