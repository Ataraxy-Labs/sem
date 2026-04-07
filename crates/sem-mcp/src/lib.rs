pub mod cache;
pub mod server;
pub mod tools;

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
        let transport = (tokio::io::stdin(), tokio::io::stdout());
        let service = server.serve(transport).await?;
        service.waiting().await?;
        Ok(())
    })
}
