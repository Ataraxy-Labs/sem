use super::{estimated_output_capacity, orphan_summary_parts, push_line};
use colored::Colorize;
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::{BinaryFileChange, DiffResult};
use std::collections::BTreeMap;

use super::{binary_display_name, file_count, has_reportable_changes};

pub fn format_plain(result: &DiffResult, binary_changes: &[BinaryFileChange]) -> String {
    if !has_reportable_changes(result, binary_changes) {
        return "No semantic changes detected.".to_string();
    }

    let mut output =
        String::with_capacity(estimated_output_capacity(result, binary_changes, false));

    // Group changes by file (BTreeMap for sorted output)
    let mut by_file: BTreeMap<&str, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (i, change) in result.changes.iter().enumerate() {
        by_file.entry(&change.file_path).or_default().0.push(i);
    }
    for (i, change) in binary_changes.iter().enumerate() {
        by_file.entry(&change.file_path).or_default().1.push(i);
    }

    for (file_path, (indices, binary_indices)) in &by_file {
        push_line(&mut output, file_path.bold().to_string());

        for &idx in binary_indices {
            let change = &binary_changes[idx];
            let letter = "B".yellow().to_string();
            let type_label = format!("{:<12}", "file");
            let name_display = binary_display_name(change);
            push_line(
                &mut output,
                format!(
                    "  {}  {}{} {}",
                    letter,
                    type_label.dimmed(),
                    name_display,
                    format!("[binary {}]", change.status).yellow(),
                ),
            );
        }

        for &idx in indices {
            let change = &result.changes[idx];
            let letter = match change.change_type {
                ChangeType::Added => "A".green().to_string(),
                ChangeType::Modified => "M".yellow().to_string(),
                ChangeType::Deleted => "D".red().to_string(),
                ChangeType::Renamed if change.has_content_change() => "RM".cyan().to_string(),
                ChangeType::Renamed => "R".cyan().to_string(),
                ChangeType::Moved if change.has_content_change() => ">M".blue().to_string(),
                ChangeType::Moved => ">".blue().to_string(),
                ChangeType::Reordered if change.has_content_change() => "OM".magenta().to_string(),
                ChangeType::Reordered => "O".magenta().to_string(),
            };

            let type_label = format!("{:<12}", change.entity_type);
            let name_display = if let Some(ref old_name) = change.old_entity_name {
                format!("{old_name} -> {}", change.entity_name)
            } else {
                change.entity_name.clone()
            };
            push_line(
                &mut output,
                format!("  {}  {}{}", letter, type_label.dimmed(), name_display),
            );

            if matches!(change.change_type, ChangeType::Renamed | ChangeType::Moved) {
                if let Some(ref old_path) = change.old_file_path {
                    push_line(
                        &mut output,
                        format!("       {}", format!("from {old_path}").dimmed()),
                    );
                } else if let Some(ref old_parent) = change.old_parent_id {
                    let parent_name = old_parent.rsplit("::").next().unwrap_or(old_parent);
                    push_line(
                        &mut output,
                        format!("       {}", format!("moved from {parent_name}").dimmed()),
                    );
                }
            }
        }

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
            "{} across {} {files_label}{}",
            parts.join(", "),
            reported_file_count,
            orphan_suffix,
        ),
    );

    output
}
