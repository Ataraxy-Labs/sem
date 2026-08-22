//! Thin re-export of `sem_core::persist::disk_cache` (semx-r94).
//!
//! `sem-mcp` used to hand-roll its own `struct DiskCache` here, independently
//! of `sem-cli`'s `build_cache.rs` copy, over the same `cache.db` schema —
//! the duplicate-authority finding QUERY-INDEX.md §15.3 disclosed and left
//! open ("semx-r94 stands"). The schema, freshness/manifest handling, content
//! store, semantic commit index and the `DiskCache` constructor family now
//! live once, in `sem-core::persist::disk_cache` (see that module's doc for
//! the ownership rationale and the divergences this unification reconciled —
//! notably, this crate's own `save()` never used to classify test entities,
//! silently losing `entity_flags` on any cache `sem mcp` originated; the
//! unified `save()` now always does).
//!
//! `sem-mcp` has no crate-specific glue left over `DiskCache` itself — unlike
//! `sem-cli`, it never writes `index.sem` or a facts-store corpus, so there
//! is nothing here to keep local beyond this re-export. The test module below
//! is this crate's own behavioral spec for the re-exported surface (kept
//! local rather than folded into `sem-core`'s tests, per semx-r94's
//! instruction that each consumer's existing cache tests keep running
//! unchanged) — it needs the same supporting imports the pre-unification
//! `DiskCache` implementation here used to.

pub use sem_core::persist::disk_cache::*;

// Everything below is support for the test module only (`use super::*`) —
// production code needs nothing but the re-export above. `rusqlite` in
// particular is a dev-dependency now (see Cargo.toml), so this whole block
// must be test-only or a non-test build fails to resolve it.
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::hash::{Hash, Hasher};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};

#[cfg(test)]
#[allow(unused_imports)]
use rusqlite::{params, Connection, OptionalExtension, Transaction};
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::git::bridge::GitBridge;
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::git::types::{CommitInfo, DiffScope};
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::model::entity::SemanticEntity;
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::parser::differ::compute_semantic_diff;
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityInfoMap, EntityRef, RefType};
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::parser::hotspot::{aggregate_history_analytics, CommitEntityChanges, HistoryAnalytics};
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::parser::registry::ParserRegistry;
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::parser::{js_ts_import_source_files_from_set, ImportCandidates};
#[cfg(test)]
#[allow(unused_imports)]
use sem_core::utils::hash::content_hash_bytes;

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
                    .join(format!("sem-mcp-test-cache-{}-{nanos}", std::process::id()));
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
            "sem-mcp-cache-{test_name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "tester")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "tester")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn commit_py(dir: &Path, content: &str, msg: &str) {
        write_file(&dir.join("main.py"), content);
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", msg, "--no-gpg-sign"]);
    }

    #[test]
    fn semantic_commit_index_matches_live_walk_and_is_incremental() {
        let root = temp_repo_root("commit-index");
        run_git(&root, &["init", "-q"]);
        commit_py(
            &root,
            "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
            "c1",
        );
        commit_py(
            &root,
            "def alpha():\n    return 10\n\ndef beta():\n    return 20\n",
            "c2",
        );
        commit_py(
            &root,
            "def alpha():\n    return 100\n\ndef beta():\n    return 20\n",
            "c3",
        );

        let git_bridge = GitBridge::open(&root).unwrap();
        let registry = sem_core::parser::plugins::create_default_registry();

        let live =
            sem_core::parser::hotspot::compute_history_analytics(&git_bridge, &registry, None, 50);
        let stored = history_analytics_from_store(&root, &git_bridge, &registry, None, 50)
            .expect("store-backed analytics");

        assert_eq!(stored.commits_scanned, live.commits_scanned);
        assert_eq!(stored.hotspots.len(), live.hotspots.len());
        for (s, l) in stored.hotspots.iter().zip(live.hotspots.iter()) {
            assert_eq!(
                (
                    s.entity_name.as_str(),
                    s.commits,
                    s.authors,
                    s.last_short_sha.as_str()
                ),
                (
                    l.entity_name.as_str(),
                    l.commits,
                    l.authors,
                    l.last_short_sha.as_str()
                )
            );
        }
        assert_eq!(stored.co_changes.len(), live.co_changes.len());

        // A second query indexes nothing new.
        let commits = git_bridge.get_log(50).unwrap();
        let mut cache = DiskCache::open(&root).unwrap();
        assert_eq!(
            cache
                .index_commits(&git_bridge, &registry, &commits[..commits.len() - 1])
                .unwrap(),
            0
        );

        // A new commit indexes exactly one more.
        commit_py(
            &root,
            "def alpha():\n    return 1000\n\ndef beta():\n    return 20\n",
            "c4",
        );
        let commits = git_bridge.get_log(50).unwrap();
        assert_eq!(
            cache
                .index_commits(&git_bridge, &registry, &commits[..commits.len() - 1])
                .unwrap(),
            1
        );
        drop(cache);

        let stored2 =
            history_analytics_from_store(&root, &git_bridge, &registry, None, 50).unwrap();
        let alpha = stored2
            .hotspots
            .iter()
            .find(|h| h.entity_name == "alpha")
            .expect("alpha hotspot");
        assert_eq!(alpha.commits, 3); // c2, c3, c4
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

    #[test]
    fn content_store_round_trip_is_byte_identical() {
        let root = temp_repo_root("content-store-roundtrip");
        // A real source file so the extractor assigns byte spans, including a
        // multi-byte character to stress slicing, plus a nested class so the
        // 2x duplication case (class contains method) is exercised.
        let src = "class Greeter:\n    def hello(self):\n        return \"h\u{00e9}llo\"\n\n\ndef standalone():\n    return 1\n";
        std::fs::write(root.join("app.py"), src).unwrap();

        let registry = sem_core::parser::plugins::create_default_registry();
        let files = vec!["app.py".to_string()];
        let (graph, entities) =
            sem_core::parser::graph::EntityGraph::build(&root, &files, &registry);
        assert!(!entities.is_empty());
        let spanned = entities.iter().filter(|e| e.start_byte.is_some()).count();
        assert!(spanned > 0, "extractor should assign byte spans");

        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(&root, &files, &graph, &entities, CacheSourceScope::Default)
            .unwrap();

        // The store must actually engage: at least one row holds NULL content
        // and the file text landed in file_contents.
        let null_rows: i64 = cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE content IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(null_rows > 0, "no entity used the content store");
        let stored_files: i64 = cache
            .connection()
            .query_row("SELECT COUNT(*) FROM file_contents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored_files, 1);

        // Round trip: every entity's content byte-identical after load.
        let (_, loaded) = cache
            .load_with_source_scope(&root, &files, CacheSourceScope::Default)
            .expect("cache should load");
        assert_eq!(loaded.len(), entities.len());
        let by_id: HashMap<&str, &SemanticEntity> =
            entities.iter().map(|e| (e.id.as_str(), e)).collect();
        for l in &loaded {
            let orig = by_id.get(l.id.as_str()).expect("entity survived");
            assert_eq!(
                l.content, orig.content,
                "content mismatch for {} after round trip",
                l.id
            );
        }
        cleanup(root);
    }

    fn cleanup(root: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(&root);
        if let Some(cache_dir) = cache_dir_for_repo(&root) {
            let _ = std::fs::remove_dir_all(cache_dir);
        }
    }

    fn save_empty_cache(root: &Path, files: &[String]) -> DiskCache {
        let cache = DiskCache::open(root).unwrap();
        cache
            .save(root, files, &empty_graph(), &[], CacheSourceScope::Default)
            .unwrap();
        assert!(cache.load(root, files).is_some());
        cache
    }

    #[test]
    fn refresh_file_fingerprints_rolls_back_batch_on_failure() {
        let root = temp_repo_root("mtime-refresh-rollback");
        write_file(&root.join("a.rs"), "fn a() {}\n");
        write_file(&root.join("b.rs"), "fn b() {}\n");
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let cache = save_empty_cache(&root, &files);
        let before_a = cached_file_mtime(&cache, "a.rs");
        let before_b = cached_file_mtime(&cache, "b.rs");

        cache
            .connection()
            .execute_batch(
                "CREATE TRIGGER fail_b_refresh
                 BEFORE UPDATE ON files
                 WHEN OLD.path = 'b.rs'
                 BEGIN
                     SELECT RAISE(FAIL, 'stop refresh');
                 END;",
            )
            .unwrap();

        let err = refresh_file_fingerprints(
            cache.connection(),
            &[
                FileFingerprintRefresh {
                    path: "a.rs".to_string(),
                    mtime_secs: before_a.0 + 1,
                    mtime_nanos: before_a.1,
                    content_hash: "updated-a".to_string(),
                },
                FileFingerprintRefresh {
                    path: "b.rs".to_string(),
                    mtime_secs: before_b.0 + 1,
                    mtime_nanos: before_b.1,
                    content_hash: "updated-b".to_string(),
                },
            ],
        )
        .unwrap_err();

        assert!(err.to_string().contains("stop refresh"));
        assert_eq!(cached_file_mtime(&cache, "a.rs"), before_a);
        assert_eq!(cached_file_mtime(&cache, "b.rs"), before_b);

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
                CacheSourceScope::Default,
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
        let current_c = file_mtime_parts(&root.join("c.ts")).unwrap();
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
                CacheSourceScope::Default,
            )
            .unwrap();
        assert_eq!(file_import_count(&cache, "a.ts", "b.ts"), 0);
        assert_eq!(file_import_count(&cache, "a.ts", "c.ts"), 1);

        drop(cache);
        cleanup(root);
    }

    fn rewrite_after_mtime_tick(path: &Path, content: &str) {
        let before = file_mtime_parts(path).unwrap();

        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            write_file(path, content);
            if file_mtime_parts(path).unwrap() != before {
                return;
            }
        }

        panic!("mtime did not change for {}", path.display());
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
            .save(&root, &files, &graph, &entities, CacheSourceScope::Custom)
            .unwrap();

        assert!(cache
            .load_with_source_scope(&root, &files, CacheSourceScope::Default)
            .is_none());
        assert!(cache
            .load_with_source_scope(&root, &files, CacheSourceScope::Custom)
            .is_some());
        assert!(cache
            .load_partial_with_source_scope(&root, &files, CacheSourceScope::Default)
            .is_none());

        rewrite_after_mtime_tick(&root.join("b.ts"), "export function b() { return 2; }\n");
        let partial = cache
            .load_partial_with_source_scope(&root, &files, CacheSourceScope::Custom)
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
                CacheSourceScope::Custom,
            )
            .unwrap();

        assert!(cache
            .load_with_source_scope(&root, &files, CacheSourceScope::Default)
            .is_none());
        assert!(cache
            .load_with_source_scope(&root, &files, CacheSourceScope::Custom)
            .is_some());

        drop(cache);
        cleanup(root);
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

        for (expected, _, _) in CACHE_INDEXES {
            assert!(indexes.contains(*expected), "missing index {expected}");
        }
    }

    fn assert_table_empty(cache: &DiskCache, table: &str) {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let count: i64 = cache.connection().query_row(&sql, [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "{table} should be empty after schema rebuild");
    }

    fn seed_unsupported_cache(root: &Path, version: i32) {
        let cache_dir = cache_dir_for_repo(root).unwrap();
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
    fn manifest_hash_tracks_gitattributes_changes() {
        let root = temp_repo_root("gitattributes-manifest-hash");
        let files = sample_files(&root);
        let gitattributes = root.join(".gitattributes");

        let without_gitattributes = compute_manifest_hash(&root, &files).unwrap();

        write_file(&gitattributes, "*.foo linguist-language=javascript\n");
        let with_gitattributes = compute_manifest_hash(&root, &files).unwrap();
        assert_ne!(without_gitattributes, with_gitattributes);

        rewrite_after_mtime_tick(&gitattributes, "*.foo linguist-language=typescript\n");
        let modified_gitattributes = compute_manifest_hash(&root, &files).unwrap();
        assert_ne!(with_gitattributes, modified_gitattributes);

        std::fs::remove_file(&gitattributes).unwrap();
        let removed_gitattributes = compute_manifest_hash(&root, &files).unwrap();
        assert_eq!(without_gitattributes, removed_gitattributes);

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
                .map(|(file, _)| file_mtime_parts(&root.join(*file)).unwrap())
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
                CacheSourceScope::Default,
            )
            .unwrap();

        let entities = vec![
            entity("stale-id", "stale.rs", "stale", "stale new"),
            entity("clean-id", "clean.rs", "clean", "clean should stay cached"),
        ];
        cache
            .save_incremental(
                &root,
                &files,
                &["stale.rs".to_string()],
                &empty_graph(),
                &entities,
                CacheSourceScope::Default,
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
    fn save_incremental_wrapper_rewrites_stale_source_edges() {
        let root = temp_repo_root("incremental-wrapper-edges");
        write_file(&root.join("stale.rs"), "fn stale() {}\n");
        write_file(&root.join("clean.rs"), "fn clean() {}\n");
        write_file(&root.join("old.rs"), "fn old_target() {}\n");
        write_file(&root.join("new.rs"), "fn new_target() {}\n");
        let files = vec![
            "stale.rs".to_string(),
            "clean.rs".to_string(),
            "old.rs".to_string(),
            "new.rs".to_string(),
        ];
        let cache = DiskCache::open(&root).unwrap();
        let entities = vec![
            entity("stale-id", "stale.rs", "stale", "stale old"),
            entity("clean-id", "clean.rs", "clean", "clean old"),
            entity("old-target-id", "old.rs", "old_target", "old target"),
            entity("new-target-id", "new.rs", "new_target", "new target"),
        ];
        let initial_graph = graph_with_edges(
            &entities,
            vec![
                edge("stale-id", "old-target-id"),
                edge("clean-id", "old-target-id"),
            ],
        );
        cache
            .save(
                &root,
                &files,
                &initial_graph,
                &entities,
                CacheSourceScope::Default,
            )
            .unwrap();

        let updated_entities = vec![
            entity("stale-id", "stale.rs", "stale", "stale new"),
            entity("clean-id", "clean.rs", "clean", "clean should stay cached"),
            entity("old-target-id", "old.rs", "old_target", "old target"),
            entity("new-target-id", "new.rs", "new_target", "new target"),
        ];
        let updated_graph = graph_with_edges(
            &updated_entities,
            vec![
                edge("stale-id", "new-target-id"),
                edge("clean-id", "old-target-id"),
            ],
        );
        cache
            .save_incremental(
                &root,
                &files,
                &["stale.rs".to_string()],
                &updated_graph,
                &updated_entities,
                CacheSourceScope::Default,
            )
            .unwrap();

        assert_eq!(edge_count(&cache, "stale-id", "old-target-id"), 0);
        assert_eq!(edge_count(&cache, "stale-id", "new-target-id"), 1);
        assert_eq!(edge_count(&cache, "clean-id", "old-target-id"), 1);
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
                CacheSourceScope::Default,
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
                CacheSourceScope::Default,
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
    fn open_creates_schema_version_and_lookup_indexes() {
        let root = temp_repo_root("schema");
        let cache = DiskCache::open(&root).unwrap();

        assert_eq!(read_user_version(&cache), CACHE_SCHEMA_VERSION);
        assert_lookup_indexes(&cache);
        assert!(cache_db_path(&root).unwrap().exists());
        assert!(!root.join(".sem").exists());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn create_cache_dir_preserves_directory_creation_error() {
        let blocked = std::env::temp_dir().join(format!(
            "sem-mcp-cache-blocked-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&blocked, "not a directory").unwrap();
        let cache_dir = blocked.join("child");

        let err = create_cache_dir(&cache_dir).unwrap_err();

        match err {
            rusqlite::Error::SqliteFailure(sqlite_error, Some(message)) => {
                assert_eq!(sqlite_error.code, rusqlite::ErrorCode::CannotOpen);
                assert!(message.contains("failed to create cache directory"));
                assert!(message.contains(&cache_dir.display().to_string()));
            }
            other => panic!("expected preserved directory creation error, got {other:?}"),
        }

        let _ = std::fs::remove_file(blocked);
    }

    #[test]
    fn cache_path_is_external_and_canonicalized() {
        let root = temp_repo_root("external-path");
        let cache_dir = cache_dir_for_repo(&root).unwrap();

        assert_eq!(cache_dir, cache_dir_for_repo(&root.join(".")).unwrap());
        assert!(!cache_dir.starts_with(&root));

        let cache = DiskCache::open(&root).unwrap();
        assert!(cache_db_path(&root).unwrap().exists());
        assert!(!root.join(".sem").exists());

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn open_rebuilds_cache_when_schema_version_is_unsupported() {
        for version in [0, CACHE_SCHEMA_VERSION - 1, CACHE_SCHEMA_VERSION + 1] {
            let root = temp_repo_root(&format!("unsupported-{version}"));
            seed_unsupported_cache(&root, version);

            let cache = DiskCache::open(&root).unwrap();

            assert_eq!(read_user_version(&cache), CACHE_SCHEMA_VERSION);
            assert_lookup_indexes(&cache);
            for table in ["files", "entities", "edges"] {
                assert_table_empty(&cache, table);
            }

            drop(cache);
            cleanup(root);
        }
    }

    /// v1.1 (semx-2i2): kappa must round-trip through sem-mcp's own on-disk
    /// SQLite entity cache too -- a separate `DiskCache`/`save`/`load` from
    /// sem-cli's, sharing only the schema/insert helpers in this module
    /// (`initialize_schema`, `insert_entities_with_content_store`), so it
    /// needs its own proof. Mirrors
    /// `sem-cli/src/cache.rs::tests::kappa_round_trips_through_disk_cache`.
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
        let original_kappa: std::collections::HashMap<String, String> = entities
            .iter()
            .map(|e| (e.name.clone(), e.kappa.clone().expect("kappa computed")))
            .collect();
        assert_ne!(
            original_kappa["mutableCounter"], original_kappa["frozenCounter"],
            "sanity: let vs const must differ before the round trip"
        );

        let graph = graph_with_edges(&entities, vec![]);
        let cache = DiskCache::open(&root).unwrap();
        cache
            .save(&root, &files, &graph, &entities, CacheSourceScope::Default)
            .unwrap();
        drop(cache);

        let reopened = DiskCache::open(&root).unwrap();
        let (_graph, loaded_entities) = reopened
            .load(&root, &files)
            .expect("cache should be fresh and complete");
        assert_eq!(loaded_entities.len(), 3);

        for loaded in &loaded_entities {
            let expected = &original_kappa[&loaded.name];
            assert_eq!(
                loaded.kappa.as_deref(),
                Some(expected.as_str()),
                "entity `{}` must keep its kappa through save/reopen/load",
                loaded.name
            );
        }

        drop(reopened);
        cleanup(root);
    }
}
