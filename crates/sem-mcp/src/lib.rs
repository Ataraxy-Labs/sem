pub mod agent_review;
pub mod cache;
pub mod cloud;
pub mod render;
pub(crate) mod review_protocol;
pub mod server;
#[cfg(unix)]
mod shared;
pub mod tools;
mod transport;
pub mod watch;

use rmcp::ServiceExt;

/// Check repository daemon availability without starting one.
pub fn shared_status() -> serde_json::Value {
    #[cfg(unix)]
    {
        shared::status()
    }
    #[cfg(not(unix))]
    {
        serde_json::json!({"status": "unsupported", "transport": "stdio"})
    }
}

/// Run the MCP server on stdin/stdout. Blocks until the client disconnects.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        return shared::run();
    }

    #[cfg(not(unix))]
    run_stdio()
}

/// Run a standalone MCP server on stdin/stdout.
///
/// This remains the fallback when repository discovery or the shared daemon
/// is unavailable, and is intentionally public for embedders.
pub fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
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
        let transport =
            transport::ResilientStdioTransport::new(tokio::io::stdin(), tokio::io::stdout());
        let service = server.serve(transport).await?;
        service.waiting().await?;
        Ok(())
    })
}

// `sem mcp --resident` used to serve only the per-repo sidecar unix socket
// (GREP-KILLER S4): 0% availability at
// production scale (the socket accepted connections and never answered),
// a +300ms tax on every CLI call that tried it, ~2.6GB steady RSS,
// and ~935 leaked sockets observed in the wild. The index it existed to
// work around now answers cold in 6-7ms, which deleted the justification
// (the "fresh process + SQLite hydrate (~800ms)" the sidecar's own docs
// cited no longer describes any real code path). `run_resident` and the
// `sidecar` module it drove are gone; `--resident` itself is kept as a
// backward-compatible no-op flag in `sem-cli` (see main.rs) so an
// already-installed `sem setup` SessionStart hook that still invokes
// `sem mcp --resident` exits cleanly instead of erroring, until that hook
// wiring is updated separately.
