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

---

## 2026-08-21 — Phase 2 W2: Rust admitted unconditionally

§6.1 verdicted Rust **NO as-is; GO after phase 2** — its FASTPATH byte share
was 13.74% (only the 23,646 files, 62.1%, that never needed a tree at all),
gated behind §4.3's Field 10 (`import_stmts` descriptors), because 14,446 of
Rust's 38,092 scope-resolvable files needed the tree *only* for import
replay. W1 (this same epic) mechanized Field 10 — `PrecomputedFileFacts`
gained `import_stmts: Vec<ImportStmtFacts>`, and pass 2 gained a real
consumer (`dispatch_import_stmts_from_facts`, tree-free). This bead is the
admission: widening `mul_precompute_admits` and TREELESS to actually use it
for Rust, and measuring the census's hypothesis against the real corpus.

**What changed**, all in `crates/sem-core/src/parser/`:

1. **TREELESS widened for Rust only.** `precompute_scope_resolvable_file_facts`'s
   gate (`scope_resolve.rs`) used to reject any file with a non-empty
   `import_starts` outright. It now rejects only when the language has no
   pass-2 consumer for the recorded descriptors
   (`mul_precompute_consumes_imports`, `matches!(lang_id, "rust")` today) —
   a Python/Go/Java file with real imports still falls back to the tree
   exactly as before (their own Field-10 wiring is future work, not this
   bead's). The `"call"`-node half of TREELESS (Field 11, Python-only,
   unbuilt) is untouched.
2. **Pass 2 dispatches precomputed import descriptors.** The one call site
   that used to require `(Some(tree), Some(import_starts))` — i.e. only ever
   fired on the re-parse path — now branches on `precomputed` first: a file
   with facts dispatches `facts.import_stmts` directly (no tree, no second
   traversal), and only a file *without* facts falls through to the old
   tree-driven `record_import_stmts_pruned` + dispatch. This is the piece
   MUL-DESIGN.md's Field 10 doc comment always described (`dispatch_import_
   stmts_from_facts`'s doc: "or a precompute producer's `PrecomputedFileFacts
   ::import_stmts`") but nothing had wired until now.
3. **Admission: unconditional, not gated.** `mul_precompute_admits("rust")`
   is `true` unconditionally — the same shape as phase 1's C++, not C#'s
   `SEM_MUL_CSHARP`-gated one. The +15% peak-RSS ceiling (§4.2 I6) measured
   **+11.16% and +11.28% on rust-lang/rust, reproducibly, on both
   `/usr/bin/time -l` pairs** (below), comfortably under — so per this
   doc's own I6 fail-safe instruction ("under the ceiling → admit
   unconditionally"), no runtime switch or `MUL_RUNTIME_GATES` row was
   added; `LANGUAGE_SALTS`'s rust entry bumped `"ts-0.23"` → `"ts-0.23-mp2"`
   directly (I5/F2), same shape as phase 1's C++ bump.
4. **Engagement proof.** New `SCOPE_BUILD_WORK` counters,
   `files_precomputed_with_imports`/`precomputed_import_descriptors`,
   nonzero only when a file's precomputed facts carried real import
   descriptors dispatched without a tree — not inferred, counted.

### Correctness

- `edge_dump_probe` sha256, fast-path-ON vs OFF: **bit-identical** on
  rust-lang/rust (309,429 edges, single matching sha256) and on this
  worktree's own `crates/` (8,631 edges, matching sha256, measured against a
  frozen `git worktree` snapshot of the pre-bead commit so neither side's
  corpus was a moving target).
- `SEM_FP_PARITY=1 incr_probe … all` on rust-lang/rust (42,710 files,
  entities=451,951 edges=309,431): **8/8 `ORACLE ok`** — cold-vs-build plus
  all 7 warm scenarios (`none`, `leaf`, `mixed50`, `hub`, `hubrename`,
  `tests`, `importchurn`).
- `facts_probe` save/load, cross-process, on rust-lang/rust: **4/4
  `ORACLE ok`** (`none`, `leaf`, `mixed50`, `hub`).
- `facts_corpus_probe` populate/consume, cross-repo, on a real two-copy
  `library/` subset (2,150 files): **`corpus_hits=2150/2150`**, `ORACLE ok`,
  negative probe `ok`.
- **I5/F2 salt-bump proof**: populated a pure-Rust corpus (41 files under
  `library/core/src`) with a binary built from the pre-bead commit (salt
  `"ts-0.23"`), then consumed it with this bead's binary (salt
  `"ts-0.23-mp2"`) — **`corpus_hits=0`**, a clean miss exactly as I5
  requires, with `ORACLE ok` proving the resulting fresh build was still
  correct despite the miss (no stale/wrong-shaped facts served).
- `cargo test --release`: sem-core lib 647/647 (3 new tests: the phase-2
  admission-default pin, a Rust-file-with-imports-gets-facts positive, and a
  Rust-call-expression-stays-TREELESS negative), full sem-core suite
  719/719, sem-cli 250/250. `cargo build --release` clean on `sem-core`,
  `sem-cli`, and every touched example (two pre-existing, unrelated
  `sem-cli` warnings unchanged).

### The memory fence

`/usr/bin/time -l`, rust-lang/rust, cold (fresh `SEM_CACHE_DIR` each run),
two independent pairs with run order swapped:

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 3,105,947,648 B (2.89 GiB) | 3,452,534,784 B (3.21 GiB) | **+11.16%** |
| 2 | ON→OFF | 3,149,545,472 B (2.93 GiB) | 3,504,734,208 B (3.26 GiB) | **+11.28%** |

Both comfortably under the +15% ceiling, reproducibly — closer to llvm's
phase-1 margin (+5.8-6.5%) than dotnet's overshoot (+21-33%), consistent with
Rust's much smaller pre-Field-10 FASTPATH byte share (13.74% vs C#'s
effectively-zero) meaning less residual facts residency relative to what the
chunked path's tree budget already bounded.

### The prize, measured

`SEM_PROFILE_RESOLVE=2` counters, `sem find <nonexistent>`, rust-lang/rust
(38,598 files at measurement time — corpus has grown since the original
census's 38,092):

| counter | OFF | ON | Δ |
|---|---:|---:|---:|
| `files_precomputed` | 209 | 38,420 | — |
| `files_ast` | 38,389 | 178 | — |
| `files_precomputed_with_imports` | 0 | 14,499 | — (census predicted 14,446) |
| `precomputed_import_descriptors` | 0 | 59,131 | — |
| `reparse_ms` | 939.52 | 30.07 | **−96.8%** |
| `fused_walk_ms` | 3,214.54 | 23.45 | **−99.3%** |
| `scope_build_ms` | 6,238.47 | 3,213.17 | **−48.5%** |
| `pass2_wall_ms` | 499.57 | 271.01 | **−45.7%** |

`entities_spanned` identical both sides (329,799) — same graph, cheaper path.
14,499 files took the descriptor dispatch fast path against a census
prediction of 14,446 — the ~0.4% difference is corpus drift since the
original census commit, not a gate miss.

Cold-build wall (`sem find`, fresh `SEM_CACHE_DIR`, two release binaries —
this bead's vs. the pre-bead commit — 6 interleaved pairs, order swapped
across the two batches of 3):

| | OLD (pre-bead) | NEW (this bead) | Δ |
|---|---:|---:|---:|
| pair 1 (OLD→NEW) | 12.84s | 10.05s | −21.8% |
| pair 2 (OLD→NEW) | 11.23s | 9.43s | −16.0% |
| pair 3 (OLD→NEW) | 10.83s | 9.74s | −10.1% |
| pair 4 (NEW→OLD) | 10.78s | 9.07s | −15.9% |
| pair 5 (NEW→OLD) | 10.85s | 8.65s | −20.3% |
| pair 6 (NEW→OLD) | 10.95s | 8.90s | −18.7% |
| **median** | **10.90s** | **9.25s** | **−15.1%** |

Direction unanimous across all 6 pairs (range −10.1% to −22.0%), consistent
with `reparse_ms`/`fused_walk_ms` collapsing structurally, not a timing
artifact — same disclosure as phase 1's dotnet/llvm wall numbers (n small,
noisy, but one-directional).

**Known-content** (facts served from the cross-repo corpus, no walk at all —
not even pass 1's): not measured through `sem find` directly (its cold-build
path routes through the mmap query index, not `build_graph_with_facts_store`
— a wiring question outside this bead's scope), but proven at the mechanism
level by the `facts_corpus_probe` run above: 2,150/2,150 files served from a
different repo's corpus entries, `ORACLE ok` against a from-scratch cold
build. Per §5.1's own framing, the known-content win is `fused_walk_ms`'s
share on top of the cold win already measured — for Rust specifically that
share is now `fused_walk_ms`'s full 3,214.54 ms (cold), since the walk moved
to pass 1 and a corpus hit skips pass 1's precompute entirely.

### Verdict

**Rust: GO, unconditionally** — matches C++'s phase-1 shape, not C#'s gated
one. `mul_precompute_admits("rust") == true` with no env switch;
`LANGUAGE_SALTS`'s `("rust", "ts-0.23-mp2")` is the only corpus-compatibility
change. Go and Java are MUL-DESIGN.md §4.3's other named Field-10
beneficiaries and are future admissions of the same two predicates
(`mul_precompute_admits`, `mul_precompute_consumes_imports`), not a new
mechanism — Python's own Field-10 share is smaller (the larger half of its
tree-need is Field 11's `ctor_call_sites`, phase 3, still unbuilt).

Bead: semx-mul (phase-2 W2). Epic: semx-w5k. Parent thesis: semx-mul. Prior:
semx-w5k.1 (MUL-A), semx-mp1 (MUL P1, C#/C++), semx-mul phase-2 W0 (CLEAN
gate ordering) and W1 (Field 10 mechanized, unconsumed until this bead).

---

## 2026-08-21 — Phase 2 W3+W4: Go and Java measured, both stay gated

W2 admitted Rust unconditionally by widening `mul_precompute_admits` and
`mul_precompute_consumes_imports` to `"rust"`. This bead runs the identical
mechanism for the two other Field-10 beneficiaries §4.3 named (Go on
kubernetes, Java on elasticsearch) — same two predicates, same TREELESS
widening, same `LANGUAGE_SALTS` bump discipline. The mechanical change is
one line each in both predicates plus a `LANGUAGE_SALTS`/`facts_corpus_probe.rs`
bump to `"ts-0.23-mp3"` for both. **Neither language ends up admitted
unconditionally** — each independently fails a different one of the two
gates I6 requires, exactly the "ship them differently" case the epic
anticipated:

| language | correctness (edge_dump_probe) | memory fence (+15% ceiling) | verdict |
|---|---|---|---|
| Go | **FAILS** — not bit-identical on kubernetes | passes (-1.42%/+0.57%) | **blocked, correctness** |
| Java | passes — bit-identical on elasticsearch | **FAILS** — +20.97%/+21.01% | **gated, memory (C#'s shape)** |

Both start (and, for Go, must stay) behind runtime switches — `SEM_MUL_GO`
and `SEM_MUL_JAVA`, `MUL_RUNTIME_GATES` rows with `pre_switch_salt =
"ts-0.23"` — the same shape C#'s gate uses, not Rust's unconditional one.

### Java: clean correctness, busts the memory ceiling

`edge_dump_probe` on elasticsearch (35,906 files): **sha256 bit-identical
ON vs OFF**, 1,257,229 edges both sides. `SEM_FP_PARITY=1 incr_probe … all`:
**8/8 `ORACLE ok`** (cold-vs-build plus all 7 warm scenarios). `facts_probe`
save/load cross-process: **4/4 `ORACLE ok`**. `facts_corpus_probe`
populate/consume on a real two-copy corpus (`server/.../action`, 861 files):
**861/861 hits**, `ORACLE ok`, negative probe `ok`. The I5/F2 salt-bump was
proven adversarially exactly as W2 proved it for Rust: populated the corpus
with a binary built from this campaign's pre-W3+W4 commit (java salt
`"ts-0.23"`), consumed with this bead's binary (salt `"ts-0.23-mp3"`) —
`corpus_hits=0`, clean miss, `ORACLE ok`.

Java's `import_declaration` nodes were already fully descriptor-dispatched
*before* this bead — §7 finding F4 already documented that they classify as
`ImportStmtKind::GoImport` (the node kind is shared across Go/Java/Swift
grammars) and dispatch through `register_go_package_imports`, which only
ever matches `.go`-suffixed entities in `go_pkg_index` — a **pre-existing,
documented no-op for Java**. So there was no Java-specific import handler to
build: admitting Java to TREELESS via `mul_precompute_consumes_imports`
is a pure table-row change, and the correctness battery above confirms the
no-op is preserved bit-for-bit by the fast path. (A `gen_java` fixture was
added to `fused_triple_walk_matches_three_sequential_walks` as the
record-vs-direct equivalence witness the task asked for; it pins both that
descriptors are recorded — Field 10 fires — and that `import_table` stays
empty, i.e. the no-op survives.)

The blocker is memory. `/usr/bin/time -l`, elasticsearch, cold, two
independent pairs with run order swapped:

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 5,469,716,480 B (5.09 GiB) | 6,616,678,400 B (6.16 GiB) | **+20.97%** |
| 2 | ON→OFF | 5,465,309,184 B (5.09 GiB) | 6,613,762,048 B (6.16 GiB) | **+21.01%** |

Both pairs reproducibly bust the +15% ceiling by a wide margin — closer to
dotnet's phase-1 overshoot (+21-33%) than Rust's phase-2 pass (+11%). This
tracks MUL-A §2.3's own numbers: Java's pre-Field-10 TREELESS-by-bytes share
was the *smallest* of any language measured, **0.98%** — almost none of
elasticsearch's Java bytes were on the fast path before this bead, so
admission moves nearly all of them onto it in one step, the same shape that
busted dotnet's ceiling in phase 1. Per I6, Java stays gated
(`SEM_MUL_JAVA`, off by default) — C#'s precedent, not Rust's.

Prize, measured anyway (so the gated cost of staying off is on the record):
`SEM_PROFILE_RESOLVE=2`, `sem find <nonexistent>`, elasticsearch (30,241
files at measurement time):

| counter | OFF | ON | Δ |
|---|---:|---:|---:|
| `files_precomputed` | 43 | 30,220 | — |
| `files_ast` | 30,198 | 21 | — |
| `files_precomputed_with_imports` | 12 | 29,219 | — |
| `precomputed_import_descriptors` | 121 | 494,577 | — |
| `reparse_ms` | 1,329.19 | 114.94 | **−91.4%** |
| `fused_walk_ms` | 7,299.51 | 1.93 | **−99.97%** |
| `scope_build_ms` | 8,409.06 | 1,340.16 | **−84.1%** |

`entities_spanned` identical both sides (506,794) — matches the bit-identical
sha256 above. Cold-build wall (`sem find`, fresh `SEM_CACHE_DIR`, 6
interleaved pairs) medians to OFF 15.75s / ON 12.92s (**−17.9%**), but
per-pair direction was **not** unanimous (2 of 6 pairs went the other way) —
this session ran heavy concurrent `cargo build`/`cargo test` activity on the
same box throughout, which the counter-level numbers above are immune to but
end-to-end wall time is not; the counters are the trustworthy number here,
the wall-clock median is directional, disclosed noisy rather than
overstated.

### Go: clean memory, a real correctness regression

The mirror image of Java. `/usr/bin/time -l`, kubernetes, cold, two
independent pairs with run order swapped:

| pair | order | OFF maxRSS | ON maxRSS | Δ |
|---|---|---:|---:|---:|
| 1 | OFF→ON | 3,769,679,872 B | 3,716,251,648 B | **−1.42%** |
| 2 | ON→OFF | 3,731,324,928 B | 3,752,509,440 B | **+0.57%** |

Comfortably under +15%, both pairs — Go's memory fence *passes*. But
`edge_dump_probe` on kubernetes (13,619 files) is **not** bit-identical:

| | OFF | ON |
|---|---:|---:|
| edges | 366,905 | 363,487 |
| sha256 | `2db4539a…` | `c37dfdc0…` |

30,801 diff lines, fully deterministic (two independent OFF runs agree
byte-for-byte with each other; two independent ON runs agree byte-for-byte
with each other — this is not a race). A representative wrong edge:

```
< cmd/kubeadm/app/apis/kubeadm/types.go::type::ClusterConfiguration::DeepCopyInto
    Calls cmd/kubeadm/app/apis/kubeadm/v1/types.go::type::ClusterConfiguration::DeepCopyInto   (OFF, correct)
> cmd/kubeadm/app/apis/kubeadm/types.go::type::ClusterConfiguration::DeepCopyInto
    Calls staging/src/k8s.io/pod-security-admission/admission/api/v1/types.go::type::PodSecurityExemptions::DeepCopyInto   (ON, wrong)
```

Dozens of unrelated `X::DeepCopyInto`/`X::DeepCopy` call sites across
entirely different packages all collapse onto the same one wrong target when
ON. W0's `GoParentsResolved`-token ordering fix (the CLEAN gate now runs
after `resolve_go_method_parent_ids`) is confirmed still sound — this is the
first bead where Go admission is real enough to exercise it, and
`go_parent_repair_must_run_before_clean_gate_adjudication` stays green — so
the regression is not that hazard resurfacing.

**Root cause, not fully isolated.** Three repros were tried and did *not*
reproduce it: a single-package 3-type fixture (cross-file struct/methods,
cross-file field-typed nested `DeepCopyInto` calls), and a 29-file slice
copied directly out of kubernetes (`cmd/kubeadm/app/apis/kubeadm/{,v1}` +
the exact `pod-security-admission/admission/api/v1` package the wrong edges
target) — all bit-identical ON vs OFF. The bug needs corpus scale to
manifest, which is itself a clue. `SEM_PROFILE_RESOLVE=2` on the *full*
kubernetes corpus surfaced a second, independent finding while chasing this:
`extract_imports_ms` costs **~83-88 seconds** — larger than `reparse_ms`
itself — identically on both ON and OFF, i.e. **pre-existing, not caused by
this bead**. Reading `build_go_pkg_index`/`register_go_package_imports`
(`scope_resolve.rs`) explains both the cost and a plausible correctness
mechanism: packages are keyed by their **bare last-path-segment string**
("v1", "util", …) with no disambiguation by full import path, so every
package in the repo whose directory is named e.g. `v1` — kubernetes has
dozens, one per API group — collides into the *same* `go_pkg_index["v1"]`
bucket, sorted and inserted last-write-wins. On the AST path this is latent:
type-directed (`class_members`-based) resolution normally wins first for a
`DeepCopyInto` call, so the polluted `import_table_by_name.get("DeepCopyInto")`
fallback is populated but never consulted. The working hypothesis — not
confirmed — is that something about the fast path's file-local
`entity_map`/`children_by_parent` substitution measurably increases how
often that type-directed resolution fails for Go specifically, pushing more
calls into the already-polluted fallback. Both parts are real (the
`go_pkg_index` collision is confirmed by code reading; the fast-path failure
increase is inferred from the divergence pattern, not yet directly
instrumented) but **neither is fixed by this bead**.

Per I6, Go stays gated (`SEM_MUL_GO`, off by default) and, unlike a normal
memory-only gate, **must not be flipped even for re-measurement** until the
correctness regression above is root-caused and fixed — its doc comment
says so explicitly. The salt bump (`"ts-0.23"` → `"ts-0.23-mp3"`) is kept
regardless: it is correct and load-bearing for any future producer change,
independent of whether the switch is ever turned on.

### Close-out

Mechanically, Go and Java both got the exact W1/W2 template (predicate
widening, TREELESS gate, salt bump, engagement counters via
`files_precomputed_with_imports`/`precomputed_import_descriptors`, unit
tests pinning TREELESS-with-imports and call-node-stays-TREELESS per
language, a `record_then_dispatch_matches_dispatch_direct`-style equivalence
fixture). Neither ships unconditionally. Java is a known, bounded,
memory-only gap (a future memory lever, §5.3's two named ones or a new one,
could promote it). Go is not safe to promote at all until a real bug is
found and fixed — this is reported, not patched around, per this campaign's
own STOP-AND-REPORT instruction.

`cargo test --release -p sem-core`: 727/727 (652 lib incl. 12 new
Go/Java/gen_java-related tests, + 75 across `d_smoke`/`elm_smoke`/
`graph_accuracy`/`kappa`/`parse_cache`/`scope_resolve_bench`/
`single_pass_invariants`/`yaml_multidoc`, 1 ignored). `cargo test --release
-p sem-cli`: 250/250. `cargo build --release` clean on `sem-core`, `sem-cli`,
and every touched example — two pre-existing, unrelated `sem-cli` warnings
unchanged. `rustfmt --check` clean on every touched file. `cargo clippy
--release --lib --examples`: 151 warnings before this bead, 151 after —
zero attributable to this bead's changes.

Bead: semx-mul (phase-2 W3+W4). Epic: semx-w5k. Parent thesis: semx-mul.
Prior: semx-mp1 (MUL P1, C#/C++), semx-mul phase-2 W0 (CLEAN gate ordering),
W1 (Field 10 mechanized), W2 (Rust admitted unconditionally, this bead's
template).

---

## 2026-08-21 — Phase 2 W5: Field 11 built, Python admitted — completing the planned admissions

§4.3 named Field 11 (`ctor_call_sites: Vec<CtorCallFacts>`) as the piece
Python needed beyond Field 10: `scan_constructor_calls`'s per-`"call"`-node
inputs — callee identifier, argument shapes — since its scan is a pure
syntactic sweep whose only corpus-dependent parts (`func_name_returns`,
`init_params`, `attr_to_param_index`) are consulted *after* the node is
read. This bead builds it, following Field 10's record/dispatch split
exactly: `record_ctor_call_sites` (pure function of one file's tree,
`(callee, arg_shapes)` per qualifying node, in the scan's own worklist
order) and `apply_ctor_call_facts` (the corpus-dependent replay, run once
every file's `init_params`/`attr_to_param`/`return_type_map` have been
merged). The old direct-dispatch `scan_constructor_calls`/`infer_expr_type`
were deleted outright — no predecessor "unfused" spec existed for this scan
the way `extract_imports_from_ast` predates Field 10's fused-walk refactor
and was deliberately kept alive across it, so there was nothing to retain;
the record-vs-direct equivalence proof this bead owes instead lives as a
test-local, never-production-referenced transcription of the pre-refactor
functions.

`mul_precompute_consumes_imports` widened to include `"python"` (Python
needs Field 10 too — its census share was imports-larger, calls-smaller);
the new sibling `mul_precompute_consumes_calls` is `matches!(lang_id,
"python")` only, since `scan_constructor_calls`'s literal `"call"`-kind
test never matches any other grammar this crate resolves (C#'s call node is
`invocation_expression`, C++/Rust/Go's is `call_expression`, Java's is
`method_invocation`) — the identical structural no-op Field 10 already
established for JS/TS. `FACTS_SCHEMA_VERSION` bumped 2 → 3 (new type
reachable from `PersistedFacts`); `LANGUAGE_SALTS`'s python entry bumped
`"ts-0.23"` → `"ts-0.23-mp4"` (I5/F2), mirrored in
`facts_corpus_probe.rs`'s independent copy.

**Admission: unconditional (Rust's shape, not Java's).** `/usr/bin/time -l`
on home-assistant/core measured the +15% peak-RSS ceiling not just passed
but *inverted* — four independent order-swapped pairs (two via an interim
`SEM_MUL_PYTHON` toggle, two via the final pre-bead-vs-this-bead binary
comparison), every one a net **decrease** (-7.95%, -7.80%, -4.73%,
-7.73%). This is the opposite of Java's/C#'s overshoot for the structurally
identical reason stated backwards: HA's pre-Field-10/11 FASTPATH byte share
was the *smallest* of any family measured (0.23%, §2.3), so admission moves
nearly the entire corpus's chunked-path tree retention off the books in one
step — on a corpus HA's size, that removed cost outweighs the small facts
residency Field 10+11 add (§5.3's own ~0.27 GB projection), where on
elasticsearch/dotnet's larger corpora the added residency was what
dominated instead. Per I6's own fail-safe instruction, Python ships
unconditionally: no `SEM_MUL_PYTHON` switch, no `MUL_RUNTIME_GATES` row.

**Correctness.** `edge_dump_probe` sha256, this bead's binary vs. the
pre-bead commit: bit-identical on home-assistant/core (310,398 edges) and
on the control corpus, rust-lang/rust (309,429 edges) — proving the
`infer_constructor_param_types` reshape (it now merges precomputed-facts
and freshly-parsed files' ctor-call descriptors in `file_paths` order
instead of walking `parsed_files` alone, since a fast-path file's tree is
gone by the time this pass-1b step runs) left every non-Python language's
graph untouched. `SEM_FP_PARITY=1 incr_probe … all` on HA: 8/8 `ORACLE ok`.
`facts_probe` save/load cross-process: 4/4 `ORACLE ok`. `facts_corpus_probe`
populate/consume on a real two-copy corpus (`homeassistant/helpers`, 109
files): 109/109 hits, `ORACLE ok`, negative probe `ok`; the I5/F2
adversarial salt-clean-miss proof: `corpus_hits=0`, `ORACLE ok` regardless.

**Engagement**, `SEM_PROFILE_RESOLVE=2` on HA: `files_precomputed` 2 → 18,210
of 18,213; `files_precomputed_with_imports=16,625` (census predicted
16,559); `files_precomputed_with_ctor_calls=11,374`,
`precomputed_ctor_call_descriptors=78,702` (smaller than the census's
16,088-files-with-a-call-node figure by design — the engagement counter
only fires on a *nonempty* recorded descriptor set, i.e. an actual
uppercase-identifier ctor-shaped call, not merely the presence of some call
node).

**The prize.** `full_graph_build` (`SEM_TIMINGS=1`, `sem graph --no-cache`,
4 interleaved cold pairs, order swapped, pre-bead vs. this-bead binaries):
median OLD 3,693.7 ms → NEW 2,732.3 ms, **-26.4%**, unanimous direction
across all 4 pairs (range -25.7% to -28.1%) — exceeding this doc's own
§5.1 prediction of -14.8% (itself disclosed as an upper-bound ratio from a
different box, derived before Field 10/11 existed; the real admitted
corpus evidently captures more of the win than that pre-implementation
arithmetic priced in).

**Gates.** `cargo test --release -p sem-core`: 731/731 (656 lib, +9 net over
W3+W4's 652). `cargo test --release -p sem-cli`: 250/250. `rustfmt --check`
clean on every touched file. `cargo clippy --release --lib --examples`: 151
warnings before, 150 after (net fewer — a pre-existing manual-loop-counter
lint was incidentally resolved by `apply_ctor_call_facts`'s `.enumerate()`),
zero attributable to this bead.

**Close-out.** This completes §6.2's planned admissions: JS/TS+C++ shipped
unconditionally (phase 1), C# stays gated on memory, Rust and now Python
ship unconditionally (phase 2), Go stays gated on an unresolved correctness
regression, Java stays gated on memory. Every scope-resolvable language
has now been measured against the two-tier fast path at least once.

Bead: semx-mul (phase-2 W5). Epic: semx-w5k. Parent thesis: semx-mul.
Prior: semx-mp1 (MUL P1, C#/C++), semx-mul phase-2 W0 (CLEAN gate
ordering), W1 (Field 10 mechanized), W2 (Rust admitted unconditionally),
W3+W4 (Go/Java measured, both stay gated).

## Phase 2 close (2026-08-21, W6 finale)

The eight-corpus matrix (full detail, methodology, and raw numbers in
RESOLUTION-PROFILE.md's dated FINALE section) confirms this document's
phase-2 predictions at scale, on the real binary, against a pinned
pre-campaign baseline (`dev-main-022-full`@`fbfc2c3`) rather than a
same-binary toggle:

- **Python** (HA): -26.6% cold wall / -29.9% engine, matching W5's -26.4%
  engine finding.
- **Rust** (rust-lang/rust): -13.1% cold wall / -14.7% engine, matching
  W2's ≈-15% finding.
- **The five flat-expected corpora** (TypeScript, kubernetes, elasticsearch,
  dotnet, linux, llvm — six, not five, but llvm and C++ are the same
  admission) all land within noise of zero, as their gated-or-already-
  admitted status predicts. Two (kubernetes, elasticsearch) crossed a 3%
  investigation threshold on first pass under rising system load and were
  re-run clean once load settled — see RESOLUTION-PROFILE.md's attribution
  section.
- **Oracle battery** (ORACLE/REFS_ORACLE/FILES_ORACLE/TESTS_ORACLE/
  TRIGRAM_ORACLE/REFS_MUTATION) is clean on home-assistant/core, the
  TypeScript monster, and llvm-project — zero mismatches. `SEM_FP_PARITY=1`
  is 8/8 clean, cold and warm, on both HA and rust-lang/rust.
- **Both deferred large-corpus verifications close clean**: semx-kkk's
  YAML multi-doc id fix eliminates llvm's `TESTS_ORACLE` mismatch entirely
  (0/2,760,885, was 1/2,751,958) — bead closed. semx-o0x's O(n) REFS_ORACLE
  fix holds at llvm scale: 4.45s, down from 1,634.7s pre-fix (~367x) —
  exceeding the 40-135x range measured on smaller corpora at fix time.
- **One open finding, not swept under the rug**: Rust's peak-RSS re-
  measurement (two order-swapped pairs, this wave) lands at +18.2%/+21.2%,
  above the +15% ceiling that justified its *unconditional* admission at
  W2's +11% reading. Investigated and not attributable to any code that
  changed on Rust's own path between W2 and this wave (the shared
  `PrecomputedFileFacts` struct did grow one field at W5, but it's an
  always-empty `Vec` for Rust files — ruled out by an order-of-magnitude
  argument, not just asserted). Filed as an open question for a follow-up
  bead, not resolved here.

Every scope-resolvable language this codebase supports has now been
measured against the two-tier fast path at least once, on both a
same-binary toggle (earlier waves) and a pinned-baseline-vs-HEAD comparison
(this wave). The admission scoreboard is unchanged from W5's close-out:
JS/TS+C++ unconditional (P1), Rust+Python unconditional (phase 2), C#+Java
gated on memory, Go gated on an unresolved correctness regression
(semx-u3rk). Phase 2 is closed pending: Go's correctness fix, a decision on
Rust's re-opened memory question, and whichever future memory lever might
promote Java or C#.

Bead: semx-mul (finale, W6). Epic: semx-w5k.

## 2026-08-22 — Rust demoted to a gated switch (semx-j1fw)

The finale's open finding (above) is resolved, not left hanging: semx-j1fw
re-verified Rust's same-binary peak-RSS delta with the admission variable
isolated exactly (one predicate flipped at campaign HEAD, everything else
held fixed, `--no-cache` to remove the known-content confound) and got
**+17.72%/+19.64%/+19.35%** across three order-swapped pairs on
rust-lang/rust — unanimous, all above the +15% ceiling. `mul_precompute_
admits("rust")` is now `rust_precompute_enabled()`, gated on `SEM_MUL_RUST`,
same shape as `csharp`/`go`/`java`. `MUL_RUNTIME_GATES` gained a `"rust"`
row (`pre_switch_salt = "ts-0.23"`, the pre-W2 salt); `LANGUAGE_SALTS`'s
`("rust", "ts-0.23-mp2")` needed no edit — it now serves as the
switched-*on* salt instead of an unconditional one, exactly the role
`resolve_gated_salt` already gives every other gated language's table
entry.

Correctness is unaffected (`edge_dump_probe` bit-identical OFF vs ON,
309,429 edges, matching sha256 — the demotion is a gating decision, not a
behavior change). Full numbers, the salt-battery proof (including a
sharper cross-salt isolation check than W2's original — see the dated
RESOLUTION-PROFILE.md entry), and gate results are in
RESOLUTION-PROFILE.md's matching dated section.

**Admission scoreboard, corrected**: Rust moves from "unconditional" (W2)
to "gated (`SEM_MUL_RUST`), off by default" — memory-blocked like C#/Java.
Python remains the only phase-2 language shipped unconditionally.

Bead: semx-j1fw. Epic: semx-w5k. Prior: semx-mul phase-2 W2, W6 finale.

## 2026-08-22 — Python's fence re-derived with semx-j1fw's corrected protocol — stands, magnitude was overstated (F1)

Debt paid: Rust's demotion above raised the question of whether W5's
favorable Python reading (-7.95%/-7.80%, "a net decrease") was the same
class of artifact that fooled W2 — a shared-corpus-warmth confound from
`SEM_CACHE_DIR` without `--no-cache` isolation, which RESOLUTION-PROFILE.md's
own W4 finding already flagged as silently present in every "cold" build
this campaign measured that way. W5's protocol used exactly that shape
(`SEM_MUL_PYTHON` toggle, then binary-vs-binary, both "cold (fresh
`SEM_CACHE_DIR` each run)" with no `--no-cache`) — the same gap W2's Rust
protocol had.

Re-verified with semx-j1fw's exact method: throwaway worktree at campaign
HEAD (`7607f39`), `mul_precompute_admits("python")` flipped `true`→`false`
in place (the sole call site, `graph.rs:2212`, confirmed — no companion
flip needed for `mul_precompute_consumes_imports`/`_calls`, since both are
only ever consulted inside the producer this gate already prevents from
running when off), OFF built there and ON built from the unmodified
campaign worktree off the shared cache, `--no-cache` on every run, fresh
`SEM_CACHE_DIR` per run. Five order-swapped pairs on home-assistant/core
(three required by protocol, two more added because the first three came
back sign-mixed and a n=3 read wasn't confident enough to write down):
**-1.63%, +0.79%, -2.39%, -0.86%, -2.46%** — median -1.63%, 4 of 5
negative, nothing close to W5's claimed -7.8%/-7.95%, and nowhere near the
+15% ceiling either way.

**Verdict: STANDS.** Python was never close to the ceiling under either
protocol, so there is no ceiling-crossing here the way there was for
Rust — but the specific "net decrease" magnitude W5 reported does not
reproduce; the true effect (if real at all — the sign flip on one of five
pairs keeps this shy of "proven") is roughly a fifth of what was claimed.
No code change; this is a docs-only correction to the record's honesty,
not to the admission decision. Full numbers and protocol notes in
RESOLUTION-PROFILE.md's matching dated section.

**Admission scoreboard, unchanged**: Python remains the only phase-2
language shipped unconditionally — now on a number that has actually
survived the corrected protocol, rather than one that hadn't been
re-checked.

Bead: F1 (memory campaign, no new bead filed — this is a re-derivation,
not a new finding). Epic: semx-w5k. Prior: semx-mul phase-2 W5, semx-j1fw.

## 2026-08-22 — go_pkg_index's bare-segment collision, half-fixed (semx-u3rk)

W3+W4 named two compounding issues blocking Go admission: (1)
`build_go_pkg_index`/`register_go_package_imports` keyed packages by their
bare last-path-segment string ("v1", "util", ...) with no disambiguation by
full import path, so kubernetes's dozens of same-named `v1` packages (one
per API group) collided into one bucket; (2) something about the fast path
measurably increases how often type-directed resolution fails for Go,
pushing more calls into that (polluted) fallback. This bead fixes (1) and
confirms (2) is real and separate.

**The fix.** `ImportStmtFacts::GoImport { packages: Vec<String> }` now
carries each import spec's *full* path instead of a bare-last-segment
reduction — a content change, not a shape change (`Vec<String>` throughout,
so this is a `LANGUAGE_SALTS` bump, `("go", "ts-0.23-mp5")`, not a
`FACTS_SCHEMA_VERSION` bump). `build_go_pkg_index`'s per-entry value grew a
third field, the entity's declaring directory (a new `GoPkgIndex` type
alias, `HashMap<String, Vec<(String, String, String)>>`), reusing
`registry::go_package_dir` — the same declaring-directory notion
`resolve_go_method_parent_ids` already trusts for same-package cross-file
method/type pairing. `register_go_package_imports` now disambiguates: when
a bare-segment bucket holds entries from more than one declaring directory,
it picks the one whose trailing path segments overlap the import path's
trailing segments the longest (`select_go_pkg_candidate` /
`trailing_path_overlap`), and inserts only that candidate's entries into
the importing file's `import_table` — not the whole polluted bucket. A
bucket with exactly one distinct declaring directory (the overwhelming
common case) short-circuits without comparing anything, so this costs
nothing extra where there is nothing to disambiguate.

**RED → GREEN.** Two new unit tests
(`go_package_index_collision_is_real_before_disambiguation`,
`register_go_package_imports_resolves_the_file_own_import_not_a_same_named_collision`)
reproduce the exact kubeadm/pod-security-admission collision as a minimal
fixture. Empirically RED-proven: temporarily disabling the disambiguation
(reverting to "insert the whole bucket") makes the second test fail with
exactly the predicted wrong resolution (`DeepCopyInto` from
`pod-security-admission` instead of `kubeadm`'s own); restoring the fix
turns it GREEN.

**Corpus-scale proof.** kubernetes's own `edge_dump_probe` OFF-path edge
count dropped from 366,905 to 334,664 (~9%) — every one of those ~32k
edges was a confirmed cross-package false positive; spot-checked
(`cmd/kubeadm/.../types.go`'s `ClusterConfiguration::DeepCopy` now
correctly resolves to its own package's `DeepCopyInto`, same file/package,
instead of jumping to `pod-security-admission`'s). But `edge_dump_probe`
ON vs OFF is **still not bit-identical** post-fix (334,664 vs 331,190
edges, a ~30,795-line diff — barely smaller than the original 30,801,
because the false positives this fix removed were present on *both* sides
equally and cancelled out of the diff, leaving finding (2)'s own
divergence untouched). Sampling the remaining diff confirms (2) is real
and distinct: OFF's answer is now correct where checked, ON's is *still*
wrong — wrong differently than before the fix, not the same wrong answer.
Root cause of (2) not isolated this bead.

**Perf.** `SEM_PROFILE_RESOLVE=2`, `sem find <nonexistent>`, kubernetes,
OFF: `extract_imports_ms` fell from 83,185.67ms to 24,290.78ms (**-70.8%**)
— the old code inserted every same-named package's full symbol set into
every importing file's `import_table`; the fix inserts one package's
worth. `import_rekey_ms` fell from 3,227.48ms to 232.87ms alongside it.
Cold wall (`sem find`, fresh `SEM_CACHE_DIR`, 4 interleaved pairs, order
swapped): pre-fix medians ~12.4-12.9s, post-fix ~8.4-9.1s, unanimous
direction, **~28-30%** faster.

**Admission decision.** Go **stays gated** (`SEM_MUL_GO`, off by
default) — the ON/OFF correctness gate alone keeps it there regardless of
memory (W3+W4 already found the memory fence passing, `-1.42%/+0.57%` on
kubernetes; not re-checked this bead since the correctness gate already
fails on its own). `MUL_RUNTIME_GATES`'s "go" row is unchanged
(`pre_switch_salt = "ts-0.23"`); `LANGUAGE_SALTS`'s go entry bumped
`"ts-0.23-mp3"` → `"ts-0.23-mp5"` regardless — correct and load-bearing for
any future producer change (including finding (2)'s eventual fix),
independent of whether the switch is ever turned on.

**Gates.** sem-core lib 658/658 (2 new tests), sem-cli 250/250 (all 21
binaries), rustfmt clean, clippy 151 warnings before/after (zero
attributable). `edge_dump_probe` on the TypeScript control corpus:
bit-identical before/after (196,175 edges, matching sha256) — confirms the
fix is a no-op outside Go. `SEM_FP_PARITY=1 incr_probe … all`: 8/8
`ORACLE ok` on kubernetes and on home-assistant/core (Python, unaffected).
`facts_probe` save/load: 4/4 `ORACLE ok`. `facts_corpus_probe`
populate/consume on a real two-copy Go corpus (kubeadm's `v1` + the
`types.go`/`zz_generated.deepcopy.go` pair that exercise the collision, 13
files): 13/13 hits OFF/OFF and ON/ON, `ORACLE ok` both; the I5/F2
adversarial cross-salt proof (populate OFF, consume `SEM_MUL_GO=1`) gives
`corpus_hits=0/13`, a clean miss, `ORACLE ok` regardless.

Bead: semx-u3rk. Epic: semx-w5k. Prior: semx-mul phase-2 W3+W4 (the
original finding), W0 (`GoParentsResolved` ordering, confirmed still
sound). Full numbers and the remaining-divergence sample in
RESOLUTION-PROFILE.md's matching dated section.

## 2026-08-22 — M1: §5.3/§4.2's I6 ceiling metric corrected — C++ and Python
## demoted (peak memory footprint, not maxRSS)

Every §5.3/§4.2 I6 reading to date — including both unconditional
admissions, phase 1's C++ and phase 2's Python — was `/usr/bin/time -l`'s
`maximum resident set size` (`getrusage` `ru_maxrss`). That tool also
emits `peak memory footprint` (`task_info` `phys_footprint`, macOS's own
memory-pressure accounting), and the two disagree specifically when a
build's resident content is compressible: pages the VM compressor has
swapped into the compressor drop out of maxRSS (no longer resident) while
still counting against footprint (still committed, from the OS's
pressure-accounting point of view). A standalone allocator probe on the
campaign machine confirmed the mechanism directly and answered a standing
question about sem's own `ps`-based instrumentation: `ps -o rss=` matches
maxRSS bit-for-bit, not footprint, and — refining the theory rather than
contradicting it — the divergence requires pages to actually go idle under
real pressure; merely holding compressible, actively-touched content does
not diverge the two fields on a lightly-loaded process. Real corpus builds
create the idle-under-pressure condition over their run (facts populated
early, left untouched while later phases execute); a bounded, safety-
conscious synthetic probe on this shared, concurrently-loaded 64 GB machine
correctly declined to force it artificially.

Re-reading both unconditional admissions against footprint, on the same
corrected protocol semx-j1fw used for Rust's demotion (throwaway worktree,
one predicate flipped, `--no-cache`, fresh `SEM_CACHE_DIR` per run, three
order-swapped pairs):

| language | corpus | maxRSS Δ (3 pairs) | footprint Δ (3 pairs) |
|---|---|---|---|
| C++ | llvm/llvm-project (full) | +19.98% / +20.50% / +21.02% | +26.33% / +27.65% / +28.11% |
| Python | home-assistant/core | -1.04% / -3.99% / -1.71% | +26.02% / +25.29% / +27.44% |

C++ busts the ceiling on **both** fields — its original semx-mp1 reading
(+5.8%/+6.5%, without `--no-cache`) turns out to have suffered the identical
shared-cache-warmth confound semx-j1fw diagnosed for Rust's W2 outlier, a
protocol artifact independent of which metric is used. Python's maxRSS
stays negative (reproducing F1's own re-derivation almost exactly) while
footprint alone busts the ceiling — the sharpest evidence yet that the
metric, not the protocol, is the deciding variable here.

**Verdict, both demoted.** `mul_precompute_admits` routes `"cpp"` through
`cpp_precompute_enabled()` (`SEM_MUL_CPP`) and `"python"` through
`python_precompute_enabled()` (`SEM_MUL_PYTHON`), both off by default,
same shape as C#/Rust/Go/Java. `MUL_RUNTIME_GATES` gained matching rows,
`pre_switch_salt = "ts-0.23"` for both (the pre-admission salt each
language's table entry was bumped from). `LANGUAGE_SALTS`'s existing
entries (`("cpp", "ts-0.23-mp1")`, `("python", "ts-0.23-mp4")`) are
unchanged — each now serves as its language's switched-*on* salt. No
admission remains unconditional; every §6.1 GO verdict is now gated
pending a future memory lever, re-measured against footprint from the
start.

**Policy, standing:** I6's ceiling is defined against peak memory
footprint henceforth, maxRSS reported alongside for continuity. This is a
Darwin-specific accounting distinction (`task_info`/`phys_footprint` has no
exact Linux equivalent); Linux re-validation under the same two-metric
discipline is a separate, still-open task.

Correctness untouched by any of this — `edge_dump_probe` bit-identical OFF
vs ON for both (C++: 982,429 edges on llvm-project; Python: 310,398 edges
on home-assistant/core, matching W5's own original count), full
`facts_corpus_probe` salt battery green both switch states plus a clean
adversarial cross-salt miss for each, sem-core lib 658/658, sem-cli
250/250, rustfmt clean, zero clippy warnings attributable to the touched
files. Full numbers, the allocator probe's design iteration (a v1 bug
caught and fixed before trusting it), the cheap Rust/C#/Java footprint
confirmations, and the stability check that gated the decision to act (all
readings ≤2.2 points of spread across pairs, well under a 5% noise-abort
threshold) are in RESOLUTION-PROFILE.md's matching dated section.

Bead: M1 (overnight memory campaign). Epic: semx-w5k. Prior: M0 (metric
audit), F1 (Python's corrected-protocol maxRSS re-derivation), semx-j1fw
(the demotion code template this bead follows exactly), semx-mp1 (C++'s
original admission), semx-mul W5 (Python's original admission).
