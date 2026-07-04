pub mod cache;
pub mod cloud;
pub mod render;
pub mod server;
pub mod sidecar;
pub mod tools;
mod transport;
pub mod watch;

use rmcp::ServiceExt;

/// Run the MCP server on stdin/stdout. Blocks until the client disconnects.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("sem_mcp=info".parse().unwrap()),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();

        let server = server::SemServer::new();
        // Prewarm: build the CWD repo's graph in the background while the
        // transport handshakes, so the agent's first structural query answers
        // from memory instead of paying the cold build.
        server.spawn_prewarm();
        // Socket sidecar: expose the warm graph on a per-repo unix socket so
        // local short-lived callers (prompt prefetch) answer in milliseconds.
        if let Ok(repo_root) = server::SemServer::discover_repo_root(None) {
            sidecar::spawn(server.clone(), repo_root);
        }
        let transport =
            transport::ResilientStdioTransport::new(tokio::io::stdin(), tokio::io::stdout());
        let service = server.serve(transport).await?;
        service.waiting().await?;
        Ok(())
    })
}

/// Resident mode (`sem mcp --resident`, hidden plumbing): serve ONLY the
/// per-repo unix socket — no stdio MCP transport — so short-lived CLI calls
/// answer from a warm in-memory graph in milliseconds. The CLI spawns this
/// detached on a socket miss; it exits immediately if another server already
/// owns the repo's socket (bind race, or a live `sem mcp` session), and
/// retires itself after 30 minutes without a request.
pub fn run_resident() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let Ok(repo_root) = server::SemServer::discover_repo_root(None) else {
            return Ok(()); // not a git repo: nothing to serve
        };
        if sidecar::socket_is_live(&repo_root).await {
            return Ok(()); // a live server already owns this repo's socket
        }
        let server = server::SemServer::new();
        server.spawn_prewarm();
        sidecar::spawn(server.clone(), repo_root);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            // idle_secs is u64::MAX until the socket binds, so a lost bind
            // race exits at the first tick instead of parking forever.
            if sidecar::idle_secs() > 30 * 60 {
                return Ok(());
            }
        }
    })
}
