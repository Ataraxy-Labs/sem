# pi-sem

A [pi](https://pi.dev) package that gives the pi coding agent entity-native code
intelligence and editing, backed by the [sem](https://github.com/Ataraxy-Labs/sem)
CLI (read, search, impact analysis) and `weave-mcp` (entity-addressed edit plus
live multi-agent coordination). It replaces pi's built-in `read`/`edit`/`grep`/
`find`/`ls` with tools that look up and change code by entity name
(`parseConfig`, not "lines 40-58") instead of raw file text, and every edit runs
through a coordination layer that detects and merges concurrent changes instead
of overwriting them blind.

Two ways to use it, chosen at session start via `PI_SEM_MODE`: a curated
multi-tool surface (**tools mode**, the default), or a single sandboxed
`sem_code` tool that runs a short script against a typed `sem` API
(**code mode**). See [Running modes](#running-modes) below.

## Does it hold up?

Two results, each stated with its conditions:

- **Pure code mode solves real tasks without a shell.** On a 10-task
  SWE-bench Verified slice — hermetic environment (network-shimmed,
  ancestors-only checkouts), graded by SWE-bench's official harness — an
  agent running pure code mode resolved **9/10**, identical to the same
  agent with full bash access. Every one of its 87 tool calls across the
  slice was a `sem_code` entity operation: no shell, no direct file
  writes. One run, ten tasks; the claim is that the codespace is
  *sufficient*, not that it is smarter.
- **Concurrent edits don't lose work.** Two agent processes free-running
  edits against disjoint functions in the same file lose **zero edits in
  1,000 raced attempts** (plain last-writer-wins file writes lose one in
  80–100% of runs on the same harness). This test ships in this repo —
  `test/tools/weave-write-window-race-live.test.ts` — run it yourself.

## Requirements

- **`sem` on `PATH`**, speaking MCP protocol 2024-11-05 over stdio (`sem mcp`).
- **A built `weave-mcp` binary.** Resolved from `PATH` by default (`weave-mcp`);
  point at a specific binary with `PI_SEM_WEAVE_MCP_BIN=/path/to/weave-mcp` if
  it isn't on `PATH`. Build one with
  `cargo build --release -p weave-mcp -p weave-cli -p weave-driver`.
- **A git repository at the tool's working directory.** Both `sem mcp` and
  `weave-mcp` resolve file paths against a git repo root; live coordination is
  skipped (not blocked) outside one.

## Install

```bash
pi install /path/to/sem/pi
```

or, to try it without installing:

```bash
pi -e /path/to/sem/pi
```

Either way, pi loads `pi/extensions/pi-sem.ts` and registers the tools below on
`session_start`.

## Running modes

Compose the tool surface with your own `pi` binary via `PI_SEM_MODE` and
`PI_SEM_PURE`, both read once at extension load:

```bash
# code mode, pure codespace (only the sem surface; no bash/write)
PI_SEM_MODE=code pi -e /path/to/sem/pi

# code mode with the user's native tools restored alongside
PI_SEM_MODE=code PI_SEM_PURE=0 pi -e /path/to/sem/pi

# tools mode (default): sem/weave tools bridged in, replacing the built-ins they supersede,
# user's other tools untouched
pi -e /path/to/sem/pi
```

- **Tools mode** (`PI_SEM_MODE` unset or anything other than `"code"`) registers
  ten tools directly (`weave_edit`, `sem_outline`, `sem_read`, `sem_find`,
  `sem_grep`, `sem_callers`, `sem_graph`, `sem_path`, `sem_hotspots`,
  `sem_cochange`) plus whatever `src/config/allowlist.json` curates from
  `sem mcp`/`weave-mcp` (`sem_impact`, `sem_diff`, `weave_impact`), and turns off
  pi's own `read`/`edit`/`grep`/`find`/`ls` in favor of them. `bash` and `write`
  stay active, each wrapped by an audit policy (see
  [`PI_SEM_STRICT`](#environment-variables) below); nothing about any other
  installed pi package's own tools is touched.
- **Code mode** (`PI_SEM_MODE=code`, or `PI_SEM_PURE=1`) replaces that whole
  surface with one tool, `sem_code`: the model writes a short async script
  against a typed `sem` API (`src/codemode/sem-api.d.ts`, injected verbatim
  into the system prompt) instead of issuing one small tool call per lookup.
  No bridged MCP servers start in this mode.
  - **Pure** (the default once code mode is on -- `PI_SEM_PURE` unset, or `1`):
    the active tool set is exactly `[sem_code]`, no `bash`/`write` builtin.
    `sem.write` is refused (`sem.add` is the one creation door); `sem.check`
    only runs a detected project runner or a command explicitly allowlisted via
    `PI_SEM_CHECK_ALLOW`.
  - **`PI_SEM_PURE=0`** ("mixed mode") restores `bash`/`write` next to
    `sem_code` -- for benchmark-comparability runs or workflows that still want
    a general shell. The system prompt then carries an extra block
    (`CODE_MODE_MIXED_ADDENDUM`, rendered only in this mode) stating the
    division of labor: every code-content operation -- finding, grepping,
    reading, and *every* edit -- still goes through `sem`, and `bash` is for
    execution only (reproduce, run a snippet, run the real tests, provision what
    running needs, read-only `git log -S`/`git blame`). Pure mode's own block is
    unchanged.

Add `--no-extensions` to any of the above if you also want to exclude any other
installed pi package's own tools for a fully locked-down session.

## Environment variables

- **`PI_SEM_MODE`** -- `"code"` switches to code mode. See
  [Running modes](#running-modes).
- **`PI_SEM_PURE`** -- code mode's tool-surface switch. Unset or `"1"` is pure
  (`[sem_code]` only); `"0"` restores `bash`/`write`; `"1"` also forces code
  mode on even when `PI_SEM_MODE` is unset.
- **`PI_SEM_CHECK_ALLOW`** -- extends pure mode's `sem.check({cmd})` allowlist
  beyond the auto-detected runners (`npm`/`yarn`/`pnpm`/`bun`, `cargo
  test`/`build`/`check`/`clippy`, `pytest`, `go test`/`build`/`vet`, `make
  <target>`). A colon- or comma-separated list of command prefixes, e.g.
  `PI_SEM_CHECK_ALLOW="just test:tox -e py311"`. Matching is a leading-token
  prefix match (`"cargo test --release"` is allowed by `"cargo test"`); an
  unrecognized `cmd` is refused outright, naming the full allowed set.
- **`PI_SEM_STRICT`** -- `1` switches the `bash`/`write` audit wrapper (tools
  mode) from logging-only to actively refusing a matched command or write, and
  additionally refuses code mode's `sem.write(..., { overwrite: "force" })`
  bypass. Unset (or anything other than `1`) keeps both audit-only: every
  command/write is still classified and logged, nothing is blocked.
- **`PI_SEM_PROMPT`** -- selects code mode's system-prompt shape: `"table"`
  (a compact question -> verb table alone), `"dts"` (the table plus the full
  `sem-api.d.ts`), or the default, `"recipes"` (the `dts` shape plus five
  worked examples -- the measured most stable choice across task shapes).
- **`PI_SEM_WEAVE_MCP_BIN`** -- overrides which `weave-mcp` binary this
  package talks to, in both tools mode's bridged server and code/tools mode's
  live coordination. Falls back to `weave-mcp` on `PATH`.
- **`PI_SEM_CONFIG`** -- path to a config file that replaces the default
  tools-mode allowlist entirely (`.json`, or a `.ts`/`.js` module exporting the
  config as `default` or `config`). See `src/config/types.ts` for the shape.
  If set but unreadable or malformed, the session falls back to a fail-closed
  config (no bridged MCP-server tools; native tools plus `bash`/`write`
  remain) rather than silently widening to the default allowlist.
- **`PI_SEM_ROUTINES_TRUST`** -- `"all"` trusts every saved routine's replay
  unconditionally, bypassing the per-name trust gate (a routine saved by
  `sem.routine.save` this session, or listed in `.sem/routines.trust`, is
  trusted by default; an untrusted routine still replays read-only).
- **`PI_SEM_AGENT_ID`** -- the identity `weave_edit`/code mode announce to
  `weave-mcp`'s claim/release/status coordination. Defaults to a random
  per-process id; set this for a stable id across restarts, or when running
  several pi-sem agents against the same repo.

## Declaring a pin

`node scripts/verify-pin.mjs <sha>` (or `npm run verify:pin -- <sha>`) checks a
sha the way you'd want before calling it a pin -- a release tag, a "safe to
build on" commit for another lane -- by creating a clean, detached git worktree
at that sha and, only against that, running `npm install`, `npx tsc --noEmit`,
`npm test`, a real `pi --no-extensions -e extensions/pi-sem.ts` load, and a
static check of the registered tool surface. Unlike running those checks
against your own working tree (which sees whatever is on disk, tracked or
not), this catches a file that exists locally but was never committed while
something committed already imports it. Reports pass/fail per step, cleans up
the worktree unconditionally, and exits nonzero if any step failed.

`test/scripts/verify-pin.test.ts` covers `verify-pin.mjs` itself, but is
deliberately not part of the default `npm test` (it takes tens of seconds and
needs `pi`/network for `npm install` inside its temp worktrees). Run it
explicitly with `npm run test:scripts`.

## Development

```bash
cd pi
npm install
npm run typecheck
PI_SEM_WEAVE_MCP_BIN=/path/to/weave-mcp npm test
```
