//! Repository-scoped MCP daemon and transparent stdio proxy.
//!
//! Each checkout gets one warm `SemServer`. Agent processes still speak the
//! ordinary stdio MCP protocol; `sem mcp` connects them to the shared server.

use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use tokio::net::UnixStream;
use tokio::task::JoinSet;

const DAEMON_ENV: &str = "SEM_MCP_SHARED_DAEMON";
const DISABLE_ENV: &str = "SEM_MCP_NO_SHARED";
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub(crate) fn status() -> serde_json::Value {
    let Some(socket) = socket_path() else {
        return serde_json::json!({"status": "no_repository"});
    };
    let healthy = probe(&socket).is_ok();
    serde_json::json!({"status": if healthy {"ready"} else {"unavailable"},
        "socket": socket, "transport": "unix", "protocol": 2})
}

fn probe(socket: &Path) -> io::Result<()> {
    let mut stream = StdUnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"clientInfo\":{\"name\":\"sem-health\",\"version\":\"1\"}}}\n")?;
    let mut line = String::new();
    io::BufReader::new(stream).read_line(&mut line)?;
    let reply: serde_json::Value = serde_json::from_str(&line)?;
    if reply["id"] != 1 || !reply["result"].is_object() {
        return Err(io::Error::other("MCP health handshake failed"));
    }
    Ok(())
}

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return super::run_stdio();
    }
    if std::env::var_os(DAEMON_ENV).is_some() {
        return run_daemon();
    }

    let Some(socket) = socket_path() else {
        return super::run_stdio();
    };

    if probe(&socket).is_err() && (spawn_daemon().is_err() || wait_until_ready(&socket).is_err()) {
        if std::env::var_os("SEM_MCP_REQUIRE_SHARED").is_some() {
            return Err("shared MCP daemon unavailable (strict shared mode)".into());
        }
        eprintln!("sem: shared MCP unavailable; falling back to standalone stdio");
        // Sharing is an optimization, never an availability dependency.
        return super::run_stdio();
    }

    proxy_stdio(socket)
}

fn runtime_dir() -> Option<PathBuf> {
    let root = crate::server::SemServer::discover_repo_root(None).ok()?;
    runtime_dir_for(&root)
}

fn runtime_dir_for(root: &Path) -> Option<PathBuf> {
    let repo = git2::Repository::discover(root).ok()?;
    Some(repo.path().join("sem"))
}

fn socket_path() -> Option<PathBuf> {
    Some(runtime_dir()?.join("mcp-v2.sock"))
}

fn spawn_daemon() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let is_dedicated_binary = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("sem-mcp"));
    let mut command = Command::new(executable);
    if !is_dedicated_binary {
        // `sem-mcp` enters the server directly; the umbrella `sem` CLI needs
        // its subcommand preserved in the detached child.
        command.arg("mcp");
    }
    command
        .process_group(0)
        .env(DAEMON_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn wait_until_ready(socket: &Path) -> io::Result<()> {
    let mut last_error = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match probe(socket) {
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("shared MCP daemon did not start")))
}

fn proxy_stdio(socket: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let stream = UnixStream::connect(socket).await?;
        let (mut socket_read, mut socket_write) = stream.into_split();
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();

        let upload = tokio::io::copy(&mut stdin, &mut socket_write);
        let download = tokio::io::copy(&mut socket_read, &mut stdout);
        tokio::pin!(upload);
        tokio::pin!(download);

        tokio::select! {
            result = &mut upload => { result?; }
            result = &mut download => { result?; }
        }
        Ok(())
    })
}

fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let Some(runtime_dir) = runtime_dir() else {
        return Err("shared MCP daemon must start inside a git repository".into());
    };
    fs::create_dir_all(&runtime_dir)?;
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;
    // Hold an OS lock for the daemon lifetime. Crashes release the lock;
    // competing starters cannot unlink a newly bound live socket.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(runtime_dir.join("mcp-v2.lock"))?;
    if let Err(error) = lock.try_lock() {
        return match error {
            std::fs::TryLockError::WouldBlock => Ok(()),
            std::fs::TryLockError::Error(error) => Err(error.into()),
        };
    }
    let socket = runtime_dir.join("mcp-v2.sock");

    // If a live daemon owns the socket, the concurrently spawned process has
    // nothing to do. Only unlink after proving the endpoint cannot connect.
    if StdUnixStream::connect(&socket).is_ok() {
        return Ok(());
    }
    if socket.exists() {
        use std::os::unix::fs::FileTypeExt;
        if !fs::symlink_metadata(&socket)?.file_type().is_socket() {
            return Err("refusing to replace a non-socket at the MCP socket path".into());
        }
        fs::remove_file(&socket)?;
    }

    let listener = match std::os::unix::net::UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let metadata = runtime_dir.join("mcp-v2.json");
    fs::write(
        &metadata,
        serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(), "version": env!("CARGO_PKG_VERSION"),
            "protocol": 2, "socket": socket
        }))?,
    )?;
    listener.set_nonblocking(true)?;
    let result = serve(listener);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(&metadata);
    result
}

fn serve(listener: std::os::unix::net::UnixListener) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = tokio::net::UnixListener::from_std(listener)?;
        let root = crate::server::SemServer::discover_repo_root(None)?;
        let server = crate::server::SemServer::for_repository(root)?;
        server.spawn_prewarm();
        let mut clients = JoinSet::new();
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        loop {
            if clients.is_empty() {
                let accepted = tokio::select! {
                    _ = terminate.recv() => return Ok(()),
                    accepted = tokio::time::timeout(IDLE_TIMEOUT, listener.accept()) => accepted,
                };
                match accepted {
                    Ok(Ok((stream, _))) => spawn_client(&mut clients, server.new_session(), stream),
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => return Ok(()),
                }
            } else {
                tokio::select! {
                    _ = terminate.recv() => return Ok(()),
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        spawn_client(&mut clients, server.new_session(), stream);
                    }
                    _ = clients.join_next() => {}
                }
            }
        }
    })
}

fn spawn_client(clients: &mut JoinSet<()>, server: crate::server::SemServer, stream: UnixStream) {
    if clients.len() >= 64 {
        return;
    }
    clients.spawn(async move {
        let (read, write) = stream.into_split();
        let transport = crate::transport::ResilientStdioTransport::new(read, write);
        match tokio::time::timeout(Duration::from_secs(10), server.serve(transport)).await {
            Ok(Ok(service)) => {
                let _ = service.waiting().await;
            }
            Ok(Err(error)) => tracing::warn!("shared MCP client failed: {error}"),
            Err(_) => tracing::warn!("shared MCP client handshake timed out"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_lives_inside_git_metadata() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let path = runtime_dir_for(dir.path()).unwrap();
        assert_eq!(path, dir.path().canonicalize().unwrap().join(".git/sem"));
    }
}
