# Code mode

One NATIVE tool, `sem_code`, replaces pi-sem's own ~9-tool surface for a
session started with `PI_SEM_MODE=code`. **This is not a claim that
`sem_code` is the only tool reachable in that session** — a live
weave_impact tool call was traced through a code-mode run (and confirmed
in a baseline run's own tool-call histogram): bridged MCP-server tools
(e.g. `weave_impact`, registered separately via `config.servers`, not
through pi-sem's own `registerTool` calls) stay reachable regardless of
`PI_SEM_MODE`, since neither `--no-extensions` nor `--no-builtin-tools`
touch that registration path. Pin the surface down to `sem_code` alone
with pi's own `--tools sem_code` allowlist if that's the guarantee a
comparison actually needs. The model writes a short async script against
the `sem` API (`sem-api.d.ts`, injected verbatim into the system prompt
below the addendum) instead of issuing one tiny tool call per lookup — the
measured cost driver was turn count, not result size. The win is
shape-specific, not universal: code mode has a clear edge on graph-shaped
tasks (blast radius/choke point — ~1/9 vanilla's tokens, ~1/3 its wall
time, same correctness) and on token-efficiency questions (narrower gap
than tools-mode's schema overhead); it runs roughly even with tools-mode
on nav/single-file-edit tasks; and it lost on the one expensive cross-file
task (`ts-multifile-01`: 961k tokens vs. tools-mode's 432k), where the
model has a recurring habit of following a successful
`sem.write`/`sem.edit` with a native `apply_patch` re-touch anyway. That
comparison ran with a third-party extension providing `apply_patch`/
`exec_command` loaded on every arm from each session's own local settings,
not suppressed via `--no-extensions` — a clean re-baseline with that flag
set is worth running to isolate whether the loss is inherent to code
mode's routing or an artifact of that extra tool surface being available
at all.

## API surface

`sem-api.d.ts` is the contract. It wraps existing native implementations
(`../tools/internal/*`, `../tools/sem-*.ts`'s `performXxx`) — `api.ts` never
reimplements entity resolution, splicing, verification, or identity checks;
it only shells out via the one shared `runCommand` (`internal/proc.ts`) for
the CLI verbs with no existing wrapper (`impact`, `diff`; `callers` now
reuses the native `sem-callers.ts`). `edit` reuses `performWeaveEdit`
unchanged; `write` reuses `write-audit.ts`'s classifier (see "write() safety
contract" below — this was NOT true before review pass 3, see Bug log).
Every function either returns the same compact JSON `details` object the
equivalent native tool already returns, or throws an `Error` whose
`.message` is that tool's existing refusal text — scripts can `try/catch`.

`edit(request | request[])`: the array form delegates directly to
`performWeaveEdit`'s own native `edits=[...]` batch primitive (not a
client-side fan-out) — entities run in declared order, each through its
full claim/verify/dependents/release lifecycle; one failing edit is
reported inline (`{error, file, entity}` at that array index) and never
aborts the rest.

**Field-completeness note**: `sem-api.d.ts`'s promised shape and each
function's actual return value have diverged more than once (see Bug log) —
`headers()`/`read()` (fixed pre-review-pass-3) and `find()`/`grep()`/
`diff()` (fixed in review pass 3) all shipped with a real mismatch between
what the type said and what a script actually got back. There is no
mechanical check tying the two together; `test/codemode/
api-field-completeness.test.ts` is the accumulated regression suite, grown
by audit each time a mismatch is found, not a general guarantee that no
others remain.

**write() safety contract**: `write(path, content, { overwrite })` routes
through `write-audit.ts`'s `auditWriteCommand` — the SAME classifier
(`isCodeFilePath`, `PI_SEM_STRICT`) the builtin tools-mode `write` tool
wraps — but layers a stricter rule on top for code files specifically: an
EXISTING code file's overwrite needs `sem.edit()`'s identity/visibility
checks, so plain `{ overwrite: true }` (the ordinary/careless call shape) is
refused with "use sem.edit for existing entities" regardless of strict
mode. Only `{ overwrite: "force" }` bypasses it, and even that is refused
outright under `PI_SEM_STRICT` (force is not a strict-mode override — it
only lifts the extra code-file gate `sem.write()` adds beyond what the
builtin's own strict check does). A non-code file, or any brand-new file,
keeps the simple `overwrite: true` contract. Every call — refused, forced,
or ordinary — is reported through `SemApiDeps.onWriteAudit`, which
`extensions/pi-sem.ts` wires into the SAME `writeAuditEntries`/
`pi.appendEntry("pi-sem-write-audit", ...)` path the builtin write wrapper
feeds, so a force-bypass shows up in the same session-summary line rather
than being invisible next to the builtin's own audited writes.

## Sandbox contract

`node:vm`: `vm.createContext(frozenGlobals, { codeGeneration: { strings:
false, wasm: false } })`. `frozenGlobals` = exactly `{ sem, console: { log:
capture } }` — no `require`/`import`/`process`/`fetch`/`module`/`global`.
Wall-clock timeout (default 60_000ms) races the returned promise against a
timer, since vm's own `timeout` only bounds the initial *synchronous* burst,
not awaited work. Output is captured `console.log`/`sem.log` text, capped
~8k tokens. A call counter wraps every `sem.*` function (log exempt); the
201st call throws before the real function runs. Every call that DOES reach
the real function is recorded in `calls: {fn, ok, error?}[]` — the basis for
`apiCalls`/`edits` reporting below. Errors return structured
(`{message, line}`), not a bare throw.

**Two real escapes found and closed, proven empirically, not assumed**
(full writeup in `sandbox.ts`'s file header): (1) a HOST-REALM function
exposed into the context (any `sem.*` bridge) carries `.constructor` back to
the host's unrestricted `Function` — `codeGeneration:{strings:false}` does
NOT block this (proven with a throwaway probe: a marker on `process.env`
leaked straight through). Closed by compiling every capability as a
context-native trampoline (`vm.compileFunction` + `parsingContext`),
explicitly constructing `new Promise`/`new Error` around the host call's
outcome (never returning `.then()`'s species-inherited promise), and
round-tripping resolved values through context-native `JSON.stringify`/
`parse`. (2) An arrow-function script wrapper's inherited `this` reached an
unrestricted intrinsic through NO prototype fixable that way; closed by
switching to a strict-mode non-arrow `async function` IIFE (top-level `this`
becomes hard `undefined`). Both vectors are RED-tested in
`test/codemode/sandbox-escapes.test.ts` with a planted `process.env` marker
asserted never to leak — that test is the actual arbiter, not this prose.
Plain `node:vm` turned out sufficient once both were closed; `worker_threads`
was not needed.

**Disclosed, not-claimed-closed gap**: a script that busy-loops synchronously
*after* its first `await` can't be preempted from this single thread by
anything here — closing that needs a separate, terminable worker thread.
Out of scope; not exercised by a RED test (the async-hang test uses an
unresolved promise, which the outer race does catch).

**A third vector, closed in review pass 3**: the value a script *returns*
may carry its own `toJSON()`/`valueOf()` — the original code ran
`JSON.parse(JSON.stringify(value))` on the HOST thread, after the timeout
race had already resolved, completely unbounded by `timeoutMs`. Proven
empirically before fixing (throwaway `vm` probes, not assumed): `vm.Script`'s
native `timeout` DOES forcibly interrupt a flat, non-async script that
stringifies a busy `toJSON()` — but that protection evaporates the instant
the call is reached through *any* `await`/`.then()` boundary, even a bare
`Promise.resolve()` with zero real host-side async work. Fixed by running
`sanitizeReturnValue` as its own separate, synchronous `runInContext` call
in the *same* context the value belongs to — never chained onto the raced
script-execution promise. This closes the vector for exactly the review's
reproduction (no preceding `await`); a value whose busy `toJSON()` is only
reached *after* the script's own code crosses a real host-boundary `await`
remains inside the disclosed gap above, unchanged by this fix — not newly
introduced, not made worse. RED-tested in
`test/codemode/sandbox-return-value-toJSON-escape.review.test.ts` (fails
closed with a timeout error at ~200ms, not a 2s hang).

**A fourth vector, same class, closed for logged values too**: a value
passed to `console.log`/`sem.log` may carry the same hostile
`toJSON()`/`toString()`. Checked empirically before fixing: a host-realm
stringify call made synchronously from an active `runInContext` execution
(no preceding await) was already covered by vm's native timeout — the
gap is exactly the disclosed one above, not a new hole — but
`compileVoidTrampoline` now stringifies context-natively regardless, for
the same reason `sanitizeReturnValue` does: never call host-realm
`toJSON`/`toString` on a sandbox-authored value, anywhere in this file.

**Timeout REVOKES the run, closing the "zombie script" hole**: the timeout
race above only bounds how long `runInSandbox` *waits* — the script's own
promise chain can't be forcibly cancelled and keeps running in the
background. Confirmed live before fixing: a script that awaits something
slower than `timeoutMs`, then calls `sem.write`, lands that write on disk
~300ms *after* the tool already reported `{ok:false, timed out}`. Fixed
via `createRunCancellation` — a SingleWriterCell split (`revoke()`/
`isRevoked()`) `runInSandbox` constructs (or the caller supplies, shared
with `SemApiDeps.cancellation`) and revokes the instant its own timeout
fires. Every `sem.*` trampoline checks `isRevoked()` FIRST, before even
the call-cap; `api.ts`'s `write()`/`editOne()`/`edit()` check it again
immediately before their real disk/coordination step, narrowing (never
fully eliminating — no cooperative single-thread model can) the window
where a call was already in flight when revocation happened.
`SandboxResult.revokedCalls` reports how many refused-after-cancel
attempts happened to resume before the result was returned — honest,
best-effort telemetry, typically 0 for a genuinely-past-timeout run (the
zombie usually resumes well after `runInSandbox` has already returned);
the load-bearing guarantee is that the mutation never lands, not that
this counter is complete. Proven with a real temp file in
`test/codemode/sandbox-limits.test.ts`: absent both immediately after the
tool returns and 800ms later, past the zombie's own resolution.

## Graph-native primitives (`graph`/`path`/`hotspots`/`cochange`/`history`)

Host-algebra finding, verified empirically before designing anything: `sem
impact --depth N` is ASYMMETRIC — `dependencies` (the "out" direction) is
ALWAYS exactly 1 hop no matter what `--depth` is; only `dependents`/
`impact.entities[]` (the "in" direction) is genuinely depth-aware. So `sem
impact` cannot serve a uniform out/in/both hop-neighborhood query at all.
`sem graph --json` (the full entity+edge dump) is the only primitive that
can — measured ~12-220ms warm depending on repo size and query breadth (see
below), well under the 60s sandbox budget, so `graph()`/`path()` fetch it
once per call and do local BFS over its edges in Node, uniformly, in
whichever direction was asked. `hotspots`/`cochange` come from `sem log
--json` (no entity — repo analytics); `history` from `sem log <entity>
--json`; both proven against real shapes, not guessed.

Real timings, live smoke on `weave` (2168 entities/2187 edges,
`entity_merge` — the single most-connected function in the repo, 138 raw
edges, used deliberately as the stress case):

| call | time | result |
|---|---|---|
| `graph(entity_merge, {hops:2})` | 220ms | 162 nodes, 176 edges, not truncated |
| `graph(entity_merge, {hops:1, direction:"out"})` | 271ms | 3 nodes |
| `graph(entity_merge, {hops:5})` | 286ms | capped at 300 nodes, `truncated:true` |
| `path(entity_merge, merge_file)` | 146ms | 3-node chain found |
| `hotspots({limit:5})` | 1558ms | the slowest of the five — `sem log`'s commit scan is the cost, not our BFS |
| `cochange(entity_merge)` | 82ms | 0 pairs (real answer, not an error) |
| `history(entity_merge)` | 303ms | 1 change |

`path()` treats graph edges as UNDIRECTED for BFS — "how are A and B
connected at all" is more useful than a strict call-direction-only
question, and matches how a person would ask it; `max_hops` (default 6)
bounds the search. `graph()`'s neighborhood caps at 300 nodes with
`truncated: true`; both correctly terminate on a real cycle (proven with a
dedicated 3-node-cycle fixture, not just reasoned about) since BFS tracks
`visited` regardless of direction.

## System prompt addendum (9 lines, precedes the `.d.ts`)

Lines 7-8 exist because the paired eval found the remaining gap once code
mode otherwise won everywhere: provider-native tools (apply_patch/
exec_command) firing NEXT TO sem_code — the model explored through us but
edited through a native bypass, skipping verification and weave-mcp
coordination. Line 9 points at the graph-native primitives before a script
falls back to grepping for callers by hand.

```
Code mode: call sem_code with a short async JavaScript program instead of many small tool calls.
The program runs in a sandbox with one global, `sem`, typed below -- no require/process/fetch/filesystem outside it.
Batch everything one turn needs: grep + read + analyze + edit in a single script; return only the final answer via console.log or a returned value.
sem.log(...) streams progress for long scripts; console.log output is what you'll see back, capped at ~8k tokens.
Call sem.edit(request or request[]) for entity edits -- pass an array to touch many entities across files in one call -- and sem.write(path, content) for new files; same safety guards as before.
Wall-clock timeout 60s (override with timeout_ms), max 200 sem.* calls per run -- write efficient scripts, not infinite loops.
All code edits go through sem.edit (or sem.write for new files) inside sem_code -- never through apply_patch/patch/exec_command/bash redirection; those bypass verification and coordination.
Run tests/builds with sem_code? No -- use bash only for running commands (tests, builds, git); reading/searching/editing is sem's job.
For blast radius / dependency chains / change history use sem.graph / sem.path / sem.cochange before grepping.
```

## Tool registration and telemetry

`tool.ts` registers exactly one tool, `sem_code`, params
`{code: string, timeout_ms?: number}`, description (≤160 chars). Its result
`details` carries, beyond `ok`/`truncated`/`callCount`/`value`:
`apiCalls: {count, histogram}` (a per-function call count derived from
sandbox.ts's generic `calls[]`) and `edits: {count, refused, reasons}`
(same log, filtered to `fn === "edit"`) — the "how much work went through
us" signal the eval scores against a provider-native bypass. See
`deriveApiCallStats` in `tool.ts`. A batch `edit(request[])` call reports
one `CallRecord` per entity, not one for the whole call, via sandbox.ts's
`SUB_CALLS` protocol (a non-enumerable symbol property api.ts's `edit()`
attaches to the SAME array it always returned, so a direct caller
bypassing the sandbox sees byte-for-byte the same value as before) — so
`edits.refused`/`reasons` are precise for both single-request and batch
calls, not just `edits.count`.
`before_agent_start` prepends the addendum + `.d.ts` to the system prompt
(same hook `extensions/pi-sem.ts` already uses for `systemPromptAddendum`
— that handler is gated on `!CODE_MODE` so the two addenda never compose).

## Wiring (`extensions/pi-sem.ts`)

`PI_SEM_MODE` env, default `"tools"`. When `"code"`: `registerSemCode(pi,
{...})` replaces the six per-tool registrars, and `activeTools`'s extras
become `[SEM_CODE_TOOL_NAME]` alone (bash/write policy unchanged). Default
behavior (`PI_SEM_MODE` unset) is byte-for-byte what exists today.

## Bug log

Real bugs found and fixed in this area, kept here rather than only in commit
messages since each one is evidence about what kind of mistake this
subsystem is prone to (thin wrapper over native tools, JSON shapes assumed
rather than probed):

- **`headers()`/`read()` field completeness** (pre-review-pass-3, live
  repro): `sem_read`'s `mode:"headers"` nests
  `{name,type,parent_name}` under `.entity` and never exposes
  `signature`/`doc` as top-level fields; `headers()` was spreading that raw
  shape instead of reshaping it. `read()`'s `entity` was separately missing
  `file`. Both fixed; see `api-field-completeness.test.ts`.
- **`sandbox.ts`'s return-value `toJSON()` host-thread escape** (review pass
  3, critical #1): see "Sandbox contract" above.
- **`sem.write()` bypassing every guard `sem.edit()` has** (review pass 3,
  critical #2): `write()` had zero connection to `write-audit.ts`, zero
  `PI_SEM_STRICT` gating, zero code-file awareness — `{overwrite: true}` on
  an EXISTING code file landed unchecked, reopening the exact
  export-dropping regression class `sem.edit()`'s identity check exists to
  catch. See "write() safety contract" above.
- **`find()`: d.ts said `results`, impl returned `hits`** (review pass 3,
  item 3): `sem-api.d.ts`'s `FindResult.results` never matched
  `performSemFind`'s actual `hits` field, on every call, always. Fixed by
  renaming the d.ts to `hits` (not the impl — `hits` is what sem_find's own
  native tool already calls it). While auditing this, found the batch-mode
  d.ts was ALSO wrong in a different way: `find(names[])` was declared as
  returning a bare `FindResult[]`, but it actually returns one
  `FindBatchResult` object (`{total_queries, ran, omitted, results}`) — not
  an array at all. Both fixed.
- **`grep()`: `GrepResult` under-declared, and a false assumption about its
  own batch shape**: `path`/`glob`/`context`/`shown` were real fields
  `performSemGrep` returns that the d.ts never mentioned. Separately
  confirmed (by direct measurement of `api.grep()`, not by reading the code
  alone) that `grep()`'s batch mode is genuinely DIFFERENT from `find()`'s:
  `api.ts`'s `grep()` unwraps to a bare array, discarding
  `total_patterns`/`ran`/`omitted` — this is real, existing, deliberate
  behavior (not a bug fixed here), now accurately documented instead of
  assumed to match find's convention.
- **`diff()`: implementation never mapped the real CLI shape at all**
  (review pass 3, item 3's audit extended beyond what was asked): the prior
  `RawDiffJson`/`diff()` assumed entries were already
  `{name,type,file,change}` and passed them straight through. A live `sem
  diff HEAD~1 --json` returns `{summary, changes, binaryChanges}` with each
  entry shaped `{entityId, changeType, entityType, entityName, filePath,
  oldEntityName, oldFilePath, ...}` — none of the four promised fields
  existed on it. Every prior `sem.diff()` call handed the model `undefined`
  for `name`/`type`/`file`/`change` on every entry, always — the most
  severe of this pass's field-completeness bugs (the others dropped SOME
  fields; this dropped ALL of them). Fixed with a real mapping function;
  `change` stays typed `string` (not the previously-asserted
  `"added"|"modified"|"deleted"|"moved"` union) since `changeType` was
  directly observed to also take `"renamed"`, and `sem`'s own vocabulary
  (the `summary` object's key names) implies `"moved"`/`"reordered"` are
  real categories too, neither of which was directly witnessed.
- **Batch `edit(request[])` telemetry under-counted refusals** (a
  separately-tracked, disclosed gap, closed after review pass 3): a batch
  call's per-entry refusals were visible to the SCRIPT (inline `{error}`
  entries) but always collapsed to one `ok: true` "edit" `CallRecord` in
  sandbox.ts's call log, hiding refusals from `deriveApiCallStats`'s
  `edits.refused`/`reasons`. Fixed via `SUB_CALLS`, a non-enumerable
  symbol property `api.ts`'s `edit()` attaches to the SAME array it always
  returned (not a wrapper around it, so a direct caller bypassing the
  sandbox -- e.g. a test calling `api.edit([...])` directly -- sees
  byte-for-byte the same value as before); sandbox.ts's trampoline reads it
  off the return value and records N `CallRecord`s in place of its usual
  single generic one. Proven end-to-end (not just at the api.ts layer) in
  test/codemode/tool.test.ts's batch-telemetry test: a real sandboxed
  2-success-1-failure batch call now reports `edits: {count:3, refused:1}`,
  not `{count:1, refused:0}`.
- **Logged value host-thread escape (a later review pass, item a)**: the
  SAME class of vector as the return-value `toJSON()` escape above, reached
  via a LOGGED value's `toJSON()`/`toString()` instead. Checked empirically
  BEFORE fixing (not assumed): a host-realm `JSON.stringify` call made
  synchronously from within an active `runInContext` execution (no
  preceding await) was ALREADY covered by vm's native timeout -- the
  vector only manifests once a real host-boundary await has already been
  crossed, the SAME pre-existing, disclosed "busy loop after first await"
  gap this file documents elsewhere, not a new hole. Fixed uniformly
  anyway (`compileVoidTrampoline` now stringifies context-natively before
  crossing to the host), for the same reason `sanitizeReturnValue` exists:
  never call host-realm `toJSON`/`toString` on a sandbox-authored value.
- **Zombie script mutates state after the tool already reported "timed
  out"** (a later review pass, item b — CRITICAL, genuinely new,
  confirmed live before fixing): a script that `await`s something slower
  than `timeoutMs`, then calls `sem.write`/`sem.edit`, lands that mutation
  on disk ~300ms AFTER `runInSandbox` already returned `{ok:false, timed
  out}` — a "cancelled" run that still mutates state. Fixed via
  `createRunCancellation` (see "Sandbox contract" above): a
  SingleWriterCell split where `runInSandbox`'s own timeout handler is the
  sole writer, every `sem.*` trampoline (and `api.ts`'s `write()`/
  `editOne()`/`edit()`, immediately before their real mutation) is the
  reader. Proven end-to-end with a real temp file in
  test/codemode/sandbox-limits.test.ts: the file is confirmed absent both
  immediately after the tool returns AND 800ms later, well after the
  zombie's own slow operation has resolved.
- **Refusal messages were mode-blind** (a later review pass, item c):
  `performWeaveEdit`/`performSemRead`/`Find`/`Grep`/`Outline`/`Callers` all
  prefix their own error text with their OWN native tool name (correct in
  tools-mode) — `api.ts`'s `editOne()` was confirmed live throwing
  `"weave_edit: no entity named..."` verbatim, even though code mode never
  registers a `weave_edit` tool at all. Fixed via `toCodeModeMessage()`, a
  single boundary-crossing rewrite applied to all 8 `throw new
  Error(outcome.text)` sites in api.ts, since api.ts ONLY EVER runs in
  code mode — no actual "mode" parameter was needed, just an unconditional
  translation at the one place these results reach a code-mode caller.
- **`find`/`grep` batch shape inconsistency, decided** (a later review
  pass, item d): `grep(patterns[])` previously unwrapped to a bare
  array, silently discarding `total_patterns`/`ran`/`omitted` — the SAME
  information `find(names[])`'s wrapper object exposes. A `.meta` property
  attached to a returned array was considered and ruled out: an array's
  non-index properties, enumerable or not, never survive `JSON.stringify`
  (verified directly), and every value crossing the sandbox boundary is
  JSON-round-tripped. Decided: upgrade `grep()` to the SAME wrapper-object
  shape as `find()` (`{total_patterns, ran, omitted, results}`), rather
  than downgrade `find()` to grep's old bare-array shape, since that would
  have thrown away real information just for shape parity.
- **A rename via `sem.edit()` left dangling imports, invisibly** (dogfood
  round 2 finding, closed incrementally here): code mode's task-3 rename correctly updated the
  definition and both callers' call sites, but left the ORIGINAL import
  statements in place and added a second, separate import for the new
  name instead of editing the existing one — both vanilla and tools-mode
  did a clean rename on the same task. Root cause traced to
  `performWeaveEdit`'s own response (`weave-edit.ts`'s `formatSuccess`):
  it never passes the entity's NEW name through to `details`/`text` at
  all, only the OLD one — a script had zero visibility into what it had
  just renamed things TO, let alone which other files still mention the
  OLD name. Since `weave-edit.ts` is outside this file's ownership, the
  new name is re-derived locally (`detectRenamedTo`, re-outlines the
  just-edited file and reads off whichever entity now occupies the edited
  range) rather than adding a field there. `findLeftoverReferences` then
  sweeps the repo for the OLD name (word-boundary-safe, so a prefix match
  like `resolveEntity` inside `resolveEntityRef` never false-positives)
  and reports every remaining mention as `leftover_references` on
  `EditResult`, for both the single and batch `edit()` forms. Informational
  only, matching `internal/impact.ts`'s own philosophy for cross-file
  checks: a failed sweep degrades to `[]` rather than failing an edit that
  already landed. This is the foundation `sem.rename` builds on with the
  `performRename` engine (`src/tools/internal/rename.ts`); this fix already
  closes the reported bug on its own.
