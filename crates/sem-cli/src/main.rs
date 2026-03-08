mod commands;
mod formatters;

use clap::{Parser, Subcommand};
use commands::blame::{blame_command, BlameOptions};
use commands::diff::{diff_command, DiffOptions, OutputFormat};
use commands::graph::{graph_command, GraphFormat, GraphOptions};
use commands::impact::{impact_command, ImpactOptions};
use commands::changelog::{changelog_command, ChangelogFormat, ChangelogOptions};
use commands::log::{log_command, LogFormat, LogOptions};
use commands::review::{review_command, ReviewFormat, ReviewOptions};
use sem_core::utils::date::today_date;

#[derive(Parser)]
#[command(name = "sem", version = env!("CARGO_PKG_VERSION"), about = "Semantic version control")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show semantic diff of changes
    Diff {
        /// Two files to compare (e.g. sem diff old.ts new.ts)
        #[arg(num_args = 0..=2)]
        files: Vec<String>,

        /// Show only staged changes
        #[arg(long)]
        staged: bool,

        /// Show changes from a specific commit
        #[arg(long)]
        commit: Option<String>,

        /// Start of commit range
        #[arg(long)]
        from: Option<String>,

        /// End of commit range
        #[arg(long)]
        to: Option<String>,

        /// Read FileChange[] JSON from stdin instead of git
        #[arg(long)]
        stdin: bool,

        /// Output format: terminal or json
        #[arg(long, default_value = "terminal")]
        format: String,

        /// Show internal timing profile
        #[arg(long, hide = true)]
        profile: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long)]
        file_exts: Vec<String>,
    },
    /// Show impact of changing an entity (what else would break?)
    Impact {
        /// Name of the entity to analyze
        #[arg()]
        entity: String,

        /// Specific files to analyze (default: all supported files)
        #[arg(long)]
        files: Vec<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long)]
        file_exts: Vec<String>,
    },
    /// Show semantic blame — who last modified each entity
    Blame {
        /// File to blame
        #[arg()]
        file: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Generate changelog / release notes from a commit range
    Changelog {
        /// Show only staged changes
        #[arg(long)]
        staged: bool,

        /// Show changes from a specific commit
        #[arg(long)]
        commit: Option<String>,

        /// Start of commit range
        #[arg(long)]
        from: Option<String>,

        /// End of commit range
        #[arg(long)]
        to: Option<String>,

        /// Output format: terminal, markdown, or json
        #[arg(long, default_value = "terminal")]
        format: String,

        /// Section heading (default: "Unreleased")
        #[arg(long, default_value = "Unreleased")]
        heading: String,

        /// Date string for the changelog header
        #[arg(long)]
        date: Option<String>,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long)]
        file_exts: Vec<String>,
    },
    /// Semantic PR review: groups changes by impact and assesses risk
    Review {
        /// Show only staged changes
        #[arg(long)]
        staged: bool,

        /// Show changes from a specific commit
        #[arg(long)]
        commit: Option<String>,

        /// Start of commit range
        #[arg(long)]
        from: Option<String>,

        /// End of commit range
        #[arg(long)]
        to: Option<String>,

        /// Output format: terminal or json
        #[arg(long, default_value = "terminal")]
        format: String,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long)]
        file_exts: Vec<String>,
    },
    /// Show full history of a single entity across commits
    Log {
        /// Entity name to track
        #[arg(long)]
        entity: String,

        /// Scope to a specific file if entity name is ambiguous
        #[arg(long)]
        file: Option<String>,

        /// Start of history range
        #[arg(long)]
        from: Option<String>,

        /// End of history range (default: HEAD)
        #[arg(long)]
        to: Option<String>,

        /// Output format: terminal or json
        #[arg(long, default_value = "terminal")]
        format: String,

        /// Follow renames (default: true)
        #[arg(long, default_value_t = true)]
        follow: bool,

        /// Only include files with these extensions
        #[arg(long)]
        file_exts: Vec<String>,
    },
    /// Show entity dependency graph
    Graph {
        /// Specific files to analyze (default: all supported files)
        #[arg()]
        files: Vec<String>,

        /// Show dependencies/dependents for a specific entity
        #[arg(long)]
        entity: Option<String>,

        /// Output format: terminal or json
        #[arg(long, default_value = "terminal")]
        format: String,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long)]
        file_exts: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Diff {
            files,
            staged,
            commit,
            from,
            to,
            stdin,
            format,
            profile,
            file_exts,
        }) => {
            let output_format = match format.as_str() {
                "json" => OutputFormat::Json,
                _ => OutputFormat::Terminal,
            };

            diff_command(DiffOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                format: output_format,
                staged,
                commit,
                from,
                to,
                stdin,
                profile,
                file_exts,
                files,
            });
        }
        Some(Commands::Blame { file, json }) => {
            blame_command(BlameOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                file_path: file,
                json,
            });
        }
        Some(Commands::Impact {
            entity,
            files,
            json,
            file_exts,
        }) => {
            impact_command(ImpactOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                entity_name: entity,
                file_paths: files,
                json,
                file_exts,
            });
        }
        Some(Commands::Changelog {
            staged,
            commit,
            from,
            to,
            format,
            heading,
            date,
            file_exts,
        }) => {
            let changelog_format = match format.as_str() {
                "json" => ChangelogFormat::Json,
                "markdown" | "md" => ChangelogFormat::Markdown,
                _ => ChangelogFormat::Terminal,
            };

            let date_str = date.unwrap_or_else(today_date);

            changelog_command(ChangelogOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                from,
                to,
                commit,
                staged,
                format: changelog_format,
                heading,
                date: date_str,
                file_exts,
            });
        }
        Some(Commands::Review {
            staged,
            commit,
            from,
            to,
            format,
            file_exts,
        }) => {
            let review_format = match format.as_str() {
                "json" => ReviewFormat::Json,
                _ => ReviewFormat::Terminal,
            };

            review_command(ReviewOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                from,
                to,
                commit,
                staged,
                format: review_format,
                file_exts,
            });
        }
        Some(Commands::Log {
            entity,
            file,
            from,
            to,
            format,
            follow,
            file_exts,
        }) => {
            let log_format = match format.as_str() {
                "json" => LogFormat::Json,
                _ => LogFormat::Terminal,
            };

            log_command(LogOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                entity_name: entity,
                file,
                from,
                to,
                format: log_format,
                follow,
                file_exts,
            });
        }
        Some(Commands::Graph {
            files,
            entity,
            format,
            file_exts,
        }) => {
            let graph_format = match format.as_str() {
                "json" => GraphFormat::Json,
                _ => GraphFormat::Terminal,
            };

            graph_command(GraphOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                file_paths: files,
                entity,
                format: graph_format,
                file_exts,
            });
        }
        None => {
            // Default to diff when no subcommand is given
            diff_command(DiffOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                format: OutputFormat::Terminal,
                staged: false,
                commit: None,
                from: None,
                to: None,
                stdin: false,
                profile: false,
                file_exts: vec![],
                files: vec![],
            });
        }
    }
}
