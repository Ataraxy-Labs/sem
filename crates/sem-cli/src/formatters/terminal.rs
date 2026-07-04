use super::{estimated_output_capacity, orphan_summary_parts, push_line};
use colored::Colorize;
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::{BinaryFileChange, DiffResult};
use similar::{ChangeTag, TextDiff};
use std::collections::BTreeMap;

use super::{binary_display_name, file_count, has_reportable_changes};

fn sanitize_terminal_text(input: &str) -> String {
    if !input.chars().any(char::is_control) {
        return input.to_string();
    }

    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_control() {
            output.extend(ch.escape_debug());
        } else {
            output.push(ch);
        }
    }
    output
}

/// Runs word-level diff on two lines and returns (delete_line, insert_line)
/// with changed words highlighted (strikethrough+red / bold+green).
fn render_inline_diff(old_line: &str, new_line: &str) -> (String, String) {
    let diff = TextDiff::from_words(old_line, new_line);
    let mut del = String::new();
    let mut ins = String::new();

    for change in diff.iter_all_changes() {
        let val = sanitize_terminal_text(change.value());
        match change.tag() {
            ChangeTag::Equal => {
                del.push_str(&val.dimmed().to_string());
                ins.push_str(&val.dimmed().to_string());
            }
            ChangeTag::Delete => {
                del.push_str(&val.red().strikethrough().bold().to_string());
            }
            ChangeTag::Insert => {
                ins.push_str(&val.green().bold().to_string());
            }
        }
    }

    (del, ins)
}

/// Default inner width of the per-file box (number of `─` after the corner).
const DEFAULT_BOX_WIDTH: usize = 55;

/// Resolve the box width. `SEM_WIDTH` overrides the default, which matters when
/// sem runs without a TTY (e.g. as a `lazygit` pager) where it otherwise falls
/// back to a fixed width that may not match the surrounding pane. The value is
/// the total box width in columns; the inner dash budget is one less (the
/// corner glyph). Clamped to a sane minimum.
fn resolve_box_width(sem_width: Option<&str>) -> usize {
    match sem_width.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(w) => w.saturating_sub(1).max(8),
        None => DEFAULT_BOX_WIDTH,
    }
}

fn box_width() -> usize {
    resolve_box_width(std::env::var("SEM_WIDTH").ok().as_deref())
}

pub fn format_terminal(
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
    verbose: bool,
) -> String {
    if !has_reportable_changes(result, binary_changes) {
        return "No semantic changes detected.".dimmed().to_string();
    }

    let box_width = box_width();

    let mut output =
        String::with_capacity(estimated_output_capacity(result, binary_changes, verbose));

    // Group changes by file (BTreeMap for sorted output)
    let mut by_file: BTreeMap<&str, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (i, change) in result.changes.iter().enumerate() {
        by_file.entry(&change.file_path).or_default().0.push(i);
    }
    for (i, change) in binary_changes.iter().enumerate() {
        by_file.entry(&change.file_path).or_default().1.push(i);
    }

    for (file_path, (indices, binary_indices)) in &by_file {
        // Skip files where all changes are orphans in non-verbose mode
        if !verbose
            && binary_indices.is_empty()
            && indices
                .iter()
                .all(|&i| result.changes[i].entity_type == "orphan")
        {
            continue;
        }

        let header = format!("─ {} ", sanitize_terminal_text(file_path));
        let pad_len = box_width.saturating_sub(header.len());
        push_line(
            &mut output,
            format!("┌{header}{}", "─".repeat(pad_len))
                .dimmed()
                .to_string(),
        );
        push_line(&mut output, "│".dimmed().to_string());

        for &idx in binary_indices {
            let change = &binary_changes[idx];
            let symbol = "■".yellow().to_string();
            let tag = format!("[binary {}]", change.status).yellow().to_string();
            let type_label = format!("{:<10}", "file");
            let name_label = format!("{:<25}", binary_display_name(change));

            push_line(
                &mut output,
                format!(
                    "{}  {} {} {} {}",
                    "│".dimmed(),
                    symbol,
                    type_label.dimmed(),
                    name_label.bold(),
                    tag,
                ),
            );
        }

        for &idx in indices {
            let change = &result.changes[idx];

            // Orphan changes (module-level) only shown in verbose mode
            if change.entity_type == "orphan" && !verbose {
                continue;
            }

            let content_suffix = if change.has_content_change() {
                if change.structural_change == Some(false) {
                    "+cosmetic"
                } else {
                    "+modified"
                }
            } else {
                ""
            };
            let (symbol, tag) = match change.change_type {
                ChangeType::Added => ("⊕".green().to_string(), "[added]".green().to_string()),
                ChangeType::Modified => {
                    let is_cosmetic = change.structural_change == Some(false);
                    if is_cosmetic {
                        ("~".dimmed().to_string(), "[cosmetic]".dimmed().to_string())
                    } else {
                        ("∆".yellow().to_string(), "[modified]".yellow().to_string())
                    }
                }
                ChangeType::Deleted => ("⊖".red().to_string(), "[deleted]".red().to_string()),
                ChangeType::Moved => (
                    "→".blue().to_string(),
                    format!("[moved{content_suffix}]").blue().to_string(),
                ),
                ChangeType::Renamed => (
                    "↻".cyan().to_string(),
                    format!("[renamed{content_suffix}]").cyan().to_string(),
                ),
                ChangeType::Reordered => (
                    "↕".magenta().to_string(),
                    format!("[reordered{content_suffix}]").magenta().to_string(),
                ),
            };

            let type_label = format!("{:<10}", sanitize_terminal_text(&change.entity_type));
            let base_name = if let Some(ref old_name) = change.old_entity_name {
                format!(
                    "{} -> {}",
                    sanitize_terminal_text(old_name),
                    sanitize_terminal_text(&change.entity_name)
                )
            } else {
                sanitize_terminal_text(&change.entity_name)
            };
            let display_name = match &change.parent_name {
                Some(p) => format!("{}::{base_name}", sanitize_terminal_text(p)),
                None => base_name,
            };
            // Optionally make the entity name a clickable link to its
            // definition (file:line). Pad on the visible text so the OSC8
            // escape (zero-width) doesn't break column alignment.
            let name_label = if crate::hyperlinks::enabled() {
                let pad = 25usize.saturating_sub(display_name.chars().count());
                let linked =
                    crate::hyperlinks::link(&display_name, &change.file_path, change.start_line);
                format!("{linked}{}", " ".repeat(pad))
            } else {
                format!("{:<25}", display_name)
            };

            push_line(
                &mut output,
                format!(
                    "{}  {} {} {} {}",
                    "│".dimmed(),
                    symbol,
                    type_label.dimmed(),
                    name_label.bold(),
                    tag,
                ),
            );

            // Show content diff
            if verbose {
                match change.change_type {
                    ChangeType::Added => {
                        if let Some(ref content) = change.after_content {
                            for line in content.lines() {
                                let line = sanitize_terminal_text(line);
                                push_line(
                                    &mut output,
                                    format!("{}    {}", "│".dimmed(), format!("+ {line}").green()),
                                );
                            }
                        }
                    }
                    ChangeType::Deleted => {
                        if let Some(ref content) = change.before_content {
                            for line in content.lines() {
                                let line = sanitize_terminal_text(line);
                                push_line(
                                    &mut output,
                                    format!("{}    {}", "│".dimmed(), format!("- {line}").red()),
                                );
                            }
                        }
                    }
                    ChangeType::Modified | ChangeType::Renamed | ChangeType::Moved => {
                        if let (Some(before), Some(after)) =
                            (&change.before_content, &change.after_content)
                        {
                            let diff = TextDiff::from_lines(before.as_str(), after.as_str());
                            for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
                                push_line(
                                    &mut output,
                                    format!(
                                        "{}    {}",
                                        "│".dimmed(),
                                        format!("{}", hunk.header()).dimmed(),
                                    ),
                                );
                                for op in hunk.ops() {
                                    let mut deletes: Vec<String> = Vec::new();
                                    let mut inserts: Vec<String> = Vec::new();

                                    for diff_change in diff.iter_changes(op) {
                                        let line = sanitize_terminal_text(
                                            diff_change.value().trim_end_matches('\n'),
                                        );
                                        match diff_change.tag() {
                                            ChangeTag::Delete => deletes.push(line),
                                            ChangeTag::Insert => inserts.push(line),
                                            ChangeTag::Equal => {
                                                push_line(
                                                    &mut output,
                                                    format!(
                                                        "{}    {}",
                                                        "│".dimmed(),
                                                        format!("  {line}").dimmed(),
                                                    ),
                                                );
                                            }
                                        }
                                    }

                                    let paired = deletes.len().min(inserts.len());
                                    for i in 0..paired {
                                        let (del, ins) =
                                            render_inline_diff(&deletes[i], &inserts[i]);
                                        push_line(
                                            &mut output,
                                            format!("{}    {} {}", "│".dimmed(), "-".red(), del),
                                        );
                                        push_line(
                                            &mut output,
                                            format!("{}    {} {}", "│".dimmed(), "+".green(), ins),
                                        );
                                    }
                                    for d in &deletes[paired..] {
                                        push_line(
                                            &mut output,
                                            format!(
                                                "{}    {}",
                                                "│".dimmed(),
                                                format!("- {d}").red()
                                            ),
                                        );
                                    }
                                    for i in &inserts[paired..] {
                                        push_line(
                                            &mut output,
                                            format!(
                                                "{}    {}",
                                                "│".dimmed(),
                                                format!("+ {i}").green()
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            } else if change.change_type == ChangeType::Modified {
                if let (Some(before), Some(after)) = (&change.before_content, &change.after_content)
                {
                    let before_line_count = before.lines().count();
                    let after_line_count = after.lines().count();

                    if before_line_count <= 3 && after_line_count <= 3 {
                        for line in before.lines() {
                            let line = sanitize_terminal_text(line.trim());
                            push_line(
                                &mut output,
                                format!("{}    {}", "│".dimmed(), format!("- {line}").red()),
                            );
                        }
                        for line in after.lines() {
                            let line = sanitize_terminal_text(line.trim());
                            push_line(
                                &mut output,
                                format!("{}    {}", "│".dimmed(), format!("+ {line}").green()),
                            );
                        }
                    }
                }
            }

            // Show rename/move details
            if matches!(change.change_type, ChangeType::Renamed | ChangeType::Moved) {
                if let Some(ref old_path) = change.old_file_path {
                    push_line(
                        &mut output,
                        format!(
                            "{}    {}",
                            "│".dimmed(),
                            format!("from {}", sanitize_terminal_text(old_path)).dimmed(),
                        ),
                    );
                } else if let Some(ref old_parent) = change.old_parent_id {
                    // Intra-file move: extract parent name from entity ID
                    let parent_name = old_parent.rsplit("::").next().unwrap_or(old_parent);
                    push_line(
                        &mut output,
                        format!(
                            "{}    {}",
                            "│".dimmed(),
                            format!("moved from {}", sanitize_terminal_text(parent_name)).dimmed(),
                        ),
                    );
                }
            }
        }

        push_line(&mut output, "│".dimmed().to_string());
        push_line(
            &mut output,
            format!("└{}", "─".repeat(box_width)).dimmed().to_string(),
        );
        push_line(&mut output, "");
    }

    // Summary
    let mut parts: Vec<String> = Vec::new();
    if result.added_count > 0 {
        parts.push(format!("{} added", result.added_count).green().to_string());
    }
    if result.modified_count > 0 {
        parts.push(
            format!("{} modified", result.modified_count)
                .yellow()
                .to_string(),
        );
    }
    if result.deleted_count > 0 {
        parts.push(
            format!("{} deleted", result.deleted_count)
                .red()
                .to_string(),
        );
    }
    if result.moved_count > 0 {
        parts.push(format!("{} moved", result.moved_count).blue().to_string());
    }
    if result.renamed_count > 0 {
        parts.push(
            format!("{} renamed", result.renamed_count)
                .cyan()
                .to_string(),
        );
    }
    if result.reordered_count > 0 {
        parts.push(
            format!("{} reordered", result.reordered_count)
                .magenta()
                .to_string(),
        );
    }
    if !binary_changes.is_empty() {
        parts.push(
            format!("{} binary", binary_changes.len())
                .yellow()
                .to_string(),
        );
    }

    let reported_file_count = file_count(result, binary_changes);
    let files_label = if reported_file_count == 1 {
        "file"
    } else {
        "files"
    };
    let orphan_parts = orphan_summary_parts(result);
    let orphan_suffix = if orphan_parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", orphan_parts.join(", "))
            .dimmed()
            .to_string()
    };

    push_line(
        &mut output,
        format!(
            "Summary: {} across {} {files_label}{}",
            parts.join(", "),
            reported_file_count,
            orphan_suffix,
        ),
    );

    // Show noise-filtered line when entities were analyzed
    let entities_analyzed = result
        .total_entities_before
        .max(result.total_entities_after);
    let changes_detected = result.added_count
        + result.modified_count
        + result.deleted_count
        + result.moved_count
        + result.renamed_count
        + result.reordered_count
        + binary_changes.len();
    if entities_analyzed > changes_detected {
        let noise = entities_analyzed - changes_detected;
        push_line(
            &mut output,
            format!(
                "Analyzed {} entities, {} unchanged filtered out",
                entities_analyzed, noise
            )
            .dimmed()
            .to_string(),
        );
    }

    // Warn if fallback chunking was used (unsupported file extension)
    let chunk_files: Vec<String> = result
        .changes
        .iter()
        .filter(|c| c.entity_type == "chunk")
        .map(|c| sanitize_terminal_text(&c.file_path))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !chunk_files.is_empty() {
        push_line(&mut output, "");
        push_line(
            &mut output,
            format!(
                "Warning: {} used line-based chunking (unsupported file extension).",
                chunk_files.join(", ")
            )
            .yellow()
            .to_string(),
        );
        push_line(
            &mut output,
            "If this language should be supported, open an issue: https://github.com/Ataraxy-Labs/sem/issues"
                .dimmed()
                .to_string(),
        );
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::model::change::SemanticChange;

    #[test]
    fn box_width_resolves_from_sem_width() {
        assert_eq!(resolve_box_width(None), DEFAULT_BOX_WIDTH);
        assert_eq!(resolve_box_width(Some("80")), 79); // total width minus the corner glyph
        assert_eq!(resolve_box_width(Some("  120 ")), 119);
        assert_eq!(resolve_box_width(Some("garbage")), DEFAULT_BOX_WIDTH);
        assert_eq!(resolve_box_width(Some("3")), 8); // clamped to a sane minimum
    }

    #[test]
    fn terminal_source_content_escapes_control_characters() {
        let output = sanitize_terminal_text("\u{1b}[31mRED\u{1b}[0m\t\r\n");

        assert!(!output.contains('\u{1b}'));
        assert!(output.contains("\\u{1b}[31mRED\\u{1b}[0m"));
        assert!(output.contains("\\t\\r\\n"));
    }

    #[test]
    fn terminal_chunk_warning_escapes_file_path_control_characters() {
        colored::control::set_override(false);
        let change: SemanticChange = serde_json::from_value(serde_json::json!({
            "id": "change::bad.txt::chunk::1",
            "entityId": "bad.txt::chunk::1",
            "changeType": "modified",
            "entityType": "chunk",
            "entityName": "chunk 1",
            "entityLine": 1,
            "startLine": 1,
            "endLine": 2,
            "filePath": "bad\u{1b}[31m.txt",
            "beforeContent": "alpha\nbeta\n",
            "afterContent": "alpha\nchanged\n",
            "structuralChange": true
        }))
        .unwrap();
        let result = DiffResult {
            changes: vec![change],
            file_count: 1,
            added_count: 0,
            modified_count: 1,
            deleted_count: 0,
            moved_count: 0,
            renamed_count: 0,
            reordered_count: 0,
            orphan_count: 0,
            total_entities_before: 1,
            total_entities_after: 1,
        };

        let output = format_terminal(&result, &[], true);

        assert!(!output.contains('\u{1b}'), "{output}");
        assert!(output.contains("bad\\u{1b}[31m.txt"), "{output}");
        assert!(
            output.contains("used line-based chunking (unsupported file extension)"),
            "{output}"
        );
    }
}
