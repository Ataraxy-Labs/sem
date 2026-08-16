//! Client for the sem-cloud "agent review listener" API.
//!
//! Lets an MCP-connected agent join a hosted code review as a live listener:
//! long-poll for reviewer questions attached to lines of a diff ("branches"),
//! and stream answers back. This is the client layer only — the MCP tools
//! that drive the listener loop (`join_review`, `wait_for_branch`,
//! `reply_to_branch`) live in `server.rs` and call through here.
//!
//! Config is env-first so a headless/demo run never touches the operator's
//! real `sem login` credentials: `SEM_CLOUD_URL` and `SEM_API_KEY` override
//! everything; `~/.sem/credentials.json` (via `sem_cloud_client`) is only
//! consulted for whichever of the two is left unset.
//!
//! Contract (sem-cloud, built in parallel with this client — endpoints may
//! 404 until the server side lands):
//! - `GET  /v1/diffs/{id}/agent/next?wait_ms=N` (N <= 50000) — long-poll.
//!   `{"status":"branch","branch":{...}}` or `{"status":"timeout"}`.
//! - `POST /v1/diffs/{id}/agent/reply` — `{"commentId","content","partial"?}`.
//! - `POST /v1/diffs/{id}/agent/presence` — `{"status","label"?}`.
//! - `GET  /v1/diffs/{id}/manifest` — review summary (existing endpoint).
//!
//! All requests are Bearer-authenticated. Every HTTP call here is blocking
//! (`ureq`, matching the rest of this crate's cloud client); the MCP tool
//! handlers in `server.rs` run these on `tokio::task::spawn_blocking` so a
//! long poll (up to 50s) never parks an async worker thread.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Long-poll waits longer than the rest of the calls, which are effectively
/// instantaneous; give the underlying HTTP client enough rope for the
/// longest legal `wait_ms` (50s) plus network slack.
const LONG_POLL_TIMEOUT_SLACK: Duration = Duration::from_secs(10);
/// Timeout for the non-long-poll calls (reply/presence/manifest).
const SHORT_CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Server-side cap on `wait_ms` for `agent/next`.
pub const MAX_WAIT_MS: u64 = 50_000;

// ─── Config ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AgentReviewConfig {
    pub base_url: String,
    pub api_key: String,
}

impl AgentReviewConfig {
    /// Resolve config env-first: `SEM_CLOUD_URL` / `SEM_API_KEY` override
    /// everything; `~/.sem/credentials.json` fills in whichever is unset.
    /// Errors only when no API key can be found anywhere (a base URL always
    /// resolves, falling back to the sem-cloud default endpoint).
    pub fn from_env_or_credentials() -> Result<Self, String> {
        Self::resolve(
            |k| std::env::var(k).ok(),
            sem_cloud_client::load_credentials,
        )
    }

    /// Testable core of `from_env_or_credentials`: takes the env lookup and
    /// credentials loader as functions so tests can supply fakes instead of
    /// mutating real process env / `$HOME`.
    fn resolve(
        env_var: impl Fn(&str) -> Option<String>,
        load_credentials: impl Fn() -> Option<sem_cloud_client::CloudCredentials>,
    ) -> Result<Self, String> {
        let non_empty = |s: String| -> Option<String> { (!s.is_empty()).then_some(s) };
        let creds = load_credentials();

        let base_url = env_var("SEM_CLOUD_URL")
            .and_then(non_empty)
            .or_else(|| creds.as_ref().map(|c| c.endpoint.clone()))
            .unwrap_or_else(sem_cloud_client::default_endpoint);

        let api_key = env_var("SEM_API_KEY")
            .and_then(non_empty)
            .or_else(|| creds.as_ref().map(|c| c.api_key.clone()))
            .ok_or_else(|| {
                "No sem-cloud API key found. Set SEM_API_KEY (or SEM_CLOUD_URL) in the \
                 environment, or run `sem login` to write ~/.sem/credentials.json."
                    .to_string()
            })?;

        Ok(Self { base_url, api_key })
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AgentReviewError {
    /// Config could not be resolved (no API key anywhere).
    Config(String),
    /// Server answered with a non-2xx status.
    Http { status: u16, body: String },
    /// Network-level failure (DNS, connect, timeout, ...).
    Transport(String),
    /// 2xx response body didn't match the expected shape.
    Decode(String),
}

impl fmt::Display for AgentReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentReviewError::Config(msg) => write!(f, "{msg}"),
            AgentReviewError::Http { status, body } => {
                let body = body.trim();
                if body.is_empty() {
                    write!(f, "sem-cloud returned HTTP {status}")
                } else {
                    write!(f, "sem-cloud returned HTTP {status}: {body}")
                }
            }
            AgentReviewError::Transport(msg) => write!(f, "sem-cloud unreachable: {msg}"),
            AgentReviewError::Decode(msg) => {
                write!(f, "sem-cloud sent an unexpected response: {msg}")
            }
        }
    }
}

impl std::error::Error for AgentReviewError {}

impl AgentReviewError {
    /// True when this error means the call's TARGET is permanently gone or
    /// already settled from under us — never worth retrying the same call.
    /// Learned from the rabbithole study: a listener loop that treats every
    /// error the same way can wedge forever retrying a call that will never
    /// succeed (the comment it's replying to was deleted, or another
    /// listener already answered/claimed it). Terminal today: 404 (comment
    /// or diff not found) and 409 (conflict — already answered, or a stale
    /// lease claimed by someone else). Deliberately NOT terminal: network
    /// failures and 5xx, which are transient and worth retrying; also not
    /// terminal: any other 4xx (e.g. 400/401/403), which usually means a
    /// bug in the caller's own request rather than a vanished target, so it
    /// should surface as a real error instead of being silently swallowed.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, AgentReviewError::Http { status, .. } if *status == 404 || *status == 409)
    }
}

impl From<ureq::Error> for AgentReviewError {
    fn from(err: ureq::Error) -> Self {
        match err {
            ureq::Error::Status(status, resp) => {
                let body = resp.into_string().unwrap_or_default();
                AgentReviewError::Http { status, body }
            }
            ureq::Error::Transport(t) => AgentReviewError::Transport(t.to_string()),
        }
    }
}

/// `unanswerable_result` / `review_gone_result`: the typed terminal-result
/// builders for `reply_to_branch` / `wait_for_branch` respectively. Defined
/// in [`crate::review_protocol`] alongside the rest of the review-listener
/// loop-discipline text (`server.rs`'s tool handlers need the same two
/// functions this module's own tests below do), and re-exported here so
/// existing `agent_review::unanswerable_result` / `agent_review::review_gone_result`
/// call sites keep working unchanged.
pub(crate) use crate::review_protocol::{review_gone_result, unanswerable_result};

// ─── Wire types ──────────────────────────────────────────────────────────

// AgentNext/Branch/BranchContext/BranchCallEdge: no external (cross-crate)
// consumer — sem-cli never references these types by name. They're consumed
// entirely within sem-mcp, and only via whole-struct serde serialization
// (server.rs's `wait_for_branch` tool does
// `serde_json::json!({"branch": branch})`, so every field including the
// nested BranchContext/BranchCallEdge does get consumed — by the MCP client
// on the other end of that tool call, not by Rust field access). pub(crate)
// is the real boundary; serde's derived impls live in this same module
// regardless of field visibility, so narrowing doesn't touch (de)serialization.

/// One endpoint of a call edge attached to a branch's diff context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BranchCallEdge {
    pub(crate) name: String,
    pub(crate) file: String,
}

/// Repository context sem-cloud attached to the branch: the raw patch plus
/// the callers/callees of the entity the comment is anchored to. All optional
/// since sem-cloud may not always be able to compute them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct BranchContext {
    #[serde(default)]
    pub(crate) patch: Option<String>,
    #[serde(default)]
    pub(crate) callers: Vec<BranchCallEdge>,
    #[serde(default)]
    pub(crate) callees: Vec<BranchCallEdge>,
}

/// A reviewer's question anchored to a line of a diff, as delivered by
/// `GET /v1/diffs/{id}/agent/next`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Branch {
    #[serde(rename = "commentId")]
    pub(crate) comment_id: String,
    pub(crate) text: String,
    #[serde(rename = "entityId", default)]
    pub(crate) entity_id: Option<String>,
    #[serde(default)]
    pub(crate) side: Option<String>,
    #[serde(default)]
    pub(crate) line: Option<u64>,
    #[serde(rename = "filePath", default)]
    pub(crate) file_path: Option<String>,
    #[serde(default)]
    pub(crate) state: Option<String>,
    #[serde(default)]
    pub(crate) context: Option<BranchContext>,
}

/// Result of one `agent/next` long-poll. `Branch` is boxed since it's much
/// larger than the unit `Timeout` variant (the common case on an idle
/// listener), keeping the enum itself cheap to move around.
#[derive(Debug, Clone)]
pub(crate) enum AgentNext {
    Branch(Box<Branch>),
    Timeout,
}

/// Raw wire shape of one comment from `GET /v1/diffs/{id}/comments`
/// (sem-cloud's `DiffComment`, camelCase). Deserialized defensively (every
/// field but `id`/`text` optional) since this endpoint is server-owned and
/// only its shape as observed is asserted here.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    id: String,
    #[serde(default)]
    entity_id: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    line: Option<i64>,
    text: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCommentsResponse {
    comments: Vec<RawComment>,
}

/// Max characters kept in `OpenBranch::excerpt` before truncation.
const EXCERPT_MAX_CHARS: usize = 160;

fn truncate_excerpt(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}…")
}

/// One open (or being-answered) root question, as returned by
/// `list_open_branches` — a read-only reconciliation view, never a claim.
///
/// No external (cross-crate) consumer — same pub(crate) rationale as
/// `AgentNext`/`Branch` above: consumed only within sem-mcp
/// (server.rs's `list_open_branches` tool serializes `Vec<OpenBranch>`
/// wholesale into the MCP response).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OpenBranch {
    pub(crate) id: String,
    /// The entity the comment is anchored to, if any (falls back to the
    /// file path when there's no entity, since either is useful context).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) entity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<i64>,
    /// Truncated to `EXCERPT_MAX_CHARS` — this is a reconciliation list, not
    /// a full read of every question's text.
    pub(crate) excerpt: String,
    /// "open" or "answering" — every other state is filtered out before
    /// this type is constructed.
    pub(crate) state: String,
}

impl From<RawComment> for OpenBranch {
    fn from(c: RawComment) -> Self {
        OpenBranch {
            id: c.id,
            entity: c.entity_id.or(c.file_path),
            line: c.line,
            excerpt: truncate_excerpt(&c.text, EXCERPT_MAX_CHARS),
            state: c.state.unwrap_or_default(),
        }
    }
}

// ─── Client ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AgentReviewClient {
    config: AgentReviewConfig,
    agent: ureq::Agent,
}

impl AgentReviewClient {
    pub fn new(config: AgentReviewConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(SHORT_CALL_TIMEOUT)
            .build();
        Self { config, agent }
    }

    pub(crate) fn from_env_or_credentials() -> Result<Self, AgentReviewError> {
        let config =
            AgentReviewConfig::from_env_or_credentials().map_err(AgentReviewError::Config)?;
        Ok(Self::new(config))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url.trim_end_matches('/'), path)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.api_key)
    }

    /// Long-poll for the next reviewer question on `diff_id`. `wait_ms` is
    /// clamped to the server's documented cap (50000ms).
    pub(crate) fn next_branch(
        &self,
        diff_id: &str,
        wait_ms: u64,
    ) -> Result<AgentNext, AgentReviewError> {
        let wait_ms = wait_ms.min(MAX_WAIT_MS);
        let url = self.url(&format!("/v1/diffs/{diff_id}/agent/next?wait_ms={wait_ms}"));
        let resp = self
            .agent
            .get(&url)
            .set("Authorization", &self.auth_header())
            .timeout(Duration::from_millis(wait_ms) + LONG_POLL_TIMEOUT_SLACK)
            .call()?;
        let value: Value = resp
            .into_json()
            .map_err(|e| AgentReviewError::Decode(e.to_string()))?;
        parse_agent_next(value)
    }

    /// Post an answer to a branch. `partial: Some(true)` streams (content is
    /// the full cumulative answer so far); `None`/`Some(false)` commits it.
    pub(crate) fn reply(
        &self,
        diff_id: &str,
        comment_id: &str,
        content: &str,
        partial: Option<bool>,
    ) -> Result<(), AgentReviewError> {
        let url = self.url(&format!("/v1/diffs/{diff_id}/agent/reply"));
        let mut body = serde_json::json!({ "commentId": comment_id, "content": content });
        if let Some(p) = partial {
            body["partial"] = serde_json::json!(p);
        }
        self.agent
            .post(&url)
            .set("Authorization", &self.auth_header())
            .send_json(body)?;
        Ok(())
    }

    /// Announce (or retract) this session's presence as a review listener.
    pub(crate) fn presence(
        &self,
        diff_id: &str,
        status: &str,
        label: Option<&str>,
    ) -> Result<(), AgentReviewError> {
        let url = self.url(&format!("/v1/diffs/{diff_id}/agent/presence"));
        let mut body = serde_json::json!({ "status": status });
        if let Some(label) = label {
            body["label"] = serde_json::json!(label);
        }
        self.agent
            .post(&url)
            .set("Authorization", &self.auth_header())
            .send_json(body)?;
        Ok(())
    }

    /// Fetch the review manifest. Returned as raw JSON (rather than a typed
    /// struct) because the manifest endpoint predates this client and its
    /// exact shape is defined server-side; `render_manifest_summary` below
    /// extracts common fields defensively so an unexpected shape degrades to
    /// a short summary instead of a hard failure.
    pub fn manifest(&self, diff_id: &str) -> Result<Value, AgentReviewError> {
        let url = self.url(&format!("/v1/diffs/{diff_id}/manifest"));
        let resp = self
            .agent
            .get(&url)
            .set("Authorization", &self.auth_header())
            .call()?;
        resp.into_json()
            .map_err(|e| AgentReviewError::Decode(e.to_string()))
    }

    /// Read-only backlog view: every ROOT comment (never a reply) whose
    /// state is `open` or `answering`, WITHOUT claiming anything — this
    /// hits `GET /v1/diffs/{id}/comments`, the same plain read the Stop-hook
    /// backstop uses, not `agent/next` (which would claim the oldest one on
    /// the caller's behalf). Lets a confused/resumed agent reconcile what's
    /// still waiting before deciding what to do next.
    pub(crate) fn list_open_branches(
        &self,
        diff_id: &str,
    ) -> Result<Vec<OpenBranch>, AgentReviewError> {
        let url = self.url(&format!("/v1/diffs/{diff_id}/comments"));
        let resp = self
            .agent
            .get(&url)
            .set("Authorization", &self.auth_header())
            .call()?;
        let parsed: RawCommentsResponse = resp
            .into_json()
            .map_err(|e| AgentReviewError::Decode(e.to_string()))?;
        Ok(parsed
            .comments
            .into_iter()
            .filter(|c| c.parent_id.is_none())
            .filter(|c| matches!(c.state.as_deref(), Some("open") | Some("answering")))
            .map(OpenBranch::from)
            .collect())
    }
}

fn parse_agent_next(value: Value) -> Result<AgentNext, AgentReviewError> {
    match value.get("status").and_then(Value::as_str) {
        Some("branch") => {
            let branch_val = value.get("branch").cloned().ok_or_else(|| {
                AgentReviewError::Decode("status=branch but no `branch` field".into())
            })?;
            let branch: Branch = serde_json::from_value(branch_val)
                .map_err(|e| AgentReviewError::Decode(format!("malformed branch: {e}")))?;
            Ok(AgentNext::Branch(Box::new(branch)))
        }
        Some("timeout") => Ok(AgentNext::Timeout),
        Some(other) => Err(AgentReviewError::Decode(format!(
            "unknown status \"{other}\""
        ))),
        None => Err(AgentReviewError::Decode("missing `status` field".into())),
    }
}

/// Best-effort human-readable summary of a review manifest for `join_review`
/// to hand back to the agent. Defensive about field names/shape since the
/// manifest endpoint is server-owned and may evolve independently of this
/// client: known fields render nicely, anything unrecognized still shows up
/// as compact JSON rather than being silently dropped.
pub(crate) fn render_manifest_summary(diff_id: &str, manifest: &Value) -> String {
    let label = manifest
        .get("label")
        .or_else(|| manifest.get("title"))
        .or_else(|| manifest.get("name"))
        .and_then(Value::as_str);

    let files = manifest
        .get("files")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .or_else(|| {
            manifest
                .get("fileCount")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
        });

    let comments = manifest
        .get("comments")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .or_else(|| {
            manifest
                .get("commentCount")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
        });

    let mut out = format!("Joined review {diff_id}");
    if let Some(label) = label {
        out.push_str(&format!(" — {label}"));
    }
    out.push('\n');
    if files.is_some() || comments.is_some() {
        out.push_str("Manifest: ");
        let mut parts = Vec::new();
        if let Some(f) = files {
            parts.push(format!("{f} files"));
        }
        if let Some(c) = comments {
            parts.push(format!("{c} comments"));
        }
        out.push_str(&parts.join(", "));
        out.push('\n');
    } else {
        out.push_str(&format!(
            "Manifest: {}\n",
            serde_json::to_string(manifest).unwrap_or_default()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = vars.iter().copied().collect();
        move |k: &str| map.get(k).map(|v| v.to_string())
    }

    #[test]
    fn config_prefers_env_over_credentials() {
        let cfg = AgentReviewConfig::resolve(
            env(&[
                ("SEM_CLOUD_URL", "http://127.0.0.1:8080"),
                ("SEM_API_KEY", "env-key"),
            ]),
            || {
                Some(sem_cloud_client::CloudCredentials {
                    api_key: "file-key".into(),
                    endpoint: "https://sem-cloud.fly.dev".into(),
                })
            },
        )
        .unwrap();

        assert_eq!(cfg.base_url, "http://127.0.0.1:8080");
        assert_eq!(cfg.api_key, "env-key");
    }

    #[test]
    fn config_falls_back_to_credentials_file_per_field() {
        // Only SEM_API_KEY set in env; base_url must come from the creds file.
        let cfg = AgentReviewConfig::resolve(env(&[("SEM_API_KEY", "env-key")]), || {
            Some(sem_cloud_client::CloudCredentials {
                api_key: "file-key".into(),
                endpoint: "https://file.example".into(),
            })
        })
        .unwrap();

        assert_eq!(cfg.base_url, "https://file.example");
        assert_eq!(cfg.api_key, "env-key");
    }

    #[test]
    fn config_errors_without_any_api_key() {
        let err = AgentReviewConfig::resolve(env(&[]), || None).unwrap_err();
        assert!(err.contains("SEM_API_KEY"));
    }

    #[test]
    fn config_ignores_empty_env_vars() {
        // An explicitly-set-but-empty env var should not shadow the creds file.
        let cfg =
            AgentReviewConfig::resolve(env(&[("SEM_CLOUD_URL", ""), ("SEM_API_KEY", "")]), || {
                Some(sem_cloud_client::CloudCredentials {
                    api_key: "file-key".into(),
                    endpoint: "https://file.example".into(),
                })
            })
            .unwrap();

        assert_eq!(cfg.base_url, "https://file.example");
        assert_eq!(cfg.api_key, "file-key");
    }

    #[test]
    fn parse_agent_next_branch() {
        let value = serde_json::json!({
            "status": "branch",
            "branch": {
                "commentId": "c1",
                "text": "why here?",
                "entityId": "e1",
                "side": "right",
                "line": 42,
                "filePath": "src/lib.rs",
                "state": "open",
                "context": {
                    "patch": "+foo",
                    "callers": [{"name": "a", "file": "a.rs"}],
                    "callees": []
                }
            }
        });
        match parse_agent_next(value).unwrap() {
            AgentNext::Branch(b) => {
                assert_eq!(b.comment_id, "c1");
                assert_eq!(b.text, "why here?");
                assert_eq!(b.line, Some(42));
                let ctx = b.context.unwrap();
                assert_eq!(ctx.patch.as_deref(), Some("+foo"));
                assert_eq!(ctx.callers.len(), 1);
            }
            AgentNext::Timeout => panic!("expected branch"),
        }
    }

    #[test]
    fn parse_agent_next_branch_tolerates_missing_optional_fields() {
        // Only commentId + text are guaranteed; everything else may be absent.
        let value = serde_json::json!({
            "status": "branch",
            "branch": { "commentId": "c1", "text": "hi" }
        });
        match parse_agent_next(value).unwrap() {
            AgentNext::Branch(b) => {
                assert_eq!(b.comment_id, "c1");
                assert!(b.context.is_none());
                assert!(b.line.is_none());
            }
            AgentNext::Timeout => panic!("expected branch"),
        }
    }

    #[test]
    fn parse_agent_next_timeout() {
        let value = serde_json::json!({ "status": "timeout" });
        assert!(matches!(
            parse_agent_next(value).unwrap(),
            AgentNext::Timeout
        ));
    }

    #[test]
    fn parse_agent_next_rejects_unknown_status() {
        let value = serde_json::json!({ "status": "bogus" });
        assert!(parse_agent_next(value).is_err());
    }

    #[test]
    fn render_manifest_summary_uses_known_fields() {
        let manifest = serde_json::json!({
            "label": "Add rabbithole UX",
            "files": ["a.rs", "b.rs"],
            "comments": [1, 2, 3]
        });
        let out = render_manifest_summary("d1", &manifest);
        assert!(out.contains("Add rabbithole UX"));
        assert!(out.contains("2 files"));
        assert!(out.contains("3 comments"));
    }

    #[test]
    fn render_manifest_summary_degrades_gracefully_for_unknown_shape() {
        let manifest = serde_json::json!({ "somethingElse": 1 });
        let out = render_manifest_summary("d1", &manifest);
        assert!(out.contains("Joined review d1"));
        assert!(out.contains("somethingElse"));
    }

    // ── Integration tests against a real stub HTTP server ──
    //
    // Spin up a tiny axum server on a random port and drive the blocking
    // ureq-based client against it, covering the branch/timeout/reply paths
    // end to end (request shape sent, response shape parsed).

    mod stub_server {
        use axum::extract::{Path, Query, State};
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tokio::net::TcpListener;

        // Test-only, and only ever consumed within this file's `tests`
        // module (no external or cross-crate use) — pub(crate) throughout.
        #[derive(Default)]
        pub(crate) struct Recorded {
            pub(crate) next_calls: Vec<(String, String)>, // (diff_id, wait_ms)
            pub(crate) replies: Vec<serde_json::Value>,
            pub(crate) presences: Vec<serde_json::Value>,
            pub(crate) auth_headers: Vec<String>,
        }

        pub(crate) struct StubServer {
            pub(crate) base_url: String,
            pub(crate) recorded: Arc<Mutex<Recorded>>,
            /// Queued responses for consecutive `agent/next` calls, popped
            /// front-to-back; the last one repeats once exhausted.
            _handle: tokio::task::JoinHandle<()>,
        }

        pub(crate) async fn start(next_responses: Vec<serde_json::Value>) -> StubServer {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let queue = Arc::new(Mutex::new(next_responses));

            #[derive(Clone)]
            struct AppState {
                recorded: Arc<Mutex<Recorded>>,
                queue: Arc<Mutex<Vec<serde_json::Value>>>,
            }

            async fn next_handler(
                State(state): State<AppState>,
                Path(diff_id): Path<String>,
                Query(params): Query<HashMap<String, String>>,
                headers: axum::http::HeaderMap,
            ) -> Json<serde_json::Value> {
                let wait_ms = params.get("wait_ms").cloned().unwrap_or_default();
                let mut rec = state.recorded.lock().unwrap();
                rec.next_calls.push((diff_id, wait_ms));
                if let Some(auth) = headers.get("authorization") {
                    rec.auth_headers
                        .push(auth.to_str().unwrap_or_default().to_string());
                }
                drop(rec);
                let mut q = state.queue.lock().unwrap();
                let resp = if q.len() > 1 {
                    q.remove(0)
                } else {
                    q.first()
                        .cloned()
                        .unwrap_or(serde_json::json!({"status": "timeout"}))
                };
                Json(resp)
            }

            async fn reply_handler(
                State(state): State<AppState>,
                Path(_diff_id): Path<String>,
                Json(body): Json<serde_json::Value>,
            ) -> Json<serde_json::Value> {
                state.recorded.lock().unwrap().replies.push(body);
                Json(serde_json::json!({"ok": true}))
            }

            async fn presence_handler(
                State(state): State<AppState>,
                Path(_diff_id): Path<String>,
                Json(body): Json<serde_json::Value>,
            ) -> Json<serde_json::Value> {
                state.recorded.lock().unwrap().presences.push(body);
                Json(serde_json::json!({"ok": true}))
            }

            async fn manifest_handler(Path(diff_id): Path<String>) -> Json<serde_json::Value> {
                Json(serde_json::json!({
                    "label": format!("review {diff_id}"),
                    "files": ["a.rs", "b.rs", "c.rs"],
                    "comments": []
                }))
            }

            let state = AppState {
                recorded: recorded.clone(),
                queue,
            };
            let app = Router::new()
                .route("/v1/diffs/{diff_id}/agent/next", get(next_handler))
                .route("/v1/diffs/{diff_id}/agent/reply", post(reply_handler))
                .route("/v1/diffs/{diff_id}/agent/presence", post(presence_handler))
                .route("/v1/diffs/{diff_id}/manifest", get(manifest_handler))
                .with_state(state);

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            StubServer {
                base_url: format!("http://{addr}"),
                recorded,
                _handle: handle,
            }
        }
    }

    fn test_client(base_url: &str) -> AgentReviewClient {
        AgentReviewClient::new(AgentReviewConfig {
            base_url: base_url.to_string(),
            api_key: "test-key".to_string(),
        })
    }

    async fn start_with_router(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    /// A stub `agent/reply` route that always answers with a fixed status —
    /// for exercising `AgentReviewError::is_terminal` against the terminal
    /// (404/409) and non-terminal (5xx) cases without a real sem-cloud.
    async fn start_reply_status(
        status: axum::http::StatusCode,
    ) -> (String, tokio::task::JoinHandle<()>) {
        async fn handler(
            axum::extract::State(status): axum::extract::State<axum::http::StatusCode>,
        ) -> axum::http::StatusCode {
            status
        }
        let app = axum::Router::new()
            .route(
                "/v1/diffs/{diff_id}/agent/reply",
                axum::routing::post(handler),
            )
            .with_state(status);
        start_with_router(app).await
    }

    /// A stub `GET /v1/diffs/{id}/comments` route that always answers with
    /// the given body — for `list_open_branches`.
    async fn start_comments_body(body: serde_json::Value) -> (String, tokio::task::JoinHandle<()>) {
        async fn handler(
            axum::extract::State(body): axum::extract::State<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            axum::Json(body)
        }
        let app = axum::Router::new()
            .route("/v1/diffs/{diff_id}/comments", axum::routing::get(handler))
            .with_state(body);
        start_with_router(app).await
    }

    #[tokio::test]
    async fn next_branch_parses_branch_response_from_stub_server() {
        let branch_json = serde_json::json!({
            "status": "branch",
            "branch": {
                "commentId": "c42",
                "text": "why is this unwrap safe?",
                "entityId": "e1",
                "side": "right",
                "line": 10,
                "filePath": "src/main.rs",
                "state": "open",
                "context": { "patch": "+ .unwrap()", "callers": [], "callees": [] }
            }
        });
        let server = stub_server::start(vec![branch_json]).await;
        let client = test_client(&server.base_url);

        let result = tokio::task::spawn_blocking(move || client.next_branch("diff1", 1000))
            .await
            .unwrap();

        match result.unwrap() {
            AgentNext::Branch(b) => {
                assert_eq!(b.comment_id, "c42");
                assert_eq!(b.file_path.as_deref(), Some("src/main.rs"));
            }
            AgentNext::Timeout => panic!("expected a branch"),
        }

        let rec = server.recorded.lock().unwrap();
        assert_eq!(
            rec.next_calls,
            vec![("diff1".to_string(), "1000".to_string())]
        );
        assert_eq!(rec.auth_headers, vec!["Bearer test-key".to_string()]);
    }

    #[tokio::test]
    async fn next_branch_parses_timeout_response_from_stub_server() {
        let server = stub_server::start(vec![serde_json::json!({"status": "timeout"})]).await;
        let client = test_client(&server.base_url);

        let result = tokio::task::spawn_blocking(move || client.next_branch("diff1", 500))
            .await
            .unwrap();

        assert!(matches!(result.unwrap(), AgentNext::Timeout));
    }

    #[tokio::test]
    async fn next_branch_clamps_wait_ms_to_server_cap() {
        let server = stub_server::start(vec![serde_json::json!({"status": "timeout"})]).await;
        let client = test_client(&server.base_url);

        let _ = tokio::task::spawn_blocking(move || client.next_branch("diff1", 999_999))
            .await
            .unwrap();

        let rec = server.recorded.lock().unwrap();
        assert_eq!(rec.next_calls[0].1, MAX_WAIT_MS.to_string());
    }

    #[tokio::test]
    async fn reply_sends_partial_flag_and_cumulative_content() {
        let server = stub_server::start(vec![serde_json::json!({"status": "timeout"})]).await;
        let client = test_client(&server.base_url);

        let c1 = client.clone();
        tokio::task::spawn_blocking(move || c1.reply("diff1", "c42", "So far so good", Some(true)))
            .await
            .unwrap()
            .unwrap();
        let c2 = client.clone();
        tokio::task::spawn_blocking(move || {
            c2.reply("diff1", "c42", "So far so good, done.", None)
        })
        .await
        .unwrap()
        .unwrap();

        let rec = server.recorded.lock().unwrap();
        assert_eq!(rec.replies.len(), 2);
        assert_eq!(rec.replies[0]["partial"], serde_json::json!(true));
        assert_eq!(
            rec.replies[0]["content"],
            serde_json::json!("So far so good")
        );
        assert_eq!(rec.replies[1].get("partial"), None);
        assert_eq!(
            rec.replies[1]["content"],
            serde_json::json!("So far so good, done.")
        );
    }

    #[tokio::test]
    async fn presence_sends_status_and_label() {
        let server = stub_server::start(vec![serde_json::json!({"status": "timeout"})]).await;
        let client = test_client(&server.base_url);

        tokio::task::spawn_blocking(move || {
            client.presence("diff1", "listening", Some("claude-code"))
        })
        .await
        .unwrap()
        .unwrap();

        let rec = server.recorded.lock().unwrap();
        assert_eq!(rec.presences.len(), 1);
        assert_eq!(rec.presences[0]["status"], serde_json::json!("listening"));
        assert_eq!(rec.presences[0]["label"], serde_json::json!("claude-code"));
    }

    #[tokio::test]
    async fn manifest_round_trips_through_render_summary() {
        let server = stub_server::start(vec![serde_json::json!({"status": "timeout"})]).await;
        let client = test_client(&server.base_url);

        let value = tokio::task::spawn_blocking(move || client.manifest("diff1"))
            .await
            .unwrap()
            .unwrap();

        let summary = render_manifest_summary("diff1", &value);
        assert!(summary.contains("review diff1"));
        assert!(summary.contains("3 files"));
    }

    #[tokio::test]
    async fn next_branch_surfaces_404_as_http_error() {
        // Simulates the "sem-cloud endpoint doesn't exist yet" case: a bare
        // router with none of the agent routes wired up.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new()).await.unwrap();
        });
        let client = test_client(&format!("http://{addr}"));

        let result = tokio::task::spawn_blocking(move || client.next_branch("diff1", 500))
            .await
            .unwrap();

        match result {
            Err(AgentReviewError::Http { status, .. }) => assert_eq!(status, 404),
            other => panic!("expected Http{{404}}, got {other:?}"),
        }
    }

    // ── (a) UNWEDGE: reply_to_branch terminal-vs-transient classification ──

    #[tokio::test]
    async fn reply_404_comment_deleted_is_terminal_and_unanswerable() {
        let (base_url, _handle) = start_reply_status(axum::http::StatusCode::NOT_FOUND).await;
        let client = test_client(&base_url);

        let result =
            tokio::task::spawn_blocking(move || client.reply("diff1", "c1", "answer", None))
                .await
                .unwrap();

        let err = result.expect_err("expected an error");
        assert!(
            err.is_terminal(),
            "404 (comment deleted) must be terminal: {err:?}"
        );

        let unanswerable = unanswerable_result(&err);
        assert_eq!(unanswerable["status"], serde_json::json!("unanswerable"));
        assert_eq!(
            unanswerable["instruction"],
            serde_json::json!("Call wait_for_branch again now.")
        );
        assert!(unanswerable["reason"].as_str().unwrap().contains("404"));
    }

    #[tokio::test]
    async fn reply_409_conflict_is_terminal() {
        // Already-answered / stale-lease conflict.
        let (base_url, _handle) = start_reply_status(axum::http::StatusCode::CONFLICT).await;
        let client = test_client(&base_url);

        let result =
            tokio::task::spawn_blocking(move || client.reply("diff1", "c1", "answer", None))
                .await
                .unwrap();

        let err = result.expect_err("expected an error");
        assert!(
            err.is_terminal(),
            "409 (already answered / stale lease) must be terminal: {err:?}"
        );
    }

    #[tokio::test]
    async fn reply_5xx_is_not_terminal() {
        let (base_url, _handle) =
            start_reply_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let client = test_client(&base_url);

        let result =
            tokio::task::spawn_blocking(move || client.reply("diff1", "c1", "answer", None))
                .await
                .unwrap();

        let err = result.expect_err("expected an error");
        assert!(
            !err.is_terminal(),
            "5xx is transient and must NOT be terminal: {err:?}"
        );
    }

    #[tokio::test]
    async fn reply_network_failure_is_not_terminal() {
        // Nothing listening on this port: a pure transport failure, not an
        // HTTP status at all.
        let client = test_client("http://127.0.0.1:1");

        let result =
            tokio::task::spawn_blocking(move || client.reply("diff1", "c1", "answer", None))
                .await
                .unwrap();

        let err = result.expect_err("expected an error");
        assert!(matches!(err, AgentReviewError::Transport(_)));
        assert!(
            !err.is_terminal(),
            "a transport failure must NOT be terminal: {err:?}"
        );
    }

    // ── (c) wait_for_branch: diff-gone 404 -> review_gone ──

    #[tokio::test]
    async fn next_branch_404_is_the_review_gone_case() {
        // Same bare-router 404 as next_branch_surfaces_404_as_http_error
        // above, but asserting the half that matters for wait_for_branch:
        // a 404 on agent/next means the DIFF itself vanished (expired or
        // deleted), which maps to review_gone — the one legitimate
        // self-stop — not a generic retry-me error.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new()).await.unwrap();
        });
        let client = test_client(&format!("http://{addr}"));

        let result = tokio::task::spawn_blocking(move || client.next_branch("diff1", 500))
            .await
            .unwrap();
        let err = result.expect_err("expected an error");
        assert!(matches!(&err, AgentReviewError::Http { status, .. } if *status == 404));

        let gone = review_gone_result();
        assert_eq!(gone["status"], serde_json::json!("review_gone"));
        assert_eq!(
            gone["instruction"],
            serde_json::json!("Stop listening; the review no longer exists.")
        );
    }

    /// The realistic shape of `review_gone`: not a first-call 404 (already
    /// covered above), but a diff that vanishes partway through an
    /// otherwise-normal listener loop — timeout, then a branch, THEN the
    /// diff disappears. Regresses against a client (or server route) that
    /// only detects `review_gone` on the very first call.
    #[tokio::test]
    async fn next_branch_404_mid_loop_after_earlier_successful_polls_is_still_review_gone() {
        use axum::extract::{Path, State};
        use axum::Json;
        use std::sync::atomic::{AtomicU32, Ordering};

        #[derive(Clone)]
        struct MidLoopState(std::sync::Arc<AtomicU32>);

        async fn mid_loop_handler(
            State(state): State<MidLoopState>,
            Path(_diff_id): Path<String>,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let call = state.0.fetch_add(1, Ordering::SeqCst);
            match call {
                // Call 1: normal idle poll.
                0 => Json(serde_json::json!({"status": "timeout"})).into_response(),
                // Call 2: a real question arrives, same as any other poll.
                1 => Json(serde_json::json!({
                    "status": "branch",
                    "branch": {"commentId": "c1", "text": "why here?"}
                }))
                .into_response(),
                // Call 3+: the diff was deleted/expired out from under the
                // still-listening loop.
                _ => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        }

        let state = MidLoopState(std::sync::Arc::new(AtomicU32::new(0)));
        let app = axum::Router::new()
            .route(
                "/v1/diffs/{diff_id}/agent/next",
                axum::routing::get(mid_loop_handler),
            )
            .with_state(state);
        let (base_url, _handle) = start_with_router(app).await;
        let client = test_client(&base_url);

        // Call 1: timeout — the normal idle state, loop continues.
        let c1 = client.clone();
        let r1 = tokio::task::spawn_blocking(move || c1.next_branch("diff1", 500))
            .await
            .unwrap();
        assert!(
            matches!(r1, Ok(AgentNext::Timeout)),
            "call 1 should be a normal timeout: {r1:?}"
        );

        // Call 2: a branch arrives — still nothing review_gone about this.
        let c2 = client.clone();
        let r2 = tokio::task::spawn_blocking(move || c2.next_branch("diff1", 500))
            .await
            .unwrap();
        match r2 {
            Ok(AgentNext::Branch(b)) => assert_eq!(b.comment_id, "c1"),
            other => panic!("call 2 should be a branch: {other:?}"),
        }

        // Call 3: the diff is gone. This — not call 1 — is what must map to
        // review_gone; a listener loop's whole point is surviving many
        // calls before this eventually happens.
        let c3 = client.clone();
        let r3 = tokio::task::spawn_blocking(move || c3.next_branch("diff1", 500))
            .await
            .unwrap();
        let err = r3.expect_err("call 3 should error");
        assert!(
            matches!(&err, AgentReviewError::Http { status, .. } if *status == 404),
            "expected a 404 on the call where the diff vanishes: {err:?}"
        );

        let gone = review_gone_result();
        assert_eq!(gone["status"], serde_json::json!("review_gone"));
        assert_eq!(
            gone["instruction"],
            serde_json::json!("Stop listening; the review no longer exists.")
        );
    }

    // ── (b) BACKLOG: list_open_branches ──

    #[tokio::test]
    async fn list_open_branches_returns_only_root_open_and_answering() {
        let long_text = format!("why is this unwrap safe? {}", "x".repeat(200));
        let comments = serde_json::json!({
            "comments": [
                {
                    "id": "root-open",
                    "entityId": "a.ts::function::foo",
                    "filePath": "a.ts",
                    "line": 10,
                    "text": long_text,
                    "state": "open",
                },
                {
                    "id": "root-answering",
                    "filePath": "b.ts",
                    "line": 5,
                    "text": "second question",
                    "state": "answering",
                },
                { "id": "root-answered", "text": "already answered", "state": "answered" },
                { "id": "root-resolved", "text": "done", "state": "resolved" },
                {
                    "id": "reply-1",
                    "parentId": "root-open",
                    "text": "a reply, not a root",
                    "state": "open",
                },
            ]
        });
        let (base_url, _handle) = start_comments_body(comments).await;
        let client = test_client(&base_url);

        let branches = tokio::task::spawn_blocking(move || client.list_open_branches("diff1"))
            .await
            .unwrap()
            .unwrap();

        let ids: Vec<&str> = branches.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["root-open", "root-answering"],
            "must exclude replies, answered, and resolved comments — never claims anything"
        );

        let open = &branches[0];
        assert_eq!(open.entity.as_deref(), Some("a.ts::function::foo"));
        assert_eq!(open.line, Some(10));
        assert_eq!(open.state, "open");
        assert!(
            open.excerpt.ends_with('…'),
            "long text must be truncated: {}",
            open.excerpt
        );
        assert!(open.excerpt.chars().count() <= EXCERPT_MAX_CHARS + 1);

        let answering = &branches[1];
        assert_eq!(
            answering.entity.as_deref(),
            Some("b.ts"),
            "falls back to filePath when there's no entityId"
        );
        assert_eq!(answering.excerpt, "second question");
        assert_eq!(answering.state, "answering");
    }

    #[tokio::test]
    async fn list_open_branches_empty_when_nothing_open() {
        let comments = serde_json::json!({
            "comments": [
                { "id": "root-answered", "text": "already answered", "state": "answered" },
            ]
        });
        let (base_url, _handle) = start_comments_body(comments).await;
        let client = test_client(&base_url);

        let branches = tokio::task::spawn_blocking(move || client.list_open_branches("diff1"))
            .await
            .unwrap()
            .unwrap();

        assert!(branches.is_empty());
    }
}
