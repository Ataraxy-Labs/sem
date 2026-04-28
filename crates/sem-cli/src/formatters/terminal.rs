use colored::Colorize;
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::DiffResult;
use similar::{ChangeTag, TextDiff};
use std::collections::BTreeMap;

/// Runs word-level diff on two lines and returns (delete_line, insert_line)
/// with changed words highlighted (strikethrough+red / bold+green).
fn render_inline_diff(old_line: &str, new_line: &str) -> (String, String) {
    let diff = TextDiff::from_words(old_line, new_line);
    let mut del = String::new();
    let mut ins = String::new();

    for change in diff.iter_all_changes() {
        let val = change.value();
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

pub fn format_terminal(result: &DiffResult, verbose: bool) -> String {
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
        // Skip files where all changes are orphans in non-verbose mode
        if !verbose
            && indices
                .iter()
                .all(|&i| result.changes[i].entity_type == "orphan")
        {
            continue;
        }

        let header = format!("─ {file_path} ");
        let pad_len = 55usize.saturating_sub(header.len());
        lines.push(format!("┌{header}{}", "─".repeat(pad_len)).dimmed().to_string());
        lines.push("│".dimmed().to_string());

        for &idx in indices {
            let change = &result.changes[idx];

            // Orphan changes (module-level) only shown in verbose mode
            if change.entity_type == "orphan" && !verbose {
                continue;
            }

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
                ChangeType::Reordered => (
                    "↕".magenta().to_string(),
                    "[reordered]".magenta().to_string(),
                ),
            };

            let type_label = format!("{:<10}", change.entity_type);
            let base_name = if let Some(ref old_name) = change.old_entity_name {
                format!("{old_name} -> {}", change.entity_name)
            } else {
                change.entity_name.clone()
            };
            let display_name = match &change.parent_name {
                Some(p) => format!("{p}::{base_name}"),
                None => base_name,
            };
            let truncated = if display_name.len() > 25 {
                format!("{}…", display_name.char_indices().nth(24).map(|(i, _)| &display_name[..i]).unwrap_or(&display_name))
            } else {
                display_name
            };
            let name_label = format!("{:<25}", truncated);

            lines.push(format!(
                "{}  {} {} {} {}",
                "│".dimmed(),
                symbol,
                type_label.dimmed(),
                name_label.bold(),
                tag,
            ));

            // Show content diff
            if verbose {
                match change.change_type {
                    ChangeType::Added => {
                        if let Some(ref content) = change.after_content {
                            for line in content.lines() {
                                lines.push(format!(
                                    "{}    {}",
                                    "│".dimmed(),
                                    format!("+ {line}").green(),
                                ));
                            }
                        }
                    }
                    ChangeType::Deleted => {
                        if let Some(ref content) = change.before_content {
                            for line in content.lines() {
                                lines.push(format!(
                                    "{}    {}",
                                    "│".dimmed(),
                                    format!("- {line}").red(),
                                ));
                            }
                        }
                    }
                    ChangeType::Modified | ChangeType::Renamed | ChangeType::Moved => {
                        if let (Some(before), Some(after)) =
                            (&change.before_content, &change.after_content)
                        {
                            let diff = TextDiff::from_lines(before.as_str(), after.as_str());
                            for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
                                lines.push(format!(
                                    "{}    {}",
                                    "│".dimmed(),
                                    format!("{}", hunk.header()).dimmed(),
                                ));
                                for op in hunk.ops() {
                                    let mut deletes: Vec<String> = Vec::new();
                                    let mut inserts: Vec<String> = Vec::new();

                                    for diff_change in diff.iter_changes(op) {
                                        let line = diff_change.value().trim_end_matches('\n');
                                        match diff_change.tag() {
                                            ChangeTag::Delete => deletes.push(line.to_string()),
                                            ChangeTag::Insert => inserts.push(line.to_string()),
                                            ChangeTag::Equal => {
                                                lines.push(format!(
                                                    "{}    {}",
                                                    "│".dimmed(),
                                                    format!("  {line}").dimmed(),
                                                ));
                                            }
                                        }
                                    }

                                    let paired = deletes.len().min(inserts.len());
                                    for i in 0..paired {
                                        let (del, ins) =
                                            render_inline_diff(&deletes[i], &inserts[i]);
                                        lines.push(format!(
                                            "{}    {} {}",
                                            "│".dimmed(),
                                            "-".red(),
                                            del,
                                        ));
                                        lines.push(format!(
                                            "{}    {} {}",
                                            "│".dimmed(),
                                            "+".green(),
                                            ins,
                                        ));
                                    }
                                    for d in &deletes[paired..] {
                                        lines.push(format!(
                                            "{}    {}",
                                            "│".dimmed(),
                                            format!("- {d}").red(),
                                        ));
                                    }
                                    for i in &inserts[paired..] {
                                        lines.push(format!(
                                            "{}    {}",
                                            "│".dimmed(),
                                            format!("+ {i}").green(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            } else if change.change_type == ChangeType::Modified {
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
                } else if let Some(ref old_parent) = change.old_parent_id {
                    // Intra-file move: extract parent name from entity ID
                    let parent_name = old_parent.rsplit("::").next().unwrap_or(old_parent);
                    lines.push(format!(
                        "{}    {}",
                        "│".dimmed(),
                        format!("moved from {parent_name}").dimmed(),
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
    if result.reordered_count > 0 {
        parts.push(
            format!("{} reordered", result.reordered_count)
                .magenta()
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
