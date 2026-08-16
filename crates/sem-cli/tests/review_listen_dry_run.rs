//! `sem review listen --dry-run` — drives the real `sem` binary against a
//! stub HTTP server standing in for sem-cloud, mirroring the stub-server
//! pattern in `diff_cloud_relations.rs` / `sem-mcp/src/agent_review.rs`; the
//! shared bits of that pattern (spinning up the axum server, rendering a
//! subprocess's output for a failure message) live in `tests/support`.
//!
//! `--dry-run` is both a user-facing safety valve (see the exact command and
//! env before it execs `claude`) and, per its own contract, "this is also
//! how tests verify" the subcommand: it still resolves credentials and
//! validates the diff exists against sem-cloud (a real network call to the
//! stub here), then prints the assembled invocation instead of launching.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::routing::get;
use axum::{Json, Router};

#[path = "support/mod.rs"]
mod support;
use support::{output_text, serve_router};

const DIFF_ID: &str = "4c1ca0f5-fbf8-4347-b0f0-244d1d91e92d";
const API_KEY: &str = "sem_test_1234567890abcdef";

#[derive(Default)]
struct Recorded {
    manifest_gets: Vec<String>,
}

struct StubServer {
    base_url: String,
    recorded: Arc<Mutex<Recorded>>,
    _handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct AppState {
    recorded: Arc<Mutex<Recorded>>,
    found: bool,
}

async fn manifest_handler(
    State(state): State<AppState>,
    AxumPath(diff_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    state.recorded.lock().unwrap().manifest_gets.push(diff_id.clone());
    if !state.found {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    Ok(Json(serde_json::json!({
        "label": format!("review {diff_id}"),
        "files": ["a.rs", "b.rs"],
        "comments": [],
    })))
}

async fn start_stub(found: bool) -> StubServer {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = AppState { recorded: recorded.clone(), found };
    let app = Router::new()
        .route("/v1/diffs/{diff_id}/manifest", get(manifest_handler))
        .with_state(state);

    let (base_url, handle) = serve_router(app).await;

    StubServer {
        base_url,
        recorded,
        _handle: handle,
    }
}

/// A fake plugin directory (just needs `.claude-plugin/plugin.json` to
/// satisfy `locate_plugin_dir`'s marker check) — independent of wherever
/// this checkout's real `integrations/claude-review-listener` happens to be,
/// so the test doesn't depend on the test binary's location relative to it.
fn fake_plugin_dir(home: &Path) -> std::path::PathBuf {
    let dir = home.join("fake-plugin");
    fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    fs::write(
        dir.join(".claude-plugin").join("plugin.json"),
        r#"{"name":"sem-review-listener","description":"test","version":"0.0.0"}"#,
    )
    .unwrap();
    dir
}

fn run_dry_run(diff_arg: &str, home: &Path, stub: &StubServer, plugin_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(["review", "listen", diff_arg, "--dry-run"])
        .env("HOME", home)
        .env("SEM_CLOUD_URL", &stub.base_url)
        .env("SEM_API_KEY", API_KEY)
        .env("SEM_LISTENER_PLUGIN_DIR", plugin_dir)
        .env_remove("SEM_LISTENER_MODEL")
        .output()
        .expect("run sem review listen --dry-run")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_assembles_the_documented_command_for_a_bare_diff_id() {
    let home = tempfile::tempdir().unwrap();
    let stub = start_stub(true).await;
    let plugin_dir = fake_plugin_dir(home.path());

    let output = run_dry_run(DIFF_ID, home.path(), &stub, &plugin_dir);
    assert!(output.status.success(), "{}", output_text(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[dry-run] would launch:"), "{stdout}");
    assert!(stdout.contains("claude --plugin-dir"), "{stdout}");
    assert!(stdout.contains(&plugin_dir.display().to_string()), "{stdout}");
    assert!(stdout.contains("--model claude-opus-5"), "{stdout}");
    assert!(stdout.contains("--allowedTools"), "{stdout}");
    assert!(
        stdout.contains("mcp__plugin_sem-review-listener_sem-review__*"),
        "{stdout}"
    );
    assert!(stdout.contains(" Read "), "{stdout}");
    assert!(stdout.contains(" Grep "), "{stdout}");
    assert!(stdout.contains(" Glob "), "{stdout}");
    assert!(stdout.contains("-p "), "{stdout}");
    assert!(stdout.contains(DIFF_ID), "the join prompt must carry the diff id: {stdout}");
    assert!(stdout.contains("join_review"), "{stdout}");

    assert!(stdout.contains(&format!("SEM_CLOUD_URL={}", stub.base_url)), "{stdout}");
    assert!(stdout.contains(&format!("SEM_REVIEW_DIFF_ID={DIFF_ID}")), "{stdout}");

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(rec.manifest_gets, vec![DIFF_ID.to_string()], "must validate the diff exists");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_masks_the_api_key() {
    let home = tempfile::tempdir().unwrap();
    let stub = start_stub(true).await;
    let plugin_dir = fake_plugin_dir(home.path());

    let output = run_dry_run(DIFF_ID, home.path(), &stub, &plugin_dir);
    assert!(output.status.success(), "{}", output_text(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(API_KEY),
        "the full API key must never be printed: {stdout}"
    );
    assert!(stdout.contains("SEM_API_KEY="), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_normalizes_a_hosted_url_with_canvas_suffix_and_query_string() {
    let home = tempfile::tempdir().unwrap();
    let stub = start_stub(true).await;
    let plugin_dir = fake_plugin_dir(home.path());

    let url = format!("https://sem-cloud.fly.dev/diffs/{DIFF_ID}/canvas?tab=files#L10");
    let output = run_dry_run(&url, home.path(), &stub, &plugin_dir);
    assert!(output.status.success(), "{}", output_text(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&format!("SEM_REVIEW_DIFF_ID={DIFF_ID}")), "{stdout}");
    assert!(!stdout.contains("canvas"), "the /canvas suffix must be stripped: {stdout}");

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(rec.manifest_gets, vec![DIFF_ID.to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_reports_a_friendly_error_when_the_diff_does_not_exist() {
    let home = tempfile::tempdir().unwrap();
    let stub = start_stub(false).await; // manifest 404s
    let plugin_dir = fake_plugin_dir(home.path());

    let output = run_dry_run(DIFF_ID, home.path(), &stub, &plugin_dir);
    assert!(!output.status.success(), "{}", output_text(&output));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(DIFF_ID), "{stderr}");
    assert!(stderr.to_lowercase().contains("could not find"), "{stderr}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("[dry-run] would launch:"),
        "an invalid diff must not print an assembled command: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_honors_sem_listener_model_override() {
    let home = tempfile::tempdir().unwrap();
    let stub = start_stub(true).await;
    let plugin_dir = fake_plugin_dir(home.path());

    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(["review", "listen", DIFF_ID, "--dry-run"])
        .env("HOME", home.path())
        .env("SEM_CLOUD_URL", &stub.base_url)
        .env("SEM_API_KEY", API_KEY)
        .env("SEM_LISTENER_PLUGIN_DIR", &plugin_dir)
        .env("SEM_LISTENER_MODEL", "claude-sonnet-5")
        .output()
        .expect("run sem review listen --dry-run");
    assert!(output.status.success(), "{}", output_text(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--model claude-sonnet-5"), "{stdout}");
    assert!(!stdout.contains("claude-opus-5"), "{stdout}");
}
