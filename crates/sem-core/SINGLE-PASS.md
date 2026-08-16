# SINGLE-PASS.md — the single-pass columnar build (W1, semx-3tb)

The design document for wave 1 of the sub-1s campaign. Its claim is not that
the build can be made faster by working harder; it is that **the build is
already computing each thing more than once, and the duplication is
provably removable** — every removal below is licensed by a named theorem,
not by a benchmark. The oracles then witness what the theorem already
proves.

Inputs this document is derived from, and does not restate:

- `RESOLUTION-PROFILE.md` "## Sub-1s physics budget (semx-8lf)" — the pass
  census (passes A..N with `file:line`) and the floor ledger.
- `RESOLUTION-PROFILE.md` "## W0.5: the free lunch, landed and measured
  (semx-ccg)" — the post-W0.5 baselines this wave is measured against, and
  the disclosed bare-import residual.
- `QUERY-INDEX.md` — the index sections (`ENTITIES`/`NAMES`/`REFS`/
  `TRIGRAM`/`FILES`/`DIRS`) the new flow must still emit, byte-for-byte.
- `KAPPA.md` — the kappa identity hash, and where it is computed.
- `RESOLUTION-PROFILE.md` "## Interning (semx-5nc)" — the measured decline
  of `u32` interning, which §4 of this document respects rather than
  re-litigates.

---

## 1. The algebra, and the data flow derived from it

### 1.1 The structures

**S1. Fold-fusion (Meijer–Fokkinga–Paterson 1991, §2.3 the fold-fusion
invariant for pairs; Bird–de Moor, *Algebra of Programming*, §6.)**

Carrier: `μF` = the concrete syntax tree of one file (tree-sitter's CST),
and `E*` = the free monoid of source bytes.

```
cata :: (F b → b) → μF → b
```

Invariant (the fold-fusion invariant for pairs):

```
⟨cata f, cata g⟩  =  cata ⟨f, g⟩            (BS)
```

Read operationally: *N independent folds over the same tree are equal to one
fold producing the N-tuple.* Not "approximately equal", not "equal up to
scheduling" — equal. Every pass in the census that is a fold over the same
`μF` (entity extraction, the kappa/structural hash, reference collection,
trigram extraction over the same bytes) is therefore already, by (BS), a
component of a single fold that nobody has written down yet. **The pass
collapse is an identity, and the only thing it can change is cost.**

The corollary that matters for the gates: because (BS) is an equality of
functions, the fused walk's output is *definitionally* the tuple of the
unfused outputs. A bit-identical gate is not a hope about a rewrite; it is
the theorem's observable shadow. If a gate ever fails, the code did not
implement `cata ⟨f,g⟩` — the invariant is not in question.

**S2. Deforestation (Meijer–Fokkinga–Paterson 1991 §4;
Wadler 1990, "Deforestation: transforming programs to eliminate trees".)**

```
fused f g  =  cata f ∘ ana g          and       cata f ∘ ana g  never needs μF
```

The parse tree, and the file's byte string, are *intermediate* structures
between bytes and facts. Where consumption is single-pass, the intermediate
need not be materialized past the walk. This is the well-behaved form of "drop
the bytes early": it is not a memory hack, it is the statement that the
walk's μF is not part of the denotation. Consequence for W1: the fused
per-file walk emits **derived columns** and drops the bytes *inside the
parallel closure* — the corpus's content is never simultaneously resident.

**S3. Separation algebra / the frame rule (O'Hearn 2001,
separation logic; realized in this repo already and proven in the red-green
work, `RESOLUTION-PROFILE.md` "Red-green incremental resolution".)**

```
⟦repo⟧  =  ⊕_{f ∈ files} ⟦f⟧        ⊕ commutative, associative, on disjoint keys
```

Per-file facts compose by disjoint union. This is the license for
per-file parallelism *without coordination*: the fused walk is a `map` into
a commutative monoid, and the merge is `⊕`. Nothing in the fused walk may
read another file's column — that, and only that, is the proof obligation
the frame rule imposes on the implementation.

**S4. Free monoid quotient (interning) (Mac Lane, CWM III.1, free objects;
Wadler 1992 — the free monoid on `A` is `List A`).**

An interner is the injective quotient `ι : String ↣ u32` with
`ι(x) = ι(y) ⇔ x = y`. Equality-by-token is *sound by injectivity*, which
is what licenses replacing string joins with integer joins. §4 designs it;
§5 fences its implementation, on measured grounds (semx-5nc), not on
doubt about the algebra.

**S5. Work–span / Brent's theorem (Brent 1974, "The parallel evaluation of
general arithmetic expressions", §2).**

```
T_P  ≥  max(T_1 / P, T_∞)
```

`T_1/P` is the work term (what deleting duplicate passes shrinks); `T_∞`
is the span — the critical path, here "the slowest single file", which no
amount of parallelism removes. The floor ledger in `RESOLUTION-PROFILE.md`
§3 is this inequality instantiated per giant. **W1 attacks `T_1` only.**
Any claim that a fusion moves `T_∞` would need the span re-measured, and
W1 makes no such claim.

### 1.2 The denotation

Write `⟦·⟧` for the meaning map from the build's syntax (its phases) to the
artifacts. The whole of W1 is the assertion that this diagram commutes:

```
        bytes(f)
           │  ana (tree-sitter)
           ▼
         μF(f)                     ⟨entities, κ, refs/scopes, trigrams,
           │  cata ⟨f₁..f₅⟩   ──▶   is_test, spans, hashes⟩ = Col(f)
           ▼
         Col(f)
           │
           ⊕ over files (S3)
           ▼
      Columns(repo)  ──join──▶  edges  ──serialize──▶ {facts, index, cache.db}
```

and that today's code computes the *same* `Col(f)` several times over,
because it re-derives `bytes(f)` several times (four disk reads) and walks
`Col(f)`'s components in separate phases.

### 1.3 The target flow (derived, not chosen)

```
bytes ─[one read]→ tree ─[one walk]→ per-file columnar facts
      ─[⊕ disjoint merge]→ corpus columns ─[one join]→ edges
      ─[serialize from columns]→ {facts blobs, index sections, cache tables}
```

Each arrow is forced:

| arrow | forced by |
|---|---|
| one read | (S2): bytes are the fused walk's intermediate; a second read is a second `ana` of a value the first `ana` already produced |
| one walk | (BS): the per-file passes are folds over one `μF` |
| per-file columns | (S3): `⟦f⟧` must be self-contained for `⊕` to be the merge |
| ⊕ merge | (S3): commutative-associative on disjoint keys ⇒ order-free, parallel |
| one join | edges are a relational join of two columns; (S4) says the key may be a token |
| N serializations from one form | the artifacts are *encodings* of `Columns`, not recomputations of it |

**The theorem that does the most work in this wave is the smallest one.**
`crates/sem-mcp/src/cache.rs`'s `file_content_hash` is
`format!("{:016x}", xxh3_64(bytes))` (`sem-core/src/utils/hash.rs:9-11`),
and `sem-core/src/parser/incremental.rs:440`'s `content_hash` is
`Xxh3::new().update(bytes).digest()` — the same xxh3-64 of the same bytes.
So

```
cache.db files.content_hash  =  hex₁₆ ( index FileFingerprint.content_hash )
```

Two "different" fingerprints, one number, two encodings; `hex₁₆` is an
injection, so neither loses information the other has. The build currently
reads the corpus twice and hashes it twice to compute a value and its own
hex rendering. Naming that identity deletes an entire full-corpus read and
an entire full-corpus hash, with no approximation anywhere.

---

## 2. The pass-collapse map

Every row of semx-8lf's census (`RESOLUTION-PROFILE.md` §1, passes A..N),
its place in the derived flow, and its verdict. `FUSE` = survives as a
component of the one walk. `DELETE` = the pass ceases to exist. `KEEP` =
stays a distinct phase, with the reason it resists fusion stated.

| # | pass | file:line (census) | place in the new flow | verdict |
|---|---|---|---|---|
| A | file discovery walk | `commands/graph.rs:54` | produces the index set of `⊕`; touches no file bytes | **KEEP** — metadata only; not a content pass |
| B | parse read (1st byte read) | `parser/graph.rs:2066` (`build_incremental_core`), `:3068` (`build_direct_dependencies`) | *the* read; the `ana` of the flow | **KEEP as the single read** |
| C | parse + extract, κ fused | `entity_extractor.rs` / `compute_structural_hash_and_kappa` | the `cata` of the flow | **KEEP (already fused — verified, §2.1)** |
| D | bow content snapshot (in-memory full copy) | `parser/graph.rs:1499-1522`, `content.clone()` at `:1510` | nothing: `Col(f)`'s content column is shared, not copied | **FUSE→DELETE the copy** (share, don't clone) |
| E | bow index build + tokenize | `graph.rs:1308-1436` | per-file, already fused with that file's own resolve step | **KEEP fused** — see §2.2; a prior de-fusion regressed |
| F | scope/reference resolution | `scope_resolve.rs`, `graph.rs:1525+` | the join's candidate generation | **KEEP** — W3's lane (scope fence, §5) |
| G | edge assembly/dedupe/sort | `resolve_profile.rs` accumulators | the join | **KEEP** |
| H | file fingerprint (2nd byte read) | `build_cache.rs:340-348` → `cache.rs:687` | column `content_hash`, already computed by the one read | **DELETE** (hex₁₆ of the same u64, §1.3) |
| I | refresh file-import entries (3rd byte read) | `build_cache.rs:368` → `cache.rs:872-926` | column `js_ts_imports`, computed in the one read | **FUSE** (read deleted, scan kept) |
| J | insert entities with content | `shared_cache::insert_entities_with_content_store` | serialization from the entity column | **KEEP** — W4's lane (scope fence, §5) |
| K | insert edges | `build_cache.rs` edges block | serialization from the edge column | **KEEP** |
| L | index write parallel re-read + re-hash (4th byte read) | `build_cache.rs:172-197` | columns `content_hash` + `trigrams` | **FUSE** — read deleted, trigram extraction moved into the one read |
| M | index build image | `index/writer.rs:157` | serialization from columns | **KEEP**, but consumes `trigrams` instead of re-walking `contents` |
| N | sqlite commit + atomic index write | `tx.commit()`, `index::write_atomic` | the write | **KEEP** |

### 2.1 Pass C: the κ fusion is already landed (verified, not assumed)

`compute_structural_hash_and_kappa` (`entity_extractor.rs`) computes the
structural hash **and** the kappa identity hash in the same node walk that
produces the entity, inside the same `extract_entities*` call that pass B's
read feeds. There is no separate κ pass to fuse; semx-8lf's census already
recorded this ("confirmed fused, not a separate kappa pass"). W1 verifies
and documents it; it writes no code for it. Stating this plainly is part of
the deliverable: *a fusion that is already done must not be re-sold as a
win.*

### 2.2 Pass E: fused, and it must stay per-file

`snapshot_bow_content`'s doc comment records a measured failure: an earlier
bead built every file's `FileReferenceIndex` in a separate phase, inserting
a `.collect()` barrier between "index every file" and "resolve every file",
and **regressed** wall time (+5-8% build total, +8-17% resolve) because
every file's resolve then waited on the corpus's *slowest* index build
instead of its own. In (S5) terms: the barrier converted a sum of per-file
spans into `N ×` the maximum span — it raised `T_∞`. This is the standing
counterexample to naive "fuse everything into one phase" reading of (BS):
**(BS) fuses folds over the same structure; it does not license inserting a
synchronization barrier between two folds that were previously pipelined.**
W1 keeps E where it is and only removes the *copy* feeding it (pass D).

### 2.3 What the collapse leaves

Target state, stated so it can be checked rather than believed:

- **1 corpus read** (pass B) — every other full-corpus byte read deleted.
- **1 walk per file** (pass C) producing the column tuple.
- **1 join** (F+G).
- **N serializations** (J, K, M, N) that *encode* columns and never
  re-derive them.

§8 records how far the landed implementation actually got, per fusion, with
the residual named.

---

## 3. The columnar form

Build-scoped, produced once, consumed by every serializer. One row per
file, stored column-major so a serializer touches only the columns it
encodes:

```
Col(f) = ⟨ path        : &str          -- the ⊕ key (S3)
         , mtime       : (i64, u32)    -- stat, not read
         , hash        : u64           -- xxh3-64; hex₁₆ for cache.db (§1.3)
         , entities    : [SemanticEntity]
         , kappa       : u64           -- fused in C
         , refs/scopes  : PrecomputedFileFacts
         , trigrams    : FxHashSet<u32>
         , imports     : [String]      -- resolved JS/TS import targets
         , is_test     : bool
         , spans       : (u32, u32)
         ⟩
```

`⊕` is disjoint-key union: `Columns = ⊕_f Col(f)`, which in Rust is exactly
`par_iter().map(col).collect()` — the frame rule is discharged by the
closure capturing nothing per-file-mutable.

**Bytes are not a column.** They are the fused walk's intermediate (S2): read,
folded into the columns above, dropped inside the closure. Today's
`write_query_index` materializes `HashMap<String, Vec<u8>>` of the *entire*
corpus (1.5 GB on linux) *and* the per-file trigram sets simultaneously;
the columnar form holds only the latter.

---

## 4. The intern table (design; implementation fenced)

Design, per (S4), for W3's join keys — decided at ingest, in the one walk:

```
ι : String ↣ u32          build-scoped, monotone, never reused across builds
Sym = u32                 ι(x) = ι(y) ⇔ x = y                     (injective)
```

- **Build-scoped, not session-scoped.** A session-owned interner makes
  token identity a function of build history, which breaks the "identical
  inputs ⇒ identical artifacts" property every oracle in this campaign
  depends on. Build-scoped keeps `ι` a pure function of the corpus.
- **Decided at ingest.** The one walk is the only place every string is
  already in hand; interning anywhere later means a second visit — exactly
  the duplication this wave exists to delete.
- **Two-level, per (S3).** Each file interns into a *local* table during
  its own closure (no shared mutable state, frame rule preserved); the
  merge `⊕` renumbers local tokens into the global table. Renumbering is a
  monoid homomorphism, so the merge stays associative.
- **What it buys (W3):** `symbol_table`/`entity_map` joins on `u32` instead
  of `String`; `resolve_ref`'s bucket walk becomes integer comparison.

**Fenced, deliberately.** `RESOLUTION-PROFILE.md` "## Interning (semx-5nc)"
already measured this: a 93.1% win *inside* the idealized token-in-hand
lookup, but the function it lives in is 0.11-2.29% of cold build time on
every corpus measured — below the materiality bar, before the interner's
own build cost. The algebra is sound and the design above stands; W1 does
not implement it, because implementing it would add a noun and move no
measured number. Re-opening it is W3's call, with W3's own measurement.

---

## 5. Scope fences — what W1 deliberately does not do

- **No resolve-internals rework (W3, semx-1ff).** `scope_resolve.rs`,
  candidate generation, the tie-break contract, `REF_CACHE`, and
  `scope_build`'s effective parallelism are untouched. W1 may not change
  which edges are produced — the bit-identical gate is the enforcement.
- **No parser swaps (W2, semx-au8).** No oxc, no alternative extractor, no
  change to `structural_hash`'s definition over the tree-sitter CST.
- **No cache.db content-store decision (W4, semx-431).** Whether
  `insert_entities_with_content_store` (pass J, 1.8-14.9 s/giant, the
  single largest cache-write cost) is still load-bearing on a cold build is
  the question semx-8lf §3 surfaced and explicitly assigned to W4. W1 makes
  J cheaper only insofar as it stops re-reading bytes; it does not delete
  the write.
- **No index format change.** `QUERY-INDEX.md`'s sections keep their bytes.
  A fusion that changed section bytes would fail the index oracle, which is
  the intended outcome.
- **No new artifact, no new query path.** W1 is a pass collapse, not a
  feature.

---

## 6. The property-test plan — one fusion-invariant witness per fused pass

Each fusion ships with an **invariant-shaped property test**: not "the output
looks right on this fixture", but the fold-fusion equation itself,
quantified over arbitrary fixtures.

The general shape, for a fusion that replaces walks `f` and `g` by one walk
producing the pair:

```
∀ corpus.  fused(corpus)  ≡  ⟨unfused_f(corpus), unfused_g(corpus)⟩       (BS-witness)
```

The unfused side is kept alive in the test as the *specification*, so the
test cannot degrade into "the new code agrees with itself".

| # | fusion | invariant witnessed | arbitrary |
|---|---|---|---|
| W1-F1 | one save-plane read feeding fingerprint ⊕ imports ⊕ trigrams | `read_columns(files) = ⟨fingerprint_of_each, imports_of_each, trigrams_of_each⟩` where each right-hand component is computed by an independent re-read | arbitrary file corpora: sizes incl. 0/1/2 bytes (trigram edge), non-UTF-8, manifest names, nested dirs |
| W1-F2 | `hex₁₆ ∘ u64` hash identity | `file_content_hash(p) = format!("{:016x}", incremental::content_hash(read(p)))` | arbitrary byte strings incl. empty |
| W1-F3 | bow content sharing (no copy) | `bow_edges(shared) = bow_edges(copied)` — the ⊕-image is invariant under representation of the content column | arbitrary multi-file corpora with cross-file references |
| W1-F4 | stem-index bare-import resolution | `resolve_bare_import_stem(index, s) = sorted_candidates.find(stem = s)` — *min over a set = first element of its sorted restriction*, a total-order identity | arbitrary candidate path sets + specifiers, incl. ties across extension priorities |

Plus the standing non-vacuity discipline: each property asserts its generated case is non-trivial (corpus
non-empty, ≥1 file with ≥3 bytes so trigrams exist, ≥1 resolvable import),
and each has a positive control — a deliberately broken variant must turn
the test RED — recorded in the test file's header.

And the standing oracles, unchanged, as the corpus-scale witness of the
same invariants: entity/edge counts, sorted-edge hash, kappa partition, index
bytes, `cache.db` table dumps, all six `index_probe` oracles, and the
diff-output battery.

---

## 7. Honest non-claims

- W1 does not claim any giant reaches <1 s. semx-8lf's floor ledger already
  says four of five cannot, for reasons W1 does not touch (parse physics,
  resolve disambiguation).
- W1 does not claim a `T_∞` (span) improvement — only `T_1` (work).
- W1 does not claim the fused walk is *faster per byte*; it claims there
  are fewer walks. Where a fusion trades wall time for peak memory (holding
  a derived column longer than the bytes it came from), the trade is
  measured and reported, and fenced if it loses.
- The bit-identical gates are not evidence that the fusion is correct *in
  general* — they are evidence that on the measured corpora the
  implementation realizes `cata ⟨f,g⟩`. The property tests are what carry
  the general claim.

---

## 8. What landed (post-hoc, against §2's plan)

Full numbers, per-fusion deltas, the five-giant battery and the LOC ledger
live in `RESOLUTION-PROFILE.md`'s "## W1: single-pass columnar (semx-3tb)"
section. The scoreboard against this document's own plan:

| planned | landed? | where |
|---|---|---|
| κ fold fused into the extraction walk | already true — **verified, no code** | §2.1 |
| bow content copy deleted | **yes** (F3, `Cow::Borrowed`) | `graph.rs` `snapshot_bow_content` |
| trigram extraction fused into a read | **yes** (F1) | `corpus_columns.rs` + `index::TrigramSource` |
| one read feeding fingerprint + index hash | **yes** (F1) — and the two hashes turned out to be one | §1.3 |
| cache.db + facts + index serialized from one columnar form | **partly** — `cache.db`'s `files`/`file_imports` and the index's `FILES`/`TRIGRAM` all come from `CorpusColumns`; the entity content store (pass J) still walks entities separately, which is W4's fence | §5 |
| stem-index residual wired | **yes** (F4) | `ImportCandidates` |
| **the single corpus read** (parse ⊕ save columns) | **no — fenced, with a number** | profile §"The fence" |

Reads went 4 → 2, not 4 → 1. The remaining pair is fenced on a measurement
(the second read is 0.9-2.8% of a cold build; fusing it would hold ≈0.32×
corpus bytes of trigram columns across the resolve-phase memory peak), not
on difficulty — which is the honest form of §7's non-claims.

Bead: semx-3tb.
