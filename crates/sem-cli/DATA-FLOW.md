# Data flow: cloud consent + env authority (sem-cli, sem-mcp)

Scope: the cloud-touching command paths — `sem diff`'s upload, `sem review
listen`, `sem cloud …` / `sem login`, and their sem-mcp counterparts
(`agent_review`, the MCP tool handlers' cloud fast paths). This is not a
whole-CLI audit — `sem-cli` has ~50 other `std::env::var` call sites (width,
progress bars, telemetry, hyperlink detection, update checks, …) that carry
no consent/credential authority and are out of scope.

## The rule

**Each command entry resolves one context, once, from env + consent state +
credentials. Everything below that point takes the context as data and never
calls `std::env::var` or a consent check itself.** A "command entry" is the
`pub fn`/`async fn` that `main.rs` (or the MCP tool router) dispatches
directly to — `diff_command`, `review::listen`, `consent::enable`, the
`sem_impact`/`sem_context` tool handlers, etc. One level of indirection into
a *named resolver* (`DiffCloudContext::resolve`, `AgentReviewConfig::
from_env_or_credentials`) counts as "the edge," not a violation — the
violation is env reads scattered through unrelated business logic several
frames further down, especially inside code that can run off the main
thread (the old `relations_time_budget`, see below).

## Precedence rules

| Concern | Vars (highest → lowest precedence) | Owner |
|---|---|---|
| sem-cli cloud creds + endpoint | `SEM_TOKEN` (+ optional `SEM_CLOUD_ENDPOINT`) → `~/.sem/credentials.json` | `commands::cloud::credentials_or_env` |
| sem-cli force-local kill switch | `SEM_LOCAL=1` OR `SEM_NO_NETWORK` (non-empty, `!= "0"`) | `commands::cloud::is_local_forced` |
| sem-cli per-repo consent | `SEM_CLOUD=1` (CI/scripted override) → `~/.sem/cloud.json` (`sem cloud enable`/`share`/`never`) | `commands::consent::cloud_enabled_for` |
| sem diff relations escape hatch | `SEM_RELATIONS_LOCAL=1` | `diff::cloud_upload::DiffCloudContext::resolve` |
| sem diff relations budget | `SEM_RELATIONS_BUDGET_MS` → adaptive curve on tracked-file count | `diff::cloud_upload::DiffCloudContext::resolve` |
| sem review listen creds + endpoint | `SEM_API_KEY` / `SEM_CLOUD_URL` (**independently** — either alone falls back to the credentials file for just that field) → `~/.sem/credentials.json` | `sem_mcp::agent_review::AgentReviewConfig::from_env_or_credentials` |
| sem review listen model / plugin dir | `SEM_LISTENER_MODEL` / `SEM_LISTENER_PLUGIN_DIR` → defaults | `commands::review::listen` |
| sem-mcp graph-routing creds | `~/.sem/credentials.json` only (no env override) + `SEM_LOCAL=1` | `sem_cloud_client::CloudClient::from_credentials` |
| sem-mcp cloud opt-in for impact/context | `SEM_MCP_CLOUD=1` | the `sem_impact`/`sem_context` tool handlers directly |

**Known divergence, not fixed here (would be a contract change):** sem-cli's
own `CloudClient` (`commands/cloud.rs`), the shared `sem-cloud-client` crate's
`CloudClient` (used by sem-mcp's graph routing), and `AgentReviewConfig`
(sem-mcp's listener client) are three independent credential-resolution
implementations with three different env-var vocabularies (`SEM_TOKEN`/
`SEM_CLOUD_ENDPOINT` vs. none vs. `SEM_API_KEY`/`SEM_CLOUD_URL`) and three
different fallback shapes (whole-struct-from-env vs. no-env vs.
per-field-from-env). Each is internally consistent and already
edge-resolved by a single named function; unifying the *vocabulary* across
all three is a breaking change to documented env vars and out of scope for
this pass. Flagged here so it isn't rediscovered as a bug.

## Command-entry contexts

### `sem diff` → cloud upload (`commands/diff/cloud_upload.rs`)

`diff_command` never touches the cloud for computing the diff itself (a
range diff only parses the files in that range; local always wins — see the
comment in `diff_command`). The **only** cloud decision point for `sem diff`
is `cloud_upload::maybe_upload_cloud_diff_snapshot`, called once from
`run_diff_pipeline` after the diff has already been computed and printed.

- `DiffCloudContext::resolve(opts, from_stdin)` is the edge: it is the single
  place that reads `SEM_LOCAL`/`SEM_NO_NETWORK` (via `cloud::is_local_forced`),
  opens git and checks consent (`consent::cloud_enabled_for`, which reads
  `SEM_CLOUD`), resolves credentials (`cloud::CloudClient::from_credentials`,
  which reads `SEM_TOKEN`/`SEM_CLOUD_ENDPOINT`), and parses the diff-specific
  `SEM_RELATIONS_LOCAL` / `SEM_RELATIONS_BUDGET_MS` knobs. Returns `None` —
  "don't touch the cloud" — or `Some(DiffCloudContext)`.
- `maybe_upload_cloud_diff_snapshot`, `run_inline_relations_flow`,
  `run_upload_first_flow`, and `execute_relations_plan` (the upload-flow
  enum + relations-plan enum machinery) take `&DiffCloudContext` as **data**
  — they decide *what to do* (upload now vs. inline relations, PUT relations
  vs. not) purely from the response and the context fields, never re-reading
  env or re-checking consent.
- The local relations pass (`diff::relations::build_changed_entity_relations`
  → `relations_time_budget`) now takes the parsed `budget_override_ms:
  Option<u64>` as a parameter instead of reading `SEM_RELATIONS_BUDGET_MS`
  itself. Previously this read happened 4-5 call frames below `diff_command`,
  inside the function that also spawns the detached worker thread for the
  budgeted pass — the deepest env read in the whole cloud path, and the one
  named explicitly as the model violation for this pass.

Documented exception: `CloudClient::diff_snapshot_url` reads
`SEM_DIFF_VIEWER_URL` at the point it formats a URL string, one call inside
the flow functions. This is not a decision input — it can't change *whether*
anything happens, only *what string gets printed* — so it stays local rather
than becoming another `DiffCloudContext` field.

### `sem review listen` (`commands/review.rs`)

- `AgentReviewConfig::from_env_or_credentials()` — resolved once at the top
  of `listen()` — is the edge for creds+endpoint (`SEM_API_KEY`/
  `SEM_CLOUD_URL` → `~/.sem/credentials.json`, independently per field). This
  was already correct before this pass; it's the pattern the rest of the
  audit measures against.
- `listener_model()` (`SEM_LISTENER_MODEL`) and `locate_plugin_dir()`
  (`SEM_LISTENER_PLUGIN_DIR`) used to be called from *inside*
  `build_launch_plan`, which its own doc comment already (incorrectly)
  called "a pure function of its inputs." They're now resolved in `listen()`
  itself, after the manifest-existence network check (preserving the
  existing error precedence — a missing diff still reports before a missing
  plugin dir) and passed into `build_launch_plan(diff_id, &config,
  &plugin_dir, &model)`, which is now actually pure: it only formats a
  `LaunchPlan` from arguments. `PATH` (`find_claude_on_path`) is read
  directly in `listen()` itself — already at the edge, one direct call, no
  change needed.
- `LaunchPlan` is the resolved-context-as-data object here: `print_dry_run`
  and `exec_claude` both consume exactly the same plan, so they can't drift.

### `sem login` / `sem cloud …` (`commands/cloud.rs`, `commands/consent.rs`)

Each subcommand (`login`, `login_github`, `logout`, `whoami`, `xref`,
`consent::enable`, `share`, `list`, `status`, `preview`, `log`, `never`,
`forget`) *is itself* the command entry dispatched directly from `main.rs`.
Their env reads (`SEM_GITHUB_CLIENT_ID`, `SEM_NO_BROWSER`, `SEM_CLOUD`, the
credentials/consent files) are one direct call inside the entry function
body or its immediate one-line helper (`credentials_or_env`,
`cloud_enabled_for`) — already at the edge, not below it. No change made.

### sem-mcp

- `agent_review::AgentReviewConfig` — see above; unchanged, already the
  reference pattern.
- `sem_impact` / `sem_context` tool handlers (`server.rs`) check
  `SEM_MCP_CLOUD` directly in the handler body before calling
  `crate::cloud::try_impact`/`try_context` — each MCP tool call is itself a
  command entry (the protocol dispatches directly to the handler), so this
  is edge-resident, not deep.
- `SEM_REPO` (repo-root discovery) and `SEM_PREWARM` (background graph
  warm-up) are process-startup / resource knobs, not consent or credential
  inputs — out of scope for this pass.

## Diagram: `sem diff`'s cloud path and `sem review listen`

```
sem diff                                   sem review listen <id>
   │                                            │
   ▼                                            ▼
diff_command (entry)                       listen (entry)
   │ compute + print local diff                │ AgentReviewConfig::from_env_or_credentials()
   │ (never touches cloud)                      │   SEM_API_KEY / SEM_CLOUD_URL → credentials.json
   ▼                                            │ client.manifest(diff_id)  — validates against sem-cloud
run_diff_pipeline                               │ locate_plugin_dir()  SEM_LISTENER_PLUGIN_DIR
   │                                            │ listener_model()     SEM_LISTENER_MODEL
   ▼                                            │ find_claude_on_path()  PATH
maybe_upload_cloud_diff_snapshot                ▼
   │                                        build_launch_plan(diff_id, config,
   ▼                                                          plugin_dir, model)
DiffCloudContext::resolve()  ◄── EDGE           │  (pure: no env reads)
   │ SEM_LOCAL / SEM_NO_NETWORK                  ▼
   │ git remote + SEM_CLOUD /                LaunchPlan (data)
   │   ~/.sem/cloud.json (consent)               │
   │ SEM_TOKEN / SEM_CLOUD_ENDPOINT /             ├─ --dry-run → print_dry_run(plan)
   │   ~/.sem/credentials.json (creds)            └─ else      → exec_claude(plan)
   │ SEM_RELATIONS_LOCAL
   │ SEM_RELATIONS_BUDGET_MS
   ▼
Some(DiffCloudContext) ──────► run_inline_relations_flow /
                                run_upload_first_flow      (pure execution over
                                    │                        the resolved context)
                                    ▼
                                execute_relations_plan
                                    │
                                    ▼
                          relations::build_changed_entity_relations(
                              opts, result, ctx.relations_budget_override_ms)
                                    │  (no env read — takes the override as data)
                                    ▼
                          relations_time_budget(cwd, override_ms)
```

## Env-read audit: before / after

"Depth" = call frames below the command entry (`diff_command`, `listen`,
etc.) where the `std::env::var[_os]` call itself executes. Files outside the
scoped list (`diff/`, `review.rs`, `cloud.rs`, `consent.rs`, sem-mcp) are not
included — see the note at the top of this doc.

| Site (before) | Var(s) | Depth (before) | Verdict | Depth (after) |
|---|---|---|---|---|
| `diff/relations.rs::relations_time_budget` | `SEM_RELATIONS_BUDGET_MS` | **5** (`diff_command` → `run_diff_pipeline` → `maybe_upload_cloud_diff_snapshot` → `run_*_flow`/`execute_relations_plan` → `build_changed_entity_relations` → `relations_time_budget`) | **Fixed** — moved to `DiffCloudContext::resolve`, threaded as `Option<u64>` | 0 (param) |
| `diff/cloud_upload.rs::relations_forced_local` (called from `maybe_upload_cloud_diff_snapshot`) | `SEM_RELATIONS_LOCAL` | 2 | **Fixed** — moved into `DiffCloudContext::resolve` | 1 (single named resolver) |
| `diff/cloud_upload.rs` consent + creds + local-forced checks (previously inline in `maybe_upload_cloud_diff_snapshot`) | `SEM_LOCAL`/`SEM_NO_NETWORK`, `SEM_CLOUD`, `SEM_TOKEN`/`SEM_CLOUD_ENDPOINT` | 2 (function body was both edge *and* deep module) | **Fixed** — extracted into `DiffCloudContext::resolve`, decision flow now takes the struct as data (lane 2) | 1 |
| `review.rs::listener_model` / `locate_plugin_dir` (called from `build_launch_plan`) | `SEM_LISTENER_MODEL`, `SEM_LISTENER_PLUGIN_DIR` | 2 (`listen` → `build_launch_plan` → helper) | **Fixed** — resolved in `listen()`, `build_launch_plan` is now pure | 1 |
| `review.rs::find_claude_on_path` | `PATH` | 1 (direct call in `listen`) | Already edge — no change | 1 |
| `cloud.rs::is_local_forced` / `network_disabled` / `credentials_or_env` | `SEM_LOCAL`, `SEM_NO_NETWORK`, `SEM_TOKEN`, `SEM_CLOUD_ENDPOINT` | 1 (from each caller: `try_cloud_*`, `DiffCloudContext::resolve`, `consent::*`) | Already edge (single named resolver, one indirection) — no change | 1 |
| `consent.rs::cloud_enabled_for` | `SEM_CLOUD` | 1 (from each caller) | Already edge — no change | 1 |
| `cloud.rs::diff_snapshot_url` | `SEM_DIFF_VIEWER_URL` | 2 (flow fn → `diff_snapshot_url`) | Documented exception — formatting only, not a decision input | 2 (unchanged, now documented) |
| `cloud.rs`/`consent.rs` `HOME`/`USERPROFILE` (credentials/repo-cache/consent/login-hint paths) | `HOME`, `USERPROFILE` | 1 (leaf path-resolution helpers) | Out of scope — local storage path, not a decision input; pre-existing 5x duplication noted but not touched (no behavior change requested) | 1 |
| `cloud.rs::login_github` | `SEM_GITHUB_CLIENT_ID`, `SEM_GITHUB_DEVICE_CODE_URL`, `SEM_GITHUB_DEVICE_TOKEN_URL`, `SEM_NO_BROWSER` | 0–1 (entry function itself / `open_url`) | Already edge — no change | unchanged |
| `sem-mcp/agent_review.rs::AgentReviewConfig::resolve` | `SEM_API_KEY`, `SEM_CLOUD_URL` | 1 (`from_env_or_credentials` → `resolve`) | Reference pattern — no change | 1 |
| `sem-mcp/server.rs` `sem_impact`/`sem_context` handlers | `SEM_MCP_CLOUD` | 0 (direct in handler, the MCP command entry) | Already edge — no change | 0 |
| `sem-mcp/server.rs` (`SEM_REPO`, `SEM_PREWARM`) | — | — | Out of scope — not consent/credential inputs | unchanged |

**Summary:** 3 genuinely deep reads found in the explicitly-scoped
cloud-upload path (all in `diff/cloud_upload.rs` + `diff/relations.rs`,
depth 2–5 below `diff_command`) and 2 in `review.rs` (depth 2 below
`listen`) — all 5 converged to a single named edge-resolver per command
(`DiffCloudContext::resolve`, and `listen()`'s own top few lines) and now
depth ≤ 1. Everything else already matched the AgentReviewConfig pattern
(one named resolver, one level of indirection) or is out of scope (local
storage paths, MCP-handler-local opt-ins, process-startup knobs).

## Keeping this from rotting

This document is a snapshot, not a live check — it can drift from the code.
When touching a `std::env::var[_os]` call site, adding or changing a fn
parameter typed `*Config`/`*Context`/`*Ctx`/`*Client`/`*Credentials`/`*Env`
(the authority-bearing shapes this doc's table tracks by hand), or adding
`Arc`/`Mutex`/`RwLock` ownership on the cloud-touching paths above, update
the relevant row in this file's table in the same change.
