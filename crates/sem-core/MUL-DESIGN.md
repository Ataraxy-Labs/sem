# MUL-A: can `PrecomputedFileFacts` go past JS/TS soundly?

**Bead**: semx-w5k.1 (MUL-A), under epic semx-w5k, answering phase A of semx-mul.
**Status**: design + census only. **No production code changed by this bead** —
the one file added is `crates/sem-core/examples/mul_census.rs`, a probe.

W3 §5 named two fences on extending the JS/TS precompute to every language:
**memory** (C# measures ~40x tree-bytes per source-byte, and semx-g6t's byte
budget exists to bound that) and **semantics** (`PrecomputedFileFacts` is
licensed by *"JS/TS declarations never nest across files"*, stated to be FALSE
for C# partial classes and C++ out-of-line member definitions). This document
measures both fences and reports what they actually are.

**Headline.** The semantics fence is **empirically empty, and provably so from
`build_entity_id`'s shape**: across 4,836,244 entities on seven corpora and
seven language families — including **18,006 C# `partial` type declarations** in
dotnet-runtime and **164,431 C++ out-of-line member definitions** in
llvm-project and dotnet-runtime — there are **zero** cross-file parent links.
The real fence is the *other* half of the license, the structural one, and it
points the **opposite** way from the bead's hypothesis: **C# and C++ are the
easy families and Python is the hard one.** dotnet's C# files need their tree in
pass 2 for nothing at all; HA's Python files need it for 99.77% of their bytes.

**Verdict**: **GO for C# and C++** on a per-file gate with no facts-schema
change (dotnet **−30.2%** of `full_graph_build` cold, llvm **−10.6%**);
**NO-GO as-is for Python, Go, Java, Rust**, whose prize is real but is gated
behind a facts extension (import statement descriptors) that is priced here and
scheduled as phase 2.

---

## 1. What the license actually requires

The stated license is a language property. The property the *code* needs is
narrower, and it is checkable per file.

`precompute_js_ts_file_facts` (`scope_resolve.rs:1116`) differs from the pass-2
AST path in exactly one input: where the AST path passes the corpus-wide
`entity_map` and `children_by_parent`, the precompute passes **file-local
substitutes** built from this file's entities alone. Everything else — the
`FileEntityLookup`, the config, the source bytes — is already file-local on both
paths.

Reading every use of those two maps inside `scope_visit_node`
(`scope_resolve.rs:2903`), which is the whole of the scope walk's per-node
semantics:

| use site | key | file-local? |
|---|---|---|
| `children_by_parent.get(ce.id)` (class-like) | `ce` from `file_lookup.find_at_line` | key is this file's |
| `children_by_parent.get(ie.id)` (impl) | `ie` from `file_lookup.find_at_line` | key is this file's |
| `children_by_parent.get(me.id)` (Rust `mod_item`) | `me` from `file_lookup.find_at_line` | key is this file's |
| `entity_map.get(oid)` (Go `external_method`) | `oid` = `scopes[i].owner_id`, only ever set from a `file_lookup` hit | key is this file's |

Every key is an id this file's own `FileEntityLookup` produced. So the *values*
are the only thing that can differ, and only in one way:

> **Predicate CLEAN(F).** For every entity `e` declared in `F`,
> `{ x : x.parent_id == e.id } ⊆ entities(F)`.
>
> i.e. no entity outside `F` may name an entity of `F` as its parent.

**CLEAN(F) ⟺ the file-local substitutes are observationally identical to the
corpus-wide maps for `F`.** That is the whole semantic license, restated as a
per-file, measurable predicate. It is not a statement about a language; it is a
statement about one file's rows in `children_by_parent`.

### 1.1 The theorem: why CLEAN is currently universal

`build_entity_id` (`model/entity.rs:57`):

```rust
match parent_id {
    Some(pid) => format!("{pid}::{name}"),
    None      => format!("{file_path}::{entity_type}::{name}"),
}
```

Entity extraction is **per file**: `registry.extract_entities(file_path,
&content)` (graph.rs:2120) sees one file's bytes and constructs every id and
every `parent_id` inside that one call. Therefore, by induction on the parent
chain:

> **Theorem (file-rootedness).** For every entity `e`, `parent(e) ∈
> entities(file(e))`, and `id(e)` has `file(e)::` as a prefix.
>
> **Corollary.** `children_by_parent[e] ⊆ entities(file(e))` for every `e`, in
> every language — the exact predicate the license needs, unconditionally.

The corollary has one hole, because `children_by_parent` is keyed by an id
**string**, not by identity: two files could collide on an id string. The
census measures that directly (`dup_cross_file`, §2.2) and finds **zero** across
all seven corpora.

**Why the fence's counterexamples don't bite.** A C# `partial class C` split
across `Foo.cs` and `Bar.cs` produces **two entities**, `Foo.cs::class::C` and
`Bar.cs::class::C`, each owning only its own members. The corpus-wide
`children_by_parent` and the file-local one agree on both. A C++ out-of-line
member definition `void A::f() {…}` in `A.cpp` produces a **separate top-level
entity whose name is `A::f`** — the census counts 141,537 such qualified names in
llvm's C++ files against 141,502 textual out-of-line definitions, a 1.00 ratio —
and no parent link back into `A.h` at all. Both constructs are *real and
abundant*; neither creates the cross-file nesting the fence assumed.

### 1.2 The second half of the license, which is the real one

`PrecomputedFileFacts`'s doc comment carries a second clause that W3 §5 did not
quote, and it is the binding one:

> *"Every other tree-touching computation the chunked path performs —
> `extract_imports_from_ast`'s Python/Rust/Go branches; ctor-infer's
> `scan_constructor_calls`; Swift call-signature building — is a **structural
> no-op** for a JS/TS AST."*

That is what makes a JS/TS file able to skip a tree *entirely*. After semx-3ao's
fusion, the pass-2 per-file closure has exactly **one** remaining tree use
(`scope_resolve.rs:~1991`):

```rust
if let (Some((_, _, tree)), Some(import_starts)) = (reparsed, &fused_import_starts) {
    … replay_import_stmts_pruned(tree.root_node(), …)
```

gated on `import_starts` being non-empty. Outside the closure, three whole-file
consumers read `parsed_files`: `build_ts_default_export_table` (dead on the
graph-build path — an import table is always supplied),
`build_swift_call_signatures` (gated on `corpus_has_swift`), and
`infer_constructor_param_types` → `scan_constructor_calls`, which fires only on
the node kind `"call"` (Python's grammar; C# uses `invocation_expression`, C++
`call_expression`, Rust `call_expression`).

So the structural predicate is:

> **Predicate TREELESS(F).** `F` has a `scope_resolve` config, contains **no**
> node kind that `classify_import_stmt` handles
> (`import_from_statement`, `import_statement`, `export_statement`,
> `use_declaration`, `import_declaration`), contains **no** node of kind
> `"call"`, and is not `.swift`.

TREELESS is decidable **during the fused walk**, at the one program point BS3
created: the walk already records `import_starts`; `"call"` is one extra `kind`
comparison on nodes it already visits; `.swift` is the extension.

> **FASTPATH(F) ⟺ CLEAN(F) ∧ TREELESS(F).** Both halves are measured below.

---

## 2. The violation census

**Instrument**: `crates/sem-core/examples/mul_census.rs`, this bead's probe.
Walks the corpus with the product's own file admission
(`registry.get_explicit_plugin` + `is_default_excluded` + `is_probably_binary_path`),
runs the product's own `registry.extract_entities` per file, builds a global
`id → owning file` map, and reports both predicates plus the raw language
constructs. `TREELESS` is evaluated by parsing each scope-resolvable file with
the product's `parse_tree` + `get_language_config` and testing node kinds — the
same kinds `classify_import_stmt` and `scan_constructor_calls` test.

### 2.1 The semantics half — CLEAN

**Zero violations everywhere.**

| corpus | family | files | entities | entities with parent | **cross-file children** | **files failing CLEAN** |
|---|---|---:|---:|---:|---:|---:|
| dotnet-runtime | C# | 32,522 | 656,256 | 605,765 | **0** | **0** |
| llvm-project | C++ | 39,484 | 562,228 | 210,035 | **0** | **0** |
| home-assistant-core | Python | 18,145 | 129,643 | 47,107 | **0** | **0** |
| TypeScript monster *(control)* | JS/TS | 39,296 | 418,475 | 142,537 | **0** | **0** |
| kubernetes | Go | 13,321 | 175,940 | 81,935 | **0** | **0** |
| rust-lang-rust | Rust | 38,092 | 326,450 | 113,145 | **0** | **0** |
| elasticsearch | Java | 30,054 | 502,531 | 472,735 | **0** | **0** |
| **all seven corpora, all families** | | **4,836,244 entities total** | | | **0** | **0** |

The **monster row is the positive control**: those 39,296 files are exactly the
ones the production precompute path serves today, under gates that have been
bit-identical for four waves. They show the same `0` the C#/C++/Python rows show
— i.e. **the predicate that is already proven sound in production is the
predicate every other family also satisfies.** The license is not
language-specific in this codebase.

### 2.2 The constructs the fence named, counted

| corpus | construct | count | files containing it | % of that family's files |
|---|---|---:|---:|---:|
| dotnet-runtime | C# `partial class/struct/interface/record` declarations | **18,006** | 7,646 | **23.5%** |
| dotnet-runtime | C++ out-of-line member definitions | 22,929 | 824 | 44.5% |
| llvm-project | C++ out-of-line member definitions | **141,502** | 10,047 | **25.4%** |
| llvm-project | C++ entity names containing `::` (what the extractor emits for them) | 141,537 | — | — |

The fence's counterexamples are **abundant** — this is not a corpus that happens
to avoid them. They simply do not produce cross-file *nesting* in sem's entity
model.

**Id collisions** (`dup_cross_file` = the only mechanism that could break the
corollary):

| corpus | dup ids across files | dup ids within one file |
|---|---:|---:|
| dotnet | **0** | 195 |
| llvm | **0** | 2,602 |
| HA | **0** | 0 |
| monster | **0** | 13 |
| kubernetes | **0** | 2,708 |
| rust | **0** | 73 |
| elasticsearch | **0** | 249,740 |

Within-file duplicates are harmless to CLEAN — both colliding entities belong to
the same file, so the corpus-wide and file-local maps merge them identically —
but elasticsearch's 249,740 is **surfaced, not absorbed** (§7, finding F3).

### 2.3 The structural half — TREELESS, per family

Fraction of scope-resolvable files whose tree pass 2 still needs after the walk's
outputs are precomputed:

| corpus | family | scope-resolvable files | needs tree | **TREELESS files** | imports | `"call"` |
|---|---|---:|---:|---:|---:|---:|
| dotnet | C# | 32,522 | **0** | **32,522 (100%)** | 0 | 0 |
| llvm | C++ | 39,484 | **0** | **39,484 (100%)** | 0 | 0 |
| dotnet | C++ | 1,850 | **0** | **1,850 (100%)** | 0 | 0 |
| rust | Rust | 38,092 | 14,446 | 23,646 (62.1%) | 14,446 | 0 |
| kubernetes | Go | 13,321 | 11,289 | 2,032 (15.3%) | 11,289 | 0 |
| HA | Python | 18,145 | 16,575 | 1,570 (8.7%) | 16,559 | 16,088 |
| elasticsearch | Java | 30,054 | 29,084 | 970 (3.2%) | 29,084 | 0 |
| monster | JS/TS | 39,296 | 10,587 → **0 effective** | 39,296 | 10,587 | 0 |

*(JS/TS's import kinds are handled but `skip_js_ts_imports` is unconditionally
true on the chunked path — a pre-built import table is always supplied — which is
precisely why the existing precompute is sound for them.)*

**By bytes, which is what the re-parse costs:**

| corpus | scope-resolvable non-JS/TS bytes | FASTPATH bytes | **% of bytes on the fast path** |
|---|---:|---:|---:|
| **dotnet** | 504.77 MB | 503.45 MB | **99.74%** |
| **llvm** | 467.92 MB | 452.78 MB | **96.76%** |
| rust-lang-rust | 143.57 MB | 19.73 MB | 13.74% |
| kubernetes | 134.84 MB | 7.07 MB | 5.24% |
| elasticsearch | 278.98 MB | 2.75 MB | 0.98% |
| **HA** | 115.62 MB | **0.27 MB** | **0.23%** |

HA's file-count figure (8.7%) badly overstates its prize: its 1,570 TREELESS
Python files average **170 bytes** — they are `__init__.py` stubs. **On bytes,
the un-extended two-tier gate buys HA essentially nothing.**

---

## 3. Measurements: where the prize is today

**Protocol**: release binary, darwin, `available_parallelism` = 18,
`SEM_LOCAL=1 SEM_TIMINGS=1 SEM_PROFILE_RESOLVE=2 SEM_FACTS_CACHE=0`, fresh
`SEM_CACHE_DIR` per run — a genuine cold build with no facts-corpus service, so
the attribution isolates pass 2's own work. **n=1 per corpus**, disclosed: this
box runs ~1.3-1.7× the box RESOLUTION-PROFILE's LOCAL-COLD sections used (HA
total 10.29 s here against ~6.2 s there), so **absolutes are upper bounds and
every conclusion below is stated as a ratio.**

| corpus | files | `reparse_ms` (wall) | `pass2_wall_ms` | `scope_build_ms` (thread) | `fused_walk_ms` | `extract_imports_ms` | `full_graph_build` | total |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| HA | 18,150 | **1,018.7** | 533.0 | 7,885.6 | 5,126.7 | 2,394.6 | 7,763.8 | 10,294.2 |
| llvm | 43,270 | **5,227.9** | 13,753.7 | 23,479.7 | 17,587.7 | 4,479.9 | 52,439.1 | 64,193.2 |
| dotnet | 34,898 | **17,647.5** | 5,494.9 | 23,093.4 | 20,252.3 | 1,534.2 | 58,335.7 | 68,697.8 |

Work counters (`SCOPE_BUILD_WORK`), which the memory model in §5 consumes:

| corpus | files on AST path | files precomputed | entities spanned | scopes built | refs collected |
|---|---:|---:|---:|---:|---:|
| HA | 18,148 | 2 | 129,645 | 151,998 | 610,274 |
| llvm | 43,209 | 61 | 582,065 | 543,234 | 3,718,735 |
| dotnet | 34,670 | 228 | 721,805 | 634,101 | 2,500,456 |

`reparse_ms` is the **wall** elapsed of the parallel re-read+re-parse region,
summed over chunks — it is the second `read_to_string` + `parse_tree` of every
non-precomputed file, and it is **30.3% of dotnet's entire `full_graph_build`**.
Scaled to the LOCAL-COLD box (÷1.66, HA's ratio) that is ~10.6 s, agreeing with
W3 §5's independently-derived ~11.2 s.

---

## 4. The design

### 4.1 Shape: compute-then-gate, two tiers, no new semantics

The gate cannot be evaluated before the facts are built — CLEAN needs the
corpus-wide `children_by_parent`, which does not exist until pass 1 has
assembled `all_entities`, while the facts need the tree, which exists only
*inside* pass 1's per-file closure. The resolution is to invert the order:

1. **Pass 1, per file, tree in hand** (the closure at `graph.rs:2119` that today
   calls `registry.extract_entities` and discards the tree): call
   `extract_entities_with_tree` instead for any file whose language has a
   `scope_resolve` config, run the **fused triple walk** (semx-3ao's
   `fused_scope_refs_import_walk`, whose four outputs *are*
   `PrecomputedFileFacts`' first four fields — BS3 §5), plus `scan_return_types`
   and `scan_init_self_attrs` (fields 6-9, already file-local), and evaluate
   **TREELESS** from what the walk saw. Emit facts only if TREELESS.
   The tree dies at the end of the closure exactly as it does today.
2. **After pass-1 assembly**, in one O(entities) pass over the
   `children_by_parent` that `PrebuiltEntityIndex::build` already constructs,
   evaluate **CLEAN** per file: for each `(parent_id, children)` row, if any
   child's `file_path` differs from the parent's, mark the parent's file dirty.
   **Drop the facts of every dirty file.** Measured cost: one scan of 721,805
   entities on dotnet ≈ tens of ms; measured yield today: zero files dropped.
3. **Pass 2** is unchanged. A file with facts is already handled by semx-6rd
   CUT 1's existing code: the re-parse loop skips it, the closure clones its
   facts, `parsed_files` never contains it.

Nothing about the resolver's *semantics* changes. The two tiers are "this file's
facts were precomputed" and "this file gets a tree", which is the split that has
shipped since semx-6rd — the change is only **which files are eligible**.

### 4.2 Invariants

- **I1 (soundness).** `FASTPATH(F) ⇒ file-local (entity_map, children_by_parent)
  are observationally identical to the corpus-wide ones for F.` Established by
  §1's use-site enumeration plus CLEAN, and **checked at run time** rather than
  argued: step 2 computes CLEAN and fails toward the old path.
- **I2 (seed order).** The precompute must seed `scopes[0].defs` /
  `entity_scope_map` by iterating this file's entities in **`entity_ranges`
  order** — `(start_line, end_line, id)`, as `PreBuiltLookups` sorts them
  (`scope_resolve.rs:1306`) — because that is the order the AST path uses
  (`scope_resolve.rs:1927`) and `defs.insert` is last-write-wins. *The existing
  JS/TS precompute uses extraction order instead* — see finding F1, §7.
- **I3 (structural).** `FASTPATH(F) ⇒` no pass-2 consumer reads F's tree.
  Decided **by the walk itself**, from the kinds it visits, not by a language
  table — so a grammar change cannot silently invalidate it.
- **I4 (red-green composition).** Facts are a pure function of
  `(F's content, F's own entities)`. Both are already `ScopeIncremental`
  dependencies of F, so the GREEN read-set logic is unchanged: a GREEN file's
  facts survive in the session store (`graph.rs:2176`), a RED file recomputes
  them from its fresh tree. No new `Table` fingerprint, no new whole-table guard.
- **I5 (facts-corpus keying).** Facts travel in the existing
  `CorpusFile.precomputed: Option<PrecomputedFileFacts>` under the existing key
  `(relative_path, content_hash, lang_salt)`. Because §fqh made corpus dedup
  **first-writer-wins**, an existing corpus entry for a `.cs` file carrying
  `precomputed: None` will **permanently deny** the new facts a slot. The
  producer change therefore *requires* a `lang_salt` (or
  `FACTS_SCHEMA_VERSION`) bump — see finding F2, §7. This is a deployment
  correctness-of-speed issue, not of results.
- **I6 (fail-safe).** Every gate failure routes F to today's re-parse path. An
  ungated file is never wrong, only slower. There is no state in which the fast
  path is taken on a file that fails either predicate.

### 4.3 What a facts extension would have to carry (phases 2-3)

For the families TREELESS rejects, the tree is needed for exactly two things,
both of which are **syntactic extraction feeding corpus-wide resolution** — the
handler reads `symbol_table` / `entity_map` / `go_pkg_index` /
`top_level_entities`, none of which exist in pass 1, so the *handler* must stay
in pass 2 while what it reads *from the tree* can move to pass 1:

- **Field 10, `import_stmts: Vec<ImportStmtFacts>`.** One serializable
  descriptor per node in the replay set — `(kind, module path string, [(original,
  local)] specifier pairs, alias)` — emitted **in `replay_import_stmts_pruned`'s
  exact order** by running that same pruned replay at precompute time against the
  live tree and recording instead of dispatching. `dispatch_import_stmt`'s six
  handlers are refactored to consume a descriptor instead of a
  `tree_sitter::Node`. Order is preserved by construction (the replay is the
  order-defining algorithm; BS3 already proved a document-order variant RED
  against it in `import_replay_order_is_load_bearing`), which is what makes this
  a mechanical extraction rather than a semantics change. Unlocks **Rust, Go,
  Java** outright and is the larger half of **Python**.
- **Field 11, `ctor_call_sites: Vec<CtorCallFacts>`.** `scan_constructor_calls`'
  per-`"call"`-node inputs — `(callee identifier, [argument shapes])` — since its
  scan is a pure syntactic sweep whose only corpus-dependent parts
  (`func_name_returns`, `init_params`, `attr_to_param_index`) are consulted
  *after* the node is read. Python only.
- **Swift** is out of scope in every phase: `build_swift_call_signatures` walks
  every tree against corpus-wide `entity_ranges`/`entity_map`, i.e. it is not a
  per-file function at all. Swift files keep their trees.

---

## 5. The arithmetic

### 5.1 Time, per corpus

`FASTPATH` bytes from §2.3; timings from §3. Two tiers of saving:

- **Cold** (nothing served from the facts corpus): the re-parse disappears. The
  fused walk *moves* to pass 1, where the tree is already in hand — same work,
  no second parse — so it is a wash at worst, and better at best (pass 1 is one
  flat parallel map; pass 2 is 30 chunk-serialized ones on dotnet). Additionally
  `bow_index_io` — bag-of-words' own second read of files
  `snapshot_bow_content` did not cover — disappears, because
  `PrecomputedFileFacts::content()` covers them (semx-bkz's existing mechanism);
  priced at the doc's measured 9-10× bow parallelism.
- **Known-content**: facts arrive from the corpus, so the walk disappears too.
  Its wall share is its share of pass-2 thread work
  (`scope_build + ref_collect + ref_loop`) applied to `pass2_wall_ms`.

| | dotnet | llvm | HA (gate only) | HA (with phase 2+3) |
|---|---:|---:|---:|---:|
| fast-path byte share | 99.74% | 96.76% | 0.23% | 100% |
| re-parse eliminated | **−17,601 ms** | **−5,058 ms** | −2 ms | −1,019 ms |
| `bow_index_io` eliminated (wall) | −353 ms | −478 ms | −0.3 ms | −128 ms |
| **cold total** | **−17,954 ms** | **−5,536 ms** | **−2 ms** | **−1,147 ms** |
| **cold, as % of `full_graph_build`** | **−30.8%** | **−10.6%** | −0.03% | **−14.8%** |
| cold, as % of CLI total | −26.1% | −8.6% | −0.02% | −11.1% |
| fused walk also eliminated (known-content) | −3,225 ms | −2,219 ms | −0.7 ms | −316 ms |
| **known-content total** | **−21,179 ms** | **−7,755 ms** | −3 ms | **−1,463 ms** |
| **known-content, as % of `full_graph_build`** | **−36.3%** | **−14.8%** | −0.04% | **−18.8%** |

The bead's stated prize (dotnet ~7.4 s, llvm ~9.6 s, HA ~1.15 s) was the
*`scope_build`-relocation* half only. Re-derived here against the post-hoist
tree, the re-parse half is **larger than the walk half on dotnet by 5.5×**, and
the two together are what §5.1 reports.

### 5.2 The ≥80% test the bead set

> *"If the two-tier scheme captures ≥80% of the prize with a per-file gate and no
> semantics change, that's likely the winning shape."*

Taking "the prize" as the full-extension known-content number per corpus:

| corpus | gate-only (cold) | gate-only (known) | full extension (known) | **gate-only ÷ full** |
|---|---:|---:|---:|---:|
| dotnet | 17,954 ms | 21,179 ms | 21,238 ms | **99.7%** |
| llvm | 5,536 ms | 7,755 ms | 8,014 ms | **96.8%** |
| HA | 2 ms | 3 ms | 1,463 ms | **0.2%** |

**The two-tier gate passes the ≥80% test decisively on C#/C++ and fails it
completely on Python.** That is the census deciding, exactly as instructed — and
it decides differently per family, which is why the verdict is per family.

### 5.3 Memory — the first fence, priced

**Model**, calibrated on the one corpus where the production path already
retains facts corpus-wide. monster, `SEM_PROFILE_MEM=1`, measured:

```
SEM_PROFILE_MEM[peak-resolve]  precomputed_facts  271.3MB     (39,296 files,
   130.4 MB of TS source, 418,475 entities, 197,386 scopes, 254,124 refs)
SEM_PROFILE_MEM[peak-resolve]  process_rss      2048.8MB
```

`approx_heap_bytes` deliberately does not walk nested `String`s inside
`Scope::defs` / `AstRef`, and its own doc says so. Reconstructing those from the
measured entity sizes (monster's `entity_map` = 203.9 MB / 418,475 entities)
adds ~120 MB, so monster's true facts residency is **~390 MB ≈ 3.0 × source
bytes**. Applying the same per-unit constants (`sizeof(Scope)` ≈ 344 B,
`sizeof(AstRef)` ≈ 88 B, plus id-string keys at the measured per-corpus id
width) to the §3 work counters:

| corpus | source on fast path | scopes | refs | **projected facts residency** | measured peak RSS | **as % of peak** |
|---|---:|---:|---:|---:|---:|---:|
| dotnet | 503.5 MB | 634,101 | 2,500,456 | **~1.25-1.35 GB** | 10,405 MB | +12-13% |
| llvm | 452.8 MB | 543,234 | 3,718,735 | **~1.25-1.31 GB** | 8,669 MB | +14-15% |
| HA (phase 2+3) | 115.6 MB | 151,998 | 610,274 | ~0.27 GB | — | — |

Against that, what it **removes**: today's chunked path holds a
`(path, content, tree)` triple per file for every file of the chunk, under
semx-g6t's 20 MiB byte budget. Measured on dotnet
(`SEM_PROFILE_MEM[chunk-reparse]`, 30 chunks): content 0.6-23.0 MB per chunk,
and process RSS across the entire chunk loop rises **7,086 → 7,789 MB (+703
MB)** — an upper bound on chunk-tree residency, since 482 MB of that is
attributed to `scope_edges` + `scope_consumed_words` accumulating. So the trees
cost ≲ 220 MB of high-water, **not** the ~800 MB that 20 MiB × C#'s 40× tree
ratio would suggest, because the budget already bounds it and mimalloc recycles
between chunks.

> **Net memory: dotnet ≈ +1.0-1.15 GB (+10-11%), llvm ≈ +1.05-1.1 GB (+12-13%).**
> semx-g6t's byte-budget win was −19.6% on dotnet (10.30 → 8.28 GB). **This
> change gives back roughly half of it.** That is a real cost and it is the
> single number that could turn the GO into a NO; it is stated as a projection,
> not a measurement, and **phase 1's gate must measure it on the real producer
> before the change lands** (§6).

**Serialized size** (per-repo `FactsStore` pack, `SEM_FACTS_CORPUS=0`, fresh
`SEM_CACHE_DIR`, measured this session):

| corpus | factpack | precomputed inside? | bytes / entity |
|---|---:|---|---:|
| monster | **672 MB** | yes, 39,296 files | 1,586 |
| dotnet | **2,151 MB** | no (228 JS/TS only) | 2,272 |
| HA | **333 MB** | no | 1,354 |

There is no in-tree control that turns monster's precompute off, so the
precomputed *share* of monster's 672 MB is not separable from these three
numbers alone; the honest bracket is the in-memory 271-390 MB, i.e. dotnet's pack
would grow from 2,151 MB by **roughly +1.0-1.4 GB (+50-65%)** and llvm's
similarly. §fqh made corpus **read** cost independent of corpus size, so this is
disk and write-path cost, not read latency — but it is disclosed as an estimate
and phase 1 must measure it.

**Two memory levers exist and are named but not taken here**: (i) fast-path files
never enter `parsed_files`, so on dotnet/llvm the 20 MiB byte budget governs
~0.3%/3.2% of bytes and the 30-chunk partition could be relaxed to near-1 chunk,
deleting 29 chunk barriers; (ii) facts could be spilled to the per-repo store at
pass-1 exit and read back per chunk, making residency chunk-bounded again at the
cost of I/O.

---

## 6. Verdict and phase plan

### 6.1 GO / NO-GO per family

| family | CLEAN | TREELESS (by bytes) | cold prize | **verdict** |
|---|---|---:|---:|---|
| **C#** (dotnet) | **100%** | **99.74%** | −30.8% of `full_graph_build` | **GO — phase 1** |
| **C++** (llvm) | **100%** | **96.76%** | −10.6% | **GO — phase 1** |
| **Rust** | 100% | 13.74% | not separately measured | **NO as-is; GO after phase 2** |
| **Go** | 100% | 5.24% | not separately measured | **NO as-is; GO after phase 2** |
| **Java** | 100% | 0.98% | not separately measured | **NO as-is; GO after phase 2** |
| **Python** (HA) | 100% | 0.23% | −0.03% as-is, **−14.8% after phases 2+3** | **NO as-is; GO after phases 2+3** |
| **Swift** | 100% | n/a | — | **NO — out of scope in every phase** (`build_swift_call_signatures` is corpus-wide, not per-file) |

The bead's own hypothesis — *"a narrower sound subset (e.g. Python first: no
partial classes, no out-of-line defs)"* — is **falsified in both directions**.
Python's semantics are no cleaner than C#'s (both are 100%), and Python is the
**worst** family structurally, while C#/C++ — the two the fence excluded — are
the only two that are 100% ready today. On class reopening and monkey-patching
specifically, the honest answer is that neither is a *declaration*-nesting event
in sem's model: reopening a class in another module produces a second
independent entity exactly as a C# partial half does, and monkey-patching is a
runtime assignment that produces no entity at all.

### 6.2 Phases (days, per the two-tier shape winning)

- **Phase 1 — C# + C++ behind the per-file gate. ~2-3 days.**
  1. Pass 1 takes `extract_entities_with_tree` for every scope-resolvable
     language, runs the fused walk + the two scans, and evaluates TREELESS from
     the walk (import-start set empty ∧ no `"call"` node ∧ not `.swift`).
  2. The CLEAN pass over `PrebuiltEntityIndex::children_by_parent` after pass-1
     assembly; drop dirty files' facts (I1, I6).
  3. Fix the seed order to `entity_ranges` order in the precompute (I2 / F1) —
     this is a **latent divergence in today's JS/TS path**, so it lands with its
     own bit-identical gate on monster + tiptap before anything else changes.
  4. `lang_salt` / schema bump so existing corpora do not first-writer-wins-deny
     the new facts (I5 / F2).
  5. Gates: bit-identical `index.sem` sha256 + sorted `edge_dump_probe` on
     rails, HA, monster, dotnet, llvm, linux; six `index_probe` oracles;
     `facts_probe` 8/8; `facts_corpus_probe` 2/2; suites 612+/3/248/93;
     **peak RSS measured on dotnet and llvm against §5.3's projection, with a
     stated ceiling — if the real number exceeds +15% of peak, phase 1 stops and
     the memory levers in §5.3 are taken first.**
- **Phase 2 — `import_stmts` descriptors. ~3-4 days.** Unlocks Rust, Go, Java,
  and the larger half of Python. Six handlers refactored to descriptors; the
  order-equivalence witness is BS3's existing
  `import_replay_order_is_load_bearing` extended to the descriptor path, with the
  document-order variant held RED as the positive control.
- **Phase 3 — `ctor_call_sites` descriptors. ~1-2 days.** Python only; completes
  HA's −14.8%.
- **Phase 4 (optional) — relax `SCOPE_RESOLVE_BYTE_BUDGET` chunking** once
  fast-path files no longer enter `parsed_files`. Deletes ~29 chunk barriers on
  dotnet. Sized after phase 1's memory measurement, not before.

---

## 7. Surfaced findings (reported, not fixed — this bead is design-only)

- **F1 — latent seed-order divergence in today's JS/TS precompute.**
  `precompute_js_ts_file_facts` seeds `scopes[0].defs` by iterating this file's
  entities in **extraction order** (`scope_resolve.rs:1172`), while the AST path
  iterates `entity_ranges[file]`, sorted `(start_line, end_line, id)`
  (`scope_resolve.rs:1927`). `defs.insert` is last-write-wins, so the two can
  disagree for two same-named top-level entities on the **same line** — the exact
  shape `test_same_line_duplicate_parent_ids_are_propagated_to_children` already
  documents for ids. It has never been observed (monster is bit-identical), but
  it is an unstated invariant, and extending the producer to C++ (namespace-scope
  overloads) widens its reach. Impact: a single-edge flip, of the semx-nuv class.
- **F2 — first-writer-wins corpus dedup blocks producer upgrades.** §fqh made
  `CorpusFile` dedup first-writer-wins on the stated grounds that *"the only
  observable difference is `precomputed` presence, which costs speed on a later
  build, never correctness."* That is true of a *fixed* producer. Any change that
  makes a previously-`None` file precomputable is silently denied by an existing
  corpus entry until the key changes. Impact: the whole of this change would
  appear to do nothing on any machine with a warm corpus.
- **F3 — 249,740 within-file duplicate entity ids in elasticsearch (Java)**,
  2,708 in kubernetes (Go), 2,602 in llvm, 195 in dotnet, 13 in monster, 0 in HA.
  Harmless to CLEAN (both sides of a within-file collision are in the same file,
  so the file-local and corpus-wide maps merge them identically) and pre-existing,
  but 30% of elasticsearch's Java entities colliding on id is a signal about the
  Java extractor's disambiguation that no bead has looked at.
- **F4 — the Go import handler fires on Java and Swift trees**
  (`import_declaration` is shared), documented as existing behavior in
  `fused_scope_refs_import_walk`'s plan and preserved by BS3. Noted because
  phase 2's descriptor refactor must preserve it verbatim.

## 8. Gates for this bead

- **No production code changed.** `git diff --stat` touches only this file; the
  one added file is `crates/sem-core/examples/mul_census.rs`. Untouchables
  (`README.md`, `examples/hosted-diff/*`, `languages.rs` reflow hunks, and the
  five WIP `sem-cli` files) byte-identical, confirmed before and after.
- Because nothing was implemented, the bit-identical / oracle / suite battery an
  *implementing* bead owes was **not run**, and is stated as not run rather than
  implied. `cargo build --release --example mul_census -p sem-core` and
  `cargo build --release -p sem-cli --bin sem` are clean (one pre-existing
  unrelated `sem-cli` warning in `commands/setup.rs`, a WIP untouchable).
  `rustfmt --check` and `cargo clippy --release --example mul_census` are
  **clean on the probe — zero warnings attributed to `mul_census.rs`** — and the
  probe was re-run after formatting, reproducing HA's census byte-for-byte
  (`clean_semantics=18148 clean_and_treeless=1573
  clean_and_treeless_bytes=268260`).
- **Raw runs**: 7 census runs (dotnet, llvm, HA, monster, kubernetes,
  rust-lang-rust, elasticsearch); 3 profiled cold CLI builds at
  `SEM_PROFILE_RESOLVE=2`; 2 memory-profiled cold builds at `SEM_PROFILE_MEM=1`
  (dotnet, monster); 3 facts-store sizing builds (monster, dotnet, HA). Every CLI
  run used a fresh `SEM_CACHE_DIR`, removed afterwards; no user cache or shared
  corpus was written.
- **n=1 per timing arm** and a box measurably ~1.3-1.7× slower than
  RESOLUTION-PROFILE's LOCAL-COLD sections. Stated, not hidden: every conclusion
  in §5 is a ratio within a single run, and the one cross-checkable absolute
  (dotnet's re-parse, ~10.6 s scaled) agrees with W3 §5's independent ~11.2 s.

Bead: semx-w5k.1 (MUL-A). Epic: semx-w5k. Parent thesis: semx-mul.
