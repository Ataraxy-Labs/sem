use std::collections::{hash_map::DefaultHasher, HashMap};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lru::LruCache;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use sem_core::format::json::format_diff_json_with_binary_changes;
use sem_core::git::bridge::GitBridge;
use sem_core::git::types::{BlameLineInfo, CommitInfo, DiffScope};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::differ::{collect_binary_file_changes, compute_semantic_diff};
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};
use tokio::sync::Mutex;

use crate::cache;
use crate::tools::*;
use crate::watch::{watch_enabled, RepoWatcher};

const MCP_INSTRUCTIONS: &str = "sem: entity-level code intelligence \
    (functions/classes/methods plus a real cross-file call and import graph). \
    Prefer these over grep/find for structural questions:\n\
    - \"what calls X / what breaks if I change X\" -> sem_impact (not grep)\n\
    - \"understand X with its real callers and callees\" -> sem_context (not reading whole files)\n\
    - \"where is X / find the code that does Y\" (you don't know the name) -> sem_entities with a `query` (ranked structural search), not grep\n\
    - \"list the entities in this file/dir\" -> sem_entities with a `path`\n\
    - entity-level change review -> sem_diff; who last changed X -> sem_blame; how X evolved -> sem_log\n\
    Use grep/find only for text/string search, error messages, config keys, \
    discovery by an unknown name, and non-code files. sem is deterministic and \
    cross-file, so it won't hallucinate edges or miss callers the way a text search does.";

const ENTITY_LOOKUP_CANDIDATE_LIMIT: usize = 10;

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

const BINARY_PROBE_BYTES: usize = 4096;

fn has_nul_byte(path: &Path) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    let mut buffer = [0; BINARY_PROBE_BYTES];
    let len = file.read(&mut buffer)?;
    Ok(buffer[..len].contains(&0))
}

/// Cached entity graph + all entities, keyed by manifest hash.
struct CachedGraph {
    manifest_hash: u64,
    graph: Arc<EntityGraph>,
    entities: Arc<Vec<SemanticEntity>>,
}

struct CachedTopology {
    manifest_hash: u64,
    graph: Arc<EntityGraph>,
}

/// Live-watch bookkeeping for whole-repo graph queries. Lets `sem_impact` and
/// `sem_context` serve a hot cached graph without re-walking + re-stat-ing the
/// tree when nothing has changed since the last build.
struct WatchSlot {
    /// The OS file watcher. `None` until first use; stays `None` if disabled.
    watcher: Option<RepoWatcher>,
    /// False once we've decided not to watch (disabled or failed to start).
    enabled: bool,
    /// Whether the in-memory graph has been built at least once.
    built_once: bool,
    /// Change generation captured at the last build.
    last_built_generation: u64,
    /// Current whole-repo source file list (input to the graph build).
    file_paths: Vec<String>,
}

impl Default for WatchSlot {
    fn default() -> Self {
        Self {
            watcher: None,
            enabled: true,
            built_once: false,
            last_built_generation: 0,
            file_paths: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct SemServer {
    context: Arc<Mutex<Option<RepoContext>>>,
    registry: Arc<ParserRegistry>,
    entity_cache: Arc<Mutex<EntityCache>>,
    graph_cache: Arc<Mutex<Option<CachedGraph>>>,
    topology_cache: Arc<Mutex<Option<CachedTopology>>>,
    watch: Arc<Mutex<WatchSlot>>,
    _tool_router: ToolRouter<Self>,
}

impl SemServer {
    fn discover_repo_root(file_path_hint: Option<&str>) -> Result<PathBuf, String> {
        // Strategy 1: Absolute file path -> GitBridge::open on parent dir
        if let Some(fp) = file_path_hint {
            let p = Path::new(fp);
            if p.is_absolute() {
                let search_dir = if p.is_dir() {
                    p
                } else {
                    p.parent().unwrap_or(p)
                };
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

        Err("Cannot find git repository. Either:\n\
             - Pass an absolute file path\n\
             - Set SEM_REPO env var to the repo root\n\
             - Run sem-mcp from within a git repo"
            .to_string())
    }

    fn resolve_file_path(repo_root: &Path, file_path: &str) -> (String, PathBuf) {
        let p = Path::new(file_path);
        if p.is_absolute() {
            let relative_path = p
                .strip_prefix(repo_root)
                .ok()
                .map(Path::to_path_buf)
                .or_else(|| canonical_relative_path(repo_root, p))
                .map(|path| normalize_relative_path(&path));
            let relative = relative_path
                .map(|r| path_to_slash(&r))
                .unwrap_or_else(|| file_path.replace('\\', "/"));
            (relative, p.to_path_buf())
        } else {
            let abs_path = repo_root.join(file_path);
            let relative_path = normalize_relative_path(p);
            (path_to_slash(&relative_path), abs_path)
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
        Self::find_supported_files_with_options(root, registry, false)
    }

    fn find_supported_files_with_options(
        root: &Path,
        registry: &ParserRegistry,
        no_default_excludes: bool,
    ) -> Result<Vec<String>, String> {
        if !root.exists() {
            return Err(format!(
                "Failed to read directory {}: No such file or directory",
                root.display()
            ));
        }
        let mut files = Vec::new();
        let mut builder = ignore::WalkBuilder::new(root);
        builder
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);
        let semignore = root.join(".semignore");
        if semignore.exists() {
            builder.add_ignore(semignore);
        }
        Self::prune_default_excluded_dirs(&mut builder, root, no_default_excludes);
        let walker = builder.build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if (!no_default_excludes && is_default_excluded(&rel_str))
                    || is_probably_binary_path(&rel_str)
                {
                    continue;
                }
                if registry.get_plugin(&rel_str).is_none() {
                    continue;
                }
                if has_nul_byte(path).unwrap_or(false) {
                    continue;
                }
                files.push(rel_str);
            }
        }
        files.sort();
        Ok(files)
    }

    /// Walk a subdirectory, returning paths relative to `prefix_root` (e.g. the repo root).
    fn walk_dir_files_with_options(
        dir: &Path,
        prefix_root: &Path,
        registry: &ParserRegistry,
        no_default_excludes: bool,
    ) -> Result<Vec<String>, String> {
        let mut files = Vec::new();
        let mut builder = ignore::WalkBuilder::new(dir);
        builder
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);
        let semignore = prefix_root.join(".semignore");
        if semignore.exists() {
            builder.add_ignore(semignore);
        }
        Self::prune_default_excluded_dirs(&mut builder, prefix_root, no_default_excludes);
        let walker = builder.build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(prefix_root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if (!no_default_excludes && is_default_excluded(&rel_str))
                    || is_probably_binary_path(&rel_str)
                {
                    continue;
                }
                if registry.get_plugin(&rel_str).is_none() {
                    continue;
                }
                if has_nul_byte(path).unwrap_or(false) {
                    continue;
                }
                files.push(rel_str);
            }
        }
        files.sort();
        Ok(files)
    }

    fn prune_default_excluded_dirs(
        builder: &mut ignore::WalkBuilder,
        prefix_root: &Path,
        no_default_excludes: bool,
    ) {
        if no_default_excludes {
            return;
        }

        let prefix_root = prefix_root.to_path_buf();
        builder.filter_entry(move |entry| {
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
            {
                return true;
            }

            let Ok(rel) = entry.path().strip_prefix(&prefix_root) else {
                return true;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            !is_default_excluded(&rel_str)
        });
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

    async fn cached_extract_entities(&self, content: &str, rel_path: &str) -> Vec<SemanticEntity> {
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

    /// Find entity by name in the target file.
    fn find_entity_in_graph<'a>(
        graph: &'a EntityGraph,
        entity_name: &str,
        rel_path: &str,
    ) -> Result<&'a str, String> {
        // Match the bare name, or `Class.method` addressing (a child entity
        // whose parent is named by the qualifier before the final dot). Agents
        // reach for `Class.method` naturally.
        let qualified = entity_name.rsplit_once('.');
        let matches = |e: &sem_core::parser::graph::EntityInfo| {
            e.name == entity_name
                || qualified.is_some_and(|(parent_part, child_part)| {
                    e.name == child_part
                        && e.parent_id
                            .as_ref()
                            .and_then(|pid| graph.entities.get(pid))
                            .is_some_and(|p| p.name == parent_part)
                })
        };

        if let Some(entity) = graph
            .entities
            .values()
            .find(|e| matches(e) && e.file_path == rel_path)
        {
            return Ok(entity.id.as_str());
        }

        let mut candidates: Vec<&str> = graph
            .entities
            .values()
            .filter(|e| matches(e))
            .map(|e| e.file_path.as_str())
            .collect();
        candidates.sort_unstable();
        candidates.dedup();

        if candidates.is_empty() {
            Err(format!(
                "Entity '{}' not found in '{}'",
                entity_name, rel_path
            ))
        } else {
            Err(format!(
                "Entity '{}' not found in '{}' (existing candidates: {})",
                entity_name,
                rel_path,
                format_entity_lookup_candidates(&candidates)
            ))
        }
    }

    /// Get cached graph or build a new one. Checks: memory cache -> SQLite cache -> fresh build.
    async fn get_or_build_graph(
        &self,
        repo_root: &Path,
        file_paths: &[String],
        source_scope: cache::CacheSourceScope,
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
            if let Some((graph, entities)) =
                disk.load_with_source_scope(repo_root, file_paths, source_scope)
            {
                let graph = Arc::new(graph);
                let entities = Arc::new(entities);
                let mut guard = self.graph_cache.lock().await;
                *guard = Some(CachedGraph {
                    manifest_hash,
                    graph: graph.clone(),
                    entities: entities.clone(),
                });
                let mut topology_guard = self.topology_cache.lock().await;
                *topology_guard = Some(CachedTopology {
                    manifest_hash,
                    graph: graph.clone(),
                });
                return (graph, entities);
            }

            // Incremental: load clean cached data, rebuild only stale files
            if let Some(partial) =
                disk.load_partial_with_source_scope(repo_root, file_paths, source_scope)
            {
                let (graph, entities, metadata) =
                    EntityGraph::build_incremental_with_metadata_and_import_candidates(
                        repo_root,
                        &partial.stale_files,
                        file_paths,
                        partial.cached_entities,
                        partial.cached_edges,
                        partial.stale_file_entities,
                        Some(&partial.cached_importing_stale_files),
                        &self.registry,
                    );
                let _ = disk.save_incremental_with_repair_metadata(
                    repo_root,
                    file_paths,
                    &partial.stale_files,
                    &graph,
                    &entities,
                    metadata.repaired_clean_entity_ids,
                    &metadata.recomputed_edge_source_ids,
                    &metadata.deleted_entity_ids,
                    source_scope,
                );

                let graph = Arc::new(graph);
                let entities = Arc::new(entities);
                let mut guard = self.graph_cache.lock().await;
                *guard = Some(CachedGraph {
                    manifest_hash,
                    graph: graph.clone(),
                    entities: entities.clone(),
                });
                let mut topology_guard = self.topology_cache.lock().await;
                *topology_guard = Some(CachedTopology {
                    manifest_hash,
                    graph: graph.clone(),
                });
                return (graph, entities);
            }
        }

        // Fresh build
        let (graph, entities) = EntityGraph::build(repo_root, file_paths, &self.registry);

        // Persist to SQLite (best-effort)
        if let Ok(disk) = cache::DiskCache::open(repo_root) {
            let _ = disk.save(repo_root, file_paths, &graph, &entities, source_scope);
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
        {
            let mut guard = self.topology_cache.lock().await;
            *guard = Some(CachedTopology {
                manifest_hash,
                graph: graph.clone(),
            });
        }

        (graph, entities)
    }

    async fn get_or_build_graph_topology(
        &self,
        repo_root: &Path,
        file_paths: &[String],
        source_scope: cache::CacheSourceScope,
    ) -> Arc<EntityGraph> {
        let manifest_hash = cache::compute_manifest_hash(repo_root, file_paths).unwrap_or(0);

        {
            let guard = self.graph_cache.lock().await;
            if let Some(ref cached) = *guard {
                if cached.manifest_hash == manifest_hash {
                    return cached.graph.clone();
                }
            }
        }

        {
            let guard = self.topology_cache.lock().await;
            if let Some(ref cached) = *guard {
                if cached.manifest_hash == manifest_hash {
                    return cached.graph.clone();
                }
            }
        }

        if let Ok(disk) = cache::DiskCache::open(repo_root) {
            if let Some(graph) =
                disk.load_graph_topology_with_source_scope(repo_root, file_paths, source_scope)
            {
                let graph = Arc::new(graph);
                let mut guard = self.topology_cache.lock().await;
                *guard = Some(CachedTopology {
                    manifest_hash,
                    graph: graph.clone(),
                });
                return graph;
            }
        }

        let (graph, _) = self
            .get_or_build_graph(repo_root, file_paths, source_scope)
            .await;
        graph
    }

    fn cache_source_scope(repo_root: &Path, no_default_excludes: bool) -> cache::CacheSourceScope {
        if no_default_excludes || repo_root.join(".semignore").exists() {
            cache::CacheSourceScope::Custom
        } else {
            cache::CacheSourceScope::Default
        }
    }

    /// Ensure the in-memory whole-repo caches are fresh with respect to the file
    /// watcher, returning the current source file list. On the fast path
    /// (nothing changed since the last build) this avoids re-walking and
    /// re-stat-ing the tree entirely. Returns `None` when watching is disabled
    /// or unavailable, in which case the caller uses the stat-based path.
    async fn ensure_live(&self, repo_root: &Path) -> Option<Vec<String>> {
        if !watch_enabled() {
            return None;
        }

        let mut slot = self.watch.lock().await;

        // Lazily start the watcher for this repo on first use.
        if slot.watcher.is_none() {
            if !slot.enabled {
                return None;
            }
            match RepoWatcher::start(repo_root) {
                Ok(w) => slot.watcher = Some(w),
                Err(_) => {
                    slot.enabled = false;
                    return None;
                }
            }
        }

        let drained = slot.watcher.as_ref().unwrap().drain();

        // Fast path: nothing has changed since the last build, so the cached
        // graph is still valid. No walk, no stat storm.
        let clean = slot.built_once
            && drained.generation == slot.last_built_generation
            && !slot.file_paths.is_empty();
        if clean {
            return Some(slot.file_paths.clone());
        }

        // Something changed (or first build). Refresh the file list only when
        // the set of files may have changed; content-only edits reuse it.
        if slot.file_paths.is_empty() || drained.needs_rewalk {
            match Self::find_supported_files(repo_root, &self.registry) {
                Ok(files) => slot.file_paths = files,
                Err(_) => return None,
            }
        }
        let file_paths = slot.file_paths.clone();

        // Rebuild (incrementally, via the disk cache) and repopulate the memory
        // caches that live_graph / live_topology read from.
        let source_scope = Self::cache_source_scope(repo_root, false);
        let _ = self
            .get_or_build_graph(repo_root, &file_paths, source_scope)
            .await;
        slot.last_built_generation = drained.generation;
        slot.built_once = true;
        Some(file_paths)
    }

    /// Whole-repo (graph, entities), kept hot by the file watcher when active.
    async fn live_graph(&self, repo_root: &Path) -> (Arc<EntityGraph>, Arc<Vec<SemanticEntity>>) {
        if self.ensure_live(repo_root).await.is_some() {
            let guard = self.graph_cache.lock().await;
            if let Some(ref cached) = *guard {
                return (cached.graph.clone(), cached.entities.clone());
            }
        }
        let file_paths = Self::find_supported_files(repo_root, &self.registry).unwrap_or_default();
        let source_scope = Self::cache_source_scope(repo_root, false);
        self.get_or_build_graph(repo_root, &file_paths, source_scope)
            .await
    }

    /// Whole-repo graph topology, kept hot by the file watcher when active.
    async fn live_topology(&self, repo_root: &Path) -> Arc<EntityGraph> {
        if self.ensure_live(repo_root).await.is_some() {
            {
                let guard = self.graph_cache.lock().await;
                if let Some(ref cached) = *guard {
                    return cached.graph.clone();
                }
            }
            {
                let guard = self.topology_cache.lock().await;
                if let Some(ref cached) = *guard {
                    return cached.graph.clone();
                }
            }
        }
        let file_paths = Self::find_supported_files(repo_root, &self.registry).unwrap_or_default();
        let source_scope = Self::cache_source_scope(repo_root, false);
        self.get_or_build_graph_topology(repo_root, &file_paths, source_scope)
            .await
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
            topology_cache: Arc::new(Mutex::new(None)),
            watch: Arc::new(Mutex::new(WatchSlot::default())),
            _tool_router: Self::tool_router(),
        }
    }

    // ── Tool 1: Entities ──

    #[tool(
        description = "List semantic entities (functions, classes, etc.) under a file or directory path (defaults to '.'). Pass `query` to instead search the whole repo by intent and find the most relevant entities when you don't know the name (prefer this over grep for \"where is X\")."
    )]
    async fn sem_entities(
        &self,
        Parameters(params): Parameters<EntitiesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = params.path().unwrap_or(".");
        let ctx = match self.get_context(Some(path)).await {
            Ok(ctx) => ctx,
            Err(err) => return Ok(tool_error(err)),
        };

        // Query mode: rank the whole-repo entity graph by relevance to the
        // query (structural search), instead of listing a path.
        if let Some(query) = params.query() {
            let (graph, all_entities) = self.live_graph(&ctx.repo_root).await;
            let hits = sem_core::parser::orient::orient(
                &all_entities,
                &graph,
                query,
                params.limit(),
            );
            let result: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "name": h.name,
                        "type": h.entity_type,
                        "file": h.file_path,
                        "start_line": h.start_line,
                        "signature": h.signature,
                        "dependents": h.dependents,
                        "dependencies": h.dependencies,
                    })
                })
                .collect();
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        let (rel_path, abs_path) = Self::resolve_file_path(&ctx.repo_root, path);
        let (entities, include_file) = if abs_path.is_file() {
            if !params.no_default_excludes() && is_default_excluded(&rel_path) {
                return Ok(tool_error(format!(
                    "Path is excluded by default: {}",
                    rel_path
                )));
            }
            let content = match Self::read_file_at(&abs_path, &rel_path) {
                Ok(content) => content,
                Err(err) => return Ok(tool_error(err)),
            };

            let entities = self.cached_extract_entities(&content, &rel_path).await;
            if entities.is_empty() {
                if self.registry.get_plugin(&rel_path).is_none() {
                    return Ok(tool_error(format!("No parser for file: {}", rel_path)));
                }
            }
            (entities, false)
        } else if abs_path.is_dir() {
            let file_paths = match Self::walk_dir_files_with_options(
                &abs_path,
                &ctx.repo_root,
                &self.registry,
                params.no_default_excludes(),
            ) {
                Ok(file_paths) => file_paths,
                Err(err) => return Ok(tool_error(err)),
            };

            let all_entities = match self
                .extract_entities_from_files(&ctx.repo_root, &file_paths)
                .await
            {
                Ok(entities) => entities,
                Err(err) => return Ok(tool_error(err)),
            };
            (all_entities, true)
        } else {
            return Ok(tool_error(format!("Path not found: {}", path)));
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

    #[tool(
        description = "Semantic diff between two refs: shows entity-level changes (added, modified, deleted, renamed) instead of line-level diffs"
    )]
    async fn sem_diff(
        &self,
        Parameters(params): Parameters<DiffParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = match self.get_context(params.file_path.as_deref()).await {
            Ok(ctx) => ctx,
            Err(err) => return Ok(tool_error(err)),
        };

        let scope = if let Some(ref base) = params.base_ref {
            let target_ref = params.target_ref.as_deref().unwrap_or("HEAD");
            DiffScope::Range {
                from: base.clone(),
                to: target_ref.to_string(),
            }
        } else {
            // Default: working-tree changes, same as CLI `sem diff` (#154)
            DiffScope::Working
        };

        let pathspecs: Vec<String> = if let Some(ref fp) = params.file_path {
            let (rel, abs_path) = Self::resolve_file_path(&ctx.repo_root, fp);
            if let Some(err) = pathspec_error(&ctx.git, &scope, &rel, fp, &abs_path) {
                return Ok(tool_error(err));
            }
            vec![rel]
        } else {
            vec![]
        };

        let file_changes = match ctx.git.get_changed_files(&scope, &pathspecs) {
            Ok(file_changes) => file_changes,
            Err(err) => return Ok(tool_error(err.to_string())),
        };

        let binary_changes = collect_binary_file_changes(&file_changes);
        let diff_result = compute_semantic_diff(&file_changes, &self.registry, None, None);

        Ok(CallToolResult::success(vec![Content::text(
            format_diff_json_with_binary_changes(&diff_result, &binary_changes),
        )]))
    }

    // ── Tool 3: Blame ──

    #[tool(
        description = "Entity-level git blame: for each entity in a file, shows who last modified it, when, and why"
    )]
    async fn sem_blame(
        &self,
        Parameters(params): Parameters<BlameParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = match self.get_context(Some(&params.file_path)).await {
            Ok(ctx) => ctx,
            Err(err) => return Ok(tool_error(err)),
        };
        let (rel_path, abs_path) = Self::resolve_file_path(&ctx.repo_root, &params.file_path);
        let content = match Self::read_file_at(&abs_path, &rel_path) {
            Ok(content) => content,
            Err(err) => return Ok(tool_error(err)),
        };

        let entities = self.cached_extract_entities(&content, &rel_path).await;
        if entities.is_empty() {
            if self.registry.get_plugin(&rel_path).is_none() {
                return Ok(tool_error(format!("No parser for file: {}", rel_path)));
            }
        }

        let blame = match ctx.git.blame_file_porcelain(Path::new(&rel_path)) {
            Ok(blame) => blame,
            Err(err) => return Ok(tool_error(format!("Cannot blame {}: {}", rel_path, err))),
        };
        let blame_by_line: HashMap<usize, BlameLineInfo> = blame
            .into_iter()
            .map(|line| (line.line_number, line))
            .collect();

        let mut results: Vec<serde_json::Value> = Vec::new();

        for entity in &entities {
            let mut selected: Option<&BlameLineInfo> = None;

            for line in entity.start_line..=entity.end_line {
                if let Some(info) = blame_by_line.get(&line) {
                    if info.commit_sha.is_none() {
                        selected = Some(info);
                        break;
                    }

                    let is_newer = match (info.author_time, selected.and_then(|s| s.author_time)) {
                        (Some(current), Some(previous)) => current > previous,
                        (Some(_), None) => true,
                        _ => selected.is_none(),
                    };
                    if is_newer {
                        selected = Some(info);
                    }
                }
            }

            let (author, date, commit_sha, summary) = match selected {
                Some(info) => (
                    if info.author.is_empty() {
                        "unknown".to_string()
                    } else {
                        info.author.clone()
                    },
                    info.author_time.map(chrono_lite_format).unwrap_or_default(),
                    info.commit_sha.clone(),
                    info.summary.clone(),
                ),
                None => (String::from("unknown"), String::new(), None, String::new()),
            };

            results.push(serde_json::json!({
                "name": entity.name,
                "type": entity.entity_type,
                "lines": [entity.start_line, entity.end_line],
                "author": author,
                "date": date,
                "commit": commit_sha,
                "summary": summary,
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

    #[tool(
        description = "Unified entity analysis: dependencies, dependents, transitive impact, and affected tests. Use 'mode' to narrow: 'all' (default), 'deps', 'dependents', 'tests'."
    )]
    async fn sem_impact(
        &self,
        Parameters(params): Parameters<ImpactAnalysisParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = match self.get_context(Some(&params.file_path)).await {
            Ok(ctx) => ctx,
            Err(err) => return Ok(tool_error(err)),
        };
        let (rel_path, abs_path) = Self::resolve_file_path(&ctx.repo_root, &params.file_path);
        if let Some(err) = file_path_error(&params.file_path, &abs_path) {
            return Ok(tool_error(err));
        }
        if self.registry.get_plugin(&rel_path).is_none() {
            return Ok(tool_error(format!("No parser for file: {}", rel_path)));
        }
        let no_default_excludes = params.no_default_excludes.unwrap_or(false);

        let mode = params.mode.as_deref().unwrap_or("all");
        let valid_modes = ["all", "deps", "dependents", "tests"];
        if !valid_modes.contains(&mode) {
            return Ok(tool_error(format!(
                "Invalid mode '{}'. Valid modes: {}",
                mode,
                valid_modes.join(", ")
            )));
        }

        if matches!(mode, "deps" | "dependents") {
            let graph = if no_default_excludes {
                let file_paths = match Self::find_supported_files_with_options(
                    &ctx.repo_root,
                    &self.registry,
                    no_default_excludes,
                ) {
                    Ok(file_paths) => file_paths,
                    Err(err) => return Ok(tool_error(err)),
                };
                let source_scope = Self::cache_source_scope(&ctx.repo_root, no_default_excludes);
                self.get_or_build_graph_topology(&ctx.repo_root, &file_paths, source_scope)
                    .await
            } else {
                self.live_topology(&ctx.repo_root).await
            };
            let entity_id = match Self::find_entity_in_graph(&graph, &params.entity_name, &rel_path)
            {
                Ok(entity_id) => entity_id,
                Err(err) => return Ok(tool_error(err)),
            };

            let output = match mode {
                "deps" => {
                    let deps = graph.get_dependencies(entity_id);
                    let result: Vec<serde_json::Value> = deps
                        .iter()
                        .map(|d| {
                            serde_json::json!({
                                "name": d.name, "type": d.entity_type,
                                "file": d.file_path, "lines": [d.start_line, d.end_line],
                            })
                        })
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
                        .map(|d| {
                            serde_json::json!({
                                "name": d.name, "type": d.entity_type,
                                "file": d.file_path, "lines": [d.start_line, d.end_line],
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "entity": params.entity_name,
                        "file": rel_path,
                        "mode": "dependents",
                        "dependents": result,
                    })
                }
                _ => unreachable!(),
            };

            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&output).unwrap(),
            )]));
        }

        let (graph, all_entities) = if no_default_excludes {
            let file_paths = match Self::find_supported_files_with_options(
                &ctx.repo_root,
                &self.registry,
                no_default_excludes,
            ) {
                Ok(file_paths) => file_paths,
                Err(err) => return Ok(tool_error(err)),
            };
            let source_scope = Self::cache_source_scope(&ctx.repo_root, no_default_excludes);
            self.get_or_build_graph(&ctx.repo_root, &file_paths, source_scope)
                .await
        } else {
            self.live_graph(&ctx.repo_root).await
        };

        let entity_id = match Self::find_entity_in_graph(&graph, &params.entity_name, &rel_path) {
            Ok(entity_id) => entity_id,
            Err(err) => return Ok(tool_error(err)),
        };

        let output = match mode {
            "tests" => {
                let tests = graph.test_impact_with_custom_dirs(
                    entity_id,
                    &all_entities,
                    &self.registry.custom_test_dirs,
                );
                let result: Vec<serde_json::Value> = tests
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "name": d.name, "type": d.entity_type,
                            "file": d.file_path, "lines": [d.start_line, d.end_line],
                        })
                    })
                    .collect();
                serde_json::json!({
                    "entity": params.entity_name,
                    "file": rel_path,
                    "mode": "tests",
                    "tests_affected": result.len(),
                    "tests": result,
                })
            }
            "deps" | "dependents" => unreachable!(),
            _ => {
                // "all" mode: everything
                let deps = graph.get_dependencies(entity_id);
                let dependents = graph.get_dependents(entity_id);
                let impact = graph.impact_analysis(entity_id);
                let tests = graph.test_impact_with_custom_dirs(
                    entity_id,
                    &all_entities,
                    &self.registry.custom_test_dirs,
                );

                let map_entities =
                    |list: &[&sem_core::parser::graph::EntityInfo]| -> Vec<serde_json::Value> {
                        list.iter()
                            .map(|d| {
                                serde_json::json!({
                                    "name": d.name, "type": d.entity_type,
                                    "file": d.file_path, "lines": [d.start_line, d.end_line],
                                })
                            })
                            .collect()
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

    #[tool(
        description = "Entity evolution history: trace how a specific entity changed across git commits, distinguishing logic changes from cosmetic ones"
    )]
    async fn sem_log(
        &self,
        Parameters(params): Parameters<LogParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = match self.get_context(params.file_path.as_deref()).await {
            Ok(ctx) => ctx,
            Err(err) => return Ok(tool_error(err)),
        };

        // Resolve file path: use provided or auto-detect
        let file_path = match params.file_path {
            Some(ref fp) => {
                let (rel, _) = Self::resolve_file_path(&ctx.repo_root, fp);
                rel
            }
            None => {
                let files = match Self::find_supported_files(&ctx.repo_root, &self.registry) {
                    Ok(files) => files,
                    Err(err) => return Ok(tool_error(err)),
                };
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
                        return Ok(tool_error(format!(
                            "Entity '{}' not found in any file",
                            params.entity_name
                        )))
                    }
                    1 => found_in.into_iter().next().unwrap(),
                    _ => {
                        return Ok(tool_error(format!(
                            "Entity '{}' found in multiple files: {}. Specify file_path to disambiguate.",
                            params.entity_name,
                            found_in.join(", ")
                        )))
                    }
                }
            }
        };

        let limit = params.limit.unwrap_or(50);
        let use_file_history = ctx
            .git
            .get_head_sha()
            .ok()
            .and_then(|head| {
                mcp_entity_by_name_at_ref(
                    &ctx.git,
                    &self.registry,
                    &head,
                    &file_path,
                    &params.entity_name,
                )
            })
            .is_some();

        let mut commits = if use_file_history {
            match ctx.git.get_file_commits_follow_renames(&file_path, 0) {
                Ok(file_commits) if !file_commits.is_empty() => {
                    file_commits.into_iter().map(|info| info.commit).collect()
                }
                Ok(_) => match ctx.git.get_log(0) {
                    Ok(log) => log,
                    Err(e) => return Ok(tool_error(format!("Failed to get history: {}", e))),
                },
                Err(e) => return Ok(tool_error(format!("Failed to get file history: {}", e))),
            }
        } else {
            match ctx.git.get_log(0) {
                Ok(log) => log,
                Err(e) => return Ok(tool_error(format!("Failed to get history: {}", e))),
            }
        };

        if commits.is_empty() {
            return Ok(tool_error(format!("No commits found for {}", file_path)));
        }
        commits.reverse();

        let Some(seed) = mcp_find_seed_occurrence(
            &ctx.git,
            &self.registry,
            &commits,
            &params.entity_name,
            Some(&file_path),
        ) else {
            return Ok(tool_error(format!(
                "Entity '{}' not found in any commit of {}",
                params.entity_name, file_path
            )));
        };

        let entity_type = seed.entity.entity_type.clone();
        let mut entries =
            mcp_trace_back_to_origin(&ctx.git, &self.registry, &commits, seed.clone());
        entries.extend(mcp_trace_forward_from_seed(
            &ctx.git,
            &self.registry,
            &commits,
            seed,
        ));
        if limit != 0 && entries.len() > limit {
            let drop_count = entries.len() - limit;
            entries.drain(0..drop_count);
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

    #[tool(
        description = "Pack optimal entity context into a token budget. Priority: target entity > direct dependencies > direct dependents > transitive dependencies > transitive dependents."
    )]
    async fn sem_context(
        &self,
        Parameters(params): Parameters<ContextParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ctx = match self.get_context(Some(&params.file_path)).await {
            Ok(ctx) => ctx,
            Err(err) => return Ok(tool_error(err)),
        };
        let (rel_path, abs_path) = Self::resolve_file_path(&ctx.repo_root, &params.file_path);
        if let Some(err) = file_path_error(&params.file_path, &abs_path) {
            return Ok(tool_error(err));
        }
        if self.registry.get_plugin(&rel_path).is_none() {
            return Ok(tool_error(format!("No parser for file: {}", rel_path)));
        }
        let no_default_excludes = params.no_default_excludes.unwrap_or(false);

        let (graph, all_entities) = if no_default_excludes {
            let file_paths = match Self::find_supported_files_with_options(
                &ctx.repo_root,
                &self.registry,
                no_default_excludes,
            ) {
                Ok(file_paths) => file_paths,
                Err(err) => return Ok(tool_error(err)),
            };
            let source_scope = Self::cache_source_scope(&ctx.repo_root, no_default_excludes);
            self.get_or_build_graph(&ctx.repo_root, &file_paths, source_scope)
                .await
        } else {
            self.live_graph(&ctx.repo_root).await
        };

        let entity_id = match Self::find_entity_in_graph(&graph, &params.entity_name, &rel_path) {
            Ok(entity_id) => entity_id,
            Err(err) => return Ok(tool_error(err)),
        };

        let budget = params.token_budget.unwrap_or(8000);
        let context_result = sem_core::parser::context::build_context_result(
            &graph,
            entity_id,
            &all_entities,
            budget,
        );

        let result: Vec<serde_json::Value> = context_result
            .entries
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
                "tokens_used": context_result.total_tokens,
                "truncated": context_result.truncated,
                "target_omitted": context_result.target_omitted,
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(MCP_INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::parser::plugins::create_default_registry;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn git(repo: &TempDir, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo.path())
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn commit_all_tempdir(repo: &TempDir, message: &str, timestamp: i64) {
        git(repo, &["add", "-A"]);
        let status = Command::new("git")
            .current_dir(repo.path())
            .env("GIT_AUTHOR_DATE", format!("@{timestamp}"))
            .env("GIT_COMMITTER_DATE", format!("@{timestamp}"))
            .args(["commit", "-q", "-m", message])
            .status()
            .unwrap();
        assert!(status.success(), "git commit failed");
    }

    fn rename_history_repo() -> TempDir {
        let repo = TempDir::new().unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "test"]);
        fs::write(repo.path().join("a.py"), "def original(): return 1\n").unwrap();
        commit_all_tempdir(&repo, "v1", 946684800);
        fs::write(repo.path().join("a.py"), "def renamed_func(): return 1\n").unwrap();
        commit_all_tempdir(&repo, "v2: rename function", 946771200);
        git(&repo, &["mv", "a.py", "b.py"]);
        commit_all_tempdir(&repo, "v3: move file", 946857600);
        fs::write(repo.path().join("b.py"), "def renamed_func(): return 2\n").unwrap();
        commit_all_tempdir(&repo, "v4: modify body", 946944000);
        repo
    }

    fn mcp_log_labels(repo: &TempDir, entity_name: &str, file_path: &str) -> Vec<String> {
        let git = GitBridge::open(repo.path()).unwrap();
        let registry = create_default_registry();
        let mut commits = if git
            .get_head_sha()
            .ok()
            .and_then(|head| {
                mcp_entity_by_name_at_ref(&git, &registry, &head, file_path, entity_name)
            })
            .is_some()
        {
            git.get_file_commits_follow_renames(file_path, 0)
                .unwrap()
                .into_iter()
                .map(|info| info.commit)
                .collect()
        } else {
            git.get_log(0).unwrap()
        };
        commits.reverse();
        let seed =
            mcp_find_seed_occurrence(&git, &registry, &commits, entity_name, Some(file_path))
                .unwrap();
        let mut entries = mcp_trace_back_to_origin(&git, &registry, &commits, seed.clone());
        entries.extend(mcp_trace_forward_from_seed(&git, &registry, &commits, seed));
        entries
            .iter()
            .map(|entry| entry["change_type"].as_str().unwrap().to_string())
            .collect()
    }

    fn temp_dir() -> PathBuf {
        let name = format!(
            "sem-mcp-files-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn temp_git_repo(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("sem-mcp-{}-{}-{}", name, std::process::id(), nanos));
        std::fs::create_dir_all(&root).unwrap();
        git2::Repository::init(&root).unwrap();
        root
    }

    fn commit_all(root: &Path, message: &str, removals: &[&str]) {
        let repo = git2::Repository::open(root).unwrap();
        let sig = git2::Signature::now("sem test", "sem@example.com").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        for path in removals {
            index.remove_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();

        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .unwrap();
    }

    fn assert_tool_error(result: CallToolResult, expected_text: &str) {
        let value = serde_json::to_value(result).unwrap();

        assert_eq!(value["isError"], true);
        assert_eq!(value["content"][0]["type"], "text");
        assert!(
            value["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(expected_text),
            "expected tool error to contain {expected_text:?}, got {value}"
        );
    }

    fn assert_tool_success(result: CallToolResult) {
        let value = serde_json::to_value(result).unwrap();

        assert_eq!(value["isError"], false);
    }

    #[test]
    fn entity_lookup_candidate_list_is_bounded() {
        let candidates = [
            "a.py", "b.py", "c.py", "d.py", "e.py", "f.py", "g.py", "h.py", "i.py", "j.py", "k.py",
            "l.py",
        ];

        assert_eq!(
            format_entity_lookup_candidates(&candidates),
            "a.py, b.py, c.py, d.py, e.py, f.py, g.py, h.py, i.py, j.py (+2 more)"
        );
    }

    async fn server_for_repo(root: &Path) -> SemServer {
        let server = SemServer::new();
        let git = GitBridge::open(root).unwrap();
        let repo_root = git.repo_root().to_path_buf();
        *server.context.lock().await = Some(RepoContext { git, repo_root });
        server
    }

    #[test]
    fn find_supported_files_returns_walk_errors() {
        let missing_root =
            std::env::temp_dir().join(format!("sem-mcp-missing-root-{}", std::process::id()));
        let registry = ParserRegistry::new();

        let err = SemServer::find_supported_files(&missing_root, &registry).unwrap_err();

        assert!(err.contains("Failed to read directory"));
    }

    #[tokio::test]
    async fn live_graph_reflects_working_tree_edits_via_watcher() {
        // Proves the watcher keeps the in-memory graph in sync with on-disk
        // edits: after renaming an entity, the live graph must surface the new
        // name without restarting the server.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("a.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();

        let server = SemServer::new();

        // First build seeds the watcher and caches the graph.
        let (graph, _) = server.live_graph(root).await;
        assert!(
            graph.entities.values().any(|e| e.name == "alpha"),
            "initial graph should contain alpha"
        );
        assert!(
            !graph.entities.values().any(|e| e.name == "beta"),
            "initial graph should not contain beta"
        );

        // Edit on disk: rename the entity. content_hash differs, so the change
        // is detected even within the same mtime tick.
        std::fs::write(root.join("a.rs"), "pub fn beta() -> i32 { 2 }\n").unwrap();

        // Poll until the watcher delivers the event and the rebuild lands.
        let mut saw_beta = false;
        for _ in 0..150 {
            let (graph, _) = server.live_graph(root).await;
            if graph.entities.values().any(|e| e.name == "beta") {
                saw_beta = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            saw_beta,
            "live graph should reflect the renamed entity after the edit"
        );
    }

    #[test]
    fn get_info_instructions_reference_registered_tool_names() {
        let info = SemServer::new().get_info();

        assert_eq!(info.instructions.as_deref(), Some(MCP_INSTRUCTIONS));
        assert!(MCP_INSTRUCTIONS.contains("sem_entities"));
        assert!(!MCP_INSTRUCTIONS.contains("tools: entities"));
    }

    #[tokio::test]
    async fn sem_diff_returns_cli_json_envelope() {
        let temp = tempfile::tempdir().unwrap();
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.name", "Sem Test"]);
        run_git(temp.path(), &["config", "user.email", "sem@example.com"]);

        let file_path = temp.path().join("a.py");
        std::fs::write(&file_path, "def foo():\n    return 1\n").unwrap();
        run_git(temp.path(), &["add", "a.py"]);
        run_git(temp.path(), &["commit", "-m", "initial"]);

        std::fs::write(
            &file_path,
            "def foo():\n    return 1\n\n\ndef bar():\n    return 2\n",
        )
        .unwrap();
        let file_path = std::fs::canonicalize(file_path).unwrap();

        let result = SemServer::new()
            .sem_diff(Parameters(DiffParams {
                base_ref: None,
                target_ref: None,
                file_path: Some(file_path.to_string_lossy().to_string()),
            }))
            .await
            .unwrap();

        let text = match &result.content.first().unwrap().raw {
            rmcp::model::RawContent::Text(text) => &text.text,
            other => panic!("expected text content, got {other:?}"),
        };
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        let changes = payload["changes"].as_array().unwrap();
        let change = changes.first().unwrap();

        assert!(payload.get("summary").is_some());
        assert_eq!(payload["summary"]["fileCount"], 1);
        assert_eq!(payload["summary"]["total"], changes.len());
        assert!(payload.get("base_ref").is_none());
        assert!(payload.get("files_analyzed").is_none());
        assert!(change.get("entityId").is_some());
        assert!(change.get("changeType").is_some());
        assert!(change.get("filePath").is_some());
        assert!(change.get("entity_name").is_none());
        assert!(change.get("change_type").is_none());
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn mcp_log_helpers_trace_current_and_historical_names() {
        let repo = rename_history_repo();
        let expected = vec!["added", "renamed", "moved", "modified (logic)"];
        assert_eq!(mcp_log_labels(&repo, "renamed_func", "b.py"), expected);
        assert_eq!(mcp_log_labels(&repo, "original", "a.py"), expected);
    }

    #[test]
    fn find_supported_files_skips_binary_and_default_excludes() {
        let root = temp_dir();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("src/generated")).unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("src/app.js"), "export function app() {}\n").unwrap();
        fs::write(root.join("src/blob.weird"), b"abc\0def").unwrap();
        fs::write(root.join("src/icon.png"), b"\x89PNG\r\n").unwrap();
        fs::write(
            root.join("src/generated/schema.ts"),
            "export function generatedSchema() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/api.generated.ts"),
            "export function generatedApi() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/styles.module.scss.d.ts"),
            "declare const styles: Record<string, string>;\nexport default styles;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/logo.svg.d.ts"),
            "declare const src: string;\nexport default src;\n",
        )
        .unwrap();
        fs::write(
            root.join("dist/generated.js"),
            "export function generated() {}\n",
        )
        .unwrap();

        let registry = create_default_registry();
        let files = SemServer::find_supported_files(&root, &registry).unwrap();

        assert_eq!(files, vec!["src/app.js".to_string()]);

        let files_with_generated =
            SemServer::find_supported_files_with_options(&root, &registry, true).unwrap();
        assert!(files_with_generated.contains(&"src/app.js".to_string()));
        assert!(files_with_generated.contains(&"src/generated/schema.ts".to_string()));
        assert!(files_with_generated.contains(&"src/api.generated.ts".to_string()));
        assert!(files_with_generated.contains(&"src/styles.module.scss.d.ts".to_string()));
        assert!(files_with_generated.contains(&"src/logo.svg.d.ts".to_string()));
        assert!(files_with_generated.contains(&"dist/generated.js".to_string()));
        assert!(!files_with_generated.contains(&"src/blob.weird".to_string()));
        assert!(!files_with_generated.contains(&"src/icon.png".to_string()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normalize_relative_path_returns_dot_for_empty_paths() {
        assert_eq!(normalize_relative_path(Path::new("")), PathBuf::from("."));
        assert_eq!(normalize_relative_path(Path::new("./")), PathBuf::from("."));
        assert_eq!(
            normalize_relative_path(Path::new("src/../sample.py")),
            PathBuf::from("sample.py")
        );
        assert_eq!(
            normalize_relative_path(Path::new("a/../b")),
            PathBuf::from("b")
        );
        assert_eq!(
            normalize_relative_path(Path::new("a/b/../../c")),
            PathBuf::from("c")
        );
        assert_eq!(
            normalize_relative_path(Path::new("a/../../b")),
            PathBuf::from("../b")
        );
    }

    #[test]
    fn resolve_file_path_normalizes_missing_relative_paths_lexically() {
        let root = temp_git_repo("missing-relative-normalize");

        let (rel_path, abs_path) = SemServer::resolve_file_path(&root, "./missing.py");

        assert_eq!(rel_path, "missing.py");
        assert_eq!(abs_path, root.join("./missing.py"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn path_to_slash_converts_backslashes() {
        assert_eq!(path_to_slash(Path::new("a\\b\\c.py")), "a/b/c.py");
        assert_eq!(path_to_slash(Path::new("a/b/c.py")), "a/b/c.py");
    }

    #[test]
    fn resolve_file_path_returns_forward_slashes() {
        let root = temp_git_repo("forward-slash-relative");

        let (rel_path, _) = SemServer::resolve_file_path(&root, "src/inner/file.py");

        assert_eq!(rel_path, "src/inner/file.py");
        assert!(!rel_path.contains('\\'));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_entities_returns_tool_error_for_missing_path() {
        let root = temp_git_repo("missing-path");
        let missing_path = root.join("nonexistent_path.py");
        let server = SemServer::new();

        let result = server
            .sem_entities(Parameters(EntitiesParams {
                path: Some(missing_path.display().to_string()),
                no_default_excludes: None,
                query: None,
                limit: None,
            }))
            .await
            .unwrap();

        assert_tool_error(result, "Path not found:");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_entities_can_opt_into_generated_file_path() {
        let root = temp_git_repo("generated-entities-file");
        std::fs::create_dir_all(root.join("src/generated")).unwrap();
        std::fs::write(
            root.join("src/generated/schema.ts"),
            "export function generatedTarget() { return 1; }\n",
        )
        .unwrap();
        let server = server_for_repo(&root).await;

        let default_result = server
            .sem_entities(Parameters(EntitiesParams {
                path: Some("src/generated/schema.ts".to_string()),
                no_default_excludes: None,
                query: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert_tool_error(default_result, "Path is excluded by default:");

        let opt_in_result = server
            .sem_entities(Parameters(EntitiesParams {
                path: Some("src/generated/schema.ts".to_string()),
                no_default_excludes: Some(true),
                query: None,
                limit: None,
            }))
            .await
            .unwrap();

        let text = match &opt_in_result.content.first().unwrap().raw {
            rmcp::model::RawContent::Text(text) => &text.text,
            other => panic!("expected text content, got {other:?}"),
        };
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(
            payload.as_array().unwrap().iter().any(|entity| {
                entity["name"] == "generatedTarget" && entity["type"] == "function"
            }),
            "generated target should be returned when default excludes are disabled: {payload}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_diff_returns_tool_error_for_missing_file_path() {
        let root = temp_git_repo("missing-diff-file");
        let file_path = root.join("missing.py");
        let server = SemServer::new();

        let result = server
            .sem_diff(Parameters(DiffParams {
                base_ref: None,
                target_ref: None,
                file_path: Some(file_path.display().to_string()),
            }))
            .await
            .unwrap();

        assert_tool_error(result, "Path not found:");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_diff_allows_ref_range_file_absent_from_working_tree() {
        let root = temp_git_repo("range-diff-historical-file");
        std::fs::write(root.join("base.py"), "def base():\n    return 1\n").unwrap();
        commit_all(&root, "base", &[]);
        let base_sha = git2::Repository::open(&root)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();

        let range_file = root.join("branch_only.py");
        std::fs::write(&range_file, "def branch_only():\n    return 1\n").unwrap();
        commit_all(&root, "add branch-only file", &[]);
        let add_sha = git2::Repository::open(&root)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();

        std::fs::remove_file(&range_file).unwrap();
        commit_all(&root, "delete branch-only file", &["branch_only.py"]);
        let server = server_for_repo(&root).await;

        let result = server
            .sem_diff(Parameters(DiffParams {
                base_ref: Some(base_sha),
                target_ref: Some(add_sha),
                file_path: Some("branch_only.py".to_string()),
            }))
            .await
            .unwrap();

        assert_tool_success(result);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_diff_preserves_invalid_ref_errors_with_file_path() {
        let root = temp_git_repo("range-diff-invalid-ref");
        std::fs::write(root.join("sample.py"), "def sample():\n    return 1\n").unwrap();
        commit_all(&root, "initial", &[]);
        let server = server_for_repo(&root).await;

        let result = server
            .sem_diff(Parameters(DiffParams {
                base_ref: Some("missing-ref".to_string()),
                target_ref: Some("HEAD".to_string()),
                file_path: Some("sample.py".to_string()),
            }))
            .await
            .unwrap();

        assert_tool_error(result, "git error:");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_impact_returns_tool_error_for_unknown_entity() {
        let root = temp_git_repo("unknown-entity");
        let file_path = root.join("sample.py");
        std::fs::write(&file_path, "def known_entity():\n    return 1\n").unwrap();
        let server = SemServer::new();

        let result = server
            .sem_impact(Parameters(ImpactAnalysisParams {
                file_path: file_path.display().to_string(),
                entity_name: "nonexistent_zzz".to_string(),
                mode: None,
                no_default_excludes: None,
            }))
            .await
            .unwrap();

        assert_tool_error(result, "Entity 'nonexistent_zzz' not found in 'sample.py'");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_impact_returns_tool_error_for_missing_file_path() {
        let root = temp_git_repo("missing-impact-file");
        let file_path = root.join("missing.py");
        let server = SemServer::new();

        let result = server
            .sem_impact(Parameters(ImpactAnalysisParams {
                file_path: file_path.display().to_string(),
                entity_name: "anything".to_string(),
                mode: None,
                no_default_excludes: None,
            }))
            .await
            .unwrap();

        assert_tool_error(result, "Path not found:");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_impact_returns_tool_error_when_entity_is_not_in_file_path() {
        let root = temp_git_repo("wrong-impact-file");
        let file_path = root.join("notes.txt");
        std::fs::write(&file_path, "known_entity\n").unwrap();
        std::fs::write(
            root.join("sample.py"),
            "def known_entity():\n    return 1\n",
        )
        .unwrap();
        let server = SemServer::new();

        let result = server
            .sem_impact(Parameters(ImpactAnalysisParams {
                file_path: file_path.display().to_string(),
                entity_name: "known_entity".to_string(),
                mode: None,
                no_default_excludes: None,
            }))
            .await
            .unwrap();

        assert_tool_error(
            result,
            "Entity 'known_entity' not found in 'notes.txt' (existing candidates: sample.py)",
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_impact_normalizes_relative_file_path_before_entity_lookup() {
        let root = temp_git_repo("normalized-impact-file");
        std::fs::write(
            root.join("sample.py"),
            "def known_entity():\n    return 1\n",
        )
        .unwrap();
        let server = server_for_repo(&root).await;

        let result = server
            .sem_impact(Parameters(ImpactAnalysisParams {
                file_path: "./sample.py".to_string(),
                entity_name: "known_entity".to_string(),
                mode: Some("deps".to_string()),
                no_default_excludes: None,
            }))
            .await
            .unwrap();

        assert_tool_success(result);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_impact_can_opt_into_generated_files() {
        let root = temp_git_repo("generated-impact-file");
        std::fs::create_dir_all(root.join("src/generated")).unwrap();
        std::fs::write(
            root.join("src/generated/schema.ts"),
            "export function generatedTarget() { return 1; }\n",
        )
        .unwrap();
        let server = server_for_repo(&root).await;

        let default_result = server
            .sem_impact(Parameters(ImpactAnalysisParams {
                file_path: "src/generated/schema.ts".to_string(),
                entity_name: "generatedTarget".to_string(),
                mode: Some("deps".to_string()),
                no_default_excludes: None,
            }))
            .await
            .unwrap();
        assert_tool_error(
            default_result,
            "Entity 'generatedTarget' not found in 'src/generated/schema.ts'",
        );

        let opt_in_result = server
            .sem_impact(Parameters(ImpactAnalysisParams {
                file_path: "src/generated/schema.ts".to_string(),
                entity_name: "generatedTarget".to_string(),
                mode: Some("deps".to_string()),
                no_default_excludes: Some(true),
            }))
            .await
            .unwrap();

        assert_tool_success(opt_in_result);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_context_can_opt_into_generated_files() {
        let root = temp_git_repo("generated-context-file");
        std::fs::create_dir_all(root.join("src/generated")).unwrap();
        std::fs::write(
            root.join("src/generated/schema.ts"),
            "export function generatedTarget() { return 1; }\n",
        )
        .unwrap();
        let server = server_for_repo(&root).await;

        let result = server
            .sem_context(Parameters(ContextParams {
                file_path: "src/generated/schema.ts".to_string(),
                entity_name: "generatedTarget".to_string(),
                token_budget: Some(2000),
                no_default_excludes: Some(true),
            }))
            .await
            .unwrap();

        assert_tool_success(result);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_context_returns_tool_error_when_entity_is_not_in_file_path() {
        let root = temp_git_repo("wrong-context-file");
        let file_path = root.join("notes.txt");
        std::fs::write(&file_path, "known_entity\n").unwrap();
        std::fs::write(
            root.join("sample.py"),
            "def known_entity():\n    return 1\n",
        )
        .unwrap();
        let server = SemServer::new();

        let result = server
            .sem_context(Parameters(ContextParams {
                file_path: file_path.display().to_string(),
                entity_name: "known_entity".to_string(),
                token_budget: None,
                no_default_excludes: None,
            }))
            .await
            .unwrap();

        assert_tool_error(
            result,
            "Entity 'known_entity' not found in 'notes.txt' (existing candidates: sample.py)",
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_context_returns_tool_error_for_unknown_entity() {
        let root = temp_git_repo("unknown-context-entity");
        let file_path = root.join("sample.py");
        std::fs::write(&file_path, "def known_entity():\n    return 1\n").unwrap();
        let server = SemServer::new();

        let result = server
            .sem_context(Parameters(ContextParams {
                file_path: file_path.display().to_string(),
                entity_name: "nonexistent_zzz".to_string(),
                token_budget: None,
                no_default_excludes: None,
            }))
            .await
            .unwrap();

        assert_tool_error(result, "Entity 'nonexistent_zzz' not found in 'sample.py'");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_log_allows_deleted_file_path_from_history() {
        let root = temp_git_repo("deleted-log-file");
        let file_path = root.join("old.py");
        std::fs::write(&file_path, "def old_entity():\n    return 1\n").unwrap();
        commit_all(&root, "add old file", &[]);
        std::fs::remove_file(&file_path).unwrap();
        commit_all(&root, "delete old file", &["old.py"]);
        let server = server_for_repo(&root).await;

        let result = server
            .sem_log(Parameters(LogParams {
                entity_name: "old_entity".to_string(),
                file_path: Some("old.py".to_string()),
                limit: Some(10),
            }))
            .await
            .unwrap();

        assert_tool_success(result);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sem_impact_returns_tool_error_for_invalid_mode() {
        let root = temp_git_repo("invalid-mode");
        let file_path = root.join("sample.py");
        std::fs::write(&file_path, "def known_entity():\n    return 1\n").unwrap();
        let server = SemServer::new();

        let result = server
            .sem_impact(Parameters(ImpactAnalysisParams {
                file_path: file_path.display().to_string(),
                entity_name: "known_entity".to_string(),
                mode: Some("invalid".to_string()),
                no_default_excludes: None,
            }))
            .await
            .unwrap();

        assert_tool_error(result, "Invalid mode 'invalid'");
        let _ = std::fs::remove_dir_all(root);
    }
}

fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg.into())])
}

fn format_entity_lookup_candidates(candidates: &[&str]) -> String {
    let shown = candidates
        .iter()
        .take(ENTITY_LOOKUP_CANDIDATE_LIMIT)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = candidates
        .len()
        .saturating_sub(ENTITY_LOOKUP_CANDIDATE_LIMIT);

    if remaining == 0 {
        shown
    } else {
        format!("{shown} (+{remaining} more)")
    }
}

fn file_path_error(path: &str, abs_path: &Path) -> Option<String> {
    if abs_path.is_file() {
        None
    } else if abs_path.exists() {
        Some(format!("Expected file path: {}", path))
    } else {
        Some(format!("Path not found: {}", path))
    }
}

fn pathspec_error(
    git: &GitBridge,
    scope: &DiffScope,
    rel_path: &str,
    display_path: &str,
    abs_path: &Path,
) -> Option<String> {
    let found = match scope {
        DiffScope::Working => abs_path.exists(),
        DiffScope::Range { from, to } => match (
            path_exists_at_ref(git, from, rel_path),
            path_exists_at_ref(git, to, rel_path),
        ) {
            (Some(from_found), Some(to_found)) => from_found || to_found,
            _ => return None,
        },
        _ => true,
    };

    if found {
        return None;
    }

    Some(format!("Path not found: {}", display_path))
}

fn path_exists_at_ref(git: &GitBridge, refspec: &str, rel_path: &str) -> Option<bool> {
    git.read_file_at_ref(refspec, rel_path)
        .ok()
        .map(|content| content.is_some())
}

fn canonical_relative_path(repo_root: &Path, abs_path: &Path) -> Option<PathBuf> {
    let canonical_path = abs_path.canonicalize().ok()?;
    let canonical_root = repo_root.canonicalize().ok()?;
    canonical_path
        .strip_prefix(canonical_root)
        .ok()
        .map(Path::to_path_buf)
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match normalized.components().next_back() {
                Some(std::path::Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(std::path::Component::ParentDir) | None => normalized.push(".."),
                Some(std::path::Component::RootDir)
                | Some(std::path::Component::Prefix(_))
                | Some(std::path::Component::CurDir) => {}
            },
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                normalized.push(component.as_os_str())
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

// Graph entity `file_path`s are forward-slash, so relative paths must be too or lookups miss on Windows.
fn path_to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[derive(Clone)]
struct McpLogOccurrence {
    commit_index: usize,
    file_path: String,
    entity: SemanticEntity,
}

fn mcp_find_seed_occurrence(
    git: &GitBridge,
    registry: &ParserRegistry,
    commits: &[CommitInfo],
    entity_name: &str,
    file_path: Option<&str>,
) -> Option<McpLogOccurrence> {
    for (index, commit) in commits.iter().enumerate().rev() {
        let paths = match file_path {
            Some(path) => vec![path.to_string()],
            None => git
                .get_commit_changed_files(&commit.sha)
                .unwrap_or_default(),
        };
        for path in paths {
            if let Some(entity) =
                mcp_entity_by_name_at_ref(git, registry, &commit.sha, &path, entity_name)
            {
                return Some(McpLogOccurrence {
                    commit_index: index,
                    file_path: path,
                    entity,
                });
            }
        }
    }
    None
}

fn mcp_trace_back_to_origin(
    git: &GitBridge,
    registry: &ParserRegistry,
    commits: &[CommitInfo],
    seed: McpLogOccurrence,
) -> Vec<serde_json::Value> {
    let mut current = seed;
    let mut entries = Vec::new();
    for child_index in (1..=current.commit_index).rev() {
        let child_commit = &commits[child_index];
        let parent_commit = &commits[child_index - 1];
        let changed_paths = git
            .get_commit_changed_files(&child_commit.sha)
            .unwrap_or_default();
        let previous = mcp_find_related_entity_at_ref(
            git,
            registry,
            &parent_commit.sha,
            &current.file_path,
            &current.entity.name,
            current.entity.structural_hash.as_deref(),
            &changed_paths,
        );
        let Some(previous) = previous else {
            entries.push(mcp_added_entry(child_commit, &current));
            entries.reverse();
            return entries;
        };
        if let Some(entry) = mcp_transition_entry(child_commit, &previous, &current) {
            entries.push(entry);
        }
        current = McpLogOccurrence {
            commit_index: child_index - 1,
            ..previous
        };
    }
    entries.push(mcp_added_entry(&commits[current.commit_index], &current));
    entries.reverse();
    entries
}

fn mcp_trace_forward_from_seed(
    git: &GitBridge,
    registry: &ParserRegistry,
    commits: &[CommitInfo],
    seed: McpLogOccurrence,
) -> Vec<serde_json::Value> {
    let mut current = seed;
    let mut entries = Vec::new();
    for child_index in current.commit_index + 1..commits.len() {
        let child_commit = &commits[child_index];
        let changed_paths = git
            .get_commit_changed_files(&child_commit.sha)
            .unwrap_or_default();
        let next = mcp_find_related_entity_at_ref(
            git,
            registry,
            &child_commit.sha,
            &current.file_path,
            &current.entity.name,
            current.entity.structural_hash.as_deref(),
            &changed_paths,
        );
        let Some(next) = next else {
            entries.push(mcp_deleted_entry(child_commit, &current));
            break;
        };
        if let Some(entry) = mcp_transition_entry(child_commit, &current, &next) {
            entries.push(entry);
        }
        current = McpLogOccurrence {
            commit_index: child_index,
            ..next
        };
    }
    entries
}

fn mcp_find_related_entity_at_ref(
    git: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    preferred_file: &str,
    entity_name: &str,
    structural_hash: Option<&str>,
    changed_paths: &[String],
) -> Option<McpLogOccurrence> {
    let paths = mcp_candidate_paths(preferred_file, changed_paths);
    for path in &paths {
        if let Some(entity) = mcp_entity_by_name_at_ref(git, registry, sha, path, entity_name) {
            return Some(McpLogOccurrence {
                commit_index: 0,
                file_path: path.clone(),
                entity,
            });
        }
    }
    let structural_hash = structural_hash?;
    for path in &paths {
        if let Some(entity) =
            mcp_entity_by_structural_hash_at_ref(git, registry, sha, path, structural_hash)
        {
            return Some(McpLogOccurrence {
                commit_index: 0,
                file_path: path.clone(),
                entity,
            });
        }
    }
    None
}

fn mcp_candidate_paths(preferred_file: &str, changed_paths: &[String]) -> Vec<String> {
    let mut paths = vec![preferred_file.to_string()];
    for path in changed_paths {
        if !paths.iter().any(|existing| existing == path) {
            paths.push(path.clone());
        }
    }
    paths
}

fn mcp_entity_by_name_at_ref(
    git: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    file_path: &str,
    entity_name: &str,
) -> Option<SemanticEntity> {
    mcp_entities_at_ref(git, registry, sha, file_path)
        .into_iter()
        .find(|entity| entity.name == entity_name)
}

fn mcp_entity_by_structural_hash_at_ref(
    git: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    file_path: &str,
    structural_hash: &str,
) -> Option<SemanticEntity> {
    mcp_entities_at_ref(git, registry, sha, file_path)
        .into_iter()
        .find(|entity| entity.structural_hash.as_deref() == Some(structural_hash))
}

fn mcp_entities_at_ref(
    git: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    file_path: &str,
) -> Vec<SemanticEntity> {
    git.read_file_at_ref(sha, file_path)
        .ok()
        .flatten()
        .map(|content| registry.extract_entities(file_path, &content))
        .unwrap_or_default()
}

fn mcp_transition_entry(
    commit: &CommitInfo,
    before: &McpLogOccurrence,
    after: &McpLogOccurrence,
) -> Option<serde_json::Value> {
    let file_changed = before.file_path != after.file_path;
    let name_changed = before.entity.name != after.entity.name;
    let content_changed = before.entity.content_hash != after.entity.content_hash;
    let change_type = if file_changed {
        "moved"
    } else if name_changed {
        "renamed"
    } else if content_changed {
        if mcp_structural_changed(&before.entity, &after.entity) {
            "modified (logic)"
        } else {
            "modified (cosmetic)"
        }
    } else {
        return None;
    };
    let mut entry = mcp_base_entry(commit, change_type, Some(&after.file_path));
    if file_changed {
        entry["prev_file_path"] = serde_json::Value::String(before.file_path.clone());
    }
    Some(entry)
}

fn mcp_structural_changed(before: &SemanticEntity, after: &SemanticEntity) -> bool {
    match (&before.structural_hash, &after.structural_hash) {
        (Some(before), Some(after)) => before != after,
        _ => true,
    }
}

fn mcp_added_entry(commit: &CommitInfo, occurrence: &McpLogOccurrence) -> serde_json::Value {
    mcp_base_entry(commit, "added", Some(&occurrence.file_path))
}

fn mcp_deleted_entry(commit: &CommitInfo, occurrence: &McpLogOccurrence) -> serde_json::Value {
    mcp_base_entry(commit, "deleted", Some(&occurrence.file_path))
}

fn mcp_base_entry(
    commit: &CommitInfo,
    change_type: &str,
    file_path: Option<&str>,
) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "commit": commit.sha,
        "author": commit.author,
        "date": chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0)),
        "message": commit.message.lines().next().unwrap_or(""),
        "change_type": change_type,
    });
    if let Some(file_path) = file_path {
        entry["file_path"] = serde_json::Value::String(file_path.to_string());
    }
    entry
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
