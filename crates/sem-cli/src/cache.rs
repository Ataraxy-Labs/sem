use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, Connection};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};

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
        let cache_dir = repo_root.join(".sem");
        std::fs::create_dir_all(&cache_dir).ok();
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path)?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
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
             );",
        )?;

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
            let mut stmt = tx.prepare(
                "INSERT INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)",
            )?;
            for file in files {
                let full = root.join(file);
                if let Ok(meta) = std::fs::metadata(&full) {
                    if let Ok(mtime) = meta.modified() {
                        let dur = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        stmt.execute(params![file, dur.as_secs() as i64, dur.subsec_nanos() as i64])?;
                    }
                }
            }
        }

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

    pub fn load(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<(EntityGraph, Vec<SemanticEntity>)> {
        let cached_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .ok()?;
        if cached_count as usize != files.len() {
            return None;
        }

        let mut stmt = self
            .conn
            .prepare("SELECT mtime_secs, mtime_nanos FROM files WHERE path = ?1")
            .ok()?;
        for file in files {
            let full = root.join(file);
            let meta = std::fs::metadata(&full).ok()?;
            let mtime = meta.modified().ok()?;
            let dur = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();

            let (secs, nanos): (i64, i64) = stmt
                .query_row(params![file], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()?;
            if secs != dur.as_secs() as i64 || nanos != dur.subsec_nanos() as i64 {
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

    /// Load a partial cache: identify stale files and return clean cached data.
    /// Returns None if cache is empty or ALL files are stale (full rebuild is better).
    pub fn load_partial(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<PartialCache> {
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

        let current_set: HashSet<&str> = files.iter().map(|s| s.as_str()).collect();

        // Find stale files: mtime differs or not in cache
        let mut stale_files: Vec<String> = Vec::new();
        for file in files {
            match cached_files.get(file) {
                Some(&(secs, nanos)) => {
                    let full = root.join(file);
                    let meta = std::fs::metadata(&full).ok();
                    let is_stale = meta
                        .and_then(|m| m.modified().ok())
                        .map(|mtime| {
                            let dur = mtime
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default();
                            secs != dur.as_secs() as i64 || nanos != dur.subsec_nanos() as i64
                        })
                        .unwrap_or(true);
                    if is_stale {
                        stale_files.push(file.clone());
                    }
                }
                None => {
                    stale_files.push(file.clone());
                }
            }
        }

        // Files in cache but not on disk anymore count as stale/deleted
        for cached_path in cached_files.keys() {
            if !current_set.contains(cached_path.as_str()) {
                stale_files.push(cached_path.clone());
            }
        }

        // If nothing stale, full load would have worked
        if stale_files.is_empty() {
            return None;
        }

        // If everything is stale, skip incremental
        if stale_files.len() >= files.len() {
            return None;
        }

        let stale_set: HashSet<&str> = stale_files.iter().map(|s| s.as_str()).collect();

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

        // Only stale files that still exist on disk (filter out deleted)
        let stale_files: Vec<String> = stale_files
            .into_iter()
            .filter(|f| current_set.contains(f.as_str()))
            .collect();

        Some(PartialCache {
            stale_files,
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
        let stale_set: HashSet<&str> = stale_files.iter().map(|s| s.as_str()).collect();

        let tx = self.conn.unchecked_transaction()?;

        // Delete stale file entries
        {
            let mut del_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for f in stale_files {
                del_files.execute(params![f])?;
            }
        }

        // Delete files that are no longer in the file list (deleted from disk)
        {
            let current_set: HashSet<&str> = all_files.iter().map(|s| s.as_str()).collect();
            let mut cached_stmt = tx.prepare("SELECT path FROM files")?;
            let cached_paths: Vec<String> = cached_stmt
                .query_map([], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();
            let mut del_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for path in &cached_paths {
                if !current_set.contains(path.as_str()) {
                    del_files.execute(params![path])?;
                }
            }
        }

        // Insert new mtimes for stale files
        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)",
            )?;
            for file in stale_files {
                let full = root.join(file);
                if let Ok(meta) = std::fs::metadata(&full) {
                    if let Ok(mtime) = meta.modified() {
                        let dur = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        ins.execute(params![file, dur.as_secs() as i64, dur.subsec_nanos() as i64])?;
                    }
                }
            }
        }

        // Delete entities for stale files
        {
            let mut del = tx.prepare("DELETE FROM entities WHERE file_path = ?1")?;
            for f in stale_files {
                del.execute(params![f])?;
            }
        }

        // Insert new entities for stale files
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

        // Delete all edges and re-insert from graph
        // (Edges are complex to incrementally update since affected clean entities
        //  get re-resolved too. Simpler to just rewrite all edges.)
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
