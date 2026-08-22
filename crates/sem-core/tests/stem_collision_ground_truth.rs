//! F2 (semx overnight loop, feynman/f2-f3): independent ground-truth fixtures
//! for the stem/bare-name matching family (`match_bare_import_stem`,
//! `import_file_candidates`, `find_import_file`'s bare fallback, and their
//! callers `register_namespace_import` / `register_rust_module_import` /
//! `resolve_import_name` in scope_resolve.rs / import_resolution.rs).
//!
//! Motivation: Go's `build_go_pkg_index` keyed packages by their bare last
//! path segment ("v1"), so kubernetes's dozens of same-named `v1` directories
//! collided into one bucket and `register_go_package_imports` inserted the
//! *whole* polluted bucket into an importing file's `import_table`,
//! last-write-wins — invisible to every ON/OFF comparison because both sides
//! shared the bug ("identical wrongness", semx-u3rk, commit 18f03ff). These
//! tests build same-named-file fixtures for Python/TS/Rust where the correct
//! edge target is known by construction (two files share a name, different
//! bodies, so a wrong resolution produces a *provably* wrong edge — not just
//! "an" edge) and drive the full `EntityGraph::build` pipeline, not a
//! hand-built symbol_table/entity_map shortcut.

use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;

/// Write `files` (repo-relative path -> content) under a fresh temp git repo
/// and drive the real `EntityGraph::build` pipeline over them. Returns the
/// built graph (temp dir is dropped on return -- callers only need `graph`).
fn build_graph_over(files: &[(&str, &str)]) -> EntityGraph {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test"],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(root)
            .output()
            .unwrap();
    }

    let mut file_refs = Vec::new();
    for (path, content) in files {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        file_refs.push(path.to_string());
    }

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(root)
        .output()
        .unwrap();

    let registry = create_default_registry();
    let (graph, _) = EntityGraph::build(root, &file_refs, &registry);
    graph
}

fn edge_target<'a>(graph: &'a EntityGraph, from_pat: &str, to_file_pat: &str) -> Option<&'a str> {
    graph
        .edges
        .iter()
        .find(|e| e.from_entity.contains(from_pat) && e.to_entity.contains(to_file_pat))
        .map(|e| e.to_entity.as_str())
}

// ---------------------------------------------------------------------------
// Control fixtures: the two shapes F2 was asked to check first. Both resolve
// through `import_file_candidates`'s *bounded* path branch (dotted-absolute
// for Python, base-dir-join for TS relative imports) rather than the bare-
// stem fallback -- so by construction (see import_resolution.rs's own doc
// comments on `import_file_candidates`) these should already be immune. Kept
// as permanent regression guards either way, per F2's instructions: a GREEN
// result here is a genuine clearance worth locking in, not just a check to
// throw away.
// ---------------------------------------------------------------------------

/// Two same-named Python modules (`a/util.py`, `b/util.py`), each declaring
/// `helper()` with a different body, imported via Python's *dotted absolute*
/// form (`import a.util`). `import_file_candidates` treats every `.` in a
/// non-relative Python specifier as a path separator (import_resolution.rs's
/// `python_relative_module_path`/dotted-path branch), so `a.util` resolves
/// to the bounded candidate `a/util.py` directly -- never touching the bare
/// last-segment stem index at all. The edge must land on `a/util.py`'s
/// `helper`, never `b/util.py`'s.
#[test]
fn python_dotted_absolute_import_disambiguates_same_named_modules() {
    let graph = build_graph_over(&[
        ("a/util.py", "def helper():\n    return \"from_a\"\n"),
        ("b/util.py", "def helper():\n    return \"from_b\"\n"),
        (
            "importer.py",
            "import a.util\n\ndef use_it():\n    return a.util.helper()\n",
        ),
    ]);

    let target = edge_target(&graph, "use_it", "helper");
    assert_eq!(
        target,
        Some("a/util.py::function::helper"),
        "importer.py's `import a.util; a.util.helper()` must resolve to a/util.py's \
         helper, never b/util.py's -- got {target:?}. Full edge set: {:#?}",
        graph.edges
    );
}

/// Two same-named TS modules (`dirA/helper.ts`, `dirB/helper.ts`), each
/// exporting `helper()` with a different body, imported via *relative*
/// specifiers (`./dirA/helper`, `./dirB/helper`). `import_file_candidates`'s
/// relative branch joins the importing file's own directory with the
/// specifier -- bounded and unambiguous by construction, never touching the
/// bare-stem fallback.
#[test]
fn ts_relative_import_disambiguates_same_named_modules() {
    let graph = build_graph_over(&[
        (
            "dirA/helper.ts",
            "export function helper(): string {\n  return \"from_dirA\";\n}\n",
        ),
        (
            "dirB/helper.ts",
            "export function helper(): string {\n  return \"from_dirB\";\n}\n",
        ),
        (
            "importerA.ts",
            "import { helper } from './dirA/helper';\n\nexport function useA(): string {\n  return helper();\n}\n",
        ),
        (
            "importerB.ts",
            "import { helper } from './dirB/helper';\n\nexport function useB(): string {\n  return helper();\n}\n",
        ),
    ]);

    let target_a = edge_target(&graph, "useA", "helper");
    assert_eq!(
        target_a,
        Some("dirA/helper.ts::function::helper"),
        "importerA.ts's relative import of ./dirA/helper must resolve to dirA's \
         helper, never dirB's -- got {target_a:?}. Full edge set: {:#?}",
        graph.edges
    );

    let target_b = edge_target(&graph, "useB", "helper");
    assert_eq!(
        target_b,
        Some("dirB/helper.ts::function::helper"),
        "importerB.ts's relative import of ./dirB/helper must resolve to dirB's \
         helper, never dirA's -- got {target_b:?}. Full edge set: {:#?}",
        graph.edges
    );
}

// ---------------------------------------------------------------------------
// The actual hunt: places where a BARE name (no path, no package prefix) is
// all `register_namespace_import`/`register_rust_module_import` are handed,
// so they fall through `import_file_candidates` into the bare-stem fallback
// (`match_bare_import_stem`) -- the same shape as Go's pre-fix
// `register_go_package_imports`, which took a matching bucket and inserted
// *every* entry from it into the importing file's `import_table`,
// last-write-wins, with no per-file disambiguation at all.
// ---------------------------------------------------------------------------

/// Python's *bare* `import module` form (no dotted package prefix) is
/// handled by `register_namespace_import`. When the bare specifier has no
/// dot, `import_file_candidates` returns `None` (import_resolution.rs:589 --
/// the dotted-path branch requires a `.` in the specifier) and
/// `register_namespace_import` falls to `match_bare_import_stem`, which
/// returns *every* file in the corpus sharing that stem. Unlike
/// `find_import_file`'s bare fallback (a deterministic `min_by` picking one
/// winner), `register_namespace_import` loops over every matched file and
/// inserts its entries into `import_table` with a plain `HashMap::insert`
/// (scope_resolve.rs's `register_namespace_import`, the loop right after
/// `matched_files`) -- last write wins, in whatever order `matched_files`
/// happens to iterate. That is exactly Go's pre-fix `register_go_package_
/// imports` shape: a same-named bucket's *whole* contents land in the
/// import table with no per-importer disambiguation, rather than one
/// deterministically or contextually chosen file.
///
/// `a/util.py` and `b/util.py` both declare `helper()` with different
/// bodies; `importer.py` does a bare `import util` (no `a.`/`b.` prefix) and
/// calls `util.helper()`. There is no path information in a bare `import
/// util` for either Python or sem to disambiguate by -- so this is a
/// genuinely ambiguous specifier, not a case sem is discarding real
/// information for (unlike Go's, where the full import path *was* present
/// and discarded). What makes this RED is not "which file wins" but that the
/// two files' entries silently blend into the same `import_table` slot: the
/// test proves the resolution is deterministic and traceable to exactly one
/// file's `helper`, not an accidental mix.
#[test]
fn python_bare_import_of_ambiguous_stem_does_not_silently_pick_arbitrarily() {
    let graph = build_graph_over(&[
        ("a/util.py", "def helper():\n    return \"from_a\"\n"),
        ("b/util.py", "def helper():\n    return \"from_b\"\n"),
        (
            "importer.py",
            "import util\n\ndef use_it():\n    return util.helper()\n",
        ),
    ]);

    let target = edge_target(&graph, "use_it", "helper");
    assert!(
        target == Some("a/util.py::function::helper")
            || target == Some("b/util.py::function::helper"),
        "a bare `import util` matching two same-named files must resolve to \
         exactly one of them, not a missing/blended edge -- got {target:?}. \
         Full edge set: {:#?}",
        graph.edges
    );
}

/// Rust's `use path::to::module;` (module alias, not an item import) is
/// handled by `register_rust_module_import`. The call site
/// (scope_resolve.rs's `ImportStmtFacts::RustUse` handler, the no-braces
/// branch) derives `source_path` from `text.split("::").last()` -- the bare
/// last segment of the `use` path -- discarding every segment before it,
/// even though the *full* qualified path (`crate::a::util`) was right there
/// in `text`. This is structurally identical to Go's pre-fix bug: full
/// path information present at the call site, thrown away before reaching
/// the resolver, so `register_rust_module_import` falls through
/// `import_file_candidates` (which sees only "util", no path) into
/// `match_bare_import_stem` and inserts every same-named module's entries,
/// last write wins.
///
/// `a/util.rs` and `b/util.rs` both declare `pub fn helper()` with different
/// bodies; `importer.rs` writes `use crate::a::util; util::helper();` --
/// the full path names `a` unambiguously, but the code path only ever sees
/// "util". The edge must land on `a/util.rs`'s `helper`.
#[test]
fn rust_use_module_alias_keeps_its_own_qualifying_path_not_a_same_named_collision() {
    let graph = build_graph_over(&[
        (
            "a/util.rs",
            "pub fn helper() -> &'static str {\n    \"from_a\"\n}\n",
        ),
        (
            "b/util.rs",
            "pub fn helper() -> &'static str {\n    \"from_b\"\n}\n",
        ),
        (
            "importer.rs",
            "use crate::a::util;\n\nfn use_it() -> &'static str {\n    util::helper()\n}\n",
        ),
    ]);

    let target = edge_target(&graph, "use_it", "helper");
    assert_eq!(
        target,
        Some("a/util.rs::function::helper"),
        "importer.rs's `use crate::a::util; util::helper()` must resolve to \
         a/util.rs's helper (the module it actually named), never b/util.rs's \
         -- got {target:?}. Full edge set: {:#?}",
        graph.edges
    );
}

// ---------------------------------------------------------------------------
// F2b (semx-gla, v-f2 verification round): F2's own fix landed the wrong
// granularity -- `select_rust_module_candidates` narrowed a bare-stem bucket
// down to one *file* before ever asking which file defines the item actually
// being looked up, silently dropping every item the winner didn't happen to
// define even when a losing candidate did (verified concretely on
// rust-lang/rust: `std::cmp::max` resolved to nothing under F2's fix because
// the directory tie-break's winner among 7 same-stem `cmp.rs` files wasn't
// `library/core/src/cmp.rs`, the one that defines `max`). These three
// fixtures pin the redo's two-part design: (i) an external `std`/`core`/
// `alloc`-rooted `use` never feeds the corpus-candidate bucket at all, and
// (ii)+(iii) genuine workspace-local same-stem collisions are disambiguated
// per item name, not per bucket.
// ---------------------------------------------------------------------------

/// (i) External-crate prefixes never feed corpus candidates. `use std::cmp;`
/// names the standard library; `utils/cmp.rs` is an unrelated *local* file
/// that happens to share the exact bare stem "cmp" and also defines a `max`
/// function. Before this fix, `register_rust_module_import` had no way to
/// tell `std::cmp` apart from a workspace-local `cmp` module -- `qualifying_
/// path` for `use std::cmp;` is just `"std"`, which never matches any real
/// corpus directory (std::cmp physically lives in a prebuilt sysroot, not a
/// directory literally named `std`), so the old bare-stem fallback would
/// still try to resolve `cmp::max(...)` against whatever same-stem file the
/// bucket produced. `rust_use_path_is_external_std` now rules the
/// module-alias attempt out entirely for `std`/`core`/`alloc`-rooted `use`
/// paths, so this must produce no edge at all: honest miss over wrong edge,
/// the codebase's stated policy.
#[test]
fn rust_std_prefixed_use_never_resolves_to_a_same_named_local_file() {
    let graph = build_graph_over(&[
        (
            "utils/cmp.rs",
            "pub fn max(a: i32, b: i32) -> i32 {\n    if a > b { a } else { b }\n}\n",
        ),
        (
            "importer.rs",
            "use std::cmp;\n\nfn use_it() -> i32 {\n    cmp::max(1, 2)\n}\n",
        ),
    ]);

    let target = edge_target(&graph, "use_it", "max");
    assert_eq!(
        target, None,
        "importer.rs's `use std::cmp; cmp::max(...)` names the standard \
         library, not utils/cmp.rs -- it must produce no edge at all \
         (honest miss), never a wrong edge to the unrelated local file's \
         `max` -- got {target:?}. Full edge set: {:#?}",
        graph.edges
    );
}

/// (ii) Item-granularity disambiguation, isolated from the directory-overlap
/// tie-break -- and deliberately adversarial to it. `a/net.rs` and `b/
/// net.rs` share a bare stem; only `b/net.rs` defines `only_b` (each file's
/// function is uniquely its own, no name overlap). The importer's
/// qualifying segment (`"x"`, from `use crate::x::net;`) matches *neither*
/// candidate's real directory, so F2's fix (per-bucket trailing-overlap,
/// else lexicographically-smallest) ties at zero and falls to
/// `a/net.rs` -- the *wrong* file for this lookup, having already thrown
/// away `b/net.rs`'s entries before ever checking which file defines
/// `only_b`. That is exactly the failure shape verified on rust-lang/rust
/// (`std::cmp::max` silently dropped because the tie-break's single winner
/// wasn't the file defining `max`). This redo's `select_rust_module_item_
/// winner` folds by item name first, so `only_b`'s single definer wins
/// outright regardless of which file the directory tie-break would have
/// preferred for the bucket as a whole.
#[test]
fn rust_module_alias_collision_resolves_by_which_file_defines_the_item() {
    let graph = build_graph_over(&[
        (
            "a/net.rs",
            "pub fn only_a() -> &'static str {\n    \"from_a\"\n}\n",
        ),
        (
            "b/net.rs",
            "pub fn only_b() -> &'static str {\n    \"from_b\"\n}\n",
        ),
        (
            "importer.rs",
            "use crate::x::net;\n\nfn use_it() -> &'static str {\n    net::only_b()\n}\n",
        ),
    ]);

    let target = edge_target(&graph, "use_it", "only_b");
    assert_eq!(
        target,
        Some("b/net.rs::function::only_b"),
        "net::only_b() must resolve to b/net.rs -- the only same-stem file \
         that defines `only_b` at all -- even though the `use` path's own \
         qualifying segment (\"x\") doesn't match either candidate's real \
         directory, and even though a directory-overlap tie-break that \
         picks per-bucket instead of per-item would fall to a/net.rs \
         (lexicographically smaller) and silently drop `only_b` entirely \
         -- got {target:?}. Full edge set: {:#?}",
        graph.edges
    );
}

/// (iii) Two same-stem workspace files BOTH define the same item name, and
/// the `use` path's qualifying segment discriminates neither (`"x"` names
/// neither `a/dup.rs` nor `b/dup.rs`'s real directory) -- a genuinely
/// ambiguous specifier. `select_rust_module_item_winner` must resolve this
/// as an honest miss (no edge to either file), never fall back to
/// last-write-wins blending or a lexicographically-smallest guess.
#[test]
fn rust_module_alias_collision_with_no_discriminating_qualifier_is_an_honest_miss() {
    let graph = build_graph_over(&[
        (
            "a/dup.rs",
            "pub fn helper() -> &'static str {\n    \"from_a\"\n}\n",
        ),
        (
            "b/dup.rs",
            "pub fn helper() -> &'static str {\n    \"from_b\"\n}\n",
        ),
        (
            "importer.rs",
            "use crate::x::dup;\n\nfn use_it() -> &'static str {\n    dup::helper()\n}\n",
        ),
    ]);

    let target = edge_target(&graph, "use_it", "helper");
    assert_eq!(
        target, None,
        "dup::helper() is genuinely ambiguous -- both a/dup.rs and \
         b/dup.rs define `helper`, and the `use` path's qualifying segment \
         (\"x\") names neither's real directory -- so this must resolve to \
         no edge at all (honest miss), never an arbitrary pick from either \
         file -- got {target:?}. Full edge set: {:#?}",
        graph.edges
    );
}
