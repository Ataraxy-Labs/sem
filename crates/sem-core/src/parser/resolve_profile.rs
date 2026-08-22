//! Opt-in, behavior-neutral instrumentation for pass-2 (resolution) of
//! `EntityGraph::build`, gated by the `SEM_PROFILE_RESOLVE=1` environment
//! variable.
//!
//! Every public function here is a cheap no-op (a single `enabled()` check,
//! itself a cached env-var read) unless the variable is set, and nothing in
//! this module changes resolution output — it only observes timings and
//! candidate-list sizes at points the resolver already visits. Built to
//! settle step 1: see `crates/sem-core/.
//!
//! Design notes:
//! - Phase-level timers (reparse, pass-1 scan, scope build, ref collection,
//!   the ref-resolution loop, `resolve_ref` itself) are plain `AtomicU64`
//!   nanosecond accumulators, summed across every `resolve_with_scopes_full`
//!   call (one per 5,000-file chunk on repos over `PARSED_FILE_REUSE_LIMIT`,
//!   one call otherwise) so the numbers cover a whole build.
//! - Per-name candidate-lookup stats (`class_members`/`symbol_table` bucket
//!   sizes actually hit during resolution, and how long disambiguation over
//!   them took) are accumulated *locally per file* with zero locking on the
//!   hot path, then merged into a global map with one short lock per file —
//!   not per reference — to keep contention off the timed critical section.

use std::collections::HashMap as StdHashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;

/// Number of log2-ish candidate-count buckets. Bucket 0 = {0}, bucket b>=1 =
/// [2^(b-1), 2^b - 1], clamped at the top bucket. 24 buckets covers up to
/// ~8.3M candidates, far above anything a repo's symbol table produces.
const NBUCKETS: usize = 24;

/// Phase-level timing, i.e. everything in this module except the per-name
/// candidate samples. On at `SEM_PROFILE_RESOLVE=1` and at `=2`.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("SEM_PROFILE_RESOLVE").as_deref(),
            Ok("1") | Ok("2")
        )
    })
}

/// Per-name candidate sampling ([`FileAccum`]/[`BowFileAccum`]): on **only**
/// at `SEM_PROFILE_RESOLVE=1`.
///
/// Profiling measured what this costs and disclosed it
/// rather than absorbing it: profiled `full_graph_build` ran 1.45-1.65x the
/// clean wall time on dotnet/llvm/linux, all of it the per-lookup
/// `name.to_string()` and per-file map merge these accumulators do, and all
/// of it landing *inside* resolve — which is exactly the phase the phase
/// timers are trying to attribute. The conclusion — that its sub-phase
/// numbers were "attribution, not wall time" — is the direct consequence.
///
/// `=2` is that conclusion turned into a mode: phase timers on, name sampling
/// off, so shares can be read off a run whose wall time is the real one. The
/// two accumulators are constructed behind this gate (`scope_resolve.rs`,
/// `graph.rs`), so at `=2` the `Option` is `None`, `select_member_profiled!`
/// takes its untimed branch, and not one `to_string` happens.
pub fn names_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SEM_PROFILE_RESOLVE").as_deref() == Ok("1"))
}

fn bucket_index(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let b = (usize::BITS - n.leading_zeros()) as usize;
    b.min(NBUCKETS - 1)
}

fn bucket_range_label(b: usize) -> String {
    if b == 0 {
        "0".to_string()
    } else if b == NBUCKETS - 1 {
        format!(">={}", 1usize << (b - 1))
    } else {
        let lo = 1usize << (b - 1);
        let hi = (1usize << b) - 1;
        format!("{lo}-{hi}")
    }
}

// ---- phase-level wall-time accumulators (nanoseconds) ----
static REPARSE_NS: AtomicU64 = AtomicU64::new(0);
static PASS1_SCAN_NS: AtomicU64 = AtomicU64::new(0);
static CTOR_INFER_NS: AtomicU64 = AtomicU64::new(0);
static IMPORT_GROUP_NS: AtomicU64 = AtomicU64::new(0);
static PASS2_WALL_NS: AtomicU64 = AtomicU64::new(0);
static SCOPE_BUILD_NS: AtomicU64 = AtomicU64::new(0);

// ---- scope_build decomposition ----
//
// `scope_build_ns` was the last unmeasured box inside resolve: 6.6 s on
// dotnet, and on home-assistant — the one giant whose *parse* fits the 1 s
// budget at 0.72x — the 4.3 s of resolve it dominates is the whole reason HA
// misses the target. Every unmeasured box this campaign has opened has hidden
// something, so this opens it.
//
// These are the constituents of the region `__scope_build_t0` already spans in
// `scope_resolve.rs`, each timed where it happens and summed across files.
// They are recorded per-file into a plain [`ScopeBuildAccum`] (no allocation,
// no locking) and merged once per file, exactly like the phase timers above,
// so they are subject to the same zero-cost-when-off contract.
/// `entities_by_file` slice -> owned `Vec` + `FileEntityLookup::new`.
static SB_ENTITY_LOOKUP_NS: AtomicU64 = AtomicU64::new(0);
/// `find_entity_source_spans`: locating each entity's byte span in the file.
static SB_ENTITY_SPANS_NS: AtomicU64 = AtomicU64::new(0);
/// The JS/TS precomputed path: cloning `scopes`, `entity_scope_map`,
/// `entity_inner_scope` and `ast_refs` out of `PrecomputedFileFacts`. Pure
/// copying — no analysis — and the corpus/facts tier is what puts it here.
static SB_PRECOMPUTED_CLONE_NS: AtomicU64 = AtomicU64::new(0);
/// The non-precomputed path: `build_scopes_from_ast`, the actual scope tree.
static SB_BUILD_SCOPES_AST_NS: AtomicU64 = AtomicU64::new(0);
/// The fused triple walk (`fused_scope_refs_import_walk`): scopes +
/// refs + recorded import starts in one traversal. Populated instead of
/// `build_scopes_ast`/`collect_refs` for files on the fused path; the pruned
/// replay of the import handlers still lands in `extract_imports`.
static SB_FUSED_WALK_NS: AtomicU64 = AtomicU64::new(0);
/// The non-precomputed path: `collect_all_file_refs`.
static SB_COLLECT_REFS_NS: AtomicU64 = AtomicU64::new(0);
/// `extract_imports_from_ast` (skipped for JS/TS precomputed files).
static SB_EXTRACT_IMPORTS_NS: AtomicU64 = AtomicU64::new(0);
/// Seeding scope 0 from the pre-built import table, plus the by-name re-key.
static SB_IMPORT_REKEY_NS: AtomicU64 = AtomicU64::new(0);
/// `inject_return_type_bindings`.
static SB_INJECT_RETURN_TYPES_NS: AtomicU64 = AtomicU64::new(0);
/// `inject_field_type_bindings`.
static SB_INJECT_FIELD_TYPES_NS: AtomicU64 = AtomicU64::new(0);
/// Files that took the JS/TS precomputed path.
static SB_FILES_PRECOMPUTED: AtomicU64 = AtomicU64::new(0);
/// Files that took the AST path (everything else).
static SB_FILES_AST: AtomicU64 = AtomicU64::new(0);
/// Of the AST-path files, those that took the fused triple walk.
static SB_FILES_FUSED: AtomicU64 = AtomicU64::new(0);
/// Entities `find_entity_source_spans` was asked to place, summed — the
/// demand term the span cost should be proportional to.
static SB_ENTITIES_SPANNED: AtomicU64 = AtomicU64::new(0);
/// Scopes present after the scope tree is built, summed.
static SB_SCOPES_BUILT: AtomicU64 = AtomicU64::new(0);
/// AST refs collected/cloned, summed.
static SB_REFS_COLLECTED: AtomicU64 = AtomicU64::new(0);
/// MUL: files whose precomputed facts carried
/// nonempty `import_stmts`, dispatched without a tree.
static SB_PRECOMPUTED_IMPORT_FILES: AtomicU64 = AtomicU64::new(0);
/// MUL: total `import_stmts` descriptors dispatched
/// from precomputed facts, summed across those files.
static SB_PRECOMPUTED_IMPORT_DESCRIPTORS: AtomicU64 = AtomicU64::new(0);
/// MUL Phase 2 (MUL;): files whose
/// precomputed facts carried nonempty `ctor_call_sites`, applied without a
/// tree (`infer_constructor_param_types`, not `ScopeBuildAccum` — this scan
/// runs once per build in its own pass-1b step, not per file inside pass 2's
/// closure, so it is bumped directly rather than through
/// [`merge_scope_build`]).
static SB_PRECOMPUTED_CTOR_CALL_FILES: AtomicU64 = AtomicU64::new(0);
/// MUL: total `ctor_call_sites` descriptors applied
/// from precomputed facts, summed across those files.
static SB_PRECOMPUTED_CTOR_CALL_DESCRIPTORS: AtomicU64 = AtomicU64::new(0);

/// MUL Phase 2 (MUL;): record that one
/// file's precomputed `ctor_call_sites` were applied without a tree.
/// Engagement proof, not inference — called only when
/// `infer_constructor_param_types` actually sourced a file's descriptors
/// from `PrecomputedFileFacts` and that `Vec` was nonempty, mirroring
/// `ScopeBuildAccum::precomputed_import_descriptors`'s discipline for Field
/// 10.
pub fn add_precomputed_ctor_call_engagement(files: u64, descriptors: u64) {
    if !enabled() {
        return;
    }
    SB_PRECOMPUTED_CTOR_CALL_FILES.fetch_add(files, Ordering::Relaxed);
    SB_PRECOMPUTED_CTOR_CALL_DESCRIPTORS.fetch_add(descriptors, Ordering::Relaxed);
}

// ---- pass-2 diagnostic counters (entity scope-index lookups + type-directed
// method resolution). Same zero-cost-when-off contract as everything above;
// like the phase timers they are on at `SEM_PROFILE_RESOLVE=1` *and* `=2`,
// and their call sites decide any extra gating themselves.
//
// The scope-index pair exists because the JS/TS precomputed path clones its
// scope maps from `PrecomputedFileFacts`, and nothing observed whether the
// clone actually contains every entity the ref loop then asks about: an
// entity missing from both maps silently resolves against scope 0 (module),
// which is either correct (a genuine module-level entity) or a silent
// misattribution. Counted only for precomputed files so the denominator stays
// the population the question is about.

/// Entity scope-index lookups in pass 2's per-entity loop, summed across files
/// whose scopes were cloned from `PrecomputedFileFacts`. Every entity in such
/// a file bumps this once, hit or miss.
static ENTITY_SCOPE_LOOKUPS: AtomicU64 = AtomicU64::new(0);
/// Of [`ENTITY_SCOPE_LOOKUPS`], how many missed **both**
/// `entity_inner_scope` and `entity_scope_map` and took the `unwrap_or(0)`
/// default. A hit on scope 0 itself does not count — only the miss path.
static ENTITY_SCOPE_LOOKUP_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Record one entity scope-index lookup from a precomputed-facts file.
/// `fell_back_to_scope_zero` is `true` exactly when both map lookups missed
/// and resolution defaulted the entity into scope 0.
pub fn add_entity_scope_lookup(fell_back_to_scope_zero: bool) {
    if !enabled() {
        return;
    }
    ENTITY_SCOPE_LOOKUPS.fetch_add(1, Ordering::Relaxed);
    if fell_back_to_scope_zero {
        ENTITY_SCOPE_LOOKUP_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    }
}

// ---- resolve_ref's type-directed MethodCall path (`receiver.method()` ->
// look up the receiver's type via `lookup_type_before_class_scope`, then match
// `method` in that type's `class_members`). Attempt/success split answers "how
// much of type tracking is even attempted, and how often does it land" —
// neither of which the wall-time buckets can express.

/// MethodCall references where the receiver was a plain identifier handed to
/// `lookup_type_before_class_scope` (the type-directed attempt), summed.
static RESOLVE_REF_TYPE_DIRECTED_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
/// Of [`RESOLVE_REF_TYPE_DIRECTED_ATTEMPTED`], how many resolved the receiver
/// to a type *and* matched the method in that type's `class_members` (the
/// `"type_tracking"` edge).
static RESOLVE_REF_TYPE_DIRECTED_SUCCEEDED: AtomicU64 = AtomicU64::new(0);

/// Record one type-directed MethodCall attempt: the receiver was a plain
/// identifier candidate handed to `lookup_type_before_class_scope`. Exactly
/// one call per attempt, at the attempt site; the success counter fires
/// separately at the (early-returning) match site.
pub fn add_resolve_ref_type_directed_attempt() {
    if !enabled() {
        return;
    }
    RESOLVE_REF_TYPE_DIRECTED_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
}

/// Record that a type-directed MethodCall attempt (one already counted by
/// [`add_resolve_ref_type_directed_attempt`]) produced the `"type_tracking"`
/// edge: receiver resolved to a type and `class_members` matched the method.
pub fn add_resolve_ref_type_directed_success() {
    if !enabled() {
        return;
    }
    RESOLVE_REF_TYPE_DIRECTED_SUCCEEDED.fetch_add(1, Ordering::Relaxed);
}

/// Per-file scope_build sub-phase accumulator. Plain integers mutated inside
/// one rayon closure with no locking or allocation, merged once per file by
/// [`merge_scope_build`] — the same discipline [`FileAccum`] uses, minus the
/// maps that made that one expensive (which is why this one is on at `=2`).
#[derive(Default, Clone, Copy)]
pub struct ScopeBuildAccum {
    pub entity_lookup_ns: u64,
    pub entity_spans_ns: u64,
    pub precomputed_clone_ns: u64,
    pub build_scopes_ast_ns: u64,
    pub collect_refs_ns: u64,
    pub fused_walk_ns: u64,
    pub extract_imports_ns: u64,
    pub import_rekey_ns: u64,
    pub inject_return_types_ns: u64,
    pub inject_field_types_ns: u64,
    pub precomputed_path: bool,
    pub fused_path: bool,
    pub entities_spanned: u64,
    pub scopes_built: u64,
    pub refs_collected: u64,
    /// MUL: number of `PrecomputedFileFacts::import_stmts`
    /// descriptors dispatched for this file without a tree — nonzero only
    /// when `precomputed` is `Some` *and* those descriptors were nonempty,
    /// i.e. proof (not assumption) that a phase-2-admitted language's
    /// import-bearing file actually took the descriptor fast path instead of
    /// falling back to `record_import_stmts_pruned` against a re-parsed tree.
    pub precomputed_import_descriptors: u64,
}

/// Merge one file's scope_build decomposition into the global counters.
pub fn merge_scope_build(a: ScopeBuildAccum) {
    if !enabled() {
        return;
    }
    SB_ENTITY_LOOKUP_NS.fetch_add(a.entity_lookup_ns, Ordering::Relaxed);
    SB_ENTITY_SPANS_NS.fetch_add(a.entity_spans_ns, Ordering::Relaxed);
    SB_PRECOMPUTED_CLONE_NS.fetch_add(a.precomputed_clone_ns, Ordering::Relaxed);
    SB_BUILD_SCOPES_AST_NS.fetch_add(a.build_scopes_ast_ns, Ordering::Relaxed);
    SB_COLLECT_REFS_NS.fetch_add(a.collect_refs_ns, Ordering::Relaxed);
    SB_FUSED_WALK_NS.fetch_add(a.fused_walk_ns, Ordering::Relaxed);
    SB_EXTRACT_IMPORTS_NS.fetch_add(a.extract_imports_ns, Ordering::Relaxed);
    SB_IMPORT_REKEY_NS.fetch_add(a.import_rekey_ns, Ordering::Relaxed);
    SB_INJECT_RETURN_TYPES_NS.fetch_add(a.inject_return_types_ns, Ordering::Relaxed);
    SB_INJECT_FIELD_TYPES_NS.fetch_add(a.inject_field_types_ns, Ordering::Relaxed);
    if a.precomputed_path {
        SB_FILES_PRECOMPUTED.fetch_add(1, Ordering::Relaxed);
    } else {
        SB_FILES_AST.fetch_add(1, Ordering::Relaxed);
        if a.fused_path {
            SB_FILES_FUSED.fetch_add(1, Ordering::Relaxed);
        }
    }
    SB_ENTITIES_SPANNED.fetch_add(a.entities_spanned, Ordering::Relaxed);
    SB_SCOPES_BUILT.fetch_add(a.scopes_built, Ordering::Relaxed);
    SB_REFS_COLLECTED.fetch_add(a.refs_collected, Ordering::Relaxed);
    if a.precomputed_import_descriptors > 0 {
        SB_PRECOMPUTED_IMPORT_FILES.fetch_add(1, Ordering::Relaxed);
        SB_PRECOMPUTED_IMPORT_DESCRIPTORS
            .fetch_add(a.precomputed_import_descriptors, Ordering::Relaxed);
    }
}
static REF_COLLECT_NS: AtomicU64 = AtomicU64::new(0);
static REF_LOOP_NS: AtomicU64 = AtomicU64::new(0);
static RESOLVE_REF_NS: AtomicU64 = AtomicU64::new(0);
/// Per-chunk (wall, summed across chunks): building `entities_by_file` +
/// `children_by_parent` by scanning **all** `all_entities` (the whole-corpus
/// entity list, not just this chunk's files) at the top of
/// `resolve_with_scopes_full_inner` — runs once per chunk, over the same
/// full entity list every time, on repos over `PARSED_FILE_REUSE_LIMIT`.
static CHUNK_ENTITY_INDEX_NS: AtomicU64 = AtomicU64::new(0);
/// Per-chunk (wall, summed across chunks): `deterministic_return_types_by_name`
/// — iterates the **whole-corpus** `symbol_table` (not just this chunk's
/// names) once per chunk to build a by-name return-type map.
static RETURN_TYPES_BY_NAME_NS: AtomicU64 = AtomicU64::new(0);
/// Per-chunk (wall, summed across chunks): merging each file's edges/log/
/// consumed-words out of `per_file_results` into the chunk-level accumulators,
/// right after the parallel section `pass2_wall_ns` covers.
static SCOPE_MERGE_NS: AtomicU64 = AtomicU64::new(0);
/// Per-chunk (wall, summed across chunks): `resolve_with_scopes_full_inner`'s
/// own index-based sort+dedup of `all_edges` (distinct from `graph.rs`'s
/// later `dedupe_resolved_edges`/`sort_resolved_refs`, which run once, after
/// scope edges are merged with bag-of-words + export-alias edges).
static SCOPE_DEDUP_NS: AtomicU64 = AtomicU64::new(0);
static CACHE_HIT: AtomicU64 = AtomicU64::new(0);
static CACHE_MISS: AtomicU64 = AtomicU64::new(0);
static FILES_PROCESSED: AtomicU64 = AtomicU64::new(0);

// ---- residual sub-phase accumulators: everything in `EntityGraph::build`
// that runs inside `resolve_phase_ms` (post `BuildPhase::Resolving`) or inside
// `import_table_derived_ms` (pre-hook) but outside scope_resolve.rs's own buckets
// above. Same zero-cost-when-off contract as everything else in this module.

/// `build_import_table_with_default_export_paths`: total wall time (pre-hook,
/// part of `import_table_derived_ms`).
static IMPORT_TABLE_WALL_NS: AtomicU64 = AtomicU64::new(0);
/// Sum across files (parallel) of `import_source_content`'s file read — only
/// hit when a file wasn't already covered by pass 1's retained parse trees
/// (i.e. always, on repos over `PARSED_FILE_REUSE_LIMIT`).
static IMPORT_TABLE_IO_NS: AtomicU64 = AtomicU64::new(0);
/// Sum across files (parallel) of `scan_import_file`'s regex/content scanning.
static IMPORT_TABLE_SCAN_NS: AtomicU64 = AtomicU64::new(0);
/// Sequential merge of per-file scans into the final import table (default/
/// namespace/re-export resolution + `HashMap` inserts) — sum of the two
/// sub-buckets below plus negligible glue.
static IMPORT_TABLE_MERGE_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of the merge: building `default_exports`/`named_exports_by_file`
/// from the scans, `resolve_ts_default_re_exports` (re-export chain
/// resolution), and `TsDefaultExportTable` construction.
static IMPORT_TABLE_EXPORT_BUILD_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of the merge: the final `for scan in scans { import_table.insert(..) }`
/// loop — the actual population of the returned import table.
static IMPORT_TABLE_INSERT_NS: AtomicU64 = AtomicU64::new(0);

/// `build_imports_by_file`: grouping the finished import table by file for
/// bag-of-words lookup (sequential, part of `resolve_phase_ms`).
static IMPORTS_BY_FILE_NS: AtomicU64 = AtomicU64::new(0);
/// `build_symbol_table_by_file`: pre-bucketing `symbol_table`'s
/// per-name candidate lists by file, once per build, so bag-of-words'
/// global-ref match becomes O(candidates in this file) instead of
/// O(candidates in the whole corpus). Parallel across names.
static SYMBOL_TABLE_BY_FILE_NS: AtomicU64 = AtomicU64::new(0);

/// `build_incremental_core`'s "Pass A + Pass B" — the single O(all
/// entities) loop building `symbol_table`, `entity_map`, `scope_class_members`,
/// `scope_owner_members`, `scope_entity_ranges`, plus the local `&str`-borrowed
/// maps (`class_members`, `enclosing_class`, `class_child_names`, …) and
/// `go_pkg_index` — every one of them rebuilt whole on *every* warm rebuild
/// before this change, never previously instrumented as its own bucket (it sat
/// inside the unattributed gap between `pass1_scan_ms` and `resolve_phase_ms`
/// in every prior section of this document).
static ENTITY_LOOKUP_BUILD_NS: AtomicU64 = AtomicU64::new(0);
/// `fingerprint_corpus_tables` — the whole-table hash pass over
/// `symbol_table`/`class_members`/`owner_members`/`entity_map`/`go_pkg_index`
/// that runs once per build, before any read set is evaluated, regardless of
/// how many files are RED. Never previously instrumented as its own bucket.
static FINGERPRINT_CORPUS_TABLES_NS: AtomicU64 = AtomicU64::new(0);

// (continuation): sub-buckets of `ENTITY_LOOKUP_BUILD_NS`, added so
// the "borrowed Pass A/B maps" residual the first half of this change named
// (~600ms, flat across 1/50/500 changed files) could be attributed to a
// specific structure instead of a whole phase. They sum to
// `ENTITY_LOOKUP_BUILD_NS` up to the handful of statements between them.
/// The `&str`-borrowed Pass A loop: `parent_child_pairs`, `child_line_ranges`,
/// `class_child_names`, `class_entity_names`, `class_entity_files`,
/// `id_to_name`.
static LOOKUP_PASS_A_NS: AtomicU64 = AtomicU64::new(0);
/// `build_child_ranges_by_parent` — byte-span computation per parent/child
/// pair, by far the most expensive single member of the borrowed half.
static LOOKUP_CHILD_RANGES_NS: AtomicU64 = AtomicU64::new(0);
/// The five owned, `String`-keyed tables: either
/// `maintain_entity_lookups_incremental` (warm) or the whole rebuild (cold).
static LOOKUP_OWNED_NS: AtomicU64 = AtomicU64::new(0);
/// The `&str`-borrowed Pass B loop: `enclosing_class` + bag-of-words
/// `class_members`.
static LOOKUP_PASS_B_NS: AtomicU64 = AtomicU64::new(0);
/// `go_pkg_index` (zero on corpora with no `.go` files).
static LOOKUP_GO_PKG_NS: AtomicU64 = AtomicU64::new(0);
/// `fingerprint_bow_tables` — the whole-table fold over `class_members`,
/// `class_entity_files` and `parent_child_pairs` run immediately before
/// bag-of-words' read sets are evaluated. Sibling of
/// `FINGERPRINT_CORPUS_TABLES_NS`, and previously unattributed.
static FINGERPRINT_BOW_TABLES_NS: AtomicU64 = AtomicU64::new(0);
/// Pass 1's `maybe_par_iter!(file_paths)` collect — the per-file
/// read/parse/extract (or the four-map GREEN-reuse check that skips it).
static PASS1_WALL_NS: AtomicU64 = AtomicU64::new(0);
/// The sequential loop that folds pass 1's per-file products into
/// `all_entities`/`entity_spans`/`fresh_precomputed`, plus the carry
/// bookkeeping (`precomputed`/`content_hashes` retain, `entity_spans` clone)
/// that follows it.
static ASSEMBLE_NS: AtomicU64 = AtomicU64::new(0);
/// MUL Phase 1. The CLEAN gate:
/// `scope_resolve::clean_gate_dirty_files`'s two passes (a free slice over
/// just the candidate files' own entities, then one O(corpus) scan for
/// cross-file parent edges into them — see that function's doc comment for
/// why the second pass must stay corpus-wide), run once per build right
/// after `all_entities` is assembled (a sub-span of `ASSEMBLE_NS`, broken out
/// here because its cost is a first-class question — "how much does the
/// CLEAN-gate check cost" — not just a component of a pre-existing bucket).
static CLEAN_GATE_NS: AtomicU64 = AtomicU64::new(0);
/// Sibling counter to `CLEAN_GATE_NS`: how many files' fresh precomputed
/// facts the CLEAN gate dropped this build (the gate firing). Zero on every real
/// corpus census checked — nonzero only means the gate
/// caught a cross-file parent link (or a fixture built to have one).
static CLEAN_GATE_FILES_DROPPED: AtomicU64 = AtomicU64::new(0);
/// `GraphSession::run`'s pre-build state hand-off: `known`/`eligible` sets and
/// the `all_entities` → `prev_entities` per-file split. Written by
/// `session.rs`, so deliberately *not* cleared by [`reset`] (which runs inside
/// `build_incremental_core`, i.e. after this phase).
static SESSION_PREP_NS: AtomicU64 = AtomicU64::new(0);
/// `GraphSession::run`'s post-build state hand-back: `changed_key_count`,
/// `file_paths.to_vec()`, and moving every carried structure back onto the
/// session. Same non-reset rule as `SESSION_PREP_NS`, and reported by the
/// *next* build's report because it happens after `maybe_print_report`.
static SESSION_POST_NS: AtomicU64 = AtomicU64::new(0);
/// Wall time of the whole scope-resolution stage (`resolve_with_scopes_full` or
/// `resolve_scopes_in_file_chunks`), so the chunk-loop *wrapper* — chunk
/// partitioning, per-chunk entity slicing, edge merge — is attributable
/// separately from `CHUNKS sum_ms`, which only sums the chunks themselves.
static SCOPE_WALL_NS: AtomicU64 = AtomicU64::new(0);
/// Wall time from the end of scope resolution to the end of the build: import
/// index views, bag-of-words, export aliases, dedupe/sort, and the edge index.
static POST_RESOLVE_NS: AtomicU64 = AtomicU64::new(0);
/// `resolve_scopes_in_file_chunks`' own per-chunk merge of each chunk's edges
/// and consumed-word map into the build-wide accumulators.
static CHUNK_WORDS_MERGE_NS: AtomicU64 = AtomicU64::new(0);
/// Freeing the previous build's state at the end of `GraphSession::run`:
/// per-file cached resolutions (edges + consumed words + read sets), the
/// previous fingerprint map, and the RED files' old entities. Pure
/// deallocation, `O(corpus)`, and invisible to every other timer here because
/// it happens after the build function has already returned.
static SESSION_DROP_NS: AtomicU64 = AtomicU64::new(0);

/// `resolve_references_with_file_indexes` (the "bag-of-words" path): total
/// wall time.
static BOW_WALL_NS: AtomicU64 = AtomicU64::new(0);
/// Sum across files (parallel) of `build_file_reference_index`:
/// `strip_for_language` + `FileReferenceIndex` construction, still fused with
/// this file's own resolve step inside `resolve_references_with_file_indexes`'s
/// per-file closure (: `BOW_INDEX_IO_NS` below is the sub-bucket that
/// used to dominate this — a genuine second file read, past pass 1 and pass
/// 2's reparse — now near-zero for files `snapshot_bow_content` covered;
/// see that function's doc comment for why the fusion itself is unchanged).
static BOW_INDEX_BUILD_NS: AtomicU64 = AtomicU64::new(0);
/// Sum across files (parallel) of the per-entity `resolve_entity_references`
/// loop (dot-chain extraction + local-binding scan + candidate matching).
static BOW_RESOLVE_NS: AtomicU64 = AtomicU64::new(0);
/// wall time of `snapshot_bow_content` — the small pre-step, run
/// right after pass 1 and before scope resolution moves `parsed_files` away,
/// that copies every file's content pass 1 already read into a map
/// `build_file_reference_index` can look up instead of re-reading. Cheap and
/// memcpy-bound (no strip/tokenize work here — that stays fused with the
/// resolve loop, see `BOW_INDEX_BUILD_NS`'s doc comment for why a first
/// version of this change that *did* move tokenize into this phase regressed
/// wall time). Kept as its own bucket to make that "this phase is cheap"
/// claim checkable rather than asserted.
static BOW_INDEX_PRECOMPUTE_WALL_NS: AtomicU64 = AtomicU64::new(0);

// ---- bag-of-words sub-phase accumulators: drilling below the
// index-build vs resolve-loop split left as a proposal. Same
// zero-cost-when-off contract; see `BowFileAccum` below for the per-file,
// zero-locking accumulation discipline (identical shape to `FileAccum`).

/// Sub-bucket of `BOW_INDEX_BUILD_NS`: `std::fs::read_to_string` inside
/// `build_file_reference_index` (the *second* read of a file's content, past
/// pass 1 and pass 2's own reads).
static BOW_INDEX_IO_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of `BOW_INDEX_BUILD_NS`: `strip_for_language` +
/// `FileReferenceIndex::from_stripped` (per-line tokenization, string
/// interning into `tokens`/`token_ids`, dot-chain extraction) for the whole
/// file.
static BOW_INDEX_TOKENIZE_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of `BOW_RESOLVE_NS`, summed across entities: extracting the
/// dot-chain list for one entity (`dot_chains_in_ranges` on the pre-built
/// index, or `extract_dot_chains_with_positions` on the AST-fallback path).
static BOW_DOTCHAIN_EXTRACT_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of `BOW_RESOLVE_NS`, summed across entities: matching each
/// extracted dot-chain against `class_members` (the `self`/receiver member
/// lookup loop) — the bag-of-words analog of `resolve_ref`'s
/// `select_member_candidate` scan, not previously instrumented separately.
static BOW_DOTCHAIN_MATCH_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of `BOW_RESOLVE_NS`, summed across entities:
/// `local_binding_names_filtered`.
static BOW_LOCAL_BINDING_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of `BOW_RESOLVE_NS`, summed across entities: extracting the
/// reference-word list for one entity (`refs_with_types_in_ranges` on the
/// pre-built index, or `extract_references_with_stripped_filtered` on the
/// AST-fallback path).
static BOW_REF_EXTRACT_NS: AtomicU64 = AtomicU64::new(0);
/// Sub-bucket of `BOW_RESOLVE_NS`, summed across entities: matching each
/// extracted reference word against `imports_by_file`/`symbol_table` — the
/// bag-of-words analog of `resolve_ref`'s `symbol_table.get(name)` fast
/// path, not previously instrumented separately.
static BOW_REF_MATCH_NS: AtomicU64 = AtomicU64::new(0);

static HIST_BOW_CLASS: [AtomicU64; NBUCKETS] = [const { AtomicU64::new(0) }; NBUCKETS];
static HIST_BOW_SYMBOL: [AtomicU64; NBUCKETS] = [const { AtomicU64::new(0) }; NBUCKETS];

/// `build_export_alias_edges` (sequential).
static EXPORT_EDGES_NS: AtomicU64 = AtomicU64::new(0);
/// `dedupe_resolved_edges` (sequential).
static DEDUPE_NS: AtomicU64 = AtomicU64::new(0);
/// `sort_resolved_refs` (sequential).
static SORT_NS: AtomicU64 = AtomicU64::new(0);
/// Building the `dependents`/`dependencies` `HashMap`s + `edges: Vec<EntityRef>`
/// from the sorted/deduped edge list (sequential).
static EDGE_INDEX_NS: AtomicU64 = AtomicU64::new(0);

static HIST_METHOD: [AtomicU64; NBUCKETS] = [const { AtomicU64::new(0) }; NBUCKETS];
static HIST_CALL: [AtomicU64; NBUCKETS] = [const { AtomicU64::new(0) }; NBUCKETS];

#[derive(Default, Clone)]
pub struct NameAgg {
    pub calls: u64,
    pub total_candidates: u64,
    pub max_candidates: u32,
    pub total_ns: u64,
}

impl NameAgg {
    /// Record one candidate-lookup sample: `candidates` scanned, and
    /// `elapsed` when the call site actually timed the scan. The bare-call
    /// fast path (`FileAccum::record_call_global`) doesn't scan the list, so
    /// it has no timing to contribute and passes `None` — `total_ns` is left
    /// untouched rather than double-counting a zero duration.
    fn record(&mut self, candidates: usize, elapsed: Option<Duration>) {
        self.calls += 1;
        self.total_candidates += candidates as u64;
        self.max_candidates = self.max_candidates.max(candidates as u32);
        if let Some(elapsed) = elapsed {
            self.total_ns += elapsed.as_nanos() as u64;
        }
    }
}

fn name_stats_method() -> &'static Mutex<StdHashMap<String, NameAgg>> {
    static M: OnceLock<Mutex<StdHashMap<String, NameAgg>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn name_stats_call() -> &'static Mutex<StdHashMap<String, NameAgg>> {
    static M: OnceLock<Mutex<StdHashMap<String, NameAgg>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn bow_stats_class() -> &'static Mutex<StdHashMap<String, NameAgg>> {
    static M: OnceLock<Mutex<StdHashMap<String, NameAgg>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn bow_stats_symbol() -> &'static Mutex<StdHashMap<String, NameAgg>> {
    static M: OnceLock<Mutex<StdHashMap<String, NameAgg>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn threads_seen() -> &'static Mutex<std::collections::HashSet<ThreadId>> {
    static M: OnceLock<Mutex<std::collections::HashSet<ThreadId>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn chunk_wall_ns() -> &'static Mutex<Vec<u64>> {
    static M: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Vec::new()))
}

/// Per-file accumulator for candidate-lookup samples. Built and mutated
/// locally inside one rayon closure (one file), with no locking, then handed
/// to [`merge_file`] once at the end of the closure.
#[derive(Default)]
pub struct FileAccum {
    method_call: StdHashMap<String, NameAgg>,
    call_global: StdHashMap<String, NameAgg>,
    hist_method: [u64; NBUCKETS],
    hist_call: [u64; NBUCKETS],
}

impl FileAccum {
    /// Record one `class_members.get(type_hint)` -> `select_member_candidate`
    /// disambiguation call: the candidate-list length actually scanned and
    /// the wall time `select_member_candidate` took.
    pub fn record_method_call(&mut self, type_hint: &str, candidates: usize, elapsed: Duration) {
        let agg = self.method_call.entry(type_hint.to_string()).or_default();
        agg.record(candidates, Some(elapsed));
        self.hist_method[bucket_index(candidates)] += 1;
    }

    /// Record one `symbol_table.get(name)` global candidate-list read on the
    /// bare-call fast path (which does not scan the list — see resolve_ref —
    /// so only the candidate-list size is meaningful here, not a timing).
    pub fn record_call_global(&mut self, name: &str, candidates: usize) {
        let agg = self.call_global.entry(name.to_string()).or_default();
        agg.record(candidates, None);
        self.hist_call[bucket_index(candidates)] += 1;
    }
}

/// Per-file accumulator for bag-of-words sub-phase timing + candidate-scan
/// samples, built and mutated locally inside one rayon closure
/// (one file, across every entity in it) with no locking, then handed to
/// [`merge_bow_file`] once at the end of the closure — same discipline as
/// [`FileAccum`], and separate from it because bag-of-words and the scope
/// resolver (`resolve_ref`) are different code paths over different tables.
#[derive(Default)]
pub struct BowFileAccum {
    dotchain_extract_ns: u64,
    dotchain_match_ns: u64,
    local_binding_ns: u64,
    ref_extract_ns: u64,
    ref_match_ns: u64,
    class_scan: StdHashMap<String, NameAgg>,
    symbol_scan: StdHashMap<String, NameAgg>,
    hist_class: [u64; NBUCKETS],
    hist_symbol: [u64; NBUCKETS],
}

impl BowFileAccum {
    #[inline]
    pub fn add_dotchain_extract(&mut self, d: Duration) {
        self.dotchain_extract_ns += d.as_nanos() as u64;
    }

    #[inline]
    pub fn add_local_binding(&mut self, d: Duration) {
        self.local_binding_ns += d.as_nanos() as u64;
    }

    #[inline]
    pub fn add_ref_extract(&mut self, d: Duration) {
        self.ref_extract_ns += d.as_nanos() as u64;
    }

    /// Record one `class_members.get(owner)`-driven member-name scan (the
    /// `self`/receiver dot-chain match loop): the candidate-list length
    /// actually scanned and the wall time spent scanning it.
    pub fn record_class_scan(&mut self, owner: &str, candidates: usize, elapsed: Duration) {
        self.dotchain_match_ns += elapsed.as_nanos() as u64;
        let agg = self.class_scan.entry(owner.to_string()).or_default();
        agg.record(candidates, Some(elapsed));
        self.hist_class[bucket_index(candidates)] += 1;
    }

    /// Record one `symbol_table.get(name)`-driven `.iter().find(..)` scan
    /// (the global-ref candidate match): the candidate-list length actually
    /// scanned and the wall time spent scanning it.
    pub fn record_symbol_scan(&mut self, name: &str, candidates: usize, elapsed: Duration) {
        self.ref_match_ns += elapsed.as_nanos() as u64;
        let agg = self.symbol_scan.entry(name.to_string()).or_default();
        agg.record(candidates, Some(elapsed));
        self.hist_symbol[bucket_index(candidates)] += 1;
    }
}

/// Merge one file's worth of bag-of-words sub-phase timing + candidate scans
/// into the global counters. Called once per file (not per entity or per
/// reference), so lock contention scales with file count.
pub fn merge_bow_file(accum: BowFileAccum) {
    if !enabled() {
        return;
    }
    BOW_DOTCHAIN_EXTRACT_NS.fetch_add(accum.dotchain_extract_ns, Ordering::Relaxed);
    BOW_DOTCHAIN_MATCH_NS.fetch_add(accum.dotchain_match_ns, Ordering::Relaxed);
    BOW_LOCAL_BINDING_NS.fetch_add(accum.local_binding_ns, Ordering::Relaxed);
    BOW_REF_EXTRACT_NS.fetch_add(accum.ref_extract_ns, Ordering::Relaxed);
    BOW_REF_MATCH_NS.fetch_add(accum.ref_match_ns, Ordering::Relaxed);

    for (i, v) in accum.hist_class.iter().enumerate() {
        if *v > 0 {
            HIST_BOW_CLASS[i].fetch_add(*v, Ordering::Relaxed);
        }
    }
    for (i, v) in accum.hist_symbol.iter().enumerate() {
        if *v > 0 {
            HIST_BOW_SYMBOL[i].fetch_add(*v, Ordering::Relaxed);
        }
    }

    if !accum.class_scan.is_empty() {
        let mut g = bow_stats_class().lock().unwrap();
        for (k, v) in accum.class_scan {
            let e = g.entry(k).or_default();
            e.calls += v.calls;
            e.total_candidates += v.total_candidates;
            e.max_candidates = e.max_candidates.max(v.max_candidates);
            e.total_ns += v.total_ns;
        }
    }
    if !accum.symbol_scan.is_empty() {
        let mut g = bow_stats_symbol().lock().unwrap();
        for (k, v) in accum.symbol_scan {
            let e = g.entry(k).or_default();
            e.calls += v.calls;
            e.total_candidates += v.total_candidates;
            e.max_candidates = e.max_candidates.max(v.max_candidates);
            e.total_ns += v.total_ns;
        }
    }
}

macro_rules! add_ns_fn {
    ($fn_name:ident, $bucket:ident) => {
        pub fn $fn_name(d: Duration) {
            if enabled() {
                $bucket.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
            }
        }
    };
}
add_ns_fn!(add_reparse_ns, REPARSE_NS);
add_ns_fn!(add_pass1_scan_ns, PASS1_SCAN_NS);
add_ns_fn!(add_ctor_infer_ns, CTOR_INFER_NS);
add_ns_fn!(add_import_group_ns, IMPORT_GROUP_NS);
add_ns_fn!(add_pass2_wall_ns, PASS2_WALL_NS);
add_ns_fn!(add_chunk_entity_index_ns, CHUNK_ENTITY_INDEX_NS);
add_ns_fn!(add_return_types_by_name_ns, RETURN_TYPES_BY_NAME_NS);
add_ns_fn!(add_scope_merge_ns, SCOPE_MERGE_NS);
add_ns_fn!(add_scope_dedup_ns, SCOPE_DEDUP_NS);
add_ns_fn!(add_import_table_wall_ns, IMPORT_TABLE_WALL_NS);
add_ns_fn!(add_import_table_io_ns, IMPORT_TABLE_IO_NS);
add_ns_fn!(add_import_table_scan_ns, IMPORT_TABLE_SCAN_NS);
add_ns_fn!(add_import_table_merge_ns, IMPORT_TABLE_MERGE_NS);
add_ns_fn!(
    add_import_table_export_build_ns,
    IMPORT_TABLE_EXPORT_BUILD_NS
);
add_ns_fn!(add_import_table_insert_ns, IMPORT_TABLE_INSERT_NS);
add_ns_fn!(add_imports_by_file_ns, IMPORTS_BY_FILE_NS);
add_ns_fn!(add_symbol_table_by_file_ns, SYMBOL_TABLE_BY_FILE_NS);
add_ns_fn!(add_entity_lookup_build_ns, ENTITY_LOOKUP_BUILD_NS);
add_ns_fn!(
    add_fingerprint_corpus_tables_ns,
    FINGERPRINT_CORPUS_TABLES_NS
);
add_ns_fn!(add_lookup_pass_a_ns, LOOKUP_PASS_A_NS);
add_ns_fn!(add_lookup_child_ranges_ns, LOOKUP_CHILD_RANGES_NS);
add_ns_fn!(add_lookup_owned_ns, LOOKUP_OWNED_NS);
add_ns_fn!(add_lookup_pass_b_ns, LOOKUP_PASS_B_NS);
add_ns_fn!(add_lookup_go_pkg_ns, LOOKUP_GO_PKG_NS);
add_ns_fn!(add_fingerprint_bow_tables_ns, FINGERPRINT_BOW_TABLES_NS);
add_ns_fn!(add_pass1_wall_ns, PASS1_WALL_NS);
add_ns_fn!(add_assemble_ns, ASSEMBLE_NS);
add_ns_fn!(add_clean_gate_ns, CLEAN_GATE_NS);

/// MUL Phase 1: record how many files the CLEAN gate dropped this
/// build (see `CLEAN_GATE_FILES_DROPPED`'s doc comment).
pub(crate) fn add_clean_gate_files_dropped(n: u64) {
    if enabled() && n > 0 {
        CLEAN_GATE_FILES_DROPPED.fetch_add(n, Ordering::Relaxed);
    }
}
add_ns_fn!(add_scope_wall_ns, SCOPE_WALL_NS);
add_ns_fn!(add_chunk_words_merge_ns, CHUNK_WORDS_MERGE_NS);
add_ns_fn!(add_post_resolve_ns, POST_RESOLVE_NS);

/// Overwrites (does not accumulate): one session build has exactly one prep and
/// one post phase, and neither is inside [`reset`]'s span.
pub(crate) fn set_session_prep_ns(d: Duration) {
    if enabled() {
        SESSION_PREP_NS.store(d.as_nanos() as u64, Ordering::Relaxed);
    }
}

pub(crate) fn set_session_post_ns(d: Duration) {
    if enabled() {
        SESSION_POST_NS.store(d.as_nanos() as u64, Ordering::Relaxed);
    }
}

pub(crate) fn set_session_drop_ns(d: Duration) {
    if enabled() {
        SESSION_DROP_NS.store(d.as_nanos() as u64, Ordering::Relaxed);
    }
}
add_ns_fn!(add_bow_wall_ns, BOW_WALL_NS);
add_ns_fn!(add_bow_index_build_ns, BOW_INDEX_BUILD_NS);
add_ns_fn!(add_bow_resolve_ns, BOW_RESOLVE_NS);
add_ns_fn!(add_bow_index_io_ns, BOW_INDEX_IO_NS);
add_ns_fn!(add_bow_index_tokenize_ns, BOW_INDEX_TOKENIZE_NS);
add_ns_fn!(
    add_bow_index_precompute_wall_ns,
    BOW_INDEX_PRECOMPUTE_WALL_NS
);
add_ns_fn!(add_export_edges_ns, EXPORT_EDGES_NS);
add_ns_fn!(add_dedupe_ns, DEDUPE_NS);
add_ns_fn!(add_sort_ns, SORT_NS);
add_ns_fn!(add_edge_index_ns, EDGE_INDEX_NS);

pub fn note_chunk(d: Duration) {
    if enabled() {
        chunk_wall_ns().lock().unwrap().push(d.as_nanos() as u64);
    }
}

/// Merge one file's worth of per-file timing + candidate accumulation into
/// the global counters. Called once per file (not per reference), so lock
/// contention scales with file count, not reference count.
#[allow(clippy::too_many_arguments)]
pub fn merge_file(
    accum: FileAccum,
    scope_build_ns: u64,
    ref_collect_ns: u64,
    ref_loop_ns: u64,
    resolve_ref_ns: u64,
    cache_hit: u64,
    cache_miss: u64,
) {
    if !enabled() {
        return;
    }
    FILES_PROCESSED.fetch_add(1, Ordering::Relaxed);
    SCOPE_BUILD_NS.fetch_add(scope_build_ns, Ordering::Relaxed);
    REF_COLLECT_NS.fetch_add(ref_collect_ns, Ordering::Relaxed);
    REF_LOOP_NS.fetch_add(ref_loop_ns, Ordering::Relaxed);
    RESOLVE_REF_NS.fetch_add(resolve_ref_ns, Ordering::Relaxed);
    CACHE_HIT.fetch_add(cache_hit, Ordering::Relaxed);
    CACHE_MISS.fetch_add(cache_miss, Ordering::Relaxed);

    for (i, v) in accum.hist_method.iter().enumerate() {
        if *v > 0 {
            HIST_METHOD[i].fetch_add(*v, Ordering::Relaxed);
        }
    }
    for (i, v) in accum.hist_call.iter().enumerate() {
        if *v > 0 {
            HIST_CALL[i].fetch_add(*v, Ordering::Relaxed);
        }
    }

    if !accum.method_call.is_empty() {
        let mut g = name_stats_method().lock().unwrap();
        for (k, v) in accum.method_call {
            let e = g.entry(k).or_default();
            e.calls += v.calls;
            e.total_candidates += v.total_candidates;
            e.max_candidates = e.max_candidates.max(v.max_candidates);
            e.total_ns += v.total_ns;
        }
    }
    if !accum.call_global.is_empty() {
        let mut g = name_stats_call().lock().unwrap();
        for (k, v) in accum.call_global {
            let e = g.entry(k).or_default();
            e.calls += v.calls;
            e.total_candidates += v.total_candidates;
            e.max_candidates = e.max_candidates.max(v.max_candidates);
        }
    }

    threads_seen()
        .lock()
        .unwrap()
        .insert(std::thread::current().id());
}

/// Reset all accumulators. Called at the start of a top-level
/// `EntityGraph::build` so each build reports fresh numbers instead of
/// accumulating across repeated builds in one process.
pub fn reset() {
    if !enabled() {
        return;
    }
    REPARSE_NS.store(0, Ordering::Relaxed);
    PASS1_SCAN_NS.store(0, Ordering::Relaxed);
    CTOR_INFER_NS.store(0, Ordering::Relaxed);
    IMPORT_GROUP_NS.store(0, Ordering::Relaxed);
    PASS2_WALL_NS.store(0, Ordering::Relaxed);
    SCOPE_BUILD_NS.store(0, Ordering::Relaxed);
    for c in [
        &SB_ENTITY_LOOKUP_NS,
        &SB_ENTITY_SPANS_NS,
        &SB_PRECOMPUTED_CLONE_NS,
        &SB_BUILD_SCOPES_AST_NS,
        &SB_COLLECT_REFS_NS,
        &SB_EXTRACT_IMPORTS_NS,
        &SB_IMPORT_REKEY_NS,
        &SB_INJECT_RETURN_TYPES_NS,
        &SB_INJECT_FIELD_TYPES_NS,
        &SB_FILES_PRECOMPUTED,
        &SB_FILES_AST,
        &SB_ENTITIES_SPANNED,
        &SB_SCOPES_BUILT,
        &SB_REFS_COLLECTED,
        &SB_PRECOMPUTED_IMPORT_FILES,
        &SB_PRECOMPUTED_IMPORT_DESCRIPTORS,
        &SB_PRECOMPUTED_CTOR_CALL_FILES,
        &SB_PRECOMPUTED_CTOR_CALL_DESCRIPTORS,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    REF_COLLECT_NS.store(0, Ordering::Relaxed);
    REF_LOOP_NS.store(0, Ordering::Relaxed);
    RESOLVE_REF_NS.store(0, Ordering::Relaxed);
    CACHE_HIT.store(0, Ordering::Relaxed);
    CACHE_MISS.store(0, Ordering::Relaxed);
    FILES_PROCESSED.store(0, Ordering::Relaxed);
    ENTITY_SCOPE_LOOKUPS.store(0, Ordering::Relaxed);
    ENTITY_SCOPE_LOOKUP_FALLBACKS.store(0, Ordering::Relaxed);
    RESOLVE_REF_TYPE_DIRECTED_ATTEMPTED.store(0, Ordering::Relaxed);
    RESOLVE_REF_TYPE_DIRECTED_SUCCEEDED.store(0, Ordering::Relaxed);
    CHUNK_ENTITY_INDEX_NS.store(0, Ordering::Relaxed);
    RETURN_TYPES_BY_NAME_NS.store(0, Ordering::Relaxed);
    SCOPE_MERGE_NS.store(0, Ordering::Relaxed);
    SCOPE_DEDUP_NS.store(0, Ordering::Relaxed);
    IMPORT_TABLE_WALL_NS.store(0, Ordering::Relaxed);
    IMPORT_TABLE_IO_NS.store(0, Ordering::Relaxed);
    IMPORT_TABLE_SCAN_NS.store(0, Ordering::Relaxed);
    IMPORT_TABLE_MERGE_NS.store(0, Ordering::Relaxed);
    IMPORT_TABLE_EXPORT_BUILD_NS.store(0, Ordering::Relaxed);
    IMPORT_TABLE_INSERT_NS.store(0, Ordering::Relaxed);
    IMPORTS_BY_FILE_NS.store(0, Ordering::Relaxed);
    SYMBOL_TABLE_BY_FILE_NS.store(0, Ordering::Relaxed);
    ENTITY_LOOKUP_BUILD_NS.store(0, Ordering::Relaxed);
    FINGERPRINT_CORPUS_TABLES_NS.store(0, Ordering::Relaxed);
    LOOKUP_PASS_A_NS.store(0, Ordering::Relaxed);
    LOOKUP_CHILD_RANGES_NS.store(0, Ordering::Relaxed);
    LOOKUP_OWNED_NS.store(0, Ordering::Relaxed);
    LOOKUP_PASS_B_NS.store(0, Ordering::Relaxed);
    LOOKUP_GO_PKG_NS.store(0, Ordering::Relaxed);
    FINGERPRINT_BOW_TABLES_NS.store(0, Ordering::Relaxed);
    PASS1_WALL_NS.store(0, Ordering::Relaxed);
    ASSEMBLE_NS.store(0, Ordering::Relaxed);
    CLEAN_GATE_NS.store(0, Ordering::Relaxed);
    CLEAN_GATE_FILES_DROPPED.store(0, Ordering::Relaxed);
    SCOPE_WALL_NS.store(0, Ordering::Relaxed);
    CHUNK_WORDS_MERGE_NS.store(0, Ordering::Relaxed);
    POST_RESOLVE_NS.store(0, Ordering::Relaxed);
    BOW_WALL_NS.store(0, Ordering::Relaxed);
    BOW_INDEX_BUILD_NS.store(0, Ordering::Relaxed);
    BOW_INDEX_PRECOMPUTE_WALL_NS.store(0, Ordering::Relaxed);
    BOW_RESOLVE_NS.store(0, Ordering::Relaxed);
    BOW_INDEX_IO_NS.store(0, Ordering::Relaxed);
    BOW_INDEX_TOKENIZE_NS.store(0, Ordering::Relaxed);
    BOW_DOTCHAIN_EXTRACT_NS.store(0, Ordering::Relaxed);
    BOW_DOTCHAIN_MATCH_NS.store(0, Ordering::Relaxed);
    BOW_LOCAL_BINDING_NS.store(0, Ordering::Relaxed);
    BOW_REF_EXTRACT_NS.store(0, Ordering::Relaxed);
    BOW_REF_MATCH_NS.store(0, Ordering::Relaxed);
    EXPORT_EDGES_NS.store(0, Ordering::Relaxed);
    DEDUPE_NS.store(0, Ordering::Relaxed);
    SORT_NS.store(0, Ordering::Relaxed);
    EDGE_INDEX_NS.store(0, Ordering::Relaxed);
    for a in &HIST_METHOD {
        a.store(0, Ordering::Relaxed);
    }
    for a in &HIST_CALL {
        a.store(0, Ordering::Relaxed);
    }
    for a in &HIST_BOW_CLASS {
        a.store(0, Ordering::Relaxed);
    }
    for a in &HIST_BOW_SYMBOL {
        a.store(0, Ordering::Relaxed);
    }
    name_stats_method().lock().unwrap().clear();
    name_stats_call().lock().unwrap().clear();
    bow_stats_class().lock().unwrap().clear();
    bow_stats_symbol().lock().unwrap().clear();
    threads_seen().lock().unwrap().clear();
    chunk_wall_ns().lock().unwrap().clear();
}

fn percentile_from_hist(hist: &[u64; NBUCKETS], p: f64) -> (usize, u64) {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return (0, 0);
    }
    let target = ((total as f64) * p).ceil() as u64;
    let mut cum = 0u64;
    for (b, &count) in hist.iter().enumerate() {
        cum += count;
        if cum >= target {
            return (b, count);
        }
    }
    (NBUCKETS - 1, hist[NBUCKETS - 1])
}

fn max_bucket(hist: &[u64; NBUCKETS]) -> usize {
    hist.iter()
        .enumerate()
        .rev()
        .find(|(_, &c)| c > 0)
        .map(|(b, _)| b)
        .unwrap_or(0)
}

/// Print a full report to stderr if profiling is enabled; a no-op otherwise.
/// Called once, at the end of a top-level `EntityGraph::build`.
pub fn maybe_print_report() {
    if !enabled() {
        return;
    }
    let ms = |ns: u64| ns as f64 / 1_000_000.0;

    eprintln!("SEM_PROFILE_RESOLVE report ---------------------------------");
    eprintln!(
        "PHASE_NS files={} reparse_ms={:.2} pass1_scan_ms={:.2} ctor_infer_ms={:.2} return_types_by_name_ms={:.2} import_group_ms={:.2} pass2_wall_ms={:.2} chunk_entity_index_ms={:.2} scope_merge_ms={:.2} scope_dedup_ms={:.2} scope_build_ms={:.2} ref_collect_ms={:.2} ref_loop_ms={:.2} resolve_ref_ms={:.2}",
        FILES_PROCESSED.load(Ordering::Relaxed),
        ms(REPARSE_NS.load(Ordering::Relaxed)),
        ms(PASS1_SCAN_NS.load(Ordering::Relaxed)),
        ms(CTOR_INFER_NS.load(Ordering::Relaxed)),
        ms(RETURN_TYPES_BY_NAME_NS.load(Ordering::Relaxed)),
        ms(IMPORT_GROUP_NS.load(Ordering::Relaxed)),
        ms(PASS2_WALL_NS.load(Ordering::Relaxed)),
        ms(CHUNK_ENTITY_INDEX_NS.load(Ordering::Relaxed)),
        ms(SCOPE_MERGE_NS.load(Ordering::Relaxed)),
        ms(SCOPE_DEDUP_NS.load(Ordering::Relaxed)),
        ms(SCOPE_BUILD_NS.load(Ordering::Relaxed)),
        ms(REF_COLLECT_NS.load(Ordering::Relaxed)),
        ms(REF_LOOP_NS.load(Ordering::Relaxed)),
        ms(RESOLVE_REF_NS.load(Ordering::Relaxed)),
    );

    // scope_build's constituents. `residual_ms` is
    // `scope_build_ms` minus the sum below — the part of the region no
    // sub-timer covers (config lookup, content selection, the timers
    // themselves). Reported rather than silently folded into a neighbour.
    let sb_sum = SB_ENTITY_LOOKUP_NS.load(Ordering::Relaxed)
        + SB_ENTITY_SPANS_NS.load(Ordering::Relaxed)
        + SB_PRECOMPUTED_CLONE_NS.load(Ordering::Relaxed)
        + SB_BUILD_SCOPES_AST_NS.load(Ordering::Relaxed)
        + SB_COLLECT_REFS_NS.load(Ordering::Relaxed)
        + SB_FUSED_WALK_NS.load(Ordering::Relaxed)
        + SB_EXTRACT_IMPORTS_NS.load(Ordering::Relaxed)
        + SB_IMPORT_REKEY_NS.load(Ordering::Relaxed)
        + SB_INJECT_RETURN_TYPES_NS.load(Ordering::Relaxed)
        + SB_INJECT_FIELD_TYPES_NS.load(Ordering::Relaxed);
    eprintln!(
        "SCOPE_BUILD_NS total_ms={:.2} entity_lookup_ms={:.2} entity_spans_ms={:.2} precomputed_clone_ms={:.2} build_scopes_ast_ms={:.2} collect_refs_ms={:.2} fused_walk_ms={:.2} extract_imports_ms={:.2} import_rekey_ms={:.2} inject_return_types_ms={:.2} inject_field_types_ms={:.2} residual_ms={:.2}",
        ms(SCOPE_BUILD_NS.load(Ordering::Relaxed)),
        ms(SB_ENTITY_LOOKUP_NS.load(Ordering::Relaxed)),
        ms(SB_ENTITY_SPANS_NS.load(Ordering::Relaxed)),
        ms(SB_PRECOMPUTED_CLONE_NS.load(Ordering::Relaxed)),
        ms(SB_BUILD_SCOPES_AST_NS.load(Ordering::Relaxed)),
        ms(SB_COLLECT_REFS_NS.load(Ordering::Relaxed)),
        ms(SB_FUSED_WALK_NS.load(Ordering::Relaxed)),
        ms(SB_EXTRACT_IMPORTS_NS.load(Ordering::Relaxed)),
        ms(SB_IMPORT_REKEY_NS.load(Ordering::Relaxed)),
        ms(SB_INJECT_RETURN_TYPES_NS.load(Ordering::Relaxed)),
        ms(SB_INJECT_FIELD_TYPES_NS.load(Ordering::Relaxed)),
        ms(SCOPE_BUILD_NS.load(Ordering::Relaxed).saturating_sub(sb_sum)),
    );
    eprintln!(
        "SCOPE_BUILD_WORK files_precomputed={} files_ast={} files_fused={} entities_spanned={} scopes_built={} refs_collected={} files_precomputed_with_imports={} precomputed_import_descriptors={} files_precomputed_with_ctor_calls={} precomputed_ctor_call_descriptors={}",
        SB_FILES_PRECOMPUTED.load(Ordering::Relaxed),
        SB_FILES_AST.load(Ordering::Relaxed),
        SB_FILES_FUSED.load(Ordering::Relaxed),
        SB_ENTITIES_SPANNED.load(Ordering::Relaxed),
        SB_SCOPES_BUILT.load(Ordering::Relaxed),
        SB_REFS_COLLECTED.load(Ordering::Relaxed),
        SB_PRECOMPUTED_IMPORT_FILES.load(Ordering::Relaxed),
        SB_PRECOMPUTED_IMPORT_DESCRIPTORS.load(Ordering::Relaxed),
        SB_PRECOMPUTED_CTOR_CALL_FILES.load(Ordering::Relaxed),
        SB_PRECOMPUTED_CTOR_CALL_DESCRIPTORS.load(Ordering::Relaxed),
    );

    // Pass-2 diagnostic counters. The scope-index pair covers precomputed-file
    // entities only (see the statics' doc comments); fallback_pct is the share
    // of those lookups that silently defaulted into scope 0.
    let es_total = ENTITY_SCOPE_LOOKUPS.load(Ordering::Relaxed);
    let es_fallback = ENTITY_SCOPE_LOOKUP_FALLBACKS.load(Ordering::Relaxed);
    let es_pct = if es_total > 0 {
        es_fallback as f64 / es_total as f64 * 100.0
    } else {
        0.0
    };
    eprintln!(
        "ENTITY_SCOPE_LOOKUP precomputed_file_lookups={es_total} fallback_default0={es_fallback} fallback_pct={es_pct:.2}"
    );
    let td_attempted = RESOLVE_REF_TYPE_DIRECTED_ATTEMPTED.load(Ordering::Relaxed);
    let td_succeeded = RESOLVE_REF_TYPE_DIRECTED_SUCCEEDED.load(Ordering::Relaxed);
    let td_pct = if td_attempted > 0 {
        td_succeeded as f64 / td_attempted as f64 * 100.0
    } else {
        0.0
    };
    eprintln!(
        "RESOLVE_REF_TYPE_DIRECTED attempted={td_attempted} succeeded={td_succeeded} succeed_pct={td_pct:.2}"
    );

    eprintln!(
        "IMPORT_TABLE_NS wall_ms={:.2} io_ms={:.2} scan_ms={:.2} merge_ms={:.2} merge_export_build_ms={:.2} merge_insert_ms={:.2}",
        ms(IMPORT_TABLE_WALL_NS.load(Ordering::Relaxed)),
        ms(IMPORT_TABLE_IO_NS.load(Ordering::Relaxed)),
        ms(IMPORT_TABLE_SCAN_NS.load(Ordering::Relaxed)),
        ms(IMPORT_TABLE_MERGE_NS.load(Ordering::Relaxed)),
        ms(IMPORT_TABLE_EXPORT_BUILD_NS.load(Ordering::Relaxed)),
        ms(IMPORT_TABLE_INSERT_NS.load(Ordering::Relaxed)),
    );
    eprintln!(
        "RESIDUAL_NS imports_by_file_ms={:.2} symbol_table_by_file_ms={:.2} bow_wall_ms={:.2} bow_index_build_ms={:.2} bow_index_precompute_wall_ms={:.2} bow_resolve_ms={:.2} export_edges_ms={:.2} dedupe_ms={:.2} sort_ms={:.2} edge_index_ms={:.2} entity_lookup_build_ms={:.2} fingerprint_corpus_tables_ms={:.2}",
        ms(IMPORTS_BY_FILE_NS.load(Ordering::Relaxed)),
        ms(SYMBOL_TABLE_BY_FILE_NS.load(Ordering::Relaxed)),
        ms(BOW_WALL_NS.load(Ordering::Relaxed)),
        ms(BOW_INDEX_BUILD_NS.load(Ordering::Relaxed)),
        ms(BOW_INDEX_PRECOMPUTE_WALL_NS.load(Ordering::Relaxed)),
        ms(BOW_RESOLVE_NS.load(Ordering::Relaxed)),
        ms(EXPORT_EDGES_NS.load(Ordering::Relaxed)),
        ms(DEDUPE_NS.load(Ordering::Relaxed)),
        ms(SORT_NS.load(Ordering::Relaxed)),
        ms(EDGE_INDEX_NS.load(Ordering::Relaxed)),
        ms(ENTITY_LOOKUP_BUILD_NS.load(Ordering::Relaxed)),
        ms(FINGERPRINT_CORPUS_TABLES_NS.load(Ordering::Relaxed)),
    );
    eprintln!(
        "LOOKUP_NS pass_a_ms={:.2} child_ranges_ms={:.2} owned_ms={:.2} pass_b_ms={:.2} go_pkg_ms={:.2} fingerprint_bow_ms={:.2}",
        ms(LOOKUP_PASS_A_NS.load(Ordering::Relaxed)),
        ms(LOOKUP_CHILD_RANGES_NS.load(Ordering::Relaxed)),
        ms(LOOKUP_OWNED_NS.load(Ordering::Relaxed)),
        ms(LOOKUP_PASS_B_NS.load(Ordering::Relaxed)),
        ms(LOOKUP_GO_PKG_NS.load(Ordering::Relaxed)),
        ms(FINGERPRINT_BOW_TABLES_NS.load(Ordering::Relaxed)),
    );
    eprintln!(
        "FRAME_NS pass1_wall_ms={:.2} assemble_ms={:.2} scope_wall_ms={:.2} chunk_words_merge_ms={:.2} post_resolve_ms={:.2} session_prep_ms={:.2} session_post_prev_ms={:.2} session_drop_prev_ms={:.2}",
        ms(PASS1_WALL_NS.load(Ordering::Relaxed)),
        ms(ASSEMBLE_NS.load(Ordering::Relaxed)),
        ms(SCOPE_WALL_NS.load(Ordering::Relaxed)),
        ms(CHUNK_WORDS_MERGE_NS.load(Ordering::Relaxed)),
        ms(POST_RESOLVE_NS.load(Ordering::Relaxed)),
        ms(SESSION_PREP_NS.load(Ordering::Relaxed)),
        ms(SESSION_POST_NS.load(Ordering::Relaxed)),
        ms(SESSION_DROP_NS.load(Ordering::Relaxed)),
    );
    eprintln!(
        "MUL_CLEAN_GATE clean_gate_ms={:.2} files_dropped={}",
        ms(CLEAN_GATE_NS.load(Ordering::Relaxed)),
        CLEAN_GATE_FILES_DROPPED.load(Ordering::Relaxed),
    );
    eprintln!(
        "BOW_PHASE_NS index_io_ms={:.2} index_tokenize_ms={:.2} dotchain_extract_ms={:.2} dotchain_match_ms={:.2} local_binding_ms={:.2} ref_extract_ms={:.2} ref_match_ms={:.2}",
        ms(BOW_INDEX_IO_NS.load(Ordering::Relaxed)),
        ms(BOW_INDEX_TOKENIZE_NS.load(Ordering::Relaxed)),
        ms(BOW_DOTCHAIN_EXTRACT_NS.load(Ordering::Relaxed)),
        ms(BOW_DOTCHAIN_MATCH_NS.load(Ordering::Relaxed)),
        ms(BOW_LOCAL_BINDING_NS.load(Ordering::Relaxed)),
        ms(BOW_REF_EXTRACT_NS.load(Ordering::Relaxed)),
        ms(BOW_REF_MATCH_NS.load(Ordering::Relaxed)),
    );

    let hit = CACHE_HIT.load(Ordering::Relaxed);
    let miss = CACHE_MISS.load(Ordering::Relaxed);
    let total_refs = hit + miss;
    let hit_rate = if total_refs > 0 {
        hit as f64 / total_refs as f64 * 100.0
    } else {
        0.0
    };
    eprintln!(
        "REF_CACHE total_refs={total_refs} cache_hit={hit} cache_miss={miss} hit_rate_pct={hit_rate:.2}"
    );

    let n_threads = threads_seen().lock().unwrap().len();
    let avail = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    eprintln!("THREAD_UTIL distinct_worker_threads_seen={n_threads} available_parallelism={avail}");

    let chunks = chunk_wall_ns().lock().unwrap();
    if !chunks.is_empty() {
        let mut sorted = chunks.clone();
        sorted.sort_unstable();
        let sum: u64 = sorted.iter().sum();
        let min = *sorted.first().unwrap();
        let max = *sorted.last().unwrap();
        let avg = sum / sorted.len() as u64;
        eprintln!(
            "CHUNKS count={} sum_ms={:.2} min_ms={:.2} avg_ms={:.2} max_ms={:.2}",
            sorted.len(),
            ms(sum),
            ms(min),
            ms(avg),
            ms(max)
        );
    }
    drop(chunks);

    for (label, hist) in [
        ("method_call", &HIST_METHOD),
        ("call_global", &HIST_CALL),
        ("bow_class_members", &HIST_BOW_CLASS),
        ("bow_symbol_table", &HIST_BOW_SYMBOL),
    ] {
        let hist_snapshot: [u64; NBUCKETS] = {
            let mut arr = [0u64; NBUCKETS];
            for (i, a) in hist.iter().enumerate() {
                arr[i] = a.load(Ordering::Relaxed);
            }
            arr
        };
        let total: u64 = hist_snapshot.iter().sum();
        if total == 0 {
            continue;
        }
        let (p50_b, _) = percentile_from_hist(&hist_snapshot, 0.50);
        let (p95_b, _) = percentile_from_hist(&hist_snapshot, 0.95);
        let (p99_b, _) = percentile_from_hist(&hist_snapshot, 0.99);
        let max_b = max_bucket(&hist_snapshot);
        eprintln!(
            "CANDIDATE_DIST kind={label} total_lookups={total} p50={} p95={} p99={} max_bucket={}",
            bucket_range_label(p50_b),
            bucket_range_label(p95_b),
            bucket_range_label(p99_b),
            bucket_range_label(max_b),
        );
    }

    let mut method_stats: Vec<(String, NameAgg)> = name_stats_method()
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .collect();
    method_stats.sort_unstable_by_key(|(_, agg)| std::cmp::Reverse(agg.total_ns));
    eprintln!(
        "TOP20_METHOD_CALL_NAMES_BY_TIME (type_hint calls total_candidates max_candidates total_ms avg_candidates)"
    );
    for (name, agg) in method_stats.iter().take(20) {
        let avg_cand = if agg.calls > 0 {
            agg.total_candidates as f64 / agg.calls as f64
        } else {
            0.0
        };
        eprintln!(
            "  {name} calls={} total_candidates={} max_candidates={} total_ms={:.3} avg_candidates={:.1}",
            agg.calls,
            agg.total_candidates,
            agg.max_candidates,
            ms(agg.total_ns),
            avg_cand
        );
    }

    let mut call_stats: Vec<(String, NameAgg)> = name_stats_call()
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .collect();
    call_stats.sort_unstable_by_key(|(_, agg)| std::cmp::Reverse(agg.total_candidates));
    eprintln!(
        "TOP20_CALL_GLOBAL_NAMES_BY_CANDIDATES (name calls total_candidates max_candidates avg_candidates) -- fast path, not scanned, size shown for reference only"
    );
    for (name, agg) in call_stats.iter().take(20) {
        let avg_cand = if agg.calls > 0 {
            agg.total_candidates as f64 / agg.calls as f64
        } else {
            0.0
        };
        eprintln!(
            "  {name} calls={} total_candidates={} max_candidates={} avg_candidates={:.1}",
            agg.calls, agg.total_candidates, agg.max_candidates, avg_cand
        );
    }

    let mut bow_class_stats: Vec<(String, NameAgg)> = bow_stats_class()
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .collect();
    bow_class_stats.sort_unstable_by_key(|(_, agg)| std::cmp::Reverse(agg.total_ns));
    eprintln!(
        "TOP20_BOW_CLASS_MEMBERS_BY_TIME (owner calls total_candidates max_candidates total_ms avg_candidates) -- bag-of-words self/receiver dot-chain member match"
    );
    for (name, agg) in bow_class_stats.iter().take(20) {
        let avg_cand = if agg.calls > 0 {
            agg.total_candidates as f64 / agg.calls as f64
        } else {
            0.0
        };
        eprintln!(
            "  {name} calls={} total_candidates={} max_candidates={} total_ms={:.3} avg_candidates={:.1}",
            agg.calls,
            agg.total_candidates,
            agg.max_candidates,
            ms(agg.total_ns),
            avg_cand
        );
    }

    let mut bow_symbol_stats: Vec<(String, NameAgg)> = bow_stats_symbol()
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .collect();
    bow_symbol_stats.sort_unstable_by_key(|(_, agg)| std::cmp::Reverse(agg.total_ns));
    eprintln!(
        "TOP20_BOW_SYMBOL_TABLE_BY_TIME (name calls total_candidates max_candidates total_ms avg_candidates) -- bag-of-words global ref candidate scan (symbol_table.get(name).iter().find(..))"
    );
    for (name, agg) in bow_symbol_stats.iter().take(20) {
        let avg_cand = if agg.calls > 0 {
            agg.total_candidates as f64 / agg.calls as f64
        } else {
            0.0
        };
        eprintln!(
            "  {name} calls={} total_candidates={} max_candidates={} total_ms={:.3} avg_candidates={:.1}",
            agg.calls,
            agg.total_candidates,
            agg.max_candidates,
            ms(agg.total_ns),
            avg_cand
        );
    }
    eprintln!("SEM_PROFILE_RESOLVE report end ------------------------------");
}
