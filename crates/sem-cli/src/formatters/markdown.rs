use sem_core::model::change::ChangeType;
use sem_core::parser::differ::DiffResult;
use std::collections::BTreeMap;

pub fn format_markdown(result: &DiffResult) -> String {
    if result.changes.is_empty() {
        return "No semantic changes detected.".to_string();
    }

    let mut lines: Vec<String> = Vec::new();

    // Group changes by file (BTreeMap for sorted output)
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, change) in result.changes.iter().enumerate() {
        by_file.entry(&change.file_path).or_default().push(i);
    }

    for (file_path, indices) in &by_file {
        lines.push(format!("### {file_path}"));
        lines.push(String::new());
        lines.push("| Status | Type | Name |".to_string());
        lines.push("|--------|------|------|".to_string());

        let mut post_table: Vec<String> = Vec::new();

        for &idx in indices {
            let change = &result.changes[idx];
            let status = match change.change_type {
                ChangeType::Added => "+",
                ChangeType::Modified => {
                    if change.structural_change == Some(false) {
                        "~"
                    } else {
                        "Δ"
                    }
                }
                ChangeType::Modified => {
                    if change.structural_change == Some(false) {
                        "~"
                    } else {
                        "Δ"
                    }
                }
                ChangeType::Moved => "→",
                ChangeType::Renamed => "↻",
            };

            lines.push(format!(
                "| {} | {} | {} |",
                status, change.entity_type, change.entity_name
            ));

            // Show content diff for modified entities with short before/after
            if change.change_type == ChangeType::Modified {
                if let (Some(before), Some(after)) =
                    (&change.before_content, &change.after_content)
                {
                    let before_lines: Vec<&str> = before.lines().collect();
                    let after_lines: Vec<&str> = after.lines().collect();

                    if before_lines.len() <= 3 && after_lines.len() <= 3 {
                        post_table.push(String::new());
                        post_table.push("```diff".to_string());
                        for line in &before_lines {
                            post_table.push(format!("- {}", line.trim()));
                        }
                        for line in &after_lines {
                        post_table.push(String::new());
                        post_table.push(format!("**`{}`**", change.entity_name));
                        post_table.push("```diff".to_string());
                        }
                        post_table.push("```".to_string());
                    }
                }
            }

            // Show rename/move details
            if matches!(
                change.change_type,
                ChangeType::Renamed | ChangeType::Moved
            ) {
                if let Some(ref old_path) = change.old_file_path {
                    post_table.push(String::new());
                    post_table.push(format!("> from {old_path}"));
                }
            }
        }

        lines.extend(post_table);
        lines.push(String::new());
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

    let files_label = if result.file_count == 1 {
        "file"
    } else {
        "files"
    };

    lines.push(format!(
        "**Summary:** {} across {} {files_label}",
        parts.join(", "),
        result.file_count,
    ));

    lines.join("\n")
}
