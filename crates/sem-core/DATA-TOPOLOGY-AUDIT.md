# DATA-TOPOLOGY-AUDIT: a callgraph-driven review for over-wide grants

**Method.** Build sem's own callgraph once (`sem graph --json` over this
repository's `crates/`), use `sem callers` / `sem refs` / `sem entities` to
find which functions have real fan-in (many callers paying for the same
signature) and which are dead ends, then read the actual body of every
candidate before claiming anything. The question asked of every function:
does its parameter list (or, for methods, its receiver) hand it more than it
touches, when the extra material could just as easily be looked up, derived,
or narrowed at the point of use? This is a companion to
`DATA-FLOW-AUDIT.md` (which hunts allocation/clone waste on the cold-build
path) — this document hunts a different shape: functions and methods that
receive a wide struct, cache handle, or options bag and use only a slice of
it. HEAD: `602dc6e`. Binary: `cargo build --release -p sem-cli`, run from
`crates/target/release/sem` against this repository (5,922 entities, 8,690
edges). Read-only on production code; the only file this bead writes is this
one.

`DATA-FLOW-AUDIT.md`'s own D7 finding is the exemplar this document was
asked to hunt for more of: `EntityGraph` used to eagerly build two
`entity_id -> Vec<entity_id>` adjacency maps on every graph build, even
though the cold-build path never read them. That finding shipped
(`EntityGraph { entities, edges, adjacency: OnceLock<EdgeAdjacency> }`,
`graph.rs:644` / `:1990-2022`, verified live at this HEAD) — the maps are now
derived from `edges` the first time a caller actually asks for them. Most of
what follows is the search for that same shape elsewhere: eager work, wide
handles, or pass-through parameters that a narrower interface would make
visible as unnecessary.

**Headline: this codebase has already been through several rounds of exactly
this kind of review.** `GraphSession` (20 fields), the `ImpactOptions` /
`ContextOptions` command layer, and the `get_or_build_graph*` wrapper family
all carry doc comments citing prior beads (`semx-h1s`, `semx-4an`, `semx-dev`,
`semx-zvq`) that already did this narrowing. Reading them turned up KEEP
after KEEP, which is itself useful data and is recorded in §2 rather than
discarded. The two genuine findings below are smaller and more localized than
D1-D11 were, which is the expected shape for a second sweep over
already-audited ground: the callgraph pointed at two functions, in two
different crates, that hold something they never touch.

---

## 1. Findings

### L1 — `render_context` takes the whole CLI options struct to read two fields of it
**SAFE-MECHANICAL** — narrow the parameter, no behavior change
`crates/sem-cli/src/commands/context.rs:122-225`

```rust
fn render_context(
    entity_name: &str,
    entity_type: &str,
    entity_id: &str,
    opts: &ContextOptions,
    context_result: &sem_core::parser::context::ContextResult,
) {
    if opts.json { ... }              // used
    ...
    opts.budget                        // used, twice
    ...
}
```

`ContextOptions` (`context.rs:11-23`) carries ten fields: `cwd`,
`entity_name`, `entity_id`, `file_path`, `budget`, `hops`, `json`,
`file_exts`, `no_cache`, `no_default_excludes`. `render_context`'s full body
(103 lines, read in full) touches exactly two of them: `opts.json` (one
branch) and `opts.budget` (two print sites). The other eight — including the
repo path, both cache-bypass flags, and the extension filter — are never
read. `sem callers render_context` confirms both real call sites:
`context_command` (`context.rs:107`) and `try_index_context`
(`context.rs:352`), each already holding a full `&ContextOptions` for its own
reasons and each passing the whole thing on.

**Why it happened.** The function's own doc comment explains the actual
constraint correctly: `render_context` is shared *verbatim* between the
index-backed fast path and the always-correct full walk specifically so the
two routes are byte-identical by construction, not by two hand-kept-in-sync
copies. That reasoning is sound and should stay — but it argues for one
render function, not for handing that one function the caller's entire
options bag. `entity_name` / `entity_type` / `entity_id` are already passed
as three explicit strings alongside `opts`, so the pattern of "pass exactly
what's used" is half-applied here already; `opts` is the one parameter that
regressed to "pass everything."

**The contrast that makes this an easy call.** The sibling command file,
`impact.rs`, calls its own renderer the way this one should:
`print_cached_result(&result, opts.mode, opts.json, opts.depth)`
(`impact.rs:516`) destructures the three fields the renderer needs at the
call site rather than forwarding `opts`. Same shape of problem (a CLI options
struct feeding a pure output function), already solved correctly one file
over.

**Fix shape.** Change the signature to
`fn render_context(entity_name: &str, entity_type: &str, entity_id: &str, json: bool, budget: usize, context_result: &ContextResult)`
and update both call sites to pass `opts.json, opts.budget`. Behavior-identical
by inspection — the deleted parameter was never read. No test should need to
change; this is exactly the shape `print_cached_result` already proves is
idiomatic in this codebase.

**Yield.** Not a performance finding — `opts` is a cheap reference, so there is
no measurable cost. The value is legibility and future-proofing: today a
reader (or an editor adding a ninth `ContextOptions` field later) has to
manually confirm `render_context` doesn't secretly depend on `cwd` or
`no_cache` before touching those fields. A two-field signature makes that
question unaskable.

---

### L2 — four MCP review-branch tools receive the entire server capability set and touch none of it
**NEEDS-DESIGN** (framework-shaped constraint, not a mechanical fix)
`crates/sem-mcp/src/server.rs:2105` (`join_review`), `:2155`
(`wait_for_branch`), `:2210` (`reply_to_branch`), `:2258`
(`list_open_branches`)

`SemServer` (`server.rs:170-189`) is the single struct behind every MCP tool
this crate exposes. It carries nine fields: the active repo context, the
parser registry, an entity-body cache, a graph cache, a topology cache, a
per-manifest build-lock table, the file watcher slot, the attention ledger,
and the tool router itself — repo indexing state built up over the life of a
connection.

Checking every `&self` method in the file for which fields it actually reads
(grep for `self.<field>` inside each method body, then read the four
zero-hit bodies in full to confirm) turns up four methods that read *none*
of it, directly or by calling another `&self` helper: `join_review`,
`wait_for_branch`, `reply_to_branch`, `list_open_branches`. Each one's entire
body is: resolve a review-service HTTP client from `resolve_agent_review_client()`
(a free function, `server.rs:3411`, that reads its own env/config — not
`self`), make one blocking call through it, and format the result. None of
them opens the repo, touches a cache, or reads the registry.

`sem callers` makes the shape of the risk concrete rather than abstract:

```
$ sem callers join_review
  (callers: none)
$ sem callers wait_for_branch
  (callers: none)
$ sem callers reply_to_branch
  (callers: none)
$ sem callers list_open_branches
  function list_open_branches crates/sem-mcp/src/agent_review.rs:414   <- a different, same-named function
  (callers: none)                                                      <- the actual server.rs:2258 tool method
```

Every one of the four shows zero callers, because they are not called from
anywhere sem's static analysis can see — they are invoked by the `rmcp`
tool-routing framework's runtime dispatch (`#[tool_router]` /
`#[tool_handler]`, wired at `server.rs:1275` / `:2323`), which looks up a
handler by the tool's string name at request time. That dispatch path is
externally reachable: any MCP client connected to this server can invoke
these four tools directly. So the four methods that hold the *most*
capability for the *least* reason are also the ones a static caller-count
would make look like dead code.

**Why it's shaped this way, and why the fix isn't free.** The `#[tool]` /
`#[tool_router]` macros require every registered tool to be a method on the
one struct implementing `ServerHandler` — that's what makes `&self`
mandatory here regardless of whether the body needs it. Rust doesn't have a
smaller receiver type these four methods could ask for and still satisfy the
macro; splitting them onto a second struct with its own narrower state (just
an `AgentReviewClient`) would need either a second `ToolRouter` merged in
under the same `Self` type — which doesn't change the receiver, only where
the code that never calls it lives — or a deeper split of `SemServer` into
multiple handler types that MCP's single-server-per-connection model doesn't
obviously support. That's a real design question, not a signature edit.

**What *is* mechanical, and worth doing regardless of the framework
question:** the four bodies already read nothing from `self`. Extracting each
one into a free function (e.g. `agent_review::handle_join_review(params) -> Result<...>`)
that the `#[tool]` method forwards to in one line would make the "this code
needs nothing from the server" fact visible in the type signature of the
code that actually runs, rather than leaving it as a fact only provable by
reading the body. The `#[tool]` wrapper still has to exist and still has to
be `&self` — but everything downstream of it stops being able to reach into
the repo cache, the build locks, or the watcher, because it's no longer
holding a reference to any of them. That is the real narrowing available
here; shrinking the wrapper's own signature is not.

**Yield.** No performance number — this is a blast-radius argument, not a
cost one. Today a bug or future edit inside these four handlers has the
server's entire cache/lock/watch state sitting in scope even though nothing
in the current code reaches for it; extracting the bodies removes that
temptation for the next person who touches them, at the cost of one new
small module and four one-line forwarders.

---

## 2. Investigated and cleared (KEEP)

Six candidates that looked like the same shape and are not, recorded so this
reads as a real audit rather than a two-item highlight reel:

1. **`sem-mcp/src/cache.rs` re-exporting `sem_core::persist::disk_cache`
   wholesale.** Looks like a crate pulling in a wide surface it barely uses.
   It isn't: the file's own doc comment explains this crate used to hand-roll
   its own `DiskCache`, a duplicate-authority problem an earlier bead
   (`semx-r94`) already closed by deleting the copy and re-exporting the one
   in `sem-core`. Nothing crate-specific is left to narrow. **KEEP.**

2. **`GraphSession`'s 20 fields.** The obvious first guess for "layered state
   with a field nobody reads." Every non-obvious field carries its own doc
   comment citing the bead that put it there and why it can't be derived
   cheaper — `child_ranges` and `corpus_fp` both explicitly say "derivable,
   but re-deriving costs more than what deriving-on-demand would save,"
   which is the correct trade-off stated instead of assumed. This looks like
   ground `semx-h1s` and `semx-4an` already worked over field-by-field.
   Re-litigating it would need per-field usage counts across a warm-rebuild
   run, which is out of scope for a static callgraph pass. **KEEP, pending a
   dynamic-usage pass if anyone wants to re-open it** — flagged, not cleared
   with full confidence.

3. **`try_index_impact_deps` / `_dependents` / `_transitive` taking
   `&ImpactOptions`.** `ImpactOptions` has ten fields; a function taking the
   whole struct is the exact shape this audit hunts. Read in full:
   `try_index_impact_deps` touches all ten (`no_cache`, `mode`, `cwd`,
   `file_exts`, `no_default_excludes`, `file_hint`, `entity_id`,
   `entity_name`, `json`, `depth`). Nothing spare. **KEEP.**

4. **`EntityGraph::dependents()`/`dependencies()` forcing a full-corpus
   adjacency build for what looks like one node's neighbors** (used by
   `collect_reachable_related` in `context.rs:434-478`, which does a bounded
   BFS from a single entity). Looked like a live instance of D7 — a
   corpus-wide structure built to answer a local question. It isn't: a single
   `sem context` call reads this map at minimum four times in sequence
   (`get_dependencies`, `get_dependents`, then the transitive walk in both
   directions), and `get_dependencies`/`get_dependents` themselves route
   through the *same* `OnceLock<EdgeAdjacency>` D7 introduced
   (`graph.rs:4385-4398`) rather than scanning `edges` per call. Building the
   index once and amortizing it across every lookup in the same command is
   the textbook-correct trade, not an instance of the D7 mistake. **KEEP.**

5. **The `get_or_build_graph*` naming family in `sem-cli/src/commands/graph.rs`**
   (`get_or_build_graph` → `_with_timings` → `_with_cache_policy`, and the
   topology/test-data variants alongside them). Six functions with
   overlapping names looked, from the `grep` list alone, like constructor
   proliferation. Reading them shows each is a thin wrapper adding exactly
   one new parameter over the one it calls (timings, then a save-policy enum)
   — the same "progressive specialization by composition" pattern
   `find_supported_files` → `find_supported_files_with_options` already uses
   correctly elsewhere in the same file. Every wrapper's own doc comment or
   the enum it introduces explains the one thing it adds. **KEEP** (naming
   could be shorter, but that's a style opinion, not an authority finding).

6. **`build_image`'s `graph: &EntityGraph` parameter**
   (`sem-core/src/index/writer.rs:274`). `EntityGraph` post-D7 is three
   fields: `entities`, `edges`, and a lazy `OnceLock` adjacency index.
   `build_image` reads `graph.entities` and (further down, for the REFS
   section) `graph.edges` directly; it never calls `.dependents()`/
   `.dependencies()`, so the lazy field is never forced. Taking `&EntityGraph`
   here costs nothing beyond what's used. **KEEP.**

---

## 3. What sem's own query surface could and couldn't answer for this task

This audit hunts a different shape than the DATA-FLOW-AUDIT's, and that
changed which of sem's verbs earned their keep.

**Where the callgraph did real work.** `sem callers` was the tool that turned
"this function looks unused-by-self" from a guess into a checkable claim
twice: it confirmed `render_context` genuinely has exactly the two call
sites its doc comment claims (§1, L1), and it independently surfaced the
finding in L2 by returning `(callers: none)` for all four review-branch
tools — which is what sent this audit looking for *why* a live, tool-routed
MCP method has zero static callers, rather than assuming it was dead code.
`sem entities <path>` was the coverage check before trusting the graph
(confirmed 305/137/46/318/125 entities across graph.rs, server.rs, cache.rs,
index/, session.rs before relying on `sem graph --json`'s 5,922/8,690 totals).

**Where it structurally can't help, and that's a real gap for this class of
audit.** Everything this document actually hunts — "does this function read
every field of the struct it was handed" — lives below the granularity sem's
index models. sem's callgraph answers *who calls what*; it has no concept of
*which fields of a parameter a function body actually touches*. Every finding
above (and every KEEP) was decided by reading the function body directly,
with `sem callers`/`sem entities` used only to establish fan-in and
call-site truth beforehand. That's not a bug in sem — entity/edge
granularity is the right level for the navigation questions it's built to
answer — but it means a least-privilege sweep is a two-tool job by
construction: sem to find and rank the candidates worth reading, `Read`/grep
to actually adjudicate them. No amount of graph-side tooling replaces the
second step for this particular question.

**The one place the gap actively misleads, not just under-serves.** L2's
`(callers: none)` result is *correct* — sem is not wrong that no other
function in this codebase calls `join_review`. But read without the context
of how `#[tool_router]` dispatches at runtime, "zero callers" reads as "dead
code, safe to ignore or delete," when these four are in fact the most
externally exposed, least-audited entry points in the crate. This is a
sharper version of the "macro-generated functions are not entities" gap
`DATA-FLOW-AUDIT.md` §5 already flagged for `add_ns_fn!`-generated timing
accumulators — that gap was honest-failure (the function doesn't exist to
sem). This one is worse: the function *does* exist, has a correct signature,
and still reports a caller count that undersells its real reachability,
because the framework's dispatch table is invisible to static analysis.
Worth adding to the query-shapes backlog (`semx-4rz`) alongside the existing
asks: some way to mark, or have sem recognize, framework-attributed entry
points (`#[tool]`, `#[test]`, `main`, etc.) as implicit reachability roots,
so `(callers: none)` on one of them reads as "externally invoked," not "dead."

**Secondary, smaller friction.** No depth-bounded traversal (the same gap
`DATA-FLOW-AUDIT.md` §5 gap 2 already named) meant tracing *how* the four
review tools connect to the tool router at all — i.e. confirming they really
are registered, not orphaned — took a manual follow-up `grep` for
`#[tool_router]`/`#[tool_handler]` rather than a single graph query. That is
the same gap, a second confirmation of it, not a new one.

---

## 4. Ranked summary

| # | finding | file:line | risk class | value |
|---|---|---|---|---|
| L1 | `render_context` takes `&ContextOptions` (10 fields) to read 2 | `sem-cli/src/commands/context.rs:122` | **safe-mechanical** | small but real; makes the function's true dependency set checkable at a glance instead of by reading the body |
| L2 | 4 MCP review-branch tools hold the full 9-field `SemServer` and read none of it | `sem-mcp/src/server.rs:2105,2155,2210,2258` | **needs-design** (framework-forced `&self`); bodies are mechanically extractable to free functions today | blast-radius reduction on the crate's most externally-reachable, least-audited entry points |

**Total prospective simplification: small.** This is the honest headline.
Unlike the DATA-FLOW-AUDIT's D1-D11 (multiple GB of RSS and hundreds of ms
across the cold-build path), this sweep's yield is legibility and blast
radius, not wall-clock or memory — because the callgraph pointed the search
at code that has mostly already been through this kind of review. Two
findings, one mechanical and small, one real but bounded by a framework
constraint neither is worth inflating. The KEEP list in §2 is the actual
majority finding: the codebase's high-fan-in, wide-looking surfaces
(`GraphSession`, `ImpactOptions`, the adjacency accessors, the
`get_or_build_graph*` family) turn out to be justified on inspection, which
is what "we already did this work" looks like when checked rather than
assumed.

---

## 5. Second pass (exhaustive hunters), 2026-08-22

Four dedicated hunters ran an exhaustive follow-up sweep over this same
question — every parameter of every function, not just the callgraph-ranked
candidates §1-§4 covered — split by shape: **H1** (wide-struct/options-bag
parameters, this crate + `sem-cli`), **H2** (`SemServer`'s and other
services' field-by-field authority audit, plus doc-truthfulness), **H3**
(cross-module threading — a value carried through two or more hops before
its first real read), **H4** (the same three shapes, scoped to `sem-cloud`,
executed and reported separately in that repo — out of scope for this
document). Coverage arithmetic as reported by each hunter: **H1** 189
candidate parameters read in full, 11 real findings, 177 KEEPs (already
minimal on inspection). **H2** 118 fields audited field-by-field across the
services examined, 0 real over-grants found, 2 doc-truthfulness gaps (both
executed below as H2-1/H2-2). **H3** 2 real threading findings plus 7 KEEPs
(values that looked like they crossed hops untouched but had a real
intermediate reader). **H4**: executed separately in `sem-cloud`, not part
of this ledger.

This wave (T1) executed the eleven H1/H2/H3 findings plus the two
first-pass carry-overs (L1/L2, already ranked above) against this worktree
at HEAD `18f03ff`, re-verifying every read against current source before
editing rather than trusting the hunters' line numbers verbatim — two
findings did not survive that re-verification:

- **H1-6** (`graph.rs:5293` `scan_import_file`'s `clojure_ns_index`
  claimed-dead) **did not hold**: the parameter is read directly at
  `graph.rs:5644`, forwarded to `resolve_clojure_as`. Skipped; no edit
  made.
- **H1-7**'s `register_go_package_imports` dead-param claim (`_symbol_table`,
  `_entity_map`) was re-verified against current HEAD specifically because
  wave A had just edited that function's Go-collision fix — both parameters
  were confirmed still genuinely unread post-wave-A and executed as
  specified.

Executed, each re-verified by reading the full function body and every call
site before editing:

- **L1** — `render_context` narrowed from `&ContextOptions` (10 fields) to
  `(json: bool, budget: usize)`; both call sites (`context.rs:107`, `:352`)
  now destructure at the call site, the same shape `impact.rs`'s
  `print_cached_result` already used.
- **L2** — `join_review`/`wait_for_branch`/`reply_to_branch`/
  `list_open_branches` bodies (confirmed zero `self.<field>` reads, directly
  or via helper) extracted verbatim into four free functions
  (`*_impl`, `server.rs`, right after the `#[tool_router] impl` block); the
  `#[tool]` methods are now one-line forwarders. Kept in `server.rs` itself
  rather than moved into `agent_review.rs` — that module's own doc comment
  states the client/tool-layer split explicitly ("the MCP tools ... live in
  `server.rs`"), and a free function's inability to reference `self` is
  already the structural guarantee the finding asked for; moving files
  wasn't necessary to get it.
- **H1-1..H1-5 + H3-1** (executed as one coherent restructure, per the
  brief's instruction not to run two overlapping passes on `diff/mod.rs`'s
  cloud-upload chain): `DiffCloudContext::resolve` narrowed to
  `(cwd: &str, from_stdin: bool)`; `upload_diff_snapshot_or_warn` narrowed
  to `(client, remote, label, ...)`; `build_git_context` narrowed to
  `(git, staged, commit, from, to, parsed_scope: &Option<ParsedScope>)` —
  folding in H3-1's `parsed: &ParsedArgs` → `scope` narrowing on the same
  chain; `build_changed_entity_relations` (relations.rs) narrowed to
  `(cwd: &str, file_exts: &[String], result, budget_override_ms)`. The
  cascade (H1-5): a new module-local `DiffCloudFields<'a>` (7 borrowed
  fields — `cwd`, `staged`, `commit`, `from`, `to`, `label`, `file_exts`,
  confirmed by grep as the complete set this module's chain ever reads out
  of `DiffOptions`'s 14) replaces `&DiffOptions`/`&ParsedArgs` in
  `maybe_upload_cloud_diff_snapshot`, `run_inline_relations_flow`,
  `run_upload_first_flow`, and `execute_relations_plan`; `diff/mod.rs`'s one
  call site now builds it via `DiffCloudFields::from_opts(opts)` and passes
  `&parsed.scope` directly. Chose a struct over further scalar explosion
  because these four functions already carry
  `#[allow(clippy::too_many_arguments)]` from six-plus positional params;
  adding the 7 destructured DiffOptions fields on top of the existing
  ctx/head_sha/git_context/file_changes/result/binary_changes list would
  have made every signature worse to read, not better.
- **H3-2** — `write_index_only`'s two O(1) gates (`SEM_NO_INDEX` env check,
  `cache_dir_for_repo`) hoisted from inside `write_query_index` to the top
  of `write_index_only` itself, mirroring `impact.rs`'s
  `try_index_impact_deps` (cheapest-decline-first) pattern. Measured
  (`SEM_TIMINGS=1 SEM_NO_INDEX=1`, fresh `SEM_CACHE_DIR` per run, two
  interleaved before/after pairs on home-assistant/core): `index_only_save`
  345.103ms → 0.003ms and 273.080ms → 0.003ms (before-binary vs
  after-binary, alternated). Testing-path-only as expected — `SEM_NO_INDEX`
  is a debug/isolation escape hatch, not the default path — and
  `full_graph_build` (the dominant cost, ~2.5-2.7s) is unaffected either
  way, as it must be: nothing about this fix touches the graph build
  itself.
- **H2-1** — `PrecomputedFileFacts::import_stmts`'s W1-era doc comment
  ("always empty today") was stale since W5: Python is admitted
  unconditionally (`mul_precompute_admits`) and has real imports, so the
  field is populated on every active Python file; C#/Rust/Go/Java populate
  it only when their own gate env var is set (off by default). Rewrote both
  this doc comment and a second, matching-stale inline comment at the
  dispatch call site (`scope_resolve.rs`, pass 2) that made the identical
  "Rust today" claim.
- **H2-2** — `sem-mcp`'s `CachedTopology` had no doc comment explaining why
  it isn't redundant with `CachedGraph`; added one after confirming (via
  `live_topology`/`get_or_build_graph_topology`) it's a genuinely separate,
  lighter disk-load tier (`DiskCache::load_graph_topology_with_source_scope`)
  that never deserializes entity bodies.

**Notable KEEPs** (from the hunters' reports, not re-litigated here — flagged
for anyone picking this up next): `ScopeResolveConfig` was flagged as a
candidate for a future pass rather than executed this wave (wide, but every
field's usage is plausibly load-bearing per-language config, not proven
either way at hunt time). `try_index_graph`, `try_cloud_entities`, and
`text_search` were each measured just above the "worth narrowing" threshold
— real fan-in, most fields read, not egregious enough to justify the
call-site churn this wave, but close enough to be worth a second look if a
future pass has budget.

**Gates**: sem-core 658/658, sem-cli 250/250, sem-mcp 93/93 — all green,
unchanged counts (no test added or removed; every finding here was a
signature/comment change, not a behavior change). `cargo build --release
--workspace` clean. `edge_dump_probe` sha256 on home-assistant/core
(310,398 edges): bit-identical before/after
(`7744ae4a...662fcb6`, matching on all digits). `cargo clippy --workspace
--release` (default lint level, no `-D warnings` — the baseline already
carries 170 pre-existing warnings across the workspace unrelated to this
wave) and `cargo fmt --check`: zero warnings/diffs inside any line range
this wave touched, confirmed by cross-referencing every flagged line against
this wave's edited functions. One real formatting regression this wave
caused (a `build_git_context` condition shortened enough by the parameter
narrowing to refit rustfmt's line-width rule) was found and fixed before
commit; pre-existing drift in untouched code (cloud_upload.rs's import
order, an unrelated `eprintln!` collapse, and dozens of files elsewhere in
the workspace) was left alone, consistent with the brief's "minimal,
behavior-identical" instruction for `cloud_upload.rs`/`relations.rs`
specifically.

**Net LOC**: +87 (371 insertions, 284 deletions across 7 files) — positive,
not negative, and expected to be: L2's forwarder pattern is inherently
additive (the audit's own L2 entry called this out — "one new small module
and four one-line forwarders" trades line count for capability narrowing,
not the other way around), `DiffCloudFields`'s new struct+impl adds ~25
lines in exchange for deleting scattered `&DiffOptions` threading, and
H2-1/H2-2 turn stale-or-missing doc comments into longer, truthful ones.
The negative-LOC expectation in the brief describes the campaign in
aggregate (which includes H1's 177 KEEPs contributing zero lines and H4's
separate sem-cloud execution); this wave's slice, on its own, was never
going to net negative given L2's shape was already known going in.
