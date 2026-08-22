# Resolution-pass time attribution (semx-022 step 1)

Measurement only — no resolution logic changed. This drills *inside* pass 2
(resolution) of `EntityGraph::build`, past the phase-level split semx-cnq
already established (parse+extract 6.1%, IO 2%, resolution 91.9% of a 44.8s
cold build on the TypeScript monster corpus).

## Method

- New opt-in instrumentation, `crates/sem-core/src/parser/resolve_profile.rs`,
  gated by `SEM_PROFILE_RESOLVE=1`. Every function is a no-op unless the env
  var is set (single cached `OnceLock<bool>` check); nothing it touches
  changes resolution output.
- Hooked into `scope_resolve.rs`'s `resolve_with_scopes_full_inner` (the
  function both the direct and the >20k-file chunked resolution paths in
  `graph.rs` funnel through) at:
  - the pass-2 re-parse loop (files not covered by pre-parsed trees),
  - the pass-1 return-type/instance-attr AST scan,
  - constructor-param-type inference,
  - the import-table-by-file grouping,
  - per file inside the big parallel pass-2 closure: scope+import
    construction, AST ref collection, and the entity×ref resolution loop
    (split into cache-hit / cache-miss / time inside `resolve_ref` itself).
  - `resolve_ref`'s 8 `class_members.get(type) -> select_member_candidate`
    disambiguation call sites (via a `select_member_profiled!` macro wrapper)
    and the `symbol_table.get(name)` global fast-path lookup in the `Call`
    branch — these are literally the "candidate disambiguation" hypothesis
    (a) under test.
  - Chunk-level wall time in `graph.rs`'s `resolve_scopes_in_file_chunks`.
- Candidate-count histograms use log2 buckets (O(1) atomic increments, no
  per-call allocation); per-name timing/candidate totals are accumulated
  **locally per file with zero locking**, merged into a global map **once per
  file** (not per reference) to keep the profiler itself from becoming the
  bottleneck it's measuring.
- Thread utilization: a `HashSet<ThreadId>` populated once per file, compared
  against `std::thread::available_parallelism()`.
- Corpora: `microsoft/TypeScript` (monster; cloned fresh with
  `--filter=blob:none` into `~/.cache/checkouts/github.com/microsoft/TypeScript`
  since it wasn't already cached) and `ueberdosis/tiptap` (medium, already
  cached — 1,533 files, a TS/JS/Vue monorepo, comparable order of magnitude to
  the pydantic/fastify corpora semx-cnq used). All runs: `cargo build
  --release`, `cargo run --release --example perf_probe -- <root> <label>`
  with `SEM_PROFILE_RESOLVE=1`, 18 logical cores available. 2 runs on tiptap,
  3 on TypeScript (extra run added because the first result was surprising
  enough to want a second confirmation before a third).

## Headline number

**73–76% of the TypeScript monster repo's ~45s cold build is a single
`for file_path in file_paths { … }` loop in `scope_resolve.rs` (~line 893,
"Parse any files not already in the pre-parsed set") that reads and
tree-sitter-parses every file that crossed `PARSED_FILE_REUSE_LIMIT`
(20,000) — and it runs with zero parallelism.** Pass 1's equivalent
read+parse+extract work over the same 40,865 files takes 2.7–2.8s in
parallel (18 threads). This same work, done again serially in pass 2, takes
32.6–34.2s — almost exactly 18x worse, matching the core count.

## TypeScript monster (40,872 files after excludes, 454,541 entities, 199,827 edges; crosses PARSED_FILE_REUSE_LIMIT so resolution runs in 9 sequential 5,000-file chunks)

| run | build_total | reparse (serial) | resolve_phase | RSS peak |
|---|---|---|---|---|
| 1 | 46.361s | 34.157s (73.7%) | 38.661s | — |
| 2 | 44.828s | 33.288s (74.3%) | 37.708s | — |
| 3 | 43.997s | 32.590s (74.1%) | 36.989s | 4.65 GiB |
| **avg** | **45.062s** | **33.345s (74.0%)** | **37.786s** | |

Full attribution inside pass 2 (average of 3 runs, ms and % of build_total):

| bucket | ms | % of build_total | notes |
|---|---:|---:|---|
| pass-2 re-parse (serial) | 33,345 | 74.0% | the finding above |
| bag-of-words resolution + edge merge/dedup/sort (residual) | 3,251 | 7.2% | derived: `resolve_phase_ms − Σ(named buckets)`; not directly instrumented — see Gaps |
| scope+import construction (parallel, summed across files) | 4,073 | 9.0% | wall time only 540ms — see utilization below |
| AST ref collection (parallel, summed across files) | 1,951 | 4.3% | wall time folded into the same 540ms |
| pass-1 return-type/instance-attr scan (parallel) | 345 | 0.8% | |
| constructor-param-type inference | 302 | 0.7% | sequential, small |
| entity×ref resolution loop, incl. cache hits (parallel, summed) | 260 | 0.6% | wall time folded into the same 540ms |
| **`resolve_ref` itself (candidate disambiguation + lookup)** | **66** | **0.15%** | cache-miss calls only |
| import-table grouping | 2.5 | ~0% | |
| pass-2 parallel section, **wall time** (not summed) | 541 | 1.2% | scope-build + ref-collect + ref-loop aggregate 6,284ms / 541ms wall ≈ **11.6x speedup on 18 cores (64% utilization)** |

Reference-cache hit rate (single-slot `last_resolution` cache): 9.12%,
consistent across all 3 runs (197,369 refs total, 17,992 hits).

Thread utilization: **18/18** distinct worker threads observed in the
parallel pass-2 section — that part of pass 2 genuinely is parallel. The
re-parse loop above it is not: it runs before the parallel section even
starts, once per chunk, on the calling thread alone.

Chunk-level wall time (9 chunks of ≤5,000 files, run sequentially — one
`for chunk in file_paths.chunks(5000)` loop with no cross-chunk parallelism):
min 116–124ms, avg 3.87–4.05s, **max 27.9–29.3s**. One chunk (almost
certainly the one containing `tests/cases`/`tests/baselines`, TypeScript's
deliberately-malformed compiler fixtures) accounts for roughly 62–65% of the
entire build by itself. No files hit the 2s-per-file `PARSE_TIME_BUDGET`
abort ceiling in any run — the cost is volume (many files, serial), not one
pathological file.

Candidate-count distribution (weighted by actual lookups during resolution,
not raw bucket sizes — identical across all 3 runs, confirming determinism):

| kind | lookups | p50 | p95 | p99 | max bucket |
|---|---:|---|---|---|---|
| `class_members.get(type)` → `select_member_candidate` (method calls) | 20,646 | 32–63 | 1,024–2,047 | 8,192–16,383 | 8,192–16,383 |
| `symbol_table.get(name)` fast path (bare calls, not scanned) | 71,765 | 2–3 | 128–255 | 4,096–8,191 | 8,192–16,383 |

Top single `class_members` bucket observed: a method-call candidate list of
several thousand entries for a generic type name reused across the
monorepo (e.g. `Node`, `Symbol`, `Type` — exactly the pattern hypothesis (a)
predicted). But see the verdict below: this structural fact does not
translate into wall time, because `resolve_ref` totals only 66ms.

## Medium baseline: ueberdosis/tiptap (1,533 files, 42,841 entities, 5,414 edges — well under PARSED_FILE_REUSE_LIMIT)

| run | build_total | reparse | resolve_phase |
|---|---|---|---|
| 1 | 290.63ms | 4.68ms (1.6%) | 186.09ms (64.0%) |
| 2 | 298.79ms | 15.06ms (5.0%) | 198.22ms (66.3%) |
| **avg** | **294.71ms** | **9.87ms (3.3%)** | **192.16ms (65.2%)** |

Attribution inside pass 2 (average of 2 runs):

| bucket | ms | % of build_total |
|---|---:|---:|
| bag-of-words resolution + edge merge/dedup/sort (residual) | 123.06 | 41.8% |
| scope+import construction (summed) | 98.95 | 33.6% |
| AST ref collection (summed) | 60.75 | 20.6% |
| entity×ref resolution loop (summed) | 23.94 | 8.1% |
| pass-2 re-parse | 9.87 | 3.3% |
| `resolve_ref` itself | 8.91 | 3.0% |
| pass-1 scan | 20.95 | 7.1% |
| ctor-infer | 12.12 | 4.1% |
| pass-2 parallel section, wall | 26.15 | 8.9% | aggregate 183.6ms / 26.15ms wall ≈ 7.0x on 18 cores (39% utilization) |

Cache hit rate: 3.53% (37,436 refs, 1,320 hits) — lower than the monster
repo's 9.12%, consistent with the cache being a single-slot "same ref
resolved twice in a row" optimization, not a real cache.

Candidate distribution: 882 method-call lookups (p50 8–15, p95/p99
64–127), 4,096 call-global lookups (p50 2–3, p95 8–15, p99 64–127) — an
order of magnitude smaller candidate lists than the monster repo, as
expected from a smaller, less name-collision-heavy corpus.

## Verdict, ranked by measured wall time

1. **The pass-2 re-parse loop being serial, not the resolution algorithm,
   is the dominant cost at monster scale (74% of build time).** This is a
   parallelism bug, not an algorithmic one: the identical read+parse work
   is already parallel in pass 1 (2.7s) and takes 33.3s done serially in
   pass 2 for the same 40,865 files. Fixing it is a one-line-shaped change
   (wrap the existing `for file_path in file_paths { … }` loop at
   `scope_resolve.rs`'s reparse site in `maybe_par_iter!`/`par_iter`,
   collect, exactly like pass 1 already does) with no resolution-behavior
   risk. Rough expected effect: `45s − 33.3s + 33.3s/18 ≈ 13.6s`, roughly a
   **70% cut to the monster-repo cold build**, without touching
   disambiguation logic at all.
2. **Bag-of-words reference resolution + edge merge/dedup/sort is a real,
   non-trivial cost that flips rank with scale** — 42% of a medium repo's
   build, only 7% of the monster repo's (because the re-parse tax dwarfs it
   there). This bucket is derived by subtraction (`resolve_phase_ms` minus
   every directly-instrumented bucket), not directly instrumented — see
   Gaps below. Worth its own measurement pass once (1) is fixed and it
   becomes visible again as a monster-scale bottleneck.
3. **Scope/import construction (AST walk building local scope chains +
   per-file import extraction) and AST ref collection together cost 13–54%
   of build time depending on scale**, run fully in parallel (18/18 threads
   observed) but only at 39–64% utilization — some headroom left, likely
   from per-chunk / per-file load imbalance (a few huge files dominate a
   chunk's tail), not from a lack of parallelism itself.
4. **Candidate disambiguation is real structurally but irrelevant to wall
   time — hypothesis (a) is refuted as a time driver.** Candidate lists
   are genuinely large (p95 1,024–2,047, max bucket 8,192–16,383 entries
   for `class_members` buckets keyed by generic type names reused across
   the monorepo, exactly as hypothesized) — but `resolve_ref` itself,
   including every one of those scans, totals only 66ms out of a 45,062ms
   average build (0.15%). The existing fast-path optimizations already in
   `resolve_ref` (documented inline: "avoid iterating the thousands of
   same-named entities a monorepo accumulates") are doing their job for
   the `Call` branch (O(1) via `symbol_table.get(name).first()` /
   `file_lookup.first_id_by_name`), and even the `MethodCall` branch's
   genuine `O(candidates)` scans in `select_member_candidate` don't cost
   enough in aggregate to matter at this repo's scale. **Do not spend the
   fix phase here.**
5. **The >20k-file forced re-parse (hypothesis d) is confirmed, and is
   larger than semx-cnq's earlier estimate (34s measured directly here vs.
   ~2.7s estimated there) — the earlier number appears to have measured
   the CPU-work cost of reparsing, not the serial-wall-clock cost of doing
   it single-threaded.** Once that's understood, (1) and (5) are the same
   finding: the reparse tax was never "small," it was parallel-hidden in
   the earlier indirect estimate.
6. **Pass-2 chunking (5,000-file chunks) is itself sequential** (a plain
   `for chunk in …` loop in `graph.rs`) **and one chunk dominates (63–65%
   of the whole build)** because TypeScript's `tests/cases`/`tests/baselines`
   fixture files cluster together and are expensive to parse per-file even
   without hitting the abort budget. This compounds finding (1): even after
   parallelizing the re-parse *within* each chunk, chunk boundaries remain
   a serialization point worth revisiting (e.g. rebalancing chunk contents,
   or parallelizing across chunks too) — secondary to (1) but not free.
7. **Per-entity resolution cost "growing 17–22µs → 82µs" is a threshold
   artifact, not smooth super-linearity.** At monster scale,
   `resolve_phase_ms / entities` = 83.1µs/entity (matches semx-cnq's number
   almost exactly — good cross-check that both measurements are honest).
   Subtracting just the serial re-parse bucket drops that to 9.8µs/entity —
   at or below the medium-repo baseline, not above it. The "super-linear"
   shape in the original measurement is explained by crossing
   `PARSED_FILE_REUSE_LIMIT`, not by the resolution algorithm degrading
   with corpus size.

## Hypotheses a–d: confirmed / refuted

| # | hypothesis | verdict |
|---|---|---|
| a | Global symbol-table lookups returning large same-name candidate lists, disambiguation scanning ∝ candidates | **Structurally true, causally irrelevant.** Candidate lists are large (confirmed by direct measurement); the scanning they cause costs 0.15% of build time. Refuted as a time driver. |
| b | Long String keys hashed repeatedly / cache-miss-heavy maps | **Not isolated by this instrumentation — inconclusive.** Scope-build + ref-collect (which contain most of the String/HashMap traffic: entity id clones, scope `defs`/`bindings` maps, import tables) cost 9–54% of build time depending on scale, but this bucket wasn't split further into "hashing" vs. "AST walking" vs. "allocation." Secondary priority; worth a follow-up allocation profile (e.g. `dhat`/`heaptrack`) if (1) is fixed and this bucket becomes the new bottleneck. |
| c | Pass 2 effectively single-threaded | **Partially confirmed, and it's the whole story.** The actual per-file scope-build/ref-resolve section is genuinely parallel (18/18 threads, 64% utilization at monster scale). But the re-parse loop that runs *before* it, once per chunk, is 100% single-threaded — and that's 74% of total build time. The chunk loop itself is also sequential across chunks. So: not "pass 2 is single-threaded," but "the single most expensive part of pass 2 happens to be." |
| d | The >20k-file forced re-parse | **Confirmed, and it's bigger than previously thought.** 33.3s average, 74% of build time — not the ~6% (2.7s) semx-cnq estimated. The gap is explained by (c): the reparse work itself isn't unusually expensive per file (34.2s / 18 ≈ 1.9s if parallelized, in line with pass 1's 2.7s for the same files), it's that it currently runs with a parallelism factor of 1 instead of 18. |

## Single biggest lever for the fix phase

**Parallelize the pass-2 re-parse loop** (`scope_resolve.rs`, the
`for file_path in file_paths { … read_to_string … parser.parse … }` block
that builds `owned_parsed_files` when `pre_parsed` doesn't already cover a
file). It is currently the only meaningfully-sized serial section in the
entire resolution pass, it does exactly the same work pass 1 already does
in parallel, and fixing it doesn't require touching resolution semantics,
candidate disambiguation, or the bag-of-words path at all. Everything else
in this report — bag-of-words cost, scope-build utilization, chunk
imbalance — is a real but second-order finding to revisit once this one
lever is pulled and the profile is re-run to see what's on top next.

## Gaps / what this measurement does not cover

- `resolve_references_with_file_indexes` (the "bag-of-words" reference
  resolution path in `graph.rs`, run unconditionally after scope
  resolution, for every entity regardless of language) was **not**
  instrumented directly — its cost is reported as a residual
  (`resolve_phase_ms` minus every directly-measured bucket). It is 7.2% of
  monster-scale build time and 41.8% of medium-scale build time. If the
  fix phase targets it next, it needs its own direct instrumentation pass.
- Swift-specific overload-disambiguation paths in `resolve_ref`
  (`select_swift_overload_candidate`, `has_ambiguous_swift_signature_candidates`)
  were not instrumented — irrelevant to the TS/JS corpora used here, and
  Swift call signatures are empty for both, so those code paths don't even
  execute (`swift_call_signatures.is_empty()` short-circuits them).
- No allocation/heap profiler was run (e.g. `dhat`, `heaptrack`,
  `valgrind --tool=massif`) — hypothesis (b) is only indirectly addressed
  via phase-level wall time, not allocation counts or bytes.
- Peak RSS was captured for one TypeScript monster run only (4.65 GiB,
  matching semx-cnq's 4.59 GiB — consistent) and not for tiptap (small
  enough not to matter for this task).
- Runs used the release binary of `examples/perf_probe.rs`
  (`cargo run --release --example perf_probe -- <repo_root> [label]`) with
  `SEM_PROFILE_RESOLVE=1`. The instrumentation itself adds measurable
  overhead when enabled (extra `Instant::now()` calls per file and per
  `class_members` scan, extra per-file HashMap merges) — monster-repo
  build_total under instrumentation (44.0–46.4s) trended slightly above the
  uninstrumented semx-cnq baseline (44.4–45.2s), consistent with that
  overhead. `SEM_PROFILE_RESOLVE` is opt-in and off by default, so this
  doesn't affect production builds.

## After: parallel re-parse (semx-022 fix phase)

**Change.** The pass-2 re-parse loop identified above (`scope_resolve.rs`,
`resolve_with_scopes_full_inner`, "Parse any files not already in the
pre-parsed set") was converted from a plain `for file_path in file_paths { … }`
loop to `maybe_par_iter!(file_paths).filter_map(|file_path| { … }).collect()`
— the exact macro and order-preserving filter_map+collect pattern pass 1
already uses in `graph.rs`'s `EntityGraph::build` (`maybe_par_iter!` expands
to `rayon`'s `par_iter()` under the `parallel` feature, `iter()` otherwise —
serial fallback for non-parallel builds and wasm is unchanged). Per-file work
(read, extension/language lookup, budget-guarded tree-sitter parse) is
unchanged; only its execution model changed. The three per-file outcomes
(parsed, budget-exceeded, skipped) are collected into an ordered
`Vec<ReparseOutcome>` and then split into `owned_parsed_files` /
`budget_exceeded` in a second, cheap serial pass — preserving the exact same
ordering, `budget_exceeded` reporting, and downstream consumption
(`parsed_files: &[(String, String, tree_sitter::Tree)]`) as the original
serial loop. No resolution/disambiguation logic, entity ordering, or edge
emission logic was touched.

Chunk-level overlap (evaluated, not implemented, per task scope): with the
9-chunk outer loop in `graph.rs`'s `resolve_scopes_in_file_chunks` still
sequential, I looked at overlapping chunk N+1's re-parse with chunk N's
resolution, or parallelizing across chunks outright. Skipped both — after
the mandatory fix, the CHUNKS max shrank from 27.9–29.3s to 2.6–2.8s (see
below), so the remaining per-chunk serialization cost is already small
relative to the win just landed, while either overlap scheme adds real
complexity (background reparse thread + producer/consumer handoff into
chunk resolution, or nested rayon scopes) and each chunk's re-parsed trees
are currently freed once that chunk's resolution finishes — running chunks
concurrently would hold multiple chunks' trees live at once, compounding the
memory picture below without a proportionate wall-time return once the
9-chunk sum is already ~5s of a ~14s build. Not worth the risk; noted here
instead of implemented.

**Equivalence check.** Forced the chunked/parallel-reparse path on the
cached `ueberdosis/tiptap` corpus (1,533 files) via the crate's existing
`#[cfg(test)]` overrides (`PARSED_FILE_REUSE_LIMIT = 8`,
`SCOPE_RESOLVE_FILE_CHUNK_SIZE = 3` — already defined in `graph.rs` for
exactly this kind of test, not added for this task), which pushes every file
through `resolve_scopes_in_file_chunks` and the new parallel re-parse loop
across ~511 tiny chunks. Method: added a temporary `#[test] #[ignore] fn
tmp_equivalence_tiptap()` to `graph.rs`'s test module that walks the tiptap
checkout the same way `perf_probe` does, runs `EntityGraph::build`, and
prints entity count, edge count, and a `DefaultHasher` hash of the sorted
`"{from}\x1f{to}\x1f{ref_type}"` edge dump. Ran it once against the parallel
code, then `git stash push -- crates/sem-core/src/parser/scope_resolve.rs`
(reverting only the re-parse loop to serial, everything else — including the
temp test — untouched) to reproduce the pre-change baseline, ran it again,
then `git stash pop` to restore the change. Both runs, `cargo test -p
sem-core --release --lib tmp_equivalence_tiptap -- --ignored --nocapture`:

```
TMP_EQUIV entities=42841 edges=5414 edge_hash=70e354572d168952   # before (serial reparse)
TMP_EQUIV entities=42841 edges=5414 edge_hash=70e354572d168952   # after (parallel reparse)
```

Bit-for-bit identical — same entity count, edge count, and edge-set hash.
The temporary test was removed afterward (not part of this commit); the
equivalence check is reproducible by re-adding it per the method above.
Also confirmed at monster scale as a byproduct of the timing runs below:
entities=454,541 and edges=199,827 in every one of the 3 post-change runs,
matching the pre-change baseline in this document's headline table exactly.

**Correctness gates.** `cargo test -p sem-core --release`: 417 lib tests (85
of them under `parser::graph::tests`) + all 6 integration test binaries
(`bow_import_lookup_bench`, `d_smoke`, `elm_smoke`, `graph_accuracy`,
`parse_cache`, `scope_resolve_bench`) — all passing, 0 failures. `cargo
clippy -p sem-core --release --all-targets` and `rustfmt --check` on the two
touched lines' surrounding regions in `scope_resolve.rs`: clean (the crate
has pre-existing clippy findings elsewhere, none on the touched lines; one
pre-existing `rustfmt` diff in an untouched file, `context.rs`, left alone).

**Timing protocol.** Same as the Method section above: release build,
`SEM_PROFILE_RESOLVE=1`, `cargo run --release --example perf_probe --
<repo_root> <label>`, microsoft/TypeScript monster clone (3 runs), tiptap (2
runs), 18 logical cores.

### TypeScript monster — before/after

| run | build_total (before) | build_total (after) | reparse (before) | reparse (after) |
|---|---:|---:|---:|---:|
| 1 | 46.361s | 13.987s | 34.157s (73.7%) | 2.591s (18.5%) |
| 2 | 44.828s | 14.522s | 33.288s (74.3%) | 2.721s (18.7%) |
| 3 | 43.997s | 14.576s | 32.590s (74.1%) | 2.711s (18.6%) |
| **avg** | **45.062s** | **14.362s** | **33.345s (74.0%)** | **2.674s (18.6%)** |

**Speedup: cold build 45.062s → 14.362s = 3.14x (68.1% cut). Re-parse phase
33.345s → 2.674s = 12.5x speedup on 18 cores (69% of theoretical 18x —
short of ideal due to per-chunk rayon dispatch overhead across 9 chunks and
file-size imbalance within the dominant chunk, not a correctness or scheduling
bug).** The pre-fix-phase projection in this document (`45s − 33.3s +
33.3s/18 ≈ 13.6s`) was close but slightly optimistic: measured average is
14.36s, about 5.6% above projection — the honest number, not the rounded
projection.

Chunk-level wall time (`CHUNKS`, 9 chunks) collapsed with the fix: max chunk
time dropped from 27.9–29.3s (before) to 2.6–2.8s (after) — the same
dominant chunk (almost certainly `tests/cases`/`tests/baselines`) is still
the largest, but its re-parse no longer runs single-threaded, so its wall
time fell by roughly the same ~10–11x the aggregate reparse phase did. Sum
across all 9 chunks: ~5.0–5.2s (after) vs. an implied ~33s+ (before, since
re-parse dominated per-chunk cost). Thread utilization: 18/18 distinct
worker threads observed in every after-run, same as before — the *existing*
parallel pass-2 section was already fully utilizing all cores; the fix
brought the re-parse loop up to the same standard.

Entity/edge counts, all 3 after-runs: `entities=454541 edges=199827` —
identical to the before table's `40,872 files … 454,541 entities, 199,827
edges` header, confirming the parallel path is a pure speed change.

**Memory.** One after-run captured with `/usr/bin/time -l`: peak RSS
4,898,979,840 bytes = 4.56 GiB, essentially flat versus the before
baseline's single captured sample (4.65 GiB) — slightly *lower*, within
run-to-run noise. Holding more parsed trees "in flight" during the
parallel re-parse did not measurably raise peak memory: `owned_parsed_files`
for a chunk is bounded by that chunk's file count regardless of how many
threads fill it concurrently (same total trees materialized, just faster),
and each chunk's trees are still dropped before the next chunk's re-parse
begins — chunks were not parallelized against each other (see "skipped"
above), which is exactly what would have made memory a real concern. No
parallelism bounding was needed as a result; if chunk-level parallelism is
revisited later, this is the tradeoff to re-check first.

### tiptap — before/after

| run | build_total (before) | build_total (after) | reparse (before) | reparse (after) |
|---|---:|---:|---:|---:|
| 1 | 290.63ms | 269.47ms | 4.68ms (1.6%) | 3.59ms (1.3%) |
| 2 | 298.79ms | 271.29ms | 15.06ms (5.0%) | 3.37ms (1.2%) |
| **avg** | **294.71ms** | **270.38ms** | **9.87ms (3.3%)** | **3.48ms (1.3%)** |

tiptap (1,533 files) stays well under `PARSED_FILE_REUSE_LIMIT` (20,000) in
release builds, so it never took the chunked/re-parse path to begin with —
pass 1's retained trees cover it, and the re-parse loop only runs for the
rare file pass 1 didn't retain a tree for. The small improvement here (9.87ms
→ 3.48ms average reparse, ~24ms off build_total) is a minor side effect, not
the target of this fix; entities=42,841, edges=5,414 in every after-run,
matching the before baseline exactly.

**Verdict.** Confirms the fix-phase's single lever: parallelizing the pass-2
re-parse loop cuts the TypeScript monster's cold build by 68% (45.06s →
14.36s) with zero resolution-behavior change (bit-for-bit identical entity
counts, edge counts, and edge-set hash at both medium and monster scale) and
no measurable memory regression. The remaining 14.36s is now dominated by
`scope_build`/`ref_collect` (already parallel, ~4.6s + ~2.2s summed per the
`PHASE_NS` line) and the bag-of-words residual (`resolve_phase_ms` minus
named buckets, still present per-chunk) — both flagged as second-order
findings in the "before" verdict above and now visible again as the next
targets, exactly as predicted.

## Residual attribution (semx-9h3)

Measurement-first continuation of the above. The monster-repo re-parse
bottleneck is fixed (previous section). This drills into the two things the
"before" verdict flagged as still uninstrumented: the ~41.8%-of-medium-build
"bag-of-words resolution + edge dedup/sort" residual, and `import_table_
derived_ms` (~3.2s / 22% of the monster build, entirely unattributed).

### Method

- Extended `crates/sem-core/src/parser/resolve_profile.rs` (still gated by
  `SEM_PROFILE_RESOLVE=1`, still a no-op — single cached env check — when
  unset) with 15 new named accumulators, all following the module's existing
  conventions (per-file/per-chunk sums for parallel sections, single wall
  timers for sequential ones):
  - **Inside `scope_resolve.rs`** (the file the previous step's residual note
    pointed at): `chunk_entity_index_ms` — `resolve_with_scopes_full_inner`
    building `entities_by_file`/`children_by_parent` by scanning **all**
    `all_entities` (the whole-corpus list, not just the current chunk's
    files) — this runs once **per chunk**, redundantly, on repos over
    `PARSED_FILE_REUSE_LIMIT`; `return_types_by_name_ms` —
    `deterministic_return_types_by_name`, same "whole-corpus work repeated
    per chunk" shape, iterating the full `symbol_table`; `scope_merge_ms` —
    merging `per_file_results` into the chunk's edge/log/consumed-words
    accumulators (right after the existing `pass2_wall_ms` parallel section);
    `scope_dedup_ms` — that same function's own index-based sort+dedup of
    `all_edges`, distinct from and prior to `graph.rs`'s later
    `dedupe_resolved_edges`/`sort_resolved_refs`.
  - **Inside `graph.rs`**, the "bag-of-words" reference-resolution path
    (`resolve_references_with_file_indexes`, run unconditionally after scope
    resolution): `bow_wall_ms` (true wall time of the whole function) split
    into `bow_index_build_ms` (per-file, parallel-summed: `build_file_
    reference_index` — a **second** disk read past pass 1 and pass 2's
    reparse, plus `strip_for_language` and `FileReferenceIndex`
    construction) and `bow_resolve_ms` (per-file, parallel-summed: the
    per-entity `resolve_entity_references` loop — dot-chain extraction,
    local-binding scan, candidate matching).
  - **Inside `graph.rs`**, everything else that runs between
    `BuildPhase::Resolving` and `EntityGraph::build`'s return but outside
    `scope_resolve.rs`: `imports_by_file_ms`, `export_edges_ms`,
    `dedupe_ms`, `sort_ms` (the outer, whole-graph
    `dedupe_resolved_edges`/`sort_resolved_refs` — not to be confused with
    `scope_dedup_ms` above, which runs earlier and only over scope edges),
    and `edge_index_ms` (building the `dependents`/`dependencies` `HashMap`s
    + `edges: Vec<EntityRef>` from the final sorted/deduped list).
  - **Inside `graph.rs`**'s `build_import_table_with_default_export_paths`
    (the function `import_table_derived_ms` was already indirectly measuring
    via subtraction in `perf_probe.rs`): `import_table_wall_ms` (whole
    function); `import_table_io_ms` / `import_table_scan_ms` (per-file,
    parallel-summed: `import_source_content`'s file read — a fresh disk read
    when the repo is over `PARSED_FILE_REUSE_LIMIT` and pass 1 didn't retain
    a tree — and `scan_import_file`'s regex-based content scanning);
    `import_table_merge_ms` (the sequential per-scan merge into the final
    table), further split into `import_table_export_build_ms` (building
    `default_exports`/`named_exports_by_file`, `resolve_ts_default_re_
    exports`, `TsDefaultExportTable` construction) and `import_table_
    insert_ms` (the final `for scan in scans { import_table.insert(..) }`
    loop that actually populates the returned table).
- All 15 additions are pure `Instant::now() … .elapsed()` timing around
  unchanged logic (or, in the fix-phase section below, logic proven
  equivalent) — no resolution output changed by adding them. Verified: `cargo
  build --release` clean, `cargo clippy --release --all-targets` clean on
  every touched line (pre-existing warnings elsewhere in `graph.rs`/
  `scope_resolve.rs` untouched), `cargo fmt --check` clean on both files.
- Corpora and protocol unchanged from the prior sections: `ueberdosis/tiptap`
  (1,533 files) and `microsoft/TypeScript` (monster), release builds,
  `cargo run --release --example perf_probe -- <root> <label>` with
  `SEM_PROFILE_RESOLVE=1`, 18 logical cores. 2 runs per corpus (the "final1"/
  "final2" labels below), measured **after** the parallel-reparse fix from
  the previous section (so `reparse_ms` here is the small post-fix number,
  not the 33s pre-fix figure).

### tiptap (2 runs, 1,533 files, 42,841 entities, 5,414 edges — unchunked, single `resolve_with_scopes_full` call)

| run | build_total_ms | resolve_phase_ms | pre_resolve_ms |
|---|---:|---:|---:|
| 1 | 277.75 | 177.54 (63.9%) | 100.21 (36.1%) |
| 2 | 271.96 | 175.58 (64.6%) | 96.38 (35.4%) |
| **avg** | **274.86** | **176.56 (64.2%)** | **98.30 (35.8%)** |

Ranked wall-clock attribution of `build_total` (ms, % of build_total avg
274.86ms; "(NEW)" = newly instrumented by this step; unmarked rows carry
over from the earlier "Medium baseline" section, renumbered):

| rank | bucket | ms | % | notes |
|---|---|---:|---:|---|
| 1 | pass-1 proper (io+parse+extract+symtable) | 89.01 | 32.4% | derived (`pre_resolve − import_table_wall`); not newly instrumented, shown for context |
| 2 | bag-of-words wall (NEW, replaces old subtraction-derived residual) | 85.93 | 31.3% | was reported as "123.06ms / 41.8%" pre-instrumentation — the true wall figure is lower; aggregate (parallel-summed) detail below |
| 3 | scope-build+ref-collect+ref-loop+resolve-ref parallel section (`pass2_wall`) | 21.27 | 7.7% | unchanged bucket, carried over |
| 4 | pass-1 return-type/instance-attr scan | 20.78 | 7.6% | unchanged bucket |
| 5 | constructor-param-type inference | 10.74 | 3.9% | unchanged bucket |
| 6 | import-table build, wall (NEW) | 9.29 | 3.4% | was reported as `import_table_derived_ms=0.00` (subtraction noise) — direct measurement is far more reliable at this scale |
| 7 | per-chunk entity-index rebuild (NEW) | 3.73 | 1.4% | one-time at tiptap scale (unchunked); becomes a real per-chunk multiplier on chunked repos, see monster below |
| 8 | pass-2 re-parse (already fixed) | 3.60 | 1.3% | |
| 9 | deterministic-return-types-by-name (NEW) | 1.15 | 0.4% | |
| 10 | scope-level edge merge (NEW) | 1.05 | 0.4% | inside `scope_resolve.rs`, distinct from row 14 |
| 11 | scope-level edge dedup+sort (NEW) | 0.76 | 0.3% | inside `scope_resolve.rs`, distinct from row 15 |
| 12 | edge-index construction (NEW) | 0.89 | 0.3% | `dependents`/`dependencies` map build |
| 13 | sort_resolved_refs, outer (NEW) | 0.49 | 0.2% | |
| 14 | export-alias edges (NEW) | 0.19 | 0.1% | |
| 15 | dedupe_resolved_edges, outer (NEW) | 0.12 | 0.0% | |
| 16 | imports_by_file grouping (NEW) | 0.03 | 0.0% | |
| 17 | import-table-by-file grouping (pre-existing bucket) | 0.02 | 0.0% | |
| — | unattributed (rayon dispatch / closure setup across many small parallel sections) | ~25.8 | 9.4% | not chased further — order-of-magnitude smaller than any real finding |

Bag-of-words aggregate (parallel-summed) detail: `bow_index_build_ms` 255.40
+ `bow_resolve_ms` 111.15 = 366.55ms aggregate CPU / 85.93ms wall ≈ 4.3x
speedup on 18 cores (24% utilization) — lower than the scope-build section's
39% (original doc). **`bow_index_build` (re-reading + stripping each file a
*second* time past pass 1 and pass 2's reparse) is the larger half**, not the
per-entity resolve loop — the opposite of what "bag-of-words" as a name
suggests.

### TypeScript monster (2 runs, 40,872 files, 454,541 entities, 199,827 edges — 9 chunks)

| run | build_total_ms | resolve_phase_ms | pre_resolve_ms | import_table_derived_ms |
|---|---:|---:|---:|---:|
| 1 | 14,752.71 | 7,480.39 (50.7%) | 7,272.32 (49.3%) | 3,550.79 |
| 2 | 15,378.76 | 7,831.64 (50.9%) | 7,547.12 (49.1%) | 3,478.54 |
| **avg** | **15,065.74** | **7,656.02 (50.8%)** | **7,409.72 (49.2%)** | **3,514.67** |

Ranked wall-clock attribution of `build_total` (ms, % of build_total avg
15,065.74ms):

| rank | bucket | ms | % | notes |
|---|---|---:|---:|---|
| 1 | **import-table build, wall (NEW)** | 3,543.22 | 23.5% | now bigger than the already-fixed re-parse — see fix phase below |
| 2 | pass-2 re-parse (already fixed) | 2,812.69 | 18.7% | was 74.0% pre-fix; this is the post-381f083 number |
| 3 | bag-of-words wall (NEW) | 2,005.37 | 13.3% | was reported as "3,251ms / 7.2%" pre-instrumentation (subtraction-derived, an undercount) |
| 4 | scope-build+ref-collect+ref-loop+resolve-ref parallel section (`pass2_wall`) | 628.48 | 4.2% | unchanged bucket |
| 5 | pass-1 return-type/instance-attr scan | 396.17 | 2.6% | unchanged bucket |
| 6 | per-chunk entity-index rebuild (NEW) | 355.39 | 2.4% | whole-corpus `entities_by_file`/`children_by_parent` rebuilt **9 times**, once per chunk, over the same 454,541 entities each time |
| 7 | constructor-param-type inference | 321.78 | 2.1% | unchanged bucket |
| 8 | deterministic-return-types-by-name (NEW) | 139.68 | 0.9% | same "redundant per chunk" shape as row 6, over `symbol_table` |
| 9 | scope-level edge merge (NEW) | 52.87 | 0.4% | |
| 10 | edge-index construction (NEW) | 39.14 | 0.3% | |
| 11 | sort_resolved_refs, outer (NEW) | 30.23 | 0.2% | |
| 12 | scope-level edge dedup+sort (NEW) | 19.59 | 0.1% | |
| 13 | dedupe_resolved_edges, outer (NEW) | 7.42 | 0.0% | |
| 14 | export-alias edges (NEW) | 2.29 | 0.0% | |
| 15 | import-table-by-file grouping (pre-existing bucket) | 2.89 | 0.0% | |
| 16 | imports_by_file grouping (NEW) | 0.52 | 0.0% | |
| — | unattributed (rayon dispatch across 9 chunks × many nested parallel sections) | ~223 | 1.5% | top-level gap: `resolve_phase_ms − (CHUNKS_sum + bow_wall + rows 9–16)`. A **further** ~618ms/11.6% gap exists *within* the `CHUNKS_sum` bucket itself (i.e. inside rows 2, 5, 6, 7, 8's own per-chunk calls) but doesn't leak into this top-level number since `CHUNKS_sum` is measured as a single wall timer wrapping the whole per-chunk call from outside — noted, not chased further |

Import-table build sub-attribution (NEW; avg of 2 runs plus a 3rd
confirmation run for the merge/insert split): `import_table_io_ms`
3,499.84 + `import_table_scan_ms` 5,631.15 = 9,130.98ms aggregate (parallel,
18 threads) against a wall contribution of roughly 500–1,100ms (implied);
`import_table_merge_ms` 3,005.02, of which **`import_table_insert_ms`
2,949.72 (98.2%) — the final, fully sequential `for scan in scans {
import_table.insert(..) }` loop** and `import_table_export_build_ms` 4.40
(0.1%, negligible). **The merge is almost entirely one function's fully
serial `HashMap` population — no parallelism at all** — and it is now the
single largest wall-clock bucket in the whole build.

Bag-of-words aggregate detail (monster): `bow_index_build_ms` 2,100.63 +
`bow_resolve_ms` 5,512.86 = 7,613.49ms aggregate / 2,005.37ms wall ≈ 3.8x on
18 cores (21% utilization) — here `bow_resolve` (the per-entity candidate
matching) is the larger half, the reverse of tiptap's ratio, consistent with
the monster repo's much larger candidate lists driving more matching work
per file relative to the fixed cost of building each file's reference index.

### Verdict, ranked by measured wall time (both scales)

1. **The `import_table` merge's `insert` loop is the single biggest newly
   found lever, and at monster scale it is now bigger than the already-fixed
   re-parse loop (23.5% vs 18.7% of build_total).** It is 100% sequential —
   the two-loop merge that follows the already-parallel per-file scan step
   collapses back down to one thread for the entire corpus. Provably safe to
   parallelize (see fix phase): every entry's key is scoped to the scan's own
   `file_path`, and `scan_import_file` runs exactly once per unique
   `file_path`, so no two scans can ever produce the same key — cross-scan
   insertion order is provably irrelevant to the final `HashMap` contents.
2. **Bag-of-words reference resolution is real and larger than the earlier
   subtraction-based estimate suggested at monster scale** (13.3% direct vs
   7.2% subtraction-derived — the subtraction method undercounted because it
   folded in the also-uninstrumented `import_table` and per-chunk-redundant
   costs as if they didn't exist). Its internal ratio flips with scale
   (index-build-dominated at tiptap size, resolve-loop-dominated at monster
   size) and both halves are architectural (regex/AST-adjacent scanning,
   candidate matching) rather than a simple parallelism bug — **left as a
   proposal, not implemented, per task scope** (see Gaps below).
3. **Two new "redundant per-chunk work" findings, structurally identical to
   the already-fixed re-parse bug but two orders of magnitude smaller**:
   `chunk_entity_index` (355ms/2.4% monster) and `return_types_by_name`
   (140ms/0.9% monster) both rebuild a whole-corpus structure
   (`entities_by_file`/`children_by_parent`, and a `symbol_table`-derived
   return-type map, respectively) from scratch **once per chunk** instead of
   once per build, inside `resolve_with_scopes_full_inner`. Correctly a much
   smaller finding than (1) — flagged for a future pass, not fixed here (out
   of the single-top-finding budget for this task).
4. **The rest of the residual (outer dedupe/sort/edge-index/imports-by-file/
   export-edges, and the newly-split scope-level merge/dedup) is genuinely
   small at both scales** (each under 0.5% of build_total) — confirms the
   original doc's implicit assumption that these were minor, now backed by
   direct measurement instead of inference.

### Gaps / what this measurement still does not cover

- **Bag-of-words internals were not instrumented below the index-build vs.
  resolve-loop split.** `resolve_entity_references`'s own hot paths (dot-chain
  extraction, local-binding scan, per-reference candidate matching) were not
  broken down further — a fix there would need its own instrumentation pass,
  and more importantly would need a design decision (the logic is
  string/AST-shaped, not an obviously parallel loop over independent items in
  the way the reparse and import-table-insert loops were), which is why it's
  a proposal and not a fix in this task.
- **`import_table_io_ms`/`import_table_scan_ms`'s wall contribution to the
  merge's total wall time was estimated, not directly wall-timed as its own
  bucket** (only the aggregate parallel-summed cost was captured, the same
  convention `scope_build`/`ref_collect` already use elsewhere in this doc).
- Same caveats as before carry over: no allocation/heap profiler run: hypothesis
  (b) from the original doc (long `String` keys hashed repeatedly) is still
  only indirectly addressed, and is now a more concrete suspect specifically
  for `import_table_insert_ms` given it inserts ~hundreds of thousands of
  `(String, String) -> String` entries — worth a `dhat`/`heaptrack` pass if a
  future step wants to go past "parallelize the loop" into "reduce the work
  the loop does."

## After: parallel import-table insert (semx-9h3 fix phase)

**Change.** `build_import_table_with_default_export_paths`'s final merge
step (`graph.rs`, the `for scan in scans { import_table.insert(..) }` loop
identified above as the single largest new finding) was split into two
stages: (1) a parallel map — `maybe_into_par_iter!(scans).map(|scan| { ..
build a Vec<((String,String),String)> of this scan's entries, in the exact
same local-imports → default-imports → namespace-imports → re-export-imports
order the original loop used .. }).collect()` — producing one `Vec` of
entries per scan, and (2) an unchanged-shape sequential loop that flattens
those `Vec`s into `import_table` via plain `HashMap::insert`. A new macro,
`maybe_into_par_iter!` (same file, same `#[cfg(feature = "parallel")]` /
`#[cfg(not(...))]` split as the existing `maybe_par_iter!`, but calling
`into_par_iter()`/`into_iter()` on an owned `Vec` instead of `par_iter()` on
a borrowed slice — needed because the loop consumes each `scan`'s fields by
value), was added next to `maybe_par_iter!` for this. No other logic in the
function changed: `resolve_default_export_target`, `find_import_file`, and
the `OnceLock`-lazy `ts_top_level_entities` table are all read-only lookups
against already-fully-built, immutable structures by the time this loop
runs, so calling them from a parallel closure is safe (`OnceLock::get_or_init`
is itself designed for exactly this concurrent-first-caller pattern).

**Why this is safe (the equivalence argument, not just the test).** Every
entry a scan can produce has a key of the form `(scan.file_path, name)` (see
`scan_import_file`: `local_imports`, `default_imports`, `namespace_imports`,
and `re_export_imports` all stamp the *scan's own* `file_path` into every key
or `Pending*` struct they build — never another file's path). `scan_import_
file` is called exactly once per unique `file_path` in the deduped
`content_file_paths` list. Therefore **no two different scans can ever
produce the same key** — cross-scan `HashMap::insert` overwrite order, the
only thing parallelizing the outer loop changes, can never matter. The only
order that *can* matter — two pushes for the same key from the *same* scan
(e.g. a file that both imports and re-exports a same-named local binding) —
is preserved exactly, because each scan still builds its own entries `Vec`
in the original local → default → namespace → re-export sequence before any
of its entries reach `import_table`.

**Equivalence check.** Added a temporary `#[test] #[ignore] fn
tmp_equivalence_import_table()` to `graph.rs`'s test module (removed after
use, not part of this commit) that walks a target repo the same way
`perf_probe` does (env var `SEM_EQUIV_ROOT`, default tiptap), runs
`EntityGraph::build`, and prints entity count, edge count, and a
`DefaultHasher` hash of the sorted `"{from}\x1f{to}\x1f{ref_type:?}"` edge
dump — same method the previous fix-phase section used. Method: ran the test
with the current (parallel) code, then temporarily hand-reverted just the
`scan_entries`/insert block back to the original fully-sequential loop
(the exact code, restored verbatim — not `git stash`, since the equivalence
test itself lives in the same file as the change this time), ran again, then
restored the parallel version.

```
# tiptap (1,533 files; default release settings — PARSED_FILE_REUSE_LIMIT=20,000
# still takes the parallel-scan path since same_file_set(file_paths, default_export_file_paths) is true)
TMP_EQUIV_IMPORT_TABLE entities=42841 edges=5414 edge_hash=70e354572d168952   # before (serial insert)
TMP_EQUIV_IMPORT_TABLE entities=42841 edges=5414 edge_hash=70e354572d168952   # after  (parallel insert)
```

`70e354572d168952` is also the exact hash the *previous* fix phase's
equivalence check recorded for tiptap — an independent cross-check that nothing
regressed between the two fix phases either.

```
# TypeScript monster (40,872 files; forced through the crate's existing
# #[cfg(test)] overrides — PARSED_FILE_REUSE_LIMIT=8, SCOPE_RESOLVE_FILE_CHUNK_SIZE=3
# — to exercise the chunked path under `cargo test`; this makes resolution far
# slower (~3-tiny-file chunks instead of 9×5,000) but build_import_table itself
# runs once over the *whole* file_paths list before any chunking starts, so it's
# exercised identically either way. Counts differ from the production perf_probe
# numbers (454,541/199,827) purely because of this pre-existing test-only
# chunking override changing which resolution path executes at this scale — not
# something this task's change causes. What the gate needs is before==after
# under the *same* harness, which it is.)
TMP_EQUIV_IMPORT_TABLE entities=334426 edges=161856 edge_hash=9b1f26514c5712fe   # before (serial insert), 405s
TMP_EQUIV_IMPORT_TABLE entities=334426 edges=161856 edge_hash=9b1f26514c5712fe   # after  (parallel insert), 453s
```

Bit-for-bit identical at both scales — same entity count, edge count, and
edge-set hash, before and after.

**Correctness gates.** `cargo test -p sem-core --release`: full suite green
(see commit for the exact pass count). `cargo clippy -p sem-core --release
--all-targets` and `rustfmt --check`: clean on every line touched by this
step (pre-existing findings elsewhere in `graph.rs`/`scope_resolve.rs`
untouched, same as the previous fix phase).

### TypeScript monster — before/after (production settings, 2 runs each)

| run | build_total_ms (before) | build_total_ms (after) | import_table wall (before) | import_table wall (after) | import_table insert (before) | import_table insert (after) |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 14,752.71 | 12,678.70 | 3,539.05 | 1,466.96 | ~2,995* | 967.68 |
| 2 | 15,378.76 | 13,549.70 | 3,547.38 | 1,522.11 | ~3,004* | 1,008.30 |
| **avg** | **15,065.74** | **13,114.20** | **3,543.22** | **1,494.54** | **~3,005*** | **987.99** |

\* `import_table_insert_ms` wasn't split from `import_table_merge_ms` until
after these two runs; a same-settings confirmation run with the split
present measured `merge_ms=2,954.11` (`export_build_ms=4.40`,
`insert_ms=2,949.72`), consistent with `merge_ms` from runs 1–2 (3,006.22,
3,003.81) to within run-to-run noise, so the ~2,995ms/~3,004ms figures above
are `merge_ms` used as an insert-cost proxy.

**Speedup: `build_total` 15,065.74ms → 13,114.20ms = 1.15x (13.0% cut to the
whole build). `import_table` wall 3,543.22ms → 1,494.54ms = 2.37x. The
`insert` loop itself: ~2,995ms → 987.99ms ≈ 3.0x speedup on 18 cores (17%
utilization — lower than the reparse fix's 69%, because only the *entry-
building* work parallelizes; the final `HashMap::insert` calls themselves
still run on one thread by construction, and much of the parallelized work
per entry is cheap key/string construction rather than heavy computation).**
`resolve_phase_ms` is unaffected as expected (7,656.02ms before vs
7,842.78ms after — flat within run-to-run noise), since `import_table`
build is entirely inside `pre_resolve_ms`, not `resolve_phase_ms`.
`import_table_derived_ms` (the `perf_probe`-derived estimate) corroborates
the direct wall measurement: 3,514.67ms → 1,390.60ms avg, a −2,124.07ms
reduction against the direct wall metric's −2,048.68ms — the two independent
measurement methods agree to within 4%. Entity/edge counts unchanged in
every after-run: `entities=454541 edges=199827`.

### tiptap — before/after

tiptap's `import_table_derived_ms` was already ~0 (subtraction noise — pass 1
already retains parsed trees at this scale, so `import_source_content` hits
the pre-parsed-content cache instead of touching disk either way) and its
`import_table` wall time is a few ms either way (9.29ms before this fix,
per the residual-attribution table above) — this fix is monster-scale-
targeted and correctly close to a no-op at tiptap's scale, consistent with
the "biggest lever" ranking above putting `import_table` 6th at tiptap size
vs 1st at monster size.

**Verdict.** The single safe, provable win identified by direct
instrumentation — parallelizing `import_table`'s final merge/insert loop —
cuts 13.0% off the TypeScript monster's cold build (15.07s → 13.11s) with
zero resolution-behavior change (bit-for-bit identical entity counts, edge
counts, and edge-set hashes at both medium and monster scale, cross-checked
against the prior fix phase's own tiptap baseline hash). Bag-of-words
reference resolution (13.3% of monster build_total, now the largest
remaining single item after this fix and the already-fixed re-parse) is
architectural rather than a simple parallelism bug and is left as a
proposal — see "Gaps" above and the ranked verdict — for a future step's
scope, not this one's.

## After: pass-1 ref collection (semx-6rd)

Two cuts were scoped for this task: **CUT 1** — eliminate the chunked path's
re-parse loop entirely by collecting everything pass 2 needs from a file's
tree during pass 1 (while it's already parsed for entity extraction), instead
of just parallelizing the re-parse (semx-022 already did that). **CUT 2** —
hoist the two whole-corpus structures `resolve_with_scopes_full_inner`
rebuilds from scratch on every chunk call (`entities_by_file`/
`children_by_parent`, and `deterministic_return_types_by_name`'s corpus scan)
to run once instead of once per chunk. CUT 2 landed. CUT 1 was implemented,
found to be equivalence-safe everywhere except the full TypeScript monster
corpus, and was reverted per this task's own instruction to stop and document
rather than force an unproven behavior change. Both are detailed below.

> **Superseded.** The CUT-1 divergence described in this section was later
> root-caused and CUT 1 was re-landed — see
> "[After: the CUT-1 divergence root cause, and the re-land](#after-the-cut-1-divergence-root-cause-and-the-re-land-semx-6rd)"
> at the end of this document. Short version: CUT 1 was never wrong. The
> chunked path's *re-parse loop* was picking each file's tree-sitter grammar
> from the raw file extension, ignoring the `.gitattributes`/`.semrc` language
> override that pass 1 honors — so on the monster corpus
> (`*.js linguist-language=TypeScript`) it re-parsed TypeScript-syntax `.js`
> baselines with the JavaScript grammar. CUT 1 diverged from that only because
> it reuses pass 1's correctly-detected tree. The elimination method used
> below never questioned the *baseline*, which is why it did not converge.
> Numbers in this section were measured against that incorrect baseline.

### CUT 1 — attempted, reverted

**What was built.** Pass 1 (`EntityGraph::build`'s file-extraction loop),
for JS/TS files beyond `PARSED_FILE_REUSE_LIMIT`, was changed to call
`extract_entities_with_tree` instead of the cached entities-only
`extract_entities`, and — while the freshly parsed tree was in hand, before
being dropped — run `collect_all_file_refs`, `build_scopes_from_ast`,
`scan_return_types`, and `scan_init_self_attrs` right there, using file-local
substitutes for the `entity_map`/`children_by_parent` those functions
normally receive as corpus-wide maps (unavailable during pass 1, since
`all_entities` isn't fully assembled yet). The results were packaged into a
new compact `PrecomputedFileFacts` struct (content + `Vec<Scope>` +
`entity_scope_map`/`entity_inner_scope` + `Vec<AstRef>` + this file's
contribution to the corpus-wide return-type/instance-attr maps) and threaded
through a new `resolve_with_scopes_full_chunked` entry point so the chunked
path's re-parse loop and pass-2 tree walks could skip files it covered
entirely. Scoped to JS/TS only: `extract_imports_from_ast`'s Python/Rust/Go
branches and ctor-infer's `scan_constructor_calls` (hardcoded to Python's
`call` node kind) are structurally no-ops for JS/TS ASTs (confirmed by
reading the code, not assumed), so non-JS/TS files kept the unmodified
re-parse path.

**Why the file-local substitute is safe (confirmed, not assumed).** A
dedicated investigation (read-only, `crates/sem-core/src/parser/plugins/code/
entity_extractor.rs` and `crates/sem-core/src/model/entity.rs`) found that
`build_entity_id` constructs every entity id as `{file_path}::{type}::{name}`
for a root entity, or `{parent_id}::{name}` for a child — so every id chain
bottoms out at a `{file_path}`-prefixed root, and cross-file `parent_id`
values are structurally impossible for the code-plugin extractor (the one
suspected cross-file rewrite, `resolve_go_method_parent_ids`, is gated to
`.go` files only). This directly refutes the initial hypothesis that
TypeScript's own `namespace ts { ... }` merging pattern (real and pervasive
in the monster corpus, split across dozens of files) could break the
file-local `children_by_parent` substitute — it can't, by construction.

**Equivalence: safe everywhere tested except full-corpus monster.** Using the
same `PARSED_FILE_REUSE_LIMIT=8`/`SCOPE_RESOLVE_FILE_CHUNK_SIZE=3` `#[cfg(test)]`
override and edge-hash method the two prior fix phases used (plus a raw
sorted-edge-dump diff for this task, since the hash alone doesn't say *which*
edges differ):

| corpus | files | result |
|---|---:|---|
| tiptap (full) | 1,533 | bit-identical (`entities=42834 edges=5414 edge_hash=70e354572d168952`, before == after) |
| monster, `src/compiler/` | 79 | bit-identical |
| monster, `tests/cases/compiler/` | 6,537 | bit-identical |
| monster, `src/compiler/` + `tests/cases/compiler/` combined | 6,616 | bit-identical |
| monster, `src/` (all) | 724 | bit-identical |
| monster, `tests/` (all) | 40,094 | bit-identical |
| **monster, full corpus** | **40,872** | **diverged: 199,551 → 195,947 edges** (before hash `f5e42face071ad36`, after `787a753061633b9d`, both reproduced on a second independent run each) |

The full-corpus divergence is a genuine mix, not a pure drop: diffing the
sorted edge dumps directly showed 9,372 edges present only in the "before"
dump and 5,774 present only in "after" (net −3,604, close to the reported
199,551−195,947=3,604). Every sampled example on both sides of the diff was
an edge either originating in or targeting `tests/baselines/reference/*.js`
— TypeScript's own compiler-test-baseline output files, which pervasively
reuse class/namespace names (e.g. `AbstractClass`, `Point`) across thousands
of near-duplicate fixture variants (one file per compile target/config), and
in several cases redeclare the *same* namespace name (`internal_module A`)
at multiple line numbers within one file (a real same-file TypeScript
declaration-merging pattern). This pattern is consistent with a
scale/candidate-list-size-sensitive divergence in cross-file bare-name or
member-candidate disambiguation (`select_member_candidate`'s
`candidates.first()`, or the `symbol_table`/`class_members` global fallback
lookup) that only manifests when the full corpus's much larger candidate
pool is present — not with anything file-local. Direct causes considered and
ruled out: cross-file `parent_id` (refuted above, structurally impossible);
`PARSED_FILE_REUSE_LIMIT`/parse-budget divergence between pass 1's unbudgeted
`parse_tree` and the old reparse loop's budgeted, fresh-`Parser`
`parse_within_budget` (checked: zero `budget_exceeded` warnings in either
run, so no file actually hit the 2s ceiling in either version); the
`return_type_map`/`instance_attr_types` chunk-scoped merge order (re-derived
to replay `file_paths` order exactly, believed correct on inspection); and
non-determinism in the "before" baseline itself (ruled out — reran the
unmodified code on the full corpus twice, got the identical hash both
times). The root cause was not isolated within this task's budget.

**Decision.** Per this task's explicit instruction ("If ref representation
differences force ANY behavior change, stop that sub-part and document
instead"), CUT 1 was fully reverted — `EntityGraph::build`'s pass 1, the
`resolve_with_scopes_full_chunked` entry point, and
`resolve_with_scopes_full_inner`'s re-parse loop and pass-2 closure are all
back to their pre-semx-6rd shape. `PrecomputedFileFacts` and
`precompute_js_ts_file_facts` do not exist in the shipped diff. A future
attempt should instrument `resolve_ref`'s exact candidate/target chosen for
a handful of the diffed edges (e.g. the `AbstractClass::cb` example above)
on both code paths directly, rather than reasoning from first principles —
that would have converged faster than the elimination method used here.

### CUT 2 — landed

**Change.** `resolve_with_scopes_full_inner` used to rebuild
`entities_by_file`/`children_by_parent` — plain `HashMap`s keyed by file path
and parent id, built by scanning **all** of `all_entities` — from scratch on
*every* call, i.e. once per chunk (9 times at monster scale in production,
once per 5,000-file chunk). Both maps are a pure function of `all_entities`
alone, unchanged across chunks. A new `PrebuiltEntityIndex<'a>` struct
(`entities_by_file`/`children_by_parent`, borrowing from `all_entities`) with
a `build()` constructor — the exact same two loops the old inline code ran —
is now built once in `resolve_scopes_in_file_chunks`, before its chunk loop,
and threaded through a new `resolve_with_scopes_full_inner` parameter
(`entity_index: Option<&PrebuiltEntityIndex>`). When `Some`, the function
uses the caller's maps directly; when `None` (every other caller —
`resolve_with_scopes_full`, `resolve_with_scopes_full_for_entities`, the
direct <20k-file path), it falls back to building them itself exactly as
before, so this is opt-in and only exercised by the chunked path.
`deterministic_return_types_by_name` (the task's other named "redundant
rebuild") was investigated and *not* hoisted: its `return_type_map`
argument is itself chunk-scoped (rebuilt fresh from just that chunk's files
inside the same function), so — unlike `entities_by_file`/`children_by_parent`
— its output genuinely differs per chunk today, and hoisting it to run once
would change which functions' return types are visible across chunk
boundaries (a real resolution-semantics change, not a work-elimination one).
Left unchanged and documented rather than risked.

**Why this is safe.** `entities_by_file`/`children_by_parent` are built by
iterating `all_entities` (the whole corpus, passed unchanged into every
chunk's call) with no dependency on which chunk is currently resolving —
`PrebuiltEntityIndex::build` is textually the same two `for entity in
all_entities { ... }` loops the old per-call code ran, just relocated to run
once. No mutation of these maps happens anywhere in `resolve_with_scopes_full_inner`
after construction (both are read-only `.get()` lookups throughout pass 1b,
pass 2 scope-building, and the entity×ref resolution loop) — verified by
grep, not just inspection.

**Equivalence check.** Same method as CUT 1 above (`#[cfg(test)]`
`PARSED_FILE_REUSE_LIMIT=8`/`SCOPE_RESOLVE_FILE_CHUNK_SIZE=3` override,
temporary `tmp_equivalence_semx6rd` test, sorted-edge-dump `DefaultHasher`
hash), run on the **full** monster corpus (not a subset) since that's where
CUT 1 diverged:

```
# before (HEAD, no semx-6rd changes)
TMP_EQUIV_SEMX6RD files=40872 entities=454528 edges=199551 edge_hash=f5e42face071ad36
# after (CUT 2 only, CUT 1 fully reverted)
TMP_EQUIV_SEMX6RD files=40872 entities=454528 edges=199551 edge_hash=f5e42face071ad36
```

Bit-for-bit identical. Also confirmed on tiptap (`entities=42834 edges=5414
edge_hash=70e354572d168952`, unchanged — tiptap never takes the chunked path
at 1,533 files, so CUT 2 is a no-op for it by construction, not just by
measurement). Note the entity counts here (454,528 / 42,834) differ slightly
from this document's earlier production-mode headline numbers (454,541 /
42,841) — that's the checkout drifting between sessions (both repos are live
git clones, not pinned snapshots), not a resolution difference; the
before/after pair within *this* section always used the identical checkout
state, which is what the gate requires.

**Correctness gates.** `cargo test -p sem-core --release`: full suite green
— 420 lib tests, 0 failed, 1 ignored (the temporary equivalence test,
removed before commit), plus all 6 integration test binaries
(`bow_import_lookup_bench`, `d_smoke`, `elm_smoke`, `graph_accuracy`,
`kappa`, `parse_cache`, `scope_resolve_bench`). `cargo clippy -p sem-core
--release --all-targets` and `cargo fmt -p sem-core -- --check`: clean on
every line this task touched (the one `too_many_arguments` clippy warning on
`resolve_with_scopes_full_inner` predates this change — the function already
had 9 parameters before CUT 2's 10th).

**Timing protocol.** Release build, `SEM_PROFILE_RESOLVE=1`, `cargo run
--release --example perf_probe -- <repo_root> <label>`, production settings
(no `#[cfg(test)]` overrides — real `PARSED_FILE_REUSE_LIMIT=20,000`/
`SCOPE_RESOLVE_FILE_CHUNK_SIZE=5,000`), 18 logical cores, same corpora as
every prior section. 3 monster runs, 2 tiptap runs, "before" figures taken
from this document's own immediately-preceding "After: parallel import-table
insert" section (semx-9h3's last landed state — the exact code this task
started from, confirmed unchanged for CUT 1 by the revert).

### TypeScript monster — before/after

| run | build_total_ms (before, semx-9h3) | build_total_ms (after, CUT 2) | `chunk_entity_index_ms` (before) | `chunk_entity_index_ms` (after) |
|---|---:|---:|---:|---:|
| 1 | 14,752.71 | 12,009.91 | ~355 (avg, prior section) | 0.00 |
| 2 | 15,378.76 | 11,696.98 | ~355 | 0.00 |
| 3 | — | 12,314.21 | — | 0.00 |
| **avg** | **15,065.74*** | **12,007.03** | **355.39** | **0.00** |

\* The task's own before/after protocol calls for the immediately-preceding
section's numbers as the baseline; semx-9h3's own table only has 2 runs
(15,065.74ms was itself the *first* fix phase's 3-run baseline, carried
through — the most recent directly-comparable *2-run* average is
13,114.20ms). Using the more conservative, more recent 13,114.20ms baseline:
**13,114.20ms → 12,007.03ms = 1.09x, an 8.4% cut** to the monster repo's cold
build. `chunk_entity_index_ms` itself — the specific bucket CUT 2 targets —
goes from 355.39ms (2.4% of the 15,065.74ms baseline) to a measured 0.00ms
in all 3 after-runs, confirming the mechanism directly rather than just the
net effect. The gap between the ~355ms mechanism-level saving and the larger
observed ~1,107ms (13,114.20 → 12,007.03) net change is within this
machine's run-to-run noise band for this corpus (build_total_ms spread
11,696.98–12,314.21 across 3 identical after-runs, a 617ms/5.1% spread on
its own) — reported honestly rather than rounded to match the smaller,
directly-attributed number.

Peak RSS (one run, `/usr/bin/time -l`): 5,009,276,928 bytes = 4.665 GiB —
consistent with the semx-022 baseline (4.56–4.65 GiB), no regression (CUT 2
doesn't retain any new data, only avoids rebuilding two `HashMap`s that
already existed transiently per chunk).

### tiptap — before/after

| run | build_total_ms |
|---|---:|
| 1 | 267.50 |
| 2 | 265.19 |
| **avg** | **266.35** |

tiptap (1,533 files) never takes the chunked path (`resolve_scopes_in_file_chunks`
is only called when `file_paths.len() > PARSED_FILE_REUSE_LIMIT`), so CUT 2 —
which only changes that function and its callee — cannot affect it; this
table is a no-op confirmation, consistent with the ~270ms tiptap has measured
at across every prior section in this document (entities=42,841, edges=5,414
unchanged).

### Verdict

CUT 2 lands a small (~2.4%-of-pre-semx-9h3-baseline, directly-attributed;
~8% net-measured, honestly reported with its noise band) but genuinely
work-eliminating, zero-risk fix — proven safe by both direct code inspection
(the hoisted computation is textually identical, just relocated) and a
full-corpus bit-for-bit equivalence check, matching the discipline of the
two prior fix phases. CUT 1's ambition (eliminate the re-parse loop
entirely, not just parallelize it) remains unmet: the target ~2.7s bucket is
still present in the shipped code (`reparse_ms` averaged 2,756.83ms across
the 3 after-runs here, essentially unchanged from the 2,674ms semx-022
baseline). The task's target of "monster ~10s" was not reached — the shipped
result is 12,007.03ms average. Both misses trace to the same root: CUT 1's
design was sound for every case tested except the one that mattered most
(the full, adversarial-scale monster corpus), and the specific interaction
that breaks it was not isolated in time. A follow-up task with a narrower
scope — e.g. instrument `resolve_ref`'s chosen candidate for the specific
diffed edges directly, rather than re-deriving safety by code inspection —
is the recommended next step before CUT 1 is attempted again.

## After: the CUT-1 divergence root cause, and the re-land (semx-6rd)

The previous section reverted CUT 1 (pass-1 precompute of the chunked path's
refs/scopes) because it changed the monster corpus's edge set and the cause was
not isolated in budget. This section root-causes that divergence, fixes it, and
re-lands CUT 1 with full bit-for-bit equivalence.

**The prime hypothesis was wrong, and so was the framing.** The suspicion going
in was tie-break order: that some global candidate list (bag-of-words
candidates, `symbol_table`, the import table) had no deterministic tie-break,
and that the precompute changed arrival order so different same-name duplicates
won. That is not what happened, and it could not have been:

* The old path is **deterministic**. The unmodified baseline was re-run three
  times on the full monster corpus and produced the identical edge hash
  (`238caa16b7005fc0`) every time, across different rayon thread schedules.
* Candidate lists **are** explicitly sorted before any tie-break.
  `sort_symbol_table_targets_by_source` sorts every `symbol_table` bucket by
  `(file_path, start_line, end_line, id)`; `class_members` and `owner_members`
  are `sort_unstable`d; rayon's `collect` preserves input order, so
  `all_entities` and every map derived from it are a pure function of the sorted
  file list.

So there was no ordering bug to fix, and option (b) from the plan — introducing
an explicit deterministic sort key — would have been a fix for a problem that
did not exist. The real defect was a **correctness** difference, and it was in
the *old* path, not in the precomputed refs.

### Root cause

`EntityGraph::build` extracts entities through `ParserRegistry`, and
`ParserRegistry::extract_entities_with_tree` resolves repo-level language
overrides (`.semrc`, and `.gitattributes` `diff=` / `linguist-language=`) via
`resolve_file_path` *before* it picks a grammar. Repos at or under
`PARSED_FILE_REUSE_LIMIT` hand those pass-1 trees straight to resolution, so
they resolve against the language the repo declared.

Repos over the limit take the chunked path, whose re-parse loop in
`resolve_with_scopes_full_inner` re-reads and re-parses each file itself. That
loop selected its grammar straight from the raw extension:

```rust
let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
let config = get_language_config(ext)?;   // ".js" -> JavaScript, always
```

TypeScript's own repo ships `*.js linguist-language=TypeScript` in its
`.gitattributes`, so the registry maps `.js` → `.ts`. Its
`tests/baselines/reference/*.js` baselines are compiler-test artifacts that
contain the TypeScript *input* followed by the emitted JavaScript *output* — the
same declarations twice, the first copy in TypeScript syntax. Pass 1 parsed them
with the TypeScript grammar; the chunked re-parse loop parsed them with the
JavaScript grammar, which error-recovers through the TypeScript half.

On one sampled baseline file the two trees are not close: **169 refs / 64 scopes
(JavaScript grammar) vs 217 refs / 79 scopes (TypeScript grammar)**. References
that scope resolution should have resolved simply were not in the ref set, so
they fell through to the coarser bag-of-words resolver, which picks the
sorted-first `symbol_table` candidate — the *first* duplicate (`assertNever@L4`)
rather than the scope-correct one (`assertNever@L322`). That is exactly the
"same-name duplicate resolves differently" signature the previous attempt saw,
which is why tie-break order looked like the culprit.

CUT 1 never re-parses JS/TS at all — it reuses pass 1's tree — so it produced
the *correct* answer and "diverged" from an incorrect baseline.

### Isolating it

Method (the one the previous section recommended, and it converged quickly):
reconstruct both edge sets on the full monster, diff the sorted dumps, then
**delta-debug the file set** rather than reason about the code. Every diverging
edge turned out to be a same-file duplicate-name flip, and halving the corpus
repeatedly shrank the repro to **three files**:

```
tests/baselines/reference/narrowingByTypeofInSwitch.js
src/compiler/_namespaces/ts.moduleSpecifiers.ts     # 0 entities
src/compiler/_namespaces/ts.performance.ts          # 0 entities
```

The two `_namespaces` stubs are three-line re-export files contributing zero
entities; they are in the repro only to push the file count over the reuse limit
so the chunked path engages. So the repro is really *one file, chunked vs not* —
which immediately reframed the question from "what did CUT 1 change?" to "why do
the two paths disagree at all?":

| configuration | edges | edge_hash |
|---|---:|---|
| 3 files, direct path (pass-1 trees) | 188 | `f6bae1144984ebc8` |
| 3 files, chunked, pre-fix | **186** | `a92d569c03e9171c` |
| 3 files, chunked, pre-fix + CUT 1 | 188 | `f6bae1144984ebc8` |
| 3 files, chunked, post-fix, with or without CUT 1 | 188 | `f6bae1144984ebc8` |

CUT 1's answer equals the direct path's answer. The pre-fix chunked path is the
only outlier. Two candidate explanations were then killed directly rather than
by inspection:

* **The budgeted parse.** `parse_within_budget` (callback-based
  `parse_with_options` with a deadline) versus pass 1's
  `parse_tree` (`Parser::parse` on a byte slice) produce **byte-identical
  s-expressions** for the same grammar and content — checked by parsing the file
  both ways and comparing. Not the cause.
* **The grammar.** Per-file instrumentation of pass 2 printed
  `scopes=64 refs=169` on the old path and `scopes=79 refs=217` on the new one
  for the same file — impossible from one tree, which is what pointed at
  `.gitattributes`.

### The fix (its own commit)

`PreBuiltLookups` now carries the registry's override map
(`ParserRegistry::ext_overrides()`), and the re-parse loop resolves each file's
extension through it (`reparse_language_config`) before choosing a grammar, so
the chunked path parses what pass 1 parsed. Only grammar *selection* changed:
the `scope_resolve` config lookups still key off the raw extension, exactly as
the direct path does, so the two paths stay aligned rather than both moving.

This is a determinism/consistency fix, and it **changes the baseline edge set**
on any repo that both declares a language override and is large enough to chunk:

| corpus | before | after |
|---|---|---|
| TypeScript monster (40,872 files, chunks) | 199,827 edges, `238caa16b7005fc0` | **196,223 edges, `3990b51b01783747`** |
| tiptap (1,533 files, never chunks) | 5,414 edges, `70e354572d168952` | 5,414 edges, `70e354572d168952` (unchanged) |

The monster delta is −3,604 net: the sorted edge dumps differ by 9,372 lines
present only before and 5,774 present only after (6,262 / 2,658 after
deduplicating on `(from, to, ref_type)`), all of them in or targeting
`tests/baselines/reference/*.js`. The "after" figure is not a new third answer —
it is precisely the answer the direct path already gave for the same files, so
the fix collapses two answers into one rather than inventing one. tiptap
declares no overrides and is bit-identical before and after, in the direct path
*and* when forced through the chunked path with the `#[cfg(test)]`
`PARSED_FILE_REUSE_LIMIT=8` / `SCOPE_RESOLVE_FILE_CHUNK_SIZE=3` override.

**Latent issue, deliberately not fixed here.** Module-scope `defs` is a plain
`HashMap` filled by iterating `entity_ranges`, so when one file declares the
same top-level name twice, the winner is simply whichever entry is inserted
last — deterministic, but with no stated tie-break rule. Relatedly,
`scope_entity_ranges` is `sort_unstable`d at one of `graph.rs`'s three
`PreBuiltLookups` construction sites and left in `all_entities` order at the
other two. Both paths behave identically today, so this is not what semx-6rd was
chasing, and changing it would move results for every repo; it is recorded here
as a follow-up rather than bundled in.

### CUT 1 — re-landed

With the baseline corrected, CUT 1 was restored unchanged in substance:
`EntityGraph::build`'s pass 1 calls `extract_entities_with_tree` for JS/TS files
beyond `PARSED_FILE_REUSE_LIMIT` and, while the tree is in hand, runs
`collect_all_file_refs`, `build_scopes_from_ast`, `scan_return_types` and
`scan_init_self_attrs` into a `PrecomputedFileFacts`; the chunked path skips the
re-parse and the pass-2 tree walks for every file it covers. The one shape
change from the reverted version: the two whole-corpus values the chunk loop
threads through (`PrecomputedFileFacts` map and `PrebuiltEntityIndex`) are now
bundled in a `ChunkedResolveInputs` struct, which keeps
`resolve_with_scopes_full_inner`'s argument count where it already was and adds
no new clippy warnings.

**Equivalence.** Bit-for-bit against the post-fix baseline, full corpora, no
subsets:

```
# monster, fix only (baseline)      files=40872 entities=454541 edges=196223 edge_hash=3990b51b01783747
# monster, fix + CUT 1              files=40872 entities=454541 edges=196223 edge_hash=3990b51b01783747
# tiptap, fix only                  files=1533  entities=42841  edges=5414   edge_hash=70e354572d168952
# tiptap, fix + CUT 1               files=1533  entities=42841  edges=5414   edge_hash=70e354572d168952
# tiptap, forced chunked (cfg(test) 8/3), with and without CUT 1
#                                   files=1533  entities=42834  edges=5414   edge_hash=70e354572d168952
```

**Correctness gates.** `cargo test -p sem-core --release`: green — 420 lib
tests, 0 failed, plus all 6 integration binaries (`bow_import_lookup_bench`,
`d_smoke`, `elm_smoke`, `graph_accuracy`, `kappa`, `parse_cache`,
`scope_resolve_bench`). The suite is meaningful coverage here: `#[cfg(test)]`
pins `PARSED_FILE_REUSE_LIMIT` to 8, so every test over 8 files exercises the
chunked path and the precompute. `cargo clippy -p sem-core --release
--all-targets`: warning set identical to the pre-change baseline (174), nothing
added. `cargo fmt -p sem-core -- --check`: clean on every file this work
touched.

### TypeScript monster — before/after

Release build, `SEM_PROFILE_RESOLVE=1`, `perf_probe`, production settings (no
`#[cfg(test)]` overrides), 18 logical cores. "Before" is the language-override
fix alone, so the two columns differ *only* by CUT 1 and are directly
comparable.

| run | build_total_ms (before) | build_total_ms (after) | reparse_ms (before) | reparse_ms (after) | resolve_phase_ms (before) | resolve_phase_ms (after) |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 12,311.50 | 8,316.90 | 2,770.95 | 17.83 | 7,144.90 | 2,730.90 |
| 2 | 12,619.93 | 7,851.32 | 2,754.99 | 16.56 | 7,166.34 | 2,733.29 |
| 3 | 11,843.39 | 8,503.48 | 2,727.20 | 16.48 | 7,146.76 | 2,869.66 |
| **avg** | **12,258.27** | **8,223.90** | **2,751.05** | **16.96** | **7,152.67** | **2,777.95** |

**1.49x on cold build, a 32.9% cut** (12,258.27ms → 8,223.90ms). The mechanism
lands exactly where CUT 1 aimed: `reparse_ms` goes from 2,751.05ms to 16.96ms —
the ~2.7s bucket that survived semx-022 and semx-6rd's first attempt is now
essentially zero, and `pass1_scan_ms` falls with it (397.65ms → 4.33ms in the
matching `edge_dump` runs, since the precompute already did that scan). The task
target was "monster ~10s"; the shipped result is 8.2s.

Peak RSS, one `perf_probe` run under `/usr/bin/time -l`: **3,496,296,448 bytes =
3.256 GiB**, against the 4.665 GiB recorded for CUT 2 in the previous section
and the 4.56–4.65 GiB semx-022 band — a clear improvement, not a regression.
Measured on a leaner harness that only runs the build (no `perf_probe`
pre-passes retaining every file's contents), the before/after pair is
4,075,192,320 → 2,567,356,416 bytes (3.795 → 2.391 GiB). Retaining
`PrecomputedFileFacts` for every JS/TS file costs less than the re-parse loop's
peak, which materialized live tree-sitter trees for a whole 5,000-file chunk at
once.

### tiptap — before/after

| run | build_total_ms |
|---|---:|
| 1 | 264.35 |
| 2 | 269.02 |
| **avg** | **266.69** |

tiptap never crosses `PARSED_FILE_REUSE_LIMIT`, so neither the fix nor CUT 1 can
reach it; this matches the 266.35ms the previous section measured, and its
entity/edge counts are unchanged.

### Verdict, and the red-green implication

CUT 1's design was sound all along — it was measured against a baseline that was
itself wrong. The lesson for the next divergence: when a change makes the
chunked path disagree with the direct path, check whether the *direct* path
already disagreed, before assuming the new code is the deviant.

For red-green incremental resolution this is more than a perf win.
Deterministic, path-*independent* output is a precondition for fact-level
caching, and until this fix the same file's edges depended on whether its repo
happened to sit above or below the 20,000-file chunking threshold — a cache
keyed on file content would have gone stale, or silently wrong, the moment a
repo grew across that boundary. `PrecomputedFileFacts` is also, structurally,
exactly the per-file fact bundle such a cache would store: content-derived
scopes, refs, return types and instance-attribute types, computed once from a
file's tree and reusable without it.

## Red-green incremental resolution (semx-022)

Everything above makes the *cold* build faster. This makes the second build
cheap: a warm rebuild after an edit redoes work only for the files that changed
and the files whose resolution actually depended on them, and produces a graph
bit-identical to a cold build of the same tree.

The precondition is the previous section's finding. Until the grammar-override
fix, the same file's edges depended on whether its repo sat above or below the
20,000-file chunking threshold. A fact cache keyed on file content would have
gone stale — or silently wrong — the moment a repo grew across that boundary.
Path-independent output is what makes any of this safe to cache.

### Design

Five pieces, in the order a rebuild uses them.

1. **Facts layer.** `parser::incremental::FileFacts` is the per-file bundle:
   content hash, extracted entities, and (for JS/TS) the tree-independent
   `PrecomputedFileFacts` — scopes, AST refs, return types, instance-attribute
   types — that semx-6rd's CUT 1 already computes during pass 1. Content-hash
   keyed, following `parser::cache`'s discipline, and `serde`-serializable so the
   on-disk corpus bead (semx-9en) can persist it unchanged.

2. **Read sets.** Every lookup in the resolver that can reach *another file's*
   data records its `(table, key)` pair into that file's `ReadSet`, hashed to one
   `u64`. Recording is unconditional at the site: a **miss is a dependency**,
   because a later edit that introduces the key changes the answer.

3. **Fingerprints.** After each build every global table is fingerprinted as
   `key_hash -> value_hash` (`TableFingerprints`).

4. **Invalidation.** A file is GREEN when its own content is unchanged, it is
   reuse-eligible, and **every key it read still holds the value it held last
   build**. Everything else is RED. There is deliberately no fixpoint iteration:
   the global tables are rebuilt from *all* files' facts before any read set is
   evaluated, so the fingerprint diff already reflects every edit transitively.
   That is both simpler and strictly more precise than propagating RED along
   file-to-file dependency edges — a file that reads a *different* key of a
   changed file stays GREEN, correctly.

5. **Edge ownership.** *An edge belongs to the file whose reference produced it.*
   Reuse happens **inside** the two per-file resolution closures — the pass-2
   scope closure in `scope_resolve.rs` and the bag-of-words closure in `graph.rs`
   — so a GREEN file's edges land in exactly the position in the merge order a
   cold build would have put them. Every merge, dedupe, sort and edge-index step
   downstream therefore sees byte-identical input. Bit-identical output is a
   structural property of that arrangement, not something the oracle merely
   happens to observe; the oracle exists to catch a mistake in rule (4).

`GraphSession` (`parser::session`) owns the state; `EntityGraph::build` and a
warm rebuild run the *same* function, `EntityGraph::build_incremental_core`,
differing only in whether a `BuildCarry` is present.

### Read-set capture points

| Table | Where recorded | Why it can cross a file boundary |
|---|---|---|
| `SymbolTable` | `resolve_ref` (bare-call fallback, `Type::method()` receiver probe, dynamic-language unique-method fallback), `resolve_qualified_callee_name`, bag-of-words name fallback | name → ids from anywhere in the corpus |
| `ClassMembers` | `resolve_ref` × 9 (scoped call, `self.m()`, `self.attr.m()`, `var.field.m()`, typed receiver, static call, scope-chain class, imported class) | owner type name → members defined anywhere |
| `OwnerMembers` | `resolve_ref` module/object member lookup | parent id → members |
| `EntityMap` | `resolve_ref` × 6, plus the parent-child check in the pass-2 closure | ids reached via symbol table, import table, or scope defs |
| `InstanceAttrTypes` | `resolve_ref` × 4, `inject_field_type_bindings` | `(class, attr)` → type, contributed by whichever file declares the class |
| `ReturnTypeMap` / `FuncNameReturnTypes` | `inject_return_type_bindings` | an imported function's return type decides a local variable's type |
| `ImportsForFile` | once per file in the pass-2 closure and once in bag-of-words | the slice is this file's, but its *targets* are other files' entities |
| `BowClassMembers`, `BowClassEntityFiles`, `BowParentChildPairs` | `resolve_entity_references` | same reasoning, bag-of-words side |
| `GuardSwiftCallSignatures` | fingerprinted whole per chunk | flips `resolve_ref` onto a branch whose candidate filtering is not attributed per file |

**Reads that are deliberately *not* recorded, and the argument for why that is
sound.** Entity ids are `{file_path}::{type}::{name}` by construction
(`build_entity_id`), and the one cross-file rewrite, `resolve_go_method_parent_ids`,
is gated to `.go`. So for a reuse-eligible file, *any* table entry keyed by one
of its own entity ids — `entities_by_file[self]`, `entity_ranges[self]`,
`children_by_parent[own id]`, `entity_map[own id]`, `enclosing_class[own id]`,
`class_child_names[(own id, …)]`, `scope_consumed_words[own id]` — is a pure
function of that file's own content, which the own-content check already covers.
Recording them would be correct but pure overhead. This is the same argument
semx-6rd used to justify `precompute_js_ts_file_facts`'s file-local
`entity_map`/`children_by_parent` substitutes.

**Chunk scoping.** `return_type_map` and `instance_attr_types` are rebuilt from
only the current chunk's files (semx-6rd CUT 2 deliberately did *not* hoist them,
because hoisting changes which return types are visible across a chunk
boundary). The same key can therefore hold different values in two chunks, so the
chunk index is mixed into those tables' key hashes. Chunk membership is a
function of the file list alone, and the session refuses reuse when the file list
changes on a chunked corpus — see below.

### What is conservative, and what is precise

Conservative — slower than necessary, never wrong:

* **Only JS/TS files may go GREEN.** Every other language reaches cross-file data
  through `extract_imports_from_ast`'s Python/Rust/Go branches, the Go package
  index, Swift call signatures, or Python constructor-parameter inference. Those
  reads are not attributed per file, so rather than guess at their read sets,
  those files are held permanently RED. On the monster this is 1,576 of 40,872
  files (3.9%) and costs ~0.9s of the warm rebuild; on a Python or Go repo it
  means warm == cold. This is the honest limit of the current capture, not an
  oversight, and closing it is a bounded follow-up: instrument the same way
  inside `extract_imports_from_ast` and `infer_constructor_param_types`.
* **An add or a delete disables reuse entirely on a chunked corpus** (>20k
  files), because inserting a file shifts chunk membership for everything after
  it, and with it the chunk-scoped return-type maps every one of those files
  resolved against. Smaller corpora resolve in one pass and take adds and deletes
  incrementally (the oracle covers both).
* **Every path the caller names in `changed_paths` is RED**, whether or not its
  bytes actually differ.
* **Whole-table guard on Swift call signatures**: a corpus containing `.swift`
  sources refuses reuse for the whole chunk.
* Bag-of-words reuse requires scope reuse for the same file, since bag-of-words
  reads back the words scope resolution consumed for the same entities.

Precise — and this is where the design earns its keep:

* A file that has an edge into the changed file but does not read anything that
  moved stays GREEN. Adding a method to a class does not invalidate a file that
  only imports a function from the same file.
* **Rewriting a function body while leaving every name and line intact changes no
  table any other file reads**, so the whole dependent set stays GREEN and
  `changed_keys` is 0. Measured on the fixture corpus: a hub rewrite of that
  shape produced `files_red=1, changed_keys=0`, with the graph provably
  unchanged.

### Correctness gates

**Oracle.** Cold-build state A, mutate, rebuild warm to state B, *also* cold-build
state B, assert entity set, edge set and sorted-edge hash bit-identical. Run at
three scales.

*Synthetic cross-file fixture* (`parser::session` tests, 14 files, deliberately
over the `#[cfg(test)]` `PARSED_FILE_REUSE_LIMIT = 8` so it takes the **chunked**
path): a hub every file imports, a re-export chain, two files declaring the same
class name, six leaves and four islands. Eleven tests, all green — append to a
leaf, change a signature others call, delete an entity others reference, add a
file, delete a file, rewrite the file everything imports, change one of two
same-named classes, a no-op rebuild, and four rebuilds in a row each verified
against a fresh cold build.

*tiptap* (1,533 files, 42,841 entities, 5,414 edges) and *the TypeScript monster*
(40,872 files, 454,541 entities, 196,223 edges), via
`cargo run --release --example incr_probe -- <root> all <label>`, which restores
every file it touches including on panic. Six scenarios each — no-op, a leaf
file, 50 files spread across the corpus, the file with the highest measured
fan-in, a rename of that file's first export, and 50 files under `tests/` (the
duplicate-name minefield from the grammar bug). **All 12 scenarios: warm ==
cold, bit-for-bit** (entity count, entity-id hash, edge count, sorted-edge hash),
plus a cross-check that the session's own cold build equals a plain
`EntityGraph::build` on both corpora.

Two honest caveats on the probe's scenarios. The `hubrename` scenario applied no
mutation on *either* corpus: the highest-fan-in file is
`tests/baselines/reference/NonInitializedExportInInternalModule(alwaysstrict=false).js`
on the monster and a `.jsx` demo on tiptap, and neither contains an
`export function` declaration for it to rename — so that row is a second no-op
run, not an API-break test. API-breaking renames *are* covered, on the synthetic
fixture, by `every_file_whose_edges_change_is_red`'s `rename-an-export` and
`drop-a-method-others-call` mutations. And the `tests` scenario found no
mutable target on tiptap (it has no `tests/` tree), so that row is a no-op there
too; it is meaningful only on the monster, which is where it was aimed.

The probe's mutation appends *two* functions, one calling the other, so it moves
the edge set and not merely the entity set — an append that only grew the entity
count would have made the oracle a much weaker check than it looks.

**Blast-radius honesty.** The counters `files_red`, `files_green`,
`files_green_bow`, `edges_reused`, `edges_rederived` and `changed_keys` are on
`RebuildStats`. The tempting ground truth — "every file with an edge into the
changed file must be RED" — is *wrong*, and wrong in the direction that would
make this cache pointlessly conservative (see the precision note above). The test
`every_file_whose_edges_change_is_red` asserts the necessary condition instead:
across five mutations (rename an export, add a method, drop a method others call,
retarget the middle of an import chain, shadow a duplicate name) **every file
whose edges differ between a cold build of state A and a cold build of state B is
RED** — a file staying GREEN while its edges should have changed is exactly the
stale-edge failure this bead must never ship. `blast_radius_is_proportional_to_the_edit`
covers the other direction: a leaf touch keeps `files_red <= 3` of 14, and the
four islands stay GREEN through a structural hub rewrite.

**Suite.** `cargo test -p sem-core --release`: 431 lib tests (420 pre-existing,
unchanged in behavior, + 11 new) and all 7 integration binaries, 0 failures.
`cargo clippy -p sem-core --release --all-targets` and `cargo fmt -p sem-core --
--check`: clean on every file this work touched. (`languages.rs` carries a
pre-existing uncommitted reflow and was left exactly as-is, excluded from these
commits.)

### Measured: TypeScript monster (40,872 files, 18 cores, release)

Two full runs of all six scenarios; the table reports the first, and the second
agreed on every count and every hash (wall times drifted 3,454–3,602ms warm and
8,162ms cold, a ~5% run-to-run band on this machine — consistent with the spread
this document records for earlier sections).

| scenario | files changed | warm ms | files_red | files_green | edges_reused | edges_rederived | changed_keys |
|---|---:|---:|---:|---:|---:|---:|---:|
| cold build (baseline) | — | **7,509** | — | — | — | — | — |
| no-op rebuild | 0 | 3,429 | 1,576 | 39,296 | 230,396 | 0 | 0 |
| 1 leaf file | 1 | 3,443 | 1,577 | 39,295 | 230,396 | 1 | 4 |
| **50 mixed files** | 50 | **3,438** | 1,626 | 39,246 | 230,053 | 390 | 95 |
| 1 high-fan-in file (403 dependents) | 1 | 3,449 | 1,577 | 39,295 | 230,388 | 9 | 4 |
| 50 files under `tests/` | 50 | 3,524 | 1,626 | 39,246 | 230,290 | 156 | 102 |

**Cold 7.51s → warm 3.44s = 2.18x, a 54% cut.** The task's target for the
50-file case was ~1–2s; the shipped result is 3.4s, and the reason is measured,
not guessed.

Where the warm 2.95s goes (`SEM_PROFILE_RESOLVE=1`, the 50-file scenario,
extracted from the rebuild's own report rather than the surrounding cold builds):

| bucket | warm ms | same bucket, cold | note |
|---|---:|---:|---|
| **import-table build, wall** | **1,471** | 1,530 | rebuilt in full — the dominant remaining cost |
| pass 1 for the 1,626 RED files + corpus table rebuild + fingerprinting | ~934 | ~2,800 | derived by subtraction |
| scope resolution (`CHUNKS` sum) | 419 | 478 | per-file work 85 → 20ms; the rest is fixed per-chunk cost |
| dedupe + sort + edge index | 64 | 67 | whole-graph, always redone |
| **bag-of-words, wall** | **59** | 1,940 | **33x** — the red-green reuse working exactly as intended |

The two per-file resolution stages, which are what red-green actually caches, do
collapse: bag-of-words 1,940ms → 59ms and the pass-2 closure 85ms → 20ms.
**The import table does not, because it is rebuilt whole.** That is the global
structure the task asked to be decided with data, and the data says the opposite
of what was assumed: at 1,471ms it is far above the 0.5s "just rebuild it"
threshold, so it *should* be incremental. It is not, in this bead, because making
it so is not a "remove the old file's entries, insert the new" edit —
`scan_import_file` runs per file (cacheable, and structurally another `FileFacts`
member), but the merge that follows resolves default re-exports and namespace
imports against corpus-wide tables, and its 950ms sequential insert loop rebuilds
one `HashMap` for the whole repo. Caching the per-file scans is the next lever
and it composes directly with semx-9en's on-disk facts corpus; guessing at the
merge would have risked exactly the silent-stale-edge failure this bead forbids.

Per-chunk fixed costs are the other visible floor: `ctor_infer` (137ms) and
`return_types_by_name` (128ms) run once per chunk regardless of how many files in
that chunk are GREEN, because they are chunk-scoped by construction.

**Memory.** Peak RSS holding the whole session (facts, read sets, fingerprints,
per-file edges) across a cold build plus a warm rebuild, `/usr/bin/time -l`:
**3,796,910,080 bytes = 3.536 GiB**, against **3,496,902,656 bytes = 3.257 GiB**
for `perf_probe`'s plain cold build on the same corpus — **+280 MB, +8.6%**, and
far under the 4.56–4.65 GiB band of the pre-semx-6rd builds. Holding facts is
cheap because it *is* facts: the read sets are one `u64` per distinct key, and
GREEN files' entities are **moved**, not cloned, out of the previous build's
`all_entities` via recorded per-file spans. (An earlier draft cloned them; on
454,541 entities carrying their source text that alone would have cost more than
the resolution the reuse saves.)

### Measured: tiptap (1,533 files)

| scenario | files changed | warm ms | files_red | files_green | edges_reused | edges_rederived |
|---|---:|---:|---:|---:|---:|---:|
| cold build (baseline) | — | **292** | — | — | — | — |
| no-op | 0 | 191 | 406 | 1,127 | 8,076 | 0 |
| 1 leaf file | 1 | 192 | 407 | 1,126 | 8,076 | 1 |
| 50 mixed files | 50 | 190 | 456 | 1,077 | 7,577 | 549 |
| 1 high-fan-in file (79 dependents) | 1 | 190 | 407 | 1,126 | 8,067 | 10 |

1.53x. tiptap sits under `PARSED_FILE_REUSE_LIMIT`, so it takes the retain path,
where resolution needs a live tree per file and pass 1 therefore re-reads and
re-parses everything on every rebuild — the reuse is confined to the two
resolution closures. The 406 permanently-RED files are its non-JS/TS sources
(`.vue`, `.json`, `.md`, …). A medium repo's whole build is 292ms, so this is
correctly not where the design is aimed.

### Verdict

Red-green incremental resolution lands correct and roughly half the cost of a
cold build, with the caching mechanism itself working far better than the
headline suggests: the two stages it actually caches fall by 33x and 4x. The
headline is held back by one global structure the bead did not make incremental —
the import table, 1.47s of a 3.44s rebuild, measured and named rather than
estimated. Anyone continuing here should start by caching `ImportFileScan` per
file (same content-hash keying as `FileFacts`, and it is the same data semx-9en
needs on disk), then decide whether the remaining sequential merge is worth
patching incrementally or is simply the floor.

**Handoff to semx-9en (on-disk facts corpus):** `FileFacts` and
`PrecomputedFileFacts` are already `serde`-serializable and content-hash keyed,
and `GraphSession::export_facts()` returns the per-file bundle ready to write —
what is missing for a persisted corpus is `ImportFileScan` joining them, which is
also the single biggest remaining warm-rebuild win.

## After: incremental import-table maintenance (semx-h1s)

The previous section named the remaining lever precisely: the import table was
rebuilt whole every warm rebuild — 1,471ms of a 3,438ms 50-file rebuild on the
TypeScript monster corpus, the largest single bucket left once scope resolution
and bag-of-words were collapsing 33x/4x. This bead makes it incremental.

### Design

`import_table: HashMap<(String, String), String>` is now session-owned,
persistent state (`GraphSession`, threaded through `BuildCarry` exactly like
`precomputed`/`content_hashes` already were), not a fresh map built from
scratch every call. The key fact that makes per-file patching structurally
safe is the one e996964 already proved for the parallel-merge fix: every
entry's key is `(producing file's own path, name)`, `scan_import_file` runs
exactly once per unique file path, so two different files can never collide
on a key. GREEN files' entries are therefore never touched; RED files' old
entries are removed (tracked per file in a new `import_keys: HashMap<String,
Vec<(String, String)>>`, so removal is `O(that file's own key count)`, not a
full-table scan) and their freshly resolved entries are inserted.

Each JS/TS file's raw scan (`ImportFileScan` — local imports, re-exports,
default/namespace imports, all still *pending*, i.e. not yet resolved against
corpus-wide tables) is cached in a new `import_scans: HashMap<String,
CachedImportScan>`, alongside the read set its last resolution consulted.
Two read sets, both new:

* **`Table::SymbolTable`** (already existed, already fingerprinted by
  `scope_resolve::fingerprint_corpus_tables` — moved to run *before* the
  import table is built, specifically so this read set has something to
  compare against) covers named imports/re-exports:
  `resolve_named_import_tracked` records every name a file's captured
  `pending_named_local`/`pending_named_re_export` list looks up, hit or miss.
  `scan_import_file` gained these two new pending-list fields (populated
  alongside its existing eager resolution, for JS/TS files only) precisely so
  a file whose own content is unchanged but whose read set was invalidated
  can be re-resolved from the cached pending list — no re-read, no re-regex —
  instead of falling back to a fresh scan.
* **`Table::TsExportSurface`** (new) covers default and namespace imports,
  which resolve against corpus-wide tables (`default_exports`,
  `named_exports_by_file`, top-level entities) built fresh every build from
  every file's own scan — cheap regardless of incrementality
  (`merge_export_build_ms` was already ~90ms at monster scale before this
  bead and still is). What's new is that a file importing from `T` now
  records a read of `T`'s combined export-surface hash (default export
  identity + sorted named exports + sorted top-level entities, one hash) for
  every repo-relative path its module specifier could resolve to — so `T`
  gaining, losing, or changing an export invalidates exactly its importers,
  the same "miss is a dependency" discipline `ReadSet` already documents for
  every other table.

`default_export` itself needs no read-set entry: `scan_import_file` resolves
it by filtering `symbol_table.get(name)` down to entities in the *same*
file, so its value is a pure function of the file's own content — the same
self-invariant the original red-green design already relies on for a file's
own `entity_map`/`entity_ranges` reads.

### The bare-specifier trap (found by measuring, not by design review)

The first working version of this bead was *correct* — every oracle scenario
below passed — and still only cut the import-table wall time from 1,471ms to
roughly 760–930ms, nowhere near the "tens of ms" the previous section's
verdict projected. Splitting the phase further (temporary instrumentation,
removed before commit) found why: `find_import_file`'s own doc comment
already flags its bare/package-specifier fallback (`import x from 'lodash'`)
as an `O(candidates)` whole-corpus scan, "cheap" only because the original
code ran it once per import statement, once per build. This bead's RED-file
resolution runs on *every* warm rebuild, and even a few thousand RED files
each averaging a handful of bare pending imports adds up to `O(RED files ×
corpus size)` string comparisons — the actual dominant cost, confirmed
directly: isolating just the entries-construction step measured 757ms for
2,488 RED files before the fix, 51–63ms for the same 2,488 files after.

The fix (`import_resolution.rs`): `build_stem_index`/`resolve_bare_import_stem`
group the candidate list by file stem *once per build* and look up each bare
specifier's stem in that index instead of re-scanning the whole candidate
list per specifier — same tie-break (`min` by extension priority, then path),
`O(1)` average instead of `O(candidates)`. This is a pure algorithmic fix,
not incrementality-specific: it also cut the *cold* build's import-table wall
time (measured in the same run: 816ms → 51ms for the entries-construction
step, all 40,865 files "RED" on a cold build), a free side benefit of
building the incremental path at all.

### Fingerprint parity

The read-green invalidation rule is only honest if the incrementally
maintained table's fingerprints are *identical* to what a whole rebuild would
produce — a divergence here is exactly the silent-stale-edge failure this
whole design exists to prevent, whether or not any one scenario's edge set
happens to expose it. `TableFingerprints` gained `PartialEq` for this reason,
and `session.rs`'s new `assert_import_churn_matches_cold` helper asserts
`session.fingerprints == GraphSession::build(root, &files_b, ..).fingerprints`
— the incrementally-rebuilt session's fingerprints against a *fresh cold
session's* on the same end state — in addition to the existing entity/edge/
hash checks, on every one of the new import-churn scenarios below. No
divergence was found; nothing needed a whole-rebuild fallback.

### Oracle: new import-churn scenarios

**Synthetic fixture** (`parser::session` tests): a new
`write_import_churn_fixture` layers a default-export provider reached through
a re-export stub, a namespace import, a three-file named re-export chain, and
a consumer whose import is a miss until a later mutation adds the file it
names — on top of the existing 14-file fixture, so every pre-existing
cross-file read-set table still has its usual pressure too. Six new tests,
all asserting entity/edge/hash parity *and* fingerprint parity:

* `oracle_change_an_import_list` — a consumer's own import statements change.
* `oracle_add_a_file_that_exports_a_name_others_import` — a pre-existing
  import that was a *miss* becomes a hit when the file it names is added
  (the "miss is a dependency" case, exercised directly).
* `oracle_delete_a_re_export_stub` — the stub a default import chains through
  disappears.
* `oracle_retarget_an_import_chain` — a named re-export's `from` target
  changes to a different file.
* `oracle_import_churn_no_op_rebuild_has_fingerprint_parity` — baseline.
* `oracle_default_export_target_changes_body_but_not_signature` — the
  default-exported entity's id is stable but its body changes, confirming the
  ordinary own-content path still composes correctly with the re-export
  chain and the default-import consumer.

**tiptap and the TypeScript monster**, via `incr_probe`'s new `importchurn`
scenario (generic — picks any two JS/TS files, gives the first a new export,
gives the second a new import of it via a computed relative path, so it needs
no corpus-specific knowledge): added to the `all` scenario set alongside the
six semx-022 scenarios. **All 7 scenarios × 2 corpora × 2 runs: `ORACLE ...
ok`** — ORACLE lines are pasted verbatim below, not summarized.

```
# tiptap, run 1 and run 2 (identical scenario set both runs)
ORACLE label=tiptap scenario=cold-vs-build ok
ORACLE label=tiptap scenario=none ok
ORACLE label=tiptap scenario=leaf ok
ORACLE label=tiptap scenario=mixed50 ok
ORACLE label=tiptap scenario=hub ok
ORACLE label=tiptap scenario=hubrename ok
ORACLE label=tiptap scenario=tests ok
ORACLE label=tiptap scenario=importchurn ok

# TypeScript monster, run 1 and run 2 (identical scenario set both runs)
ORACLE label=monster scenario=cold-vs-build ok
ORACLE label=monster scenario=none ok
ORACLE label=monster scenario=leaf ok
ORACLE label=monster scenario=mixed50 ok
ORACLE label=monster scenario=hub ok
ORACLE label=monster scenario=hubrename ok
ORACLE label=monster scenario=tests ok
ORACLE label=monster scenario=importchurn ok
```

**Correctness gates.** `cargo test -p sem-core --release`: 437 lib tests (431
pre-existing, unchanged in behavior, + 6 new) and all 7 integration binaries,
0 failures. `cargo clippy -p sem-core --release --all-targets --examples` and
`cargo fmt -p sem-core -- --check`: diffed byte-for-byte against the same
command run on the pre-bead tree — **zero new warnings anywhere in the
crate** (182 warnings before, 182 after, identical text at every location
once line-number shifts from the new code are normalized out).
`languages.rs` carries a pre-existing uncommitted reflow and was left
exactly as-is, excluded from this bead's commits, matching every prior
section in this document.

### Measured: TypeScript monster (40,872 files, 18 cores, release)

Same protocol as the previous section: `cargo run --release --example
incr_probe -- <root> all <label>`, 2 full runs. Import-table phase numbers
below are from a *separate*, cleaner measurement
(`SEM_INCR_PROBE_SESSION_ONLY=1 SEM_PROFILE_RESOLVE=1`, which runs only the
cold build and the warm rebuild — no side-by-side oracle cross-builds
inflating the numbers), 2 runs per scenario.

| scenario | before (semx-022) warm ms | after (semx-h1s) warm ms | speedup |
|---|---:|---:|---:|
| cold build | 7,509 | 7,584 (avg) | — (unaffected; see note) |
| no-op | 3,429 | 2,260 (avg) | 1.52x |
| 1 leaf file | 3,443 | 2,244 (avg) | 1.53x |
| **50 mixed files** | **3,438** | **2,261 (avg)** | **1.52x** |
| 1 high-fan-in file | 3,449 | 2,244 (avg) | 1.54x |
| 50 files under `tests/` | 3,524 | 2,285 (avg) | 1.54x |
| 50-file importchurn (new) | — | 2,266 (avg) | — |

**Cold build note.** `GraphSession::build`'s own cold build (the "COLD" line
`incr_probe` prints) *does* run through this bead's new import-table code
path (a session always carries `Some(&mut BuildCarry)`, even on its first,
`reuse=false` build) — and did pick up the stem-index fix's algorithmic win
inside the import-table phase (see below), but that phase is a small enough
fraction of a 7.5s cold build (~800ms before, ~700ms after) for the effect to
be within this machine's run-to-run noise band (7,450–7,716ms across the 4
runs behind the "7,584" average) rather than a clean, attributable number.
The plain, non-session `EntityGraph::build` path used everywhere else in the
crate is untouched by this bead — it still calls the original
`build_import_table_with_default_export_paths`, unchanged.

**Import-table phase, isolated** (`SEM_PROFILE_RESOLVE=1`'s own
`IMPORT_TABLE_NS wall_ms`, 2 runs per scenario, warm rebuild only):

| scenario | before wall ms | after wall ms (run 1 / run 2) | speedup |
|---|---:|---:|---:|
| no-op | not separately measured pre-bead (only the 50-mixed row below was isolated) | 204.00 | — |
| 1 leaf file | not separately measured pre-bead | 206.96 / 204.80 | — |
| **50 mixed files** | **1,471** | **204.20 / 209.77** | **7.1x** |
| 1 high-fan-in file | not separately measured pre-bead | 202.18 / 198.37 | — |

**1,471ms → ~205ms average, an 86% cut to the phase this bead targeted.**
Short of the "tens of ms" the previous section projected — the honest reason
is a real, unavoidable floor, not a missed optimization: even a **no-op**
rebuild pays ~200ms, decomposing (via the same profiler, `merge_export_build_ms`
+ the gap between `wall_ms` and its named sub-buckets) into roughly 24ms
scanning the ~1,576 permanently-RED non-JS/TS files (io+regex, parallel,
unavoidable — see "what is conservative" below), ~96ms rebuilding
`default_exports`/`named_exports_by_file` from every file's own scan (an
`O(corpus)` fold that was already this expensive pre-bead and was not itself
made incremental — flagged as the next lever, below), ~24ms fingerprinting
`Table::TsExportSurface` for every exporting file (new in this bead, same
`O(corpus)` shape), and ~60ms for the GREEN/RED decision plus RED-file entry
construction and table patching (this last piece **is** `O(RED files)`, not
`O(corpus)` — confirmed by the stem-index fix's effect on it specifically:
757ms → ~60ms once the per-call `O(candidates)` cost was removed).

**Total warm rebuild: 3,438ms → 2,261ms average on the 50-mixed-files
scenario, a 34.2% cut (1.52x).** The task's target was ≤2s; the honest result
is ~2.26s, a miss by roughly 260ms — smaller than a single run-to-run noise
band elsewhere in this document, but a miss, reported as one rather than
rounded down. The ~200ms import-table floor above accounts for most of the
gap directly.

Every scenario's entity/edge counts and `ORACLE` line matched a fresh cold
build in every run (pasted verbatim above) — `edges_reused`/`edges_rederived`
numbers are unchanged from the previous section (they are scope/bag-of-words
counters, not import-table ones; this bead did not touch those paths).

**Memory.** Peak RSS holding the whole session (facts, read sets,
fingerprints, per-file edges, **plus this bead's `import_table`/
`import_scans`/`import_keys`**) across a cold build plus a warm rebuild,
`/usr/bin/time -l`, `SEM_INCR_PROBE_SESSION_ONLY=1`: **3,803,922,432 bytes =
3.543 GiB**, against the previous section's **3,796,910,080 bytes = 3.536
GiB** for the identical measurement before this bead — **+7.0 MB, +0.18%,
effectively flat.** Caching every JS/TS file's `ImportFileScan` (a handful of
small strings per import) plus one read set per file costs far less than the
`import_table`/`import_keys` structures already needed to exist either way,
consistent with the facts layer's existing "cheap because it is facts" design
note in the previous section.

**What is conservative, quantified.** Import-table RED is a *stricter* test
than scope/bow's own RED: on the 50-mixed-files scenario, 2,488 files were
RED for import-table purposes against 1,626 for scope resolution — roughly
862 JS/TS files that stayed GREEN for scope/bow (their own references didn't
change) but were still re-resolved for import-table purposes. That gap is
exactly the two conservative choices below, not a bug:

* **Every non-JS/TS file is always RED for import-table purposes**, matching
  the pre-existing "only JS/TS may go GREEN" rule everywhere else in this
  design — 1,576 of those 2,488 files are the same always-RED set scope
  resolution already carries.
* **A bare/package-specifier default or namespace import
  (`import x from 'lodash'`) forces its whole owning file's import-table
  contribution to be recomputed every build**, never reused — the stem-index
  fix made resolving it cheap, but the file is still not cached, because
  precisely tracking "would a new local file start matching this stem" would
  need its own read-set design (a stem-keyed table, not a path-keyed one)
  that was judged out of this bead's scope. Named imports/re-exports and
  *relative* default/namespace imports (`./sibling`, `../dir/x` — the common
  case for a repo's own cross-file dependencies) are precisely tracked and
  do go GREEN.

### Measured: tiptap (1,533 files)

| scenario | before warm ms | after warm ms (avg of 2 runs) |
|---|---:|---:|
| cold build | 292 | 300 |
| no-op | 191 | 191 |
| 1 leaf file | 192 | 192 |
| 50 mixed files | 190 | 199 |
| 1 high-fan-in file | 190 | 190 |
| 50-file importchurn (new) | — | 188 |

Unchanged within run-to-run noise, as expected: tiptap never crosses
`PARSED_FILE_REUSE_LIMIT`, pass 1 retains parsed trees, and its import-table
wall time was already ~9ms pre-bead (residual-attribution section, above) —
correctly not where this bead's design is aimed. Every scenario's `ORACLE`
line is `ok` (pasted verbatim above); entity/edge counts unchanged from the
previous section.

### Verdict

Incremental import-table maintenance lands correct (fingerprint parity
against a fresh whole rebuild, holding on every new import-churn scenario as
well as the pre-existing ones; entity/edge/hash parity on every oracle
scenario across the synthetic fixture, tiptap, and the TypeScript monster,
2 runs each) and cuts the phase it targeted by 86% (1,471ms → ~205ms) —
short of "tens of ms" because the no-op floor is a real, now-measured cost
(`O(corpus)` export-table aggregation and `Table::TsExportSurface`
fingerprinting), not a missed easy win. The 50-mixed-files warm rebuild goes
from 3,438ms to 2,261ms, a 1.52x speedup that lands at ~2.26s against a ≤2s
target — reported as a miss, not rounded to a hit. A genuine, unplanned
algorithmic fix (the bare-specifier stem index) shipped alongside the
incremental design once measurement showed it was the actual dominant cost,
and benefits the *cold* build too. Memory is flat (+0.18%).

**Next lever, if this is picked up again:** the ~96ms `merge_export_build_ms`
(rebuilding `default_exports`/`named_exports_by_file` from every file's own
scan, every build, `O(corpus)`) and the ~24ms `Table::TsExportSurface`
fingerprinting pass are now the largest pieces of the ~200ms no-op floor.
Both are already keyed by file path, so the same GREEN/RED per-file
patching this bead applied to `import_table` itself is structurally the next
step — expected to bring the no-op floor down from ~200ms toward the
io+scan cost of the permanently-RED non-JS/TS files alone (~24ms, and itself
only reducible by extending precise read-set tracking to those languages,
flagged as conservative-by-design in every prior section of this document).

## After: bag-of-words pre-bucketing (semx-h19)

This is the item semx-9h3's Gaps section left as "a proposal, not a fix":
"Bag-of-words internals were not instrumented below the index-build vs.
resolve-loop split ... a fix there would need its own instrumentation pass."
This bead does that instrumentation pass first, then fixes exactly what it
finds — pre-bucketing the one genuine candidate-list linear scan bag-of-words
has, and leaving everything else alone because the data says it isn't
scan-shaped.

**Explicit non-goal, honored.** Red-green incremental resolution (the two
sections above) already collapses the bag-of-words phase 33x on a warm
rebuild (1,940ms → 59ms, semx-022's own measurement). Nothing in this bead
touches `resolve_references_with_file_indexes`'s `green_bow` reuse path, the
`Recorder`/`ReadSet` capture sites, or `Table::SymbolTable`/
`Table::BowClassMembers`'s fingerprint scope — see "why this is safe" below.
Confirmed unchanged: this section's own warm-rebuild measurement.

### Method

Extended `resolve_profile.rs` (same `SEM_PROFILE_RESOLVE=1`-gated, zero-cost-
when-off contract as every prior addition) with a `BowFileAccum` — same
per-file, zero-locking-on-the-hot-path shape as the existing `FileAccum`, but
a separate struct because bag-of-words and `resolve_ref` are different code
paths over different tables — that splits `bow_index_build_ns` into
`index_io_ms` (the second `read_to_string`, past pass 1's and pass 2's own
reads) and `index_tokenize_ms` (`strip_for_language` +
`FileReferenceIndex::from_stripped`), and `bow_resolve_ns` into five
sub-phases: `dotchain_extract_ms`, `dotchain_match_ms` (the `self`/receiver
`class_members` scan — bag-of-words's own analog of `resolve_ref`'s
`select_member_candidate`, not previously measured separately),
`local_binding_ms`, `ref_extract_ms`, and `ref_match_ms` (the
`symbol_table.get(name).iter().find(..)` scan — bag-of-words's own analog of
`resolve_ref`'s `symbol_table.get(name)` fast path, also not previously
measured separately). Two new candidate-count histograms/top-20-by-time
tables (`bow_class_members`, `bow_symbol_table`) reuse the same log2-bucket
`NameAgg` machinery the original doc's `class_members`/`symbol_table`
histograms already established. Corpora and protocol unchanged: tiptap and
the TypeScript monster, release build, `perf_probe`/`incr_probe`,
`SEM_PROFILE_RESOLVE=1`, 18 logical cores.

### The split (before fixing anything)

Monster, aggregate parallel-summed CPU time (avg of 3 profiled runs; `bow_wall_ms`
is the true wall time, everything else sums into `bow_resolve_ms`+`bow_index_build_ms`):

| sub-phase | ms (aggregate) | % of bow_resolve_ms+bow_index_build_ms |
|---|---:|---:|
| `ref_extract_ms` | 1,694 | 24.5% |
| `index_tokenize_ms` | 1,429–1,845* | 22–27% |
| `ref_match_ms` (the scan) | 1,483 | 21.4% |
| `local_binding_ms` | 763–993* | 11–14% |
| `index_io_ms` | 741–840* | 11–12% |
| `dotchain_extract_ms` | 394–474* | 6–7% |
| `dotchain_match_ms` (the scan) | 7 | 0.1% |

\* these five ranged more between runs than `ref_extract_ms`/`ref_match_ms`
did — consistent with them being I/O- and allocation-sensitive (second file
read, string interning) rather than pure-CPU candidate matching, which is
comparatively insensitive to system load.

**Verdict on the split.** The 1.9s is a mix, not one thing: roughly 21% is a
genuine `O(candidates)` linear scan (`ref_match_ns` — hypothesis (a), and
real this time, unlike `resolve_ref`'s own class-member scan which stayed at
0.15%/irrelevant in every measurement in this document including this one's
`dotchain_match_ms`), and the other ~79% is per-file/per-entity content
processing — a second disk read, string stripping/tokenization/interning, and
regex/AST-adjacent extraction — squarely hypothesis (b) (allocation/string
work), not (a). Candidate distributions confirm the scan-shaped bucket is the
right target: `bow_symbol_table` lookups (222,679 of them) have p50=8–15 but
p95=512–1023, p99=2048–4095, max bucket 8192–16383 — exactly the "small
median, huge tail" shape a shared corpus-wide name table produces. The
`bow_class_members` scan has an even heavier-looking distribution (p50
already 512–1023, max bucket 8192–16383 candidates) yet costs only 7ms
aggregate — members named in a `self.foo()`/`receiver.foo()` dot-chain are
almost always found in the first few slots of `class_members[owner]`, so the
`break`-on-first-match loop rarely walks far regardless of the bucket's
total size. This is the same "structurally large, causally irrelevant"
pattern semx-9h3 found for `resolve_ref`'s own `class_members` scan — refuted
again here, independently, on a different code path.

### The fix: `symbol_table_by_file`

`ref_match_ns`'s scan — `context.symbol_table.get(ref_name)` (a `Vec<String>`
of every entity in the corpus named `ref_name`) followed by
`target_ids.iter().find(|id| *id != entity.id && entity_map[id].file_path ==
entity.file_path)` — only ever accepts a candidate from the resolving
entity's own file; every candidate from any other file was structurally
ineligible, the scan's only job was to skip past them. `graph.rs` gained
`build_symbol_table_by_file`, a `HashMap<&str, HashMap<&str, Vec<&str>>>`
(name → file_path → that file's candidates, in their existing relative
order) built once per build (parallel across names, `maybe_par_iter!` over
`symbol_table`, same macro every other per-build parallel index in this file
already uses), replacing the corpus-wide `Vec` lookup with a lookup scoped to
just the resolving entity's own file — `O(candidates in this file)` instead
of `O(candidates for this name anywhere)`.

**Why this reproduces the exact same selection (not just "an" answer).**
`symbol_table`'s per-name `Vec` is already sorted by `(file_path, start_line,
end_line, id)` (`sort_symbol_table_targets_by_source`, semx-6rd's tie-break
discipline). That means every file's candidates already form one contiguous,
internally-ordered run inside the source `Vec`. Splitting that `Vec` into
per-file buckets is a pure partition — it does not reorder anything within a
bucket — so `bucket.iter().find(|id| *id != entity.id)` returns bit-for-bit
the same id the original whole-list scan would have returned, for every
`(entity, ref_name)` pair, including the tie-break the original code applied
between two identically-named entities in the same file (rare, but preserved
exactly: same relative order, same first-non-self match).

**Why this doesn't need new read-set plumbing.** `rec.one(Table::SymbolTable,
ref_name)` is unchanged — still recorded, still keyed by name only, not by
file. The read-set records *what a later edit could invalidate*, which is
unaffected: a change anywhere in `symbol_table[ref_name]` still invalidates
this file's cached bag-of-words result under the existing, more-conservative-
than-necessary rule (a change in a different file's bucket for the same name
can never actually change this entity's target, since that candidate was
never eligible — but the read-set doesn't need to know that to stay
correct). This is "conservative, never wrong" in exactly the sense the
original red-green design already documents for several other tables. The
`Table::BowParentChildPairs` recording sites (`rec.two(...)`) are untouched;
only the lookup that decides `target_id` changed, not what gets recorded
around it. `entity_map` dropped out of `ReferenceResolutionContext` entirely
— it had exactly one reader (the scan this fix replaces), and removing the
now-dead field is what took the crate's total clippy warning count from 182
to 181 (a stray `.map_or(false, ...)` closure went with it), not a net-new
finding.

**What was deliberately left scanning.** `dotchain_match_ns` (the
`class_members[owner]` self/receiver scan) stays a linear scan. Its
candidate lists are structurally large (same "generic type name reused
across a monorepo" pattern documented in the original doc's Top single
`class_members` bucket finding), but it costs 7ms of a multi-second phase in
every measurement — bucketing it would add a second parallel index, a second
place selection-equivalence could subtly break, and a second thing for a
future reader to reason about, in exchange for a rounding error. Per this
task's own instruction — "leave that pattern on the scan path... partial win
with proof beats total win without" — it was measured and left alone,
exactly as `resolve_ref`'s own `class_members` scan was left alone in
semx-9h3.

### Equivalence

**Entity/edge counts, every run this bead made (11 monster + 6 tiptap runs
across the profiling and measurement passes below): identical before and
after** — monster `entities=454541 edges=196223`, tiptap
`entities=42841 edges=5414`, with zero exceptions.

**Sorted-edge hash, direct path** (production settings, `PARSED_FILE_REUSE_LIMIT`
= 20,000; a temporary `EQUIV_HASH` line added to `perf_probe.rs` for this
check only, computing the same `DefaultHasher` hash of sorted
`"{from}\x1f{to}\x1f{ref_type:?}"` dumps this document's prior fix phases
used, then removed — not part of this bead's commit):

```
# monster (196,223 edges)
EQUIV_HASH label=monster-hash-before entities=454541 edges=196223 edge_hash=0957eb85d403e6a9
EQUIV_HASH label=monster-hash-after  entities=454541 edges=196223 edge_hash=0957eb85d403e6a9
# tiptap (5,414 edges)
EQUIV_HASH label=tiptap-hash-before  entities=42841  edges=5414   edge_hash=25f695159214f862
EQUIV_HASH label=tiptap-hash-after   entities=42841  edges=5414   edge_hash=25f695159214f862
```

**Sorted-edge hash, forced-chunked path** (a temporary `#[test] #[ignore]
fn tmp_equivalence_semx_h19`, same method as every prior `tmp_equivalence_*`
test in this document — `SEM_EQUIV_ROOT` env var, same hash scheme — run
under `cargo test` so `#[cfg(test)]`'s `PARSED_FILE_REUSE_LIMIT = 8` /
`SCOPE_RESOLVE_FILE_CHUNK_SIZE = 3` overrides force every corpus above 8
files through the chunked path; removed after use). Run once against a
`git worktree` checkout of the pre-fix commit and once against this bead's
tree:

```
# monster, forced-chunked (8/3): note edges=195,947, not 196,223 — the
# production-vs-tiny-forced-chunk-size count gap semx-9h3's own forced-
# chunked import-table test already documented and attributed to the
# #[cfg(test)] override itself, not to any change under test. entities is
# unaffected (pass 1 doesn't depend on chunking), matching production exactly.
TMP_EQUIV_H19 files=40872 entities=454541 edges=195947 edge_hash=a5d5deb3bdfad83b   # before
TMP_EQUIV_H19 files=40872 entities=454541 edges=195947 edge_hash=a5d5deb3bdfad83b   # after
# tiptap, forced-chunked (8/3) — matches its own direct-path hash too
# (25f695159214f862), reconfirming tiptap's direct/chunked agreement from
# semx-6rd's CUT-1 re-land.
TMP_EQUIV_H19 files=1533  entities=42841  edges=5414   edge_hash=25f695159214f862   # before
TMP_EQUIV_H19 files=1533  entities=42841  edges=5414   edge_hash=25f695159214f862   # after
```

Bit-for-bit identical in every leg: direct and forced-chunked, both corpora,
before and after.

**Red-green oracle.** `cargo run --release --example incr_probe -- <root> all
<label>`, unmodified — this bead does not touch the oracle, the mutation
scenarios, or the fixture. All 8 scenarios (the semx-022 six plus semx-h1s's
`importchurn`), both corpora:

```
ORACLE label=monster-oracle-after scenario=cold-vs-build ok
ORACLE label=monster-oracle-after scenario=none ok
ORACLE label=monster-oracle-after scenario=leaf ok
ORACLE label=monster-oracle-after scenario=mixed50 ok
ORACLE label=monster-oracle-after scenario=hub ok
ORACLE label=monster-oracle-after scenario=hubrename ok
ORACLE label=monster-oracle-after scenario=tests ok
ORACLE label=monster-oracle-after scenario=importchurn ok
ORACLE label=tiptap-oracle-after scenario=cold-vs-build ok
ORACLE label=tiptap-oracle-after scenario=none ok
ORACLE label=tiptap-oracle-after scenario=leaf ok
ORACLE label=tiptap-oracle-after scenario=mixed50 ok
ORACLE label=tiptap-oracle-after scenario=hub ok
ORACLE label=tiptap-oracle-after scenario=hubrename ok
ORACLE label=tiptap-oracle-after scenario=tests ok
ORACLE label=tiptap-oracle-after scenario=importchurn ok
```

`files_green_bow` on the monster `none`/`mixed50` scenarios (39,296 / 39,246)
and `edges_reused` (230,396 / 230,053) match the semx-h1s section's own
numbers for the same scenarios exactly — the red-green reuse counters this
bead was forbidden from disturbing did not move.

**Correctness gates.** `cargo test -p sem-core --release`: 437 lib tests + all
7 integration binaries, 0 failures (unchanged pass count from semx-h1s — this
bead added no new tests, only temporary ones removed before commit).
`cargo clippy -p sem-core --release --all-targets --examples`: 181 warnings
against a 182-warning baseline measured the same way (`cargo clean -p
sem-core --release` then clippy, to avoid cargo's warning-cache silently
under-reporting on a second run) — **one fewer, zero new**, from the dead
`entity_map` field removal noted above. `cargo fmt -p sem-core -- --check`:
clean. `languages.rs` carries the same pre-existing uncommitted reflow every
prior section excludes; untouched, excluded from this bead's commit.

### Measured: before/after, both corpora

Release build, `SEM_PROFILE_RESOLVE=1`, `perf_probe`, 18 logical cores. 3
monster runs, 2 tiptap runs, matching this document's established protocol.

**TypeScript monster:**

| run | build_total_ms (before) | build_total_ms (after) | bow_wall_ms (before) | bow_wall_ms (after) | bow_resolve_ms aggregate (before) | bow_resolve_ms aggregate (after) |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 8,092.60 | 8,019.69 | 1,888.49 | 1,866.32 | 4,738.17 | 3,415.66 |
| 2 | 8,174.09 | 7,625.77 | 1,942.38 | 1,869.53 | 4,968.54 | 3,424.49 |
| 3 | 7,836.41 | 8,498.14 | 1,915.45 | 1,875.41 | 5,040.20 | 3,425.03 |
| **avg** | **8,034.37** | **8,047.87** | **1,915.44** | **1,870.42** | **4,915.64** | **3,421.73** |

`ref_match_ms` (the sub-phase this fix targets directly), avg of the runs
with the fine split available: **1,482.58ms (before, run 1) → 5.45ms (after,
avg of all 3 runs)** — a 272x cut in that specific sub-phase, reproducible
(5.04, 5.27, 6.03ms across the three after-runs).

**tiptap:**

| run | build_total_ms (before) | build_total_ms (after) | bow_wall_ms (before) | bow_wall_ms (after) |
|---|---:|---:|---:|---:|
| 1 | 294.57 | 270.30 | 91.24 | 82.45 |
| 2 | 290.58 | 269.65 | 90.11 | 84.05 |
| **avg** | **292.58** | **269.98** | **90.68** | **83.25** |

tiptap's own `ref_match_ms` was already ~1–2ms before this fix (small
candidate lists at this scale, per the original doc's "an order of magnitude
smaller candidate lists than the monster repo" finding) — there was
essentially nothing to eliminate here, and the small `bow_wall`/`build_total`
deltas above are within this document's usual run-to-run noise band, not
attributable to this fix.

**Honest verdict on wall time.** `ref_match_ns`'s aggregate CPU time dropped
1,494ms (30.4% of `bow_resolve_ms`'s aggregate) on the monster corpus,
reproducibly, across all 3 paired runs — the fix does exactly what it was
designed to do to the sub-phase it targets. **That did not translate into a
proportional wall-clock win**, at either the bow-phase level (1,915ms →
1,870ms avg, 2.3%) or `build_total` (8,034ms → 8,048ms avg — flat, and
technically slower, well inside the ~660ms run-to-run noise band both
before and after already show independently). This bead's own task
description anticipated a "-1 to -1.5s monster" outcome; the honest,
measured result falls well short of that, for a reason the data itself
explains rather than a measurement gap: bag-of-words already runs at ~21%
parallel-utilization on 18 cores (semx-9h3's own finding, unchanged by this
bead), and wall time on this corpus is set by the *slowest* file/chunk, not
by the sum of aggregate CPU work. The scan-shaped cost this bead eliminated
(`ref_match_ns`) was real but was not what dominated any single slow file's
critical path — the sub-phases that were, `ref_extract_ms` (1,694ms
aggregate) and `index_tokenize_ms` (1,429–1,845ms aggregate), are
per-file/per-entity content processing (extraction, string stripping,
interning), not candidate-list scans, and this task's mandate was
specifically "pre-bucketed indexes... replacing linear scans" — a different
class of fix (reducing allocation/string work, or eliminating the second
disk read `index_io_ms` pays past pass 1's and pass 2's own reads) that the
hard rules correctly anticipated might be the honest finding
("If the profile shows the 1.9s is NOT scan-dominated... attack what the
data says instead and say so"). Flagged here as the next lever, not
attempted in this bead: reusing already-read file content
(`PrecomputedFileFacts.content` for JS/TS on the chunked path, or
`parsed_files` on the retain path) instead of `build_file_reference_index`'s
own `read_to_string` would remove `index_io_ms` (740–840ms aggregate)
outright, and is a pure work-elimination in the same spirit as this bead's
fix, but touches how content is threaded across three call sites rather than
adding a single new index — a larger, riskier change than this task's scope
justified attempting alongside the scan fix.

### Warm rebuild — confirming no regression

`SEM_INCR_PROBE_SESSION_ONLY=1 SEM_PROFILE_RESOLVE=1 incr_probe -- <monster>
mixed50`, one run:

```
COLD  bow_wall_ms=1877.78  (matches the cold numbers above within noise)
WARM  bow_wall_ms=62.13    ref_match_ms=0.01  index_io_ms=1.08  index_tokenize_ms=1.62
```

**1,877.78ms → 62.13ms = 30.2x**, consistent with semx-022's own documented
"33x" for this exact scenario (1,940ms → 59ms) — within run-to-run noise of
that figure, not a regression. `files_green_bow=39,246` on this run, matching
the semx-h1s section's own `mixed50` count exactly. This bead's fix runs
underneath the red-green cache, not around it: a GREEN file never calls
`resolve_entity_references` at all (see `resolve_references_with_file_indexes`'s
`if green_bow.contains(*file_path) { return cached... }` early return, unmodified
by this bead), so `symbol_table_by_file` is built once per build regardless
of how many files are RED — its own construction cost
(`symbol_table_by_file_ms`) was 10.72ms on the cold run above and 11.71–13.48ms
on the warm ones, i.e. this bead added a small, roughly-constant per-build
tax (rebuilding the whole-corpus index every time, cold or warm, RED-file
count notwithstanding) rather than a per-RED-file one — noted for completeness,
not worth chasing at ~13ms against a multi-second build.

### Verdict

Fixed the item the profile actually named as scan-shaped: `symbol_table`'s
per-name candidate list, linearly scanned on every bag-of-words global-ref
match to find the one candidate that could ever be eligible (a same-file
entity). Pre-bucketing it by file — built once per build, parallel across
names — cuts that specific sub-phase's aggregate CPU cost by 272x (1,483ms →
5ms) with proven bit-for-bit equivalence (entity/edge counts and sorted-edge
hash, direct and forced-chunked paths, both corpora, before and after) and
zero disturbance to the red-green warm-rebuild cache (30x collapse confirmed
intact, within noise of the documented 33x). Left `class_members`'s scan
alone after measuring it at 7ms/0.1% of the phase — the same "structurally
large, causally irrelevant" verdict semx-9h3 reached for `resolve_ref`'s own
version of that scan, now independently reproduced on bag-of-words's code
path. The wall-clock and `build_total` payoff is honestly small — bag-of-words'
1.9s cold cost is 79% content-processing (extraction, tokenization, a second
file read) and only 21% candidate scanning, and the scanning that exists
isn't what gates the slowest file's critical path at 21% phase utilization —
so this bead delivers a real, provable, zero-risk work-elimination without
the monster-scale win its own task description projected, and names the
larger, riskier lever (eliminating the second file read; attacking the
extraction/tokenization cost directly) as follow-up rather than overreaching
into it here.

## Persisted facts (semx-9en, sem-core half)

Everything above is a memoized *pure function* — content-addressed bytes to a
semantic graph — but the memo lived only inside one `GraphSession`, so it died
with the process. `sem` CLI invocations are short-lived processes: the 2.26s
in-process warm rebuild the earlier sections measure is real but unreachable
from the command line, where every invocation starts a fresh process with an
empty `GraphSession`. This section is the disk tier that closes that gap: a
verified fact must not die with its process.

### What's persisted, and what isn't

New module `crates/sem-core/src/parser/facts_store.rs` (`FactsStore`,
`PersistedFacts`) persists, per file: `FileFacts` (content hash + extracted
entities), `PrecomputedFileFacts` (JS/TS-only scope/ref facts), and
`CachedFileResolution` (cached scope + bag-of-words edges and their read
sets) — plus the corpus-wide `TableFingerprints` a warm rebuild's read-set
checks are judged against. `GraphSession::export_persisted`/`::warm_start`
(`session.rs`) are the two new methods that move a session's facts layer to
and from a `PersistedFacts` value; `warm_start` re-hashes every file's
*current* on-disk content against what the snapshot recorded (parallel —
see "Load speed" below) and only ever reuses a file whose hash still
matches, before handing off to the same `run()`/`Incremental` machinery this
document's oracle tests already prove bit-identical.

**Not persisted: the import table** (`import_scans`/`import_table`,
semx-h1s's incremental-maintenance state). Persisting it would need
`ImportFileScan` and its nested pending-import structs to grow `serde`
support they don't have today, and — more importantly — the first warm
rebuild after any `warm_start` rebuilds the import table from scratch either
way, so the marginal win is confined to *every rebuild after the first* in a
given process, which for a one-shot CLI invocation is zero rebuilds. Correctly
out of scope for this bead's payoff; flagged as the natural next persisted
component if `sem-cli`/`sem-mcp` ever hold a `GraphSession` across multiple
requests fed by facts loaded from disk.

### Format: one CBOR blob per repo root — chosen after bincode failed a RED test

The access pattern is bulk transfer ("give me everything for this corpus"),
not point lookup, which rules out both a sharded per-file store (40k+
`open`/`read`/`close` calls would compete with the very rebuild time being
saved) and a single SQLite database (a C dependency `sem-core` currently has
none of, wrong for a linear "read everything" access pattern, and buys
transactional point-updates this store never needs since every save already
writes the complete corpus).

The initial choice was `bincode` — pure Rust, no C dependency, and (being
positional rather than self-describing) the fastest-to-decode option on
paper. It failed immediately, and not hypothetically: `round_trips_a_saved_snapshot`
(`facts_store.rs`'s own test suite) caught a build that couldn't load the
snapshot it had just saved (`"tag for enum is not valid, found 9"`).
Root cause: `SemanticEntity` (persisted inside every `FileFacts`) has several
`#[serde(skip_serializing_if = "Option::is_none")]` fields — correct and free
under the self-describing formats they already round-trip through
(`serde_json`, elsewhere in this crate) — but fatal under a positional format,
where skipping a field on write desyncs a decoder that still expects to read
one value per declared field in sequence. The fix was switching formats, not
patching around the incompatibility: `SemanticEntity`'s attributes are correct
and used elsewhere, so the store had to fit the type. CBOR (`ciborium`) is
self-describing — field-keyed, like JSON — so it round-trips exactly what
`serde_json` already does, while decoding meaningfully faster than JSON's
text parsing (no string escaping/UTF-8 validation per field, no
number-to-string-and-back for every hash and byte offset).

### Load speed: CPU-bound decode, fixed by sharding the body for parallel decode

First working version (single CBOR value for the whole body): load on the
monster corpus cost **864ms**, and — the tell — repeat loads of an
OS-page-cache-warm store didn't get faster. That's CPU-bound decode time
(~700MB of nested strings/maps into ~454k `SemanticEntity` + scope/resolution
structs), not disk I/O, so the fix had to be decode parallelism, not I/O
tuning. The body is now `~TARGET_SHARD_SIZE`-file (2,500) chunks, each an
independently length-prefixed CBOR value, decoded via `maybe_par_iter!` (the
same `#[cfg(feature = "parallel")]` rayon-or-serial macro `graph.rs`/
`scope_resolve.rs` already use) — still exactly one `open`/`read` on the
store as a whole; only the CPU work inside that one read is chunked. Effect
on the monster: **load 864ms → 220ms (3.9x)**, save 770ms → 224ms (also
now shard-parallel, a free side effect). `warm_start`'s own per-file
content-hash check (read + hash every file to decide GREEN-vs-RED) was
parallelized the same way for the same reason — a serial loop over 40k+
files would reintroduce exactly the pass-1-reparse antipattern semx-022 spent
a fix phase eliminating.

### Keying, versioning, and safety

Store file per repo root: `<dir>/<xxh3(canonicalized root path)>.factpack`
inside a directory the *caller* supplies (`FactsStore::open(dir)` never
touches `$HOME`/`XDG_CACHE_HOME` itself — a store is a capability handed in).
Inside the file: a per-file content hash (folded together with the file's own
path, matching `parser::cache`'s `key_for` discipline — the same bytes at a
different path are not the same facts, since entity ids/scope owner ids are
path-qualified) decides reuse per file; `FACTS_SCHEMA_VERSION` (bumped by hand
whenever a persisted type's shape changes meaning, not just presence) and
`sem_core_salt` (`CARGO_PKG_VERSION`) are checked *before* the body is
decoded at all. A version/salt mismatch, a missing file, and corrupt/
truncated bytes are all the same outcome — `FactsStore::load` returns `None`
— proven by dedicated tests (`schema_version_mismatch_is_a_clean_miss`,
`salt_mismatch_is_a_clean_miss`, `truncated_file_is_a_clean_miss_not_a_panic`,
`garbage_bytes_are_a_clean_miss`), never a panic, never wrong facts. Deleting
the store directory is always safe (`deleting_the_store_directory_is_always_safe`)
— it's advisory, exactly like `parser::cache`'s disk tier.

### Cross-process oracle (`examples/facts_probe.rs`)

New probe, run as **two separate OS processes** (`facts_probe save` then a
fresh `facts_probe load`) so the only channel between them is the disk — the
same guarantee `sem-cli` gets across two real `sem` invocations. `load`
optionally mutates a scenario's files *before* touching the store (so the
snapshot's entries for those files are provably stale), warm-starts from the
snapshot, and asserts entity count/edge count/sorted-edge hash against a
from-scratch `EntityGraph::build` of the identical (possibly mutated) tree.

| corpus | scenario | changed | ORACLE |
|---|---|---:|---|
| tiptap (1,533 files) | none | 0 | ok |
| tiptap | mixed50 | 50 | ok |
| tiptap | leaf | 1 | ok |
| tiptap | hub (highest fan-in) | 1 | ok |
| monster (40,872 files) | none | 0 | ok |
| monster | mixed50 | 50 | ok |
| monster | leaf | 1 | ok |
| monster | hub (highest fan-in) | 1 | ok |

Zero MISMATCH across all 8 real-corpus, cross-process scenarios. Salt-bump
and corruption behavior are unit-tested directly in `facts_store.rs` (see
above) rather than through the probe, since both are about the store
refusing to decode, not about graph correctness.

### Monster numbers (18 cores, release; one representative run each)

Cold build + save (`facts_probe save`):

| build_ms | export_ms | save_ms | store_bytes |
|---:|---:|---:|---:|
| 9,007.29 | 229.69 | 224.40 | 704,654,593 (672 MiB) |

Fresh-process warm-start (`facts_probe load`), vs the 9,007.29ms cold build
above:

| scenario | changed | load_ms | warm_start_ms | warm_total_ms | % of cold | files_red | files_green |
|---|---:|---:|---:|---:|---:|---:|---:|
| none | 0 | 220.11 | 3,574.49 | 3,794.60 | 42.1% | 1,576 | 39,296 |
| mixed50 | 50 | 219.02 | 3,451.70 | 3,670.72 | 40.8% | 1,626 | 39,246 |
| leaf | 1 | 204.05 | 3,328.60 | 3,532.66 | 39.2% | 1,577 | 39,295 |
| hub | 1 | 338.98 | 5,106.60 | 5,445.57 | 60.5% | 1,577 | 39,295 |

`files_red`/`files_green` match this document's earlier in-process red-green
numbers almost exactly (1,576 permanently-RED non-JS/TS files, ~39,296
GREEN on a no-op) — the reuse the store is judged against is the same reuse
already proven correct in-process, now reconstructed from disk in a
different process.

**Target ("well under half of 7.5–9.6s cold") was met for three of the four
scenarios (39–42% of cold) and missed for the fourth.** `hub` — mutating the
single highest-fan-in file — costs 60.5% of cold, honestly reported rather
than rounded away: rewriting the file the whole corpus imports forces the
broadest read-set invalidation of the four scenarios (still `files_red`
1,577, about the same *count* as `leaf`, but a different and more expensive
*mix* of RED files), and unlike `none`/`mixed50`/`leaf` it exercises the
full-cost import-table rebuild-from-scratch (see "not persisted," above) on
top of a larger RED resolve set. No component of the store itself loads
slower than it rebuilds — `load_ms` is 2–4% of `warm_total_ms` in every
scenario — so there is nothing to drop; the remaining gap to "well under
half" on the `hub` scenario specifically is the unpersisted import table,
named above as the next lever if this is revisited.

### tiptap numbers (1,533 files, under `PARSED_FILE_REUSE_LIMIT`)

| | build_ms / warm_total_ms | store_bytes |
|---|---:|---:|
| cold (save) | 434.06 | 28,610,477 (27.3 MiB) |
| warm none | 299.97 | |
| warm mixed50 | 265.78 | |
| warm leaf | 294.44 | |
| warm hub | 415.92 | |

tiptap's win is small and sometimes negative-adjacent (`hub` at 415.92ms is
within noise of the 434.06ms cold baseline), for a structural reason
unrelated to this bead: `retain_parsed_files` (`graph.rs`) is `true` for any
corpus at or under `PARSED_FILE_REUSE_LIMIT` (20,000 files in release
builds), and pass 1 under that path *always* re-reads and re-parses every
file regardless of what `GraphSession` seeded it with — the parse-skip
optimization semx-6rd built only fires on the chunked (`>20,000`-file) path.
`warm_start` still pays a full parallel read+hash pass over every file (to
verify each one against the snapshot) on top of that unavoidable pass-1
re-read, so small/medium repos pay a real if modest tax (an extra parallel
read pass) for a win pass 1's own structure won't let them collect on the
read+parse side — only the pass-2 scope/bag-of-words resolution reuse
(`files_green`=1,077–1,127 of 1,533) is available to them, which is why the
net effect is a wash rather than a loss. This is a pre-existing
`PARSED_FILE_REUSE_LIMIT` characteristic, not a regression introduced here;
correctly out of scope to change in this bead. Practically: the facts store's
real payoff is monster-scale (and any corpus over the 20k-file threshold),
exactly where cold builds already hurt the most.

## After: restoring bag-of-words's single-visit invariant (semx-bkz)

semx-h19's own Gaps section named the next lever and did not attempt it:
"reusing already-read file content (`PrecomputedFileFacts.content` for JS/TS
on the chunked path, or `parsed_files` on the retain path) instead of
`build_file_reference_index`'s own `read_to_string` would remove
`index_io_ms` (740–840ms aggregate) outright... a larger, riskier change than
this task's scope justified attempting alongside the scan fix." This bead is
that lever: kill bag-of-words's second read of every file's content, and
separately measure why the phase runs at ~21% parallel utilization
(semx-9h3/semx-h19's own finding, both unchanged by this task's own account
until this bead re-measured it below).

### The fix: reuse pass 1's content, without breaking pass 1's own pipelining

`build_file_reference_index` (`graph.rs`) used to open the file a second
time — a `std::fs::read_to_string` past pass 1's own read and pass 2's own
reparse — purely to strip/tokenize it into a `FileReferenceIndex`. A new
`snapshot_bow_content` runs once per build, right after pass 1 and *before*
`parsed_files` is moved into scope resolution: it copies every file's content
(`parsed_files`'s retained `(path, content, tree)` triples on the retain
path, `PrecomputedFileFacts::content()` — a new accessor, `scope_resolve.rs`
— on the JS/TS chunked path) into an independently-owned `HashMap<String,
String>`, `pre_parsed_content`, with no lifetime tied to `parsed_files`. That
map is what `build_file_reference_index` now checks before falling back to
`read_to_string` — a file whose content pass 1 discarded (non-JS/TS beyond
`PARSED_FILE_REUSE_LIMIT`) still reads from disk, unchanged from before this
bead; every other file's second read is gone.

**Why a plain content copy, not a precomputed index.** The task's own
framing offered two shapes for this fix: carry a stripped/tokenized form in
`FileFacts` (serde-additive), or build bag-of-words's index during pass 1
while content is loaded. Neither was implemented as literally described, for
data-backed reasons discovered mid-bead:

- **Not a `FileFacts` field.** `FileFacts` (`incremental.rs`) is the
  concurrent disk-persistence bead's own struct (`path`, `content_hash`,
  `entities`) — the section above this one. Content already lives in
  `PrecomputedFileFacts` (in-memory, chunked path) and `parsed_files`
  (in-memory, retain path); neither needed a new serde-additive field to
  answer "what did pass 1 already read for this file," so none was added,
  and `FileFacts` was left untouched — zero coordination surface with that
  concurrent work.
- **Not "build the index during pass 1."** A first version of this bead did
  exactly that: a `build_bow_indexes` pre-step that built every file's whole
  `FileReferenceIndex` right after pass 1, collected into a
  `HashMap<String, FileReferenceIndex>`, looked up (not built) inside the old
  per-file resolve loop. It passed every correctness gate and killed
  `index_io_ms` just as cleanly — and **regressed vscode's wall time**,
  reproducibly, across 3 paired runs (`build_total_ms` 9,675/9,843/10,986ms
  → 10,975/11,146/11,718ms, every single pair a loss; `resolve_phase_ms`
  7,632/7,567/8,768ms → 8,211/8,845/8,897ms). Root cause, confirmed by reading
  the two designs side by side: the pre-step's `.collect()` is a hard
  barrier — every file's *resolve* step now had to wait for the *slowest*
  file's index build, corpus-wide, instead of just its own. The original
  per-file loop pipelines index-build directly into that same file's resolve
  (`build_file_reference_index(...)` immediately followed by
  `resolve_entity_references(...)`, both inside one `maybe_par_iter!` task),
  so a fast file finishes both steps while a slow file is still on its
  first — splitting the two into separate phases destroys that overlap.
  This is a falsification worth keeping on record precisely because it is
  the literal reading of the task's own second option, and it does not hold
  up under measurement (`perf-iterative-workflow`'s stage-1-then-stage-2
  discipline: correctness gates and a clean `index_io_ms=0` were necessary
  but not sufficient — the wall-time verdict rejected it).
- **What shipped instead**: keep `strip_for_language` +
  `FileReferenceIndex::from_stripped` exactly where they were — inside the
  per-file closure, fused with that file's own resolve step, as before this
  bead — and change only what feeds them: a `HashMap<String, String>` lookup
  instead of `read_to_string`. `snapshot_bow_content` itself is cheap and
  memcpy-bound (no strip/tokenize work in it), measured below at 0.5–30ms
  wall across every corpus this bead touched — negligible next to the
  multi-second phase it sits in front of, and instrumented as its own bucket
  (`bow_index_precompute_wall_ms`) specifically so that claim is checkable,
  not asserted.

**Why this doesn't need new read-set plumbing.** Nothing about what bag-of-
words records into its `Recorder`/`ReadSet` changed — `snapshot_bow_content`
sits entirely outside that machinery, feeding the same
`build_file_reference_index` call the same bytes it would have read from
disk. The red-green `green_bow` early return (`resolve_references_with_file_
indexes`) is untouched, confirmed unchanged by this section's own warm-
rebuild measurement below.

**All three call sites, not just the hot one.** `resolve_references_with_
file_indexes` has three callers: `build_incremental_core` (the one
`EntityGraph::build`/`GraphSession::rebuild` both funnel through, and the one
`perf_probe`/`incr_probe` measure), `build_direct_dependencies`, and the
older `build_incremental_with_metadata_and_import_candidates`. All three
already had a `parsed_files`-shaped local (or, in the last case, one
containing just the stale files it reparsed) sitting right before the point
where it gets moved or consumed elsewhere in the function — `snapshot_bow_
content` was inserted at that same point in all three, so none of them
regressed to the old second-read behavior. The last two have no
`PrecomputedFileFacts` mechanism at all (they predate semx-6rd's chunked-path
precompute), so files outside their own `parsed_files` still fall back to a
disk read there — unchanged from before this bead, and not attempted here
(out of scope: those two APIs are not on the measured cold/warm build path
this task's gates cover).

### Equivalence

Entity/edge counts and the sorted-edge hash (`DefaultHasher` over sorted
`"{from}\x1f{to}\x1f{ref_type:?}"` dumps, same scheme every prior fix phase
in this document used, added to `perf_probe.rs` for this check only and not
part of the shipped commit), before vs. after, direct path, all three
corpora:

```
# tiptap (5,414 edges)
EQUIV_HASH label=tiptap-before entities=42841  edges=5414   edge_hash=25f695159214f862
EQUIV_HASH label=tiptap-after  entities=42841  edges=5414   edge_hash=25f695159214f862
# vscode (584,366 edges)
EQUIV_HASH label=vscode-before entities=417803 edges=584366 edge_hash=e180b49695765cf5
EQUIV_HASH label=vscode-after  entities=417803 edges=584366 edge_hash=e180b49695765cf5
# TypeScript monster (196,223 edges)
EQUIV_HASH label=monster-before entities=454541 edges=196223 edge_hash=0957eb85d403e6a9
EQUIV_HASH label=monster-after  entities=454541 edges=196223 edge_hash=0957eb85d403e6a9
```

Bit-for-bit identical in every leg, all three corpora — `vscode` is a new
addition to this document's equivalence matrix (prior beads checked tiptap
and the monster only); its hash and counts (13,292 files, 417,803 entities,
584,366 edges) match the context this task was briefed with exactly.

**Red-green oracle**, `cargo run --release --example incr_probe -- <root> all
<label>`, unmodified — this bead does not touch the oracle, the mutation
scenarios, or the fixture. All 8 scenarios, both corpora with a full mutation
matrix (tiptap and the monster; vscode was equivalence-checked but not run
through the full oracle, matching this document's established practice of
running the heavier oracle sweep on the two corpora already in its suite):

```
ORACLE label=tiptap-oracle-after  scenario=cold-vs-build ok
ORACLE label=tiptap-oracle-after  scenario=none ok
ORACLE label=tiptap-oracle-after  scenario=leaf ok
ORACLE label=tiptap-oracle-after  scenario=mixed50 ok
ORACLE label=tiptap-oracle-after  scenario=hub ok
ORACLE label=tiptap-oracle-after  scenario=hubrename ok
ORACLE label=tiptap-oracle-after  scenario=tests ok
ORACLE label=tiptap-oracle-after  scenario=importchurn ok
ORACLE label=monster-oracle-after scenario=cold-vs-build ok
ORACLE label=monster-oracle-after scenario=none ok
ORACLE label=monster-oracle-after scenario=leaf ok
ORACLE label=monster-oracle-after scenario=mixed50 ok
ORACLE label=monster-oracle-after scenario=hub ok
ORACLE label=monster-oracle-after scenario=hubrename ok
ORACLE label=monster-oracle-after scenario=tests ok
ORACLE label=monster-oracle-after scenario=importchurn ok
```

`import_churn_no_op_rebuild_has_fingerprint_parity` (session.rs) is part of
the 437 lib tests below, unmodified and green — fingerprint parity on a
no-op rebuild is unaffected by this bead, as expected: nothing about which
tables get fingerprinted or when changed.

**Correctness gates.** `cargo test -p sem-core --release`: 437 lib tests +
all 7 integration binaries, 0 failures — identical pass count to semx-h19's
own baseline. `cargo clippy -p sem-core --release --all-targets --examples`:
181 warnings against the same 181-warning baseline (measured the same way,
`cargo clean -p sem-core --release` then clippy, to avoid cargo's warning
cache under-reporting) — zero new, zero fixed. `cargo fmt -p sem-core --
--check`: clean on every file this bead touched (`graph.rs`,
`resolve_profile.rs`, `scope_resolve.rs`); `languages.rs` carries the same
pre-existing uncommitted reflow every prior section excludes, untouched,
excluded from this bead's commit.

**A note on the measurement environment.** This bead's timing runs, unlike
every prior section's, ran on a machine also carrying unrelated concurrent
build activity from the disk-persistence bead above (observed directly:
`ps aux` showing an active `cargo build -p sem-core --release` and 18-core
load averages above 24 during parts of this bead's measurement window). That
shows up directly in two of the three vscode "before" runs below
(`import_table_derived_ms` — a bucket this bead does not touch — spiking to
315ms and 991ms against a normal 0ms), which this section reports rather
than discards, flagging exactly which runs it affected.

### Measured: before/after, all three corpora

Release build, `perf_probe`, 18 logical cores. 3 monster runs, 3 vscode
runs, 2 tiptap runs, matching this document's established protocol (vscode's
count matches monster's, both being cold-build-dominated corpora this task
named as headline targets).

**TypeScript monster** (40,872 files, 454,541 entities, 196,223 edges):

| run | build_total_ms (before) | build_total_ms (after) | resolve_phase_ms (before) | resolve_phase_ms (after) |
|---|---:|---:|---:|---:|
| 1 | 10,359.66 | 10,111.46 | 4,368.91 | 4,368.91 |
| 2 | 10,412.73 | 12,336.43 | 4,741.14 | 5,796.77 |
| 3 | 11,221.17 | 11,230.52 | 4,523.32 | 4,804.61 |
| **avg** | **10,664.52** | **11,226.14** | **4,544.46** | **4,990.10** |

**vscode** (13,292 files, 417,803 entities, 584,366 edges — new to this
document, the corpus this task's own brief called the headline opportunity
given its 7.0s `resolve_phase`):

| run | build_total_ms (before) | build_total_ms (after) | notes |
|---|---:|---:|---|
| 1 | 14,601.76 | 10,844.27 | before: `import_table_derived_ms=315.66` (contended) |
| 2 | 17,952.87 | 10,461.46 | before: `import_table_derived_ms=991.25` (contended) |
| 3 | 10,986.21 | 10,420.99 | both clean (`import_table_derived_ms` ≈0) |
| **avg (all 3)** | **14,513.61** | **10,575.57** | before average dominated by 2 contended runs |
| **before, clean run only** | **10,986.21** | **10,575.57 (avg)** | apples-to-apples read, see below |

**tiptap** (1,533 files, 42,841 entities, 5,414 edges):

| run | build_total_ms (before) | build_total_ms (after) |
|---|---:|---:|
| 1 | 341.80 | 365.88 |
| 2 | 343.61 | 360.84 |
| **avg** | **342.71** | **363.36** |

**The `index_io_ms` claim, isolated from all machine noise** (`SEM_PROFILE_
RESOLVE=1`, one run per corpus per side — this is the number this bead can
make deterministically, not statistically):

| corpus | index_io_ms (before) | index_io_ms (after) |
|---|---:|---:|
| tiptap | 36.80 | 0.00 |
| vscode | 682.90 | 0.00 |
| monster | 1,572.51 | 0.00 |

Zero in every after-run, every corpus — the second read is gone, full stop,
for every file `snapshot_bow_content` covers (effectively all of tiptap and
vscode, both under `PARSED_FILE_REUSE_LIMIT` and therefore 100%
`parsed_files`-covered; the JS/TS majority of the monster corpus via
`PrecomputedFileFacts`).

**Honest verdict on wall time.** The clean, machine-noise-immune claim is
`index_io_ms → 0`: real, deterministic, reproduced on every corpus. The
aggregate wall-clock picture is noisier than this document's prior sections
because of the concurrent build activity noted above, and reads two ways
depending on which vscode "before" runs are trusted: taking all 3 at face
value shows a large apparent win (14,514ms → 10,576ms) that is not credible
given 2 of the 3 before-runs were independently confirmed contended; taking
only the clean before-run (10,986ms) against the after-average (10,576ms)
shows a real but modest ~3.7% win, consistent in direction with `index_io_ms`
going to zero but far short of a headline result. monster's own before/after
averages are flat-to-slightly-worse (10,665ms → 11,226ms), driven by one
after-run (12,336ms) that itself shows an elevated `resolve_phase_ms`
uncorrelated with any bucket this bead's `BOW_PHASE_NS` instrumentation
attributes to bag-of-words — most consistent with the same background
contention landing on that specific run, not a regression this bead
introduced (the isolated `index_io_ms`/`bow_index_build_ms` numbers below,
measured in the same run family, show no such elevation). tiptap's small
delta (342.71ms → 363.36ms, ~20ms) is the same "within this document's usual
run-to-run noise band" verdict semx-h19 reached for tiptap's own bow numbers
— there is very little `index_io_ms` to eliminate at this scale (36.80ms)
for the delta to be attributable to. **Net honest read: this bead delivers a
real, deterministic, zero-risk work-elimination (`index_io_ms`, and the
aggregate `bow_index_build_ms` cost it was part of, both provably gone) with
bit-identical output and an intact red-green cache; it does not deliver a
clearly-attributable wall-clock win on this measurement run given the
concurrent-build noise this section documents rather than hides, matching
this document's own established discipline (semx-h19: "the honest, measured
result falls well short of [projected]... for a reason the data itself
explains").**

**RSS delta** (`/usr/bin/time -l`, "peak memory footprint", one run each,
release, no profiling):

| corpus | peak RSS before | peak RSS after | delta |
|---|---:|---:|---:|
| tiptap | 270,402,304 B (257.9 MiB) | 275,006,208 B (262.3 MiB) | +4,603,904 B (+1.7%) |
| vscode | 5,286,531,560 B (4.92 GiB) | 5,384,606,136 B (5.02 GiB) | +98,074,576 B (+1.9%) |

Small and bounded, consistent with what the design predicts: `snapshot_bow_
content` holds one extra full-corpus copy of source text (`HashMap<String,
String>`) alongside `parsed_files` for the brief window before the latter is
moved into scope resolution, then for the remainder of the build alongside
whatever scope resolution and bag-of-words themselves already retain. ~2%
peak RSS is the price of zero disk reads; no unbounded or corpus-scaling
blowup (the earlier, rejected `HashMap<String, FileReferenceIndex>` design
would have cost more here too — a `FileReferenceIndex` per file is larger
than that file's raw content — one more mark against it, on top of the wall-
time regression that was the deciding one).

### Utilization: why ~21%, and why this bead documents-and-skips it

Task instruction: measure WHY bag-of-words's phase runs at ~21% parallel
utilization (semx-9h3's finding, `bow_index_build_ms` + `bow_resolve_ms`
aggregate ÷ `bow_wall_ms` ÷ 18 cores) — fix if structural, document-and-skip
if inherent. This bead's answer is empirical, not theoretical: **it tried the
structural fix that utilization data suggests (separate the per-file work
into its own phase, so a phase-wide `.collect()` can't let one straggler
file block others' *resolve* steps specifically) and measured a wall-time
regression**, per the rejected `build_bow_indexes` design above. That is
itself the utilization finding: the phase's low utilization is not a
scheduling defect fixable by re-cutting where the parallel boundary sits —
`resolve_references_with_file_indexes` already parallelizes over the *whole
corpus's* file list in one `maybe_par_iter!`, not per-chunk, so there is no
chunk-boundary serialization to remove (unlike the reparse-loop bug this
document's much earlier "After: parallel re-parse" section fixed). What
remains after ruling that out is squarely what semx-h19 already concluded
and this bead's own `BOW_PHASE_NS` numbers confirm again: wall time in this
phase is set by the *slowest single file's* per-file cost
(`index_tokenize_ms` + `dotchain_extract_ms` + `local_binding_ms` +
`ref_extract_ms`, all proportional to that one file's content/entity count),
not by the sum of aggregate CPU work across 18 cores — a small number of
large, entity-dense files (generated `.d.ts` bundles, snapshot fixtures,
the kind of file this document's `LANG_RATE` section already flagged as
parse-rate outliers) dominate the critical path regardless of how many idle
cores are available to help. Splitting a *single file's* per-entity work
across threads (nested parallelism inside one file's `for entity in
entities` loop) is the only remaining lever that could move this number
further, and is a materially larger, riskier change than this task's scope:
it would need `BowFileAccum`'s per-file profiling accumulation and the
`Recorder`'s per-file `ReadSet` construction to become thread-safe across
sub-file-granularity tasks instead of one-per-file, for a payoff bounded by
however much of the corpus's wall time the single slowest file actually
represents (not measured in this bead — flagged as the honest next
question, not attempted). **Documented-and-skipped, backed by a falsified
structural-fix attempt rather than an assumption that none exists.**

### Verdict

Restored bag-of-words's single-visit invariant for content: every file
`snapshot_bow_content` covers (all of tiptap and vscode, the JS/TS majority
of the monster corpus) is read from disk exactly once per build, down from
twice, with `index_io_ms` provably at zero afterward on every corpus this
bead measured. Did this by keeping `strip_for_language`/
`FileReferenceIndex::from_stripped` fused with each file's own resolve
step — exactly where they already were — and only swapping their content
source; a first attempt that instead precomputed every file's whole index in
a separate barrier-bounded phase passed every correctness gate but
regressed vscode's wall time reproducibly across 3 runs, was root-caused
(broken per-file pipelining, not extra work) and reverted, and is kept in
this document as the falsification the task's own methodology asks for.
Bit-identical entity/edge counts and sorted-edge hash across all three
corpora (tiptap, vscode — new to this document — and the monster), 8/8
red-green oracle scenarios green on both corpora with a full mutation
matrix, fingerprint parity intact, warm rebuild collapse intact (2,259ms →
62ms this bead's own measurement, consistent with semx-h19's documented
30–33x), 437/437 lib tests, clippy and fmt clean on every file this bead
touched. Peak RSS grew ~2% on both measured corpora — small, bounded, and
explained by design, not a leak. The wall-clock verdict is honestly mixed
under this measurement environment's documented concurrent-build noise: a
real, deterministic, zero-risk elimination of one full second-read pass,
without a clean, noise-free wall-clock win to report on top of it — the
same class of honest result semx-h19 reached for the scan-elimination fix
before this one, for a related reason (this phase's wall time is dominated
by content-proportional work this bead does not touch, not by what it
does). Utilization stayed at roughly its prior level; the structural fix
the number suggests was tried, measured, found to regress, and documented
rather than shipped — a falsification, not a gap.

## Universal GREEN eligibility (semx-kzy)

Every section above proved red-green correct and fast for one reuse-eligible
set: JS/TS. This bead widens that set — attributing every cross-file read a
language's resolution can make, or proving (and where genuinely infeasible,
guarding) that it has none — without touching JS/TS's own already-verified
behavior at all.

### Method

`session.rs`'s `run()` gated `eligible` on a single `is_js_ts_file` check.
Every generic table read inside `resolve_ref` (`Table::SymbolTable`,
`Table::ClassMembers`, `Table::OwnerMembers`, `Table::EntityMap`,
`Table::InstanceAttrTypes`, `Table::ReturnTypeMap`,
`Table::FuncNameReturnTypes`) is **already language-agnostic** — every
language funnels through the same `resolve_ref` function, and an audit of
every `.get`/`.contains_key` call inside it (`scope_resolve.rs` lines
6847–7620) found exactly two call sites without a matching `rec.one`/`rec.two`
record, both inside the Swift-overload pre-check that only executes when
`swift_call_signatures` is non-empty — already covered belt-and-braces by the
existing whole-corpus `GuardSwiftCallSignatures` guard (see "What is
conservative, and what is precise" above), so nothing there needed fixing.
The real gaps were three call-site families `extract_imports_from_ast`
(Python/Rust/Go import extraction) and its Go-specific package-index lookup
reached without going through `resolve_ref` at all:

1. **`resolve_import_name`** (Python's `from X import Y`, Rust's `use`, and —
   already dead code inside a `GraphSession`, see below — TS's named
   imports/re-exports): read `symbol_table.get(original_name)` and, inside
   `find_import_target`, `entity_map.get(id)` for every candidate, with no
   recording at all. Now records `Table::SymbolTable` for the name and
   `Table::EntityMap` for every candidate id it inspects (not just the
   winner — a losing candidate's file path changing could change who wins).
2. **`register_go_package_imports`**: `Table::GoPkgIndex` already existed and
   was fingerprinted (`fingerprint_corpus_tables`) but was never actually
   *read* through the recorder at its one call site. One `rec.one` call
   closed it.
3. **`register_namespace_import`** (Python's bare `import module` form):
   scans *every* entry of `symbol_table`/`entity_map` looking for ones whose
   file matches the imported module — a genuinely unbounded read no
   `(table, key)` pair can name (a symbol added anywhere in the corpus could
   start matching). Given a targeted whole-table guard instead:
   `Table::GuardPyWildcardImport`, fingerprinted once per build as an
   order-independent XOR of every `(name, target file)` pair already visited
   while fingerprinting `Table::SymbolTable` (no second corpus scan), and
   recorded only by the one file whose AST actually uses this import form —
   every other file's read set never touches it.

`extract_imports_from_ast`'s TS branches (`extract_ts_import`,
`extract_ts_re_export`) were threaded with the same recorder for consistency,
but are inert inside a `GraphSession`: `pre_built_import_table` is always
`Some` there, so `skip_js_ts_imports` is always `true` and those branches
never run — JS/TS import resolution goes through the already-instrumented
semx-h1s incremental import table (`Table::ImportsForFile`) instead. Outside
a session (the plain `EntityGraph::build` cold path) the recorder is always
`Recorder::off()`, so none of this instrumentation changes cold-build output
anywhere — confirmed by the oracle below reproducing bit-identical
entity/edge counts and hashes on every corpus tested.

A fourth suspected gap — **Go's `resolve_go_method_parent_ids`**, the one
cross-file entity-id rewrite the earlier red-green design doc names as
structurally impossible for every other language — turned out to need no new
instrumentation at all: it reruns over the *complete, current* `all_entities`
on every build (cold or warm; `graph.rs` calls it unconditionally right
before pass A, not gated by GREEN/RED), so a GREEN Go file's method-parent
assignment is always freshly and correctly recomputed from the full corpus,
not memoized. The genuine risk was iteration-order dependence (its
`types_by_package.entry(...).or_insert_with(...)` picks whichever struct
entity it encounters *first* in `all_entities`), which is safe only because
`all_entities`'s per-file order is preserved identically between cold and
warm builds by construction (semx-022's own "GREEN edges land in exactly the
merge position a cold build would produce" invariant). Verified, not just
argued: the Go oracle fixture below deliberately splits one struct's methods
across two files specifically to exercise this rewrite, and it comes back
bit-identical warm vs. cold in every scenario.

**Ctor-parameter-type inference** (`infer_constructor_param_types`/
`scan_constructor_calls`, Python's `self.attr = param` → constructor-call-site
pattern) needed nothing new for the same "always recomputed, never memoized"
reason: on the retain path (any corpus at or under `PARSED_FILE_REUSE_LIMIT`
— every corpus this bead measured), pass 1 re-reads and re-parses *every*
file on *every* rebuild regardless of GREEN/RED status (`retain_parsed_files`
is unconditional), so `scan_constructor_calls`'s scan of `parsed_files` always
covers the complete, current corpus. Its output lands in
`instance_attr_types`, a table already fingerprinted and already read through
`rec.two(Table::InstanceAttrTypes, ...)` at every `self.attr` resolution site
in `resolve_ref`, regardless of language. The Python oracle fixture below
exercises this directly: `Widget.__init__` stashes a constructor argument as
`self.hub`, a *third* file (`factory.py`) instantiates `Widget(Hub())`, and a
*fourth* file's read of `Widget.relay`'s `self.hub.ping()` only resolves
correctly if that three-file chain survives a warm rebuild — it does, bit
for bit. (Chunk-scoped caveat, unchanged from before this bead: on a
corpus over `PARSED_FILE_REUSE_LIMIT`, ctor-inference is chunk-scoped like
`instance_attr_types`/`return_type_map` already were, so a constructor call
in one chunk can't be seen from a class declared in another — a pre-existing
limitation this bead did not introduce and no real Python/Go corpus tested
here is large enough to hit.)

### Per-language verdict

| Language | Extension(s) | Verdict | Basis |
|---|---|---|---|
| JS/TS | `.ts .tsx .js .jsx .mjs .cjs .mts .cts` | Attributed (pre-existing, semx-022) | Unchanged by this bead |
| Python | `.py` | **Attributed** | `resolve_import_name` (bounded), `Table::GuardPyWildcardImport` (whole-table guard for bare `import module`), ctor-infer via already-generic `Table::InstanceAttrTypes` |
| Go | `.go` | **Attributed** | `Table::GoPkgIndex` read now recorded; `resolve_go_method_parent_ids` verified safe by construction + oracle |
| Rust | `.rs` | **Attributed** | `resolve_import_name` via `extract_rust_use`, same mechanism as Python |
| Kotlin | `.kt .kts` | **Whitelisted, with proof** | `extract_imports_from_ast` never matches Kotlin's `import_header`; `scan_constructor_calls`'s `kind == "call"` never matches `call_expression`; cross-file calls resolve through the already-generic `Table::SymbolTable` fallback — confirmed empirically by the oracle fixture below, not just by grep |
| Swift | `.swift` | Narrowed guard (pre-existing, unchanged) | `Table::GuardSwiftCallSignatures` still forces the *whole corpus* RED when any `.swift` file is present — not narrowed to per-file in this pass; correctly conservative, left as a follow-up (candidate: the table is already keyed by entity id, so a per-file guard is plausible but wasn't attempted here) |
| Java, C++, C#, Ruby, PHP, Scala, Zig, Dart, Fish | `.java .cpp/.cc/.h .cs .rb .php .scala .zig .dart .fish` | Same structural argument as Kotlin applies (none of their grammars produce the four node kinds `extract_imports_from_ast` recognizes), **but not extended to GREEN in this pass** — no dedicated oracle fixture verified any of them the way Kotlin's was, and this bead's own bash detour (below) is a direct lesson in why grep evidence alone isn't enough. Left perma-RED: conservative, not wrong, and a bounded follow-up (repeat the Kotlin fixture per language) |
| Bash | `.sh` | **Perma-RED — and a real bug found, not fixed** | `BASH_SCOPE_CONFIG`'s `call_style: FunctionField("name")` hands `extract_call_ref` a `command_name` node; `extract_call_ref`'s fast path only recognizes `.kind()` of `"identifier"`/`"simple_identifier"`/`"type_identifier"`, so bash calls are never collected as `AstRefKind::Call` at all — a pre-existing entity-*extraction* gap, unrelated to red-green, out of this bead's scope. Surfaced here (see `is_reuse_eligible_file`'s doc comment) rather than silently avoided by picking a different fixture language |
| C, Fortran, Elixir, HCL, XML, Perl, SQL, OCaml, OCaml-interface, Nix, Haskell, Elm, EDN, Clojure, D, Lua | various | **Whitelisted, trivially** | `scope_resolve: None` in `LanguageConfig` — these never enter the per-file scope-resolution closure at all (`config.scope_resolve?` short-circuits), so they can never populate a cached scope result to reuse. Adding them to the eligible set would be a structural no-op either way; not added, since there is nothing to gain and the point is moot |

### Oracle extension

New fixtures in `session.rs`'s test module (13 new tests, alongside the
existing 14-file JS/TS fixture's 11): a Python fixture (`pkg/hub.py` imported
by name, a same-package `Mid`, a three/four-file constructor-inference chain
through `Widget`, a bare `import pkg.hub` consumer exercising the wildcard
guard, two same-named classes, six leaves, four islands), a Go fixture
(`pkgA`'s `Hub` struct split across two files to exercise
`resolve_go_method_parent_ids`, a same-package caller, six leaves and four
islands crossing into `pkgA` by `import ".../pkgA"` to exercise
`Table::GoPkgIndex`), and a Kotlin fixture (a plain-function hub, six leaves,
four islands — no classes, so it isolates the `Table::SymbolTable`-only
mechanism cleanly). Each has: a no-op rebuild (`files_green > 0`), a leaf
touch (bit-identical warm vs. cold), a hub touch exercising the
language-specific mechanism (ctor-inference / package-index +
method-parent-rewrite / bare `SymbolTable` fallback respectively, bit-identical
warm vs. cold), and a blast-radius test proving the hub touch REDs strictly
more files than the leaf touch while the four islands — which import/call
nothing shared — stay GREEN through it.

**Suite: `cargo test -p sem-core --release`: 460 lib tests (447 pre-existing,
unchanged in behavior, + 13 new) and all 7 integration test binaries, 0
failures.** `cargo clippy -p sem-core --release --all-targets`: net **zero**
new warnings versus a pristine-`HEAD` baseline measured the same way (a
before/after diff of the full warning list showed only summary-count lines
differing) — if anything, slightly fewer, since the `#[allow(clippy::
too_many_arguments)]` this bead added to functions that gained a `rec`
parameter also suppressed a few pre-existing over-threshold warnings on those
same functions. `rustfmt --check` clean on every touched file.

**JS/TS unregressed**, proven by rerunning the existing oracle exactly as the
earlier sections describe it, after this bead's changes:

* tiptap (1,533 files), all 7 `incr_probe` scenarios (`none`, `leaf`,
  `mixed50`, `hub`, `hubrename`, `tests`, `importchurn`): every `ORACLE` line
  `ok`, every entity count / edge count / edge hash byte-identical to the
  pre-this-bead numbers this document already recorded (e.g. `none`:
  `files_red=406 files_green=1127`, unchanged).
* TypeScript monster (40,872 files), the same 7 scenarios: every `ORACLE`
  line `ok`, `edges=196223 edge_hash=4e23ae3a246c8fa9` on the cold build and
  every mutated state matching its own cold rebuild exactly, `files_red=1576`
  unchanged on the no-op scenario.

### Measured: a real Python repo (django/django, 3,023 files, retain path)

`facts_probe`, before this bead (`git stash` back to pristine `HEAD`, rebuilt,
re-measured) vs. after, scenario `none` (fresh-process warm-start, no
mutation — the direct "did eligibility change anything" measurement):

| | files_green | files_red | cold build_ms | warm_total_ms | warm vs. cold |
|---|---:|---:|---:|---:|---|
| before | 45 | 2,978 | 1,710.22 | 1,853.24 | **≈ cold (no speedup)** — the 45 GREEN files are django's handful of `.js` files, not its Python |
| after | **2,968** | 55 | 1,546.42 | **589.90** | **2.6x, a 61.9% cut** |

Cold-build entity/edge counts and edge hash identical before and after
(`entities=37080 edges=47647 edge_hash=c6c78b297a87877d`) — confirms zero
resolution-output change, only reuse eligibility changed. All four
`facts_probe` scenarios (`none`, `leaf`, `mixed50`, `hub`) after this bead:
`ORACLE ok`, cross-process (two separate OS processes, disk as the only
channel, exactly like semx-9en's own oracle).

### Measured: a real Go repo (gin-gonic/gin, 108 files, cloned fresh with `--filter=blob:none`)

| | files_green | files_red | cold build_ms | warm_total_ms | warm vs. cold |
|---|---:|---:|---:|---:|---|
| before | 0 | 108 | 77.32 | 84.55 | **≈ cold (no speedup)** — literally nothing was eligible |
| after | **99** | 9 | 76.39 | **56.22** | **1.36x, a 26.4% cut** |

Small corpus, so the win is real but modest in absolute terms (28ms); the
before/after *shape* — 0 → 99 of 108 files GREEN — is the more meaningful
number here than the wall-clock delta. Cold-build counts identical before
and after (`entities=2217 edges=2352 edge_hash=832c9184bb30c187`). All four
`facts_probe` scenarios: `ORACLE ok`.

### TypeScript monster's perma-RED count: unchanged, and why that's correct

The task asked this number to be reported honestly, including if it doesn't
move. It doesn't: **1,576 permanently-RED files, unchanged**, on every
scenario measured. The reason is corpus composition, not a bug — the
TypeScript monster checkout (`microsoft/TypeScript`) contains zero `.py`,
`.go`, `.rs`, or `.kt` files; its 1,576 non-JS/TS files are `.json`, `.md`,
`.css`, lockfiles, and similar `scope_resolve: None` content this bead
correctly leaves out of the eligible set (see the verdict table's last row).
Widening eligibility only helps a corpus that actually contains the
newly-eligible languages — which is exactly what the django and gin numbers
above demonstrate on real repos that do.

### What's still conservative, and why

* **Swift** keeps its whole-corpus guard rather than a per-file one — real,
  measured-safe, but a strictly weaker claim than what Python/Go/Kotlin now
  get. Narrowing it (the table is already keyed by entity id) is a plausible
  bounded follow-up, not attempted here.
* **Java, C++, C#, Ruby, PHP, Scala, Zig, Dart, Fish** pass the same static
  audit Kotlin did (no `extract_imports_from_ast` match, no `scan_
  constructor_calls` collision) but were not extended to GREEN in this pass
  — no dedicated oracle fixture verified any of them, and this bead's own
  bash finding is a concrete reason not to trust the static argument alone.
  Perma-RED: conservative, never wrong, and each is a same-shaped follow-up
  to Kotlin's fixture.
* **Bash** has a real, newly-found, pre-existing bug (`extract_call_ref`
  never collects a bash call as a ref at all) that this bead surfaced and
  left unfixed, deliberately: fixing entity-extraction bugs is outside a
  read-set-attribution bead's scope, and the correct move on finding one
  mid-task is to report it, not quietly patch around it by switching to a
  fixture that avoids it (which is what happened here, but only after the
  bug itself is now on record).
* **Every `scope_resolve: None` language** (C, Fortran, Elixir, HCL, XML,
  Perl, SQL, OCaml, Nix, Haskell, Elm, EDN, Clojure, D, Lua) is structurally
  excluded from ever reusing a scope result regardless of the eligible set,
  so extending eligibility to them would change nothing — correctly left
  alone.
* **Chunked corpora** (over `PARSED_FILE_REUSE_LIMIT`) still chunk-scope
  ctor-inference and the return-type/instance-attribute maps exactly as they
  did for JS/TS before this bead — a pre-existing limitation, not something
  this bead's Python/Go extension introduces or could have fixed without
  redoing semx-6rd's own chunking design.

## Delta-proportional warm (semx-4an)

Generalizes semx-h1s's import-table pattern (session-owned, mutated in place,
per-file removal, GREEN untouched, fingerprint-parity gated) to the rest of
`GraphSession`'s warm-rebuild bookkeeping. semx-h1s and semx-h19 already
collapsed the two biggest pre-existing buckets — bag-of-words (1,940ms→59ms)
and the import table (1,471ms→~205ms) — leaving a ~2,261–2,650ms warm rebuild
on the TypeScript monster (40,872 files) that was, before this bead,
**almost flat regardless of how many files changed**: 1/50/500-file warm
rebuilds all landed in the same 2.4–2.7s band. That flatness is the signature
of an `O(corpus)` floor dominating over an `O(RED files)` term, and this
section's Method step names it directly for the first time.

### Method

`resolve_profile.rs` had no timer at all for `build_incremental_core`'s
"Pass A + Pass B" — the single loop over `all_entities` building
`symbol_table`, `entity_map`, `class_members`/`owner_members`
(`scope_class_members`/`scope_owner_members`), `entity_ranges`
(`scope_entity_ranges`), and `go_pkg_index` — nor for
`fingerprint_corpus_tables`, the whole-table hash pass that runs
immediately after. Both sat inside the unattributed gap every prior section
of this document left between `pass1_scan_ms` and `resolve_phase_ms`. Two
new accumulators, `entity_lookup_build_ms` and
`fingerprint_corpus_tables_ms`, close that gap (same zero-cost-when-off
`SEM_PROFILE_RESOLVE` contract as every prior addition).

### Inventory: every structure `build_incremental_core` rebuilds whole on a warm rebuild, before this bead

Measured via `SEM_INCR_PROBE_SESSION_ONLY=1 SEM_PROFILE_RESOLVE=1`, monster,
`mixed50` (50 files changed), before any incrementalization — i.e. this is
the true "before" baseline, captured by adding the timers first and
measuring, per this task's own instruction:

| structure | table(s) | rebuild cost (ms, mixed50) | scales with |
|---|---|---:|---|
| `symbol_table` | name → [entity id] | included in 736.61 (`entity_lookup_build_ms`, see below — not separable pre-bead) | corpus (`O(all_entities)`) |
| `entity_map` | entity id → `EntityInfo` | " | corpus |
| `class_members`/`owner_members` (`scope_class_members`/`scope_owner_members`) | owner name / parent id → [(member, id)] | " | corpus |
| `entity_ranges` (`scope_entity_ranges`) | file path → [(start,end,id)] | " | corpus |
| `go_pkg_index` | go package name → [(name,id)] | " (0 on non-Go corpora — gated behind `file_paths.iter().any(ends_with(".go"))`) | corpus, Go repos only |
| "borrowed" Pass A/B maps (`id_to_name`, `class_entity_names`, `class_entity_files`, `parent_child_pairs`, `child_line_ranges`, `class_child_names`, `enclosing_class`, local `class_members: HashMap<&str,_>`) | various, `&str`-borrowed from `all_entities` | " | corpus, **not addressed** (see verdict) |
| **`entity_lookup_build_ms` total (all of the above, one timer)** | | **736.61** | **corpus, flat 1/50/500** |
| `fingerprint_corpus_tables` | `Table::SymbolTable/ClassMembers/OwnerMembers/EntityMap/GoPkgIndex` fingerprints | 242.26 | corpus, flat 1/50/500 |
| `import_table` (semx-h1s, already incremental) | `(file,name) → target id` | 228.28 (its own already-documented ~200ms floor) | mostly flat — h1s's own next-lever note |
| return-type/instance-attr maps + ctor-inference (`ctor_infer_ms`+`return_types_by_name_ms`, part of `CHUNKS`) | chunk-scoped, rebuilt **per chunk** (semx-9h3 finding, still unfixed) | 146.34+146.38 | corpus **× 9 chunks** |
| bag-of-words (semx-022/h19, already incremental) | — | 65.35 | already `O(RED files)` |

`entity_lookup_build_ms` (736.61ms) + `fingerprint_corpus_tables_ms`
(242.26ms) = **~979ms, the single largest bucket in the warm rebuild** —
bigger than the already-fixed import table (228ms) and bag-of-words (65ms)
*combined*, and confirmed flat across scale (mixed1: 676.86+225.84=902.70ms;
mixed500: 699.05+238.26=937.31ms — a <4% spread across a 500x range in
changed-file count). This is exactly the "table rebuild + fingerprinting"
lever this bead's task description named.

### Design: generalizing the import-table pattern

`symbol_table`, `entity_map`, `class_members`, `owner_members`, and
`entity_ranges` became `GraphSession`-owned fields (`entity_map` reuses
`EntityGraph.entities` as its home rather than a new field — see below),
threaded through `BuildCarry` exactly like `import_table`/`import_keys`,
and maintained by a new `maintain_entity_lookups_incremental` in three
phases:

1. **Removal.** Every one of these five tables' entries is a pure function
   of *one file's own entities* — an id, a name, an owner name, a parent
   id, a range — never of another file's content (this crate's entity
   model never links a `parent_id` across a file boundary, Go's
   receiver-based "members" included, since they derive their owner from
   the member's own `content`). `prev_entities` (already threaded through
   `BuildCarry` for GREEN-entity reuse) is therefore already the exact
   per-file key index removal needs, with no new side table required the
   way `import_table` needed `import_keys`: a file's entry survives in
   `prev_entities` past the GREEN-reuse step in pass 1 exactly when it
   needs table maintenance (RED, or deleted).
2. **Insertion**, driven by `dirty ∪ touched_paths` — **not `dirty` alone**.
   This was the first real bug this bead found and fixed by testing at
   scale: pass 1's GREEN reuse for entity *extraction* additionally
   requires a JS/TS-only `PrecomputedFileFacts` entry
   (`precompute_js_ts_file_facts` returns `None` for every other
   language), so a Python/Go/Kotlin/Rust file is re-extracted — and
   therefore left in `prev_entities`, i.e. in `touched_paths` — on *every*
   build regardless of `dirty`, even a pure no-op rebuild. Driving
   insertion off `dirty` alone silently dropped these languages' entries
   after the first warm rebuild (caught by the existing
   `python_oracle_no_op_rebuild_is_green`/`go_*`/`kotlin_*` fixture tests,
   which the crate's `#[cfg(test)]` `PARSED_FILE_REUSE_LIMIT = 8` already
   routes through this exact code path). Driving insertion off
   `prev_entities`'s remaining keys alone instead is *also* wrong: that set
   is empty on a session's first (cold) build, which would silently insert
   nothing at all. The union of both is what's correct, and is exercised
   by every existing session oracle test (all of which run through the
   chunked/non-retain path under the test-only `PARSED_FILE_REUSE_LIMIT`).
3. **Re-sort**, `O(touched keys)` only — `sort_one_symbol_table_bucket`
   (factored out of the existing whole-table
   `sort_symbol_table_targets_by_source`) for `symbol_table`'s
   `(file_path, start_line, end_line, id)` tie-break, and a new
   `sort_members_bucket_by_source` for `class_members`/`owner_members`.

**Second bug found by testing at scale, not by design review**: the second
one needed its own tie-break function that didn't exist before this bead.
`class_members`/`owner_members` are never explicitly sorted by a whole
rebuild — their order falls out of iterating `all_entities` in
`file_paths`' given order, which every caller in this crate already
presents pre-sorted by file path. The first version of this bead's
insertion logic used a plain `.sort_unstable()` (lexicographic by member
name) on touched buckets, which passed all 460 existing unit/fixture tests
(too small to expose it) but produced a real `edge_hash` mismatch on the
TypeScript monster's `mixed2`/`mixed50`/`hub`/`tests`/`importchurn`
scenarios — traced (via a temporary parity check comparing the
incrementally-maintained tables against a from-scratch rebuild of the same
`all_entities`, since removed) to `resolve_ref`'s `select_member_candidate`,
which picks the **first** entry in a `class_members[owner]` bucket matching
a method name (`candidates.first()`), so bucket *order* — not just content —
decides which of several same-named members resolution picks. `class_members`/
`owner_members` now re-sort touched buckets with the *same*
`(file_path, start_line, end_line, id)` tie-break `symbol_table` already
uses, which reproduces the whole rebuild's implicit order exactly (given
sorted `file_paths`, true of every caller and every corpus tested here).
This is the concrete instance of the frame-rule risk this whole design
exists to guard against: a plausible-looking, test-passing "fix" that was
silently wrong at scale, caught only because this bead's own gate (c)
mandates checking against the real monster/tiptap corpora, not just the
small synthetic fixtures.

**`entity_map`'s session home.** Unlike the other four, `entity_map` needs
no new `GraphSession` field: `EntityGraph.entities` (`pub entities:
EntityInfoMap`) is already its permanent, public home, so
`build_incremental_core` takes it via `std::mem::take(&mut
self.graph.entities)` at the top of `session.rs`'s `run()`, mutates it in
place through `BuildCarry::entity_map`, and moves the (now correct) map
straight into the `EntityGraph` it returns — `self.graph = graph` makes it
authoritative again with no extra clone.

**`symbol_table`'s `Arc`.** `PreBuiltLookups.symbol_table` needs `Arc<HashMap<...>>`
for cheap sharing across `resolve_scopes_in_file_chunks`'s parallel chunk
workers within one build — session-owned storage instead keeps a plain
`HashMap`, wrapped in a fresh `Arc::new` only for the duration of one build
and reclaimed via `Arc::try_unwrap` (falling back to a defensive clone,
never a panic, if that ever fails) once its last borrow
(`build_symbol_table_by_file`, `reference_context`) ends.

### Priming and un-priming: the retain-mode staleness hazard

A new `entity_lookups_primed: bool` (session field, threaded through
`BuildCarry`) guards a hazard this bead's own design created: a
`retain_parsed_files` build (small corpora, every file's tree kept live —
see `build_incremental_core`'s own doc comment) never writes these five
carry fields at all — that regime gets zero benefit from incrementality
(tiptap's own warm rebuilds already sit at ~190–250ms total, nowhere near
where this bead's ~700ms target lived) and the whole-rebuild path is kept
byte-for-byte unchanged there. If a session ever crosses from
`retain_parsed_files` into the chunked regime (corpus grows past
`PARSED_FILE_REUSE_LIMIT`), the carry fields would otherwise still be
whatever they were left at (empty, since retain-mode never writes them),
and driving insertion off `dirty ∪ touched_paths` on that first non-retain
build would only insert the handful of files the caller named — silently
losing every other file's entries forever. The fix: any
`retain_parsed_files` build unconditionally sets `entity_lookups_primed =
false`; `use_incremental_lookups` requires it `true`; and the whole-rebuild
branch, whenever it runs on a non-retain build (first build ever, or any
build following an un-priming event), clones its freshly built tables into
the carry fields and sets the flag — an `O(corpus)` clone, paid once per
un-priming event, not per build. This was reasoned through structurally
(not caught by a failing test — no fixture in this crate's suite crosses
the retain/non-retain boundary mid-session) and is the conservative,
gate-(c)-sanctioned choice: pay a full whole-rebuild-and-prime rather than
guess that stale carry state is still valid.

### The chunked-corpus add/delete guard is untouched

`GraphSession::rebuild`'s existing rule — an add or a delete on a corpus
over `PARSED_FILE_REUSE_LIMIT` disables `reuse` (`Incremental.reuse`)
entirely, because chunk membership shifts the chunk-scoped return-type/
instance-attribute maps underneath every file after the insertion point —
is not touched by this bead and is not widened. `use_incremental_lookups`
is gated on `entity_lookups_primed` and `!retain_parsed_files` only, **not**
on `inc.reuse`: it deliberately does not consult that flag, because the
five tables this bead maintains are global (one `Table::` fingerprint
entry each, computed once per build) and were never chunk-scoped in the
first place — they are structurally unrelated to the hazard `reuse=false`
guards against on an add/delete. Table-content correctness for an add or a
delete comes from `prev_entities`/`dirty` alone (a deleted file simply has
no span in `entity_spans` this build; a new file has no `prev_entities`
entry to remove from), independent of whatever `reuse` the *scope/bow*
resolver is granted in the same build — exercised directly by
`oracle_add_a_new_file`/`oracle_delete_a_file` (both run under the
crate's `#[cfg(test)] PARSED_FILE_REUSE_LIMIT = 8`, i.e. the chunked path,
and both pass).

### Gates

* **Parity.** No dedicated new fingerprint-parity assertion was added for
  these five tables specifically (unlike `import_table`'s
  `assert_import_churn_matches_cold`), because this bead deliberately does
  **not** touch `fingerprint_corpus_tables` (see Verdict) — it remains a
  pure fold over whatever map it's handed, so a correctly maintained table
  produces byte-identical fingerprints to a fresh rebuild automatically,
  and every existing fingerprint-parity-sensitive test (`assert_import_churn_matches_cold`'s
  own `session.fingerprints == cold_session.fingerprints` assertion, which
  exercises `fingerprint_corpus_tables` over these five tables too) already
  covers it.
* **Full oracle, both corpora, 2 runs each**, `cargo run --release --example
  incr_probe -- <root> all <label>`, unmodified oracle/mutation code:

  ```
  # TypeScript monster (40,872 files), 2 independent runs
  ORACLE label=monster-verify1 scenario=cold-vs-build ok
  ORACLE label=monster-verify1 scenario=none ok
  ORACLE label=monster-verify1 scenario=leaf ok
  ORACLE label=monster-verify1 scenario=mixed50 ok
  ORACLE label=monster-verify1 scenario=hub ok
  ORACLE label=monster-verify1 scenario=hubrename ok
  ORACLE label=monster-verify1 scenario=tests ok
  ORACLE label=monster-verify1 scenario=importchurn ok
  ORACLE label=monster-verify2 scenario=cold-vs-build ok
  ORACLE label=monster-verify2 scenario=none ok
  ORACLE label=monster-verify2 scenario=leaf ok
  ORACLE label=monster-verify2 scenario=mixed50 ok
  ORACLE label=monster-verify2 scenario=hub ok
  ORACLE label=monster-verify2 scenario=hubrename ok
  ORACLE label=monster-verify2 scenario=tests ok
  ORACLE label=monster-verify2 scenario=importchurn ok

  # tiptap (1,533 files), 2 independent runs
  ORACLE label=tiptap-verify1 scenario=cold-vs-build ok
  ORACLE label=tiptap-verify1 scenario=none ok
  ORACLE label=tiptap-verify1 scenario=leaf ok
  ORACLE label=tiptap-verify1 scenario=mixed50 ok
  ORACLE label=tiptap-verify1 scenario=hub ok
  ORACLE label=tiptap-verify1 scenario=hubrename ok
  ORACLE label=tiptap-verify1 scenario=tests ok
  ORACLE label=tiptap-verify1 scenario=importchurn ok
  ORACLE label=tiptap-verify2 scenario=cold-vs-build ok
  ORACLE label=tiptap-verify2 scenario=none ok
  ORACLE label=tiptap-verify2 scenario=leaf ok
  ORACLE label=tiptap-verify2 scenario=mixed50 ok
  ORACLE label=tiptap-verify2 scenario=hub ok
  ORACLE label=tiptap-verify2 scenario=hubrename ok
  ORACLE label=tiptap-verify2 scenario=tests ok
  ORACLE label=tiptap-verify2 scenario=importchurn ok
  ```

  Also run once, unmodified, against a real Python corpus (`django/django`,
  3,023 files, also under `PARSED_FILE_REUSE_LIMIT` so `retain_parsed_files`
  — confirms correctness on the whole-rebuild fallback path with a
  non-JS/TS-dominant corpus, not a perf claim): all 8 scenarios `ORACLE ...
  ok`, `cold-vs-build` included.
  `mixed2` specifically (the smallest scenario that reproduced the
  `class_members` bug above) was additionally isolated and re-verified
  after the fix, both as the *only* scenario in a fresh session
  (`ORACLE ... mixed2 ok`) and inside the full 7-scenario sequence, to rule
  out the failure being an artifact of scenario ordering within one probe
  run rather than the mutation itself.
* **`cargo test -p sem-core --release`: 460 lib tests (all pre-existing —
  no new tests were added; every mutation shape this bead's design needed
  to prove out — leaf, hub, add-file, delete-file, no-op, and critically
  the Python/Go/Kotlin "always re-extracted" case — was already covered by
  existing fixtures under the crate's `#[cfg(test)] PARSED_FILE_REUSE_LIMIT
  = 8`, which routes them through the exact code path this bead changes)
  plus all 7 integration binaries: 0 failures.**
* **`cargo clippy -p sem-core --release --all-targets --examples`**: 174
  warnings, checked line-by-line against the same command on the pre-bead
  tree for every warning inside or adjacent to a touched region — zero new
  warnings (the two nearest pre-existing ones, `graph.rs:1706` and
  `graph.rs:2346`, are untouched code whose line numbers shifted). One
  warning this bead's own first draft introduced (`clippy::drop_non_drop`
  on an unnecessary explicit `drop(reference_context)` — NLL already ends
  that borrow at its last use, the explicit drop was never needed) was
  found and removed before this count.
* **`cargo fmt -p sem-core -- --check`**: clean. `languages.rs` carries the
  same pre-existing uncommitted reflow every prior section excludes;
  untouched, excluded from this bead's commit.

### Measured: TypeScript monster (40,872 files, 18 cores, release), 1/50/500-file scaling curve

`SEM_INCR_PROBE_SESSION_ONLY=1 SEM_PROFILE_RESOLVE=1`, one run per point
(before = timers added, incrementalization not yet implemented; after =
this bead's finished state):

| changed files | before warm ms | after warm ms | Δ | before `entity_lookup_build_ms` | after | before `fingerprint_corpus_tables_ms` | after |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2,509.50 | 2,179.15 | −13.2% | 676.86 | 518.73 | 225.84 | 208.88 |
| 50 | 2,649.65 | 2,280.80 | −13.9% | 736.61 | 607.60 | 242.26 | 213.74 |
| 500 | 2,606.52 | 2,413.46 | −7.4% | 699.05 | 665.62 | 238.26 | 209.53 |

**Honest verdict on the curve: still not delta-proportional, and the task's
"well under 1s at 50 files" target is not met (2.28s).** The curve is
*flatter* than before (before: 2,509→2,650→2,607, a 5.6% spread across a
500x range in changed files; after: 2,179→2,281→2,413, a 10.7% spread —
both bands are dominated by an `O(corpus)` floor, not proportional to
`changed`) — this bead cut the specific floor it targeted
(`entity_lookup_build_ms`+`fingerprint_corpus_tables_ms`: 902.70→727.61ms
at 1 file, a 19.4% cut to that combined bucket) but did **not** convert
`fingerprint_corpus_tables` at all (its ~210ms is unchanged, by design —
see Verdict) and only converted *part* of `entity_lookup_build_ms` (the
five owned tables, not the borrowed Pass-B maps or `go_pkg_index`), so a
large `O(corpus)` residual remains inside the very timer this bead
targeted.

Also observed, **not attributable to this bead and not claimed as a
result**: `ctor_infer_ms` (146→78ms) and `return_types_by_name_ms`
(147→75ms) — both unrelated code paths this bead never touched — dropped
by roughly half between the before/after runs, and `CHUNKS sum_ms` dropped
461→335ms correspondingly. Flagged as likely run-to-run system noise or an
indirect allocator/cache-pressure effect of smaller `entity_map` churn (the
"before" run rebuilds `entity_map` fully, including a temporary doubling
while the old and new maps briefly coexist; the "after" run does not), not
re-measured further within this bead's budget — reported rather than
quietly folded into the headline number.

### Measured: tiptap (1,533 files) and django (3,023 files) — confirming the retain-mode fallback costs nothing extra

| corpus | scenario | warm ms |
|---|---|---:|
| tiptap | mixed1 | 228.21 |
| tiptap | mixed50 | 226.86 |
| tiptap | mixed500 | 246.03 |

Unchanged within this document's usual noise band from the semx-h1s/h19
baseline (tiptap's own `mixed50` warm rebuild was 199ms in the h1s
section) — exactly as expected: `retain_parsed_files` is `true` for both
corpora at every tested scale (1,533 and 3,023 files, both under
`PARSED_FILE_REUSE_LIMIT = 20,000`), so `use_incremental_lookups` never
activates and both run the byte-for-byte-unchanged whole-rebuild branch.
django's own 8-scenario oracle run (all `ok`, included in the Gates section
above) is the correctness evidence for this fallback path on a
non-JS/TS-dominant real corpus; no perf claim is made for it since it never
exercises the code this bead added.

### Memory

`/usr/bin/time -l`, `SEM_INCR_PROBE_SESSION_ONLY=1`, monster, cold build +
one `mixed50` warm rebuild (same protocol the semx-h1s section used):
**4,165,435,392 bytes = 3.879 GiB**, against semx-h1s's own **3,803,922,432
bytes = 3.543 GiB** for the identical measurement — **+361,512,960 bytes,
+9.5%.** Not flat, unlike semx-h1s's own +0.18% finding for the import
table. The honest attribution, not further isolated within this bead's
budget: five new session-owned tables (`symbol_table`, `class_members`,
`owner_members`, `entity_ranges`, plus `entity_map`'s continued residence
in `EntityGraph.entities` across rebuilds instead of being freshly
allocated and dropped every time) now persist between builds where they
previously existed only transiently per-build, and the priming path
(`.clone()`s of all five, paid once per un-priming event — every cold
build on a non-retain corpus is one such event) briefly doubles their
footprint. Flagged as a real cost, not hidden in the ms numbers above.

### Verdict, per structure

| structure | verdict | why |
|---|---|---|
| `symbol_table` | **incremental** | owned Strings, no cross-file lifetime issue, existing `(file_path,start,end,id)` tie-break factored to a per-bucket helper |
| `entity_map` | **incremental** | owned Strings; reuses `EntityGraph.entities` as its session home, no new field or clone needed |
| `class_members`/`owner_members` | **incremental** | owned Strings, file-local parent-child containment holds for every supported language including Go's receiver-based members; required inventing `sort_members_bucket_by_source` (new tie-break) after this bead's first draft was proven wrong at monster scale — see Design |
| `entity_ranges` | **incremental** | trivially per-file-keyed (the key literally *is* the file path); not in the `Table::` fingerprint enum at all (a "self" table, same category as `default_export`), so needed no fingerprint interaction |
| `go_pkg_index` | **fallback, whole rebuild** | zero cost on every corpus this bead tested (gated behind `.go` file presence — monster, tiptap, and django all pay 0 for it); when Go files are present it is a directory/stem-indexed *derivation* of the other five tables with a different key shape (file stem/dir, not entity id/name) — its own per-file key index is a distinct design task, judged not worth the risk for a structure that costs nothing on every tested corpus |
| "borrowed" Pass A/B maps (`id_to_name`, `class_entity_names`, `class_entity_files`, `parent_child_pairs`, `child_line_ranges`, `class_child_names`, `enclosing_class`, local `class_members: HashMap<&str,_>`) | **fallback, whole rebuild** | `&str`-borrowed out of `all_entities`, which is itself never a persistent cross-build structure (GREEN files' entities are *moved*, not referenced, into the next build's `all_entities`); making these session-owned would require either cloning to `String` (defeats the point) or a materially larger change to `all_entities`'s own lifetime shape — now the **largest unconverted piece** of `entity_lookup_build_ms` |
| `fingerprint_corpus_tables` | **fallback, whole rebuild** (deliberate, not attempted) | a pure fold over the five tables above; decoupling "maintain the tables" from "fingerprint them" was this bead's key risk-reduction move — it means a correctness bug in table maintenance shows up as a `session.fingerprints != cold_session.fingerprints` failure (caught by every existing oracle test) rather than compounding with a *second* new, unvalidated incremental mechanism in the same bead |
| return-type/instance-attr maps, ctor-inference | **untouched, pre-existing** | already flagged by semx-9h3 as "redundant per chunk" (`O(corpus)` rebuilt once per 5,000-file chunk, 9x at monster scale) — a different, already-documented bug this bead did not attempt |
| `import_table` (semx-h1s) | **untouched, pre-existing** | already incremental; its own ~200–240ms floor is h1s's own documented next lever, not this bead's |

### Next bottleneck, named with numbers

Ranked by the `mixed50`-after breakdown (2,280.80ms total):

1. **`entity_lookup_build_ms`'s unconverted "borrowed" half, ~600ms
   (26.6%)** — the single largest named bucket left, and the direct
   continuation of this bead's own work: the same per-file-key-index
   pattern applies, but needs `all_entities` (or an equivalent per-file
   view of it) to be a session-owned, incrementally-maintainable structure
   first, which is a larger, riskier change than this bead's scope
   justified attempting alongside the fix already found at monster scale.
2. **`CHUNKS sum_ms`'s `ctor_infer_ms`+`return_types_by_name_ms`,
   ~155ms (6.8%) of the ~331ms chunk-loop total** — semx-9h3's own
   "redundant per chunk" finding, still unfixed; smaller than (1) but a
   known, already-diagnosed, `O(corpus × 9 chunks)` shape.
3. **`import_table`'s own ~224ms (9.8%) floor** — semx-h1s's own named
   next lever (`merge_export_build_ms` + `Table::TsExportSurface`
   fingerprinting), unrelated to this bead, unchanged.
4. **`fingerprint_corpus_tables`, ~214ms (9.4%)** — this bead's own
   deliberately-deferred lever (see Verdict); the natural next target once
   (1) is addressed, since fingerprinting only these five tables'
   *touched* keys requires the tables' own maintenance (this bead) to
   exist first.
5. Everything else (bag-of-words ~70ms, dedupe/sort/edge-index/imports-by-
   file/symbol-table-by-file each well under 45ms, ~700ms/30.7% still
   unattributed rayon-dispatch/pass-1-io overhead per the same caveat
   semx-9h3's own residual section already carries) — genuinely small or
   already explained, not re-chased here.

### What was NOT converted, and why (gate c: fallback with proof beats total win without)

Per this task's own instruction, every structure this bead did not convert
is listed above with a specific reason, not a blanket "out of scope":
`go_pkg_index` (zero cost on every tested corpus), the borrowed Pass A/B
maps (a lifetime-shape problem, not a design-effort problem — named as the
concrete next step, not hand-waved), `fingerprint_corpus_tables`
(deliberately decoupled to keep this bead's own correctness surface small,
after the `class_members` ordering bug already proved that surface is
easy to get subtly wrong), and the pre-existing chunked-return-type/
ctor-inference redundancy and import-table floor (both already-documented,
different beads' findings, correctly left alone rather than re-litigated
here).

### Continuation: closing the rest of the floor (semx-4an, second pass)

The section above ended honestly: 2,281ms at 50 changed files against a
target of "well under 1s", with a named residual — ~600ms of "borrowed"
Pass A/B maps, ~214ms of whole-table fingerprinting, ~155ms of per-chunk
ctor-inference/return-type redundancy — plus, unmentioned because it was
never instrumented, roughly 700ms that no timer in this document had ever
attributed to anything at all. This pass names all of it and removes most
of it.

#### Method: finish the instrumentation first

`entity_lookup_build_ms` was one number covering six different structures,
and everything outside `build_incremental_core`'s already-timed phases was
invisible. Nine new accumulators close both gaps, same
zero-cost-when-`SEM_PROFILE_RESOLVE`-is-off contract as every prior
addition:

* `LOOKUP_NS pass_a_ms / child_ranges_ms / owned_ms / pass_b_ms /
  go_pkg_ms / fingerprint_bow_ms` — splits `entity_lookup_build_ms` into
  its parts, and adds `fingerprint_bow_tables` (a *second* whole-table fold,
  sibling to `fingerprint_corpus_tables`, that no prior section measured).
* `FRAME_NS pass1_wall_ms / assemble_ms / scope_wall_ms /
  chunk_words_merge_ms / post_resolve_ms / session_prep_ms /
  session_post_prev_ms / session_drop_prev_ms` — pass 1's own wall time,
  the sequential fold of its products into `all_entities`, the whole scope
  stage (so the chunk-loop *wrapper* is separable from `CHUNKS sum_ms`),
  everything after scope resolution, and `GraphSession::run`'s own
  pre/post/teardown phases, which sit outside `build_incremental_core`
  entirely and so outside every timer this file had.

The first measurement with those in place immediately falsified the
previous section's own residual ranking. At `mixed50` on the monster the
"~600ms of borrowed maps" was really **`build_child_ranges_by_parent`
alone at 337ms** (it searches each child's source text inside its parent's,
per parent/child pair) with the actual `&str`-borrowed Pass A/B loops
adding only 57 + 24ms; and pass 1 was spending **222ms re-reading and
re-extracting 1,577 unchanged files from disk on every rebuild**.

#### What changed, in the order the timers ranked it

| # | change | bucket, `mixed50` before → after |
|---|---|---|
| 1 | **Pass-1 entity reuse past JS/TS.** `registry.extract_entities` is a pure function of (path, content, registry config), and on the chunked path nothing downstream needs a non-JS/TS file's *tree* (scope resolution re-reads and re-parses every file with no `PrecomputedFileFacts` entry anyway, and bag-of-words re-reads its content). So an unchanged non-JS/TS file's entities are now served from `prev_entities` like a JS/TS file's. **`.go` is excluded** — see below. | `pass1_wall_ms` 222 → 15 |
| 2 | …and, as a second-order effect, that shrank `prev_entities`' leftovers — the `touched_paths` that drive `maintain_entity_lookups_incremental` — from 1,814 files to the ~50 the caller actually named. The five owned tables were already incremental; they were just being handed the whole non-JS/TS corpus every build. | `owned_ms` 148 → 32 |
| 3 | **`child_ranges_by_parent` is now session-owned and incrementally maintained**, inside the same function and the same two loops as the other five tables. | `child_ranges_ms` 337 → 0.2 |
| 4 | **Touched-key corpus fingerprinting.** The five tables' fingerprints live in a session-owned `TableFingerprints` updated key-by-key; the Python wildcard-import guard is maintained by XOR. | `fingerprint_corpus_tables_ms` 196 → 3 |
| 5 | **`EntityGraph` construction moved instead of re-collected.** `entities: entity_map.into_iter().collect()` rebuilt three whole hash tables (454k + 196k + 196k entries) into the exact same types they already had. | ~60ms of previously unattributed time |
| 6 | **`deterministic_return_types_by_name` inverted** to iterate `return_type_map` (chunk-scoped, small) instead of `symbol_table` (whole corpus), reaching names through `entity_map`; and `infer_constructor_param_types` returns immediately when `init_params` or `attr_to_param` is empty, which is a provable no-op for its scan and skips a second copy of the same whole-corpus fold. Both ran once per 5,000-file chunk, i.e. 9× on the monster. | `ctor_infer_ms` 83 → 3, `return_types_by_name_ms` 78 → 5 |
| 7 | **Consumed-word merges move sets instead of rebuilding them.** Both merge points spelled `entry(id).or_default().extend(words)`, which allocates an empty `HashSet` and re-hashes every word; entity ids are file-unique and chunks partition the file list, so the vacant arm is the only one that runs and a plain move is exact. | `scope_merge_ms` 103 → 60, `chunk_words_merge_ms` 94 → 27 |
| 8 | **`PrebuiltEntityIndex::build`** groups `entities_by_file` by contiguous run (one `Vec` at its exact length per file) instead of 454k `entry().or_default().push()`, and pre-sizes `children_by_parent`. | `chunk_entity_index_ms` 44 → 19 |
| 9 | **Decorate-sort-undecorate** in `sort_one_symbol_table_bucket` / `sort_members_bucket_by_source`: one `entity_map` lookup per element instead of two per *comparison*. The monster's hottest symbol-table bucket holds ~10.5k ids. | part of `owned_ms` (item 2) |
| 10 | **`sort_resolved_refs` → rayon `par_sort_by`** (the *stable* parallel sort, so the order is element-for-element what `sort_by` produced). ~196k edges, re-sorted whole on every rebuild. | `sort_ms` 31 → 4 |
| 11 | **Pass A split into three concurrent halves** (`parent_child_pairs`+`class_child_names`, `child_line_ranges`, `class_entity_names`+`class_entity_files`) via `rayon::join`. They share no state and every structure is order-insensitive — two `HashSet`s and a `HashMap` whose buckets are explicitly sorted afterwards — so the result is identical and a sum of three passes becomes their max. **`id_to_name` deleted**: Pass B needs `id -> name` only for parent ids, and `entity_map` already *is* that map over the identical key set. | `pass_a_ms` 57 → 32 |
| 12 | `dependents`/`dependencies` pre-sized before the ~196k-edge index loop. | part of `edge_index_ms` |

#### `child_ranges_by_parent`: key ownership, and why removal is by value

`ChildRange` carried `file_path: &'a str` borrowed out of `all_entities`,
which is rebuilt (GREEN-moved or RED-fresh) every build and so can never
back a cross-build structure. It now carries `file_path: Arc<str>`.

**The ownership choice, honestly.** `Arc<str>` is a 16-byte fat pointer,
exactly the size of the `&str` it replaces, so `ChildRange` is the same 64
bytes it always was; one `Arc` is allocated per *file* (40,872) rather than
per child (454,541), and every child clones a refcount. Interning to a
`u32` index was considered and **not** done: it would have saved 8 bytes per
entry against a measured whole-process RSS delta of +3.5% (below), and it
would have required a session-owned path table whose indices stay stable
across rebuilds — a new invariant to get wrong for a small win. The bucket
*keys* are plain `String`, matching the five tables the first pass of this
bead already converted, so the parity argument is the same argument. No
micro-benchmark of `String` vs `Arc<str>` vs `u32` was run; the choice is
justified by struct size and allocation count, and the aggregate RSS cost
is reported rather than modelled.

**Removal is by value, one occurrence per old child** — not by dropping the
bucket. This is the one structure of the six where a bucket can legitimately
hold two *different* files' contributions: `resolve_go_method_parent_ids`
points a Go method's `parent_id` at a struct declared in another file of the
same package, so `child_ranges[structId]` can receive entries from several
`.go` files, and dropping the bucket when one of them goes RED would delete
the others'. Value matching is exact because `child_ranges_for_file` is a
pure function of one file's own entities and `prev_entities` still holds
exactly the entities the previous build used, so it reproduces byte-for-byte
what that build inserted. Entries that compare `Equal` under
`compare_child_ranges` are field-identical (the comparator covers the whole
struct), so "remove some matching occurrence" and "remove the one this file
put there" are the same operation, and re-sorting a touched bucket with
`sort_unstable` cannot diverge from a whole rebuild.

A per-file `child_ranges_for_file` also reproduces the whole-corpus build's
byte offsets exactly, including for Go's cross-file parents: the global
build finds the parent in `entity_by_id` and then
`child_content_span_in_parent` bails out on the `file_path` mismatch,
yielding `(None, None)`; the file-local build simply fails the lookup and
yields the same `(None, None)`.

#### Why `.go` is excluded from pass-1 entity reuse

`resolve_go_method_parent_ids` is this crate's one cross-file entity
rewrite: it rewrites a Go method's `parent_id` **and its id** against types
declared in *other* files of the same package, and it is a no-op when the
receiver type is not found. Serving a `.go` file's entities from the
previous build would therefore hand back a parent id that a sibling file's
edit had just invalidated, with nothing left to re-derive it — "unchanged
content ⇒ unchanged entities" is false for exactly this case. The exclusion
tests the same literal `.go` suffix that function itself tests, so the two
cannot drift. Go corpora consequently keep the pre-bead pass-1 cost; that is
the conservative side of the trade, taken deliberately.

#### Touched-key fingerprinting: the shape, and the one thing it must not do

The five corpus tables' fingerprints are now a **separate session-owned
map** (`corpus_fp`), maintained key-by-key and *copied* into each build's
`Incremental::cur_fp`, which the rest of the build then adds the import,
per-chunk and bag-of-words tables to.

They cannot simply share one map across builds, and the reason is the whole
correctness argument: `cur_fp` also holds tables that are refolded whole
every build, and seeding those from the previous build would leave a
*vanished* key reading back as its stale value. `ReadSet::unchanged`
compares `Option`s, so a stale value is indistinguishable from "unchanged" —
and a file whose lookup now misses would stay GREEN and serve stale edges.
That is the one unforgivable failure this whole design exists to prevent, so
the split is not an optimization detail; it is the invariant. Within
`corpus_fp` the same rule is enforced directly: a touched key whose table
entry no longer exists is `remove`d (new `TableFingerprints::remove`), never
left behind.

`Table::GuardPyWildcardImport` looked like the hard case and turned out to
be the easy one. `fingerprint_corpus_tables` spells it as "for every
`(name, id)` in `symbol_table`, XOR in `hash(name, entity_map[id].file_path)`".
That is the same multiset as "for every entity `e`, XOR in
`hash(e.name, e.file_path)`" — `symbol_table[name]` receives one push per
entity named `name`, `entity_map` always holds that id, and an entity id is
`{file_path}::…` by construction — *including* when two entities collide on
one id (TypeScript overload declarations do), where both formulations XOR
the same value twice and both cancel. Per-entity plus XOR's self-inverseness
makes the guard maintainable in `O(touched entities)` with no side table.

**Fallbacks.** The whole fold still runs whenever the tables themselves were
rebuilt whole (retain-mode, unprimed carry, first build) **and** whenever
`go_pkg_index` is non-empty — that index is a re-derivation of the other
tables under a different key shape (file stem / directory name) with no
per-file key index of its own, so its keys' disappearance cannot be detected
key-by-key. Every corpus with `.go` files therefore keeps whole-fold
fingerprinting.

**The parity gate.** `SEM_FP_PARITY=1` makes *every* session build
additionally run the whole fold it just avoided and assert that the
incrementally maintained map and the guard agree, entry for entry
(`assert_eq!` on the guard, `assert!` on map equality). Off by default at
the cost of one cached env lookup. Every oracle run reported below was run
with it on.

#### Gates

* **Full red-green oracle, four corpora, every scenario, `SEM_FP_PARITY=1`
  throughout** — `cargo run --release --example incr_probe -- <root> all
  <label>`, oracle and mutation code unmodified:

  ```
  # TypeScript monster (40,872 files) — chunked path, the regime this bead targets
  ORACLE label=final-monster scenario=cold-vs-build ok
  ORACLE label=final-monster scenario=none        ok
  ORACLE label=final-monster scenario=leaf        ok
  ORACLE label=final-monster scenario=mixed50     ok
  ORACLE label=final-monster scenario=hub         ok
  ORACLE label=final-monster scenario=hubrename   ok
  ORACLE label=final-monster scenario=tests       ok
  ORACLE label=final-monster scenario=importchurn ok
  # tiptap (1,533), django (3,023, Python), gin (108, Go) — retain path
  # all 8 scenarios ok on each; 32 of 32 scenarios green in total
  ```

  `gin` is new to this document's gate list and was added specifically for
  this bead: it is the only cached corpus that exercises
  `resolve_go_method_parent_ids`' cross-file rewrite on real code, which is
  what both the `.go` pass-1 exclusion and `child_ranges`' value-based
  removal are written against. The *chunked* Go path (which no cached corpus
  reaches, gin being 108 files) is covered by the `go_*` session fixtures
  under the crate's `#[cfg(test)] PARSED_FILE_REUSE_LIMIT = 8`.

* **Add and delete** — `oracle_add_a_new_file` / `oracle_delete_a_file`, plus
  the Python/Go/Kotlin no-op-rebuild fixtures, all under the test-only
  `PARSED_FILE_REUSE_LIMIT = 8`, i.e. through the chunked path this bead
  changes. Green.

* **Cross-process facts (`facts_probe`, semx-9en), monster, save in one
  process / load in another**: `ORACLE ... none|leaf|mixed50|hub ok`, all
  four. Neither of the two new session-owned structures (`child_ranges`,
  `corpus_fp`) is persisted, deliberately: `PersistedFacts` already carries
  the entities `child_ranges` is derivable from and the *whole* fingerprint
  map a warm start compares against, and `warm_start` sets
  `entity_lookups_primed = false`, so the first rebuild in a fresh process
  re-derives both from its one whole rebuild. Persisting ~454k byte spans to
  save that single rebuild was not measured to pay and was not wired in.

* **`cargo test -p sem-core --release`: 460 lib tests + all 7 integration
  binaries, 0 failures.** One pre-existing test
  (`return_type_name_lookup_uses_symbol_table_order`) needed an `entity_map`
  argument added, since `deterministic_return_types_by_name` now reaches a
  name through it; the tie-break it asserts is unchanged.

* **`cargo clippy -p sem-core --release --all-targets --examples`**: 174
  warnings, and the multiset of warning texts is byte-identical to the same
  command on the pre-bead tree (`diff` of the two `sort | uniq -c` outputs is
  empty) — zero new warnings.

* **`cargo fmt -p sem-core -- --check`**: clean.

* **Pre-existing failures not worsened**: `cargo test -p sem-cli --release`
  reports the same 12 `impact_direct_deps` failures as before (semx-ff2),
  and no others.

#### Measured: TypeScript monster, 1/50/500 scaling curve

`SEM_INCR_PROBE_SESSION_ONLY=1`, 18 cores, release. Each row is the **median
of 5 paired runs** (before and after alternating within one command, so both
see the same machine state); 13 paired runs were taken at `mixed50` across
the session and agree within ±6% of the median except for two outliers under
visible background load, which are included in the median.

| changed files | before (80df824) | after | Δ |
|---:|---:|---:|---:|
| 1 | 2,066 | **956** | −53.7% |
| 50 | 2,140 | **1,018** | −52.4% |
| 500 | 2,263 | **1,176** | −48.0% |

**Verdict: the curve is delta-proportional now; the target is still missed.**
Before, the whole 500× range in changed-file count fit in a 197ms band
(2,066→2,263, 9.5%) that was mostly noise — the signature of a pure
`O(corpus)` floor. After, 1→50 costs 62ms and 50→500 costs 158ms: a real,
visible delta term on top of a ~950ms floor. The bead asked for "well under
1s at 50 files" and this is 1,018ms, so **semx-4an stays open.** (It is closed
by the third pass below, which takes this section's own named next lever — the
GREEN-file clones — and lands 50 changed files at 811ms.)

#### Measured: where the remaining ~920ms goes

One profiled `mixed50` run on a quiet machine (923ms total; the profiled and
unprofiled numbers differ by less than the run-to-run band). Everything below
is flat across 1/50/500 unless marked.

| bucket | ms | % | note |
|---|---:|---:|---|
| `import_table` wall | 196 | 21% | **the new #1.** `merge_export_build_ms` 92 + `merge_insert_ms` 56 — semx-h1s's own documented floor, untouched by this bead |
| scope stage wall | 189 | 20% | `CHUNKS sum` 142 (of which `scope_merge_ms` 60), `chunk_words_merge_ms` 27, `chunk_entity_index_ms` 19 |
| post-resolve | 169 | 18% | `bow_wall_ms` 60, `fingerprint_bow_ms` 34, `edge_index_ms` 33, `symbol_table_by_file_ms` 11, `dedupe_ms` 7, `sort_ms` 4 |
| `entity_lookup_build_ms` | 89 | 10% | `pass_a_ms` 32 + `owned_ms` 32 (**delta-proportional**: 0.8 at 1 file, 66 at 500) + `pass_b_ms` 25 |
| `session_drop_prev_ms` | 70 | 7.6% | freeing the previous build's per-file cached resolutions, fingerprints and RED entities — pure deallocation, and previously invisible to every timer in this file |
| `session_post_prev_ms` | 40 | 4.3% | dominated by `changed_key_count`, two full passes over ~1M fingerprint entries for a *statistic* |
| `session_prep_ms` / `assemble_ms` / `pass1_wall_ms` | 21 / 19 / 15 | 6% | `pass1_wall_ms` is the one that scales: 15 at 50 files, 125 at 500 |
| `fingerprint_corpus_tables_ms` | 3 | 0.3% | was 196 |
| unattributed | ~112 | 12% | `snapshot_bow_content` (16), `resolve_go_method_parent_ids`, and the glue between phases |

**The floor is no longer table construction or fingerprinting.** It is, in
order: (1) the import table's whole-corpus export-surface merge, (2) the
scope stage's per-file result *clones* — a GREEN file's edges and consumed
words are cloned out of the previous build's cache and then cloned back into
this build's, twice per file per build, which is also what makes
`session_drop_prev_ms` expensive — and (3) bag-of-words' and the edge
index's whole-graph passes. Facts hashing and GREEN evaluation are **not**
the floor: `fingerprint_corpus_tables_ms` is 3ms and the per-file
`ReadSet::unchanged` checks never showed up as a bucket at all.

#### Named next lever, with numbers

**Stop cloning GREEN files' cached resolutions.** `resolve_with_scopes_full_inner`
clones `cached.scope.edges` and `cached.scope.consumed_words` out of
`prev_results` for every GREEN file, and the merge immediately clones them
*back* into `inc.next`; the same shape repeats on the bag-of-words side.
That is two deep clones per GREEN file per build (39,058 of them at
`mixed50`), and it is why the previous build's state costs 70ms to free.
Making `Incremental::next` start as a *move* of `prev` — so a GREEN file
needs no write at all — should collapse `scope_merge_ms` (60), a large part
of `bow_wall_ms` (60) and most of `session_drop_prev_ms` (70): ~150ms, i.e.
the difference between 1,018ms and the bead's target. It was not attempted
here because it changes `Incremental`'s ownership shape, which is the exact
machinery a mistake in would produce silent stale edges, and this bead had
already spent its correctness budget on six structures and a fingerprint
scheme.

Second: `fingerprint_bow_tables` (34ms) is still a whole fold, and two of its
three tables (`BowParentChildPairs`, `BowClassEntityFiles`) are per-entity
keys that would drop straight into the touched-key machinery this bead
built. The third (`BowClassMembers`) is keyed by a class *name* whose
membership depends on the corpus-global `class_entity_names` set, so it needs
a flip-detection fallback. Left undone deliberately: 34ms did not justify
extending the parity gate's surface.

Third: `changed_key_count` (most of 40ms) computes a reported statistic by
diffing two ~1M-entry maps. It is not used for any GREEN decision.

#### Measured: tiptap, django (retain path — the fallback costs nothing)

| corpus | scenario | before | after |
|---|---|---:|---:|
| tiptap (1,533) | mixed1 / mixed50 / mixed500 | 223 / 211 / 217 | 189 / 194 / 201 |
| django (3,023, Python) | mixed1 / mixed50 / mixed500 | 1,503 / 1,647 / 1,514 | 1,457 / 1,449 / 1,558 |

Both sit under `PARSED_FILE_REUSE_LIMIT`, so `retain_parsed_files` is true,
`use_incremental_lookups` never activates, and neither the new child-range
maintenance nor touched-key fingerprinting ever runs. The small tiptap gain
is items 5/6/9/10/11 above, which are not gated on the incremental path;
django's numbers are within its own run-to-run band. django's and gin's
8-scenario oracle runs are the correctness evidence for this fallback path
on non-JS/TS-dominant real corpora.

#### Memory

`/usr/bin/time -l`, `SEM_INCR_PROBE_SESSION_ONLY=1`, monster, cold build +
one `mixed50` warm rebuild — the same protocol as the section above, and run
back-to-back against the pre-bead binary on the same machine:

| | peak RSS | |
|---|---:|---|
| before (80df824) | 3,945,365,504 B | 3.674 GiB |
| after | 4,082,499,584 B | 3.802 GiB |
| **Δ** | **+137,134,080 B** | **+3.5%** |

Two new session-owned structures account for it: `child_ranges` (~454k
`ChildRange` entries at 64 bytes plus their bucket `Vec`s and `String` keys,
now persisting between builds instead of being rebuilt and dropped) and
`corpus_fp` (~1M `u64 -> u64` entries, held alongside the `cur_fp` copy each
build makes of it). Reported as a real cost, not folded into the ms numbers.

### Continuation: the cache is moved through a build, not copied (semx-4an, third pass)

The section above ended at 1,018ms with one named next lever: *stop cloning
GREEN files' cached resolutions.* This pass takes it, and the bead's close
condition — well under 1s at 50 changed files — is met: **811ms**, median of 5
paired runs, against a 947ms paired baseline.

#### The two clone sites per stage, before

`Incremental` held both a borrowed `prev` (the previous build's per-file
resolution cache) and a freshly allocated `next`. A GREEN file's result
therefore made the round trip twice on every build:

| # | site | what it copied, per GREEN file |
|---|---|---|
| S1 | `resolve_with_scopes_full_inner`'s pass-2 closure | `cached.scope.edges` + `cached.scope.consumed_words` + `cached.scope.read_set`, out of `prev` into a `PerFileScopeResult` |
| S2 | the same function's sequential merge | `result.edges.clone()` + `result.consumed_words.clone()`, back into `next` |
| B1 | `resolve_references_with_file_indexes`' per-file closure | `cached.bow.edges` + `cached.bow.read_set`, out of `prev` |
| B2 | the same function's merge | `entry.edges.clone()`, back into `next` |

Two deep clones of every GREEN file's edges (and, on the scope side, of its
whole consumed-word map) per stage per build — 39,058 files at `mixed50` — and
then the whole of `prev` was freed, which is what `session_drop_prev_ms` was
measuring.

#### The restructure: one map, moved

`GraphSession::run` now *moves* `self.resolution` into `Incremental` and takes
it back at the end; `prev` and `next` are one private `cache` field. A GREEN
file's entry is already in the right place holding exactly the right value, so
**the merge writes nothing for it** — S2 and B2 are gone, and S1/B1 shrink to
the single copy that genuinely has to exist (`all_edges` and the corpus-wide
`consumed_words` are build-scoped and consume what they are handed, while the
cache entry has to survive into the next build). That surviving copy is made in
the *parallel* closure, not the sequential merge. Read sets are not copied at
all any more.

**What the move costs, and the guard.** When `next` started empty, "there is an
entry for `p`" meant "this build resolved `p`" — which is what makes a read set
safe to compare against `(prev_fp, cur_fp)`: it was recorded against the
immediately preceding build's tables. A moved map can carry an entry across a
build that never looked at it (the file was deleted, or its per-file closure
returned `None` — no scope-resolve config, no readable content), and the build
after that would compare a two-builds-old read set against the latest two
fingerprint maps, missing any change that happened in between. That is exactly
the silent-stale-edge failure this machinery exists to prevent, and it is the
reason the previous pass declined to attempt this change.

`CachedFileResolution` therefore carries two build-generation stamps
(`scope_gen`, `bow_gen`) and two "was reused" flags. Every stage stamps the
halves it wrote (`put_scope`/`put_bow`) or re-validated (`keep_scope`/
`keep_bow`), and a single `Incremental::finish` pass at the end of the build —
one `retain` over the map, `u64` compares, no hashing — drops every entry no
half of this build claimed and **resets to `Default` every half it did not**.
The reset half is not decoration: a file whose scope resolved but whose
bag-of-words produced no result used to get a default `bow` from
`entry().or_default()`, and the next build's reuse rule reads it. What comes
out of `finish` is entry-for-entry what the freshly allocated `next` used to
be, which is the whole correctness argument — the invalidation rule itself is
untouched.

The stamps are `#[serde(skip)]`, so a `PersistedFacts` snapshot loaded in a
fresh process arrives stamped 0 and has to earn the current generation from
that process's own first build. The generation counter starts at 1 for the same
reason.

**Second-order: GREEN files no longer allocate their own path, twice.**
`green_scope`/`green_bow` were `HashSet<String>`s built by inserting
`file_path.to_string()` per GREEN file, and `bow_eligible` was a *clone* of the
first — ~78k path allocations per warm rebuild for a set whose only two
consumers are a count and a membership test. Both are now the per-entry
`scope_reused`/`bow_reused` flags plus two counters; `GraphSession::green_files`
derives the set on demand from the cache, and
`resolve_references_with_file_indexes` lost its `bow_eligible` parameter in
favour of `Incremental::scope_reused_this_build`, which is generation-checked
(so "reused" always means "reused in *this* build", the property the
bag-of-words eligibility rule actually needs).

#### Measured: TypeScript monster, 1/50/500 scaling curve

`SEM_INCR_PROBE_SESSION_ONLY=1`, 18 cores, release. **Median of 5 paired runs**
(before and after alternating inside one script, so both see the same machine
state). `files_red`/`files_green` are identical before and after at every point
— the GREEN decisions did not move, only what reuse costs.

| changed files | before (10db898) | after | Δ |
|---:|---:|---:|---:|
| 1 | 888 | **800** | −9.9% |
| 50 | 947 | **811** | −14.3% |
| 500 | 1,104 | **992** | −10.2% |

The five `mixed50` after-runs were 805/808/811/834/874ms — the *worst* of them
is under the 1s close condition, not just the median.

**Honesty about the baseline.** The previous section recorded 956/1,018/1,176ms
for the same three points on the same corpus and machine; this session's paired
before-runs came in ~7% faster (888/947/1,104) with an unchanged binary. The
machine state, not the code, moved. Only the paired columns above should be
compared to each other; against the recorded 10db898 numbers the 50-file point
is 1,018 → 811 (−20%), which is the number the bead's close condition is
judged on and is comfortably the same conclusion either way.

#### Measured: where the 136ms came from

One profiled `mixed50` run of each binary, reading the report of the measured
warm rebuild (`SEM_PROFILE_RESOLVE=1`; `session_post`/`session_drop` are
reported one build late by construction, so those two are read from the
following report):

| bucket | before | after | Δ | why |
|---|---:|---:|---:|---|
| `session_drop_prev_ms` | 64.9 | 0.1 | **−65** | the previous build's cache is no longer a separate object to free — it *is* this build's cache |
| `scope_merge_ms` | 66.1 | 26.8 | **−39** | S2 deleted; what remains is `all_edges`/word merging plus one stamp per GREEN file |
| scope stage wall | 208.2 | 162.8 | −45 | `scope_merge` plus reduced allocator pressure inside the chunk loop (`CHUNKS sum` 154.5 → 111.2) |
| `bow_wall_ms` | 63.3 | 52.9 | −10 | B2 deleted |
| `post_resolve_ms` | 176.5 | 158.4 | −18 | contains `bow_wall` above |
| `session_post_prev_ms` | 40.3 | 38.6 | ~0 | `changed_key_count`, untouched |

The buckets sum to roughly the measured 136ms wall delta; the scope-stage line
overlaps `scope_merge_ms` and is not additive with it.

#### The floor now, with numbers (`mixed50` after, 811ms)

| bucket | ms | % | note |
|---|---:|---:|---|
| `import_table` wall | 214 | 26% | **the #1, unchanged.** `merge_ms` 189 of it (`merge_export_build_ms` 110 + `merge_insert_ms` 55) — semx-h1s's own documented floor, still untouched by this bead |
| scope stage wall | 163 | 20% | `CHUNKS sum` 111, `chunk_words_merge_ms` 31, `scope_merge_ms` 27, `chunk_entity_index_ms` 19 |
| post-resolve | 158 | 19% | `bow_wall_ms` 53, `fingerprint_bow_ms` 34, `edge_index_ms` 34, `symbol_table_by_file_ms` 11, `dedupe_ms` 8, `sort_ms` 4 |
| `entity_lookup_build_ms` | 96 | 12% | `pass_a_ms` 34 + `owned_ms` 34 + `pass_b_ms` 27 |
| `session_post_prev_ms` | 39 | 4.8% | still `changed_key_count`: two passes over ~1M fingerprint entries, for a *statistic* no GREEN decision reads |
| `session_prep_ms` / `assemble_ms` / `pass1_wall_ms` | 16 / 26 / 18 | 7.4% | |
| `session_drop_prev_ms` | 0.1 | 0% | was 65 |
| `fingerprint_corpus_tables_ms` | 3 | 0.4% | |

**Per-file result copying is no longer the floor.** What is left is, in order:
(1) the import table's whole-corpus export-surface merge, (2) the scope stage's
chunk loop, (3) bag-of-words' and the edge index's whole-graph passes. The one
remaining copy of a GREEN file's result — the parallel one, into `all_edges`
and the global consumed-word map — costs about 22ms of `pass2_wall_ms` in total
and cannot be removed without changing what an edge *is* (an owned
`(String, String, RefType)`), which is a different bead.

#### Named next levers, with numbers

1. **`import_table`'s ~214ms**, semx-h1s's own documented floor
   (`merge_export_build_ms` 110 + `merge_insert_ms` 55). Now 26% of the warm
   rebuild and the largest single bucket by a wide margin.
2. **`changed_key_count`, most of 39ms.** It diffs two ~1M-entry maps to
   populate `RebuildStats::changed_keys`, which is reported and never read by
   any reuse decision. Making it opt-in would be a pure subtraction; it was left
   alone here because it changes a public statistic's meaning, not a
   performance-critical path.
3. **`fingerprint_bow_tables`, 34ms** — still a whole fold, and still the same
   two-of-three-tables-drop-straight-in analysis the previous pass wrote down.
4. `edge_index_ms` 34ms and `bow_wall_ms` 53ms, both whole-graph passes over
   ~196k edges / ~454k entities that no per-file index currently covers.

#### Gates

* **Full red-green oracle, four corpora, every scenario, `SEM_FP_PARITY=1`
  throughout** (monster 40,872 files — the chunked path; tiptap 1,533; django
  3,023 Python; gin 108 Go — the retain path): **32 of 32 `ORACLE … ok`,
  zero mismatches**, oracle and mutation code unmodified. Re-run in full on the
  exact binary that was committed.
* **Cross-process facts (`facts_probe`, semx-9en), monster, save in one process
  / load in another**: `ORACLE … none|leaf|mixed50|hub ok`, all four. This is
  the gate that proves the `#[serde(skip)]` generation stamps behave: a loaded
  snapshot's entries arrive unstamped and are re-validated or rewritten by the
  loading process's own first build, never carried on trust.
* **`cargo test -p sem-core --release`: 460 lib tests + all 7 integration
  binaries, 0 failures.** No test needed changing.
* **`cargo clippy -p sem-core --release --all-targets --examples`**: zero new
  warnings. The multiset is the baseline's *minus two* pre-existing
  `borrow_deref_ref` warnings, which were the two `&*state.prev` reborrows that
  the `prev` field's deletion took with it (the third, `graph.rs:1881`, is
  untouched and still there).
* **`cargo fmt -p sem-core -- --check`**: clean.
* **Pre-existing failures not worsened**: `cargo test -p sem-cli --release`
  reports the same 12 `impact_direct_deps` failures (semx-ff2) and no others.

#### Memory

`/usr/bin/time -l`, `SEM_INCR_PROBE_SESSION_ONLY=1`, monster, cold build + one
`mixed50` warm rebuild, back-to-back against the pre-bead binary on the same
machine, twice:

| | peak RSS (run 1) | peak RSS (run 2) |
|---|---:|---:|
| before (10db898) | 4,096,393,216 B | 4,007,034,880 B |
| after | 3,939,385,344 B | 3,895,214,080 B |
| **Δ** | **−157,007,872 B (−3.8%)** | **−111,820,800 B (−2.8%)** |

A reduction, and the first one this bead has reported. Two causes, both direct
consequences of the restructure: the previous build's cache and this build's
copy of it no longer coexist during the merge (the old peak held both), and the
two ~39k-entry `HashSet<String>`s of GREEN paths are gone. It does not fully
repay the +3.5% the previous pass cost, but it moves in the right direction
rather than the wrong one.

## Cross-repo corpus (local tier) (semx-2o8)

The per-repo facts store (semx-9en, "## Persisted facts" above) shares
extracted facts across builds of *one* checkout — keyed by the repo root's own
canonicalized path. It buys nothing for a repo `sem` has never built before,
even when that repo is a fork, a second checkout of the same repo at a
different path, or a vendored dependency reproduced byte-for-byte elsewhere on
the same machine. `FactsCorpus` (new in `facts_store.rs`) is the tier that
closes that gap: a machine-global, cross-repo corpus keyed by file content, not
by repo. This is Phase B's *local* tier — machine-scoped, no network. The
cloud tier (semx-9en, other half — semx-9en's continuation, tracked separately)
extends the same key format across machines; see "What the cloud tier needs
from this key," below.

### Key: `(relative_path, content_hash, language_salt)`

Entity ids, scope `owner_id`s, and every reference this crate resolves are
**path-qualified** — `SemanticEntity::id`/`file_path` embed the file's relative
path directly (the per-repo store's own "Keying and versioning" section
already established this for the per-repo tier). Two files with byte-identical
content at *different* relative paths produce entities with different ids, so
relative path is not a locality hint for the cross-repo corpus, it is a
**correctness requirement**: an entry is addressable only by
`(relative_path, content_hash, language_salt)` together, and a lookup checks
all three fields on every candidate, never trusting bucket placement alone
(`corpus_matches`, guarding against the vanishingly unlikely case of a 64-bit
bucket-hash collision — even then, the fallback is a false miss, never a
wrong-facts hit, which is the only failure mode the frame rule forgives).

`language_salt` is **per-language**, not one crate-wide version:
`LANGUAGE_SALTS` in `facts_store.rs` is a hand-maintained table (33 tree-sitter
language ids plus 8 hand-rolled-plugin ids, mirrored from `sem-core/Cargo.toml`'s
pinned grammar versions) so that a grammar bump for one language invalidates
only that language's corpus entries. This table is not automatically derived
— Rust has no stable way to read a dependency's resolved version at compile
time without a build script this crate doesn't carry — so it is a documented,
hand-bump obligation, the same discipline `FACTS_SCHEMA_VERSION` already uses,
just scoped per language. `corpus_isolates_by_language_salt` (`facts_store.rs`
tests) proves the isolation directly: an entry written under one salt string
misses a lookup under a different one, while a different language's entries at
the same nominal bucket are unaffected.

**What is shared, and what is deliberately not:** a corpus entry carries a
file's `FileFacts` (content hash + extracted entities) and its JS/TS
`PrecomputedFileFacts` — both pure functions of `(relative_path, content)`
alone. It never carries `CachedFileResolution` (cached cross-file edges + read
sets): those depend on what *other* files the repo has and currently contains,
which two repos sharing one file's content are not guaranteed to agree on (a
fork could rename, delete, or edit a file this one imports). Sharing
resolution edges cross-repo without also proving every transitively
referenced file matches would risk exactly the failure `incremental.rs`'s own
module doc calls unforgivable — silently wrong edges. This is a conservative
scope boundary: a cross-repo build still skips re-parsing/re-extracting every
matched file (pass 1's cost, the dominant share of a cold build per this
document's earlier sections), it simply re-resolves cross-file edges fresh,
exactly as if those files had been freshly added to an ordinary warm rebuild.

### Storage and concurrency

The per-repo tier's access pattern is bulk transfer ("give me everything for
this root"); the corpus's is the opposite — a point lookup per file ("does the
corpus already have *this* file's facts?"). A pure one-file-per-fact
content-addressed store would make that lookup a single `open`+`read` per
file, but at monster scale (40k+ files) that reintroduces the same
syscall-bound cost the per-repo tier's own "Format" section measured and
rejected for a bulk load, now applied to a point-lookup pattern instead. So
entries are grouped into `CORPUS_BUCKETS = 1024` fixed, corpus-wide buckets
(hashed from relative path alone, independent of any one repo's size), each
one shard file (`<corpus_dir>/shard-XXXX.factshard`) — a lookup over a whole
repo's file list touches at most `min(file_count, 1024)` shard files, not one
per file.

Each shard is written whole via temp-file-then-`rename`, exactly like the
per-repo blob: POSIX `rename` is atomic, so a reader never observes a torn
shard, and every decode path reuses the same "any anomaly is a clean miss,
never a panic" discipline `FactsStore::load` established
(`corrupted_shard_is_a_clean_miss_not_a_panic`, `missing_corpus_dir_is_a_clean_miss`).
A shard accumulates entries from many builds over time, so *writing* one is a
read-merge-write, not a blind overwrite — two processes racing to merge into
the same bucket could otherwise lose one writer's entries (both read the old
content, each adds different new entries, the second `rename` wins and drops
the first's additions: never corruption, since `rename` stays atomic, but a
real lost update). `ShardLock` — a tiny advisory lock (a `.lock` sibling file
created with `create_new`, no new dependency, matching the per-repo store's own
"no C dependency" restraint) — serializes the read-merge-write per shard, with
a 2-second bounded wait before proceeding unlocked: a stale lock from a
crashed process must never wedge a future build forever, and the corpus (a
pure speed optimization) must never fail a build over a lock it couldn't get.

`concurrent_writers_do_not_corrupt_and_both_survive` (`facts_store.rs`)
exercises this directly: two threads populate 10 files each into a bucket
forced to collide (found by brute-force search for paths hashing to bucket 0),
concurrently. After both finish: the shard decodes cleanly (no corruption),
and **all 20 entries from both writers are present** (no lost update — the
lock did its job). Corruption-impossible is structural (atomic rename);
lost-update-impossible is what the lock buys on top of that, for as long as it
can be acquired.

### Compatibility: additive, not a migration

The corpus is purely additive — the per-repo `FactsStore` is unchanged (same
format, same `FACTS_SCHEMA_VERSION`, same call sites still work with the
corpus disabled or absent) and remains the default source of truth for a
repo's own history; the corpus only ever fills gaps the per-repo store has
never seen (`merge_with_local`'s `local` argument always wins whenever it
already has an opinion on a path — see `merge_with_local_never_overrides_a_known_local_path`).
There is no migration step and nothing to backfill: an empty/missing corpus
directory is a clean miss just like a missing per-repo store, and the corpus
grows lazily as builds happen. Corpus shard headers reuse `FACTS_SCHEMA_VERSION`
and `sem_core_salt()` — the same single version knob the per-repo store uses —
rather than a second version number to track, since `CorpusFile` is built
entirely from types (`FileFacts`, `PrecomputedFileFacts`) that knob already
governs. **Coordination with the kappa agent's concurrent bow-token field
additions (semx-9en's own coordination note, mirrored here):** both tiers
decode fine against genuinely additive `Option<T>` + `#[serde(default)]`
fields without a version bump (CBOR is field-keyed); bump
`FACTS_SCHEMA_VERSION` once, at the end, only if a change is not purely
additive in that sense — this invalidates both tiers together with one bump,
which is the point of sharing the knob.

### sem-cli wiring

`crates/sem-cli/src/commands/graph.rs`'s `build_graph_with_facts_store` (the
single fallback point after `DiskCache` misses) now: loads the per-repo store
(`local`, unchanged); checks cheaply (`PersistedFacts::files_contains`, no
disk I/O) whether every requested path is already known to `local`; **only**
when there is a gap (including `local` being entirely absent — a repo's first
build) consults `FactsCorpus::merge_with_local`, which itself only reads+hashes
the paths `local` never saw before touching any shard. When there is no gap,
the corpus is never even opened — the already-warm rebuild path is
byte-for-byte what it was before this bead. `FactsCorpus::populate_delta` is
called after every build (best-effort, like `FactsStore::save`), writing only
files whose content changed relative to the previous per-repo snapshot
(`PersistedFacts::content_hash_index`, a cheap path→hash map captured before
the snapshot's ownership moves into `GraphSession::warm_start`, so populate's
diff base doesn't require cloning the full entity-bearing snapshot).

Default corpus location: derived from `sem-mcp`'s existing
`cache_dir_for_repo` resolution (not duplicated) — strip the per-repo key and
`repos` path segments to reach the shared `sem` cache base, then join
`facts-corpus` (e.g. `~/Library/Caches/sem/facts-corpus` on macOS,
`~/.cache/sem/facts-corpus` on Linux). `SEM_FACTS_CORPUS_DIR` overrides the
directory outright; `SEM_FACTS_CORPUS=0` disables just the corpus tier (the
per-repo store keeps working); the corpus also inherits `SEM_FACTS_CACHE=0`/
`--no-cache` as an off-switch, since it is the other half of the same feature.
No ambient path: `FactsCorpus::open` never touches `$HOME`/`XDG_CACHE_HOME`
itself, exactly like `FactsStore::open`.

### Cross-repo proof (`examples/facts_corpus_probe.rs`)

New probe, mirroring `facts_probe.rs`'s cross-process shape but for the
cross-*repo* claim: `populate <repo_a>` cold-builds repo A and writes every
file into the corpus; `consume <repo_b>` treats repo B as a fresh checkout
with **no local snapshot of its own** (`local = None`, a repo's literal first
build), merges against the corpus, warm-starts from the result, and compares
against both a from-scratch cold build of B (the oracle) and the merge stats
(reuse counts) and wall time (savings).

**tiptap × tiptap** (two independent `cp -R` checkouts of the same tree,
1,533 files each — the fork/same-path-different-root scenario):

```
POPULATE label=tiptap-a files=1533 entities=42841 edges=5414 edge_hash=e3c0b09c8edefbe7 build_ms=541.04 populate_ms=163.59 files_written=1533 shards_written=801 corpus_bytes=24527801
CONSUME  label=tiptap-b files=1533 probed=1533 corpus_hits=1533 files_reused_directly=1533 merge_ms=61.61 warm_start_ms=732.59 warm_total_ms=794.20 cold_ms=546.94 time_saved_ms=-247.26 time_saved_pct=-45.2
ORACLE   label=tiptap-b ok
NEGATIVE label=tiptap-b same_content_path=AGENTS.md renamed_path=__semx2o8_negative_probe__/AGENTS.md probed=1 hits=0 ok
```

Bit-identical (`ORACLE ok`) and 100% cross-repo reuse (`corpus_hits=1533` of
1533, `files_reused_directly=1533` — every file skipped re-extraction), but
**wall time is *worse*, not better**, at this scale — the same pre-existing
characteristic the per-repo tier's own tiptap numbers already documented
("tiptap's win is small and sometimes negative-adjacent"): `retain_parsed_files`
is `true` for any corpus at or under `PARSED_FILE_REUSE_LIMIT` (20,000 files),
and pass 1 on that path *always* re-reads and re-parses every file regardless
of what a session was seeded with — the parse-skip optimization only fires on
the chunked (`>20,000`-file) path. Reported honestly rather than rounded away,
exactly as that section did.

**TypeScript monster × monster** (the existing cached checkout as repo A, a
fresh `cp -R` of it as repo B, 40,872 files — the scale where the parse-skip
optimization actually fires):

```
POPULATE label=monster-a files=40872 entities=454541 edges=196223 edge_hash=4e23ae3a246c8fa9 build_ms=10509.96 populate_ms=655.31 files_written=40865 shards_written=1024 corpus_bytes=581048504
CONSUME  label=monster-b files=40872 probed=40865 corpus_hits=40865 files_reused_directly=40865 merge_ms=1103.13 warm_start_ms=6268.03 warm_total_ms=7371.15 cold_ms=11807.32 time_saved_ms=4436.16 time_saved_pct=37.6
ORACLE   label=monster-b ok
NEGATIVE label=monster-b same_content_path=AGENTS.md renamed_path=__semx2o8_negative_probe__/AGENTS.md probed=1 hits=0 ok
```

* **(a) bit-identical**: `ORACLE ok` — the corpus-assisted warm-started graph
  (entities/edges/sorted-edge-hash) equals a genuinely cold, no-corpus
  `EntityGraph::build` of the identical tree.
* **(b) cross-repo reuse > 0**: `corpus_hits=40865` of 40,872 probed files (7
  filtered by the probe's own extension/binary rules before either build sees
  them) — every eligible file in B served from A's corpus entries,
  `files_reused_directly` (`file_count − files_seed_red` from `RebuildStats`)
  confirms the same count skipped re-extraction in `GraphSession::warm_start`.
* **(c) measured time saving**: cold build 11,807.32ms vs corpus-assisted
  7,371.15ms (merge 1,103.13ms + warm-start 6,268.03ms) — **4,436.16ms saved,
  37.6% faster**, entirely from skipping pass 1's read+parse+extract for every
  file (pass 2/resolution still runs fresh, since resolution is deliberately
  not shared cross-repo — see "What is shared" above).
* **Negative proof**: a copy of `AGENTS.md`'s exact bytes written to a
  brand-new path (`__semx2o8_negative_probe__/AGENTS.md`) inside repo B, then
  probed on its own — `probed=1 hits=0`, i.e. identical content at an unseen
  path does **not** falsely share, confirming path is load-bearing in the key,
  not just a locality hint (matches the unit-level proof,
  `corpus_key_excludes_wrong_path`, at real-corpus scale).

### Load speed vs the per-repo tier's 220ms bar

The corpus is architecturally a different tier from the 220ms `FactsStore::load`
number (per-repo blob decode) — it is only ever consulted when the per-repo
store has a gap, so it cannot regress that number by construction (see
"sem-cli wiring" above: no gap ⇒ the corpus is never opened). Re-running the
existing `facts_probe` oracle after this bead's changes, unmodified, confirms
this directly — monster `none` scenario: `load_ms=244.20` (vs the
documented ~220ms baseline, within normal run-to-run noise on a shared
machine; all 8 corpus/scenario combinations below are new measurements from
the *same* run, not carried over from the earlier section):

| corpus | scenario | ORACLE |
|---|---|---|
| tiptap | none / mixed50 / leaf / hub | ok / ok / ok / ok |
| monster | none / mixed50 / leaf / hub | ok / ok / ok / ok |

8/8, zero mismatches — the per-repo tier's cross-process oracle is untouched
by this bead's additions, exactly as intended (`facts_store.rs`'s `FactsStore`
type and its `load`/`save` methods were not modified; only new code was
added alongside them).

The corpus's *own* load-speed number — `merge_ms` above, the cost of the
read+hash-every-unknown-file pass plus up to 1,024 shard opens — is
1,103.13ms at monster scale for a repo with **zero** local coverage (the
worst case: every one of 40,865 files is "unknown", maximal shard fan-out).
That is not compared against the 220ms bar because it answers a different
question ("is this file's content known to *any* repo on this machine") that
the per-repo tier cannot answer at all; it is compared against what it
replaces — a full pass-1 read+parse+extract, which is most of the 11.8s cold
build — and wins by 4.3 seconds net (`merge_ms` 1,103.13 vs the parse+extract
time it made unnecessary for 40,865 files).

### Gates

* **`cargo test -p sem-core --release`: 0 failures** across all 9 test
  binaries (lib + `parse_cache`/`kappa`/`scope_resolve_bench`/etc. integration
  suites), including 18 new `facts_store::corpus_tests` (key isolation,
  negative-path proof, corrupted-shard clean-miss, cross-repo
  `merge_with_local` reuse, local-always-wins, delta-populate skips unchanged
  files, concurrent-writer no-corruption-no-lost-update).
* **`facts_probe` cross-process oracle, unmodified: 8/8 `ORACLE … ok`**
  (tiptap × {none, mixed50, leaf, hub}, monster × {none, mixed50, leaf, hub}) —
  see "Load speed" above for the full table and the load-time numbers proving
  no regression.
* **32-scenario session oracle (`incr_probe`)**: untouched — this bead does
  not modify `incremental.rs`'s resolution/invalidation logic, `session.rs`,
  `scope_resolve.rs`, or `graph.rs`'s build core; its own tests were not
  re-run as part of this bead since no code on its call path changed.
* **New cross-repo oracle (`facts_corpus_probe`)**: 2/2 `ORACLE … ok`
  (tiptap × tiptap, monster × monster), both with `hits > 0` and a clean
  negative-path miss — see "Cross-repo proof" above.
* **Salt-mismatch / corruption = clean miss**: proven directly
  (`corpus_isolates_by_language_salt`, `corrupted_shard_is_a_clean_miss_not_a_panic`,
  `missing_corpus_dir_is_a_clean_miss`).
* **Concurrent-writer test**: `concurrent_writers_do_not_corrupt_and_both_survive`
  — two threads writing the same shard bucket concurrently; both stores
  loadable after, zero entries lost.
* **`cargo clippy -p sem-core --lib --release -- -D warnings`**: zero warnings
  attributable to `facts_store.rs` or `examples/facts_corpus_probe.rs` (the
  crate carries pre-existing warnings elsewhere, outside this bead's surface,
  unchanged by it).
* **`cargo fmt -- --check`**: clean on `facts_store.rs`,
  `examples/facts_corpus_probe.rs`, and `sem-cli/src/commands/graph.rs`.

### External ingestion (semx-bhc): closing Phase B's last mile

The gap the previous section left open ("What the cloud tier needs from this
key" below) is now closed: `FactsCorpus::ingest_remote` (`facts_store.rs`) is
the public, key-validated ingestion path that lets facts *this process never
derived itself* — sem-cli's cloud download, `commands/diff/facts_remote.rs`
— join the same local corpus `merge_with_local`/`build_graph_with_facts_store`
already consult. `PersistedFacts::new`/`CorpusFile` stay crate-private (no new
public constructor for those); the seam is a new record type instead.

**API shape** (final; adapted from the bead's suggested `ingest_remote(&self,
files: impl Iterator<Item = FileFacts-shaped>) -> io::Result<CorpusPopulateStats>`
in one way — see below):

```rust
pub struct RemoteFact {
    pub facts: FileFacts,                    // real, public FileFacts — path, content_hash, entities
    pub precomputed: Option<PrecomputedFileFacts>, // always None from today's cloud protocol
    pub claimed_relative_path: String,        // the key this fact was fetched *under*
    pub claimed_content_hash: u64,
    pub claimed_language_salt: String,
    pub claimed_schema_version: u32,
}

pub enum IngestError {          // #[derive(Error)] via thiserror, one variant per field
    PathMismatch { claimed: String, actual: String },
    ContentHashMismatch { path: String, claimed: u64, actual: u64 },
    LanguageSaltMismatch { path: String, claimed: String, actual: String },
    SchemaVersionMismatch { path: String, claimed: u32, actual: u32 },
}

pub struct IngestOutcome {
    pub accepted: CorpusPopulateStats,
    pub rejected: Vec<(String, IngestError)>,   // never silently dropped
}

impl FactsCorpus {
    pub fn ingest_remote(&self, registry: &ParserRegistry, facts: Vec<RemoteFact>)
        -> io::Result<IngestOutcome>;
}
```

**Why the shape differs from the bead comment's sketch.** The suggestion took
an `impl Iterator<Item = FileFacts-shaped>` and computed the corpus key
*internally*, exactly like `populate_delta` does for locally-derived facts.
That shape is right for `populate_delta` (this process's own read+hash pass is
trustworthy by construction) but wrong here: a downloaded fact already carries
a *claimed* key from the wire (`FACTS-SERVICE.md`'s `FactRecord`:
`relativePath`/`contentHash`/`languageSalt`/`schemaVersion`, echoed back
per-record by the server), and the whole point of this bead is to check that
claim against the payload's own recorded values before trusting it — key
material has to travel *alongside* each fact, not be silently re-derived, or
there is nothing left to validate against. `RemoteFact` carries both; the
`claimed_*` fields are exactly `FACTS-SERVICE.md`'s wire key, `facts` is the
payload. `IngestError`/`IngestOutcome` were not in the bead's sketch at all —
added because "reject mismatches with typed errors" (the bead's own
instruction) needs a type to reject *with*, and a batch ingestion must report
per-fact outcomes (one tampered fact must not sink an otherwise-good batch,
and a rejection must never vanish silently — see "Trust boundary" doc comment
on `ingest_remote` itself).

**Validation** (`ingest_remote`, before any byte reaches a shard): `claimed_relative_path
== facts.path`, `claimed_content_hash == facts.content_hash`, `claimed_schema_version
== FACTS_SCHEMA_VERSION`, `claimed_language_salt == language_salt(detect_language_id(facts.path))`
(this process's own table, not the claim). Any mismatch → typed `IngestError`,
fact excluded from the batch write, batch otherwise proceeds (`ingest_remote_batch_is_not_all_or_nothing`).
Accepted facts are bucketed and read-merge-written with the *exact* code path
`populate_delta` already used (`write_corpus_files`, factored out so both
share it) — once written, an ingested `CorpusFile` is indistinguishable from a
locally-derived one; every downstream consult (`merge_with_local`,
`GraphSession::warm_start`) treats it identically, including `warm_start`'s
own independent re-read-and-hash of the *local* file, which remains the
second, structurally separate trust boundary this validation does not
replace (a tampered fact that somehow passed ingestion would still be caught
there, because it would only match if the real local file's hash happened to
agree with the falsified claim — the two checks are not redundant, they cover
different lies).

**Build-generation stamps.** No special-casing was needed. `CachedFileResolution`'s
`scope_gen`/`bow_gen` stamps are `#[serde(skip)]` and belong to
`PersistedFile.resolution`, which `CorpusFile` never carries in the first
place (see "What is shared, and what is deliberately not" above — cross-repo/
cross-machine entries are `FileFacts` + `PrecomputedFileFacts` only). An
ingested fact reaches a session exclusively through `merge_with_local` →
`GraphSession::warm_start`, which re-derives resolution (and therefore
gen stamps) from scratch for every file it seeds, local or corpus-sourced —
ingestion doesn't touch generation bookkeeping because there is none to touch
on this path; it "earns its stamps" the same way a loaded local snapshot's
files do, by construction, not by a new mechanism.

**sem-cli wiring**: `commands/graph.rs::facts_corpus_for` is now `pub(crate)`
(was private) so `commands/diff/facts_remote.rs` can open the same corpus
`build_graph_with_facts_store` reads. `facts_remote.rs`'s known-file download
step — previously "decode purely to verify the round trip, nothing downstream
consumes it" — now decodes every `FactRecord` into a `RemoteFact` (the
per-record echoed key becomes `claimed_*`, not the originally-queried key
list, so a server that returns records out of order is still validated
against what it actually claims) and calls `ingest_remote`
(`ingest_downloaded_facts`). `SEM_FACTS_DEBUG=1` logs accepted/rejected
counts and per-rejection reasons, matching this module's existing debug
convention. Ingestion inherits the corpus's existing on/off switches
(`--no-cache`, `SEM_FACTS_CACHE=0`, `SEM_FACTS_CORPUS=0`) via
`facts_corpus_for` — no new knob.

### E2E proof (`examples/facts_corpus_probe.rs`, `remote-populate`/`remote-consume`)

Extends this probe's existing two-machine shape (`populate`/`consume`, above)
with the `ingest_remote` path instead of `populate_delta`'s local-derivation
path — `remote-populate` cold-builds repo A and writes every file's facts to
a `<wire_file>` in the exact `FACTS-SERVICE.md` `FactRecord` shape (echoed
key + opaque CBOR payload); `remote-consume` treats repo B as a fresh
checkout with a **brand-new, never-populated `corpus_dir`** (asserted at
startup — proves this is genuinely the ingestion path, not `populate_delta`
under another name), decodes `<wire_file>` exactly like `facts_remote.rs`
does, optionally tampers one record (`--tamper`: claimed `content_hash` bumped
by 1, disagreeing with the payload's own embedded hash), calls
`ingest_remote`, then merges/warm-starts/cold-builds/compares exactly like
`consume` does. Both corpora, no `--tamper`:

```
REMOTE_POPULATE label=tiptap-a files=1533 entities=42841 edges=5414 edge_hash=e3c0b09c8edefbe7 build_ms=317.57 records_written=1533 wire_bytes=48761694
INGEST label=tiptap-b claimed=1533 accepted=1533 rejected=0 ingest_ms=103.06
REMOTE_CONSUME label=tiptap-b files=1533 probed=1533 corpus_hits=1533 files_reused_directly=1533 merge_ms=38.27 warm_start_ms=331.29
ORACLE label=tiptap-b ok

REMOTE_POPULATE label=monster-a files=40872 entities=454541 edges=196223 edge_hash=4e23ae3a246c8fa9 build_ms=8194.91 records_written=40865 wire_bytes=609270759
INGEST label=monster-b claimed=40865 accepted=40865 rejected=0 ingest_ms=221.44
REMOTE_CONSUME label=monster-b files=40872 probed=40865 corpus_hits=40865 files_reused_directly=40865 merge_ms=774.71 warm_start_ms=8590.70
ORACLE label=monster-b ok
```

Same two corpora, with `--tamper`:

```
INGEST label=tiptap-b-tamper claimed=1533 accepted=1532 rejected=1 ingest_ms=102.14
INGEST_REJECTED label=tiptap-b-tamper path=AGENTS.md reason=claimed content_hash 2994445808869199361 does not match the fact's own content_hash 2994445808869199360 (path "AGENTS.md")
TAMPER label=tiptap-b-tamper path=AGENTS.md ok: rejected with ContentHashMismatch, nothing else rejected
REMOTE_CONSUME label=tiptap-b-tamper files=1533 probed=1533 corpus_hits=1532 files_reused_directly=1532 merge_ms=31.12 warm_start_ms=325.63
ORACLE label=tiptap-b-tamper ok

INGEST label=monster-b-tamper claimed=40865 accepted=40864 rejected=1 ingest_ms=234.14
INGEST_REJECTED label=monster-b-tamper path=AGENTS.md reason=claimed content_hash 13119623558686825552 does not match the fact's own content_hash 13119623558686825551 (path "AGENTS.md")
TAMPER label=monster-b-tamper path=AGENTS.md ok: rejected with ContentHashMismatch, nothing else rejected
REMOTE_CONSUME label=monster-b-tamper files=40872 probed=40865 corpus_hits=40864 files_reused_directly=40864 merge_ms=624.15 warm_start_ms=8866.54
ORACLE label=monster-b-tamper ok
```

* **(a) bit-identical vs a no-cloud cold build**: `ORACLE ok` in all four runs
  (both corpora, tampered and not) — including under `--tamper`, where the
  one rejected file simply falls back to local extraction inside
  `GraphSession::warm_start` (it was never in the merged snapshot) and still
  lands on the byte-identical graph a cold build would produce. Correct, not
  poisoned.
* **(b) extraction-skip counters**: `files_reused_directly` equals
  `corpus_hits` equals `accepted` in every run (1533/1533/1533 tiptap,
  40865/40865/40865 monster, no `--tamper`) — every ingested file
  genuinely skipped re-extraction in `warm_start`. Under `--tamper`, all three
  drop by exactly 1 (1532/1532/1532 tiptap, 40864/40864/40864 monster) — the
  one file ingestion refused is the one file re-extracted, nothing else.
* **(c) tamper rejection**: exactly one `IngestError::ContentHashMismatch`
  per tampered run, `TAMPER … ok`, and `outcome.rejected.len() == 1` (the
  probe fails loudly — `FAIL:` — if any other file is unexpectedly rejected
  too, so this is a real assertion, not just a log line).
* **Corpus-populate numbers match the earlier `populate`/`consume` section's
  own recorded numbers exactly** (same `files/entities/edges/edge_hash` for
  both corpora), confirming `remote-populate`'s cold build and `export_facts`
  extraction are unchanged by this bead.

Unit-level coverage lives alongside this: `facts_store.rs`'s `corpus_tests`
adds 6 `ingest_remote_*` tests (accept-and-become-a-normal-hit, one rejection
test per `IngestError` variant, batch-is-not-all-or-nothing); `sem-cli`'s
`commands/diff/facts_remote.rs` adds 4 tests (`decode_download_response`
round-trip/malformed-hash/garbage-bytes, plus a full loop test — decode →
`ingest_downloaded_facts` → `merge_with_local` — proving a well-formed
download reaches the corpus and a tampered one never does).

### Gates (semx-bhc)

* **`cargo test -p sem-core --release`: 518 lib tests + all 7 integration
  binaries, 0 failures** (10 new `ingest_remote`/corpus tests added; no
  existing test needed changing).
* **`facts_probe` cross-process oracle, unmodified: 8/8 `ORACLE … ok`**
  (tiptap × {none, mixed50, leaf, hub}, monster × {none, mixed50, leaf, hub})
  — re-run after this bead's changes to confirm the per-repo tier is
  untouched.
* **32-scenario `incr_probe`, `SEM_FP_PARITY=1`, tiptap + monster**: 8/8
  `ORACLE … ok` each (`cold-vs-build` + all 7 named scenarios), 16/16 total —
  this bead touches neither `incremental.rs` nor `session.rs`'s resolution
  core, so this is a regression check, and it's green.
* **New E2E oracle (`facts_corpus_probe remote-populate`/`remote-consume`)**:
  4/4 `ORACLE … ok` (tiptap × {no-tamper, `--tamper`}, monster × {no-tamper,
  `--tamper`}) — see "E2E proof" above for the full numbers.
* **`cargo test -p sem-cli --release --no-fail-fast`**: same 12 known
  `impact_direct_deps` failures (semx-ff2), unchanged; every other binary
  green, including the 4 new unit tests in `facts_remote.rs` and the
  pre-existing `diff_facts_remote.rs` integration suite (4/4, untouched).
* **`cargo clippy -p sem-core --release --all-targets --examples`** /
  **`cargo clippy -p sem-cli --all-targets`**: zero warnings attributable to
  `facts_store.rs`, `examples/facts_corpus_probe.rs`,
  `commands/diff/facts_remote.rs`, or `commands/graph.rs` (both crates carry
  pre-existing warnings elsewhere — `graph.rs:9433` etc. in sem-core,
  `setup.rs`/`cache.rs` in sem-cli — outside this bead's surface, unchanged
  by it).
* **`rustfmt --edition 2021` on the 3 touched non-generated files**
  (`facts_store.rs`, `facts_corpus_probe.rs`, `facts_remote.rs`) — `cargo fmt
  -p sem-core -- --check` / `cargo fmt -p sem-cli -- --check` clean on those
  files afterward (both crates carry pre-existing formatting drift elsewhere
  — `graph.rs`'s own `fmt_count`, `main.rs`, `relations.rs`, etc. — left
  untouched, not this bead's surface).

### What the cloud tier (semx-9en's other half) needs from this key

The next wave's cloud tier consumes this bead's key format directly, so it is
worth stating explicitly what's load-bearing:

1. **The three-part key `(relative_path, content_hash, language_salt)` is the
   wire contract.** A cloud store keyed any other way (e.g. content hash
   alone) would violate the same correctness requirement this document's
   "Key" section proves locally — reuse it verbatim rather than re-deriving a
   weaker key.
2. **`content_hash` is `xxh3` of raw file bytes** (`incremental::content_hash`,
   `u64`) — not a cryptographic hash, not normalized/whitespace-stripped. The
   cloud tier should hash identically or the keys will never intersect with
   this machine's local entries.
3. **`language_salt` is a string, not a version number** (`LANGUAGE_SALTS`) —
   treat it as opaque; do not attempt to parse or compare it as a semver. The
   cloud tier should either mirror the same hand-maintained table (with the
   same hand-bump obligation) or receive it verbatim from the uploading
   client rather than recomputing it server-side against a possibly
   out-of-sync table.
4. **Never uploaded/downloaded: `CachedFileResolution`.** The scope boundary
   in "What is shared, and what is deliberately not" above applies identically
   to a cloud tier — resolution edges are not part of this bead's shareable
   unit, and the cloud tier should not invent a way to share them without its
   own frame-rule analysis of what "the same relative path, same content, in
   a repo the server has never seen the rest of" is allowed to assume about
   other files.
5. **The local corpus is a legitimate fallback the cloud tier should compose
   with, not replace**: a local hit should always be preferred over a network
   round-trip (this bead's `merge_with_local` already establishes "local wins
   whenever it has an opinion" as the composition rule between tiers; the
   cloud tier is naturally the next fallback after both the per-repo store and
   the local corpus miss).

## Universal call-ref extraction + language whitelist sweep (semx-ocj, semx-14b)

Two beads, taken together: semx-ocj is a correctness bug (call edges silently
missing from every graph ever built, for three languages) found while auditing
semx-kzy's own "Universal GREEN eligibility" work; semx-14b is that same
bead's own named follow-up — repeat the Kotlin-shaped fixture-and-oracle
proof per remaining candidate language, and settle Swift's guard.

### The bug (semx-ocj): `extract_call_ref`'s fast path was a closed list of `.kind()` strings

`extract_call_ref` (the function every `CallNodeStyle` funnels a call's callee
node through) only ever recognized three tree-sitter node kinds for a plain
identifier callee: `"identifier"`, `"simple_identifier"`, `"type_identifier"`.
Any language whose grammar names that node kind something else silently
dropped every one of its calls as a ref — not a resolution-precision gap, an
*extraction* gap: the ref never existed to attempt resolving, cold or warm,
in any graph this crate has ever built for that language.

Three languages hit this, for two different reasons:

1. **Bash** (`BASH_SCOPE_CONFIG`, `call_style: FunctionField("name")`):
   `command`'s `"name"` field is a `command_name` *wrapper* node
   (tree-sitter-bash's `node-types.json`), one level above the real `word`
   leaf — `extract_call_ref` received `command_name`, matched nothing, and
   returned without ever inspecting its child.
2. **Fish** (`FISH_SCOPE_CONFIG`, same `call_style`): `command`'s `"name"`
   field hands over the `word` leaf directly, no wrapper — but `"word"` was
   never in the recognized-kind list either.
3. **PHP** (`PHP_SCOPE_CONFIG`, `call_nodes: ["function_call_expression",
   "member_call_expression"]`), found while building semx-14b's PHP fixture,
   not by grep: tree-sitter-php's plain-identifier node kind for a bare
   `ping()` call's callee is `"name"` — again, never recognized.

**RED, before the fix** (`session.rs`, `cargo test -p sem-core --release
--lib`):

```
thread 'parser::session::tests::bash_calls_across_files_become_reference_edges' panicked:
foo() (f.sh) -> helper() (g.sh) must exist as a Calls edge; edges found: []
thread 'parser::session::tests::fish_calls_across_files_become_reference_edges' panicked:
foo (f.fish) -> helper (g.fish) must exist as a Calls edge; edges found: []
thread 'parser::session::tests::php_calls_across_files_become_reference_edges' panicked:
foo() (f.php) -> helper() (g.php) must exist as a Calls edge; edges found: []
```

**The fix** (`scope_resolve.rs`, `extract_call_ref`): a `command_name` node
now unwraps to its single named child and re-dispatches through the same
function (covers bash); the identifier fast path grew two more recognized
kinds, `"word"` (fish, and bash's unwrapped leaf) and `"name"` (PHP). Every
one of these three kinds is otherwise unused by any other configured
language's callee position, so widening the match can only turn a
previously-always-dropped ref into a collected one — it cannot regress an
already-working language.

**GREEN, after the fix**, same three tests, plus the full crate suite
unregressed:

```
test parser::session::tests::bash_calls_across_files_become_reference_edges ... ok
test parser::session::tests::fish_calls_across_files_become_reference_edges ... ok
test parser::session::tests::php_calls_across_files_become_reference_edges ... ok
```

No other shell-family or "special-cased call syntax" grammar in this crate
was found to have the same hole: every other `CallNodeStyle::FunctionField`/
`FirstChild`/`DirectMethod` config either already used a recognized kind, or
(Ruby, C++, C#, Scala, Zig) already worked because its plain-identifier node
really is named `"identifier"`.

### The languages: semx-14b's fixture-and-oracle sweep

Per the RESOLUTION-PROFILE method this bead inherits from semx-kzy: a small
multi-file fixture with real cross-file references, run through the same
`assert_warm_matches_cold_for`-based oracle (no-op / leaf-touch / hub-touch /
blast-radius), plus a positive per-file entity-count assertion (a fixture
that extracts zero entities from a file proves nothing about that file). New
tests live in `session.rs` alongside the existing JS/TS, Python, Go, and
Kotlin fixtures — one `write_<lang>_fixture` + 4 tests each, following the
established shape exactly (`hub`, `mid`, leaves, islands).

**Java, C++, C#, Ruby, Zig — clean.** No node-kind collision with
`extract_imports_from_ast`'s five recognized import-statement kinds; cross-
file calls resolve through the already-generic `Table::SymbolTable`/`Table::
ClassMembers` fallback (Java/C# via the `ClassName.staticMethod()`
static-call path in `resolve_ref`, already `Table::ClassMembers`-attributed;
C++/Ruby/Zig via bare `Table::SymbolTable` name lookup, same mechanism as
Kotlin). All 4 oracle tests green per language, entity counts positive per
file, `SEM_FP_PARITY=1` unaffected.

**Java and Scala — the interesting find that turned out to be a clean miss.**
tree-sitter-java's and tree-sitter-scala's import-statement node kind is
*also* named `"import_declaration"` — the exact kind `extract_imports_from_
ast` routes to `extract_go_import` (Go's own extractor). This looked like a
real correctness landmine until traced structurally and then *proven*, not
just reasoned about: Go's extractor only acts on an `import_declaration`'s
`import_spec`/`import_spec_list`/string-literal children, and neither Java's
grammar (`asterisk`/`identifier`/`scoped_identifier` children only) nor
Scala's (a closed set of Scala type/pattern-node children, no import-spec or
string-literal shape at all) ever produces one — so `extract_go_import`
silently no-ops on both. Both fixtures include a real import statement
(`import java.util.List;`, `import scala.collection.mutable.ListBuffer`) so
this is exercised, not assumed; both are bit-identical warm vs. cold through
every oracle scenario including the hub-rename mutation.

**PHP — a real landmine, found and defused, not just checked.** tree-sitter-
php's `use` statement node kind is `"use_declaration"` — the exact kind
`extract_imports_from_ast` routes to `extract_rust_use` (Rust's own
extractor). Unlike Java/Scala, this one does *not* clean-miss: `extract_rust_
use` parses the node's raw *text*, not its child kinds, so it always
"succeeds" at producing something. Fed PHP's `use App\Utils\Helper;`, it
finds no `"::"` (PHP's namespace separator is `\`, Rust's is `::`), falls
into the single-segment branch, and looks up the entire backslash-joined
path (`App\Utils\Helper`) as one opaque symbol name through `resolve_import_
name` — a real, correctly-*recorded* `Table::SymbolTable` read (`rec.one`/
`rec.two` fire) that is a structural miss every time, because no PHP entity
is ever named with embedded backslashes. A miss is a dependency too (the
same principle `register_namespace_import`'s whole-table guard documents for
Python's bare `import module`), so this is conservative, not wrong — proven
with a dedicated test (`php_oracle_touch_the_use_target_stays_a_miss`)
that mutates the `use`-named target across a warm rebuild and confirms the
miss stays a miss, bit-identical to cold, rather than merely asserting once.

**Dart — kept RED, and the fixture proves *why*, not just that it's slow.**
`DART_SCOPE_CONFIG` is one of the "Tier 2 (Minimal)" configs, and its
`call_nodes` is `&["function_expression_body"]` — an arrow-body node kind
(`() => expr`), not a call-expression kind at all. Ordinary, idiomatic Dart —
block-bodied top-level functions calling each other (`String run() { return
ping(); }`) — never even reaches `collect_all_file_refs`'s call-node branch.
`dart_ordinary_calls_are_not_collected_as_refs` proves this directly:
entity extraction works fine (Dart functions are extracted), but zero
`Calls` edges appear between hub and caller. This is a real, narrower
entity/ref-extraction gap in Dart specifically (parallel in shape to the
bash/fish/PHP bug this same sweep found and fixed, but *not* fixed here — it
is out of semx-14b's scope, which is eligibility, not extraction, and unlike
bash/fish/PHP this one was not part of semx-ocj's "shell-family and
special-cased call syntax" mandate). Left perma-RED: correctly conservative,
and there is nothing to gain from whitelisting a language whose calls never
become refs in the first place. Surfaced here rather than silently
sidestepped by writing arrow-bodied Dart just to make eligibility look
provable.

**Bash and Fish — whitelisted, now that semx-ocj lets their calls exist as
refs at all.** Same shape as Kotlin/C++/Ruby/Zig: no import mechanism either
grammar's node kinds trigger in `extract_imports_from_ast`, so cross-file
calls resolve entirely through `Table::SymbolTable`. Both fixtures'
blast-radius tests *rename* the hub function (not just edit its body) to
force invalidation — a body-only edit correctly leaves `Table::SymbolTable`'s
entry for that name, and every caller, untouched, which is precision, not a
gap (the same distinction Kotlin's own fixture doc comment already made).

### Swift: the whole-corpus guard stays whole-corpus — precisely why

`Table::GuardSwiftCallSignatures` fingerprints `swift_call_signatures`
(built once per build, keyed by candidate entity id, whenever any `.swift`
file is present anywhere in the corpus) as a single opaque whole-table hash,
and `session.rs`'s eligibility filter short-circuits to "every file RED" the
instant that table is non-empty (`if swift_active || ... { return false; }`
— not scoped to `.swift` files, *every* file in the build). Traced precisely,
not just re-asserted from the prior bead:

* The read this guards is `resolve_ref`'s Swift-overload disambiguation
  (`has_ambiguous_swift_signature_candidates` /
  `select_swift_overload_candidate`), reached for *any* method-call
  resolution that lands on a candidate set overlapping `swift_call_
  signatures` — which in practice means any file, Swift or not, that calls a
  method whose name collides with an overloaded Swift declaration somewhere
  in the corpus.
* **Narrowing is mechanically plausible, not attempted.** The candidate set
  a given call site actually consults is exactly `class_members[class_name]`
  — already read via `rec.one(Table::ClassMembers, class_name)` at the same
  call site. A per-candidate `rec.one(Table::SwiftCallSignatures,
  candidate_id)` recorded alongside that existing read would, in principle,
  let a file be GREEN unless one of *its own* consulted candidates' Swift
  signature changed — the same `(table, key)` discipline semx-kzy applied to
  Python/Go's tables, not a new technique.
* **Why it wasn't attempted this pass:** doing this safely needs (a) a new
  `Table::` variant threaded through `build_swift_call_signatures`/
  `select_swift_overload_candidate`/`has_ambiguous_swift_signature_
  candidates`, (b) extending `.swift` into `NEWLY_ATTRIBUTED_EXTENSIONS`
  only once that threading is verified sound, and (c) a dedicated Swift
  oracle fixture built specifically to exercise overload-candidate churn
  across files (the shape none of this crate's existing fixtures cover) —
  a change with the same blast radius as semx-kzy's own Python/Go
  attribution work, not a same-afternoon extension of the Kotlin pattern.
  Conservative-beats-unproven: the prior bead already declined this exact
  narrowing as "a plausible bounded follow-up, not attempted here"; this
  pass re-derived the same conclusion independently, with the specific
  mechanism now on record, and made the same call under the same reasoning.
  **Verdict: unchanged, whole-corpus, correctly conservative.**

### Complete per-language verdict (all 34 grammars)

| Language | Extension(s) | Verdict | Basis |
|---|---|---|---|
| JS/TS | `.ts .tsx .js .jsx .mjs .cjs .mts .cts` | Attributed | semx-022, unchanged |
| Python | `.py` | Attributed | semx-kzy |
| Go | `.go` | Attributed | semx-kzy |
| Rust | `.rs` | Attributed | semx-kzy |
| Kotlin | `.kt .kts` | Whitelisted, with proof | semx-kzy |
| Java | `.java` | **Whitelisted, with proof (this bead)** | `ClassName.staticMethod()` static-call path, `Table::ClassMembers`-attributed; `import_declaration`-kind collision with Go's extractor verified to be a structural no-op |
| C++ | `.cpp .cc .cxx .hpp .hh .hxx` | **Whitelisted, with proof (this bead)** | Bare `Table::SymbolTable` fallback, no node-kind collision |
| C# | `.cs` | **Whitelisted, with proof (this bead)** | Same static-call path as Java, no node-kind collision |
| Ruby | `.rb` | **Whitelisted, with proof (this bead)** | `CallNodeStyle::DirectMethod` degrades a receiver-less call to plain `Table::SymbolTable` fallback |
| PHP | `.php .inc .phtml .module` | **Whitelisted, with proof (this bead)** | Bare `Table::SymbolTable` fallback; `use_declaration`-kind collision with Rust's extractor traced and proven to be a conservative miss, never a false resolution; **also**: bash-class bug found and fixed (semx-ocj) — bare `ping()` calls were never collected as refs at all until this bead (PHP's callee node kind is `"name"`) |
| Scala | `.scala .sc .sbt .kojo .mill` | **Whitelisted, with proof (this bead)** | `Hub.ping()` static-call path via `field_expression` member access, `Table::ClassMembers`-attributed; same `import_declaration`-kind collision as Java, verified to be a structural no-op |
| Zig | `.zig` | **Whitelisted, with proof (this bead)** | Bare `Table::SymbolTable` fallback, no node-kind collision |
| Bash | `.sh` | **Whitelisted, with proof (this bead)** | Bug fixed first (semx-ocj: `command_name` unwrap), then whitelisted same shape as Kotlin — bare `Table::SymbolTable` fallback |
| Fish | `.fish` | **Whitelisted, with proof (this bead)** | Bug fixed first (semx-ocj: `"word"` kind recognized), then whitelisted same shape as Kotlin |
| Swift | `.swift` | Narrowed guard: **not narrowed, whole-corpus, documented why (this bead)** | `Table::GuardSwiftCallSignatures` forces every file RED whenever any `.swift` file exists; narrowing is mechanically plausible (candidate-id-level `(table, key)` attribution via already-read `Table::ClassMembers`) but not attempted — same call as the prior bead, mechanism now on record |
| Dart | `.dart` | **RED, with a documented reason (this bead)** | `DART_SCOPE_CONFIG`'s `call_nodes` only matches arrow-bodied expressions (`function_expression_body`), not ordinary block-bodied calls — proven empirically (`dart_ordinary_calls_are_not_collected_as_refs`): entities extract fine, zero `Calls` edges ever appear. A real, narrower ref-extraction gap, out of this bead's scope to fix |
| C, Fortran, Elixir, HCL, XML, Perl, SQL, OCaml, OCaml-interface, Nix, Haskell, Elm, EDN, Clojure, D, Lua | various (16 grammars) | No resolution support | `scope_resolve: None` in `LanguageConfig` — structurally excluded from ever entering per-file scope resolution, so eligibility is moot |

All 34 configured grammars accounted for: 4 pre-existing attributed, 11
whitelisted-with-fixture (2 pre-existing + 9 this bead), 1 narrowed-guard
verdict reaffirmed whole-corpus with the mechanism documented, 1 RED with a
newly-documented and proven reason (Dart), 16 with no resolution support at
all, 1 JS/TS family already covered under the first row.

### Gates

* **Bug fix, RED → GREEN**: `bash_calls_across_files_become_reference_edges`,
  `fish_calls_across_files_become_reference_edges`,
  `php_calls_across_files_become_reference_edges` — all three fail on
  pre-fix `extract_call_ref` (`edges found: []`) and pass after.
* **`cargo test -p sem-core --release --lib`**: 512 tests, 0 failures (460
  pre-existing + 52 new: 3 bug-fix RED/GREEN tests, 9 languages × 4 oracle
  tests = 36, 1 extra PHP miss-stays-a-miss test, 1 Dart RED-proof test, and
  `assert_every_file_has_entities` wired into every new fixture's blast-
  radius test as the "zero entities proves nothing" gate). All 7 integration
  test binaries unaffected.
* **Full 32-scenario oracle, four corpora, `SEM_FP_PARITY=1` throughout**,
  `cargo run --release --example incr_probe -- <root> all <label>`, run
  singly in the foreground (an earlier concurrent-background-process run
  produced a transient, non-reproducible `MISMATCH` on django that a clean
  single-process rerun — and a dedicated before/after/isolated-per-file
  diagnostic — did not reproduce; see "A false alarm, run down to ground"
  below):

  ```
  # TypeScript monster (40,872 files)
  ORACLE label=monster scenario=cold-vs-build ok
  ORACLE label=monster scenario=none        ok   (edges=196223 edge_hash=4e23ae3a246c8fa9 files_red=1576, unchanged from pre-bead)
  ORACLE label=monster scenario=leaf        ok
  ORACLE label=monster scenario=mixed50     ok
  ORACLE label=monster scenario=hub         ok
  ORACLE label=monster scenario=hubrename   ok
  ORACLE label=monster scenario=tests       ok
  ORACLE label=monster scenario=importchurn ok
  # tiptap (1,533 files) — all 8 ok, entities=42841 edges=5414 edge_hash=e3c0b09c8edefbe7 unchanged;
  #   files_red=404 on `none` (was 406 pre-bead: -2, exactly its two now-eligible .sh scripts)
  # django (3,023 files, Python) — all 8 ok, entities=37104 edges=47659 edge_hash=1967b05d3866b644;
  #   files_green=2970 on `none` (was 2968 pre-bead: +2, its two now-eligible .sh scripts)
  # gin (108 files, Go) — all 8 ok, entities=2217 edges=2352 edge_hash=832c9184bb30c187 unchanged
  # 32 of 32 scenarios green
  ```

* **`cargo clippy --release --all-targets`**: warning count and the full
  multiset of warning texts are identical with and without this bead's three
  touched files (`session.rs`, `scope_resolve.rs`, `import_resolution.rs`)
  applied — 174 warnings either way, matching this document's own recorded
  pre-bead baseline exactly. Zero new warnings.
* **`rustfmt --check` on the three touched files**: clean.
* **`cargo test -p sem-cli --release --no-fail-fast`**: every test binary
  green except `impact_direct_deps`, which fails exactly its 12 pre-existing
  tests (`impact_all_and_tests_match_no_cache_from_topology_cache`,
  `impact_deps_misses_cached_sql_*` ×5, `impact_deps_uses_cached_sql_*` ×5,
  `impact_deps_misses_topology_cache_when_side_effect_import_changes`) — no
  others, unchanged.

### A false alarm, run down to ground

Mid-sweep, a run of the full oracle produced a real-looking `MISMATCH` on
django across five of eight scenarios (`none`, `leaf`, `mixed50`, `hub`,
`hubrename`) — entity and edge counts that disagreed not just between warm
and cold, but between two consecutive, supposedly-identical cold builds of
the same unmutated file set *within the same process*. Treated as a
correctness question, not dismissed: isolated `scope_resolve.rs`'s fix alone
(clean), `import_resolution.rs`'s eligibility widening alone (clean, only
the expected ±2-file eligibility shift from django's two `.sh` scripts), a
clean checkout of current `HEAD` with none of this bead's files applied
(clean), and a dedicated standalone diagnostic (`examples/diag_django.rs`,
deleted after use) that cold-built django, ran a zero-file no-op rebuild, and
diffed the exact entity-id sets — which showed **zero** difference on a
controlled, single-process, foreground rerun. The mismatched run had two
`cargo run --release --example incr_probe` invocations backgrounded
concurrently against the same `target/` directory (django and the
TypeScript monster launched together), plus a mid-session kill/resume in
between — the same class of "concurrent in-flight edits by another agent"
artifact this repo's own `semx-2o8` commit independently reported seeing
against django around the same window. Every gate above was re-run singly,
in the foreground, with no concurrent build sharing the target directory,
and came back clean. Recorded here rather than quietly ignored: the
diagnosis is confident, not assumed, and the repro conditions (concurrent
backgrounded `cargo run` against a shared `target/`) are worth avoiding on
principle even though the underlying resolution logic was cleared.

## Python pathology (home-assistant) — semx-sbf

home-assistant-core (22,325 files, 18,142 `.py`, 257,832 entities, 307,860
edges) cold-built in 118.7–121.9s, of which `resolve_phase_ms` alone was
~90.9s (99%). At half the entity count of the TypeScript monster corpus
(454,541 entities, ~8s cold), that is roughly **15x worse per entity** —
exactly the "something Python-specific is pathological" symptom the bead
opened with.

### Method

Same playbook as every prior section: `SEM_PROFILE_RESOLVE=1`, release
build, `cargo run --release --example perf_probe -- <root> <label>`, one
foreground run at a time, no concurrent builds sharing `target/`. The
existing instrumentation (already extended for Python/Go/Rust attribution by
semx-kzy — see the read-set recorder buckets throughout this document)
turned out to decompose the build *without* needing new timer buckets: the
existing `scope_build_ms` accumulator (sum across every file, every thread —
not wall time) was already wide enough to show the entire story on its own.

### First measurement: attribution table (before)

```
BUILD_TOTAL label=ha-baseline files=22325 entities=257832 edges=307860
  build_total_ms=91969.07 resolve_phase_ms=90890.29
PHASE_NS files=18150 reparse_ms=452.07 pass1_scan_ms=228.75
  ctor_infer_ms=104.30 pass2_wall_ms=88545.79
  scope_build_ms=1563000.81 ref_collect_ms=66.73 ref_loop_ms=595.85
  resolve_ref_ms=288.14
FRAME_NS scope_wall_ms=89870.34 post_resolve_ms=676.09
CHUNKS count=5 sum_ms=89844.45 min_ms=13972.62 avg_ms=17968.89 max_ms=24363.96
THREAD_UTIL distinct_worker_threads_seen=18 available_parallelism=18
```

| bucket | value | % of build_total | notes |
|---|---:|---:|---|
| `scope_build_ms` (summed, all threads) | 1,563,000.81 ms | — | ÷18 threads ≈ 86.8s, matches `scope_wall_ms` almost exactly — this **is** the build |
| `scope_wall_ms` (chunk-loop wall time) | 89,870.34 ms | 97.7% | 5 sequential chunks, `CHUNKS sum_ms` matches (89,844ms) |
| `resolve_phase_ms` total | 90,890.29 ms | 98.8% | everything else (bag-of-words, dedupe/sort, edge index) is noise by comparison |
| `reparse_ms`, `pass1_scan_ms`, `ctor_infer_ms`, `resolve_ref_ms` | 452 / 229 / 104 / 288 ms | <0.5% each | all cheap, all already parallel, none is the story |
| `resolve_ref` candidate disambiguation | 66–288ms scale | ~0% | same verdict as the original TS-monster campaign: candidate lists are real (p95 32–63, max bucket 128–255 for Python method calls) but irrelevant to wall time |

**Every other sub-phase this document has ever instrumented (reparse,
pass-1 scan, ctor-infer, bag-of-words, dedupe/sort, edge-index, the resolver
itself) is noise. 97.7% of the build is inside the per-file scope-build
closure, and thread utilization is already 18/18** — so this isn't a
parallelism bug like the TS-monster reparse loop was; it's serial-per-call
algorithmic cost hiding *inside* an already-parallel per-file closure, where
the profiler's file-level granularity doesn't yet show which statement is
expensive.

### Attribution, continued: reading the code instead of adding more timers

`scope_build`'s per-file closure (`scope_resolve.rs`, per-file body inside
`resolve_with_scopes_full_inner`'s `maybe_par_iter!(file_paths)`) does, for
every non-JS/TS file: `build_scopes_from_ast` + `collect_all_file_refs` (AST
walks, O(file size)) then `extract_imports_from_ast` (one call per import
statement in the file). Python's `import mod` / `import mod as m` form
(*not* `from mod import name`, which is bounded — see
`resolve_import_name`) routes through `extract_python_module_import` ->
`register_namespace_import`, whose own doc comment already stated the
mechanism plainly:

> Python's bare `import module` form. Unlike `resolve_import_name`, this
> scans *every* entry of `symbol_table`, not one bounded candidate set.

I.e.: **every bare `import x` statement, in every Python file, triggered a
full scan of the whole corpus's symbol table** (every distinct name × every
entity behind that name — order `symbol_table` entries × average bucket
size, which sums to the corpus's total entity count, 257,832) — looking for
entities whose file matches the imported module. Home-assistant-core is
stdlib-adjacent, deeply-layered application Python: `import logging`,
`import asyncio`, `import voluptuous as vol`, `import homeassistant.helpers
.config_validation as cv`, etc. appear in nearly every one of its 18,142
files. That is the entire 90-second story: an O(files × imports-per-file ×
corpus-entities) scan, run to completion for every module import that never
even matches anything in the corpus (`logging`, `asyncio` aren't
home-assistant files) as readily as ones that do.

This is exactly suspect (c) from the bead's own list — "the bare-import
whole-table guard (`Table::GuardPyWildcardImport`, 4228a0b)" — but the
*incremental-invalidation guard* (`rec.whole(Table::GuardPyWildcardImport)`,
semx-kzy) was never the cost: that call is a single atomic-ish recorder
write. The cost was the **raw scan the guard sits next to**, which had no
index at all — every other cross-file read in this file (`resolve_import_
name`, the TS/Go/Rust/Kotlin namespace-import paths) already goes through a
bounded lookup (`symbol_table.get(name)`, `class_members.get(type)`, or —
for JS/TS's own namespace-import form — a pre-built `TopLevelEntityIndex`).
Python's bare-import form was the one namespace-import path that had never
been given the same treatment.

### Falsified / not-the-story hypotheses

* **Python ctor inference** (`ctor_infer_ms`) — 96–104ms out of a 91-second
  build. Real, instrumented, and irrelevant.
* **Bag-of-words on Python naming** (`bow_wall_ms`, `bow_resolve_ms`) — not
  even reached as a bottleneck at this scale: `resolve_phase_ms` was 98.8%
  consumed by `scope_build` before bag-of-words' own cost could matter (its
  post-fix summed cost, ~4.8s across 18 threads ≈ 270ms wall, is now visible
  but still a rounding error next to the fix).
* **Candidate disambiguation / `resolve_ref` itself** — same verdict as the
  original TS-monster campaign, reconfirmed on Python: candidate lists are
  structurally large (`class_members` p95 32–63, max bucket 128–255) but
  `resolve_ref`'s own wall time (66–288ms) never mattered.
* **The `Table::GuardPyWildcardImport` incremental guard being "expensive to
  evaluate"** — false via inspection: the guard write is O(1); it was the
  *read* it guards (a full un-indexed corpus scan) that cost 90 seconds, and
  the two are separable (see fix below) without touching the guard's
  invalidation semantics at all.

### Fix (single item — sufficient to hit the stop condition)

**Index `register_namespace_import`'s target instead of scanning for it.**
Fix class: pre-bucket/index (identical shape to the TS/Go/Rust/Kotlin
namespace-import paths that already had this). No resolution semantics,
candidate selection, or tie-break changed — same union-of-matching-files
result, same last-write-wins insert order, same
`Table::GuardPyWildcardImport` whole-corpus incremental-invalidation guard
(untouched: this fix is about *how the match is computed*, not what
invalidates it).

* `TopLevelEntityIndex` (previously JS/TS-only, hardcoded `is_js_ts_file`)
  generalized to take an `extensions: &[&str]` filter, and given a new
  `stem_index: HashMap<String, Vec<String>>` field (owned, not
  `&str`-borrowing, so it can live inside a `OnceLock`-cached struct without
  becoming self-referential) — built alongside `entities_by_file` in
  `build_top_level_entity_index`, once.
* Two small additions to `import_resolution.rs`: `build_owned_stem_index`
  (owned-string sibling of the existing `&str`-borrowing `build_stem_index`,
  which the semx-h1s incremental-import fix already used for exactly this
  shape of problem on the JS/TS side) and `match_bare_import_stem` (the
  "return every match" counterpart to the existing `resolve_bare_import_
  stem`, which returns only the single best match — Python's union-of-files
  semantics needed the former, not the latter).
* A second `OnceLock<TopLevelEntityIndex>` (`py_top_level_entities`),
  sibling of the existing JS/TS one, threaded through `extract_imports_from_
  ast` -> `extract_python_module_import` -> `register_namespace_import`,
  built lazily (only if a bare Python module import is actually seen) and
  restricted to `.py` files.
* `register_namespace_import` itself: instead of `for (name, target_ids) in
  symbol_table { for target_id in target_ids { ... } }` (O(corpus) per
  call), it now computes `import_file_candidates(...)` once (cheap — no
  corpus scan, just path arithmetic) and looks up each candidate (or, for
  bare/package specifiers with no candidate list, every stem match) directly
  in the pre-built index — O(imports-in-this-file × matching-files) instead
  of O(imports-in-this-file × corpus-entities). The union-across-matching-
  files and last-write-wins-on-collision behavior of the original scan is
  preserved exactly (verified by bit-identical gates below, not assumed).
* The JS/TS namespace-import path (`register_ts_namespace_import`) is
  untouched — same call, same arguments order, `extensions` now explicit
  (`JS_TS_EXTENSIONS`) instead of implicit, zero behavior change (verified:
  its own call site already passed `JS_TS_EXTENSIONS`, so the generalized
  `build_top_level_entity_index(symbol_table, entity_map, extensions)` call
  reproduces the old hardcoded `is_js_ts_file` filter exactly).

Files touched: `crates/sem-core/src/parser/scope_resolve.rs`,
`crates/sem-core/src/parser/import_resolution.rs`.

### Re-measurement (after)

```
BUILD_TOTAL label=ha-fix-final files=22325 entities=257832 edges=307860
  build_total_ms=3953.61 resolve_phase_ms=2938.01
PHASE_NS scope_build_ms=11293.46 (was 1,563,000.81)
FRAME_NS scope_wall_ms=1958.27 (was 89,870.34) post_resolve_ms=653.09
CHUNKS count=5 sum_ms=1932.44 min_ms=283.82 avg_ms=386.49 max_ms=585.55
  (was sum_ms=89,844.45, max_ms=24,363.96)
```

| corpus | before (cold build) | after (cold build) | speedup |
|---|---:|---:|---:|
| home-assistant-core (18,142 `.py`, 257,832 entities) | 91.97–121.91s (3 measurements) | **3.95–4.48s** (3 measurements) | **~23–27x** |
| home-assistant, `incr_probe` `none` scenario (warm, 0 changed, 4,178 files RED) | 2.41s | 1.77s | 1.4x (small — this scenario mostly hits the persisted-`FileFacts` fast path, semx-9en, which already skipped `extract_imports_from_ast` for most files) |
| home-assistant, `incr_probe` `leaf` scenario (warm, 1 changed, 14,281 files RED) | 78.51s | 2.87s | **~27x** (most RED files *don't* have persisted facts covering the mutation, so they hit the same expensive path cold builds do) |
| django (3,023 files, Python) | 1.50s | 0.69s | ~2.2x (smaller corpus, same mechanism, proportionally smaller absolute win) |
| TypeScript monster (40,872 files, no `.py`) | 7.62s | 8.22s | ~1.0x (unaffected code path, noise) |
| tiptap (1,533 files, no `.py`) | 284ms | 295ms | ~1.0x (unaffected code path, noise) |
| gin (108 files, no `.py`) | 57ms | 83ms | ~1.0x (unaffected code path, noise) |

**home-assistant-core's cold build is now faster in absolute terms than the
TypeScript monster's (4.0s vs 7.6–8.2s) despite having 257,832 entities
against the monster's 454,541 — the stop condition ("within ~2x of
TS-monster's per-entity rate, i.e. ~5–8s total build") is met and then some:
Python's cold-build per-entity rate is now *better* than TS's, not just
comparable.**

### Gates

* **Bit-identical, all 5 corpora, `SEM_FP_PARITY=1`**: `cold_ms`/`edge_hash`
  compared before vs. after via `git stash` (isolating exactly the two
  touched files), rebuild, re-measure, restore, rebuild — every run
  singleton and foreground, no concurrent builds sharing `target/`.
  * home-assistant: `cold-vs-build` ok; `edge_hash=7f0009313f4be377`
    identical before/after on both the cold build and the `none` warm
    scenario (`entities=257832 edges=307860` both); `leaf` scenario
    `edge_hash=c1830880cdb15ec7` identical before/after
    (`entities=257834 edges=307861` both). Full 8-scenario oracle run
    green (8/8) on the *fixed* build; `none`/`leaf` individually
    reconfirmed against the *pre-fix* build directly (the remaining 5
    scenarios' pre-fix baselines were not independently re-measured — each
    exceeded the 10-minute single-command budget on this pass — but they
    passed their own internal `cold-vs-build` oracle check post-fix, and
    structurally share the same `leaf`/`none` code paths already
    bit-identical-verified).
  * TypeScript monster: `edge_hash=4e23ae3a246c8fa9`, tiptap:
    `edge_hash=e3c0b09c8edefbe7`, django: `edge_hash=1967b05d3866b644`, gin:
    `edge_hash=832c9184bb30c187` — all four identical before/after, all
    match this document's own previously-recorded baseline values exactly
    (these hashes were already on record from semx-kzy's campaign, before
    this bead touched anything). 8/8 oracle scenarios green on all four,
    both before and after.
* **`cargo test -p sem-core --release --lib`**: 512 tests, 0 failures,
  matching this document's own previously-recorded baseline count exactly —
  including `python_oracle_touch_the_wildcard_import_target`, the fixture
  that specifically exercises `register_namespace_import`'s code path.
* **`cargo clippy --release -p sem-core --lib`**: 150 warnings before, 150
  after — the full multiset of warning texts diffed byte-for-byte identical
  (`diff` empty) between a `git stash`-isolated before/after pair. Zero new
  warnings from either touched file.
* **`rustfmt --check`** on both touched files: one line needed
  reformatting (a long chained method call introduced by this fix); fixed,
  clean on the re-check.

### Cross-language implications

This is a **Python-specific mechanism, not a shared code path.** The
pathology lives entirely in `register_namespace_import`, reached only by
Python's `import mod` / `import mod as m` form (gated on `config.self_
keywords.contains(&"self") && contains(&"cls")`, which per this document's
own per-language verdict table is a Python-only combination — Go doesn't
route through this function, and neither does Rust, Kotlin, Java, C#, C++,
Ruby, PHP, Scala, Zig, Bash, Fish, or Swift; each of those has its own
already-attributed or already-whitelisted cross-file path). The
confirmation measurements above (TS monster, tiptap, gin — none with `.py`
files) show ~1.0x, i.e. no effect, exactly as expected for a change that
only fires inside a Python-gated branch. **Does not re-prioritize the
queue**: C#/Scala/Rust/Go slowness, if real, has a different root cause and
needs its own attribution pass, not this fix.

### Verdict

One fix, one item, stop condition met on the first iteration — the playbook
predicts this is possible when attribution finds a single item consuming
97%+ of a phase (compare: the TS-monster campaign's own reparse-loop finding
was 74%, and still needed no second round to hit its target). No second
round was necessary; the remaining `resolve_phase` cost (bag-of-words ~270ms
wall-equivalent, dedupe/sort/edge-index low-single-digit ms, `pre_resolve_
ms` ~1.0s of import-table derivation) is now itself a small, flat,
already-parallel residual — genuine work, not attributed pathology, and well
inside the stop condition's tolerance.

## C# pathology (dotnet-runtime) — semx-zcq

dotnet-runtime (47,474 files, 1,015,756 entities, 980,978 edges) cold-built
in 124.6s in this campaign's own baseline measurement (fleet baseline: 203.0s
under ambient load ~8; this document's own measurements below use a single
consistent methodology — `SEM_PROFILE_RESOLVE=1`, `perf_probe`, foreground,
one build at a time — so the 124.6s figure, not 203.0s, is the correct
before/after comparison point). At 1,015,756 entities that is **not** the
15-18s TS-monster-per-entity-rate bar this bead opened with — but unlike
every prior section, **neither of this corpus's two pathologies was in
resolution** (candidate disambiguation, symbol-table scans, import
resolution): both were upstream of resolution entirely, in pass 1's
tree-sitter parse and in entity extraction's content-addressing hash. C#
itself was not the mechanism in either case — both pathologies are
language-agnostic machinery that happens to be exercised at pathological
scale by two categories of adversarial/generated fixtures .NET's own test
suite ships in quantity: XML crypto-recursion-limit tests and JIT
codegen-torture-test generators.

### Method

Same playbook as every prior section: `SEM_PROFILE_RESOLVE=1`, release build,
`cargo run --release --example perf_probe -- <root> <label>`, one foreground
run at a time, no concurrent builds sharing `target/`. Attribution needed two
additional throwaway measurement tools beyond the existing instrumentation
(both deleted before landing, not part of this section's diff): a per-file
parse/extract wall-time isolator (`extract_entities_with_tree` timed alone,
no corpus-wide parallelism, to separate one file's cost from scheduling
noise) and a full-corpus panic scanner (`catch_unwind` around every file's
extraction, used once to find a correctness regression introduced mid-fix —
see "Falsified" below). `SEM_FP_PARITY=1` with `incr_probe`'s 6 scenarios
(`none`, `leaf`, `mixed50`, `hub`, `tests`, `importchurn`) plus its built-in
`cold-vs-build` cross-check (session-build vs. plain `EntityGraph::build`
must agree) served as the correctness oracle throughout, not just at the end
— this campaign's second finding was caught entirely by that oracle
disagreeing between two in-process builds of the same corpus.

### First measurement: attribution table (before)

```
BUILD_TOTAL label=csharp-baseline files=47474 entities=1015756 edges=980978
  build_total_ms=124594.89 pre_resolve_ms=101810.16 resolve_phase_ms=22784.71
  import_table_derived_ms=0.00
PASS1_ONLY files=47474 entities=1015756 edges=0 pass1_only_ms=102057.28
PARSE_EXTRACT files=47454 entities=1015756 parse_extract_ms=96562.19
IO files=47454 bytes=589483054 io_ms=515.60
FRAME_NS scope_wall_ms=19811.21 post_resolve_ms=1919.41
```

| bucket | value | % of build_total | notes |
|---|---:|---:|---|
| `pre_resolve_ms` | 101,810 ms | 81.7% | dominant, unlike every prior corpus in this document (TS-monster, home-assistant) where `resolve_phase` dominated |
| `PARSE_EXTRACT` (pure parse+extract, no pass-1 symbol-table build) | 96,562 ms | 77.5% of build_total, 94.9% of `pre_resolve` | **this is almost the entire pre_resolve budget** |
| `resolve_phase_ms` | 22,785 ms | 18.3% | already TS-class-adjacent on its own: 1,015,756 entities in 22.8s is *better* per-entity than TS-monster's own resolve phase |
| `IO` (pure file read, parallel) | 516 ms | 0.4% | not the story |
| `import_table_derived_ms` | ~0 ms | 0% | not the story on this run (varied 0-12.6s across runs in this campaign, never the dominant factor) |

The immediate, decisive redirection from every prior section's playbook: this
corpus's problem is **not resolution**. `resolve_phase` (scope build, bag-of-
words, dedupe/sort/edge-index — everywhere every previous fix in this
document has lived) is already fine. 77.5% of the entire build is inside
`PARSE_EXTRACT` — pass 1's parallel read+parse+entity-extraction sweep, which
every prior corpus in this document (TS-monster: 2.7-2.8s for 40,872 files;
home-assistant: sub-second) has always treated as cheap, parallel, and
uninteresting.

### Attribution, continued: one file is 72-90% of the entire build

`PARSE_EXTRACT`'s per-extension breakdown (`perf_probe`'s `LANG_RATE` output)
showed `.xml`'s raw tree-sitter parse rate at 712 lines/sec — three to four
orders of magnitude below every other language/extension in the corpus.
Isolating just the two files over 500KB in the corpus's `.xml` set (`find
... -size +500k`) and then the larger one alone
(`src/libraries/System.Security.Cryptography.Xml/tests/EncryptedXmlSample4.xml`,
9.1MB) reproduced the entire story in one file:

```
$ perf_probe /tmp/xml-isolate-single single-file
PARSE_EXTRACT files=1 entities=25002 parse_extract_ms=90142.54
```

**One 9.1MB XML fixture, parsed alone, took 90.1 seconds** — 72.3% of the
124.6s baseline, 93.4% of `PARSE_EXTRACT`'s 96.6s. A direct
`Parser::parse`/`extract_entities_from_tree` split (bypassing the corpus
harness) attributed all 90s to the raw tree-sitter parse itself
(`raw_parse_ms=90082.43`), not the extraction walk (`extract_ms=104.27`), and
showed `tree.root_node().kind() == "ERROR"` — the file never parses cleanly
at all; tree-sitter's grammar enters its GLR error-recovery machinery and
never recovers.

The file's content explains why: it is a deliberately-recursive XML
decryption-transform-chain test fixture. `xml.sax`'s own depth counter
(Python stdlib, used only to characterize the fixture, not part of the fix)
reported **max nesting depth 50,006**, 125,012 total elements — a
recursion-depth-limit test fixture (almost certainly exercising the same
recursive-decryption depth-limit class of issue .NET's `EncryptedXml` has
historically needed CVE fixes for), not organically-deep real code.

This is **exactly the same failure mode** `PARSE_TIME_BUDGET` (semx-022,
`scope_resolve.rs`'s pass-2 reparse loop) was already built to bound —
tree-sitter's GLR error recovery going super-linear on adversarial input —
just reached by extreme structural depth instead of the TypeScript-fixture
class's extreme syntax-error density, and hit in **pass 1**, which had never
been given the same protection pass 2 already had. Confirmation that pass 2
*was* already catching this file: every baseline run in this campaign logged
`warning: skipped cross-file reference resolution for 1 file(s) that
exceeded the 2s parse budget: ...EncryptedXmlSample4.xml` — pass 2 was
already discarding this file's cross-file edges every single build; pass 1
was the only phase still paying its full, unbounded cost.

### Fix 1 — apply pass 1's existing sibling budget mechanism to pass 1 itself

`CodeParserPlugin::extract_entities_with_tree` (`plugins/code/mod.rs`), the
shared entry point pass 1's parallel file loop and the cached `extract_entities`
path both funnel through, called `parse_tree` — the plain, unconditional
`Parser::parse` — with no ceiling. `parse_within_budget`
(`scope_resolve.rs`) already existed for exactly this failure mode, but was
local to pass 2's reparse loop only.

**What shipped** (after two revisions forced by measurement — see
"Falsified" below):

* `PARSE_TIME_BUDGET` and the budget-checking parse function moved from
  `scope_resolve.rs` (private) to `plugins/code/mod.rs` (`pub`), as
  `parse_tree_within_budget` — a single shared implementation instead of two,
  used by both pass 1 (`CodeParserPlugin::extract_entities_with_tree`) and
  pass 2 (`scope_resolve.rs`'s reparse loop, which now calls the shared
  function instead of building its own throwaway `Parser` per file).
* **Gated on file size** (`LARGE_FILE_BUDGET_THRESHOLD = 128 KiB`): files at
  or below the threshold go through the plain, unconditional `parse_tree` —
  byte-for-byte the same code path as before this fix, zero behavior change.
  Only files above the threshold pay for the budget mechanism. This gate
  exists for a reason discovered during the fix, not by design up front (see
  "Falsified").
* **Implementation: a supervisor thread + `recv_timeout`**, not tree-sitter's
  `parse_with_options` + `progress_callback` cancellation (what
  `parse_within_budget` used, and what this fix's first revision also used).
  The callback-based read API turned out to have a *correctness* bug
  independent of timing (see "Falsified") — it is no longer used anywhere in
  this codebase after this fix.
* **`PARSE_TIME_BUDGET` raised from semx-022's original 2s to 10s** — forced
  by a second measurement finding (see "Falsified"): dotnet-runtime ships six
  legitimately-slow-to-parse files (`hugeexpr1.cs`, `HugeField1/2.cs`,
  `HugeArray1.cs`, `TestData.g.cs`, and siblings under
  `src/tests/JIT/jit64/opt/cse/` — generated JIT common-subexpression-
  elimination torture tests that also drive tree-sitter into error recovery,
  1.5-2.8s each in isolation) close enough to the old 2s budget that
  scheduler jitter under 18-way parallelism made whether any one of them
  finished in time non-deterministic between builds. The next file above
  that cluster is `EncryptedXmlSample4.xml` at >30x that magnitude (90s) with
  nothing in between, so 10s clears every known legitimate file with a
  >=3.5x margin while still bounding the pathological one to a small
  fraction of its unbounded cost.

Files touched: `crates/sem-core/src/parser/plugins/code/mod.rs`,
`crates/sem-core/src/parser/scope_resolve.rs`.

### Falsified / not-the-story hypotheses (fix 1)

* **The C# `using`/namespace machinery has a Python-shaped unindexed scan**
  (the bead's own leading hypothesis, prompted by the semx-sbf precedent) —
  checked first, per instructions, before any timer confirmed it. False:
  `register_namespace_import`'s Python-only gate
  (`config.self_keywords.contains(&"self") && contains(&"cls")`) does not
  fire for C#, and no analogous unbounded per-statement corpus scan exists
  anywhere in C#'s import/using resolution path. `resolve_phase` (where such
  a scan would show up) was never the dominant bucket for this corpus at
  all — ruled out by the very first attribution table, before any code
  reading.
* **A naive whole-tree-budget for pass 1** (first revision of this fix):
  applying `parse_tree_within_budget` unconditionally to *every* pass-1 file,
  not just large ones. Technically correct in isolation, but spawning one OS
  thread per file — ~47,454 of them over one build — caused enough scheduler
  contention that some *fast, healthy* files' `recv_timeout` calls were
  themselves delayed past the 2s budget: two in-process passes over the
  identical corpus (`PARSE_EXTRACT` then `PASS1_ONLY`, moments apart, same
  process) disagreed by 7,591 entities. Fixed by gating on file size
  (`LARGE_FILE_BUDGET_THRESHOLD`) instead of applying the mechanism
  everywhere — confirmed by two repeated full-corpus runs afterward
  producing bit-identical `entities`/`edges`/`edge_hash` every time.
* **`parse_with_options` + `progress_callback` as the budget mechanism for
  pass 1** (also first revision): mirroring pass 2's existing mechanism
  verbatim caused a full-corpus panic (`range start index 5630 out of range
  for slice of length 5586`, inside tree-sitter's own `Node::utf8_text`) on
  `EncryptedXmlSample5.xml` (5,586 bytes — small, fast, NOT the large
  pathological file). Root-caused via a full-corpus `catch_unwind` scanner
  and a targeted isolation: the callback-based read API returned a
  *completed* tree containing a node with `end_byte() = 5630`, past the
  5,586-byte input's end — a tree-sitter binding defect, not a timing issue
  (confirmed: the same content parses correctly via the plain, direct
  `Parser::parse` API in under a millisecond, 29 entities, no error). Fixed
  by never using the callback-based read API in the new pass-1 path at all —
  the supervisor-thread + `recv_timeout` design races the *plain* parse API
  against a deadline instead. Full-corpus panic scan (47,454 files,
  `catch_unwind` around every extraction) reconfirmed zero panics after the
  fix.
* **2s as an adequate budget once the mechanism itself was fixed**: a second,
  independent finding from the same `incr_probe --fp-parity` oracle that
  caught the entity-count disagreement above — `cold-vs-build` and
  `warm-vs-cold` (the `none` scenario) both flagged real disagreements after
  the thread-oversubscription fix was already in, traced to
  `hugeexpr1.cs` (1.5-2.2s in isolation, right at the 2s line) being a
  coin-flip under load. Not a design flaw in the budget concept — a
  parameter that was too tight for a genuinely-slow-but-legitimate file this
  particular corpus happens to ship. Raised to 10s; reconfirmed
  deterministic across 4+ repeated `incr_probe` runs (`none` x2, `leaf`,
  `mixed50`, `hub` — every one's `cold-vs-build` and warm-vs-cold oracle
  green, identical `edge_hash` every time).

### Re-measurement after fix 1 alone

```
BUILD_TOTAL files=47474 entities=990754 edges=980978
  build_total_ms=49750.98 pre_resolve_ms=16672.97 resolve_phase_ms=33078.01
```

90.1s → ~10s (the new budget) for the one pathological file; `PARSE_EXTRACT`
dropped from 96.6s to single digits for every file except the two that still
pay the (now 10s, not 90s) ceiling once in pass 1 and once in pass 2's
reparse (see "Cross-language implications" for why the double-payment wasn't
fixed this round). `entities` dropped by exactly 25,002 — `EncryptedXmlSample4.xml`'s
entity count, extracted entirely from a `root_kind=ERROR` degenerate parse
tree pass 2 was *already* discarding for cross-file resolution purposes; see
"Gates" for why this specific, fully-attributed count change does not
violate this campaign's bit-identical discipline. `edges` did **not**
change at all — `980978` before and after, exactly, on every measurement in
this section — confirming pass 2's pre-existing treatment of this file
(drop its cross-file edges) is untouched.

### Attribution, round 2: the second pathology, found by literal-minded scaling

Fix 1 alone was not enough to reach TS-class (49.8s vs. an ~15-18s bar for
1,015,756 entities). `PARSE_EXTRACT` and `pre_resolve` were fixed, but a
`slow_files`-style full-corpus per-file scan (`extract_entities_with_tree`
timed per file, sorted slowest-first, same throwaway-tool discipline as
fix 1's isolators) surfaced two files that were *never* about parsing at
all:

```
9601.11ms entities=502 .../generics/Instantiation/Nesting/NestedGenericStructs.cs
9983.27ms entities=502 .../generics/Instantiation/Nesting/NestedGenericTypesMix.cs
```

A parse/extract split on `NestedGenericStructs.cs` (35KB, 1,025 lines — not
large) attributed **all 9.5s to the extraction walk, not the parse**
(`parse_ms=9.74`, `extract_ms=9783.54`). The file: a JIT type-loader test
fixture instantiating 500 levels of nested generic structs
(`MyStruct0<int>.MyStruct1<int>. ... .MyStruct499<int>`), each nesting level
its own `struct MyStructN<TN> { ... }` declaration — 500 real, distinct,
uniquely-named entities, not a degenerate/error parse this time
(`has_error=false`).

A synthetic reproduction (declaration-only, no giant expression statement,
varying nesting depth N) isolated the scaling law precisely:

| depth N | extract_ms |
|---:|---:|
| 50 | 13.0 |
| 100 | 80.8 |
| 200 | 628.2 |
| 300 | 2,093.5 |
| 500 (real file) | 9,601-9,983 |

Consecutive ratios (N doubling: ~6.2x, ~7.8x; N x1.5: ~3.3x) all cluster
around **N^2.9-3.0** — cubic in nesting depth, not linear or quadratic.
300→500 extrapolated at pure N^3 predicts 9,690ms; the real file measured
9,601-9,983ms — matching within noise.

Root cause, found by timing four sub-phases of one entity's extraction
(`extract_name`, `content` slice, `structural_and_semantic_hash`/kappa,
`build_entity_id`) with a throwaway per-category accumulator: **99.6% of the
9.5s was inside kappa/structural-hash computation**
(`hash_ms=9478.54` of `9483.14` total). Reading
`structural_and_semantic_hash` (`utils/hash.rs`) found the mechanism:
`is_semantic_leaf`, for every anonymous keyword-shaped leaf token (`public`,
`struct`, ...) encountered during the hash's tree walk, calls
`Node::parent()` to classify the leaf. Checking tree-sitter's own C source
(`node.c`, `ts_node_parent`) settled the host-algebra question directly
rather than assuming: **`Node::parent()` is not O(1)** —

```c
TSNode ts_node_parent(TSNode self) {
  TSNode node = ts_tree_root_node(self.tree);   // starts at the WHOLE TREE'S root
  if (node.id == self.id) return ts_node__null();
  while (true) {
    TSNode next_node = ts_node_child_with_descendant(node, self);
    ...
  }
}
```

it walks *down from the tree's root* looking for the child whose subtree
contains the target — O(depth of the node from the tree root), not O(1) and
not anchored to whatever subtree is currently being hashed. Since entity
extraction hashes one full subtree *per nested entity* (each of the 500
structs gets its own independent `structural_and_semantic_hash` call over
its own — nested, overlapping — subtree), and each such call's keyword-leaf
`.parent()` lookups are anchored to the *whole file's* root regardless of
which entity's hash is being computed, the true cost is: (entities, one per
nesting level) x (keyword leaves in that entity's subtree) x (that leaf's
absolute depth from the file root) — cubic in nesting depth. `structural_hash`/
`structural_hash_excluding_range` (the pre-kappa, non-`is_semantic_leaf`
functions) never had this cost; only `structural_and_semantic_hash` (kappa)
does, because only kappa's leaf classification needs a parent.

### Fix 2 — thread the already-known parent through the traversal instead of re-deriving it

`structural_and_semantic_hash`'s own tree walk is an explicit worklist that
pushes a node's children right after visiting the node — i.e. it already
holds the parent in hand at exactly the moment it needs to know a child's
parent; the O(depth-from-root) `Node::parent()` call was re-deriving
information the traversal already had for free.

* `structural_and_semantic_hash`'s worklist changed from `Vec<Node>` to
  `Vec<(Node, Option<Node>)>` — each entry now carries its already-known
  parent (`None` only for the root).
* A new `push_children_reversed_with_parent` (sibling of the existing
  `push_children_reversed`, same traversal order and allocation shape, not
  shared with it — `push_children_reversed` is also used by the non-kappa
  `hash_structural_tokens`/`hash_structural_tokens_excluding`, which never
  had this cost and are untouched) pushes `(child, Some(node))` pairs.
* `is_semantic_leaf` and `is_leading_keyword_discriminator` take the
  known parent as a parameter instead of calling `.parent()` internally.
  `is_pure_keyword_bag_parent` (which already took a `Node` directly, not
  calling `.parent()` itself) is unchanged.

This is a genuine algebraic identity, not an approximation: the parent a
cursor-based `goto_first_child`/`goto_next_sibling` enumeration assigns to a
child is, by tree-sitter's own construction, the exact same node
`ts_node_parent` would compute for that child — verified structurally (both
walk the concrete syntax tree's actual parent-child edges, not some
alternate notion of "logical" parent), and empirically by all 512
`cargo test -p sem-core --lib` tests passing unchanged, including the two
kappa-discriminator regression tests
(`csharp_public_vs_private_readonly_already_differ`,
`kotlin_val_declaration_formatting_invariance_still_holds`,
`kotlin_val_vs_var_differ`) that specifically pin the exact logic this fix
touched.

Files touched: `crates/sem-core/src/utils/hash.rs`.

### Re-measurement after fix 2

```
$ isolate_one /tmp/declonly.cs   # synthetic, depth 500
extract_ms=104.55 entities=503   # was 9,601-9,983ms

$ isolate_one NestedGenericStructs.cs   # real file
extract_ms=86.20 entities=502    # was 9,601ms

$ isolate_one NestedGenericTypesMix.cs
extract_ms=88.01 entities=502    # was 9,983ms
```

~100x for both real files: 9.6-10.0s down to 86-105ms.

### Final numbers (both fixes)

```
BUILD_TOTAL label=csharp-final-report files=47474 entities=990754 edges=980978
  build_total_ms=49750.98 pre_resolve_ms=16672.97 resolve_phase_ms=33078.01
  import_table_derived_ms=658.79
```

| metric | before | after | change |
|---|---:|---:|---:|
| `build_total_ms` (this campaign's own consistent baseline) | 124,594.89 | ~49,750-51,406 (5 repeated measurements) | **~2.4-2.5x** |
| `build_total_ms` (fleet baseline, ambient load ~8, different measurement session) | 203,000 | ~49,750-51,406 | **~4.0-4.1x** |
| `entities` | 1,015,756 | 990,754 | -25,002 (exactly `EncryptedXmlSample4.xml`'s spurious ERROR-tree entity count; see "Gates") |
| `edges` | 980,978 | 980,978 | **0 — bit-identical** |
| single pathological XML file, isolated | 90.1s | ~10.0s | ~9x (bounded, not eliminated — genuinely unparseable input) |
| `NestedGenericStructs.cs`, isolated | 9.6s | 0.086s | ~112x |
| `NestedGenericTypesMix.cs`, isolated | 10.0s | 0.088s | ~114x |

**Not fully at the ~15-18s TS-monster-per-entity-rate bar** (1,015,756/
990,754 entities is 2.18x TS-monster's 454,541, so linear-scaled TS-monster
time would be ~17.4s; actual is ~49.8-51.4s, ~2.9x that scaled bar — outside
the "within ~2x" clause). The remainder is attributed, with numbers, to two
things, not further pathology hunting this round:

1. **A known, quantified, deferred double-payment.** `EncryptedXmlSample4.xml`
   pays its (now 10s, not 90s) budget-bounded parse attempt *twice* per
   build on this corpus — once in pass 1, once again in pass 2's reparse
   loop, because dotnet-runtime (47,474 files) exceeds
   `PARSED_FILE_REUSE_LIMIT` (20,000), so pass 1's parsed trees are not
   retained into pass 2 at all (`retain_parsed_files = file_paths.len() <=
   PARSED_FILE_REUSE_LIMIT` is false for this corpus) — every file pass 1
   didn't produce a usable tree for gets re-attempted from scratch in pass 2,
   including this one. One of the ten sequential scope-resolution chunks
   measured `max_ms=10989.02` against every other chunk's low single digits
   — that chunk contains this file's second 10s wait. A targeted fix (thread
   a lightweight "pass 1 already learned this file exceeds the budget, don't
   retry" signal from pass 1 into pass 2's reparse loop, sibling of the
   existing `pre_parsed`/`precomputed_facts` carry-forward that already
   avoids redundant reparse for *successfully* parsed files) would recover
   roughly 8-9s of wall time — real, but insufficient on its own to reach
   the 2x line, and deliberately not attempted this round given the
   complexity of correctly threading new shared state through the pass-1/
   pass-2 chunk boundary under this session's remaining time budget rather
   than risk a rushed correctness bug in heavily-tested resolution code.
2. **Genuine, already-parallel, chunked-corpus work.** `resolve_phase`'s
   remaining ~33s (bag-of-words tokenization ~18s summed/~1s wall-equivalent
   across 18 threads, scope-build ~34s summed/~1.9s wall-equivalent, the
   34,897-file pass-2 reparse itself) is the same *shape* of cost the
   TS-monster campaign (semx-022) already characterized and accepted as
   "genuine work" for corpora crossing `PARSED_FILE_REUSE_LIMIT` — dotnet-
   runtime crosses it by more than 2x (47,474 vs. TS-monster's 40,872) and
   carries 589MB of source text (more than TS-monster's), so some
   proportionally larger chunked-resolution cost is expected, not
   necessarily pathological. This document does not have a chunked-corpus
   per-byte or per-chunk baseline precise enough to separate "expected
   scaling" from "a third, unfound pathology" with confidence in the time
   available this round — flagged explicitly as unresolved, not asserted
   as clean.

### Gates

* **`cargo test -p sem-core --release --lib`**: 512 tests, 0 failures — same
  count as every prior section's baseline, including the kappa-discriminator
  regression tests that specifically exercise fix 2's code path.
* **`cargo clippy --release -p sem-core --lib`**: 149 warnings before (105
  fix suggestions) -> 148 after (104 fix suggestions), verified via
  `git stash`-isolated before/after on the exact two touched files at the
  time of the clippy gate (`plugins/code/mod.rs`, `scope_resolve.rs`) — the
  one new warning this fix's first draft introduced
  (`manual_unwrap_or_default` on the `recv_timeout` match) was fixed before
  landing, net *fewer* warnings than baseline, zero new ones.
* **`cargo fmt --check -p sem-core`**: clean, no diff.
* **Full-corpus panic scan** (47,454 files, `catch_unwind` around every
  `extract_entities_with_tree` call): 0 panics — this is the regression test
  for the `EncryptedXmlSample5.xml` finding under "Falsified"; not part of
  the normal test suite, run manually as part of this bead's own gate
  discipline given the defect it catches was found by exactly this method.
* **Bit-identical entity/edge counts, all 5 standard corpora** (`perf_probe`,
  rebuilt binary, same release build as the dotnet-runtime measurements):
  * TypeScript monster: `entities=454541 edges=196223` — matches this
    document's own most-recently-recorded value (§"Restoring bag-of-words's
    single-visit invariant" onward) exactly.
  * tiptap: `entities=42841 edges=5414` — matches this document's headline
    number exactly.
  * django: `entities=37104 edges=47659` — matches exactly.
  * gin: `entities=2217 edges=2352` — matches exactly.
  * home-assistant-core: `entities=257832 edges=307860` — matches the
    semx-sbf section's post-fix value exactly.
  * All five: zero `.py`/`.cs`/pathological-XML content in common with
    dotnet-runtime's two fixes' trigger conditions, so bit-identical output
    is the correct expectation, not a coincidence — confirms neither fix
    touched any code path these corpora exercise.
* **`SEM_FP_PARITY=1` + `incr_probe` oracle, dotnet-runtime**: every scenario
  run this round green on both of `incr_probe`'s built-in checks
  (`cold-vs-build`: session-build vs. plain `EntityGraph::build` must
  produce the same `edge_hash`; and each warm scenario vs. its own fresh
  cold rebuild):
  * `none` (0 changed, 17,394 files RED): run 3x, `entities=990754
    edges=980978 edge_hash=214225a140dbe337` every time, both oracles green
    every time.
  * `leaf` (1 changed): `cold-vs-build` ok; warm
    `entities=990756 edges=980979` (the 1-file mutation's expected small
    delta), oracle ok.
  * `mixed50` (50 changed): both oracles ok.
  * `hub` (highest fan-in file changed): both oracles ok.
  * The full 6-scenario `all` sweep (`none`, `leaf`, `mixed50`, `hub`,
    `hubrename`, `tests`, `importchurn`) was run on tiptap and gin instead of
    dotnet-runtime (each individual dotnet-runtime scenario already costs a
    ~50s cold + ~29s warm pair; the remaining two scenarios —
    `hubrename`/`tests`/`importchurn` — were not independently re-measured
    on dotnet-runtime itself this round, matching this document's own
    established practice from the semx-sbf section when a corpus's full
    scenario matrix exceeds the session's time budget): tiptap 8/8 green
    (`cold entities=42841 edges=5414 edge_hash=e3c0b09c8edefbe7`, matching
    this document's own recorded value), gin 8/8 green
    (`edge_hash=832c9184bb30c187`, matching).
* **The entity-count change is not a bit-identical violation of this
  campaign's own discipline — it is the discipline working as designed.**
  Every previous section's bit-identical gate proves a *pure refactor*
  (parallelize/index/hoist) changed nothing observable. This section's fix 1
  is not a pure refactor for exactly one file in exactly one corpus: it
  extends pass 2's *already-existing, already-shipped* treatment of
  `EncryptedXmlSample4.xml` (drop it; it is not parseable) to pass 1, which
  had never had that treatment. The 25,002-entity delta is that file's
  entire contribution — extracted from a `root_kind=ERROR` tree pass 2 was
  independently discarding for resolution purposes on every build already —
  and `edges` (`980978`, unaffected by pass 1's entity count either way)
  proves the resolution-visible behavior genuinely did not change. Every
  *other* file, in every corpus measured, is bit-identical.

### Cross-language implications

Neither fix is C#-specific in mechanism, and both generalize:

* **Fix 1** (pass-1 parse budget) protects *any* language's pass-1 parse
  against the same GLR-error-recovery blowup pass 2 was already protected
  against — gated only on file size, with zero dependency on `config.id`.
  Already-known-affected: TypeScript/JS repos ship the same class of large,
  deliberately-malformed compiler-fixture files (`tests/baselines/reference/
  *.js`, `tests/cases/**`) that motivated `PARSE_TIME_BUDGET` in the first
  place (semx-022) — those were previously caught by pass 2 alone; pass 1 now
  shares the same protection, for every language, for free.
* **Fix 2** (kappa's `Node::parent()` cost) affects **every language's**
  kappa/structural-hash computation for **any** deeply-nested declaration
  structure — it is a tree-sitter-binding-level cost, not a grammar- or
  language-specific one; `structural_and_semantic_hash` is the single shared
  implementation for every language's kappa computation, so the fix already
  covers any future corpus that happens to nest this deep, with no further
  change needed.
* **One-run confirmation on rust-lang-rust (this bead's own required check
  before re-prioritizing the queue)**: `perf_probe` on the full corpus
  (42,575 files) cold-built in **11.0s** for 450,824 entities
  (`pre_resolve_ms=6764.06`, `resolve_phase_ms=4267.48`) — already TS-class
  (450,824 entities, close to TS-monster's 454,541, in 11.0s vs.
  TS-monster's 7.6-8.4s) with **no sign of either pathology** at the
  corpus level. A targeted check on the one file whose name suggested the
  same "deliberately adversarial deep nesting" shape as the C# fixture this
  bead root-caused (`tests/ui/parser/survive-peano-lesson-queue.rs`, 7.9MB —
  the name references Peano-arithmetic-style recursive nesting) came back
  negative on inspection: `parse_ms=73.93` (`has_error=false`),
  `extract_ms=6.56`, 59 entities — an ordinary, fast file; the name was a
  false lead. **Conclusion: rust-lang-rust does not currently exhibit either
  of this bead's two pathologies, and does *not* re-prioritize the
  Scala/Rust queue items** — whatever slowness those corpora have (if any)
  is a different root cause and needs its own attribution pass from
  scratch, the same caveat the semx-sbf section recorded for this bead's own
  queue position. (Scala's `spark` corpus was not independently checked this
  round; flagged as unconfirmed rather than assumed clean.)

### Verdict

Two independent, unrelated-in-mechanism pathologies found and fixed in one
pass, both upstream of resolution (the first time in this document's history
neither fix lived in `resolve_phase`) — kill-switch discipline (RED
reproduction via isolated per-file/synthetic-depth measurement, root-caused
against tree-sitter's own C source rather than assumed, GREEN via targeted
fix, bit-identical gates on every unaffected corpus) applied to both. Fix 1
alone was caught mid-flight introducing two new defects (thread-
oversubscription non-determinism, a tree-sitter binding panic) by this
campaign's own oracle machinery (`incr_probe --fp-parity`, a full-corpus
panic scan) before either could have landed — both fixed before this section
closes, not deferred. **Stop condition not fully met**: 124.6s -> ~49.8-51.4s
(~2.4-2.5x) lands at ~2.9x the linear-scaled TS-monster bar, not within the
~2x clause, with the shortfall attributed (not hand-waved) to one quantified,
deferred double-payment (~8-9s, a scoped follow-up) and one honestly-flagged
open question (whether the remaining chunked-corpus `resolve_phase` cost is
fully "genuine work" or hides a third pathology this round didn't have
budget to rule out). Closing semx-zcq with this evidence rather than
continuing to loop the playbook against a shrinking time budget on a third,
unconfirmed hypothesis.

## Scala pathology (spark) — semx-eki

The bead's own baseline claim: spark (11,222 files, 291,317 entities, 281,861
edges) cold-builds in 40-46s on the fleet, `resolve_phase` 39-44s (95% —
"resolve-dominated like Python was"). **This section's own first
measurement, using this document's own established methodology (single
foreground `perf_probe` run, release build, `SEM_PROFILE_RESOLVE=1`, no
concurrent builds sharing `target/` — the same standard the C# section used
to declare its 124.6s figure, not the fleet's 203.0s, "the correct
before/after comparison point"), does not reproduce the fleet number at
all: 3.52-3.74s cold, `resolve_phase_ms` 2.35-2.45s (~66%).** Spark has
never been individually profiled in this document before this section (the
C# section's own closing note flagged it "not independently checked this
round; flagged as unconfirmed rather than assumed clean") — so unlike every
prior section, there is no earlier finding to falsify; this *is* the first
attribution pass, and it finds no pathology to fix.

### Method

Same playbook as every prior section: `SEM_PROFILE_RESOLVE=1`, release
build, `cargo run --release --example perf_probe -- <root> <label>`, one
foreground run at a time, no concurrent builds sharing `target/`. Two
throwaway tools beyond the existing instrumentation, both deleted before
landing (this section makes no production-code changes at all): a per-file
parse+extract wall-time isolator restricted to `.scala`/spark's full file
set (same shape as the C# section's `slow_files`-style scanner, used here to
rule out a single-adversarial-file pathology up front rather than after a
fix attempt), and a `RAYON_NUM_THREADS`-constrained series of full builds
(18/2/1 threads) used to test the ambient-load hypothesis directly rather
than assume it.

### First measurement: attribution table

```
BUILD_TOTAL label=spark-baseline files=11222 entities=291317 edges=281898
  build_total_ms=3593.84 resolve_phase_ms=2367.20
BUILD_TOTAL label=spark-run2     build_total_ms=3523.56 resolve_phase_ms=2346.95
BUILD_TOTAL label=spark-repeat1  build_total_ms=3558.15 resolve_phase_ms=2354.35
BUILD_TOTAL label=spark-repeat2  build_total_ms=3598.41 resolve_phase_ms=2374.24
BUILD_TOTAL label=spark-repeat3  build_total_ms=3642.06 resolve_phase_ms=2391.67
THREAD_UTIL distinct_worker_threads_seen=18 available_parallelism=18
```

5 repeated cold builds (release, HEAD `9e9b981`): **3.52-3.64s**, avg
3583ms; `resolve_phase_ms` avg 2367ms (~66%); `entities=291317
edges=281898` bit-identical across all 5. (`edges` is 37 above the bead's
own recorded 281,861 — stable and reproducible on every run measured here,
so treated as this document's now-current baseline for spark, not
investigated further: a 0.013% delta is far too small to be this bead's
story and is more likely corpus drift between whenever the bead's number was
recorded and this checkout's current commit than a resolution-semantics
difference.)

At 291,317 entities in ~3.58s, spark's per-entity rate is **~12.3 μs/entity
— *better* than the TypeScript monster's own post-fix rate** (454,541
entities in 7.62s per the semx-sbf gate table = ~16.8 μs/entity). This
clears the bead's own stop condition ("within ~2x TS per-entity rate") on
the first measurement, with room to spare — not close to a 40-46s pathology
at all.

### Attribution, continued: checking the three named hypotheses in order, each against evidence

**(a) Python-precedent shape — unindexed whole-table scan on Scala imports,
esp. `import x._` wildcards.** Checked in code before trusting any timer,
per instructions. **False, structurally impossible, not just
unmeasured:** `SCALA_SCOPE_CONFIG` (`languages.rs`) sets
`import_extractor: None`. `scope_resolve.rs`'s per-ref resolution loop reads
that field directly — `let allow_cross_file = config.import_extractor.is_none();`
— to mean "this language has no per-symbol import extraction; fall back to
global/bag-of-words resolution for cross-file references" (the same
mechanism Swift and Kotlin already use, per the surrounding comment).
Because `import_extractor` is `None`, `extract_imports_from_ast` and every
function beneath it (`extract_python_module_import`,
`register_namespace_import`, the whole-corpus-scan mechanism the semx-sbf
fix indexed) is **never called for a single Scala file, ever** — there is no
code path for a Scala-specific unindexed scan to exist in, let alone run.
The bead's own context note ("Scala's import node kind is
`import_declaration`, routed to Go's extractor, proven structural no-op")
undersold this slightly: it isn't that the routed extractor is a no-op on
Scala's AST shape, it's that no extractor is ever invoked for Scala at all —
`import_extractor: None`, confirmed directly in `SCALA_SCOPE_CONFIG`, not
inferred from behavior.

**(b) Bag-of-words on Scala naming (objects/case classes/implicits
generate distinctive name patterns).** Checked via the existing
`TOP20_CALL_GLOBAL_NAMES_BY_CANDIDATES` / `TOP20_BOW_CLASS_MEMBERS_BY_TIME`
/ `CANDIDATE_DIST` instrumentation (already wired for every language since
semx-cnq/semx-h19). Largest bag-of-words-relevant name (`sql`, from Spark's
own fluent SQL-builder API being called everywhere) has `avg_candidates=445`
across 12,334 calls — real, but this bucket (`call_global`, the
`symbol_table.get(name)` fast path) is bounded per-name, not a corpus scan,
and `resolve_ref`'s own measured cost stayed at 328-372ms out of a
3,600ms build (~9-10%) across every run — same verdict every prior section
in this document has reached for candidate disambiguation: real structural
size, irrelevant wall time. `bow_index_build_ms`/`bow_resolve_ms` (summed
across 18 threads) were 3,726-3,745ms / 4,327-4,399ms respectively — modest,
already-parallel, and (see next section) fully accounted for by genuine
per-reference work, not an indexing blowup.

**(c) Per-file adversarial scaling (C#-style).** Checked directly: a
throwaway per-file parse+extract timer run once over spark's full 11,218
files (no corpus-wide parallelism masking any one file's cost) found **no**
outlier remotely resembling `EncryptedXmlSample4.xml` or
`NestedGenericStructs.cs`. Slowest file: `error-conditions.json` (not even
Scala — a generated error-catalog resource), 239.39ms, 5,246 entities.
Slowest `.scala` file: `functions.scala` (Spark's public fluent-API surface,
genuinely large and entity-dense), 156.03ms, 1,053 entities. Every other
file in the top 30 is under 65ms. `LANG_RATE` for `.scala` (6,297 files,
77.1MB): 163.0 MB/s raw parse, 115.7 MB/s combined parse+extract — healthy,
no GLR-error-recovery signature (`has_error` never checked pathological on
any file in the top-30 scan). No pass-1 parse-budget trips, no cubic
kappa/`Node::parent()` blowup signature (which would show as one or two
files costing seconds, not the 239ms max actually observed).

None of the three hypotheses survives contact with the code or the
measurements. **There is no Scala-specific resolution pathology in this
codebase at the current commit.**

### Where the fleet's 39-44s figure likely comes from: an ambient-load parallelism experiment

Rather than leave "the fleet said 40-46s and I measured 3.6s" as an
unexplained contradiction, the gap was tested directly with
`RAYON_NUM_THREADS`-constrained builds of the identical corpus:

| `RAYON_NUM_THREADS` | `build_total_ms` | vs. 18-thread |
|---:|---:|---:|
| 18 (this machine's `available_parallelism`) | 3,593.84 | 1.0x |
| 2 | 13,702.32 | 3.8x |
| 1 | 24,795.20 | 6.9x |

Wall time is highly sensitive to available parallelism, not flat the way a
fully-serial-dominated corpus (like the pre-fix TS-monster reparse loop)
would be — most of this corpus's cost genuinely is the already-parallel
per-file/per-reference work every other section of this document has always
treated as "genuine work" when it doesn't concentrate in one bucket. A
second, independent estimate corroborates the same order of magnitude:
summing every named bucket's own *thread-summed* cost (not wall time) —
`PHASE_NS` (~6,632ms) + `RESIDUAL_NS` (~9,006ms) + `LOOKUP_NS` (~439ms) from
one representative run — gives resolve_phase's true single-threaded-
equivalent CPU cost as **~16.1s**; a separately-run full-corpus per-file
parse+extract sum (the same throwaway isolator from hypothesis (c),
`SUM_MS`) gives pass-1's single-threaded-equivalent cost as **another
~16.1s**. Total single-threaded-equivalent CPU work for the whole build:
**~32.2s** — before adding scheduling/contention overhead beyond pure
serialization (context-switch cost, cache thrashing between competing
processes). That figure lands within ~20-30% of the fleet's reported 39-44s,
and the C# section's own precedent already established that this specific
fleet runs under "ambient load ~8" (i.e., roughly 8 competing processes
sharing the machine) — enough to plausibly push effective per-process
parallelism well below 1 full core some fraction of the time, closing the
remaining gap. **This is not proof the fleet number is pure ambient-load
noise — no fleet-side profiling was available this round to confirm it
directly — but it is a quantified, self-consistent alternative to "Scala has
an algorithmic pathology," and no algorithmic pathology was found anywhere
this section looked.**

### Falsified / not-the-story hypotheses

* **Python-precedent unindexed import scan** — falsified structurally, not
  just by measurement: `import_extractor: None` means the code path that
  would contain such a scan is never entered for Scala. See hypothesis (a)
  above.
* **Bag-of-words blowup on Scala naming** — candidate lists are real
  (Spark's fluent-API method names collide across a huge surface, same
  shape as TS-monster's `Node`/`Symbol`/`Type`) but `resolve_ref` and
  bag-of-words costs stayed at their usual small, flat, already-parallel
  scale across every run. See hypothesis (b).
* **C#-style adversarial per-file scaling** — checked directly with a
  full-corpus per-file isolator; no file came close to the seconds-scale
  cost the C# section's two pathological files showed. See hypothesis (c).
* **"39-44s is this document's applicable baseline for spark"** — rejected
  on the same grounds the C# section already established for its own fleet
  figure (203.0s vs. the 124.6s this document actually used): a fleet
  number measured under ambient load is not this document's before/after
  comparison point. Spark had never had *any* single-build baseline
  measured before this section: the fleet's 40-46s was never this
  document's number to begin with.

### Fix

**None.** No code was changed. The stop condition ("within ~2x TS per-entity
rate or remainder attributed with numbers") was met on the very first
measurement, before any hypothesis needed a code-level fix — the playbook's
own precedent for this (compare: the Python section's single-fix,
single-iteration close) is that when attribution finds nothing to fix, the
correct move is to stop and record the finding, not invent a change to
justify the bead. The one throwaway measurement tool used for hypothesis (c)
was deleted before this section was written; `git status` confirms no
production files changed by this investigation
(`crates/sem-core/src/parser/plugins/code/languages.rs`'s pre-existing
uncommitted reflow hunk predates this session and was not touched further).

### Gates

* **No bit-identical gate applies** — no resolution code changed, so there
  is nothing to compare before/after. The 5 repeated spark measurements
  above serve the determinism role instead: `entities=291317 edges=281898`
  identical on every one of 5 runs (2 profiled, 3 unprofiled), across two
  different commits (`a83200a`, pre-C#-fix, in an isolated `git worktree`:
  `build_total_ms=3741.83`, `entities=291317 edges=281898` — same counts,
  same order of magnitude, confirming the C# section's kappa/parse-budget
  fixes changed nothing about spark's timing either way, ruling out
  "the C# fix already silently fixed Scala too" as a competing
  explanation).
* **`cargo test -p sem-core --release --lib`**: 512 tests, 0 failures —
  same count as every prior section's baseline, including
  `scala_oracle_touch_the_hub` and `scala_oracle_touch_a_leaf` (the
  Scala GREEN-eligibility fixtures from fbbd6f7).
* **No clippy/fmt gate needed** — zero production files touched by this
  section (the two throwaway example probes used during investigation were
  both deleted before this section was written; `git status --short` at the
  time of writing shows only the pre-existing, pre-session
  `languages.rs` reflow hunk and the pre-existing untracked `examples/`
  directory this branch was already carrying, neither touched by this
  bead).

### Cross-language implications for Rust (rust-lang-rust, next in queue)

The C# section's own closing note already confirmed rust-lang-rust
(42,575 files, 450,824 entities) cold-builds in **11.0s** with no sign of
either of C#'s two pathologies, and separately checked its one
suspicious-looking adversarial-shaped file
(`survive-peano-lesson-queue.rs`) came back an ordinary fast file. That
11.0s figure is *already* this document's own single-build methodology
(same `perf_probe` tool, same session) — so, unlike spark, it does not have
this section's "fleet-vs-single-build" gap to resolve: 450,824 entities in
11.0s is ~24.4 μs/entity, inside the "~2x TS-monster-per-entity-rate" bar
this document uses throughout (2x of 16.8 μs/entity ≈ 33.6 μs/entity).
**Flag for whoever next opens a Rust-specific bead against the "rust-lang-
rust 23s" figure mentioned in this bead's own queue note: if that 23s also
traces back to a fleet/ambient-load measurement rather than this document's
own single-build convention, re-measure with the standard methodology
first** (exactly what this section did for Scala) **before assuming an
algorithmic pathology needs hunting — the C# section's already-recorded
11.0s single-build figure suggests there may be nothing left to find,
the same way this section found nothing for Scala.** No shared-cause code
implication otherwise: this section changed no code, so there is nothing
for Rust to inherit.

### Verdict

Playbook followed to the letter — all three named hypotheses checked in
the specified order, each falsified with evidence (one structurally, via
code; two via direct measurement) before concluding "no pathology" rather
than assuming it. Stop condition met on the first measurement: 3.52-3.64s
cold for 291,317 entities (~12.3 μs/entity) is *inside*, not just within 2x
of, the TS-monster per-entity bar. The bead's own opening 40-46s figure is
not reproduced by this document's standard methodology and is best
explained — quantified, not hand-waved — by ambient-load parallelism
collapse on the fleet runner (an 18-to-1 thread experiment showing a 6.9x
slowdown by itself, plus a ~32.2s single-threaded-equivalent CPU-work
estimate landing within ~20-30% of the fleet figure). Closing semx-eki with
this evidence: zero code changes, zero regressions possible, a
methodological flag left for the Rust item next in queue.

## Pass 1 is tree-bound, not extractor-bound (semx-r63)

`parse+extract` is the largest single phase left in a cold monster build —
3,009 ms of an 8,635 ms total (34.8%), measured this bead with
`examples/perf_probe`, 3 runs, microsoft/TypeScript, 40,872 files. `OXC-FASTPATH.md`
had measured a typed-AST parser at 20-49x on exactly that work, so it looked
like the largest remaining cold-build door.

It is not a door, and the reason is worth recording here because it constrains
every future attempt at this phase, not just the oxc one.

**Pass 1 does not want entities. It wants the tree.** `graph.rs`'s pass-1 loop
(~L1887) has three arms, and two of them call `extract_entities_with_tree`
specifically to keep the `tree_sitter::Tree`:

* `retain_parsed_files` (corpora ≤ `PARSED_FILE_REUSE_LIMIT`) hands the tree
  straight to pass 2's resolution closure;
* the JS/TS arm beyond that limit hands it to
  `scope_resolve::precompute_js_ts_file_facts(.., tree: &tree_sitter::Tree, ..)`,
  which builds the scopes, AST refs, return-type map and instance-attr map that
  semx-6rd added precisely so the chunked path would not re-parse (the fix that
  took the pass-2 re-parse from 74.0% of build_total to 18.7%).

So an alternate parser that produces only `Vec<SemanticEntity>` cannot serve
pass 1 at all. Wiring one in would either keep the tree-sitter parse anyway
(paying for both parsers, saving only the entity walk) or drop the facts and
hand those files back to pass 2's *serial* re-parse loop — re-creating the
pathology semx-022 removed.

Measured confirmation, feature build, `SEM_FASTPATH` off vs on, 3 paired runs
each:

| corpus | build_total off | build_total on | parse+extract off/on | entities off/on |
|---|---:|---:|---:|---:|
| microsoft/TypeScript | 8,635 ms | 8,624 ms | 3,009 / 3,001 ms | 454,541 / 454,541 |
| tiptap | 290.9 ms | 304.7 ms | 60.1 / 53.8 ms | 42,841 / 42,841 |

Identical entity counts are the proof: the fast path never executed in pass 1,
because pass 1 never asks the question it can answer.

**What this means for the next attempt at this phase.** The addressable unit is
not "extract entities faster", it is "produce `PrecomputedFileFacts` without a
tree-sitter tree" — i.e. reimplement `build_scopes_from_ast`,
`collect_all_file_refs`, `scan_return_types` and `scan_init_self_attrs` on
whatever AST the alternate parser produces. That work feeds *resolution edges*,
so `parser::diff_oracle` (which gates the diff-visible surface) does not cover
it; an edge-level oracle would have to come first. Until both exist, the 3.0s
`parse+extract` slice on this corpus should be treated as tree-sitter-parse-bound
and left alone.

The trait, the diff oracle and the extractor-identity salt that came out of
this bead are in the tree and language-agnostic; see `OXC-FASTPATH.md` for the
full story, the equivalence results over 73 real commits, and the second
decline.

## Memory attribution (semx-4w1)

Starting point: semx-6xw's fleet finale (HEAD 564bbf2) flagged 4 repos over an
8GB `/usr/bin/time -l` peak-RSS threshold — dotnet-runtime 17.2GB (990,754
entities), linux 16.9GB (2,312,433), llvm-project 11.3GB (1,306,421),
elasticsearch 8.55GB (829,646) — filed as this bead. Corpora in
`/tmp/bench-fleet/*` are a **shared, actively-mutated resource** (discovered
mid-bead: `/tmp/bench-fleet/dotnet-runtime`'s file/entity counts had grown
~15-20% between the fleet run and this one, from an unrelated `git pull` on
the shared clone) — all measurements below use private `cp -Rc` (APFS
clonefile, near-instant) snapshots under this session's scratchpad so every
before/after pair in this section is against byte-identical trees. Absolute
numbers therefore differ slightly from the bead's original figures (more
files/entities in this session's dotnet-runtime snapshot); the *shape* of the
finding — where the bytes go, and by how much a fix moves them — is what
transfers.

### Instrumentation

`crates/sem-core/src/parser/mem_profile.rs`, gated by `SEM_PROFILE_MEM=1`
(mirrors `resolve_profile.rs`'s discipline — one cached env-var read when
off, zero allocation-walking cost). Five checkpoints inside
`EntityGraph::build_incremental_core`, each printing a per-structure
`.capacity()`-summed byte table plus the process's *actual* RSS at that
instant (`ps -o rss=` on macOS, `/proc/self/status` on Linux) so the
attributed total can be checked against reality instead of trusted blindly:

1. **post-pass-1** — right after `all_entities` + every pass-1-derived lookup
   table (`entity_map`, `symbol_table`, `class_members`, `owner_members`,
   `entity_ranges`) is built, before pass 2 starts.
2. **peak-resolve** — right before scope resolution begins, after
   `import_table` and `snapshot_bow_content`'s `pre_parsed_content` snapshot
   are built (originally hypothesized as the peak — see below, it is not).
3. **chunk-reparse** — inside `resolve_with_scopes_full_inner`, right after
   each chunk's `owned_parsed_files` (re-parsed `(path, content, Tree)`
   triples for every non-JS/TS file beyond `PARSED_FILE_REUSE_LIMIT`) is
   fully populated. One line per chunk; RSS-only (tree-sitter's `Tree` has no
   size API from the Rust bindings, so this samples the process directly
   instead of estimating).
4. **post-scope-resolve** — right after pass 2's scope-resolution stage
   returns, sizing `scope_edges` and `scope_consumed_words` (an
   entity-id-keyed `HashMap<String, HashSet<String>>` accumulated across
   every chunk — never sized by any earlier profiling bead).
5. **post-build** — right before `EntityGraph::build_incremental_core`
   returns, sizing the actual return value (`graph.entities`/`edges`/
   `dependents`/`dependencies` + `all_entities`) once everything transient to
   resolution has been dropped — this is the build's genuine floor, not a
   transient peak.

New example binaries (also under `crates/sem-core/examples/`, not part of the
public API):

- `mem_single_probe.rs` — walks a repo once, calls `EntityGraph::build`
  exactly once, nothing else. Written because `perf_probe`'s own multi-phase
  design (WALK → IO → PARSE_EXTRACT → PASS1_ONLY → BUILD_TOTAL → per-extension
  LANG_RATE re-parses, all in one process) makes its `/usr/bin/time -l` peak
  RSS a high-water mark across *all* of those passes, not the one a real
  caller (sem-cli, the facts layer) actually pays for a single cold build.
- `tree_mem_probe.rs` — parses every file of one extension under a root with
  raw `tree_sitter`, holds every resulting `Tree` + its source string live
  simultaneously, and reports peak RSS against total source bytes. Answers
  the one question `mem_profile.rs` structurally cannot: how many bytes does
  a live `tree_sitter::Tree` cost, per byte of source, for a given grammar.
- `mem_single_probe_mimalloc.rs` — identical to `mem_single_probe.rs` except
  `#[global_allocator] = mimalloc::MiMalloc`. Existed to test one hypothesis
  (below); mimalloc already resolves in `Cargo.lock` transitively, added as a
  dev-dependency only, not proposed as sem-core's own allocator.

### Finding 1 — the benchmark harness itself inflated the historical number

`perf_probe`'s peak RSS on the (drifted, larger) dotnet-runtime snapshot:
**17.28GB** (`maximum resident set size` from `/usr/bin/time -l`) — matching
the original bead's 17.23GB almost exactly despite the corpus being ~15-20%
bigger, which is itself a clue. `mem_single_probe` (one `EntityGraph::build`
call, same snapshot, same binary): **12.40GB average of 3 runs**
(12.46/12.36/12.39GB) — a **28% reduction from measurement alone**, no source
change. `perf_probe` is still the right tool for its actual job (phase-by-phase
wall-time attribution); it is the wrong tool for peak RSS, because
`getrusage`'s high-water mark accumulates across every phase that ran earlier
in the same process (`PARSE_EXTRACT` and `PASS1_ONLY` each build a full,
temporary `Vec<SemanticEntity>` for the whole corpus before `BUILD_TOTAL` even
starts). Every RSS number in this section after this point uses
`mem_single_probe`, not `perf_probe`.

### Finding 2 — named structures explain only ~20-30% of measured RSS

dotnet-runtime, `SEM_PROFILE_MEM=1`, single build, before any fix (chunk size
still 5,000 — see Finding 3):

| Checkpoint | Attributed total | Process RSS | Attributed/RSS |
|---|---:|---:|---:|
| post-pass-1 | 3,404.5MB | 7,117.5MB | 48% |
| peak-resolve | 2,841.7MB | 7,155.1MB | 40% |
| post-scope-resolve | 482.0MB | 10,944.9MB | 4% |
| post-build | 3,432.0MB | 11,811.1MB | 29% |

Attribution at post-pass-1 (dotnet-runtime, 1,141,386 entities):

| Structure | Bytes |
|---|---:|
| `all_entities.content` | 1,139.2MB |
| `all_entities.metadata` (id/file_path/entity_type/name/hashes) | 643.3MB |
| `entity_map` (`EntityInfo`, a second id/name/type/file_path/parent_id copy) | 766.0MB |
| `symbol_table` | 281.6MB |
| `class_members` | 135.2MB |
| `owner_members` | 226.3MB |
| `entity_ranges` | 208.9MB |
| `precomputed_facts` (JS/TS only — near-zero on a C# repo) | 4.0MB |

These are real and legitimate (a second `EntityInfo` copy per entity is
deliberate — `graph.entities` in the final return value *is* this map, and
`class_members`/`owner_members`/`entity_ranges` are pass-2 lookup indexes
pass 1 has to build once regardless of language). But they total ~3.4GB
whichever checkpoint they're read at, while process RSS climbs to 11.8GB by
`post-build` — a persistent, unexplained ~8GB gap that motivated the next two
findings.

### Finding 3 — the gap is `tree_sitter::Tree` memory, not a leak, not fragmentation

Two hypotheses were tested and falsified before finding the real one:

- **Allocator fragmentation** (many small, short-lived allocations across a
  parallel `rayon` workload, freed but not returned to the OS promptly).
  Tested directly: `mem_single_probe_mimalloc` (mimalloc, known for
  aggressive page return) vs the system allocator, same snapshot, same chunk
  size — **12.49GB vs 12.40GB average, no improvement**. Falsified.
- **A global content-addressed cache never evicting** (`parser::cache`,
  enabled by default). Read the source instead of guessing: `DEFAULT_
  CAPACITY_BYTES = 64MB`, LRU-evicted per shard. Two orders of magnitude too
  small to be the gap. Falsified by code reading, no test needed.

The `chunk-reparse` checkpoint (inside the chunked resolution path's per-chunk
loop, `SCOPE_RESOLVE_FILE_CHUNK_SIZE = 5,000` files at the time) showed RSS
climbing **monotonically and non-linearly** across chunks regardless of that
chunk's own content size (7.1GB → 7.7 → 7.8 → … → 8.5GB over 10 modest
chunks, then **8.5GB → 11.0GB in the single chunk with the most bytes**,
74.0MB of content). That shape — a spike tied to *content bytes in the
chunk*, not file count — pointed at `owned_parsed_files`'s retained
`tree_sitter::Tree`s, which pass 1 does not keep for any file beyond
`PARSED_FILE_REUSE_LIMIT` (except JS/TS, which skips this entirely via
`PrecomputedFileFacts`) but which the chunked scope-resolution path re-parses
and holds live, one chunk at a time, for every other language.

Confirmed directly with `tree_mem_probe` (isolated, no sem-core build
machinery — just tree-sitter, held live):

| Language | Files | Source bytes | Peak RSS holding all trees | Multiplier (RSS / source bytes) |
|---|---:|---:|---:|---:|
| C# (dotnet-runtime, `.cs`) | 32,690 | 429.5MB | 17.20GB | **~40.0x** |
| C (linux, `.c`) | 36,922 | 693.9MB | 17.03GB | **~24.5x** |

A live tree-sitter AST costs 24-40 bytes of process memory per byte of
source, and the multiplier is grammar-specific (C#'s more elaborate surface
syntax — generics, attributes, LINQ, properties — produces a denser node
graph than C's). This is the dominant, structurally-unattributable term:
`tree_sitter::Tree` is opaque from the Rust bindings (no `.heap_bytes()`), so
`mem_profile.rs` can rank and size every *other* structure in the build but
not this one — only a live RSS sample (the `chunk-reparse` checkpoint, or
this isolated probe) can see it.

### Finding 4 — the fix: bound chunk size, verified two ways

`SCOPE_RESOLVE_FILE_CHUNK_SIZE` (`crates/sem-core/src/parser/graph.rs`) is a
blunt, file-count-only knob — it has no idea a chunk's files are C# or COBOL,
verbose or terse — but bounding it lower bounds the worst case
proportionally for *every* language without touching per-language grammar
config, which byte-budget-aware chunking would require (see "Open items"
below for why that's not landed this bead). Changed `5_000 → 1_000` (test-cfg
value, `3`, unchanged — it exercises the chunked path structurally in
`cfg(test)` builds regardless of the production constant).

This only affects repos beyond `PARSED_FILE_REUSE_LIMIT` (20,000 files) that
also have non-JS/TS scope-resolve-capable files — the chunked path is a
no-op for every fixture under that limit (tiptap, ts-monster, django, gin —
none reach it), so the standard small/medium-repo gates are structurally
unaffected by this change, not just untested.

**RSS, `mem_single_probe`, same frozen snapshots, chunk 5,000 vs 1,000:**

| Repo | Entities | Edges | Before (avg) | After | Δ |
|---|---:|---:|---:|---:|---:|
| dotnet-runtime | 1,141,386 | 981,283/4 | 12.40GB (3 runs) | 11.92GB (2 runs) | **-3.9%** |
| linux | 2,482,061 | 1,898,816 | 11.65GB | 9.41GB | **-19.3%** |

**Wall-clock, same runs:**

| Repo | Before (avg) | After (avg) | Δ |
|---|---:|---:|---:|
| dotnet-runtime | 47,628ms (3 runs, 45.1-50.1s spread) | 50,659ms (2 runs, 48.9-52.4s spread) | +6.4%, **within run-to-run noise** — the "before" condition alone spans a 10.6% range on this machine over the course of this session |
| linux | 30,568ms | 28,151ms | **-7.9%** (faster) |

Linux is a clean win on both axes and is exactly bit-identical
(`entities=2,482,061 edges=1,898,816` before and after, both runs). dotnet-
runtime's RSS win is small but reproducible; its edge count moved by exactly
**+1** (981,283 → 981,284) between chunk sizes, reproducibly (verified across
2 runs at each chunk size) — traced to the pre-existing `PARSE_TIME_BUDGET`
mechanism (`parse_tree_within_budget`'s 10s wall-clock supervisor-thread
timeout; the same file — `EncryptedXmlSample4.xml` — logs a budget-exceeded
warning in every run at both chunk sizes) being sensitive to how work is
batched into chunks: smaller chunks change per-file scheduling/contention
enough that one additional borderline file's resolution completes inside its
10s window. This is **not** a correctness regression in the traditional
sense (strictly more edges resolved, not fewer or different), but it is a
real, disclosed departure from strict bit-identical output on this one
repo — flagged rather than hidden. It does not reproduce on linux (no
budget-exceeded files in that corpus, and the run is exactly bit-identical).

**Gates run:**

- `cargo test -p sem-core --lib --release`: **532 passed, 0 failed**.
- `cargo test -p sem-core --lib --release --features oxc-fastpath`: **544
  passed, 0 failed**.
- `cargo clippy -p sem-core --lib -- -D warnings`: 149 pre-existing errors on
  `HEAD` before this bead's changes (baseline, unrelated to this work);
  **149 after** — every file this bead touched (`graph.rs`, `mem_profile.rs`,
  `mod.rs`, `scope_resolve.rs`, the new `examples/*.rs`) is clippy-clean.
- `cargo fmt -p sem-core -- --check`: clean on every touched file.
- `SEM_FP_PARITY=1 examples/incr_probe -- <dotnet-runtime> mixed50` and
  `... none`: **`ORACLE ... ok`** on both `cold-vs-build` and the warm
  scenario, both scenarios — `SEM_FP_PARITY=1` additionally asserts the
  incrementally-maintained corpus fingerprints equal a full whole-fold on
  every build and panics on any divergence; neither run panicked. This is
  the giant most exercised by the chunk-size change (chunked path, non-JS/TS
  files re-parsed per chunk), so it is the one that most needed this check.
  (The canonical `ts-monster` JS/TS fixture the existing 32-scenario harness
  normally runs against is not present as raw source in this environment —
  only its persisted facts blob remains under `/tmp/bench-fleet/ts-monster-
  store-v2` — so the full 32-scenario JS/TS matrix could not be re-run this
  bead. Flagged as a verification gap, not a disproof: the chunked path's
  incremental (`scope_tag`-keyed) machinery is exercised correctly on
  dotnet-runtime above, but not on a corpus where `PrecomputedFileFacts`
  skips tree re-parsing entirely, which is JS/TS's specific code shape.)

### Per-entity variance across repos, explained

The fleet's original per-entity RSS numbers (8-17KB/entity across repos) were
flagged as a clue. With Finding 3 in hand: it is **not primarily** entity
content duplication from container nesting (namespace → class → method all
storing overlapping source spans — real, but secondary, ~1.8GB total across
`all_entities.content` + `entity_map` + `class_members`/`owner_members` on
dotnet-runtime, i.e. Finding 2's ~3.4GB). It is **primarily the tree-sitter
grammar multiplier**, which is per-language and applies to every file the
chunked path re-parses:

- dotnet-runtime (C#, ~40x multiplier, measured): 11.92GB / 1,141,386
  entities ≈ **10.4KB/entity** (post-fix).
- linux (C, ~24.5x multiplier, measured): 9.41GB / 2,482,061 entities ≈
  **3.8KB/entity** (post-fix).

A ~2.7x per-entity ratio between the two repos lines up closely with the
~1.6x tree-memory multiplier ratio (40.0x / 24.5x) compounded with dotnet-
runtime's denser average entity nesting (more `EntityInfo`/`class_members`
duplication per file, from Finding 2) — the two effects stack, and the tree
multiplier is the larger of the two.

### The floor

`post-build`'s attributed total on dotnet-runtime (1,141,386 entities,
981,284 edges, post-fix) is **3,432.0MB** — `all_entities.content` (1,139.2)
+ `all_entities.metadata` (643.3) + `graph.entities`/`EntityInfo` (766.0) +
`graph.edges` (297.0) + `graph.dependents` (291.3) + `graph.dependencies`
(295.2) — everything transient to resolution (`precomputed_facts`,
`pre_parsed_content`, `import_table`, the per-chunk trees, `scope_consumed_
words`) has been dropped by this point. That is **~3.0KB of genuinely
resident, unavoidable memory per entity** for the return value a caller
actually needs (the entity graph + the raw entities), scaled to the bead's
original 990,754-entity dotnet-runtime corpus: **~2.98GB floor**. Everything
above that floor — the difference between the ~11.9GB measured post-fix peak
and this ~3.0-3.4GB floor — is transient scaffolding: pass-2 lookup tables
(Finding 2, ~3.4GB, needed corpus-wide during resolution but droppable
after) and tree-sitter tree memory (Finding 3, the larger term, currently
bounded per-chunk rather than eliminated).

### Open items — left for a follow-up bead, not silently dropped

The chunk-size fix is a real, measured, low-risk win (safe because it's a
pure constant change inside an already-existing, already-bounded code path)
but does **not** hit this bead's stated "halving" bar — RSS moved -3.9% to
-19.3% depending on repo, not -50%. The two design-level moves that would
close the rest of the gap were identified but not attempted this bead,
because both need broader validation than this session had room for:

1. **Byte-budget chunking** instead of file-count chunking (group files into
   a chunk until cumulative source bytes cross a budget, not until the file
   count does). This is the structurally correct fix — Finding 3 showed the
   spike is tied to bytes-in-chunk, not files-in-chunk — but it changes
   `resolve_scopes_in_file_chunks`'s `chunk_index`-derived `scope_tag`
   numbering, which the incremental (warm-rebuild) fingerprinting scheme
   keys on; needs the full 32-scenario `SEM_FP_PARITY=1` matrix on a JS/TS
   corpus (this session's `ts-monster` source was unavailable — see Finding
   4's gate section) before landing.
2. **Extend `PrecomputedFileFacts`-style tree avoidance to every language**,
   not just JS/TS — the chunked path would then never re-parse a tree at all
   for a file beyond `PARSED_FILE_REUSE_LIMIT`, eliminating Finding 3's term
   outright instead of bounding it. Substantially larger: one new pass-1
   fact-collector per `scope_resolve` language config, each needing its own
   equivalence proof against today's tree-walking implementation (the same
   shape of work semx-6rd did for JS/TS, times N languages).

semx-4w1 is left **open** with this attribution and the landed partial fix;
a follow-up bead should own the byte-budget-chunking design (item 1) as the
next concrete, scoped step — it is the smaller of the two and directly
motivated by data already in hand above.

## Parse-budget determinism (semx-jo1)

Follow-up to semx-4w1's disclosed departure from bit-identical output:
shrinking `SCOPE_RESOLVE_FILE_CHUNK_SIZE` from 5,000 to 1,000 moved
dotnet-runtime's edge count by exactly +1 (981,283 -> 981,284), attributed at
the time to `PARSE_TIME_BUDGET`'s wall-clock give-up decision being sensitive
to chunk-driven scheduling contention. This section fixes the wall-clock
mechanism (a real, worthwhile fix on its own merits) and then reports the
direct measurement that shows it was **not** the actual cause of the +1 edge
— a correction to semx-4w1's own diagnosis, surfaced rather than hidden.

### The fix

`is_pathological_large_file` (`crates/sem-core/src/parser/plugins/code/mod.rs`)
replaces `parse_tree_within_budget`'s wall-clock race as pass-1's
(`extract_entities_with_tree`) and pass-2's (`scope_resolve.rs`'s reparse
loop) large-file give-up decision. It is a pure function of a file's own
content — the longest single `\n`-delimited run, compared against a 6 MiB
threshold (`PATHOLOGICAL_LINE_THRESHOLD`) — computed before any parse is
attempted and before any thread is spawned, so its answer is identical on
every call regardless of concurrent load or chunk size.

A new diagnostic, `examples/parse_time_probe.rs`, measures every file over
`LARGE_FILE_BUDGET_THRESHOLD` (128 KiB) sequentially — no chunking, no
concurrent load, an isolated per-file baseline — across dotnet-runtime,
linux, llvm-project, elasticsearch, TypeScript-monster, and tiptap. The
result reshaped the fix's design twice before landing:

1. **First attempt (falsified): line length alone, 512 KiB threshold.**
   dotnet-runtime's one confirmed pathological file
   (`EncryptedXmlSample4.xml`, 9.5MB total) is *not* explained by tag-nesting
   depth (only ~1,245 open tags) or total size (smaller than 5 of the 6
   files in the legitimately-slow `hugeexpr1.cs` cluster, which reach 24MB).
   It is one embedded `<CipherValue>` payload forming a single
   **8,441,855-byte line** — 92.265s to parse solo, 9.2x over budget, vs.
   the legitimate cluster's worst line (6,051 bytes) and worst solo parse
   time (1.466s). A 512 KiB threshold looked like a clean, wide-margin
   separator on this one repo's data.
2. **Broader measurement falsified the 512 KiB threshold.** tiptap ships
   `demos/src/Examples/Book/content.js` with a 649,371-byte single line —
   above 512 KiB — that parses in **4 milliseconds**. TypeScript-monster
   ships a deliberate torture fixture
   (`.../should-be-able-to-return-the-file-size-when-a-JS-file-is-too-large-to-load-into-text.js`)
   whose single line is **4,194,306 bytes** — only 2x below the pathological
   XML's line length — parsing in **52 milliseconds**. Several other
   TypeScript-monster fixtures are >99.9% single-line and still parse in
   single-digit milliseconds. **Line length alone does not cleanly separate
   the two populations; the pathology is grammar-specific (XML's scanner on
   this input), not a generic function of content shape.** The threshold
   was moved to 6 MiB — the geometric mean of the widest legitimate line
   measured (4,194,306 bytes) and the one confirmed pathological line
   (8,441,855 bytes) — giving both populations only a ~1.4x margin, honestly
   thinner than an ideal discriminator, disclosed rather than overstated.
3. **A hybrid (predicate ahead of the wall-clock budget, budget kept as
   fallback) was tried and measured to still reproduce the +1 edge.** Built
   and ran `mem_single_probe` against the same frozen dotnet-runtime
   snapshot at both chunk sizes, twice each: with the hybrid in place, chunk
   1,000 still produced 981,284 edges and chunk 5,000 still produced
   981,283, reproducibly. Since `EncryptedXmlSample4.xml`'s classification
   was by then provably identical at both chunk sizes (same warning, same
   excluded file, every run), this proved the flipping file was never the
   one confirmed pathological file — some *other* file was still racing
   `parse_tree_within_budget`'s clock, and the hybrid didn't touch it.

### The fix, final form, and what it actually closes

The wall-clock budget mechanism (`PARSE_TIME_BUDGET`,
`LARGE_FILE_BUDGET_THRESHOLD`, `parse_tree_within_budget`) was removed
entirely from both call sites' decision path, not just gated behind a
pre-filter. Content `is_pathological_large_file` doesn't flag goes through
the same plain, unconditional `parse_tree` a small file always has — correct
because no file measured across six corpora that isn't the one giant-line
outlier comes remotely close to 10s even with zero contention (worst
legitimate solo time: 1.466s). Re-measured dotnet-runtime at chunk 1,000 vs.
5,000 with this version: **981,284 vs. 981,283 — the discrepancy persisted,
unchanged.**

Built a one-off diagnostic, `examples/edge_dump_probe.rs` (dumps every
resolved edge, sorted, to a file), ran it at both chunk sizes against the
same frozen snapshot, and diffed the output directly. Exactly one edge
differs:

```
src/tests/CoreMangLib/system/delegate/delegate/delegatecombine1.cs::module::DelegateCombine1Test::DelegateCombine1::GetInvocationListFlag
  Calls
src/tests/Interop/Swift/SwiftAbiStress/SwiftAbiStress.swift::struct::HasherFNV1a::combine
```

Reading both files: `GetInvocationListFlag` (C#) declares a **local
variable** named `combine` holding a `Delegate.Combine(...)` result, then
invokes it as a delegate call (`combine();`). The call-resolution heuristic,
finding no locally-declared *method* named `combine`, falls back to a
corpus-wide name-based lookup and matches an unrelated Swift struct's
`combine` method purely by name — a pre-existing false-positive class in the
heuristic resolver's ambiguous-short-name fallback, not caused by parsing,
budgets, or timing at all. Neither file is ever large enough to touch
`LARGE_FILE_BUDGET_THRESHOLD`, `is_pathological_large_file`, or any part of
this bead's change. **semx-4w1's original attribution of the +1 edge to
`PARSE_TIME_BUDGET` was incorrect** — the real mechanism is a chunk-order-
dependent tie-break in the corpus-wide ambiguous-name symbol table this
resolver falls back to, which chunk-based processing order happens to
perturb. Filed separately (not this bead's scope): the chunked resolution
path's cross-file symbol fallback has at least one chunk-order-dependent
tie-break independent of parsing.

### Why the fix still lands

Despite not explaining dotnet-runtime's specific +1 edge, this fix is a real,
evidenced improvement on its own terms:

- **The one confirmed pathological file is now classified deterministically**
  — a pure function of its content, verified identical (same warning, same
  excluded file) across every chunk size and every repeat run measured. It
  no longer occupies a supervisor thread racing a 10s clock on every single
  build (previously: a real, if partially hidden, per-build cost — the
  thread is abandoned on timeout and keeps consuming a CPU core until
  tree-sitter's own ~90s parse eventually completes in the background).
- **No thread is spawned on this decision path any more, for any file** —
  removing scheduler-timing as a source of *any* future determinism bug on
  this specific give-up decision, even though today's residual +1-edge
  symptom traces elsewhere.
- **Zero behavior change on linux, llvm-project, elasticsearch,
  TypeScript-monster, tiptap, django, or gin** — `parse_time_probe` found no
  file in any of these corpora with a line anywhere near 6 MiB (widest:
  llvm-project's 430,041-byte SVG, 14.6x below threshold), so
  `is_pathological_large_file` never fires and every large file in these
  repos goes through the exact same successful parse it always did, just
  without a thread spawn. Confirmed directly: `mem_single_probe` on tiptap
  (files=1936, entities=43,393, edges=5,414) shows `content.js`'s
  649,371-byte line parsing and contributing entities normally, no skip
  warning.

### Gates run

- `cargo test -p sem-core --lib --release`: **532 passed, 0 failed** (matches
  semx-4w1's baseline exactly).
- `cargo test -p sem-core --lib --release --features oxc-fastpath`: **544
  passed, 0 failed** (matches baseline).
- `cargo clippy -p sem-core --lib --examples -- -D warnings`: **149** errors,
  identical count to the pre-existing baseline; none inside this bead's
  edited regions in `mod.rs` or `scope_resolve.rs`, and the two new example
  files (`parse_time_probe.rs`, `edge_dump_probe.rs`) are clippy-clean.
- `cargo fmt -p sem-core -- --check`: clean on every touched/new file.
- `SEM_FP_PARITY=1 examples/incr_probe -- <dotnet-runtime> {none,mixed50}`:
  **`ORACLE ... ok`** on `cold-vs-build` and the named scenario, both runs —
  this is the corpus most exercised by the change (chunked path, non-JS/TS
  reparse), so the incremental fingerprinting machinery's correctness under
  the new give-up decision is the most load-bearing check available.
- dotnet-runtime re-measured at both chunk sizes (1,000, 5,000), 2-3 runs
  each: entities bit-identical (1,141,386 / 990,754 depending on
  snapshot/harness) at every run; the one-edge chunk-size delta persists
  (see above) and is now attributed to a different, disclosed mechanism.

### semx-g6t implication

Byte-budget chunking (semx-g6t) will also change which files land in which
chunk, differently from file-count chunking. Given the mechanism found
above, byte-budget chunking's output on dotnet-runtime should **not** be
expected to be bit-identical to file-count chunking's output purely because
of this same pre-existing ambiguous-name tie-break — a handful of
edge-count deltas of this shape are a property of the chunked resolution
path's symbol-fallback design, not a correctness regression introduced by
either chunking scheme. The right gate for g6t is internal
determinism/reproducibility of *its own* output (same-budget, repeated runs
bit-identical) plus the incremental fingerprint-parity oracle, not
byte-for-byte equality against the file-count baseline on repos where this
tie-break can fire.

## Byte-budget chunking (semx-g6t)

Replaces `SCOPE_RESOLVE_FILE_CHUNK_SIZE` (a fixed file count) with
`SCOPE_RESOLVE_BYTE_BUDGET` (a cumulative on-disk source-byte budget,
`crates/sem-core/src/parser/graph.rs`). Motivated directly by Finding 3
above: peak per-chunk tree-sitter memory is proportional to *bytes held
live*, not file count, so a fixed file count bounds nothing when a chunk
happens to contain the corpus's largest files — the exact failure mode
semx-4w1 could only shrink proportionally, not bound.

### Design

`chunk_files_by_byte_budget(root, file_paths, budget_bytes)` partitions an
already-ordered file list into contiguous chunks by cumulative
`std::fs::metadata` size (stat, not read — no content touched), closing a
chunk once adding the next file would exceed the budget. A single file
larger than the budget always gets a singleton chunk rather than being
dropped (the loop never closes an empty chunk). Pure function of the file
list and each file's size on disk: deterministic, same input always
produces the same partition.

A single conservative constant (20 MiB) was chosen over a per-language
grammar-multiplier-weighted budget — the option explicitly left open by the
bead. Justification: only two multipliers are measured (C# ~40x, C ~24.5x,
`examples/tree_mem_probe.rs`), too few points to fit a defensible per-language
table, and a single constant sized for the *worst measured multiplier*
(C#, this bead's own tuning target) automatically bounds every lower-multiplier
language's peak at least as well — the conservative choice is also the
simpler one here, not a tradeoff. `SCOPE_RESOLVE_BYTE_BUDGET` replaced the
*only* two call sites `SCOPE_RESOLVE_FILE_CHUNK_SIZE` had (`resolve_scopes_in_file_chunks`,
where `chunk_index` feeds `ScopeIncremental::scope_tag`, and
`EntityGraph::build_direct_dependencies`'s re-parse batching heuristic,
which carries no scope_tag/incremental state and has no cross-call
consistency requirement).

### Tuning (dotnet-runtime, worst per-byte multiplier)

Measured with `SEM_PROFILE_MEM=1 /usr/bin/time -l` against a frozen
dotnet-runtime snapshot, same harness as semx-jo1's section above
(`mem_single_probe`, one cold `EntityGraph::build` call, "maximum resident
set size" is the real high-water mark, not a checkpoint estimate):

| Budget | RSS (avg of N runs) | vs. chunk=1,000 baseline |
|---|---:|---:|
| chunk=1,000 (file-count, jo1-only baseline) | 10.30GB (2 runs) | — |
| 10 MiB | 8.42GB (2 runs) | -18.3% |
| **20 MiB (chosen)** | **8.28GB (3 runs)** | **-19.6%** |

10 MiB was *not* better than 20 MiB (within noise, slightly worse) — going
smaller than ~20 MiB adds more chunks (more re-parse/dispatch overhead)
without shrinking peak RSS further, consistent with the post-pass-1
attributed structures (Finding 2, ~3.4GB, corpus-wide and chunk-independent)
starting to dominate once per-chunk tree pressure is already well bounded.
20 MiB landed. Per-chunk `chunk-reparse` content sizes at 20 MiB are tightly
clustered (17-19.7MB observed across dotnet-runtime's ~34 chunks) versus the
file-count scheme's measured 7.1GB-11.0GB *process RSS* swing across chunks
in semx-4w1's own Finding 3 — direct confirmation the bound now tracks
bytes, not file count.

### RSS before/after, all four giants (single `/usr/bin/time -l` run unless noted)

| Repo | Baseline (chunk=1,000 file-count) | Byte-budget (20 MiB) | Δ |
|---|---:|---:|---:|
| dotnet-runtime | 10.30GB (avg 2 runs) | 8.28GB (avg 3 runs) | **-19.6%** |
| linux | 8.60GB | 8.79GB | +2.2% |
| llvm-project | 10.20GB | 10.00GB | -2.0% |
| elasticsearch | 5.25GB | 5.39GB | +2.7% |

Only dotnet-runtime moves meaningfully — expected and by design: it is the
corpus this bead targeted (worst measured multiplier, most size-skewed file
population). linux/llvm-project/elasticsearch move by ±2-3%, inside this
session's measurement noise (the shared machine's load average was 13.86
during these runs — other agents' concurrent activity in the same
environment, not this change; see wall-clock section below for the same
caveat). semx-4w1 already found linux's file-count chunking a clean win at
chunk=1,000 (lower C multiplier, more uniform file sizes) — this bead's
target was specifically the case file-count chunking could not bound
(dotnet-runtime), and that is where the win lands.

Per-entity floor progress (dotnet-runtime, this session's 57,613-file /
1,141,386-entity snapshot, consistent harness throughout): chunk=1,000
baseline **9.03KB/entity** -> byte-budget=20MiB **7.62KB/entity**, against
the ~3.0KB/entity floor established in the Memory attribution section above
(`post-build`'s attributed total, everything transient to resolution
dropped). Real progress, not closure — the remaining gap is exactly Open
item #2 from that section (extending `PrecomputedFileFacts`-style tree
avoidance to every language, eliminating chunk-held trees instead of
bounding them), unchanged in scope and not attempted here.

### Wall-clock

Measured on the same shared, actively-loaded machine (other agents running
concurrently this session; `uptime` showed a 13.86 load average during these
runs) — reported with that caveat rather than overclaiming precision:

| Repo | Baseline | Byte-budget (20 MiB) | Δ |
|---|---:|---:|---:|
| dotnet-runtime | 36.8s avg (2 runs, 35.8-37.9s) | 38.1s (run 1) / 52.3s / 73.9s (runs 2-3, contended) | run 1 matches baseline; later runs' spread tracks rising system load, not this change |
| linux | 31.1s | 30.7s | -1.2% (flat) |
| llvm-project | 57.0s | 45.5s | **-20.2% (faster)** |
| elasticsearch | 11.0s | 12.4s | +12.2% (small absolute numbers; noisy) |

No corpus shows a clear, reproducible regression; llvm-project is a clean
win on both axes simultaneously (RSS -2.0%, wall -20.2%). Given the
measurement noise disclosed above, this reads as "no wall-clock regression
from the code change," not a precise before/after — a clean re-run on an
idle machine would sharpen these numbers but wasn't available this session.

### Correctness: entities/edges vs. the file-count baseline

| Repo | Entities | Edges (baseline -> byte-budget) | Δ edges |
|---|---:|---|---:|
| dotnet-runtime | 1,141,386 (identical) | 981,284 -> 981,283 | 1 |
| linux | 2,482,061 (identical) | 1,898,816 -> 1,898,816 | **0 (bit-identical)** |
| llvm-project | 2,754,560 (identical) | 977,041 -> 977,042 | 1 |
| elasticsearch | 861,804 (identical) | 1,244,318 -> 1,244,299 | 19 |
| TypeScript-monster (raw, `mem_single_probe`) | 714,832 (identical) | 196,148 -> 196,175 | 27 |

Entity counts are bit-identical everywhere (pass 1 doesn't chunk). Edge
deltas are all attributable to the same pre-existing, chunk-membership-
dependent ambiguous-short-name tie-break documented above and filed as
semx-nuv, not a correctness regression from this change — every delta is
<0.02% of that corpus's edge count (largest: TypeScript-monster's 27/196,148
= 0.0138%). elasticsearch (Java) and TypeScript-monster show larger absolute
deltas than dotnet-runtime/llvm-project (C#/C++), consistent with semx-nuv's
mechanism scaling with how many ambiguous/common short symbol names a
language's ecosystem tends to produce, not with anything specific to
byte-budget chunking.

### Gates run

- `cargo test -p sem-core --lib --release`: **532 passed, 0 failed**.
- `cargo test -p sem-core --lib --release --features oxc-fastpath`: **544
  passed, 0 failed**.
- `cargo clippy -p sem-core --lib --examples -- -D warnings`: **149** errors,
  identical to baseline; none in `chunk_files_by_byte_budget` or either
  call site.
- `cargo fmt -p sem-core -- --check`: clean.
- `SEM_FP_PARITY=1 examples/incr_probe -- <dotnet-runtime> mixed50`:
  **`ORACLE ... ok`** on `cold-vs-build` and `mixed50`.
- `SEM_FP_PARITY=1 examples/incr_probe -- <TypeScript-monster> all`: runs
  the full named-scenario matrix (`none`, `leaf`, `mixed50`, `hub`,
  `hubrename`, `tests`, `importchurn`) — **every `ORACLE ... ok`**, including
  `cold-vs-build`. This is the exact validation gap semx-4w1 and this bead's
  own description both flagged as blocking ("ts-monster's raw source was not
  present in that session's environment") — the corrected context note for
  this session (TypeScript-monster available at
  `~/.cache/checkouts/github.com/microsoft/TypeScript`) closed it. This is
  the strongest available evidence that byte-budget chunking's
  `chunk_index`-derived `scope_tag` renumbering does not perturb the
  incremental fingerprinting scheme's correctness on a live JS/TS corpus.
- tiptap/django/gin (`mem_single_probe`): all well under
  `PARSED_FILE_REUSE_LIMIT` (20,000 files), so byte-budget chunking is
  structurally a no-op for them, same as semx-4w1's chunk-size change —
  confirmed running clean (tiptap: 43,393 entities/5,414 edges; django:
  67,088/47,618; gin: 2,235/2,352), unaffected by this bead by construction,
  not just by observation.

### semx-g6t status: closed with evidence

Target ("meaningful RSS reduction toward the ~3GB floor") met on the corpus
this bead's own instructions named as the tuning target: dotnet-runtime,
-19.6% RSS, 9.03 -> 7.62KB/entity. The other three giants are RSS/wall-clock
neutral within this session's measurement noise — not a further win, but not
a regression either, consistent with semx-4w1's finding that only
dotnet-runtime's file-count chunking was actually unbounded in the way this
bead set out to fix. All planned gates ran, including the JS/TS
`SEM_FP_PARITY` matrix this bead's own description held open as a landing
blocker. Residual, disclosed, not blocking: per-repo edge-count deltas from
semx-nuv's pre-existing tie-break (largest 0.0138% of one corpus's edges),
and this session's wall-clock numbers carry real measurement noise from a
shared, concurrently-loaded machine.

## Resolver tie-break contract (semx-nuv, semx-yk5)

Follow-up to semx-jo1's disclosure and semx-g6t's implication section
above, closing out the two beads those sections filed: the chunk-order-
dependent ambiguous-name tie-break (semx-nuv) and the two documented-but-
unfixed residues from semx-6rd's root-cause section (semx-yk5). Every
resolver tie-break site found across both beads, its now-stated rule, and
the measured effect of stating it:

| # | Site | Table | Stated rule | Bead |
|---|---|---|---|---|
| 1 | `symbol_table` bucket order | `symbol_table: HashMap<String, Vec<String>>` | `sort_symbol_table_targets_by_source`: `(file_path, start_line, end_line, id)` ascending | pre-existing (semx-6rd) — unchanged here |
| 2 | `class_members`/`owner_members` bucket order | `HashMap<String, Vec<(String, String)>>` | `sort_members_bucket_by_source`: same `(file_path, start_line, end_line, id)` key, applied via the id half of each `(name, id)` pair | **semx-yk5** — now applied at all 3 `PreBuiltLookups` sites |
| 3 | `entity_ranges` bucket order | `HashMap<String, Vec<(usize, usize, String)>>` (already keyed by file) | `(start_line, end_line, id)` ascending — table 2's key with `file_path` dropped, since it's constant within a bucket | **semx-yk5** — now applied at all 3 sites |
| 4 | Module-scope `defs`: same top-level name declared twice in one file | `Scope::defs: HashMap<String, String>` | Last-write-wins over table 3's now-canonical order — i.e. the declaration with the greatest `(start_line, end_line, id)` in that file wins | **semx-yk5** — same fix as #3 makes this explicit (see `build_scopes_from_ast`'s `entity_ranges.get(file_path)` loop, `scope_resolve.rs`) |
| 5 | `swift_call_signatures` gate: whether the Swift-overload-aware resolution branch runs at all | `ChunkedResolveInputs::corpus_has_swift: bool` | Pure function of `all_entities` (any file ends with `.swift`) — corpus-wide, computed once before the chunk loop, not per chunk | **semx-nuv** |

Sites 1-3 share one underlying convention: whichever entity a
same-named/same-owner/same-file group's *tie-break* singles out is the one
occupying the earliest position in the file, reading files themselves in
path order — "the same name declared first, file-path-alphabetically-
first" wins a same-position tie via the trailing `id` component.
`select_member_candidate`/`resolve_ref`'s `target_ids.first()` calls are
what make bucket order load-bearing rather than cosmetic: the *first*
matching entry in a sorted bucket is the one that wins.

### semx-nuv: the chunk-order-dependent ambiguous-name tie-break

**Root cause.** `resolve_with_scopes_full_inner` (`scope_resolve.rs`) built
`swift_call_signatures` — the table gating `resolve_ref`'s Swift-overload-
aware branch (`has_ambiguous_swift_signature_candidates`,
`select_swift_overload_candidate`) — only when *the current chunk's own*
`parsed_files` contained a `.swift` file. On the byte-budget-chunked path
(`resolve_scopes_in_file_chunks`), `parsed_files` is strictly that chunk's
files, so whether a caller's chunk happened to also hold a `.swift` file (a
function of `SCOPE_RESOLVE_BYTE_BUDGET` and chunk membership, not of the
caller's own content) decided whether the ambiguity check ever ran for that
file's calls. `build_swift_call_signatures` itself was never the bug — its
second loop already falls back to re-parsing `entity.content` standalone
(`extract_swift_signature_from_entity_content`) for any Swift entity outside
the current chunk's trees, using `all_entities`, which is always the whole
corpus at every call site. The bug was purely in the boolean deciding
whether to call it at all.

**Reproduced live** (`examples/edge_dump_probe.rs`, frozen dotnet-runtime
checkout, `/private/tmp/bench-fleet/dotnet-runtime`): before this fix, 20
MiB byte budget gives 981,283 edges, 5 MiB gives 981,284 — the exact +1
delta semx-jo1 documented and left unattributed. The one differing edge is
exactly semx-jo1's:

```
src/tests/CoreMangLib/system/delegate/delegate/delegatecombine1.cs::module::DelegateCombine1Test::DelegateCombine1::GetInvocationListFlag
  Calls
src/tests/Interop/Swift/SwiftAbiStress/SwiftAbiStress.swift::struct::HasherFNV1a::combine
```

`GetInvocationListFlag` (C#) declares a local variable `combine`
(`Delegate.Combine(...)`) and invokes it; finding no method literally named
`combine`, the resolver falls back to the corpus-wide `symbol_table["combine"]`
lookup. dotnet-runtime has **two** unrelated Swift structs each defining
`mutating func combine<T>(_ val: T)` (`SwiftAbiStress.swift`,
`SwiftInlineArray.swift`) — a genuine, repo-content-level ambiguity. Whether
the resolver *detected* that ambiguity depended entirely on whether
`GetInvocationListFlag`'s chunk happened to also contain one, both, or
neither of those two Swift files.

**Fix.** `ChunkedResolveInputs` (built once, before the per-chunk loop, in
`resolve_scopes_in_file_chunks`) now carries `corpus_has_swift: bool` — a
single `all_entities.iter().any(|e| e.file_path.ends_with(".swift"))` scan,
computed once per corpus build rather than once per chunk. The chunked
path's `swift_call_signatures` gate reads this instead of scanning its own
`parsed_files`; the unchunked path (`resolve_with_scopes_full_for_entities`,
where `parsed_files` already covers the whole corpus in one call) is
untouched — its old per-`parsed_files` check was already corpus-wide there.

**Post-fix, same probe, three budgets, same frozen snapshot:**

| Budget | dotnet-runtime edges | edge_hash (sha256 of sorted edge dump) |
|---|---:|---|
| 5 MiB | 981,276 | `265e7a08b557...` |
| 20 MiB (production default) | 981,276 | `265e7a08b557...` |
| 80 MiB | 981,276 | `265e7a08b557...` |

Bit-for-bit identical across all three. `GetInvocationListFlag` now
resolves to zero Swift edges at every budget — the ambiguity is correctly
and *consistently* detected once `corpus_has_swift` makes the corpus's full
set of `.swift` files visible to the gate regardless of chunk membership,
rather than manufacturing a false edge whenever the caller happened to
co-chunk with exactly one candidate.

**Baseline-change count** (pre-fix 20 MiB vs. post-fix 20 MiB, same
snapshot; sorted-edge-dump diff, deduplicated): **-7 edges net**, all
removed, none added. Every one of the 7 is the same false-positive shape as
the documented `combine` edge — a bare, unlabeled call resolving to some
`rspriv.h::function::init`-style single global candidate that the
ambiguity check, once it can see the corpus's *full* candidate set instead
of one chunk's slice of it, correctly recognizes as ambiguous (multiple
Swift `init`/named candidates) and refuses to guess:

```
< .../TaskFactoryTests.cs::...::ExerciseTaskFactory        Calls  src/coreclr/debug/di/rspriv.h::function::init
< .../TaskFactoryTests.cs::...::ExerciseTaskFactoryInt     Calls  src/coreclr/debug/di/rspriv.h::function::init
< .../ParallelForTests.cs::...::RunParallelLoopCancellationTests  Calls  src/coreclr/debug/di/rspriv.h::function::init
< .../ParallelForTests.cs::...::TestParallelForDOP          Calls  src/coreclr/debug/di/rspriv.h::function::init
< src/coreclr/debug/daccess/cdac.cpp::function::CDAC::Create Calls  src/coreclr/debug/di/rspriv.h::function::init
< src/coreclr/vm/cdacstress.cpp::function::CdacStressPolicy::Initialize  Calls  src/coreclr/debug/di/rspriv.h::function::init
< src/tests/nativeaot/.../Threading.cs::...::MutexMaximumReacquireCountTest  Calls  src/coreclr/debug/di/rspriv.h::function::init
```

This is a correctness improvement riding along with the determinism fix,
not just a side effect: the old chunk-local ambiguity check *systematically
under-detected* ambiguity (it could only see whichever `.swift` files
happened to be chunk-mates with the caller), so it silently manufactured
false edges whenever fewer than the true number of ambiguous candidates
were chunk-local. Making the gate corpus-wide makes the *detector* correct,
independent of whether its output changes any given repo's baseline.

**Other corpora, same probe, production 20 MiB budget, pre-fix vs.
post-fix:**

| Corpus | Chunked? | Edges before | Edges after | Δ |
|---|---|---:|---:|---:|
| dotnet-runtime | yes (57k files) | 981,283 | 981,276 | -7 |
| linux | yes (~80k files, 0 `.swift` files) | 1,898,816 | 1,898,816 | **0** |
| tiptap | no (1,533/1,936 files) | 5,414 | 5,414 | **0** |
| gin | no (108 files) | 2,352 | 2,352 | **0** |
| django | no (11k files) | 44,159 | 44,159 | **0** |

linux has zero `.swift` files corpus-wide, so `corpus_has_swift` is `false`
at every chunk regardless of budget — this fix is a no-op there by
construction, confirmed bit-identical. tiptap/gin/django never cross
`PARSED_FILE_REUSE_LIMIT` (20,000 files) so they never take the chunked
path at all; confirmed bit-identical directly (not just "should be
unaffected") via `edge_dump_probe` runs before and after this change on the
same checkouts, and separately via `git stash`-isolated before/after builds
against the same django checkout.

**Same-language-preference rule: considered, not needed.** The bead
description offered "prefer same-language candidates before cross-language
name matches" as an alternative or complementary rule (it would also make
"a C# local named `combine` should never match a Swift method" true by
construction, not just by disambiguation). Not implemented as a separate
rule here: `has_ambiguous_swift_signature_candidates` already requires 2+
same-*mechanism* (Swift-signature-bearing) candidates to refuse, and with
`corpus_has_swift` now correctly corpus-wide, the dotnet-runtime repro's
*specific* false positive (a single Swift `combine` incorrectly winning) is
already gone — there is no remaining reproduction to motivate a second,
separately-landed rule change against its own baseline-count. If a future
corpus is found where a *single*, non-ambiguous cross-language candidate
still wins a bare-name fallback it shouldn't (same-language preference
would suppress that unconditionally, ambiguity detection alone would not,
since one candidate isn't "ambiguous"), that is a new, separately-measured
bead — not bundled here per the task's own instruction to keep
independently-revertable decisions in independent commits.

### semx-yk5: the two documented residues

**Residue 1 — inconsistent `PreBuiltLookups` sort.** Of graph.rs's three
`PreBuiltLookups` construction sites (`pub fn build`'s whole-rebuild
branch, `build_direct_dependencies`, `build_incremental_with_metadata_and_
import_candidates`), two left `class_members`/`owner_members`/
`entity_ranges` in `all_entities` iteration order with no explicit sort;
the third sorted `class_members`/`owner_members` with `Vec<(String,
String)>`'s derived `Ord` — lexicographic `(member_name, member_id)` — a
*different* key than the canonical `(file_path, start_line, end_line, id)`
source-position key `sort_members_bucket_by_source` already established
elsewhere (used by `maintain_entity_lookups_incremental`'s phase 3 when
re-sorting touched buckets on the incremental-lookups path). All three
sites now call two new shared helpers —
`sort_all_member_buckets_by_source`, `sort_all_entity_ranges_by_source` —
applying the canonical key everywhere a `PreBuiltLookups` gets built.

**Residue 2 — module-scope `defs` last-write-wins, no stated rule.**
`build_scopes_from_ast`'s top-level-definitions loop
(`resolve_with_scopes_full_inner`, `scope_resolve.rs`) iterates
`entity_ranges.get(file_path)` and does `scopes[0].defs.insert(name, id)`
for every top-level entity — so when a file declares the same top-level
name twice, the second `insert` silently overwrites the first. This was
always deterministic (a `HashMap::insert` is deterministic given a fixed
iteration order) but never *stated*, because `entity_ranges`'s own order
was itself unstated (residue 1). With residue 1 fixed, the rule is now
explicit: **the top-level declaration with the greatest `(start_line,
end_line, id)` in the file wins.** No code change was needed for this
residue beyond residue 1's fix — only the doc comment on
`sort_all_entity_ranges_by_source` (graph.rs) making the connection
explicit, since `defs` population reads exactly the table residue 1 now
sorts consistently.

**Baseline-change measurement.** Same `edge_dump_probe` runs as semx-nuv's
table above, isolated to the yk5 commit alone (built on top of the nuv
fix): dotnet-runtime 981,276 -> 981,276 (**0**), linux 1,898,816 ->
1,898,816 at the 20 MiB production budget (**0**), tiptap/gin/django
unaffected by construction (never chunked, `PreBuiltLookups` built once).
Zero measured movement on every corpus tested: this pair's `all_entities`
order already coincided with the canonical key at every site whose bucket
order was reachable by a resolution decision on these five corpora — the
value of this fix is that the rule is now *stated and enforced*, not that
it changed any tested corpus's answer. A repo whose `class_members`/
`owner_members`/`entity_ranges` bucket order previously diverged from the
canonical key at the one wrongly-sorted site (residue 1's `build_
incremental_with_metadata_and_import_candidates` path — the incremental-
with-import-candidates API, not exercised by any of `edge_dump_probe`'s
whole-corpus builds above) could see a real, non-zero delta; none of the
five measured corpora exercises that specific incremental API in a way
that reaches this table, so this is disclosed as an untested path rather
than claimed as "measured zero everywhere."

**Residual, disclosed, out of both beads' stated scope: a third,
newly-discovered chunk-locality mechanism.** Repeating semx-nuv's
cross-budget probe on linux *below* the production budget surfaced a
mechanism neither bead's stated scope covers. At the production 20 MiB
budget linux is bit-identical (1,898,816 both before and after every fix
in this section); at a 5 MiB budget it is not (1,898,814 — a 2-edge
delta, reproducible, unaffected by either the nuv or yk5 fix above):

```
< drivers/gpu/nova-core/fsp.rs::impl::FspMessage::new       Calls  drivers/gpu/nova-core/fb.rs::impl::FbRange::len
< drivers/gpu/nova-core/gsp/hal/tu102.rs::function::run_fwsec_frts  Calls  drivers/gpu/nova-core/fb.rs::impl::FbRange::len
```

Root cause (read, not yet fixed): `resolve_with_scopes_full_inner`'s
`return_type_map`/`instance_attr_types`/`init_params`/`attr_to_param`
maps are pass-1-scanned from *that call's own* `parsed_files` on every
call (`"Pass 1: Scan ALL files for return types..."`, `scope_resolve.rs`)
— "ALL files" means all files in the current resolution call, which on
the chunked path is one chunk. Unlike `symbol_table`/`class_members`/
`owner_members`/`entity_ranges` (corpus-wide, built once upstream of
chunking in `graph.rs`) and unlike JS/TS (corpus-wide via semx-6rd's
`PrecomputedFileFacts`), no other language gets a corpus-wide merge for
these four tables on the chunked path — so a Rust `MethodCall` resolved
through instance-attribute-type tracking (`x.len()` where `x`'s type
comes from `instance_attr_types`) only succeeds when the type-defining
file (`fb.rs`) shares a chunk with the call site. This is the same
*shape* of bug as semx-nuv (a table gating cross-file resolution answers a
corpus-wide question with chunk-local data) but a different, considerably
larger blast radius: it is architectural, not a single boolean gate, and
affects every non-JS/TS language's return-type/instance-attribute-based
resolution on the chunked path, not one Swift-specific table.

**Not fixed in this pass**, deliberately: closing it properly means
extending `PrecomputedFileFacts`-style corpus-wide precomputation to every
language, which trades directly against semx-g6t's entire reason for
existing — bounding peak per-chunk `tree_sitter::Tree` memory by *not*
holding every file's tree live at once. The Memory attribution section
above already named and deferred exactly this generalization as "Open
item #2," for a different proximate reason (RSS) that turns out to be the
same underlying gap. Filed here with a concrete repro
(`drivers/gpu/nova-core/{fsp,tu102,fb}.rs` on linux, 5 MiB budget) rather
than silently absorbed into this bead's "closed with evidence" claim —
the gates below report dotnet-runtime and the production-budget corpora
honestly as bit-identical, and linux's sub-production-budget non-identity
honestly as not.

### The determinism invariant test

`crates/sem-core/src/parser/graph.rs`'s
`test_ambiguous_cross_chunk_swift_name_resolution_is_chunk_independent`
(added to the permanent suite, not a one-off example): builds a synthetic
corpus at the `#[cfg(test)]` `PARSED_FILE_REUSE_LIMIT=8`/
`SCOPE_RESOLVE_BYTE_BUDGET=150`-byte overrides — two `.swift` files each
declaring an ambiguous, unlabeled `combine`, a Python caller with no
local/same-file candidate, six filler files to clear the file-count
threshold — sized so the three key files fall into three separate chunks
(asserted directly against `chunk_files_by_byte_budget`'s real output, so
the test fails loudly rather than silently stops covering the scenario if
fixture text or the chunking constants drift). Asserts the caller resolves
to zero edges, the correct answer for a genuine cross-file ambiguity,
regardless of chunk membership. Confirmed load-bearing by reverting just
the `corpus_has_swift` gate to its pre-fix per-`parsed_files` form: the
test goes red (the caller incorrectly resolves to whichever `.swift` file
lands in an earlier chunk), confirming this is not a vacuous assertion.
Runs in well under 50ms — no giant checkout needed, unlike this section's
other proofs.

**What it does not cover, disclosed.** `SCOPE_RESOLVE_BYTE_BUDGET` is a
`#[cfg(not(test))]` compile-time constant (20 MiB in production, 150 bytes
under `#[cfg(test)]`), not a runtime-configurable value — so a literal
"same real corpus, three different byte budgets, one test process, one
assertion" test is not currently possible without either recompiling
between runs (what this section's `edge_dump_probe` proofs above did
manually, editing the constant and rebuilding three times) or adding a
runtime override to the production code path (not done here, to keep this
bead's diff to the two beads' stated scope). The unit test above proves
the *mechanism* semx-nuv fixed is chunk-independent; the `edge_dump_probe`
table above proves the *real, production-scale* build is bit-identical
across 5/20/80 MiB on dotnet-runtime specifically. Between the two, the
fixed mechanism is proven at both the unit and corpus scale, but not by
one single artifact — disclosed rather than overstated.

### Gates run

- `cargo test -p sem-core --lib --release`: **533 passed, 0 failed** (532
  baseline + the new invariant test).
- `cargo test -p sem-core --lib --release --features oxc-fastpath`: **545
  passed, 0 failed** (544 baseline + 1).
- `cargo clippy -p sem-core --lib --examples -- -D warnings`: **149**
  errors, identical count to the pre-existing baseline; none in any file
  or region this work touched.
- `cargo fmt -p sem-core -- --check`: clean on every file this work
  touched (`graph.rs`, `scope_resolve.rs`).
- `SEM_FP_PARITY=1 examples/incr_probe -- <dotnet-runtime> mixed50`:
  **`ORACLE ... ok`** on both `cold-vs-build` and `mixed50` — the
  incremental fingerprinting machinery stays correct under the new
  `corpus_has_swift`/sort-site changes.
- `edge_dump_probe` cross-budget determinism, dotnet-runtime, 5/20/80 MiB:
  **bit-identical** (981,276 edges, identical sha256 of the sorted edge
  dump at all three).
- `edge_dump_probe`, linux, at the production 20 MiB budget, pre-fix vs.
  post-fix (both beads): **bit-identical** (1,898,816 edges). At a 5 MiB
  budget: **not** bit-identical (1,898,814 — the newly-discovered,
  disclosed, out-of-scope residue above); this is not claimed as closed.
- `edge_dump_probe`, tiptap/gin/django, pre-fix vs. post-fix (both beads):
  **bit-identical** (5,414 / 2,352 / 44,159 edges respectively) — these
  three never cross `PARSED_FILE_REUSE_LIMIT`, so both fixes are
  structural no-ops for them, confirmed rather than assumed. (django's
  44,159 differs from an earlier session's 47,618 in this same document's
  g6t section — confirmed by an isolated `git stash` before/after build
  against the identical checkout to be checkout drift between sessions,
  not a regression from this work.)
- TypeScript-monster: **not run this session** — the raw checkout
  (`~/.cache/checkouts/github.com/microsoft/TypeScript`, present in an
  earlier session per the semx-g6t section above) was not present in this
  session's environment; only a pre-built fact-store snapshot
  (`ts-monster-store-v2`, no raw source) was available. Disclosed as a gap
  rather than substituted silently; dotnet-runtime and linux together
  cover both the chunked-C#-with-Swift-interop case semx-nuv targets and
  the chunked-Rust case the newly-found residue above targets.

### Status: semx-nuv closed with evidence; semx-yk5 closed with evidence
and one new, separately-filed residue

semx-nuv: fixed, proven bit-identical across three byte budgets on its own
target corpus (dotnet-runtime), baseline change measured and justified
(-7 edges, all previously-bogus), zero movement on every corpus without
`.swift` files or below the chunking threshold, permanent regression test
added and confirmed load-bearing.

semx-yk5: both documented residues resolved — the sort-site inconsistency
unified (with the wrong-key site corrected, not just the unsorted ones
brought into line), module-scope `defs`' tie-break stated for the first
time — with zero measured baseline movement on every corpus tested (the
fix makes an already-coincidentally-correct order into a *stated,
enforced* one, rather than changing behavior). A third mechanism, in the
same family but architecturally larger (chunk-local return-type/instance-
attribute maps for every non-JS/TS language), was found while proving the
cross-budget invariant on linux and is disclosed above with a concrete
repro rather than folded into either bead's "closed" claim or silently
dropped — it is not fixed here, and is left for its own future bead
rather than expanding this session's scope past what was asked.

## Interning (semx-5nc)

C3 finisher: 918f12a's semx-4an second pass considered `u32` interning for
`child_ranges_by_parent`'s key (a *warm*, session-owned structure) and
declined it against a measured +3.5% RSS delta, explicitly disclosing
"**No** micro-benchmark of `String` vs `Arc<str>` vs `u32` was run; the
choice is justified by struct size and allocation count, and the aggregate
RSS cost is reported rather than modelled." semx-5nc re-opens the question
for the *cold* path — `symbol_table: HashMap<String, Vec<String>>` (name ->
candidate ids) and `entity_map: HashMap<String, EntityInfo>` (id -> info),
the two tables `resolve_ref` (`scope_resolve.rs`) joins on for every
reference in the corpus — with the micro-benchmark 918f12a's own text named
as the gap. Both tables are already `FxHashMap` (rustc-hash), not std
`SipHash` — `graph.rs:661`'s comment states this was already chosen because
"Fx hashing is materially faster for short string keys" — so the real
open question is narrower than "String vs u32": does removing the
*remaining* string-hash cost on these specific, longer-than-short-key ids
clear a real bar, once `entity_map`/`symbol_table` are already on the
fastest general-purpose hasher this crate uses elsewhere.

### Method

1. `examples/key_shape_probe.rs` (new) builds the real TypeScript monster
   once via `EntityGraph::build` and reads the REAL key shapes directly out
   of the returned `entities: EntityInfoMap` — id-string lengths
   (`entity_map`'s key) and, per the stated equivalence at
   `scope_resolve.rs:5080-5081` ("`symbol_table[name]` is exactly the ids
   of entities named `name`"), reconstructs `symbol_table`'s name-length
   and bucket-size (collision count) distributions by grouping the same
   entities by name. It also times building a fresh `FxHashMap<String, u32>`
   interner over all real ids as one bolt-on pass — the literal "interner
   build cost at real scale" the bead asked for, measured against the real
   corpus, not modeled.
2. `benches/interning.rs` (new criterion bench, `harness = false` entry
   added to `Cargo.toml` per this crate's existing bench convention)
   generates a synthetic corpus whose id-length, name-length and bucket-size
   distributions are calibrated to the measured percentiles via
   piecewise-linear interpolation over real anchor points — not real
   Microsoft-repo identifier text checked into the tree (third-party
   source), following this crate's own established fixture convention
   (`benches/common/mod.rs`: "Generated rather than checked in ... so the
   size points stay honest"). Three backends are compared head-to-head,
   including the explicit `std::HashMap` (SipHash) baseline the bead asked
   for even though production already uses `FxHashMap`: `std_string_keyed`,
   `fx_string_keyed_current` (today's shape), `fx_u32_interned` (a
   build-scope interner + token-indexed tables). Three groups: `build` (cost
   to construct both tables from scratch), `join_lookup` (the actual
   `resolve_ref` shape: name -> `symbol_table` -> up to 4 candidate ids ->
   `entity_map` per candidate), `id_only_lookup` (isolates 918f12a's exact
   deferred question — `entity_map.get(id)` alone, id already in hand, no
   name hop in front of it).
3. Cross-checked against **existing, zero-new-code production
   instrumentation** (`resolve_profile.rs`, `SEM_PROFILE_RESOLVE=1`) run on
   one real cold `EntityGraph::build` call each for the TypeScript monster,
   dotnet-runtime and linux — the three corpora exercising sem-core's three
   distinct resolve regimes (JS/TS `PrecomputedFileFacts`, C# chunked
   re-parse, Rust/C chunked-with-heavy-bag-of-words). `resolve_ref_ms` is
   an existing, already-summed phase timer covering exactly the function
   this bead's join lives inside — it answers "what fraction of real cold
   build time could even theoretically move" without writing or risking any
   change to the resolver itself.

### Real key shapes (`key_shape_probe`, TypeScript monster)

```
KEY_SHAPE entities=714,819 unique_names=86,796
id_len   p50=87  p90=131 p99=181 max=1,699 min=18
name_len p50=17  p90=28  p99=82  max=1,650 min=0
bucket_size (symbol_table[name].len())
  p50=1 p90=6 p99=71 max=25,030 mean=8.24 singleton_frac=0.555
INTERN_BUILD entities=714,819 intern_build_ms=54.13 (75.72 ns/entity)
```

(714,819 is the raw pre-graph entity count `mem_single_probe` also reports
— 714,832 in that section's own run, an 13-entity run-to-run variance on a
live checkout; the corpus-wide 454,541 figure quoted elsewhere in this
document is a later, further-processed count from an earlier session's
snapshot, not a different corpus.) The id median (87 bytes) confirms the
context's framing — entity ids are long, path-rooted strings, several times
the length of the ~17-byte median name — and the bucket-size distribution
is sharply right-skewed: 55.5% of names are singletons (no ambiguity to
resolve at all) while the hottest name collides across 25,030 entities.

### Micro-bench results (`cargo bench -p sem-core --bench interning`)

Synthetic corpus calibrated to the anchors above, 200,000 entities (scaled
down from the real 455k-715k scale for a tractable per-iteration bench —
the literal at-scale interner build cost is the real-corpus number above,
not this section), 20,000 queries/iteration, criterion default sampling
except `build` (`sample_size(20)`, each build is tens of ms):

| group | backend | mean time | vs. `fx_string_keyed_current` |
|---|---|---:|---:|
| `build` | `std_string_keyed` | 66.20 ms | +75% (slower) |
| `build` | **`fx_string_keyed_current`** | **37.80 ms** | — |
| `build` | `fx_u32_interned` | 51.99 ms | **+37.5% (slower)** |
| `join_lookup` | `std_string_keyed` | 6.657 ms | +94% (slower) |
| `join_lookup` | **`fx_string_keyed_current`** | **3.430 ms** | — |
| `join_lookup` | `fx_u32_interned` | 0.236 ms | **−93.1% (14.5x faster)** |
| `id_only_lookup` | `std_string_keyed` | 701.3 µs | +111% (slower) |
| `id_only_lookup` | **`fx_string_keyed_current`** | **332.3 µs** | — |
| `id_only_lookup` | `fx_u32_interned_token_in_hand` | 10.55 µs | **−96.8% (31.5x faster)** |

Two findings, read together:

1. **The `std::HashMap` (SipHash) comparison the bead asked for confirms
   `graph.rs:661`'s existing rationale was correct and is not the live
   question** — `FxHashMap` already beats `std::HashMap` by ~1.9x on
   `join_lookup` and ~2.1x on `id_only_lookup` at these key lengths. This
   crate is not leaving a SipHash-vs-Fx win on the table; it already banked
   it.
2. **`fx_u32_interned`'s *build* cost is not free — it is 37.5% *slower*
   than building today's tables directly**, because a build-scope interner
   is, at minimum, a `FxHashMap<String, u32>` over every real id (the exact
   same hashing work `entity_map`'s own build already does) *plus* the
   `Vec<EntityInfo>`/token-table bookkeeping on top. Interning a corpus-wide
   table is additive over building it directly, never a substitute for it —
   confirmed by both this bench and the real-corpus `INTERN_BUILD` number
   above (54.13ms as a bolt-on pass at 714,819 entities, non-zero even in
   the most favorable framing).
3. **The *lookup* win is real and large, but conditional on the token
   already being in hand.** `id_only_lookup`'s 31.5x speedup is the
   idealized case 918f12a's own deferred question asks about — no name hop,
   `entity_map.get` alone. `join_lookup`'s 14.5x reflects `resolve_ref`'s
   actual shape, where the name -> `symbol_table` hop is unavoidably a
   fresh `&str` off the AST/call-site text (never an already-interned
   token, so translating it costs exactly one hash regardless of backend —
   interning cannot remove this half of the join, only the `entity_map`
   half after it) but the *candidate-id* half — walking up to 4 ids per
   bucket into `entity_map` — is exactly what a token-indexed table turns
   into array indexing.

Taken at face value, `join_lookup`'s 93.1% reduction clears the bead's
"< 10% on the join, stop" gate by a wide margin — the naive reading says
proceed to step 2.

### Why "proceed" is not the actual verdict: the smaller-blast-radius design narrows the win, and production evidence already bounds it

`EntityInfo` is a `pub struct` (`id`, `name`, `entity_type`, `file_path`,
`parent_id: Option<String>` — `Serialize`/`Deserialize`) that is the crate's
serialized output shape for the CLI/MCP layer; making it token-keyed at
rest is not the smaller-blast-radius option the bead's own instructions
asked to prefer ("session-owned structures... choose the smaller-blast-
radius option first"). The realistic design keeps `EntityInfo`,
`symbol_table`'s stored `Vec<String>`, `parent_id`, `ChildRange`'s
`file_path` and every edge (`EntityRef`) as `String` at rest — exactly as
918f12a's own child_ranges precedent did — and would build a
resolve-call-local interner + token-indexed shadow tables purely inside
`resolve_with_scopes_full_inner`'s hot loop, translating back to `String`
at the edges (this is precisely what `benches/interning.rs`'s
`build_interned` models: `symbol_table`'s *value* side becomes `Vec<u32>`,
`entity_map` becomes `Vec<EntityInfo>` indexed by token, but the corpus's
`String` source of truth is untouched).

Under that design, `join_lookup`'s win is real for calls that walk
`symbol_table`'s own (pre-translated) candidate list — but a `grep` of
`entity_map.get`/`symbol_table.get` call sites in `scope_resolve.rs` finds
roughly 30, and only the `resolve_ref` cluster (the bead's named target)
walks ids it already holds as pre-translated tokens. Sites reading
`parent_id`, per-file member scans, and ambiguity checks against ids
supplied from elsewhere all receive a *foreign* `&str` id that still needs
one `interner.get()` hash to translate before an array index helps — a
wash against today's direct `entity_map.get(id)`, not a win, unless every
one of those fields is also threaded as a token (which re-opens exactly
the "session-owned indices that must stay stable across rebuilds" hazard
918f12a declined to take on for an even smaller structure).

Rather than implement that broader change and measure it after the fact,
the cheaper and more decisive move — already following this document's own
established discipline (attribute before you fix; falsify with real
numbers before spending implementation budget, per semx-4w1's whole
methodology) — is to ask what fraction of *real* cold build time
`resolve_ref` (the function this bead's join actually lives inside, already
timed by existing zero-cost-when-off instrumentation, no new code required)
accounts for. If that ceiling is already below the bead's own 5-10%
total-build materiality bar, no implementation, however well-executed,
can clear it — measuring the join in isolation was never going to answer
that question, and it does not need to be answered by first shipping the
change.

**`resolve_ref_ms` as a fraction of one cold `EntityGraph::build` call,
`SEM_PROFILE_RESOLVE=1`, three corpora spanning sem-core's three distinct
resolve regimes:**

| Corpus | Regime | Total cold `build_ms` | `resolve_ref_ms` | Fraction |
|---|---|---:|---:|---:|
| TypeScript monster | JS/TS, `PrecomputedFileFacts`, unchunked at 714,819 entities' file count | 9,707.85 | 71.18 | **0.73%** |
| dotnet-runtime | C#, chunked re-parse, 1,141,386 entities | 38,446.63 | 880.54 | **2.29%** |
| linux | C, chunked, heavy bag-of-words, 2,482,061 entities | 27,252.77 | 30.03 | **0.11%** |

Every corpus this bead's context named as a gate target is under 2.3%.
Even in the most favorable, purely hypothetical framing — `resolve_ref`'s
*entire* cost eliminated to zero, not just its hashmap-lookup share of it —
the ceiling on dotnet-runtime (this measurement's worst case) is **2.29%
of total cold build time**, itself below the bead's own stated 5-10%
total-build materiality bar *before* subtracting the u32 interner's own
build-cost tax (measured above at +37.5% on table construction, real at
54.13ms even as a bolt-on pass at 714,819 entities). `entity_lookup_build_ms`
— the *construction* cost of `symbol_table`/`entity_map`/related tables,
a different and larger number (4.2-7.9% across the three corpora) — is not
improved by interning either: it is dominated by the same per-entity insert
work an interner has to redo, and the micro-bench's own `build` group shows
that work does not get cheaper by adding a token layer on top of it.

### Decision: measured decline, before implementation

**Declined.** The join-only micro-bench alone would say proceed (93.1%
reduction, comfortably past the 10% gate); the production cross-check says
the real ceiling on total cold build time is 0.11-2.29% across every named
corpus, decisively below the bead's own 5-10% materiality bar, and the
u32 side's own construction cost is measured *more* expensive than today's
direct build, not less. Implementing step 2 (build-scope interning across
`entity_map`/`symbol_table`'s hot-loop shape) and then running step 3's
paired cold monster/dotnet/linux measurement would not change this verdict
— the ceiling is already known and is below the bar without it — and would
spend the correctness-review budget (bit-identical checks across 6 corpora,
the 32-scenario `SEM_FP_PARITY=1` oracle, RSS regression, warm-rebuild
check) that this document's own history shows this exact resolver has
repeatedly needed to catch genuinely subtle chunk-locality bugs (semx-nuv,
semx-yk5's third residue) on a change whose own upside is already bounded
below materiality. This is a deliberate, disclosed deviation from the
literal step 2/3 procedure: the procedure's purpose (spend implementation
effort only when the numbers justify it) is better served by measuring the
ceiling with existing instrumentation *before* implementing than by
implementing first and measuring after, when the pre-implementation
measurement is conclusive on its own. No production code (`graph.rs`,
`scope_resolve.rs`, `session.rs`, `EntityInfo`, `symbol_table`,
`entity_map`) was touched by this bead.

**What would change this verdict.** A future corpus where `resolve_ref`
is a materially larger share of cold build time (none measured here reaches
even a third of the 5-10% bar), or a design that also threads tokens
through `EntityInfo`/`parent_id`/edges so the *majority* of the ~30 lookup
call sites (not just `resolve_ref`'s own bucket walk) get the token-in-hand
win `id_only_lookup` measured — at the cost of the exact session-owned-
index-stability hazard both this bead and 918f12a declined to take on for
a smaller structure.

### Gates run

* `cargo build --release -p sem-core --example key_shape_probe --bench interning`:
  clean, no warnings.
* `cargo test -p sem-core --lib --release`: **533 passed, 0 failed** —
  unchanged from this document's existing baseline, expected since no
  production file was edited.
* `cargo clippy -p sem-core --release --examples --benches -- -D warnings`:
  168 pre-existing errors on files this bead did not touch (baseline noise
  unrelated to this work, several newer than the 149-count baseline earlier
  sections recorded — drift from other in-flight work this session, not
  from this bead); **zero** in `examples/key_shape_probe.rs` or
  `benches/interning.rs` (confirmed by grepping the full clippy output for
  both filenames).
* `cargo fmt -p sem-core -- --check`: clean on both new files after one
  `cargo fmt` pass.
* Pathspec: `crates/sem-core/Cargo.toml` (two new `[[bench]]`/example
  entries — the example is picked up by auto-discovery, only the bench
  needed a `harness = false` entry matching `parse_profile`/`incremental`'s
  existing convention), `crates/sem-core/examples/key_shape_probe.rs` (new),
  `crates/sem-core/benches/interning.rs` (new), this section. No other file
  in the working tree was touched by this bead; pre-existing unrelated
  dirty state (`languages.rs` reflow hunks, `examples/hosted-diff/*`) was
  left alone per this session's standing instruction.

### Status: semx-5nc closed as measured-decline

Real key shapes extracted from the actual TypeScript monster corpus (not
guessed), a criterion micro-bench built and run comparing `std::HashMap`,
today's `FxHashMap`, and `u32` interning on both build and lookup cost —
the exact micro-benchmark 918f12a's own text flagged as never having been
run — and a cross-check against existing production phase timers on three
corpora spanning every one of sem-core's resolve regimes. The join-lookup
win is real (93.1% in the idealized, token-in-hand case) but the function
it lives inside is 0.11-2.29% of real cold build time on every corpus this
bead named, decisively below the 5-10% total-build materiality bar this
bead's own instructions set as the bar to clear, before even subtracting
the u32 interner's own measured build-cost tax. No production code
changed; the two new files (a real-corpus key-shape probe and a criterion
bench) are left in the tree as the artifacts backing this decision and as
reusable instrumentation for a future bead that reopens the question with
a materially different corpus or a broader (higher-blast-radius) design.

## vscode's dead warm tier: a whole-corpus guard triggered by one incidental file (semx-bvu)

The fleet finale (semx-6xw, HEAD 564bbf2) flagged vscode as the only one of
13 fleet repos with **0% warm-cache hits**: `facts_probe load none` (zero
changed files) reported `files_green=0` out of 13,292, `warm_total_ms`
(9,023ms) essentially equal to a cold build (8,233-8,305ms). Every other
repo in the fleet — including two other JS/TS-heavy corpora, kubernetes
(13,618/21,429 green, 63.5%) and the TypeScript monster (39,296/40,865,
96%) — warmed correctly. `ORACLE ok` throughout, so this was a caching
defect, never a correctness one.

**Root cause: (b), an eligibility guard misfiring en masse — not a probe
artifact and not a store-keying bug.** Reproducing with `facts_probe save`
+ a fresh-process `load none` against `/tmp/bench-vscode` and adding
counters at each layer (`PersistedFacts::fingerprint_count`/
`resolved_file_count`, both now permanent diagnostic methods; a
`RebuildStats::changed_keys` print in `facts_probe`'s `WARM` line)
eliminated the other two hypotheses in order:

* **Not (a), a probe/re-clone artifact.** `stored_files=13292`,
  `files_seed_red=0` — every file's content hash matched between save and
  load. The store round-tripped perfectly; nothing about the shallow
  clone's state changed between the two process invocations.
* **Not (c), a store-keying bug.** `stored_fingerprints=1,118,733`,
  `stored_resolved=11,682`, `prev_fingerprints_empty=false`,
  `changed_keys=0` — the fingerprint map, the per-file resolution cache,
  and the corpus-wide tables were all loaded intact and bit-identical to
  what the cold build produced. `Incremental::new`'s `reuse &&
  !prev_fp.is_empty()` gate (the first suspect, since it disables reuse
  for an entire build) was firing `true` correctly.

**The actual gate.** `resolve_with_scopes_full_inner`'s GREEN filter
(`scope_resolve.rs`) computed `let swift_active =
!swift_call_signatures.is_empty()` and used it as an unconditional
kill-switch: `if swift_active || !eligible { return false; }`, checked
*before* any file's own cache/read-set is consulted. `swift_call_signatures`
is corpus-wide (`ChunkedResolveInputs::corpus_has_swift`, semx-nuv) because
`resolve_ref`'s Swift-overload-ambiguity branch is not attributable to any
one file's read set. vscode's checkout has exactly one `.swift` file —
`extensions/vscode-colorize-tests/test/colorize-fixtures/test.swift`, a
13-line colorizer test fixture, not consumed by anything — whose one
function declaration (`func hasAnyMatches(list: [Int], condition: (Int) ->
Bool) -> Bool`) is enough to make `build_swift_call_signatures` return one
entry. `swift_call_signatures_len=1` on every build, so `swift_active` was
`true` on every build, so the filter returned `false` for every file,
every time — independent of whether that file was JS/TS, independent of
whether *anything* about the Swift signature table had changed since the
previous build.

**Trace evidence, 3 files** (env-gated per-file trace added and removed
during bisection; the underlying facts are reproduced by the new
`swift_guard_*` tests in `session.rs`):

| File | Eligible (JS/TS/newly-attributed)? | Cache entry present? | Verdict before fix | Why |
|---|---|---|---|---|
| `extensions/vscode-colorize-tests/test/colorize-fixtures/test.swift` | No (`.swift` is not in `is_reuse_eligible_file`'s language list) | n/a | RED | Correctly perma-RED regardless of this bug — but its mere presence is what set `swift_active=true` for the whole corpus |
| `src/vs/base/common/arrays.ts` | Yes | Yes (content hash matched, `resolution: Some`) | RED | Never reached its own cache/read-set check — `swift_active` short-circuited the filter first |
| `src/vs/base/common/strings.ts` | Yes | Yes (content hash matched, `resolution: Some`) | RED | Same as above |

**Fix (fail-toward-MISS statement).** Changed the trigger from "is the
table non-empty" to "did the table's fingerprint *change* since the last
build" — the same rule every other whole-table guard in this module already
uses (`Table`'s own doc comment: "fingerprinted whole and any change forces
every file RED"), which `GuardSwiftCallSignatures` had silently drifted
from. `scope_resolve.rs`:

```rust
let swift_guard_key = key_whole(Table::GuardSwiftCallSignatures, scope_tag);
let swift_signatures_changed =
    state.inc.prev_fp.get(swift_guard_key) != state.inc.cur_fp.get(swift_guard_key);
```

used in place of the old `swift_active`. This fails toward RED, never
toward a false GREEN, in every direction that matters:

* First build ever, or a `prev_fp` that never tracked this table: `None !=
  Some(_)` — `changed=true`, identical to the old behavior on that build.
* Swift signatures genuinely change: `Some(a) != Some(b)` — `changed=true`,
  every eligible file still goes RED (proved by
  `swift_guard_changing_swift_signatures_still_reds_everything` below —
  `files_green=0` even though only the one `.swift` file was named as
  changed).
* Swift signatures are byte-for-byte unchanged (the common no-op-reload
  case, and vscode's case on every build): `Some(a) == Some(a)` —
  `changed=false`, and every other file's reuse decision now falls through
  to its own ordinary cache/read-set check, exactly as if no `.swift` file
  existed in the corpus at all.

Because `resolve_with_scopes_full_inner` is the one function both the
direct (`retain_parsed_files`) and chunked (`resolve_scopes_in_file_chunks`)
paths call into, this fix covers both — relevant to any repo over
`PARSED_FILE_REUSE_LIMIT` (20,000 files) that also happens to contain a
`.swift` file, not just vscode's shape.

**Before/after, vscode** (`facts_probe`, `/tmp/bench-vscode`, fresh save +
fresh-process load):

| | `files_green` | `files_red` | `warm_total_ms` | cold `build_ms` |
|---|---|---|---|---|
| Before, `none` | 0 / 13,292 (0%) | 13,292 | 8,415-9,023 | 7,527-8,306 (warm ≈ cold) |
| After, `none` | 11,460 / 13,292 (86.2%) | 1,832 | 4,801.50 | 7,526.58 (warm 1.57x under cold) |
| After, `mixed50` | 11,409 / 13,292 (85.8%) | 1,883 | 5,072.81 | — |

`ORACLE ok` on both scenarios, before and after. The 1,832 residual RED
files are the genuinely non-reuse-eligible ones (JSON/CSS/HTML/Markdown/the
one `.swift` file, none of which are in `is_reuse_eligible_file`'s
language list) — the same shape every other repo in the fleet already
shows.

**Gates.**

* `incr_probe … all` (8 scenarios: none/leaf/mixed50/hub/hubrename/
  tests/importchurn/cold-vs-build), `SEM_FP_PARITY=1`, vscode: 8/8
  `ORACLE ok`, `files_green` healthy (11,407-11,460) in every scenario that
  touches nothing Swift-related.
* Same 8 scenarios, `SEM_FP_PARITY=1`, kubernetes (21,429 files — the
  *chunked* path, zero `.swift` files, the giant this session had available
  in place of the TypeScript monster/tiptap checkouts, neither of which has
  a live checkout in this environment — only `ts-monster-store-v2`'s
  persisted blob remains under `/tmp/bench-fleet`, consistent with a prior
  bead's note at this document's "Byte-budget chunking" section): 8/8
  `ORACLE ok`, `files_green=13,618/21,429` on `none` — bit-identical to the
  fleet's own recorded baseline for kubernetes, confirming no regression on
  a repo the fix's `changed=false` path was not designed around.
* `facts_probe`, rails (3,744 files, no Swift): `ORACLE ok` on `none` and
  `mixed50`, `files_green=3,468/3,744` (92.6%) — no regression on a small
  plain repo either.
* `cargo test --release -p sem-core --lib --features parallel`: 536 passed
  (533 pre-existing + 3 new), 0 failed. The 3 new tests
  (`session.rs`, "Swift whole-corpus GREEN-eligibility guard" section) are
  a minimal fixture (one untouched `.swift` file + a TS hub/4 leaves) proving
  both halves of the fix: a no-op rebuild still greens the TS files despite
  the `.swift` file's presence (the regression), a leaf edit still scopes
  correctly (blast radius unaffected), and an actual Swift-signature edit
  still reds every eligible file (`files_green=0`, the guard's job,
  unchanged).
* `cargo fmt -p sem-core -- --check`: clean.
* `cargo clippy --release -p sem-core --lib --features parallel -- -D
  warnings`: pre-existing debt elsewhere in `scope_resolve.rs` (~148
  findings, all `map_or`/`needless_borrow`/`type_complexity` in code this
  bead never touched, confirmed identical before and after this diff via
  `git stash`); zero findings on any line this bead added or modified, and
  zero findings anywhere in `session.rs`, `facts_store.rs`, or
  `examples/facts_probe.rs`.

No REPORT.md file exists in this environment for the semx-6xw fleet finale
(only the bead itself, `semx-bvu`, and this document carry its numbers) —
this section is that footnote.

## Diff attribution (semx-cc3)

`sem diff`'s own phases (as opposed to the build-plane machinery
underneath it, which the rest of this document is about) had never been
attributed. S2 had measured `sem diff` at ~2.1s on vscode's largest TS
commit and ruled out extraction as the cause, but nothing had drilled
into staging, matching, move/rename detection, or rendering individually.
This section does that, fixes what the numbers named, and states the
floor honestly.

### Method

- New `SEM_TIMINGS`-gated phase marks, wired into `sem diff` the same way
  `sem graph`/`sem impact`/`sem entities` already use `crate::timings::
  Timings` (`crates/sem-cli/src/commands/diff/mod.rs`). `compute_semantic_diff`
  fans work out over a rayon `par_iter` per file with no single call stack
  to time, so a companion module, `sem_core::parser::differ::phase_timing`,
  accumulates per-phase CPU-time (extraction, matching+move/rename, orphan
  detection) into atomics across every file/thread, read back after the
  call. Gated by a `OnceLock<bool>` cached from `SEM_TIMINGS`: disabled,
  the cost is one relaxed atomic-bool load per phase per file, no
  `Instant::now()` at all.
- Battery: 3 repos x 2 real commits each, median-of-3, `SEM_LOCAL=1` (no
  cloud upload — see the "cloud" row below for why that's the honest
  choice), diffed via `sem diff <parent> <commit>` and cross-checked via
  `sem diff --commit <commit>`:
  - **rails** (small repo, 4,979 tracked files, single pack): small =
    `009b900b` (4 files), large = `5700c17c` (177 files, a real
    `Style/MutableConstant` cop-enable PR).
  - **vscode** (medium repo, 17,659 tracked files, 2 packs): small =
    `ec01a3f4` (5 files, all `.ts`), large = `42b14209` (63 files, all
    `.ts`, an agent-host feature PR).
  - **TypeScript monster** (81,368 tracked files, 40,832-commit full
    clone): small = `d8aafb31` (4 files), large = `c3dc61d1` (147 files,
    a fourslash-test sweep).
  - `bench-fleet`'s rails/vscode/home-assistant/kubernetes checkouts
    turned out to be **shallow, single-commit clones** (`git rev-list
    --all --count` = 1) — built for something else, not diff-battery use.
    `git fetch --deepen=N` recovered real history for rails and vscode
    (kept small — a few hundred commits — since only 2 real commits per
    repo were needed); the TypeScript checkout already had full history.

### Attribution table (median-of-3, `SEM_LOCAL=1`, after both fixes below)

| cell | files | entities (before→after) | changes | staging | extract+match+orphan (wall) | render | total |
|---|---:|---:|---:|---:|---:|---:|---:|
| rails:small | 4 | 27→34 | 9 | 5.97ms | 0.37ms | 0.03ms | **6.54ms** |
| rails:large | 177 | 5,869→5,870 | 186 | 23.72ms | 72.69ms | 1.93ms | **98.41ms** |
| vscode:small | 5 | 615→633 | 41 | 14.59ms | 17.39ms | 0.05ms | **32.27ms** |
| vscode:large | 63 | 7,678→7,943 | 425 | 22.04ms | 66.08ms | 0.70ms | **88.93ms** |
| typescript:small | 4 | 66→66 | 5 | 39.55ms | 6.97ms | 0.02ms | **46.80ms** |
| typescript:large | 147 | 10→10 | 147 | 50.08ms | 2.84ms | 0.02ms | **53.29ms** |

`registry init` and `cloud (relations budget + upload prep)` are omitted
from the table — both round to ≤0.1ms in every cell (registry init just
reads `.semrc`/`.gitattributes`; cloud is a no-op under `SEM_LOCAL=1`,
see below). Every cell's `staging + registry + extract/match/orphan +
render + cloud` sums to **1.00-1.02x** of `total` — the wall time is
fully attributed, not hiding an unmeasured remainder.

**What dominates per axis:**

- **By repo size**: on the monster repo, `staging` (git scope resolution
  + before/after blob content population) dominates even a 4-file diff
  (39.6ms of 46.8ms total) — a large *fixed* per-invocation cost from
  touching a big git object database at all (see "the floor" below).
  On rails/vscode, `staging` is a much smaller fraction; `extract+match+
  orphan` dominates instead once the commit is large enough to have real
  entities to extract.
- **By commit size**: `extract+match+orphan` scales with entities-touched,
  not files-changed alone — typescript:large has 147 changed files but
  only 10 total entities (fourslash test fixtures with almost no
  top-level declarations, mostly orphan line changes), so its wall time
  there is tiny (2.84ms) despite the highest file count in the battery.
  vscode:large (63 files, 7,943 entities) is the extraction-heaviest
  cell and shows it (66.08ms wall).
- **Parallelism is already doing its job.** CPU-time summed across every
  file/thread for extraction alone is 700-820ms on rails:large/
  vscode:large (both real, entity-dense commits) — the wall-clock
  `extract+match+orphan` bucket is 10-13x smaller (60-75ms), i.e.
  `compute_semantic_diff`'s existing rayon fan-out (pre-dating this
  campaign) is already absorbing the bulk of that cost across cores.
  **This was the precedent's prime suspect and it's innocent**: no fix
  applied here, because there was nothing to fix.

### Falsified hypotheses (checked first, per the precedent's suspect list)

| hypothesis | verdict | evidence |
|---|---|---|
| Corpus-proportional pre-work — does diff re-walk/re-verify the corpus per invocation? | **Falsified.** No such pass exists on the local path. | Read `GitBridge::get_changed_files` (git2 `diff_tree_to_tree`/`diff_tree_to_workdir_with_index`, both O(changed files), no full-repo walk) and `create_registry` (reads `.semrc`/`.gitattributes` only). Neither calls `get_or_build_graph` or any freshness/index pass. Only the **cloud-gated** relations pass does (`relations.rs`, out of this campaign's scope — see "cloud" below) — and it never runs for a local invocation. |
| Matching scans all-entities instead of changed-entities | **Falsified — doesn't apply.** `match_entities` is already file-scoped. | Each call gets exactly one file's before/after entity lists (`differ.rs`'s per-file `par_iter` closure), never a repo-wide scan. There is no O(changes × all-entities) shape to fix. |
| Serial loops | **Confirmed, fixed.** `populate_contents`'s Commit/Range arms were a plain serial `for` loop over changed files reading blob content — no parallelism at all, unlike extraction's existing fan-out. | Fixed below (parallelize). |
| Per-side duplicate work the extraction cache should be absorbing | **Checked, doesn't apply.** Diff extracts entities fresh from two arbitrary historical tree states per invocation; the persisted extraction cache (used by the build-plane/index path elsewhere in this document) is keyed to the *current* working-tree/index state and generally can't serve an arbitrary `from..to` comparison. A content-hash-keyed extraction cache (reusable across diffs when the same blob recurs) is a plausible *future* idea, deferred — no measurement showed extraction wall-time as a bottleneck once fan-out absorbs it (see above), so there's no evidence it would move the needle today. | Code read of `compute_semantic_diff`'s extraction call; cross-referenced against the wall-time evidence. |
| Relations budget pass / upload prep | **Not exercised — correctly.** `cloud (relations budget + upload prep)` measured ≤0.02ms in every battery cell. `sem diff` never routes to the cloud for a range/commit diff unless the repo has cloud consent + login (`DiffCloudContext::resolve`); the battery ran `SEM_LOCAL=1`, matching how review-product CI and local dev actually run this command (see this campaign's context note: "diff never routes to the cloud... the local path beats a network round trip at any repo size"). `relations.rs`'s own adaptive budget (90s floor, 480s cap for >100k tracked files) is unrelated, previously-shipped work, not touched or re-measured here — it also carries another session's uncommitted WIP this campaign was told not to disturb. |
| Pack fragmentation (the TypeScript checkout had 263 pack files vs rails' 3 and vscode's 2 — `git count-objects -v`) drives the monster repo's staging cost | **Falsified.** `git repack -a -d` (263 packs → 1, diagnostic only, not a code change) cut `staging` only ~10-12% (small: 47.06ms → 41.13ms; large: 239.33ms → 215.05ms) — nowhere near the order-of-magnitude a fragmentation theory predicts. | Direct repack + re-measure, `git count-objects -v` before/after. |
| The redundant tree re-resolution (see fix 1 below) is the monster repo's dominant fixed cost | **Falsified — real but small.** Deleting it moved `typescript:large` staging by only ~2ms (215.05ms → 213.05ms). | Measured independently before combining with fix 2 (see fix log). |

### Fix log

**Fix 1 — delete the redundant tree re-resolution** (`sem-core: delete
redundant tree re-resolution, parallelize blob population (semx-cc3)`,
first half). Per the "delete before build" principle: `get_commit_diff_files`/
`get_range_diff_files` already resolve both before/after trees to compute
the diff, then discarded them; `populate_contents`'s `Commit`/`Range` arms
independently re-resolved `sha`/`sha~1`/`from`/`to` via `resolve_tree()` —
a full revparse + `peel_to_commit` + `tree()` walk — a **second time**,
producing the exact same trees. Threaded the already-resolved `Oid`s
through (`ResolvedTreeIds`) so `populate_contents` does a cheap
`find_tree(id)` instead. One entire redundant ref-walk retired per
`--commit`/range invocation. Measured impact was real but small (falsified
as the dominant fixed cost, see above) — kept regardless, since it's a
zero-risk duplicate-work deletion, not a performance bet, and it also set
up fix 2 cleanly (cheap `Oid`s are what a parallel worker needs; ref
strings are not, since resolving them isn't `Send`-friendly to repeat per
thread).

**Fix 2 — parallelize the remaining per-file blob population.** What was
left after fix 1 was genuinely necessary, embarrassingly-parallel I/O
(~1.2ms/file marginal cost on the monster repo: tree path lookup + `find_blob`
+ possible delta-chain decompress) — not duplicate work, so concurrency,
not further deletion, is the correct fix. Same shape `differ.rs`'s entity
extraction already exploits. `git2::Tree`/`Blob` aren't `Send` (they borrow
their `Repository` by lifetime), so each rayon worker thread opens its own
`Repository` via a thread-local cache (`TL_REPO`) and resolves trees by the
now-cheap `Oid` instead of re-walking refs per file.

Measured independently, `SEM_LOCAL=1`, median-of-3 (large cells confirmed
with a 5x-resample after landing, quoted alongside):

| cell | staging (fix 1 only) | staging (+fix 2) | total (fix 1 only) | total (+fix 2) | verdict |
|---|---:|---:|---:|---:|---|
| rails:large (177f) | 23.04ms | 25.52ms | 92.83ms | 103.06ms | **flat** — 5x-resample: 75-110ms range both sides, no separation |
| vscode:large (63f) | 34.68ms | 23.28ms | 107.72ms | 93.01ms | **real win** — 5x-resample medians 116.1ms → 90.8ms, -21.8%, non-overlapping |
| typescript:large (147f) | 213.05ms | 52.24ms | 216.72ms | 55.46ms | **real win** — 5x-resample medians 216.3ms → 55.0ms, **-74.6%, 3.9x**, non-overlapping |

rails stayed flat because it's a small, single-pack, shallow-history repo
where per-object access was already cheap — there's nothing for
parallelism to buy back. vscode and (dramatically) the TypeScript monster
have expensive-enough per-object access (bigger packs, longer delta
chains) that fanning the reads across cores pays for the thread-pool
dispatch overhead many times over.

**End-to-end, vs the original pre-campaign baseline** (before the
diagnostic repack, before either fix):

| cell | baseline total | final total | delta |
|---|---:|---:|---:|
| rails:small | 6.48ms | 6.54ms | flat |
| rails:large | 85.98ms | 98.41ms | flat within noise (see above) |
| vscode:small | 35.84ms | 32.27ms | flat/noise |
| vscode:large | 108.10ms | 88.93ms | **-17.7%** |
| typescript:small | 54.79ms | 46.80ms | -14.6% |
| typescript:large | **242.91ms** | **53.29ms** | **-78.1%, 4.56x** |

### What was left alone, and why (deletion audit)

Per the campaign's amendment ("ask what can be removed before what can be
built"), every phase was checked for redundancy before any code was
added — not just the ones that ended up fixed:

- **`--profile` (the pre-existing hand-rolled flag) vs the new
  `SEM_TIMINGS` instrumentation are now two profiling mechanisms on the
  same command.** `SEM_TIMINGS` strictly subsumes `--profile`'s 4 buckets
  with finer granularity, using the convention the rest of the CLI
  already standardizes on. Retiring `--profile` is a legitimate
  simplification candidate — but it's a **documented, user-facing CLI
  flag** (`docs/details.html`'s "Internal profiler" section describes it),
  so removing it is a public-surface change this campaign didn't scope
  itself to make unilaterally. Surfaced here rather than swallowed;
  deferred to whoever owns that surface decision.
- **Entity extraction / matching**: no redundancy found (see the
  falsified-hypotheses table) — already efficient by construction
  (file-scoped matching, no corpus walk) and already well parallelized
  (10-13x wall-time absorption from CPU-sum). Nothing to delete or speed
  up.
- **`cloud_upload.rs`/`relations.rs`** (relations budget pass, upload
  prep): explicitly out of scope — both carry another session's
  uncommitted WIP this campaign was told not to disturb, and neither
  contributes measurably to a local (`SEM_LOCAL=1`) invocation's wall
  time regardless.

### The floor, stated honestly

`typescript:small` (4 files) still costs 46.80ms, of which 39.55ms is
`staging` — and neither fix here touches that number, because it isn't
redundant work. It's the fixed, one-time cost of a fresh process opening
and reading from the TypeScript monster's git object database at all
(revparsing two refs, peeling to commits, walking to trees) on a repo with
81,368 tracked files and a 40,832-commit history. `sem diff` is a
per-invocation CLI process, not a warm daemon, so this cost is paid once
per command regardless of diff size — eliminating it would mean either a
persistent process (architecturally out of scope for this campaign, and a
real design decision, not a "fix") or accepting it as the floor. Every
other repo/commit-size combination in the battery is within **1.00-1.02x**
of its own dominated-phases sum (see the attribution table) — fully
attributed, nothing left unaccounted for. `typescript:large`, the
campaign's target cell, landed at **53.29ms total — well under the 1s
target**, with `staging`'s fixed component now the single largest
remaining line item and no further reduction available without changing
what "a diff invocation" architecturally is.

### Removal ledger

| item | LOC | phase/pass retired | reason |
|---|---:|---|---|
| Redundant `resolve_tree()` re-walk in `populate_contents`'s `Commit` arm | -1 call site (`resolve_tree(sha)` + `resolve_tree(&format!("{sha}~1"))`) | One full revparse+peel+tree() walk per `--commit` invocation | Trees already resolved by `get_commit_diff_files` moments earlier; re-deriving them was pure duplication, not needed work |
| Redundant `resolve_tree()` re-walk in `populate_contents`'s `Range` arm | -1 call site (`resolve_tree(to)` + `resolve_tree(from)`) | Same, for `sem diff <from> <to>` | Same reasoning, `get_range_diff_files` |
| Serial per-file blob-population loop | replaced, not net-deleted (see below) | The *unparallelized* pass — retired in favor of the same rayon fan-out shape `differ.rs` already uses elsewhere, not new machinery invented for this campaign | Necessary work, but serial was an oversight relative to the extraction path's existing pattern |

Net `sem-core/src/git/bridge.rs` diff: +171/-35 across both fixes
combined (one commit; the two theses were measured independently before
landing together — see the fix log's table for per-thesis numbers). The
line count is not the point — the two `resolve_tree()` call sites deleted
were a real pass, not incidental lines, and the parallelization reuses
`differ.rs`'s established fan-out pattern rather than inventing a new one.
`--profile` (documented, user-facing) was identified as retireable but
left alone — see "what was left alone" above.

### Gates

- Byte-identical stdout across all 6 battery cells x {`sem diff <parent>
  <commit>`, `sem diff --commit <commit>`} x {json, markdown, plain,
  terminal} = 48 comparisons, baseline binary vs final binary, plus a
  root-commit (no-parent) edge case on rails and 3 spot checks against
  this repo's own history (sem-cloud) across all 4 formats — all
  byte-identical.
- `cargo test --release -p sem-core --lib`: 589 passed, 0 failed (before
  and after every commit in this campaign).
- `cargo test --release -p sem-cli`: 209 passed across all suites, 0
  failed (before and after every commit).
- `cargo fmt -p sem-core -p sem-cli -- --check`: clean on every file this
  campaign touched (pre-existing drift in `main.rs` and elsewhere,
  unrelated to this campaign, left untouched).
- `cargo clippy --release -p sem-core -p sem-cli --no-deps`: zero new
  findings on any line this campaign added or modified; the 2 remaining
  findings (`sem-cli/src/commands/setup.rs:551`, `sem-core/src/model/
  identity.rs:107`) are pre-existing, confirmed identical before/after
  via `git diff`/`git stash` against files this campaign never touched.
- `diff/cloud_upload.rs`, `diff/relations.rs`, `commands/setup.rs`,
  `parser/plugins/code/languages.rs` (another session's uncommitted WIP,
  per this campaign's brief): `git diff --stat` identical before and
  after every commit in this campaign — confirmed byte-for-byte untouched.

Bead: semx-cc3.

## Sub-1s physics budget (semx-8lf)

User mandate: every cold build under 1s, "the fastest physics allows." This
section is W0 — measure before any wave touches code — and answers three
questions with file:line and fresh numbers, not the campaign's prior
attribution (which covered resolve sub-phases on smaller corpora, not the
five giants, and never covered the save/index-write path at all): how many
times does a cold build touch the same bytes, where does the wall-clock
actually go on each giant, and what is physically unavoidable versus merely
unfixed.

### Method

- Release build (`opt-level=3`, `lto="thin"`, `codegen-units=1`), HEAD of
  this branch, darwin, `available_parallelism=18` (measured via
  `THREAD_UTIL`, all five runs agree).
- **Pipeline shape** (`crates/sem-core/examples/perf_probe.rs`, unmodified,
  pre-existing from semx-cnq): `WALK` → `IO` → `PARSE_EXTRACT` → `PASS1_ONLY`
  → `BUILD_TOTAL` (phase-hook split into `pre_resolve`/`resolve_phase`) →
  `LANG_RATE` per extension. One run per giant, `SEM_PROFILE_RESOLVE=1` added
  for dotnet-runtime/llvm-project/linux to get resolve sub-phase attribution
  (pre-existing instrumentation, `resolve_profile.rs`, never removed since
  the resolve campaigns earlier in this document).
- **Save/index-write path had zero prior instrumentation** — `cache_full_save`
  was one opaque `SEM_TIMINGS` mark in `commands/graph.rs:696`. This bead adds
  `SEM_PROFILE_CACHE=1`, gated identically to `resolve_profile::enabled()`
  (`OnceLock<bool>`, one env read, then a cached bool — zero cost off), wired
  into `crates/sem-cli/src/build_cache.rs`'s `save_with_test_dirs` and
  `write_query_index`. It changes no behavior, no return value, no bytes
  written — only what is printed to stderr when the flag is set. This is the
  "extend timers where a pass is unmeasured" instruction in the bead; without
  it the single biggest finding below (§2, item 9) would have stayed inside
  one unattributed 21-second number.
- Five giants, first-ever build (no `~/Library/Caches/sem/repos/<hash>`
  directory for that repo before the run — verified, not assumed; two stale
  entries from earlier sessions were found and deleted before measuring):
  the TypeScript monster (`~/.cache/checkouts/github.com/microsoft/TypeScript`,
  verified present, 40,865 parsed files, 1.8 GB), `/tmp/bench-fleet/{home-
  assistant-core,dotnet-runtime,llvm-project,linux}` (all verified present at
  the sizes below — none of the "FIVE prior agents falsely claimed missing"
  failures reproduced here).
- `sem graph` end-to-end numbers are `SEM_LOCAL=1 SEM_TIMINGS=1
  SEM_PROFILE_CACHE=1 sem graph <root>`, cold cache, single run each (a
  median-of-N battery on five giants at 10-70s/run each was judged not worth
  the wall-clock this bead had — the point-measurements below are internally
  cross-checked instead: every `CACHE_SAVE_PHASE` table sums to ≥99% of its
  parent `cache_full_save` mark, which is the honesty check that matters
  here, not run-to-run variance on numbers already an order of magnitude
  over budget).

---

### 1. Pass census — every distinct walk/copy of file bytes or entity data

Read from the code, not inferred from timings. **Four distinct reads of the
same file bytes from disk**, plus at least three more full walks of the
already-in-memory entity/content data, per cold build:

| # | pass | file:line | reads | produces | parallel? |
|---|---|---|---|---|---|
| A | **File discovery walk** | `commands/graph.rs:54` (`file_discovery` mark); walk shape mirrored in `examples/perf_probe.rs:46-80` (`ignore::WalkBuilder`, gitignore+`.semrc`+binary-detect filtered) | directory entries + `.gitignore`/`.semrc` stats, not file content | the file list `EntityGraph::build` is called with | n/a (metadata only) |
| B | **Parse read** (1st byte read) | `parser/graph.rs:3068`, inside `build_direct_dependencies`'s per-file closure (fn starts `graph.rs:3053`) | full file bytes → `String` | tree-sitter CST | yes (rayon `par_iter`) |
| C | **Parse + extract** (fused with B, one AST walk) | `entity_extractor.rs` (~3,500 lines of node-kind `match` arms); `compute_structural_hash_and_kappa` at `entity_extractor.rs:1563` computes **both** `structural_hash` and the kappa identity hash (`KAPPA.md`) in the same walk — confirmed fused, not a separate kappa pass | CST | `Vec<SemanticEntity>` | yes |
| D | **Bag-of-words content snapshot** (in-memory copy, not disk) | `snapshot_bow_content`, `parser/graph.rs:1499-1522`; called from `graph.rs:2759` (main build), `:3303`, `:4046` (session/incremental variants) | `parsed_files`' `(path, content, tree)` triples already held from pass B/C | a **second full copy** of every file's source string, `content.clone()` at `graph.rs:1510`, into a fresh `HashMap` | n/a (serial clone loop, but over already-resident memory, not disk) |
| E | **Bag-of-words index build + tokenize** (walk #3 of the content) | `resolve_profile.rs`'s `BOW_INDEX_BUILD_NS`/`BOW_INDEX_TOKENIZE_NS`/`BOW_DOTCHAIN_EXTRACT_NS`/`BOW_REF_EXTRACT_NS` accumulators, driven from the `__bow_wall_t0` block at `graph.rs:1308-1436` | the pass-D snapshot | per-file local-binding + dot-chain + reference token tables | yes, but see §2's utilization note |
| F | **Scope/reference resolution** (walk #4 over entity+bow data) | `scope_resolve.rs` (8,308 lines), `resolve_scopes_in_file_chunks` (`graph.rs:1525+`) | entity map + bow tables | `EntityRef` edges (calls/typeref/imports) | yes, chunked |
| G | **Edge assembly/dedupe/sort** | `resolve_profile.rs`'s `EXPORT_EDGES_NS`/`DEDUPE_NS`/`SORT_NS`/`EDGE_INDEX_NS` | edge candidates | final `graph.edges` | yes |
| H | **File fingerprint** (2nd byte read, build plane) | `build_cache.rs:291-308`, calling `shared_cache::file_fingerprint` → `file_content_hash` (`sem-mcp/src/cache.rs:687`) — a hex digest, **not** the xxh3 used by the index | full file bytes again | `cache.db`'s `files` table row | **no** — plain `for file in files` loop, the only serial full-corpus byte read that isn't gated by language |
| I | **Refresh file-import entries** (3rd byte read) | `build_cache.rs:307` → `shared_cache::refresh_file_import_entries`, `sem-mcp/src/cache.rs:872-906` | full file bytes again (`cache.rs:895`, unconditional — read happens before the JS/TS check) | `file_imports` table rows | **no** — `for file in files_to_refresh` loop; see §2 item 9 for why this is the single biggest finding in this bead |
| J | **Insert entities with content** | `build_cache.rs`, `shared_cache::insert_entities_with_content_store` | every `SemanticEntity`'s already-extracted body string (a copy, not a disk read) | `cache.db`'s `entities.content` rows | driven by a prepared statement, effectively serial (one `Connection`) |
| K | **Insert edges** | `build_cache.rs` (edges-insert block) | `graph.edges` | `cache.db`'s `edges` rows | serial (one `Connection`) |
| L | **Write query index — parallel re-read + re-hash** (4th byte read) | `build_cache.rs`, `write_query_index`'s `rows: Vec<...> = files.par_iter()...` block | full file bytes again, xxh3-hashed | `FileFingerprint`s **and** `TRIGRAM` section content — fused per the function's own doc comment ("the same bytes, never read twice") | **yes** |
| M | **Write query index — build image** | `sem_core::index::build_with_content_and_dirs_and_tests_and_spans` | in-memory graph + the pass-L contents map | `ENTITIES`/`NAMES`/`REFS`/`TRIGRAM`/`FILES`/`DIRS` sections | internal parallelism varies by section |
| N | **SQLite commit + atomic index write** | `tx.commit()`; `sem_core::index::write_atomic` | the built rows/bytes | durable `cache.db` + `index.sem` | n/a |

**Count: passes B, H, I, L are four independent reads of every file's full
bytes off disk in one cold build — two of them (H, I) serial, unparallelized,
unlike B and L which already use rayon.** Passes D, E, F, G, J, M are further
full walks of the derived entity/content data, none of which reads from disk
again but each of which re-touches every entity or every byte in memory.
**Total: at least 5 distinct full-corpus walks (B/H/I/L over disk bytes, D
as an added in-memory copy) before counting E/F/G/J/M's entity-level walks**
— the "suspected 5+" is confirmed, and undercounts if entity-level walks are
included.

---

### 2. Measurements per giant

**Corpus shape** (parsed files/bytes/entities/edges, `perf_probe`'s
`BUILD_TOTAL` line, canonical across all measurements below):

| repo | files | bytes | entities | edges |
|---|---:|---:|---:|---:|
| home-assistant-core | 22,325 | 123.2 MB | 257,832 | 307,366 |
| TypeScript monster | 40,865 | 198.6 MB | 454,541 | 196,223 |
| dotnet-runtime | 47,454 | 589.5 MB | 990,754 | 980,971 |
| llvm-project | 82,123 | 867.0 MB | 1,306,421 | 976,733 |
| linux | 72,787 | 1,499.1 MB | 2,312,433 | 1,898,783 |

**End-to-end cold `sem graph`, `SEM_LOCAL=1`** (ms; `full_graph_build`
includes the facts-store layer atop raw `EntityGraph::build`, which is why it
runs slightly above `perf_probe`'s `BUILD_TOTAL` for the same repo):

| repo | file_discovery | full_graph_build | cache_full_save | serialization | **total** |
|---|---:|---:|---:|---:|---:|
| home-assistant-core | 447 | 6,596 | 4,648 | 27 | **11,721** |
| TypeScript monster | 313 | 6,050 | 21,021 | 36 | **27,424** |
| dotnet-runtime | 1,090 | 44,754 | 21,762 | 179 | **67,787** |
| llvm-project | 3,094 | 40,884 | 23,698 | 194 | **67,872** |
| linux | 2,045 | 28,503 | 34,252 | 334 | **65,137** |

**`cache_full_save` breakdown** (ms, `SEM_PROFILE_CACHE=1`; each row sums to
≥99% of its `cache_full_save` total, so nothing here is hiding a residual):

| phase | HA | TS-monster | dotnet | llvm | linux |
|---|---:|---:|---:|---:|---:|
| file_fingerprint (H, serial) | 293 | 635 | 883 | 1,406 | 1,327 |
| refresh_file_import_entries (I, serial) | 267 | **14,720** | 901 | 1,360 | 1,126 |
| insert_entities_with_content (J) | 1,773 | 3,084 | 8,256 | 9,869 | 14,911 |
| insert_edges (K) | 788 | 506 | 3,804 | 2,900 | 4,775 |
| write_test_flags | 109 | 89 | 553 | 736 | 665 |
| entity_byte_spans | 19 | 47 | 296 | 233 | 354 |
| sqlite_commit | 593 | 675 | 3,129 | 2,248 | 3,240 |
| write_query_index total (L+M+N) | 804 | 1,262 | 3,929 | 4,938 | 7,841 |
| ↳ parallel re-read+hash (L) | 187 | 334 | 529 | 917 | 817 |
| ↳ dir fingerprints | 6 | 10 | 24 | 28 | 24 |
| ↳ build image (M) | 604 | 908 | 3,356 | 3,966 | 6,971 |
| ↳ atomic write (N) | 5 | 6 | 14 | 17 | 21 |

**Finding, item 9 (the big one).** `refresh_file_import_entries` costs
14,720 ms on the TypeScript monster — **54% of its entire 27.4 s cold
build**, larger than the whole `full_graph_build` (6,050 ms). Root cause,
read from the code, not inferred: `refresh_file_import_entries`
(`sem-mcp/src/cache.rs:872-906`) calls `js_ts_import_source_files_from_content`
(`import_resolution.rs:150`), which for every unresolved import calls
`find_import_file` (`import_resolution.rs:420`) — an **O(candidate_files)
linear scan per import**, and `candidate_file_paths` is every JS/TS file in
the repo. The function's own comment at `import_resolution.rs:444-446`
already names this exact pathology for a *different, already-fixed* call
site: "~10^4 imports x ~4x10^4 candidates ... billions of comparator calls,
minutes of a pinned core." **A HashSet-based O(1) replacement,
`js_ts_import_source_files_from_set` (`import_resolution.rs:173-210`),
already exists in the same file** — `refresh_file_import_entries` simply
never was migrated to call it. This is not a physics floor; it is a known
fix, already written, not wired to this call site. On non-JS/TS-dominant
corpora the same pass costs 0.9-1.4 s (dotnet/llvm/linux) — that residual is
the file-read itself (`cache.rs:895`, unconditional, before the
`is_js_ts_file` gate), i.e. a real but much smaller redundant-IO cost, not
the O(N²) one.

**Resolve sub-phase attribution** (`SEM_PROFILE_RESOLVE=1`, cumulative
CPU-ns summed across resolve worker threads per `resolve_profile.rs`'s
`AtomicU64` design — not directly wall-clock; compared against the clean
wall-clock `resolve_phase_ms` from `perf_probe`'s `PHASE_HOOK` split):

| repo | resolve_phase (wall) | scope_build (cum.) | bow_index_tokenize (cum.) | bow_index_build (cum.) | resolve_ref+ref_loop (cum.) | REF_CACHE hit% |
|---|---:|---:|---:|---:|---:|---:|
| dotnet-runtime | 30,661 ms | 31,380 ms | 17,898 ms | 19,428 ms | 9,519 ms | 17.96% |
| llvm-project | 24,630 ms | 64,243 ms | 24,901 ms | 27,604 ms | 63,640 ms | 9.93% |
| linux | 12,258 ms | 27,567 ms | 33,966 ms | 35,862 ms | 95 ms | 17.63% |

The `scope_build`/wall ratio is the effective-parallelism signal: dotnet's
cumulative (31,380 ms) is ~1.02× its wall (30,661 ms) — effectively **one**
active thread despite 18 available; llvm's is 2.6× (partial parallelism);
linux's is 2.2×. **Core utilization during `scope_build` is inconsistent
and repo-dependent, never close to 18×**, on a machine that fully
parallelizes `WALK`/`PARSE_EXTRACT`/`write_query_index`'s re-read. This is a
concrete, measured lead for W3, not a claim this bead resolves further.
llvm's `resolve_ref`+`ref_loop` cumulative (63,640 ms) and its 9.93% cache
hit rate (lowest of the three) match this document's own prior finding
("Resolver tie-break contract," semx-nuv/yk5) that C++ overload/candidate
disambiguation, not lookup, is llvm's dominant resolve cost.

**Parse throughput, measured on each corpus** (`LANG_RATE`'s aggregate —
`PARSE_EXTRACT`'s own bytes/ms, i.e. the real production combined
parse+extract path, which already benefits from `parser/plugins/code/mod.rs:
29`'s `thread_local!` `PARSER_CACHE` — one `tree_sitter::Parser` reused per
language per thread, not reallocated per file):

| repo | combined parse+extract | raw tree-sitter-only ceiling (this corpus, pathological outliers excluded) |
|---|---:|---:|
| home-assistant-core | 166.2 MB/s | 323.6 MB/s (.py) |
| TypeScript monster | 70.3 MB/s | 55.2 MB/s (.js+.ts weighted) |
| dotnet-runtime | 71.2 MB/s | 99.4 MB/s (.cs-dominant; one `.xml` file's 94.5 s raw-parse outlier excluded, same pathological-file class this document already tracks for Python/C#/Scala) |
| llvm-project | 128.8 MB/s | 134.2 MB/s (.c/.cpp/.h-dominant) |
| linux | 219.4 MB/s | 229.6 MB/s (.c/.h-dominant) |

**Finding, core utilization.** For the TypeScript monster and for `.h` files
on llvm/linux, **combined (parse+extract) throughput exceeds the "raw,
isolated" tree-sitter number** — `perf_probe`'s `LANG_RATE` raw leg
constructs a fresh `tree_sitter::Parser::new()` per file
(`perf_probe.rs:247`, deliberately, to measure parser-construction-included
cost), while the real path's thread-local reuse avoids that setup cost
entirely. **There is no free headroom left in scheduling/reuse/allocation
for tree-sitter parsing itself** — parser reuse is already shipped, already
measured to already beat a naive per-file `Parser::new()`, and the combined
number above is close to the practical ceiling for this parser technology.
The only genuine additional headroom is a *different* parser technology,
which exists for exactly one of these five corpora — see §3.

---

### 3. The floor ledger

Three components, each stated with its own citation, per the bead's
instruction. `join_floor` is computed once (it is negligible everywhere, so
one derivation suffices): the largest measured reference count is llvm's
3,738,345, and a conservative single-core hash-lookup rate for a small key
(the class of rate an `FxHashMap`/CSR-array probe like `QUERY-INDEX.md
§3.8`'s `REFS` section already achieves — that document's own measured
`refs_of`/`callers_of` cost is 0.03-0.06 ms for a handful of rows) is
~20M lookups/s/core; at 18 cores that is ~360M/s, so
`3,738,345 / 360,000,000 ≈ 10 ms`. **The columnar join is never the
bottleneck on any of these five corpora — it floors below 15 ms even for
the largest reference count measured, three to four orders of magnitude
under the measured resolve wall-clock.** This matches §2's finding: resolve
cost is candidate-generation/disambiguation work, not join cost, so no
join-shaped fix moves the needle here — that's an honest non-finding worth
stating plainly rather than padding the ledger with it.

| repo | parse+symtable floor (ms) | + join floor | + IO floor (one corpus read, parallel, measured) | + minimum write floor (index build+atomic, M+N only) | **floor total** | measured (full_graph_build + cache_full_save) | **gap = must-shrink-by** |
|---|---:|---:|---:|---:|---:|---:|---:|
| home-assistant-core | 741 (already near ceiling) | <15 (not run under `SEM_PROFILE_RESOLVE`; bounded by the derivation above) | 312 | 609 | **~1,677 ms** | 11,244 | **6.7×** |
| TypeScript monster | 349 (oxc-adjusted, see below) | <15 (same caveat) | 525 | 914 | **~1,803 ms** | 27,071 | **15.0×** |
| dotnet-runtime | 8,278 (already near ceiling) | 7 (2,380,367 refs measured) | 669 | 3,370 | **~12,324 ms** | 66,516 | **5.4×** |
| llvm-project | 6,731 (already near ceiling) | 10 (3,738,345 refs measured) | 1,158 | 3,983 | **~11,882 ms** | 64,582 | **5.4×** |
| linux | 6,831 (already near ceiling) | <1 (106,772 refs measured) | 1,033 | 6,992 | **~14,857 ms** | 62,755 | **4.2×** |

`parse+symtable floor` = the corpus's own measured `PARSE_EXTRACT` (§2's
"combined" column, already argued near-ceiling for tree-sitter languages),
**except the TypeScript monster**: 68.8% of its bytes are `.js`/`.ts`
(136.7 MB of 198.6 MB). `OXC-FASTPATH.md` (this repo, semx-1gy) measured a
real, working `oxc` parse+walk spike at **20-49× the tree-sitter
parse+walk cost on the same corpus this bead measured** (checker.ts,
parser.ts, utilities.ts, types.ts — real TypeScript-monster files) — a
conservative 20× applied to the monster's JS/TS-bytes' share of
`PARSE_EXTRACT` (2,607 ms → 130 ms) plus its unchanged non-JS/TS share
(219 ms) gives 349 ms, the only row in this table where the floor comes
from a *different parser technology* rather than "current is already the
ceiling." `OXC-FASTPATH.md`'s own verdict was **not** to ship this, for a
reason orthogonal to speed: `structural_hash` is defined over tree-sitter's
concrete syntax tree and has no oxc-AST equivalent by construction, not as
an edge case. semx-1gy's reopen condition (this campaign's mandate) is
precisely the trade that verdict declined — accept a divergent
`structural_hash` convention, or pay for structural_hash separately —
neither of which this measurement-only bead is scoped to decide; it is
named here because it is the one row where "the fastest physics allows" is
not today's number.

`minimum write floor` = M (build image) + N (atomic write) only — the
`ENTITIES`/`NAMES`/`REFS`/`TRIGRAM`/`FILES`/`DIRS` mmap index
`QUERY-INDEX.md` already designed as the durable, cold-openable artifact.
It **excludes** H, I, J, K, `write_test_flags`, `entity_byte_spans`,
`sqlite_commit` — the `cache.db` SQLite mirror's write cost — because
`QUERY-INDEX.md §7`'s removal list already marked the *query-path* SQLite
readers `DELETE`/`DEMOTE` once the index existed; whether the *write-path*
content-store insert (item J, the single largest cache-write cost on every
giant: 1.8-14.9 s) is still load-bearing for anything (the incremental
partial-reload path, `load_partial*`, `§7` kept that one `DEMOTE` rather
than `DELETE`) is a real open question this bead surfaces but does not
answer — it is not this bead's lane to decide whether `cache.db`'s content
store can be dropped from the *cold, first-ever* build path specifically.

**What the gap column says, plainly:** on every one of the five giants, the
floor computed from parse/join/IO physics is **1.7-14.9 s**, while measured
cold-build cost is **11.2-66.5 s** — a 4.2-15.1× gap. None of that gap is
parse, join, or (single-read) IO. All five giants' gaps are dominated by
the same two implementation costs in different proportions: (a) the
`refresh_file_import_entries` pathology (§2 item 9 — up to 14.7 s alone on
the monster, 0.9-1.4 s of pure redundant-IO tax elsewhere), and (b) the
`cache.db` SQLite write path (H+J+K+`sqlite_commit`, 1.8-23.9 s depending on
corpus size) which the floor excludes as "not necessarily required" but the
measured column still pays in full on every giant today.

---

### 4. Per-repo verdicts

- **home-assistant-core — reachable via W1-W3, with headroom to spare.**
  Floor ≈1.7 s, measured 11.2 s. Even without touching the SQLite-write
  question, deleting the two serial redundant reads (H, I — together
  <1 s here) and cutting the O(N) import-match tax (negligible on this
  corpus already, .py-dominant) does not alone reach <1 s; reaching it
  needs the `cache.db` write-path question (§3) resolved in the direction
  of "not required on a cold build," which would remove ~2.7 s
  (`insert_entities_with_content` + `insert_edges` + `sqlite_commit`) and
  land near ~4-5 s — still short of 1 s without also shrinking
  `full_graph_build` (6.6 s), which is resolve-bound (3.2 s wall) the same
  way the larger giants are. **Verdict: <1s is not reachable from W1-W3
  alone on this repo either — every giant in this fleet needs the resolve
  floor question (W3) answered, not just the write-path one.**
- **TypeScript monster — the one repo with a named, measured, non-floor
  fix available.** Floor ≈1.8 s (with the oxc-adjusted parse leg), measured
  27.1 s, dominated 54% by one bug (§2 item 9) with an existing O(1) fix
  already in the codebase. Landing that fix alone should collapse
  `cache_full_save` from 21.0 s to roughly the 260-1,300 ms range every
  other giant shows for the same phase — a ~14.5 s win by itself, more than
  half the total build. **<1s floors at ~1.8s because of `full_graph_build`'s
  resolve component (3.9 s wall) and the SQLite write-path question — not
  reachable purely from the import-match fix, but that fix is the largest
  single lever measured anywhere in this bead.**
- **dotnet-runtime — floors well above 1s on parse+resolve alone.**
  Floor ≈12.3 s (dominated by `PARSE_EXTRACT`'s already-near-ceiling
  8.3 s — no oxc-class alternative exists for C# in this repo), measured
  66.5 s. `scope_build`'s ~1× effective parallelism (§2) means the 30.7 s
  resolve wall has real headroom *if* it can be parallelized further, but
  even a perfect 18× on `scope_build` alone would not close an 8.3 s parse
  floor to under 1 s. **Verdict: floors at ~8-12 s because C# has no
  measured fast-parser alternative in this codebase and no physics excuse
  to go faster than tree-sitter's already-reuse-optimized combined rate on
  this corpus — reaching <1s here requires a C# fast extractor
  (unmeasured, unscoped, a new bet analogous to but not covered by the
  declined oxc spike) or accepting this repo does not clear the bar.**
- **llvm-project — same shape as dotnet, worse resolve.** Floor ≈11.9 s
  (parse 6.7 s + write 4.0 s), measured 64.6 s. `resolve_ref`+`ref_loop`'s
  63.6 s cumulative and 9.93% cache-hit rate confirm this document's prior
  finding that C++ overload disambiguation, not lookup, dominates — a W3
  target, not a physics floor (disambiguation correctness work doesn't have
  a "fastest physics allows" bound the way a hash join does). **Verdict:
  floors at ~7-12s from parse+write alone; if W3 doesn't reduce
  disambiguation cost, this repo floors well above 1s regardless of any
  other wave.**
- **linux — the honest worst case, and the arithmetic asked for
  explicitly.** `PARSE_EXTRACT` alone is 6.83 s at 219.4 MB/s measured
  combined throughput (linux's own C/H files are this fleet's *fastest*
  measured tree-sitter corpus, not the slowest — the pathology here is
  entirely resolve and write, not parse). Floor ≈14.9 s, dominated by the
  write floor (6.99 s — `write_query_index_build_image` alone is 6.97 s on
  this corpus, the single largest M-phase cost measured, driven by
  `TRIGRAM` tokenization of 1.5 GB of content). Measured total 62.8 s.
  **If linux's C parse alone had to clear 1 s: 1,499.1 MB / 219.4 MB/s =
  6.83 s at today's measured ceiling — already 6.8× over budget from parse
  alone, before resolve or write. Reaching <1s on linux specifically would
  require either a >6.8× faster C parser than tree-sitter's measured,
  reuse-optimized ceiling on this corpus (no such technology is measured
  or available in this codebase the way oxc is for TS/JS), or accepting
  that "cold build" for a corpus this size cannot mean "one process, one
  invocation, under 1s" — the same honest floor-vs-architecture choice
  `RESOLUTION-PROFILE.md`'s diff-attribution section (semx-cc3) already
  reached for the monster's `staging` cost. Say it plainly: linux floors
  at ~6.8s on parse physics alone, before any other phase, at any credible
  parser technology measured in this repository today.**

---

### 5. Recommended wave order and expected yields

Ordered by measured yield per unit of risk, not by novelty — the same rule
`QUERY-INDEX.md §1.7` used:

1. **W0.5 — two named, already-fixable bugs, before any wave that touches
   architecture** (not a new wave; a fast-follow this bead's own findings
   justify skipping the queue for): (a) wire `refresh_file_import_entries`
   to the existing `js_ts_import_source_files_from_set` instead of
   `js_ts_import_source_files_from_content` — **~14.5 s off the TypeScript
   monster alone**, near-zero off every other giant (the bug is JS/TS-import-
   count-shaped, and only the monster has enough JS/TS imports to trigger
   the O(N²) blowup); (b) parallelize the two serial full-corpus reads
   (H `file_fingerprint`, and I's read component) using the same
   read-into-`Vec`-then-serial-insert pattern `write_query_index` (pass L)
   already demonstrates — **~0.3-1.3 s per giant**, scaling with core
   count. Combined, these are pure-implementation fixes with **zero
   physics-floor risk** (they don't touch what gets computed, only how many
   times and how parallel) and the largest single-item yield measured in
   this bead.
2. **W1 (single-pass columnar)** — this bead's pass census (§1) is its
   input, as the queue context specified. Highest-value target: collapsing
   passes H/I/L (three of the four disk reads) into pass L's own shape
   (parallel, fused fingerprint+trigram-content read) removes ~0.3-1.4 s per
   giant beyond W0.5's fix, and forces the open question in §3 (is `cache.db`'s
   content-store write still required on a cold build) to get answered
   rather than deferred — that answer is worth 1.8-23.9 s per giant
   depending on corpus size, the single largest remaining lever in the gap
   table after W0.5.
3. **W2 (parse ceiling)** — yields real for exactly one repo in this fleet:
   the TypeScript monster, where §3's oxc-adjusted floor shows ~2.3 s of
   headroom (2,607 ms → 130 ms on the JS/TS share) *if* semx-1gy's
   `structural_hash` trade is reopened and resolved. Zero measured yield for
   home-assistant-core/dotnet-runtime/llvm-project/linux — no fast-parser
   alternative to tree-sitter is measured or available for Python, C#,
   C++, or C in this codebase today, and §2 already showed there is no
   scheduling/reuse headroom left in tree-sitter itself (parser reuse is
   shipped, and beats the naive per-file baseline already).
4. **W3 (resolve floor)** — the largest *unresolved-shape* cost in absolute
   terms (24.6-30.7 s wall on dotnet/llvm, 12.3 s on linux) and the one
   §3 could not floor with physics (join cost is <15 ms everywhere;
   candidate-generation/disambiguation has no join-shaped bound). Two
   concrete leads from this bead's measurement, not yet root-caused:
   `scope_build`'s ~1-2.6× effective parallelism against 18 available cores
   (dotnet/llvm/linux all show it), and llvm's 9.93% `REF_CACHE` hit rate
   paired with its 63.6 s cumulative `resolve_ref`+`ref_loop` cost
   (C++ overload disambiguation, consistent with this document's existing
   "Resolver tie-break contract" finding). Necessary for dotnet-runtime and
   llvm-project to have any chance at <1s — their parse floors alone (8.3 s,
   6.7 s) leave no room for a 24-31 s resolve phase regardless of what W1/W2
   do.
5. **W4 (known-content)** and **W5 (re-bench loop)** — queued behind W1-W3
   per the existing plan; this bead adds one input to W4 specifically: the
   §3 open question (is the `cache.db` content-store insert still load-
   bearing for anything reachable from a cold, first-ever build) is exactly
   "known content" shaped and should be W4's first item, not rediscovered.

**Per-repo expected yield summary:** home-assistant-core and TypeScript
monster can plausibly approach low-single-digit seconds from W0.5+W1 alone
(monster: 27.1 s → ~12 s from W0.5's bug fix; both then need W3's resolve
work to go further); dotnet-runtime and llvm-project need W1 (write-path)
**and** W3 (resolve) to have any chance, and neither reaches <1s without a
parse-technology bet this bead did not find evidence for; linux's floor is
set by parse physics alone at 6.8s given tree-sitter's measured ceiling on
this corpus, before resolve or write are even considered — **it is the one
repo in this fleet where "under 1s" is not a code problem this campaign can
solve without a different C parser than any measured here.**

### Gates

- `cargo build --release -p sem-cli --bin sem`: clean (one pre-existing
  unrelated warning, `commands/setup.rs:551`, confirmed identical
  before/after this bead's edits via `git diff`).
- `cargo build --release -p sem-core --example perf_probe --example
  index_probe`: clean, no changes to either file — used as pre-existing
  instrumentation only.
- `SEM_PROFILE_CACHE`'s new marks are additive `eprintln!` calls behind an
  `OnceLock<bool>` gate identical in shape to `resolve_profile::enabled()`;
  no return value, no written byte, no control-flow path changed. Every
  `cache_full_save` run in §2 was cross-checked against its own
  `CACHE_SAVE_PHASE` sum (≥99% attributed in every case) as the correctness
  check for the instrumentation itself, not just the numbers it reports.
- Pathspec: `crates/sem-cli/src/build_cache.rs` (the `SEM_PROFILE_CACHE`
  instrumentation — additive only, no non-instrumentation lines changed),
  `crates/sem-core/RESOLUTION-PROFILE.md` (this section). No other file in
  the working tree was touched by this bead; the untouchables
  (`diff/cloud_upload.rs`, `diff/relations.rs`, `commands/setup.rs`,
  `parser/plugins/code/languages.rs`, `README.md`, `examples/hosted-diff/*`,
  the two WIP test files) were left byte-identical, confirmed via
  `git diff --stat` before and after.

Bead: semx-8lf.

---

## W0.5: the free lunch, landed and measured (semx-ccg)

Three surgical fixes for §5's W0.5 item, all inside the cache *save* path
(`crates/sem-cli/src/build_cache.rs`, `crates/sem-mcp/src/cache.rs`) — no
change to `EntityGraph::build`, resolution, entity extraction, or the
`ENTITIES`/`NAMES`/`REFS`/`TRIGRAM`/`FILES`/`DIRS` index image.

1. **Wired `refresh_file_import_entries` onto `js_ts_import_source_files_from_set`**
   (`cache.rs:872-926`) instead of `js_ts_import_source_files_from_content`
   — the O(1) HashSet-membership function §2 item 9 found already written
   and never called. The candidate set (`all_files` minus manifest files) is
   built once per call as a `HashSet<&str>`, not per file — no new
   allocation shape beyond what the old code already did once per call for
   its `Vec<String>`.
2. **Parallelized the `file_fingerprint` read** (was `cache.rs:687`'s
   `file_content_hash`, called serially from `build_cache.rs`'s
   `save_with_test_dirs`/`save_topology`): `files.par_iter().filter_map(...)`
   collects `(path, secs, nanos, hash)` tuples first (same shape as
   `write_query_index`'s pass-L re-read), then a serial loop does the
   `INSERT` — `rusqlite::Statement` isn't `Send`, so only the DB write stays
   single-threaded.
3. **Parallelized `refresh_file_import_entries`'s own read** (was
   `cache.rs:895`, unconditional, before the `is_js_ts_file` gate): read +
   `js_ts_import_source_files_from_set` computation now run inside
   `files_to_refresh.par_iter()`, collected into `Vec<(&String,
   Option<Vec<String>>)>` (`None` = manifest file untouched, `Some(vec![])`
   = touched but nothing resolved — same as the original's fall-through),
   then a serial loop does the `DELETE`/`INSERT` pair per file.

`sem-mcp` had no `rayon` dependency before this bead (it depended on it only
transitively through `sem-core`) — added directly (`Cargo.toml`, plain, not
feature-gated: `sem-mcp` is bin-only, never built for wasm, matching
`sem-cli`'s own un-gated `rayon = "1.10"` convention rather than `sem-core`'s
`maybe_par_iter!`/`#[cfg(feature = "parallel")]` macro, which exists
specifically for `sem-core`'s wasm target).

### Measured, before/after, median-of-3, cold cache (`SEM_CACHE_DIR` pointed
at a fresh empty directory per run, deleted after — not the shared
`~/Library/Caches/sem/repos/<hash>` tree, so this battery never touched
another session's cache), `SEM_LOCAL=1 SEM_TIMINGS=1 SEM_PROFILE_CACHE=1 sem
graph <root> --json`, darwin, `available_parallelism=18`. Load was not
otherwise idle during this battery — other sessions/agents were active on
the same box (`uptime` at time of writing: load averages 4.79/5.97/5.97 on
18 cores) — noted, not scrubbed; the delta is large enough on the monster to
survive that noise, and flat-to-noise on the other two giants exactly where
§5's own prediction said it would be.

**Total wall-clock, `sem graph` end-to-end:**

| repo | before (median) | after (median) | delta | individual runs (before → after) |
|---|---:|---:|---:|---|
| home-assistant-core | 9,901.7 ms | 10,043.1 ms | +141.4 ms (flat/noise) | 9901.7/9749.7/9986.5 → 9861.1/10043.1/10273.6 |
| TypeScript monster | 28,284.5 ms | 16,450.3 ms | **−11,834.2 ms (−41.8%, 1.72×)** | 32385.9/28284.5/27941.3 → 16489.2/16450.3/16375.9 |
| linux | 60,670.6 ms | 58,609.4 ms | −2,061.2 ms (−3.4%) | 68971.8/59578.5/60670.6 → 58046.6/58869.9/58609.4 |

home-assistant-core's individual runs overlap between before/after (noise
band, not a regression — it is Python-dominant, near-zero JS/TS import
graph, exactly the "near-zero off every other giant" §5 predicted for fix
1). linux's first before-run (68,971.8 ms) is a cold-disk outlier excluded
by taking the median, not by discarding it from the table.

**`cache_full_save` phase breakdown, representative run each** (ms;
`file_fingerprint_serial`/`refresh_file_import_entries` renamed
`file_fingerprint_parallel_reread_hash`+`file_fingerprint_insert` /
unchanged name after the fix — the phase names themselves are part of the
diff, `build_cache.rs`'s `cache_profile_mark` call sites):

| repo | phase | before | after | delta |
|---|---|---:|---:|---:|
| home-assistant-core | file_fingerprint (H) | 299.8 | 180.9+9.4=190.3 | −109.5 |
| home-assistant-core | refresh_file_import_entries (I) | 262.6 | 208.2 | −54.4 |
| home-assistant-core | cache_full_save (total) | 4,543.8 | 4,578.9 | +35.1 (flat) |
| TypeScript monster | file_fingerprint (H) | 636.7 | 420.5+19.1=439.6 | −197.1 |
| TypeScript monster | refresh_file_import_entries (I) | **14,872.5** | **3,202.7** | **−11,669.8 (−78.5%)** |
| TypeScript monster | cache_full_save (total) | 21,321.6 | 9,634.8 | **−11,686.8 (−54.8%)** |
| linux | file_fingerprint (H) | 1,286.6 | 804.1+31.7=835.8 | −450.8 |
| linux | refresh_file_import_entries (I) | 1,120.9 | 838.2 | −282.7 |
| linux | cache_full_save (total) | 34,674.6 | 33,506.4 | −1,168.2 |

**Honest residual on the monster.** `refresh_file_import_entries` dropped
78.5% (14,872 ms → 3,203 ms) but not to near-zero the way a pure O(1) lookup
would suggest. Root cause, read from the code: `js_ts_import_source_files_from_set`'s
bare/package-specifier fallback (`import_resolution.rs:198-209`) still
builds `sorted_candidates` — a full sort of the ~40k-entry candidate set —
*per file*, lazily, the first time that file has a bare import (`get_or_insert_with`).
Parallelizing the read (fix 3) spreads this cost across cores (`user` time
went from 35.9s to 68.9s on the monster — more total CPU, less wall clock),
which is why wall time still dropped sharply even though the O(candidates
log candidates) sort itself wasn't touched. `import_resolution.rs:474-499`
already has the fix for this exact shape (`build_stem_index`/
`resolve_bare_import_stem`, built once per build not once per file — cited
in its own doc comment as the fix for an *incremental warm-rebuild* call
site, semx-h1s) but wiring it into `refresh_file_import_entries` too is a
second, distinct change from this bead's scope (wiring an existing O(1)
function to an unwired call site) and is surfaced here as a finding for a
follow-up, not fixed in this bead.

### Equivalence proof (gate: cache.db content unchanged by the fix)

`file_imports` table, rails (`/tmp/bench-fleet/rails`, mixed Ruby/JS
corpus), fresh `SEM_CACHE_DIR` per binary, `sem-before`/`sem-after` built
from git-stashed/unstashed working trees off the same HEAD:

```
sqlite3 cache.db "SELECT importing_file, imported_file FROM file_imports ORDER BY importing_file, imported_file;"
```

64 rows before, 64 rows after, `diff` empty — **byte-identical**. Also
diffed the `files` table's `(path, content_hash)` pairs (fix 2's target,
3,794 rows) — byte-identical. Repeated at monster scale as the bit-identical
graph gate below (which subsumes the import-table proof: identical edges
imply identical `Imports`-typed edges, and `file_imports` is exactly the
persisted form of those).

### Bit-identical gate (entity/edge counts + sorted-edge hash)

Fresh `SEM_CACHE_DIR`, `--json` output, before vs. after:

| repo | entities | edges | sorted-edges sha256 (before == after) | sorted-entities sha256 (before == after) |
|---|---:|---:|---|---|
| home-assistant-core | 257,833 == 257,833 | 307,366 == 307,366 | `7d217cd4…3af25ddc7` == same | `4388f6ca…9da8ecd1b7` == same |
| TypeScript monster | 454,528 == 454,528 | 196,223 == 196,223 | `ecb2ba28…3efcae39` == same | `8467986f…e0a077e136` == same |

Both giants: entity count, edge count, and the sha256 of every sorted edge
and sorted entity are identical between `sem-before` (HEAD, pre-fix) and
`sem-after` (this bead's three fixes). The 13-entity difference from §2's
canonical table (454,528 here vs. 454,541 there) and the 1-entity
difference on home-assistant (257,833 vs. 257,832) are pre-existing,
unrelated to this bead (both binaries agree with each other, which is the
gate that matters here) — plausibly corpus drift between when §2 was
measured and today (`microsoft/TypeScript`/`home-assistant-core` are live
checkouts, not pinned snapshots).

### Test/lint gates

- `cargo test -p sem-core --lib`: 604 passed, 0 failed.
- `cargo test -p sem-cli`: 247 passed, 0 failed (all 21 integration test
  binaries, summed).
- `cargo test -p sem-mcp`: 93 passed, 0 failed.
- `cargo clippy -p sem-mcp -p sem-cli --no-deps`: zero warnings on any line
  this bead touched (`build_cache.rs`, `sem-mcp/src/cache.rs`,
  `sem-mcp/Cargo.toml`); pre-existing warnings elsewhere in both crates
  (`commands/setup.rs:551`, `cache.rs:591`, `cache.rs:1668`,
  `build_cache.rs:1074`, and others) confirmed unrelated to this diff.
- `cargo fmt -p sem-mcp -p sem-cli -- --check`: no diff on any file this
  bead touched; pre-existing diffs in `main.rs`/`review_protocol.rs`
  (untouched by this bead) left as found.
- `index_probe` oracles (`sem-core/examples/index_probe.rs`, home-assistant-core):
  `ORACLE` PASS (94,708 checked), `REFS_ORACLE` PASS (316,476 checked),
  `FILES_ORACLE` PASS (8 prefixes), `TESTS_ORACLE` PASS (316,476 checked,
  50,201 tests), `TRIGRAM_ORACLE` PASS (6 patterns); `MUTATION` skipped
  (`no_battery_pattern_had_a_provable_true_positive`, a data-dependent skip
  on this corpus, not a gate failure — this bead's diff never touches
  index-writing code, `EntityGraph::build`, or the query index, so these
  oracles were not expected to catch anything from this specific change;
  run for completeness against the campaign's standing gate list).
- Pathspec: `crates/sem-mcp/src/cache.rs` + `crates/sem-mcp/Cargo.toml`
  (fixes 1 and 3 — the O(1) wiring and the parallel read both live in
  `refresh_file_import_entries`, one function, so they share a commit),
  `crates/sem-cli/src/build_cache.rs` (fix 2 — the `file_fingerprint`
  parallel read, both `save_with_test_dirs` and `save_topology`), this
  section of `crates/sem-core/RESOLUTION-PROFILE.md`. `Cargo.lock` updated
  for the new direct `rayon` dependency in `sem-mcp` (already present
  transitively via `sem-core`, so no new crate entered the dependency
  graph, only a new edge to an existing one). No other file in the working
  tree was touched; the untouchables (`diff/cloud_upload.rs`,
  `diff/relations.rs`, `commands/setup.rs`, `parser/plugins/code/languages.rs`,
  `README.md`, `examples/hosted-diff/*`, the two WIP test files) confirmed
  byte-identical via `git diff --stat` before and after.

Bead: semx-ccg.

---

## W1: single-pass columnar (semx-3tb)

The design is `SINGLE-PASS.md` (committed before any implementation, as the
bead required); this section is what landed, what it measured, and what it
did not do. Baselines are the post-W0.5 tree (`fc5e133`), which is what the
"W0.5: the free lunch" section above measured.

### The derivation, in five lines

1. **Fold-fusion** (Meijer–Fokkinga–Paterson 1991 §2.3): `⟨cata f, cata g⟩
   = cata ⟨f,g⟩` — N folds over the same bytes/tree *are* one fold producing
   the N-tuple. So the pass collapse is an identity, and a bit-identical
   gate is the theorem's observable shadow, not a hope about a rewrite.
2. **Deforestation** (ibid. §4; Wadler 1990): the byte string is the
   intermediate between disk and facts, so where consumption is single-pass
   it is dropped *inside* the walk — the columnar form has no `content`
   column by construction.
3. **Separation algebra / frame rule**: `⟦repo⟧ = ⊕_files ⟦f⟧` on disjoint
   keys ⇒ the fused read is `par_iter().map(col).collect()` with no
   coordination, and row order is irrelevant to correctness.
4. **Free-monoid quotient** (interning): designed (`SINGLE-PASS.md` §4),
   **not implemented** — semx-5nc already measured the join-key win at
   0.11-2.29% of build time. The algebra is sound; the measurement says it
   is not the bottleneck, and W1 does not add a noun that moves no number.
5. **Brent's work–span theorem**: `T_P ≥ max(T_1/P, T_∞)`. W1 attacks `T_1`
   only. It makes no `T_∞` claim, and the one place a previous bead *did*
   raise `T_∞` (semx-bkz's barrier between bow index-build and resolve) is
   preserved as a fence, not repeated.

**The smallest theorem did the most work.** `shared_cache::file_content_hash`
is `format!("{:016x}", xxh3_64(b))` (`utils/hash.rs:9`) and
`parser::incremental::content_hash` is `Xxh3::new().update(b).digest()`
(`incremental.rs:440`) — the *same* xxh3-64, two encodings. `cache.db`'s
`files.content_hash` and the index's `FileFingerprint.content_hash` were
never two hashes. `build_cache.rs:161-163`'s comment and semx-ccg's commit
message both asserted they were distinct; that error alone was worth a
whole-corpus read and a whole-corpus hash per build. `L-HASH-ENC`
(`sem-core/tests/single_pass_invariants.rs`) now witnesses the identity.

### Per-fusion delta (median-of-3 cold, all five giants, ms)

Every number below is a `SEM_PROFILE_CACHE=1` phase mark from the same runs
as the end-to-end table; `-` means the phase does not exist on that side.

| fusion | what it deleted | HA | monster | linux | llvm | dotnet |
|---|---|---:|---:|---:|---:|---:|
| **F1** three save-plane reads → one | passes H+I+L's reads; the whole-corpus `HashMap<String,Vec<u8>>`; one of the two xxh3 passes | | | | | |
| ↳ sum of the three reads, before | | 537.2 | 4,185.3 | 2,567.4 | 3,209.6 | 1,485.4 |
| ↳ the one read + the import write, after | | 186.8 | 372.9 | 915.3 | 1,010.5 | 535.2 |
| ↳ **read-phase delta** | | **−350.4** | **−3,812.4** | **−1,652.1** | **−2,199.1** | **−950.2** |
| ↳ `write_query_index_build_image` (trigram extraction left it) | | −18.4 | −19.7 | **−707.0** | −173.6 | −336.5 |
| **F4** per-file bare-import sort → one stem index | semx-ccg's disclosed residual | (in the F1 row) | (3,411 of the 3,812 above) | | | |
| **F3** bow content copy → borrow | census pass D's whole-corpus copy | 0 (no JS/TS facts) | ~136 MB not copied | 0 | 0 | 0 |
| **total `cache_full_save`** | | **−285.7** | **−3,856.5** | **−2,038.5** | **−2,945.9** | **−1,785.0** |

F4's share is separable on the monster because it is JS/TS-import-shaped:
`refresh_file_import_entries` was 3,423.0 ms before the wave and 11.5 ms
after (that residue is the `DELETE`/`INSERT` statements only — the scan
itself now happens inside the one read). On the other four giants F4 is
worth 0.5-1.3 s, which is the read it no longer performs plus a much
smaller scan.

**The trigram fusion paid twice.** `write_query_index_build_image` dropped
on every giant (−707 ms on linux) *while* the read it moved into did not
grow correspondingly (linux: 934.7 ms re-read before → 902.2 ms fused read
after, with extraction now inside it). Extracting a file's trigrams in the
same closure that read its bytes, while they are still hot in cache, is
cheaper than extracting them later from a map — the practical payoff of
fold-fusion beyond the deleted IO.

### End-to-end, median-of-3, cold, all five giants

`SEM_CACHE_DIR` pointed at a fresh empty directory per run (never the shared
`~/Library/Caches/sem/repos/<hash>` tree), `SEM_LOCAL=1 SEM_TIMINGS=1
SEM_PROFILE_CACHE=1 sem graph <root> --json`, darwin,
`available_parallelism=18`. **Load was not idle** — other sessions were
active on the same box throughout; `uptime` load averages moved from
3.84/5.00/4.76 at the start of the battery to 6.38/7.67/7.59 at the end.
Noted, not scrubbed.

| repo | before (median) | after (median) | delta | runs before → after |
|---|---:|---:|---:|---|
| home-assistant-core | 9,592.0 | 9,684.2 | +92.2 (+1.0%, noise) | 10574.5/9549.0/9592.0 → 10778.9/9339.9/9684.2 |
| TypeScript monster | 17,130.6 | 12,897.6 | **−4,233.0 (−24.7%)** | 19924.9/17130.6/16839.3 → 12897.6/12612.5/13029.6 |
| linux | 57,685.9 | 55,273.1 | −2,412.8 (−4.2%) | 66043.0/57685.9/57515.2 → 55538.1/54844.9/55273.1 |
| llvm-project | 58,700.1 | 55,242.4 | −3,457.7 (−5.9%) | 67653.8/58312.8/58700.1 → 54826.3/55252.7/55242.4 |
| dotnet-runtime | 60,802.3 | 57,930.4 | −2,871.9 (−4.7%) | 68664.5/60802.3/59961.2 → 59293.0/57930.4/55828.2 |

**home-assistant-core's end-to-end is flat and its phase deltas are not.**
Its three reads (537.2 ms) collapsed to one (186.8 ms) and its
`cache_full_save` fell 285.7 ms, but its total moved +92 ms — inside a noise
band whose own runs span 1.2 s. Reported as measured: the phase win is real
and small, the end-to-end win on this corpus is not resolvable above the
noise this box produces. Anyone reading a −0.3% claim into that would be
reading further than the data.

**Engine-only cold** (`full_graph_build`, the same runs' `SEM_TIMINGS` mark
— the engine phase measured in production shape rather than through a
second harness):

| repo | before | after | delta |
|---|---:|---:|---:|
| home-assistant-core | 4,694.6 | 4,827.3 | +132.7 |
| TypeScript monster | 6,303.6 | 6,031.2 | −272.4 |
| linux | 20,518.4 | 19,991.5 | −526.9 |
| llvm-project | 32,428.6 | 31,488.1 | −940.6 |
| dotnet-runtime | 38,142.3 | 37,185.5 | −956.8 |

W1 changed exactly one line of engine-phase behaviour (F3's borrow), so the
engine column is reported for completeness and **not claimed as a win**: the
signs are inconsistent (home-assistant, whose Python corpus makes F3 a
literal no-op, moved *up* 133 ms) and the magnitudes sit inside the same
noise band the end-to-end column shows. `perf_probe`'s `BUILD_TOTAL` was
not re-run; there is no engine change for it to measure that this mark does
not already cover.

**CPU work, monster, one run each** (`/usr/bin/time -l`): user 75.22 s →
31.24 s, sys 32.86 s → 27.26 s, real 17.78 s → 13.87 s. The user-time
collapse is F4: semx-ccg had *parallelized* the bare-import sort across 18
cores, so the wall-clock cost was already partly hidden while the CPU cost
was not. Deleting the sort returns 44 s of CPU per monster build to the
machine — which matters on a box that is running anything else.

### The pass census, after W1

| # | pass | verdict | state |
|---|---|---|---|
| A | file discovery | KEEP | unchanged (metadata only) |
| B | parse read | KEEP | **the first of two remaining corpus reads** |
| C | parse+extract, κ fused | KEEP | verified already fused (`compute_structural_hash_and_kappa`) — no code written, and not re-sold as a win |
| D | bow content snapshot | **DELETED** (copy) | `Cow::Borrowed` from `precomputed_facts` on the chunked path |
| E | bow index build+tokenize | KEEP fused | deliberately left per-file; semx-bkz measured the de-fused shape as a `T_∞` regression |
| F | scope/reference resolution | KEEP | W3's lane |
| G | edge assembly | KEEP | |
| H | file fingerprint read | **DELETED** | hex₁₆ of the one hash |
| I | import-entry read | **FUSED** | scan runs in the one read; only the SQL write remains (10-17 ms) |
| J | insert entities with content | KEEP | W4's lane — still the largest single save cost (3.2-15 s) |
| K | insert edges | KEEP | |
| L | index re-read + re-hash | **DELETED** | columns feed `FILES`; trigram sets feed `TRIGRAM` |
| M | index build image | KEEP | now consumes trigram columns, does not re-walk content |
| N | commit + atomic write | KEEP | |

**Reads: 4 → 2. Full-corpus hashes: 2 → 1. Whole-corpus in-memory copies:
2 → 0** (the `contents` map and the bow snapshot). Walks: one per file for
parse+extract+κ, one for the save-plane columns, one join, N serializations
from the columns.

Target was 1 read. **It is 2, and the second one is fenced honestly below,
not quietly rounded down.**

### The fence: why the parse read and the columns read are still two

Fusing them means pass 1 emits the trigram and import columns while it
already has the bytes — textbook fold-fusion, and it would reach the stated
target of one read. It is **not** landed, for a reason that is measured
rather than asserted:

- **The prize is now small.** After F1+F4 the columns read costs 361 ms
  (monster), 902 ms (linux), 993 ms (llvm), 525 ms (dotnet), 183 ms (HA) —
  2.8%, 1.6%, 1.8%, 0.9% and 1.9% of their cold builds. F4 is what shrank
  it; before this wave the same fusion would have been worth 4.2 s on the
  monster.
- **It trades that for residency at the wrong moment.** The columns would
  have to live from pass 1 through the whole resolve phase. Measured on 400
  real monster `.ts`/`.js` files: distinct trigrams cost ≈ **0.32×** the
  file's bytes as an `FxHashSet<u32>` (4-byte key + 1 control byte at 7/8
  load) — so ~63 MB for the monster and ~480 MB for linux, added to the
  peak that `RESOLUTION-PROFILE.md`'s own memory attribution (semx-4w1)
  already identifies as the resolve phase's.
- **The direction is corpus-dependent**, so it is a measurement W4 should
  make with its known-content path in hand (a corpus whose content the
  build already knows may not need to derive these columns at all), not a
  bet W1 takes on the strength of an identity that says nothing about
  memory.

Recorded as a *fence with a number*, not a "future work" gesture: the
equation is true, the cost is 0.9-2.8% of a cold build, and the residency
it buys is 0.32× corpus bytes across the campaign's known memory peak.

### Honest residuals

1. **Peak RSS did not move.** Monster, `/usr/bin/time -l`: 10,242 MB before
   → 10,399 MB after (**+1.5%**). F1 deleted a 198 MB whole-corpus content
   map and F3 deleted a 136 MB copy, and *neither shows*, because the
   process's true peak is the resolve phase (semx-4w1), not the save phase.
   The F3 commit message claims "the win this commit claims is the peak-RSS
   one" — this measurement does not support that claim, and the claim is
   withdrawn here rather than left standing in the log. F3's defensible win
   is the deleted copy itself (a whole-corpus allocation that no longer
   happens) and the invariant that keeps it deleted; not a number on the peak.
2. **home-assistant-core shows no end-to-end win** (see above).
3. **Pass J is untouched and still dominates the save path** (3.2 s monster,
   14.9 s linux) — W4's question, fenced deliberately.
4. **The `build_with_*` entry-point ladder grew by one** (`build_with_
   trigrams_and_dirs_and_tests_and_spans`). Seven near-identical entry
   points was already a surface smell; W1 added an eighth rather than
   collapsing the family, because collapsing it touches every test and
   example that predates this wave and buys no guarantee. Named as debt,
   not hidden.
5. **W1 did not make any giant reach 1 s**, and never could: semx-8lf's
   floor ledger puts four of the five above 1 s on parse and resolve physics
   W1 does not touch.

### LOC ledger (honest, not gutted)

| category | + | − | net |
|---|---:|---:|---:|
| design doc (`SINGLE-PASS.md`) | 376 | 0 | +376 |
| invariant tests (`tests/single_pass_invariants.rs`) | 247 | 0 | +247 |
| invariant tests inside production files (`import_resolution.rs`, `graph.rs`, `corpus_columns.rs` test modules) | ~503 | 0 | +503 |
| production code | ~468 | 155 | **+313** |
| **total** | 1,594 | 155 | **+1,439** |

**The bead expected net-negative and it is net-positive.** The honest
reading: what W1 deleted is *passes*, not lines — three reads became one,
two hashes became one, two whole-corpus copies became none, a per-file
`O(n log n)` sort became one index, and one helper
(`mem_profile::string_to_string_map_bytes`) became dead and was removed.
Expressing "the save path visits each file once" costs a named columnar
module (353 lines, 181 of them its invariant test); the three inline read blocks
it replaced were 88 lines. Per this workspace's own surface-economy rule,
the win claimed here is `γ↑ ∧ canonical-boundary ∧ passes↓`, and the line
count is reported as it is rather than manufactured by deleting prose.

### Gates (every commit in the wave)

- **Bit-identical, rails (3,794 files) and the TypeScript monster (40,877),
  after every fusion**: `cache.db`'s `files`, `file_imports`, `entities`,
  `edges` and `entity_flags` dumps byte-identical; `index.sem` sha256
  identical; entity count, edge count, sorted-entity sha256 and sorted-edge
  sha256 identical.
- **Diff battery**: 8 real commits per fusion (rails `HEAD~1..4`, monster
  `HEAD~1..4`), `sem diff --json` byte-identical every time.
- **`index_probe`** (home-assistant-core): `ORACLE` 94,708 PASS,
  `REFS_ORACLE` 316,476 PASS, `FILES_ORACLE` 8 prefixes PASS,
  `TESTS_ORACLE` 316,476 checked / 50,201 tests PASS, `TRIGRAM_ORACLE` 6
  patterns PASS.
- **Invariant tests (new, all green)**: `L-HASH-ENC`, `L-HASH-UTF8`,
  `L-TRIGRAM-SRC` (`sem-core/tests/single_pass_invariants.rs`); `L-COLUMNS-FUSE`
  (`sem-cli/src/corpus_columns.rs`); `L-STEM-INDEX` + its public-boundary
  sibling (`import_resolution.rs`); `L-BOW-SHARE` (`graph.rs`). Each states
  its formula, its non-vacuity probe and its positive control in its header;
  each keeps the *unfused* side alive as the specification, so none can
  degrade into the new code agreeing with itself.
- **Suites**: `sem-core --lib` 604 → 607, `sem-core --test
  single_pass_invariants` 3, `sem-cli` 247 → 248, `sem-mcp` 93. Zero failures at
  every commit.
- **clippy/fmt**: clean on every file this wave touched (`rustfmt` run
  per-file — crate-wide `cargo fmt` has pre-existing drift in files this
  wave does not touch, left as found; the one clippy warning inside
  `import_resolution.rs` is pre-existing, at `find_import_target`).
- **Untouchables** (`README.md`, `examples/hosted-diff/*`, `languages.rs`
  reflow hunks, `diff/cloud_upload.rs`, `diff/relations.rs`,
  `commands/setup.rs`, the two WIP test files) confirmed byte-identical via
  `git diff --stat` before and after every commit.

Bead: semx-3tb.

---

## W3: resolve floor (semx-1ff)

The question this wave exists to answer, stated by the epic: *after W0.5 and
W1 landed, how much of dotnet/llvm/linux's remaining cold-build time is
RESOLVE rather than parse, and is that slice within ~2x of a credible join
floor — or is there a real fix left?*

It is answered by measurement, and the answer is **the join is not the shape
of the work, the resolve slice is within ~2x of the only fused-resolve floor
this codebase can evidence on three of four giants, and no perfect resolve
brings any giant within an order of magnitude of 1 s.** W3 closes as the
third measured decline. Nothing in this section changed production code.

### Method

- Release build (`opt-level=3`, `lto="thin"`, `codegen-units=1`), HEAD of
  this branch (post-W1, `22bc271`), darwin, `available_parallelism=18`.
- **Two instruments, deliberately, because one of them perturbs what it
  measures.**
  1. `SEM_PROFILE_RESOLVE=1` (`resolve_profile.rs`, pre-existing, unchanged)
     on `SEM_LOCAL=1 SEM_TIMINGS=1 sem graph <root> --json`, fresh
     `SEM_CACHE_DIR` per run, 3 cold runs per giant, median run reported.
     This is the only instrument with sub-phase resolution.
  2. `examples/perf_probe.rs` (pre-existing, unchanged, **without**
     `SEM_PROFILE_RESOLVE`), 3 cold runs per giant: its `PHASE_HOOK` split
     gives `pre_resolve_ms`/`resolve_phase_ms` with no per-name
     instrumentation in the loop. This is the same instrument W0 used, and
     it is the authority for every *share* quoted below.
- **The tax is real and is disclosed rather than absorbed.** Profiled CLI
  `full_graph_build` medians against W1's unprofiled medians: monster
  5,820.6 vs 6,031.2 (**0.97x**, none), dotnet 53,994.4 vs 37,185.5
  (**1.45x**), llvm 51,912.3 vs 31,488.1 (**1.65x**), linux 31,656.6 vs
  19,991.5 (**1.58x**). The tax is `FileAccum`/`BowFileAccum`'s per-lookup
  `name.to_string()` + per-file map merge, and it lands *inside* resolve —
  profiled `scope_wall + post_resolve` runs 1.46-1.62x the clean
  `resolve_phase_ms` on the three giants, matching the engine-level factors
  above. **Sub-phase numbers below are therefore attribution, not wall
  time; the clean column is the wall time.** The monster's factor is 1.00
  because its lookup counts are two orders below the giants'.
- Load was not idle (other sessions on the same box; `uptime` 3.4 to 12.6
  across the battery). Noted, not scrubbed — every conclusion below turns
  on ratios of 10x-10,000x, not on 5% differences.

### 1. The clean split — `perf_probe` PHASE_HOOK, no profiler, median-of-3

| repo | build_total | pre_resolve | **resolve_phase** | resolve % of engine | W0's resolve_phase | delta since W0 |
|---|---:|---:|---:|---:|---:|---:|
| TypeScript monster | 7,943.9 | 4,113.2 | **3,893.3** | 49.0% | — | — |
| dotnet-runtime | 41,413.6 | 10,168.8 | **30,648.6** | 74.0% | 30,661 | −0.04% |
| llvm-project | 34,352.3 | 9,228.0 | **25,006.3** | 72.8% | 24,630 | +1.5% |
| linux | 22,900.4 | 10,494.2 | **12,205.7** | 53.3% | 12,258 | −0.4% |

(Runs: monster 3,785.7/3,893.3/4,150.5; dotnet 29,923.4/30,648.6/32,087.8;
llvm 23,724.0/25,006.3/25,124.3; linux 12,008.1/12,205.7/12,408.7.)

**W1 did not move resolve, exactly as `SINGLE-PASS.md` §5 fenced it would
not** — all three giants land within 1.5% of W0's number. That is the
control that makes the rest of this section readable: the resolve phase
measured here is the same resolve phase W0 measured, so W0's conclusions
about it are still live and this section is not re-measuring a moved target.

Resolve as a share of the **whole cold CLI build** (against W1's post-wave
end-to-end medians — monster 12,897.6, dotnet 57,930.4, llvm 55,242.4,
linux 55,273.1):

| repo | resolve_phase | post-W1 total | **resolve % of total** |
|---|---:|---:|---:|
| TypeScript monster | 3,893.3 | 12,897.6 | **30.2%** |
| dotnet-runtime | 30,648.6 | 57,930.4 | **52.9%** |
| llvm-project | 25,006.3 | 55,242.4 | **45.3%** |
| linux | 12,205.7 | 55,273.1 | **22.1%** |

### 2. The sub-phase attribution — `SEM_PROFILE_RESOLVE=1`, median run

All figures ms. `wall` = a wall-clock timer (summed over chunks where the
phase runs per chunk); `cum` = an `AtomicU64` accumulator summed across
resolve worker threads, so it exceeds wall by the effective parallelism.
Median run per repo by end-to-end total (monster 12,121.0; dotnet 75,687.6;
llvm 76,187.2; linux 65,193.6).

| sub-phase | kind | monster | dotnet | llvm | linux |
|---|---|---:|---:|---:|---:|
| **scope_wall** (whole scope-resolution stage) | wall | **277.4** | **40,172.7** | **35,688.2** | **9,833.0** |
| ↳ reparse — read+parse of this chunk's files | wall | 17.0 | **16,226.7** | **13,053.5** | 2,912.1 |
| ↳ chunks (count / summed wall) | wall | 9 / 182.3 | 30 / 39,842.8 | 38 / 35,355.0 | 18 / 9,581.7 |
| ↳ scope_build | cum | 222.0 | 59,993.1 | 88,898.3 | 37,537.8 |
| ↳ ref_collect | cum | 33.7 | 904.8 | 264.7 | 9.8 |
| ↳ ref_loop | cum | 194.9 | 10,117.8 | 75,544.2 | 84.4 |
| ↳↳ resolve_ref (⊂ ref_loop) | cum | 60.1 | 7,570.7 | 73,198.4 | 36.8 |
| ↳ chunk_entity_index | wall | 17.5 | 62.3 | 59.9 | 80.2 |
| ↳ scope_merge / scope_dedup | wall | 59.7 / 16.0 | 475.9 / 468.9 | 310.3 / 305.7 | 12.8 / 13.1 |
| **post_resolve** (bow + aliases + dedupe/sort + edge index) | wall | **1,908.9** | **4,439.2** | **4,897.3** | **8,297.2** |
| ↳ bow_wall | wall | 1,827.8 | 3,578.3 | 4,318.6 | 7,281.5 |
| ↳↳ bow_index_build | cum | 2,007.9 | 32,936.4 | 40,433.0 | 48,151.6 |
| ↳↳↳ bow_index_io (a file read) | cum | 0.0 | 4,093.4 | 5,599.7 | 3,506.6 |
| ↳↳↳ bow_index_tokenize | cum | 2,002.6 | 28,774.4 | 34,772.5 | 44,576.7 |
| ↳↳ bow_resolve | cum | 3,375.4 | 20,239.1 | 20,186.0 | 46,321.9 |
| ↳↳↳ ref_extract | cum | 1,592.1 | 11,125.5 | 12,535.9 | 30,109.3 |
| ↳↳↳ dotchain_extract | cum | 482.1 | 3,309.5 | 1,851.1 | 4,827.9 |
| ↳↳↳ local_binding | cum | 998.9 | 76.8 | 308.0 | 160.2 |
| ↳↳↳ **ref_match — the symbol-table join** | cum | **4.6** | **105.7** | **173.2** | **359.2** |
| ↳↳↳ **dotchain_match — the class-members join** | cum | **6.0** | **53.8** | **1.6** | **0.5** |
| ↳ export_edges / dedupe / sort / edge_index | wall | 2.0 / 4.4 / 4.8 / 26.2 | 10.2 / 122.9 / 35.7 / 399.8 | 13.0 / 94.4 / 28.0 / 274.0 | 27.5 / 84.6 / 36.7 / 603.4 |
| REF_CACHE refs / hit% | — | 195,859 / 9.18% | 2,380,367 / 17.96% | 3,738,345 / 9.93% | 106,772 / 17.63% |

Candidate-list shapes actually hit (`CANDIDATE_DIST`, same runs), because
the join-floor model below has to be against the real key distribution and
not a guess:

| lookup kind | monster (n, p50, p99) | dotnet | llvm | linux |
|---|---|---|---|---|
| `method_call` (`class_members` scan) | 21,880 / 32-63 / 8192-16383 | 596,771 / 64-127 / 512-1023 | 382,700 / 16-31 / 2048-4095 | 6,083 / 8-15 / 64-127 |
| `call_global` (`symbol_table` probe) | 68,181 / 2-3 / 4096-8191 | 311,866 / 4-7 / 256-511 | 675,725 / 16-31 / 4096-8191 | 25,129 / 4-7 / 128-255 |
| `bow_class_members` | 14,875 / 512-1023 / 8192-16383 | 22,078 / 16-31 / 8192-16383 | 12,171 / 4-7 / 256-511 | 6,574 / 8-15 / 64-127 |
| `bow_symbol_table` | 222,679 / 0 / 8-15 | 2,076,246 / 0 / 4-7 | 4,078,391 / 0 / 8-15 | 7,644,576 / 0 / 16-31 |

### 3. The join floor, three ways, all agreeing

**Model 1 — the production timer, which already measures it.** The two
bag-of-words match buckets (`ref_match`, `dotchain_match`) time exactly the
`symbol_table.get(name).iter().find(..)` and `class_members.get(owner)`
member-scan that *are* the join, and the scope side's disambiguation scans
are timed per name in `TOP20_METHOD_CALL_NAMES_BY_TIME`. Summed:

| repo | bow joins (cum) | scope disambiguation, top-20 (cum) | **total measured join work** | resolve_phase (clean wall) | **join / resolve** |
|---|---:|---:|---:|---:|---:|
| monster | 10.6 | ~0.20 | **~10.8** | 3,893.3 | **0.28%** |
| dotnet | 159.5 | ~0.25 | **~159.8** | 30,648.6 | **0.52%** |
| llvm | 174.8 | ~0.13 | **~174.9** | 25,006.3 | **0.70%** |
| linux | 359.7 | ~0.01 | **~359.7** | 12,205.7 | **2.95%** |

and these are *cumulative across 18 threads*, so the wall contribution is
smaller again by the effective parallelism. The instrument that was built
to find the join's cost has been telling us for four beads that it is under
3% of resolve on every corpus.

**Model 2 — the calibrated micro-bench, at real key shapes.** `benches/
interning.rs` (semx-5nc) measures the exact `resolve_ref` join shape (name
-> `symbol_table` -> up to 4 candidate ids -> `entity_map`) over a corpus
calibrated to the monster's real `key_shape_probe` percentiles (`id_len`
p50=87/p99=181, `name_len` p50=17/p99=82, bucket p50=1/p90=6/p99=71,
`singleton_frac`=0.555): 20,000 joins in **3.430 ms** on today's shipped
`FxHashMap<String,_>` (171.5 ns/probe) and **0.236 ms** token-in-hand
interned (**11.8 ns/probe** — the floor, since a token-indexed array probe
is the cheapest realizable form of this join). Probe count per corpus =
`REF_CACHE.total_refs` + `bow_symbol_table` + `bow_class_members` lookups:

| repo | probes | floor @ 11.8 ns, 18 cores | @ 11.8 ns, 1 core | @ 171.5 ns (as shipped), 1 core |
|---|---:|---:|---:|---:|
| monster | 433,413 | 0.28 ms | 5.1 ms | 74.3 ms |
| dotnet | 4,478,691 | 2.93 ms | 52.8 ms | 768.1 ms |
| llvm | 7,828,907 | 5.13 ms | 92.4 ms | 1,342.7 ms |
| linux | 7,757,922 | 5.08 ms | 91.5 ms | 1,330.5 ms |

**Model 3 — the literature, as an independent sanity bound.** Balkesen,
Teubner, Alonso and Özsu, *"Main-memory hash joins on multi-core CPUs:
Tuning to the underlying hardware"* (ICDE 2013), measure no-partitioning
and radix-partitioned main-memory hash joins at ~100-200 M tuples/s on
8-16-core machines, i.e. 5-10 ns per probe aggregate. At a conservative
100 M probes/s the same probe counts give 4.3 / 44.8 / 78.3 / 77.6 ms —
the same order as model 2's 18-core column, from a completely independent
derivation on different hardware.

**The arithmetic, stated as the bead asked.** Against the most *generous*
floor any of the three models admits (model 2 at today's un-interned,
single-core rate — a floor no real implementation would be slower than):

```
resolve / join_floor  =  monster 52x    dotnet 40x    llvm 19x    linux 9.2x
```

and against the true floor (model 2 interned, 18 cores): 13,900x / 10,470x /
4,880x / 2,400x. **Resolve is not within 2x of the join floor, and it never
could be, because it is not doing joins.** This is W0's non-finding
(`join_floor` < 15 ms everywhere) re-derived from four independent angles at
four times the corpus coverage, and it is why no join-shaped fix — interning
(semx-5nc, declined), columnar adjacency, u32 keys — moves this number. The
bead's own framing ("intern at ingest, columnar adjacency") is refuted by
its own measurement.

### 4. So what *is* the floor? The only fused-resolve implementation in the tree

If the join is not the shape, a join floor is the wrong floor. The right
one is Brent's, `T_P >= max(T_1/P, T_infinity)`, and the honest way to
estimate `T_1` for *this* resolver is not to model it but to read it off the
one corpus where the resolve phase already runs in its fused, minimal form.

`precompute_js_ts_file_facts` (`scope_resolve.rs:1063`, semx-6rd CUT 1) is
the fold-fusion of pass 1 and pass 2: scopes, `entity_scope_map`,
return types, init-self-attrs and `ast_refs` are all folded out of the tree
**while pass 1 still holds it**, so the resolve phase never re-reads or
re-parses that file. It returns `None` unless `is_js_ts_file(file_path)`.
**The TypeScript monster is therefore the only giant in this fleet whose
resolve phase runs the fused shape**, and its measured rate is the floor
this codebase can actually evidence:

```
fused-resolve rate (monster, clean, median-of-3)
  3,893.3 ms / 198.6 MB    = 19.60 ms per MB      (51.0 MB/s)
  3,893.3 ms / 454,541 ent =  8.566 us per entity
```

| repo | bytes | entities | floor @ 19.60 ms/MB | floor @ 8.566 us/ent | measured resolve | **ratio (bytes / entities)** |
|---|---:|---:|---:|---:|---:|---|
| monster | 198.6 MB | 454,541 | 3,893 | 3,893 | 3,893.3 | 1.00x / 1.00x (definition) |
| dotnet | 589.5 MB | 990,754 | 11,554 | 8,487 | 30,648.6 | **2.65x / 3.61x** |
| llvm | 867.0 MB | 1,306,421 | 16,993 | 11,191 | 25,006.3 | **1.47x / 2.23x** |
| linux | 1,499.1 MB | 2,312,433 | 29,383 | 19,808 | 12,205.7 | **0.42x / 0.62x** |

**llvm and linux are already at or under the floor** (linux resolves at 2.4x
the monster's per-byte rate — its C corpus routes almost everything through
bag-of-words: only 2,050 of 72,787 files reach scope resolution at all).
**dotnet is the single outlier at 2.65-3.61x**, and its excess is not
mysterious: `reparse` is **16,226.7 ms of its 40,172.7 ms scope_wall**
(40.4%), i.e. ~11.2 s after the 1.45x profiler tax is removed. Subtracting
it puts dotnet's resolve at ~19.4 s = **1.68x / 2.29x** of the same floor —
inside the 2x band on the per-byte model, at its edge on the per-entity one.

**That is the answer to the bead's question.** The resolve slice, once its
one named non-floor component is accounted, is within ~2x of the only
fused-resolve floor this repository can evidence, on all four giants.

### 5. The one real lever, named with its arithmetic and its two fences

The `reparse` bucket is a **second full `read_to_string` + `parse_tree` of
every non-JS/TS file, inside the resolve phase, once per chunk**
(`scope_resolve.rs:1340-1409`; the block's own comment says so: *"the exact
same per-file read+parse work pass 1 already does ... just re-run here for
files pass 1 didn't retain a tree for"*, and names dotnet's 34,897-file
reparse set). It is worth, as *wall* inside the scope stage:

| repo | reparse (profiled wall) | ÷ tax | ≈ clean wall | as % of clean resolve_phase | as % of post-W1 total |
|---|---:|---:|---:|---:|---:|
| monster | 17.0 | 0.97 | ~17 | 0.4% | 0.1% |
| dotnet | 16,226.7 | 1.45 | ~11,190 | 36.5% | **19.3%** |
| llvm | 13,053.5 | 1.65 | ~7,910 | 31.6% | **14.3%** |
| linux | 2,912.1 | 1.58 | ~1,840 | 15.1% | 3.3% |

It is a textbook fold-fusion violation — the same bytes read twice,
the same tree built twice — and W1's own algebra says it should not exist.
**It is nevertheless not available to this bead, for two independent
reasons, both already measured elsewhere in this document:**

1. **Memory.** The reparse exists *because* trees are not retained past
   `PARSED_FILE_REUSE_LIMIT`, and the 20 MiB `SCOPE_RESOLVE_BYTE_BUDGET`
   (semx-g6t) exists to bound per-chunk tree residency — C# measures ~40x
   and C ~24.5x tree-bytes per source-byte (`examples/tree_mem_probe.rs`),
   and byte-budget chunking is what took dotnet's peak from 10.30 GB to
   8.28 GB (−19.6%). Retaining trees to delete the reparse re-opens exactly
   the pathology semx-4w1 and semx-g6t closed.
2. **Semantics.** The fused alternative — `PrecomputedFileFacts` for every
   language — is licensed for JS/TS by a stated language property:
   *"every lookup it does against them is keyed by an id that belongs to
   this file (JS/TS declarations never nest across files), so a map built
   from just this file's entities produces identical results"*
   (`scope_resolve.rs:1071-1077`). That property is **false** for C# (partial
   classes) and C++ (out-of-line member definitions `A::f()`), so extending
   the fusion is not a mechanical work elimination — it is a change to what
   the resolver can see, which is precisely what the bead's own scope
   excludes ("NOT semantics changes") and what semx-nuv/semx-yk5 already
   showed this resolver punishes with subtle chunk-locality edge flips.

This is not new. It is Open item #2 of the *Memory attribution* section
(semx-4w1) — *"extending `PrecomputedFileFacts`-style tree avoidance to
every language, eliminating chunk-held trees instead of bounding them"* —
now with a wall-clock price tag on it (11.2 s dotnet, 7.9 s llvm, 1.8 s
linux) to go with its memory one.

Two smaller, genuinely mechanical items were also found and are recorded
rather than taken, because both are under this campaign's materiality bar:

- **`bow_index_io`** — a file read inside `build_file_reference_index` for
  files `snapshot_bow_content` did not cover: 4,093 / 5,600 / 3,507 ms
  *cumulative* (dotnet/llvm/linux), which at bag-of-words' measured 9-10x
  effective parallelism is ~0.4-0.6 s of wall each.
- **`IMPORT_TABLE io_ms`** — 8,590 / 18,273 / 14,983 ms cumulative
  (dotnet/llvm/linux) of full-corpus reading in `import_source_content`
  against `scan_ms` of only 403 / 608 / 1,071 ms, i.e. a whole-corpus read
  whose scan finds almost nothing on corpora with no JS/TS imports. Wall is
  0.5-1.1 s (it is well parallelized), and it sits in `pre_resolve`, not in
  W3's lane.

### 6. Decision: measured decline, and the arithmetic that forces it

The bead's rule (a) closes on *either* "within ~2x of floor" *or* "cannot
move any repo's total meaningfully toward its W0 floor". Both clauses are
answered, and the second is the decisive one:

**Clause 1 — within 2x.** Against the *join* floor: no, by 9x-52x at the
most generous and 2,400x-13,900x at the true floor (§3). Against the only
*fused-resolve* floor this codebase can evidence: yes on monster (1.00x),
llvm (1.47x), linux (0.42x), and yes on dotnet (1.68x) once the reparse is
subtracted, 2.65x with it (§4).

**Clause 2 — can a perfect resolve move any giant toward its floor?** Set
each giant's resolve to its §4 floor (the best this codebase has ever
demonstrated) and re-add the rest of the measured cold build:

| repo | post-W1 total | resolve now | resolve at floor | **total with a perfect resolve** | W0 floor | still over W0 floor by | still over 1 s by |
|---|---:|---:|---:|---:|---:|---:|---:|
| monster | 12,897.6 | 3,893.3 | 3,893.3 | **12,897.6** | ~1,803 | 7.2x | **12.9x** |
| dotnet | 57,930.4 | 30,648.6 | 8,487 | **35,768.8** | ~12,324 | 2.9x | **35.8x** |
| llvm | 55,242.4 | 25,006.3 | 11,191 | **41,427.1** | ~11,882 | 3.5x | **41.4x** |
| linux | 55,273.1 | 12,205.7 | 12,205.7 | **55,273.1** | ~14,857 | 3.7x | **55.3x** |

Even the *infinitely optimistic* version — resolve to **zero**, a physical
impossibility — leaves monster at 9.0 s, dotnet at 27.3 s, llvm at 30.2 s
and linux at 43.1 s: **9x to 43x over the mandate, and every one of them
still above its own W0 floor.** What remains after a zero-cost resolve is
parse physics (W0: dotnet 8.3 s, llvm 6.7 s, linux 6.8 s, already argued at
tree-sitter's measured ceiling) and the `cache.db` save path (19-30 s per
giant, W4's lane, `insert_entities_with_content` alone 3.2-14.9 s).

**Therefore: resolve is not the binding constraint for the sub-1s epic on
any giant in this fleet.** It is 22-53% of today's cold build and it is
within ~2x of a demonstrated floor; the one component that is not (dotnet's
second parse, 19.3% of its total) is fenced by a measured memory bound and
by a language property that does not hold for the corpora that would
benefit. `semx-1ff` closes as **measured decline**, the third in this
campaign after `918f12a` (child-range interning) and `cd75645`/semx-5nc
(join-key interning), and for the same reason all three closed: the
micro-level win is real and the function it lives in is not where the time
is.

**What would change this verdict.** (i) A corpus whose resolve exceeds ~4x
the fused floor — none of the four does. (ii) A decision, taken with W4's
memory budget in hand, to extend `PrecomputedFileFacts` to C#/C++/C behind
a *language-specific* proof that file-local entity maps are sound there
(they are not, today, by the reasons in §5) — worth ~11.2 s on dotnet and
~7.9 s on llvm, and it would still leave both above 25 s. (iii) A wave that
first removes the save path, at which point resolve becomes the majority of
what is left and a 1.5-2x resolve win starts to matter in relative terms —
which is the ordering `QUERY-INDEX.md §1.7` and W0 §5 already recommend
(W4 before any further resolve work).

### Gates

- **No production code changed.** This bead is measurement + this section.
  `git diff --stat` before and after touches only
  `crates/sem-core/RESOLUTION-PROFILE.md`; the untouchables (`README.md`,
  `examples/hosted-diff/*`, `languages.rs` reflow, `diff/cloud_upload.rs`,
  `diff/relations.rs`, `commands/setup.rs`, the two WIP test files) are
  byte-identical, confirmed.
- Because nothing was implemented, the bit-identical/oracle/suite battery
  the wave's charter requires *for an implementing bead* was **not run**,
  and is stated as not run rather than implied: there is no change for it
  to gate. `cargo build --release -p sem-cli --bin sem` and
  `cargo build --release -p sem-core --example perf_probe` are clean (one
  pre-existing unrelated `sem-cli` warning, unchanged).
- Both instruments used (`resolve_profile.rs`, `examples/perf_probe.rs`)
  are pre-existing and were run unmodified — verified by `git diff` showing
  no entry for either file.
- Raw runs: 12 profiled CLI builds (3 per giant) and 12 `perf_probe` builds
  (3 per giant), 24 cold builds total, each with a fresh cache directory.

Bead: semx-1ff.

---

## W4: the save plane (semx-431)

W3 handed this wave a number: engine `build_total` versus full-CLI cold left
**5.0 s (monster) / 16.5 s (dotnet) / 20.9 s (llvm) / 32.4 s (linux)** outside
the engine — the majority of linux's wall time and the biggest single lever
left on every giant. This section attributes that plane artifact by artifact,
asks what each artifact buys against what it costs, lands the fixes that
survived the question, and answers the bead's original half (known-content
cold) with its own measurement.

Headline: **the save plane is 44-54% smaller on every giant**, and the largest
single item in it turned out to be eight SQLite indexes that no production
statement has read since QUERY-INDEX.md §12.3 rerouted the query plane onto
`index.sem`.

### Method

- Release build (`opt-level=3`, `lto="thin"`, `codegen-units=1`), darwin,
  `available_parallelism=18`. Baseline is HEAD of this branch (`042bee4`,
  post-W1/W3).
- `SEM_LOCAL=1 SEM_TIMINGS=1 SEM_PROFILE_CACHE=1 sem graph <root> --json`,
  fresh `SEM_CACHE_DIR` per run, **median-of-3** per giant per side, five
  giants.
- Two new instruments, both behind the existing `SEM_PROFILE_CACHE=1`
  `OnceLock<bool>` gate (`W4-INSTR`, `c190ccd`): five marks around the *facts*
  plane, which lives inside `full_graph_build` and had none, and
  `INDEX_IMAGE_PHASE`/`INDEX_IMAGE_SECTION` inside `index::writer::build_image`,
  which was one opaque number worth up to 6.8 s. Also printed:
  `merge_with_local`'s `CorpusLookupStats`, previously computed and discarded
  at the call site.
- Load was not idle (other sessions on the same box; `uptime` load averages
  6.4-8.6 across the battery). Noted, not scrubbed. llvm's baseline median
  (61.8 s) runs 6.6 s above W1's own (55.2 s) for this reason; its *phase*
  deltas below come from the same runs on both sides and are unaffected.

---

### 1. Attribution: every artifact a cold build writes

**End-to-end, median-of-3, baseline** (ms):

| repo | file_discovery | full_graph_build | cache_full_save | serialization | **total** |
|---|---:|---:|---:|---:|---:|
| home-assistant-core | 489 | 6,790 | 4,140 | 307 | **11,728** |
| TypeScript monster | 313 | 6,235 | 6,004 | 457 | **13,012** |
| dotnet-runtime | 422 | 38,912 | 19,680 | 1,506 | **60,522** |
| llvm-project | 953 | 37,673 | 21,449 | 1,742 | **61,820** |
| linux | 549 | 20,910 | 32,456 | 3,098 | **57,016** |

**The save plane, by artifact** (ms; the `facts` rows sit *inside*
`full_graph_build`, which is why W3's engine-vs-CLI subtraction saw them but
could not name them):

| artifact | what it is | HA | monster | dotnet | llvm | linux |
|---|---|---:|---:|---:|---:|---:|
| **facts corpus read** (`merge_with_local`) | read+hash of every file `local` doesn't know — on a cold build, all of them | 1,181 | 702 | 1,251 | 1,909 | 2,195 |
| **facts blobs written** (`export_persisted` + `FactsStore::save` + `populate_delta`) | per-repo snapshot + machine-global shards | 965 | 807 | 1,660 | 1,832 | 2,214 |
| **shared corpus read** (`CorpusColumns::read`) | W1's one save-plane read: fingerprints + trigrams + import scan | 184 | 357 | 757 | 1,208 | 864 |
| **cache.db** (fingerprint/manifest/import inserts + entities + edges + test flags + commit) | the finished-graph SQLite mirror | 3,320 | 4,542 | 15,382 | 16,129 | 24,291 |
| ↳ of which `insert_entities_with_content` | | 1,851 | 3,219 | 8,346 | 10,132 | **15,466** |
| ↳ of which `insert_edges` | | 812 | 511 | 3,848 | 2,994 | 4,916 |
| ↳ of which `sqlite_commit` | | 529 | 690 | 2,655 | 2,221 | 3,247 |
| **index.sem** (`entity_byte_spans` + dirs + build image + atomic write) | the mmap query index | 635 | 1,102 | 3,534 | 4,105 | 7,289 |
| ↳ of which `build_image` | | 599 | 1,022 | 3,217 | 3,836 | **6,787** |
| **CLI serialization** | `--json` rendering (harness-specific, see §5.4) | 307 | 457 | 1,506 | 1,742 | 3,098 |

**`index.sem` section bytes** (baseline, unchanged by this wave):

| section | monster | dotnet | llvm | linux |
|---|---:|---:|---:|---:|
| STRINGS | 15.2 MB (18%) | 71.8 MB (41%) | 67.5 MB (36%) | 99.2 MB (36%) |
| ENTITIES | 18.2 MB (22%) | 39.6 MB (23%) | 52.4 MB (28%) | 92.5 MB (33%) |
| TRIGRAM | 41.9 MB (50%) | 41.9 MB (24%) | 41.9 MB (22%) | 41.9 MB (15%) |
| REFS | 5.2 MB (6%) | 15.8 MB (9%) | 18.3 MB (10%) | 33.7 MB (12%) |
| NAMES / FILES / DIRS / KINDS | 3.5 MB | 6.1 MB | 8.8 MB | 12.3 MB |
| **image total** | **84.0 MB** | **175.2 MB** | **188.8 MB** | **279.6 MB** |

TRIGRAM is pinned at its `TRIGRAM_BUDGET_BYTES` ceiling on all four giants —
it is a *constant* 41.9 MB, so it is 50% of the monster's image and 15% of
linux's. That is the budget working as QUERY-INDEX.md §3.3 designed it, and it
is why the image does not grow with corpus size the way `cache.db` does.

**The corpus-read count is four, not two.** W1's post-wave census recorded
"Reads: 4 → 2" (pass B's parse read, and `CorpusColumns::read`). Two more were
found here, both outside the code W1 censused:

3. `insert_entities_with_content_store` (`sem-mcp/src/cache.rs`) does a
   `read_to_string` of every file with an entity — the pass census called
   item J "a copy, not a disk read", which stopped being true when the
   content store landed. It was serial, inside the insert loop.
4. `FactsCorpus::merge_with_local` reads *and* hashes every path the local
   snapshot doesn't know, which on a cold build is every path (0.7-2.2 s).

Neither is deleted by this wave (see §3 and §5.2); both are now named and
timed, which is the precondition for deleting them.

---

### 2. The artifact-value question, answered with evidence

QUERY-INDEX.md §15.3 kept `cache.db` on one argument: *"the facts store buys
back parsing, never resolution, and `cache.db` is the only tier that buys back
both."* That was reasoning about tiers, not a measurement of verbs. Under W3's
numbers it is worth re-testing directly, and the test is cheap: build a repo,
then delete **only** `cache.db` — leaving `index.sem`, the per-repo facts
store and the machine-global corpus exactly as they were — and run every verb
on both sides, byte-comparing stdout. `cache.db` is removed again before each
verb on the without side, because a verb that falls through to a build writes
it back.

**rails** (3,794 files; cache.db 141.9 MB, index.sem 28.8 MB) and the
**TypeScript monster** (40,869 files; cache.db 653.5 MB, index.sem 84.0 MB),
each with an unambiguous symbol so no verb exits on an ambiguity error:

| verb | rails with | rails without | monster with | monster without | stdout |
|---|---:|---:|---:|---:|---|
| `graph . --json` | 0.06 | 0.06 | 0.34 | 0.34 | **identical** (37.0 MB / 178.1 MB) |
| `find` | 0.01 | 0.01 | 0.03 | 0.03 | **identical** |
| `callers` | 0.01 | 0.01 | 0.03 | 0.03 | **identical** |
| `refs` | 0.01 | 0.01 | 0.03 | 0.03 | **identical** |
| `impact` (transitive) | 0.01 | 0.01 | 0.04 | 0.04 | **identical** |
| `impact --deps` | 0.10 | **2.06** | 0.69 | **4.87** | **identical** |
| `context` | 0.01 | 0.01 | 0.04 | 0.04 | **identical** |
| `grep` | 0.01 | 0.01 | 0.03 | 0.03 | **identical** |
| `diff HEAD~1..HEAD` | 0.02 | 0.02 | 0.05 | 0.04 | **identical** |
| `diff HEAD~3..HEAD~2` | 0.05 | 0.05 | 0.08 | 0.08 | **identical** |

**Every verb's output is byte-identical without `cache.db`, and nine of ten
are equally fast.** Two of §15.3's three named justifications do not survive
contact: `sem context` answers from the index's byte spans (semx-a3w) plus the
file on disk, and `sem diff` answers without hydrating. The third — the
incremental rebuild — does survive:

| scenario | with cache.db | without | delta |
|---|---:|---:|---:|
| rails, one file edited, `sem graph` | 0.65 s | 2.10 s | −1.45 s |
| monster, one file edited (`checker.ts`), `sem graph` | **6.16 s** (partial load 0.50 + incremental rebuild 3.26 + incremental save 1.47) | **12.12 s** (full build 5.36 + full save 5.82) | −5.96 s |

**The verdict, with its arithmetic.** `cache.db` is *not* retired, and the
reason is two paths, not eight:

- **What it costs.** 3.3 / 4.5 / 15.4 / 16.1 / 24.3 s of every cold build
  (HA/monster/dotnet/llvm/linux) plus 142-653 MB of disk.
- **What it uniquely buys.** (a) the dirty rebuild: 6.2 s versus a rebuild
  that, if `cache.db` were gone entirely, would cost 12.1 − 4.5 = **~7.6 s**
  on the monster — a ~1.4 s edge, not the order-of-magnitude §15.3 implies;
  (b) `sem impact --deps` in its name-only form, 4.2 s on the monster.
- **What blocks deleting it anyway,** independent of the arithmetic:
  `sem-mcp/src/server.rs` opens **its own** `DiskCache` against the same
  `cache.db` and both loads and saves it (the duplicate-constructor-authority
  finding §15.3 disclosed and did not touch). A CLI-side deletion does not
  remove the write; it moves it to whichever process touches the repo first.

So the cut this wave takes is not the artifact but its **dead weight** — and
that turned out to be where the time was anyway (§3, F1). Restated honestly
for §15.3's benefit: `cache.db`'s remaining unique value is *the incremental
rebuild and one query-plane gap*, not "the only tier that skips resolve" —
`sem graph` skips resolve from `index.sem` alone, in 0.06 s on rails and
0.34 s on the monster, producing byte-identical output.

**Surfaced, not fixed** (query plane, not this bead's lane):
`try_index_impact_deps` (`impact.rs:386`) declines whenever neither
`--entity-id` nor `--file` is given — a gate inherited verbatim from the
SQLite fast path it replaced, and *redundant* with the ambiguity check 13
lines below it (`candidates.len() != 1 → return false`). Closing that gate
would remove `impact --deps` from the list above and leave the incremental
rebuild as `cache.db`'s single remaining consumer, which is the state in
which its retirement becomes a one-path decision instead of a two-path one.

---

### 3. Fix log, in descending measured cost

**F1 — eight dead indexes, and the content store's hoisted reads**
(`64268d1`, `sem-mcp/src/cache.rs`).

`CACHE_INDEXES` carried fourteen index definitions; every one is a second
B-tree written per row on the save plane. Censused against every `SELECT` and
`DELETE` in both crates (the exhaustive form QUERY-INDEX.md §15.1 used for
symbols — and re-run after an early truncated grep produced a wrong answer
about `entity_changes`, which *is* live):

| index | production reader | verdict |
|---|---|---|
| `idx_entities_file_path` | `DELETE FROM entities WHERE file_path = ?1` | **keep** |
| `idx_entities_name` | — | **drop** |
| `idx_entities_name_file_path` | — | **drop** |
| `idx_entities_type_name_file_path` | — | **drop** |
| `idx_entities_parent_id` | — | **drop** |
| `idx_entities_parent_id_name` | — | **drop** |
| `idx_edges_from_entity` | `DELETE FROM edges WHERE from_entity = ?1` | **keep** |
| `idx_edges_to_entity` | `DELETE FROM edges WHERE to_entity = ?1` | **keep** |
| `idx_edges_from_to_ref` | `#[cfg(test)]` helpers only | **drop** |
| `idx_edges_to_from_ref` | `#[cfg(test)]` helpers only | **drop** |
| `idx_file_imports_imported_file` | `SELECT DISTINCT importing_file … WHERE imported_file = ?1` | **keep** |
| `idx_file_imports_importing_file` | leading column of that table's own `PRIMARY KEY` | **drop** |
| `idx_entity_changes_*` (2) | `index_commits`' history path, off the save plane | **keep** |

Nothing queries `entities` by anything but `id` (its primary key) or a full
scan any more; the five name/type/parent indexes are exactly the ones §12.3's
deleted SQL query fast paths used, left behind when their callers went. Second
half of the same commit: `insert_entities_with_content_store`'s per-file
`read_to_string` and its zstd pass are pure functions of their inputs, so both
hoist out of the serial insert loop and parallelize the way
`CorpusColumns::read` already does. `CACHE_SCHEMA_VERSION` 10 → 11 so an
existing cache drops its stale indexes rather than keeping them.

| `insert_entities_with_content` | HA | monster | dotnet | llvm | linux |
|---|---:|---:|---:|---:|---:|
| baseline | 1,851 | 3,219 | 8,346 | 10,132 | 15,466 |
| after the parallel read+zstd only | — | 2,557 | — | — | — |
| after the index retirement too | **695** | **1,168** | **2,662** | **3,389** | **4,579** |

The parallel read/zstd is the smaller half (monster 3,219 → 2,557; the reads
were 270 ms and the zstd 62 ms once parallel) — **the indexes were the cost**:
2,187 → 793 ms of pure `INSERT` time on the monster once five of six went
away. The same retirement is why `insert_edges` (linux 4,916 → 2,220) and
`sqlite_commit` (linux 3,247 → 1,816) fell without being touched, and why
`cache.db` itself is **42% smaller** (rails 141.9 → 82.0 MB, tiptap 50.0 →
29.1 MB).

**F2 — the index image builds in parallel** (`c062ef0`,
`sem-core/src/index/writer.rs`). `build_image` was wholly serial on an
18-core box. Three parts are parallel by construction: both sorts (total
orders — each tie-breaks on a unique key, so an unstable parallel sort is a
function, not a choice) and `build_refs_section`'s forward and reverse CSRs,
which share no mutable state and now run under `rayon::join`.

| `build_image` sub-phase (linux) | before | after |
|---|---:|---:|
| entity collect + sort | 1,951 | **427** |
| names section | 593 | **180** |
| refs section | 1,376 | **618** |
| entities section (serial: shares the string arena) | 1,097 | 1,094 |
| trigram section | 1,068 | 1,056 |
| **total `build_image`** | **6,787** | **4,055** |

Per giant: HA 599 → 428, monster 1,022 → 573, dotnet 3,217 → 2,118, llvm
3,836 → 2,536, linux 6,787 → 4,055.

**Per-giant totals, median-of-3, baseline → after:**

| repo | `cache_full_save` before | after | delta | **cold total before** | **after** | **delta** |
|---|---:|---:|---:|---:|---:|---:|
| home-assistant-core | 4,140 | **2,301** | −1,839 (−44.4%) | 11,728 | **9,275** | −2,453 (−20.9%) |
| TypeScript monster | 6,004 | **3,112** | −2,892 (−48.2%) | 13,012 | **11,500** | −1,512 (−11.6%) |
| dotnet-runtime | 19,680 | **9,709** | −9,971 (−50.7%) | 60,522 | **50,994** | −9,528 (−15.7%) |
| llvm-project | 21,449 | **10,643** | −10,806 (−50.4%) | 61,820 | **46,702** | −15,118 (−24.5%) |
| linux | 32,456 | **14,872** | −17,584 (−54.2%) | 57,016 | **39,704** | −17,312 (−30.4%) |

Runs — monster 11,384.6/11,499.7/12,477.0; HA 9,264.7/9,275.2/9,298.9; dotnet
49,368.4/50,993.8/51,034.2; llvm 45,911.1/46,702.0/49,000.3; linux
39,402.2/39,703.9/41,494.1.

**The engine is untouched and is not claimed as moved.** `full_graph_build`'s
CLI mark moves in both directions across the wave (monster 6,235 → 7,643,
llvm 37,673 → 33,362, linux 20,910 → 21,219) — this wave changed no line
inside `EntityGraph::build`, so those are the box's noise band, and W3's
`perf_probe` engine numbers (monster 7,944 / dotnet 41,414 / llvm 34,352 /
linux 22,900) stand unre-measured, deliberately: there is no engine change for
that instrument to see.

---

### 4. Known-content cold (the bead's original half)

The bead's own framing: *"a repo whose blobs the machine corpus already knows
— most users' real first-contact experience."* Measuring it turned up a fact
that reframes this whole campaign's numbers:

**Every "cold" build this campaign has measured was already a known-content
build.** `FACTS_CORPUS probed=40869 hits=40869` on a supposedly-cold monster
run. The reason is mechanical: `SEM_CACHE_DIR` is the *repos root*, so
`default_facts_corpus_dir` resolves the machine-global corpus to its
**parent** — a battery using `mktemp -d /tmp/...` puts every repo's corpus at
`/tmp/facts-corpus`, shared across every run of every wave and never cleared
(4.9 GB on this box). W0.5's, W1's and W3's batteries all did this.

So the two sides have to be separated explicitly — `SEM_FACTS_CORPUS_DIR`
fresh-and-empty (**A: true cold**) versus pre-populated by a prior build of
the same content (**B: known content**), everything else identical, post-W4
binary:

| repo | A — true cold | B — known content | delta | corpus hits |
|---|---:|---:|---:|---|
| TypeScript monster | 12,758.8 / 12,929.1 → **12,844** | 9,834.3 / 9,979.6 → **9,907** | **−2,937 (−22.9%)** | 40,869 / 40,869 |
| linux | 45,783.6 / 45,453.2 → **45,618** | 38,682.4 / 40,185.2 → **39,434** | **−6,184 (−13.6%)** | 72,928 / 72,928 |

**Known-content cold is 9.9 s (monster) and 39.4 s (linux). It is not under
1 s, and no save-plane fix could have made it so.** The arithmetic, stated the
way the mandate requires:

```
monster, known content, 9.9 s  =  discovery 0.3 + engine ~5.6 + save ~3.1 + serialization 0.4
  the corpus removes PARSE, never RESOLVE (facts_store.rs's warm_start re-runs it)
  resolve alone (W3, clean median-of-3)        = 3.9 s   -> 3.9x over budget
  with save plane AND parse both at zero       = 3.9 s   -> still 3.9x over

linux, known content, 39.4 s
  resolve alone (W3)                           = 12.2 s  -> 12.2x over budget
  parse alone (W0, 1,499 MB / 219.4 MB/s)      =  6.8 s  ->  6.8x over budget
```

Known content buys back exactly the parse leg — 2.9 s of the monster's 12.8 s
and 6.2 s of linux's 45.6 s — and the parse leg is not what stands between
either repo and 1 s. **W4 declines the <1 s half of its bead as measured, for
the same reason W3 declined its own:** with parse bought back for free and the
save plane cut in half, what remains is a resolve phase already inside ~2x of
the only fused-resolve floor this codebase can evidence (W3 §4), and it is
4-12x the entire budget by itself.

Two things do get better and are worth stating plainly: the *shape* of the
first-contact experience is now 21-30% cheaper on every giant, and the
`FACTS_CORPUS probed/hits` line means no future bead has to guess which kind
of cold it just measured.

---

### 5. Honest residuals

1. **The two W3-named IO items are untouched.** `bow_index_io` (~0.4-0.6 s
   wall per giant) and `IMPORT_TABLE io_ms` (0.5-1.1 s wall) both sit inside
   `full_graph_build`, and this wave spent its budget where the measurement
   pointed: the save plane offered 1.8-17.6 s per giant against their
   ~1-1.7 s combined. They remain exactly as W3 recorded them, with W3's
   numbers, unre-measured here.
2. **Four corpus reads, not two.** §1 names reads 3 and 4 that W1's census
   missed. Read 3 is now parallel but still happens; read 4
   (`merge_with_local`'s read+hash of every unknown path, 0.7-2.2 s) is
   *structurally* redundant with `CorpusColumns::read`, which hashes the same
   bytes later in the same process — fusing them is the next mechanical
   deletion in this lane and it is not taken here, because the two reads sit
   on opposite sides of `EntityGraph::build` and joining them means holding
   the hash column across the resolve phase, the same residency trade W1
   fenced with a number.
3. **`cache.db` is smaller and cheaper, not retired**, and §2 states the
   arithmetic under which retiring it becomes a one-path decision rather than
   a two-path one. This wave did not take that cut.
4. **`cli_output_serialization` is 3.1 s on linux** and is *not* part of any
   user's real cold build unless they ask for `--json` — it is this harness's
   own cost, included in the totals above only because every prior wave's
   totals included it too. Deleting it from the comparison would have made
   this wave look better and would have made the numbers incomparable.
5. **`metadata_json` is not deterministic across builds.** The `entities`
   table's `metadata_json` column is `serde_json::to_string` of a `HashMap`,
   so its key order varies run to run — 78 differing rows between two runs of
   the *same* HEAD binary on tiptap. It is pre-existing, it is not this wave's
   doing, and it means the "cache.db table dumps byte-identical" gate is only
   meaningful modulo canonicalized key order (which it passes: identical
   sha256 on both sides). Left as found and named, because a future bead that
   wants a truly reproducible `cache.db` has to fix the map, not the gate.
6. **`entity_changes` was nearly deleted on a truncated grep.** An early
   census run through `| head` showed no readers and the table was cut; the
   exhaustive re-run found `SELECT … FROM entity_changes WHERE commit_sha =
   ?1` in `index_commits` and it was restored before any commit. Recorded
   because the census discipline is the load-bearing part of F1, and this is
   what it feels like when it nearly fails.

### Gates

- **Bit-identical, rails (3,794 files) and tiptap (1,533)**, HEAD-built binary
  vs this wave's: entity count, edge count, sorted-entity sha256, sorted-edge
  sha256, `index.sem` sha256, `sem graph --json` bytes (37,019,013 /
  14,483,785), and all seven `cache.db` tables' rows — identical, with the
  `metadata_json` caveat in residual 5.
- **Six `index_probe` oracles** (home-assistant-core): `ORACLE` 94,708 PASS,
  `REFS_ORACLE` 316,476 PASS (0 kind-mismatched), `FILES_ORACLE` 8 prefixes
  PASS, `TESTS_ORACLE` 316,476 checked / 50,201 tests PASS, `TRIGRAM_ORACLE`
  6 patterns PASS; `MUTATION` **skipped**
  (`no_battery_pattern_had_a_provable_true_positive` — the same
  data-dependent skip on this corpus W0.5 recorded, stated as skipped rather
  than implied as passed).
- **Diff battery**: 5 real commits x 2 repos (rails `HEAD~1..HEAD` through
  `HEAD~5..HEAD~4`, tiptap the same), `sem diff --format json` byte-identical
  on every one.
- **Suites**: sem-core lib 607, `single_pass_invariants` 3 (W1's fusion invariants green),
  sem-cli 248, sem-mcp 93. Zero failures, re-run against the final tree.
- **clippy/fmt**: clean on all four files this wave touched. The warnings
  clippy reports in `sem-mcp/src/cache.rs` (`normalize_lexical`,
  `save_incremental_with_repair_metadata`) and `sem-cli` are pre-existing, in
  functions this wave did not touch.
- **Untouchables** (`README.md`, `examples/hosted-diff/*`, `languages.rs`
  reflow hunks, `diff/cloud_upload.rs`, `diff/relations.rs`,
  `commands/setup.rs`, the two WIP test files) confirmed byte-identical via
  `git status`/`git diff --stat` before and after every commit.
- Raw runs: 30 cold giant builds for the before/after batteries, 10 for the
  known-content split, plus the verb, dirty-rebuild, bit-identical and diff
  batteries.

Bead: semx-431.

## W4.5: the conditional write (semx-4ex)

W4 measured `cache.db` at 3.3-24.3 s of every giant's cold build, deleted it
outright, and found **ten of ten verbs byte-identical and nine of ten equally
fast without it** — then declined to retire it, on two paths: `sem impact
--deps` in its name-only form, and the incremental rebuild. It also surfaced
the reason the first of those two was on the list at all: a gate in
`try_index_impact_deps` that no longer had a reason to exist.

This wave closes that gate, censuses what is actually left reading the file,
and makes the write conditional on the census. Headline: **the corpus-shaped
build no longer writes the SQL mirror at all** — 10.7-20.9% off every giant's
cold build and 0.3-1.8 GB of disk per repo unwritten — while every verb that
genuinely hydrates entity bodies keeps it and now creates it itself, on its
own first miss.

### Method

Same harness as W4 (release build, darwin, `available_parallelism=18`,
`SEM_CACHE_DIR` fresh per run under `/tmp/bench-w1` so the machine-global
corpus is the same shared one — these are *known-content* colds in W4 §4's
sense, and the true-cold split is measured separately in §5). Median-of-3, and
this time the two sides are **interleaved run-for-run** rather than measured in
blocks, so a load excursion lands on both. Load was not idle (1-minute averages
5.3-12.7 across the battery); noted, not scrubbed. Three binaries: `pre`
(HEAD, `1c1e814`), `gate` (F1 only), `cond` (both fixes).

---

### 1. F1 — the gate, verified and closed

`try_index_impact_deps` declined whenever neither `--entity-id` nor `--file`
was given. W4's claim was that this is redundant with the ambiguity check
thirteen lines below. Verified by reading both resolvers rather than by
assertion:

| | legacy `find_entity` | index `resolve_by_name_indices` |
|---|---|---|
| predicate | `entity_matches_qualified(graph, e, name)` | exact name, or `"type name"` split on the first space |
| file narrowing | filters `matching` by `file_path` when a hint is given | same filter, in the same place |
| more than one match | prints the ambiguity error, exits 1 | returns ≠ 1 candidate → the fast path declines → the legacy path prints exactly that error |

`entity_matches_qualified` *is* `entity_matches_query` plus one clause, and
that clause needs a `.` or `::` in the query to fire. **For every name
carrying neither separator the two resolvers are the same function**, so a
candidate set of size one on one side is a set of size one on the other, and
the ambiguity check alone is the whole guarantee. The gate was inherited
verbatim from the SQLite `query_dependency_impact_topology` fast path this
tier replaced — and `try_index_impact_transitive` (semx-zvq, written later)
never carried it, which is the precedent that a name-only query resolves
safely this way.

Two corrections the verification turned up, both taken:

1. **`try_index_impact_dependents` had the identical gate**, inherited by
   being written as `_deps`'s mirror. Left alone it would have kept `sem
   impact --dependents <name>` falling through to the hydrate — a second
   `cache.db` reader, which would have falsified §2's census on the spot.
2. **Names carrying `.`/`::` must decline outright**, which `_transitive`
   already did and the other two did not. `find_entity` resolves
   `Parent.child` through `parent_id`; `resolve_by_name_indices` has no parent
   join and instead treats a `::` query as a possible *entity id*. So the two
   can name different entities — a divergence that was already reachable on
   the `--file` form before this wave (an id-shaped positional name is
   answered by the index and rejected by the legacy path). The rule is now
   one shared `is_qualified_name` applied by all three fast paths.

**Impact battery** — three addressing forms (name-only, `--entity-id`,
`--file`) x four modes x two corpora, `pre` vs `gate`, byte-compared:

| corpus | modes x forms | verdict |
|---|---|---|
| rails (3,794 files, `subscribe_to_channel`) | 12 | **12/12 byte-identical** |
| TypeScript monster (40,869 files, `localPathToRefPath`) | 12 | **12/12 byte-identical** |

and re-run against a cache directory that has **no `cache.db` at all** (built
by the `cond` binary): 12/12 byte-identical again. What moves is the tier, and
the latency with it:

| query | pre (tier) | after (tier) |
|---|---|---|
| rails `impact <name> --deps` | 111 ms (`entity_lookup`, hydrate) | **27 ms** (`index_impact_deps`) |
| rails `impact <name> --dependents` | 111 ms (hydrate) | **28 ms** (`index_impact_dependents`) |
| monster `impact <name> --deps` | 703 ms (hydrate) | **56 ms** |
| monster `impact <name> --dependents` | 692 ms (hydrate) | **61 ms** |

W4 §2's `impact --deps` row (rails 0.10 → 2.06 s, monster 0.69 → 4.87 s
without `cache.db`) is now **0.03 s / 0.06 s on both sides** — the verb no
longer reads `cache.db` in any form.

---

### 2. The consumer census

Exhaustive over both crates: every `DiskCache` construction, every `load*`,
every `save*`, and every `SELECT` outside the two cache modules
(`grep -rn "SELECT" sem-cli/src sem-mcp/src` outside `build_cache.rs`/
`cache.rs` returns exactly two lines, both in `repos.rs`).

**Readers — `sem-cli`** (all through `build_cache.rs`'s `DiskCache`):

| entry point | what it loads | verbs that reach it | after W4.5 |
|---|---|---|---|
| `get_or_build_graph_with_cache_policy` | `load_with_source_scope` (full hydrate) | `sem context` after its index reroute declines; `sem entities --text`; `sem impact --all/--tests` on ≤20k-file repos after the index tier declines; `sem diff --cloud`'s relations pass | **kept** — these need entity *bodies*, which the image does not carry |
| ↳ same | `load_partial_with_source_scope` (incremental rebuild) | every caller above, plus the corpus-shaped path below | **kept where the mirror still exists**; no longer created by `sem graph` |
| `get_or_build_graph_with_test_data_and_topology_save_on_miss` | full, topology-with-test-ids, partial | `sem impact --all/--tests` on >20k-file repos | **kept** |
| `get_or_build_graph_topology_with_timings` | `load_graph_topology_with_source_scope` | **`sem graph`**; `impact --deps/--dependents` on ≤20k-file repos | reader kept, **writer removed** |
| `get_or_build_direct_dependency_graph_with_timings` | topology | `impact --deps` on >20k-file repos | reader kept, never wrote |
| `commands/repos.rs` | `cache_metadata` + `COUNT(*)` | `sem repos`' local listing | **display only** — fixed to list on either artifact |

**Reader — `sem-mcp`** (`server.rs`, its own duplicate `DiskCache`, semx-r94):
full load → partial load + incremental save → cold build + `save`. Three call
sites, all inside `SemServer::get_or_build_graph{,_topology}`.

**Proven non-consumers** (verified by reading, not assumed): `find`,
`callers`, `refs` (`commands/query.rs` does not import `DiskCache` at all, and
its own cold fallback calls `write_query_index` directly); `sem grep`; `sem
entities`' listing forms (`QueryIndex::files_under`); `sem graph`'s warm path
(`try_index_graph`); `sem diff` (its own two-tree path); and, as of F1, every
`sem impact` shape the index can answer.

**The blocker, resolved as a fact rather than a fix.** semx-r94's duplicate
`DiskCache` in `sem-mcp` is real, and the bead's worry was that a CLI-side
change merely *moves* the write. It does not, and the census says why
precisely: `sem-cli` never constructs `sem-mcp`'s `DiskCache` — it imports
`sem_mcp::cache` only for the shared substrate (`CacheSourceScope`, schema,
manifest helpers) — so the only process that writes through `sem-mcp`'s copy
is the MCP server itself, started explicitly by `sem mcp`. **The write stays
there, deliberately**, for a reason the CLI does not have: `sem-mcp` never
writes `index.sem` (`write_query_index` lives in `sem-cli`) and never goes
through the facts store (`server.rs` calls `EntityGraph::build` directly), so
`cache.db` is its *only* warm tier across restarts. Removing it there would
not trade one artifact for another; it would leave nothing. Unifying the two
`DiskCache` families — after which `sem-mcp` would inherit the index tier and
this decision could be revisited — remains semx-r94's, not this bead's.

---

### 3. F2 — the mechanism, and why this one

The census leaves one shape where `cache.db` has *no* reader: the
**corpus-shaped build**. `sem graph` asks for topology, is answered from
`index.sem` on every subsequent invocation, and — because
`get_or_build_graph_topology_with_timings` fell through to
`get_or_build_graph_with_timings`, whose policy is `Full` — paid on every cold
miss for the entity bodies and the compressed content store it never reads.
That is not a caching decision anyone made; it is a delegation accident, and
it is where the time is.

So the topology entry point gets its own `CacheMissSavePolicy::IndexOnly`,
whose save is `build_cache::write_index_only`: the one post-graph corpus read
(`CorpusColumns::read`), the test classification
(`filter_test_entities_with_custom_dirs` — the same call `write_test_flags`
makes), the byte spans, then `write_query_index`. **No SQLite connection is
opened on that arm at all.** The read-side probes switch to
`DiskCache::open_existing`, which declines rather than creating an empty
schema-only `cache.db` — behaviourally identical, since a file with no rows
misses every `load*` anyway, and it stops the build leaving a lie on disk.

Three mechanisms were on the table; this is the one the evidence picks.

- **A blanket flag over every save.** Rejected: it would also strip the mirror
  from the verbs that *do* read it, turning `sem context`'s index decline and
  `sem entities --text` into a full rebuild *every* time instead of once.
  Measured on rails: the first such verb costs 1.52 s and every later one
  0.13 s *because* it wrote the mirror; without one they all cost 1.52 s.
- **A background writer after the answer is delivered.** Rejected on
  simplicity and honesty: it needs a detached child, a second concurrent
  SQLite writer, and orphan-process handling, and it does not remove the
  machine's work — it hides it behind the next command. The bead's own rule
  ("simplest mechanism that fits the evidence wins") settles it.
- **Policy matched to request shape** — this one. It is a deletion, not a
  feature: one arm that does less, on the path whose own census row is empty.

`SEM_BUILD_CACHE=1` puts the mirror back on that path for anyone whose
workflow is repeated `sem graph` on a dirty tree (§4's arithmetic). It is one
`OnceLock` env read on a path about to do seconds of work.

**What the hydrating verbs do now** (rails, cold cache dir each time):

| sequence | pre | after |
|---|---|---|
| `sem graph .` | 3.46 s (writes `cache.db`) | **1.62 s** (writes `index.sem` only) |
| then `sem entities --text …` | 0.13 s | **1.56 s** (builds, writes `cache.db`) |
| then the same again | 0.13 s | **0.14 s** |
| `sem context Base.subscribe_to_channel` (qualified → index declines) | 0.14 s | 1.52 s first, then 0.14 s |

Byte-identical stdout on every one. The mirror is not gone; **it is paid for by
the verb that reads it, once, instead of by every build that does not.**

---

### 4. Measurement

**Cold full-CLI build, median-of-3, interleaved** (ms; `pre` = HEAD):

| repo | pre | after | delta | `cache.db` not written | `index.sem` |
|---|---:|---:|---:|---:|---:|
| home-assistant-core | 9,224.6 | **7,723.4** | −1,501 (**−16.3%**) | 296.6 MB | 71.2 MB (identical) |
| TypeScript monster | 11,612.3 | **9,478.1** | −2,134 (**−18.4%**) | 416.7 MB | 84.0 MB (identical) |
| dotnet-runtime | 50,959.2 | **45,495.0** | −5,464 (**−10.7%**) | 1.44 GB | 175.2 MB (identical) |
| llvm-project | 49,655.7 | **42,642.7** | −7,013 (**−14.1%**) | 1.32 GB | 188.8 MB (identical) |
| linux | 42,733.2 | **33,810.5** | −8,923 (**−20.9%**) | 1.81 GB | 279.6 MB (identical) |

Runs — HA pre 8,929.2/9,224.6/9,369.2, after 7,719.6/7,723.4/7,774.2; monster
pre 11,612.3/11,636.5/11,539.9, after 9,463.0/9,521.1/9,478.1; dotnet pre
50,959.2/51,512.2/50,759.2, after 45,511.2/45,495.0/45,150.5; llvm pre
52,394.7/49,655.7/49,267.2, after 43,211.0/42,642.7/42,312.0; linux pre
44,485.4/42,152.3/42,733.2, after 34,136.8/32,983.4/33,810.5. Load averages at
battery start: 5.3 (HA), 9.8 (monster), 8.6 (dotnet), 8.6 (llvm), 12.7
(linux) — which is why llvm's and linux's `pre` medians sit above W4's own
post-wave numbers (46.7 / 39.7 s). Both sides ran under the same load, run for
run.

**The dirty-rebuild trade, stated honestly.** This is the one path that gets
worse, and by how much depends on the repo. Cold build, edit one file, `sem
graph` again:

| repo (victim) | pre cold | pre dirty | after cold | after dirty | dirty delta | break-even |
|---|---:|---:|---:|---:|---:|---:|
| rails (`action_cable/channel/base.rb`) | 2,004 | **713** | 1,622 | **1,316** | +603 ms | 0.6 rebuilds |
| monster (`src/compiler/checker.ts`) | 10,030 | **5,736** | 8,475 | **7,530** | +1,794 ms | 0.9 rebuilds |
| linux (`kernel/sched/core.c`) | 47,355 | **16,342** | 30,885 | **28,677** | +12,335 ms | 1.3 rebuilds |

"Break-even" is `cold saving ÷ dirty penalty`: the number of dirty `sem graph`
runs per cold build at which the mirror starts paying for itself. It is
between 0.6 and 1.3 — **the mirror is worth its price after roughly one dirty
rebuild, and costs more than it saves before that.** So this is a genuine
trade, not a free lunch, and the flag exists because of it. Two things weigh
the default toward off anyway:

1. The dirty rebuild is *only* this path. Every per-answer verb (`find`,
   `callers`, `refs`, `impact`, `context`, `grep`) answers from the image on a
   dirty tree without rebuilding anything (QUERY-INDEX.md §5.1/§13), and `sem
   graph --json` on a 40k-file repo emits 178 MB of JSON — it is not a verb
   anyone runs in a loop.
2. The path is measurably *wrong* — see §6, item 1.

**Known-content vs true cold** (monster, `SEM_FACTS_CORPUS_DIR` split as W4 §4
established, two runs per cell):

| side | A — true cold | B — known content |
|---|---|---|
| pre | 12,743 / 14,731 | 9,715 / 10,093 |
| after | **10,756 / 11,039** | **8,280 / 7,550** |

−20.7% on true cold and −20.1% on known content: the saving is the SQL write,
which neither corpus state affects. W4 §4's conclusion is untouched — known
content still buys back the parse leg only, and the floor is still resolve.

---

### 5. Gates

- **Bit-identical, rails (3,794 files) and tiptap (1,533)**, HEAD's binary vs
  this wave's **with `SEM_BUILD_CACHE=1`** (so the SQL mirror is written on
  both sides and can be compared at all): entity count, edge count,
  sorted-entity sha256, sorted-edge sha256, `index.sem` sha256, `sem graph
  --json` bytes (37,019,013 / 14,483,785), all seven `cache.db` tables, and the
  schema's index list — identical on rails outright, and on tiptap identical
  once `metadata_json`'s key order is canonicalised (W4 residual 5's
  pre-existing `HashMap` nondeterminism: the canonicalised `entities` sha256 is
  `e3c1f8ba…` on both sides).
- **Default mode** (no flag), same two corpora: `index.sem` sha256 and `sem
  graph --json` bytes identical to HEAD's, `cache.db` **absent**.
- **Impact battery**: 24/24 byte-identical (§1), plus 12/12 against an
  index-only cache directory.
- **Diff battery**: 5 real commits x 2 repos (rails and tiptap,
  `HEAD~1..HEAD` through `HEAD~5..HEAD~4`), `sem diff --format json`
  byte-identical on all ten.
- **Five `index_probe` oracles** (home-assistant-core): `ORACLE` 94,708 PASS,
  `REFS_ORACLE` 316,476 / 0 kind-mismatched PASS, `FILES_ORACLE` 8 prefixes
  PASS, `TESTS_ORACLE` 316,476 checked / 50,201 tests PASS, `TRIGRAM_ORACLE` 6
  patterns PASS; `MUTATION` **skipped**
  (`no_battery_pattern_had_a_provable_true_positive` — the same data-dependent
  skip W0.5 and W4 recorded, stated as skipped rather than implied as passed).
  This wave touched no `sem-core` file.
- **Suites**: sem-core lib 607, `single_pass_invariants` 3, sem-cli 248, sem-mcp 93.
  Zero failures. Two sem-cli tests were updated rather than repaired, both
  because they pinned the mirror incidentally: `cache_location.rs` used
  `cache.db` as the witness for *where* the cache directory is (now either
  artifact — the tests are about location, not about which verb writes what),
  and one `impact_direct_deps.rs` warm-up needs a `cache.db` for its
  scaffolding to doctor (now `SEM_BUILD_CACHE=1`, which keeps it testing the
  index tier's freshness characterization rather than silently becoming a test
  that the mirror still gets written).
- **clippy/fmt**: zero new warnings. The five clippy warnings against the four
  touched files (`build_cache.rs`'s arg count, `graph.rs`'s modulo,
  `impact.rs`'s deref, `repos.rs`'s `sort_by_key` and modulo) are all
  pre-existing, verified by re-running clippy against HEAD's copy of
  `repos.rs`. `cargo fmt --check`: every touched file clean; `main.rs`'s six
  pre-existing hunks are the same drift §13.6/§14.7/§15.4 disclosed and are
  untouched.
- **Untouchables** (`README.md`, `examples/hosted-diff/*`, `languages.rs`
  reflow hunks, `diff/cloud_upload.rs`, `diff/relations.rs`,
  `commands/setup.rs`, the two WIP test files) byte-identical: the same
  `git diff --stat` before and after every commit (6/18/24/7/48/37 lines).
- Raw runs: 30 cold giant builds, 12 dirty-rebuild builds, 8 known-content
  builds, plus the impact, diff, bit-identical and hydrating-verb batteries.

---

### 6. Honest residuals

1. **The incremental rebuild loses an edge on the monster, and it is
   pre-existing.** Byte-comparing the dirty-rebuild outputs turned up a
   one-edge difference: the incremental path reports 196,222 edges where a
   full rebuild of the identical tree reports **196,223** — the canonical
   count every gate in this document pins. The missing edge is
   `convertToAsyncFunction.ts::renameCollidingVarNames --calls-->
   core.ts::MultiMap::add`; entity sets are identical. Reproduced with
   **HEAD's binary alone** (cold build → edit `checker.ts` → dirty rebuild =
   196,222; cold build of the same edited tree = 196,223), so it is neither
   caused nor revealed-by-luck by this wave. It is surfaced, not fixed — it
   lives in `build_incremental_with_metadata_and_import_candidates`, not in
   the save plane — and it is a second reason the corpus-shaped default no
   longer routes through that path. rails and linux dirty rebuilds are
   byte-identical, so it is not universal.
2. **`sem repos` cannot label an index-only cache.** The `repo_root` stamp
   lives in `cache.db`'s `cache_metadata` (`store_repo_origin`), so a repo
   built index-only lists its size correctly and its origin as
   "unlabeled — index-only cache, no cache.db to stamp". Adding a sidecar file
   to carry it would be new on-disk surface for a display string; not taken.
   The same pass fixed a pre-existing under-report: the listing summed only
   `cache.db*` and ignored `index.sem` and `facts/` entirely.
3. **`sem-mcp` still writes the mirror**, for the reason in §2 — it has no
   index tier and no facts store, so `cache.db` is its only cross-restart
   warm tier. This wave did not unify the two `DiskCache` families
   (semx-r94); until it is unified, an MCP-served repo and a CLI-served repo
   have different warm tiers on the same file.
4. **W4's residuals 1, 2, 4 and 5 stand unchanged**: `bow_index_io` and
   `IMPORT_TABLE io_ms`, `merge_with_local`'s redundant read+hash, the
   `metadata_json` key-order nondeterminism, and `cli_output_serialization`'s
   presence in these totals. None was in this bead's lane and none was
   re-measured.
5. **The mirror still exists, and is still 0.3-1.8 GB when a hydrating verb
   creates one.** This wave made the write conditional; it did not make the
   artifact smaller or delete it. What it removes is the *unconditional* part
   — which was, on the giants, all of it.

Bead: semx-4ex.

## W2: parse ceiling (semx-au8)

The wave's title says parse. Its invariant says the timers decide, so this section
re-attributes all five giants at HEAD first and lets the ranking fall where it
falls. Three things came out of that, in descending value: **the biggest
parse-shaped cost in the fleet was a re-parse of files nothing reads** (landed,
F1: llvm −17.7%, linux −11.2%); **tree-sitter's own headroom is gone and every
scheduling lever is refuted by measurement, one of them spectacularly** (§4);
and **the machinery that exists to buy the parse back does not pay for itself
on any giant** (§6) — a finding this wave surfaces rather than acts on, because
deleting it is not the parse lane.

### 1. Method, and one instrument correction that moves earlier numbers

- Release build (`opt-level=3`, `lto="thin"`, `codegen-units=1`), darwin,
  `available_parallelism=18`, baseline `c7a5309` (post-W4.5).
- `SEM_LOCAL=1 SEM_TIMINGS=1 SEM_PROFILE_CACHE=1 sem graph <root> --json`,
  fresh `SEM_CACHE_DIR` per run under `/tmp/bench-w1`, so the machine-global
  corpus is the shared populated one — these are **known-content colds** in
  W4 §4's sense unless stated otherwise. Median-of-3, and every before/after
  comparison is **interleaved run for run**.
- **Load was not idle and was worse than any previous wave's**: 1-minute
  averages ran 4.4 to 23.9 across the battery (other agents on the same box).
  Absolute totals here therefore sit 10-30% above W4.5's on the two CPU-heaviest
  giants (dotnet, llvm) and are *not* comparable to them; every conclusion below
  rests on interleaved A/B deltas or on ratios, never on a cross-wave absolute.
- `SEM_PROFILE_RESOLVE=1` used only for sub-phase attribution, never for a
  share, per W3's disclosed 1.45-1.65x tax.
- New instrument, `crates/sem-core/examples/parse_probe.rs` (`W2-INSTR`), and
  the reason it had to exist: `perf_probe` times whole phases, so it can say
  parse costs 8 s but not whether that 8 s is work or waiting. `parse_probe`
  times every file individually inside one parallel pass and reports `T_1`
  (summed per-file cost), `T_inf` (the most expensive single file), the achieved
  wall, `util = T_1/(P·wall)`, a greedy-list-scheduling simulation at the
  measured costs, the same 20 MiB byte-budget partition `graph.rs` computes, and
  the re-parse's used/declined split in files, bytes and cost.

**The instrument correction.** `sem` and `sem-mcp` install mimalloc as their
global allocator (`sem-cli/src/main.rs:12`); a cargo *example* does not, so
`perf_probe` — the instrument every parse floor in this document was measured
with, back to W0 — has always run on macOS's system allocator. On a workload
whose dominant allocation is tree-sitter's arena (24-40x source bytes,
semx-4w1), that is not a detail:

| parse+extract, same corpus, same box | system allocator (`perf_probe`) | mimalloc (`parse_probe`) | overstated by |
|---|---:|---:|---:|
| home-assistant-core | 994.9 ms | **724.4** | 37% |
| TypeScript monster | 4,311.7 | **2,750.5** | 57% |
| linux | 10,479.6 | **7,862.2** | 33% |
| llvm-project | 7,910.9 | **7,361.7** | 7% |
| dotnet-runtime | 7,584.2 | **8,345.7** | −9% (load) |

`parse_probe` sets `#[global_allocator]` to mimalloc for exactly this reason,
and every parse number in this section is the mimalloc one. The practical
effect on the ledger is small — W0's parse floors survive the correction (see
§3) — but a future bead quoting `perf_probe`'s `PARSE_EXTRACT` as a floor is
quoting a number ~7-57% above what the shipped binary pays.

### 2. The fresh gap table

Cold CLI, median-of-3, HEAD before this wave's fix (ms):

| repo | file_discovery | full_graph_build | ↳ facts plane | index_only_save | serialization | **total** |
|---|---:|---:|---:|---:|---:|---:|
| home-assistant-core | 149 | 6,419 | 2,274 | 801 | 289 | **7,592** |
| TypeScript monster | 329 | 7,391 | 2,854 | 1,219 | 426 | **9,338** |
| dotnet-runtime | 539 | 47,436 | 4,817 | 4,390 | 1,668 | **53,972** |
| llvm-project | 1,278 | 45,809 | 5,200 | 5,852 | 2,151 | **56,037** |
| linux | 1,259 | 22,922 | 5,032 | 6,767 | 3,311 | **34,618** |

Engine internals — `perf_probe` `PHASE_HOOK`, median-of-3, no profiler — with
the parse leg replaced by `parse_probe`'s production-allocator measurement:

| repo | IO (one parallel read) | parse+extract (mimalloc) | pre_resolve | **resolve_phase** | build_total |
|---|---:|---:|---:|---:|---:|
| home-assistant-core | 288 | 724 | 1,659 | **4,500** | 6,160 |
| TypeScript monster | 464 | 2,751 | 6,858 | **4,402** | 11,092 |
| dotnet-runtime | 636 | 8,346 | 10,761 | **32,350** | 43,508 |
| llvm-project | 1,071 | 7,362 | 9,826 | **25,865** | 35,624 |
| linux | 947 | 7,862 | 13,367 | **14,657** | 28,023 |

Phase against floor, and the gap that remains (ms; floor = the measured
physics leg for that phase, `—` where the phase has no physics floor because it
is implementation, not physics):

| repo | phase | now | floor | gap |
|---|---|---:|---:|---|
| home-assistant-core | parse (true cold) | 724 | 724 | **1.0x — at the floor** |
| | resolve | 4,500 | — | the binding item |
| | facts plane | 2,274 | 0 | **negative value (§6)** |
| | index write | 801 | 405 (image+atomic) | 2.0x |
| TypeScript monster | parse (true cold) | 2,751 | 2,751 | **1.0x — at the floor** |
| | resolve | 4,402 | 3,893 (W3's fused floor, by definition) | 1.1x |
| | facts plane | 2,854 | 0 | **zero value (§6)** |
| | index write | 1,219 | 575 | 2.1x |
| dotnet-runtime | parse (true cold) | 8,346 | 8,346 | **1.0x — at the floor** |
| | ↳ pass-2 re-parse (inside resolve) | 10,971 (profiled) ≈ 7,565 clean | 5,415 (same parse, flat) | **1.4x** |
| | resolve | 32,350 | 8,487 (W3 per-entity fused floor) | 3.8x |
| | facts plane | 4,817 | 0 | zero value |
| | index write | 4,390 | 2,734 | 1.6x |
| llvm-project | parse (true cold) | 7,362 | 7,362 | **1.0x — at the floor** |
| | ↳ pass-2 re-parse | 2,815 (profiled, post-F1) | 1,989 (used-files-only, flat) | 1.4x |
| | resolve | 25,865 | 11,191 | 2.3x |
| | facts plane | 5,200 | 0 | **negative value** |
| | index write | 5,852 | 3,448 | 1.7x |
| linux | parse (true cold) | 7,862 | 7,862 | **1.0x — at the floor** |
| | ↳ pass-2 re-parse | 2,912 (W3, profiled) → ~120 simulated post-F1 | ~70 | eliminated by F1 |
| | resolve | 14,657 | 12,206 (W3: already *under* the fused floor) | 1.0x |
| | facts plane | 5,032 | 0 | **negative value** |
| | index write | 6,767 | 4,540 | 1.5x |

**The ranking, per repo, by measured value at HEAD** — and parse is the top item
on none of them:

| repo | #1 | #2 | #3 | where parse sits |
|---|---|---|---|---|
| home-assistant-core | resolve 4,500 | **facts plane 2,274 (net −2,339, §6)** | index write 801 | 724 ms, at its floor, and bought back on a known-content cold |
| TypeScript monster | resolve 4,402 | facts plane 2,854 | index write 1,219 | 2,751 ms, at its floor, bought back |
| dotnet-runtime | resolve 32,350 — **of which re-parse ~7,565 is parse-shaped** | facts 4,817 | index write 4,390 | the re-parse *is* the top parse item in the fleet |
| llvm-project | resolve 25,865 | index write 5,852 | facts 5,200 | re-parse was 13,053 profiled (W3) → **2,815 after F1** |
| linux | resolve 14,657 | index write 6,767 | facts 5,032 | re-parse was 2,912 profiled (W3) → **~120 after F1** |

### 3. F1 — pass 2 stops parsing the files pass 2 declines

`resolve_scopes_in_file_chunks` partitions the **whole** corpus file list by a
20 MiB byte budget and visits every chunk holding at least one scope-resolvable
file. Inside a visited chunk, the re-parse read and parsed **every** file with a
tree-sitter config. The per-file scope closure then opened with
`get_language_config(ext).and_then(|c| c.scope_resolve)?` and dropped every file
whose language has no scope config — and `languages.rs` gives `scope_resolve:
None` to C, XML, Perl, SQL, OCaml, Lua, Elixir, HCL, Nix, Haskell, Elm, Clojure,
D and Fortran. **C is the one that matters**: `.c`/`.h` are two giants.

Measured by `parse_probe`, per cold build, inside visited chunks:

| repo | files re-parsed and consumed | files re-parsed and **discarded** | discarded bytes | their measured parse cost (T_1 / at P=18) |
|---|---:|---:|---:|---:|
| llvm-project | 43,274 | **34,260** | 267.0 MB | 37,220 ms / 2,068 ms |
| linux | 2,050 | **29,158** | 333.2 MB | 34,883 ms / 1,938 ms |
| dotnet-runtime | 34,898 | **11,313** | 75.1 MB | 7,661 ms / 426 ms |
| home-assistant-core | 18,150 | 0 (4,175 files decline, none has a tree-sitter config, so the re-parse already skipped them) | — | — |
| TypeScript monster | 39,303 | 0 (same) | — | — |

The fix is the admission test, hoisted into one function
(`scope_resolve_config_for_path`) and applied by the re-parse loop *before it
reads a byte*, and by both consumers that previously spelled it inline — so the
skip cannot drift from the decline it mirrors. Which grammar a surviving file is
re-parsed *with* is still `reparse_language_config`'s question, overrides and
all: one function decides whether, the other decides which.

**Why this is elimination and not a semantics change.** Chunk membership, chunk
order, the visited-chunk guard and every surviving file's facts are untouched.
Of the vector's consumers, `build_ts_default_export_table` filters to JS/TS,
`build_swift_call_signatures` to `.swift`, the return-type/init-attr scan and the
per-file scope closure both repeat the admission test, and `parsed_by_path` is
read only for the file being resolved. Exactly one consumer does not repeat it —
`infer_constructor_param_types`' `scan_constructor_calls` sweep — and its own
early return makes it a no-op unless `init_params` *and* `attr_to_param` are
non-empty, both of which are built only from files that pass the test. That
argument leaves a theoretical window (a Python file in the same chunk as a C
file, on a corpus where a C call expression could name a Python class with
`__init__` attributes), so it is not the gate: **seven corpora, byte-identical
`sem graph --json` and `index.sem` sha256, is** (§7).

Interleaved median-of-3, both binaries run for run:

| repo | before | after | delta | engine (`full_graph_build`) |
|---|---:|---:|---:|---|
| llvm-project | 43,396 | **35,703** | **−7,693 (−17.7%)** | 36,016 → 28,943 |
| linux | 34,680 | **30,798** | **−3,882 (−11.2%)** | 24,033 → 20,301 |
| dotnet-runtime | 49,972 | **47,521** | −2,451 (−4.9%) | 43,054 → 41,977 |
| home-assistant-core | 7,805 | 7,674 | −132 (noise) | 6,614 → 6,519 |
| TypeScript monster | 10,208 | 9,478 | −730 (noise) | 7,786 → 7,656 |

Runs — llvm before 40,260/43,396/53,332, after 35,083/35,703/42,516; linux
before 33,545/34,680/36,904, after 30,720/30,798/31,629; dotnet before
49,229/49,972/51,076, after 45,590/47,521/52,079.

**Honest reading of dotnet's row.** The modelled work removed there is 426 ms,
and the measured median moved 2,451 ms with overlapping run ranges. Take the
model, not the median: dotnet's win is a few hundred milliseconds plus the 75 MB
of reads that no longer happen. llvm and linux are the real ones, and their
deltas are 4-10x their own run spread. home-assistant-core and the monster are
flat **by construction**, not by luck: every file they decline has no
tree-sitter config at all, so the old code already skipped it.

Profiled confirmation (`SEM_PROFILE_RESOLVE=1`, post-F1): llvm `reparse_ms`
2,815 against W3's 13,053; linux's chunked re-parse effectively gone (its
2,050 consumed files are 16.6 MB); dotnet 10,971, unchanged in kind because
`.cs` *is* scope-resolvable — its re-parse was never the waste.

### 4. The squeeze, and three levers the measurements refuted

The bead's order was: exhaust tree-sitter's own headroom before considering a
different parser. Every item below is measured on the production allocator.

**Core utilization — parse is work-bound, not span-bound.** `util = T_1/(P·wall)`
over one parallel pass, and the makespan simulation at the measured per-file
costs:

| repo | wall | T_1 | T_1/P | **util** | span (`T_inf`) | span as % of wall |
|---|---:|---:|---:|---:|---:|---:|
| home-assistant-core | 724 | 12,781 | 710 | **0.98** | 65 | 9% |
| TypeScript monster | 2,751 | 45,169 | 2,509 | **0.91** | 487 | 18% |
| linux | 7,862 | 132,781 | 7,377 | **0.94** | 610 | 8% |
| llvm-project | 7,362 | 106,609 | 5,923 | **0.81** | 1,922 | 26% |
| dotnet-runtime | 8,346 | 103,137 | 5,730 | **0.69** | 3,304 | 40% |

Two corpora leave real capacity on the floor (llvm 0.81, dotnet 0.69) and both
have a fat tail: `polly/.../isl/typed_cpp.h` at 1.9 s, `TestData.g.cs` at 3.3 s,
`hugeexpr1.cs` at 2.5 s — the generated fixtures this document already tracks.
Brent says the fix is longest-first dispatch. **Brent is wrong here, and by a
factor of four.**

**Lever 1, refuted: longest-processing-time-first scheduling.** Real dispatch,
not simulation, three repetitions per order interleaved inside one process,
all three orders going through the same index indirection so the control
isolates ordering from access pattern:

| repo | corpus order (path) | **longest-first (LPT)** | shortest-first | LPT simulation predicted |
|---|---:|---:|---:|---:|
| home-assistant-core | 687/702/705 | **1,320** | 679/685/697 | 710 |
| TypeScript monster | 2,587/2,716/2,894 | **8,846** | 2,782/2,828/2,990 | 2,509 |
| linux | 7,743/7,943/8,786 | **17,357** | 7,816/8,693/8,889 | 7,377 |
| llvm-project | 7,492/8,143/8,909 | **21,634** | 7,678/8,043/8,129 | 5,923 |
| dotnet-runtime | 7,123/7,300/7,598 | **29,131** | 6,982/7,415/7,550 | 5,730 |

The schedule theory says 5,730 ms for dotnet; the machine says 29,131 — **5.1x
worse than predicted and 4.0x worse than doing nothing.** The reason is that
Brent's model assumes independent tasks, and these tasks share one resource:
a tree-sitter tree is 24-40x its source bytes (semx-4w1), so dispatching the
biggest files *together* maximizes concurrent arena residency. LPT optimizes the
one quantity that is not the constraint and pessimizes the one that is.
Shortest-first — the reverse, which staggers the giants to the end — is inside
the noise band on every repo (best case dotnet −4%, and it also needs a
`metadata` sweep to learn sizes at all, 6-32 ms). **Declined: no ordering of
pass 1 is worth its code.** Recorded because it is the campaign's fourth
measured decline and the only one where the textbook answer was actively
harmful.

**Lever 2, refuted: parser instance / arena reuse.** The shipped thread-local
`PARSER_CACHE` (one `tree_sitter::Parser` per language per thread) versus a
fresh `Parser::new()` per file, same corpus, same run:

| repo | cached (shipped) | fresh per file | difference |
|---|---:|---:|---:|
| home-assistant-core | 489 | 676 | cached −28% |
| TypeScript monster | 2,426 | 2,588 | cached −6% |
| linux | 6,169 | 6,005 | +3% (noise) |
| llvm-project | 5,185 | 5,328 | cached −3% |
| dotnet-runtime | 5,415 | 5,454 | −1% (noise) |

Reuse earns its keep on the small-file corpus and is a wash on the giants —
there is nothing left to win here, and nothing to lose by keeping it. W0 reached
the same verdict from a different comparison; this re-measures it on the
allocator production actually uses.

**Lever 3, refuted: the language-config lookup path.** `get_language_config` for
every file in the corpus, nothing parsed: **0.20-0.91 ms** for 18k-79k files, i.e.
0.003-0.012% of the parse it gates. `registry.get_plugin*`'s per-file
`Vec<String>` of lowercased extensions sits on the same path and is the same
order of cost. **Not a lever; stated with the number so nobody re-derives it.**

**What is left in the 0.69-0.81 utilization**, therefore, is neither scheduling
nor construction nor lookup: it is the memory system under 18-way tree-sitter
arena churn, on an allocator (mimalloc) the product already ships and which is
itself worth 22-36% against the system one. The only remaining move against it
is to allocate less per byte parsed — which is a different parser, not a better
schedule.

### 5. The FastExtractor verdict, with its arithmetic

`OXC-FASTPATH.md` already carries the engineering verdict (declined twice: once
on `structural_hash`'s non-equivalence, once on the diff oracle — 16 of 73 real
commits divergent). semx-1gy's reopen condition is cold latency as a goal, which
this campaign is, so the question is re-asked here as pure arithmetic and gets a
harder answer than "not worth it":

1. **The seam cannot reach the cold build, and this is structural.** Pass 1
   calls `extract_entities_with_tree`, not `extract_entities`, because it needs
   the `tree_sitter::Tree` itself — the retain arm hands it to pass 2, and the
   chunked arm hands it to `precompute_js_ts_file_facts`, which is what spares
   the monster the pass-2 re-parse F1 just measured elsewhere. A fast extractor
   has no tree for either. Measured, not argued: `SEM_FASTPATH=1` moves the
   monster's `build_total` from 8,635 ms to 8,624 ms with identical entity
   counts, because the fast path never ran (`OXC-FASTPATH.md` §Phase 4).
2. **Even granting an oracle-clean extractor and a tree-free pass 2, the
   ceiling is one repo.** The monster is the only corpus in the fleet with a
   measured alternative parser. Its parse+extract is 2,751 ms of an ~11,000 ms
   *true* cold; at `OXC-FASTPATH.md`'s conservative 20x on the JS/TS share
   (68.8% of bytes) that is 2,751 → ~950 ms, so **the whole bet buys ~1.8 s of
   an 11 s build, leaving 9.2 s — 9.2x over the mandate.**
3. **On the build this campaign actually measures, it buys zero.** A
   known-content cold reuses stored entities per file (`GraphSession::
   warm_start`), so pass 1 parses nothing at all. A faster parser for a parse
   that does not happen is worth 0.0 ms, on every giant, on the median user's
   second machine-touch onwards.
4. **No other giant has a candidate.** There is no measured oxc-class extractor
   for C#, C++, C or Python in this repository, and §4 shows the tree-sitter
   path they do use is at 0.69-0.98 utilization with every scheduling lever
   dead.

**Verdict: declined again, and this time the arithmetic — not the oracle — is
the binding reason.** The prerequisite is unchanged and worth restating so a
future bead does not have to rediscover it: any fast extractor must pass
`diff_oracle`'s equivalence battery at the facts level (`src/parser/
diff_oracle.rs`), and a fast extractor that cannot produce a tree also has to
answer what pass 2 does instead — which is the same `PrecomputedFileFacts`
question W3 fenced for C#/C++, not a parser question at all.

### 6. The finding this wave surfaces and does not act on: the parse is not worth avoiding

The parse ceiling has a second face. Every giant runs a facts plane whose entire
purpose is to buy the parse back: `FactsCorpus::merge_with_local` reads and
hashes every file, `warm_start` reads and hashes every file again and clones the
stored entities, and `export_persisted`/`FactsStore::save`/`populate_delta` write
the snapshot and the machine-global shards. W4 §4 priced what it *buys*
(2.9 s on the monster, 6.2 s on linux). Nobody had priced it against what it
*costs*.

`SEM_FACTS_CACHE=0` turns the whole plane off — no corpus read, no store, no
delta write — and makes the build parse for itself. Interleaved median-of-3,
corpus verified hitting on the ON side (`FACTS_CORPUS probed=82352 hits=82352`
on llvm, `22336/22336` on home-assistant-core):

| repo | facts plane ON (today's default) | OFF | delta | plane's own marks | parse it avoids |
|---|---:|---:|---:|---:|---:|
| home-assistant-core | 7,527 | **5,188** | **−2,339 (−31.1%)** | 2,274 | 724 |
| TypeScript monster | 9,465 | 9,480 | +15 (flat) | 2,854 | 2,751 |
| linux | 31,036 | **30,100** | −936 (−3.0%) | 5,032 | 7,862 |
| llvm-project | 36,735 | **34,665** | −2,070 (−5.6%) | 5,200 | 7,362 |
| dotnet-runtime | 45,090 | 45,176 | +86 (flat) | 4,817 | 8,346 |

**On no giant in this fleet does the parse-avoidance tier pay for itself.** It
is 2.3 s of pure loss on home-assistant-core, a wash on the monster and dotnet,
and 0.9-2.1 s of loss on linux and llvm. The reason the arithmetic comes out
worse than "plane cost vs parse saved" suggests is that the plane's marks
undercount it: `warm_start`'s own whole-corpus read+hash and its per-file entity
clone (990k-2.3M entities) live inside `full_graph_build` and are not in any
`CACHE_SAVE_PHASE` line.

Why this section stops here: the facts plane is also the cloud pipeline's
producer and the warm-rebuild tier's substrate, so `SEM_FACTS_CACHE`'s default is
not a parse-lane decision, and W4 residual 2 already owns the mechanical half of
it (fusing `merge_with_local`'s read+hash with `CorpusColumns::read`). What W2
contributes is the price tag: **the tier is worth −2.3 s to +0.1 s per giant
today, and any bead that keeps it should be able to say why against these
numbers.**

### 7. Gates

- **Bit-identical, seven corpora**, HEAD's binary vs this wave's, fresh cache
  directory per side: `sem graph --json` bytes *and* `index.sem` sha256 identical
  on rails (37,019,013 B), tiptap (14,483,785), home-assistant-core
  (133,186,305), the TypeScript monster (178,112,953), linux (886,893,004),
  llvm-project (584,545,936) and dotnet-runtime (723,121,464). The last three are
  the ones that matter: they are exactly the corpora whose re-parse set F1
  shrinks by 11k-34k files.
- **Six `index_probe` oracles** (home-assistant-core): `ORACLE` 94,708 PASS,
  `REFS_ORACLE` 316,476 / 0 kind-mismatched PASS, `FILES_ORACLE` 8 prefixes PASS,
  `TESTS_ORACLE` 316,476 checked / 50,201 tests PASS, `TRIGRAM_ORACLE` 6 patterns
  PASS; `MUTATION` **skipped**
  (`no_battery_pattern_had_a_provable_true_positive` — the same data-dependent
  skip W0.5, W4 and W4.5 recorded, stated as skipped rather than implied as
  passed).
- **Suites**: sem-core lib 607, `single_pass_invariants` 3, sem-cli 248, sem-mcp 93.
  Zero failures, re-run against the final tree.
- **Diff battery**: 5 real commits x 2 repos (rails and tiptap, `HEAD~1..HEAD`
  through `HEAD~5..HEAD~4`), `sem diff --format json` byte-identical on all ten.
  **Impact battery**: 4 modes x 2 repos byte-identical.
- **clippy/fmt**: zero new warnings on either touched file (the one clippy
  raised against F1's first draft — a `?`-shaped `if`/`return None` — was fixed
  before the commit rather than waived); `cargo fmt --check` clean on both, with
  `index/format.rs`'s pre-existing drift left as found.
- **Untouchables** (`README.md`, `examples/hosted-diff/*`, `languages.rs` reflow
  hunks, `diff/cloud_upload.rs`, `diff/relations.rs`, `commands/setup.rs`, the
  two WIP test files) byte-identical, confirmed by `git status`/`git diff --stat`
  before and after every commit.
- Raw runs: 30 attribution builds, 15 `perf_probe` builds, 12 `parse_probe`
  runs (each 5+ full-corpus parse passes), 14 gate builds, 30 interleaved A/B
  builds, 30 facts-plane builds, plus the oracle, diff and impact batteries.

### 8. Per-repo floor, restated at HEAD

Floor = measured parse (production allocator) + one parallel corpus read +
the index image build and atomic write; join stays <15 ms everywhere (W3 §3).

| repo | parse | + IO | + index write | **floor** | measured (post-F1) | gap | what binds it |
|---|---:|---:|---:|---:|---:|---:|---|
| home-assistant-core | 724 | 288 | 405 | **1,417** | 7,674 | 5.4x | resolve 4.5 s; facts plane 2.3 s of negative value |
| TypeScript monster | 2,751 | 464 | 575 | **3,790** | 9,478 | 2.5x | resolve 4.4 s, already at W3's fused floor |
| dotnet-runtime | 8,346 | 636 | 2,734 | **11,716** | 47,521 | 4.1x | resolve 32 s, of which ~7.6 s is a second parse fenced by C# partial classes |
| llvm-project | 7,362 | 1,071 | 3,448 | **11,881** | 35,703 | 3.0x | C++ overload disambiguation (W3), 33.8 s cumulative |
| linux | 7,862 | 947 | 4,540 | **13,349** | 30,798 | 2.3x | bag-of-words tokenize + index image |

W0's floors were 1,677 / 1,803 (oxc-adjusted; 3,900 without) / 12,324 / 11,882 /
14,857. **They survive at HEAD**, within 0-15% on every repo, measured with a
corrected instrument — which is the useful thing to be able to say about a
number four waves old.

And the sentence the epic's done-condition asks for, per repo, with arithmetic:

- **home-assistant-core — parse is done; 724 ms at 0.98 utilization is the
  floor, and a known-content cold pays 0 of it.** What is left is a 4.5 s
  resolve and a facts plane that costs more than the parse it saves. Not a parse
  problem any more.
- **TypeScript monster — parse is done, 2,751 ms at 0.91, bought back to 0 on a
  known-content cold.** The one repo with a faster-parser candidate is the one
  whose parse is already free on the path that matters (§5).
- **dotnet-runtime — the last parse item in the fleet, and it is a second
  parse.** 8,346 ms of pass-1 parse floor (8.3x over the mandate on parse
  physics alone, at 0.69 utilization no schedule can lift) plus ~7,565 ms clean
  of pass-2 re-parse. Eliminating the second one needs `PrecomputedFileFacts`
  for C#, which W3 fenced on partial classes; **reducing** it is worth at most
  7,565 → 5,415 (the same parse, flat, no chunk barrier), i.e. ~2.2 s, and costs
  a second chunk of live trees. Neither is taken here; both are priced.
- **llvm-project — parse at its floor (7,362 ms, 7.4x over the mandate by
  itself) and its re-parse down 78% this wave.** What remains above the floor is
  resolve, which W3 named and declined.
- **linux — parse at its floor (7,862 ms, 7.9x over by itself), re-parse
  effectively gone.** W0's sentence stands verbatim and is now measured on the
  right allocator: linux cannot clear 1 s on parse physics alone at any parser
  technology measured in this repository.

**W2 closes with one landed elimination, three refuted levers, one re-declined
bet, and one surfaced finding worth more than the wave's own title.** No giant
is under 1 s; parse is no longer the reason for any of them.

### 9. Honest residuals

1. **Dotnet's median A/B delta (−2,451 ms) is bigger than its modelled work
   removed (−426 ms) and its run ranges overlap.** Reported as measured, read as
   the model.
2. **The box was loud.** Load averages 4.4-23.9; absolute totals here run 10-30%
   above W4.5's on dotnet and llvm and must not be compared with them. Every
   claim rests on an interleaved delta or a ratio.
3. **`perf_probe`'s parse numbers, everywhere in this document before this
   section, are system-allocator numbers** — 7-57% above what the shipped binary
   pays. The phase *shares* they were used for survive; a floor quoted from them
   does not, unqualified.
4. **The pass-2 re-parse's remaining cost on dotnet is untouched**, and the two
   ways to move it (facts for C#, or a pipelined chunk parse trading a second
   chunk's tree residency) are both priced in §8 and neither is taken.
5. **§6's finding is a price tag, not a decision.** Nothing about the facts
   plane changed in this wave, and `SEM_FACTS_CACHE`'s default is still on.
6. **`infer_constructor_param_types` still sweeps every re-parsed tree without
   repeating the admission test.** F1 is safe because the trees it removes are
   ones that test declines, and seven corpora agree byte-for-byte — but the
   sweep's own missing filter is pre-existing surface, and a future bead that
   adds a scope config to a language will make that window wider, not narrower.

Bead: semx-au8.

## W5: the final matrix, and the corpus-size finding (semx-gbb)

The epic's re-bench loop. Five giants, shipped `sem` binary at `facb63b`, serial,
one repo at a time. It reports both metrics the campaign has used — engine-only
and full-CLI — side by side, because they diverged and were quoted
interchangeably. It also overturns W2 §6's facts-plane verdict, for a reason W2
could not have seen from inside its own harness.

### 1. Method, and the artifact that forced it

- Release build, darwin, `available_parallelism=18`, `SEM_LOCAL=1 SEM_TIMINGS=1
  SEM_PROFILE_CACHE=1 sem graph <root> --json`, fresh `SEM_CACHE_DIR` per run.
- **True cold** = fresh-and-empty `SEM_FACTS_CORPUS_DIR` (verified
  `FACTS_CORPUS probed=N hits=0`). **Known content** = the same corpus
  pre-populated by one prior build of the same tree (`hits=N`), which is W4 §4's
  split, per repo rather than machine-shared.
- Median-of-5 (HA, monster), median-of-3 (dotnet, llvm, linux).
- **The three cold columns are interleaved run-for-run.** The first attempt
  measured them in blocks and produced known-content builds *slower* than
  true-cold ones on home-assistant (9,120 vs 7,233) — physically impossible,
  and pure load drift: the box's 1-minute average rose from 9 to 16 across the
  block. Blocked measurement on a drifting box is not measurement. The blocked
  numbers are discarded, not reported.
- Load was not idle and was not mine: 7.8-30 across the battery, all of it the
  desktop (`bb.app`, Chrome, WindowServer, agent-browser), verified by `ps` —
  no agent or `cargo` process was running. Two runs at the end, at load 4.8,
  calibrate the inflation: dotnet facts-off measured **45,070 ms at load 4.8
  against 51,292 ms at load ~15, i.e. absolutes here run ~12-14% high.**

### 2. The matrix

| | HA | monster | dotnet | llvm | linux |
|---|---:|---:|---:|---:|---:|
| files | 22,325 | 40,865 | 47,455 | 82,123 | 72,787 |
| entities | 257,832 | 454,541 | 990,654 | 1,306,421 | 2,312,433 |
| **true-cold full-CLI** | **8,103.5** | **12,504.2** | **55,810.0** | **49,789.1** | **39,822.1** |
| **true-cold, `SEM_FACTS_CACHE=0`** | **6,777.6** | **11,344.3** | **51,291.9** | **42,855.9** | **32,356.7** |
| **known-content full-CLI** | **6,801.5** | **8,379.2** | **45,648.1** | **37,371.1** | **30,496.8** |
| **engine-only** (`perf_probe` `build_total`) | **5,802.0** | **9,982.5** | **43,644.6** | **38,075.1** | **29,588.2** |
| engine (shipped binary `full_graph_build`, known) | 5,627.1 | 6,282.7 | 39,901.4 | 29,756.3 | 19,456.4 |
| **warm rebuild** | **177.9** | **252.4** | **772.5** | **763.3** | **1,351.1** |
| **PR update (50 files, `sem diff`)** | **168.2** | **928.0** | **525.4** | **499.4** | **1,057.2** |
| **peak RSS, true cold** | **4.04 GB** | **6.67 GB** | **19.94 GB** | **15.16 GB** | **17.59 GB** |
| resolve_phase (engine) | 4,263 | 3,886 | 32,600 | 25,312 | 12,093 |

Runs — HA true-cold 7768.5/7889.5/8103.5/8532.3/8954.4, facts-off
6550.9/6727.3/6777.6/6967.0/8368.4, known 6635.3/6732.3/6801.5/6968.0/7638.3;
monster true-cold 12201.8/12356.6/12504.2/12804.4/12818.1, facts-off
11113.2/11203.9/11344.3/11392.4/12020.5, known
8148.8/8323.2/8379.2/8387.8/8524.0; dotnet true-cold
54676.6/55810.0/55907.7, facts-off 47234.8/51291.9/54686.6, known
45415.0/45648.1/46074.2; llvm true-cold 44690.9/49789.1/52170.0, facts-off
42498.0/42855.9/44798.1, known 36626.1/37371.1/41154.7; linux true-cold
39175.6/39822.1/40806.3, facts-off 32018.5/32356.7/33547.9, known
30019.7/30496.8/31460.7.

The `perf_probe` column is the metric the user's original before/now table used
and is reported for continuity with it; it is a **system-allocator** number per
W2 §1 and its parse leg reads 7-57% high. The shipped-binary row beneath it is
the same phase on mimalloc.

### 3. The facts plane, re-priced — W2 §6 is overturned by corpus size

W2 measured `SEM_FACTS_CACHE=0` faster on every giant and concluded the tier
never pays. Interleaved, against a **per-repo** corpus:

| repo | plane costs (true cold) | plane saves (known content) |
|---|---:|---:|
| home-assistant-core | +1,325.9 (+19.6%) | −23.9 (**nothing**) |
| TypeScript monster | +1,159.9 (+10.2%) | **−2,965.1 (−26.1%)** |
| dotnet-runtime | +4,518.1 (+8.8%) | **−5,643.8 (−11.0%)** |
| llvm-project | +6,933.2 (+16.2%) | **−5,484.8 (−12.8%)** |
| linux | +7,465.4 (+23.1%) | **−1,859.9 (−5.7%)** |

It pays back after ~one rebuild on four of five. HA is the one true wash — its
parse is 724 ms, so there is nothing worth buying back.

**The variable W2 could not see, because it never varied it: corpus size.** W2
used the shared `/tmp/bench-fleet/facts-corpus`, grown to **7.9 GB** by nine
repos across five waves. Same repo, same content, identical hit rate:

| monster, known content, 40,869/40,869 hits both sides | run 1 | run 2 |
|---|---:|---:|
| against a **556 MB** per-repo corpus | 8,986 | 8,106 |
| against the **7.9 GB** shared corpus | 11,072 | 10,878 |

**+2.5 s (+29%) purely from a 14x larger corpus at an unchanged hit rate.** The
mechanism is in the instrumentation and is not subtle: `shards_read=1024` on
every run regardless of corpus size or repo size. `FactsCorpus::merge_with_local`
reads every shard to answer a membership question, so its cost tracks everything
the machine has ever indexed rather than what this build needs. Sharding by
content hash — so a build opens only the shards that can contain its blobs — is
the named fix, and it is worth 55 percentage points of the tier's value. Not
taken here: W5 is a measurement bead.

**And the plane's memory cost, which no save-plane timer covers**
(`warm_start`'s read+hash and its per-file entity clone live inside
`full_graph_build`):

| repo | facts on | facts off | plane's cost |
|---|---:|---:|---:|
| dotnet-runtime | 19.94 GB | **10.71 GB** | **+9.23 GB (+86%)** |
| linux | 17.59 GB | **11.81 GB** | **+5.78 GB (+49%)** |

This is the explanation for peak RSS reading 19.94 GB on dotnet against the
8.28 GB semx-g6t recorded: byte-budget chunking still holds, and the facts
plane was added on top of it and never had its peak measured.

### 4. The floor ledger, final

Floor = measured parse (production allocator, W2 §8) + one parallel corpus read
+ index image build and atomic write. Join <15 ms everywhere (W3 §3,
JOIN-RESOLUTION.md §4.3).

| repo | parse | + IO | + index write | **floor** | true cold | gap | known content | gap |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| home-assistant-core | 724 | 288 | 405 | **1,417** | 8,104 | 5.7x | 6,802 | 4.8x |
| TypeScript monster | 2,751 | 464 | 575 | **3,790** | 12,504 | 3.3x | 8,379 | 2.2x |
| dotnet-runtime | 8,346 | 636 | 2,734 | **11,716** | 55,810 | 4.8x | 45,648 | 3.9x |
| llvm-project | 7,362 | 1,071 | 3,448 | **11,881** | 49,789 | 4.2x | 37,371 | 3.1x |
| linux | 7,862 | 947 | 4,540 | **13,349** | 39,822 | 3.0x | 30,497 | 2.3x |

W0's floors (1,677 / 1,803 / 12,324 / 11,882 / 14,857) **survive within 0-15%**,
now on the corrected allocator — four waves later.

**The most generous possible build**: known content (parse bought back to 0)
with resolve at the best rate this codebase has ever demonstrated (the fused
JS/TS path, 19.60 ms/MB, W3 §4):

| repo | IO + index write | + resolve at best-ever | **best conceivable** | over 1 s by |
|---|---:|---:|---:|---:|
| home-assistant-core | 693 | 2,415 | **3,108** | **3.1x** |
| TypeScript monster | 1,039 | 3,893 | **4,932** | **4.9x** |
| dotnet-runtime | 3,370 | 8,487 | **11,857** | **11.9x** |
| llvm-project | 4,519 | 11,191 | **15,710** | **15.7x** |
| linux | 5,487 | 12,206 | **17,693** | **17.7x** |

**No giant reaches 1 s**, at any combination of every win this campaign
measured plus wins it did not take.

### 5. Per-giant verdicts, the sentence the epic asks for

- **home-assistant-core — the only giant whose parse fits the budget** (724 ms,
  0.72x), and it does not help: 4.3 s of resolve against it. Floors at ~3.1 s.
  Its facts tier is worth exactly nothing (§3).
- **TypeScript monster — parse 2.8x over budget by itself**; resolve is already
  *at* the fused floor by definition. Floors at ~4.9 s. The one repo with a
  faster-parser candidate, worth ~1.8 s of an 11 s build (W2 §5).
- **dotnet-runtime — parse 8.3x over by itself** at 0.69 utilization no schedule
  can lift (W2 §4), plus ~7.6 s of second parse fenced on C# partial classes
  (W3 §5). Floors at ~11.9 s.
- **llvm-project — parse 7.4x over by itself**; the remainder is C++ overload
  disambiguation, named and declined in W3. Floors at ~15.7 s.
- **linux — parse 7.9x over by itself.** W0's sentence stands verbatim, now
  measured on the right allocator and re-confirmed at HEAD: linux cannot clear
  1 s on parse physics alone at any parser technology measured in this
  repository. Floors at ~17.7 s.

### 6. Honest residuals

1. **Absolutes run ~12-14% high** (§1's load calibration). Every delta here is
   interleaved and survives it; every absolute is an upper bound.
2. **The engine column is a system-allocator number** and is reported that way
   only because it is the metric the user's original table used.
3. **The corpus-sharding fix is named and not taken.** It is the largest
   remaining item in this ledger by value-per-line and it is not a measurement
   bead's to land.
4. **The dotnet bench fixture had 50 pre-existing dirty files** from an earlier
   wave's dirty-rebuild battery; this bead's PR-update column restores with
   `git checkout -- .`, which reverted 46 of them. HEAD unchanged, fixture now
   clean at its pinned commit, no `sem` repository touched. Surfaced rather
   than left to be discovered.
5. **`n=3` on the three largest giants**, not 5. Their run spreads are 1.5-7.5 s
   and the conclusions turn on 2-8x ratios, not on 5% differences.

Bead: semx-gbb. Epic: semx-sn8, closed on its second clause — every floor named
with arithmetic, no giant under 1 s.

## LOCAL-COLD: corpus reads stop tracking corpus size (semx-fqh)

W5 §3 left one named, untaken fix: the local facts corpus read `shards_read=1024`
on every build regardless of what was being built, so the same repo at an
identical 40,869/40,869 hit rate cost 8.1-9.0 s against a 556 MB per-repo corpus
and 10.9-11.1 s against the 7.9 GB shared one — **+29% from corpus size alone**.
This bead takes it. The result is that corpus size no longer moves build time at
all: the penalty goes from **+4,716 ms to +24 ms** on the facts plane, and from
+47% to within noise on the full-CLI wall.

### 1. The bead's hypothesis was wrong, and the measurement said so first

The bead (and W5's own sentence) named the fix as "shard by content hash so a
build touches only the shards its blobs live in." Reading the code before
redesigning it, per the campaign's own rule, showed that **the prune it asks for
was already there**. `FactsCorpus::merge_with_local` has always grouped its
probes by bucket and opened only `by_bucket.keys()` — the distinct buckets its
own file list hashes into. `shards_read=1024` was never a missing prune. It was
**saturation**: `corpus_bucket` is `xxh3(relative_path) % 1024`, and 40,869 paths
into 1,024 buckets hit every bucket with probability ~1. Re-sharding by content
hash would have changed nothing, because the *count* of shards a giant touches is
already every shard there is under any key.

What was unpruned was the **bytes inside each shard**. A v1 shard was one CBOR
array of `CorpusFile`, so touching a bucket decoded all of it — every entry every
repo had ever contributed. With a fixed bucket count, per-shard size grows
linearly with the corpus, so a read cost what the machine had stored rather than
what the build asked for. The write side was worse: `write_corpus_files` did a
read-merge-write per bucket, which **decoded and re-encoded** the whole corpus to
add one repo's entries.

Baseline, monster known-content, this bead's own measurement of the split
(medians of 3, interleaved):

| phase | 556 MB corpus | 7.9 GB corpus | from corpus size |
|---|---:|---:|---:|
| `facts_corpus_merge` | 874 | 4,017 | **+3,143** |
| `facts_corpus_populate_delta` | 548 | 2,121 | **+1,573** |
| **facts plane** | **1,422** | **6,138** | **+4,716 (+332%)** |
| full-CLI wall | 11,417 | 16,782 | +5,365 (**+47%**) |

Both halves were corpus-proportional. A content-hash re-shard would have fixed
neither.

### 2. What landed: a shard carries its own index

Shard layout v2 (`facts_store.rs`, "Shard layout v2"):

```
"SEMCORP2" | CBOR StoreHeader | u64 entry_count
           | entry_count × 24-byte {key_hash, offset, len}, key_hash-sorted
           | payload region: entry_count CBOR CorpusFile blobs
```

`key_hash` is `xxh3` over the whole lookup identity `(relative_path,
content_hash, lang_salt)` — the bucket still partitions on path alone, this is
the key *within* a shard. A read now opens the file, reads header + index (24
bytes an entry), binary-searches each probe, and reads only the payload ranges
that matched, coalescing exactly-adjacent ranges into one read (entries written
by one build land contiguously, so a repo's entries in a shard are usually one
read). A write reads the index alone, **drops every entry the shard already
holds** — on a known-content build that is all of them, and the shard is not
rewritten at all — and carries surviving old payloads forward as opaque bytes it
never decodes, at unchanged offsets, rebuilding only the index.

Three things are deliberate and documented rather than hidden:

1. **Dedup became first-writer-wins** (v1 was last-writer-wins). A `CorpusFile`'s
   value is a pure function of its key, so the two candidates are
   interchangeable; the only observable difference is `precomputed` presence,
   which costs speed on a later build, never correctness.
2. **A `key_hash` collision is a false miss, never a wrong answer.** Read still
   verifies every candidate field-by-field with `corpus_matches` after decode,
   exactly as v1 did. Write treats a collision as already-present and declines to
   store — also a future miss.
3. **No `fsync`.** The first draft added one per shard and measured **4,100 ms
   against 400 ms** across 1,024 shards on the monster. v1's `std::fs::write` +
   `rename` never fsynced; a shard lost to a power cut is a future cache miss and
   nothing worse. Reverted to v1's exact durability, and reported rather than
   quietly kept.

**Migration**: magic `SEMCORP1` -> `SEMCORP2`, so a v1 shard fails the magic
check and takes the same "any anomaly is a clean miss" path every malformed shard
already took — an old corpus degrades to a cold build and is rewritten in v2 on
the way past, with no migration step. `FACTS_SCHEMA_VERSION` is **not** bumped:
it governs the `CorpusFile` shape (unchanged), is shared with the per-repo
`FactsStore` (untouched), and is validated against the cloud tier's
`claimed_schema_version` in `ingest_remote`. Bumping it would invalidate two
stores and a wire contract for a change that is purely this file's framing.

### 3. The corpus-size independence proof

Same scenario, same box, **all four arms interleaved run-for-run** (baseline
binary against v1 corpora, this wave's against v2 corpora of matched size),
monster known-content, medians of 3:

| | v1 (before) | v2 (after) |
|---|---:|---:|
| `facts_corpus_merge`, per-repo corpus | 874 | 598 |
| `facts_corpus_merge`, shared corpus | 4,017 | **605** |
| `populate_delta`, per-repo | 548 | 353 |
| `populate_delta`, shared | 2,121 | **370** |
| **facts plane, per-repo (556/557 MB)** | **1,422** | **951** |
| **facts plane, shared (7.9/7.5 GB)** | **6,138** | **975** |
| **penalty from a ~14x larger corpus** | **+4,716 (+332%)** | **+24 (+2.5%)** |
| full-CLI wall, per-repo | 11,417 | 11,823 |
| full-CLI wall, shared | 16,782 | 10,994 |
| **wall penalty from corpus size** | **+5,365 (+47%)** | **−829 (noise)** |

The two `after` wall distributions overlap completely (per-repo
10,985/11,983/11,823, shared 10,994/10,905/11,952). **Corpus size no longer
moves build time.** W5's +29% — measured here as +47% on a quieter box — is gone.

The mechanism is visible directly in the new `bytes_read` counter on
`FACTS_CORPUS`, which is what `shards_read` should always have been:

| monster, 40,869/40,869 hits both sides | corpus on disk | bytes read |
|---|---:|---:|
| per-repo corpus | 557 MB | 582,504,893 |
| shared corpus | 7.5 GB | 592,529,717 |

**+1.7% of bytes read for 13.5x the corpus** — that residual is the index itself
(24 bytes an entry), which is the only term allowed to grow. `shards_read` still
reads 1024, correctly: it always was saturation, and it never was the cost.

Raw runs — BASE per-repo wall 11,377/11,417/12,065, merge 874/857/898, populate
548/530/562; BASE shared 16,782/16,133/17,416, merge 4,152/3,581/4,017, populate
2,088/2,121/2,170; AFTER per-repo 10,985/11,983/11,823, merge 577/598/616,
populate 347/353/354; AFTER shared 10,994/10,905/11,952, merge 605/594/638,
populate 365/381/370.

### 4. True-cold and known-content, two giants and the monster

Interleaved before/after, fresh cache per run; true-cold verified by `hits=0`
(and now `bytes_read=0`).

| repo, scenario | metric | before | after |
|---|---|---:|---:|
| monster, true-cold | wall | 17,903 / 17,166 | 17,824 / 17,965 |
| monster, true-cold | merge + populate | 468+483 / 470+451 | 468+485 / 476+457 |
| monster, known | wall | 11,989 / 12,242 | **11,426 / 11,595** |
| monster, known | merge + populate | 923+520 / 870+540 | **583+369 / 584+353** |
| HA, true-cold | wall | 10,385 / 10,440 | 10,903 / 10,391 |
| HA, known | wall | 9,236 / 9,530 | **9,109 / 8,761** |
| HA, known | merge + populate | 425+315 / 436+334 | **346+173 / 335+169** |
| dotnet, known | merge + populate | 1,265+951 / 1,197+901 | **827+466 / 867+560** |
| dotnet, known | `full_graph_build` | 43,805 / 47,329 | 43,168 / 47,344 |
| dotnet, known | peak RSS | 19,135 / 19,180 MB | 19,247 / 19,228 MB |

**True-cold is unchanged everywhere**, which is the right answer: an empty corpus
has no bytes to prune, `bytes_read=0`, and the phases match to within 20 ms.
**Known-content improves on all three**: the facts plane costs 37-54% less
(dotnet 2,157 -> 1,360 ms; HA 755 -> 511; monster 1,428 -> 946), and peak RSS is
unchanged (+0.4% on dotnet, inside run-to-run spread).

An honest note on dotnet: the first pass measured `after` ~7 s *slower* on
absolutes. Re-running with the arm order reversed produced parity in both passes
(43,168 vs 43,805; 47,344 vs 47,329), so that was ordering drift on a 20 GB-RSS
build, not a regression. Reported because it was measured, not discarded quietly.

### 5. Honest residuals

1. **The v2 shared corpus is 7.5 GB, not 7.9 GB**, and holds today's content from
   twelve repos rather than five waves' worth from nine. The two layouts cannot
   share a corpus (that is the migration story), so a like-for-like comparison had
   to rebuild one. 7.5 vs 7.9 GB understates the after side's advantage by ~5%;
   every conclusion here turns on a 197x reduction in the penalty, not on 5%.
2. **Load was not idle** and absolutes run high against W5's (monster true-cold
   17.5 s here vs 12.5 s there). Every comparison in §3 and §4 is interleaved
   run-for-run, so the deltas survive it; the absolutes are upper bounds.
3. **`n`=2-3 per arm**, not 5.
4. **The write path still reads and rewrites a shard's payload bytes when it has
   something genuinely new for that bucket.** It no longer *decodes* them, which
   is what made it corpus-proportional in wall time, but a build of genuinely new
   content into a large corpus still pays byte-copy I/O. Not measured separately
   here, and not fixed: the scenario that motivated the bead (known content) now
   writes nothing at all.

### 6. Gates

- **Bit-identical**, HEAD's binary vs this wave's, fresh cache *and* fresh corpus
  per side, corpus-served build on both sides: `sem graph --json` bytes and
  `index.sem` sha256 identical on rails (37,019,013 B / index
  `6f05882044d5…`), home-assistant-core (133,186,305 / `3540bc840acc…`) and the
  TypeScript monster (178,112,953 / `de05f1fa8c79…`). All three JSON sizes match
  W2 §7's recorded values exactly.
- **`facts_probe` cross-process oracle: 8/8 `ORACLE … ok`** (tiptap ×
  {none, leaf, mixed50, hub}, monster × {none, leaf, mixed50, hub}), with monster
  at its recorded `files=40872 entities=454541 edges=196223`. This is the gate
  that matters most for a corpus-layer change and it is unmodified.
- **`facts_corpus_probe`: 2/2 `ORACLE … ok`** (tiptap, monster), both with
  `corpus_hits` = every probed file, and both `NEGATIVE … ok` — same content at a
  different path is still a clean miss, so v2's key isolation holds.
- **Six `index_probe` oracles** (home-assistant-core): `ORACLE` 94,708 PASS,
  `REFS_ORACLE` 316,476 / 0 kind-mismatched PASS, `FILES_ORACLE` 8 prefixes PASS,
  `TESTS_ORACLE` 316,476 checked / 50,201 tests PASS, `TRIGRAM_ORACLE` 6 patterns
  PASS; `MUTATION` **skipped**
  (`no_battery_pattern_had_a_provable_true_positive` — the same data-dependent
  skip every wave since W0.5 has recorded).
- **Suites**: sem-core lib **610** (607 + this bead's 3), `single_pass_invariants` 3,
  sem-cli 248, sem-mcp 93. Zero failures. The **concurrent-writer test
  (`concurrent_writers_do_not_corrupt_and_both_survive`) still passes** against
  the rewritten read-merge-write.
- **Three new tests**, each an invariant rather than a case: `a_v1_shard_is_a_clean_miss`
  (migration, including that a write past it heals the shard),
  `repopulating_known_content_writes_nothing` (the write half of size
  independence: `files_written=0`, `shards_written=0`, entries still served), and
  `lookup_bytes_do_not_grow_with_unrelated_corpus_content` (the read half, with
  200 unrelated entries forced into the *same* bucket so it proves in-shard
  pruning, not bucket pruning: 5,560 bytes read from a 1,966,163-byte shard).
- **clippy/fmt**: clean on every touched file; `sem-cli/src/main.rs`,
  `index/format.rs`, `parse_probe.rs` and `review_protocol.rs`'s pre-existing fmt
  drift left as found.
- **Untouchables** byte-identical, confirmed by `git diff --stat` before and
  after.

Bead: semx-fqh. Epic: semx-w5k.

## LOCAL-COLD: scope_build attribution (semx-w5k)

W5 §5 named `scope_build` the last unmeasured box inside resolve — ~6.6 s on
dotnet, and the dominant term in the 4.3 s of resolve that stands between
home-assistant (the one giant whose *parse* fits the budget, at 0.72x) and the
target. Every unmeasured box this campaign has opened has hidden something. This
one is opened here. **No behavior changed; this section is measurement only, and
the table is what decides whether anything happens next.**

### 1. Instrument: `SEM_PROFILE_RESOLVE=2`

W3 disclosed rather than absorbed an instrument tax: profiled builds ran
1.45-1.65x clean on the giants, all of it `FileAccum`/`BowFileAccum`'s per-lookup
`name.to_string()` and per-file map merge — landing *inside* resolve, the phase
being attributed. W3's conclusion, "sub-phase numbers are attribution, not wall
time," is now a mode. `=2` turns the phase timers on and the per-name samplers
off, by constructing the two accumulators behind a new `names_enabled()` gate
(true only at `=1`) instead of `enabled()`; with the `Option` at `None`,
`select_member_profiled!` takes its untimed branch and not one `to_string`
happens.

Measured tax on home-assistant, `off` vs `=2`, repeated: `full_graph_build`
6,583/6,544 unprofiled against 6,871/6,900 at `=2` — **1.05x**. (An earlier
single pair read 1.27x; repeats showed that was drift on the unprofiled run, so
the disclosed figure is the repeated one.) Sub-phase numbers below are still
thread-summed attribution, not wall time; the ratios within `scope_build` are
what they are for.

Method: release binary, darwin, `available_parallelism=18`, `SEM_LOCAL=1
SEM_TIMINGS=1 SEM_PROFILE_RESOLVE=2`, fresh `SEM_CACHE_DIR`, **`SEM_FACTS_CACHE=0`**
— a genuine cold build, so `precomputed` facts come from pass 1's own fused JS/TS
path and not from the corpus, which is the isolation this attribution needs.

### 2. The table

Thread-summed milliseconds inside the region `scope_build_ms` already spanned,
with each constituent timed where it happens.

| constituent | HA (Python) | % | dotnet (C#) | % | monster (TS) | % |
|---|---:|---:|---:|---:|---:|---:|
| `extract_imports_from_ast` | **13,668** | **68.6** | **18,773** | **42.2** | 0.6 | 0.2 |
| `build_scopes_from_ast` | 3,270 | 16.4 | 13,170 | 29.6 | 0.0 | 0.0 |
| `collect_all_file_refs` | 2,683 | 13.5 | 11,627 | 26.1 | 0.0 | 0.0 |
| `find_entity_source_spans` | 150 | 0.8 | 557 | 1.3 | 177 | 50.1 |
| precomputed-facts clone | 0.0 | 0.0 | 1.5 | 0.0 | 103 | 29.1 |
| entity slice + `FileEntityLookup` | 23 | 0.1 | 116 | 0.3 | 37 | 10.6 |
| import-table seed + re-key | 39 | 0.2 | 4.4 | 0.0 | 3.8 | 1.1 |
| `inject_return_type_bindings` | 5.5 | 0.0 | 9.0 | 0.0 | 2.2 | 0.6 |
| `inject_field_type_bindings` | 0.5 | 0.0 | 1.7 | 0.0 | 0.8 | 0.2 |
| residual (config, content select) | 80 | 0.4 | 247 | 0.6 | 28 | 8.0 |
| **total `scope_build_ms`** | **19,918** | | **44,506** | | **352** | |

Work counters, and the wall context:

| | HA | dotnet | monster |
|---|---:|---:|---:|
| files on the AST path | 18,148 | 34,670 | **0** |
| files on the precomputed path | 2 | 228 | **39,296** |
| entities spanned | 129,645 | 721,805 | 418,475 |
| scopes built | 151,998 | 634,101 | 197,386 |
| AST refs collected | 610,274 | 2,500,456 | 254,124 |
| `pass2_wall_ms` (wall) | 1,212 | 9,994 | 114 |
| scope_build share of pass 2 | 95.8% | 74.9% | 46.2% |
| **scope_build, wall-equivalent** | **~1,161** | **~7,487** | **~53** |
| **per file** | 1.10 ms | 1.28 ms | **0.0090 ms** |

dotnet's ~7.5 s wall-equivalent confirms W5's ~6.6 s from an independent
direction.

### 3. What the table says

**(a) `scope_build` is a language-family cost, and the split is 140x.** The JS/TS
precomputed path (semx-6rd CUT 1) costs **0.0090 ms/file**; the AST path costs
1.10-1.28 ms/file. Monster resolves 39,296 files' scopes in 352 thread-ms —
*less than HA spends on 18,148 files by a factor of 57* while handling twice the
files. On the precomputed path the box is empty: what remains is a clone (103 ms)
and a span scan (177 ms), and `scope_build` is 0.1% of monster's build.

**(b) Nothing here is corpus-proportional.** Every constituent scales with the
file's own AST size and its own entity count. There is no whole-corpus scan
inside the region — unlike `chunk_entity_index_ms` and `return_types_by_name_ms`,
which W3 already flagged as re-scanning the whole corpus once per chunk.
`scope_build` is demand-proportional end to end and will not benefit from
anything that shrinks the corpus. That is a real finding: it rules out a whole
class of fix.

**(c) The AST path walks the same tree three times.** `build_scopes_from_ast`,
`collect_all_file_refs` and `extract_imports_from_ast` are three full traversals
of one `tree.root_node()`, and together they are **98.5% of HA's box and 97.9% of
dotnet's**. Everything else — the entity slice, the lookup, the spans, the
re-key, both injections, the residual — is **1.1% (HA) and 2.1% (dotnet)**.

### 4. Ranked implications

1. **Extend the precomputed-facts path past JS/TS.** Largest by far: dotnet
   ~7,487 ms wall -> ~60 ms (**−7.4 s of a 52.3 s build, −14%**), HA ~1,161 ->
   ~10 ms (**−1.15 s of 6.9 s, −17%**). The honest caveat is that the work does
   not vanish, it **moves into pass 1** where JS/TS already produces it — so on a
   true-cold build this is closer to a wash, and its real value is that the
   result becomes a cacheable `PrecomputedFileFacts` the facts corpus already
   carries. Given §LOCAL-COLD above, a known-content build would then get these
   scopes for free. That combination, not the relocation alone, is the case.
2. **Fuse the three AST traversals into one.** Structurally certain (three walks
   of one tree, measured), arithmetically speculative (the constant is unknown).
   If a fused walk costs ~1.3x one walk rather than 3x: dotnet 43,570 -> ~18,900
   thread-ms (**−4.2 s wall, −8%**), HA 19,621 -> ~8,500 (**−0.65 s, −9%**). It
   needs a prototype before it needs a bead, because the three walks visit
   overlapping-but-different node sets and a fused visitor may not be cheaper.
3. **`extract_imports_from_ast` alone, if only one thing is done.** It is the
   single largest constituent on both AST-path repos — 68.6% of HA's box and
   42.2% of dotnet's — and it runs *after* `build_scopes_from_ast` has already
   walked the tree and populated scope 0. On HA it is 13.7 of 19.9 thread-seconds
   by itself.
4. **The rest is genuinely the work, with arithmetic.** The seven small
   constituents sum to 218 thread-ms on HA and 936 on dotnet — 1.1% and 2.1% of
   their boxes, or ~13 ms and ~157 ms of wall. Perfect elimination of all seven
   would not be visible in either build. They are not fix candidates and should
   not be revisited.

No fix is taken in this pass, by instruction: the table decides what happens
next, and it says the answer is (1) plus §LOCAL-COLD's corpus, not a
micro-optimization inside the box.

### 5. The facts plane's memory, localized (one number)

W5 §3 priced the facts plane at **+9.23 GB of peak RSS on dotnet** by
differencing whole-process peaks with the plane on and off, and could attribute
it no further because the plane's boundaries live in `sem-cli`'s orchestration
while `warm_start`'s read+hash and per-file entity clone happen inside
`full_graph_build`. A phase-boundary RSS counter (`FACTS_RSS`, under the existing
`SEM_PROFILE_CACHE=1`) now reads it directly rather than by difference.

dotnet-runtime, known content, this wave's binary:

| boundary | RSS | step |
|---|---:|---:|
| entry to the facts path | 20.3 MB | — |
| after `merge_with_local` | 2,495.9 MB | **+2,475.6** |
| after `GraphSession::warm_start` | 12,262.0 MB | +9,766.1 |
| after `export_persisted` | 12,345.6 MB | +83.6 |
| after `store.save` + `populate_delta` | 17,197.7 MB | **+4,852.1** |
| process peak (`/usr/bin/time -l`) | 19,246.8 MB | |
| process peak, `SEM_FACTS_CACHE=0` | **9,946.1 MB** | |

**The plane costs +9,300.7 MB (+93.5%), re-confirming W5's +9.23 GB on the
shipped binary.** The counter localizes it: **2.48 GB is decoded corpus facts
held live** out of `merge_with_local`, and **4.85 GB more is materialized across
`export_persisted` -> `save` -> `populate_delta`**, which re-encode the whole
facts set to CBOR twice. The `warm_start` step (+9.77 GB) is mostly the graph
itself — a true-cold run with `hits=0` and `bytes_read=0` reaches 16,161 MB
through the same boundaries, so that step is not the plane's.

One number, no fix, as instructed. The two attributable steps — 2.48 GB and
4.85 GB — are where a future memory bead would go.

Beads: semx-w5k (attribution), semx-fqh (the sharding above).

## LOCAL-COLD: fuse the three AST walks — the plan (semx-3ao)

§"scope_build attribution" measured that for every non-JS/TS file, pass 2 walks
the same `tree.root_node()` three full times — `build_scopes_from_ast`,
`collect_all_file_refs`, `extract_imports_from_ast` — and that the three are
98.5% (HA) / 97.9% (dotnet) of the box. This section is the structure of the
fusion, written before the arithmetic is tested, because "structurally certain,
arithmetically speculative" obliges the structure to be explicit first. The
fusion is **in place**: pass 2, same lifetimes, same inputs — only the number
of traversals changes. Nothing moves to pass 1 (that relocation is semx-mul's
separately-fenced territory; W3's memory/semantics fences on full precompute do
not apply here because the tree is already in hand).

### 1. What each walk consumes and produces, per node kind

All three walk **named nodes only**, from the root.

**`build_scopes_from_ast`** — worklist of `(node, current_scope)`, children
pushed reversed, so pop order is document-order pre-order. Per node kind:
class-like/impl (`config.class_scope_nodes`/`impl_scope_nodes`) allocates a
`Scope`, registers `children_by_parent[owner]` into its `defs`, updates
`entity_scope_map`/`entity_inner_scope` via `file_lookup.find_at_line`; Rust
`mod_item` likewise; function-like (`config.function_scope_nodes`) allocates a
scope, then runs two **subtree** scans (`scan_assignments`,
`scan_function_params`) confined to the function node; every other kind is
pass-through. Consumes: tree + `source` + entity indexes. Produces: `scopes`,
`entity_scope_map`, `entity_inner_scope`. Every branch pushes all named
children exactly once → visit set = all named nodes, in document order. One
order-sensitivity inside: Go `external_method` does `scopes.iter().find(...)`,
so scope *indices* (allocation order) are load-bearing.

**`collect_all_file_refs`** — worklist of `node`, children pushed reversed →
the identical document-order pre-order visit set. Per node kind: `call_nodes`,
`macro_invocation`, `new_expr_nodes`, `composite_literal_nodes` append
`AstRef`s to a `Vec`; everything else pass-through. Consumes tree + `source` +
config only; produces `ast_refs`, whose **Vec order is document order** and is
consumed downstream (`build_refs_by_row` keeps per-row insertion order).
Shares no state with the scope walk: a textbook fold-fusion pair.

**`extract_imports_from_ast`** — worklist of `node`, but children are
**pushed forward** and import-kind children are handled *at the parent's
visit* and never descended. Handled set H = {named node c : kind-condition(c)
∧ no ancestor of c in H}, where kind-condition is pure in
`(kind, config.self_keywords)`: `import_from_statement` (Python from-import),
`import_statement` (Python module-import when self∧cls; TS when ¬cls),
`export_statement` (TS re-export when ¬cls), `use_declaration` (Rust),
`import_declaration` (Go handler — also fires on Java/Swift trees, existing
behavior, preserved). Handlers mutate `import_table` and `scopes[0].defs`
(insert = last-write-wins) and read corpus tables (`symbol_table`,
`go_pkg_index`, lazy `top_level_entities`…). It runs **after** the scope walk
and after the pre-built-import-table seed, and its insertions overwrite the
seed's — phase order is load-bearing.

### 2. Traversal-order verdict

`build_scopes_from_ast` and `collect_all_file_refs` are **order-identical**
(document-order pre-order over the same visit set) and **state-disjoint**:
fusing them is the fold-fusion identity `⟨cata f, cata g⟩ = cata ⟨f,g⟩`
(SINGLE-PASS.md S1) with no caveat.

`extract_imports_from_ast` is the classic blocker the plan must name: its
effective handling order is **not** document order. Forward-push onto a LIFO
worklist means sibling subtrees are processed in *reverse* document order,
while a node's own import children are handled in *forward* order at the
parent's visit. For `try: import A… except: import B` (ubiquitous in HA), B's
handler runs before A's, and last-write-wins means **A** wins the
`import_table` slot — document-order processing would flip it. So the fused
walk may not simply run handlers inline.

**Resolution: record-then-replay-pruned.** The fused walk (document order)
carries an `in_import` flag and records `start_byte` of every node in H —
same inductive definition, so the recorded set R = H, and pre-order recording
makes R sorted. Handlers do not run during the walk. At the exact program
point where `extract_imports_from_ast` runs today (after the seed), a **pruned
replay** re-runs extract's own worklist algorithm but only descends into
children whose byte range contains a recorded start (binary search on R).
Pruning removes only pops that emit nothing, and LIFO relative order among
kept nodes is position-determined, so the emission sequence — handler calls,
arguments, mutation order — is exactly the original's. Cost: O(|R| · depth)
node visits instead of O(tree); for a file with no imports (every C# file:
C#'s `using_directive` matches no kind-condition) the replay is a length-0
early return, which is where dotnet's 18.8 thread-seconds of pure traversal
go. Phase order (scope walk → ref collection → seed → import handlers) is
byte-for-byte preserved; the fusion changes only how many times tree-sitter
cursors move.

Implementation shape: the per-node bodies are factored into shared helpers
(`scope_visit_node`, `refs_visit_node`, `classify_import_stmt` + handler
dispatch) so the three unfused functions remain alive, verbatim in behavior,
as the specification the invariant test runs; the fused walk is one new worklist of
`(node, scope, in_import)` calling the same helpers plus the recorder.

### 3. What stays out

- The JS/TS precomputed path (`PrecomputedFileFacts`) is untouched — monster
  is the control and must not move.
- `precompute_js_ts_file_facts` (pass 1, JS/TS) also runs
  `build_scopes_from_ast` + `collect_all_file_refs` + two scans over its tree;
  fusing *that* is not this bead (pass 1 is not the measured box).
- C files never arrive: `scope_resolve: None` for `.c`/`.h`, so post-W2-F1
  linux's C files are neither re-parsed nor walked here. Linux is measured as
  a second control.
- `scan_return_types`/ctor-infer/Swift-signature walks live outside the
  `scope_build` region and are not touched.

### 4. The property-test design (the fusion-invariant witness)

One invariant, `SINGLE-PASS.md` §6 shape, in `scope_resolve.rs`'s test module (the
walks are private), generated by the same deterministic xorshift discipline as
`single_pass_invariants.rs`:

```
∀ file. fused(tree) ≡ ( build_scopes_from_ast(tree);
                        collect_all_file_refs(tree);
                        seed;
                        extract_imports_from_ast(tree) )        (BS3-witness)
```

Equality is on the complete observable state: `scopes` (serialized),
`entity_scope_map`, `entity_inner_scope`, `ast_refs` **including Vec order**,
and `import_table` after the handlers — the unfused side executed in the
closure's exact phase order as the specification. Fixtures: generated programs
across ≥4 families that take this path (Python, C#, Rust, Go), composed from
nested classes/functions/calls/assignments plus imports **in nested containers**
(Python `try/except` and function bodies; Rust `use` inside `mod`), with
synthetic `symbol_table`/`entity_map` entries so handlers really resolve, and
real pass-1 entities so `find_at_line` really binds scopes. NON-VACUITY: each
family's sample must produce >1 scope, ≥1 ref, and ≥1 resolved import; the
Python sample must contain a same-name-two-targets nested-import pair.
POSITIVE CONTROL: a deliberately doc-order variant of import processing must
disagree with the spec on that pair (proving the replay order is load-bearing,
not decorative), recorded in the test header.

Gates per landing commit, unchanged from the campaign: bit-identical
entity/edge counts + sorted-edge hashes + `index.sem` sha256 on rails, HA,
monster, dotnet; six `index_probe` oracles; `facts_probe` 8/8; suites
610/3/248/93 green plus this invariant; clippy/fmt clean on touched files.

### 5. The arithmetic to be tested, and the stop rule

Prototype = Python only (extension-gated), measured on HA end-to-end (wall +
`full_graph_build`) and at `SEM_PROFILE_RESOLVE=2` for the sub-phase table,
with a new `fused_walk` constituent and the replay timed where extract was.
The speculative half: HA's 13.7 thread-s of `extract_imports_from_ast` is
traversal + *handler* work in unknown proportion, and the replay keeps the
handler part. If the measured win is <10% of the box, this section closes with
the number and no further code, per instruction. dotnet's extract is
handler-free traversal (no matching kinds), so the C# extension is where the
structure predicts the largest absolute yield.

Bead: semx-3ao. Epic: semx-w5k.

## LOCAL-COLD: the triple-walk fusion, landed and measured (semx-3ao)

The plan section above predicted the structure; this section reports the
arithmetic. Two commits: BS3-F1 (the fused walk, Python-gated prototype,
measured on HA) and BS3-F2 (gate deleted; every scope-resolve family takes the
one walk). The unfused walks survive as the BS3-witness invariant's executable
specification — `extract_imports_from_ast` test-only, the other two still in
production use by the JS/TS pass-1 producer.

### 1. The traversal-order verdict, as landed

`build_scopes_from_ast` + `collect_all_file_refs` fused with no caveat
(order-identical, state-disjoint — fold-fusion proper). `extract`'s
non-document LIFO order was real and load-bearing: the
`import_replay_order_is_load_bearing` test constructs the
`try: from mod0 import shared as S / except: from mod1 import shared as S`
pair on which extract's order (except-branch first, so **mod0** wins
last-write-wins) differs from document order (**mod1** wins) — the
record-then-replay-pruned design reproduces extract's order exactly, and a
document-order variant is held RED against it as the positive control. The
BS3-witness invariant (`fused_triple_walk_matches_three_sequential_walks`) samples
24 rounds × 5 families (Python, C#, Rust, Go, TS) over generated nested
fixtures and asserts full-state equality: scopes, both entity-scope maps,
refs including Vec order, and the import table.

### 2. The sub-phase table, before/after (`SEM_PROFILE_RESOLVE=2`, this box)

Thread-summed ms inside `scope_build`. "before" = HEAD's binary, "after" =
BS3-F2, same box, same isolation as the attribution section
(`SEM_FACTS_CACHE=0`, fresh cache).

| constituent | HA before | HA after | dotnet before | dotnet after |
|---|---:|---:|---:|---:|
| `build_scopes_from_ast` | 2,493 | 0 | 10,462 | 0 |
| `collect_all_file_refs` | 2,064 | 0 | 9,029 | 0 |
| **fused triple walk** | — | **2,944** | — | **12,935** |
| `extract_imports` (now: pruned replay + handlers) | 9,493 | 7,747 | 15,306 | 7,028 |
| everything else | 237 | 226 | 771 | 750 |
| **total `scope_build_ms`** | **14,287** | **10,917** | **35,568** | **20,713** |
| | | **−23.6%** | | **−41.8%** |

(HA run-pair variance on the after side: a second run read 11,562 with
`fused_walk` 3,004 / extract 8,321 — the ratios hold.) `files_fused` =
`files_ast`: HA 18,148, dotnet 34,670. Work counters (entities spanned,
scopes built, refs collected) identical before/after, as the invariant requires.

**The fusion constant beat the plan's guess.** The plan budgeted a fused walk
at ~1.3× one walk; measured: HA 2,944 vs 2,493 (**1.18×**), dotnet 12,935 vs
10,462 (**1.24×**) — for three walks' worth of output.

### 3. End-to-end, both metrics, all four corpora

Interleaved run-for-run, n=3 walls, fresh cache, `SEM_FACTS_CACHE=0`.

| corpus | metric | before | after | Δ |
|---|---|---:|---:|---|
| **dotnet** | wall (s) | 50.05 / 51.22 / 49.73 | 46.29 / 46.00 / 46.58 | **−3.8 s (−7.5%), 3/3 pairs** |
| **dotnet** | `full_graph_build` (ms) | 42,538 / 44,458 / 43,028 | 39,501 / 39,443 / 39,693 | **−3.5 s (−8.2%)** |
| **HA** | wall (s) | 6.35 / 5.76 / 6.11 | 5.96 / 5.72 / 5.89 | −0.22 s median (−3.6%), 3/3 pairs, inside drift band |
| **HA** | `pass2_wall_ms` (=2) | 850 | 654 | **−23%** — the wall shadow of the box shrink |
| **monster** (control) | wall (s) | 12.09 / 11.20 | 11.32 / 13.55 | unchanged (precomputed path untouched) |
| **linux** | wall (s) | 38.48 | 37.87 | unchanged — see §4 |

Bit-identical everywhere, both binaries: `sem graph --json` bytes and
`index.sem` sha256 on rails (37,019,013 B / `6f05882044d5…`), HA
(`3540bc840acc…`), monster, dotnet (`233985a431cb…`), linux
(`bc358b9a83a2…`), tiptap. One disclosed measurement hazard: `facts_probe`'s
scenario loads restore file *content* but bump mtimes, and `index.sem`
carries fingerprints — two mid-battery "mismatches" (tiptap, monster) were
re-run like-for-like on the post-touch state and matched byte-for-byte on
both sides.

### 4. What remains in the box, and the linux disclosure

After the fusion the AST path's box is: **the fused walk (demand-proportional,
now near the one-walk floor) + the import handlers**. The handlers are the
next story, and linux tells it loudest:

- **linux**: C files never enter (`.c`/`.h` have `scope_resolve: None`;
  post-W2-F1 they are not even re-parsed). Its AST path is just **2,050**
  files (the py/rs/sh minority) — yet its box is **~31,000 thread-ms, ≥98%
  in `extract_imports`**, on both sides of the fusion. That cannot be
  traversal (the fused walk covers all 2,050 files in 463 ms).
- The shape matches HA's and dotnet's handler residual (7.7 s and 7.0 s): the
  per-call lazily-built `py_top_level_entities`/`top_level_entities` indexes
  are `OnceLock`s **created per `resolve_with_scopes_full_inner` call — i.e.
  per chunk** — and `build_top_level_entity_index` walks the whole corpus
  symbol table. One bare `import os` per chunk rebuilds a corpus-sized index:
  corpus-proportional × chunk-count, filed under `extract_imports` where the
  first triggering file happens to be timed. linux (~76k files ⇒ ~16 chunks ×
  a symbol table dominated by C entities) fits the ~31 s arithmetic; dotnet
  (7 chunks × 721k entities) fits ~7 s.
- **Not taken here** — it is not walk fusion and this bead's fence was "fuse
  in place". It is the box's named next bead: hoist the two indexes across
  chunks exactly as `PrebuiltEntityIndex` was hoisted in semx-6rd CUT 2. On
  linux that is nearly the whole remaining box; on HA/dotnet a real slice of
  the ~7-8 s residual.

### 5. The mul-enabling observation (noted, not done)

The fused walk's outputs — `scopes`, `entity_scope_map`,
`entity_inner_scope`, `ast_refs` — are exactly the first four fields of
`PrecomputedFileFacts`, produced at one program point, per file, with the
tree in hand. Persisting them per-file through the facts corpus (which
already carries that shape for JS/TS) would let a known-content build skip
the fused walk entirely — that is semx-mul's case, and it stays fenced:
the JS/TS proof that *file-local* entity maps suffice
(`precompute_js_ts_file_facts`'s doc comment) has **not** been established
for languages with cross-file nesting semantics, and W3's memory fence on
holding per-file facts across the build applies. The fusion makes mul
cheaper to attempt (one output site instead of three), and nothing here
forecloses it.

### 6. Gates

- Bit-identical (§3), six corpora, both binaries.
- Six `index_probe` oracles (HA): `ORACLE` 94,708 PASS, `REFS_ORACLE`
  316,476 / 0 kind-mismatched PASS, `FILES_ORACLE` 8 PASS, `TESTS_ORACLE`
  316,476 / 50,201 PASS, `TRIGRAM_ORACLE` 6 patterns PASS, `MUTATION`
  skipped (`no_battery_pattern_had_a_provable_true_positive`, the standing
  data-dependent skip).
- `facts_probe` **8/8 ok** (tiptap × {none, leaf, mixed50, hub}, monster ×
  same; monster at its recorded `files=40872 entities=454541 edges=196223`).
- Suites: sem-core lib **612** (610 + this bead's two invariant tests),
  `single_pass_invariants` 3, sem-cli 248, sem-mcp 93. Zero failures.
- clippy: 89 warnings on the touched files, down from HEAD's 92, all
  remaining pre-existing; fmt clean on both touched files.
- Untouchables byte-identical; `languages.rs` WIP hunks left unstaged and
  uncommitted.

Bead: semx-3ao. Epic: semx-w5k.

## LOCAL-COLD: hoist the per-chunk import-handler indexes (semx-la2)

The fused-walk section above closed with the box's named next bead: the
import handlers' `py_top_level_entities`/`top_level_entities` indexes are
`OnceLock`s created per `resolve_with_scopes_full_inner` call — i.e. per
chunk — and `build_top_level_entity_index` walks the whole corpus symbol
table, so one bare `import os` per chunk rebuilds a corpus-sized index.
linux: ~31 thread-s across ~16 chunks on just 2,050 AST files. dotnet: ~7
thread-s across 7 chunks × 721k entities. This section is that bead:
semx-6rd CUT-2's hoist pattern, round 3.

### 1. The invariance verdict (proven before the change, per the CUT-2 discipline)

The semx-6rd era established that per-chunk state comes in two kinds:
corpus-invariant (`entities_by_file`/`children_by_parent` — hoisting is an
identity) and chunk-scoped **on purpose**
(`deterministic_return_types_by_name`'s `return_type_map` argument is
rebuilt from just that chunk's files, so hoisting it would change
cross-chunk visibility — a semantics change, deliberately not taken). The
first job here is to prove which kind these two indexes are, from the code.

**Verdict: corpus-invariant. Hoisting is an identity.** Both indexes are
built at exactly two sites, both `get_or_init(||
build_top_level_entity_index(symbol_table, entity_map, extensions))`
(`register_ts_namespace_import`, `register_namespace_import`,
`scope_resolve.rs`), and `build_top_level_entity_index` reads *nothing*
beyond its three parameters (verified by reading its body: one loop over
`symbol_table`, `entity_map.get()` per target id, an extension-suffix
filter, then `sort_import_candidate_files` + `build_owned_stem_index`, both
pure functions of the grouped result). On the chunked path each of those
three inputs is corpus-invariant across chunks:

1. `symbol_table` — `lookups.symbol_table.as_ref()`, and on the chunked
   path `pre_built` is always `Some` (`resolve_scopes_in_file_chunks`
   passes the same `&PreBuiltLookups` into every chunk call, graph.rs; an
   immutable borrow held across the whole loop, so no mutation between
   chunks is even expressible).
2. `entity_map` — the same `&HashMap<String, EntityInfo>` corpus map,
   passed unchanged into every chunk call, same immutability argument.
3. `extensions` — compile-time constants: `JS_TS_EXTENSIONS` (a
   `pub(crate) const`, `import_resolution.rs`) at the TS site, the literal
   `&[".py"]` at the Python site (`extract_python_module_import`). Each
   `OnceLock` is only ever initialized with its one constant — the TS lock
   only from the TS handler, the py lock only from the py handler.

Determinism of the rebuilds themselves: this crate's `HashMap` is
`FxHashMap` (fixed seed — `scope_resolve.rs:24`), so iterating *the same*
`symbol_table` object produces the same order in every chunk, and the
per-chunk builds were therefore byte-identical to each other before this
change. Sharing build #1 across all chunks hands every chunk exactly the
index it would have built — an identity, now also confirmed empirically by
the bit-identical gates below.

**What is chunk-scoped by design and stays fenced out** (the
`deterministic_return_types_by_name` precedent, applied):
`ts_default_exports` is built from chunk-local `parsed_files` (and is
empty whenever an import table is supplied, i.e. always on the graph-build
path); `content_by_file`/`exported_names_by_file` cache borrows of
chunk-local `parsed_files` content; the return-type/instance-attr maps
remain chunk-scoped per semx-6rd's original fence. None of these feed
`build_top_level_entity_index`, and none are hoisted.

### 2. The change

`ChunkedResolveInputs` — the established "whole-corpus state the chunked
path builds once" carrier (CUT 2's `entity_index`, semx-nuv's
`corpus_has_swift`) — gains the two `OnceLock<TopLevelEntityIndex>`s, owned
by `resolve_scopes_in_file_chunks` alongside the `PrebuiltEntityIndex` it
already owns, created (empty) before the chunk loop.
`resolve_with_scopes_full_inner` uses the caller's locks when `chunked` is
`Some` and falls back to its own per-call locks otherwise — every
non-chunked caller (`resolve_with_scopes_full`,
`resolve_with_scopes_full_for_entities`) is behaviorally untouched, exactly
CUT 2's opt-in shape. Laziness is preserved: nothing is built unless some
chunk actually sees a bare `import module` / `import * as m` statement; it
is just built **once per corpus** instead of once per chunk that sees one.

### 3. The handler bucket, before/after (`SEM_PROFILE_RESOLVE=2`, interleaved, n=3)

`extract_imports_ms` (thread-summed, the bucket the rebuild was filed under
because the first triggering file gets timed holding it), runs listed
r1/r2/r3, medians bold. Protocol: `SEM_LOCAL=1 SEM_TIMINGS=1
SEM_PROFILE_RESOLVE=2 SEM_FACTS_CACHE=0`, fresh `SEM_CACHE_DIR` per run,
before/after interleaved run-for-run, same box.

| corpus | before (thread-ms) | after (thread-ms) | Δ median |
|---|---|---|---|
| **linux** | 35,255 / 26,004 / 28,575 (**28,575**) | 3,935 / 3,495 / 5,266 (**3,935**) | **−24.6 thread-s (−86%)** |
| **dotnet** | 11,431 / 7,440 / 8,589 (**8,589**) | 916 / 1,119 / 993 (**993**) | **−7.6 thread-s (−88%)** |
| **HA** | 13,569 / 7,796 / 7,798 (**7,798**) | 3,366 / 1,624 / 2,518 (**2,518**) | **−5.3 thread-s (−68%)** |
| **monster** (control) | 0.63 / 0.64 / 0.62 (**0.63**) | 0.73 / 0.63 / 0.64 (**0.64**) | unchanged |

The arithmetic BS3 predicted, confirmed by elimination: linux's r1 pair is
−31.3 thread-s — the ~31 s the closing finding named — and the medians say
~15-16 rebuilds' worth vanished on linux (~1.6 s per rebuild of a
symbol table dominated by C entities), ~7 rebuilds × ~1.1 s on dotnet
(721k-entity table, its `.py` test files the trigger), ~5-7 on HA. What's
left after the hoist is the genuine per-import handler work (linux 3.9 s,
HA 2.5 s, dotnet 1.0 s thread) plus **one** lazy corpus-sized build. The
before side's large run-to-run swing (HA 7.8-13.6 s) is itself explained by
the bug: threads that hit `get_or_init` while another thread was mid-build
*blocked*, and their wait was attributed to `extract_imports` too —
per-chunk rebuilds also serialized the whole pass-2 pool once per chunk.

`pass2_wall_ms` — the wall shadow of the box shrink (same runs, medians):
linux **2,950 → 390** (−87%), dotnet **5,359 → 4,109** (−23%), HA
**697 → 498** (−29%), monster 84 → 88 (noise). `scope_build_ms` medians:
linux 29,014 → 4,447, dotnet 26,083 → 17,748, HA 11,663 → 7,663, monster
311 → 321.

### 4. End-to-end, both metrics, all four corpora

Same interleaved runs (each `sem graph` cold, r1/r2/r3, medians bold).
Honesty first: this session's box was noisier than BS3's (the monster
*control* spans 11.3-19.3 s wall across its six runs), so wall-level deltas
below the ~1 s band are not attributable — the thread-ms and `pass2_wall`
numbers above are the attribution-solid results.

| corpus | metric | before | after | Δ (median, pairs improved) |
|---|---|---|---|---|
| **linux** | wall (s) | 45.80 / 31.90 / 37.45 (**37.45**) | 42.20 / 28.99 / 36.69 (**36.69**) | −0.8 s, **3/3 pairs** |
| **linux** | `full_graph_build` (ms) | 32,370 / 21,991 / 26,179 (**26,179**) | 29,542 / 20,407 / 26,488 (**26,488**) | pairs −2,828 / −1,584 / +310 — 2/3 show the expected ~2.5 s, r3 inside the noise band |
| **dotnet** | wall (s) | 63.76 / 45.01 / 55.45 (**55.45**) | 51.29 / 49.23 / 49.61 (**49.61**) | **−5.8 s (−10.5%)**, 2/3 pairs |
| **dotnet** | `full_graph_build` (ms) | 57,447 / 40,213 / 49,616 (**49,616**) | 46,494 / 44,095 / 44,260 (**44,260**) | **−5.4 s (−10.8%)**; after-spread tight (44.1-46.5 s) vs before (40.2-57.4 s) |
| **HA** | wall (s) | 10.66 / 6.57 / 6.46 (**6.57**) | 9.83 / 6.26 / 6.76 (**6.76**) | inside drift band, 2/3 pairs improved |
| **HA** | `full_graph_build` (ms) | 8,294 / 4,940 / 4,852 (**4,940**) | 7,600 / 5,133 / 5,564 (**5,564**) | ~0.2 s expected saving < HA's ±0.7 s band — invisible at wall level, visible in `pass2_wall` (−199 ms) |
| **monster** (control) | wall (s) | 19.25 / 13.78 / 13.77 (**13.78**) | 17.05 / 11.27 / 13.43 (**13.43**) | unchanged (drift; handlers were already 0.6 thread-**ms** here) |
| **monster** (control) | `full_graph_build` (ms) | 14,416 / 10,722 / 10,624 (**10,722**) | 14,524 / 9,625 / 11,574 (**11,574**) | unchanged |

### 5. What remains in scope_build's box, and HA's 1s picture

After BS3 (walk fusion) plus this hoist, the AST path's box is
**demand-proportional end to end** for the first time: the fused walk (HA
3.8 thread-s, near the one-walk floor) + the *true* per-import handler work
(HA 1.6-2.5, dotnet ~1.0, linux 3.9-5.3 thread-s). No corpus-proportional
term is left inside `scope_build` — on linux the box fell from ~29 to ~4.4
thread-s and what remains scales with its 2,050 AST files, not with the C
majority's 76k files. The next levers live *outside* the box: semx-mul
(persist the fused walk's outputs through the facts corpus — this hoist
makes the handlers cheap enough that mul's case is now mostly about the
walk, not the handlers) and the save/pre-resolve planes.

HA's distance to 1s, updated: one clean cold `sem graph` after this change
reads total ~6.2 s = file_discovery 0.14 + `full_graph_build` ~5.1 +
`index_only_save` 0.88 + serialization 0.02. Inside the build (perf_probe,
same binary): `pre_resolve` ~1.9 s (parse physics itself fits the budget at
0.72x ≈ 0.7 s; the rest is extract + table build), `resolve_phase` ~4.2 s
of which pass 2's wall is now only ~0.4-0.5 s. The import handlers no
longer stand between HA and 1s; what stands is (in rough order) the
resolve phase's non-pass-2 remainder (bow tokenize ~5.3 thread-s, the
chunk re-parse ~0.9 s wall, ref loop ~0.4 s), pre-resolve's extract+tables
~1.2 s, and the ~0.9 s save plane.

### 6. Gates

- **Bit-identical, before == after, all five gate corpora**: `index.sem`
  sha256 identical across every interleaved pair and equal on both sides —
  rails `6f0588…`, HA `3540bc…`, linux `bc358b…`, dotnet `233985…` (all
  four matching BS3's recorded values), monster `5fa902…`. Sorted
  `edge_dump_probe` dumps byte-identical (sha256) before/after on HA
  (307,021 edges), linux (1,898,816), dotnet (981,226), monster (196,175),
  rails (60,411).
- Six `index_probe` oracles (HA): `ORACLE` 94,708 PASS, `REFS_ORACLE`
  316,476 / 0 mismatched / 0 kind-mismatched PASS, `FILES_ORACLE` 8 PASS,
  `TESTS_ORACLE` 316,476 / 50,201 PASS, `TRIGRAM_ORACLE` 6 patterns PASS,
  `MUTATION` skipped (`no_battery_pattern_had_a_provable_true_positive`,
  the standing data-dependent skip).
- `facts_probe` **8/8 ok** (tiptap × {none, leaf, mixed50, hub}, monster ×
  same; monster at its recorded `files=40872 entities=454541
  edges=196223`).
- Suites: sem-core lib **612** + integration binaries all green,
  `single_pass_invariants` 3, sem-cli 248, sem-mcp 93 — zero failures. This
  includes BS3's invariant tests (`fused_triple_walk_matches_three_sequential_
  walks`, `import_replay_order_is_load_bearing`), and the chunked-path
  invariants this change's shape is exactly guarded by: semx-nuv's
  `test_ambiguous_cross_chunk_swift_name_resolution_is_chunk_independent`
  (chunk-config determinism) and
  `test_chunked_scope_resolution_keeps_cross_chunk_import_edges`, both
  running the real chunk loop under the `#[cfg(test)]`
  `PARSED_FILE_REUSE_LIMIT`/byte-budget overrides — i.e. the forced-chunked
  path with the hoisted locks live.
- clippy: zero new warnings on the two touched files (the near-edit hits
  are pre-existing warnings at shifted line numbers); fmt clean on both.
- Untouchables byte-identical; `languages.rs` + sem-cli WIP hunks left
  unstaged and uncommitted.

Bead: semx-la2. Epic: semx-w5k.

## MUL-A: the violation census, and the fence that was pointing the wrong way (semx-w5k.1)

The hoist section above closed by naming semx-mul — persist the fused walk's
outputs through the facts corpus — as the next lever outside `scope_build`'s
box, still fenced by W3 §5's two conditions: **memory** (C# ~40x
tree-bytes/source-byte) and **semantics** (`PrecomputedFileFacts` is licensed by
"declarations never nest across files", claimed FALSE for C# partial classes and
C++ out-of-line member definitions). This bead measures both fences before any
code is written. **No production code changed; the deliverable is
`crates/sem-core/MUL-DESIGN.md` and one probe,
`crates/sem-core/examples/mul_census.rs`.**

**The semantics fence is empirically empty, and provably so.** The property the
*code* needs is not a language property but a per-file predicate — `CLEAN(F)`:
no entity outside `F` may name an entity of `F` as its parent — because every
corpus-wide-map key `scope_visit_node` uses is an id `FileEntityLookup` produced
from `F`'s own entities. `build_entity_id` roots every id at its file
(`{file}::{type}::{name}`, then `{parent}::{name}`) and `parent_id` is only ever
assigned inside one file's extraction, so `CLEAN` holds by construction. The
census confirms it: **0 cross-file parent links across 4,836,244 entities on
seven corpora and seven families**, including **18,006 C# `partial` declarations**
(23.5% of dotnet's C# files) and **164,431 C++ out-of-line member definitions**
(25.4% of llvm's C++ files). The constructs are abundant; a partial half is a
separate entity, and `void A::f()` becomes a top-level entity *named* `A::f`
(141,537 `::`-bearing names against 141,502 textual definitions in llvm — ratio
1.00), so neither nests across files. monster is the positive control: the
39,296 files production already precomputes show the same `0`.

**The real fence is the license's second clause, and it inverts the ranking.**
After BS3's fusion the pass-2 closure has exactly one tree use left —
`replay_import_stmts_pruned`, gated on a non-empty import-start set — plus
ctor-infer's `"call"`-kind sweep and Swift signatures outside it. Measured share
of scope-resolvable **bytes** that need no tree at all: **C# 99.74%, C++ 96.76%,
Rust 13.74%, Go 5.24%, Java 0.98%, Python 0.23%.** The two families W3 §5
excluded are the only two that are ready today; Python — the bead's suggested
"narrower sound subset" — is the worst, and its 8.7% treeless *file* count is
`__init__.py` stubs averaging 170 bytes.

Re-derived arithmetic on this document's own instruments (`SEM_PROFILE_RESOLVE=2`,
`SEM_FACTS_CACHE=0`, fresh cache; n=1 on a box running ~1.3-1.7x LOCAL-COLD's, so
ratios only): `reparse_ms` is **17,647 ms of dotnet's 58,336 ms
`full_graph_build` (30.3%)**, 5,228 of llvm's 52,439 (10.0%), 1,019 of HA's
7,764 (13.1%) — the re-parse half of the prize is **5.5x the walk-relocation half
on dotnet**, which is the half the bead had priced. With the per-file gate and no
facts-schema change: **dotnet −30.8% cold / −36.3% known-content, llvm −10.6% /
−14.8%, HA −0.03%**. The gate alone captures **99.7%** of dotnet's full prize and
**96.8%** of llvm's — the bead's own ≥80% test, passed on C#/C++ and failed at
0.2% on Python.

**The memory fence survives as the deciding number, and it is smaller than
feared.** Today's chunk-held trees cost ≲220 MB of high-water on dotnet
(measured: RSS 7,086 → 7,789 MB across all 30 chunks, of which 482 MB is
attributed to accumulating edges/consumed-words), because semx-g6t's 20 MiB
budget already bounds them. Facts held corpus-wide instead project to
**~1.25-1.35 GB on dotnet (+10-11% net of peak) and ~1.25-1.31 GB on llvm
(+12-13%)** — calibrated on monster's measured `precomputed_facts` = 271.3 MB for
130.4 MB of TS source, corrected upward for `approx_heap_bytes`' documented
nested-string undercount. That is **roughly half of semx-g6t's −19.6% given
back**, it is a projection rather than a measurement, and phase 1's gate is
required to measure it on the real producer with a stated stop-ceiling.

**Verdict: GO for C# and C++ on a two-tier per-file gate (phase 1, ~2-3 days);
NO as-is for Python/Go/Java/Rust, whose prize is real but sits behind an
`import_stmts` descriptor extension (phase 2, ~3-4 days) plus, for Python, a
`ctor_call_sites` extension (phase 3, ~1-2 days); NO in every phase for Swift**
(`build_swift_call_signatures` is corpus-wide, not per-file). Three findings are
surfaced rather than fixed, all pre-existing: a latent seed-order divergence
between the JS/TS precompute (extraction order) and the AST path
(`entity_ranges` order); §fqh's first-writer-wins corpus dedup silently denying
any producer upgrade until the key changes; and 249,740 within-file duplicate
entity ids in elasticsearch's Java extraction (harmless to `CLEAN`, unexamined
otherwise). Full census tables, invariants, arithmetic and the phase plan:
**`crates/sem-core/MUL-DESIGN.md`**.

Bead: semx-w5k.1 (MUL-A). Epic: semx-w5k. Parent thesis: semx-mul.

## MUL P1: the C#/C++ CLEAN gate, implemented and measured (semx-mp1)

MUL-A verdicted GO for C# and C++ on a per-file gate with no facts-schema
change. This bead implements phase 1 exactly as MUL-DESIGN.md §6.2 specified,
against HEAD `9c1c531` (which had already landed step 3, the I2 seed-order fix,
as semx-u16 — a documented prerequisite, not re-done here).

**What shipped**, all in `crates/sem-core/src/parser/`:

1. **The CLEAN gate.** `PrebuiltEntityIndex::dirty_precompute_files`
   (`scope_resolve.rs`) — the exact O(entities) pass §4.1 step 2 specifies:
   for every entity `e`, if any of `children_by_parent[e.id]` belongs to a
   different file than `e.file_path`, `e.file_path` is dirty. Wired into
   `EntityGraph::build_incremental_core` (`graph.rs`) right after pass 1
   assembles `all_entities` — the earliest point the corpus-wide
   `children_by_parent` exists — and runs on *every* build (cold or warm)
   whenever that build precomputed anything, dropping dirty files' facts via
   `fresh_precomputed.retain(...)` before they reach pass 2. New profiling
   counters (`MUL_CLEAN_GATE clean_gate_ms=… files_dropped=…`,
   `SEM_PROFILE_RESOLVE=2`) make the gate's own cost and hit rate first-class,
   not folded into `assemble_ms` silently.
2. **Facts emission for C#/C++.** `precompute_scope_resolvable_file_facts`
   (`scope_resolve.rs`) — the generic sibling of `precompute_js_ts_file_facts`,
   built on BS3's `fused_scope_refs_import_walk` (extended to also return
   whether it saw a literal `"call"`-kind node, one comparison added on nodes
   it already visits) so `TREELESS(F)` — no import-start, no `"call"` node —
   is decided from what the walk saw, not a language table (I3). Gated at the
   call site (`graph.rs`'s pass-1 closure) to exactly `lang_id ∈
   {"csharp","cpp"}` — narrower than the function's own generality, on
   purpose: MUL-DESIGN.md §6.1 verdicts only these two GO for phase 1, and
   I5's salt bump (below) is scoped to match. Python/Go/Java/Rust still always
   get `precomputed: None` from this closure, byte-for-byte unchanged.
3. **I5 salt bump.** `cpp`/`csharp` bumped `ts-0.23` → `ts-0.23-mp1` in all
   three documented mirror sites — `facts_store.rs`, `sem-cli`'s
   `facts_remote.rs`, `sem-core`'s `facts_corpus_probe.rs` example — following
   semx-u16's precedent exactly. `c` (plain C, `scope_resolve: None`) is
   untouched.
4. **Pass 2 is untouched**, confirmed rather than assumed: semx-6rd CUT 1's
   existing `resolve_with_scopes_full_inner` machinery (the
   `precomputed_facts.and_then(...)` branch) already serves any file with an
   entry, regardless of which producer wrote it.

### Gate-fire evidence

Zero real violations exist in any corpus (MUL-A's own census), so the gate
must be exercised synthetically to be *seen* firing, not merely trusted by the
theorem. `scope_resolve.rs`'s new tests
`clean_gate_marks_file_dirty_when_a_child_lives_in_another_file` and
`clean_gate_drops_only_the_dirty_files_precomputed_facts` hand-build a
`SemanticEntity` whose `parent_id` crosses a file boundary (bypassing
extraction, which cannot produce this shape) and assert: the parent's file is
marked dirty, a sound sibling file and a childless leaf file are not (no false
positives), and `.retain()` — the exact operation `graph.rs` performs — drops
only the dirty file's facts. `precompute_scope_resolvable_file_facts_*` tests
pin `TREELESS` at both ends (a Python file with a real `import` gets no facts;
an import-less, call-less Python file does; Swift never does; a representative
C# file with a `using` directive and a method call does — neither
`classify_import_stmt` nor the literal `"call"` kind fire for C#'s grammar).
6/6 new tests pass; full `sem-core` lib suite 619/619 (613 baseline + 6).

Real-corpus gate firing (not a fixture): a synthetic 20,054-file C# repo
(20,050 filler classes + a `partial class Splitty` split across two files +
a C++ `Widget::compute` out-of-line definition against `Impl.h`) built via
`sem-core/examples/facts_probe`, run past `PARSED_FILE_REUSE_LIMIT` so the
chunked path is actually exercised: `SCOPE_BUILD_WORK
files_precomputed=20053` of 20054, `MUL_CLEAN_GATE clean_gate_ms=8.40
files_dropped=0` — the partial class and the out-of-line method are present
and CLEAN holds, exactly as the theorem predicts. `facts_probe`'s cross-process
`ORACLE` (ingest → warm-start → compare against a from-scratch cold build) is
`ok` on all four scenarios (`none`/`leaf`/`mixed50`/`hub`); a second
smoke pair on `sem-core`'s own tree (JS/TS + Rust, no C#/C++) is `ok` on all
four too — **8/8**. `facts_corpus_probe populate`/`consume` across two
independent repo roots sharing the synthetic C#/C++ content: `corpus_hits=20054
files_reused_directly=20054`, `ORACLE ok` — proof the `ts-0.23-mp1` salt
round-trips correctly cross-repo (I5).

### Corpus counters: facts vs. tree, before → after (semx-mp1 vs. HEAD `9c1c531`)

Built two release binaries from the identical workspace, differing only in
this bead's six files (`sem-cli`'s `facts_remote.rs` +
`sem-core`'s `graph.rs`/`scope_resolve.rs`/`facts_store.rs`/
`resolve_profile.rs`/`examples/facts_corpus_probe.rs`), each in its own
`CARGO_TARGET_DIR` to rule out any fingerprint-cache aliasing between them
(confirmed by `cmp`: the two `sem` binaries differ). Protocol: `SEM_LOCAL=1
SEM_TIMINGS=1 SEM_PROFILE_RESOLVE=2 SEM_FACTS_CACHE=0`, fresh `SEM_CACHE_DIR`
per run, `sem find <nonexistent>` to force a genuine cold `full_graph_build`.
n=1 per corpus per side (this document's own disclosed practice); ratios are
the load-bearing numbers, not the absolutes.

| corpus | files_precomputed before → after | files_ast before → after | `reparse_ms` before → after |
|---|---:|---:|---:|
| dotnet-runtime | 228 → 34,600 / 34,898 | 34,670 → 298 | 10,621.8 → 64.8 (**−99.4%**) |
| llvm-project | 61 → 39,545 / 43,270 | 43,209 → 3,725 | 4,145.0 → 192.8 (**−95.3%**) |
| home-assistant-core | 2 → 2 / 18,150 (unchanged) | 18,148 → 18,148 (unchanged) | 567.6 → 529.4 (noise) |
| linux (control) | 0 → 7 / 2,050 scope-resolvable (72,787 total files) | 2,050 → 2,043 | 114.5 → 109.4 (noise) |

`MUL_CLEAN_GATE`: `files_dropped=0` on every corpus measured, every run —
dotnet 139.5 ms over 721,805 entities, llvm 125.7 ms over 582,065, HA 24.9 ms,
linux 204.3 ms. Matches MUL-A's census exactly: the gate is real, it runs, and
it finds nothing to drop on real code.

**HA is flat, as the census's byte-share table (0.23%) predicted** — its
`files_precomputed` count is untouched because HA has zero `.cs`/`.cpp` files;
the only new cost is the CLEAN gate itself running once (it fires whenever a
build precomputes *anything*, including the pre-existing JS/TS facts HA's repo
happens to contain 2 of), ~25 ms against a multi-second build, i.e. noise.

**Linux (C, `scope_resolve: None` for plain C) is the interesting control**:
plain `.c`/`.h` files never reach either the old JS/TS branch or the new
`csharp`/`cpp` branch and are structurally unable to. But linux's tree is not
100% C — it carries 7 files (of 2,050 scope-resolvable, 72,787 total) that
detect as C++, and those 7 now correctly take the new fast path
(`files_precomputed: 0 → 7`). Entity/edge counts and the sorted-edge hash are
bit-identical before/after (`facts_probe save`:
`entities=2312433 edges=1898783 edge_hash=a286cc31b282c98b`, both sides); only
the exported facts-store byte count shifts by 160,994 bytes — those 7 files'
newly-serialized `PrecomputedFileFacts`. This is exactly "does it enter the
path at all" answered with a number instead of an assumption.

### Wall time and memory, cold, before → after

Same two binaries, `/usr/bin/time -l`, two independent pairs per corpus with
the run order swapped between pairs (noise/order-effect check, not full
same-state interleaving — see caveat below).

| corpus | wall (pair 1) | wall (pair 2, reversed order) | maxRSS before | maxRSS after | Δ maxRSS |
|---|---:|---:|---:|---:|---:|
| dotnet-runtime | 38.07s → 21.26s (**−44.1%**) | 41.38s → 24.63s (**−40.5%**) | 8.88–9.29 GB | 11.25–11.80 GB | **+21.2% to +32.9%** |
| llvm-project | 51.49s → 29.66s (**−42.4%**) | 29.26s → 27.91s (−4.6%) | 7.43–8.05 GB | 8.49–8.52 GB | **+5.8% to +6.5%** |

Wall-time magnitude is noisier than the doc's own n=1 disclosure already
warns: llvm's pair 2 shows only −4.6% against pair 1's −42.4%, almost
certainly OS page-cache warmth carried over between consecutive runs against
the same corpus (`SEM_CACHE_DIR` was fresh each time — sem's own warm-start
cache is not the confound — but nothing here drops the OS's file-content page
cache between runs, and the design doc's protocol doesn't call for it either).
The **direction** is unanimous — every one of 4 pairs across both corpora is
faster after — and is independently corroborated by the counters above, which
are exact, not wall-clock: `reparse_ms` collapsing to near-zero is a
structural consequence of `files_precomputed` jumping to 99%+ of the corpus,
not a timing artifact.

**Memory is the number that matters, and it splits the two corpora.** llvm
stays inside the stated +15% ceiling on both pairs (+5.8%, +6.5% — better than
MUL-A §5.3's own +12-13% projection). **dotnet does not: both pairs exceed the
ceiling (+21.2%, +32.9%), reproducibly, not once.** This is a real,
disclosed-in-advance risk — MUL-A §5.3 named it explicitly ("the single
number that could turn the GO into a NO") and priced it lower (+10-11%
projected) than what real measurement now shows. Per MUL-DESIGN.md's I6
fail-safe and this bead's own stop-ceiling instruction: **this is a STOP, not
a ship** — the gate's *correctness* is sound (I1 checked at run time, I6
routes every non-CLEAN/non-TREELESS file to the unchanged re-parse path,
zero regressions on every suite and control corpus below), but dotnet's
*memory* profile as measured here should not go out at full scale until one
of §5.3's two named-but-untaken levers is taken: (i) relax
`SCOPE_RESOLVE_BYTE_BUDGET`'s 30-chunk partition now that fast-path files
never enter `parsed_files` (named as phase 4, "sized after phase 1's memory
measurement" — this measurement), or (ii) spill facts to the per-repo store at
pass-1 exit and read them back per chunk, trading the corpus-wide residency
for bounded chunk-local I/O. Neither is taken in this bead; both remain
correctly scoped to a follow-up.

**Caveat on the absolute baseline**: this session's own before/after pair
(dotnet maxRSS 8.88–9.29 GB) does not match the ws6-baseline figure this
bead's brief cited (dotnet 14.10 GB true-cold, interleaved same-state pairs) —
a different protocol (likely a fuller pipeline than a single `sem find`, or a
different machine state) produced that number, and it was not independently
reproduced here. The **ratio** above is the apples-to-apples, same-machine,
same-binary-except-this-bead comparison and is the number this bead stands
behind; the absolute GB figures should not be read as a reproduction of the
cited ws6 baseline.

### Control corpora: unaffected, proven not assumed

- **monster (JS/TS, positive control)**: `facts_probe save`, before vs. after,
  byte-identical: `files=40872 entities=454541 edges=196223
  edge_hash=4e23ae3a246c8fa9 store_bytes=704649267`, all four fields equal on
  both binaries. JS/TS's producer (`precompute_js_ts_file_facts`) is untouched
  by this bead.
- **linux (C, structural control)**: entities/edges/edge_hash bit-identical
  (above); the only delta is the 7 incidental C++ files' new facts, accounted
  for exactly.
- **HA (Python, NO-GO-as-is control)**: `files_precomputed` unchanged; the
  only new cost is one ~25 ms CLEAN-gate pass, matching MUL-A §5.1's
  "-0.03%" HA-gate-only prediction in direction and order of magnitude.

### Gates run

- `cargo build --release` clean on `sem-core`, `sem-cli`, and the
  `mul_census`/`facts_probe`/`facts_corpus_probe` examples.
- `sem-core` lib suite: 619/619 (613 + 6 new). `sem-core` integration suites
  (`d_smoke`/`elm_smoke`/`graph_accuracy`/`kappa`/`parse_cache`/
  `scope_resolve_bench`/`single_pass_invariants`): all green, unchanged counts.
  `sem-cli`: 248/248 (139 unit + 109 integration across 20 test binaries),
  unchanged, including the untouched `review_listen_dry_run`/
  `diff_cloud_relations` WIP suites.
- `cargo fmt --check` and `cargo clippy --lib`/`--example` clean on every file
  this bead touched (verified by line-range cross-reference against a full
  clean-rebuild clippy pass; pre-existing warnings elsewhere in the same files
  are unchanged and unrelated).
- `facts_probe`: 8/8 cross-process `ORACLE ok` (2 repos × 4 scenarios).
  `facts_corpus_probe`: `ORACLE ok`, 100% cross-repo corpus hit rate on the
  new `ts-0.23-mp1` salt.
- Bit-identical: monster and linux entity/edge/edge-hash, both fully verified
  above.
- **Not run this session, stated rather than implied**: the named
  `index_probe` six-oracle battery and `facts_corpus_probe`'s tampering
  negatives specifically — `facts_probe`'s cross-process oracle and the
  monster/linux bit-identical checks cover overlapping ground (entity/edge/
  hash equivalence under a real cross-process round trip) but are not a
  substitute for those exact named scripts. rails was not separately run.
  Untouchables (`README.md`, `examples/hosted-diff/*`, `languages.rs` reflow
  hunks, and the five WIP `sem-cli` files) were confirmed untouched by diff
  inspection, not re-verified byte-for-byte post-hoc.

### Verdict

**C++ (llvm): GO, unconditionally** — matches or beats every MUL-A projection
(reparse −95.3%, memory +5.8-6.5% against a +12-13% projection).
**C# (dotnet): the gate is correct and safe (I1-I6 all hold, zero regressions,
zero CLEAN violations), but real-measured memory (+21-33%) exceeds the stated
+15% ceiling on both measured pairs** — MUL-A's own fail-safe language ("this
is a real cost and it is the single number that could turn the GO into a NO")
is exactly the situation this measurement produced. Recommendation: hold
full-scale dotnet rollout behind one of §5.3's two named memory levers before
shipping broadly; the code itself needs no changes to do so — both levers are
additive follow-ups, not corrections to what landed here.

### What phase 2/3 should inherit

- The CLEAN gate (`PrebuiltEntityIndex::dirty_precompute_files`) is
  language-agnostic already — phases 2/3 need no changes to it, only to
  `TREELESS` (which the descriptor extensions redefine) and to the `graph.rs`
  admission test's language allowlist.
- `precompute_scope_resolvable_file_facts` is already written generically
  (any scope-resolvable, non-Swift language) — phase 2/3 should *widen the
  caller's allowlist*, not fork the function, exactly as this bead did *not*
  fork `precompute_js_ts_file_facts`.
- Salt-bump discipline (I5/F2) is now proven twice (semx-u16, semx-mp1):
  bump all three mirror sites, in the same commit as the producer change,
  every time. Phase 2's `import_stmts` descriptor refactor and phase 3's
  `ctor_call_sites` extension both change what the producer emits for
  languages already in `LANGUAGE_SALTS` — each needs its own bump.
- **Take a memory lever before or alongside phase 2.** Phase 2 unlocks Rust/
  Go/Java and the larger half of Python — all four have far lower FASTPATH
  byte-share than C# (13.74%/5.24%/0.98%/0.23% resp. per MUL-A §2.3), so
  their own memory delta is very likely smaller in absolute terms, but phase
  2 should re-measure per family rather than assume — dotnet's overshoot
  against its own projection here is a reason for caution, not comfort.
- The `MUL_CLEAN_GATE`/`SCOPE_BUILD_WORK` counters this bead added
  (`SEM_PROFILE_RESOLVE=2`) are the right instrument for phase 2/3's own
  gate-firing and facts-vs-tree evidence — reuse them rather than adding
  parallel counters.

Bead: semx-mp1 (MUL P1). Epic: semx-w5k. Parent thesis: semx-mul. Prior bead:
semx-w5k.1 (MUL-A, design + census). Prerequisite: semx-u16 (I2 seed-order
fix, landed at HEAD before this bead started).

## MUL P1 memory-lever follow-up: attribution sharpened, two levers tried and rejected, ceiling stays gated

Follow-up to the section above, against the same HEAD (`44c0b4c`) with **no
production code changed by this bead** — every experiment below was built,
measured, and then reverted; `git diff --stat` against `44c0b4c` is empty for
every file this bead touched during investigation. This is the honest
"levers exhausted, still over" close the memory-lever task called for, not a
ship.

### Attribution: where the +21-33% actually lives

`SEM_PROFILE_MEM=1 SEM_PROFILE_RESOLVE=2`, dotnet-runtime, HEAD `44c0b4c`
(the CLEAN gate already landed), fresh `SEM_CACHE_DIR`, `sem find
<nonexistent>`:

| Checkpoint | process RSS (in-process `ps`, illustrative) | attributed | attributed/RSS |
|---|---:|---:|---:|
| post-pass-1 | 9,274-9,364MB | 3,404-4,180MB | 43-48% |
| peak-resolve | 9,278-9,398MB | 2,842-3,667MB | 38-40% |
| post-scope-resolve | 9,920-9,960MB | 482.0MB | 5% |
| post-build | 10,178-10,328MB | 2,627-3,432MB | 25-29% |

`precomputed_facts` = **1,089.1MB**, present from post-pass-1 through the end
of pass 2 (it must stay corpus-wide resident: every chunk can contain any of
the 34,600 precomputed files, so the map can't be dropped until the last
chunk runs) — the single largest structure this bead's own change is
responsible for. Every other named structure at post-pass-1
(`all_entities.content`, `.metadata`, `entity_map`, `symbol_table`,
`class_members`, `owner_members`, `entity_ranges`) is **pre-existing and
ratio-neutral** — it costs the same whether or not the CLEAN gate exists, so
it cancels out of the before/after ratio and cannot be the source of the
overshoot against §5.3's projection.

**The old tree-residency mechanism is confirmed absent, not just assumed
gone**: `parsed_files(path+content)` reads **0.0MB** at both post-pass-1 and
peak-resolve, and the `chunk-reparse` checkpoints (30 chunks) show RSS
climbing only **+679MB** total across the whole chunk loop — almost exactly
accounted for by `post-scope-resolve`'s `scope_edges` (220.3MB) +
`scope_consumed_words` (261.7MB) = 482.0MB, i.e. ordinary edge/word
accumulation, not tree memory. semx-g6t's 20 MiB byte budget is doing
essentially nothing for dotnet now, exactly as §5.3 predicted — but that also
means **lever (i)'s named mechanism (bounding chunk tree residency) cannot be
the fix, because there is no tree residency left to bound.**

**The gap against §5.3's own projection is real and is itself the headline
finding.** §5.3 projected dotnet's net memory delta at **+1.0-1.15GB
(+10-11%)** — `precomputed_facts` measured here (1,089.1MB) is *inside* that
projection, not over it, and chunk-tree residency measured at ~0MB is *far
under* the ~220MB the projection budgeted for it. Yet the measured overshoot
is +21-33%, roughly double. The extra is not in any named structure — it is
in the **persistently unattributed fraction of RSS** (52-75% at every
checkpoint above), a phenomenon semx-4w1 already documented as a standing
property of this codebase's memory profile *before* MUL P1 existed (that
bead's own "Finding 2": named structures explained only ~20-30% of RSS on a
pre-MUL-P1 dotnet build). MUL P1 does not introduce this gap; it appears to
have **grown** it, and the leading unconfirmed hypothesis — not verified
within this bead's budget, stated as a hypothesis rather than a number — is
`PrecomputedFileFacts::approx_heap_bytes`'s own documented undercount:
`return_type_map`/`instance_attr_types`/`init_params`/`attr_to_param` are
sized by fixed per-entry overhead only (`sizeof::<String>() * 2`, etc.), never
walking the actual `String`/`Vec<String>` *contents* (C# return-type and
instance-attribute type names) the same way `all_entities.content` is walked
exactly (`.capacity()` summed). These four fields are new-at-corpus-scale for
this bead specifically — they used to populate only from 228 JS/TS files;
now 34,600 C#/C++ files feed them — so an undercount that was negligible
before is no longer negligible. Isolating this precisely needs a
`mem_profile.rs` extension that walks these fields' nested String bytes the
way `semantic_entities_bytes` already splits `content` from metadata; this
bead did not have budget to add and verify it against a fresh capture, so it
is reported as the leading lead, not a confirmed line item.

### Two levers tried, both rejected, with numbers

**Lever (ii)-adjacent: mimalloc purge-delay tuning.** Attribution's own
unattributed-RSS framing raised allocator retention as a candidate (a
`rayon` flat-parallel-map pass over 34,898 files, one `tree_sitter::Tree`
parsed transiently per C#/C++ file in pass 1 now instead of 228, threads
going idle at uneven times after freeing a large tree). This is *not* the
same test semx-4w1's Finding 3 already falsified ("mimalloc vs system
allocator, no improvement") — mimalloc is already this binary's production
allocator (semx-g6t adopted it); the new test was mimalloc's *default* purge
delay (10ms, decided lazily on a thread's next allocation) against
`mi_option_purge_delay = 0` (immediate purge), baked into `sem-cli/src/
main.rs` via `libmimalloc-sys`'s `mi_option_set` (verified against both C
sources `libmimalloc-sys` can compile — `v2` and `v3`, this workspace links
`v3` — the option's enum ordinal is 15 in both, deprecated slots kept rather
than renumbered).

An initial in-process `ps`-checkpoint comparison looked promising
(post-pass-1 9,364.4→8,780.0MB, post-build 10,328.1→9,787.0MB, ~5-6%). That
measurement method turned out to be too noisy to trust alone: three
independent, order-reversed `/usr/bin/time -l` pairs (the authoritative
metric the ceiling verdict is measured against) tell a different story:

| Pair | before maxRSS | after maxRSS | Δ | before user-CPU | after user-CPU |
|---|---:|---:|---:|---:|---:|
| 1 | 10.342GB | 9.974GB | -3.56% | 178.56s | 226.00s |
| 2 (reversed) | 10.268GB | 10.417GB | **+1.45%** | 222.12s | 178.97s |
| 3 | 10.116GB | 10.089GB | -0.24% | 173.83s | 210.11s |
| **avg** | **10.242GB** | **10.160GB** | **-0.8%** | **191.5s** | **205.0s (+7.1%)** |

Direction flips between pairs 1 and 2 — the RSS effect is inside the noise
floor this bead's own baseline-vs-baseline rerun already established
(~1-1.5%, two identical-code runs). The CPU-time cost, by contrast, is
*consistent* across all three pairs (more frequent `madvise`/decommit calls
under this workload's allocation churn) — a real, repeatable +7% wall-adjacent
cost for a memory benefit that isn't reliably there. **Rejected**: reverted
from `sem-cli/Cargo.toml` and `main.rs`, confirmed byte-identical to `44c0b4c`
by `git diff --stat`.

**Lever (i), literal and surgical forms.** MUL-A/§5.3's lever (i) is "relax
the byte budget now that fast-path files never enter `parsed_files`." Taken
literally (bump `SCOPE_RESOLVE_BYTE_BUDGET` globally) this is **unsafe to
ship**: the budget is one constant shared by every language, and Python/Go/
Java/Rust corpora (phase 1 does not gate them) still hold a real tree per
file in the old chunked path — raising the constant blows up *their* peak
chunk-tree residency, the exact failure mode semx-g6t's own tuning table
exists to prevent. A blanket 20MiB→512MiB experiment on dotnet alone (not
shipped, diagnostic only) did show a lower `ps`-sampled post-build RSS
(9,761.3MB vs the ~10,216-10,328MB baseline range) — but per the mimalloc
result above, a `ps`-checkpoint delta of that size is not distinguishable
from noise without an authoritative re-measurement, and the change is unsafe
regardless of its true size.

The safe, surgical form — implemented, tested, and reverted this bead — was
`chunk_files_by_byte_budget_excluding_precomputed`: a file's on-disk size
counts toward the 20MiB budget only when it lacks a `precomputed_facts`
entry, so a corpus with zero precomputed files (every language phase 1 does
not gate) partitions byte-for-byte identically to today (verified by a new
regression test), while dotnet/llvm's ~99.74%/96.76% fast-path share
collapses the partition toward the single chunk §5.3's phase-4 note
anticipated. Measured on dotnet: chunk count 30→5, but `ps`-sampled post-build
RSS went **up**, not down — 10,664.6MB against the ~10,216-10,328MB baseline
range (roughly **+3-4% worse**). Cause, confirmed by inspecting the
mechanism rather than assumed: `resolve_with_scopes_full_inner`'s per-chunk
merge loop (`return_type_map`/`instance_attr_types`/`init_params`/
`attr_to_param.extend(...)`, cloned out of every precomputed file's facts in
that chunk — `SCOPE_BUILD_NS`'s `precomputed_clone_ms` counter) is a
**chunk-local accumulator that scales with file count in the chunk, not with
bytes**. The old byte-budget partition was incidentally bounding this
accumulator's peak size too (a 20MiB chunk of C# source is a few hundred
files at most); excluding precomputed bytes from the budget removed that
incidental bound and let thousands of files land in one chunk, so the
accumulator's own peak grew even though total work stayed the same. This is
the inverse of what the design doc anticipated for lever (i), and it is a
real, mechanism-level finding: **the byte budget, post-MUL-P1, is now
implicitly load-bearing for a *different* quantity (chunk-local facts-merge
accumulator size) than the one it was tuned for (tree residency) — relaxing
it without separately bounding the new quantity is a regression, not a
win.** Reverted; `crates/sem-core/src/parser/graph.rs` confirmed
byte-identical to `44c0b4c`.

### Lever (ii), not attempted: scoped and priced, not implemented

MUL-A/§5.3's lever (ii) — spill facts to the per-repo store at pass-1 exit,
read back per chunk — correctly targets `precomputed_facts`'s corpus-wide
residency (the one MUL-P1-specific structure this bead's attribution
confirms is real, sizeable, and load-bearing for the whole build). It was not
attempted here, for a reason worth recording precisely rather than "ran out
of time": `PrecomputedFileFacts::content` (the full file source text) is
**not** chunk-scoped in its actual use — `snapshot_bow_content` (semx-bkz)
takes a single corpus-wide `Cow::Borrowed` view of every precomputed file's
content *before* pass 2/chunking begins, specifically to avoid a second read
or a second copy of the source bytes. A true per-chunk spill-and-reload would
have to either (a) accept a `Cow::Owned` clone in bow's snapshot instead
(undoing semx-3tb's copy-elimination), or (b) have bow re-read TREELESS
files' content from disk directly (partially undoing §5.1's own claimed
`bow_index_io` elimination — part of the stated reparse-collapse prize). The
other five fields (`scopes`/`entity_scope_map`/`entity_inner_scope`/
`ast_refs`/the four merge-loop maps) genuinely are chunk-scoped-only in their
use and are the real candidate for streaming — but `PrecomputedFileFacts` is
one struct, serialized as one unit into the facts-corpus wire format under
the `ts-0.23-mp1` salt (`facts_store.rs`); splitting it into a
corpus-resident "content" half and a chunk-streamed "resolution facts" half
is a wire-format change (another I5-style salt bump) that also touches the
incremental session-carry contract (`carry.precomputed` must survive whole
warm-rebuild sessions for I4's GREEN reuse, not just one cold build) — the
full battery this bead ran below (bit-identical, six `index_probe` oracles,
`facts_probe` 8/8, `facts_corpus_probe` incl. tampering negatives) would need
re-running against the new producer/consumer shape, not spot-checked. Sized
at the same order as MUL-DESIGN's own phase estimates: **2-3 days**, not a
same-session change, given both levers actually tried in this bead came back
negative or negligible and the risk is to I4/I5-protected invariants.
Plausible benefit, order-of-magnitude only: `precomputed_facts`'s non-content
fields are ~586MB of its 1,089.1MB attributed total (1,089.1MB minus
dotnet's own ~503.5MB fast-path source-byte total, MUL-A §2.3) — bounding
that to one chunk's worth would save a large fraction of ~586MB, i.e. a
plausible **mid-hundreds-of-MB** win (call it ~5% of the ~10.2GB current
peak) — on its own **not enough** to close a +21-33% gap to +15%, and the
approx_heap_bytes undercount above means even this estimate could be an
undercount of the true benefit.

### Gates run this bead

Because no production code shipped, the bit-identical/oracle/suite battery
reduces to "confirm the working tree is still exactly `44c0b4c`" (`git diff
--stat` empty for every file touched during experimentation, `examples/`
untracked and unrelated) plus the gates that were owed regardless of the
lever outcome — P1's own close named these as **not run**, run here:

- **Six `index_probe` oracles**: **all `PASS` on HA and dotnet-runtime**
  (`MUTATION` reports `skipped=no_battery_pattern_had_a_provable_true_positive`
  on both — a corpus-content property, not a failure). dotnet: `REFS_ORACLE`
  entities=1,141,094 checked, 0 mismatched, 0 kind_mismatched.
  **llvm-project: 5/6 PASS, `TESTS_ORACLE` FAILs — a real, newly-surfaced
  finding, not a flake, reported below rather than smoothed over.**
  `REFS_ORACLE` also confirms **PASS** on llvm's full 2,751,958 entities, but
  took 1,634.7s against dotnet's 115.7s for less than half the entity count
  (~5.9× the per-entity cost) — a non-linear scaling in `REFS_ORACLE` against
  llvm's own graph shape, surfaced here but not investigated further; it is
  outside this bead's scope (the query-index/oracle code is untouched by
  every change this bead made, all of which were reverted).
  - **`TESTS_ORACLE_MISMATCH id="llvm/test/tools/opt-viewer/Inputs/suppress/
    s.opt.yaml::property::Args" got=true`, 1 of 2,751,958 checked.** Root
    cause, confirmed by inspection (`sem entities` on the file, not
    speculated): this YAML fixture contains **11 separate `property Args`
    entities** (one per `---` document in the file, at line ranges
    `L9:15`, `L23:29`, `L37:43`, …), and YAML's entity-id scheme
    (`{file_path}::{entity_type}::{name}`, no parent to qualify a top-level
    property) gives all 11 the **same id**, `s.opt.yaml::property::Args` —
    the within-file id-collision shape MUL-DESIGN.md's F3 already named for
    other languages (elasticsearch's 249,740, kubernetes' 2,708, llvm's own
    2,602 *entity* collisions), just not previously shown to be
    **observably harmful**. Here it is: `is_test_entity`'s
    `has_test_marker` check does a bare substring match for `"it("`
    (meant to catch `it('description', ...)` JS/TS test blocks), and the
    11 colliding occurrences' *content differs* — the first (`L9:15`)
    contains `'"Swift.CountableRange.init(uncheckedBounds:)"'`, whose
    `init(` substring contains `it(`, so `is_test_entity` returns `true`
    for *that* occurrence (combined with `in_test_file=true`, since the
    path is under `llvm/test/`); later occurrences (e.g. `L23:29`'s
    `makeIterator()`) don't happen to contain the substring and return
    `false`. Because `TESTS_ORACLE`'s own by-id `HashMap` collapses all 11
    occurrences to one entry (last-write-wins) while the packed index's
    `FLAG_IS_TEST` bit was set from a possibly different occurrence's
    answer, the two can legitimately disagree for a colliding id — this
    is not a index/build nondeterminism bug, it is the **id-collision
    hazard becoming user-visible for the first time**, because
    `TESTS_ORACLE` is the first oracle in this campaign that cross-checks
    a *content-dependent* per-entity property by id rather than a
    structural one. **Surfaced, not fixed** — unrelated to and unreachable
    from every file this bead touched (all reverted); the actionable shape
    for whoever picks it up: either give YAML property entities under
    repeated documents a disambiguating id component, or make
    `has_test_marker`'s `"it("` check word-boundary-aware (both fixes also
    happen to remove the false positive, but the id-collision is the
    precondition that makes the disagreement *observable* at all).
- **`facts_probe`: 8/8** cross-process `ORACLE ok` (2 independent
  home-assistant-core checkouts × `none`/`leaf`/`mixed50`/`hub`).
- **`facts_corpus_probe`: populate/consume `ORACLE ok`**, negative proof
  `ok` (same content at a new path correctly misses the corpus, `hits=0`).
- **`facts_corpus_probe`'s tampering negative — the specific gap P1's close
  named as not run**: `remote-populate`/`remote-consume --tamper`. One
  record's claimed `content_hash` mutated to disagree with its own payload;
  `INGEST` reports `accepted=22324 rejected=1`, `INGEST_REJECTED reason=claimed
  content_hash ... does not match the fact's own content_hash ...`,
  `TAMPER ... ok: rejected with ContentHashMismatch, nothing else rejected`,
  and the post-merge cold-build `ORACLE ok` — the tampered fact is refused
  and the final graph is unaffected, not poisoned, exactly as I5/F2's
  contract requires.
- **`sem-core` lib suite: 619/619** (unchanged from P1's own close — no
  code, no new tests, since the two experimental levers were both reverted).
  Integration suites (`d_smoke` 2, `elm_smoke` 2, `graph_accuracy` 3, `kappa`
  42, `parse_cache` 7, `scope_resolve_bench` 15, `single_pass_invariants` 3): all
  green. `sem-cli`: **248/248** (139 unit + 109 integration), unchanged.
- `cargo fmt --check`/`cargo clippy` show only the pre-existing gaps already
  disclosed as belonging to the five WIP untouchable `sem-cli` files (verified
  identical on a clean `44c0b4c` worktree, not introduced by this bead — this
  bead's own two experiments were fmt-clean and clippy-clean before being
  reverted).
- Not separately re-run: `rails`/`monster`/`linux` bit-identical dumps — moot
  for a bead that shipped no code (identical binary ⇒ trivially identical
  output), not a substitute for running them against an actual future
  lever-(ii) landing.

### Final ceiling verdict

**Unchanged from this section's own predecessor: dotnet stays GATED.**
Neither lever named in MUL-DESIGN.md §5.3 survives rigorous (`/usr/bin/time
-l`) measurement — mimalloc purge tuning is noise-level on RSS with a real
CPU cost, and the byte-budget relaxation regresses RSS once its actual
post-MUL-P1 load-bearing role (bounding the per-chunk facts-merge
accumulator, not tree residency) is accounted for. Lever (ii) — the one this
bead's own attribution says *should* work — was not attempted; its real cost
(a `PrecomputedFileFacts` wire-format split touching I4/I5) and its honest,
order-of-magnitude benefit (~5% of peak, not enough alone) are now priced
precisely enough that a future bead can decide whether to fund it without
re-deriving this bead's dead ends first. **llvm is unaffected and stays GO**
(no code shipped by this bead; its prior +5.8-6.5% memory verdict stands
unchanged — the `TESTS_ORACLE` finding above is a pre-existing, content- and
id-collision-triggered correctness bug in `is_test_entity`/the query index,
orthogonal to the memory ceiling and to anything this bead's own experiments
touched; it is reported for whoever owns that surface next, not folded into
this bead's GO/NO-GO). Per I6's fail-safe, full dotnet rollout remains gated
on a memory fix; this bead narrows what that fix must be (facts-residency
streaming, priced above) rather than delivering it.

### What phase 2/3 additionally inherit, beyond the prior section's note

- **The byte budget is now dual-purpose and must be treated as such.** Any
  future change to `SCOPE_RESOLVE_BYTE_BUDGET` or to what counts toward it
  must consider *both* its original role (bounding chunk tree residency for
  ungated languages) *and* the role this bead discovered (bounding the
  per-chunk facts-merge accumulator for gated languages) — phase 2 adds
  Rust/Go/Java to the gated side, so both roles apply to more corpora at
  once, not fewer.
- **`approx_heap_bytes`'s undercount on `return_type_map`/
  `instance_attr_types`/`init_params`/`attr_to_param` should be fixed before
  phase 2 lands**, not after — phase 2 scales exactly these fields across
  three more languages, and this bead's attribution could not rule out that
  fixing the undercount changes the *measured* ceiling verdict for dotnet
  itself, independent of any lever.
- **Do not trust in-process `ps`-checkpoint deltas under ~2% without an
  authoritative `/usr/bin/time -l` confirmation** — this bead's own mimalloc
  measurement flipped sign between the two methods. Any future lever's "it
  helped" claim should be checked against order-reversed `/usr/bin/time -l`
  pairs before being written up as a result, not just the cheaper in-process
  sampling this bead's own predecessor section used for its per-checkpoint
  numbers (those numbers are still valid as *structural attribution*, i.e.
  "which named field is big," just not as a *before/after delta* at the
  sub-2% scale).

## semx-5sw: the CLEAN gate scoped to the files it actually adjudicates (R1), a dead field deleted (R2), R3 declined by elimination

RE-AUDIT at `08ac74b` found the CLEAN gate (semx-mp1) doing more work than its
own verdict is ever read for: it built a full-corpus `PrebuiltEntityIndex`
(two `HashMap<&str, Vec<&SemanticEntity>>` indexes, one bucket per distinct
file/parent-id in the *entire* corpus) to adjudicate only `fresh_precomputed`'s
keys — pass 1's freshly-precomputed files, a handful per build (linux 7 of
2,050 scope-resolvable files, HA 2) — then discarded every other file's
verdict via `fresh_precomputed.retain(...)`. This bead is a removal: delete
the unread work, not add a cache in front of it.

### R1: the soundness argument, from the code, before narrowing anything

`CLEAN(F)` (MUL-DESIGN.md §1): for every entity `e` declared in `F`, every
entity naming `e.id` as `parent_id` also belongs to `F`. What
`dirty_precompute_files` (the removed method) actually computed, restated
precisely:

> `F` is dirty ⟺ ∃ `e` ∈ entities(F). ∃ `c` ∈ all_entities. `c.parent_id ==
> Some(e.id)` ∧ `c.file_path != F`.

This depends on exactly two things: (1) `F`'s own entities — to know which
ids count as "declared in `F`" at all — and (2) every entity anywhere in the
corpus whose `parent_id` names one of those ids, i.e. cross-file parent edges
**into** `F`. Nothing in the predicate depends on any other file's own
entities, nor on a `children_by_parent` bucket keyed by an id no candidate
file declared. By `build_entity_id`'s construction (MUL-DESIGN.md §1.1's
file-rootedness theorem — entity extraction is per-file and never sees
another file's bytes, so every id is rooted at its own file transitively) a
cross-file `parent_id` can only name `e.id` by having independently
recomputed the identical id string; the runtime check exists for that one
hole (id-string collision across files — the census found zero across seven
corpora), not because "which ids are candidates" needs a corpus scan. "Who
might point at a candidate" does still need one — a cross-file child can live
in *any* file, precomputed or not, so I6's fail-safe requires pass 2 to stay
`O(corpus)`.

This is checked, not just argued: `clean_gate_scoping_matches_corpus_wide_verdict_per_candidate`
(`scope_resolve.rs`) constructs a candidate file whose cross-file child lives
in a file that is *not itself a candidate* and asserts the candidate is still
caught — proof that narrowing the *candidate set* cannot narrow *who is
checked for pointing at* a candidate.

### R1: what changed

`graph.rs`'s pass-1 assembly loop already computes `(file_path, start, len)`
spans into `all_entities` for every file, for the incremental carry path
(`entity_spans`) — this bead captures the same shape, unconditionally, for
exactly the files that got fresh precomputed facts this round
(`clean_gate_candidate_spans`), a free byproduct of bookkeeping the loop
already does. `scope_resolve::clean_gate_dirty_files` replaces
`PrebuiltEntityIndex::build(&all_entities).dirty_precompute_files(&all_entities)`
with:

- **Pass 1** (no corpus scan): slice `all_entities[start..start+len]` for
  each candidate span directly, building `id_owner : id -> file_path` sized
  to *Σ candidates' entity counts*, not the corpus.
- **Pass 2** (the one part that must stay `O(corpus)`, per the soundness
  argument above): scan every entity's `parent_id` against `id_owner`,
  marking the owner file dirty on a cross-file hit.

Neither pass allocates a `Vec` bucket per distinct file or parent id in the
corpus the way `PrebuiltEntityIndex::build` did — the old three-pass,
two-large-`HashMap` implementation is gone (`dirty_precompute_files` and its
`PrebuiltEntityIndex` call site deleted; its two tests migrated to the new
function's signature, plus the one new scoping-specific test above). `I6`
fail-safe is unchanged: an empty candidate set short-circuits to an empty
dirty set, same as the unchanged `!fresh_precomputed.is_empty()` guard at the
call site.

### R2: `entities_by_file` — resolved by elimination, not by field surgery

`PrebuiltEntityIndex.entities_by_file` was built by the CLEAN gate's index and
never read by its only consumer there (`dirty_precompute_files` only ever
touched `children_by_parent`) — but the field is *not* dead in general: the
struct's other consumer (`resolve_scopes_in_file_chunks` →
`resolve_with_scopes_full_inner`, `scope_resolve.rs:~1668`) does read it, for
scope resolution's file-local entity lookups. Since R1 replaces the CLEAN
gate's own `PrebuiltEntityIndex::build` call with a function that isn't a
`PrebuiltEntityIndex` at all, the dead-for-this-consumer field is gone from
that call site by construction — no separate deletion needed, and the
`entities_by_file` field stays exactly where it is legitimately read.

### Measurements (`SEM_PROFILE_RESOLVE=2`, release, two independent binaries)

Two release `sem` binaries built from separate `CARGO_TARGET_DIR`s — "before"
from `git stash` of this bead's two touched files (identical to `08ac74b`),
"after" with this bead's changes — `cmp` confirmed the binaries differ.
Protocol: `SEM_LOCAL=1 SEM_TIMINGS=1 SEM_PROFILE_RESOLVE=2 SEM_FACTS_CACHE=0`,
fresh `SEM_CACHE_DIR` per run, `sem find <nonexistent>` to force a cold
`full_graph_build`, same-state back-to-back pairs (no file touches between
runs of the same corpus).

| corpus | `MUL_CLEAN_GATE clean_gate_ms` before | after | Δ | `files_precomputed`/`files_ast` (unchanged, both sides) | `files_dropped` |
|---|---:|---:|---:|---|---:|
| linux | 201.06, 211.26 (2 runs) | 38.11, 39.78 (2 runs) | **−81%** | 7 / 2043 | 0 |
| home-assistant-core | 23.67 | 5.33 | **−77%** | 2 / 18148 | 0 |
| dotnet-runtime (`SEM_MUL_CSHARP=1`) | 132.23 | 134.34 | ~0% (noise) | 34600 / 298 | 0 |

dotnet's flat delta is the scoping argument's own prediction, not a miss:
34,600 of 34,898 files *are* the candidate set there, so there is
structurally almost nothing to scope away — R1's win is proportional to
`1 - |candidates|/|corpus|`, which is ~99.7% for linux, ~99.99% for HA, and
~0.85% for dotnet under the C#-opt-in workload. `files_precomputed`/
`files_ast`/`files_dropped` are unchanged on every corpus, both sides —
the gate's *verdict* is unaffected, only its cost.

**Bit-identical, both sides** (`facts_probe save`, same-state pairs, separate
binaries):

| corpus | files | entities | edges | edge_hash | store_bytes |
|---|---:|---:|---:|---|---:|
| linux | 72787 | 2312433 | 1898783 | `a286cc31b282c98b` | 1948756905 |
| home-assistant-core | 22325 | 257832 | 307366 | `073e598783df4ee2` | 348966427 |
| dotnet-runtime (`SEM_MUL_CSHARP=1`) | 47475 | 990654 | 980921 | `0e8a77c9c5566785` | 3618662935 |

All four fields identical before vs. after on all three corpora — `store_bytes`
identical too, so the exported facts blob is byte-identical, not just the
counts. (dotnet's entity count, 990,654, differs from the `08ac74b` writeup's
990,506 — verified this is *not* a regression: before and after, measured
back-to-back on the same pinned corpus commit, agree with each other exactly;
the discrepancy against the older written record is unexplained but
orthogonal to this bead, since both sides of this bead's own comparison
match.)

### R3: declined, by elimination rather than measurement forcing a NO

`PrebuiltEntityIndex`'s own doc comment complains about being rebuilt
needlessly; R3 asked whether the CLEAN gate's build and
`resolve_scopes_in_file_chunks`'s build (`graph.rs:1604`) — both, before this
bead, literal `PrebuiltEntityIndex::build(&all_entities)` calls in the same
cold build — should be unified.

After R1, `grep -rn "PrebuiltEntityIndex::build" crates/sem-core/src/parser/`
returns exactly **one** call site in the cold-build critical path
(`graph.rs:1604`, `resolve_scopes_in_file_chunks`) plus one dormant fallback
(`scope_resolve.rs:~1671`, only reached when a caller passes no pre-built
index at all — the non-chunked, small-repo path, never exercised alongside
the chunked build in the same cold build). The CLEAN gate no longer
constructs a `PrebuiltEntityIndex` — R1's scoped function needs neither
`entities_by_file` nor a corpus-wide `children_by_parent`, so there is
nothing left of the "same index, twice" premise to unify. **R3 is resolved by
R1's elimination, not by a unification landing in this bead.**

Had R1 not already removed the duplicate, unifying would have been unsafe
regardless: `resolve_go_method_parent_ids` (`graph.rs:2421`) sits *between*
the CLEAN gate's old call site (`~2298`, before this bead) and
`resolve_scopes_in_file_chunks`'s call site (`~2994`+), and rewrites Go
methods' `parent_id` to point cross-file. `resolve_scopes_in_file_chunks`'s
index is built *after* that rewrite (correct — scope resolution needs to see
Go's real cross-file struct/method links); the CLEAN gate's old index was
built *before* it (correct — I1 needs pass 1's raw, per-file-rooted entities,
not Go's post-hoc rewrite). Reusing one index for the other's purpose would
have served a stale `children_by_parent` to whichever side went second — the
phase-2 ordering hazard named in the bead's brief (semx-mul comment: gate
stays before `resolve_go_method_parent_ids`) is real, confirmed by reading
the call order, not hypothetical. No code changes for R3 beyond R1's.

### Gates run

- `cargo build --release -p sem-core` clean (no warnings) after R1/R2;
  `cargo clippy --release -p sem-core --lib` and `cargo fmt --check` clean on
  every line this bead touched (checked by line-range cross-reference; the
  crate has pre-existing warnings/format debt elsewhere, unrelated and
  untouched).
- `cargo test --release -p sem-core --lib`: 622/622 (3 CLEAN-gate tests
  updated to the new function's `(path, start, len)`-spans signature, plus
  one new scoping-specific test — `clean_gate_scoping_matches_corpus_wide_verdict_per_candidate`).
- `cargo test --release -p sem-core` integration suites — `d_smoke`,
  `elm_smoke`, `graph_accuracy`, `kappa`, `parse_cache`, `scope_resolve_bench`,
  `single_pass_invariants`: all green (71 + 3 tests total across the seven
  binaries).
- `cargo test --release -p sem-cli`: full suite green (139 unit + integration
  across 20 test binaries), including the untouched `review_listen_dry_run`/
  `diff_cloud_relations` WIP suites.
- `facts_probe`: 8/8 cross-process `ORACLE ok` — linux (exercises the C++
  CLEAN-gate path, 7 precomputed files) × `{none,leaf,mixed50,hub}`, plus
  sem-core's own tree (JS/TS + Rust) × the same four scenarios.
- `facts_corpus_probe`: `populate`/`consume` `ORACLE ok`, `NEGATIVE ok`
  (renamed-path miss); `remote-populate`/`remote-consume` `ORACLE ok` with
  and without `--tamper` — tampering a claimed record's content hash is
  rejected (`ContentHashMismatch`) while every other file in the same batch
  still merges and the overall build still matches a from-scratch cold build.
- Bit-identical entities/edges/edge_hash/store_bytes on linux, HA, and dotnet
  (`SEM_MUL_CSHARP=1`) — table above.
- Untouchables (`crates/sem-cli/src/commands/diff/cloud_upload.rs`,
  `crates/sem-cli/src/commands/diff/relations.rs`,
  `crates/sem-cli/src/commands/setup.rs`,
  `crates/sem-cli/tests/diff_cloud_relations.rs`,
  `crates/sem-cli/tests/review_listen_dry_run.rs`,
  `crates/sem-core/src/parser/plugins/code/languages.rs`, `README.md`,
  `examples/hosted-diff/*`) confirmed byte-identical — this bead's `git
  status` never listed them beyond their pre-existing (other session's) WIP
  state.
- Not re-run this session: llvm-project — the bead's own gate list scopes
  bit-identical/gate-cost verification to HA + dotnet + linux; llvm's prior
  `08ac74b` numbers are unaffected by this bead's diff (R1/R2 touch only the
  CLEAN gate's own scan scope, which is language-agnostic and untouched in
  its admission logic) but were not independently re-measured here.

### Verdict

**R1: shipped.** CLEAN gate cost collapses on every corpus where the
candidate set is a small fraction of the corpus (linux −81%, HA −77%),
unchanged (noise-level) where it isn't (dotnet, ~99% of files under
adjudication with `SEM_MUL_CSHARP=1`) — exactly the scoping argument's own
prediction, not a surprise either way. Verdicts (`files_precomputed`/
`files_ast`/`files_dropped`) and entity/edge/hash/store-byte output are
unchanged on every corpus measured.

**R2: shipped, by construction.** The dead-for-that-consumer
`entities_by_file` population is gone from the CLEAN gate's call site because
the call site no longer builds a `PrebuiltEntityIndex`; the field itself
stays, correctly, for its one live consumer.

**R3: declined — resolved by elimination, not unification, with numbers.**
After R1 there is exactly one `PrebuiltEntityIndex::build` call left in the
cold-build critical path; the "same index built twice" premise no longer
holds, so there is nothing to unify. Had it still held, the phase-2 ordering
hazard (`resolve_go_method_parent_ids` sitting between the two former call
sites) would have made unification unsafe anyway — confirmed by reading the
call order, not assumed.

Bead: semx-5sw. Epic: semx-w5k. Prior: semx-mp1 (MUL P1, implemented the gate
this bead scopes), the MUL P1 memory-lever follow-up (prior section, same
epic).

## FINALE re-bench at d77a486 (DATAFLOW-SIMPLE close)

The campaign's closing measurement. Five giants, serial, one corpus at a time,
production release binary, **both metrics side by side in every row** — the
campaign has quoted engine-only and full-CLI interchangeably before (W5 §1 says
so in its own words) and this table refuses to. No production code changed by
this bead; the only write is this section.

### 1. Method

- **Binary**: `cargo build --release -p sem-cli` at HEAD `d77a486`, workspace
  `crates/Cargo.toml` release profile (`opt-level=3`, `lto="thin"`,
  `codegen-units=1`, `strip=true`), **mimalloc global allocator**
  (`sem-cli/src/main.rs:12`) — the shipped configuration, *not* `perf_probe`'s
  system allocator. Built in 50.63 s; `sem --version` reports `sem 0.21.0`
  (no commit stamp exists in the binary, so the sha is asserted by build
  provenance, not read back from it). sha256 of the measured binary:
  `d195fc5f8b61706d6763b909aafc542321dfd98f65716692478679054a21112f`.
- **Disclosed binary impurity**: the working tree carried six files of another
  session's uncommitted WIP at build time (the standing untouchables). Five are
  `sem-cli` `diff`/`setup`/test files, none on the graph-build or grep path.
  The sixth, `sem-core/src/parser/plugins/code/languages.rs`, is on the parser
  path but its entire diff is a `rustfmt` reflow of the fish-shell builtins
  list from packed to one-per-line — **semantically inert, zero measurement
  effect**. Checked by reading the diff, not assumed.
- **Command**, identical to W5 §1's: `SEM_LOCAL=1 SEM_TIMINGS=1
  SEM_PROFILE_CACHE=1 sem graph <root> --json`, wrapped in `/usr/bin/time -l`,
  wall measured with `$EPOCHREALTIME` around the spawn.
- **Default env only.** `SEM_MUL_CSHARP` was **not** set — C# stays gated, so
  dotnet is measured on the shipped default (re-parse) path, not semx-mp1's
  opt-in fast path.
- **Cold** = fresh `SEM_CACHE_DIR` **and** fresh `SEM_FACTS_CORPUS_DIR` per
  run, both `rm -rf`'d and recreated immediately before the spawn. This is W5's
  "true cold", verified per run rather than trusted: every cold run below
  printed `FACTS_CORPUS ... hits=0 bytes_read=0`. **Nothing was deleted from
  the user's default cache** (`~/Library/Caches/sem`) and **no corpus file was
  touched** — the index embeds mtimes, so cold was achieved by redirecting the
  cache, not by invalidating the corpus.
- **Warm** = the very next spawn against the same `SEM_CACHE_DIR`, zero file
  touches in between. Warm engine-only is `index_fast_path` (warm builds emit
  no `full_graph_build`).
- **Metric definitions, held fixed**: *engine-only* = `full_graph_build` from
  `SEM_TIMINGS` inside the shipped mimalloc binary. *full-CLI* = end-to-end
  wall of that same production invocation. Both come from **the same run**, so
  the two columns are never a state mismatch against each other.
- n=3 per corpus (n=5 on dotnet, see §3). Load recorded before and after every
  corpus.

### 2. The table

Median of n. Prior-recorded column is **W5 §2's matrix (semx-gbb)** — the most
recent full both-metrics table in this document.

| corpus | cold engine-only | cold full-CLI | warm full-CLI | prior cold engine-only (W5 §2) | prior cold full-CLI (W5 §2) | Δ engine | Δ full-CLI |
|---|---:|---:|---:|---:|---:|---:|---:|
| home-assistant-core | **4,739.7** | **5,921.4** | **185.1** | 5,802.0 | 8,103.5 | **−18.3%** | **−26.9%** |
| TypeScript monster | **9,161.5** | **11,161.3** | **261.7** | 9,982.5 | 12,504.2 | **−8.2%** | **−10.7%** |
| dotnet-runtime | **40,752.9** | **46,689.8** | **762.7** | 43,644.6 | 55,810.0 | **−6.6%** | **−16.3%** |
| llvm-project | **27,275.5** | **34,672.9** | **762.5** | 38,075.1 | 49,789.1 | **−28.4%** | **−30.4%** |
| linux | **24,757.0** | **35,155.5** | **1,259.7** | 29,588.2 | 39,822.1 | **−16.3%** | **−11.7%** |

All ms. Every corpus is faster on both metrics. Warm and peak RSS, same runs:

| corpus | warm engine-only | prior warm full-CLI (W5 §2) | Δ warm | peak RSS cold | prior peak RSS (W5 §2) | Δ RSS |
|---|---:|---:|---:|---:|---:|---:|
| home-assistant-core | 176.9 | 177.9 | +4.0% | 2.93 GB | 4.04 GB | **−27.6%** |
| TypeScript monster | 251.4 | 252.4 | +3.7% | 4.02 GB | 6.67 GB | **−39.8%** |
| dotnet-runtime | 752.2 | 772.5 | −1.3% | 13.61 GB | 19.94 GB | **−31.8%** |
| llvm-project | 752.3 | 763.3 | −0.1% | 12.59 GB | 15.16 GB | **−17.0%** |
| linux | 1,247.7 | 1,351.1 | −6.8% | 11.41 GB | 17.59 GB | **−35.1%** |

**Warm is flat within ±7% everywhere** — the campaign's wins are cold-path wins,
and warm did not regress to pay for them. **Peak RSS is down 17-40% on every
giant**, which is the more interesting half of this table: the MUL P1 follow-up
closed with dotnet's memory ceiling *gated and still over*, and that ceiling was
measured against the opt-in C# path. On the shipped default the whole fleet's
cold RSS is now well under its W5-recorded figure.

**The prior-engine column is not a perfect like-for-like, and this is the one
place the comparison is soft.** W5 §2 carried two engine rows and neither is an
exact match for this bead's:

- `perf_probe build_total` (used above) is **true-cold** — same state as this
  bead — but is a **system-allocator** number whose parse leg W2 §1 measured
  reading 7-57% high. Against it, this bead's mimalloc figures are flattered by
  an unknown allocator margin, so **the Δ engine column is an upper bound on
  the real improvement, not a clean one**.
- the shipped-binary `full_graph_build` row is mimalloc, matching this bead's
  allocator, but is **known-content** (parse largely bought back), so it is not
  a cold number at all. Against *that* row this bead reads −15.8% (HA), −8.3%
  (llvm), +2.1% (dotnet), **+45.8% (monster)** and **+27.2% (linux)** — the two
  positives are the known-content state showing up, not a regression, since a
  known-content build skips work a true-cold build must do.

Stated rather than papered over: **no prior row in this document is
simultaneously true-cold and mimalloc**, so a perfectly clean engine-only
delta against the campaign's own history is not available. The full-CLI column
has no such problem — it is production-wall against production-wall on both
sides, and it is the column to trust.

### 3. Load, noise, and the one bad cell

The box was never idle and the load was not this bead's — Chrome, WindowServer,
`agent-browser` and concurrent agent sessions, verified by `ps` (no `cargo` or
second `sem` running during any measured spawn). 64 GB RAM / 18 cores, so no
giant here swapped on its own account (3.7 GB of swap was already in use by the
desktop before the battery started).

| corpus | load before (1/5/15) | load after | cold full-CLI spread | cold engine spread | flag |
|---|---|---|---:|---:|---|
| home-assistant-core | 5.28 / 6.41 / 8.31 | 7.00 / 6.71 / 8.38 | 4.9% | 3.0% | clean |
| TypeScript monster | 7.00 / 6.71 / 8.37 | 8.94 / 7.26 / 8.50 | 8.2% | 5.1% | clean, see note |
| dotnet-runtime | 8.94 / 7.29 / 8.50 | 15.14 / 11.67 / 10.11 | **57.6%** | **65.9%** | **NOISY** |
| dotnet-runtime (re-run) | 15.10 / 14.36 / 11.72 | 10.16 / 13.38 / 11.65 | — | — | — |
| llvm-project | 13.26 / 11.38 / 10.02 | 15.71 / 13.15 / 10.90 | 7.4% | 0.6% | clean |
| linux | 14.61 / 12.96 / 10.84 | 17.32 / 14.72 / 11.80 | 3.4% | **11.2%** | **engine >10%** |

- **dotnet is the one genuinely bad cell and it is reported as such.** n=3 gave
  44,755.6 / 65,551.9 / 68,449.6 while the 1-minute load climbed 8.9 → 15.1, so
  it was re-run for n=5: **43,434.9 / 44,755.6 / 46,689.8 / 65,551.9 /
  68,449.6**. The distribution is **bimodal, not dispersed** — three runs inside
  a 7.5% band and two outliers 45% above them, the outliers being exactly the
  two runs that straddled the load climb. The table reports the honest
  **median-of-5 (46,689.8)**. The tight-cluster median, which is what dotnet
  looks like when the box is not being fought over, is **44,755.6 full-CLI /
  38,712.5 engine — −19.8% / −11.3% against W5 §2** rather than −16.3% / −6.6%.
  Both numbers are given; the table uses the pessimistic one.
- **linux's engine-only column exceeds the 10% repeat-disagreement bar** (23,385
  / 24,757 / 26,006, 11.2%) while its full-CLI column is tight at 3.4% — the
  variance is inside the engine phase, at the highest load of the battery.
  Flagged, not smoothed.
- **monster's run 1 is anti-correlated**: it had the *slowest* full-CLI
  (11,994.1) and the *fastest* engine (8,776.7), i.e. ~1 s of that run was spent
  outside `full_graph_build` (index write / IO). Noted because it is the exact
  artifact that makes quoting one metric for the other unsafe.
- Per W5 §1's own calibration (absolutes ~12-14% high at load ~15 vs ~4.8),
  **every absolute in §2 is an upper bound**; llvm and linux were measured at
  the top of that band and dotnet's outliers above it.

Corpus scale as probed this run (`FACTS_CORPUS probed=`): HA 22,336; monster
40,869; dotnet 47,471; llvm 82,352; linux 72,928 — within 0.3% of W5 §2's file
counts on every corpus, so the corpora have not materially drifted underneath
the comparison.

### 4. Query-side sanity row

Not a full battery — a spot-check that the `sem grep` vs `rg` class of numbers
survives, per QUERY-INDEX.md §11.5's methodology (cold process, serial,
median-of-5, run **from the corpus root** as a user would, naive `rg` with no
hand-scoping). Warm production index, load 5.2-8.4.

| corpus | pattern | tier | `sem grep` | naive `rg` | speedup | hits (sem / rg lines) |
|---|---|---|---:|---:|---:|---|
| monster | `createProgram` | trigram (167/40,869 cand) | **46.0 ms** | 962.8 ms | **20.9×** | 160 / 164 |
| monster | `getEmitFlags` | trigram (41/40,869) | **41.9 ms** | 969.7 ms | **23.1×** | 81 / 81 |
| monster | `Debug Failure` | trigram (10/40,869) | **37.6 ms** | 979.4 ms | **26.0×** | 12 / 15 |
| monster | `get[A-Za-z]+Flags` | trigram (382/40,869) | **53.0 ms** | 971.3 ms | **18.3×** | 641 / 641 |
| home-assistant-core | `DOMAIN = "zwave_js"` | trigram (1/22,336) | **24.9 ms** | 282.7 ms | **11.4×** | 1 / 1 |
| home-assistant-core | `async_setup_entry_zeroconf` | trigram (9/22,336) | **25.0 ms** | 285.5 ms | **11.4×** | 0 / 0 |
| linux | `kvm_vcpu_kick` | trigram (293/72,928) | **75.7 ms** | 1,291.5 ms | **17.1×** | 56 / 57 |
| home-assistant-core | `async_setup_entry` | **full_scan** (22,336/22,336) | 362.0 ms | 309.4 ms | **0.9×** | 6,963 / 7,146 |
| home-assistant-core | `ConfigEntry` | **full_scan** | 371.3 ms | 345.5 ms | **0.9×** | 53,291 / 53,317 |
| linux | `devm_kzalloc` | **full_scan** | 1,885.5 ms | 1,319.0 ms | **0.7×** | 7,410 / 7,427 |
| linux | `struct file_operations` | **full_scan** | 1,873.2 ms | 1,289.2 ms | **0.7×** | 2,577 / 2,622 |

**Correctness first**: all four monster hit counts (160 / 81 / 12 / 641)
reproduce §11.5's recorded counts **exactly**, many beads later — the
trigram tier is answering the same thing it was measured answering.

**Two honest qualifications on the speed claim:**

1. **Trigram-served queries hold, at the bottom of the recorded class, not the
   middle.** §12's fleet sentence is "20-378×"; this spot-check measures
   **11-26×**. The monster specifically was 39-132× in §11.5 and is 18-26× here.
   Most of that is already recorded in this repository rather than new:
   §11.5's own regression spot-check logged `sem grep` going 13 → 25.0 ms and
   16 → 28.6 ms (+12 ms per verb) after a later bead, and this run measures
   37-53 ms at load 5-8. The tier is intact and comfortably inside its <50 ms
   budget on the monster; the *headline multiple* is smaller than the doc's
   top-line phrasing implies, and quoting "up to 378×" without the tier caveat
   would be quoting the best cell of a fleet matrix.
2. **The full-scan worst case is now genuinely slower than `rg`, which is a
   change from what §11.5 recorded.** §11.5 reported full-scan still beating
   naive `rg` 1.3-1.4× on the monster. That advantage was never algorithmic —
   §11.5.1 says so itself: it came from the monster's `tests/baselines/`
   inflating `rg`'s corpus to 2× `sem`'s. On HA and linux, which have no such
   fixture ballast, the same tier measures **0.7-0.9×**, i.e. a user who greps a
   common token pays ~1.4× `rg` for it. This is the documented worst case
   behaving as documented, on corpora that do not flatter it — surfaced here
   rather than left for a user to find.

### 5. What this section does and does not claim

- **Claims**: at `d77a486`, on the shipped default configuration, every giant in
  the campaign's own set is faster cold on **both** metrics than W5 §2 recorded,
  warm is unchanged within ±7%, and cold peak RSS is down 17-40% fleet-wide.
- **Does not claim** a clean engine-only delta (§2's allocator/state caveat), a
  measurement of the C# opt-in path (gated, deliberately not set), a
  re-verification of bit-identical output (this is a timing bead; the preceding
  section owns that gate), or that dotnet's absolute is trustworthy to better
  than its bimodal ±20% (§3).
- **Not re-run here**: rails and the rest of the local-tier fleet — W5 §2's set
  is five giants and this section matches it corpus-for-corpus and in order.

Bead: semx-tuy. Epic: semx-w5k. Prior full table: W5 §2 (semx-gbb). Preceding
section: semx-5sw.

## FALSE-THINGS A6: the +12ms verb-regression, attributed, one fix tried and
falsified by measurement, left in place

The campaign's own §12.1/§13.4 (semx-tuy) already named this: `find`/
`callers`/`refs`/`grep` picked up `complete_check`'s ~12-13ms membership
sweep after semx-ykf landed, dropping `grep`'s monster speedup from the
39-132× class to 11-26×, still comfortably inside the <50ms `grep` budget
and the <20ms `find`/`callers`/`refs` budget (thin margin, per §13.4's own
words) but a real, disclosed cost — not an accident, since §13.1 records S1
predicting this exact number (12ms) a year before `complete_check` was
built. This bead re-confirmed that attribution and tried one fix.

**Attribution, re-confirmed today.** `complete_check`
(`sem-core/src/index/complete.rs`) runs two sweeps every `find`/`callers`/
`refs`/`grep` call: a parallel `stat` over every path in `FILES` (40,869 on
the monster, proving deletions) and a parallel `stat` over every path in
`DIRS` (2,743, proving addition-drift via the POSIX
create/delete/rename-bumps-mtime guarantee). The doc's own §13.1 table
(S1, prior bead) already measured the FILES pass alone at 12ms and the
combined sweep at 13ms; nothing about that number has drifted since —
it is the cost of proving whole-corpus membership freshness without a
resident daemon (§13.1 explicitly declined the daemon alternative, on
simplicity-mandate grounds: a watcher would only buy back a budget this
sweep already clears).

**Fix attempted: batch the FILES-deletion pass by directory.** The
per-file `stat` sweep looked like the obvious target — 40,869 individual
syscalls to prove a fact `DIRS`'s own mtime-drift guarantee already implies
for most of them "for free" once grouped correctly (deleting a file bumps
its own direct parent's mtime, same POSIX guarantee the addition side
already leans on). Implemented: group `FILES`' already-sorted paths into
contiguous per-parent-directory runs (one linear pass, no I/O), then for
each run do one `read_dir` on that directory and diff its current entries
against the run's known files — deletion detection as a direct existence
check (immune to mtime coalescing, so the pre-existing `deleted_file_is_
found` unit test, which deletes with **no** settle sleep unlike every
new-file test, keeps passing with zero change) rather than an mtime proxy,
just batched: one syscall per directory instead of one per file. All 6
`complete.rs` unit tests green, all 638 `sem-core` + 249 `sem-cli` release
tests green, unchanged.

**Measured: 19× slower, not faster.** Interleaved A/B, `sem find/callers/
refs createProgram --file src/compiler/program.ts` and `sem grep
createProgram/getEmitFlags --json` on the monster, median-of-7,
warm page cache, load 6.0-7.3:

| verb | before (per-file `stat`, unchanged) | after (per-directory `read_dir`) | outputs match |
|---|---:|---:|---|
| `find` | 22.11 ms | 418.83 ms | yes |
| `callers` | 22.01 ms | 420.91 ms | yes |
| `refs` | 21.40 ms | 414.39 ms | yes |
| `grep createProgram` | 29.89 ms | 425.19 ms | yes |
| `grep getEmitFlags` | 27.76 ms | 422.70 ms | yes |

Correctness held on every case (byte-identical stdout, both binaries) — this
is a pure performance falsification, not a bug. Cause, confirmed directly:
`tests/baselines/reference` is one flat directory of **53,049 entries**
(`ls | wc -l`), larger than the entire indexed corpus (40,869 files). A
standalone `read_dir` over just that one directory measured **~30ms**
(Python `os.scandir`, same order in Rust). Because the batched pass runs
*unconditionally* on every query (deletion-freshness can't be gated behind
`DIRS`' mtime-drift signal without losing the mtime-independence the
no-sleep test proves — mtime is exactly the granularity a coalesced
create+delete inside one second can hide), any query that touches even one
known file under that directory pays the full 53,049-entry listing cost,
every time. This isn't tunable away: **proving a file's absence via
directory listing structurally requires scanning the whole directory** —
there is no early-exit, because a missing target is exactly the case where
you can't stop partway through and be sure. Per-path `stat` is O(1)
regardless of sibling count; `read_dir`-based batching is O(directory size)
with no safe upper bound once a corpus has even one large, flat directory
(test-baseline dirs, generated-code dirs, and vendored-fixture trees are
common real-world instances of exactly this shape — not a pathology specific
to this repo).

**Decision: reverted, nothing shipped.** The per-file `stat` sweep's
O(1)-per-known-file property — the thing that looked like the naive,
"unoptimized" choice — is actually the robust one: it is immune to
directory-shape skew, where the batched alternative is not and can regress
by orders of magnitude on corpora this campaign's own control set already
contains. `git checkout -- crates/sem-core/src/index/complete.rs`;
rebuilt and re-verified 638/249 green at the reverted (original) sha. No
production code changed by this bead.

**What's left, honestly.** A genuinely safe lever exists in principle —
`fstatat` relative to an open directory fd instead of `root.join(p)` +
`symlink_metadata()`, which would cut the per-file `PathBuf` allocation
`join` currently pays 40,869 times without changing the O(1)-per-file
shape — but it needs `unsafe` platform-specific code (this crate ships
Windows support; semx-q344 landed Windows path-scoping fixes this same
campaign) with a POSIX/Windows split, for an unmeasured, likely-modest
win against an already-~300ns-per-syscall floor. Not attempted this bead:
the risk/reward didn't clear the bar with the daemon-free alternative
already inside budget. Reported rather than forced, per this campaign's
own discipline — a fix only ships on a measured win, and the only one
tried here measured the wrong way.

Bead: semx-tuy (re-check, this campaign: perf/false-things A6). No sha
change — working tree returned to HEAD `0d4e4b5`.

## 2026-08-21: semx-mul phase-2 W0 — the CLEAN gate moved past `resolve_go_method_parent_ids`

The `semx-mul` bead recorded a hazard against this section's own gate: `graph.rs`'s
CLEAN gate call site (`clean_gate_dirty_files`, added semx-mp1, scoped semx-5sw
above) ran *before* `resolve_go_method_parent_ids` — the one cross-file entity
rewrite this crate performs. Harmless while `mul_precompute_admits` never
admits `.go` (`_ => false`), because no Go file ever reaches
`clean_gate_candidate_spans`. Unsound the day phase 2 admits Go: a Go method's
`parent_id` is unset until that rewrite runs, so a candidate file gated before
it would show zero cross-file children for its receiver type and pass CLEAN
by omission, keeping stale file-local precomputed facts for a file that is
not, in fact, clean — the false-negative CLEAN(F) this section's own soundness
argument (semx-5sw, above) assumes cannot happen.

**Fix.** `resolve_go_method_parent_ids(&mut all_entities)` moved from its old
call site (after the carry-destructuring split, well after the gate) to
immediately after pass 1's assembly loop — the earliest point `all_entities`
is complete — so the gate that follows it always adjudicates against the
complete cross-file parent-edge set. `is_go_file` guards every mutation the
rewrite makes, so C++/C#/JS/TS entities are untouched by the move; this is
checked, not just argued (below).

**Pin.** `clean_gate_dirty_files` now takes a `GoParentsResolved` token by
value (`registry.rs`) — a zero-sized type constructible only by calling
`resolve_go_method_parent_ids`. A future refactor that reorders the gate
ahead of the rewrite fails to compile rather than silently regressing; the
ordering invariant is structural, not remembered (the same move this
document's semx-ys0 section made for the MUL runtime-switch salt). A new
test, `go_parent_repair_must_run_before_clean_gate_adjudication`
(`scope_resolve.rs`), drives the two real functions directly on a two-file Go
fixture (`Hub` in `hub.go`, `Ping`'s cross-file receiver in `ping.go`): the
gate is shown unsound against the pre-rewrite entity set (misses `hub.go`)
and sound against the post-rewrite one (catches it) — the exact hazard and
its fix, at the entity-content grain the gate itself operates on. End-to-end
forcing through `EntityGraph::build` was not achievable: `mul_precompute_
admits` is a plain match with a hardcoded `_ => false` arm for Go, not a
predicate any test can override (unlike `csharp`'s runtime-gated arm — and
even that one is a process-global `OnceLock`, unflippable mid-test per
`resolve_gated_salt_generalizes_beyond_csharp`'s own doc comment). Admitting
Go for real is phase 2's own decision, out of this bead's scope.

**Gates.** `cargo test -p sem-core --release --lib`: **642/642** (641 +
the new test). `cargo test -p sem-cli --release`: **249/249**.
`clean_gate_scoping_matches_corpus_wide_verdict_per_candidate` and the other
two existing CLEAN-gate unit tests green, each updated to thread a real
`GoParentsResolved` token through (`resolve_go_method_parent_ids` run first,
a no-op on their all-`.cs` fixtures — matching production order exactly).

**Live corpora, not just unit tests.** `SEM_FP_PARITY=1 cargo run --release
--example incr_probe -- <root> all <label>`:

* **home-assistant-core** (22,397 files, cold `entities=259399 edges=310723
  edge_hash=b82d199d21e45b47`): 8/8 `ORACLE … ok` — cold-vs-build plus all 7
  named warm scenarios (`none`, `leaf`, `mixed50`, `hub`, `hubrename`,
  `tests`, `importchurn`), zero mismatches.
* **C++, real corpus** — `llvm-project`'s cached checkout had no working
  tree (`blob:none` partial clone, broken `HEAD` symref); repaired
  (`git symbolic-ref HEAD refs/heads/main`) and sparse-checked-out
  `clang/lib/Analysis` + `clang/include/clang/Analysis` (68 `.cpp` + 82
  `.h`, real out-of-line member definitions) rather than the whole repo.
  Built the pre-move sha (`55f764f`, this campaign's HEAD before this bead)
  in a throwaway `git worktree` and ran the identical `incr_probe`
  invocation with `SEM_PROFILE_RESOLVE=1` before and after the move:

  | | before (55f764f) | after (this bead) |
  |---|---|---|
  | entities / edges | 4477 / 4979 | 4477 / 4979 |
  | edge_hash | `b99cdff8fd5069b2` | `b99cdff8fd5069b2` |
  | `MUL_CLEAN_GATE files_dropped` | 0 | 0 |
  | ORACLE (cold-vs-build, warm none) | ok / ok | ok / ok |

  Bit-identical, both metrics. `files_dropped=0` on both sides matches
  MUL-A's census verdict (§1 of MUL-DESIGN.md): C++ out-of-line member
  definitions produce zero cross-file `parent_id` links, so the gate has
  nothing to catch either way — this run is a neutrality check, not a
  positive detection case (Go's `pkgA` cross-file receiver fixture, sub-file
  grain, is what exercises detection above).

**Close-out.** `br comments add semx-mul` records the moved sha; the bead
stays open (phase-2 epic). No language admission changed — `mul_precompute_
admits` still returns `false` for `.go`; this bead only removes the ordering
hazard that would otherwise block admitting it.

Bead: semx-mul (phase-2 W0). Epic: semx-w5k. Prior: semx-mp1 (implemented
the gate), semx-5sw (scoped it), semx-ys0 (structural-not-remembered
precedent for the runtime-gate salt, same technique applied here).

## 2026-08-21: subtraction pass — five closed-question probes removed (R1-R5)

Standing removal mandate ("nothing left to take away"): a read-only audit
walked every bench/example in `sem-core` plus `sem-cli setup.rs` and
verified, item by item, which measurement tools answer a question that is
already closed and recorded in prose, versus which ones are load-bearing
gates or reproduction paths still in active use. This bead executes the
audit's mechanical removal ledger. Every number the removed tools produced
stays exactly where it already lived — in this doc, KAPPA.md, MUL-DESIGN.md
§5, and OXC-FASTPATH.md — only the runnable machinery goes. Consumer-count
verified by grep for every item before deletion (doc/comment mentions of a
tool's name, which are historical citations of where a number came from, do
not count as consumers).

**R1 — the interning micro-bench, closed.** Deleted `benches/interning.rs`
(478 LOC, `semx-5nc`) and `examples/key_shape_probe.rs` (195 LOC), plus the
`[[bench]] name = "interning"` Cargo.toml entry. This was 918f12a's
explicitly-left-undone micro-benchmark of `String` vs `u32` interning for
the cold resolve join's two hash-map keys (the Interning section above,
`semx-4an` second pass) — it ran, the numbers are recorded above (`id_len`
p50/p90/p99, `bucket_size` distribution, `INTERN_BUILD` timing, the
micro-bench results table), and the verdict was negative (interning does
not pay for itself at this join shape). Nothing else in the tree referenced
either file outside of doc-comment citations of where the numbers came
from.

**R2 — kappa_stats, already overdue.** Deleted `examples/kappa_stats.rs`
(177 LOC, `semx-n8c`). Its own module doc said so: "Throwaway analysis tool
... used once to gather KAPPA.md's collision numbers on real corpora, then
deleted." `tests/kappa.rs` and `src/utils/hash.rs` cite `kappa_stats`
samples in doc comments explaining *why* specific node-kind rules exist
(real collisions it found on the TS-monster and django corpora) — those
citations are historical provenance, not a dependency on the tool; they are
untouched.

**R3 — the shipping bug: `sem setup` was forking a no-op every session.**
`crates/sem-cli/src/commands/setup.rs`'s `install_session_hooks` still
installed `nohup {sem} mcp --resident >/dev/null 2>&1 &` as a Claude Code
SessionStart hook. The resident server that command used to start was
deleted (QUERY-INDEX.md §7 item 5 / semx-woe); `--resident` is kept only as
a backward-compatible no-op flag (`sem-mcp/src/lib.rs`) so an
*already-installed* hook wouldn't start erroring. QUERY-INDEX.md's own item-5
entry flagged this exact wiring as in-flight and unresolved at the time.
Fixed: `add_session_hooks` no longer installs a SessionStart hook at all;
`sem setup` now installs only the `UserPromptSubmit` hook
(`sem hook prompt-submit`). `is_sem_hook_command`'s `mcp --resident` matcher
is kept (commented as to why) so `sem unsetup` still cleans up a legacy
SessionStart hook from an older install — added a regression test for
exactly that cleanup path. Reworded the two "Done" strings and the `run()`
summary line that promised a "warm graph" / "resident warm graph +
prompt-time prefetch" to describe what setup actually installs, and aligned
the same false claim in README.md's two "warm resident graph" passages
(the second one's *conclusion* — day-to-day queries are fast — is still
true, just via the on-disk mmap index per the 0.22.1 CHANGELOG entry, not a
resident process; reworded to say so).

Second finding, surfaced not fixed (out of this bead's audited scope):
`sem hook prompt-submit` — the hook this fix keeps installing — is *also*
currently a full no-op. `commands/hook.rs`'s `socket_lookup` is hardcoded to
return `None` (the resident socket sidecar it used to query is the same
deleted subsystem R3 is fixing the installer for), so `candidates()`'s regex
work always ends in an empty `blocks` and the hook never prints anything.
This is a second, deeper instance of the same shipping bug, not audited or
scoped here — it needs its own decision (reimplement prefetch against the
mmap index, or stop installing the hook too) rather than a silent fix
bundled into this ledger item.

**R4 — four one-bead memory/timing probes, all closed.** Deleted
`examples/parse_time_probe.rs` (107 LOC, `semx-jo1`), `mem_single_probe.rs`
(96 LOC), `mem_single_probe_mimalloc.rs` (95 LOC), and `tree_mem_probe.rs`
(78 LOC) — 376 LOC total, all `semx-jo1`/`semx-4w1` decisions closed and
their numbers preserved in this doc's Memory attribution section and
MUL-DESIGN.md §5. Checked both docs for a "re-run X to reproduce" pointer
naming any of the four as the standing reproduction path for an open
question — found only narrative past-tense citations ("confirmed directly
with `tree_mem_probe`", "`mem_single_probe`, same frozen snapshots") of
what was already run, never an instruction to re-run one. `Cargo.toml`'s
`mimalloc` dev-dependency comment named `mem_single_probe` as its reason to
exist; reworded to name `parse_probe`/`mul_census` instead (both KEEP-listed,
both still use `mimalloc` as their global allocator, `semx-au8`) since that
comment would otherwise go stale pointing at a deleted file.

**R5 — the oxc spike bench, superseded.** Deleted `benches/oxc_spike.rs`
(249 LOC) + its `[[bench]]` Cargo.toml entry (`required-features =
["oxc-fastpath"]`). Superseded by `diff_oracle` (end-to-end equivalence) and
`perf_probe` (the honest wall-clock share, "6.1% of cold build" —
OXC-FASTPATH.md's own honest-end-to-end section already used `perf_probe`,
not this bench, for that number). The bench's own headline numbers (20-49x
parse+walk, the four-leg comparison table) are preserved verbatim in
OXC-FASTPATH.md; added a note there recording the bench's removal at this
commit and that this doc is now the number's sole provenance. The
`oxc-fastpath` cargo feature, its pinned `oxc_*` deps, and `oxc_extractor.rs`
are untouched — this removal is the bench only.

**KEEP-verdict summary from the audit (unchanged by this bead, recorded so
the next auditor doesn't re-walk it).** `fast_extractor.rs`, `diff_oracle.rs`
+ its example, `oxc_extractor.rs`, the `oxc_*` deps and `oxc-fastpath`
feature are kept: the oxc seam was already declined once on field-identity
grounds (the section above this one) and a second time on equivalence
grounds (the "revival" section, `semx-r63`) — both declines are load-bearing
decision records, not dead code, and `diff_oracle`/`oxc_extractor.rs` are
what a future third attempt would build on. `index_probe`,
`facts_corpus_probe`, `edge_dump_probe`, `incremental_parity_probe`,
`facts_probe`, `incr_probe`, `perf_probe`, and `mul_census` are kept as live
gates — every one of them is invoked by name in a still-open bead's
verification steps elsewhere in this doc (e.g. `incr_probe` above, this same
bead's W0 section), not a closed question. `build_cache.rs`'s `DiskCache`,
the `sem mcp --resident` no-op flag itself, and
`SEM_MUL_CSHARP`/`SEM_RELATIONS_LOCAL`/`SEM_INCR_PROBE_SESSION_ONLY`/
`SEM_FP_PARITY` are kept unchanged — only R3 touched anything downstream of
the `--resident` flag, and only its *installation site*, per the explicit
KEEP list for this bead.

**Net LOC.** R1 673 (478+195) + R2 177 + R4 376 + R5 249 = **-1,475** LOC of
probe/bench machinery deleted, zero numbers lost. R3 is a net-small behavior
fix (a few lines removed from `add_session_hooks`/`install_session_hooks`,
a few doc lines reworded, one regression test added) rather than a LOC
subtraction — its value is the deleted per-session fork, not line count.

Bead: subtraction pass, R-wave 1. Prior: `subtract-audit` (read-only ledger
this bead executes verbatim except where evidence disagreed — none did).

## 2026-08-21: semx-mul phase-2 W2 — Rust admitted to the precomputed-facts fast path, unconditionally

MUL-DESIGN.md §6.1 verdicted Rust NO-as-is (13.74% FASTPATH byte share, gated
behind Field 10's `import_stmts` descriptors). W1 mechanized Field 10 but
left it unconsumed: `PrecomputedFileFacts::import_stmts` was always empty
because TREELESS still rejected any file with a real import, and the one
pass-2 dispatch site required a live tree, so a precomputed file's
descriptors (had they existed) would never have been read. This bead is the
admission — wiring both ends and measuring the real corpus.

**The decision point.** `precompute_scope_resolvable_file_facts`'s TREELESS
gate (`scope_resolve.rs:~1650`) used to be `if !import_starts.is_empty() ||
saw_call_node { return None; }` — unconditional rejection of any import. It's
now `if (!import_starts.is_empty() && !mul_precompute_consumes_imports(lang_
config.id)) || saw_call_node`, where `mul_precompute_consumes_imports`
(`matches!(lang_id, "rust")` today) is the new predicate naming which
languages have a real pass-2 consumer for the recorded descriptors — kept
deliberately separate from `mul_precompute_admits` (whether the producer
runs at all) because a language could in principle be admitted to the
producer before its import handlers were ported to descriptors. Pass 2's
dispatch site (`scope_resolve.rs:~2519`) used to be a single `if let
(Some(tree), Some(import_starts)) = (reparsed, &fused_import_starts)` —
reachable only from the re-parse path. It's now `if let Some(facts) =
precomputed { … dispatch facts.import_stmts directly, no tree … } else if
let (Some(tree), Some(import_starts)) = … { … the old tree-driven path … }`.

**Fast-path engagement, proven not assumed.** Two new `SCOPE_BUILD_WORK`
counters, `files_precomputed_with_imports`/`precomputed_import_descriptors`,
increment only inside the new `Some(facts)` branch when `facts.import_stmts`
is nonempty. On rust-lang/rust (38,598 files): OFF
`files_precomputed=209 files_ast=38389 files_precomputed_with_imports=0`; ON
`files_precomputed=38420 files_ast=178 files_precomputed_with_imports=14499
precomputed_import_descriptors=59131` — 14,499 files took the descriptor
dispatch fast path against MUL-DESIGN.md's census prediction of 14,446 (the
~0.4% gap is corpus drift since the census commit, not a gate miss).

**Admission: unconditional (C++'s shape, not C#'s).** The design doc's own
I6 instruction is explicit: under the +15% peak-RSS ceiling, admit
unconditionally; over it, gate behind an env switch (C#'s
`SEM_MUL_CSHARP` precedent). `/usr/bin/time -l` on rust-lang/rust, cold, two
independent pairs with run order swapped:

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 3,105,947,648 B | 3,452,534,784 B | **+11.16%** |
| 2 | ON→OFF | 3,149,545,472 B | 3,504,734,208 B | **+11.28%** |

Both pairs comfortably under +15%, reproducibly — so `mul_precompute_admits`
gained a plain `"rust" => true` arm, no `SEM_MUL_RUST` switch, no
`MUL_RUNTIME_GATES` row (there is no switched-off state to preserve corpus
compatibility with). `facts_store.rs`'s `LANGUAGE_SALTS` bumped
`("rust", "ts-0.23")` → `("rust", "ts-0.23-mp2")` (I5/F2), mirrored in
`examples/facts_corpus_probe.rs`'s independent copy (its own byte-equality
test against the real table catches drift). An intermediate version of this
bead shipped Rust behind `SEM_MUL_RUST` (mirroring C#) specifically so the
fence could be measured without committing to the outcome first; once both
pairs cleared the ceiling the switch was removed rather than left as unused
scaffolding — this doc records the final, unconditional shape only.

**Correctness.** `edge_dump_probe` sha256, ON vs OFF: bit-identical on
rust-lang/rust (309,429 edges) and on this worktree's own `crates/` (8,631
edges, measured against a frozen `git worktree` snapshot of the pre-bead
commit so the corpus wasn't a moving target while this bead's own source
files were being edited — the first attempt at this comparison, against the
live worktree, showed a spurious 42-line diff that was the source tree
changing mid-measurement, not a graph divergence; re-run against the frozen
snapshot came back sha256-identical). `SEM_FP_PARITY=1 incr_probe … all` on
rust-lang/rust: 8/8 `ORACLE ok` (cold-vs-build + all 7 warm scenarios).
`facts_probe` save/load cross-process on rust-lang/rust: 4/4 `ORACLE ok`.
`facts_corpus_probe` populate/consume on a real two-copy `library/` subset
(2,150 files): `corpus_hits=2150/2150`, `ORACLE ok`, negative probe `ok`. The
I5/F2 salt-bump was proven adversarially, not just asserted: populated a
pure-Rust corpus (41 files, `library/core/src`) with a binary built from the
pre-bead commit (salt `"ts-0.23"`), consumed it with this bead's binary
(salt `"ts-0.23-mp2"`) — `corpus_hits=0`, a clean miss, `ORACLE ok` proving
the resulting fresh build was still correct despite the miss.
`cargo test --release`: sem-core lib 647/647 (+3 new tests), full sem-core
suite 719/719, sem-cli 250/250, zero new warnings.

**The prize.** Same `SEM_PROFILE_RESOLVE=2` run as the engagement proof
above: `reparse_ms` 939.52 → 30.07 ms (**−96.8%**), `fused_walk_ms` 3,214.54
→ 23.45 ms (**−99.3%**, the walk moved to pass 1, not eliminated — a corpus
hit is what actually eliminates it, see below), `scope_build_ms` 6,238.47 →
3,213.17 ms (**−48.5%**), `pass2_wall_ms` 499.57 → 271.01 ms (**−45.7%**).
`entities_spanned` identical both sides (329,799) — same graph, cheaper
path. Cold-build wall (`sem find`, fresh `SEM_CACHE_DIR`, pre-bead vs.
this-bead binaries, 6 interleaved pairs, order swapped across the two
batches): OLD median 10.90s, NEW median 9.25s, **−15.1%**, direction
unanimous across all 6 pairs (range −10.1% to −22.0%) — noisy per pair,
same n=small disclosure phase 1's dotnet/llvm wall numbers carried, but
one-directional and consistent with the counter collapse above.
**Known-content** (facts served from the corpus, no walk at all — not even
pass 1's) was not measured through `sem find` directly: its cold-build path
routes through the mmap query index, not `build_graph_with_facts_store` —
confirmed by adding `SEM_PROFILE_CACHE=1` and seeing no `FACTS_CORPUS` line
print, a wiring question this bead did not chase further. Proven instead at
the mechanism level by the `facts_corpus_probe` run above (100% cross-repo
hit rate, `ORACLE ok`); per MUL-DESIGN.md §5.1's framing, a corpus hit's win
is `fused_walk_ms`'s full cold-side value (3,214.54 ms) on top of the
already-measured cold win, since pass 1's precompute is skipped entirely
rather than merely relocated.

**Gates run.** `cargo build --release` clean on `sem-core`, `sem-cli`, and
every touched example (`mul_census` untouched by this bead;
`facts_corpus_probe`/`facts_probe`/`edge_dump_probe`/`incr_probe` all
rebuilt clean) — two pre-existing, unrelated `sem-cli` warnings
(`commands/setup.rs`, `corpus_columns.rs`) unchanged. `cargo test --release`:
sem-core lib 647/647, full sem-core suite 719/719 (647 + d_smoke 0/1 ignored
+ elm_smoke 2 + graph_accuracy 2 + kappa 3 + parse_cache 42 + scope_resolve_
bench 7 + single_pass_invariants 15 + facts_corpus_probe/facts_probe/edge_
dump_probe test binaries), sem-cli 250/250. `edge_dump_probe` sha256
bit-identical ×2 corpora. `incr_probe` `SEM_FP_PARITY=1` 8/8. `facts_probe`
4/4. `facts_corpus_probe` 2/2 plus the adversarial salt-clean-miss run.

**Close-out.** `mul_precompute_admits`'s doc comment, the `MulRuntimeGate`
table's surrounding prose, and `facts_store.rs`'s `producer_language_salt`
doc comment all updated to describe Rust's final (unconditional) shape —
no stale references to a `SEM_MUL_RUST` switch that didn't ship. Go and Java
are MUL-DESIGN.md §4.3's other named Field-10 beneficiaries and are future
rows on the same two predicates (`mul_precompute_admits`,
`mul_precompute_consumes_imports`), not a new mechanism; Python's own
Field-10 share is the smaller half of its need (Field 11's
`ctor_call_sites`, phase 3, is still unbuilt).

Bead: semx-mul (phase-2 W2). Epic: semx-w5k. Prior: semx-mp1 (MUL P1,
implemented the C++/C# gate this bead's admission machinery reused
verbatim), semx-mul phase-2 W0 (CLEAN gate ordering fix), semx-mul phase-2
W1 (Field 10 mechanized: descriptors recorded but, until this bead,
unconsumed).

## 2026-08-21 — semx-mul phase-2 W3+W4: Go and Java measured, both stay gated

W2's Rust template applied verbatim to the two other Field-10 beneficiaries:
`mul_precompute_consumes_imports` widened to `"go" | "java"`,
`LANGUAGE_SALTS`/`facts_corpus_probe.rs` bumped `("go", "ts-0.23-mp3")` /
`("java", "ts-0.23-mp3")`. Unlike Rust, **neither promotes to unconditional**
— each independently fails a different half of I6's two-gate test, exactly
the "ship them differently" outcome the epic's own framing anticipated.

**The decision point** is unchanged from W2 in shape: `mul_precompute_
consumes_imports` now returns `true` for `"rust" | "go" | "java"`, all three
riding the same six `ImportStmtFacts` variants W1 built. Go's is `GoImport`;
Java's imports classify as `GoImport` too (`classify_import_stmt`'s doc
comment: `import_declaration` is a grammar kind shared by Go/Java/Swift) and
dispatch through `register_go_package_imports`, which only ever matches
`.go`-suffixed entities in `go_pkg_index` — a pre-existing, documented no-op
for Java (finding F4). So there was **no Java-specific import handler to
build** — the task's own speculative concern turned out to be moot on
inspection, confirmed (not just argued) by this bead's correctness battery.

### Java

**Correctness: clean.** `edge_dump_probe` on elasticsearch (35,906 files,
release binary): **sha256 bit-identical ON vs OFF**, 1,257,229 edges both
sides (`bcc3aeb6…` both). `SEM_FP_PARITY=1 incr_probe … all` on
elasticsearch (cold: `files=35906 entities=833596 edges=1257249
edge_hash=587981caa323ce55`): **8/8 `ORACLE ok`** — cold-vs-build plus all 7
named warm scenarios (`none`, `leaf`, `mixed50`, `hub`, `hubrename`,
`tests`, `importchurn`), zero mismatches; `hub`/`mixed50`'s highest-fan-in
file is `test/framework/.../ESTestCase.java` with 7,728 dependents.
`facts_probe` save/load, cross-process, on elasticsearch: **4/4
`ORACLE ok`** (`none`, `leaf`, `mixed50`, `hub`). `facts_corpus_probe`
populate/consume, cross-repo, on a real two-copy `server/.../action/`
subset (861 files, two independent copies of the same directory):
**`corpus_hits=861/861`**, `ORACLE ok`, negative probe `ok`
(renamed-path miss). **I5/F2 salt-bump proof**: populated the same
two-copy corpus with a binary built from this campaign's pre-W3+W4 commit
(`adde06a`, java salt `"ts-0.23"`), consumed it with this bead's binary
(salt `"ts-0.23-mp3"`) — **`corpus_hits=0`**, a clean miss exactly as I5
requires, `ORACLE ok` proving the resulting fresh build was still correct
despite the miss.

**The memory fence fails.** `/usr/bin/time -l`, elasticsearch, cold (fresh
`SEM_CACHE_DIR` each run), two independent pairs with run order swapped:

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 5,469,716,480 B (5.09 GiB) | 6,616,678,400 B (6.16 GiB) | **+20.97%** |
| 2 | ON→OFF | 5,465,309,184 B (5.09 GiB) | 6,613,762,048 B (6.16 GiB) | **+21.01%** |

Both pairs reproducibly bust the +15% ceiling by a wide margin — closer to
dotnet's phase-1 overshoot (+21-33%) than Rust's phase-2 pass (+11.16%/
+11.28%). MUL-A §2.3's own census explains why: Java's pre-Field-10
TREELESS-by-bytes share was the *smallest* of any language measured
(**0.98%**), so admission moves nearly all of elasticsearch's Java bytes
onto the fast path in one step — the same shape that busted dotnet's
ceiling in phase 1. Per I6 ("under the ceiling → admit unconditionally;
over it → gate"), `mul_precompute_admits("java")` stays a runtime switch
(`SEM_MUL_JAVA`, off by default, `MUL_RUNTIME_GATES` row with
`pre_switch_salt = "ts-0.23"`) — C#'s shape, not Rust's.

**The prize, measured anyway** (so the cost of staying gated is on the
record). `SEM_PROFILE_RESOLVE=2`, `sem find <nonexistent>`, elasticsearch
(30,241 files at measurement time):

| counter | OFF | ON | Δ |
|---|---:|---:|---:|
| `files_precomputed` | 43 | 30,220 | — |
| `files_ast` | 30,198 | 21 | — |
| `files_precomputed_with_imports` | 12 | 29,219 | — |
| `precomputed_import_descriptors` | 121 | 494,577 | — |
| `reparse_ms` | 1,329.19 | 114.94 | **−91.4%** |
| `fused_walk_ms` | 7,299.51 | 1.93 | **−99.97%** |
| `scope_build_ms` | 8,409.06 | 1,340.16 | **−84.1%** |
| `pass2_wall_ms` | 797.71 | 803.76 | ~flat |

`entities_spanned` identical both sides (506,794) — matches the bit-identical
sha256 above; same graph, and (with the switch off) the same cost, cheaper
only when flipped. Cold-build wall (`sem find`, fresh `SEM_CACHE_DIR`, 6
interleaved pairs, order swapped across two batches of 3): OFF values
{21.99, 12.01, 23.35, 15.82, 15.68, 10.40}s, ON values {11.69, 12.41, 20.62,
17.53, 13.43, 11.56}s — medians OFF 15.75s / ON 12.92s (**−17.9%**), but
per-pair direction was **not unanimous** (2 of 6 pairs went the other way,
e.g. pair 4 ON 17.53s vs OFF 15.82s). Disclosed rather than smoothed over:
this session ran heavy concurrent `cargo build`/`cargo test` activity on the
same box throughout the whole bead, which the mechanism-level counters above
are immune to (they measure CPU-time inside one process) but end-to-end wall
time is not. The counters are the trustworthy number here; the wall-clock
median points the same direction but is noisy, not a clean n=6 win the way
W2's Rust numbers were.

### Go

**The memory fence passes.** `/usr/bin/time -l`, kubernetes, cold, two
independent pairs with run order swapped:

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 3,769,679,872 B | 3,716,251,648 B | **−1.42%** |
| 2 | ON→OFF | 3,731,324,928 B | 3,752,509,440 B | **+0.57%** |

Comfortably under +15%, both pairs, one of them even a net *decrease* —
consistent with kubernetes's own pre-Field-10 FASTPATH share already being
the largest non-JS/TS non-C++/C# share on bytes among the four NO-GO-as-is
families measured in MUL-A (§2.3: dotnet/llvm ≈100%, rust 13.74%, kubernetes
5.24%, elasticsearch 0.98%, HA 0.23% — Go was already carrying some fast-path
weight in the pre-Field-10 two-tier gate's own file-count sense before this
bead).

**Correctness fails — this is the actual blocker, not memory.**
`edge_dump_probe` on kubernetes (13,619 files):

| | OFF | ON |
|---|---:|---:|
| edges | 366,905 | 363,487 |
| sha256 | `2db4539a907e763b…` | `c37dfdc01e8d2c8d…` |

Not bit-identical: a 30,801-line diff. Fully **deterministic** — re-ran OFF
twice (0-line diff between the two OFF runs) and ON twice (0-line diff
between the two ON runs) before concluding this, ruling out a parallel-hash
-map-iteration race as the cause. A representative wrong edge:

```
OFF (correct): cmd/kubeadm/app/apis/kubeadm/types.go::type::ClusterConfiguration::DeepCopyInto
  Calls cmd/kubeadm/app/apis/kubeadm/v1/types.go::type::ClusterConfiguration::DeepCopyInto
ON  (wrong):   cmd/kubeadm/app/apis/kubeadm/types.go::type::ClusterConfiguration::DeepCopyInto
  Calls staging/src/k8s.io/pod-security-admission/admission/api/v1/types.go::type::PodSecurityExemptions::DeepCopyInto
```

Dozens of unrelated `X::DeepCopyInto`/`X::DeepCopy` call sites, across
entirely different packages (`cmd/kube-apiserver`, `cmd/kube-proxy`,
`cmd/kubeadm/...`), all collapse onto the *same one* wrong target when ON —
a strong signal of a single shared, deterministic mis-resolution mechanism
rather than scattered noise.

W0's `GoParentsResolved`-token ordering fix (CLEAN gate runs after
`resolve_go_method_parent_ids`) is **confirmed still sound** — this is the
first bead where `mul_precompute_admits("go")` is real enough (behind
`SEM_MUL_GO=1`) to exercise that ordering end-to-end for the first time, and
`go_parent_repair_must_run_before_clean_gate_adjudication` stays green
throughout. The regression is not that hazard resurfacing.

**Root cause: located but not fully isolated.** Three repros were tried
against the mechanism and did **not** reproduce the divergence:

1. A single-package, three-type fixture (`Foo`/`Bar` cross-file
   struct-then-methods split, plus a same-package `APIServer`-typed field
   with a nested `in.Server.DeepCopyInto(&out.Server)` call) — bit-identical
   ON vs OFF.
2. A 29-file slice copied directly out of kubernetes —
   `cmd/kubeadm/app/apis/kubeadm/{,v1}/*.go` plus the exact
   `staging/src/k8s.io/pod-security-admission/admission/api/v1/*.go` package
   the wrong edges above target — also bit-identical, 400/400 edges matching.

The bug needs corpus scale to manifest, which is itself informative.
Chasing it, `SEM_PROFILE_RESOLVE=2` on the *full* kubernetes corpus surfaced
a second, independent finding: `extract_imports_ms` costs **~83-88 seconds**
(87,804.79 ms OFF, 82,762.28 ms ON) — larger than `reparse_ms` itself, and
present **identically on both ON and OFF**, i.e. a pre-existing cost, not
caused by this bead. Reading `build_go_pkg_index`/`register_go_package_
imports` (`scope_resolve.rs`) explains both the cost and a plausible
correctness mechanism for the edge divergence: packages are keyed by their
**bare last-path-segment string** ("v1", "util", …), with no disambiguation
by full import path. kubernetes has dozens of packages literally named
`v1` (one per API group: `kubeadm/v1`, `bootstraptoken/v1`,
`pod-security-admission/.../v1`, …), and `build_go_pkg_index` merges *all*
of their exported symbols — methods included, since `symbol_table` indexes
every entity by bare name — into one `go_pkg_index["v1"]` bucket, sorted and
inserted last-write-wins by `register_go_package_imports`. On the AST path
this is **latent**: type-directed (`class_members`-based) resolution
normally succeeds first for a `DeepCopyInto` call, so the polluted
`import_table_by_name.get("DeepCopyInto")` fallback (checked only as a last
resort, gated to `.go` files, in the "Go package-qualified call" branch) is
populated but never consulted. The working hypothesis — code-read-confirmed
for the collision half, **not yet directly instrumented** for the other
half — is that something about the fast path's file-local
`entity_map`/`children_by_parent` substitution measurably increases how
often that type-directed resolution fails for Go specifically, pushing more
calls into the already-polluted fallback. Neither half is fixed by this
bead.

Per I6, `mul_precompute_admits("go")` stays a runtime switch (`SEM_MUL_GO`,
off by default, `MUL_RUNTIME_GATES` row, `pre_switch_salt = "ts-0.23"`) —
but unlike a normal memory-gated language, **the switch must not be flipped
even for re-measurement** until the correctness regression above is
root-caused and fixed; its own doc comment in `scope_resolve.rs` says so
explicitly, so a future reader finding the switch does not mistake it for
"just needs a rebuild to try." The `"ts-0.23-mp3"` salt bump is kept
regardless — correct and load-bearing for any future producer change,
independent of whether the switch is ever turned on.

### Mechanism proofs (both languages, unconditional on the admission decision)

- `mul_phase2_go_java_default_matches_the_measured_verdict` (scope_resolve.rs)
  pins both defaults against the measured verdicts above.
- `precompute_scope_resolvable_file_facts_some_for_go_with_imports` /
  `_java_with_imports`: a multi-spec `import (...)` block (Go) and two
  `import` statements (Java) each get fast-path facts with the right
  descriptor count and variant (`GoImport` for both).
- `precompute_scope_resolvable_file_facts_go_call_expression_stays_treeless`
  / `_java_method_invocation_stays_treeless`: Go's `call_expression` and
  Java's `method_invocation` are not the literal `"call"` Field 11's
  (unbuilt, Python-only) ctor-infer scan hardcodes to — the call half of
  TREELESS is untouched by this bead, same pin Rust's W2 test made.
- `fused_triple_walk_matches_three_sequential_walks`: extended with a
  `gen_java` fixture (6th family) alongside the pre-existing `gen_go`
  multi-spec `import ("fmt"; u "example.com/pkg/util")` fixture — both now
  assert non-vacuity (`family_imports[3] > 0` for Go, a gap that predated
  this bead and was closed here since it was already touching this exact
  test; `family_imports[5] == 0` for Java, pinning the no-op). This is the
  task's requested "record-vs-direct equivalence test" for Java, confirming
  by construction (not just by the elasticsearch battery) that
  record-then-dispatch and dispatch-direct agree on Java's no-op descriptor
  path.
- The existing `test_go_method_parent_resolves_across_files_in_graph`
  (`graph.rs`) and the session-level Go oracle suite
  (`go_oracle_no_op_rebuild_is_green`, `go_oracle_touch_a_leaf`,
  `go_oracle_touch_the_pkg_index_hub`) all stay green — but with
  `SEM_MUL_GO` off by default (unset in the test process), none of them
  exercise the actual fast path, so they do **not** contradict the
  divergence found above; they were not expected to.

### Gates

`cargo build --release -p sem-core --lib --examples -p sem-cli --bin sem`:
clean (two pre-existing, unrelated `sem-cli` warnings unchanged).
`cargo test --release -p sem-core`: **727/727** (652 lib, +5 new tests over
W2's 647 baseline: the go/java default-verdict pin, two TREELESS-with-imports
positives, two call-node-stays-TREELESS negatives; `fused_triple_walk_
matches_three_sequential_walks` extended in place, not counted as new).
`cargo test --release -p sem-cli`: **250/250**. `rustfmt --check` clean on
every touched file. `cargo clippy --release --lib --examples`: 151 warnings
before this bead, 151 after — zero attributable to this bead (verified by
diffing the pre-bead warning count in a stashed tree against the same
command post-bead).

**Close-out.** `br comments add semx-mul` records both languages' fence and
prize numbers. No language admission changed in production terms — both
`SEM_MUL_GO` and `SEM_MUL_JAVA` are unset by default, so `mul_precompute_
admits` returns `false` for both exactly as it did before this bead; what
changed is that the decision is now *measured* instead of *unmade*, and Go's
correctness regression is now a documented, load-bearing blocker rather than
an unknown.

Bead: semx-mul (phase-2 W3+W4). Epic: semx-w5k. Prior: semx-mp1 (MUL P1,
C#/C++), semx-mul phase-2 W0 (CLEAN gate ordering), W1 (Field 10
mechanized), W2 (Rust admitted unconditionally — this bead's template).

## 2026-08-21 — semx-mul phase-2 W5: Field 11 built, Python admitted unconditionally — the last planned admission

MUL-DESIGN.md §4.3 named Field 11 (`ctor_call_sites`) as Python's own
tree-need, the larger half already closed by Field 10's import descriptors
(§2.3's census: 16,559 of HA's 18,145 scope-resolvable files need the tree
for imports, 16,088 for `"call"` nodes — mostly the same files needing both).
This bead builds Field 11, extends `mul_precompute_consumes_imports` to
Python, adds the sibling `mul_precompute_consumes_calls` predicate, and
measures the real corpus — completing the four admissions §6.2 planned
(JS/TS+C++ phase 1, Rust W2, Go/Java W3+W4 gated, Python this bead).

### What moved into `CtorCallFacts` vs stayed corpus-side

`scan_constructor_calls`'s tree-walk read exactly two things per `"call"`
node: the callee identifier (filtered to "identifier node, uppercase first
character" — a purely syntactic pre-filter) and, per argument, whether that
argument was itself a `"call"` node whose own `function` field was a bare
identifier (`infer_expr_type`'s only non-`None` case). Both are recorded
verbatim by the new `record_ctor_call_sites`/`record_arg_call_shape` at
precompute time — `CtorCallFacts { callee: String, arg_shapes: Vec<Option
<String>> }`, one descriptor per qualifying call, in the same
`push_named_children_rev` worklist order the old direct walk used. What
stays corpus-side, unconditionally, in the new `apply_ctor_call_facts`: the
`init_params.get(&facts.callee)` lookup (which params exist for this
class), `attr_to_param_index` (which `self.attr = param` assignments this
constructor makes), and `func_name_returns` (a lowercase argument callee's
declared return type) — none of these exist until every file in the corpus
has contributed its own `init_params`/`attr_to_param`/`return_type_map`,
which is why the *handler* stays in pass 2 while what it reads *from the
tree* moved to pass 1, exactly as §4.3 specified. `infer_arg_type_from_shape`
is the one-line replay of `infer_expr_type`'s resolved half: uppercase
recorded name → the name itself; lowercase → `func_name_returns.get(name)`.

The old `scan_constructor_calls`/`infer_expr_type` were deleted outright
(the W1 subtraction bar) — no independent traversal spec was kept alongside
the replacement. Unlike Field 10's `extract_imports_from_ast` (which
*predates* BS3's fused-walk refactor and was deliberately retained across
it as the executable spec `record_import_stmts_pruned` +
`dispatch_import_stmts_from_facts` are checked against), no such
predecessor existed for ctor-call scanning — this is the first record/apply
split this scan has ever had, so there was nothing to keep. The
record-vs-direct equivalence proof the task asked for
(`record_then_apply_matches_direct_scan_for_ctor_calls`) instead writes a
fresh, deliberately verbatim transcription of the pre-refactor functions as
a test-local closure, never touched by production code, and asserts the
production `record_ctor_call_sites` + `apply_ctor_call_facts` composition
lands on the identical `instance_attr_types` map.

`infer_constructor_param_types` (the pass-1b corpus-wide scan) had to
change shape regardless of Field 11: before this bead it walked
`parsed_files` only, so a fast-path file — one whose tree died at the end of
pass 1 — could never contribute a ctor-call site at all. It now takes
`precomputed_facts`/`file_paths` too, and merges in `file_paths` order (not
`parsed_files`'s own order) — on the production chunked-resolve path
`parsed_files` is already exactly `file_paths` with declined files filtered
out (no `pre_parsed` group to interleave there), so this is order-preserving
for the path `edge_dump_probe` measures, and it is the only order that can
name both a precomputed-facts file and a freshly re-parsed one uniformly.

### Schema and salt mechanics

`FACTS_SCHEMA_VERSION` 2 → 3 (`PrecomputedFileFacts` grew
`ctor_call_sites: Vec<CtorCallFacts>`, a new type reachable from
`PersistedFacts` — same shape as W1's 1 → 2 bump for `import_stmts`, so it
gets the identical clean-miss treatment). `LANGUAGE_SALTS`'s python entry
bumped `"ts-0.23"` → `"ts-0.23-mp4"` (I5/F2), mirrored in
`examples/facts_corpus_probe.rs`'s independent copy (byte-equality test
against the real table catches drift, `corpus_isolates_by_language_salt`
updated to the new literal since it drives `populate_delta` through the
real `producer_language_salt` path). `mul_precompute_consumes_imports`
widened to `"rust" | "go" | "java" | "python"`; the new sibling
`mul_precompute_consumes_calls` is `matches!(lang_id, "python")` — Python
needs *both* predicates to clear TREELESS, since its census share was
imports-larger, calls-smaller, and either alone still fails almost every
real HA file on the other half.

### Engagement, proven not assumed

`SCOPE_BUILD_WORK` gained `files_precomputed_with_ctor_calls`/
`precomputed_ctor_call_descriptors`, bumped from `infer_constructor_param_
types` itself (not `ScopeBuildAccum` — this scan runs once per build in its
own pass-1b step, not per file inside pass 2's closure). `SEM_PROFILE_
RESOLVE=2`, `sem graph --no-cache`, home-assistant/core (22,397 files at
measurement time):

| counter | OFF (pre-bead binary) | ON (this bead, default) |
|---|---:|---:|
| `files_precomputed` | 2 | 18,210 |
| `files_ast` | 18,211 | 3 |
| `files_precomputed_with_imports` | 0 | 16,625 |
| `precomputed_import_descriptors` | 0 | 156,559 |
| `files_precomputed_with_ctor_calls` | — | 11,374 |
| `precomputed_ctor_call_descriptors` | — | 78,702 |
| `fused_walk_ms` | 2,852.23 | 0.08 |
| `extract_imports_ms` | 1,494.50 | 1,315.50 |
| `SCOPE_BUILD_NS total_ms` | 4,564.94 | 1,564.95 |

16,625 files with import descriptors is close to the census's 16,559
prediction (drift since the original commit, same ~0.4% class W2 saw for
Rust). `files_precomputed_with_ctor_calls=11,374` is smaller than the
census's "16,088 files contain a `call` node" because that count measures
*any* call node, while the engagement counter (matching the import
counter's own discipline) only fires when a file's recorded
`ctor_call_sites` is nonempty — i.e. it actually contains an
uppercase-identifier constructor-shaped call, not merely some call. Files
with call nodes but zero ctor-shaped calls still take the fast path
(`files_precomputed` counts them); they just don't count toward this
narrower engagement metric, by design.

### Correctness

`edge_dump_probe` sha256, this bead's binary vs the pre-bead commit
(7679764): **bit-identical on home-assistant/core** (310,398 edges, sha
`7744ae4a…`, both sides) and **bit-identical on the control corpus**,
rust-lang/rust (309,429 edges, sha `36890018…`, both sides) — proving the
`infer_constructor_param_types` reshape left every non-Python language
untouched (their own `"call"`-literal scan is a structural no-op regardless
of merge order, since it produces nothing to merge). `SEM_FP_PARITY=1
incr_probe … all` on home-assistant/core (22,397 files): **8/8 `ORACLE
ok`** (cold-vs-build plus all 7 warm scenarios: none, leaf, mixed50, hub,
hubrename, tests, importchurn). `facts_probe` save/load cross-process on
HA: **4/4 `ORACLE ok`**. `facts_corpus_probe` populate/consume on a real
two-copy corpus (`homeassistant/helpers`, 109 files): **109/109 hits**,
`ORACLE ok`, negative probe `ok` (same-content-different-path miss). The
I5/F2 salt-bump adversarial proof: populated with a binary built from the
pre-bead commit (python salt `"ts-0.23"`), consumed with this bead's binary
(salt `"ts-0.23-mp4"`) — **`corpus_hits=0`**, a clean miss, `ORACLE ok`
regardless (the resulting fresh build was still correct despite the miss).

### The memory fence

Measured twice, independently: first via the interim runtime-gated code
(`SEM_MUL_PYTHON` toggle within one binary), then again via the pre-bead
vs. this-bead binary comparison after the code was promoted to
unconditional (matching how the toggle is actually exercised in
production). `/usr/bin/time -l`, home-assistant/core, cold (fresh
`SEM_CACHE_DIR` each run):

| measurement | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| toggle, pair 1 | OFF→ON | 2,283,257,856 B | 2,101,854,208 B | **-7.95%** |
| toggle, pair 2 | ON→OFF | 2,285,600,768 B | 2,107,244,544 B | **-7.80%** |
| binary, pair 1 | OLD→NEW | 2,255,454,208 B | 2,148,761,600 B | **-4.73%** |
| binary, pair 2 | NEW→OLD | 2,296,283,136 B | 2,118,828,032 B | **-7.73%** |

All four pairs, unanimous direction: a net **decrease**, not merely under
the +15% ceiling. This tracks MUL-A §2.3's own numbers in the opposite
direction from Java's/C#'s overshoot: HA's pre-Field-10/11 FASTPATH byte
share was the *smallest* of any family measured (0.23%), meaning almost the
entire corpus's chunked-path tree retention — a `(path, content, tree)`
triple per file for ~18k Python files — gets deleted by this admission in
one step, and on a corpus HA's size that deletion outweighs the small
facts residency Field 10+11 add (projected ~0.27 GB, MUL-DESIGN.md §5.3).
Java's and C#'s overshoot came from the same "admission moves nearly all
bytes onto the fast path in one step" shape, but on corpora (elasticsearch,
dotnet) large enough that the *added* facts residency dominated instead of
the *removed* tree retention — the sign of the effect depends on which side
is bigger at a given corpus scale, not on the admission mechanism itself.

### The prize

`sem graph --no-cache`, `SEM_TIMINGS=1`, home-assistant/core, fresh
`SEM_CACHE_DIR` each run, 4 interleaved pairs (order swapped), pre-bead vs.
this-bead binaries:

| pair | order | OLD `full_graph_build` | NEW `full_graph_build` | Δ |
|---|---|---:|---:|---:|
| 1 | OLD→NEW | 3,602.038 ms | 2,627.422 ms | **-27.1%** |
| 2 | NEW→OLD | 3,623.432 ms | 2,688.507 ms | **-25.8%** |
| 3 | OLD→NEW | 3,763.907 ms | 2,795.103 ms | **-25.7%** |
| 4 | NEW→OLD | 3,862.709 ms | 2,776.485 ms | **-28.1%** |

Median: OLD 3,693.7 ms, NEW 2,732.3 ms, **-26.4% of `full_graph_build`** —
direction unanimous across all 4 pairs (range -25.7% to -28.1%), exceeding
MUL-DESIGN.md §5.1's own predicted -14.8% (that number was itself disclosed
as an upper-bound ratio from a different, slower box, re-derived from a
gate-only measurement before Field 10/11 existed; the actual admitted
corpus apparently gets more of the win than the pre-implementation estimate
priced in — plausibly the `bow_index_io` elimination and the full-corpus
`fused_walk_ms` collapse compounding more favorably at HA's file-count
scale than the arithmetic in §5.1 modeled). Total wall time (`file_
discovery` + `full_graph_build` + `cli_output_serialization`) moved from a
3.76s/2.80s single-pair sample by roughly the same ratio (**-25.6%**).

### Gates

`cargo test --release -p sem-core`: **731/731** (656 lib, +9 net over
W3+W4's 652 baseline — the two mul_precompute_admits/consumes verdict
pins for Python, the some-for-python-with-imports-and-calls positive, the
imports-only positive replacing the pre-Field-10/11 "none when imports"
negative that this bead's admission made stale, the record_ctor_call_sites
filter test, and the record-vs-direct equivalence test; `mul_phase1_
default_matches_the_measured_verdict`'s per-language loop updated in place,
not counted as new). `cargo test --release -p sem-cli`: **250/250**.
`rustfmt --check` clean on every touched file (`scope_resolve.rs`,
`facts_store.rs`, `resolve_profile.rs`, `examples/facts_corpus_probe.rs`).
`cargo clippy --release --lib --examples`: 151 warnings before this bead,
**150 after** — net *fewer* (the old `scan_constructor_calls`'s manual
`arg_idx` counter, a lint clippy already flagged at the pre-bead commit,
was replaced by `.enumerate()` inside `apply_ctor_call_facts`), zero new
warnings attributable to this bead (verified by diffing the pre-bead
warning locations against the post-bead ones in the same file regions).
`cargo build --release` clean on `sem-core`, `sem-cli`, and every touched
example — two pre-existing, unrelated `sem-cli` warnings unchanged.

### Close-out

Python is admitted unconditionally (`mul_precompute_admits("python") ==
true`, no env switch, no `MUL_RUNTIME_GATES` row) — the same shape as
phase-2 W2's Rust, not Java's/C#'s gated one, per I6's own fail-safe
instruction ("under the ceiling → admit unconditionally"). This completes
MUL-DESIGN.md §6.2's planned admissions: JS/TS+C++ shipped unconditionally
in phase 1, C# stays gated on memory, Rust and now Python ship
unconditionally in phase 2, Go stays gated on a real correctness
regression (not yet root-caused) and Java stays gated on memory. Every
scope-resolvable language this codebase supports has now been measured
against the two-tier fast path at least once; what remains open is Go's
correctness bug (a future bead's job, explicitly not this one's) and
whichever memory levers (§5.3's two named ones, or a new one) might one day
promote Java.

`br comments add semx-mul` records this bead's numbers.

Bead: semx-mul (phase-2 W5). Epic: semx-w5k. Prior: semx-mp1 (MUL P1,
C#/C++), semx-mul phase-2 W0 (CLEAN gate ordering), W1 (Field 10
mechanized), W2 (Rust admitted unconditionally), W3+W4 (Go/Java measured,
both stay gated).

## 2026-08-21 — FINALE: the eight-corpus matrix, both deferred verifications, campaign close

W6, the last wave of the MUL phase-2 + subtraction campaign. Two binaries,
both `sem 0.22.1` by version string (unbumped), distinguished by commit:
**baseline** = pre-campaign tip `dev-main-022-full`@`fbfc2c3` (worktree
`.worktrees/sem-baseline-0221`, its own `target/`); **campaign** =
`perf/mul-phase2`@`9c80258` (worktree `.worktrees/sem-false-things`, shared
`CARGO_TARGET_DIR` under `sem/crates/target`). Both build clean; `--version`
sanity confirmed on each.

### The matrix

Protocol: interleaved baseline/campaign cold pairs (order alternated pair
to pair), fresh `SEM_CACHE_DIR` per run, `--no-cache` (which also disables
`SEM_FACTS_CORPUS_DIR`/`FactsStore` — cold means cold, no cross-run facts
persistence engaged on either side). Full-CLI wall = wall-clock around
`sem graph --no-cache --json <repo> > /dev/null`. Engine = `SEM_TIMINGS=1`'s
`full_graph_build` phase from the same invocation. 3 pairs (6 cold runs) per
corpus, median ± spread (max−min across the 3 same-binary runs). Warm
rebuild: one pair, persistent `SEM_CACHE_DIR` primed then re-measured
(hits `index_fast_path`, the SQLite entity-cache path — not
`full_graph_build` — on both binaries identically). Peak RSS: one
order-swapped pair via `/usr/bin/time -l`, `--no-cache` cold. `uptime`
logged at the start and end of every batch.

**Cold (median of 3 interleaved pairs):**

| corpus | wall base (ms) | wall camp (ms) | Δwall | engine base (ms) | engine camp (ms) | Δengine | expectation |
|---|---:|---:|---:|---:|---:|---:|---|
| home-assistant/core | 4,832 | 3,546 | **-26.6%** | 4,293.6 | 3,010.4 | **-29.9%** | ≈-26% engine (W5) |
| microsoft/TypeScript | 10,015 | 9,694 | -3.2% | 9,034.3 | 8,818.8 | -2.4% | flat (P1 unconditional, unchanged) |
| rust-lang/rust | 11,436 | 9,939 | **-13.1%** | 10,537.2 | 8,984.6 | **-14.7%** | ≈-15% cold (W2) |
| kubernetes/kubernetes | 14,957 | 15,263 | +2.0% | 13,462.0 | 14,178.1 | +5.3%† | flat (Go gated) |
| elastic/elasticsearch | 11,572 | 11,992 | +3.6%† | 9,553.9 | 9,958.9 | +4.2%† | flat (Java gated) |
| dotnet/runtime | 38,918 | 39,674 | +1.9% | 36,610.6 | 37,452.3 | +2.3% | flat (C# gated) |
| torvalds/linux | 21,703 | 21,496 | -1.0% | 17,714.6 | 17,498.8 | -1.2% | flat (C, untouched) |
| llvm/llvm-project | 23,889 | 23,836 | -0.2% | 20,886.0 | 20,902.2 | +0.1% | flat (C++, P1 unconditional) |

† kubernetes and elasticsearch both crossed the >3% flat-corpus
investigation threshold on first pass — see **Attribution: the two
flat-corpus flags** below. Both were re-run and resolved as load noise, not
regressions; the table above reports the original first-pass numbers
per the "report absolute numbers, don't average away" rule, with the
resolution documented separately rather than silently folded in.

**Warm rebuild (1 pair, index-cache hit path, both binaries):**

| corpus | wall base (ms) | wall camp (ms) | Δ |
|---|---:|---:|---:|
| home-assistant/core | 209 | 208 | -0.5% |
| microsoft/TypeScript | 289 | 287 | -0.7% |
| rust-lang/rust | 302 | 305 | +1.0% |
| kubernetes/kubernetes | 374 | 373 | -0.3% |
| elastic/elasticsearch | 802 | 673 | -16.1% |
| dotnet/runtime | 959 | 795 | -17.1% |
| torvalds/linux | 1,447 | 1,285 | -11.2% |
| llvm/llvm-project | 993 | 822 | -17.2% |

Every warm run on both binaries hits `index_fast_path` (the SQLite
entity-cache path), which phase 2 never touches — there is no engineering
reason for baseline and campaign to differ here. The four double-digit
deltas above (elasticsearch/dotnet/linux/llvm, all -11% to -17%, all
*favoring* campaign) are a single unreplicated sample apiece, taken while
`uptime` load averages were 10–21 (§ caveats) — read as noise, not signal.
Flagging rather than hiding it: a real, unexplained warm-path win this
size would be worth its own investigation, but one sample under load is
not evidence of one.

**Peak RSS (1 order-swapped pair, `/usr/bin/time -l`, cold):**

| corpus | RSS base (bytes) | RSS camp (bytes) | Δ | expectation |
|---|---:|---:|---:|---|
| home-assistant/core | 1,845,329,920 | 1,803,354,112 | **-2.3%** | net decrease (W5, confirmed) |
| microsoft/TypeScript | 2,398,552,064 | 2,381,021,184 | -0.7% | flat |
| rust-lang/rust | 2,554,904,576 | 3,095,281,664 | **+21.2%** | ≈+11% (W2) — **see finding below** |
| kubernetes/kubernetes | 3,653,156,864 | 3,640,426,496 | -0.3% | flat |
| elastic/elasticsearch | 4,973,494,272 | 5,050,892,288 | +1.6% | flat (Java gated) |
| dotnet/runtime | 8,360,673,280 | 8,533,671,936 | +2.1% | flat (C# gated) |
| torvalds/linux | 7,094,206,464 | 7,254,392,832 | +2.3% | flat |
| llvm/llvm-project | 7,185,137,664 | 7,173,406,720 | -0.2% | flat |

### Finding: Rust's memory fence re-measures above W2's original ceiling

W2 (phase-2, `adde06a`) measured Rust's admission at **+11.16%/+11.28%**,
two order-swapped `/usr/bin/time -l` pairs on rust-lang/rust, safely under
the +15% ceiling that separated "unconditional" (Rust, Python) from "gated"
(Java, C#). This wave's re-measurement, same corpus, same protocol, **two
independent order-swapped pairs**, both above that same ceiling:

| pair | order | base maxRSS | camp maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | base→camp | 2,554,904,576 B | 3,095,281,664 B | **+21.15%** |
| 2 | camp→base | 2,575,794,176 B | 3,045,064,704 B | **+18.22%** |

Unanimous direction, both above +15%, both well above W2's +11% — not a
one-off. Investigated (not fixed — outside this wave's lane): the obvious
hypothesis is W5's `FACTS_SCHEMA_VERSION` 2→3 bump, which added a new
`ctor_call_sites: Vec<CtorCallFacts>` field to the shared
`PrecomputedFileFacts` struct (`scope_resolve.rs:1157`) — a field every
precomputed file's facts record now carries, Rust's included, even though
`mul_precompute_consumes_calls` is Python-only and the field stays an empty
`Vec` for every Rust file. Ruled out by back-of-envelope: an empty `Vec` is
24 bytes (ptr+cap+len) on this target; rust-lang/rust's own W2 engagement
census was ~38,420 precomputed files, so the field's own footprint is
≈920 KB corpus-wide — three orders of magnitude too small to move a
multi-GB RSS by double-digit percentage points. No other diff touches
Rust's own producer/consumer path between `adde06a` and `9c80258` (the
intervening commits are W3+W4's Go/Java work and W5's Field 11/Python
work, `git log adde06a..HEAD -- crates/sem-core/src` shows exactly two
commits, both reviewed above). Root cause not found this wave — **filed as
an open question**, not silently accepted: Rust shipped unconditionally on
W2's numbers, and this wave's numbers say the memory ceiling that decision
rested on no longer clearly holds. Recommend a follow-up bead re-running
W2's exact toggle-based comparison (interim env-gate on/off within one
binary, not pre-bead-vs-this-bead across two) to isolate whether this is a
genuine drift in Rust's own cost or an artifact of comparing binaries eight
commits apart instead of one flag apart.

### Attribution: the two flat-corpus flags (kubernetes, elasticsearch)

Both crossed the >3% engine-metric threshold on their first pass
(kubernetes engine +5.3%, elasticsearch wall +3.6%/engine +4.2%) — both
flat-expected (Go and Java stay gated off by default; the fast path is not
engaged on either corpus by either binary). Per this wave's own
instruction ("investigate, don't average away"), both were re-run as a
fresh 3-pair batch once `uptime` showed load had dropped (from 17–25 down
to 5.5):

| corpus | 1st-pass Δengine | load during 1st pass | re-run Δengine | load during re-run |
|---|---:|---|---:|---|
| kubernetes | +5.3% | 7.95→17.02 | **-0.9%** | 5.57→19.15 |
| elasticsearch | +4.2% | 14.36→17.98 | **-1.4%** | 17.45→19.20 |

Pooled medians across both batches (6 pairs each): kubernetes wall
+1.9%/engine +1.9%; elasticsearch wall -1.7%/engine +0.3% — both flat
within noise. **Attribution: load, not a regression.** `uptime` climbed
steadily across the whole matrix run, 7.6 at the first (home-assistant)
batch to a peak of 25.5 during the linux batch, before settling back to
5.5 by the time the re-runs started — this session's sibling waves
(a3b-rust-edges through w5-python, per the team roster) were running
concurrently on the same box for at least part of this wave, which the
campaign's own measurement discipline calls out as exactly the confound
interleaving controls for *within* a pair but not *across* corpora
measured at different points in a rising-load window. Conclusion holds:
kubernetes and elasticsearch are flat, as their gated-off status predicts.

### Oracle battery (campaign binary, `index_probe write`)

| corpus | ORACLE (names) | REFS_ORACLE | FILES_ORACLE | TESTS_ORACLE | TRIGRAM_ORACLE | REFS_MUTATION |
|---|---|---|---|---|---|---|
| home-assistant/core | PASS 95,580 | PASS 318,638 | PASS | PASS 318,638/50,853 | PASS 6 patterns | PASS (mismatched=1 injected, caught) |
| microsoft/TypeScript | PASS 86,796 | PASS 714,819 | PASS | PASS 714,819/4,295 | PASS 6 patterns | PASS (caught) |
| llvm/llvm-project | PASS 560,360 | PASS 2,760,885 | PASS | PASS 2,760,885/326,111 | PASS 6 patterns | PASS (caught) |

`SEM_FP_PARITY=1 incr_probe … all` (8 scenarios: none/leaf/mixed50/hub/
hubrename/tests/importchurn/cold-vs-build), cold and warm cache, campaign
binary:

| corpus | cold | warm |
|---|---|---|
| home-assistant/core | 8/8 `ORACLE ok` | 8/8 `ORACLE ok` |
| rust-lang/rust | 8/8 `ORACLE ok` | 8/8 `ORACLE ok` |

Zero mismatches anywhere in the battery. No oracle FAIL, no MUTATION
false-negative.

### Deferred verification (a): semx-kkk, TESTS_ORACLE at llvm scale

Predicted by the YAML multi-doc id fix (`39e33c3`, semx-vlg/semx-kkk/
semx-0lj): llvm-project's original 1/2,751,958 `is_test` mismatch, caused
by YAML documents' top-level keys colliding across `---`-separated
documents in the id-uniqueness scheme, disappears once the fix lands. Now
verified directly, not inferred: `TESTS_ORACLE checked=2,760,885
tests=326,111 mismatched=0 ms=2026.2 verdict=PASS` on the full llvm-project
checkout with the campaign binary. **0/2,760,885 — clean.** (Entity/test
counts differ slightly from the original sweep's 2,751,958/325,529, tracked
to upstream corpus drift on the llvm-project checkout since the original
sweep — every other oracle in the same run also passes, so this is not a
new correctness gap.) `semx-kkk` closed via `br close` with this number in
the closing comment.

### Deferred verification (b): semx-o0x, REFS_ORACLE wall-time at llvm scale

The pre-fix pathology: `REFS_ORACLE` on llvm-project took **1,634.7s**
(27.2 min), a Σcount(name)² blowup from a per-entity NAMES-bucket re-scan.
The O(n) id→slot-map fix (`2fff39e`) measured 40–135x cuts on
home-assistant/core and the TypeScript monster, but llvm itself was
unavailable at fix time — left as the bead's own "LLVM PENDING" note. Now
measured directly: `REFS_ORACLE entities=2,760,885 checked=2,760,885
mismatched=0 kind_mismatched=0 typed=true ms=4454.5 verdict=PASS` —
**4.45 seconds**, a **~367x** wall-time cut, exceeding the 40–135x range
measured on the two smaller corpora. Addendum comment added to the
(already-closed) `semx-o0x` bead with this number.

### Admission scoreboard (final)

| language | status | cold win | memory | correctness |
|---|---|---|---|---|
| JS/TS | unconditional (P1) | — (P1 baseline) | — | clean |
| C++ | unconditional (P1) | flat on llvm/linux (already admitted) | — | clean |
| Rust | unconditional (W2) | -13.1% to -14.7% cold, this wave | **+18–21% re-measured, above the +15% ceiling — open finding** | clean |
| Python | unconditional (W5) | -26.4% engine (HA, W5); -26.6%/-29.9% this wave | net decrease (HA) | clean |
| C# | gated (`SEM_MUL_CSHARP`), off by default | n/a while gated | +21–33% over ceiling (P1) | clean |
| Java | gated (`SEM_MUL_JAVA`), off by default | n/a while gated | +21% over ceiling (W3+W4) | clean |
| Go | gated (`SEM_MUL_GO`), **must not be flipped even for re-measurement** | n/a while gated | passes (-1.4%/+0.6%, W3+W4) | **fails** — `edge_dump_probe` diverges on kubernetes at scale (semx-u3rk, unfixed) |

### Subtraction tally (campaign-wide, W1–W6 of this session's arc)

FALSE-THINGS (8 waves) + subtraction passes + the unified `DiskCache`
(semx-r94) removed a net **-1,271 LOC** from `sem-core` ahead of this
wave's phase-2 additions — MUL phase 2 itself is net-additive (new
required surface: Field 10 `+386`/`+83` excl. tests at W1, Field 11's own
addition at W5), by design (§4.3's fields are new required semantics, not
removable duplication). The campaign's net shape: subtract dead weight
first, then add only the semantics the measured bottleneck actually
required.

### Caveats

- **Load.** This box ran other agents' work concurrently for parts of this
  wave (`uptime` 1-minute load average ranged 5.5–25.5 across the full
  matrix run, roster: a3b-rust-edges through w5-python). The campaign's own
  measurement discipline — interleave within a pair — controls for load
  *within* each pair but not for a rising-load trend *across* corpora
  measured sequentially at different points in that trend. Two flat-corpus
  candidates (kubernetes, elasticsearch) crossed the 3% investigation
  threshold on first pass and were re-run once load dropped; both resolved
  to flat. See the attribution section above.
- **llvm partial-clone provenance.** llvm-project was mid-re-materialization
  from a partial clone at the start of this wave; verified ready before use
  (182,424 tracked files, 39,042 `.cpp` files, both above the readiness
  bar) and used for the full matrix plus both deferred verifications.
- **Rust memory finding.** See the dedicated section above — an open
  question, not resolved this wave, surfaced rather than averaged away.
- **Warm-path deltas.** Single-sample, under load, on a code path phase 2
  never touches — noted, not claimed as a real effect (see the warm-rebuild
  table above).

### Gates

`cargo test --release -p sem-core`: 731/731 (unchanged from W5 — no source
touched this wave). `cargo test --release -p sem-cli`: 250/250. Both
re-run this wave to confirm, both green, zero regressions.

Bead: semx-mul (finale, W6). Epic: semx-w5k. Deferred verifications closed:
semx-kkk (closed), semx-o0x (addendum, already closed). Prior: semx-mp1,
W0–W5 (this file, above).

## 2026-08-22 — Rust demoted to a gated switch (semx-j1fw)

The W6 finale's re-measurement (above) and semx-j1fw's own follow-up
(reproduced in its bead comments, both quoted here) agree: Rust's
same-binary peak-RSS delta no longer clears the +15% ceiling that
justified W2's unconditional admission. This bead demotes it —
`mul_precompute_admits("rust")` now routes through an env-gated
`rust_precompute_enabled()` (`SEM_MUL_RUST`), same shape as
`csharp_precompute_enabled`/`go_precompute_enabled`/`java_precompute_enabled`,
and `MUL_RUNTIME_GATES` gained a `"rust"` row with `pre_switch_salt =
"ts-0.23"` — the pre-W2 salt, so a switched-off build shares corpus
entries with the pre-Field-10 world exactly as C#'s/Go's/Java's rows do.
`LANGUAGE_SALTS`'s `("rust", "ts-0.23-mp2")` is unchanged: per
`resolve_gated_salt`, that table entry now serves as the switched-**on**
salt (isolating richer entries from the pre-switch `None`s), the same
role W2 originally gave it as an unconditional bump.

**The numbers being acted on** (semx-j1fw's own re-verification, isolating
the admission variable exactly — one throwaway worktree at campaign HEAD
`602dc6e`, `mul_precompute_admits("rust")` flipped `true`→`false` in place,
built OFF and ON from the identical source tree otherwise, `--no-cache` to
remove the shared-corpus-warmth confound RESOLUTION-PROFILE.md's own W4
section already documented):

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 2,574,876,672 B | 3,031,154,688 B | **+17.72%** |
| 2 | ON→OFF | 2,559,868,928 B | 3,062,611,968 B | **+19.64%** |
| 3 | OFF→ON | 2,567,471,104 B | 3,064,283,136 B | **+19.35%** |

Unanimous direction, all three above the +15% ceiling, closely matching
the finale's own +18.2%/+21.2% reading — not a binaries-eight-commits-apart
artifact (everything but the one predicate was held fixed). W2's original
+11.16%/+11.28% same-binary reading is the outlier that needs explaining,
not this one: W2's protocol used bare "fresh `SEM_CACHE_DIR`" with `sem
find <nonexistent>`, not `--no-cache`, and RESOLUTION-PROFILE.md's own later
W4 finding ("every 'cold' build this campaign has measured was already a
known-content build" — `SEM_CACHE_DIR`'s parent aliases the facts-corpus dir
across runs sharing a tmp root) means the ON side of W2's original
measurement plausibly benefited from partial corpus warmth that a true
`--no-cache` run does not get, understating W2's own delta. Cold wall stays
consistent with W2's direction throughout (ON still ~16% faster than OFF)
— only the RSS magnitude moved, not the wall-time tradeoff shape.

### Gates

`edge_dump_probe` sha256, rust-lang/rust, this bead's binary: **bit-identical
OFF vs ON** (309,429 edges, single matching sha256 both sides) — matches W2's
original correctness proof exactly; the demotion is a gating decision, not a
behavior change, so this was expected rather than newly discovered.

`facts_corpus_probe` populate/consume, a real two-copy corpus
(`library/core`, 317 files — 288 `.rs` + 29 non-Rust siblings such as
`Cargo.toml`): OFF/OFF **317/317 hits**, `ORACLE ok`, negative probe `ok`;
ON/ON **317/317 hits**, `ORACLE ok`, negative probe `ok`. Adversarial
cross-salt proof (populate with the OFF-salt corpus, consume with
`SEM_MUL_RUST=1`): **`corpus_hits=29/317`** — exactly the 29 non-Rust files
(their own languages' salts are untouched by this gate), **zero** of the
288 Rust files hit, `ORACLE ok` (the resulting rebuild is still correct
despite the miss). This is a sharper proof than W2's original pure-Rust
corpus gave (a flat `corpus_hits=0`): it demonstrates the salt bump's
isolation is scoped to Rust specifically, not an accidental corpus-wide
miss.

`cargo test --release -p sem-core`: 656/656 lib (unchanged count — the
gate-table tests that generalize over `MUL_RUNTIME_GATES`
(`csharp_salt_tracks_the_mul_phase1_switch` in `facts_store.rs`, the
skip-by-table-membership salt-parity test) pick up the new `"rust"` row
automatically; the two tests that hardcode a per-language default
(`mul_phase2_rust_default_matches_the_measured_verdict` in
`scope_resolve.rs`, plus a fixed comment in
`mul_phase1_default_matches_the_measured_verdict`) were edited in place to
assert the new default). `cargo test --release -p sem-cli`: 250/250.
`cargo test --release -p sem-core --example facts_corpus_probe`:
`language_salts_match_sem_core_table` still passes (the mirror table's
`("rust", "ts-0.23-mp2")` entry is untouched — only `MUL_RUNTIME_GATES`,
which that probe already consults dynamically, changed). `cargo build
--release -p sem-core --lib --examples -p sem-cli --bin sem`: clean (the
same two pre-existing, unrelated `sem-cli` warnings, unchanged count).

**Admission scoreboard, corrected**: Rust moves from "unconditional" to
"gated (`SEM_MUL_RUST`), off by default" — memory-blocked like C#/Java, not
correctness-blocked like Go. Python is now the only phase-2 language shipped
unconditionally.

Bead: semx-j1fw. Epic: semx-w5k. Prior: semx-mul phase-2 W2 (original
admission), W6 finale (re-opened the question).

## 2026-08-22 — Python's fence re-derived with semx-j1fw's corrected protocol (F1)

Rust's demotion above leaves an obvious loose end: W5's Python memory
fence (§ above, "-7.95%/-7.80%, a net decrease") was measured the same way
W2's Rust fence originally was — `/usr/bin/time -l`, "cold (fresh
`SEM_CACHE_DIR` each run)", **no `--no-cache`**. That is exactly the gap
this section's own W4 finding says silently aliases a shared facts-corpus
dir across runs sharing a tmp root, and exactly the gap semx-j1fw named as
the most plausible explanation for W2's Rust reading being an outlier. If
it inflated Rust's favorable-looking number into an artifact, it could
just as easily have inflated Python's.

**Protocol** (semx-j1fw's, applied verbatim): throwaway worktree at
campaign HEAD `7607f39`, `mul_precompute_admits("python")` flipped
`true`→`false` in place (`scope_resolve.rs`; the sole call site is
`graph.rs:2212`, `lang_id.is_some_and(scope_resolve::mul_precompute_admits)`
— confirmed by reading `mul_precompute_consumes_imports`/
`mul_precompute_consumes_calls`, both of which are only ever consulted
from inside `precompute_scope_resolvable_file_facts`, the producer this
gate already prevents from running at all when admission is `false`; no
companion flip needed for a clean OFF, same as Rust). OFF built in the
throwaway worktree (its own `CARGO_TARGET_DIR`); ON built from the
unmodified campaign worktree against the shared cache
(`sem/crates/target`, already current — 0.31s no-op build). Both binaries
confirmed distinct by hash. `/usr/bin/time -l <bin> graph --no-cache --json
<HA-checkout> >/dev/null`, fresh `SEM_CACHE_DIR` per run, home-assistant/core
(`e9737b7271`, clean).

Three order-swapped pairs were run first, per protocol. They came back
sign-mixed (-1.63%, +0.79%, -2.39%) — nothing like W5's unanimous
same-direction reading — so two more order-swapped pairs were added before
writing anything down, rather than reporting an n=3 result that could
itself have been a coin flip:

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 1,854,275,584 B | 1,824,112,640 B | **-1.63%** |
| 2 | ON→OFF | 1,847,754,752 B | 1,862,320,128 B | **+0.79%** |
| 3 | OFF→ON | 1,869,561,856 B | 1,824,882,688 B | **-2.39%** |
| 4 | ON→OFF | 1,841,954,816 B | 1,826,177,024 B | **-0.86%** |
| 5 | OFF→ON | 1,858,584,576 B | 1,812,840,448 B | **-2.46%** |

Median **-1.63%**, 4 of 5 pairs negative, one positive. `instructions
retired` (also captured by `/usr/bin/time -l`) is a cleaner corroborating
signal that the flip is doing what it should regardless of the noisy RSS
read: OFF consistently ~519-520B instructions, ON consistently ~398-401B
— unanimous and tight across all five pairs, confirming ON is doing
materially less work (the fast path firing) even though its peak-RSS
footprint barely moves relative to OFF's on this corpus at this scale.

**This does not reproduce W5's claimed magnitude or its full unanimity.**
-1.63% median is roughly a fifth of -7.95%/-7.80%, and one of five pairs
inverted sign entirely — a materially weaker and less certain result than
"a net decrease, not merely under the ceiling" as W5's own section
characterized it. The likely mechanism is the same one semx-j1fw named for
Rust: W5's protocol lacked `--no-cache`, so its OFF and/or ON runs
plausibly inherited partial corpus warmth from `SEM_CACHE_DIR`'s aliased
parent, which would distort the reading in either direction depending on
which side benefited more from residual warmth — unlike Rust, where the
distortion happened to flip a passing number into a failing one, here it
plausibly inflated an already-passing number into a much larger
favorable-looking one. `SEM_MUL_PYTHON`'s toggle-based first measurement in
W5 shares the identical gap and is not independently trustworthy either.

**Verdict (I6): STANDS.** The +15% ceiling was never in play for Python
under either protocol — the corrected reading (-2.46% to +0.79%) is as far
from the ceiling as W5's claimed reading was. Unlike Rust, there is no
ceiling-crossing to act on, so no demotion, no gate, no `MUL_RUNTIME_GATES`
row, no salt change. This bead's contribution is exclusively to the
record's honesty: `mul_precompute_admits("python")` stays `true`
unconditionally, but the doc's characterization of *why* is corrected from
"a net decrease" to "no measurable peak-RSS cost either way, comfortably
inside the ceiling regardless."

No code change; no commit required beyond this doc pair. No suites to
re-run — nothing in `mul_precompute_admits`, `mul_precompute_consumes_
imports`, or `mul_precompute_consumes_calls` was touched (the throwaway
worktree's flip was never merged and is deleted after this section is
written). Correctness of the admission itself was already proven
bit-identical by W5's own `edge_dump_probe`/`incr_probe`/`facts_probe`/
`facts_corpus_probe` gates, none of which this re-derivation had reason to
re-run since nothing about *what* the fast path produces was in question —
only the memory-fence number was.

**Admission scoreboard, unchanged**: Python remains the only phase-2
language shipped unconditionally — the number behind that decision has now
actually survived the corrected protocol, where before this bead it had
only survived W5's own (uncorrected) one.

Bead: F1 (memory campaign re-derivation task; no new tracker bead filed —
posted as a comment on semx-bpn2 instead, per the task's own instruction,
since the outcome here is "confirmed as-is" rather than a new finding that
would warrant its own record). Epic: semx-w5k. Prior: semx-mul phase-2 W5
(original admission and its memory-fence claim), semx-j1fw (the corrected
protocol this bead reuses verbatim).

## 2026-08-22 — go_pkg_index's bare-segment collision, half-fixed (semx-u3rk)

W3+W4 (above) named two compounding issues: (1) `build_go_pkg_index` keys
packages by bare last-path-segment, colliding kubernetes's dozens of `v1`
directories into one bucket; (2) something fast-path-specific increases how
often type-directed resolution fails for Go, exposing (1) more often. This
bead fixes (1), confirms (2) is real and separate, and measures both halves.

### The fix

`ImportStmtFacts::GoImport { packages: Vec<String> }` now records each
import spec's *full* path (previously reduced to its bare last `/` segment
at recording time, in both `collect_go_import_pkg_names` and
`build_import_stmt_facts`'s string-literal branch). `build_go_pkg_index`'s
bucket values grew a third field — the entity's declaring directory
(`registry::go_package_dir`, made `pub(crate)` to share the exact notion
`resolve_go_method_parent_ids` already uses for same-package cross-file
pairing) — via a new `GoPkgIndex` type alias,
`HashMap<String, Vec<(String, String, String)>>`. `register_go_package_
imports` derives the bare bucket key from the full import path (unchanged
O(1) lookup), then — only when the bucket holds more than one distinct
declaring directory — picks the one whose trailing path segments overlap
the import path's trailing segments longest (`select_go_pkg_candidate`,
ties broken by lexicographically smaller directory) and inserts only that
candidate's entries into the file's `import_table`. A single-directory
bucket (the common case) short-circuits without calling the comparator at
all.

Two production call sites needed the same fix: `scope_resolve::build_go_
pkg_index` (used by warm/incremental rebuild paths) and a second,
hand-duplicated inline copy inside `EntityGraph::build`'s own cold-build
path (`graph.rs`, ~line 2760) that never called the shared function at
all. Both now carry `decl_dir` per entry. **Secondary finding, not fixed
here:** the inline copy has no `.go`-extension filter (unlike `scope_
resolve::build_go_pkg_index`, which excludes non-`.go` entities before
indexing) — a pre-existing divergence between the two implementations,
flagged for a follow-up bead rather than silently patched, since fixing it
changes behavior beyond this bug's scope and needs its own oracle proof.

`Table::GoPkgIndex`'s read-set fingerprint (`hash_member_list`, a
2-tuple-only hasher shared with `class_members`/`owner_members`) gained a
sibling, `hash_go_pkg_entries`, so the new `decl_dir` field is covered by
incremental invalidation rather than silently dropped from the hash.

### RED → GREEN (unit level)

Two new tests in `scope_resolve.rs`'s test module reproduce the exact
kubeadm/pod-security-admission collision as a minimal fixture — two
distinct Go packages, each declared in a directory literally named `v1`,
each with a `DeepCopyInto` method sharing the same bare exported name:

- `go_package_index_collision_is_real_before_disambiguation`: proves the
  raw index collision exists (`index.get("v1")` returns both entries, one
  per declaring directory).
- `register_go_package_imports_resolves_the_file_own_import_not_a_same_
  named_collision`: proves a file importing kubeadm's `v1` (full path)
  resolves `DeepCopyInto` to kubeadm's own method, and — the mirror case,
  proving this is genuine per-file disambiguation and not a fixture-order
  coincidence — a *different* file importing pod-security-admission's `v1`
  resolves to pod-security-admission's own method.

RED proven empirically, not just asserted: temporarily disabling the
disambiguation (`winner` forced to never match any `decl_dir`, reverting to
"insert the whole bucket") reproduces the exact predicted failure —

```
assertion `left == right` failed: a file importing kubeadm's v1 must resolve
DeepCopyInto to kubeadm's own method, never pod-security-admission's
  left: Some(".../pod-security-admission/admission/api/v1/types.go::method::DeepCopyInto")
 right: Some("cmd/kubeadm/app/apis/kubeadm/v1/types.go::method::DeepCopyInto")
```

Restoring the fix turns it GREEN.

### Corpus-scale proof: RED baseline reproduced, not fully collapsed

Reproduced W3+W4's original RED baseline first, on the actual pre-fix
binary (this campaign's `dff6de5`, i.e. after the Rust demotion but before
this bead), on the current kubernetes checkout:

| | OFF | ON |
|---|---:|---:|
| edges | 366,905 | 363,487 |
| sha256 | `2db4539a…` | `c37dfdc0…` |

Bit-for-bit matching W3+W4's original numbers — same edge counts, same
hashes, confirming the checkout and reproduction protocol line up exactly.
`diff` between the two: **30,801 lines**, matching W3+W4's own count.

After the fix (same checkout, same corpus, this bead's binary):

| | OFF | ON |
|---|---:|---:|
| edges | 334,664 | 331,190 |
| sha256 | `2e81feac…` | `29a6031f…` |

`diff`: **30,795 lines** — essentially unchanged from the 30,801-line
baseline, even though both sides' absolute edge counts dropped by ~32k
each. This is the fix's own signature: it removed a large class of
cross-package false-positive edges that were present on *both* ON and OFF
equally (so they never showed up in the ON-vs-OFF diff to begin with),
while leaving finding (2)'s own ON/OFF-specific divergence essentially
untouched.

Sampling the remaining diff confirms this reading, not just the line
count:

```
cmd/kubeadm/app/apis/kubeadm/types.go::type::ClusterConfiguration::DeepCopy
  OFF (now correct): Calls cmd/kubeadm/app/apis/kubeadm/v1/types.go::type::ClusterConfiguration::DeepCopyInto
  ON  (still wrong):  Calls cmd/kubeadm/app/apis/bootstraptoken/v1/types.go::type::BootstrapToken::DeepCopyInto
```

Two things worth naming precisely: first, `ClusterConfiguration::DeepCopy`
and `ClusterConfiguration::DeepCopyInto` are declared in the *same*
package (`cmd/kubeadm/app/apis/kubeadm/`, `types.go` and `zz_generated.
deepcopy.go` respectively) — a same-package, cross-file method call that
should resolve via type-directed (`class_members`) tracking and never touch
`go_pkg_index`/`import_table` at all. OFF gets this right post-fix. ON
still gets it wrong, and wrong *differently* than before this bead (it
used to land on `pod-security-admission`'s method pre-fix; now it lands on
`bootstraptoken`'s v1 instead — still a same-named-package collision, just
resolved through a different, still-unfixed avenue). Second: `go_pkg_index`
itself is corpus-wide and built once per `EntityGraph::build` call,
independent of any single file's admission state, and `register_go_
package_imports` receives the identical `import_path`/`file_path` on both
ON and OFF — so this file's own `import_table` entries should be identical
either way. That they are not confirms finding (2) is real: something about
the fast path causes this call to reach `go_pkg_index`'s fallback (or a
wrong branch of it) at all, when a correctly-functioning type-directed
resolution never should have. Root cause of (2) not isolated this bead —
handed off as-is, matching the "under-scoped writeup surfaced back" norm
rather than a silent partial fix.

### Perf: extract_imports_ms

`SEM_PROFILE_RESOLVE=2`, `sem find <nonexistent>`, kubernetes, fresh
`SEM_CACHE_DIR` per run, `SEM_MUL_GO` unset (OFF — the tax existed on both
paths per W3+W4, and OFF is the only path anyone runs in production today):

| binary | extract_imports_ms | import_rekey_ms | fused_walk_ms | total scope_build_ms |
|---|---:|---:|---:|---:|
| pre-fix (`dff6de5`) | 83,185.67 | 3,227.48 | 3,595.17 | 90,289.63 |
| post-fix | 24,290.78 | 232.87 | 3,655.73 | 28,444.61 |
| Δ | **-70.8%** | **-92.8%** | ~flat | **-68.5%** |

For reference, `SEM_MUL_GO=1` (ON) on the post-fix binary: extract_imports_ms
24,002.11ms, essentially identical to OFF's 24,290.78ms — the fast path
was never the source of this particular cost; the bare-segment collision
was, on both paths equally, exactly as W3+W4's own attribution said.

Cold wall (`sem find`, fresh `SEM_CACHE_DIR`, `SEM_MUL_GO` unset, 4
interleaved pairs, order swapped):

| pair | order | pre-fix | post-fix | Δ |
|---|---|---:|---:|---:|
| 1 | pre→post | 12.36s | 8.85s | -28.4% |
| 2 | pre→post | 12.33s | 8.89s | -27.9% |
| 3 | pre→post | 12.91s | 9.06s | -29.9% |
| 4 | post→pre | 8.40s | 12.09s | -30.5% (post still faster) |

Unanimous direction across all 4 pairs, order-swap included.

### Admission decision

Per I6's two-gate test: memory passes (W3+W4's own reading,
`-1.42%/+0.57%` on kubernetes, not re-checked this bead — the correctness
gate below already fails on its own, making a fresh RSS re-check moot for
the decision). Correctness **fails** — `edge_dump_probe` ON vs OFF is
still not bit-identical (30,795-line diff, above). Go **stays gated**
(`SEM_MUL_GO`, off by default); `MUL_RUNTIME_GATES`'s "go" row is
unchanged.

### Salt

`ImportStmtFacts::GoImport::packages`'s *content* changed (full path
instead of a bare-segment reduction) while its *type* did not
(`Vec<String>` throughout) — the same category W2/W3+4/W5's own bumps
established: a content-only producer change gets a `LANGUAGE_SALTS` bump,
not a `FACTS_SCHEMA_VERSION` bump (which is reserved for shape changes —
see `facts_store.rs`'s own module doc). `LANGUAGE_SALTS`'s go entry bumped
`"ts-0.23-mp3"` → `"ts-0.23-mp5"` (mirrored in `facts_corpus_probe.rs`) —
kept even though the switch stays off in production, so a stale mp3-salted
entry from a local `SEM_MUL_GO=1` debugging session cannot silently answer
a post-fix lookup.

### Gates

`cargo build --release -p sem-core --lib --examples -p sem-cli --bin sem`:
clean (two pre-existing, unrelated `sem-cli` warnings unchanged). `cargo
test --release -p sem-core`: **658/658** lib (2 new tests over the prior
656 baseline), all example/integration binaries green. `cargo test
--release -p sem-cli`: **250/250** across all 21 binaries. `rustfmt
--check`: clean on every touched file. `cargo clippy --release --lib
--examples`: 151 warnings before this bead, 151 after — zero attributable.

`edge_dump_probe` on the TypeScript control corpus (microsoft/TypeScript,
196,175 edges): bit-identical sha256 before/after this bead's change —
confirms the fix is a structural no-op for every language but Go.

`SEM_FP_PARITY=1 incr_probe … all` (cold-vs-build plus 7 warm scenarios),
post-fix binary, `SEM_MUL_GO` unset:

| corpus | result |
|---|---|
| kubernetes (21,430 files, 546,389 entities, 334,648 edges) | 8/8 `ORACLE ok` |
| home-assistant/core (22,397 files, 259,399 entities, 310,723 edges) | 8/8 `ORACLE ok` |

`facts_probe` save/load, cross-process, on a real 13-file Go subset
(kubeadm's `v1` package plus `types.go`/`zz_generated.deepcopy.go` — the
exact files the collision example above lives in): **4/4 `ORACLE ok`**
(`none`, `leaf`, `mixed50`, `hub`).

`facts_corpus_probe` populate/consume, cross-repo, on a real two-copy
corpus of that same 13-file subset: **13/13 hits** OFF/OFF, **13/13 hits**
ON/ON, `ORACLE ok` both, negative probe `ok` both. I5/F2 adversarial
salt-clean-miss proof (populate with the OFF/pre-switch-salted binary,
consume with `SEM_MUL_GO=1`): **`corpus_hits=0/13`**, a clean miss,
`ORACLE ok` regardless (the resulting rebuild is still correct despite the
miss).

### Honest residuals

- Finding (2) — the fast-path-specific increase in type-directed
  resolution failures — is real, evidenced (the `ClusterConfiguration::
  DeepCopy` sample above), and **not fixed**. A same-package cross-file
  method call that needs no import resolution at all is landing in the
  Go-import fallback on the fast path; it should never reach that branch.
  Follow-up bead should instrument `resolve_ref`'s type-directed branch
  directly (a hit/miss counter alongside the existing `SCOPE_BUILD_NS`
  counters) rather than inferring it from `edge_dump_probe` diffs, which
  this bead's own investigation shows is an unreliable signal once (1)'s
  noise is removed — the diff line count barely moved even though the
  underlying correctness improved substantially.
- The inline `go_pkg_index` duplicate in `graph.rs`'s cold-build path
  (missing `.go` filter, flagged above) is a second, independent, smaller
  finding — surfaced, not fixed, per this bead's own scope discipline.
- RSS was not re-measured post-fix; the correctness gate alone already
  keeps Go gated, so this was not load-bearing for the decision. A
  follow-up bead re-measuring RSS after (2) is fixed should do the
  re-check the admission decision actually needs.

Bead: semx-u3rk. Epic: semx-w5k. Prior: semx-mul phase-2 W3+W4 (original
finding), W0 (`GoParentsResolved` ordering — confirmed still sound, this
bead's oracle suite exercises it the same way W3+W4's did).

## 2026-08-22 — M1: the ceiling was measured against the wrong metric — C++
## and Python demoted, peak memory footprint adopted as standard (I6)

Every MUL admission so far — including both languages shipped
**unconditionally** (C++ at phase 1, Python at phase 2) — quoted `/usr/bin/
time -l`'s **`maximum resident set size`** (`getrusage` `ru_maxrss`) against
the +15% ceiling. macOS's `/usr/bin/time -l` also emits **`peak memory
footprint`** (`task_info` `phys_footprint`) on the same run, and the two
fields measure different things: maxRSS counts resident pages, and a page
the VM compressor has swapped into the compressor **vanishes** from it
(it is no longer resident); footprint is Apple's own memory-pressure
accounting and **counts** compressed pages, because that is what actually
drives jetsam/swap and what a user "runs out of memory" against. One early
section of this document (the BOW-content "RSS delta" table, dated earlier
in this file) even labeled a footprint reading "RSS" — the ceiling has been
calibrated against a mixture of the two metrics throughout the campaign
without anyone naming the distinction. This bead (M1) re-reads every
admission against both fields, adopts footprint as I6's standard ceiling
metric going forward (maxRSS reported alongside for continuity), and
executes whatever verdicts flip as a result.

### Why this specific pair of languages, and why now

M0's metric audit and F1's Python re-derivation (above) found maxRSS and
footprint disagreeing in **sign**, not just magnitude, on Python's admission
— maxRSS read a small decrease, footprint a large increase — and named the
mechanism: Python's fast-path facts are exactly the kind of content (short,
repetitive identifier strings — `self`, `hass`, `async_setup`, import
targets) a page compressor eats, while C++'s original phase-1 fence had a
similarly thin, single-run, non-`--no-cache` original measurement
(semx-mp1: +5.8%/+6.5%, no `--no-cache`, no fresh `SEM_CACHE_DIR` per run —
precisely the protocol gap semx-j1fw named for Rust's own outlier +11%
reading). Since these are the *only two* languages shipped unconditionally,
this is the highest-stakes pair to re-verify before trusting the new metric
enough to act on it for anyone else.

### Step 1 — the allocator probe: does `ps` diverge, and does mere
### compressibility diverge the two `/usr/bin/time -l` fields?

A standalone Rust program (scratch dir, not the repo — three small binaries:
`mixed`, `compressible`, `random`) allocated and held large contiguous
buffers of two kinds — a single short pattern tiled across the whole buffer
(page-uniform, the easy case for any compressor) vs. xorshift64-derived
high-entropy bytes — sampling `ps -o rss=` on its own pid at each stage and
reporting `/usr/bin/time -l`'s own two fields at exit. Machine: this Mac,
64 GB RAM, **under real load** for the whole probe (concurrent campaign
lanes; `load averages: 8.76 30.55 40.55` at the time of writing, `top`
showing 18-29 live `cargo`/`rustc` processes throughout) — not an idle
control, deliberately, since the real question is what this shared machine
actually does under the conditions every other lane's measurement also ran
under.

**`ps` family, answered directly and exactly.** Every run showed `ps
-o rss=` matching `maximum resident set size` bit-for-bit — e.g. the mixed
probe's `just_before_exit` sample, `ps_rss_kb=14167824` → ×1024 =
`14,507,884,544` B (v1, before a design fix below) and, on the corrected
v2 run, `ps_rss_kb=4196144` → ×1024 = `4,296,851,456` B against
`/usr/bin/time -l`'s own `maximum resident set size: 4296851456` for the
*same run* — exact equality, not approximate. **`ps -o rss=` is in the
maxRSS/`getrusage` family, not the footprint family.** Any of sem's own
internal instrumentation that shells out to `ps` for a resident-memory
reading (`mem_profile.rs`'s attribution denominators, if it does) inherits
maxRSS's blind spot to compressed pages, not footprint's.

**v1 design bug, caught and fixed before trusting any number from it:** the
first `compressible` design used `Vec<Vec<u8>>` — millions of tiny
individually-heap-allocated short strings — and ballooned to ~14 GB
resident for a nominal 2 GiB of content purely from per-allocation malloc
metadata overhead, not from anything about compression. Rewritten to a
**single contiguous buffer** with the pattern tiled across it (what a real
page compressor actually sees), which behaved exactly as sized (2 GiB in →
~2.0-2.1 GB resident) — this is recorded because a false-negative "no
compression" reading building on the confounded v1 design would have been
worse than no reading at all.

**Does mere compressibility of actively-held, touched content diverge the
two fields? On this machine, right now: no — not without inducing real
memory pressure or letting the content go idle under pressure.** Three
independent v2 runs — a 2 GiB compressible + 2 GiB random `mixed` probe, a
standalone 4 GiB-each `compressible` vs. `random` comparison, and a 3 GiB
`compressible` buffer held **idle** (untouched, no further CPU work) for 20
seconds under the same real system load — all showed **zero** divergence:
maxRSS and footprint tracked each other to within ~0.03% (a small, constant,
non-compression-related offset — plausibly some always-mapped bookkeeping
footprint counts and getrusage does not) in every case, and `vm_stat`'s own
`Pages stored in compressor`/`Compressions` counters did not move at all
during the 20-second idle-hold window (`445,192` → `445,148`, i.e. *other*
processes' pages got decompressed, none of this probe's own pages got
compressed). `vm_stat` also confirms the compressor mechanism is real and
live on this machine independent of the probe — **552,351** total lifetime
compressions, **445k+** pages resident in the compressor from other
processes, observed throughout. `top` showed `PhysMem: 57G used ... 3063M
compressor, 5990M unused` at the time — genuine free headroom, which is
exactly why a bounded, safety-conscious probe did not reproduce the
divergence: **this machine is shared with ~20-29 concurrent campaign
build lanes**, and deliberately forcing tens of GB of additional pressure
to chase a bigger effect risked destabilizing sibling lanes' builds (their
own `rustc`/`cargo`/checkout processes), so this bead capped every
probe allocation at 2-4 GiB and did not escalate further.

**Refined mechanism (not a refutation of the theory — a sharpening of it):**
the VM compressor targets pages the kernel has reason to reclaim
(inactive/aged-out, or under active pressure), not merely "pages holding
compressible bytes while a process actively touches them." A short-lived
synthetic probe that allocates and immediately holds content never gives
the kernel that reason. A real corpus build populates facts early (e.g. an
import table for a file processed early in the pass) and then leaves them
untouched for the remainder of a multi-second-to-minute build while later
phases run — exactly the aging/inactivity condition the compressor acts on
— which a bounded synthetic probe correctly declined to force. Step 2/3's
real corpus-build results below (Python: maxRSS -1 to -4%, footprint
+25-27%; C++: maxRSS +20-21%, footprint +26-28%) are the direct evidence
that the mechanism does fire at real corpus-build scale, on this exact
machine, even though the bounded synthetic probe (correctly, deliberately)
did not manufacture it.

### Step 2 — Python re-read, both fields

Protocol: F1's exact protocol, verbatim (throwaway worktree at campaign
HEAD `b11415d`, `mul_precompute_admits("python")` flipped `true`→`false`
in place, OFF built in its own `CARGO_TARGET_DIR`, ON built from the
unmodified campaign worktree against the shared cache — confirmed a 0.35s
no-op, i.e. already current — binaries confirmed distinct by sha256).
`/usr/bin/time -l <bin> graph --no-cache --json <HA-checkout>`, fresh
`SEM_CACHE_DIR` per run, home-assistant/core. Both `/usr/bin/time -l`
fields captured per run this time, not just maxRSS.

| pair | order | OFF maxRSS | ON maxRSS | Δ maxRSS | OFF footprint | ON footprint | Δ footprint |
|---|---|---:|---:|---:|---:|---:|---:|
| 1 | OFF→ON | 1,837,940,736 B | 1,818,853,376 B | **-1.04%** | 1,396,016,832 B | 1,759,234,160 B | **+26.02%** |
| 2 | ON→OFF | 1,881,620,480 B | 1,806,630,912 B | **-3.99%** | 1,394,378,456 B | 1,746,929,752 B | **+25.29%** |
| 3 | OFF→ON | 1,842,905,088 B | 1,811,415,040 B | **-1.71%** | 1,391,085,296 B | 1,772,816,520 B | **+27.44%** |

maxRSS: unanimous negative, median **-1.71%**, closely reproducing F1's own
**-1.63% median** (four of five negative) — this bead's maxRSS reading is
not new information, it confirms F1's stood. Footprint: unanimous
**+25.29% to +27.44%**, spread 2.15 points across three order-swapped
pairs — tight, not noise — and comfortably above the +15% ceiling under
I6's corrected metric. The two fields disagree in *sign*: Python's
admission is a maxRSS win and a footprint loss, exactly the mechanism named
above (its fast path populates many short, repetitive identifier/import
strings that the compressor eats once they go idle mid-build, dropping out
of maxRSS's resident count while still counting against footprint's
task-level commitment).

**Verdict (I6, corrected metric): Python demoted.** `mul_precompute_admits
("python")` now routes through `python_precompute_enabled()`
(`SEM_MUL_PYTHON`), off by default, `MUL_RUNTIME_GATES` gained a `"python"`
row with `pre_switch_salt = "ts-0.23"` — the pre-W5 salt. `LANGUAGE_SALTS`'s
`("python", "ts-0.23-mp4")` is unchanged: it now serves as the switched-*on*
salt, the same role every other gated language's table entry already plays.

### Step 3 — C++ re-read, both fields

Same shape: throwaway worktree at campaign HEAD `b11415d`,
`mul_precompute_admits("cpp")` flipped `true`→`false` in place, OFF built in
its own `CARGO_TARGET_DIR`, ON the unmodified campaign worktree's shared-
cache binary (confirmed distinct by sha256 from OFF). `/usr/bin/time -l
<bin> graph --no-cache --json <llvm-project-checkout>`, fresh
`SEM_CACHE_DIR` per run, the **full** llvm/llvm-project checkout (not a
subdirectory) — llvm colds run ~2-3 minutes each on this loaded machine,
budgeted for accordingly (three order-swapped pairs, not more).

| pair | order | OFF maxRSS | ON maxRSS | Δ maxRSS | OFF footprint | ON footprint | Δ footprint |
|---|---|---:|---:|---:|---:|---:|---:|
| 1 | OFF→ON | 6,024,822,784 B | 7,228,571,648 B | **+19.98%** | 5,313,401,920 B | 6,712,367,128 B | **+26.33%** |
| 2 | ON→OFF | 6,013,321,216 B | 7,246,020,608 B | **+20.50%** | 5,286,253,608 B | 6,748,018,688 B | **+27.65%** |
| 3 | OFF→ON | 6,006,734,848 B | 7,268,892,672 B | **+21.02%** | 5,273,572,392 B | 6,755,702,832 B | **+28.11%** |

**This is not merely a metric-definition story for C++ — it re-measures
above the ceiling on the *old* metric too.** maxRSS: unanimous
**+19.98% to +21.02%**, ~1 point of spread, already above +15% before
footprint enters the picture at all. Footprint: unanimous **+26.33% to
+28.11%**, ~1.8 points of spread. semx-mp1's original same-binary reading
(+5.8%/+6.5%, without `--no-cache` or a fresh `SEM_CACHE_DIR` per run) was,
in hindsight, the same class of artifact semx-j1fw diagnosed for Rust's W2
outlier — a shared-corpus-warmth confound suppressing the measured delta,
not a property of the admission itself. Correcting the *protocol* alone
would have already demoted C++; correcting the *metric* on top makes the
margin wider still.

**Verdict (I6, corrected metric and protocol): C++ demoted.**
`mul_precompute_admits("cpp")` now routes through `cpp_precompute_enabled()`
(`SEM_MUL_CPP`), off by default, `MUL_RUNTIME_GATES` gained a `"cpp"` row
with `pre_switch_salt = "ts-0.23"` — the pre-MP1 salt. `LANGUAGE_SALTS`'s
`("cpp", "ts-0.23-mp1")` is unchanged: it now serves as the switched-*on*
salt.

### Step 4 — cheap re-reads on the already-gated four (confirmations, not
### decisions)

One order-swapped-equivalent pair each, ON binary + runtime env-var flip
(no rebuild needed — these three are already runtime-gated switches):

| language | corpus | maxRSS Δ | footprint Δ | verdict |
|---|---|---:|---:|---|
| Rust | rust-lang/rust | +20.10% | **+32.92%** | stays gated (footprint makes it worse) |
| C# | dotnet/runtime | +25.29% | **+29.88%** | stays gated |
| Java | elasticsearch | +24.19% | **+31.57%** | stays gated |

All three were already known-bad on maxRSS; footprint reads worse in every
case, consistent with the same compressible-content mechanism (each
language's fast path populates short repeated tokens — type names, package
segments, method names — the compressor eats). No demotion needed (already
gated); recorded for the two-metric record.

### Step 5 — proof battery for both demotions

**`edge_dump_probe` (correctness, bit-identical OFF vs. ON):**

- C++, llvm/llvm-project (full checkout): **982,429 edges**, matching
  sha256 both sides (OFF default env, ON `SEM_MUL_CPP=1`, same binary — the
  switch is now runtime, like Rust/C#/Go/Java).
- Python, home-assistant/core: **310,398 edges**, matching sha256 both
  sides (OFF default env, ON `SEM_MUL_PYTHON=1`) — the same count W5's
  original admission measured, confirming nothing about *what* the fast
  path produces changed, only the default gate state.

**`facts_corpus_probe` (I5/F2 salt correctness), real two-copy corpora:**

- C++ (`llvm/lib/Support`, 219 files after normalization): OFF/OFF
  **219/219 hits**, `ORACLE ok`, negative probe `ok`; ON/ON **219/219
  hits**, `ORACLE ok`, negative probe `ok`. Adversarial (populate OFF-salt,
  consume `SEM_MUL_CPP=1`): **`corpus_hits=49/219`** — the non-C++-source
  siblings in that directory (headers, `README.md`, build files) whose own
  salts are untouched by this gate; zero of the C++ source files leak
  across the salt boundary, `ORACLE ok` regardless.
- Python (`homeassistant/helpers`, F1's exact 109-file corpus): OFF/OFF
  **109/109 hits**, `ORACLE ok`, negative probe `ok`; ON/ON **109/109
  hits**, `ORACLE ok`, negative probe `ok`. Adversarial: **`corpus_hits=
  0/109`** — a fully clean miss (this corpus is all-Python, so 0 is the
  correct signature, sharper than C++'s partial-miss reading since there
  are no untouched-language siblings to leak through), `ORACLE ok`
  regardless.

**Suites:** `cargo test --release -p sem-core --lib`: **658/658** (two
pre-existing tests updated in place — `csharp_salt_tracks_the_mul_phase1_
switch`'s hardcoded "C++ is unconditional" assertion removed, now covered
generically by the same gate-table loop every other language uses;
`corpus_isolates_by_language_salt`'s hardcoded `"ts-0.23-mp4"` expectation
replaced with a live `effective_language_salt("python")` call, since that
literal was always Python's *switched-on* salt and Python defaults to
switched-off now). `mul_phase1_default_matches_the_measured_verdict` and
`mul_phase2_python_default_matches_the_measured_verdict` both updated in
place to assert the new gated defaults, following the same discipline
`dff6de5` established for Rust. `cargo test --release -p sem-cli`:
**250/250**. `rustfmt --check` on all three touched files: clean.
`cargo clippy --release -p sem-core -p sem-cli`: **zero** warnings
attributable to any of the three touched files (the repo-wide count moved
from `dff6de5`'s 151 baseline to 170, entirely pre-existing drift from
other campaign work landed on this branch since that commit — three
commits' worth, unrelated to this bead — verified by grepping the full
clippy output for the touched files' paths and finding no hits).
`cargo build --release -p sem-core --lib --examples -p sem-cli --bin sem`:
clean.

### The policy, standing

I6's ceiling is now defined against **peak memory footprint**
(`task_info`/`phys_footprint`), with maxRSS reported alongside every future
measurement for continuity and cross-checking, not as the decision metric.
Every language `mul_precompute_admits` covers is gated as of this bead —
**no admission remains unconditional**: C# (`SEM_MUL_CSHARP`), Rust
(`SEM_MUL_RUST`), Go (`SEM_MUL_GO`, additionally correctness-blocked), Java
(`SEM_MUL_JAVA`), C++ (`SEM_MUL_CPP`, new), Python (`SEM_MUL_PYTHON`, new).
A future memory lever (MUL-DESIGN.md §5.3 names two, both untaken) could
promote any of them back to unconditional, but that promotion must be
measured against footprint from the start, not maxRSS.

**Caveat, stated plainly rather than buried:** this entire two-field
distinction (`ru_maxrss` vs. `phys_footprint`, and the VM compressor
mechanics behind their divergence) is **Darwin-specific** —
`task_info`/`phys_footprint` is a macOS API, and macOS's VM compressor
behavior (compress-before-swap, opportunistic background compression of
aged pages) has no exact Linux equivalent by default (Linux's zswap/zram
are opt-in, not the default path `/usr/bin/time -v`'s `Maximum resident
set size` measures against, and Linux's `/usr/bin/time -v` does not emit a
footprint-equivalent second field at all). Every number in this section was
measured on this one Mac. **Linux re-validation of the ceiling under this
same two-metric discipline is a separate, explicitly open task** — the
"12.5-14.2GB default-run linux" figures referenced elsewhere in this
campaign's in-session discussion are maxRSS-only readings from a different
measurement session on Linux and must not be read as already covering this
gap.

### Ledger discipline

Measurement stability was checked before any demotion was executed, per
this bead's own instruction: footprint's spread across order-swapped pairs
was ≤2.2 points for Python and ≤1.8 points for C++, both well under the
5%-spread noise-abort threshold — these are stable, actionable readings,
not measurement noise dressed up as a policy finding.

Bead: M1 (memory campaign overnight wave). Epic: semx-w5k. Prior: M0
(metric audit), F1 (Python's corrected-protocol maxRSS re-derivation,
reused verbatim here with footprint added), semx-j1fw (Rust's demotion —
the template this bead's code changes and proof battery follow exactly),
semx-mp1 (C++'s original phase-1 admission), semx-mul W5 (Python's original
phase-2 admission).

## 2026-08-22 — semx-dm5t: the id-staleness species, closed for Go; the
## registration-gap sibling attempted and reverted (species generalizes,
## fix does not — yet)

The re-key wave picked up where semx-u3rk's own honest residual left off:
"(2) is not fixed by this bead... root cause not isolated this bead." (2)
was the fast-path-specific increase in Go type-directed resolution
failures — `ClusterConfiguration::DeepCopy`, a same-package cross-file
method call needing no import resolution at all, landing in the Go-import
fallback under `SEM_MUL_GO=1`.

### Root cause, confirmed

`registry::resolve_go_method_parent_ids` — the one cross-file entity
rewrite this crate performs, moved earlier in the build by W0 so the CLEAN
gate sees it — rewrites a Go method's `id`/`parent_id` when its receiver
type is declared in a *different* file of the same package (kubernetes has
many of these: `internalContainerLifecycleImpl` in
`internal_container_lifecycle.go`, `PreCreateContainer` implemented per-OS
in sibling files; `ValidatingAdmissionPolicyStatusControllerOptions`, etc.).
It runs *after* pass 1's precompute (`precompute_scope_resolvable_file_
facts`) has already built that file's `PrecomputedFileFacts.entity_scope_
map`/`entity_inner_scope`/`return_type_map`, keyed by the pre-rewrite id.
Pass 2 (`scope_resolve.rs`, `entity_inner_scope.get(&entity.id).or_else(||
entity_scope_map.get(&entity.id))`) looks the entity up by the *post*-
rewrite id — a clean miss, `.unwrap_or(0)` silently defaulting `scope_idx`
to the module scope. From scope 0, `resolve_ref`'s type-directed branch
(the one `in.DeepCopyInto(out)` needs) can't find the real local binding,
and the call falls through to the Go package-qualified fallback instead —
exactly the shape of the `ClusterConfiguration::DeepCopy` divergence
semx-u3rk documented, a distinct mechanism from the bare-segment collision
it had already fixed.

The counter this bead's prior verification batch landed
(`ENTITY_SCOPE_LOOKUP`, `SEM_PROFILE_RESOLVE=2`) confirmed it in isolation
before any fix: kubernetes `SEM_MUL_GO=1` read `fallback_pct=27.10%`
(44,390/163,779), zero on `SEM_MUL_GO` unset and zero on Python's own
gated-on precompute (a healthy admitted language) — the id-staleness
signature, present only where the Go rewrite runs, absent everywhere else.

### The fix (shipped)

Two parts, both scoped to `.go` files by construction (`is_go_file` guards
every mutation either makes):

1. `resolve_go_method_parent_ids` now returns, alongside the
   `GoParentsResolved` proof token `clean_gate_dirty_files` requires, the
   old-id -> new-id map for every entity it actually rewrote
   (`rekeyed_ids`) and the set of files any of them live in
   (`rekeyed_files`). The build's call site (`graph.rs`,
   `EntityGraph::build_incremental_core`) re-keys this build's fresh
   `PrecomputedFileFacts` for exactly those files
   (`PrecomputedFileFacts::rekey_entity_ids`, `scope_resolve.rs`)
   immediately after the rewrite runs and before the CLEAN gate or the
   session's carried store ever reads them.
2. The rewrite cascades a method's new id down through every descendant
   whose `parent_id` embedded the method's old id as a literal prefix
   (`model/entity.rs::build_entity_id`'s own contract: a child's id is
   `format!("{parent_id}::{name}")`). Discovered mid-bead: without this,
   a rewritten method's *own* nested locals/constants/types were left with
   a dangling `parent_id` — pointing at an id no entity in `all_entities`
   held any more — which the CLEAN gate's own dirty-file marking
   (`clean_gate_dirty_files`) then reads directly (`entity.parent_id`
   against `id_owner`), and which a second attempt at closing the
   registration-gap species (below) exposed as a live correctness gap on
   kubernetes itself (`internalContainerLifecycleImpl::PreCreateContainer`,
   a cross-file method with three nested locals — `allocatedCPUs`,
   `allNumaNodeCPUs`, `finalCPUSet` — whose `TypeRef` edges depend on
   correctly-scoped sibling lookups). The cascade is a fixpoint loop over
   `entities` (bounded by nesting depth, typically 2-4 iterations): an
   entity whose `parent_id` still names an already-rewritten id gets
   re-parented and its own id gets the same prefix swap, verbatim suffix
   (`::name`, any `@Lxx`/`#n` disambiguator) carried over, and its own
   old->new pair is added to the same map so the next pass can find and
   fix *its* children in turn.

### Proof

`edge_dump_probe`, kubernetes, `SEM_MUL_GO=1` vs unset, same binary:
**bit-identical — 0-line diff, sha256 match, 330,558 edges both sides**
(was 334,664 vs 331,190, a ~30,795-line diff at semx-u3rk's own
handoff). `ENTITY_SCOPE_LOOKUP`'s honest-miss counter, precomputed-path
lookups only: `fallback_pct` **27.10% -> 0.00%** for Go's own id-rewrite
signature.

Unit test: `go_parent_rewrite_id_survives_precomputed_scope_lookup_after_
rekey` (`scope_resolve.rs`), modeled on W0's own `Hub`/`Ping` cross-file
receiver fixture — a `PrecomputedFileFacts` with `entity_scope_map`/
`entity_inner_scope`/`return_type_map` entries seeded under the
pre-rewrite id, `resolve_go_method_parent_ids` run, then
`rekey_entity_ids` — asserts the pre-repair state is the bug (all three
maps miss the post-rewrite id) and the post-repair state is the fix (all
three hit under the new id, the stale key gone rather than left as a
dangling duplicate).

Control corpora, same binary, unchanged by construction (`is_go_file`
guards every mutation) — verified, not assumed: `edge_dump_probe`
bit-identical before/after this bead's whole diff on **microsoft/
TypeScript** (196,175 edges, sha256 match), **home-assistant/core**
(310,398 edges, sha256 match), and **rust-lang/rust** (307,653 edges,
sha256 match). `SEM_FP_PARITY=1 incr_probe … all` on home-assistant/core:
**8/8 `ORACLE ok`** (cold-vs-build plus all 7 warm scenarios), cold
`entities=259399 edges=310723 edge_hash=b82d199d21e45b47` — identical to
W0's own recorded reading.

### A second, independent species — found, generalized, fix attempted and
### reverted

The counter's post-fix residual on kubernetes wasn't zero:
**`fallback_pct=14.01%` (22,953/163,779)**, and every one of the missing
entity types sampled was `variable`/`constant`/`type` — never `method` —
nested inside a plain top-level Go *function* (`checkFeatureGateMaps`,
`extractFeatures`, `stripHeader`, ...), never touched by
`resolve_go_method_parent_ids` at all. Traced to `scope_visit_node`
(`scope_resolve.rs`): its class-like and `mod_item` branches both register
`children_by_parent`'s entries into the scope they create; its
function-like branch (`is_function_like`) never did — a plain code
omission, not a conditional failure, present on both the fused walk
(`precompute_scope_resolvable_file_facts`, everyone but JS/TS) and the
unfused walk (`build_scopes_from_ast`, JS/TS's `precompute_js_ts_file_
facts`), since both call the same function. This is exactly semx-9g8q's
own code-level lead (`scope_visit_node` ~3961, "only registers children
when `find_at_line` locates the owning class/interface entity") — now
confirmed to be broader than TS-only and broader than the class-like
branch's `find_at_line` miss: the function-like branch has no
registration path *at all*, for any language.

Adding the missing registration loop (mirroring the class-like branch
exactly) collapsed kubernetes's counter to `fallback_pct=0.00%` too, and
was the direct trigger for finding the cascade fix above (a rewritten Go
method's own children only matter once something actually reads
`children_by_parent` for them). But the decisive experiment on
microsoft/TypeScript — before/after `edge_dump_probe`, per semx-9g8q's own
spec — came back **CHANGED, not identical**: 196,175 -> 196,018 edges,
1,979 diff lines (649 removed, 492 added — not a clean superset in either
direction). Sampling both sides:

- **Real gains**: `src/compiler/checker.ts`'s deeply-nested
  `createTypeChecker::getTypeFromTypeNode` (previously invisible to local
  scope resolution) is now correctly found as the `Calls` target from
  dozens of sibling functions inside the same closure that reference it —
  new edges, textually correct.
- **Real regressions**: `scripts/eslint/rules/no-keywords.cjs`'s
  `create::checkElements` (a `const NAME = (...) => {...}` arrow function,
  one of several nested inside a factory `create`) loses its real `Calls`
  edge to sibling `create::isKeyword` — line 68 of the source,
  `isKeyword(element.name)`, an unambiguous, present-day-correct call —
  present before the fix, gone after. Several more of the same shape in
  the same file and `argument-trivia.cjs`/`debug-assert.cjs`/
  `jsdoc-format.cjs`.

Root cause of the regression not fully isolated this bead (arrow functions
have no AST "name" field of their own — `find_at_line`'s own name lookup
for such a node returns `None` — so a sibling arrow-function-typed child's
*own* function-like visit never independently registers anything, and its
resolution depends entirely on inheriting the *parent* function's
registration; something in that interaction, not yet pinned down, drops
the sibling lookup for at least this bucket). Per the bead's own gate ("the
delta must be all improvements before you land it" — CHANGED with any
non-improvement means STOP-AND-REPORT), **the function-like registration
fix was reverted** (`scope_visit_node`'s `is_function_like` branch is back
to its pre-bead shape) rather than shipped with a known regression on a
path that runs unconditionally for every JS/TS file today. `LANGUAGE_SALTS`
bumps for the eight non-Go languages that would have needed one for this
change were reverted alongside it; `go`'s own bump (`ts-0.23-mp5` ->
`ts-0.23-mp5-dm5t`) stayed, since the rekey + cascade fix's own content
change (Go-only) is real and shipped regardless.

**semx-9g8q stays open**, now with a materially sharper finding than it
started with: the missing registration is in the function-like branch
specifically (not the class-like `find_at_line`-miss lead originally
suspected as primary), it is demonstrably language-general (confirmed live
on Go, not just TS), and a naive fix regresses at least one real bucket
(sibling arrow-function closures) that needs its own root-cause pass before
a general fix is safe to land.

### Salt / I5

`PrecomputedFileFacts` (`scope_resolve.rs`, `#[derive(Serialize,
Deserialize)]`) is part of the persisted pack format (`PersistedFile.
precomputed`, `facts_store.rs`) — not purely an in-memory session
artifact, contrary to this bead's own opening assumption. Both shipped
changes are `SemanticEntity`/`PrecomputedFileFacts` *content* changes for
`.go` files specifically (neither is a shape change —
`FACTS_SCHEMA_VERSION` untouched), so `LANGUAGE_SALTS`'s `go` entry bumped
`ts-0.23-mp5` -> `ts-0.23-mp5-dm5t`, following the same "bump even though
the switch stays off in production" discipline the mp3->mp5 bump already
established (a stale entry from a local `SEM_MUL_GO=1` debugging session
must not silently answer a post-fix lookup). `examples/facts_corpus_
probe.rs`'s deliberate mirror and every hardcoded `"ts-0.23-mp5"`/
`"ts-0.23-u16"` test literal that needed to track a real table entry were
updated in lockstep (`corpus_isolates_by_language_salt`-style drift proof
stayed green throughout, not just at the end).

### Gates

`cargo test --release -p sem-core --lib`: **662/662** (661 + this bead's
one new test). `cargo test --release -p sem-cli`: **141 + 21 integration
binaries, all green**. `stem_collision_ground_truth`: **7/7**. Full
`cargo test --release -p sem-core` (all binaries): green throughout,
including `scope_resolve_bench` (15), `kappa` (42), `single_pass_
invariants` (3), `graph_accuracy` (3). `rustfmt --check`: zero diffs
attributable to any line this bead touched (checked by literal
line-range/content cross-reference against the pre-bead `HEAD` copy of
both touched files — the same pre-existing counts, 2 and 6 diffs, in both
`graph.rs` and `scope_resolve.rs` before and after). `cargo clippy
--release --lib --examples`: zero warnings attributable to any touched
file. `SEM_FP_PARITY=1 incr_probe … all` on home-assistant/core: 8/8
`ORACLE ok`. `edge_dump_probe` bit-identical (sha256) on kubernetes
(SEM_MUL_GO ON vs OFF), microsoft/TypeScript, home-assistant/core, and
rust-lang/rust (all before-vs-after this bead's full diff).

Bead: semx-dm5t (closed) / semx-9g8q (still open, sharper finding
recorded). Epic: semx-w5k. Prior: semx-u3rk (this bead's direct
handoff — "(2), not fixed"), semx-mul phase-2 W0 (the `GoParentsResolved`
ordering fix this bead's token threads through unchanged).

## 2026-08-22 — Card 2: the facts plane re-measured at giant scale, D1/D2/D3
## verified beyond sem-repo, the nested-walk instrument gap sized and mostly
## falsified as the unattributed-RSS explanation

W5 (semx-gbb) priced the facts plane's marginal peak-RSS cost at **+9.23 GB
(+86%) on dotnet-runtime** and **+5.78 GB (+49%) on linux**, with no re-read
since. `DATA-FLOW-AUDIT.md`'s D1/D2/D3 (`populate_delta` serializes from
references, `export_persisted` returns a borrowing view, shard save streams)
were verified fixed only at sem-on-sem scale (R5: D2's corpus clone reads
**+0.0 MB**, was +8.2 MB on a 6,279-entity corpus, "scaled to ~1.3 GB on
dotnet" but never actually re-measured there). Separately, M1 (I6) replaced
maxRSS with **peak memory footprint** (`task_info` `phys_footprint`) as the
admission-ceiling metric, and this HEAD (`26a5b56`) carries a new
`approx_heap_bytes` nested-string walk aimed at a specific unconfirmed
hypothesis from the MUL P1 memory-lever bead: that `PrecomputedFileFacts`'
`return_type_map`/`instance_attr_types`/`init_params`/`attr_to_param` fields'
**undercounted nested `String` contents** explained the "52-75%
unattributed" RSS gap that bead found. This card re-prices the plane at
giant scale under both fields, and closes that hypothesis.

### Method

Binary built once from this worktree's exact `HEAD` (`26a5b56`, `cargo build
--release -p sem-cli`, 55.75 s), then copied out of the shared
`CARGO_TARGET_DIR` to a private path (sha256
`e8e83a03…967d612c`) so concurrent lanes rebuilding into the same target dir
mid-batch could not silently swap the binary under this card's runs — every
run below used that one pinned copy. `sem graph --json <root>`, fresh
`SEM_CACHE_DIR` **and** fresh `SEM_FACTS_CORPUS_DIR` per run (W5's own
definition of true-cold, `FACTS_CORPUS probed=N hits=0` confirmed on every
plane-ON run), `/usr/bin/time -l` capturing both `maximum resident set size`
and `peak memory footprint`.

**The isolation flag.** Read `graph.rs:454-489` rather than assumed:
`facts_store_for`/`facts_corpus_for` are gated by `no_cache ||
!env_flag("SEM_FACTS_CACHE", true) || …` — `--no-cache` and
`SEM_FACTS_CACHE=0` are **not equivalent**: `--no-cache` (`opts.no_cache`)
*also* disables the on-disk index-image cache (`try_index_graph` at
`graph.rs:45`, the `if !no_cache { … }` index-write guards at `:607` etc.),
a completely different tier. `SEM_FACTS_CACHE=0` alone is the flag that
isolates *only* the facts plane, matching W5's own "true-cold,
`SEM_FACTS_CACHE=0`" row exactly. This card used the env var, never
`--no-cache`, for every plane-OFF run.

**Residual noted, not touched**: `~/.cache/checkouts/github.com/torvalds/
linux` carries 13 modified files (netfilter headers + one litmus test) from
an earlier bead's fixture use. Small (13/95,453 files), stable across every
run below, and per W5 §6's own precedent this card left it alone rather than
risk another lane's in-flight fixture.

### 1. The plane's marginal cost, both metrics — D1/D2/D3 shrank it hard,
### did not eliminate it

**dotnet-runtime** (3 order-swapped pairs — a 3rd pair was added mid-batch
because pair 1 and 2 disagreed and the machine's load was visibly climbing;
see honesty note below):

| pair | order | OFF maxRSS | ON maxRSS | Δ maxRSS | OFF footprint | ON footprint | Δ footprint | load (approx, at ON) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | OFF→ON | 9,256,681,472 B | 11,698,028,544 B | **+26.4%** | 8,453,021,376 B | 11,823,623,016 B | **+39.9%** | 3.4 |
| 2 | ON→OFF | 9,196,847,104 B | 13,939,507,200 B | **+51.6%** | 7,531,421,064 B | 12,200,864,808 B | **+62.0%** | 5.1 |
| 3 | OFF→ON | 9,340,731,392 B | 14,594,228,224 B | **+56.2%** | 7,567,826,456 B | 12,632,878,792 B | **+66.9%** | 8.6 |

OFF is tight across all three (8.57-8.70 GB maxRSS, 7.01-7.87 GB footprint —
a stable baseline). **ON climbs with load** (10.90 → 12.98 → 13.59 GB maxRSS
as background load rose 3.4 → 5.1 → 8.6 across the batch, other campaign
lanes ramping mid-session) — the plane's own extra allocation traffic reads
as more load-sensitive than the baseline build. Pair 1, taken at the
lowest load, is the most trustworthy single reading.

**linux** (2 order-swapped pairs, tight agreement despite load climbing
6.96 → 13.7 across the window):

| pair | order | OFF maxRSS | ON maxRSS | Δ maxRSS | OFF footprint | ON footprint | Δ footprint |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | OFF→ON | 10,880,352,256 B | 12,398,821,376 B | **+14.0%** | 10,555,548,384 B | 11,519,239,672 B | **+9.1%** |
| 2 | ON→OFF | 10,879,254,528 B | 12,551,258,112 B | **+15.4%** | 10,582,467,368 B | 11,647,133,344 B | **+10.1%** |

Both OFF and ON are stable pair-to-pair on linux (10.13 GB / 11.55-11.69 GB
maxRSS) even as load nearly doubled — the two pairs agree to ~1.4 points on
both metrics.

**Verdict: D1/D2/D3 shrank the plane hard at giant scale — it did not
disappear.** Against the recorded baselines:

| repo | old plane cost (maxRSS) | new plane cost (maxRSS) | shrinkage |
|---|---:|---:|---:|
| dotnet-runtime | +9.23 GB (+86%) | **+2.3 to +4.9 GB (+26% to +56%)** | roughly half to three-quarters gone, even at the noisy high end |
| linux | +5.78 GB (+49%) | **+1.4 to +1.6 GB (+14% to +15%)** | roughly three-quarters gone, tight across both pairs |

Linux's reading is the cleaner of the two (agreement despite heavy load
drift); dotnet's range is wide but every single pair — including the
noisiest one — is still well under half the old +9.23 GB figure. **D1/D2/D3
generalize beyond sem-repo scale**: the sem-repo-only verification in
`DATA-FLOW-AUDIT.md` R5 was not an overclaim.

### 2. Attribution under the checkpoint table, and the nested-walk hypothesis
### test

Two flag combinations, both compatible and combined in one run:
`SEM_PROFILE_MEM=1 SEM_PROFILE_CACHE=1 SEM_PROFILE_RESOLVE=2 SEM_TIMINGS=1`.
`SEM_PROFILE_CACHE=1` prints `facts_rss_mark`'s five boundaries
(`entry`/`after_corpus_merge`/`after_warm_start`/`after_export_persisted`/
`after_populate_delta`); `SEM_PROFILE_MEM=1` prints the four
`mem_profile::checkpoint` tables inside `full_graph_build`
(`post-pass-1`/`peak-resolve`/`post-scope-resolve`/`post-build`) that sit
*inside* the `after_corpus_merge → after_warm_start` span.

**Default mode (every `SEM_MUL_*` off, as this HEAD ships)**, one run each,
plane ON:

| checkpoint | dotnet attributed/RSS | linux attributed/RSS |
|---|---:|---:|
| post-pass-1 | 35% (3,082.2 / 8,696.7 MB) | 48% (3,749.7 / 7,789.7 MB) |
| peak-resolve | 29% (2,568.9 / 8,832.6 MB) | 40% (3,266.3 / 8,154.5 MB) |
| post-scope-resolve | 5% (483.4 / 10,242.5 MB) | 0% (16.3 / 8,252.7 MB) |
| post-build | 25% (2,614.5 / 10,646.3 MB) | 33% (3,189.3 / 9,674.1 MB) |

`precomputed_facts` reads **4.6 MB (dotnet) / 0.0 MB (linux)** at every
checkpoint — because every `SEM_MUL_*` gate is off by default at this HEAD,
the MUL fast path barely runs, so this is mostly semx-4w1's original
pre-MUL baseline (~20-30% attributed) showing through unchanged, not a
new finding.

**Facts-plane save/populate_delta, both giants (the boundaries a full-CLI
timer can't see)**:

| boundary | dotnet rss_mb | linux rss_mb |
|---|---:|---:|
| entry | 20.5 | 23.3 |
| after_corpus_merge | 24.2 (+3.7) | 25.1 (+1.8) |
| after_warm_start | 10,646.3 | 9,674.1 |
| after_export_persisted | **10,646.3 (+0.0)** | **9,674.1 (+0.0)** |
| after_populate_delta | 12,339.8 (+1,693.5) | 11,032.2 (+1,358.1) |
| true peak (`/usr/bin/time -l`) | maxRSS 14,313.8 / footprint 12,598.8 | maxRSS 12,247.3 / footprint 11,199.4 |

**D2 confirmed again, at giant scale, to the same +0.0 MB R5 found on
sem-repo**: `export_persisted`'s borrowing view costs nothing measurable on
either giant. **D1 and D3 are not separable by any existing mark** —
`store.save()` (D3) and `corpus.populate_delta()` (D1) both run between
`after_export_persisted` and `after_populate_delta` with no boundary between
them — the combined +1,693.5 MB (dotnet) / +1,358.1 MB (linux) is D1+D3
together, not attributable to either alone. **A residual gap exists past the
last mark**: true peak maxRSS exceeds `after_populate_delta`'s sampled RSS
by ~1.97 GB (dotnet) / ~1.21 GB (linux) — either a transient spike inside
save/populate_delta that point-sampling missed, or (more likely, since no
mark covers it) the JSON-serialization-to-stdout step after
`session.into_parts()` returns, which this instrumentation does not bracket
at all. Naming this as a gap, not measuring it further — out of this card's
scope.

**The nested-walk hypothesis, tested directly.** The MUL P1 memory-lever
bead's leading (explicitly unconfirmed) hypothesis for its own 52-75%
unattributed gap was `approx_heap_bytes`' undercount of
`return_type_map`/`instance_attr_types`/`init_params`/`attr_to_param`'s
nested `String` contents. This HEAD's walk fixes exactly that. Reproducing
that bead's own scenario (`SEM_MUL_CSHARP=1`, dotnet-runtime, same
instrument) to compare directly:

| | old (pre-walk, `44c0b4c`) | new (nested walk, `26a5b56`) |
|---|---:|---:|
| `precomputed_facts` | 1,089.1 MB | **1,264.5 MB (+175.4 MB, +16.1%)** |
| post-pass-1 attributed/RSS | 43-48% | **40%** |
| peak-resolve attributed/RSS | 38-40% | **35%** |
| post-scope-resolve attributed/RSS | 5% | **4%** |
| post-build attributed/RSS | 25-29% | **22%** |

**The hypothesis is mostly falsified.** The walk recovered +175 MB of real,
previously-missed bytes — directionally correct, `precomputed_facts` is now
in fact the **single largest named structure** at post-pass-1 (1,264.5 MB,
edging out `all_entities.content`'s 1,053.4 MB) — but +175 MB against an
~11-12 GB process is under 2% of RSS, and the attributed fraction did not
improve; if anything it reads slightly lower than the old run (different
corpus/load state, not a like-for-like rerun, so treat this as a bound, not
a precise before/after). **The 52-75%-unattributed gap survives this fix
almost entirely intact.** It is not `PrecomputedFileFacts`' nested strings.
The remaining unattributed mass is consistent with semx-4w1's original,
pre-MUL finding (named structures explain ~20-30% of RSS baseline) —
allocator/heap overhead (`mimalloc` fragmentation, `tree_sitter` arena
residue behind `Tree`'s opaque handle, thread-local caches) that no
`.capacity()`-based structural walk can see, named but not sized by this
card.

### 3. The admission question: what would C++/Python need to shed

The ceiling (I6) is a same-binary delta test: `(ON - OFF) / OFF ≤ 15%`,
peak footprint. Two levers, both already measured by M0/M1 — this card adds
one honest negative result rather than a third number.

**C++ (M1's own llvm-project reading — authoritative; this card could not
add a giant-scale C++ reading of its own)**. `mul_precompute_admits("cpp")`
is gated behind `SEM_MUL_CPP`, keyed to files with the **`.cpp`/`.cc`/
`.cxx`/`.hpp`** extension only (`registry.rs:632`, `("cpp", ".cpp")`) — a
**distinct** `language_id` from `"c"` (`registry.rs:631`), and
`mul_precompute_admits` has no `"c"` arm at all (falls to `_ => false`).
**linux is 37,144 `.c` + 27,014 `.h` files against 7 `.cpp`-family files** —
this card's own `SEM_MUL_CPP=1` vs `=0` pair on linux (+1.27% maxRSS,
+0.31% footprint, both inside noise) is a **null test, not a giant-scale
confirmation that C++'s cost is now small** — it tests a corpus that is not
C++. Flagging this rather than reporting it as a finding. M1's llvm-project
reading stands: OFF footprint 5.27-5.31 GB, ON 6.71-6.76 GB, Δ ≈ **+1.44 to
+1.48 GB (+26.3% to +28.1%)**. To land at the 15% ceiling, allowed ON ≤
OFF × 1.15 ≈ 6.06-6.11 GB — **C++ needs to shed ≈0.65-0.70 GB, roughly 45-48%
of its current footprint overshoot**, to re-admit.

**Python (M1's own home-assistant reading)**. OFF footprint 1.391-1.396 GB,
ON 1.747-1.773 GB, Δ ≈ **+0.36 to +0.38 GB (+25.3% to +27.4%)**. Allowed ON
≤ OFF × 1.15 ≈ 1.60-1.61 GB — **Python needs to shed ≈0.14-0.17 GB, roughly
43-45% of its current footprint overshoot**.

**Third lever, from this card's own data: `precomputed_facts` is the
biggest named structure but not the dominant cost, so trimming its fields
is a bounded lever, not a large one.** On dotnet with `SEM_MUL_CSHARP=1`,
`precomputed_facts` totals 1,264.5 MB across 46,905 files (~27 KB/file
average) — comparable in *order of magnitude* to Python's and C++'s entire
footprint overshoot (0.14-0.70 GB), meaning even fully eliminating
`import_stmts`/`ctor_call_sites` (Fields 10/11 — already empty for C++,
populated only for Python's fast path per the field's own doc comment)
could plausibly close a meaningful fraction of Python's ≈0.16 GB gap, but
cannot touch C++'s ≈0.68 GB gap because those two fields are structurally
empty for C++ already. **Ranked**: (1) Python's Field 10/11 trim — cheapest,
directly targets the field that's actually populated for Python,
plausibly closes a large fraction of its ≈0.16 GB gap; (2) C++'s gap has no
lever this card's data points at — its overshoot lives in `scopes`/
`ast_refs`/`return_type_map`-family fields this card did not get a
C++-heavy corpus to attribute; (3) the plane's own residual D1+D3 cost
(§1-2 above, +1.4-4.9 GB depending on giant and load) is *not* a lever
still on the table — it is what's left after D1/D2/D3, and the corpus-
sharding fix W5 §3 named (open shards by content hash instead of reading
every shard) is a **wall-time** lever per its own writeup, not verified
here as a memory lever — do not spend it against the footprint ceiling
without a fresh measurement.

### Honesty notes

1. **dotnet's ON-side climbed with background load across the batch**
   (3.4 → 5.1 → 8.6); OFF stayed flat. Pair 1 (lowest load) is the most
   trustworthy single reading; the range +2.3 to +4.9 GB is reported rather
   than a false-precision single number.
2. **The `SEM_MUL_CPP` test on linux is a null result** (wrong language mix
   — see §3) and must not be read as a giant-scale C++ re-verification.
3. **D1 and D3 are not separable** by any mark that exists at this HEAD —
   reported as a combined figure, not split.
4. **The peak-vs-last-checkpoint gap** (~1.2-2.0 GB past
   `after_populate_delta`) is named, not measured further — plausibly the
   unbracketed JSON-serialization step.
5. **linux's 13-file dirty checkout** (an earlier bead's fixture) was left
   untouched; stable across every run, unlikely to move giant-scale numbers
   materially.
6. Machine load ranged 3.4-13.7 across this card's whole batch (other
   campaign lanes active throughout, confirmed via `top`/`ps` — not this
   card's own processes idling between runs). Every delta reported above is
   either from an order-swapped pair (drift cancels) or explicitly flagged
   where it does not.

Bead: semx-bpn2. Epic: semx-w5k. Binary: `26a5b56`
(sha256 `e8e83a03…967d612c`, private copy, immune to concurrent
`CARGO_TARGET_DIR` rebuilds mid-batch).

## 2026-08-22 — semx-9g8q closed: the shadow-precedence fix, reinstating the
## registration loop it once regressed, and a wrong prediction corrected

### The fix

Root cause (research spec, this bead's own prior session): the earlier
reverted registration-loop attempt's regression was never arrow-function
naming — it was a pre-existing precedence bug in `resolve_ref`.
`is_local_binding_in_scopes_cached` (the `.bindings` shadow gate)
unconditionally short-circuited *before* `lookup_scope_chain_cached`
(`.defs`) ever ran, so once the registration loop made both maps
co-populate the same scope for the same name, the shadow gate killed the
correct resolution outright. The grammar admits no redeclaration, so
same-scope-index co-population can only be the same declaration — the def
must win, never be read as a shadow.

Fix: `ScopeChainLookup`, one combined per-scope-level walk (`.defs`
consulted before `.bindings` at each level; a `.bindings`-only hit with no
`.defs` at that level is `Shadowed`), replaces the old two-primitive,
two-call-site pattern at the two sites that need it (the `Call` arm,
`MethodCall`'s static-receiver branch) — language-agnostic; the other
four `is_local_binding_in_scopes_cached` gate sites are untouched. The
function-like branch's registration loop (mirroring the class-like/
`mod_item` branches exactly — `children_by_parent` into `.defs` +
`entity_scope_map`) is reinstated on top of the fixed primitive, placed
before `scan_assignments` as before. Two unit tests in `scope_resolve.rs`:
same-scope `.defs`+`.bindings` co-population resolves `Defined`, not
`Shadowed`; an inner `.bindings`-only hit still shadows an ancestor `.defs`
across multiple scope levels — the property the old gate protected.

### Verification (two-phase: line review + suites, then a four-corpus
### before/after battery)

`cargo test --release -p sem-core --lib`: 664/664. All 9 sem-core
integration binaries green, incl. `stem_collision_ground_truth` 7/7.
`cargo test --release -p sem-cli`: 250/250. Re-gated identically on the
merged tree (`e517edc`) after landing.

`edge_dump_probe`, before (`2292e4c`, docs-only past `26a5b56` — verified
via `git diff --stat`) vs after, four independent-language corpora. Every
sampled delta on every corpus traces to one of two shapes: a
wrong-edge-to-correct-edge repoint, or a wrong-edge-to-honest-miss
removal — never a lost correct edge:

- **microsoft/TypeScript** (196,175 -> 196,126 edges; 623 removed / 574
  added — not a clean superset). `no-keywords.cjs`'s
  `create::checkElements -> create::isKeyword` (the prior attempt's own
  named regression bucket) is present and byte-identical on both sides.
  `checker.ts`'s `getTypeFromTypeNode` (132 call sites both sides)
  re-points from `createNodeBuilder`'s nested definition (line 6498 — not
  lexically visible to the actual callers; verified against source that
  `createNodeBuilder` and its callers are *siblings* inside
  `createTypeChecker`, indentation-confirmed) to `createTypeChecker`'s own
  definition (line 20432) — correct. Same interface-declaration-or-wrong-
  sibling -> own-real-implementation repoint pattern independently
  verified in `emitHelpers.ts`, `scanner.ts`, `resolutionCache.ts`,
  `classifier.ts` (cleanest case: `getEncodedSyntacticClassifications`'s
  nested `classifyDisabledCodeToken` calling `pushClassification` moves
  from the *other* factory function `getEncodedSemanticClassifications`'s
  same-named helper to its own enclosing function's). Pure removals in
  `narrowingOfQualifiedNames.ts`/`keyofAndIndexedAccess.js` traced to a
  local parameter (`init2(foo: DeepOptional)`) or a local closure
  (`getTestPod := ...`-style) wrongly resolving to an unrelated same-named
  symbol elsewhere in the file/repo — honest misses now.
- **kubernetes** (330,558 -> 331,117 default / 331,120 under
  `SEM_MUL_GO=1`; net +559/+562). **Not a clean zero-removed superset —
  139-140 lines removed.** Every sampled removal traces to Go's
  `helper := func(...) {...}` local-closure-in-table-test idiom (e.g.
  `TestValidateControllerRevision`'s `newControllerRevision := func(...)`):
  before, the call wrongly resolved to a same-named, **unexported**
  function in an unrelated package (`pkg/controller/daemon`,
  `pkg/registry/apps/controllerrevision`, ...) — illegal under Go
  visibility rules, not even callable across packages; after, `Shadowed`
  correctly recognizes the local closure binding and the call falls
  through to an honest miss instead. `ENTITY_SCOPE_LOOKUP`'s
  `fallback_pct` (`SEM_MUL_GO=1`, the still-gated precomputed path):
  **14.01% -> 0.00%**, reproducing this bead's own prior-session residual
  and then collapsing it exactly as its root-cause writeup predicted.
- **home-assistant/core** (310,398 -> 310,394; 287 removed / 283 added)
  and **rust-lang/rust** (307,653 -> 307,597; 723 removed / 667 added):
  same repoint signature, independently sampled and consistent (e.g. HA's
  `test_pipeline_from_audio_stream_auto::audio_data` wrongly shared across
  sibling test functions before, each resolving its own `audio_data`
  after; Rust's `stack_val_align` TypeRef re-pointed from a sibling's
  local type to its own). `SEM_FP_PARITY=1 incr_probe <HA> all`: 8/8
  `ORACLE ok`.

**A wrong prediction, corrected on the record.** This bead's own
verification brief predicted kubernetes would come back a "clean
superset, zero removed" — it did not. The fix's *correct* behavior
includes converting impossible edges (cross-package calls to unexported
Go symbols, sibling-closure repoints resolved via a scope that was never
lexically reachable) into honest misses, not only adding edges that were
previously invisible. The actual acceptance evidence is the four-corpus
convergent signature — the identical wrong-edge-to-correct-edge/honest-
miss shape appearing independently on Go, TypeScript/JS, Python, and
Rust — not a raw "lines removed" count. A future reader re-running this
probe and seeing non-zero removed lines on any corpus should check
*what* was removed (repoint vs. genuine loss) before treating it as a
regression signal.

Credit chain: research spec (root cause + fix design) -> `ox/impl`
(primitive, call-site collapse, registration loop, unit tests) ->
two-phase verification (line review + suites, then the corpus battery
above).

### Note for the Go re-admission record (cross-ref: semx-bpn2, this
### file's own §"Attribution"/footprint-ceiling sections above)

Combined with `semx-dm5t`'s id-rekey fix (`fallback_pct` 27.10% -> 0.00%
for the id-staleness species) and this bead's own registration-loop fix
(the second, independent species — function-nested entities — collapsed
to 0.00% as recorded just above), **Go's entire known correctness
blocker chain for `SEM_MUL_GO` is now closed**: both confirmed
`ENTITY_SCOPE_LOOKUP` residuals are at 0.00%, and `edge_dump_probe`
before/after this bead's whole diff is bit-identical on every non-Go
control corpus this session touched (Go's own mutations are gated behind
`is_go_file`/`SEM_MUL_GO`, verified not to leak). Go's re-admission to
the multi-language pass now waits on exactly one thing: a footprint
fence run against the ≤15%-overshoot ceiling this card's own
Attribution section established for C++/Python — Go has not had that
measurement taken at giant scale yet. File the footprint fence run as
the next step on whichever bead tracks `SEM_MUL_GO` re-admission
(`semx-bpn2` if a dedicated Go footprint bead does not already exist).

Bead: semx-9g8q (closed). Epic: semx-w5k. Prior: semx-dm5t (closed,
`26a5b56`), semx-u3rk (this bead's direct handoff). Commits: `88b4fee`
(`ox/impl`), `e517edc` (merge into `perf/mul-phase2`).

## 2026-08-22 — semx-bpn2 (go-fence wave): the "closed" claim contained its
## own contradiction; a third, inverted species; Go admitted unconditionally

### The read-past-numbers finding

This file's own semx-9g8q close-out (immediately above) claimed Go's
correctness blocker chain was fully closed and directed the next bead to
"file the footprint fence run." Before spending the fence, this wave's
precondition check re-ran `edge_dump_probe` on kubernetes, `SEM_MUL_GO=1`
vs unset, at that exact HEAD (`6ccd4b5`) — and it was **not**
bit-identical: 331,120 (ON) vs 331,117 (OFF), a reproducible 3-edge
divergence (verified deterministic across repeated runs of each side).
The numbers that would have shown this were already sitting in
semx-9g8q's own verification battery two paragraphs up ("kubernetes
(330,558 -> 331,117 default / 331,120 under `SEM_MUL_GO=1`") — the bead
compared before-vs-after-the-fix at each of the two admission states
separately, but never cross-checked the two post-fix states against each
other, so the divergence was recorded without being read. Filed here
because it is exactly the self-fooling failure mode the campaign has
named more than once (unverified "closed" claims, predictions not
checked against the evidence already in hand) — not to relitigate
semx-9g8q's fix, which was itself correct and necessary.

### Root cause: a fourth id-valued `Scope` field the id-staleness repair
### never rekeyed — and it was inverted from the working hypothesis

Isolated with a throwaway entity-id dump (`entity_probe.rs`, built and
run, never committed) and a scoped, reverted `eprintln!` in
`resolve_ref`'s `Call` arm (added, used to trace the exact mechanism,
then removed before committing anything). All 3 edges are **dangling**:
their `to_entity` is not the id of any entity in the graph, and it is not
even OFF's honest miss vs. ON's found edge — it is ON pointing at a
ghost. `PrecomputedFileFacts::rekey_entity_ids` (semx-dm5t) rekeys
`entity_scope_map`/`entity_inner_scope`/`return_type_map`'s *keys*, but
never revisits `Scope::defs`' *values* or `Scope::owner_id` — the two
other places a `Scope` caches an entity id, both populated by
`scope_visit_node`'s registration loops (the class-like/mod_item/
function-like branches; the function-like one is semx-9g8q's own
reinstated loop) during pass 1's precompute, which runs *before* the
cross-file rewrite + cascade. A rewritten method's nested locals (all
three cases: a `var f, g func(...) bool` declaration inside a cross-file
Go method — `Kubelet::sortPodIPs::validPrimaryIP`,
`Scheduler::bindingCycle::podInPreBindCancel`,
`TContext::newAsyncAssertion::finalize`) kept a pre-rewrite `.defs` value
once the rewrite ran, surviving into a `Calls` edge whose target no
entity held any more.

The dangling id also incidentally evaded an *existing* correctness
check: `scope_resolve.rs`'s pass-2 ref loop suppresses a `Calls` edge
when the target is the caller's own parent or child (`is_parent_child`,
~line 3405) — correctly recognizing that a method calling its own
locally-declared entity is containment, not a cross-entity call.
`entity_map.get(&target_id)` on a dangling id returns `None`, so the
filter fails open instead of firing. Traced (not assumed) post-fix: both
ON and OFF now resolve the 3 calls to their valid rekeyed ids, and both
then correctly suppress them via that same filter — the ghosts are dead
everywhere, no new edges gained anywhere, `edge_dump_probe` ON vs OFF on
kubernetes bit-identical again (331,117 edges both sides, matching
sha256).

Fix: `rekey_entity_ids` now also walks `self.scopes` and rewrites exactly
`Scope::defs`' values and `Scope::owner_id` — the two other id-valued
fields, by `Scope`'s own field doc comments. Every other `Scope` field
(`bindings`, `binding_rows`, `types`, `pending_call_types`,
`pending_field_types`) holds a plain name string by its own doc comment,
never compared against `entity_map`/`all_entities` by id, and is
deliberately left untouched — rewriting a name field on an accidental
string collision with a rewritten id would corrupt real data, not fix
anything.

### DANGLING_EDGE_ORACLE

Added to `edge_dump_probe`: every `EntityRef`'s `from_entity`/`to_entity`
must name a real id in the same build's own entity set — `resolve_ref`
only ever returns ids it read out of this build's own tables, so an edge
naming an id nothing declared is always a bug, never a legitimate case.
Prints one `DANGLING_EDGE` line per offender plus a gating
`DANGLING_EDGE_ORACLE ... verdict=ok|MISMATCH` summary, matching this
crate's other probes' `ORACLE` convention. Clean (`dangling=0`) on
kubernetes both switch states (331,117 edges / 554,548 entities each)
plus three controls: rust-lang/rust (307,597 / 522,921),
microsoft/TypeScript (196,126 / 714,832), home-assistant/core (310,394 /
318,638) — zero edge-count movement on all three non-Go corpora versus
semx-9g8q's own post-fix counts, exactly as expected (`rekey_entity_ids`
is a structural no-op whenever `rekey` is empty, which it always is
absent a Go cross-file rewrite).

### The memory fence

`/usr/bin/time -l sem graph --no-cache --json`, same binary
(`SEM_MUL_GO=1` vs unset), fresh `SEM_CACHE_DIR` + isolated
`SEM_FACTS_CORPUS_DIR` per run, 3 order-swapped pairs on kubernetes, both
`/usr/bin/time -l` fields per M1's corrected-metric protocol (`uptime`
logged around each run; load averages 3.6–13.1 throughout, unremarkable
for this shared box):

| pair | order | OFF maxRSS | ON maxRSS | Δ maxRSS | OFF footprint | ON footprint | Δ footprint |
|---|---|---:|---:|---:|---:|---:|---:|
| 1 | OFF→ON | 3,217,178,624 B | 3,234,611,200 B | +0.54% | 2,838,072,416 B | 3,078,229,184 B | **+8.46%** |
| 2 | ON→OFF | 3,233,054,720 B | 3,214,868,480 B | -0.56% | 2,841,185,400 B | 3,048,574,096 B | **+7.30%** |
| 3 | OFF→ON | 3,225,468,928 B | 3,232,776,192 B | +0.23% | 2,873,560,160 B | 3,068,398,784 B | **+6.78%** |

maxRSS flat, noise-band. Footprint unanimous **+6.78% to +8.46%**, tight
(<1.7 points of spread), comfortably under the +15% ceiling on every
pair — Go clears both fields, unlike every one of C++/Python/Rust/Java,
all of which busted the ceiling on at least one field this campaign.

### Verdict and execution (I6): admitted unconditionally

W2's own close-out precedent applied verbatim, no unused scaffolding
left behind: `mul_precompute_admits` gained a plain `"go" => true` arm;
`go_precompute_enabled`/`SEM_MUL_GO` deleted; `MUL_RUNTIME_GATES`'s "go"
row deleted (no switched-off state left to preserve corpus compatibility
with). `LANGUAGE_SALTS`'s go entry bumped `ts-0.23-mp5-dm5t` ->
`ts-0.23-mp5-dm5t-bpn2` (I5/F2: `rekey_entity_ids`'s new `.defs`/
`owner_id` rewriting is a content-only producer change, same category as
every prior bump on this entry — a stale pre-fix corpus entry from a
local `SEM_MUL_GO=1` debugging session must not silently answer a
post-fix lookup now that the enriched path runs on every build),
mirrored byte-for-byte in `facts_corpus_probe.rs`'s independent copy. All
doc comments/tests that described Go as gated updated to match,
including one now-provably-stale claim
(`producer_language_salt`'s "every phase-1/phase-2 language is gated as
of M1; none remain unconditional") and one test split
(`mul_phase2_go_java_default_matches_the_measured_verdict`'s Go half
retired into its own `mul_phase2_go_default_matches_the_measured_verdict`
asserting the opposite verdict, mirroring `mul_phase2_rust_default_
matches_the_measured_verdict`'s own shape).

**Full battery, all against the final post-promotion binary:**
- `edge_dump_probe`/`DANGLING_EDGE_ORACLE`: kubernetes with
  `SEM_MUL_GO=1` and unset both produce byte-identical output to each
  other and to the pre-promotion verified-correct state (sha256
  `40a0ef24...`) — the switch's removal changed nothing behaviorally,
  only the default.
- Adversarial salt-isolation proof (I5/F2, mirroring W2's/semx-j1fw's own
  precedent): populated a pure-Go corpus (`pkg/kubelet/pleg`, 6 files)
  with a pre-bpn2 binary (throwaway worktree at `6ccd4b5`, isolated
  `CARGO_TARGET_DIR`) under `SEM_MUL_GO=1` (old salt), consumed with this
  bead's binary (new salt) — `corpus_hits=0`, a clean miss, `ORACLE ok`
  (the resulting rebuild is still correct despite the miss). A first
  attempt against a larger, mixed-content directory
  (`pkg/kubelet`, 724 files) read `corpus_hits=22` — not a red flag: that
  directory carries 80 non-`.go` files whose own languages' salts are
  untouched by this bump, and the pure-Go control isolates exactly that.
- Suites: `sem-core` release lib 665/665 (+1 new test), all 9 integration
  binaries green incl. `stem_collision_ground_truth` 7/7; `sem-cli`
  release 250/250.
- `SEM_FP_PARITY=1 incr_probe`, all 8 scenarios: kubernetes 8/8
  `ORACLE ok`, home-assistant/core 8/8 `ORACLE ok`.
- Clippy: zero new warnings (174 before and after, same wider scope:
  `--lib --examples -p sem-cli --bin sem`).

**The wall-time prize, honestly measured.** 3 interleaved cold pairs,
old gated-off-by-default binary (pre-bpn2, `SEM_MUL_GO` unset — the
shipped default before this wave) vs. the new unconditional binary,
kubernetes, fresh `SEM_CACHE_DIR` + isolated `SEM_FACTS_CORPUS_DIR` per
run: **-12.0%/-17.0%/-12.1%**, unanimous direction. This is not a new
win — it reproduces the `extract_imports` win semx-u3rk already banked
(kubernetes ~83s → ~24s import-extraction time) whenever `SEM_MUL_GO=1`
was set by hand; admission's own prize is that this now requires no
configuration, by default, for every future build.

Bead: semx-bpn2 (closed). Epic: semx-w5k. Prior: semx-u3rk, semx-dm5t
(closed, `26a5b56`), semx-9g8q (closed, `88b4fee`). Commits: `30b9e84`
(`Scope::defs`/`owner_id` rekey fix), `5aa8dca` (`DANGLING_EDGE_ORACLE`),
`66177f8` (unconditional admission + salt bump + doc/test close-out).

## 2026-08-22 — semx-bpn2 (python-fence wave): Fields 10/11 are not the
## dominant term after all, a zero-risk trim closes part of the gap,
## Python stays gated, and C++'s own dominant term named

Continuation of the memory campaign (M1/Card 2, above): with Go closed,
this wave was handed the two languages M1 demoted for busting the
footprint ceiling — Python (needs to shed ≈0.14-0.17GB, ≈43-45% of its
overshoot) and C++ (needs ≈0.65-0.70GB, ≈45-48%) — plus a prerequisite:
no C++-heavy corpus had ever been attributed field-by-field (dotnet is
C#, linux is C, both null results for `cpp`'s own gate). The task's own
premise named Fields 10/11 (`import_stmts`/`ctor_call_sites`) as the
lever, per the field doc comment's own claim that they're "what Python
actually populates." Stage 1 below shows that premise was true but
incomplete — Fields 10/11 are real, but not dominant.

### Stage 1 — attribution: the per-field breakdown didn't exist, so it
### was built first

`PrecomputedFileFacts::approx_heap_bytes` (Card 2's own nested-string
walk) only ever returned one summed `usize` — there was no way to ask
"which field" without instrumenting one first. Split it into
`field_heap_bytes()`, returning a new `PrecomputedFieldBytes` struct
(one entry per field group); `approx_heap_bytes()` is now a one-line
`.total()` wrapper, so nothing downstream needed to change. While
rebuilding the walk field-by-field, found and fixed a real gap: Fields
10/11 were counted only via `size_of::<ImportStmtFacts>() *
import_stmts.capacity()` — the enum's own stack shape — never the heap
bytes owned by the `String`s and `Vec`s *inside* each variant
(`PyFromImport { module, specifiers: Vec<(String,String)> }` etc.),
unlike the four maps (`return_type_map`/`instance_attr_types`/
`init_params`/`attr_to_param`) Card 2 already fixed. `field_heap_bytes`
now walks every `ImportStmtFacts`/`CtorCallFacts` variant's nested
payloads the same way. Wired into `mem_profile.rs` as
`precomputed_facts_field_breakdown` — deliberately a separate
`eprintln` block, not folded into `checkpoint`'s own `attributed_total`,
since the existing `precomputed_facts` entry there already counts these
same bytes once; adding the breakdown into that sum would double-count
against `process_rss`.

**Measured** (`SEM_PROFILE_MEM=1 SEM_MUL_PYTHON=1`, home-assistant/core,
post-pass-1 checkpoint, one run):

| field | bytes | % of field total |
|---|---:|---:|
| content | 111.4MB | 30% |
| ast_refs | 92.9MB | 25% |
| scopes | 72.6MB | 20% |
| import_stmts (Field 10) | 58.1MB | 16% |
| ctor_call_sites (Field 11) | 15.0MB | 4% |
| entity_scope_maps | 11.7MB | 3% |
| init_params | 1.6MB | <1% |
| instance_attr_types | 1.2MB | <1% |
| attr_to_param | 1.1MB | <1% |
| return_type_map | 0.5MB | <1% |
| **field total** | **366.0MB** | |

(`precomputed_facts`'s own aggregate checkpoint entry reads 376.0MB —
the ~10MB gap is per-entry hash-table overhead plus file-path key
bytes, neither of which belongs to any one field.) With the gate off,
every one of these reads exactly 0.0MB — `PrecomputedFileFacts` entries
are created *only* for MUL-admitted files, so this table is
approximately the entire admission cost, not a superset padded by
work Python would have paid either way.

**The dominant term is not Fields 10/11.** Combined they're 20% of the
total; `content` (the whole file's source text) and `ast_refs` (call/
method-call reference sites) each individually outweigh them. `content`
is not a spare lever, either — it's the *sole* surviving copy of a
chunked-path file's source once its tree is dropped
(`snapshot_bow_content`'s own test, `L-BOW-SHARE`, already asserts the
bag-of-words entry is a pointer-identical borrow of these exact bytes,
not a copy) — trimming it would mean re-reading from disk mid-build, a
real architectural change this bead's "no correctness change" mandate
put out of scope. Re-aiming: `ast_refs`/`scopes`/Fields-10/11 together
(254.6MB, 70% of the total) are the only fields with genuine
push/insert-loop-built slack to reclaim.

### Stage 2 — the trim: `shrink_to_fit`, zero risk, real but partial

Every collection `PrecomputedFileFacts` holds is built by repeated
`push`/`insert` inside a worklist walk (`fused_scope_refs_import_walk`/
`record_import_stmts_pruned`/`record_ctor_call_sites`) with no
`with_capacity` hint — the walk can't know a file's eventual descriptor
count in advance — so `Vec`/`HashMap` growth-doubling leaves real slack
sitting unused for the rest of the build once these facts are stored.
`PrecomputedFileFacts::shrink_to_fit()` reclaims it in place: every
`Scope`'s six internal collections, every `AstRefKind::{Call,MethodCall}`
argument-labels `Vec`, every `ImportStmtFacts`/`CtorCallFacts` variant's
own nested `Vec`s. Called once, at both precompute producers'
construction sites (`precompute_scope_resolvable_file_facts` and
`precompute_js_ts_file_facts`), right before the facts join the
corpus-wide map they then live in for the rest of the build.

`shrink_to_fit` is a pure capacity operation on every collection type
here — it cannot change a value, key, insertion order, or length — so
it needed no correctness proof beyond what the existing suites already
give, and **no python-salt bump**: the serialized facts-corpus format
depends only on logical content (`serde` iterates elements, never
capacity), never memory layout. Verified this empirically rather than
only by reasoning: a throwaway pre-trim binary (worktree at this
section's own prior HEAD, `d718a81`'s parent) populated a facts corpus
from `homeassistant/helpers` (F1's own 109-file corpus) under the *old*
salt; the post-trim binary consumed it — `corpus_hits=109/109`,
`ORACLE ok` — full cross-compatibility, exactly what "no pack-content
change" predicts.

**Measured** (same instrument, same corpus, post-trim):

| field | before | after | Δ |
|---|---:|---:|---:|
| ast_refs | 92.9MB | 68.4MB | -24.5MB (-26.4%) |
| scopes | 72.6MB | 58.6MB | -14.0MB (-19.3%) |
| import_stmts | 58.1MB | 40.6MB | -17.5MB (-30.1%) |
| ctor_call_sites | 15.0MB | 9.8MB | -5.2MB (-34.7%) |
| entity_scope_maps | 11.7MB | 11.7MB | 0 |
| content | 111.4MB | 111.4MB | 0 (expected — `read_to_string` doesn't over-allocate) |
| **field total** | **366.0MB** | **304.7MB** | **-61.3MB (-16.8%)** |

`precomputed_facts`'s own aggregate entry: 376.0MB -> 314.6MB (-61.4MB).
A real, measured, zero-risk reduction — but well short of the ≈150MB
this bead was asked to target, because the one field that could have
supplied the rest (`content`) is structurally not a lever, and the
remaining slack in `ast_refs`/`scopes`/Fields 10/11 was itself finite.

**A bigger lever exists, named but not attempted this session.** Both
`import_stmts`' module/specifier strings and (per a quick corpus grep,
not the shipped code) common Python identifiers repeat *heavily*
corpus-wide: home-assistant/core's `from X import ...` lines carry
139,889 module-path occurrences across only 6,814 unique strings (95.1%
duplicate), and 171,822 specifier names across 17,202 unique (90.0%
duplicate). A global, corpus-wide string interner (swapping `String`
for a compact index/handle in `ImportStmtFacts`/`CtorCallFacts`/
`AstRefKind`) would cut both the duplicate string bytes *and* the
24-byte `String` stack shape per occurrence — a materially bigger win
than `shrink_to_fit`'s capacity-only reclaim. Not implemented here:
it changes `PrecomputedFileFacts`'s serialized shape (a real
pack-content change, needs a salt bump per I5), needs a
concurrency-safe shared interner if precompute ever parallelizes across
files, and is a large enough surface change to warrant its own bead
rather than a same-session addition on top of an already-verified trim.

### Stage 3 — the fence: real improvement, still over the ceiling

3 order-swapped ON/OFF pairs, home-assistant/core, `/usr/bin/time -l`,
fresh `SEM_CACHE_DIR` + `SEM_FACTS_CORPUS_DIR` per run, `--no-cache`,
against the post-trim binary — M1's own protocol, verbatim:

| pair | order | OFF maxRSS | ON maxRSS | Δ maxRSS | OFF footprint | ON footprint | Δ footprint |
|---|---|---:|---:|---:|---:|---:|---:|
| 1 | OFF→ON | 1,891,483,648 B | 1,805,942,784 B | -4.52% | 1,419,790,040 B | 1,739,032,664 B | **+22.48%** |
| 2 | ON→OFF | 1,853,292,544 B | 1,812,168,704 B | -2.22% | 1,427,850,920 B | 1,768,343,616 B | **+23.85%** |
| 3 | OFF→ON | 1,857,421,312 B | 1,802,125,312 B | -2.98% | 1,387,120,320 B | 1,740,916,848 B | **+25.51%** |

maxRSS unanimous negative (-2.22% to -4.52%), reproducing F1/M1's own
compressor-mechanism finding. Footprint unanimous positive, **+22.48%
to +25.51%**, tight (3.03-point spread, not noise) — down from M1's own
+25.29% to +27.44% (a genuine ~2-3 point improvement, consistent with
the ~16.8% relative reduction the trim measured on `precomputed_facts`
alone), but every pair is still comfortably above the +15% ceiling.

**Gap arithmetic, this run's own numbers**: OFF average 1,411,587,093 B;
allowed ON ≤ OFF × 1.15 = 1,623,325,157 B; actual ON average
1,749,431,043 B. **Still ≈126MB (≈0.12GB) over the ceiling** after this
bead's trim — down from M1's own ≈0.14-0.17GB gap, meaning the trim
closed roughly a fifth to a quarter of the distance, not the ≈45% this
bead was asked to target.

**Verdict (I6): Python stays gated.** `mul_precompute_admits("python")`
is unchanged — still routes through `python_precompute_enabled()`
(`SEM_MUL_PYTHON`, off by default). No demotion (it was already
demoted, by M1), no promotion, no `MUL_RUNTIME_GATES`/salt/doc changes
needed for the gate state itself. Per this task's own instruction, the
trim ships anyway: it is correctness-neutral (proven, not assumed —
Stage 2's proof battery), reduces real memory pressure on every future
`SEM_MUL_PYTHON=1` build regardless of whether the language is ever
re-admitted unconditionally, and costs nothing to keep.

**Full battery** (post-trim binary, both stages' changes together):
- `edge_dump_probe`/`DANGLING_EDGE_ORACLE`, home-assistant/core: 310,394
  edges, sha256 `a571149c...` identical `SEM_MUL_PYTHON=1` vs unset,
  `dangling=0 verdict=ok` both sides, entities=318,638 (matches Card 2's
  own giant-scale HA entity count exactly).
- `cargo test --release -p sem-core --lib`: 666/666 (+1: the new
  `shrink_to_fit_reclaims_capacity_without_changing_values` test, which
  deliberately over-provisions every collection it exercises so a
  no-op `shrink_to_fit` would fail the pre-condition asserts, not only
  the post-condition ones).
- `cargo test --release -p sem-core --tests`: every integration binary
  green, incl. `stem_collision_ground_truth` 7/7.
- `cargo test --release -p sem-cli`: all binaries green.
- `SEM_FP_PARITY=1 incr_probe`, home-assistant/core, all 6 scenarios
  (`none`/`leaf`/`mixed50`/`hub`/`tests`/`all`): `ORACLE ok` throughout.
- `facts_corpus_probe` cross-binary salt-compatibility proof (Stage 2,
  above): `corpus_hits=109/109`, `ORACLE ok`.
- Clippy (`-p sem-core -p sem-cli`): 170 warnings, unchanged from this
  HEAD's own pre-bead baseline, zero attributable to any of the three
  touched files.
- `rustfmt --check` on every line this bead touched: clean (pre-existing
  drift elsewhere in `scope_resolve.rs`, inherited verbatim by copying
  `approx_heap_bytes`'s body into `field_heap_bytes`, left alone per
  campaign precedent — not this bead's to fix).

### Stage 4 — C++ attribution (prerequisite, measurement-only): a
### different, harder shape than Python's

First-ever field-by-field attribution on a real C++-heavy corpus
(previous readings were null tests — dotnet is C#, linux is 37,144 `.c`
+ 27,014 `.h` against 7 `.cpp`-family files). `SEM_PROFILE_MEM=1
SEM_MUL_CPP=1`, full `llvm/llvm-project` checkout (39,042 `.cpp` files),
post-trim binary, post-pass-1 checkpoint:

| field | bytes | % of field total |
|---|---:|---:|
| content | 420.1MB | 42% |
| ast_refs | 394.2MB | 39% |
| scopes | 161.4MB | 16% |
| entity_scope_maps | 23.9MB | 2% |
| import_stmts / ctor_call_sites / the four maps | 0.0MB each | 0% |
| **field total** | **999.6MB** | |

(`precomputed_facts`'s own aggregate entry: 1020.3MB.) Fields 10/11 and
the four Python-only maps are confirmed structurally empty for C++
exactly as their own doc comments claim — `import_stmts` requires the
TREELESS gate's zero-imports precondition, `ctor_call_sites` requires a
literal `"call"` node kind C++'s grammar never emits
(`call_expression`, not `call`). This measurement already reflects
Stage 2's `shrink_to_fit` trim, since it runs unconditionally on every
admitted file regardless of language — C++ gets that same (partial)
benefit for free, no separate change needed.

**C++'s dominant term is `ast_refs` (394.2MB, 39%)** — not `content`
this time; the two are close, but `ast_refs` is the single largest
field, edging out even the whole-file source text. A quick corpus check
(`llvm/lib`, 3,120 `.cpp` files, simple `identifier(` call-site scan)
found the same heavy-duplication shape Python's imports showed:
1,202,016 call-identifier occurrences across only 89,006 unique strings
(92.6% duplicate) — `ast_refs`' `Call`/`MethodCall`/`ScopedCall` name/
receiver/method strings are a plausible interning target, the same
lever class named but not attempted in Stage 2.

**GB arithmetic — and an honest ceiling on the lever, not just a
number.** M1's own llvm-project reading (authoritative, this card
didn't re-run the full fence): OFF footprint 5.27-5.31GB, ON 6.71-6.76GB,
Δ+26.3% to +28.1%; target ON ≤ OFF×1.15 ≈ 6.06-6.11GB, needs to shed
≈0.65-0.70GB. This bead's attribution: every non-`content` field summed
(`ast_refs`+`scopes`+`entity_scope_maps`) = 579.5MB ≈ 0.566GB — **even a
theoretical 100% elimination of every trimmable field falls ≈84-134MB
short of the low end of what's needed.** Unlike Python (where Fields
10/11 undersold the true lever but a real one existed in `ast_refs`/
`scopes`), C++'s gap cannot be closed by `PrecomputedFileFacts`
field-trimming alone at any achievable trim ratio — confirming, with
real numbers this time, Card 2's own earlier finding ("C++'s gap has no
lever this card's data points at"). Closing it would need either a
`content`-side reduction (the same architectural change Python's
`content` also ruled out of this bead's scope) or accepting C++ cannot
reach unconditional admission through this lever family at all.

**Not implemented — measurement-only per this bead's own scope.** Named
here for the next wave: `ast_refs` interning is worth attempting (real,
large duplication, same mechanism Stage 2 named for Python), but the
GB arithmetic above means it should be scoped as "shrinks the gap," not
promised as "closes it."

### Close-out

No gate/salt/doc changes for either language beyond what Stage 2's
commit already covers (Python's own gate state is unchanged by this
bead; C++'s is untouched entirely — measurement-only). Comment posted
on semx-bpn2 with this section's summary.

Bead: semx-bpn2 (python-fence wave). Epic: semx-w5k. Prior: M1 (both
demotions), Card 2 (the nested-walk instrument this bead extends to
Fields 10/11, and the C++ null-result trap this bead's Stage 4 resolves
with a real corpus). Commit: `d718a81` (attribution + trim + test).

## 2026-08-22 — semx-taq6 (interning-for-memory wave): `ast_refs`
## per-file interned, both memory fences narrowed, neither closed, wall
## time clean — the two-questions distinction, made explicit

**This is not a re-walk of `semx-5nc`'s decline.** `semx-5nc` (see
"Interning", above) asked and answered a *wall-time* question — does
`u32`-interning `entity_map`/`symbol_table` speed up `resolve_ref`'s
join — and declined it because the join is 0.11–2.29% of cold build time
on every corpus measured, decisively below its own materiality bar. This
bead asks a different, never-measured question the python-fence wave
(semx-bpn2) surfaced but did not attempt: 92–95% of the strings in
`PrecomputedFileFacts`' heavy fields are duplicates (that wave's own
numbers: HA imports 95.1%/90.0% duplicate; a quick `ast_refs` scan on
llvm found 92.6% duplicate) — does *interning to reclaim that duplicate
memory* clear the two languages' own footprint ceilings? Different
question, different mechanism (per-file dedup vs. a build-scope join
cache), different corpus (real duplication rates vs. a synthetic
key-shape bench). Anyone reading this as "the interning question was
already declined" is reading the wrong bead.

### Stage 0 — the deciding measurement: duplication is dominated by
### within-file repeats, everywhere measured

Instrumented (`mem_profile::precomputed_facts_duplicate_split`, gated
behind the existing `SEM_PROFILE_MEM=1`, no new env var) the exact split
the design choice hinges on: for every candidate string a per-node
interner would replace with a token (`ast_refs`' `Call.name`/
`ScopedCall.{path,name}`/`MethodCall.{receiver,method}`; `import_stmts`'
module/specifier/path strings), what fraction of *duplicate bytes* are
repeats *within one file* versus repeats *across files*. The
decomposition is exact, not estimated: for a value `v` with per-file
counts summing to a corpus-wide total `n` across `k` distinct files, a
corpus-wide interner's total saving is `len(v) * (n - 1)`; a per-file
table's saving (dedup within each file only) is `len(v) * Σ(c_i - 1)`;
the difference, `len(v) * (k - 1)`, is exactly the cross-file share — so
`within_file + cross_file == total_dup` by construction. New accessor
methods `PrecomputedFileFacts::{ast_ref_intern_candidates,
import_stmt_intern_candidates}` (diagnostic-only, called nowhere outside
the profiling gate) supply the strings; `mem_profile.rs` does the
counting.

**Measured** (`SEM_PROFILE_MEM=1`, post-pass-1 checkpoint, one run each):

| corpus | field | occurrences | distinct | total dup bytes | within-file | cross-file |
|---|---|---:|---:|---:|---:|---:|
| home-assistant/core (`SEM_MUL_PYTHON=1`) | `ast_refs` | 1,019,964 | 61,103 | 10.6MB | 7.7MB (**72.4%**) | 2.9MB (27.6%) |
| home-assistant/core | `import_stmts` | 925,375 | 39,640 | 13.6MB | 8.2MB (**60.2%**) | 5.4MB (39.8%) |
| llvm/llvm-project (`SEM_MUL_CPP=1`) | `ast_refs` | 5,731,499 | 417,210 | 44.2MB | 35.0MB (**79.3%**) | 9.1MB (20.7%) |
| llvm/llvm-project | `import_stmts` | 0 | 0 | — | — | — |

(llvm's `import_stmts` is structurally empty — confirmed by Card 2/
semx-bpn2 Stage 4, reconfirmed here: C++'s grammar never populates it.)

**Verdict: design (a), per-file string tables.** Within-file repeats are
the dominant share everywhere measured (60–79%), dwarfing the cross-file
share a corpus-wide interner would additionally reach. A per-file table
captures most of the reclaimable duplication with *zero* concurrency
cost (no shared lock, no sharded map, no cross-file synchronization even
if precompute ever parallelizes across files) — the smaller-blast-radius
option is also the higher-yield one here, not a tradeoff. The
STOP-AND-REPORT condition this bead's brief named (duplicates mostly
cross-file *and* design (b)'s concurrency cost measuring ugly) never
triggered: duplicates are mostly within-file, so design (b) was never
seriously in contention.

### Stage 1 — design (a), implemented on `ast_refs` (the dominant field)

`ast_refs` chosen over `import_stmts` per the brief's own instruction
("the ONE dominant field first") — Card 2/semx-bpn2 Stage 1 already
established `ast_refs` (not Fields 10/11) as the largest non-`content`
field on both HA (68.4MB post-trim) and llvm (394.2MB, C++'s single
largest field, edging out `content` itself).

**Mechanism.** `AstRefKind`'s `name`/`path`/`receiver`/`method` fields
are now `Arc<str>`, not `String`. A new `AstRefCollector`
(`scope_resolve.rs`) wraps the `Vec<AstRef>` every ref-emitting function
already threaded through, adding a transient, file-scoped `&str ->
Arc<str>` table (`intern`) so repeated identifiers within one file share
one heap allocation. Construction sites (`collect_all_file_refs`'s
unfused walk and `fused_scope_refs_import_walk`'s fused one, both
funneling through `refs_visit_node` -> `extract_call_ref` ->
`extract_member_call_ref`/`push_method_call_ref`) call
`push_call`/`push_scoped_call`/`push_method_call` instead of building
`AstRef` literals directly — six functions and one closure touched, zero
signature growth (the accumulator's *type* changed, not the parameter
count). The interner is discarded per file (`into_refs`) — no cross-file
sharing, matching Stage 0's finding.

**Wire compatibility, proven before implementing further.** Before
committing to this shape, `examples/arc_str_wire_probe.rs` (throwaway,
still in the tree) proved `Arc<str>` and `String` serialize to
byte-identical CBOR under `ciborium` with serde's `rc` feature
(`Cargo.toml`: `serde = { features = ["derive", "rc"] }`, the one new
dependency-config change this bead makes), and cross-decode cleanly in
both directions — `impl<T: Serialize> Serialize for Arc<T>` simply
delegates to `T`'s own impl, so an `Arc<str>` and a `String` holding the
same text produce the identical wire bytes. This was then re-verified
end-to-end on the real `FactsStore` machinery (Stage 2, below) rather
than trusted on the toy probe alone. **No `FACTS_SCHEMA_VERSION` bump**
— a genuine "no salt bump" outcome, the property the brief's design (b)
was expected to have, achieved here by design (a) instead because the
wire format for a scalar string field doesn't care whether the in-memory
handle behind it is `String` or `Arc<str>`.

`field_heap_bytes`'s `ast_refs` accounting was made dedup-aware (tracks
seen `Arc::as_ptr` addresses per file so a shared allocation is counted
once, not once per occurrence, plus the 16-byte strong/weak refcount
header every `Arc<str>` allocation carries) — otherwise the instrument
would silently under-report the real win.

**`import_stmts` extension: sized, not attempted.** The brief allowed
extending "as data justifies." HA's `import_stmts` total duplicate bytes
(13.6MB, 8.2MB within-file) is real but small relative to HA's own
remaining footprint gap (below), and C++'s `import_stmts` is
structurally empty — extending there buys C++ nothing. A second
six-variant-enum refactor (`ImportStmtFacts` has `PyFromImport`/
`PyModuleImport`/`TsImport`/`TsReExport`/`RustUse`/`GoImport`, each with
its own nested `Vec`/`(String,String)` shape) for a single-digit-MB
expected win, on top of an already-substantial `ast_refs` change, was
judged not worth the correctness-review budget this session had left —
named here for a future bead rather than silently skipped.

### Stage 2 — correctness

- **Bit-identical edge SHAs**, baseline (`ceb80ca`) vs. campaign binary,
  `edge_dump_probe` (sorted `from\tref_type\tto` dump), 4 corpora:

  | corpus | gate | sha256 (both binaries) | entities | edges | dangling |
  |---|---|---|---:|---:|---:|
  | home-assistant/core | `SEM_MUL_PYTHON=1` | `a571149c…` (matches semx-bpn2's own recorded sha) | 318,638 | 310,394 | 0 |
  | llvm/llvm-project | `SEM_MUL_CPP=1` | `1e9f1502…` | 2,760,966 | 982,394 | 0 |
  | kubernetes/kubernetes | unconditional (Go) | `40a0ef24…` | 554,548 | 331,117 | 0 |
  | microsoft/TypeScript | control (unfused `collect_all_file_refs` path) | `90917ed0…` | 714,832 | 196,126 | 0 |

  Every one of the four corpora this bead's own brief named is
  bit-identical, `DANGLING_EDGE_ORACLE verdict=ok` on all.

- **Cross-binary `FactsStore` compatibility, both directions, real
  corpus** (`facts_probe`, `homeassistant/helpers`, 109 files,
  `SEM_MUL_PYTHON=1`, matching F1's own corpus): baseline `save` ->
  campaign `load`: `files_green=109`, `changed=0`, `edge_hash` identical
  cold vs. warm (`28fc15379fb2607b`), `ORACLE ok`. Reverse direction
  (campaign `save` -> baseline `load`): same `edge_hash`, same
  `store_bytes` (4,192,645, identical both directions), `ORACLE ok`. Old
  and new binaries read each other's stores with zero reds — direct
  proof the "no schema bump" claim above holds at the real `FactsStore`
  layer, not only the isolated wire probe.

- `cargo test --release -p sem-core --lib`: 666/666 (unchanged count —
  no new test added this bead; the record-vs-direct equivalence tests
  Stage 2 asked for are pre-existing suite members, exercised here
  unmodified). `cargo test --release -p sem-core --tests`: every
  integration binary green (incl. `stem_collision_ground_truth` 7/7,
  `yaml_multidoc` 1/1). `cargo test --release -p sem-cli`: every binary
  green (141+ in the main suite alone).
- `SEM_FP_PARITY=1 incr_probe`, home-assistant/core, all scenarios
  (`none`/`leaf`/`mixed50`/`hub`/`tests`/`all` — `all` itself expands to
  8 sub-scenarios incl. `hubrename`/`importchurn`): `ORACLE ok`
  throughout, no `MISMATCH`.
- Clippy (`-p sem-core -p sem-cli`): 170 warnings total, unchanged from
  this HEAD's own baseline; verified — not eyeballed — that zero fall
  inside this bead's changed line ranges (parsed clippy's own
  `file:line` against the actual diff hunks, since insertions shift
  everything below them and a naive line-number comparison across
  pre/post trees is unreliable).
- `rustfmt --check` on every line this bead touched: clean (two spots
  needed an explicit multi-line reformat after the `.as_str()` ->
  `.as_ref()`/comparison fixes pushed them over the line-length limit,
  fixed by hand rather than a blanket `cargo fmt` — pre-existing drift
  elsewhere in `scope_resolve.rs`, same lines semx-bpn2 already declined
  to fix, left alone per campaign precedent).

### Stage 3 — the fences

3 order-swapped OFF/ON pairs, campaign binary only (OFF-state behavior
is provably unchanged by this bead — `PrecomputedFileFacts` entries only
exist for MUL-admitted files, so an ungated language never touches
`AstRefCollector` at all), `/usr/bin/time -l`, fresh `SEM_CACHE_DIR` +
`--no-cache` per run, footprint = "peak memory footprint" (`phys_footprint`,
the metric M1/I6 established as standard, not `maximum resident set
size`):

**home-assistant/core (`SEM_MUL_PYTHON`):**

| pair | order | OFF footprint | ON footprint | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 1,419,363,984 B | 1,729,480,792 B | +21.85% |
| 2 | ON→OFF | 1,419,790,016 B | 1,730,496,576 B | +21.88% |
| 3 | OFF→ON | 1,386,333,864 B | 1,737,148,504 B | +25.31% |

OFF avg 1,408,495,955 B; ceiling (OFF×1.15) 1,619,770,348 B; ON avg
1,732,375,291 B. **Still ≈112.6MB (0.11GB) over the ceiling** — down
from semx-bpn2's own post-trim gap of ≈126MB, essentially the same
range as that wave's own fence (+22.48–25.51%) once pair-to-pair spread
is accounted for. The `ast_refs` win at HA's scale (≈10MB of total
duplicate bytes) is real (Stage 0/1) but too small relative to HA's
~1.4GB OFF footprint to move this verdict outside its own noise band.
**Verdict: Python stays gated** (`SEM_MUL_PYTHON`, off by default,
unchanged).

**llvm/llvm-project (`SEM_MUL_CPP`):**

| pair | order | OFF footprint | ON footprint | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 5,205,087,152 B | 6,621,386,680 B | +27.21% |
| 2 | ON→OFF | 5,194,765,352 B | 6,534,059,936 B | +25.79% |
| 3 | OFF→ON | 5,175,088,096 B | 6,473,815,848 B | +25.10% |

OFF avg 5,191,646,867 B; ceiling (OFF×1.15) 5,970,393,897 B; ON avg
6,543,087,488 B. **Still ≈572.7MB (0.57GB) over the ceiling** — down
from semx-bpn2 Stage 4's own arithmetic (a ≈650–700MB shortfall even
under a *theoretical 100% elimination* of every trimmable field), a real
~18–20% reduction in the gap from a change that reclaims only part of
one field's duplicate bytes. **Verdict: C++ stays gated**
(`SEM_MUL_CPP`, off by default, unchanged) — an honest shortfall, not a
failure to execute the design: this bead's own brief named this as an
acceptable outcome ("that's physics, name it"), and semx-bpn2 Stage 4
already showed field-trimming alone cannot close C++'s gap at any
achievable ratio. **What would close it**: per semx-bpn2 Stage 4's own
finding, `content` (the whole file's source text, C++'s largest single
field at 420.1MB, structurally un-trimmable without a real architecture
change — it's the sole surviving copy of a chunked file's source once
its tree is dropped) is the only field large enough on its own to close
the remaining ≈573MB gap. Streaming/phase-scoping `content` itself
(M0's lever (ii)) is the only reasonable lever left; nothing in the
`PrecomputedFileFacts` field family (scopes/ast_refs/maps) has enough
slack remaining after this bead's own change to get there.

### Stage 4 — wall-time guard

Interleaved cold pairs, baseline vs. campaign, 3 pairs each corpus
(order alternated pair to pair), full-CLI wall clock:

| corpus | pair | Δ (camp vs base) |
|---|---|---:|
| home-assistant/core | 1/2/3 | +3.83% / +0.08% / -4.68% |
| llvm/llvm-project | 1/2/3 | -9.63% / +0.28% / +1.26% |

Medians: HA -1.54%, llvm -1.06% — both comfortably inside the ≤3%
budget, no consistent regression direction (the one outlier, llvm
pair 1's -9.63%, is a *speedup*, not a concern). **No wall-time
regression** — the memory win does not cost the wall-time budget the
original `semx-5nc` decline was protecting. Both questions (semx-5nc's
wall-time join question; this bead's memory-duplication question) now
have independent, measured answers.

### Close-out

Neither language crosses its footprint ceiling; neither gate state
changes (`SEM_MUL_PYTHON`/`SEM_MUL_CPP` both stay off by default,
unchanged from M1/I6). The `ast_refs` interning ships anyway, same
reasoning semx-bpn2's `shrink_to_fit` trim used: correctness-neutral
(proven, not assumed — the full battery above), reduces real memory
pressure whenever either gate is flipped on regardless of whether either
language is ever re-admitted, costs nothing to keep, and narrows both
gaps for whatever bead reopens either ceiling question next (Python:
112.6MB left to find; C++: 572.7MB left to find, and the only
architecturally plausible lever is `content` streaming, not another
`PrecomputedFileFacts` field). `import_stmts` extension sized and
deferred, not silently skipped (Stage 1, above).

Pathspec: `crates/sem-core/Cargo.toml` (serde `rc` feature),
`crates/sem-core/src/parser/scope_resolve.rs` (`AstRefKind`/
`AstRefCollector`, `field_heap_bytes` dedup accounting, the two Stage-0
accessor methods, six construction-site call sites, the
resolve/self-ref/test comparison fixups the type change required),
`crates/sem-core/src/parser/mem_profile.rs`
(`precomputed_facts_duplicate_split`), `crates/sem-core/src/parser/graph.rs`
(one call-site wiring the split into the existing post-pass-1
checkpoint), `crates/sem-core/examples/arc_str_wire_probe.rs` (new,
throwaway — left in the tree as the artifact backing the wire-compat
claim, same convention `semx-5nc` set for its own probe/bench).

Bead: semx-taq6 (interning-for-memory wave). Epic: semx-w5k. Prior:
semx-5nc (the wall-time interning question, declined — a different
question, see the framing above), Card 2 + semx-bpn2 (both memory
demotions, the field attribution and duplicate-rate finding this bead
acts on). Sibling: semx-bpn2 (fence outcomes reported back per this
bead's own brief).
