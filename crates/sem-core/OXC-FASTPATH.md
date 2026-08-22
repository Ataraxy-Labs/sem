# An oxc fast path for TS/JS extraction: measured, and not shipped

## The question

`INCREMENTAL-PARSE.md` measured that tree-sitter parse is 48-59% of
`extract_entities` and the entity walk is 41-52%. Both are tree-sitter-shaped
costs. [oxc](https://oxc.rs) is a from-scratch Rust JS/TS toolchain with an
arena-allocated, typed AST instead of tree-sitter's generic concrete syntax
tree (CST). It is widely benchmarked as one of the fastest JS/TS parsers that
exist. So: does routing `.ts`/`.tsx`/`.js`/`.jsx`/`.mts`/`.cts` extraction
through oxc, behind a feature flag, win — on the thing sem-core is actually
asked for, `Vec<SemanticEntity>`, field-identical to the tree-sitter path, the
same contract the content-addressed cache (`src/parser/cache.rs`) shipped
under?

## The measurement

`benches/oxc_spike.rs`, gated by the new `oxc-fastpath` cargo feature
(off by default; adds `oxc_parser`/`oxc_ast`/`oxc_ast_visit`/`oxc_allocator`/
`oxc_span`/`oxc_syntax`, all pinned to `=0.143.0`). It compares four legs on
every TS fixture already in the repo, the three `benches/common` synthetic
TS sizes, and four large real files from a local
`~/.cache/checkouts/github.com/microsoft/TypeScript` clone (skipped, not
failed, when that checkout isn't present):

* **(a) `ts_parse`** — `parser::plugins::code::parse_tree`, the real
  tree-sitter parse path.
* **(b) `ts_parse_and_walk`** — `extract_entities_from_tree` on top of (a):
  today's real `extract_entities_with_tree`, i.e. the number that matters.
* **(c) `oxc_parse`** — `oxc_parser::Parser::parse`.
* **(d) `oxc_parse_and_walk`** — (c) plus a hand-written `oxc_ast_visit::Visit`
  walk (`Collector` in the bench) that counts named functions, classes,
  methods, interfaces, type aliases, enums, and const-assigned
  functions/arrows. This walk is **not** pinned to `entity_extractor.rs`'s
  output — see "What full parity would also require" below for why not, and
  what the entity-count columns already show about the gap.

Apple silicon, `--release`, criterion medians (large-real group runs at
`sample_size(20)`, 0.5s warm-up, 3s measurement — the same "spike, not a
full suite" tradeoff `INCREMENTAL-PARSE.md` used for its own large fixtures).

| fixture | bytes | (a) ts parse | (b) ts parse+walk | (c) oxc parse | (d) oxc parse+walk | (b)/(d) | entities ts / oxc |
|---|---:|---:|---:|---:|---:|---:|---:|
| models.ts | 785 | 40.8 µs | 89.3 µs | 1.81 µs | 1.94 µs | **46x** | 15 / 12 |
| service.ts | 993 | 49.8 µs | 99.9 µs | 2.46 µs | 2.76 µs | **36x** | 4 / 4 |
| database.ts | 504 | 30.2 µs | 66.3 µs | 2.06 µs | 1.84 µs | **36x** | 11 / 10 |
| handlers.ts | 800 | 52.5 µs | 163 µs¹ | 2.21 µs | 2.06 µs | **79x**¹ | 5 / 5 |
| synthetic-small | 1,394 | 80.4 µs | 163 µs | 3.43 µs | 3.59 µs | **45x** | 12 / 7 |
| synthetic-medium | 14,214 | 854 µs | 1.67 ms | 34.2 µs | 36.1 µs | **46x** | 122 / 77 |
| synthetic-large | 113,621 | 11.2 ms | 15.0 ms | 281 µs | 306 µs | **49x** | 969 / 616 |
| checker.ts (real) | 3,151,774 | 102.7 ms | 215.7 ms | 6.32 ms | 7.61 ms | **28x** | 2,608 / 2,519 |
| parser.ts (real) | 539,685 | 21.2 ms | 39.9 ms | 1.06 ms | 1.04 ms | **38x** | 644 / 717 |
| utilities.ts (real) | 512,951 | 17.9 ms | 30.8 ms | 1.14 ms | 1.45 ms | **21x** | 985 / 861 |
| types.ts (real) | 487,846 | 13.2 ms | 28.8 ms | 909 µs | 917 µs | **31x** | 3,793 / 872 |

¹ `handlers.ts`'s `ts_parse_and_walk` sample had 13% outliers (range
137-190 µs) — small-file criterion noise, not a real 79x. Every other row is
clean. Treat the small-fixture speedups as "40-50x", the large-real ones as
"20-40x".

Every row clears the spike's 1.5x bar by more than an order of magnitude —
`oxc`'s typed AST does make the walk faster, not just the parse, which is
the question `INCREMENTAL-PARSE.md` left open ("oxc only wins big if it
speeds BOTH — that is the central question"). Answer: it does. **On raw
throughput alone this is an emphatic go.**

The last column already shows the walk here isn't complete: `types.ts` is
almost entirely `interface { ... }` bodies, and tree-sitter's TypeScript
`entity_node_types` (`languages.rs`) lists `property_signature` and
`method_signature` as their own entity kinds nested under each interface —
3,793 entities. The spike's `Collector` never descends into interface
members, so it counts 872. A full-fidelity walk would visit more oxc node
kinds than this spike did, which would eat into some of that 20-79x margin
— but not close a 20-40x gap. Raw speed was never the blocker.

## Why raw speed is not what decides this

`src/parser/cache.rs`'s doc comment states the contract this flag must also
honor: "the same three inputs always produce the same
`Vec<SemanticEntity>`" — and `tests/parse_cache.rs::assert_same` pins every
field, including `structural_hash`, between the cached and uncached paths.
The task for this flag is the same bar: field-identical output, not
approximately-similar output.

`structural_hash` (`src/utils/hash.rs::hash_structural_tokens`) is defined
as: for every **internal** node in the tree, hash its `node.kind()` string;
for every **leaf**, hash its raw source bytes. It walks tree-sitter's full
concrete syntax tree (CST) — which, unlike an AST, represents *every*
grammar production as a node, including anonymous leaves for keywords and
punctuation (`class`, `{`, `}`, `(`, `)`, `,`, ...). Two TS snippets that are
identical ASTs but different concrete syntax (e.g. different parenthesization
choices tree-sitter's grammar treats as distinct productions, or a
`function_signature` vs a `function_declaration` overload node) hash
differently today, by design — that granularity is what makes
`structural_hash` useful for the "formatting changed but structure didn't"
and rename-detection uses in `src/parser/differ.rs`.

oxc's AST (`oxc_ast::ast`, inspected directly: `Function`, `Class`,
`ClassBody`, `MethodDefinition`, ...) has no representation for keyword or
punctuation tokens as addressable nodes at all — a `Function` struct is
`span, r#type, id, generator, async, type_parameters, params, body, ...`,
full stop. There is no node for `function`, no node for `(`/`)`, no way to
walk "the token stream, one node per production" the way
`hash_structural_tokens` requires, because oxc is an AST (what the code
*means*) and tree-sitter is a CST (exactly how it was *written*). This is
not a missing feature to work around; an AST discards the information a CST
walk depends on, by construction, on every single node — not on a
short-tail of exotic constructs like decorators or ambient declarations.

That is the difference from the escape hatch the task anticipated ("if
specific constructs can't be made identical... gate the fast path off for
files containing them"). Gating works when the divergence is
construct-shaped: turn the flag off for files with JSX, or decorators, or
namespaces, and the remaining files are still identical. `structural_hash`'s
divergence is not construct-shaped — it fires on the first entity in the
first file, always, because every entity's hash walks the same
CST-vs-AST-shaped mismatch. "Gate off files that contain it" degrades to
"gate off every file", which is not a fast path, it's an always-off feature
flag with extra steps.

The one way to close this gap — recompute `structural_hash` via a real
tree-sitter parse even on the oxc-fastpath, so the two paths at least agree
on that one field — defeats the feature: `INCREMENTAL-PARSE.md` already
established tree-sitter parse is 48-59% of extraction cost, so paying for a
full tree-sitter parse *in addition to* the oxc parse is strictly worse than
today's single tree-sitter pass, not faster.

## What full parity would also require (separate from `structural_hash`)

Even setting `structural_hash` aside, `entity_extractor.rs` is ~3,500 lines
of tree-sitter-node-kind-driven, TS/JS-specific rules that the spike's
`Collector` does not reproduce and that a real implementation would have to,
field-for-field:

* Multi-declarator splitting (`const a = 1, b = 2` → two entities, not one) —
  `entity_extractor.rs:485-540`.
* `describe`/`it`/`test` call-expression container detection, which recurses
  into callback bodies as if they were nested scopes — `:640-690`.
* TS overload-signature suppression (`should_skip_ts_overload_signature`) so
  only the implementation, not each overload signature, becomes an entity.
* `promote_js_ts_const_function` and the object-method-pair case
  (`{ foo() {} }` as a method entity) — `:836-870`.
* Re-export entity synthesis for `export { x } from './y'` — `:930+`.
* Same-line overload/duplicate-class disambiguation
  (`f@L1#1` / `f@L1#2`, `build_entity_id_disambiguated_with_ordinal`) —
  pinned by `test_same_line_typescript_overload_ids_are_unique` and
  `test_same_line_duplicate_parent_ids_are_propagated_to_children` in
  `src/parser/plugins/code/mod.rs`.

None of this is impossible the way `structural_hash` is — it's a large,
tractable, multi-day mapping exercise from oxc's `ast::ast` enum variants to
`entity_extractor.rs`'s tree-sitter-node-kind `match` arms. It just isn't
worth doing first, since `structural_hash` already forecloses shipping
either way.

## The verdict: no, not as a field-identical fast path

The raw numbers are a clean go — 20-49x on parse+walk together, matching or
beating the parse-only speedups `INCREMENTAL-PARSE.md` measured for
incremental tree-sitter reparsing. But the deliverable this bead asked for
was a fast path with the *same contract* as the extraction cache:
field-identical output. `structural_hash` cannot be made field-identical
between a CST walk and an AST walk in principle, not as an unimplemented
edge case — so per the task's own rule ("if genuinely unreachable, ...
never ship silent divergence"), and given the gate degrades to "always off"
rather than "off for a minority of files", the honest call is not to wire
this into `CodeParserPlugin::extract_entities` at all.

### Cache-key reasoning (for the record, since this was asked for explicitly)

Not applicable to what's shipped here — nothing routes through oxc, so
`src/parser/cache.rs::key_for` is untouched. Worth recording the reasoning
anyway, since a future attempt will hit it immediately: if a fast path *were*
ever shipped with a deliberately different, internally-consistent
`structural_hash` convention (accepting the divergence rather than declining
over it), the cache key would need the parser identity folded in
(`"code"` vs e.g. `"code-oxc"`), not just `(plugin_id, file_path, content)`
as today. Without that, flipping `oxc-fastpath` on or off between two runs
of the same fleet against the same on-disk cache tier
(`SEM_PARSE_CACHE_DISK=1`) could silently serve a tree-sitter-computed
`structural_hash` to a caller expecting the oxc convention or vice versa —
exactly the silent-divergence failure mode the cache's own doc comment
("`extract` **must** be a pure function of its inputs") exists to prevent.
This is one more argument for declining rather than shipping a
knowingly-divergent hash: doing so would require a cache-key migration and a
`structural_hash` versioning story, on top of the entity-mapping work above.

## What was delivered instead

* `Cargo.toml`: `oxc-fastpath` feature (off by default, no effect on
  `cargo test -p sem-core`), pinned exact-version optional deps
  (`=0.143.0`) so a future equivalence attempt isn't chasing a moving
  upstream AST mid-effort.
* `benches/oxc_spike.rs`: the four-leg comparison above, over the existing
  TS fixtures, `benches/common`'s synthetic sizes, and (when present) large
  real files from a `microsoft/TypeScript` checkout. Run it with
  `cargo bench -p sem-core --bench oxc_spike --features oxc-fastpath`.

No entity-mapping code, no equivalence-pinning test suite: there is no
shipped code path for one to pin. The bench **was** the pinned record, the
same role `benches/incremental.rs` plays for `INCREMENTAL-PARSE.md` —
`benches/oxc_spike.rs` was removed in the 2026-08-21 subtraction pass
(superseded by `diff_oracle` for equivalence and `perf_probe` for the
end-to-end share; see the removal ledger in `RESOLUTION-PROFILE.md`). This
document is the number's sole remaining provenance: the 20-49x and honest
end-to-end figures below are the closed record, not a live, re-runnable
measurement.

## The honest end-to-end number

From `semx-cnq`'s cold-build phase-attribution probe
(`crates/sem-core/examples/perf_probe.rs`) on the monster corpus
(microsoft/TypeScript, 40,872 files, 454,541 entities): `parse+extract` is
2.753s of a 44.794s cold `EntityGraph::build` — **6.1%**. Resolution is
91.9%. That probe's own conclusion, independent of this spike: *"TS-specific/
alternate parsers are not supported as a lever at all by this measurement"* —
this spike arrives at the same place from the equivalence side rather than
the wall-clock-share side.

Even in the counterfactual where equivalence *were* reachable and every
JS/TS file in that corpus got a representative ~30x parse+walk speedup (the
geomean of this spike's clean rows, excluding the noisy `handlers.ts` row):
`parse+extract` would drop from 2.753s to about 0.09s, saving ~2.66s —
**44.8s → ~42.1s cold, about 6% faster end-to-end.** That was always the
ceiling; it is not a large number relative to the 44.8s total, because
extraction was never the dominant cost. Declining costs little upside.

Sibling bead semx-022 (pass-2 re-parse elimination) is orthogonal: it
targets the ~2.72s "literal reparse-tax" inside the *resolution* bucket
(`scope_resolve.rs` re-parsing every file a second time once a build crosses
`PARSED_FILE_REUSE_LIMIT`), not the `parse+extract` bucket this spike
targets. If semx-022 lands, total cold build drops to ~42.1s independently,
and `parse+extract`'s ~2.75s becomes a very slightly larger share (~6.5%) of
a smaller total — the two numbers move separately and neither one's
conclusion depends on the other landing first.

## When to revisit

Two independent things would have to change, not one:

1. `structural_hash`'s contract would have to become deliberately
   parser-scoped (a version tag, a migration plan for already-stored hashes,
   and the cache-key change above) rather than "one canonical CST-shaped
   hash regardless of parser" — a product decision, not an engineering one,
   and out of scope for a flag that's supposed to be invisible to callers.
2. Separately, someone would still need to do the entity-mapping work in
   "What full parity would also require" — multi-declarator splitting,
   overload suppression, re-export synthesis, same-line disambiguation,
   JSX/decorator/ambient-declaration coverage — none of which this spike
   attempted.

If both happen, this document's numbers say there's 20-49x of headroom on
the walk itself to spend on closing that gap before the win disappears. The
bench that measured it is gone (see the note above); reproducing the number
would mean writing a new one from this section's four-leg design.

---

# The revival (bead semx-r63): a pluggable fast-extractor architecture,
# gated on DIFF-level equivalence

Everything above is still true and is left unedited: `structural_hash` is a
CST walk, oxc has no CST, and *field-identical* output is unreachable. What
changed is not that argument — it is the bar, and the stakes.

**The bar.** Field identity was never the product. What a user sees is a
`sem diff`: an entity set, a change classification per entity, and a rendered
result. So the revival defines equivalence *there*, and proves it empirically
per candidate extractor instead of assuming it from a hash:

> Two extractors are equivalent on a change set iff they produce (1) the same
> entity set — id, kind, name, parent, kappa, span, in order — on every side
> of every file, (2) the same `DiffResult`, and (3) the same rendered
> `sem diff --json` envelope.

**The stakes.** The "honest end-to-end number" section above computed the
ceiling as ~6% of a 44.8s cold build, because resolution was 91.9% of it.
Resolution has since been cut by roughly 5x (see `RESOLUTION-PROFILE.md`:
semx-022's pass-2 re-parse fix, semx-6rd's precompute, semx-4an's
delta-proportional warm). `parse+extract` is now ~3.5s of an ~8.7s cold
monster build — the same absolute number, against a total that shrank around
it. Extraction went from a rounding error to the largest remaining cold-build
door precisely because everything downstream of it got fixed.

## Phase 1: the trait and the oracle (landed first, on purpose)

The gate was built and proven *before* any oxc code existed, so that the
answer could not be shaped by the effort already spent on it.

### `src/parser/fast_extractor.rs` — the contract

```rust
pub trait FastExtractor: Send + Sync {
    fn identity(&self) -> &str;
    fn claims(&self, file_path: &str) -> bool;
    fn extract(&self, file_path: &str, content: &str) -> Option<Vec<SemanticEntity>>;
}
```

Three decisions worth naming:

* **`None` is a first-class answer.** A parse error, an unsupported
  construct, a dialect the extractor doesn't model — all are *declines*, not
  errors, and fall through to tree-sitter silently and safely for that one
  file. This is what makes a partial extractor shippable at all.
* **`identity()` is mandatory and is folded into cache keys.** `key_for` in
  `src/parser/cache.rs` now includes the installed extractor's identity
  alongside `plugin_id`, for exactly the reason that section's own
  "Cache-key reasoning" note predicted: two extractors can claim the same
  path and legitimately produce different entities, so a process that flips
  the switch — or two runs sharing `SEM_PARSE_CACHE_DISK=1` — must not read
  each other's entries. With the fast path off or empty, nothing is
  contributed and the key is byte-identical to what it always was.
* **The seam is `extract_entities`, not `extract_entities_with_tree`.** The
  entities-only API is the one a treeless parser can answer in full. Pass 1
  of `EntityGraph::build` calls the tree-bearing variant *because it needs the
  tree* — the `retain_parsed_files` arm hands it to pass 2, and the JS/TS arm
  hands it to `precompute_js_ts_file_facts`. Routing those through a fast path
  would trade a parallel tree-sitter parse for a serial pass-2 re-parse: a
  loss, not a win. Which call sites can be served is an integration question
  answered per site with the oracle as arbiter, not a property of the trait.

### `src/parser/diff_oracle.rs` — the gate

`diff_oracle::run(file_changes, registry, label)` runs the **full** diff
pipeline twice in one process — leg A with the fast path forced off, leg B
with it on — and compares three layers, each named so a failure localizes:

| layer | what it compares | why separately |
|---|---|---|
| `EntitySet` | per file-side: id, kind, name, parent, kappa, span | strongest; localizes a failure to one entity |
| `DiffResult` | counters + every `SemanticChange` field | what the pipeline actually computes |
| `RenderedJson` | the `sem diff --json` envelope | what a user actually sees |

`structural_hash` and `content_hash` are deliberately **outside** the entity
fingerprint — requiring them to match is the unreachable bar. Their
*behaviour* is fully covered: every decision they drive (the phase-2 fallback
match in `model/identity.rs`, the `structuralChange` cosmetic verdict) lands
in the `DiffResult` layer, and the mutation tests below prove it does.

Two failure modes the gate refuses to launder:

* **Vacuity.** An extractor that declines every file trivially produces an
  identical diff. Every run reports `claimed`/`served`, and the verdict is
  `Vacuous` — never `Equivalent` — when nothing was served. The oracle also
  clears the parse cache before each leg, so `served` counts real extractions
  rather than cache misses (this was found by the sweep test failing: a
  faithful mutant read its own cached entries from an earlier test and
  reported `served=0`).
* **A gate that cannot fail.** See the mutation testing below.

### Mutation-testing the oracle

Seven `Mutation` variants wrap the real tree-sitter extraction and perturb
exactly one observable property each. `Faithful` is the false-positive
control. Results, from `cargo test -p sem-core --lib diff_oracle` (9 tests,
all green) and from a 5-commit real-history sweep on `ueberdosis/tiptap`:

| mutation | unit fixture | tiptap 5 commits | layers that caught it |
|---|---|---|---|
| `Faithful` | Equivalent ✓ | 5/5 Equivalent | — (control) |
| decline-everything | Vacuous ✓ | — | — (verdict, not divergence) |
| `DropLastEntity` | caught | 5/5 divergent | EntitySet + DiffResult + RenderedJson |
| `ShiftSpan` | caught | 5/5 divergent | EntitySet + RenderedJson |
| `RenameEntities` | caught | 5/5 divergent | EntitySet + RenderedJson |
| `DropStructuralHash` | caught | 4/5 divergent | DiffResult + RenderedJson **only** |
| `DropKappa` | caught | 5/5 divergent | EntitySet **only** |
| `MergeDeclarators` | caught | 0/5 (inert) | EntitySet |

The two "only" rows are the oracle's resolution boundary, asserted as tests so
they cannot drift silently:

* `DropStructuralHash` is invisible to the entity layer by design and shows up
  purely as `structuralChange: true → null` — i.e. an AST-based extractor that
  cannot produce a rename-insensitive structural signal fails this gate at the
  rendered-output layer. The one non-divergent tiptap commit is one where no
  matched entity had a content change, so the cosmetic verdict was never
  computed.
* `DropKappa` is invisible to the *diff* layers, because nothing in
  `differ.rs` or `model/identity.rs` reads `kappa` today. kappa is the facts
  layer's parser-independent identity, not a diff input — which is exactly why
  the oracle checks the entity set separately from the diff instead of
  trusting the diff to cover it.

`MergeDeclarators` was **inert** on those five tiptap commits — none of the
changed files contain a multi-declarator statement. The probe reports that as
`ORACLE_LEG_INERT`, not as a pass and not as a failure: a fact about the
corpus, not about the oracle. The unit fixture, which does contain
`const first = 1, second = 2`, catches it.

### Running it

```
cargo run --release --example diff_oracle -- <repo_root> \
    [--commits N] [--skip N] [--exts .ts,.tsx] [--mutate KIND|all] [--verbose]
```

Picks the most recent non-merge commits touching the target extensions,
replays each as a synthetic PR, and prints one `ORACLE` line per commit plus
an `ORACLE_TOTAL` per leg. Exit status 1 on any divergence, or on a mutation
that escaped undetected.

## Phase 2: the oxc extractor, and what the oracle made it fix

`src/parser/plugins/code/oxc_extractor.rs` (~600 lines) implements the trait
against the pinned `=0.143.0` oxc crates. Two design choices carried the work:

**Decline the whole file, not the construct.** The moment the walk meets
something it does not model — a namespace, a re-export with a source, a
destructuring declarator, a computed member key, a class static block, an
accessor property, a function without a body (TS overload), a duplicate entity
id it would have to disambiguate, or any oxc diagnostic at all — it returns
`None` and the file goes to tree-sitter. A partial answer on every file would
be worse than an exact answer on some files, because no subset would be
trustworthy. Coverage then becomes an honest, measurable number instead of a
guess.

**kappa and `structural_hash` are computed from a canonical token stream, not
from a tree.** `canonical_token_hash` applies `KAPPA.md`'s canonicalization
discipline — drop comments and whitespace, drop `;` and `,` (the two marks a
formatter and ASI legitimately move), keep everything else including
brackets/braces (grouping) and keywords — to the entity's source slice.
`structural_hash` is the same stream with the entity's own name token
excised, which preserves the rename-insensitivity `model/identity.rs`'s
phase-2 match requires. Values are deliberately parser-scoped; see the
KAPPA.md errata below on why matching tree-sitter's values is not a thing
that can be done.

### The oracle as a rule-discovery tool

This is the part worth recording. Every rule below was found by running the
oracle over real commits and reading the first divergence — not by reading
`entity_extractor.rs` and guessing what mattered. Measured over the same 73
commits (25 from tiptap, 24 from vscode, 24 from microsoft/TypeScript):

| after adding | equivalent | vacuous | divergent | entity-set divergences |
|---|---:|---:|---:|---:|
| first working extractor | 7 | 27 | 39 | 366 |
| variable suppression inside function/method/arrow bodies; restricted initializer descent; object-literal method+arrow promotion | 14 | 27 | 32 | 366 |
| member spans trimmed of the trailing `;` | 15 | 27 | 31 | 212 |
| type-literal property/method signatures; `describe`/`it`/`test`/hook call entities; function entities descend into the body only | 29 | 26 | 18 | 66 |
| object-literal accessors (`get x() {}`); suppressed declarators keep the generic traversal (so type annotations are still reached) | 30 | 26 | 17 | 37 |
| `let f = () => {}` stays a `variable` (only `const` promotes) | **31** | **26** | **16** | **31** |

The single largest correction — 366 → 212 entity divergences — was one byte:
oxc's `PropertyDefinition`/`TSPropertySignature` spans run to the terminator,
tree-sitter's stop at the member. No amount of reading either codebase would
have made that the obvious first thing to fix; the oracle said so in one line.

The traversal model that had to be recovered and reproduced is documented on
`Frame` in `oxc_extractor.rs`: suppression of local `const`/`let` inside any
function body, promotion of `const f = () => {}` *through* that suppression,
and — the subtle one — the fact that an emitted variable or field entity
**stops** the generic child traversal and descends only into an initializer
that is a function/arrow/class/object, so `const x = wrap(function () {
function inner() {} })` never yields `inner`.

## Phase 3: integration, and the salt

`facts_store.rs::effective_language_salt` appends the installed extractor's
identity to the per-language grammar salt. Two extractor generations therefore
never satisfy each other's corpus lookups — they legitimately disagree on
`structural_hash` conventions, on kappa values, and (while one is unproven) on
entity sets. The corpus already isolates by grammar version with exactly this
mechanism, so extractor identity belongs in the same string rather than a
second version knob: one comparison, one invalidation story, and
`ingest_remote`'s claimed-salt validation means the cross-machine tiers
inherit the isolation for free. With no extractor installed the salt is
byte-identical to before, so nothing on disk was invalidated by landing this
(`effective_salt_is_unchanged_with_no_fast_extractor_installed`).

`cache::key_for` folds the same identity in, which is what makes the oracle's
two in-process legs sound.

**The feature is opt-in twice.** `oxc-fastpath` is still off by default, and
compiling it still leaves the fast path *off* at runtime unless
`SEM_FASTPATH=1`. Compiling a feature is a build-system decision; enabling an
unproven extractor is a correctness decision; they must not be the same
switch.

## Phase 4: measurement, and the finding that decides it

### The seam cannot reach the cold build. At all.

`EntityGraph::build`'s pass 1 calls `extract_entities_with_tree`, not
`extract_entities`, for every JS/TS file — because it needs the
`tree_sitter::Tree` itself: the `retain_parsed_files` arm hands it to pass 2,
and the >`PARSED_FILE_REUSE_LIMIT` arm hands it to
`precompute_js_ts_file_facts`, which takes `&tree_sitter::Tree` and builds the
scopes and AST refs pass 2 would otherwise re-parse for. A fast extractor has
no tree to give either caller. Routing them through it anyway would trade a
parallel tree-sitter parse for a *serial* pass-2 re-parse — the exact
pathology `RESOLUTION-PROFILE.md`'s semx-022 spent a whole bead removing.

Measured, feature build, `examples/perf_probe`, 3 paired runs:

| corpus | `SEM_FASTPATH=0` build_total (median) | `SEM_FASTPATH=1` | parse+extract off/on | entities |
|---|---:|---:|---:|---:|
| microsoft/TypeScript (40,872 files) | 8,635 ms | 8,624 ms | 3,009 / 3,001 ms | 454,541 / 454,541 |
| tiptap (1,533 files) | 290.9 ms | 304.7 ms | 60.1 / 53.8 ms | 42,841 / 42,841 |

Identical entity counts with the flag on are the proof, not the timings: the
fast path never ran in pass 1, so it could not have changed anything. The
~3.5s parse+extract slice this bead set out to attack is **structurally
unreachable from this seam**.

### Where the fast path *is* live, it is not the bottleneck either

`sem diff` does go through `extract_entities`. On vscode's largest recent
TS-touching commit (714 files, 912 changes), 3 runs each:

| | median wall |
|---|---:|
| `SEM_FASTPATH=0` | 2.13 s |
| `SEM_FASTPATH=1` | 2.15 s |
| `SEM_FASTPATH=0`, `--file-exts .ts .tsx` | 2.12 s |
| `SEM_FASTPATH=1`, `--file-exts .ts .tsx` | 2.11 s |

No measurable difference. A `sem diff` of this size is dominated by git blob
reads and by serializing 7.4 MB of before/after content, not by entity
extraction — so a 20-49x speedup on a small share of the work is invisible end
to end. This is the same lesson the original decline's "honest end-to-end
number" section taught, arriving from the other direction.

### The oracle's verdict on the extractor itself

73 real commits across three repos, at the final rule set:

| repo | commits | equivalent | vacuous | divergent |
|---|---:|---:|---:|---:|
| ueberdosis/tiptap | 25 | 14 | 9 | 2 |
| microsoft/vscode | 24 | 10 | 4 | 10 |
| microsoft/TypeScript | 24 | 7 | 13 | 4 |
| **total** | **73** | **31 (42%)** | **26 (36%)** | **16 (22%)** |

Coverage on the files it did not decline: **207 of 348 claimed file-sides
(59.5%)**. Residual divergences: 31 entity-set, 9 kappa-partition, 3
`DiffResult`, 1 rendered-JSON.

**16 divergent commits is a fail.** The rule is "ship nothing that fails the
oracle", and this fails.

## The second verdict: no again — but for a different, smaller reason

The original decline said *equivalence is unreachable in principle*. That was
right about `structural_hash` and wrong as a reason to stop, because
`structural_hash` was never the product. At the diff level equivalence turned
out to be perfectly reachable in principle and largely reachable in practice:
a few days of oracle-guided work took a from-scratch extractor from 7 to 31
equivalent commits out of 73, and every remaining divergence is a named,
finite, tractable rule — not a wall.

The reason to stop is different and simpler: **there is nothing on the other
side of the wall.** The seam a treeless extractor can occupy is
`extract_entities`, and neither of that seam's two consumers is
extraction-bound. Pass 1 cannot use it at all (it needs the tree, measured:
identical entity counts and identical build totals with the flag on), and
`sem diff` does not care (measured: 2.13s vs 2.15s). Finishing the extractor
would buy a correct answer to a question nobody is asking.

So: **declined a second time, on cost/benefit rather than on impossibility**,
with the equivalence question answered honestly instead of assumed.

### What would have to change to revisit

Exactly one thing, and it is not the extractor:

* **A JS/TS fast extractor would have to produce `PrecomputedFileFacts`, not
  just entities** — i.e. reimplement `scope_resolve.rs`'s
  `build_scopes_from_ast`, `collect_all_file_refs`, `scan_return_types` and
  `scan_init_self_attrs` on the same AST. Only then can pass 1 skip the
  tree-sitter parse for JS/TS, which is the only way the ~3.5s parse+extract
  slice becomes addressable. That is a much larger job than the entity rules,
  it feeds *resolution edges* rather than the diff, and the diff oracle would
  not cover it — a second oracle at the edge level would be needed first.

Note what is *not* on that list: `structural_hash`, kappa values, and the
~3,500 lines of entity rules. The first two are handled (parser-scoped, salt
isolated, partition-compared); the third turned out to be a finite list the
oracle enumerates for you.

## What was delivered, and stays

* `parser::fast_extractor` — the trait, the installable set, the identity, the
  opt-in switch. Language-agnostic; the next candidate (any language, any
  parser) plugs into it unchanged.
* `parser::diff_oracle` — the gate, with three failure modes it refuses to
  launder (divergence, vacuity, and a gate that cannot fail), and 9 mutation
  tests proving it fails when it should.
* `examples/diff_oracle` — replays real git history through both legs.
* `plugins/code/oxc_extractor` — feature-gated, opt-in, never on by default.
  Kept because it is the worked example of the trait and the corpus the next
  attempt starts from, not because it ships.
* `facts_store::effective_language_salt` and the `cache::key_for` identity —
  the cross-generation isolation, which is required infrastructure for *any*
  future fast path and is now in place and tested.
* `KAPPA.md`'s errata: kappa's values are grammar-scoped; its partition is the
  portable artifact.

### Gates

* `cargo test -p sem-core --release`: **532 lib tests** green (518 before this
  bead) plus every integration file; **544** green with `--features
  oxc-fastpath`.
* `incr_probe` 8 scenarios × 4 corpora (monster, tiptap, django, gin) with
  `SEM_FP_PARITY=1` — **32/32 ok in all three states**: default build,
  feature build with the flag off, feature build with `SEM_FASTPATH=1`.
* `facts_probe` save/load on tiptap: `edge_hash` identical cold vs warm,
  `ORACLE ... ok`.
* `cargo test -p sem-cli --release --bins`: 140/140. `cargo test -p sem-mcp
  --release`: 93/93.
* `cargo clippy` (both feature states) and `cargo fmt`: clean on every file
  this bead touched.
