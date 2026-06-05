use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, Connection};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};
use sem_mcp::cache as shared_cache;

/// Result of a partial cache load: stale files that need reparsing, plus cached clean data.
pub struct PartialCache {
    pub stale_files: Vec<String>,
    pub cached_entities: Vec<SemanticEntity>,
    pub cached_edges: Vec<EntityRef>,
    /// Cached entities from stale files (for entity-level content_hash comparison)
    pub stale_file_entities: Vec<SemanticEntity>,
}

pub struct DiskCache {
    conn: Connection,
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
                if shared_cache::is_manifest_file_name(file) {
                    continue;
                }
                let full = root.join(file);
                if let Some((secs, nanos)) = shared_cache::file_mtime_parts(&full) {
                    stmt.execute(params![file, secs, nanos])?;
                }
            }
        }

        shared_cache::refresh_manifest_entries(&tx, root)?;

        // Insert entities with prepared statement (already in a transaction, so fast)
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO entities (id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

        // Insert edges with prepared statement
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

    pub fn load(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<(EntityGraph, Vec<SemanticEntity>)> {
        if shared_cache::is_manifest_stale(&self.conn, root) {
            return None;
        }

        let cached_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .ok()?;
        if (cached_count - shared_cache::manifest_entry_count(&self.conn)) as usize
            != shared_cache::source_file_count(files)
        {
            return None;
        }

        // Load all cached mtimes in one query and validate against disk
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime_secs, mtime_nanos FROM files")
            .ok()?;
        let cached_mtimes: HashMap<String, (i64, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
                ))
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        for file in files {
            if shared_cache::is_manifest_file_name(file) {
                continue;
            }
            let (secs, nanos) = cached_mtimes.get(file.as_str()).copied()?;
            let full = root.join(file);
            let (current_secs, current_nanos) = shared_cache::file_mtime_parts(&full)?;
            if secs != current_secs || nanos != current_nanos {
                return None;
            }
        }

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
        if shared_cache::is_manifest_stale(&self.conn, root) {
            return None;
        }

        let cached_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .ok()?;
        if (cached_count - shared_cache::manifest_entry_count(&self.conn)) as usize
            != shared_cache::source_file_count(files)
        {
            return None;
        }

        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime_secs, mtime_nanos FROM files")
            .ok()?;
        let cached_mtimes: HashMap<String, (i64, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
                ))
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        for file in files {
            if shared_cache::is_manifest_file_name(file) {
                continue;
            }
            let (secs, nanos) = cached_mtimes.get(file.as_str()).copied()?;
            let full = root.join(file);
            let (current_secs, current_nanos) = shared_cache::file_mtime_parts(&full)?;
            if secs != current_secs || nanos != current_nanos {
                return None;
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
        if shared_cache::is_manifest_stale(&self.conn, root) {
            return None;
        }

        // Load all cached file paths + mtimes
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
            .filter(|file| !shared_cache::is_manifest_file_name(file))
            .collect();
        let source_file_count = source_files.len();
        let current_set: HashSet<&str> = source_files.iter().map(|file| file.as_str()).collect();

        // Find stale source files: mtime differs or not in cache
        let mut stale_source_files: Vec<String> = Vec::new();
        let mut stale_current_file_count = 0;
        for file in source_files {
            match cached_files.get(file) {
                Some(&(secs, nanos)) => {
                    let full = root.join(file);
                    let is_stale = shared_cache::file_mtime_parts(&full)
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
                "INSERT OR REPLACE INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)",
            )?;
            for file in &source_stale_files {
                let full = root.join(file);
                if let Some((secs, nanos)) = shared_cache::file_mtime_parts(&full) {
                    ins.execute(params![file, secs, nanos])?;
                }
            }
        }

        shared_cache::refresh_manifest_entries(&tx, root)?;

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
        cache.save(root, files, &empty_graph(), &[]).unwrap();
        assert!(cache.load(root, files).is_some());
        cache
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
            .save_incremental_with_repair_metadata(
                &root,
                &files,
                &["stale.rs".to_string()],
                &empty_graph(),
                &entities,
                false,
                &["stale-id".to_string()],
                &[],
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
            .save(&root, &files, &initial_graph, &entities)
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
            .save(&cli_to_mcp, &cli_to_mcp_files, &empty_graph(), &[])
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
            .save(&mcp_to_cli, &mcp_to_cli_files, &empty_graph(), &[])
            .unwrap();
        let cli_cache = DiskCache::open(&mcp_to_cli).unwrap();
        assert!(cli_cache.load(&mcp_to_cli, &mcp_to_cli_files).is_some());
        drop(cli_cache);
        drop(mcp_cache);
        cleanup(mcp_to_cli);
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
}
