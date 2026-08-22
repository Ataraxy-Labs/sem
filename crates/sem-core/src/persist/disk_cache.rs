//! `cache.db`: the on-disk SQLite cache of a resolved `EntityGraph` plus
//! entity bodies, and the single `DiskCache` constructor family over it.
//!
//! # Provenance
//!
//! Until this module existed, `sem-cli` (`build_cache.rs`) and `sem-mcp`
//! (`cache.rs`) each defined their own `struct DiskCache` over the same
//! `cache.db` schema — "a second, independently-written constructor family
//! for the identical semantic object" (disclosed
//! finding). Perf fixes landed on whichever copy its author was looking at:
//! `sem-mcp`'s own `refresh_file_import_entries` picked up the O(1)-candidate-
//! set fix and the single-corpus-read fusion's *shared* half
//! (`insert_entities_with_content_store`'s parallel read) because
//! those functions already lived in `sem-mcp/src/cache.rs` — the "shared
//! substrate" both crates compiled against — but each crate's *own*
//! `DiskCache::save`/`save_with_test_dirs` reimplemented the SQL orchestration
//! around that substrate separately, and the fusion `sem-cli` applied to its
//! copy ("one corpus read for the whole save path") never reached
//! `sem-mcp`'s copy, which still paid 2-3 separate reads per save. Worse:
//! `sem-mcp`'s own `save` never called the test-classification write at all
//! (`entity_flags` stayed empty on every MCP-originated cache), so a
//! `cache.db` last written by `sem mcp` silently lost `sem impact
//! --tests`/`--all`'s "covered by N tests" answer until a CLI save
//! rewrote it — a correctness gap at the exact boundary the two copies
//! shared a database file across (proven by this crate's own
//! `cli_and_mcp_caches_share_manifest_entries`/cross-load tests).
//!
//! This module is the single owner. `sem-cli`'s `build_cache.rs` and
//! `sem-mcp`'s `cache.rs` are now thin consumers: each re-exports this
//! module's public surface and keeps only what is genuinely its own —
//! `sem-cli` keeps `write_query_index`/`index.sem` orchestration (`sem-mcp`
//! writes no index at all, per the superseding note), and
//! neither crate keeps a second copy of the SQL.
//!
//! ## Why `sem-core`, not `sem-mcp` (where the substrate happened to live)
//!
//! `sem-cli` depends on both `sem-core` and `sem-mcp`; `sem-mcp` depends on
//! `sem-core` only (a binary crate, `sem-cli`, must never be a dependency of
//! a library/server crate, `sem-mcp`). `sem-core` is the only crate both
//! already depend on, so it is the only ownership point where a generic
//! disk-cache module needs no new fan-out edge in either direction. It is
//! also where this data belongs structurally: `index_commits`/
//! `entity_changes_for` (the semantic commit index) and `save`/`load`'s SQL
//! bodies touch nothing but `sem_core` types already
//! (`EntityGraph`/`SemanticEntity`/`GitBridge`/`compute_semantic_diff`/
//! `ParserRegistry`) — `sem-mcp` housing them was an accident of which crate
//! got to `cache.rs` first, not a real dependency.
//!
//! Gated behind the `disk-cache` feature (see `Cargo.toml`): `rusqlite`'s
//! `bundled` feature compiles libsqlite3 from C source, which the crate
//! otherwise keeps optional (`git`, `mmap`) so the `wasm` target keeps
//! building without it.
//!
//! ## The one real design decision: decoupling `index.sem` from the SQL save
//!
//! `sem-cli`'s old `save_with_test_dirs`/`save_topology` did their SQL work
//! *and* wrote `index.sem` in the same method, sharing one corpus read
//! between both. `index.sem` is `sem-cli`-only — `sem-mcp` never
//! writes one — so a verbatim move would have forced this module to carry
//! `sem-cli`'s index-writing side effect into a struct `sem-mcp` also
//! constructs: `Needs(DiskCache) ⊋ declared(DiskCache)`. The fix:
//! [`DiskCache::save_with_test_dirs_precomputed`]/
//! [`DiskCache::save_topology_precomputed`] do *only* the SQL write and
//! return the test-entity-id classification the caller may still want (what
//! `write_test_flags` always computed, now handed back instead of consumed
//! internally). They accept an optional [`FileCacheColumns`] slice so a
//! caller that already read the corpus once for its own purposes (`sem-cli`'s
//! `CorpusColumns`, which also derives `index.sem`'s trigrams) can hand those
//! columns down instead of paying a second read — the exact pattern
//! `refresh_file_import_entries_precomputed` already established one level
//! down. A caller with nothing precomputed (`sem-mcp`, and the plain
//! `save`/`save_topology` convenience wrappers every existing test still
//! calls) gets a single internal fused read via [`read_file_cache_columns`]
//! — strictly better than the 2-3 reads `sem-mcp`'s old `save` used to pay,
//! and the fix for the "perf landed on one side" problem this change was
//! opened to close.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::git::bridge::GitBridge;
use crate::git::types::{CommitInfo, DiffScope};
use crate::model::entity::SemanticEntity;
use crate::parser::differ::compute_semantic_diff;
use crate::parser::graph::{EntityGraph, EntityInfo, EntityInfoMap, EntityRef, RefType};
use crate::parser::hotspot::{aggregate_history_analytics, CommitEntityChanges, HistoryAnalytics};
use crate::parser::registry::ParserRegistry;
use crate::parser::{js_ts_import_source_files_from_set, ImportCandidates};
use crate::utils::hash::content_hash_bytes;

/// Bumping this drops and recreates every on-disk cache on next open
/// (`initialize_schema`'s `user_version` check -> `CACHE_RESET_SQL`) --
/// the migration story for this cache is "stale schema -> full rebuild",
/// not in-place `ALTER TABLE`. v10 added the `entities.kappa` column (see
/// `crates/sem-core/ v1.1 -- entities round-tripped through this
/// cache previously always came back with `kappa: None`, silently losing
/// the field, even though it was computed correctly on first extraction).
/// v11 reset eight secondary indexes and the `entity_changes`
/// table's shape, none of the removed indexes having had a production reader
/// left after rerouted the SQL query fast paths onto
/// `index.sem` -- the bump is what makes an existing cache drop its stale
/// indexes instead of keeping them. `entity_changes` the *table* survives
/// (the semantic commit index below still owns it); only its two indexes
/// were part of that census.
pub const CACHE_SCHEMA_VERSION: i32 = 11;
pub const CACHE_KIND_FULL: &str = "full";
pub const CACHE_KIND_TOPOLOGY: &str = "topology";
/// Every index this cache maintains, and the one production statement that
/// needs each. censused these against every `SELECT`/`DELETE`
/// in both crates -- the way censused the module's
/// symbols -- because each index is a second B-tree written per row on the
/// save plane, and `insert_entities_with_content` was the single largest
/// cold-build cost on every giant. Eight of the fourteen had no production
/// consumer at all: five on `entities` (`name`, `name,file_path`,
/// `entity_type,name,file_path`, `parent_id`, `parent_id,name` -- nothing
/// queries `entities` by anything but `id` or a full scan any more, since
/// moved name/type/parent lookups to the index's `NAMES`/`ENTITIES`
/// sections), two composites on `edges` (`from,to,ref` and `to,from,ref` --
/// read only by `#[cfg(test)]` helpers, which the surviving single-column
/// indexes serve anyway), and `idx_file_imports_importing_file`, whose column
/// is already the leading column of that table's `PRIMARY KEY`.
pub const CACHE_INDEXES: &[(&str, &str, &str)] = &[
    // `DELETE FROM entities WHERE file_path = ?1` (incremental save).
    ("idx_entities_file_path", "entities", "file_path"),
    // `DELETE FROM edges WHERE from_entity = ?1` (incremental save).
    ("idx_edges_from_entity", "edges", "from_entity"),
    // `DELETE FROM edges WHERE to_entity = ?1` (incremental save).
    ("idx_edges_to_entity", "edges", "to_entity"),
    // `SELECT DISTINCT importing_file FROM file_imports WHERE imported_file = ?1`
    // (`importing_files_of`). The reverse direction is the table's own PK.
    (
        "idx_file_imports_imported_file",
        "file_imports",
        "imported_file",
    ),
    // `SELECT ... FROM entity_changes WHERE commit_sha = ?1` (`index_commits`'s
    // history analytics). Kept as found: that table is written only by the
    // history path, never by the save plane, so its indexes cost a cold build
    // nothing and this census had no measurement to justify touching them.
    (
        "idx_entity_changes_commit_sha",
        "entity_changes",
        "commit_sha",
    ),
    (
        "idx_entity_changes_entity_name",
        "entity_changes",
        "entity_name",
    ),
];

// Cache-only keys use a NUL prefix so they cannot collide with git paths.
pub const CACHE_MANIFEST_FILES: &[(&str, &str)] = &[
    (".semrc", "\0sem-manifest:.semrc"),
    (".gitattributes", "\0sem-manifest:.gitattributes"),
    (".semignore", "\0sem-manifest:.semignore"),
];
pub const CACHE_SOURCE_SCOPE_KEY: &str = "source_scope";
pub const CACHE_SOURCE_SCOPE_DEFAULT: &str = "default";

/// Set once a cache has computed test flags, so All/Tests-mode impact can
/// trust the cached `entity_flags` (an empty set then means "no tests," not
/// "unknown").
const META_TEST_FLAGS: &str = "test_flags_computed";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheSourceScope {
    Default,
    Custom,
}

const CACHE_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    mtime_secs INTEGER NOT NULL,
    mtime_nanos INTEGER NOT NULL,
    content_hash TEXT
);
CREATE TABLE IF NOT EXISTS entities (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    file_path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_byte INTEGER,
    end_byte INTEGER,
    content TEXT,
    content_hash TEXT NOT NULL,
    structural_hash TEXT,
    kappa TEXT,
    parent_id TEXT,
    metadata_json TEXT
);
CREATE TABLE IF NOT EXISTS edges (
    from_entity TEXT NOT NULL,
    to_entity TEXT NOT NULL,
    ref_type TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS file_imports (
    importing_file TEXT NOT NULL,
    imported_file TEXT NOT NULL,
    PRIMARY KEY (importing_file, imported_file)
);
CREATE TABLE IF NOT EXISTS file_contents (
    path TEXT PRIMARY KEY,
    ztext BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS cache_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS entity_flags (
    entity_id TEXT PRIMARY KEY,
    is_test INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS commits (
    sha TEXT PRIMARY KEY,
    short_sha TEXT NOT NULL,
    author TEXT NOT NULL,
    committed_at INTEGER NOT NULL,
    message TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS entity_changes (
    commit_sha TEXT NOT NULL,
    entity_name TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    file_path TEXT NOT NULL,
    change_type TEXT NOT NULL,
    old_entity_name TEXT,
    old_file_path TEXT,
    structural INTEGER
);
";

const CACHE_RESET_SQL: &str = "
DROP TABLE IF EXISTS files;
DROP TABLE IF EXISTS entities;
DROP TABLE IF EXISTS edges;
DROP TABLE IF EXISTS file_imports;
DROP TABLE IF EXISTS file_contents;
DROP TABLE IF EXISTS cache_metadata;
DROP TABLE IF EXISTS entity_flags;
DROP TABLE IF EXISTS commits;
DROP TABLE IF EXISTS entity_changes;
";

/// Per-connection performance pragmas. These are not persisted in the database
/// file, so they must be re-applied on every connection open (including
/// read-only opens). `mmap_size` serves reads via memory-mapped I/O, a larger
/// `cache_size` keeps more pages hot, and `temp_store=MEMORY` keeps temporary
/// b-trees off disk. This speeds the topology load on large repos.
pub fn apply_performance_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "PRAGMA mmap_size=268435456;
         PRAGMA cache_size=-65536;
         PRAGMA temp_store=MEMORY;",
    )
}

/// Entity bodies are not duplicated into the cache when they can be proven
/// recoverable: the file's text is stored once (zstd) in `file_contents`, and
/// any entity whose `content` equals `file_text[start_byte..end_byte]`
/// byte-for-byte stores NULL content and is re-sliced on load. Entities that
/// fail the proof (no spans, normalized line endings, disk changed since
/// extraction) keep their content inline — identity by construction, never by
/// assumption. On a 139K-entity corpus this removes the ~2x source duplication
/// nested entities cause (#322).
/// Sub-phase timing for the content store, gated by `SEM_PROFILE_CACHE=1` —
/// the same `OnceLock<bool>` contract `sem-cli`'s `cache_profile_mark` and
/// `sem-core`'s `resolve_profile::enabled()` use: zero cost when unset, never
/// changes a written byte. Added because profiling found
/// `insert_entities_with_content` to be the single largest save-plane cost on
/// every giant (1.9-15.5 s) with no attribution inside it.
fn store_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| env::var("SEM_PROFILE_CACHE").as_deref() == Ok("1"))
}

fn store_mark(phase: &str, t0: std::time::Instant) {
    if store_profile_enabled() {
        eprintln!(
            "CONTENT_STORE_PHASE phase={phase} ms={:.2}",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
}

pub fn compress_file_text(text: &str) -> Option<Vec<u8>> {
    zstd::encode_all(text.as_bytes(), 3).ok()
}

pub fn decompress_file_text(blob: &[u8]) -> Option<String> {
    zstd::decode_all(blob)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Insert entities using the content store. `replace` selects INSERT OR
/// REPLACE (incremental saves) vs plain INSERT (full saves). Reads each
/// referenced file from disk at most once, verifies every span slice against
/// the entity's content, and stores the compressed file text only when at
/// least one entity actually needs it reconstructed from it.
pub fn insert_entities_with_content_store(
    tx: &Transaction<'_>,
    root: &Path,
    entities: &[&SemanticEntity],
    replace: bool,
) -> Result<(), rusqlite::Error> {
    let verb = if replace {
        "INSERT OR REPLACE INTO"
    } else {
        "INSERT INTO"
    };
    let sql = format!(
        "{verb} entities (id, name, entity_type, file_path, start_line, end_line, start_byte, end_byte, content, content_hash, structural_hash, kappa, parent_id, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
    );
    let mut stmt = tx.prepare(&sql)?;

    // the per-file `read_to_string` used to happen lazily
    // inside the serial entity loop below — a third full-corpus read (after
    // pass 1's parse read and the save plane's `CorpusColumns::read`), on one
    // thread, interleaved with SQLite `execute`s. It is a pure function of
    // the path, so it hoists out of the loop and parallelizes exactly the way
    // `CorpusColumns::read` and fingerprint read already do:
    // read in parallel, then do the (necessarily serial, `Statement` isn't
    // `Send`) inserts against the finished map. Same bytes read, same rows
    // written, same order.
    let __reads_t0 = std::time::Instant::now();
    let mut distinct_paths: Vec<&str> = entities.iter().map(|e| e.file_path.as_str()).collect();
    distinct_paths.sort_unstable();
    distinct_paths.dedup();
    let file_texts: HashMap<&str, Option<String>> = distinct_paths
        .par_iter()
        .map(|path| (*path, std::fs::read_to_string(root.join(path)).ok()))
        .collect();
    store_mark("file_reads_parallel", __reads_t0);

    let __ins_t0 = std::time::Instant::now();
    let mut files_to_store: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for e in entities {
        let metadata_json = e
            .metadata
            .as_ref()
            .and_then(|m| serde_json::to_string(m).ok());
        let text = file_texts
            .get(e.file_path.as_str())
            .expect("every entity's file path was collected above");
        let sliceable = match (text.as_deref(), e.start_byte, e.end_byte) {
            (Some(t), Some(sb), Some(eb)) => t.get(sb..eb) == Some(e.content.as_str()),
            _ => false,
        };
        let stored_content: Option<&str> = if sliceable {
            files_to_store.insert(e.file_path.as_str());
            None
        } else {
            Some(e.content.as_str())
        };
        stmt.execute(params![
            e.id,
            e.name,
            e.entity_type,
            e.file_path,
            e.start_line as i64,
            e.end_line as i64,
            e.start_byte.map(|v| v as i64),
            e.end_byte.map(|v| v as i64),
            stored_content,
            e.content_hash,
            e.structural_hash,
            e.kappa,
            e.parent_id,
            metadata_json,
        ])?;
    }

    store_mark("entity_inserts_serial", __ins_t0);

    // Same hoist for the zstd pass: `compress_file_text` is a pure function of
    // the text, so every blob is computed in parallel and only the `INSERT`
    // stays serial. `files_to_store` is a `HashSet`, whose iteration order was
    // already unspecified, so the write order was never a contract; sorting
    // makes it one, which is strictly better for the byte-identical gate.
    let __zstd_t0 = std::time::Instant::now();
    let mut to_store: Vec<&str> = files_to_store.into_iter().collect();
    to_store.sort_unstable();
    let blobs: Vec<(&str, Vec<u8>)> = to_store
        .par_iter()
        .filter_map(|path| {
            let Some(Some(text)) = file_texts.get(*path) else {
                return None;
            };
            compress_file_text(text).map(|blob| (*path, blob))
        })
        .collect();
    store_mark("compress_parallel", __zstd_t0);

    let __fc_t0 = std::time::Instant::now();
    let mut fc =
        tx.prepare("INSERT OR REPLACE INTO file_contents (path, ztext) VALUES (?1, ?2)")?;
    for (path, blob) in &blobs {
        fc.execute(params![path, blob])?;
    }
    store_mark("file_contents_insert_serial", __fc_t0);
    Ok(())
}

/// Load-side counterpart: resolves an entity's content from the inline column
/// or by slicing the stored file text. Decompresses each file at most once.
pub struct ContentReconstructor<'c> {
    conn: &'c Connection,
    cache: HashMap<String, Option<String>>,
}

impl<'c> ContentReconstructor<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self {
            conn,
            cache: HashMap::new(),
        }
    }

    pub fn content(
        &mut self,
        file_path: &str,
        inline: Option<String>,
        start_byte: Option<usize>,
        end_byte: Option<usize>,
    ) -> String {
        if let Some(c) = inline {
            return c;
        }
        let conn = self.conn;
        let text = self.cache.entry(file_path.to_string()).or_insert_with(|| {
            conn.query_row(
                "SELECT ztext FROM file_contents WHERE path = ?1",
                params![file_path],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .ok()
            .and_then(|blob| decompress_file_text(&blob))
        });
        match (text.as_deref(), start_byte, end_byte) {
            (Some(t), Some(sb), Some(eb)) => t.get(sb..eb).unwrap_or("").to_string(),
            _ => String::new(),
        }
    }
}

// ── Semantic commit index (storage engine layer 2) ──
//
// History stored as entity deltas: each commit is semantic-diffed against its
// FIRST PARENT exactly once and persisted as entity-change rows keyed by sha.
// Every later history query is a lookup plus a diff of only the commits git
// gained since. Rows are branch-agnostic (sha-keyed), so switching branches
// never invalidates the index. Merge commits are recorded with no changes:
// their first-parent diff restates the merged-in commits, which are indexed
// individually on their own shas.

/// Answer repo history analytics from the semantic commit index, indexing any
/// commits it has not seen. Returns None when the cache is unusable so callers
/// can fall back to the live git walk.
pub fn history_analytics_from_store(
    repo_root: &Path,
    git: &GitBridge,
    registry: &ParserRegistry,
    file_path: Option<&str>,
    max_commits: usize,
) -> Option<HistoryAnalytics> {
    let commits = git.get_log(max_commits.saturating_add(1)).ok()?;
    if commits.len() < 2 {
        return Some(aggregate_history_analytics(&[], file_path));
    }
    let mut cache = DiskCache::open(repo_root).ok()?;
    // The oldest walked commit is the diff baseline only, mirroring the
    // windows(2) accounting of the live walk.
    let scan = &commits[..commits.len() - 1];
    cache.index_commits(git, registry, scan).ok()?;
    let mut scanned = Vec::with_capacity(scan.len());
    for info in scan {
        scanned.push(CommitEntityChanges {
            short_sha: info.short_sha.clone(),
            author: info.author.clone(),
            changed: cache.entity_changes_for(&info.sha).ok()?,
        });
    }
    Some(aggregate_history_analytics(&scanned, file_path))
}

pub fn initialize_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;",
    )?;
    apply_performance_pragmas(conn)?;

    let user_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != CACHE_SCHEMA_VERSION {
        conn.execute_batch(CACHE_RESET_SQL)?;
    }

    let index_sql = CACHE_INDEXES
        .iter()
        .map(|(name, table, column)| {
            format!("CREATE INDEX IF NOT EXISTS {name} ON {table}({column});")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let schema_sql = format!(
        "{} {} PRAGMA user_version = {};",
        CACHE_SCHEMA_SQL, index_sql, CACHE_SCHEMA_VERSION
    );
    conn.execute_batch(&schema_sql)
}

/// Stamp the cache with the repo root it was built from. The cache directory
/// name is a hash, so without this a cache on disk can't be mapped back to
/// its repo (used by `sem repos` to label local storage).
pub fn set_cache_repo_root(tx: &Transaction<'_>, root: &Path) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT OR REPLACE INTO cache_metadata (key, value) VALUES ('repo_root', ?1)",
        params![root.to_string_lossy()],
    )?;
    Ok(())
}

pub fn set_cache_kind(tx: &Transaction<'_>, kind: &str) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT OR REPLACE INTO cache_metadata (key, value) VALUES ('cache_kind', ?1)",
        params![kind],
    )?;
    Ok(())
}

pub fn set_cache_source_scope(
    tx: &Transaction<'_>,
    source_scope: CacheSourceScope,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "DELETE FROM cache_metadata WHERE key = ?1",
        params![CACHE_SOURCE_SCOPE_KEY],
    )?;
    if matches!(source_scope, CacheSourceScope::Default) {
        tx.execute(
            "INSERT INTO cache_metadata (key, value) VALUES (?1, ?2)",
            params![CACHE_SOURCE_SCOPE_KEY, CACHE_SOURCE_SCOPE_DEFAULT],
        )?;
    }
    Ok(())
}

pub fn cache_has_default_source_scope(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT value FROM cache_metadata WHERE key = ?1",
        params![CACHE_SOURCE_SCOPE_KEY],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .as_deref()
        == Some(CACHE_SOURCE_SCOPE_DEFAULT)
}

pub fn cache_has_source_scope(conn: &Connection, source_scope: CacheSourceScope) -> bool {
    let is_default = cache_has_default_source_scope(conn);
    match source_scope {
        CacheSourceScope::Default => is_default,
        CacheSourceScope::Custom => !is_default,
    }
}

pub fn cache_has_kind(conn: &Connection, accepted: &[&str]) -> bool {
    conn.query_row(
        "SELECT value FROM cache_metadata WHERE key = 'cache_kind'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .is_some_and(|kind| accepted.contains(&kind.as_str()))
}

pub fn cache_db_path(repo_root: &Path) -> Option<PathBuf> {
    Some(cache_dir_for_repo(repo_root)?.join("cache.db"))
}

pub fn cache_dir_for_repo(repo_root: &Path) -> Option<PathBuf> {
    Some(cache_root(repo_root)?.join(repo_cache_key(repo_root)))
}

pub fn create_cache_dir(cache_dir: &Path) -> Result<(), rusqlite::Error> {
    std::fs::create_dir_all(cache_dir).map_err(|err| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::CannotOpen,
                extended_code: rusqlite::ffi::SQLITE_CANTOPEN,
            },
            Some(format!(
                "failed to create cache directory {}: {}",
                cache_dir.display(),
                err
            )),
        )
    })
}

fn cache_root(repo_root: &Path) -> Option<PathBuf> {
    let repo_lexical = normalize_lexical(&absolute_path(repo_root));
    let repo_resolved = canonicalize_existing_prefix(&repo_lexical);

    for candidate in cache_root_candidates() {
        let lexical = normalize_lexical(&absolute_path(&candidate));
        let resolved = canonicalize_existing_prefix(&lexical);
        if path_is_external_to_repo(&lexical, &resolved, &repo_lexical, &repo_resolved) {
            return Some(resolved);
        }
    }

    fallback_external_cache_root(&repo_lexical, &repo_resolved)
}

fn cache_root_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = non_empty_env("SEM_CACHE_DIR") {
        candidates.push(path);
    }
    if cfg!(target_os = "windows") {
        if let Some(path) = non_empty_env("LOCALAPPDATA").or_else(|| non_empty_env("APPDATA")) {
            candidates.push(path.join("sem").join("repos"));
        }
    } else {
        if let Some(path) = non_empty_env("XDG_CACHE_HOME") {
            candidates.push(path.join("sem").join("repos"));
        }

        if cfg!(target_os = "macos") {
            if let Some(home) = non_empty_env("HOME") {
                candidates.push(
                    home.join("Library")
                        .join("Caches")
                        .join("sem")
                        .join("repos"),
                );
            }
        }
    }

    if let Some(home) = non_empty_env("HOME").or_else(|| non_empty_env("USERPROFILE")) {
        candidates.push(home.join(".cache").join("sem").join("repos"));
    }

    candidates.push(env::temp_dir().join("sem").join("repos"));
    candidates
}

fn fallback_external_cache_root(repo_lexical: &Path, repo_resolved: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(parent) = repo_resolved.parent() {
        candidates.push(parent.join(".sem-cache").join("repos"));
    }
    candidates.push(env::temp_dir().join("sem").join("repos"));

    for candidate in candidates {
        let lexical = normalize_lexical(&absolute_path(&candidate));
        let resolved = canonicalize_existing_prefix(&lexical);
        if path_is_external_to_repo(&lexical, &resolved, repo_lexical, repo_resolved) {
            return Some(resolved);
        }
    }

    None
}

fn path_is_external_to_repo(
    candidate_lexical: &Path,
    candidate_resolved: &Path,
    repo_lexical: &Path,
    repo_resolved: &Path,
) -> bool {
    let lexical_is_inside =
        candidate_lexical.starts_with(repo_lexical) || candidate_lexical.starts_with(repo_resolved);
    let resolved_is_inside = candidate_resolved.starts_with(repo_lexical)
        || candidate_resolved.starts_with(repo_resolved);

    !lexical_is_inside && !resolved_is_inside
}

fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut missing = Vec::<OsString>::new();

    for ancestor in path.ancestors() {
        if let Ok(existing) = ancestor.canonicalize() {
            let mut resolved = normalize_lexical(&existing);
            for part in missing.iter().rev() {
                resolved.push(part);
            }
            return normalize_lexical(&resolved);
        }

        if let Some(part) = ancestor.file_name() {
            missing.push(part.to_os_string());
        }
    }

    normalize_lexical(path)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    let mut has_prefix = false;
    let mut has_root = false;

    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                has_prefix = true;
                normalized.push(component.as_os_str());
            }
            Component::RootDir => {
                has_root = true;
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.as_os_str().is_empty() {
                    if !has_prefix && !has_root {
                        normalized.push("..");
                    }
                } else if normalized.ends_with("..") {
                    normalized.push("..");
                } else if !normalized.pop() && !has_prefix && !has_root {
                    normalized.push("..");
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn non_empty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn repo_cache_key(repo_root: &Path) -> String {
    let canonical = repo_root
        .canonicalize()
        .unwrap_or_else(|_| absolute_path(repo_root));
    let mut hash = 0xcbf29ce484222325u64;

    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }

    format!("{hash:016x}")
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
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

/// Compute a manifest hash from file paths + mtimes.
/// If any source file can't be stat'd, returns None.
pub fn compute_manifest_hash(root: &Path, files: &[String]) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in files {
        let full = root.join(file);
        let (secs, nanos) = file_mtime_parts(&full)?;
        file.hash(&mut hasher);
        secs.hash(&mut hasher);
        nanos.hash(&mut hasher);
    }
    files.len().hash(&mut hasher);

    for (file_name, _) in CACHE_MANIFEST_FILES {
        let full = root.join(file_name);
        if !full.exists() {
            continue;
        }

        file_name.hash(&mut hasher);
        match file_mtime_parts(&full) {
            Some((secs, nanos)) => {
                true.hash(&mut hasher);
                secs.hash(&mut hasher);
                nanos.hash(&mut hasher);
            }
            None => {
                false.hash(&mut hasher);
            }
        }
    }

    Some(hasher.finish())
}

pub fn file_mtime_parts(path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dur = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Some((dur.as_secs() as i64, dur.subsec_nanos() as i64))
}

pub fn file_content_hash(path: &Path) -> Option<String> {
    let content = std::fs::read(path).ok()?;
    Some(content_hash_bytes(&content))
}

pub fn file_fingerprint(path: &Path) -> Option<(i64, i64, String)> {
    let (secs, nanos) = file_mtime_parts(path)?;
    let hash = file_content_hash(path)?;
    Some((secs, nanos, hash))
}

pub enum FileFreshness {
    Fresh,
    FreshWithUpdatedFingerprint {
        secs: i64,
        nanos: i64,
        content_hash: String,
    },
    Stale,
}

pub struct FileFingerprintRefresh {
    pub path: String,
    pub mtime_secs: i64,
    pub mtime_nanos: i64,
    pub content_hash: String,
}

pub fn refresh_file_fingerprints(
    conn: &Connection,
    refreshes: &[FileFingerprintRefresh],
) -> Result<(), rusqlite::Error> {
    if refreshes.is_empty() {
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE files SET mtime_secs = ?2, mtime_nanos = ?3, content_hash = ?4 WHERE path = ?1",
        )?;
        for refresh in refreshes {
            stmt.execute(params![
                &refresh.path,
                refresh.mtime_secs,
                refresh.mtime_nanos,
                &refresh.content_hash
            ])?;
        }
    }
    tx.commit()
}

/// Attempts to persist refreshed file fingerprints without changing cache-hit validity.
pub fn refresh_file_fingerprints_best_effort(conn: &Connection, refreshes: &[FileFingerprintRefresh]) {
    let _ = refresh_file_fingerprints(conn, refreshes);
}

pub fn file_freshness(
    path: &Path,
    cached_secs: i64,
    cached_nanos: i64,
    cached_content_hash: Option<&str>,
) -> Option<FileFreshness> {
    let (current_secs, current_nanos) = file_mtime_parts(path)?;
    if cached_secs == current_secs && cached_nanos == current_nanos {
        return Some(FileFreshness::Fresh);
    }

    let cached_content_hash = cached_content_hash?;
    let current_content_hash = file_content_hash(path)?;
    if current_content_hash == cached_content_hash {
        return Some(FileFreshness::FreshWithUpdatedFingerprint {
            secs: current_secs,
            nanos: current_nanos,
            content_hash: current_content_hash,
        });
    }

    Some(FileFreshness::Stale)
}

pub fn is_cache_manifest_key(path: &str) -> bool {
    CACHE_MANIFEST_FILES
        .iter()
        .any(|(_, cache_key)| *cache_key == path)
}

pub fn is_manifest_file_name(path: &str) -> bool {
    CACHE_MANIFEST_FILES
        .iter()
        .any(|(file_name, _)| *file_name == path)
}

pub fn source_file_count(files: &[String]) -> usize {
    files
        .iter()
        .filter(|file| !is_manifest_file_name(file))
        .count()
}

fn cached_file_fingerprint(conn: &Connection, cache_key: &str) -> Option<(i64, i64, Option<String>)> {
    conn.query_row(
        "SELECT mtime_secs, mtime_nanos, content_hash FROM files WHERE path = ?1",
        params![cache_key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .ok()
}

pub fn is_manifest_stale(conn: &Connection, root: &Path) -> bool {
    let mut fingerprint_refreshes = Vec::new();
    for (file_name, cache_key) in CACHE_MANIFEST_FILES {
        let full = root.join(file_name);
        let cached = cached_file_fingerprint(conn, cache_key);

        match (full.exists(), cached) {
            (true, None) | (false, Some(_)) => return true,
            (false, None) => {}
            (true, Some((secs, nanos, content_hash))) => {
                match file_freshness(&full, secs, nanos, content_hash.as_deref()) {
                    Some(FileFreshness::Fresh) => {}
                    Some(FileFreshness::FreshWithUpdatedFingerprint {
                        secs,
                        nanos,
                        content_hash,
                    }) => {
                        fingerprint_refreshes.push(FileFingerprintRefresh {
                            path: (*cache_key).to_string(),
                            mtime_secs: secs,
                            mtime_nanos: nanos,
                            content_hash,
                        });
                    }
                    Some(FileFreshness::Stale) | None => return true,
                }
            }
        }
    }

    refresh_file_fingerprints_best_effort(conn, &fingerprint_refreshes);
    false
}

pub fn manifest_entry_count(conn: &Connection) -> i64 {
    CACHE_MANIFEST_FILES
        .iter()
        .map(|(_, cache_key)| {
            conn.query_row(
                "SELECT COUNT(*) FROM files WHERE path = ?1",
                params![cache_key],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
        })
        .sum()
}

pub fn refresh_manifest_entries(tx: &Transaction<'_>, root: &Path) -> Result<(), rusqlite::Error> {
    {
        let mut delete = tx.prepare("DELETE FROM files WHERE path = ?1")?;
        for (_, cache_key) in CACHE_MANIFEST_FILES {
            delete.execute(params![cache_key])?;
        }
    }

    let mut insert = tx.prepare(
        "INSERT OR REPLACE INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (file_name, cache_key) in CACHE_MANIFEST_FILES {
        let full = root.join(file_name);
        if let Some((secs, nanos, content_hash)) = file_fingerprint(&full) {
            insert.execute(params![cache_key, secs, nanos, content_hash])?;
        }
    }

    Ok(())
}

pub fn refresh_file_import_entries(
    tx: &Transaction<'_>,
    root: &Path,
    files_to_refresh: &[String],
    all_files: &[String],
) -> Result<(), rusqlite::Error> {
    // O(1)-membership candidate set, built once per call (not per file) --
    // `js_ts_import_source_files_from_set` (import_resolution.rs) turns each
    // relative-import candidate check into a HashSet lookup instead of the
    // O(candidate_files) linear scan `js_ts_import_source_files_from_content`
    // did via `find_import_file`'s `candidate_file_paths.iter().find(...)`.
    // item 9.
    let candidate_files = ImportCandidates::new(
        all_files
            .iter()
            .filter(|file| !is_manifest_file_name(file))
            .map(String::as_str),
    );

    let refreshed: Vec<(&String, Option<Vec<String>>)> = files_to_refresh
        .par_iter()
        .map(|file| {
            if is_manifest_file_name(file) {
                return (file, None);
            }
            let imports = std::fs::read_to_string(root.join(file))
                .ok()
                .map(|content| js_ts_import_source_files_from_set(file, &content, &candidate_files))
                .unwrap_or_default();
            (file, Some(imports))
        })
        .collect();

    let mut delete = tx.prepare("DELETE FROM file_imports WHERE importing_file = ?1")?;
    let mut insert = tx.prepare(
        "INSERT OR IGNORE INTO file_imports (importing_file, imported_file) VALUES (?1, ?2)",
    )?;

    for (file, imports) in refreshed {
        let Some(imports) = imports else {
            continue;
        };

        delete.execute(params![file])?;
        for imported_file in imports {
            insert.execute(params![file, imported_file])?;
        }
    }

    Ok(())
}

/// [`refresh_file_import_entries`] with the read and the import scan already
/// done by the caller (pass I).
///
/// Semantics are identical to the sibling's, deliberately, file for file: a
/// manifest file is skipped entirely (no `DELETE`), every other file in
/// `files_to_refresh` gets its `DELETE` whether or not it resolved any
/// import — an unreadable file (absent from `imports`) lands on the same
/// `DELETE`-then-insert-nothing path the sibling's `Some(vec![])` produced.
pub fn refresh_file_import_entries_precomputed(
    tx: &Transaction<'_>,
    files_to_refresh: &[String],
    imports: &HashMap<&str, Vec<String>>,
) -> Result<(), rusqlite::Error> {
    let mut delete = tx.prepare("DELETE FROM file_imports WHERE importing_file = ?1")?;
    let mut insert = tx.prepare(
        "INSERT OR IGNORE INTO file_imports (importing_file, imported_file) VALUES (?1, ?2)",
    )?;

    for file in files_to_refresh {
        if is_manifest_file_name(file) {
            continue;
        }
        delete.execute(params![file])?;
        let Some(imported_files) = imports.get(file.as_str()) else {
            continue;
        };
        for imported_file in imported_files {
            insert.execute(params![file, imported_file])?;
        }
    }

    Ok(())
}

pub fn cached_importing_files_for_stale_files(
    conn: &Connection,
    stale_files: &[String],
    current_source_files: &[&String],
) -> Vec<String> {
    let current_set: HashSet<&str> = current_source_files
        .iter()
        .map(|file| file.as_str())
        .collect();
    let stale_set: HashSet<&str> = stale_files.iter().map(String::as_str).collect();
    let mut importing_files = HashSet::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT DISTINCT importing_file FROM file_imports WHERE imported_file = ?1")
    else {
        return Vec::new();
    };

    for stale_file in stale_files {
        let Ok(rows) = stmt.query_map(params![stale_file], |row| row.get::<_, String>(0)) else {
            continue;
        };
        for importing_file in rows.filter_map(|row| row.ok()) {
            if current_set.contains(importing_file.as_str())
                && !stale_set.contains(importing_file.as_str())
            {
                importing_files.insert(importing_file);
            }
        }
    }

    let mut importing_files: Vec<String> = importing_files.into_iter().collect();
    importing_files.sort_unstable();
    importing_files
}

/// One file's cache-relevant columns from a single read of its bytes: the
/// fingerprint `files` wants and the imports `file_imports` wants. A strict
/// subset of what an index-building corpus read (trigrams included) also
/// derives from the same bytes — a caller that already did that richer read
/// for its own purposes (`sem-cli`'s `CorpusColumns`, which also feeds
/// `index.sem`) projects its own columns into this shape and hands them to
/// [`DiskCache::save_with_test_dirs_precomputed`]/
/// [`DiskCache::save_topology_precomputed`] via `precomputed`, so the fused
/// read below isn't a second pass over files that read already covered
/// (fix for the read-amplification half of the duplicate-`DiskCache`
/// finding).
pub struct FileCacheColumns {
    pub path: String,
    pub mtime_secs: i64,
    pub mtime_nanos: i64,
    pub content_hash: String,
    pub imports: Vec<String>,
}

/// The default single-read fusion: mtime, content hash and resolved imports
/// for every non-manifest file in `files`, from one parallel read of each
/// file's bytes — semantics deliberately identical, file for file, to the
/// pre-fusion pair this replaces (`file_fingerprint` + `refresh_file_import_
/// entries`'s own read): a file that cannot be read as raw bytes is entirely
/// absent from the result (no `files` row, matching `file_fingerprint`
/// returning `None`); a file whose bytes are not valid UTF-8 still gets its
/// fingerprint (hash is over raw bytes) but resolves zero imports (matching
/// `read_to_string`'s failure on non-UTF-8 content).
pub fn read_file_cache_columns(root: &Path, files: &[String]) -> Vec<FileCacheColumns> {
    let candidate_files = ImportCandidates::new(
        files
            .iter()
            .filter(|file| !is_manifest_file_name(file))
            .map(String::as_str),
    );

    files
        .par_iter()
        .filter(|file| !is_manifest_file_name(file))
        .filter_map(|file| {
            let full = root.join(file);
            let (mtime_secs, mtime_nanos) = file_mtime_parts(&full)?;
            let bytes = std::fs::read(&full).ok()?;
            let content_hash = content_hash_bytes(&bytes);
            let imports = match std::str::from_utf8(&bytes) {
                Ok(text) => js_ts_import_source_files_from_set(file, text, &candidate_files),
                Err(_) => Vec::new(),
            };
            Some(FileCacheColumns {
                path: file.clone(),
                mtime_secs,
                mtime_nanos,
                content_hash,
                imports,
            })
        })
        .collect()
}

/// Classify test entities and persist the classification, so a later
/// point-query can trust an EMPTY result as "no tests" rather than "not
/// computed." Called from every full/topology save. Returns the
/// classification it persisted so a caller with its own downstream use for
/// it (`sem-cli`'s `index.sem` writer) doesn't need to recompute it.
fn write_test_flags(
    tx: &Transaction<'_>,
    graph: &EntityGraph,
    entities: &[SemanticEntity],
    custom_test_dirs: &[String],
) -> Result<HashSet<String>, rusqlite::Error> {
    let test_entity_ids: HashSet<String> = graph
        .filter_test_entities_with_custom_dirs(entities, custom_test_dirs)
        .into_iter()
        .map(str::to_string)
        .collect();
    {
        let mut stmt = tx.prepare("INSERT INTO entity_flags (entity_id, is_test) VALUES (?1, 1)")?;
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

fn insert_files_table(
    tx: &Transaction<'_>,
    columns: &[FileCacheColumns],
) -> Result<(), rusqlite::Error> {
    let mut stmt = tx.prepare(
        "INSERT INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for c in columns {
        stmt.execute(params![c.path, c.mtime_secs, c.mtime_nanos, c.content_hash])?;
    }
    Ok(())
}

fn insert_edges_table(tx: &Transaction<'_>, graph: &EntityGraph) -> Result<(), rusqlite::Error> {
    let mut stmt =
        tx.prepare("INSERT INTO edges (from_entity, to_entity, ref_type) VALUES (?1, ?2, ?3)")?;
    for edge in &graph.edges {
        let rt = match edge.ref_type {
            RefType::Calls => "calls",
            RefType::TypeRef => "typeref",
            RefType::Imports => "imports",
        };
        stmt.execute(params![edge.from_entity, edge.to_entity, rt])?;
    }
    Ok(())
}

pub struct DiskCache {
    conn: Connection,
}

impl DiskCache {
    /// Raw connection escape hatch. Not part of the save/load contract —
    /// exists so tooling (`sem repos`) and each consumer crate's own cache
    /// tests, which verify low-level schema/row invariants directly, can
    /// reach the connection now that `DiskCache` lives across a crate
    /// boundary from them rather than in the same module its
    /// private `conn` field used to be directly reachable from.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn open(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let cache_dir = cache_dir_for_repo(repo_root)
            .ok_or_else(|| rusqlite::Error::InvalidPath(repo_root.to_path_buf()))?;
        create_cache_dir(&cache_dir)?;
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path)?;

        initialize_schema(&conn)?;

        Ok(Self { conn })
    }

    /// [`Self::open`] for the *read* side: declines instead of creating an
    /// empty `cache.db`.
    ///
    /// `Connection::open` creates the file, so every cache-tier probe used to
    /// leave a schema-only SQLite database behind even on a path that never
    /// wrote a row. Behaviour is unchanged for every caller: a `cache.db`
    /// that does not exist has no rows, so every `load*`/`has_fresh_*` below
    /// would have missed anyway. Callers that intend to *write* still use
    /// `open`.
    pub fn open_existing(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let cache_dir = cache_dir_for_repo(repo_root)
            .ok_or_else(|| rusqlite::Error::InvalidPath(repo_root.to_path_buf()))?;
        let db_path = cache_dir.join("cache.db");
        if !db_path.exists() {
            return Err(rusqlite::Error::InvalidPath(db_path));
        }
        Self::open(repo_root)
    }

    /// Index every commit in `commits` that the store has not seen: semantic
    /// diff against its first parent, persisted as entity-change rows. Merge
    /// commits are stored with zero changes. Returns the number of commits
    /// newly indexed.
    pub fn index_commits(
        &mut self,
        git: &GitBridge,
        registry: &ParserRegistry,
        commits: &[CommitInfo],
    ) -> Result<usize, rusqlite::Error> {
        let mut newly_indexed = 0usize;
        for info in commits {
            let exists = self
                .conn
                .query_row(
                    "SELECT 1 FROM commits WHERE sha = ?1",
                    params![info.sha],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                continue;
            }

            let is_merge = git
                .commit_parent_count(&info.sha)
                .map(|n| n > 1)
                .unwrap_or(false);
            let changes = if is_merge {
                Vec::new()
            } else {
                let scope = DiffScope::Commit {
                    sha: info.sha.clone(),
                };
                match git.get_changed_files(&scope, &[]) {
                    Ok(fc) => {
                        compute_semantic_diff(&fc, registry, Some(&info.sha), Some(&info.author))
                            .changes
                    }
                    Err(_) => Vec::new(),
                }
            };

            let tx = self.conn.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO commits (sha, short_sha, author, committed_at, message)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    info.sha,
                    info.short_sha,
                    info.author,
                    info.date.parse::<i64>().unwrap_or(0),
                    info.message,
                ],
            )?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO entity_changes
                     (commit_sha, entity_name, entity_type, file_path, change_type,
                      old_entity_name, old_file_path, structural)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for c in &changes {
                    stmt.execute(params![
                        info.sha,
                        c.entity_name,
                        c.entity_type,
                        c.file_path,
                        c.change_type.to_string(),
                        c.old_entity_name,
                        c.old_file_path,
                        c.structural_change.map(i64::from),
                    ])?;
                }
            }
            tx.commit()?;
            newly_indexed += 1;
        }
        Ok(newly_indexed)
    }

    /// (entity_name, entity_type, file_path) rows stored for one commit.
    pub fn entity_changes_for(&self, sha: &str) -> Result<Vec<(String, String, String)>, rusqlite::Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT entity_name, entity_type, file_path
             FROM entity_changes WHERE commit_sha = ?1",
        )?;
        let rows = stmt.query_map(params![sha], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect()
    }

    /// Full save with no test-directory classification and no precomputed
    /// columns — the entry point `sem-mcp` calls in production, and every
    /// existing test in both consumer crates that doesn't care about test
    /// flags or a caller-supplied corpus read.
    pub fn save(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        source_scope: CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        self.save_with_test_dirs(root, files, graph, entities, &[], source_scope)
            .map(|_| ())
    }

    /// Full save that also records test flags, so `sem impact` in the default
    /// (All) mode can be answered from the indexed point-query instead of
    /// hydrating the whole graph. Does its own single-read corpus fusion
    /// internally; callers that already read the corpus for another reason
    /// should use [`Self::save_with_test_dirs_precomputed`] instead.
    pub fn save_with_test_dirs(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        custom_test_dirs: &[String],
        source_scope: CacheSourceScope,
    ) -> Result<HashSet<String>, rusqlite::Error> {
        self.save_with_test_dirs_precomputed(
            root,
            files,
            graph,
            entities,
            custom_test_dirs,
            source_scope,
            None,
        )
    }

    /// [`Self::save_with_test_dirs`] with the corpus read optionally supplied
    /// by the caller. `precomputed: None` performs the same fused
    /// read [`read_file_cache_columns`] does internally; `Some(columns)` skips
    /// it and uses the given columns directly — for a caller (`sem-cli`) that
    /// already read the corpus once for `index.sem`'s trigrams and would
    /// otherwise pay a second full read here.
    ///
    /// Returns the entity ids classified as tests, so a caller that also
    /// needs that classification (again, `index.sem`'s writer) doesn't
    /// recompute it.
    #[allow(clippy::too_many_arguments)]
    pub fn save_with_test_dirs_precomputed(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        custom_test_dirs: &[String],
        source_scope: CacheSourceScope,
        precomputed: Option<&[FileCacheColumns]>,
    ) -> Result<HashSet<String>, rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch(
            "DELETE FROM files; DELETE FROM entities; DELETE FROM edges; DELETE FROM file_imports; DELETE FROM entity_flags;",
        )?;

        let owned_columns;
        let columns: &[FileCacheColumns] = match precomputed {
            Some(c) => c,
            None => {
                owned_columns = read_file_cache_columns(root, files);
                &owned_columns
            }
        };

        insert_files_table(&tx, columns)?;
        refresh_manifest_entries(&tx, root)?;
        let imports_by_file: HashMap<&str, Vec<String>> = columns
            .iter()
            .map(|c| (c.path.as_str(), c.imports.clone()))
            .collect();
        refresh_file_import_entries_precomputed(&tx, files, &imports_by_file)?;

        insert_entities_with_content_store(&tx, root, &entities.iter().collect::<Vec<_>>(), true)?;
        insert_edges_table(&tx, graph)?;

        let test_entity_ids = write_test_flags(&tx, graph, entities, custom_test_dirs)?;

        set_cache_kind(&tx, CACHE_KIND_FULL)?;
        set_cache_source_scope(&tx, source_scope)?;
        // Best-effort: this is `sem repos`' labeling metadata, not part of
        // the cache's correctness contract, so a failure here should not
        // fail an otherwise-successful save that just paid for the rest of
        // this transaction's work.
        let _ = set_cache_repo_root(&tx, root);
        tx.commit()?;
        Ok(test_entity_ids)
    }

    /// Topology-only save: no entity bodies, no content store — cheaper than
    /// [`Self::save_with_test_dirs`] for callers that only need graph shape
    /// (large-repo `sem impact`). See [`Self::save_topology_precomputed`] for
    /// the precomputed-columns entry point.
    pub fn save_topology(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        custom_test_dirs: &[String],
        source_scope: CacheSourceScope,
    ) -> Result<HashSet<String>, rusqlite::Error> {
        self.save_topology_precomputed(
            root,
            files,
            graph,
            entities,
            custom_test_dirs,
            source_scope,
            None,
        )
    }

    /// [`Self::save_topology`] with the corpus read optionally supplied by
    /// the caller — see [`Self::save_with_test_dirs_precomputed`]'s doc for
    /// why this exists.
    #[allow(clippy::too_many_arguments)]
    pub fn save_topology_precomputed(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        custom_test_dirs: &[String],
        source_scope: CacheSourceScope,
        precomputed: Option<&[FileCacheColumns]>,
    ) -> Result<HashSet<String>, rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch(
            "DELETE FROM files; DELETE FROM entities; DELETE FROM edges; DELETE FROM file_imports; DELETE FROM entity_flags;",
        )?;

        let owned_columns;
        let columns: &[FileCacheColumns] = match precomputed {
            Some(c) => c,
            None => {
                owned_columns = read_file_cache_columns(root, files);
                &owned_columns
            }
        };

        insert_files_table(&tx, columns)?;
        refresh_manifest_entries(&tx, root)?;
        let imports_by_file: HashMap<&str, Vec<String>> = columns
            .iter()
            .map(|c| (c.path.as_str(), c.imports.clone()))
            .collect();
        refresh_file_import_entries_precomputed(&tx, files, &imports_by_file)?;

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

        insert_edges_table(&tx, graph)?;

        let test_entity_ids = write_test_flags(&tx, graph, entities, custom_test_dirs)?;

        set_cache_kind(&tx, CACHE_KIND_TOPOLOGY)?;
        set_cache_source_scope(&tx, source_scope)?;
        let _ = set_cache_repo_root(&tx, root);
        tx.commit()?;
        Ok(test_entity_ids)
    }

    pub fn load(&self, root: &Path, files: &[String]) -> Option<(EntityGraph, Vec<SemanticEntity>)> {
        self.load_with_source_scope(root, files, CacheSourceScope::Default)
    }

    pub fn load_with_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: CacheSourceScope,
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
            let mut recon = ContentReconstructor::new(&self.conn);
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

        let edges = self.load_edges()?;

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
    pub fn load_graph_topology(&self, root: &Path, files: &[String]) -> Option<EntityGraph> {
        self.load_graph_topology_with_source_scope(root, files, CacheSourceScope::Default)
    }

    pub fn load_graph_topology_with_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: CacheSourceScope,
    ) -> Option<EntityGraph> {
        if !self.has_fresh_topology_cache(root, files, source_scope) {
            return None;
        }

        self.load_graph_topology_rows()
    }

    pub fn load_graph_topology_with_test_ids(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<(EntityGraph, HashSet<String>)> {
        self.load_graph_topology_with_test_ids_and_source_scope(
            root,
            files,
            CacheSourceScope::Default,
        )
    }

    pub fn load_graph_topology_with_test_ids_and_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: CacheSourceScope,
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
        source_scope: CacheSourceScope,
    ) -> bool {
        if !cache_has_kind(&self.conn, &[CACHE_KIND_FULL]) {
            return false;
        }

        self.has_fresh_cache(root, files, source_scope)
    }

    fn has_fresh_topology_cache(
        &self,
        root: &Path,
        files: &[String],
        source_scope: CacheSourceScope,
    ) -> bool {
        if !cache_has_kind(&self.conn, &[CACHE_KIND_FULL, CACHE_KIND_TOPOLOGY]) {
            return false;
        }

        self.has_fresh_cache(root, files, source_scope)
    }

    fn has_fresh_topology_only_cache(
        &self,
        root: &Path,
        files: &[String],
        source_scope: CacheSourceScope,
    ) -> bool {
        if !cache_has_kind(&self.conn, &[CACHE_KIND_TOPOLOGY]) {
            return false;
        }

        self.has_fresh_cache(root, files, source_scope)
    }

    fn has_fresh_cache(&self, root: &Path, files: &[String], source_scope: CacheSourceScope) -> bool {
        if !cache_has_source_scope(&self.conn, source_scope) {
            return false;
        }

        if is_manifest_stale(&self.conn, root) {
            return false;
        }

        let cached_count: i64 = match self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        {
            Ok(count) => count,
            Err(_) => return false,
        };
        if (cached_count - manifest_entry_count(&self.conn)) as usize != source_file_count(files) {
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
            if is_manifest_file_name(file) {
                continue;
            }
            let Some((secs, nanos, content_hash)) = cached_mtimes.get(file.as_str()) else {
                return false;
            };
            let full = root.join(file);
            match file_freshness(&full, *secs, *nanos, content_hash.as_deref()) {
                Some(FileFreshness::Fresh) => {}
                Some(FileFreshness::FreshWithUpdatedFingerprint {
                    secs,
                    nanos,
                    content_hash,
                }) => {
                    fingerprint_refreshes.push(FileFingerprintRefresh {
                        path: file.clone(),
                        mtime_secs: secs,
                        mtime_nanos: nanos,
                        content_hash,
                    });
                }
                Some(FileFreshness::Stale) | None => return false,
            }
        }

        refresh_file_fingerprints_best_effort(&self.conn, &fingerprint_refreshes);
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
    pub fn load_partial(&self, root: &Path, files: &[String]) -> Option<PartialCache> {
        self.load_partial_with_source_scope(root, files, CacheSourceScope::Default)
    }

    pub fn load_partial_with_source_scope(
        &self,
        root: &Path,
        files: &[String],
        source_scope: CacheSourceScope,
    ) -> Option<PartialCache> {
        if !cache_has_kind(&self.conn, &[CACHE_KIND_FULL]) {
            return None;
        }

        if !cache_has_source_scope(&self.conn, source_scope) {
            return None;
        }

        if is_manifest_stale(&self.conn, root) {
            return None;
        }

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
            .filter(|file| !is_manifest_file_name(file))
            .collect();
        let source_file_count = source_files.len();
        let current_set: HashSet<&str> = source_files.iter().map(|file| file.as_str()).collect();

        let mut stale_source_files: Vec<String> = Vec::new();
        let mut stale_current_file_count = 0;
        let mut fingerprint_refreshes = Vec::new();
        for file in &source_files {
            match cached_files.get(file.as_str()) {
                Some((secs, nanos, content_hash)) => {
                    let full = root.join(file.as_str());
                    match file_freshness(&full, *secs, *nanos, content_hash.as_deref()) {
                        Some(FileFreshness::Fresh) => {}
                        Some(FileFreshness::FreshWithUpdatedFingerprint {
                            secs,
                            nanos,
                            content_hash,
                        }) => {
                            fingerprint_refreshes.push(FileFingerprintRefresh {
                                path: (*file).clone(),
                                mtime_secs: secs,
                                mtime_nanos: nanos,
                                content_hash,
                            });
                        }
                        Some(FileFreshness::Stale) | None => {
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
            if !is_cache_manifest_key(cached_path)
                && !is_manifest_file_name(cached_path)
                && !current_set.contains(cached_path.as_str())
            {
                deleted_cached_files.push(cached_path.clone());
            }
        }

        refresh_file_fingerprints_best_effort(&self.conn, &fingerprint_refreshes);

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
        let cached_importing_stale_files =
            cached_importing_files_for_stale_files(&self.conn, &import_stale_files, &source_files);

        // Load ALL entities, split into clean vs stale-file
        let mut entity_stmt = self
            .conn
            .prepare("SELECT id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json, start_byte, end_byte, kappa FROM entities")
            .ok()?;
        let mut cached_entities = Vec::new();
        let mut stale_file_entities = Vec::new();
        let mut recon = ContentReconstructor::new(&self.conn);
        let mut entity_rows = entity_stmt.query([]).ok()?;
        while let Some(row) = entity_rows.next().ok()? {
            let metadata_json: Option<String> = row.get(10).ok()?;
            let mut entity = SemanticEntity {
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

        let cached_edges = self.load_edges().unwrap_or_default();

        Some(PartialCache {
            stale_files: stale_source_files,
            cached_entities,
            cached_edges,
            cached_importing_stale_files,
            stale_file_entities,
        })
    }

    /// Incrementally update the cache: only rewrite stale file entries.
    pub fn save_incremental(
        &self,
        root: &Path,
        all_files: &[String],
        stale_files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        source_scope: CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        self.save_incremental_with_repair_metadata(
            root,
            all_files,
            stale_files,
            graph,
            entities,
            false,
            &[],
            &[],
            source_scope,
        )
    }

    /// Incrementally update the cache with graph-repair metadata. Pure SQL —
    /// unlike the full/topology saves this never fused with an `index.sem`
    /// write even before (the old `sem-cli` copy re-read the corpus
    /// for `index.sem` separately here regardless), so there is nothing for a
    /// caller to precompute and hand down.
    #[allow(clippy::too_many_arguments)]
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
        source_scope: CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        let source_stale_files: Vec<&String> = stale_files
            .iter()
            .filter(|file| !is_manifest_file_name(file))
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
            .filter(|path| !is_manifest_file_name(path))
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
                !is_cache_manifest_key(path)
                    && !is_manifest_file_name(path)
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
                if let Some((secs, nanos, content_hash)) = file_fingerprint(&full) {
                    ins.execute(params![file, secs, nanos, content_hash])?;
                }
            }
        }

        refresh_manifest_entries(&tx, root)?;
        let mut import_files_to_refresh: Vec<String> = source_stale_files
            .iter()
            .map(|file| (*file).clone())
            .collect();
        import_files_to_refresh.extend(deleted_cached_files.iter().cloned());
        refresh_file_import_entries(&tx, root, &import_files_to_refresh, all_files)?;

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
            insert_entities_with_content_store(&tx, root, &to_insert, true)?;
        }

        if repair_changed_clean_entity_ids {
            tx.execute("DELETE FROM edges", [])?;
            insert_edges_table(&tx, graph)?;
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

        set_cache_kind(&tx, CACHE_KIND_FULL)?;
        set_cache_source_scope(&tx, source_scope)?;
        // An incremental write leaves the cache reflecting the current working
        // tree, which may differ from HEAD (uncommitted edits). Refresh the
        // epoch so freshness reflects reality now.
        let _ = set_cache_repo_root(&tx, root);
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Focused tests for the divergences reconciled between the two
    //! pre-unification `DiskCache` copies. Each consumer crate's own,
    //! larger cache test suite (sem-cli's `build_cache.rs`, sem-mcp's
    //! `cache.rs`) still runs unchanged against this module's re-exported
    //! surface — this module only adds coverage for behavior that changed.
    use super::*;

    fn temp_repo_root(test_name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("sem-disk-cache-test-{test_name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        env::set_var("SEM_CACHE_DIR", root.join(".cache"));
        root
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn cleanup(root: PathBuf) {
        let _ = std::fs::remove_dir_all(&root);
    }

    fn empty_graph() -> EntityGraph {
        EntityGraph::from_parts(EntityInfoMap::default(), Vec::new())
    }

    fn entity(id: &str, file_path: &str, name: &str) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            name: name.to_string(),
            entity_type: "function".to_string(),
            file_path: file_path.to_string(),
            start_line: 1,
            end_line: 1,
            start_byte: None,
            end_byte: None,
            content: "fn f() {}".to_string(),
            content_hash: "h".to_string(),
            structural_hash: None,
            kappa: None,
            parent_id: None,
            metadata: None,
        }
    }

    /// The gap this change closed: MCP's old bare `save` never called
    /// `write_test_flags`, so `entity_flags` stayed empty forever on any
    /// cache MCP originated — a correctness gap at the exact file both
    /// consumers share. The unified `save()` now routes through
    /// `save_with_test_dirs(&[])`, which always classifies (even with an
    /// empty custom-dirs list, `filter_test_entities_with_custom_dirs` still
    /// applies its built-in test-path heuristics), so `entity_flags`'
    /// completion marker is always set after any save.
    #[test]
    fn bare_save_always_marks_test_flags_computed() {
        let root = temp_repo_root("bare-save-test-flags");
        let files = vec!["src/a.rs".to_string()];
        write_file(&root.join("src/a.rs"), "fn f() {}\n");

        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(&root, &files, &empty_graph(), &[entity("a", "src/a.rs", "f")], CacheSourceScope::Default)
            .unwrap();

        let computed: bool = cache
            .conn
            .query_row(
                "SELECT value FROM cache_metadata WHERE key = 'test_flags_computed'",
                [],
                |row| row.get::<_, String>(0),
            )
            .is_ok();
        assert!(computed, "save() must mark test flags computed");
        cleanup(root);
    }

    /// The precomputed-columns path must produce a cache indistinguishable
    /// (via the public load API) from the internal-fusion path, for the same
    /// on-disk files — proving `Some(columns)` isn't a second, subtly
    /// different write path from `None`.
    #[test]
    fn precomputed_columns_match_internal_fusion() {
        let root_a = temp_repo_root("precomputed-a");
        let root_b = temp_repo_root("precomputed-b");
        let files = vec!["src/a.ts".to_string(), "src/b.ts".to_string()];
        for root in [&root_a, &root_b] {
            write_file(&root.join("src/a.ts"), "import { b } from './b';\nexport const a = 1;\n");
            write_file(&root.join("src/b.ts"), "export const b = 2;\n");
        }
        let graph = empty_graph();
        let entities = vec![
            entity("a-id", "src/a.ts", "a"),
            entity("b-id", "src/b.ts", "b"),
        ];

        let cache_a = DiskCache::open(&root_a).unwrap();
        cache_a
            .save_with_test_dirs_precomputed(
                &root_a,
                &files,
                &graph,
                &entities,
                &[],
                CacheSourceScope::Default,
                None,
            )
            .unwrap();

        let precomputed = read_file_cache_columns(&root_b, &files);
        let cache_b = DiskCache::open(&root_b).unwrap();
        cache_b
            .save_with_test_dirs_precomputed(
                &root_b,
                &files,
                &graph,
                &entities,
                &[],
                CacheSourceScope::Default,
                Some(&precomputed),
            )
            .unwrap();

        let (_, loaded_a) = cache_a.load(&root_a, &files).unwrap();
        let (_, loaded_b) = cache_b.load(&root_b, &files).unwrap();
        let mut ids_a: Vec<&str> = loaded_a.iter().map(|e| e.id.as_str()).collect();
        let mut ids_b: Vec<&str> = loaded_b.iter().map(|e| e.id.as_str()).collect();
        ids_a.sort_unstable();
        ids_b.sort_unstable();
        assert_eq!(ids_a, ids_b);

        let imports_a: i64 = cache_a
            .conn
            .query_row("SELECT COUNT(*) FROM file_imports", [], |r| r.get(0))
            .unwrap();
        let imports_b: i64 = cache_b
            .conn
            .query_row("SELECT COUNT(*) FROM file_imports", [], |r| r.get(0))
            .unwrap();
        assert_eq!(imports_a, imports_b);
        assert_eq!(imports_a, 1, "src/a.ts's import of src/b.ts must resolve");

        cleanup(root_a);
        cleanup(root_b);
    }

    /// `set_cache_repo_root`'s failure must not fail the save transaction —
    /// exercised indirectly here by confirming a save still commits (and is
    /// loadable) even though this is best-effort metadata, matching the
    /// robustness discipline the rest of the save path documents.
    #[test]
    fn save_commits_even_though_repo_root_stamp_is_best_effort() {
        let root = temp_repo_root("repo-root-best-effort");
        let files = vec!["src/a.rs".to_string()];
        write_file(&root.join("src/a.rs"), "fn f() {}\n");
        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(&root, &files, &empty_graph(), &[entity("a", "src/a.rs", "f")], CacheSourceScope::Default)
            .unwrap();
        assert!(cache.load(&root, &files).is_some());
        cleanup(root);
    }
}
