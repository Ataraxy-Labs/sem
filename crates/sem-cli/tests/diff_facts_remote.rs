//! `sem diff`'s cross-machine facts sync (`commands/diff/facts_remote.rs`,
//! cloud half): drives the real `sem` binary against a stub
//! sem-cloud, same pattern `diff_cloud_relations.rs` already established
//! (see `tests/support/mod.rs`) — a fresh stub axum server per test,
//! `sem diff` run as a subprocess with `SEM_CLOUD_ENDPOINT` pointed at it.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

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

fn init_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/test/repo.git",
        ],
    );
    fs::write(
        repo.join("a.ts"),
        "export function source() { return 1; }\n",
    )
    .unwrap();
    git(repo, &["add", "a.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn change_source(repo: &Path) {
    fs::write(
        repo.join("a.ts"),
        "export function source() { return 2; }\n",
    )
    .unwrap();
}

#[derive(Default)]
struct Recorded {
    query_bodies: Vec<serde_json::Value>,
    put_bodies: Vec<Vec<u8>>,
}

#[derive(Clone)]
struct AppState {
    recorded: Arc<Mutex<Recorded>>,
    /// Indices to report as already-known in the query response — lets a
    /// test exercise the "known, download it" path for at least one key.
    known_indices: Vec<usize>,
}

async fn diffs_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "id": "snap-1", "relationsStatus": "enrichmentQueued" }))
}

async fn facts_query_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let key_count = body["keys"].as_array().map(|a| a.len()).unwrap_or(0);
    state.recorded.lock().unwrap().query_bodies.push(body);
    let unknown: Vec<usize> = (0..key_count)
        .filter(|i| !state.known_indices.contains(i))
        .collect();
    Json(serde_json::json!({
        "knownIndices": state.known_indices,
        "unknownIndices": unknown,
    }))
}

async fn facts_download_handler(
    State(_state): State<AppState>,
    Json(_body): Json<serde_json::Value>,
) -> axum::response::Response {
    #[derive(Serialize)]
    struct DownloadBody {
        truncated: bool,
        facts: Vec<serde_json::Value>,
    }
    let mut buf = Vec::new();
    ciborium::into_writer(
        &DownloadBody {
            truncated: false,
            facts: vec![],
        },
        &mut buf,
    )
    .unwrap();
    use axum::response::IntoResponse;
    (
        [(axum::http::header::CONTENT_TYPE, "application/cbor")],
        buf,
    )
        .into_response()
}

async fn facts_put_handler(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Json<serde_json::Value> {
    state
        .recorded
        .lock()
        .unwrap()
        .put_bodies
        .push(body.to_vec());
    Json(serde_json::json!({ "accepted": [0], "rejected": [] }))
}

struct StubServer {
    base_url: String,
    recorded: Arc<Mutex<Recorded>>,
    _handle: tokio::task::JoinHandle<()>,
}

async fn start_stub(known_indices: Vec<usize>) -> StubServer {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = AppState {
        recorded: recorded.clone(),
        known_indices,
    };
    let app = Router::new()
        .route("/v1/diffs", post(diffs_handler))
        .route("/v1/facts/query", post(facts_query_handler))
        .route("/v1/facts/download", post(facts_download_handler))
        .route("/v1/facts/put", post(facts_put_handler))
        .with_state(state);
    let (base_url, handle) = serve_router(app).await;
    StubServer {
        base_url,
        recorded,
        _handle: handle,
    }
}

fn run_sem_diff(repo: &Path, home: &Path, stub: &StubServer, extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sem"));
    cmd.args(["diff"])
        .current_dir(repo)
        .env("HOME", home)
        .env("SEM_TOKEN", "test-token")
        .env("SEM_CLOUD", "1")
        .env("SEM_CLOUD_ENDPOINT", &stub.base_url)
        .env("SEM_RELATIONS_BUDGET_MS", "5000")
        .env_remove("SEM_LOCAL")
        .env_remove("SEM_NO_NETWORK")
        .env_remove("SEM_RELATIONS_LOCAL")
        .env_remove("SEM_FACTS_REMOTE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("run sem diff")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_queries_and_uploads_novel_facts_for_touched_files() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    let stub = start_stub(vec![]).await; // nothing known -> forces extraction+upload
    let output = run_sem_diff(repo.path(), home.path(), &stub, &[]);
    assert!(output.status.success(), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(
        rec.query_bodies.len(),
        1,
        "expected exactly one /v1/facts/query call: {}",
        output_text(&output)
    );
    let keys = rec.query_bodies[0]["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1, "one touched file (a.ts)");
    assert_eq!(keys[0]["relativePath"], "a.ts");
    assert!(
        keys[0]["contentHash"].is_string(),
        "content_hash travels as decimal text"
    );
    // MUL phase 2: FACTS_SCHEMA_VERSION
    // bumped 1 -> 2 for `PrecomputedFileFacts::import_stmts`.
    assert_eq!(
        keys[0]["schemaVersion"],
        i64::from(sem_core::parser::facts_store::FACTS_SCHEMA_VERSION)
    );

    assert_eq!(
        rec.put_bodies.len(),
        1,
        "the unknown file must be extracted and uploaded: {}",
        output_text(&output)
    );
    // Decode the CBOR PUT body and check its shape matches sem-cloud's
    // documented wire format (FACTS-SERVICE.md).
    let decoded: ciborium::value::Value = ciborium::from_reader(&rec.put_bodies[0][..]).unwrap();
    let map = decoded.as_map().unwrap();
    let clone_url = map
        .iter()
        .find(|(k, _)| k.as_text() == Some("cloneUrl"))
        .unwrap()
        .1
        .as_text()
        .unwrap();
    assert_eq!(clone_url, "https://github.com/test/repo.git");
    let facts = map
        .iter()
        .find(|(k, _)| k.as_text() == Some("facts"))
        .unwrap()
        .1
        .as_array()
        .unwrap();
    assert_eq!(facts.len(), 1);
    let rec0 = facts[0].as_map().unwrap();
    let payload = rec0
        .iter()
        .find(|(k, _)| k.as_text() == Some("payload"))
        .unwrap()
        .1
        .as_bytes()
        .unwrap();
    let payload_value: ciborium::value::Value = ciborium::from_reader(&payload[..]).unwrap();
    let payload_map = payload_value.as_map().unwrap();
    let path = payload_map
        .iter()
        .find(|(k, _)| k.as_text() == Some("path"))
        .unwrap()
        .1
        .as_text()
        .unwrap();
    assert_eq!(path, "a.ts");
    let entities = payload_map
        .iter()
        .find(|(k, _)| k.as_text() == Some("entities"))
        .unwrap()
        .1
        .as_array()
        .unwrap();
    assert!(
        !entities.is_empty(),
        "the uploaded fact must carry the file's real extracted entities"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_skips_upload_and_downloads_instead_when_already_known() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    let stub = start_stub(vec![0]).await; // the one touched file is already known
    let output = run_sem_diff(repo.path(), home.path(), &stub, &[]);
    assert!(output.status.success(), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert_eq!(rec.query_bodies.len(), 1);
    assert!(
        rec.put_bodies.is_empty(),
        "a file the cloud already knows must never be re-extracted and re-uploaded: {}",
        output_text(&output)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sem_facts_remote_0_disables_the_whole_module() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    let stub = start_stub(vec![]).await;
    let output = run_sem_diff(
        repo.path(),
        home.path(),
        &stub,
        &[("SEM_FACTS_REMOTE", "0")],
    );
    assert!(output.status.success(), "{}", output_text(&output));

    let rec = stub.recorded.lock().unwrap();
    assert!(
        rec.query_bodies.is_empty(),
        "SEM_FACTS_REMOTE=0 must reach no /v1/facts endpoint at all: {}",
        output_text(&output)
    );
    assert!(rec.put_bodies.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_upload_never_fails_the_diff_command() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    change_source(repo.path());

    // Stub server whose /v1/facts/put always 500s -- the diff command must
    // still succeed and print its normal output.
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = AppState {
        recorded: recorded.clone(),
        known_indices: vec![],
    };
    async fn failing_put() -> axum::http::StatusCode {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    }
    let app = Router::new()
        .route("/v1/diffs", post(diffs_handler))
        .route("/v1/facts/query", post(facts_query_handler))
        .route("/v1/facts/put", post(failing_put))
        .with_state(state);
    let (base_url, _handle) = serve_router(app).await;
    let stub = StubServer {
        base_url,
        recorded,
        _handle,
    };

    let output = run_sem_diff(repo.path(), home.path(), &stub, &[]);
    assert!(
        output.status.success(),
        "a facts-upload failure must never fail the diff command: {}",
        output_text(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("source"), "{}", output_text(&output));
}
