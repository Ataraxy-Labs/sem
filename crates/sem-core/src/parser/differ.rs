#[cfg(feature = "parallel")]
use rayon::prelude::*;
use serde::Serialize;

use crate::git::types::{FileChange, FileStatus};

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
use crate::model::change::{ChangeType, SemanticChange};
use crate::model::entity::{disambiguate_colliding_entity_ids, SemanticEntity};
use crate::model::identity::match_entities;
use crate::parser::plugin::SemanticParserPlugin;
use crate::parser::registry::ParserRegistry;
use std::collections::{HashMap, HashSet};

mod phase_timing;
use phase_timing::PHASE_ACC;
pub use phase_timing::{diff_phase_timings, DiffPhaseTimings};

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub changes: Vec<SemanticChange>,
    pub file_count: usize,
    pub added_count: usize,
    pub modified_count: usize,
    pub deleted_count: usize,
    pub moved_count: usize,
    pub renamed_count: usize,
    pub reordered_count: usize,
    pub orphan_count: usize,
    pub total_entities_before: usize,
    pub total_entities_after: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryFileChange {
    pub file_path: String,
    pub status: FileStatus,
    pub old_file_path: Option<String>,
}

impl From<&FileChange> for BinaryFileChange {
    fn from(file: &FileChange) -> Self {
        Self {
            file_path: file.file_path.clone(),
            status: file.status.clone(),
            old_file_path: file.old_file_path.clone(),
        }
    }
}

pub fn collect_binary_file_changes(file_changes: &[FileChange]) -> Vec<BinaryFileChange> {
    file_changes
        .iter()
        .filter(|file| lacks_diffable_content(file))
        .map(BinaryFileChange::from)
        .collect()
}

fn lacks_diffable_content(file: &FileChange) -> bool {
    match &file.status {
        FileStatus::Added => file.after_content.is_none(),
        FileStatus::Deleted => file.before_content.is_none(),
        FileStatus::Modified | FileStatus::Renamed => {
            file.before_content.is_none() || file.after_content.is_none()
        }
    }
}

pub fn compute_semantic_diff(
    file_changes: &[FileChange],
    registry: &ParserRegistry,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> DiffResult {
    // Fresh accumulation for this call — see phase_timing's module doc for
    // why a top-level reset is needed before the parallel fan-out below.
    phase_timing::reset_diff_phase_timings();

    // Process files in parallel: each file's entity extraction and matching is independent
    let per_file_changes: Vec<(String, Vec<SemanticChange>, usize, usize)> =
        maybe_par_iter!(file_changes)
            .filter(|file| !lacks_diffable_content(file))
            .filter_map(|file| {
                let content_hint = file
                    .after_content
                    .as_deref()
                    .or(file.before_content.as_deref())
                    .unwrap_or("");
                let resolved = registry.resolve_file_path(&file.file_path);
                let detection_path = resolved.as_deref().unwrap_or(&file.file_path);
                let plugin = registry.get_plugin_with_content(detection_path, content_hint)?;

                let before_entities = if let Some(ref content) = file.before_content {
                    let before_path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                    let before_resolved = registry.resolve_file_path(before_path);
                    let before_detection = before_resolved.as_deref().unwrap_or(before_path);
                    phase_timing::timed(&PHASE_ACC.extraction_ns, || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut entities = plugin.extract_entities(content, before_detection);
                            disambiguate_colliding_entity_ids(&mut entities);
                            entities
                        }))
                        .unwrap_or_default()
                    })
                } else {
                    Vec::new()
                };

                let after_entities = if let Some(ref content) = file.after_content {
                    phase_timing::timed(&PHASE_ACC.extraction_ns, || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut entities = plugin.extract_entities(content, detection_path);
                            disambiguate_colliding_entity_ids(&mut entities);
                            entities
                        }))
                        .unwrap_or_default()
                    })
                } else {
                    Vec::new()
                };

                let before_count = before_entities.len();
                let after_count = after_entities.len();

                let mut result = phase_timing::timed(&PHASE_ACC.matching_ns, || {
                    let mut result = match_entities(
                        &before_entities,
                        &after_entities,
                        &file.file_path,
                        None,
                        commit_sha,
                        author,
                    );

                    // Suppress parent entities whose modification is already explained
                    // by child entity changes (e.g. impl blocks when methods changed).
                    suppress_redundant_parents(
                        &mut result.changes,
                        &before_entities,
                        &after_entities,
                    );
                    result
                });

                // Detect orphan changes (lines that changed outside any entity span).
                let orphans = phase_timing::timed(&PHASE_ACC.orphan_ns, || {
                    detect_orphan_changes(
                        file,
                        &before_entities,
                        &after_entities,
                        Some(plugin),
                        detection_path,
                        commit_sha,
                        author,
                    )
                });
                result.changes.extend(orphans);

                // Total order: `entity_line` alone is not one — same-line
                // entities would keep whatever relative order the phases
                // happened to push them in.
                result.changes.sort_by(|a, b| {
                    a.entity_line
                        .cmp(&b.entity_line)
                        .then_with(|| a.entity_id.cmp(&b.entity_id))
                });

                if result.changes.is_empty() {
                    None
                } else {
                    Some((
                        file.file_path.clone(),
                        result.changes,
                        before_count,
                        after_count,
                    ))
                }
            })
            .collect();

    let mut all_changes: Vec<SemanticChange> = Vec::new();
    let mut files_with_changes: HashSet<String> = HashSet::new();
    let mut total_entities_before: usize = 0;
    let mut total_entities_after: usize = 0;
    for (file_path, changes, before_count, after_count) in per_file_changes {
        files_with_changes.insert(file_path);
        all_changes.extend(changes);
        total_entities_before += before_count;
        total_entities_after += after_count;
    }

    // Single-pass counting. Orphans are first-class changes for the
    // change-type buckets, and orphan_count is cross-cutting metadata.
    let mut added_count = 0;
    let mut modified_count = 0;
    let mut deleted_count = 0;
    let mut moved_count = 0;
    let mut renamed_count = 0;
    let mut reordered_count = 0;
    let mut orphan_count = 0;

    for c in &all_changes {
        if c.entity_type == "orphan" {
            orphan_count += 1;
        }
        match c.change_type {
            ChangeType::Added => added_count += 1,
            ChangeType::Modified => modified_count += 1,
            ChangeType::Deleted => deleted_count += 1,
            ChangeType::Moved => {
                moved_count += 1;
                if c.has_content_change() {
                    modified_count += 1;
                }
            }
            ChangeType::Renamed => {
                renamed_count += 1;
                if c.has_content_change() {
                    modified_count += 1;
                }
            }
            ChangeType::Reordered => {
                reordered_count += 1;
                if c.has_content_change() {
                    modified_count += 1;
                }
            }
        }
    }

    DiffResult {
        changes: all_changes,
        file_count: files_with_changes.len(),
        added_count,
        modified_count,
        deleted_count,
        moved_count,
        renamed_count,
        reordered_count,
        orphan_count,
        total_entities_before,
        total_entities_after,
    }
}

fn suppress_redundant_parents(
    changes: &mut Vec<SemanticChange>,
    before: &[SemanticEntity],
    after: &[SemanticEntity],
) {
    if changes.len() < 2 {
        return;
    }

    const CONTAINER_TYPES: &[&str] = &[
        "impl",
        "trait",
        "module",
        "class",
        "interface",
        "protocol",
        "mixin",
        "extension",
        "namespace",
        "export",
        "package",
        "field",
        "variable",
        "svelte_instance_script",
        "svelte_module_script",
        "object",
    ];

    let before_by_id: HashMap<&str, &SemanticEntity> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();
    let after_by_id: HashMap<&str, &SemanticEntity> =
        after.iter().map(|e| (e.id.as_str(), e)).collect();

    let mut before_children: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    for e in before {
        if let Some(ref pid) = e.parent_id {
            before_children.entry(pid.as_str()).or_default().push(e);
        }
    }
    let mut after_children: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    for e in after {
        if let Some(ref pid) = e.parent_id {
            after_children.entry(pid.as_str()).or_default().push(e);
        }
    }

    let changed_ids: HashSet<&str> = changes.iter().map(|c| c.entity_id.as_str()).collect();

    // Did the container's OWN declaration change — its content with its
    // children's content stripped out? `None` when the two sides are not
    // comparable (the container is missing on one side, or its type changed),
    // i.e. when we cannot tell. Suppression is only ever sound when the
    // container change is fully explained by the child changes we do report.
    let own_declaration_changed = |eid: &str| -> Option<bool> {
        let bp = before_by_id.get(eid)?;
        let ap = after_by_id.get(eid)?;
        if bp.entity_type != ap.entity_type {
            return None;
        }
        let b_children: &[&SemanticEntity] = before_children
            .get(eid)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let a_children: &[&SemanticEntity] =
            after_children.get(eid).map(|v| v.as_slice()).unwrap_or(&[]);
        Some(
            strip_children_content(&bp.content, bp.start_line, b_children)
                != strip_children_content(&ap.content, ap.start_line, a_children),
        )
    };

    let mut suppress: HashSet<String> = HashSet::new();
    for change in changes.iter() {
        if !matches!(
            change.change_type,
            ChangeType::Modified | ChangeType::Added | ChangeType::Deleted
        ) {
            continue;
        }
        if !CONTAINER_TYPES.contains(&change.entity_type.as_str()) {
            continue;
        }
        let eid = change.entity_id.as_str();
        let b_children = before_children
            .get(eid)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let a_children = after_children.get(eid).map(|v| v.as_slice()).unwrap_or(&[]);

        let has_changed_child = b_children
            .iter()
            .any(|c| changed_ids.contains(c.id.as_str()))
            || a_children
                .iter()
                .any(|c| changed_ids.contains(c.id.as_str()));
        if !has_changed_child {
            continue;
        }

        // Added/Deleted: suppress unconditionally; the children carry the detail.
        // Modified: only suppress if the container's own declaration is unchanged
        // and the value type didn't transition.
        let should_suppress = if change.change_type == ChangeType::Modified {
            own_declaration_changed(eid) == Some(false)
        } else {
            true
        };

        if should_suppress {
            suppress.insert(change.entity_id.clone());
        }
    }

    // Suppress an old parent that a Moved child left behind when the old
    // parent itself appears as a change — handles the parent-rename case
    // where the parent itself failed to match.
    //
    // But only when the old parent's own declaration did not itself change:
    // the Moved child does not explain an edit to its former container's
    // declaration, and a Moved pairing can be a false positive (phase 5
    // matches on Jaccard >= 0.8 and entity_type alone, with no locality
    // prior). Suppressing unconditionally dropped e.g. `interface Beta
    // extends Base` losing its `extends Base` from the change record
    // entirely. `None` (not comparable) keeps the old unconditional
    // behaviour, which is what the parent-rename case above relies on.
    for change in changes.iter() {
        if change.change_type == ChangeType::Moved {
            if let Some(ref old_pid) = change.old_parent_id {
                if changed_ids.contains(old_pid.as_str())
                    && own_declaration_changed(old_pid) != Some(true)
                {
                    suppress.insert(old_pid.clone());
                }
            }
        }
    }

    if !suppress.is_empty() {
        changes.retain(|c| !suppress.contains(&c.entity_id));
    }

    // Drop a Moved child whose key is unchanged and whose old parent matches
    // a Renamed entity — the child only "moved" because the parent renamed.
    let renamed_before_ids: HashSet<&str> = changes
        .iter()
        .filter(|c| c.change_type == ChangeType::Renamed)
        .filter_map(|c| {
            let old_name = c.old_entity_name.as_deref()?;
            let after_entity = after_by_id.get(c.entity_id.as_str())?;
            before
                .iter()
                .find(|e| {
                    e.name == old_name
                        && e.entity_type == after_entity.entity_type
                        && e.parent_id == after_entity.parent_id
                })
                .map(|e| e.id.as_str())
        })
        .collect();

    if !renamed_before_ids.is_empty() {
        changes.retain(|c| {
            !(c.change_type == ChangeType::Moved
                && c.old_entity_name.is_none()
                && c.old_parent_id
                    .as_deref()
                    .map_or(false, |pid| renamed_before_ids.contains(pid)))
        });
    }
}

fn strip_children_content(
    content: &str,
    parent_start_line: usize,
    children: &[&SemanticEntity],
) -> String {
    let mut line_starts = vec![0];
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            line_starts.push(idx + ch.len_utf8());
        }
    }

    let mut excluded_ranges: Vec<(usize, usize)> = Vec::new();
    for child in children {
        let start_idx = child.start_line.saturating_sub(parent_start_line);
        let end_idx = child.end_line.saturating_sub(parent_start_line);
        let search_start = line_starts.get(start_idx).copied().unwrap_or(0);
        let search_end = line_starts
            .get(end_idx.saturating_add(1))
            .copied()
            .unwrap_or(content.len())
            .min(content.len());

        if !child.content.is_empty() && search_start <= search_end {
            let search_window = &content[search_start..search_end];
            if search_window.starts_with(&child.content) {
                excluded_ranges.push((search_start, search_start + child.content.len()));
                continue;
            }

            if let Some(relative_start) = search_window.find(&child.content) {
                let start = search_start + relative_start;
                excluded_ranges.push((start, start + child.content.len()));
                continue;
            }
        }
    }

    if excluded_ranges.is_empty() {
        return normalize_content_for_parent_suppression(content);
    }

    excluded_ranges.sort_unstable();
    let mut merged_ranges: Vec<(usize, usize)> = Vec::new();
    for (start, end) in excluded_ranges {
        if let Some((_, merged_end)) = merged_ranges.last_mut() {
            if start <= *merged_end {
                *merged_end = (*merged_end).max(end);
                continue;
            }
        }
        merged_ranges.push((start, end));
    }

    let mut stripped = String::with_capacity(content.len());
    let mut cursor = 0;
    for (start, end) in merged_ranges {
        if cursor < start {
            stripped.push_str(&content[cursor..start]);
        }
        cursor = end.max(cursor);
    }
    if cursor < content.len() {
        stripped.push_str(&content[cursor..]);
    }

    normalize_content_for_parent_suppression(&stripped)
}

fn normalize_content_for_parent_suppression(content: &str) -> String {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Detect changes in the bytes that fall outside every entity span.
/// These are things like use statements, crate-level attributes, standalone
/// comments, macro invocations that aren't tracked as entities — and the
/// residue of a line an entity only partly occupies, such as a trailing
/// comment after a closing brace.
fn detect_orphan_changes(
    file: &FileChange,
    before_entities: &[SemanticEntity],
    after_entities: &[SemanticEntity],
    plugin: Option<&dyn SemanticParserPlugin>,
    detection_path: &str,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> Vec<SemanticChange> {
    let before_text = file.before_content.as_deref().unwrap_or("");
    let after_text = file.after_content.as_deref().unwrap_or("");

    let before_orphans = orphan_segments(before_text, before_entities);
    let after_orphans = orphan_segments(after_text, after_entities);
    let mut changes = Vec::new();

    for (before_idx, after_idx) in orphan_segment_change_pairs(&before_orphans, &after_orphans) {
        let before_orphan = before_idx.and_then(|idx| before_orphans.get(idx));
        let after_orphan = after_idx.and_then(|idx| after_orphans.get(idx));
        let before_content = orphan_content(before_orphan);
        let after_content = orphan_content(after_orphan);

        // Skip if orphan content is unchanged, including blank-only segments.
        if before_content == after_content {
            continue;
        }

        let change_type = if before_content.is_none() {
            ChangeType::Added
        } else if after_content.is_none() {
            ChangeType::Deleted
        } else {
            ChangeType::Modified
        };

        let current_orphan = match change_type {
            ChangeType::Deleted => before_orphan,
            _ => after_orphan.or(before_orphan),
        };
        let Some(current_orphan) = current_orphan else {
            continue;
        };
        let span_label = if change_type == ChangeType::Deleted {
            "oldL"
        } else {
            "L"
        };
        let orphan_id = format!(
            "{}::orphan::{}@{}{}-{}",
            file.file_path,
            change_type,
            span_label,
            current_orphan.start_line,
            current_orphan.end_line
        );

        changes.push(SemanticChange {
            id: format!("change::{orphan_id}"),
            entity_id: orphan_id,
            change_type,
            entity_type: "orphan".to_string(),
            entity_name: "module-level".to_string(),
            entity_line: current_orphan.start_line,
            start_line: current_orphan.start_line,
            end_line: current_orphan.end_line,
            old_start_line: before_orphan.map(|orphan| orphan.start_line),
            old_end_line: before_orphan.map(|orphan| orphan.end_line),
            parent_name: None,
            file_path: file.file_path.clone(),
            old_entity_name: None,
            old_file_path: None,
            old_parent_id: None,
            before_content: before_content.map(str::to_string),
            after_content: after_content.map(str::to_string),
            commit_sha: commit_sha.map(String::from),
            author: author.map(String::from),
            timestamp: None,
            structural_change: orphan_structural_change(
                before_content,
                after_content,
                plugin,
                detection_path,
            ),
        });
    }

    changes
}

fn orphan_structural_change(
    before_content: Option<&str>,
    after_content: Option<&str>,
    plugin: Option<&dyn SemanticParserPlugin>,
    detection_path: &str,
) -> Option<bool> {
    let plugin = plugin?;
    let before_hash =
        plugin.structural_hash_content(before_content.unwrap_or_default(), detection_path)?;
    let after_hash =
        plugin.structural_hash_content(after_content.unwrap_or_default(), detection_path)?;

    Some(before_hash != after_hash)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanSegment {
    start_line: usize,
    end_line: usize,
    content: String,
}

/// Byte offset of the start of each line, plus a final sentinel at
/// `text.len()`. `line_starts[i]` is where line `i + 1` begins.
fn line_starts_of(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut pos = 0usize;
    for line in text.split_inclusive('\n') {
        pos += line.len();
        starts.push(pos);
    }
    starts
}

/// 1-based line number containing `byte`.
fn line_of(line_starts: &[usize], byte: usize) -> usize {
    line_starts.partition_point(|&start| start <= byte).max(1)
}

/// The byte ranges the entities cover, sorted and merged.
///
/// An entity's own `start_byte..end_byte` is used when it actually slices
/// `text` to that entity's content — the extraction lens's `get` law. When it
/// does not (a plugin that reports no span, or one whose offsets are relative
/// to an extracted sub-document, as vue/svelte children are), the range falls
/// back to the entity's full line span, which is what the orphan cover used
/// to assume for *every* entity.
fn covered_byte_ranges(
    text: &str,
    entities: &[SemanticEntity],
    line_starts: &[usize],
) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(entities.len());

    for entity in entities {
        if let (Some(start), Some(end)) = (entity.start_byte, entity.end_byte) {
            if start < end
                && end <= text.len()
                && text.is_char_boundary(start)
                && text.is_char_boundary(end)
                && text[start..end] == entity.content
            {
                ranges.push((start, end));
                continue;
            }
        }

        if entity.start_line == 0 || entity.start_line > entity.end_line {
            continue;
        }
        let (Some(&start), Some(&end)) = (
            line_starts.get(entity.start_line - 1),
            line_starts.get(entity.end_line),
        ) else {
            continue;
        };
        if start < end {
            ranges.push((start, end));
        }
    }

    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, prev_end)) if start <= *prev_end => *prev_end = (*prev_end).max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The orphan cover: the byte-complement of the entity cover. Together the
/// two are jointly epic over the file, so every byte difference between two
/// files is attributed to an entity change or to an orphan change.
///
/// Blank runs are dropped here rather than reported: they are the diff's
/// declared kernel (blank-line-only edits outside entities are invisible),
/// and the complement is full of one-byte newline slivers between adjacent
/// entities that carry no information.
fn orphan_segments(text: &str, entities: &[SemanticEntity]) -> Vec<OrphanSegment> {
    let line_starts = line_starts_of(text);
    let mut segments = Vec::new();

    let push = |start: usize, end: usize, segments: &mut Vec<OrphanSegment>| {
        let slice = &text[start..end];
        if slice.trim().is_empty() {
            return;
        }
        // Drop the segment's own line terminator, so a whole-line segment
        // reads as its lines and not as its lines plus a dangling newline.
        let content = slice.strip_suffix('\n').unwrap_or(slice);
        let content = content.strip_suffix('\r').unwrap_or(content);
        // Report the lines the segment's *meaningful* content occupies. A
        // segment starts at the byte after the preceding entity ends, which is
        // that entity's own closing line; blaming its line number on this
        // segment would be misleading. The content still carries every byte.
        let lead = slice.len() - slice.trim_start().len();
        let trail = slice.len() - slice.trim_end().len();
        segments.push(OrphanSegment {
            start_line: line_of(&line_starts, start + lead),
            end_line: line_of(&line_starts, end - trail - 1),
            content: content.to_string(),
        });
    };

    let mut cursor = 0usize;
    for (start, end) in covered_byte_ranges(text, entities, &line_starts) {
        if cursor < start {
            push(cursor, start, &mut segments);
        }
        cursor = cursor.max(end);
    }
    if cursor < text.len() {
        push(cursor, text.len(), &mut segments);
    }

    segments
}

fn orphan_content(segment: Option<&OrphanSegment>) -> Option<&str> {
    segment
        .map(|segment| segment.content.as_str())
        .filter(|content| !content.trim().is_empty())
}

fn orphan_segment_change_pairs(
    before: &[OrphanSegment],
    after: &[OrphanSegment],
) -> Vec<(Option<usize>, Option<usize>)> {
    let anchors = orphan_segment_lcs(before, after);
    let mut pairs = Vec::new();
    let mut before_start = 0;
    let mut after_start = 0;

    for (before_anchor, after_anchor) in anchors {
        append_orphan_gap_pairs(
            &mut pairs,
            before_start,
            before_anchor,
            after_start,
            after_anchor,
        );
        before_start = before_anchor + 1;
        after_start = after_anchor + 1;
    }

    append_orphan_gap_pairs(
        &mut pairs,
        before_start,
        before.len(),
        after_start,
        after.len(),
    );

    pairs
}

fn append_orphan_gap_pairs(
    pairs: &mut Vec<(Option<usize>, Option<usize>)>,
    before_start: usize,
    before_end: usize,
    after_start: usize,
    after_end: usize,
) {
    let before_len = before_end.saturating_sub(before_start);
    let after_len = after_end.saturating_sub(after_start);

    if before_len == after_len {
        for i in 0..before_len {
            pairs.push((Some(before_start + i), Some(after_start + i)));
        }
        return;
    }

    for i in 0..before_len {
        pairs.push((Some(before_start + i), None));
    }
    for i in 0..after_len {
        pairs.push((None, Some(after_start + i)));
    }
}

fn orphan_segment_lcs(before: &[OrphanSegment], after: &[OrphanSegment]) -> Vec<(usize, usize)> {
    let mut dp = vec![vec![0; after.len() + 1]; before.len() + 1];

    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            dp[i][j] = if orphan_segments_equal(&before[i], &after[j]) {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut anchors = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < before.len() && j < after.len() {
        if orphan_segments_equal(&before[i], &after[j]) {
            anchors.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }

    anchors
}

fn orphan_segments_equal(before: &OrphanSegment, after: &OrphanSegment) -> bool {
    match (orphan_content(Some(before)), orphan_content(Some(after))) {
        (Some(before), Some(after)) => before == after,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{FileChange, FileStatus};
    use crate::parser::plugins::create_default_registry;

    fn modified_file(path: &str, before: &str, after: &str) -> FileChange {
        FileChange {
            file_path: path.to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(before.to_string()),
            after_content: Some(after.to_string()),
        }
    }

    fn renamed_file(old_path: &str, new_path: &str, before: &str, after: &str) -> FileChange {
        FileChange {
            file_path: new_path.to_string(),
            status: FileStatus::Renamed,
            old_file_path: Some(old_path.to_string()),
            before_content: Some(before.to_string()),
            after_content: Some(after.to_string()),
        }
    }

    fn entity_span(id: &str, start_line: usize, end_line: usize) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: "a.rs".to_string(),
            entity_type: "function".to_string(),
            name: id.to_string(),
            parent_id: None,
            content: String::new(),
            content_hash: String::new(),
            structural_hash: None,

            kappa: None,
            start_line,
            end_line,
            start_byte: None,
            end_byte: None,
            metadata: None,
        }
    }

    #[test]
    fn orphan_only_change_counts_file_and_orphan() {
        let before = "# old module comment\n\ndef value():\n    return 1\n";
        let after = "# new module comment\n\ndef value():\n    return 1\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("app.py", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.file_count, 1);
        assert_eq!(result.orphan_count, 1);
        assert_eq!(result.modified_count, 1);
        assert_eq!(result.changes[0].entity_type, "orphan");
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
        assert_eq!(result.changes[0].structural_change, Some(false));
    }

    #[test]
    fn orphan_code_change_is_structural() {
        let before = "import os\n\ndef value():\n    return 1\n";
        let after = "import sys\n\ndef value():\n    return 1\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("app.py", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].entity_type, "orphan");
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
        assert_eq!(result.changes[0].structural_change, Some(true));
    }

    #[test]
    fn orphan_shebang_change_is_structural() {
        let before = "#!/usr/bin/env python3\ndef value():\n    return 1\n";
        let after = "#!/usr/bin/env python\ndef value():\n    return 1\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("script", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].entity_type, "orphan");
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
        assert_eq!(result.changes[0].structural_change, Some(true));
    }

    #[test]
    fn test_parent_suppressed_when_only_child_modified() {
        let before = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id)\n";
        let after  = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id, include_deleted=False)\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("svc.py", before, after)],
            &registry,
            None,
            None,
        );

        let names: Vec<&str> = result
            .changes
            .iter()
            .map(|c| c.entity_name.as_str())
            .collect();
        assert!(
            result.changes.iter().any(|c| c.entity_name == "get_user"),
            "expected method get_user in changes, got: {names:?}"
        );
        assert!(
            !result
                .changes
                .iter()
                .any(|c| c.entity_name == "UserService" && c.change_type == ChangeType::Modified),
            "class should be suppressed when only the method body changed, got: {names:?}"
        );
    }

    #[test]
    fn test_protocol_parent_suppressed_when_only_associatedtype_renamed() {
        let before = "protocol Repository {\n    associatedtype Item\n}\n";
        let after = "protocol Repository {\n    associatedtype Canvas\n}\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("Repository.swift", before, after)],
            &registry,
            None,
            None,
        );

        let names: Vec<&str> = result
            .changes
            .iter()
            .map(|c| c.entity_name.as_str())
            .collect();
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_type == "associatedtype"),
            "expected associatedtype change, got: {names:?}"
        );
        assert!(
            !result
                .changes
                .iter()
                .any(|c| c.entity_name == "Repository" && c.change_type == ChangeType::Modified),
            "protocol should be suppressed when only the associatedtype changed, got: {names:?}"
        );
    }

    #[test]
    fn test_protocol_parent_not_suppressed_when_own_declaration_changes() {
        let before = "protocol Repository {\n    associatedtype Item\n}\n";
        let after = "protocol Repository: Sendable {\n    associatedtype Canvas\n}\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("Repository.swift", before, after)],
            &registry,
            None,
            None,
        );

        let names: Vec<&str> = result
            .changes
            .iter()
            .map(|c| c.entity_name.as_str())
            .collect();
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_type == "associatedtype"),
            "expected associatedtype change, got: {names:?}"
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_name == "Repository" && c.change_type == ChangeType::Modified),
            "protocol should remain Modified when its own declaration changed, got: {names:?}"
        );
    }

    #[test]
    fn test_parent_not_suppressed_when_own_declaration_changes() {
        let before = "class UserService:\n    def get_user(self, user_id):\n        return db.find(user_id)\n";
        let after  = "class UserService(BaseService):\n    def get_user(self, user_id):\n        return db.find(user_id, include_deleted=False)\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("svc.py", before, after)],
            &registry,
            None,
            None,
        );

        let names: Vec<&str> = result
            .changes
            .iter()
            .map(|c| c.entity_name.as_str())
            .collect();
        assert!(
            result.changes.iter().any(|c| c.entity_name == "get_user"),
            "expected method get_user in changes, got: {names:?}"
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_name == "UserService" && c.change_type == ChangeType::Modified),
            "class should remain Modified when its own declaration changed, got: {names:?}"
        );
    }

    #[test]
    fn test_nested_typescript_class_field_diff_reports_leaf_method() {
        let before = r#"class L1 {
  L2 = class {
    L3 = class {
      L4 = class {
        method() { return 1; }
      };
    };
  };
}
"#;
        let after = r#"class L1 {
  L2 = class {
    L3 = class {
      L4 = class {
        method() { return 999; }
      };
    };
  };
}
"#;

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("a.ts", before, after)],
            &registry,
            None,
            None,
        );

        let changes: Vec<_> = result
            .changes
            .iter()
            .map(|c| (c.entity_name.as_str(), c.entity_type.as_str()))
            .collect();
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_id == "a.ts::class::L1::L2::L3::L4::method"),
            "expected method leaf change, got: {changes:?}"
        );
        assert!(
            !result.changes.iter().any(|c| c.entity_type == "field"),
            "field containers should be suppressed when only a nested method changed, got: {changes:?}"
        );
    }

    #[test]
    fn test_nested_typescript_object_literal_diff_reports_leaf_method() {
        let before = r#"export const svc = {
  open(): number { return 1; },
  close(): number { return 0; },
};
"#;
        let after = r#"export const svc = {
  open(): number { return 2; },
  close(): number { return 0; },
};
"#;

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("service.ts", before, after)],
            &registry,
            None,
            None,
        );

        let changes: Vec<_> = result
            .changes
            .iter()
            .map(|c| (c.entity_name.as_str(), c.entity_type.as_str()))
            .collect();
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_id == "service.ts::variable::svc::open"),
            "expected object-literal method leaf change, got: {changes:?}"
        );
        assert!(
            !result
                .changes
                .iter()
                .any(|c| c.entity_name == "svc" && c.entity_type == "variable"),
            "variable container should be suppressed when only a nested method changed, got: {changes:?}"
        );
    }

    #[test]
    fn test_nested_typescript_object_literal_pair_diff_reports_leaf_methods() {
        let before = r#"export const svc = {
  reset: () => 1,
  flush: function() { return 0; },
};
"#;
        let after = r#"export const svc = {
  reset: () => 2,
  flush: function() { return 3; },
};
"#;

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("service.ts", before, after)],
            &registry,
            None,
            None,
        );

        let changes: Vec<_> = result
            .changes
            .iter()
            .map(|c| (c.entity_name.as_str(), c.entity_type.as_str()))
            .collect();
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_id == "service.ts::variable::svc::reset"),
            "expected arrow-valued object method change, got: {changes:?}"
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_id == "service.ts::variable::svc::flush"),
            "expected function-valued object method change, got: {changes:?}"
        );
        assert!(
            !result
                .changes
                .iter()
                .any(|c| c.entity_name == "svc" && c.entity_type == "variable"),
            "variable container should be suppressed when only nested function-valued properties changed, got: {changes:?}"
        );
    }

    #[test]
    fn test_inline_typescript_object_literal_keeps_parent_variable_changes() {
        let before = "export const svc = { open() { return 1; }, enabled: true };\n";
        let after = "export let svc = { open() { return 2; }, enabled: false };\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("service.ts", before, after)],
            &registry,
            None,
            None,
        );

        let changes: Vec<_> = result
            .changes
            .iter()
            .map(|c| (c.entity_name.as_str(), c.entity_type.as_str()))
            .collect();
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_id == "service.ts::variable::svc::open"),
            "expected nested method change, got: {changes:?}"
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_name == "svc" && c.entity_type == "variable"),
            "parent variable change should remain visible, got: {changes:?}"
        );
    }

    #[test]
    fn renamed_file_with_edited_entity_reports_move_not_add_delete() {
        let before = "def foo():\n    return alpha + beta + gamma\n";
        let after = "def foo():\n    return one + two + three\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[renamed_file("old.py", "new.py", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.added_count, 0);
        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.modified_count, 1);
        assert_eq!(result.moved_count, 1);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].entity_name, "foo");
        assert_eq!(result.changes[0].old_file_path.as_deref(), Some("old.py"));
        assert_eq!(result.changes[0].structural_change, Some(true));
    }

    #[test]
    fn duplicate_markdown_heading_reports_first_section_modification() {
        let before = "# Same Title\n\noriginal content of section A\n\n# Same Title\n\ncontent of section B\n";
        let after = "# Same Title\n\nMODIFIED content of section A\n\n# Same Title\n\ncontent of section B\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("doc.md", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.modified_count, 1, "{:?}", result.changes);
        assert_eq!(result.changes.len(), 1, "{:?}", result.changes);

        let change = &result.changes[0];
        assert_eq!(change.change_type, ChangeType::Modified);
        assert_eq!(change.entity_name, "Same Title");
        assert_eq!(change.entity_line, 1);
        assert!(change
            .before_content
            .as_deref()
            .unwrap_or_default()
            .contains("original content of section A"));
        assert!(change
            .after_content
            .as_deref()
            .unwrap_or_default()
            .contains("MODIFIED content of section A"));
    }

    #[test]
    fn orphan_changes_count_toward_change_type_buckets() {
        let before = "def foo():\n    return 1\n\ndef bar():\n    return 2\n";
        let after = "# just a comment\n";

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("svc.py", before, after)],
            &registry,
            None,
            None,
        );

        assert_eq!(result.added_count, 1);
        assert_eq!(result.deleted_count, 2);
        assert_eq!(result.modified_count, 0);
        assert_eq!(result.orphan_count, 1);
        assert!(result
            .changes
            .iter()
            .any(|c| c.entity_type == "orphan" && c.change_type == ChangeType::Added));
        assert!(result.changes.iter().any(|c| {
            c.entity_type == "orphan"
                && c.change_type == ChangeType::Added
                && c.structural_change == Some(false)
        }));

        let named_bucket_total = result.added_count
            + result.modified_count
            + result.deleted_count
            + result.moved_count
            + result.renamed_count
            + result.reordered_count;
        assert_eq!(named_bucket_total, result.changes.len());
    }

    #[test]
    fn orphan_changes_use_contiguous_line_spans() {
        let file = modified_file(
            "a.rs",
            "use alpha;\nfn foo() {}\nuse beta;\nfn bar() {}\n",
            "use gamma;\nfn foo() {}\nuse delta;\nfn bar() {}\n",
        );
        let entities = vec![entity_span("foo", 2, 2), entity_span("bar", 4, 4)];

        let changes = detect_orphan_changes(&file, &entities, &entities, None, "a.rs", None, None);

        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].start_line, 1);
        assert_eq!(changes[0].end_line, 1);
        assert_eq!(changes[0].old_start_line, Some(1));
        assert_eq!(changes[0].old_end_line, Some(1));
        assert_eq!(changes[0].before_content.as_deref(), Some("use alpha;"));
        assert_eq!(changes[0].after_content.as_deref(), Some("use gamma;"));
        assert_eq!(changes[1].start_line, 3);
        assert_eq!(changes[1].end_line, 3);
        assert_eq!(changes[1].old_start_line, Some(3));
        assert_eq!(changes[1].old_end_line, Some(3));
        assert_eq!(changes[1].before_content.as_deref(), Some("use beta;"));
        assert_eq!(changes[1].after_content.as_deref(), Some("use delta;"));
    }

    #[test]
    fn blank_only_orphan_segments_are_ignored() {
        let file = modified_file("a.rs", "fn foo() {}\n", "\nfn foo() {}\n");
        let before_entities = vec![entity_span("foo", 1, 1)];
        let after_entities = vec![entity_span("foo", 2, 2)];

        let changes = detect_orphan_changes(
            &file,
            &before_entities,
            &after_entities,
            None,
            "a.rs",
            None,
            None,
        );

        assert!(changes.is_empty());
    }

    #[test]
    fn inserted_orphan_segment_does_not_modify_unchanged_later_segment() {
        let file = modified_file(
            "a.rs",
            "fn foo() {}\nuse a;\nfn bar() {}\n",
            "use x;\nfn foo() {}\nuse a;\nfn bar() {}\n",
        );
        let before_entities = vec![entity_span("foo", 1, 1), entity_span("bar", 3, 3)];
        let after_entities = vec![entity_span("foo", 2, 2), entity_span("bar", 4, 4)];

        let changes = detect_orphan_changes(
            &file,
            &before_entities,
            &after_entities,
            None,
            "a.rs",
            None,
            None,
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Added);
        assert_eq!(changes[0].start_line, 1);
        assert_eq!(changes[0].end_line, 1);
        assert!(changes[0].old_start_line.is_none());
        assert_eq!(changes[0].before_content, None);
        assert_eq!(changes[0].after_content.as_deref(), Some("use x;"));
    }

    #[test]
    fn uneven_orphan_gaps_are_not_forced_into_modifications() {
        let file = modified_file(
            "a.rs",
            "use a;\nfn foo() {}\nuse old;\nfn mid() {}\nuse c;\nfn bar() {}\n",
            "use a;\nfn foo() {}\nuse new1;\nfn mid() {}\nuse new2;\nfn baz() {}\nuse c;\nfn bar() {}\n",
        );
        let before_entities = vec![
            entity_span("foo", 2, 2),
            entity_span("mid", 4, 4),
            entity_span("bar", 6, 6),
        ];
        let after_entities = vec![
            entity_span("foo", 2, 2),
            entity_span("mid", 4, 4),
            entity_span("baz", 6, 6),
            entity_span("bar", 8, 8),
        ];

        let changes = detect_orphan_changes(
            &file,
            &before_entities,
            &after_entities,
            None,
            "a.rs",
            None,
            None,
        );

        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].change_type, ChangeType::Deleted);
        assert!(changes[0].entity_id.contains("::deleted@oldL3-3"));
        assert_eq!(changes[0].before_content.as_deref(), Some("use old;"));
        assert_eq!(changes[1].change_type, ChangeType::Added);
        assert_eq!(changes[1].after_content.as_deref(), Some("use new1;"));
        assert_eq!(changes[2].change_type, ChangeType::Added);
        assert_eq!(changes[2].after_content.as_deref(), Some("use new2;"));
    }

    /// Regression: a commented-out key inside an EDN map must not displace the
    /// key/value pairing for the entries that follow the comment.
    ///
    /// In tree-sitter-clojure-orchard, `comment` nodes are *named* children of
    /// `map_lit`. The old code called `named_children()` without filtering, so a
    /// `;` comment consumed one slot and shifted every subsequent key/value pair
    /// by one position. Uncommenting `:published` then produced a spurious rename
    /// (`:spinning-genai → :slug`) for a completely unchanged entry.
    #[test]
    #[cfg(feature = "lang-edn")]
    fn edn_comment_inside_map_does_not_displace_key_value_pairing() {
        let before = r#"{:body [:div]
 ; :published #inst "2025-12-14T14:05:00Z"
 :slug :my-post
 :title "Hello"}"#;

        let after = r#"{:body [:div]
 :published #inst "2025-12-14T14:05:00Z"
 :slug :my-post
 :title "Hello"}"#;

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file("post.edn", before, after)],
            &registry,
            None,
            None,
        );

        let non_orphan: Vec<_> = result
            .changes
            .iter()
            .filter(|c| c.entity_type != "orphan")
            .collect();

        // Only :published should be added; :slug and :title are unchanged
        assert_eq!(
            non_orphan.len(),
            1,
            "expected only :published to be added, got: {:?}",
            non_orphan
                .iter()
                .map(|c| (&c.entity_name, &c.change_type))
                .collect::<Vec<_>>()
        );
        assert_eq!(non_orphan[0].entity_name, ":published");
        assert_eq!(non_orphan[0].change_type, ChangeType::Added);
    }
}
