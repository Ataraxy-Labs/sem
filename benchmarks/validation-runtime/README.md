# Experimental validation runtime

Persistent TypeScript diagnostics and overlapping independent checks for agent
benchmarks. These are research adapters, not new Sem commands or production
compiler certification. They do not change `sem check` or Weave defaults.

## What is included

- `compiler-session-ts/project-worker.cjs`: one persistent no-emit TypeScript
  worker per project, using the project's installed compiler. Content and
  filesystem observations invalidate reused diagnostics, including negative
  lookups, package metadata and root-file changes.
- `bin/bench-exec`: synchronize a Git checkout into a **disposable** Docker
  container's `/testbed`, then run a command under the validation workspace lock.
  It resets that container checkout; never point it at a development container.
- `bin/bench-validate`: run up to two independent read-only checks after one
  synchronization. Preserve their full output and exit statuses; either failure
  fails the batch. Do not combine generators, formatters, or checks sharing
  mutable output artifacts.
- `bin/bench-format`: apply installed Prettier output to explicit host files
  after checking that their contents still match the inputs.
- `ab/typescript-adaptive-validation.txt`: the experimental agent policy.

## Use in a benchmark harness

Requirements: Python 3.11+, Node 22+, Git, Docker, and a disposable Linux
container with Bash, Python, Node, GNU timeout and the project's dependencies
already installed. The host checkout and container checkout must have matching
base content. No network access is needed during validation.

Copy `project-worker.cjs` and `compiler-observations.cjs` into the container's
`/opt/sem-compiler/`, and install `compiler-session-ts/bench-tsc` as
`/root/.local/bin/bench-tsc`. Add this package's `bin` directory to the **host**
PATH and set `BENCH_CONTAINER`, `BENCH_REPO` (absolute host checkout), and
`BENCH_BASE` (its base commit). Run from the host checkout:

```sh
bench-exec 'bench-tsc --capture-baseline -p tsconfig.json'
# Make the change through the agent's Sem/Weave interface, then:
bench-validate 'bench-tsc -p tsconfig.json' 'npx jest --runInBand --runTestsByPath path/to/test.ts'
```

`bench-tsc` exists inside the container, not on the host. For baseline overlap,
the harness sets `BENCH_EXEC_READY_FILE` on the baseline invocation and waits for
that marker before launching the agent. The marker means synchronization has
completed while the workspace lock is held. Start timing **before** baseline
launch, join that process before stopping the session timer, and collect its
result. Never exclude baseline compilation to manufacture a speedup.

## Boundaries

Use a pinned toolchain and cooperative serialized source writes. Restart workers
after replacing the compiler/plugins/dependencies. Project references and
compiler plugins are rejected. This is no-emit checking, not behavioral testing,
declaration generation or bundling. Two matching baseline errors still mean a
failed compiler check; the baseline is comparison evidence, not permission to
return success. Edits require affected revalidation. Receipts and sockets live
in the disposable environment's `/tmp`; this is not a multi-tenant service.

The formatter assumes exclusive/cooperative host edits. Its per-file replacements
are atomic, but the whole batch is not a multi-writer transaction. Shell commands
are trusted harness inputs, not a sandbox boundary.

## Evidence

On `ant-design__ant-design-53739`, one completed session took **269.1 seconds**
versus **326.4 seconds** for native: **17.6% less elapsed time**. Both passed the
unchanged official evaluator. Model: gpt-6-astra, medium; Codex 0.160.1;
Sem 0.28.1; Weave MCP 0.6.1; 16 CPUs / 64 GB per sandbox. Baseline compilation
is included; preparation and official grading are excluded for both arms.

This is one successful sample after tuning, not a consistent speedup claim.
Slower completed trials are retained in `results.json`. Input tokens increased
from 2,118,748 to 3,036,843, including cached input: **not a token-efficiency win**.
The winning run retained two pre-existing compiler errors. Later compiler checks
took about 3–4 seconds; tests and repair decisions still dominated some runs.

Both arms disabled Jest caching because the packaged transformer returned a
constant cache key. Do not compare a cached arm with this control. Test patches
and model trajectories differed, so the entire time difference cannot be
attributed to one scheduling change. A same-patch single-file probe measured only
about 20.8s with workers versus 20.2s in-process; it did not explain a large gain.

This package does not include the full model harness, benchmark task, credentials
or raw private session logs. It preserves the reusable validation components and
aggregate results, not a standalone reproduction of the end-to-end session.

## Checks

```sh
node compiler-session-ts/test-observations.cjs
python3 compiler-session-ts/test-validation-batch.py
```

The CI fixture installs TypeScript 5.8.3 under `/testbed`, copies the worker to
`/opt/sem-compiler`, then runs `probe-project.cjs --fixture-only`. It compares
diagnostics to a fresh compiler across changes to source, roots, configuration,
imports, package resolution, and missing dependencies. The larger project/replay
probe is optional and requires the benchmark checkout.
