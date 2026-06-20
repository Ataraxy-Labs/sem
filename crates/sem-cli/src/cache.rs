use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};
use sem_core::parser::{
    js_ts_has_default_re_export_from_content,
    js_ts_import_source_files_from_filesystem_with_unscoped,
};
use sem_core::utils::scan::is_default_excluded;
use sem_mcp::cache as shared_cache;
use serde::Serialize;

const CACHED_TEST_IMPACT_LIMIT: usize = 10_000;
const SQL_PARAM_CHUNK: usize = 500;

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

#[derive(Clone, Copy)]
pub enum CachedImpactMode {
    All,
    Deps,
    Dependents,
    Tests,
}

pub struct CachedImpactResult {
    pub entity: EntityInfo,
    pub dependencies: Vec<EntityInfo>,
    pub dependents: Vec<EntityInfo>,
    pub impact: Vec<(EntityInfo, usize)>,
    pub tests: Vec<EntityInfo>,
    pub tests_truncated: bool,
}

#[derive(Serialize)]
struct EntityListingJsonRow<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    entity_type: &'a str,
    start_line: usize,
    end_line: usize,
    parent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a str>,
}

#[derive(Debug)]
pub enum CachedImpactError {
    CacheReadFailed,
    MissingEntityQuery,
    EntityIdNotFound(String),
    EntityNotFound(String),
    EntityNotFoundInFile {
        name: String,
        file: String,
    },
    AmbiguousEntity {
        name: String,
        matches: Vec<EntityInfo>,
    },
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

    pub fn open_existing_readonly(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let db_path = shared_cache::cache_db_path(repo_root)
            .ok_or_else(|| rusqlite::Error::InvalidPath(repo_root.to_path_buf()))?;
        if !db_path.exists() {
            return Err(rusqlite::Error::InvalidPath(db_path));
        }

        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { conn })
    }

    pub fn save(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch(
            "DELETE FROM files; DELETE FROM entities; DELETE FROM edges; DELETE FROM file_imports; DELETE FROM entity_flags;",
        )?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for file in files {
                if shared_cache::is_manifest_file_name(file) {
                    continue;
                }
                let full = root.join(file);
                if let Some((secs, nanos, content_hash)) = shared_cache::file_fingerprint(&full) {
                    stmt.execute(params![file, secs, nanos, content_hash])?;
                }
            }
        }

        shared_cache::refresh_manifest_entries(&tx, root)?;
        shared_cache::refresh_file_import_entries(&tx, root, files, files)?;

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

        shared_cache::set_cache_kind(&tx, shared_cache::CACHE_KIND_FULL)?;
        shared_cache::set_cache_source_scope(&tx, source_scope)?;
        tx.commit()?;
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

        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (path, mtime_secs, mtime_nanos, content_hash) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for file in files {
                if shared_cache::is_manifest_file_name(file) {
                    continue;
                }
                let full = root.join(file);
                if let Some((secs, nanos, content_hash)) = shared_cache::file_fingerprint(&full) {
                    stmt.execute(params![file, secs, nanos, content_hash])?;
                }
            }
        }

        shared_cache::refresh_manifest_entries(&tx, root)?;
        shared_cache::refresh_file_import_entries(&tx, root, files, files)?;

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

        let test_entity_ids =
            graph.filter_test_entities_with_custom_dirs(entities, custom_test_dirs);
        {
            let mut stmt =
                tx.prepare("INSERT INTO entity_flags (entity_id, is_test) VALUES (?1, 1)")?;
            for entity_id in &test_entity_ids {
                stmt.execute(params![entity_id])?;
            }
        }

        shared_cache::set_cache_kind(&tx, shared_cache::CACHE_KIND_TOPOLOGY)?;
        shared_cache::set_cache_source_scope(&tx, source_scope)?;
        tx.commit()?;
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
    pub fn query_impact_topology(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
        cache_first: bool,
        entity_name: Option<&str>,
        entity_id: Option<&str>,
        file_hint: Option<&str>,
        mode: CachedImpactMode,
        depth: usize,
    ) -> Result<Option<CachedImpactResult>, CachedImpactError> {
        if !shared_cache::cache_has_kind(
            &self.conn,
            &[
                shared_cache::CACHE_KIND_FULL,
                shared_cache::CACHE_KIND_TOPOLOGY,
            ],
        ) {
            return Ok(None);
        }

        if matches!(mode, CachedImpactMode::All | CachedImpactMode::Tests)
            && !shared_cache::cache_has_kind(&self.conn, &[shared_cache::CACHE_KIND_TOPOLOGY])
        {
            return Ok(None);
        }

        if matches!(mode, CachedImpactMode::Deps) {
            return self.query_dependency_impact_topology(
                root,
                files,
                source_scope,
                cache_first,
                entity_name,
                entity_id,
                file_hint,
            );
        }

        if !self.has_fresh_cache(root, files, source_scope) {
            return Ok(None);
        }

        self.query_fresh_impact_topology(entity_name, entity_id, file_hint, mode, depth)
    }

    fn query_fresh_impact_topology(
        &self,
        entity_name: Option<&str>,
        entity_id: Option<&str>,
        file_hint: Option<&str>,
        mode: CachedImpactMode,
        depth: usize,
    ) -> Result<Option<CachedImpactResult>, CachedImpactError> {
        let entity = self.find_cached_impact_entity(entity_name, entity_id, file_hint)?;
        let dependencies = if matches!(mode, CachedImpactMode::All | CachedImpactMode::Deps) {
            match self.direct_dependencies(&entity.id) {
                Ok(dependencies) => dependencies,
                Err(_) => return Err(CachedImpactError::CacheReadFailed),
            }
        } else {
            Vec::new()
        };
        let impact = if matches!(mode, CachedImpactMode::All) {
            match self.impact_entities(&entity.id, depth, None) {
                Ok(impact) => impact,
                Err(_) => return Err(CachedImpactError::CacheReadFailed),
            }
        } else {
            Vec::new()
        };
        let dependents = if matches!(mode, CachedImpactMode::All) {
            impact
                .iter()
                .filter(|(_, depth)| *depth == 1)
                .map(|(entity, _)| entity.clone())
                .collect()
        } else if matches!(mode, CachedImpactMode::Dependents) {
            match self.direct_dependents(&entity.id) {
                Ok(dependents) => dependents,
                Err(_) => return Err(CachedImpactError::CacheReadFailed),
            }
        } else {
            Vec::new()
        };
        let (tests, tests_truncated) =
            if matches!(mode, CachedImpactMode::All | CachedImpactMode::Tests) {
                match self.test_impact_entities(&entity.id) {
                    Ok(tests) => tests,
                    Err(_) => return Err(CachedImpactError::CacheReadFailed),
                }
            } else {
                (Vec::new(), false)
            };

        Ok(Some(CachedImpactResult {
            entity,
            dependencies,
            dependents,
            impact,
            tests,
            tests_truncated,
        }))
    }

    fn query_dependency_impact_topology(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
        cache_first: bool,
        entity_name: Option<&str>,
        entity_id: Option<&str>,
        file_hint: Option<&str>,
    ) -> Result<Option<CachedImpactResult>, CachedImpactError> {
        if shared_cache::is_manifest_stale(&self.conn, root) {
            return Ok(None);
        }

        if cache_first {
            if !shared_cache::cache_has_default_source_scope(&self.conn) {
                return Ok(None);
            }
        } else if !self.has_fresh_cache(root, files, source_scope) {
            return Ok(None);
        }

        let entity = match self.find_cached_impact_entity(entity_name, entity_id, file_hint) {
            Ok(entity) => entity,
            Err(CachedImpactError::CacheReadFailed) => {
                return Err(CachedImpactError::CacheReadFailed);
            }
            Err(_) => return Ok(None),
        };
        let dependencies = self
            .direct_dependencies(&entity.id)
            .map_err(|_| CachedImpactError::CacheReadFailed)?;

        if cache_first && !self.has_fresh_dependency_impact_files(root, &entity, &dependencies)? {
            return Ok(None);
        }

        Ok(Some(CachedImpactResult {
            entity,
            dependencies,
            dependents: Vec::new(),
            impact: Vec::new(),
            tests: Vec::new(),
            tests_truncated: false,
        }))
    }

    fn has_fresh_dependency_impact_files(
        &self,
        root: &Path,
        entity: &EntityInfo,
        dependencies: &[EntityInfo],
    ) -> Result<bool, CachedImpactError> {
        if !self
            .cached_files_are_fresh(root, HashSet::from([entity.file_path.clone()]))
            .map_err(|_| CachedImpactError::CacheReadFailed)?
        {
            return Ok(false);
        }

        let mut required_files = HashSet::new();
        for dependency in dependencies {
            required_files.insert(dependency.file_path.clone());
        }
        let cached_imported_files = self
            .cached_imported_files(&entity.file_path)
            .map_err(|_| CachedImpactError::CacheReadFailed)?;
        let Some(current_imports) = current_imported_files(root, &entity.file_path)? else {
            return Ok(false);
        };
        if cached_imported_files != current_imports.files {
            return Ok(false);
        }
        if current_imports.has_default_re_export {
            return Ok(false);
        }
        for imported_file in &cached_imported_files {
            if file_has_default_re_export(root, imported_file)? {
                return Ok(false);
            }
            required_files.insert(imported_file.clone());
        }

        self.cached_files_are_fresh(root, required_files)
            .map_err(|_| CachedImpactError::CacheReadFailed)
    }

    fn cached_imported_files(&self, file_path: &str) -> Result<HashSet<String>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT imported_file FROM file_imports
             WHERE importing_file = ?1
             ORDER BY imported_file",
        )?;
        let rows = stmt.query_map(params![file_path], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    fn cached_files_are_fresh(
        &self,
        root: &Path,
        files: HashSet<String>,
    ) -> Result<bool, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT mtime_secs, mtime_nanos, content_hash
             FROM files WHERE path = ?1",
        )?;
        let mut fingerprint_refreshes = Vec::new();

        for file in files {
            let cached: Option<(i64, i64, Option<String>)> = stmt
                .query_row(params![file], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .optional()?;
            let Some((secs, nanos, content_hash)) = cached else {
                return Ok(false);
            };
            match shared_cache::file_freshness(
                &root.join(&file),
                secs,
                nanos,
                content_hash.as_deref(),
            ) {
                Some(shared_cache::FileFreshness::Fresh) => {}
                Some(shared_cache::FileFreshness::FreshWithUpdatedFingerprint {
                    secs,
                    nanos,
                    content_hash,
                }) => {
                    fingerprint_refreshes.push(shared_cache::FileFingerprintRefresh {
                        path: file,
                        mtime_secs: secs,
                        mtime_nanos: nanos,
                        content_hash,
                    });
                }
                Some(shared_cache::FileFreshness::Stale) | None => return Ok(false),
            }
        }

        shared_cache::refresh_file_fingerprints_best_effort(&self.conn, &fingerprint_refreshes);
        Ok(true)
    }

    fn load_graph_topology_rows(&self) -> Option<EntityGraph> {
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

    fn find_cached_impact_entity(
        &self,
        entity_name: Option<&str>,
        entity_id: Option<&str>,
        file_hint: Option<&str>,
    ) -> Result<EntityInfo, CachedImpactError> {
        if let Some(id) = entity_id {
            return self
                .entity_by_id(id)
                .map_err(|_| CachedImpactError::CacheReadFailed)?
                .ok_or_else(|| CachedImpactError::EntityIdNotFound(id.to_string()));
        }

        let name = entity_name.ok_or(CachedImpactError::MissingEntityQuery)?;
        let mut matching = self
            .entity_candidates_for_query(name, file_hint)
            .map_err(|_| CachedImpactError::CacheReadFailed)?;

        if matching.is_empty() {
            if let Some(file) = file_hint {
                let global_matches = self
                    .entity_candidates_for_query(name, None)
                    .map_err(|_| CachedImpactError::CacheReadFailed)?;
                if global_matches.is_empty() {
                    return Err(CachedImpactError::EntityNotFound(name.to_string()));
                }
                return Err(CachedImpactError::EntityNotFoundInFile {
                    name: name.to_string(),
                    file: file.to_string(),
                });
            }
            return Err(CachedImpactError::EntityNotFound(name.to_string()));
        }

        if matching.len() == 1 {
            return Ok(matching.into_iter().next().unwrap());
        }

        matching.sort_by_key(|entity| {
            (
                entity.file_path.clone(),
                entity.start_line,
                entity.id.clone(),
            )
        });
        Err(CachedImpactError::AmbiguousEntity {
            name: name.to_string(),
            matches: matching,
        })
    }

    fn entity_by_id(&self, id: &str) -> Result<Option<EntityInfo>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id
                 FROM entities WHERE id = ?1",
                params![id],
                entity_info_from_row,
            )
            .optional()
    }

    fn entity_candidates_for_query(
        &self,
        query: &str,
        file_hint: Option<&str>,
    ) -> Result<Vec<EntityInfo>, rusqlite::Error> {
        let mut by_id = HashMap::<String, EntityInfo>::new();

        if let Some(file_hint) = file_hint {
            self.add_entity_candidates(
                "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id
                 FROM entities WHERE name = ?1 AND file_path = ?2",
                &[query, file_hint],
                &mut by_id,
            )?;
        } else {
            self.add_entity_candidates(
                "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id
                 FROM entities WHERE name = ?1",
                &[query],
                &mut by_id,
            )?;
        }

        if let Some((entity_type, name)) = split_type_qualified_query(query) {
            if let Some(file_hint) = file_hint {
                self.add_entity_candidates(
                    "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id
                     FROM entities
                     WHERE entity_type = ?1 AND name = ?2 AND file_path = ?3",
                    &[entity_type, name, file_hint],
                    &mut by_id,
                )?;
            } else {
                self.add_entity_candidates(
                    "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id
                     FROM entities WHERE entity_type = ?1 AND name = ?2",
                    &[entity_type, name],
                    &mut by_id,
                )?;
            }
        }

        if let Some((parent_name, child_name)) = query.rsplit_once('.') {
            if let Some(file_hint) = file_hint {
                self.add_entity_candidates(
                    "SELECT child.id, child.name, child.entity_type, child.file_path,
                            child.start_line, child.end_line, child.parent_id
                     FROM entities child
                     JOIN entities parent ON child.parent_id = parent.id
                     WHERE child.name = ?1 AND parent.name = ?2 AND child.file_path = ?3",
                    &[child_name, parent_name, file_hint],
                    &mut by_id,
                )?;
            } else {
                self.add_entity_candidates(
                    "SELECT child.id, child.name, child.entity_type, child.file_path,
                            child.start_line, child.end_line, child.parent_id
                     FROM entities child
                     JOIN entities parent ON child.parent_id = parent.id
                     WHERE child.name = ?1 AND parent.name = ?2",
                    &[child_name, parent_name],
                    &mut by_id,
                )?;
            }
        }

        let mut candidates: Vec<_> = by_id.into_values().collect();
        candidates.sort_by_key(|entity| {
            (
                entity.file_path.clone(),
                entity.start_line,
                entity.id.clone(),
            )
        });
        Ok(candidates)
    }

    fn add_entity_candidates(
        &self,
        sql: &str,
        args: &[&str],
        by_id: &mut HashMap<String, EntityInfo>,
    ) -> Result<(), rusqlite::Error> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter().copied()), entity_info_from_row)?;
        for row in rows {
            let entity = row?;
            by_id.insert(entity.id.clone(), entity);
        }
        Ok(())
    }

    fn direct_dependencies(&self, entity_id: &str) -> Result<Vec<EntityInfo>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT entities.id, entities.name, entities.entity_type, entities.file_path,
                    entities.start_line, entities.end_line, entities.parent_id
             FROM edges
             JOIN entities ON entities.id = edges.to_entity
             WHERE edges.from_entity = ?1
             ORDER BY edges.to_entity, edges.ref_type",
        )?;
        let rows = stmt.query_map(params![entity_id], entity_info_from_row)?;
        rows.collect()
    }

    fn direct_dependents(&self, entity_id: &str) -> Result<Vec<EntityInfo>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT entities.id, entities.name, entities.entity_type, entities.file_path,
                    entities.start_line, entities.end_line, entities.parent_id
             FROM edges
             JOIN entities ON entities.id = edges.from_entity
             WHERE edges.to_entity = ?1
             ORDER BY edges.from_entity, edges.ref_type",
        )?;
        let rows = stmt.query_map(params![entity_id], entity_info_from_row)?;
        rows.collect()
    }

    fn impact_entities(
        &self,
        entity_id: &str,
        max_depth: usize,
        max_count: Option<usize>,
    ) -> Result<Vec<(EntityInfo, usize)>, rusqlite::Error> {
        let impact_ids = self.impact_ids(entity_id, max_depth, max_count)?;
        let ids: Vec<String> = impact_ids.iter().map(|(id, _)| id.clone()).collect();
        let infos = self.entity_infos_by_id(&ids)?;
        Ok(impact_ids
            .into_iter()
            .filter_map(|(id, depth)| infos.get(&id).cloned().map(|info| (info, depth)))
            .collect())
    }

    fn test_impact_entities(
        &self,
        entity_id: &str,
    ) -> Result<(Vec<EntityInfo>, bool), rusqlite::Error> {
        let mut impact_ids = self.impact_ids(entity_id, 0, Some(CACHED_TEST_IMPACT_LIMIT + 1))?;
        let tests_truncated = impact_ids.len() > CACHED_TEST_IMPACT_LIMIT;
        if tests_truncated {
            impact_ids.truncate(CACHED_TEST_IMPACT_LIMIT);
        }
        let ids: Vec<String> = impact_ids.into_iter().map(|(id, _)| id).collect();
        let test_ids = self.test_ids_from(&ids)?;
        let ordered_test_ids: Vec<String> = ids
            .iter()
            .filter(|id| test_ids.contains(*id))
            .cloned()
            .collect();
        let infos = self.entity_infos_by_id(&ordered_test_ids)?;
        let tests = ordered_test_ids
            .into_iter()
            .filter_map(|id| infos.get(&id).cloned())
            .collect();
        Ok((tests, tests_truncated))
    }

    fn impact_ids(
        &self,
        entity_id: &str,
        max_depth: usize,
        max_count: Option<usize>,
    ) -> Result<Vec<(String, usize)>, rusqlite::Error> {
        let mut visited = HashSet::new();
        let mut frontier = vec![entity_id.to_string()];
        let mut result = Vec::new();
        let mut depth = 0;
        visited.insert(entity_id.to_string());

        while !frontier.is_empty() {
            if max_depth > 0 && depth >= max_depth {
                break;
            }
            let next_depth = depth + 1;
            let mut next_frontier = Vec::new();
            for dependent_id in self.dependent_ids_for(&frontier)? {
                if visited.insert(dependent_id.clone()) {
                    result.push((dependent_id.clone(), next_depth));
                    next_frontier.push(dependent_id);
                    if max_count.is_some_and(|limit| result.len() >= limit) {
                        return Ok(result);
                    }
                }
            }
            frontier = next_frontier;
            depth = next_depth;
        }

        Ok(result)
    }

    fn dependent_ids_for(&self, entity_ids: &[String]) -> Result<Vec<String>, rusqlite::Error> {
        let mut dependents = Vec::new();
        for chunk in entity_ids.chunks(SQL_PARAM_CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = repeat_vars(chunk.len());
            let sql = format!(
                "SELECT to_entity, from_entity FROM edges WHERE to_entity IN ({placeholders})
                 ORDER BY to_entity, from_entity, ref_type"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(chunk.iter().map(String::as_str)), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
            let mut by_target = HashMap::<String, Vec<String>>::new();
            for row in rows {
                let (target, dependent) = row?;
                by_target.entry(target).or_default().push(dependent);
            }
            for entity_id in chunk {
                if let Some(ids) = by_target.remove(entity_id) {
                    dependents.extend(ids);
                }
            }
        }
        Ok(dependents)
    }

    fn entity_infos_by_id(
        &self,
        entity_ids: &[String],
    ) -> Result<HashMap<String, EntityInfo>, rusqlite::Error> {
        let mut infos = HashMap::new();
        for chunk in entity_ids.chunks(SQL_PARAM_CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = repeat_vars(chunk.len());
            let sql = format!(
                "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id
                 FROM entities WHERE id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params_from_iter(chunk.iter().map(String::as_str)),
                entity_info_from_row,
            )?;
            for row in rows {
                let entity = row?;
                infos.insert(entity.id.clone(), entity);
            }
        }
        Ok(infos)
    }

    fn test_ids_from(&self, entity_ids: &[String]) -> Result<HashSet<String>, rusqlite::Error> {
        let mut ids = HashSet::new();
        for chunk in entity_ids.chunks(SQL_PARAM_CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = repeat_vars(chunk.len());
            let sql = format!(
                "SELECT entity_id FROM entity_flags
                 WHERE is_test != 0 AND entity_id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(chunk.iter().map(String::as_str)), |row| {
                    row.get::<_, String>(0)
                })?;
            for row in rows {
                ids.insert(row?);
            }
        }
        Ok(ids)
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

    pub fn write_graph_json_topology<W: Write>(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
        mut writer: W,
    ) -> std::io::Result<bool> {
        if !self.has_fresh_topology_cache(root, files, source_scope) {
            return Ok(false);
        }

        let entity_count: i64 =
            match self
                .conn
                .query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))
            {
                Ok(count) => count,
                Err(_) => return Ok(false),
            };
        let edge_count: i64 = match self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
        {
            Ok(count) => count,
            Err(_) => return Ok(false),
        };

        let mut entity_stmt = match self.conn.prepare(
            "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id FROM entities ORDER BY id",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Ok(false),
        };
        let mut edge_stmt = match self.conn.prepare(
            "SELECT from_entity, to_entity, ref_type FROM edges ORDER BY from_entity, to_entity, CASE ref_type WHEN 'calls' THEN 0 WHEN 'imports' THEN 1 ELSE 2 END",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Ok(false),
        };

        writer.write_all(b"{\"entities\":[")?;
        let mut entity_rows = entity_stmt.query([]).map_err(sql_io_error)?;
        let mut first = true;
        while let Some(row) = entity_rows.next().map_err(sql_io_error)? {
            if first {
                first = false;
            } else {
                writer.write_all(b",")?;
            }
            let entity = EntityInfo {
                id: row.get(0).map_err(sql_io_error)?,
                name: row.get(1).map_err(sql_io_error)?,
                entity_type: row.get(2).map_err(sql_io_error)?,
                file_path: row.get(3).map_err(sql_io_error)?,
                start_line: row.get::<_, i64>(4).map_err(sql_io_error)? as usize,
                end_line: row.get::<_, i64>(5).map_err(sql_io_error)? as usize,
                parent_id: row.get(6).map_err(sql_io_error)?,
            };
            serde_json::to_writer(&mut writer, &entity).map_err(json_io_error)?;
        }

        writer.write_all(b"],\"edges\":[")?;
        let mut edge_rows = edge_stmt.query([]).map_err(sql_io_error)?;
        let mut first = true;
        while let Some(row) = edge_rows.next().map_err(sql_io_error)? {
            if first {
                first = false;
            } else {
                writer.write_all(b",")?;
            }
            let rt: String = row.get(2).map_err(sql_io_error)?;
            let ref_type = match rt.as_str() {
                "calls" => RefType::Calls,
                "imports" => RefType::Imports,
                _ => RefType::TypeRef,
            };
            let edge = EntityRef {
                from_entity: row.get(0).map_err(sql_io_error)?,
                to_entity: row.get(1).map_err(sql_io_error)?,
                ref_type,
            };
            serde_json::to_writer(&mut writer, &edge).map_err(json_io_error)?;
        }

        write!(
            writer,
            "],\"stats\":{{\"entityCount\":{},\"edgeCount\":{}}}}}\n",
            entity_count, edge_count
        )?;
        Ok(true)
    }

    pub fn query_entities_listing(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
    ) -> Result<Option<Vec<EntityInfo>>, rusqlite::Error> {
        if !self.has_fresh_topology_cache_for_files(root, files, source_scope) {
            return Ok(None);
        }

        if files.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let mut entities = Vec::new();
        for chunk in files.chunks(SQL_PARAM_CHUNK) {
            let placeholders = repeat_vars(chunk.len());
            let sql = format!(
                "SELECT id, name, entity_type, file_path, start_line, end_line, parent_id
                 FROM entities
                 WHERE file_path IN ({placeholders})
                 ORDER BY file_path, start_line, end_line, entity_type, name"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params_from_iter(chunk.iter().map(String::as_str)),
                entity_info_from_row,
            )?;
            entities.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        sort_entity_infos(&mut entities);
        Ok(Some(entities))
    }

    fn has_fresh_topology_cache_for_files(
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

        if !shared_cache::cache_has_source_scope(&self.conn, source_scope) {
            return false;
        }

        if shared_cache::is_manifest_stale(&self.conn, root) {
            return false;
        }

        self.cached_files_are_fresh(
            root,
            files
                .iter()
                .filter(|file| !shared_cache::is_manifest_file_name(file))
                .cloned()
                .collect(),
        )
        .unwrap_or(false)
    }

    pub fn write_entities_listing_json<W: Write>(
        &self,
        root: &Path,
        files: &[String],
        source_scope: shared_cache::CacheSourceScope,
        include_file: bool,
        writer: &mut W,
    ) -> std::io::Result<Option<u64>> {
        if !self.has_fresh_topology_cache_for_files(root, files, source_scope) {
            return Ok(None);
        }

        writer.write_all(b"[")?;
        let mut first = true;
        let mut count = 0u64;
        for chunk in files.chunks(SQL_PARAM_CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = repeat_vars(chunk.len());
            let sql = format!(
                "SELECT name, entity_type, file_path, start_line, end_line, parent_id
                 FROM entities
                 WHERE file_path IN ({placeholders})
                 ORDER BY file_path, start_line, end_line, entity_type, name"
            );
            let mut stmt = self.conn.prepare(&sql).map_err(sql_io_error)?;
            let mut rows = stmt
                .query(params_from_iter(chunk.iter().map(String::as_str)))
                .map_err(sql_io_error)?;
            while let Some(row) = rows.next().map_err(sql_io_error)? {
                if first {
                    first = false;
                } else {
                    writer.write_all(b",")?;
                }

                let name: String = row.get(0).map_err(sql_io_error)?;
                let entity_type: String = row.get(1).map_err(sql_io_error)?;
                let file_path: String = row.get(2).map_err(sql_io_error)?;
                let start_line = row.get::<_, i64>(3).map_err(sql_io_error)? as usize;
                let end_line = row.get::<_, i64>(4).map_err(sql_io_error)? as usize;
                let parent_id: Option<String> = row.get(5).map_err(sql_io_error)?;
                let listing = EntityListingJsonRow {
                    name: &name,
                    entity_type: &entity_type,
                    start_line,
                    end_line,
                    parent_id: parent_id.as_deref(),
                    file: include_file.then_some(file_path.as_str()),
                };
                serde_json::to_writer(&mut *writer, &listing).map_err(json_io_error)?;
                count += 1;
            }
        }
        writer.write_all(b"]\n")?;

        Ok(Some(count))
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
            .prepare("SELECT id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json FROM entities")
            .ok()?;
        let mut cached_entities = Vec::new();
        let mut stale_file_entities = Vec::new();
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
                content: row.get(6).ok()?,
                content_hash: row.get(7).ok()?,
                structural_hash: row.get(8).ok()?,
                parent_id: row.get(9).ok()?,
                metadata: metadata_json.and_then(|j| serde_json::from_str(&j).ok()),
            };
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

        shared_cache::set_cache_kind(&tx, shared_cache::CACHE_KIND_FULL)?;
        shared_cache::set_cache_source_scope(&tx, source_scope)?;
        tx.commit()?;
        Ok(())
    }
}

fn entity_info_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntityInfo> {
    Ok(EntityInfo {
        id: row.get(0)?,
        name: row.get(1)?,
        entity_type: row.get(2)?,
        file_path: row.get(3)?,
        start_line: row.get::<_, i64>(4)? as usize,
        end_line: row.get::<_, i64>(5)? as usize,
        parent_id: row.get(6)?,
    })
}

fn sort_entity_infos(entities: &mut [EntityInfo]) {
    entities.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.entity_type.cmp(&b.entity_type))
            .then(a.name.cmp(&b.name))
    });
}

fn repeat_vars(len: usize) -> String {
    std::iter::repeat("?")
        .take(len)
        .collect::<Vec<_>>()
        .join(",")
}

fn split_type_qualified_query(query: &str) -> Option<(&str, &str)> {
    let (entity_type, name) = query.split_once(' ')?;
    if entity_type.is_empty() || name.is_empty() {
        return None;
    }
    Some((entity_type, name))
}

struct CurrentImports {
    files: HashSet<String>,
    has_default_re_export: bool,
}

fn current_imported_files(
    root: &Path,
    file_path: &str,
) -> Result<Option<CurrentImports>, CachedImpactError> {
    let content = std::fs::read_to_string(root.join(file_path))
        .map_err(|_| CachedImpactError::CacheReadFailed)?;
    let (files, has_unscoped_imports) =
        js_ts_import_source_files_from_filesystem_with_unscoped(root, file_path, &content);
    if has_unscoped_imports || !is_js_ts_cache_freshness_supported(file_path) {
        return Ok(None);
    }
    let files = files
        .into_iter()
        .filter(|file| !is_default_excluded(file))
        .collect();
    Ok(Some(CurrentImports {
        files,
        has_default_re_export: js_ts_has_default_re_export_from_content(&content),
    }))
}

fn file_has_default_re_export(root: &Path, file_path: &str) -> Result<bool, CachedImpactError> {
    let content = std::fs::read_to_string(root.join(file_path))
        .map_err(|_| CachedImpactError::CacheReadFailed)?;
    Ok(js_ts_has_default_re_export_from_content(&content))
}

fn is_js_ts_cache_freshness_supported(file_path: &str) -> bool {
    [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .any(|extension| file_path.ends_with(extension))
}

fn sql_io_error(error: rusqlite::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, error)
}

fn json_io_error(error: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, error)
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
        let entity_map: HashMap<String, EntityInfo> = entities
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
    fn write_graph_json_topology_streams_fresh_cache() {
        let root = temp_repo_root("graph-json-topology");
        write_file(&root.join("a.rs"), "fn a() {}\n");
        write_file(&root.join("b.rs"), "fn b() { a(); }\n");
        let files = vec!["b.rs".to_string(), "a.rs".to_string()];
        let entities = vec![
            entity("b-id", "b.rs", "b", "fn b() { a(); }"),
            entity("a-id", "a.rs", "a", "fn a() {}"),
        ];
        let graph = graph_with_edges(&entities, vec![edge("b-id", "a-id")]);
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

        let mut output = Vec::new();
        assert!(cache
            .write_graph_json_topology(
                &root,
                &files,
                shared_cache::CacheSourceScope::Default,
                &mut output
            )
            .unwrap());
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();

        assert_eq!(
            value["stats"],
            serde_json::json!({"entityCount": 2, "edgeCount": 1})
        );
        assert_eq!(value["entities"][0]["id"], "a-id");
        assert_eq!(value["entities"][1]["id"], "b-id");
        assert_eq!(value["edges"][0]["fromEntity"], "b-id");
        assert_eq!(value["edges"][0]["toEntity"], "a-id");

        drop(cache);
        cleanup(root);
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

        let mut output = Vec::new();
        assert!(cache
            .write_graph_json_topology(
                &root,
                &files,
                shared_cache::CacheSourceScope::Default,
                &mut output
            )
            .unwrap());
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            value["stats"],
            serde_json::json!({"entityCount": 3, "edgeCount": 2})
        );

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
    fn cache_first_dependency_impact_ignores_default_excluded_imports() {
        let root = temp_repo_root("impact-ignores-excluded-imports");
        std::fs::create_dir_all(root.join("src/generated")).unwrap();
        write_file(
            &root.join("src/a.ts"),
            "import { generated } from './generated/client';\nexport function target() { return generated(); }\n",
        );
        write_file(
            &root.join("src/generated/client.ts"),
            "export function generated() { return 1; }\n",
        );
        let files = vec!["src/a.ts".to_string()];
        let entities = vec![entity(
            "a-id",
            "src/a.ts",
            "target",
            "export function target() { return generated(); }",
        )];
        let graph = graph_with_edges(&entities, vec![]);
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

        let result = cache
            .query_impact_topology(
                &root,
                &[],
                shared_cache::CacheSourceScope::Default,
                true,
                Some("target"),
                None,
                Some("src/a.ts"),
                CachedImpactMode::Deps,
                2,
            )
            .unwrap();

        assert!(
            result.is_some(),
            "default-scoped cache-first deps should ignore imports outside the default source set"
        );

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn query_impact_topology_reads_cached_adjacency_without_graph_load() {
        let root = temp_repo_root("impact-topology-query");
        write_file(&root.join("a.rs"), "fn target() {}\n");
        write_file(&root.join("b.rs"), "fn direct() { target(); }\n");
        write_file(&root.join("c.rs"), "fn transitive() { direct(); }\n");
        write_file(
            &root.join("a_test.rs"),
            "#[test]\nfn target_test() { target(); }\n",
        );
        let files = vec![
            "a.rs".to_string(),
            "b.rs".to_string(),
            "c.rs".to_string(),
            "a_test.rs".to_string(),
        ];
        let entities = vec![
            entity("a-id", "a.rs", "target", "fn target() {}"),
            entity("b-id", "b.rs", "direct", "fn direct() { target(); }"),
            entity(
                "c-id",
                "c.rs",
                "transitive",
                "fn transitive() { direct(); }",
            ),
            entity(
                "test-id",
                "a_test.rs",
                "target_test",
                "#[test]\nfn target_test() { target(); }",
            ),
        ];
        let graph = graph_with_edges(
            &entities,
            vec![
                edge("b-id", "a-id"),
                edge("c-id", "b-id"),
                edge("test-id", "a-id"),
            ],
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

        let result = cache
            .query_impact_topology(
                &root,
                &files,
                shared_cache::CacheSourceScope::Default,
                false,
                Some("target"),
                None,
                Some("a.rs"),
                CachedImpactMode::All,
                2,
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.entity.id, "a-id");
        assert!(result.dependencies.is_empty());
        assert_eq!(
            result
                .dependents
                .iter()
                .map(|entity| entity.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b-id", "test-id"]
        );
        assert_eq!(
            result
                .impact
                .iter()
                .map(|(entity, depth)| (entity.id.as_str(), *depth))
                .collect::<Vec<_>>(),
            vec![("b-id", 1), ("test-id", 1), ("c-id", 2)]
        );
        assert_eq!(
            result
                .tests
                .iter()
                .map(|entity| entity.id.as_str())
                .collect::<Vec<_>>(),
            vec!["test-id"]
        );

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn cached_test_impact_reports_truncated_traversal() {
        let root = temp_repo_root("test-impact-truncated");
        let cache = DiskCache::open(&root).unwrap();
        let tx = cache.conn.unchecked_transaction().unwrap();

        tx.execute(
            "INSERT INTO entities
             (id, name, entity_type, file_path, start_line, end_line, content, content_hash)
             VALUES (?1, ?2, 'function', ?3, 1, 1, '', '')",
            rusqlite::params!["root-id", "root", "root.rs"],
        )
        .unwrap();

        {
            let mut entity_stmt = tx
                .prepare(
                    "INSERT INTO entities
                     (id, name, entity_type, file_path, start_line, end_line, content, content_hash)
                     VALUES (?1, ?2, 'function', ?3, 1, 1, '', '')",
                )
                .unwrap();
            let mut edge_stmt = tx
                .prepare(
                    "INSERT INTO edges (from_entity, to_entity, ref_type)
                     VALUES (?1, 'root-id', 'calls')",
                )
                .unwrap();
            let mut test_stmt = tx
                .prepare("INSERT INTO entity_flags (entity_id, is_test) VALUES (?1, 1)")
                .unwrap();

            for index in 0..=CACHED_TEST_IMPACT_LIMIT {
                let id = format!("test-{index:05}");
                entity_stmt
                    .execute(rusqlite::params![&id, &id, format!("{id}.rs")])
                    .unwrap();
                edge_stmt.execute(rusqlite::params![&id]).unwrap();
                test_stmt.execute(rusqlite::params![&id]).unwrap();
            }
        }

        tx.commit().unwrap();

        let (tests, truncated) = cache.test_impact_entities("root-id").unwrap();
        assert!(truncated);
        assert_eq!(tests.len(), CACHED_TEST_IMPACT_LIMIT);
        assert!(tests.iter().any(|entity| entity.id == "test-00000"));
        assert!(!tests.iter().any(|entity| entity.id == "test-10000"));

        drop(cache);
        cleanup(root);
    }

    #[test]
    fn query_impact_topology_preserves_bfs_frontier_order() {
        let root = temp_repo_root("impact-topology-bfs-order");
        let file_contents = [
            ("root.rs", "fn root() {}\n"),
            ("b_parent.rs", "fn b_parent() { root(); }\n"),
            ("c_parent.rs", "fn c_parent() { root(); }\n"),
            ("z_mid.rs", "fn z_mid() { b_parent(); }\n"),
            ("a_mid.rs", "fn a_mid() { c_parent(); }\n"),
            ("z_leaf.rs", "fn z_leaf() { z_mid(); }\n"),
            ("a_leaf.rs", "fn a_leaf() { a_mid(); }\n"),
        ];
        for (file, content) in &file_contents {
            write_file(&root.join(*file), content);
        }
        let files: Vec<String> = file_contents
            .iter()
            .map(|(file, _)| (*file).to_string())
            .collect();
        let entities = vec![
            entity("root-id", "root.rs", "root", "fn root() {}"),
            entity(
                "b-parent-id",
                "b_parent.rs",
                "b_parent",
                "fn b_parent() { root(); }",
            ),
            entity(
                "c-parent-id",
                "c_parent.rs",
                "c_parent",
                "fn c_parent() { root(); }",
            ),
            entity(
                "z-mid-id",
                "z_mid.rs",
                "z_mid",
                "fn z_mid() { b_parent(); }",
            ),
            entity(
                "a-mid-id",
                "a_mid.rs",
                "a_mid",
                "fn a_mid() { c_parent(); }",
            ),
            entity(
                "z-leaf-id",
                "z_leaf.rs",
                "z_leaf",
                "fn z_leaf() { z_mid(); }",
            ),
            entity(
                "a-leaf-id",
                "a_leaf.rs",
                "a_leaf",
                "fn a_leaf() { a_mid(); }",
            ),
        ];
        let graph = graph_with_edges(
            &entities,
            vec![
                edge("b-parent-id", "root-id"),
                edge("c-parent-id", "root-id"),
                edge("z-mid-id", "b-parent-id"),
                edge("a-mid-id", "c-parent-id"),
                edge("z-leaf-id", "z-mid-id"),
                edge("a-leaf-id", "a-mid-id"),
            ],
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

        let result = cache
            .query_impact_topology(
                &root,
                &files,
                shared_cache::CacheSourceScope::Default,
                false,
                Some("root"),
                None,
                Some("root.rs"),
                CachedImpactMode::All,
                3,
            )
            .unwrap()
            .unwrap();

        assert_eq!(
            result
                .impact
                .iter()
                .map(|(entity, depth)| (entity.id.as_str(), *depth))
                .collect::<Vec<_>>(),
            vec![
                ("b-parent-id", 1),
                ("c-parent-id", 1),
                ("z-mid-id", 2),
                ("a-mid-id", 2),
                ("z-leaf-id", 3),
                ("a-leaf-id", 3),
            ]
        );

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
    fn open_existing_readonly_does_not_create_missing_cache() {
        let root = temp_repo_root("readonly-missing");
        let db_path = shared_cache::cache_db_path(&root).unwrap();

        assert!(!db_path.exists());
        assert!(DiskCache::open_existing_readonly(&root).is_err());
        assert!(!db_path.exists());

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
