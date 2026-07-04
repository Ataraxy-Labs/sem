//! Cloud routing for the MCP tools.
//!
//! When the agent's environment is logged into sem-cloud (`sem login`) and the
//! current repo is a large, registered, public repo, the graph queries route to
//! the cloud's warm cache instead of building the graph locally. This is the
//! same path the CLI takes — both depend on `sem-cloud-client` — so a logged-in
//! agent session produces metered cloud usage identically to the CLI.
//!
//! Every helper returns `Option<Value>`: `Some(json)` means the cloud answered
//! and the tool should return it verbatim (the JSON shape matches the local
//! output exactly, so agents can't tell the difference); `None` means "not
//! eligible or cloud unreachable — fall through to the local path silently."

use std::path::Path;

use serde_json::{json, Value};

use sem_cloud_client::{known_small_repo, CloudClient, CloudEntityBrief};
use sem_core::git::bridge::GitBridge;

/// Resolve the registered cloud repo_id for this working copy, if it is
/// eligible for cloud routing (logged in, has a remote, large enough that the
/// network beats the local cache, registered/public). `None` short-circuits to
/// the local path.
fn eligible_repo(git: &GitBridge) -> Option<(CloudClient, String)> {
    let client = CloudClient::from_credentials()?;
    let remote = git.get_remote_url()?;
    // Small repos answer graph queries faster from the local SQLite cache than
    // a network round trip — stay local.
    if known_small_repo(&remote) {
        return None;
    }
    let repo_id = client.ensure_repo(&remote).ok()?;
    Some((client, repo_id))
}

/// Map a cloud entity brief to the MCP entity shape used by `sem_impact`
/// (`{name, type, file, lines: [start, end]}`).
fn brief_to_impact_entity(d: &CloudEntityBrief) -> Value {
    json!({
        "name": d.name,
        "type": d.entity_type,
        "file": d.file_path,
        "lines": [d.start_line.unwrap_or(0), d.end_line.unwrap_or(0)],
    })
}

/// Try to answer `sem_impact` from the cloud. Mirrors the local output shape
/// per mode. `tests` mode is never routed — the cloud API does not expose the
/// test classification the local path computes, so it stays local.
pub fn try_impact(git: &GitBridge, entity: &str, rel_path: &str, mode: &str) -> Option<Value> {
    if mode == "tests" {
        return None;
    }
    let (client, repo_id) = eligible_repo(git)?;
    let result = client.impact(&repo_id, entity, rel_path).ok()?;

    let map = |list: &[CloudEntityBrief]| -> Vec<Value> {
        list.iter().map(brief_to_impact_entity).collect()
    };

    let out = match mode {
        "deps" => json!({
            "entity": entity,
            "file": rel_path,
            "mode": "deps",
            "dependencies": map(&result.dependencies),
        }),
        "dependents" => json!({
            "entity": entity,
            "file": rel_path,
            "mode": "dependents",
            "dependents": map(&result.dependents),
        }),
        // "all" — the cloud impact endpoint has no test classification, so the
        // `tests` field is empty here (matching the CLI's cloud path).
        _ => json!({
            "entity": entity,
            "file": rel_path,
            "mode": "all",
            "dependencies": map(&result.dependencies),
            "dependents": map(&result.dependents),
            "impact": {
                "total": result.transitive_impact.len(),
                "entities": map(&result.transitive_impact),
            },
            "tests": [],
        }),
    };
    Some(out)
}

/// Try to answer `sem_context` from the cloud. Only routes when `hops == 0`
/// (the default neighborhood) — the cloud context endpoint packs by token
/// budget and does not bound by graph hops, so a hop-limited request stays
/// local where `build_context_result_bounded` honors it.
pub fn try_context(
    git: &GitBridge,
    entity: &str,
    rel_path: &str,
    budget: usize,
    hops: usize,
) -> Option<Value> {
    if hops != 0 {
        return None;
    }
    let (client, repo_id) = eligible_repo(git)?;
    let result = client.context(&repo_id, entity, rel_path, budget).ok()?;

    let entries: Vec<Value> = result
        .entries
        .iter()
        .map(|e| {
            json!({
                "entity": e.name,
                "type": e.entity_type,
                "file": e.file_path,
                "role": e.role,
                "tokens": e.estimated_tokens,
                "content": e.content,
            })
        })
        .collect();

    Some(json!({
        "entity": entity,
        "file": rel_path,
        "token_budget": budget,
        "tokens_used": result.tokens_used,
        "truncated": result.truncated,
        // The cloud endpoint always includes the target; it never omits it to
        // fit budget the way the local packer can.
        "target_omitted": false,
        "entries": entries.len(),
        "context": entries,
    }))
}

/// Try to answer a whole-repo `sem_entities` listing from the cloud. Only the
/// repo-root directory listing routes — single files and subdirectories parse
/// few files and win locally. Returns a JSON array matching the local shape.
pub fn try_entities(git: &GitBridge, repo_root: &Path, abs_path: &Path) -> Option<Value> {
    // Whole-repo listing only: the requested directory must be the repo root.
    let root = repo_root.canonicalize().ok()?;
    if abs_path.canonicalize().ok()? != root {
        return None;
    }
    let (client, repo_id) = eligible_repo(git)?;
    let resp = client.entities(&repo_id, None).ok()?;

    let mut entities = resp.entities;
    entities.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.entity_type.cmp(&b.entity_type))
            .then(a.name.cmp(&b.name))
    });

    let result: Vec<Value> = entities
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "name": e.name,
                "type": e.entity_type,
                "start_line": e.start_line,
                "end_line": e.end_line,
                "parent_id": e.parent_id,
                "file": e.file_path,
            })
        })
        .collect();

    Some(Value::Array(result))
}
