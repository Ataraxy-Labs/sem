use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use rusqlite::{params, Connection, Transaction};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};
use sem_core::parser::js_ts_import_source_files_from_content;
use sem_core::utils::hash::content_hash_bytes;

pub const CACHE_SCHEMA_VERSION: i32 = 4;
pub const CACHE_INDEXES: &[(&str, &str, &str)] = &[
    ("idx_entities_file_path", "entities", "file_path"),
    ("idx_entities_name", "entities", "name"),
    ("idx_entities_parent_id", "entities", "parent_id"),
    ("idx_edges_from_entity", "edges", "from_entity"),
    ("idx_edges_to_entity", "edges", "to_entity"),
    (
        "idx_file_imports_imported_file",
        "file_imports",
        "imported_file",
    ),
    (
        "idx_file_imports_importing_file",
        "file_imports",
        "importing_file",
    ),
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
CREATE TABLE IF NOT EXISTS file_imports (
    importing_file TEXT NOT NULL,
    imported_file TEXT NOT NULL,
    PRIMARY KEY (importing_file, imported_file)
);
";

const CACHE_RESET_SQL: &str = "
DROP TABLE IF EXISTS files;
DROP TABLE IF EXISTS entities;
DROP TABLE IF EXISTS edges;
DROP TABLE IF EXISTS file_imports;
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
                } else if !normalized.pop() {
                    if !has_prefix && !has_root {
                        normalized.push("..");
                    }
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
        "INSERT OR REPLACE INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, NULL)",
    )?;
    for (file_name, cache_key) in CACHE_MANIFEST_FILES {
        let full = root.join(file_name);
        if let Some((secs, nanos)) = file_mtime_parts(&full) {
            insert.execute(params![cache_key, secs, nanos])?;
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
    let candidate_files: Vec<String> = all_files
        .iter()
        .filter(|file| !is_manifest_file_name(file))
        .cloned()
        .collect();

    let mut delete = tx.prepare("DELETE FROM file_imports WHERE importing_file = ?1")?;
    let mut insert = tx.prepare(
        "INSERT OR IGNORE INTO file_imports (importing_file, imported_file) VALUES (?1, ?2)",
    )?;

    for file in files_to_refresh {
        if is_manifest_file_name(file) {
            continue;
        }

        delete.execute(params![file])?;
        let Ok(content) = std::fs::read_to_string(root.join(file)) else {
            continue;
        };
        for imported_file in
            js_ts_import_source_files_from_content(file, &content, &candidate_files)
        {
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

pub struct DiskCache {
    conn: Connection,
}

impl DiskCache {
    pub fn open(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let cache_dir = cache_dir_for_repo(repo_root)
            .ok_or_else(|| rusqlite::Error::InvalidPath(repo_root.to_path_buf()))?;
        create_cache_dir(&cache_dir)?;
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

        tx.execute_batch(
            "DELETE FROM files; DELETE FROM entities; DELETE FROM edges; DELETE FROM file_imports;",
        )?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for file in files {
                if is_manifest_file_name(file) {
                    continue;
                }
                let full = root.join(file);
                if let Some((secs, nanos, content_hash)) = file_fingerprint(&full) {
                    stmt.execute(params![file, secs, nanos, content_hash])?;
                }
            }
        }

        refresh_manifest_entries(&tx, root)?;
        refresh_file_import_entries(&tx, root, files, files)?;

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
            .prepare("SELECT mtime_secs, mtime_nanos, content_hash FROM files WHERE path = ?1")
            .ok()?;
        for file in files {
            if is_manifest_file_name(file) {
                continue;
            }
            let (secs, nanos, content_hash): (i64, i64, Option<String>) = stmt
                .query_row(params![file], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .ok()?;
            let full = root.join(file);
            match file_freshness(&full, secs, nanos, content_hash.as_deref())? {
                FileFreshness::Fresh => {}
                FileFreshness::FreshWithUpdatedFingerprint {
                    secs,
                    nanos,
                    content_hash,
                } => {
                    self.refresh_file_fingerprint(file, secs, nanos, &content_hash);
                }
                FileFreshness::Stale => return None,
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

    /// Load only graph topology from a fresh cache.
    pub fn load_graph_topology(&self, root: &Path, files: &[String]) -> Option<EntityGraph> {
        if is_manifest_stale(&self.conn, root) {
            return None;
        }

        let cached_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .ok()?;
        if (cached_count - manifest_entry_count(&self.conn)) as usize != source_file_count(files) {
            return None;
        }

        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime_secs, mtime_nanos, content_hash FROM files")
            .ok()?;
        let cached_mtimes: HashMap<String, (i64, i64, Option<String>)> = stmt
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

        for file in files {
            if is_manifest_file_name(file) {
                continue;
            }
            let (secs, nanos, content_hash) = cached_mtimes.get(file.as_str())?;
            let full = root.join(file);
            match file_freshness(&full, *secs, *nanos, content_hash.as_deref())? {
                FileFreshness::Fresh => {}
                FileFreshness::FreshWithUpdatedFingerprint {
                    secs,
                    nanos,
                    content_hash,
                } => {
                    self.refresh_file_fingerprint(file, secs, nanos, &content_hash);
                }
                FileFreshness::Stale => return None,
            }
        }

        let mut entity_stmt = self
            .conn
            .prepare(
                "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id FROM entities",
            )
            .ok()?;
        let entity_map: HashMap<String, EntityInfo> = entity_stmt
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

    fn refresh_file_fingerprint(&self, file: &str, secs: i64, nanos: i64, content_hash: &str) {
        let _ = self.conn.execute(
            "UPDATE files SET mtime_secs = ?2, mtime_nanos = ?3, content_hash = ?4 WHERE path = ?1",
            params![file, secs, nanos, content_hash],
        );
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
                            self.refresh_file_fingerprint(file, secs, nanos, &content_hash);
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
        let mut import_stale_files = stale_source_files.clone();
        import_stale_files.extend(deleted_cached_files.iter().cloned());
        let cached_importing_stale_files =
            cached_importing_files_for_stale_files(&self.conn, &import_stale_files, &source_files);

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
        )
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

        {
            let mut del_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for path in &deleted_cached_files {
                del_files.execute(params![path])?;
            }
        }

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
        } else {
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
                if !repair_changed_clean_entity_ids
                    && !source_stale_set.contains(e.file_path.as_str())
                {
                    continue;
                }

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

        tx.commit()?;
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

    fn empty_graph() -> EntityGraph {
        EntityGraph::from_parts(HashMap::new(), Vec::new())
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
            start_line: 1,
            end_line: 1,
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
        let entity_map = entities
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
        if let Some(cache_dir) = cache_dir_for_repo(&root) {
            let _ = std::fs::remove_dir_all(cache_dir);
        }
    }

    fn save_empty_cache(root: &Path, files: &[String]) -> DiskCache {
        let cache = DiskCache::open(root).unwrap();
        cache.save(root, files, &empty_graph(), &[]).unwrap();
        assert!(cache.load(root, files).is_some());
        cache
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
        cache.save(&root, &files, &empty_graph(), &[]).unwrap();

        assert_eq!(file_import_count(&cache, "a.ts", "b.ts"), 1);

        rewrite_after_mtime_tick(
            &root.join("b.ts"),
            "export function target() { return 3; }\n",
        );
        let partial = cache.load_partial(&root, &files).unwrap();
        assert_eq!(partial.stale_files, vec!["b.ts"]);
        assert_eq!(partial.cached_importing_stale_files, vec!["a.ts"]);

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
        let file = root.join("same.rs");
        write_file(&file, "fn same() {}\n");
        let files = vec!["same.rs".to_string()];
        let cache = save_empty_cache(&root, &files);
        let before = cached_file_mtime(&cache, "same.rs");

        rewrite_after_mtime_tick(&file, "fn same() {}\n");
        let current = file_mtime_parts(&file).unwrap();
        assert_ne!(before, current);

        assert!(cache.load_graph_topology(&root, &files).is_some());
        assert!(cache.load_partial(&root, &files).is_none());
        assert_eq!(cached_file_mtime(&cache, "same.rs"), current);

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
            .save(&root, &files, &initial_graph, &entities)
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
}
