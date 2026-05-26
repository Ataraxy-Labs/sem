mod cache;
mod commands;
mod formatters;
mod stats;

use clap::CommandFactory;
use clap::{Parser, Subcommand, ValueEnum};
use colored::control;
use colored::Colorize;
use commands::blame::{blame_command, BlameOptions};
use commands::context::{context_command, ContextOptions};
use commands::diff::{diff_command, DiffOptions, OutputFormat};
use commands::entities::{entities_command, EntitiesOptions};
use commands::graph::{graph_command, GraphOptions};
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
    /// Show semantic diff of changes (supports git diff syntax). Untracked files are excluded, matching git behavior.
    Diff {
        /// Display path label for direct file comparison
        #[arg(long, hide = true)]
        label: Option<String>,

        /// Git refs, files, or pathspecs (supports ref1..ref2, ref1...ref2, -- paths)
        #[arg(num_args = 0.., value_name = "ARG")]
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

        /// Shorthand for --format json
        #[arg(long)]
        json: bool,

        /// Show inline content diffs for each entity
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Show internal timing profile
        #[arg(long, hide = true)]
        profile: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Hide cosmetic changes (formatting, whitespace, comments only)
        #[arg(long)]
        no_cosmetics: bool,

        /// When to use colors
        #[arg(long, default_value = "auto")]
        color: ColorMode,

        /// Run as if started in this directory (like git -C)
        #[arg(short = 'C', long = "cwd")]
        directory: Option<String>,

        /// Pathspecs for filtering, passed after --
        #[arg(last = true, allow_hyphen_values = true, value_name = "PATHSPEC")]
        pathspecs: Vec<String>,
    },
    /// Show impact of changing an entity (deps, dependents, transitive impact, tests)
    Impact {
        /// Name of the entity to analyze
        #[arg(required_unless_present = "entity_id")]
        entity: Option<String>,

        /// Look up entity by its ID (from sem diff --format json output)
        #[arg(long)]
        entity_id: Option<String>,

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

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
        #[arg(long)]
        json: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Max traversal depth for transitive impact (default 2, 0 = unlimited)
        #[arg(long, default_value = "2")]
        depth: usize,

        /// Skip the SQLite entity cache (rebuild from scratch)
        #[arg(long)]
        no_cache: bool,
    },
    /// Show the full entity dependency graph
    Graph {
        /// Repository path (defaults to current directory)
        #[arg(default_value = ".")]
        path: String,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
        #[arg(long)]
        json: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Skip the SQLite entity cache (rebuild from scratch)
        #[arg(long)]
        no_cache: bool,

        /// Include directories that are excluded by default (fixtures, vendor, node_modules, etc.)
        #[arg(long)]
        no_default_excludes: bool,
    },
    /// Show semantic blame — who last modified each entity
    Blame {
        /// File to blame
        #[arg()]
        file: String,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
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

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
        #[arg(long)]
        json: bool,

        /// Show content diff between versions
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// List entities under a file or directory path
    Entities {
        /// File or directory path to extract entities from (defaults to .)
        #[arg()]
        path: Option<String>,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
        #[arg(long)]
        json: bool,
    },
    /// Show token-budgeted context for an entity
    Context {
        /// Name of the entity
        #[arg(required_unless_present = "entity_id")]
        entity: Option<String>,

        /// Look up entity by its ID (from sem diff --format json output)
        #[arg(long)]
        entity_id: Option<String>,

        /// File containing the entity (disambiguates if multiple matches)
        #[arg(long)]
        file: Option<String>,

        /// Token budget
        #[arg(long, default_value = "8000")]
        budget: usize,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
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
        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
        #[arg(long)]
        json: bool,

        /// Compare working tree vs HEAD, find broken callers from signature changes
        #[arg(long)]
        diff: bool,

        /// Only include files with these extensions (e.g. --file-exts .py .rs)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,
    },
    /// Show lifetime diff statistics
    Stats,
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

/// Resolve --format / --json into a single bool.
fn resolve_json(format: Option<String>, json: bool) -> bool {
    if let Some(f) = format {
        f == "json"
    } else {
        json
    }
}

fn combine_diff_positionals(mut args: Vec<String>, pathspecs: Vec<String>) -> Vec<String> {
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs);
    }
    args
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
            label,
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
            json,
            profile,
            file_exts,
            no_cosmetics,
            color,
            directory,
            pathspecs,
        }) => {
            apply_color_mode(color);

            let cwd = directory.unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

            let effective_format = if json { OutputFormat::Json } else { format };
            let args = combine_diff_positionals(args, pathspecs);

            diff_command(DiffOptions {
                cwd,
                format: effective_format,
                staged: staged || cached,
                commit,
                from,
                to,
                stdin,
                patch,
                verbose,
                profile,
                file_exts,
                no_cosmetics,
                label,
                args,
            });
        }
        Some(Commands::Graph {
            path,
            format,
            json,
            file_exts,
            no_cache,
            no_default_excludes,
        }) => {
            let cwd = if path == "." {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            } else {
                path
            };

            graph_command(GraphOptions {
                cwd,
                json: resolve_json(format, json),
                file_exts,
                no_cache,
                no_default_excludes,
            });
        }
        Some(Commands::Blame { file, format, json }) => {
            blame_command(BlameOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                file_path: file,
                json: resolve_json(format, json),
            });
        }
        Some(Commands::Impact {
            entity,
            entity_id,
            file,
            deps,
            dependents,
            tests,
            format,
            json,
            file_exts,
            depth,
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
                entity_id,
                file_hint: file,
                json: resolve_json(format, json),
                file_exts,
                mode,
                depth,
                no_cache,
            });
        }
        Some(Commands::Log {
            entity,
            file,
            limit,
            format,
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
                json: resolve_json(format, json),
                verbose,
            });
        }
        Some(Commands::Entities { path, format, json }) => {
            entities_command(EntitiesOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                path,
                json: resolve_json(format, json),
            });
        }
        Some(Commands::Context {
            entity,
            entity_id,
            file,
            budget,
            format,
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
                entity_id,
                file_path: file,
                budget,
                json: resolve_json(format, json),
                file_exts,
                no_cache,
            });
        }
        Some(Commands::Verify {
            format,
            json,
            diff,
            file_exts,
        }) => {
            verify_command(VerifyOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                json: resolve_json(format, json),
                diff,
                file_exts,
            });
        }
        Some(Commands::Stats) => {
            commands::stats::run();
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
                no_cosmetics: false,
                label: None,
                args: vec![],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_command(argv: &[&str]) -> Commands {
        Cli::try_parse_from(argv).unwrap().command.unwrap()
    }

    #[test]
    fn diff_accepts_flags_after_ref_positionals() {
        match parse_command(&[
            "sem",
            "diff",
            "HEAD",
            "--json",
            "--staged",
            "--no-cosmetics",
            "--verbose",
        ]) {
            Commands::Diff {
                args,
                pathspecs,
                json,
                staged,
                no_cosmetics,
                verbose,
                ..
            } => {
                assert_eq!(args, ["HEAD"]);
                assert!(pathspecs.is_empty());
                assert!(json);
                assert!(staged);
                assert!(no_cosmetics);
                assert!(verbose);
            }
            _ => panic!("expected diff command"),
        }
    }

    #[test]
    fn diff_accepts_format_after_file_positionals() {
        match parse_command(&["sem", "diff", "a.ts", "b.ts", "--format", "json"]) {
            Commands::Diff {
                args,
                pathspecs,
                format,
                ..
            } => {
                assert_eq!(args, ["a.ts", "b.ts"]);
                assert!(pathspecs.is_empty());
                assert!(matches!(format, OutputFormat::Json));
            }
            _ => panic!("expected diff command"),
        }
    }

    #[test]
    fn diff_keeps_pathspecs_after_separator_distinct() {
        match parse_command(&[
            "sem",
            "diff",
            "HEAD",
            "--json",
            "--",
            "pkg/a.py",
            "--literal",
        ]) {
            Commands::Diff {
                args,
                pathspecs,
                json,
                ..
            } => {
                assert_eq!(args, ["HEAD"]);
                assert_eq!(pathspecs, ["pkg/a.py", "--literal"]);
                assert!(json);

                let combined = combine_diff_positionals(args, pathspecs);
                assert_eq!(combined, ["HEAD", "--", "pkg/a.py", "--literal"]);
            }
            _ => panic!("expected diff command"),
        }
    }
}
