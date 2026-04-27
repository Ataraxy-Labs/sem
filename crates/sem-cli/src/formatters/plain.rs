use colored::Colorize;
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::DiffResult;
use std::collections::BTreeMap;

pub fn format_plain(result: &DiffResult) -> String {
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
        lines.push(file_path.bold().to_string());

        for &idx in indices {
            let change = &result.changes[idx];
            let letter = match change.change_type {
                ChangeType::Added => "A".green().to_string(),
                ChangeType::Modified => "M".yellow().to_string(),
                ChangeType::Deleted => "D".red().to_string(),
                ChangeType::Renamed => "R".cyan().to_string(),
                ChangeType::Moved => ">".blue().to_string(),
                ChangeType::Reordered => "O".magenta().to_string(),
            };

            let type_label = format!("{:<12}", change.entity_type);
            let name_display = if let Some(ref old_name) = change.old_entity_name {
                format!("{old_name} -> {}", change.entity_name)
            } else {
                change.entity_name.clone()
            };
            lines.push(format!(
                "  {}  {}{}",
                letter,
                type_label.dimmed(),
                name_display,
            ));

            if matches!(change.change_type, ChangeType::Renamed | ChangeType::Moved) {
                if let Some(ref old_path) = change.old_file_path {
                    lines.push(format!("       {}", format!("from {old_path}").dimmed()));
                } else if let Some(ref old_parent) = change.old_parent_id {
                    let parent_name = old_parent.rsplit("::").next().unwrap_or(old_parent);
                    lines.push(format!("       {}", format!("moved from {parent_name}").dimmed()));
                }
            }
        }

        lines.push(String::new());
    }

    // Summary
    let mut parts: Vec<String> = Vec::new();
    if result.added_count > 0 {
        parts.push(format!("{} added", result.added_count).green().to_string());
    }
    if result.modified_count > 0 {
        parts.push(format!("{} modified", result.modified_count).yellow().to_string());
    }
    if result.deleted_count > 0 {
        parts.push(format!("{} deleted", result.deleted_count).red().to_string());
    }
    if result.moved_count > 0 {
        parts.push(format!("{} moved", result.moved_count).blue().to_string());
    }
    if result.renamed_count > 0 {
        parts.push(format!("{} renamed", result.renamed_count).cyan().to_string());
    }
    if result.reordered_count > 0 {
        parts.push(format!("{} reordered", result.reordered_count).magenta().to_string());
    }

    let files_label = if result.file_count == 1 { "file" } else { "files" };
    lines.push(format!(
        "{} across {} {files_label}",
        parts.join(", "),
        result.file_count,
    ));

    lines.join("\n")
}
