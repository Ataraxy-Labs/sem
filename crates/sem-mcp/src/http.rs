//! Trusted-client, single-repository Streamable HTTP endpoint.
//! This is not a multi-tenant sandbox or a public hosting service.
use std::{
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    Router,
};
use rmcp::{
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData, RoleServer, ServerHandler,
};
use subtle::ConstantTimeEq;

use crate::server::SemServer;

const TOOLS: &[&str] = &["sem_find", "sem_grep"];

#[derive(Clone)]
struct ReadServer {
    inner: SemServer,
    root: Arc<PathBuf>,
}

impl ServerHandler for ReadServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Read-only code discovery for one configured repository. Use sem_find for entities or file reads and sem_grep for text. No edits, builds, validation commands, or cloud operations are exposed.")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: crate::server::listed_tools()
                .into_iter()
                .filter(|t| TOOLS.contains(&t.name.as_ref()))
                .collect(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if !TOOLS.contains(&request.name.as_ref()) {
            return Err(ErrorData::invalid_params(
                "Tool is not exposed over HTTP",
                None,
            ));
        }
        if let Some(args) = &request.arguments {
            for key in ["in", "file"] {
                if let Some(value) = args.get(key).and_then(|v| v.as_str()) {
                    check_path(&self.root, value)?;
                }
            }
            if let Some(files) = args.get("files").and_then(|v| v.as_array()) {
                for file in files {
                    if let Some(value) = file.as_str() {
                        check_path(&self.root, value)?;
                    }
                }
            }
        }
        self.inner.call_tool(request, context).await
    }
}

// Reject traversal and existing symlink escapes, including glob prefixes.
// The checkout must remain trusted: this is not protection against a local
// process racing a symlink change while a tool executes.
fn check_path(root: &Path, value: &str) -> Result<(), ErrorData> {
    let invalid = || {
        ErrorData::invalid_params(
            "Use repository-relative paths without traversal or symlink escapes",
            None,
        )
    };
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(invalid());
    }
    let mut current = root.to_path_buf();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                current.push(part);
                if let Ok(resolved) = current.canonicalize() {
                    if !resolved.starts_with(root) {
                        return Err(invalid());
                    }
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Access {
    authorization: String,
    authority: String,
    permits: Arc<tokio::sync::Semaphore>,
}

fn authorized(access: &Access, request: &Request) -> Result<(), StatusCode> {
    // No browser access/CORS in this first version. Protect against DNS rebinding.
    if request.headers().contains_key("origin") {
        return Err(StatusCode::FORBIDDEN);
    }
    if request.headers().get("host").and_then(|h| h.to_str().ok())
        != Some(access.authority.as_str())
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let supplied = request
        .headers()
        .get("authorization")
        .map(|h| h.as_bytes())
        .unwrap_or_default();
    if !bool::from(supplied.ct_eq(access.authorization.as_bytes())) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

async fn guard(
    State(access): State<Access>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    authorized(&access, &request)?;
    let _permit = access
        .permits
        .try_acquire()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(next.run(request).await)
}

pub fn run(address: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    if !address.ip().is_loopback() {
        return Err("HTTP currently requires a loopback address; use a trusted SSH tunnel for remote clients".into());
    }
    let token = std::env::var("SEM_MCP_HTTP_TOKEN")
        .map_err(|_| "Set SEM_MCP_HTTP_TOKEN to a random token of at least 32 characters")?;
    if token.len() < 32 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(
            "SEM_MCP_HTTP_TOKEN must contain at least 32 printable non-space ASCII characters"
                .into(),
        );
    }
    let root =
        PathBuf::from(std::env::var("SEM_REPO").map_err(|_| "HTTP requires an explicit SEM_REPO")?)
            .canonicalize()?;
    let server = SemServer::for_repository(root.clone())?;
    tokio::runtime::Runtime::new()?.block_on(async move {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let mut config = StreamableHttpServerConfig::default();
        // No unbounded idle session registry. Each request uses a fresh ledger,
        // sharing only repository caches. No cross-client read deduplication.
        config.stateful_mode = false;
        config.json_response = true;
        config.allowed_hosts = vec![address.to_string()];
        let shutdown = config.cancellation_token.clone();
        let root = Arc::new(root);
        let service = StreamableHttpService::new(
            move || {
                Ok(ReadServer {
                    inner: server.new_session(),
                    root: root.clone(),
                })
            },
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let access = Access {
            authorization: format!("Bearer {token}"),
            authority: address.to_string(),
            permits: Arc::new(tokio::sync::Semaphore::new(8)),
        };
        let app = Router::new()
            .nest_service("/mcp", service)
            .layer(tower_http::limit::RequestBodyLimitLayer::new(1024 * 1024))
            .layer(middleware::from_fn_with_state(access, guard));
        eprintln!("Sem read-only MCP: http://{address}/mcp (trusted clients only)");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = tokio::signal::ctrl_c().await;
                shutdown.cancel();
            })
            .await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn access_requires_token_exact_host_and_no_origin() {
        let access = Access {
            authorization: "Bearer secret".into(),
            authority: "127.0.0.1:8765".into(),
            permits: Arc::new(tokio::sync::Semaphore::new(8)),
        };
        let request = |host: &str, token: &str| {
            Request::builder()
                .header("host", host)
                .header("authorization", token)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        assert!(authorized(&access, &request("127.0.0.1:8765", "Bearer secret")).is_ok());
        assert_eq!(
            authorized(&access, &request("attacker.example", "Bearer secret")),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            authorized(&access, &request("127.0.0.1:8765", "Bearer wrong")),
            Err(StatusCode::UNAUTHORIZED)
        );
        let mut r = request("127.0.0.1:8765", "Bearer secret");
        r.headers_mut()
            .insert("origin", "http://127.0.0.1:8765".parse().unwrap());
        assert_eq!(authorized(&access, &r), Err(StatusCode::FORBIDDEN));
    }
    #[test]
    fn paths_stay_relative() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(check_path(&root, "src/**/*.rs").is_ok());
        for path in ["/etc/passwd", "../secret", "src/../../secret"] {
            assert!(check_path(&root, path).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
            assert!(check_path(&root, "escape/etc/passwd").is_err());
        }
    }
}
