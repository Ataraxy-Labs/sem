//! `sem diff`'s cloud-upload / relations flow (see
//! commands/diff/cloud_upload.rs `maybe_upload_cloud_diff_snapshot`): the
//! upload happens immediately with no relations computed, then the client
//! branches on the server's `relationsStatus` — computing relations locally
//! and PUTting them only when the server says it won't ("unavailable", or
//! an older server that omits the field entirely). These tests drive the
//! real `sem` binary against a stub HTTP server standing in for sem-cloud,
//! mirroring the stub-server pattern in `sem-mcp/src/agent_review.rs`; the
//! shared bits of that pattern (spinning up the axum server, rendering a
//! subprocess's output for a failure message) live in `tests/support`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::routing::{post, put};
use axum::{Json, Router};

#[path = "support/mod.rs"]
mod support;
use support::{output_text, serve_router};

fn git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Two TS files where `b.ts::consume` calls `a.ts::source`, matching the
/// topology already relied on by `impact_direct_deps.rs`. Modifying
/// `source`'s body afterward gives `build_changed_entity_relations` a real
/// caller edge to report (b.ts::consume as a dependent of a.ts::source).
fn init_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    // A remote is required for `maybe_upload_cloud_diff_snapshot` to run at
    // all; the stub server never resolves it against anything real.
    git(
        repo,
        &["remote", "add", "origin", "https://github.com/test/repo.git"],
    );

    fs::write(
        repo.join("a.ts"),
        "export function source() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("b.ts"),
        "import { source } from './a';\nexport function consume() { return source(); }\n",
    )
    .unwrap();
    git(repo, &["add", "a.ts", "b.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

/// Modify `source`'s body so `sem diff` reports a real semantic change to
/// it (an entity change, not just a file touch), giving the relations pass
/// something to look up callers for.
fn change_source(repo: &Path) {
    fs::write(
        repo.join("a.ts"),
        "export function source() { return 2; }\n",
    )
    .unwrap();
}

#[derive(Default)]
struct Recorded {
    diff_posts: Vec<serde_json::Value>,
    relations_puts: Vec<(String, serde_json::Value)>,
}

struct StubServer {
    base_url: String,
    recorded: Arc<Mutex<Recorded>>,
    _handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct AppState {
    recorded: Arc<Mutex<Recorded>>,
    /// `None` omits `relationsStatus` from the /v1/diffs response entirely,
    /// simulating an older server that predates the field.
    relations_status: Option<String>,
}

async fn diffs_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    state.recorded.lock().unwrap().diff_posts.push(body);
    let mut resp = serde_json::json!({ "id": "snap-1" });
    if let Some(status) = &state.relations_status {
        resp["relationsStatus"] = serde_json::json!(status);
    }
    Json(resp)
}

async fn relations_put_handler(
    State(state): State<AppState>,
    AxumPath(diff_id): AxumPath<String>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    state
        .recorded
        .lock()
        .unwrap()
        .relations_puts
        .push((diff_id, body));
    Json(serde_json::json!({}))
}

/// Start a stub sem-cloud standing in for `/v1/diffs` (POST) and
/// `/v1/diffs/{id}/relations` (PUT). `relations_status` controls what the
/// POST response reports (or omits, for `None`).
async fn start_stub(relations_status: Option<&str>) -> StubServer {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = AppState {
        recorded: recorded.clone(),
        relations_status: relations_status.map(|s| s.to_string()),
    };
    let app = Router::new()
        .route("/v1/diffs", post(diffs_handler))
        .route("/v1/diffs/{diff_id}/relations", put(relations_put_handler))
        .with_state(state);

    let (base_url, handle) = serve_router(app).await;

    StubServer {
        base_url,
        recorded,
        _handle: handle,
    }
}

/// Run `sem diff` against `repo`, with cloud credentials pointed at the
/// stub server. `cloud_on` toggles `SEM_CLOUD=1` (the env-based consent
/// override); when false, no consent decision exists for this repo at all,
/// so the upload path should never even look at the stub server.
fn run_sem_diff(repo: &Path, home: &Path, stub: &StubServer, cloud_on: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sem"));
    cmd.args(["diff"])
        .current_dir(repo)
        .env("HOME", home)
        .env("SEM_TOKEN", "test-token")
        .env("SEM_CLOUD_ENDPOINT", &stub.base_url)
        // Small but generous: the relations pass over a two-file repo is
        // near-instant; this just keeps a genuinely stuck test from hanging.
        .env("SEM_RELATIONS_BUDGET_MS", "5000")
        .env_remove("SEM_LOCAL")
        .env_remove("SEM_NO_NETWORK")
        .env_remove("SEM_RELATIONS_LOCAL");
    if cloud_on {
        cmd.env("SEM_CLOUD", "1");
    } else {
        cmd.env_remove("SEM_CLOUD");
    }
    cmd.output().expect("run sem diff")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_consent_never_touches_the_network() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    // No `SEM_CLOUD=1` and no prior `sem cloud enable`/`share` — consent is
    // off. Point credentials at the stub anyway: if anything upload-shaped
    // ran, it would show up here.
    let stub = start_stub(Some("enrichmentQueued")).await;

    let output = run_sem_diff(repo.path(), home.path(), &stub, false);
    assert!(output.status.success(), "{}", output_text(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("source"), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert!(
        rec.diff_posts.is_empty(),
        "no HTTP request should be made when cloud consent is off: {:?}",
        rec.diff_posts
    );
    assert!(rec.relations_puts.is_empty());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("sem cloud diff:"),
        "byte-identical to no-cloud-account behavior: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrichment_queued_uploads_without_relations_and_does_not_put() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    let stub = start_stub(Some("enrichmentQueued")).await;
    let output = run_sem_diff(repo.path(), home.path(), &stub, true);
    assert!(output.status.success(), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(rec.diff_posts.len(), 1, "{}", output_text(&output));
    assert_eq!(
        rec.diff_posts[0]["relations"],
        serde_json::json!({}),
        "the upload must not carry locally computed relations"
    );
    assert!(
        rec.relations_puts.is_empty(),
        "server owns enrichment; the client must not also PUT relations"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sem cloud diff:"), "{stderr}");
    assert!(
        stderr.contains("computing in cloud"),
        "expected the enrichment-queued one-liner: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_uploads_without_relations_then_puts_them_locally() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    let stub = start_stub(Some("unavailable")).await;
    let output = run_sem_diff(repo.path(), home.path(), &stub, true);
    assert!(output.status.success(), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(rec.diff_posts.len(), 1, "{}", output_text(&output));
    assert_eq!(
        rec.diff_posts[0]["relations"],
        serde_json::json!({}),
        "the upload itself must never carry relations"
    );

    assert_eq!(
        rec.relations_puts.len(),
        1,
        "expected exactly one PUT of locally computed relations: {}",
        output_text(&output)
    );
    assert_eq!(rec.relations_puts[0].0, "snap-1");
    let relations = &rec.relations_puts[0].1;
    let obj = relations.as_object().expect("relations body is an object");
    assert!(
        obj.keys().any(|k| k.ends_with("::source")),
        "expected a relations entry for the changed `source` entity, got: {relations}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sem cloud diff:"), "{stderr}");
    assert!(
        stderr.contains("computed locally and attached"),
        "expected the locally-attached one-liner: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_relations_status_falls_back_to_local_compute_and_put() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    // No `relationsStatus` field at all: an older server, predating this
    // contract. Version-tolerant fallback: same as "unavailable".
    let stub = start_stub(None).await;
    let output = run_sem_diff(repo.path(), home.path(), &stub, true);
    assert!(output.status.success(), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(rec.diff_posts.len(), 1, "{}", output_text(&output));
    assert_eq!(rec.diff_posts[0]["relations"], serde_json::json!({}));

    assert_eq!(
        rec.relations_puts.len(),
        1,
        "an older server (no relationsStatus field) should still get relations attached via PUT: {}",
        output_text(&output)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("computed locally and attached"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sem_relations_local_forces_inline_relations_and_skips_put() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    // Even though the stub reports "unavailable" — which would otherwise
    // trigger a PUT — the escape hatch reproduces the old single-upload
    // behavior: relations inline in the POST body, no PUT at all.
    let stub = start_stub(Some("unavailable")).await;
    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(["diff"])
        .current_dir(repo.path())
        .env("HOME", home.path())
        .env("SEM_TOKEN", "test-token")
        .env("SEM_CLOUD_ENDPOINT", &stub.base_url)
        .env("SEM_CLOUD", "1")
        .env("SEM_RELATIONS_LOCAL", "1")
        .env("SEM_RELATIONS_BUDGET_MS", "5000")
        .output()
        .expect("run sem diff");
    assert!(output.status.success(), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(rec.diff_posts.len(), 1);
    let relations = &rec.diff_posts[0]["relations"];
    let obj = relations.as_object().expect("relations body is an object");
    assert!(
        obj.keys().any(|k| k.ends_with("::source")),
        "SEM_RELATIONS_LOCAL=1 should compute relations before uploading, got: {relations}"
    );
    assert!(
        rec.relations_puts.is_empty(),
        "the escape hatch uploads once with relations inline; it must not also PUT"
    );
}
