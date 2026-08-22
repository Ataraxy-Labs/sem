mod build_cache;
mod corpus_columns;
mod commands;
mod formatters;
mod hyperlinks;
mod progress;
mod stats;
mod telemetry;
mod timings;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
use commands::log::{history_command, log_command, HistoryOptions, LogOptions};

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
    #[command(long_about = "Show semantic diff of changes (supports git diff syntax). Untracked files are excluded, matching git behavior.\n\n\
        Cloud upload (when this repo has cloud consent — `sem cloud enable`/`share`, or SEM_CLOUD=1):\n\
        the local diff above is always computed and printed first, unaffected by anything below. If \
        consent is on, the diff is then uploaded to sem cloud immediately, WITHOUT first computing \
        caller/callee relations locally — relations are the slow part on cold/large repos, and the \
        upload should never wait on them. What happens next depends on the server's response:\n\
        \x20 - it queued its own relations enrichment: sem prints a one-line note and exits, no local \
        graph work at all;\n\
        \x20 - it can't compute them for this repo (private/unregistered — the consent boundary), or \
        it's an older server that doesn't know about this flow yet: sem runs the same budgeted local \
        relations pass as before and attaches the result afterward, printing what happened either way.\n\
        \x20 The budget is adaptive to repo size by default: 90s up to ~10k tracked files, ramping \
        linearly to an 8min cap at ~100k+ tracked files (tracked-file count read from the git index, \
        no repo walk). Set SEM_RELATIONS_BUDGET_MS to override with an exact value in milliseconds — \
        it always wins over the adaptive curve.\n\
        Set SEM_RELATIONS_LOCAL=1 to always compute relations locally before uploading (the old, \
        single-upload behavior), instead of the above. With cloud consent off, none of this runs: no \
        network, no relations upload, identical to running with no cloud account at all.")]
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
        /// Name of the entity to analyze, optionally as "type name"
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

        /// Include files and directories excluded by default (generated, fixtures, vendor, benchmarks)
        #[arg(long)]
        no_default_excludes: bool,
    },
    /// Find entity definitions by name — answers from the mmap query index
    /// when one exists (cold-process, <10ms on a large repo); falls back to
    /// a fresh build otherwise, which then leaves an index for next time.
    Find {
        /// Entity name, optionally as "type name" (e.g. "function createProgram")
        query: String,

        /// Restrict to entities defined in this file
        #[arg(long)]
        file: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show direct callers of an entity (who calls/references it) — the
    /// index's reverse postings, same freshness/fallback discipline as `find`.
    Callers {
        /// Entity name or id
        query: String,

        /// Disambiguate by defining file
        #[arg(long)]
        file: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show direct refs of an entity (what it calls/references) — the
    /// index's forward postings, same freshness/fallback discipline as `find`.
    Refs {
        /// Entity name or id
        query: String,

        /// Disambiguate by defining file
        #[arg(long)]
        file: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Search file text — rg-compatible `file:line:text` output, served from
    /// the mmap query index's trigram postings when one exists (cold
    /// process, target <50ms on a large repo); falls back to a plain scan
    /// otherwise. Pattern is always a regex (same default as rg without
    /// `-F`); patterns with no usable trigram (e.g. `-i`, short literals,
    /// unconstrained alternation) degrade to an honest full scan rather than
    /// a wrong answer.
    Grep {
        /// Regex or literal pattern
        pattern: String,

        /// Case-insensitive match (disables the trigram prefilter)
        #[arg(long, short = 'i')]
        ignore_case: bool,

        /// Output as JSON (one object: hits, candidate_files, total_files, origin)
        #[arg(long)]
        json: bool,
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

        /// Include files and directories excluded by default (generated, fixtures, vendor, benchmarks)
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
    /// Internal plumbing for agent-harness hooks (hidden)
    #[command(hide = true)]
    Hook {
        /// Hook kind, e.g. prompt-submit
        kind: String,
    },
    /// Show evolution of an entity through git history, or, with no entity,
    /// the repo's history analytics: hotspots and co-change pairs
    Log {
        /// Name of the entity to trace (omit for repo hotspots + co-changes)
        #[arg()]
        entity: Option<String>,

        /// File containing the entity (auto-detected if omitted)
        #[arg(long)]
        file: Option<String>,

        /// Maximum number of commits to scan (0 = unlimited)
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
    /// List entities under one or more file or directory paths
    Entities {
        /// File or directory paths to extract entities from (defaults to .)
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
        #[arg(long)]
        json: bool,

        /// Include files and directories excluded by default (generated, fixtures, vendor, benchmarks)
        #[arg(long)]
        no_default_excludes: bool,

        /// Only include files with these extensions (e.g. --file-exts .ts .tsx)
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// List only entities of these kinds (repeatable), e.g. --only function --only struct.
        /// Kinds are language-dependent; an unknown kind reports the kinds found.
        #[arg(long = "only", value_name = "KIND")]
        only_kinds: Vec<String>,

        /// List all entities except these kinds (repeatable), e.g. --except import.
        /// Cannot be combined with --only.
        #[arg(long = "except", value_name = "KIND", conflicts_with = "only_kinds")]
        except_kinds: Vec<String>,

        /// Search entity bodies for an exact substring instead of listing:
        /// hits come back entity-addressed (file, innermost entity, line,
        /// matched text). Use instead of grep for strings in code.
        #[arg(long, value_name = "SUBSTRING")]
        text: Option<String>,
    },
    /// Show token-budgeted context for an entity
    Context {
        /// Name of the entity, optionally as "type name"
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

        /// Bound related entities to this many graph hops from the target (0 = unbounded)
        #[arg(long, default_value = "0")]
        hops: usize,

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

        /// Include files and directories excluded by default (generated, fixtures, vendor, benchmarks)
        #[arg(long)]
        no_default_excludes: bool,
    },
    /// Show lifetime diff statistics
    Stats,
    /// Start the MCP server (stdin/stdout transport)
    Mcp {
        /// Removed: used to spawn the
        /// per-repo sidecar socket. The mmap query index answers cold in
        /// 6-7ms, deleting the sidecar's reason to exist. Kept as a
        /// backward-compatible no-op (exits immediately, does nothing) so an
        /// existing `sem setup` SessionStart hook that still invokes
        /// `sem mcp --resident` doesn't error.
        #[arg(long, hide = true)]
        resident: bool,
    },
    /// Replace `git diff` with `sem diff` globally
    Setup,
    /// Restore default `git diff` behavior
    Unsetup,
    /// Log in to sem cloud
    Login {
        /// API key (omit to log in with GitHub)
        #[arg()]
        key: Option<String>,
        /// API endpoint
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Log out of sem cloud
    Logout,
    /// Show current sem cloud identity
    Whoami,
    /// Manage cloud acceleration for a repo (off until you enable it)
    Cloud {
        #[command(subcommand)]
        action: CloudAction,
    },
    /// Attach an agent to a sem-cloud code review
    Review {
        #[command(subcommand)]
        action: ReviewAction,
    },
    /// Control anonymous usage telemetry (local by default — nothing uploaded)
    Telemetry {
        #[command(subcommand)]
        action: TelemetryAction,
    },
    /// Show cross-repo dependencies across your indexed repos (requires sem login)
    Xref {
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show where your code is stored: repos indexed on your cloud account and local entity caches
    Repos {
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Update sem to the latest released version
    Update,
    /// Generate shell completions
    Completions {
        /// The shell to generate the completions for
        #[arg(value_enum)]
        shell: clap_complete_command::Shell,
    },
    /// Flush spooled telemetry (internal; spawned in the background)
    #[command(name = "__telemetry-flush", hide = true)]
    TelemetryFlush,
    /// Refresh the cached latest-version info (internal; spawned in the background)
    #[command(name = "__update-check", hide = true)]
    UpdateCheck,
}

#[derive(Subcommand)]
enum CloudAction {
    /// Enable cloud queries for this public repo (shows what's sent, asks first)
    Enable,
    /// Share this private repo's index with the cloud (extra confirmation)
    Share,
    /// List every repo indexed under your account
    List,
    /// Show cloud + telemetry state for this repo (offline; sends nothing)
    Status,
    /// Print the exact request a cloud query would send
    Preview,
    /// Print the local ledger of every outbound cloud request
    Log,
    /// Stop offering cloud for this repo (or suppress the tip globally)
    Never,
    /// Delete this repo's cloud index and unregister it
    Forget,
}

#[derive(Subcommand)]
enum ReviewAction {
    /// Join a sem-cloud code review as a live listener (the one-command agent attach)
    #[command(long_about = "Join a sem-cloud code review as a live listener: resolves credentials, \
        validates the diff exists, and execs `claude` pre-configured with the sem-review-listener \
        plugin so the session joins the review and answers reviewer questions in a loop for as long \
        as it runs.\n\n\
        Accepts either a bare diff id or a hosted review URL (…/diffs/{id}, optionally with a \
        trailing /canvas and a query string or fragment).\n\n\
        Environment:\n  \
        SEM_CLOUD_URL, SEM_API_KEY   Override ~/.sem/credentials.json (same precedence as every \
                                     other cloud command).\n  \
        SEM_LISTENER_MODEL          Model for the listener session (default: claude-opus-5).\n  \
        SEM_LISTENER_PLUGIN_DIR     Override the claude-review-listener plugin directory \
                                     (auto-detected relative to the sem binary otherwise).")]
    Listen {
        /// Diff id, or a hosted review URL (…/diffs/{id}, optionally /canvas)
        diff_id_or_url: String,
        /// Print the assembled command and environment (secrets masked) instead of launching
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum TelemetryAction {
    /// Record usage locally and upload it to help improve sem
    On,
    /// Record usage locally only; never upload (the default)
    Local,
    /// Record nothing
    Off,
    /// Show the current mode and what would be sent
    Preview,
}

/// Command name recorded in anonymous usage telemetry. Names only — no
/// arguments, paths, or repo information.
fn telemetry_command_name(command: &Option<Commands>) -> Option<&'static str> {
    Some(match command {
        Some(Commands::Diff { .. }) => "diff",
        Some(Commands::Impact { .. }) => "impact",
        Some(Commands::Graph { .. }) => "graph",
        Some(Commands::Blame { .. }) => "blame",
        Some(Commands::Hook { .. }) => "hook",
        Some(Commands::Log { .. }) => "log",
        Some(Commands::Entities { .. }) => "entities",
        Some(Commands::Find { .. }) => "find",
        Some(Commands::Callers { .. }) => "callers",
        Some(Commands::Refs { .. }) => "refs",
        Some(Commands::Grep { .. }) => "grep",
        Some(Commands::Context { .. }) => "context",
        Some(Commands::Stats) => "stats",
        Some(Commands::Mcp { .. }) => "mcp",
        Some(Commands::Setup) => "setup",
        Some(Commands::Unsetup) => "unsetup",
        Some(Commands::Login { .. }) => "login",
        Some(Commands::Logout) => "logout",
        Some(Commands::Whoami) => "whoami",
        Some(Commands::Cloud { .. }) => "cloud",
        Some(Commands::Review { .. }) => "review",
        Some(Commands::Telemetry { .. }) => "telemetry",
        Some(Commands::Xref { .. }) => "xref",
        Some(Commands::Repos { .. }) => "repos",
        Some(Commands::Update) => "update",
        Some(Commands::Completions { .. }) => "completions",
        Some(Commands::TelemetryFlush) | Some(Commands::UpdateCheck) => return None,
        None => "diff",
    })
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

    if let Some(name) = telemetry_command_name(&cli.command) {
        telemetry::record(name);
        commands::update::maybe_notify(name);
    }

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
            no_default_excludes,
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
                no_default_excludes,
            });
        }
        Some(Commands::Find { query, file, json }) => {
            commands::query::find_command(commands::query::QueryOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                query,
                file,
                json,
            });
        }
        Some(Commands::Callers { query, file, json }) => {
            commands::query::callers_command(commands::query::QueryOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                query,
                file,
                json,
            });
        }
        Some(Commands::Refs { query, file, json }) => {
            commands::query::refs_command(commands::query::QueryOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                query,
                file,
                json,
            });
        }
        Some(Commands::Grep {
            pattern,
            ignore_case,
            json,
        }) => {
            commands::grep::grep_command(commands::grep::GrepOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                pattern,
                case_insensitive: ignore_case,
                json,
            });
        }
        Some(Commands::Hook { kind }) => {
            if kind == "prompt-submit" {
                commands::hook::prompt_submit();
            }
        }
        Some(Commands::Log {
            entity,
            file,
            limit,
            format,
            json,
            verbose,
        }) => {
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            match entity {
                Some(entity) => log_command(LogOptions {
                    cwd,
                    entity_name: entity,
                    file_path: file,
                    limit,
                    json: resolve_json(format, json),
                    verbose,
                }),
                // No entity: repo-level history analytics (hotspots + co-changes).
                None => history_command(HistoryOptions {
                    cwd,
                    file_path: file,
                    limit,
                    json: resolve_json(format, json),
                }),
            }
        }
        Some(Commands::Entities {
            paths,
            format,
            json,
            no_default_excludes,
            file_exts,
            only_kinds,
            except_kinds,
            text,
        }) => {
            entities_command(EntitiesOptions {
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                paths,
                json: resolve_json(format, json),
                no_default_excludes,
                file_exts,
                only_kinds,
                except_kinds,
                text,
            });
        }
        Some(Commands::Context {
            entity,
            entity_id,
            file,
            budget,
            hops,
            format,
            json,
            file_exts,
            no_cache,
            no_default_excludes,
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
                hops,
                json: resolve_json(format, json),
                file_exts,
                no_cache,
                no_default_excludes,
            });
        }
        Some(Commands::Stats) => {
            commands::stats::run();
        }
        Some(Commands::Mcp { resident }) => {
            if resident {
                // No-op: see the `resident` field's doc comment above.
                return;
            }
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
        Some(Commands::Login { key, endpoint }) => {
            let result = commands::cloud::login(key, endpoint);
            if let Err(e) = result {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Logout) => {
            if let Err(e) = commands::cloud::logout() {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Whoami) => {
            if let Err(e) = commands::cloud::whoami() {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Cloud { action }) => {
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            match action {
                CloudAction::Enable => commands::consent::enable(&cwd),
                CloudAction::Share => commands::consent::share(&cwd),
                CloudAction::List => commands::consent::list(&cwd),
                CloudAction::Status => commands::consent::status(&cwd),
                CloudAction::Preview => commands::consent::preview(&cwd),
                CloudAction::Log => commands::consent::log(),
                CloudAction::Never => commands::consent::never(&cwd),
                CloudAction::Forget => commands::consent::forget(&cwd),
            }
        }
        Some(Commands::Review { action }) => match action {
            ReviewAction::Listen { diff_id_or_url, dry_run } => {
                if let Err(e) = commands::review::listen(&diff_id_or_url, dry_run) {
                    eprintln!("{} {}", "error:".red().bold(), e);
                    std::process::exit(1);
                }
            }
        },
        Some(Commands::Telemetry { action }) => match action {
            TelemetryAction::On => telemetry::set_mode("on"),
            TelemetryAction::Local => telemetry::set_mode("local"),
            TelemetryAction::Off => telemetry::set_mode("off"),
            TelemetryAction::Preview => telemetry::preview(),
        },
        Some(Commands::Xref { json }) => {
            if let Err(e) = commands::cloud::xref(json) {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Repos { json }) => {
            if let Err(e) = commands::repos::run(json) {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Update) => {
            if let Err(e) = commands::update::run() {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Completions { shell }) => {
            shell.generate(&mut Cli::command(), &mut std::io::stdout());
        }
        Some(Commands::TelemetryFlush) => {
            telemetry::flush();
        }
        Some(Commands::UpdateCheck) => {
            commands::update::background_check();
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
