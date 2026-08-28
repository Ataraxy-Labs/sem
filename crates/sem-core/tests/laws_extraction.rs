//! Law witnesses — entity EXTRACTION as a lens family over the free monoid
//! of source bytes.
//!
//! STRUCTURE (see `laws_extraction_DOSSIER.md` for citations and status):
//!
//! Extraction `extract : File -> [Entity]` equips each entity with a byte
//! span, i.e. a *very well-behaved lens* per entity (Foster et al. 2007,
//! "Combinators for bidirectional tree transformations", TOPLAS 29(3)):
//!   get(F)      = F[s..t]                    (the entity's content)
//!   put(F, c')  = F[..s] ++ c' ++ F[t..]     (byte-range splice)
//! with the lens laws
//!   GetPut: put(F, get(F)) = F                       (round-trip)
//!   PutGet: get*(put(F, c')) = c'                    (re-extraction sees c')
//! and the FRAME condition (Reynolds 2002, separation logic; O'Hearn's frame
//! rule): a put into entity e's footprint leaves every *disjoint* entity's
//! identity and content unchanged. Together these are what make entity-level
//! editing sound.
//!
//! Extraction is also a *factorization in the free monoid* (Sigma*, ++, e):
//! the file is preamble ++ entity_1 ++ gap ++ entity_2 ++ ..., and the
//! fallback plugin realizes the total (chunking) factorization for inputs
//! outside the parser's domain — partiality made honest.
//!
//! Entity IDs are paths in the containment forest (the free category on the
//! parent-child graph); `disambiguate_colliding_entity_ids` restores
//! faithfulness (injectivity) of the naming functor when names collide.
//!
//! All properties run at the public boundary `ParserRegistry::extract_entities`
//! and compare images (Hoare 1972: proofs relate the abstraction, not the
//! representation). No internals are read.

mod laws_common;

use laws_common::*;
use proptest::prelude::*;
use proptest::test_runner::Config as PtConfig;

fn cfg(cases: u32) -> PtConfig {
    PtConfig {
        cases,
        failure_persistence: None,
        ..PtConfig::default()
    }
}

// ===========================================================================
// LAW E1 + E2 — Lens section/retraction: get is a slice, GetPut is identity.
//   For every span-bearing entity e in extract(F):
//     F[e.start_byte .. e.end_byte] == e.content            (get = slice)
//     splice(F, e.span, e.content) == F   (byte-identical)  (GetPut)
// ===========================================================================
proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_e1_e2_lens_get_slice_agreement_and_getput(
        p in program_strategy(true, 1, 6),
    ) {
        let src = render_program(&p);
        let entities = extract(p.lang.path(), &src);

        // Non-vacuity floor: every generated function must surface as an
        // entity, and at least that many entities carry byte spans.
        prop_assert!(
            entities.len() >= p.rets.len(),
            "conservation floor: {} generated items but only {} entities",
            p.rets.len(),
            entities.len()
        );

        let with_spans = check_slice_agreement(&src, &entities)
            .map_err(|e| TestCaseError::fail(e))?;
        prop_assert!(
            with_spans >= p.rets.len(),
            "non-vacuity: only {with_spans} span-bearing entities for {} items",
            p.rets.len()
        );

        // GetPut: splicing an entity's own content back over its span is the
        // identity on the file, byte for byte.
        for e in &entities {
            if let (Some(s), Some(t)) = (e.start_byte, e.end_byte) {
                let respliced = splice(&src, s, t, &e.content);
                prop_assert_eq!(
                    &respliced, &src,
                    "GetPut violated for entity {}", &e.id
                );
            }
        }
    }
}

// ===========================================================================
// LAW E3 — PutGet + FRAME: replacing one entity's content with a well-formed
// variant changes exactly that entity; every other entity's identity, type,
// name, content and line span are unchanged (separation-logic frame rule
// over disjoint footprints).
// ===========================================================================
proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_e3_putget_and_frame(
        (p, target) in program_strategy(false, 2, 6)
            .prop_map(|mut p| { p.with_class = false; p })
            .prop_flat_map(|p| {
                let n = p.rets.len();
                (Just(p), 0..n)
            }),
    ) {
        let src = render_program(&p);
        let entities = extract(p.lang.path(), &src);
        let target_name = format!("f{target}");

        let e = entities
            .iter()
            .find(|e| e.name == target_name)
            .expect("generated function must be extracted");
        let (s, t) = (e.start_byte.unwrap(), e.end_byte.unwrap());

        // A well-formed variant of the same entity: bump the body literal.
        let ret = p.rets[target];
        let new_content = e.content.replacen(
            &body_marker(ret),
            &body_marker(ret + 1),
            1,
        );
        prop_assert_ne!(&new_content, &e.content, "edit must change bytes");

        let before_frame = frame_fingerprint(&entities, &target_name);

        let src2 = splice(&src, s, t, &new_content);
        let entities2 = extract(p.lang.path(), &src2);

        // PutGet: the re-extracted target sees exactly the spliced content.
        let e2 = entities2
            .iter()
            .find(|e| e.name == target_name)
            .expect("target entity must survive the splice");
        prop_assert_eq!(&e2.content, &new_content, "PutGet violated");
        prop_assert_eq!(&e2.id, &e.id, "target identity must be stable under put");

        // FRAME: everything outside the footprint is untouched.
        let after_frame = frame_fingerprint(&entities2, &target_name);
        prop_assert_eq!(
            before_frame, after_frame,
            "frame violated: entities outside the edited span changed"
        );
    }
}

// ===========================================================================
// LAW E4 — Extraction is a function (determinism): repeated extraction, with
// independently constructed registries, yields identical results (identical
// serialized images — order, hashes, metadata, everything observable).
// ===========================================================================
proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_e4_extraction_deterministic(
        p in program_strategy(true, 1, 6),
    ) {
        let src = render_program(&p);
        let a = extract(p.lang.path(), &src);
        let b = extract(p.lang.path(), &src); // fresh registry inside
        prop_assert!(!a.is_empty(), "non-vacuity: extraction must be non-empty");
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        prop_assert_eq!(ja, jb, "extraction is not a function of its input");
    }
}

// ===========================================================================
// LAW E5 — Faithful naming: entity IDs are unique even under deliberate
// name collisions (Python allows textual redefinition; the disambiguator
// must restore injectivity, never panic, never drop entities).
// ===========================================================================
proptest! {
    #![proptest_config(cfg(48))]
    #[test]
    fn law_e5_entity_ids_unique_under_collisions(
        rets in prop::collection::vec(0u32..90, 1..5),
        dup_rets in prop::collection::vec(0u32..90, 1..4),
    ) {
        // n distinct functions, then dup_rets.len() extra definitions all
        // named f0 — forced ID collisions.
        let mut src = String::new();
        for (i, r) in rets.iter().enumerate() {
            src.push_str(&render_fn(Lang::Py, i, *r));
        }
        for r in &dup_rets {
            src.push_str(&render_fn(Lang::Py, 0, *r));
        }

        let entities = extract("gen/dup.py", &src);

        // Non-vacuity: the colliding definitions are all extracted.
        let f0_count = entities.iter().filter(|e| e.name == "f0").count();
        prop_assert_eq!(
            f0_count,
            1 + dup_rets.len(),
            "collision probe: expected all duplicate definitions extracted"
        );

        // Faithfulness: IDs are pairwise distinct.
        let mut ids: Vec<&str> = entities.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        prop_assert_eq!(before, ids.len(), "duplicate entity IDs after disambiguation");
    }
}

// ===========================================================================
// LAW E6 — Partiality honesty: an unknown-extension file goes to the
// declared line-chunking fallback. Chunks are labeled "chunk" (never fake
// semantic entities), their line spans partition the file in order, and
// their contents reassemble the input — a total factorization in the free
// monoid of lines.
// ===========================================================================
proptest! {
    #![proptest_config(cfg(64))]
    #[test]
    fn law_e6_fallback_total_line_cover(
        lines in plain_lines_strategy(60),
    ) {
        let src = lines.join("\n");
        let entities = extract("gen/data.qqq", &src);
        check_fallback_cover(&lines, &entities).map_err(TestCaseError::fail)?;
    }
}

// ===========================================================================
// Positive controls — every checker must go RED on a deliberately broken
// artifact; a checker that cannot fail witnesses nothing.
// ===========================================================================

#[test]
fn control_slice_checker_rejects_shifted_span() {
    let p = Program {
        lang: Lang::Rs,
        rets: vec![3, 4],
        preamble: true,
        with_class: false,
    };
    let src = render_program(&p);
    let mut entities = extract(p.lang.path(), &src);
    let e = entities
        .iter_mut()
        .find(|e| e.start_byte.is_some())
        .expect("span-bearing entity");
    *e.start_byte.as_mut().unwrap() += 1; // corrupt the lens
    assert!(
        check_slice_agreement(&src, &entities).is_err(),
        "positive control failed: corrupted span not detected"
    );
}

#[test]
fn control_frame_fingerprint_detects_mutation() {
    let p = Program {
        lang: Lang::Py,
        rets: vec![1, 2, 3],
        preamble: false,
        with_class: false,
    };
    let src = render_program(&p);
    let entities = extract(p.lang.path(), &src);
    let a = frame_fingerprint(&entities, "f0");
    let mut mutated = entities.clone();
    mutated
        .iter_mut()
        .find(|e| e.name == "f1")
        .unwrap()
        .content
        .push('X');
    let b = frame_fingerprint(&mutated, "f0");
    assert_ne!(a, b, "positive control failed: frame fingerprint is blind");
}

#[test]
fn control_fallback_checker_rejects_dropped_line() {
    let lines: Vec<String> = (0..25).map(|i| format!("line {i}")).collect();
    let src = lines.join("\n");
    let entities = extract("gen/data.qqq", &src);
    // Claim one line fewer than the input has: the cover check must fail.
    let mut short = lines.clone();
    short.push("phantom".to_string());
    assert!(
        check_fallback_cover(&short, &entities).is_err(),
        "positive control failed: incomplete cover not detected"
    );
}
