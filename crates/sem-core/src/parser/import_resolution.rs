use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use crate::parser::graph::EntityInfo;
use regex::Regex;

pub(crate) const JS_TS_EXTENSIONS: &[&str] =
    &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

pub(crate) fn is_js_ts_file(file_path: &str) -> bool {
    JS_TS_EXTENSIONS
        .iter()
        .any(|extension| file_path.ends_with(extension))
}

/// Extensions whose cross-file reads `scope_resolve.rs` now attributes through
/// the read-set recorder (semx-kzy), beyond the original JS/TS set:
/// * `.py` — `extract_python_import`/`extract_python_module_import`, via
///   `resolve_import_name` (`Table::SymbolTable`/`Table::EntityMap`, bounded
///   per import) and `register_namespace_import` (whole-table guard,
///   `Table::GuardPyWildcardImport`, for the bare `import module` form only).
/// * `.go` — `register_go_package_imports` now records `Table::GoPkgIndex`
///   (the table already existed and was fingerprinted; only the *read* site
///   was missing).
/// * `.rs` — `extract_rust_use`, via the same `resolve_import_name` path as
///   Python's `from X import Y`.
/// * `.kt` — whitelisted, not attributed: `KOTLIN_SCOPE_CONFIG` produces
///   `call_expression`/`navigation_expression` nodes, none of which are
///   `import_from_statement`/`import_statement`/`use_declaration`/
///   `import_declaration` (Kotlin's own `import_header` node kind is not
///   handled by `extract_imports_from_ast` at all), so that function is a
///   structural no-op for Kotlin source, and `scan_constructor_calls`
///   (hardcoded to `kind == "call"`) never matches Kotlin's `call_expression`
///   nodes either. Its cross-file resolution is entirely the generic
///   `Table::SymbolTable`/`Table::ClassMembers` fallback every language
///   already shares — confirmed empirically, not just by grep: a bare
///   cross-file function call (no import statement at all) resolves via
///   `resolve_ref`'s step-3 `Table::SymbolTable` fallback exactly like
///   Python's or Go's does. Proof is grep evidence plus the oracle fixture
///   below, not a per-file read-set gap closed, so it belongs in the same
///   list as the genuinely newly-instrumented languages for GREEN
///   eligibility purposes.
///
///   (bash was evaluated first and rejected by the semx-kzy bead: `BASH_
///   SCOPE_CONFIG`'s `call_style: FunctionField("name")` hands `extract_
///   call_ref` a `command_name` node, whose `.kind()` is `"command_name"` —
///   not `"identifier"`/`"simple_identifier"`/`"type_identifier"`, the only
///   kinds `extract_call_ref`'s fast path recognized at the time — so bash
///   calls were never collected as `AstRefKind::Call` at all, cold or warm.
///   semx-ocj fixed this: `extract_call_ref` now unwraps `command_name` to
///   its `word` child and recognizes bare `"word"` nodes directly (fish's
///   `command` node hands over `word` with no wrapper, so the same one-line
///   fix covers both). See `bash_calls_across_files_become_reference_edges`
///   / `fish_calls_across_files_become_reference_edges` in `session.rs` for
///   the RED→GREEN proof.
/// * `.sh` (bash) / `.fish` — whitelisted, not attributed, same shape as
///   Kotlin: bash and fish have no import mechanism `extract_imports_from_
///   ast` recognizes at all (no dedicated node kind, and `source`/builtins
///   filter out what would otherwise look like a call to the sourced file).
///   Cross-file calls resolve entirely through the generic `Table::
///   SymbolTable` fallback, confirmed by the oracle fixtures in `session.rs`
///   (`write_bash_fixture`/`write_fish_fixture`) now that semx-ocj lets a
///   bash/fish call become a ref to resolve in the first place.
/// * `.java` — whitelisted, not attributed, with a caveat worth recording:
///   tree-sitter-java's import statement node kind is *also* named
///   `"import_declaration"` — the exact kind `extract_imports_from_ast`
///   routes to `extract_go_import` (Go's extractor). This looked like a
///   correctness landmine until traced through: Go's extractor only acts on
///   an `import_declaration`'s `import_spec`/`import_spec_list`/string-
///   literal children, and Java's grammar only ever puts `asterisk`/
///   `identifier`/`scoped_identifier` children under that node — no overlap,
///   so it silently no-ops on Java source (verified: the fixture below
///   includes a real `import java.util.List;` statement and stays bit-
///   identical warm vs. cold). Java's actual cross-file resolution is the
///   `ClassName.staticMethod()` static-call path in `resolve_ref`
///   (`Table::ClassMembers`, already generic and recorded).
/// * `.cs` (C#) — whitelisted, not attributed: no node-kind collision at
///   all (`c-sharp`'s grammar has no node named any of the five kinds
///   `extract_imports_from_ast` recognizes), same static-call path as Java.
/// * `.cpp`/`.cc`/`.cxx`/`.hpp`/`.hh`/`.hxx` (C++) — whitelisted, not
///   attributed: no node-kind collision; free functions resolve through
///   `Table::SymbolTable` exactly like Kotlin/bash.
/// * `.rb` (Ruby) — whitelisted, not attributed: no node-kind collision;
///   `RUBY_SCOPE_CONFIG`'s `CallNodeStyle::DirectMethod` with no `receiver`
///   field present degrades a bare `helper()` to a plain `AstRefKind::Call`,
///   so free-method calls resolve through `Table::SymbolTable`.
/// * `.php`/`.inc`/`.phtml`/`.module` (PHP) — whitelisted, **with a real
///   landmine found and defused, not just checked**: tree-sitter-php's `use`
///   statement node kind is `"use_declaration"` — the exact kind `extract_
///   imports_from_ast` routes to `extract_rust_use` (Rust's extractor).
///   Unlike Java/Go, this one does not clean-miss: `extract_rust_use` reads
///   the node's raw *text*, not its child kinds, so it always "succeeds" at
///   parsing something. Fed PHP's `use App\Foo\Bar;`, it finds no `"::"`
///   (PHP's separator is `\`), so it falls into the single-segment branch
///   and treats the entire backslash-joined path as one opaque symbol name
///   — a real symbol lookup, correctly recorded through `resolve_import_name`
///   (`rec.one`/`rec.two` fire), that is a structural *miss* every time
///   because no PHP entity is ever named with embedded backslashes. A miss
///   is a dependency too (see `register_namespace_import`'s doc comment),
///   so this is conservative, not wrong — proven, not assumed: the fixture
///   below includes a real `use` statement and both the oracle
///   (warm-vs-cold bit-identical) and a `use`-target-touching mutation stay
///   green through it.
/// * `.scala`/`.sc`/`.sbt`/`.kojo`/`.mill` (Scala) — whitelisted, with the
///   same `"import_declaration"`-name collision as Java, checked the same
///   way: Scala's `import_declaration` never has an `import_spec`/`import_
///   spec_list`/string-literal child either (its children are Scala type/
///   pattern nodes), so `extract_go_import` no-ops here too — verified by a
///   fixture containing a real `import` statement, not just by grep.
/// * `.zig` (Zig) — whitelisted, not attributed: no node-kind collision;
///   resolves through `Table::SymbolTable` like Kotlin.
/// * `.dart` (Dart) — **left RED, not whitelisted**: `DART_SCOPE_CONFIG` is
///   one of the "Tier 2 (Minimal)" configs and its `call_nodes` is `&
///   ["function_expression_body"]`, not an actual call-expression node kind
///   — ordinary statement-body function calls (`helper();` inside a `{ }`
///   block) are never even reached by `collect_all_file_refs`'s call-node
///   branch, only expression-bodied arrow functions (`() => helper()`) are.
///   The oracle fixture below (ordinary block-bodied functions, the normal
///   Dart style) confirms this empirically: zero `Calls` edges appear
///   between its hub and its callers even after semx-ocj, so there is
///   nothing here for GREEN eligibility to be unsound *about* — but nothing
///   to gain either. This is a real, narrower entity/ref-extraction gap in
///   Dart (not scope-resolution eligibility), out of this bead's scope to
///   fix, surfaced here rather than silently routed around by writing
///   arrow-bodied Dart just to make eligibility look proven.
///
/// See RESOLUTION-PROFILE.md's "Universal GREEN eligibility" section for the
/// full per-language verdict table and why every other language stays
/// perma-RED for now (either genuinely unattributed, or attributable but not
/// yet exercised by a dedicated oracle fixture in this pass).
const NEWLY_ATTRIBUTED_EXTENSIONS: &[&str] = &[
    ".py", ".go", ".rs", ".kt", ".sh", ".fish", ".java", ".cs", ".cpp", ".cc", ".cxx", ".hpp",
    ".hh", ".hxx", ".rb", ".php", ".inc", ".phtml", ".module", ".scala", ".sc", ".sbt", ".kojo",
    ".mill", ".zig",
];

/// Whether `file_path`'s detected language may reuse cached resolution
/// results on a warm rebuild (semx-022 + semx-kzy). See
/// [`NEWLY_ATTRIBUTED_EXTENSIONS`] for what changed and why; everything not
/// named here is held perma-RED — slower, never wrong.
pub(crate) fn is_reuse_eligible_file(file_path: &str) -> bool {
    is_js_ts_file(file_path)
        || NEWLY_ATTRIBUTED_EXTENSIONS
            .iter()
            .any(|extension| file_path.ends_with(extension))
}

pub fn js_ts_import_source_files_from_content<P: AsRef<str>>(
    file_path: &str,
    content: &str,
    candidate_file_paths: &[P],
) -> Vec<String> {
    if !is_js_ts_file(file_path) {
        return Vec::new();
    }

    let mut imported_files = HashSet::new();
    for source in js_ts_import_sources(content) {
        if let Some(imported_file) =
            find_import_file(candidate_file_paths, source, file_path, JS_TS_EXTENSIONS)
        {
            imported_files.insert(imported_file.to_string());
        }
    }

    let mut imported_files: Vec<String> = imported_files.into_iter().collect();
    sort_import_candidate_files(&mut imported_files, JS_TS_EXTENSIONS);
    imported_files
}

/// The corpus's import-candidate set, **prepared once per build** (semx-3tb,
/// `SINGLE-PASS.md` §2 pass I): O(1) membership for relative specifiers, plus
/// the stem index [`build_stem_index`] already provided for bare/package
/// specifiers.
///
/// Both halves are properties of the candidate *corpus*, not of the importing
/// file, so building them per file was pure repetition — semx-ccg measured
/// the second half of that repetition as the monster's residual after the
/// O(1)-membership fix: `sorted_candidates` re-sorted the whole ~40k-entry
/// candidate list on the first bare import *of every file*.
///
/// The swap is an identity, not an approximation. The old fallback took the
/// first element of the `(extension_priority, path)`-sorted candidate list
/// whose stem matched; [`resolve_bare_import_stem`] takes the minimum under
/// that same total order among the stem's matches. For a total order, the
/// minimum of a subset **is** the first element of the sorted whole
/// restricted to that subset — `sort_import_candidate_files` and
/// `resolve_bare_import_stem` are literally written against the same
/// comparator (`extension_priority(..).cmp(..).then_with(|| a.cmp(b))`), so
/// the two agree on every input, ties included. Witnessed by
/// `bare_import_stem_index_agrees_with_sorted_scan`.
pub struct ImportCandidates<'a> {
    paths: HashSet<&'a str>,
    stems: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> ImportCandidates<'a> {
    pub fn new(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let paths: HashSet<&'a str> = paths.into_iter().collect();
        let mut stems: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
        for path in &paths {
            stems.entry(file_stem(path)).or_default().push(path);
        }
        Self { paths, stems }
    }

    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
    }
}

pub fn js_ts_import_source_files_from_set(
    file_path: &str,
    content: &str,
    candidates: &ImportCandidates<'_>,
) -> Vec<String> {
    if !is_js_ts_file(file_path) {
        return Vec::new();
    }

    let mut imported_files = HashSet::new();
    for source in js_ts_import_sources(content) {
        if let Some(relative) = import_file_candidates(file_path, source, JS_TS_EXTENSIONS) {
            if let Some(imported_file) = relative
                .iter()
                .find(|candidate| candidates.contains(candidate.as_str()))
            {
                imported_files.insert(imported_file.clone());
            }
            continue;
        }

        if let Some(imported_file) =
            resolve_bare_import_stem(&candidates.stems, source, JS_TS_EXTENSIONS)
        {
            imported_files.insert(imported_file.to_string());
        }
    }

    let mut imported_files: Vec<String> = imported_files.into_iter().collect();
    sort_import_candidate_files(&mut imported_files, JS_TS_EXTENSIONS);
    imported_files
}

pub fn js_ts_import_source_files_from_filesystem(
    root: &Path,
    file_path: &str,
    content: &str,
) -> Vec<String> {
    js_ts_import_source_files_from_filesystem_with_unscoped(root, file_path, content).0
}

pub fn js_ts_import_source_files_from_filesystem_with_unscoped(
    root: &Path,
    file_path: &str,
    content: &str,
) -> (Vec<String>, bool) {
    if !is_js_ts_file(file_path) {
        return (Vec::new(), false);
    }

    let mut imported_files = HashSet::new();
    let mut has_unscoped_imports = false;
    for source in js_ts_import_sources(content) {
        let Some(candidates) = import_file_candidates(file_path, source, JS_TS_EXTENSIONS) else {
            has_unscoped_imports = true;
            continue;
        };
        if let Some(imported_file) = candidates
            .iter()
            .find(|candidate| root.join(candidate).is_file())
        {
            imported_files.insert(imported_file.clone());
        }
    }

    let mut imported_files: Vec<String> = imported_files.into_iter().collect();
    sort_import_candidate_files(&mut imported_files, JS_TS_EXTENSIONS);
    (imported_files, has_unscoped_imports)
}

fn js_ts_import_sources(content: &str) -> Vec<&str> {
    static FROM_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\bfrom\s*['"]([^'"]+)['"]"#).unwrap());
    static SIDE_EFFECT_IMPORT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\bimport\s*['"]([^'"]+)['"]"#).unwrap());

    let mut sources = Vec::new();
    for cap in FROM_RE.captures_iter(content) {
        if let Some(source) = cap.get(1).map(|m| m.as_str()) {
            sources.push(source);
        }
    }
    for cap in SIDE_EFFECT_IMPORT_RE.captures_iter(content) {
        if let Some(source) = cap.get(1).map(|m| m.as_str()) {
            sources.push(source);
        }
    }
    sources
}

pub fn js_ts_has_default_re_export_from_content(content: &str) -> bool {
    static DEFAULT_REEXPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"export\s+(?:type\s+)?\{[^}]*\bdefault\b[^}]*\}\s*from\s*['"][^'"]+['"]"#)
            .unwrap()
    });

    DEFAULT_REEXPORT_RE.is_match(content)
}

pub(crate) fn js_ts_named_exports_from_content(content: &str) -> HashSet<String> {
    static NAMED_DECL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\bexport\s+(?:declare\s+)?(?:async\s+)?(?:abstract\s+)?(?:function\s*\*?|class|interface|type|enum)\s+([A-Za-z_$][\w$]*)",
        )
        .unwrap()
    });
    static VAR_DECL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bexport\s+(?:declare\s+)?(?:const|let|var)\s+([^;\n]+)").unwrap()
    });
    static SPECIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"export\s+(?:type\s+)?\{([^}]+)\}(?:\s*from\s*['"][^'"]+['"])?\s*;?"#).unwrap()
    });

    let mut names = HashSet::new();

    for cap in NAMED_DECL_RE.captures_iter(content) {
        names.insert(cap.get(1).unwrap().as_str().to_string());
    }

    for cap in VAR_DECL_RE.captures_iter(content) {
        for declarator in split_js_ts_var_declarators(cap.get(1).unwrap().as_str()) {
            if let Some(name) = js_ts_identifier_prefix(declarator) {
                names.insert(name.to_string());
            }
        }
    }

    for cap in SPECIFIER_RE.captures_iter(content) {
        for name_part in cap.get(1).unwrap().as_str().split(',') {
            if let Some(exported_name) = js_ts_export_specifier_name(name_part) {
                names.insert(exported_name.to_string());
            }
        }
    }

    names
}

fn split_js_ts_var_declarators(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;

    for (idx, ch) in input.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&input[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(&input[start..]);
    parts
}

fn js_ts_export_specifier_name(name_part: &str) -> Option<&str> {
    let name_part = name_part.trim();
    if name_part.is_empty() {
        return None;
    }

    let exported = if let Some(pos) = name_part.find(" as ") {
        &name_part[pos + 4..]
    } else {
        name_part
    };
    let exported = exported.trim();
    let exported = exported.strip_prefix("type ").unwrap_or(exported).trim();

    js_ts_identifier_prefix(exported)
}

fn js_ts_identifier_prefix(input: &str) -> Option<&str> {
    let input = input.trim_start();
    let mut chars = input.char_indices();
    let (_, first) = chars.next()?;
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return None;
    }

    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch == '_' || ch == '$' || ch.is_ascii_alphanumeric() {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    Some(&input[..end])
}

pub(crate) fn sort_import_candidate_files<P: AsRef<str>>(paths: &mut [P], extensions: &[&str]) {
    paths.sort_by(|left, right| {
        let left = left.as_ref();
        let right = right.as_ref();
        extension_priority(left, extensions)
            .cmp(&extension_priority(right, extensions))
            .then_with(|| left.cmp(right))
    });
}

fn extension_priority(file_path: &str, extensions: &[&str]) -> usize {
    extensions
        .iter()
        .position(|extension| file_path.ends_with(extension))
        .unwrap_or(extensions.len())
}

pub(crate) fn find_import_target<'a, S>(
    target_ids: &'a [String],
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    entity_map: &HashMap<String, EntityInfo, S>,
) -> Option<&'a String>
where
    S: BuildHasher,
{
    let target_files: Vec<&str> = target_ids
        .iter()
        .filter_map(|id| entity_map.get(id).map(|entity| entity.file_path.as_str()))
        .collect();
    let target_file = find_import_file(&target_files, source_path, file_path, extensions)?;

    target_ids.iter().find(|id| {
        entity_map
            .get(*id)
            .map_or(false, |entity| entity.file_path == target_file)
    })
}

pub(crate) fn find_import_file<'a, P: AsRef<str>>(
    candidate_file_paths: &'a [P],
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
) -> Option<&'a str> {
    if let Some(candidates) = import_file_candidates(file_path, source_path, extensions) {
        return candidates.iter().find_map(|candidate_path| {
            candidate_file_paths
                .iter()
                .map(AsRef::as_ref)
                .find(|path| *path == candidate_path.as_str())
        });
    }

    // Bare/package import: no path to resolve against, so fall back to matching
    // by module stem. This is equivalent to sorting every candidate by
    // (extension priority, path) and taking the first stem match — i.e. the
    // minimum of the stem-matching subset under that same total order — but it
    // is a single O(candidates) scan instead of a clone + O(n log n) sort of the
    // whole repo, and it evaluates `extension_priority` only for the handful of
    // candidates whose stem actually matches.
    //
    // The sort mattered: this runs once per import statement, and
    // `candidate_file_paths` is every JS/TS file in the repo. On
    // microsoft/TypeScript that is ~10^4 imports x ~4x10^4 candidates sorted
    // each time — billions of comparator calls, minutes of a pinned core.
    let source_module = import_stem(source_path);
    candidate_file_paths
        .iter()
        .map(AsRef::as_ref)
        .filter(|path| file_stem(path) == source_module)
        .min_by(|left, right| {
            extension_priority(left, extensions)
                .cmp(&extension_priority(right, extensions))
                .then_with(|| (*left).cmp(*right))
        })
}

/// `stem -> matching file paths` index over a candidate list, for resolving
/// many bare/package specifiers against the *same* candidate list without
/// re-scanning it per specifier.
///
/// `find_import_file`'s own doc comment already flags its bare-specifier
/// fallback as expensive when run "once per import statement" against a
/// whole-repo candidate list. That comment was written for a caller that
/// visits each import once per build; `graph.rs`'s incremental import-table
/// maintenance (semx-h1s) re-resolves every RED file's pending imports on
/// *every* warm rebuild, so the same O(candidates) fallback, run repeatedly
/// against a stable candidate list, dominated a warm rebuild's wall time at
/// monster scale (measured: cut one warm-rebuild phase from over 700ms to
/// under 50ms). Building this index once per build and looking up each
/// specifier's stem in it is the fix — `resolve_bare_import_stem` is the
/// paired lookup.
pub(crate) fn build_stem_index<P: AsRef<str>>(paths: &[P]) -> HashMap<&str, Vec<&str>> {
    let mut index: HashMap<&str, Vec<&str>> = HashMap::new();
    for p in paths {
        let p = p.as_ref();
        index.entry(file_stem(p)).or_default().push(p);
    }
    index
}

/// Resolve a bare/package specifier against a pre-built [`build_stem_index`],
/// exactly reproducing `find_import_file`'s bare-fallback tie-break (minimum
/// by `(extension priority, path)` among stem matches) without re-scanning
/// the whole candidate list.
pub(crate) fn resolve_bare_import_stem<'a>(
    stem_index: &HashMap<&str, Vec<&'a str>>,
    source_path: &str,
    extensions: &[&str],
) -> Option<&'a str> {
    let source_module = import_stem(source_path);
    let candidates = stem_index.get(source_module)?;
    candidates.iter().copied().min_by(|left, right| {
        extension_priority(left, extensions)
            .cmp(&extension_priority(right, extensions))
            .then_with(|| (*left).cmp(*right))
    })
}

/// `stem -> matching file paths` index over a candidate list, built with
/// owned strings so it can be stored alongside (not borrowing from) the
/// candidate list it was built from — needed by callers, like Python's
/// bare `import module` resolution below, that build the index once per
/// build in a cached struct rather than borrowing a slice with a matching
/// lifetime (the `&str`-borrowing [`build_stem_index`] above needs the
/// source slice to outlive the index, which a `OnceLock`-cached struct
/// field can't guarantee without becoming self-referential).
pub(crate) fn build_owned_stem_index<P: AsRef<str>, S: BuildHasher + Default>(
    paths: &[P],
) -> HashMap<String, Vec<String>, S> {
    let mut index: HashMap<String, Vec<String>, S> = HashMap::default();
    for p in paths {
        let p = p.as_ref();
        index
            .entry(file_stem(p).to_string())
            .or_default()
            .push(p.to_string());
    }
    index
}

/// Every file path in a [`build_owned_stem_index`] index whose stem matches
/// `source_path`'s bare/package specifier stem — the "match every candidate,
/// not just the best one" counterpart to [`resolve_bare_import_stem`], for
/// callers that need to reproduce [`import_source_matches_file`]'s
/// union-of-matches semantics (every corpus file that could plausibly be the
/// import target) instead of [`find_import_file`]'s single-best-match
/// semantics (the one file resolution would actually pick).
pub(crate) fn match_bare_import_stem<'a, S: BuildHasher>(
    stem_index: &'a HashMap<String, Vec<String>, S>,
    source_path: &str,
) -> Option<&'a Vec<String>> {
    stem_index.get(import_stem(source_path))
}

pub(crate) fn import_source_matches_file(
    importing_file_path: &str,
    source_path: &str,
    extensions: &[&str],
    candidate_file_path: &str,
) -> bool {
    import_file_candidates(importing_file_path, source_path, extensions).map_or_else(
        || file_stem(candidate_file_path) == import_stem(source_path),
        |paths| paths.iter().any(|path| path == candidate_file_path),
    )
}

/// The bounded set of repo-relative paths a relative import specifier could
/// resolve to (module path + each known extension, then `/index` variants),
/// in resolution-priority order — or `None` for a bare/package specifier,
/// which has no bounded candidate set (resolved, if at all, by a whole-corpus
/// stem scan; see [`find_import_file`]'s fallback branch).
///
/// `pub(crate)` so `graph.rs`'s incremental import-table maintenance
/// (semx-h1s) can enumerate the same candidates `find_import_file` would, to
/// record a read-set dependency on each one without duplicating the
/// extension/index-variant logic.
pub(crate) fn import_file_candidates(
    file_path: &str,
    source_path: &str,
    extensions: &[&str],
) -> Option<Vec<String>> {
    let source_path = source_path.trim();
    if source_path.is_empty() {
        return None;
    }

    let module_path = if source_path.starts_with('.')
        && !source_path.starts_with("./")
        && !source_path.starts_with("../")
    {
        python_relative_module_path(file_path, source_path)?
    } else if source_path.starts_with("./") || source_path.starts_with("../") {
        let base_dir = Path::new(file_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        normalize_repo_path(base_dir.join(source_path))?
    } else if extensions.len() == 1 && extensions[0] == ".py" && source_path.contains('.') {
        normalize_repo_path(PathBuf::from(source_path.replace('.', "/")))?
    } else {
        return None;
    };

    Some(module_candidates(&module_path, extensions))
}

fn python_relative_module_path(file_path: &str, source_path: &str) -> Option<String> {
    let dot_count = source_path.chars().take_while(|c| *c == '.').count();
    if dot_count == 0 {
        return None;
    }

    let mut base = PathBuf::from(
        Path::new(file_path)
            .parent()
            .unwrap_or_else(|| Path::new("")),
    );
    for _ in 1..dot_count {
        base = base.parent()?.to_path_buf();
    }

    let remainder = source_path[dot_count..].replace('.', "/");
    if remainder.is_empty() {
        normalize_repo_path(base)
    } else {
        normalize_repo_path(base.join(remainder))
    }
}

fn module_candidates(module_path: &str, extensions: &[&str]) -> Vec<String> {
    let mut candidates = Vec::new();
    let known_ext = extensions.iter().find(|ext| module_path.ends_with(**ext));

    if let Some(_ext) = known_ext {
        candidates.push(module_path.to_string());
    } else {
        for ext in extensions {
            candidates.push(format!("{module_path}{ext}"));
        }
        for ext in extensions {
            candidates.push(format!("{module_path}/index{ext}"));
        }
    }

    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));
    candidates
}

fn normalize_repo_path(path: PathBuf) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

fn import_stem(source_path: &str) -> &str {
    let source_path = source_path.trim_start_matches('.');
    let source_path = source_path.rsplit('/').next().unwrap_or(source_path);
    let stem = file_stem(source_path);
    if stem == source_path {
        source_path.rsplit('.').next().unwrap_or(source_path)
    } else {
        stem
    }
}

fn file_stem(file_path: &str) -> &str {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    file_name
        .strip_suffix(".py")
        .or_else(|| file_name.strip_suffix(".rs"))
        .or_else(|| file_name.strip_suffix(".mts"))
        .or_else(|| file_name.strip_suffix(".cts"))
        .or_else(|| file_name.strip_suffix(".ts"))
        .or_else(|| file_name.strip_suffix(".tsx"))
        .or_else(|| file_name.strip_suffix(".mjs"))
        .or_else(|| file_name.strip_suffix(".cjs"))
        .or_else(|| file_name.strip_suffix(".js"))
        .or_else(|| file_name.strip_suffix(".jsx"))
        .or_else(|| file_name.strip_suffix(".go"))
        .unwrap_or(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(file_path: &str) -> EntityInfo {
        EntityInfo {
            id: format!("{file_path}::function::helper"),
            name: "helper".to_string(),
            entity_type: "function".to_string(),
            file_path: file_path.to_string(),
            parent_id: None,
            start_line: 1,
            end_line: 1,
        }
    }

    #[test]
    fn explicit_relative_import_prefers_exact_extension() {
        let ids = vec![
            "src/util.js::function::helper".to_string(),
            "src/util.ts::function::helper".to_string(),
        ];
        let entity_map = HashMap::from([
            (ids[0].clone(), entity("src/util.js")),
            (ids[1].clone(), entity("src/util.ts")),
        ]);

        let target = find_import_target(
            &ids,
            "./util.ts",
            "src/main.ts",
            JS_TS_EXTENSIONS,
            &entity_map,
        );

        assert_eq!(target, Some(&ids[1]));
    }

    #[test]
    fn explicit_relative_import_requires_exact_extension() {
        let ids = vec!["src/util.js::function::helper".to_string()];
        let entity_map = HashMap::from([(ids[0].clone(), entity("src/util.js"))]);

        let target = find_import_target(
            &ids,
            "./util.ts",
            "src/main.ts",
            JS_TS_EXTENSIONS,
            &entity_map,
        );

        assert_eq!(target, None);
    }

    #[test]
    fn js_ts_import_source_files_resolves_relative_imports_and_re_exports() {
        let candidates = vec![
            "src/a.ts".to_string(),
            "src/b.ts".to_string(),
            "src/c/index.ts".to_string(),
            "src/unused.ts".to_string(),
        ];
        let content = r#"
import { a } from './a';
import DefaultB from "./b";
import './c';
export { a as publicA } from './a';
"#;

        let imports = js_ts_import_source_files_from_content("src/main.ts", content, &candidates);

        assert_eq!(imports, vec!["src/a.ts", "src/b.ts", "src/c/index.ts"]);
    }

    #[test]
    fn js_ts_default_re_export_detection_handles_aliases() {
        assert!(js_ts_has_default_re_export_from_content(
            "export { default } from './target';"
        ));
        assert!(js_ts_has_default_re_export_from_content(
            "export { default as PublicTarget } from './target';"
        ));
        assert!(!js_ts_has_default_re_export_from_content(
            "export { value as PublicTarget } from './target';"
        ));
    }

    #[test]
    fn js_ts_import_source_files_from_filesystem_resolves_existing_relative_files() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("src/c")).unwrap();
        std::fs::write(root.path().join("src/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(root.path().join("src/c/index.ts"), "export const c = 1;\n").unwrap();
        let content = r#"
import { a } from './a';
import './c';
import './missing';
"#;

        let imports =
            js_ts_import_source_files_from_filesystem(root.path(), "src/main.ts", content);

        assert_eq!(imports, vec!["src/a.ts", "src/c/index.ts"]);
    }

    #[test]
    fn absolute_python_import_uses_dotted_path() {
        let ids = vec![
            "src/a/util.py::function::helper".to_string(),
            "src/b/util.py::function::helper".to_string(),
        ];
        let entity_map = HashMap::from([
            (ids[0].clone(), entity("src/a/util.py")),
            (ids[1].clone(), entity("src/b/util.py")),
        ]);

        let target = find_import_target(&ids, "src.b.util", "src/main.py", &[".py"], &entity_map);

        assert_eq!(target, Some(&ids[1]));
    }

    #[test]
    fn bare_import_with_extension_uses_file_stem() {
        let ids = vec!["src/util.ts::function::helper".to_string()];
        let entity_map = HashMap::from([(ids[0].clone(), entity("src/util.ts"))]);

        let target = find_import_target(
            &ids,
            "util.ts",
            "src/main.ts",
            JS_TS_EXTENSIONS,
            &entity_map,
        );

        assert_eq!(target, Some(&ids[0]));
    }

    #[test]
    fn bare_import_prefers_ordered_module_variant() {
        let ids = vec![
            "lib.js::function::helper".to_string(),
            "lib.ts::function::helper".to_string(),
        ];
        let entity_map = HashMap::from([
            (ids[0].clone(), entity("lib.js")),
            (ids[1].clone(), entity("lib.ts")),
        ]);

        let target = find_import_target(&ids, "lib", "consumer.ts", JS_TS_EXTENSIONS, &entity_map);

        assert_eq!(target, Some(&ids[1]));
    }

    #[test]
    fn explicit_relative_import_resolves_module_variants_before_same_named_ts() {
        for extension in [".mts", ".cts", ".mjs", ".cjs"] {
            let ids = vec![
                "src/config.ts::function::helper".to_string(),
                format!("src/deep/config{extension}::function::helper"),
            ];
            let entity_map = HashMap::from([
                (ids[0].clone(), entity("src/config.ts")),
                (
                    ids[1].clone(),
                    entity(&format!("src/deep/config{extension}")),
                ),
            ]);

            let target = find_import_target(
                &ids,
                &format!("./deep/config{extension}"),
                "src/main.ts",
                JS_TS_EXTENSIONS,
                &entity_map,
            );

            assert_eq!(target, Some(&ids[1]), "extension: {extension}");
        }
    }
}

#[cfg(test)]
mod single_pass_laws {
    use super::*;

    /// xorshift64*, so a failure is reproducible from its seed.
    struct Gen(u64);

    impl Gen {
        fn new(seed: u64) -> Self {
            Gen(seed | 1)
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// The **specification**: the bare/package fallback exactly as it was
    /// written before semx-3tb — sort the whole candidate list by
    /// `(extension_priority, path)` and take the first entry whose stem
    /// matches. Expressed with the production comparator and the production
    /// stem functions, so this is the old algorithm, not a paraphrase of the
    /// new one.
    fn sorted_scan<'a>(candidates: &[&'a str], source: &str) -> Option<&'a str> {
        let source_module = import_stem(source);
        let mut sorted: Vec<&str> = candidates.to_vec();
        sort_import_candidate_files(&mut sorted, JS_TS_EXTENSIONS);
        sorted
            .into_iter()
            .find(|path| file_stem(path) == source_module)
    }

    /// **L-STEM-INDEX** (`SINGLE-PASS.md` §6, W1-F4)
    ///
    /// ```text
    /// ∀ candidates, s.  resolve_bare_import_stem(build_stem_index(candidates), s)
    ///                 = first(sort_(prio,path)(candidates) ∩ {stem = stem(s)})
    /// ```
    ///
    /// The two are equal because `≤ = (extension_priority, path)` is a
    /// **total** order on a set of distinct paths, and for a total order the
    /// minimum of a subset is the first element of the sorted whole
    /// restricted to that subset. That identity — not a benchmark — is what
    /// licenses replacing a per-file O(n log n) sort with one index built
    /// once per build.
    ///
    /// NON-VACUITY: the generator deliberately produces **stem collisions
    /// across extension priorities** (the same base name as `.ts`, `.js`,
    /// `.tsx`, … in different directories), which is the only case where the
    /// tie-break can be observed at all; the test asserts a floor on how
    /// often a resolution actually had ≥2 candidates to choose between.
    /// POSITIVE CONTROL: it also asserts that some specifier resolves to
    /// something and some resolves to nothing, so an implementation that
    /// always returned `None` (or always the first path) fails.
    #[test]
    fn bare_import_stem_index_agrees_with_sorted_scan() {
        let mut g = Gen::new(0x5EED_0004);
        let stems = ["foo", "bar", "baz", "index", "utils", "a"];
        let mut contended = 0;
        let mut resolved = 0;
        let mut unresolved = 0;

        for _ in 0..300 {
            // A candidate corpus with heavy stem collision across extensions
            // and directories — the tie-break's whole domain.
            let mut paths: Vec<String> = Vec::new();
            for _ in 0..(1 + g.below(24)) {
                let stem = stems[g.below(stems.len())];
                let ext = JS_TS_EXTENSIONS[g.below(JS_TS_EXTENSIONS.len())];
                let dir = match g.below(4) {
                    0 => String::new(),
                    1 => "src/".to_string(),
                    2 => "src/lib/".to_string(),
                    _ => format!("pkg{}/", g.below(3)),
                };
                paths.push(format!("{dir}{stem}{ext}"));
            }
            paths.sort();
            paths.dedup();
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();

            let index = build_stem_index(&refs);
            for _ in 0..8 {
                let source = match g.below(3) {
                    0 => stems[g.below(stems.len())].to_string(),
                    1 => format!("@scope/{}", stems[g.below(stems.len())]),
                    _ => format!("{}/sub", stems[g.below(stems.len())]),
                };
                let got = resolve_bare_import_stem(&index, &source, JS_TS_EXTENSIONS);
                let want = sorted_scan(&refs, &source);
                assert_eq!(
                    got, want,
                    "L-STEM-INDEX failed: source={source:?} candidates={refs:?}"
                );
                match got {
                    Some(_) => resolved += 1,
                    None => unresolved += 1,
                }
                let matches = refs
                    .iter()
                    .filter(|p| file_stem(p) == import_stem(&source))
                    .count();
                if matches >= 2 {
                    contended += 1;
                }
            }
        }

        assert!(
            contended > 100,
            "non-vacuity: only {contended} resolutions had a tie to break"
        );
        assert!(
            resolved > 100 && unresolved > 10,
            "positive control: resolved={resolved} unresolved={unresolved} — the invariant must \
             witness both outcomes"
        );
    }

    /// **L-STEM-INDEX at the public boundary.** The same equality observed
    /// through `js_ts_import_source_files_from_set`, which is what the save
    /// path actually calls: a file's whole resolved import list is unchanged
    /// by the swap.
    #[test]
    fn public_import_resolution_unchanged_by_stem_index() {
        let paths = [
            "src/foo.ts",
            "src/foo.js",
            "pkg/foo.tsx",
            "src/bar.js",
            "src/nested/bar.ts",
            "src/importer.ts",
        ];
        let content = r#"
            import a from 'foo';
            import b from 'bar';
            import c from './nested/bar';
            import d from 'missing';
        "#;
        let candidates = ImportCandidates::new(paths.iter().copied());
        let got = js_ts_import_source_files_from_set("src/importer.ts", content, &candidates);

        // Specification: relative specifiers resolve by membership (unchanged
        // by this commit), bare ones by `sorted_scan` above.
        let mut want: Vec<String> = Vec::new();
        for source in ["foo", "bar"] {
            if let Some(hit) = sorted_scan(&paths, source) {
                want.push(hit.to_string());
            }
        }
        want.push("src/nested/bar.ts".to_string());
        want.sort();
        want.dedup();
        sort_import_candidate_files(&mut want, JS_TS_EXTENSIONS);

        assert_eq!(got, want);
        assert!(
            got.len() >= 2,
            "non-vacuity: the fixture must resolve several imports, got {got:?}"
        );
    }
}
