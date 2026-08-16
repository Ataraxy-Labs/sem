//! The acceptance demo for kappa (κ), the semantic identity hash computed
//! additively alongside `structural_hash`. See `crates/sem-core/KAPPA.md` for
//! the spec these tests pin.
//!
//! Four v1 properties, each proven on real entities extracted through the
//! real `CodeParserPlugin` (not a toy hasher):
//!
//! (a) formatting-only changes (trailing commas, semicolon style, brace
//!     placement, indentation) leave kappa IDENTICAL while `structural_hash`
//!     CHANGES -- across TypeScript, Python, and Rust.
//! (b) real semantic changes (rename, changed literal, added param, changed
//!     body logic) CHANGE kappa.
//! (c) kappa is stable across repeated extraction runs.
//! (d) two entities with identical semantic content but different formatting
//!     in different files get the SAME kappa -- the cache-collapse property.
//!
//! v1.1 adds two more sections, both against real entities through the real
//! plugin:
//!
//! (e) the declaration-keyword discriminator fix (KAPPA.md v1.1 §1): `let`
//!     vs `const` (TS/JS, including the multi-declarator path) and Java's
//!     multi-modifier `public final` vs `private final` no longer collide,
//!     while formatting-invariance still holds for the same declaration
//!     forms. Sibling cases that were swept and found to need NO fix (Rust
//!     `mut`, Python `global`/`nonlocal`, Go `:=`/`var`/`const`) are pinned
//!     here too, as regression proof the v1.1 change didn't touch them.
//! (f) universality: formatting-invariance + semantic-sensitivity (rename or
//!     logic-change) pairs for JS, Go, Java, Ruby, C++, and Kotlin -- the
//!     grammar-core and long-tail languages beyond the original TS/Python/
//!     Rust set. Kotlin's `val`/`var` fix specifically (property_declaration
//!     entities aren't currently surfaced by `CodeParserPlugin` at all --
//!     `extract_name` only reads a `name` field, which Kotlin's grammar
//!     doesn't put directly on `property_declaration`; a separate,
//!     pre-existing gap, not touched here) is instead pinned directly
//!     against the real tree-sitter-kotlin-ng parser + `hash.rs`'s public
//!     `structural_and_semantic_hash` in `hash.rs`'s own test module.

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::plugin::SemanticParserPlugin;
use sem_core::parser::plugins::code::CodeParserPlugin;

fn extract(content: &str, file_path: &str) -> Vec<SemanticEntity> {
    CodeParserPlugin.extract_entities(content, file_path)
}

fn entity_by_name<'a>(entities: &'a [SemanticEntity], name: &str) -> &'a SemanticEntity {
    entities
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no entity named `{name}` among {entities:#?}"))
}

fn kappa_of<'a>(entities: &'a [SemanticEntity], name: &str) -> &'a str {
    entity_by_name(entities, name)
        .kappa
        .as_deref()
        .unwrap_or_else(|| panic!("entity `{name}` has no kappa"))
}

fn structural_hash_of<'a>(entities: &'a [SemanticEntity], name: &str) -> &'a str {
    entity_by_name(entities, name)
        .structural_hash
        .as_deref()
        .unwrap_or_else(|| panic!("entity `{name}` has no structural_hash"))
}

// ---------------------------------------------------------------------------
// (a) formatting-invariance: kappa unchanged, structural_hash changed
// ---------------------------------------------------------------------------

#[test]
fn typescript_formatting_only_change_leaves_kappa_identical() {
    let before = r#"
function add(a: number, b: number): number {
    return a + b;
}
"#;
    // Trailing comma in the param list + no semicolon after the return
    // expression (legal under ASI) + reformatted braces/indentation.
    let after = r#"
function add(
    a: number,
    b: number,
): number
{
    return a + b
}
"#;

    let before_entities = extract(before, "before.ts");
    let after_entities = extract(after, "after.ts");

    assert_eq!(
        kappa_of(&before_entities, "add"),
        kappa_of(&after_entities, "add"),
        "kappa must be identical across a formatting-only TS change"
    );
    assert_ne!(
        structural_hash_of(&before_entities, "add"),
        structural_hash_of(&after_entities, "add"),
        "structural_hash SHOULD change here: trailing comma and semicolon \
         presence are both leaf tokens it hashes, unlike kappa"
    );
}

#[test]
fn python_formatting_only_change_leaves_kappa_identical() {
    let before = r#"
def add(a, b):
    return a + b
"#;
    // Trailing comma in the param list, reflowed onto multiple lines, plus a
    // comment (comments are excluded from both hashes, but included here to
    // demonstrate the pair is otherwise a real reformat, not a no-op).
    let after = r#"
def add(
    a,
    b,
):
    # sum the two args
    return a + b
"#;

    let before_entities = extract(before, "before.py");
    let after_entities = extract(after, "after.py");

    assert_eq!(
        kappa_of(&before_entities, "add"),
        kappa_of(&after_entities, "add"),
        "kappa must be identical across a formatting-only Python change"
    );
    assert_ne!(
        structural_hash_of(&before_entities, "add"),
        structural_hash_of(&after_entities, "add"),
        "structural_hash SHOULD change here: the added trailing comma is a \
         leaf token it hashes, unlike kappa"
    );
}

#[test]
fn rust_formatting_only_change_leaves_kappa_identical() {
    let before = r#"
fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;
    // Trailing comma in the param list + reflowed signature + a comment.
    let after = r#"
fn add(
    a: i32,
    b: i32,
) -> i32 {
    // sum the two args
    a + b
}
"#;

    let before_entities = extract(before, "before.rs");
    let after_entities = extract(after, "after.rs");

    assert_eq!(
        kappa_of(&before_entities, "add"),
        kappa_of(&after_entities, "add"),
        "kappa must be identical across a formatting-only Rust change"
    );
    assert_ne!(
        structural_hash_of(&before_entities, "add"),
        structural_hash_of(&after_entities, "add"),
        "structural_hash SHOULD change here: the added trailing comma is a \
         leaf token it hashes, unlike kappa"
    );
}

// ---------------------------------------------------------------------------
// (b) real semantic changes change kappa
// ---------------------------------------------------------------------------

#[test]
fn typescript_rename_changes_kappa() {
    let original = "function add(a: number, b: number): number { return a + b; }";
    let renamed = "function sum(a: number, b: number): number { return a + b; }";

    let original_entities = extract(original, "a.ts");
    let renamed_entities = extract(renamed, "b.ts");

    assert_ne!(
        kappa_of(&original_entities, "add"),
        kappa_of(&renamed_entities, "sum"),
        "kappa must change on rename: unlike structural_hash, it includes \
         the name and is deliberately rename-sensitive"
    );
    // The unchanged half of the story: structural_hash stays the same on
    // rename, as it always has -- kappa is additive, not a replacement.
    assert_eq!(
        structural_hash_of(&original_entities, "add"),
        structural_hash_of(&renamed_entities, "sum"),
        "structural_hash must remain rename-invariant, unchanged by kappa"
    );
}

#[test]
fn typescript_changed_literal_changes_kappa() {
    let original = "function scale(x: number): number { return x * 2; }";
    let changed = "function scale(x: number): number { return x * 3; }";

    let original_entities = extract(original, "a.ts");
    let changed_entities = extract(changed, "b.ts");

    assert_ne!(
        kappa_of(&original_entities, "scale"),
        kappa_of(&changed_entities, "scale"),
        "kappa must change when a literal changes"
    );
}

#[test]
fn typescript_added_param_changes_kappa() {
    let original = "function add(a: number, b: number): number { return a + b; }";
    let with_param = "function add(a: number, b: number, c: number): number { return a + b; }";

    let original_entities = extract(original, "a.ts");
    let with_param_entities = extract(with_param, "b.ts");

    assert_ne!(
        kappa_of(&original_entities, "add"),
        kappa_of(&with_param_entities, "add"),
        "kappa must change when a parameter is added"
    );
}

#[test]
fn typescript_changed_body_logic_changes_kappa() {
    let original = "function add(a: number, b: number): number { return a + b; }";
    let changed_logic = "function add(a: number, b: number): number { return a - b; }";

    let original_entities = extract(original, "a.ts");
    let changed_entities = extract(changed_logic, "b.ts");

    assert_ne!(
        kappa_of(&original_entities, "add"),
        kappa_of(&changed_entities, "add"),
        "kappa must change when body logic changes (+ became -)"
    );
}

#[test]
fn python_and_rust_semantic_changes_also_change_kappa() {
    // Spot-check the other two languages so (b) isn't TS-only.
    let py_original = extract("def add(a, b):\n    return a + b\n", "a.py");
    let py_renamed = extract("def sum(a, b):\n    return a + b\n", "b.py");
    assert_ne!(
        kappa_of(&py_original, "add"),
        kappa_of(&py_renamed, "sum"),
        "Python: rename must change kappa"
    );

    let rs_original = extract("fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n", "a.rs");
    let rs_logic = extract("fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n", "b.rs");
    assert_ne!(
        kappa_of(&rs_original, "add"),
        kappa_of(&rs_logic, "add"),
        "Rust: changed body logic must change kappa"
    );
}

// ---------------------------------------------------------------------------
// (c) kappa is stable across repeated runs
// ---------------------------------------------------------------------------

#[test]
fn kappa_is_stable_across_repeated_runs() {
    let source = r#"
class Greeter {
    private name: string;
    constructor(name: string) {
        this.name = name;
    }
    greet(): string {
        return `Hello, ${this.name}!`;
    }
}
"#;

    let mut runs = Vec::new();
    for _ in 0..5 {
        let entities = extract(source, "greeter.ts");
        let greet = kappa_of(&entities, "greet").to_string();
        let constructor = kappa_of(&entities, "constructor").to_string();
        runs.push((greet, constructor));
    }

    for pair in runs.windows(2) {
        assert_eq!(pair[0], pair[1], "kappa must be identical across runs");
    }
}

// ---------------------------------------------------------------------------
// (d) cache-collapse: identical semantics + different formatting, different
// files -> same kappa
// ---------------------------------------------------------------------------

#[test]
fn cache_collapse_same_kappa_across_files_with_different_formatting() {
    // File A: compact style.
    let file_a = r#"
export function clamp(value: number, min: number, max: number): number {
    if (value < min) return min;
    if (value > max) return max;
    return value;
}
"#;
    // File B: same function, same name, same params, same logic -- but
    // reformatted (multi-line signature, trailing comma, brace-on-own-line,
    // no semicolons) as if a different author/formatter produced it.
    let file_b = r#"
export function clamp(
    value: number,
    min: number,
    max: number,
): number
{
    if (value < min) return min
    if (value > max) return max
    return value
}
"#;

    let entities_a = extract(file_a, "utils/math_a.ts");
    let entities_b = extract(file_b, "utils/math_b.ts");

    let kappa_a = kappa_of(&entities_a, "clamp");
    let kappa_b = kappa_of(&entities_b, "clamp");

    assert_eq!(
        kappa_a, kappa_b,
        "two files with semantically identical, differently-formatted \
         entities must collapse to the same kappa"
    );
    // Sanity: this is NOT trivially true of every hash on the entity --
    // content_hash (raw text) and the id (file-path-qualified) both differ,
    // which is exactly the point: kappa is the field that collapses them.
    assert_ne!(
        entity_by_name(&entities_a, "clamp").content_hash,
        entity_by_name(&entities_b, "clamp").content_hash,
        "sanity: content_hash should differ (different raw text)"
    );
    assert_ne!(
        entity_by_name(&entities_a, "clamp").id,
        entity_by_name(&entities_b, "clamp").id,
        "sanity: id should differ (different file paths)"
    );
}

#[test]
fn kappa_is_none_for_plugins_that_do_not_compute_it() {
    // A markdown fixture goes through a non-code plugin path; kappa should
    // stay `None` there rather than panicking or defaulting to garbage.
    // This directly exercises the serde compat story: an entity that never
    // sets kappa serializes without the field (skip_serializing_if).
    use sem_core::parser::plugins::markdown::MarkdownParserPlugin;
    let entities = MarkdownParserPlugin.extract_entities(
        "# Title\n\nSome paragraph text under the title.\n",
        "doc.md",
    );
    assert!(
        !entities.is_empty(),
        "expected at least one markdown entity"
    );
    for e in &entities {
        assert!(
            e.kappa.is_none(),
            "markdown entities should not have kappa computed in v1"
        );
    }

    let json = serde_json::to_string(&entities[0]).expect("serialize");
    assert!(
        !json.contains("\"kappa\""),
        "kappa must be omitted from serialized JSON when None (compat with \
         existing snapshots/consumers), got: {json}"
    );
}

// ---------------------------------------------------------------------------
// (e) v1.1: the declaration-keyword discriminator fix, plus the sibling-gap
// sweep's "no fix needed" cases pinned as regression proof.
// ---------------------------------------------------------------------------

#[test]
fn typescript_let_vs_const_differ() {
    let let_entities = extract("let x = 1;", "a.ts");
    let const_entities = extract("const x = 1;", "a.ts");
    assert_ne!(
        kappa_of(&let_entities, "x"),
        kappa_of(&const_entities, "x"),
        "v1.1: `let x = 1` and `const x = 1` must no longer collide -- both \
         are `lexical_declaration` nodes and the leading keyword is the only \
         thing that differs"
    );
}

#[test]
fn typescript_let_vs_const_differ_multi_declarator() {
    // The multi-declarator path (#149: `let a = 1, b = 2` -> one entity per
    // declarator) computes both hashes over just the `variable_declarator`
    // subtree, so the enclosing keyword needs the explicit fold-in fix in
    // `entity_extractor.rs::fold_declaration_keyword_into_kappa` -- this is
    // a distinct code path from the single-declarator case above and needs
    // its own proof.
    let let_entities = extract("let a = 1, b = 2;", "a.ts");
    let const_entities = extract("const a = 1, b = 2;", "a.ts");
    assert_ne!(
        kappa_of(&let_entities, "a"),
        kappa_of(&const_entities, "a"),
        "v1.1: multi-declarator `let a = 1, b = 2` vs `const a = 1, b = 2` \
         must differ for declarator `a`"
    );
    assert_ne!(
        kappa_of(&let_entities, "b"),
        kappa_of(&const_entities, "b"),
        "v1.1: multi-declarator `let a = 1, b = 2` vs `const a = 1, b = 2` \
         must differ for declarator `b`"
    );
}

#[test]
fn typescript_var_already_differed_from_let_and_const() {
    // `var` was never part of the bug: tree-sitter-typescript gives `var`
    // its own node kind (`variable_declaration`, distinct from
    // `lexical_declaration`), so the node-kind hash alone already
    // discriminates it -- regardless of whether the keyword leaf itself is
    // included. Pinned here so a future change can't silently break this.
    let var_entities = extract("var x = 1;", "a.ts");
    let let_entities = extract("let x = 1;", "a.ts");
    let const_entities = extract("const x = 1;", "a.ts");
    assert_ne!(kappa_of(&var_entities, "x"), kappa_of(&let_entities, "x"));
    assert_ne!(kappa_of(&var_entities, "x"), kappa_of(&const_entities, "x"));
}

#[test]
fn typescript_let_declaration_formatting_invariance_still_holds() {
    // The v1.1 fix must not turn kappa formatting-*sensitive* for the very
    // declaration form it now discriminates on keyword identity.
    let compact = extract("let x = 1;", "a.ts");
    let spaced = extract("let   x   =   1  ;", "b.ts");
    assert_eq!(
        kappa_of(&compact, "x"),
        kappa_of(&spaced, "x"),
        "extra whitespace around `let x = 1` must not change kappa"
    );
}

#[test]
fn typescript_method_modifiers_all_differ() {
    // Found by the corpus collision sweep, not the original per-language
    // sibling sweep: `kappa_stats` on the TypeScript-monster corpus
    // surfaced a real group merging `foo() {}` and `get foo() {}` under one
    // kappa (see KAPPA.md v1.1's collision analysis). Root cause is the
    // exact same shape as `let`/`const`: `method_definition` is the node
    // kind for a plain method, a getter, a setter, a static method, and an
    // async method alike -- `get`/`set`/`static`/`async` are anonymous,
    // non-sole-child leaves under it. Unlike `let`/`const`, these can
    // *stack* (`static async foo() {}` has both `static` and `async`),
    // which is why the v1.1 rule had to become position-independent, not
    // just "the first child", partway through this task.
    let plain = extract("class C { foo() {} }", "a.ts");
    let getter = extract("class C { get foo() {} }", "b.ts");
    let setter = extract("class C { set foo(v) {} }", "c.ts");
    let static_method = extract("class C { static foo() {} }", "d.ts");
    let async_method = extract("class C { async foo() {} }", "e.ts");
    let static_async = extract("class C { static async foo() {} }", "f.ts");

    let kappas = [
        ("plain", kappa_of(&plain, "foo")),
        ("getter", kappa_of(&getter, "foo")),
        ("setter", kappa_of(&setter, "foo")),
        ("static", kappa_of(&static_method, "foo")),
        ("async", kappa_of(&async_method, "foo")),
        ("static_async", kappa_of(&static_async, "foo")),
    ];
    for (i, (name_a, kappa_a)) in kappas.iter().enumerate() {
        for (name_b, kappa_b) in &kappas[i + 1..] {
            assert_ne!(
                kappa_a, kappa_b,
                "v1.1: `{name_a}` and `{name_b}` method variants of `foo` \
                 must not collide"
            );
        }
    }
}

#[test]
fn typescript_method_formatting_invariance_still_holds() {
    let compact = extract("class C { get foo() {} }", "a.ts");
    let spaced = extract("class C {\n  get   foo()   {\n  }\n}", "b.ts");
    assert_eq!(
        kappa_of(&compact, "foo"),
        kappa_of(&spaced, "foo"),
        "extra whitespace around a getter must not change kappa"
    );
}

#[test]
fn typescript_async_function_differs_from_plain_function() {
    // Same shape and discovery path as the method-modifier fix above, but
    // caught by a *different* `kappa_stats` sample: `async function fn1()
    // {}` merged with plain `function fn1() {}` under one kappa on the
    // TS-monster corpus. `function_declaration`'s `async` keyword is an
    // anonymous non-sole child, exactly like `method_definition`'s.
    let plain = extract("function fn1() { }", "a.ts");
    let async_fn = extract("async function fn1() { }", "b.ts");
    assert_ne!(
        kappa_of(&plain, "fn1"),
        kappa_of(&async_fn, "fn1"),
        "v1.1: `async function fn1() {{}}` must not collide with `function \
         fn1() {{}}`"
    );
}

#[test]
fn typescript_async_arrow_and_generator_variants_all_differ() {
    // Sweeping every other "keyword modifies a callable" node kind the
    // async-function finding above prompted checking: function
    // expressions, arrow functions, and (async) generators all have the
    // same anonymous-`async`-child shape.
    let arrow = extract("const f = () => {};", "a.ts");
    let async_arrow = extract("const f = async () => {};", "b.ts");
    assert_ne!(
        kappa_of(&arrow, "f"),
        kappa_of(&async_arrow, "f"),
        "arrow fn"
    );

    let func_expr = extract("const f = function () {};", "c.ts");
    let async_func_expr = extract("const f = async function () {};", "d.ts");
    assert_ne!(
        kappa_of(&func_expr, "f"),
        kappa_of(&async_func_expr, "f"),
        "function expression"
    );

    let gen_decl = extract("function* fn1() { }", "e.ts");
    let async_gen_decl = extract("async function* fn1() { }", "f.ts");
    assert_ne!(
        kappa_of(&gen_decl, "fn1"),
        kappa_of(&async_gen_decl, "fn1"),
        "generator function declaration"
    );

    let gen_expr = extract("const f = function* () {};", "g.ts");
    let async_gen_expr = extract("const f = async function* () {};", "h.ts");
    assert_ne!(
        kappa_of(&gen_expr, "f"),
        kappa_of(&async_gen_expr, "f"),
        "generator function expression"
    );
}

#[test]
fn typescript_interface_method_signature_getter_differs_from_plain() {
    // `method_signature` is `method_definition`'s interface-context
    // sibling: `get foo(): number;` vs `foo(): number;` inside an
    // `interface` share the same anonymous-`get`-child shape.
    let plain = extract("interface I { foo(): number; }", "a.ts");
    let getter = extract("interface I { get foo(): number; }", "b.ts");
    assert_ne!(
        kappa_of(&plain, "foo"),
        kappa_of(&getter, "foo"),
        "v1.1: interface method signature `get foo(): number` must not \
         collide with `foo(): number`"
    );
}

#[test]
fn typescript_export_default_as_namespace_and_eq_all_differ() {
    // Found by a later `kappa_stats` sample: `export_statement` is the node
    // kind for `export default Foo;`, `export as namespace Foo;`, and
    // `export = foo;` alike -- `default`/`as`+`namespace`/`=` are anonymous
    // non-sole children (same shape, same discovery loop as the method-
    // modifier and readonly-property fixes above).
    let default_export = extract("export default Foo;", "a.ts");
    let namespace_export = extract("export as namespace Foo;", "b.ts");
    let eq_export = extract("export = Foo;", "c.ts");
    assert_ne!(
        kappa_of(&default_export, "Foo"),
        kappa_of(&namespace_export, "Foo"),
        "v1.1: `export default Foo` must not collide with `export as \
         namespace Foo`"
    );
    assert_ne!(
        kappa_of(&default_export, "Foo"),
        kappa_of(&eq_export, "Foo"),
        "`export default Foo` vs `export = Foo` (already distinguished \
         pre-v1.1 since `=` is a symbolic, not keyword-shaped, anonymous \
         leaf -- pinned here as a regression guard)"
    );
}

#[test]
fn typescript_interface_readonly_property_differs_from_plain() {
    // `property_signature` is `public_field_definition`'s interface/
    // object-type-literal sibling: `readonly length: number;` vs `length:
    // number;` share the same anonymous-`readonly`-child shape. Found on a
    // second look at the same `kappa_stats` sample that caught
    // `public_field_definition` -- a 2152-entity group on the TS-monster
    // corpus that was STILL merging `length: number` and `readonly length:
    // number` after that first fix landed, because interfaces use a
    // different node kind than classes for the same modifier.
    let plain = extract("interface I { length: number; }", "a.ts");
    let readonly = extract("interface I { readonly length: number; }", "b.ts");
    assert_ne!(
        kappa_of(&plain, "length"),
        kappa_of(&readonly, "length"),
        "v1.1: interface `readonly length: number` must not collide with \
         `length: number`"
    );
}

#[test]
fn typescript_readonly_field_differs_from_plain_field() {
    // Same shape, same discovery path as the method-modifier fix above:
    // `public_field_definition` is the node kind for both `readonly x = 1`
    // and `x = 1`; `readonly` is an anonymous, non-sole-child leaf.
    let plain = extract("class C { x = 1; }", "a.ts");
    let readonly = extract("class C { readonly x = 1; }", "b.ts");
    assert_ne!(
        kappa_of(&plain, "x"),
        kappa_of(&readonly, "x"),
        "v1.1: `readonly x = 1` must not collide with `x = 1`"
    );
}

#[test]
fn java_public_final_vs_private_final_field_differ() {
    // v1.1 Rule B (the generalized "pure keyword bag" parent test): Java's
    // `modifiers` node holds a *flat list* of keyword children
    // (`repeat1(choice('public', 'private', 'final', ...))`), so with two+
    // modifiers present neither is the v1 rule's "sole child" and both were
    // silently dropped, collapsing every combination that shares a `final`
    // to one kappa.
    let public_final = extract("class C { public final int x = 1; }", "a.java");
    let private_final = extract("class C { private final int x = 1; }", "a.java");
    assert_ne!(
        kappa_of(&public_final, "x"),
        kappa_of(&private_final, "x"),
        "v1.1: `public final int x` and `private final int x` must no \
         longer collide"
    );
}

#[test]
fn java_single_modifier_field_unaffected_by_v1_1() {
    // Sanity/regression: the single-modifier case already worked under v1's
    // "sole child of a named parent" rule (a lone modifier keyword is the
    // `modifiers` node's only child) -- v1.1's generalization must not
    // change this case's kappa story.
    let plain = extract("class C { int x = 1; }", "a.java");
    let final_only = extract("class C { final int x = 1; }", "a.java");
    assert_ne!(
        kappa_of(&plain, "x"),
        kappa_of(&final_only, "x"),
        "a single `final` modifier must still change kappa vs. no modifier"
    );
}

#[test]
fn rust_let_vs_let_mut_already_differ() {
    // Sibling-gap sweep, "no fix needed": tree-sitter-rust represents `mut`
    // as a *named* leaf (`mutable_specifier`), so v1's rule 1 (named leaves
    // always included) already covers it -- confirmed unaffected by v1.1.
    let plain = extract("fn f() { let x = 1; }", "a.rs");
    let mutable = extract("fn f() { let mut x = 1; }", "a.rs");
    assert_ne!(
        kappa_of(&plain, "f"),
        kappa_of(&mutable, "f"),
        "`let x` vs `let mut x` must differ (already true pre-v1.1)"
    );
}

#[test]
fn python_async_def_differs_from_plain_def() {
    // Found on **django**, not the TS-monster corpus: `kappa_stats` on a
    // real Python repo surfaced `def __call__(self, **kwargs): ...` and
    // `async def __call__(self, **kwargs): ...` (two behaviorally
    // different methods in `tests/signals/tests.py`) sharing one kappa.
    // `function_definition` is the same node kind for both; `async` is an
    // anonymous non-sole child -- the exact TS/JS `async` shape, in a
    // completely different grammar. The original per-language sweep only
    // checked Python's `global`/`nonlocal`, not `async def`.
    let plain = extract("def f():\n    pass\n", "a.py");
    let async_def = extract("async def f():\n    pass\n", "b.py");
    assert_ne!(
        kappa_of(&plain, "f"),
        kappa_of(&async_def, "f"),
        "v1.1: Python `async def f()` must not collide with `def f()`"
    );
}

#[test]
fn python_global_statement_already_differs_from_plain_assignment() {
    // Sibling-gap sweep, "no fix needed": Python's grammar gives `global`
    // and `nonlocal` their own node kinds (`global_statement`,
    // `nonlocal_statement`), distinct from a plain `assignment` -- the
    // node-kind hash alone discriminates them, same shape as Go below.
    let plain = extract("def f():\n    x = 1\n    return x\n", "a.py");
    let global = extract("def f():\n    global x\n    x = 1\n    return x\n", "a.py");
    assert_ne!(
        kappa_of(&plain, "f"),
        kappa_of(&global, "f"),
        "a function using `global x` must have different kappa from one \
         that doesn't (already true pre-v1.1)"
    );
}

#[test]
fn go_declaration_keywords_already_differ() {
    // Sibling-gap sweep, "no fix needed": Go's grammar gives `:=`, `var`,
    // and `const` declarations three different node kinds
    // (`short_var_declaration`, `var_declaration`, `const_declaration`), so
    // -- like TS/JS `var` above -- they were never at risk of the
    // let/const-shaped collision.
    let var_entities = extract("package p\nvar x = 1\n", "a.go");
    let const_entities = extract("package p\nconst x = 1\n", "a.go");
    assert_ne!(
        kappa_of(&var_entities, "x"),
        kappa_of(&const_entities, "x"),
        "Go `var x = 1` vs `const x = 1` must differ (already true \
         pre-v1.1)"
    );
}

// ---------------------------------------------------------------------------
// (f) v1.1 universality: formatting-invariance + semantic-sensitivity beyond
// TS/Python/Rust -- JS, Go, Java, Ruby, C++, Kotlin.
// ---------------------------------------------------------------------------

#[test]
fn javascript_formatting_only_change_leaves_kappa_identical() {
    let before = "function add(a, b) {\n  return a + b;\n}\n";
    let after = "function add(\n  a,\n  b,\n) {\n  return a + b\n}\n";
    let before_entities = extract(before, "before.js");
    let after_entities = extract(after, "after.js");
    assert_eq!(
        kappa_of(&before_entities, "add"),
        kappa_of(&after_entities, "add")
    );
    assert_ne!(
        structural_hash_of(&before_entities, "add"),
        structural_hash_of(&after_entities, "add")
    );
}

#[test]
fn javascript_semantic_changes_change_kappa() {
    let original = extract("function add(a, b) {\n  return a + b;\n}\n", "a.js");
    let renamed = extract("function sum(a, b) {\n  return a + b;\n}\n", "b.js");
    let logic_changed = extract("function add(a, b) {\n  return a - b;\n}\n", "c.js");
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&renamed, "sum"),
        "rename"
    );
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&logic_changed, "add"),
        "logic change"
    );
}

#[test]
fn go_formatting_only_change_leaves_kappa_identical() {
    // Go permits a trailing comma when a parameter list spans multiple
    // lines -- the reliable "adds a real leaf token, changes nothing
    // semantic" reformat this fixture family needs (plain re-indentation
    // adds/removes no tokens at all, so it can't move `structural_hash`).
    let before = "package p\nfunc Add(a int, b int) int {\n\treturn a + b\n}\n";
    let after = "package p\nfunc Add(\n\ta int,\n\tb int,\n) int {\n\treturn a + b\n}\n";
    let before_entities = extract(before, "before.go");
    let after_entities = extract(after, "after.go");
    assert_eq!(
        kappa_of(&before_entities, "Add"),
        kappa_of(&after_entities, "Add")
    );
    assert_ne!(
        structural_hash_of(&before_entities, "Add"),
        structural_hash_of(&after_entities, "Add")
    );
}

#[test]
fn go_semantic_changes_change_kappa() {
    let original = extract(
        "package p\nfunc Add(a int, b int) int {\n\treturn a + b\n}\n",
        "a.go",
    );
    let renamed = extract(
        "package p\nfunc Sum(a int, b int) int {\n\treturn a + b\n}\n",
        "b.go",
    );
    let logic_changed = extract(
        "package p\nfunc Add(a int, b int) int {\n\treturn a - b\n}\n",
        "c.go",
    );
    assert_ne!(
        kappa_of(&original, "Add"),
        kappa_of(&renamed, "Sum"),
        "rename"
    );
    assert_ne!(
        kappa_of(&original, "Add"),
        kappa_of(&logic_changed, "Add"),
        "logic change"
    );
}

#[test]
fn java_formatting_only_change_leaves_kappa_identical() {
    // Java doesn't allow a trailing comma in a *parameter* list, but it does
    // in an array *initializer* -- same "adds a real, harmless leaf token"
    // property this fixture family needs.
    let before = "class C { int[] arr = {1, 2, 3}; }";
    let after = "class C { int[] arr = {1, 2, 3,}; }";
    let before_entities = extract(before, "before.java");
    let after_entities = extract(after, "after.java");
    assert_eq!(
        kappa_of(&before_entities, "arr"),
        kappa_of(&after_entities, "arr")
    );
    assert_ne!(
        structural_hash_of(&before_entities, "arr"),
        structural_hash_of(&after_entities, "arr")
    );
}

#[test]
fn java_semantic_changes_change_kappa() {
    let original = extract(
        "class C {\n  int add(int a, int b) {\n    return a + b;\n  }\n}\n",
        "a.java",
    );
    let renamed = extract(
        "class C {\n  int sum(int a, int b) {\n    return a + b;\n  }\n}\n",
        "b.java",
    );
    let logic_changed = extract(
        "class C {\n  int add(int a, int b) {\n    return a - b;\n  }\n}\n",
        "c.java",
    );
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&renamed, "sum"),
        "rename"
    );
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&logic_changed, "add"),
        "logic change"
    );
}

#[test]
fn ruby_formatting_only_change_leaves_kappa_identical() {
    // Ruby permits a trailing comma in a method parameter list.
    let before = "def add(a, b)\n  a + b\nend\n";
    let after = "def add(a, b,)\n  a + b\nend\n";
    let before_entities = extract(before, "before.rb");
    let after_entities = extract(after, "after.rb");
    assert_eq!(
        kappa_of(&before_entities, "add"),
        kappa_of(&after_entities, "add")
    );
    assert_ne!(
        structural_hash_of(&before_entities, "add"),
        structural_hash_of(&after_entities, "add")
    );
}

#[test]
fn ruby_semantic_changes_change_kappa() {
    let original = extract("def add(a, b)\n  a + b\nend\n", "a.rb");
    let renamed = extract("def sum(a, b)\n  a + b\nend\n", "b.rb");
    let logic_changed = extract("def add(a, b)\n  a - b\nend\n", "c.rb");
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&renamed, "sum"),
        "rename"
    );
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&logic_changed, "add"),
        "logic change"
    );
}

#[test]
fn cpp_formatting_only_change_leaves_kappa_identical() {
    // C++ doesn't allow a trailing comma in a function *parameter* list
    // either, but C++11 permits one in an enumerator list -- same trick as
    // Java's array initializer above.
    let before = "enum Color { RED, GREEN, BLUE };";
    let after = "enum Color { RED, GREEN, BLUE, };";
    let before_entities = extract(before, "before.cpp");
    let after_entities = extract(after, "after.cpp");
    assert_eq!(
        kappa_of(&before_entities, "Color"),
        kappa_of(&after_entities, "Color")
    );
    assert_ne!(
        structural_hash_of(&before_entities, "Color"),
        structural_hash_of(&after_entities, "Color")
    );
}

#[test]
fn cpp_semantic_changes_change_kappa() {
    let original = extract("int add(int a, int b) {\n  return a + b;\n}\n", "a.cpp");
    let renamed = extract("int sum(int a, int b) {\n  return a + b;\n}\n", "b.cpp");
    let logic_changed = extract("int add(int a, int b) {\n  return a - b;\n}\n", "c.cpp");
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&renamed, "sum"),
        "rename"
    );
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&logic_changed, "add"),
        "logic change"
    );
}

#[test]
fn cpp_function_definition_modifiers_unaffected_by_python_fix() {
    // C/C++ shares the node-kind string `function_definition` with Python
    // (the discriminator table is global, not namespaced per-language --
    // see `KAPPA_LEADING_KEYWORD_DISCRIMINATOR_PARENTS`'s doc comment).
    // Regression guard: C/C++ modifiers (`static`, `constexpr`, ...) are
    // each wrapped in their own named node (`storage_class_specifier`,
    // `type_qualifier`) with no bare anonymous keyword directly under
    // `function_definition`, so adding `function_definition` for Python's
    // `async def` must not change anything here -- confirmed still
    // differing (via the pre-existing "sole child of a named parent" rule,
    // untouched by this table).
    let plain = extract("int foo() { return 1; }", "a.cpp");
    let static_fn = extract("static int foo() { return 1; }", "b.cpp");
    let constexpr_fn = extract("constexpr int foo() { return 1; }", "c.cpp");
    assert_ne!(kappa_of(&plain, "foo"), kappa_of(&static_fn, "foo"));
    assert_ne!(kappa_of(&plain, "foo"), kappa_of(&constexpr_fn, "foo"));
}

#[test]
fn kotlin_formatting_only_change_leaves_kappa_identical() {
    let before = "fun add(a: Int, b: Int): Int {\n    return a + b\n}\n";
    let after = "fun add(\n    a: Int,\n    b: Int,\n): Int {\n    return a + b\n}\n";
    let before_entities = extract(before, "before.kt");
    let after_entities = extract(after, "after.kt");
    assert_eq!(
        kappa_of(&before_entities, "add"),
        kappa_of(&after_entities, "add")
    );
    assert_ne!(
        structural_hash_of(&before_entities, "add"),
        structural_hash_of(&after_entities, "add")
    );
}

#[test]
fn kotlin_semantic_changes_change_kappa() {
    let original = extract(
        "fun add(a: Int, b: Int): Int {\n    return a + b\n}\n",
        "a.kt",
    );
    let renamed = extract(
        "fun sum(a: Int, b: Int): Int {\n    return a + b\n}\n",
        "b.kt",
    );
    let logic_changed = extract(
        "fun add(a: Int, b: Int): Int {\n    return a - b\n}\n",
        "c.kt",
    );
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&renamed, "sum"),
        "rename"
    );
    assert_ne!(
        kappa_of(&original, "add"),
        kappa_of(&logic_changed, "add"),
        "logic change"
    );
}
