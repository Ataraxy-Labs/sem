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

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::model::entity::SemanticEntity;

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
    find_import_file, find_import_target, import_source_matches_file, is_js_ts_file,
    js_ts_named_exports_from_content, sort_import_candidate_files, JS_TS_EXTENSIONS,
};
use crate::parser::plugins::code::languages::{
    get_language_config, AssignmentStrategy, CallNodeStyle, ClassNameField, InitStrategy,
    ParamNameField, ScopeResolveConfig,
};

type AttrToParamIndex<'a> = HashMap<(&'a str, &'a str), Vec<(&'a str, &'a str)>>;

/// A scope in the scope tree. Scopes are nested: module -> class -> function -> block.
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

/// Reference found in the AST
struct AstRef {
    /// Kind of reference
    kind: AstRefKind,
    /// Row (0-indexed) where this reference appears in the source
    row: usize,
    /// Byte range for the referenced syntax node in the file.
    start_byte: usize,
    end_byte: usize,
}

enum AstRefKind {
    /// Bare name call: `foo()`
    Call {
        name: String,
        argument_labels: Option<Vec<Option<String>>>,
    },
    /// Qualified path call: `module::function()`
    ScopedCall { path: String, name: String },
    /// Attribute call: `x.method()`
    MethodCall {
        receiver: String,
        method: String,
        argument_labels: Option<Vec<Option<String>>>,
    },
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
    /// Go package index: pkg_name → [(entity_name, entity_id)]
    /// Avoids O(symbol_table) scan per Go import.
    pub(crate) go_pkg_index: HashMap<String, Vec<(String, String)>>,
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

struct TopLevelEntityIndex {
    entities_by_file: HashMap<String, Vec<(String, String)>>,
    sorted_files: Vec<String>,
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
    defs: HashMap<usize, HashMap<String, Option<String>>>,
    local_bindings: HashMap<usize, HashMap<String, bool>>,
    types: HashMap<usize, HashMap<String, Option<String>>>,
    enclosing_classes: HashMap<usize, Option<String>>,
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

pub(crate) fn class_member_owner_name(parent: &EntityInfo) -> Option<&str> {
    matches!(
        parent.entity_type.as_str(),
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
    .then_some(parent.name.as_str())
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
    )
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
) -> ScopeResultFull {
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
        };
        &owned_lookups
    };
    let symbol_table = lookups.symbol_table.as_ref();
    let class_members = &lookups.class_members;
    let owner_members = &lookups.owner_members;
    let entity_ranges = &lookups.entity_ranges;
    let go_pkg_index = &lookups.go_pkg_index;

    // Build file-path indexed entity lookup: file_path -> Vec<&SemanticEntity>
    let mut entities_by_file: HashMap<&str, Vec<&SemanticEntity>> = HashMap::default();
    for entity in all_entities {
        entities_by_file
            .entry(entity.file_path.as_str())
            .or_default()
            .push(entity);
    }

    // Build parent_id indexed entity lookup: parent_id -> Vec<&SemanticEntity>
    let mut children_by_parent: HashMap<&str, Vec<&SemanticEntity>> = HashMap::default();
    for entity in all_entities {
        if let Some(ref pid) = entity.parent_id {
            children_by_parent
                .entry(pid.as_str())
                .or_default()
                .push(entity);
        }
    }

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
    // Parse any files not already in the pre-parsed set
    for file_path in file_paths {
        if pre_set.contains(file_path) {
            continue;
        }
        let full_path = root.join(file_path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
        let config = match get_language_config(ext) {
            Some(c) => c,
            None => continue,
        };
        let language = match (config.get_language)() {
            Some(l) => l,
            None => continue,
        };
        let mut parser = tree_sitter::Parser::new();
        let _ = parser.set_language(&language);
        if let Some(tree) = parser.parse(content.as_bytes(), None) {
            owned_parsed_files.push((file_path.clone(), content, tree));
        }
    }
    let parsed_files: &[(String, String, tree_sitter::Tree)] = &owned_parsed_files;
    let content_by_file = OnceLock::new();
    let exported_names_by_file: Mutex<HashMap<String, Arc<HashSet<String>>>> =
        Mutex::new(HashMap::default());
    // The default-export table is consulted only while resolving JS/TS imports.
    // When an import table is supplied (the graph-build path), those imports are
    // already resolved and `extract_ts_import`/`extract_ts_re_export` are skipped,
    // so the table is never read — building it would be pure waste on a large repo.
    let ts_default_exports = if pre_built_import_table.is_some() {
        TsDefaultExportTable {
            exports_by_file: HashMap::default(),
            sorted_files: Vec::new(),
        }
    } else {
        build_ts_default_export_table(parsed_files, &symbol_table, entity_map)
    };
    let top_level_entities = OnceLock::new();

    // Pass 1: Scan ALL files for return types and instance attr types first
    // This ensures cross-file return type info is available during resolution
    // Parallelized: each file produces local maps, then merged sequentially.
    let pass1_results: Vec<(
        HashMap<String, String>,
        HashMap<(String, String), String>,
        HashMap<String, Vec<String>>,
        HashMap<(String, String), String>,
    )> = maybe_par_iter!(parsed_files)
        .filter_map(|(file_path, content, tree)| {
            let source = content.as_bytes();
            let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
            let config = get_language_config(ext).and_then(|c| c.scope_resolve)?;

            let file_entities = entities_by_file
                .get(file_path.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let file_lookup = FileEntityLookup::new(file_entities);

            let mut local_return_type_map: HashMap<String, String> = HashMap::default();
            scan_return_types(
                tree.root_node(),
                file_path,
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
                file_path,
                file_entities,
                entity_map,
                source,
                &mut local_instance_attr_types,
                &mut local_init_params,
                &mut local_attr_to_param,
                config,
            );

            Some((
                local_return_type_map,
                local_instance_attr_types,
                local_init_params,
                local_attr_to_param,
            ))
        })
        .collect();

    for (local_rtm, local_iat, local_ip, local_atp) in pass1_results {
        return_type_map.extend(local_rtm);
        instance_attr_types.extend(local_iat);
        init_params.extend(local_ip);
        attr_to_param.extend(local_atp);
    }

    // Pass 1b: Infer constructor parameter types from call sites
    // For `Transaction(get_connection())`, infer conn param has type Connection.
    // Then resolve self.conn = conn -> (Transaction, conn) -> Connection
    infer_constructor_param_types(
        parsed_files,
        &return_type_map,
        &init_params,
        &attr_to_param,
        &symbol_table,
        &mut instance_attr_types,
    );
    let func_name_return_types = deterministic_return_types_by_name(&return_type_map, symbol_table);

    let swift_call_signatures = if parsed_files
        .iter()
        .any(|(file_path, _, _)| file_path.ends_with(".swift"))
    {
        build_swift_call_signatures(parsed_files, all_entities, &entity_ranges, entity_map)
    } else {
        HashMap::default()
    };

    // Group the prebuilt import table by importing file once. Otherwise every file
    // in Pass 2 would rescan the entire table to find its own entries — O(files ×
    // imports), which is quadratic on a large repo. Grouping makes each file O(its
    // own imports).
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

    // Pass 2: Build scopes, imports, and resolve references per file (parallel)
    let per_file_results: Vec<(
        Vec<(String, String, RefType)>,
        Vec<ResolutionEntry>,
        HashMap<String, HashSet<String>>,
    )> = maybe_par_iter!(parsed_files)
        .filter_map(|(file_path, content, tree)| {
            let source = content.as_bytes();
            let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
            let config = get_language_config(ext).and_then(|c| c.scope_resolve)?;

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

            let file_entities: Vec<&SemanticEntity> = entities_by_file
                .get(file_path.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .to_vec();
            let file_lookup = FileEntityLookup::new(&file_entities);
            let entity_spans = find_entity_source_spans(&file_entities, content);

            build_scopes_from_ast(
                tree.root_node(),
                0,
                &mut scopes,
                &mut entity_scope_map,
                &mut entity_inner_scope,
                &file_lookup,
                &children_by_parent,
                entity_map,
                file_path,
                source,
                config,
            );

            let mut local_import_table: HashMap<(String, String), String> = HashMap::default();
            if pre_built_import_table.is_some() {
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
            extract_imports_from_ast(
                tree.root_node(),
                file_path,
                source,
                &symbol_table,
                entity_map,
                &mut local_import_table,
                &mut scopes,
                config,
                &go_pkg_index,
                &ts_default_exports,
                &top_level_entities,
                parsed_files,
                &content_by_file,
                &exported_names_by_file,
                pre_built_import_table.is_some(),
            );

            // The per-file import table is keyed by (file_path, name) but only ever
            // holds this file's entries, so re-key it by name once. resolve_ref then
            // looks up imports without allocating a key string per reference.
            let local_import_by_name: HashMap<&str, &str> = local_import_table
                .iter()
                .map(|((_, name), target_id)| (name.as_str(), target_id.as_str()))
                .collect();

            // Resolve pending call types using the complete return type map.
            inject_return_type_bindings(
                &mut scopes,
                &func_name_return_types,
                &return_type_map,
                &local_import_by_name,
            );
            // Resolve `val x = obj.field` accesses against the class field-type map.
            inject_field_type_bindings(&mut scopes, &instance_attr_types);

            let mut file_edges: Vec<(String, String, RefType)> = Vec::new();
            let mut file_log: Vec<ResolutionEntry> = Vec::new();
            let mut file_consumed_words: HashMap<String, HashSet<String>> = HashMap::default();

            // Walk the AST once for the entire file, collecting all refs with row positions
            let all_file_refs = collect_all_file_refs(tree.root_node(), source, config);
            let refs_by_row = build_refs_by_row(&all_file_refs);
            let descendant_ranges_by_entity =
                build_descendant_ranges_by_entity(&file_entities, entity_map);
            let mut lookup_cache = ScopeLookupCache::default();
            let mut last_resolution: Option<(
                ResolutionCacheKey<'_>,
                Option<(String, RefType, &'static str)>,
            )> = None;

            for entity in &file_entities {
                if emit_entity_ids
                    .as_ref()
                    .is_some_and(|ids| !ids.contains(entity.id.as_str()))
                {
                    continue;
                }

                let scope_idx = entity_inner_scope
                    .get(&entity.id)
                    .or_else(|| entity_scope_map.get(&entity.id))
                    .copied()
                    .unwrap_or(0);

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
                            AstRefKind::Call { name, .. } => name == &entity.name,
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
                                cached.clone()
                            } else {
                                let resolved = resolve_ref(
                                    ast_ref,
                                    scope_idx,
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
                                );
                                last_resolution = Some((cache_key, resolved.clone()));
                                resolved
                            }
                        } else {
                            resolve_ref(
                                ast_ref,
                                scope_idx,
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
                            )
                        };

                        if let Some((target_id, ref_type, method)) = resolution {
                            if target_id != entity.id {
                                let is_parent_child =
                                    entity.parent_id.as_ref().map_or(false, |pid| {
                                        pid == &target_id
                                            || entity_map.get(&target_id).map_or(false, |t| {
                                                t.parent_id.as_ref() == Some(&entity.id)
                                            })
                                    });

                                if !is_parent_child {
                                    let reference = ref_description(ast_ref);
                                    file_edges.push((
                                        entity.id.clone(),
                                        target_id.clone(),
                                        ref_type,
                                    ));
                                    add_scope_reference_words(entity_consumed, &reference);
                                    file_log.push(ResolutionEntry {
                                        from_entity: entity.id.clone(),
                                        reference,
                                        resolved_to: Some(target_id),
                                        method,
                                    });
                                }
                            }
                        } else {
                            let reference = ref_description(ast_ref);
                            add_scope_reference_words(entity_consumed, &reference);
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

            Some((file_edges, file_log, file_consumed_words))
        })
        .collect();

    for (file_edges, file_log, file_consumed_words) in per_file_results {
        all_edges.extend(file_edges);
        log.extend(file_log);
        for (entity_id, words) in file_consumed_words {
            consumed_words.entry(entity_id).or_default().extend(words);
        }
    }

    // Deduplicate edges
    let mut seen: HashSet<(String, String)> =
        HashSet::with_capacity_and_hasher(all_edges.len(), Default::default());
    let deduped_edges: Vec<(String, String, RefType)> = {
        let mut result = Vec::with_capacity(all_edges.len());
        for edge in all_edges {
            if seen.insert((edge.0.clone(), edge.1.clone())) {
                result.push(edge);
            }
        }
        result
    };
    let all_edges = deduped_edges;

    ScopeResultFull {
        edges: all_edges,
        resolution_log: log,
        consumed_words,
    }
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
/// Creates class scopes and maps methods to them.
/// Uses an iterative worklist to avoid stack overflow on deeply nested ASTs.
/// Fixes: https://github.com/Ataraxy-Labs/sem/issues/103
fn push_named_children_rev<'a>(
    worklist: &mut Vec<tree_sitter::Node<'a>>,
    node: tree_sitter::Node<'a>,
) {
    for idx in (0..node.named_child_count()).rev() {
        if let Some(child) = node.named_child(idx as u32) {
            worklist.push(child);
        }
    }
}

fn push_scoped_named_children_rev<'a>(
    worklist: &mut Vec<(tree_sitter::Node<'a>, usize)>,
    node: tree_sitter::Node<'a>,
    scope: usize,
) {
    for idx in (0..node.named_child_count()).rev() {
        if let Some(child) = node.named_child(idx as u32) {
            worklist.push((child, scope));
        }
    }
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
    _file_path: &str,
    source: &[u8],
    config: &ScopeResolveConfig,
) {
    // Each entry: (node, current_scope)
    let mut worklist: Vec<(tree_sitter::Node, usize)> = vec![(root, root_scope)];

    while let Some((node, current_scope)) = worklist.pop() {
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

                push_scoped_named_children_rev(&mut worklist, node, class_scope_idx);
                continue;
            } else if !is_impl {
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
                push_scoped_named_children_rev(&mut worklist, node, class_scope_idx);
                continue;
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

            push_scoped_named_children_rev(&mut worklist, node, mod_scope_idx);
            continue;
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

            push_scoped_named_children_rev(&mut worklist, node, func_scope_idx);
            continue;
        }

        push_scoped_named_children_rev(&mut worklist, node, current_scope);
    }
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

/// Build Go package index: pkg_name → [(entity_name, entity_id)]
/// Maps file stems and parent directory names to entities for O(1) package import lookup.
pub(crate) fn build_go_pkg_index(
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> HashMap<String, Vec<(String, String)>> {
    let mut idx: HashMap<String, Vec<(String, String)>> = HashMap::default();
    for (name, target_ids) in symbol_table.iter() {
        for target_id in target_ids {
            if let Some(entity) = entity_map.get(target_id) {
                if !entity.file_path.ends_with(".go") {
                    continue;
                }
                let file_stem = entity
                    .file_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&entity.file_path);
                let file_stem = file_stem.strip_suffix(".go").unwrap_or(file_stem);
                idx.entry(file_stem.to_string())
                    .or_default()
                    .push((name.clone(), target_id.clone()));
                if let Some(parent_start) = entity.file_path.rfind('/') {
                    let parent_path = &entity.file_path[..parent_start];
                    if let Some(dir_name_start) = parent_path.rfind('/') {
                        let dir_name = &parent_path[dir_name_start + 1..];
                        if dir_name != file_stem {
                            idx.entry(dir_name.to_string())
                                .or_default()
                                .push((name.clone(), target_id.clone()));
                        }
                    } else if !parent_path.is_empty() && parent_path != file_stem {
                        idx.entry(parent_path.to_string())
                            .or_default()
                            .push((name.clone(), target_id.clone()));
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
    _file_path: &str,
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
    _file_path: &str,
    _file_entities: &[&SemanticEntity],
    _entity_map: &HashMap<String, EntityInfo>,
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
fn infer_constructor_param_types(
    parsed_files: &[(String, String, tree_sitter::Tree)],
    return_type_map: &HashMap<String, String>,
    init_params: &HashMap<String, Vec<String>>,
    attr_to_param: &HashMap<(String, String), String>,
    symbol_table: &HashMap<String, Vec<String>>,
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let func_name_returns = deterministic_return_types_by_name(return_type_map, symbol_table);
    let attr_to_param_index = build_attr_to_param_index(attr_to_param);

    // Scan all files for constructor call sites: ClassName(arg1, arg2, ...)
    // Parallelized: each file produces local results, then merged.
    let local_results: Vec<HashMap<(String, String), String>> = maybe_par_iter!(parsed_files)
        .map(|(_file_path, content, tree)| {
            let source = content.as_bytes();
            let mut local_attr_types: HashMap<(String, String), String> = HashMap::default();
            scan_constructor_calls(
                tree.root_node(),
                source,
                &func_name_returns,
                init_params,
                &attr_to_param_index,
                &mut local_attr_types,
            );
            local_attr_types
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

fn deterministic_return_types_by_name(
    return_type_map: &HashMap<String, String>,
    symbol_table: &HashMap<String, Vec<String>>,
) -> HashMap<String, String> {
    let mut by_name = HashMap::with_capacity_and_hasher(return_type_map.len(), Default::default());
    for (name, target_ids) in symbol_table {
        if let Some(return_type) = target_ids
            .iter()
            .find_map(|target_id| return_type_map.get(target_id))
        {
            by_name.insert(name.clone(), return_type.clone());
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

fn scan_constructor_calls(
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
                                    let arg_type = infer_expr_type(arg, source, func_name_returns);

                                    if let Some(at) = arg_type {
                                        if let Some(attrs) = attr_to_param_index
                                            .get(&(class_name, param_name.as_str()))
                                        {
                                            for (cn, attr) in attrs {
                                                instance_attr_types
                                                    .entry(((*cn).to_string(), (*attr).to_string()))
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
fn infer_expr_type(
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

/// Resolve pending call types using the return type map.
/// For scopes with `x = func()` where func has a known return type, bind x to that type.
fn inject_return_type_bindings(
    scopes: &mut Vec<Scope>,
    func_name_return_types: &HashMap<String, String>,
    return_type_map: &HashMap<String, String>,
    import_table_by_name: &HashMap<&str, &str>,
) {
    // Resolve pending call types in all scopes
    for scope in scopes.iter_mut() {
        let resolved: Vec<(String, String)> = scope
            .pending_call_types
            .iter()
            .filter_map(|(var_name, func_name)| {
                import_table_by_name
                    .get(func_name.as_str())
                    .and_then(|target_id| return_type_map.get(*target_id))
                    .or_else(|| func_name_return_types.get(func_name))
                    .map(|ret_type| (var_name.clone(), ret_type.clone()))
            })
            .collect();

        for (var_name, ret_type) in resolved {
            scope.types.insert(var_name, ret_type);
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
) {
    for _ in 0..4 {
        // Collect resolutions under immutable borrows, then apply mutably.
        let mut resolutions: Vec<(usize, String, String)> = Vec::new();
        for scope_idx in 0..scopes.len() {
            for (var, (obj, prop)) in &scopes[scope_idx].pending_field_types {
                if let Some(obj_type) = lookup_type_in_scopes(scope_idx, scopes, obj) {
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

fn build_top_level_entity_index(
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> TopLevelEntityIndex {
    let mut entities_by_file: HashMap<String, Vec<(String, String)>> = HashMap::default();

    for (name, target_ids) in symbol_table {
        for target_id in target_ids {
            let Some(info) = entity_map.get(target_id) else {
                continue;
            };
            if !is_js_ts_file(&info.file_path) || info.parent_id.is_some() {
                continue;
            }
            entities_by_file
                .entry(info.file_path.clone())
                .or_default()
                .push((name.clone(), target_id.clone()));
        }
    }

    let mut sorted_files: Vec<String> = entities_by_file.keys().cloned().collect();
    sort_import_candidate_files(&mut sorted_files, JS_TS_EXTENSIONS);

    TopLevelEntityIndex {
        entities_by_file,
        sorted_files,
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
fn extract_imports_from_ast<'a>(
    root: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    config: &ScopeResolveConfig,
    go_pkg_index: &HashMap<String, Vec<(String, String)>>,
    ts_default_exports: &TsDefaultExportTable,
    top_level_entities: &OnceLock<TopLevelEntityIndex>,
    parsed_files: &'a [(String, String, tree_sitter::Tree)],
    content_by_file: &OnceLock<HashMap<&'a str, &'a str>>,
    exported_names_by_file: &Mutex<HashMap<String, Arc<HashSet<String>>>>,
    skip_js_ts_imports: bool,
) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let ck = child.kind();
            let handled = match ck {
                "import_from_statement" => {
                    extract_python_import(
                        child,
                        file_path,
                        source,
                        symbol_table,
                        entity_map,
                        import_table,
                        scopes,
                    );
                    true
                }
                "import_statement"
                    if config.self_keywords.contains(&"self")
                        && config.self_keywords.contains(&"cls") =>
                {
                    // Python: `import mod` or `import mod as m`
                    extract_python_module_import(
                        child,
                        file_path,
                        source,
                        symbol_table,
                        entity_map,
                        import_table,
                        scopes,
                    );
                    true
                }
                "import_statement" if !config.self_keywords.contains(&"cls") => {
                    if !skip_js_ts_imports {
                        extract_ts_import(
                            child,
                            file_path,
                            source,
                            symbol_table,
                            entity_map,
                            import_table,
                            scopes,
                            ts_default_exports,
                            top_level_entities,
                            parsed_files,
                            content_by_file,
                            exported_names_by_file,
                        );
                    }
                    true
                }
                "export_statement" if !config.self_keywords.contains(&"cls") => {
                    if !skip_js_ts_imports {
                        extract_ts_re_export(
                            child,
                            file_path,
                            source,
                            symbol_table,
                            entity_map,
                            import_table,
                            scopes,
                            ts_default_exports,
                        );
                    }
                    true
                }
                "use_declaration" => {
                    extract_rust_use(
                        child,
                        file_path,
                        source,
                        symbol_table,
                        entity_map,
                        import_table,
                        scopes,
                    );
                    true
                }
                "import_declaration" => {
                    extract_go_import(
                        child,
                        file_path,
                        source,
                        symbol_table,
                        entity_map,
                        import_table,
                        scopes,
                        go_pkg_index,
                    );
                    true
                }
                _ => false,
            };
            if !handled {
                worklist.push(child);
            }
        }
    }
}

/// TS: `import { Foo, Bar } from './module'` or `import Foo from './module'`
fn extract_ts_import<'a>(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    ts_default_exports: &TsDefaultExportTable,
    top_level_entities: &OnceLock<TopLevelEntityIndex>,
    parsed_files: &'a [(String, String, tree_sitter::Tree)],
    content_by_file: &OnceLock<HashMap<&'a str, &'a str>>,
    exported_names_by_file: &Mutex<HashMap<String, Arc<HashSet<String>>>>,
) {
    // Extract the source module from the `from '...'` clause
    let source_path = node
        .child_by_field_name("source")
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("")
        .trim_matches(|c: char| c == '\'' || c == '"');

    if source_path.is_empty() {
        return;
    }

    // Walk children to find import clause
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_clause" {
            let mut clause_cursor = child.walk();
            for clause_child in child.named_children(&mut clause_cursor) {
                if clause_child.kind() == "named_imports" {
                    // { Foo, Bar as Baz }
                    let mut imports_cursor = clause_child.walk();
                    for spec in clause_child.named_children(&mut imports_cursor) {
                        if spec.kind() == "import_specifier" {
                            let original = spec
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("");
                            let local = spec
                                .child_by_field_name("alias")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or(original);

                            if !original.is_empty() {
                                resolve_import_name(
                                    original,
                                    local,
                                    source_path,
                                    file_path,
                                    JS_TS_EXTENSIONS,
                                    symbol_table,
                                    entity_map,
                                    import_table,
                                    scopes,
                                );
                            }
                        }
                    }
                } else if clause_child.kind() == "namespace_import" {
                    // import * as m from './module'
                    // Register exported source module entities so m.foo() resolves.
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
                        register_ts_namespace_import(
                            alias,
                            source_path,
                            file_path,
                            JS_TS_EXTENSIONS,
                            top_level_entities,
                            symbol_table,
                            entity_map,
                            parsed_files,
                            content_by_file,
                            exported_names_by_file,
                            import_table,
                            scopes,
                        );
                    }
                } else if clause_child.kind() == "identifier" {
                    // Default import: import Foo from './module'
                    let name = clause_child.utf8_text(source).unwrap_or("");
                    if !name.is_empty() {
                        resolve_default_import(
                            name,
                            source_path,
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
}

fn extract_ts_re_export(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    ts_default_exports: &TsDefaultExportTable,
) {
    let source_path = node
        .child_by_field_name("source")
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("")
        .trim_matches(|c: char| c == '\'' || c == '"');

    if source_path.is_empty() {
        return;
    }

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

                    if original == "default" {
                        resolve_default_import(
                            local,
                            source_path,
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
                            source_path,
                            file_path,
                            JS_TS_EXTENSIONS,
                            symbol_table,
                            entity_map,
                            import_table,
                            scopes,
                        );
                    }
                }
                "export_clause" | "namespace_export" => {
                    worklist.push(child);
                }
                _ => {}
            }
        }
    }
}

/// Rust: `use crate::module::Name;` or `use crate::module::{A, B};`
/// Parse from the text of the use_declaration for reliability.
fn extract_rust_use(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    let text = node.utf8_text(source).unwrap_or("").trim().to_string();
    // Strip `use ` prefix and trailing `;`
    let text = text.strip_prefix("use ").unwrap_or(&text);
    let text = text.strip_prefix("pub use ").unwrap_or(text);
    let text = text.trim_end_matches(';').trim();

    // Strip crate/super/self prefix
    let text = text
        .strip_prefix("crate::")
        .or_else(|| text.strip_prefix("super::"))
        .or_else(|| text.strip_prefix("self::"))
        .unwrap_or(text);

    // Check for grouped import: module::{A, B, C}
    if let Some(brace_pos) = text.find("::{") {
        let module_path = &text[..brace_pos];
        let source_module = module_path.rsplit("::").next().unwrap_or(module_path);

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
                );
            }
        }
    } else {
        // Simple import: module::Name
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
            );
        }
    }
}

/// Go: `import ("module/path")` - maps package names to entities
fn extract_go_import(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    go_pkg_index: &HashMap<String, Vec<(String, String)>>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_spec" || child.kind() == "import_spec_list" {
            extract_go_import_specs(
                child,
                file_path,
                source,
                symbol_table,
                entity_map,
                import_table,
                scopes,
                go_pkg_index,
            );
        } else if child.kind() == "interpreted_string_literal"
            || child.kind() == "raw_string_literal"
        {
            let path = child
                .utf8_text(source)
                .unwrap_or("")
                .trim_matches('"')
                .trim_matches('`');
            let pkg_name = path.rsplit('/').next().unwrap_or(path);
            register_go_package_imports(
                pkg_name,
                file_path,
                symbol_table,
                entity_map,
                import_table,
                scopes,
                go_pkg_index,
            );
        }
    }
}

fn extract_go_import_specs(
    root: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    go_pkg_index: &HashMap<String, Vec<(String, String)>>,
) {
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
                    let pkg_name = path.rsplit('/').next().unwrap_or(path);
                    register_go_package_imports(
                        pkg_name,
                        file_path,
                        symbol_table,
                        entity_map,
                        import_table,
                        scopes,
                        go_pkg_index,
                    );
                }
            } else {
                worklist.push(child);
            }
        }
    }
}

fn register_go_package_imports(
    pkg_name: &str,
    file_path: &str,
    _symbol_table: &HashMap<String, Vec<String>>,
    _entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    go_pkg_index: &HashMap<String, Vec<(String, String)>>,
) {
    // Use pre-built package index for O(1) lookup instead of O(symbol_table) scan
    if let Some(entries) = go_pkg_index.get(pkg_name) {
        for (name, target_id) in entries {
            import_table.insert((file_path.to_string(), name.clone()), target_id.clone());
            if !scopes.is_empty() {
                scopes[0].defs.insert(name.clone(), target_id.clone());
            }
        }
    }
}

/// Shared helper: resolve an imported name against the symbol table
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
) {
    if let Some(target_ids) = symbol_table.get(original_name) {
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
    _scopes: &mut Vec<Scope>,
) {
    let top_level_entities =
        top_level_entities.get_or_init(|| build_top_level_entity_index(symbol_table, entity_map));
    let Some(candidate_file) = find_import_file(
        &top_level_entities.sorted_files,
        source_path,
        file_path,
        extensions,
    ) else {
        return;
    };
    let Some(entries) = top_level_entities.entities_by_file.get(candidate_file) else {
        return;
    };
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
}

fn register_namespace_import(
    alias: &str,
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    _scopes: &mut Vec<Scope>,
) {
    // Find all top-level entities whose file matches the imported module.
    for (name, target_ids) in symbol_table {
        for target_id in target_ids {
            if let Some(info) = entity_map.get(target_id) {
                if import_source_matches_file(file_path, source_path, extensions, &info.file_path)
                    && info.parent_id.is_none()
                {
                    let qualified_name = format!("{alias}.{name}");
                    import_table.insert(
                        (file_path.to_string(), qualified_name.clone()),
                        target_id.clone(),
                    );
                }
            }
        }
    }
}

fn extract_python_import(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    // import_from_statement has:
    //   module_name (dotted_name or relative_import)
    //   name fields (imported names)
    let module_node = node.child_by_field_name("module_name");
    let module_name = module_node
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("");

    // Walk children to find imported names
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
                (orig, alias)
            } else {
                let name = child.utf8_text(source).unwrap_or("");
                (name, name)
            };

            if original.is_empty() {
                continue;
            }

            resolve_import_name(
                original,
                local,
                module_name,
                file_path,
                &[".py"],
                symbol_table,
                entity_map,
                import_table,
                scopes,
            );
        }
    }
}

/// Python: `import mod` or `import mod as m` — registers all entities from
/// the module so that `m.foo()` resolves via the method-call path.
fn extract_python_module_import(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let (module_name, _alias) = match child.kind() {
            "dotted_name" => {
                let name = child.utf8_text(source).unwrap_or("");
                (name, name)
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
                (orig, alias)
            }
            _ => continue,
        };

        if module_name.is_empty() {
            continue;
        }

        // Register all entities from the source module
        register_namespace_import(
            _alias,
            module_name,
            file_path,
            &[".py"],
            symbol_table,
            entity_map,
            import_table,
            scopes,
        );
    }
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
    let mut refs = Vec::new();
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let node_row = node.start_position().row;
        let kind = node.kind();

        // Call nodes (e.g. "call", "call_expression", "method_invocation")
        if config.call_nodes.contains(&kind) {
            match &config.call_style {
                CallNodeStyle::FunctionField(field) => {
                    if let Some(func) = node.child_by_field_name(field) {
                        // Pass empty entity_name — self-ref filtering is done at resolution time
                        extract_call_ref(
                            func, node, "", "", source, &mut refs, config, node_row, None,
                        );
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
                            &mut refs,
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
                            let receiver = obj_node.utf8_text(source).unwrap_or("").to_string();
                            let receiver = receiver.trim_end_matches('.').to_string();
                            refs.push(AstRef {
                                kind: AstRefKind::MethodCall {
                                    receiver,
                                    method: method_name.to_string(),
                                    argument_labels: None,
                                },
                                row: node_row,
                                start_byte: node.start_byte(),
                                end_byte: node.end_byte(),
                            });
                        } else {
                            refs.push(AstRef {
                                kind: AstRefKind::Call {
                                    name: method_name.to_string(),
                                    argument_labels: None,
                                },
                                row: node_row,
                                start_byte: node.start_byte(),
                                end_byte: node.end_byte(),
                            });
                        }
                    }
                }
            }
            push_named_children_rev(&mut worklist, node);
            continue;
        }

        // Macro invocations (Rust: macro_invocation, macro name in "macro" field)
        if kind == "macro_invocation" {
            if let Some(macro_node) = node.child_by_field_name("macro") {
                let macro_name = macro_node.utf8_text(source).unwrap_or("");
                if !macro_name.is_empty() && !is_builtin(macro_name, config) {
                    refs.push(AstRef {
                        kind: AstRefKind::Call {
                            name: macro_name.to_string(),
                            argument_labels: None,
                        },
                        row: macro_node.start_position().row,
                        start_byte: macro_node.start_byte(),
                        end_byte: macro_node.end_byte(),
                    });
                }
            }
            push_named_children_rev(&mut worklist, node);
            continue;
        }

        // New expression nodes (e.g. "new_expression", "object_creation_expression")
        if config.new_expr_nodes.contains(&kind) {
            if let Some(type_node) = node.child_by_field_name(config.new_expr_type_field) {
                let name = type_node.utf8_text(source).unwrap_or("");
                let name = name.rsplit('.').next().unwrap_or(name);
                if !name.is_empty() && !is_builtin(name, config) {
                    refs.push(AstRef {
                        kind: AstRefKind::Call {
                            name: name.to_string(),
                            argument_labels: None,
                        },
                        row: type_node.start_position().row,
                        start_byte: type_node.start_byte(),
                        end_byte: type_node.end_byte(),
                    });
                }
            }
            push_named_children_rev(&mut worklist, node);
            continue;
        }

        // Composite literal nodes (e.g. Go "composite_literal")
        if config.composite_literal_nodes.contains(&kind) {
            if let Some(type_node) = node.child_by_field_name("type") {
                let name = type_node.utf8_text(source).unwrap_or("");
                if name.chars().next().map_or(false, |c| c.is_uppercase())
                    && !is_builtin(name, config)
                {
                    refs.push(AstRef {
                        kind: AstRefKind::Call {
                            name: name.to_string(),
                            argument_labels: None,
                        },
                        row: type_node.start_position().row,
                        start_byte: type_node.start_byte(),
                        end_byte: type_node.end_byte(),
                    });
                }
            }
        }

        // Recurse into children
        push_named_children_rev(&mut worklist, node);
    }
    refs
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
    refs: &mut Vec<AstRef>,
    config: &ScopeResolveConfig,
    row: usize,
    argument_labels: Option<Vec<Option<String>>>,
) {
    let func_kind = func.kind();

    if func_kind == "identifier"
        || func_kind == "simple_identifier"
        || func_kind == "type_identifier"
    {
        let name = func.utf8_text(source).unwrap_or("");
        if !name.is_empty() && name != entity_name && !is_builtin(name, config) {
            refs.push(AstRef {
                kind: AstRefKind::Call {
                    name: name.to_string(),
                    argument_labels,
                },
                row,
                start_byte: ref_node.start_byte(),
                end_byte: ref_node.end_byte(),
            });
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
            let emit_call = |refs: &mut Vec<AstRef>| {
                refs.push(AstRef {
                    kind: AstRefKind::Call {
                        name: method_name.to_string(),
                        argument_labels: None,
                    },
                    row,
                    start_byte: ref_node.start_byte(),
                    end_byte: ref_node.end_byte(),
                });
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
                    refs.push(AstRef {
                        kind: AstRefKind::MethodCall {
                            receiver: receiver_base.to_string(),
                            method: method_name.to_string(),
                            argument_labels: None,
                        },
                        row,
                        start_byte: ref_node.start_byte(),
                        end_byte: ref_node.end_byte(),
                    });
                } else {
                    refs.push(AstRef {
                        kind: AstRefKind::ScopedCall {
                            path: receiver,
                            name: method_name.to_string(),
                        },
                        row,
                        start_byte: ref_node.start_byte(),
                        end_byte: ref_node.end_byte(),
                    });
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
    refs: &mut Vec<AstRef>,
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
    refs: &mut Vec<AstRef>,
    node: tree_sitter::Node,
    row: usize,
    argument_labels: Option<Vec<Option<String>>>,
) {
    refs.push(AstRef {
        kind: AstRefKind::MethodCall {
            receiver: obj.to_string(),
            method: method.to_string(),
            argument_labels,
        },
        row,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    });
}

/// Resolve a single reference against scopes and symbol tables.
fn resolve_ref(
    ast_ref: &AstRef,
    scope_idx: usize,
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
) -> Option<(String, RefType, &'static str)> {
    match &ast_ref.kind {
        AstRefKind::Call {
            name,
            argument_labels,
        } => {
            if is_local_binding_in_scopes_cached(scope_idx, scopes, name, lookup_cache) {
                return None;
            }

            // Swift overload disambiguation needs call-signature data that is only
            // built for Swift sources. For every other language the pre-resolution
            // candidate scan below is inert, yet it scans the global symbol table —
            // in a large monorepo a single common name maps to thousands of entities,
            // so this scan dominates graph resolution. Skip it unless Swift
            // signatures are present.
            if !swift_call_signatures.is_empty() {
                if argument_labels.is_some() {
                    if let Some(target_ids) = symbol_table.get(name.as_str()) {
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
                } else if let Some(target_ids) = symbol_table.get(name.as_str()) {
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
            if let Some(eid) = lookup_scope_chain_cached(scope_idx, scopes, name, lookup_cache) {
                if eid != from_entity_id {
                    return Some((eid, RefType::Calls, "scope_chain"));
                }
            }

            // 2. Check import table. The per-file table only holds this file's
            // imports, so a name lookup suffices — avoiding a (path, name) key string
            // allocated for every reference (millions on a large repo).
            if let Some(target_id) = import_table_by_name.get(name.as_str()) {
                return Some(((*target_id).to_string(), RefType::Calls, "import"));
            }

            // 3. Global symbol table fallback (constructor calls or cross-file functions)
            if let Some(target_ids) = symbol_table.get(name.as_str()) {
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

        AstRefKind::ScopedCall { .. } => None,

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
                        if let Some(class_name) = scopes[idx]
                            .owner_id
                            .as_ref()
                            .and_then(|owner_id| entity_map.get(owner_id))
                            .map(|owner| owner.name.as_str())
                        {
                            if let Some(members) = class_members.get(class_name) {
                                match select_member_candidate(
                                    members,
                                    method,
                                    argument_labels.as_deref(),
                                    swift_call_signatures,
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
                        if let Some(eid) = scopes[idx].defs.get(method.as_str()) {
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
                    find_enclosing_class_cached(scope_idx, scopes, entity_map, lookup_cache);
                if let Some(cn) = class_name {
                    // Look up instance attribute type
                    if let Some(attr_type) = instance_attr_types.get(&(cn, attr_name.to_string())) {
                        if let Some(members) = class_members.get(attr_type.as_str()) {
                            match select_member_candidate(
                                members,
                                method,
                                argument_labels.as_deref(),
                                swift_call_signatures,
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
                        if let Some(attr_type) =
                            instance_attr_types.get(&(var_type, field_part.to_string()))
                        {
                            if let Some(members) = class_members.get(attr_type.as_str()) {
                                match select_member_candidate(
                                    members,
                                    method,
                                    argument_labels.as_deref(),
                                    swift_call_signatures,
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

            // receiver.method() -> look up receiver type, then resolve method
            let receiver_type = if let Some(receiver_type) =
                lookup_type_before_class_scope(scope_idx, scopes, receiver)
            {
                Some(receiver_type)
            } else if allow_implicit_instance_member_receiver
                && is_simple_identifier_name(receiver)
                && !is_local_binding_in_scopes_cached(scope_idx, scopes, receiver, lookup_cache)
            {
                match find_enclosing_class_cached(scope_idx, scopes, entity_map, lookup_cache) {
                    Some(class_name) => instance_attr_types
                        .get(&(class_name, receiver.to_string()))
                        .cloned(),
                    None => None,
                }
            } else {
                None
            };

            if let Some(class_name) = receiver_type {
                if let Some(members) = class_members.get(class_name.as_str()) {
                    match select_member_candidate(
                        members,
                        method,
                        argument_labels.as_deref(),
                        swift_call_signatures,
                    ) {
                        SwiftOverloadSelection::Matched(mid) => {
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
                if let Some(members) = class_members.get(receiver) {
                    if let SwiftOverloadSelection::Matched(mid) = select_member_candidate(
                        members,
                        method,
                        argument_labels.as_deref(),
                        swift_call_signatures,
                    ) {
                        return Some((mid, RefType::Calls, "static_call"));
                    }
                }
            }

            // Inside class methods, unqualified property receivers resolve
            // against the enclosing instance when no local binding shadows them.
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
                    find_enclosing_class_cached(scope_idx, scopes, entity_map, lookup_cache)
                {
                    if let Some(attr_type) =
                        instance_attr_types.get(&(class_name, receiver.to_string()))
                    {
                        if let Some(members) = class_members.get(attr_type.as_str()) {
                            match select_member_candidate(
                                members,
                                method,
                                argument_labels.as_deref(),
                                swift_call_signatures,
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
            // not shadowed by a local binding.
            if !is_local_binding_in_scopes_cached(scope_idx, scopes, receiver, lookup_cache) {
                if let Some(class_id) =
                    lookup_scope_chain_cached(scope_idx, scopes, receiver, lookup_cache)
                {
                    if let Some(info) = entity_map.get(&class_id) {
                        if matches!(info.entity_type.as_str(), "module" | "variable" | "object")
                            && info.name == receiver
                        {
                            if let Some(mid) =
                                lookup_entity_member(owner_members, &class_id, method).or_else(
                                    || lookup_owned_scope_member(scopes, &class_id, method),
                                )
                            {
                                return Some((mid, RefType::Calls, "scope_chain"));
                            }
                        } else if matches!(
                            info.entity_type.as_str(),
                            "class" | "struct" | "interface"
                        ) && info.name == receiver
                        {
                            if let Some(members) = class_members.get(&info.name) {
                                match select_member_candidate(
                                    members,
                                    method,
                                    argument_labels.as_deref(),
                                    swift_call_signatures,
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
            }

            // Fallback: check import table for the receiver
            if !is_local_binding_in_scopes_cached(scope_idx, scopes, receiver, lookup_cache) {
                if let Some(target_id) = import_table_by_name.get(receiver) {
                    if let Some(info) = entity_map.get(*target_id) {
                        if matches!(info.entity_type.as_str(), "class" | "struct") {
                            if let Some(members) = class_members.get(&info.name) {
                                match select_member_candidate(
                                    members,
                                    method,
                                    argument_labels.as_deref(),
                                    swift_call_signatures,
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
            // Try the method name directly in the import table
            if file_path.ends_with(".go") {
                if let Some(target_id) = import_table_by_name.get(method.as_str()) {
                    return Some(((*target_id).to_string(), RefType::Calls, "import"));
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
) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if scopes[idx].kind == "class" {
            if let Some(ref oid) = scopes[idx].owner_id {
                return entity_map.get(oid).map(|e| e.name.clone());
            }
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

fn find_enclosing_class_cached(
    start_scope: usize,
    scopes: &[Scope],
    entity_map: &HashMap<String, EntityInfo>,
    cache: &mut ScopeLookupCache,
) -> Option<String> {
    if let Some(cached) = cache.enclosing_classes.get(&start_scope) {
        return cached.clone();
    }
    let value = find_enclosing_class(start_scope, scopes, entity_map);
    cache.enclosing_classes.insert(start_scope, value.clone());
    value
}

/// Walk up the scope chain looking for a definition.
fn lookup_scope_chain(start_scope: usize, scopes: &[Scope], name: &str) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if let Some(eid) = scopes[idx].defs.get(name) {
            return Some(eid.clone());
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

fn lookup_scope_chain_cached(
    start_scope: usize,
    scopes: &[Scope],
    name: &str,
    cache: &mut ScopeLookupCache,
) -> Option<String> {
    if let Some(cached) = cache
        .defs
        .get(&start_scope)
        .and_then(|scope_cache| scope_cache.get(name))
    {
        return cached.clone();
    }
    let value = lookup_scope_chain(start_scope, scopes, name);
    cache
        .defs
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

    #[test]
    fn resolution_cache_key_includes_resolution_context() {
        let ast_ref = AstRef {
            kind: AstRefKind::Call {
                name: "load".to_string(),
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
                receiver: "client".to_string(),
                method: "load".to_string(),
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
                receiver: "!client".to_string(),
                method: "load".to_string(),
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

        let by_name = deterministic_return_types_by_name(&return_type_map, &symbol_table);

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
                ("alpha".to_string(), second_id),
                ("zeta".to_string(), first_id),
            ])
        );
    }
}
