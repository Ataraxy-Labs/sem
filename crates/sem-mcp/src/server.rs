use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lru::LruCache;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use sem_core::git::bridge::GitBridge;
use sem_core::git::types::DiffScope;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use tokio::sync::Mutex;

use crate::cache;
use crate::tools::*;

/// Lazily-initialized repo context.
struct RepoContext {
    git: GitBridge,
    repo_root: PathBuf,
}

/// LRU cache for parsed entities keyed on (file_path, content_hash).
type EntityCache = LruCache<(String, u64), Vec<SemanticEntity>>;

fn content_hash_u64(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Cached entity graph + all entities, keyed by manifest hash.
struct CachedGraph {
    manifest_hash: u64,
    graph: Arc<EntityGraph>,
    entities: Arc<Vec<SemanticEntity>>,
}

#[derive(Clone)]
pub struct SemServer {
    context: Arc<Mutex<Option<RepoContext>>>,
    registry: Arc<ParserRegistry>,
    entity_cache: Arc<Mutex<EntityCache>>,
    graph_cache: Arc<Mutex<Option<CachedGraph>>>,
    tool_router: ToolRouter<Self>,
}

impl SemServer {
    fn discover_repo_root(file_path_hint: Option<&str>) -> Result<PathBuf, String> {
        // Strategy 1: Absolute file path -> GitBridge::open on parent dir
        if let Some(fp) = file_path_hint {
            let p = Path::new(fp);
            if p.is_absolute() {
                let search_dir = if p.is_dir() { p } else { p.parent().unwrap_or(p) };
                if let Ok(bridge) = GitBridge::open(search_dir) {
                    return Ok(bridge.repo_root().to_path_buf());
                }
            }
        }

        // Strategy 2: SEM_REPO env var
        if let Ok(repo) = std::env::var("SEM_REPO") {
            let p = PathBuf::from(&repo);
            if p.is_dir() {
                return Ok(p);
            }
        }

        // Strategy 3: CWD-based discovery
        if let Ok(cwd) = std::env::current_dir() {
            if let Ok(bridge) = GitBridge::open(&cwd) {
                return Ok(bridge.repo_root().to_path_buf());
            }
        }

        Err(
            "Cannot find git repository. Either:\n\
             - Pass an absolute file path\n\
             - Set SEM_REPO env var to the repo root\n\
             - Run sem-mcp from within a git repo"
                .to_string(),
        )
    }

    fn resolve_file_path(repo_root: &Path, file_path: &str) -> (String, PathBuf) {
        let p = Path::new(file_path);
        if p.is_absolute() {
            let relative = p
                .strip_prefix(repo_root)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| file_path.to_string());
            (relative, p.to_path_buf())
        } else {
            (file_path.to_string(), repo_root.join(file_path))
        }
    }

    async fn get_context(
        &self,
        file_path_hint: Option<&str>,
    ) -> Result<tokio::sync::MappedMutexGuard<'_, RepoContext>, String> {
        {
            let mut guard = self.context.lock().await;
            if guard.is_none() {
                let repo_root = Self::discover_repo_root(file_path_hint)?;
                let git = GitBridge::open(&repo_root)
                    .map_err(|e| format!("Failed to open git repo: {}", e))?;
                *guard = Some(RepoContext { git, repo_root });
            }
        }
        let guard = self.context.lock().await;
        Ok(tokio::sync::MutexGuard::map(guard, |opt| {
            opt.as_mut().unwrap()
        }))
    }

    fn find_supported_files(root: &Path, registry: &ParserRegistry) -> Result<Vec<String>, String> {
        let mut files = Vec::new();
        Self::walk_dir(root, root, registry, &mut files)?;
        files.sort();
        Ok(files)
    }

    fn walk_dir(
        dir: &Path,
        root: &Path,
        registry: &ParserRegistry,
        files: &mut Vec<String>,
    ) -> Result<(), String> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => return Err(format!("Failed to read directory {}: {}", dir.display(), e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.')
                    || name == "node_modules"
                    || name == "target"
                    || name == "__pycache__"
                    || name == "venv"
                    || name == "vendor"
                    || name == "dist"
                    || name == "build"
                {
                    continue;
                }
            }
            if path.is_dir() {
                Self::walk_dir(&path, root, registry, files)?;
            } else if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().to_string();
                if registry.get_plugin(&rel_str).is_some() {
                    files.push(rel_str);
                }
            }
        }
        Ok(())
    }

    fn read_file_at(abs_path: &Path, display_path: &str) -> Result<String, String> {
        std::fs::read_to_string(abs_path)
            .map_err(|e| format!("Failed to read {}: {}", display_path, e))
    }

    async fn extract_entities_from_files(
        &self,
        root: &Path,
        file_paths: &[String],
    ) -> Result<Vec<SemanticEntity>, String> {
        let mut entities = Vec::new();
        for rel_path in file_paths {
            let abs_path = root.join(rel_path);
            let content = match std::fs::read_to_string(&abs_path) {
                Ok(content) => content,
                Err(err) if err.kind() == ErrorKind::InvalidData => continue,
                Err(err) => return Err(format!("Failed to read {}: {}", rel_path, err)),
            };
            entities.extend(self.cached_extract_entities(&content, rel_path).await);
        }
        Ok(entities)
    }

    async fn cached_extract_entities(
        &self,
        content: &str,
        rel_path: &str,
    ) -> Vec<SemanticEntity> {
        let hash = content_hash_u64(content);
        let key = (rel_path.to_string(), hash);

        {
            let mut cache = self.entity_cache.lock().await;
            if let Some(entities) = cache.get(&key) {
                return entities.clone();
            }
        }

        let plugin = match self.registry.get_plugin(rel_path) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let entities = plugin.extract_entities(content, rel_path);

        {
            let mut cache = self.entity_cache.lock().await;
            cache.put(key, entities.clone());
        }

        entities
    }

    /// Find entity by name in graph, preferring match in the target file.
    fn find_entity_in_graph<'a>(
        graph: &'a EntityGraph,
        entity_name: &str,
        rel_path: &str,
    ) -> Result<&'a str, rmcp::ErrorData> {
        graph
            .entities
            .values()
            .find(|e| e.name == entity_name && e.file_path == rel_path)
            .or_else(|| graph.entities.values().find(|e| e.name == entity_name))
            .map(|e| e.id.as_str())
            .ok_or_else(|| internal_err(format!("Entity '{}' not found in graph", entity_name)))
    }

    /// Extract all entities from all supported files in parallel.
    fn extract_all_entities(
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
    ) -> Vec<SemanticEntity> {
        file_paths
            .iter()
            .filter_map(|fp| {
                let full = root.join(fp);
                let content = std::fs::read_to_string(&full).ok()?;
                let plugin = registry.get_plugin(fp)?;
                Some(plugin.extract_entities(&content, fp))
            })
            .flatten()
            .collect()
    }

    /// Get cached graph or build a new one. Checks: memory cache -> SQLite cache -> fresh build.
    async fn get_or_build_graph(
        &self,
        repo_root: &Path,
        file_paths: &[String],
    ) -> (Arc<EntityGraph>, Arc<Vec<SemanticEntity>>) {
        let manifest_hash = cache::compute_manifest_hash(repo_root, file_paths).unwrap_or(0);

        // Check memory cache
        {
            let guard = self.graph_cache.lock().await;
            if let Some(ref cached) = *guard {
                if cached.manifest_hash == manifest_hash {
                    return (cached.graph.clone(), cached.entities.clone());
                }
            }
        }

        // Check SQLite cache (full hit, then incremental)
        if let Ok(disk) = cache::DiskCache::open(repo_root) {
            // Full cache hit
            if let Some((graph, entities)) = disk.load(repo_root, file_paths) {
                let graph = Arc::new(graph);
                let entities = Arc::new(entities);
                let mut guard = self.graph_cache.lock().await;
                *guard = Some(CachedGraph {
                    manifest_hash,
                    graph: graph.clone(),
                    entities: entities.clone(),
                });
                return (graph, entities);
            }

            // Incremental: load clean cached data, rebuild only stale files
            if let Some(partial) = disk.load_partial(repo_root, file_paths) {
                let (graph, entities) = EntityGraph::build_incremental(
                    repo_root,
                    &partial.stale_files,
                    file_paths,
                    partial.cached_entities,
                    partial.cached_edges,
                    partial.stale_file_entities,
                    &self.registry,
                );
                let _ = disk.save_incremental(
                    repo_root,
                    file_paths,
                    &partial.stale_files,
                    &graph,
                    &entities,
                );

                let graph = Arc::new(graph);
                let entities = Arc::new(entities);
                let mut guard = self.graph_cache.lock().await;
                *guard = Some(CachedGraph {
                    manifest_hash,
                    graph: graph.clone(),
                    entities: entities.clone(),
                });
                return (graph, entities);
            }
        }

        // Fresh build
        let (graph, entities) = EntityGraph::build(repo_root, file_paths, &self.registry);

        // Persist to SQLite (best-effort)
        if let Ok(disk) = cache::DiskCache::open(repo_root) {
            let _ = disk.save(repo_root, file_paths, &graph, &entities);
        }

        let graph = Arc::new(graph);
        let entities = Arc::new(entities);

        // Store in memory cache
        {
            let mut guard = self.graph_cache.lock().await;
            *guard = Some(CachedGraph {
                manifest_hash,
                graph: graph.clone(),
                entities: entities.clone(),
            });
        }

        (graph, entities)
    }
}

#[tool_router]
impl SemServer {
    pub fn new() -> Self {
        Self {
            context: Arc::new(Mutex::new(None)),
            registry: Arc::new(create_default_registry()),
            entity_cache: Arc::new(Mutex::new(LruCache::new(
                std::num::NonZeroUsize::new(500).unwrap(),
            ))),
            graph_cache: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    // ── Tool 1: Entities ──

    #[tool(description = "List semantic entities (functions, classes, etc.) under a file or directory path. Defaults to '.'.")]
    async fn sem_entities(
        &self,
        Parameters(params): Parameters<EntitiesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = params.path().unwrap_or(".");
        let ctx = self
            .get_context(Some(path))
            .await
            .map_err(internal_err)?;

        let (rel_path, abs_path) = Self::resolve_file_path(&ctx.repo_root, path);
        let (entities, include_file) = if abs_path.is_file() {
            let content = Self::read_file_at(&abs_path, &rel_path).map_err(internal_err)?;

            let entities = self.cached_extract_entities(&content, &rel_path).await;
            if entities.is_empty() {
                if self.registry.get_plugin(&rel_path).is_none() {
                    return Err(internal_err(format!("No parser for file: {}", rel_path)));
                }
            }
            (entities, false)
        } else if abs_path.is_dir() {
            let mut file_paths = Vec::new();
            Self::walk_dir(&abs_path, &ctx.repo_root, &self.registry, &mut file_paths)
                .map_err(internal_err)?;
            file_paths.sort();

            let all_entities = self
                .extract_entities_from_files(&ctx.repo_root, &file_paths)
                .await
                .map_err(internal_err)?;
            (all_entities, true)
        } else {
            return Err(internal_err(format!("Path not found: {}", path)));
        };

        let result: Vec<serde_json::Value> = entities
            .iter()
            .map(|e| {
                let mut value = serde_json::json!({
                    "id": e.id,
                    "name": e.name,
                    "type": e.entity_type,
                    "start_line": e.start_line,
                    "end_line": e.end_line,
                    "parent_id": e.parent_id,
                });
                if include_file {
                    value["file"] = serde_json::json!(e.file_path);
                }
                value
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    // ── Tool 2: Diff ──

    #[tool(description = "Semantic diff between two refs: shows entity-level changes (added, modified, deleted, renamed) instead of line-level diffs")]
    async fn sem_diff(
        &self,
        Parameters(params): Parameters<DiffParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = self
            .get_context(params.file_path.as_deref())
            .await
            .map_err(internal_err)?;

        let target_ref = params.target_ref.as_deref().unwrap_or("HEAD");

        let scope = DiffScope::Range {
            from: params.base_ref.clone(),
            to: target_ref.to_string(),
        };

        let pathspecs: Vec<String> = if let Some(ref fp) = params.file_path {
            let (rel, _) = Self::resolve_file_path(&ctx.repo_root, fp);
            vec![rel]
        } else {
            vec![]
        };

        let file_changes = ctx
            .git
            .get_changed_files(&scope, &pathspecs)
            .map_err(|e| internal_err(e.to_string()))?;

        let diff_result =
            compute_semantic_diff(&file_changes, &self.registry, None, None);

        let changes: Vec<serde_json::Value> = diff_result
            .changes
            .iter()
            .map(|c| {
                let mut obj = serde_json::json!({
                    "file": c.file_path,
                    "entity_name": c.entity_name,
                    "entity_type": c.entity_type,
                    "change_type": c.change_type.to_string(),
                });
                if let Some(ref old_name) = c.old_entity_name {
                    obj["old_entity_name"] = serde_json::json!(old_name);
                }
                if let Some(ref old_path) = c.old_file_path {
                    obj["old_file_path"] = serde_json::json!(old_path);
                }
                obj
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "base_ref": params.base_ref,
                "target_ref": target_ref,
                "files_analyzed": diff_result.file_count,
                "total_changes": changes.len(),
                "changes": changes,
            }))
            .unwrap_or_default(),
        )]))
    }

    // ── Tool 3: Blame ──

    #[tool(description = "Entity-level git blame: for each entity in a file, shows who last modified it, when, and why")]
    async fn sem_blame(
        &self,
        Parameters(params): Parameters<BlameParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = self
            .get_context(Some(&params.file_path))
            .await
            .map_err(internal_err)?;
        let (rel_path, abs_path) = Self::resolve_file_path(&ctx.repo_root, &params.file_path);
        let content = Self::read_file_at(&abs_path, &rel_path).map_err(internal_err)?;

        let entities = self.cached_extract_entities(&content, &rel_path).await;
        if entities.is_empty() {
            if self.registry.get_plugin(&rel_path).is_none() {
                return Err(internal_err(format!("No parser for file: {}", rel_path)));
            }
        }

        let blame = ctx
            .git
            .blame_file(Path::new(&rel_path))
            .map_err(|e| internal_err(format!("Cannot blame {}: {}", rel_path, e)))?;

        let mut results: Vec<serde_json::Value> = Vec::new();

        for entity in &entities {
            let mut latest_time: i64 = 0;
            let mut latest_author = String::new();
            let mut latest_sha = String::new();
            let mut latest_summary = String::new();
            let mut latest_date = String::new();

            for line in entity.start_line..=entity.end_line {
                if let Some(hunk) = blame.get_line(line) {
                    let sig = hunk.final_signature();
                    let time = sig.when().seconds();
                    if time > latest_time {
                        latest_time = time;
                        latest_author = sig.name().unwrap_or("unknown").to_string();
                        let oid = hunk.final_commit_id();
                        latest_sha = format!("{}", oid);
                        latest_summary = ctx.git.commit_summary(oid).unwrap_or_default();
                        latest_date = chrono_lite_format(sig.when().seconds());
                    }
                }
            }

            results.push(serde_json::json!({
                "name": entity.name,
                "type": entity.entity_type,
                "lines": [entity.start_line, entity.end_line],
                "author": latest_author,
                "date": latest_date,
                "commit": &latest_sha[..8.min(latest_sha.len())],
                "summary": latest_summary,
            }));
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "file": rel_path,
                "entities": results.len(),
                "blame": results,
            }))
            .unwrap_or_default(),
        )]))
    }

    // ── Tool 4: Impact ──

    #[tool(description = "Unified entity analysis: dependencies, dependents, transitive impact, and affected tests. Use 'mode' to narrow: 'all' (default), 'deps', 'dependents', 'tests'.")]
    async fn sem_impact(
        &self,
        Parameters(params): Parameters<ImpactAnalysisParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = self
            .get_context(Some(&params.file_path))
            .await
            .map_err(internal_err)?;
        let (rel_path, _) = Self::resolve_file_path(&ctx.repo_root, &params.file_path);

        let file_paths =
            Self::find_supported_files(&ctx.repo_root, &self.registry).map_err(internal_err)?;
        let (graph, all_entities) = self.get_or_build_graph(&ctx.repo_root, &file_paths).await;

        let entity_id = Self::find_entity_in_graph(&graph, &params.entity_name, &rel_path)?;

        let mode = params.mode.as_deref().unwrap_or("all");

        let output = match mode {
            "deps" => {
                let deps = graph.get_dependencies(entity_id);
                let result: Vec<serde_json::Value> = deps
                    .iter()
                    .map(|d| serde_json::json!({
                            "name": d.name, "type": d.entity_type,
                            "file": d.file_path, "lines": [d.start_line, d.end_line],
                    }))
                    .collect();
                serde_json::json!({
                    "entity": params.entity_name,
                    "file": rel_path,
                    "mode": "deps",
                    "dependencies": result,
                })
            }
            "dependents" => {
                let deps = graph.get_dependents(entity_id);
                let result: Vec<serde_json::Value> = deps
                    .iter()
                    .map(|d| serde_json::json!({
                            "name": d.name, "type": d.entity_type,
                            "file": d.file_path, "lines": [d.start_line, d.end_line],
                    }))
                    .collect();
                serde_json::json!({
                    "entity": params.entity_name,
                    "file": rel_path,
                    "mode": "dependents",
                    "dependents": result,
                })
            }
            "tests" => {
                let tests = graph.test_impact(entity_id, &all_entities);
                let result: Vec<serde_json::Value> = tests
                    .iter()
                    .map(|d| serde_json::json!({
                            "name": d.name, "type": d.entity_type,
                            "file": d.file_path, "lines": [d.start_line, d.end_line],
                    }))
                    .collect();
                serde_json::json!({
                    "entity": params.entity_name,
                    "file": rel_path,
                    "mode": "tests",
                    "tests_affected": result.len(),
                    "tests": result,
                })
            }
            _ => {
                // "all" mode: everything
                let deps = graph.get_dependencies(entity_id);
                let dependents = graph.get_dependents(entity_id);
                let impact = graph.impact_analysis(entity_id);
                let tests = graph.test_impact(entity_id, &all_entities);

                let map_entities = |list: &[&sem_core::parser::graph::EntityInfo]| -> Vec<serde_json::Value> {
                    list.iter().map(|d| serde_json::json!({
                                    "name": d.name, "type": d.entity_type,
                                    "file": d.file_path, "lines": [d.start_line, d.end_line],
                    })).collect()
                    };

                serde_json::json!({
                    "entity": params.entity_name,
                    "file": rel_path,
                    "mode": "all",
                    "dependencies": map_entities(&deps),
                    "dependents": map_entities(&dependents),
                    "impact": {
                        "total": impact.len(),
                        "entities": map_entities(&impact),
                    },
                    "tests": map_entities(&tests),
                })
            }
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&output).unwrap_or_default(),
        )]))
    }

    // ── Tool 5: Log ──

    #[tool(description = "Entity evolution history: trace how a specific entity changed across git commits, distinguishing logic changes from cosmetic ones")]
    async fn sem_log(
        &self,
        Parameters(params): Parameters<LogParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = self
            .get_context(params.file_path.as_deref())
            .await
            .map_err(internal_err)?;

        // Resolve file path: use provided or auto-detect
        let file_path = match params.file_path {
            Some(ref fp) => {
                let (rel, _) = Self::resolve_file_path(&ctx.repo_root, fp);
                rel
            }
            None => {
                let files = Self::find_supported_files(&ctx.repo_root, &self.registry)
                    .map_err(internal_err)?;
                let mut found_in: Vec<String> = Vec::new();
                for fp in &files {
                    let full = ctx.repo_root.join(fp);
                    if let Ok(content) = std::fs::read_to_string(&full) {
                        if let Some(plugin) = self.registry.get_plugin(fp) {
                            let entities = plugin.extract_entities(&content, fp);
                            if entities.iter().any(|e| e.name == params.entity_name) {
                                found_in.push(fp.clone());
                            }
                        }
                    }
                }
                match found_in.len() {
                    0 => {
                        return Err(internal_err(format!(
                            "Entity '{}' not found in any file",
                            params.entity_name
                        )))
                    }
                    1 => found_in.into_iter().next().unwrap(),
                    _ => {
                        return Err(internal_err(format!(
                            "Entity '{}' found in multiple files: {}. Specify file_path to disambiguate.",
                            params.entity_name,
                            found_in.join(", ")
                        )))
                    }
                }
            }
        };

        let plugin = self
            .registry
            .get_plugin(&file_path)
            .ok_or_else(|| internal_err(format!("No parser for file: {}", file_path)))?;

        let limit = params.limit.unwrap_or(50);
        let commits = ctx
            .git
            .get_file_commits(&file_path, limit)
            .map_err(|e| internal_err(format!("Failed to get file history: {}", e)))?;

        if commits.is_empty() {
            return Err(internal_err(format!("No commits found for {}", file_path)));
        }

        let mut entries: Vec<serde_json::Value> = Vec::new();
        let mut prev_entity_content: Option<String> = None;
        let mut prev_structural_hash: Option<String> = None;
        let mut entity_type = String::new();
        let mut found_at_least_once = false;

        // Process oldest to newest
        for commit in commits.iter().rev() {
            let content = match ctx.git.read_file_at_ref(&commit.sha, &file_path) {
                Ok(Some(c)) => c,
                _ => {
                    if prev_entity_content.is_some() {
                        let date = chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0));
                        entries.push(serde_json::json!({
                            "commit": commit.short_sha,
                            "author": commit.author,
                            "date": date,
                            "message": commit.message.lines().next().unwrap_or(""),
                            "change_type": "deleted",
                        }));
                        prev_entity_content = None;
                        prev_structural_hash = None;
                    }
                    continue;
                }
            };

            let file_entities = plugin.extract_entities(&content, &file_path);
            let entity = file_entities.iter().find(|e| e.name == params.entity_name);
            let date = chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0));
            let msg = commit.message.lines().next().unwrap_or("").to_string();

            match entity {
                Some(ent) => {
                    if !found_at_least_once {
                        entity_type = ent.entity_type.clone();
                    }

                    if !found_at_least_once || prev_entity_content.is_none() {
                        found_at_least_once = true;
                        entries.push(serde_json::json!({
                            "commit": commit.short_sha,
                            "author": commit.author,
                            "date": date,
                            "message": msg,
                            "change_type": "added",
                        }));
                    } else {
                        let prev_hash = prev_entity_content
                            .as_ref()
                            .map(|c| sem_core::utils::hash::content_hash(c));
                        let content_changed =
                            prev_hash.as_deref() != Some(ent.content_hash.as_str());

                        if content_changed {
                            let structural_changed = match (
                                ent.structural_hash.as_deref(),
                                prev_structural_hash.as_deref(),
                            ) {
                                (Some(cur), Some(prev)) => cur != prev,
                                _ => true,
                            };

                            let change_type = if structural_changed {
                                "modified (logic)"
                            } else {
                                "modified (cosmetic)"
                            };

                            entries.push(serde_json::json!({
                                "commit": commit.short_sha,
                                "author": commit.author,
                                "date": date,
                                "message": msg,
                                "change_type": change_type,
                            }));
                        }
                    }

                    prev_entity_content = Some(ent.content.clone());
                    prev_structural_hash = ent.structural_hash.clone();
                }
                None => {
                    if prev_entity_content.is_some() {
                        entries.push(serde_json::json!({
                            "commit": commit.short_sha,
                            "author": commit.author,
                            "date": date,
                            "message": msg,
                            "change_type": "deleted",
                        }));
                        prev_entity_content = None;
                        prev_structural_hash = None;
                    }
                }
            }
        }

        if !found_at_least_once {
            return Err(internal_err(format!(
                "Entity '{}' not found in any commit of {}",
                params.entity_name, file_path
            )));
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "entity": params.entity_name,
                "file": file_path,
                "type": entity_type,
                "total_changes": entries.len(),
                "changes": entries,
            }))
            .unwrap_or_default(),
        )]))
    }

    // ── Tool 6: Context ──

    #[tool(description = "Pack optimal entity context into a token budget. Priority: target entity (full) > direct dependents (full) > transitive (signature only).")]
    async fn sem_context(
        &self,
        Parameters(params): Parameters<ContextParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = self
            .get_context(Some(&params.file_path))
            .await
            .map_err(internal_err)?;
        let (rel_path, _) = Self::resolve_file_path(&ctx.repo_root, &params.file_path);

        let file_paths =
            Self::find_supported_files(&ctx.repo_root, &self.registry).map_err(internal_err)?;
        let (graph, all_entities) = self.get_or_build_graph(&ctx.repo_root, &file_paths).await;

        let entity_id = Self::find_entity_in_graph(&graph, &params.entity_name, &rel_path)?;

        let budget = params.token_budget.unwrap_or(8000);
        let entries = sem_core::parser::context::build_context(
            &graph,
            entity_id,
            &all_entities,
            budget,
        );

        let total_tokens: usize = entries.iter().map(|e| e.estimated_tokens).sum();
        let result: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "entity": e.entity_name,
                    "type": e.entity_type,
                    "file": e.file_path,
                    "role": e.role,
                    "tokens": e.estimated_tokens,
                    "content": e.content,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "entity": params.entity_name,
                "file": rel_path,
                "token_budget": budget,
                "tokens_used": total_tokens,
                "entries": result.len(),
                "context": result,
            }))
            .unwrap_or_default(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for SemServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "sem MCP server for entity-level semantic code intelligence. \
             6 tools: entities, diff, blame, impact, log, context.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_supported_files_returns_walk_errors() {
        let missing_root = std::env::temp_dir().join(format!(
            "sem-mcp-missing-root-{}",
            std::process::id()
        ));
        let registry = ParserRegistry::new();

        let err = SemServer::find_supported_files(&missing_root, &registry).unwrap_err();

        assert!(err.contains("Failed to read directory"));
    }
}

fn internal_err(msg: impl ToString) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(msg.to_string(), None)
}

/// Simple timestamp formatting without external deps.
fn chrono_lite_format(unix_seconds: i64) -> String {
    let days = unix_seconds / 86400;
    let mut y = 1970i64;
    let mut remaining_days = days;
    loop {
        let year_days = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md {
            m = i;
            break;
        }
        remaining_days -= md;
    }
    format!("{:04}-{:02}-{:02}", y, m + 1, remaining_days + 1)
}
