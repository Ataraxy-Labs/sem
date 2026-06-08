//! Entity dependency graph — cross-file reference extraction.
//!
//! Implements a two-pass approach inspired by arXiv:2601.08773 (Reliable Graph-RAG):
//! Pass 1: Extract all entities, build a symbol table (name → entity ID).
//! Pass 2: For each entity, extract identifier references from its AST subtree,
//!         resolve them against the symbol table to create edges.
//!
//! This enables impact analysis: "if I change entity X, what else is affected?"

use std::borrow::Cow;
use std::collections::{HashMap as StdHashMap, HashSet as StdHashSet};
use std::io::BufRead;
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use regex::Regex;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use serde::{Deserialize, Serialize};

/// Helper macro to select parallel or sequential iteration based on feature flag.
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

use crate::git::types::{FileChange, FileStatus};
use crate::model::entity::SemanticEntity;
use crate::parser::import_resolution::{
    find_import_file, find_import_target, import_source_matches_file, is_js_ts_file,
    js_ts_import_source_files_from_content, js_ts_named_exports_from_content,
    sort_import_candidate_files, JS_TS_EXTENSIONS,
};
use crate::parser::registry::{resolve_go_method_parent_ids, ParserRegistry};
use crate::parser::scope_resolve;

#[cfg(not(test))]
const PARSED_FILE_REUSE_LIMIT: usize = 20_000;
#[cfg(test)]
const PARSED_FILE_REUSE_LIMIT: usize = 8;
#[cfg(not(test))]
const SCOPE_RESOLVE_FILE_CHUNK_SIZE: usize = 5_000;
#[cfg(test)]
const SCOPE_RESOLVE_FILE_CHUNK_SIZE: usize = 3;

#[derive(Clone, Copy)]
struct ChildRange<'a> {
    file_path: &'a str,
    start_line: usize,
    end_line: usize,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
}

fn build_child_ranges_by_parent<'a>(
    entities: &'a [SemanticEntity],
) -> HashMap<&'a str, Vec<ChildRange<'a>>> {
    let entity_by_id: HashMap<&str, &SemanticEntity> = entities
        .iter()
        .map(|entity| (entity.id.as_str(), entity))
        .collect();
    let mut line_starts_by_parent: HashMap<&'a str, Vec<usize>> = HashMap::default();
    let mut child_ranges_by_parent: HashMap<&'a str, Vec<ChildRange<'a>>> = HashMap::default();

    for child in entities {
        let Some(parent_id) = child.parent_id.as_deref() else {
            continue;
        };
        let (start_byte, end_byte) = entity_by_id
            .get(parent_id)
            .and_then(|parent| {
                let parent_line_starts = line_starts_by_parent
                    .entry(parent_id)
                    .or_insert_with(|| line_start_offsets(&parent.content));
                child_content_span_in_parent(parent, child, parent_line_starts)
            })
            .map_or((None, None), |(start, end)| (Some(start), Some(end)));

        child_ranges_by_parent
            .entry(parent_id)
            .or_default()
            .push(ChildRange {
                file_path: child.file_path.as_str(),
                start_line: child.start_line,
                end_line: child.end_line,
                start_byte,
                end_byte,
            });
    }

    for child_ranges in child_ranges_by_parent.values_mut() {
        child_ranges.sort_unstable_by(|left, right| {
            match (left.start_byte, right.start_byte) {
                (Some(left_start), Some(right_start)) => left_start.cmp(&right_start),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| left.end_byte.cmp(&right.end_byte))
            .then_with(|| left.file_path.cmp(right.file_path))
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.end_line.cmp(&right.end_line))
        });
    }

    child_ranges_by_parent
}

fn child_content_span_in_parent(
    parent: &SemanticEntity,
    child: &SemanticEntity,
    parent_line_starts: &[usize],
) -> Option<(usize, usize)> {
    if parent.file_path != child.file_path || child.content.is_empty() {
        return None;
    }

    let expected_local_line = child.start_line.checked_sub(parent.start_line)? + 1;
    if let Some(span) = child_content_span_at_expected_line(
        &parent.content,
        &child.content,
        expected_local_line,
        parent_line_starts,
    ) {
        return Some(span);
    }

    for (offset, _) in parent.content.match_indices(&child.content) {
        let local_line = line_for_byte(&parent.content, offset);
        if local_line == expected_local_line {
            return Some((offset, offset + child.content.len()));
        }
    }

    None
}

fn child_content_span_at_expected_line(
    parent_content: &str,
    child_content: &str,
    expected_local_line: usize,
    parent_line_starts: &[usize],
) -> Option<(usize, usize)> {
    let line_start = *parent_line_starts.get(expected_local_line.checked_sub(1)?)?;
    if let Some(span) = content_span_at(parent_content, child_content, line_start) {
        return Some(span);
    }

    let line_end = parent_line_starts
        .get(expected_local_line)
        .copied()
        .map(|next_line_start| next_line_start.saturating_sub(1))
        .unwrap_or(parent_content.len());
    let line = parent_content.get(line_start..line_end)?;

    let trimmed_line_start = line_start + line.len().saturating_sub(line.trim_start().len());
    if trimmed_line_start != line_start {
        if let Some(span) = content_span_at(parent_content, child_content, trimmed_line_start) {
            return Some(span);
        }
    }

    let first_child_line = child_content
        .split_once('\n')
        .map_or(child_content, |(line, _)| line);
    if first_child_line.is_empty() {
        return None;
    }

    for (candidate_offset, _) in line.match_indices(first_child_line) {
        if let Some(span) =
            content_span_at(parent_content, child_content, line_start + candidate_offset)
        {
            return Some(span);
        }
    }

    None
}

fn content_span_at(content: &str, needle: &str, start: usize) -> Option<(usize, usize)> {
    let end = start.checked_add(needle.len())?;
    (content.get(start..end) == Some(needle)).then_some((start, end))
}

fn entity_owns_content_span(
    entity_id: &str,
    file_path: &str,
    source_line: usize,
    local_start_byte: Option<usize>,
    local_end_byte: Option<usize>,
    child_ranges_by_parent: &HashMap<&str, Vec<ChildRange<'_>>>,
) -> bool {
    let Some(child_ranges) = child_ranges_by_parent.get(entity_id) else {
        return true;
    };

    let child_has_source_line = |child: &ChildRange<'_>| {
        child.file_path == file_path
            && source_line >= child.start_line
            && source_line <= child.end_line
    };

    let first_without_byte = child_ranges.partition_point(|child| child.start_byte.is_some());
    if let (Some(start), Some(end)) = (local_start_byte, local_end_byte) {
        let byte_ranges = &child_ranges[..first_without_byte];
        let possible_end = byte_ranges.partition_point(|child| {
            child
                .start_byte
                .is_some_and(|child_start| child_start < end)
        });
        for child in byte_ranges[..possible_end].iter().rev() {
            let (Some(child_start), Some(child_end)) = (child.start_byte, child.end_byte) else {
                continue;
            };
            if child_end <= start {
                break;
            }
            if start < child_end && end > child_start && child_has_source_line(child) {
                return false;
            }
        }
    } else if child_ranges.iter().any(child_has_source_line) {
        return false;
    }

    !child_ranges[first_without_byte..]
        .iter()
        .any(child_has_source_line)
}

fn source_line_for_entity_content(entity: &SemanticEntity, local_line: usize) -> usize {
    entity.start_line + local_line.saturating_sub(1)
}

fn entity_requires_content_span_filter(
    entity: &SemanticEntity,
    child_ranges_by_parent: &HashMap<&str, Vec<ChildRange<'_>>>,
) -> bool {
    entity.start_line == entity.end_line
        || child_ranges_by_parent
            .get(entity.id.as_str())
            .map_or(false, |children| {
                children.iter().any(|child| {
                    child.start_line == child.end_line
                        || child.start_line == entity.start_line
                        || child.end_line == entity.end_line
                })
            })
}

fn line_for_byte(content: &str, byte: usize) -> usize {
    1 + content[..byte]
        .bytes()
        .filter(|current| *current == b'\n')
        .count()
}

fn line_start_offsets(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

/// A reference from one entity to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityRef {
    pub from_entity: String,
    pub to_entity: String,
    pub ref_type: RefType,
}

/// Type of reference between entities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefType {
    /// Function/method call
    Calls,
    /// Type reference (extends, implements, field type)
    TypeRef,
    /// Import/use statement reference
    Imports,
}

/// A complete entity dependency graph for a set of files.
#[derive(Debug)]
pub struct EntityGraph {
    /// All entities indexed by ID
    pub entities: EntityInfoMap,
    /// Edges: from_entity → [(to_entity, ref_type)]
    pub edges: Vec<EntityRef>,
    /// Reverse index: entity_id → entities that reference it
    pub dependents: EntityAdjacencyMap,
    /// Forward index: entity_id → entities it references
    pub dependencies: EntityAdjacencyMap,
}

/// Metadata describing repairs made during an incremental graph build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalBuildMetadata {
    pub repaired_clean_entity_ids: bool,
    pub recomputed_edge_source_ids: Vec<String>,
    pub deleted_entity_ids: Vec<String>,
}

/// Minimal entity info stored in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityInfo {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}

pub type EntityInfoMap = StdHashMap<String, EntityInfo>;
pub type EntityAdjacencyMap = StdHashMap<String, Vec<String>>;

fn sort_symbol_table_targets_by_source(
    symbol_table: &mut HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) {
    for target_ids in symbol_table.values_mut() {
        if target_ids.len() > 1 {
            target_ids.sort_unstable_by(|left, right| {
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
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => left.cmp(right),
                }
            });
        }
    }
}

fn dedupe_resolved_edges(
    combined: Vec<(String, String, RefType)>,
) -> Vec<(String, String, RefType)> {
    let mut keep = vec![false; combined.len()];
    let mut seen_edges: HashSet<(&str, &str)> =
        HashSet::with_capacity_and_hasher(combined.len(), Default::default());
    for (index, (from_entity, to_entity, _)) in combined.iter().enumerate() {
        if seen_edges.insert((from_entity.as_str(), to_entity.as_str())) {
            keep[index] = true;
        }
    }
    drop(seen_edges);

    combined
        .into_iter()
        .enumerate()
        .filter_map(|(index, edge)| keep[index].then_some(edge))
        .collect()
}

fn sort_resolved_refs(refs: &mut [(String, String, RefType)]) {
    refs.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| ref_type_sort_key(&left.2).cmp(&ref_type_sort_key(&right.2)))
    });
}

fn sort_entity_refs(refs: &mut [EntityRef]) {
    refs.sort_by(|left, right| {
        left.from_entity
            .cmp(&right.from_entity)
            .then_with(|| left.to_entity.cmp(&right.to_entity))
            .then_with(|| {
                ref_type_sort_key(&left.ref_type).cmp(&ref_type_sort_key(&right.ref_type))
            })
    });
}

fn ref_type_sort_key(ref_type: &RefType) -> u8 {
    match ref_type {
        RefType::Calls => 0,
        RefType::Imports => 1,
        RefType::TypeRef => 2,
    }
}

#[derive(Debug)]
struct LineReferenceIndex {
    words: Vec<IndexedWordRef>,
    dot_chains: Vec<(u32, u32)>,
}

#[derive(Debug)]
struct FileReferenceIndex {
    tokens: Vec<String>,
    token_ids: HashMap<String, u32>,
    lines: Vec<Option<LineReferenceIndex>>,
}

#[derive(Debug, Clone, Copy)]
struct IndexedWordRef {
    token_id: u32,
    flags: u8,
}

impl IndexedWordRef {
    const CALL: u8 = 0b01;
    const IMPORT: u8 = 0b10;
}

impl FileReferenceIndex {
    #[cfg(test)]
    fn from_content(content: &str, extra_ident_chars: &'static [char]) -> Self {
        let stripped = strip_comments_and_strings(content);
        Self::from_stripped(&stripped, extra_ident_chars)
    }

    fn from_stripped(stripped: &str, extra_ident_chars: &'static [char]) -> Self {
        let mut index = Self {
            tokens: Vec::new(),
            token_ids: HashMap::default(),
            lines: Vec::new(),
        };
        let lines = stripped
            .lines()
            .map(|line| LineReferenceIndex::from_stripped_line(line, &mut index, extra_ident_chars))
            .collect();
        index.lines = lines;
        index
    }

    fn dot_chains_in_ranges(&self, ranges: &[(usize, usize)]) -> Vec<(&str, &str)> {
        let mut chains = Vec::new();
        let mut seen: HashSet<(u32, u32)> = HashSet::default();
        for &(start_line, end_line) in ranges {
            for line in self.line_range(start_line, end_line) {
                for (receiver, member) in &line.dot_chains {
                    let pair = (*receiver, *member);
                    if seen.insert(pair) {
                        chains.push((self.token(*receiver), self.token(*member)));
                    }
                }
            }
        }
        chains
    }

    fn refs_with_types_in_ranges(
        &self,
        ranges: &[(usize, usize)],
        own_name: &str,
    ) -> Vec<(&str, RefType)> {
        let mut refs = Vec::new();
        let mut seen: HashMap<u32, u8> = HashMap::default();
        for &(start_line, end_line) in ranges {
            for line in self.line_range(start_line, end_line) {
                for word in &line.words {
                    let word_text = self.token(word.token_id);
                    if word_text == own_name {
                        continue;
                    }
                    let first_seen = !seen.contains_key(&word.token_id);
                    let flags = seen.entry(word.token_id).or_insert(0);
                    *flags |= word.flags;
                    if first_seen {
                        refs.push(word.token_id);
                    }
                }
            }
        }
        refs.into_iter()
            .map(|token_id| {
                let flags = seen.get(&token_id).copied().unwrap_or_default();
                let ref_type = if flags & IndexedWordRef::CALL != 0 {
                    RefType::Calls
                } else if flags & IndexedWordRef::IMPORT != 0 {
                    RefType::Imports
                } else {
                    RefType::TypeRef
                };
                (self.token(token_id), ref_type)
            })
            .collect()
    }

    fn line_range(
        &self,
        start_line: usize,
        end_line: usize,
    ) -> impl Iterator<Item = &LineReferenceIndex> {
        let start = start_line.saturating_sub(1).min(self.lines.len());
        let end = end_line.min(self.lines.len()).max(start);
        self.lines[start..end].iter().filter_map(Option::as_ref)
    }

    fn intern(&mut self, token: &str) -> u32 {
        if let Some(id) = self.token_ids.get(token) {
            return *id;
        }
        let id = self.tokens.len() as u32;
        self.tokens.push(token.to_string());
        self.token_ids.insert(token.to_string(), id);
        id
    }

    fn token(&self, token_id: u32) -> &str {
        self.tokens
            .get(token_id as usize)
            .map(String::as_str)
            .unwrap_or("")
    }
}

impl LineReferenceIndex {
    fn from_stripped_line(
        line: &str,
        file_index: &mut FileReferenceIndex,
        extra_ident_chars: &'static [char],
    ) -> Option<Self> {
        let mut words = Vec::new();
        let mut seen_words: HashSet<u32> = HashSet::default();
        let import_like = {
            let trimmed = line.trim();
            trimmed.starts_with("import ")
                || trimmed.starts_with("use ")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("require(")
        };

        for (word, end_byte) in identifier_tokens(line, extra_ident_chars) {
            if !is_reference_word(word) {
                continue;
            }
            let token_id = file_index.intern(word);
            let mut flags = 0;
            if line.as_bytes().get(end_byte) == Some(&b'(') {
                flags |= IndexedWordRef::CALL;
            }
            if import_like {
                flags |= IndexedWordRef::IMPORT;
            }
            if seen_words.insert(token_id) {
                words.push(IndexedWordRef { token_id, flags });
            } else if let Some(indexed) = words
                .iter_mut()
                .find(|indexed| indexed.token_id == token_id)
            {
                indexed.flags |= flags;
            }
        }

        let dot_chains: Vec<(u32, u32)> = extract_dot_chains(line)
            .into_iter()
            .map(|(receiver, member)| (file_index.intern(receiver), file_index.intern(member)))
            .collect();

        if words.is_empty() && dot_chains.is_empty() {
            return None;
        }

        Some(Self { words, dot_chains })
    }
}

fn identifier_tokens<'a>(
    line: &'a str,
    extra_ident_chars: &'static [char],
) -> impl Iterator<Item = (&'a str, usize)> {
    let mut start = None;
    let mut chars = line.char_indices();

    std::iter::from_fn(move || {
        for (idx, ch) in chars.by_ref() {
            if ch.is_alphanumeric() || ch == '_' || extra_ident_chars.contains(&ch) {
                if start.is_none() {
                    start = Some(idx);
                }
            } else if let Some(token_start) = start.take() {
                return Some((&line[token_start..idx], idx));
            }
        }

        start
            .take()
            .map(|token_start| (&line[token_start..], line.len()))
    })
}

fn is_reference_word(word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    if is_keyword(word) || word.len() < 2 {
        return false;
    }
    if word.starts_with(|c: char| c.is_lowercase()) && word.len() < 3 {
        return false;
    }
    // Reject purely symbolic tokens (e.g. `*` used as arithmetic in Clojure).
    // A valid name always contains at least one alphanumeric char or `-`/`_`.
    // Note: extra_ident_chars like `*`, `?`, `!`, `=` are intentionally NOT added
    // to this allowlist because bare `?` or `*` alone are never namespace references.
    // Mixed tokens such as `my-fn?` still pass: their alphanumeric chars make
    // `all()` return false, so we do not reject them.
    if word
        .chars()
        .all(|c| !c.is_alphanumeric() && c != '_' && c != '-')
    {
        return false;
    }
    // Tokens starting with '-' or '*' only appear here for Clojure-family files
    // because `extra_ident_chars_for_file` controls tokenization upstream.
    // '?', '!', '=' only appear as suffixes in symbols such as empty?, reset!,
    // and not=, so the start-character check below suffices.
    if !word.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '-' || c == '*') {
        return false;
    }
    if is_common_local_name(word) {
        return false;
    }
    true
}

fn is_function_like_entity_type(entity_type: &str) -> bool {
    matches!(
        entity_type,
        "function" | "method" | "constructor" | "getter" | "setter"
    )
}

fn fallback_reference_end_line(entity: &SemanticEntity, has_scope_resolve: bool) -> usize {
    if !has_scope_resolve || is_function_like_entity_type(&entity.entity_type) {
        return entity.end_line;
    }

    let mut prefix_lines = 0usize;
    for line in entity.content.lines() {
        prefix_lines += 1;
        let trimmed = line.trim_end();
        if line.contains('{') || trimmed.ends_with(':') || trimmed.ends_with(';') {
            break;
        }
        if prefix_lines >= 16 {
            break;
        }
    }

    (entity.start_line + prefix_lines.saturating_sub(1))
        .min(entity.end_line)
        .max(entity.start_line)
}

fn direct_reference_line_ranges(
    entity: &SemanticEntity,
    fallback_end_line: usize,
    child_line_ranges: &HashMap<String, Vec<(usize, usize)>>,
) -> Vec<(usize, usize)> {
    let start_line = entity.start_line;
    let end_line = fallback_end_line.min(entity.end_line).max(start_line);
    let mut ranges = Vec::new();
    let mut next_line = start_line;

    if let Some(children) = child_line_ranges.get(&entity.id) {
        for &(child_start, child_end) in children {
            if child_end < next_line {
                continue;
            }
            if child_start > end_line {
                break;
            }

            let child_start = child_start.max(start_line);
            let child_end = child_end.min(end_line);
            if next_line < child_start {
                ranges.push((next_line, child_start - 1));
            }
            next_line = next_line.max(child_end.saturating_add(1));
            if next_line > end_line {
                break;
            }
        }
    }

    if next_line <= end_line {
        ranges.push((next_line, end_line));
    }

    ranges
}

type ImportsByFile<'a> = HashMap<&'a str, HashMap<&'a str, &'a str>>;

fn build_imports_by_file<'a>(
    import_table: &'a HashMap<(String, String), String>,
) -> ImportsByFile<'a> {
    let mut imports_by_file: ImportsByFile<'a> = HashMap::default();
    for ((file_path, import_name), target_id) in import_table {
        imports_by_file
            .entry(file_path.as_str())
            .or_default()
            .insert(import_name.as_str(), target_id.as_str());
    }
    imports_by_file
}

struct ReferenceResolutionContext<'a> {
    symbol_table: &'a HashMap<String, Vec<String>>,
    entity_map: &'a HashMap<String, EntityInfo>,
    imports_by_file: &'a ImportsByFile<'a>,
    scope_consumed_words: &'a HashMap<String, HashSet<String>>,
    child_ranges_by_parent: &'a HashMap<&'a str, Vec<ChildRange<'a>>>,
    child_line_ranges: &'a HashMap<String, Vec<(usize, usize)>>,
    parent_child_pairs: &'a HashSet<(&'a str, &'a str)>,
    class_child_names: &'a HashSet<(&'a str, &'a str)>,
    class_entity_files: &'a HashSet<(&'a str, &'a str)>,
    enclosing_class: &'a HashMap<&'a str, &'a str>,
    class_members: &'a HashMap<&'a str, Vec<(&'a str, &'a str)>>,
}

fn resolve_references_with_file_indexes<'a>(
    root: &Path,
    file_paths: &[String],
    all_entities: &'a [SemanticEntity],
    needs_resolution: Option<&HashSet<&'a str>>,
    context: &ReferenceResolutionContext<'a>,
) -> Vec<(String, String, RefType)> {
    let mut entities_by_file: HashMap<&'a str, Vec<&'a SemanticEntity>> = HashMap::default();
    for entity in all_entities {
        if needs_resolution
            .as_ref()
            .is_some_and(|ids| !ids.contains(entity.id.as_str()))
        {
            continue;
        }
        let ext = entity
            .file_path
            .rfind('.')
            .map(|i| &entity.file_path[i..])
            .unwrap_or("");
        if crate::parser::plugins::code::languages::get_language_config(ext).is_none() {
            continue;
        }
        entities_by_file
            .entry(entity.file_path.as_str())
            .or_default()
            .push(entity);
    }

    let mut sorted_file_paths = file_paths.to_vec();
    sorted_file_paths.sort_unstable();
    sorted_file_paths.dedup();

    maybe_par_iter!(sorted_file_paths)
        .filter_map(|file_path| {
            let entities = entities_by_file.get(file_path.as_str())?;
            let needs_index = entities.iter().any(|entity| {
                !entity_requires_content_span_filter(entity, context.child_ranges_by_parent)
            });
            let reference_index = if needs_index {
                build_file_reference_index(root, file_path)
            } else {
                None
            };

            let mut file_edges = Vec::new();
            for entity in entities {
                file_edges.extend(resolve_entity_references(
                    entity,
                    reference_index.as_ref(),
                    context,
                ));
            }
            Some(file_edges)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

fn build_file_reference_index(root: &Path, file_path: &str) -> Option<FileReferenceIndex> {
    let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    let config = crate::parser::plugins::code::languages::get_language_config(ext)?;
    let content = std::fs::read_to_string(root.join(file_path)).ok()?;
    let stripped = strip_for_language(config.strip_strategy(), &content);
    Some(FileReferenceIndex::from_stripped(
        &stripped,
        extra_ident_chars_for_file(file_path),
    ))
}

fn resolve_scopes_in_file_chunks(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    pre_built: &scope_resolve::PreBuiltLookups,
    import_table: &HashMap<(String, String), String>,
) -> (
    Vec<(String, String, RefType)>,
    HashMap<String, HashSet<String>>,
) {
    let mut all_edges = Vec::new();
    let mut all_consumed_words: HashMap<String, HashSet<String>> = HashMap::default();

    for chunk in file_paths.chunks(SCOPE_RESOLVE_FILE_CHUNK_SIZE) {
        if !chunk.iter().any(|file_path| {
            let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
            crate::parser::plugins::code::languages::get_language_config(ext)
                .and_then(|config| config.scope_resolve)
                .is_some()
        }) {
            continue;
        }

        let result = scope_resolve::resolve_with_scopes_full(
            root,
            chunk,
            all_entities,
            entity_map,
            None,
            Some(pre_built),
            Some(import_table),
            false,
        );
        all_edges.extend(result.edges);
        for (entity_id, words) in result.consumed_words {
            all_consumed_words
                .entry(entity_id)
                .or_default()
                .extend(words);
        }
    }

    (all_edges, all_consumed_words)
}

fn resolve_entity_references(
    entity: &SemanticEntity,
    reference_index: Option<&FileReferenceIndex>,
    context: &ReferenceResolutionContext<'_>,
) -> Vec<(String, String, RefType)> {
    let ext = entity
        .file_path
        .rfind('.')
        .map(|i| &entity.file_path[i..])
        .unwrap_or("");
    let Some(language_config) = crate::parser::plugins::code::languages::get_language_config(ext)
    else {
        return vec![];
    };
    let fallback_end_line =
        fallback_reference_end_line(entity, language_config.scope_resolve.is_some());
    let fallback_ranges =
        direct_reference_line_ranges(entity, fallback_end_line, context.child_line_ranges);

    let mut entity_edges = Vec::new();

    let reference_index =
        if entity_requires_content_span_filter(entity, context.child_ranges_by_parent) {
            None
        } else {
            reference_index
        };
    let fallback_stripped = if reference_index.is_none() {
        Some(strip_for_language(
            language_config.strip_strategy(),
            &entity.content,
        ))
    } else {
        None
    };
    let local_bindings =
        local_binding_names_filtered(&entity.content, ext, |local_line, start, end| {
            entity_owns_content_span(
                entity.id.as_str(),
                entity.file_path.as_str(),
                source_line_for_entity_content(entity, local_line),
                Some(start),
                Some(end),
                context.child_ranges_by_parent,
            )
        });

    let dot_chains: Vec<(&str, &str, Option<(usize, usize, usize)>)> = match reference_index {
        Some(index) => index
            .dot_chains_in_ranges(&fallback_ranges)
            .into_iter()
            .map(|(receiver, member)| (receiver, member, None))
            .collect(),
        None => extract_dot_chains_with_positions(fallback_stripped.as_ref().unwrap())
            .into_iter()
            .map(|(receiver, member, line, start, end)| {
                (receiver, member, Some((line, start, end)))
            })
            .collect(),
    };
    let mut consumed_words: HashSet<&str> = context
        .scope_consumed_words
        .get(&entity.id)
        .map(|set| set.iter().map(String::as_str).collect())
        .unwrap_or_default();

    for (receiver, member, position) in &dot_chains {
        if consumed_words.contains(*member) {
            continue;
        }
        if let Some((local_line, local_start_byte, local_end_byte)) = *position {
            if !entity_owns_content_span(
                entity.id.as_str(),
                entity.file_path.as_str(),
                source_line_for_entity_content(entity, local_line),
                Some(local_start_byte),
                Some(local_end_byte),
                context.child_ranges_by_parent,
            ) {
                continue;
            }
        }
        let edge_count_before = entity_edges.len();
        if *receiver == "self" || *receiver == "this" {
            if let Some(class_name) = context.enclosing_class.get(entity.id.as_str()) {
                if let Some(members) = context.class_members.get(class_name) {
                    for (name, target_id) in members {
                        if *name == *member && *target_id != entity.id.as_str() {
                            entity_edges.push((
                                entity.id.clone(),
                                target_id.to_string(),
                                RefType::Calls,
                            ));
                            consumed_words.insert(*member);
                            break;
                        }
                    }
                }
            }
        } else if context
            .class_entity_files
            .contains(&(*receiver, entity.file_path.as_str()))
        {
            if let Some(members) = context.class_members.get(*receiver) {
                for (name, target_id) in members {
                    if *name == *member {
                        entity_edges.push((
                            entity.id.clone(),
                            target_id.to_string(),
                            RefType::Calls,
                        ));
                        consumed_words.insert(*member);
                        consumed_words.insert(*receiver);
                        break;
                    }
                }
            }
        }
        if entity_edges.len() == edge_count_before {
            consumed_words.insert(*member);
        }
    }

    let refs: Vec<(&str, RefType)> = match reference_index {
        Some(index) => index.refs_with_types_in_ranges(&fallback_ranges, &entity.name),
        None => {
            let stripped = fallback_stripped.as_ref().unwrap();
            extract_references_with_stripped_filtered(
                &entity.content,
                &entity.name,
                stripped,
                extra_ident_chars_for_file(&entity.file_path),
                |local_line, local_start_byte, local_end_byte| {
                    entity_owns_content_span(
                        entity.id.as_str(),
                        entity.file_path.as_str(),
                        source_line_for_entity_content(entity, local_line),
                        Some(local_start_byte),
                        Some(local_end_byte),
                        context.child_ranges_by_parent,
                    )
                },
            )
            .into_iter()
            .map(|ref_name| (ref_name, infer_ref_type(&entity.content, ref_name)))
            .collect()
        }
    };
    let entity_id = entity.id.as_str();
    let imports_for_file = context.imports_by_file.get(entity.file_path.as_str());

    for (ref_name, ref_type) in refs {
        if consumed_words.contains(ref_name) {
            continue;
        }
        if local_bindings.contains(ref_name) {
            continue;
        }

        if context
            .class_child_names
            .contains(&(entity.id.as_str(), ref_name))
        {
            continue;
        }

        if let Some(import_target_id) =
            imports_for_file.and_then(|imports| imports.get(ref_name).copied())
        {
            if import_target_id != entity_id
                && !context
                    .parent_child_pairs
                    .contains(&(entity_id, import_target_id))
                && !context
                    .parent_child_pairs
                    .contains(&(import_target_id, entity_id))
            {
                entity_edges.push((entity.id.clone(), import_target_id.to_string(), ref_type));
            }
            continue;
        }

        if let Some(target_ids) = context.symbol_table.get(ref_name) {
            let target = target_ids.iter().find(|id| {
                *id != &entity.id
                    && context
                        .entity_map
                        .get(*id)
                        .map_or(false, |e| e.file_path == entity.file_path)
            });

            if let Some(target_id) = target {
                if context
                    .parent_child_pairs
                    .contains(&(entity.id.as_str(), target_id.as_str()))
                    || context
                        .parent_child_pairs
                        .contains(&(target_id.as_str(), entity.id.as_str()))
                {
                    continue;
                }
                entity_edges.push((entity.id.clone(), target_id.clone(), ref_type));
            }
        }
    }

    // Resolve namespace-qualified calls (alias/name) for languages that use this pattern.
    // The regular tokenizer splits `alias/name` at the slash, so bare `name` tokens don't
    // match cross-file entities via the symbol table. We scan the stripped content for
    // `alias/name` patterns and resolve them via the per-file import map.
    if language_config.has_slash_qualified_refs() {
        // Always restrip via the language's own strategy: fallback_stripped may have been
        // computed with a different strategy when reference_index was non-None above.
        let qualified_ref_stripped =
            strip_for_language(language_config.strip_strategy(), &entity.content);
        for cap in CLOJURE_QUALIFIED_REF_RE.captures_iter(&qualified_ref_stripped) {
            let qualified = cap.get(1).unwrap().as_str();
            if let Some(import_target_id) =
                imports_for_file.and_then(|imports| imports.get(qualified).copied())
            {
                if import_target_id != entity_id
                    && !context
                        .parent_child_pairs
                        .contains(&(entity_id, import_target_id))
                    && !context
                        .parent_child_pairs
                        .contains(&(import_target_id, entity_id))
                {
                    entity_edges.push((
                        entity.id.clone(),
                        import_target_id.to_string(),
                        RefType::Calls,
                    ));
                }
            }
        }
    }

    entity_edges
}

impl EntityGraph {
    /// Reconstruct an EntityGraph from pre-loaded parts (e.g. from a cache).
    pub fn from_parts(entities: EntityInfoMap, mut edges: Vec<EntityRef>) -> Self {
        sort_entity_refs(&mut edges);
        let mut dependents: EntityAdjacencyMap = StdHashMap::new();
        let mut dependencies: EntityAdjacencyMap = StdHashMap::new();
        for edge in &edges {
            dependents
                .entry(edge.to_entity.clone())
                .or_default()
                .push(edge.from_entity.clone());
            dependencies
                .entry(edge.from_entity.clone())
                .or_default()
                .push(edge.to_entity.clone());
        }
        EntityGraph {
            entities,
            edges,
            dependents,
            dependencies,
        }
    }

    /// Build an entity graph from a set of files.
    ///
    /// Pass 1: Extract all entities from all files using the parser registry.
    /// Pass 2: For each entity, find identifier tokens and resolve them against
    ///         the symbol table to create reference edges.
    pub fn build(
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
    ) -> (Self, Vec<SemanticEntity>) {
        let retain_parsed_files = file_paths.len() <= PARSED_FILE_REUSE_LIMIT;
        // Pass 1: Extract all entities in parallel (file I/O + tree-sitter parsing)
        // Small and medium repos reuse parse trees in scope resolution; large repos
        // keep peak memory bounded by reparsing scope chunks.
        let per_file: Vec<(
            Vec<SemanticEntity>,
            Option<(String, String, tree_sitter::Tree)>,
        )> = maybe_par_iter!(file_paths)
            .filter_map(|file_path| {
                let full_path = root.join(file_path);
                let content = std::fs::read_to_string(&full_path).ok()?;
                if retain_parsed_files {
                    let (entities, tree) =
                        registry.extract_entities_with_tree(file_path, &content)?;
                    let parsed = tree.map(|tree| (file_path.clone(), content, tree));
                    Some((entities, parsed))
                } else {
                    let entities = registry.extract_entities(file_path, &content);
                    Some((entities, None))
                }
            })
            .collect();

        let mut all_entities: Vec<SemanticEntity> = Vec::new();
        let mut parsed_files: Vec<(String, String, tree_sitter::Tree)> = Vec::new();
        for (entities, parsed) in per_file {
            all_entities.extend(entities);
            if let Some(p) = parsed {
                parsed_files.push(p);
            }
        }
        resolve_go_method_parent_ids(&mut all_entities);

        // Pass A: Build all lookup structures in a single pass over all_entities.
        // This merges what was previously 6 separate O(E) iterations.
        let mut symbol_table: HashMap<String, Vec<String>> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());
        let mut entity_map: HashMap<String, EntityInfo> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());
        let mut parent_child_pairs: HashSet<(&str, &str)> = HashSet::default();
        let mut child_line_ranges: HashMap<String, Vec<(usize, usize)>> = HashMap::default();
        let mut class_child_names: HashSet<(&str, &str)> = HashSet::default();
        let child_ranges_by_parent = build_child_ranges_by_parent(&all_entities);
        let mut class_entity_names: HashSet<&str> = HashSet::default();
        let mut class_entity_files: HashSet<(&str, &str)> = HashSet::default();
        let mut id_to_name: HashMap<&str, &str> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());
        let mut scope_entity_ranges: HashMap<String, Vec<(usize, usize, String)>> =
            HashMap::default();

        for entity in &all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());

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
                parent_child_pairs.insert((pid.as_str(), entity.id.as_str()));
                child_line_ranges
                    .entry(pid.clone())
                    .or_default()
                    .push((entity.start_line, entity.end_line));
                class_child_names.insert((pid.as_str(), entity.name.as_str()));
            }

            if is_nominal_member_container(entity.entity_type.as_str()) {
                class_entity_names.insert(entity.name.as_str());
                class_entity_files.insert((entity.name.as_str(), entity.file_path.as_str()));
            }

            id_to_name.insert(entity.id.as_str(), entity.name.as_str());

            scope_entity_ranges
                .entry(entity.file_path.clone())
                .or_default()
                .push((entity.start_line, entity.end_line, entity.id.clone()));
        }
        for ranges in child_line_ranges.values_mut() {
            ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
        }

        // Pass B: Build enclosing_class, class_members, and scope_class_members
        // (depends on id_to_name, class_entity_names, and entity_map from Pass A)
        let mut enclosing_class: HashMap<&str, &str> = HashMap::default();
        let mut class_members: HashMap<&str, Vec<(&str, &str)>> = HashMap::default();
        let mut scope_class_members: HashMap<String, Vec<(String, String)>> = HashMap::default();
        let mut scope_owner_members: HashMap<String, Vec<(String, String)>> = HashMap::default();

        for entity in &all_entities {
            if let Some(ref pid) = entity.parent_id {
                scope_owner_members
                    .entry(pid.clone())
                    .or_default()
                    .push((entity.name.clone(), entity.id.clone()));
                if let Some(&parent_name) = id_to_name.get(pid.as_str()) {
                    if class_entity_names.contains(parent_name) {
                        enclosing_class.insert(entity.id.as_str(), parent_name);
                        class_members
                            .entry(parent_name)
                            .or_default()
                            .push((entity.name.as_str(), entity.id.as_str()));
                    }
                }
                // scope_class_members for scope resolver (checks entity_type of parent)
                if let Some(parent) = entity_map.get(pid.as_str()) {
                    if let Some(owner_name) = scope_resolve::class_member_owner_name(parent) {
                        scope_class_members
                            .entry(owner_name.to_string())
                            .or_default()
                            .push((entity.name.clone(), entity.id.clone()));
                    }
                }
            }
            // Go receiver-based methods
            if entity.entity_type == "method" && entity.file_path.ends_with(".go") {
                if let Some(struct_name) = scope_resolve::extract_go_receiver_type(&entity.content)
                {
                    scope_class_members
                        .entry(struct_name)
                        .or_default()
                        .push((entity.name.clone(), entity.id.clone()));
                }
            }
        }
        sort_symbol_table_targets_by_source(&mut symbol_table, &entity_map);
        let symbol_table = Arc::new(symbol_table);

        // Build import table: (file_path, imported_name) → target entity ID
        // e.g. ("io_handler.py", "validate") → "core.py::function::validate"
        let import_table = build_import_table(
            root,
            file_paths,
            &symbol_table,
            &entity_map,
            retain_parsed_files.then_some(parsed_files.as_slice()),
        );
        // Build owned Go package index for scope resolver
        let owned_go_pkg_index: HashMap<String, Vec<(String, String)>> =
            if file_paths.iter().any(|f| f.ends_with(".go")) {
                let mut idx: HashMap<String, Vec<(String, String)>> = HashMap::default();
                for (name, target_ids) in symbol_table.iter() {
                    for target_id in target_ids {
                        if let Some(entity) = entity_map.get(target_id) {
                            let file_stem = entity
                                .file_path
                                .rsplit('/')
                                .next()
                                .unwrap_or(&entity.file_path);
                            let file_stem = strip_file_ext(file_stem);
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
            } else {
                HashMap::default()
            };

        let pre_built = scope_resolve::PreBuiltLookups {
            symbol_table: Arc::clone(&symbol_table),
            class_members: scope_class_members,
            owner_members: scope_owner_members,
            entity_ranges: scope_entity_ranges,
            go_pkg_index: owned_go_pkg_index,
        };

        // Run scope-aware resolver for supported languages (reuse pre-parsed trees)
        let has_scope_lang = file_paths.iter().any(|f| {
            let ext = f.rfind('.').map(|i| &f[i..]).unwrap_or("");
            crate::parser::plugins::code::languages::get_language_config(ext)
                .and_then(|c| c.scope_resolve)
                .is_some()
        });
        let (scope_edges, scope_consumed_words) = if has_scope_lang && retain_parsed_files {
            let result = scope_resolve::resolve_with_scopes_full(
                root,
                file_paths,
                &all_entities,
                &entity_map,
                Some(parsed_files),
                Some(&pre_built),
                Some(&import_table),
                false,
            );
            (result.edges, result.consumed_words)
        } else if has_scope_lang {
            resolve_scopes_in_file_chunks(
                root,
                file_paths,
                &all_entities,
                &entity_map,
                &pre_built,
                &import_table,
            )
        } else {
            (vec![], HashMap::default())
        };

        let imports_by_file = build_imports_by_file(&import_table);
        let reference_context = ReferenceResolutionContext {
            symbol_table: symbol_table.as_ref(),
            entity_map: &entity_map,
            imports_by_file: &imports_by_file,
            scope_consumed_words: &scope_consumed_words,
            child_ranges_by_parent: &child_ranges_by_parent,
            child_line_ranges: &child_line_ranges,
            parent_child_pairs: &parent_child_pairs,
            class_child_names: &class_child_names,
            class_entity_files: &class_entity_files,
            enclosing_class: &enclosing_class,
            class_members: &class_members,
        };
        let resolved_refs = resolve_references_with_file_indexes(
            root,
            file_paths,
            &all_entities,
            None,
            &reference_context,
        );

        let export_edges = build_export_alias_edges(&all_entities, &import_table);

        // Merge scope edges with bag-of-words edges, deduplicating
        let mut combined: Vec<(String, String, RefType)> = scope_edges;
        combined.extend(export_edges);
        combined.extend(resolved_refs);
        let mut all_resolved = dedupe_resolved_edges(combined);
        sort_resolved_refs(&mut all_resolved);

        // Build edge indexes from resolved references
        let mut edges: Vec<EntityRef> = Vec::with_capacity(all_resolved.len());
        let mut dependents: HashMap<String, Vec<String>> = HashMap::default();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::default();

        for (from_entity, to_entity, ref_type) in all_resolved {
            dependents
                .entry(to_entity.clone())
                .or_default()
                .push(from_entity.clone());
            dependencies
                .entry(from_entity.clone())
                .or_default()
                .push(to_entity.clone());
            edges.push(EntityRef {
                from_entity,
                to_entity,
                ref_type,
            });
        }

        let graph = EntityGraph {
            entities: entity_map.into_iter().collect(),
            edges,
            dependents: dependents.into_iter().collect(),
            dependencies: dependencies.into_iter().collect(),
        };

        (graph, all_entities)
    }

    /// Build an entity graph containing dependency edges for selected entities.
    ///
    /// The graph includes every entity so callers can perform the same lookup and
    /// ambiguity checks as a full graph build. Only selected entities contribute
    /// outgoing dependency edges.
    pub fn build_direct_dependencies<F>(
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
        mut should_resolve: F,
    ) -> (Self, Vec<SemanticEntity>)
    where
        F: FnMut(&EntityInfo) -> bool,
    {
        let retain_parsed_files = file_paths.len() <= PARSED_FILE_REUSE_LIMIT;
        let per_file: Vec<(
            Vec<SemanticEntity>,
            Option<(String, String, tree_sitter::Tree)>,
        )> = maybe_par_iter!(file_paths)
            .filter_map(|file_path| {
                let content = std::fs::read_to_string(root.join(file_path)).ok()?;
                if retain_parsed_files {
                    let (entities, tree) =
                        registry.extract_entities_with_tree(file_path, &content)?;
                    let parsed = tree.map(|tree| (file_path.clone(), content, tree));
                    Some((entities, parsed))
                } else {
                    Some((registry.extract_entities(file_path, &content), None))
                }
            })
            .collect();

        let mut all_entities: Vec<SemanticEntity> = Vec::new();
        let mut retained_parsed_files: Vec<(String, String, tree_sitter::Tree)> = Vec::new();
        for (entities, parsed) in per_file {
            all_entities.extend(entities);
            if let Some(parsed) = parsed {
                retained_parsed_files.push(parsed);
            }
        }
        resolve_go_method_parent_ids(&mut all_entities);

        let mut symbol_table: HashMap<String, Vec<String>> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());
        let mut entity_map: HashMap<String, EntityInfo> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());
        let mut parent_child_pairs: HashSet<(&str, &str)> = HashSet::default();
        let mut child_line_ranges: HashMap<String, Vec<(usize, usize)>> = HashMap::default();
        let mut class_child_names: HashSet<(&str, &str)> = HashSet::default();
        let child_ranges_by_parent = build_child_ranges_by_parent(&all_entities);
        let mut class_entity_names: HashSet<&str> = HashSet::default();
        let mut class_entity_files: HashSet<(&str, &str)> = HashSet::default();
        let mut id_to_name: HashMap<&str, &str> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());
        let mut scope_entity_ranges: HashMap<String, Vec<(usize, usize, String)>> =
            HashMap::default();

        for entity in &all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());

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
                parent_child_pairs.insert((pid.as_str(), entity.id.as_str()));
                child_line_ranges
                    .entry(pid.clone())
                    .or_default()
                    .push((entity.start_line, entity.end_line));
                class_child_names.insert((pid.as_str(), entity.name.as_str()));
            }

            if is_nominal_member_container(entity.entity_type.as_str()) {
                class_entity_names.insert(entity.name.as_str());
                class_entity_files.insert((entity.name.as_str(), entity.file_path.as_str()));
            }

            id_to_name.insert(entity.id.as_str(), entity.name.as_str());

            scope_entity_ranges
                .entry(entity.file_path.clone())
                .or_default()
                .push((entity.start_line, entity.end_line, entity.id.clone()));
        }
        for ranges in child_line_ranges.values_mut() {
            ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
        }

        let mut enclosing_class: HashMap<&str, &str> = HashMap::default();
        let mut class_members: HashMap<&str, Vec<(&str, &str)>> = HashMap::default();
        let mut scope_class_members: HashMap<String, Vec<(String, String)>> = HashMap::default();
        let mut scope_owner_members: HashMap<String, Vec<(String, String)>> = HashMap::default();

        for entity in &all_entities {
            if let Some(ref pid) = entity.parent_id {
                scope_owner_members
                    .entry(pid.clone())
                    .or_default()
                    .push((entity.name.clone(), entity.id.clone()));
                if let Some(&parent_name) = id_to_name.get(pid.as_str()) {
                    if class_entity_names.contains(parent_name) {
                        enclosing_class.insert(entity.id.as_str(), parent_name);
                        class_members
                            .entry(parent_name)
                            .or_default()
                            .push((entity.name.as_str(), entity.id.as_str()));
                    }
                }
                if let Some(parent) = entity_map.get(pid.as_str()) {
                    if let Some(owner_name) = scope_resolve::class_member_owner_name(parent) {
                        scope_class_members
                            .entry(owner_name.to_string())
                            .or_default()
                            .push((entity.name.clone(), entity.id.clone()));
                    }
                }
            }
            if entity.entity_type == "method" && entity.file_path.ends_with(".go") {
                if let Some(struct_name) = scope_resolve::extract_go_receiver_type(&entity.content)
                {
                    scope_class_members
                        .entry(struct_name)
                        .or_default()
                        .push((entity.name.clone(), entity.id.clone()));
                }
            }
        }
        sort_symbol_table_targets_by_source(&mut symbol_table, &entity_map);
        let symbol_table = Arc::new(symbol_table);

        let mut needs_resolution: HashSet<String> = HashSet::default();
        let mut resolve_file_paths: Vec<String> = Vec::new();
        let mut resolve_file_set: HashSet<String> = HashSet::default();
        let mut entity_ids: Vec<&String> = entity_map.keys().collect();
        entity_ids.sort_unstable();
        for entity_id in entity_ids {
            let Some(entity) = entity_map.get(entity_id) else {
                continue;
            };
            if should_resolve(entity) {
                needs_resolution.insert(entity.id.clone());
                if resolve_file_set.insert(entity.file_path.clone()) {
                    resolve_file_paths.push(entity.file_path.clone());
                }
            }
        }
        resolve_file_paths.sort_unstable();

        if needs_resolution.is_empty() {
            return (
                EntityGraph {
                    entities: entity_map.into_iter().collect(),
                    edges: Vec::new(),
                    dependents: StdHashMap::new(),
                    dependencies: StdHashMap::new(),
                },
                all_entities,
            );
        }

        let scope_file_paths = if file_paths.len() > PARSED_FILE_REUSE_LIMIT {
            let mut scoped = Vec::new();
            for chunk in file_paths.chunks(SCOPE_RESOLVE_FILE_CHUNK_SIZE) {
                if chunk.iter().any(|file| resolve_file_set.contains(file)) {
                    scoped.extend(chunk.iter().cloned());
                }
            }
            scoped
        } else {
            file_paths.to_vec()
        };
        let has_scope_lang = resolve_file_paths.iter().any(|f| {
            let ext = f.rfind('.').map(|i| &f[i..]).unwrap_or("");
            crate::parser::plugins::code::languages::get_language_config(ext)
                .and_then(|c| c.scope_resolve)
                .is_some()
        });
        let parsed_files: Vec<(String, String, tree_sitter::Tree)> = if !has_scope_lang {
            Vec::new()
        } else if !retained_parsed_files.is_empty() && scope_file_paths.len() == file_paths.len() {
            retained_parsed_files
        } else {
            maybe_par_iter!(&scope_file_paths)
                .filter_map(|file_path| {
                    let content = std::fs::read_to_string(root.join(file_path)).ok()?;
                    let (_entities, tree) =
                        registry.extract_entities_with_tree(file_path, &content)?;
                    tree.map(|tree| (file_path.clone(), content, tree))
                })
                .collect()
        };

        let import_table = build_import_table_with_default_export_paths(
            root,
            &resolve_file_paths,
            file_paths,
            &symbol_table,
            &entity_map,
            Some(parsed_files.as_slice()),
        );

        let owned_go_pkg_index: HashMap<String, Vec<(String, String)>> =
            if resolve_file_paths.iter().any(|f| f.ends_with(".go")) {
                scope_resolve::build_go_pkg_index(&symbol_table, &entity_map)
            } else {
                HashMap::default()
            };

        let pre_built = scope_resolve::PreBuiltLookups {
            symbol_table: Arc::clone(&symbol_table),
            class_members: scope_class_members,
            owner_members: scope_owner_members,
            entity_ranges: scope_entity_ranges,
            go_pkg_index: owned_go_pkg_index,
        };

        let needs_resolution_refs: HashSet<&str> =
            needs_resolution.iter().map(String::as_str).collect();
        let (scope_edges, scope_consumed_words) = if has_scope_lang {
            let result = scope_resolve::resolve_with_scopes_full_for_entities(
                root,
                &scope_file_paths,
                &all_entities,
                &entity_map,
                (!parsed_files.is_empty()).then_some(parsed_files),
                Some(&pre_built),
                Some(&import_table),
                &needs_resolution_refs,
            );
            (result.edges, result.consumed_words)
        } else {
            (vec![], HashMap::default())
        };

        let imports_by_file = build_imports_by_file(&import_table);
        let reference_context = ReferenceResolutionContext {
            symbol_table: symbol_table.as_ref(),
            entity_map: &entity_map,
            imports_by_file: &imports_by_file,
            scope_consumed_words: &scope_consumed_words,
            child_ranges_by_parent: &child_ranges_by_parent,
            child_line_ranges: &child_line_ranges,
            parent_child_pairs: &parent_child_pairs,
            class_child_names: &class_child_names,
            class_entity_files: &class_entity_files,
            enclosing_class: &enclosing_class,
            class_members: &class_members,
        };
        let resolved_refs = resolve_references_with_file_indexes(
            root,
            &resolve_file_paths,
            &all_entities,
            Some(&needs_resolution_refs),
            &reference_context,
        );

        let export_edges = build_export_alias_edges(&all_entities, &import_table)
            .into_iter()
            .filter(|(from_entity, _, _)| needs_resolution.contains(from_entity))
            .collect::<Vec<_>>();

        let mut combined: Vec<(String, String, RefType)> = scope_edges
            .into_iter()
            .filter(|(from_entity, _, _)| needs_resolution.contains(from_entity))
            .collect();
        combined.extend(export_edges);
        combined.extend(resolved_refs);
        let mut all_resolved = dedupe_resolved_edges(combined);
        sort_resolved_refs(&mut all_resolved);

        let mut edges: Vec<EntityRef> = Vec::with_capacity(all_resolved.len());
        let mut dependents: HashMap<String, Vec<String>> = HashMap::default();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::default();

        for (from_entity, to_entity, ref_type) in all_resolved {
            dependents
                .entry(to_entity.clone())
                .or_default()
                .push(from_entity.clone());
            dependencies
                .entry(from_entity.clone())
                .or_default()
                .push(to_entity.clone());
            edges.push(EntityRef {
                from_entity,
                to_entity,
                ref_type,
            });
        }

        (
            EntityGraph {
                entities: entity_map.into_iter().collect(),
                edges,
                dependents: dependents.into_iter().collect(),
                dependencies: dependencies.into_iter().collect(),
            },
            all_entities,
        )
    }

    /// Incrementally build an entity graph: reparse only stale files, reuse cached data for clean files.
    ///
    /// Uses the same full 3-phase resolution (scope + dot-chain + bag-of-words) as `build()`,
    /// but only runs it for entities in stale files + clean entities whose cached edges
    /// pointed into stale files (they need re-resolution since their targets may have changed).
    pub fn build_incremental(
        root: &Path,
        stale_files: &[String],
        all_file_paths: &[String],
        cached_entities: Vec<SemanticEntity>,
        cached_edges: Vec<EntityRef>,
        stale_file_cached_entities: Vec<SemanticEntity>,
        registry: &ParserRegistry,
    ) -> (Self, Vec<SemanticEntity>) {
        let (graph, entities, _) = Self::build_incremental_with_metadata(
            root,
            stale_files,
            all_file_paths,
            cached_entities,
            cached_edges,
            stale_file_cached_entities,
            registry,
        );
        (graph, entities)
    }

    pub fn build_incremental_with_metadata(
        root: &Path,
        stale_files: &[String],
        all_file_paths: &[String],
        cached_entities: Vec<SemanticEntity>,
        cached_edges: Vec<EntityRef>,
        stale_file_cached_entities: Vec<SemanticEntity>,
        registry: &ParserRegistry,
    ) -> (Self, Vec<SemanticEntity>, IncrementalBuildMetadata) {
        Self::build_incremental_with_metadata_and_import_candidates(
            root,
            stale_files,
            all_file_paths,
            cached_entities,
            cached_edges,
            stale_file_cached_entities,
            None,
            registry,
        )
    }

    pub fn build_incremental_with_metadata_and_import_candidates(
        root: &Path,
        stale_files: &[String],
        all_file_paths: &[String],
        cached_entities: Vec<SemanticEntity>,
        cached_edges: Vec<EntityRef>,
        stale_file_cached_entities: Vec<SemanticEntity>,
        cached_importing_stale_files: Option<&[String]>,
        registry: &ParserRegistry,
    ) -> (Self, Vec<SemanticEntity>, IncrementalBuildMetadata) {
        // Build set of stale file paths for quick lookup
        let stale_set: HashSet<&str> = stale_files.iter().map(|s| s.as_str()).collect();

        // Parse stale files in parallel to get new entities + trees
        let per_file: Vec<(
            Vec<SemanticEntity>,
            Option<(String, String, tree_sitter::Tree)>,
        )> = maybe_par_iter!(stale_files)
            .filter_map(|file_path| {
                let full_path = root.join(file_path);
                let content = std::fs::read_to_string(&full_path).ok()?;
                let (entities, tree) = registry.extract_entities_with_tree(file_path, &content)?;
                let parsed = tree.map(|t| (file_path.clone(), content, t));
                Some((entities, parsed))
            })
            .collect();

        let mut new_entities: Vec<SemanticEntity> = Vec::new();
        let mut parsed_files: Vec<(String, String, tree_sitter::Tree)> = Vec::new();
        for (entities, parsed) in per_file {
            new_entities.extend(entities);
            if let Some(p) = parsed {
                parsed_files.push(p);
            }
        }

        // Merge clean cached entities with newly parsed stale-file entities before
        // repairing Go method parents; Go receiver types may live in clean files.
        let mut all_entities: Vec<SemanticEntity> = cached_entities
            .into_iter()
            .chain(new_entities.into_iter())
            .collect();
        let entity_ids_before_parent_repair: HashSet<String> =
            all_entities.iter().map(|e| e.id.clone()).collect();
        resolve_go_method_parent_ids(&mut all_entities);
        let parent_repaired_ids: HashSet<&str> = all_entities
            .iter()
            .filter(|e| !entity_ids_before_parent_repair.contains(&e.id))
            .map(|e| e.id.as_str())
            .collect();
        let repaired_clean_entity_ids = all_entities.iter().any(|e| {
            parent_repaired_ids.contains(e.id.as_str()) && !stale_set.contains(e.file_path.as_str())
        });

        // Entity-level diffing: compare repaired stale-file entities against cached versions.
        let stale_cached_entity_ids: HashSet<&str> = stale_file_cached_entities
            .iter()
            .map(|e| e.id.as_str())
            .collect();

        // Build content_hash lookup from cached stale-file entities
        let cached_hashes: HashMap<&str, &str> = stale_file_cached_entities
            .iter()
            .map(|e| (e.id.as_str(), e.content_hash.as_str()))
            .collect();

        // Classify new stale-file entities
        let mut truly_changed_ids: HashSet<String> = HashSet::default();
        let mut content_clean_ids: HashSet<String> = HashSet::default();
        for entity in all_entities
            .iter()
            .filter(|e| stale_set.contains(e.file_path.as_str()))
        {
            match cached_hashes.get(entity.id.as_str()) {
                Some(old_hash) if *old_hash == entity.content_hash.as_str() => {
                    content_clean_ids.insert(entity.id.clone());
                }
                _ => {
                    // Hash differs or entity is new
                    truly_changed_ids.insert(entity.id.clone());
                }
            }
        }

        // Detect deleted entities: in cached stale but not in new
        let new_entity_ids: HashSet<&str> = all_entities
            .iter()
            .filter(|e| stale_set.contains(e.file_path.as_str()))
            .map(|e| e.id.as_str())
            .collect();
        let deleted_ids: HashSet<&str> = stale_file_cached_entities
            .iter()
            .filter(|e| !new_entity_ids.contains(e.id.as_str()))
            .map(|e| e.id.as_str())
            .collect();

        let mut symbol_table: HashMap<String, Vec<String>> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());
        let mut entity_map: HashMap<String, EntityInfo> =
            HashMap::with_capacity_and_hasher(all_entities.len(), Default::default());

        for entity in &all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());
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
        }
        sort_symbol_table_targets_by_source(&mut symbol_table, &entity_map);
        let symbol_table = Arc::new(symbol_table);

        let entity_file_paths: HashMap<&str, &str> = all_entities
            .iter()
            .map(|e| (e.id.as_str(), e.file_path.as_str()))
            .collect();
        let stale_entity_ids: HashSet<&str> = all_entities
            .iter()
            .filter(|e| stale_set.contains(e.file_path.as_str()))
            .map(|e| e.id.as_str())
            .collect();
        let current_entity_ids: HashSet<&str> =
            all_entities.iter().map(|e| e.id.as_str()).collect();
        let mut stale_or_cached_stale_entity_ids: HashSet<&str> = HashSet::with_capacity_and_hasher(
            stale_entity_ids.len() + stale_cached_entity_ids.len(),
            Default::default(),
        );
        stale_or_cached_stale_entity_ids.extend(stale_entity_ids.iter().copied());
        stale_or_cached_stale_entity_ids.extend(stale_cached_entity_ids.iter().copied());

        let has_new_or_deleted_stale_entities = all_entities.iter().any(|entity| {
            stale_set.contains(entity.file_path.as_str())
                && !cached_hashes.contains_key(entity.id.as_str())
        }) || !deleted_ids.is_empty();

        // Find clean entities whose cached outgoing edges are invalidated by stale targets.
        let mut affected_clean_ids: HashSet<String> = HashSet::default();
        let mut affected_clean_file_paths: HashSet<&str> = HashSet::default();
        for edge in &cached_edges {
            let to_truly_changed = truly_changed_ids.contains(&edge.to_entity)
                || deleted_ids.contains(edge.to_entity.as_str());
            let to_stale_file = stale_or_cached_stale_entity_ids.contains(edge.to_entity.as_str());
            let from_file_path = entity_file_paths.get(edge.from_entity.as_str()).copied();
            let from_clean_file =
                from_file_path.is_some_and(|file_path| !stale_set.contains(file_path));

            if (to_truly_changed || to_stale_file) && from_clean_file {
                affected_clean_ids.insert(edge.from_entity.clone());
                if let Some(file_path) = from_file_path {
                    affected_clean_file_paths.insert(file_path);
                }
            }
        }

        let mut affected_target_names: HashSet<&str> = all_entities
            .iter()
            .filter(|entity| {
                truly_changed_ids.contains(&entity.id)
                    || parent_repaired_ids.contains(entity.id.as_str())
            })
            .map(|entity| entity.name.as_str())
            .collect();
        affected_target_names.extend(
            stale_file_cached_entities
                .iter()
                .filter(|entity| deleted_ids.contains(entity.id.as_str()))
                .map(|entity| entity.name.as_str()),
        );

        // Clean entities can gain edges to names introduced by stale files even when
        // no cached edge existed.
        if !affected_target_names.is_empty() {
            let affected_target_candidate_files: HashSet<&str> = affected_target_names
                .iter()
                .filter_map(|name| symbol_table.get(*name))
                .flatten()
                .filter_map(|entity_id| entity_file_paths.get(entity_id.as_str()).copied())
                .filter(|file_path| !stale_set.contains(*file_path))
                .collect();

            for entity in all_entities.iter().filter(|entity| {
                affected_target_candidate_files.contains(entity.file_path.as_str())
            }) {
                if stale_set.contains(entity.file_path.as_str())
                    || affected_clean_ids.contains(&entity.id)
                {
                    continue;
                }

                let ext = entity
                    .file_path
                    .rfind('.')
                    .map(|i| &entity.file_path[i..])
                    .unwrap_or("");
                if crate::parser::plugins::code::languages::get_language_config(ext).is_none() {
                    continue;
                }

                let extra = extra_ident_chars_for_file(&entity.file_path);
                if !text_mentions_any_name(&entity.content, &affected_target_names, extra) {
                    continue;
                }

                let stripped =
                    strip_for_language(strip_strategy_for_file(&entity.file_path), &entity.content);
                if text_mentions_any_name(&stripped, &affected_target_names, extra) {
                    affected_clean_ids.insert(entity.id.clone());
                    affected_clean_file_paths.insert(entity.file_path.as_str());
                }
            }
        }

        let import_table = if has_new_or_deleted_stale_entities {
            Some(build_import_table(
                root,
                all_file_paths,
                &symbol_table,
                &entity_map,
                Some(&parsed_files),
            ))
        } else {
            None
        };

        let mut new_stale_entity_ids: HashSet<&str> = HashSet::default();
        let mut new_stale_names: HashSet<&str> = HashSet::default();
        for entity in &all_entities {
            if stale_set.contains(entity.file_path.as_str())
                && !cached_hashes.contains_key(entity.id.as_str())
            {
                new_stale_entity_ids.insert(entity.id.as_str());
                new_stale_names.insert(entity.name.as_str());
            }
        }
        if !new_stale_names.is_empty() {
            let import_table = import_table
                .as_ref()
                .expect("new stale entity analysis requires a full import table");
            let new_stale_import_refs: HashSet<(&str, &str)> = import_table
                .iter()
                .filter(|(_, target_id)| new_stale_entity_ids.contains(target_id.as_str()))
                .map(|((file_path, local_name), _)| (file_path.as_str(), local_name.as_str()))
                .collect();
            let new_stale_file_paths: HashSet<&str> = new_stale_entity_ids
                .iter()
                .filter_map(|entity_id| entity_file_paths.get(*entity_id).copied())
                .collect();
            let mut clean_import_candidate_files: HashSet<&str> = new_stale_import_refs
                .iter()
                .map(|(file_path, _)| *file_path)
                .collect();
            let mut clean_entities_mentioning_new_stale_names: HashSet<&str> = HashSet::default();
            for entity in all_entities
                .iter()
                .filter(|entity| !stale_set.contains(entity.file_path.as_str()))
            {
                let extra = extra_ident_chars_for_file(&entity.file_path);
                if !new_stale_names
                    .iter()
                    .any(|name| content_contains_identifier(&entity.content, name, extra))
                {
                    continue;
                }

                let stripped =
                    strip_for_language(strip_strategy_for_file(&entity.file_path), &entity.content);
                if text_mentions_any_name(&stripped, &new_stale_names, extra) {
                    clean_entities_mentioning_new_stale_names.insert(entity.id.as_str());
                    clean_import_candidate_files.insert(entity.file_path.as_str());
                }
            }

            let clean_file_import_tokens: HashMap<&str, Vec<String>> = clean_import_candidate_files
                .into_iter()
                .filter_map(|file_path| {
                    let content = read_import_scan_prefix(&root.join(file_path))?;
                    let mut tokens: Vec<String> = new_stale_file_paths
                        .iter()
                        .flat_map(|stale_file_path| {
                            content_import_tokens_for_file(file_path, &content, stale_file_path)
                        })
                        .collect();
                    if tokens.is_empty() {
                        return None;
                    }
                    tokens.sort_unstable();
                    tokens.dedup();
                    Some((file_path, tokens))
                })
                .collect();
            let mut new_stale_import_refs_by_file: HashMap<&str, Vec<&str>> = HashMap::default();
            for (file_path, local_name) in &new_stale_import_refs {
                new_stale_import_refs_by_file
                    .entry(*file_path)
                    .or_default()
                    .push(*local_name);
            }

            for entity in all_entities
                .iter()
                .filter(|entity| !stale_set.contains(entity.file_path.as_str()))
            {
                if affected_clean_ids.contains(&entity.id) {
                    continue;
                }

                let entity_mentions_new_stale_name =
                    clean_entities_mentioning_new_stale_names.contains(entity.id.as_str());
                if !entity_mentions_new_stale_name
                    && !clean_file_import_tokens.contains_key(entity.file_path.as_str())
                    && !new_stale_import_refs_by_file.contains_key(entity.file_path.as_str())
                {
                    continue;
                }

                let import_tokens = clean_file_import_tokens.get(entity.file_path.as_str());
                let mentions_new_stale_name = entity_mentions_new_stale_name;
                let extra = extra_ident_chars_for_file(&entity.file_path);
                let strip_strategy = strip_strategy_for_file(&entity.file_path);
                let mentions_new_stale_import_token = import_tokens.map_or(false, |tokens| {
                    tokens
                        .iter()
                        .any(|token| content_contains_identifier(&entity.content, token, extra))
                });
                let imported_new_stale_ref = new_stale_import_refs_by_file
                    .get(entity.file_path.as_str())
                    .map_or(false, |local_names| {
                        local_names.iter().any(|local_name| {
                            content_contains_identifier(&entity.content, local_name, extra)
                        })
                    });
                let refs = extract_references_from_content(
                    &entity.content,
                    &entity.name,
                    extra,
                    strip_strategy,
                );
                if mentions_new_stale_name
                    || mentions_new_stale_import_token
                    || imported_new_stale_ref
                    || refs.iter().any(|ref_name| {
                        new_stale_names.contains(*ref_name)
                            || new_stale_import_refs
                                .contains(&(entity.file_path.as_str(), *ref_name))
                    })
                {
                    affected_clean_ids.insert(entity.id.clone());
                    affected_clean_file_paths.insert(entity.file_path.as_str());
                }
            }
        }

        let stale_js_ts_file_paths: Vec<&str> = stale_set
            .iter()
            .copied()
            .filter(|file_path| is_js_ts_file(file_path))
            .collect();
        if !stale_js_ts_file_paths.is_empty() {
            let clean_import_candidate_files: Vec<&str> = match cached_importing_stale_files {
                Some(files) => files
                    .iter()
                    .map(String::as_str)
                    .filter(|file_path| !stale_set.contains(*file_path) && is_js_ts_file(file_path))
                    .collect(),
                None => all_file_paths
                    .iter()
                    .map(String::as_str)
                    .filter(|file_path| !stale_set.contains(*file_path) && is_js_ts_file(file_path))
                    .collect(),
            };
            let clean_js_ts_import_tokens: HashMap<&str, Vec<String>> =
                clean_import_candidate_files
                    .into_iter()
                    .filter_map(|file_path| {
                        let content = read_import_scan_prefix(&root.join(file_path))?;
                        let mut tokens: Vec<String> = stale_js_ts_file_paths
                            .iter()
                            .flat_map(|stale_file_path| {
                                content_import_tokens_for_file(file_path, &content, stale_file_path)
                            })
                            .collect();
                        if tokens.is_empty() {
                            return None;
                        }
                        tokens.sort_unstable();
                        tokens.dedup();
                        Some((file_path, tokens))
                    })
                    .collect();

            for entity in all_entities
                .iter()
                .filter(|entity| !stale_set.contains(entity.file_path.as_str()))
            {
                if affected_clean_ids.contains(&entity.id) {
                    continue;
                }
                let Some(tokens) = clean_js_ts_import_tokens.get(entity.file_path.as_str()) else {
                    continue;
                };
                let extra = extra_ident_chars_for_file(&entity.file_path);
                if tokens
                    .iter()
                    .any(|token| content_contains_identifier(&entity.content, token, extra))
                {
                    affected_clean_ids.insert(entity.id.clone());
                    affected_clean_file_paths.insert(entity.file_path.as_str());
                }
            }
        }

        let import_table = match import_table {
            Some(import_table) => import_table,
            None => {
                let mut file_paths = stale_files.to_vec();
                file_paths.extend(
                    affected_clean_file_paths
                        .iter()
                        .map(|file_path| (*file_path).to_string()),
                );
                file_paths.sort_unstable();
                file_paths.dedup();
                build_import_table_with_default_export_paths(
                    root,
                    &file_paths,
                    all_file_paths,
                    &symbol_table,
                    &entity_map,
                    Some(&parsed_files),
                )
            }
        };

        // Keep edges where both endpoints are in clean (non-stale) files and from_entity
        // is not affected by target changes. Drop ALL cached edges from stale-file entities
        // (even content_clean ones) because import/scope context may have changed even when
        // entity content didn't. See: https://github.com/Ataraxy-Labs/sem/issues/116
        let kept_edges: Vec<EntityRef> = cached_edges
            .into_iter()
            .filter(|e| {
                if !current_entity_ids.contains(e.from_entity.as_str())
                    || !current_entity_ids.contains(e.to_entity.as_str())
                {
                    return false;
                }

                let from_stale = stale_or_cached_stale_entity_ids.contains(e.from_entity.as_str());
                let to_stale = stale_or_cached_stale_entity_ids.contains(e.to_entity.as_str());

                if !from_stale && !to_stale && !affected_clean_ids.contains(&e.from_entity) {
                    // Both endpoints in clean files, from not affected
                    return true;
                }
                false
            })
            .collect();

        // Set of entity IDs that need resolution: all stale-file entities + affected clean.
        // Content-clean stale entities must be re-resolved because import/scope context
        // may have changed even if entity body content is identical.
        let needs_resolution: HashSet<&str> = all_entities
            .iter()
            .filter(|e| {
                truly_changed_ids.contains(&e.id)
                    || content_clean_ids.contains(&e.id)
                    || parent_repaired_ids.contains(e.id.as_str())
                    || affected_clean_ids.contains(&e.id)
            })
            .map(|e| e.id.as_str())
            .collect();

        // Now run the same resolution logic as build() but only for entities in needs_resolution.
        // The lookup structures still include ALL entities.

        // Build parent-child set
        let parent_child_pairs: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id
                    .as_ref()
                    .map(|pid| (pid.as_str(), e.id.as_str()))
            })
            .collect();
        let mut child_line_ranges: HashMap<String, Vec<(usize, usize)>> = HashMap::default();
        for entity in &all_entities {
            if let Some(pid) = &entity.parent_id {
                child_line_ranges
                    .entry(pid.clone())
                    .or_default()
                    .push((entity.start_line, entity.end_line));
            }
        }
        for ranges in child_line_ranges.values_mut() {
            ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
        }

        let class_child_names: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id
                    .as_ref()
                    .map(|pid| (pid.as_str(), e.name.as_str()))
            })
            .collect();

        let child_ranges_by_parent = build_child_ranges_by_parent(&all_entities);

        let class_entity_names: HashSet<&str> = all_entities
            .iter()
            .filter(|e| is_nominal_member_container(e.entity_type.as_str()))
            .map(|e| e.name.as_str())
            .collect();
        let class_entity_files: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter(|e| is_nominal_member_container(e.entity_type.as_str()))
            .map(|e| (e.name.as_str(), e.file_path.as_str()))
            .collect();

        let id_to_name: HashMap<&str, &str> = all_entities
            .iter()
            .map(|e| (e.id.as_str(), e.name.as_str()))
            .collect();

        let mut enclosing_class: HashMap<&str, &str> = HashMap::default();
        let mut class_members: HashMap<&str, Vec<(&str, &str)>> = HashMap::default();
        let mut scope_class_members: HashMap<String, Vec<(String, String)>> = HashMap::default();
        let mut scope_owner_members: HashMap<String, Vec<(String, String)>> = HashMap::default();
        let mut scope_entity_ranges: HashMap<String, Vec<(usize, usize, String)>> =
            HashMap::default();

        for entity in &all_entities {
            scope_entity_ranges
                .entry(entity.file_path.clone())
                .or_default()
                .push((entity.start_line, entity.end_line, entity.id.clone()));
            if let Some(ref pid) = entity.parent_id {
                scope_owner_members
                    .entry(pid.clone())
                    .or_default()
                    .push((entity.name.clone(), entity.id.clone()));
                if let Some(parent) = entity_map.get(pid.as_str()) {
                    if let Some(owner_name) = scope_resolve::class_member_owner_name(parent) {
                        scope_class_members
                            .entry(owner_name.to_string())
                            .or_default()
                            .push((entity.name.clone(), entity.id.clone()));
                    }
                }
                if let Some(&parent_name) = id_to_name.get(pid.as_str()) {
                    if class_entity_names.contains(parent_name) {
                        enclosing_class.insert(entity.id.as_str(), parent_name);
                        class_members
                            .entry(parent_name)
                            .or_default()
                            .push((entity.name.as_str(), entity.id.as_str()));
                    }
                }
            }
            if entity.entity_type == "method" && entity.file_path.ends_with(".go") {
                if let Some(struct_name) = scope_resolve::extract_go_receiver_type(&entity.content)
                {
                    scope_class_members
                        .entry(struct_name)
                        .or_default()
                        .push((entity.name.clone(), entity.id.clone()));
                }
            }
        }
        for members in scope_class_members.values_mut() {
            members.sort_unstable();
        }
        for members in scope_owner_members.values_mut() {
            members.sort_unstable();
        }
        for ranges in scope_entity_ranges.values_mut() {
            ranges.sort_unstable();
        }

        // Run scope-aware resolver only on files that need resolution
        let resolve_file_paths: Vec<String> = all_file_paths
            .iter()
            .filter(|f| {
                stale_set.contains(f.as_str()) || affected_clean_file_paths.contains(f.as_str())
            })
            .cloned()
            .collect();

        let has_scope_lang = resolve_file_paths.iter().any(|f| {
            let ext = f.rfind('.').map(|i| &f[i..]).unwrap_or("");
            crate::parser::plugins::code::languages::get_language_config(ext)
                .and_then(|c| c.scope_resolve)
                .is_some()
        });
        let (scope_edges, scope_consumed_words) = if has_scope_lang {
            // Pass pre-parsed stale-file trees; scope_resolve reads affected clean files from disk
            let resolve_set: HashSet<&str> =
                resolve_file_paths.iter().map(|s| s.as_str()).collect();
            let relevant_parsed: Vec<(String, String, tree_sitter::Tree)> = parsed_files
                .into_iter()
                .filter(|(fp, _, _)| resolve_set.contains(fp.as_str()))
                .collect();
            let pre = if relevant_parsed.is_empty() {
                None
            } else {
                Some(relevant_parsed)
            };
            let owned_go_pkg_index: HashMap<String, Vec<(String, String)>> =
                if resolve_file_paths.iter().any(|f| f.ends_with(".go")) {
                    scope_resolve::build_go_pkg_index(&symbol_table, &entity_map)
                } else {
                    HashMap::default()
                };
            let pre_built = scope_resolve::PreBuiltLookups {
                symbol_table: Arc::clone(&symbol_table),
                class_members: scope_class_members,
                owner_members: scope_owner_members,
                entity_ranges: scope_entity_ranges,
                go_pkg_index: owned_go_pkg_index,
            };
            let result = scope_resolve::resolve_with_scopes_full(
                root,
                &resolve_file_paths,
                &all_entities,
                &entity_map,
                pre,
                Some(&pre_built),
                Some(&import_table),
                false,
            );
            (result.edges, result.consumed_words)
        } else {
            (vec![], HashMap::default())
        };

        let imports_by_file = build_imports_by_file(&import_table);
        let reference_context = ReferenceResolutionContext {
            symbol_table: symbol_table.as_ref(),
            entity_map: &entity_map,
            imports_by_file: &imports_by_file,
            scope_consumed_words: &scope_consumed_words,
            child_ranges_by_parent: &child_ranges_by_parent,
            child_line_ranges: &child_line_ranges,
            parent_child_pairs: &parent_child_pairs,
            class_child_names: &class_child_names,
            class_entity_files: &class_entity_files,
            enclosing_class: &enclosing_class,
            class_members: &class_members,
        };
        let resolved_refs = resolve_references_with_file_indexes(
            root,
            &resolve_file_paths,
            &all_entities,
            Some(&needs_resolution),
            &reference_context,
        );

        let export_edges = build_export_alias_edges(&all_entities, &import_table);

        // Merge scope edges + bag-of-words edges + kept cached edges
        let mut combined: Vec<(String, String, RefType)> = scope_edges;
        combined.extend(export_edges);
        combined.extend(resolved_refs);
        let mut all_resolved = dedupe_resolved_edges(combined);
        sort_resolved_refs(&mut all_resolved);

        // Build final edge list: kept edges + newly resolved edges
        let mut edges: Vec<EntityRef> = Vec::with_capacity(kept_edges.len() + all_resolved.len());

        let mut kept_edge_pairs: HashSet<(&str, &str)> =
            HashSet::with_capacity_and_hasher(kept_edges.len(), Default::default());
        for edge in &kept_edges {
            kept_edge_pairs.insert((edge.from_entity.as_str(), edge.to_entity.as_str()));
        }

        let mut new_edges: Vec<(String, String, RefType)> = Vec::with_capacity(all_resolved.len());
        for (from_entity, to_entity, ref_type) in all_resolved {
            if kept_edge_pairs.contains(&(from_entity.as_str(), to_entity.as_str())) {
                continue;
            }
            new_edges.push((from_entity, to_entity, ref_type));
        }
        drop(kept_edge_pairs);

        for edge in kept_edges {
            edges.push(edge);
        }

        for (from_entity, to_entity, ref_type) in new_edges {
            edges.push(EntityRef {
                from_entity,
                to_entity,
                ref_type,
            });
        }

        let graph = EntityGraph::from_parts(entity_map.into_iter().collect(), edges);

        let mut recomputed_edge_source_ids: Vec<String> = needs_resolution
            .iter()
            .map(|id| (*id).to_string())
            .collect();
        recomputed_edge_source_ids.sort_unstable();
        recomputed_edge_source_ids.dedup();

        let mut deleted_entity_ids: Vec<String> =
            deleted_ids.iter().map(|id| (*id).to_string()).collect();
        deleted_entity_ids.sort_unstable();
        deleted_entity_ids.dedup();

        (
            graph,
            all_entities,
            IncrementalBuildMetadata {
                repaired_clean_entity_ids,
                recomputed_edge_source_ids,
                deleted_entity_ids,
            },
        )
    }

    /// Get entities that depend on the given entity (reverse deps).
    pub fn get_dependents(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependents
            .get(entity_id)
            .map(|ids| ids.iter().filter_map(|id| self.entities.get(id)).collect())
            .unwrap_or_default()
    }

    /// Get entities that the given entity depends on (forward deps).
    pub fn get_dependencies(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependencies
            .get(entity_id)
            .map(|ids| ids.iter().filter_map(|id| self.entities.get(id)).collect())
            .unwrap_or_default()
    }

    /// Impact analysis: if the given entity changes, what else might be affected?
    /// Returns all transitive dependents (breadth-first), capped at 10k.
    pub fn impact_analysis(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.impact_analysis_capped(entity_id, 10_000)
    }

    /// Depth-limited impact analysis. Returns transitive dependents with their BFS depth.
    /// `max_depth == 0` means unlimited. Default depth of 2 covers direct + one transitive level.
    pub fn impact_analysis_bounded(
        &self,
        entity_id: &str,
        max_depth: usize,
    ) -> Vec<(&EntityInfo, usize)> {
        let mut visited: HashSet<&str> = HashSet::default();
        let mut queue: std::collections::VecDeque<(&str, usize)> =
            std::collections::VecDeque::new();
        let mut result = Vec::new();

        let start_key = match self.entities.get_key_value(entity_id) {
            Some((k, _)) => k.as_str(),
            None => return result,
        };

        queue.push_back((start_key, 0));
        visited.insert(start_key);

        while let Some((current, depth)) = queue.pop_front() {
            if let Some(deps) = self.dependents.get(current) {
                let next_depth = depth + 1;
                if max_depth > 0 && next_depth > max_depth {
                    continue;
                }
                for dep in deps {
                    if visited.insert(dep.as_str()) {
                        if let Some(info) = self.entities.get(dep.as_str()) {
                            result.push((info, next_depth));
                        }
                        queue.push_back((dep.as_str(), next_depth));
                    }
                }
            }
        }

        result
    }

    /// Impact analysis with a cap on maximum nodes visited.
    /// Returns transitive dependents up to the cap. Uses borrowed strings.
    pub fn impact_analysis_capped(&self, entity_id: &str, max_visited: usize) -> Vec<&EntityInfo> {
        let mut visited: HashSet<&str> = HashSet::default();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        let mut result = Vec::new();

        let start_key = match self.entities.get_key_value(entity_id) {
            Some((k, _)) => k.as_str(),
            None => return result,
        };

        queue.push_back(start_key);
        visited.insert(start_key);

        while let Some(current) = queue.pop_front() {
            if result.len() >= max_visited {
                break;
            }
            if let Some(deps) = self.dependents.get(current) {
                for dep in deps {
                    if visited.insert(dep.as_str()) {
                        if let Some(info) = self.entities.get(dep.as_str()) {
                            result.push(info);
                        }
                        queue.push_back(dep.as_str());
                        if result.len() >= max_visited {
                            break;
                        }
                    }
                }
            }
        }

        result
    }

    /// Count transitive dependents without collecting them (faster for large graphs).
    /// Uses borrowed strings to avoid allocation overhead.
    pub fn impact_count(&self, entity_id: &str, max_count: usize) -> usize {
        let mut visited: HashSet<&str> = HashSet::default();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        let mut count = 0;

        // We need entity_id to live long enough; look it up in our entities map
        let start_key = match self.entities.get_key_value(entity_id) {
            Some((k, _)) => k.as_str(),
            None => return 0,
        };

        queue.push_back(start_key);
        visited.insert(start_key);

        while let Some(current) = queue.pop_front() {
            if count >= max_count {
                break;
            }
            if let Some(deps) = self.dependents.get(current) {
                for dep in deps {
                    if visited.insert(dep.as_str()) {
                        count += 1;
                        queue.push_back(dep.as_str());
                        if count >= max_count {
                            break;
                        }
                    }
                }
            }
        }

        count
    }

    /// Filter entities to those that look like tests.
    /// Uses name heuristics, file path patterns, and content patterns.
    pub fn filter_test_entities(
        &self,
        entities: &[crate::model::entity::SemanticEntity],
    ) -> StdHashSet<String> {
        self.filter_test_entities_with_custom_dirs(entities, &[])
    }

    /// Like [`filter_test_entities`], but also considers user-configured
    /// test directories from `.semrc`.
    pub fn filter_test_entities_with_custom_dirs(
        &self,
        entities: &[crate::model::entity::SemanticEntity],
        custom_test_dirs: &[String],
    ) -> StdHashSet<String> {
        let mut test_ids = StdHashSet::new();
        for entity in entities {
            if is_test_entity(entity, custom_test_dirs) {
                test_ids.insert(entity.id.clone());
            }
        }
        test_ids
    }

    /// Impact analysis filtered to test entities only.
    /// Returns transitive dependents that are test functions/methods.
    pub fn test_impact(
        &self,
        entity_id: &str,
        all_entities: &[crate::model::entity::SemanticEntity],
    ) -> Vec<&EntityInfo> {
        self.test_impact_with_custom_dirs(entity_id, all_entities, &[])
    }

    /// Like [`test_impact`], but also considers user-configured test
    /// directories from `.semrc`.
    pub fn test_impact_with_custom_dirs(
        &self,
        entity_id: &str,
        all_entities: &[crate::model::entity::SemanticEntity],
        custom_test_dirs: &[String],
    ) -> Vec<&EntityInfo> {
        let test_ids = self.filter_test_entities_with_custom_dirs(all_entities, custom_test_dirs);
        let impact = self.impact_analysis(entity_id);
        impact
            .into_iter()
            .filter(|info| test_ids.contains(&info.id))
            .collect()
    }

    /// Incrementally update the graph from a set of changed files.
    ///
    /// Instead of rebuilding the entire graph, this only re-extracts entities
    /// from changed files and re-resolves their references. This is faster
    /// than a full rebuild when only a few files changed.
    ///
    /// For each changed file:
    /// - Deleted: remove all entities from that file, prune edges
    /// - Added/Modified: remove old entities, extract new ones, rebuild references
    /// - Renamed: update file paths in entity info
    pub fn update_from_changes(
        &mut self,
        changed_files: &[FileChange],
        root: &Path,
        registry: &ParserRegistry,
    ) {
        let mut affected_files: HashSet<String> = HashSet::default();
        let mut new_entities: Vec<SemanticEntity> = Vec::new();

        for change in changed_files {
            affected_files.insert(change.file_path.clone());
            if let Some(ref old_path) = change.old_file_path {
                affected_files.insert(old_path.clone());
            }

            match change.status {
                FileStatus::Deleted => {
                    self.remove_entities_for_file(&change.file_path);
                }
                FileStatus::Renamed => {
                    // Update file paths for renamed files
                    if let Some(ref old_path) = change.old_file_path {
                        self.remove_entities_for_file(old_path);
                    }
                    // Extract entities from the new file
                    if let Some(entities) = self.extract_file_entities(
                        &change.file_path,
                        change.after_content.as_deref(),
                        root,
                        registry,
                    ) {
                        new_entities.extend(entities);
                    }
                }
                FileStatus::Added | FileStatus::Modified => {
                    // Remove old entities for this file
                    self.remove_entities_for_file(&change.file_path);
                    // Extract new entities
                    if let Some(entities) = self.extract_file_entities(
                        &change.file_path,
                        change.after_content.as_deref(),
                        root,
                        registry,
                    ) {
                        new_entities.extend(entities);
                    }
                }
            }
        }

        // Add new entities to the entity map
        for entity in &new_entities {
            self.entities.insert(
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
        }

        // Rebuild the global symbol table from all current entities
        let symbol_table = self.build_symbol_table();
        let child_ranges_by_parent = build_child_ranges_by_parent(&new_entities);

        // Re-resolve references for new entities
        for entity in &new_entities {
            self.resolve_entity_references(entity, &symbol_table, &child_ranges_by_parent);
        }

        // Also re-resolve references for entities in OTHER files that might
        // reference entities in changed files (their targets may have changed)
        let changed_entity_names: HashSet<String> =
            new_entities.iter().map(|e| e.name.clone()).collect();

        // Find entities in unchanged files that reference any changed entity name
        let entities_to_recheck: Vec<String> = self
            .entities
            .values()
            .filter(|e| !affected_files.contains(&e.file_path))
            .filter(|e| {
                self.dependencies.get(&e.id).map_or(false, |deps| {
                    deps.iter().any(|dep_id| {
                        self.entities
                            .get(dep_id)
                            .map_or(false, |dep| changed_entity_names.contains(&dep.name))
                    })
                })
            })
            .map(|e| e.id.clone())
            .collect();

        // We don't have the full SemanticEntity for unchanged files, so we skip
        // deep re-resolution here. The forward/reverse indexes are already updated
        // by remove_entities_for_file and resolve_entity_references.
        // For entities that had dangling references (their target was deleted),
        // the edges were already pruned.
        let _ = entities_to_recheck; // acknowledge but don't act on for now
    }

    /// Extract entities from a file, using provided content or reading from disk.
    fn extract_file_entities(
        &self,
        file_path: &str,
        content: Option<&str>,
        root: &Path,
        registry: &ParserRegistry,
    ) -> Option<Vec<SemanticEntity>> {
        let content = if let Some(c) = content {
            c.to_string()
        } else {
            let full_path = root.join(file_path);
            std::fs::read_to_string(&full_path).ok()?
        };

        Some(registry.extract_entities(file_path, &content))
    }

    /// Remove all entities belonging to a specific file and prune their edges.
    fn remove_entities_for_file(&mut self, file_path: &str) {
        // Collect entity IDs to remove
        let ids_to_remove: Vec<String> = self
            .entities
            .values()
            .filter(|e| e.file_path == file_path)
            .map(|e| e.id.clone())
            .collect();

        let id_set: HashSet<&str> = ids_to_remove.iter().map(|s| s.as_str()).collect();

        // Remove from entity map
        for id in &ids_to_remove {
            self.entities.remove(id);
        }

        // Remove edges involving these entities
        self.edges.retain(|e| {
            !id_set.contains(e.from_entity.as_str()) && !id_set.contains(e.to_entity.as_str())
        });

        // Clean up dependency/dependent indexes
        for id in &ids_to_remove {
            // Remove forward deps
            if let Some(deps) = self.dependencies.remove(id) {
                // Also remove from reverse index
                for dep in &deps {
                    if let Some(dependents) = self.dependents.get_mut(dep) {
                        dependents.retain(|d| d != id);
                    }
                }
            }
            // Remove reverse deps
            if let Some(deps) = self.dependents.remove(id) {
                // Also remove from forward index
                for dep in &deps {
                    if let Some(dependencies) = self.dependencies.get_mut(dep) {
                        dependencies.retain(|d| d != id);
                    }
                }
            }
        }
    }

    /// Build a symbol table from all current entities.
    fn build_symbol_table(&self) -> HashMap<String, Vec<String>> {
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::default();
        let mut entities = self.entities.values().collect::<Vec<_>>();
        entities.sort_unstable_by(|left, right| {
            left.file_path
                .cmp(&right.file_path)
                .then_with(|| left.start_line.cmp(&right.start_line))
                .then_with(|| left.end_line.cmp(&right.end_line))
                .then_with(|| left.id.cmp(&right.id))
        });
        for entity in entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());
        }
        symbol_table
    }

    /// Resolve references for a single entity against the symbol table.
    fn resolve_entity_references(
        &mut self,
        entity: &SemanticEntity,
        symbol_table: &HashMap<String, Vec<String>>,
        child_ranges_by_parent: &HashMap<&str, Vec<ChildRange<'_>>>,
    ) {
        let stripped = strip_comments_and_strings(&entity.content);
        let refs = extract_references_with_stripped_filtered(
            &entity.content,
            &entity.name,
            &stripped,
            extra_ident_chars_for_file(&entity.file_path),
            |local_line, local_start_byte, local_end_byte| {
                entity_owns_content_span(
                    entity.id.as_str(),
                    entity.file_path.as_str(),
                    source_line_for_entity_content(entity, local_line),
                    Some(local_start_byte),
                    Some(local_end_byte),
                    child_ranges_by_parent,
                )
            },
        );

        for ref_name in refs {
            if let Some(target_ids) = symbol_table.get(ref_name) {
                let target = target_ids
                    .iter()
                    .find(|id| {
                        *id != &entity.id
                            && self
                                .entities
                                .get(*id)
                                .map_or(false, |e| e.file_path == entity.file_path)
                    })
                    .or_else(|| target_ids.iter().find(|id| *id != &entity.id));

                if let Some(target_id) = target {
                    let ref_type = infer_ref_type(&entity.content, &ref_name);
                    self.edges.push(EntityRef {
                        from_entity: entity.id.clone(),
                        to_entity: target_id.clone(),
                        ref_type,
                    });
                    self.dependents
                        .entry(target_id.clone())
                        .or_default()
                        .push(entity.id.clone());
                    self.dependencies
                        .entry(entity.id.clone())
                        .or_default()
                        .push(target_id.clone());
                }
            }
        }
    }
}

fn is_nominal_member_container(entity_type: &str) -> bool {
    matches!(
        entity_type,
        "class" | "struct" | "interface" | "class_type" | "enum" | "protocol"
    )
}

#[cfg(test)]
fn is_scope_member_container(entity_type: &str) -> bool {
    matches!(
        entity_type,
        "class"
            | "struct"
            | "interface"
            | "impl"
            | "enum"
            | "protocol"
            | "object_declaration"
            | "companion_object"
    )
}

/// Check if an entity looks like a test based on name, file path, and content patterns.
fn is_test_entity(entity: &crate::model::entity::SemanticEntity, custom_test_dirs: &[String]) -> bool {
    let name = &entity.name;
    let content = &entity.content;

    // Name patterns
    if name.starts_with("test_")
        || name.starts_with("Test")
        || name.ends_with("_test")
        || name.ends_with("Test")
    {
        return true;
    }
    if name.starts_with("it_") || name.starts_with("describe_") || name.starts_with("spec_") {
        return true;
    }

    // File path patterns (shared detection)
    let in_test_file =
        crate::parser::test_detect::is_test_path_with_custom_dirs(&entity.file_path, custom_test_dirs);

    // Content patterns (test annotations/decorators)
    let has_test_marker = content.contains("#[test]")
        || content.contains("#[cfg(test)]")
        || content.contains("@Test")
        || content.contains("@pytest")
        || content.contains("@test")
        || content.contains("describe(")
        || content.contains("it(")
        || content.contains("test(");

    in_test_file && has_test_marker
}

fn build_export_alias_edges(
    all_entities: &[SemanticEntity],
    import_table: &HashMap<(String, String), String>,
) -> Vec<(String, String, RefType)> {
    all_entities
        .iter()
        .filter(|entity| entity.entity_type == "export")
        .filter_map(|entity| {
            let key = (entity.file_path.clone(), entity.name.clone());
            let target_id = import_table.get(&key)?;
            if target_id == &entity.id {
                return None;
            }
            Some((entity.id.clone(), target_id.clone(), RefType::Imports))
        })
        .collect()
}

struct TsDefaultExportTable {
    exports_by_file: HashMap<String, String>,
    sorted_files: Vec<String>,
}

struct TsTopLevelEntityTable {
    entities_by_file: HashMap<String, Vec<(String, String)>>,
    sorted_files: Vec<String>,
}

struct TsDefaultReExport {
    file_path: String,
    original_name: String,
    module_path: String,
}

struct PendingDefaultImport {
    file_path: String,
    local_name: String,
    module_path: String,
}

struct PendingNamespaceImport {
    file_path: String,
    alias: String,
    module_path: String,
}

struct ImportFileScan {
    default_export: Option<(String, String)>,
    default_re_exports: Vec<TsDefaultReExport>,
    named_exports: Option<(String, HashSet<String>)>,
    local_imports: Vec<((String, String), String)>,
    default_imports: Vec<PendingDefaultImport>,
    namespace_imports: Vec<PendingNamespaceImport>,
    re_export_imports: Vec<((String, String), String)>,
}

fn sorted_default_export_files(default_exports: &HashMap<String, String>) -> Vec<String> {
    let mut sorted_files: Vec<String> = default_exports.keys().cloned().collect();
    sort_import_candidate_files(&mut sorted_files, JS_TS_EXTENSIONS);
    sorted_files
}

fn build_ts_top_level_entity_table(
    entity_map: &HashMap<String, EntityInfo>,
) -> TsTopLevelEntityTable {
    let mut entities_by_file: HashMap<String, Vec<(String, String)>> = HashMap::default();
    for entity in entity_map.values() {
        if !is_js_ts_file(&entity.file_path) || entity.parent_id.is_some() {
            continue;
        }
        entities_by_file
            .entry(entity.file_path.clone())
            .or_default()
            .push((entity.name.clone(), entity.id.clone()));
    }
    for entries in entities_by_file.values_mut() {
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    }
    let mut sorted_files: Vec<String> = entities_by_file.keys().cloned().collect();
    sort_import_candidate_files(&mut sorted_files, JS_TS_EXTENSIONS);
    TsTopLevelEntityTable {
        entities_by_file,
        sorted_files,
    }
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

fn default_export_names_from_content(content: &str) -> Vec<String> {
    static DEFAULT_FUNCTION_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bexport\s+default\s+(?:async\s+)?function\s*\*?\s+([A-Za-z_$][\w$]*)")
            .unwrap()
    });
    static DEFAULT_CLASS_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bexport\s+default\s+(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)").unwrap()
    });
    static DEFAULT_IDENTIFIER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\bexport\s+default\s+([A-Za-z_$][\w$]*)").unwrap());
    static DEFAULT_SPECIFIER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"export\s+(?:type\s+)?\{([^}]+)\}\s*;?"#).unwrap());

    let mut names = Vec::new();
    for cap in DEFAULT_FUNCTION_RE.captures_iter(content) {
        names.push(cap.get(1).unwrap().as_str().to_string());
    }
    for cap in DEFAULT_CLASS_RE.captures_iter(content) {
        names.push(cap.get(1).unwrap().as_str().to_string());
    }
    for cap in DEFAULT_IDENTIFIER_RE.captures_iter(content) {
        let name = cap.get(1).unwrap();
        let line_tail = content[name.end()..]
            .split_once('\n')
            .map_or(&content[name.end()..], |(line, _)| line);
        if only_js_ts_statement_trivia(line_tail) {
            names.push(name.as_str().to_string());
        }
    }
    for cap in DEFAULT_SPECIFIER_RE.captures_iter(content) {
        let rest = content[cap.get(0).unwrap().end()..].trim_start();
        if rest.starts_with("from ") {
            continue;
        }
        let names_str = cap.get(1).unwrap().as_str();
        for name_part in names_str.split(',') {
            let Some((original_name, local_name)) = parse_js_ts_import_specifier(name_part) else {
                continue;
            };
            if local_name == "default" {
                names.push(original_name.to_string());
            }
        }
    }

    names
}

fn default_re_exports_from_content(content: &str) -> Vec<(String, String)> {
    static REEXPORT_SPECIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"export\s+(?:type\s+)?\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#).unwrap()
    });

    let mut re_exports = Vec::new();
    for cap in REEXPORT_SPECIFIER_RE.captures_iter(content) {
        let names_str = cap.get(1).unwrap().as_str();
        let module_path = cap.get(2).unwrap().as_str();
        for name_part in names_str.split(',') {
            let Some((original_name, local_name)) = parse_js_ts_import_specifier(name_part) else {
                continue;
            };
            if local_name == "default" {
                re_exports.push((original_name.to_string(), module_path.to_string()));
            }
        }
    }
    re_exports
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

fn resolve_default_export_target(
    default_exports: &TsDefaultExportTable,
    module_path: &str,
    file_path: &str,
) -> Option<String> {
    let target_file = find_import_file(
        &default_exports.sorted_files,
        module_path,
        file_path,
        JS_TS_EXTENSIONS,
    )?;
    default_exports.exports_by_file.get(target_file).cloned()
}

fn parse_js_ts_import_specifier(name_part: &str) -> Option<(&str, &str)> {
    let name_part = name_part.trim();
    if name_part.is_empty() {
        return None;
    }

    let (original, local) = if let Some(pos) = name_part.find(" as ") {
        let original = name_part[..pos].trim();
        let local = name_part[pos + 4..].trim();
        (original, local)
    } else {
        (name_part, name_part)
    };

    let original = original.strip_prefix("type ").unwrap_or(original).trim();
    let local = local.strip_prefix("type ").unwrap_or(local).trim();
    if original.is_empty() || local.is_empty() {
        return None;
    }

    Some((original, local))
}

fn import_source_content<'a>(
    root: &Path,
    pre_parsed_content: &HashMap<&'a str, &'a str>,
    file_path: &str,
) -> Option<Cow<'a, str>> {
    if let Some(content) = pre_parsed_content.get(file_path) {
        Some(Cow::Borrowed(*content))
    } else {
        std::fs::read_to_string(root.join(file_path))
            .ok()
            .map(Cow::Owned)
    }
}

fn same_file_set(left: &[String], right: &[String]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let left_set: HashSet<&str> = left.iter().map(String::as_str).collect();
    right
        .iter()
        .all(|file_path| left_set.contains(file_path.as_str()))
}

fn scan_import_file(
    file_path: &str,
    content: &str,
    parse_imports: bool,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    clojure_ns_index: &ClojureNsIndex,
) -> ImportFileScan {
    let mut scan = ImportFileScan {
        default_export: None,
        default_re_exports: Vec::new(),
        named_exports: None,
        local_imports: Vec::new(),
        default_imports: Vec::new(),
        namespace_imports: Vec::new(),
        re_export_imports: Vec::new(),
    };

    let is_js_ts = is_js_ts_file(file_path);
    if is_js_ts {
        for name in default_export_names_from_content(content) {
            let Some(target_ids) = symbol_table.get(name.as_str()) else {
                continue;
            };
            let target = target_ids.iter().find(|id| {
                entity_map.get(*id).map_or(false, |entity| {
                    entity.file_path == file_path && entity.parent_id.is_none()
                })
            });
            if let Some(target_id) = target {
                scan.default_export = Some((file_path.to_string(), target_id.clone()));
            }
        }

        scan.default_re_exports = default_re_exports_from_content(content)
            .into_iter()
            .map(|(original_name, module_path)| TsDefaultReExport {
                file_path: file_path.to_string(),
                original_name,
                module_path,
            })
            .collect();
        scan.named_exports = Some((
            file_path.to_string(),
            js_ts_named_exports_from_content(content)
                .into_iter()
                .collect(),
        ));
    }

    if !parse_imports {
        return scan;
    }

    // Join multi-line Python imports into single logical lines.
    let mut logical_lines: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut in_parens = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if in_parens {
            let clean = trimmed.trim_end_matches(|c: char| c == ')' || c == ',');
            let clean = clean.split('#').next().unwrap_or(clean).trim();
            if !clean.is_empty() && clean != "(" {
                current_line.push_str(", ");
                current_line.push_str(clean);
            }
            if trimmed.contains(')') {
                in_parens = false;
                logical_lines.push(std::mem::take(&mut current_line));
            }
        } else if trimmed.starts_with("from ") && trimmed.contains(" import ") {
            if trimmed.contains('(') && !trimmed.contains(')') {
                in_parens = true;
                let before_paren = trimmed.split('(').next().unwrap_or(trimmed);
                current_line = before_paren.trim().to_string();
                if let Some(after) = trimmed.split('(').nth(1) {
                    let after = after.trim().trim_end_matches(')').trim();
                    if !after.is_empty() {
                        current_line.push(' ');
                        current_line.push_str(after);
                    }
                }
            } else {
                logical_lines.push(trimmed.to_string());
            }
        }
    }

    for logical_line in &logical_lines {
        if let Some(rest) = logical_line.strip_prefix("from ") {
            let import_match = rest
                .find(" import ")
                .map(|pos| (pos, 8))
                .or_else(|| rest.find(" import,").map(|pos| (pos, 8)));
            if let Some((import_pos, skip)) = import_match {
                let module_path = &rest[..import_pos];
                let names_str = &rest[import_pos + skip..];

                for name_part in names_str.split(',') {
                    let name_part = name_part.trim();
                    let imported_name = name_part.split_whitespace().next().unwrap_or(name_part);
                    let imported_name =
                        imported_name.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
                    if imported_name.is_empty() {
                        continue;
                    }

                    if let Some(target_ids) = symbol_table.get(imported_name) {
                        let target = find_import_target(
                            target_ids,
                            module_path,
                            file_path,
                            &[".py"],
                            entity_map,
                        );
                        if let Some(target_id) = target {
                            scan.local_imports.push((
                                (file_path.to_string(), imported_name.to_string()),
                                target_id.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }

    if is_js_ts {
        static JS_NAMED_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r#"import\s+(?:type\s+)?(?:[A-Za-z_$][\w$]*\s*,\s*)?\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#,
            )
            .unwrap()
        });
        static JS_DEFAULT_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r#"import\s+(?:type\s+)?([A-Za-z_$][\w$]*)(?:\s*,\s*\{[^}]*\})?\s*from\s*['"]([^'"]+)['"]"#,
            )
            .unwrap()
        });
        static JS_REEXPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"export\s+(?:type\s+)?\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#).unwrap()
        });
        static JS_NAMESPACE_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"import\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s*from\s*['"]([^'"]+)['"]"#)
                .unwrap()
        });

        for cap in JS_NAMED_RE.captures_iter(content) {
            let names_str = cap.get(1).unwrap().as_str();
            let module_path = cap.get(2).unwrap().as_str();

            for name_part in names_str.split(',') {
                let Some((original_name, local_name)) = parse_js_ts_import_specifier(name_part)
                else {
                    continue;
                };

                if let Some(target_ids) = symbol_table.get(original_name) {
                    let target = find_import_target(
                        target_ids,
                        module_path,
                        file_path,
                        JS_TS_EXTENSIONS,
                        entity_map,
                    );
                    if let Some(target_id) = target {
                        scan.local_imports.push((
                            (file_path.to_string(), local_name.to_string()),
                            target_id.clone(),
                        ));
                    }
                }
            }
        }

        for cap in JS_DEFAULT_RE.captures_iter(content) {
            scan.default_imports.push(PendingDefaultImport {
                file_path: file_path.to_string(),
                local_name: cap.get(1).unwrap().as_str().to_string(),
                module_path: cap.get(2).unwrap().as_str().to_string(),
            });
        }

        for cap in JS_NAMESPACE_RE.captures_iter(content) {
            scan.namespace_imports.push(PendingNamespaceImport {
                file_path: file_path.to_string(),
                alias: cap.get(1).unwrap().as_str().to_string(),
                module_path: cap.get(2).unwrap().as_str().to_string(),
            });
        }

        for cap in JS_REEXPORT_RE.captures_iter(content) {
            let names_str = cap.get(1).unwrap().as_str();
            let module_path = cap.get(2).unwrap().as_str();

            for name_part in names_str.split(',') {
                let Some((original_name, local_name)) = parse_js_ts_import_specifier(name_part)
                else {
                    continue;
                };

                if original_name == "default" {
                    scan.default_imports.push(PendingDefaultImport {
                        file_path: file_path.to_string(),
                        local_name: local_name.to_string(),
                        module_path: module_path.to_string(),
                    });
                } else if let Some(target_id) =
                    symbol_table.get(original_name).and_then(|target_ids| {
                        find_import_target(
                            target_ids,
                            module_path,
                            file_path,
                            JS_TS_EXTENSIONS,
                            entity_map,
                        )
                        .cloned()
                    })
                {
                    scan.re_export_imports
                        .push(((file_path.to_string(), local_name.to_string()), target_id));
                }
            }
        }
    }

    if file_path.ends_with(".rs") {
        static RUST_USE_SIMPLE_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?m)^\s*use\s+(?:(?:crate|super|self)::)?([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)\s*;",
            )
            .unwrap()
        });
        static RUST_USE_GROUP_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?m)^\s*use\s+(?:(?:crate|super|self)::)?([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)::\{([^}]+)\}\s*;").unwrap()
        });
        let mut local_import_table: HashMap<(String, String), String> = HashMap::default();

        for cap in RUST_USE_SIMPLE_RE.captures_iter(content) {
            let full_path_str = cap.get(1).unwrap().as_str();
            let parts: Vec<&str> = full_path_str.split("::").collect();
            if parts.is_empty() {
                continue;
            }
            let imported_name = parts[parts.len() - 1];
            let source_module = if parts.len() >= 2 {
                parts[parts.len() - 2]
            } else {
                parts[0]
            };
            resolve_rust_import(
                file_path,
                imported_name,
                source_module,
                symbol_table,
                entity_map,
                &mut local_import_table,
            );
        }

        for cap in RUST_USE_GROUP_RE.captures_iter(content) {
            let module_path = cap.get(1).unwrap().as_str();
            let names_str = cap.get(2).unwrap().as_str();
            let source_module = module_path.rsplit("::").next().unwrap_or(module_path);

            for name_part in names_str.split(',') {
                let name_part = name_part.trim();
                let (original, local) = if let Some(pos) = name_part.find(" as ") {
                    (&name_part[..pos], name_part[pos + 4..].trim())
                } else {
                    (name_part, name_part)
                };
                let original = original.trim();
                let local = local.trim();
                if original.is_empty() || local.is_empty() {
                    continue;
                }

                resolve_rust_import(
                    file_path,
                    original,
                    source_module,
                    symbol_table,
                    entity_map,
                    &mut local_import_table,
                );
                if local != original {
                    if let Some(target) = local_import_table
                        .get(&(file_path.to_string(), original.to_string()))
                        .cloned()
                    {
                        local_import_table
                            .insert((file_path.to_string(), local.to_string()), target);
                    }
                }
            }
        }

        scan.local_imports.extend(local_import_table);
    }

    let file_ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    if let Some(file_config) =
        crate::parser::plugins::code::languages::get_language_config(file_ext)
    {
        if file_config.has_slash_qualified_refs() {
            let clojure_stripped = strip_for_language(file_config.strip_strategy(), content);
            for cap in CLOJURE_REFER_RE.captures_iter(&clojure_stripped) {
                let ns_name = cap.get(1).unwrap().as_str();
                let symbols_str = cap.get(2).unwrap().as_str();
                for symbol in symbols_str.split_whitespace() {
                    let symbol =
                        symbol.trim_matches(|c: char| c == ',' || c == '(' || c == ')');
                    if symbol.is_empty() {
                        continue;
                    }
                    resolve_clojure_require(
                        file_path,
                        ns_name,
                        symbol,
                        symbol_table,
                        entity_map,
                        &mut scan.local_imports,
                    );
                }
            }
            for cap in CLOJURE_AS_RE.captures_iter(&clojure_stripped) {
                let ns_name = cap.get(1).unwrap().as_str();
                let alias = cap.get(2).unwrap().as_str();
                resolve_clojure_as(
                    file_path,
                    ns_name,
                    alias,
                    clojure_ns_index,
                    &mut scan.local_imports,
                );
            }
        }
    }

    scan
}

/// Build import table: maps (file_path, imported_name) → target entity ID.
///
/// Parses `from X import Y` / `import X` / `use X` style statements from entity content
/// and resolves Y to the entity it refers to in the symbol table.
fn build_import_table(
    root: &Path,
    file_paths: &[String],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    pre_parsed_content: Option<&[(String, String, tree_sitter::Tree)]>,
) -> HashMap<(String, String), String> {
    build_import_table_with_default_export_paths(
        root,
        file_paths,
        file_paths,
        symbol_table,
        entity_map,
        pre_parsed_content,
    )
}

fn build_import_table_with_default_export_paths(
    root: &Path,
    file_paths: &[String],
    default_export_file_paths: &[String],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    pre_parsed_content: Option<&[(String, String, tree_sitter::Tree)]>,
) -> HashMap<(String, String), String> {
    let mut pre_parsed_content_map: HashMap<&str, &str> = HashMap::default();
    if let Some(files) = pre_parsed_content {
        pre_parsed_content_map.extend(
            files
                .iter()
                .map(|(fp, content, _)| (fp.as_str(), content.as_str())),
        );
    }
    let import_source_set: HashSet<&str> = file_paths.iter().map(String::as_str).collect();
    let clojure_ns_index = build_clojure_ns_index(entity_map);
    let mut content_file_set: HashSet<String> = file_paths.iter().cloned().collect();
    let mut pre_scanned_files: HashMap<String, ImportFileScan> = HashMap::default();
    if same_file_set(file_paths, default_export_file_paths) {
        content_file_set.extend(default_export_file_paths.iter().cloned());
    } else {
        let mut content_file_queue: Vec<String> = file_paths.to_vec();
        while let Some(file_path) = content_file_queue.pop() {
            if file_path.ends_with(".go") {
                continue;
            }
            if pre_scanned_files.contains_key(file_path.as_str()) {
                continue;
            }

            let Some(content) = import_source_content(root, &pre_parsed_content_map, &file_path)
            else {
                continue;
            };
            for imported_file in js_ts_import_source_files_from_content(
                &file_path,
                content.as_ref(),
                default_export_file_paths,
            ) {
                if content_file_set.insert(imported_file.clone()) {
                    content_file_queue.push(imported_file);
                }
            }
            pre_scanned_files.insert(
                file_path.clone(),
                scan_import_file(
                    &file_path,
                    content.as_ref(),
                    import_source_set.contains(file_path.as_str()),
                    symbol_table,
                    entity_map,
                    &clojure_ns_index,
                ),
            );
        }
    }
    let mut content_file_paths: Vec<String> = content_file_set.into_iter().collect();
    content_file_paths.sort_unstable();

    let mut scans: Vec<ImportFileScan> = if pre_scanned_files.is_empty() {
        maybe_par_iter!(content_file_paths)
            .filter_map(|file_path| {
                if file_path.ends_with(".go") {
                    return None;
                }

                let content = import_source_content(root, &pre_parsed_content_map, file_path)?;
                Some(scan_import_file(
                    file_path,
                    content.as_ref(),
                    import_source_set.contains(file_path.as_str()),
                    symbol_table,
                    entity_map,
                    &clojure_ns_index,
                ))
            })
            .collect()
    } else {
        let mut scans = Vec::with_capacity(content_file_paths.len());
        for file_path in &content_file_paths {
            if file_path.ends_with(".go") {
                continue;
            }
            if let Some(scan) = pre_scanned_files.remove(file_path.as_str()) {
                scans.push(scan);
                continue;
            }

            let Some(content) = import_source_content(root, &pre_parsed_content_map, file_path)
            else {
                continue;
            };
            scans.push(scan_import_file(
                file_path,
                content.as_ref(),
                import_source_set.contains(file_path.as_str()),
                symbol_table,
                entity_map,
                &clojure_ns_index,
            ));
        }
        scans
    };

    let mut default_exports = HashMap::default();
    let mut re_exports = Vec::new();
    let mut named_exports_by_file: HashMap<String, HashSet<String>> = HashMap::default();
    for scan in &mut scans {
        if let Some((file_path, target_id)) = scan.default_export.take() {
            default_exports.insert(file_path, target_id);
        }
        re_exports.append(&mut scan.default_re_exports);
        if let Some((file_path, named_exports)) = scan.named_exports.take() {
            named_exports_by_file.insert(file_path, named_exports);
        }
    }
    resolve_ts_default_re_exports(&mut default_exports, re_exports, symbol_table, entity_map);
    let ts_default_exports = TsDefaultExportTable {
        sorted_files: sorted_default_export_files(&default_exports),
        exports_by_file: default_exports,
    };
    let ts_top_level_entities = OnceLock::new();

    let mut import_table: HashMap<(String, String), String> = HashMap::default();
    for scan in scans {
        for (key, val) in scan.local_imports {
            import_table.insert(key, val);
        }
        for pending in scan.default_imports {
            if let Some(target_id) = resolve_default_export_target(
                &ts_default_exports,
                &pending.module_path,
                &pending.file_path,
            ) {
                import_table.insert((pending.file_path, pending.local_name), target_id);
            }
        }
        for pending in scan.namespace_imports {
            let ts_top_level_entities =
                ts_top_level_entities.get_or_init(|| build_ts_top_level_entity_table(entity_map));
            let Some(target_file) = find_import_file(
                &ts_top_level_entities.sorted_files,
                &pending.module_path,
                &pending.file_path,
                JS_TS_EXTENSIONS,
            ) else {
                continue;
            };
            let Some(entries) = ts_top_level_entities.entities_by_file.get(target_file) else {
                continue;
            };
            let Some(exported_names) = named_exports_by_file.get(target_file) else {
                continue;
            };
            for (name, target_id) in entries {
                if !exported_names.contains(name) {
                    continue;
                }
                import_table.insert(
                    (
                        pending.file_path.clone(),
                        format!("{}.{}", pending.alias, name),
                    ),
                    target_id.clone(),
                );
            }
        }
        for (key, val) in scan.re_export_imports {
            import_table.insert(key, val);
        }
    }

    import_table
}

/// Resolve a Rust import: find the target entity in the symbol table
/// by matching the imported name against entities in files whose stem matches source_module.
fn resolve_rust_import(
    file_path: &str,
    imported_name: &str,
    source_module: &str,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
) {
    if let Some(target_ids) = symbol_table.get(imported_name) {
        let target = target_ids.iter().find(|id| {
            entity_map.get(*id).map_or(false, |e| {
                let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                let stem = strip_file_ext(stem);
                stem == source_module
            })
        });
        if let Some(target_id) = target {
            import_table.insert(
                (file_path.to_string(), imported_name.to_string()),
                target_id.clone(),
            );
        }
    }
}

/// Pre-built index for Clojure namespace resolution.
/// Maps file-path-without-extension → Vec<(entity_name, entity_id)>.
/// Built once before the import-table loop to avoid O(total-entities) scans per :as alias.
type ClojureNsIndex = HashMap<String, Vec<(String, String)>>;

fn build_clojure_ns_index(entity_map: &HashMap<String, EntityInfo>) -> ClojureNsIndex {
    let mut index: ClojureNsIndex = HashMap::default();
    for (entity_id, entity_info) in entity_map {
        let fp = &entity_info.file_path;
        if !fp.ends_with(".clj") && !fp.ends_with(".cljs") && !fp.ends_with(".cljc") {
            continue;
        }
        let path_no_ext = fp.rsplit_once('.').map(|(p, _)| p).unwrap_or(fp.as_str());
        index
            .entry(path_no_ext.to_string())
            .or_default()
            .push((entity_info.name.clone(), entity_id.clone()));
    }
    index
}

/// Resolve one symbol from a Clojure (:require [ns :refer [symbol]]) form.
/// Converts namespace dots to slashes and hyphens to underscores to derive the file path,
/// then matches against entity_map to find the target entity.
fn resolve_clojure_require(
    file_path: &str,
    ns_name: &str,
    symbol: &str,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    local_imports: &mut Vec<((String, String), String)>,
) {
    if ns_name.is_empty() {
        return;
    }
    let Some(target_ids) = symbol_table.get(symbol) else {
        return;
    };
    // www.util → www/util; my-app.core → my_app/core
    let ns_path = ns_name.replace('.', "/").replace('-', "_");
    // Pre-build the three suffixes once to avoid allocating inside the find loop.
    let suffix_clj = format!("{ns_path}.clj");
    let suffix_cljs = format!("{ns_path}.cljs");
    let suffix_cljc = format!("{ns_path}.cljc");
    // Match at a path-component boundary: exact or preceded by '/'.
    // This prevents "notmyapp/core.clj" from matching namespace "myapp.core".
    let ns_matches = |fp: &str, suffix: &str| fp == suffix || fp.ends_with(&format!("/{suffix}"));
    let target = target_ids.iter().find(|id| {
        entity_map.get(*id).map_or(false, |e| {
            let fp = &e.file_path;
            ns_matches(fp, &suffix_clj)
                || ns_matches(fp, &suffix_cljs)
                || ns_matches(fp, &suffix_cljc)
        })
    });
    if let Some(target_id) = target {
        local_imports.push((
            (file_path.to_string(), symbol.to_string()),
            target_id.clone(),
        ));
    }
}

/// Resolve all entities from a Clojure namespace aliased with `:as alias`.
/// Adds `(importing_file, "alias/entity_name")` → entity_id entries so that
/// namespace-qualified calls like `(alias/fn-name ...)` are resolved via the
/// import table when `resolve_entity_references` scans for `alias/name` patterns.
fn resolve_clojure_as(
    file_path: &str,
    ns_name: &str,
    alias: &str,
    ns_index: &ClojureNsIndex,
    local_imports: &mut Vec<((String, String), String)>,
) {
    let ns_path = ns_name.replace('.', "/").replace('-', "_");
    let ns_path_suffix = format!("/{}", ns_path);
    for (path_no_ext, entities) in ns_index {
        if path_no_ext == &ns_path || path_no_ext.ends_with(&ns_path_suffix) {
            for (entity_name, entity_id) in entities {
                local_imports.push((
                    (file_path.to_string(), format!("{}/{}", alias, entity_name)),
                    entity_id.clone(),
                ));
            }
        }
    }
}

/// Strip Clojure semicolon line comments from already-string-blanked content.
/// `strip_comments_and_strings` blanks string literals but does not remove `;` line comments,
/// so any remaining `;` after that call is a real comment start.
fn strip_clojure_line_comments(s: &str) -> String {
    // `.lines()` in Rust drops the final empty element produced by a trailing '\n',
    // so `join("\n")` loses exactly one trailing newline — the push restores it.
    // No double-newline: "a\nb\n" → lines=["a","b"] → join="a\nb" → push → "a\nb\n".
    let mut result = s
        .lines()
        .map(|line| line.find(';').map_or(line, |pos| &line[..pos]))
        .collect::<Vec<_>>()
        .join("\n");
    if s.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Strip common file extensions from a filename.
fn strip_file_ext(s: &str) -> &str {
    s.strip_suffix(".py")
        .or_else(|| s.strip_suffix(".ts"))
        .or_else(|| s.strip_suffix(".js"))
        .or_else(|| s.strip_suffix(".tsx"))
        .or_else(|| s.strip_suffix(".jsx"))
        .or_else(|| s.strip_suffix(".rs"))
        .unwrap_or(s)
}

/// Strip comments and string literals from content to avoid false references.
/// Returns a new string with comments/docstrings replaced by spaces.
fn blank_span_preserving_newlines(result: &mut [u8], bytes: &[u8], start: usize, end: usize) {
    for idx in start..end.min(bytes.len()) {
        result[idx] = if bytes[idx] == b'\n' { b'\n' } else { b' ' };
    }
}

fn strip_comments_and_strings(content: &str) -> String {
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut result = vec![b' '; len];
    let mut i = 0;

    while i < len {
        // Triple-quoted strings (Python docstrings)
        if i + 2 < len && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
            let span_start = i;
            i += 3;
            while i < len {
                if i + 2 < len && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    i += 3;
                    break;
                }
                if bytes[i] == b'\n' {
                    result[i] = b'\n';
                }
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        if i + 2 < len && bytes[i] == b'\'' && bytes[i + 1] == b'\'' && bytes[i + 2] == b'\'' {
            let span_start = i;
            i += 3;
            while i < len {
                if i + 2 < len
                    && bytes[i] == b'\''
                    && bytes[i + 1] == b'\''
                    && bytes[i + 2] == b'\''
                {
                    i += 3;
                    break;
                }
                if bytes[i] == b'\n' {
                    result[i] = b'\n';
                }
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        // Double-quoted strings
        if bytes[i] == b'"' {
            let span_start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(len);
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                if bytes[i] == b'\n' {
                    result[i] = b'\n';
                }
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        // Single-quoted strings
        if bytes[i] == b'\'' {
            let span_start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(len);
                    continue;
                }
                if bytes[i] == b'\'' {
                    i += 1;
                    break;
                }
                if bytes[i] == b'\n' {
                    result[i] = b'\n';
                }
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        // Python/Ruby single-line comments
        if bytes[i] == b'#' {
            let span_start = i;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        // C-style single-line comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let span_start = i;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        // C-style block comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let span_start = i;
            i += 2;
            while i < len {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        // Regular code: copy through
        result[i] = bytes[i];
        i += 1;
    }

    String::from_utf8(result).expect("stripped source preserves UTF-8 boundaries")
}

/// Strip double-quoted string literals from Clojure content, preserving everything else.
///
/// Unlike `strip_comments_and_strings`, this does NOT treat `#` as a line comment.
/// In Clojure, `#` is used for gensyms (`result#`), reader dispatch (`#?`, `#{}`), and
/// other reader macros — not for comments. Treating it as a comment would blank out
/// everything after e.g. `result#` on a line, including calls like
/// `(rewrite/add-expected-value! ...)` that follow in the same binding form.
///
/// Semicolon line comments must be handled separately via `strip_clojure_line_comments`.
fn strip_clojure_content(content: &str) -> String {
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut result = vec![b' '; len];
    let mut i = 0;

    while i < len {
        // Double-quoted strings: blank them (preserving newlines for line alignment)
        if bytes[i] == b'"' {
            let span_start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(len);
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            blank_span_preserving_newlines(&mut result, bytes, span_start, i);
            continue;
        }
        // Regular code: copy through
        result[i] = bytes[i];
        i += 1;
    }

    String::from_utf8(result).expect("stripped source preserves UTF-8 boundaries")
}

/// Dispatch to the appropriate content stripper for the given language strategy.
/// Each `StripStrategy` variant must be handled explicitly.
fn strip_for_language(
    strategy: crate::parser::plugins::code::languages::StripStrategy,
    content: &str,
) -> String {
    use crate::parser::plugins::code::languages::StripStrategy;
    match strategy {
        StripStrategy::Generic => strip_comments_and_strings(content),
        StripStrategy::Clojure => strip_clojure_line_comments(&strip_clojure_content(content)),
    }
}

/// Extract dot-chains (receiver.member) from content for precise resolution.
/// Returns unique (receiver, member) pairs found in the content.
fn extract_dot_chains<'a>(content: &'a str) -> Vec<(&'a str, &'a str)> {
    extract_dot_chains_with_positions(content)
        .into_iter()
        .map(|(receiver, member, _, _, _)| (receiver, member))
        .collect()
}

/// Returns unique receiver/member pairs with one-based content lines and byte offsets.
fn extract_dot_chains_with_positions<'a>(
    content: &'a str,
) -> Vec<(&'a str, &'a str, usize, usize, usize)> {
    static DOT_CHAIN_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b([A-Za-z_]\w*)\.([A-Za-z_]\w*)").unwrap());

    let mut chains = Vec::new();
    let mut seen: HashSet<(&str, &str, usize, usize)> = HashSet::default();
    for cap in DOT_CHAIN_RE.captures_iter(content) {
        let matched = cap.get(0).unwrap();
        let line = line_for_byte(content, matched.start());
        let receiver = cap.get(1).unwrap().as_str();
        let member = cap.get(2).unwrap().as_str();
        if seen.insert((receiver, member, line, matched.start())) {
            chains.push((receiver, member, line, matched.start(), matched.end()));
        }
    }
    chains
}

fn local_binding_names_filtered<F>(
    content: &str,
    ext: &str,
    mut include_token: F,
) -> HashSet<String>
where
    F: FnMut(usize, usize, usize) -> bool,
{
    let mut names = HashSet::default();
    if !matches!(ext, ".js" | ".jsx" | ".ts" | ".tsx" | ".py" | ".swift") {
        return names;
    }

    let mut line_no = 1;
    let mut line_start = 0;
    for chunk in content.split_inclusive('\n') {
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        match ext {
            ".js" | ".jsx" | ".ts" | ".tsx" | ".swift" => {
                collect_local_binding_captures(
                    line,
                    line_no,
                    line_start,
                    &JS_TS_SWIFT_LOCAL_DECL_RE,
                    &mut include_token,
                    &mut names,
                );
            }
            ".py" => {
                collect_python_local_bindings(
                    line,
                    line_no,
                    line_start,
                    &mut include_token,
                    &mut names,
                );
            }
            _ => {}
        }
        line_start += chunk.len();
        line_no += 1;
    }

    names
}

static JS_TS_SWIFT_LOCAL_DECL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:const|let|var)\s+([A-Za-z_]\w*)").unwrap());

static PY_LOCAL_ASSIGN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([A-Za-z_]\w*)\s*(?::[^=]+)?([+\-*/%&|^]?=)").unwrap());

static PY_FOR_BINDING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*for\s+([A-Za-z_]\w*)\s+in\b").unwrap());

// Clojure `:require [ns :refer [sym1 sym2]]` — matches inside any require form.
// `[^\[\]]*` prevents crossing both `[` and `]` boundaries, so the regex cannot
// span from one require form's namespace into a later form's :refer list.
static CLOJURE_REFER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[([a-zA-Z][a-zA-Z0-9._-]*)\b[^\[\]]*:refer\s+\[([^\]]+)\]").unwrap()
});

// Clojure `:require [ns :as alias]` — matches inside any require form.
// `[^\]]*` prevents crossing bracket boundaries.
static CLOJURE_AS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[([a-zA-Z][a-zA-Z0-9._-]*)\b[^\]]*:as\s+([a-zA-Z][a-zA-Z0-9_-]*)").unwrap()
});

// Clojure `alias/name` qualified references, e.g. `u/vectorize-if-not-sequential`.
// Alias and name may contain hyphens, `?` (predicates), or `!` (bang fns).
static CLOJURE_QUALIFIED_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([a-zA-Z][a-zA-Z0-9_?!=*-]*/[a-zA-Z][a-zA-Z0-9_?!=*-]*)").unwrap()
});

fn collect_local_binding_captures<F>(
    line: &str,
    line_no: usize,
    line_start: usize,
    regex: &Regex,
    include_token: &mut F,
    names: &mut HashSet<String>,
) where
    F: FnMut(usize, usize, usize) -> bool,
{
    for cap in regex.captures_iter(line) {
        if let Some(name_match) = cap.get(1) {
            maybe_add_local_binding_name(
                name_match.as_str(),
                line_no,
                line_start,
                name_match,
                include_token,
                names,
            );
        }
    }
}

fn collect_python_local_bindings<F>(
    line: &str,
    line_no: usize,
    line_start: usize,
    include_token: &mut F,
    names: &mut HashSet<String>,
) where
    F: FnMut(usize, usize, usize) -> bool,
{
    if let Some(cap) = PY_LOCAL_ASSIGN_RE.captures(line) {
        if let (Some(name_match), Some(op_match)) = (cap.get(1), cap.get(2)) {
            if line.as_bytes().get(op_match.end()) != Some(&b'=') {
                maybe_add_local_binding_name(
                    name_match.as_str(),
                    line_no,
                    line_start,
                    name_match,
                    include_token,
                    names,
                );
            }
        }
    }

    collect_local_binding_captures(
        line,
        line_no,
        line_start,
        &PY_FOR_BINDING_RE,
        include_token,
        names,
    );
}

fn maybe_add_local_binding_name<F>(
    name: &str,
    line_no: usize,
    line_start: usize,
    name_match: regex::Match<'_>,
    include_token: &mut F,
    names: &mut HashSet<String>,
) where
    F: FnMut(usize, usize, usize) -> bool,
{
    if !is_reference_word(name) {
        return;
    }
    let start = line_start + name_match.start();
    let end = line_start + name_match.end();
    if include_token(line_no, start, end) {
        names.insert(name.to_string());
    }
}

/// Returns the extra identifier characters for a given file path.
/// Clojure uses '-' as a word character; all other languages use none.
fn extra_ident_chars_for_file(file_path: &str) -> &'static [char] {
    let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    crate::parser::plugins::code::languages::get_language_config(ext)
        .map_or(&[], |c| c.extra_ident_chars())
}

fn strip_strategy_for_file(
    file_path: &str,
) -> crate::parser::plugins::code::languages::StripStrategy {
    let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
    crate::parser::plugins::code::languages::get_language_config(ext).map_or(
        crate::parser::plugins::code::languages::StripStrategy::Generic,
        |c| c.strip_strategy(),
    )
}

/// Extract identifier references from entity content using simple token analysis.
/// Strips comments and strings first to avoid false positives from docstrings.
/// Returns borrowed slices from the stripped content.
fn extract_references_from_content<'a>(
    content: &'a str,
    own_name: &str,
    extra_ident_chars: &'static [char],
    strip_strategy: crate::parser::plugins::code::languages::StripStrategy,
) -> Vec<&'a str> {
    let stripped = strip_for_language(strip_strategy, content);
    extract_references_with_stripped(content, own_name, &stripped, extra_ident_chars)
}

/// Yields each contiguous run of identifier characters (alphanumeric, `_`, or `extra`) as a
/// `&str` slice. Used by `text_mentions_any_name` and `content_contains_identifier` to avoid
/// duplicating the same char-walk state machine.
fn token_iter<'a>(text: &'a str, extra: &'static [char]) -> impl Iterator<Item = &'a str> + 'a {
    let mut token_start: Option<usize> = None;
    let mut char_iter = text.char_indices();
    std::iter::from_fn(move || loop {
        match char_iter.next() {
            None => {
                return token_start.take().map(|s| &text[s..]);
            }
            Some((idx, ch)) => {
                if ch.is_alphanumeric() || ch == '_' || extra.contains(&ch) {
                    token_start.get_or_insert(idx);
                } else if let Some(s) = token_start.take() {
                    return Some(&text[s..idx]);
                }
            }
        }
    })
}

fn text_mentions_any_name(
    text: &str,
    names: &HashSet<&str>,
    extra_ident_chars: &'static [char],
) -> bool {
    token_iter(text, extra_ident_chars).any(|t| names.contains(t))
}

fn content_contains_identifier(
    content: &str,
    identifier: &str,
    extra_ident_chars: &'static [char],
) -> bool {
    token_iter(content, extra_ident_chars).any(|t| t == identifier)
}

const IMPORT_SCAN_PREFIX_LINES: usize = 80;

fn read_import_scan_prefix(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut content = String::new();
    for line in std::io::BufReader::new(file)
        .lines()
        .take(IMPORT_SCAN_PREFIX_LINES)
    {
        content.push_str(&line.ok()?);
        content.push('\n');
    }
    Some(content)
}

fn content_import_tokens_for_file(
    importing_file_path: &str,
    content: &str,
    candidate_file_path: &str,
) -> Vec<String> {
    let mut tokens = Vec::new();

    if importing_file_path.ends_with(".py") {
        for line in content.lines() {
            let trimmed = line.split('#').next().unwrap_or("").trim();
            if let Some(rest) = trimmed.strip_prefix("from ") {
                let Some(import_pos) = rest.find(" import ") else {
                    continue;
                };
                let source_path = rest[..import_pos].trim();
                if !import_source_matches_file(
                    importing_file_path,
                    source_path,
                    &[".py"],
                    candidate_file_path,
                ) {
                    continue;
                }

                let names = rest[import_pos + " import ".len()..].trim();
                for import_part in names.split(',') {
                    let import_part = import_part
                        .trim()
                        .trim_matches(|c: char| c == '(' || c == ')' || c == ',');
                    if import_part.is_empty() {
                        continue;
                    }
                    let (original, local) = split_import_alias(import_part);
                    push_import_token(&mut tokens, original);
                    push_import_token(&mut tokens, local);
                }
            } else if let Some(rest) = trimmed.strip_prefix("import ") {
                for import_part in rest.split(',') {
                    let import_part = import_part.trim();
                    let (source_path, alias) = split_import_alias(import_part);
                    let source_path = source_path.split_whitespace().next().unwrap_or("").trim();
                    if source_path.is_empty()
                        || !import_source_matches_file(
                            importing_file_path,
                            source_path,
                            &[".py"],
                            candidate_file_path,
                        )
                    {
                        continue;
                    }

                    let default_local = source_path.split('.').next().unwrap_or(source_path);
                    push_import_token(&mut tokens, alias);
                    push_import_token(&mut tokens, default_local);
                }
            }
        }
    }

    if importing_file_path.ends_with(".js")
        || importing_file_path.ends_with(".ts")
        || importing_file_path.ends_with(".jsx")
        || importing_file_path.ends_with(".tsx")
    {
        static JS_NAMED_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r#"import\s+(?:type\s+)?(?:[A-Za-z_$][\w$]*\s*,\s*)?\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#,
            )
            .unwrap()
        });
        static JS_NAMESPACE_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"import\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s*['"]([^'"]+)['"]"#)
                .unwrap()
        });
        static JS_DEFAULT_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r#"import\s+(?:type\s+)?([A-Za-z_$][\w$]*)(?:\s*,\s*\{[^}]*\})?\s*from\s*['"]([^'"]+)['"]"#,
            )
            .unwrap()
        });
        static JS_REEXPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"export\s+(?:type\s+)?\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#).unwrap()
        });

        for cap in JS_NAMED_IMPORT_RE.captures_iter(content) {
            let names = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let source_path = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if !import_source_matches_file(
                importing_file_path,
                source_path,
                &[".ts", ".tsx", ".js", ".jsx"],
                candidate_file_path,
            ) {
                continue;
            }
            for name_part in names.split(',') {
                let name_part = name_part.trim();
                let name_part = name_part.strip_prefix("type ").unwrap_or(name_part);
                let (original, local) = split_import_alias(name_part);
                push_import_token(&mut tokens, original);
                push_import_token(&mut tokens, local);
            }
        }

        for cap in JS_NAMESPACE_IMPORT_RE.captures_iter(content) {
            let alias = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let source_path = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if import_source_matches_file(
                importing_file_path,
                source_path,
                &[".ts", ".tsx", ".js", ".jsx"],
                candidate_file_path,
            ) {
                push_import_token(&mut tokens, alias);
            }
        }

        for cap in JS_DEFAULT_IMPORT_RE.captures_iter(content) {
            let local = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let source_path = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if import_source_matches_file(
                importing_file_path,
                source_path,
                &[".ts", ".tsx", ".js", ".jsx"],
                candidate_file_path,
            ) {
                push_import_token(&mut tokens, local);
            }
        }

        for cap in JS_REEXPORT_RE.captures_iter(content) {
            let names = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let source_path = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if !import_source_matches_file(
                importing_file_path,
                source_path,
                &[".ts", ".tsx", ".js", ".jsx"],
                candidate_file_path,
            ) {
                continue;
            }
            for name_part in names.split(',') {
                let name_part = name_part.trim();
                let name_part = name_part.strip_prefix("type ").unwrap_or(name_part);
                let (original, local) = split_import_alias(name_part);
                push_import_token(&mut tokens, original);
                push_import_token(&mut tokens, local);
            }
        }
    }

    tokens
}

fn split_import_alias(import_part: &str) -> (&str, &str) {
    if let Some(pos) = import_part.find(" as ") {
        let original = import_part[..pos].trim();
        let local = import_part[pos + 4..].trim();
        (original, local)
    } else {
        let name = import_part.split_whitespace().next().unwrap_or("").trim();
        (name, name)
    }
}

fn push_import_token(tokens: &mut Vec<String>, token: &str) {
    let token = token.trim();
    if !token.is_empty() && token != "*" {
        tokens.push(token.to_string());
    }
}

/// Extract references using a pre-stripped version of the content.
/// Use this when you already have the stripped content (e.g. from dot-chain extraction)
/// to avoid stripping comments/strings twice.
fn extract_references_with_stripped<'a>(
    content: &'a str,
    own_name: &str,
    stripped: &str,
    extra_ident_chars: &'static [char],
) -> Vec<&'a str> {
    extract_references_with_stripped_filtered(
        content,
        own_name,
        stripped,
        extra_ident_chars,
        |_, _, _| true,
    )
}

fn extract_references_with_stripped_filtered<'a, F>(
    content: &'a str,
    own_name: &str,
    stripped: &str,
    extra_ident_chars: &'static [char],
    mut include_token: F,
) -> Vec<&'a str>
where
    F: FnMut(usize, usize, usize) -> bool,
{
    let mut refs = Vec::new();
    let mut seen: HashSet<&str> = HashSet::default();
    let mut token_start: Option<usize> = None;
    let mut line = 1;

    for (idx, ch) in content.char_indices() {
        if ch.is_alphanumeric() || ch == '_' || extra_ident_chars.contains(&ch) {
            if token_start.is_none() {
                token_start = Some(idx);
            }
            continue;
        }

        if let Some(start) = token_start.take() {
            maybe_push_reference_token(
                content,
                stripped,
                start,
                idx,
                line,
                own_name,
                &mut seen,
                &mut refs,
                &mut include_token,
            );
        }

        if ch == '\n' {
            line += 1;
        }
    }

    if let Some(start) = token_start {
        maybe_push_reference_token(
            content,
            stripped,
            start,
            content.len(),
            line,
            own_name,
            &mut seen,
            &mut refs,
            &mut include_token,
        );
    }

    refs
}

fn maybe_push_reference_token<'a, F>(
    content: &'a str,
    stripped: &str,
    start: usize,
    end: usize,
    line: usize,
    own_name: &str,
    seen: &mut HashSet<&'a str>,
    refs: &mut Vec<&'a str>,
    include_token: &mut F,
) where
    F: FnMut(usize, usize, usize) -> bool,
{
    let word = &content[start..end];
    if word.is_empty() || word == own_name {
        return;
    }
    if is_keyword(word) || word.len() < 2 {
        return;
    }
    // Skip very short lowercase identifiers (likely local vars: i, x, a, ok, id, etc.)
    if word.starts_with(|c: char| c.is_lowercase()) && word.len() < 3 {
        return;
    }
    // Reject purely symbolic tokens (e.g. `*` used as arithmetic in Clojure).
    if word
        .chars()
        .all(|c| !c.is_alphanumeric() && c != '_' && c != '-')
    {
        return;
    }
    // Tokens starting with '-' or '*' only appear here for Clojure-family files
    // because `extra_ident_chars_for_file` controls tokenization upstream.
    // '?', '!', '=' only appear as suffixes in symbols such as empty?, reset!,
    // and not=, so the start-character check below suffices.
    if !word.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '-' || c == '*') {
        return;
    }
    // Skip common local variable names that create false graph edges
    if is_common_local_name(word) {
        return;
    }
    // Skip words that only appear in comments/strings
    if stripped.get(start..end) != Some(word) {
        return;
    }
    if !include_token(line, start, end) {
        return;
    }
    if seen.insert(word) {
        refs.push(word);
    }
}

static COMMON_LOCAL_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "result", "results", "data", "config", "value", "values", "item", "items", "input",
        "output", "args", "opts", "name", "path", "file", "line", "count", "index", "temp", "prev",
        "next", "curr", "current", "node", "left", "right", "root", "head", "tail", "body", "text",
        "content", "source", "target", "entry", "error", "errors", "message", "response",
        "request", "context", "state", "props", "event", "handler", "callback", "options",
        "params", "query", "list", "base", "info", "meta", "kind", "mode", "flag", "size",
        "length", "width", "height", "start", "stop", "begin", "done", "found", "status", "code",
    ]
    .into_iter()
    .collect()
});

/// Names that are overwhelmingly local variables, not entity references.
/// These create massive false-positive edges in the dependency graph.
fn is_common_local_name(word: &str) -> bool {
    COMMON_LOCAL_NAMES.contains(word)
}

/// Infer reference type from context using word-boundary-aware matching.
fn infer_ref_type(content: &str, ref_name: &str) -> RefType {
    // Check if it's a function call: ref_name followed by ( with word boundary before.
    // Avoids format! allocation by finding ref_name and checking the next char.
    let bytes = content.as_bytes();
    let name_bytes = ref_name.as_bytes();
    let mut search_start = 0;
    while let Some(rel_pos) = content[search_start..].find(ref_name) {
        let pos = search_start + rel_pos;
        let after = pos + name_bytes.len();
        // Check next char is '('
        if after < bytes.len() && bytes[after] == b'(' {
            // Verify word boundary before
            let is_boundary = pos == 0 || {
                let prev = bytes[pos - 1];
                !prev.is_ascii_alphanumeric() && prev != b'_'
            };
            if is_boundary {
                return RefType::Calls;
            }
        }
        // Advance past pos to the next char boundary to avoid slicing inside a multi-byte UTF-8 char.
        search_start = pos + 1;
        while search_start < content.len() && !content.is_char_boundary(search_start) {
            search_start += 1;
        }
    }

    // Check if it's in an import/use statement (line-level, not substring)
    for line in content.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("import ")
            || trimmed.starts_with("use ")
            || trimmed.starts_with("from ")
            || trimmed.starts_with("require("))
            && trimmed.contains(ref_name)
        {
            return RefType::Imports;
        }
    }

    // Default to type reference
    RefType::TypeRef
}

static KEYWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // Common across languages
        "if",
        "else",
        "for",
        "while",
        "do",
        "switch",
        "case",
        "break",
        "continue",
        "return",
        "try",
        "catch",
        "finally",
        "throw",
        "new",
        "delete",
        "typeof",
        "instanceof",
        "in",
        "of",
        "true",
        "false",
        "null",
        "undefined",
        "void",
        "this",
        "super",
        "class",
        "extends",
        "implements",
        "interface",
        "enum",
        "const",
        "let",
        "var",
        "function",
        "async",
        "await",
        "yield",
        "import",
        "export",
        "default",
        "from",
        "as",
        "static",
        "public",
        "private",
        "protected",
        "abstract",
        "final",
        "override",
        // Rust
        "fn",
        "pub",
        "mod",
        "use",
        "struct",
        "impl",
        "trait",
        "where",
        "type",
        "self",
        "Self",
        "mut",
        "ref",
        "match",
        "loop",
        "move",
        "unsafe",
        "extern",
        "crate",
        "dyn",
        // Python
        "def",
        "elif",
        "except",
        "raise",
        "with",
        "pass",
        "lambda",
        "nonlocal",
        "global",
        "assert",
        "True",
        "False",
        "and",
        "or",
        "not",
        "is",
        // Go
        "func",
        "package",
        "range",
        "select",
        "chan",
        "go",
        "defer",
        "map",
        "make",
        "append",
        "len",
        "cap",
        // C/C++
        "auto",
        "register",
        "volatile",
        "sizeof",
        "typedef",
        "template",
        "typename",
        "namespace",
        "virtual",
        "inline",
        "constexpr",
        "nullptr",
        "noexcept",
        "explicit",
        "friend",
        "operator",
        "using",
        "cout",
        "endl",
        "cerr",
        "cin",
        "printf",
        "scanf",
        "malloc",
        "free",
        "NULL",
        "include",
        "ifdef",
        "ifndef",
        "endif",
        "define",
        "pragma",
        // Ruby
        "end",
        "then",
        "elsif",
        "unless",
        "until",
        "begin",
        "rescue",
        "ensure",
        "when",
        "require",
        "attr_accessor",
        "attr_reader",
        "attr_writer",
        "puts",
        "nil",
        "module",
        "defined",
        // C#
        "internal",
        "sealed",
        "readonly",
        "partial",
        "delegate",
        "event",
        "params",
        "out",
        "object",
        "decimal",
        "sbyte",
        "ushort",
        "uint",
        "ulong",
        "nint",
        "nuint",
        "dynamic",
        "get",
        "set",
        "value",
        "init",
        "record",
        // Types (primitives)
        "string",
        "number",
        "boolean",
        "int",
        "float",
        "double",
        "bool",
        "char",
        "byte",
        "i8",
        "i16",
        "i32",
        "i64",
        "u8",
        "u16",
        "u32",
        "u64",
        "f32",
        "f64",
        "usize",
        "isize",
        "str",
        "String",
        "Vec",
        "Option",
        "Result",
        "Box",
        "Arc",
        "Rc",
        "HashMap",
        "HashSet",
        "Some",
        "Ok",
        "Err",
    ]
    .into_iter()
    .collect()
});

fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{FileChange, FileStatus};
    use crate::parser::plugins::code::languages::StripStrategy;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, ParserRegistry) {
        let dir = TempDir::new().unwrap();
        let registry = crate::parser::plugins::create_default_registry();
        (dir, registry)
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    fn dependency_ids(graph: &EntityGraph, entity_id: &str) -> Vec<String> {
        let mut ids = graph
            .get_dependencies(entity_id)
            .into_iter()
            .map(|entity| entity.id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    fn assert_direct_dependencies_match_full(
        root: &Path,
        files: &[String],
        registry: &ParserRegistry,
        entity_id: &str,
    ) {
        let (full_graph, _) = EntityGraph::build(root, files, registry);
        let expected = dependency_ids(&full_graph, entity_id);
        let (direct_graph, _) =
            EntityGraph::build_direct_dependencies(root, files, registry, |entity| {
                entity.id == entity_id
            });
        let actual = dependency_ids(&direct_graph, entity_id);
        assert_eq!(actual, expected);
    }

    fn graph_json_payload(graph: &EntityGraph) -> serde_json::Value {
        let mut entities = graph.entities.values().collect::<Vec<_>>();
        entities.sort_by(|a, b| a.id.cmp(&b.id));

        let mut edges = graph.edges.iter().collect::<Vec<_>>();
        edges.sort_by(|a, b| {
            a.from_entity
                .cmp(&b.from_entity)
                .then_with(|| a.to_entity.cmp(&b.to_entity))
                .then_with(|| {
                    test_ref_type_sort_key(&a.ref_type).cmp(&test_ref_type_sort_key(&b.ref_type))
                })
        });

        serde_json::json!({
            "entities": entities,
            "edges": edges,
            "stats": {
                "entityCount": graph.entities.len(),
                "edgeCount": graph.edges.len(),
            },
        })
    }

    fn test_ref_type_sort_key(ref_type: &RefType) -> u8 {
        match ref_type {
            RefType::Calls => 0,
            RefType::Imports => 1,
            RefType::TypeRef => 2,
        }
    }

    fn deep_typescript(depth: usize) -> String {
        let mut content = String::from("class L0 {\n");
        for i in 1..depth {
            content.push_str(&"  ".repeat(i));
            content.push_str(&format!("L{i} = class {{\n"));
        }
        content.push_str(&"  ".repeat(depth));
        content.push_str("method() { return 1; }\n");
        for i in (1..depth).rev() {
            content.push_str(&"  ".repeat(i));
            content.push_str("};\n");
        }
        content.push_str("}\n");
        content
    }

    #[test]
    fn test_file_reference_index_matches_reference_helpers() {
        let content = "\
import { Foo } from './foo';
class Runner {
    run() { return this.validate(Foo); }
    validate(input) { return input; }
}
";
        let index = FileReferenceIndex::from_content(content, &[]);
        let refs = index.refs_with_types_in_ranges(&[(1, 5)], "run");
        assert!(refs.iter().any(|(word, _)| *word == "Foo"));
        assert!(refs.iter().any(|(word, _)| *word == "Runner"));
        assert!(!refs.iter().any(|(word, _)| *word == "input"));

        let dot_chains = index.dot_chains_in_ranges(&[(1, 5)]);
        assert!(dot_chains.contains(&("this", "validate")));
        assert!(refs
            .iter()
            .any(|(word, ref_type)| *word == "Foo" && *ref_type == RefType::Imports));
        assert!(refs
            .iter()
            .any(|(word, ref_type)| *word == "validate" && *ref_type == RefType::Calls));
    }

    #[test]
    fn test_js_ts_import_token_scan_matches_supported_import_forms() {
        let content = "\
import type { X as TypeX } from './stale';
import DefaultThing, { X as Y, Z } from './stale';
import * as ns$ from './stale';
export { default as PublicDefault, X as PublicX } from './stale';
";

        let mut tokens = content_import_tokens_for_file("consumer.ts", content, "stale.ts");
        tokens.sort_unstable();
        tokens.dedup();

        for expected in [
            "X",
            "TypeX",
            "DefaultThing",
            "Y",
            "Z",
            "ns$",
            "default",
            "PublicDefault",
            "PublicX",
        ] {
            assert!(
                tokens.iter().any(|token| token == expected),
                "missing token {expected}; tokens: {tokens:?}"
            );
        }
    }

    #[test]
    fn test_direct_reference_ranges_skip_nested_child_entities() {
        let parent = SemanticEntity {
            id: "parent".to_string(),
            file_path: "sample.ts".to_string(),
            entity_type: "function".to_string(),
            name: "outer".to_string(),
            parent_id: None,
            content: String::new(),
            content_hash: String::new(),
            structural_hash: None,
            start_line: 1,
            end_line: 7,
            metadata: None,
        };
        let mut child_line_ranges = HashMap::default();
        child_line_ranges.insert("parent".to_string(), vec![(3, 5)]);

        let ranges = direct_reference_line_ranges(&parent, parent.end_line, &child_line_ranges);
        assert_eq!(ranges, vec![(1, 2), (6, 7)]);

        let content = "\
function outer() {
  setup();
  function inner() {
    nested();
  }
  finish();
}
";
        let index = FileReferenceIndex::from_content(content, &[]);
        let refs = index.refs_with_types_in_ranges(&ranges, "outer");

        assert!(refs.iter().any(|(word, _)| *word == "setup"));
        assert!(refs.iter().any(|(word, _)| *word == "finish"));
        assert!(!refs.iter().any(|(word, _)| *word == "nested"));
    }

    #[test]
    fn test_deep_nested_typescript_graph_builds() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        let depth = 160;
        write_file(root, "deep.ts", &deep_typescript(depth));

        let (graph, entities) = EntityGraph::build(root, &["deep.ts".into()], &registry);

        assert!(graph.entities.contains_key("deep.ts::class::L0"));
        assert!(entities.iter().any(|e| e.name == "method"));
        assert_eq!(entities.len(), depth + 1);
    }

    #[test]
    fn test_chunked_scope_resolution_keeps_cross_chunk_import_edges() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        let mut files = Vec::new();
        for index in 0..10 {
            let file_name = format!("file_{index}.ts");
            let content = if index == 0 {
                "export function target() { return 1; }\n".to_string()
            } else if index == 9 {
                "import { target } from './file_0';\nexport function caller() { return target(); }\n"
                    .to_string()
            } else {
                format!("export function filler_{index}() {{ return {index}; }}\n")
            };
            write_file(root, &file_name, &content);
            files.push(file_name);
        }

        let (graph, _) = EntityGraph::build(root, &files, &registry);
        let caller_id = graph
            .entities
            .iter()
            .find(|(_, entity)| entity.name == "caller")
            .map(|(id, _)| id.clone())
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(&caller_id);

        assert!(
            deps.iter().any(|dep| dep.name == "target"),
            "caller should resolve imported target across scope chunks. Deps: {:?}",
            deps.iter().map(|dep| &dep.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_ts_class_extends_type_ref_survives_scope_fallback_bound() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "types.ts",
            "\
class Base {}
class Child extends Base {
    run() { return 1; }
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["types.ts".into()], &registry);

        assert!(
            graph.edges.iter().any(|edge| {
                edge.from_entity.contains("Child")
                    && graph
                        .entities
                        .get(&edge.to_entity)
                        .map_or(false, |e| e.name == "Base")
            }),
            "Child should keep a type-ref edge to Base. Edges: {:?}",
            graph.edges
        );
    }

    #[test]
    fn test_multiline_block_comment_preserves_reference_line_index() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "calls.c",
            "\
/*
 multiline
 comment
*/
int helper() { return 1; }
int caller() { return helper(); }
",
        );

        let (graph, _) = EntityGraph::build(root, &["calls.c".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter().any(|dep| dep.name == "helper"),
            "caller should depend on helper after a multiline block comment. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_strip_comments_and_strings_preserves_newlines_and_utf8() {
        let content = "\
const value = \"é\\
still string\";
const done = call();
/* unterminated block
comment with Helper
";

        let stripped = strip_comments_and_strings(content);
        let newline_count = |text: &str| text.bytes().filter(|byte| *byte == b'\n').count();

        assert_eq!(newline_count(&stripped), newline_count(content));
        assert!(stripped.contains("const done = call();"));
        assert!(!stripped.contains("still string"));
        assert!(!stripped.contains("Helper"));

        let trailing_escape = strip_comments_and_strings("const value = \"unterminated\\");
        assert_eq!(newline_count(&trailing_escape), 0);

        let triple = strip_comments_and_strings("'''doc\nwith Helper");
        assert_eq!(newline_count(&triple), 1);
        assert!(!triple.contains("Helper"));
    }

    #[test]
    fn test_incremental_add_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // Start with one file
        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\n");

        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 2);

        // Add a new file
        write_file(root, "c.ts", "export function baz() { return foo(); }\n");
        graph.update_from_changes(
            &[FileChange {
                file_path: "c.ts".into(),
                status: FileStatus::Added,
                old_file_path: None,
                before_content: None,
                after_content: None, // will read from disk
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 3);
        assert!(graph.entities.contains_key("c.ts::function::baz"));
        // baz references foo
        let baz_deps = graph.get_dependencies("c.ts::function::baz");
        assert!(
            baz_deps.iter().any(|d| d.name == "foo"),
            "baz should depend on foo. Deps: {:?}",
            baz_deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_incremental_delete_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\n");

        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 2);

        // Delete b.ts
        graph.update_from_changes(
            &[FileChange {
                file_path: "b.ts".into(),
                status: FileStatus::Deleted,
                old_file_path: None,
                before_content: None,
                after_content: None,
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 1);
        assert!(!graph.entities.contains_key("b.ts::function::bar"));
        // foo's dependency on bar should be pruned
        let foo_deps = graph.get_dependencies("a.ts::function::foo");
        assert!(
            foo_deps.is_empty(),
            "foo's deps should be empty after bar deleted. Deps: {:?}",
            foo_deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_incremental_modify_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(
            root,
            "b.ts",
            "export function bar() { return 1; }\nexport function baz() { return 2; }\n",
        );

        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 3);

        // Modify a.ts to call baz instead of bar
        write_file(root, "a.ts", "export function foo() { return baz(); }\n");
        graph.update_from_changes(
            &[FileChange {
                file_path: "a.ts".into(),
                status: FileStatus::Modified,
                old_file_path: None,
                before_content: None,
                after_content: None,
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 3);
        // foo should now depend on baz, not bar
        let foo_deps = graph.get_dependencies("a.ts::function::foo");
        let dep_names: Vec<&str> = foo_deps.iter().map(|d| d.name.as_str()).collect();
        assert!(
            dep_names.contains(&"baz"),
            "foo should depend on baz after modification. Deps: {:?}",
            dep_names
        );
        assert!(
            !dep_names.contains(&"bar"),
            "foo should no longer depend on bar. Deps: {:?}",
            dep_names
        );
    }

    #[test]
    fn test_incremental_stale_target_file_re_resolves_clean_caller() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.py", "def use_it():\n    return helper()\n");
        write_file(root, "b.py", "def helper():\n    return 1\n");

        let (cached_graph, cached_entities) =
            EntityGraph::build(root, &["a.py".into(), "b.py".into()], &registry);
        assert!(
            cached_graph
                .get_dependents("b.py::function::helper")
                .iter()
                .any(|entity| entity.id == "a.py::function::use_it"),
            "initial graph should include use_it -> helper"
        );

        write_file(
            root,
            "b.py",
            "def helper():\n    return 1\n\n\ndef unrelated():\n    return 42\n",
        );

        let cached_clean_entities = cached_entities
            .iter()
            .filter(|entity| entity.file_path != "b.py")
            .cloned()
            .collect();
        let cached_stale_entities = cached_entities
            .into_iter()
            .filter(|entity| entity.file_path == "b.py")
            .collect();

        let (graph, _) = EntityGraph::build_incremental(
            root,
            &["b.py".into()],
            &["a.py".into(), "b.py".into()],
            cached_clean_entities,
            cached_graph.edges,
            cached_stale_entities,
            &registry,
        );
        let (fresh_graph, _) = EntityGraph::build(root, &["a.py".into(), "b.py".into()], &registry);

        let mut helper_dependents = graph
            .get_dependents("b.py::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        helper_dependents.sort_unstable();
        let mut fresh_dependents = fresh_graph
            .get_dependents("b.py::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        fresh_dependents.sort_unstable();
        assert_eq!(
            helper_dependents, fresh_dependents,
            "incremental graph should match fresh resolution"
        );
        assert!(
            helper_dependents
                .iter()
                .any(|entity_id| *entity_id == "a.py::function::use_it"),
            "clean caller should still depend on content-clean helper. Dependents: {:?}",
            helper_dependents
        );
    }

    #[test]
    fn test_incremental_added_stale_target_re_resolves_clean_reference() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.py", "def use_it():\n    return helper()\n");
        write_file(root, "b.py", "def other():\n    return 1\n");

        let (cached_graph, cached_entities) =
            EntityGraph::build(root, &["a.py".into(), "b.py".into()], &registry);
        assert!(
            !cached_graph
                .get_dependencies("a.py::function::use_it")
                .iter()
                .any(|entity| entity.name == "helper"),
            "initial graph should not resolve helper"
        );

        write_file(
            root,
            "b.py",
            "def other():\n    return 1\n\n\ndef helper():\n    return 42\n",
        );

        let cached_clean_entities = cached_entities
            .iter()
            .filter(|entity| entity.file_path != "b.py")
            .cloned()
            .collect();
        let cached_stale_entities = cached_entities
            .into_iter()
            .filter(|entity| entity.file_path == "b.py")
            .collect();

        let (incremental_graph, _) = EntityGraph::build_incremental(
            root,
            &["b.py".into()],
            &["a.py".into(), "b.py".into()],
            cached_clean_entities,
            cached_graph.edges,
            cached_stale_entities,
            &registry,
        );
        let (fresh_graph, _) = EntityGraph::build(root, &["a.py".into(), "b.py".into()], &registry);

        let mut incremental_dependents = incremental_graph
            .get_dependents("b.py::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        incremental_dependents.sort_unstable();
        let mut fresh_dependents = fresh_graph
            .get_dependents("b.py::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        fresh_dependents.sort_unstable();
        assert_eq!(
            incremental_dependents, fresh_dependents,
            "incremental graph should match fresh resolution"
        );
        assert!(
            incremental_dependents
                .iter()
                .any(|entity_id| *entity_id == "a.py::function::use_it"),
            "clean caller should resolve to added helper. Dependents: {:?}",
            incremental_dependents
        );
    }

    #[test]
    fn test_incremental_added_stale_target_re_resolves_aliased_clean_reference() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "a.ts",
            "import { helper as h } from './b';\n\nexport function useIt() { return h(); }\n",
        );
        write_file(root, "b.ts", "export function other() { return 1; }\n");

        let (cached_graph, cached_entities) =
            EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert!(
            !cached_graph
                .get_dependencies("a.ts::function::useIt")
                .iter()
                .any(|entity| entity.name == "helper"),
            "initial graph should not resolve aliased helper"
        );

        write_file(
            root,
            "b.ts",
            "export function other() { return 1; }\n\nexport function helper() { return 42; }\n",
        );

        let cached_clean_entities = cached_entities
            .iter()
            .filter(|entity| entity.file_path != "b.ts")
            .cloned()
            .collect();
        let cached_stale_entities = cached_entities
            .into_iter()
            .filter(|entity| entity.file_path == "b.ts")
            .collect();

        let (incremental_graph, _) = EntityGraph::build_incremental(
            root,
            &["b.ts".into()],
            &["a.ts".into(), "b.ts".into()],
            cached_clean_entities,
            cached_graph.edges,
            cached_stale_entities,
            &registry,
        );
        let (fresh_graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);

        let mut incremental_dependents = incremental_graph
            .get_dependents("b.ts::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        incremental_dependents.sort_unstable();
        let mut fresh_dependents = fresh_graph
            .get_dependents("b.ts::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        fresh_dependents.sort_unstable();
        assert_eq!(
            incremental_dependents, fresh_dependents,
            "incremental graph should match fresh alias resolution"
        );
        assert!(
            incremental_dependents
                .iter()
                .any(|entity_id| *entity_id == "a.ts::function::useIt"),
            "aliased clean caller should resolve to added helper. Dependents: {:?}",
            incremental_dependents
        );
    }

    #[test]
    fn test_incremental_added_stale_target_re_resolves_python_alias() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "a.py",
            "from b import helper as h\n\ndef use_it():\n    return h()\n",
        );
        write_file(root, "b.py", "def other():\n    return 1\n");

        let (cached_graph, cached_entities) =
            EntityGraph::build(root, &["a.py".into(), "b.py".into()], &registry);
        assert!(
            !cached_graph
                .get_dependencies("a.py::function::use_it")
                .iter()
                .any(|entity| entity.name == "helper"),
            "initial graph should not resolve aliased helper"
        );

        write_file(
            root,
            "b.py",
            "def other():\n    return 1\n\n\ndef helper():\n    return 42\n",
        );

        let cached_clean_entities = cached_entities
            .iter()
            .filter(|entity| entity.file_path != "b.py")
            .cloned()
            .collect();
        let cached_stale_entities = cached_entities
            .into_iter()
            .filter(|entity| entity.file_path == "b.py")
            .collect();

        let (incremental_graph, _) = EntityGraph::build_incremental(
            root,
            &["b.py".into()],
            &["a.py".into(), "b.py".into()],
            cached_clean_entities,
            cached_graph.edges,
            cached_stale_entities,
            &registry,
        );
        let (fresh_graph, _) = EntityGraph::build(root, &["a.py".into(), "b.py".into()], &registry);

        let mut incremental_dependents = incremental_graph
            .get_dependents("b.py::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        incremental_dependents.sort_unstable();
        let mut fresh_dependents = fresh_graph
            .get_dependents("b.py::function::helper")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        fresh_dependents.sort_unstable();
        assert_eq!(
            incremental_dependents, fresh_dependents,
            "incremental graph should match fresh Python alias resolution"
        );
        assert!(
            incremental_dependents
                .iter()
                .any(|entity_id| *entity_id == "a.py::function::use_it"),
            "aliased clean caller should resolve to added helper. Dependents: {:?}",
            incremental_dependents
        );
    }

    #[test]
    fn test_incremental_added_stale_target_re_resolves_namespace_short_reference() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "a.ts",
            "import * as b from './b';\n\nexport function useIt() { return b.go(); }\n",
        );
        write_file(root, "b.ts", "export function other() { return 1; }\n");

        let (cached_graph, cached_entities) =
            EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert!(
            !cached_graph
                .get_dependencies("a.ts::function::useIt")
                .iter()
                .any(|entity| entity.name == "go"),
            "initial graph should not resolve namespace go"
        );

        write_file(
            root,
            "b.ts",
            "export function other() { return 1; }\n\nexport function go() { return 42; }\n",
        );

        let cached_clean_entities = cached_entities
            .iter()
            .filter(|entity| entity.file_path != "b.ts")
            .cloned()
            .collect();
        let cached_stale_entities = cached_entities
            .into_iter()
            .filter(|entity| entity.file_path == "b.ts")
            .collect();

        let (incremental_graph, _) = EntityGraph::build_incremental(
            root,
            &["b.ts".into()],
            &["a.ts".into(), "b.ts".into()],
            cached_clean_entities,
            cached_graph.edges,
            cached_stale_entities,
            &registry,
        );
        let (fresh_graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);

        let mut incremental_dependents = incremental_graph
            .get_dependents("b.ts::function::go")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        incremental_dependents.sort_unstable();
        let mut fresh_dependents = fresh_graph
            .get_dependents("b.ts::function::go")
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        fresh_dependents.sort_unstable();
        assert_eq!(
            incremental_dependents, fresh_dependents,
            "incremental graph should match fresh namespace resolution"
        );
        assert!(
            incremental_dependents
                .iter()
                .any(|entity_id| *entity_id == "a.ts::function::useIt"),
            "namespace clean caller should resolve to added go. Dependents: {:?}",
            incremental_dependents
        );
    }

    #[test]
    fn test_incremental_stale_default_re_export_re_resolves_clean_barrel() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "a.ts",
            "export default function targetA() { return 1; }\n",
        );
        write_file(
            root,
            "b.ts",
            "export default function targetB() { return 2; }\n",
        );
        write_file(root, "stale.ts", "export { default } from './a';\n");
        write_file(
            root,
            "barrel.ts",
            "export { default as publicTarget } from './stale';\n",
        );

        let all_files = vec![
            "a.ts".to_string(),
            "b.ts".to_string(),
            "stale.ts".to_string(),
            "barrel.ts".to_string(),
        ];
        let (cached_graph, cached_entities) = EntityGraph::build(root, &all_files, &registry);
        let initial_deps = cached_graph.get_dependencies("barrel.ts::export::publicTarget");
        assert!(
            initial_deps
                .iter()
                .any(|entity| entity.file_path == "a.ts" && entity.name == "targetA"),
            "initial barrel export should resolve through stale.ts to a.ts. Deps: {:?}",
            initial_deps
                .iter()
                .map(|entity| (&entity.file_path, &entity.name))
                .collect::<Vec<_>>()
        );

        write_file(root, "stale.ts", "export { default } from './b';\n");

        let cached_clean_entities = cached_entities
            .iter()
            .filter(|entity| entity.file_path != "stale.ts")
            .cloned()
            .collect();
        let cached_stale_entities = cached_entities
            .into_iter()
            .filter(|entity| entity.file_path == "stale.ts")
            .collect();

        let (incremental_graph, _) = EntityGraph::build_incremental(
            root,
            &["stale.ts".into()],
            &all_files,
            cached_clean_entities,
            cached_graph.edges,
            cached_stale_entities,
            &registry,
        );
        let (fresh_graph, _) = EntityGraph::build(root, &all_files, &registry);

        let mut incremental_deps = incremental_graph
            .get_dependencies("barrel.ts::export::publicTarget")
            .iter()
            .map(|entity| (entity.file_path.as_str(), entity.name.as_str()))
            .collect::<Vec<_>>();
        incremental_deps.sort_unstable();
        let mut fresh_deps = fresh_graph
            .get_dependencies("barrel.ts::export::publicTarget")
            .iter()
            .map(|entity| (entity.file_path.as_str(), entity.name.as_str()))
            .collect::<Vec<_>>();
        fresh_deps.sort_unstable();
        assert_eq!(
            incremental_deps, fresh_deps,
            "incremental graph should match fresh re-export retargeting"
        );
        assert!(
            incremental_deps
                .iter()
                .any(|(file_path, name)| *file_path == "b.ts" && *name == "targetB"),
            "clean barrel export should retarget to b.ts. Deps: {:?}",
            incremental_deps
        );
    }

    #[test]
    fn test_incremental_import_candidates_re_resolve_clean_barrel() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "a.ts",
            "export default function targetA() { return 1; }\n",
        );
        write_file(
            root,
            "b.ts",
            "export default function targetB() { return 2; }\n",
        );
        write_file(root, "stale.ts", "export { default } from './a';\n");
        write_file(
            root,
            "barrel.ts",
            "export { default as publicTarget } from './stale';\n",
        );

        let all_files = vec![
            "a.ts".to_string(),
            "b.ts".to_string(),
            "stale.ts".to_string(),
            "barrel.ts".to_string(),
        ];
        let (cached_graph, cached_entities) = EntityGraph::build(root, &all_files, &registry);

        write_file(root, "stale.ts", "export { default } from './b';\n");

        let cached_clean_entities = cached_entities
            .iter()
            .filter(|entity| entity.file_path != "stale.ts")
            .cloned()
            .collect();
        let cached_stale_entities = cached_entities
            .into_iter()
            .filter(|entity| entity.file_path == "stale.ts")
            .collect();
        let cached_importing_stale_files = vec!["barrel.ts".to_string()];

        let (incremental_graph, _, _) =
            EntityGraph::build_incremental_with_metadata_and_import_candidates(
                root,
                &["stale.ts".into()],
                &all_files,
                cached_clean_entities,
                cached_graph.edges,
                cached_stale_entities,
                Some(&cached_importing_stale_files),
                &registry,
            );
        let (fresh_graph, _) = EntityGraph::build(root, &all_files, &registry);

        let mut incremental_deps = incremental_graph
            .get_dependencies("barrel.ts::export::publicTarget")
            .iter()
            .map(|entity| (entity.file_path.as_str(), entity.name.as_str()))
            .collect::<Vec<_>>();
        incremental_deps.sort_unstable();
        let mut fresh_deps = fresh_graph
            .get_dependencies("barrel.ts::export::publicTarget")
            .iter()
            .map(|entity| (entity.file_path.as_str(), entity.name.as_str()))
            .collect::<Vec<_>>();
        fresh_deps.sort_unstable();

        assert_eq!(incremental_deps, fresh_deps);
        assert!(
            incremental_deps
                .iter()
                .any(|(file_path, name)| *file_path == "b.ts" && *name == "targetB"),
            "candidate-aware incremental rebuild should retarget the clean barrel export. Deps: {:?}",
            incremental_deps
        );
    }

    #[test]
    fn test_incremental_swift_overload_uses_clean_callee_signatures() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "Callee.swift",
            r#"func load(id: Int) -> String { return "id" }

func load(name: String) -> String { return "name" }
"#,
        );
        write_file(
            root,
            "Caller.swift",
            r#"func call() -> String { return load(id: 1) }
"#,
        );

        let file_paths = vec!["Caller.swift".to_string(), "Callee.swift".to_string()];
        let (initial_graph, initial_entities) = EntityGraph::build(root, &file_paths, &registry);

        write_file(
            root,
            "Caller.swift",
            r#"func call() -> String { return load(name: "x") }
"#,
        );

        let stale_file_cached_entities: Vec<SemanticEntity> = initial_entities
            .iter()
            .filter(|entity| entity.file_path == "Caller.swift")
            .cloned()
            .collect();
        let cached_entities: Vec<SemanticEntity> = initial_entities
            .into_iter()
            .filter(|entity| entity.file_path != "Caller.swift")
            .collect();

        let (graph, _) = EntityGraph::build_incremental(
            root,
            &["Caller.swift".to_string()],
            &file_paths,
            cached_entities,
            initial_graph.edges,
            stale_file_cached_entities,
            &registry,
        );

        let has_edge = |from: &str, to: &str| {
            graph.edges.iter().any(|edge| {
                edge.from_entity == from && edge.to_entity == to && edge.ref_type == RefType::Calls
            })
        };

        assert!(
            has_edge(
                "Caller.swift::function::call",
                "Callee.swift::function::load@L3"
            ),
            "incremental caller should resolve load(name:) using clean callee signatures"
        );
        assert!(
            !has_edge(
                "Caller.swift::function::call",
                "Callee.swift::function::load@L1"
            ),
            "incremental caller should not fall back to load(id:)"
        );
    }

    #[test]
    fn test_incremental_with_content() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return 1; }\n");
        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 1);

        // Add file with content provided directly (no disk read needed)
        graph.update_from_changes(
            &[FileChange {
                file_path: "b.ts".into(),
                status: FileStatus::Added,
                old_file_path: None,
                before_content: None,
                after_content: Some("export function bar() { return foo(); }\n".into()),
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 2);
        let bar_deps = graph.get_dependencies("b.ts::function::bar");
        assert!(bar_deps.iter().any(|d| d.name == "foo"));
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn test_go_method_parent_resolves_across_files_in_graph() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "models.go", "package demo\n\ntype Service struct{}\n");
        write_file(
            root,
            "methods.go",
            "package demo\n\nfunc (s *Service) Run() {}\n",
        );

        let (graph, entities) =
            EntityGraph::build(root, &["models.go".into(), "methods.go".into()], &registry);
        let service = graph
            .entities
            .get("models.go::type::Service")
            .expect("Service type should be in the graph");
        let run = entities
            .iter()
            .find(|e| e.name == "Run" && e.file_path == "methods.go")
            .expect("Run method should be extracted");

        assert_eq!(run.parent_id.as_deref(), Some(service.id.as_str()));
        assert!(graph.entities.contains_key("models.go::type::Service::Run"));
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn test_incremental_go_parent_repair_handles_clean_cached_method() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();
        let models = "package demo\n\ntype Service struct{}\n";
        let methods = "package demo\n\nfunc (s *Service) Run() {}\n";

        write_file(root, "models.go", models);
        write_file(root, "methods.go", methods);

        let cached_entities = registry.extract_entities("methods.go", methods);
        let cached_run = cached_entities
            .iter()
            .find(|e| e.name == "Run")
            .expect("cached Run method should be extracted");
        assert_eq!(
            cached_run.parent_id.as_deref(),
            Some("methods.go::type::Service")
        );

        let stale_file_cached_entities = registry.extract_entities("models.go", models);
        let (graph, entities, metadata) = EntityGraph::build_incremental_with_metadata(
            root,
            &["models.go".into()],
            &["models.go".into(), "methods.go".into()],
            cached_entities,
            vec![],
            stale_file_cached_entities,
            &registry,
        );
        let service = graph
            .entities
            .get("models.go::type::Service")
            .expect("Service type should be in the graph");
        let run = entities
            .iter()
            .find(|e| e.name == "Run" && e.file_path == "methods.go")
            .expect("Run method should be retained from clean cache");

        assert_eq!(run.parent_id.as_deref(), Some(service.id.as_str()));
        assert!(graph.entities.contains_key("models.go::type::Service::Run"));
        assert!(!graph
            .entities
            .contains_key("methods.go::type::Service::Run"));
        assert!(metadata.repaired_clean_entity_ids);
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn test_go_receiver_child_range_does_not_hide_parent_file_edges() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "models.go",
            "package demo\n\
             type Dependency struct{}\n\
             type Service struct { Dependency }\n",
        );
        write_file(
            root,
            "methods.go",
            "package demo\n\n\
             func (s *Service) Run() {}\n",
        );

        let (graph, _) =
            EntityGraph::build(root, &["models.go".into(), "methods.go".into()], &registry);

        assert!(
            graph.edges.iter().any(|edge| {
                edge.from_entity == "models.go::type::Service"
                    && edge.to_entity == "models.go::type::Dependency"
            }),
            "Service should keep its Dependency edge. Edges: {:?}",
            graph.edges
        );
    }

    #[cfg(feature = "lang-swift")]
    #[test]
    fn test_swift_extension_member_resolves_through_receiver_type() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "Example.swift",
            r#"
struct Widget {
    let name: String
}

extension Widget {
    func render() -> String { return name }
}

func draw(widget: Widget) {
    print(widget.render())
}
"#,
        );

        let (graph, _) = EntityGraph::build(root, &["Example.swift".into()], &registry);
        let draw = graph
            .entities
            .values()
            .find(|entity| entity.name == "draw")
            .expect("draw function should be in the graph");
        let extension = graph
            .entities
            .values()
            .find(|entity| entity.entity_type == "extension")
            .expect("extension should be in the graph");
        assert_eq!(extension.name, "Widget");
        let render = graph
            .entities
            .values()
            .find(|entity| entity.name == "render")
            .expect("extension method should be in the graph");
        assert_eq!(render.parent_id.as_deref(), Some(extension.id.as_str()));

        assert!(
            graph.edges.iter().any(|edge| {
                edge.from_entity == draw.id
                    && edge.to_entity == render.id
                    && edge.ref_type == RefType::Calls
            }),
            "draw should call Widget.render. Edges: {:?}",
            graph.edges
        );

        let render_dependents = graph.get_dependents(&render.id);
        assert!(
            render_dependents.iter().any(|entity| entity.id == draw.id),
            "render should be impacted by draw. Dependents: {:?}",
            render_dependents
                .iter()
                .map(|entity| &entity.name)
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-swift")]
    #[test]
    fn test_swift_extension_member_resolves_without_prebuilt_lookup() {
        let (_dir, registry) = create_test_repo();
        let source = r#"
struct Widget {
    let name: String
}

extension Widget {
    func render() -> String { return name }
}

func draw(widget: Widget) {
    print(widget.render())
}
"#;

        let all_entities = registry.extract_entities("Example.swift", source);
        let extension = all_entities
            .iter()
            .find(|entity| entity.entity_type == "extension")
            .expect("extension should be extracted");
        assert_eq!(extension.name, "Widget");
        let render = all_entities
            .iter()
            .find(|entity| entity.name == "render")
            .expect("extension method should be extracted");
        assert_eq!(render.parent_id.as_deref(), Some(extension.id.as_str()));
        let entity_map: HashMap<String, EntityInfo> = all_entities
            .iter()
            .map(|entity| {
                (
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
                )
            })
            .collect();

        let result = scope_resolve::resolve_with_scopes(
            Path::new("."),
            &["Example.swift".into()],
            &all_entities,
            &entity_map,
            Some(vec![(
                "Example.swift".into(),
                source.into(),
                registry
                    .extract_entities_with_tree("Example.swift", source)
                    .and_then(|(_, tree)| tree)
                    .expect("Swift parser should produce a tree"),
            )]),
        );
        let draw = all_entities
            .iter()
            .find(|entity| entity.name == "draw")
            .expect("draw function should be extracted");

        assert!(
            result.edges.iter().any(|(from, to, ref_type)| {
                from == &draw.id && to == &render.id && *ref_type == RefType::Calls
            }),
            "fallback scope resolver should resolve draw to Widget.render. Edges: {:?}",
            result.edges
        );
    }

    #[test]
    fn test_extract_references() {
        let content = "function processData(input) {\n  const result = validateInput(input);\n  return transform(result);\n}";
        let refs =
            extract_references_from_content(content, "processData", &[], StripStrategy::Generic);
        assert!(refs.contains(&"validateInput"));
        assert!(refs.contains(&"transform"));
        assert!(!refs.contains(&"processData")); // self excluded
    }

    #[test]
    fn test_container_does_not_inherit_child_call_edges() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "app.ts",
            "export function helper() { return 1; }\n\
             export class Service {\n\
               method() { return helper(); }\n\
             }\n\
             export class InlineService { method() { return helper(); } }\n",
        );

        let (graph, _) = EntityGraph::build(root, &["app.ts".into()], &registry);
        let helper_id = "app.ts::function::helper";

        for (class_id, method_id) in [
            ("app.ts::class::Service", "app.ts::class::Service::method"),
            (
                "app.ts::class::InlineService",
                "app.ts::class::InlineService::method",
            ),
        ] {
            assert!(
                graph.edges.iter().any(|edge| {
                    edge.from_entity == method_id
                        && edge.to_entity == helper_id
                        && edge.ref_type == RefType::Calls
                }),
                "{method_id} should call helper. Edges: {:?}",
                graph.edges
            );
            assert!(
                !graph
                    .edges
                    .iter()
                    .any(|edge| edge.from_entity == class_id && edge.to_entity == helper_id),
                "{class_id} should not call helper. Edges: {:?}",
                graph.edges
            );
        }
    }

    #[test]
    fn test_incremental_container_does_not_inherit_child_call_edges() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "app.ts",
            "export function helper() { return 1; }\n\
             export class Service {\n\
               method() { return helper(); }\n\
             }\n",
        );
        let (initial_graph, initial_entities) =
            EntityGraph::build(root, &["app.ts".into()], &registry);

        write_file(
            root,
            "app.ts",
            "export function helper() { return 2; }\n\
             export function extra() { return 3; }\n\
             export class Service {\n\
               method() { return helper(); }\n\
             }\n",
        );

        let (graph, _) = EntityGraph::build_incremental(
            root,
            &["app.ts".into()],
            &["app.ts".into()],
            vec![],
            initial_graph.edges,
            initial_entities,
            &registry,
        );

        let class_id = "app.ts::class::Service";
        let method_id = "app.ts::class::Service::method";
        let helper_id = "app.ts::function::helper";
        assert!(
            graph.edges.iter().any(|edge| {
                edge.from_entity == method_id
                    && edge.to_entity == helper_id
                    && edge.ref_type == RefType::Calls
            }),
            "{method_id} should call helper. Edges: {:?}",
            graph.edges
        );
        assert!(
            !graph
                .edges
                .iter()
                .any(|edge| edge.from_entity == class_id && edge.to_entity == helper_id),
            "{class_id} should not call helper. Edges: {:?}",
            graph.edges
        );
    }

    #[test]
    fn test_same_line_container_and_child_refs_use_byte_spans() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "app.ts",
            "export function helper() { return 1; }\n\
             export function other() { return 2; }\n\
             export class Service { static { helper(); } method() { return other(); } }\n",
        );

        let (graph, _) = EntityGraph::build(root, &["app.ts".into()], &registry);
        let class_id = "app.ts::class::Service";
        let method_id = "app.ts::class::Service::method";
        let helper_id = "app.ts::function::helper";
        let other_id = "app.ts::function::other";

        assert!(
            graph.edges.iter().any(|edge| {
                edge.from_entity == class_id
                    && edge.to_entity == helper_id
                    && edge.ref_type == RefType::Calls
            }),
            "{class_id} should own the static-block helper call. Edges: {:?}",
            graph.edges
        );
        assert!(
            graph.edges.iter().any(|edge| {
                edge.from_entity == method_id
                    && edge.to_entity == other_id
                    && edge.ref_type == RefType::Calls
            }),
            "{method_id} should own the method-body other call. Edges: {:?}",
            graph.edges
        );
        assert!(
            !graph
                .edges
                .iter()
                .any(|edge| edge.from_entity == method_id && edge.to_entity == helper_id),
            "{method_id} should not inherit the static-block helper call. Edges: {:?}",
            graph.edges
        );
        assert!(
            !graph
                .edges
                .iter()
                .any(|edge| edge.from_entity == class_id && edge.to_entity == other_id),
            "{class_id} should not inherit the method-body other call. Edges: {:?}",
            graph.edges
        );
    }

    #[test]
    fn test_extract_references_skips_keywords() {
        let content = "function foo() { if (true) { return false; } }";
        let refs = extract_references_from_content(content, "foo", &[], StripStrategy::Generic);
        assert!(!refs.contains(&"if"));
        assert!(!refs.contains(&"true"));
        assert!(!refs.contains(&"return"));
        assert!(!refs.contains(&"false"));
    }

    #[test]
    fn test_infer_ref_type_call() {
        assert_eq!(
            infer_ref_type("validateInput(data)", "validateInput"),
            RefType::Calls,
        );
    }

    #[test]
    fn test_infer_ref_type_type() {
        assert_eq!(
            infer_ref_type("let x: MyType = something", "MyType"),
            RefType::TypeRef,
        );
    }

    #[test]
    fn test_infer_ref_type_multibyte_utf8() {
        // Ensure no panic when content contains multi-byte UTF-8 characters
        assert_eq!(infer_ref_type("let café = foo(x)", "foo"), RefType::Calls,);
        assert_eq!(
            infer_ref_type(
                "class HandicapfrPublicationFieldsEnum:\n    É = 1\n    bar()",
                "bar"
            ),
            RefType::Calls,
        );
        // No match should not panic either
        assert_eq!(
            infer_ref_type("// 日本語コメント\nlet x = 1", "missing"),
            RefType::TypeRef,
        );
    }

    #[test]
    fn test_dot_chain_self_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "service.py",
            "\
class MyService:
    def process(self):
        return self.validate()

    def validate(self):
        return True
",
        );

        let (graph, _) = EntityGraph::build(root, &["service.py".into()], &registry);

        // process should have an edge to validate via self.validate()
        let process_id = graph
            .entities
            .keys()
            .find(|id| id.contains("process"))
            .expect("process entity should exist");
        let deps = graph.get_dependencies(process_id);
        assert!(
            deps.iter().any(|d| d.name == "validate"),
            "process should depend on validate via self.validate(). Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_dot_chain_this_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "service.ts",
            "\
class UserService {
    process() {
        return this.validate();
    }
    validate() {
        return true;
    }
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["service.ts".into()], &registry);

        let process_id = graph
            .entities
            .keys()
            .find(|id| id.contains("process"))
            .expect("process entity should exist");
        let deps = graph.get_dependencies(process_id);
        assert!(
            deps.iter().any(|d| d.name == "validate"),
            "process should depend on validate via this.validate(). Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-swift")]
    #[test]
    fn test_swift_bare_instance_property_receiver_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "Example.swift",
            "\
class Connection {
    func execute(query: String) {}
    func commit() {}
}

class Transaction {
    let conn: Connection
    init(conn: Connection) { self.conn = conn }

    func execute(query: String) {
        conn.execute(query: query)
    }

    func commit() {
        conn.commit()
    }
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["Example.swift".into()], &registry);

        let transaction_execute_id = graph
            .entities
            .iter()
            .find(|(id, info)| info.name == "execute" && id.contains("Transaction"))
            .map(|(id, _)| id.clone())
            .expect("Transaction.execute entity should exist");
        let execute_deps = graph.get_dependencies(&transaction_execute_id);
        assert!(
            execute_deps.iter().any(|d| {
                d.name == "execute"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("Connection"))
            }),
            "Transaction.execute should depend on Connection.execute. Deps: {:?}",
            execute_deps
                .iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );

        let transaction_commit_id = graph
            .entities
            .iter()
            .find(|(id, info)| info.name == "commit" && id.contains("Transaction"))
            .map(|(id, _)| id.clone())
            .expect("Transaction.commit entity should exist");
        let commit_deps = graph.get_dependencies(&transaction_commit_id);
        assert!(
            commit_deps.iter().any(|d| {
                d.name == "commit"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("Connection"))
            }),
            "Transaction.commit should depend on Connection.commit. Deps: {:?}",
            commit_deps
                .iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-swift")]
    #[test]
    fn test_swift_static_method_does_not_resolve_instance_property_receiver() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "Example.swift",
            "\
class Connection {
    func execute(query: String) {}
}

class Transaction {
    let conn: Connection

    static func run() {
        conn.execute(query: \"SELECT 1\")
    }
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["Example.swift".into()], &registry);

        let run_id = graph
            .entities
            .keys()
            .find(|id| id.contains("Transaction") && id.contains("run"))
            .expect("Transaction.run entity should exist");
        let deps = graph.get_dependencies(run_id);
        assert!(
            !deps.iter().any(|d| {
                d.name == "execute"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("Connection"))
            }),
            "static Transaction.run should not depend on Connection.execute via instance property. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-swift")]
    #[test]
    fn test_swift_multi_binding_property_receivers_resolve() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "Example.swift",
            "\
class PrimaryConnection {
    func execute(query: String) {}
}

class BackupConnection {
    func flush() {}
}

class Transaction {
    let conn: PrimaryConnection, backup: BackupConnection

    func run(query: String) {
        conn.execute(query: query)
        backup.flush()
    }
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["Example.swift".into()], &registry);

        let run_id = graph
            .entities
            .keys()
            .find(|id| id.contains("Transaction") && id.contains("run"))
            .expect("Transaction.run entity should exist");
        let deps = graph.get_dependencies(run_id);
        assert!(
            deps.iter().any(|d| {
                d.name == "execute"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("PrimaryConnection"))
            }),
            "conn.execute should resolve to PrimaryConnection.execute. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
        assert!(
            deps.iter().any(|d| {
                d.name == "flush"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("BackupConnection"))
            }),
            "backup.flush should resolve to BackupConnection.flush. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-swift")]
    #[test]
    fn test_swift_nested_local_binding_shadows_instance_property_receiver() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "Example.swift",
            "\
class Connection {
    func execute(query: String) {}
}

class MockConnection {
    func execute(query: String) {}
}

class Transaction {
    let conn: Connection
    init(conn: Connection) { self.conn = conn }

    func execute(query: String, useMock: Bool) {
        if useMock {
            let conn = MockConnection()
            conn.execute(query: query)
        }
    }
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["Example.swift".into()], &registry);

        let transaction_execute_id = graph
            .entities
            .iter()
            .find(|(id, info)| info.name == "execute" && id.contains("Transaction"))
            .map(|(id, _)| id.clone())
            .expect("Transaction.execute entity should exist");
        let deps = graph.get_dependencies(&transaction_execute_id);
        assert!(
            deps.iter().any(|d| {
                d.name == "execute"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("MockConnection"))
            }),
            "nested local conn should resolve to MockConnection.execute. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps.iter().any(|d| {
                d.name == "execute"
                    && d.parent_id.as_deref().map_or(false, |parent| {
                        parent.contains("Connection") && !parent.contains("MockConnection")
                    })
            }),
            "nested local conn should shadow Transaction.conn. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_typescript_bare_identifier_does_not_resolve_instance_property() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "service.ts",
            "\
class Connection {
    execute() {
        return true;
    }
}

class Transaction {
    conn: Connection;
    constructor(conn: Connection) {
        this.conn = conn;
    }

    run() {
        return conn.execute();
    }
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["service.ts".into()], &registry);

        let run_id = graph
            .entities
            .keys()
            .find(|id| id.contains("Transaction") && id.contains("run"))
            .expect("Transaction.run entity should exist");
        let deps = graph.get_dependencies(run_id);
        assert!(
            !deps.iter().any(|d| {
                d.name == "execute"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("Connection"))
            }),
            "bare conn.execute() should not resolve through a TypeScript instance property. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_dot_chain_class_static() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "utils.ts",
            "\
class MathUtils {
    static compute() { return 1; }
}
function caller() { return MathUtils.compute(); }
",
        );

        let (graph, _) = EntityGraph::build(root, &["utils.ts".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter().any(|d| d.name == "compute"),
            "caller should depend on compute via MathUtils.compute(). Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_protocols_are_member_containers() {
        assert!(is_nominal_member_container("protocol"));
        assert!(is_scope_member_container("protocol"));
    }

    #[test]
    fn test_js_ts_import_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "helper.ts",
            "\
export function helper() { return 1; }
",
        );
        write_file(
            root,
            "main.ts",
            "\
import { helper } from './helper';
export function main() { return helper(); }
",
        );

        let (graph, _) =
            EntityGraph::build(root, &["helper.ts".into(), "main.ts".into()], &registry);

        let main_id = graph
            .entities
            .keys()
            .find(|id| id.contains("main"))
            .expect("main entity should exist");
        let deps = graph.get_dependencies(main_id);
        assert!(
            deps.iter().any(|d| d.name == "helper"),
            "main should depend on helper via JS import. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_direct_dependencies_match_full_graph_for_js_ts_import_forms() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function namedThing() { return 1; }
export default function defaultThing() { return 2; }
",
        );
        write_file(
            root,
            "consumer.ts",
            "\
import defaultThing, { namedThing } from './lib';
import * as lib from './lib';

export function useEverything() {
    return defaultThing() + namedThing() + lib.namedThing();
}
",
        );
        let files = vec!["lib.ts".to_string(), "consumer.ts".to_string()];
        assert_direct_dependencies_match_full(
            root,
            &files,
            &registry,
            "consumer.ts::function::useEverything",
        );
    }

    #[test]
    fn test_js_ts_relative_import_resolution_uses_full_path() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/a/util.ts",
            "\
export function helper() { return 1; }
",
        );
        write_file(
            root,
            "src/b/util.ts",
            "\
export function helper() { return 2; }
",
        );
        write_file(
            root,
            "src/main.ts",
            "\
import { helper } from './b/util';
export function caller() { return helper(); }
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &[
                "src/a/util.ts".into(),
                "src/b/util.ts".into(),
                "src/main.ts".into(),
            ],
            &registry,
        );

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter()
                .any(|d| d.name == "helper" && d.file_path == "src/b/util.ts"),
            "caller should resolve helper to src/b/util.ts. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "helper" && d.file_path == "src/a/util.ts"),
            "caller should not resolve helper to src/a/util.ts. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_relative_import_with_extension_prefers_exact_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/util.js",
            "\
export function helper() { return 1; }
",
        );
        write_file(
            root,
            "src/util.ts",
            "\
export function helper() { return 2; }
",
        );
        write_file(
            root,
            "src/main.ts",
            "\
import { helper } from './util.ts';
export function caller() { return helper(); }
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &[
                "src/util.js".into(),
                "src/util.ts".into(),
                "src/main.ts".into(),
            ],
            &registry,
        );

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter()
                .any(|d| d.name == "helper" && d.file_path == "src/util.ts"),
            "caller should resolve helper to explicit src/util.ts. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "helper" && d.file_path == "src/util.js"),
            "caller should not resolve explicit ./util.ts to src/util.js. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_default_import_resolves_static_member() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "base.ts",
            "\
export default class Widget {
  static make(): string { return 'ok'; }
}
",
        );
        write_file(
            root,
            "consumer.ts",
            "\
import RenamedWidget from './base';
export function useWidget(): string { return RenamedWidget.make(); }
",
        );

        let (graph, _) =
            EntityGraph::build(root, &["base.ts".into(), "consumer.ts".into()], &registry);

        let use_widget_id = graph
            .entities
            .keys()
            .find(|id| id.contains("useWidget"))
            .expect("useWidget entity should exist");
        let deps = graph.get_dependencies(use_widget_id);
        assert!(
            deps.iter()
                .any(|d| d.name == "make" && d.file_path == "base.ts"),
            "default import alias should resolve the static member. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_re_export_alias_resolves_through_barrel() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function core(): string { return 'core'; }
",
        );
        write_file(
            root,
            "barrel.ts",
            "\
export { core as publicCore } from './lib';
",
        );
        write_file(
            root,
            "consumer.ts",
            "\
import { publicCore } from './barrel';
export function usePublicCore(): string { return publicCore(); }
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &["lib.ts".into(), "barrel.ts".into(), "consumer.ts".into()],
            &registry,
        );

        let public_core = graph
            .entities
            .values()
            .find(|entity| {
                entity.name == "publicCore"
                    && entity.file_path == "barrel.ts"
                    && entity.entity_type == "export"
            })
            .expect("barrel export alias entity should exist");
        let alias_deps = graph.get_dependencies(&public_core.id);
        assert!(
            alias_deps
                .iter()
                .any(|d| d.name == "core" && d.file_path == "lib.ts"),
            "barrel export alias should depend on lib.ts core. Deps: {:?}",
            alias_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );

        let use_public_core_id = graph
            .entities
            .keys()
            .find(|id| id.contains("usePublicCore"))
            .expect("usePublicCore entity should exist");
        let consumer_deps = graph.get_dependencies(use_public_core_id);
        assert!(
            consumer_deps
                .iter()
                .any(|d| d.name == "publicCore" && d.file_path == "barrel.ts"),
            "consumer should resolve publicCore through the barrel export. Deps: {:?}",
            consumer_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_re_export_alias_overrides_colliding_default_import_name() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "default_source.ts",
            "\
export default function Public(): string { return 'default'; }
",
        );
        write_file(
            root,
            "named_source.ts",
            "\
export function named(): string { return 'named'; }
",
        );
        write_file(
            root,
            "barrel.ts",
            "\
import Public from './default_source';
export { named as Public } from './named_source';
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &[
                "default_source.ts".into(),
                "named_source.ts".into(),
                "barrel.ts".into(),
            ],
            &registry,
        );

        let public_export = graph
            .entities
            .values()
            .find(|entity| {
                entity.name == "Public"
                    && entity.file_path == "barrel.ts"
                    && entity.entity_type == "export"
            })
            .expect("barrel export alias entity should exist");
        let alias_deps = graph.get_dependencies(&public_export.id);
        assert!(
            alias_deps
                .iter()
                .any(|d| d.name == "named" && d.file_path == "named_source.ts"),
            "barrel export alias should prefer the re-export target over the colliding default import. Deps: {:?}",
            alias_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !alias_deps
                .iter()
                .any(|d| d.name == "Public" && d.file_path == "default_source.ts"),
            "colliding default import should not win over the re-export alias. Deps: {:?}",
            alias_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_direct_dependencies_match_full_graph_for_js_ts_re_exports() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function core(): string { return 'core'; }
",
        );
        write_file(
            root,
            "barrel.ts",
            "\
export { core as publicCore } from './lib';
",
        );
        write_file(
            root,
            "consumer.ts",
            "\
import { publicCore } from './barrel';
export function usePublicCore(): string { return publicCore(); }
",
        );
        let files = vec![
            "lib.ts".to_string(),
            "barrel.ts".to_string(),
            "consumer.ts".to_string(),
        ];

        assert_direct_dependencies_match_full(
            root,
            &files,
            &registry,
            "barrel.ts::export::publicCore",
        );
        assert_direct_dependencies_match_full(
            root,
            &files,
            &registry,
            "consumer.ts::function::usePublicCore",
        );
    }

    #[test]
    fn test_js_ts_namespace_import_resolves_re_export_alias() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function core(): string { return 'core'; }
",
        );
        write_file(
            root,
            "barrel.ts",
            "\
export { core as publicCore } from './lib';
",
        );
        write_file(
            root,
            "consumer.ts",
            "\
import * as barrel from './barrel';
export function usePublicCore(): string { return barrel.publicCore(); }
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &["lib.ts".into(), "barrel.ts".into(), "consumer.ts".into()],
            &registry,
        );

        let use_public_core_id = graph
            .entities
            .keys()
            .find(|id| id.contains("usePublicCore"))
            .expect("usePublicCore entity should exist");
        let consumer_deps = graph.get_dependencies(use_public_core_id);
        assert!(
            consumer_deps
                .iter()
                .any(|d| d.name == "publicCore" && d.file_path == "barrel.ts"),
            "namespace import should resolve the exported barrel alias. Deps: {:?}",
            consumer_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_object_literal_receiver_resolves_owned_member() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "service.ts",
            "\
export const other = {
    open() { return 'other'; }
};
export const svc = {
    open() { return 'svc'; }
};
export function run(): string {
    return svc.open();
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["service.ts".into()], &registry);

        let run_id = graph
            .entities
            .keys()
            .find(|id| id.contains("run"))
            .expect("run entity should exist");
        let deps = graph.get_dependencies(run_id);
        assert!(
            deps.iter()
                .any(|d| d.name == "open"
                    && d.parent_id.as_deref().is_some_and(|id| id.contains("svc"))),
            "svc.open() should resolve to the object literal member owned by svc. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps.iter().any(|d| d.name == "open"
                && d.parent_id
                    .as_deref()
                    .is_some_and(|id| id.contains("other"))),
            "svc.open() should not resolve to another object literal member. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_python_relative_import_resolution_uses_full_path() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/a/util.py",
            "\
def helper():
    return 1
",
        );
        write_file(
            root,
            "src/b/util.py",
            "\
def helper():
    return 2
",
        );
        write_file(
            root,
            "src/main.py",
            "\
from .b.util import helper

def caller():
    return helper()
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &[
                "src/a/util.py".into(),
                "src/b/util.py".into(),
                "src/main.py".into(),
            ],
            &registry,
        );

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter()
                .any(|d| d.name == "helper" && d.file_path == "src/b/util.py"),
            "caller should resolve helper to src/b/util.py. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "helper" && d.file_path == "src/a/util.py"),
            "caller should not resolve helper to src/a/util.py. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_python_absolute_import_resolution_uses_full_path() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/a/util.py",
            "\
def helper():
    return 1
",
        );
        write_file(
            root,
            "src/b/util.py",
            "\
def helper():
    return 2
",
        );
        write_file(
            root,
            "src/main.py",
            "\
from src.b.util import helper

def caller():
    return helper()
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &[
                "src/a/util.py".into(),
                "src/b/util.py".into(),
                "src/main.py".into(),
            ],
            &registry,
        );

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter()
                .any(|d| d.name == "helper" && d.file_path == "src/b/util.py"),
            "caller should resolve helper to src/b/util.py. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "helper" && d.file_path == "src/a/util.py"),
            "caller should not resolve helper to src/a/util.py. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_named_import_does_not_resolve_unrelated_method_receiver() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function foo() { return 1; }
",
        );
        write_file(
            root,
            "main.ts",
            "\
import { foo } from './lib';
export function caller(other) { return other.foo(); }
export function actual() { return foo(); }
",
        );

        let (graph, _) = EntityGraph::build(root, &["lib.ts".into(), "main.ts".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let caller_deps = graph.get_dependencies(caller_id);
        assert!(
            !caller_deps
                .iter()
                .any(|d| d.name == "foo" && d.file_path == "lib.ts"),
            "other.foo() should not resolve through a bare named import. Deps: {:?}",
            caller_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );

        let actual_id = graph
            .entities
            .keys()
            .find(|id| id.contains("actual"))
            .expect("actual entity should exist");
        let actual_deps = graph.get_dependencies(actual_id);
        assert!(
            actual_deps
                .iter()
                .any(|d| d.name == "foo" && d.file_path == "lib.ts"),
            "foo() should still resolve through the named import. Deps: {:?}",
            actual_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_unresolved_method_does_not_block_unrelated_fallback_import() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export const answer = 1;
export function foo() { return 1; }
",
        );
        write_file(
            root,
            "main.ts",
            "\
import { answer, foo } from './lib';
export function caller(other) {
    other.foo();
    return answer;
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["lib.ts".into(), "main.ts".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter()
                .any(|d| d.name == "answer" && d.file_path == "lib.ts"),
            "unresolved other.foo() should not block bare answer import fallback. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "foo" && d.file_path == "lib.ts"),
            "other.foo() should not resolve through the named import. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_namespace_import_respects_receiver_alias() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function foo() { return 1; }
",
        );
        write_file(
            root,
            "other.ts",
            "\
export function foo() { return 2; }
",
        );
        write_file(
            root,
            "main.ts",
            "\
import * as lib from './lib';
export function caller(other) { return other.foo(); }
export function actual() { return lib.foo(); }
",
        );

        let (graph, _) = EntityGraph::build(
            root,
            &["lib.ts".into(), "other.ts".into(), "main.ts".into()],
            &registry,
        );

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let caller_deps = graph.get_dependencies(caller_id);
        assert!(
            !caller_deps.iter().any(|d| d.name == "foo"),
            "other.foo() should not resolve via namespace import lib. Deps: {:?}",
            caller_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );

        let actual_id = graph
            .entities
            .keys()
            .find(|id| id.contains("actual"))
            .expect("actual entity should exist");
        let actual_deps = graph.get_dependencies(actual_id);
        assert!(
            actual_deps
                .iter()
                .any(|d| d.name == "foo" && d.file_path == "lib.ts"),
            "lib.foo() should resolve to lib.ts. Deps: {:?}",
            actual_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !actual_deps
                .iter()
                .any(|d| d.name == "foo" && d.file_path == "other.ts"),
            "lib.foo() should not resolve to other.ts. Deps: {:?}",
            actual_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_namespace_import_skips_unexported_top_level_entities() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
function hidden() { return 1; }
export function visible() { return 2; }
",
        );
        write_file(
            root,
            "main.ts",
            "\
import * as lib from './lib';
export function callVisible() { return lib.visible(); }
export function callHidden() { return lib.hidden(); }
",
        );

        let (graph, _) = EntityGraph::build(root, &["lib.ts".into(), "main.ts".into()], &registry);

        let visible_id = graph
            .entities
            .keys()
            .find(|id| id.contains("callVisible"))
            .expect("callVisible entity should exist");
        let visible_deps = graph.get_dependencies(visible_id);
        assert!(
            visible_deps
                .iter()
                .any(|d| d.name == "visible" && d.file_path == "lib.ts"),
            "lib.visible() should resolve to the exported function. Deps: {:?}",
            visible_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );

        let hidden_id = graph
            .entities
            .keys()
            .find(|id| id.contains("callHidden"))
            .expect("callHidden entity should exist");
        let hidden_deps = graph.get_dependencies(hidden_id);
        assert!(
            !hidden_deps
                .iter()
                .any(|d| d.name == "hidden" && d.file_path == "lib.ts"),
            "lib.hidden() should not resolve to a module-private function. Deps: {:?}",
            hidden_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_local_binding_shadows_imported_class_receiver() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export class Service {
    static run() { return 1; }
}
",
        );
        write_file(
            root,
            "main.ts",
            "\
import { Service } from './lib';
export function caller(Service) { return Service.run(); }
",
        );

        let (graph, _) = EntityGraph::build(root, &["lib.ts".into(), "main.ts".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "run" && d.file_path == "lib.ts"),
            "local parameter Service should shadow imported class receiver. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "Service" && d.file_path == "lib.ts"),
            "local parameter Service should shadow imported class name. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_local_binding_shadows_namespace_receiver() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function foo() { return 1; }
",
        );
        write_file(
            root,
            "main.ts",
            "\
import * as lib from './lib';
export function caller(lib) { return lib.foo(); }
",
        );

        let (graph, _) = EntityGraph::build(root, &["lib.ts".into(), "main.ts".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "foo" && d.file_path == "lib.ts"),
            "local parameter lib should shadow namespace import receiver. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_local_binding_shadows_named_import_call() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export function foo() { return 1; }
",
        );
        write_file(
            root,
            "main.ts",
            "\
import { foo } from './lib';
export function caller(foo) { return foo(); }
",
        );

        let (graph, _) = EntityGraph::build(root, &["lib.ts".into(), "main.ts".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            !deps
                .iter()
                .any(|d| d.name == "foo" && d.file_path == "lib.ts"),
            "local parameter foo should shadow named import. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_nested_local_binding_does_not_hide_parent_reference() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "lib.ts",
            "\
export const answer = 42;
",
        );
        write_file(
            root,
            "main.ts",
            "\
import { answer } from './lib';
export function outer() {
    const value = answer;
    function inner() {
        const answer = 0;
        return answer;
    }
    return value;
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["lib.ts".into(), "main.ts".into()], &registry);

        let outer_id = graph
            .entities
            .iter()
            .find(|(_, entity)| entity.name == "outer")
            .map(|(id, _)| id)
            .expect("outer entity should exist");
        let outer_deps = graph.get_dependencies(outer_id);
        assert!(
            outer_deps
                .iter()
                .any(|d| d.name == "answer" && d.file_path == "lib.ts"),
            "parent bare reference to imported answer should remain resolved. Deps: {:?}",
            outer_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );

        let inner_id = graph
            .entities
            .iter()
            .find(|(_, entity)| entity.name == "inner")
            .map(|(id, _)| id)
            .expect("inner entity should exist");
        let inner_deps = graph.get_dependencies(inner_id);
        assert!(
            !inner_deps
                .iter()
                .any(|d| d.name == "answer" && d.file_path == "lib.ts"),
            "nested local binding answer should not resolve to imported answer. Deps: {:?}",
            inner_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_python_local_binding_shadows_same_file_function() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "b.py",
            "\
def total(items):
    return sum(items)

def report():
    total = 0
    for i in range(10):
        total = total + i
    return total
",
        );

        let (graph, _) = EntityGraph::build(root, &["b.py".into()], &registry);

        let report_id = graph
            .entities
            .iter()
            .find(|(_, entity)| entity.name == "report")
            .map(|(id, _)| id)
            .expect("report entity should exist");
        let deps = graph.get_dependencies(report_id);
        assert!(
            !deps.iter().any(|d| d.name == "total"),
            "local variable total should not resolve to same-file function total. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_constructor_return_type_tie_break_uses_stable_source_order() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "a_primary.py",
            "\
class Primary:
    def get(self):
        return True

def make_conn():
    return Primary()
",
        );
        write_file(
            root,
            "holder.py",
            "\
class Holder:
    def __init__(self, conn):
        self.conn = conn

    def use(self):
        return self.conn.get()

def wire():
    Holder(make_conn())
",
        );
        write_file(
            root,
            "z_backup.py",
            "\
class Backup:
    def get(self):
        return False

def make_conn():
    return Backup()
",
        );

        let files = vec![
            "a_primary.py".to_string(),
            "holder.py".to_string(),
            "z_backup.py".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &files, &registry);
        let graph_payload = graph_json_payload(&graph);

        let use_id = graph
            .entities
            .iter()
            .find(|(_, entity)| entity.name == "use")
            .map(|(id, _)| id)
            .expect("Holder.use entity should exist");
        let deps = graph.get_dependencies(use_id);

        assert!(
            deps.iter().any(|d| {
                d.name == "get"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("Primary"))
            }),
            "Holder.use should resolve conn.get to Primary.get. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );
        assert!(
            !deps.iter().any(|d| {
                d.name == "get"
                    && d.parent_id
                        .as_deref()
                        .map_or(false, |parent| parent.contains("Backup"))
            }),
            "Holder.use should not resolve conn.get to Backup.get. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.parent_id))
                .collect::<Vec<_>>()
        );

        for _ in 0..16 {
            let (repeat_graph, _) = EntityGraph::build(root, &files, &registry);
            assert_eq!(graph_json_payload(&repeat_graph), graph_payload);
        }
    }

    #[test]
    fn test_rust_impl_container_does_not_inherit_child_build_call() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "graph.rs",
            "\
pub struct EntityGraph;

impl EntityGraph {
    pub fn build(a: i32, b: i32, c: i32) -> i32 {
        a + b + c
    }
}
",
        );
        write_file(
            root,
            "server.rs",
            "\
use crate::graph::EntityGraph;

struct SemServer;

impl SemServer {
    fn find_supported_files() {
        let mut builder = ignore::WalkBuilder::new(\".\");
        let walker = builder.build();
    }

    fn get_or_build_graph() {
        let _ = EntityGraph::build(1, 2, 3);
    }
}

impl SemServer {
    fn metadata(&self) -> i32 {
        1
    }
}
",
        );

        let (graph, _) =
            EntityGraph::build(root, &["graph.rs".into(), "server.rs".into()], &registry);

        let sem_server_impls: Vec<_> = graph
            .entities
            .iter()
            .filter(|(_, entity)| entity.entity_type == "impl" && entity.name == "SemServer")
            .collect();
        assert!(
            sem_server_impls.len() >= 2,
            "test fixture should produce duplicate SemServer impl entities"
        );
        for (impl_id, _) in sem_server_impls {
            let impl_deps = graph.get_dependencies(impl_id);
            assert!(
                !impl_deps
                    .iter()
                    .any(|d| d.name == "build" && d.file_path == "graph.rs"),
                "impl container should not inherit child build calls. Deps: {:?}",
                impl_deps
                    .iter()
                    .map(|d| (&d.name, &d.file_path))
                    .collect::<Vec<_>>()
            );
        }

        let method_id = graph
            .entities
            .iter()
            .find(|(_, entity)| entity.name == "get_or_build_graph")
            .map(|(id, _)| id)
            .expect("get_or_build_graph entity should exist");
        let method_deps = graph.get_dependencies(method_id);
        assert!(
            method_deps
                .iter()
                .any(|d| d.name == "build" && d.file_path == "graph.rs"),
            "direct EntityGraph::build call should remain resolved. Deps: {:?}",
            method_deps
                .iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_rust_lowercase_scoped_path_does_not_fallback_to_local_function() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "main.rs",
            "\
fn baz() {}

fn caller() {
    foo::bar::baz();
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["main.rs".into()], &registry);

        let caller_id = graph
            .entities
            .iter()
            .find(|(_, entity)| entity.name == "caller")
            .map(|(id, _)| id)
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            !deps.iter().any(|d| d.name == "baz"),
            "lowercase scoped path should not fall back to local baz function. Deps: {:?}",
            deps.iter()
                .map(|d| (&d.name, &d.file_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_dot_chain_no_false_edges() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // Two classes with same method name "process".
        // self.process() in ClassA should NOT create edge to ClassB::process.
        write_file(
            root,
            "a.py",
            "\
class ClassA:
    def run(self):
        return self.process()

    def process(self):
        return 1
",
        );
        write_file(
            root,
            "b.py",
            "\
class ClassB:
    def process(self):
        return 2
",
        );

        let (graph, _) = EntityGraph::build(root, &["a.py".into(), "b.py".into()], &registry);

        let run_id = graph
            .entities
            .keys()
            .find(|id| id.contains("run"))
            .expect("run entity should exist");
        let deps = graph.get_dependencies(run_id);
        // Should have edge to ClassA::process, NOT ClassB::process
        for dep in &deps {
            if dep.name == "process" {
                assert!(
                    dep.file_path == "a.py",
                    "run's process dep should be in a.py, not {}",
                    dep.file_path
                );
            }
        }
    }

    #[test]
    fn test_dot_chain_fallback() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // someVar.unknownMethod() - "someVar" is not a class,
        // so the chain is unresolved and words fall through to bag-of-words.
        // "helper" should still resolve via bag-of-words.
        write_file(
            root,
            "app.ts",
            "\
export function helper() { return 1; }
export function caller() {
    const val = helper();
    return val;
}
",
        );

        let (graph, _) = EntityGraph::build(root, &["app.ts".into()], &registry);

        let caller_id = graph
            .entities
            .keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter().any(|d| d.name == "helper"),
            "caller should still resolve helper via bag-of-words. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-clojure")]
    #[test]
    fn test_clojure_namespace_alias_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // util.cljs defines a function
        write_file(
            root,
            "src/myapp/util.cljs",
            r#"(ns myapp.util)

(defn vectorize-if-not-sequential [x]
  (if (sequential? x) x [x]))
"#,
        );

        // elements.cljs requires util with :as u and calls u/vectorize-if-not-sequential
        write_file(
            root,
            "src/myapp/elements.cljs",
            r#"(ns myapp.elements
  (:require [myapp.util :as u]))

(defn render-items [items]
  (u/vectorize-if-not-sequential items))
"#,
        );

        let file_paths = vec![
            "src/myapp/util.cljs".to_string(),
            "src/myapp/elements.cljs".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &file_paths, &registry);

        let render_id = graph
            .entities
            .keys()
            .find(|id| id.contains("render-items"))
            .expect("render-items entity should exist");

        let deps = graph.get_dependencies(render_id);
        assert!(
            deps.iter().any(|d| d.name == "vectorize-if-not-sequential"),
            "render-items should depend on vectorize-if-not-sequential via :as alias. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );

        let util_fn_id = graph
            .entities
            .keys()
            .find(|id| id.contains("vectorize-if-not-sequential"))
            .expect("vectorize-if-not-sequential entity should exist");

        let dependents = graph.get_dependents(util_fn_id);
        assert!(
            dependents.iter().any(|d| d.name == "render-items"),
            "vectorize-if-not-sequential should be depended on by render-items. Dependents: {:?}",
            dependents.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-clojure")]
    #[test]
    fn test_clojure_refer_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/myapp/strings.clj",
            r#"(ns myapp.strings)

(defn capitalize-first [s]
  (str (clojure.string/upper-case (subs s 0 1)) (subs s 1)))
"#,
        );

        write_file(
            root,
            "src/myapp/greeting.clj",
            r#"(ns myapp.greeting
  (:require [myapp.strings :refer [capitalize-first]]))

(defn greet [name]
  (str "Hello, " (capitalize-first name) "!"))
"#,
        );

        let file_paths = vec![
            "src/myapp/strings.clj".to_string(),
            "src/myapp/greeting.clj".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &file_paths, &registry);

        let greet_id = graph
            .entities
            .iter()
            .find(|(_, e)| e.name == "greet")
            .map(|(id, _)| id)
            .expect("greet entity should exist");

        let deps = graph.get_dependencies(greet_id);
        assert!(
            deps.iter().any(|d| d.name == "capitalize-first"),
            "greet should depend on capitalize-first via :refer. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-clojure")]
    #[test]
    fn test_clojure_kebab_reference_tracking() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/myapp/math.clj",
            r#"(ns myapp.math)

(defn square-root-of [n]
  (Math/sqrt n))
"#,
        );

        write_file(
            root,
            "src/myapp/stats.clj",
            r#"(ns myapp.stats
  (:require [myapp.math :refer [square-root-of]]))

(defn std-deviation [xs]
  (square-root-of (/ (reduce + xs) (count xs))))
"#,
        );

        let file_paths = vec![
            "src/myapp/math.clj".to_string(),
            "src/myapp/stats.clj".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &file_paths, &registry);

        let std_dev_id = graph
            .entities
            .keys()
            .find(|id| id.contains("std-deviation"))
            .expect("std-deviation entity should exist");

        let deps = graph.get_dependencies(std_dev_id);
        assert!(
            deps.iter().any(|d| d.name == "square-root-of"),
            "std-deviation should depend on square-root-of (kebab name via :refer). Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-clojure")]
    #[test]
    fn test_clojure_arithmetic_star_no_false_edge() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // math.clj defines a function whose body uses (*) for multiplication.
        // It does NOT import anything from another file.
        write_file(
            root,
            "src/myapp/math.clj",
            r#"(ns myapp.math)

(defn hypotenuse [a b]
  (Math/sqrt (+ (* a a) (* b b))))
"#,
        );

        // other.clj defines a function named * — this should NOT become a dependency
        // of hypotenuse because myapp.math never requires myapp.other.
        write_file(
            root,
            "src/myapp/other.clj",
            r#"(ns myapp.other)

(defn * [x y] (* x y))
"#,
        );

        let file_paths = vec![
            "src/myapp/math.clj".to_string(),
            "src/myapp/other.clj".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &file_paths, &registry);

        let hyp_id = graph
            .entities
            .keys()
            .find(|id| id.contains("hypotenuse"))
            .expect("hypotenuse entity should exist");

        let deps = graph.get_dependencies(hyp_id);
        assert!(
            !deps.iter().any(|d| d.name == "*"),
            "hypotenuse should not have a false '*' dependency from arithmetic use. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_gensym_does_not_blank_qualified_call() {
        // Regression: `strip_comments_and_strings` treated `#` as a Python/Ruby line
        // comment, so `result# (rewrite/fn! ...)` had everything from `#` to EOL blanked.
        // This prevented `CLOJURE_QUALIFIED_REF_RE` from finding `rewrite/fn!`.
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/myapp/rewrite.cljc",
            r#"(ns myapp.rewrite)

(defn add-expected-value! [path line value]
  (str path line value))
"#,
        );

        // The macro body contains `result#` (a gensym) followed by a qualified call
        // `rewrite/add-expected-value!` on the same line — this is the pattern that
        // was being incorrectly blanked.
        write_file(
            root,
            "src/myapp/core.cljc",
            r#"(ns myapp.core
  (:require [myapp.rewrite :as rewrite]))

(defmacro snap! [path line]
  `(let [result# (rewrite/add-expected-value! ~path ~line :result)]
     result#))
"#,
        );

        let file_paths = vec![
            "src/myapp/rewrite.cljc".to_string(),
            "src/myapp/core.cljc".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &file_paths, &registry);

        let snap_id = graph
            .entities
            .iter()
            .find(|(_, e)| e.name == "snap!")
            .map(|(id, _)| id.clone())
            .expect("snap! macro entity should exist");

        let deps = graph.get_dependencies(&snap_id);
        assert!(
            deps.iter().any(|d| d.name == "add-expected-value!"),
            "snap! should depend on add-expected-value! via rewrite/add-expected-value! alias call. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_reader_conditional_require_alias_resolved() {
        // Regression: `strip_comments_and_strings` treated `#` as a comment, so
        // `#?(:clj [still.rewrite :as rewrite] ...)` was blanked from `#` to EOL,
        // preventing CLOJURE_AS_RE from finding the `:as rewrite` alias.
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/myapp/backend.clj",
            r#"(ns myapp.backend)

(defn do-work! [x] x)
"#,
        );

        write_file(
            root,
            "src/myapp/shared.cljc",
            r#"(ns myapp.shared
  (:require #?(:clj [myapp.backend :as backend])))

(defn entry-point []
  (backend/do-work! 42))
"#,
        );

        let file_paths = vec![
            "src/myapp/backend.clj".to_string(),
            "src/myapp/shared.cljc".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &file_paths, &registry);

        let entry_id = graph
            .entities
            .iter()
            .find(|(_, e)| e.name == "entry-point")
            .map(|(id, _)| id.clone())
            .expect("entry-point entity should exist");

        let deps = graph.get_dependencies(&entry_id);
        assert!(
            deps.iter().any(|d| d.name == "do-work!"),
            "entry-point should depend on do-work! via reader-conditional alias. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_multiline_require_refer_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(
            root,
            "src/myapp/strings.clj",
            r#"(ns myapp.strings)

(defn capitalize-first [s]
  (str (clojure.string/upper-case (subs s 0 1)) (subs s 1)))
"#,
        );

        // :refer vector is on a separate line from the namespace vector opening bracket.
        write_file(
            root,
            "src/myapp/greeting.clj",
            r#"(ns myapp.greeting
  (:require
   [myapp.strings
    :refer [capitalize-first]]))

(defn greet [name]
  (str "Hello, " (capitalize-first name) "!"))
"#,
        );

        let file_paths = vec![
            "src/myapp/strings.clj".to_string(),
            "src/myapp/greeting.clj".to_string(),
        ];
        let (graph, _) = EntityGraph::build(root, &file_paths, &registry);

        let greet_id = graph
            .entities
            .iter()
            .find(|(_, e)| e.name == "greet")
            .map(|(id, _)| id)
            .expect("greet entity should exist");

        let deps = graph.get_dependencies(greet_id);
        assert!(
            deps.iter().any(|d| d.name == "capitalize-first"),
            "greet should depend on capitalize-first via multi-line :refer. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    // ── is_test_entity / filter_test_entities tests ─────────────────────

    fn make_entity(name: &str, file_path: &str, content: &str) -> SemanticEntity {
        SemanticEntity {
            id: format!("{}::function::{}", file_path, name),
            name: name.to_string(),
            entity_type: "function".to_string(),
            file_path: file_path.to_string(),
            start_line: 1,
            end_line: 5,
            content: content.to_string(),
            content_hash: String::new(),
            structural_hash: None,
            parent_id: None,
            metadata: None,
        }
    }

    #[test]
    fn test_entity_detected_by_name_pattern() {
        let entity = make_entity("test_login", "src/auth.py", "def test_login(): pass");
        assert!(is_test_entity(&entity, &[]));
    }

    #[test]
    fn test_entity_detected_by_path_and_content_marker() {
        // Path match + content marker both required
        let entity = make_entity("run", "e2e-tests/login.ts", "describe('login', () => { it('works', () => {}) })");
        assert!(is_test_entity(&entity, &[]));
    }

    #[test]
    fn test_entity_not_detected_in_production_code() {
        let entity = make_entity("handle_request", "src/server.rs", "fn handle_request() {}");
        assert!(!is_test_entity(&entity, &[]));
    }

    #[test]
    fn test_entity_path_match_without_content_marker_not_detected() {
        // Path says test dir, but content has no test marker → not a test
        let entity = make_entity("helper", "tests/helpers.py", "def helper(): return 42");
        assert!(!is_test_entity(&entity, &[]));
    }

    #[test]
    fn test_entity_detected_in_hyphenated_test_dir() {
        let entity = make_entity("check", "integration-tests/api.py", "@pytest.mark.slow\ndef check(): pass");
        assert!(is_test_entity(&entity, &[]));
    }

    #[test]
    fn test_entity_detected_in_dunder_tests_dir() {
        let entity = make_entity("render", "__tests__/Button.test.tsx", "test('renders', () => {})");
        assert!(is_test_entity(&entity, &[]));
    }

    #[test]
    fn test_entity_detected_with_custom_dir() {
        let entity = make_entity("verify", "qa/smoke.py", "@pytest.fixture\ndef verify(): pass");
        // Without custom dirs: not detected (no name match, "qa" not a built-in)
        assert!(!is_test_entity(&entity, &[]));
        // With custom dirs: detected because path matches + content has @pytest
        let custom = vec!["qa".to_string()];
        assert!(is_test_entity(&entity, &custom));
    }

    #[test]
    fn test_entity_contest_dir_not_false_positive() {
        let entity = make_entity("solve", "contest/problem_a.py", "def solve(): test('input')");
        assert!(!is_test_entity(&entity, &[]));
    }

    #[test]
    fn filter_test_entities_with_custom_dirs_includes_custom_matches() {
        let entities = vec![
            make_entity("test_a", "src/lib.rs", "#[test]\nfn test_a() {}"),
            make_entity("run", "qa/smoke.rs", "#[test]\nfn run() {}"),
            make_entity("main", "src/main.rs", "fn main() {}"),
        ];
        let entity_map: std::collections::HashMap<String, EntityInfo> = entities
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    EntityInfo {
                        id: e.id.clone(),
                        name: e.name.clone(),
                        entity_type: e.entity_type.clone(),
                        file_path: e.file_path.clone(),
                        parent_id: None,
                        start_line: e.start_line,
                        end_line: e.end_line,
                    },
                )
            })
            .collect();
        let graph = EntityGraph::from_parts(entity_map, vec![]);

        let builtin = graph.filter_test_entities(&entities);
        assert!(builtin.contains("src/lib.rs::function::test_a"));
        assert!(!builtin.contains("qa/smoke.rs::function::run"));

        let custom = vec!["qa".to_string()];
        let with_custom = graph.filter_test_entities_with_custom_dirs(&entities, &custom);
        assert!(with_custom.contains("src/lib.rs::function::test_a"));
        assert!(with_custom.contains("qa/smoke.rs::function::run"));
        assert!(!with_custom.contains("src/main.rs::function::main"));
    }
}
