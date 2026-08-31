# Routines: reason once, run many

## The argument, from our own numbers

An internal oracle-floor benchmark measured what every eval task costs
when a script goes straight to the answer: 200–2,000 tokens,
1–5 calls. The best model runs pay 20×–1,100× that, and the entire multiple
is re-reasoning — re-discovering, on every run, a composition the API could
already express. Every task in the suite graded *discovery-bound*: the API
is never the bottleneck, deciding what to call is.

A routine converts one solved discovery into a permanent, parameterized
script. The model reasons through a task once, saves the working script with
its inputs lifted to parameters, and every later occurrence of the same task
shape replays deterministic code instead of re-deriving it. Reasoning is the
expensive, fallible step; replay is neither.

## Design

Three verbs, one storage convention:

- **`sem.routine.save(name, {params, description, update?})`** — called at
  the end of a script that just worked. Captures *the current script's own
  source* (the sandbox already holds it), substitutes each declared param's
  literal value with a `params.<key>` reference, and writes
  `.sem/routines/<name>.mjs` with a one-line JSON header (description,
  example params, created-at). Refuses to overwrite without `update: true`.
- **`sem.routine(name, params)`** — loads the file, merges the call's params
  over the saved examples, and runs the body as a script in the same
  sandbox machinery: same token budgets (shared spend), same revocation on
  the outer run's timeout for every mutator, same receipts, same
  coordination. A routine is just a script; it holds **no new authority**.
  Returns the routine's own return value. A missing name refuses with the
  list of available routines (the next-call rule).
- **`sem.routines()`** — names + descriptions + param keys.

Routines are ordinary repo files: committed, diffable, reviewable, and
entity-mergeable by weave like any other source. There is no special store,
no registry service, no cross-repo anything — a routine exists exactly where
the knowledge applies.

Telemetry keeps replay distinguishable from reasoning: each `sem_code`
result's details carry `routines: [{name, apiCalls, edits, merged}]` for
every replay this run performed, alongside the top-level stats for the
script's own direct calls. The `apiCallSequence` contract is untouched — a
replay appears as a plain `"routine"` entry, same vocabulary as every other
call.

## Risks, named, with their mitigations

- **Staleness.** The repo moves; a routine's entities rename or vanish.
  Mitigation is structural: routines address entities by *name through the
  live API*, so a stale routine fails with the normal, honest entity
  refusal — never a silent wrong edit — and the failure text says exactly
  what to do: re-explore, then `sem.routine.save(name, {update: true})`.
  Every replayed write still runs the full edit guards, merge backstop, and
  `check()` gates; replay is cheap, not trusted.
- **Wrong-routine replay.** A name can mislead. Mitigations: descriptions
  are mandatory surface in `sem.routines()`, replay receipts (entity diffs,
  `changed()`) make what actually happened auditable, and a routine cannot
  call other routines (depth 1) — no towers of unreviewed composition.
- **Cross-repo leakage.** None by construction: routines are files in this
  repo's `.sem/routines/`, resolved against `cwd`. Nothing travels unless a
  human commits and another repo copies it — which is code review, the
  system working as intended.
- **Param substitution is literal.** Save-time substitution replaces quoted
  occurrences of each example value with `params.<key>`; a value that
  appears in the script for unrelated reasons substitutes too, and a value
  the script spelled differently is missed (reported as a warning in the
  save result). The saved file is plain source precisely so the model — or
  a human — can read and correct it.

## What was deliberately not built

No auto-save (the model decides what is worth keeping, nudged by one
addendum line), no routine versioning (git is the version history), no
sharing/marketplace layer, no speculative "self-healing" — a failed replay
reports and defers to fresh reasoning, it does not mutate its own source.
The smallest thing that converts a solved task into a file is the feature.
