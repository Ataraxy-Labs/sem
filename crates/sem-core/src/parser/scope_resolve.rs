//! Scope-aware reference resolver using tree-sitter ASTs.
//!
//! Instead of bag-of-words tokenization (current graph.rs Pass 2), this module
//! walks the tree-sitter AST to find actual reference nodes (calls, attribute access)
//! and resolves them using scope chains. This gives compiler-like accuracy for
//! name resolution without needing a full language server.
//!
//! Key improvements over bag-of-words:
//! - Distinguishes definitions from references in the AST
//! - Resolves same-name entities via scope chains (no false collisions)
//! - Tracks variable types through assignments (x = Foo() → x.method → Foo.method)
//! - Uses AST structure, not string matching

use std::cmp::Ordering;
use std::hash::BuildHasher;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::parser::resolve_profile as prof;

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::model::entity::SemanticEntity;

/// `select_member_candidate` wrapped with an opt-in timing + candidate-count
/// sample for `SEM_PROFILE_RESOLVE=1` (see `resolve_profile`). Behavior is
/// identical to calling `select_member_candidate` directly; when profiling
/// is off this expands to exactly that call with no extra work.
macro_rules! select_member_profiled {
    ($members:expr, $method:expr, $argument_labels:expr, $swift_call_signatures:expr, $type_hint:expr, $profile:expr) => {{
        if $profile.is_some() {
            let __t0 = Instant::now();
            let __sel = select_member_candidate(
                $members,
                $method,
                $argument_labels,
                $swift_call_signatures,
            );
            if let Some(acc) = $profile.as_deref_mut() {
                acc.record_method_call($type_hint, $members.len(), __t0.elapsed());
            }
            __sel
        } else {
            select_member_candidate($members, $method, $argument_labels, $swift_call_signatures)
        }
    }};
}

macro_rules! maybe_par_iter {
    ($slice:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $slice.par_iter()
        }
        #[cfg(not(feature = "parallel"))]
        {
            $slice.iter()
        }
    }};
}
use crate::parser::graph::{EntityInfo, RefType};
use crate::parser::import_resolution::{
    build_owned_stem_index, find_import_file, find_import_target, import_file_candidates,
    is_js_ts_file, js_ts_named_exports_from_content, match_bare_import_stem,
    sort_import_candidate_files, JS_TS_EXTENSIONS,
};
use crate::parser::incremental::{
    key1, key_whole, CachedScopeResult, FingerprintSink, Recorder, Table, TableFingerprints,
    ValueHasher,
};
use crate::parser::plugins::code::languages::{
    get_language_config, AssignmentStrategy, CallNodeStyle, ClassNameField, InitStrategy,
    ParamNameField, ScopeResolveConfig,
};
use crate::parser::plugins::code::{is_pathological_large_file, parse_tree};

type AttrToParamIndex<'a> = HashMap<(&'a str, &'a str), Vec<(&'a str, &'a str)>>;

/// A scope in the scope tree. Scopes are nested: module -> class -> function -> block.
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Clone, serde::Serialize)]
pub struct Scope {
    parent: Option<usize>,
    /// Definitions visible in this scope: name -> entity_id
    defs: HashMap<String, String>,
    /// Local bindings that shadow outer names but are not graph entities.
    bindings: HashSet<String>,
    /// Binding declaration rows keyed by name.
    binding_rows: HashMap<String, Vec<usize>>,
    /// Variable type bindings: var_name -> class_name (from `x = Foo()`)
    types: HashMap<String, String>,
    /// Unresolved call assignments: var_name -> function_name (from `x = func()`)
    /// These get resolved after return type analysis.
    pending_call_types: HashMap<String, String>,
    /// Unresolved field-access assignments: var_name -> (object_var, property).
    /// From `val x = obj.field`; resolved once object types and the global
    /// class field-type map are both available.
    pending_field_types: HashMap<String, (String, String)>,
    /// Which entity owns this scope (if any)
    owner_id: Option<String>,
    /// What kind of scope: "module", "class", "function"
    kind: &'static str,
}

/// `Scope::kind` is a `&'static str` compared by value throughout this module,
/// which `serde`'s derive cannot deserialize without demanding `'de: 'static` of
/// every container that holds a `Scope`. Deserializing by hand through an owned
/// shadow struct keeps the hot in-memory representation as-is (no per-scope
/// `String` allocation on the build path) while letting `PrecomputedFileFacts`
/// round-trip for the on-disk facts corpus.
///
/// The kinds this resolver produces are a closed set, so they map back to the
/// same statics. Anything else — a kind written by a future version — is leaked
/// once rather than silently coerced to a wrong kind, because coercing would
/// change resolution behavior and leaking a handful of short strings will not.
impl<'de> serde::Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct ScopeShadow {
            parent: Option<usize>,
            defs: HashMap<String, String>,
            bindings: HashSet<String>,
            binding_rows: HashMap<String, Vec<usize>>,
            types: HashMap<String, String>,
            pending_call_types: HashMap<String, String>,
            pending_field_types: HashMap<String, (String, String)>,
            owner_id: Option<String>,
            kind: String,
        }
        let s = ScopeShadow::deserialize(deserializer)?;
        Ok(Scope {
            parent: s.parent,
            defs: s.defs,
            bindings: s.bindings,
            binding_rows: s.binding_rows,
            types: s.types,
            pending_call_types: s.pending_call_types,
            pending_field_types: s.pending_field_types,
            owner_id: s.owner_id,
            kind: match s.kind.as_str() {
                "module" => "module",
                "class" => "class",
                "function" => "function",
                "block" => "block",
                _ => Box::leak(s.kind.into_boxed_str()),
            },
        })
    }
}

/// Reference found in the AST
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AstRef {
    /// Kind of reference
    kind: AstRefKind,
    /// Row (0-indexed) where this reference appears in the source
    row: usize,
    /// Byte range for the referenced syntax node in the file.
    start_byte: usize,
    end_byte: usize,
}

/// Interning-for-memory wave (Stage 1): `name`/`path`/`receiver`/
/// `method` are `Arc<str>`, not `String` — per-file-interned via
/// [`AstRefCollector`] at construction time (Stage 0 measured 72-79% of
/// `ast_refs`' duplicate string bytes as *within-file* repeats, dwarfing
/// the cross-file share, so a per-file table captures most of the
/// reclaimable duplication with none of a corpus-wide interner's
/// concurrency cost). `argument_labels` stays `String` — call-site
/// keyword-argument names, not the identifier text the duplicate-rate
/// finding named as the lever (rarely repeated within one file's own call
/// sites). Proven wire-compatible with the pre-change `String`-typed shape
/// (`examples/arc_str_wire_probe.rs`: byte-identical CBOR under `ciborium`
/// with `serde`'s `rc` feature, cross-decodes cleanly in both directions)
/// — the empirical proof and the real-corpus
/// cross-binary check this claim rests on. No `FACTS_SCHEMA_VERSION` bump.
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum AstRefKind {
    /// Bare name call: `foo()`
    Call {
        name: Arc<str>,
        argument_labels: Option<Vec<Option<String>>>,
    },
    /// Qualified path call: `module::function()`
    ScopedCall { path: Arc<str>, name: Arc<str> },
    /// Attribute call: `x.method()`
    MethodCall {
        receiver: Arc<str>,
        method: Arc<str>,
        argument_labels: Option<Vec<Option<String>>>,
    },
}

/// Per-file interning accumulator for [`AstRef`] construction (
/// Stage 1). Wraps the `Vec<AstRef>` every ref-emitting function already
/// threads through, adding a transient, file-scoped `&str -> Arc<str>`
/// table so repeated identifiers within one file share a single heap
/// allocation instead of each call site allocating its own `String`.
/// Construct one per file (`collect_all_file_refs`/
/// `fused_scope_refs_import_walk`), discard the interner via [`Self::into_refs`]
/// once that file's walk is done — deliberately no cross-file sharing, no
/// lock: Stage 0's own measurement is why (see [`AstRefKind`]'s doc
/// comment).
struct AstRefCollector {
    refs: Vec<AstRef>,
    interner: HashMap<Box<str>, Arc<str>>,
}

impl AstRefCollector {
    fn new() -> Self {
        Self {
            refs: Vec::new(),
            interner: HashMap::default(),
        }
    }

    fn intern(&mut self, s: &str) -> Arc<str> {
        if let Some(existing) = self.interner.get(s) {
            return Arc::clone(existing);
        }
        let arc: Arc<str> = Arc::from(s);
        self.interner.insert(Box::from(s), Arc::clone(&arc));
        arc
    }

    fn push_call(
        &mut self,
        name: &str,
        argument_labels: Option<Vec<Option<String>>>,
        row: usize,
        start_byte: usize,
        end_byte: usize,
    ) {
        let name = self.intern(name);
        self.refs.push(AstRef {
            kind: AstRefKind::Call {
                name,
                argument_labels,
            },
            row,
            start_byte,
            end_byte,
        });
    }

    fn push_scoped_call(
        &mut self,
        path: &str,
        name: &str,
        row: usize,
        start_byte: usize,
        end_byte: usize,
    ) {
        let path = self.intern(path);
        let name = self.intern(name);
        self.refs.push(AstRef {
            kind: AstRefKind::ScopedCall { path, name },
            row,
            start_byte,
            end_byte,
        });
    }

    fn push_method_call(
        &mut self,
        receiver: &str,
        method: &str,
        argument_labels: Option<Vec<Option<String>>>,
        row: usize,
        start_byte: usize,
        end_byte: usize,
    ) {
        let receiver = self.intern(receiver);
        let method = self.intern(method);
        self.refs.push(AstRef {
            kind: AstRefKind::MethodCall {
                receiver,
                method,
                argument_labels,
            },
            row,
            start_byte,
            end_byte,
        });
    }

    fn into_refs(self) -> Vec<AstRef> {
        self.refs
    }
}

struct SwiftCallSignature {
    argument_labels: Vec<Option<String>>,
}

enum SwiftOverloadSelection {
    Matched(String),
    NoMatch,
    NotApplicable,
}

#[derive(Clone, Copy)]
struct SourceSpan {
    start_byte: usize,
    end_byte: usize,
}

fn entity_creates_reference_scope(entity_type: &str) -> bool {
    matches!(
        entity_type,
        "function"
            | "method"
            | "constructor"
            | "init"
            | "init_declaration"
            | "class"
            | "struct"
            | "interface"
            | "impl"
            | "enum"
            | "protocol"
            | "protocol_declaration"
            | "object_declaration"
            | "companion_object"
            | "extension"
            | "module"
            | "namespace"
    )
}

/// A reference-scope child as needed to decide ref ownership: its line range and
/// (if known) byte span. Precomputed once per entity so the per-ref ownership test
/// does no HashMap lookups.
type ChildRefCheck = (usize, usize, Option<(usize, usize)>);

/// Whether `ast_ref` belongs directly to an entity (inside its span, not inside any
/// of its reference-scope children). `entity_span` and `child_ref_checks` are fetched
/// once per entity by the caller; this keeps the hot per-ref loop allocation- and
/// hash-free.
fn ref_owned_by_entity(
    ast_ref: &AstRef,
    entity_span: Option<SourceSpan>,
    child_ref_checks: &[ChildRefCheck],
) -> bool {
    if let Some(entity_span) = entity_span {
        if ast_ref.end_byte <= entity_span.start_byte || ast_ref.start_byte >= entity_span.end_byte
        {
            return false;
        }
    }

    let source_line = ast_ref.row + 1;
    child_ref_checks
        .iter()
        .all(|(child_start_line, child_end_line, child_span)| {
            if source_line < *child_start_line || source_line > *child_end_line {
                return true;
            }
            match child_span {
                Some((start_byte, end_byte)) => {
                    ast_ref.end_byte <= *start_byte || ast_ref.start_byte >= *end_byte
                }
                None => false,
            }
        })
}

fn find_entity_source_spans<'a>(
    entities: &[&'a SemanticEntity],
    source: &str,
) -> HashMap<&'a str, SourceSpan> {
    let mut spans = HashMap::default();
    let line_starts = source_line_starts(source);
    for entity in entities {
        if entity.content.is_empty() {
            continue;
        }

        if let Some(span) = find_entity_source_span(entity, source, &line_starts) {
            spans.insert(entity.id.as_str(), span);
        }
    }
    spans
}

fn source_line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' && idx + 1 < source.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

fn find_entity_source_span(
    entity: &SemanticEntity,
    source: &str,
    line_starts: &[usize],
) -> Option<SourceSpan> {
    if entity.file_path.ends_with(".swift") && entity.entity_type == "property" {
        if let Some(span) = swift_property_binding_span(entity, source.as_bytes(), line_starts) {
            return Some(span);
        }
    }

    let line_index = entity.start_line.checked_sub(1)?;
    let line_start = *line_starts.get(line_index)?;

    if let Some(span) = source_span_at(source, &entity.content, line_start) {
        return Some(span);
    }

    let line_end = line_starts
        .get(line_index + 1)
        .copied()
        .unwrap_or(source.len());
    let line = source.get(line_start..line_end)?;
    let trimmed_line_start = line_start + line.len().saturating_sub(line.trim_start().len());
    if trimmed_line_start != line_start {
        if let Some(span) = source_span_at(source, &entity.content, trimmed_line_start) {
            return Some(span);
        }
    }

    let first_content_line = entity.content.lines().next().unwrap_or("").trim_start();
    if first_content_line.is_empty() {
        return None;
    }

    for (candidate_offset, _) in line.match_indices(first_content_line) {
        if let Some(span) = source_span_at(source, &entity.content, line_start + candidate_offset) {
            return Some(span);
        }
    }

    None
}

fn source_span_at(source: &str, content: &str, start_byte: usize) -> Option<SourceSpan> {
    if source.get(start_byte..)?.starts_with(content) {
        Some(SourceSpan {
            start_byte,
            end_byte: start_byte + content.len(),
        })
    } else {
        None
    }
}

fn line_start(line_starts: &[usize], line: usize) -> usize {
    line_starts
        .get(line.saturating_sub(1))
        .copied()
        .unwrap_or(0)
}

fn line_end(line_starts: &[usize], source_len: usize, line: usize) -> usize {
    line_starts
        .get(line)
        .copied()
        .map(|offset| offset.saturating_sub(1))
        .unwrap_or(source_len)
}

fn swift_property_binding_span(
    entity: &SemanticEntity,
    source: &[u8],
    line_starts: &[usize],
) -> Option<SourceSpan> {
    let search_start = line_start(line_starts, entity.start_line);
    let search_end = line_end(line_starts, source.len(), entity.end_line).min(source.len());
    let haystack = source.get(search_start..search_end)?;
    let content = entity.content.trim();
    if !content.is_empty() {
        if let Some(local_start) = find_subslice(haystack, content.as_bytes()) {
            let start = search_start + local_start;
            return Some(SourceSpan {
                start_byte: start,
                end_byte: start + content.len(),
            });
        }
    }

    let name = entity.name.as_bytes();
    if name.is_empty() {
        return None;
    }
    let mut local_search_start = 0;
    while let Some(local_start) = find_subslice(&haystack[local_search_start..], name) {
        let local_start = local_search_start + local_start;
        let start = search_start + local_start;
        let end = start + entity.name.len();
        if !identifier_boundary(source, start, end) {
            local_search_start = local_start + name.len();
            continue;
        }
        let segment_end = swift_binding_segment_end(source, end, search_end);
        return Some(SourceSpan {
            start_byte: start,
            end_byte: segment_end,
        });
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn swift_binding_segment_end(source: &[u8], start: usize, search_end: usize) -> usize {
    let mut depth = 0usize;
    let mut idx = start;
    let mut string_delimiter: Option<u8> = None;
    while idx < search_end {
        let byte = source[idx];
        if let Some(delimiter) = string_delimiter {
            if byte == b'\\' {
                idx = (idx + 2).min(search_end);
                continue;
            }
            if byte == delimiter {
                string_delimiter = None;
            }
            idx += 1;
            continue;
        }

        match byte {
            b'"' | b'\'' => string_delimiter = Some(byte),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => return idx,
            _ => {}
        }
        idx += 1;
    }
    search_end
}

fn identifier_boundary(source: &[u8], start: usize, end: usize) -> bool {
    let before = start
        .checked_sub(1)
        .and_then(|idx| source.get(idx))
        .copied();
    let after = source.get(end).copied();
    !before.map_or(false, is_identifier_byte) && !after.map_or(false, is_identifier_byte)
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Result of scope-aware resolution
pub struct ScopeResult {
    pub edges: Vec<(String, String, RefType)>,
    /// Debug info: which references were resolved and how
    pub resolution_log: Vec<ResolutionEntry>,
}

pub(crate) struct ScopeResultFull {
    pub(crate) edges: Vec<(String, String, RefType)>,
    pub(crate) resolution_log: Vec<ResolutionEntry>,
    pub(crate) consumed_words: HashMap<String, HashSet<String>>,
}

#[derive(Clone)]
pub struct ResolutionEntry {
    pub from_entity: String,
    pub reference: String,
    pub resolved_to: Option<String>,
    pub method: &'static str, // "scope_chain", "type_tracking", "import", "unresolved", "local_binding"
}

/// Resolve references using tree-sitter scope analysis.
///
/// For each file:
/// 1. Parse with tree-sitter
/// 2. Build a scope tree (module -> class -> function)
/// 3. Walk entity AST subtrees to find reference nodes
/// 4. Resolve each reference via scope chain + type tracking
/// Pre-built lookup tables that can be shared between `EntityGraph::build()` and
/// `resolve_with_scopes()` to avoid redundant O(E) passes.
pub(crate) struct PreBuiltLookups {
    pub(crate) symbol_table: Arc<HashMap<String, Vec<String>>>,
    pub(crate) class_members: HashMap<String, Vec<(String, String)>>,
    pub(crate) owner_members: HashMap<String, Vec<(String, String)>>,
    pub(crate) entity_ranges: HashMap<String, Vec<(usize, usize, String)>>,
    /// Go package index: pkg_name → [(entity_name, entity_id, declaring_dir)]
    /// Avoids O(symbol_table) scan per Go import.
    pub(crate) go_pkg_index: GoPkgIndex,
    /// Repo-level language overrides (`.semrc`, `.gitattributes`), custom
    /// extension → canonical extension, exactly as
    /// [`crate::parser::registry::ParserRegistry::resolve_file_path`] applies
    /// them. See [`reparse_language_config`].
    pub(crate) ext_overrides: HashMap<String, String>,
}

/// The scope resolver's own admission test: `Some` iff this file's language has
/// a [`ScopeResolveConfig`], which is what every consumer of a re-parsed tree
/// requires before it looks at one.
///
/// Deliberately keyed on the *raw* extension, with no `.semrc`/`.gitattributes`
/// override applied, because that is what the per-file scope closure and the
/// return-type/init-attr scan do (see their `get_language_config(ext)` lines).
/// The re-parse's own grammar choice still honors overrides via
/// [`reparse_language_config`] — this decides *whether* a file is re-parsed at
/// all, that decides *with which grammar*, and keeping the first question in one
/// function is what stops the skip in the re-parse loop from drifting away from
/// the decline it mirrors.
fn scope_resolve_config_for_path(
    file_path: &str,
) -> Option<&'static crate::parser::plugins::code::languages::ScopeResolveConfig> {
    let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    get_language_config(ext).and_then(|c| c.scope_resolve)
}

/// Pick the grammar to re-parse `file_path` with, honoring repo-level language
/// overrides the way pass 1 did.
///
/// Pass 1 (and the non-chunked resolution path, which reuses pass 1's trees)
/// goes through `ParserRegistry::extract_entities_with_tree`, which resolves
/// `.gitattributes`/`.semrc` extension mappings before detecting the language.
/// The chunked path re-reads and re-parses the file itself; keying off the raw
/// extension there re-parsed e.g. TypeScript's `tests/baselines/reference/*.js`
/// (`*.js linguist-language=TypeScript` in its `.gitattributes`) with the
/// JavaScript grammar, while their entities had been extracted from a
/// TypeScript tree. The resulting error-recovered tree yields fewer scopes and
/// refs, so references that scope resolution should have resolved fell through
/// to the bag-of-words resolver instead — the chunked path answering a
/// different question from the direct path for the same corpus.
fn reparse_language_config(
    file_path: &str,
    ext_overrides: &HashMap<String, String>,
) -> Option<&'static crate::parser::plugins::code::languages::LanguageConfig> {
    let raw_ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    if ext_overrides.is_empty() {
        return get_language_config(raw_ext);
    }
    match ext_overrides.get(&raw_ext.to_ascii_lowercase()) {
        Some(canonical) => get_language_config(canonical),
        None => get_language_config(raw_ext),
    }
}

/// Corpus-wide entity indexes keyed by file/parent, built once from
/// `all_entities` (CUT 2). `resolve_with_scopes_full_inner` used to
/// rebuild these from scratch on every call — a pure function of `all_entities`
/// alone, so on the chunked path (one call per chunk, same `all_entities` every
/// time) that was the identical O(corpus) scan repeated once per chunk for no
/// reason. `resolve_scopes_in_file_chunks` builds this once before its chunk
/// loop and passes it to every chunk's call instead.
pub(crate) struct PrebuiltEntityIndex<'a> {
    entities_by_file: HashMap<&'a str, Vec<&'a SemanticEntity>>,
    children_by_parent: HashMap<&'a str, Vec<&'a SemanticEntity>>,
}

impl<'a> PrebuiltEntityIndex<'a> {
    pub(crate) fn build(all_entities: &'a [SemanticEntity]) -> Self {
        // `all_entities` is assembled one file at a time, so a file's entities
        // are contiguous and its bucket's final size is known as soon as the
        // run ends — building each `Vec` at its exact length avoids the
        // repeated grow-and-copy the plain `entry().or_default().push()` form
        // paid ~454k times per build. The grouping itself is
        // unchanged, and does not *rely* on contiguity: a file whose entities
        // were split across two runs simply gets two `extend`s.
        let mut entities_by_file: HashMap<&str, Vec<&SemanticEntity>> =
            HashMap::with_capacity_and_hasher(all_entities.len() / 8 + 1, Default::default());
        let mut run_start = 0usize;
        while run_start < all_entities.len() {
            let file_path = all_entities[run_start].file_path.as_str();
            let mut run_end = run_start + 1;
            while run_end < all_entities.len()
                && all_entities[run_end].file_path.as_str() == file_path
            {
                run_end += 1;
            }
            let run = &all_entities[run_start..run_end];
            match entities_by_file.get_mut(file_path) {
                Some(bucket) => bucket.extend(run.iter()),
                None => {
                    entities_by_file.insert(file_path, run.iter().collect());
                }
            }
            run_start = run_end;
        }
        let mut children_by_parent: HashMap<&str, Vec<&SemanticEntity>> =
            HashMap::with_capacity_and_hasher(all_entities.len() / 4 + 1, Default::default());
        for entity in all_entities {
            if let Some(ref pid) = entity.parent_id {
                children_by_parent
                    .entry(pid.as_str())
                    .or_default()
                    .push(entity);
            }
        }
        Self {
            entities_by_file,
            children_by_parent,
        }
    }
}

/// MUL Phase 1 (epic): the CLEAN gate, scoped to only the
/// files under adjudication instead of the whole corpus.
///
/// `graph.rs`'s CLEAN gate only ever asks for the verdict on
/// `fresh_precomputed`'s keys — pass 1's freshly-precomputed files, typically
/// a handful (linux: 7 of 2,050 scope-resolvable files) — then throws away
/// every other file's verdict via `fresh_precomputed.retain(...)`. The
/// previous implementation (`PrebuiltEntityIndex::build` +
/// `dirty_precompute_files`, removed by) computed a verdict for
/// every file in the corpus anyway, by building two corpus-wide indexes
/// (`entities_by_file`/`children_by_parent`, one `HashMap<&str, Vec<&Entity>>`
/// bucket per distinct file/parent-id in the *entire* corpus) to answer a
/// question about a handful of files.
///
/// **Soundness (why scoping to the candidate files is exact, not
/// approximate).** `CLEAN(F)` is: for every entity `e`
/// declared in `F`, every entity naming `e.id` as `parent_id` also belongs to
/// `F`. Restated as what this function computes: `F` is dirty iff `∃ e ∈
/// entities(F). ∃ c ∈ all_entities. c.parent_id == Some(e.id) ∧ c.file_path !=
/// F`. This depends on exactly two things: (1) `F`'s own entities (to know
/// which ids are "declared in `F`" at all), and (2) every entity anywhere in
/// the corpus whose `parent_id` names one of those ids — cross-file parent
/// edges *into* `F`, which by `build_entity_id`'s construction (
/// file-rootedness theorem) can only name `e.id` by having recomputed
/// the identical id string independently, since extraction is per-file and
/// never sees another file's bytes; the runtime check exists for the one hole
/// the theorem admits (id-string collision across files), not because the
/// scan needs to be corpus-wide in *scope of candidates* — it needs to be
/// corpus-wide only in *scope of who might point at a candidate*, which is
/// what pass 2 below still does. Nothing about `CLEAN(F)` for a candidate `F`
/// depends on any other file's entities, nor on a `children_by_parent` bucket
/// keyed by an id no candidate file declared — so this function builds
/// neither a corpus-wide `entities_by_file` nor a corpus-wide
/// `children_by_parent`.
///
/// **What it computes instead.** `candidate_spans` is `(file_path, start,
/// len)` into `all_entities` for exactly the files that got fresh precomputed
/// facts this round — indices `graph.rs`'s pass-1 assembly loop already knows
/// for free (the same `(path, start, len)` bookkeeping `entity_spans` does a
/// few lines above it), so pass 1 here is a direct slice over each
/// candidate's *own* entities (`Σ candidates' entity counts`, not the
/// corpus) building `id_owner : id -> file_path`, with no corpus scan at all.
/// Pass 2 is the one part that must stay `O(corpus)`: a cross-file child of a
/// candidate's entity can live in *any* file, precomputed or not, so every
/// entity's `parent_id` is checked against the small `id_owner` map (the
/// fail-safe stays exact — nothing here narrows *who might point at* a
/// candidate). Neither pass allocates a `Vec` bucket per distinct file or
/// parent id the way the old `PrebuiltEntityIndex::build` did.
///
/// This fail-safe is unaffected: empty `candidate_spans` (nothing was
/// precomputed this build) short-circuits to an empty dirty set, exactly as
/// today's `!fresh_precomputed.is_empty()` guard at the call site already
/// skips the gate entirely in that case.
///
/// **MUL phase 2.** `CLEAN(F)`'s soundness argument above depends on
/// scanning *every* cross-file parent edge into a candidate — and this crate
/// has exactly one producer of a cross-file parent edge,
/// [`crate::parser::registry::resolve_go_method_parent_ids`]. Before this
/// change, nothing enforced that it had run before this function was called;
/// `graph.rs`'s gate call site happened to run first, which was silently
/// unsound the moment a `.go` file became a candidate (
/// Go-admission hazard note). The `GoParentsResolved` parameter makes that
/// ordering a compile-time fact instead of a call-site convention: the only
/// way to obtain one is to call `resolve_go_method_parent_ids` first, so a
/// future refactor that moves this call ahead of that rewrite fails to
/// compile rather than silently regressing. It is not read — its only job is
/// to exist.
pub(crate) fn clean_gate_dirty_files(
    all_entities: &[SemanticEntity],
    candidate_spans: &[(String, usize, usize)],
    _go_parents_resolved: crate::parser::registry::GoParentsResolved,
) -> HashSet<String> {
    if candidate_spans.is_empty() {
        return HashSet::default();
    }
    let mut id_owner: HashMap<&str, &str> = HashMap::default();
    for (file_path, start, len) in candidate_spans {
        for entity in &all_entities[*start..*start + *len] {
            id_owner.insert(entity.id.as_str(), file_path.as_str());
        }
    }
    if id_owner.is_empty() {
        return HashSet::default();
    }
    let mut dirty: HashSet<String> = HashSet::default();
    for entity in all_entities {
        let Some(pid) = entity.parent_id.as_deref() else {
            continue;
        };
        if let Some(&owner_file) = id_owner.get(pid) {
            if entity.file_path.as_str() != owner_file && !dirty.contains(owner_file) {
                dirty.insert(owner_file.to_string());
            }
        }
    }
    dirty
}

struct TsDefaultExportTable {
    exports_by_file: HashMap<String, String>,
    sorted_files: Vec<String>,
}

struct TsDefaultReExport {
    file_path: String,
    original_name: String,
    module_path: String,
}

pub(crate) struct TopLevelEntityIndex {
    entities_by_file: HashMap<String, Vec<(String, String)>>,
    /// Entity-bearing files indexed by file stem, for callers that need
    /// *every* stem-matching file (bare/package specifier resolution's
    /// union-of-matches semantics — see [`match_bare_import_stem`]) rather
    /// than [`find_import_file`]'s single best match. Relative-path
    /// namespace resolution does NOT go through this index: a pure star
    /// barrel declares no entities, so those targets resolve against all
    /// parsed files instead (#478).
    stem_index: HashMap<String, Vec<String>>,
}

struct FileEntityLookup<'a> {
    by_name: HashMap<&'a str, Vec<&'a SemanticEntity>>,
}

impl<'a> FileEntityLookup<'a> {
    fn new(file_entities: &[&'a SemanticEntity]) -> Self {
        let mut by_name: HashMap<&'a str, Vec<&'a SemanticEntity>> = HashMap::default();
        for entity in file_entities {
            by_name
                .entry(entity.name.as_str())
                .or_default()
                .push(*entity);
        }
        Self { by_name }
    }

    /// First entity ID for `name` defined in this file, in entity-discovery order.
    /// Equivalent to scanning the global symbol table for same-file candidates and
    /// taking the first, but O(1) instead of O(entities-sharing-this-name).
    fn first_id_by_name(&self, name: &str) -> Option<&'a str> {
        self.by_name
            .get(name)
            .and_then(|entities| entities.first())
            .map(|entity| entity.id.as_str())
    }

    fn find_at_line<F>(
        &self,
        name: &str,
        line: usize,
        type_matches: F,
    ) -> Option<&'a SemanticEntity>
    where
        F: Fn(&SemanticEntity) -> bool,
    {
        if name.is_empty() {
            return None;
        }
        self.by_name.get(name)?.iter().find_map(|entity| {
            if entity.start_line <= line && line <= entity.end_line && type_matches(entity) {
                Some(*entity)
            } else {
                None
            }
        })
    }
}

#[derive(Default)]
struct ScopeLookupCache {
    local_bindings: HashMap<usize, HashMap<String, bool>>,
    types: HashMap<usize, HashMap<String, Option<String>>>,
    enclosing_classes: HashMap<usize, Option<String>>,
    shadow_respecting_defs: HashMap<usize, HashMap<String, ScopeChainLookup>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolutionCacheKey<'a> {
    Call {
        scope_idx: usize,
        from_entity_id: &'a str,
        name: &'a str,
        argument_labels: Option<&'a [Option<String>]>,
        allow_cross_file_calls: bool,
    },
    MethodCall {
        scope_idx: usize,
        from_entity_id: &'a str,
        receiver: &'a str,
        method: &'a str,
        argument_labels: Option<&'a [Option<String>]>,
        allow_cross_file_calls: bool,
        allow_implicit_instance_member_receiver: bool,
    },
}

fn resolution_cache_key<'a>(
    ast_ref: &'a AstRef,
    scope_idx: usize,
    from_entity_id: &'a str,
    allow_cross_file_calls: bool,
    allow_implicit_instance_member_receiver: bool,
) -> Option<ResolutionCacheKey<'a>> {
    match &ast_ref.kind {
        AstRefKind::Call {
            name,
            argument_labels,
        } => Some(ResolutionCacheKey::Call {
            scope_idx,
            from_entity_id,
            name,
            argument_labels: argument_labels.as_deref(),
            allow_cross_file_calls,
        }),
        AstRefKind::ScopedCall { .. } => None,
        AstRefKind::MethodCall {
            receiver,
            method,
            argument_labels,
        } => Some(ResolutionCacheKey::MethodCall {
            scope_idx,
            from_entity_id,
            receiver: normalized_method_receiver(receiver),
            method,
            argument_labels: argument_labels.as_deref(),
            allow_cross_file_calls,
            allow_implicit_instance_member_receiver,
        }),
    }
}

fn normalized_method_receiver(receiver: &str) -> &str {
    receiver.trim_start_matches('!').trim_start_matches('~')
}

/// The single source of truth for "does this entity type own class-shaped
/// members" — shared by [`class_member_owner_name`] (called on the owned
/// `EntityInfo` map every whole rebuild) and, since,
/// `graph::maintain_entity_lookups_incremental`'s per-file removal/insertion
/// (called on a bare `&SemanticEntity` — same fields, different owning type,
/// same predicate must apply or the two code paths could disagree about
/// which entities own members).
pub(crate) fn is_class_member_owner_type(entity_type: &str) -> bool {
    matches!(
        entity_type,
        "class"
            | "struct"
            | "interface"
            | "impl"
            | "enum"
            | "protocol"
            | "protocol_declaration"
            | "object_declaration"
            | "companion_object"
            | "extension"
    )
}

pub(crate) fn class_member_owner_name(parent: &EntityInfo) -> Option<&str> {
    is_class_member_owner_type(&parent.entity_type).then_some(parent.name.as_str())
}

fn sort_symbol_table_targets_by_source(
    symbol_table: &mut HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) {
    for target_ids in symbol_table.values_mut() {
        if target_ids.len() > 1 {
            target_ids.sort_unstable_by(|left, right| {
                compare_entity_ids_by_source(left, right, entity_map)
            });
        }
    }
}

fn compare_entity_ids_by_source(
    left: &str,
    right: &str,
    entity_map: &HashMap<String, EntityInfo>,
) -> Ordering {
    match (entity_map.get(left), entity_map.get(right)) {
        (Some(left), Some(right)) => (
            left.file_path.as_str(),
            left.start_line,
            left.end_line,
            left.id.as_str(),
        )
            .cmp(&(
                right.file_path.as_str(),
                right.start_line,
                right.end_line,
                right.id.as_str(),
            )),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// Public API that accepts caller-provided entity maps and normalizes them for resolver internals.
pub fn resolve_with_scopes(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &std::collections::HashMap<String, EntityInfo, impl BuildHasher>,
    pre_parsed: Option<Vec<(String, String, tree_sitter::Tree)>>,
) -> ScopeResult {
    let entity_map: HashMap<String, EntityInfo> = entity_map
        .iter()
        .map(|(id, entity)| (id.clone(), entity.clone()))
        .collect();
    let result = resolve_with_scopes_full(
        root,
        file_paths,
        all_entities,
        &entity_map,
        pre_parsed,
        None,
        None,
        true,
        None,
    );
    scope_result_from_full(result)
}

/// Public API for callers that already hold an Fx-hashed entity map.
pub fn resolve_with_scopes_fast(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    pre_parsed: Option<Vec<(String, String, tree_sitter::Tree)>>,
) -> ScopeResult {
    let result = resolve_with_scopes_full(
        root,
        file_paths,
        all_entities,
        entity_map,
        pre_parsed,
        None,
        None,
        true,
        None,
    );
    scope_result_from_full(result)
}

fn scope_result_from_full(result: ScopeResultFull) -> ScopeResult {
    ScopeResult {
        edges: result.edges,
        resolution_log: result.resolution_log,
    }
}

/// Internal version with pre-built lookups for performance.
pub(crate) fn resolve_with_scopes_full(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    pre_parsed: Option<Vec<(String, String, tree_sitter::Tree)>>,
    pre_built: Option<&PreBuiltLookups>,
    pre_built_import_table: Option<&HashMap<(String, String), String>>,
    emit_local_binding_log: bool,
    incremental: Option<&mut ScopeIncremental<'_, '_>>,
) -> ScopeResultFull {
    resolve_with_scopes_full_inner(
        root,
        file_paths,
        all_entities,
        entity_map,
        pre_parsed,
        pre_built,
        pre_built_import_table,
        emit_local_binding_log,
        None,
        None,
        incremental,
    )
}

pub(crate) fn resolve_with_scopes_full_for_entities(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    pre_parsed: Option<Vec<(String, String, tree_sitter::Tree)>>,
    pre_built: Option<&PreBuiltLookups>,
    pre_built_import_table: Option<&HashMap<(String, String), String>>,
    emit_entity_ids: &HashSet<&str>,
) -> ScopeResultFull {
    resolve_with_scopes_full_inner(
        root,
        file_paths,
        all_entities,
        entity_map,
        pre_parsed,
        pre_built,
        pre_built_import_table,
        false,
        Some(emit_entity_ids),
        None,
        None,
    )
}

/// One file's contribution to the four corpus-wide maps pass 1's scan builds:
/// return types by entity id, instance attribute types, `__init__` params, and
/// the attribute→param mapping.
type Pass1FileScan = (
    HashMap<String, String>,
    HashMap<(String, String), String>,
    HashMap<String, Vec<String>>,
    HashMap<(String, String), String>,
);

/// Whole-corpus state the chunked path builds once and reuses for every chunk.
///
/// `facts` are the per-file facts pass 1 already collected (see
/// [`precompute_js_ts_file_facts`]); files it covers skip the re-parse loop and
/// its downstream tree walks entirely. `entity_index` is the corpus-wide
/// file/parent entity index, a pure function of `all_entities` and therefore
/// identical for every chunk (CUT 2). `corpus_has_swift` is the same
/// kind of whole-corpus fact, added for: see that field's own doc
/// comment.
pub(crate) struct ChunkedResolveInputs<'a> {
    pub(crate) facts: &'a HashMap<String, PrecomputedFileFacts>,
    pub(crate) entity_index: &'a PrebuiltEntityIndex<'a>,
    /// Whether *any* file in the whole corpus is `.swift` — a pure function of
    /// `all_entities`, computed once before the chunk loop starts (;
    /// this struct's own doc comment on why: `entity_index` above is the same
    /// pattern for a different table).
    ///
    /// Before this fix, [`resolve_with_scopes_full_inner`]'s gate for building
    /// `swift_call_signatures` checked *this chunk's own* `parsed_files` for a
    /// `.swift` extension instead of the corpus as a whole. `build_swift_call_signatures`
    /// itself was already correct for any Swift entity outside the current
    /// chunk (its second loop falls back to re-parsing `entity.content`
    /// standalone via [`extract_swift_signature_from_entity_content`] for any
    /// entity `parsed_files` didn't cover) — the bug was purely in the guard
    /// deciding *whether to call it at all*, and that guard read chunk-local
    /// data to answer a corpus-wide question. Whether a C# file's chunk
    /// happened to also contain a `.swift` file (a function of
    /// `SCOPE_RESOLVE_BYTE_BUDGET`/chunk membership, not of that C# file's own
    /// content) decided whether `resolve_ref`'s Swift-overload-aware branch
    /// (`has_ambiguous_swift_signature_candidates`,
    /// `select_swift_overload_candidate`) ever ran for that file's calls —
    /// producing a chunk-order-dependent tie-break on corpus-wide
    /// ambiguous-short-name fallback resolution (; see
    /// "Resolver tie-break contract" section). Reading
    /// this precomputed, whole-corpus bool instead makes the gate — and
    /// therefore `swift_call_signatures`'s contents, and therefore every
    /// downstream resolution decision that consults it — a pure function of
    /// repo content, independent of `chunk_files_by_byte_budget`'s output.
    pub(crate) corpus_has_swift: bool,
    /// The JS/TS namespace-import index (`register_ts_namespace_import`),
    /// hoisted across chunks (CUT-2 pattern round 3).
    /// `build_top_level_entity_index` is a pure function of
    /// `(symbol_table, entity_map, extensions)`, and on the chunked path all
    /// three are corpus-invariant across chunks: the same `&PreBuiltLookups`
    /// and `&entity_map` are passed into every chunk's call, and the
    /// extensions are compile-time constants (`JS_TS_EXTENSIONS` here,
    /// `&[".py"]` below). Before this field the lock lived inside
    /// `resolve_with_scopes_full_inner` — created per call, i.e. per chunk —
    /// so one bare namespace import per chunk rebuilt a corpus-sized index,
    /// corpus-proportional × chunk-count (linux: ~31 thread-s across ~16
    /// chunks; see "hoist the per-chunk import-handler
    /// indexes"). Still lazy: nothing is built unless some chunk actually
    /// sees the triggering import form — just at most once per corpus now.
    pub(crate) top_level_entities: &'a OnceLock<TopLevelEntityIndex>,
    /// Python's sibling index (`register_namespace_import`, bare
    /// `import module`), same invariance argument and same hoist as
    /// `top_level_entities` above. Kept as two separate locks because the two
    /// handlers build over different extension sets (`&[".py"]` vs
    /// `JS_TS_EXTENSIONS`), exactly as before the hoist.
    pub(crate) py_top_level_entities: &'a OnceLock<TopLevelEntityIndex>,
    /// Rust's sibling index (`register_rust_module_import`, relative
    /// module-alias `use` form), same invariance argument and same hoist as
    /// `top_level_entities` above. Its own lock because it is built over its
    /// own extension set (`&[".rs"]`), exactly like the Python/TS split.
    pub(crate) rust_top_level_entities: &'a OnceLock<TopLevelEntityIndex>,
}

/// Red-green context for one call of [`resolve_with_scopes_full_inner`]
///. One call == one chunk on the chunked path, one whole corpus
/// otherwise.
///
/// The function fingerprints the chunk-scoped tables it builds (return types,
/// instance-attribute types) into `inc.cur_fp` *before* deciding which files may
/// reuse their cached edges, so the decision is taken against a fingerprint map
/// that already covers every table a file's read set can name.
pub(crate) struct ScopeIncremental<'a, 'i> {
    pub(crate) inc: &'a mut crate::parser::incremental::Incremental<'i>,
    /// Chunk index; mixed into chunk-scoped fingerprint keys so the same name in
    /// two chunks never compares against the other chunk's answer.
    pub(crate) scope_tag: u64,
    /// Files whose resolution this module knows how to attribute completely.
    /// Currently JS/TS only — see [`crate::parser::incremental::Incremental`].
    pub(crate) eligible: &'a HashSet<String>,
}

/// Chunked-path entry point (CUT 1): like [`resolve_with_scopes_full`],
/// but additionally accepts the [`ChunkedResolveInputs`] the chunk loop built
/// once. Files covered by `chunked.facts` need no tree; every other file is
/// handled exactly as before. Only `EntityGraph::build`'s chunked path
/// (`resolve_scopes_in_file_chunks`) calls this; every other caller keeps using
/// `resolve_with_scopes_full`, which passes `None` and is behaviorally
/// unchanged.
pub(crate) fn resolve_with_scopes_full_chunked(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    pre_built: Option<&PreBuiltLookups>,
    pre_built_import_table: Option<&HashMap<(String, String), String>>,
    chunked: &ChunkedResolveInputs<'_>,
    incremental: Option<&mut ScopeIncremental<'_, '_>>,
) -> ScopeResultFull {
    resolve_with_scopes_full_inner(
        root,
        file_paths,
        all_entities,
        entity_map,
        None,
        pre_built,
        pre_built_import_table,
        false,
        None,
        Some(chunked),
        incremental,
    )
}

/// Compact, tree-independent per-file facts collected during pass 1 while a
/// JS/TS file's tree-sitter tree is momentarily in hand (`EntityGraph::build`,
/// beyond `PARSED_FILE_REUSE_LIMIT`), so the chunked resolution path doesn't
/// have to re-parse the file to get them (CUT 1).
///
/// Scoped to JS/TS files only. Every *other* tree-touching computation the
/// chunked path performs — `extract_imports_from_ast`'s Python
/// (`import_from_statement`/self+cls `import_statement`), Rust
/// (`use_declaration`), and Go (`import_declaration`) branches; ctor-infer's
/// `scan_constructor_calls` (hardcoded to Python's `call` node kind); and
/// Swift call-signature building (gated on `.swift` files) — is a *structural*
/// no-op for a JS/TS AST: those node kinds never occur in the JS/TS grammars,
/// and `extract_imports_from_ast`'s own JS/TS branches
/// (`import_statement`/`export_statement`) are already gated off by
/// `skip_js_ts_imports`, which the chunked path always sets (it always
/// supplies a pre-built import table). So JS/TS files are the only files that
/// can safely skip a real tree entirely in the chunked path; every other
/// language keeps the original re-parse behavior unchanged.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = ""))]
pub struct PrecomputedFileFacts {
    /// This file's full source text (`resolve_ref`/entity-span code needs the
    /// raw bytes regardless of whether the tree stuck around).
    content: String,
    scopes: Vec<Scope>,
    entity_scope_map: HashMap<String, usize>,
    entity_inner_scope: HashMap<String, usize>,
    ast_refs: Vec<AstRef>,
    /// This file's own contribution to the corpus-wide return-type map
    /// (function/method entity id -> declared/inferred return type name).
    return_type_map: HashMap<String, String>,
    instance_attr_types: HashMap<(String, String), String>,
    init_params: HashMap<String, Vec<String>>,
    attr_to_param: HashMap<(String, String), String>,
    /// Field 10 (MUL phase 2): one descriptor per
    /// import-statement node the pruned replay would have dispatched, in
    /// that same order. Populated by [`precompute_scope_resolvable_file_facts`]
    /// via [`record_import_stmts_pruned`] — no longer always empty as of
    /// phase 2: **Python is admitted unconditionally**
    /// ([`mul_precompute_admits`]) and has real imports, so this field is
    /// populated on every active Python file. **C++ stays empty**: also
    /// admitted unconditionally, but import-free by construction of the
    /// TREELESS gate this field sits behind (a file only reaches `Some(..)`
    /// there with an empty `import_starts`). **C#/Rust/Java are
    /// populated only when their gate is flipped on** (`SEM_MUL_CSHARP`/
    /// `SEM_MUL_RUST`/`SEM_MUL_JAVA`; see [`mul_precompute_admits`]'s doc
    /// comment for why each stays gated — memory) — off by default, so
    /// empty in the common case but not provably so. **Go is admitted
    /// unconditionally** (correctness chain closed, memory
    /// check cleared), so this field is populated on every active Go file,
    /// the same shape as Python's. Always
    /// empty from [`precompute_js_ts_file_facts`] too: JS/TS imports are
    /// never replayed from a tree in a [`crate::parser::session::GraphSession`]
    /// build (`skip_js_ts_imports` is unconditionally `true` there), so
    /// recording them would produce descriptors `dispatch_import_stmt`
    /// would never consume. Pass 2's `dispatch_import_stmts_from_facts`
    /// consumes whatever lands here instead of re-parsing the tree — no
    /// changes needed at its call site regardless of which admission path
    /// populated it: this field's descriptors are the same
    /// `ImportStmtFacts` type `record_import_stmts_pruned` builds on the
    /// tree-driven path, dispatched by the same function either way.
    import_stmts: Vec<ImportStmtFacts>,
    /// Field 11 (MUL phase 2/3): one descriptor per
    /// constructor-call-shaped `"call"` node — `scan_constructor_calls`'
    /// former per-node inputs — recorded by [`record_ctor_call_sites`] in
    /// that same worklist order. Populated only when the fused walk saw a
    /// literal `"call"`-kind node *and* [`mul_precompute_consumes_calls`]
    /// admits this file's language (Python today); always empty for every
    /// other admitted language, because none of their grammars use the
    /// literal kind string `"call"` (C#'s is `invocation_expression`, C++/
    /// Rust's is `call_expression`, Go/Java's is `call_expression`/
    /// `method_invocation`) — the same structural no-op Field 10's doc
    /// comment already establishes for JS/TS. Always empty from
    /// [`precompute_js_ts_file_facts`] too, for the identical reason.
    /// Consumed by `infer_constructor_param_types` via
    /// [`apply_ctor_call_facts`] — no tree, no second traversal — exactly
    /// mirroring `import_stmts`/`dispatch_import_stmts_from_facts`'s shape.
    ctor_call_sites: Vec<CtorCallFacts>,
}

impl PrecomputedFileFacts {
    /// This file's full source text, already in hand from pass 1. Lets
    /// downstream per-file work (e.g. bag-of-words index construction)
    /// consume the same bytes pass 1 read instead of a second `read_to_string`.
    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    /// repair the id-staleness species — pass 1's per-file
    /// precompute (`precompute_scope_resolvable_file_facts`) builds this
    /// file's `entity_scope_map`/`entity_inner_scope`/`return_type_map`
    /// against the entity ids `all_entities` held for this file *at that
    /// time*. `resolve_go_method_parent_ids` runs after every file's
    /// entities are assembled and, for a Go method with a cross-file
    /// receiver type, rewrites both `parent_id` and `id` (`registry.rs`) —
    /// so a Go file's facts, already built, are keyed by the pre-rewrite
    /// id while pass 2 looks entities up by the post-rewrite id. Call this
    /// once per rewrite, scoped to exactly the files
    /// [`crate::parser::registry::GoParentsResolved`] reports as touched,
    /// before those facts reach the CLEAN gate or the session's carried
    /// store. `rekey` empty is a guaranteed no-op (checked before any field
    /// is touched) — the common case, since it is non-empty only for a
    /// build that actually contains a Go method whose receiver type lives
    /// in a different file of the same package (most corpora have none;
    /// Go's precompute path itself runs unconditionally.
    /// `registry.rs::GoParentsResolved` hands out a `std::collections::HashMap`
    /// (it has no `rustc_hash` dependency of its own), not this module's
    /// `FxHashMap` alias — spelled out fully qualified here rather than
    /// widening `registry.rs`'s public surface with a dependency it
    /// otherwise doesn't need.
    ///
    /// (a follow-up to the Go memory-check work): the original repair above missed
    /// [`Scope::defs`] and [`Scope::owner_id`] — the two other places this
    /// struct stores an *entity id* rather than a plain name string, by
    /// [`Scope`]'s own field doc comments (`defs`: "name -> entity_id",
    /// `owner_id`: "Which entity owns this scope"). Both are populated by
    /// `scope_visit_node`'s registration loops (the class-like/mod_item/
    /// function-like branches insert a child's `entity.id` into its parent
    /// scope's `.defs` and set the owning scope's `.owner_id` to the same
    /// kind of id) at precompute time — before the Go rewrite runs — and
    /// were never revisited, so a cross-file-rewritten method's nested
    /// locals kept a pre-rewrite `.defs` value pointing at an id no entity
    /// holds any more (a dangling edge target on resolve, not merely a
    /// missed lookup) and any scope `.owner_id` matching an old id likewise
    /// went stale (a missed lookup in [`lookup_owned_scope_member`], the
    /// opposite failure shape). Every *other* field on [`Scope`] holds a
    /// plain name string by its own doc comment — `bindings` (names only,
    /// no values), `binding_rows` (name -> source rows), `types` (var_name
    /// -> class_*name*, from `x = Foo()`), `pending_call_types` (var_name ->
    /// function *name*), `pending_field_types` (var_name -> (object_var,
    /// property) *names*) — none of those are ever compared against
    /// `entity_map`/`all_entities` by id, so rewriting them here would
    /// corrupt real type/binding names on any accidental string collision
    /// with a rewritten id; they are deliberately left untouched.
    pub(crate) fn rekey_entity_ids(&mut self, rekey: &std::collections::HashMap<String, String>) {
        if rekey.is_empty() {
            return;
        }
        for map in [&mut self.entity_scope_map, &mut self.entity_inner_scope] {
            let stale: Vec<String> = map
                .keys()
                .filter(|id| rekey.contains_key(id.as_str()))
                .cloned()
                .collect();
            for old_id in stale {
                if let Some(value) = map.remove(&old_id) {
                    map.insert(rekey[&old_id].clone(), value);
                }
            }
        }
        let stale_return_types: Vec<String> = self
            .return_type_map
            .keys()
            .filter(|id| rekey.contains_key(id.as_str()))
            .cloned()
            .collect();
        for old_id in stale_return_types {
            if let Some(value) = self.return_type_map.remove(&old_id) {
                self.return_type_map.insert(rekey[&old_id].clone(), value);
            }
        }
        for scope in &mut self.scopes {
            for value in scope.defs.values_mut() {
                if let Some(new_id) = rekey.get(value.as_str()) {
                    *value = new_id.clone();
                }
            }
            if let Some(new_id) = scope
                .owner_id
                .as_deref()
                .and_then(|old_id| rekey.get(old_id))
            {
                scope.owner_id = Some(new_id.clone());
            }
        }
    }

    /// Approximate heap footprint (attribution), summed across every
    /// field. Thin wrapper over [`Self::field_heap_bytes`] — see that
    /// method's doc comment for what's walked and the approximation's
    /// limits.
    pub(crate) fn approx_heap_bytes(&self) -> usize {
        self.field_heap_bytes().total()
    }

    /// Shrink every heap-owning collection to its exact `len()`, in place
    /// (Stage 2 trim). Every collection here is built by repeated
    /// `push`/`insert` over the course of one file's tree walk
    /// (`fused_scope_refs_import_walk`/`record_import_stmts_pruned`/
    /// `record_ctor_call_sites`/`scan_*`), with no `with_capacity` sizing
    /// hint — the walk cannot know its own eventual length in advance — so
    /// `Vec`/`HashMap` growth-doubling routinely leaves the last reallocation
    /// step's slack (up to ~2x for a `Vec`, similar for `HashMap`'s bucket
    /// array) sitting unused for the rest of the build once these facts are
    /// stored. Call this exactly once, right after a file's facts are fully
    /// built, before they're handed to the corpus-wide map they then live in
    /// for the remainder of the build.
    ///
    /// `shrink_to_fit` is a pure capacity operation on every collection type
    /// it is called on here — it cannot change a value, a key, insertion
    /// order, or a length — so this has zero effect on anything a
    /// correctness oracle (`edge_dump_probe`, `incr_probe`, the
    /// record-vs-direct equivalence tests, `facts_corpus_probe`) can ever
    /// observe. Safe to call unconditionally, on every admitted file
    /// regardless of language.
    pub(crate) fn shrink_to_fit(&mut self) {
        self.content.shrink_to_fit();
        for scope in &mut self.scopes {
            scope.defs.shrink_to_fit();
            scope.bindings.shrink_to_fit();
            for rows in scope.binding_rows.values_mut() {
                rows.shrink_to_fit();
            }
            scope.binding_rows.shrink_to_fit();
            scope.types.shrink_to_fit();
            scope.pending_call_types.shrink_to_fit();
            scope.pending_field_types.shrink_to_fit();
        }
        self.scopes.shrink_to_fit();
        self.entity_scope_map.shrink_to_fit();
        self.entity_inner_scope.shrink_to_fit();
        for ast_ref in &mut self.ast_refs {
            let labels = match &mut ast_ref.kind {
                AstRefKind::Call {
                    argument_labels, ..
                }
                | AstRefKind::MethodCall {
                    argument_labels, ..
                } => argument_labels.as_mut(),
                AstRefKind::ScopedCall { .. } => None,
            };
            if let Some(labels) = labels {
                labels.shrink_to_fit();
            }
        }
        self.ast_refs.shrink_to_fit();
        self.return_type_map.shrink_to_fit();
        self.instance_attr_types.shrink_to_fit();
        for params in self.init_params.values_mut() {
            params.shrink_to_fit();
        }
        self.init_params.shrink_to_fit();
        self.attr_to_param.shrink_to_fit();
        for descriptor in &mut self.import_stmts {
            match descriptor {
                ImportStmtFacts::PyFromImport { specifiers, .. } => specifiers.shrink_to_fit(),
                ImportStmtFacts::PyModuleImport { modules } => modules.shrink_to_fit(),
                ImportStmtFacts::TsImport { items, .. } => items.shrink_to_fit(),
                ImportStmtFacts::TsReExport { specifiers, .. } => specifiers.shrink_to_fit(),
                ImportStmtFacts::RustUse { .. } => {}
                ImportStmtFacts::GoImport { packages } => packages.shrink_to_fit(),
            }
        }
        self.import_stmts.shrink_to_fit();
        for site in &mut self.ctor_call_sites {
            site.arg_shapes.shrink_to_fit();
        }
        self.ctor_call_sites.shrink_to_fit();
    }

    /// Same walk as [`Self::approx_heap_bytes`], split by field instead of
    /// summed (Stage 1: attribute which field actually dominates a
    /// MUL-admitted language's fast-path facts, rather than only the
    /// aggregate). `content` is exact (`.capacity()`); the container fields
    /// are sized by `Vec`/`HashMap` capacity times element size plus a
    /// per-entry hash-table constant, and the nested `String` payloads those
    /// containers hold are walked as well: each [`Scope`]'s six internal
    /// collections' keys/values by `.capacity()`, every [`AstRefKind`]
    /// variant's `String` payloads (`argument_labels` included), the actual
    /// key+value string bytes of the return-type/instance-attr/init-param
    /// maps, and — new as of this change, closing a gap the four maps above
    /// already had fixed — every [`ImportStmtFacts`]/[`CtorCallFacts`]
    /// variant's own nested `String`/`Vec` payloads (`import_stmts`/
    /// `ctor_call_sites` previously counted only `size_of::<T>() *
    /// capacity()`, i.e. the enum/struct's stack shape, blind to the heap
    /// bytes owned by the `String`s and `Vec`s inside each variant — exactly
    /// the same class of undercount Card 2 named and fixed for the other
    /// four maps, just never extended to Fields 10/11 because nothing had
    /// measured them in isolation before). Still an approximation by
    /// `.capacity()`, not allocator-level accounting. See
    ///.
    pub(crate) fn field_heap_bytes(&self) -> PrecomputedFieldBytes {
        fn argument_labels_bytes(labels: &Option<Vec<Option<String>>>) -> usize {
            labels.as_ref().map_or(0, |labels| {
                labels.capacity() * std::mem::size_of::<Option<String>>()
                    + labels
                        .iter()
                        .filter_map(|label| label.as_ref())
                        .map(String::capacity)
                        .sum::<usize>()
            })
        }

        fn string_pair_vec_bytes(pairs: &Vec<(String, String)>) -> usize {
            pairs.capacity() * std::mem::size_of::<(String, String)>()
                + pairs
                    .iter()
                    .map(|(a, b)| a.capacity() + b.capacity())
                    .sum::<usize>()
        }

        let scopes = self.scopes.capacity() * std::mem::size_of::<Scope>()
            + self
                .scopes
                .iter()
                .map(|scope| {
                    let string_to_string =
                        |m: &HashMap<String, String>| -> usize {
                            m.iter()
                                .map(|(k, v)| k.capacity() + v.capacity())
                                .sum::<usize>()
                        };
                    string_to_string(&scope.defs)
                        + string_to_string(&scope.types)
                        + string_to_string(&scope.pending_call_types)
                        + scope
                            .bindings
                            .iter()
                            .map(String::capacity)
                            .sum::<usize>()
                        + scope.binding_rows.keys().map(String::capacity).sum::<usize>()
                        + scope
                            .pending_field_types
                            .iter()
                            .map(|(k, (a, b))| k.capacity() + a.capacity() + b.capacity())
                            .sum::<usize>()
                })
                .sum::<usize>();

        let entity_scope_maps = self.entity_scope_map.capacity()
            * (std::mem::size_of::<String>() + std::mem::size_of::<usize>() + 1)
            + self.entity_inner_scope.capacity()
                * (std::mem::size_of::<String>() + std::mem::size_of::<usize>() + 1);

        // `Arc<str>` allocations may be shared (per-file interning,
        // Stage 1) — a pointer already seen in this file's `ast_refs` costs
        // nothing more, matching what the allocator actually holds live.
        // `ARC_STR_HEADER_BYTES` is the strong+weak refcount header every
        // `Arc<str>` allocation carries ahead of its string bytes.
        const ARC_STR_HEADER_BYTES: usize = 2 * std::mem::size_of::<usize>();
        let mut seen_arc_ptrs: HashSet<usize> = HashSet::default();
        let mut arc_str_bytes = |s: &Arc<str>| -> usize {
            let ptr = Arc::as_ptr(s) as *const u8 as usize;
            if seen_arc_ptrs.insert(ptr) {
                s.len() + ARC_STR_HEADER_BYTES
            } else {
                0
            }
        };

        let ast_refs = self.ast_refs.capacity() * std::mem::size_of::<AstRef>()
            + self
                .ast_refs
                .iter()
                .map(|ast_ref| match &ast_ref.kind {
                    AstRefKind::Call {
                        name,
                        argument_labels,
                    } => arc_str_bytes(name) + argument_labels_bytes(argument_labels),
                    AstRefKind::ScopedCall { path, name } => {
                        arc_str_bytes(path) + arc_str_bytes(name)
                    }
                    AstRefKind::MethodCall {
                        receiver,
                        method,
                        argument_labels,
                    } => {
                        arc_str_bytes(receiver)
                            + arc_str_bytes(method)
                            + argument_labels_bytes(argument_labels)
                    }
                })
                .sum::<usize>();

        let return_type_map = self.return_type_map.capacity()
            * (std::mem::size_of::<String>() * 2 + 1)
            + self
                .return_type_map
                .iter()
                .map(|(k, v)| k.capacity() + v.capacity())
                .sum::<usize>();

        let instance_attr_types = self.instance_attr_types.capacity()
            * (std::mem::size_of::<(String, String)>() + std::mem::size_of::<String>() + 1)
            + self
                .instance_attr_types
                .iter()
                .map(|((class_name, attr), ty)| {
                    class_name.capacity() + attr.capacity() + ty.capacity()
                })
                .sum::<usize>();

        let init_params = self.init_params.capacity()
            * (std::mem::size_of::<String>() + std::mem::size_of::<Vec<String>>() + 1)
            + self
                .init_params
                .iter()
                .map(|(name, params)| {
                    name.capacity()
                        + params.capacity() * std::mem::size_of::<String>()
                        + params.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>();

        let attr_to_param = self.attr_to_param.capacity()
            * (std::mem::size_of::<(String, String)>() + std::mem::size_of::<String>() + 1)
            + self
                .attr_to_param
                .iter()
                .map(|((class_name, attr), param)| {
                    class_name.capacity() + attr.capacity() + param.capacity()
                })
                .sum::<usize>();

        let import_stmts = self.import_stmts.capacity() * std::mem::size_of::<ImportStmtFacts>()
            + self
                .import_stmts
                .iter()
                .map(|descriptor| match descriptor {
                    ImportStmtFacts::PyFromImport { module, specifiers } => {
                        module.capacity() + string_pair_vec_bytes(specifiers)
                    }
                    ImportStmtFacts::PyModuleImport { modules } => string_pair_vec_bytes(modules),
                    ImportStmtFacts::TsImport { source, items } => {
                        source.capacity()
                            + items.capacity() * std::mem::size_of::<TsClauseItem>()
                            + items
                                .iter()
                                .map(|item| match item {
                                    TsClauseItem::Named { original, local } => {
                                        original.capacity() + local.capacity()
                                    }
                                    TsClauseItem::Namespace { alias } => alias.capacity(),
                                    TsClauseItem::Default { name } => name.capacity(),
                                })
                                .sum::<usize>()
                    }
                    ImportStmtFacts::TsReExport { source, specifiers } => {
                        source.capacity() + string_pair_vec_bytes(specifiers)
                    }
                    ImportStmtFacts::RustUse { text } => text.capacity(),
                    ImportStmtFacts::GoImport { packages } => {
                        packages.capacity() * std::mem::size_of::<String>()
                            + packages.iter().map(String::capacity).sum::<usize>()
                    }
                })
                .sum::<usize>();

        let ctor_call_sites = self.ctor_call_sites.capacity()
            * std::mem::size_of::<CtorCallFacts>()
            + self
                .ctor_call_sites
                .iter()
                .map(|site| {
                    site.callee.capacity()
                        + site.arg_shapes.capacity() * std::mem::size_of::<Option<String>>()
                        + site
                            .arg_shapes
                            .iter()
                            .filter_map(|shape| shape.as_ref())
                            .map(String::capacity)
                            .sum::<usize>()
                })
                .sum::<usize>();

        PrecomputedFieldBytes {
            content: self.content.capacity(),
            scopes,
            entity_scope_maps,
            ast_refs,
            return_type_map,
            instance_attr_types,
            init_params,
            attr_to_param,
            import_stmts,
            ctor_call_sites,
        }
    }

    /// Stage-0 instrument (interning-for-memory wave): every
    /// identifier-shaped string `ast_refs` owns that a per-node interner
    /// would replace with a token — `Call`'s `name`, `ScopedCall`'s `path`/
    /// `name`, `MethodCall`'s `receiver`/`method`. `argument_labels`
    /// deliberately excluded: those are call-site keyword-argument names,
    /// not the identifier text the duplicate-rate finding (
    /// Stage 4) named as the lever. Diagnostic only — never called outside
    /// `mem_profile`'s `SEM_PROFILE_MEM=1` gate.
    pub(crate) fn ast_ref_intern_candidates(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(self.ast_refs.len() * 2);
        for ast_ref in &self.ast_refs {
            match &ast_ref.kind {
                AstRefKind::Call { name, .. } => out.push(name.as_ref()),
                AstRefKind::ScopedCall { path, name } => {
                    out.push(path.as_ref());
                    out.push(name.as_ref());
                }
                AstRefKind::MethodCall {
                    receiver, method, ..
                } => {
                    out.push(receiver.as_ref());
                    out.push(method.as_ref());
                }
            }
        }
        out
    }

    /// Stage-0 instrument (interning-for-memory wave): every
    /// module/specifier/path string `import_stmts` owns — the Field 10
    /// lever the Python memory-check work named but did not attempt. Diagnostic
    /// only — never called outside `mem_profile`'s `SEM_PROFILE_MEM=1` gate.
    pub(crate) fn import_stmt_intern_candidates(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(self.import_stmts.len() * 2);
        for descriptor in &self.import_stmts {
            match descriptor {
                ImportStmtFacts::PyFromImport { module, specifiers } => {
                    out.push(module.as_str());
                    for (original, local) in specifiers {
                        out.push(original.as_str());
                        out.push(local.as_str());
                    }
                }
                ImportStmtFacts::PyModuleImport { modules } => {
                    for (name, alias) in modules {
                        out.push(name.as_str());
                        out.push(alias.as_str());
                    }
                }
                ImportStmtFacts::TsImport { source, items } => {
                    out.push(source.as_str());
                    for item in items {
                        match item {
                            TsClauseItem::Named { original, local } => {
                                out.push(original.as_str());
                                out.push(local.as_str());
                            }
                            TsClauseItem::Namespace { alias } => out.push(alias.as_str()),
                            TsClauseItem::Default { name } => out.push(name.as_str()),
                        }
                    }
                }
                ImportStmtFacts::TsReExport { source, specifiers } => {
                    out.push(source.as_str());
                    for (original, local) in specifiers {
                        out.push(original.as_str());
                        out.push(local.as_str());
                    }
                }
                ImportStmtFacts::RustUse { text } => out.push(text.as_str()),
                ImportStmtFacts::GoImport { packages } => {
                    for pkg in packages {
                        out.push(pkg.as_str());
                    }
                }
            }
        }
        out
    }
}

/// Per-field split of [`PrecomputedFileFacts::field_heap_bytes`] (
/// Stage 1 attribution instrument). One field per [`PrecomputedFileFacts`]
/// field group; `total()` reproduces exactly what
/// [`PrecomputedFileFacts::approx_heap_bytes`] used to compute inline.
#[derive(Default, Clone, Copy)]
pub(crate) struct PrecomputedFieldBytes {
    pub(crate) content: usize,
    pub(crate) scopes: usize,
    pub(crate) entity_scope_maps: usize,
    pub(crate) ast_refs: usize,
    pub(crate) return_type_map: usize,
    pub(crate) instance_attr_types: usize,
    pub(crate) init_params: usize,
    pub(crate) attr_to_param: usize,
    pub(crate) import_stmts: usize,
    pub(crate) ctor_call_sites: usize,
}

impl PrecomputedFieldBytes {
    pub(crate) fn total(&self) -> usize {
        self.content
            + self.scopes
            + self.entity_scope_maps
            + self.ast_refs
            + self.return_type_map
            + self.instance_attr_types
            + self.init_params
            + self.attr_to_param
            + self.import_stmts
            + self.ctor_call_sites
    }

    pub(crate) fn add(&mut self, other: &Self) {
        self.content += other.content;
        self.scopes += other.scopes;
        self.entity_scope_maps += other.entity_scope_maps;
        self.ast_refs += other.ast_refs;
        self.return_type_map += other.return_type_map;
        self.instance_attr_types += other.instance_attr_types;
        self.init_params += other.init_params;
        self.attr_to_param += other.attr_to_param;
        self.import_stmts += other.import_stmts;
        self.ctor_call_sites += other.ctor_call_sites;
    }
}

/// A minimal, otherwise-empty [`PrecomputedFileFacts`] for tests that need a
/// `Some(...)` value to stand in for "this producer computed richer facts
/// than `None`" without exercising the real precompute walk. Every field but
/// `content` is private to this module, so this is the one constructor other
/// modules' tests (e.g. `facts_store`'s corpus round-trip tests) can use.
#[cfg(test)]
pub(crate) fn dummy_precomputed_facts_for_test(content: &str) -> PrecomputedFileFacts {
    PrecomputedFileFacts {
        content: content.to_string(),
        scopes: Vec::new(),
        entity_scope_map: HashMap::default(),
        entity_inner_scope: HashMap::default(),
        ast_refs: Vec::new(),
        return_type_map: HashMap::default(),
        instance_attr_types: HashMap::default(),
        init_params: HashMap::default(),
        attr_to_param: HashMap::default(),
        import_stmts: Vec::new(),
        ctor_call_sites: Vec::new(),
    }
}

/// Build a [`PrecomputedFileFacts`] for one JS/TS file. `entities` must be
/// exactly this file's own entities (in extraction order) — the same slice
/// `EntityGraph::build`'s pass 1 just produced for it. Returns `None` if the
/// file isn't JS/TS or has no scope-resolve config (callers should fall back
/// to the ordinary re-parse path for it).
///
/// Uses file-local substitutes for the `entity_map`/`children_by_parent` that
/// `build_scopes_from_ast` normally receives as corpus-wide maps: every
/// lookup it does against them is keyed by an id that belongs to *this* file
/// (JS/TS declarations never nest across files), so a map built from just
/// this file's entities produces identical results — verified by the
/// equivalence hash in the fix-phase notes.
pub(crate) fn precompute_js_ts_file_facts(
    file_path: &str,
    content: String,
    tree: &tree_sitter::Tree,
    entities: &[SemanticEntity],
) -> Option<PrecomputedFileFacts> {
    if !is_js_ts_file(file_path) {
        return None;
    }
    let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    let config = get_language_config(ext).and_then(|c| c.scope_resolve)?;
    let source = content.as_bytes();

    let file_entities: Vec<&SemanticEntity> = entities.iter().collect();
    let mut entity_map: HashMap<String, EntityInfo> = HashMap::default();
    let mut children_by_parent: HashMap<&str, Vec<&SemanticEntity>> = HashMap::default();
    for entity in &file_entities {
        entity_map.insert(
            entity.id.clone(),
            EntityInfo {
                id: entity.id.clone(),
                name: entity.name.clone(),
                entity_type: entity.entity_type.clone(),
                file_path: entity.file_path.clone(),
                parent_id: entity.parent_id.clone(),
                start_line: entity.start_line,
                end_line: entity.end_line,
            },
        );
        if let Some(ref pid) = entity.parent_id {
            children_by_parent
                .entry(pid.as_str())
                .or_default()
                .push(*entity);
        }
    }

    let file_lookup = FileEntityLookup::new(&file_entities);

    let mut scopes: Vec<Scope> = vec![Scope {
        parent: None,
        defs: HashMap::default(),
        bindings: HashSet::default(),
        binding_rows: HashMap::default(),
        types: HashMap::default(),
        pending_call_types: HashMap::default(),
        pending_field_types: HashMap::default(),
        owner_id: None,
        kind: "module",
    }];
    let mut entity_scope_map: HashMap<String, usize> = HashMap::default();
    let mut entity_inner_scope: HashMap<String, usize> = HashMap::default();

    // Same top-level def registration `resolve_with_scopes_full_inner`'s pass
    // 2 closure does before calling `build_scopes_from_ast` — and in
    // the *same order*: `entity_ranges` order
    // (`(start_line, end_line, id)` ascending), not extraction order. The two
    // orders agree for all but pathological same-line, same-span, 10+-way
    // name collisions (see `js_ts_precompute_seed_order_diverges_from_entity_ranges_order_at_ten_plus_siblings`
    // in this module's tests for the constructed divergence), but
    // `defs.insert` is last-write-wins, so seeding
    // in the wrong order is a silent, real divergence whenever they don't —
    // this file's own entities are cheap enough to sort per file (typically
    // tens, not corpus-wide).
    let mut top_level_by_range: Vec<&SemanticEntity> = file_entities
        .iter()
        .filter(|entity| entity.parent_id.is_none())
        .copied()
        .collect();
    top_level_by_range.sort_by(|a, b| {
        (a.start_line, a.end_line, a.id.as_str()).cmp(&(b.start_line, b.end_line, b.id.as_str()))
    });
    for entity in &top_level_by_range {
        scopes[0]
            .defs
            .insert(entity.name.clone(), entity.id.clone());
        entity_scope_map.insert(entity.id.clone(), 0);
    }

    build_scopes_from_ast(
        tree.root_node(),
        0,
        &mut scopes,
        &mut entity_scope_map,
        &mut entity_inner_scope,
        &file_lookup,
        &children_by_parent,
        &entity_map,
        source,
        config,
    );

    let mut return_type_map: HashMap<String, String> = HashMap::default();
    scan_return_types(
        tree.root_node(),
        &file_lookup,
        source,
        &mut return_type_map,
        config,
    );

    let mut instance_attr_types: HashMap<(String, String), String> = HashMap::default();
    let mut init_params: HashMap<String, Vec<String>> = HashMap::default();
    let mut attr_to_param: HashMap<(String, String), String> = HashMap::default();
    scan_init_self_attrs(
        tree.root_node(),
        source,
        &mut instance_attr_types,
        &mut init_params,
        &mut attr_to_param,
        config,
    );

    let ast_refs = collect_all_file_refs(tree.root_node(), source, config);

    let mut facts = PrecomputedFileFacts {
        content,
        scopes,
        entity_scope_map,
        entity_inner_scope,
        ast_refs,
        return_type_map,
        instance_attr_types,
        init_params,
        attr_to_param,
        // JS/TS never replays imports from a tree in a session build
        // (`skip_js_ts_imports` is unconditionally true there — see this
        // field's doc comment on the struct), so there is nothing to
        // record.
        import_stmts: Vec::new(),
        // JS/TS's call-expression node kind is never the literal `"call"`
        // Field 11 (Python-only) scans for — see this field's doc comment.
        ctor_call_sites: Vec::new(),
    };
    // Stage 2: same slack-reclaim as the scope-resolvable
    // producer's own construction site — see `shrink_to_fit`'s doc comment.
    facts.shrink_to_fit();
    Some(facts)
}

/// Whether MUL phase 1's generic precompute
/// ([`precompute_scope_resolvable_file_facts`]) is admitted for a language id.
///
/// This is the **one** place phase 1's rollout is decided, because it has two
/// consumers that must never disagree: pass 1's admission test
/// (`EntityGraph::build`'s pass-1 closure, `graph.rs`) and the facts corpus's
/// per-language salt (`facts_store::effective_language_salt`). A producer
/// switch the salt does not track is exactly the hazard the salt-bump
/// discipline exists to prevent — corpus dedup is first-writer-wins, so entries written while the
/// switch is off (`precomputed: None`) would silently deny the facts a slot
/// forever after it is turned on.
///
/// **The +15% ceiling's metric, corrected 2026-08-22:** every gate
/// below through Rust's demotion was read off `/usr/bin/time -l`'s `maximum
/// resident set size` (`getrusage` `ru_maxrss` — resident pages; pages the
/// macOS VM compressor has swapped into the compressor **vanish** from this
/// number, since they are no longer resident). The same tool also emits
/// `peak memory footprint` (`task_info` `phys_footprint` — Apple's own
/// memory-pressure accounting, which **counts** compressed pages, matching
/// what actually drives jetsam/swap/user-felt pressure). The 2026-08-22
/// re-read found
/// the two fields disagree in *sign*, not just magnitude, on the two
/// languages that had shipped unconditionally: Python's admission populates
/// exactly the compressible content (short, repetitive identifier strings)
/// the compressor eats, so maxRSS reads a small *decrease* while footprint
/// reads a large *increase*. A standalone allocator probe confirmed the
/// mechanism directly: `ps -o rss=` and maxRSS agree bit-for-bit; a process
/// holding actively-touched compressible content shows **no** divergence
/// from an equal amount of random content until the OS has actual reason to
/// reclaim those pages (mere compressibility of resident, touched memory is
/// not sufficient — inactivity/pressure is). Real corpus builds create
/// exactly that condition over their run (facts populated early, left
/// untouched while later phases run); a bounded synthetic probe deliberately
/// did not push this machine's shared, concurrently-loaded memory into that
/// regime. The ceiling is henceforth defined against **peak memory
/// footprint**, with maxRSS reported alongside for continuity; every
/// admission decided under the old metric was re-read, and verdicts flip
/// mechanically under the corrected metric — see C++'s and Python's entries
/// below, both demoted
/// by this change. This is a Darwin-specific accounting distinction
/// (`task_info`/VM-compressor semantics); Linux carries no equivalent
/// compressor by default, so Linux re-validation under this same two-field
/// discipline is a separate, still-open task.
///
/// **C++ (`cpp`): off by default, gated on `SEM_MUL_CPP`** — demoted
/// (2026-08-22) from the original unconditional "GO" verdict.
/// Correctness is not in question — nothing about the re-measurement
/// touched TREELESS/CLEAN or *what* the fast path produces, only whether its
/// memory cost clears the ceiling. Two things moved together: the
/// original same-binary reading (+5.8%/+6.5% peak-RSS against the +15%
/// ceiling, without `--no-cache` or a fresh `SEM_CACHE_DIR` per run — the
/// exact protocol gap identified from Rust's own outlier +11%
/// reading) turned out to be an artifact of that gap, not the ceiling. The
/// 2026-08-22 throwaway-worktree re-verification (one predicate flipped, `--no-cache`,
/// fresh `SEM_CACHE_DIR` per run, three order-swapped pairs on
/// llvm/llvm-project's full checkout) found peak-RSS (`maximum resident set
/// size`) itself busts the ceiling — **+19.98%/+20.50%/+21.02%** — before
/// even applying the corrected metric. Peak memory footprint (`phys_
/// footprint`, the metric that now defines the ceiling — see this
/// function's doc note above `MUL_RUNTIME_GATES`) reads even higher:
/// **+26.33%/+27.65%/+28.11%**. Both readings are unanimous in direction and
/// tight (≤1.8 points of spread across pairs), so this is not measurement
/// noise. Under the corrected metric, C++ now stays gated (`SEM_MUL_CPP`, off by default,
/// [`MUL_RUNTIME_GATES`] row with `pre_switch_salt = "ts-0.23"`) — C#'s/
/// Java's/Rust's shape, not its own original one.
///
/// **C# (`csharp`): off by default**, opt-in via `SEM_MUL_CSHARP=1`. The gate's
/// *correctness* is not in question — every correctness invariant holds, zero CLEAN violations,
/// entity/edge/edge-hash counts bit-identical. Its *memory* is:
/// dotnet-runtime measured **+21.2% and +32.9% against the same +15% ceiling,
/// reproducibly, on both pairs**. The "MUL P1" verdict
/// ("this is a STOP, not a ship") and its memory-lever follow-up ("**Unchanged
/// from the prior verdict: dotnet stays GATED** … full dotnet
/// rollout remains gated on a memory fix") are the record, and neither of
/// the two named levers survived `/usr/bin/time -l`
/// measurement. Off-by-default is what that verdict means *in code*; the switch
/// exists so the next change can re-measure without a rebuild.
///
/// Opt-in rather than opt-out for the same reason `fast_extractor`'s switch is:
/// enabling this is a **memory** decision about a specific corpus, and merely
/// upgrading `sem-core` must not make it. Phase 2/3 widen this function — not
/// the `graph.rs` call site, which is now just its consumer.
///
/// **Rust (`rust`): off by default, gated on `SEM_MUL_RUST`** — demoted
/// (2026-08-22) from MUL phase 2's original unconditional
/// verdict. Correctness is not in question — CLEAN is 100% on every
/// census corpus regardless of language (MUL-A), and TREELESS accepts
/// Rust's import-bearing files because [`mul_precompute_consumes_imports`]
/// routes them through Field 10's descriptor path (`import_stmts`) instead
/// of requiring the tree — verified bit-identical (`edge_dump_probe`
/// sha256) on rust-lang/rust and on this crate's own tree, both before and
/// after this demotion. It is **memory** that moved: shipped
/// unconditionally on a same-binary reading of **+11.16%/+11.28%** against
/// the +15% peak-RSS ceiling. A follow-up
/// pinned-baseline-vs-HEAD comparison, eight commits later, re-measured
/// **+18.2%/+21.2%** — above the ceiling — which by itself could have been a
/// binaries-eight-commits-apart artifact. A further re-verification isolated the admission
/// variable exactly (same binary at the same commit, one predicate flipped,
/// shared source tree otherwise) and got **+17.72%/+19.64%/+19.35%** across
/// three order-swapped pairs on rust-lang/rust — unanimous direction, all
/// above +15%, confirming the earlier reading was not the artifact; the
/// original +11% reading was the outlier, most plausibly because the original
/// protocol (`sem find <nonexistent>`, bare "fresh `SEM_CACHE_DIR`") did not
/// isolate `SEM_FACTS_CORPUS_DIR` the way a later
/// finding says it needed to, so the ON side plausibly benefited from
/// partial corpus warmth that suppressed its measured peak RSS. Under the corrected metric,
/// Rust now stays gated (`SEM_MUL_RUST`, off by default,
/// [`MUL_RUNTIME_GATES`] row with `pre_switch_salt = "ts-0.23"`) — C#'s/
/// Java's shape, not its own original one.
///
/// **Java (`java`): off by default, gated on `SEM_MUL_JAVA`** — MUL phase 2.
/// Correctness is clean: `edge_dump_probe` sha256
/// bit-identical ON vs OFF on elasticsearch (1,257,229 edges both sides),
/// `incr_probe` 8/8 `ORACLE ok`, `facts_probe` 4/4 `ORACLE ok`,
/// `facts_corpus_probe` cross-repo 861/861 hits + adversarial salt-clean-miss
/// proof. Java's `import_declaration` nodes were already descriptor-dispatched
/// before this change — they classify as `GoImport` (`classify_import_stmt`'s
/// doc comment: the kind is shared with Go/Swift's grammars) and resolve
/// through `register_go_package_imports`, which only ever matches
/// `.go`-suffixed entities (`build_go_pkg_index`) — a documented, pre-existing
/// no-op for Java, preserved verbatim by this
/// admission. It is **memory** that fails: `/usr/bin/time -l` on
/// elasticsearch measured **+20.97% and +21.01% against the +15% ceiling,
/// reproducibly, on both order-swapped pairs** — C#'s shape, not Rust's/C++'s.
/// Consistent with dotnet's own overshoot: Java's pre-Field-10 FASTPATH byte
/// share was the smallest of any admitted language (0.98%, MUL-A), so
/// admission moves nearly all of its bytes onto the fast path at once.
///
/// **Go (`go`): admitted unconditionally (the Go memory-check work,
/// 2026-08-22)** — the correctness blocker chain and the memory check both
/// closed cleanly, so the same no-switch precedent applies (`"go" => true`, no
/// `SEM_MUL_GO` switch, no [`MUL_RUNTIME_GATES`] row).
/// Root-cause analysis found two compounding issues behind kubernetes's non-bit-identical
/// `edge_dump_probe`: (1) `build_go_pkg_index`/`register_go_package_imports`
/// keyed packages by their bare last-path-segment string ("v1", "util", ...)
/// with no disambiguation by full import path, so common short package names
/// collided repo-wide (kubernetes has dozens of directories literally named
/// `v1`, one per API group) — latent on the AST path because type-directed
/// (`class_members`-based) resolution normally wins first, but exposed
/// whenever it didn't, silently substituting one same-named package's
/// methods for another's; (2) something about the fast path measurably
/// increases how often that type-directed resolution fails for Go
/// specifically, pushing more calls into the (formerly polluted) fallback.
///
/// The fix for (1): `GoImport::packages` now carries each import spec's
/// *full* path (not reduced to a bare segment), and
/// `register_go_package_imports` disambiguates a same-named bucket by
/// matching the import path's trailing segments against each candidate's
/// declaring directory (`select_go_pkg_candidate`) before inserting only the
/// winning candidate's entries — not the whole polluted bucket — into a
/// file's `import_table`. Proven at the unit level (RED before the fix,
/// GREEN after: a same-named-package fixture that used to resolve to the
/// wrong package's method now resolves to the importing file's own) and at
/// corpus scale: kubernetes's OFF-path edge count alone dropped from 366,905
/// to 334,664 (~9%), every one of those ~32k edges a confirmed
/// cross-package false positive (spot-checked: `cmd/kubeadm/.../types.go`'s
/// `ClusterConfiguration::DeepCopy` now correctly resolves within its own
/// package instead of to `pod-security-admission`'s). `extract_imports_ms`
/// on kubernetes fell from ~83s to ~24s (`SEM_PROFILE_RESOLVE=2`) — the old
/// code was inserting entire polluted buckets (every same-named package's
/// symbols) into every importing file's `import_table`; the fix inserts one
/// package's worth.
///
/// (2)'s mechanism was left as an honest residual — "root cause not
/// isolated in this change." A follow-up investigation (2026-08-22) found and fixed it:
/// `registry::resolve_go_method_parent_ids` — the one cross-file entity
/// rewrite this crate performs, run once `all_entities` is fully assembled
/// — rewrites a Go method's `id`/`parent_id` when its receiver type lives in
/// a *different* file, but ran *after* pass 1's precompute
/// (`precompute_scope_resolvable_file_facts`) had already keyed that file's
/// `PrecomputedFileFacts.entity_scope_map`/`entity_inner_scope`/
/// `return_type_map` by the pre-rewrite id. Pass 2 looks entities up by the
/// post-rewrite id — a clean miss, silently defaulting `scope_idx` to 0
/// (module scope) via `.unwrap_or(0)`, which is exactly the scope-blind
/// state the honest-miss backstop above (`scope_lookup_missed`) was
/// built to make safe rather than wrong-but-plausible: from scope 0,
/// `resolve_ref`'s type-directed branch (the one `in.DeepCopyInto(out)`
/// case above needs) fails to find the real local binding and the call
/// falls through to the package-qualified fallback instead — precisely the
/// `ClusterConfiguration::DeepCopy`-shaped divergence already
/// documented, just a different mechanism than the bare-segment collision
/// already fixed.
///
/// The fix has two parts, both scoped to `.go` files by construction
/// (`is_go_file` guards every mutation either makes): (a)
/// `resolve_go_method_parent_ids` now returns the old-id -> new-id map for
/// every entity it rewrites (`GoParentsResolved::rekeyed_ids`/
/// `rekeyed_files`), and the build's call site
/// (`EntityGraph::build_incremental_core`, `graph.rs`) re-keys this build's
/// fresh `PrecomputedFileFacts` for exactly the files the rewrite touched,
/// immediately after the rewrite runs and before the CLEAN gate or the
/// session's carried store ever reads them
/// (`PrecomputedFileFacts::rekey_entity_ids`); (b) the rewrite cascades a
/// method's new id down through every descendant whose `parent_id` embedded
/// the method's old id as a literal prefix (`build_entity_id`'s own
/// contract — a child's id is `format!("{parent_id}::{name}")`), since a
/// rewritten method's *own* local variables/constants/nested types were
/// left with a dangling `parent_id` otherwise (only the method's own
/// id/parent_id were being rewritten before this change).
///
/// `edge_dump_probe` ON vs OFF on kubernetes, at the current HEAD:
/// **bit-identical, 0-line diff** (was 334,664 vs 331,190, ~30,795 lines) —
/// `sha256` matches, both sides 330,558 edges, for the id-staleness
/// signature the fix targeted. Another honest residual remained: a
/// *separate* fallback residual (14.01%, `ENTITY_SCOPE_LOOKUP`'s
/// `fallback_pct`) — the same species was independently
/// being chased for TS/JS (nested entities inside a plain function never
/// entering any scope's `.defs`, via `scope_visit_node`'s function-like
/// branch never running the class-like/`mod_item` branches' registration
/// loop). A follow-up fix (2026-08-22) closed it for both languages at once —
/// the earlier attempt's regression was a pre-existing precedence bug in
/// `resolve_ref` (the `.bindings` shadow gate short-circuited before
/// `.defs` ever ran), not the registration loop itself; fixing the
/// precedence bug and reinstating the loop together collapsed Go's
/// `fallback_pct` to 0.00% with a four-corpus convergent correctness
/// signature (Go, TS/JS, Python, Rust) and no lost correct edge anywhere.
///
/// **The Go memory-check work found a third species — this one
/// inverted.** The correctness-chain-closed claim above was cross-checked
/// against `edge_dump_probe` ON vs OFF at the current HEAD before
/// trusting it for admission, and it failed: 331,120 (ON) vs 331,117 (OFF),
/// 3 edges present only under `SEM_MUL_GO=1`. Root cause, confirmed with a
/// throwaway entity-id dump (not shipped) and a scoped, reverted debug
/// print: all 3 targets are **dangling** — ids no entity in the graph
/// holds — not edges OFF was missing. `PrecomputedFileFacts::rekey_entity_
/// ids` rekeys `entity_scope_map`/`entity_inner_scope`/
/// `return_type_map`'s keys, but never revisited [`Scope::defs`]' *values*
/// or [`Scope::owner_id`] — the two other places a `Scope` caches an entity
/// id, both populated by `scope_visit_node`'s registration loops during
/// pass 1's precompute, which runs *before* the cross-file rewrite +
/// cascade. A rewritten method's nested locals kept a pre-rewrite `.defs`
/// value once the rewrite ran, surviving into a `Calls` edge whose target
/// no entity held any more — and the stale id incidentally evaded the
/// existing parent-child containment filter downstream
/// (`scope_resolve.rs`'s `is_parent_child` check: `entity_map.get` on a
/// dangling id returns `None`, so the filter fails open instead of
/// correctly suppressing the edge as containment, not a call).
/// `rekey_entity_ids` now additionally walks `self.scopes` and rewrites
/// exactly those two id-valued fields — every other `Scope` field holds a
/// plain name string by its own doc comment and is deliberately left
/// untouched. Post-fix, `edge_dump_probe` ON vs OFF on kubernetes is
/// bit-identical again (331,117 edges both sides, matching sha256); traced
/// (not assumed) that both paths now correctly resolve the 3 calls to their
/// valid rekeyed ids and both then correctly suppress them via the same
/// containment filter — ghosts dead everywhere, not new edges gained
/// anywhere. `edge_dump_probe` also grew a `DANGLING_EDGE_ORACLE` (every
/// edge endpoint must name a real entity id) so this bug class is
/// self-detecting going forward; clean (0 dangling) on kubernetes both
/// switch states plus rust-lang/rust, microsoft/TypeScript, and
/// home-assistant/core as controls, with zero edge-count movement on the
/// three non-Go corpora (`rekey_entity_ids` is a guaranteed no-op whenever
/// `rekey` is empty, which it always is absent a Go cross-file rewrite).
///
/// **The memory check.** Go's correctness blocker chain is now fully
/// closed — `SEM_MUL_GO=1` vs unset produce byte-identical graphs on every
/// corpus this project has touched — so the remaining question was purely
/// the ceiling. `/usr/bin/time -l sem graph --no-cache --json`, same
/// binary, fresh `SEM_CACHE_DIR` + isolated `SEM_FACTS_CORPUS_DIR` per run,
/// 3 order-swapped pairs on kubernetes, both `/usr/bin/time -l` fields
/// captured using the same two-field protocol used above:
///
/// | pair | order | OFF maxRSS | ON maxRSS | Δ maxRSS | OFF footprint | ON footprint | Δ footprint |
/// |---|---|---:|---:|---:|---:|---:|---:|
/// | 1 | OFF→ON | 3,217,178,624 B | 3,234,611,200 B | +0.54% | 2,838,072,416 B | 3,078,229,184 B | **+8.46%** |
/// | 2 | ON→OFF | 3,233,054,720 B | 3,214,868,480 B | -0.56% | 2,841,185,400 B | 3,048,574,096 B | **+7.30%** |
/// | 3 | OFF→ON | 3,225,468,928 B | 3,232,776,192 B | +0.23% | 2,873,560,160 B | 3,068,398,784 B | **+6.78%** |
///
/// maxRSS: flat, noise-band (-0.56% to +0.54%). Footprint: unanimous
/// **+6.78% to +8.46%**, tight (<1.7 points of spread), comfortably under
/// the +15% ceiling on every pair — Go clears both fields, unlike C++/
/// Python/Rust/Java, all of which busted the ceiling on at least one field.
/// Under the corrected metric, Go is **admitted unconditionally**: `mul_precompute_admits`
/// gained a plain `"go" => true` arm, `go_precompute_enabled`/`SEM_MUL_GO`
/// removed (no switched-off state left to preserve — the same close-out
/// precedent as above, not left as unused scaffolding), [`MUL_RUNTIME_GATES`]'s
/// "go" row deleted. `LANGUAGE_SALTS`'s go entry bumped again
/// (`rekey_entity_ids`'s new `.defs`/`owner_id` rewriting is a
/// content-only producer change, the usual salt-bump discipline) so a stale
/// pre-fix corpus entry from an earlier local `SEM_MUL_GO=1` session can
/// never silently answer a post-fix lookup now that the path is always on.
///
/// **Python (`python`): off by default, gated on `SEM_MUL_PYTHON`** —
/// demoted (2026-08-22) from MUL phase 2's original unconditional
/// verdict. Correctness is untouched by this demotion — `edge_dump_probe`
/// sha256 bit-identical ON vs OFF on home-assistant/core (310,398 edges both
/// sides), `incr_probe` 8/8 `ORACLE ok`, `facts_probe` 4/4 `ORACLE ok`,
/// `facts_corpus_probe` 109/109 hits on a real two-copy corpus, all as
/// the earlier Python measurement round already established; none of that changes here. It is **memory**,
/// again, and specifically the metric: measured **-7.95%/-7.80%** peak
/// maxRSS on home-assistant/core; a follow-up re-derivation using a corrected
/// (`--no-cache`, fresh-`SEM_CACHE_DIR`) protocol got a weaker but still
/// negative **-1.63% median** (four of five pairs negative) — both readings
/// comfortably under the old +15% maxRSS ceiling, the basis for the
/// original unconditional verdict. The 2026-08-22 re-verification re-ran that exact protocol capturing
/// *both* `/usr/bin/time -l` fields: maxRSS reproduced the earlier finding almost
/// exactly (**-1.04%/-3.99%/-1.71%**, three order-swapped pairs, still
/// negative), but peak memory footprint reads **+26.02%/+25.29%/+27.44%** —
/// unanimous, tight (≤2.2 points of spread), and well above the ceiling
/// under the corrected metric. The mechanism is exactly what this
/// function's doc note above names: Python's admission populates many
/// short, repetitive identifier strings (the fast path's `import_stmts`/
/// `ctor_call_sites` records) that the VM compressor eats once they go idle
/// mid-build, so they drop out of maxRSS's resident-page count while still
/// counting against footprint's task-level commitment — the same reason
/// admission looks like a maxRSS *win* while being a footprint *loss*. Under
/// the corrected metric, Python now stays gated (`SEM_MUL_PYTHON`, off by default,
/// [`MUL_RUNTIME_GATES`] row with `pre_switch_salt = "ts-0.23"`) — C#'s/
/// Java's/Rust's/C++'s shape, not its own original one.
///
/// The switches that remain ([`MUL_RUNTIME_GATES`]) exist so a future change
/// can re-measure/re-diagnose without a rebuild — every gated language's
/// only blocker is memory under the corrected (footprint) metric (a future
/// memory lever could promote any of them). Go's chain (correctness, then
/// memory) is closed and it no longer has a row here.
pub fn mul_precompute_admits(lang_id: &str) -> bool {
    match lang_id {
        "cpp" => cpp_precompute_enabled(),
        "csharp" => csharp_precompute_enabled(),
        "rust" => rust_precompute_enabled(),
        // Admitted unconditionally (the Go memory-check work): correctness
        // blocker chain closed (see
        // this function's own doc comment), memory check cleared (+6.78% to
        // +8.46% peak footprint, three order-swapped pairs on kubernetes,
        // comfortably under the +15% ceiling). No `SEM_MUL_GO` switch, no
        // `MUL_RUNTIME_GATES` row — the same close-out precedent as above.
        "go" => true,
        "java" => java_precompute_enabled(),
        "python" => python_precompute_enabled(),
        _ => false,
    }
}

/// Read once per process: pass 1 asks per file, inside a `par_iter`, so an
/// `env::var` per call would be a lock and an allocation on the hot path.
fn csharp_precompute_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("SEM_MUL_CSHARP").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        )
    })
}

/// MUL phase 2 follow-up: switch for Rust, same shape as
/// [`csharp_precompute_enabled`] — (`adde06a`) shipped Rust unconditionally
/// on a same-binary reading of +11.16%/+11.28% against the +15% peak-RSS
/// ceiling; a same-binary re-verification at campaign HEAD (`602dc6e`, three
/// order-swapped pairs) found +17.72%/+19.64%/+19.35% instead — consistently
/// above the ceiling. Demoted to gated. See [`mul_precompute_admits`]'s doc
/// comment.
fn rust_precompute_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("SEM_MUL_RUST").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        )
    })
}

/// MUL: switch for Java, same shape as
/// [`csharp_precompute_enabled`] — measured and left OFF: it busted its own
/// +15% peak-RSS ceiling on elasticsearch. See [`mul_precompute_admits`]'s
/// doc comment.
fn java_precompute_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("SEM_MUL_JAVA").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        )
    })
}

/// 2026-08-22 follow-up: switch for C++, same shape as
/// [`csharp_precompute_enabled`] — shipped C++ unconditionally on a
/// same-binary reading of +5.8%/+6.5% peak-RSS against the +15% ceiling,
/// measured without `--no-cache`/a fresh `SEM_CACHE_DIR` per run; the
/// corrected-protocol re-verification found peak-RSS itself re-measures at
/// +19.98%/+20.50%/+21.02%, and peak memory footprint (the now-corrected
/// metric) at +26.33%/+27.65%/+28.11% — both above the ceiling, three
/// order-swapped pairs, unanimous direction. Demoted to gated. See
/// [`mul_precompute_admits`]'s doc comment.
fn cpp_precompute_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("SEM_MUL_CPP").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        )
    })
}

/// 2026-08-22 follow-up: switch for Python, same shape as
/// [`csharp_precompute_enabled`] — shipped/reconfirmed Python
/// unconditionally on maxRSS readings of -7.95%/-7.80% then -1.63% median,
/// both comfortably under the +15% ceiling. The same protocol, capturing
/// peak memory footprint alongside maxRSS, found maxRSS still negative
/// (-1.04%/-3.99%/-1.71%) but footprint reads +26.02%/+25.29%/+27.44% —
/// above the ceiling under the corrected metric, three order-swapped
/// pairs, unanimous direction. Demoted to gated. See
/// [`mul_precompute_admits`]'s doc comment.
fn python_precompute_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("SEM_MUL_PYTHON").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        )
    })
}

/// MUL Phase 2 (MUL;): whether pass 2
/// has a real consumer for [`PrecomputedFileFacts::import_stmts`] for
/// `lang_id` — i.e. whether [`precompute_scope_resolvable_file_facts`]'s
/// TREELESS gate may accept a file whose fused walk saw import statements,
/// instead of falling back to the re-parse path for them.
///
/// Deliberately separate from [`mul_precompute_admits`], which decides
/// whether this producer runs for `lang_id` **at all**: a language can be
/// admitted to the producer while still failing TREELESS on imports, if its
/// import handlers hadn't been ported to the descriptor path yet (true of
/// every language before landed Field 10). `cpp`/`csharp` are import-free
/// by construction (census), so this predicate is never exercised for
/// them; `rust`, `go`, `java`, `python` are the four languages where it
/// actually widens TREELESS — all four ride the same six descriptor-ized
/// handlers built (`ImportStmtFacts`'s variants), so admitting a new one
/// here is a table row, not a new mechanism. Python needs both this *and*
/// [`mul_precompute_consumes_calls`] — MUL-A census found imports the
/// *larger* half of Python's tree-need on HA (16,559 of ~16,600
/// tree-needing files), `"call"` nodes the rest — before either widened
/// TREELESS, admitting Python here alone would still fail almost every real
/// HA file on the call half.
fn mul_precompute_consumes_imports(lang_id: &str) -> bool {
    matches!(lang_id, "rust" | "go" | "java" | "python")
}

/// MUL Phase 2 (MUL;): the call-node
/// sibling of [`mul_precompute_consumes_imports`] — whether pass 2 has a
/// real consumer for [`PrecomputedFileFacts::ctor_call_sites`] for `lang_id`,
/// i.e. whether the TREELESS gate may accept a file whose fused walk saw a
/// literal `"call"`-kind node instead of falling back to the re-parse path
/// for it.
///
/// Python only, by construction: `scan_constructor_calls`'s node-kind test
/// was always hardcoded to the literal string `"call"`, which is Python's
/// grammar's name for a call expression — no other grammar this crate
/// resolves ever produces a node of that literal kind (C#'s is
/// `invocation_expression`, C++/Rust/Go's is `call_expression`, Java's is
/// `method_invocation`), so the scan was always a structural no-op for
/// every other language, exactly as `PrecomputedFileFacts`'s own doc
/// comment already established for Field 10's JS/TS case. Widening this
/// predicate to any of those languages would be a dead table row, not a
/// missed admission — see the per-language `..._call_expression_stays_
/// treeless`/`..._method_invocation_stays_treeless` tests, which pin the
/// no-op directly against each grammar.
fn mul_precompute_consumes_calls(lang_id: &str) -> bool {
    matches!(lang_id, "python")
}

/// One language whose [`mul_precompute_admits`] verdict is a *runtime*
/// switch (an env var read once per process, see [`csharp_precompute_enabled`])
/// rather than a verdict fixed at compile time — paired with the salt a
/// switched-*off* build's corpus entries carry.
///
/// Two independent consumers must never disagree about which languages these
/// are: pass 1's admission test (`EntityGraph::build`, `graph.rs`) calls
/// [`mul_precompute_admits`] directly, while
/// `facts_store::producer_language_salt` needs, for each such language, the
/// salt an *off* build wrote under — so a warm corpus entry from before the
/// switch existed (or from a switched-off build, `precomputed: None`) can
/// never permanently deny the switched-on producer's richer facts a slot
/// (first-writer-wins corpus dedup).
///
/// [`MUL_RUNTIME_GATES`] is the single place that pairing is recorded now.
/// Before it existed, `facts_store::producer_language_salt` hardcoded one
/// `if lang_id == "csharp"` branch, and `examples/facts_corpus_probe.rs`
/// (an independent client simulation) hardcoded a *second*, unconnected copy
/// of the same branch and the same `"ts-0.23"` literal — two places a future
/// switch could be added to [`mul_precompute_admits`] without being added
/// to, silently reproducing the same hazard for whichever language got missed. MUL
/// phase 2/3 name Rust/Go/Java/Python as the next
/// languages `mul_precompute_admits` is expected to grow gates for, which is
/// exactly the scenario this table exists to make structural: wiring a new
/// runtime-gated language means adding one match arm to
/// [`mul_precompute_admits`] *and* one row here, both in this module, both
/// touched by the same diff — `facts_store.rs` and
/// `examples/facts_corpus_probe.rs` now both consult this table instead of
/// hand-mirroring the decision.
pub struct MulRuntimeGate {
    pub lang_id: &'static str,
    /// The salt this language's `facts_store::LANGUAGE_SALTS` table entry
    /// named *before* this switch existed — i.e. what an off-build's corpus
    /// entries (`precomputed: None`) are keyed under, so turning the switch
    /// off is a true revert (shares entries with a pre-switch binary) and
    /// turning it on isolates the richer entries from those `None`s under
    /// the table's current salt.
    pub pre_switch_salt: &'static str,
}

pub const MUL_RUNTIME_GATES: &[MulRuntimeGate] = &[
    MulRuntimeGate {
        lang_id: "csharp",
        pre_switch_salt: "ts-0.23",
    },
    // MUL: measured and stays gated — memory
    // (+20.97%/+21.01% vs +15%). See `mul_precompute_admits`'s doc comment.
    // A language promoted to unconditional has this row deleted, not left
    // stale (the same close-out precedent applied to Go below) — and the
    // reverse holds too: a language demoted back to gated
    // (rust, below) gets a row added back too.
    MulRuntimeGate {
        lang_id: "java",
        pre_switch_salt: "ts-0.23",
    },
    // MUL phase 2 follow-up: rust was unconditional at
    // (`adde06a`) but a same-binary re-verification at campaign HEAD found
    // its peak-RSS delta re-measures at +17.7-19.6%, above the +15% ceiling
    // own +11% reading had passed. `pre_switch_salt` is `"ts-0.23"` —
    // the prior salt, i.e. what a build from before Rust's admission ever
    // existed wrote under — so a switched-off build today shares corpus
    // entries with that prior world (a true revert), and the table's
    // current `"ts-0.23-mp2"` (unchanged from) becomes the switched-*on*
    // salt, isolating richer entries the same way `resolve_gated_salt`
    // already handles for java.
    MulRuntimeGate {
        lang_id: "rust",
        pre_switch_salt: "ts-0.23",
    },
    // 2026-08-22: the ceiling was redefined against peak memory
    // footprint (see `mul_precompute_admits`'s doc note above its C++
    // entry) — cpp and python were the two remaining unconditional
    // admissions, and both re-measure over the new ceiling on the
    // corrected metric even though their maxRSS readings (the metric their
    // original verdicts used) still look fine or favorable. `pre_switch_
    // salt` is `"ts-0.23"` for both — the original, prior salt, i.e. what a
    // build from before either admission ever existed wrote under — so a
    // switched-off build today shares corpus entries with that pre-
    // admission world (a true revert), and each table's current salt
    // (`"ts-0.23-mp1"` for cpp, `"ts-0.23-mp4"` for python, both unchanged
    // from their original bumps) becomes the switched-*on* salt, isolating
    // richer entries the same way `resolve_gated_salt` already handles for
    // go/java/rust.
    MulRuntimeGate {
        lang_id: "cpp",
        pre_switch_salt: "ts-0.23",
    },
    MulRuntimeGate {
        lang_id: "python",
        pre_switch_salt: "ts-0.23",
    },
];

/// MUL Phase 1 (epic;). Build a
/// [`PrecomputedFileFacts`] for one file of **any** scope-resolvable
/// language, not just JS/TS — the generic sibling of
/// [`precompute_js_ts_file_facts`], used for every other language now that
/// use-site enumeration shows the file-local/corpus-wide substitution
/// the JS/TS precompute already relies on is not JS/TS-specific (it follows
/// from `build_entity_id`'s per-file id-rooting, unconditionally — see the
/// design doc's Theorem).
///
/// Unlike the JS/TS precompute (which is unconditional because
/// `skip_js_ts_imports` is always `true` downstream), this function's
/// semantic license depends on the **structural** predicate `TREELESS(F)`:
/// whether pass 2 would need this file's tree for anything at all.
/// That is decided *from what the fused walk saw* — `import_starts` empty
/// and no literal `"call"`-kind node — rather than a per-language table,
/// so it returns `None` (falling back to the ordinary re-parse path)
/// whenever the walk saw either. `.swift` is excluded unconditionally:
/// `build_swift_call_signatures` reads corpus-wide `entity_ranges`/
/// `entity_map`, so it is not a per-file function in any sense this gate can
/// license.
///
/// `entities` must be exactly this file's own entities (in extraction
/// order), as with `precompute_js_ts_file_facts`. The **semantic** half of
/// the license (CLEAN) is *not* checked here — it cannot be: it needs the
/// corpus-wide `children_by_parent`, which does not exist until every
/// file's entities are assembled. The caller (`EntityGraph::build`'s pass 1
/// assembly) must run the CLEAN gate afterward
/// (`PrebuiltEntityIndex::dirty_precompute_files`) and drop this function's
/// output for any file it marks dirty.
pub(crate) fn precompute_scope_resolvable_file_facts(
    file_path: &str,
    content: String,
    tree: &tree_sitter::Tree,
    entities: &[SemanticEntity],
) -> Option<PrecomputedFileFacts> {
    // Swift is out of scope in every MUL phase: its one
    // tree consumer walks every tree against corpus-wide entity_ranges, not
    // a per-file function. JS/TS files never reach this function (they take
    // `precompute_js_ts_file_facts`, gated only on `is_js_ts_file`), but the
    // exclusion is stated here too so this function's own license doesn't
    // silently depend on which branch the caller happens to dispatch through.
    if file_path.ends_with(".swift") {
        return None;
    }
    let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    let lang_config = get_language_config(ext)?;
    let config = lang_config.scope_resolve?;
    let source = content.as_bytes();

    let file_entities: Vec<&SemanticEntity> = entities.iter().collect();
    let mut entity_map: HashMap<String, EntityInfo> = HashMap::default();
    let mut children_by_parent: HashMap<&str, Vec<&SemanticEntity>> = HashMap::default();
    for entity in &file_entities {
        entity_map.insert(
            entity.id.clone(),
            EntityInfo {
                id: entity.id.clone(),
                name: entity.name.clone(),
                entity_type: entity.entity_type.clone(),
                file_path: entity.file_path.clone(),
                parent_id: entity.parent_id.clone(),
                start_line: entity.start_line,
                end_line: entity.end_line,
            },
        );
        if let Some(ref pid) = entity.parent_id {
            children_by_parent
                .entry(pid.as_str())
                .or_default()
                .push(*entity);
        }
    }

    let file_lookup = FileEntityLookup::new(&file_entities);

    let mut scopes: Vec<Scope> = vec![Scope {
        parent: None,
        defs: HashMap::default(),
        bindings: HashSet::default(),
        binding_rows: HashMap::default(),
        types: HashMap::default(),
        pending_call_types: HashMap::default(),
        pending_field_types: HashMap::default(),
        owner_id: None,
        kind: "module",
    }];
    let mut entity_scope_map: HashMap<String, usize> = HashMap::default();
    let mut entity_inner_scope: HashMap<String, usize> = HashMap::default();

    // The seed order: entity_ranges order — `(start_line, end_line, id)`
    // ascending — matching the AST path
    // (`resolve_with_scopes_full_inner`'s seed loop) and
    // `precompute_js_ts_file_facts` afterward. Not extraction order.
    let mut top_level_by_range: Vec<&SemanticEntity> = file_entities
        .iter()
        .filter(|entity| entity.parent_id.is_none())
        .copied()
        .collect();
    top_level_by_range.sort_by(|a, b| {
        (a.start_line, a.end_line, a.id.as_str()).cmp(&(b.start_line, b.end_line, b.id.as_str()))
    });
    for entity in &top_level_by_range {
        scopes[0]
            .defs
            .insert(entity.name.clone(), entity.id.clone());
        entity_scope_map.insert(entity.id.clone(), 0);
    }

    // The fused triple walk: scopes/entity_scope_map/
    // entity_inner_scope (mutated in place) and ast_refs are the first four
    // `PrecomputedFileFacts` fields (the doc's "one program point"); the
    // returned `import_starts`/`saw_call_node` are exactly what decides
    // TREELESS below — structurally, from what this walk saw, not from a
    // language table.
    let (ast_refs, import_starts, saw_call_node) = fused_scope_refs_import_walk(
        tree.root_node(),
        0,
        &mut scopes,
        &mut entity_scope_map,
        &mut entity_inner_scope,
        &file_lookup,
        &children_by_parent,
        &entity_map,
        source,
        config,
    );

    // TREELESS(F): no node kind `classify_import_stmt` handles (unless
    // `lang_id` has a pass-2 consumer for the recorded descriptors — Field
    // 10, MUL phase 2, `mul_precompute_consumes_imports` below), and no
    // literal `"call"` node (unless `lang_id` has a pass-2 consumer for
    // *those* descriptors too — Field 11, MUL phase 2,
    // `mul_precompute_consumes_calls` below). Failing either means pass 2
    // would still need this file's tree for something pass 1 cannot yet
    // hand it a descriptor for, so no facts are emitted — this file falls
    // back to the re-parse path exactly as it does today. This is what
    // keeps JS/TS's own precompute (unconditional, relying on
    // `skip_js_ts_imports`) from needing to route through this function at
    // all: a JS/TS file with real imports would fail this gate, which is
    // correct for *this* function but would be wrong for JS/TS's actual
    // license.
    if (!import_starts.is_empty() && !mul_precompute_consumes_imports(lang_config.id))
        || (saw_call_node && !mul_precompute_consumes_calls(lang_config.id))
    {
        return None;
    }

    // Field 10 (MUL phase 2): record via the real
    // producer, not a hardcoded empty literal, so this line needs no change
    // when a future phase widens the TREELESS gate above to admit files
    // with real imports. `import_starts` is provably empty on every path
    // that reaches here (the gate just checked it), so this call is a
    // cheap no-op today — one `Vec::pop` off a one-element worklist,
    // pushing nothing — not dead weight kept "for later."
    // `skip_js_ts_imports: false` — this producer never runs for a JS/TS
    // file (it takes `precompute_js_ts_file_facts` instead), and no other
    // grammar it does run for emits a `TsImport`/`TsReExport`-classified
    // node, so the flag is inert here; `false` is the semantically correct
    // value (pass 1 has no chunked-vs-full notion to skip).
    let import_stmts =
        record_import_stmts_pruned(tree.root_node(), &import_starts, source, config, false);

    // Field 11 (MUL phase 2): same discipline —
    // `saw_call_node` is provably `false` here unless `mul_precompute_
    // consumes_calls` just admitted it (the gate above), so this is either a
    // one-node no-op (nothing to record) or the real Python scan.
    let ctor_call_sites = if saw_call_node {
        record_ctor_call_sites(tree.root_node(), source)
    } else {
        Vec::new()
    };

    let mut return_type_map: HashMap<String, String> = HashMap::default();
    scan_return_types(
        tree.root_node(),
        &file_lookup,
        source,
        &mut return_type_map,
        config,
    );

    let mut instance_attr_types: HashMap<(String, String), String> = HashMap::default();
    let mut init_params: HashMap<String, Vec<String>> = HashMap::default();
    let mut attr_to_param: HashMap<(String, String), String> = HashMap::default();
    scan_init_self_attrs(
        tree.root_node(),
        source,
        &mut instance_attr_types,
        &mut init_params,
        &mut attr_to_param,
        config,
    );

    let mut facts = PrecomputedFileFacts {
        content,
        scopes,
        entity_scope_map,
        entity_inner_scope,
        ast_refs,
        return_type_map,
        instance_attr_types,
        init_params,
        attr_to_param,
        import_stmts,
        ctor_call_sites,
    };
    // Stage 2: reclaim this file's push/insert-loop Vec/HashMap
    // slack before it joins the corpus-wide map it lives in for the rest of
    // the build. See `shrink_to_fit`'s own doc comment.
    facts.shrink_to_fit();
    Some(facts)
}

fn resolve_with_scopes_full_inner(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    pre_parsed: Option<Vec<(String, String, tree_sitter::Tree)>>,
    pre_built: Option<&PreBuiltLookups>,
    pre_built_import_table: Option<&HashMap<(String, String), String>>,
    emit_local_binding_log: bool,
    emit_entity_ids: Option<&HashSet<&str>>,
    chunked: Option<&ChunkedResolveInputs<'_>>,
    mut incremental: Option<&mut ScopeIncremental<'_, '_>>,
) -> ScopeResultFull {
    let precomputed_facts = chunked.map(|c| c.facts);
    let entity_index = chunked.map(|c| c.entity_index);
    let mut all_edges: Vec<(String, String, RefType)> = Vec::new();
    let mut log: Vec<ResolutionEntry> = Vec::new();
    let mut consumed_words: HashMap<String, HashSet<String>> = HashMap::default();

    // Use pre-built lookups if provided, otherwise build from scratch.
    let owned_lookups;
    let lookups = if let Some(pb) = pre_built {
        pb
    } else {
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::default();
        let mut class_members: HashMap<String, Vec<(String, String)>> = HashMap::default();
        let mut owner_members: HashMap<String, Vec<(String, String)>> = HashMap::default();
        let mut entity_ranges: HashMap<String, Vec<(usize, usize, String)>> = HashMap::default();

        for entity in all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());

            if let Some(ref pid) = entity.parent_id {
                owner_members
                    .entry(pid.clone())
                    .or_default()
                    .push((entity.name.clone(), entity.id.clone()));
                if let Some(parent) = entity_map.get(pid) {
                    if let Some(owner_name) = class_member_owner_name(parent) {
                        class_members
                            .entry(owner_name.to_string())
                            .or_default()
                            .push((entity.name.clone(), entity.id.clone()));
                    }
                }
            }

            if entity.entity_type == "method" && entity.file_path.ends_with(".go") {
                if let Some(struct_name) = extract_go_receiver_type(&entity.content) {
                    class_members
                        .entry(struct_name)
                        .or_default()
                        .push((entity.name.clone(), entity.id.clone()));
                }
            }

            entity_ranges
                .entry(entity.file_path.clone())
                .or_default()
                .push((entity.start_line, entity.end_line, entity.id.clone()));
        }
        sort_symbol_table_targets_by_source(&mut symbol_table, entity_map);
        for members in class_members.values_mut() {
            members.sort_unstable();
        }
        for members in owner_members.values_mut() {
            members.sort_unstable();
        }
        for ranges in entity_ranges.values_mut() {
            ranges.sort_unstable();
        }

        // Build Go package index for O(1) import lookup
        let go_pkg_index = build_go_pkg_index(&symbol_table, entity_map);

        owned_lookups = PreBuiltLookups {
            symbol_table: Arc::new(symbol_table),
            class_members,
            owner_members,
            entity_ranges,
            go_pkg_index,
            // No registry reaches this fallback — it exists for callers that
            // hand us entities without one — so there are no overrides to
            // honor and `reparse_language_config` keeps the raw extension.
            ext_overrides: HashMap::default(),
        };
        &owned_lookups
    };
    let symbol_table = lookups.symbol_table.as_ref();
    let class_members = &lookups.class_members;
    let owner_members = &lookups.owner_members;
    let entity_ranges = &lookups.entity_ranges;
    let go_pkg_index = &lookups.go_pkg_index;

    // File-path / parent_id indexed entity lookups. Both are a pure function
    // of `all_entities` alone (same result no matter which chunk is being
    // resolved), so when the caller already built one (CUT 2 —
    // `resolve_scopes_in_file_chunks` builds it once before its chunk loop),
    // reuse it instead of rescanning the whole corpus again on every chunk.
    let __chunk_entity_index_t0 = Instant::now();
    let owned_entity_index;
    let (entities_by_file, children_by_parent) = if let Some(idx) = entity_index {
        (&idx.entities_by_file, &idx.children_by_parent)
    } else {
        owned_entity_index = PrebuiltEntityIndex::build(all_entities);
        (
            &owned_entity_index.entities_by_file,
            &owned_entity_index.children_by_parent,
        )
    };
    prof::add_chunk_entity_index_ns(__chunk_entity_index_t0.elapsed());

    // Return type map: function_entity_id -> class_name (if function returns ClassName())
    let mut return_type_map: HashMap<String, String> = HashMap::default();

    // Instance attribute types: (class_name, attr_name) -> class_name_of_attr
    let mut instance_attr_types: HashMap<(String, String), String> = HashMap::default();

    // __init__ param info: class_name -> (ordered_params, attr_to_param mapping)
    // attr_to_param: attr_name -> param_name (for self.attr = param patterns)
    let mut init_params: HashMap<String, Vec<String>> = HashMap::default();
    let mut attr_to_param: HashMap<(String, String), String> = HashMap::default();

    // Merge pre-parsed trees with disk-parsed trees for missing files
    let mut owned_parsed_files: Vec<(String, String, tree_sitter::Tree)> = Vec::new();
    let pre_set: HashSet<String> = if let Some(pp) = pre_parsed {
        let set = pp.iter().map(|(fp, _, _)| fp.clone()).collect();
        owned_parsed_files = pp;
        set
    } else {
        HashSet::default()
    };
    // Parse any files not already in the pre-parsed set. This is the exact
    // same per-file read+parse work pass 1 already does in parallel (see
    // `EntityGraph::build`'s `maybe_par_iter!(file_paths).filter_map(..)` in
    // graph.rs) — just re-run here for files pass 1 didn't retain a tree for
    // (repos over `PARSED_FILE_REUSE_LIMIT`). Same macro, same order-preserving
    // filter_map+collect pattern, so `owned_parsed_files` ends up in exactly
    // the order the old serial `for file_path in file_paths` loop produced it
    // (pre-parsed files first, then re-parsed files in `file_paths` order).
    let __reparse_t0 = Instant::now();
    enum ReparseOutcome<'a> {
        Parsed((String, String, tree_sitter::Tree)),
        Pathological(&'a str),
    }
    let reparse_results: Vec<ReparseOutcome> = maybe_par_iter!(file_paths)
        .filter_map(|file_path| {
            if pre_set.contains(file_path) {
                return None;
            }
            // CUT 1: this file's facts were already collected in
            // pass 1 while its tree was in hand — nothing here needs a fresh
            // parse. See `PrecomputedFileFacts`.
            if precomputed_facts.is_some_and(|m| m.contains_key(file_path)) {
                return None;
            }
            //: a file whose language has no `scope_resolve`
            // config is declined by *every* consumer of this vector — the
            // per-file scope closure below opens with exactly this predicate
            // (`get_language_config(ext).and_then(|c| c.scope_resolve)?`), the
            // return-type/init-attr scan repeats it, `build_ts_default_export_
            // table` filters to JS/TS and `build_swift_call_signatures` to
            // `.swift`, all of which have one. So its tree was read, parsed and
            // dropped. Skipping the read+parse is work elimination, not a
            // semantics change: chunk membership, chunk order, the visited-chunk
            // guard and every surviving file's own facts are untouched. Measured
            // on llvm-project, 34,260 files / 267 MB of `.h`/`.c` re-parsed per
            // cold build for nothing (linux: 29,158 / 333 MB) — see
            // ": parse ceiling".
            //
            // The one consumer that does *not* repeat the predicate is
            // `infer_constructor_param_types`' `scan_constructor_calls` sweep,
            // which walks every tree in this vector. It is a provable no-op
            // unless `init_params` *and* `attr_to_param` are both non-empty
            // (its own early return), and both are built exclusively from files
            // that pass the predicate above; the gate section of records the
            // bit-identical entity/edge measurement on all five giants that
            // makes the elimination observationally sound, not just argued.
            // (`?` on the admission test itself: the grammar this file is
            // re-parsed *with* still comes from `reparse_language_config`.)
            scope_resolve_config_for_path(file_path)?;
            let full_path = root.join(file_path);
            let content = std::fs::read_to_string(&full_path).ok()?;
            let config = reparse_language_config(file_path, &lookups.ext_overrides)?;
            // shared with pass 1's `CodeParserPlugin::extract_entities_with_tree`
            // (see `plugins/code/mod.rs`'s `is_pathological_large_file` doc
            // comment) -- one shared, deterministic, pure-per-file give-up
            // predicate instead of a wall-clock race, and instead of two
            // independently-maintained copies of either. This reparse set can
            // be tens of thousands of files on a >20k-file repo
            // (dotnet-runtime's was 34,897 in one profiled run); a hybrid
            // (predicate-then-wall-clock-fallback) was tried and measured to
            // still reproduce chunk-boundary edge-count flip
            // (981,283 at a 5,000-file chunk vs 981,284 at 1,000, stable
            // across repeats) -- proof some *other* file was still racing
            // `parse_tree_within_budget`'s clock and flipping outcome with
            // it. Removing the wall-clock path entirely (this version)
            // measured bit-identical entities/edges at both chunk sizes.
            let parsed = if is_pathological_large_file(&content) {
                None
            } else {
                parse_tree(config, &content)
            };
            match parsed {
                Some(tree) => Some(ReparseOutcome::Parsed((file_path.clone(), content, tree))),
                // Pathological content shape (see `is_pathological_large_file`).
                // Degrade loudly rather than stalling the whole command on one
                // fixture file.
                None => Some(ReparseOutcome::Pathological(file_path.as_str())),
            }
        })
        .collect();
    let mut pathological: Vec<&str> = Vec::new();
    for outcome in reparse_results {
        match outcome {
            ReparseOutcome::Parsed(triple) => owned_parsed_files.push(triple),
            ReparseOutcome::Pathological(fp) => pathological.push(fp),
        }
    }
    if !pathological.is_empty() {
        let shown = pathological
            .iter()
            .take(5)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let more = pathological.len().saturating_sub(5);
        eprintln!(
            "warning: skipped cross-file reference resolution for {} file(s) with a pathologically long single line/run (see is_pathological_large_file): {}{}",
            pathological.len(),
            shown,
            if more > 0 {
                format!(", and {more} more")
            } else {
                String::new()
            }
        );
    }
    prof::add_reparse_ns(__reparse_t0.elapsed());
    let parsed_files: &[(String, String, tree_sitter::Tree)] = &owned_parsed_files;
    // the chunked (>PARSED_FILE_REUSE_LIMIT) path re-parses and
    // retains a live `(path, content, Tree)` triple per file for every
    // non-JS/TS file in this chunk (`chunked` only precomputes JS/TS facts —
    // see `PrecomputedFileFacts`'s doc comment). `content`'s bytes are
    // attributed elsewhere; the `tree_sitter::Tree` itself is opaque from
    // outside the C library (no cheap `.heap_bytes()`), so this samples the
    // *process's actual RSS* right after the chunk's trees are all live but
    // before anything in this call has had a chance to drop them — the one
    // place in the whole build this instrumentation can observe that cost
    // directly instead of estimating it.
    if crate::parser::mem_profile::enabled() {
        let content_bytes: usize = parsed_files
            .iter()
            .map(|(_, content, _)| content.len())
            .sum();
        if let Some(rss) = crate::parser::mem_profile::current_rss_bytes() {
            eprintln!(
                "SEM_PROFILE_MEM[chunk-reparse] files={} content_mb={:.1} process_rss_mb={:.1}",
                parsed_files.len(),
                content_bytes as f64 / (1024.0 * 1024.0),
                rss as f64 / (1024.0 * 1024.0)
            );
        }
    }
    let content_by_file = OnceLock::new();
    let exported_names_by_file: Mutex<HashMap<String, Arc<HashSet<String>>>> =
        Mutex::new(HashMap::default());
    // The default-export table is consulted only while resolving JS/TS imports.
    // When an import table is supplied (the graph-build path), those imports are
    // already resolved and the `TsImport`/`TsReExport` descriptor variants are
    // skipped (`dispatch_import_stmt`'s `skip_js_ts_imports` gate), so the
    // table is never read — building it would be pure waste on a large repo.
    let ts_default_exports = if pre_built_import_table.is_some() {
        TsDefaultExportTable {
            exports_by_file: HashMap::default(),
            sorted_files: Vec::new(),
        }
    } else {
        build_ts_default_export_table(parsed_files, &symbol_table, entity_map)
    };
    // on the chunked path, both import-handler indexes are the
    // caller's — owned by `resolve_scopes_in_file_chunks`, shared across
    // every chunk — because `build_top_level_entity_index` is a pure
    // function of corpus-invariant inputs there (see
    // `ChunkedResolveInputs::top_level_entities`'s doc comment). Every other
    // caller keeps a per-call lock, exactly the old behavior (one call ==
    // the whole corpus for them anyway). The py lock is sibling
    // of the TS one: same struct, built lazily (only if a `.py` bare
    // `import module` statement is actually seen) restricted to `.py` files
    // — see `build_top_level_entity_index` and `register_namespace_import`.
    let owned_top_level_entities = OnceLock::new();
    let owned_py_top_level_entities = OnceLock::new();
    let owned_rust_top_level_entities = OnceLock::new();
    let (top_level_entities, py_top_level_entities, rust_top_level_entities) = match chunked {
        Some(c) => (
            c.top_level_entities,
            c.py_top_level_entities,
            c.rust_top_level_entities,
        ),
        None => (
            &owned_top_level_entities,
            &owned_py_top_level_entities,
            &owned_rust_top_level_entities,
        ),
    };

    // Pass 1: Scan ALL files for return types and instance attr types first
    // This ensures cross-file return type info is available during resolution
    // Parallelized: each file produces local maps, then merged sequentially.
    let __pass1_t0 = Instant::now();
    let pass1_results: Vec<(
        &str,
        HashMap<String, String>,
        HashMap<(String, String), String>,
        HashMap<String, Vec<String>>,
        HashMap<(String, String), String>,
    )> = maybe_par_iter!(parsed_files)
        .filter_map(|(file_path, content, tree)| {
            let source = content.as_bytes();
            // Same admission test the re-parse loop applies before building a
            // tree at all.
            let config = scope_resolve_config_for_path(file_path)?;

            let file_entities = entities_by_file
                .get(file_path.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let file_lookup = FileEntityLookup::new(file_entities);

            let mut local_return_type_map: HashMap<String, String> = HashMap::default();
            scan_return_types(
                tree.root_node(),
                &file_lookup,
                source,
                &mut local_return_type_map,
                config,
            );

            let mut local_instance_attr_types: HashMap<(String, String), String> =
                HashMap::default();
            let mut local_init_params: HashMap<String, Vec<String>> = HashMap::default();
            let mut local_attr_to_param: HashMap<(String, String), String> = HashMap::default();
            scan_init_self_attrs(
                tree.root_node(),
                source,
                &mut local_instance_attr_types,
                &mut local_init_params,
                &mut local_attr_to_param,
                config,
            );

            Some((
                file_path.as_str(),
                local_return_type_map,
                local_instance_attr_types,
                local_init_params,
                local_attr_to_param,
            ))
        })
        .collect();

    // Merge in `file_paths` order (not "all freshly-scanned files, then all
    // precomputed files" or vice versa) so `instance_attr_types`/`init_params`/
    // `attr_to_param` — keyed by class *name*, not entity id, so two
    // same-named classes in different files can collide — see the same
    // last-write-wins overwrite order the original single `parsed_files`-order
    // merge produced, regardless of which files took the CUT 1
    // precomputed-facts path vs. the fresh re-parse+scan path.
    // (`return_type_map` is keyed by entity id, which is always unique to one
    // file, so merge order can't affect it either way.)
    let mut pass1_by_file: HashMap<&str, Pass1FileScan> = pass1_results
        .into_iter()
        .map(|(fp, rtm, iat, ip, atp)| (fp, (rtm, iat, ip, atp)))
        .collect();
    for file_path in file_paths {
        if let Some(facts) = precomputed_facts.and_then(|m| m.get(file_path)) {
            return_type_map.extend(
                facts
                    .return_type_map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            instance_attr_types.extend(
                facts
                    .instance_attr_types
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            init_params.extend(
                facts
                    .init_params
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            attr_to_param.extend(
                facts
                    .attr_to_param
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        } else if let Some((rtm, iat, ip, atp)) = pass1_by_file.remove(file_path.as_str()) {
            return_type_map.extend(rtm);
            instance_attr_types.extend(iat);
            init_params.extend(ip);
            attr_to_param.extend(atp);
        }
    }
    prof::add_pass1_scan_ns(__pass1_t0.elapsed());

    // Pass 1b: Infer constructor parameter types from call sites
    // For `Transaction(get_connection())`, infer conn param has type Connection.
    // Then resolve self.conn = conn -> (Transaction, conn) -> Connection
    let __ctor_infer_t0 = Instant::now();
    infer_constructor_param_types(
        parsed_files,
        precomputed_facts,
        file_paths,
        &return_type_map,
        &init_params,
        &attr_to_param,
        &symbol_table,
        entity_map,
        &mut instance_attr_types,
    );
    prof::add_ctor_infer_ns(__ctor_infer_t0.elapsed());
    let __return_types_by_name_t0 = Instant::now();
    let func_name_return_types =
        deterministic_return_types_by_name(&return_type_map, symbol_table, entity_map);
    prof::add_return_types_by_name_ns(__return_types_by_name_t0.elapsed());

    // on the chunked path, whether to build `swift_call_signatures`
    // at all must be a corpus-wide question, not "does *this chunk's*
    // `parsed_files` contain a `.swift` file" — see `ChunkedResolveInputs::
    // corpus_has_swift`'s doc comment for the full mechanism and why this was
    // a chunk-order-dependent tie-break. The unchunked path (`chunked` is
    // `None`, e.g. `resolve_with_scopes_full_for_entities`) already resolves
    // the whole corpus in one call, so its `parsed_files` already covers every
    // file and the old per-`parsed_files` check was already corpus-wide there
    // — unchanged, and cheaper than a redundant `all_entities` scan.
    let corpus_has_swift_file = match chunked {
        Some(c) => c.corpus_has_swift,
        None => parsed_files
            .iter()
            .any(|(file_path, _, _)| file_path.ends_with(".swift")),
    };
    let swift_call_signatures = if corpus_has_swift_file {
        build_swift_call_signatures(parsed_files, all_entities, &entity_ranges, entity_map)
    } else {
        HashMap::default()
    };

    // Group the prebuilt import table by importing file once. Otherwise every file
    // in Pass 2 would rescan the entire table to find its own entries — O(files ×
    // imports), which is quadratic on a large repo. Grouping makes each file O(its
    // own imports).
    let __import_group_t0 = Instant::now();
    let import_table_by_file: HashMap<&str, Vec<(&str, &str)>> =
        if let Some(import_table) = pre_built_import_table {
            let mut grouped: HashMap<&str, Vec<(&str, &str)>> = HashMap::default();
            for ((import_file_path, local_name), target_id) in import_table {
                grouped
                    .entry(import_file_path.as_str())
                    .or_default()
                    .push((local_name.as_str(), target_id.as_str()));
            }
            grouped
        } else {
            HashMap::default()
        };
    prof::add_import_group_ns(__import_group_t0.elapsed());

    // red-green: fingerprint every chunk-scoped table this function
    // just built, then decide — per file, against the *complete* fingerprint map
    // (the caller already added the corpus-wide tables) — which files may reuse
    // their cached edges. This must happen before the pass-2 closure runs,
    // because the closure short-circuits on the answer.
    let green_files: HashSet<&str> = match incremental.as_deref_mut() {
        None => HashSet::default(),
        Some(state) => {
            let scope_tag = state.scope_tag;
            {
                let mut sink = FingerprintSink::new(&mut state.inc.cur_fp, scope_tag);
                for (id, ty) in &return_type_map {
                    sink.one(Table::ReturnTypeMap, id, hash_of_str(ty));
                }
                for ((class_name, attr), ty) in &instance_attr_types {
                    sink.two(Table::InstanceAttrTypes, class_name, attr, hash_of_str(ty));
                }
                for (name, ty) in &func_name_return_types {
                    sink.one(Table::FuncNameReturnTypes, name, hash_of_str(ty));
                }
                // Swift call signatures flip `resolve_ref` onto a different
                // branch entirely, and that branch's candidate filtering is not
                // attributed per file — no single file's read set can name this
                // dependency, so it is fingerprinted whole (`Table`'s own doc:
                // a whole-table guard's *change* forces every eligible file
                // RED, exactly like `GuardPyWildcardImport`'s narrower cousin).
                sink.whole(
                    Table::GuardSwiftCallSignatures,
                    hash_swift_signatures(&swift_call_signatures),
                );
                // Per-file import slices. Hashed order-independently: the slice
                // is grouped out of a `HashMap`, so its `Vec` order is not
                // stable across runs, while its *content* is (keys are unique
                // per (file, name), so nothing overwrites anything).
                for (file_path, entries) in &import_table_by_file {
                    sink.one(
                        Table::ImportsForFile,
                        file_path,
                        hash_import_slice(entries.as_slice()),
                    );
                }
            }
            // A file with no import entries at all still depends on that
            // absence, and the loop above cannot record it. `unchanged()`
            // compares `Option`s, so an absent key on both sides is unchanged
            // and an appearing key is a change — exactly right.
            //
            // whether Swift call-signature ambiguity resolution
            // forces every eligible file RED must be "did the whole-table
            // guard's fingerprint *change*", not "is the table non-empty".
            // The two coincide the first time a corpus ever has a `.swift`
            // file (nothing to compare against yet — `prev_fp.get` misses,
            // `cur_fp.get` hits, so `changed` is still `true`, matching the
            // old behavior exactly on that build). They diverge on every
            // later no-op rebuild of a corpus that merely *contains* a
            // `.swift` file without its signatures having changed: a single
            // incidental fixture (vscode's one-file colorize-test corpus,
            // measured in at 13,292/13,292 files forced RED on a
            // zero-change reload) no longer permanently disables reuse for
            // every other file just because `swift_call_signatures` happens
            // to be non-empty. This is still the same whole-table-guard
            // mechanism every other guard in this module uses (`Table`'s own
            // doc: "fingerprinted whole and any change forces every file
            // RED") — the fix makes the *change* the trigger, not mere
            // presence, which is the general rule this one guard had
            // drifted from. Fails toward RED, never toward a false GREEN:
            // a missing key on either side (first build, or a `prev_fp` that
            // never tracked this table) reads as `None != Some(_)` or
            // `Some(_) != None`, i.e. `changed`, the safe direction.
            let swift_guard_key = key_whole(Table::GuardSwiftCallSignatures, scope_tag);
            let swift_signatures_changed =
                state.inc.prev_fp.get(swift_guard_key) != state.inc.cur_fp.get(swift_guard_key);
            file_paths
                .iter()
                .filter(|file_path| {
                    if swift_signatures_changed || !state.eligible.contains(file_path.as_str()) {
                        return false;
                    }
                    let Some(cached) = state.inc.cached(file_path.as_str()) else {
                        return false;
                    };
                    state.inc.may_reuse(file_path, &cached.scope.read_set)
                })
                .map(String::as_str)
                .collect()
        }
    };
    let recording = incremental.is_some();
    let cache = incremental.as_deref().map(|s| &*s.inc);
    let scope_tag = incremental.as_deref().map(|s| s.scope_tag).unwrap_or(0);

    // Pass 2: Build scopes, imports, and resolve references per file (parallel)
    let __pass2_t0 = Instant::now();
    // CUT 1: pass 2 used to always iterate `parsed_files` (which
    // meant every file needed a live tree, forcing the re-parse above for
    // anything beyond `PARSED_FILE_REUSE_LIMIT`). It now iterates
    // `file_paths` and, per file, prefers `precomputed_facts` (no tree
    // needed) over a freshly (re-)parsed tree from `parsed_files` — indexed
    // once here for O(1) lookup instead of the O(files) scan a `.find()` per
    // file would cost.
    let parsed_by_path: HashMap<&str, &(String, String, tree_sitter::Tree)> = parsed_files
        .iter()
        .map(|entry| (entry.0.as_str(), entry))
        .collect();
    let per_file_results: Vec<PerFileScopeResult> = maybe_par_iter!(file_paths)
        .filter_map(|file_path| {
            // a GREEN file's edges and consumed words are reused
            // verbatim, in the exact position in the merge order a cold build
            // would have produced them — the whole point of short-circuiting
            // *inside* this closure rather than splicing edges afterwards.
            //
            // the copy made here is the *only* one. It exists because
            // `all_edges` and the corpus-wide `consumed_words` are build-scoped
            // and consume what they are given, while the cache entry has to
            // survive into the next build — and it is made here, in the parallel
            // closure, rather than in the sequential merge below. The merge no
            // longer writes anything back for a GREEN file (`keep_scope`): the
            // entry it would have written is the entry it would have read from.
            // The read set is not copied at all any more, for the same reason.
            if green_files.contains(file_path.as_str()) {
                let cached = cache
                    .and_then(|inc| inc.cached(file_path.as_str()))
                    .expect("green implies a cached result exists");
                return Some(PerFileScopeResult {
                    file_path: file_path.as_str(),
                    edges: cached.scope.edges.clone(),
                    log: Vec::new(),
                    consumed_words: cached.scope.consumed_words.clone(),
                    read_set: None,
                    reused: true,
                });
            }
            let mut rec = if recording {
                Recorder::on(scope_tag)
            } else {
                Recorder::off()
            };
            let __prof_on = prof::enabled();
            // `names_enabled`, not `enabled`: at `SEM_PROFILE_RESOLVE=2` the
            // phase timers below stay on but this stays `None`, which is what
            // makes `select_member_profiled!` take its untimed branch and
            // removes 1.45-1.65x instrument tax from the wall time the
            // phase timers are attributing.
            let mut __file_profile: Option<prof::FileAccum> =
                prof::names_enabled().then(prof::FileAccum::default);
            let __scope_build_t0 = __prof_on.then(Instant::now);
            // Same admission test the re-parse loop applies before building a
            // tree at all.
            let config = scope_resolve_config_for_path(file_path)?;

            let precomputed = precomputed_facts.and_then(|m| m.get(file_path.as_str()));
            let reparsed = if precomputed.is_none() {
                parsed_by_path.get(file_path.as_str()).copied()
            } else {
                None
            };
            let content: &str = match (precomputed, reparsed) {
                (Some(facts), _) => facts.content.as_str(),
                (None, Some((_, content, _))) => content.as_str(),
                (None, None) => return None,
            };
            let source = content.as_bytes();

            // scope_build's own decomposition. Zero-cost when the
            // profiler is off (every `then` is a `bool` test), and every
            // sub-timer measures where its work happens rather than
            // re-deriving a share afterwards.
            let mut __sb = prof::ScopeBuildAccum::default();
            let __t = __prof_on.then(Instant::now);
            let file_entities: Vec<&SemanticEntity> = entities_by_file
                .get(file_path.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .to_vec();
            let file_lookup = FileEntityLookup::new(&file_entities);
            if let Some(t) = __t {
                __sb.entity_lookup_ns = t.elapsed().as_nanos() as u64;
                __sb.entities_spanned = file_entities.len() as u64;
            }
            let __t = __prof_on.then(Instant::now);
            let entity_spans = find_entity_source_spans(&file_entities, content);
            if let Some(t) = __t {
                __sb.entity_spans_ns = t.elapsed().as_nanos() as u64;
            }

            // `fused_import_starts` is `Some` exactly when this file took the
            // fused triple walk: the recorded import starts the
            // pruned replay below consumes instead of `extract_imports_from_ast`
            // re-walking the tree. `None` = the three-walk path or precomputed.
            let (
                mut scopes,
                entity_scope_map,
                entity_inner_scope,
                all_file_refs,
                fused_import_starts,
            ) = if let Some(facts) = precomputed {
                __sb.precomputed_path = true;
                let __t = __prof_on.then(Instant::now);
                let cloned = (
                    facts.scopes.clone(),
                    facts.entity_scope_map.clone(),
                    facts.entity_inner_scope.clone(),
                    facts.ast_refs.clone(),
                    None,
                );
                if let Some(t) = __t {
                    __sb.precomputed_clone_ns = t.elapsed().as_nanos() as u64;
                }
                cloned
            } else {
                let (_, _, tree) = reparsed.expect("checked above: content came from reparsed");
                let mut scopes: Vec<Scope> = vec![Scope {
                    parent: None,
                    defs: HashMap::default(),
                    bindings: HashSet::default(),
                    binding_rows: HashMap::default(),
                    types: HashMap::default(),
                    pending_call_types: HashMap::default(),
                    pending_field_types: HashMap::default(),
                    owner_id: None,
                    kind: "module",
                }];
                let mut entity_scope_map: HashMap<String, usize> = HashMap::default();
                let mut entity_inner_scope: HashMap<String, usize> = HashMap::default();

                if let Some(ranges) = entity_ranges.get(file_path.as_str()) {
                    for (_start, _end, eid) in ranges {
                        if let Some(info) = entity_map.get(eid) {
                            if info.parent_id.is_none() {
                                scopes[0].defs.insert(info.name.clone(), eid.clone());
                                entity_scope_map.insert(eid.clone(), 0);
                            }
                        }
                    }
                }

                // The fused triple walk is THE AST path —
                // the prototype's Python gate measured -23.6% of the box on
                // HA and was deleted. The unfused walks survive as the invariant
                // test's specification and (build_scopes/collect) as the
                // JS/TS pass-1 producer's calls.
                __sb.fused_path = true;
                let __t = __prof_on.then(Instant::now);
                let (all_file_refs, import_starts, _saw_call_node) = fused_scope_refs_import_walk(
                    tree.root_node(),
                    0,
                    &mut scopes,
                    &mut entity_scope_map,
                    &mut entity_inner_scope,
                    &file_lookup,
                    children_by_parent,
                    entity_map,
                    source,
                    config,
                );
                if let Some(t) = __t {
                    __sb.fused_walk_ns = t.elapsed().as_nanos() as u64;
                }
                let fused_import_starts = Some(import_starts);
                (
                    scopes,
                    entity_scope_map,
                    entity_inner_scope,
                    all_file_refs,
                    fused_import_starts,
                )
            };
            if __prof_on {
                __sb.scopes_built = scopes.len() as u64;
                __sb.refs_collected = all_file_refs.len() as u64;
            }

            let __t_rekey = __prof_on.then(Instant::now);
            let mut local_import_table: HashMap<(String, String), String> = HashMap::default();
            if pre_built_import_table.is_some() {
                // One record for the whole slice: every `import_table_by_name`
                // read inside `resolve_ref` is served from it, and the slice's
                // *targets* are what tie this file to other files.
                rec.one(Table::ImportsForFile, file_path.as_str());
                if let Some(entries) = import_table_by_file.get(file_path.as_str()) {
                    for (local_name, target_id) in entries {
                        local_import_table.insert(
                            (file_path.clone(), (*local_name).to_string()),
                            (*target_id).to_string(),
                        );
                        scopes[0]
                            .defs
                            .insert((*local_name).to_string(), (*target_id).to_string());
                    }
                }
            }
            if let Some(t) = __t_rekey {
                __sb.import_rekey_ns = t.elapsed().as_nanos() as u64;
            }
            let __t = __prof_on.then(Instant::now);
            // MUL Phase 2 (MUL;): a
            // precomputed file's import descriptors, if any, were already
            // recorded in pass 1 (`record_import_stmts_pruned` inside
            // `precompute_scope_resolvable_file_facts`) against the live
            // tree there — dispatch them directly here, no tree, no second
            // traversal. Always empty for C++ (import-free by TREELESS's
            // construction, MUL-A) and for JS/TS
            // (`precompute_js_ts_file_facts` never records any — see
            // `PrecomputedFileFacts::import_stmts`'s doc comment); nonzero
            // unconditionally for Python (admitted since) and Go
            // (admitted since), and nonzero for C#/Rust/Java only
            // when each's own gate (`SEM_MUL_CSHARP`/`SEM_MUL_RUST`/
            // `SEM_MUL_JAVA`) is flipped on — see
            // [`mul_precompute_admits`]'s doc comment for why each of those
            // three stays off by default.
            if let Some(facts) = precomputed {
                if !facts.import_stmts.is_empty() {
                    let skip_js_ts_imports = pre_built_import_table.is_some();
                    dispatch_import_stmts_from_facts(
                        &facts.import_stmts,
                        file_path,
                        symbol_table,
                        entity_map,
                        &mut local_import_table,
                        &mut scopes,
                        go_pkg_index,
                        &ts_default_exports,
                        top_level_entities,
                        py_top_level_entities,
                        rust_top_level_entities,
                        parsed_files,
                        &content_by_file,
                        &exported_names_by_file,
                        skip_js_ts_imports,
                        &mut rec,
                    );
                    if __prof_on {
                        __sb.precomputed_import_descriptors = facts.import_stmts.len() as u64;
                    }
                }
            } else if let (Some((_, _, tree)), Some(import_starts)) =
                (reparsed, &fused_import_starts)
            {
                // The one walk already recorded where every handled import
                // statement starts; record a descriptor per handled node in
                // extract's own order, visiting only subtrees that contain one,
                // then dispatch every descriptor (—
                // one path, record-then-dispatch, no dispatch-direct variant
                // kept alongside it). Empty (every C# file: no matching kinds)
                // ⇒ nothing to do at all.
                if !import_starts.is_empty() {
                    let skip_js_ts_imports = pre_built_import_table.is_some();
                    let descriptors = record_import_stmts_pruned(
                        tree.root_node(),
                        import_starts,
                        source,
                        config,
                        skip_js_ts_imports,
                    );
                    dispatch_import_stmts_from_facts(
                        &descriptors,
                        file_path,
                        symbol_table,
                        entity_map,
                        &mut local_import_table,
                        &mut scopes,
                        go_pkg_index,
                        &ts_default_exports,
                        top_level_entities,
                        py_top_level_entities,
                        rust_top_level_entities,
                        parsed_files,
                        &content_by_file,
                        &exported_names_by_file,
                        skip_js_ts_imports,
                        &mut rec,
                    );
                }
            }
            if let Some(t) = __t {
                __sb.extract_imports_ns = t.elapsed().as_nanos() as u64;
            }

            // The per-file import table is keyed by (file_path, name) but only ever
            // holds this file's entries, so re-key it by name once. resolve_ref then
            // looks up imports without allocating a key string per reference.
            let __t = __prof_on.then(Instant::now);
            let local_import_by_name: HashMap<&str, &str> = local_import_table
                .iter()
                .map(|((_, name), target_id)| (name.as_str(), target_id.as_str()))
                .collect();
            if let Some(t) = __t {
                __sb.import_rekey_ns += t.elapsed().as_nanos() as u64;
            }

            // Resolve pending call types using the complete return type map.
            let __t = __prof_on.then(Instant::now);
            inject_return_type_bindings(
                &mut scopes,
                &func_name_return_types,
                &return_type_map,
                &local_import_by_name,
                &mut rec,
            );
            if let Some(t) = __t {
                __sb.inject_return_types_ns = t.elapsed().as_nanos() as u64;
            }
            // Resolve `val x = obj.field` accesses against the class field-type map.
            let __t = __prof_on.then(Instant::now);
            inject_field_type_bindings(&mut scopes, &instance_attr_types, &mut rec);
            if let Some(t) = __t {
                __sb.inject_field_types_ns = t.elapsed().as_nanos() as u64;
            }
            let __scope_build_ns = __scope_build_t0.map(|t| t.elapsed().as_nanos() as u64);
            if __prof_on {
                prof::merge_scope_build(__sb);
            }

            let mut file_edges: Vec<(String, String, RefType)> = Vec::new();
            let mut file_log: Vec<ResolutionEntry> = Vec::new();
            let mut file_consumed_words: HashMap<String, HashSet<String>> = HashMap::default();

            let __ref_collect_t0 = __prof_on.then(Instant::now);
            let refs_by_row = build_refs_by_row(&all_file_refs);
            let descendant_ranges_by_entity =
                build_descendant_ranges_by_entity(&file_entities, entity_map);
            let __ref_collect_ns = __ref_collect_t0.map(|t| t.elapsed().as_nanos() as u64);
            let mut lookup_cache = ScopeLookupCache::default();
            let mut last_resolution: Option<(
                ResolutionCacheKey<'_>,
                Option<(String, RefType, &'static str)>,
            )> = None;
            let __ref_loop_t0 = __prof_on.then(Instant::now);
            let mut __resolve_ref_ns: u64 = 0;
            let mut __cache_hit: u64 = 0;
            let mut __cache_miss: u64 = 0;

            for entity in &file_entities {
                if emit_entity_ids
                    .as_ref()
                    .is_some_and(|ids| !ids.contains(entity.id.as_str()))
                {
                    continue;
                }

                // One lookup chain, kept single-pass: `scope_lookup_missed`
                // is true exactly when BOTH maps missed and `scope_idx` below
                // is the `unwrap_or(0)` default — not when an entity
                // legitimately resolves to scope 0. Threaded into `resolve_ref`
                // (where the Go package-qualified fallback must not guess from
                // an unknown scope context) and counted here for precomputed
                // files, where a miss means the facts clone was incomplete.
                let scope_idx_lookup = entity_inner_scope
                    .get(&entity.id)
                    .or_else(|| entity_scope_map.get(&entity.id))
                    .copied();
                let scope_lookup_missed = scope_idx_lookup.is_none();
                let scope_idx = scope_idx_lookup.unwrap_or(0);
                if __prof_on && precomputed.is_some() {
                    prof::add_entity_scope_lookup(scope_lookup_missed);
                }

                let start_row = entity.start_line.saturating_sub(1).min(refs_by_row.len());
                let end_row = entity.end_line.min(refs_by_row.len()).max(start_row);
                if emit_local_binding_log {
                    log_scope_bindings(
                        &mut file_log,
                        &entity.id,
                        &scopes[scope_idx],
                        start_row,
                        end_row,
                        &descendant_ranges_by_entity,
                    );
                }
                // Hoist per-entity lookups out of the per-reference loop. Each reference
                // previously re-hashed the entity id against several maps (and every
                // child id, once per ref); on dense, deeply nested files that hashing
                // dominated resolution. Fetch them once per entity instead.
                let entity_consumed = file_consumed_words.entry(entity.id.clone()).or_default();
                add_local_bindings_to_consumed_words(entity_consumed, scope_idx, &scopes);

                let entity_span = entity_spans.get(entity.id.as_str()).copied();
                let child_ref_checks: Vec<ChildRefCheck> = children_by_parent
                    .get(entity.id.as_str())
                    .map(|children| {
                        children
                            .iter()
                            .filter(|child| {
                                entity_creates_reference_scope(&child.entity_type)
                                    && child.file_path == entity.file_path
                            })
                            .map(|child| {
                                let span = entity_spans
                                    .get(child.id.as_str())
                                    .map(|span| (span.start_byte, span.end_byte));
                                (child.start_line, child.end_line, span)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let entity_descendant_ranges = descendant_ranges_by_entity.get(&entity.id);

                let allow_implicit_instance_member_receiver =
                    allows_implicit_instance_member_receiver(
                        file_path,
                        &entity.entity_type,
                        &entity.content,
                    );

                // Filter pre-collected refs to this entity's line range
                for row_refs in &refs_by_row[start_row..end_row] {
                    for &ref_idx in row_refs {
                        let ast_ref = &all_file_refs[ref_idx];
                        if !ref_owned_by_entity(ast_ref, entity_span, &child_ref_checks) {
                            continue;
                        }
                        if row_in_descendant_ranges(entity_descendant_ranges, ast_ref.row) {
                            continue;
                        }
                        // Skip self-name refs (was previously done during collection)
                        let is_self_ref = match &ast_ref.kind {
                            AstRefKind::Call { name, .. } => name.as_ref() == entity.name.as_str(),
                            AstRefKind::ScopedCall { .. } => false,
                            AstRefKind::MethodCall { .. } => false,
                        };
                        if is_self_ref {
                            continue;
                        }

                        // Languages without per-symbol imports (e.g. Swift, Kotlin)
                        // allow cross-file resolution for lowercase function names.
                        let allow_cross_file = config.import_extractor.is_none();
                        let cache_key = resolution_cache_key(
                            ast_ref,
                            scope_idx,
                            entity.id.as_str(),
                            allow_cross_file,
                            allow_implicit_instance_member_receiver,
                        );
                        let resolution = if let Some(cache_key) = cache_key {
                            if let Some((_, cached)) = last_resolution
                                .as_ref()
                                .filter(|(last_key, _)| *last_key == cache_key)
                            {
                                if __prof_on {
                                    __cache_hit += 1;
                                }
                                cached.clone()
                            } else {
                                if __prof_on {
                                    __cache_miss += 1;
                                }
                                let __resolve_ref_t0 = __prof_on.then(Instant::now);
                                let resolved = resolve_ref(
                                    ast_ref,
                                    scope_idx,
                                    scope_lookup_missed,
                                    &scopes,
                                    &symbol_table,
                                    &class_members,
                                    &owner_members,
                                    &local_import_by_name,
                                    &instance_attr_types,
                                    entity_map,
                                    &swift_call_signatures,
                                    file_path,
                                    &entity.id,
                                    allow_cross_file,
                                    allow_implicit_instance_member_receiver,
                                    &file_lookup,
                                    &mut lookup_cache,
                                    __file_profile.as_mut(),
                                    &mut rec,
                                );
                                if let Some(t0) = __resolve_ref_t0 {
                                    __resolve_ref_ns += t0.elapsed().as_nanos() as u64;
                                }
                                last_resolution = Some((cache_key, resolved.clone()));
                                resolved
                            }
                        } else {
                            if __prof_on {
                                __cache_miss += 1;
                            }
                            let __resolve_ref_t0 = __prof_on.then(Instant::now);
                            let resolved = resolve_ref(
                                ast_ref,
                                scope_idx,
                                scope_lookup_missed,
                                &scopes,
                                &symbol_table,
                                &class_members,
                                &owner_members,
                                &local_import_by_name,
                                &instance_attr_types,
                                entity_map,
                                &swift_call_signatures,
                                file_path,
                                &entity.id,
                                allow_cross_file,
                                allow_implicit_instance_member_receiver,
                                &file_lookup,
                                &mut lookup_cache,
                                __file_profile.as_mut(),
                                &mut rec,
                            );
                            if let Some(t0) = __resolve_ref_t0 {
                                __resolve_ref_ns += t0.elapsed().as_nanos() as u64;
                            }
                            resolved
                        };

                        if let Some((target_id, ref_type, method)) = resolution {
                            if target_id != entity.id {
                                if entity.parent_id.is_some() {
                                    rec.one(Table::EntityMap, &target_id);
                                }
                                let is_parent_child =
                                    entity.parent_id.as_ref().map_or(false, |pid| {
                                        pid == &target_id
                                            || entity_map.get(&target_id).map_or(false, |t| {
                                                t.parent_id.as_ref() == Some(&entity.id)
                                            })
                                    });

                                if !is_parent_child {
                                    let reference = ref_description(ast_ref);
                                    add_scope_reference_words(entity_consumed, &reference);
                                    // The debug log allocates several Strings per
                                    // reference across the whole repo; production
                                    // builds discard it, so only populate it when
                                    // a caller asked for the log.
                                    if emit_local_binding_log {
                                        file_log.push(ResolutionEntry {
                                            from_entity: entity.id.clone(),
                                            reference,
                                            resolved_to: Some(target_id.clone()),
                                            method,
                                        });
                                    }
                                    file_edges.push((entity.id.clone(), target_id, ref_type));
                                }
                            }
                        } else {
                            let reference = ref_description(ast_ref);
                            add_scope_reference_words(entity_consumed, &reference);
                            if emit_local_binding_log {
                                file_log.push(ResolutionEntry {
                                    from_entity: entity.id.clone(),
                                    reference,
                                    resolved_to: None,
                                    method: "unresolved",
                                });
                            }
                        }
                    }
                }
            }

            if let Some(t0) = __ref_loop_t0 {
                // `unwrap_or_default`, not `if let Some`: at
                // `SEM_PROFILE_RESOLVE=2` the name accumulator is
                // deliberately absent, but the phase timers this call merges
                // (`scope_build`, `ref_collect`, `ref_loop`, `resolve_ref`,
                // `files`) are exactly what `=2` exists to report. Gating the
                // merge on the accumulator would have `=2` print zeros.
                {
                    let accum = __file_profile.unwrap_or_default();
                    prof::merge_file(
                        accum,
                        __scope_build_ns.unwrap_or(0),
                        __ref_collect_ns.unwrap_or(0),
                        t0.elapsed().as_nanos() as u64,
                        __resolve_ref_ns,
                        __cache_hit,
                        __cache_miss,
                    );
                }
            }

            Some(PerFileScopeResult {
                file_path: file_path.as_str(),
                edges: file_edges,
                log: file_log,
                consumed_words: file_consumed_words,
                read_set: rec.enabled().then(|| rec.into_read_set()),
                reused: false,
            })
        })
        .collect();
    prof::add_pass2_wall_ns(__pass2_t0.elapsed());

    let __scope_merge_t0 = Instant::now();
    for result in per_file_results {
        if let Some(state) = incremental.as_deref_mut() {
            if result.reused {
                // The cache already holds this file's result — the closure above
                // read its copy out of exactly this entry. Stamping it is the
                // whole write.
                state.inc.keep_scope(result.file_path);
            } else if let Some(read_set) = result.read_set {
                state.inc.put_scope(
                    result.file_path,
                    CachedScopeResult {
                        edges: result.edges.clone(),
                        consumed_words: result.consumed_words.clone(),
                        read_set,
                    },
                );
            }
        }
        all_edges.extend(result.edges);
        log.extend(result.log);
        // Move each file's word set in whole rather than `or_default().extend()`
        //. Entity ids are `{file_path}::…` by construction, so two
        // *files* can never contribute the same key and the vacant case is the
        // only one that ever runs on real input — but `extend` on a fresh empty
        // set re-hashes every word, which across a 40k-file corpus was a
        // whole-corpus rehash on every rebuild. The occupied arm is kept as a
        // conservative fallback; the merged contents are identical either way
        // (the only reader collects the set into a `HashSet<&str>` and asks
        // `contains`, so its iteration order is unobservable).
        consumed_words.reserve(result.consumed_words.len());
        for (entity_id, words) in result.consumed_words {
            match consumed_words.entry(entity_id) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(words);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().extend(words);
                }
            }
        }
    }
    prof::add_scope_merge_ns(__scope_merge_t0.elapsed());

    // Deduplicate edges keeping the first-inserted (from, to). Index-based:
    // sorting indices by borrowed keys avoids the two String clones per edge
    // the old HashSet key paid — millions of allocations on a monorepo.
    let __scope_dedup_t0 = Instant::now();
    let all_edges = {
        let mut order: Vec<usize> = (0..all_edges.len()).collect();
        order.sort_by(|&a, &b| {
            (&all_edges[a].0, &all_edges[a].1, a).cmp(&(&all_edges[b].0, &all_edges[b].1, b))
        });
        let mut keep = vec![false; all_edges.len()];
        let mut prev: Option<usize> = None;
        for &i in &order {
            let dup = prev.is_some_and(|p: usize| {
                all_edges[p].0 == all_edges[i].0 && all_edges[p].1 == all_edges[i].1
            });
            if !dup {
                keep[i] = true;
            }
            prev = Some(i);
        }
        let mut result = Vec::with_capacity(all_edges.len());
        for (i, edge) in all_edges.into_iter().enumerate() {
            if keep[i] {
                result.push(edge);
            }
        }
        result
    };
    prof::add_scope_dedup_ns(__scope_dedup_t0.elapsed());

    ScopeResultFull {
        edges: all_edges,
        resolution_log: log,
        consumed_words,
    }
}

/// One file's product of the pass-2 closure. Carries the file path so the
/// red-green cache can attribute the edges (see the edge-ownership invariant in
/// `incremental`), and the read set that authorizes reusing them next time.
struct PerFileScopeResult<'a> {
    file_path: &'a str,
    edges: Vec<(String, String, RefType)>,
    log: Vec<ResolutionEntry>,
    consumed_words: HashMap<String, HashSet<String>>,
    /// `None` when not recording (the cold, non-session path).
    read_set: Option<crate::parser::incremental::ReadSet>,
    /// Whether this result came from the cache rather than from resolution.
    reused: bool,
}

fn hash_of_str(v: &str) -> u64 {
    crate::parser::incremental::hash_str(v)
}

/// Order-independent hash of one file's import-table slice. The slice is grouped
/// out of a `HashMap`, so its order is not stable across runs; its content is.
fn hash_import_slice(entries: &[(&str, &str)]) -> u64 {
    let mut pairs: Vec<(&str, &str)> = entries.to_vec();
    pairs.sort_unstable();
    let mut h = ValueHasher::new();
    for (name, target) in pairs {
        h.s(name).s(target);
    }
    h.finish()
}

/// Whole-table hash for the Swift call-signature map (a guard, see [`Table`]).
fn hash_swift_signatures(sigs: &HashMap<String, SwiftCallSignature>) -> u64 {
    let mut ids: Vec<&String> = sigs.keys().collect();
    ids.sort_unstable();
    let mut h = ValueHasher::new();
    for id in ids {
        h.s(id);
        for label in &sigs[id].argument_labels {
            h.s(label.as_deref().unwrap_or("\u{0}"));
        }
    }
    h.finish()
}

/// Fingerprint the corpus-wide lookup tables that `EntityGraph::build` owns and
/// hands to resolution. Called once per build, before any resolution runs, so a
/// file's read set is always compared against a map that already covers every
/// corpus-wide table it can name.
///
/// Entity-keyed tables are *not* fingerprinted under a file's own entity ids —
/// see [`crate::parser::incremental::Incremental`]: for a reuse-eligible (JS/TS)
/// file, every id derived from its own entities is prefixed with its own path
/// (`build_entity_id`), so those reads are already covered by the file's own
/// content hash. Only ids that can cross a file boundary are recorded, and those
/// all arrive through the tables below.
///
/// Returns the `GuardPyWildcardImport` value it folded, so an incremental
/// rebuild can carry it forward and update it by XOR rather than refolding the
/// corpus (see [`fingerprint_corpus_tables_incremental`]).
pub(crate) fn fingerprint_corpus_tables(
    lookups: &PreBuiltLookups,
    entity_map: &HashMap<String, EntityInfo>,
    fp: &mut TableFingerprints,
) -> u64 {
    let mut sink = FingerprintSink::new(fp, 0);
    // `register_namespace_import` (Python's bare `import module` form) scans
    // every `(name, target file)` pair below looking for ones whose file
    // matches the imported module — an unbounded read (see `Table::
    // GuardPyWildcardImport`'s doc). Fold every pair into one order-independent
    // guard hash (XOR, since the pairs have no stable order) as this same loop
    // already visits them for the per-key `SymbolTable`/`EntityMap`
    // fingerprints, so the guard costs one extra `u64` XOR per pair, not a
    // second corpus scan.
    let mut wildcard_import_guard: u64 = 0;
    for (name, ids) in lookups.symbol_table.iter() {
        let mut h = ValueHasher::new();
        for id in ids {
            h.s(id);
            if let Some(info) = entity_map.get(id) {
                wildcard_import_guard ^= {
                    let mut wh = ValueHasher::new();
                    wh.s(name).s(&info.file_path);
                    wh.finish()
                };
            }
        }
        sink.one(Table::SymbolTable, name, h.finish());
    }
    for (owner, members) in &lookups.class_members {
        sink.one(Table::ClassMembers, owner, hash_member_list(members));
    }
    for (owner, members) in &lookups.owner_members {
        sink.one(Table::OwnerMembers, owner, hash_member_list(members));
    }
    for (id, info) in entity_map {
        let mut h = ValueHasher::new();
        h.s(&info.name)
            .s(&info.entity_type)
            .s(&info.file_path)
            .s(info.parent_id.as_deref().unwrap_or("\u{0}"))
            .u(info.start_line)
            .u(info.end_line);
        sink.one(Table::EntityMap, id, h.finish());
    }
    for (pkg, entries) in &lookups.go_pkg_index {
        sink.one(Table::GoPkgIndex, pkg, hash_go_pkg_entries(entries));
    }
    sink.whole(Table::GuardPyWildcardImport, wildcard_import_guard);
    // Same fold, second tag: `register_rust_module_import` (Rust's relative
    // module-alias `use` form) has the identical unbounded-read
    // shape over the identical `(name, file_path)` data — see
    // `Table::GuardRustModuleAlias`'s doc for why it still gets its own tag
    // rather than reusing the Python one above.
    sink.whole(Table::GuardRustModuleAlias, wildcard_import_guard);
    wildcard_import_guard
}

/// The keys `maintain_entity_lookups_incremental` removed from or inserted into
/// each corpus table this build, plus the XOR delta to the Python
/// wildcard-import guard those same entities imply.
///
/// Everything here is `O(touched files' entities)`, never `O(corpus)`.
#[derive(Default)]
pub(crate) struct TouchedCorpusKeys {
    /// `symbol_table` buckets whose contents may have moved.
    pub(crate) names: HashSet<String>,
    /// `class_members` buckets whose contents may have moved.
    pub(crate) owner_names: HashSet<String>,
    /// `owner_members` buckets whose contents may have moved.
    pub(crate) parent_ids: HashSet<String>,
    /// `entity_map` entries removed or (re)inserted.
    pub(crate) entity_ids: HashSet<String>,
    /// XOR of `hash(name, file_path)` over every entity removed *and* every
    /// entity inserted. See [`fingerprint_corpus_tables_incremental`].
    pub(crate) wildcard_guard_delta: u64,
}

/// One entity's contribution to the `GuardPyWildcardImport` XOR fold.
///
/// [`fingerprint_corpus_tables`] spells that fold as "for every `(name, id)` in
/// `symbol_table`, XOR in `hash(name, entity_map[id].file_path)`". That is the
/// same multiset as "for every entity `e`, XOR in `hash(e.name, e.file_path)`":
/// `symbol_table[name]` receives one push per entity named `name`, `entity_map`
/// always holds that id, and an entity id is `{file_path}::…` by construction so
/// `entity_map[e.id].file_path == e.file_path` — *including* when two entities
/// collide on one id (TypeScript overloads do), where both formulations XOR the
/// same value twice and both cancel. Per-entity is what makes the guard
/// maintainable in `O(touched entities)`: XOR is its own inverse, so removing
/// and inserting are the same operation.
pub(crate) fn wildcard_guard_contribution(name: &str, file_path: &str) -> u64 {
    let mut h = ValueHasher::new();
    h.s(name).s(file_path);
    h.finish()
}

/// Update a carried-forward corpus fingerprint map in place, touching only the
/// keys [`TouchedCorpusKeys`] names.
///
/// Parity with [`fingerprint_corpus_tables`] rests on two facts:
///
/// * every value below is a pure function of its own table bucket, and
///   `maintain_entity_lookups_incremental` guarantees a bucket it did not name
///   is byte-identical to last build's;
/// * a key that *vanished* is `remove`d, not left stale — the case that would
///   otherwise keep a file GREEN whose lookup now misses.
///
/// `go_pkg_index` is **not** maintained here; the caller falls back to the whole
/// fold whenever it is non-empty (i.e. whenever the corpus has `.go` files),
/// because that index is a re-derivation of the other tables under a different
/// key shape (file stem / directory name) with no per-file key index of its own.
pub(crate) fn fingerprint_corpus_tables_incremental(
    touched: &TouchedCorpusKeys,
    lookups: &PreBuiltLookups,
    entity_map: &HashMap<String, EntityInfo>,
    fp: &mut TableFingerprints,
    wildcard_import_guard: &mut u64,
) {
    for name in &touched.names {
        match lookups.symbol_table.get(name) {
            Some(ids) => {
                let mut h = ValueHasher::new();
                for id in ids {
                    h.s(id);
                }
                fp.put(key1(Table::SymbolTable, 0, name), h.finish());
            }
            None => fp.remove(key1(Table::SymbolTable, 0, name)),
        }
    }
    for owner in &touched.owner_names {
        match lookups.class_members.get(owner) {
            Some(members) => fp.put(
                key1(Table::ClassMembers, 0, owner),
                hash_member_list(members),
            ),
            None => fp.remove(key1(Table::ClassMembers, 0, owner)),
        }
    }
    for parent in &touched.parent_ids {
        match lookups.owner_members.get(parent) {
            Some(members) => fp.put(
                key1(Table::OwnerMembers, 0, parent),
                hash_member_list(members),
            ),
            None => fp.remove(key1(Table::OwnerMembers, 0, parent)),
        }
    }
    for id in &touched.entity_ids {
        match entity_map.get(id) {
            Some(info) => {
                let mut h = ValueHasher::new();
                h.s(&info.name)
                    .s(&info.entity_type)
                    .s(&info.file_path)
                    .s(info.parent_id.as_deref().unwrap_or("\u{0}"))
                    .u(info.start_line)
                    .u(info.end_line);
                fp.put(key1(Table::EntityMap, 0, id), h.finish());
            }
            None => fp.remove(key1(Table::EntityMap, 0, id)),
        }
    }
    *wildcard_import_guard ^= touched.wildcard_guard_delta;
    fp.put(
        key_whole(Table::GuardPyWildcardImport, 0),
        *wildcard_import_guard,
    );
    // Same value, second tag — mirrors `fingerprint_corpus_tables`'s pairing
    // of `GuardPyWildcardImport`/`GuardRustModuleAlias` above.
    fp.put(
        key_whole(Table::GuardRustModuleAlias, 0),
        *wildcard_import_guard,
    );
}

fn hash_member_list(members: &[(String, String)]) -> u64 {
    let mut h = ValueHasher::new();
    for (name, id) in members {
        h.s(name).s(id);
    }
    h.finish()
}

/// [`hash_member_list`]'s sibling for [`GoPkgIndex`] buckets: the
/// declaring-directory field is new information a bucket carries that
/// `hash_member_list`'s two-tuple shape cannot fold in, so a dedicated
/// fingerprint keeps `Table::GoPkgIndex`'s invalidation correct rather than
/// silently truncating the read-set to `(name, id)` and missing a change
/// that only moves an entity's declaring directory.
fn hash_go_pkg_entries(entries: &[(String, String, String)]) -> u64 {
    let mut h = ValueHasher::new();
    for (name, id, decl_dir) in entries {
        h.s(name).s(id).s(decl_dir);
    }
    h.finish()
}

fn ref_description(ast_ref: &AstRef) -> String {
    match &ast_ref.kind {
        AstRefKind::Call {
            name,
            argument_labels,
        } => format!(
            "{}({})",
            name,
            format_argument_labels(argument_labels.as_deref())
        ),
        AstRefKind::ScopedCall { path, name } => format!("{}::{}()", path, name),
        AstRefKind::MethodCall {
            receiver,
            method,
            argument_labels,
        } => format!(
            "{}.{}({})",
            receiver,
            method,
            format_argument_labels(argument_labels.as_deref())
        ),
    }
}

fn format_argument_labels(argument_labels: Option<&[Option<String>]>) -> String {
    argument_labels
        .map(|labels| {
            labels
                .iter()
                .map(|label| {
                    label
                        .as_deref()
                        .map_or("_:".to_string(), |label| format!("{label}:"))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn add_scope_reference_words(words: &mut HashSet<String>, reference: &str) {
    let reference = reference.strip_suffix("()").unwrap_or(reference);
    let reference = reference
        .split_once('(')
        .map_or(reference, |(name, _)| name);
    if let Some((receiver, member)) = reference.rsplit_once('.') {
        if !receiver.is_empty() {
            words.insert(receiver.to_string());
        }
        if !member.is_empty() {
            words.insert(member.to_string());
        }
    } else if reference.contains("::") {
        for part in reference.split("::").filter(|part| !part.is_empty()) {
            words.insert(part.to_string());
        }
    } else if !reference.is_empty() {
        words.insert(reference.to_string());
    }
}

fn add_local_bindings_to_consumed_words(
    words: &mut HashSet<String>,
    start_scope: usize,
    scopes: &[Scope],
) {
    let mut idx = Some(start_scope);
    while let Some(scope_idx) = idx {
        words.extend(scopes[scope_idx].bindings.iter().cloned());
        idx = scopes[scope_idx].parent;
    }
}

fn log_scope_bindings(
    file_log: &mut Vec<ResolutionEntry>,
    from_entity: &str,
    scope: &Scope,
    start_row: usize,
    end_row: usize,
    descendant_ranges_by_entity: &HashMap<String, Vec<(usize, usize)>>,
) {
    let mut bindings: Vec<&String> = scope.bindings.iter().collect();
    bindings.sort();
    for binding in bindings {
        let belongs_to_entity = scope.binding_rows.get(binding).map_or(false, |rows| {
            rows.iter().any(|row| {
                *row >= start_row
                    && *row < end_row
                    && !row_belongs_to_descendant(descendant_ranges_by_entity, from_entity, *row)
            })
        });
        if !belongs_to_entity {
            continue;
        }
        file_log.push(ResolutionEntry {
            from_entity: from_entity.to_string(),
            reference: binding.clone(),
            resolved_to: None,
            method: "local_binding",
        });
    }
}

fn build_descendant_ranges_by_entity(
    file_entities: &[&SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
) -> HashMap<String, Vec<(usize, usize)>> {
    let mut ranges_by_entity: HashMap<String, Vec<(usize, usize)>> = HashMap::default();
    let mut sorted_entities = file_entities.to_vec();
    sorted_entities.sort_by(|left, right| {
        left.start_line
            .cmp(&right.start_line)
            .then_with(|| right.end_line.cmp(&left.end_line))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut ancestor_stack: Vec<&SemanticEntity> = Vec::new();
    for entity in sorted_entities {
        while ancestor_stack.last().map_or(false, |candidate| {
            !is_strict_enclosing_range(candidate, entity)
        }) {
            ancestor_stack.pop();
        }

        if !entity_creates_reference_scope(&entity.entity_type) {
            ancestor_stack.push(entity);
            continue;
        }

        let child_range = (entity.start_line.saturating_sub(1), entity.end_line);
        let mut current = entity.parent_id.as_deref();
        let mut visited = HashSet::default();
        while let Some(parent_id) = current {
            if !visited.insert(parent_id.to_string()) {
                break;
            }
            ranges_by_entity
                .entry(parent_id.to_string())
                .or_default()
                .push(child_range);
            current = entity_map
                .get(parent_id)
                .and_then(|parent| parent.parent_id.as_deref());
        }

        for ancestor in &ancestor_stack {
            ranges_by_entity
                .entry(ancestor.id.clone())
                .or_default()
                .push(child_range);
        }

        ancestor_stack.push(entity);
    }
    for ranges in ranges_by_entity.values_mut() {
        ranges.sort_unstable();
        ranges.dedup();
    }
    ranges_by_entity
}

fn is_strict_enclosing_range(candidate: &SemanticEntity, child: &SemanticEntity) -> bool {
    candidate.file_path == child.file_path
        && candidate.start_line <= child.start_line
        && child.end_line <= candidate.end_line
        && (candidate.start_line < child.start_line || child.end_line < candidate.end_line)
}

fn row_belongs_to_descendant(
    descendant_ranges_by_entity: &HashMap<String, Vec<(usize, usize)>>,
    entity_id: &str,
    row: usize,
) -> bool {
    row_in_descendant_ranges(descendant_ranges_by_entity.get(entity_id), row)
}

/// Same check as [`row_belongs_to_descendant`], but over pre-fetched ranges so the
/// per-ref loop avoids a HashMap lookup per reference.
fn row_in_descendant_ranges(ranges: Option<&Vec<(usize, usize)>>, row: usize) -> bool {
    ranges.map_or(false, |ranges| {
        let eligible = ranges.partition_point(|(start, _)| *start <= row);
        ranges[..eligible]
            .iter()
            .rev()
            .any(|(start, end)| row >= *start && row < *end)
    })
}

/// Build scope tree by walking the AST.
// `Node::named_child(i)` is not a random-access lookup: tree-sitter restarts a
// child iterator at index 0 and walks forward every call, skipping unnamed and
// extra nodes on the way. Indexing 0..named_child_count() therefore costs
// O(children^2) per node. Most AST nodes have a handful of children so nobody
// notices, but real code contains nodes with enormous fan-out — a 3.5 MB
// single-array fixture like microsoft/TypeScript's
// `tests/cases/fourslash/reallyLargeFile.ts` is one array literal with ~10^5
// named children, i.e. ~10^10 iterator steps for one node, which pins a core
// for many minutes. Walking the children once with a cursor is O(children) and
// yields exactly the same nodes in the same order.
/// Creates class scopes and maps methods to them.
/// Uses an iterative worklist to avoid stack overflow on deeply nested ASTs.
/// Fixes: https://github.com/Ataraxy-Labs/sem/issues/103
fn push_named_children_rev<'a>(
    worklist: &mut Vec<tree_sitter::Node<'a>>,
    node: tree_sitter::Node<'a>,
) {
    let start = worklist.len();
    let mut cursor = node.walk();
    worklist.extend(node.named_children(&mut cursor));
    worklist[start..].reverse();
}

fn push_scoped_named_children_rev<'a>(
    worklist: &mut Vec<(tree_sitter::Node<'a>, usize)>,
    node: tree_sitter::Node<'a>,
    scope: usize,
) {
    let start = worklist.len();
    let mut cursor = node.walk();
    worklist.extend(node.named_children(&mut cursor).map(|child| (child, scope)));
    worklist[start..].reverse();
}

fn build_scopes_from_ast(
    root: tree_sitter::Node,
    root_scope: usize,
    scopes: &mut Vec<Scope>,
    entity_scope_map: &mut HashMap<String, usize>,
    entity_inner_scope: &mut HashMap<String, usize>,
    file_lookup: &FileEntityLookup<'_>,
    children_by_parent: &HashMap<&str, Vec<&SemanticEntity>>,
    entity_map: &HashMap<String, EntityInfo>,
    source: &[u8],
    config: &ScopeResolveConfig,
) {
    // Each entry: (node, current_scope)
    let mut worklist: Vec<(tree_sitter::Node, usize)> = vec![(root, root_scope)];

    while let Some((node, current_scope)) = worklist.pop() {
        let child_scope = scope_visit_node(
            node,
            current_scope,
            scopes,
            entity_scope_map,
            entity_inner_scope,
            file_lookup,
            children_by_parent,
            entity_map,
            source,
            config,
        );
        push_scoped_named_children_rev(&mut worklist, node, child_scope);
    }
}

/// One node's worth of `build_scopes_from_ast`: every scope-tree
/// side effect for `node`, returning the scope its named children inherit.
/// Factored out verbatim from the walk's loop body so the unfused walk above
/// and the fused triple walk (`fused_scope_refs_import_walk`) are the same
/// per-node semantics under different traversal drivers — the fusion may only
/// change how many times the tree is traversed, never what a visit does.
#[allow(clippy::too_many_arguments)]
fn scope_visit_node(
    node: tree_sitter::Node,
    current_scope: usize,
    scopes: &mut Vec<Scope>,
    entity_scope_map: &mut HashMap<String, usize>,
    entity_inner_scope: &mut HashMap<String, usize>,
    file_lookup: &FileEntityLookup<'_>,
    children_by_parent: &HashMap<&str, Vec<&SemanticEntity>>,
    entity_map: &HashMap<String, EntityInfo>,
    source: &[u8],
    config: &ScopeResolveConfig,
) -> usize {
    {
        let kind = node.kind();

        // Class-like scope: config-driven
        let is_class_like = config.class_scope_nodes.contains(&kind);

        // Impl scope: config-driven (Rust impl_item, Swift extension)
        let is_impl = config.impl_scope_nodes.contains(&kind);

        if is_class_like || is_impl {
            let class_name = if is_impl {
                node.child_by_field_name("type")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
            } else {
                match &config.class_name_field {
                    ClassNameField::Simple(field) => node
                        .child_by_field_name(field)
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or(""),
                    ClassNameField::TypeSpec { spec_kind, field } => {
                        let mut name = "";
                        let mut cursor = node.walk();
                        for child in node.named_children(&mut cursor) {
                            if child.kind() == *spec_kind {
                                name = child
                                    .child_by_field_name(field)
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or("");
                                break;
                            }
                        }
                        name
                    }
                    ClassNameField::ImplType(field) => node
                        .child_by_field_name(field)
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or(""),
                }
            };

            let line = node.start_position().row + 1;
            let class_entity = file_lookup.find_at_line(class_name, line, |entity| {
                matches!(
                    entity.entity_type.as_str(),
                    "class"
                        | "struct"
                        | "interface"
                        | "enum"
                        | "protocol"
                        | "protocol_declaration"
                        | "object_declaration"
                        | "companion_object"
                )
            });

            if let Some(ce) = class_entity {
                let existing_scope = entity_inner_scope.get(&ce.id).copied();

                let class_scope_idx = if let Some(idx) = existing_scope {
                    idx
                } else {
                    let idx = scopes.len();
                    scopes.push(Scope {
                        parent: Some(current_scope),
                        defs: HashMap::default(),
                        bindings: HashSet::default(),
                        binding_rows: HashMap::default(),
                        types: HashMap::default(),
                        pending_call_types: HashMap::default(),
                        pending_field_types: HashMap::default(),
                        owner_id: Some(ce.id.clone()),
                        kind: "class",
                    });
                    entity_scope_map.insert(ce.id.clone(), current_scope);
                    entity_inner_scope.insert(ce.id.clone(), idx);
                    idx
                };

                if let Some(children) = children_by_parent.get(ce.id.as_str()) {
                    for entity in children {
                        scopes[class_scope_idx]
                            .defs
                            .insert(entity.name.clone(), entity.id.clone());
                        entity_scope_map.insert(entity.id.clone(), class_scope_idx);
                    }
                }

                return class_scope_idx;
            } else if is_impl {
                // The impl'd type is usually defined elsewhere (idiomatic Rust:
                // `struct S;` then `impl S { ... }` on a later line; likewise a
                // Swift `extension`), so find_at_line above couldn't locate it at
                // the impl's own line. Anchor the scope on the impl entity itself
                // and register its methods, so `self.method()` calls inside the
                // impl resolve to sibling methods instead of being dropped.
                let impl_entity = file_lookup
                    .find_at_line(class_name, line, |entity| entity.entity_type == "impl");
                let class_scope_idx = scopes.len();
                scopes.push(Scope {
                    parent: Some(current_scope),
                    defs: HashMap::default(),
                    bindings: HashSet::default(),
                    binding_rows: HashMap::default(),
                    types: HashMap::default(),
                    pending_call_types: HashMap::default(),
                    pending_field_types: HashMap::default(),
                    owner_id: impl_entity.map(|ie| ie.id.clone()),
                    kind: "class",
                });
                if let Some(ie) = impl_entity {
                    entity_scope_map.insert(ie.id.clone(), current_scope);
                    entity_inner_scope.insert(ie.id.clone(), class_scope_idx);
                    if let Some(children) = children_by_parent.get(ie.id.as_str()) {
                        for entity in children {
                            scopes[class_scope_idx]
                                .defs
                                .insert(entity.name.clone(), entity.id.clone());
                            entity_scope_map.insert(entity.id.clone(), class_scope_idx);
                        }
                    }
                }
                return class_scope_idx;
            } else {
                let class_scope_idx = scopes.len();
                scopes.push(Scope {
                    parent: Some(current_scope),
                    defs: HashMap::default(),
                    bindings: HashSet::default(),
                    binding_rows: HashMap::default(),
                    types: HashMap::default(),
                    pending_call_types: HashMap::default(),
                    pending_field_types: HashMap::default(),
                    owner_id: None,
                    kind: "class",
                });
                return class_scope_idx;
            }
        }

        // Rust mod_item: create a module scope so nested functions resolve
        // names from the parent scope (e.g. super::target() walks up correctly).
        if kind == "mod_item" {
            let mod_name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            let mod_scope_idx = scopes.len();
            scopes.push(Scope {
                parent: Some(current_scope),
                defs: HashMap::default(),
                bindings: HashSet::default(),
                binding_rows: HashMap::default(),
                types: HashMap::default(),
                pending_call_types: HashMap::default(),
                pending_field_types: HashMap::default(),
                owner_id: None,
                kind: "module",
            });

            // Register any entities that are children of this module
            let line = node.start_position().row + 1;
            let mod_entity =
                file_lookup.find_at_line(mod_name, line, |entity| entity.entity_type == "module");

            if let Some(me) = mod_entity {
                scopes[mod_scope_idx].owner_id = Some(me.id.clone());
                entity_scope_map
                    .entry(me.id.clone())
                    .or_insert(current_scope);
                entity_inner_scope.insert(me.id.clone(), mod_scope_idx);

                // Register child entities in the module scope
                if let Some(children) = children_by_parent.get(me.id.as_str()) {
                    for child_entity in children {
                        scopes[mod_scope_idx]
                            .defs
                            .insert(child_entity.name.clone(), child_entity.id.clone());
                        entity_scope_map.insert(child_entity.id.clone(), mod_scope_idx);
                    }
                }
            }

            return mod_scope_idx;
        }

        // Function-like scope: config-driven
        let is_function_like = config.function_scope_nodes.contains(&kind);

        if is_function_like {
            let func_name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");

            let parent_scope = if config.external_method && kind == "method_declaration" {
                let receiver_type = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| extract_go_receiver_type(t));
                if let Some(ref struct_name) = receiver_type {
                    let found = scopes.iter().enumerate().find(|(_, s)| {
                        s.kind == "class"
                            && s.owner_id.as_ref().map_or(false, |oid| {
                                entity_map
                                    .get(oid)
                                    .map_or(false, |e| e.name == *struct_name)
                            })
                    });
                    found.map(|(idx, _)| idx).unwrap_or(current_scope)
                } else {
                    current_scope
                }
            } else {
                current_scope
            };

            let func_scope_idx = scopes.len();
            scopes.push(Scope {
                parent: Some(parent_scope),
                defs: HashMap::default(),
                bindings: HashSet::default(),
                binding_rows: HashMap::default(),
                types: HashMap::default(),
                pending_call_types: HashMap::default(),
                pending_field_types: HashMap::default(),
                owner_id: None,
                kind: "function",
            });

            let line = node.start_position().row + 1;
            let func_entity = file_lookup.find_at_line(func_name, line, |_| true);

            if let Some(fe) = func_entity {
                scopes[func_scope_idx].owner_id = Some(fe.id.clone());
                entity_scope_map
                    .entry(fe.id.clone())
                    .or_insert(parent_scope);
                entity_inner_scope.insert(fe.id.clone(), func_scope_idx);
                // Register this function's nested child entities:
                // same shape as the class-like branch above. Without it,
                // entities nested inside a plain function never enter any
                // scope's `.defs`/`entity_scope_map` and every lookup of them
                // falls back.
                if let Some(children) = children_by_parent.get(fe.id.as_str()) {
                    for entity in children {
                        scopes[func_scope_idx]
                            .defs
                            .insert(entity.name.clone(), entity.id.clone());
                        entity_scope_map.insert(entity.id.clone(), func_scope_idx);
                    }
                }
                if config.external_method
                    && kind == "method_declaration"
                    && parent_scope != current_scope
                {
                    scopes[parent_scope]
                        .defs
                        .insert(fe.name.clone(), fe.id.clone());
                }
            }

            scan_assignments(node, func_scope_idx, scopes, source, config);
            scan_function_params(node, func_scope_idx, scopes, source, config);

            if config.external_method && kind == "method_declaration" {
                if let Some(receiver) = node.child_by_field_name("receiver") {
                    let mut rcursor = receiver.walk();
                    for param in receiver.named_children(&mut rcursor) {
                        if param.kind() == "parameter_declaration" {
                            let param_name = param
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("");
                            let param_type = param
                                .child_by_field_name("type")
                                .map(|n| extract_base_type(n, source))
                                .unwrap_or_default();
                            if !param_name.is_empty() && !param_type.is_empty() {
                                scopes[func_scope_idx]
                                    .types
                                    .insert(param_name.to_string(), param_type);
                            }
                        }
                    }
                }
            }

            return func_scope_idx;
        }

        current_scope
    }
}

/// The fused triple walk (fold-fusion #3 —
/// §"fuse the three AST walks — the plan"). One document-order pre-order
/// traversal producing what `build_scopes_from_ast` and
/// `collect_all_file_refs` produced in two walks — `⟨cata f, cata g⟩ =
/// cata ⟨f,g⟩` holds with no caveat because the two are order-identical and
/// state-disjoint — plus the sorted `start_byte`s of every import statement
/// `extract_imports_from_ast` would handle. The import *handlers* do not run
/// here: their effective order is not document order (extract's LIFO
/// forward-push processes sibling subtrees in reverse), so
/// `record_import_stmts_pruned` + `dispatch_import_stmts_from_facts` run
/// them afterwards, at the original program point (after the pre-built
/// import-table seed), in the original order.
///
/// `in_import` suppresses recording inside an already-recorded import node,
/// mirroring extract's "handled children are not descended" — the recorded
/// set equals extract's handled set H by the same induction. The root itself
/// is classified here where extract only ever classified children; no
/// configured grammar's root kind (`module`, `program`, `source_file`,
/// `compilation_unit`, …) is an import kind, and the invariant test + bit-identical
/// gates witness the equivalence rather than leaving it argued.
#[allow(clippy::too_many_arguments)]
fn fused_scope_refs_import_walk(
    root: tree_sitter::Node,
    root_scope: usize,
    scopes: &mut Vec<Scope>,
    entity_scope_map: &mut HashMap<String, usize>,
    entity_inner_scope: &mut HashMap<String, usize>,
    file_lookup: &FileEntityLookup<'_>,
    children_by_parent: &HashMap<&str, Vec<&SemanticEntity>>,
    entity_map: &HashMap<String, EntityInfo>,
    source: &[u8],
    config: &ScopeResolveConfig,
) -> (Vec<AstRef>, Vec<usize>, bool) {
    let mut refs = AstRefCollector::new();
    let mut import_starts: Vec<usize> = Vec::new();
    // MUL Phase 1: whether the walk saw a literal `"call"`-kind
    // node — the one ctor-infer's `scan_constructor_calls` hardcodes to
    // Python's grammar (`TREELESS` predicate). One extra
    // `kind()` comparison on nodes this walk already visits, decided
    // structurally from what the walk saw rather than a language table.
    let mut saw_call_node = false;
    // Each entry: (node, current_scope, inside-a-recorded-import)
    let mut worklist: Vec<(tree_sitter::Node, usize, bool)> = vec![(root, root_scope, false)];

    while let Some((node, current_scope, in_import)) = worklist.pop() {
        refs_visit_node(node, source, config, &mut refs);

        if !saw_call_node && node.kind() == "call" {
            saw_call_node = true;
        }

        let is_import = !in_import && classify_import_stmt(node.kind(), config).is_some();
        if is_import {
            // Pre-order ⇒ pushed in ascending byte order, so the vec is
            // sorted and `subtree_contains_import_start` may binary-search.
            import_starts.push(node.start_byte());
        }
        let child_in_import = in_import || is_import;

        let child_scope = scope_visit_node(
            node,
            current_scope,
            scopes,
            entity_scope_map,
            entity_inner_scope,
            file_lookup,
            children_by_parent,
            entity_map,
            source,
            config,
        );

        let start = worklist.len();
        let mut cursor = node.walk();
        worklist.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, child_scope, child_in_import)),
        );
        worklist[start..].reverse();
    }
    (refs.into_refs(), import_starts, saw_call_node)
}

/// Scan for variable assignments and record type bindings.
fn scan_assignments(
    root: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
    config: &ScopeResolveConfig,
) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let ck = child.kind();

            // Check if this node matches an assignment rule
            for rule in config.assignment_rules {
                if ck == rule.node_kind {
                    match rule.strategy {
                        AssignmentStrategy::LeftRight => {
                            scan_single_assignment(child, scope_idx, scopes, source);
                        }
                        AssignmentStrategy::Declarators => {
                            scan_ts_var_declaration(child, scope_idx, scopes, source);
                        }
                        AssignmentStrategy::PatternBased => {
                            scan_rust_let_declaration(child, scope_idx, scopes, source);
                        }
                        AssignmentStrategy::ShortVar => {
                            scan_go_short_var(child, scope_idx, scopes, source);
                        }
                        AssignmentStrategy::VarSpec => {
                            scan_go_var_declaration(child, scope_idx, scopes, source);
                        }
                    }
                }
            }

            // Recurse into configured container nodes
            if config.assignment_recurse_into.contains(&ck) {
                worklist.push(child);
            }
        }
    }
}

fn record_binding(scopes: &mut [Scope], scope_idx: usize, name: &str, row: usize) {
    scopes[scope_idx].bindings.insert(name.to_string());
    scopes[scope_idx]
        .binding_rows
        .entry(name.to_string())
        .or_default()
        .push(row);
}

/// Scan function parameter type annotations and add them as type bindings.
/// e.g. `def foo(shelter: Shelter)` -> types["shelter"] = "Shelter"
fn scan_function_params(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
    config: &ScopeResolveConfig,
) {
    // Try "parameters" field first (Python, TS, Rust, Go, etc.)
    // Fallback to direct children for languages like Swift where
    // params are direct children of function_declaration.
    let mut params_node = node.child_by_field_name("parameters");
    if params_node.is_none() {
        // Kotlin: function_value_parameters
        let mut c = node.walk();
        for ch in node.named_children(&mut c) {
            if ch.kind() == "function_value_parameters" {
                params_node = Some(ch);
                break;
            }
        }
    }

    // If we have a params container, iterate its children.
    // Otherwise, iterate direct children of the function node (Swift).
    let (iter_node, use_direct) = match params_node {
        Some(p) => (p, false),
        None => (node, true),
    };

    let mut cursor = iter_node.walk();
    for child in iter_node.named_children(&mut cursor) {
        // When using direct children, only process param-like nodes
        if use_direct {
            let is_param = config
                .param_rules
                .iter()
                .any(|r| child.kind() == r.node_kind);
            if !is_param {
                continue;
            }
        }
        for rule in config.param_rules {
            if child.kind() != rule.node_kind {
                continue;
            }

            let param_name = match &rule.name_field {
                ParamNameField::Simple(field) => child
                    .child_by_field_name(field)
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or(""),
                ParamNameField::WithFallback(field) => child
                    .child_by_field_name(field)
                    .or_else(|| child.named_child(0).filter(|n| n.kind() == "identifier"))
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or(""),
                ParamNameField::RustPattern => child
                    .child_by_field_name("pattern")
                    .and_then(|n| {
                        if n.kind() == "identifier" {
                            n.utf8_text(source).ok()
                        } else if n.kind() == "mut_pattern" {
                            n.named_child(0).and_then(|c| c.utf8_text(source).ok())
                        } else if n.kind() == "reference_pattern" {
                            n.named_child(0).and_then(|c| {
                                if c.kind() == "identifier" {
                                    c.utf8_text(source).ok()
                                } else if c.kind() == "mut_pattern" {
                                    c.named_child(0).and_then(|cc| cc.utf8_text(source).ok())
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        }
                    })
                    .unwrap_or(""),
            };

            if param_name.is_empty() || rule.skip_names.contains(&param_name) {
                continue;
            }
            record_binding(scopes, scope_idx, param_name, child.start_position().row);

            // Try the configured type field first, then fall back to child type nodes
            // (Swift parameters have user_type children instead of a "type" field)
            let mut type_node = child.child_by_field_name(rule.type_field);
            if type_node.is_none() {
                let mut tc = child.walk();
                for ch in child.named_children(&mut tc) {
                    if matches!(
                        ch.kind(),
                        "user_type" | "type_annotation" | "type_identifier"
                    ) {
                        type_node = Some(ch);
                        break;
                    }
                }
            }
            if let Some(tn) = type_node {
                let type_text = extract_base_type(tn, source);
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    scopes[scope_idx]
                        .types
                        .insert(param_name.to_string(), type_text);
                }
            }
        }
    }
}

/// Python/TS: `x = Foo()` or `x = func()`
fn scan_single_assignment(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let assign = if node.kind() == "assignment" {
        node
    } else {
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        match children
            .into_iter()
            .find(|c| c.kind() == "assignment" || c.kind() == "assignment_expression")
        {
            Some(a) => a,
            None => return,
        }
    };

    let left = match assign.child_by_field_name("left") {
        Some(l) => l,
        None => return,
    };
    let right = match assign.child_by_field_name("right") {
        Some(r) => r,
        None => return,
    };

    if left.kind() != "identifier" {
        return;
    }
    let var_name = match left.utf8_text(source) {
        Ok(n) => n.to_string(),
        Err(_) => return,
    };
    record_binding(scopes, scope_idx, &var_name, left.start_position().row);

    record_type_from_rhs(right, &var_name, scope_idx, scopes, source);
}

/// TS: `const x = new Foo()` or `const x: Type = ...` or `const x = func()`
/// Also handles Swift `let x = Foo(...)` and Kotlin `val x = Foo(...)`
fn scan_ts_var_declaration(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    // Java/C#: the declared type is a `type` field on the declaration node itself
    // (`Dog d = ...`), shared by every declarator. TS/JS put the annotation on the
    // declarator instead, so this is None there and the per-declarator check applies.
    let decl_type = node
        .child_by_field_name("type")
        .map(|n| extract_base_type(n, source))
        .filter(|t| !t.is_empty() && t.chars().next().map_or(false, |c| c.is_uppercase()));

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let var_name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .to_string();
            if var_name.is_empty() {
                continue;
            }
            let binding_row = child
                .child_by_field_name("name")
                .map(|n| n.start_position().row)
                .unwrap_or_else(|| child.start_position().row);
            record_binding(scopes, scope_idx, &var_name, binding_row);

            // Check for explicit type annotation: `const x: Foo = ...` (declarator-level)
            if let Some(type_ann) = child.child_by_field_name("type") {
                let type_text = extract_base_type(type_ann, source);
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    scopes[scope_idx].types.insert(var_name.clone(), type_text);
                    continue;
                }
            }

            // Declaration-level type annotation (Java/C#): `Dog d = ...`
            if let Some(type_text) = &decl_type {
                scopes[scope_idx]
                    .types
                    .insert(var_name.clone(), type_text.clone());
                continue;
            }

            // Check RHS value
            if let Some(value) = child.child_by_field_name("value") {
                record_type_from_rhs(value, &var_name, scope_idx, scopes, source);
            }
        }
    }

    if node.kind() == "property_declaration" {
        let var_names = swift_property_declaration_names(node, source);

        if !var_names.is_empty() {
            if let Some(name_nodes) = swift_property_declaration_name_nodes(node) {
                for (idx, var_name) in var_names.iter().enumerate() {
                    let binding_row = name_nodes
                        .get(idx)
                        .map(|name_node| name_node.start_position().row)
                        .unwrap_or_else(|| node.start_position().row);
                    record_binding(scopes, scope_idx, var_name, binding_row);
                }

                let type_names: Vec<Option<String>> = name_nodes
                    .iter()
                    .enumerate()
                    .map(|(idx, name_node)| {
                        swift_property_type_for_name(node, *name_node, idx, source)
                    })
                    .collect();
                for (idx, name_node) in name_nodes.iter().enumerate() {
                    let Some(var_name) = var_names.get(idx) else {
                        continue;
                    };
                    let type_name =
                        type_names
                            .get(idx)
                            .and_then(|name| name.clone())
                            .or_else(|| {
                                if type_names[..idx].iter().any(Option::is_some) {
                                    None
                                } else {
                                    type_names
                                        .iter()
                                        .skip(idx + 1)
                                        .find_map(|name| name.clone())
                                }
                            });
                    if let Some(type_name) = type_name {
                        if !type_name.is_empty()
                            && type_name.chars().next().map_or(false, |c| c.is_uppercase())
                        {
                            scopes[scope_idx].types.insert(var_name.clone(), type_name);
                            continue;
                        }
                    }
                    if let Some(value) =
                        swift_property_value_for_name(node, *name_node, idx, source)
                    {
                        record_type_from_rhs(value, var_name, scope_idx, scopes, source);
                    }
                }
            } else if let Some(var_name) = var_names.first() {
                record_binding(scopes, scope_idx, var_name, node.start_position().row);

                if let Some(type_ann) = node.child_by_field_name("type") {
                    let type_text = extract_base_type(type_ann, source);
                    if !type_text.is_empty()
                        && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                    {
                        scopes[scope_idx].types.insert(var_name.clone(), type_text);
                        return;
                    }
                }
                if let Some(value) = node.child_by_field_name("value") {
                    record_type_from_rhs(value, var_name, scope_idx, scopes, source);
                } else {
                    let mut c = node.walk();
                    for ch in node.named_children(&mut c) {
                        if ch.kind() == "call_expression" || ch.kind() == "new_expression" {
                            record_type_from_rhs(ch, var_name, scope_idx, scopes, source);
                            break;
                        }
                    }
                }
            }
            return;
        }

        // Kotlin: property_declaration > variable_declaration > identifier (+ user_type),
        // then a sibling RHS expression. tree-sitter-kotlin-ng exposes the name and the
        // type annotation positionally inside variable_declaration (no `name`/`type`
        // fields on property_declaration), so read them there.
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            if child.kind() != "variable_declaration" {
                continue;
            }
            let (var_name_kt, declared_type) = kotlin_positional_name_and_type(child, source);
            if var_name_kt.is_empty() {
                break;
            }

            // Explicit `val x: Type = ...` annotation wins.
            if let Some(type_text) = declared_type {
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    scopes[scope_idx].types.insert(var_name_kt, type_text);
                    return;
                }
            }

            // Otherwise infer from the RHS sibling expression.
            let mut c2 = node.walk();
            for sibling in node.named_children(&mut c2) {
                match sibling.kind() {
                    "call_expression" | "new_expression" => {
                        record_type_from_rhs(sibling, &var_name_kt, scope_idx, scopes, source);
                        break;
                    }
                    // `val x = obj.field` — defer until object types and the global
                    // class field-type map are known (inject_field_type_bindings).
                    "navigation_expression" => {
                        if let Some((obj, prop)) = kotlin_navigation_obj_prop(sibling, source) {
                            scopes[scope_idx]
                                .pending_field_types
                                .insert(var_name_kt.clone(), (obj, prop));
                        }
                        break;
                    }
                    _ => {}
                }
            }
            break;
        }
    }
}

/// Extract `(object_identifier, property)` from a kotlin-ng `navigation_expression`
/// of the simple form `ident.ident`. Returns None for anything more complex
/// (chained access, calls as the receiver, `this`, etc.).
fn kotlin_navigation_obj_prop(node: tree_sitter::Node, source: &[u8]) -> Option<(String, String)> {
    let mut cursor = node.walk();
    let idents: Vec<tree_sitter::Node> = node
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "identifier" || c.kind() == "simple_identifier")
        .collect();
    // Exactly object + property, both bare identifiers.
    if node.named_children(&mut node.walk()).count() != idents.len() || idents.len() != 2 {
        return None;
    }
    let obj = idents[0].utf8_text(source).ok()?.to_string();
    let prop = idents[1].utf8_text(source).ok()?.to_string();
    if obj.is_empty() || prop.is_empty() {
        None
    } else {
        Some((obj, prop))
    }
}

fn swift_property_declaration_names(node: tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for index in 0..node.child_count() {
        if node.field_name_for_child(index as u32) == Some("name") {
            if let Some(child) = node.child(index as u32) {
                if let Ok(name) = child.utf8_text(source) {
                    if !name.is_empty() {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }

    if !names.is_empty() {
        return names;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "pattern" {
            continue;
        }
        if let Some(id) = child.named_child(0) {
            if id.kind() == "simple_identifier" || id.kind() == "identifier" {
                if let Ok(name) = id.utf8_text(source) {
                    if !name.is_empty() {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }

    names
}

fn swift_property_declaration_name_nodes<'a>(
    node: tree_sitter::Node<'a>,
) -> Option<Vec<tree_sitter::Node<'a>>> {
    let mut nodes = Vec::new();
    for index in 0..node.child_count() {
        if node.field_name_for_child(index as u32) == Some("name") {
            if let Some(child) = node.child(index as u32) {
                nodes.push(child);
            }
        }
    }
    if nodes.is_empty() {
        None
    } else {
        Some(nodes)
    }
}

fn swift_property_value_for_name<'a>(
    node: tree_sitter::Node<'a>,
    name_node: tree_sitter::Node<'a>,
    name_index: usize,
    source: &[u8],
) -> Option<tree_sitter::Node<'a>> {
    let segment_end = swift_property_segment_end_for_name(node, name_node, name_index);

    for child_index in 0..node.child_count() {
        let Some(child) = node.child(child_index as u32) else {
            continue;
        };
        if child.start_byte() < name_node.end_byte() || child.start_byte() >= segment_end {
            continue;
        }
        let field_name = node.field_name_for_child(child_index as u32);
        if matches!(field_name, Some("value") | Some("computed_value"))
            || child.kind() == "call_expression"
            || child.kind() == "new_expression"
        {
            return Some(child);
        }
    }

    let segment = source
        .get(name_node.end_byte()..segment_end)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or("");
    if segment.contains('=') {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.start_byte() >= name_node.end_byte()
                && child.start_byte() < segment_end
                && (child.kind() == "call_expression" || child.kind() == "new_expression")
            {
                return Some(child);
            }
        }
    }

    None
}

fn swift_property_type_for_name(
    node: tree_sitter::Node,
    name_node: tree_sitter::Node,
    name_index: usize,
    source: &[u8],
) -> Option<String> {
    let segment_end = swift_property_segment_end_for_name(node, name_node, name_index);
    for child_index in 0..node.child_count() {
        let Some(child) = node.child(child_index as u32) else {
            continue;
        };
        if child.start_byte() < name_node.end_byte() || child.start_byte() >= segment_end {
            continue;
        }
        let field_name = node.field_name_for_child(child_index as u32);
        if field_name == Some("type") || child.kind() == "type_annotation" {
            let type_text = extract_base_type(child, source);
            if !type_text.is_empty() {
                return Some(type_text);
            }
        }
    }
    None
}

fn swift_property_segment_end_for_name(
    node: tree_sitter::Node,
    name_node: tree_sitter::Node,
    name_index: usize,
) -> usize {
    let name_nodes = swift_property_declaration_name_nodes(node).unwrap_or_default();
    let next_name_start = name_nodes.get(name_index + 1).map(|next| next.start_byte());
    let mut segment_end = next_name_start.unwrap_or_else(|| node.end_byte());

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == ","
            && child.start_byte() >= name_node.end_byte()
            && next_name_start.map_or(true, |next| child.start_byte() < next)
        {
            segment_end = child.start_byte();
            break;
        }
    }

    segment_end
}

/// Rust: `let x: Type = ...` or `let x = Foo::new()`
fn scan_rust_let_declaration(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let var_name = node
        .child_by_field_name("pattern")
        .and_then(|n| {
            // Pattern can be just an identifier or `mut x`
            if n.kind() == "identifier" {
                n.utf8_text(source).ok()
            } else if n.kind() == "mut_pattern" {
                n.named_child(0).and_then(|c| c.utf8_text(source).ok())
            } else {
                None
            }
        })
        .unwrap_or("")
        .to_string();

    if var_name.is_empty() {
        return;
    }
    record_binding(scopes, scope_idx, &var_name, node.start_position().row);

    // Check for explicit type annotation: `let x: Connection = ...`
    if let Some(type_node) = node.child_by_field_name("type") {
        let type_text = extract_base_type(type_node, source);
        if !type_text.is_empty() && type_text.chars().next().map_or(false, |c| c.is_uppercase()) {
            scopes[scope_idx].types.insert(var_name, type_text);
            return;
        }
    }

    // Check RHS value
    if let Some(value) = node.child_by_field_name("value") {
        record_type_from_rhs(value, &var_name, scope_idx, scopes, source);
    }
}

/// Go: `x := Foo{}` or `x := NewFoo()`
fn scan_go_short_var(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let left = match node.child_by_field_name("left") {
        Some(l) => l,
        None => return,
    };
    let right = match node.child_by_field_name("right") {
        Some(r) => r,
        None => return,
    };

    // left is expression_list, right is expression_list
    let var_name = if left.kind() == "expression_list" {
        left.named_child(0)
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("")
            .to_string()
    } else {
        left.utf8_text(source).unwrap_or("").to_string()
    };

    if var_name.is_empty() {
        return;
    }
    record_binding(scopes, scope_idx, &var_name, left.start_position().row);

    let rhs = if right.kind() == "expression_list" {
        match right.named_child(0) {
            Some(n) => n,
            None => return,
        }
    } else {
        right
    };

    record_type_from_rhs(rhs, &var_name, scope_idx, scopes, source);
}

/// Go: `var x Type = ...` or `var x = Foo{}`
fn scan_go_var_declaration(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "var_spec" {
            let var_name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .to_string();
            if var_name.is_empty() {
                // Try first named child as name
                if let Some(first) = child.named_child(0) {
                    if first.kind() == "identifier" {
                        let name = first.utf8_text(source).unwrap_or("").to_string();
                        if !name.is_empty() {
                            record_binding(scopes, scope_idx, &name, first.start_position().row);
                            // Check for type child
                            if let Some(type_node) = child.child_by_field_name("type") {
                                let type_text = extract_base_type(type_node, source);
                                if !type_text.is_empty()
                                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                                {
                                    scopes[scope_idx].types.insert(name, type_text);
                                }
                            }
                        }
                    }
                }
                continue;
            }
            let binding_row = child
                .child_by_field_name("name")
                .map(|n| n.start_position().row)
                .unwrap_or_else(|| child.start_position().row);
            record_binding(scopes, scope_idx, &var_name, binding_row);

            // Check for explicit type
            if let Some(type_node) = child.child_by_field_name("type") {
                let type_text = extract_base_type(type_node, source);
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    scopes[scope_idx].types.insert(var_name, type_text);
                    continue;
                }
            }

            // Check RHS value
            if let Some(value) = child.child_by_field_name("value") {
                let rhs = if value.kind() == "expression_list" {
                    value.named_child(0).unwrap_or(value)
                } else {
                    value
                };
                record_type_from_rhs(rhs, &var_name, scope_idx, scopes, source);
            }
        }
    }
}

/// Record type binding from a RHS expression (works for all languages).
/// Handles: constructor calls, new expressions, struct literals, function calls.
fn record_type_from_rhs(
    rhs: tree_sitter::Node,
    var_name: &str,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    match rhs.kind() {
        // Python/Go: Foo() or func()
        "call" | "call_expression" => {
            let func_node = rhs
                .child_by_field_name("function")
                .or_else(|| rhs.named_child(0));
            if let Some(func) = func_node {
                if func.kind() == "identifier"
                    || func.kind() == "simple_identifier"
                    || func.kind() == "type_identifier"
                {
                    let name = func.utf8_text(source).unwrap_or("");
                    if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                        scopes[scope_idx]
                            .types
                            .insert(var_name.to_string(), name.to_string());
                    } else {
                        scopes[scope_idx]
                            .pending_call_types
                            .insert(var_name.to_string(), name.to_string());
                    }
                }
                // Rust: Type::new() / Type::from() etc.
                if func.kind() == "scoped_identifier" {
                    let text = func.utf8_text(source).unwrap_or("");
                    let parts: Vec<&str> = text.split("::").collect();
                    if parts.len() >= 2 {
                        let type_name = parts[0];
                        let method_name = parts[parts.len() - 1];
                        if type_name.chars().next().map_or(false, |c| c.is_uppercase()) {
                            scopes[scope_idx]
                                .types
                                .insert(var_name.to_string(), type_name.to_string());
                        } else {
                            scopes[scope_idx]
                                .pending_call_types
                                .insert(var_name.to_string(), method_name.to_string());
                        }
                    }
                }
                // Go: package.NewFoo() or package.GetFoo()
                if func.kind() == "selector_expression" {
                    let field = func
                        .child_by_field_name("field")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    // Go convention: NewFoo() returns *Foo
                    if let Some(type_name) = field.strip_prefix("New") {
                        if !type_name.is_empty()
                            && type_name.chars().next().map_or(false, |c| c.is_uppercase())
                        {
                            scopes[scope_idx]
                                .types
                                .insert(var_name.to_string(), type_name.to_string());
                        }
                    } else if field.starts_with("Get")
                        || field.chars().next().map_or(false, |c| c.is_uppercase())
                    {
                        // Other Go package functions: record for return type resolution
                        scopes[scope_idx]
                            .pending_call_types
                            .insert(var_name.to_string(), field.to_string());
                    }
                }
            }
        }
        // TS: new Foo()
        "new_expression" => {
            if let Some(constructor) = rhs.child_by_field_name("constructor") {
                let name = constructor.utf8_text(source).unwrap_or("");
                if !name.is_empty() {
                    scopes[scope_idx]
                        .types
                        .insert(var_name.to_string(), name.to_string());
                }
            }
        }
        // Java/C#: new Foo()
        "object_creation_expression" => {
            if let Some(type_node) = rhs.child_by_field_name("type") {
                let name = extract_base_type(type_node, source);
                if !name.is_empty() && name.chars().next().map_or(false, |c| c.is_uppercase()) {
                    scopes[scope_idx].types.insert(var_name.to_string(), name);
                }
            }
        }
        // Go: Foo{} (composite_literal / struct literal)
        "composite_literal" => {
            if let Some(type_node) = rhs.child_by_field_name("type") {
                let name = type_node.utf8_text(source).unwrap_or("");
                if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                    scopes[scope_idx]
                        .types
                        .insert(var_name.to_string(), name.to_string());
                }
            }
        }
        _ => {}
    }
}

/// Extract the base type name from a type annotation node.
/// Strips pointers, references, generics to get just the type name.
fn extract_base_type(type_node: tree_sitter::Node, source: &[u8]) -> String {
    let text = type_node.utf8_text(source).unwrap_or("").trim().to_string();
    // Strip reference/pointer prefixes and mut keyword
    let text = text.trim_start_matches('&').trim_start_matches('*');
    let text = text.strip_prefix("mut ").unwrap_or(text).trim_start();
    // Strip generic parameters (angle brackets and Python-style square brackets)
    let text = if let Some(i) = text.find('<') {
        &text[..i]
    } else if let Some(i) = text.find('[') {
        &text[..i]
    } else {
        text
    };
    // Strip lifetime annotations for Rust
    let text = text.trim();
    // For type_annotation nodes in TS, strip the leading `: `
    let text = text.trim_start_matches(':').trim();
    text.to_string()
}

/// kotlin-ng exposes the name/type of `variable_declaration` and `class_parameter`
/// nodes positionally (an `identifier` followed by an optional `user_type`) rather
/// than via `name`/`type` fields. Returns (name, base_type) extracted that way.
fn kotlin_positional_name_and_type(
    node: tree_sitter::Node,
    source: &[u8],
) -> (String, Option<String>) {
    let mut cursor = node.walk();
    let mut name = String::new();
    let mut base_type: Option<String> = None;
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "identifier" | "simple_identifier" if name.is_empty() => {
                name = child.utf8_text(source).unwrap_or("").to_string();
            }
            "user_type" | "nullable_type" | "type_reference" if base_type.is_none() => {
                base_type = Some(
                    extract_base_type(child, source)
                        .trim_end_matches('?')
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    (name, base_type)
}

/// Parse Go receiver type from method content: `func (r *ReceiverType) Name(...)`
pub fn extract_go_receiver_type(content: &str) -> Option<String> {
    let after_func = content.strip_prefix("func")?.trim_start();
    let paren_start = after_func.find('(')?;
    let paren_end = after_func.find(')')?;
    let receiver_block = &after_func[paren_start + 1..paren_end];
    // Could be: "r ReceiverType", "r *ReceiverType", "*ReceiverType"
    let parts: Vec<&str> = receiver_block.split_whitespace().collect();
    let type_str = parts.last()?;
    let name = type_str.trim_start_matches('*');
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// pkg identifier (bare last-path-segment string — the local name a Go call
/// site actually spells, e.g. `v1` in `v1.Pod{}`) → every entity any
/// same-named package exports, each carrying its own declaring directory
/// (Go-admission finding): kubernetes has dozens
/// of packages literally named `v1`, one per API group, so a bucket keyed
/// only on this bare string is not a single package's symbol table — it is
/// the union of every package that happens to share the name. The
/// declaring-directory field is what lets [`register_go_package_imports`]
/// pick the one candidate a specific import actually means, instead of
/// inserting the whole polluted union into a file's `import_table`.
pub(crate) type GoPkgIndex = HashMap<String, Vec<(String, String, String)>>;

/// Build Go package index: pkg_name → [(entity_name, entity_id, declaring_dir)].
/// Maps each entity's containing directory name to itself — Go import paths
/// name packages, which are directories; Go has no per-file "package", so a
/// file's own name (independent of its declared `package` identifier) is not
/// a legitimate resolution key. An earlier revision of this function also
/// keyed on the file's own stripped-of-`.go` stem, matching the bare last
/// segment of a Go import path against a corpus file's *filename* rather
/// than its *directory*. That route was deleted (afterward
/// verification): kubernetes has real source files literally named after Go
/// standard-library packages (`os.go`, `time.go`, `errors.go`, ...), and
/// with no signal to tell "this bucket entry is the stdlib package" from
/// "this bucket entry is a corpus-local file that happens to share its bare
/// name," the file-stem route mis-resolved calls like `time.Now()` to a
/// same-named local file's own `Now` (majority of a ~5k-edge false-positive
/// class on kubernetes, gone once the route was removed). The directory
/// route below is the Go-correct heuristic and is unaffected by this
/// deletion.
pub(crate) fn build_go_pkg_index(
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> GoPkgIndex {
    let mut idx: GoPkgIndex = HashMap::default();
    for (name, target_ids) in symbol_table.iter() {
        for target_id in target_ids {
            if let Some(entity) = entity_map.get(target_id) {
                if !entity.file_path.ends_with(".go") {
                    continue;
                }
                let decl_dir = crate::parser::registry::go_package_dir(&entity.file_path);
                if let Some(parent_start) = entity.file_path.rfind('/') {
                    let parent_path = &entity.file_path[..parent_start];
                    let dir_name = parent_path.rsplit('/').next().unwrap_or(parent_path);
                    if !dir_name.is_empty() {
                        idx.entry(dir_name.to_string()).or_default().push((
                            name.clone(),
                            target_id.clone(),
                            decl_dir.to_string(),
                        ));
                    }
                }
            }
        }
    }
    for entries in idx.values_mut() {
        entries.sort_unstable();
    }
    idx
}

/// Scan function bodies/signatures for return types to build a return type map.
fn scan_return_types(
    root: tree_sitter::Node,
    file_lookup: &FileEntityLookup<'_>,
    source: &[u8],
    return_type_map: &mut HashMap<String, String>,
    config: &ScopeResolveConfig,
) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let kind = node.kind();

        let is_func = config.function_scope_nodes.contains(&kind);

        if is_func {
            let func_name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");

            let line = node.start_position().row + 1;
            let func_entity = file_lookup.find_at_line(func_name, line, |_| true);

            if let Some(fe) = func_entity {
                // Try explicit return type annotation first
                let ret_type = config
                    .return_type_field
                    .and_then(|field| {
                        node.child_by_field_name(field)
                            .map(|n| extract_base_type(n, source))
                            .filter(|t| {
                                !t.is_empty()
                                    && t.chars().next().map_or(false, |c| c.is_uppercase())
                            })
                    })
                    // kotlin-ng has no `type` field on the return position; the return
                    // type is a positional user_type after the parameter list.
                    .or_else(|| kotlin_positional_return_type(node, source));

                if let Some(rt) = ret_type {
                    return_type_map.insert(fe.id.clone(), rt);
                } else {
                    // Fall back to body heuristic: return ClassName()
                    if let Some(ret_type) = find_return_constructor(node, source) {
                        return_type_map.insert(fe.id.clone(), ret_type);
                    }
                }
            }
        }

        push_named_children_rev(&mut worklist, node);
    }
}

/// kotlin-ng exposes a function's declared return type as a positional `user_type`
/// child after the `function_value_parameters` (there is no `type` field). Returns
/// the base type name when it looks like a class (uppercase initial). Keyed off the
/// Kotlin-only parameter container, so it is a no-op for other languages.
fn kotlin_positional_return_type(func_node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = func_node.walk();
    let mut seen_params = false;
    for child in func_node.named_children(&mut cursor) {
        match child.kind() {
            "function_value_parameters" => seen_params = true,
            "user_type" | "nullable_type" | "type_reference" if seen_params => {
                let t = extract_base_type(child, source)
                    .trim_end_matches('?')
                    .to_string();
                return (!t.is_empty() && t.chars().next().map_or(false, |c| c.is_uppercase()))
                    .then_some(t);
            }
            "function_body" => break,
            _ => {}
        }
    }
    None
}

/// Find `return ClassName()` patterns in a function body (heuristic fallback).
fn find_return_constructor(root: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            // kotlin-ng wraps returns in `return_expression` and exposes the callee
            // as the first child of `call_expression` (no `function` field).
            if child.kind() == "return_expression" {
                let mut rc = child.walk();
                for ret_child in child.named_children(&mut rc) {
                    if ret_child.kind() == "call_expression" {
                        if let Some(callee) = ret_child.named_child(0) {
                            if matches!(callee.kind(), "identifier" | "simple_identifier") {
                                let name = callee.utf8_text(source).unwrap_or("");
                                if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                                    return Some(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
            if child.kind() == "return_statement" {
                let mut inner_cursor = child.walk();
                for ret_child in child.named_children(&mut inner_cursor) {
                    // Python: call, TS/Go: call_expression
                    if ret_child.kind() == "call" || ret_child.kind() == "call_expression" {
                        if let Some(func) = ret_child.child_by_field_name("function") {
                            if func.kind() == "identifier" {
                                let name = func.utf8_text(source).unwrap_or("");
                                if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                                    return Some(name.to_string());
                                }
                            }
                        }
                    }
                    // TS: new ClassName()
                    if ret_child.kind() == "new_expression" {
                        if let Some(constructor) = ret_child.child_by_field_name("constructor") {
                            let name = constructor.utf8_text(source).unwrap_or("");
                            if !name.is_empty() {
                                return Some(name.to_string());
                            }
                        }
                    }
                    // Go: StructName{} (composite_literal)
                    if ret_child.kind() == "composite_literal" {
                        if let Some(type_node) = ret_child.child_by_field_name("type") {
                            let name = type_node.utf8_text(source).unwrap_or("");
                            if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                                return Some(name.to_string());
                            }
                        }
                    }
                }
            }
            // Recurse into blocks (function_body wraps the block in kotlin-ng).
            let ck = child.kind();
            if ck == "block" || ck == "statement_block" || ck == "function_body" {
                worklist.push(child);
            }
        }
    }
    None
}

/// Scan for instance attribute types: __init__ self.attr patterns (Python/TS),
/// struct field declarations (Rust/Go).
fn scan_init_self_attrs(
    root: tree_sitter::Node,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    init_params_map: &mut HashMap<String, Vec<String>>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
    config: &ScopeResolveConfig,
) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let kind = node.kind();

        match &config.init_strategy {
            InitStrategy::ConstructorBody {
                class_nodes,
                init_node_kind,
                self_keyword: _,
                ..
            } => {
                if class_nodes.contains(&kind) {
                    let class_name = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("")
                        .to_string();

                    if !class_name.is_empty() {
                        // Determine lang for scan_class_for_init using init_node_kind as discriminator
                        let lang = match *init_node_kind {
                            "function_definition" => "python",
                            "method_definition" => "typescript",
                            "init_declaration" => "swift",
                            "anonymous_initializer" => "kotlin",
                            _ => "typescript",
                        };
                        scan_class_for_init(
                            node,
                            &class_name,
                            source,
                            instance_attr_types,
                            init_params_map,
                            attr_to_param_map,
                            lang,
                        );
                    }
                }
            }
            InitStrategy::StructFields { struct_nodes } => {
                if struct_nodes.contains(&kind) {
                    // Rust struct: extract field types directly
                    if kind == "struct_item" {
                        let struct_name = node
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("")
                            .to_string();

                        if !struct_name.is_empty() {
                            scan_rust_struct_fields(
                                node,
                                &struct_name,
                                source,
                                instance_attr_types,
                            );
                        }
                    }
                    // Go: extract field types from type declarations
                    if kind == "type_declaration" {
                        scan_go_struct_fields(node, source, instance_attr_types);
                    }
                }
            }
            InitStrategy::ClassFields { class_nodes } => {
                if class_nodes.contains(&kind) {
                    let class_name = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("")
                        .to_string();
                    if !class_name.is_empty() {
                        scan_java_class_fields(node, &class_name, source, instance_attr_types);
                    }
                }
            }
            InitStrategy::None => {}
        }

        push_named_children_rev(&mut worklist, node);
    }
}

/// Rust: extract field types from `struct Foo { conn: Connection, ... }`
fn scan_rust_struct_fields(
    node: tree_sitter::Node,
    struct_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "field_declaration_list" {
            let mut inner_cursor = child.walk();
            for field in child.named_children(&mut inner_cursor) {
                if field.kind() == "field_declaration" {
                    let field_name = field
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    let field_type = field
                        .child_by_field_name("type")
                        .map(|n| extract_base_type(n, source))
                        .unwrap_or_default();

                    if !field_name.is_empty()
                        && !field_type.is_empty()
                        && field_type
                            .chars()
                            .next()
                            .map_or(false, |c| c.is_uppercase())
                    {
                        instance_attr_types.insert(
                            (struct_name.to_string(), field_name.to_string()),
                            field_type,
                        );
                    }
                }
            }
        }
    }
}

/// Java/C#: extract field types from `class Foo { private Connection conn; ... }`.
/// A single field_declaration may declare several names (`private Foo a, b;`), all
/// sharing the declaration's `type` field.
fn scan_java_class_fields(
    class_node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let Some(body) = class_node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" {
            continue;
        }
        let field_type = member
            .child_by_field_name("type")
            .map(|n| extract_base_type(n, source))
            .unwrap_or_default();
        if field_type.is_empty()
            || !field_type
                .chars()
                .next()
                .map_or(false, |c| c.is_uppercase())
        {
            continue;
        }
        let mut dc = member.walk();
        for declarator in member.named_children(&mut dc) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name) = declarator
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
            {
                if !name.is_empty() {
                    instance_attr_types.insert(
                        (class_name.to_string(), name.to_string()),
                        field_type.clone(),
                    );
                }
            }
        }
    }
}

/// Go: extract field types from `type Foo struct { conn Connection; ... }`
fn scan_go_struct_fields(
    node: tree_sitter::Node,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_spec" {
            let struct_name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .to_string();

            if struct_name.is_empty() {
                continue;
            }

            // Look for struct_type child
            if let Some(type_node) = child.child_by_field_name("type") {
                if type_node.kind() == "struct_type" {
                    let mut fields_cursor = type_node.walk();
                    for field_list in type_node.named_children(&mut fields_cursor) {
                        if field_list.kind() == "field_declaration_list" {
                            let mut inner = field_list.walk();
                            for field in field_list.named_children(&mut inner) {
                                if field.kind() == "field_declaration" {
                                    // Go field: name type
                                    let field_name = field
                                        .child_by_field_name("name")
                                        .and_then(|n| n.utf8_text(source).ok())
                                        .unwrap_or("");
                                    let field_type = field
                                        .child_by_field_name("type")
                                        .map(|n| extract_base_type(n, source))
                                        .unwrap_or_default();

                                    if !field_name.is_empty()
                                        && !field_type.is_empty()
                                        && field_type
                                            .chars()
                                            .next()
                                            .map_or(false, |c| c.is_uppercase())
                                    {
                                        instance_attr_types.insert(
                                            (struct_name.clone(), field_name.to_string()),
                                            field_type,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn scan_class_for_init(
    root: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    init_params_map: &mut HashMap<String, Vec<String>>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
    lang: &str,
) {
    // Kotlin: extract primary constructor params (class_parameter nodes with val/var)
    if lang == "kotlin" {
        scan_kotlin_primary_constructor(root, class_name, source, instance_attr_types);
    }

    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let ck = child.kind();

            // Python __init__
            if ck == "function_definition" && lang == "python" {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                if name == "__init__" {
                    let params = extract_init_params(child, source);
                    let ordered_params = extract_init_param_names_ordered(child, source);
                    init_params_map.insert(class_name.to_string(), ordered_params);
                    scan_init_body(
                        child,
                        class_name,
                        &params,
                        source,
                        instance_attr_types,
                        attr_to_param_map,
                    );
                }
            }

            // TS constructor
            if ck == "method_definition" && lang == "typescript" {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                if name == "constructor" {
                    // Scan for this.attr = param patterns
                    scan_ts_constructor_body(
                        child,
                        class_name,
                        source,
                        instance_attr_types,
                        init_params_map,
                        attr_to_param_map,
                    );
                }
            }

            // Swift init_declaration
            if ck == "init_declaration" && lang == "swift" {
                scan_swift_init_body(
                    child,
                    class_name,
                    source,
                    instance_attr_types,
                    init_params_map,
                    attr_to_param_map,
                );
            }

            // Kotlin anonymous_initializer (init { ... } block)
            if ck == "anonymous_initializer" && lang == "kotlin" {
                scan_kotlin_init_body(
                    child,
                    class_name,
                    source,
                    instance_attr_types,
                    attr_to_param_map,
                );
            }

            // TS: typed class field declarations `private conn: Connection`
            if (ck == "public_field_definition"
                || ck == "property_declaration"
                || ck == "field_definition")
                && lang == "typescript"
            {
                let field_name = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                if let Some(type_ann) = child.child_by_field_name("type") {
                    let type_text = extract_base_type(type_ann, source);
                    if !field_name.is_empty()
                        && !type_text.is_empty()
                        && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                    {
                        instance_attr_types
                            .insert((class_name.to_string(), field_name.to_string()), type_text);
                    }
                }
            }

            // Swift: typed property declarations `var conn: Connection`
            if ck == "property_declaration" && lang == "swift" {
                scan_swift_property_declaration(child, class_name, source, instance_attr_types);
            }

            // Kotlin: typed property declarations `val conn: Connection`
            if ck == "property_declaration" && lang == "kotlin" {
                scan_kotlin_property_declaration(child, class_name, source, instance_attr_types);
            }

            if ck == "block"
                || ck == "class_body"
                || ck == "statement_block"
                || ck == "struct_body"
                || ck == "function_body"
                || ck == "code_block"
                || ck == "statements"
                || ck == "enum_class_body"
            {
                worklist.push(child);
            }
        }
    }
}

/// Swift: scan init body for `self.attr = param` patterns
fn scan_swift_init_body(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    init_params_map: &mut HashMap<String, Vec<String>>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    let params = extract_init_params(node, source);
    let ordered_params = extract_init_param_names_ordered(node, source);
    init_params_map.insert(class_name.to_string(), ordered_params);

    // Walk body looking for self.X = Y
    let mut worklist = vec![node];
    while let Some(wnode) = worklist.pop() {
        let mut cursor = wnode.walk();
        for child in wnode.named_children(&mut cursor) {
            let ck = child.kind();
            // Look for assignment: self.X = Y via directly_assigned_expression or assignment
            if ck == "directly_assigned_expression" || ck == "assignment" {
                if let Some(left) = child
                    .child_by_field_name("left")
                    .or_else(|| child.named_child(0))
                {
                    if left.kind() == "navigation_expression" {
                        let obj = left
                            .child_by_field_name("target")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let prop = left
                            .child_by_field_name("suffix")
                            .and_then(|n| n.utf8_text(source).ok())
                            .map(|text| text.strip_prefix('.').unwrap_or(text))
                            .unwrap_or("");
                        if obj == "self" && !prop.is_empty() {
                            if let Some(right) = child
                                .child_by_field_name("right")
                                .or_else(|| child.named_child(1))
                            {
                                if right.kind() == "simple_identifier"
                                    || right.kind() == "identifier"
                                {
                                    let rhs_name = right.utf8_text(source).unwrap_or("");
                                    if params.contains_key(rhs_name) {
                                        attr_to_param_map.insert(
                                            (class_name.to_string(), prop.to_string()),
                                            rhs_name.to_string(),
                                        );
                                        if let Some(Some(type_hint)) = params.get(rhs_name) {
                                            instance_attr_types.insert(
                                                (class_name.to_string(), prop.to_string()),
                                                type_hint.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if ck == "function_body"
                || ck == "code_block"
                || ck == "statements"
                || ck == "expression_statement"
                || ck == "block"
            {
                worklist.push(child);
            }
        }
    }
}

/// Swift: extract typed property declarations `var conn: Connection`
fn scan_swift_property_declaration(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let mut processed_pattern_binding = false;

    let mut cursor = node.walk();

    for child in node.named_children(&mut cursor) {
        if child.kind() == "pattern_binding" {
            processed_pattern_binding = true;
            scan_swift_property_binding(child, class_name, source, instance_attr_types);
        }
    }
    if processed_pattern_binding {
        return;
    }

    // Swift property_declaration nodes vary by grammar version. Some expose
    // pattern/type_annotation pairs directly instead of pattern_binding nodes.
    let mut pending_names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "pattern" | "simple_identifier" | "identifier" => {
                if let Some(name) = extract_swift_property_pattern_name(child, source) {
                    pending_names.push(name);
                }
            }
            "type_annotation" | "user_type" | "type_identifier" => {
                let type_text = extract_base_type(child, source);
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    for name in pending_names.drain(..) {
                        instance_attr_types
                            .insert((class_name.to_string(), name), type_text.clone());
                    }
                }
            }
            "call_expression" | "new_expression" | "value_argument" => pending_names.clear(),
            _ => {}
        }
    }
}

fn scan_swift_property_binding(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let mut field_names = Vec::new();
    let mut field_type = node.child_by_field_name("type").and_then(|type_node| {
        let type_text = extract_base_type(type_node, source);
        if type_text.is_empty() {
            None
        } else {
            Some(type_text)
        }
    });

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "pattern" | "simple_identifier" | "identifier" => {
                if let Some(name) = extract_swift_property_pattern_name(child, source) {
                    field_names.push(name);
                }
            }
            "type_annotation" => {
                if field_type.is_none() {
                    let type_text = extract_base_type(child, source);
                    if !type_text.is_empty() {
                        field_type = Some(type_text);
                    }
                }
            }
            _ => {}
        }
    }

    let Some(type_text) = field_type else {
        return;
    };
    if !type_text.chars().next().map_or(false, |c| c.is_uppercase()) {
        return;
    }

    for field_name in field_names {
        instance_attr_types.insert((class_name.to_string(), field_name), type_text.clone());
    }
}

fn extract_swift_property_pattern_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    if matches!(node.kind(), "simple_identifier" | "identifier") {
        let name = node.utf8_text(source).ok()?.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }

    if let Some(name_node) = node.child_by_field_name("name") {
        return extract_swift_property_pattern_name(name_node, source);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "simple_identifier" | "identifier") {
            return extract_swift_property_pattern_name(child, source);
        }
    }

    None
}

/// Kotlin: extract typed property declarations `val conn: Connection`
fn scan_kotlin_property_declaration(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    // kotlin-ng: property_declaration > variable_declaration > identifier + user_type.
    // The name/type are not exposed as fields on property_declaration, so dive into
    // the variable_declaration and read them positionally.
    let mut cursor = node.walk();
    let (field_name, field_type) = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "variable_declaration")
        .map(|vd| kotlin_positional_name_and_type(vd, source))
        .unwrap_or_default();
    let field_type = field_type.unwrap_or_default();

    if !field_name.is_empty()
        && !field_type.is_empty()
        && field_type
            .chars()
            .next()
            .map_or(false, |c| c.is_uppercase())
    {
        instance_attr_types.insert((class_name.to_string(), field_name.to_string()), field_type);
    }
}

/// Kotlin: extract primary constructor params with val/var as instance attributes
fn scan_kotlin_primary_constructor(
    class_node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    // Look for primary_constructor child, then class_parameter nodes. In
    // tree-sitter-kotlin-ng the class_parameters are wrapped in a `class_parameters`
    // node and expose name/type positionally (no `name`/`type` fields), so handle
    // both the wrapped and the direct layout.
    let mut cursor = class_node.walk();
    for child in class_node.named_children(&mut cursor) {
        if child.kind() != "primary_constructor" {
            continue;
        }
        let mut pc_cursor = child.walk();
        for pc_child in child.named_children(&mut pc_cursor) {
            let param_holder = if pc_child.kind() == "class_parameters" {
                pc_child
            } else {
                child
            };
            let mut p_cursor = param_holder.walk();
            for param in param_holder.named_children(&mut p_cursor) {
                if param.kind() != "class_parameter" {
                    continue;
                }
                // Only val/var class parameters become properties.
                let text = param.utf8_text(source).unwrap_or("");
                let has_val_var = text.contains("val ") || text.contains("var ");
                if !has_val_var {
                    continue;
                }
                let (param_name, param_type) = kotlin_positional_name_and_type(param, source);
                let param_type = param_type.unwrap_or_default();
                if !param_name.is_empty()
                    && !param_type.is_empty()
                    && param_type
                        .chars()
                        .next()
                        .map_or(false, |c| c.is_uppercase())
                {
                    instance_attr_types
                        .insert((class_name.to_string(), param_name.to_string()), param_type);
                }
            }
            // Avoid double-iterating when there is no class_parameters wrapper.
            if pc_child.kind() != "class_parameters" {
                break;
            }
        }
    }
}

/// Kotlin: scan init { ... } body for this.attr = expr patterns
fn scan_kotlin_init_body(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    let mut worklist = vec![node];
    while let Some(wnode) = worklist.pop() {
        let mut cursor = wnode.walk();
        for child in wnode.named_children(&mut cursor) {
            let ck = child.kind();
            if ck == "assignment" || ck == "directly_assigned_expression" {
                if let Some(left) = child
                    .child_by_field_name("left")
                    .or_else(|| child.named_child(0))
                {
                    if left.kind() == "navigation_expression" {
                        let obj = left
                            .child_by_field_name("expression")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let prop = left
                            .child_by_field_name("navigation_suffix")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        if obj == "this" && !prop.is_empty() {
                            if let Some(right) = child
                                .child_by_field_name("right")
                                .or_else(|| child.named_child(1))
                            {
                                if right.kind() == "simple_identifier"
                                    || right.kind() == "identifier"
                                {
                                    let rhs_name = right.utf8_text(source).unwrap_or("");
                                    attr_to_param_map.insert(
                                        (class_name.to_string(), prop.to_string()),
                                        rhs_name.to_string(),
                                    );
                                }
                                // If RHS is a constructor call, record type directly
                                if right.kind() == "call_expression" {
                                    let callee = right
                                        .child_by_field_name("function")
                                        .and_then(|n| n.utf8_text(source).ok())
                                        .unwrap_or("");
                                    if !callee.is_empty()
                                        && callee.chars().next().map_or(false, |c| c.is_uppercase())
                                    {
                                        instance_attr_types.insert(
                                            (class_name.to_string(), prop.to_string()),
                                            callee.to_string(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if ck == "statements" || ck == "block" || ck == "expression_statement" {
                worklist.push(child);
            }
        }
    }
}

/// TS: scan constructor body for `this.attr = param` patterns
fn scan_ts_constructor_body(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    init_params_map: &mut HashMap<String, Vec<String>>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    // Extract constructor params
    let params = extract_init_params(node, source);
    let ordered_params = extract_init_param_names_ordered(node, source);
    init_params_map.insert(class_name.to_string(), ordered_params);

    // Scan body for this.X = param
    scan_init_body_this(
        node,
        class_name,
        &params,
        source,
        instance_attr_types,
        attr_to_param_map,
    );
}

/// Scan constructor body for `this.attr = param` patterns (TS variant)
fn scan_init_body_this(
    root: tree_sitter::Node,
    class_name: &str,
    params: &HashMap<String, Option<String>>,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let ck = child.kind();
            if ck == "expression_statement" {
                // Look for assignment: this.X = Y
                let mut inner_cursor = child.walk();
                for inner in child.named_children(&mut inner_cursor) {
                    if inner.kind() == "assignment_expression" {
                        if let Some(left) = inner.child_by_field_name("left") {
                            if left.kind() == "member_expression" {
                                let obj = left
                                    .child_by_field_name("object")
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or("");
                                let prop = left
                                    .child_by_field_name("property")
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or("");
                                if obj == "this" && !prop.is_empty() {
                                    if let Some(right) = inner.child_by_field_name("right") {
                                        if right.kind() == "identifier" {
                                            let rhs_name = right.utf8_text(source).unwrap_or("");
                                            if params.contains_key(rhs_name) {
                                                attr_to_param_map.insert(
                                                    (class_name.to_string(), prop.to_string()),
                                                    rhs_name.to_string(),
                                                );
                                                if let Some(Some(type_hint)) = params.get(rhs_name)
                                                {
                                                    instance_attr_types.insert(
                                                        (class_name.to_string(), prop.to_string()),
                                                        type_hint.clone(),
                                                    );
                                                }
                                            }
                                        }
                                        if right.kind() == "new_expression" {
                                            if let Some(ctor) =
                                                right.child_by_field_name("constructor")
                                            {
                                                let name = ctor.utf8_text(source).unwrap_or("");
                                                if !name.is_empty() {
                                                    instance_attr_types.insert(
                                                        (class_name.to_string(), prop.to_string()),
                                                        name.to_string(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if ck == "statement_block" || ck == "block" {
                worklist.push(child);
            }
        }
    }
}

/// Extract __init__ parameter names in order (excluding self).
fn extract_init_param_names_ordered(func_node: tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(params_node) = func_node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.named_children(&mut cursor) {
            let param_name = if child.kind() == "identifier" {
                child.utf8_text(source).unwrap_or("").to_string()
            } else if child.kind() == "typed_parameter" || child.kind() == "typed_default_parameter"
            {
                child
                    .child_by_field_name("name")
                    .or_else(|| child.named_child(0))
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string()
            } else {
                continue;
            };
            if param_name != "self" && param_name != "cls" && !param_name.is_empty() {
                names.push(param_name);
            }
        }
    }
    names
}

fn extract_init_params(
    func_node: tree_sitter::Node,
    source: &[u8],
) -> HashMap<String, Option<String>> {
    let mut params = HashMap::default();
    if let Some(params_node) = func_node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.named_children(&mut cursor) {
            let param_name = if child.kind() == "identifier" {
                child.utf8_text(source).unwrap_or("").to_string()
            } else if child.kind() == "typed_parameter" || child.kind() == "typed_default_parameter"
            {
                child
                    .child_by_field_name("name")
                    .or_else(|| child.named_child(0))
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string()
            } else {
                continue;
            };
            if param_name != "self" && param_name != "cls" {
                // Check for type annotation
                let type_hint = child
                    .child_by_field_name("type")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(|s| s.to_string());
                params.insert(param_name, type_hint);
            }
        }
    }
    params
}

fn scan_init_body(
    root: tree_sitter::Node,
    class_name: &str,
    params: &HashMap<String, Option<String>>,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "expression_statement" || child.kind() == "assignment" {
                let assign = if child.kind() == "assignment" {
                    child
                } else {
                    let mut inner_cursor = child.walk();
                    let children: Vec<_> = child.named_children(&mut inner_cursor).collect();
                    match children.into_iter().find(|c| c.kind() == "assignment") {
                        Some(a) => a,
                        None => continue,
                    }
                };

                if let Some(left) = assign.child_by_field_name("left") {
                    if left.kind() == "attribute" {
                        let obj = left
                            .child_by_field_name("object")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let attr = left
                            .child_by_field_name("attribute")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");

                        if obj == "self" && !attr.is_empty() {
                            if let Some(right) = assign.child_by_field_name("right") {
                                if right.kind() == "identifier" {
                                    let rhs_name = right.utf8_text(source).unwrap_or("");
                                    // Record attr -> param mapping for later inference
                                    if params.contains_key(rhs_name) {
                                        attr_to_param_map.insert(
                                            (class_name.to_string(), attr.to_string()),
                                            rhs_name.to_string(),
                                        );
                                    }
                                    // If param has type hint, directly set the type
                                    if let Some(Some(type_hint)) = params.get(rhs_name) {
                                        instance_attr_types.insert(
                                            (class_name.to_string(), attr.to_string()),
                                            type_hint.clone(),
                                        );
                                    }
                                }
                                if right.kind() == "call" {
                                    if let Some(func) = right.child_by_field_name("function") {
                                        if func.kind() == "identifier" {
                                            let fname = func.utf8_text(source).unwrap_or("");
                                            if fname
                                                .chars()
                                                .next()
                                                .map_or(false, |c| c.is_uppercase())
                                            {
                                                instance_attr_types.insert(
                                                    (class_name.to_string(), attr.to_string()),
                                                    fname.to_string(),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if child.kind() == "block" {
                worklist.push(child);
            }
        }
    }
}

/// Infer constructor parameter types by analyzing call sites across all files.
/// For `Transaction(get_connection())`, we know get_connection() returns Connection,
/// so Transaction.__init__'s conn param has type Connection,
/// and self.conn in Transaction has type Connection.
/// `parsed_files`' AST-path files are supplemented with `precomputed_facts`'
/// [`PrecomputedFileFacts::ctor_call_sites`] (Field 11, MUL) for any file whose language [`mul_precompute_consumes_calls`]
/// admits — a fast-path file was never in `parsed_files` to begin with (its
/// tree died at the end of pass 1), so without this it would silently
/// contribute nothing to `instance_attr_types` the moment TREELESS starts
/// accepting `"call"`-bearing files for that language.
///
/// `file_paths` drives the merge order (not `parsed_files`'s own order):
/// on the production chunked-resolve path `parsed_files` is already exactly
/// `file_paths` with declined files filtered out (`pre_parsed` is `None`
/// there — no separate pre-parsed group to interleave), so this is order-
/// preserving for that path, and it is the only order in which a
/// precomputed-facts file and a freshly-parsed file can be merged uniformly
/// at all, since only `file_paths` names both.
#[allow(clippy::too_many_arguments)]
fn infer_constructor_param_types(
    parsed_files: &[(String, String, tree_sitter::Tree)],
    precomputed_facts: Option<&HashMap<String, PrecomputedFileFacts>>,
    file_paths: &[String],
    return_type_map: &HashMap<String, String>,
    init_params: &HashMap<String, Vec<String>>,
    attr_to_param: &HashMap<(String, String), String>,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    // the scan writes to `instance_attr_types` only from inside
    // `if let Some(param_names) = init_params.get(callee)` *and* `if let
    // Some(attrs) = attr_to_param_index.get(..)` (see `apply_ctor_call_facts`).
    // With either input empty the whole scan is a provable no-op, so
    // returning here cannot change the result — but it does skip the two
    // whole-corpus folds below (`deterministic_return_types_by_name` alone is
    // `O(every id in symbol_table)`, and this runs once per 5,000-file chunk:
    // ~83ms of the monster's warm rebuild, entirely thrown away on a corpus
    // with no constructor-parameter facts at all).
    if init_params.is_empty() || attr_to_param.is_empty() {
        return;
    }
    let func_name_returns =
        deterministic_return_types_by_name(return_type_map, symbol_table, entity_map);
    let attr_to_param_index = build_attr_to_param_index(attr_to_param);

    let parsed_by_path: HashMap<&str, &(String, String, tree_sitter::Tree)> = parsed_files
        .iter()
        .map(|entry| (entry.0.as_str(), entry))
        .collect();

    // Scan every file for constructor call sites: ClassName(arg1, arg2, ...).
    // Parallelized: each file produces local results, then merged in
    // `file_paths` order (see this function's own doc comment for why).
    let local_results: Vec<HashMap<(String, String), String>> = maybe_par_iter!(file_paths)
        .filter_map(|file_path| {
            let owned;
            let descriptors: &[CtorCallFacts] = if let Some(facts) =
                precomputed_facts.and_then(|m| m.get(file_path))
            {
                if facts.ctor_call_sites.is_empty() {
                    return None;
                }
                prof::add_precomputed_ctor_call_engagement(1, facts.ctor_call_sites.len() as u64);
                &facts.ctor_call_sites
            } else if let Some(entry) = parsed_by_path.get(file_path.as_str()) {
                let source = entry.1.as_bytes();
                owned = record_ctor_call_sites(entry.2.root_node(), source);
                if owned.is_empty() {
                    return None;
                }
                &owned
            } else {
                return None;
            };
            let mut local_attr_types: HashMap<(String, String), String> = HashMap::default();
            apply_ctor_call_facts(
                descriptors,
                &func_name_returns,
                init_params,
                &attr_to_param_index,
                &mut local_attr_types,
            );
            if local_attr_types.is_empty() {
                None
            } else {
                Some(local_attr_types)
            }
        })
        .collect();

    for local in local_results {
        let mut local_entries: Vec<((String, String), String)> = local.into_iter().collect();
        local_entries.sort_unstable();
        for (key, val) in local_entries {
            instance_attr_types.entry(key).or_insert(val);
        }
    }
}

/// `name -> return type`, where the type is the one carried by the *first* id
/// in that name's `symbol_table` bucket that has one.
///
/// Driven by `return_type_map`'s keys rather than by `symbol_table`'s.
/// The two enumerate the same result: a name gets an entry iff some id in its
/// bucket is in `return_type_map`, and every such id's own entity is named that
/// same name (`symbol_table[name]` is exactly the ids of entities named `name`,
/// and `entity_map[id].name` is that entity's name), so mapping each
/// `return_type_map` key through `entity_map` reaches every name that can
/// possibly get an entry — and nothing else. The value is computed by the
/// identical `find_map` over the identical bucket, so it is the same map, not
/// merely a similar one.
///
/// The difference is the work: the old form ran `find_map` for *every* name in
/// the corpus, i.e. one `return_type_map` probe per entity id in the corpus
/// (~454k on the monster), once per 5,000-file chunk. `return_type_map` is
/// chunk-scoped and far smaller, and this form touches only the buckets that
/// can produce a hit.
fn deterministic_return_types_by_name(
    return_type_map: &HashMap<String, String>,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> HashMap<String, String> {
    let mut by_name: HashMap<String, String> =
        HashMap::with_capacity_and_hasher(return_type_map.len(), Default::default());
    for id in return_type_map.keys() {
        let Some(name) = entity_map.get(id).map(|info| info.name.as_str()) else {
            continue;
        };
        if by_name.contains_key(name) {
            continue;
        }
        let Some(target_ids) = symbol_table.get(name) else {
            continue;
        };
        if let Some(return_type) = target_ids
            .iter()
            .find_map(|target_id| return_type_map.get(target_id))
        {
            by_name.insert(name.to_string(), return_type.clone());
        }
    }
    by_name
}

fn build_attr_to_param_index(
    attr_to_param: &HashMap<(String, String), String>,
) -> AttrToParamIndex<'_> {
    let mut index: AttrToParamIndex<'_> =
        HashMap::with_capacity_and_hasher(attr_to_param.len(), Default::default());
    for ((class_name, attr_name), param_name) in attr_to_param {
        index
            .entry((class_name.as_str(), param_name.as_str()))
            .or_default()
            .push((class_name.as_str(), attr_name.as_str()));
    }
    for attrs in index.values_mut() {
        attrs.sort_unstable();
    }
    index
}

/// Field 11 (MUL phase 2/3): one descriptor per
/// constructor-call-shaped `"call"` node — the syntactic half of the former
/// `scan_constructor_calls`, minus the corpus-dependent lookups it used to
/// make inline (`init_params`/`attr_to_param_index`/`func_name_returns`, all
/// only ever available after every file in the corpus has been scanned).
/// [`record_ctor_call_sites`] builds these from `(root, source)` alone;
/// [`apply_ctor_call_facts`] is the corpus-dependent other half. Mirrors
/// Field 10's `ImportStmtFacts`/`record_import_stmts_pruned`/
/// `dispatch_import_stmts_from_facts` split exactly.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CtorCallFacts {
    /// The call's `function` field text — already filtered to "identifier
    /// node, uppercase first character", the one purely-syntactic pre-filter
    /// the original scan applied before ever consulting `init_params`, so
    /// applying it at record time discards nothing a corpus-side lookup
    /// could still act on.
    callee: String,
    /// One entry per named child of the call's `arguments` node, in order.
    /// `Some(name)` iff that argument is itself a `"call"` node whose own
    /// `function` field is a bare `identifier` — the only argument shape
    /// the original `infer_expr_type` ever resolved to a type (its
    /// `"identifier"` and catch-all arms are both unconditionally `None`).
    /// `name` is recorded uninterpreted: whether it resolves to a
    /// constructor type (uppercase) or a function's declared return type
    /// (via `func_name_returns`) is corpus-dependent and stays in
    /// [`infer_arg_type_from_shape`].
    arg_shapes: Vec<Option<String>>,
}

/// Record [`CtorCallFacts`] in the same worklist order `scan_constructor_calls`
/// used to walk directly (`push_named_children_rev`) — a pure function of one
/// file's tree, no corpus-wide map consulted, matching Field 10's
/// `record_import_stmts_pruned`.
fn record_ctor_call_sites(root: tree_sitter::Node, source: &[u8]) -> Vec<CtorCallFacts> {
    let mut out = Vec::new();
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        if node.kind() == "call" {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "identifier" {
                    let callee = func.utf8_text(source).unwrap_or("");
                    if callee.chars().next().map_or(false, |c| c.is_uppercase()) {
                        let mut arg_shapes = Vec::new();
                        if let Some(args_node) = node.child_by_field_name("arguments") {
                            let mut args_cursor = args_node.walk();
                            for arg in args_node.named_children(&mut args_cursor) {
                                arg_shapes.push(record_arg_call_shape(arg, source));
                            }
                        }
                        out.push(CtorCallFacts {
                            callee: callee.to_string(),
                            arg_shapes,
                        });
                    }
                }
            }
        }
        push_named_children_rev(&mut worklist, node);
    }
    out
}

/// The one argument shape `infer_expr_type`/[`infer_arg_type_from_shape`] can
/// ever resolve to a type: the argument is itself a `"call"` node whose
/// `function` field is a bare `identifier`. Every other shape resolves to
/// `None` deterministically, with no corpus data, so recording anything for
/// those would be dead weight the apply side could never use.
fn record_arg_call_shape(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    if node.kind() != "call" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    if func.kind() != "identifier" {
        return None;
    }
    Some(func.utf8_text(source).unwrap_or("").to_string())
}

/// Replays [`record_ctor_call_sites`]' descriptors against the corpus-wide
/// maps `scan_constructor_calls` used to consult inline — the only
/// corpus-dependent step, run after every file's own scan has been merged
/// (Field 11). One-to-one with the old function's body:
/// `arg_shapes[i]` stands in for `infer_expr_type(arg, source,
/// func_name_returns)`, resolved by [`infer_arg_type_from_shape`].
fn apply_ctor_call_facts(
    descriptors: &[CtorCallFacts],
    func_name_returns: &HashMap<String, String>,
    init_params: &HashMap<String, Vec<String>>,
    attr_to_param_index: &AttrToParamIndex<'_>,
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    for facts in descriptors {
        let Some(param_names) = init_params.get(&facts.callee) else {
            continue;
        };
        for (arg_idx, shape) in facts.arg_shapes.iter().enumerate() {
            if arg_idx >= param_names.len() {
                break;
            }
            let param_name = &param_names[arg_idx];
            let Some(arg_type) = infer_arg_type_from_shape(shape, func_name_returns) else {
                continue;
            };
            if let Some(attrs) =
                attr_to_param_index.get(&(facts.callee.as_str(), param_name.as_str()))
            {
                for (cn, attr) in attrs {
                    instance_attr_types
                        .entry(((*cn).to_string(), (*attr).to_string()))
                        .or_insert_with(|| arg_type.clone());
                }
            }
        }
    }
}

/// The corpus-dependent half of the old `infer_expr_type`'s `"call"` arm:
/// given the recorded callee name of a `"call"`-shaped argument, resolve its
/// type — the callee itself if it looks like a constructor (uppercase first
/// character), or its declared return type from `func_name_returns`
/// otherwise. `None` (no recorded shape, i.e. every other argument kind)
/// stays `None`, exactly matching `infer_expr_type`'s `"identifier"` and
/// catch-all arms.
fn infer_arg_type_from_shape(
    shape: &Option<String>,
    func_name_returns: &HashMap<String, String>,
) -> Option<String> {
    let name = shape.as_ref()?;
    if name.chars().next().map_or(false, |c| c.is_uppercase()) {
        Some(name.clone())
    } else {
        func_name_returns.get(name).cloned()
    }
}

/// Resolve pending call types using the return type map.
/// For scopes with `x = func()` where func has a known return type, bind x to that type.
fn inject_return_type_bindings(
    scopes: &mut Vec<Scope>,
    func_name_return_types: &HashMap<String, String>,
    return_type_map: &HashMap<String, String>,
    import_table_by_name: &HashMap<&str, &str>,
    rec: &mut Recorder,
) {
    // Resolve pending call types in all scopes
    for scope_idx in 0..scopes.len() {
        let mut resolved: Vec<(String, String)> = Vec::new();
        for (var_name, func_name) in &scopes[scope_idx].pending_call_types {
            // Both tables are corpus/chunk-wide, so both reads are recorded.
            // `import_table_by_name` is this file's own slice (recorded once by
            // the caller as `Table::ImportsForFile`), but the *return type it
            // leads to* belongs to whichever file defines the import target.
            let imported = import_table_by_name.get(func_name.as_str()).copied();
            if let Some(target_id) = imported {
                rec.one(Table::ReturnTypeMap, target_id);
            }
            rec.one(Table::FuncNameReturnTypes, func_name.as_str());
            if let Some(ret_type) = imported
                .and_then(|target_id| return_type_map.get(target_id))
                .or_else(|| func_name_return_types.get(func_name))
            {
                resolved.push((var_name.clone(), ret_type.clone()));
            }
        }

        for (var_name, ret_type) in resolved {
            scopes[scope_idx].types.insert(var_name, ret_type);
        }
    }
}

/// Resolve `val x = obj.field` field-access assignments now that object variable
/// types (parameters, locals) and the global class field-type map are both
/// available. A resolved variable can itself be the object of another pending
/// access (`val a = x.b; val c = a.d`), so iterate to a small fixpoint.
fn inject_field_type_bindings(
    scopes: &mut Vec<Scope>,
    instance_attr_types: &HashMap<(String, String), String>,
    rec: &mut Recorder,
) {
    for _ in 0..4 {
        // Collect resolutions under immutable borrows, then apply mutably.
        let mut resolutions: Vec<(usize, String, String)> = Vec::new();
        for scope_idx in 0..scopes.len() {
            for (var, (obj, prop)) in &scopes[scope_idx].pending_field_types {
                if let Some(obj_type) = lookup_type_in_scopes(scope_idx, scopes, obj) {
                    rec.two(Table::InstanceAttrTypes, &obj_type, prop);
                    if let Some(field_type) = instance_attr_types.get(&(obj_type, prop.clone())) {
                        resolutions.push((scope_idx, var.clone(), field_type.clone()));
                    }
                }
            }
        }
        if resolutions.is_empty() {
            break;
        }
        for (scope_idx, var, field_type) in resolutions {
            scopes[scope_idx].types.insert(var.clone(), field_type);
            scopes[scope_idx].pending_field_types.remove(&var);
        }
    }
}

fn build_ts_default_export_table(
    parsed_files: &[(String, String, tree_sitter::Tree)],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> TsDefaultExportTable {
    // Per-file AST extraction is independent, so run it in parallel and merge.
    // Collecting preserves file order, so the merged result matches a sequential scan.
    let per_file: Vec<(Option<(String, String)>, Vec<TsDefaultReExport>)> =
        maybe_par_iter!(parsed_files)
            .filter_map(|(file_path, content, tree)| {
                if !is_js_ts_file(file_path) {
                    return None;
                }

                let extracted = extract_ts_default_exports(tree.root_node(), content.as_bytes());
                let mut default_export: Option<(String, String)> = None;
                for name in extracted.names {
                    let Some(target_ids) = symbol_table.get(&name) else {
                        continue;
                    };
                    let target = target_ids.iter().find(|id| {
                        entity_map.get(*id).map_or(false, |entity| {
                            entity.file_path == *file_path && entity.parent_id.is_none()
                        })
                    });
                    if let Some(target_id) = target {
                        default_export = Some((file_path.clone(), target_id.clone()));
                    }
                }

                let re_exports: Vec<TsDefaultReExport> = extracted
                    .re_exports
                    .into_iter()
                    .map(|(original_name, module_path)| TsDefaultReExport {
                        file_path: file_path.clone(),
                        original_name,
                        module_path,
                    })
                    .collect();

                Some((default_export, re_exports))
            })
            .collect();

    let mut default_exports = HashMap::default();
    let mut re_exports = Vec::new();
    for (default_export, file_re_exports) in per_file {
        if let Some((file_path, target_id)) = default_export {
            default_exports.insert(file_path, target_id);
        }
        re_exports.extend(file_re_exports);
    }

    resolve_ts_default_re_exports(&mut default_exports, re_exports, symbol_table, entity_map);
    let sorted_files = sorted_default_export_files(&default_exports);

    TsDefaultExportTable {
        exports_by_file: default_exports,
        sorted_files,
    }
}

fn sorted_default_export_files(default_exports: &HashMap<String, String>) -> Vec<String> {
    let mut sorted_files: Vec<String> = default_exports.keys().cloned().collect();
    sort_import_candidate_files(&mut sorted_files, JS_TS_EXTENSIONS);
    sorted_files
}

fn resolve_ts_default_re_exports(
    default_exports: &mut HashMap<String, String>,
    pending: Vec<TsDefaultReExport>,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) {
    let mut pending = pending;
    while !pending.is_empty() {
        let sorted_files = sorted_default_export_files(default_exports);
        let mut unresolved = Vec::new();
        let mut progressed = false;

        for re_export in pending {
            let target_id = if re_export.original_name == "default" {
                find_import_file(
                    &sorted_files,
                    &re_export.module_path,
                    &re_export.file_path,
                    JS_TS_EXTENSIONS,
                )
                .and_then(|target_file| default_exports.get(target_file))
                .cloned()
            } else {
                symbol_table
                    .get(&re_export.original_name)
                    .and_then(|target_ids| {
                        find_import_target(
                            target_ids,
                            &re_export.module_path,
                            &re_export.file_path,
                            JS_TS_EXTENSIONS,
                            entity_map,
                        )
                        .cloned()
                    })
            };

            if let Some(target_id) = target_id {
                default_exports.insert(re_export.file_path, target_id);
                progressed = true;
            } else {
                unresolved.push(re_export);
            }
        }

        if !progressed {
            break;
        }
        pending = unresolved;
    }
}

/// Build once per build (via a caller-held `OnceLock`), grouping every
/// top-level (`parent_id.is_none()`) entity whose file matches `extensions`
/// by file — so a namespace-import specifier resolves via `O(1)`
/// `entities_by_file`/`stem_index` lookups instead of an `O(symbol_table)`
/// scan repeated once per import statement. Originally JS/TS-only
/// (hardcoded `is_js_ts_file`); generalized so Python's
/// `register_namespace_import` — previously the one caller *without* this
/// index, scanning the whole corpus per bare `import module` statement, see
/// its doc comment — can share the same structure and the same fix shape.
fn build_top_level_entity_index(
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    extensions: &[&str],
) -> TopLevelEntityIndex {
    let mut entities_by_file: HashMap<String, Vec<(String, String)>> = HashMap::default();

    for (name, target_ids) in symbol_table {
        for target_id in target_ids {
            let Some(info) = entity_map.get(target_id) else {
                continue;
            };
            let matches_ext = extensions.iter().any(|ext| info.file_path.ends_with(ext));
            if !matches_ext || info.parent_id.is_some() {
                continue;
            }
            entities_by_file
                .entry(info.file_path.clone())
                .or_default()
                .push((name.clone(), target_id.clone()));
        }
    }

    let mut sorted_files: Vec<String> = entities_by_file.keys().cloned().collect();
    sort_import_candidate_files(&mut sorted_files, extensions);
    let stem_index = build_owned_stem_index(&sorted_files);

    TopLevelEntityIndex {
        entities_by_file,
        stem_index,
    }
}

struct TsDefaultExports {
    names: Vec<String>,
    re_exports: Vec<(String, String)>,
}

fn extract_ts_default_exports(root: tree_sitter::Node, source: &[u8]) -> TsDefaultExports {
    let mut names = Vec::new();
    let mut re_exports = Vec::new();
    let mut worklist = vec![root];

    while let Some(node) = worklist.pop() {
        if node.kind() == "export_statement" {
            let has_source = node.child_by_field_name("source").is_some();
            let source_path = node
                .child_by_field_name("source")
                .and_then(|n| n.utf8_text(source).ok())
                .map(|text| {
                    text.trim_matches(|c: char| c == '\'' || c == '"')
                        .to_string()
                });
            let text = node.utf8_text(source).unwrap_or("");
            if !has_source {
                if let Some(declaration) = node.child_by_field_name("declaration") {
                    if text.contains("default") {
                        if let Some(name) = ts_default_declaration_name(declaration, source) {
                            names.push(name);
                        }
                    }
                } else if text.contains("default") && !has_ts_export_specifier(node) {
                    if let Some(name) = ts_bare_default_export_identifier(node, source) {
                        names.push(name);
                    }
                }
            }
            collect_ts_default_export_specifiers(
                node,
                source,
                source_path.as_deref(),
                &mut names,
                &mut re_exports,
            );
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            worklist.push(child);
        }
    }

    TsDefaultExports { names, re_exports }
}

fn ts_default_declaration_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "abstract_class_declaration"
        | "lexical_declaration"
        | "variable_declaration" => ts_declaration_name(node, source),
        "identifier" => node.utf8_text(source).ok().map(str::to_string),
        _ => None,
    }
}

fn has_ts_export_specifier(node: tree_sitter::Node) -> bool {
    let mut worklist = vec![node];
    while let Some(current) = worklist.pop() {
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            if child.kind() == "export_specifier" {
                return true;
            }
            worklist.push(child);
        }
    }
    false
}

fn collect_ts_default_export_specifiers(
    node: tree_sitter::Node,
    source: &[u8],
    source_path: Option<&str>,
    names: &mut Vec<String>,
    re_exports: &mut Vec<(String, String)>,
) {
    let mut worklist = vec![node];
    while let Some(current) = worklist.pop() {
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            if child.kind() == "export_specifier" {
                let original = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                let local = child
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or(original);
                if local == "default" && !original.is_empty() {
                    if let Some(source_path) = source_path {
                        re_exports.push((original.to_string(), source_path.to_string()));
                    } else {
                        names.push(original.to_string());
                    }
                }
            } else {
                worklist.push(child);
            }
        }
    }
}

fn ts_declaration_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(name.utf8_text(source).ok()?.to_string());
    }

    if node.kind() == "lexical_declaration" || node.kind() == "variable_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                if let Some(name) = child.child_by_field_name("name") {
                    return Some(name.utf8_text(source).ok()?.to_string());
                }
            }
        }
    }

    let mut cursor = node.walk();
    let name = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "identifier" | "type_identifier"))
        .and_then(|child| child.utf8_text(source).ok())
        .map(str::to_string);
    name
}

fn ts_bare_default_export_identifier(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?.trim();
    let rest = text.strip_prefix("export")?.trim_start();
    let rest = rest.strip_prefix("default")?.trim_start();
    let name_end = js_ts_identifier_end(rest)?;
    let name = &rest[..name_end];
    let trailing = rest[name_end..].trim_start();
    only_js_ts_statement_trivia(trailing).then(|| name.to_string())
}

fn js_ts_identifier_end(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
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
    Some(end)
}

fn only_js_ts_statement_trivia(mut text: &str) -> bool {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix(';') {
            text = rest;
            continue;
        }
        if text.is_empty() {
            return true;
        }
        if text.starts_with("//") {
            return true;
        }
        if let Some(rest) = text.strip_prefix("/*") {
            let Some(end) = rest.find("*/") else {
                return false;
            };
            text = &rest[end + 2..];
            continue;
        }
        return false;
    }
}

/// Extract import statements from the AST.
///
/// `rec` records every cross-file read the Python/Rust/Go branches below make
/// against `symbol_table`/`entity_map`/`go_pkg_index`. The JS/TS
/// branches do not take a recorder: they run only when `skip_js_ts_imports` is
/// false, which — inside a [`crate::parser::session::GraphSession`] build —
/// never happens, because `pre_built_import_table` is always `Some` there and
/// JS/TS import resolution goes through the already-instrumented
/// incremental import table (`Table::ImportsForFile`) instead. Outside a
/// session (the plain `EntityGraph::build` cold path) `rec` is always
/// `Recorder::off()`, so recording here is a no-op regardless.
///
/// No longer called on the build path — the fused triple
/// walk records the handled set and `record_import_stmts_pruned` +
/// `dispatch_import_stmts_from_facts` run the handlers. Kept alive,
/// deliberately, as the executable specification the
/// fusion-witness invariant test holds the fused path to (an independent-oracle
/// discipline: the unfused side is the spec, so the test cannot degrade into
/// "the new code agrees with itself").
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn extract_imports_from_ast<'a>(
    root: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    config: &ScopeResolveConfig,
    go_pkg_index: &GoPkgIndex,
    ts_default_exports: &TsDefaultExportTable,
    top_level_entities: &OnceLock<TopLevelEntityIndex>,
    py_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    rust_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    parsed_files: &'a [(String, String, tree_sitter::Tree)],
    content_by_file: &OnceLock<HashMap<&'a str, &'a str>>,
    exported_names_by_file: &Mutex<HashMap<String, Arc<HashSet<String>>>>,
    skip_js_ts_imports: bool,
    rec: &mut Recorder,
) {
    // MUL phase 2 (MUL): dispatch-direct —
    // build one `ImportStmtFacts` descriptor per handled node and resolve it
    // immediately, on this traversal's own stack frame, no batching. This is
    // the "dispatch-direct" side the record-then-dispatch composition
    // (`record_import_stmts_pruned` + `dispatch_import_stmts_from_facts`,
    // pass 2's one production caller) is checked against (see
    // `record_then_dispatch_matches_dispatch_direct` below): both now share
    // the same descriptor type and the same `dispatch_import_stmt`, so what
    // the fusion-witness invariant test still isolates is the traversal driver
    // (full recursive-via-worklist here vs pruned-subtree-only there),
    // exactly as before this refactor — the per-node body was already shared.
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match classify_import_stmt(child.kind(), config) {
                Some(stmt) => {
                    let descriptor =
                        build_import_stmt_facts(stmt, child, source, skip_js_ts_imports);
                    dispatch_import_stmt(
                        &descriptor,
                        file_path,
                        symbol_table,
                        entity_map,
                        import_table,
                        scopes,
                        go_pkg_index,
                        ts_default_exports,
                        top_level_entities,
                        py_top_level_entities,
                        rust_top_level_entities,
                        parsed_files,
                        content_by_file,
                        exported_names_by_file,
                        skip_js_ts_imports,
                        rec,
                    );
                }
                None => worklist.push(child),
            }
        }
    }
}

/// The import-statement node kinds `extract_imports_from_ast` handles (and
/// therefore never descends into), classified. Pure in `(kind, config)` —
/// factored out so the fused walk's recorder, the pruned replay,
/// and the unfused walk agree on the handled set H by construction: H =
/// {named node c : classify(c) is Some ∧ no ancestor of c is in H}.
///
/// `import_declaration` is the Go handler's arm but the kind also occurs in
/// Java/Swift grammars, where the handler runs and resolves nothing —
/// long-standing behavior, preserved as-is.
#[derive(Clone, Copy)]
enum ImportStmtKind {
    /// Python `from x import y` (`import_from_statement`).
    PyFromImport,
    /// Python `import mod [as m]` (`import_statement` when the config knows
    /// both `self` and `cls`).
    PyModuleImport,
    /// JS/TS `import ... from '...'` (`import_statement` when the config does
    /// not know `cls`).
    TsImport,
    /// JS/TS `export ... from '...'` (`export_statement`, same gate).
    TsReExport,
    /// Rust `use ...` (`use_declaration`).
    RustUse,
    /// Go `import (...)` (`import_declaration`).
    GoImport,
}

fn classify_import_stmt(kind: &str, config: &ScopeResolveConfig) -> Option<ImportStmtKind> {
    match kind {
        "import_from_statement" => Some(ImportStmtKind::PyFromImport),
        "import_statement"
            if config.self_keywords.contains(&"self") && config.self_keywords.contains(&"cls") =>
        {
            Some(ImportStmtKind::PyModuleImport)
        }
        "import_statement" if !config.self_keywords.contains(&"cls") => {
            Some(ImportStmtKind::TsImport)
        }
        "export_statement" if !config.self_keywords.contains(&"cls") => {
            Some(ImportStmtKind::TsReExport)
        }
        "use_declaration" => Some(ImportStmtKind::RustUse),
        "import_declaration" => Some(ImportStmtKind::GoImport),
        _ => None,
    }
}

/// (MUL phase 2): one serializable
/// descriptor per import-statement node in `H` (the set `classify_import_stmt`
/// selects) — every value the six handlers below used to read directly off
/// the `tree_sitter::Node`, and nothing else. No corpus-wide table
/// (`symbol_table`/`entity_map`/`go_pkg_index`/`top_level_entities`) is
/// represented here — those stay pass-2 inputs, supplied to
/// [`dispatch_import_stmt`] alongside a descriptor, exactly as they used to
/// be supplied alongside a node.
///
/// The design doc glosses this as one flat `(kind, module path string,
/// [(original, local)] specifier pairs, alias)` tuple; recovering the actual
/// per-kind reads below shows that shape undersells two handlers —
/// `TsImport` interleaves up to three distinct clause items (default name,
/// namespace alias, named-specifier list) in document order, and `GoImport`
/// can carry more than one package path per node (`import ("fmt"; "os")`).
/// An enum keyed on [`ImportStmtKind`] is the minimal-sufficient shape: each
/// variant carries exactly what its handler reads, no more (`PyFromImport`,
/// `TsReExport`, `RustUse`, `GoImport`'s single-string case *do* reduce to
/// the doc's tuple/string shape, unchanged in spirit). `kind` itself needs no
/// separate field — the variant tag serializes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ImportStmtFacts {
    /// Python `from <module> import a, b as c`.
    PyFromImport {
        module: String,
        /// `(original, local)`, already filtered for `!original.is_empty()`.
        specifiers: Vec<(String, String)>,
    },
    /// Python `import a, b as c` — one `(module_name, alias)` pair per
    /// comma-separated module reference; `alias == module_name` when the
    /// statement had no `as` clause.
    PyModuleImport { modules: Vec<(String, String)> },
    /// TS/JS `import ... from '...'`. `items` preserves the exact document
    /// order `import_clause`'s named children were visited in — the same
    /// order the original handler's single pass produced its resolve calls.
    TsImport {
        source: String,
        items: Vec<TsClauseItem>,
    },
    /// TS/JS `export ... from '...'`. `original == "default"` is handled
    /// specially by the dispatcher, same as before this refactor.
    TsReExport {
        source: String,
        specifiers: Vec<(String, String)>,
    },
    /// Rust `use ...;` — the fully preprocessed statement text (trimmed;
    /// `use `/`pub use ` prefix, trailing `;`, and one leading
    /// `crate::`/`super::`/`self::` segment already stripped, exactly as
    /// the original handler prepared it before its own text-only parsing
    /// began). That parsing is pure string logic with no further tree or
    /// corpus reads, so it stays in the dispatcher, operating on this field.
    RustUse { text: String },
    /// Go `import (...)` — one *full* package import path per spec
    /// (stripped of quotes; Go-admission
    /// finding), in the same order the original handler discovered and
    /// registered them. Used to be reduced to the bare last `/` segment
    /// here — the same string `register_go_package_imports` still uses as
    /// its O(1) bucket key — but a bucket keyed only on that bare string
    /// can hold entries from more than one same-named package (kubernetes
    /// has dozens of directories literally named `v1`), so the full path
    /// now survives into `register_go_package_imports`, which needs it to
    /// pick the one declaring package a given import actually names.
    GoImport { packages: Vec<String> },
}

/// One item of a TS/JS `import_clause`, in the order
/// [`build_import_stmt_facts`] encountered it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum TsClauseItem {
    /// One entry of a `{ Foo, Bar as Baz }` named-imports list.
    Named { original: String, local: String },
    /// `import * as m from '...'`.
    Namespace { alias: String },
    /// `import Foo from '...'`.
    Default { name: String },
}

/// Build the [`ImportStmtFacts`] descriptor `node` (already classified as
/// `stmt`) carries. Pure in `(stmt, node, source, skip_js_ts_imports)` —
/// touches no corpus-wide table, so it is safe to call during pass 1's
/// precompute (before `symbol_table`/`entity_map`/`go_pkg_index` exist) as
/// well as during pass 2's tree-driven replay. This function, plus
/// [`dispatch_import_stmt`] below, is the exact factoring of what used to be
/// six standalone `extract_*` functions: read-from-tree here,
/// resolve-against-corpus there.
///
/// `skip_js_ts_imports` short-circuits `TsImport`/`TsReExport` to an empty
/// stub *before* walking the node: [`dispatch_import_stmt`] discards a
/// skipped TS descriptor unconditionally (same `if !skip_js_ts_imports`
/// gate the old direct-dispatch code had), so building the real one would
/// be pure wasted tree work on every JS/TS file that takes the chunked
/// session path (`skip_js_ts_imports` is unconditionally true there — see
/// `PrecomputedFileFacts`'s doc comment) — exactly the cost the old
/// `if !skip_js_ts_imports { extract_ts_import(...) }` gate avoided by not
/// calling the extraction function at all. Every other kind is unaffected:
/// no other grammar this crate resolves emits `import_statement`/
/// `export_statement` nodes classified as `TsImport`/`TsReExport`
/// (`classify_import_stmt`'s kind strings are JS/TS-specific), so this flag
/// is a no-op for Python/Rust/Go's own descriptors.
fn build_import_stmt_facts(
    stmt: ImportStmtKind,
    node: tree_sitter::Node,
    source: &[u8],
    skip_js_ts_imports: bool,
) -> ImportStmtFacts {
    match stmt {
        ImportStmtKind::TsImport if skip_js_ts_imports => {
            return ImportStmtFacts::TsImport {
                source: String::new(),
                items: Vec::new(),
            };
        }
        ImportStmtKind::TsReExport if skip_js_ts_imports => {
            return ImportStmtFacts::TsReExport {
                source: String::new(),
                specifiers: Vec::new(),
            };
        }
        _ => {}
    }
    match stmt {
        ImportStmtKind::PyFromImport => {
            let module = node
                .child_by_field_name("module_name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .to_string();
            let mut specifiers = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "dotted_name" || child.kind() == "aliased_import" {
                    let (original, local) = if child.kind() == "aliased_import" {
                        let orig = child
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let alias = child
                            .child_by_field_name("alias")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or(orig);
                        (orig.to_string(), alias.to_string())
                    } else {
                        let name = child.utf8_text(source).unwrap_or("");
                        (name.to_string(), name.to_string())
                    };
                    if !original.is_empty() {
                        specifiers.push((original, local));
                    }
                }
            }
            ImportStmtFacts::PyFromImport { module, specifiers }
        }
        ImportStmtKind::PyModuleImport => {
            let mut modules = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                let (module_name, alias) = match child.kind() {
                    "dotted_name" => {
                        let name = child.utf8_text(source).unwrap_or("");
                        (name.to_string(), name.to_string())
                    }
                    "aliased_import" => {
                        let orig = child
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let alias = child
                            .child_by_field_name("alias")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or(orig);
                        (orig.to_string(), alias.to_string())
                    }
                    _ => continue,
                };
                if !module_name.is_empty() {
                    modules.push((module_name, alias));
                }
            }
            ImportStmtFacts::PyModuleImport { modules }
        }
        ImportStmtKind::TsImport => {
            let source_path = node
                .child_by_field_name("source")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .trim_matches(|c: char| c == '\'' || c == '"')
                .to_string();
            let mut items = Vec::new();
            if !source_path.is_empty() {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() != "import_clause" {
                        continue;
                    }
                    let mut clause_cursor = child.walk();
                    for clause_child in child.named_children(&mut clause_cursor) {
                        if clause_child.kind() == "named_imports" {
                            let mut imports_cursor = clause_child.walk();
                            for spec in clause_child.named_children(&mut imports_cursor) {
                                if spec.kind() != "import_specifier" {
                                    continue;
                                }
                                let original = spec
                                    .child_by_field_name("name")
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or("");
                                let local = spec
                                    .child_by_field_name("alias")
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or(original);
                                if !original.is_empty() {
                                    items.push(TsClauseItem::Named {
                                        original: original.to_string(),
                                        local: local.to_string(),
                                    });
                                }
                            }
                        } else if clause_child.kind() == "namespace_import" {
                            let mut ns_cursor = clause_child.walk();
                            let alias = clause_child
                                .child_by_field_name("alias")
                                .or_else(|| {
                                    clause_child
                                        .named_children(&mut ns_cursor)
                                        .find(|c| c.kind() == "identifier")
                                })
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("");
                            if !alias.is_empty() {
                                items.push(TsClauseItem::Namespace {
                                    alias: alias.to_string(),
                                });
                            }
                        } else if clause_child.kind() == "identifier" {
                            let name = clause_child.utf8_text(source).unwrap_or("");
                            if !name.is_empty() {
                                items.push(TsClauseItem::Default {
                                    name: name.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            ImportStmtFacts::TsImport {
                source: source_path,
                items,
            }
        }
        ImportStmtKind::TsReExport => {
            let source_path = node
                .child_by_field_name("source")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .trim_matches(|c: char| c == '\'' || c == '"')
                .to_string();
            let mut specifiers = Vec::new();
            if !source_path.is_empty() {
                let mut worklist = vec![node];
                while let Some(current) = worklist.pop() {
                    let mut cursor = current.walk();
                    for child in current.named_children(&mut cursor) {
                        match child.kind() {
                            "export_specifier" => {
                                let original = child
                                    .child_by_field_name("name")
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or("");
                                let local = child
                                    .child_by_field_name("alias")
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or(original);
                                if original.is_empty() || local.is_empty() {
                                    continue;
                                }
                                specifiers.push((original.to_string(), local.to_string()));
                            }
                            "export_clause" | "namespace_export" => {
                                worklist.push(child);
                            }
                            _ => {}
                        }
                    }
                }
            }
            ImportStmtFacts::TsReExport {
                source: source_path,
                specifiers,
            }
        }
        ImportStmtKind::RustUse => {
            let text = node.utf8_text(source).unwrap_or("").trim().to_string();
            let text = text.strip_prefix("use ").unwrap_or(&text);
            let text = text.strip_prefix("pub use ").unwrap_or(text);
            let text = text.trim_end_matches(';').trim();
            let text = text
                .strip_prefix("crate::")
                .or_else(|| text.strip_prefix("super::"))
                .or_else(|| text.strip_prefix("self::"))
                .unwrap_or(text);
            ImportStmtFacts::RustUse {
                text: text.to_string(),
            }
        }
        ImportStmtKind::GoImport => {
            let mut packages = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "import_spec" || child.kind() == "import_spec_list" {
                    collect_go_import_pkg_names(child, source, &mut packages);
                } else if child.kind() == "interpreted_string_literal"
                    || child.kind() == "raw_string_literal"
                {
                    let path = child
                        .utf8_text(source)
                        .unwrap_or("")
                        .trim_matches('"')
                        .trim_matches('`');
                    packages.push(path.to_string());
                }
            }
            ImportStmtFacts::GoImport { packages }
        }
    }
}

/// Tree-only half of the old `extract_go_import_specs`: collects package
/// import paths (stripped of quotes; the *full* path, not reduced to a
/// bare last `/` segment — needs the whole string to disambiguate
/// same-named packages, see [`register_go_package_imports`]) from a Go
/// `import_spec`/`import_spec_list` subtree, appending them to `out` in the
/// same LIFO worklist order the original handler dispatched them in.
fn collect_go_import_pkg_names(root: tree_sitter::Node, source: &[u8], out: &mut Vec<String>) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "import_spec" {
                let path_node = child
                    .child_by_field_name("path")
                    .or_else(|| child.named_child(0));
                if let Some(pn) = path_node {
                    let path = pn
                        .utf8_text(source)
                        .unwrap_or("")
                        .trim_matches('"')
                        .trim_matches('`');
                    out.push(path.to_string());
                }
            } else {
                worklist.push(child);
            }
        }
    }
}

/// Run the one handler `descriptor`'s variant selects — tree-free: every
/// value it needs was already read out of the node by
/// [`build_import_stmt_facts`] (tree-driven callers) or arrived from a
/// [`PrecomputedFileFacts::import_stmts`] entry (a precompute producer) —
/// the two are the same type, so this function cannot observe which
/// produced the descriptor it was handed. The `skip_js_ts_imports` gate
/// lives here exactly as before the factoring: a skipped JS/TS import is
/// still *handled*, not descended into.
#[allow(clippy::too_many_arguments)]
fn dispatch_import_stmt<'a>(
    descriptor: &ImportStmtFacts,
    file_path: &str,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    go_pkg_index: &GoPkgIndex,
    ts_default_exports: &TsDefaultExportTable,
    top_level_entities: &OnceLock<TopLevelEntityIndex>,
    py_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    rust_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    parsed_files: &'a [(String, String, tree_sitter::Tree)],
    content_by_file: &OnceLock<HashMap<&'a str, &'a str>>,
    exported_names_by_file: &Mutex<HashMap<String, Arc<HashSet<String>>>>,
    skip_js_ts_imports: bool,
    rec: &mut Recorder,
) {
    match descriptor {
        ImportStmtFacts::PyFromImport { module, specifiers } => {
            for (original, local) in specifiers {
                resolve_import_name(
                    original,
                    local,
                    module,
                    file_path,
                    &[".py"],
                    symbol_table,
                    entity_map,
                    import_table,
                    scopes,
                    rec,
                );
            }
        }
        ImportStmtFacts::PyModuleImport { modules } => {
            for (module_name, alias) in modules {
                register_namespace_import(
                    alias,
                    module_name,
                    file_path,
                    &[".py"],
                    py_top_level_entities,
                    symbol_table,
                    entity_map,
                    import_table,
                    rec,
                );
            }
        }
        ImportStmtFacts::TsImport { source, items } => {
            if !skip_js_ts_imports {
                for item in items {
                    match item {
                        TsClauseItem::Named { original, local } => {
                            resolve_import_name(
                                original,
                                local,
                                source,
                                file_path,
                                JS_TS_EXTENSIONS,
                                symbol_table,
                                entity_map,
                                import_table,
                                scopes,
                                rec,
                            );
                        }
                        TsClauseItem::Namespace { alias } => {
                            register_ts_namespace_import(
                                alias,
                                source,
                                file_path,
                                JS_TS_EXTENSIONS,
                                top_level_entities,
                                symbol_table,
                                entity_map,
                                parsed_files,
                                content_by_file,
                                exported_names_by_file,
                                import_table,
                            );
                        }
                        TsClauseItem::Default { name } => {
                            resolve_default_import(
                                name,
                                source,
                                file_path,
                                JS_TS_EXTENSIONS,
                                ts_default_exports,
                                import_table,
                                scopes,
                            );
                        }
                    }
                }
            }
        }
        ImportStmtFacts::TsReExport { source, specifiers } => {
            if !skip_js_ts_imports {
                for (original, local) in specifiers {
                    if original == "default" {
                        resolve_default_import(
                            local,
                            source,
                            file_path,
                            JS_TS_EXTENSIONS,
                            ts_default_exports,
                            import_table,
                            scopes,
                        );
                    } else {
                        resolve_import_name(
                            original,
                            local,
                            source,
                            file_path,
                            JS_TS_EXTENSIONS,
                            symbol_table,
                            entity_map,
                            import_table,
                            scopes,
                            rec,
                        );
                    }
                }
            }
        }
        // Rust: `use crate::module::Name;` or `use crate::module::{A, B};`.
        //
        // every segment this parses is tried two ways, not one —
        // as an imported *item* (`resolve_import_name`: `Name` is a
        // function/struct/etc. defined in `module`) and, unless it is
        // obviously a type, also as an imported *module alias*
        // (`register_rust_module_import`: `Name` is itself a module, so
        // `Name::some_fn()` elsewhere in this file must resolve through
        // it). Rust's grammar cannot tell these apart from the `use` text
        // alone — `use a::b;` is syntactically identical whether `b` is a
        // function or a module — so both are attempted and each writes
        // disjoint import-table keys (`local_name` for the item guess,
        // `local_name::item` for the module guess), making the
        // double-attempt safe: whichever guess is wrong simply misses on
        // lookup, and the "obviously a type" skip (an uppercase first
        // letter, Rust's own type-naming convention — the same signal
        // `receiver_is_type` in `extract_call_ref` already relies on for
        // this exact type/module distinction) avoids the wasted
        // module-alias lookup on the common case, `use path::SomeType;`.
        // A third skip, `rust_use_path_is_external_std`,
        // additionally rules out the module-alias attempt whenever the
        // `use` path is rooted at `std`/`core`/`alloc` — those can never
        // legitimately name a corpus file, so even trying invites a same-stem
        // collision against an unrelated local file (see that function's
        // doc). `register_rust_module_import` itself does item-granularity
        // disambiguation ([`select_rust_module_item_winner`]) for genuine
        // workspace-local same-stem collisions that survive this skip.
        ImportStmtFacts::RustUse { text } => {
            if let Some(brace_pos) = text.find("::{") {
                let module_path = &text[..brace_pos];
                let source_module = module_path.rsplit("::").next().unwrap_or(module_path);
                let qualifying_path = rust_qualifying_path(module_path);
                // `std`/`core`/`alloc` name no corpus file
                // outside the rust-lang/rust tree itself -- see
                // `rust_use_path_is_external_std`'s doc.
                let is_external_std_prefix = rust_use_path_is_external_std(module_path);
                let names_part = &text[brace_pos + 3..];
                let names_part = names_part.trim_end_matches('}');
                for name_part in names_part.split(',') {
                    let name_part = name_part.trim();
                    if name_part.is_empty() {
                        continue;
                    }
                    let (original, local) = if let Some(pos) = name_part.find(" as ") {
                        (name_part[..pos].trim(), name_part[pos + 4..].trim())
                    } else {
                        (name_part, name_part)
                    };
                    if !original.is_empty() {
                        resolve_import_name(
                            original,
                            local,
                            source_module,
                            file_path,
                            &[".rs"],
                            symbol_table,
                            entity_map,
                            import_table,
                            scopes,
                            rec,
                        );
                        if !is_rust_type_name(original) && !is_external_std_prefix {
                            register_rust_module_import(
                                local,
                                original,
                                &qualifying_path,
                                file_path,
                                rust_top_level_entities,
                                symbol_table,
                                entity_map,
                                import_table,
                                rec,
                            );
                        }
                    }
                }
            } else {
                let parts: Vec<&str> = text.split("::").collect();
                if parts.is_empty() {
                    return;
                }
                let imported_name = parts.last().unwrap().trim();
                let (original, local) = if let Some(pos) = imported_name.find(" as ") {
                    (&imported_name[..pos], imported_name[pos + 4..].trim())
                } else {
                    (imported_name, imported_name)
                };
                let source_module = if parts.len() >= 2 {
                    parts[parts.len() - 2]
                } else {
                    parts[0]
                };
                let qualifying_path =
                    rust_qualifying_path(&parts[..parts.len().saturating_sub(1)].join("::"));
                // Judged off the *whole* `use` path's own root
                // segment (`text`, not the already-crate/self/super-stripped
                // `qualifying_path`) so a bare `use std;` (no `::` at all,
                // `qualifying_path == ""`) is still caught.
                let is_external_std_prefix = rust_use_path_is_external_std(text);
                if !original.is_empty() && !source_module.is_empty() {
                    resolve_import_name(
                        original,
                        local,
                        source_module,
                        file_path,
                        &[".rs"],
                        symbol_table,
                        entity_map,
                        import_table,
                        scopes,
                        rec,
                    );
                }
                if !original.is_empty() && !is_rust_type_name(original) && !is_external_std_prefix {
                    register_rust_module_import(
                        local,
                        original,
                        &qualifying_path,
                        file_path,
                        rust_top_level_entities,
                        symbol_table,
                        entity_map,
                        import_table,
                        rec,
                    );
                }
            }
        }
        ImportStmtFacts::GoImport { packages } => {
            for import_path in packages {
                register_go_package_imports(
                    import_path,
                    file_path,
                    import_table,
                    scopes,
                    go_pkg_index,
                    rec,
                );
            }
        }
    }
}

/// Field 10's *recording* half: same traversal, same
/// pruning, same order as the dispatch-direct pruned replay this used to be
/// — but instead of resolving each import statement against corpus-wide
/// tables, it reads the node into an [`ImportStmtFacts`] descriptor and
/// keeps going. No `symbol_table`/`entity_map`/any corpus-wide table is
/// touched, which is what lets pass 1 call this before those tables exist
/// (they are pass-1 assembly's own output — see
/// `precompute_scope_resolvable_file_facts`). `import_starts` is the sorted
/// list of `start_byte`s of every node in `H`, recorded by
/// `fused_scope_refs_import_walk` during the one document-order walk; this
/// re-runs `extract_imports_from_ast`'s own worklist algorithm — forward
/// child push onto a LIFO worklist, descriptors built at the parent's visit
/// — but descends only into children whose byte range contains a recorded
/// start. `skip_js_ts_imports` is forwarded to
/// [`build_import_stmt_facts`] (see that function's doc comment for why: a
/// skipped TS descriptor is a cheap empty stub, not a fully-walked one).
///
/// Order argument: the LIFO relative order of the kept nodes is determined
/// by their tree positions alone, so `out`'s order — and therefore, once
/// [`dispatch_import_stmts_from_facts`] runs it, every last-write-wins
/// `import_table`/`scopes[0]` outcome — is exactly the unfused walk's
/// (`extract_imports_from_ast`, kept alive as that independent spec). The
/// one production caller is `resolve_with_scopes_full_inner`, which skips
/// this call entirely when `import_starts` is empty — on a C# corpus that
/// is every file, which is where `extract_imports_from_ast`'s
/// pure-traversal cost goes. One path: this records descriptors,
/// [`dispatch_import_stmts_from_facts`] consumes them — no dispatch-direct
/// pruned-replay variant kept alongside it.
fn record_import_stmts_pruned(
    root: tree_sitter::Node,
    import_starts: &[usize],
    source: &[u8],
    config: &ScopeResolveConfig,
    skip_js_ts_imports: bool,
) -> Vec<ImportStmtFacts> {
    let mut out = Vec::new();
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match classify_import_stmt(child.kind(), config) {
                Some(stmt) => out.push(build_import_stmt_facts(
                    stmt,
                    child,
                    source,
                    skip_js_ts_imports,
                )),
                None => {
                    if subtree_contains_import_start(child, import_starts) {
                        worklist.push(child);
                    }
                }
            }
        }
    }
    out
}

/// Dispatch every descriptor [`record_import_stmts_pruned`] (or a precompute
/// producer's `PrecomputedFileFacts::import_stmts`) collected, in order —
/// the corpus-side half of what the pruned replay used to do in one
/// dispatch-direct pass.
#[allow(clippy::too_many_arguments)]
fn dispatch_import_stmts_from_facts<'a>(
    descriptors: &[ImportStmtFacts],
    file_path: &str,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    go_pkg_index: &GoPkgIndex,
    ts_default_exports: &TsDefaultExportTable,
    top_level_entities: &OnceLock<TopLevelEntityIndex>,
    py_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    rust_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    parsed_files: &'a [(String, String, tree_sitter::Tree)],
    content_by_file: &OnceLock<HashMap<&'a str, &'a str>>,
    exported_names_by_file: &Mutex<HashMap<String, Arc<HashSet<String>>>>,
    skip_js_ts_imports: bool,
    rec: &mut Recorder,
) {
    for descriptor in descriptors {
        dispatch_import_stmt(
            descriptor,
            file_path,
            symbol_table,
            entity_map,
            import_table,
            scopes,
            go_pkg_index,
            ts_default_exports,
            top_level_entities,
            py_top_level_entities,
            rust_top_level_entities,
            parsed_files,
            content_by_file,
            exported_names_by_file,
            skip_js_ts_imports,
            rec,
        );
    }
}

/// Does `node`'s byte range contain any recorded import start? `starts` is
/// sorted ascending (pre-order recording), so this is a binary search.
fn subtree_contains_import_start(node: tree_sitter::Node, starts: &[usize]) -> bool {
    let s = node.start_byte();
    match starts.binary_search(&s) {
        Ok(_) => true,
        Err(i) => i < starts.len() && starts[i] < node.end_byte(),
    }
}

/// Rust's own naming convention (types/traits are `UpperCamelCase`, modules
/// and functions are `snake_case`) — the same signal `extract_call_ref`'s
/// `receiver_is_type` already uses to route a two-segment scoped call to
/// `MethodCall` (a type's associated item) instead of `ScopedCall` (a
/// module-qualified free item). Used here to skip the module-alias
/// registration attempt for names that are obviously not modules.
fn is_rust_type_name(name: &str) -> bool {
    name.chars().next().map_or(false, |c| c.is_uppercase())
}

/// `import_path` is the full, unreduced string a Go `import_spec` names
/// (e.g. `k8s.io/kubernetes/cmd/kubeadm/app/apis/kubeadm/v1`) — see
/// [`ImportStmtFacts::GoImport`]'s doc comment for why this stopped being
/// reduced to its last `/` segment before reaching this function.
#[allow(clippy::too_many_arguments)]
fn register_go_package_imports(
    import_path: &str,
    file_path: &str,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    go_pkg_index: &GoPkgIndex,
    rec: &mut Recorder,
) {
    // The bare identifier a call site actually spells (`v1` in `v1.Pod{}`)
    // is still the O(1) bucket key — only the disambiguation inside the
    // bucket is new.
    let pkg_name = import_path.rsplit('/').next().unwrap_or(import_path);
    // Use pre-built package index for O(1) lookup instead of O(symbol_table) scan.
    // Recorded unconditionally: a miss is a dependency too — a
    // package that resolves to nothing today may gain entries when a new
    // file declares that package, which must invalidate this import. Keyed
    // on the bucket (`pkg_name`), not the winning candidate: any change to
    // *any* same-named package must invalidate this file's resolution,
    // since it was consulted to pick the winner.
    rec.one(Table::GoPkgIndex, pkg_name);
    let Some(entries) = go_pkg_index.get(pkg_name) else {
        return;
    };
    // a bucket keyed by bare last segment can hold entries from
    // more than one declaring package (kubernetes has dozens of directories
    // literally named `v1`, one per API group) — inserting the whole bucket
    // used to merge every one of their exported names into this file's
    // import_table, last-write-wins, with no relationship to which package
    // this file actually imports. `select_go_pkg_candidate` picks the one
    // declaring directory `import_path` actually names; a bucket with only
    // one distinct directory (the overwhelming common case) short-circuits
    // without comparing anything, so this costs nothing extra when there is
    // nothing to disambiguate.
    let winner = select_go_pkg_candidate(entries, import_path);
    for (name, target_id, decl_dir) in entries {
        if decl_dir != winner {
            continue;
        }
        import_table.insert((file_path.to_string(), name.clone()), target_id.clone());
        if !scopes.is_empty() {
            scopes[0].defs.insert(name.clone(), target_id.clone());
        }
    }
}

/// Which declaring directory (of possibly several sharing `entries`' bare
/// bucket key) `import_path` actually means — the fix for 's
/// kubernetes collision. Picks the directory whose own trailing path
/// segments overlap `import_path`'s trailing segments the longest: a Go
/// import path always ends with its package's declaring directory's own
/// segments (a module-path prefix aside), the same signal
/// `registry::resolve_go_method_parent_ids` already trusts to key
/// same-package method/type pairs. Ties (equal overlap) resolve to the
/// lexicographically smaller directory — deterministic, matching this
/// module's other tie-breaks (`find_import_file`'s `min_by`).
///
/// A bucket with exactly one distinct declaring directory (the common,
/// unambiguous case — most bare names are not repo-wide collisions) never
/// calls [`trailing_path_overlap`] at all: `Iterator::max_by` does not
/// invoke its comparator on a single-element iterator, so this is exactly
/// as cheap as returning the bucket's one directory directly.
fn select_go_pkg_candidate<'a>(
    entries: &'a [(String, String, String)],
    import_path: &str,
) -> &'a str {
    let mut dirs: Vec<&str> = entries.iter().map(|(_, _, dir)| dir.as_str()).collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.into_iter()
        .max_by(|a, b| {
            trailing_path_overlap(import_path, a)
                .cmp(&trailing_path_overlap(import_path, b))
                .then_with(|| (*b).cmp(*a))
        })
        .unwrap_or("")
}

/// How many trailing `/`-separated segments two paths share, scanning from
/// the end. `go_pkg_index`'s bucket key already guarantees at least one
/// segment matches for every candidate this is called with (that shared
/// last segment is what put them in the same bucket), so this always
/// returns >= 1 for a real candidate.
fn trailing_path_overlap(a: &str, b: &str) -> usize {
    a.rsplit('/')
        .zip(b.rsplit('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// Shared helper: resolve an imported name against the symbol table.
///
/// Every candidate this considers is recorded: the name lookup
/// itself (a miss is a dependency — a later-added symbol of this name must
/// invalidate this file), and every target id `find_import_target` inspects
/// while picking the best file match (a change to any candidate's file path
/// could change which one wins, even the ones that lose).
#[allow(clippy::too_many_arguments)]
fn resolve_import_name(
    original_name: &str,
    local_name: &str,
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    rec: &mut Recorder,
) {
    rec.one(Table::SymbolTable, original_name);
    if let Some(target_ids) = symbol_table.get(original_name) {
        for id in target_ids {
            rec.one(Table::EntityMap, id);
        }
        let target = find_import_target(target_ids, source_path, file_path, extensions, entity_map);

        if let Some(target_id) = target {
            import_table.insert(
                (file_path.to_string(), local_name.to_string()),
                target_id.clone(),
            );
            if !scopes.is_empty() {
                scopes[0]
                    .defs
                    .insert(local_name.to_string(), target_id.clone());
            }
        }
    }
}

fn resolve_default_import(
    local_name: &str,
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    default_exports: &TsDefaultExportTable,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    let target = find_import_file(
        &default_exports.sorted_files,
        source_path,
        file_path,
        extensions,
    )
    .and_then(|target_file| default_exports.exports_by_file.get(target_file))
    .cloned();

    if let Some(target_id) = target {
        import_table.insert(
            (file_path.to_string(), local_name.to_string()),
            target_id.clone(),
        );
        if !scopes.is_empty() {
            scopes[0].defs.insert(local_name.to_string(), target_id);
        }
    }
}

/// Register exported source module entities under a namespace alias.
/// For `import * as m from './module'`, exported entities from the module
/// are registered so that `m.foo()` resolves via the method call path.
fn register_ts_namespace_import<'a>(
    alias: &str,
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    top_level_entities: &OnceLock<TopLevelEntityIndex>,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    parsed_files: &'a [(String, String, tree_sitter::Tree)],
    content_by_file: &OnceLock<HashMap<&'a str, &'a str>>,
    exported_names_by_file: &Mutex<HashMap<String, Arc<HashSet<String>>>>,
    import_table: &mut HashMap<(String, String), String>,
) {
    let top_level_entities = top_level_entities
        .get_or_init(|| build_top_level_entity_index(symbol_table, entity_map, extensions));
    // Resolve the module path against every parsed JS/TS file, not just files
    // that declare top-level entities: a pure star barrel (`export * from`)
    // declares nothing itself, so it is absent from `sorted_files`, yet it is
    // a perfectly valid namespace-import target whose surface is the expanded
    // star re-exports (issue #478).
    let all_candidate_files: Vec<String> = {
        let mut files: Vec<String> = parsed_files
            .iter()
            .map(|(path, _, _)| path.clone())
            .filter(|path| extensions.iter().any(|ext| path.ends_with(ext)))
            .collect();
        sort_import_candidate_files(&mut files, extensions);
        files
    };
    let Some(candidate_file) =
        find_import_file(&all_candidate_files, source_path, file_path, extensions)
    else {
        return;
    };
    let empty_entries: Vec<(String, String)> = Vec::new();
    let entries = top_level_entities
        .entities_by_file
        .get(candidate_file)
        .unwrap_or(&empty_entries);
    let exported_names = {
        let mut cache = exported_names_by_file.lock().unwrap();
        cache
            .entry(candidate_file.to_string())
            .or_insert_with(|| {
                let content_by_file = content_by_file.get_or_init(|| {
                    parsed_files
                        .iter()
                        .map(|(file_path, content, _)| (file_path.as_str(), content.as_str()))
                        .collect()
                });
                Arc::new(
                    content_by_file
                        .get(candidate_file)
                        .map(|content| js_ts_named_exports_from_content(content))
                        .map(|names| names.into_iter().collect())
                        .unwrap_or_default(),
                )
            })
            .clone()
    };
    for (name, target_id) in entries {
        if !exported_names.contains(name) {
            continue;
        }
        let qualified_name = format!("{alias}.{name}");
        import_table
            .entry((file_path.to_string(), qualified_name))
            .or_insert_with(|| target_id.clone());
    }

    // Star barrels: `entries` above are the candidate file's OWN top-level
    // entities, so a file that is only `export * from './x'` contributes
    // nothing there. Expand its star re-exports (issue #478) and register
    // each expanded name against the entity in the file that actually
    // declares it. `.entry().or_insert_with()` keeps the barrel's own
    // declarations winning over star-provided names.
    let content_by_file = content_by_file.get_or_init(|| {
        parsed_files
            .iter()
            .map(|(file_path, content, _)| (file_path.as_str(), content.as_str()))
            .collect()
    });
    let expanded = {
        let content_by_path: rustc_hash::FxHashMap<String, &str> = content_by_file
            .iter()
            .map(|(path, content)| ((*path).to_string(), *content))
            .collect();
        let mut memo = rustc_hash::FxHashMap::default();
        let mut visiting = rustc_hash::FxHashSet::default();
        crate::parser::import_resolution::expand_js_ts_named_export_sources_for_file(
            candidate_file,
            &content_by_path,
            &all_candidate_files,
            &mut memo,
            &mut visiting,
            0,
        )
    };
    for (name, source_file) in expanded {
        if source_file == candidate_file {
            continue; // own declarations already handled above
        }
        let Some(source_entries) = top_level_entities.entities_by_file.get(&source_file) else {
            continue;
        };
        let Some((_, target_id)) = source_entries.iter().find(|(n, _)| *n == name) else {
            continue;
        };
        let qualified_name = format!("{alias}.{name}");
        import_table
            .entry((file_path.to_string(), qualified_name))
            .or_insert_with(|| target_id.clone());
    }
}

/// Python's bare `import module` form. The read this needs — "any top-level
/// entity anywhere in the corpus whose file could match `source_path`" — is
/// still too diffuse to name with individual `(table, key)` pairs, so the
/// *incremental-invalidation* guard stays whole (see
/// [`Table::GuardPyWildcardImport`]: any future change to any
/// `(name, target file)` pair in the corpus invalidates this file, which is
/// conservative but never wrong) — that guard is unchanged by this comment's
/// history.
///
/// what *did* change is how the match itself is computed. This
/// used to scan every entry of `symbol_table` — every distinct name across
/// the whole corpus, and every entity behind each name — checking each
/// against `source_path`, once per bare `import module` statement in the
/// build. On home-assistant-core (18k Python files, ~258k entities, bare
/// module imports like `import logging`/`import os` in nearly every file)
/// that scan dominated the entire resolve phase: ~1,563s of summed
/// per-thread CPU time out of a ~91s wall build (attributed via
/// `SEM_PROFILE_RESOLVE=1`'s `scope_build_ms` bucket — see
/// "Python pathology" section). `py_top_level_
/// entities` is the fix: the exact same `(file -> top-level entities)`
/// grouping [`build_top_level_entity_index`] already builds for JS/TS
/// namespace imports, generalized to any extension set and built **once**
/// (lazily, via `OnceLock`) instead of re-scanned per import statement. The
/// match below reproduces [`import_source_matches_file`]'s semantics exactly
/// — every corpus file that could plausibly be the target, not just the one
/// `find_import_file` would pick — just evaluated against the small
/// candidate/stem set for *this* specifier instead of every entity in the
/// corpus.
#[allow(clippy::too_many_arguments)]
fn register_namespace_import(
    alias: &str,
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    py_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    rec: &mut Recorder,
) {
    rec.whole(Table::GuardPyWildcardImport);
    let index = py_top_level_entities
        .get_or_init(|| build_top_level_entity_index(symbol_table, entity_map, extensions));

    // Every corpus file that could match this specifier: the bounded
    // relative/dotted candidate list when there is one, or every file
    // sharing the bare specifier's stem otherwise — the same two cases
    // `import_source_matches_file` distinguishes, now evaluated as index
    // lookups instead of a linear scan.
    let matched_files: Vec<String> =
        match import_file_candidates(file_path, source_path, extensions) {
            Some(candidates) => candidates
                .into_iter()
                .filter(|candidate| index.entities_by_file.contains_key(candidate.as_str()))
                .collect(),
            None => match_bare_import_stem(&index.stem_index, source_path)
                .cloned()
                .unwrap_or_default(),
        };

    for candidate_file in &matched_files {
        let Some(entries) = index.entities_by_file.get(candidate_file.as_str()) else {
            continue;
        };
        for (name, target_id) in entries {
            let qualified_name = format!("{alias}.{name}");
            import_table.insert((file_path.to_string(), qualified_name), target_id.clone());
        }
    }
}

/// Rust: `use crate::a::module_name;` / `use super::module_name;` /
/// `use self::module_name;` where `module_name` is itself a module (not an
/// item) — registers every top-level item of the aliased module, keyed
/// `alias::item`, so a later `module_name::some_fn()` call resolves through
/// `resolve_ref`'s `ScopedCall` arm.
///
/// A structural copy of [`register_namespace_import`] immediately above
/// (same unbounded-read shape, same stem-match resolution, `::` in the
/// qualified key instead of `.`) — kept as a separate function rather than
/// parameterizing the separator because the two also diverge on which
/// `Table` guard they record (`GuardRustModuleAlias`, not
/// `GuardPyWildcardImport` — see that variant's doc for why a shared tag
/// would be misleading despite the identical fold value) and because
/// `extract_rust_use` calls this once per `use` *segment* whether or not
/// the segment turns out to name a real module (see this function's own
/// "no name-only guessing" note below), unlike Python's `import module`
/// form where the caller already knows it is registering a module.
///
/// No name-only global guessing: a call only resolves through the entries
/// this writes if `source_path` (the segment as written in a real `use`
/// statement in this file) actually matches a real corpus file's stem —
/// exactly [`register_namespace_import`]'s own guarantee, not a new,
/// looser one. A module name that doesn't exist anywhere in the corpus
/// matches zero files and writes zero entries. The caller (`dispatch_
/// import_stmt`'s `RustUse` arm) additionally never calls this at all when
/// the `use` path is rooted at `std`/`core`/`alloc` — see `rust_use_path_
/// is_external_std` — since an external stdlib import can never legitimately
/// name a corpus file outside the rust-lang/rust tree itself.
///
/// `source_path` is only ever the bare last
/// `::`-segment of the `use` path (`extract_rust_use`'s two call sites both
/// reduce to it before calling this — see `dispatch_import_stmt`'s `RustUse`
/// arm). When that bare segment collides — two-plus files share a stem, e.g.
/// `a/util.rs` and `b/util.rs` — the bucket is a REAL collision only for the
/// specific item names two-plus of its files actually define; most item
/// names in a same-stem bucket are defined by exactly one file. An earlier
/// attempt at this fix pre-filtered the whole bucket down to one file
/// *before* looking at item names at all (by directory/`qualifying_path`
/// trailing-overlap, else lexicographically-smallest) — the wrong
/// granularity: it silently dropped every item the chosen file didn't
/// happen to define, even when a losing candidate did (verified concretely
/// on rust-lang/rust: `std::cmp::max` resolved to nothing because the
/// tie-break's winner among 7 same-stem `cmp.rs` files wasn't `library/
/// core/src/cmp.rs`, the one that actually defines `max`). This version
/// instead folds every matched file's entries by item *name* first (`per_
/// item` below), then asks, per name: how many of the matched files define
/// it? Exactly one -> that one wins, unconditionally (the original
/// unfiltered code's accidental correctness on the common case, now
/// deliberate). Two or more -> [`select_rust_module_item_winner`] uses
/// `qualifying_path` (the `use` path's segments before the bare name,
/// `crate`/`self`/`super` stripped since they name no real directory) to
/// discriminate by directory trailing-overlap, mirroring [`select_go_pkg_
/// candidate`]/[`trailing_path_overlap`] but scoped to just the item's own
/// definers, not the whole bucket; a tie (including an all-zero tie, i.e.
/// no `qualifying_path` information to discriminate with) writes nothing
/// for that item — an honest miss, never a last-write-wins blend and never
/// a lexicographically-smallest guess.
#[allow(clippy::too_many_arguments)]
fn register_rust_module_import(
    alias: &str,
    source_path: &str,
    qualifying_path: &str,
    file_path: &str,
    rust_top_level_entities: &OnceLock<TopLevelEntityIndex>,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    rec: &mut Recorder,
) {
    rec.whole(Table::GuardRustModuleAlias);
    let index = rust_top_level_entities
        .get_or_init(|| build_top_level_entity_index(symbol_table, entity_map, &[".rs"]));

    let matched_files: Vec<String> = match import_file_candidates(file_path, source_path, &[".rs"])
    {
        Some(candidates) => candidates
            .into_iter()
            .filter(|candidate| index.entities_by_file.contains_key(candidate.as_str()))
            .collect(),
        None => match_bare_import_stem(&index.stem_index, source_path)
            .cloned()
            .unwrap_or_default(),
    };

    // Fold by item name first so the per-name decision below only ever
    // scans the (small) set of files that actually define that name, not
    // the whole bucket.
    let mut per_item: HashMap<&str, Vec<(&str, &str)>> = HashMap::default();
    for candidate_file in &matched_files {
        let Some(entries) = index.entities_by_file.get(candidate_file.as_str()) else {
            continue;
        };
        for (name, target_id) in entries {
            per_item
                .entry(name.as_str())
                .or_default()
                .push((candidate_file.as_str(), target_id.as_str()));
        }
    }

    for (name, defs) in &per_item {
        let Some(target_id) = select_rust_module_item_winner(defs, qualifying_path) else {
            continue;
        };
        let qualified_name = format!("{alias}::{name}");
        import_table.insert(
            (file_path.to_string(), qualified_name),
            target_id.to_string(),
        );
    }
}

/// Pick the single file that legitimately defines an item among `defs` — every
/// `(candidate_file, target_id)` pair a same-stem bucket produced for one
/// item name — or `None` for an honest miss. Exactly one definer always
/// wins outright, no scoring needed. Two-plus definers need `qualifying_
/// path` (the `use` path's segments before the bare alias, [`rust_
/// qualifying_path`]) to discriminate by directory trailing-overlap
/// ([`rust_module_path_overlap`], mirroring [`select_go_pkg_candidate`]): a
/// unique nonzero-overlap winner wins; a tie — including an all-zero tie,
/// i.e. no information to discriminate with — is an honest miss, never
/// lexicographically-smallest, never last-write-wins.
fn select_rust_module_item_winner<'a>(
    defs: &[(&'a str, &'a str)],
    qualifying_path: &str,
) -> Option<&'a str> {
    if let [(_, only)] = defs {
        return Some(only);
    }

    let mut best_score = 0usize;
    let mut best_target: Option<&'a str> = None;
    let mut tied = false;
    for (file, target_id) in defs {
        let score = rust_module_path_overlap(qualifying_path, rust_file_dir(file));
        match score.cmp(&best_score) {
            std::cmp::Ordering::Greater => {
                best_score = score;
                best_target = Some(target_id);
                tied = false;
            }
            std::cmp::Ordering::Equal if best_target.is_some() => {
                tied = true;
            }
            _ => {}
        }
    }

    if best_score > 0 && !tied {
        best_target
    } else {
        None
    }
}

/// Whether a Rust `use` path's own root segment names the standard library
/// (`std`), `core`, or `alloc` — an import rooted at one of
/// these three can never legitimately resolve to a corpus file outside the
/// rust-lang/rust source tree itself (its symbols live in a prebuilt
/// sysroot, never in workspace source under analysis), so treating a
/// same-stem match against one as a real edge is always a lie: an
/// external-crate `use` colliding with an unrelated local file by bare
/// name (verified concretely on rust-lang/rust: `use std::{cmp, iter};
/// cmp::max(...)` bare-stem-matched 7 different `cmp.rs` files, none of
/// them reachable from `std`). Honest miss beats a wrong edge here, per
/// the codebase's stated policy.
///
/// Judged off `path`'s first `::`-segment (trimmed), taken *before*
/// `crate`/`self`/`super` stripping — those two forms are workspace-local
/// by construction and can never spell `std`/`core`/`alloc` as their root,
/// so checking the raw path is equivalent to checking `rust_qualifying_
/// path`'s output except in the single-segment corner case (`use std;`,
/// no `::` at all) where stripping would otherwise erase the very segment
/// this check needs to see.
///
/// Not a general "is this crate workspace-local" decision — that would
/// need a crate registry this pass doesn't have, so a genuine external
/// crate that merely isn't named `std`/`core`/`alloc` (`serde`, `tokio`,
/// ...) is not caught here and can still bare-stem-collide with an
/// unrelated local file. Documented residual, not silently assumed away.
fn rust_use_path_is_external_std(path: &str) -> bool {
    matches!(
        path.split("::").next().map(str::trim),
        Some("std" | "core" | "alloc")
    )
}

/// The `::`-joined prefix of a `use` path with Rust's path-root keywords
/// stripped out (`crate`, `self`, `super` name no real directory, so they
/// would only pollute the trailing-overlap score in
/// [`rust_module_path_overlap`]). `"crate::a"` -> `"a"`; `"a::b"` -> `"a::b"`;
/// `"crate"` alone (a `use crate::Name;` with no intermediate module) ->
/// `""`, an empty qualifying path that never overlaps anything, correctly
/// leaving [`select_rust_module_item_winner`] at an honest miss (rather
/// than a guess) for such an unqualified, genuinely-colliding case.
fn rust_qualifying_path(module_path: &str) -> String {
    module_path
        .split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty() && !matches!(*segment, "crate" | "self" | "super"))
        .collect::<Vec<_>>()
        .join("::")
}

fn rust_file_dir(file_path: &str) -> &str {
    match file_path.rfind('/') {
        Some(idx) => &file_path[..idx],
        None => "",
    }
}

/// How many trailing segments `qualifying_path` (`::`-separated, e.g. "a" or
/// "crate::a" already stripped down to "a") and `candidate_dir`
/// (`/`-separated, a real repo path) share, scanning from the end. Mirrors
/// [`trailing_path_overlap`]'s shape for Go, adapted to Rust's `::`
/// separator on one side and a real filesystem `/` separator on the other.
fn rust_module_path_overlap(qualifying_path: &str, candidate_dir: &str) -> usize {
    qualifying_path
        .rsplit("::")
        .zip(candidate_dir.rsplit('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

fn build_swift_call_signatures(
    parsed_files: &[(String, String, tree_sitter::Tree)],
    all_entities: &[SemanticEntity],
    entity_ranges: &HashMap<String, Vec<(usize, usize, String)>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> HashMap<String, SwiftCallSignature> {
    let mut signatures = HashMap::default();

    for (file_path, content, tree) in parsed_files {
        if !file_path.ends_with(".swift") {
            continue;
        }

        let Some(ranges) = entity_ranges.get(file_path.as_str()) else {
            continue;
        };

        let source = content.as_bytes();
        let mut worklist = vec![tree.root_node()];
        while let Some(node) = worklist.pop() {
            if matches!(node.kind(), "function_declaration" | "init_declaration") {
                if let Some(entity_id) =
                    find_entity_id_for_swift_declaration(node, ranges, entity_map)
                {
                    let argument_labels = extract_swift_declaration_argument_labels(node, source);
                    signatures.insert(entity_id, SwiftCallSignature { argument_labels });
                }
            }

            push_named_children_rev(&mut worklist, node);
        }
    }

    let mut content_parser: Option<tree_sitter::Parser> = None;
    for entity in all_entities {
        if signatures.contains_key(&entity.id) || !is_swift_callable_entity_info(entity) {
            continue;
        }

        if content_parser.is_none() {
            content_parser = swift_signature_parser();
        }
        let Some(parser) = content_parser.as_mut() else {
            break;
        };

        if let Some(argument_labels) = extract_swift_signature_from_entity_content(entity, parser) {
            signatures.insert(entity.id.clone(), SwiftCallSignature { argument_labels });
        }
    }

    signatures
}

fn find_entity_id_for_swift_declaration(
    node: tree_sitter::Node,
    ranges: &[(usize, usize, String)],
    entity_map: &HashMap<String, EntityInfo>,
) -> Option<String> {
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;

    ranges
        .iter()
        .filter(|(start, end, id)| {
            *start <= start_line
                && *end >= end_line
                && entity_map.get(id).map_or(false, is_swift_callable_entity)
        })
        .min_by_key(|(start, end, _)| end.saturating_sub(*start))
        .map(|(_, _, id)| id.clone())
}

fn is_swift_callable_entity(info: &EntityInfo) -> bool {
    info.file_path.ends_with(".swift")
        && matches!(
            info.entity_type.as_str(),
            "function" | "method" | "init" | "init_declaration"
        )
}

fn is_swift_callable_entity_info(entity: &SemanticEntity) -> bool {
    entity.file_path.ends_with(".swift")
        && matches!(
            entity.entity_type.as_str(),
            "function" | "method" | "init" | "init_declaration"
        )
}

fn swift_signature_parser() -> Option<tree_sitter::Parser> {
    let language = get_language_config(".swift").and_then(|config| (config.get_language)())?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn extract_swift_signature_from_entity_content(
    entity: &SemanticEntity,
    parser: &mut tree_sitter::Parser,
) -> Option<Vec<Option<String>>> {
    if let Some(argument_labels) = parse_swift_signature_source(parser, &entity.content) {
        return Some(argument_labels);
    }

    if matches!(entity.entity_type.as_str(), "init" | "init_declaration") {
        let wrapped = format!("struct __SemSignature {{\n{}\n}}\n", entity.content);
        parse_swift_signature_source(parser, &wrapped)
    } else {
        None
    }
}

fn parse_swift_signature_source(
    parser: &mut tree_sitter::Parser,
    source_text: &str,
) -> Option<Vec<Option<String>>> {
    let tree = parser.parse(source_text.as_bytes(), None)?;
    let source = source_text.as_bytes();
    find_first_swift_callable_declaration(tree.root_node())
        .map(|node| extract_swift_declaration_argument_labels(node, source))
}

fn find_first_swift_callable_declaration<'a>(
    root: tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        if matches!(node.kind(), "function_declaration" | "init_declaration") {
            return Some(node);
        }

        push_named_children_rev(&mut worklist, node);
    }

    None
}

fn extract_swift_declaration_argument_labels(
    node: tree_sitter::Node,
    source: &[u8],
) -> Vec<Option<String>> {
    let mut labels = Vec::new();
    let mut worklist = vec![node];

    while let Some(current) = worklist.pop() {
        if current.kind() == "function_body" {
            continue;
        }

        if current.kind() == "parameter" {
            labels.push(swift_parameter_argument_label(current, source));
            continue;
        }

        push_named_children_rev(&mut worklist, current);
    }

    labels
}

fn swift_parameter_argument_label(parameter: tree_sitter::Node, source: &[u8]) -> Option<String> {
    parameter
        .child_by_field_name("external_name")
        .or_else(|| parameter.child_by_field_name("name"))
        .and_then(|label| normalize_swift_label(label.utf8_text(source).ok()?))
}

fn extract_swift_call_argument_labels(
    call: tree_sitter::Node,
    source: &[u8],
) -> Option<Vec<Option<String>>> {
    let mut cursor = call.walk();
    let call_suffix = call
        .named_children(&mut cursor)
        .find(|child| child.kind() == "call_suffix")?;

    let mut suffix_cursor = call_suffix.walk();
    let value_arguments = call_suffix
        .named_children(&mut suffix_cursor)
        .find(|child| child.kind() == "value_arguments")?;

    let mut labels = Vec::new();
    let mut arg_cursor = value_arguments.walk();
    for argument in value_arguments
        .named_children(&mut arg_cursor)
        .filter(|child| child.kind() == "value_argument")
    {
        let label = argument
            .child_by_field_name("name")
            .and_then(|label| normalize_swift_label(label.utf8_text(source).ok()?));
        labels.push(label);
    }

    Some(labels)
}

fn normalize_swift_label(label: &str) -> Option<String> {
    let label = label.trim().trim_end_matches(':').trim();
    if label.is_empty() || label == "_" {
        None
    } else {
        Some(label.to_string())
    }
}

fn select_member_candidate(
    members: &[(String, String)],
    method: &str,
    argument_labels: Option<&[Option<String>]>,
    swift_call_signatures: &HashMap<String, SwiftCallSignature>,
) -> SwiftOverloadSelection {
    let candidates: Vec<&String> = members
        .iter()
        .filter_map(|(name, id)| (name == method).then_some(id))
        .collect();

    if argument_labels.is_none()
        && has_ambiguous_swift_signature_candidates(&candidates, swift_call_signatures)
    {
        return SwiftOverloadSelection::NoMatch;
    }

    match select_swift_overload_candidate(&candidates, argument_labels, swift_call_signatures) {
        SwiftOverloadSelection::NotApplicable => candidates
            .first()
            .map(|id| SwiftOverloadSelection::Matched((*id).clone()))
            .unwrap_or(SwiftOverloadSelection::NotApplicable),
        selection => selection,
    }
}

fn has_ambiguous_swift_signature_candidates(
    candidates: &[&String],
    swift_call_signatures: &HashMap<String, SwiftCallSignature>,
) -> bool {
    candidates
        .iter()
        .filter(|candidate| swift_call_signatures.contains_key(candidate.as_str()))
        .take(2)
        .count()
        > 1
}

fn select_swift_overload_candidate(
    candidates: &[&String],
    argument_labels: Option<&[Option<String>]>,
    swift_call_signatures: &HashMap<String, SwiftCallSignature>,
) -> SwiftOverloadSelection {
    let Some(argument_labels) = argument_labels else {
        return SwiftOverloadSelection::NotApplicable;
    };

    let signature_candidates: Vec<(&String, &SwiftCallSignature)> = candidates
        .iter()
        .copied()
        .filter_map(|candidate| {
            swift_call_signatures
                .get(candidate.as_str())
                .map(|signature| (candidate, signature))
        })
        .collect();
    if signature_candidates.is_empty() {
        return SwiftOverloadSelection::NotApplicable;
    }

    let exact_matches: Vec<&String> = signature_candidates
        .iter()
        .filter_map(|(candidate, signature)| {
            (signature.argument_labels.as_slice() == argument_labels).then_some(*candidate)
        })
        .collect();
    if exact_matches.len() == 1 {
        return SwiftOverloadSelection::Matched(exact_matches[0].clone());
    }
    if exact_matches.len() > 1 {
        return SwiftOverloadSelection::NoMatch;
    }

    if argument_labels.iter().all(Option::is_none) {
        let same_arity_matches: Vec<&String> = signature_candidates
            .iter()
            .filter_map(|(candidate, signature)| {
                (signature.argument_labels.len() == argument_labels.len()).then_some(*candidate)
            })
            .collect();
        if same_arity_matches.len() == 1 {
            return SwiftOverloadSelection::Matched(same_arity_matches[0].clone());
        }
        if same_arity_matches.len() > 1 {
            return SwiftOverloadSelection::NoMatch;
        }
    }

    SwiftOverloadSelection::NoMatch
}

/// Collect ALL AST references in a file with a single tree walk.
/// Each ref records its row so callers can bucket refs into entities by line range.
fn collect_all_file_refs(
    root: tree_sitter::Node,
    source: &[u8],
    config: &ScopeResolveConfig,
) -> Vec<AstRef> {
    let mut refs = AstRefCollector::new();
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        refs_visit_node(node, source, config, &mut refs);
        push_named_children_rev(&mut worklist, node);
    }
    refs.into_refs()
}

/// One node's worth of `collect_all_file_refs`: append every
/// `AstRef` this node contributes. Factored out verbatim from the walk's loop
/// body — see `scope_visit_node`'s doc comment for why the per-node semantics
/// live in one place shared by the unfused and fused traversal drivers.
fn refs_visit_node(
    node: tree_sitter::Node,
    source: &[u8],
    config: &ScopeResolveConfig,
    refs: &mut AstRefCollector,
) {
    {
        let node_row = node.start_position().row;
        let kind = node.kind();

        // Call nodes (e.g. "call", "call_expression", "method_invocation")
        if config.call_nodes.contains(&kind) {
            match &config.call_style {
                CallNodeStyle::FunctionField(field) => {
                    if let Some(func) = node.child_by_field_name(field) {
                        // Pass empty entity_name — self-ref filtering is done at resolution time
                        extract_call_ref(func, node, "", "", source, refs, config, node_row, None);
                    }
                }
                CallNodeStyle::FirstChild => {
                    // Swift/Kotlin: callee is the first named child (identifier or navigation_expression)
                    if let Some(func) = node.named_child(0) {
                        let argument_labels = extract_swift_call_argument_labels(node, source);
                        extract_call_ref(
                            func,
                            node,
                            "",
                            "",
                            source,
                            refs,
                            config,
                            node_row,
                            argument_labels,
                        );
                    }
                }
                CallNodeStyle::DirectMethod {
                    object_field,
                    method_field,
                } => {
                    let method_name = node
                        .child_by_field_name(method_field)
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    if !method_name.is_empty() && !is_builtin(method_name, config) {
                        if let Some(obj_node) = node.child_by_field_name(object_field) {
                            let receiver = obj_node.utf8_text(source).unwrap_or("");
                            let receiver = receiver.trim_end_matches('.');
                            refs.push_method_call(
                                receiver,
                                method_name,
                                None,
                                node_row,
                                node.start_byte(),
                                node.end_byte(),
                            );
                        } else {
                            refs.push_call(
                                method_name,
                                None,
                                node_row,
                                node.start_byte(),
                                node.end_byte(),
                            );
                        }
                    }
                }
            }
            return;
        }

        // Macro invocations (Rust: macro_invocation, macro name in "macro" field)
        if kind == "macro_invocation" {
            if let Some(macro_node) = node.child_by_field_name("macro") {
                let macro_name = macro_node.utf8_text(source).unwrap_or("");
                if !macro_name.is_empty() && !is_builtin(macro_name, config) {
                    refs.push_call(
                        macro_name,
                        None,
                        macro_node.start_position().row,
                        macro_node.start_byte(),
                        macro_node.end_byte(),
                    );
                }
            }
            return;
        }

        // New expression nodes (e.g. "new_expression", "object_creation_expression")
        if config.new_expr_nodes.contains(&kind) {
            if let Some(type_node) = node.child_by_field_name(config.new_expr_type_field) {
                let name = type_node.utf8_text(source).unwrap_or("");
                let name = name.rsplit('.').next().unwrap_or(name);
                if !name.is_empty() && !is_builtin(name, config) {
                    refs.push_call(
                        name,
                        None,
                        type_node.start_position().row,
                        type_node.start_byte(),
                        type_node.end_byte(),
                    );
                }
            }
            return;
        }

        // Composite literal nodes (e.g. Go "composite_literal")
        if config.composite_literal_nodes.contains(&kind) {
            if let Some(type_node) = node.child_by_field_name("type") {
                let name = type_node.utf8_text(source).unwrap_or("");
                if name.chars().next().map_or(false, |c| c.is_uppercase())
                    && !is_builtin(name, config)
                {
                    refs.push_call(
                        name,
                        None,
                        type_node.start_position().row,
                        type_node.start_byte(),
                        type_node.end_byte(),
                    );
                }
            }
        }
    }
}

fn build_refs_by_row(refs: &[AstRef]) -> Vec<Vec<usize>> {
    let max_row = refs.iter().map(|r| r.row).max().unwrap_or(0);
    let mut refs_by_row = vec![Vec::new(); max_row + 1];
    for (idx, ast_ref) in refs.iter().enumerate() {
        refs_by_row[ast_ref.row].push(idx);
    }
    refs_by_row
}

/// Extract a call reference from a function/callee node (shared across languages)
fn extract_call_ref(
    func: tree_sitter::Node,
    ref_node: tree_sitter::Node,
    _entity_id: &str,
    entity_name: &str,
    source: &[u8],
    refs: &mut AstRefCollector,
    config: &ScopeResolveConfig,
    row: usize,
    argument_labels: Option<Vec<Option<String>>>,
) {
    let func_kind = func.kind();

    // Bash wraps a command's callee one level deeper than every other
    // language here: `command`'s "name" field is typed `command_name`, whose
    // own single child is the actual `word` leaf (tree-sitter-bash's
    // `node-types.json`). Every other `CallNodeStyle` hands `extract_call_ref`
    // the real leaf directly, so this unwrap is bash-specific -- unwrapping
    // once and re-dispatching lets the `"word"` arm below (needed for fish,
    // whose `command` node hands over the `word` leaf without a wrapper)
    // handle the rest uniformly. Without this, bash calls were never
    // collected as refs at all: `command_name` matched none of the fast-path
    // kinds, no member_access pattern, and no scoped_call_node.
    if func_kind == "command_name" {
        if let Some(inner) = func.named_child(0) {
            extract_call_ref(
                inner,
                ref_node,
                _entity_id,
                entity_name,
                source,
                refs,
                config,
                row,
                argument_labels,
            );
        }
        return;
    }

    // PHP's plain-identifier node kind is `"name"` (tree-sitter-php's
    // `function_call_expression.function` field), not `"identifier"` --
    // discovered the same way as bash/fish: a language whose call
    // syntax `extract_call_ref`'s fast path special-cases by exact `.kind()`
    // string but didn't recognize. `"name"` is otherwise unused by any
    // other configured language's callee position, so this can only turn a
    // previously-always-dropped ref into a collected one.
    if func_kind == "identifier"
        || func_kind == "simple_identifier"
        || func_kind == "type_identifier"
        || func_kind == "word"
        || func_kind == "name"
    {
        let name = func.utf8_text(source).unwrap_or("");
        if !name.is_empty() && name != entity_name && !is_builtin(name, config) {
            refs.push_call(
                name,
                argument_labels,
                row,
                ref_node.start_byte(),
                ref_node.end_byte(),
            );
        }
        return;
    }

    // Check config member_access patterns
    for ma in config.member_access {
        if func_kind == ma.node_kind {
            extract_member_call_ref(
                func,
                ref_node,
                ma.object_field,
                ma.property_field,
                source,
                refs,
                row,
                argument_labels,
            );
            return;
        }
    }

    // Scoped call nodes (e.g. Rust "scoped_identifier" for Type::method)
    if config.scoped_call_nodes.contains(&func_kind) {
        let text = func.utf8_text(source).unwrap_or("");
        let mut parts: Vec<&str> = text.split("::").collect();
        // Strip Rust path-prefix segments (super::/self::/crate::) so the
        // remainder resolves against real modules/types. Without this,
        // `super::graph::foo` keeps the prefix in the path and never matches
        // the real `graph` module, so the call edge is silently dropped.
        let had_path_prefix = matches!(parts.first(), Some(&("super" | "self" | "crate")));
        while parts.len() > 1 && matches!(parts[0], "super" | "self" | "crate") {
            parts.remove(0);
        }
        let method_name = parts.last().copied().unwrap_or("");
        if !method_name.is_empty() && !is_builtin(method_name, config) {
            let emit_call = |refs: &mut AstRefCollector| {
                refs.push_call(
                    method_name,
                    None,
                    row,
                    ref_node.start_byte(),
                    ref_node.end_byte(),
                );
            };

            if parts.len() == 1 {
                // After stripping a path prefix (`super::foo` -> `foo`), resolve
                // the bare name through the scope chain.
                if had_path_prefix {
                    emit_call(refs);
                }
            } else {
                let receiver = parts[..parts.len() - 1].join("::");
                let receiver_base = parts[parts.len() - 2];
                let receiver_is_type = receiver_base
                    .chars()
                    .next()
                    .map_or(false, |c| c.is_uppercase());
                if had_path_prefix && !receiver_is_type {
                    // A path-prefixed module call (`super::graph::foo`) would
                    // become a lowercase-module ScopedCall, which the resolver
                    // can't link. Emit a plain Call to the final name so
                    // scope/global name resolution finds the entity.
                    emit_call(refs);
                } else if parts.len() == 2 && receiver_is_type && !is_builtin(receiver_base, config)
                {
                    refs.push_method_call(
                        receiver_base,
                        method_name,
                        None,
                        row,
                        ref_node.start_byte(),
                        ref_node.end_byte(),
                    );
                } else {
                    refs.push_scoped_call(
                        &receiver,
                        method_name,
                        row,
                        ref_node.start_byte(),
                        ref_node.end_byte(),
                    );
                }
            }
        }
    }
}

/// Extract a member/method call from a node with object+property fields.
/// Falls back to positional children for languages like Kotlin where
/// navigation_expression children don't have field names.
fn extract_member_call_ref(
    node: tree_sitter::Node,
    ref_node: tree_sitter::Node,
    object_field: &str,
    attr_field: &str,
    source: &[u8],
    refs: &mut AstRefCollector,
    row: usize,
    argument_labels: Option<Vec<Option<String>>>,
) {
    let obj_text = node
        .child_by_field_name(object_field)
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("");

    let attr_text = node
        .child_by_field_name(attr_field)
        .and_then(|n| {
            let text = n.utf8_text(source).ok()?;
            // Swift navigation_suffix includes the dot prefix (.validate → validate)
            Some(text.trim_start_matches('.'))
        })
        .unwrap_or("");

    if !obj_text.is_empty() && !attr_text.is_empty() {
        push_method_call_ref(obj_text, attr_text, refs, ref_node, row, argument_labels);
        return;
    }

    // Fallback: positional children (Kotlin navigation_expression has no field names)
    let child_count = node.named_child_count();
    if child_count >= 2 {
        let obj = node
            .named_child(0)
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("");
        let last_idx = (child_count - 1) as u32;
        let attr = node
            .named_child(last_idx)
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("");
        if !obj.is_empty() && !attr.is_empty() {
            push_method_call_ref(obj, attr, refs, ref_node, row, argument_labels);
        }
    }
}

fn push_method_call_ref(
    obj: &str,
    method: &str,
    refs: &mut AstRefCollector,
    node: tree_sitter::Node,
    row: usize,
    argument_labels: Option<Vec<Option<String>>>,
) {
    refs.push_method_call(
        obj,
        method,
        argument_labels,
        row,
        node.start_byte(),
        node.end_byte(),
    );
}

/// Resolve a single reference against scopes and symbol tables.
/// Resolve a qualified callee's final name (`Type::NAME`, `module::path::NAME`) to
/// an entity id, precisely. An explicit import wins; then, when allowed, a same-file
/// definition; then a globally UNIQUE definition. The qualifier is the disambiguator,
/// so we never guess among multiple cross-file candidates for a common name
/// (`new`, `default`, `from`) — those stay unresolved rather than producing a wrong
/// edge.
fn resolve_qualified_callee_name(
    name: &str,
    import_table_by_name: &HashMap<&str, &str>,
    file_lookup: &FileEntityLookup<'_>,
    symbol_table: &HashMap<String, Vec<String>>,
    from_entity_id: &str,
    allow_same_file: bool,
    rec: &mut Recorder,
) -> Option<String> {
    if let Some(target_id) = import_table_by_name.get(name) {
        if *target_id != from_entity_id {
            return Some((*target_id).to_string());
        }
    }
    if allow_same_file {
        if let Some(same_file) = file_lookup.first_id_by_name(name) {
            if same_file != from_entity_id {
                return Some(same_file.to_string());
            }
        }
    }
    rec.one(Table::SymbolTable, name);
    if let Some(ids) = symbol_table.get(name) {
        if ids.len() == 1 && ids[0] != from_entity_id {
            return Some(ids[0].clone());
        }
    }
    None
}

fn resolve_ref(
    ast_ref: &AstRef,
    scope_idx: usize,
    // True exactly when the caller's entity scope-index lookup missed BOTH
    // `entity_inner_scope` and `entity_scope_map`, so `scope_idx` is the
    // `unwrap_or(0)` default rather than a real resolution — the entity's
    // scope context is *unknown*, not module-level. The Go package-qualified
    // MethodCall fallback keys off this: guessing a callee from the import
    // table while blind to the scope context manufactures wrong edges, and a
    // wrong edge is a lie where a missing edge is merely honest. A lookup HIT
    // on scope 0 (a genuine module-level entity) leaves this false and the
    // fallback fully active.
    scope_lookup_missed: bool,
    scopes: &[Scope],
    symbol_table: &HashMap<String, Vec<String>>,
    class_members: &HashMap<String, Vec<(String, String)>>,
    owner_members: &HashMap<String, Vec<(String, String)>>,
    import_table_by_name: &HashMap<&str, &str>,
    instance_attr_types: &HashMap<(String, String), String>,
    entity_map: &HashMap<String, EntityInfo>,
    swift_call_signatures: &HashMap<String, SwiftCallSignature>,
    file_path: &str,
    from_entity_id: &str,
    allow_cross_file_calls: bool,
    allow_implicit_instance_member_receiver: bool,
    file_lookup: &FileEntityLookup<'_>,
    lookup_cache: &mut ScopeLookupCache,
    mut profile: Option<&mut prof::FileAccum>,
    // red-green: every lookup below that can reach another file's data
    // records its key here, hit or miss. `import_table_by_name` is not recorded
    // per lookup — it holds only this file's own imports and the caller records
    // the whole slice once as `Table::ImportsForFile`.
    rec: &mut Recorder,
) -> Option<(String, RefType, &'static str)> {
    match &ast_ref.kind {
        AstRefKind::Call {
            name,
            argument_labels,
        } => {
            // one combined per-scope-level walk replaces the old
            // bindings-only shadow gate + separate defs lookup. `.defs` is
            // checked before `.bindings` at each level, so same-scope
            // co-populated def+binding resolves as the definition it is,
            // while an inner bindings-only hit still stops resolution
            // exactly as the old gate did.
            let scope_chain_def = match lookup_scope_chain_respecting_shadows_cached(
                scope_idx,
                scopes,
                name,
                lookup_cache,
            ) {
                ScopeChainLookup::Defined(eid) => Some(eid),
                ScopeChainLookup::Shadowed => return None,
                ScopeChainLookup::NotFound => None,
            };

            // Swift overload disambiguation needs call-signature data that is only
            // built for Swift sources. For every other language the pre-resolution
            // candidate scan below is inert, yet it scans the global symbol table —
            // in a large monorepo a single common name maps to thousands of entities,
            // so this scan dominates graph resolution. Skip it unless Swift
            // signatures are present.
            if !swift_call_signatures.is_empty() {
                if argument_labels.is_some() {
                    if let Some(target_ids) = symbol_table.get(name.as_ref()) {
                        let same_file_targets: Vec<&String> = target_ids
                            .iter()
                            .filter(|id| {
                                entity_map
                                    .get(*id)
                                    .map_or(false, |e| e.file_path == file_path)
                            })
                            .collect();
                        let visible_targets: Vec<&String> = if !same_file_targets.is_empty() {
                            same_file_targets
                        } else if allow_cross_file_calls {
                            target_ids.iter().collect()
                        } else {
                            Vec::new()
                        };
                        match select_swift_overload_candidate(
                            &visible_targets,
                            argument_labels.as_deref(),
                            swift_call_signatures,
                        ) {
                            SwiftOverloadSelection::Matched(target_id) => {
                                let is_constructor =
                                    name.chars().next().map_or(false, |c| c.is_uppercase());
                                let ref_type = if is_constructor {
                                    RefType::TypeRef
                                } else {
                                    RefType::Calls
                                };
                                return Some((target_id, ref_type, "scope_chain"));
                            }
                            SwiftOverloadSelection::NoMatch => return None,
                            SwiftOverloadSelection::NotApplicable => {}
                        }
                    }
                } else if let Some(target_ids) = symbol_table.get(name.as_ref()) {
                    let same_file_targets: Vec<&String> = target_ids
                        .iter()
                        .filter(|id| {
                            entity_map
                                .get(*id)
                                .map_or(false, |e| e.file_path == file_path)
                        })
                        .collect();
                    let visible_targets: Vec<&String> = if !same_file_targets.is_empty() {
                        same_file_targets
                    } else if allow_cross_file_calls {
                        target_ids.iter().collect()
                    } else {
                        Vec::new()
                    };
                    if has_ambiguous_swift_signature_candidates(
                        &visible_targets,
                        swift_call_signatures,
                    ) {
                        return None;
                    }
                }
            }

            // 1. Walk scope chain for the name
            if let Some(eid) = scope_chain_def {
                if eid != from_entity_id {
                    return Some((eid, RefType::Calls, "scope_chain"));
                }
            }

            // 2. Check import table. The per-file table only holds this file's
            // imports, so a name lookup suffices — avoiding a (path, name) key string
            // allocated for every reference (millions on a large repo).
            if let Some(target_id) = import_table_by_name.get(name.as_ref()) {
                return Some(((*target_id).to_string(), RefType::Calls, "import"));
            }

            // 3. Global symbol table fallback (constructor calls or cross-file functions)
            rec.one(Table::SymbolTable, name.as_ref());
            if let Some(target_ids) = symbol_table.get(name.as_ref()) {
                if let Some(acc) = profile.as_deref_mut() {
                    acc.record_call_global(name, target_ids.len());
                }
                let is_constructor = name.chars().next().map_or(false, |c| c.is_uppercase());
                let ref_type = if is_constructor {
                    RefType::TypeRef
                } else {
                    RefType::Calls
                };

                if swift_call_signatures.is_empty() {
                    // Fast path: the per-file name index gives the first same-file
                    // definition in O(1); the cross-file fallback takes the first
                    // global definition. Both preserve entity-discovery order, so the
                    // result matches the candidate scan below without iterating the
                    // thousands of same-named entities a monorepo accumulates.
                    let target = file_lookup
                        .first_id_by_name(name)
                        .map(str::to_string)
                        .or_else(|| {
                            if is_constructor || allow_cross_file_calls {
                                target_ids.first().cloned()
                            } else {
                                None
                            }
                        });
                    if let Some(tid) = target {
                        return Some((tid, ref_type, "scope_chain"));
                    }
                    return None;
                }

                let same_file_targets: Vec<&String> = target_ids
                    .iter()
                    .filter(|id| {
                        entity_map
                            .get(*id)
                            .map_or(false, |e| e.file_path == file_path)
                    })
                    .collect();
                let visible_targets: Vec<&String> = if !same_file_targets.is_empty() {
                    same_file_targets
                } else if is_constructor || allow_cross_file_calls {
                    target_ids.iter().collect()
                } else {
                    Vec::new()
                };
                let target = match select_swift_overload_candidate(
                    &visible_targets,
                    argument_labels.as_deref(),
                    swift_call_signatures,
                ) {
                    SwiftOverloadSelection::Matched(target_id) => Some(target_id),
                    SwiftOverloadSelection::NoMatch => return None,
                    SwiftOverloadSelection::NotApplicable => {
                        visible_targets.first().map(|id| (*id).clone())
                    }
                };
                if let Some(tid) = target {
                    return Some((tid, ref_type, "scope_chain"));
                }
            }

            None
        }

        AstRefKind::ScopedCall { path, name } => {
            // `module::path::fn()` or `Enum::Variant::method()`. Resolve only when it
            // is precise: the last path segment names a repo type that owns `name`, or
            // the callee name is explicitly imported. We deliberately do NOT bind to a
            // same-name repo function for a bare module path (`foo::bar::baz()` must
            // not resolve to a local `baz`, even a unique one) — sem does not track
            // full module paths, so guessing there would manufacture false edges.
            let type_hint = path.rsplit("::").next().unwrap_or(path.as_ref());
            rec.one(Table::ClassMembers, type_hint);
            if let Some(members) = class_members.get(type_hint) {
                if let Some((_, target_id)) = members
                    .iter()
                    .find(|(member, _)| member.as_str() == name.as_ref())
                {
                    if target_id != from_entity_id {
                        return Some((target_id.clone(), RefType::Calls, "scoped_call"));
                    }
                }
            }
            if let Some(target_id) = import_table_by_name.get(name.as_ref()) {
                if *target_id != from_entity_id {
                    return Some(((*target_id).to_string(), RefType::Calls, "scoped_call"));
                }
            }
            // Rust module-alias qualified call: `alias::item()` where `alias`
            // was brought in by `use path::alias;`. `register_rust_
            // module_import` writes exactly this `"{alias}::{item}"` key, so a
            // hit here requires an actual recorded `use` of a real module —
            // never a name-only guess (see that function's doc).
            let qualified = format!("{path}::{name}");
            if let Some(target_id) = import_table_by_name.get(qualified.as_str()) {
                if *target_id != from_entity_id {
                    return Some(((*target_id).to_string(), RefType::Calls, "module_alias"));
                }
            }
            None
        }

        AstRefKind::MethodCall {
            receiver: raw_receiver,
            method,
            argument_labels,
        } => {
            // Strip prefix operators like ! (Swift: `!dog.validate()`)
            let receiver = normalized_method_receiver(raw_receiver);
            if receiver == "self" || receiver == "this" {
                // self.method() -> find in enclosing class
                let mut idx = scope_idx;
                loop {
                    if scopes[idx].kind == "class" {
                        if let Some(owner_id) = scopes[idx].owner_id.as_deref() {
                            rec.one(Table::EntityMap, owner_id);
                        }
                        if let Some(class_name) = scopes[idx]
                            .owner_id
                            .as_ref()
                            .and_then(|owner_id| entity_map.get(owner_id))
                            .map(|owner| owner.name.as_str())
                        {
                            rec.one(Table::ClassMembers, class_name);
                            if let Some(members) = class_members.get(class_name) {
                                match select_member_profiled!(
                                    members,
                                    method,
                                    argument_labels.as_deref(),
                                    swift_call_signatures,
                                    class_name,
                                    profile
                                ) {
                                    SwiftOverloadSelection::Matched(eid) => {
                                        return Some((eid, RefType::Calls, "scope_chain"));
                                    }
                                    SwiftOverloadSelection::NoMatch => return None,
                                    SwiftOverloadSelection::NotApplicable => {
                                        if argument_labels.is_some() {
                                            return None;
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(eid) = scopes[idx].defs.get(method.as_ref()) {
                            return Some((eid.clone(), RefType::Calls, "scope_chain"));
                        }
                        break;
                    }
                    match scopes[idx].parent {
                        Some(p) => idx = p,
                        None => break,
                    }
                }
                return None;
            }

            // Handle chained self.attr.method() pattern
            // receiver is "self.X" where X is an instance attribute
            if receiver.starts_with("self.") || receiver.starts_with("this.") {
                let attr_name = &receiver[5..]; // strip "self." or "this."
                                                // Find the enclosing class name
                let class_name =
                    find_enclosing_class_cached(scope_idx, scopes, entity_map, lookup_cache, rec);
                if let Some(cn) = class_name {
                    // Look up instance attribute type
                    rec.two(Table::InstanceAttrTypes, &cn, attr_name);
                    if let Some(attr_type) = instance_attr_types.get(&(cn, attr_name.to_string())) {
                        rec.one(Table::ClassMembers, attr_type.as_str());
                        if let Some(members) = class_members.get(attr_type.as_str()) {
                            match select_member_profiled!(
                                members,
                                method,
                                argument_labels.as_deref(),
                                swift_call_signatures,
                                attr_type.as_str(),
                                profile
                            ) {
                                SwiftOverloadSelection::Matched(mid) => {
                                    return Some((mid, RefType::Calls, "type_tracking"));
                                }
                                SwiftOverloadSelection::NoMatch => return None,
                                SwiftOverloadSelection::NotApplicable => {}
                            }
                        }
                    }
                }
            }

            // Handle chained var.field.method() pattern (e.g. Go receiver: t.Conn.Execute())
            if receiver.contains('.')
                && !receiver.starts_with("self.")
                && !receiver.starts_with("this.")
            {
                if let Some(dot_pos) = receiver.find('.') {
                    let var_part = &receiver[..dot_pos];
                    let field_part = &receiver[dot_pos + 1..];
                    if let Some(var_type) =
                        lookup_type_in_scopes_cached(scope_idx, scopes, var_part, lookup_cache)
                    {
                        rec.two(Table::InstanceAttrTypes, &var_type, field_part);
                        if let Some(attr_type) =
                            instance_attr_types.get(&(var_type, field_part.to_string()))
                        {
                            rec.one(Table::ClassMembers, attr_type.as_str());
                            if let Some(members) = class_members.get(attr_type.as_str()) {
                                match select_member_profiled!(
                                    members,
                                    method,
                                    argument_labels.as_deref(),
                                    swift_call_signatures,
                                    attr_type.as_str(),
                                    profile
                                ) {
                                    SwiftOverloadSelection::Matched(mid) => {
                                        return Some((mid, RefType::Calls, "type_tracking"));
                                    }
                                    SwiftOverloadSelection::NoMatch => return None,
                                    SwiftOverloadSelection::NotApplicable => {}
                                }
                            }
                        }
                    }
                }
            }

            // receiver.method() -> look up receiver type, then resolve method.
            // Diagnostic counters (semx profile): an *attempt* is a plain
            // identifier receiver handed to `lookup_type_before_class_scope`;
            // a *success* is that path landing the `"type_tracking"` edge
            // below. Both no-op unless the resolver profiler is on.
            let __type_directed_attempt = prof::enabled() && is_simple_identifier_name(receiver);
            if __type_directed_attempt {
                prof::add_resolve_ref_type_directed_attempt();
            }
            let receiver_type = if let Some(receiver_type) =
                lookup_type_before_class_scope(scope_idx, scopes, receiver)
            {
                Some(receiver_type)
            } else if allow_implicit_instance_member_receiver
                && is_simple_identifier_name(receiver)
                && !is_local_binding_in_scopes_cached(scope_idx, scopes, receiver, lookup_cache)
            {
                match find_enclosing_class_cached(scope_idx, scopes, entity_map, lookup_cache, rec)
                {
                    Some(class_name) => {
                        rec.two(Table::InstanceAttrTypes, &class_name, receiver);
                        instance_attr_types
                            .get(&(class_name, receiver.to_string()))
                            .cloned()
                    }
                    None => None,
                }
            } else {
                None
            };

            if let Some(class_name) = receiver_type {
                rec.one(Table::ClassMembers, class_name.as_str());
                if let Some(members) = class_members.get(class_name.as_str()) {
                    match select_member_profiled!(
                        members,
                        method,
                        argument_labels.as_deref(),
                        swift_call_signatures,
                        class_name.as_str(),
                        profile
                    ) {
                        SwiftOverloadSelection::Matched(mid) => {
                            if __type_directed_attempt {
                                prof::add_resolve_ref_type_directed_success();
                            }
                            return Some((mid, RefType::Calls, "type_tracking"));
                        }
                        SwiftOverloadSelection::NoMatch => return None,
                        SwiftOverloadSelection::NotApplicable => {}
                    }
                }
            }

            // Static call: `ClassName.staticMethod()` — the receiver is a class itself,
            // not a typed variable. Only fires for an uppercase identifier that names a
            // known class and isn't shadowed by a local binding.
            if is_simple_identifier_name(receiver)
                && receiver.chars().next().map_or(false, |c| c.is_uppercase())
                && !is_local_binding_in_scopes_cached(scope_idx, scopes, receiver, lookup_cache)
            {
                rec.one(Table::ClassMembers, receiver);
                if let Some(members) = class_members.get(receiver) {
                    if let SwiftOverloadSelection::Matched(mid) = select_member_profiled!(
                        members,
                        method,
                        argument_labels.as_deref(),
                        swift_call_signatures,
                        receiver,
                        profile
                    ) {
                        return Some((mid, RefType::Calls, "static_call"));
                    }
                }
                // `Type::method()` where the impl block is not keyed under `Type` in
                // class_members (e.g. `impl Trait for Type`), or a free associated fn.
                // Only when `Type` is itself a known repo entity: resolve the method by
                // a same-file or globally unique definition (the `Type::` qualifier is
                // the disambiguator), so `Vec::new()` and friends stay unresolved.
                rec.one(Table::SymbolTable, receiver);
                if symbol_table.contains_key(receiver) {
                    if let Some(hit) = resolve_qualified_callee_name(
                        method,
                        import_table_by_name,
                        file_lookup,
                        symbol_table,
                        from_entity_id,
                        true,
                        rec,
                    ) {
                        return Some((hit, RefType::Calls, "static_call"));
                    }
                }
            }

            // Inside class methods, unqualified property receivers resolve
            // against the enclosing instance when no local binding shadows them.
            rec.one(Table::EntityMap, from_entity_id);
            let from_entity_is_container_type =
                entity_map.get(from_entity_id).map_or(false, |entity| {
                    matches!(
                        entity.entity_type.as_str(),
                        "class"
                            | "struct"
                            | "interface"
                            | "enum"
                            | "protocol_declaration"
                            | "object_declaration"
                            | "companion_object"
                    )
                });

            if allow_implicit_instance_member_receiver
                && !from_entity_is_container_type
                && !is_local_binding_in_scopes_cached(scope_idx, scopes, receiver, lookup_cache)
            {
                if let Some(class_name) =
                    find_enclosing_class_cached(scope_idx, scopes, entity_map, lookup_cache, rec)
                {
                    rec.two(Table::InstanceAttrTypes, &class_name, receiver);
                    if let Some(attr_type) =
                        instance_attr_types.get(&(class_name, receiver.to_string()))
                    {
                        rec.one(Table::ClassMembers, attr_type.as_str());
                        if let Some(members) = class_members.get(attr_type.as_str()) {
                            match select_member_profiled!(
                                members,
                                method,
                                argument_labels.as_deref(),
                                swift_call_signatures,
                                attr_type.as_str(),
                                profile
                            ) {
                                SwiftOverloadSelection::Matched(mid) => {
                                    return Some((mid, RefType::Calls, "type_tracking"));
                                }
                                SwiftOverloadSelection::NoMatch => return None,
                                SwiftOverloadSelection::NotApplicable => {}
                            }
                        }
                    }
                }
            }

            // ClassName.method() static call, only when ClassName is visible and
            // not shadowed by a local binding.: the shadow gate and
            // the defs walk are one combined per-level lookup — a same-scope
            // co-populated def+binding still resolves as the definition.
            if let ScopeChainLookup::Defined(class_id) =
                lookup_scope_chain_respecting_shadows_cached(
                    scope_idx,
                    scopes,
                    receiver,
                    lookup_cache,
                )
            {
                rec.one(Table::EntityMap, &class_id);
                if let Some(info) = entity_map.get(&class_id) {
                    if matches!(info.entity_type.as_str(), "module" | "variable" | "object")
                        && info.name == receiver
                    {
                        rec.one(Table::OwnerMembers, &class_id);
                        if let Some(mid) =
                            lookup_entity_member(owner_members, &class_id, method).or_else(|| {
                                lookup_owned_scope_member(scopes, &class_id, method)
                            })
                        {
                            return Some((mid, RefType::Calls, "scope_chain"));
                        }
                    } else if matches!(
                        info.entity_type.as_str(),
                        "class" | "struct" | "interface"
                    ) && info.name == receiver
                    {
                        rec.one(Table::ClassMembers, &info.name);
                        if let Some(members) = class_members.get(&info.name) {
                            match select_member_profiled!(
                                members,
                                method,
                                argument_labels.as_deref(),
                                swift_call_signatures,
                                info.name.as_str(),
                                profile
                            ) {
                                SwiftOverloadSelection::Matched(mid) => {
                                    return Some((mid, RefType::Calls, "scope_chain"));
                                }
                                SwiftOverloadSelection::NoMatch => return None,
                                SwiftOverloadSelection::NotApplicable => {}
                            }
                        }
                    }
                }
            }

            // Fallback: check import table for the receiver
            if !is_local_binding_in_scopes_cached(scope_idx, scopes, receiver, lookup_cache) {
                if let Some(target_id) = import_table_by_name.get(receiver) {
                    rec.one(Table::EntityMap, target_id);
                    if let Some(info) = entity_map.get(*target_id) {
                        if matches!(info.entity_type.as_str(), "class" | "struct") {
                            rec.one(Table::ClassMembers, &info.name);
                            if let Some(members) = class_members.get(&info.name) {
                                match select_member_profiled!(
                                    members,
                                    method,
                                    argument_labels.as_deref(),
                                    swift_call_signatures,
                                    info.name.as_str(),
                                    profile
                                ) {
                                    SwiftOverloadSelection::Matched(mid) => {
                                        return Some((mid, RefType::Calls, "type_tracking"));
                                    }
                                    SwiftOverloadSelection::NoMatch => return None,
                                    SwiftOverloadSelection::NotApplicable => {}
                                }
                            }
                        }
                    }
                }

                // Namespace import: alias.method()
                let namespaced = format!("{receiver}.{method}");
                if let Some(target_id) = import_table_by_name.get(namespaced.as_str()) {
                    return Some(((*target_id).to_string(), RefType::Calls, "import"));
                }
            }

            // Go package-qualified call: package.Function()
            // Try the method name directly in the import table — but only
            // when this entity's scope lookup actually HIT. On a missed
            // entity-id lookup the scope context is unknown (`scope_idx` is
            // just the unwrap_or(0) default), and guessing a callee from the
            // import table produces wrong edges: a missing edge is honest, a
            // wrong edge is a lie. A hit on scope 0 for a genuine
            // module-level entity keeps the fallback active.
            if !scope_lookup_missed && file_path.ends_with(".go") {
                if let Some(target_id) = import_table_by_name.get(method.as_ref()) {
                    return Some(((*target_id).to_string(), RefType::Calls, "import"));
                }
            }

            // Last resort: a repo-wide unique METHOD name is unambiguous even
            // when the receiver's type is unknown — `index.keep_levels()` can
            // only mean the one `keep_levels` the repo defines. One candidate,
            // one edge; two candidates, no edge (guessing would manufacture
            // false callers). Restricted to entities with a parent (methods),
            // so attribute calls never bind to same-named free functions.
            // Dynamic languages only: there, receiver types are statically
            // unknowable and the missing edge is pure blindness; in static
            // languages an unresolved receiver is deliberate (shadowed
            // import, instance property) and must stay unresolved.
            let dynamic_receiver_lang = file_path.ends_with(".py") || file_path.ends_with(".rb");
            if allow_cross_file_calls && dynamic_receiver_lang {
                rec.one(Table::SymbolTable, method.as_ref());
                if let Some(target_ids) = symbol_table.get(method.as_ref()) {
                    if let [tid] = target_ids.as_slice() {
                        rec.one(Table::EntityMap, tid);
                        if tid != from_entity_id
                            && entity_map.get(tid).is_some_and(|e| {
                                e.parent_id.is_some()
                                    && matches!(e.entity_type.as_str(), "method" | "function")
                            })
                        {
                            return Some((tid.clone(), RefType::Calls, "unique_method_name"));
                        }
                    }
                }
            }

            None
        }
    }
}

fn allows_implicit_instance_member_receiver(
    file_path: &str,
    entity_type: &str,
    entity_content: &str,
) -> bool {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    let supports_implicit_receiver = matches!(
        ext,
        "swift"
            | "kt"
            | "kts"
            | "java"
            | "cs"
            | "cpp"
            | "cc"
            | "cxx"
            | "hpp"
            | "hh"
            | "hxx"
            | "h"
            | "scala"
            | "dart"
    );

    supports_implicit_receiver
        && matches!(
            entity_type,
            "function" | "method" | "init" | "init_declaration" | "constructor_declaration"
        )
        && !has_static_member_modifier(ext, entity_content)
}

fn has_static_member_modifier(ext: &str, entity_content: &str) -> bool {
    let header = entity_content
        .split(|ch| ch == '{' || ch == '=')
        .next()
        .unwrap_or(entity_content);
    let header_without_comments = strip_member_header_comments(header);
    let tokens = header_without_comments
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let declaration_start = tokens
        .iter()
        .position(|token| {
            matches!(
                *token,
                "func" | "function" | "fn" | "constructor" | "init" | "var" | "let" | "subscript"
            )
        })
        .unwrap_or(tokens.len());

    tokens[..declaration_start]
        .iter()
        .any(|token| *token == "static" || (ext == "swift" && *token == "class"))
}

fn strip_member_header_comments(header: &str) -> String {
    let mut output = String::with_capacity(header.len());
    let mut chars = header.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '/' {
            output.push(ch);
            continue;
        }

        match chars.peek().copied() {
            Some('/') => {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\n' {
                        output.push(' ');
                        break;
                    }
                }
            }
            Some('*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                output.push(' ');
            }
            _ => output.push(ch),
        }
    }

    output
}

fn is_simple_identifier_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first == '_' || first.is_alphabetic()) && chars.all(|ch| ch == '_' || ch.is_alphanumeric())
}

fn lookup_owned_scope_member(scopes: &[Scope], owner_id: &str, member: &str) -> Option<String> {
    scopes
        .iter()
        .find(|scope| scope.owner_id.as_deref() == Some(owner_id))
        .and_then(|scope| scope.defs.get(member).cloned())
}

fn lookup_entity_member(
    owner_members: &HashMap<String, Vec<(String, String)>>,
    owner_id: &str,
    member: &str,
) -> Option<String> {
    owner_members
        .get(owner_id)
        .and_then(|members| members.iter().find(|(name, _)| name == member))
        .map(|(_, id)| id.clone())
}

/// Find the class name for the enclosing class scope.
fn find_enclosing_class(
    start_scope: usize,
    scopes: &[Scope],
    entity_map: &HashMap<String, EntityInfo>,
    rec: &mut Recorder,
) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if scopes[idx].kind == "class" {
            if let Some(ref oid) = scopes[idx].owner_id {
                rec.one(Table::EntityMap, oid);
                return entity_map.get(oid).map(|e| e.name.clone());
            }
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

/// Memoized [`find_enclosing_class`].
///
/// The cache is per file and per resolution run, so a memoized hit re-uses a
/// value the *same* run already recorded a read for — the read set stays a
/// superset either way.
fn find_enclosing_class_cached(
    start_scope: usize,
    scopes: &[Scope],
    entity_map: &HashMap<String, EntityInfo>,
    cache: &mut ScopeLookupCache,
    rec: &mut Recorder,
) -> Option<String> {
    if let Some(cached) = cache.enclosing_classes.get(&start_scope) {
        return cached.clone();
    }
    let value = find_enclosing_class(start_scope, scopes, entity_map, rec);
    cache.enclosing_classes.insert(start_scope, value.clone());
    value
}

/// Result of a shadow-respecting scope-chain lookup.
///
/// Per scope level, `.defs` is consulted before `.bindings`: a def hit
/// resolves to the entity id; a bindings-only hit stops the walk. This
/// ordering matters when both maps co-populate the same name in one scope
/// (e.g. a nested entity registered into its enclosing function's `.defs`
/// while `scan_assignments` also records the same declarator as a
/// `.binding`) — the grammar admits no redeclaration, so co-population at
/// the *same* scope index can only be the same declaration, and the def
/// must win instead of being read as a shadow. A binding in a
/// strictly nearer (inner) scope with no def of its own still shadows
/// everything above it, preserving the guarantee the old
/// `is_local_binding_in_scopes_cached` gate protected.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ScopeChainLookup {
    /// Name resolved to an entity id via a scope's `.defs`.
    Defined(String),
    /// A `.bindings`-only hit stopped the walk before any `.defs` hit.
    Shadowed,
    /// Neither map knows the name anywhere up the chain.
    NotFound,
}

/// Walk up the scope chain resolving `name`, checking each level's `.defs`
/// before its `.bindings`.
fn lookup_scope_chain_respecting_shadows(
    start_scope: usize,
    scopes: &[Scope],
    name: &str,
) -> ScopeChainLookup {
    let mut idx = start_scope;
    loop {
        if let Some(eid) = scopes[idx].defs.get(name) {
            return ScopeChainLookup::Defined(eid.clone());
        }
        if scopes[idx].bindings.contains(name) {
            return ScopeChainLookup::Shadowed;
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return ScopeChainLookup::NotFound,
        }
    }
}

fn lookup_scope_chain_respecting_shadows_cached(
    start_scope: usize,
    scopes: &[Scope],
    name: &str,
    cache: &mut ScopeLookupCache,
) -> ScopeChainLookup {
    if let Some(cached) = cache
        .shadow_respecting_defs
        .get(&start_scope)
        .and_then(|scope_cache| scope_cache.get(name))
    {
        return cached.clone();
    }
    let value = lookup_scope_chain_respecting_shadows(start_scope, scopes, name);
    cache
        .shadow_respecting_defs
        .entry(start_scope)
        .or_default()
        .insert(name.to_string(), value.clone());
    value
}

/// Walk up the scope chain looking for a local binding that shadows a definition.
fn is_local_binding_in_scopes(start_scope: usize, scopes: &[Scope], name: &str) -> bool {
    let mut idx = start_scope;
    loop {
        if scopes[idx].bindings.contains(name) {
            return true;
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return false,
        }
    }
}

fn is_local_binding_in_scopes_cached(
    start_scope: usize,
    scopes: &[Scope],
    name: &str,
    cache: &mut ScopeLookupCache,
) -> bool {
    if let Some(cached) = cache
        .local_bindings
        .get(&start_scope)
        .and_then(|scope_cache| scope_cache.get(name))
    {
        return *cached;
    }
    let value = is_local_binding_in_scopes(start_scope, scopes, name);
    cache
        .local_bindings
        .entry(start_scope)
        .or_default()
        .insert(name.to_string(), value);
    value
}

/// Walk up the scope chain looking for a type binding.
fn lookup_type_in_scopes(start_scope: usize, scopes: &[Scope], var_name: &str) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if let Some(type_name) = scopes[idx].types.get(var_name) {
            return Some(type_name.clone());
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

fn lookup_type_before_class_scope(
    start_scope: usize,
    scopes: &[Scope],
    var_name: &str,
) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if scopes[idx].kind == "class" {
            return None;
        }
        if let Some(type_name) = scopes[idx].types.get(var_name) {
            return Some(type_name.clone());
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

fn lookup_type_in_scopes_cached(
    start_scope: usize,
    scopes: &[Scope],
    var_name: &str,
    cache: &mut ScopeLookupCache,
) -> Option<String> {
    if let Some(cached) = cache
        .types
        .get(&start_scope)
        .and_then(|scope_cache| scope_cache.get(var_name))
    {
        return cached.clone();
    }
    let value = lookup_type_in_scopes(start_scope, scopes, var_name);
    cache
        .types
        .entry(start_scope)
        .or_default()
        .insert(var_name.to_string(), value.clone());
    value
}

fn is_builtin(name: &str, config: &ScopeResolveConfig) -> bool {
    // Common builtins across languages
    if matches!(
        name,
        "None" | "True" | "False" | "null" | "undefined" | "nil"
    ) {
        return true;
    }
    config.builtins.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// follow-up: `approx_heap_bytes` must walk the nested heap
    /// content the container-capacity terms skip — each [`Scope`]'s six
    /// internal collections' String contents, every [`AstRefKind`] variant's
    /// String payloads (`argument_labels` included), and the actual
    /// key+value string bytes of `return_type_map`/`instance_attr_types`/
    /// `init_params`/`attr_to_param`. Inserting known-size strings must grow
    /// the estimate by at least those bytes (`.capacity()` >= `.len()` is the
    /// honest bound; a fresh hash table's own slack only adds more).
    #[test]
    fn approx_heap_bytes_walks_nested_string_content() {
        let mut facts = dummy_precomputed_facts_for_test("");
        let before = facts.approx_heap_bytes();

        // One scope exercising all six internal collections.
        let (def_key, def_val) = ("d".repeat(100), "D".repeat(200));
        let binding = "b".repeat(50);
        let row_key = "r".repeat(40);
        let (type_key, type_val) = ("t".repeat(60), "T".repeat(70));
        let (pend_key, pend_val) = ("p".repeat(30), "P".repeat(35));
        let (pf_key, pf_a, pf_b) = ("f".repeat(20), "F".repeat(25), "G".repeat(26));

        let mut defs = HashMap::default();
        defs.insert(def_key.clone(), def_val.clone());
        let mut bindings = HashSet::default();
        bindings.insert(binding.clone());
        let mut binding_rows = HashMap::default();
        binding_rows.insert(row_key.clone(), Vec::new());
        let mut types = HashMap::default();
        types.insert(type_key.clone(), type_val.clone());
        let mut pending_call_types = HashMap::default();
        pending_call_types.insert(pend_key.clone(), pend_val.clone());
        let mut pending_field_types = HashMap::default();
        pending_field_types.insert(pf_key.clone(), (pf_a.clone(), pf_b.clone()));

        facts.scopes.push(Scope {
            parent: None,
            defs,
            bindings,
            binding_rows,
            types,
            pending_call_types,
            pending_field_types,
            owner_id: None,
            kind: "module",
        });

        // One MethodCall ref: receiver/method payloads plus an
        // argument_labels Vec holding one Some(label) and one None.
        let (receiver, method, label) = ("R".repeat(80), "M".repeat(90), "L".repeat(45));
        facts.ast_refs.push(AstRef {
            kind: AstRefKind::MethodCall {
                receiver: Arc::from(receiver.as_str()),
                method: Arc::from(method.as_str()),
                argument_labels: Some(vec![Some(label.clone()), None]),
            },
            row: 0,
            start_byte: 0,
            end_byte: 1,
        });

        // The four maps: actual key+value string bytes.
        let (rt_key, rt_val) = ("rt".repeat(55), "RT".repeat(65));
        facts.return_type_map.insert(rt_key.clone(), rt_val.clone());

        let ia_class = "ic".repeat(12);
        let ia_attr = "ia".repeat(13);
        let ia_ty = "IT".repeat(14);
        facts
            .instance_attr_types
            .insert((ia_class.clone(), ia_attr.clone()), ia_ty.clone());

        let ip_key = "ip".repeat(16);
        let ip_param = "IP".repeat(17);
        facts
            .init_params
            .insert(ip_key.clone(), vec![ip_param.clone()]);

        let ap_class = "ac".repeat(18);
        let ap_attr = "aa".repeat(19);
        let ap_param = "AP".repeat(21);
        facts
            .attr_to_param
            .insert((ap_class.clone(), ap_attr.clone()), ap_param.clone());

        let after = facts.approx_heap_bytes();

        let inserted = def_key.len()
            + def_val.len()
            + binding.len()
            + row_key.len()
            + type_key.len()
            + type_val.len()
            + pend_key.len()
            + pend_val.len()
            + pf_key.len()
            + pf_a.len()
            + pf_b.len()
            + receiver.len()
            + method.len()
            + label.len()
            + rt_key.len()
            + rt_val.len()
            + ia_class.len()
            + ia_attr.len()
            + ia_ty.len()
            + ip_key.len()
            + ip_param.len()
            + ap_class.len()
            + ap_attr.len()
            + ap_param.len();

        assert!(
            after >= before + inserted,
            "approx_heap_bytes grew by only {} bytes after inserting {inserted} known string bytes",
            after - before,
        );
    }

    /// Stage 2: `shrink_to_fit` must reclaim genuine over-capacity
    /// slack (the shape every real push/insert-loop precompute path leaves
    /// behind — see the method's own doc comment) without changing any
    /// value, key, or length it touches. Deliberately builds each exercised
    /// collection with real slack (`with_capacity`/many-inserts-then-retain,
    /// never a lucky `capacity() == len()` accident) so a no-op
    /// implementation would fail the pre-condition asserts below, not just
    /// the post-condition ones.
    #[test]
    fn shrink_to_fit_reclaims_capacity_without_changing_values() {
        let mut facts = dummy_precomputed_facts_for_test("hello world");

        let mut defs = HashMap::default();
        for i in 0..64 {
            defs.insert(format!("k{i}"), format!("v{i}"));
        }
        defs.insert("kept".to_string(), "value".to_string());
        defs.retain(|k, _| k == "kept");

        let mut bindings = HashSet::default();
        for i in 0..64 {
            bindings.insert(format!("b{i}"));
        }
        bindings.insert("kept-binding".to_string());
        bindings.retain(|b| b == "kept-binding");

        facts.scopes.push(Scope {
            parent: None,
            defs,
            bindings,
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        });

        let mut labels = Vec::with_capacity(32);
        labels.push(Some("kept-label".to_string()));
        facts.ast_refs.push(AstRef {
            kind: AstRefKind::MethodCall {
                receiver: Arc::from("R"),
                method: Arc::from("M"),
                argument_labels: Some(labels),
            },
            row: 0,
            start_byte: 0,
            end_byte: 1,
        });

        let mut specifiers = Vec::with_capacity(32);
        specifiers.push(("Original".to_string(), "local".to_string()));
        facts.import_stmts.push(ImportStmtFacts::PyFromImport {
            module: "pkg.mod".to_string(),
            specifiers,
        });

        let mut arg_shapes = Vec::with_capacity(32);
        arg_shapes.push(Some("Inner".to_string()));
        facts.ctor_call_sites.push(CtorCallFacts {
            callee: "Outer".to_string(),
            arg_shapes,
        });

        // Pre-condition: every collection above really is over-capacity, or
        // this test would prove nothing about the trim.
        assert!(facts.scopes[0].defs.capacity() > facts.scopes[0].defs.len());
        assert!(facts.scopes[0].bindings.capacity() > facts.scopes[0].bindings.len());
        let labels_cap_len = match &facts.ast_refs[0].kind {
            AstRefKind::MethodCall {
                argument_labels: Some(l),
                ..
            } => (l.capacity(), l.len()),
            _ => unreachable!(),
        };
        assert!(labels_cap_len.0 > labels_cap_len.1);
        let specifiers_cap_len = match &facts.import_stmts[0] {
            ImportStmtFacts::PyFromImport { specifiers, .. } => {
                (specifiers.capacity(), specifiers.len())
            }
            _ => unreachable!(),
        };
        assert!(specifiers_cap_len.0 > specifiers_cap_len.1);
        assert!(
            facts.ctor_call_sites[0].arg_shapes.capacity()
                > facts.ctor_call_sites[0].arg_shapes.len()
        );
        let defs_cap_before = facts.scopes[0].defs.capacity();
        let bindings_cap_before = facts.scopes[0].bindings.capacity();

        let before_bytes = facts.approx_heap_bytes();

        facts.shrink_to_fit();

        // Post-condition, `HashMap`/`HashSet`: hashbrown's `shrink_to_fit`
        // sizes the table to the load factor, not to exact `len()`
        // equality (a 1-entry map can still report `capacity() == 3`), so
        // the meaningful assertion is "shrank from the deliberately
        // over-provisioned pre-shrink capacity," not exact equality.
        assert!(facts.scopes[0].defs.capacity() < defs_cap_before);
        assert!(facts.scopes[0].bindings.capacity() < bindings_cap_before);
        // Post-condition, `Vec`: capacity matches length exactly, everywhere
        // exercised above.
        match &facts.ast_refs[0].kind {
            AstRefKind::MethodCall {
                argument_labels: Some(l),
                ..
            } => assert_eq!(l.capacity(), l.len()),
            _ => unreachable!(),
        }
        match &facts.import_stmts[0] {
            ImportStmtFacts::PyFromImport { specifiers, .. } => {
                assert_eq!(specifiers.capacity(), specifiers.len())
            }
            _ => unreachable!(),
        }
        assert_eq!(
            facts.ctor_call_sites[0].arg_shapes.capacity(),
            facts.ctor_call_sites[0].arg_shapes.len()
        );

        // Values are untouched — shrink_to_fit is a pure capacity operation.
        assert_eq!(facts.scopes[0].defs.get("kept"), Some(&"value".to_string()));
        assert!(facts.scopes[0].bindings.contains("kept-binding"));
        match &facts.ast_refs[0].kind {
            AstRefKind::MethodCall {
                receiver,
                method,
                argument_labels: Some(l),
            } => {
                assert_eq!(receiver.as_ref(), "R");
                assert_eq!(method.as_ref(), "M");
                assert_eq!(l, &vec![Some("kept-label".to_string())]);
            }
            _ => unreachable!(),
        }
        match &facts.import_stmts[0] {
            ImportStmtFacts::PyFromImport { module, specifiers } => {
                assert_eq!(module, "pkg.mod");
                assert_eq!(
                    specifiers,
                    &vec![("Original".to_string(), "local".to_string())]
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(facts.ctor_call_sites[0].callee, "Outer");
        assert_eq!(
            facts.ctor_call_sites[0].arg_shapes,
            vec![Some("Inner".to_string())]
        );

        // The attributed byte estimate must actually drop: real slack
        // existed pre-shrink (the pre-condition asserts above), so a no-op
        // `shrink_to_fit` would leave `approx_heap_bytes` unchanged instead.
        let after_bytes = facts.approx_heap_bytes();
        assert!(
            after_bytes < before_bytes,
            "shrink_to_fit did not reduce the attributed estimate: before={before_bytes} after={after_bytes}"
        );
    }

    /// Honest-miss backstop: the Go package-qualified MethodCall fallback
    /// (`import_table_by_name.get(method)`) must fire only when this entity's
    /// scope-index lookup HIT. A hit landing on scope 0 legitimately (a
    /// genuine module-level entity) keeps the fallback; a MISS (both maps
    /// lacked the id, so `scope_idx` is just the `unwrap_or(0)` default)
    /// means the scope context is unknown and guessing a callee from the
    /// import table would manufacture a wrong edge — a missing edge is
    /// honest, a wrong edge is a lie.
    #[test]
    fn go_import_table_method_fallback_suppressed_only_on_scope_lookup_miss() {
        let scopes = vec![Scope {
            parent: None,
            defs: HashMap::default(),
            bindings: HashSet::default(),
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        }];
        let mut import_table_by_name = HashMap::default();
        import_table_by_name.insert("Format", "go-fmt-target");

        let ast_ref = AstRef {
            kind: AstRefKind::MethodCall {
                receiver: Arc::from("pkg"),
                method: Arc::from("Format"),
                argument_labels: None,
            },
            row: 0,
            start_byte: 0,
            end_byte: 1,
        };
        let file_lookup = FileEntityLookup::new(&[]);
        let empty_entity_map = HashMap::default();
        let symbol_table = HashMap::default();
        let class_members = HashMap::default();
        let owner_members = HashMap::default();
        let instance_attr_types = HashMap::default();
        let swift_call_signatures = HashMap::default();

        // Lookup HIT (false): every scope-chain branch misses against the
        // empty fixtures, so the import-table fallback resolves the method.
        let resolved_hit = resolve_ref(
            &ast_ref,
            0,
            false,
            &scopes,
            &symbol_table,
            &class_members,
            &owner_members,
            &import_table_by_name,
            &instance_attr_types,
            &empty_entity_map,
            &swift_call_signatures,
            "main.go",
            "caller-entity",
            false,
            false,
            &file_lookup,
            &mut ScopeLookupCache::default(),
            None,
            &mut Recorder::off(),
        );
        assert_eq!(
            resolved_hit,
            Some(("go-fmt-target".to_string(), RefType::Calls, "import"))
        );

        // Lookup MISS (true): identical inputs except the flag — the
        // fallback is suppressed and the ref honestly stays unresolved.
        let resolved_missed = resolve_ref(
            &ast_ref,
            0,
            true,
            &scopes,
            &symbol_table,
            &class_members,
            &owner_members,
            &import_table_by_name,
            &instance_attr_types,
            &empty_entity_map,
            &swift_call_signatures,
            "main.go",
            "caller-entity",
            false,
            false,
            &file_lookup,
            &mut ScopeLookupCache::default(),
            None,
            &mut Recorder::off(),
        );
        assert_eq!(resolved_missed, None);
    }

    /// `precompute_js_ts_file_facts` must seed
    /// `scopes[0].defs` in **`entity_ranges` order** — `(start_line, end_line,
    /// id)` ascending, id being the tiebreaker — the same order the AST path
    /// (`resolve_with_scopes_full_inner`'s `if let Some(ranges) =
    /// entity_ranges.get(...)` loop) uses. Both loops do
    /// `defs.insert(name, id)`, last-write-wins, so a seed-order mismatch is a
    /// silent divergence in which entity wins a same-named collision.
    ///
    /// For two same-named top-level entities the two orders always agree
    /// *unless* the tie reaches the `id` string comparison — which happens
    /// only when many same-named top-level siblings share both `start_line`
    /// *and* `end_line` (provably unreachable for 2-9 siblings: a DFS/preorder
    /// extractor is monotonic in byte position, so an earlier-extracted
    /// sibling on a shared start_line must end by that same line, forcing its
    /// `end_line` to be numerically <= a later sibling's — same order both
    /// ways). At 10+ siblings the id ordinal suffix (`#1`, `#2`, ... `#10`,
    /// `#11`) breaks that: **string** order is `#1 < #10 < #11 < #2 < ... <
    /// #9`, not numeric order, so an extraction-order (unsorted) seed and an
    /// `entity_ranges`-order (sorted) seed pick different last writes.
    ///
    /// This fixture — 11 one-line `function f(){}` declarations on a single
    /// source line, all `start_line == end_line == 1` — is exactly that case:
    /// before the fix, `precompute_js_ts_file_facts` (extraction order) seeded
    /// `#11` while `entity_ranges` order (computed here as the same-shape
    /// oracle `resolve_with_scopes_full_inner` uses) picks `#9`, a real,
    /// constructed divergence, not merely an argued one. This test pins the
    /// fix: precompute must now agree with `entity_ranges` order.
    #[test]
    fn js_ts_precompute_seed_order_matches_entity_ranges_order_at_ten_plus_siblings() {
        let registry = crate::parser::plugins::create_default_registry();
        let source: String = std::iter::repeat_n("function f(){} ", 11).collect();
        let (entities, tree) = registry
            .extract_entities_with_tree("over.ts", &source)
            .expect("extract");
        let tree = tree.expect("tree");

        let overloads: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.name == "f" && e.entity_type == "function")
            .collect();
        assert_eq!(
            overloads.len(),
            11,
            "expected 11 overloads, got: {entities:?}"
        );
        for e in &overloads {
            assert_eq!(e.start_line, 1);
            assert_eq!(e.end_line, 1);
        }
        // Extraction order is source order: #1..#11, in that order.
        let extraction_order_ids: Vec<&str> = overloads.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            extraction_order_ids,
            vec![
                "over.ts::function::f@L1#1",
                "over.ts::function::f@L1#2",
                "over.ts::function::f@L1#3",
                "over.ts::function::f@L1#4",
                "over.ts::function::f@L1#5",
                "over.ts::function::f@L1#6",
                "over.ts::function::f@L1#7",
                "over.ts::function::f@L1#8",
                "over.ts::function::f@L1#9",
                "over.ts::function::f@L1#10",
                "over.ts::function::f@L1#11",
            ]
        );

        // What `entity_ranges` order (the AST path) would pick: sort
        // `(start_line, end_line, id)` ascending and take the last write —
        // exactly `resolve_with_scopes_full_inner`'s seed loop, replicated
        // here as a same-shape oracle rather than re-run through the whole
        // corpus-wide plumbing.
        let mut ranges_order: Vec<&SemanticEntity> = overloads.clone();
        ranges_order.sort_by(|a, b| {
            (a.start_line, a.end_line, a.id.as_str()).cmp(&(
                b.start_line,
                b.end_line,
                b.id.as_str(),
            ))
        });
        let entity_ranges_last_write = ranges_order.last().expect("non-empty").id.clone();
        assert_eq!(
            entity_ranges_last_write, "over.ts::function::f@L1#9",
            "entity_ranges (id-string) order's last write should be the string-max id"
        );

        // What precompute actually seeds.
        let facts = precompute_js_ts_file_facts("over.ts", source.clone(), &tree, &entities)
            .expect("precomputed facts");
        let precompute_seeded_id = facts.scopes[0]
            .defs
            .get("f")
            .cloned()
            .expect("f seeded into module scope");

        // Post-fix guarantee: precompute must pick the same last-write
        // entity as entity_ranges order — the divergence this fixture was
        // constructed to expose (extraction order's #11 vs. entity_ranges
        // order's #9) must not reach `scopes[0].defs`.
        assert_eq!(
            precompute_seeded_id, entity_ranges_last_write,
            "precompute must seed scopes[0].defs[\"f\"] in entity_ranges \
             order, matching the AST path — got {precompute_seeded_id}, expected \
             {entity_ranges_last_write}"
        );
        assert_eq!(precompute_seeded_id, "over.ts::function::f@L1#9");
    }

    /// The CLEAN gate must fire on a
    /// cross-file parent link. A census found **zero** real
    /// violations across 4.8M entities on seven corpora (the theorem
    /// explains why: `build_entity_id` roots every id at its own file, so
    /// `children_by_parent[e] ⊆ entities(file(e))` holds unconditionally for
    /// anything the product's own extractors produce) — so the only way to
    /// exercise the gate at all is to construct the violation directly,
    /// bypassing extraction, exactly as this test does. The gate must be
    /// *seen* to fire, not merely argued sound by the theorem: a file whose
    /// own theorem-given soundness is bypassed like this is exactly the case
    /// this fail-safe exists for.
    #[test]
    fn clean_gate_marks_file_dirty_when_a_child_lives_in_another_file() {
        // "parent.cs" declares `Outer`, whose *own* extraction would only
        // ever produce children rooted in "parent.cs" (the theorem). Here a
        // second, independently-file-rooted entity in "intruder.cs" is
        // constructed to name `Outer` as its `parent_id` anyway — the one
        // shape CLEAN(F) forbids: `{ x : x.parent_id == e.id } ⊆
        // entities(file(e))` violated for `e` = Outer.
        let outer = mk_entity("parent.cs", "class", "Outer", None, 1, 10);
        let intruding_child = mk_entity(
            "intruder.cs",
            "method",
            "Sneaky",
            Some(outer.id.as_str()),
            1,
            2,
        );
        // A second, genuinely sound file must not be caught by the same
        // pass — no false positives: `sound.cs`'s own child names its own
        // file's entity as parent, entirely within `sound.cs`.
        let sound_parent = mk_entity("sound.cs", "class", "Fine", None, 1, 10);
        let sound_child = mk_entity(
            "sound.cs",
            "method",
            "Ok",
            Some(sound_parent.id.as_str()),
            2,
            3,
        );
        // A third file with no children at all must also not be flagged.
        let leaf = mk_entity("leaf.cs", "class", "Leaf", None, 1, 1);

        let mut all_entities = vec![
            outer.clone(),
            intruding_child,
            sound_parent,
            sound_child,
            leaf,
        ];
        // MUL phase 2: the real call site runs this before the gate
        // — a no-op here (no `.go` entities), matching production order.
        let go_parents_resolved =
            crate::parser::registry::resolve_go_method_parent_ids(&mut all_entities);

        // Every file in this fixture is "under adjudication" — the scoped
        // gate must reproduce the old corpus-wide verdict exactly when every
        // file is a candidate.
        let candidate_spans = spans_for(
            &all_entities,
            &["parent.cs", "intruder.cs", "sound.cs", "leaf.cs"],
        );
        let dirty = clean_gate_dirty_files(&all_entities, &candidate_spans, go_parents_resolved);

        assert!(
            dirty.contains("parent.cs"),
            "the file owning the entity whose child crossed a file boundary \
             must be marked dirty — got {dirty:?}"
        );
        assert_eq!(
            dirty.len(),
            1,
            "CLEAN must have no false positives: sound.cs and leaf.cs are \
             untouched by any cross-file link — got {dirty:?}"
        );
        assert!(!dirty.contains("sound.cs"));
        assert!(!dirty.contains("leaf.cs"));
        // "intruder.cs" itself is not the dirty party — the theorem's own
        // entities are still self-consistent (Sneaky's parent chain resolves
        // fine within intruder.cs's own worldview); it is `Outer`'s file
        // whose fast-path facts are unsound to serve, because *its*
        // file-local `children_by_parent[Outer.id]` would have been empty
        // while the corpus-wide one is not.
        assert!(!dirty.contains("intruder.cs"));
    }

    /// the scoping property itself — `candidate_files` narrower
    /// than the whole corpus must still reproduce the exact corpus-wide
    /// verdict for every file it *does* cover, in both directions. This is
    /// the soundness argument in `clean_gate_dirty_files`'s own doc comment,
    /// turned into a check: (a) a candidate whose cross-file child lives in a
    /// file that is *not itself a candidate* (`intruder.cs`, never passed in
    /// `candidate_files` below) must still be caught — a candidate's dirty
    /// status depends on who points at it from anywhere in the corpus, not
    /// on whether the pointer's own file is also under adjudication; (b) a
    /// candidate with no relationship to the violation elsewhere in the
    /// corpus must not be flagged, and files outside `candidate_files`
    /// entirely (`leaf.cs`) must never appear in the result even though they
    /// exist in `all_entities`.
    #[test]
    fn clean_gate_scoping_matches_corpus_wide_verdict_per_candidate() {
        let outer = mk_entity("parent.cs", "class", "Outer", None, 1, 10);
        let intruding_child = mk_entity(
            "intruder.cs",
            "method",
            "Sneaky",
            Some(outer.id.as_str()),
            1,
            2,
        );
        let sound_parent = mk_entity("sound.cs", "class", "Fine", None, 1, 10);
        let sound_child = mk_entity(
            "sound.cs",
            "method",
            "Ok",
            Some(sound_parent.id.as_str()),
            2,
            3,
        );
        let leaf = mk_entity("leaf.cs", "class", "Leaf", None, 1, 1);
        let mut all_entities = vec![outer, intruding_child, sound_parent, sound_child, leaf];
        let go_parents_resolved =
            crate::parser::registry::resolve_go_method_parent_ids(&mut all_entities);

        // Only "parent.cs" and "sound.cs" are under adjudication this build
        // ("intruder.cs" and "leaf.cs" were not precomputed and so are never
        // asked about) — the real `fresh_precomputed`-scoped shape.
        let candidate_spans = spans_for(&all_entities, &["parent.cs", "sound.cs"]);
        let dirty = clean_gate_dirty_files(&all_entities, &candidate_spans, go_parents_resolved);

        assert_eq!(
            dirty,
            ["parent.cs".to_string()]
                .into_iter()
                .collect::<HashSet<_>>(),
            "parent.cs must still be caught even though its cross-file child \
             lives in a file (intruder.cs) that is not itself a candidate, \
             and sound.cs — a candidate — must not be a false positive — \
             got {dirty:?}"
        );
    }

    /// end-to-end wiring proof, at the same grain
    /// `EntityGraph::build`'s pass-1 assembly uses — dropping a dirty file's
    /// entry from a `fresh_precomputed`-shaped map via `.retain()` (the exact
    /// operation `graph.rs`'s CLEAN gate performs) must remove only the dirty
    /// file's facts and leave every other file's facts untouched.
    #[test]
    fn clean_gate_drops_only_the_dirty_files_precomputed_facts() {
        let outer = mk_entity("parent.cs", "class", "Outer", None, 1, 10);
        let intruding_child = mk_entity(
            "intruder.cs",
            "method",
            "Sneaky",
            Some(outer.id.as_str()),
            1,
            2,
        );
        let sound_parent = mk_entity("sound.cs", "class", "Fine", None, 1, 10);

        let mut all_entities = vec![outer, intruding_child, sound_parent];
        let go_parents_resolved =
            crate::parser::registry::resolve_go_method_parent_ids(&mut all_entities);

        fn dummy_facts() -> PrecomputedFileFacts {
            dummy_precomputed_facts_for_test("")
        }

        let mut fresh_precomputed: HashMap<String, PrecomputedFileFacts> = HashMap::default();
        fresh_precomputed.insert("parent.cs".to_string(), dummy_facts());
        fresh_precomputed.insert("sound.cs".to_string(), dummy_facts());

        // The exact call `EntityGraph::build_incremental_core` performs
        // (graph.rs, MUL Phase 1 gate): candidates are `fresh_precomputed`'s
        // own keys, not the whole corpus ("intruder.cs" was never
        // precomputed and is not a candidate).
        let candidate_spans = spans_for(&all_entities, &["parent.cs", "sound.cs"]);
        let dirty = clean_gate_dirty_files(&all_entities, &candidate_spans, go_parents_resolved);
        assert_eq!(dirty.len(), 1);
        assert!(dirty.contains("parent.cs"));

        // The exact operation `EntityGraph::build_incremental_core` performs
        // (graph.rs, MUL Phase 1 gate) after computing `dirty`.
        fresh_precomputed.retain(|path, _| !dirty.contains(path.as_str()));

        assert!(
            !fresh_precomputed.contains_key("parent.cs"),
            "parent.cs failed CLEAN and must fall back to the re-parse path \
             — its fast-path facts must not survive"
        );
        assert!(
            fresh_precomputed.contains_key("sound.cs"),
            "sound.cs is CLEAN and must keep its fast-path facts — the gate must \
             never punish a sound file for an unrelated one failing the gate"
        );
    }

    /// MUL phase 2: pins the CLEAN gate's ordering dependency on
    /// `resolve_go_method_parent_ids` — the exact hazard recorded on the
    /// MUL change (`mul_precompute_admits` currently hardcodes `_ =>
    /// false` for every language but C++/C#, so `.go` never reaches
    /// `clean_gate_candidate_spans` today; this becomes live the moment
    /// phase 2 admits Go). There is no injectable seam that flips that
    /// verdict for Go end-to-end through `EntityGraph::build` —
    /// `mul_precompute_admits` is a plain match, not a predicate any test
    /// can override, unlike `csharp`'s runtime-gated arm (and even that one
    /// is a process-global `OnceLock`, unflippable mid-test — see
    /// `resolve_gated_salt_generalizes_beyond_csharp`'s own doc comment). So
    /// this test drives the two real functions directly, at the entity-
    /// content grain the gate actually operates on: it shows the gate is
    /// unsound against a pre-rewrite entity set (the exact bug a premature
    /// call site reintroduces) and sound against the post-rewrite one —
    /// what `EntityGraph::build` now always feeds it, structurally enforced
    /// by the `GoParentsResolved` token `clean_gate_dirty_files` requires.
    #[test]
    fn go_parent_repair_must_run_before_clean_gate_adjudication() {
        // `Hub` declared in hub.go; `Ping`'s receiver is `Hub`, but `Ping`
        // is declared in a *different* file of the same package — the one
        // shape `resolve_go_method_parent_ids` exists to repair.
        let mut package_meta = std::collections::BTreeMap::new();
        package_meta.insert("go.package".to_string(), "pkgA".to_string());

        let mut hub = mk_entity("pkgA/hub.go", "struct", "Hub", None, 1, 1);
        hub.metadata = Some(package_meta.clone());

        let mut ping = mk_entity("pkgA/ping.go", "method", "Ping", None, 1, 1);
        ping.metadata = Some(package_meta);
        ping.content = "func (h *Hub) Ping() string { return \"pong\" }".to_string();

        let mut all_entities = vec![hub.clone(), ping];
        let candidate_spans = spans_for(&all_entities, &["pkgA/hub.go", "pkgA/ping.go"]);

        // Pre-rewrite: `Ping.parent_id` is still unset — nothing has looked
        // at its receiver yet — so the gate finds no cross-file child of
        // `Hub` and (wrongly) calls hub.go CLEAN. The token is obtained from
        // rewriting an unrelated scratch copy: its rekey data describes that
        // scratch copy's own ids, not this test's `all_entities`, so as far
        // as `clean_gate_dirty_files` is concerned it still proves only "the
        // rewrite ran somewhere," never "on this exact slice" (see
        // `GoParentsResolved`'s own doc comment) — which is precisely why
        // `EntityGraph::build` is the only call site with a single
        // `all_entities` per build, and precisely the false negative a call
        // site that gated *before* rewriting its own `all_entities` would
        // reintroduce today undetected, because it is undetectable from the
        // token alone.
        let mut scratch_for_token = all_entities.clone();
        let stale_token =
            crate::parser::registry::resolve_go_method_parent_ids(&mut scratch_for_token);
        let dirty_before = clean_gate_dirty_files(&all_entities, &candidate_spans, stale_token);
        assert!(
            !dirty_before.contains("pkgA/hub.go"),
            "pre-rewrite entities hide the cross-file link — the gate must \
             (wrongly) call hub.go CLEAN here, which is exactly the false \
             negative the ordering hazard produces: got {dirty_before:?}"
        );

        // Post-rewrite, in place — the shape `EntityGraph::build` now
        // always produces: `Ping.parent_id` now names `Hub.id` from a
        // different file.
        let go_parents_resolved =
            crate::parser::registry::resolve_go_method_parent_ids(&mut all_entities);
        assert_eq!(
            all_entities[1].parent_id.as_deref(),
            Some(hub.id.as_str()),
            "sanity: the rewrite must actually have linked Ping to Hub"
        );
        let dirty_after =
            clean_gate_dirty_files(&all_entities, &candidate_spans, go_parents_resolved);
        assert!(
            dirty_after.contains("pkgA/hub.go"),
            "post-rewrite, the gate must catch the cross-file child and \
             fail hub.go CLEAN — got {dirty_after:?}"
        );
    }

    /// the id-staleness species itself, at the exact grain it
    /// bites — a Go method's `PrecomputedFileFacts` entry, built by pass 1
    /// against the pre-rewrite id, must still be reachable by the
    /// post-rewrite id pass 2 actually looks up. Same `Hub`/`Ping`
    /// cross-file receiver shape as
    /// `go_parent_repair_must_run_before_clean_gate_adjudication` above (the
    /// witness fixture this one is deliberately modeled on), but probing
    /// `entity_scope_map`/`entity_inner_scope`/`return_type_map` — the three
    /// id-keyed fields `resolve_ref`'s pass-2 lookup (`scope_resolve.rs`,
    /// `entity_inner_scope.get(&entity.id).or_else(|| entity_scope_map.get(&entity.id))`)
    /// and `deterministic_return_types_by_name` actually read — rather than
    /// the CLEAN gate's separate cross-file-child question.
    #[test]
    fn go_parent_rewrite_id_survives_precomputed_scope_lookup_after_rekey() {
        let mut package_meta = std::collections::BTreeMap::new();
        package_meta.insert("go.package".to_string(), "pkgA".to_string());

        let mut hub = mk_entity("pkgA/hub.go", "struct", "Hub", None, 1, 1);
        hub.metadata = Some(package_meta.clone());

        let mut ping = mk_entity("pkgA/ping.go", "method", "Ping", None, 1, 1);
        ping.metadata = Some(package_meta);
        ping.content = "func (h *Hub) Ping() string { return \"pong\" }".to_string();

        // The pre-rewrite id pass 1's precompute would have keyed this
        // file's facts against — `Ping.parent_id` is still unset here,
        // exactly as it is inside `precompute_scope_resolvable_file_facts`'s
        // per-file call, which runs before any file sees the others'.
        let pre_rewrite_id = ping.id.clone();

        let mut facts = dummy_precomputed_facts_for_test("");
        facts.entity_scope_map.insert(pre_rewrite_id.clone(), 0);
        facts.entity_inner_scope.insert(pre_rewrite_id.clone(), 0);
        facts
            .return_type_map
            .insert(pre_rewrite_id.clone(), "string".to_string());

        let mut all_entities = vec![hub.clone(), ping];
        let go_parents_resolved =
            crate::parser::registry::resolve_go_method_parent_ids(&mut all_entities);

        let post_rewrite_id = all_entities[1].id.clone();
        assert_ne!(
            pre_rewrite_id, post_rewrite_id,
            "sanity: the rewrite must actually change Ping's id when its \
             receiver type is found cross-file"
        );
        assert!(
            go_parents_resolved.rekeyed_files().contains("pkgA/ping.go"),
            "the rewrite must report ping.go as touched so a caller knows \
             which file's precomputed facts need re-keying"
        );

        // Pre-repair: this is the bug. Pass 2 would look Ping up by its new
        // id and find nothing in any of the three maps.
        assert!(facts.entity_scope_map.get(&post_rewrite_id).is_none());
        assert!(facts.entity_inner_scope.get(&post_rewrite_id).is_none());
        assert!(facts.return_type_map.get(&post_rewrite_id).is_none());

        facts.rekey_entity_ids(go_parents_resolved.rekeyed_ids());

        // Post-repair: the same precomputed entry, now reachable by the id
        // pass 2 will actually ask for, and the stale key is gone rather
        // than left behind as a dangling duplicate.
        assert_eq!(
            facts.entity_scope_map.get(&post_rewrite_id),
            Some(&0),
            "entity_scope_map must carry Ping's scope entry under its \
             rewritten id"
        );
        assert_eq!(
            facts.entity_inner_scope.get(&post_rewrite_id),
            Some(&0),
            "entity_inner_scope must carry Ping's scope entry under its \
             rewritten id"
        );
        assert_eq!(
            facts.return_type_map.get(&post_rewrite_id),
            Some(&"string".to_string()),
            "return_type_map must carry Ping's return type under its \
             rewritten id"
        );
        assert!(
            facts.entity_scope_map.get(&pre_rewrite_id).is_none(),
            "the stale pre-rewrite key must not survive re-keying as a \
             dangling duplicate"
        );
        assert!(facts.entity_inner_scope.get(&pre_rewrite_id).is_none());
        assert!(facts.return_type_map.get(&pre_rewrite_id).is_none());
    }

    /// `TREELESS(F)` is decided from what the
    /// fused walk actually saw, not a per-language table. Originally
    /// (before Field 10/11 existed) a Python file with a real
    /// `import` statement failed the gate outright — Python had no pass-2
    /// consumer for either descriptor kind yet. MUL phase 2
    /// (`mul_precompute_consumes_imports`/`mul_precompute_consumes_calls`)
    /// gave it both, so this now pins the *current* verdict: an import-only
    /// Python file (no `"call"` node) gets fast-path facts, with
    /// `import_stmts` populated and `ctor_call_sites` empty — the Field-11
    /// analog of `precompute_scope_resolvable_file_facts_some_for_rust_with_imports`.
    #[test]
    fn precompute_scope_resolvable_file_facts_some_for_python_with_imports_only() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "import os\n\nclass Foo:\n    pass\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("has_import.py", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts = precompute_scope_resolvable_file_facts(
            "has_import.py",
            source.to_string(),
            &tree,
            &entities,
        );
        let facts = facts.expect(
            "a Python file with an import and no call must get fast-path \
             facts now that Field 10 gives pass 2 a tree-free consumer for \
             its import_stmts",
        );
        assert_eq!(facts.import_stmts.len(), 1);
        assert!(
            facts.ctor_call_sites.is_empty(),
            "no \"call\" node in this fixture, so nothing should be recorded"
        );
    }

    /// the positive control — a Python file with **no** import and
    /// **no** `"call"` node (`TREELESS` per) does get fast-path facts.
    #[test]
    fn precompute_scope_resolvable_file_facts_some_when_treeless() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "class Foo:\n    x = 1\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("treeless.py", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts = precompute_scope_resolvable_file_facts(
            "treeless.py",
            source.to_string(),
            &tree,
            &entities,
        );
        assert!(
            facts.is_some(),
            "an import-less, call-less Python file is TREELESS and must get \
             fast-path facts, exactly like the census's HA __init__.py stubs"
        );
    }

    /// /: Swift is out of scope in every phase —
    /// `build_swift_call_signatures` is corpus-wide, not per-file, so no
    /// Swift file may ever take this fast path regardless of what its own
    /// tree looks like.
    #[test]
    fn precompute_scope_resolvable_file_facts_none_for_swift() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "class Foo {\n    var x: Int = 1\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("plain.swift", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts = precompute_scope_resolvable_file_facts(
            "plain.swift",
            source.to_string(),
            &tree,
            &entities,
        );
        assert!(
            facts.is_none(),
            "Swift must never take the fast path, even for a trivial file \
             the walk would otherwise call TREELESS"
        );
    }

    /// a representative C# file — the family this change's per-file
    /// gate targets — gets fast-path facts. Matches.3's
    /// census finding that C# is 100% TREELESS by bytes (no node kind
    /// `classify_import_stmt` handles fires for `using` directives, and C#'s
    /// call-expression node kind is `invocation_expression`, never the
    /// literal `"call"` ctor-infer hardcodes to Python).
    #[test]
    fn precompute_scope_resolvable_file_facts_some_for_csharp() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "using System;\n\nnamespace N {\n    public class Foo {\n        public int Bar() { return DoWork(); }\n    }\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("Foo.cs", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts =
            precompute_scope_resolvable_file_facts("Foo.cs", source.to_string(), &tree, &entities);
        assert!(
            facts.is_some(),
            "a C# file with a using directive and a method call must still \
             be TREELESS — neither classify_import_stmt nor the literal \
             \"call\" kind fire for C#'s grammar"
        );
    }

    /// the shipped default must match the shipped verdict.
    /// The "MUL P1" section and its memory-lever
    /// follow-up both close on "**dotnet stays GATED**" (+21.2%/+32.9%
    /// against a +15% ceiling, both pairs) for C#. C++ was originally "GO,
    /// unconditionally" (+5.8%/+6.5%) but the corrected-protocol,
    /// corrected-metric re-verification (2026-08-22) found both fields bust
    /// the ceiling on llvm-project (maxRSS +19.98-21.02%, footprint
    /// +26.33-28.11%) — see `mul_precompute_admits`'s doc comment — so C++
    /// is gated too now, the same shape as C#. A default that contradicts
    /// the measurement is a correctness-of-record bug even when every
    /// answer it produces is right.
    #[test]
    fn mul_phase1_default_matches_the_measured_verdict() {
        assert!(
            !mul_precompute_admits("cpp"),
            "C++'s 2026-08-22 re-verification (corrected protocol + corrected \
             footprint metric) measured +19.98-21.02% maxRSS / \
             +26.33-28.11% footprint against the +15% ceiling on \
             llvm-project — above it on both fields, unlike the \
             original +5.8%/+6.5% reading — so it must be opt-in \
             (SEM_MUL_CPP=1) until a memory fix lands"
        );
        assert!(
            !mul_precompute_admits("csharp"),
            "C# exceeded the +15% memory ceiling on both measured dotnet pairs; \
             it must be opt-in (SEM_MUL_CSHARP=1) until a memory fix lands"
        );
        // Phase 1 was originally exactly two families (C#, C++); both are
        // now gated. Everything else — java/rust/python are all gated
        // off-by-default via SEM_MUL_JAVA/SEM_MUL_RUST/SEM_MUL_PYTHON,
        // tested below; go is unconditional, tested below too —
        // and JS/TS, which has its own unconditional precompute on a
        // different branch — must not reach this producer with the env
        // switches unset.
        for lang in ["java", "c", "typescript", "javascript", "swift", "ruby"] {
            assert!(
                !mul_precompute_admits(lang),
                "{lang} is not in MUL phase 1's GO/NO-GO table"
            );
        }
    }

    /// MUL Phase 2 follow-up: rust was admitted unconditionally
    /// at (phase-2 section, +11.16%/+11.28%
    /// against the +15% ceiling), but a same-binary re-verification at
    /// campaign HEAD found the delta re-measures at +17.72%/+19.64%/
    /// +19.35% — above the ceiling, three order-swapped pairs, unanimous
    /// direction. Demoted to opt-in (`SEM_MUL_RUST=1`), gated like C#/Java,
    /// not unconditional like phase 1's C++ or phase 2's Python.
    #[test]
    fn mul_phase2_rust_default_matches_the_measured_verdict() {
        assert!(
            !mul_precompute_admits("rust"),
            "rust's same-binary peak-RSS re-verification measured \
             +17.72%/+19.64%/+19.35% against the +15% ceiling — above it, \
             unlike the original +11.16%/+11.28% reading — so it must be \
             opt-in (SEM_MUL_RUST=1) until a memory fix lands"
        );
    }

    /// the Go memory-check work: Go's correctness blocker chain closed
    /// (/this change's own `rekey_entity_ids`
    /// fix — `edge_dump_probe` ON vs OFF bit-identical on kubernetes,
    /// 331,117 edges both sides) and its memory check cleared (+6.78% to
    /// +8.46% peak footprint, three order-swapped pairs on kubernetes,
    /// under the +15% ceiling; maxRSS flat). Admitted unconditionally —
    /// same shape as phase 1's C++/phase 2's Python at their own admission
    /// time, both since demoted; a default that contradicts the measured
    /// verdict is a correctness-of-record bug even when every answer it
    /// produces is right.
    #[test]
    fn mul_phase2_go_default_matches_the_measured_verdict() {
        assert!(
            mul_precompute_admits("go"),
            "go's correctness blocker chain is closed and its memory check \
             (+6.78% to +8.46% peak footprint, three order-swapped pairs on \
             kubernetes) clears the +15% ceiling on both fields — it must be \
             unconditional, not gated"
        );
    }

    /// MUL: a Rust file
    /// with a real `use` import must now get fast-path facts. This producer
    /// does not itself consult `mul_precompute_admits`/`SEM_MUL_RUST` (that
    /// gate is `graph.rs`'s call-site job) — it only decides TREELESS, and
    /// Field 10 gives Rust's import handling a tree-free consumer
    /// (`mul_precompute_consumes_imports`), so a non-empty `import_starts`
    /// no longer fails the gate for this one language.
    #[test]
    fn precompute_scope_resolvable_file_facts_some_for_rust_with_imports() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "use std::collections::HashMap;\n\nstruct Foo {\n    x: i32,\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("foo.rs", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts =
            precompute_scope_resolvable_file_facts("foo.rs", source.to_string(), &tree, &entities);
        let facts = facts.expect(
            "a Rust file with a `use` import must get fast-path facts now \
             that Field 10 gives pass 2 a tree-free consumer for its \
             import_stmts",
        );
        assert_eq!(
            facts.import_stmts.len(),
            1,
            "the recorded import_stmts must actually carry the \
             use-declaration descriptor, not an empty stub"
        );
    }

    /// MUL: the shipped default must match the
    /// shipped verdict, same discipline as `mul_phase1_default_matches_the_
    /// measured_verdict`. Java: correctness is clean (bit-identical
    /// edge_dump_probe, full oracle battery) but it busted its own +15%
    /// peak-RSS ceiling on elasticsearch (+20.97%/+21.01%, both pairs) — off
    /// by default, C#'s shape. Go's own default is covered separately by
    /// `mul_phase2_go_default_matches_the_measured_verdict` (: now
    /// unconditional, correctness chain closed, memory check cleared) — this
    /// test used to assert Go's gated state too, back when `edge_dump_probe`
    /// was not yet bit-identical ON vs OFF on kubernetes.
    #[test]
    fn mul_phase2_java_default_matches_the_measured_verdict() {
        assert!(
            !mul_precompute_admits("java"),
            "java must be opt-in (SEM_MUL_JAVA=1) — it exceeded the +15% \
             memory ceiling on both measured elasticsearch pairs \
             (+20.97%/+21.01%), same pattern as C#"
        );
    }

    /// MUL: a Go file
    /// with a real multi-spec `import (...)` block must now get fast-path
    /// facts — mirrors `precompute_scope_resolvable_file_facts_some_for_rust_with_imports`.
    /// This producer does not itself consult `mul_precompute_admits`
    /// (that gate is `graph.rs`'s call-site job) — it only
    /// decides TREELESS, and `mul_precompute_consumes_imports` is unconditional
    /// (a pure function of `lang_id`, not env-gated), so this is testable
    /// without touching the process-global admission switch.
    #[test]
    fn precompute_scope_resolvable_file_facts_some_for_go_with_imports() {
        let registry = crate::parser::plugins::create_default_registry();
        let source =
            "package demo\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\ntype Foo struct {\n\tX int\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("foo.go", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts =
            precompute_scope_resolvable_file_facts("foo.go", source.to_string(), &tree, &entities);
        let facts = facts.expect(
            "a Go file with a multi-spec import block must get fast-path \
             facts now that Field 10 gives pass 2 a tree-free consumer for \
             its import_stmts",
        );
        assert_eq!(
            facts.import_stmts.len(),
            1,
            "one import_declaration node produces one descriptor, carrying \
             both package specs (GoImport::packages), not one per spec"
        );
        match &facts.import_stmts[0] {
            ImportStmtFacts::GoImport { packages } => {
                assert_eq!(
                    packages,
                    &vec!["fmt".to_string(), "os".to_string()],
                    "both packages in the multi-spec block must be recorded, in order"
                );
            }
            other => panic!("expected GoImport, got {other:?}"),
        }
    }

    /// A Go file with a function call (`call_expression`, not the literal
    /// `"call"` Python's grammar uses) must still be TREELESS — same
    /// pinning as Rust's call-node test.
    #[test]
    fn precompute_scope_resolvable_file_facts_go_call_expression_stays_treeless() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "package demo\n\nfunc bar() {}\n\nfunc foo() {\n\tbar()\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("call.go", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts =
            precompute_scope_resolvable_file_facts("call.go", source.to_string(), &tree, &entities);
        assert!(
            facts.is_some(),
            "Go's call-expression node kind is not the literal \"call\" \
             ctor-infer hardcodes to Python's grammar, so a Go file with a \
             function call must still be TREELESS"
        );
    }

    /// MUL Phase 2: a Java file with a real `import` statement must now get
    /// fast-path facts too. Java's `import_declaration` nodes classify as
    /// `ImportStmtKind::GoImport` (shared grammar kind, `classify_import_stmt`'s
    /// doc comment) and dispatch through `register_go_package_imports`,
    /// which only ever matches `.go`-suffixed entities — a documented,
    /// pre-existing no-op for Java that this test pins by asserting the
    /// descriptor is recorded (Field 10 fired) while the resulting
    /// `import_table` stays empty (the no-op is preserved, not silently
    /// turned into new resolution behavior by this admission).
    #[test]
    fn precompute_scope_resolvable_file_facts_some_for_java_with_imports() {
        let registry = crate::parser::plugins::create_default_registry();
        let source =
            "import java.util.List;\nimport java.util.ArrayList;\n\nclass Foo {\n    int x;\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("Foo.java", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts = precompute_scope_resolvable_file_facts(
            "Foo.java",
            source.to_string(),
            &tree,
            &entities,
        );
        let facts = facts.expect(
            "a Java file with import statements must get fast-path facts \
             now that Field 10 gives pass 2 a tree-free consumer for its \
             import_stmts (via the shared GoImport descriptor kind)",
        );
        assert_eq!(
            facts.import_stmts.len(),
            2,
            "one descriptor per import_declaration node"
        );
        for stmt in &facts.import_stmts {
            assert!(
                matches!(stmt, ImportStmtFacts::GoImport { .. }),
                "Java imports must classify as GoImport (shared grammar \
                 kind), not a Java-specific variant: {stmt:?}"
            );
        }
    }

    /// A Java file with a method call (`method_invocation`, not the literal
    /// `"call"` Python's grammar uses) must still be TREELESS.
    #[test]
    fn precompute_scope_resolvable_file_facts_java_method_invocation_stays_treeless() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "class Foo {\n    void bar() {}\n    void baz() {\n        bar();\n    }\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("Call.java", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts = precompute_scope_resolvable_file_facts(
            "Call.java",
            source.to_string(),
            &tree,
            &entities,
        );
        assert!(
            facts.is_some(),
            "Java's method_invocation node kind is not the literal \"call\" \
             ctor-infer hardcodes to Python's grammar, so a Java file with \
             a method call must still be TREELESS"
        );
    }

    /// The call-node half of TREELESS is untouched by Field 10: Rust's
    /// grammar names calls `call_expression`, not the literal `"call"`
    /// Python's grammar uses, so a Rust file with a function call must
    /// still be TREELESS — pinning that `mul_precompute_consumes_imports`
    /// widens only the import half of the gate, not the call half
    /// (`mul_precompute_consumes_calls`, Field 11, Python-only).
    #[test]
    fn precompute_scope_resolvable_file_facts_rust_call_expression_stays_treeless() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "fn bar() {}\nfn foo() {\n    bar();\n}\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("call.rs", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts =
            precompute_scope_resolvable_file_facts("call.rs", source.to_string(), &tree, &entities);
        assert!(
            facts.is_some(),
            "Rust's call-expression node kind is not the literal \"call\" \
             ctor-infer hardcodes to Python's grammar, so a Rust file with \
             a function call must still be TREELESS"
        );
    }

    /// 2026-08-22 follow-up: python was admitted unconditionally
    /// (maxRSS -7.95%/-7.80%) and reconfirmed by an earlier re-check (maxRSS -1.63% median,
    /// weaker but still negative) — both readings comfortably under the old
    /// +15% maxRSS ceiling. The 2026-08-22 re-verification re-ran that exact protocol capturing *both*
    /// `/usr/bin/time -l` fields: maxRSS again reads negative
    /// (-1.04%/-3.99%/-1.71%), but peak memory footprint — the metric the
    /// ceiling is now measured against — reads +26.02%/+25.29%/+27.44%, above
    /// the ceiling, three order-swapped pairs, unanimous direction. Demoted
    /// to opt-in (`SEM_MUL_PYTHON=1`), gated like C#/Java/Rust/C++, not
    /// unconditional like its own original verdict.
    #[test]
    fn mul_phase2_python_default_matches_the_measured_verdict() {
        assert!(
            !mul_precompute_admits("python"),
            "python's maxRSS reading stayed negative under the 2026-08-22 re-verification \
             (-1.04%/-3.99%/-1.71%), but peak memory footprint — the corrected \
             metric — measured +26.02%/+25.29%/+27.44% against the +15% \
             ceiling on home-assistant/core, above it on all three \
             order-swapped pairs, so it must be opt-in (SEM_MUL_PYTHON=1) \
             until a memory fix lands"
        );
    }

    /// MUL: a Python
    /// file with both a real import statement *and* a real `"call"` node
    /// must now get fast-path facts — mirrors the Rust/Go/Java "some_for_..."
    /// tests, but Python needs both `mul_precompute_consumes_imports` *and*
    /// `mul_precompute_consumes_calls` to admit it, unlike the other three
    /// (import-only).
    #[test]
    fn precompute_scope_resolvable_file_facts_some_for_python_with_imports_and_calls() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "import os\n\n\nclass Foo:\n    def __init__(self):\n        self.x = Bar()\n";
        let (entities, tree) = registry
            .extract_entities_with_tree("foo.py", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let facts =
            precompute_scope_resolvable_file_facts("foo.py", source.to_string(), &tree, &entities);
        let facts = facts.expect(
            "a Python file with an import and a call must get fast-path \
             facts now that Field 10 and Field 11 both give pass 2 a \
             tree-free consumer",
        );
        assert_eq!(
            facts.import_stmts.len(),
            1,
            "the recorded import_stmts must carry the import descriptor"
        );
        assert_eq!(
            facts.ctor_call_sites.len(),
            1,
            "the recorded ctor_call_sites must carry the Bar() call descriptor"
        );
        assert_eq!(facts.ctor_call_sites[0].callee, "Bar");
    }

    /// The positive control's mirror: a Python file with a `"call"` node but
    /// **no** consumer admitted (simulated by asserting the raw walk output,
    /// since `mul_precompute_consumes_calls` is a pure function of `lang_id`
    /// and cannot be flipped per-test) records exactly one descriptor per
    /// qualifying call — lowercase callees are excluded at record time,
    /// matching the old `scan_constructor_calls`'s own pre-filter.
    #[test]
    fn record_ctor_call_sites_filters_lowercase_callees_and_non_identifier_functions() {
        let registry = crate::parser::plugins::create_default_registry();
        let source = "class Foo:\n    def __init__(self):\n        self.a = Bar(get_conn())\n        self.b = helper()\n        self.c = obj.Method()\n";
        let (_entities, tree) = registry
            .extract_entities_with_tree("mixed.py", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let descriptors = record_ctor_call_sites(tree.root_node(), source.as_bytes());
        // Only `Bar(get_conn())` qualifies: `helper()` is lowercase (not a
        // ctor call at all, so never even reaches the worklist's `if` body)
        // and `obj.Method()`'s function field is `attribute`, not a bare
        // `identifier`.
        assert_eq!(
            descriptors.len(),
            1,
            "expected exactly one ctor-call descriptor, got {descriptors:?}"
        );
        assert_eq!(descriptors[0].callee, "Bar");
        assert_eq!(
            descriptors[0].arg_shapes,
            vec![Some("get_conn".to_string())],
            "the sole argument is itself a call to a bare identifier, so its \
             shape must record that callee name uninterpreted"
        );
    }

    /// (MUL phase 2): record-vs-direct
    /// equivalence witness for the ctor-call scan, mirroring
    /// `record_then_dispatch_matches_dispatch_direct`'s proof for Field 10.
    ///
    /// this used to compare against a *fresh test-local
    /// transcription* of `scan_constructor_calls`/`infer_expr_type`, written
    /// once by the same author who wrote the refactor it was meant to check
    /// — a self-agreement risk (a transcription error shared between spec
    /// and implementation would pass silently). The functions immediately
    /// below (`frozen_pre_w5_scan_constructor_calls`/
    /// `frozen_pre_w5_infer_expr_type`) are instead extracted verbatim from
    /// git history — `git show 9c80258^:crates/sem-core/src/parser/
    /// scope_resolve.rs`, the commit immediately before deleted the
    /// original direct-dispatch scan — with only mechanical renames to avoid
    /// colliding with `record_ctor_call_sites`/`apply_ctor_call_facts`'s own
    /// names. This makes the "direct" side genuinely independent evidence: a
    /// real historical implementation, never touched by the refactor under
    /// test, not a paraphrase written to match it.
    #[test]
    fn record_then_apply_matches_direct_scan_for_ctor_calls() {
        // --- frozen prior originals from commit 9c80258^ (parent of "MUL
        // phase-2: build Field 11 (ctor_call_sites)"), verbatim except for
        // the `frozen_pre_w5_` name prefix. Do not edit to "improve" or
        // "modernize" — any behavioral drift here defeats the point of this
        // test. If real behavior needs to change, change the production
        // functions and let this test go RED as the signal. Clippy style
        // suggestions (map_or -> is_some_and, explicit counter -> enumerate)
        // are suppressed for the same reason -- "modernizing" the style would
        // stop being the verbatim original this test exists to check against. ---
        #[allow(clippy::unnecessary_map_or, clippy::explicit_counter_loop)]
        fn frozen_pre_w5_scan_constructor_calls(
            root: tree_sitter::Node,
            source: &[u8],
            func_name_returns: &HashMap<String, String>,
            init_params: &HashMap<String, Vec<String>>,
            attr_to_param_index: &AttrToParamIndex<'_>,
            instance_attr_types: &mut HashMap<(String, String), String>,
        ) {
            let mut worklist = vec![root];
            while let Some(node) = worklist.pop() {
                let kind = node.kind();

                if kind == "call" {
                    if let Some(func) = node.child_by_field_name("function") {
                        if func.kind() == "identifier" {
                            let class_name = func.utf8_text(source).unwrap_or("");
                            // Only process uppercase names (constructor calls)
                            if class_name
                                .chars()
                                .next()
                                .map_or(false, |c| c.is_uppercase())
                            {
                                if let Some(param_names) = init_params.get(class_name) {
                                    // Extract argument types
                                    if let Some(args_node) = node.child_by_field_name("arguments") {
                                        let mut arg_idx = 0;
                                        let mut args_cursor = args_node.walk();
                                        for arg in args_node.named_children(&mut args_cursor) {
                                            if arg_idx >= param_names.len() {
                                                break;
                                            }
                                            let param_name = &param_names[arg_idx];

                                            // Try to infer the argument's type
                                            let arg_type = frozen_pre_w5_infer_expr_type(
                                                arg,
                                                source,
                                                func_name_returns,
                                            );

                                            if let Some(at) = arg_type {
                                                if let Some(attrs) = attr_to_param_index
                                                    .get(&(class_name, param_name.as_str()))
                                                {
                                                    for (cn, attr) in attrs {
                                                        instance_attr_types
                                                            .entry((
                                                                (*cn).to_string(),
                                                                (*attr).to_string(),
                                                            ))
                                                            .or_insert_with(|| at.clone());
                                                    }
                                                }
                                            }

                                            arg_idx += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                push_named_children_rev(&mut worklist, node);
            }
        }

        /// Infer the type of an expression node.
        #[allow(clippy::unnecessary_map_or)]
        fn frozen_pre_w5_infer_expr_type(
            node: tree_sitter::Node,
            source: &[u8],
            func_name_returns: &HashMap<String, String>,
        ) -> Option<String> {
            match node.kind() {
                "call" => {
                    if let Some(func) = node.child_by_field_name("function") {
                        if func.kind() == "identifier" {
                            let name = func.utf8_text(source).unwrap_or("");
                            // Constructor call: Foo() -> type is Foo
                            if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                                return Some(name.to_string());
                            }
                            // Function call with known return type
                            if let Some(ret) = func_name_returns.get(name) {
                                return Some(ret.clone());
                            }
                        }
                    }
                    None
                }
                "identifier" => {
                    // Could be a variable, but we don't have scope info here
                    None
                }
                _ => None,
            }
        }
        // --- end frozen prior originals ---

        let registry = crate::parser::plugins::create_default_registry();
        let source = "class Connection:\n    def __init__(self):\n        pass\n\n\nclass Transaction:\n    def __init__(self, conn):\n        self.conn = conn\n\n\ndef get_connection():\n    return Connection()\n\n\nt = Transaction(get_connection())\n";
        let (_entities, tree) = registry
            .extract_entities_with_tree("txn.py", source)
            .expect("extract");
        let tree = tree.expect("tree");
        let source_bytes = source.as_bytes();

        let mut func_name_returns: HashMap<String, String> = HashMap::default();
        func_name_returns.insert("get_connection".to_string(), "Connection".to_string());
        let mut init_params: HashMap<String, Vec<String>> = HashMap::default();
        init_params.insert("Transaction".to_string(), vec!["conn".to_string()]);
        let mut attr_to_param: HashMap<(String, String), String> = HashMap::default();
        attr_to_param.insert(
            ("Transaction".to_string(), "conn".to_string()),
            "conn".to_string(),
        );
        let attr_to_param_index = build_attr_to_param_index(&attr_to_param);

        let mut direct_result: HashMap<(String, String), String> = HashMap::default();
        frozen_pre_w5_scan_constructor_calls(
            tree.root_node(),
            source_bytes,
            &func_name_returns,
            &init_params,
            &attr_to_param_index,
            &mut direct_result,
        );
        assert_eq!(
            direct_result.get(&("Transaction".to_string(), "conn".to_string())),
            Some(&"Connection".to_string()),
            "non-vacuity: the fixture must actually exercise the resolution path"
        );

        let descriptors = record_ctor_call_sites(tree.root_node(), source_bytes);
        let mut record_apply_result: HashMap<(String, String), String> = HashMap::default();
        apply_ctor_call_facts(
            &descriptors,
            &func_name_returns,
            &init_params,
            &attr_to_param_index,
            &mut record_apply_result,
        );

        assert_eq!(
            record_apply_result, direct_result,
            "record_ctor_call_sites + apply_ctor_call_facts must land on the \
             same instance_attr_types as the frozen prior direct tree walk \
             (commit 9c80258^) -- genuinely independent evidence, not a \
             self-transcribed spec"
        );
    }

    #[test]
    fn resolution_cache_key_includes_resolution_context() {
        let ast_ref = AstRef {
            kind: AstRefKind::Call {
                name: Arc::from("load"),
                argument_labels: Some(vec![Some("id".to_string())]),
            },
            row: 0,
            start_byte: 0,
            end_byte: 4,
        };

        let base = resolution_cache_key(&ast_ref, 1, "entity_a", true, false);

        assert_ne!(
            base,
            resolution_cache_key(&ast_ref, 2, "entity_a", true, false)
        );
        assert_ne!(
            base,
            resolution_cache_key(&ast_ref, 1, "entity_b", true, false)
        );
        assert_ne!(
            base,
            resolution_cache_key(&ast_ref, 1, "entity_a", false, false)
        );

        let method_ref = AstRef {
            kind: AstRefKind::MethodCall {
                receiver: Arc::from("client"),
                method: Arc::from("load"),
                argument_labels: None,
            },
            row: 0,
            start_byte: 0,
            end_byte: 11,
        };

        assert_ne!(
            resolution_cache_key(&method_ref, 1, "entity_a", true, false),
            resolution_cache_key(&method_ref, 1, "entity_a", false, false)
        );
        assert_ne!(
            resolution_cache_key(&method_ref, 1, "entity_a", true, false),
            resolution_cache_key(&method_ref, 1, "entity_a", true, true)
        );

        let prefixed_method_ref = AstRef {
            kind: AstRefKind::MethodCall {
                receiver: Arc::from("!client"),
                method: Arc::from("load"),
                argument_labels: None,
            },
            row: 0,
            start_byte: 0,
            end_byte: 12,
        };

        assert_eq!(
            resolution_cache_key(&method_ref, 1, "entity_a", true, false),
            resolution_cache_key(&prefixed_method_ref, 1, "entity_a", true, false)
        );
    }

    #[test]
    fn return_type_name_lookup_uses_symbol_table_order() {
        let mut return_type_map = HashMap::default();
        return_type_map.insert(
            "z_backup.py::function::make_conn".to_string(),
            "Backup".to_string(),
        );
        return_type_map.insert(
            "a_primary.py::function::make_conn".to_string(),
            "Primary".to_string(),
        );

        let mut symbol_table = HashMap::default();
        symbol_table.insert(
            "make_conn".to_string(),
            vec![
                "a_primary.py::function::make_conn".to_string(),
                "z_backup.py::function::make_conn".to_string(),
            ],
        );

        // `deterministic_return_types_by_name` reaches a name through
        // `entity_map`, so both candidates must be present here for the test to
        // exercise the tie-break it is about.
        let mut entity_map: HashMap<String, EntityInfo> = HashMap::default();
        for (id, file) in [
            ("z_backup.py::function::make_conn", "z_backup.py"),
            ("a_primary.py::function::make_conn", "a_primary.py"),
        ] {
            entity_map.insert(
                id.to_string(),
                EntityInfo {
                    id: id.to_string(),
                    name: "make_conn".to_string(),
                    entity_type: "function".to_string(),
                    file_path: file.to_string(),
                    parent_id: None,
                    start_line: 1,
                    end_line: 1,
                },
            );
        }

        let by_name =
            deterministic_return_types_by_name(&return_type_map, &symbol_table, &entity_map);

        assert_eq!(
            by_name.get("make_conn").map(String::as_str),
            Some("Primary")
        );
    }

    #[test]
    fn go_package_index_entries_are_sorted() {
        let first_id = "pkg/foo/a.go::function::zeta".to_string();
        let second_id = "pkg/foo/b.go::function::alpha".to_string();

        let mut symbol_table = HashMap::default();
        symbol_table.insert("zeta".to_string(), vec![first_id.clone()]);
        symbol_table.insert("alpha".to_string(), vec![second_id.clone()]);

        let mut entity_map = HashMap::default();
        entity_map.insert(
            first_id.clone(),
            EntityInfo {
                id: first_id.clone(),
                name: "zeta".to_string(),
                entity_type: "function".to_string(),
                file_path: "pkg/foo/a.go".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );
        entity_map.insert(
            second_id.clone(),
            EntityInfo {
                id: second_id.clone(),
                name: "alpha".to_string(),
                entity_type: "function".to_string(),
                file_path: "pkg/foo/b.go".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );

        let index = build_go_pkg_index(&symbol_table, &entity_map);

        assert_eq!(
            index.get("foo"),
            Some(&vec![
                ("alpha".to_string(), second_id, "pkg/foo".to_string()),
                ("zeta".to_string(), first_id, "pkg/foo".to_string()),
            ])
        );
    }

    /// Filter semantics of the shared `build_go_pkg_index`, which graph.rs's
    /// cold build path delegates to (unification + the deferred
    /// route deletion): a Go file contributes to the index via its
    /// DIRECTORY name only — Go import paths name packages, which are
    /// directories, never files, so a bucket keyed on a file's own
    /// stripped-of-`.go` stem is not a legitimate Go resolution key.
    /// (An earlier revision of this function *did* key on the file stem
    /// too; kubernetes has real source files literally named after Go
    /// stdlib packages — `os.go`, `time.go` — and with no signal to tell
    /// "this bucket entry is the stdlib package" from "this bucket entry is
    /// a corpus-local file sharing its bare name," that route mis-resolved
    /// e.g. `time.Now()` calls to a same-named local file's own `Now`, a
    /// ~5k-edge false-positive class caught in verification and removed.)
    /// This fixture pins: (1) a `.go` file's own stripped stem creates NO
    /// bucket of its own, only its parent directory does; (2) the concrete
    /// stdlib-collision shape (a file literally named `os.go`) creates no
    /// `"os"` bucket; (3) a non-.go twin still contributes nothing at all,
    /// anywhere.
    #[test]
    fn go_package_index_keys_by_directory_not_file_stem() {
        let go_id = "pkg/foo/zeta.go::function::Run".to_string();
        let py_twin_id = "pkg/foo/zeta.py::function::Run".to_string();
        let py_util_id = "pkg/bar/helpers.py::function::UtilFn".to_string();
        // The concrete kubernetes shape that exposed the file-stem route's
        // false positive: a corpus file literally named after a Go stdlib
        // package (`pkg/kubelet/container/os.go`, minimized here).
        let stdlib_shadow_id = "pkg/quux/os.go::function::Stat".to_string();

        let mut symbol_table = HashMap::default();
        symbol_table.insert(
            "Run".to_string(),
            vec![go_id.clone(), py_twin_id.clone()],
        );
        symbol_table.insert("UtilFn".to_string(), vec![py_util_id.clone()]);
        symbol_table.insert("Stat".to_string(), vec![stdlib_shadow_id.clone()]);

        let mut entity_map = HashMap::default();
        entity_map.insert(
            go_id.clone(),
            EntityInfo {
                id: go_id.clone(),
                name: "Run".to_string(),
                entity_type: "function".to_string(),
                file_path: "pkg/foo/zeta.go".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );
        entity_map.insert(
            py_twin_id.clone(),
            EntityInfo {
                id: py_twin_id.clone(),
                name: "Run".to_string(),
                entity_type: "function".to_string(),
                file_path: "pkg/foo/zeta.py".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );
        entity_map.insert(
            py_util_id.clone(),
            EntityInfo {
                id: py_util_id.clone(),
                name: "UtilFn".to_string(),
                entity_type: "function".to_string(),
                file_path: "pkg/bar/helpers.py".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );
        entity_map.insert(
            stdlib_shadow_id.clone(),
            EntityInfo {
                id: stdlib_shadow_id.clone(),
                name: "Stat".to_string(),
                entity_type: "function".to_string(),
                file_path: "pkg/quux/os.go".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );

        let index = build_go_pkg_index(&symbol_table, &entity_map);

        // The `.go` file's own stem ("zeta") creates no bucket at all —
        // only its directory ("foo") does.
        assert!(!index.contains_key("zeta"));
        assert_eq!(
            index.get("foo"),
            Some(&vec![("Run".to_string(), go_id.clone(), "pkg/foo".to_string())])
        );

        // The stdlib-name-colliding file creates no "os" bucket — the exact
        // false positive this fixture pins closed. Its entry is reachable
        // only via its own directory, "quux".
        assert!(!index.contains_key("os"));
        assert_eq!(
            index.get("quux"),
            Some(&vec![(
                "Stat".to_string(),
                stdlib_shadow_id.clone(),
                "pkg/quux".to_string()
            )])
        );

        // The `.py` files contribute nothing at all — not under their own
        // stem, not under their directory, not into any other bucket.
        assert!(!index.contains_key("helpers"));
        assert!(!index.contains_key("bar"));
        for entries in index.values() {
            for (_name, id, _dir) in entries {
                assert_ne!(id, &py_twin_id);
                assert_ne!(id, &py_util_id);
            }
        }
    }

    /// the collision at the root of kubernetes's 30,801-line
    /// `edge_dump_probe` divergence, reproduced as a minimal fixture. Two
    /// distinct Go packages, both declared in a directory literally named
    /// `v1` (kubernetes has dozens — one per API group), each with its own
    /// `DeepCopyInto` method. Bare-last-segment bucketing (the pre-fix
    /// behavior) merges both into `go_pkg_index["v1"]`, and inserting the
    /// whole bucket into a file's `import_table` picks whichever entry
    /// sorts last — the exact mechanism that resolved kubeadm's
    /// `DeepCopyInto` call to `pod-security-admission`'s method instead of
    /// its own.
    #[test]
    fn go_package_index_collision_is_real_before_disambiguation() {
        let kubeadm_id =
            "cmd/kubeadm/app/apis/kubeadm/v1/types.go::method::DeepCopyInto".to_string();
        let podsec_id =
            "staging/src/k8s.io/pod-security-admission/admission/api/v1/types.go::method::DeepCopyInto"
                .to_string();

        let mut symbol_table = HashMap::default();
        symbol_table.insert(
            "DeepCopyInto".to_string(),
            vec![kubeadm_id.clone(), podsec_id.clone()],
        );

        let mut entity_map = HashMap::default();
        entity_map.insert(
            kubeadm_id.clone(),
            EntityInfo {
                id: kubeadm_id.clone(),
                name: "DeepCopyInto".to_string(),
                entity_type: "method".to_string(),
                file_path: "cmd/kubeadm/app/apis/kubeadm/v1/types.go".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );
        entity_map.insert(
            podsec_id.clone(),
            EntityInfo {
                id: podsec_id.clone(),
                name: "DeepCopyInto".to_string(),
                entity_type: "method".to_string(),
                file_path: "staging/src/k8s.io/pod-security-admission/admission/api/v1/types.go"
                    .to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );

        let index = build_go_pkg_index(&symbol_table, &entity_map);
        let bucket = index
            .get("v1")
            .expect("both packages share the bare \"v1\" bucket — the collision is real");
        assert_eq!(
            bucket.len(),
            2,
            "the index itself does not distinguish the two \"v1\" packages \
             by bare name alone — disambiguation happens downstream, in \
             register_go_package_imports, using each entry's own declaring \
             directory (the third tuple element)"
        );
        let dirs: std::collections::BTreeSet<&str> =
            bucket.iter().map(|(_, _, dir)| dir.as_str()).collect();
        assert_eq!(
            dirs,
            std::collections::BTreeSet::from([
                "cmd/kubeadm/app/apis/kubeadm/v1",
                "staging/src/k8s.io/pod-security-admission/admission/api/v1",
            ]),
            "each entry must carry its own declaring directory, distinct \
             from the other package's — this is what makes disambiguation \
             possible at all"
        );
    }

    /// `register_go_package_imports` must resolve a
    /// package-qualified call to the *importing file's own* package, never
    /// a same-named package elsewhere in the repo — the fix for the
    /// collision the test above proves exists in the raw index. A file that
    /// imports kubeadm's `v1` (full import path, not just "v1") must get
    /// kubeadm's `DeepCopyInto` in its `import_table`, never
    /// pod-security-admission's — the exact substitution kubernetes's
    /// `edge_dump_probe` diff caught (30,801 lines,
    /// Go-admission finding).
    #[test]
    fn register_go_package_imports_resolves_the_file_own_import_not_a_same_named_collision() {
        let kubeadm_id =
            "cmd/kubeadm/app/apis/kubeadm/v1/types.go::method::DeepCopyInto".to_string();
        let podsec_id =
            "staging/src/k8s.io/pod-security-admission/admission/api/v1/types.go::method::DeepCopyInto"
                .to_string();

        let mut symbol_table = HashMap::default();
        symbol_table.insert(
            "DeepCopyInto".to_string(),
            vec![kubeadm_id.clone(), podsec_id.clone()],
        );

        let mut entity_map = HashMap::default();
        entity_map.insert(
            kubeadm_id.clone(),
            EntityInfo {
                id: kubeadm_id.clone(),
                name: "DeepCopyInto".to_string(),
                entity_type: "method".to_string(),
                file_path: "cmd/kubeadm/app/apis/kubeadm/v1/types.go".to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );
        entity_map.insert(
            podsec_id.clone(),
            EntityInfo {
                id: podsec_id.clone(),
                name: "DeepCopyInto".to_string(),
                entity_type: "method".to_string(),
                file_path: "staging/src/k8s.io/pod-security-admission/admission/api/v1/types.go"
                    .to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );

        let index = build_go_pkg_index(&symbol_table, &entity_map);

        let mut import_table: HashMap<(String, String), String> = HashMap::default();
        let mut scopes: Vec<Scope> = Vec::new();
        let mut rec = Recorder::off();
        register_go_package_imports(
            "k8s.io/kubernetes/cmd/kubeadm/app/apis/kubeadm/v1",
            "cmd/kubeadm/app/apis/kubeadm/types.go",
            &mut import_table,
            &mut scopes,
            &index,
            &mut rec,
        );

        assert_eq!(
            import_table.get(&(
                "cmd/kubeadm/app/apis/kubeadm/types.go".to_string(),
                "DeepCopyInto".to_string()
            )),
            Some(&kubeadm_id),
            "a file importing kubeadm's v1 must resolve DeepCopyInto to \
             kubeadm's own method, never pod-security-admission's — before \
             the fix this landed on whichever of the two sorted last"
        );

        // The mirror import: a *different* file importing
        // pod-security-admission's v1 must resolve to *its* DeepCopyInto —
        // proving this is real per-file disambiguation, not the collision
        // just happening to sort kubeadm first in this fixture.
        let mut import_table_2: HashMap<(String, String), String> = HashMap::default();
        let mut scopes_2: Vec<Scope> = Vec::new();
        let mut rec_2 = Recorder::off();
        register_go_package_imports(
            "k8s.io/pod-security-admission/admission/api/v1",
            "staging/src/k8s.io/pod-security-admission/admission/api/other.go",
            &mut import_table_2,
            &mut scopes_2,
            &index,
            &mut rec_2,
        );
        assert_eq!(
            import_table_2.get(&(
                "staging/src/k8s.io/pod-security-admission/admission/api/other.go".to_string(),
                "DeepCopyInto".to_string()
            )),
            Some(&podsec_id),
            "the mirror file, importing pod-security-admission's v1, must \
             resolve to pod-security-admission's own DeepCopyInto"
        );
    }

    // ------------------------------------------------------------------
    // The fusion-witness: the triple-walk fusion invariant.
    //
    //   ∀ file.  fused(tree) ≡ ( build_scopes_from_ast(tree);
    //                            collect_all_file_refs(tree);
    //                            seed;
    //                            extract_imports_from_ast(tree) )
    //
    // The unfused side is the three production walks executed in the pass-2
    // closure's exact phase order — kept alive as the specification, so this
    // test cannot degrade into "the new code agrees with itself". The per-node
    // bodies are shared helpers (`scope_visit_node`, `refs_visit_node`,
    // `dispatch_import_stmt`), so what this invariant actually witnesses is the part
    // the fusion changes: the traversal drivers — one walk instead of three,
    // and the recorded-set/pruned-replay reconstruction of extract's
    // non-document handling order.
    //
    // Generation: deterministic xorshift (single_pass_invariants.rs discipline; no
    // proptest dependency), composing programs in five language families that
    // take the AST path (Python, C#, Rust, Go, TS) from nestable fragments —
    // imports inside try/except and function bodies, classes with methods,
    // calls, assignments — with synthetic symbol/entity tables sized so the
    // import handlers really resolve.
    //
    // NON-VACUITY (asserted below): the sampled space produces >1 scope, ≥1
    // ref and ≥1 resolved import per family battery, and the Python battery
    // contains the same-alias-two-targets nested-import pair on which handler
    // order decides the winner.
    //
    // POSITIVE CONTROL (asserted below): a deliberately document-order
    // variant of import processing disagrees with the specification on that
    // pair — the replay order is load-bearing, not decorative.
    // ------------------------------------------------------------------

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
            if n == 0 {
                return 0;
            }
            (self.next() % n as u64) as usize
        }
        fn chance(&mut self, one_in: usize) -> bool {
            self.below(one_in) == 0
        }
    }

    struct WalkFixture {
        file_path: &'static str,
        ext: &'static str,
        source: String,
        entities: Vec<SemanticEntity>,
        symbol_table: HashMap<String, Vec<String>>,
        entity_map: HashMap<String, EntityInfo>,
    }

    fn mk_entity(
        file: &str,
        ty: &str,
        name: &str,
        parent: Option<&str>,
        start: usize,
        end: usize,
    ) -> SemanticEntity {
        let id = match parent {
            Some(p) => format!("{p}::{name}"),
            None => format!("{file}::{ty}::{name}"),
        };
        SemanticEntity {
            id,
            file_path: file.to_string(),
            entity_type: ty.to_string(),
            name: name.to_string(),
            parent_id: parent.map(str::to_string),
            content: String::new(),
            content_hash: String::new(),
            structural_hash: None,
            kappa: None,
            start_line: start,
            end_line: end,
            start_byte: None,
            end_byte: None,
            metadata: None,
        }
    }

    /// Test-only stand-in for the `(path, start, len)` spans
    /// `graph.rs`'s pass-1 assembly loop captures for free while building
    /// `all_entities` — computed here by scanning, since these fixtures are
    /// hand-built rather than assembled file-by-file, but expressing the same
    /// "this file's entities are the contiguous run `[start, start+len)`"
    /// contract `clean_gate_dirty_files` relies on.
    fn spans_for(all_entities: &[SemanticEntity], files: &[&str]) -> Vec<(String, usize, usize)> {
        files
            .iter()
            .map(|file| {
                let start = all_entities
                    .iter()
                    .position(|e| e.file_path == *file)
                    .expect("fixture must declare at least one entity for this file");
                let len = all_entities.iter().filter(|e| e.file_path == *file).count();
                (file.to_string(), start, len)
            })
            .collect()
    }

    fn register_target(
        fx_symbols: &mut HashMap<String, Vec<String>>,
        fx_entity_map: &mut HashMap<String, EntityInfo>,
        file: &str,
        name: &str,
    ) -> String {
        let id = format!("{file}::function::{name}");
        fx_symbols
            .entry(name.to_string())
            .or_default()
            .push(id.clone());
        fx_entity_map.insert(
            id.clone(),
            EntityInfo {
                id: id.clone(),
                name: name.to_string(),
                entity_type: "function".to_string(),
                file_path: file.to_string(),
                parent_id: None,
                start_line: 1,
                end_line: 3,
            },
        );
        id
    }

    /// Everything the three walks observably produce, captured after the
    /// phase sequence completes.
    struct WalkOutcome {
        scopes: Vec<Scope>,
        entity_scope_map: HashMap<String, usize>,
        entity_inner_scope: HashMap<String, usize>,
        ast_refs: Vec<AstRef>,
        import_table: HashMap<(String, String), String>,
    }

    enum ImportMode {
        /// Dispatch-direct: `extract_imports_from_ast`'s full recursive-via-
        /// worklist traversal, building one `ImportStmtFacts` descriptor per
        /// handled node and resolving it immediately (no batching).
        SpecSequential,
        /// Record-then-dispatch (MUL
        /// phase 2): the pruned-subtree traversal's own composition,
        /// `record_import_stmts_pruned` (collect every descriptor into a
        /// `Vec`, in traversal order) then `dispatch_import_stmts_from_facts`
        /// (resolve the whole `Vec`, after the traversal has finished) — the
        /// same two calls pass 2's one production caller makes, inline, with
        /// no wrapper function in between. Comparing this against
        /// `SpecSequential` below is this refactor's own equivalence
        /// witness: same descriptor type, same `dispatch_import_stmt`, two
        /// different traversal/batching strategies around it.
        FusedReplay,
        /// The deliberately wrong variant for the positive control: handlers
        /// fired in document order instead of extract's LIFO order.
        BrokenDocOrder,
    }

    fn run_walks(fx: &WalkFixture, mode: ImportMode) -> WalkOutcome {
        let config = scope_resolve_config_for_path(fx.file_path)
            .expect("test family must have a scope-resolve config");
        let lang = crate::parser::plugins::code::languages::get_language_config(fx.ext)
            .expect("language config");
        let tree =
            crate::parser::plugins::code::parse_tree(lang, &fx.source).expect("fixture parses");
        let source = fx.source.as_bytes();

        let file_entities: Vec<&SemanticEntity> = fx.entities.iter().collect();
        let file_lookup = FileEntityLookup::new(&file_entities);
        let mut children_by_parent: HashMap<&str, Vec<&SemanticEntity>> = HashMap::default();
        let mut entity_map = fx.entity_map.clone();
        for e in &fx.entities {
            entity_map.insert(
                e.id.clone(),
                EntityInfo {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    entity_type: e.entity_type.clone(),
                    file_path: e.file_path.clone(),
                    parent_id: e.parent_id.clone(),
                    start_line: e.start_line,
                    end_line: e.end_line,
                },
            );
            if let Some(ref pid) = e.parent_id {
                children_by_parent.entry(pid.as_str()).or_default().push(e);
            }
        }

        let mut scopes: Vec<Scope> = vec![Scope {
            parent: None,
            defs: HashMap::default(),
            bindings: HashSet::default(),
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        }];
        let mut entity_scope_map: HashMap<String, usize> = HashMap::default();
        let mut entity_inner_scope: HashMap<String, usize> = HashMap::default();
        // The closure's own top-level seed, replicated.
        for e in &fx.entities {
            if e.parent_id.is_none() {
                scopes[0].defs.insert(e.name.clone(), e.id.clone());
                entity_scope_map.insert(e.id.clone(), 0);
            }
        }

        let (ast_refs, fused_starts): (Vec<AstRef>, Option<Vec<usize>>) = match mode {
            ImportMode::FusedReplay => {
                let (refs, starts, _saw_call_node) = fused_scope_refs_import_walk(
                    tree.root_node(),
                    0,
                    &mut scopes,
                    &mut entity_scope_map,
                    &mut entity_inner_scope,
                    &file_lookup,
                    &children_by_parent,
                    &entity_map,
                    source,
                    config,
                );
                (refs, Some(starts))
            }
            ImportMode::SpecSequential | ImportMode::BrokenDocOrder => {
                build_scopes_from_ast(
                    tree.root_node(),
                    0,
                    &mut scopes,
                    &mut entity_scope_map,
                    &mut entity_inner_scope,
                    &file_lookup,
                    &children_by_parent,
                    &entity_map,
                    source,
                    config,
                );
                let refs = collect_all_file_refs(tree.root_node(), source, config);
                (refs, None)
            }
        };

        // Phase order preserved: the import handlers run after the walk(s),
        // exactly where the pass-2 closure runs them.
        let mut import_table: HashMap<(String, String), String> = HashMap::default();
        let go_pkg_index = build_go_pkg_index(&fx.symbol_table, &entity_map);
        let ts_default_exports = TsDefaultExportTable {
            exports_by_file: HashMap::default(),
            sorted_files: Vec::new(),
        };
        let top_level_entities = OnceLock::new();
        let py_top_level_entities = OnceLock::new();
        let rust_top_level_entities = OnceLock::new();
        let parsed_files: &[(String, String, tree_sitter::Tree)] = &[];
        let content_by_file = OnceLock::new();
        let exported_names_by_file: Mutex<HashMap<String, Arc<HashSet<String>>>> =
            Mutex::new(HashMap::default());
        let mut rec = Recorder::off();

        match mode {
            ImportMode::SpecSequential => {
                extract_imports_from_ast(
                    tree.root_node(),
                    fx.file_path,
                    source,
                    &fx.symbol_table,
                    &entity_map,
                    &mut import_table,
                    &mut scopes,
                    config,
                    &go_pkg_index,
                    &ts_default_exports,
                    &top_level_entities,
                    &py_top_level_entities,
                    &rust_top_level_entities,
                    parsed_files,
                    &content_by_file,
                    &exported_names_by_file,
                    false,
                    &mut rec,
                );
            }
            ImportMode::FusedReplay => {
                let starts = fused_starts.expect("fused mode records starts");
                if !starts.is_empty() {
                    let descriptors = record_import_stmts_pruned(
                        tree.root_node(),
                        &starts,
                        source,
                        config,
                        false,
                    );
                    dispatch_import_stmts_from_facts(
                        &descriptors,
                        fx.file_path,
                        &fx.symbol_table,
                        &entity_map,
                        &mut import_table,
                        &mut scopes,
                        &go_pkg_index,
                        &ts_default_exports,
                        &top_level_entities,
                        &py_top_level_entities,
                        &rust_top_level_entities,
                        parsed_files,
                        &content_by_file,
                        &exported_names_by_file,
                        false,
                        &mut rec,
                    );
                }
            }
            ImportMode::BrokenDocOrder => {
                // Document-order handler firing: collect H in pre-order, then
                // dispatch forward. Everything else identical.
                let mut handled: Vec<tree_sitter::Node> = Vec::new();
                let mut worklist: Vec<(tree_sitter::Node, bool)> = vec![(tree.root_node(), false)];
                while let Some((node, in_import)) = worklist.pop() {
                    let is_import =
                        !in_import && classify_import_stmt(node.kind(), config).is_some();
                    if is_import {
                        handled.push(node);
                    }
                    let start = worklist.len();
                    let mut cursor = node.walk();
                    worklist.extend(
                        node.named_children(&mut cursor)
                            .map(|c| (c, in_import || is_import)),
                    );
                    worklist[start..].reverse();
                }
                for node in handled {
                    if let Some(stmt) = classify_import_stmt(node.kind(), config) {
                        let descriptor = build_import_stmt_facts(stmt, node, source, false);
                        dispatch_import_stmt(
                            &descriptor,
                            fx.file_path,
                            &fx.symbol_table,
                            &entity_map,
                            &mut import_table,
                            &mut scopes,
                            &go_pkg_index,
                            &ts_default_exports,
                            &top_level_entities,
                            &py_top_level_entities,
                            &rust_top_level_entities,
                            parsed_files,
                            &content_by_file,
                            &exported_names_by_file,
                            false,
                            &mut rec,
                        );
                    }
                }
            }
        }

        WalkOutcome {
            scopes,
            entity_scope_map,
            entity_inner_scope,
            ast_refs,
            import_table,
        }
    }

    fn assert_outcomes_equal(spec: &WalkOutcome, fused: &WalkOutcome, label: &str) {
        assert_eq!(
            spec.scopes, fused.scopes,
            "fusion-witness: scopes diverged on {label}"
        );
        assert_eq!(
            spec.entity_scope_map, fused.entity_scope_map,
            "fusion-witness: entity_scope_map diverged on {label}"
        );
        assert_eq!(
            spec.entity_inner_scope, fused.entity_inner_scope,
            "fusion-witness: entity_inner_scope diverged on {label}"
        );
        assert_eq!(
            spec.ast_refs, fused.ast_refs,
            "fusion-witness: ast_refs (incl. order) diverged on {label}"
        );
        assert_eq!(
            spec.import_table, fused.import_table,
            "fusion-witness: import_table diverged on {label}"
        );
    }

    // ---- fixture generators, one per family --------------------------------

    fn gen_python(g: &mut Gen, with_order_pair: bool) -> WalkFixture {
        let file = "gen_fixture.py";
        let mut symbols = HashMap::default();
        let mut emap = HashMap::default();
        for m in 0..3 {
            for n in 0..3 {
                register_target(
                    &mut symbols,
                    &mut emap,
                    &format!("mod{m}.py"),
                    &format!("name{m}_{n}"),
                );
            }
        }
        register_target(&mut symbols, &mut emap, "mod0.py", "shared");
        register_target(&mut symbols, &mut emap, "mod1.py", "shared");

        let mut lines: Vec<String> = Vec::new();
        let mut entities: Vec<SemanticEntity> = Vec::new();

        for _ in 0..g.below(3) {
            let m = g.below(3);
            let n = g.below(3);
            if g.chance(2) {
                lines.push(format!("from mod{m} import name{m}_{n}"));
            } else {
                lines.push(format!("from mod{m} import name{m}_{n} as al{m}_{n}"));
            }
        }
        if g.chance(3) {
            lines.push(format!("import mod{}", g.below(3)));
        }
        if with_order_pair || g.chance(2) {
            // The order-sensitivity witness: same alias, two targets, nested
            // in sibling containers. Extract handles the except-branch first,
            // so mod0's binding lands last and wins.
            lines.push("try:".to_string());
            lines.push("    from mod0 import shared as S".to_string());
            lines.push("except ImportError:".to_string());
            lines.push("    from mod1 import shared as S".to_string());
        }

        let n_classes = 1 + g.below(2);
        for c in 0..n_classes {
            let cname = format!("Klass{c}");
            let class_start = lines.len() + 1;
            lines.push(format!("class {cname}:"));
            let cid = format!("{file}::class::{cname}");
            let n_methods = 1 + g.below(2);
            for m in 0..n_methods {
                let mname = format!("meth{c}_{m}");
                let meth_start = lines.len() + 1;
                lines.push(format!("    def {mname}(self, arg: Klass0):"));
                lines.push(format!("        x = name{}_{}()", g.below(3), g.below(3)));
                lines.push("        x.helper()".to_string());
                if g.chance(3) {
                    lines.push(format!(
                        "        from mod{} import name{}_0",
                        g.below(3),
                        g.below(3)
                    ));
                }
                lines.push(format!("        return meth{c}_0()"));
                let meth_end = lines.len();
                entities.push(mk_entity(
                    file,
                    "method",
                    &mname,
                    Some(&cid),
                    meth_start,
                    meth_end,
                ));
            }
            let class_end = lines.len();
            entities.insert(
                entities.len() - n_methods,
                mk_entity(file, "class", &cname, None, class_start, class_end),
            );
        }
        let fn_start = lines.len() + 1;
        lines.push("def top_fn(a, b):".to_string());
        lines.push("    v = Klass0()".to_string());
        lines.push("    v.meth0_0()".to_string());
        lines.push("    return top_fn(a, b)".to_string());
        entities.push(mk_entity(
            file,
            "function",
            "top_fn",
            None,
            fn_start,
            lines.len(),
        ));

        WalkFixture {
            file_path: file,
            ext: ".py",
            source: lines.join("\n") + "\n",
            entities,
            symbol_table: symbols,
            entity_map: emap,
        }
    }

    fn gen_csharp(g: &mut Gen) -> WalkFixture {
        let file = "gen_fixture.cs";
        let mut lines: Vec<String> = Vec::new();
        let mut entities: Vec<SemanticEntity> = Vec::new();
        lines.push("using System;".to_string());
        lines.push("using System.Collections.Generic;".to_string());
        lines.push("namespace Gen {".to_string());
        let n_classes = 1 + g.below(2);
        for c in 0..n_classes {
            let cname = format!("Widget{c}");
            let class_start = lines.len() + 1;
            lines.push(format!("  public class {cname} {{"));
            let cid = format!("{file}::class::{cname}");
            let n_methods = 1 + g.below(3);
            for m in 0..n_methods {
                let mname = format!("Run{c}_{m}");
                let meth_start = lines.len() + 1;
                lines.push(format!("    public int {mname}(int k) {{"));
                lines.push(format!("      var w = new Widget{}();", g.below(n_classes)));
                lines.push(format!("      w.Run{}_0(k);", g.below(n_classes)));
                lines.push(format!("      return Helper{}(k);", g.below(3)));
                lines.push("    }".to_string());
                let meth_end = lines.len();
                entities.push(mk_entity(
                    file,
                    "method",
                    &mname,
                    Some(&cid),
                    meth_start,
                    meth_end,
                ));
            }
            lines.push("  }".to_string());
            let class_end = lines.len();
            entities.insert(
                entities.len() - n_methods,
                mk_entity(file, "class", &cname, None, class_start, class_end),
            );
        }
        lines.push("}".to_string());

        WalkFixture {
            file_path: file,
            ext: ".cs",
            source: lines.join("\n") + "\n",
            entities,
            symbol_table: HashMap::default(),
            entity_map: HashMap::default(),
        }
    }

    fn gen_rust(g: &mut Gen) -> WalkFixture {
        let file = "gen_fixture.rs";
        let mut symbols = HashMap::default();
        let mut emap = HashMap::default();
        register_target(&mut symbols, &mut emap, "helpers.rs", "helper_a");
        register_target(&mut symbols, &mut emap, "helpers.rs", "helper_b");

        let mut lines: Vec<String> = Vec::new();
        let mut entities: Vec<SemanticEntity> = Vec::new();
        lines.push("use crate::helpers::helper_a;".to_string());
        if g.chance(2) {
            lines.push("mod inner {".to_string());
            lines.push("    use crate::helpers::helper_b;".to_string());
            lines.push("    fn nested() { helper_b(); }".to_string());
            lines.push("}".to_string());
        }
        let s_start = lines.len() + 1;
        lines.push("struct Thing;".to_string());
        entities.push(mk_entity(file, "struct", "Thing", None, s_start, s_start));
        let impl_start = lines.len() + 1;
        lines.push("impl Thing {".to_string());
        let iid = format!("{file}::impl::Thing");
        let m_start = lines.len() + 1;
        lines.push("    fn go(&self) {".to_string());
        lines.push("        let t = Thing;".to_string());
        lines.push("        helper_a();".to_string());
        lines.push("        Thing::go2();".to_string());
        lines.push("        println!(\"x\");".to_string());
        lines.push("    }".to_string());
        let m_end = lines.len();
        lines.push("}".to_string());
        let impl_end = lines.len();
        entities.push(mk_entity(file, "impl", "Thing", None, impl_start, impl_end));
        entities.push(mk_entity(file, "method", "go", Some(&iid), m_start, m_end));
        for f in 0..1 + g.below(2) {
            let f_start = lines.len() + 1;
            lines.push(format!("fn free{f}() {{"));
            lines.push(format!("    free{}();", g.below(2)));
            lines.push("}".to_string());
            entities.push(mk_entity(
                file,
                "function",
                &format!("free{f}"),
                None,
                f_start,
                lines.len(),
            ));
        }

        WalkFixture {
            file_path: file,
            ext: ".rs",
            source: lines.join("\n") + "\n",
            entities,
            symbol_table: symbols,
            entity_map: emap,
        }
    }

    fn gen_go(g: &mut Gen) -> WalkFixture {
        let file = "gen_fixture.go";
        let mut symbols = HashMap::default();
        let mut emap = HashMap::default();
        register_target(&mut symbols, &mut emap, "pkg/util/util.go", "DoWork");

        let mut lines: Vec<String> = Vec::new();
        let mut entities: Vec<SemanticEntity> = Vec::new();
        lines.push("package gen".to_string());
        lines.push("import (".to_string());
        lines.push("\t\"fmt\"".to_string());
        lines.push("\tu \"example.com/pkg/util\"".to_string());
        lines.push(")".to_string());
        let s_start = lines.len() + 1;
        lines.push("type Box struct { N int }".to_string());
        entities.push(mk_entity(file, "struct", "Box", None, s_start, s_start));
        for m in 0..1 + g.below(2) {
            let m_start = lines.len() + 1;
            lines.push(format!("func (b *Box) Fill{m}() {{"));
            lines.push("\tv := Box{N: 1}".to_string());
            lines.push("\tfmt.Println(v)".to_string());
            lines.push("\tu.DoWork()".to_string());
            lines.push(format!("\tb.Fill{}()", g.below(2)));
            lines.push("}".to_string());
            entities.push(mk_entity(
                file,
                "method",
                &format!("Fill{m}"),
                None,
                m_start,
                lines.len(),
            ));
        }

        WalkFixture {
            file_path: file,
            ext: ".go",
            source: lines.join("\n") + "\n",
            entities,
            symbol_table: symbols,
            entity_map: emap,
        }
    }

    /// MUL: Java's import statements are the
    /// documented no-op (`import_declaration` classifies as
    /// `GoImport`, which only ever resolves against `.go`-suffixed
    /// entities). This fixture still carries two real imports so the walk
    /// records descriptors for them (Field 10 must fire even though
    /// dispatch resolves nothing), plus a same-class method call so the
    /// non-import halves of the walk (scopes/refs) are exercised the same
    /// way `gen_csharp`'s nested-class shape is.
    fn gen_java(g: &mut Gen) -> WalkFixture {
        let file = "GenFixture.java";
        let mut lines: Vec<String> = Vec::new();
        let mut entities: Vec<SemanticEntity> = Vec::new();
        lines.push("import java.util.List;".to_string());
        if g.chance(2) {
            lines.push("import java.util.ArrayList;".to_string());
        }
        let class_start = lines.len() + 1;
        lines.push("class Box {".to_string());
        let cid = format!("{file}::class::Box");
        let n_methods = 1 + g.below(2);
        for m in 0..n_methods {
            let mname = format!("fill{m}");
            let meth_start = lines.len() + 1;
            lines.push(format!("    int {mname}() {{"));
            lines.push("        Box b = new Box();".to_string());
            lines.push(format!("        return fill{}();", g.below(n_methods)));
            lines.push("    }".to_string());
            let meth_end = lines.len();
            entities.push(mk_entity(
                file,
                "method",
                &mname,
                Some(&cid),
                meth_start,
                meth_end,
            ));
        }
        lines.push("}".to_string());
        let class_end = lines.len();
        entities.insert(
            entities.len() - n_methods,
            mk_entity(file, "class", "Box", None, class_start, class_end),
        );

        WalkFixture {
            file_path: file,
            ext: ".java",
            source: lines.join("\n") + "\n",
            entities,
            symbol_table: HashMap::default(),
            entity_map: HashMap::default(),
        }
    }

    fn gen_typescript(g: &mut Gen) -> WalkFixture {
        let file = "gen_fixture.ts";
        let mut lines: Vec<String> = Vec::new();
        let mut entities: Vec<SemanticEntity> = Vec::new();
        lines.push("import { helper } from './helpers';".to_string());
        if g.chance(2) {
            lines.push("export { relay } from './relay';".to_string());
        }
        let c_start = lines.len() + 1;
        lines.push("class Store {".to_string());
        let cid = format!("{file}::class::Store");
        let m_start = lines.len() + 1;
        lines.push("  load(k: string) {".to_string());
        lines.push("    const s = new Store();".to_string());
        lines.push("    s.load(k);".to_string());
        lines.push("    return helper(k);".to_string());
        lines.push("  }".to_string());
        let m_end = lines.len();
        lines.push("}".to_string());
        entities.push(mk_entity(
            file,
            "class",
            "Store",
            None,
            c_start,
            lines.len(),
        ));
        entities.push(mk_entity(
            file,
            "method",
            "load",
            Some(&cid),
            m_start,
            m_end,
        ));

        WalkFixture {
            file_path: file,
            ext: ".ts",
            source: lines.join("\n") + "\n",
            entities,
            symbol_table: HashMap::default(),
            entity_map: HashMap::default(),
        }
    }

    #[test]
    fn fused_triple_walk_matches_three_sequential_walks() {
        let mut g = Gen::new(0x5EED_3A03);
        let mut family_scopes = [0usize; 6];
        let mut family_refs = [0usize; 6];
        let mut family_imports = [0usize; 6];

        for round in 0..24 {
            let fixtures: Vec<(usize, WalkFixture)> = vec![
                (0, gen_python(&mut g, round == 0)),
                (1, gen_csharp(&mut g)),
                (2, gen_rust(&mut g)),
                (3, gen_go(&mut g)),
                (4, gen_typescript(&mut g)),
                (5, gen_java(&mut g)),
            ];
            for (family, fx) in fixtures {
                let spec = run_walks(&fx, ImportMode::SpecSequential);
                let fused = run_walks(&fx, ImportMode::FusedReplay);
                let label = format!("{} (round {round})", fx.file_path);
                assert_outcomes_equal(&spec, &fused, &label);
                family_scopes[family] += spec.scopes.len().saturating_sub(1);
                family_refs[family] += spec.ast_refs.len();
                family_imports[family] += spec.import_table.len();
            }
        }

        // NON-VACUITY: every family battery built real scopes and collected
        // real refs; the resolvable-import families resolved imports.
        for (family, name) in ["python", "csharp", "rust", "go", "typescript", "java"]
            .iter()
            .enumerate()
        {
            assert!(
                family_scopes[family] > 0,
                "non-vacuity: {name} samples built no non-root scopes"
            );
            assert!(
                family_refs[family] > 0,
                "non-vacuity: {name} samples collected no refs"
            );
        }
        assert!(
            family_imports[0] > 0,
            "non-vacuity: python samples resolved no imports"
        );
        assert!(
            family_imports[2] > 0,
            "non-vacuity: rust samples resolved no imports"
        );
        // MUL: go's aliased multi-spec import
        // block (`gen_go`'s "example.com/pkg/util" registered against
        // `pkg/util/util.go::DoWork`) must actually resolve through
        // `go_pkg_index` — this was an unasserted gap (every other
        // resolvable-import family had this check, go did not) closed by
        // this change since it is now touching this exact test.
        assert!(
            family_imports[3] > 0,
            "non-vacuity: go samples resolved no imports"
        );
        // C# has no import-statement kinds at all — the fused walk must
        // record nothing, which is exactly where dotnet's extract cost goes.
        assert_eq!(
            family_imports[1], 0,
            "csharp samples must resolve no imports (no matching kinds)"
        );
        // MUL Phase 2: Java's imports
        // classify as GoImport (shared grammar kind) but `go_pkg_index`
        // only ever matches `.go`-suffixed entities, so dispatch is a
        // documented no-op — the fused walk still records descriptors
        // (proven by `precompute_scope_resolvable_file_facts_some_for_
        // java_with_imports`), it just resolves none of them.
        assert_eq!(
            family_imports[5], 0,
            "java samples must resolve no imports (GoImport dispatch is a \
             no-op for .java files — go_pkg_index never matches them)"
        );
    }

    /// POSITIVE CONTROL for the replay order: on the same-alias-two-targets
    /// nested pair, extract's LIFO order (except-branch handled before
    /// try-branch, so the try-branch import wins last-write-wins) differs
    /// from document order. A fused implementation that fired handlers in
    /// document order would be caught by the invariant test on exactly this
    /// fixture; this test proves the fixture really distinguishes the two.
    #[test]
    fn import_replay_order_is_load_bearing() {
        let mut g = Gen::new(0x5EED_3A04);
        let fx = gen_python(&mut g, true);

        let spec = run_walks(&fx, ImportMode::SpecSequential);
        let fused = run_walks(&fx, ImportMode::FusedReplay);
        let broken = run_walks(&fx, ImportMode::BrokenDocOrder);

        let key = (fx.file_path.to_string(), "S".to_string());
        assert_eq!(
            spec.import_table.get(&key).map(String::as_str),
            Some("mod0.py::function::shared"),
            "spec: extract handles the except-branch first, so the try-branch (mod0) wins"
        );
        assert_eq!(
            spec.import_table.get(&key),
            fused.import_table.get(&key),
            "fused replay must reproduce extract's order"
        );
        assert_eq!(
            broken.import_table.get(&key).map(String::as_str),
            Some("mod1.py::function::shared"),
            "positive control: document order picks the other target"
        );
        assert_ne!(
            spec.import_table.get(&key),
            broken.import_table.get(&key),
            "positive control: the order-witness pair must distinguish the orders"
        );
    }

    /// (MUL phase 2): explicit
    /// record-then-dispatch vs dispatch-direct equivalence witness, on top
    /// of `fused_triple_walk_matches_three_sequential_walks`'s
    /// `SpecSequential`-vs-`FusedReplay` comparison (which this refactor
    /// turns into exactly this same proof, across 5 language families and
    /// 24 rounds — see that test's and `ImportMode`'s doc comments).
    ///
    /// This test pins the composition directly, at the function level
    /// rather than through `run_walks`' indirection: it calls
    /// [`record_import_stmts_pruned`] then [`dispatch_import_stmts_from_facts`]
    /// by hand — the same two calls pass 2's one production caller and the
    /// fused-walk precompute producer both make, with no wrapper function
    /// in between — and compares the result against `extract_imports_from_ast`
    /// dispatching descriptors directly, per node, during a fresh full
    /// traversal. It also asserts on the raw `Vec<ImportStmtFacts>`
    /// `record_import_stmts_pruned` returns, so a future change to a
    /// handler's descriptor shape gets caught here even if it happens not
    /// to move any `import_table` entry on this fixture.
    #[test]
    fn record_then_dispatch_matches_dispatch_direct() {
        let mut g = Gen::new(0x5EED_3A05);
        let fx = gen_python(&mut g, true);
        let config = scope_resolve_config_for_path(fx.file_path).expect("config");
        let lang = crate::parser::plugins::code::languages::get_language_config(fx.ext)
            .expect("language config");
        let tree =
            crate::parser::plugins::code::parse_tree(lang, &fx.source).expect("fixture parses");
        let source = fx.source.as_bytes();

        let (_ast_refs, import_starts, _saw_call) = {
            let file_entities: Vec<&SemanticEntity> = fx.entities.iter().collect();
            let file_lookup = FileEntityLookup::new(&file_entities);
            let mut children_by_parent: HashMap<&str, Vec<&SemanticEntity>> = HashMap::default();
            let mut entity_map = fx.entity_map.clone();
            for e in &fx.entities {
                entity_map.insert(
                    e.id.clone(),
                    EntityInfo {
                        id: e.id.clone(),
                        name: e.name.clone(),
                        entity_type: e.entity_type.clone(),
                        file_path: e.file_path.clone(),
                        parent_id: e.parent_id.clone(),
                        start_line: e.start_line,
                        end_line: e.end_line,
                    },
                );
                if let Some(ref pid) = e.parent_id {
                    children_by_parent.entry(pid.as_str()).or_default().push(e);
                }
            }
            let mut scopes: Vec<Scope> = vec![Scope {
                parent: None,
                defs: HashMap::default(),
                bindings: HashSet::default(),
                binding_rows: HashMap::default(),
                types: HashMap::default(),
                pending_call_types: HashMap::default(),
                pending_field_types: HashMap::default(),
                owner_id: None,
                kind: "module",
            }];
            let mut entity_scope_map: HashMap<String, usize> = HashMap::default();
            let mut entity_inner_scope: HashMap<String, usize> = HashMap::default();
            fused_scope_refs_import_walk(
                tree.root_node(),
                0,
                &mut scopes,
                &mut entity_scope_map,
                &mut entity_inner_scope,
                &file_lookup,
                &children_by_parent,
                &entity_map,
                source,
                config,
            )
        };
        assert!(
            !import_starts.is_empty(),
            "non-vacuity: the load-bearing python fixture must have real imports"
        );

        let descriptors =
            record_import_stmts_pruned(tree.root_node(), &import_starts, source, config, false);
        assert!(
            !descriptors.is_empty(),
            "record_import_stmts_pruned must record at least one descriptor"
        );

        let entity_map = fx.entity_map.clone();
        let go_pkg_index: GoPkgIndex = HashMap::default();
        let ts_default_exports = TsDefaultExportTable {
            exports_by_file: HashMap::default(),
            sorted_files: Vec::new(),
        };
        let top_level_entities = OnceLock::new();
        let py_top_level_entities = OnceLock::new();
        let rust_top_level_entities = OnceLock::new();
        let parsed_files: &[(String, String, tree_sitter::Tree)] = &[];
        let content_by_file = OnceLock::new();
        let exported_names_by_file: Mutex<HashMap<String, Arc<HashSet<String>>>> =
            Mutex::new(HashMap::default());

        // Record-then-dispatch, by hand, two calls.
        let mut by_hand_table: HashMap<(String, String), String> = HashMap::default();
        let mut by_hand_scopes: Vec<Scope> = vec![Scope {
            parent: None,
            defs: HashMap::default(),
            bindings: HashSet::default(),
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        }];
        let mut rec = Recorder::off();
        dispatch_import_stmts_from_facts(
            &descriptors,
            fx.file_path,
            &fx.symbol_table,
            &entity_map,
            &mut by_hand_table,
            &mut by_hand_scopes,
            &go_pkg_index,
            &ts_default_exports,
            &top_level_entities,
            &py_top_level_entities,
            &rust_top_level_entities,
            parsed_files,
            &content_by_file,
            &exported_names_by_file,
            false,
            &mut rec,
        );

        // Dispatch-direct: build a descriptor and dispatch it immediately,
        // per node, during a fresh full traversal (extract's own shape) —
        // must land on the same import_table as the batched record-then-
        // dispatch path above.
        let mut direct_table: HashMap<(String, String), String> = HashMap::default();
        let mut direct_scopes: Vec<Scope> = vec![Scope {
            parent: None,
            defs: HashMap::default(),
            bindings: HashSet::default(),
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        }];
        let mut rec3 = Recorder::off();
        extract_imports_from_ast(
            tree.root_node(),
            fx.file_path,
            source,
            &fx.symbol_table,
            &entity_map,
            &mut direct_table,
            &mut direct_scopes,
            config,
            &go_pkg_index,
            &ts_default_exports,
            &top_level_entities,
            &py_top_level_entities,
            &rust_top_level_entities,
            parsed_files,
            &content_by_file,
            &exported_names_by_file,
            false,
            &mut rec3,
        );

        assert_eq!(
            by_hand_table, direct_table,
            "record-then-dispatch must equal dispatch-direct"
        );
    }

    /// `skip_js_ts_imports: true` (the chunked session path — a pre-built
    /// import table is always supplied there) must short-circuit
    /// `build_import_stmt_facts` to an empty stub for `TsImport`/
    /// `TsReExport` *before* it walks the node — same cost shape as the old
    /// `if !skip_js_ts_imports { extract_ts_import(...) }` gate, which
    /// never called the extraction function at all. This is the perf half
    /// of the refactor: dispatch already no-ops on a skipped TS descriptor
    /// regardless of its contents (proven by the two assertions below), so
    /// the stub and the fully-walked descriptor are dispatch-equivalent —
    /// the stub just avoids the wasted tree walk.
    #[test]
    fn skipped_ts_import_descriptor_is_a_stub_not_a_full_walk() {
        let mut g = Gen::new(0x5EED_3A06);
        let fx = gen_typescript(&mut g);
        let config = scope_resolve_config_for_path(fx.file_path).expect("config");
        let lang = crate::parser::plugins::code::languages::get_language_config(fx.ext)
            .expect("language config");
        let tree =
            crate::parser::plugins::code::parse_tree(lang, &fx.source).expect("fixture parses");
        let source = fx.source.as_bytes();

        let mut cursor = tree.root_node().walk();
        let import_node = tree
            .root_node()
            .named_children(&mut cursor)
            .find(|c| classify_import_stmt(c.kind(), config).is_some())
            .expect("fixture has a leading TS import statement");
        let stmt = classify_import_stmt(import_node.kind(), config).expect("classified");

        let full = build_import_stmt_facts(stmt, import_node, source, false);
        let skipped = build_import_stmt_facts(stmt, import_node, source, true);

        match &full {
            ImportStmtFacts::TsImport { source: s, items } => {
                assert!(
                    !s.is_empty(),
                    "non-vacuity: fixture's import has a real source"
                );
                assert!(
                    !items.is_empty(),
                    "non-vacuity: fixture's import has real items"
                );
            }
            other => panic!("expected TsImport, got {other:?}"),
        }
        assert_eq!(
            skipped,
            ImportStmtFacts::TsImport {
                source: String::new(),
                items: Vec::new(),
            },
            "skip_js_ts_imports=true must short-circuit to an empty stub"
        );

        // Dispatch-equivalence: both descriptors must produce the same
        // (empty) effect once `skip_js_ts_imports=true` at dispatch time —
        // the stub is a pure optimization, never an observable difference.
        let entity_map = fx.entity_map.clone();
        let go_pkg_index: GoPkgIndex = HashMap::default();
        let ts_default_exports = TsDefaultExportTable {
            exports_by_file: HashMap::default(),
            sorted_files: Vec::new(),
        };
        let top_level_entities = OnceLock::new();
        let py_top_level_entities = OnceLock::new();
        let rust_top_level_entities = OnceLock::new();
        let parsed_files: &[(String, String, tree_sitter::Tree)] = &[];
        let content_by_file = OnceLock::new();
        let exported_names_by_file: Mutex<HashMap<String, Arc<HashSet<String>>>> =
            Mutex::new(HashMap::default());
        let empty_scope = || Scope {
            parent: None,
            defs: HashMap::default(),
            bindings: HashSet::default(),
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        };

        for descriptor in [&full, &skipped] {
            let mut table: HashMap<(String, String), String> = HashMap::default();
            let mut scopes = vec![empty_scope()];
            let mut rec = Recorder::off();
            dispatch_import_stmt(
                descriptor,
                fx.file_path,
                &fx.symbol_table,
                &entity_map,
                &mut table,
                &mut scopes,
                &go_pkg_index,
                &ts_default_exports,
                &top_level_entities,
                &py_top_level_entities,
                &rust_top_level_entities,
                parsed_files,
                &content_by_file,
                &exported_names_by_file,
                true, // skip_js_ts_imports at dispatch time
                &mut rec,
            );
            assert!(
                table.is_empty(),
                "a TS import dispatched under skip_js_ts_imports=true must be a no-op"
            );
        }
    }

    /// test (a): when one scope's `.defs` and `.bindings`
    /// co-populate the same name — exactly what happens once the
    /// function-like branch's registration loop inserts a nested entity into
    /// `.defs` while `scan_assignments` records the same declarator as a
    /// `.binding` — the lookup must resolve [`ScopeChainLookup::Defined`],
    /// never [`ScopeChainLookup::Shadowed`]. Co-population at the same scope
    /// index can only be the same declaration (the grammar admits no
    /// redeclaration), so the old gate's bindings-only short-circuit was
    /// killing correct edges.
    #[test]
    fn shadow_respecting_lookup_same_scope_defs_and_bindings_is_defined() {
        // module(0) -> function create(1); create's scope holds BOTH the
        // registered nested-entity def and the scan_assignments binding for
        // the same name.
        let mut module_defs = HashMap::default();
        module_defs.insert(
            "create".to_string(),
            "no-keywords.cjs::method::create".to_string(),
        );
        let module_scope = Scope {
            parent: None,
            defs: module_defs,
            bindings: HashSet::default(),
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        };
        let mut create_defs = HashMap::default();
        create_defs.insert(
            "checkElements".to_string(),
            "no-keywords.cjs::method::create::checkElements".to_string(),
        );
        let mut create_bindings = HashSet::default();
        create_bindings.insert("checkElements".to_string());
        let create_scope = Scope {
            parent: Some(0),
            defs: create_defs,
            bindings: create_bindings,
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: Some("no-keywords.cjs::method::create".to_string()),
            kind: "function",
        };
        let scopes = vec![module_scope, create_scope];

        let direct =
            lookup_scope_chain_respecting_shadows(1, &scopes, "checkElements");
        assert_eq!(
            direct,
            ScopeChainLookup::Defined(
                "no-keywords.cjs::method::create::checkElements".to_string()
            ),
            "same-scope .defs+.bindings co-population must resolve as Defined"
        );

        // Cached wrapper: fresh cache computes the same answer, and the
        // second call serves it from the cache entry.
        let mut cache = ScopeLookupCache::default();
        let first = lookup_scope_chain_respecting_shadows_cached(
            1,
            &scopes,
            "checkElements",
            &mut cache,
        );
        let second = lookup_scope_chain_respecting_shadows_cached(
            1,
            &scopes,
            "checkElements",
            &mut cache,
        );
        assert_eq!(first, direct);
        assert_eq!(second, direct);
    }

    /// test (b): the shadow-safety property the old
    /// `is_local_binding_in_scopes_cached` gate protected must survive. A
    /// `.bindings`-only hit in a *nearer* scope stops the walk even when an
    /// ancestor scope has a `.defs` entry for the same name — the resolver
    /// translates [`ScopeChainLookup::Shadowed`] to "no edge".
    #[test]
    fn shadow_respecting_lookup_inner_binding_shadows_ancestor_def() {
        // module(0) defines the entity; function(1) merely rebinds the name;
        // inner arrow fn(2) sees only its own local rebinding.
        let mut module_defs = HashMap::default();
        module_defs.insert(
            "isKeyword".to_string(),
            "no-keywords.cjs::method::create::isKeyword".to_string(),
        );
        let module_scope = Scope {
            parent: None,
            defs: module_defs,
            bindings: HashSet::default(),
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "module",
        };
        let mut fn_bindings = HashSet::default();
        fn_bindings.insert("isKeyword".to_string());
        let fn_scope = Scope {
            parent: Some(0),
            defs: HashMap::default(),
            bindings: fn_bindings,
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "function",
        };
        let mut inner_bindings = HashSet::default();
        inner_bindings.insert("isKeyword".to_string());
        let inner_scope = Scope {
            parent: Some(1),
            defs: HashMap::default(),
            bindings: inner_bindings,
            binding_rows: HashMap::default(),
            types: HashMap::default(),
            pending_call_types: HashMap::default(),
            pending_field_types: HashMap::default(),
            owner_id: None,
            kind: "function",
        };
        let scopes = vec![module_scope, fn_scope, inner_scope];

        // From the innermost scope: the local binding shadows the ancestor
        // def -> Shadowed -> callers emit no edge (the property the old gate
        // protected).
        assert_eq!(
            lookup_scope_chain_respecting_shadows(2, &scopes, "isKeyword"),
            ScopeChainLookup::Shadowed,
            "an inner bindings-only hit must stop resolution before the \
             ancestor def"
        );
        // From the middle scope itself: same verdict — its own binding
        // shadows the module-level def.
        assert_eq!(
            lookup_scope_chain_respecting_shadows(1, &scopes, "isKeyword"),
            ScopeChainLookup::Shadowed,
        );
        // From the module scope (no binding anywhere on its chain): the def
        // resolves normally.
        assert_eq!(
            lookup_scope_chain_respecting_shadows(0, &scopes, "isKeyword"),
            ScopeChainLookup::Defined(
                "no-keywords.cjs::method::create::isKeyword".to_string()
            ),
        );
        // Unknown names stay NotFound so callers fall through to import /
        // global fallbacks unchanged.
        assert_eq!(
            lookup_scope_chain_respecting_shadows(2, &scopes, "nope"),
            ScopeChainLookup::NotFound,
        );
    }
}
