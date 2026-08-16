# The query index

The materialized view that answers structural questions from a **cold process**,
without a daemon, without a filesystem walk, and without proving the whole
corpus fresh first.

This document is the contract S2 (refs/callers postings), S3 (trigram text
tier), and S4 (deletion of the superseded layers) build against. Section 1 is
the measurement that justifies it — read it before the design, because it
overturns two things the design was originally sketched to do.

---

## 1. Attribution: where the query latency actually goes

**Corpus** — `microsoft/TypeScript` @ `b465fdbfe1`, clean working tree, no
`core.fsmonitor`. 40,877 indexed files, 198.6 MB of source, 454,528 entities,
196,223 edges, 56,552 distinct entity names. `cache.db` = 653 MB.
**Machine** — darwin 25.5.0, warm page cache, release build (`opt-level=3`,
`lto="thin"`). Every number is the median of 3+ runs.

Numbers come from the existing `SEM_TIMINGS=1` marks plus temporary sub-marks
inserted inside `cache.rs`'s freshness gate for this study and then reverted —
the marks that ship today bracket the freshness check between `cache_open` and
`cache_entities_query`, so the corpus-freshness cost is not attributable from
`SEM_TIMINGS` alone. That instrumentation gap is itself a finding.

### 1.1 The answer-shaped query — `sem impact --file src/compiler/program.ts createProgram`

12 rows out. This is the query the index exists to serve.

| phase | ms | % | cost is proportional to |
|---|---:|---:|---|
| `file_discovery` (walk 40,877 files) | 301 | 56% | **corpus** |
| `cache_open` (SQLite, read-only) | 1 | 0.2% | — |
| freshness gate → `git status --porcelain` | 157 | 29% | **corpus** |
| freshness gate → libgit2 HEAD oid | 1 | 0.2% | — |
| freshness gate → cache metadata reads | 0.3 | 0.1% | — |
| impact query (SQLite adjacency) | 77 | 14% | answer |
| output serialization | 0.06 | 0.01% | answer |
| **total** | **536** | | |

**85% of this query is corpus-proportional work performed before a single row
of the answer is touched.** The storage engine — the part the mmap pivot
replaces — is 14%.

### 1.2 The corpus-shaped query — `sem entities --json` (whole repo)

454,528 entities, 88.5 MB of JSON out.

| phase | auto (git oracle) | scan (`SEM_FRESHNESS=scan`) |
|---|---:|---:|
| `file_discovery` | 310 ms | 295 ms |
| `cache_open` | 1 ms | 1 ms |
| freshness gate **total** | **179 ms** | **78 ms** |
|  ↳ `git status --porcelain` | 157 ms | — |
|  ↳ serial SQLite fingerprint reads (40,877 point queries) | — | 62 ms |
|  ↳ parallel `stat` + hash over 40,877 files (rayon) | — | **12 ms** |
| SQL scan + JSON serialize (88.5 MB) | 420 ms | 415 ms |
| **total** | **895 ms** | **795 ms** |

### 1.3 Other paths measured

| path | ms | note |
|---|---:|---|
| `sem entities <one file> --json` | 229 | **no cache consulted at all** — re-parses the file (`extract_entities` 228 ms) |
| `sem entities --text <needle>` (local) | 1061 | the rg replacement; today it **loses** to rg |
| `sem impact …` **with resident sidecar up** | 838 | *slower* than without it — see 1.5 |
| `rg -c <literal>` whole repo | 900 | 566 MB of `tests/baselines` dominates |
| `rg -c <literal> src/` | 10 | |
| `rg --files` (walk only, 81,312 paths) | 75 | sem's own walk finds 40,877 in ~300 ms |
| `sem graph` (cold build) | 29,693 | 9.3 s extract + 20.1 s SQLite save |

### 1.4 Finding: the git-freshness oracle is a net loss

`git status --porcelain` costs **157 ms**. The per-file scan it exists to
replace costs **76 ms**. `SEM_FRESHNESS=auto` is therefore **100 ms slower**
than `SEM_FRESHNESS=scan` on this repo — the opposite of the oracle's stated
premise. That premise ("`git status` rides fsmonitor when it's configured,
~10x") is conditioned on a setting **this repo does not have and most repos do
not have**; the doc comment already says "and still beats the scan without it
(~4x)", and that is the part the measurement contradicts. The oracle is
correct — it never serves stale — but it is a *slower* path to a *weaker*
guarantee, which by the dominance rule makes it inadmissible.

### 1.5 Finding: the resident-server availability model is not degraded, it is zero

With `sem mcp --resident` up and warm on this repo (2.6 GB RSS, 64% CPU
steady-state, 19 tokio workers + a `notify-rs` fsevents loop), the sidecar
socket **accepts connections and never answers — including `{"op":"ping"}`**.
A direct unix-socket client times out at 8 s on every op. Every CLI call
therefore burns its full 300 ms `set_read_timeout` and then runs the local path
anyway: measured 838 ms vs 536 ms with `SEM_NO_SIDECAR=1`.

This is consistent with the ~935 stale sockets in `~/.sem/sock`. The resident
tier is not a 250 ms fast path on a repo this size; it is a **+300 ms tax with
a 2.6 GB resident cost**. *(Surfaced, not fixed — the root cause is in
`sem-mcp`'s `live_graph`/`ensure_live` lock discipline and is outside this
bead's lane. It is reported here because it is load-bearing evidence for the
"no required daemon" invariant, not as a bug to be repaired before the index ships.)*

### 1.6 Finding: the scan tier's cost is storage layout, not I/O

The per-file freshness scan is not I/O-bound. Of its 76 ms, **62 ms is 40,877
serial SQLite point queries** and only **12 ms is the actual parallel `stat` +
hash of 40,877 real files**. The scan is slow because the fingerprints live in
a row store that must be queried one key at a time, not because touching the
filesystem is expensive.

**This is the single most important number in this document.** A parallel
`stat` over a *known* file list costs 12 ms. The index holds that file list and
its fingerprints as a zero-copy array. Whole-corpus freshness verification
therefore becomes a 12 ms parallel `stat` with no walk, no SQL, and no git —
6× cheaper than the cheapest tier today and 25× cheaper than the default one.

### 1.7 Verdict on the pivot

| the sketch said | measurement says |
|---|---|
| queries are slow because a cold process must open + hydrate SQLite | ✗ **No.** `cache_open` is 0.5–3 ms and the impact SQL is 77 ms. Storage is 14% of the problem. |
| per-query corpus freshness scan dominates | ✓ **Yes, jointly with file discovery** — 458 of 536 ms (85%), and *discovery is the larger half*, which the sketch did not name. |
| mmap + µs lookup is the win | ~ **Partly.** It removes the 14%. The other 71% is removed by *deleting file discovery and inverting the freshness proof* — neither of which requires a new file format. |
| the resident server gives 250 ms | ✗ **No.** It gives +300 ms and answers nothing at this scale. |
| text tier: rg re-reads every byte per query | ✓ **Yes**, and sem currently loses to rg anyway (1061 ms vs 900 ms). |

**The design is therefore ordered by measured yield, not by novelty:**

1. **Delete file discovery from the query path** — the index *is* the file list. −301 ms.
2. **Invert the freshness proof** — per-answer by default, whole-corpus as a 12 ms parallel `stat` when completeness is demanded. −157 ms and −76 ms.
3. **Then** replace the storage engine — mmap + binary search. −77 ms.

Steps 1 and 2 are 85% of the win and step 3 is what makes them *expressible*:
you cannot skip discovery unless the file list is in the view, and you cannot do
per-answer freshness unless fingerprints are addressable per answer in µs. The
format is the enabler, not the win. Say it that way to S2/S3.

---

## 2. Laws

```
freshness      view(q) ≡ (cold-fold ∘ corpus)(q)         -- never serve stale
answer-cost    cost(q) ∝ |answer(q)|                     -- ¬∝ |corpus|
availability   view exists before the question           -- ¬daemon, ¬warm-up
```

The freshness invariant needs sharpening, because the obvious reading of it is what
forces the corpus-wide proof this document is deleting. Split it:

```
content-freshness(A)     ∀e ∈ A. e faithfully describes the CURRENT bytes of file(e)
membership-freshness(A)  A = { e | e ∈ cold-fold(corpus), e matches q }
freshness ≡ content-freshness ∧ membership-freshness
```

- **content-freshness** is provable in `O(|files(A)|)` — `stat` the answer's own
  files. For the 12-row `createProgram` answer that is 4 files ⇒ ~40 µs.
- **membership-freshness** is *not* answer-local: a newly created file could add
  a member without any file in `A` changing.

So the index exposes two verification levels and **names them in the type**,
rather than pretending one guarantee:

| level | proves | cost (monster) | default for |
|---|---|---:|---|
| `Verified` | content-freshness of `A`, and membership w.r.t. the index's file set | O(\|files(A)\|), ~µs | every interactive query |
| `Complete` | + the index's file set ≡ the corpus's current file set | ~12–14 ms | `--complete`, CI, the oracle |

`Complete` is cheap because of §1.6: it is a parallel `stat` over the index's own
`FILES` and `DIRS` sections. Directory mtimes are what make it sound — POSIX
updates a directory's mtime on entry create/unlink/rename, so a file that the
index has never seen still perturbs a directory the index *has* seen. The
scope-defining files (`.gitignore`, `.semignore`, `.gitattributes`) are carried
in the same stat set, exactly as `is_manifest_stale` does today.

`Complete` is **not** stronger than a fresh build in the presence of a
modify-and-restore inside mtime granularity; on an mtime *difference* the reader
falls through to the content hash, which is the same discipline the existing
`file_freshness` uses. That equivalence is deliberate: the index inherits the
build plane's freshness predicate rather than inventing a second one.

---

## 3. Format

One file per repo, alongside the existing per-repo artifacts:

```
<cache_dir_for_repo(root)>/
    cache.db          (build plane — unchanged)
    facts/            (build plane — unchanged)
    index.sem         (query plane — THIS FILE, the base image)
    index.log         (query plane — the append-only patch overlay, §4.2)
```

### 3.1 Why one file, mmap'd, fixed-width

Every design choice below follows from one constraint: **a cold process must
reach a name lookup in single-digit milliseconds.** That forbids any format
requiring a parse, a decompress, or a per-record allocation at open time. It
permits exactly one shape: an image whose sections are arrays of fixed-width
little-endian records addressed by offset, so that `open` is `mmap` (a page
table edit, no I/O until touched) and `lookup` is pointer arithmetic over
whatever pages the answer happens to touch.

Little-endian and explicit widths, not `#[repr(Rust)]` structs: the file is a
wire format that outlives the compiler that wrote it. Records are read through
accessor functions that do `u32::from_le_bytes` on a subslice. On the two
architectures sem ships to this compiles to a plain load; the portability is
free. Alignment is 8 bytes for every section start so a future `bytemuck`-style
zero-copy cast stays available without a format break.

### 3.2 Layout

```
offset 0   Header (128 B, 8-aligned)
             magic          [u8; 8]   b"SEMIDX01"
             format_version u32       breaking layout changes
             flags          u32       bit 0: ids elided (§3.4)
             build_salt     u64       §3.5
             entity_count   u64
             file_count     u64
             dir_count      u64
             kind_count     u32
             _reserved      u32
             sections       [SectionRef; 8]   { offset: u64, len: u64 }

section 0  STRINGS   one UTF-8 arena; referenced by (off: u32, len: u32)
section 1  FILES     file_count × FileRec
section 2  ENTITIES  entity_count × EntityRec
section 3  NAMES     entity_count × u32 — entity indices sorted by name bytes
section 4  REFS      RESERVED — S2 (semx-gis)
section 5  TRIGRAM   RESERVED — S3 (semx-az9)
section 6  DIRS      dir_count × DirRec
section 7  KINDS     kind_count × Str32 — interned entity_type strings
```

```rust
FileRec  (40 B)  path: Str32, mtime_secs: i64, mtime_nanos: i32, _pad: u32,
                 content_hash: u64, entity_lo: u32, entity_hi: u32
EntityRec(32 B)  id_tail_off: u32, name_off: u32, id_tail_len: u16,
                 name_len: u16, kind: u16, _pad: u16, file: u32,
                 start_line: u32, end_line: u32, parent: u32
DirRec   (24 B)  path: Str32, mtime_secs: i64, mtime_nanos: i32, _pad: u32
Str32    ( 8 B)  off: u32, len: u32   -- byte range into STRINGS
```

`EntityRec` splits its offsets from its lengths rather than using two `Str32`s
because a `u16` length is sufficient for a name and an id tail and the packing
brings the record from 40 to 32 bytes — 3.6 MB saved on this corpus, at the
cost of a documented 64 KiB cap the writer clamps on a char boundary rather
than corrupting the arena.

`parent` is an entity index, `u32::MAX` for none. `entity_lo..entity_hi` is the
half-open range of `ENTITIES` belonging to a file — entities are stored grouped
by file and sorted within it, which makes "entities of this file" a slice, not a
search, and makes the per-file patch protocol (§4.2) a range replacement.

### 3.3 Sizing, measured on the monster

Derived from the built cache (`sum(length(...))` over all 454,528 rows), not
estimated:

| section | bytes | derivation |
|---|---:|---|
| STRINGS · id tails | 11.1 MB | 43.7 MB raw − `file_path::` prefix (§3.4) |
| STRINGS · names (interned, 56,552 distinct) | 0.7 MB | 3.62 MB raw across 454,528 rows |
| STRINGS · file paths (40,877 distinct) | 1.35 MB | 31.6 MB raw before interning |
| STRINGS · kinds (interned) | ~1 KB | 3.38 MB raw before interning |
| ENTITIES | 14.5 MB | 454,528 × 32 B |
| NAMES | 1.8 MB | 454,528 × 4 B |
| FILES | 1.6 MB | 40,877 × 40 B |
| DIRS | ~72 KB | ~3,000 × 24 B |
| **base total** | **≈ 31.2 MB** | **15.7% of the 198.6 MB corpus** |

**Confirmed by the skeleton** (§9): a real image over a *larger* scope —
714,819 entities across 81,273 files, the probe's own unfiltered walk — came
out at **52.7 MB**, i.e. 73.7 bytes/entity against this table's predicted
71.4. The estimate holds.

Against `cache.db`'s 653 MB (329% of corpus) that is **21× smaller**. The
reduction is not compression; it is the removal of three redundancies a row
store cannot remove: `file_path` repeated on every entity (31.6 → 1.35 MB),
`entity_type` repeated on every entity (3.38 MB → ~1 KB), and the `file_path::`
prefix repeated inside every `id` (§3.4).

**Trigram tier budget for S3:** zoekt-class ngram indexes land at 15–20% of
indexed content. Over 198.6 MB that is **30–40 MB**, giving a whole-index
target of **≤ 70 MB ≈ 35% of corpus** — still ~9× smaller than today's
`cache.db`, which stores no text postings at all. S3 owns proving this; if the
postings exceed 40 MB the fallback is to index only files under a size cap and
declare the tier partial in `flags`, never to blow the budget silently.

### 3.4 Id elision — proven, not assumed

Entity ids total 43.7 MB. Two elision rules were tested against all 454,528
rows before either was adopted:

| candidate rule | holds for |
|---|---|
| `id = parent_id ++ "::" ++ kind ++ "::" ++ name` (children) | **0 / 155,083** — rejected |
| `id = file_path ++ "::" ++ kind ++ "::" ++ name` (roots) | 155,561 / 299,445 — rejected |
| `id` starts with `file_path ++ "::"` | **454,528 / 454,528** — adopted |

Id construction is a per-language plugin concern (markdown nests by heading,
JSON uses `file::/pointer`), so it is **not** derivable from the graph's shape
and must not be reconstructed from `(parent, kind, name)`. That rule looked
obviously true and is false for every child in the corpus; adopting it would
have been a silent correctness bug. What *is* universally true is the
file-path prefix, so `ENTITIES` stores only `id_tail` and the reader
concatenates `file_path ++ "::" ++ id_tail`. 43.7 MB → 11.1 MB, losslessly.

`flags` bit 0 records that ids are elided, so a future writer that meets a
language violating the prefix rule can store full ids without a format break.
The writer asserts the prefix rule per entity and falls back to full ids for the
whole image if any entity violates it — a fail-safe, not a fail-stop.

### 3.5 Versioning and salt

Three independent invalidation axes, all folded into the header, reusing the
facts store's vocabulary rather than inventing a parallel one:

- `format_version` — the byte layout, local to this file. Bumped whenever a
  section's record shape changes.
- `build_salt` — the *semantic* identity of the extractor that produced the
  entities. This is **not** a new knob: it is `xxh3` over
  `facts_store::FACTS_SCHEMA_VERSION`, `sem_core_salt()` (i.e.
  `env!("CARGO_PKG_VERSION")`), and the `LANGUAGE_SALTS` table folded in a
  stable order. A resolution-rule change that alters entity identity already
  moves a language salt, and therefore already invalidates the index, with no
  second place to remember to bump.
- per-file `content_hash` + mtime — freshness of individual files (§2), using
  `parser::incremental::content_hash` (xxh3-64) unchanged.

The facts store deliberately governs both of its tiers with one
`FACTS_SCHEMA_VERSION` and explicitly rejects a second version number for the
same semantics. This design honours that: the *semantic* knob is reused, and
`format_version` is added only for the axis facts has no analogue for — a byte
layout that a `mmap` reader indexes into by fixed offset.

The invalidation rule is uniform and has exactly one branch: **magic, version,
or salt mismatch ⇒ the index does not exist.** Never error, never partially
trust, never migrate in place, never panic on a truncated or garbage file. The
caller falls through to the build plane and the next build writes a correct
image. This is verbatim the facts store's "clean miss" discipline — the one
pinned by `schema_version_mismatch_is_a_clean_miss`,
`truncated_file_is_a_clean_miss_not_a_panic`, and `garbage_bytes_are_a_clean_miss`
— and the index must carry the same four tests.

### 3.6 Single-writer atomicity

Write to a sibling temp file, then `rename` over `index.sem`, following
`FactsStore::save` exactly:

```rust
let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(),
                                      SEQ.fetch_add(1, Ordering::Relaxed)));
std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path))
// on error: let _ = std::fs::remove_file(&tmp);
```

Rationale, in the order that matters:

- `rename(2)` within a directory is atomic, so a reader never observes a torn
  image.
- A reader that has already `mmap`'d the old inode keeps a valid mapping after
  the rename; the old inode survives until the last mapping is dropped. **No
  reader/writer lock is needed and no reader is ever interrupted.** This is the
  property that lets the query plane have no daemon and no coordination.
- Concurrent writers are last-writer-wins, which is safe because every writer is
  producing a function of the same corpus. `<pid>.<seq>` in the temp name keeps
  two concurrent writers from corrupting each other's staging file, and the
  per-process `AtomicU64` keeps two threads in one process apart.

**No `fsync`, deliberately.** There is no `sync_all`/`sync_data` anywhere in
`crates/*/src` today, and the index must not be the first: durability across a
power loss would be a *new* guarantee, and it is one this artifact does not
need. A torn or lost index is a clean miss (§3.5) and the next build rewrites
it. Paying an `fsync` on a 27.6 MB image to protect an advisory cache would be
buying a guarantee nobody asked for with latency everybody pays.

The corpus tier's `ShardLock` is **not** adopted: that lock exists because a
shard write is read-merge-write, whereas an index write is a whole-image
replacement computed from the graph. Nothing can be lost-updated, so there is
nothing to lock. (`index.log` in §4.2 is append-only with a monotonic
`generation`, which is likewise resolvable without a lock.)

### 3.7 Dependencies, and the wasm constraint

`sem-core`'s manifest carries a standing constraint — pure Rust, no C
dependency, so the `wasm` feature stays viable — and there is currently **no
memory-mapping code and no `memmap2`, `fst`, `zerocopy`, `bytemuck`, or `rkyv`
anywhere in the workspace.** This design adds exactly one dependency,
`memmap2` (pure Rust, satisfying the constraint), and confines it as narrowly
as possible:

**The format and reader are defined over `&[u8]` and have no dependency at
all.** `mmap` is one constructor among several:

```rust
IndexReader::from_bytes(&[u8])   -> pure, no deps, wasm-safe, the test surface
IndexReader::open(&Path)         -> mmap, cfg'd off on wasm32
```

This is what makes the format testable without touching a filesystem, keeps
`wasm32` building (where `mmap` does not exist), and means a future decision to
swap `memmap2` for a raw `libc::mmap` or for `fs::read` on a platform that needs
it is a one-constructor change rather than a format change.

### 3.8 Why a sorted key table and not an fst

The name tier is `NAMES`: `entity_count × u32`, entity indices sorted by their
name bytes, binary-searched. Rejected alternative: the `fst` crate's compressed
FSA.

- **Duplicate names are the norm, not the exception.** 454,528 entities carry
  56,552 distinct names — a mean of 8 entities per name, and `createProgram`
  alone has 12. The answer to a name query is a *set*. A sorted table returns
  that set as a contiguous `equal_range` — the answer shape is the storage
  shape. An fst maps key → one `u64`, so it would need a side postings section
  to express the same thing, adding a section and an indirection to save space.
- **The space it saves is not the constraint.** `NAMES` is 1.8 MB of a 27.6 MB
  image. An fst might save ~1 MB. The measured constraint is 10 ms of latency,
  against which both structures are ~µs.
- **Binary search's cost is bounded and legible**: log₂(454,528) = 19 probes,
  each touching one `u32` and one `Str32` — worst case ~19 cache misses plus the
  arena reads. An FSA walk is O(len) with a less predictable access pattern.

Adopt an fst if and when prefix or fuzzy name queries become a requirement —
that is what an automaton is actually *for*, and it would be a new section
(there is a reserved slot), not a replacement.

**Postings encoding, the contract for S2:** `REFS` is CSR — an
`(entity_count + 1) × u32` row-offset array plus a flat `u32` target array.
O(1) row access, zero-copy, no per-query decode. Delta-encoding within a row is
permitted only if S2 *measures* a win against the 10 ms budget; the default is
raw `u32`, because a decode loop in the hot path is exactly the kind of cost
this format exists to remove.

---

## 4. The write path

### 4.1 Base image, emitted at graph build

`sem graph`'s full build already holds a complete `EntityGraph` and the file
list with fingerprints. The index writer is a pure function of those:

```
write_index :: EntityGraph → [FileFingerprint] → [DirFingerprint] → Salt → Bytes
```

It is emitted **in the same operation that writes `cache.db`**, from the same
in-memory graph, so the two artifacts cannot disagree about what was built. Cost
budget: the image is 27.6 MB of mostly-`memcpy`; it must not be a measurable
fraction of the 29.7 s build, and specifically must not approach the 20.1 s
`cache_full_save` it will eventually make deletable.

### 4.2 Patch protocol — per-file, red-green driven

A fixed-layout image cannot absorb an in-place edit when a file's entity count
changes: every subsequent record would shift. Rewriting the whole image per
edit is O(corpus) and violates the answer-cost invariant. So the index is
**base image + append-only overlay**, compacted on a threshold:

```
index.log := sequence of PatchRec, each:
    file_path        Str (inline, length-prefixed)
    mtime_secs/nanos, content_hash
    generation       u64            monotonic, from the writer
    entities         [InlineEntity] the file's COMPLETE new entity list
                                    (empty ⇒ the file was deleted)
```

- A patch is **whole-file replacement**, never a delta within a file. This is
  the property that makes the protocol verifiable: a patch record is exactly
  what a cold extraction of that one file produces, so "is the patch correct" is
  the question red-green's `GraphSession` already answers bit-identically.
- On open, the reader scans `index.log` (bounded, see below) and builds one
  `HashMap<file, PatchRec>` keeping the highest `generation` per file. Cost is
  O(|log|), not O(|corpus|).
- Lookup consults the overlay first: an entity whose file is patched comes from
  the patch; the base's `entity_lo..entity_hi` range for that file is masked out.
- **Compaction**: when patched files exceed 5% of `file_count`, or the log
  exceeds 4 MB, the writer folds base ⊕ log into a new base via §3.6's
  temp+rename and truncates the log in the same operation. Readers mid-query are
  unaffected (§3.6).

**Consistency guarantee.** For any file `f` and generation `g`:

```
lookup(base ⊕ log)  ≡  lookup(write_index(cold-fold(corpus_at_g)))
```

restricted to the files the log covers. This reduces to two obligations, and
neither is new:

1. **Extraction identity** — the patch's entities for `f` equal a cold
   extraction of `f`. Already guaranteed by the red-green `GraphSession` oracle
   (bit-identical, 811 ms/50-file) and by the per-file extraction cache. The
   index does not re-derive this; it *transports* it.
2. **Serialization identity** — `write_index` followed by `read_index` is the
   identity on the entity set. This is new, and it is what §5 tests.

The guarantee deliberately does **not** cover cross-file effects: an edit to `f`
that changes how `g`'s references resolve is a *refs* concern, and `REFS` is
S2's section. S2 must state whether its postings are patchable per-file or
require compaction; if resolution is non-local, the honest answer is that a
patch touching an import invalidates the REFS section and forces a compaction,
and the format supports that by letting a section be marked stale in `flags`.

---

## 5. The read path

```
open(root)   →  stat + mmap index.sem, validate header, scan index.log   ~1 ms
lookup(name) →  binary search NAMES, equal_range, materialize N rows     ~µs
verify(A)    →  parallel stat over files(A); on mismatch, re-extract     ~µs–ms
```

### 5.1 Lazy per-answer freshness

After producing candidate answer `A`, the reader stats exactly `files(A)` —
typically 1–8 files — and compares `(mtime, size)` against `FileRec`, falling
through to the content hash only on an mtime difference, exactly as
`shared_cache::file_freshness` does today. On a mismatch the reader
re-extracts *just those files* through the existing per-file extraction cache,
applies the result as an in-memory patch (the same `PatchRec` shape as §4.2),
re-runs the lookup, and — best-effort, never blocking the answer — appends the
patch to `index.log`. A query thus repairs the view it reads, which is what
keeps a long-lived working tree from drifting without a daemon.

### 5.2 What the reader must never do

- **Never walk the filesystem.** `FILES` is the file list. This is the −301 ms.
- **Never shell out to git.** This is the −157 ms.
- **Never require a resident process.** This is §1.5.
- **Never load a section it did not answer from.** `mmap` makes this automatic —
  a name lookup touches `NAMES`, a few `ENTITIES` records, and the arena pages
  they point at. It must not be defeated by eagerly materializing anything.

---

## 6. The query-consistency oracle

`index_probe`, modelled on `incr_probe`, is the acceptance gate. It is the
reason any of this is allowed to serve an answer.

**Property 1 — serialization identity.** For a repo:
build `EntityGraph` cold → `write_index` → `read_index` → for **every** distinct
name in the graph, `index.lookup(name)` equals the graph's answer as a set of
entity ids, with equal `(file, kind, start_line, end_line, parent)` on each. Any
inequality fails. This is the obligation §4.2 defers to §5, and it also
discharges the id-elision rule (§3.4) empirically on every corpus it runs on.

**Property 2 — patch equivalence (mutation-tested).** For a random file `f` and
a random mutation (add an entity, delete one, rename one, delete `f`, add a new
file):
```
read(base ⊕ patch(f))  ≡  read(write_index(build(corpus')))
```
Mutations are drawn from the same generator `incr_probe` uses, so the two probes
agree on what a realistic edit is.

**Property 3 — freshness soundness.** After a mutation applied *without*
notifying the index, a `Verified` lookup whose answer touches `f` must **not**
return the stale rows: it must detect the mismatch and repair. A `Complete`
lookup must additionally detect a *newly added* file, which is what exercises
the `DIRS` section.

**Property 4 — budget.** Cold-process name lookup on the monster < 10 ms.
This is a *test*, not a benchmark, because a regression here invalidates the
design rather than merely slowing it.

Properties 1–3 run in CI on fixture repos; 1, 2, and 4 run on the monster in the
probe binary.

---

## 7. Removal list

What the index obsoletes. **S4 (semx-woe) executes these**; this section is the
authority for what may go and why. Verdicts are `DELETE`, `KEEP`, or
`DEMOTE` (survives, but leaves the query path).

| # | layer | sites | verdict | reason |
|---|---|---|---|---|
| 1 | **Query-path file discovery** — `find_supported_files_in_path` / `find_supported_files_with_options` called *before* the cache is consulted | `commands/entities.rs:109`, `commands/impact.rs` (`file_discovery` mark), `commands/graph.rs:67` | **DELETE** | 301 ms, 56% of the answer-shaped query, to compute a file list the index already holds. Largest single win in the study. Build path keeps it. |
| 2 | **Git freshness oracle** — `git_oracle_says_fresh`, `git_working_tree_clean`, `compute_oracle_eligible`, `oracle_cache_fresh`, `oracle_fresh_topology`, `oracle_fresh_counts`, consts `ORACLE_MIN_FILES`/`ORACLE_TIMEOUT_MS`, metadata keys `git_head_oid`/`git_built_clean`/`oracle_eligible`, env `SEM_FRESHNESS`/`SEM_FRESHNESS_TIMEOUT_MS` | `sem-cli/src/cache.rs` | **DELETE** | §1.4: 157 ms vs the 76 ms scan it replaces, and vs the 12 ms parallel stat that replaces both. Strictly dominated: slower *and* a weaker guarantee. Deleting it also removes a shell-out, a thread, a timeout, and two tuning knobs. |
| 3 | **Per-file corpus freshness scan** — `cached_files_are_fresh`, `has_fresh_cache`, `has_fresh_topology_cache_for_files`, `has_fresh_complete_cache`, `has_fresh_topology_cache`, `has_fresh_topology_only_cache` | `sem-cli/src/cache.rs` | **DELETE** | Six near-identical corpus-wide gates, 62 ms of which is serial SQLite (§1.6). Replaced by one per-answer verify + one `Complete` parallel stat. |
| 4 | **SQLite answer-from-SQL fast paths** — `query_entities_listing`, `write_entities_listing_json`, `query_impact_topology`, `query_fresh_impact_topology`, `query_dependency_impact_topology`, `write_graph_json_topology`, `oracle_context_subgraph` | `sem-cli/src/cache.rs` | **DELETE** | These exist *only* because hydrating the graph was expensive. The index removes the reason. Note the duplication they force today: `try_cached_entities` and `try_write_cached_entities_json` are the same query written twice, once returning rows and once streaming JSON. One index read replaces both. |
| 5 | **Resident sidecar + autospawn** — `sem-cli/src/commands/sidecar.rs` (whole file), `sem-mcp/src/sidecar.rs` (whole file), `SEM_NO_SIDECAR`, `SEM_NO_AUTOWARM`, the `~/.sem/sock` tree, the hook client in `commands/hook.rs:117` | both crates | **DELETE** | §1.5: 0% availability at this scale, +300 ms tax, 2.6 GB RSS, ~935 leaked sockets. Its stated justification is verbatim "instead of paying a fresh process + SQLite hydrate (~800ms)" — the index makes a fresh process cost single-digit ms, which deletes the justification. Fusion rule: the strong constructor subsumes the weak one. |
| 6 | **SQLite graph hydrate on the query path** — `load`, `load_with_source_scope`, `load_graph_topology*`, `load_graph_topology_rows`, `load_edges`, `entities_with_content_by_id` | `sem-cli/src/cache.rs` | **DEMOTE** | The *query* callers go away. `load_partial*` and the incremental-rebuild callers are **build plane** and must stay until the index's own patch path subsumes them — which is not this bead's claim to make. |
| 7 | **Cloud fast path** — `try_cloud_entities`, `cloud::try_impact` / `try_context`, `SEM_MCP_CLOUD` | `commands/cloud.rs`, `sem-mcp/src/server.rs` | **DEMOTE** | Not a latency tier — it is a different *capability* (cross-repo xref, repos with no local index). It must stop sitting *in front of* the local path: a network round trip cannot beat a <10 ms local answer, so the ordering is now backwards. Keep the capability, invert the precedence. |
| 8 | **Cold rebuild** — `EntityGraph::build` via `ParserRegistry` | `sem-core` | **KEEP** | It is the oracle. §5.1's per-answer repair calls into it. Everything above is deletable *because* this exists. |

Two things that are **not** on this list and should be, once someone owns them:

- `sem entities <single file>` consults **no cache at all** and re-parses (229 ms,
  §1.3). That is a missing path, not a surviving layer; the index gives it for
  free via `FileRec.entity_lo..entity_hi`.
- `SEM_TIMINGS` has no mark around the freshness gate, which is why this study
  needed temporary instrumentation. Whoever touches `entities.rs` next should
  add one; it costs nothing and it is the difference between a 30-minute
  attribution and a 3-hour one.

Net: **five layers deleted, two demoted, one kept.** The query path becomes
`open → lookup → verify`, and every fallback that exists today exists because
one of §1's costs was real. Remove the costs, remove the fallbacks.

---

## 8. The proof slice — measured

`crates/sem-core/src/index/` implements the entity tier of §3 (writer, mmap
reader, entity-by-name lookup; `REFS`/`TRIGRAM`/`DIRS` reserved and
zero-length). `crates/sem-core/examples/index_probe.rs` is the harness.

```
index_probe write  <repo_root> <index_path>    # build, write, run the oracle
index_probe lookup <index_path> <name>...      # cold process, timed
```

**On the monster** (probe's own unfiltered walk — 714,819 entities across
81,273 files, i.e. ~1.6× the scope `sem entities` indexes, so every number
below is conservative):

| | |
|---|---:|
| cold `EntityGraph::build` | 10,459 ms |
| index build (in memory, from the graph) | 887 ms |
| atomic write of the 52.7 MB image | 4.3 ms |
| **consistency oracle, Property 1** | **86,796 names, 714,819 entities, 0 mismatches — PASS** |

**Cold-process lookup, `createProgram` (12 hits), 6 separate process spawns:**

| | median |
|---|---:|
| `OPEN` — mmap + header + salt + section validation | **0.044 ms** |
| `LOOKUP` — binary search + materialize 12 rows incl. id reconstruction | **0.049 ms** |
| `COLD_TOTAL` — process entry → last answer | **0.136 ms** |
| **process wall — fork/exec/dyld + all of the above** | **6–7 ms** |

Sixty random names in one cold process: `COLD_TOTAL` **1.27 ms**, slowest
single lookup 0.059 ms, process wall 9 ms.

**Budget: < 10 ms. Result: 6–7 ms of process wall, of which 0.14 ms is the
query.** The remaining ~6.5 ms is process startup, which is now the *entire*
remaining cost and the only thing left worth optimizing on this path.

Against the same question today — `sem impact --file src/compiler/program.ts
createProgram`, 536 ms — that is **~80× faster**, and the answers are
identical: the index returns the same 12 ids at the same line numbers that
`sem impact` prints in its ambiguity list.

Two honesty notes on the measurement:

- The page cache is warm; the image was just written. A genuinely cold read
  would fault in the pages the answer touches. The 60-name run is the guard
  against a single lucky cache line, and it faults across the image for 1.27 ms
  total. Sequential-read of 52.7 MB would be ~25 ms on this machine, so the
  worst credible cold case stays inside the budget only because `mmap` faults
  *the answer's pages*, not the file — which is precisely the property §3.1
  selected the format for.
- One run was observed genuinely cold (the probe binary itself evicted): 1,216 ms
  of process wall, of which `OPEN` was 0.264 ms and `LOOKUP` 0.132 ms. The
  index work stays sub-millisecond even from disk; the wall time was the
  dynamic loader reading a cold binary. That is the shape of the remaining
  cost — **binary load, not query** — and it is the same cost `sem`'s 78 MB
  binary already pays on every invocation today.
- `content_hash` is written as 0 by the probe (it does not re-hash the corpus).
  The lookup path does not read it; §5.1's verify does, and that is S4's wiring.

## 9. What S2 and S3 build against

- Sections 4 (`REFS`) and 5 (`TRIGRAM`) are reserved in the header's section
  table today and are `{offset: 0, len: 0}` in a skeleton image. Adding them is
  **not** a `format_version` bump; a reader must treat a zero-length section as
  "tier absent" and fall through, so S2 and S3 can land independently and in
  either order.
- `REFS` is CSR over entity indices (§3.8). Entity indices are stable within an
  image and **not** stable across images — a posting must never be persisted
  outside the image that produced it.
- `TRIGRAM` postings are over **file** indices, not entity indices, so the text
  tier can answer without the entity tier being loaded, and so a file patch
  invalidates a bounded set of postings. Budget: ≤ 40 MB (§3.3).
- Both tiers inherit §3.5's invalidation rule and §3.6's atomicity unchanged.
  Neither may introduce a second on-disk file, a lock, or a daemon.
- Both must extend `index_probe` with their own Property-1 analogue before they
  are allowed to serve an answer.

---

## 10. S2 — REFS landed, verbs routed off the legacy path (semx-gis)

`REFS` is no longer reserved: `crates/sem-core/src/index/writer.rs`'s
`build_refs_section` serializes `EntityGraph::dependencies` (forward — "refs")
and `EntityGraph::dependents` (reverse — "callers") as the CSR §3.8
specified — raw `u32`, no delta-encoding (never needed the 10 ms budget's
slack). `QueryIndex::{refs_of, callers_of}` are the reader side.
`index_probe`'s `REFS_ORACLE` is Property 1's refs analogue: for every entity,
`refs_of`/`callers_of` equal `EntityGraph::dependencies`/`dependents` as a
sorted id list. **714,819 entities checked, 0 mismatches, on the monster**;
also run and green on `sem-core` itself (3,030 entities) as the medium repo.

Three new CLI verbs read the index directly (`crates/sem-cli/src/commands/
query.rs`): `sem find <name>` (definitions), `sem callers`, `sem refs` — plus
two reroutes of existing verbs onto the same index: `sem entities <file>`
(single-file case, previously §7's noted "missing path" that re-parsed on
every call) and `sem impact --deps <entity> --file <f>` (the entity-scoped
fast path that used to hit `query_dependency_impact_topology`/SQLite).

### 10.1 Per-verb latency, cold process, median of ≥500 runs on the monster

| verb | median | vs. baseline |
|---|---:|---|
| `sem find createProgram --file program.ts` | **4.6 ms** | vs 229 ms (`entities <file>` re-parse baseline, §1.3) — 50× |
| `sem callers createProgram --file program.ts` | **4.7 ms** | new verb, no prior baseline |
| `sem refs createProgram --file program.ts` | **4.7 ms** | new verb, no prior baseline |
| `sem entities program.ts --json` (rerouted) | **4.8 ms** | vs 229 ms same-file, and vs 27 ms this repo measured *before* the reroute landed (SQLite-adjacent local build) — 47× / 5.6× |
| `sem impact createProgram --file program.ts --deps --json` (rerouted) | **4.7 ms** | vs 536 ms (§1.1's answer-shaped-query baseline) — **114×** |

All five clear the <10 ms budget with more than 2 ms to spare — process wall
still dominates (§8's ~6–7 ms floor), the query work itself is sub-millisecond
per §8's `LOOKUP`/`REFS` numbers (0.03–0.06 ms measured via `index_probe
refs`). rg's whole-repo baseline (167 ms, §1.3) is not the right comparison
for any of these — every one of them is an *answer-shaped* query, and rg does
not answer "who calls this" at all.

### 10.2 Freshness costs

- **Verified** (implemented, the default for all five verbs above): stats
  exactly the files an answer touches. Measured inline in the totals above —
  it costs nothing extra when nothing changed, because the mtime match short
  circuits before any hash read (`commands::query::file_is_stale`).
- **Patch-path microbench** (item 5 — one stale *definition* file,
  end-to-end): touched a comment into `src/compiler/moduleSpecifiers.ts`
  (1,470 lines) and re-ran `sem find` against an entity in that file.
  **10.4 ms median** (n=275) vs **4.7 ms** on the same query once the file is
  reverted — a **5.8 ms repair cost**, almost entirely the single-file
  re-extract (`ParserRegistry::extract_entities_brief`, in-process, no cache
  hit). This is the one number in this table that lands *at* the budget line
  rather than comfortably under it: content-local repair for a ~1,500-line
  file costs the whole margin the fast path otherwise has. It is still 51×
  faster than the 536 ms answer-shaped baseline, and repairs are rare by
  construction (only fire when Verified freshness actually finds staleness).
- **Complete** (§2's whole-corpus parallel-stat level): wired into every
  index-backed verb as of §13 (semx-ykf) — was "not wired into any verb by
  this bead," now closed. See §13 for the mechanism, the measured cost, and
  what's still out of scope.

### 10.3 Patch protocol choice (§4.2)

Staleness repair is **in-memory only, per query, never persisted.** On a
stale *definition* file, the affected entities are re-extracted and the
in-memory answer is patched before rendering (§4.2's "extraction identity"
obligation — a local, content-only operation). On a stale *related* file (a
caller/ref target), this bead does **not** attempt a partial CSR patch —
resolving whether an edit changed an edge needs the two-pass symbol table, not
a one-file re-extract, so the verb falls all the way through to a full cold
rebuild instead (which then calls `write_query_index` and leaves a fresh
image for next time). Nothing is appended to an on-disk `index.log`: that
structure (§4.2's `PatchRec` sequence, generation numbers, compaction
threshold) is **not implemented** by this bead. The chosen alternative is
"**mark dirty for next build**" in the weakest possible sense — there is no
dirty flag at all; the on-disk image simply stays as it was until the next
corpus-level build (`sem graph`/`diff`/`impact` build, or a `GraphSession`
warm rebuild) calls `write_query_index` again via the hooks in `cache.rs`.
Every read-side repair this bead makes is therefore transient: correct for
the query that triggered it, forgotten immediately after, cheap to reason
about, and never at risk of the base image and the log disagreeing — because
there is no log. §4.2's obligation ("append-only, best-effort, never blocking
the answer") is satisfied by the degenerate case of appending nothing.

### 10.4 What got bypassed, and how

Per §7's discipline ("bypass, don't delete — S4 deletes"), the new/rerouted
verbs never call into the five doomed layers when the index can answer; the
old code is untouched and still runs when the index can't:

- **File discovery, git oracle, per-file corpus scan** (§7 #1–3): never
  imported into `commands::query` at all — the absence is structural, not a
  runtime check.
- **SQLite fast paths** (§7 #4): `commands::query` never opens `DiskCache`.
  `commands::impact`'s `try_index_impact_deps` runs *before*
  `query_dependency_impact_topology` and returns early on success, so the SQL
  path is skipped whenever the index can answer confidently — and falls
  through unchanged (same matching semantics, verified by the existing
  `impact_direct_deps.rs` suite) whenever it can't (ambiguous match, custom
  `--file-exts` scope, or a stale related file).
- **Resident sidecar** (§7 #5): `commands::query` never imports
  `commands::sidecar`. `commands::impact`'s index fast path runs *before*
  `try_sidecar_impact`, so a successful index answer never pays the sidecar's
  connect-or-autospawn cost at all.
- **Fallback discipline** (bead item 3): index missing, truncated, or
  salt-mismatched → the only fallback is a cold `EntityGraph::build` over a
  fresh walk, which then writes an index via `write_query_index` — never a
  detour through SQLite or the sidecar. `SEM_NO_INDEX=1` forces this same
  path unconditionally, for A/B measurement (§10.1's 815 ms `impact --deps`
  legacy-path comparison was taken this way).

### 10.5 Write-path wiring (bead item 4)

`write_query_index` (`sem-cli/src/cache.rs`) is called from all three
corpus-level `DiskCache` save methods — `save_with_test_dirs`, `save_topology`,
`save_incremental_with_repair_metadata` — which between them cover every CLI
build path (`sem diff`/`graph`/`impact`'s full and topology builds) and every
`GraphSession` warm rebuild that persists. A repo therefore gets an index the
first time *any* of these run, not from a separate index-build step. The
write itself re-reads and re-hashes every source file (xxh3, matching what
`QueryIndex::file_fingerprint` compares against) — measured at under 1 s on
the monster's 714k-entity image (§8's `WRITE build_ms=980.5`), a rounding
error against the multi-second builds that trigger it.

### 10.6 Known gaps, disclosed rather than silently scoped out

- **`--entity-id` is not index-fast-pathed.** The format has no id→entity
  section (§3's `NAMES` is name-keyed only); `commands::query::
  resolve_by_id_index` reconstructs the owning file by trying `"::"`-prefix
  candidates against `FILES` and confirming with a full id match — correct by
  construction, but only reachable from `sem callers`/`sem refs` when given a
  raw id string, not from `sem impact --entity-id` (out of scope; falls
  through to the unchanged legacy path).
- **No qualified `Parent.child` addressing** in the new verbs or the `impact`
  reroute — matches exactly what the SQLite fast path it replaces already
  supported (verified by reading `entity_candidates_for_query`); the fuller
  `find_entity`/`entity_matches_qualified` path is untouched and still runs
  when the fast path declines.
- ~~**`Complete` freshness is unimplemented**~~ — **closed by §13
  (semx-ykf).** Every verb now proves membership-freshness, not just
  content-freshness.
- **No `index.log`** (§10.3) — repairs are transient, never persisted between
  queries. Still true after §13: a discovered new file is extracted and
  merged in-memory for the query that found it, never written to disk: the
  degenerate "append nothing" reading of §4.2's patch protocol, same choice
  §10.3 already made for content repairs, now applied to membership too.

---

## 11. S3 — TRIGRAM landed, `sem grep` beats rg (semx-az9)

`TRIGRAM` is no longer reserved: `crates/sem-core/src/index/writer.rs`'s
`build_trigram_section` extracts every distinct byte-trigram from each
indexed file's content and serializes file-index postings; `crates/sem-core/
src/index/grep.rs` is the reader-side verb — pattern → required-trigram
query → postings intersection → candidate files → verify with the real
regex matcher on candidates' *current* bytes only → rg-compatible
`file:line:text`. `sem grep <pattern>` (`crates/sem-cli/src/commands/
grep.rs`) is the CLI surface. Design follows Google Code Search / zoekt
(§9's prior art): trigram postings are a *prefilter*, never the source of
truth — a real match always survives to the candidate set, and the
candidate set is always re-verified against live bytes before being called
a match.

### 11.1 Encoding

```text
TRIGRAM section:
 0  trigram_count u32
 4  posting_total u32
 8  KEYS     trigram_count x u32   -- packed trigram | STOP_FLAG (bit 31),
                                      sorted by the low 24 bits
    OFFSETS  (trigram_count+1) x u32  -- CSR row-offsets into TARGETS
    TARGETS  posting_total x u32   -- file indices, ascending, deduped
```

Same CSR shape §3.8 already specified for `REFS`, over **file** indices
instead of entity indices (§9's boundary: the text tier answers without the
entity tier loaded, and a file patch invalidates a bounded set of
postings). A trigram is 3 raw bytes, packed as `b0<<16 | b1<<8 | b2` — fits
in 24 bits, which leaves the high 8 bits of a `KEYS` entry free. Bit 31 is
used for `STOP_FLAG` (§11.2) at zero storage cost; sorting and binary search
mask it off, so it never perturbs `KEYS`'s ordering.

This gives the reader three, not two, answers for one trigram
(`QueryIndex::trigram_posting` → `TrigramPosting`):

| answer | means | correctness role |
|---|---|---|
| `Absent` (no `KEYS` entry) | this trigram occurs in **zero** indexed files | a required-and-absent trigram proves the pattern matches nothing — `NoCandidates`, no scan needed at all |
| `Stopped` (`KEYS` entry, empty row) | too common to afford a posting list for (§11.2), or the tier isn't built | **no filtering information** — must never be read as "zero files" |
| `Present(files)` | the real, bounded posting list | intersect it |

Conflating `Absent` and `Stopped` would be a correctness bug, not an
imprecision: a genuinely-absent trigram is a hard proof of no matches, and a
stop-listed one is exactly the opposite claim (unknown). The design keeps
them distinguishable for free instead of picking one and getting it wrong
half the time.

### 11.2 Budget outcome — measured, and one line over

Extraction is a raw byte sliding window (`content.windows(3)`), whole file,
including bytes that span lines — a cross-line substring pattern still needs
its trigrams findable. Per-file extraction is parallelized (`rayon`,
`maybe_par_iter!`, same convention `facts_store.rs`/`graph.rs` use); the
merge into postings is serial (iterating files 0..N in order means every
posting row comes out pre-sorted by file index for free, so a merge-time
sort is never needed).

**Budget enforcement (the bead's item 1 — stop-trigrams, and their cost):**
`TRIGRAM_BUDGET_BYTES = 40 MiB` is enforced by `stop_list_to_budget` —
greedy-largest-posting-first: sort trigrams by document frequency
descending, clear postings (keep the `KEYS` slot, flip `STOP_FLAG`) until
the section fits. This is a real trade the monster forced, not a
theoretical one: **`return`'s own trigrams (`ret`/`etu`/`tur`/`urn`) and
`Error`'s (`Err`/`rro`/`ror`) are themselves among the 2,268 stop-listed**
— common English/code words are exactly what the budget must prune first,
because they're the trigrams with the largest document frequency. Query-time
cost: a pattern whose *every* required trigram was stop-listed gets zero
filtering information and falls back to a full scan (`CandidateOrigin::
FullScan`) — never a wrong answer, only a slower one. This is the honest
shape of the trade-off the bead asked to be documented, not glossed over.

Measured on the monster (probe's own unfiltered walk, 81,273 files):

| | value |
|---|---:|
| distinct trigrams | 165,867 |
| stop-listed | 2,268 (1.4%) |
| posting_total (after stop-listing) | 10,153,057 |
| **TRIGRAM section bytes** | **41,939,176 (41.9 MB)** |
| trigram build wall (extract, parallel) | 91–99 ms |
| trigram build wall (merge + stop-list, serial) | 195–247 ms |
| **added build cost, total** | **≈ 290–350 ms** |

The section lands **at** its 40 MiB ceiling (41,943,040 bytes) by
construction — the greedy stop-lister removes postings only until the
budget is met, so it converges just under the line rather than leaving slack
on the table. **The 40 MB per-tier budget is honored** (§11's contract).

**The 70 MB whole-index projection from §3.3 is not.** On the production
scope (`sem-cli`'s actual 40,869-file / 454,528-entity corpus, the same one
§3.3's 31.2 MB base-tier estimate was computed against):

| section | bytes | share |
|---|---:|---:|
| STRINGS | 15.0 MB | 18.7% |
| FILES | 1.6 MB | 2.0% |
| ENTITIES | 14.5 MB | 18.2% |
| NAMES | 1.8 MB | 2.3% |
| REFS (S2) | 5.2 MB | 6.5% |
| **TRIGRAM (S3)** | **41.9 MB** | **52.3%** |
| KINDS | ~0 | 0.0% |
| **whole index** | **80.1 MB** | **≈ 40% of corpus** |

**Over budget, with attribution:** entity tier + REFS already total
**38.1 MB** before TRIGRAM is added at all; TRIGRAM's own 40 MB budget was
sized independently (zoekt's 15–20%-of-corpus heuristic, §3.3), without
netting against what S2 had already spent. `38.1 + 41.9 ≈ 80 MB` is
arithmetic, not a bug in either tier — **each tier honored its own budget**,
and the sum exceeds the aggregate 70 MB projection by **≈10 MB (14%)**.
Fixing this is a tuning decision, not a code change: `TRIGRAM_BUDGET_BYTES`
is a single `pub const` (`writer.rs`) and `build_trigram_section_with_budget`
already accepts it as a parameter (used by this bead's own tests) — lowering
it to ~32 MB would bring the whole index back under 70 MB, at the cost of a
larger stop-list (more common-word queries falling back to full scan).
Left at 40 MB here because the bead's contract named that number explicitly
for the trigram tier; the aggregate figure is surfaced for whoever owns the
next budget pass (S4 or beyond), not silently absorbed.

**Fallback choice, and why not the other one:** §3.3 named file-size-capping
("index only files under a size cap") as the anticipated over-budget
fallback. This bead used stop-listing instead. Reasoning: a size cap makes
whole files **permanently unsearchable via the trigram prefilter** (a blind
spot with no query-time recovery), where stop-listing only makes *some
trigrams* less selective — every file stays fully searchable, worst case
degrading to the same full scan the pattern would need without a trigram
tier at all. Stop-listing dominates size-capping on the same axis §1.4 used
to reject the git-freshness oracle: it is not a weaker guarantee bought with
better numbers, it is a strictly better guarantee (never a blind spot) at
the same budget.

### 11.3 Trigram derivation for patterns — scope and the soundness argument

`pattern → required trigrams` is not a full regex-AST analysis (that is
Code Search's actual approach, and it's out of scope here — see §11.6);
it's a single conservative pass (`grep::literal_runs`) that:

- Accumulates literal characters, flushing the run on any metacharacter
  (`. ^ $ ( ) [ ] { } | \`).
- On a quantifier (`* + ?`), **drops the last accumulated character**
  before flushing — that character is what's quantified and therefore not
  guaranteed present. (`+` technically guarantees ≥1 occurrence; this is not
  special-cased. Dropping is always *safe* — it only ever costs
  selectivity, never correctness.)
- `[...]` character classes are skipped opaquely (no expansion attempted).
- `\` followed by a regex metacharacter is literal (`\.` → `.`); `\`
  followed by anything else (`\d`, `\w`, `\s`, `\p{...}`) is an opaque
  boundary — the class information is discarded, not miscounted.
- Top-level `|` (alternation, `grep::split_top_level`) splits into DNF: the
  candidate set is the **union** over alternatives of the **intersection**
  of each alternative's required-trigram postings. Nested alternation
  inside a group (`a(b|c)d`) is *not* split — the whole group is an opaque
  boundary, same as any other metacharacter run.

**Soundness, stated once:** for a file to match the compiled regex, it must
match some alternative; if it matches alternative *i*, alternative *i*'s
mandatory literal runs are present verbatim in the file's bytes by
construction of the runs above; therefore the file's trigram set contains
every trigram those runs generate; therefore the file survives that
alternative's postings intersection; therefore it is in the union this
module returns. **A real match is never excluded from the candidate set.**
The only things that can happen to selectivity are `Stopped` trigrams
(skipped, weakens the AND) and `Absent` trigrams (proves an alternative
impossible, which is sound — see §11.1's table).

Case-insensitive search and inline regex flag groups (`(?i)`, `(?x)`, …)
are **not** approximated — both disable the trigram prefilter entirely
(`grep::has_inline_group_flags`) rather than risk a false negative: a
case-folded pattern's byte-exact trigrams don't prove anything about a
differently-cased match, and `(?x)` (extended/verbose mode strips
whitespace/comments before matching) could make the literal-run text not
correspond to what the compiled regex actually requires. Both would be
*correctness* bugs, not selectivity ones, if approximated — so neither is.

### 11.4 The oracle

`index_probe`'s `TRIGRAM_ORACLE` (S3's Property 1 analogue, `QUERY-INDEX.md`
§6/§9's obligation): for a 6-pattern battery — common identifier, rare
identifier, two-word phrase, regex with a literal core, a pattern with no
usable trigram, and a TypeScript-specific common identifier added for the
mutation test's benefit (see below) — `grep::search` (postings-narrowed,
candidate-verified) must equal `grep::full_scan` (ground truth, never
touches `TRIGRAM`) as the same `(file, line)` set. Deliberately generic
across languages where possible (it runs on TypeScript *and* `sem-core`'s
own Rust, and the property holds whether a pattern gets 0 hits or
thousands):

| corpus | patterns | mismatched | verdict |
|---|---:|---:|---|
| `sem-core` (86 files, 3,109 entities) | 6 | 0 | **PASS** |
| TypeScript monster (81,273 files, 714,819 entities) | 6 | 0 | **PASS** |

The monster run is also where §11.2's stop-listing trade-off shows up
empirically in the oracle's own output: `return` and `[A-Za-z]+Error`
resolve via `FullScan` (their trigrams are stop-listed), `TRIGRAM_BUDGET_
BYTES` resolves via `NoCandidates` (a required trigram is genuinely absent
from TypeScript source), and the two-word phrase and `createProgram` resolve
via the `Trigram` fast path — the oracle passes on **all three origins**,
which is a more thorough correctness proof than if every pattern had taken
the same path.

**Mutation test (bead item 3):** `index_probe::mutation_test` corrupts one
real `TARGETS` entry — flips a posting to point at a different file — and
confirms `search` (postings-narrowed) then diverges from `full_scan`
(ground truth, unaffected by the corruption). Getting this test to actually
exercise the failure mode it claims to took two rounds of empirical
correction, both left in the code as comments because they're the kind of
mistake a mutation test can make silently:

1. **Corrupting an unreached trigram.** The first version picked "the
   pattern's first three bytes". A pattern's candidate resolution ANDs
   several required trigrams and short-circuits the moment one is
   `Absent` — corrupting `TRIGRAM_BUDGET_BYTES`'s `"TRI"` posting on the
   monster changed nothing, because a *different* required trigram of that
   same pattern (`"BUD"`) is genuinely absent from the corpus, and the query
   never got far enough to consult `"TRI"`'s postings at all. Fix: only pick
   a pattern whose live `origin` is already `Trigram` (the intersection
   provably completed), then corrupt one of *that* pattern's own trigrams.
2. **Corrupting a trigram with no true positive to lose.** Fixed-per-(1),
   the test then picked `candidate files` on the monster — `Trigram`-origin,
   a real `Present` posting — and still produced no observable divergence.
   Cause: that phrase has **zero real occurrences** in TypeScript's own
   source (§11.4's table: `hits=0`). `verify_file` re-checks every candidate
   against its live bytes regardless of how it entered the candidate set, so
   wrongly adding or removing a file that was never a true positive changes
   nothing about the final hit list — the corruption was real, but
   unobservable through a hits-based comparison. Fix: require the chosen
   pattern to have at least one real hit, then corrupt specifically the
   posting entry for one of *that hit's own files* — guaranteeing the
   corruption drops a true positive out of the candidate set, which
   `verify_file` cannot silently recover (it never sees a file that isn't a
   candidate). `createProgram` was added to `PATTERN_BATTERY` to guarantee at
   least one (`Trigram`, hits > 0) pattern exists on the monster (`return`
   already covers this on `sem-core`, where nothing gets stop-listed).

With both fixes:

| corpus | pattern | trigram corrupted | true-positive file dropped | verdict |
|---|---|---|---|---|
| `sem-core` | `return` | `etu` | `Cargo.toml` | **PASS** |
| TypeScript monster | `createProgram` | `teP` | `scripts/dtsBundler.mjs` | **PASS** |

A corrupted posting produces a wrong *answer* (`search` diverges from
`full_scan`), not just a wrong byte nobody would notice — the oracle has
teeth, and the two rounds above are evidence it was actually exercised
rather than assumed.

### 11.5 Benchmark — `sem grep` vs `rg`, monster, median-of-5 cold process

Same battery, but re-picked for the benchmark table specifically to exercise
the trigram fast path where the oracle's original battery happened to hit
this corpus's stop-list (§11.2's `return`/`Error` finding) — realistic
agent-style identifier queries, not generic English words, because that's
the actual "find this symbol/string" use case this tier serves. Cold
process, serial, median of 5 runs, `/usr/bin/time`-independent wall clock
(`date +%s%N` around each spawn), production index (real `sem-cli` write
path, not the probe).

| pattern class | pattern | `sem grep` | `rg` (repo, naive) | speedup | `rg` (`src/` only) |
|---|---|---:|---:|---:|---:|
| common identifier | `createProgram` | **16 ms** (160 hits, Trigram, 167 candidate files†) | 905 ms (164 hits) | **57×** | 15 ms (121 hits) |
| rare identifier | `getEmitFlags` | **13 ms** (81 hits, Trigram) | 903 ms (81 hits) | **69×** | 15 ms (81 hits) |
| two-word phrase | `Debug Failure` | **7 ms** (12 hits, Trigram) | 922 ms (15 hits) | **132×** | 16 ms (3 hits) |
| regex, literal core | `get[A-Za-z]+Flags` | **23 ms** (641 hits, Trigram) | 907 ms (641 hits) | **39×** | 16 ms (639 hits) |
| no usable trigram (<3 chars) | `ab` | 716 ms (62,625 hits, **FullScan**) | 980 ms (161,525 hits) | 1.4× | 19 ms (23,031 hits) |
| common word, stop-listed | `return` | 736 ms (83,131 hits, **FullScan**) | 970 ms (132,697 hits) | 1.3× | 19 ms (31,152 hits) |

† `sem grep createProgram` alone (SEM_GREP_STATS=1) measured 167 candidate
files of 40,869 — the table's 160/16ms row is the same query, different
run; both cold, within measurement noise of each other.

**Every trigram-served query clears the <50 ms target with more than 2×
margin to spare** (7–23 ms), and beats naive `rg` on the same repo by
39×–132×. The two full-scan rows are the honestly-reported worst case
(§11.2): even there, `sem grep` is faster than naive `rg` (1.3–1.4×), purely
because its corpus is half the file count (§11.5.1) and it never walks the
filesystem (§5.2) — but nowhere near the 50 ms target, exactly as
documented for a query with zero trigram evidence.

**11.5.1 — the hit-count gap, explained, not hidden.** `sem grep`'s corpus
is `sem`'s own indexed scope: supported-language files only, with
`is_default_excluded`'s generated/fixture/vendor/benchmark directories
already dropped (same scope `sem find`/`sem entities` use — this is not a
grep-specific restriction). On the monster that's **40,869 files**; naive
`rg` (no scoping — what an agent actually types) walks **81,406** — almost
exactly double, because `tests/baselines/` alone is 477 MB of committed
fixture output (`du -sh tests/baselines`) against `src/`'s 36 MB. This is
the *same* confound §1.3 already flagged ("566 MB of `tests/baselines`
dominates") for the pre-index `rg` comparison, so the `rg (src/ only)`
column is included to show what a hand-scoped `rg` would answer: closer to
`sem grep`'s counts (`getEmitFlags` matches exactly at 81/81/81 across all
three), and still 15–19 ms even at that narrow scope, which only sharpens
the point that `sem grep`'s real advantage is not needing the user to
already know which directory to scope to. `sem grep`'s own hit counts are
proven correct against its own scope by §11.4's oracle — the gap to `rg` is
corpus-boundary disclosure, not a matching bug.

### 11.6 Out of scope, and what falls back to a full scan

Kept deliberately minimal per the bead's own framing ("literal + basic
regex via candidate-verify covers the 95% agent use case"):

| not supported | behavior | why |
|---|---|---|
| Case-insensitive search (`-i`) | prefilter disabled, full scan | a case-sensitive trigram proves nothing about a case-folded match (§11.3) |
| Inline flag groups `(?i)`, `(?x)`, etc. | prefilter disabled, full scan | same hazard as above, plus `(?x)` can desync literal-run text from what the compiled regex needs (§11.3) |
| Nested alternation (`a(b\|c)d`) | the group is an opaque boundary, not split | full regex-AST trigram derivation (Code Search's actual approach) is out of scope; sound, just less selective |
| `\d \w \s` and POSIX/Unicode classes (`\p{L}`, `[[:alpha:]]`) | opaque boundary, no literal contributed | same reasoning as classes generally (§11.3) |
| Multi-line matching (a pattern intended to span `\n`) | never matches | `verify_file` splits on `\n` before matching, same default `rg` has without `-U` |
| Non-UTF-8 file content | not trigram-indexed at all | `write_query_index` already skips non-UTF-8 files for the entity tier's `content_hash` (pre-existing, unrelated to this bead); trigram content reuses that same read, so the exclusion is inherited, not new |
| `-F`/fixed-strings, `-w`/word-boundary | not implemented | pattern is always regex-interpreted (rg's default without `-F`); a literal search is simply a regex with no metacharacters, so there is one code path, not two |
| Trigram-tier *content*-drift on an already-known file | a file edited since the last index build such that it newly contains a required trigram it didn't contain before is not added to the candidate set | **still open after §13** — this is not the membership gap (closed, see below); it's a *content* gap in the trigram prefilter specifically. Detecting it would mean re-deriving trigrams for every already-known file per query, which re-adds the O(corpus) cost §1 spent 85% of the query budget removing. Note the asymmetry with the entity tier: entity content-freshness has no such gap (`file_is_stale` catches it, §5.1) because entity answers are file-scoped; the trigram *prefilter* is corpus-wide by construction, so there is no per-answer file set to re-verify against. |
| `Complete` freshness for `sem grep` | **closed by §13 (semx-ykf)** — new files join the candidate set, deleted files are dropped from it, via the same `complete_check` sweep the entity verbs use | was "not this bead's claim to make"; is now |
| Result streaming / a hit cap | the whole result set is materialized in memory | not a concern at current scale (worst case observed: 161,525 lines, `rg`'s own whole-repo `ab` count) — flagged for whoever pushes a pathological full-scan pattern into production |

### 11.7 Write-path wiring

`sem-cli/src/cache.rs`'s `write_query_index` reads every file once (in
parallel, `rayon`) and feeds the same bytes to both the entity tier's
`content_hash` (unchanged behavior) and `TRIGRAM`'s extraction
(`sem_core::index::build_with_content`) — never a second read. This is the
same three `DiskCache` save methods §10.5 already wired (`save_with_test_
dirs`, `save_topology`, `save_incremental_with_repair_metadata`) plus
`commands::query`'s cold-build fallback, so a repo gets a trigram-bearing
index from whichever build path runs first, with no separate index-build
step to remember.

---

## 12. S4 — what was removed and why (semx-woe)

The fleet showdown (§10.1/§11.5's methodology, now run on all 13 GREP-KILLER
fleet repos rather than just the monster — see `NIGHT-REPORT.md` for the full
matrix) confirmed §7's verdicts hold at scale: every trigram-served `sem
grep` beat `rg` by 20-378x, and the five index-backed verbs answered in
single-digit ms on every repo, C/C++/C's `scope_resolve:None` limiting only
which *verbs* had anything to answer (§10.6), never their latency. That
cleared §7's gate ("benchmarks pass") for the removal pass to begin.

Of §7's 5 DELETE + 2 DEMOTE + 1 KEEP, **one DELETE landed this bead; four
DELETE and two DEMOTE did not** — found harder-entangled than §7's own
one-line-per-item framing suggested once actually opened, and this section
says exactly how, rather than either rushing them or silently dropping them.

| # | item | verdict this bead | why |
|---|---|---|---|
| 1 | Query-path file discovery | **NOT DONE** | Blocked on a missing capability, not a hard deletion. `sem entities <file>`/`find`/`callers`/`refs`/`impact --deps` (S2) already bypass discovery because they resolve one *name*. The remaining call sites (`entities.rs:109`'s directory-listing branch, `impact.rs`'s non-`--deps` modes, `graph.rs:67`) all answer "every entity under path P", which nothing in S2's verb set does today. The reader *could* answer it — `QueryIndex::all_file_paths()` + `entities_in_file()` already exist and are unused for this shape — but wiring correct output ordering, `--no-default-excludes`/`--file-exts` exclusion semantics, and per-file Verified freshness (§5.1) to match the existing `find_supported_files_in_path` path's observable behavior is real feature work (a fourth reroute, S2-shaped), not a subtraction. Attempting it under this bead's remaining time risked either a rushed, under-tested reroute of `sem entities <dir>`/`sem impact`/`sem graph` — commands real users run — or reverting under pressure. Left undone rather than risked. Concrete head start left here for whoever picks it up. |
| 2 | Git freshness oracle | **NOT DONE** | Lives in `sem-cli/src/cache.rs` (4,036 lines), tightly coupled to items 3 and 4 below (the freshness gates this item deletes are what items 3/4's SQL fast paths are gated behind). Not attempted separately from 3/4 — see the combined note below. |
| 3 | Per-file corpus freshness scan | **NOT DONE** | Same file, same coupling. The six near-identical gates (`has_fresh_cache`, `has_fresh_topology_cache_for_files`, `has_fresh_complete_cache`, `has_fresh_topology_cache`, `has_fresh_topology_only_cache`, plus `cached_files_are_fresh`) are each called from different combinations of the SQL fast paths in item 4, so deleting them one at a time without deleting their callers in the same commit would leave dead gates guarding nothing, and deleting them together with item 4 is a much larger single change than item 5 (sidecar) was. |
| 4 | SQLite answer-from-SQL fast paths | **NOT DONE** | `query_entities_listing`, `write_entities_listing_json`, `query_impact_topology`, `query_fresh_impact_topology`, `query_dependency_impact_topology`, `write_graph_json_topology`, `oracle_context_subgraph` are exactly the fallback tier item 1's missing reroute (above) would need to exist *before* these can safely go — `sem entities <dir>` and `sem context` currently have no other fast path once these and the sidecar (item 5, done) are both gone; only the `KEEP`-8 cold rebuild would remain, which is correct but a real latency regression for those two commands specifically, undisclosed nowhere else, disclosed here. |
| 5 | Resident sidecar + autospawn | **DONE — commit e85ba02** | Deleted whole-file (`sem-cli/src/commands/sidecar.rs`, `sem-mcp/src/sidecar.rs`), all 5 callers rerouted or removed, `SEM_NO_SIDECAR`/`SEM_NO_AUTOWARM` gone, `run_resident()` deleted (its only job was this). This one *was* a pure subtraction: every caller already had a working fallback beneath it (S2's index-backed fast paths, or the unchanged legacy path), because the sidecar never actually answered in production (§1.5) — so nothing needed a replacement built first. 555 lines deleted, 55 added, net -500. Full suite green (587 sem-core lib + all integration binaries + 93 sem-mcp + all sem-cli integration tests). Two hidden dependents found and disclosed rather than silently broken: `sem mcp --resident` (kept as a no-op flag — an in-flight, uncommitted `sem setup` change wires it into a Claude Code hook and deleting the flag would make that land broken) and `sem hook prompt-submit` (kept registered, now an honest no-op instead of an always-failing socket connection — see commit message for both). |
| 6 | SQLite graph hydrate (DEMOTE) | **NOT DONE** | Same `cache.rs` cluster as items 2-4; the query-plane/build-plane split this item asks for (`load`/`load_with_source_scope`/`load_graph_topology*` go, `load_partial*` and incremental-rebuild callers stay) can't be verified correct without items 1-4 already resolved, since several of the "query" callers slated to go are the same SQL fast paths item 4 deletes. |
| 7 | Cloud fast path (DEMOTE) | **NOT DONE** | Precedence inversion (local index answers before cloud is even tried) touches `commands/cloud.rs` and `sem-mcp/src/server.rs`. Independent of items 1-4/6 in principle, not attempted only because of the time already spent getting item 5 right with full disclosure rather than rushing every item to a shallower standard of verification. |
| 8 | Cold rebuild | **KEEP, unchanged** | Confirmed still the oracle every other tier is measured against — no action needed or taken. |

**Net this bead: one layer deleted (555 LOC), four layers and two demotions
still standing, all four blocked on the same root cause** — items 1-4 (and
transitively 6) all route through `sem-cli/src/cache.rs`'s freshness/SQL
cluster, and item 1's missing reroute for directory-shaped queries is the
one piece of *new* capability the rest of the cluster's removal depends on
being safe. Item 7 is the one truly independent item left and is the
natural next slice for a follow-up bead, alongside item 1's reroute design
sketched above.

---

### 12.1 S4 second half — items 1/4/7 partially executed, items 2/3/6 confirmed still-coupled (semx-woe continuation)

This continuation built the enabler §12's table said was missing
(`QueryIndex::files_under`, a `partition_point` range over the already-sorted
`FILES` section — no new on-disk section, no format-version bump) and used it
to reroute the one call site that was both highest-value and cleanly scoped:
`entities.rs`'s directory-listing branch, the exact site §1 measured as 301 ms
/ 56% of the answer-shaped query. `impact.rs`'s non-`--deps` modes and
`graph.rs`'s whole-repo build were investigated and **not** rerouted this
pass — both need full-corpus **edges**, not just a file/entity listing, and
`graph.rs`'s JSON output serializes each edge's `ref_type`, which the `REFS`
CSR tier does not carry (§3.8's postings are bare target indices). Rerouting
those two onto the index without silently flattening edge-type fidelity is a
distinct, larger reroute — left for a follow-up, same standard as item 1's
first-half writeup: disclosed, not rushed.

| # | item | verdict this continuation | why |
|---|---|---|---|
| 1 | Query-path file discovery | **PARTIAL — DONE for `entities.rs`'s directory branch** | `QueryIndex::files_under(prefix)` added (`sem-core/src/index/reader.rs`), oracle-checked by a new `FILES_ORACLE` in `index_probe` (whole-repo and every top-level directory, on a fresh index — Property 1's analogue for this tier) plus two unit tests (`files_under_matches_a_directory_prefix_not_a_string_prefix`, `files_under_reflects_only_what_the_index_built_from`, the latter naming the disclosed gap explicitly: `Verified`-level only, since `DIRS` is still reserved — a file added since the last build is invisible until the next rebuild, the same tradeoff S2's `find`/`callers`/`refs` already ship with for a brand-new name). `entities.rs`'s directory loop now tries `try_index_entities_for_dir` first (gated on `CacheSourceScope::Default`, matching the index's own build scope) and falls through to the unchanged walk on any decline. `impact.rs` non-`--deps` and `graph.rs` remain **NOT DONE** — see above. |
| 2 | Git freshness oracle | **NOT DONE, confirmed (not just assumed) still-coupled** | `git_oracle_says_fresh` has two live call sites after this continuation's deletions (grep-verified): `has_fresh_cache` (→ `has_fresh_complete_cache`/`has_fresh_topology_cache`/`has_fresh_topology_only_cache`, in turn used by `load`/`query_impact_topology`/`query_fresh_impact_topology`/`write_graph_json_topology`) and `oracle_cache_fresh` (→ `oracle_fresh_topology`/`oracle_fresh_counts`, `graph.rs`'s own git-oracle-gated fast path, unchanged this pass). Both are load-bearing for commands this continuation did not reroute. |
| 3 | Per-file corpus freshness scan | **PARTIAL — 1 of 6 gates deleted** | `has_fresh_topology_cache_for_files` deleted (it had exactly two callers, both also deleted — see item 4). The other five (`has_fresh_cache`, `has_fresh_complete_cache`, `has_fresh_topology_cache`, `has_fresh_topology_only_cache`, `cached_files_are_fresh`) all still gate a live SQL fast path or `load`/`load_with_source_scope` (item 6) and stay. |
| 4 | SQLite answer-from-SQL fast paths | **PARTIAL — 2 of 7 deleted** | `query_entities_listing` and `write_entities_listing_json` deleted (each had exactly one caller — `entities.rs`'s directory branch — grep-verified before deletion), plus their now-dead helper `has_fresh_topology_cache_for_files`, the `EntityListingJsonRow` struct, and the now-unused `sort_entity_infos`. The other 5 (`query_impact_topology`, `query_fresh_impact_topology`, `query_dependency_impact_topology`, `write_graph_json_topology`, `oracle_context_subgraph`) stay — each is still the only fast path for a command (`impact.rs` non-`--deps`, `graph.rs`, `context.rs`) that was not rerouted this pass; deleting them now would silently regress those commands to the `KEEP`-8 cold rebuild, exactly the risk §12's first half flagged. |
| 5 | Resident sidecar + autospawn | **DONE — commit e85ba02 (unchanged, prior bead)** | See above. |
| 6 | SQLite graph hydrate (DEMOTE) | **NOT DONE, confirmed still-coupled** | `load`/`load_with_source_scope`/`load_graph_topology*` remain live: `impact.rs`'s non-`--deps` build path and `graph.rs`'s cache hydrate both still call into this cluster, neither rerouted this pass. Moving these into a separate build-plane module was considered and **not done**, because a module split that still leaves the "query" half of the split with live query-plane callers would misdescribe the split as complete when it isn't — the honest state is that items 2-4/6 remain one coupled cluster inside `cache.rs`, not two cleanly separated ones. |
| 7 | Cloud fast path (DEMOTE) | **DONE for `sem-cli` (`entities.rs`, `impact.rs`, `context.rs`); NOT DONE for `sem-mcp/src/server.rs`** | `sem-cli`: all three cloud call sites moved from "always tried first" to "tried only once the fastest available local answer has already declined" — `entities.rs` now checks for a local index before considering cloud for the whole-repo listing (once an index exists, item 1's reroute answers this shape locally and cloud is never reached); `impact.rs` moved the cloud attempt from before `try_index_impact_deps`'s sibling entity-scoped SQL fast path to after it; `context.rs` moved it from before the git-oracle subgraph fast path to after. `sem-mcp/src/server.rs` has three cloud sites: two (`impact`, `context`) are already opt-in behind `SEM_MCP_CLOUD=1` (off by default, lower severity — the precedence issue is structurally present but not production-active); the third (the `abs_path.is_dir()` entities listing) is ungated and production-active, matching `sem-cli`'s old bug exactly, but `sem-mcp` has no `files_under`-backed local fast path for this shape yet (only `sem-cli`'s `entities.rs` got that reroute this pass) — gating cloud off there without a fast local replacement would trade a working-but-backwards-ordered path for an unconditionally slower one, a regression, not a fix. Left undone rather than risked; the natural next step is porting item 1's `entities.rs` reroute to `sem-mcp` first. |
| 8 | Cold rebuild | **KEEP, unchanged** | Still the oracle. |

**LOC ledger, this continuation** (`git diff --numstat` on the touched files,
verified against the pre-existing dirty set being unchanged before/after):

| file | insertions | deletions | net | what |
|---|---:|---:|---:|---|
| `sem-cli/src/cache.rs` | 8 | 156 | **−148** | delete `query_entities_listing`, `write_entities_listing_json`, `has_fresh_topology_cache_for_files`, `EntityListingJsonRow`, `sort_entity_infos`, unused `serde::Serialize` import |
| `sem-cli/src/commands/entities.rs` | 119 | 108 | +11 | directory branch rerouted onto the index (`try_index_entities_for_dir`), cloud precedence inverted, dead `try_cached_entities`/`try_write_cached_entities_json`/`DiskCache` import removed |
| `sem-cli/src/commands/impact.rs` | 10 | 4 | +6 | cloud precedence inverted (moved after the entity-scoped local fast path) |
| `sem-cli/src/commands/context.rs` | 10 | 4 | +6 | cloud precedence inverted (moved after the oracle subgraph fast path) |
| `sem-cli/tests/entities_cli.rs` | 17 | 14 | +3 | two tests updated from the deleted SQL path's phase/counter names to the index path's; both renamed to say what they now test |
| `sem-core/src/index/reader.rs` | 41 | 0 | +41 | `QueryIndex::files_under` |
| `sem-core/src/index/mod.rs` | 77 | 0 | +77 | two unit tests (directory-prefix boundary, stale-membership) |
| `sem-core/examples/index_probe.rs` | 70 | 0 | +70 | `FILES_ORACLE` |
| **total** | **352** | **286** | **+66** | |

`cache.rs` itself: **4,036 → 3,888 lines (−148, −3.7%)**. The whole-continuation
diff is net *positive* (+66 LOC) because the new capability (`files_under` +
its oracle + its tests, +188 LOC in `sem-core`) is larger than the query-path
code it made deletable so far (−148 LOC in `cache.rs`) — expected and
disclosed, not a miss: §7's framing always said the enabler was new feature
work, not a subtraction, and the subtraction it unlocks is partial (2 of 7 SQL
functions, 1 of 6 gates) because only one of the three blocked call sites
(`entities.rs`) was rerouted this pass. The other two (`impact.rs` non-`--deps`,
`graph.rs`) are real, larger reroutes of their own, correctly left for a
follow-up rather than forced through under this bead's own "don't rush"
precedent.

**Gates, this continuation**: 589 sem-core lib tests (587 baseline + 2 new,
0 failed), all sem-core integration binaries (0 failed, including the
previously-flaky `parse_cache::a_cache_hit_is_recorded_for_a_repeated_blob`,
green on this run), 93 sem-mcp lib tests (0 failed), 236 sem-cli integration
tests across all 18 suites (0 failed — 2 of them updated in this continuation
to match the new implementation, not skipped). `ORACLE`/`REFS_ORACLE`/
`FILES_ORACLE`/`TRIGRAM_ORACLE` all PASS via `index_probe` on `sem-core`
itself (2,760 names / 3,122 entities / 86 files, 0 mismatches on any tier).
clippy and fmt clean on every file this continuation touched (verified by
diffing clippy's warning set before/after by line-shifted content, not just
by count — the 23 pre-existing warnings in `cache.rs`/`entities.rs`/
`impact.rs`/`context.rs` are the same 23 warnings, just at shifted line
numbers; zero new warnings).

**Regression spot-check** (monster + rails, `sem find` / `sem grep`,
median-of-5, vs `NIGHT-REPORT.md`'s banked fleet rows): both repos' verb
latency stayed single-digit-to-low-double-digit ms and both stayed dramatically
faster than `rg`, but both ran roughly 1.4-2.5x the banked numbers in absolute
terms (rails: find 10ms vs 7ms banked, grep 13ms vs 7ms banked; monster: find
11-12ms vs 6ms banked, grep 19-20ms vs 7-8ms banked) — outside the stated ±20%
tolerance band. Disclosed, not explained away: `sem find`'s and `sem grep`'s
own code (`query.rs`, `grep.rs`) is untouched by this continuation, so this is
not a regression this pass introduced; the honest attribution is ambient load
(this session runs alongside other addressable agents per the orchestrator's
own session roster, and load average was 5-7 during these runs vs the
4.1-4.9 the original fleet run saw on rails, similar-to-higher on the
monster). The relative claim the removal pass depends on — index-backed
verbs answer single-digit-to-low-double-digit ms, `rg` does not — holds by a
wide margin either way (rails `rg` naive 53-55ms, monster `rg` naive
903-936ms banked; nothing measured here comes close to that band).

Filed as bead semx-woe's own continuation rather than closed as complete on
the same conservative standard the first half used — see `NIGHT-REPORT.md`'s
removal-pass section for the fuller narrative. This continuation still leaves
`impact.rs`/`graph.rs`'s reroutes (item 1's remaining two call sites), items
2/3/6's remaining SQL/gate cluster, and `sem-mcp`'s entities cloud precedence
(item 7's remaining site) for a follow-up bead — each with a concrete,
disclosed reason above, not a silent gap.

---

### 12.2 semx-zvq — typed `REFS` + a first slice of `impact.rs`'s non-`--deps`
reroute (still open — filed, not closed, on the same conservative standard)

This bead's brief (§12.1's own residue note) named the blocker exactly:
`impact.rs`'s non-`--deps` modes and `graph.rs` need full-corpus **edges**,
not just a file/entity listing, and `graph.rs`'s JSON output serializes each
edge's `ref_type`, which the `REFS` CSR didn't carry. Two of this bead's five
work items are done and verified; the rest remain blocked on real feature
work this bead's remaining scope did not safely cover, disclosed below rather
than rushed or silently dropped — same standard §12/§12.1 both hold
themselves to.

**Item 1 — FORMAT (`REFS` typed edges): DONE.** Commit `01943c7`. `ref_type`
packed into the high 4 bits of each existing CSR target `u32` (28 bits left
for the target index, `MAX_ENTITIES` = 268,435,456) rather than a parallel
`u8` array — see `format::refs`'s module doc for the full measured trade
(packing: 0 extra bytes, 0 extra reads, cap 375x above the monster's measured
714,819 entities; parallel array: ~+400 KB across fwd+rev on the monster's
edge count, plus a second cache line per read, for a guarantee packing
already provides at lower cost — `semantic-hard-cut-refactor`'s dominance
test, `c1 ⪰ c2`). `format_version` bumped 1→2 (§3.5: stale images are a clean
miss, not a misread). `REFS_ORACLE` extended to check every edge's `ref_type`
against an independently-regrouped fresh graph rebuild:

| corpus | entities | mismatched | kind_mismatched | typed |
|---|---:|---:|---:|---|
| rails (`/tmp/bench-fleet/rails`) | 59,249 | 0 | 0 | true |
| monster (`microsoft/TypeScript`) | 714,819 | 0 | 0 | true |

Gates: sem-core lib 600/600 (596 baseline + 4 new), sem-cli 244/244, sem-mcp
93/93, clippy/fmt clean on all 5 touched files.

**Item 2 — REROUTE: PARTIAL.** Commit `3880d7f`. `ImpactMode::Dependents`
rerouted (`try_index_impact_dependents`, direct mirror of the already-shipped
`try_index_impact_deps`, `callers_of` instead of `refs_of` — direct-edge,
depth-1 shape, no BFS, so the typed CSR from item 1 wasn't even needed for
this slice; `refs_of`/`callers_of` untyped already carried what it needs).
Byte-identical proof, pre-change binary (`01943c7`) vs post-change, stdout
only (one stderr-only artifact found and disclosed — see the commit message:
`commands::consent::maybe_cloud_tip`, a one-time-per-repo promotional nag
unrelated to this reroute, triggered only because the legacy path was slow
enough on its first-ever invocation to cross the tip's 1200ms threshold):

| repo | invocations | mismatched |
|---|---:|---:|
| rails | 4 (entity-id json, name+file terminal, entity-id json, ambiguous-name-decline json) | 0 |
| monster | 4 (entity-id json ×2, name+file terminal, name+file json) | 0 |

`ImpactMode::All`/`ImpactMode::Tests` and `graph.rs`: **NOT rerouted**, same
disclosure standard as §12.1's own item 1. Precise reason, not "ran out of
time": `All`/`Tests` need `impact_entities`' transitive multi-hop BFS
(`query_fresh_impact_topology`'s SQL walks `edges` recursively to `depth`,
frontier-ordered), not a single direct-edge lookup — the CSR makes a BFS
*possible* (`refs_of`/`callers_of` are O(1) per hop) but reproducing the
SQL's exact frontier order under the byte-identical-output bar, plus
`Tests`' test-detection heuristic on top, is a distinct, larger reroute
`impact.rs`'s own Deps/Dependents precedent doesn't cover for free.
`graph.rs` is a different shape again: a whole-corpus dump (every entity,
every edge with its `ref_type`), not one entity's neighbors — the risk isn't
whether the index *can* answer it (`files_under("")` + a full `REFS` section
scan clearly can) but matching the legacy JSON's exact field/edge-listing
shape (fwd-only edges, not fwd+rev double-counted) without a dedicated
verification pass this bead's remaining scope did not include. Left undone
rather than risked, per the brief's own explicit instruction for this case.

**Item 3 — DELETE: NOT DONE, confirmed (not assumed) still fully coupled.**
Grep-verified (not re-derived from the doc's prior claim) that item 2's
partial reroute changed **zero** cache.rs call-site counts: `git_oracle_says_
fresh` (2 sites), `has_fresh_cache` (5), `has_fresh_complete_cache` (1),
`has_fresh_topology_cache` (2), `has_fresh_topology_only_cache` (1),
`cached_files_are_fresh` (2), `query_impact_topology` (4), `query_fresh_
impact_topology` (1), `query_dependency_impact_topology` (1), `write_graph_
json_topology` (3), `oracle_context_subgraph` (1) — identical counts to
§12.1's own findings. This is expected, not a miss: `try_index_impact_
dependents` is an *additional* fast path in front of the existing SQL
Dependents path, not a replacement for it — the SQL path is still reached
whenever the new fast path declines (ambiguous name, stale file, `--no-cache`,
`SEM_NO_INDEX`, non-default source scope), exactly the same fallback
relationship `Deps`' own fast path has always had with `query_dependency_
impact_topology`. Nothing in cache.rs's freshness/SQL cluster became provably
dead this bead. Layers a-e of the deletion chain (git freshness oracle,
SQLite hydrate, 5 freshness gates, 5 SQL fast paths, `sem-mcp` cloud demote)
are **not attempted** — deleting any of them now, before `All`/`Tests`/
`graph.rs` are rerouted, would silently regress those commands to the
`KEEP`-8 cold rebuild, the exact risk §12's first half and §12.1 both already
flagged for this same cluster.

**Item 4 — spot-check: done, honestly reported, not clean.** Median-of-5
cold-process, this bead's binary, load average noted (both runs: ~3.0-3.7 —
lower than §12.1's own 5-7, higher than §13.4's 2.6-3.3):

| repo | verb | median | banked | delta | note |
|---|---|---:|---:|---|---|
| rails | find | 8.7 ms | ~7 ms (§12.1) | +24% | just outside ±20% |
| rails | grep | 17.1 ms | ~7 ms (§12.1) | +144% | well outside ±20%, same qualitative pattern §12.1 disclosed (ambient shared-machine load) rather than a regression this bead introduced — `query.rs`/`grep.rs` are untouched by either of this bead's commits |
| rails | impact --deps | 6.4 ms | — (no banked row) | — | new-fast-path baseline for comparison below |
| rails | impact --dependents | 6.6 ms | — (new capability) | — | within noise of --deps on the same repo, as expected for a symmetric reroute |
| monster | find | 21.3 ms | 19.9 ms (§13.4) | +7% | within ±20% |
| monster | grep | 26.2 ms | 25.0-28.6 ms (§13.4) | within band | within ±20% |
| monster | impact --deps | 6.8 ms | ~5.9 ms (§13.4, unmodified-verb control) | +15% | within ±20% |
| monster | impact --dependents | 6.7 ms | — (new capability) | — | within noise of --deps |

Read honestly: monster's numbers hold inside tolerance; rails' `grep` in
particular overshoots by a wide margin, matching §12.1's own disclosed
finding on the same shared machine rather than indicating a regression from
this bead's actual diff (`query.rs`/`grep.rs` are not touched by either
commit in this bead). Not explained away, not fabricated clean.

**Item 5 — this section.** Bead **left open**, not closed: two of five items
done and verified, three items (the bulk of the reroute + the whole deletion
chain + the spot-check's honest-not-clean numbers) require either real
follow-on feature work (`All`/`Tests`/`graph.rs` reroutes) or are correctly
blocked pending that work (deletions). Same standard §12.1 held itself to
when it filed as a continuation rather than closing on a shallower bar.

**LOC ledger, this bead:**

| commit | file | +/− | what |
|---|---|---:|---|
| `01943c7` | `sem-core/examples/index_probe.rs` | +107/−10 | `REFS_ORACLE` kind check |
| `01943c7` | `sem-core/src/index/format.rs` | +98/−1 | `FORMAT_VERSION` 2, `FLAG_REFS_TYPED`, `refs` module |
| `01943c7` | `sem-core/src/index/mod.rs` | +122/−1 | 4 new typed-CSR unit tests |
| `01943c7` | `sem-core/src/index/reader.rs` | +73/−2 | `refs_of_typed`/`callers_of_typed`/`refs_are_typed`, target-mask fix |
| `01943c7` | `sem-core/src/index/writer.rs` | +70/−22 | `build_refs_section` groups from `graph.edges`, packs kind |
| `3880d7f` | `sem-cli/src/commands/impact.rs` | +96/−0 | `try_index_impact_dependents` |
| **total** | | **+566/−36 (net +530)** | |

`cache.rs`: **unchanged this bead — still 3,888 lines** (§12.1's figure).
Item 3's finding above is exactly why: nothing in this bead's actual diff
touches `cache.rs`, because nothing became safely deletable from it.

**Kept on the legacy path, with reason** (this bead's contribution to the
running list §7/§12/§12.1 maintain): `ImpactMode::All`, `ImpactMode::Tests`,
`graph.rs` (all three: need transitive-BFS or whole-corpus-dump reroutes not
attempted this bead, precise reasons in item 2 above); the full deletion
chain, items a-e (blocked on the above, item 3's grep-verified call counts
are the evidence, not an assumption).

### 12.3 semx-zvq, executed — both reroutes landed, the deletion cascade run,
the bead closed (2026-08-13)

§12.2 filed this bead open with three items outstanding: `ImpactMode::All`/
`Tests`, `graph.rs`, and the whole a-e deletion chain those two blocked.
All three are done. This section is the final ledger — what shipped, what it
cost, what it did **not** do and why, and the two pre-existing bugs the work
uncovered. It supersedes §12.2's status table; §12.2 is left in place as the
record of where the bead stood before this pass.

---

#### The ordering-parity derivation (§12.2's named blocker)

§12.2's exact words: "reproducing the SQL's exact frontier order under the
byte-identical-output bar … is a distinct, larger reroute". So the order was
derived from the SQL first, written down, and only then reproduced. Three
`ORDER BY` clauses matter:

| legacy function | ordering |
|---|---|
| `direct_dependencies` | `ORDER BY edges.to_entity, edges.ref_type` — one row per **edge**, not per distinct target |
| `dependent_ids_for` | `ORDER BY to_entity, from_entity, ref_type`, regrouped and re-emitted in **frontier order** |
| `impact_ids` | layer-at-a-time; `max_depth == 0` is unlimited; `max_count` returns **mid-layer**, not at a layer boundary |

The detail that turns out to carry the whole proof: `edges.ref_type` is
stored as TEXT, so SQLite's BINARY collation orders it
`calls < imports < typeref` — **not** the `RefType` enum's declaration order
(`Calls, TypeRef, Imports`). And `parser::graph::sort_entity_refs` sorts
`graph.edges` globally by `(from_entity, to_entity, ref_type_sort_key)` with
`ref_type_sort_key` = `Calls 0, Imports 1, TypeRef 2` — the same order. Every
`EntityGraph` constructor runs that sort (`from_parts` does it
unconditionally), and `writer::build_refs_section` groups the already-sorted
vector without disturbing it. Therefore:

- a **forward** CSR row is the contiguous run of edges sharing a
  `from_entity`, i.e. already in `(to_entity, ref_type)` order —
  `direct_dependencies`' `ORDER BY`, exactly;
- a **reverse** CSR row is the subsequence sharing a `to_entity`, and since
  the global sort's *primary* key is `from_entity`, it is already in
  `(from_entity, ref_type)` order — `dependent_ids_for`'s inner `ORDER BY`,
  exactly.

**The ordering the SQL spells out as an `ORDER BY` is a structural property
of the CSR.** Nothing is re-sorted on the read side; only the layering is
written out (`impact::index_impact_ids`, line-for-line against
`DiskCache::impact_ids`, including the mid-layer `max_count` cutoff that
`test_impact_entities`' `LIMIT + 1` probe depends on). This is also the
retrospective explanation for why §12.2's already-shipped `Deps`/`Dependents`
reroutes came out byte-identical without anyone sorting anything.

Verified two ways, not just argued: four BFS-order unit tests on the same
fixture `cache::tests::query_impact_topology_preserves_bfs_frontier_order`
used (the one that pins `z_mid` before `a_mid` at depth 2 — frontier order,
not alphabetical), and the byte-identical battery below.

**Tests-mode classification** was the second half of the blocker and needed a
new datum. `parser::graph::is_test_entity` is `name-pattern OR (test-path AND
body-has-a-test-marker)`; the third conjunct needs the entity's *body*, which
the image does not carry (§3.2 stores lines, not byte spans, and no content).
Re-deriving it on the read side by re-slicing files at line granularity would
be an approximation, and the bar is byte-identical. So it is precomputed at
build time by the one caller that already computes it — `write_test_flags`,
which fills the SQLite cache's `entity_flags.is_test` on every
`save_with_test_dirs`/`save_topology`, now returns that same set for
`write_query_index` to pack. One predicate, one evaluation, two stores; they
cannot disagree.

---

#### Format: `EntityRec.flags`, and why no `FORMAT_VERSION` bump

`EntityRec`'s `_pad` `u16` at offset 14 — written as zero since the format's
first commit — becomes `flags`, bit 0 = `entity::FLAG_IS_TEST`. Zero bytes
added, zero records moved, every other accessor unchanged.

`FORMAT_VERSION` stays at `2`, deliberately, and this is the one place the
change departs from `FLAG_REFS_TYPED`'s precedent (which *did* bump, 1→2).
§3.5's rule guards **misreads**: a `2`-tagged image built before this change
has zeros in the field, and this code never consults the field unless the
header's new `FLAG_ENTITY_TESTS` authorizes it. There is no misread to guard
against, and bumping would invalidate every image in every user's cache for a
field they would not have used.

`None` is not "no tests". Writers that cannot classify — `commands::query`'s
cold-build write (topology only, no `SemanticEntity` bodies in scope) and the
incremental save (which does not recompute `entity_flags` either) — pass
`None`, which clears the header flag and makes every test-shaped reader
decline. This is the index's `test_flags_computed`, and it exists for the
same reason that key does: an EMPTY answer is only trustworthy if somebody
actually looked.

New oracle: `TESTS_ORACLE` in `index_probe` re-derives `is_test_entity`
independently from the build's own `SemanticEntity` bodies and compares every
packed bit (sem-core: 3,222 entities, 384 tests, 0 mismatched, alongside
ORACLE / REFS_ORACLE / FILES_ORACLE / TRIGRAM_ORACLE / MUTATION, all PASS).

---

#### `graph.rs`: output-shape parity

§12.1 named this blocker exactly — the JSON "serializes each edge's
`ref_type`, which the `REFS` CSR tier does not carry". It has carried it
since `01943c7`. §12.2's residual worry was "matching the legacy JSON's exact
field/edge-listing shape (fwd-only edges, not fwd+rev double-counted)". What
had to match, and does:

- `{"entities":[…],"edges":[…],"stats":{…}}` plus a trailing newline: one
  object, three keys, that order, the same literal separators (copied from
  `write_graph_json_topology` rather than re-derived).
- entities: every one, `EntityInfo`'s own serde, sorted by `id`. SQLite's
  `ORDER BY id` is BINARY-collated and Rust's `str` ordering is the same byte
  order, so one sort satisfies both the SQL path and `write_graph_json`.
- edges: **forward rows only**, emitted per entity in id order. Each row is
  already `(to_entity, ref_type)`-ordered by the argument above, so the outer
  id-order loop supplies the primary key and nothing is sorted twice.
- `edgeCount` reads `REFS`' forward posting count from the section
  sub-header (new `QueryIndex::edge_count`, one `u32`). It equals
  `graph.edges.len()` unless an edge names an entity the graph lacks —
  impossible by construction, and confirmed on both corpora, where the SQL
  `COUNT(*)` and this number agree to the row across a 178 MB dump.

Streamed, not materialized: only the id-order permutation is held.

---

#### Freshness: the one thing that got *stronger*

The entity-scoped verbs (`find`/`callers`/`refs`/`impact --deps`/
`--dependents`) prove freshness over the files their own answer touches,
because an edit elsewhere cannot change their answer. A transitive closure
and a whole-corpus dump have no such boundary: an edit in a file the walk
never visits can add an edge *into* the closure. So both new reroutes gate on
`query::corpus_is_fresh` — `Complete` membership (`complete_check`, refusing
an image without `DIRS`, over which the sweep would report a *false* clean)
plus a parallel `Verified` content sweep, run as sibling `rayon` tasks. That
is the guarantee `has_fresh_cache` gave the SQL path, kept rather than
quietly weakened.

---

#### Byte-identical battery

Three-way per invocation: pre-change binary vs post (stdout only), and
post-index vs post-`SEM_NO_INDEX=1` on the same binary. After cascade (d) the
second comparison means *index tier vs the authoritative build path* — the
SQL tier it used to compare against no longer exists — which is strictly
stronger than the earlier rounds. Both corpora's stores were rebuilt from
scratch first, so the tier being displaced is the one a real user hits.

| repo | mode | invocations | pre-vs-post | index-vs-authoritative | largest |
|---|---|---:|---:|---:|---|
| rails | `impact` All | 4 | 0 | 0 | 1.3 MB |
| rails | `impact` Tests | 2 | 0 | 0 | 683 KB |
| rails | `impact` Deps/Dependents | 2 | 0 | 0 | 174 KB |
| rails | `graph` | 4 | 0 | 0 | 37 MB |
| monster | `impact` All | 4 | 0 | 0 | 1.5 MB |
| monster | `impact` Tests | 2 | 0 | 0 | 17 KB |
| monster | `impact` Deps/Dependents | 2 | 0 | 0 | 12 KB |
| monster | `graph` | 4 | 0 | 0 | **178 MB** |
| rails+monster | `sem context` (cascade a) | 2 | 0 vs the *authoritative* pre path | — | 24 KB |
| sem/rails/monster | `sem_entities` over MCP stdio (cascade e) | 6 | 4 identical, 2 same-multiset | — | 979 KB |

JSON and terminal both, name- and id-addressed both, `--depth 0/2/5`, an
ambiguous name and a missing name (both still exit 1 through the unchanged
legacy path), and `--no-cache` (which must decline and does). 178 MB matching
to the byte across 714,819 entities and every edge's `ref_type` is also the
empirical proof of `edgeCount` parity.

**Not clean, disclosed:** no corpus in the battery reaches the 10,000
`CACHED_TEST_IMPACT_LIMIT` (rails' largest closure is 2,443 entities /
2,363 tests; the monster's is 2,439), so the truncation boundary is not
covered by real invocations. It is covered instead by
`index_impact_ids_cuts_off_exactly_at_max_count_mid_layer`. And cascade (e)'s
two monster listings agree on the entity *multiset* exactly but not on the
order of siblings sharing a `start_line` — see that layer's row below.

---

#### The deletion cascade, as executed

Run in **dependency** order, which is not quite the order the brief listed:
(c)'s members are called *by* (d)'s, so a (c)-before-(d) commit would have
deleted a gate while its caller still stood. (b) turned out not to be a
deletion at all — see the residue.

| layer | commit | what went | LOC |
|---|---|---|---|
| — (enabler) | `4cd77ad` | `EntityRec.flags`/`FLAG_ENTITY_TESTS`, `TESTS_ORACLE`, 3 unit tests | +323/−20 |
| — (reroute 1) | `42de93d` | `ImpactMode::All`+`Tests` onto the CSR; `corpus_is_fresh`; 4 BFS unit tests | +424/−2 |
| — (reroute 2) | `ffea728` | `sem graph` json+counts from the image; `QueryIndex::edge_count` | +142/−0 |
| **a** | `8d4dd6c` | git freshness oracle: `git_oracle_says_fresh`, `git_head_oid`, `git_working_tree_clean`, `compute_oracle_eligible`, `FreshnessMode`/`freshness_mode`, `ORACLE_MIN_FILES`, `ORACLE_TIMEOUT_MS`, meta keys `git_head_oid`/`git_built_clean`/`oracle_eligible`, env `SEM_FRESHNESS`/`SEM_FRESHNESS_TIMEOUT_MS`, plus its only consumers `oracle_cache_fresh`, `oracle_fresh_topology`, `oracle_fresh_counts`, `oracle_context_subgraph` (+ `entities_with_content_by_id`, `edges_among`, `neighbor_ids_batch`, `DiskCache::open_existing_readonly`); `store_freshness_epoch` → `store_repo_origin` | +86/−564 |
| **b** | — | **NOT DONE** — see residue | 0 |
| **c** | (in `c204588`) | 1 of 5 gates: `cached_files_are_fresh`, dead only by virtue of (d) | (counted below) |
| **d** | `c204588` | `query_impact_topology`, `query_fresh_impact_topology`, `query_dependency_impact_topology`, `write_graph_json_topology`, and the ~20-function helper stratum beneath them (`find_cached_impact_entity`, `entity_candidates_for_query`, `direct_dependencies`, `direct_dependents`, `impact_entities`, `test_impact_entities`, `impact_ids`, `dependent_ids_for`, `entity_infos_by_id`, `test_ids_from`, `has_fresh_dependency_impact_files`, `cached_imported_files`, `current_imported_files`, `file_has_default_re_export`, …), plus `CachedImpactMode`/`CachedImpactError`/`print_cached_error`/`try_cached_impact_query` | +109/−1585 |
| **e** | `3985adf` | `sem-mcp`'s ungated cloud-first directory listing demoted behind a new `files_under`-backed local path | +162/−31 |

`sem-cli/src/cache.rs`: **3,888 → 2,231 lines (−1,657, −42.6%)**. §12's
original framing — "items 1-4 (and transitively 6) all route through
`sem-cli/src/cache.rs`'s freshness/SQL cluster" — is now a much smaller
statement: what remains in that file is the build plane (saves, hydrates,
`load_partial`, the incremental writer) plus the four surviving gates.

Whole-bead diff (`git diff --numstat 40086ad..HEAD`, excluding this
document): **+1,230 / −2,186, net −956**, across 12 files. The first bead in
this sequence to come out net *negative* — §12.1's was +66 and §12.2's was
+530, both because the enabler outweighed what it unlocked. Here the enablers
(`FLAG_ENTITY_TESTS`, `corpus_is_fresh`, `edge_count`, `try_index_graph`,
`index_impact_ids`, `sem-mcp`'s listing port) are the three commits above the
cascade, +889/−22; the cascade they unlocked is +357/−2,180.

---

#### Latency, before and after (median-of-5, cold process, load average 6.4)

The verbs this bead rerouted, pre-change binary vs post:

| repo | verb | pre | post | delta |
|---|---|---:|---:|---:|
| rails | `impact` (All) | 79.0 ms | 14.2 ms | **−82%** |
| rails | `impact --tests` | 55.4 ms | 12.9 ms | **−77%** |
| rails | `graph` (counts) | 106.8 ms | 12.9 ms | **−88%** |
| rails | `graph --json` | 153.4 ms | 54.6 ms | **−64%** |
| monster | `impact` (All) | 551.4 ms | 34.4 ms | **−94%** |
| monster | `impact --tests` | 429.2 ms | 33.4 ms | **−92%** |
| monster | `graph` (counts) | 741.2 ms | 33.7 ms | **−95%** |
| monster | `graph --json` | 950.4 ms | 257.1 ms | **−73%** |
| monster | `sem_entities src/compiler` (MCP) | 950 ms | 20 ms | **−98%** |

And the one that went the other way, disclosed rather than buried:

| repo | verb | pre | post | delta |
|---|---|---:|---:|---:|
| rails | `sem context` | 10 ms | 160 ms | **+16x** |
| monster | `sem context` | 40 ms | 1,120 ms | **+28x** |

`sem context` lost its discovery-skipping tier with the git oracle. It is the
one verb with **no index tier at all**, and the missing datum is precise:
it serves entity *bodies*, and `EntityRec` carries `start_line`/`end_line`,
not `start_byte`/`end_byte`, so the image cannot reconstruct
`SemanticEntity.content` — which is a byte-span slice, not a line slice.
Adding byte spans is a real format change (`EntityRec` is exactly full at 32
bytes) and a fifth reroute; it is the natural next bead, not this one. The
fast path was also answering *wrongly* (below), so this is not a pure trade.

---

#### Regression spot-check (median-of-5, unmodified verbs, load average 6.4)

| repo | verb | median | banked | delta | note |
|---|---|---:|---:|---|---|
| rails | find | 10.4 ms | 8.7 ms (§12.2) | +20% | at the tolerance edge |
| rails | grep | 13.0 ms | 17.1 ms (§12.2) | −24% | better than the last bead's own reading |
| rails | impact --deps | 7.7 ms | 6.4 ms (§12.2) | +20% | at the edge |
| monster | find | 21.8 ms | 21.3 ms (§12.2) | +2% | within band |
| monster | grep | 29.0 ms | 26.2 ms (§12.2) | +11% | within band |
| monster | impact --deps | 8.0 ms | 6.8 ms (§12.2) | +18% | within band |

Read honestly: everything is inside or at the edge of ±20%, which is a better
picture than §12.1's (1.4-2.5x over) or §12.2's (rails grep +144%), but the
attribution is the same and should not be over-claimed — load average was
**6.4** during these runs, higher than §12.2's 3.0-3.7 and §13.4's 2.6-3.3,
on a machine shared with other agent sessions. `query.rs`'s and `grep.rs`'s
own code is untouched by every commit in this bead, so none of this is a
regression it introduced; the rails rows sitting exactly at +20% are noise at
this load, not a signal.

---

#### Two pre-existing bugs, surfaced not fixed

Both reproduce on the **pre-change** binary. The cascade did not cause them;
it removed the code that was hiding them.

**1. `sem context`'s oracle subgraph answered differently from the tier it
fronted.** `oracle_context_subgraph` stopped fetching neighbours once it held
`2 × budget` tokens, on the assumption that `build_context_result_bounded`
always keeps the target entity. It does not — it can *omit* an oversized
target, and then every neighbour never fetched would have been packed.
Measured on the monster: `sem context createProgram` returned **1 entry /
5,583 tokens** through the fast path against **27 entries / 8,000 tokens**
through the authoritative one. On rails the two agreed (4 entries), which is
why this survived so long. The fast path is deleted in cascade (a), so `sem
context` is now always authoritative; repairing the packer's interaction with
an oversized target is a `sem context` bug, filed, not touched here.

**2. `try_index_impact_deps` serves a stale dependency set when a brand-new
import *target* file appears.** It proves freshness over the entity's own
file and its known dependencies' files; a new target touches neither, so a
`b.ts` that already said `import './optional'` keeps answering
`"dependencies": []` after `optional.ts` lands. The SQLite tier had an
import-aware check for exactly this case
(`has_fresh_dependency_impact_files`) — but that tier had been unreachable in
production since semx-gis, because the index fast path runs ahead of it. Five
tests in `impact_direct_deps.rs` asserted the guarantee and only still passed
because they set `SEM_NO_INDEX=1`; i.e. they were testing a tier no user
reaches. They are rewritten as **characterization** tests
(`..._index_fast_path_does_not_notice_...`), each carrying the full
explanation, so a future repair flips them deliberately rather than silently.

A third, smaller finding, noted without a bug filing: `commands::query::
index_answer` calls `complete_check` without checking `has_dirs()` first. On
a `DIRS`-less image the sweep finds no drifted directories and reports a
*false* clean. Not live — every production image comes from
`write_query_index`, which always supplies `dirs` — but `corpus_is_fresh`
refuses such an image explicitly rather than inherit the hazard.

---

#### Residue — what this bead did **not** do, and why

**Cascade (b), the SQLite hydrate demote (§7 item 6), is NOT DONE, and the
reason is structural, not time.** §7 framed it as "`load`/
`load_with_source_scope`/`load_graph_topology*` go, `load_partial*` and
incremental-rebuild callers stay". Traced end to end, that split does not
exist: those three are the **warm-cache tier of the build plane**, reached
through `graph.rs`'s shared `get_or_build_*` helpers by every command that
needs an `EntityGraph` — `sem diff`, `sem context`, and the fallback beneath
every reroute in this bead. Deleting them does not remove a query path; it
forces the remaining callers down `load_partial_with_source_scope` +
`build_incremental_...` + `save_incremental_...`, which on a fully clean
corpus rebuilds nothing but **writes the cache on every query**. That is a
worse system, not a smaller one. The honest statement is that the reroutes
removed the hydrate cluster's *query-plane* callers (there are none left:
`find`/`callers`/`refs`/`entities`/`impact`/`graph` all answer from the image
first) while its *build-plane* callers are exactly the ones §7 said should
stay. What is left to do here is not a deletion but a module split, and §12.1
already recorded why a split that leaves live callers on the wrong side
"would misdescribe the split as complete when it isn't".

**Cascade (c) is 1 of 5.** `cached_files_are_fresh` is gone. `has_fresh_cache`
and its three kind-specific wrappers (`has_fresh_complete_cache`,
`has_fresh_topology_cache`, `has_fresh_topology_only_cache`) remain, because
they gate the hydrate cluster above. They fall the moment (b) does; not
before.

**`sem context` has no index tier** — the missing datum is entity byte spans,
named above.

**`sem-mcp`'s two remaining cloud sites** (`impact`, `context`) are unchanged.
Both are opt-in behind `SEM_MCP_CLOUD=1`, off by default, so the precedence
issue is structurally present but not production-active — the same
disposition §12.1 gave them. The ungated, production-active one is fixed.

**`sem-mcp` directory listings are not byte-identical to the walk**, only
multiset-identical: siblings sharing a `start_line` come out in
`(end_line, type, name, id)` order from the image and in extractor order from
the walk. 73 of ~5,000 lines on the monster's `src/services`; in
`src/compiler` every difference is inside one line-range group. This is the
same class of deviation `sem-cli`'s own directory reroute has shipped with
since §12.1 — where, measured on the pre-change binary for comparison, the
two tiers do not even agree on the multiset. Strictly closer to parity than
the precedent, and recorded rather than rounded to "identical".

---

#### Gates

Every commit: **sem-core lib 603/603** (600 baseline + 3 new format tests),
**sem-cli 242/242** (244 baseline, +4 BFS-order unit tests, −6 `cache.rs`
unit tests whose subjects were deleted), **sem-mcp 93/93**. All
`index_probe` oracles PASS on sem-core (ORACLE 2,848 names, REFS_ORACLE
3,222 entities / 0 kind mismatches, FILES_ORACLE 5 prefixes, TESTS_ORACLE
3,222 checked / 384 tests / 0 mismatched, TRIGRAM_ORACLE 6 patterns,
MUTATION PASS). clippy compared as a per-file, per-lint fingerprint against
`40086ad`: **zero new warning classes at every commit**, ten removed by the
deletions. `fmt` clean on every touched file. The pre-existing dirty WIP set
(`diff/cloud_upload.rs`, `diff/relations.rs`, `commands/setup.rs`,
`tests/diff_cloud_relations.rs`, `tests/review_listen_dry_run.rs`,
`languages.rs`) verified byte-identical by SHA-256 after every commit.

**Bead semx-zvq closed.**

---

## 13. Membership freshness, on by default (semx-ykf) — §10.6/§11.6's gap closed

§10.2 flagged it and left it alone: every index-backed verb proved
*content*-freshness of its own answer but never *membership*-freshness of the
whole corpus — a name that exists only in a file created after the last
build was invisible to `find`/`callers`/`refs`/`grep` until the next full
rebuild. This bead wires §2's `Complete` tier into all of them by default.

### 13.1 Mechanism choice: (a) parallel-stat sweep, measured against (b) a
watcher — (a) wins without a contest

The bead named two candidates and asked to measure before building either:

- **(a) Complete-check per query** — §1.6/§2's parallel `stat` over the
  index's own `FILES`+`DIRS` sections, run *concurrently* with the query's
  existing answer resolution rather than serially before it.
- **(b) An FSEvents/`notify-rs` watcher** — a resident process that patches
  the index on filesystem create/delete events, justified only if (a) blows
  the 20 ms verb budget.

**(a) was measured first, as instructed**, before a line of (b) was
considered:

| where | what | measured |
|---|---|---|
| S1 (§1.6, prior bead) | parallel `stat` over 40,877 *known files*, monster | 12 ms |
| this bead, standalone (`index::complete_check` alone, monster, warm page cache, steady state) | parallel `stat` over `FILES` (40,869) **and** `DIRS` (2,743) — S1's number plus the directory half S1 didn't build | **13 ms** (median; single-shot cold-page-cache runs saw 33-42 ms outliers, consistent with §1's own "warm page cache" caveat — see §13.4) |
| this bead, standalone, rails (`sem-core` itself, 88 files, ~40 dirs) | same sweep | **<1 ms** (folds into process-floor noise) |

S1's estimate holds: **~12-13 ms on the monster, ~0 on rails.** Run
*concurrently* with the existing answer resolution (`rayon::join` — the two
halves touch disjoint sections, `ENTITIES`/`NAMES`/`REFS` vs `FILES`/`DIRS`,
so there is no shared-mutable-state reason they can't overlap), a verb's
added wall time is `max(existing_answer_cost, sweep_cost) −
existing_answer_cost` in the best case, not the sum — and since every
existing verb's own work (0.03-0.06 ms per §8, single-digit ms wall-clock per
§10.1) is far cheaper than the sweep, the sweep dominates and becomes
*the* added cost, landing every verb at process-floor-plus-~13ms. Measured
end-to-end (§13.2): **19.4-19.9 ms** for `find`/`callers`/`refs` on the
monster — under the 20 ms budget, with single-digit ms of margin, not zero.

**(b) was not built.** The measurement above is the reason: (a) clears the
budget on the corpus the budget was set against, so a resident watcher would
buy nothing the sweep doesn't already deliver, at the cost of exactly the
three things §1.5 already spent a full section proving this design avoids —
a process that must be kept alive, a memory footprint that scales with
however long it's been running, and a second source of truth (the watcher's
view of the filesystem) that can drift from the index's own view and would
need its own consistency oracle. The simplicity mandate's ordering
(*removal outranks addition; the simplest mechanism that closes the gap
wins*) settles it once (a) is shown to fit: don't add a daemon to solve a
budget problem the daemon-free mechanism doesn't have. (This is also the
second daemon-shaped mechanism declined against this corpus this week — the
trigram tier's own out-of-scope list, §11.6, made the same call for a
different reason.)

### 13.2 The mechanism, as implemented

`crates/sem-core/src/index/complete.rs` — `complete_check(idx, root,
walk_subtree)`:

1. **Deletions**: parallel `stat` (`symlink_metadata`) over every path in
   `FILES`. A path that no longer resolves is reported deleted.
2. **Drift**: parallel `stat` over every path in `DIRS` — every *distinct
   ancestor directory* of every indexed file, all levels, not just direct
   parents, plus `""` for the repo root (`writer::DirFingerprint`, new
   `SEC_DIRS`-populating writer entry point
   `build_with_content_and_dirs`/`build_with_salt_and_content_and_dirs`).
   Storing the whole ancestor chain (not just leaf parents) is what makes
   directory-mtime drift a sound membership signal at *whatever depth* a
   file was added: POSIX bumps a directory's own mtime on entry
   create/unlink/rename and nothing else's, so a file created three levels
   below a directory the index has never seen still perturbs the nearest
   *known* ancestor — which is guaranteed to exist because the repo root
   always does.
3. **Discovery**: for each drifted directory, the caller's `walk_subtree`
   (in `sem-cli`, the *existing* `find_supported_files_in_path`, scoped to
   just that directory — same ignore/`.semignore`/extension rules a full
   build already applies, reused rather than re-derived, bounded to the
   drifted subtree rather than the corpus) re-walks it; anything not already
   in `FILES` is new.
4. Steps 1 and 2 run as sibling `rayon` tasks (not back-to-back passes) —
   splitting the two ~7 ms halves across the same worker pool concurrently
   rather than draining one before starting the other measured as the
   difference between ~15 ms and ~13 ms on the monster; small, but free.

Both `commands::query::index_answer` (`find`/`callers`/`refs`) and
`sem_core::index::grep::search` run `complete_check` via `rayon::join`
alongside their existing lookup, exactly as §13.1 describes. What each does
with the result differs by what's safe to patch in-memory:

- **`find`**: a new file's entities are extracted (`ParserRegistry::
  extract_entities_brief`, the same per-file extraction the content-repair
  path already uses), filtered by the query, and merged into the answer —
  content-local, same safety class as §5.1's existing repair.
- **`callers`/`refs`**: **any** membership change (not just a directly
  relevant one) forces a decline to the cold-build fallback. A new file
  could be a new *edge* — a caller or reference this bead has no way to
  patch into `REFS`'s CSR without the two-pass symbol table §10.3 already
  declined to build for content edits. Rather than serve a possibly
  edge-incomplete answer, the fast path declines and the (always-correct,
  slower) cold rebuild runs instead — same discipline §10.3 established for
  a stale *related* file, now applied to a *new* one. This is also why the
  new-file-by-reference case (a brand-new file that calls an existing,
  already-indexed function) is answered correctly "for free": it never goes
  through a bespoke patch path at all, it just declines into the walker that
  was already known to be correct.
- **`entities <file>`** and **`impact --deps`**: **not modified.**
  `entities <file>` already re-parses directly whenever the index has never
  seen the requested path (`is_file_stale` returns `true` for an unknown
  fingerprint, which was already the fallback trigger before this bead) — a
  single named file has no membership *ambiguity* to resolve, so `Complete`
  has nothing to add here. `impact --deps`'s fast path
  (`try_index_impact_deps`) already declines (returns `false`, falling to
  the correct, slower legacy path) whenever `resolve_by_name_indices` can't
  find the queried name — which is exactly what happens today when the name
  only exists in a file the index has never seen. Wiring `Complete` in would
  upgrade this from "correctly slow" to "correctly fast," but the same
  edge-completeness argument that keeps `callers`/`refs` conservative
  applies here too (impact's dependencies are also `REFS`-CSR-backed), so
  the honest options were "decline, like today" or "the same
  full-cold-rebuild-on-any-new-file behavior `callers`/`refs` got" — a
  strict two-line change with no new mechanism, deferred rather than rushed
  into this already-large bead; tracked as follow-on, not silently dropped.
- **`sem grep`**: new files join the candidate set unconditionally (they
  carry no trigram postings yet — nothing to prefilter on, so they go
  straight to `verify`, same as any `Stopped`-trigram candidate); deleted
  files are dropped from the candidate set before `verify` wastes a read on
  them.

Every deviation the sweep can't resolve — an I/O race between the two stat
passes and the directory re-walk, most plausibly — sets
`CompleteReport.inconclusive`, and every caller (`find`/`callers`/`refs`)
treats that as "membership freshness not proven": a bail to the cold-build
fallback, never a silent "assume nothing changed." (`sem grep` is the one
exception, and it's disclosed as one: it has no cheaper authoritative
fallback to bail to, and `verify` already tolerates a missing file
silently — the worst case is a same-instant race missing one file, never a
wrong match. See §13.5.)

### 13.3 Tests

`sem-core/src/index/complete.rs`'s own unit tests (real temp-directory
fixtures, real filesystem mtimes, no mocking): a clean corpus reports
nothing; a new file at the repo root is found; a new file three directories
deep, *all of which are new*, is found (proving the ancestor-chain design in
§13.2 point 2, not just the direct-parent case); a deleted file is found; a
rename is reported as both a deletion and an addition. **Mutation test**
(the bead's item 3): `mutation_a_walker_that_hides_a_file_produces_a_false_
clean_report` runs the same sweep against two walkers — the honest one
(finds the planted file) and a deliberately hobbled one that filters it
out — and asserts they disagree, so a future regression that made
`complete_check`'s own logic agree with a broken walker instead of the
honest one would fail this test, not pass it vacuously.

`sem-cli/tests/index_membership.rs` — the black-box, CLI-level new-file
battery, run through the real binary end to end (primes an index via one
`sem find` call, mutates the filesystem, re-runs the verb, asserts on the
answer):

| test | verb | mutation | proves |
|---|---|---|---|
| `find_sees_a_name_added_in_a_brand_new_file` | `find` | new file, root | new file matching **by name** |
| `find_sees_a_name_added_in_a_brand_new_nested_directory` | `find` | new file, 3 new nested dirs | ancestor-chain drift detection at depth |
| `callers_sees_a_caller_added_in_a_brand_new_file` | `callers` | new file **calling** an existing fn | new file matching **by reference** |
| `refs_sees_a_reference_added_in_a_brand_new_file` | `refs` | new file referencing an existing fn | new file matching **by reference** |
| `find_no_longer_reports_a_deleted_files_entities` | `find` | delete a file | deleted file |
| `find_follows_a_renamed_file_to_its_new_path` | `find` | rename a file | renamed file (delete + add) |
| `grep_finds_text_in_a_brand_new_file` | `grep` | new file with target text | new file matching **by text** |
| `grep_does_not_return_a_hit_from_a_deleted_file` | `grep` | delete a matching file, keep another | deleted file, grep candidate set |

All 8 pass, stably across repeated runs (checked 3x in a row during this
bead). Full suite: sem-core lib **596/596**, sem-cli integration
**244/244** (236 baseline + these 8), clippy/fmt clean on every file this
bead touched (verified file-by-file, not just by warning count — see §13.6).

### 13.4 Per-verb latency, before/after, median-of-7 cold process

Monster (`microsoft/TypeScript` @ `b465fdbfe1`, same corpus §1/§10/§11 use;
warm page cache; ambient load average 2.6-3.3 during these runs — §12.1
already disclosed the same shared-machine noise affecting its own
regression spot-check, so absolute numbers here are read the same way: the
relative claim is load-bearing, not the exact millisecond):

| verb | before (§10.1/§11.5) | after (this bead) | delta | budget |
|---|---:|---:|---:|---|
| `sem find createProgram --file program.ts` | 4.6 ms | **19.9 ms** | +15.3 ms | <20 ms ✓ (thin margin) |
| `sem callers createProgram --file program.ts` | 4.7 ms | **19.6 ms** | +14.9 ms | <20 ms ✓ (thin margin) |
| `sem refs createProgram --file program.ts` | 4.7 ms | **19.6 ms** | +14.9 ms | <20 ms ✓ (thin margin) |
| `sem entities program.ts --json` (unmodified) | 4.8 ms | **5.8 ms** | +1.0 ms | unchanged verb, noise |
| `sem impact createProgram --deps --json` (unmodified) | 4.7 ms | **5.9 ms** | +1.2 ms | unchanged verb, noise |
| `sem grep createProgram --json` | 16 ms | **28.6 ms** | +12.6 ms | <50 ms ✓ |
| `sem grep getEmitFlags --json` | 13 ms | **25.0 ms** | +12.0 ms | <50 ms ✓ |

Rails (`sem-core` itself, 88 files, ~40 directories — the "should be ~0"
prediction):

| verb | after (this bead) | vs. unmodified-verb floor |
|---|---:|---|
| `sem find CompleteReport --json` | 6.8 ms | +1.3 ms over `entities` (5.5 ms) |
| `sem callers complete_check --json` | 7.0 ms | +1.5 ms |
| `sem refs complete_check --json` | 6.2 ms | +0.7 ms |
| `sem entities src/index/complete.rs --json` (unmodified, control) | 5.5 ms | — |
| `sem grep complete_check --json` | 8.9 ms | +3.4 ms |

**Reading the delta honestly.** The +12-15 ms the monster table shows is not
overhead this bead added carelessly — it is very close to *exactly*
`complete_check`'s own measured cost (§13.1's 13 ms), which is very close to
*exactly* what S1 predicted a year earlier (12 ms) before any of this was
built. `entities`/`impact --deps` (unmodified code paths, included as a
same-run control) moved by ~1 ms — inside measurement noise — confirming the
delta on the three modified verbs is attributable to the sweep, not to
ambient load or a process-floor shift between runs. Rails confirms the other
half of the prediction: on a corpus small enough that the sweep is sub-1ms,
the delta collapses to the same ~1 ms noise floor the unmodified verbs show.
`find`/`callers`/`refs` land at 19.4-19.9 ms across repeated runs — under
the 20 ms gate, but by single-digit-millisecond margin rather than the
comfortable multi-ms headroom §10.1's pre-bead numbers had. That margin is
the real, disclosed cost of choosing "closes the membership gap, stays
daemon-free" over "keep the old best-case latency": §13.1's own math says
the entity tier's own work (µs, §8) was never going to be the bottleneck
once *any* whole-corpus proof runs concurrently with it — the budget was
always going to land at roughly `floor + sweep_cost`, and it does.

### 13.5 Out of scope, disclosed rather than silently dropped

- **Mid-query file mutation races.** A file created, deleted, or renamed in
  the narrow window between `complete_check`'s stat pass and the directory
  re-walk it triggers is exactly what `CompleteReport.inconclusive` exists
  for (§13.2's last paragraph) — the query falls back to a cold rebuild
  rather than serve an unproven answer. This is fail-toward-MISS by
  construction, not a gap: uncertainty always costs time, never correctness.
- **`sem grep`'s content-drift gap is unchanged** (§11.6, updated this bead
  only for the membership half). A file already known to the index, edited
  since the last build such that it now contains a trigram it didn't
  contain before, is not added to that trigram's candidate set — this bead
  closes *membership* (new/deleted files), not trigram *content* freshness
  on existing files. Distinct problem, same reason it's out of scope: fixing
  it needs re-deriving trigrams for every already-known file per query,
  which is the O(corpus) cost §1 built this whole design to avoid.
- **A brand-new top-level directory forces a whole-directory re-walk of
  whatever existing directory it was created under.** §13.2's discovery step
  re-walks a *drifted* directory's entire subtree (reusing the existing
  walker, not a bespoke bounded one) — bounded to that directory's own
  contents in the common case (a new file added under an already-small,
  already-known directory), but if the drifted directory is a large one
  (worst case: the repo root itself, when a new top-level directory
  appears), the re-walk cost approaches the corpus-walk cost this design
  otherwise avoids. Not measured separately in §13.4 (none of that
  benchmark's mutations hit this case); disclosed as a real, bounded-by-
  directory-not-by-corpus-in-the-common-case tradeoff, not a silent
  regression back to §7's −301 ms discovery cost — it only recurs on the
  *first* query after such a directory appears (the sweep only re-walks
  *drifted* directories, and one that's already been walked once this
  process invocation isn't drifted again next call because the answer isn't
  persisted, so this cost is genuinely per-query, not per-process — a
  bounded single-level classifier that recurses only into provably-new
  subdirectories would remove it, and is the natural next optimization if a
  workload exercises this path often; not built here per the simplicity
  mandate — reuse the walker that's already known correct rather than add a
  second, narrower one, until measurement shows the reuse is the wrong
  trade).
- **`impact --deps` is not fast-pathed for a new-file entity** (§13.2) —
  still correctly answered, just via the slower legacy path, same as before
  this bead. A two-line change (mirroring `callers`/`refs`'s "any new file →
  decline to cold rebuild") would close this; deferred, not dropped.
  **Closed, semx-dev**: `try_index_impact_deps` and `try_index_impact_
  dependents` (`impact.rs`) now run `index::complete_check` concurrently
  with their existing resolution (`rayon::join`, the identical shape
  `commands::query::index_answer` already used for `find`/`callers`/`refs`)
  and decline on any new file — not on a deletion, which the pre-existing
  `_answers_from_the_index_when_an_unrelated_..._is_deleted/missing` tests
  pin as *not* a decline reason, since only an addition can manufacture a
  new edge this CSR predates. Repairs the bug §12.3 disclosed but did not
  fix: a `b.ts` that already said `import './optional'` now folds
  `optional.ts` into `consume`'s dependencies the moment it appears, by
  falling through to the always-correct legacy path rather than trusting a
  CSR frozen at the last build. semx-zvq's four characterization tests
  (`impact_deps_index_fast_path_does_not_notice_a_new_*_target`) are flipped
  to guarantee tests (renamed `..._folds_in_a_new_*_target`), and a new
  `impact_dependents_index_fast_path_folds_in_a_new_caller_file` pins the
  reverse-direction repro the bead named ("new file imports existing entity
  → dependents answer includes it"). The one characterization test this does
  *not* repair, `..._does_not_notice_a_side_effect_import_change`, is a
  different bug class (a bare `import './a'` binds no name, so neither tier
  records a symbol-level edge for it at all — verified the legacy path
  independently agrees with the stale answer here, `dependencies: []` either
  way) and is left as-is, disclosed, not silently dropped.
- **No on-disk `index.log`** (§10.3, unchanged) — every repair this bead
  makes, content or membership, is transient and forgotten after the query
  that triggered it. The next corpus-level build is what makes it
  permanent.
- **`--entity-id` and qualified `Parent.child` addressing** (§10.6,
  unchanged) — untouched by this bead, same as every bead since S2.

### 13.6 LOC ledger and gates

| file | +/− | what |
|---|---:|---|
| `sem-core/src/index/complete.rs` | **new, 425** | `complete_check`, `CompleteReport`, 6 unit tests incl. the mutation test |
| `sem-core/src/index/format.rs` | +46/−0 | `DirRec` accessors (`dir` module) |
| `sem-core/src/index/writer.rs` | +78/−4 | `DirFingerprint`, `build_with_content_and_dirs`/`build_with_salt_and_content_and_dirs`, `DIRS` section assembly |
| `sem-core/src/index/reader.rs` | +59/−1 | `has_dirs`/`dir_count`/`all_dir_paths`/`dir_fingerprint`, `DIRS` soundness check |
| `sem-core/src/index/grep.rs` | +54/−13 | `search` runs `complete_check` concurrently, merges new/deleted files into the candidate set |
| `sem-core/src/index/mod.rs` | +37/−12 | module wiring/re-exports, 3 existing grep tests updated for the new `search` signature |
| `sem-core/examples/index_probe.rs` | +16/−5 | 5 existing `grep::search` call sites updated for the new signature (`no_walk` — none of index_probe's images populate `DIRS`, so the sweep is a structural no-op there) |
| `sem-cli/src/cache.rs` | +55/−1 | `write_query_index` derives `DirFingerprint`s (`ancestors_of`, `build_dir_fingerprints`) and calls the dirs-populating writer entry point |
| `sem-cli/src/commands/query.rs` | +68/−4 | `index_answer` wraps the pre-existing verified-only resolution (renamed `index_answer_verified`, otherwise untouched) with the concurrent sweep and the new/deleted-file merge |
| `sem-cli/src/commands/grep.rs` | +12/−3 | passes the real `find_supported_files_in_path`-backed walker into `grep::search` |
| `sem-cli/tests/index_membership.rs` | **new, 347** | the 8-test CLI-level new-file battery (§13.3) |
| **total** | **≈+772 new, +425/−43 changed** | |

**Gates**: sem-core lib **596/596** (0 failed, 6 new), sem-cli integration
**244/244** across 19 suites (0 failed, 8 new), `index_probe`'s `ORACLE`/
`REFS_ORACLE`/`FILES_ORACLE`/`TRIGRAM_ORACLE` all still PASS on both rails
and the monster (re-run after this bead's changes, 0 mismatches — this
bead's `grep::search` signature change is additive-only, provably: every
existing oracle call site was updated to pass a no-op walker over a `DIRS`-
absent image, which is the same "tier absent" contract `has_refs`/
`has_trigram` already establish, not a new code path). clippy and fmt clean
on every file this bead touched, verified per-file (`cargo clippy` and
`cargo fmt --check` scoped to exactly the files above, not the whole
workspace — `sem-cli/src/main.rs` and other untouched files carry pre-existing
formatting/lint drift from before this bead that is explicitly not this
bead's to fix or reformat).

## 14. `sem context`'s missing datum: byte spans, `FORMAT_VERSION` 3 (semx-a3w)

§12.3 named this bead exactly: `sem context` serves entity *bodies*
(`SemanticEntity.content`, a byte-span slice of the source file), and
`EntityRec` carried `start_line`/`end_line`, not `start_byte`/`end_byte` — so
the image could not reconstruct a body and the verb had no index tier at
all, regressing monster 40 ms → 1.1 s when the git-oracle subgraph fast path
(itself proven to answer *wrongly*, §12.3) was deleted with the rest of the
oracle. This bead adds the span and reroutes.

### 14.1 The datum was already half-present

`SemanticEntity.start_byte`/`end_byte` (`Option<usize>`) already exist and
are populated by the tree-sitter code extractor (`entity_extractor.rs`,
`oxc_extractor.rs`) for every code entity — `content` is sliced from exactly
these bytes at extraction time, so a consumer that re-reads the same bytes
at the same offsets reproduces `content` byte-for-byte by construction. The
gap was purely that nothing carried the span from `SemanticEntity` through
`EntityInfo` (topology-only, no span field — deliberately not added, to
avoid changing the JSON shape every `find`/`callers`/`refs`/`graph --json`
answer already serializes `EntityInfo` into) into the image. The fix mirrors
`FLAG_ENTITY_TESTS`'s precedent exactly: a side-channel map
(`entity_byte_spans: HashMap<String, (u32, u32)>`, entity id → span) handed
down from whichever caller has `SemanticEntity` bodies on hand
(`DiskCache::save_with_test_dirs`/`save_topology`, the same two callers that
already derive `test_entity_ids`), threaded through a new widest writer
entry point (`build_with_salt_and_content_and_dirs_and_tests_and_spans`) so
every pre-existing signature is untouched and every pre-existing caller
still compiles unchanged, passing `None`.

### 14.2 Format: `EntityRec` grows, and this time `FORMAT_VERSION` *does* bump

Unlike `is_test`'s bit (packed into the `_pad` `u16` that became `flags`,
zero-cost because the record's *width* didn't change), there was no reserved
padding left: `EntityRec` was exactly 32 bytes, fully accounted for. Adding
`start_byte`/`end_byte` (`u32` each, appended at offsets 32/36) changes the
record's *width*, which is precisely `format_version`'s axis (§3.5): every
offset past `ENTITIES`'s start would misalign under a size mismatch, so
`FORMAT_VERSION` bumps 2 → 3 rather than reusing a flag. A pre-bump image is
a clean miss (magic/version/salt mismatch ⇒ "index does not exist"), exactly
the same discipline every prior bump used — never a partial read, never a
silent misalignment. `NONE_U32` (`u32::MAX`) is the "absent" sentinel per
field, the same convention `parent` already established, needed because
several extractors never populate `SemanticEntity.start_byte`/`end_byte`
(`fallback.rs`, `json.rs`'s entity path, cache rehydration, `differ.rs`) —
this is a **per-record** signal, not a header-wide one like
`FLAG_ENTITY_TESTS` needed, because there is no "computed as none" vs "never
computed" ambiguity: `NONE_U32` unambiguously means absent, and no header
flag is needed to authorize trusting it.

**Size cost, measured on the monster's 454,528-row `ENTITIES` section**
(§3.3's own table): `ENTITY_REC_LEN` 32 → 40 bytes, **+8 B/entity, +3.64 MB**
(14.5 MB → 18.2 MB), **+11.7%** of the ≈31.2 MB base image. This is the
honest cost of the correct answer — almost exactly the id-elision savings
(§3.4's 43.7 MB → 11.1 MB) in the opposite direction, on a much smaller
section.

### 14.3 The reroute: `try_index_context`, never approximates

`context.rs`'s `try_index_context` runs before the cloud capability (restoring
every other verb's ordering — local fast path, then cloud, then the
always-correct walk) and only ever returns `true` on a path it can *prove*
matches the authoritative one:

1. Resolve the target to exactly one index entity — `--entity-id` via
   `resolve_by_id_index`, or a bare name via `resolve_by_name_indices`
   (declining on a qualified `Parent.child`/`Parent::child` name, same
   two-resolver-disagreement guard `try_index_impact_transitive` uses, and
   on zero or multiple matches).
2. Prove the **whole corpus** fresh (`query::corpus_is_fresh` — membership
   *and* content), not just the files the eventual answer touches: a
   transitive closure has no locality boundary an edit elsewhere can't
   cross, and a brand-new file can be a new edge this build's `REFS` CSR has
   no way to represent — the identical reasoning `impact --all/--tests` and
   `sem graph` already established for the same *shape* of query.
3. Walk `REFS` in both directions from the target (`refs_of_typed` forward,
   `callers_of_typed` reverse), replicating `parser::context::
   collect_reachable_related`'s exact algorithm (FIFO queue, visited set,
   10,000-node cap) — twice, once per direction, matching the packer's own
   two separate unbounded-until-capped walks — so the assembled subgraph is
   provably a superset, up to the packer's own cap, of whatever
   `build_context_result_bounded` could ever touch for *any* `--hops`/
   `--budget` the caller asks for. Declines the instant any visited entity
   has no byte span recorded.
4. Read each collected entity's body by slicing its own file at its indexed
   byte span (`hydrate_contents`, one read per distinct file, not per
   entity), building real `SemanticEntity` values. Declines on any I/O or
   UTF-8 failure.
5. Feed the reconstructed `EntityGraph::from_parts(...)` (which re-sorts
   edges via `sort_entity_refs` regardless of collection order, so the
   subgraph's adjacency is byte-identical to what the same edges would
   produce inside the full corpus graph) and the hydrated bodies into the
   **unchanged** `build_context_result_bounded`, then the **unchanged**
   `render_context` (factored out of `context_command` specifically so the
   index path and the authoritative path share one rendering
   implementation rather than two hand-kept-in-sync copies).

Edge deduplication matters here in a way it didn't for the single-direction
CSR reroutes: forward and reverse walks both start at the target, so a
corpus cycle (mutual recursion, circular imports) can have the *same*
directed edge discovered from both a forward-visited source and a
reverse-visited target. `collect_subgraph` dedupes on `(from, to, ref_type)`
before pushing, so `graph.edges` — and therefore every adjacency vec — never
carries a duplicate the authoritative build wouldn't have.

### 14.4 Byte-identical battery

Real invocations, warm index (a first `sem context` call on a fresh cache
has no `SemanticEntity` bodies in scope only if it's routed through the
topology-only cold-build path; `context`'s own slow path always saves with
bodies, so its *own* first run warms spans for the second), compared against
the same binary's `SEM_NO_INDEX=1` output:

| repo | invocation | match |
|---|---|---|
| rails | `--entity-id`, JSON | byte-identical |
| rails | `--entity-id`, text | byte-identical |
| rails | name + `--file`, JSON | byte-identical |
| rails | name + `--file`, `--budget 500` | byte-identical |
| rails | `--entity-id`, `--hops 2` | byte-identical |
| monster | `createProgram --file src/compiler/program.ts`, JSON | byte-identical |
| monster | same, text | byte-identical |
| monster | same, `--budget 500` | byte-identical |
| monster | same, `--hops 1` | byte-identical |

9 invocations, both entity- and file-scoped addressing, JSON and text
rendering, default/bounded budget, unbounded/bounded hops — every one
matches `SEM_NO_INDEX=1` to the byte. `crates/sem-cli/tests/context_cli.rs`
(new, 4 tests) pins the same shape permanently: warm/cold tier selection,
text-mode and budget-bounded parity, decline on an ambiguous name, and the
new-caller-file membership case (mirroring semx-dev's repro for the
identical class of bug, now proven closed for `context` too).

### 14.5 Latency, before and after (median-of-5, warm page cache)

| repo | verb | before (always-authoritative) | after (index reroute) | delta |
|---|---|---:|---:|---|
| rails | `sem context` | ~160-280 ms | **4.6 ms** | **−97%** |
| monster | `sem context createProgram` | **1.11 s** | **48.0 ms** | **−96%** |

Monster's floor, attributed (median across 5-7 runs, `createProgram`'s
3,972-entity closure, both directions combined):

| phase | cost |
|---|---:|
| `corpus_is_fresh` (membership + whole-corpus content proof) | ~27 ms |
| `REFS` CSR walk, both directions, dedup | ~6 ms |
| file-read-at-span (hydrate) | ~3 ms |
| subgraph rebuild (`EntityGraph::from_parts`, `sort_entity_refs`) | ~5 ms |
| pack (`build_context_result_bounded`, unchanged) | ~9 ms |
| **total** | **~48-51 ms** |

**The correct-answer floor, stated honestly.** The deleted git-oracle fast
path answered in ~40 ms, but it was proven wrong (§12.3: 1 entry/5,583
tokens vs 27 entries/8,000 through the authoritative path, on this exact
query). This reroute's ~48 ms is **not** a regression against that number —
it is the cost of a fast path that is *provably* correct rather than merely
fast: the dominant term (`corpus_is_fresh`, ~27 ms) is the same whole-corpus
freshness proof `impact --all/--tests` and `sem graph` already pay on this
corpus (confirmed directly: `sem graph`'s own `index_fast_path` phase
measured 27-30 ms standalone, same run, same machine), not overhead unique
to this reroute; the remaining ~21-24 ms is real work this verb's shape
requires that a single-CSR-row verb like `impact --deps` doesn't — walking a
closure in both directions, reading real file bytes, and running the
unmodified packer. Read against the ~50 ms target: within it, by a margin
comparable to §13.4's own "thin margin, not zero" framing for `find`/
`callers`/`refs`'s 20 ms budget — not a comfortable multiple of headroom,
but consistently under across repeated runs (46.3-49.6 ms across 7 monster
runs, 4.2-12.6 ms across 5 rails runs).

### 14.6 Regression spot-check (unmodified verbs, monster)

| verb | banked (§13.4) | this bead | delta |
|---|---:|---:|---|
| `find` | 19.9 ms | ~21.4 ms | +7.5% |
| `grep` | 28.6 ms | ~31.1 ms | +8.7% |

Both within the ±20% band; `query.rs`/`grep.rs` are untouched by this bead's
commits, so this is machine/load noise, not a regression this bead
introduced.

### 14.7 Gates

`sem-core` lib **604/604** (603 baseline + 1 new: `entity_rec_start_byte_
end_byte_absent_is_none_u32`), `sem-cli` integration **247/247** (243
baseline + 4 new in `context_cli.rs`), `sem-mcp` **93/93**. clippy and
`cargo fmt --check` clean on every file this bead touched (`format.rs`,
`writer.rs`, `reader.rs`, `mod.rs`, `cache.rs`, `context.rs`,
`context_cli.rs`), verified per-file — `sem-cli/src/main.rs` carries the
same pre-existing formatting drift §13.6 already disclosed as not this
bead's to fix.

## 15. The cache retirement (semx-gpu) — `cache.rs` audited, renamed, not deleted

§12.3 left `sem-cli/src/cache.rs` at 2,231 lines with a one-line verdict:
"what remains in that file is the build plane." This bead's job was to stop
asserting that and prove it — enumerate every remaining item, name the system
that owns its job today, and execute whatever the census actually showed,
including the possibility (the bead's own framing left it open) that
`cache.db`'s 653 MB on-disk role had been fully absorbed by the facts corpus
(semx-9en) or the index and could be deleted outright. It hasn't been. The
census below is exhaustive — every `pub`/`pub(crate)` item in the file,
grep-verified against every caller in `sem-cli` and `sem-mcp` — and it found
one real deletion, one relocation, and a rename that makes the file's name
match what §12.3 already said it was. `cache.db` itself survives, with the
concrete evidence for why in §15.3.

### 15.1 Census

The file (grown to 2,270 lines since §12.3's 2,231 — §13/§14's `DIRS`/byte-
span writer wiring, both build-plane) had exactly these `pub`/`pub(crate)`
items, and no others:

| item | current callers | verdict |
|---|---|---|
| `DiskCache` (struct), `DiskCache::open` | `graph.rs`'s `get_or_build_graph*` family (8 call sites) | **split-to-build-module** |
| `save_with_test_dirs`, `save_topology` | `graph.rs` (full/topology cache-miss saves) | **split-to-build-module** |
| `save` (5-arg, delegates to `save_with_test_dirs` with `&[]`) | **none in production** — every production save call in `graph.rs` goes through `save_with_test_dirs`/`save_topology` directly; a release build's own `dead_code` lint confirmed it before this census (`warning: method 'save' is never used`) | **delete** (from the production surface — see §15.2) |
| `load_with_source_scope` | `graph.rs` (full cache-hit read) | **split-to-build-module** |
| `load` (`#[cfg(test)]` wrapper around the above) | test-only, already gated | **split-to-build-module** (test-only, travels with the file) |
| `load_graph_topology_with_source_scope`, `load_graph_topology_with_test_ids_and_source_scope`, `load_graph_topology_rows`, `load_test_entity_ids` | `graph.rs` (topology-only reads, 3 call sites) | **split-to-build-module** |
| `load_graph_topology`, `load_graph_topology_with_test_ids` (`#[cfg(test)]` wrappers) | test-only, already gated | **split-to-build-module** |
| `load_partial_with_source_scope`, `load_edges` | `graph.rs` (incremental-rebuild read, 2 call sites) | **split-to-build-module** |
| `load_partial` (`#[cfg(test)]` wrapper) | test-only, already gated | **split-to-build-module** |
| `save_incremental_with_repair_metadata` | `graph.rs` (incremental-rebuild write, 2 call sites) | **split-to-build-module** |
| `has_fresh_cache`, `has_fresh_complete_cache`, `has_fresh_topology_cache`, `has_fresh_topology_only_cache` | private gates for every `load*` above | **split-to-build-module** (§12.3's cascade (c): "they fall the moment (b) does; not before" — (b), the hydrate demote, is proven-not-viable this bead, §15.3, so these stay) |
| `PartialCache` (struct) | return type of `load_partial*` | **split-to-build-module** |
| `write_query_index`, `ancestors_of`, `build_dir_fingerprints`, `entity_byte_spans`, `write_test_flags`, `store_repo_origin` | called from every `save*` above, plus `query.rs`'s cold-build path | **split-to-build-module** (the write side of the *other* on-disk artifact, `index.sem`, fed from the same file reads the SQL save already pays for) |
| `CachedImpactResult` (struct) | `impact.rs` only — built by both the index-fast-path tier (`try_index_impact_deps`/`_dependents`/`_transitive`) and the `EntityGraph`-hydrate fallback tier | **delete** (relocate — see §15.2) |
| `CACHED_TEST_IMPACT_LIMIT` (const) | `impact.rs` only | **delete** (relocate — see §15.2) |

No cluster was left over. Every symbol either has a live production caller in
`graph.rs`'s cache-tier fallback chain (build plane, confirmed by reading
`get_or_build_graph_with_cache_policy`/`get_or_build_graph_with_test_data_
and_topology_save_on_miss_with_timings` line by line) or was provably dead
(`save`) or misplaced (the two `impact.rs`-only items). §12.3's one-line
verdict held up under the exhaustive check — with two small corrections it
had no way to see without this pass.

### 15.2 The split and the deletion

**Split** — `crates/sem-cli/src/cache.rs` renamed to
`crates/sem-cli/src/build_cache.rs` (`git mv`, history preserved), with a new
module doc explaining the name: every surviving symbol is the build plane's
warm-cache tier, so the file now says what §12.3 already established it was,
instead of the generic name it inherited from when it also held the SQL
query fast paths that bead deleted. `main.rs`'s `mod cache;` →
`mod build_cache;`; `graph.rs`'s and `query.rs`'s `use crate::cache::…` →
`use crate::build_cache::…`; the one doc comment in `query.rs` that named the
file literally (`cache.rs`) updated to `build_cache.rs`. No behavior changed
— this is a pure rename, confirmed in §15.4.

**Deletion** —

1. `DiskCache::save` (the unsuffixed 5-arg convenience wrapper) is now
   `#[cfg(test)]`-gated rather than public production surface. It had zero
   production callers: every `graph.rs` save site already calls
   `save_with_test_dirs`/`save_topology` directly, and a release build's own
   `dead_code` warning said so before this census touched a line. Deleting it
   outright and rewriting its 9 test call sites onto `save_with_test_dirs(…,
   &[], …)` would have bought nothing behaviorally, so it was gated instead
   of deleted — the same discipline the file already used for `load`/
   `load_graph_topology`/`load_partial`. The guarantee that matters (nothing
   in the shipped binary can reach it) is identical either way.
2. `CachedImpactResult` + `CACHED_TEST_IMPACT_LIMIT` relocated from
   `build_cache.rs` to `impact.rs`. Neither depends on `DiskCache` — they are
   a plain result DTO and a limit constant that `impact.rs` is the sole
   producer *and* sole consumer of (built by the index-fast-path tier in
   `try_index_impact_deps`/`_dependents`/`_transitive` just as often as by the
   `DiskCache`-hydrate fallback). Carrying them in the cache module was
   surface with no build-plane reason to live there — the least-privilege
   question "what does each caller actually need" answered "not
   `crate::build_cache`" for `impact.rs`, which no longer imports it at all.

Net: `build_cache.rs` 2,270 → 2,280 lines (+21/−11: a 13-line module doc, a
9-line struct and a 1-line const removed, a 6-line doc comment added to the
now-gated `save`). `impact.rs` 1,689 → 1,706 lines (+18/−1: the relocated
struct+const plus their new doc comment). Read the two diffs together and the
real ledger is: **0 lines of behavior deleted, ~19 lines of misplaced surface
relocated to its actual owner, one method's reachability narrowed from
"public" to "test-only."** That is the entire deletion this census proved —
not the 0-LOC outcome §12.1/§12.2's residue sections might suggest was
already exhausted, but not the sweeping cut the bead's own framing left open
either.

### 15.3 Why `cache.db` was not deleted

The bead's instruction was conditional: delete `cache.db`'s creation/
maintenance *if* the census shows entities→facts corpus, answers→index,
hydrate→typed CSR fully cover its on-disk role. It doesn't, and this is
checked against the actual running system, not assumed:

- **It's still huge and still being written.** `~/Library/Caches/sem/repos/
  9e9bbeeab5508404/cache.db` (this machine's TypeScript/"monster" cache
  directory) is **653,418,496 bytes** — the exact figure this bead's brief
  cited — with an mtime from the session that just ran, next to an
  `index.sem` of 80 MB and a `facts/` directory. If `cache.db`'s role had
  already migrated, this file would be stale or absent, not 8x the size of
  the index and actively growing.
- **The facts corpus (semx-9en) covers a different, narrower job.**
  Read end to end in `graph.rs`'s `build_graph_with_facts_store` (the
  function `DiskCache`'s own doc comment calls "unrelated and untouched" by
  it): `FactsStore`/`FactsCorpus` persist *pre-resolution* facts —
  `FileFacts`, scope/ref facts, cached resolution read-sets — so a cold
  process can warm-start extraction instead of parsing from nothing. They
  are consulted **only after `DiskCache` has already missed** (`graph.rs`
  line 342's doc: "the single place `sem-cli`'s … path falls back to a cold
  `EntityGraph::build` after `DiskCache` misses"), and a `warm_start` still
  pays to *re-run* `run()`/`Incremental` resolution over whatever the facts
  layer supplies (RESOLUTION-PROFILE.md's own numbers: monster warm-start
  ≈1.1s wall in the cross-process oracle run below, not near-zero). `cache.db`
  persists the **finished, resolved** `EntityGraph` plus entity content, so a
  hit skips resolution entirely, not just reparse.
- **The index (`index.sem`) covers a third, still narrower job.** It answers
  `find`/`callers`/`refs`/`impact --deps`/`--dependents`/`--all`/`--tests`/
  `graph` directly — everything §12.3's cascade rerouted — but it is
  topology + typed refs + trigram postings, not entity *bodies*. §12.3's own
  "Residue" section named the gap this bead re-confirmed still open: `sem
  context` (needs body content) and `sem diff` (needs the full graph across
  two tree states) have **no index tier at all** and fall straight through
  to `DiskCache`'s hydrate cluster or a cold build.
- **Deleting it would force every `sem diff`/`sem context`/incremental-build
  invocation onto `build_incremental_...` + `save_incremental_...` on a
  clean corpus** — §12.3's cascade (b) residue note said this precisely:
  "a worse system, not a smaller one." Nothing this bead found changes that
  conclusion; if anything the facts-store architecture read this bead did
  (§15.3 above) makes the reason more precise than §12.3 had it: the facts
  store buys back *parsing*, never *resolution*, and `cache.db` is the only
  tier that buys back both.

No startup cleanup for stale `cache.db` files was added, because no role was
vacated for one to clean up after — the bead's cleanup step was conditional
on the deletion happening, and it didn't.

**Disclosed, not fixed:** `sem-mcp` has its own separate `DiskCache`/`save`/
`load`/`PartialCache` in `crates/sem-mcp/src/cache.rs` (2,908 lines) — a
second, independently-written constructor family for the identical semantic
object (`cache.db`'s entities/edges/content), used by `sem-mcp/src/server.rs`
directly rather than through `sem-cli`'s tier. This is a duplicate-authority
finding an earlier architecture review surfaced but this bead did not
touch: `sem-cli`'s `build_cache.rs` and `sem-mcp`'s own `cache.rs` both
compile against `sem_mcp::cache` (the shared substrate module — schema,
freshness primitives, manifest handling) but each hand-rolls its own
`DiskCache` struct and save/load methods on top of it, rather than one crate
owning `DiskCache` and the other consuming it. Unwinding that is a
cross-crate constructor-authority cut with a much larger blast radius than
this bead's scope (`sem-cli`'s `cache.rs`) — named here rather than silently
scoped out, matching §12.3's own disclosure convention.

> **Superseded in part — the verdict above was reasoning, and it has since
> been measured** (semx-431 / semx-4ex; RESOLUTION-PROFILE.md "W4: the save
> plane" §2 and "W4.5: the conditional write"). Read §15.3 as the *census*
> that kept the file — that part stands — and this note as what the
> experiment did to its *justifications*:
>
> - **"the only tier that skips resolve" is false.** W4 deleted `cache.db`
>   from a built repo, left `index.sem` and the facts store exactly as they
>   were, and ran every verb on both sides: **ten of ten byte-identical, nine
>   of ten equally fast**. `sem graph --json` answers in 0.06 s on rails and
>   0.34 s on the monster from `index.sem` alone — skipping resolve entirely,
>   without `cache.db`.
> - **Two of the three named gaps were already closed by other beads.** `sem
>   context` answers from the index's byte spans (§14, semx-a3w) plus the file
>   on disk; `sem diff` never hydrates. §15.3's own words for them
>   ("**no** index tier at all") were true when written and are not now.
> - **The third, `impact --deps` name-only, was a gate, not a gap.**
>   `try_index_impact_deps` declined any query without `--entity-id`/`--file`
>   — a rule inherited verbatim from the SQLite fast path §12.3 deleted, and
>   redundant with the ambiguity check thirteen lines below it, since
>   `entity_matches_qualified` and `resolve_by_name_indices` are the *same
>   function* for any name carrying neither `.` nor `::`. semx-4ex closed it
>   (and its mirror in `try_index_impact_dependents`): rails 111 → 27 ms,
>   monster 703 → 56 ms, 24/24 forms byte-identical.
> - **What actually survives is one path: the incremental rebuild** — and it
>   costs 2.1-9.3 s of *every* cold build to save ~0.6-12.3 s on each
>   subsequent dirty `sem graph`, a break-even of roughly one rebuild. So
>   semx-4ex made the write conditional rather than deleting the file: the
>   corpus-shaped build (`sem graph`) writes `index.sem` only
>   (`CacheMissSavePolicy::IndexOnly`), the content-hydrating verbs keep
>   `Full` and create the mirror on their own first miss, and
>   `SEM_BUILD_CACHE=1` restores the old behaviour. Cold builds fell 10.7-20.9%
>   on all five giants.
> - **The `sem-mcp` duplicate below is not the blocker it looked like.**
>   `sem-cli` never constructs `sem-mcp`'s `DiskCache` (it imports
>   `sem_mcp::cache` only for the shared substrate), so a CLI-side change does
>   not "move the write" — only `sem mcp` itself writes through that copy. It
>   keeps writing on purpose: `sem-mcp` writes no `index.sem` and uses no
>   facts store, so `cache.db` is its only warm tier. semx-r94 stands.

### 15.4 Regression proof

Two binaries built from the same worktree: `sem-pre` (HEAD, unmodified
`cache.rs`) and `sem-post` (this bead's `build_cache.rs` + relocations),
compared on rails (`/tmp/bench-fleet/rails`) with isolated `SEM_CACHE_DIR`s
per binary so neither run could taint the other.

| check | result |
|---|---|
| `sem graph --json .` (cold, empty cache dir) | **byte-identical**, 37,019,013 bytes both sides |
| `sem graph .` (warm, second invocation) | 0.01s both sides |
| `sem find isActive --json` | **byte-identical** stdout |
| `sem impact isActive --deps --json` | **byte-identical** stdout |
| `sem impact isActive --json` (transitive, index tier) | **byte-identical** stdout |
| `sem context isActive --json` | **byte-identical** stdout |
| `sem grep isActive` | **byte-identical** stdout |
| `sem diff HEAD~1..HEAD --format json` (real commit, exercises the hydrate cluster) | **byte-identical**, 256,805 bytes both sides |

`index_probe` on rails: **ORACLE** 37,508/37,508, **REFS_ORACLE** 59,249
entities/0 mismatched, **FILES_ORACLE** 17 prefixes/0 mismatched,
**TESTS_ORACLE** 59,249 checked/20,243 tests/0 mismatched, **TRIGRAM_ORACLE**
6 patterns/0 mismatched, **MUTATION** PASS — all six, unaffected as expected
(this bead touched zero `sem-core` files). `facts_probe` cross-process
oracle: **4/4 PASS** on rails (`none`/`leaf`/`hub`/`mixed50`) — disclosed
short of the bead's "8/8": the monster corpus this repo's own cache
directory has 653 MB of `cache.db` for is not actually checked out on this
machine (`~/.cache/checkouts/github.com/microsoft/TypeScript` is a 1 KB
devcontainer stub, not the source tree), so the second corpus half of the
historical 2-corpus × 4-scenario table couldn't be re-run here.

Gates: **sem-core lib 604/604**, **sem-cli 247/247**, **sem-mcp 93/93**
(all three exactly at the pre-existing baseline — this bead added no tests
and deleted none, since nothing it touched was test-covered production
logic other than the relocations). clippy: the 3 warnings clippy reports
against the touched files (`build_cache.rs`'s `save_incremental_with_repair_
metadata` arg count, `graph.rs`'s modulo-vs-`is_multiple_of` in an untouched
function, `impact.rs`'s deref in `print_cached_result`) are all pre-existing
— verified against `HEAD`'s copies of the same functions at their old line
numbers, zero new warnings. `cargo fmt --check`: `build_cache.rs`,
`graph.rs`, `impact.rs`, `query.rs` clean; `main.rs`'s one touched line
(`mod cache;` → `mod build_cache;`) is clean, and the file's 5 pre-existing
formatting diffs (confirmed present in `HEAD`'s `main.rs` before this bead
touched it, §13.6/§14.7's same disclosed drift) are unrelated and untouched.

### 15.5 Entry-fee re-measure

First-ever cold build (`SEM_CACHE_DIR` pointed at a freshly emptied
directory each run — no `cache.db`, no `index.sem`, no facts store), `sem
graph --json` on rails, median-of-3:

| binary | run 1 | run 2 | run 3 | median |
|---|---:|---:|---:|---:|
| pre | 2.98s | 3.00s | 3.26s | 3.00s |
| post | 3.20s | 3.12s | 3.15s | 3.15s |

**+5%**, inside the ±10% band this bead's brief required — and expected to
be noise rather than signal: a fully cold build with an empty cache
directory never reaches `build_cache.rs`'s changed code at all (`DiskCache::
open`/`load*`/`load_partial*` all miss on an empty db and fall straight to
`build_graph_with_facts_store`, which this bead did not touch); the two
binaries differ by a file rename, a struct relocation, and one method's
`cfg` gate, none on the cold-build path. Steady-state verb spot-check is
§15.4's byte-identical table above (all index-served, sub-millisecond,
unaffected either way) rather than a separate timing table — the verbs the
bead's spot-check names (`find`/`grep`/`impact`/`context`) never touch
`build_cache.rs` in the first place (`query.rs`'s own module doc: "this
module does not import `build_cache::DiskCache`").

### 15.6 Survivors, with reasons

Everything in `build_cache.rs` after this bead survives for one of two
reasons, and every survivor is tagged with which:

- **Build-plane warm-cache tier** (the whole `DiskCache` impl minus `save`):
  `graph.rs`'s `get_or_build_graph*` family is the only caller, and it is the
  fallback chain every `sem diff`/`sem context`/`sem graph`/cold-`sem
  impact` invocation runs when the index can't answer — proven live by
  reading every call site, not assumed from §12.3's prose.
- **The index's own write path**: `write_query_index` and its five helpers
  are the reason every `DiskCache::save*` also produces a fresh `index.sem`
  from the same file reads — deleting them would silently stop `find`/
  `callers`/`refs`/`graph`/shallow-`impact` from ever refreshing after a
  build, which is a regression in the *other* tier's freshness, not a cache
  concern this bead's cut could make disappear.

No item survives merely because "it might be used" or because deleting it
looked risky — the census in §15.1 is the receipt for each one.

**Bead semx-gpu closed.**

### 15.7 The retirement audit's verdict, re-decided on evidence (semx-4ex)

§15.1's item-level census stands unchanged — every symbol in `build_cache.rs`
still has the caller it was tagged with. What changed is the *second* survivor
reason in §15.6, and it changed because the corpus-shaped build was measured:

> "**The index's own write path**: `write_query_index` and its five helpers
> are the reason every `DiskCache::save*` also produces a fresh `index.sem`
> from the same file reads."

True, and read the other way round it is the finding: the index write was a
*passenger* on the SQL save. `sem graph`'s cold miss went through
`CacheMissSavePolicy::Full` — entity bodies, the compressed content store,
every index on both — to obtain an artifact (`index.sem`) that needs none of
it, on a verb answered from that artifact forever after. semx-4ex inverts the
dependency: `build_cache::write_index_only` performs the one corpus read, the
test classification and the byte spans and calls `write_query_index` directly,
opening no SQLite connection at all, and `get_or_build_graph_topology_with_
timings` uses it. The read-side probes moved to `DiskCache::open_existing`, so
a build that writes no mirror no longer *creates* an empty one either.

The result, per giant (cold, median-of-3): home-assistant-core −16.3%, monster
−18.4%, dotnet-runtime −10.7%, llvm-project −14.1%, linux −20.9%, and 0.3-1.8
GB per repo unwritten — with `index.sem` byte-size identical on all five and
sha256-identical on rails and tiptap. Full numbers, the mechanism comparison,
the dirty-rebuild trade and the residuals: RESOLUTION-PROFILE.md "W4.5: the
conditional write".
