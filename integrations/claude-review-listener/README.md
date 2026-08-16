# sem-review-listener

A Claude Code plugin that turns a headless Claude session into a live listener
on a sem-cloud code review: it joins a diff, long-polls for reviewer
questions ("branches"), investigates them in the repository, and streams
answers back.

Three MCP tools do the work (`crates/sem-mcp/src/server.rs`, backed by the
client in `crates/sem-mcp/src/agent_review.rs`): `join_review`,
`wait_for_branch`, `reply_to_branch`. A `Stop` hook (`hooks/keep-listening.sh`)
is a backstop only — see "How it stays alive" below.

## Layout

```
claude-review-listener/
├── .claude-plugin/
│   └── plugin.json        # plugin identity: name "sem-review-listener"
├── .mcp.json               # declares the "sem-review" MCP server (the sem binary, `mcp` subcommand)
├── hooks/
│   ├── hooks.json           # registers the Stop-hook backstop
│   └── keep-listening.sh    # the backstop script (read-only against sem-cloud)
└── README.md
```

## Prerequisites

- Build the `sem` binary first: `cargo build -p sem-cli` from `crates/`
  (`.mcp.json` points at `crates/target/debug/sem`, a debug build — see
  "Contract doubts" below for why this path is fragile).
- A running (or soon-to-be-running) sem-cloud instance and a diff id to
  listen on.
- The `claude` CLI (this recipe was verified against `claude --version`
  2.1.226; flag names below are copied from `claude --help` on this
  machine — re-check if you're on a materially different version).

## Run recipe

From the repo root (`/path/to/sem`), with sem-cloud running locally:

```bash
# Point the listener at your local sem-cloud + demo credentials, NOT your
# real `sem login` account (the plugin's .mcp.json reads these two first,
# falling back to ~/.sem/credentials.json only if unset — see sem-mcp's
# agent_review::AgentReviewConfig::from_env_or_credentials).
export SEM_CLOUD_URL="http://127.0.0.1:8080"
export SEM_API_KEY="<your demo sem-cloud API key>"

# Consumed only by the Stop-hook backstop (hooks/keep-listening.sh), not by
# the MCP server itself — the join_review tool call is what actually tells
# sem-cloud which diff to listen on.
export SEM_REVIEW_DIFF_ID="<the diff id to review>"

claude \
  --plugin-dir integrations/claude-review-listener \
  --model claude-opus-5 \
  --allowedTools "mcp__plugin_sem-review-listener_sem-review__*" "Read" "Grep" "Glob" "Bash" \
  --permission-mode bypassPermissions \
  -p "Join the sem-cloud review for diff $SEM_REVIEW_DIFF_ID using join_review, then follow the listener protocol it returns. Never end the session yourself."
```

Notes on the flags (all verified against `claude --help` on this machine):

- `--plugin-dir <path>` — loads the plugin for this session only, no
  install step. **Must point at this in-repo path** (see "Contract doubts"
  below re: `${CLAUDE_PLUGIN_ROOT}` math).
- `--model claude-opus-5` — full model name; the alias form (`--model opus`)
  works too per `claude --help`'s "Provide an alias for the latest model
  (e.g. 'fable', 'opus', or 'sonnet') or a model's full name (e.g.
  'claude-fable-5')".
- `-p` / `--print` — non-interactive: prints and exits when the turn ends
  rather than opening a REPL. Combined with the listener protocol's
  "never end the session yourself" instruction, the session is meant to run
  for as long as the review does.
- `--allowedTools` — pre-approves tool calls so a non-interactive session
  doesn't hang on (or get silently denied by) a permission prompt it can't
  answer. **The MCP tool name is NOT `mcp__sem-review__*`** — see "Contract
  doubts". `Read`/`Grep`/`Glob`/`Bash` are included because the listener
  protocol requires investigating each question "in this repository," not
  just calling MCP tools.
- `--permission-mode bypassPermissions` — belt-and-suspenders alongside
  `--allowedTools`; drop this for anything beyond a throwaway
  sandbox/demo run, per `claude --help`'s own warning ("Recommended only
  for sandboxes with no internet access").

## How it stays alive

The **primary** mechanism is the MCP tool loop, driven entirely by the
model following the protocol text `join_review` hands back verbatim:

1. `join_review {diff_id}` — announces presence, returns the protocol.
2. `wait_for_branch {diff_id}` — long-polls (default 40s, max 45s) for the
   next reviewer question. A `status: timeout` result is normal idle
   behavior, not a stopping condition.
3. On a branch: investigate in the repo, then `reply_to_branch` with
   `partial: true` chunks while composing and a final `partial: false` (or
   omitted) call to commit.
4. Back to step 2, forever.

The `Stop` hook (`hooks/keep-listening.sh`) is a **backstop only** — for
when the model tries to end its turn anyway (e.g. after context compaction
drops the protocol instructions). It never calls `agent/next` itself (that
would claim the open branch on the hook's behalf, racing the model's own
`wait_for_branch` call); it only does a read-only
`GET /v1/diffs/{id}/comments` and blocks the stop with a reminder if it sees
an open comment, or nudges once if it's unsure. See the script's own
comments for the full state machine.

## Contract doubts

Things the task spec assumed that turned out not to match reality, verified
against `https://code.claude.com/docs/en/plugins.md`,
`plugins-reference.md`, `mcp.md`, and `hooks.md` on 2026-08-08:

1. **MCP tool naming.** A plugin-bundled MCP server's tools are namespaced
   as `mcp__plugin_<plugin-name>_<server-name>__<tool-name>`, not the flat
   `mcp__<server-name>__<tool-name>` the original recipe assumed. With
   `plugin.json`'s `name: "sem-review-listener"` and `.mcp.json`'s server
   key `"sem-review"`, the three tools register as:
   - `mcp__plugin_sem-review-listener_sem-review__join_review`
   - `mcp__plugin_sem-review-listener_sem-review__wait_for_branch`
   - `mcp__plugin_sem-review-listener_sem-review__reply_to_branch`

   `--allowedTools "mcp__sem-review__*"` as originally proposed would
   silently allow nothing; the README above uses the correct scoped
   wildcard.

2. **`${CLAUDE_PLUGIN_ROOT}/../../crates/target/debug/sem`.** Docs confirm
   `${CLAUDE_PLUGIN_ROOT}` substitutes in `command`/`args`/`env` for MCP
   `stdio` servers, but every documented example only appends subpaths
   (`${CLAUDE_PLUGIN_ROOT}/servers/db-server`) — none shows `..`
   traversal. It should work as ordinary path resolution once the
   substitution happens, and it does in practice (verified: `crates/`
   is two levels above this plugin's directory,
   `integrations/claude-review-listener/`, so `../..` lands at the repo
   root). But this **only holds when the plugin is loaded via
   `--plugin-dir` pointed at this in-repo path**; a marketplace-installed
   copy would live elsewhere (`~/.claude/plugins/...`) and this path
   would resolve to nothing. If you ever package this plugin for
   distribution, replace the hardcoded relative path with a `sem` on
   `$PATH`, or resolve the repo root via `${CLAUDE_PROJECT_DIR}` instead
   (also substituted into MCP server fields per the docs, and not
   dependent on the plugin's own nesting depth).

3. **`${SEM_CLOUD_URL:-}` / `${SEM_API_KEY:-}` with an explicit empty
   default.** Per the docs, `.mcp.json` env-var expansion leaves the
   *literal, unexpanded* `${VAR}` text in place when a referenced variable
   is unset and has no default — not an empty string. Without the `:-`
   default, forgetting to `export SEM_API_KEY` before launching `claude`
   would hand the MCP server the literal string `"${SEM_API_KEY}"` as its
   API key, which is non-empty and would silently defeat
   `AgentReviewConfig`'s env-first-then-`~/.sem/credentials.json` fallback
   (a non-empty bogus value shadows the credentials file instead of
   falling through to it). The `:-` empty default makes an unset var
   resolve to `""`, which `AgentReviewConfig::resolve` already treats as
   "unset" and correctly falls through.

4. **`stop_hook_active` does not exist.** The task's Stop-hook spec assumed
   a `stop_hook_active` field on the hook's stdin JSON and an implicit
   "block cap is 8" enforced by Claude Code itself. Neither is in the
   current documented schema
   (`session_id`, `prompt_id`, `transcript_path`, `cwd`, `permission_mode`,
   `hook_event_name`, `last_assistant_message`, `effort` — confirmed via
   two separate doc fetches, one asking specifically about this field).
   There's also no documented cap on consecutive Stop-hook blocks.
   `keep-listening.sh` substitutes its own bounded, plugin-scoped counter
   (persisted at `$CLAUDE_PLUGIN_DATA/stop-count-<session_id>`) that: blocks
   up to 8 consecutive times while an open comment is visible, blocks
   exactly once (not repeatedly) when no open comment is visible — as a
   courtesy nudge in case the model just hasn't joined yet — and always
   resets on an allowed stop. This is a best-effort emulation of the
   spec's intent, not a verified Claude Code mechanism.

5. **`/v1/diffs/{id}/manifest` and `/v1/diffs/{id}/comments` response
   shapes are inferred.** Both are sem-cloud endpoints outside this
   client's control and the task described them only loosely ("review
   summary" / "any root comment with state==\"open\""). `agent_review::
   render_manifest_summary` reads common field name candidates
   (`label`/`title`/`name`, `files`/`fileCount`, `comments`/`commentCount`)
   and falls back to raw JSON for anything else, so an unexpected shape
   degrades gracefully instead of erroring. `keep-listening.sh` accepts
   either a bare JSON array or a `{"comments": [...]}` wrapper from
   `/comments`. Re-check both against sem-cloud's actual implementation
   once it lands.

6. **404s during development.** Every endpoint in `agent_review.rs` is
   coded defensively against sem-cloud not existing yet: `next_branch`,
   `reply`, `presence`, and `manifest` all surface a clear
   `AgentReviewError::Http { status: 404, .. }` rather than panicking, and
   `join_review`'s tool result treats a manifest/presence failure as
   non-fatal (it still hands back the listener protocol so the loop can
   start once sem-cloud is up). Covered by
   `agent_review::tests::next_branch_surfaces_404_as_http_error`.
