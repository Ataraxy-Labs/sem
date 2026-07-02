//! CLI fast path over the resident server's unix socket.
//!
//! When a `sem mcp` server is resident for this repo, its sidecar socket
//! answers from the warm in-memory graph in single-digit milliseconds —
//! skipping this process's cache open + hydrate entirely. The sidecar is an
//! accelerator, never a requirement: any failure (no socket, no server, slow
//! reply, protocol mismatch) returns `None` and the caller runs the normal
//! local path. `SEM_NO_SIDECAR=1` disables it explicitly.

use std::path::Path;

/// Send one JSON request line to the repo's sidecar socket and return the
/// parsed response if — and only if — it answered `ok: true` in time.
#[cfg(unix)]
pub fn query(repo_root: &Path, request: &serde_json::Value) -> Option<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    if std::env::var_os("SEM_NO_SIDECAR").is_some() {
        return None;
    }
    let path = sem_mcp::sidecar::socket_path_for(repo_root)?;
    let mut stream = UnixStream::connect(&path).ok()?;
    // A warm server replies in milliseconds; a server mid-rebuild might not.
    // Bound the wait so the fast path can never make the CLI slower than
    // just doing the local work.
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .ok()?;

    let mut line = serde_json::to_string(request).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).ok()?;
    let value: serde_json::Value = serde_json::from_str(response.trim()).ok()?;
    if value.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        Some(value)
    } else {
        None
    }
}

#[cfg(not(unix))]
pub fn query(_repo_root: &Path, _request: &serde_json::Value) -> Option<serde_json::Value> {
    None
}
