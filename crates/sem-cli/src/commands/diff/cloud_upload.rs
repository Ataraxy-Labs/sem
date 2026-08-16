//! The cloud-upload decision tree for `sem diff`: whether to upload a diff
//! snapshot at all, and what to do about caller/callee relations once the
//! upload response comes back.
//!
//! Every env/consent input this flow needs — `SEM_LOCAL`/`SEM_NO_NETWORK`,
//! per-repo cloud consent, credentials, and the `SEM_RELATIONS_LOCAL` /
//! `SEM_RELATIONS_BUDGET_MS` escape hatches — is resolved exactly once, in
//! [`DiffCloudContext::resolve`]. Everything below that point
//! ([`run_upload_first_flow`], [`run_inline_relations_flow`],
//! [`execute_relations_plan`], and the relations pass in
//! `diff::relations`) takes the resolved [`DiffCloudContext`] as data and
//! never reads env or re-checks consent itself.
//!
//! The whole flow is decided as data first ([`UploadFlow`], [`RelationsPlan`])
//! and executed second — see [`maybe_upload_cloud_diff_snapshot`] for the
//! decision points and [`run_upload_first_flow`] / [`execute_relations_plan`]
//! for where each outcome is carried out.
//!
//! Flow (default, `SEM_RELATIONS_LOCAL` unset — [`UploadFlow::UploadFirst`]):
//!   1. Upload immediately with empty relations. This is the only network
//!      call standing between the local diff printing and the terminal
//!      lingering, and it is small (no relations payload).
//!   2. Branch on `relationsStatus` in the response ([`RelationsPlan::from_status`]):
//!        - `"enrichmentQueued"`: the server computes relations itself. Print
//!          a one-liner and stop — no local relations work at all.
//!        - `"unavailable"` (the consent boundary: private/unregistered
//!          repo) or the field is absent (an older server, version-tolerant
//!          fallback): run the existing budgeted local relations pass and PUT
//!          the result to `/v1/diffs/{id}/relations` if it finishes in time.
//!          On timeout, `relations::build_changed_entity_relations` already
//!          prints the degradation warning; nothing further to do here.
//!        - `"included"` or any other/unknown value: nothing to do locally.
//!
//! `SEM_RELATIONS_LOCAL=1` ([`UploadFlow::InlineRelations`]) skips all of
//! this and reproduces the old single-upload behavior: compute relations
//! first (budgeted), then upload once with them already inline.

use std::path::Path;

use sem_core::git::bridge::GitBridge;
use sem_core::parser::differ::{BinaryFileChange, DiffResult};
use sem_core::git::types::FileChange;

use super::relations::{build_changed_entity_relations, relations_is_empty};
use super::{DiffOptions, ParsedArgs, ParsedScope};

/// `true` when `SEM_RELATIONS_LOCAL=1` is set — the escape hatch back to the
/// pre-instant-upload behavior: compute relations first (budgeted), then
/// upload once with them already inline, regardless of what the server's
/// `relationsStatus` contract supports. Useful when server-side enrichment
/// isn't trusted or available and today's single-upload guarantee is wanted.
///
/// Read only from [`DiffCloudContext::resolve`] — never below it.
fn relations_forced_local() -> bool {
    std::env::var("SEM_RELATIONS_LOCAL").is_ok_and(|v| v == "1")
}

/// `SEM_RELATIONS_BUDGET_MS`, parsed once here instead of deep inside the
/// relations pass (`relations::relations_time_budget`, which now just takes
/// the parsed override as a parameter).
///
/// Read only from [`DiffCloudContext::resolve`] — never below it.
fn relations_budget_override_ms() -> Option<u64> {
    std::env::var("SEM_RELATIONS_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
}

/// Everything the cloud-upload decision needs, resolved once before any
/// network call or relations work happens: whether this repo/session can
/// reach the cloud at all, the client to use, and the diff-specific
/// `SEM_RELATIONS_*` escape hatches. [`resolve`](Self::resolve) is the
/// single place that reads env or checks consent for this whole flow;
/// [`maybe_upload_cloud_diff_snapshot`] and everything it calls
/// (`run_inline_relations_flow`, `run_upload_first_flow`,
/// `execute_relations_plan`) take this struct as data.
struct DiffCloudContext {
    client: super::super::cloud::CloudClient,
    git: GitBridge,
    remote: String,
    relations_local: bool,
    relations_budget_override_ms: Option<u64>,
}

impl DiffCloudContext {
    /// `None` means the cloud-upload path is not reachable at all for this
    /// invocation: streamed from stdin, `SEM_LOCAL`/`SEM_NO_NETWORK` forces
    /// local, no git remote, no consent for this repo (`sem cloud
    /// enable`/`share`, or the `SEM_CLOUD=1` CI override), or not logged in
    /// (`SEM_TOKEN` or `~/.sem/credentials.json` via `sem login`).
    fn resolve(opts: &DiffOptions, from_stdin: bool) -> Option<Self> {
        if from_stdin || super::super::cloud::is_local_forced() {
            return None;
        }

        let git = GitBridge::open(Path::new(&opts.cwd)).ok()?;
        let remote = git.get_remote_url()?;
        if !super::super::consent::cloud_enabled_for(&remote) {
            return None;
        }
        let client = super::super::cloud::CloudClient::from_credentials()?;

        Some(Self {
            client,
            git,
            remote,
            relations_local: relations_forced_local(),
            relations_budget_override_ms: relations_budget_override_ms(),
        })
    }
}

/// Which top-level shape the upload takes, decided once before any network
/// call. Computed by [`maybe_upload_cloud_diff_snapshot`] from the
/// `SEM_RELATIONS_LOCAL` escape hatch; executed by [`run_inline_relations_flow`]
/// / [`run_upload_first_flow`].
enum UploadFlow {
    /// `SEM_RELATIONS_LOCAL=1`: compute relations first (budgeted), then
    /// upload once with them already inline. No PUT ever happens.
    InlineRelations,
    /// Default: upload immediately with empty relations, then decide what
    /// to do locally from the server's `relationsStatus`.
    UploadFirst,
}

/// What to do about caller/callee relations after an [`UploadFlow::UploadFirst`]
/// upload response comes back. Computed once from `relationsStatus` by
/// [`RelationsPlan::from_status`]; executed by [`execute_relations_plan`].
enum RelationsPlan {
    /// `"enrichmentQueued"`: the server computes relations itself.
    ServerEnriching,
    /// `"unavailable"` (consent boundary) or the field is absent (an older
    /// server, version-tolerant fallback): compute locally and PUT.
    ComputeLocalAndAttach,
    /// `"included"` (server already has relations) or a future status value
    /// not recognized yet: nothing to do locally.
    NoOp,
}

impl RelationsPlan {
    fn from_status(status: Option<&str>) -> Self {
        match status {
            Some("enrichmentQueued") => Self::ServerEnriching,
            Some("unavailable") | None => Self::ComputeLocalAndAttach,
            Some(_included_or_unknown) => Self::NoOp,
        }
    }
}

/// Upload a diff snapshot, printing the standard "could not be uploaded"
/// warning and returning `None` on failure so callers can just early-return.
#[allow(clippy::too_many_arguments)]
fn upload_diff_snapshot_or_warn(
    ctx: &DiffCloudContext,
    head_sha: Option<&str>,
    opts: &DiffOptions,
    git_context: &serde_json::Value,
    file_changes: &[FileChange],
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
    relations: &serde_json::Value,
) -> Option<super::super::cloud::CloudDiffSnapshotResponse> {
    match ctx.client.upload_diff_snapshot(
        &ctx.remote,
        head_sha,
        opts.label.as_deref(),
        git_context,
        file_changes,
        result,
        binary_changes,
        relations,
    ) {
        Ok(resp) => Some(resp),
        Err(e) => {
            eprintln!(
                "warning: local diff succeeded, but the private review could not be uploaded: {e}"
            );
            None
        }
    }
}

/// Upload the diff snapshot to sem cloud, then handle caller/callee relations
/// per the server's `relationsStatus` contract (see [`super::relations::build_changed_entity_relations`]
/// for the local computation, which is unconditionally never run before the
/// upload — that's the whole point: the upload must never block on it).
pub(super) fn maybe_upload_cloud_diff_snapshot(
    opts: &DiffOptions,
    parsed: &ParsedArgs,
    from_stdin: bool,
    file_changes: &[FileChange],
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
) {
    let Some(ctx) = DiffCloudContext::resolve(opts, from_stdin) else {
        return;
    };

    // Cross-machine facts service (semx-9en cloud half): batch-query before
    // extraction, extract+upload only the files sem cloud doesn't already
    // know. Fire-and-forget, on its own client (facts_remote spawns a
    // detached thread that outlives this function) -- never blocks the diff
    // command's own output, never fails the build. See
    // commands/diff/facts_remote.rs's module docs for the exact scope of
    // what this does and does not skip extraction for. Gated the same way
    // `ctx` itself already is (SEM_LOCAL/SEM_NO_NETWORK, consent, login) --
    // this call only happens inside the branch that already resolved all of
    // that, and additionally respects its own SEM_FACTS_REMOTE=0 kill switch.
    if let Some(facts_client) = super::super::cloud::CloudClient::from_credentials() {
        let registry = super::super::create_registry(&opts.cwd);
        let touched: Vec<String> = file_changes.iter().map(|c| c.file_path.clone()).collect();
        super::facts_remote::sync(
            ctx.git.repo_root(),
            &ctx.remote,
            &registry,
            facts_client,
            &touched,
        );
    }

    let head_sha = ctx.git.get_head_sha().ok();
    let git_context = build_git_context(&ctx.git, opts, parsed);

    let flow = if ctx.relations_local {
        UploadFlow::InlineRelations
    } else {
        UploadFlow::UploadFirst
    };

    match flow {
        UploadFlow::InlineRelations => run_inline_relations_flow(
            &ctx,
            head_sha.as_deref(),
            opts,
            &git_context,
            file_changes,
            result,
            binary_changes,
        ),
        UploadFlow::UploadFirst => run_upload_first_flow(
            &ctx,
            head_sha.as_deref(),
            opts,
            &git_context,
            file_changes,
            result,
            binary_changes,
        ),
    }
}

/// `SEM_RELATIONS_LOCAL=1`: compute relations before the upload and send
/// them inline in the same POST; no follow-up PUT ever happens.
#[allow(clippy::too_many_arguments)]
fn run_inline_relations_flow(
    ctx: &DiffCloudContext,
    head_sha: Option<&str>,
    opts: &DiffOptions,
    git_context: &serde_json::Value,
    file_changes: &[FileChange],
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
) {
    let relations = build_changed_entity_relations(opts, result, ctx.relations_budget_override_ms);
    if let Some(resp) = upload_diff_snapshot_or_warn(
        ctx,
        head_sha,
        opts,
        git_context,
        file_changes,
        result,
        binary_changes,
        &relations,
    ) {
        let url = ctx.client.diff_snapshot_url(&resp.id);
        super::super::consent::record_outbound(&ctx.remote, "diff", &resp.id);
        eprintln!("sem cloud diff: {url}");
    }
}

/// Default flow: upload now with no relations computed at all yet, then
/// decide what to do locally from the response's `relationsStatus`.
#[allow(clippy::too_many_arguments)]
fn run_upload_first_flow(
    ctx: &DiffCloudContext,
    head_sha: Option<&str>,
    opts: &DiffOptions,
    git_context: &serde_json::Value,
    file_changes: &[FileChange],
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
) {
    let empty_relations = serde_json::json!({});
    let Some(resp) = upload_diff_snapshot_or_warn(
        ctx,
        head_sha,
        opts,
        git_context,
        file_changes,
        result,
        binary_changes,
        &empty_relations,
    ) else {
        return;
    };

    let url = ctx.client.diff_snapshot_url(&resp.id);
    super::super::consent::record_outbound(&ctx.remote, "diff", &resp.id);
    eprintln!("sem cloud diff: {url}");

    let plan = RelationsPlan::from_status(resp.relations_status.as_deref());
    execute_relations_plan(plan, ctx, opts, result, &resp.id);
}

/// Carry out a [`RelationsPlan`] decided by [`RelationsPlan::from_status`].
fn execute_relations_plan(
    plan: RelationsPlan,
    ctx: &DiffCloudContext,
    opts: &DiffOptions,
    result: &DiffResult,
    diff_id: &str,
) {
    match plan {
        RelationsPlan::ServerEnriching => {
            eprintln!(
                "semantics: computing in cloud (will appear in the hosted review shortly)"
            );
        }
        RelationsPlan::ComputeLocalAndAttach => {
            // "unavailable": the consent boundary — the server won't compute
            // relations for this repo, so the client does, same as before
            // this change. `None` status (folded into this arm by
            // `RelationsPlan::from_status`): an older server that predates
            // `relationsStatus`; treated the same for version tolerance —
            // the fast upload already happened, so the closest equivalent to
            // "today's behavior" is still running the local pass and getting
            // the result attached, just via PUT instead of inline.
            let relations =
                build_changed_entity_relations(opts, result, ctx.relations_budget_override_ms);
            if !relations_is_empty(&relations) {
                match ctx.client.put_diff_relations(diff_id, &relations) {
                    Ok(()) => eprintln!("semantics: computed locally and attached"),
                    Err(e) => eprintln!(
                        "warning: relations were computed locally but could not be attached: {e}"
                    ),
                }
            }
            // Empty relations here means either no callers/callees to report,
            // or the budget timed out — `build_changed_entity_relations`
            // already printed the degradation warning for the latter.
        }
        RelationsPlan::NoOp => {
            // "included" (server already has relations) or a future status
            // value we don't recognize yet: nothing to do locally.
        }
    }
}

/// Capture truthful provenance for the exact comparison that produced a
/// snapshot. Working-tree reviews deliberately say `HEAD → WORKTREE`: their
/// uploaded changes do not exist at a GitHub commit yet.
fn build_git_context(
    git: &GitBridge,
    opts: &DiffOptions,
    parsed: &ParsedArgs,
) -> serde_json::Value {
    let branch = git.get_current_branch();
    let head_commit = git.get_head_sha().ok();

    let (scope, base_ref, head_ref, base_sha, comparison_head_sha) =
        if let Some(commit) = &opts.commit {
            (
                "commit",
                Some(format!("{commit}^")),
                Some(commit.clone()),
                git.resolve_ref_sha(&format!("{commit}^")),
                git.resolve_ref_sha(commit),
            )
        } else if let (Some(from), Some(to)) = (&opts.from, &opts.to) {
            (
                "range",
                Some(from.clone()),
                Some(to.clone()),
                git.resolve_ref_sha(from),
                git.resolve_ref_sha(to),
            )
        } else {
            match parsed.scope.as_ref() {
                Some(ParsedScope::RefToWorking(base)) => (
                    if opts.staged { "staged" } else { "working" },
                    Some(base.clone()),
                    Some(if opts.staged { "INDEX" } else { "WORKTREE" }.into()),
                    git.resolve_ref_sha(base),
                    None,
                ),
                Some(ParsedScope::Range(from, to)) => (
                    "range",
                    Some(from.clone()),
                    Some(to.clone()),
                    git.resolve_ref_sha(from),
                    git.resolve_ref_sha(to),
                ),
                Some(ParsedScope::MergeBaseRange(from, to)) => (
                    "merge-base",
                    Some(from.clone()),
                    Some(to.clone()),
                    git.resolve_merge_base(from, to).ok(),
                    git.resolve_ref_sha(to),
                ),
                Some(ParsedScope::FileCompare { .. }) => ("files", None, None, None, None),
                None => (
                    if opts.staged { "staged" } else { "working" },
                    Some("HEAD".into()),
                    Some(if opts.staged { "INDEX" } else { "WORKTREE" }.into()),
                    head_commit.clone(),
                    None,
                ),
            }
        };

    serde_json::json!({
        "branch": branch,
        "scope": scope,
        "baseRef": base_ref,
        "headRef": head_ref,
        "baseSha": base_sha,
        "headSha": comparison_head_sha,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relations_plan_routes_enrichment_queued_to_server_enriching() {
        assert!(matches!(
            RelationsPlan::from_status(Some("enrichmentQueued")),
            RelationsPlan::ServerEnriching
        ));
    }

    #[test]
    fn relations_plan_routes_unavailable_and_missing_status_to_compute_local() {
        assert!(matches!(
            RelationsPlan::from_status(Some("unavailable")),
            RelationsPlan::ComputeLocalAndAttach
        ));
        assert!(matches!(
            RelationsPlan::from_status(None),
            RelationsPlan::ComputeLocalAndAttach
        ));
    }

    #[test]
    fn relations_plan_routes_included_and_unknown_status_to_noop() {
        assert!(matches!(
            RelationsPlan::from_status(Some("included")),
            RelationsPlan::NoOp
        ));
        assert!(matches!(
            RelationsPlan::from_status(Some("some-future-status")),
            RelationsPlan::NoOp
        ));
    }
}
