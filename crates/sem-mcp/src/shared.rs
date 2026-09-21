//! Repository-scoped MCP daemon and transparent stdio proxy.
//!
//! Each checkout gets one warm `SemServer`. Agent processes still speak the
//! ordinary stdio MCP protocol; `sem mcp` connects them to the shared server.

use std::fs;
use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use rmcp::ServiceExt;
use tokio::net::UnixStream;
use tokio::task::JoinSet;

const DAEMON_ENV: &str = "SEM_MCP_SHARED_DAEMON";
const DISABLE_ENV: &str = "SEM_MCP_NO_SHARED";
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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

    if StdUnixStream::connect(&socket).is_err()
        && (spawn_daemon().is_err() || wait_until_ready(&socket).is_err())
    {
        // Sharing is an optimization, never an availability dependency.
        return super::run_stdio();
    }

    proxy_stdio(socket)
}

fn runtime_dir() -> Option<PathBuf> {
    let root = crate::server::SemServer::discover_repo_root(None).ok()?;
    let repo = git2::Repository::discover(root).ok()?;
    Some(repo.path().join("sem"))
}

fn socket_path() -> Option<PathBuf> {
    Some(runtime_dir()?.join("mcp.sock"))
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
    for _ in 0..100 {
        match StdUnixStream::connect(socket) {
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
    let socket = runtime_dir.join("mcp.sock");

    // If a live daemon owns the socket, the concurrently spawned process has
    // nothing to do. Only unlink after proving the endpoint cannot connect.
    if StdUnixStream::connect(&socket).is_ok() {
        return Ok(());
    }
    if socket.exists() {
        fs::remove_file(&socket)?;
    }

    let listener = match std::os::unix::net::UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let result = serve(listener);
    let _ = fs::remove_file(&socket);
    result
}

fn serve(
    listener: std::os::unix::net::UnixListener,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = tokio::net::UnixListener::from_std(listener)?;
        let server = crate::server::SemServer::new();
        server.spawn_prewarm();
        let mut clients = JoinSet::new();

        loop {
            if clients.is_empty() {
                match tokio::time::timeout(IDLE_TIMEOUT, listener.accept()).await {
                    Ok(Ok((stream, _))) => spawn_client(&mut clients, server.clone(), stream),
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => return Ok(()),
                }
            } else {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        spawn_client(&mut clients, server.clone(), stream);
                    }
                    _ = clients.join_next() => {}
                }
            }
        }
    })
}

fn spawn_client(
    clients: &mut JoinSet<()>,
    server: crate::server::SemServer,
    stream: UnixStream,
) {
    clients.spawn(async move {
        let (read, write) = stream.into_split();
        let transport = crate::transport::ResilientStdioTransport::new(read, write);
        match server.serve(transport).await {
            Ok(service) => {
                let _ = service.waiting().await;
            }
            Err(error) => tracing::warn!("shared MCP client failed: {error}"),
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
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let path = runtime_dir().unwrap();
        std::env::set_current_dir(previous).unwrap();
        assert_eq!(
            path,
            dir.path().canonicalize().unwrap().join(".git/sem")
        );
    }
}
