//! Shared stub-server plumbing for `sem`'s cloud-facing integration tests
//! (`diff_cloud_relations.rs`, `review_listen_dry_run.rs`): both drive the
//! real `sem` binary as a subprocess against a tiny axum server standing in
//! for sem-cloud, on a random local port. This is the same pattern
//! `sem-mcp/src/agent_review.rs`'s unit tests use one crate over (see that
//! file's `stub_server` module / `start_with_router` helper) — kept as a
//! separate per-crate copy rather than a shared workspace test-support
//! crate, since sem-mcp's tests exercise the client library directly
//! in-process while these exercise the compiled `sem` binary as a
//! subprocess; sharing would mean a new workspace member for a handful of
//! lines.
//!
//! `#[allow(dead_code)]`: each integration-test binary in `tests/` only
//! uses a subset of these helpers, and cargo warns per-binary about the rest.

#![allow(dead_code)]

use std::process::Output;

/// Render a subprocess's exit status plus stdout/stderr for a test-failure
/// assertion message.
pub(crate) fn output_text(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Spin up an axum `Router` on a random local port and return its base URL
/// plus the serving task's handle (keep the handle alive for the stub
/// server's lifetime; dropping it is fine at test end since the OS reclaims
/// the port).
pub(crate) async fn serve_router(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}
