use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use rusqlite::{params, Connection, Transaction};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};

pub const CACHE_SCHEMA_VERSION: i32 = 2;
pub const CACHE_INDEXES: &[(&str, &str, &str)] = &[
    ("idx_entities_file_path", "entities", "file_path"),
    ("idx_entities_name", "entities", "name"),
    ("idx_entities_parent_id", "entities", "parent_id"),
    ("idx_edges_from_entity", "edges", "from_entity"),
    ("idx_edges_to_entity", "edges", "to_entity"),
];

// Cache-only keys use a NUL prefix so they cannot collide with git paths.
pub const CACHE_MANIFEST_FILES: &[(&str, &str)] = &[
    (".semrc", "\0sem-manifest:.semrc"),
    (".gitattributes", "\0sem-manifest:.gitattributes"),
];

const CACHE_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    mtime_secs INTEGER NOT NULL,
    mtime_nanos INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS entities (
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
CREATE TABLE IF NOT EXISTS edges (
    from_entity TEXT NOT NULL,
    to_entity TEXT NOT NULL,
    ref_type TEXT NOT NULL
);
";

const CACHE_RESET_SQL: &str = "
DROP TABLE IF EXISTS files;
DROP TABLE IF EXISTS entities;
DROP TABLE IF EXISTS edges;
";

pub fn initialize_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;",
    )?;

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

/// Result of a partial cache load: stale files that need reparsing, plus cached clean data.
pub struct PartialCache {
    pub stale_files: Vec<String>,
    pub cached_entities: Vec<SemanticEntity>,
    pub cached_edges: Vec<EntityRef>,
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

fn cached_file_mtime(conn: &Connection, cache_key: &str) -> Option<(i64, i64)> {
    conn.query_row(
        "SELECT mtime_secs, mtime_nanos FROM files WHERE path = ?1",
        params![cache_key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

pub fn is_manifest_stale(conn: &Connection, root: &Path) -> bool {
    CACHE_MANIFEST_FILES.iter().any(|(file_name, cache_key)| {
        let full = root.join(file_name);
        let cached = cached_file_mtime(conn, cache_key);

        match (full.exists(), cached) {
            (true, None) | (false, Some(_)) => true,
            (false, None) => false,
            (true, Some((secs, nanos))) => match file_mtime_parts(&full) {
                Some((current_secs, current_nanos)) => {
                    secs != current_secs || nanos != current_nanos
                }
                None => true,
            },
        }
    })
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
        "INSERT OR REPLACE INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)",
    )?;
    for (file_name, cache_key) in CACHE_MANIFEST_FILES {
        let full = root.join(file_name);
        if let Some((secs, nanos)) = file_mtime_parts(&full) {
            insert.execute(params![cache_key, secs, nanos])?;
        }
    }

    Ok(())
}

pub struct DiskCache {
    conn: Connection,
}

impl DiskCache {
    pub fn open(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let cache_dir = repo_root.join(".sem");
        std::fs::create_dir_all(&cache_dir).ok();
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path)?;

        initialize_schema(&conn)?;

        Ok(Self { conn })
    }

    /// Save the current graph + entities to the disk cache.
    pub fn save(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch("DELETE FROM files; DELETE FROM entities; DELETE FROM edges;")?;

        {
            let mut stmt = tx
                .prepare("INSERT INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)")?;
            for file in files {
                if is_manifest_file_name(file) {
                    continue;
                }
                let full = root.join(file);
                if let Some((secs, nanos)) = file_mtime_parts(&full) {
                    stmt.execute(params![file, secs, nanos])?;
                }
            }
        }

        refresh_manifest_entries(&tx, root)?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO entities (id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for e in entities {
                let metadata_json = e
                    .metadata
                    .as_ref()
                    .and_then(|m| serde_json::to_string(m).ok());
                stmt.execute(params![
                    e.id,
                    e.name,
                    e.entity_type,
                    e.file_path,
                    e.start_line as i64,
                    e.end_line as i64,
                    e.content,
                    e.content_hash,
                    e.structural_hash,
                    e.parent_id,
                    metadata_json,
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

        tx.commit()?;
        Ok(())
    }

    /// Try to load from disk cache. Returns None if mtimes don't match.
    pub fn load(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<(EntityGraph, Vec<SemanticEntity>)> {
        if is_manifest_stale(&self.conn, root) {
            return None;
        }

        // Verify file count matches
        let cached_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .ok()?;
        if (cached_count - manifest_entry_count(&self.conn)) as usize != source_file_count(files) {
            return None;
        }

        // Verify all mtimes match
        let mut stmt = self
            .conn
            .prepare("SELECT mtime_secs, mtime_nanos FROM files WHERE path = ?1")
            .ok()?;
        for file in files {
            if is_manifest_file_name(file) {
                continue;
            }
            let full = root.join(file);
            let (current_secs, current_nanos) = file_mtime_parts(&full)?;

            let (secs, nanos): (i64, i64) = stmt
                .query_row(params![file], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()?;
            if secs != current_secs || nanos != current_nanos {
                return None;
            }
        }

        // Load entities
        let mut entity_stmt = self
            .conn
            .prepare("SELECT id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json FROM entities")
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
                    content: row.get(6)?,
                    content_hash: row.get(7)?,
                    structural_hash: row.get(8)?,
                    parent_id: row.get(9)?,
                    metadata,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        // Load edges
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

        // Build entity map for graph
        let entity_map: HashMap<String, EntityInfo> = entities
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

    /// Load a partial cache: identify stale files and return clean cached data.
    /// Returns None if cache is empty or ALL files are stale (full rebuild is better).
    pub fn load_partial(&self, root: &Path, files: &[String]) -> Option<PartialCache> {
        if is_manifest_stale(&self.conn, root) {
            return None;
        }

        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime_secs, mtime_nanos FROM files")
            .ok()?;
        let cached_files: HashMap<String, (i64, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
                ))
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

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
        for file in source_files {
            match cached_files.get(file) {
                Some(&(secs, nanos)) => {
                    let full = root.join(file);
                    let is_stale = file_mtime_parts(&full)
                        .map(|(current_secs, current_nanos)| {
                            secs != current_secs || nanos != current_nanos
                        })
                        .unwrap_or(true);
                    if is_stale {
                        stale_current_file_count += 1;
                        stale_source_files.push(file.clone());
                    }
                }
                None => {
                    stale_current_file_count += 1;
                    stale_source_files.push(file.clone());
                }
            }
        }

        // Files in cache but not on disk anymore
        let mut deleted_cached_files: Vec<String> = Vec::new();
        for cached_path in cached_files.keys() {
            if !is_cache_manifest_key(cached_path)
                && !is_manifest_file_name(cached_path)
                && !current_set.contains(cached_path.as_str())
            {
                deleted_cached_files.push(cached_path.clone());
            }
        }

        if stale_source_files.is_empty() && deleted_cached_files.is_empty() {
            return None;
        }

        if stale_current_file_count >= source_file_count {
            return None;
        }

        let stale_set: HashSet<&str> = stale_source_files
            .iter()
            .chain(deleted_cached_files.iter())
            .map(|s| s.as_str())
            .collect();

        // Load ALL entities, split into clean vs stale-file
        let mut entity_stmt = self
            .conn
            .prepare("SELECT id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json FROM entities")
            .ok()?;
        let all_cached: Vec<SemanticEntity> = entity_stmt
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
                    content: row.get(6)?,
                    content_hash: row.get(7)?,
                    structural_hash: row.get(8)?,
                    parent_id: row.get(9)?,
                    metadata,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        let mut cached_entities = Vec::new();
        let mut stale_file_entities = Vec::new();
        for e in all_cached {
            if stale_set.contains(e.file_path.as_str()) {
                stale_file_entities.push(e);
            } else {
                cached_entities.push(e);
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
    ) -> Result<(), rusqlite::Error> {
        let source_stale_files: Vec<&String> = stale_files
            .iter()
            .filter(|file| !is_manifest_file_name(file))
            .collect();
        let stale_set: HashSet<&str> = source_stale_files
            .iter()
            .map(|file| file.as_str())
            .collect();

        let tx = self.conn.unchecked_transaction()?;
        let mut deleted_cached_files = Vec::new();

        {
            let mut del_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for f in &source_stale_files {
                del_files.execute(params![f])?;
            }
        }

        {
            let current_set: HashSet<&str> = all_files
                .iter()
                .map(|s| s.as_str())
                .filter(|path| !is_manifest_file_name(path))
                .collect();
            let mut cached_stmt = tx.prepare("SELECT path FROM files")?;
            let cached_paths: Vec<String> = cached_stmt
                .query_map([], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();
            let mut del_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for path in &cached_paths {
                if !is_cache_manifest_key(path) && !current_set.contains(path.as_str()) {
                    del_files.execute(params![path])?;
                    deleted_cached_files.push(path.clone());
                }
            }
        }

        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)",
            )?;
            for file in &source_stale_files {
                let full = root.join(file);
                if let Some((secs, nanos)) = file_mtime_parts(&full) {
                    ins.execute(params![file, secs, nanos])?;
                }
            }
        }

        refresh_manifest_entries(&tx, root)?;

        {
            let mut del = tx.prepare("DELETE FROM entities WHERE file_path = ?1")?;
            for f in &source_stale_files {
                del.execute(params![f])?;
            }
            for f in &deleted_cached_files {
                del.execute(params![f])?;
            }
        }

        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO entities (id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for e in entities {
                if stale_set.contains(e.file_path.as_str()) {
                    let metadata_json = e
                        .metadata
                        .as_ref()
                        .and_then(|m| serde_json::to_string(m).ok());
                    ins.execute(params![
                        e.id,
                        e.name,
                        e.entity_type,
                        e.file_path,
                        e.start_line as i64,
                        e.end_line as i64,
                        e.content,
                        e.content_hash,
                        e.structural_hash,
                        e.parent_id,
                        metadata_json,
                    ])?;
                }
            }
        }

        tx.execute("DELETE FROM edges", [])?;
        {
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
        }

        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo_root(test_name: &str) -> std::path::PathBuf {
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

    fn empty_graph() -> EntityGraph {
        EntityGraph::from_parts(HashMap::new(), Vec::new())
    }

    fn sample_files(root: &Path) -> Vec<String> {
        write_file(&root.join("sample.foo"), "export const alpha = () => 1;\n");
        vec!["sample.foo".to_string()]
    }

    fn save_empty_cache(root: &Path, files: &[String]) -> DiskCache {
        let cache = DiskCache::open(root).unwrap();
        cache.save(root, files, &empty_graph(), &[]).unwrap();
        assert!(cache.load(root, files).is_some());
        cache
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

        for (expected, _, _) in CACHE_INDEXES {
            assert!(indexes.contains(*expected), "missing index {expected}");
        }
    }

    fn assert_table_empty(cache: &DiskCache, table: &str) {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let count: i64 = cache.conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "{table} should be empty after schema rebuild");
    }

    fn seed_unsupported_cache(root: &Path, version: i32) {
        let cache_dir = root.join(".sem");
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

        let _ = std::fs::remove_dir_all(root);
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
        let _ = std::fs::remove_dir_all(root);
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
        let _ = std::fs::remove_dir_all(root);
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
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_creates_schema_version_and_lookup_indexes() {
        let root = temp_repo_root("schema");
        let cache = DiskCache::open(&root).unwrap();

        assert_eq!(read_user_version(&cache), CACHE_SCHEMA_VERSION);
        assert_lookup_indexes(&cache);

        drop(cache);
        let _ = std::fs::remove_dir_all(root);
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
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
