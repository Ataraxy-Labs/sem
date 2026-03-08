use std::path::Path;
use std::process;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::git::types::DiffScope;
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::review::{build_review, ReviewChange, ReviewResult, RiskLevel};

pub struct ReviewOptions {
    pub cwd: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub commit: Option<String>,
    pub staged: bool,
    pub format: ReviewFormat,
    pub file_exts: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReviewFormat {
    Terminal,
    Json,
}

pub fn review_command(opts: ReviewOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    // Get file changes from git
    let git = match GitBridge::open(root) {
        Ok(g) => g,
        Err(_) => {
            eprintln!("\x1b[31mError: Not inside a Git repository.\x1b[0m");
            process::exit(1);
        }
    };

    let file_changes = if let Some(ref sha) = opts.commit {
        let scope = DiffScope::Commit { sha: sha.clone() };
        match git.get_changed_files(&scope) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else if let (Some(ref from), Some(ref to)) = (&opts.from, &opts.to) {
        let scope = DiffScope::Range {
            from: from.clone(),
            to: to.clone(),
        };
        match git.get_changed_files(&scope) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else if opts.staged {
        let scope = DiffScope::Staged;
        match git.get_changed_files(&scope) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else {
        match git.detect_and_get_files() {
            Ok((_scope, files)) => files,
            Err(_) => {
                eprintln!("\x1b[31mError: Not inside a Git repository.\x1b[0m");
                process::exit(1);
            }
        }
    };

    // Filter by extensions
    let file_changes = if opts.file_exts.is_empty() {
        file_changes
    } else {
        let exts: Vec<String> = opts.file_exts.iter().map(|e| {
            if e.starts_with('.') { e.clone() } else { format!(".{}", e) }
        }).collect();
        file_changes.into_iter().filter(|fc| {
            exts.iter().any(|ext| fc.file_path.ends_with(ext.as_str()))
        }).collect()
    };

    if file_changes.is_empty() {
        println!("{}", "No changes detected.".dimmed());
        return;
    }

    // Compute semantic diff
    let diff = compute_semantic_diff(&file_changes, &registry, None, None);

    if diff.changes.is_empty() {
        println!("{}", "No semantic changes detected.".dimmed());
        return;
    }

    // Build entity graph for dependent analysis.
    // Collect all files referenced in changes + discover supported files.
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let all_files = super::graph::find_supported_files_public(root, &registry, &ext_filter);
    let graph = EntityGraph::build(root, &all_files, &registry);

    // Build the review
    let review = build_review(&diff, &graph);

    match opts.format {
        ReviewFormat::Json => print_json(&review),
        ReviewFormat::Terminal => print_terminal(&review),
    }
}

fn print_json(review: &ReviewResult) {
    let output = serde_json::to_string_pretty(review).unwrap_or_default();
    println!("{output}");
}

fn print_terminal(review: &ReviewResult) {
    let mut lines: Vec<String> = Vec::new();

    // API Surface Changes
    if !review.api_surface_changes.is_empty() {
        lines.push(String::new());
        let header = "─ API Surface Changes ";
        let pad = 55usize.saturating_sub(header.len());
        lines.push(format!("┌{header}{}", "─".repeat(pad)).dimmed().to_string());
        for rc in &review.api_surface_changes {
            lines.push(format_review_change(rc, true));
        }
        lines.push(format!("└{}", "─".repeat(55)).dimmed().to_string());
    }

    // Internal Changes
    if !review.internal_changes.is_empty() {
        lines.push(String::new());
        let header = "─ Internal Changes ";
        let pad = 55usize.saturating_sub(header.len());
        lines.push(format!("┌{header}{}", "─".repeat(pad)).dimmed().to_string());
        for rc in &review.internal_changes {
            lines.push(format_review_change(rc, false));
        }
        lines.push(format!("└{}", "─".repeat(55)).dimmed().to_string());
    }

    // Config / Data Changes
    if !review.config_changes.is_empty() {
        lines.push(String::new());
        let header = "─ Config / Data Changes ";
        let pad = 55usize.saturating_sub(header.len());
        lines.push(format!("┌{header}{}", "─".repeat(pad)).dimmed().to_string());
        for rc in &review.config_changes {
            lines.push(format_config_change(rc));
        }
        lines.push(format!("└{}", "─".repeat(55)).dimmed().to_string());
    }

    // Summary
    lines.push(String::new());
    let mut parts = Vec::new();
    if review.summary.api_surface_count > 0 {
        parts.push(format!("{} API surface", review.summary.api_surface_count));
    }
    if review.summary.internal_count > 0 {
        parts.push(format!("{} internal", review.summary.internal_count));
    }
    if review.summary.config_count > 0 {
        parts.push(format!("{} config", review.summary.config_count));
    }
    lines.push(format!(
        "{} {}",
        "Summary:".bold(),
        parts.join(", ")
    ));

    // Risk
    let risk_colored = match review.risk.level {
        RiskLevel::Low => "low".green().bold().to_string(),
        RiskLevel::Medium => "medium".yellow().bold().to_string(),
        RiskLevel::High => "high".red().bold().to_string(),
    };
    lines.push(format!(
        "{} {} ({})",
        "Risk:".bold(),
        risk_colored,
        review.risk.reason.dimmed()
    ));

    println!("{}", lines.join("\n"));
}

fn change_symbol(ct: ChangeType) -> String {
    match ct {
        ChangeType::Added => "⊕".green().to_string(),
        ChangeType::Modified => "∆".yellow().to_string(),
        ChangeType::Deleted => "⊖".red().to_string(),
        ChangeType::Moved => "→".blue().to_string(),
        ChangeType::Renamed => "↻".cyan().to_string(),
    }
}

fn format_review_change(rc: &ReviewChange, show_dependents: bool) -> String {
    let mut lines = Vec::new();
    let sym = change_symbol(rc.change_type);
    let tag = format!("[{}]", rc.change_label);
    let tag_colored = match rc.change_type {
        ChangeType::Added => tag.green().to_string(),
        ChangeType::Modified => tag.yellow().to_string(),
        ChangeType::Deleted => tag.red().to_string(),
        ChangeType::Moved => tag.blue().to_string(),
        ChangeType::Renamed => tag.cyan().to_string(),
    };

    lines.push(format!(
        "{}  {} {:<10} {:<25} {}",
        "│".dimmed(),
        sym,
        rc.entity_type.dimmed(),
        rc.entity_name.bold(),
        tag_colored,
    ));

    if show_dependents && rc.dependent_count > 0 {
        let file_label = if rc.dependent_file_count == 1 { "file" } else { "files" };
        lines.push(format!(
            "{}    ~{} dependents across {} {}",
            "│".dimmed(),
            rc.dependent_count,
            rc.dependent_file_count,
            file_label,
        ).dimmed().to_string());
    } else if rc.change_type == ChangeType::Added {
        lines.push(format!(
            "{}    {}",
            "│".dimmed(),
            "0 dependents (new)".dimmed(),
        ));
    }

    if rc.change_type == ChangeType::Deleted && !rc.was_referenced_by.is_empty() {
        let refs: Vec<&str> = rc.was_referenced_by.iter().take(3).map(|s| s.as_str()).collect();
        let suffix = if rc.was_referenced_by.len() > 3 {
            format!(" (+{} more)", rc.was_referenced_by.len() - 3)
        } else {
            String::new()
        };
        lines.push(format!(
            "{}    {}",
            "│".dimmed(),
            format!("↳ was called by: {}{}", refs.join(", "), suffix).dimmed(),
        ));
    }

    if let Some(ref old_path) = rc.old_file_path {
        lines.push(format!(
            "{}    {}",
            "│".dimmed(),
            format!("from {old_path}").dimmed(),
        ));
    }

    lines.join("\n")
}

fn format_config_change(rc: &ReviewChange) -> String {
    let sym = change_symbol(rc.change_type);
    let mut line = format!(
        "{}  {} {:<10} {}",
        "│".dimmed(),
        sym,
        rc.entity_type.dimmed(),
        rc.entity_name.bold(),
    );

    if let Some(ref vd) = rc.value_diff {
        line.push_str(&format!("  {}", format!("[{vd}]").dimmed()));
    } else {
        let tag = format!("[{}]", rc.change_label);
        let tag_colored = match rc.change_type {
            ChangeType::Added => tag.green().to_string(),
            ChangeType::Modified => tag.yellow().to_string(),
            ChangeType::Deleted => tag.red().to_string(),
            _ => tag.dimmed().to_string(),
        };
        line.push_str(&format!("  {tag_colored}"));
    }

    line
}
