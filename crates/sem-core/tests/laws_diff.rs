//! Law witnesses — entity DIFF as a partial bijection (symmetric inverse
//! monoid) computed by a layered greedy matching, plus an orphan cover.
//!
//! STRUCTURE (see `laws_extraction_DOSSIER.md` for citations and status):
//!
//! `compute_semantic_diff(F, F')` computes a *partial injective matching*
//! between extract(F) and extract(F') — an element of the symmetric inverse
//! monoid I(Entities) (Wagner 1952 / Preston 1954): matched pairs become
//! Modified/Renamed/Moved/Reordered, the unmatched domain becomes Deleted,
//! the unmatched codomain becomes Added. Laws witnessed here:
//!
//!   D1 identity:    diff(F, F) = the identity partial bijection = no changes
//!   D2 soundness:   the entity cover + orphan cover is jointly epic — every
//!                   semantic byte change is attributed somewhere
//!   D3 locality:    diff over a set of files is the monoid sum of per-file
//!                   diffs (a homomorphism from the coproduct of files)
//!   D4 reorder:     a pure permutation of unchanged entities is reported as
//!                   the complement of a longest non-decreasing subsequence
//!                   (Schensted 1961; Ulam distance; "patience diff",
//!                   B. Cohen 2005) — Reordered only, never Added+Deleted
//!   D5 quotient:    structural_hash respects the formatting congruence:
//!                   comment/whitespace edits are Modified with
//!                   structural_change = false; literal edits = true
//!   D6 duality:     diff(F', F) is the inverse partial bijection —
//!                   Added and Deleted swap, Modified is self-dual
//!
//! The matcher is NOT a stable matching (Gale-Shapley 1962) and NOT an
//! optimal assignment (Kuhn 1955): phases 1-5 are a greedy layered heuristic.
//! The `characterize_*` tests pin the heuristic boundary; the `red_*` tests
//! (all `#[ignore]`, Miller-style headers) witness laws that FAIL on HEAD.
//!
//! Boundary: `sem_core::parser::differ::compute_semantic_diff` — the exact
//! entry point `sem diff` (crates/sem-cli/src/commands/diff/mod.rs:1720)
//! calls. Realization-agnostic: only DiffResult images are inspected.

mod laws_common;

use laws_common::*;
use proptest::prelude::*;
use proptest::test_runner::Config as PtConfig;
use sem_core::model::change::ChangeType;

fn cfg(cases: u32) -> PtConfig {
    PtConfig {
        cases,
        failure_persistence: None,
        ..PtConfig::default()
    }
}

// ===========================================================================
// LAW D1 — Identity: diff(F, F) is observationally empty for every parseable
// F (TS, Python, Rust, JSON) and for fallback-chunked text.
// ===========================================================================
proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_d1_diff_identity(p in program_strategy(true, 1, 6)) {
        let src = render_program(&p);
        let result = diff(&[modified_file(p.lang.path(), &src, &src)]);
        check_diff_identity(&result).map_err(TestCaseError::fail)?;
    }
}

proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_d1_diff_identity_fallback(lines in plain_lines_strategy(40)) {
        let src = lines.join("\n");
        let result = diff(&[modified_file("gen/notes.qqq", &src, &src)]);
        check_diff_identity(&result).map_err(TestCaseError::fail)?;
    }
}

// ===========================================================================
// LAW D2 — Soundness / attribution: a semantic edit (a body literal, a
// preamble line, a JSON value) always produces a non-empty diff, attributed
// to the edited entity or to the orphan mechanism.
// ===========================================================================
#[derive(Debug, Clone, Copy)]
enum Edit {
    Body(usize),
    Preamble,
}

proptest! {
    #![proptest_config(cfg(64))]
    #[test]
    fn law_d2_soundness_code_edits(
        (p, edit) in program_strategy(false, 1, 5)
            .prop_map(|mut p| { p.preamble = true; p })
            .prop_flat_map(|p| {
                let n = p.rets.len();
                let edit = prop_oneof![
                    (0..n).prop_map(Edit::Body),
                    Just(Edit::Preamble),
                ];
                (Just(p), edit)
            }),
    ) {
        let before = render_program(&p);
        let after = match edit {
            Edit::Body(i) => {
                let mut p2 = p.clone();
                p2.rets[i] += 1;
                render_program(&p2)
            }
            Edit::Preamble => before.replacen(
                preamble_text(p.lang),
                preamble_text_alt(p.lang),
                1,
            ),
        };
        prop_assert_ne!(&before, &after, "edit must change bytes");

        let result = diff(&[modified_file(p.lang.path(), &before, &after)]);
        prop_assert!(
            !result.changes.is_empty(),
            "soundness violated: byte change produced empty diff ({:?})", edit
        );

        match edit {
            Edit::Body(i) => {
                let name = format!("f{i}");
                prop_assert!(
                    result.changes.iter().any(|c| c.entity_name == name
                        && c.change_type == ChangeType::Modified),
                    "attribution: edit to {name} not reported as Modified: {:?}",
                    result.changes.iter().map(|c| (&c.entity_name, c.change_type)).collect::<Vec<_>>()
                );
            }
            Edit::Preamble => {
                prop_assert!(
                    result.changes.iter().any(|c| c.entity_type == "orphan"),
                    "attribution: preamble edit not captured by the orphan mechanism"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_d2_soundness_json_edits(
        (rets, idx) in prop::collection::vec(0u32..90, 1..6)
            .prop_flat_map(|rets| {
                let n = rets.len();
                (Just(rets), 0..n)
            }),
    ) {
        let p = Program { lang: Lang::Json, rets: rets.clone(), preamble: false, with_class: false };
        let before = render_program(&p);
        let mut p2 = p.clone();
        p2.rets[idx] += 1;
        let after = render_program(&p2);
        prop_assert_ne!(&before, &after);

        let result = diff(&[modified_file(p.lang.path(), &before, &after)]);
        let name = format!("k{idx}");
        prop_assert!(
            result.changes.iter().any(|c| c.entity_name == name),
            "attribution: JSON value edit to {name} invisible: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, c.change_type)).collect::<Vec<_>>()
        );
    }
}

// ===========================================================================
// LAW D3 — Per-file locality (monoid homomorphism): diffing two files in one
// call equals the sum of diffing each alone — same change multiset, additive
// counters. Files are a coproduct; diff distributes over it.
// ===========================================================================
proptest! {
    #![proptest_config(cfg(40))]
    #[test]
    fn law_d3_per_file_locality(
        pa in program_strategy(false, 1, 4),
        pb in program_strategy(false, 1, 4),
    ) {
        let before_a = render_program(&pa);
        let mut pa2 = pa.clone();
        pa2.rets[0] += 1;
        let after_a = render_program(&pa2);

        let before_b = render_program(&pb);
        let mut pb2 = pb.clone();
        pb2.rets[0] += 1;
        let after_b = render_program(&pb2);

        let fa = modified_file(pa.lang.path(), &before_a, &after_a);
        let fb = modified_file(pb.lang.path_b(), &before_b, &after_b);

        let combined = diff(&[fa.clone(), fb.clone()]);
        let alone_a = diff(&[fa]);
        let alone_b = diff(&[fb]);

        // Non-vacuity: both files actually changed.
        prop_assert!(!alone_a.changes.is_empty() && !alone_b.changes.is_empty());

        let key = |r: &sem_core::parser::differ::DiffResult| {
            let mut v: Vec<(String, String, String)> = r
                .changes
                .iter()
                .map(|c| (c.file_path.clone(), c.entity_id.clone(), c.change_type.to_string()))
                .collect();
            v.sort();
            v
        };
        let mut sum = key(&alone_a);
        sum.extend(key(&alone_b));
        sum.sort();
        prop_assert_eq!(key(&combined), sum, "diff is not a per-file monoid sum");
        prop_assert_eq!(combined.file_count, alone_a.file_count + alone_b.file_count);
        prop_assert_eq!(combined.modified_count, alone_a.modified_count + alone_b.modified_count);
        prop_assert_eq!(combined.orphan_count, alone_a.orphan_count + alone_b.orphan_count);
        prop_assert_eq!(
            combined.total_entities_before,
            alone_a.total_entities_before + alone_b.total_entities_before
        );
        prop_assert_eq!(
            combined.total_entities_after,
            alone_a.total_entities_after + alone_b.total_entities_after
        );
    }
}

// ===========================================================================
// LAW D4 — Permutation: reordering unchanged entities yields Reordered
// changes only (the complement of a longest non-decreasing subsequence),
// never Added + Deleted, never Modified. |changes| is between 1 and n-1 for
// a non-identity permutation.
// ===========================================================================
proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_d4_permutation_is_reordered_only(
        (p, order) in program_strategy(false, 2, 6)
            .prop_flat_map(|p| {
                let n = p.rets.len();
                let idx: Vec<usize> = (0..n).collect();
                (Just(p), Just(idx).prop_shuffle())
            })
            .prop_filter("non-identity permutation", |(p, order)| {
                order.iter().enumerate().any(|(i, &o)| i != o) && {
                    let _ = p;
                    true
                }
            }),
    ) {
        let before = render_program(&p);
        let after = render_program_ordered(&p, Some(&order));
        prop_assert_ne!(&before, &after, "non-vacuity: permutation must move bytes");

        let result = diff(&[modified_file(p.lang.path(), &before, &after)]);

        prop_assert_eq!(result.added_count, 0, "{:?}", &result.changes);
        prop_assert_eq!(result.deleted_count, 0, "{:?}", &result.changes);
        prop_assert_eq!(result.modified_count, 0, "{:?}", &result.changes);
        prop_assert_eq!(result.renamed_count, 0, "{:?}", &result.changes);
        prop_assert_eq!(result.moved_count, 0, "{:?}", &result.changes);
        prop_assert!(
            result.changes.iter().all(|c| c.change_type == ChangeType::Reordered
                || c.entity_type == "orphan"),
            "permutation produced non-Reordered entity changes: {:?}",
            &result.changes
        );
        let reordered = result.reordered_count;
        prop_assert!(
            reordered >= 1 && reordered <= p.rets.len() - 1,
            "Ulam-distance bound violated: {reordered} reorders for n={}",
            p.rets.len()
        );
    }
}

#[test]
fn law_d4_identity_permutation_is_empty() {
    let p = Program {
        lang: Lang::Py,
        rets: vec![1, 2, 3],
        preamble: false,
        with_class: false,
    };
    let src = render_program(&p);
    let order = [0usize, 1, 2];
    let same = render_program_ordered(&p, Some(&order));
    assert_eq!(src, same);
    let result = diff(&[modified_file(p.lang.path(), &src, &same)]);
    assert!(result.changes.is_empty());
}

// ===========================================================================
// LAW D5 — Formatting quotient: structural_hash respects the congruence
// generated by comments/whitespace. A comment-only body edit is Modified
// with structural_change = Some(false); a literal edit is Some(true).
// ===========================================================================
#[derive(Debug, Clone, Copy)]
enum QuotientEdit {
    Comment,
    Literal,
}

proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_d5_formatting_quotient(
        (p, target, kind) in program_strategy(false, 1, 5)
            .prop_map(|mut p| { p.with_class = false; p })
            .prop_flat_map(|p| {
                let n = p.rets.len();
                (
                    Just(p),
                    0..n,
                    prop_oneof![Just(QuotientEdit::Comment), Just(QuotientEdit::Literal)],
                )
            }),
    ) {
        let before = render_program(&p);
        let target_fn = render_fn(p.lang, target, p.rets[target]);
        let replacement = match kind {
            QuotientEdit::Comment => render_fn_commented(p.lang, target, p.rets[target]),
            QuotientEdit::Literal => render_fn(p.lang, target, p.rets[target] + 1),
        };
        let after = before.replacen(&target_fn, &replacement, 1);
        prop_assert_ne!(&before, &after, "edit must change bytes");

        let result = diff(&[modified_file(p.lang.path(), &before, &after)]);
        let name = format!("f{target}");
        let change = result
            .changes
            .iter()
            .find(|c| c.entity_name == name && c.change_type == ChangeType::Modified);
        prop_assert!(change.is_some(), "edited entity not Modified: {:?}", &result.changes);
        let expected = match kind {
            QuotientEdit::Comment => Some(false),
            QuotientEdit::Literal => Some(true),
        };
        prop_assert_eq!(
            change.unwrap().structural_change,
            expected,
            "structural_hash does not respect the formatting congruence ({:?})", kind
        );
    }
}

// ===========================================================================
// LAW D6 — Duality: diff(F', F) is the inverse partial bijection of
// diff(F, F'): Added and Deleted counts swap, Modified is self-dual, and the
// total number of changes is preserved.
// ===========================================================================
#[derive(Debug, Clone, Copy)]
enum SymEdit {
    Add,
    Remove(usize),
    Body(usize),
}

proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_d6_duality(
        (p, edit) in program_strategy(false, 1, 5)
            .prop_map(|mut p| { p.with_class = false; p.preamble = false; p })
            .prop_flat_map(|p| {
                let n = p.rets.len();
                let edit = prop_oneof![
                    Just(SymEdit::Add),
                    (0..n).prop_map(SymEdit::Remove),
                    (0..n).prop_map(SymEdit::Body),
                ];
                (Just(p), edit)
            }),
    ) {
        let before = render_program(&p);
        let after = match edit {
            SymEdit::Add => {
                let mut out = before.clone();
                out.push_str(&render_fn(p.lang, p.rets.len(), 42));
                out
            }
            SymEdit::Remove(i) => {
                before.replacen(&render_fn(p.lang, i, p.rets[i]), "", 1)
            }
            SymEdit::Body(i) => {
                let mut p2 = p.clone();
                p2.rets[i] += 1;
                render_program(&p2)
            }
        };
        prop_assert_ne!(&before, &after);

        let fwd = diff(&[modified_file(p.lang.path(), &before, &after)]);
        let bwd = diff(&[modified_file(p.lang.path(), &after, &before)]);

        prop_assert!(!fwd.changes.is_empty(), "non-vacuity ({:?})", edit);
        prop_assert_eq!(fwd.added_count, bwd.deleted_count, "Added/Deleted not dual");
        prop_assert_eq!(fwd.deleted_count, bwd.added_count, "Deleted/Added not dual");
        prop_assert_eq!(fwd.modified_count, bwd.modified_count, "Modified not self-dual");
        prop_assert_eq!(fwd.changes.len(), bwd.changes.len(), "change count not preserved");
    }
}

// ===========================================================================
// CHARACTERIZATIONS (GREEN) — the matcher's heuristic boundary, pinned.
// These document current behavior at the exact point where the algorithm is
// NOT a stable matching / optimal assignment; they are expected to pass and
// exist so any future change to the boundary is a conscious one.
// ===========================================================================

/// Phase-5 fuzzy matching (Jaccard >= 0.8, grouped by entity_type only) has
/// NO locality prior: deleting a field from interface Beta while adding a
/// textually identical—but unrelated—field to interface Alpha is reported as
/// a single Moved change (old_parent = Beta), not Added + Deleted. This is
/// the practitioner-reported "new interface fields matched against unrelated
/// interfaces" artifact. Content is preserved on the Moved change, so the
/// bytes remain auditable; the CLASSIFICATION is the artifact. Its
/// load-bearing blast radius is witnessed by
/// `red_false_move_must_not_suppress_parent_declaration_change` below.
#[test]
fn characterize_cross_container_identical_text_is_moved() {
    let before = "interface Alpha {\n  createdAt: Date;\n}\n\ninterface Beta {\n  id: string;\n  label: string;\n}\n";
    let after = "interface Alpha {\n  id: string;\n  createdAt: Date;\n}\n\ninterface Beta {\n  label: string;\n}\n";
    let result = diff(&[modified_file("x.ts", before, after)]);

    let moved: Vec<_> = result
        .changes
        .iter()
        .filter(|c| c.change_type == ChangeType::Moved)
        .collect();
    assert_eq!(moved.len(), 1, "{:?}", result.changes);
    assert_eq!(moved[0].entity_id, "x.ts::interface::Alpha::id");
    assert_eq!(
        moved[0].old_parent_id.as_deref(),
        Some("x.ts::interface::Beta"),
        "heuristic boundary moved: cross-container fuzzy match no longer pairs these"
    );
    assert_eq!(result.added_count, 0, "{:?}", result.changes);
    assert_eq!(result.deleted_count, 0, "{:?}", result.changes);
}

/// Phase-4 cross-file signature matching has NO minimum-similarity
/// threshold (best_score starts at -inf and any candidate wins): a file
/// rename plus a COMPLETE rewrite of a same-named function is still Moved,
/// never Deleted + Added. Content is carried, so nothing is lost — but
/// "Moved" here asserts identity continuity on name alone.
#[test]
fn characterize_cross_file_rename_zero_similarity_is_moved() {
    let before = "export function foo() { return alphaOne + betaTwo + gammaThree; }\n";
    let after = "export function foo() { completely(); different(); tokens(); }\n";
    let result = diff(&[renamed_file("old.ts", "new.ts", before, after)]);
    assert_eq!(result.changes.len(), 1, "{:?}", result.changes);
    assert_eq!(result.changes[0].change_type, ChangeType::Moved);
    assert_eq!(result.changes[0].old_file_path.as_deref(), Some("old.ts"));
}

/// The denotation's declared kernel: blank-line-only edits BETWEEN entities
/// are invisible (orphan segments that trim to empty are dropped). diff's
/// domain is files modulo this congruence; D2 soundness is stated over
/// non-whitespace edits for exactly this reason.
#[test]
fn characterize_blank_line_insertion_outside_entities_is_invisible() {
    let before = "def a():\n    return 1\n\ndef b():\n    return 2\n";
    let after = "def a():\n    return 1\n\n\n\ndef b():\n    return 2\n";
    let result = diff(&[modified_file("q.py", before, after)]);
    assert!(
        result.changes.is_empty(),
        "kernel changed: blank-line-only edits are now visible: {:?}",
        result.changes
    );
}

// ===========================================================================
// RED LAWS — witnessed failures on HEAD (commit 260e2a1b, 2026-08-28).
// All #[ignore]d so the suite stays green; run with --ignored to see them
// fail. Read-only audit: no fixes here. Miller-style headers below.
// ===========================================================================

/// RED LAW / BUG REPORT (Miller-style)
///
/// LAW: Diff soundness — the entity cover + orphan cover must be jointly
///   epic: every byte-level change between parseable F and F' is attributed
///   to at least one entity change or orphan change.
/// VIOLATION: the orphan cover is built at LINE granularity
///   (`detect_orphan_changes` marks every line in `start_line..=end_line` of
///   any entity as "covered") while entity content is BYTE-granular
///   (`start_byte..end_byte`). Bytes on a covered line but outside every
///   entity's byte span — e.g. a trailing comment on the entity's closing
///   line — belong to no entity's content and to no orphan segment. An edit
///   there produces diff(F, F') = empty while F != F'.
/// WITNESS: `fn foo() { let x = 1; } // old note` -> `... // new note`
///   yields 0 changes on HEAD.
/// BLAST RADIUS: any consumer trusting `sem diff` for completeness (review
///   gates, change accounting, cache invalidation keyed on "no entity
///   changes") silently misses same-line trailing edits. Load-bearing, not
///   cosmetic.
/// MECHANISM: crates/sem-core/src/parser/differ.rs, `detect_orphan_changes`
///   — `before_covered`/`after_covered` are HashSet<usize> of line numbers.
/// DIRECTION (for the fix owner, not applied here): compute orphan segments
///   over the byte-complement of entity spans, or additionally compare the
///   uncovered residue of boundary lines.
#[test]
#[ignore = "RED on HEAD (260e2a1b): same-line trailing edit outside entity byte span is invisible to diff"]
fn red_law_soundness_same_line_residue_is_attributed() {
    let before = "fn foo() { let x = 1; } // old note\n";
    let after = "fn foo() { let x = 1; } // new note\n";
    assert_ne!(before, after);
    let result = diff(&[modified_file("a.rs", before, after)]);
    assert!(
        !result.changes.is_empty(),
        "soundness violated: byte change on the entity's closing line, outside \
         its byte span, produced an empty diff"
    );
}

/// RED LAW / BUG REPORT (Miller-style)
///
/// LAW: Diff determinism — compute_semantic_diff is a function: identical
///   inputs yield identical DiffResult, including change ORDER, across
///   repeated calls and across processes.
/// VIOLATION: Phase 1 of `match_entities` iterates `after_by_id`, a
///   std HashMap whose RandomState draws a fresh key per instance; changes
///   are pushed in that iteration order. The final `sort_by_key(entity_line)`
///   (differ.rs:172) is not a total order: entities sharing a start line
///   keep their pre-sort (random) relative order.
/// WITNESS: three one-line functions on the same line, all modified — 6
///   distinct output orders observed in 30 in-process runs on HEAD.
/// BLAST RADIUS: byte-for-byte diff output is not reproducible (golden
///   files, CI gates, caches keyed on serialized DiffResult); same mechanism
///   class as the disclosed metadata_json HashMap finding in
///   model/entity.rs. Counts and change SETS are stable; only order flaps.
/// MECHANISM: crates/sem-core/src/model/identity.rs:227 (`for (&id, ...) in
///   &after_by_id`), crates/sem-core/src/parser/differ.rs:172 (non-total
///   sort key).
/// DIRECTION: iterate `after` (a slice, deterministic) in Phase 1, or make
///   the final sort key total, e.g. (entity_line, entity_id).
#[test]
#[ignore = "RED on HEAD (260e2a1b): same-line ties ordered by HashMap RandomState; output order not a function of input"]
fn red_law_diff_output_order_is_deterministic() {
    let before = "function a(){return 1} function b(){return 2} function c(){return 3}\n";
    let after = "function a(){return 10} function b(){return 20} function c(){return 30}\n";
    let mut orders = std::collections::HashSet::new();
    for _ in 0..30 {
        let result = diff(&[modified_file("t.ts", before, after)]);
        let order: Vec<String> = result.changes.iter().map(|c| c.entity_id.clone()).collect();
        orders.insert(order);
    }
    assert_eq!(
        orders.len(),
        1,
        "determinism violated: {} distinct change orders across 30 identical runs",
        orders.len()
    );
}

/// RED LAW / BUG REPORT (Miller-style)
///
/// LAW: Diff soundness under suppression — parent-change suppression may
///   only drop a container change that is fully explained by reported child
///   changes; a container whose OWN declaration changed must stay visible.
/// VIOLATION: `suppress_redundant_parents` (differ.rs:352-360) suppresses
///   the old parent of any Moved child UNCONDITIONALLY. Combined with the
///   phase-5 no-locality fuzzy match (see
///   `characterize_cross_container_identical_text_is_moved`), a false Moved
///   pairing suppresses the unrelated old container's Modified change even
///   when that container's own declaration changed. The declaration edit
///   (`extends Base` removed) then appears NOWHERE in the diff.
/// WITNESS: Beta loses `extends Base` AND field `id`; Alpha gains an
///   unrelated identical-text `id`. HEAD reports [Alpha Modified,
///   Alpha::id Moved-from-Beta] — Beta's declaration change is invisible.
/// BLAST RADIUS: load-bearing, not display-only: a semantic interface
///   contract change (inheritance removed) is dropped from the change
///   record. The cross-entity false-positive is therefore NOT merely
///   cosmetic — it composes with suppression into a soundness hole.
/// MECHANISM: identity.rs phase 5 (no parent/name constraint at >= 0.8
///   Jaccard) x differ.rs Moved-old-parent suppression (unconditional).
/// DIRECTION: make the Moved-parent suppression conditional on the parent's
///   own stripped declaration being unchanged (the same test the Modified
///   suppression already applies).
#[test]
#[ignore = "RED on HEAD (260e2a1b): false cross-container Move suppresses the old parent's own declaration change"]
fn red_law_false_move_must_not_suppress_parent_declaration_change() {
    let before = "interface Alpha {\n  createdAt: Date;\n}\n\ninterface Beta extends Base {\n  id: string;\n  label: string;\n}\n";
    let after = "interface Alpha {\n  id: string;\n  createdAt: Date;\n}\n\ninterface Beta {\n  label: string;\n}\n";
    let result = diff(&[modified_file("x.ts", before, after)]);
    let beta_visible = result
        .changes
        .iter()
        .any(|c| c.entity_id == "x.ts::interface::Beta");
    assert!(
        beta_visible,
        "Beta's own declaration changed (lost `extends Base`) but no change \
         mentions Beta: {:?}",
        result
            .changes
            .iter()
            .map(|c| (c.change_type, c.entity_id.clone()))
            .collect::<Vec<_>>()
    );
}

// ===========================================================================
// Positive controls
// ===========================================================================

#[test]
fn control_identity_checker_rejects_real_change() {
    let before = "def a():\n    return 1\n";
    let after = "def a():\n    return 2\n";
    let result = diff(&[modified_file("c.py", before, after)]);
    assert!(
        check_diff_identity(&result).is_err(),
        "positive control failed: identity checker accepted a non-empty diff"
    );
}

#[test]
fn control_reorder_law_detects_add_delete_leak() {
    // A rename (not a pure permutation) MUST violate the D4 assertions —
    // proving the D4 property is not vacuously satisfiable by any diff.
    let before = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
    let after = "def alpha():\n    return 1\n\ndef totally_new():\n    return 99\n";
    let result = diff(&[modified_file("r.py", before, after)]);
    let d4_would_pass = result.added_count == 0
        && result.deleted_count == 0
        && result.modified_count == 0
        && result.renamed_count == 0;
    assert!(
        !d4_would_pass,
        "positive control failed: D4 assertions cannot distinguish a rewrite \
         from a permutation"
    );
}
