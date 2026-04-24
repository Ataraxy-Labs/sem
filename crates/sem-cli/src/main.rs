mod cache;
mod commands;
mod formatters;

use clap::CommandFactory;
use clap::{Parser, Subcommand, ValueEnum};
use colored::control;
use colored::Colorize;
use commands::blame::{blame_command, BlameOptions};
use commands::context::{context_command, ContextOptions};
use commands::diff::{diff_command, DiffOptions, OutputFormat};
use commands::entities::{entities_command, EntitiesOptions};
use commands::impact::{impact_command, ImpactMode, ImpactOptions};
use commands::log::{log_command, LogOptions};
use commands::verify::{verify_command, VerifyOptions};

#[derive(Parser)]
#[command(name = "sem", version = env!("CARGO_PKG_VERSION"), about = "Semantic version control")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorMode {
    Always,
    Auto,
    Never,
}

#[derive(Subcommand)]
enum Commands {
    /// Show semantic diff of changes (supports git diff syntax)
    Diff {
        /// Git refs, files, or pathspecs (supports ref1..ref2, ref1...ref2, -- paths)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,

        /// Show only staged changes (alias: --cached)
        #[arg(long)]
        staged: bool,

        /// Show only staged changes (alias for --staged)
        #[arg(long)]
        cached: bool,

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

        /// Read unified diff from stdin (e.g. git diff | sem diff --patch)
        #[arg(long)]
        patch: bool,

        /// Output format
        #[arg(long, default_value = "terminal")]
        format: OutputFormat,

        /// Show inline content diffs for each entity
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Show internal timing profile
        #[arg(long, hide = true)]
        profile: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// When to use colors
        #[arg(long, default_value = "auto")]
        color: ColorMode,

        /// Run as if started in this directory (like git -C)
        #[arg(short = 'C', long = "cwd")]
        directory: Option<String>,
    },
    /// Show impact of changing an entity (deps, dependents, transitive impact, tests)
    Impact {
        /// Name of the entity to analyze
        #[arg()]
        entity: String,

        /// File containing the entity (disambiguates if multiple matches)
        #[arg(long)]
        file: Option<String>,

        /// Show direct dependencies only
        #[arg(long)]
        deps: bool,

        /// Show direct dependents only
        #[arg(long)]
        dependents: bool,

        /// Show affected test entities only
        #[arg(long)]
        tests: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Skip the SQLite entity cache (rebuild from scratch)
        #[arg(long)]
        no_cache: bool,
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
    /// Show evolution of an entity through git history
    Log {
        /// Name of the entity to trace
        #[arg()]
        entity: String,

        /// File containing the entity (auto-detected if omitted)
        #[arg(long)]
        file: Option<String>,

        /// Maximum number of commits to scan
        #[arg(long, default_value = "50")]
        limit: usize,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Show content diff between versions
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// List entities in a file
    Entities {
        /// File to extract entities from
        #[arg()]
        file: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show token-budgeted context for an entity
    Context {
        /// Name of the entity
        #[arg()]
        entity: String,

        /// File containing the entity (disambiguates if multiple matches)
        #[arg(long)]
        file: Option<String>,

        /// Token budget
        #[arg(long, default_value = "8000")]
        budget: usize,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Skip the SQLite entity cache (rebuild from scratch)
        #[arg(long)]
        no_cache: bool,
    },
    /// Verify function call arity across the codebase
    Verify {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Compare working tree vs HEAD, find broken callers from signature changes
        #[arg(long)]
        diff: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,
    },
    /// Start the MCP server (stdin/stdout transport)
    Mcp,
    /// Replace `git diff` with `sem diff` globally
    Setup,
    /// Restore default `git diff` behavior
    Unsetup,
    /// Generate shell completions
    Completions {
        /// The shell to generate the completions for
        #[arg(value_enum)]
        shell: clap_complete_command::Shell,
    },
}

fn apply_color_mode(mode: ColorMode) {
    match mode {
        ColorMode::Always => control::set_override(true),
        ColorMode::Never => control::set_override(false),
        ColorMode::Auto => {}
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Diff {
            args,
            staged,
            cached,
            commit,
            from,
            to,
            stdin,
            patch,
            verbose,
            format,
            profile,
            file_exts,
            color,
            directory,
        }) => {
            apply_color_mode(color);

            let cwd = directory.unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

            diff_command(DiffOptions {
                cwd,
                format,
                staged: staged || cached,
                commit,
                from,
                to,
                stdin,
                patch,
                verbose,
                profile,
                file_exts,
                args,
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
            file,
            deps,
            dependents,
            tests,
            json,
            file_exts,
            no_cache,
        }) => {
            let mode = if deps {
                ImpactMode::Deps
            } else if dependents {
                ImpactMode::Dependents
            } else if tests {
                ImpactMode::Tests
            } else {
                ImpactMode::All
            };

            impact_command(ImpactOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                entity_name: entity,
                file_hint: file,
                json,
                file_exts,
                mode,
                no_cache,
            });
        }
        Some(Commands::Log {
            entity,
            file,
            limit,
            json,
            verbose,
        }) => {
            log_command(LogOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                entity_name: entity,
                file_path: file,
                limit,
                json,
                verbose,
            });
        }
        Some(Commands::Entities { file, json }) => {
            entities_command(EntitiesOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                file_path: file,
                json,
            });
        }
        Some(Commands::Context {
            entity,
            file,
            budget,
            json,
            file_exts,
            no_cache,
        }) => {
            context_command(ContextOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                entity_name: entity,
                file_path: file,
                budget,
                json,
                file_exts,
                no_cache,
            });
        }
        Some(Commands::Verify {
            json,
            diff,
            file_exts,
        }) => {
            verify_command(VerifyOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                json,
                diff,
                file_exts,
            });
        }
        Some(Commands::Mcp) => {
            if let Err(e) = sem_mcp::run() {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Setup) => {
            if let Err(e) = commands::setup::run() {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Unsetup) => {
            if let Err(e) = commands::setup::unsetup() {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Completions { shell }) => {
            shell.generate(&mut Cli::command(), &mut std::io::stdout());
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
                patch: false,
                verbose: false,
                profile: false,
                file_exts: vec![],
                args: vec![],
            });
        }
    }
}
