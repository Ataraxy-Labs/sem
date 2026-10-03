# Simple session policy (experimental)

Opt-in packaging of the September 24 PlantUML benchmark adapter. This does not
replace the default transaction server or its protocol-v2 revision contract.
It removes fixed discovery/edit call quotas and mandatory discovery-before-edit;
uses batched reads, acknowledged context reuse, scoped edits and coherent-batch
validation. Response size and snapshot-storage limits still apply.

## Run

Requires Node with TypeScript stripping (tested on Node 23.7), SEM 0.25.0 on PATH,
and the dependencies installed with `npm ci` in `sem/pi`. The benchmark used a
0.25.0 development build including the Python decorated-entity fix, not 0.22.1.

From the repository to edit:

```sh
sem graph --json > /dev/null
PI_SEM_MODE=transaction \
PI_SEM_CONFIG=/path/to/sem/pi/config/simple-transaction.mjs \
/path/to/sem/pi/node_modules/.bin/pi -e /path/to/sem/pi
```

For another MCP client, launch:

```sh
SEM_EXACT_TOOLS=1 node --experimental-strip-types /path/to/sem/pi/src/transaction/simple/sem-session-simple-mcp.mjs
```

Set its working directory to the target repository. The four tools are
`sem_plan`, `sem_exact`, `weave_transaction`, and `weave_program`. The companion
config contains the recommended agent instructions. Prewarming is explicit and
optional; subsequent index freshness is lazy, not an always-running daemon.

The config also includes shared simple-efficiency guidance from
`efficiency-policy.mjs`: reuse read handles, batch known targets, avoid unrelated
cleanup, and finish after required final-patch checks rather than rerun solely
for verbose reporting. Other clients can import `simpleEfficiencyPolicy` and
append it to their agent instructions. This is guidance, not runtime enforcement;
it adds no formatter, validation cache, or parallel-write guarantee.

This guidance was replayed with a separate context-on-request development adapter.
Publishing the guidance alone does not reproduce that adapter or its benchmarks.
Results varied by task; it is not a universal speed or token-efficiency claim.

## Validation and limits

### Session efficiency

- `sem_exact query` reads batches of entity IDs directly. `apply` already accepts
  disjoint entities within the same file; overlapping edits fail before mutation.
- Exact query/read responses include source `receipt` values. Send only receipts
  still available in the agent's context as `known_receipts` to omit unchanged
  bodies. `refresh_source:true` restores full source. IDs and snapshot metadata
  remain explicit; acknowledged source is not evidence of current filesystem state.
  Compacted child slices keep their same-response parent body available.
- `sem_exact diff` accepts `since_diff` plus the identical explicit `files` scope.
  It returns content changes since that captured review, including additions and
  deletions, instead of repeating the whole HEAD diff. Expired baselines fail
  explicitly. Delta reviews do not represent permission/mode changes.
- Cold captures parse at most four independent files concurrently. Content-keyed
  per-file caching retains unchanged parses when other files change. Snapshot IDs
  and deterministic output ordering are unchanged. This is session-local reuse,
  not a shared daemon or a compiler dependency graph.
- Relationship expansion stays opt-in (`expand_context:true`); these changes do
  not add unsolicited neighbor bodies.

Validation result reuse is **disabled by default**. An operator may configure
`SEM_VALIDATION_INPUT_KEY_ARGV` as JSON argv for a trusted, external fingerprint
provider. It receives the exact check command as its final argument and writes a
nonempty key to stdout. Do not let the agent supply this provider. It must attest
**all** validation inputs: source and generated files, missing/untracked files,
resolved dependencies, build configuration, toolchain, environment and mutable
runtime state. A Git revision/diff alone is insufficient. Only use this contract
with deterministic/hermetic checks and an immutable provider outside the edited
repository. If that cannot be guaranteed, leave it unset; normal checks run.

Only successful `stage:test` results are cached, in a bounded session-local cache.
Keys are rechecked before reuse and after execution. Changed/unavailable keys
during execution downgrade the verdict to `pass:null`; failures are never cached.
Reused results explicitly report `validation_cache.reused:true`, `executed:false`
and their original duration. This is reuse of attested evidence, not a fresh test
or general proof. It does not replace compiler incremental builds or discover the
correct test subset automatically.

Run `SEM_TEST_BIN=/path/to/sem node src/transaction/simple/benchmark-session-efficiency.mjs`
from `pi` for a cold-parse/changed-file/response-size microbenchmark. Agent-session
speedups require separate repeated, graded benchmark runs; no new end-to-end
speedup is asserted by this implementation.

Local validation uses SEM's check API. The optional benchmark Docker checker
requires Python 3 (`SEM_PYTHON` overrides the executable) and all of
`SEM_VALIDATION_IMAGE`, `SEM_VALIDATION_CONTAINER`, `SEM_VALIDATION_CWD`, and
`SEM_VALIDATION_BASE`. Use a disposable image/container with the repository at
`/testbed` and dependencies already installed. Container cleanup belongs to the
caller. Do not use a production container or mount sensitive host resources.

Run only on trusted repositories in an appropriately sandboxed agent environment.
This server is not a security sandbox. Builds execute project code. It includes
text fallbacks, not exclusively graph-native operations. Exact edits check file
hashes and entity ranges inside Weave's process-local mutation queue; this is not
cross-process isolation. General transactions do not promise whole-repository
snapshot isolation. Rollback is compensating; concurrent writers need external
coordination. Parser coverage and passing tests do not prove semantic completeness.

## Evidence

One paired PlantUML SWE-Bench ProMax task (`plantuml__plantuml-2173`), same model
(gpt-6-astra, medium), both independently graded SUCCESS:

| | Native | Simple SEM+Weave |
|---|---:|---:|
| Session seconds | 398.759 | 287.919 |
| Total tokens including cached input | 1,931,171 | 1,854,702 |
| Separate index prewarm seconds | — | 1.776 |

27.8% shorter session, 4.0% fewer total tokens. Grading time is excluded. Including
index preparation gives 289.695 seconds. Noncached input was higher for SEM+Weave;
this is not a claim of lower billed cost. The solutions differed, and one pair
does not establish causality or consistent superiority. Cargo remained slightly
slower and Angular failed grading in the related trials. No universal speed claim.
The Pi config is an integration of the tested policy, not a new measured Pi trial.

## Tests

From `sem/pi`, with SEM 0.25.0 on PATH (or `SEM_TEST_BIN` set to its absolute path):

```sh
npm run test:simple
npm run typecheck
python3 -m unittest discover -s src/transaction/simple -p 'test_*.py'
```
