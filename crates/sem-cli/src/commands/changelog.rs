use std::path::Path;

use colored::Colorize;
use sem_core::parser::changelog::{
    build_changelog, render_markdown, ChangelogResult,
};
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;

use super::common::{self, open_git_or_exit};

pub struct ChangelogOptions {
    pub cwd: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub commit: Option<String>,
    pub staged: bool,
    pub format: ChangelogFormat,
    pub heading: String,
    pub date: String,
    pub full: bool,
    pub file_exts: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChangelogFormat {
    Terminal,
    Markdown,
    Json,
}

pub fn changelog_command(opts: ChangelogOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();
    let git = open_git_or_exit(root);

    let file_changes = common::resolve_file_changes(
        &git,
        opts.commit.as_deref(),
        opts.from.as_deref(),
        opts.to.as_deref(),
        opts.staged,
    );
    let file_changes = common::filter_by_exts(file_changes, &opts.file_exts);

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

    // Build entity graph
    let ext_filter = common::normalize_exts(&opts.file_exts);
    let all_files = sem_core::utils::files::find_supported_files(root, &registry, &ext_filter);
    let graph = EntityGraph::build(root, &all_files, &registry);

    // Get commit messages for conventional commit parsing
    let commits = if let (Some(ref from), Some(ref to)) = (&opts.from, &opts.to) {
        git.get_log_range(from, to).unwrap_or_default()
    } else {
        Vec::new()
    };

    let result = build_changelog(&diff, &graph, &commits, &opts.date);

    match opts.format {
        ChangelogFormat::Json => print_json(&result),
        ChangelogFormat::Markdown => print_markdown(&result, &opts.heading, opts.full),
        ChangelogFormat::Terminal => print_terminal(&result, &opts.heading, opts.full),
    }
}

fn print_json(result: &ChangelogResult) {
    println!("{}", serde_json::to_string_pretty(result).unwrap_or_default());
}

fn print_markdown(result: &ChangelogResult, heading: &str, full: bool) {
    println!("{}", render_markdown(result, heading, full));
}

fn print_terminal(result: &ChangelogResult, heading: &str, full: bool) {
    let mut lines = Vec::new();

    lines.push(String::new());
    lines.push(format!(
        "{} {} — {}",
        "##".dimmed(),
        heading.bold(),
        result.date.dimmed()
    ));

    if !result.breaking.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}", "### Breaking Changes".red().bold()));
        for entry in &result.breaking {
            lines.push(format!("  {} {}", "!".red().bold(), entry.description));
        }
    }

    if !result.added.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}", "### Added".green().bold()));
        for entry in &result.added {
            lines.push(format!("  {} {}", "+".green(), entry.description));
        }
    }

    if !result.changed.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}", "### Changed".yellow().bold()));
        for entry in &result.changed {
            lines.push(format!("  {} {}", "~".yellow(), entry.description));
        }
    }

    if !result.removed.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}", "### Removed".red().bold()));
        for entry in &result.removed {
            lines.push(format!("  {} {}", "-".red(), entry.description));
        }
    }

    if !result.internal.is_empty() {
        lines.push(String::new());
        let total = result.internal.len();
        lines.push(format!("{}", "### Internal".dimmed().bold()));
        if full || total <= 5 {
            for entry in &result.internal {
                lines.push(format!("  {} {}", "·".dimmed(), entry.description.dimmed()));
            }
        } else {
            for entry in result.internal.iter().take(5) {
                lines.push(format!("  {} {}", "·".dimmed(), entry.description.dimmed()));
            }
            lines.push(format!(
                "  {} {}",
                "·".dimmed(),
                format!("... and {} more internal changes (use --full to show all)", total - 5).dimmed()
            ));
        }
    }

    lines.push(String::new());
    let bump_colored = match result.semver_suggestion {
        sem_core::parser::changelog::SemverBump::Major => "MAJOR".red().bold().to_string(),
        sem_core::parser::changelog::SemverBump::Minor => "MINOR".yellow().bold().to_string(),
        sem_core::parser::changelog::SemverBump::Patch => "PATCH".green().bold().to_string(),
    };
    lines.push(format!(
        "Suggested version bump: {} ({})",
        bump_colored,
        result.semver_reason.dimmed()
    ));

    println!("{}", lines.join("\n"));
}
