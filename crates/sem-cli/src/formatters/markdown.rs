use super::{estimated_output_capacity, orphan_summary_parts, push_line};
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::{BinaryFileChange, DiffResult};
use similar::{ChangeTag, TextDiff};
use std::collections::BTreeMap;

use super::{binary_display_name, file_count, has_reportable_changes};

fn longest_backtick_run(input: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;

    for ch in input.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }

    longest
}

fn push_diff_block(output: &mut String, diff_lines: Vec<String>) {
    let fence_len = diff_lines
        .iter()
        .map(|line| longest_backtick_run(line))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(3);
    let fence = "`".repeat(fence_len);

    push_line(output, format!("{fence}diff"));
    for line in diff_lines {
        push_line(output, line);
    }
    push_line(output, fence);
}

pub fn format_markdown(
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
    verbose: bool,
) -> String {
    if !has_reportable_changes(result, binary_changes) {
        return "No semantic changes detected.".to_string();
    }

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
        push_line(&mut output, format!("### {file_path}"));
        push_line(&mut output, "");
        push_line(&mut output, "| Status | Type | Name |");
        push_line(&mut output, "|--------|------|------|");

        let mut post_table = String::new();

        for &idx in binary_indices {
            let change = &binary_changes[idx];
            push_line(
                &mut output,
                format!(
                    "| B | file | {} `[binary {}]` |",
                    binary_display_name(change),
                    change.status,
                ),
            );
        }

        for &idx in indices {
            let change = &result.changes[idx];
            let status = match change.change_type {
                ChangeType::Added => "+",
                ChangeType::Deleted => "-",
                ChangeType::Modified => {
                    if change.structural_change == Some(false) {
                        "~"
                    } else {
                        "Δ"
                    }
                }
                ChangeType::Moved if change.has_content_change() => "→ Δ",
                ChangeType::Moved => "→",
                ChangeType::Renamed if change.has_content_change() => "↻ Δ",
                ChangeType::Renamed => "↻",
                ChangeType::Reordered if change.has_content_change() => "↕ Δ",
                ChangeType::Reordered => "↕",
            };

            let name_display = if let Some(ref old_name) = change.old_entity_name {
                format!("{old_name} -> {}", change.entity_name)
            } else {
                change.entity_name.clone()
            };
            push_line(
                &mut output,
                format!("| {} | {} | {} |", status, change.entity_type, name_display),
            );

            // Show content diff
            if verbose {
                match change.change_type {
                    ChangeType::Added => {
                        if let Some(ref content) = change.after_content {
                            push_line(&mut post_table, "");
                            push_line(&mut post_table, format!("**`{}`**", change.entity_name));
                            push_diff_block(
                                &mut post_table,
                                content.lines().map(|line| format!("+ {line}")).collect(),
                            );
                        }
                    }
                    ChangeType::Deleted => {
                        if let Some(ref content) = change.before_content {
                            push_line(&mut post_table, "");
                            push_line(&mut post_table, format!("**`{}`**", change.entity_name));
                            push_diff_block(
                                &mut post_table,
                                content.lines().map(|line| format!("- {line}")).collect(),
                            );
                        }
                    }
                    ChangeType::Modified | ChangeType::Moved | ChangeType::Renamed => {
                        if let (Some(before), Some(after)) =
                            (&change.before_content, &change.after_content)
                        {
                            push_line(&mut post_table, "");
                            push_line(&mut post_table, format!("**`{}`**", change.entity_name));
                            let mut diff_lines = Vec::new();
                            let diff = TextDiff::from_lines(before.as_str(), after.as_str());
                            for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
                                diff_lines.push(hunk.header().to_string());
                                for op in hunk.ops() {
                                    let mut deletes: Vec<String> = Vec::new();
                                    let mut inserts: Vec<String> = Vec::new();

                                    for diff_change in diff.iter_changes(op) {
                                        let line = diff_change.value().trim_end_matches('\n');
                                        match diff_change.tag() {
                                            ChangeTag::Delete => deletes.push(line.to_string()),
                                            ChangeTag::Insert => inserts.push(line.to_string()),
                                            ChangeTag::Equal => {
                                                diff_lines.push(format!("  {line}"))
                                            }
                                        }
                                    }

                                    let paired = deletes.len().min(inserts.len());
                                    for i in 0..paired {
                                        diff_lines.push(format!("- {}", deletes[i]));
                                        diff_lines.push(format!("+ {}", inserts[i]));
                                    }
                                    for d in &deletes[paired..] {
                                        diff_lines.push(format!("- {d}"));
                                    }
                                    for i in &inserts[paired..] {
                                        diff_lines.push(format!("+ {i}"));
                                    }
                                }
                            }
                            push_diff_block(&mut post_table, diff_lines);
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
                        push_line(&mut post_table, "");
                        push_line(&mut post_table, format!("**`{}`**", change.entity_name));
                        let diff_lines = before
                            .lines()
                            .map(|line| format!("- {}", line.trim()))
                            .chain(after.lines().map(|line| format!("+ {}", line.trim())))
                            .collect();
                        push_diff_block(&mut post_table, diff_lines);
                    }
                }
            }

            // Show rename/move details
            if matches!(change.change_type, ChangeType::Renamed | ChangeType::Moved) {
                if let Some(ref old_path) = change.old_file_path {
                    push_line(&mut post_table, "");
                    push_line(&mut post_table, format!("> from {old_path}"));
                } else if let Some(ref old_parent) = change.old_parent_id {
                    let parent_name = old_parent.rsplit("::").next().unwrap_or(old_parent);
                    push_line(&mut post_table, "");
                    push_line(&mut post_table, format!("> moved from {parent_name}"));
                }
            }
        }

        if !post_table.is_empty() {
            push_line(&mut output, "");
            push_line(&mut output, post_table);
        }
        push_line(&mut output, "");
    }

    // Summary
    let mut parts: Vec<String> = Vec::new();
    if result.added_count > 0 {
        parts.push(format!("{} added", result.added_count));
    }
    if result.modified_count > 0 {
        parts.push(format!("{} modified", result.modified_count));
    }
    if result.deleted_count > 0 {
        parts.push(format!("{} deleted", result.deleted_count));
    }
    if result.moved_count > 0 {
        parts.push(format!("{} moved", result.moved_count));
    }
    if result.renamed_count > 0 {
        parts.push(format!("{} renamed", result.renamed_count));
    }
    if result.reordered_count > 0 {
        parts.push(format!("{} reordered", result.reordered_count));
    }
    if !binary_changes.is_empty() {
        parts.push(format!("{} binary", binary_changes.len()));
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
    };

    push_line(
        &mut output,
        format!(
            "**Summary:** {} across {} {files_label}{}",
            parts.join(", "),
            reported_file_count,
            orphan_suffix,
        ),
    );

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::model::change::SemanticChange;

    fn diff_result(change: SemanticChange) -> DiffResult {
        DiffResult {
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
        }
    }

    #[test]
    fn markdown_diff_fence_is_longer_than_embedded_backticks() {
        let change: SemanticChange = serde_json::from_value(serde_json::json!({
            "id": "change::app.py::function::foo",
            "entityId": "app.py::function::foo",
            "changeType": "modified",
            "entityType": "function",
            "entityName": "foo",
            "entityLine": 1,
            "filePath": "app.py",
            "beforeContent": "def foo():\n    return \"plain\"\n",
            "afterContent": "def foo():\n    return \"````\"\n",
            "structuralChange": true
        }))
        .unwrap();

        let output = format_markdown(&diff_result(change), &[], true);
        let lines: Vec<&str> = output.lines().collect();
        let opening = lines
            .iter()
            .position(|line| *line == "`````diff")
            .expect("diff block should open with a 5-backtick fence");
        let closing = lines[opening + 1..]
            .iter()
            .position(|line| *line == "`````")
            .map(|offset| opening + 1 + offset)
            .expect("diff block should close with a matching 5-backtick fence");

        assert!(lines[opening + 1..closing]
            .iter()
            .any(|line| line.contains("````")));
        assert!(!lines.iter().any(|line| *line == "````diff"));
    }
}
