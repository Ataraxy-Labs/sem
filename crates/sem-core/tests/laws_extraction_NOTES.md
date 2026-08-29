# Design notes: the algebraic structure of extraction and diff (sem-core)

Commit audited: `260e2a1bff9504042c0d6e39c877013a756d1fd0` (2026-08-28).
Note: the working tree was on branch `docs/social-card` at audit time (these
notes name `main`; branch switching was avoided for this audit, so the
audit ran against the checked-out HEAD above — the audited files are identical
to their `main` ancestry unless that branch touched `crates/sem-core`, which
it does not appear to).

Read-only audit: no production code was modified. Deliverables are these
notes plus two witness suites (`tests/laws_extraction.rs`,
`tests/laws_diff.rs`, shared arbitraries in `tests/laws_common/mod.rs`) and
one dev-dependency (`proptest`, test-only) added to `sem-core/Cargo.toml`.

---

## 0. MAP — the boundary and its altitudes

The audited surface, at four altitudes:

- **L3 (domain):** "semantic diff" = describe the difference between two
  files at the granularity of code entities, not lines.
- **L2 (module/access graph):** `sem-cli::commands::diff` →
  `sem_core::parser::differ::compute_semantic_diff` →
  { `parser::registry::ParserRegistry` (+ per-language plugins, tree-sitter),
    `model::entity` (ID discipline, collision disambiguation),
    `model::identity::match_entities` (the matcher),
    `differ`-local orphan detection + parent suppression }.
  The registry is the sole authority for "which plugin parses this path"
  (extension map ← `.semrc` ← `.gitattributes`, shebang fallback, then the
  declared `fallback` chunker). No ambient authority observed on this path:
  `compute_semantic_diff` is a pure function of `&[FileChange]` + registry.
- **L1 (algebra):** the carriers and laws named in §1 below.
- **L0 (code detail):** phase list in `match_entities`, line-set arithmetic
  in `detect_orphan_changes`, LNDS in `detect_reorders`, hash choices in
  `utils::hash`.

The public boundary used by every witness: `ParserRegistry::extract_entities`
(extraction; includes ID disambiguation and byte-range fill — the same
composition the differ applies) and `compute_semantic_diff` (the exact entry
point `sem diff` calls at `crates/sem-cli/src/commands/diff/mod.rs:1720`).

---

## 1. Findings ladder (ranked by leverage)

### F1 — Extraction is a per-entity LENS over the free monoid of bytes — REALIZED

- **STRUCTURE:** very well-behaved lens (Foster, Greenwald, Moore, Pierce,
  Schmitt 2007, *Combinators for bidirectional tree transformations*, TOPLAS
  29(3), §3) per span-bearing entity: `get(F) = F[s..t]`,
  `put(F, c') = F[..s] ++ c' ++ F[t..]`; plus the **frame rule** (Reynolds
  2002, *Separation logic*, LICS; O'Hearn's frame rule) over disjoint entity
  footprints.
- **DENOTATION:** `⟦extract⟧ : Σ* → List(Span × Σ*)` with
  `π₂ = slice ∘ π₁` — compositional, total on the parser's domain.
- **CARRIER:** `SemanticEntity { start_byte, end_byte, content, id, … }`.
- **LAWS:**
  - GetPut: `splice(F, span(e), content(e)) = F` — **HOLDS** (E1/E2 witness).
  - PutGet: re-extraction after a well-formed splice sees exactly the new
    content, with stable identity — **HOLDS** (E3).
  - Frame: all disjoint entities keep id/type/name/content/lines — **HOLDS**
    (E3), for same-line-count well-formed replacements in TS/Python/Rust.
- **WITNESS:** `tests/laws_extraction.rs::law_e1_e2_*`, `law_e3_putget_and_frame` — GREEN.
- **FREE ON NAMING:** entity-level *editing* (splice by span) is sound; any
  consumer may implement byte-precise entity rewriting against this contract.
- **Boundary note:** the lens laws hold for span-bearing entities. Nested
  JSON entities below one level carry no spans (`start_byte = None`) — the
  lens family is *partial*, and honestly so (the `Option` is the domain
  restriction, not a lie).

### F2 — The diff is a PARTIAL BIJECTION: an element of the symmetric inverse monoid — 80%-REALIZED

- **STRUCTURE:** `match_entities(before, after)` computes a partial
  injective map `m ⊆ before × after` — an element of the **symmetric inverse
  monoid** `I(Entities)` (Wagner 1952, Preston 1954; Lawson 1998, *Inverse
  Semigroups*, §1). Change classification is a labeling of `m` (Modified /
  Renamed / Moved / Reordered) plus the complement of `dom(m)` (Deleted) and
  `cod(m)` (Added).
- **DENOTATION:** `⟦diff⟧(F,F') = (m, labels)`; the laws below are exactly
  the inverse-monoid laws restricted to what the boundary exposes.
- **LAWS:**
  - Identity: `diff(F,F) = ∅` — **HOLDS** (D1, incl. JSON and fallback text).
  - Duality: `diff(F',F)` is the inverse partial bijection (Added↔Deleted
    swap, Modified self-dual, |changes| preserved) — **HOLDS** (D6) on
    add/remove/modify edit classes.
  - Locality/monoid homomorphism: diff over a file coproduct = per-file sum
    (change multiset concat, counters additive) — **HOLDS** (D3).
  - **Injectivity/functionality of the matching is realized; STABILITY and
    OPTIMALITY are NOT** — see F5 (what it is not).
- **WITNESS:** `tests/laws_diff.rs::law_d1_*`, `law_d3_*`, `law_d6_*` — GREEN.
- **Non-composability note:** there is no patch composition
  (`diff(F,F'') ≠ diff(F,F') ⋄ diff(F',F'')` is not even typed here), so this
  is *not* a groupoid or a category of patches (contrast Darcs patch theory /
  Mimram–Di Giusto 2013). What it is not, precisely: as a "diff groupoid",
  only the inverse-monoid fragment (identity + inverse) is present, and only
  those laws are claimed.

### F3 — Reorder detection is Ulam distance via longest non-decreasing subsequence — REALIZED

- **STRUCTURE:** minimal move-set = complement of an LNDS (Schensted 1961,
  *Longest increasing and decreasing subsequences*; Ulam distance; the same
  insight as "patience diff", B. Cohen 2005).
- **LAW:** a pure permutation of unchanged entities yields Reordered changes
  only — never Added+Deleted, never Modified — with
  `1 ≤ |Reordered| ≤ n−1` — **HOLDS** (D4).
- **WITNESS:** `law_d4_permutation_is_reordered_only`,
  `law_d4_identity_permutation_is_empty` — GREEN.
- This answers law 5's positive half: *moving an entity within a
  file yields [moved/reordered], not [deleted]+[added]*, unconditionally for
  content-identical entities (phase 1 exact-ID match makes it independent of
  any similarity threshold). The heuristic boundary only matters when content
  *and* identity change simultaneously — see F5.

### F4 — structural_hash is a quotient map for the formatting congruence — REALIZED

- **STRUCTURE:** a congruence `≈fmt` on entity bodies (generated by
  comments/whitespace); `structural_hash` factors content through
  `Σ*/≈fmt` (a canonical-form homomorphism; cf. Unison's content-addressed
  identity, and term-rewriting canonicalization, Baader–Nipkow 1998).
- **LAW:** comment-only body edit ⇒ Modified with
  `structural_change = Some(false)`; literal edit ⇒ `Some(true)` — **HOLDS**
  (D5, all three code languages).
- **WITNESS:** `law_d5_formatting_quotient` — GREEN.
- Related declared kernel: blank-line-only edits *between* entities are
  invisible (`orphan_content` drops blank segments) — the diff's domain is
  files modulo that congruence. Pinned by
  `characterize_blank_line_insertion_outside_entities_is_invisible`.

### F5 — The matcher is a GREEDY LAYERED HEURISTIC, not a stable matching and not an optimal assignment — what it is not (named precisely)

- **What it actually is:** five sequential match layers of decreasing
  evidence: (1) exact ID; (2) exact `content_hash`, then `structural_hash`;
  (3) same-file signature `(file,type,name,parent-chain)` with Jaccard ≥ 0.3;
  (4) cross-file signature `(type,name,parent-chain)` with **no minimum
  similarity** (`best_score` starts at −∞ — any candidate wins); (5) fuzzy
  Jaccard ≥ 0.8 grouped by `entity_type` **only** — no name, no parent, no
  locality prior — greedy first-come per after-entity.
- **What it is not:** this is not Gale–Shapley stable matching (1962) — blocking
  pairs exist because earlier after-entities consume before-entities
  greedily; not Kuhn's Hungarian assignment (1955) — no global objective.
  Naming it "matching" without qualification promises exchange-stability
  laws that do not hold. It *is* a maximal greedy matching heuristic with a
  layered evidence order — a reasonable engineering choice, but its laws are
  weaker and the witnesses only claim the weaker ones.
- **The practitioner-reported artifact, localized:** phase 5's missing
  locality prior. Deleting a field from container B while adding a textually
  similar (≥0.8) same-type field to unrelated container A pairs them into a
  single **Moved**. Pinned GREEN as
  `characterize_cross_container_identical_text_is_moved` (so any future
  boundary shift is conscious), and its blast radius is **load-bearing, not
  cosmetic** — see F8.
- Phase 4's thresholdlessness pinned by
  `characterize_cross_file_rename_zero_similarity_is_moved`: file rename +
  total rewrite of a same-named function is still Moved. Content is carried
  on the change, so nothing is lost; the *classification* asserts identity
  continuity on name alone.

### F6 — RED: diff soundness fails at line/byte granularity mismatch — BROKEN

- **LAW (law 4):** the entity cover + orphan cover is jointly epic —
  every byte change between parseable files is attributed somewhere.
- **VIOLATION:** `detect_orphan_changes` computes coverage as *line sets*
  (`start_line..=end_line`) while entity content is *byte*-granular. Bytes
  on a covered line but outside every entity span (trailing same-line
  comment after a closing brace) are in no entity and no orphan segment.
  `fn foo() { let x = 1; } // old` → `// new` produces an **empty diff**.
- **WITNESS (RED, `#[ignore]`):**
  `red_law_soundness_same_line_residue_is_attributed` — fails on HEAD.
- **Structure that fixes it:** make the orphan cover the byte-complement of
  the entity spans (the exact set-theoretic complement in the free monoid
  factorization of F1) instead of a line approximation. Then joint epicness
  is by construction, not by luck.

### F7 — RED: diff output order is not a function of its input — BROKEN

- **LAW (law 2, diff side):** `compute_semantic_diff` is
  deterministic including change order, across calls and processes.
- **VIOLATION:** `match_entities` phase 1 iterates `after_by_id`
  (`std::HashMap`, per-instance `RandomState`) and pushes changes in that
  order; the final `sort_by_key(entity_line)` (differ.rs:172) is not a total
  order, so same-line ties keep the random pre-sort order. Observed: 6
  distinct output orders in 30 identical in-process runs.
- **WITNESS (RED, `#[ignore]`):** `red_law_diff_output_order_is_deterministic`.
- Same mechanism family as the crate's own disclosed `metadata_json`
  HashMap finding (`model/entity.rs` test
  `metadata_json_is_independent_of_map_instance`). Counts and change *sets*
  are stable; only order flaps — but order is what golden files, CI gates
  and serialized-result caches see.
- **Structure that fixes it:** restore functionality of `⟦diff⟧` by
  totalizing the order — iterate the `after` slice in phase 1 and/or sort by
  `(entity_line, entity_id)`.
- **Extraction side of law 2 is GREEN:** extraction is deterministic
  (E4witness), hashes are keyless xxh3, `metadata` is already `BTreeMap`.

### F8 — RED: unconditional Moved-parent suppression composes with F5 into a soundness hole — BROKEN

- **LAW:** suppression may only drop a container change fully explained by
  reported child changes; a container whose *own declaration* changed must
  stay visible.
- **VIOLATION:** `suppress_redundant_parents` (differ.rs:352–360) suppresses
  a Moved child's old parent **unconditionally** (the Modified-suppression
  path checks the stripped own-declaration; this path does not). Composed
  with F5's false cross-container Move: Beta loses `extends Base` *and*
  field `id`; Alpha gains an unrelated identical-text `id`. HEAD reports
  `[Alpha Modified, Alpha::id Moved-from-Beta]` — the removal of Beta's
  inheritance clause appears **nowhere**.
- **WITNESS (RED, `#[ignore]`):**
  `red_law_false_move_must_not_suppress_parent_declaration_change`.
- This upgrades the F5 artifact from "cosmetic classification quirk" to
  **load-bearing**: a semantic contract change (inheritance removed) is
  dropped from the change record.

### F9 — Partiality honesty: the fallback chunker is a declared total factorization — REALIZED

- **STRUCTURE:** for inputs outside the parser's domain, extraction degrades
  to the total factorization of the file in the free monoid of lines:
  20-line chunks, labeled `chunk`, spans partitioning `1..=n` in order,
  contents reassembling the input. Kleene-style honest partiality: the
  fallback is *declared* (entity_type "chunk"), never a misparse presented
  as semantic entities.
- **WITNESS:** `law_e6_fallback_total_line_cover`,
  `law_d1_diff_identity_fallback` — GREEN (law 6).
- **Caveat (documented, not witnessed as RED):** a *parseable-but-broken*
  file for a supported language does not fall back — tree-sitter
  error-recovers and extraction returns whatever survives; and a panic in
  extraction/disambiguation is caught (`catch_unwind` in differ.rs:111/124)
  and collapses to **zero entities**, silently degrading the whole file to
  orphan-only diffing. Changes remain attributed (soundness preserved via
  orphans) but granularity is silently lost. This is the one place where
  partiality is handled *silently* rather than honestly; a
  `degraded: bool`/diagnostic on `DiffResult` would make it declared.

### F10 — Entity IDs are paths in the containment forest; disambiguation restores faithfulness — REALIZED

- **STRUCTURE:** `build_entity_id` composes names along the parent chain
  ("::") — morphism composition in the free category on the containment
  forest. Name collisions make the naming functor non-faithful;
  `disambiguate_colliding_entity_ids` restores injectivity with `@L`/`#`/`@D`
  tags (a fixed-point iteration bounded by entity count, propagating
  through child IDs).
- **LAW:** IDs pairwise distinct after extraction, all collision copies
  retained — **HOLDS** (E5, forced Python redefinitions).
- **WITNESS:** `law_e5_entity_ids_unique_under_collisions` — GREEN.

### F11 — "Extraction as a Galois connection" — what it is not

This audit hypothesized extraction as a Galois connection between text and
entity space. The adjunction is not there: there is no lattice/order on
files or entity sets at the boundary for `α ⊣ γ` to live in, and no
concretization is exposed. What *is* there is stronger where it matters and
honest where it is weak: a lens family (F1) with a free-monoid factorization
(F9). Claiming "Galois connection" would promise `α∘γ∘α = α`-style laws no
test can currently state over the public surface. Verdict: not that structure;
the abstract-interpretation reading adds nothing the lens does not already
give, and would mis-promise.

---

## 2. Confirmations (already lawfully realized — no action)

- Lens laws per span-bearing entity (F1): GetPut, PutGet, frame — witnessed.
- Diff identity, duality, per-file monoidal locality (F2) — witnessed.
- Reorder = LNDS complement / Ulam distance (F3) — witnessed.
- structural_hash formatting quotient (F4) — witnessed.
- ID faithfulness under collisions (F10) — witnessed.
- Fallback totality + declaredness (F9) — witnessed.
- Extraction determinism in-process, incl. serialization image (E4);
  cross-process stability rests on keyless xxh3 + BTreeMap metadata (both
  verified choices in source, the latter with its own in-crate witness).

This is half the audit's value: the design's central claims are real and
now have permanent, refactor-proof property witnesses.

## 3. Simplifications this licenses (ranked; hand to the fix owner, guarded by the green laws)

1. **Byte-complement orphan cover** (fixes F6, simplifies): replace the
   line-set arithmetic in `detect_orphan_changes` with the byte-interval
   complement of entity spans (the data is already there: `start_byte`/
   `end_byte`, filled for non-code plugins at the registry boundary). Joint
   epicness becomes structural; the line/byte impedance code (covered-line
   sets, segment reconstruction) collapses. Keep green: D1, D2, D3;
   `red_law_soundness_same_line_residue_is_attributed` must flip GREEN.
2. **Totalize the change order** (fixes F7, one-line class of change):
   iterate `after` (slice) not `after_by_id` (HashMap) in phase 1, and/or
   sort by `(entity_line, entity_id)`. Keep green: everything;
   `red_law_diff_output_order_is_deterministic` must flip GREEN.
3. **Condition the Moved-parent suppression** (fixes F8): apply the same
   stripped-own-declaration test the Modified-suppression already uses.
   Keep green: `test_parent_suppressed_when_only_child_modified` and kin;
   `red_law_false_move_must_not_suppress_parent_declaration_change` must
   flip GREEN.
4. **(Optional, larger) Locality prior for phase 5**: add a parent-distance
   term or restrict phase 5 to matching parents unless similarity is 1.0
   and unique. This moves the heuristic boundary; the two `characterize_*`
   tests exist precisely so this is done consciously.
5. **(Optional) Declared degradation**: surface the `catch_unwind → empty
   entities` path (F9 caveat) as a flag/diagnostic instead of silence.

## 4. Guarantee→structure loop

| Observed/likely error class | Missing guarantee | Structure that confers it | Collapses? |
|---|---|---|---|
| Same-line trailing edits invisible (F6) | jointly-epic cover | byte-interval complement (Boolean algebra of intervals) | yes — deletes line-set arithmetic |
| Golden-file flake on same-line ties (F7) | totality of output order | total order key; function-ness of `⟦diff⟧` | yes — one sort key |
| Vanishing container declaration change (F8) | suppression ⊑ explanation | conditional suppression (same congruence test as Modified path) | partial |
| Cross-container false Move (F5) | matching locality/stability | stable matching with locality-weighted preferences (Gale–Shapley) or cost assignment (Kuhn) | no — larger algorithm change; heuristic may be the right trade |

## 5. Property inventory (law → arbitrary — the standing menu)

All arbitraries in `tests/laws_common/mod.rs`: `Program{lang ∈ {TS,PY,RS,JSON},
rets, preamble, with_class}` rendered to source; edits are abstract-value
edits (bump literal i, touch preamble, permute order, add/remove fn, comment
body); plain-line vectors for fallback. Every property carries a
non-vacuity floor and every checker a positive control
(`control_*` tests) proving it can go RED.

| Law | Property | Arbitrary | Status |
|---|---|---|---|
| E1/E2 GetPut + slice | `law_e1_e2_lens_get_slice_agreement_and_getput` | Program(all langs, 1–6 fns) | GREEN |
| E3 PutGet + frame | `law_e3_putget_and_frame` | Program(code, 2–6) × target idx | GREEN |
| E4 extraction determinism | `law_e4_extraction_deterministic` | Program(all langs) | GREEN |
| E5 ID faithfulness | `law_e5_entity_ids_unique_under_collisions` | Python + forced duplicate defs | GREEN |
| E6 fallback totality | `law_e6_fallback_total_line_cover` | 1–60 plain lines | GREEN |
| D1 identity | `law_d1_diff_identity`(+`_fallback`) | Program / plain lines | GREEN |
| D2 soundness/attribution | `law_d2_soundness_code_edits`, `_json_edits` | Program × Edit{Body,Preamble}/Value | GREEN |
| D3 locality monoid | `law_d3_per_file_locality` | Program × Program (2 files) | GREEN |
| D4 permutation/Ulam | `law_d4_permutation_is_reordered_only` | Program × non-identity shuffle | GREEN |
| D5 formatting quotient | `law_d5_formatting_quotient` | Program × {Comment,Literal} | GREEN |
| D6 duality | `law_d6_duality` | Program × {Add,Remove,Body} | GREEN |
| Soundness (byte residue) | `red_law_soundness_same_line_residue_is_attributed` | fixed witness | **RED**, `#[ignore]` |
| Diff order determinism | `red_law_diff_output_order_is_deterministic` | fixed witness, 30 runs | **RED**, `#[ignore]` |
| Suppression ⊑ explanation | `red_law_false_move_must_not_suppress_parent_declaration_change` | fixed witness | **RED**, `#[ignore]` |
| Heuristic boundary pins | `characterize_cross_container_identical_text_is_moved`, `characterize_cross_file_rename_zero_similarity_is_moved`, `characterize_blank_line_insertion_outside_entities_is_invisible` | fixed | GREEN (pins) |

These properties retire whole families of scenario tests: any hand-written
"modify one function and check the diff" scenario is an instance of D2; any
"reorder functions" scenario an instance of D4.

## 6. Bibliography

- Foster, Greenwald, Moore, Pierce, Schmitt (2007). *Combinators for
  bidirectional tree transformations*. TOPLAS 29(3). — lenses, GetPut/PutGet.
- Reynolds (2002). *Separation logic*. LICS. O'Hearn (2019, CACM). — frame rule.
- Wagner (1952); Preston (1954); Lawson (1998), *Inverse Semigroups*. —
  symmetric inverse monoid / partial bijections.
- Schensted (1961). *Longest increasing and decreasing subsequences*.
  Canad. J. Math. — LIS/LNDS; Ulam distance. Cohen (2005), patience diff.
- Wagner–Fischer (1974); Myers (1986), *An O(ND) difference algorithm*. —
  LCS DP used for orphan-segment anchoring.
- Gale, Shapley (1962). *College admissions and the stability of marriage*.
  AMM 69. — what the matcher is NOT.
- Kuhn (1955). *The Hungarian method for the assignment problem*. — ditto.
- Hoare (1972). *Proof of correctness of data representations*. Acta Inf. —
  test images at the boundary, not representations.
- Scott, Strachey (1971). *Toward a mathematical semantics for computer
  languages*. — the `⟦·⟧` recognition primitive.
- Baader, Nipkow (1998). *Term Rewriting and All That*. — canonical forms /
  quotient hashing.
- Mimram, Di Giusto (2013). *A categorical theory of patches*. — the patch
  category diff is NOT.
