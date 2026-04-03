use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::parser::plugins::create_default_registry;

use super::truncate_str;

pub struct LogOptions {
    pub cwd: String,
    pub entity_name: String,
    pub file_path: Option<String>,
    pub limit: usize,
    pub json: bool,
    pub verbose: bool,
}

#[derive(Debug)]
enum EntityChangeType {
    Added,
    ModifiedLogic,
    ModifiedCosmetic,
    Deleted,
}

impl EntityChangeType {
    fn label(&self) -> &str {
        match self {
            EntityChangeType::Added => "added",
            EntityChangeType::ModifiedLogic => "modified (logic)",
            EntityChangeType::ModifiedCosmetic => "modified (cosmetic)",
            EntityChangeType::Deleted => "deleted",
        }
    }

    fn label_colored(&self) -> colored::ColoredString {
        match self {
            EntityChangeType::Added => "added".green(),
            EntityChangeType::ModifiedLogic => "modified (logic)".yellow(),
            EntityChangeType::ModifiedCosmetic => "modified (cosmetic)".dimmed(),
            EntityChangeType::Deleted => "deleted".red(),
        }
    }
}

struct LogEntry {
    short_sha: String,
    author: String,
    date: String,
    message: String,
    change_type: EntityChangeType,
    content: Option<String>,
    prev_content: Option<String>,
}

pub fn log_command(opts: LogOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    let bridge = match GitBridge::open(root) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{} {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
    };

    // Resolve file path: use provided or auto-detect
    let file_path = match opts.file_path {
        Some(fp) => fp,
        None => match find_entity_file(root, &registry, &opts.entity_name) {
            FindResult::Found(fp) => fp,
            FindResult::Ambiguous(files) => {
                eprintln!(
                    "{} Entity '{}' found in multiple files:",
                    "error:".red().bold(),
                    opts.entity_name
                );
                for f in &files {
                    eprintln!("  {}", f);
                }
                eprintln!("\nUse --file to disambiguate.");
                std::process::exit(1);
            }
            FindResult::NotFound => {
                eprintln!(
                    "{} Entity '{}' not found in any file",
                    "error:".red().bold(),
                    opts.entity_name
                );
                std::process::exit(1);
            }
        },
    };

    // Convert file_path to be relative to git repo root (for git operations)
    let repo_root = bridge.repo_root();
    let abs_cwd = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let abs_repo = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let git_file_path = if abs_cwd != abs_repo {
        // cwd is a subdirectory of repo root, prepend the prefix
        let prefix = abs_cwd.strip_prefix(&abs_repo).unwrap_or(Path::new(""));
        prefix.join(&file_path).to_string_lossy().to_string()
    } else {
        file_path.clone()
    };

    // Verify the file has a parser
    let plugin = match registry.get_plugin(&file_path) {
        Some(p) => p,
        None => {
            eprintln!(
                "{} Unsupported file type: {}",
                "error:".red().bold(),
                file_path
            );
            std::process::exit(1);
        }
    };

    // Get commits that touched this file
    let commits = match bridge.get_file_commits(&git_file_path, opts.limit) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} Failed to get file history: {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
    };

    if commits.is_empty() {
        eprintln!("{} No commits found for {}", "warning:".yellow().bold(), git_file_path);
        return;
    }

    // Walk commits oldest-first so we can track evolution, then reverse for display
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut prev_entity_content: Option<String> = None;
    let mut prev_structural_hash: Option<String> = None;
    let mut entity_type = String::new();
    let mut found_at_least_once = false;

    // Process oldest to newest
    for commit in commits.iter().rev() {
        let content = match bridge.read_file_at_ref(&commit.sha, &git_file_path) {
            Ok(Some(c)) => c,
            _ => {
                // File doesn't exist at this commit (deleted)
                if prev_entity_content.is_some() {
                    let date = chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0));
                    let msg_first_line = commit.message.lines().next().unwrap_or("").to_string();
                    entries.push(LogEntry {
                        short_sha: commit.short_sha.clone(),
                        author: commit.author.clone(),
                        date,
                        message: msg_first_line,
                        change_type: EntityChangeType::Deleted,
                        content: None,
                        prev_content: prev_entity_content.take(),
                    });
                    prev_structural_hash = None;
                }
                continue;
            }
        };

        let entities = plugin.extract_entities(&content, &file_path);
        let entity = entities.iter().find(|e| e.name == opts.entity_name);

        let date = chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0));
        let msg_first_line = commit.message.lines().next().unwrap_or("").to_string();

        match entity {
            Some(ent) => {
                if !found_at_least_once {
                    entity_type = ent.entity_type.clone();
                }

                let cur_content_hash = &ent.content_hash;
                let cur_structural_hash = ent.structural_hash.as_deref();

                if !found_at_least_once {
                    // First appearance
                    found_at_least_once = true;
                    entries.push(LogEntry {
                        short_sha: commit.short_sha.clone(),
                        author: commit.author.clone(),
                        date,
                        message: msg_first_line,
                        change_type: EntityChangeType::Added,
                        content: Some(ent.content.clone()),
                        prev_content: None,
                    });
                } else if prev_entity_content.is_none() {
                    // Re-appeared after deletion
                    entries.push(LogEntry {
                        short_sha: commit.short_sha.clone(),
                        author: commit.author.clone(),
                        date,
                        message: msg_first_line,
                        change_type: EntityChangeType::Added,
                        content: Some(ent.content.clone()),
                        prev_content: None,
                    });
                } else {
                    // Entity existed before, check if it changed
                    let prev_hash = prev_entity_content.as_ref().map(|c| {
                        sem_core::utils::hash::content_hash(c)
                    });

                    let content_changed = prev_hash.as_deref() != Some(cur_content_hash.as_str());

                    if content_changed {
                        let structural_changed = match (cur_structural_hash, prev_structural_hash.as_deref()) {
                            (Some(cur), Some(prev)) => cur != prev,
                            _ => true, // if no structural hash, assume logic changed
                        };

                        let change_type = if structural_changed {
                            EntityChangeType::ModifiedLogic
                        } else {
                            EntityChangeType::ModifiedCosmetic
                        };

                        entries.push(LogEntry {
                            short_sha: commit.short_sha.clone(),
                            author: commit.author.clone(),
                            date,
                            message: msg_first_line,
                            change_type,
                            content: Some(ent.content.clone()),
                            prev_content: prev_entity_content.clone(),
                        });
                    }
                    // If content didn't change, skip (file changed but entity didn't)
                }

                prev_entity_content = Some(ent.content.clone());
                prev_structural_hash = ent.structural_hash.clone();
            }
            None => {
                // Entity not found in this commit
                if prev_entity_content.is_some() {
                    entries.push(LogEntry {
                        short_sha: commit.short_sha.clone(),
                        author: commit.author.clone(),
                        date,
                        message: msg_first_line,
                        change_type: EntityChangeType::Deleted,
                        content: None,
                        prev_content: prev_entity_content.take(),
                    });
                    prev_structural_hash = None;
                }
            }
        }
    }

    if !found_at_least_once {
        eprintln!(
            "{} Entity '{}' not found in any commit of {}",
            "error:".red().bold(),
            opts.entity_name,
            file_path
        );
        std::process::exit(1);
    }

    // Reverse so newest is at the bottom (display order: oldest first, top to bottom)
    // Actually keep chronological: oldest at top
    // entries are already oldest-first since we iterated commits.iter().rev()

    let total_commits = commits.len();
    let first_seen = entries.first().map(|e| e.date.clone()).unwrap_or_default();

    if opts.json {
        print_json(&opts.entity_name, &file_path, &entity_type, &entries, opts.verbose);
    } else {
        print_terminal(&opts.entity_name, &file_path, &entity_type, &entries, total_commits, &first_seen, opts.verbose);
    }
}

fn print_terminal(
    entity_name: &str,
    file_path: &str,
    entity_type: &str,
    entries: &[LogEntry],
    total_commits: usize,
    first_seen: &str,
    verbose: bool,
) {
    println!(
        "{}",
        format!("┌─ {} :: {} :: {}", file_path, entity_type, entity_name).bold()
    );
    println!("│");

    let max_author_len = entries.iter().map(|e| e.author.len()).max().unwrap_or(6);
    let max_change_len = entries.iter().map(|e| e.change_type.label().len()).max().unwrap_or(10);

    for entry in entries {
        let msg_short = truncate_str(&entry.message, 50);

        println!(
            "│  {}  {:<max_author$}  {}  {:<max_change$}  {}",
            entry.short_sha.yellow(),
            entry.author.cyan(),
            entry.date.dimmed(),
            entry.change_type.label_colored(),
            msg_short,
            max_author = max_author_len,
            max_change = max_change_len,
        );

        if verbose {
            if let (Some(prev), Some(cur)) = (&entry.prev_content, &entry.content) {
                print_inline_diff(prev, cur);
            } else if let Some(cur) = &entry.content {
                // Added: show the content
                for line in cur.lines() {
                    println!("│    {}", format!("+ {}", line).green());
                }
                println!("│");
            }
        }
    }

    println!("│");
    println!(
        "│  {}",
        format!(
            "{} changes across {} commits (first seen: {})",
            entries.len(),
            total_commits,
            first_seen
        )
        .dimmed()
    );
    println!("└{}", "─".repeat(60));
}

fn print_inline_diff(before: &str, after: &str) {
    use similar::TextDiff;

    let diff = TextDiff::from_lines(before, after);
    let mut has_changes = false;

    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Delete => {
                has_changes = true;
                print!("│    {}", format!("- {}", change).red());
            }
            similar::ChangeTag::Insert => {
                has_changes = true;
                print!("│    {}", format!("+ {}", change).green());
            }
            similar::ChangeTag::Equal => {} // skip unchanged lines in verbose diff
        }
    }

    if has_changes {
        println!("│");
    }
}

fn print_json(
    entity_name: &str,
    file_path: &str,
    entity_type: &str,
    entries: &[LogEntry],
    verbose: bool,
) {
    let json_entries: Vec<_> = entries
        .iter()
        .map(|e| {
            let mut obj = serde_json::json!({
                "commit": {
                    "sha": e.short_sha,
                    "author": e.author,
                    "date": e.date,
                    "message": e.message,
                },
                "change_type": e.change_type.label(),
                "structural_change": matches!(e.change_type, EntityChangeType::ModifiedLogic | EntityChangeType::Added),
            });

            if verbose {
                if let Some(content) = &e.content {
                    obj["after_content"] = serde_json::Value::String(content.clone());
                }
                if let Some(prev) = &e.prev_content {
                    obj["before_content"] = serde_json::Value::String(prev.clone());
                }
            }

            obj
        })
        .collect();

    let output = serde_json::json!({
        "entity": entity_name,
        "file": file_path,
        "type": entity_type,
        "changes": json_entries,
    });

    println!("{}", serde_json::to_string(&output).unwrap());
}

enum FindResult {
    Found(String),
    Ambiguous(Vec<String>),
    NotFound,
}

fn find_entity_file(
    root: &Path,
    registry: &sem_core::parser::registry::ParserRegistry,
    entity_name: &str,
) -> FindResult {
    let ext_filter: Vec<String> = vec![];
    let files = super::graph::find_supported_files_public(root, registry, &ext_filter);
    let mut found_in: Vec<String> = Vec::new();

    for file_path in &files {
        let full_path = root.join(file_path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let plugin = match registry.get_plugin(file_path) {
            Some(p) => p,
            None => continue,
        };

        let entities = plugin.extract_entities(&content, file_path);
        if entities.iter().any(|e| e.name == entity_name) {
            found_in.push(file_path.clone());
        }
    }

    match found_in.len() {
        0 => FindResult::NotFound,
        1 => FindResult::Found(found_in.into_iter().next().unwrap()),
        _ => FindResult::Ambiguous(found_in),
    }
}

/// Simple timestamp formatting without external deps.
fn chrono_lite_format(unix_seconds: i64) -> String {
    let days = unix_seconds / 86400;
    let mut y = 1970i64;
    let mut remaining_days = days;

    loop {
        let year_days = if is_leap(y) { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        y += 1;
    }

    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut m = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md {
            m = i;
            break;
        }
        remaining_days -= md;
    }

    let d = remaining_days + 1;
    format!("{:04}-{:02}-{:02}", y, m + 1, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
