mod commands;
mod formatters;

use clap::{Parser, Subcommand};
use commands::blame::{blame_command, BlameOptions};
use commands::diff::{diff_command, DiffOptions, OutputFormat};
use commands::graph::{graph_command, GraphFormat, GraphOptions};
use commands::impact::{impact_command, ImpactOptions};

#[derive(Parser)]
#[command(name = "sem", version = "0.2.0", about = "Semantic version control")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show semantic diff of changes
    Diff {
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
            staged,
            commit,
            from,
            to,
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
                profile,
                file_exts,
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
                profile: false,
                file_exts: vec![],
            });
        }
    }
}
