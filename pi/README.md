# pi-sem

A [pi](https://pi.dev) package that replaces pi's file-based `read`, `edit`,
`grep`, `find` and `ls` with tools that address code by entity: a function,
class or method found by name rather than by file and line range. Reads come
from [sem](https://github.com/Ataraxy-Labs/sem), which parses the repository
into entities and the calls and imports between them. Edits go through
`weave-mcp`, which applies them to a named entity and merges concurrent edits
from other agents instead of overwriting them.

Use it when the agent spends its turns locating code: grepping for a name,
opening the file, grepping again for callers. It helps least on tasks that name
the exact file and line, and on repositories whose languages sem does not
parse.

## Requirements

- `sem` on `PATH` (the package starts `sem mcp`).
- A `weave-mcp` binary, on `PATH` or named by `PI_SEM_WEAVE_MCP_BIN`. Build it
  from the weave repository with
  `cargo build --release -p weave-mcp -p weave-cli -p weave-driver`.
- A git repository as the working directory. Paths resolve against the
  repository root. Outside one, edit coordination between agents is skipped,
  not blocked.

## Install

```bash
pi install /path/to/sem/pi      # install
pi -e /path/to/sem/pi           # or load for one session
```

## Choosing a mode

The mode is read once, when the extension loads.

| Mode | Set with | Tools the model sees | Use it when |
|---|---|---|---|
| Tools (default) | nothing | Ten entity tools plus `bash` and `write` | You want entity reads and edits but keep a shell for tests, builds and git |
| Code | `PI_SEM_MODE=code` | One tool, `sem_code`, which runs a short script against a typed `sem` API | Tasks need many lookups that would otherwise be many small tool calls |
| Transaction | `PI_SEM_MODE=transaction` plus `PI_SEM_CONFIG` | Only the tools the config allowlists | You want the server, not the prompt, to enforce how reads, edits and checks happen |

### Tools mode

Registers `weave_edit`, `sem_outline`, `sem_read`, `sem_find`, `sem_grep`,
`sem_callers`, `sem_graph`, `sem_path`, `sem_hotspots` and `sem_cochange`,
plus `sem_impact`, `sem_diff` and `weave_impact` from
`src/config/allowlist.json`. pi's own `read`, `edit`, `grep`, `find` and `ls`
are turned off. `bash` and `write` stay on, wrapped by an audit policy that
logs matched commands, or refuses them when `PI_SEM_STRICT=1`. Tools from other
pi packages are left alone. Add `--no-extensions` to exclude them too.

`weave_edit` replaces a whole entity with the source you give it. Send the
complete new function, not a fragment.

### Code mode

The model writes an async script against the API in
`src/codemode/sem-api.d.ts`, which is injected into the system prompt. One
script can find an entity, read its callers and edit all of them, where tools
mode needs a call for each step. No MCP servers are started in this mode.

Code mode is pure by default: `sem_code` is the only tool, with no `bash` or
`write`. In pure mode:

- `sem.write` is refused. Create files with `sem.add`.
- `sem.check` runs only a detected project runner (npm, yarn, pnpm, bun,
  `cargo test|build|check|clippy`, `pytest`, `go test|build|vet`,
  `make <target>`) or a prefix you allow with `PI_SEM_CHECK_ALLOW`. Anything
  else is refused, and the refusal lists the allowed commands.

Set `PI_SEM_PURE=0` to restore `bash` and `write` next to `sem_code`, for
example in benchmarks that compare against a shell-equipped agent.

### Transaction mode

Starts only the MCP servers named in the config, exposes only their allowlisted
tools, and disables pi's builtins, `sem_code` and the lower-level sem and weave
tools. If a server fails to start, the session has no tools rather than
falling back to unrestricted ones.

The bundled config is `config/simple-transaction.mjs`. It needs the package's
dependencies installed, so run pi from this checkout:

```bash
cd /path/to/sem/pi && npm install
cd /path/to/project
PI_SEM_MODE=transaction \
PI_SEM_CONFIG=/path/to/sem/pi/config/simple-transaction.mjs \
/path/to/sem/pi/node_modules/.bin/pi -e /path/to/sem/pi
```

It exposes four tools and no shell:

| Tool | Does |
|---|---|
| `sem_plan` | Finds definitions by name or regex and reads them, with imports and class member lists. Can also read small files such as configs and tests. |
| `sem_exact` | Batched exact reads by selector; edits to captured entities checked against the file hash; `transform`, which applies one literal replacement to explicit targets with an exact expected count per entity. |
| `weave_transaction` | Applies a batch of edits, creates and import changes atomically, then runs one validation command. |
| `weave_program` | Generates repetitive edits from a short JavaScript function over source already read, then applies them like `weave_transaction`. |

Behavior that surprises people:

- Stale edits fail instead of applying. `sem_exact` (including `transform`)
  and `weave_program` check the file hash captured at read time, and `weave_transaction` requires each
  `old` anchor to match the current file exactly once. Re-read before
  retrying; resending the same edit fails the same way.
- A rename or signature change needs `allow_signature_change: true`. Without
  it, the identity guard rolls back the whole batch.
- When a batch deletes an entity or changes its signature, the response
  includes `caller_review.unedited_dependents`: references the batch did not
  update. It comes from the syntax-level graph and is a review list, not a
  compile result.
- Check results are not reused unless an operator sets
  `SEM_VALIDATION_INPUT_KEY_ARGV` to a trusted command that fingerprints every
  input the check depends on. Only passing results are reused, they report
  `validation_cache.reused: true`, and the key is rechecked before reuse.
- `validation_cmd` takes one project command (pytest, cargo, go, npm, gradle
  and similar). Shell operators, `python -c` and heredocs are rejected.

Setup details, tests and measured results are in
[src/transaction/simple/README.md](src/transaction/simple/README.md).

### Deciding the mode before a session

Starting the structural tools costs time a localized task does not repay.
`npm run route` reads the task from stdin, as raw text or as a JSON envelope,
and prints a decision:

```bash
printf '%s' '{"task":"Rename the API and update all callers","files":["src/api.ts","src/client.ts"],"index_warm":true}' \
  | npm run --silent route
```

Attach the structural tools only when `attach_structural_tools` is `true`.
Cross-entity tasks such as renames and "update all callers" route to
structural mode. Tasks that name a single source path, or that change data or
the environment, route to native. If routing itself fails, the decision is
native, so an adapter can always start the agent.

## Environment variables

| Variable | Effect |
|---|---|
| `PI_SEM_MODE` | `code` or `transaction`. Anything else is tools mode. |
| `PI_SEM_PURE` | Code mode only. Unset or `1` means `sem_code` alone; `0` adds `bash` and `write`. `1` also turns on code mode when `PI_SEM_MODE` is unset. |
| `PI_SEM_CONFIG` | Path to a config (`.json`, or a `.ts`/`.js` module exporting `default` or `config`) that replaces the tools-mode allowlist, or selects the transaction server. An unreadable or malformed config fails closed: no bridged tools, never the default allowlist. Shape in `src/config/types.ts`. |
| `PI_SEM_CHECK_ALLOW` | Extra command prefixes for pure-mode `sem.check`, separated by `:` or `,`. Matching is by leading tokens, so `cargo test` also allows `cargo test --release`. |
| `PI_SEM_STRICT` | `1` makes the tools-mode `bash`/`write` policy refuse matched commands instead of only logging them, and refuses code mode's `sem.write(..., { overwrite: "force" })`. |
| `PI_SEM_PROMPT` | Code mode prompt shape: `table`, `dts`, or `recipes` (default, which measured most stable across tasks). |
| `PI_SEM_WEAVE_MCP_BIN` | Path to the `weave-mcp` binary. Defaults to `weave-mcp` on `PATH`. |
| `PI_SEM_ROUTINES_TRUST` | `all` trusts every saved routine. By default only routines saved this session or listed in `.sem/routines.trust` run with write access; others replay read-only. |
| `PI_SEM_AGENT_ID` | The identity weave uses for edit claims. Random per process by default. Set it when several agents share a repository and you need stable ids across restarts. |

## Where it can fail

- **Parser coverage.** Entities come from tree-sitter grammars. Syntax a grammar
  does not know, macros and generated code can produce missing or mis-bounded
  entities. Tools report these as gaps; check them before trusting an empty
  result.
- **Call links are syntactic.** sem links calls by name and scope, not by type.
  Dynamic dispatch, dependency injection and name collisions (a method and a
  field with the same name) can add or drop callers. Treat `sem_callers` and
  `caller_review` as strong hints, and let the compiler and tests decide.
- **Concurrency is per process.** Edit queues and file-hash checks prevent lost
  writes within one weave process. Agents in separate processes need weave's
  claim coordination, which requires a git repository.
- **Rollback is compensating.** A failed batch is undone by restoring the files
  it touched. It is not a repository-wide snapshot, so a concurrent writer can
  still interleave.

## Evidence

- **Code mode without a shell.** On 10 SWE-bench Verified tasks, graded by the
  official harness in a network-shimmed environment, pure code mode resolved
  9, the same as the same agent with full bash. All 87 tool calls were
  `sem_code` operations. This shows the interface is sufficient, not that it
  is better. One run.
- **Concurrent edits.** Two processes editing disjoint functions in the same
  file lost no edits in 1,000 raced attempts. Plain last-writer-wins writes
  lost an edit in 80 to 100 percent of runs on the same harness. The test is
  `test/tools/weave-write-window-race-live.test.ts`.

## Verifying a pin

`npm run verify:pin -- <sha>` checks a commit in a clean detached worktree:
`npm install`, `tsc --noEmit`, `npm test`, a real
`pi --no-extensions -e extensions/pi-sem.ts` load, and the registered tool
list. It catches files that exist in your checkout but were never committed.
The worktree is removed afterwards, and the command exits nonzero if any step
failed. Its own tests take tens of seconds and need network access, so they run
separately with `npm run test:scripts`.

## Development

```bash
cd pi
npm install
npm run typecheck
PI_SEM_WEAVE_MCP_BIN=/path/to/weave-mcp npm test
npm run test:simple    # transaction-mode server
```
