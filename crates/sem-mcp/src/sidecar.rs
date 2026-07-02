//! Unix-socket sidecar: sub-10ms entity lookups from the resident graph.
//!
//! Every agent session runs `sem mcp`, which holds the repo's entity graph
//! warm in memory. This sidecar exposes that graph on a per-repo unix socket
//! so short-lived local callers (the prompt-prefetch hook, future CLI fast
//! paths) can answer entity questions in single-digit milliseconds instead of
//! paying a fresh process + SQLite hydrate (~800ms).
//!
//! Protocol: one JSON request line -> one JSON response line, connection
//! closed. Request: {"op":"context","name":"...","budget":900,"hops":1}.
//! Response: {"ok":true,"text":"..."} or {"ok":false,"error":"..."}.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;

use crate::server::SemServer;

/// Unix seconds of the last socket request (or the successful bind). 0 means
/// this process never bound the socket — resident mode uses that to exit
/// immediately when it lost the bind race to another server.
static LAST_ACTIVITY: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds since the sidecar last served a request. u64::MAX when this
/// process never successfully bound the socket.
pub fn idle_secs() -> u64 {
    match LAST_ACTIVITY.load(Ordering::Relaxed) {
        0 => u64::MAX,
        t => now_secs().saturating_sub(t),
    }
}

/// True when a live server already answers on this repo's socket.
#[cfg(unix)]
pub async fn socket_is_live(repo_root: &Path) -> bool {
    match socket_path_for(repo_root) {
        Some(path) => tokio::net::UnixStream::connect(&path).await.is_ok(),
        None => false,
    }
}

#[cfg(not(unix))]
pub async fn socket_is_live(_repo_root: &Path) -> bool {
    false
}

/// FNV-1a, chosen because it is trivially reproducible in any language the
/// socket's clients are written in (the Python hook implements the same five
/// lines). Unix socket paths cap out around 104 bytes on macOS, so the repo
/// root is identified by hash rather than by sanitized path.
#[cfg(unix)]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Socket path for a repo root: ~/.sem/sock/<fnv1a(canonical_root)>.sock
#[cfg(unix)]
pub fn socket_path_for(repo_root: &Path) -> Option<PathBuf> {
    let canonical = repo_root.canonicalize().ok()?;
    let home = dirs_home()?;
    let dir = home.join(".sem").join("sock");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!(
        "{:016x}.sock",
        fnv1a(canonical.to_string_lossy().as_bytes())
    )))
}

#[cfg(unix)]
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Bind the sidecar for `repo_root`, stealing a stale socket if a previous
/// server died without cleanup. Returns None (silently) when binding isn't
/// possible — the sidecar is an accelerator, never a requirement.
#[cfg(unix)]
pub fn spawn(server: SemServer, repo_root: PathBuf) {
    tokio::spawn(async move {
        let Some(path) = socket_path_for(&repo_root) else {
            return;
        };
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(_) => {
                // Stale socket from a dead server? If nobody answers, take over.
                if tokio::net::UnixStream::connect(&path).await.is_err() {
                    let _ = std::fs::remove_file(&path);
                    match UnixListener::bind(&path) {
                        Ok(l) => l,
                        Err(_) => return,
                    }
                } else {
                    return; // a live server already owns this repo's socket
                }
            }
        };
        LAST_ACTIVITY.store(now_secs(), Ordering::Relaxed);

        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let server = server.clone();
            let repo_root = repo_root.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut line = String::new();
                let mut reader = BufReader::new(read_half);
                if reader.read_line(&mut line).await.is_err() {
                    return;
                }
                LAST_ACTIVITY.store(now_secs(), Ordering::Relaxed);
                let resp = handle(&server, &repo_root, line.trim()).await;
                let _ = write_half.write_all(resp.as_bytes()).await;
                let _ = write_half.write_all(b"\n").await;
            });
        }
    });
}

/// Windows: no unix sockets — the sidecar is an accelerator, not a
/// requirement, so it simply doesn't exist there. Callers fall back to the
/// one-shot CLI path.
#[cfg(not(unix))]
pub fn spawn(_server: SemServer, _repo_root: PathBuf) {}

#[cfg(unix)]
async fn handle(server: &SemServer, repo_root: &Path, req: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(req) {
        Ok(v) => v,
        Err(e) => return err(&format!("bad request: {e}")),
    };
    match parsed.get("op").and_then(|v| v.as_str()) {
        Some("context") => {
            let Some(name) = parsed.get("name").and_then(|v| v.as_str()) else {
                return err("missing name");
            };
            let budget = parsed.get("budget").and_then(|v| v.as_u64()).unwrap_or(900) as usize;
            let hops = parsed.get("hops").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let session = parsed.get("session").and_then(|v| v.as_str());
            match server.quick_context(repo_root, name, budget, hops, session).await {
                Ok(text) => serde_json::json!({ "ok": true, "text": text }).to_string(),
                Err(e) => err(&e),
            }
        }
        Some("impact") => {
            let Some(name) = parsed.get("name").and_then(|v| v.as_str()) else {
                return err("missing name");
            };
            let file = parsed.get("file").and_then(|v| v.as_str());
            let depth = parsed.get("depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
            match server.quick_impact(repo_root, name, file, depth).await {
                Ok(result) => serde_json::json!({ "ok": true, "result": result }).to_string(),
                Err(e) => err(&e),
            }
        }
        Some("orient") => {
            let Some(query) = parsed.get("query").and_then(|v| v.as_str()) else {
                return err("missing query");
            };
            let limit = parsed.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            match server.quick_orient(repo_root, query, limit).await {
                Ok(text) => serde_json::json!({ "ok": true, "text": text }).to_string(),
                Err(e) => err(&e),
            }
        }
        Some("text") => {
            let Some(needle) = parsed.get("needle").and_then(|v| v.as_str()) else {
                return err("missing needle");
            };
            let limit = parsed.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            match server.quick_text(repo_root, needle, limit).await {
                Ok(text) => serde_json::json!({ "ok": true, "text": text }).to_string(),
                Err(e) => err(&e),
            }
        }
        Some("ping") => serde_json::json!({ "ok": true, "text": "pong" }).to_string(),
        _ => err("unknown op"),
    }
}

#[cfg(unix)]
fn err(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}
