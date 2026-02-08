use colored::Colorize;
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::DiffResult;
use std::collections::BTreeMap;

pub fn format_terminal(result: &DiffResult) -> String {
    if result.changes.is_empty() {
        return "No semantic changes detected.".dimmed().to_string();
    }

    let mut lines: Vec<String> = Vec::new();

    // Group changes by file (BTreeMap for sorted output)
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, change) in result.changes.iter().enumerate() {
        by_file.entry(&change.file_path).or_default().push(i);
    }

    for (file_path, indices) in &by_file {
        let header = format!("─ {file_path} ");
        let pad_len = 55usize.saturating_sub(header.len());
        lines.push(format!("┌{header}{}", "─".repeat(pad_len)).dimmed().to_string());
        lines.push("│".dimmed().to_string());

        for &idx in indices {
            let change = &result.changes[idx];
            let (symbol, tag) = match change.change_type {
                ChangeType::Added => (
                    "⊕".green().to_string(),
                    "[added]".green().to_string(),
                ),
                ChangeType::Modified => {
                    let is_cosmetic = change.structural_change == Some(false);
                    if is_cosmetic {
                        (
                            "~".dimmed().to_string(),
                            "[cosmetic]".dimmed().to_string(),
                        )
                    } else {
                        (
                            "∆".yellow().to_string(),
                            "[modified]".yellow().to_string(),
                        )
                    }
                }
                ChangeType::Deleted => (
                    "⊖".red().to_string(),
                    "[deleted]".red().to_string(),
                ),
                ChangeType::Moved => (
                    "→".blue().to_string(),
                    "[moved]".blue().to_string(),
                ),
                ChangeType::Renamed => (
                    "↻".cyan().to_string(),
                    "[renamed]".cyan().to_string(),
                ),
            };

            let type_label = format!("{:<10}", change.entity_type);
            let name_label = format!("{:<25}", change.entity_name);

            lines.push(format!(
                "{}  {} {} {} {}",
                "│".dimmed(),
                symbol,
                type_label.dimmed(),
                name_label.bold(),
                tag,
            ));

            // Show content diff for modified properties
            if change.change_type == ChangeType::Modified {
                if let (Some(before), Some(after)) =
                    (&change.before_content, &change.after_content)
                {
                    let before_lines: Vec<&str> = before.lines().collect();
                    let after_lines: Vec<&str> = after.lines().collect();

                    if before_lines.len() <= 3 && after_lines.len() <= 3 {
                        for line in &before_lines {
                            lines.push(format!(
                                "{}    {}",
                                "│".dimmed(),
                                format!("- {}", line.trim()).red(),
                            ));
                        }
                        for line in &after_lines {
                            lines.push(format!(
                                "{}    {}",
                                "│".dimmed(),
                                format!("+ {}", line.trim()).green(),
                            ));
                        }
                    }
                }
            }

            // Show rename/move details
            if matches!(
                change.change_type,
                ChangeType::Renamed | ChangeType::Moved
            ) {
                if let Some(ref old_path) = change.old_file_path {
                    lines.push(format!(
                        "{}    {}",
                        "│".dimmed(),
                        format!("from {old_path}").dimmed(),
                    ));
                }
            }
        }

        lines.push("│".dimmed().to_string());
        lines.push(format!("└{}", "─".repeat(55)).dimmed().to_string());
        lines.push(String::new());
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
        parts.push(format!("{} deleted", result.deleted_count).red().to_string());
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

    let files_label = if result.file_count == 1 {
        "file"
    } else {
        "files"
    };

    lines.push(format!(
        "Summary: {} across {} {files_label}",
        parts.join(", "),
        result.file_count,
    ));

    lines.join("\n")
}
