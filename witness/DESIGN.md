# Witness-backed CONFIRMED data flow

Status: experimental. The runner needs docker and the `pi` coding-agent CLI for the proposer.

## Problem
`sem dataflow` is a may-analysis: flow-insensitive inside a function, field-insensitive, one summary per function.
It over-approximates on purpose; measured precision of its source -> sink paths has been 35-65%, and 27% on the
sample checked for this design. No static refinement gets that to 100%: exact data-dependence is undecidable (Rice 1953), and
it stays undecidable even for interprocedural, context-sensitive data dependence without the program's full
semantics (Reps, "Undecidability of context-sensitive data-dependence analysis", TOPLAS 2000). The goal is
different: some findings must be certain. A finding becomes certain when an execution demonstrates it.

## Tiers
- **CONFIRMED**: a stored, re-runnable witness carried a fresh secret canary from the declared source site to an
  argument of the declared sink call, through the claimed path functions, in 3 of 3 sandboxed runs.
- **POSSIBLE**: the static flow, no passing witness (the path may be real but undemonstrated, or false).
- **UNKNOWN**: sem could not describe the flow precisely enough to instrument it, or it never ran.

CONFIRMED precision is 100% *by construction, given a sound observation*: the tier is never assigned without a
passing witness, and the runner, not the proposer, decides what passed. The proposer can lower recall, never
precision, as long as the observation cannot be faked. Most of the design is about that condition.

## Pipeline
1. `sem dataflow --witness` (Rust, `crates/sem-core/src/dataflow/witness.rs`) turns each static flow into a task:
   the source contract (a parameter and the body that binds it, or the source expression's byte span), the sink
   call's argument spans, and every path function's body insertion point and parameter names.
2. `instrument.py` splices hooks at those spans in a copy of the files: `_witness_inj(p)` / `_witness_src(expr)` at
   the source, `_witness_sink(arg, i)` around every sink argument, `_witness_probe` at TS path functions.
3. `propose.py`: an LLM (pi, `--no-tools`; it only returns text) writes a harness: imports, external contracts
   (env vars, files, stubs of absent packages, fakes), and ONE `W.entry(fn, ...)` call.
4. `anticheat.py` checks the harness statically; `sandbox.py` runs it (`--network none`, no capabilities, no
   credentials, unprivileged, read-only repo copied to tmpfs, limits, timeout) with the runtime
   (`rt/witness_rt.{py,mjs}`), and judges the run.
5. `orchestrate.py`: up to k = 3 proposals per task; failure reasons (never the canary) go back to the proposer; a
   pass is re-run until 3/3; the witness (harness, instrumented files, three results) is stored.
6. `report.py` lists the tiers, re-checking every stored witness.

## Why the observation is hard to fake
The central choice: **the harness never sees the canary.** The runtime generates it inside the sandbox
(`wq` + 16 random hex, fresh per run) and injects it itself, at the declared source. A harness that writes "the
canary" at the sink has nothing to write. What is left to a cheat is to learn the canary or to carry the injected
value through code it controls; each route is closed twice, statically and at run time:

| route | static (before running) | runtime (verdict) |
|---|---|---|
| call the sink function directly | hop/sink function names banned | no injection inside the entry call |
| relay the value through a stub/fake | names banned | harness frame between entry and sink; harness code emitting the canary (Python profiler) |
| learn the canary (gc, frames, /proc) | introspection modules, dunders, frame attrs, `/proc` banned | without injection at the source a sink hit does not count; with it, the relay is a harness frame |
| unittest.mock `side_effect` relay | `unittest`/`mock` banned | mock frames between entry and sink |
| patch repo code (skip a guard) | assignment into repo modules banned | repo bindings holding harness code or shadowing a builtin |
| drive another function than the source | (cannot be seen statically) | no injection at the declared source |
| call the runtime hook | `_witness*`, `globalThis` banned (TS) | an unknown canary is never "seen" |
| replace a repo module through `W.stub_module` | - | refused for repo modules in any layout (found by audit) |
| fake a pure function (`path.join`) so a guard passes | - | replaced `path`/`url`/`util`/`posixpath`/`json` functions refused (found by audit) |
| read process memory by a relative `/proc` path | `/proc` strings banned | audit hook (Python); the canary only exists once the entry call starts |

TS harnesses may define no functions besides `main` (no profiler leak check exists for JS), and use runtime-made
fakes (`W.fake`, `W.returns`) that record nothing. A cheat suite, written before any model call, ran every
row with the static check on and, separately, bypassed; its first run found one real gap (repo-module patching
passed the runtime alone), fixed before any model call; the audits later found two more (rows above), fixed and
added as cheats. The cheat suite itself is not part of this repository yet.

The rest of the verdict makes the demonstrated flow the *claimed* flow: the sink must be the declared call site (the
observer is spliced there), the hit must happen inside the single entry call with the entry frame on the stack, and
every hop function of the static path must have held the canary (Python: arguments, locals or return value at call
or return; TS: arguments at entry when the path enters by a call). A flow that is real but goes another way stays
POSSIBLE.

External boundaries are contracts, and only real boundaries qualify (filesystem, network, environment, clock,
absent packages); faking pure computation changes the program and is refused. Both protocol gaps the audits found
were widenings of "contract": a repo module replaced as if external, and `path.join` faked. Concretely: an absent package is a permissive stub (which records nothing, so it cannot
return the canary), the filesystem and environment are set by the harness (`os.path.exists = lambda p: True` is a
stated assumption, visible in the witness), the network is off. CONFIRMED therefore means "the repo's own code
carries the value from source to sink under the stated contracts", not "this is exploitable in production".

## Prior art and what is new
- **IRIS** (Li, Dutta, Naik; ICLR 2025): an LLM infers taint specifications (sources, sinks, sanitizers) for CodeQL
  and triages the alerts by reading code. Precision still rests on a static engine plus LLM judgment.
- **LLMDFA** (Wang et al.; NeurIPS 2024): LLM-driven dataflow analysis that decomposes the problem and has the LLM
  synthesise scripts (and use SMT solvers) to check path feasibility; the "witness" is a model's or solver's
  verdict over extracted conditions, not an execution of the program.
- **DART** (Godefroid, Klarlund, Sen; PLDI 2005) and **KLEE** (Cadar, Dunbar, Engler; OSDI 2008): concolic / symbolic
  execution generates concrete inputs that drive real executions; every bug they report comes with a test. The
  witness tier borrows exactly that: a finding is a reproducible run. KLEE's environment models are the ancestor of
  the stub/contract layer here.
- **CEGAR** (Clarke, Grumberg, Jha, Lu, Veith; CAV 2000): an abstract counterexample is checked against the concrete
  program and the abstraction is refined when it is spurious. Here the "abstract counterexample" is sem's static
  path and the concrete check is an execution; a failed witness leaves the path POSSIBLE (refinement of sem from
  failed witnesses is future work).
- **Verification witnesses** (SV-COMP; Beyer, Dangl, Dietsch, Heizmann, Stahlbauer, "Witness validation and stepwise
  testification across software verifiers", FSE 2015; correctness witnesses, FSE 2016; execution-based validation,
  "Tests from witnesses", TAP 2018): a verifier's claim must ship with an exchangeable witness that an independent
  validator re-checks, and violation witnesses can be turned into executable tests. The CONFIRMED tier is a violation
  witness for a data-flow property, validated only by execution.
- **Assume-guarantee / rely-guarantee contracts** (Jones 1983; Misra and Chandy 1981; Pnueli 1985): a component is
  verified under explicit assumptions about its environment. The harness's stubs and environment settings are those
  assumptions, written down and stored with the witness.

What is new is the combination and where the guarantee sits: the LLM only *proposes* (entry point, inputs,
contracts), an execution *disposes*, and the precision guarantee comes from the observation protocol (secret
canary injected by the runtime at the declared source, observer at the declared sink, path probes, stack and leak
rules, 3/3 determinism), not from any model's judgment. The model can make the tier smaller, never wrong, unless
the protocol itself is broken, which is what the cheat suite and the audits test.

## Known limits
- Recall is bounded by sem's static recall (a witness only validates a static task) and by what a harness can
  drive without network, credentials or real services.
- Flows through module-level code (an env read at import time), through state written by one call and read by
  another, through callbacks of external frameworks, or guarded by checks the canary cannot satisfy (a file named
  after the tainted value must exist) are hard or impossible to witness under these rules.
- The TS static rules (no functions, no `globalThis`, no dynamic import, no `process.chdir`) are strict; they cost
  recall.
- Canary matching is substring matching on strings/bytes/containers/object fields; a value that is hashed or
  encoded beyond recognition before the sink is not seen (a false negative, never a false positive).
- Go and Rust have no runner yet.
