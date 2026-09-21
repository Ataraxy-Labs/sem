# Shared repository MCP runtime

Run `sem mcp` with the repository as the working directory, or set `SEM_REPO`
to its absolute path. Any client supporting newline-delimited MCP over stdio
uses the same command and tool schemas. Sharing requires no model-specific
instructions or client adapter.

On macOS and Linux, clients connect to a shared daemon under the checkout's
Git metadata directory. Linked worktrees have separate daemons. The daemon
shares parsing, graph caches, and file watching. Each connection receives an
independent context-history ledger; a response previously sent to one agent
does not suppress the first response to another. Absolute paths into a
different repository are rejected: create another MCP connection for that repo.

`sem mcp --status` performs an MCP initialize exchange without starting a
daemon, and prints JSON with `ready`, `unavailable`, `no_repository`, or
`unsupported`. A successful socket connection alone does not establish health.

The daemon holds an OS file lock for its lifetime, automatically recovers stale
sockets after a crash, limits simultaneous clients to 64, and expires after ten
idle minutes. Handshakes time out after ten seconds. Runtime directories and
sockets have owner-only permissions. The v2 socket separates this implementation
from the original daemon's shared session-history behavior.

Set `SEM_MCP_NO_SHARED=1` to use an independent stdio process. Startup failures
normally fall back to this mode and emit a notice on stderr. Set
`SEM_MCP_REQUIRE_SHARED=1` for benchmarks where fallback must be an error.
Windows currently uses standalone stdio; named-pipe sharing remains unimplemented.
The OS file-lock API requires Rust 1.89 or later to build.

## Correctness boundary

This service shares **read infrastructure**. It does not provide write isolation,
an immutable repository snapshot, or a cross-agent write lock. File watching is
not a substitute for validating the revision immediately before applying edits.
The separate Pi transaction server validates revision-pinned writes; that does
not make arbitrary writes through other tools transactional. Compiler and test
validation are still required for semantic correctness.

## Reproduce the transport benchmark

```sh
cargo build --manifest-path crates/Cargo.toml -p sem-mcp -p sem-cli
python3 benchmarks/shared-mcp/run.py --binary crates/target/debug/sem-mcp
```

The script creates a temporary 41-file repository containing 1,001 Python
functions, checks eight simultaneous clients, independent context history,
stale-socket recovery, and crash recovery. It alternates ten standalone processes
with ten shared-daemon clients against a warm disk cache. JSON output includes
raw latency samples and response sizes. The script terminates its own daemons.
CI runs these checks on macOS and Linux. Timing is descriptive, not a CI gate.

This measures startup plus one context request, not completed coding tasks.
It cannot establish model token savings or end-to-end agent speedups. Those
require paired runs with identical model versions, tasks, budgets, and official
task validators, reporting success rate and total tokens alongside latency.
