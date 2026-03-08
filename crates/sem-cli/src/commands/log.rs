use std::collections::HashSet;
use std::path::Path;
use std::process;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::parser::log::{
    build_entity_log, resolve_entity, EntityEventType, EntityLogResult, EntityResolutionError,
};
use sem_core::parser::plugins::create_default_registry;

pub struct LogOptions {
    pub cwd: String,
    pub entity_name: String,
    pub file: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub format: LogFormat,
    pub follow: bool,
    pub file_exts: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Terminal,
    Json,
}

pub fn log_command(opts: LogOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    let git = match GitBridge::open(root) {
        Ok(g) => g,
        Err(_) => {
            eprintln!("{}", "Error: Not inside a Git repository.".red());
            process::exit(1);
        }
    };

    // Use git repo root for file scanning so entity paths match git paths.
    let repo_root = git.repo_root();

    // Resolve entity
    let entity = match resolve_entity(
        &registry,
        repo_root,
        &opts.entity_name,
        opts.file.as_deref(),
        &opts.file_exts,
    ) {
        Ok(e) => e,
        Err(EntityResolutionError::NotFound) => {
            eprintln!(
                "{} Entity '{}' not found",
                "error:".red().bold(),
                opts.entity_name
            );
            process::exit(1);
        }
        Err(EntityResolutionError::Ambiguous(matches)) => {
            eprintln!(
                "{} '{}' is ambiguous. Did you mean one of:",
                "error:".red().bold(),
                opts.entity_name
            );
            for (i, m) in matches.iter().enumerate() {
                eprintln!(
                    "  {}. {} :: {} :: {}",
                    i + 1,
                    m.file_path.dimmed(),
                    m.entity_type.dimmed(),
                    m.name.bold()
                );
            }
            eprintln!(
                "\n{}",
                "Use --file <path> to disambiguate.".dimmed()
            );
            process::exit(1);
        }
    };

    // Build entity log
    let result = match build_entity_log(
        &git,
        &registry,
        &entity,
        opts.from.as_deref(),
        opts.to.as_deref(),
        opts.follow,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} {}", "error:".red().bold(), e);
            process::exit(1);
        }
    };

    match opts.format {
        LogFormat::Json => print_json(&result),
        LogFormat::Terminal => print_terminal(&result),
    }
}

fn print_json(result: &EntityLogResult) {
    println!("{}", serde_json::to_string_pretty(result).unwrap_or_default());
}

fn print_terminal(result: &EntityLogResult) {
    if result.events.is_empty() {
        println!("{}", "No history found.".dimmed());
        return;
    }

    // Header: file :: type :: name
    println!();
    println!(
        "  {} :: {} :: {}",
        result.file_path.dimmed(),
        result.entity_type.dimmed(),
        result.entity_name.bold()
    );
    println!();

    // Events table (newest first for display — events are stored oldest-first, so reverse)
    for event in result.events.iter().rev() {
        let tag = match &event.event_type {
            EntityEventType::Added => format!("[{}]", "added".green()),
            EntityEventType::Modified => format!("[{}]", "modified".yellow()),
            EntityEventType::Deleted => format!("[{}]", "deleted".red()),
            EntityEventType::Renamed { .. } => format!("[{}]", "renamed".cyan()),
            EntityEventType::Moved { .. } => format!("[{}]", "moved".cyan()),
            EntityEventType::SignatureChanged { .. } => {
                format!("[{}]", "signature changed".magenta())
            }
        };

        // Pad author to consistent width (Unicode-safe truncation)
        let author: String = event.author.chars().take(12).collect();
        let author_padded = format!("{:<12}", author);

        let desc = if event.description.is_empty() {
            String::new()
        } else {
            event.description.dimmed().to_string()
        };

        println!(
            "  {}  {}  {}  {:<24}  {}",
            event.short_sha.dimmed(),
            event.date,
            author_padded,
            tag,
            desc
        );
    }

    // Summary line
    let event_count = result.events.len();
    let authors: HashSet<&str> = result.events.iter().map(|e| e.author.as_str()).collect();
    let author_count = authors.len();

    // Date range
    let date_range = if event_count > 1 {
        let first = &result.events.first().unwrap().date;
        let last = &result.events.last().unwrap().date;
        if first != last {
            format!("{first} to {last}")
        } else {
            first.clone()
        }
    } else if event_count == 1 {
        result.events[0].date.clone()
    } else {
        String::new()
    };

    println!();
    println!(
        "  {} {} — {} {} — {}",
        event_count,
        if event_count == 1 { "event" } else { "events" },
        author_count,
        if author_count == 1 { "author" } else { "authors" },
        date_range.dimmed()
    );
    println!();
}
