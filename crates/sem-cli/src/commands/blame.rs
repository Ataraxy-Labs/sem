use std::path::Path;

use colored::Colorize;
use git2::Repository;
use sem_core::parser::plugins::create_default_registry;

pub struct BlameOptions {
    pub cwd: String,
    pub file_path: String,
    pub json: bool,
}

struct EntityBlame {
    name: String,
    entity_type: String,
    start_line: usize,
    end_line: usize,
    author: String,
    date: String,
    commit_sha: String,
    summary: String,
}

pub fn blame_command(opts: BlameOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    // Read file and extract entities
    let full_path = root.join(&opts.file_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} Cannot read {}: {}", "error:".red().bold(), opts.file_path, e);
            std::process::exit(1);
        }
    };

    let plugin = match registry.get_plugin(&opts.file_path) {
        Some(p) => p,
        None => {
            eprintln!(
                "{} Unsupported file type: {}",
                "error:".red().bold(),
                opts.file_path
            );
            std::process::exit(1);
        }
    };

    let entities = plugin.extract_entities(&content, &opts.file_path);
    if entities.is_empty() {
        eprintln!("{} No entities found in {}", "warning:".yellow().bold(), opts.file_path);
        return;
    }

    // Open git repo and run blame
    let repo: git2::Repository = match Repository::discover(root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} Not a git repository: {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
    };

    // Resolve file path relative to repo root for git blame
    let repo_root = repo.workdir().unwrap_or(root);
    let abs_file = std::fs::canonicalize(root.join(&opts.file_path)).unwrap_or(full_path.clone());
    let repo_root_canonical = std::fs::canonicalize(repo_root).unwrap_or(repo_root.to_path_buf());
    let relative_path = abs_file
        .strip_prefix(&repo_root_canonical)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| opts.file_path.clone());

    let blame: git2::Blame = match repo.blame_file(Path::new(&relative_path), None) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{} Cannot blame {}: {}", "error:".red().bold(), opts.file_path, e);
            std::process::exit(1);
        }
    };

    // For each entity, find the most recent commit that touched its lines
    let mut results: Vec<EntityBlame> = Vec::new();

    for entity in &entities {
        // Find the latest commit across the entity's line range
        let mut latest_time: i64 = 0;
        let mut latest_author = String::new();
        let mut latest_sha = String::new();
        let mut latest_summary = String::new();
        let mut latest_date = String::new();

        for line in entity.start_line..=entity.end_line {
            if let Some(hunk) = blame.get_line(line) {
                let sig = hunk.final_signature();
                let time = sig.when().seconds();
                if time > latest_time {
                    latest_time = time;
                    latest_author = sig.name().unwrap_or("unknown").to_string();
                    let oid = hunk.final_commit_id();
                    latest_sha = format!("{}", oid);
                    latest_summary = repo
                        .find_commit(oid)
                        .ok()
                        .and_then(|c| c.summary().map(String::from))
                        .unwrap_or_default();

                    // Format date
                    let ts = sig.when().seconds();
                    let naive = chrono_lite_format(ts);
                    latest_date = naive;
                }
            }
        }

        results.push(EntityBlame {
            name: entity.name.clone(),
            entity_type: entity.entity_type.clone(),
            start_line: entity.start_line,
            end_line: entity.end_line,
            author: latest_author,
            date: latest_date,
            commit_sha: latest_sha,
            summary: latest_summary,
        });
    }

    if opts.json {
        let output: Vec<_> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "type": r.entity_type,
                    "lines": [r.start_line, r.end_line],
                    "author": r.author,
                    "date": r.date,
                    "commit": &r.commit_sha[..8.min(r.commit_sha.len())],
                    "summary": r.summary,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!(
            "{}",
            format!("┌─ {} ", opts.file_path).bold()
        );
        println!("│");

        // Group by parent (top-level vs nested)
        let max_name_len = results.iter().map(|r| r.name.len()).max().unwrap_or(10);
        let max_type_len = results.iter().map(|r| r.entity_type.len()).max().unwrap_or(8);

        for r in &results {
            let sha_short = if r.commit_sha.len() >= 8 {
                &r.commit_sha[..8]
            } else {
                &r.commit_sha
            };

            let indent = if results.iter().any(|other| {
                other.name != r.name
                    && other.start_line <= r.start_line
                    && other.end_line >= r.end_line
                    && !(other.start_line == r.start_line && other.end_line == r.end_line)
            }) {
                "│    "
            } else {
                "│  "
            };

            let summary_short = if r.summary.len() > 40 {
                format!("{}...", &r.summary[..37])
            } else {
                r.summary.clone()
            };

            println!(
                "{}  {:<max_type_len$}  {:<max_name_len$}  {}  {}  {}  {}",
                indent,
                r.entity_type.dimmed(),
                r.name.bold(),
                sha_short.yellow(),
                r.author.cyan(),
                r.date.dimmed(),
                summary_short,
                max_type_len = max_type_len,
                max_name_len = max_name_len,
            );
        }

        println!("│");
        println!("└{}", "─".repeat(60));
    }
}

/// Simple timestamp formatting without external deps.
fn chrono_lite_format(unix_seconds: i64) -> String {
    // Convert unix timestamp to date string
    let days = unix_seconds / 86400;
    let mut y = 1970;
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
