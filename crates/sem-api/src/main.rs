use anyhow::{Context, Result};
use axum::{extract::Json, http::StatusCode, response::IntoResponse, routing::{get, post}, Router};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tower_http::cors::CorsLayer;

use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::graph::{EntityGraph, EntityInfo};
use sem_core::parser::plugins::create_default_registry;

// ── Error ────────────────────────────────────────────────────────

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({ "error": self.0.to_string() });
        (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// ── Requests ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EntitiesReq {
    repo: String,
    path: String,
}

#[derive(Deserialize)]
struct GraphReq {
    repo: String,
    files: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct EntityReq {
    repo: String,
    entity: String,
    files: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ImpactReq {
    repo: String,
    entity: String,
    files: Option<Vec<String>>,
    max: Option<usize>,
}

#[derive(Deserialize)]
struct DiffReq {
    repo: String,
    #[serde(rename = "ref")]
    diff_ref: Option<String>,
    base: Option<String>,
    head: Option<String>,
}

#[derive(Deserialize)]
struct SearchReq {
    repo: String,
    query: String,
    files: Option<Vec<String>>,
}

// ── Repo management ──────────────────────────────────────────────

const SUPPORTED_EXT: &[&str] = &[
    "ts", "tsx", "mts", "cts", "js", "jsx", "py", "go", "rs", "java",
    "c", "cpp", "cc", "h", "hpp", "rb", "cs", "php",
];

const IGNORED_DIRS: &[&str] = &[
    "node_modules", "target", "vendor", "__pycache__",
    "dist", "build", ".git", ".next", "venv", ".venv",
];

fn resolve_repo(repo: &str) -> Result<PathBuf> {
    let path = PathBuf::from(repo);
    if path.exists() {
        return Ok(path);
    }

    let url = if repo.contains("://") {
        repo.to_string()
    } else if repo.contains('/') && !repo.starts_with('/') {
        format!("https://github.com/{}.git", repo)
    } else {
        anyhow::bail!("repo not found: {repo}");
    };

    let hash = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(url.as_bytes()));
    let cache_dir = PathBuf::from("/tmp/sem-repos").join(&hash[..12]);

    if cache_dir.join(".git").exists() {
        return Ok(cache_dir);
    }

    std::fs::create_dir_all("/tmp/sem-repos")?;
    git2::Repository::clone(&url, &cache_dir).context("clone failed")?;
    Ok(cache_dir)
}

fn discover_files(root: &Path) -> Vec<String> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !IGNORED_DIRS.contains(&name.as_ref()) && !name.starts_with('.')
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| SUPPORTED_EXT.contains(&x))
                .unwrap_or(false)
        })
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .collect()
}

fn get_files(repo_path: &Path, files: Option<Vec<String>>) -> Vec<String> {
    files.unwrap_or_else(|| discover_files(repo_path))
}

// ── Handlers ─────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "sem-api",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn entities(Json(req): Json<EntitiesReq>) -> Result<impl IntoResponse, AppError> {
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let repo_path = resolve_repo(&req.repo)?;
        let file = repo_path.join(&req.path);
        let content = std::fs::read_to_string(&file)
            .context(format!("cannot read {}", req.path))?;
        let registry = create_default_registry();
        let plugin = registry
            .get_plugin(&req.path)
            .ok_or_else(|| anyhow::anyhow!("unsupported: {}", req.path))?;
        let ents = plugin.extract_entities(&content, &req.path);
        let count = ents.len();
        Ok(serde_json::json!({
            "file": req.path,
            "entities": ents,
            "count": count,
        }))
    })
    .await??;
    Ok(Json(result))
}

async fn graph(Json(req): Json<GraphReq>) -> Result<impl IntoResponse, AppError> {
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let repo_path = resolve_repo(&req.repo)?;
        let registry = create_default_registry();
        let files = get_files(&repo_path, req.files);
        let (g, _) = EntityGraph::build(&repo_path, &files, &registry);
        let entity_count = g.entities.len();
        let edge_count = g.edges.len();
        Ok(serde_json::json!({
            "entities": g.entities,
            "edges": g.edges,
            "entityCount": entity_count,
            "edgeCount": edge_count,
        }))
    })
    .await??;
    Ok(Json(result))
}

async fn dependencies(Json(req): Json<EntityReq>) -> Result<impl IntoResponse, AppError> {
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let repo_path = resolve_repo(&req.repo)?;
        let registry = create_default_registry();
        let files = get_files(&repo_path, req.files);
        let (g, _) = EntityGraph::build(&repo_path, &files, &registry);
        let deps = g.get_dependencies(&req.entity);
        let count = deps.len();
        Ok(serde_json::json!({
            "entity": req.entity,
            "dependencies": deps,
            "count": count,
        }))
    })
    .await??;
    Ok(Json(result))
}

async fn dependents(Json(req): Json<EntityReq>) -> Result<impl IntoResponse, AppError> {
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let repo_path = resolve_repo(&req.repo)?;
        let registry = create_default_registry();
        let files = get_files(&repo_path, req.files);
        let (g, _) = EntityGraph::build(&repo_path, &files, &registry);
        let deps = g.get_dependents(&req.entity);
        let count = deps.len();
        Ok(serde_json::json!({
            "entity": req.entity,
            "dependents": deps,
            "count": count,
        }))
    })
    .await??;
    Ok(Json(result))
}

async fn impact(Json(req): Json<ImpactReq>) -> Result<impl IntoResponse, AppError> {
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let repo_path = resolve_repo(&req.repo)?;
        let registry = create_default_registry();
        let files = get_files(&repo_path, req.files);
        let (g, _) = EntityGraph::build(&repo_path, &files, &registry);
        let max = req.max.unwrap_or(10_000);
        let affected = g.impact_analysis_capped(&req.entity, max);
        let count = affected.len();
        Ok(serde_json::json!({
            "entity": req.entity,
            "impact": affected,
            "count": count,
        }))
    })
    .await??;
    Ok(Json(result))
}

async fn diff(Json(req): Json<DiffReq>) -> Result<impl IntoResponse, AppError> {
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let repo_path = resolve_repo(&req.repo)?;
        let registry = create_default_registry();
        let git = sem_core::git::bridge::GitBridge::open(&repo_path)?;

        let scope = if let Some(r) = req.diff_ref {
            sem_core::git::types::DiffScope::Commit { sha: r }
        } else if let (Some(base), Some(head)) = (req.base, req.head) {
            sem_core::git::types::DiffScope::Range { from: base, to: head }
        } else {
            git.detect_and_get_files()?.0
        };

        let files = git.get_changed_files(&scope)?;
        let sha = git.get_head_sha().ok();
        let result = compute_semantic_diff(&files, &registry, sha.as_deref(), None);
        Ok(serde_json::to_value(&result)?)
    })
    .await??;
    Ok(Json(result))
}

async fn search(Json(req): Json<SearchReq>) -> Result<impl IntoResponse, AppError> {
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let repo_path = resolve_repo(&req.repo)?;
        let registry = create_default_registry();
        let files = get_files(&repo_path, req.files);
        let (g, _) = EntityGraph::build(&repo_path, &files, &registry);

        let query_lower = req.query.to_lowercase();
        let matches: Vec<&EntityInfo> = g
            .entities
            .values()
            .filter(|e| {
                e.name.to_lowercase().contains(&query_lower)
                    || e.id.to_lowercase().contains(&query_lower)
            })
            .collect();
        let count = matches.len();
        Ok(serde_json::json!({
            "query": req.query,
            "matches": matches,
            "count": count,
        }))
    })
    .await??;
    Ok(Json(result))
}

// ── Main ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/entities", post(entities))
        .route("/v1/graph", post(graph))
        .route("/v1/dependencies", post(dependencies))
        .route("/v1/dependents", post(dependents))
        .route("/v1/impact", post(impact))
        .route("/v1/diff", post(diff))
        .route("/v1/search", post(search))
        .layer(CorsLayer::permissive());

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7777);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("sem-api listening on http://{addr}");
    println!("sem-api listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
