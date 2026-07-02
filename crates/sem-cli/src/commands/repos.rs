//! `sem repos` — where your code is stored, locally and on the cloud account.
//!
//! Two inventories side by side:
//! - **Cloud**: the authoritative `GET /v1/repos` listing for the logged-in
//!   account (status, entity/file counts, indexed commit, indexing errors).
//!   As a side effect the local `~/.sem/repos.json` mirror is reconciled with
//!   the server truth, since stale entries mis-route the local-vs-cloud
//!   decision for impact/context queries.
//! - **Local**: every on-disk entity cache under the sem cache root, with
//!   sizes, entity counts, and the repo each cache was built from (caches
//!   written before the `repo_root` stamp show as unlabeled).

use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use rusqlite::{Connection, OpenFlags};
use sem_cloud_client::{CloudClient, CloudRepoInfo};
use sem_core::git::bridge::GitBridge;
use sem_mcp::cache as shared_cache;

pub fn run(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let cloud = fetch_cloud_repos();
    let local = collect_local_caches();

    if json {
        return print_json(&cloud, &local);
    }

    match &cloud {
        CloudSection::Repos(repos) => print_cloud(repos),
        CloudSection::NotLoggedIn => {
            println!(
                "{} not logged in — cloud inventory unavailable. Run: {}",
                "cloud:".bold(),
                "sem login".cyan()
            );
        }
        CloudSection::Error(err) => {
            println!(
                "{} could not reach the cloud API: {}",
                "cloud:".bold(),
                err.dimmed()
            );
        }
    }

    println!();
    print_local(&local);
    Ok(())
}

// ─── Cloud section ───────────────────────────────────────────────────────

enum CloudSection {
    Repos(Vec<CloudRepoInfo>),
    NotLoggedIn,
    Error(String),
}

fn fetch_cloud_repos() -> CloudSection {
    let Some(client) = CloudClient::from_credentials() else {
        return CloudSection::NotLoggedIn;
    };
    match client.list_repos() {
        Ok(repos) => {
            // Keep this machine's mirror honest while we have server truth.
            sem_cloud_client::reconcile_repo_cache(&repos);
            CloudSection::Repos(repos)
        }
        Err(err) => CloudSection::Error(err.to_string()),
    }
}

fn print_cloud(repos: &[CloudRepoInfo]) {
    let ready = repos.iter().filter(|r| r.status == "ready").count();
    println!(
        "{} {} repos ({} ready)",
        "cloud account:".bold(),
        repos.len(),
        ready
    );
    if repos.is_empty() {
        println!(
            "  {}",
            "No repos indexed yet. Any repo with a GitHub remote is registered on first cloud query.".dimmed()
        );
        return;
    }

    let name_w = repos.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    println!(
        "  {:<name_w$}  {:<8}  {:>9}  {:>6}  {:<19}  {:<7}  {}",
        "NAME".dimmed(),
        "STATUS".dimmed(),
        "ENTITIES".dimmed(),
        "FILES".dimmed(),
        "LAST INDEXED".dimmed(),
        "COMMIT".dimmed(),
        "REMOTE".dimmed(),
    );
    for r in repos {
        let status = match r.status.as_str() {
            "ready" => r.status.green().to_string(),
            "error" | "failed" => r.status.red().to_string(),
            _ => r.status.yellow().to_string(),
        };
        println!(
            "  {:<name_w$}  {:<8}  {:>9}  {:>6}  {:<19}  {:<7}  {}",
            r.name.bold(),
            status,
            r.entity_count.map_or("—".into(), fmt_count),
            r.file_count.map_or("—".into(), fmt_count),
            r.last_indexed_at.as_deref().unwrap_or("—"),
            r.last_commit_sha
                .as_deref()
                .map(|s| &s[..s.len().min(7)])
                .unwrap_or("—"),
            r.clone_url.dimmed(),
        );
        if let Some(err) = r.error_message.as_deref().filter(|e| !e.is_empty()) {
            println!("  {:<name_w$}  {}", "", format!("! {err}").red());
        }
    }
}

// ─── Local section ───────────────────────────────────────────────────────

struct LocalCache {
    dir: PathBuf,
    repo_root: Option<String>,
    kind: Option<String>,
    entities: Option<u64>,
    size_bytes: u64,
}

struct LocalSection {
    root: Option<PathBuf>,
    caches: Vec<LocalCache>,
}

fn collect_local_caches() -> LocalSection {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo_root = match GitBridge::open(&cwd) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => cwd,
    };

    let Some(cache_root) = shared_cache::cache_dir_for_repo(&repo_root)
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
    else {
        return LocalSection {
            root: None,
            caches: Vec::new(),
        };
    };

    let mut caches = Vec::new();
    if let Ok(entries) = fs::read_dir(&cache_root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let db = dir.join("cache.db");
            if !db.exists() {
                continue;
            }
            let size_bytes = ["cache.db", "cache.db-wal", "cache.db-shm"]
                .iter()
                .filter_map(|f| fs::metadata(dir.join(f)).ok())
                .map(|m| m.len())
                .sum();
            let (repo_root, kind, entities) = read_cache_summary(&db);
            caches.push(LocalCache {
                dir,
                repo_root,
                kind,
                entities,
                size_bytes,
            });
        }
    }
    caches.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    LocalSection {
        root: Some(cache_root),
        caches,
    }
}

/// Read-only peek into one cache.db; every field is best-effort so a locked
/// or half-written cache never breaks the listing.
fn read_cache_summary(db: &Path) -> (Option<String>, Option<String>, Option<u64>) {
    let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return (None, None, None);
    };
    let meta = |key: &str| -> Option<String> {
        conn.query_row(
            "SELECT value FROM cache_metadata WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .ok()
    };
    let entities = conn
        .query_row("SELECT COUNT(*) FROM entities", [], |row| {
            row.get::<_, u64>(0)
        })
        .ok();
    (meta("repo_root"), meta("cache_kind"), entities)
}

fn print_local(local: &LocalSection) {
    let Some(root) = &local.root else {
        println!(
            "{} no cache root resolvable on this machine",
            "local storage:".bold()
        );
        return;
    };
    let total: u64 = local.caches.iter().map(|c| c.size_bytes).sum();
    println!(
        "{} {} ({} caches, {})",
        "local storage:".bold(),
        root.display(),
        local.caches.len(),
        fmt_size(total)
    );
    for c in &local.caches {
        let label = match &c.repo_root {
            Some(root) => root.clone(),
            None => format!(
                "(unlabeled — built before repo stamping; dir {})",
                c.dir.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            ),
        };
        println!(
            "  {:>9}  {:>9} entities  {:<9}  {}",
            fmt_size(c.size_bytes),
            c.entities.map_or("—".into(), fmt_count),
            c.kind.as_deref().unwrap_or("—"),
            label.bold(),
        );
    }
    if local.caches.is_empty() {
        println!("  {}", "No entity caches built yet.".dimmed());
    }
    if let Some(home) = std::env::var_os("HOME") {
        println!(
            "  {}",
            format!(
                "account files: {}/.sem (credentials.json, repos.json, sock/)",
                PathBuf::from(home).display()
            )
            .dimmed()
        );
    }
}

// ─── Output helpers ──────────────────────────────────────────────────────

fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn fmt_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

fn print_json(
    cloud: &CloudSection,
    local: &LocalSection,
) -> Result<(), Box<dyn std::error::Error>> {
    let cloud_json = match cloud {
        CloudSection::Repos(repos) => serde_json::json!({
            "loggedIn": true,
            "repos": repos.iter().map(|r| serde_json::json!({
                "id": r.id,
                "name": r.name,
                "cloneUrl": r.clone_url,
                "defaultBranch": r.default_branch,
                "status": r.status,
                "entityCount": r.entity_count,
                "fileCount": r.file_count,
                "lastCommitSha": r.last_commit_sha,
                "lastIndexedAt": r.last_indexed_at,
                "errorMessage": r.error_message,
                "createdAt": r.created_at,
            })).collect::<Vec<_>>(),
        }),
        CloudSection::NotLoggedIn => serde_json::json!({ "loggedIn": false }),
        CloudSection::Error(err) => serde_json::json!({ "loggedIn": true, "error": err }),
    };
    let local_json = serde_json::json!({
        "cacheRoot": local.root.as_ref().map(|p| p.display().to_string()),
        "caches": local.caches.iter().map(|c| serde_json::json!({
            "dir": c.dir.display().to_string(),
            "repoRoot": c.repo_root,
            "kind": c.kind,
            "entities": c.entities,
            "sizeBytes": c.size_bytes,
        })).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "cloud": cloud_json,
            "local": local_json,
        }))?
    );
    Ok(())
}
