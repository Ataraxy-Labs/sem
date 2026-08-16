# kappa (κ): a second, semantic identity hash — a spike

## The question

`structural_hash` (`src/utils/hash.rs`) hashes tree-sitter's full concrete
syntax tree (CST): every node kind plus every leaf's raw text, including
anonymous punctuation (`{`, `}`, `(`, `)`, `;`, `,`, ...). `OXC-FASTPATH.md`'s
cache-key section spells out why that forecloses a parser-independent fast
path: a CST walk cannot be reproduced by a typed-AST parser like oxc, babel,
or swc, because those parsers don't expose punctuation or keyword tokens as
addressable nodes at all — that information doesn't exist to walk. Separately,
`structural_hash` also churns on anything the CST represents but a human
wouldn't call a semantic change: trailing commas, semicolon style, brace
placement.

kappa is a second hash, computed **additively** alongside `structural_hash` —
nothing about `structural_hash`'s computation, meaning, or call sites changed.
kappa is defined over a canonical *semantic* form instead of the full CST, so
in principle a different parser's typed AST could reproduce it, and so pure
formatting differences (including the punctuation churn above) don't move it.

## The spec

Walk the parse tree (tree-sitter today; the rule below is written so another
parser's AST could follow it too). For every node:

- **Comments** (`comment`, `line_comment`, `block_comment`, `doc_comment`,
  `tag_comment`) are skipped entirely — same set `structural_hash` already
  skips.
- **Internal (non-leaf) named nodes**: hash the node's `kind()` string. This
  captures structure (`x = foo(bar)` vs `foo(bar) = x`), same as
  `structural_hash`, but restricted to *named* node kinds — anonymous
  internal-node wrappers (rare, grammar-specific) have no equivalent in a
  typed AST.
- **Leaf nodes**: hash the leaf's trimmed source text if, and only if, it is
  "semantically meaningful":
  1. **Named leaves** (tree-sitter's `is_named()`) are always included.
     These are identifiers, literals, and similar grammar productions — the
     tokens a typed AST exposes as node fields (`Identifier.name`,
     `Literal.value`, ...).
  2. **Anonymous leaves that are pure punctuation/delimiters** — the set
     `{ } ( ) [ ] ; ,` — are always excluded. Braces/parens/brackets/
     semicolons/commas are pure grouping and statement-termination syntax
     with no representation in a typed AST; a `Function` node has no "had a
     trailing comma" field.
  3. **Anonymous leaves that look like keywords** (text is entirely ASCII
     alphanumeric/underscore — `function`, `class`, `return`, `if`, ...) are
     excluded by default: a typed AST has no separate node for the keyword
     itself, it's implied by the enclosing node's *kind*, which is already
     hashed by the internal-node rule above.
     - **Exception**: if that keyword-shaped leaf is the *sole* child of a
       **named** parent (`parent.child_count() == 1`), the parent is a
       tree-sitter "choice of bare words" wrapper — e.g.
       tree-sitter-typescript's `predefined_type: $ => choice('any',
       'boolean', 'string', ...)` or `accessibility_modifier: $ =>
       choice('public', 'private', 'protected')`. There the keyword *is*
       the node's entire semantic payload: the parent's node-kind is
       identical no matter which word was chosen, so dropping the word
       would make `boolean` and `string` (or `public` and `private`) hash
       identically. This exception was found empirically, not designed
       up front — see "Collision analysis" below.
  4. **Everything else anonymous** — symbolic operators/punctuators that
     carry meaning (`+`, `==`, `=>`, `...`, `:`, `?`, `.`, ...) — is
     included.

Hash function: xxHash3 (`xxhash_rust::xxh3::Xxh3`), same as every other hash
in this codebase, streamed token-by-token (no intermediate string
allocation) exactly like `structural_hash`. Encoded as 16 lowercase hex
characters (`{:016x}`).

**Renames are semantic, not structural.** `structural_hash`'s entity-level
computation (`compute_structural_hash` in `entity_extractor.rs`) deliberately
excludes the entity's name token so renames of otherwise-identical entities
keep the same hash — that's what powers `differ.rs`'s rename detection.
kappa does the opposite on purpose: it includes the name, so a rename
changes kappa. kappa is an identity signal, not a rename-detection signal;
using it for rename detection would be a regression from what
`structural_hash` already does.

This spec is RFC-8785/JCS-flavored in spirit — canonicalize first (drop
whitespace, comments, punctuation, and syntax-implied-by-kind keywords),
then hash the canonical token stream — but applied to a token stream instead
of JSON.

## Implementation

- `src/utils/hash.rs::structural_and_semantic_hash(node, source,
  exclude_range)` — computes **both** `structural_hash`'s value and kappa in
  one iterative tree walk (one worklist, one cursor), writing into two
  separate `Xxh3` hashers as it goes. `exclude_range` (the name-token byte
  range) is honored only on the structural side, matching
  `structural_hash_excluding_range`'s existing behavior exactly; kappa never
  excludes it.
- `entity_extractor.rs::compute_structural_hash_and_kappa` is the thin
  per-entity wrapper (finds the name range, calls the function above) that
  replaced the old `compute_structural_hash` at all 13 `SemanticEntity`
  construction sites in the tree-sitter `code` plugin.
- **Piggybacking, not a second walk.** This was the explicit design
  constraint (target: <10% extraction overhead). Computing kappa via an
  independent second traversal per entity would have roughly doubled the
  hash-computation share of extraction time. Instead, the merged function
  walks each entity's subtree exactly once and writes to both hashers per
  node/leaf, so the *traversal* cost (tree-sitter cursor navigation, byte
  slicing, whitespace trimming) is paid once and shared; only the
  cheap per-token classification (`is_semantic_leaf`) and a second `u64`
  hasher `write()` are additional.
- **Non-regression proof for `structural_hash`.** Since
  `compute_structural_hash_and_kappa`'s structural half must be *exactly*
  what the old code computed (not "close"), `entity_extractor.rs`'s
  `kappa_regression_tests` module walks every node (not just entity nodes)
  of real TypeScript/Python/Rust fixtures and asserts the merged function's
  structural output equals the standalone `structural_hash`/
  `structural_hash_excluding_range` functions called directly. Separately,
  the entire pre-existing `structural_hash`-pinning test suite (renames,
  `parse_cache.rs`'s cached-vs-uncached equivalence, Swift operator-rename
  tests, etc.) still runs unmodified against the new code path and is green
  — see "Gate" below.

## Compat story

`SemanticEntity.kappa: Option<String>` is a new field,
`#[serde(default, skip_serializing_if = "Option::is_none")]` — identical
convention to `start_byte`/`end_byte` when those were added. Concretely:

- Old serialized JSON (no `"kappa"` key) deserializes fine — `serde(default)`
  fills `None`.
- New output omits `"kappa"` entirely when it's `None`, so any consumer doing
  exact-JSON comparison against pre-kappa output is unaffected wherever
  kappa isn't computed.
- No snapshot/golden-file tests were found pinning exact serialized entity
  JSON in this repo (checked `sem-core` and `sem-cli`), and
  `tests/kappa.rs::kappa_is_none_for_plugins_that_do_not_compute_it` directly
  asserts the omit-when-None behavior via `serde_json::to_string`.
- `tests/parse_cache.rs`'s `assert_same` (which pins `structural_hash` field-
  for-field between the cached and uncached extraction paths) was left
  untouched; it doesn't assert on kappa, but kappa rides along unmodified
  since it's computed identically on both paths.

**Coverage gaps (kappa is `None`), by design, for this spike:**

- Every plugin other than the tree-sitter `code` plugin (Markdown, JSON,
  YAML, TOML, CSV, LaTeX, Svelte, Vue, ERB, the generic fallback plugin) —
  these entities aren't extracted from a canonical-AST-shaped tree-sitter
  node the way `code` plugin entities are, so kappa isn't computed for them
  in v1. All of these mechanically gained a `kappa: None` field to keep
  compiling; none of their logic changed.
- Two `code`-plugin fallback paths that don't operate over a single clean
  AST subtree: Swift's `ERROR`-node conditional-compilation-container
  recovery, and the synthesized `SwiftPropertyBinding` segments used for
  Swift's multi-declarator `let a, b: Int` property splitting (both already
  used a text/word-based `recovered_swift_structural_hash`, not a node
  walk, before kappa existed).
- The `sem-cli` and `sem-mcp` on-disk SQLite entity caches
  (`sem-cli/src/cache.rs`, `sem-mcp/src/cache.rs`) read entities back via a
  fixed `SELECT` column list that doesn't include a `kappa` column yet, so
  entities round-tripped through those caches come back with `kappa: None`
  even though they had a real kappa before being cached. This is a real gap
  for anything that wants to rely on kappa through those caches — closing it
  needs a schema migration (new column + `ALTER TABLE`/rebuild path), which
  is out of scope for this spike. `sem-core`'s own in-process/on-disk
  content-addressed cache (`src/parser/cache.rs`) is unaffected: it
  round-trips whole `SemanticEntity` values through `serde`, so kappa
  survives it automatically.

## The acceptance demo (`tests/kappa.rs`)

11 tests, all passing, against real entities extracted through the real
`CodeParserPlugin` (not a toy hasher):

- **(a) Formatting-invariance, kappa unchanged / `structural_hash` changed —
  TypeScript, Python, and Rust, all three.** Each pair reformats a function
  (trailing comma added to the param list, TS semicolon dropped under ASI,
  reflowed signature/braces, an added comment) and asserts
  `kappa(before) == kappa(after)` while `structural_hash(before) !=
  structural_hash(after)`. **Held on all three languages.**
- **(b) Real semantic changes change kappa** — rename, changed literal
  (`x * 2` → `x * 3`), added parameter, changed body logic (`a + b` →
  `a - b`), each on TypeScript, plus a rename spot-check on Python and a
  body-logic spot-check on Rust. The rename test also re-asserts the other
  half of the story: `structural_hash` stays rename-invariant, unchanged by
  kappa's existence.
- **(c) Stability** — the same source extracted 5 times in a row produces
  identical kappa every time.
- **(d) Cache-collapse** — two separate "files" (different paths, different
  raw text: multi-line vs single-line signature, trailing comma, brace
  style, no semicolons) containing the same `clamp` function get the *same*
  kappa, while `content_hash` and `id` (both intentionally file/text
  sensitive) differ. This synthetic property was then independently
  confirmed on a real corpus — see below.

Run: `cargo test -p sem-core --release --test kappa`.

## Measurements

### Overhead

Method: `examples/perf_probe.rs`'s `PARSE_EXTRACT` phase (pure CPU:
tree-sitter parse + entity-walk on pre-read file content, parallel, no IO/
resolution) run 5-8 times each against this branch (kappa computed) and
against a clean `git worktree` at the previous commit (no kappa field at
all, `compute_structural_hash` still the old single-purpose function),
identical machine, identical corpora, `--release`.

| corpus | files | entities | kappa ON median | kappa OFF median | delta |
|---|---:|---:|---:|---:|---:|
| tiptap | 1,533 | 42,841 | 51.2 ms | 66.3 ms | **-22.7%** (ON faster) |
| microsoft/TypeScript ("monster") | 40,865 | 454,541 | 3,400 ms | 3,796 ms | **-10.4%** (ON faster) |

Kappa's ON runs were at or *below* the OFF baseline's median on both
corpora. Individual runs swung 20-40% run-to-run on this machine (background
load, thermal/scheduler noise — tiptap's OFF runs alone ranged 57.7-75.6ms
across 8 samples), which is larger than any plausible per-token hashing
cost. Honest read: **kappa's overhead is not distinguishable from this
machine's run-to-run noise floor**, and is therefore comfortably under the
spike's <10% budget. This is the expected outcome of the piggyback design
(one traversal, two hashers) rather than a surprise — the added work per
node is one cheap classification branch (`is_semantic_leaf`) plus one more
`u64` hasher `write()`, against a traversal that was already paying for
tree-sitter cursor navigation, byte slicing, and whitespace trimming.

### Collision analysis

Method: `examples/kappa_stats.rs` (kept in the repo, same pattern as
`perf_probe.rs`/`size_probe.rs` — `cargo run --release --example kappa_stats
-- <repo_root> [label] [sample_n]`) extracts every entity in a corpus,
groups by kappa, and for groups spanning more than one *distinct*
`structural_hash` (same semantic identity, different exact CST — the
interesting case), prints samples for manual review.

| corpus | entities | with kappa | distinct kappa values | groups with >1 entity | groups spanning >1 structural_hash | entities in those groups |
|---|---:|---:|---:|---:|---:|---:|
| tiptap | 42,841 | 6,562 (15.3%) | 5,545 | 331 | 2 (0.036%) | 7 |
| microsoft/TypeScript | 454,541 | 418,475 (92.1%) | 140,373 | 78,914 | 3,258 (2.32%) | 42,837 (10.2% of with-kappa) |

(`with_kappa` coverage differs a lot between corpora because tiptap is a
mixed monorepo — 402 of its files are `.vue`/`.json`/`.md` against 1,121
`.ts`/`.js`/`.tsx`, and non-code plugins don't compute kappa in v1; the
TypeScript monster is almost entirely `.ts`.)

**Found and fixed a real false-merge bug.** The first collision sample on
tiptap surfaced two cases where the *keyword-shaped-anonymous-leaf*
exclusion rule (spec item 3) was too coarse:

```
kappa=f6e7354fe2ad2ff4 n_entities=3 n_distinct_structural_hash=2
    packages/core/src/NodePos.ts :: field `editor` | private editor: Editor
    packages/extension-bubble-menu/src/bubble-menu-plugin.ts :: field `editor` | public editor: Editor
    packages/extension-floating-menu/src/floating-menu-plugin.ts :: field `editor` | public editor: Editor

kappa=0e069ebcfe9d5efe n_entities=2 n_distinct_structural_hash=2
    packages/core/src/Node.ts :: property `topNode` | topNode?: boolean
    packages/server-ai-toolkit/.../serialize-schema.ts :: property `topNode` | topNode?: string
```

`private`/`public` and `boolean`/`string` were both being dropped as
"keywords", because tree-sitter-typescript represents them as the *sole*
anonymous child of a small named wrapper node (`accessibility_modifier`,
`predefined_type` — confirmed by dumping the parse tree). Dropping them left
only the wrapper's node-kind, which is identical regardless of which word
was chosen — so a `private` field and a `public` field, or a `boolean`
property and a `string` property, hashed the same. That's a real semantic
difference a spike calling itself "identity" cannot silently merge.

Fix: the "sole child of a named parent" exception in spec item 3 (see
above) — a keyword-shaped leaf that *is* its named parent's entire content
is included, not excluded. After the fix, both examples above no longer
collide (verified by re-running `kappa_stats` on tiptap: collision groups
dropped from 4 to 2, both now genuine — see below), `cargo test -p sem-core
--release` stayed green, and the overhead measurement above is *post-fix*.

**What's left after the fix, on tiptap (2 groups, 7 entities, both good):**
duplicated `notes` array-literal fixtures across `React`/`Vue` tutorial
variants of the same demo (e.g. `demos/src/Tutorials/1-1-textarea/{React,
Vue}/...`) — same data, reformatted (multi-line vs single-line object
literals). This is exactly the cache-collapse property from acceptance-demo
(d), independently confirmed on a real corpus rather than a synthetic
fixture.

**What's left after the fix, on the TypeScript monster (3,258 groups,
42,837 entities):** overwhelmingly the same "good" pattern, but at a scale
specific to this corpus: `tests/baselines/reference/*.js` is TypeScript's
own compiler test suite — thousands of near-identical one-line snapshot
files repeated across compiler-target/module permutations
(`asyncFunctionDeclaration15_es5(target=es2015).js`,
`...(target=es5).js`, `...es6.js`, ...). Sampled groups: `length: number`
(2,152 occurrences — a common property name across many interfaces),
`class C {}` (1,047), `constructor() {}` (1,022), `function foo() {}` /
`function fn1() {}` (500-600 each), `[Symbol.dispose]() {}` (446). All
inspected samples are genuine duplicated/reformatted code, not false
merges — but this corpus is not representative of typical application code
for this metric, since a large fraction of it is auto-generated test
fixtures by construction.

**A second, un-fixed false-merge class was found and is an honest gap.**
Within the sampled monster groups: `kappa=b95d0d0870e6ad7f` merges
`const x = 1;` and `let x = 1;` (and, by the same mechanism, would merge
`var x = 1;` too) under one kappa. Unlike `accessibility_modifier`/
`predefined_type`, tree-sitter-typescript does **not** wrap `let`/`const` in
their own named node — `lexical_declaration`'s children are the bare
anonymous `let`/`const` leaf directly alongside the `variable_declarator`(s),
so the "sole child of a named parent" fix does not fire (the parent has 2+
children, same shape as `function_declaration`'s harmless, correctly-dropped
`function` keyword). Telling these apart in general requires knowing whether
a given anonymous leaf's *text* is one of several alternatives the grammar
allows at that position in that node kind — information tree-sitter's
runtime `Node` API doesn't expose per-node; it lives in the grammar's
`node-types.json`, which this implementation doesn't consult. This is
real, it is not vanishingly rare (mutability of `let`/`const`/`var` is a
meaningful, common Java Script/TypeScript distinction), and it is not fixed
in v1. See "Recommended next steps".

## v1 Gate (historical)

`cargo test -p sem-core --release`: **420 lib tests + all integration test
files green**, including the pre-existing `structural_hash`-pinning suite
(renames, `parse_cache.rs` cached/uncached equivalence, Swift operator-rename
tests) unmodified and still passing, plus the new `tests/kappa.rs` (11
tests) and `entity_extractor.rs::kappa_regression_tests` (3 tests, the
structural-hash-unchanged oracle). `cargo clippy`/`cargo fmt` are clean on
every file this spike touched (verified per-file, not workspace-wide — the
workspace has substantial pre-existing clippy debt in files this spike
didn't touch, e.g. `identity.rs`, `graph.rs`, `scope_resolve.rs`, several
`sem-cli` command modules).

## v1 Verdict (historical, superseded below)

**Kappa works, is cheap, and the additive/compat story holds — but v1 has
one confirmed, real precision gap (`let`/`const`/`var`-shaped keyword
conflation) that a foundation for parser-independent identity should not
ship silently.** The spike answered its own question honestly along the
way: the first thing it found (`accessibility_modifier`/`predefined_type`)
was fixable with a general, cheap, no-grammar-metadata rule and got fixed
in-flight; the second thing it found (`let`/`const`) needs grammar-level
information this implementation doesn't have, and got documented instead of
papered over. **v1.1 (below) closes this gap and the wider family of gaps
it turned out to be one instance of.**

---

# v1.1 (bead semx-2i2): closing the correctness gap, persisting it, and
# proving the spec universal across languages

v1.1 had four goals: (1) close the `let`/`const`/`var` gap v1 documented but
didn't fix, with a principled, reimplementable rule; (2) sweep for sibling
gaps of the *same underlying cause* (semantics carried by an anonymous
keyword the grammar doesn't wrap in its own node) across other languages;
(3) prove the spec universal — grammar-core plus long-tail languages, not
just TS; (4) persist kappa through the `sem-cli`/`sem-mcp` on-disk caches,
which v1 documented as silently dropping it.

## 1. The v1.1 discriminator rule

v1's spec had one exception to "anonymous keyword-shaped leaves are
excluded": a keyword that is the *sole child* of a named parent (the
`accessibility_modifier`/`predefined_type` fix). That rule is necessary but
not sufficient — it doesn't cover the shape `let`/`const` turned out to
have (a keyword-shaped leaf alongside *other*, non-keyword children under
one shared node kind), and it doesn't cover a grammar packing *several*
alternative keywords into one wrapper (Java's `modifiers`). v1.1 adds two
rules to `hash.rs::is_semantic_leaf`, both still purely structural (node
kind + child shape), no grammar metadata (`node-types.json`) consulted:

**Rule A — leading/any-position keyword discriminator (table-driven).** A
curated, per-node-kind allowlist,
`KAPPA_LEADING_KEYWORD_DISCRIMINATOR_PARENTS` (in `hash.rs`): for every
node kind in the table, *every* anonymous keyword-shaped child is included
in kappa, at any position — not just first, because modifiers can stack
(`static async foo() {}` has both `static` and `async`). This is safe
specifically because the table is curated per node kind: every anonymous
keyword-shaped child ever produced under one of these kinds is, by
construction of the grammar rule that produces it, a real semantic
discriminator — there's no "innocent" bare keyword sharing the slot that
inclusion could spuriously latch onto.

**Rule B — generalized "pure keyword bag" (structural, no table).** v1's
"sole child of a named parent" rule is generalized to "*every* child of the
parent is itself an anonymous keyword-shaped leaf" — a lone keyword
trivially satisfies this (subsuming v1's rule exactly), and it additionally
covers grammars that group several alternative keywords as flat siblings
under one wrapper kind instead of giving each its own single-child wrapper
(Java's `modifiers: $ => repeat1(choice('public', 'private', 'final', ...))`,
where `public final` and `private final` both produce a `modifiers` node
with two anonymous keyword children — neither is a "sole child", so v1
dropped both).

Reimplementation note for a future parser (the whole point of kappa): Rule
A needs the curated table (below) — that part is grammar-specific and has
to be rebuilt per parser/grammar the same way this implementation's table
was built (sweep + corpus collision analysis, not guesswork). Rule B is
fully structural and needs no table at all; any tree-sitter-shaped CST can
apply it as-is.

### The table (`KAPPA_LEADING_KEYWORD_DISCRIMINATOR_PARENTS`)

| node kind | language | keyword(s) discriminated | found via |
|---|---|---|---|
| `lexical_declaration` | TS/JS | `let` vs `const` | task spec (the original gap) |
| `property_declaration` | Kotlin | `val` vs `var` | per-language sibling sweep |
| `method_definition` | TS/JS | `get`/`set`/`static`/`async` (class methods; stack) | `kappa_stats` on TS-monster |
| `public_field_definition` | TS | `readonly`/`abstract`/`static`/`declare` (class fields) | `kappa_stats` on TS-monster |
| `method_signature` | TS | `get`/`set` (interface method signatures) | `kappa_stats` on TS-monster |
| `property_signature` | TS | `readonly` (interface/type-literal properties) | `kappa_stats` on TS-monster |
| `function_declaration` | TS/JS | `async` (top-level functions) | `kappa_stats` on TS-monster |
| `function_expression` | TS/JS | `async` (function expressions) | `kappa_stats` on TS-monster |
| `generator_function` | TS/JS | `async` (generator function expressions) | `kappa_stats` on TS-monster |
| `generator_function_declaration` | TS/JS | `async` (generator function declarations) | `kappa_stats` on TS-monster |
| `arrow_function` | TS/JS | `async` (arrow functions) | `kappa_stats` on TS-monster |
| `export_statement` | TS/JS | `default`/`as`+`namespace` (`=` already included: symbolic, not keyword-shaped) | `kappa_stats` on TS-monster |
| `function_definition` | Python | `async` (`async def` vs `def`) | `kappa_stats` on **django** |

Every entry after `property_declaration` was found the same way: run
`kappa_stats` on a real corpus, sample a collision group, eyeball it, check
the parse tree, add the node kind, re-run, repeat until the sample stops
turning up new kinds. This is *not* a first-principles grammar enumeration
— it's evidence-driven, and the `function_definition` (Python) entry is the
proof that matters: it was invisible to every TypeScript corpus and to the
original per-language sweep (which checked Python's `global`/`nonlocal` and
concluded Python was clean), and only surfaced once `kappa_stats` was run
against a *Python* corpus. **A single-language corpus sweep is not a
universality proof; running the collision analysis against a repo in each
language family is what actually closes gaps like this one.**

`function_definition` is also C/C++'s node kind for a function definition —
the table is a flat global list, not namespaced per language. This is safe
here specifically because C/C++ wraps every modifier (`static`, `constexpr`,
...) in its own named node (`storage_class_specifier`, `type_qualifier`),
so C/C++'s `function_definition` never has a *bare anonymous* keyword-shaped
child for Rule A to (correctly or incorrectly) fire on — confirmed by parse
tree dump and pinned by
`tests/kappa.rs::cpp_function_definition_modifiers_unaffected_by_python_fix`.

### The multi-declarator fold-in (`entity_extractor.rs`)

One more real fix, orthogonal to the hash-level rules above: TS/JS's
multi-declarator path (`const a = 1, b = 2` → one entity per declarator,
#149) computes both hashes over just the `variable_declarator` subtree, so
the enclosing `let`/`const` keyword is **never visited by the walk at all**
— no leaf-classification rule can recover what it never sees. Fixed with
`fold_declaration_keyword_into_kappa`, which mixes the declaration's leading
keyword into kappa *after* the walk, using the same hash-of-hashes idiom
this file already used to combine Dart's split signature/body kappa
(`content_hash(&format!("{}{}", sig_kappa, bod_kappa))`).
`structural_hash` is completely untouched by this — it's a pure
post-process of the returned kappa string.

### Negative tests (formatting-invariance still holds)

Every discriminator fix has a paired formatting-invariance test proving the
fix didn't make kappa formatting-*sensitive* for the form it now
discriminates on: `typescript_let_declaration_formatting_invariance_still_holds`,
`typescript_method_formatting_invariance_still_holds`, and the Kotlin
`val`/`var` case (`kotlin_val_declaration_formatting_invariance_still_holds`,
in `hash.rs`'s own test module — see below for why).

## 2. Sibling-gap sweep: full per-candidate verdict table

Every candidate the task named, plus everything the sweep + corpus loop
surfaced, checked by dumping the real parse tree and, where relevant, by
running `kappa_stats` on a real corpus:

| language | construct | collides pre-v1.1? | why / fix |
|---|---|---|---|
| TS/JS | `let` vs `const` | **yes → fixed** | `lexical_declaration`, Rule A |
| TS/JS | `var` vs `let`/`const` | no | `var` gets its own node kind (`variable_declaration`), distinct from `lexical_declaration` |
| TS/JS | `get`/`set`/`static`/`async` methods | **yes → fixed** | `method_definition`, Rule A |
| TS/JS | `readonly`/`abstract` class fields | **yes → fixed** | `public_field_definition`, Rule A |
| TS/JS | interface `get`/`set` method signatures | **yes → fixed** | `method_signature`, Rule A |
| TS/JS | interface/type-literal `readonly` properties | **yes → fixed** | `property_signature`, Rule A |
| TS/JS | `async` function/expr/arrow/generator | **yes → fixed** | 5 node kinds, Rule A |
| TS/JS | `export default`/`as namespace`/`=` | **yes → fixed** | `export_statement`, Rule A |
| TS/JS | `abstract class` vs `class` | no | own node kind (`abstract_class_declaration`) |
| TS/JS | `import type` vs `import` | n/a | imports aren't a kappa-bearing entity kind in this extractor |
| Rust | `mut` (`let x` vs `let mut x`) | no | `mutable_specifier` is a *named* leaf — rule 1 already includes it |
| Python | `global`/`nonlocal` vs plain assignment | no | each gets its own node kind (`global_statement`/`nonlocal_statement`) |
| Python | `async def` vs `def` | **yes → fixed** | `function_definition`, Rule A — found via `kappa_stats` on django, missed by the original per-language sweep |
| Go | `:=`/`var`/`const` | no | each gets its own node kind |
| Java | single modifier (`final` alone, `static` alone, ...) | no (already fine pre-v1.1) | sole child of `modifiers` — v1's original rule |
| Java | 2+ stacked modifiers (`public final` vs `private final`) | **yes → fixed** | `modifiers` is a flat list, Rule B (generalized "pure keyword bag") |
| C# | `readonly`/`public`/`private`/... | no | each modifier gets its own single-child `modifier` wrapper node |
| C/C++ | `static`/`constexpr`/`inline` on functions | no | each wrapped in its own named node (`storage_class_specifier`/`type_qualifier`); confirmed unaffected by sharing the `function_definition` table key with Python |
| C++ | `const`/`constexpr` on variables | no | each wrapped in its own named `type_qualifier` node, sole child — v1's original rule |
| Ruby | locals/globals/constants (`x`/`$x`/`X`) | no | each already a distinct *named* leaf kind (rule 1) |
| Kotlin | `val` vs `var` | **yes → fixed** | `property_declaration`, Rule A — found by the per-language sweep, not suggested by the task |
| Kotlin | visibility modifiers (`private`/`public`) | no | wrapped in `visibility_modifier`, a named node, sole child |
| Swift | `var`/`let` | no | wrapped in `value_binding_pattern`, a named node, sole child |
| Swift | `override` | no | wrapped in `override_modifier`, a named node, sole child (checked on TS/JS's `override_modifier`; Swift not independently re-verified but same wrapper-node grammar idiom) |
| PHP | visibility/`readonly` | no | each modifier gets its own single-child wrapper node (`visibility_modifier`/`readonly_modifier`) |

Schema-salt coordination note for the concurrent corpus agent (facts_store.rs/
incremental.rs): kappa values themselves are unaffected by that work — this
table only touches per-entity hash computation
(`hash.rs`/`entity_extractor.rs`), not the corpus/facts storage layer. No
shared schema salt or cache key collision expected.

## 3. Universality proof

Extended `tests/kappa.rs` with formatting-invariance + semantic-sensitivity
(rename or logic-change) pairs — real before/after fixture pairs, not
synthetic — for every grammar-core language plus three long-tail languages,
against the real `CodeParserPlugin`:

| language | family | formatting-invariance | semantic-sensitivity | verdict |
|---|---|---|---|---|
| TypeScript | grammar-core | ✓ (`typescript_formatting_only_change_leaves_kappa_identical` + declaration/method variants) | ✓ (rename, literal, param, logic, +9 v1.1 discriminator tests) | pass |
| JavaScript | grammar-core | ✓ `javascript_formatting_only_change_leaves_kappa_identical` | ✓ `javascript_semantic_changes_change_kappa` | pass |
| Python | grammar-core | ✓ `python_formatting_only_change_leaves_kappa_identical` | ✓ `python_and_rust_semantic_changes_also_change_kappa`, `python_async_def_differs_from_plain_def` | pass |
| Go | grammar-core | ✓ `go_formatting_only_change_leaves_kappa_identical` | ✓ `go_semantic_changes_change_kappa` | pass |
| Rust | grammar-core | ✓ `rust_formatting_only_change_leaves_kappa_identical` | ✓ `python_and_rust_semantic_changes_also_change_kappa` | pass |
| Java | grammar-core | ✓ `java_formatting_only_change_leaves_kappa_identical` | ✓ `java_semantic_changes_change_kappa`, `java_public_final_vs_private_final_field_differ` | pass |
| Ruby | long-tail | ✓ `ruby_formatting_only_change_leaves_kappa_identical` | ✓ `ruby_semantic_changes_change_kappa` | pass |
| C++ | long-tail | ✓ `cpp_formatting_only_change_leaves_kappa_identical` | ✓ `cpp_semantic_changes_change_kappa` | pass |
| Kotlin | long-tail | ✓ `kotlin_formatting_only_change_leaves_kappa_identical` (functions, via `CodeParserPlugin`); `kotlin_val_declaration_formatting_invariance_still_holds` (properties, direct parser) | ✓ `kotlin_semantic_changes_change_kappa`; `kotlin_val_vs_var_differ` (direct parser) | pass, with a caveat below |

**Kotlin caveat, surfaced not swallowed:** `CodeParserPlugin` doesn't
currently extract Kotlin `property_declaration` nodes as standalone
entities at all — `extract_name` (`entity_extractor.rs`) only reads a
`name` field, and Kotlin's grammar nests the identifier one level deeper
(under a `variable_declaration` child), not directly on
`property_declaration`. This is a pre-existing gap in Kotlin entity
extraction, unrelated to kappa, not touched by this task. The `val`/`var`
discriminator fix is still fully proven — just at the mechanism level,
directly against the real tree-sitter-kotlin-ng parser and the real
`structural_and_semantic_hash`, in `hash.rs`'s own `#[cfg(test)] mod tests`
(`kotlin_val_vs_var_differ`,
`kotlin_val_declaration_formatting_invariance_still_holds`), instead of
through the full entity-extraction pipeline like every other language
above. A C# pin (`csharp_public_vs_private_readonly_already_differ`) sits
next to it in the same module, proving a "no fix needed" sibling case at
the same mechanism level.

## 4. Persisting kappa through the on-disk caches

v1 documented that `sem-cli`'s and `sem-mcp`'s on-disk SQLite entity caches
(`sem-cli/src/cache.rs`, `sem-mcp/src/cache.rs`) read entities back via a
fixed `SELECT` column list that didn't include `kappa`, so cached entities
always came back `kappa: None` even though they had a real kappa before
being cached. Closed with a schema-versioned migration:

- `sem-mcp/src/cache.rs` is the schema owner both crates share
  (`sem-cli/src/cache.rs` does `use sem_mcp::cache as shared_cache` and
  calls its `initialize_schema`/`insert_entities_with_content_store`
  directly; `sem-cli` has its own separate `DiskCache`/`save`/`load`
  orchestration, but reuses the shared schema/insert helpers). One column
  addition there covers both crates' storage layer.
- `entities.kappa TEXT` added to `CACHE_SCHEMA_SQL`.
- `CACHE_SCHEMA_VERSION` bumped `9 -> 10`. This cache's migration story is
  "stale schema -> full rebuild" (`initialize_schema` compares
  `PRAGMA user_version` and runs `CACHE_RESET_SQL`, which drops every
  table, on any mismatch) — not in-place `ALTER TABLE` — so the version
  bump is the entire migration; any cache built under v9 gets dropped and
  rebuilt clean under v10 the next time it's opened.
- `insert_entities_with_content_store` (the shared insert, used by both
  crates' full-save paths) writes `e.kappa` into the new column.
- Every `SELECT ... FROM entities` that reconstructs a full `SemanticEntity`
  (3 sites in `sem-mcp/src/cache.rs`, 3 in `sem-cli/src/cache.rs` — that
  crate keeps its own duplicate read paths for its own query shapes) now
  selects `kappa` and sets `kappa: row.get(N)` instead of the old
  `kappa: None`.

**Round-trip proof, twice over:**

1. **Real integration test, both crates** —
   `sem-cli/src/cache.rs::tests::kappa_round_trips_through_disk_cache` and
   `sem-mcp/src/cache.rs::tests::kappa_round_trips_through_disk_cache`:
   extract real entities (including a `let`/`const` pair, to prove the
   v1.1 fix survives the round trip too) via the real `CodeParserPlugin`,
   `.save()` to a real on-disk SQLite file, drop the connection, reopen
   fresh, `.load()`, and assert every entity's kappa is unchanged —
   including that `let`/`const` still differ after the round trip. Both
   green.
2. **Real CLI invocation** — built the release `sem` binary, ran
   `sem impact <entity> --file decls.ts --deps --json` against a scratch
   git repo containing `let mutableCounter = 1; const frozenCounter = 2;`
   with `SEM_CACHE_DIR` pointed at an isolated scratch directory (the
   production path: `save_incremental_with_repair_metadata`, used by `sem
   graph`/`sem impact`, calls the same shared
   `insert_entities_with_content_store`), then inspected the resulting
   `cache.db` directly with `sqlite3`:

   ```
   $ sqlite3 cache.db "PRAGMA user_version;"
   10
   $ sqlite3 cache.db "SELECT name, entity_type, kappa FROM entities;"
   mutableCounter|variable|e067ca856d55b33c
   frozenCounter|variable|9a741219c8e3faef
   add|function|20049fe6fcaf1699
   ```

   Schema is v10, the `kappa` column exists and is populated by a real CLI
   run, and `mutableCounter`/`frozenCounter` (the `let`/`const` pair)
   have different kappa on disk — the v1.1 discriminator fix, persisted.

## 5. Collision analysis, re-run post-fix

Same method as v1 (`kappa_stats`), re-run after each fix, plus a **third
corpus — django, a Python repo** — added specifically because the
`async def` gap (§1/§2) was invisible to every TypeScript-only corpus and
would have been invisible to a TypeScript-only universality proof too.

| corpus | entities | with kappa | distinct kappa | groups >1 entity | collision groups (span >1 structural_hash) | entities in those groups | v1 baseline (collision groups / entities) |
|---|---:|---:|---:|---:|---:|---:|---:|
| tiptap | 42,841 | 6,562 (15.3%) | 5,545 | 331 | **2** | **7** | 2 / 7 (unchanged — no let/const/get-set/async instances happened to collide in this corpus) |
| microsoft/TypeScript ("monster") | 454,541 | 418,475 (92.1%) | 141,766 (was 140,373) | 79,862 (was 78,914) | **2,147** | **18,048** | 3,258 / 42,837 — **-34.1% groups, -57.9% entities** |
| django (new) | 37,104 | 37,011 (99.7%) | 35,092 | 687 | **4** | **10** | n/a (not tested pre-v1.1) |

**tiptap**: unchanged from v1 — its 2 groups / 7 entities are the same
`notes` array-literal React/Vue tutorial-variant duplicates v1 documented;
this small, mixed-monorepo corpus simply doesn't happen to contain any
`let`/`const`, getter/method, or `async`/non-`async` pair with matching
name+params+body to exercise the fix either way.

**microsoft/TypeScript ("monster")**: the collision mass dropped by more
than half. Iterative breakdown across the fix sequence (each row is a full
re-run after landing that fix):

| after fixing | collision groups | entities |
|---|---:|---:|
| v1 baseline | 3,258 | 42,837 |
| `let`/`const` (Rule A) + Java modifiers (Rule B) | 3,123 | 40,178 |
| `method_definition`/`public_field_definition` (class get/set/static/async/readonly/abstract) | 2,432 | 25,937 |
| + `method_signature` (interface get/set) + `function_declaration`/`function_expression`/`generator_function`/`generator_function_declaration`/`arrow_function` (`async` everywhere else) | 2,344 | 23,608 |
| + `property_signature` (interface/type-literal `readonly`) | 2,149 | 18,157 |
| + `export_statement` (`default`/`as namespace`) | 2,147 | 18,048 |
| + `function_definition` (Python `async def` — found on django, not this corpus) | 2,147 | 18,048 *(unchanged, as expected — confirms no cross-language interference from sharing the `function_definition` table key with C/C++)* |

Each row is a full `kappa_stats` re-run against the whole corpus after
landing that fix, not an estimate. The interface `readonly length: number`
sample (2,152 entities merged with plain `length: number`) specifically
dropped out between the `method_signature`/`function_declaration` row and
the `property_signature` row, which is most of that step's -1,451-entity
delta.

Sampled the top-40-by-size collision groups by hand at every step (not just
at the end). **Everything remaining is a genuine duplicate or a genuine
formatting/ASI variant** — the corpus's nature explains why: `tests/
baselines/reference/*.js` is TypeScript's own compiler test suite,
thousands of near-identical one-line snapshot files repeated across
compiler-target/module permutations (`asyncFunctionDeclaration15_es5
(target=es2015).js`, `...(target=es5).js`, `...es6.js`, ...). Representative
genuine patterns still in the tail: `length: number` (2,152, shared
property name across many real interfaces), `class C {}` (1,047, trivial
test-fixture stub repeated), `var x;`/`let x = 1;`/`const x = 1;` (100s each,
repeated across `target=`/`module=` permutation files), ASI variants of the
same statement (`return "foo";` vs `return "foo"`). No false merges found
in the post-fix sample.

**django**: 4 groups, 10 entities, **all genuine** — the same
`Migration`/`Celebrity`/`compress` class-and-function-name reuse pattern
across different test fixture files (multi-line vs single-line list
literal formatting, or byte-identical bodies in two different test
modules). No false merges found. This is the cleanest of the three corpora
by a wide margin (10 entities out of 37,011 with kappa — 0.027% — vs the
monster's 4.3%), consistent with django being real application/test code
rather than a permutation-generated compiler test suite.

## 6. Gates

- **`tests/kappa.rs`: 42 tests, all green** (11 original v1 tests + 31 new
  v1.1 tests: the discriminator fixes, their formatting-invariance
  companions, the sibling-gap "no fix needed" regression pins, and the
  6-language universality matrix above).
- **`hash.rs`'s own `#[cfg(test)] mod tests`: +3 new tests** (Kotlin
  `val`/`var` fix + formatting-invariance, C# readonly sibling-gap pin,
  the pre-existing `content_hash`/`short_hash` tests untouched).
- **`entity_extractor.rs::kappa_regression_tests`: 3 tests, still green** —
  the structural-hash-unchanged oracle. Re-verified after every v1.1
  change in this task (the discriminator rules, the multi-declarator
  fold-in): `structural_hash` is providably byte-identical to what the
  standalone `structural_hash`/`structural_hash_excluding_range` functions
  compute, on every node of real TS/Python/Rust fixtures — not just
  entity nodes.
- **`cargo test -p sem-core --release`: fully green** — 512 lib tests (up
  from 420 at v1, mostly from unrelated concurrent work landed on this
  branch during this task — `git log` shows `semx-4an`/`semx-9en`/
  `semx-kzy`/`semx-ocj`/`semx-14b`/`semx-2o8` commits from other agents in
  the same session) + every integration test file (`kappa.rs`,
  `parse_cache.rs`, `scope_resolve_bench.rs`, `graph_accuracy.rs`,
  `d_smoke.rs`, `elm_smoke.rs`, `bow_import_lookup_bench.rs`).
- **`cargo test -p sem-mcp --release`: 93/93 green**, including the new
  `kappa_round_trips_through_disk_cache`.
- **`cargo test -p sem-cli --release`: 136/136 lib unit tests green**
  (including its `kappa_round_trips_through_disk_cache`) + every
  integration test file green **except** `tests/impact_direct_deps.rs`
  (12/17 fail). Investigated and confirmed **unrelated to this task**:
  every failure is the same assertion — `SEM_TIMINGS=json` produces empty
  stderr on the cached-topology-query fast path — reproduced directly with
  the built binary (`sem impact ... --json` with `SEM_TIMINGS=json` set),
  confirmed the code path involved (`try_cached_impact_query` in
  `commands/impact.rs`, calling `DiskCache::query_impact_topology`) is a
  narrow, separate SQL query that does **not** touch the entities
  `SELECT`/`kappa` logic this task changed, and confirmed the actual
  command output (`stdout`, the JSON result) is correct in every case —
  only the `SEM_TIMINGS` side channel is affected. Neither `impact.rs` nor
  `timings.rs` is a file this task touched (surface: `hash.rs`,
  `entity_extractor.rs`, `model/entity.rs`, `tests/kappa.rs`, `sem-cli`/
  `sem-mcp` cache layers only). Surfaced, not fixed, per scope.
- **`cargo clippy`/`cargo fmt`: clean** on every file this task touched
  (`hash.rs`, `entity_extractor.rs`, `tests/kappa.rs`, `sem-cli/src/
  cache.rs`, `sem-mcp/src/cache.rs`) — verified per-file with
  `--all-targets`, same "not workspace-wide" caveat as v1 (pre-existing
  debt elsewhere, untouched by this task, left alone).
- **Extraction overhead**: re-measured with `examples/perf_probe.rs`'s
  `PARSE_EXTRACT` phase on tiptap and the TypeScript monster, comparing
  this task's changes against the same code with `hash.rs`/
  `entity_extractor.rs` stashed back to their pre-v1.1 (v1) state, same
  machine, same corpora, `--release`, several runs each. Result: **noise-
  dominated, same finding as v1's own overhead measurement** — median
  swung both above and below the pre-v1.1 baseline across repeated runs
  (e.g. tiptap: 46.6-105.9ms across both configurations, with v1.1 runs
  landing on both sides of the v1 baseline's median depending on when they
  ran), consistent with heavy concurrent load on this shared machine
  during this task (multiple other agents building/testing in the same
  working tree at the same time — see `git log` for the interleaved
  commits). Architecturally, v1.1's added cost per token is the same shape
  as v1's: one more `&str`/`&[u8]` comparison (a table lookup or a sibling
  scan bounded by a node's small child count) gated behind the existing
  `looks_like_keyword` branch, which only a small fraction of tokens ever
  reach — no new allocations, no second traversal, same one-hasher-write
  piggyback design. Not independently re-measurable to a tighter bound
  than v1's given this machine's current noise floor; the architectural
  argument is the same one v1 relied on for the same reason.

## v1.1 Verdict

**The `let`/`const`/`var` gap is closed, and it turned out to be one
instance of a much larger family — the same "keyword carries meaning but
the grammar doesn't wrap it in its own node" shape recurs across TS/JS
declarations, class members, interface members, callables, and export
statements, and, critically, is not TS/JS-specific: Python's `async def`
has it too, found only because the collision analysis was run against a
real Python corpus instead of stopping at TypeScript.** The discriminator
rule (Rule A, table-driven) and its structural sibling (Rule B, the
generalized keyword-bag test) together close every collision this task's
sweep and corpus analysis found, verified three ways: targeted unit tests
per fix, a 6-language (TS, JS, Python, Go, Rust, Java, plus Ruby/C++/Kotlin
long-tail) universality matrix, and before/after collision-count deltas on
three real corpora. kappa now persists through both on-disk caches with a
proper schema-versioned migration, proven by both a real save/reopen/load
integration test and a real CLI invocation inspected at the SQLite level.
The one open finding this task surfaced but did not fix —
`sem-cli`'s `impact_direct_deps.rs` timings-JSON regression — is outside
this task's file surface and is reported, not silently absorbed.

kappa is now precise enough, and persists reliably enough end-to-end
(extraction → cache → reload), to be the foundation the oxc-revival design
(bead semx-r63) needs for parser-independent identity, and to safely power
a "formatting-only PR" signal in the diff UI (previously blocked
specifically by the `let`/`const` gap this task closes).

---

# Errata (bead semx-r63): kappa is **not** parser-independent

This document opens by calling kappa "a second, semantic identity hash" whose
spec is "written so another parser's AST could follow it too", and v1.1's
verdict offers it as "the foundation the oxc-revival design (bead semx-r63)
needs for parser-independent identity". semx-r63 reimplemented the spec
against oxc's typed AST — the first time anything actually tried — and that
claim does not survive. This section records the correction; nothing about
kappa's *computation* changed, and no kappa value on disk moves.

## The claim, and the counterexample

The spec's second rule is: **"Internal (non-leaf) named nodes: hash the node's
`kind()` string."** Those strings are tree-sitter grammar identifiers —
`lexical_declaration`, `formal_parameters`, `required_parameter`,
`statement_block`. They are not a property of the source code; they are a
property of *the grammar that parsed it*. To reproduce a kappa value, a
second parser must emit the same kind strings, in the same depth-first order,
with the same tree shape — which is to say, it must reproduce
tree-sitter-typescript's grammar node for node. That is the same CST fidelity
`OXC-FASTPATH.md` proved unreachable, arriving through a different door.

The counterexample needs no second parser at all, only two grammars this
repo already ships. Four byte-identical files:

```
$ printf 'export function add(a, b) {\n  return a + b;\n}\n' \
    | tee add.ts > add.js && cp add.ts add.tsx && cp add.js add.jsx
$ cargo run --release --example kappa_stats -- <dir> probe
SUMMARY label=probe files=4 total_entities=4 with_kappa=4 without_kappa=0
KAPPA_GROUPS label=probe distinct_kappa=2 groups_with_gt1_entity=2
```

**Two distinct kappa values for four identical files.** The TypeScript and
JavaScript grammars wrap a parameter list differently (TS interposes
`required_parameter` where JS has a bare `identifier`), so the internal-node
rule hashes a different token stream. A hash that changes when the *grammar*
changes — with the source, the parser vendor and the language all held fixed
— is grammar-scoped, not parser-independent.

This was always visible in the design and was never tested for, because every
test in `tests/kappa.rs` compares kappa values produced by *one* grammar
against each other. That is the right test for formatting-invariance and for
semantic sensitivity, and it is silent on portability.

## What is actually portable, and what to use instead

What kappa is *for* survives the correction intact. Its job is to decide
**which entities share a semantic identity** — that is what the facts corpus
keys on, what cache-collapse exploits, and what a "formatting-only PR" signal
reads. That is an *equivalence relation*, and the relation is reproducible by
a different parser even though the labels naming its classes are not:

* κ's **values** are grammar-scoped. Two parser generations must never compare
  them, and after semx-r63 they cannot: `facts_store.rs::effective_language_salt`
  folds the extractor identity into the per-language salt, so a corpus entry
  written under one extractor can never satisfy a lookup made by another (and
  `ingest_remote` validates the claimed salt against it, so the cloud tier
  inherits the isolation).
* κ's **partition** is the portable artifact. `parser::diff_oracle`'s
  `KappaPartition` layer compares exactly that: the grouping of entities into
  identity classes, as positions, never as hashes. An extractor is
  kappa-equivalent iff it groups the same entities together, whatever it calls
  the groups.

Concretely, for anyone reimplementing kappa on another parser: **do not try to
match values.** Implement the same *canonicalization discipline* — drop
comments and whitespace, drop the punctuation a formatter owns (`;` and `,`),
keep everything else including grouping and keywords — over whatever token
stream your parser can produce, give the result a distinct extractor identity,
and prove equivalence with the partition check.
`plugins/code/oxc_extractor.rs::canonical_token_hash` is a worked example: it
is ~90 lines against oxc, versus a grammar-shape reconstruction that would
have been the whole of tree-sitter-typescript.

## What did not change

* No kappa value changes. `hash.rs`, `entity_extractor.rs`, the discriminator
  table (Rule A) and the keyword-bag rule (Rule B) are untouched by semx-r63.
* Every claim in v1 and v1.1 about formatting-invariance, semantic
  sensitivity, the false-merge rate (0.027-0.05% on django/tiptap) and
  cross-language universality stands — those are all statements about kappa
  *within one grammar*, which is where kappa is used.
* The v1.1 verdict's last sentence should be read as: kappa is precise enough
  and persists reliably enough to be the foundation for parser-independent
  *identity comparison*, via its partition — not via its values.
