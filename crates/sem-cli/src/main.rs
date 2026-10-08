mod alias;
mod build_cache;
mod commands;
mod corpus_columns;
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
use commands::imports::{imports_command, ImportsOptions};
use commands::log::{history_command, log_command, HistoryOptions, LogOptions};

const ABOUT: &str = "sem: entity-level code intelligence for git repos (functions, classes and the calls between them)";

const QUICKSTART: &str = "\
QUICKSTART
  where is it?                  sem find parseConfig      sem grep 'retry budget'
  what does my change touch?    sem impact parseConfig    sem impact --diff HEAD --tests
  is it correct?                sem check
  what should a human review?   sem certify main..HEAD

Every verb takes --json. `sem <verb> --help` shows each flag with an example.";

#[derive(Parser)]
#[command(
    name = "sem",
    version = env!("CARGO_PKG_VERSION"),
    about = ABOUT,
    after_help = QUICKSTART
)]
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
    /// Which functions and classes changed? An entity-level `git diff`
    #[command(
        display_order = 6,
        long_about = "Which functions and classes changed? An entity-level `git diff`.\n\n\
        Examples:\n  sem diff                  uncommitted changes\n  sem diff --staged\n  \
        sem diff main..HEAD --json\n\n\
        Show semantic diff of changes (supports git diff syntax). Untracked files are excluded, matching git behavior.\n\n\
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
        network, no relations upload, identical to running with no cloud account at all."
    )]
    Diff {
        /// Display path label for direct file comparison
        #[arg(long, hide = true)]
        label: Option<String>,

        /// Git refs, files, or pathspecs (supports ref1..ref2, ref1...ref2, -- paths). Example: sem diff main..HEAD
        #[arg(num_args = 0.., value_name = "ARG")]
        args: Vec<String>,

        /// Show only staged changes (alias: --cached). Example: sem diff --staged
        #[arg(long)]
        staged: bool,

        /// Show only staged changes (alias for --staged). Example: sem diff --cached
        #[arg(long)]
        cached: bool,

        /// Show changes from a specific commit. Example: sem diff --commit abc1234
        #[arg(long)]
        commit: Option<String>,

        /// Start of commit range. Example: sem diff --from HEAD~5 --to HEAD
        #[arg(long)]
        from: Option<String>,

        /// End of commit range. Example: sem diff --from HEAD~5 --to HEAD
        #[arg(long)]
        to: Option<String>,

        /// Read FileChange[] JSON from stdin instead of git. Example: cat changes.json | sem diff --stdin
        #[arg(long)]
        stdin: bool,

        /// Read unified diff from stdin. Example: git diff | sem diff --patch
        #[arg(long)]
        patch: bool,

        /// Output format: terminal, json, plain, markdown. Example: sem diff --format markdown
        #[arg(long, default_value = "terminal")]
        format: OutputFormat,

        /// Shorthand for --format json. Example: sem diff --json
        #[arg(long)]
        json: bool,

        /// Show inline content diffs for each entity. Example: sem diff -v
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Show internal timing profile
        #[arg(long, hide = true)]
        profile: bool,

        /// Only include files with these extensions. Example: sem diff --file-exts .py .rs
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Hide cosmetic changes (formatting, whitespace, comments only). Example: sem diff --no-cosmetics
        #[arg(long)]
        no_cosmetics: bool,

        /// When to use colors. Example: sem diff --color never
        #[arg(long, default_value = "auto")]
        color: ColorMode,

        /// Run as if started in this directory (like git -C). Example: sem diff -C ../other-repo
        #[arg(short = 'C', long = "cwd")]
        directory: Option<String>,

        /// Pathspecs for filtering, passed after --. Example: sem diff HEAD -- src/
        #[arg(last = true, allow_hyphen_values = true, value_name = "PATHSPEC")]
        pathspecs: Vec<String>,
    },
    /// What does changing this entity, or this diff, touch? Dependents, deps and the tests to run
    #[command(
        display_order = 3,
        long_about = "What does changing this entity, or this diff, touch? Its dependencies, \
        its dependents (transitively), and the tests that reach it.\n\n\
        Examples:\n  sem impact parseConfig                  everything parseConfig's change can reach\n  \
        sem impact parseConfig --tests          only the tests to run\n  \
        sem impact --diff HEAD --tests          tests for the uncommitted change\n  \
        sem impact --diff main..HEAD --json     one impact report per changed entity\n\n\
        With --diff and --tests in a JS/TS workspace, the answer is the module graph's exact \
        affected-test selection over the changed files; elsewhere it is the entity graph's."
    )]
    Impact {
        /// Name of the entity to analyze, optionally as "type name". Example: sem impact "function parseConfig"
        #[arg(required_unless_present_any = ["entity_id", "diff"], conflicts_with = "diff")]
        entity: Option<String>,

        /// The impact of a whole change instead of one entity: a git range (A..B, A...B) or one
        /// ref compared to the working tree. Example: sem impact --diff main..HEAD
        #[arg(long, value_name = "RANGE", conflicts_with = "entity_id")]
        diff: Option<String>,

        /// Look up entity by its ID (from sem diff --format json output). Example: --entity-id 'src/a.ts::function::parse'
        #[arg(long)]
        entity_id: Option<String>,

        /// File containing the entity (disambiguates if multiple matches). Example: --file src/config.ts
        #[arg(long)]
        file: Option<String>,

        /// Show direct dependencies only. Example: sem impact parseConfig --deps
        #[arg(long)]
        deps: bool,

        /// Show direct dependents only. Example: sem impact parseConfig --dependents
        #[arg(long)]
        dependents: bool,

        /// Show only the tests to run. Example: sem impact --diff HEAD --tests
        #[arg(long)]
        tests: bool,

        /// Output format (terminal or json). Example: --format json
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json). Example: sem impact parseConfig --json
        #[arg(long)]
        json: bool,

        /// Only include files with these extensions. Example: --file-exts .py .rs
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Max traversal depth for transitive impact (default 2, 0 = unlimited). Example: --depth 0
        #[arg(long, default_value = "2")]
        depth: usize,

        /// Skip the SQLite entity cache (rebuild from scratch). Example: --no-cache
        #[arg(long)]
        no_cache: bool,

        /// Include files and directories excluded by default (generated, fixtures, vendor, benchmarks). Example: --no-default-excludes
        #[arg(long)]
        no_default_excludes: bool,
    },
    /// Where is it defined? With --callers: who calls it; --refs: what it uses; --context: its code in context
    #[command(
        display_order = 1,
        long_about = "Where is it defined? Find entity definitions by name (functions, classes, methods, \
        keys). The same verb answers who calls it, what it uses, and shows its code with the \
        code around it; with no name, --in lists the entities under a path.\n\n\
        Examples:\n  sem find parseConfig                     where parseConfig is defined\n  \
        sem find parseConfig loadEnv             several names in one call\n  \
        sem find parseConfig --callers           who calls it (exact or marked incomplete)\n  \
        sem find parseConfig --refs              what it calls and references\n  \
        sem find parseConfig --context           its code, plus its callers and callees, in a token budget\n  \
        sem find --in src/config.ts              every entity in a file or directory\n  \
        sem find parseConfig --in src/           only definitions under src/\n\n\
        Answers come from the query index when one exists (milliseconds); otherwise sem builds it."
    )]
    #[command(group = clap::ArgGroup::new("find_mode").args(["callers", "refs", "context"]))]
    Find {
        /// Entity name(s), each optionally as "type name" (e.g. "function parseConfig").
        /// Several names run as one batch. Example: sem find parseConfig loadEnv
        #[arg(num_args = 0.., value_name = "NAME")]
        queries: Vec<String>,

        /// Who calls it? Direct callers of one entity. Example: sem find parseConfig --callers
        #[arg(long)]
        callers: bool,

        /// What does it use? Direct references of one entity. Example: sem find parseConfig --refs
        #[arg(long)]
        refs: bool,

        /// Its code, callers and callees, packed into a token budget. Example: sem find parseConfig --context
        #[arg(long)]
        context: bool,

        /// Only in this file or directory; with no name, list the entities there (repeatable).
        /// Example: sem find --in src/config.ts
        #[arg(long = "in", value_name = "PATH")]
        in_paths: Vec<String>,

        /// Restrict to entities defined in this file (same as --in with one path)
        #[arg(long, hide = true)]
        file: Option<String>,

        /// Output as JSON. Example: sem find parseConfig --json
        #[arg(long)]
        json: bool,

        /// Output format (terminal or json)
        #[arg(long, value_parser = ["terminal", "json"], hide = true)]
        format: Option<String>,

        /// With --callers: show at most this many callers. Example: sem find parseConfig --callers --limit 20
        #[arg(long, requires = "callers")]
        limit: Option<usize>,

        /// With --context: token budget (default 8000). Example: sem find parseConfig --context --budget 2000
        #[arg(long, requires = "context")]
        budget: Option<usize>,

        /// With --context: only related entities within this many graph hops (0 = no bound).
        /// Example: sem find parseConfig --context --hops 1
        #[arg(long, requires = "context")]
        hops: Option<usize>,

        /// With --context: each entity's header (signature and first doc line) instead of its body.
        /// Example: sem find parseConfig --context --headers
        #[arg(long, requires = "context")]
        headers: bool,

        /// Look up by entity id (from sem diff --json). Example: sem find --context --entity-id 'src/a.ts::function::parse'
        #[arg(long)]
        entity_id: Option<String>,

        /// Listing: only entities of these kinds (repeatable). Example: sem find --in src --only function
        #[arg(long = "only", value_name = "KIND")]
        only_kinds: Vec<String>,

        /// Listing: every kind except these (repeatable). Example: sem find --in src --except import
        #[arg(long = "except", value_name = "KIND", conflicts_with = "only_kinds")]
        except_kinds: Vec<String>,

        /// Search entity bodies for an exact substring; hits name the entity that holds them.
        /// Example: sem find --text 'retry budget' --in src
        #[arg(long, value_name = "SUBSTRING")]
        text: Option<String>,

        /// Listing: show each entity's signature and first doc line. Example: sem find --in src/config.ts --signatures
        #[arg(long)]
        signatures: bool,

        /// Only include files with these extensions
        #[arg(long, num_args = 1.., hide = true)]
        file_exts: Vec<String>,

        /// Skip the SQLite entity cache (rebuild from scratch)
        #[arg(long, hide = true)]
        no_cache: bool,

        /// Include files and directories excluded by default (generated, fixtures, vendor, benchmarks)
        #[arg(long, hide = true)]
        no_default_excludes: bool,
    },
    /// Show direct callers of an entity (who calls/references it) — the
    /// index's reverse postings, same freshness/fallback discipline as `find`.
    #[command(hide = true)]
    Callers {
        /// Entity name or id. Must resolve to exactly one definition —
        /// an ambiguous name is refused with the candidate list.
        query: String,

        /// Disambiguate by defining file
        #[arg(long)]
        file: Option<String>,

        /// Show at most this many callers (all by default)
        #[arg(long)]
        limit: Option<usize>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show direct refs of an entity (what it calls/references) — the
    /// index's forward postings, same freshness/fallback discipline as `find`.
    #[command(hide = true)]
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
    /// Where does this text appear? Regex search, rg-style `file:line:text`
    ///
    /// Examples:
    ///   sem grep 'retry budget'            every line containing it
    ///   sem grep -i todo src/              case-insensitive, under src/
    ///   sem grep -e foo -e bar --json      two patterns, hits kept apart
    ///
    /// Search file text — rg-compatible `file:line:text` output, served from
    /// the mmap query index's trigram postings when one exists (cold
    /// process, target <50ms on a large repo); falls back to a plain scan
    /// otherwise. Pattern is always a regex (same default as rg without
    /// `-F`); patterns with no usable trigram (e.g. `-i`, short literals,
    /// unconstrained alternation) degrade to an honest full scan rather than
    /// a wrong answer.
    #[command(display_order = 2)]
    Grep {
        /// Regex or literal pattern. Example: sem grep 'retry budget'
        #[arg(required_unless_present = "patterns")]
        pattern: Option<String>,

        /// Pattern to search for, repeatable (rg-style `-e p1 -e p2`); each
        /// pattern's hits are reported separately rather than merged. Example: sem grep -e foo -e bar
        #[arg(long = "regexp", short = 'e')]
        patterns: Vec<String>,

        /// Case-insensitive match (disables the trigram prefilter). Example: sem grep -i todo
        #[arg(long, short = 'i')]
        ignore_case: bool,

        /// Only report hits under these files or directories (rg-style
        /// trailing paths, relative to the current directory). Example: sem grep todo src/ tests/
        #[arg(value_name = "PATH")]
        paths: Vec<String>,

        /// Print only the paths of files with at least one hit (rg -l). Example: sem grep -l todo
        #[arg(long = "files-with-matches", short = 'l')]
        files_with_matches: bool,

        /// Accepted for rg/grep compatibility: line numbers are always shown
        #[arg(long = "line-number", short = 'n', hide = true)]
        line_number: bool,

        /// Output as JSON (one object: hits, candidate_files, total_files, origin). Example: sem grep todo --json
        #[arg(long)]
        json: bool,
    },
    /// Promises about the codebase, each verified by a deterministic check
    ///
    /// A promise is a law in `.sem/promises/*.json` (the `sem topology check`
    /// format) carrying a human `"promise"`. Code-shape laws are tree-sitter
    /// queries in any language sem parses; every non-`_` capture is a
    /// violation. Example, `.sem/promises/jsx-no-logic.json`:
    ///
    ///   {"laws": [{"id": "jsx-no-logic/conditionals",
    ///     "promise": "JSX contains no conditional rendering",
    ///     "forbidPattern": {"from": ["**/*.tsx"], "within": ["jsx_expression"],
    ///                       "query": "(ternary_expression) @conditional"}}]}
    ///
    /// `sem promises check` exits 1 while any promise is broken; after an edit,
    /// `sem promises check --changed <file>` reports only that file's violations.
    #[command(verbatim_doc_comment, hide = true)]
    Promises {
        #[command(subcommand)]
        cmd: commands::promises::PromisesCmd,
    },
    /// Module topology of a JS/TS workspace: reference graph, graph math, laws
    #[command(hide = true)]
    Topology {
        #[command(subcommand)]
        cmd: commands::topology::TopologyCmd,
    },
    /// Whole-system graph: locked dependencies, stdlib, DB schema, config/routes, contracts and a
    /// runtime trace, layered, with the share of call and flow sites each layer leaves unknown
    #[command(hide = true)]
    System {
        #[command(subcommand)]
        cmd: commands::system::SystemCmd,
    },
    /// What should a human review in this commit range? The review certificate; --arch: the architecture view
    #[command(
        display_order = 5,
        long_about = "What should a human review in this commit range? The review certificate: \
        entities touched, signature changes and the callers they leave behind, callee deltas, \
        promises kept or broken (with witnesses), module reachability deltas (JS/TS), affected \
        tests, and the static reference cone. --arch adds the architecture view: new or removed \
        data paths, side effects, dependencies, cycles and what did not change, ranked.\n\n\
        Examples:\n  sem certify main..HEAD                    markdown certificate\n  \
        sem certify main..HEAD --json             the full certificate\n  \
        sem certify main..HEAD --arch             architecture delta, plain text\n  \
        sem certify main..HEAD --arch --view      at most 10 ranked items\n  \
        sem certify main..HEAD --html > view.html one self-contained page\n\n\
        A range is `<base>..<head>`; `<base>...<head>` uses the merge base; one ref means `<ref>..HEAD`."
    )]
    Certify {
        /// Commit range. Example: sem certify main..HEAD
        #[arg(required_unless_present = "from_json")]
        range: Option<String>,
        /// The architecture view of the range instead of the certificate. Example: sem certify main..HEAD --arch
        #[arg(long)]
        arch: bool,
        /// Extra laws files (the promises format), besides `.sem/promises/*.json` at head.
        /// Example: --laws laws/layers.json
        #[arg(long, num_args = 1..)]
        laws: Vec<std::path::PathBuf>,
        /// Output as JSON (the certificate; with --arch the architecture report). Example: sem certify main..HEAD --json
        #[arg(long, conflicts_with_all = ["md", "html"])]
        json: bool,
        /// The architecture view as one self-contained HTML page (implies --arch).
        /// Example: sem certify main..HEAD --html > view.html
        #[arg(long)]
        html: bool,
        /// With --arch: at most 10 ranked items, each with what changed and why it matters.
        /// Example: sem certify main..HEAD --arch --view
        #[arg(long, conflicts_with = "html")]
        view: bool,
        /// With --arch: a compact markdown summary. Example: sem certify main..HEAD --arch --md
        #[arg(long, conflicts_with_all = ["view", "html"])]
        md: bool,
        /// Items listed per section (default 8). Example: --max-items 20
        #[arg(long, default_value_t = 8)]
        max_items: usize,
        /// Cap on the markdown certificate's size, in characters
        #[arg(long, default_value_t = 9000, hide = true)]
        max_chars: usize,
        /// With --arch: render a report saved with `--arch --json` instead of analysing a range
        #[arg(long, value_name = "REPORT_JSON", hide = true)]
        from_json: Option<std::path::PathBuf>,
        /// With --arch: extra source/sink model files
        #[arg(long, num_args = 1.., hide = true)]
        models: Vec<std::path::PathBuf>,
        /// With --arch: seconds the data-flow analysis of each tree may take (0 = no limit)
        #[arg(long, default_value_t = 45, hide = true)]
        budget: u64,
        /// With --arch: resident memory (MB) data flow may reach (0 = no limit)
        #[arg(long, default_value_t = 4096, hide = true)]
        max_memory: u64,
        /// With --arch: count examples/, benches/ and test code in the dependency graph
        #[arg(long, hide = true)]
        include_examples: bool,
        /// With --arch: what is analyzed: auto, full or diff
        #[arg(long, default_value = "auto", value_parser = ["auto", "full", "diff"], hide = true)]
        scope: String,
        /// With --arch: source MB a diff-scoped region may hold
        #[arg(long, default_value_t = 16, hide = true)]
        region_mb: u64,
    },
    /// Architecture delta of a commit range, for review instead of the line diff:
    /// new/removed data paths (source -> sink) with witnesses, side-effect changes per
    /// entity, package/file dependencies, cycles, propagation cost, centrality,
    /// complexity deltas, signature changes and their callers, laws, what did NOT
    /// change, and how much the analysis could not resolve — ranked by severity
    #[command(name = "arch-diff", hide = true)]
    ArchDiff {
        /// Commit range `<base>..<head>` (`<base>...<head>` uses the merge base; a single ref means `<ref>..HEAD`)
        #[arg(required_unless_present = "from_json")]
        range: Option<String>,
        /// Output the full report as JSON (with --view: the view model as JSON)
        #[arg(long, conflicts_with_all = ["md", "html"])]
        json: bool,
        /// Output a compact markdown summary
        #[arg(long, conflicts_with_all = ["view", "html"])]
        md: bool,
        /// Output the architecture view: at most 10 ranked items, each with what changed
        /// and why it matters; the rest counted; what did not change; what sem could not resolve
        #[arg(long, conflicts_with = "html")]
        view: bool,
        /// Output the architecture view as one self-contained HTML page (module graph
        /// with new/removed edges, ranked list, evidence on click); no network assets
        #[arg(long)]
        html: bool,
        /// Render a report saved with `--json` instead of analysing a range
        #[arg(long, value_name = "REPORT_JSON")]
        from_json: Option<std::path::PathBuf>,
        /// Extra laws files (the `sem topology check` format), besides `.sem/promises/*.json` at head
        #[arg(long, num_args = 1..)]
        laws: Vec<std::path::PathBuf>,
        /// Extra source/sink model files (besides built-ins and `.sem/models/*.json` at head)
        #[arg(long, num_args = 1..)]
        models: Vec<std::path::PathBuf>,
        /// Items listed per section
        #[arg(long, default_value_t = 8)]
        max_items: usize,
        /// Seconds the data-flow analysis of each tree may take; past it the
        /// data-path results are partial and the report says so (0 = no limit)
        #[arg(long, default_value_t = 45)]
        budget: u64,
        /// Resident memory (MB) the process may reach during data flow; past it the
        /// data-path results are partial and the report says so (0 = no limit)
        #[arg(long, default_value_t = 4096)]
        max_memory: u64,
        /// Count examples/, benches/ and test code in the dependency graph and cycles
        #[arg(long)]
        include_examples: bool,
        /// What is analyzed: `full` (both whole trees), `diff` (the changed
        /// files, their callers, importers and the definitions they call), or
        /// `auto` (diff when the whole trees would not fit in --max-memory)
        #[arg(long, default_value = "auto", value_parser = ["auto", "full", "diff"])]
        scope: String,
        /// Source MB a diff-scoped region may hold; names mentioned in more
        /// files than fit are reported as not analyzed
        #[arg(long, default_value_t = 16)]
        region_mb: u64,
    },
    /// Data flow of the working tree: per-function reads/writes (env, files, DB,
    /// network, subprocess, logs, module state, fields) and source -> sink paths
    #[command(hide = true)]
    Dataflow {
        /// Repository path (defaults to current directory)
        #[arg(default_value = ".")]
        path: String,
        /// Full report as JSON (facts per entity, flows, unknowns, coverage)
        #[arg(long)]
        json: bool,
        /// Extra source/sink model files
        #[arg(long, num_args = 1..)]
        models: Vec<std::path::PathBuf>,
        /// Paths listed
        #[arg(long, default_value_t = 20)]
        max_items: usize,
        /// Emit witness-generation tasks (JSON): per static source -> sink
        /// flow, the source and sink contracts (exact spans) and the path
        /// functions a runner instruments to demonstrate it by execution
        #[arg(long)]
        witness: bool,
    },
    /// How is the code connected? The entity graph; --modules, --dataflow or --system for other layers
    #[command(
        display_order = 7,
        subcommand_help_heading = "Operations (--modules: graph..check, --system: deps, fetch, build)",
        subcommand_value_name = "OPERATION",
        long_about = "How is the code connected? With no flag, the entity dependency graph: every \
        function, class and method, and the calls and references between them.\n\n\
        --modules   the module graph of a JS/TS workspace and the math over it (cycles, domains, \
        blast radius, paths, affected tests, laws)\n\
        --dataflow  data flow: per-function reads and writes (env, files, DB, network, \
        subprocess, logs) and source -> sink paths\n\
        --system    the whole system: locked dependencies, stdlib, DB schema, config and \
        routes, layered, with how much each layer leaves unknown\n\n\
        Examples:\n  sem graph --json                          entity graph as JSON\n  \
        sem graph --modules metrics               density, cycles, depth, centrality\n  \
        sem graph --modules blast-radius pkg-a    who breaks if pkg-a changes\n  \
        sem graph --modules affected-tests src/a.ts\n  \
        sem graph --dataflow --json               data flow facts and paths\n  \
        sem graph --dataflow --witness            (experimental) witness tasks per flow\n  \
        sem graph --system                        locked dependencies\n  \
        sem graph --system build --out sys/       the layered whole-system graph"
    )]
    #[command(group = clap::ArgGroup::new("graph_layer").args(["modules", "dataflow", "system"]))]
    Graph {
        /// Repository path (defaults to current directory). Example: sem graph ../other-repo --json
        #[arg(default_value = ".")]
        path: String,

        /// The module graph of a JS/TS workspace (default operation: graph). Example: sem graph --modules cycles
        #[arg(long)]
        modules: bool,

        /// Data flow: reads, writes and source -> sink paths. Example: sem graph --dataflow
        #[arg(long)]
        dataflow: bool,

        /// Experimental. With --dataflow: emit witness-generation tasks (JSON) per static source -> sink flow.
        /// Example: sem graph --dataflow --witness
        #[arg(long, requires = "dataflow")]
        witness: bool,

        /// The whole-system graph (default operation: deps). Example: sem graph --system build --out sys/
        #[arg(long)]
        system: bool,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"], hide = true)]
        format: Option<String>,

        /// Output as JSON. Example: sem graph --json
        #[arg(long)]
        json: bool,

        /// Only include files with these extensions. Example: sem graph --json --file-exts .py .rs
        #[arg(long, num_args = 1..)]
        file_exts: Vec<String>,

        /// Skip the SQLite entity cache (rebuild from scratch)
        #[arg(long, hide = true)]
        no_cache: bool,

        /// Include files and directories excluded by default (generated, fixtures, vendor, benchmarks).
        /// Example: sem graph --json --no-default-excludes
        #[arg(long)]
        no_default_excludes: bool,

        /// With --dataflow: extra source/sink model files
        #[arg(long, num_args = 1.., hide = true)]
        models: Vec<std::path::PathBuf>,

        /// With --dataflow: paths listed
        #[arg(long, default_value_t = 20, hide = true)]
        max_items: usize,

        #[command(subcommand)]
        op: Option<GraphOp>,
    },
    /// Show semantic blame — who last modified each entity
    #[command(hide = true)]
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
    /// Is my change correct? Runs the project's checkers; --promises proves the promises can fail
    #[command(
        display_order = 4,
        long_about = "Is my change correct? Runs the project's compiler, type checker, linter and \
        tests on the working tree: the same verdict as running each tool on the whole project, \
        rechecking only what the change can affect when that is provably exact. Exit 0 pass, \
        1 fail, 2 could not decide.\n\n\
        Examples:\n  sem check                          every checker the project has\n  \
        sem check --checkers ts,lint,tests only these\n  \
        sem check --base origin/main       against origin/main instead of HEAD\n  \
        sem check --promises               also prove every promise in .sem/promises can fail\n  \
        sem check --json                   one JSON object with a verification certificate\n\n\
        Every checker reports its mode (incremental or full), why, the files it rechecked and \
        its diagnostics. With --promises the promises verdict follows the checkers' (in --json, \
        as a second JSON object), and the exit code is the worse of the two."
    )]
    Check {
        #[command(flatten)]
        args: commands::check::CheckArgs,
        /// Also prove every promise in .sem/promises can fail: apply its mutation, expect it
        /// broken, restore. Example: sem check --promises
        #[arg(long)]
        promises: bool,
    },
    /// How did this entity change over time? With --blame: who last changed each entity in a file
    #[command(
        display_order = 8,
        long_about = "How did this entity change over time? Its versions through git history, \
        logic changes told apart from cosmetic ones. With no entity: the repo's hotspots and the \
        entities that change together. With --blame: who last changed each entity in a file.\n\n\
        Examples:\n  sem history parseConfig              every version of parseConfig\n  \
        sem history parseConfig -v           with the content diff of each version\n  \
        sem history                          hotspots and co-change pairs\n  \
        sem history --blame src/config.ts    who last changed each entity in the file"
    )]
    History {
        /// Entity to trace (with --blame: the file to blame). Example: sem history parseConfig
        #[arg(value_name = "ENTITY")]
        entity: Option<String>,

        /// Who last changed each entity in a file. Example: sem history --blame src/config.ts
        #[arg(long)]
        blame: bool,

        /// File containing the entity (auto-detected if omitted). Example: --file src/config.ts
        #[arg(long)]
        file: Option<String>,

        /// Maximum number of commits to scan (0 = unlimited). Example: --limit 200
        #[arg(long, default_value = "50")]
        limit: usize,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"], hide = true)]
        format: Option<String>,

        /// Output as JSON. Example: sem history parseConfig --json
        #[arg(long)]
        json: bool,

        /// Show the content diff between versions. Example: sem history parseConfig -v
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Set up sem: git diff integration, telemetry, shell completions, updates, usage stats
    #[command(display_order = 10)]
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Internal plumbing for agent-harness hooks (hidden)
    #[command(hide = true)]
    Hook {
        /// Hook kind, e.g. prompt-submit
        kind: String,
    },
    /// Show evolution of an entity through git history, or, with no entity,
    /// the repo's history analytics: hotspots and co-change pairs
    #[command(hide = true)]
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
    #[command(hide = true)]
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

        /// Show each entity's header under its row: the signature up to the
        /// body plus the first doc-comment line — more than a name, far less
        /// than a body
        #[arg(long)]
        signatures: bool,
    },
    /// List a file's top-level import statements as the parser sees them
    /// (kind, line span, byte span, source text). Internal: pi's sem.addImport
    /// uses it to place and supersede imports by parser position rather than
    /// by a text scan, so import-shaped text in a string, comment, or nested
    /// block is never mistaken for a real import.
    #[command(hide = true)]
    Imports {
        /// File to list top-level imports for.
        path: String,

        /// Output format
        #[arg(long, value_parser = ["terminal", "json"])]
        format: Option<String>,

        /// Output as JSON (shorthand for --format json)
        #[arg(long)]
        json: bool,
    },
    /// Show token-budgeted context for an entity
    #[command(hide = true)]
    Context {
        /// Name of the entity, optionally as "type name"
        #[arg(required_unless_present_any = ["entity_id", "entities"], conflicts_with = "entities")]
        entity: Option<String>,

        /// Entity name, repeatable (--entity A --entity B) to pack context
        /// for several entities in one invocation under one --budget; each
        /// name resolves (and refuses on ambiguity) exactly like the
        /// single-entity form
        #[arg(long = "entity", conflicts_with = "entity_id")]
        entities: Vec<String>,

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

        /// Render each packed entity as its header (signature plus first
        /// doc-comment line) instead of its body — the same budget buys a
        /// much wider map
        #[arg(long)]
        headers: bool,
    },
    /// Show lifetime diff statistics
    #[command(hide = true)]
    Stats,
    /// Run sem as an MCP server for agents (stdin/stdout): find, grep, impact, check, certify, diff, graph, history
    #[command(display_order = 11)]
    Mcp {
        /// Check shared MCP daemon health without starting it (JSON). Example: sem mcp --status
        #[arg(long, conflicts_with = "resident")]
        status: bool,
        /// Also list the cloud review-listener tools (join_review, wait_for_branch, ...);
        /// `sem cloud review listen` sets this
        #[arg(long, hide = true)]
        review: bool,
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
    #[command(hide = true)]
    Setup,
    /// Restore default `git diff` behavior
    #[command(hide = true)]
    Unsetup,
    /// Log in to sem cloud
    #[command(hide = true)]
    Login {
        /// API key (omit to log in with GitHub)
        #[arg()]
        key: Option<String>,
        /// API endpoint
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Log out of sem cloud
    #[command(hide = true)]
    Logout,
    /// Show current sem cloud identity
    #[command(hide = true)]
    Whoami,
    /// sem cloud: log in, attach an agent to a review, cross-repo queries, per-repo cloud on/off
    #[command(display_order = 9)]
    Cloud {
        #[command(subcommand)]
        action: CloudAction,
    },
    /// Attach an agent to a sem-cloud code review
    #[command(hide = true)]
    Review {
        #[command(subcommand)]
        action: ReviewAction,
    },
    /// Control anonymous usage telemetry (local by default — nothing uploaded)
    #[command(hide = true)]
    Telemetry {
        #[command(subcommand)]
        action: TelemetryAction,
    },
    /// Show cross-repo dependencies across your indexed repos (requires sem login)
    #[command(hide = true)]
    Xref {
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show where your code is stored: repos indexed on your cloud account and local entity caches
    #[command(hide = true)]
    Repos {
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Update sem to the latest released version
    #[command(hide = true)]
    Update,
    /// Generate shell completions
    #[command(hide = true)]
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
    /// Log in to sem cloud (an API key, or GitHub when omitted). Example: sem cloud login
    Login {
        /// API key (omit to log in with GitHub). Example: sem cloud login sem_live_abc123
        #[arg()]
        key: Option<String>,
        /// API endpoint. Example: --endpoint https://sem.example.org
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Log out of sem cloud. Example: sem cloud logout
    Logout,
    /// Who am I logged in as? Example: sem cloud whoami
    Whoami,
    /// Attach an agent to a sem cloud code review. Example: sem cloud review listen <diff-id>
    Review {
        #[command(subcommand)]
        action: ReviewAction,
    },
    /// What depends on what across my indexed repos? Example: sem cloud xref --json
    Xref {
        /// JSON output. Example: --json
        #[arg(long)]
        json: bool,
    },
    /// Where is my code stored? Repos indexed on the account and local caches. Example: sem cloud repos
    Repos {
        /// JSON output. Example: --json
        #[arg(long)]
        json: bool,
    },
    /// Enable cloud queries for this public repo (shows what's sent, asks first). Example: sem cloud enable
    Enable,
    /// Stop cloud for this repo (or suppress the cloud tip everywhere). Example: sem cloud disable
    Disable,
    /// Share this private repo's index with the cloud (extra confirmation)
    #[command(hide = true)]
    Share,
    /// Alias of `cloud repos`
    #[command(hide = true)]
    List,
    /// Show cloud + telemetry state for this repo (offline; sends nothing)
    #[command(hide = true)]
    Status,
    /// Print the exact request a cloud query would send
    #[command(hide = true)]
    Preview,
    /// Print the local ledger of every outbound cloud request
    #[command(hide = true)]
    Log,
    /// Stop offering cloud for this repo (or suppress the tip globally)
    #[command(hide = true)]
    Never,
    /// Delete this repo's cloud index and unregister it
    #[command(hide = true)]
    Forget,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Make `git diff` show sem's entity diff, globally. Example: sem config setup
    Setup,
    /// Restore default `git diff`. Example: sem config unsetup
    Unsetup,
    /// Anonymous usage telemetry: on, local (default, nothing uploaded), off, preview.
    /// Example: sem config telemetry off
    Telemetry {
        #[command(subcommand)]
        action: TelemetryAction,
    },
    /// Print shell completions. Example: sem config completions zsh
    Completions {
        /// The shell to generate the completions for: bash, zsh, fish, elvish, powershell. Example: sem config completions fish
        #[arg(value_enum)]
        shell: clap_complete_command::Shell,
    },
    /// Update sem to the latest release. Example: sem config update
    Update,
    /// Lifetime diff statistics. Example: sem config stats
    Stats,
}

/// `sem graph` operations: the module graph's (`--modules`) and the
/// system graph's (`--system`).
#[derive(Subcommand)]
enum GraphOp {
    #[command(flatten)]
    Modules(commands::topology::TopologyCmd),
    #[command(flatten)]
    System(commands::system::SystemCmd),
}

#[derive(Subcommand)]
enum ReviewAction {
    /// Join a sem-cloud code review as a live listener (the one-command agent attach)
    #[command(
        long_about = "Join a sem-cloud code review as a live listener: resolves credentials, \
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
                                     (auto-detected relative to the sem binary otherwise)."
    )]
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
        Some(Commands::Promises { .. }) => "promises",
        Some(Commands::Topology { .. }) => "topology",
        Some(Commands::System { .. }) => "system",
        Some(Commands::Certify { .. }) => "certify",
        Some(Commands::ArchDiff { .. }) => "arch-diff",
        Some(Commands::Dataflow { .. }) => "dataflow",
        Some(Commands::Blame { .. }) => "blame",
        Some(Commands::Hook { .. }) => "hook",
        Some(Commands::Log { .. }) => "log",
        Some(Commands::Entities { .. }) => "entities",
        Some(Commands::Imports { .. }) => "imports",
        Some(Commands::Find { .. }) => "find",
        Some(Commands::Callers { .. }) => "callers",
        Some(Commands::Refs { .. }) => "refs",
        Some(Commands::Grep { .. }) => "grep",
        Some(Commands::Context { .. }) => "context",
        Some(Commands::Check { .. }) => "check",
        Some(Commands::History { .. }) => "history",
        Some(Commands::Config { .. }) => "config",
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

fn cwd_string() -> String {
    std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

/// Exits 1 with `error: <e>` (the cloud/setup family's error convention).
fn or_exit_1(result: Result<(), Box<dyn std::error::Error>>) {
    if let Err(e) = result {
        eprintln!("{} {}", "error:".red().bold(), e);
        std::process::exit(1);
    }
}

/// Exits 2 with `error: <e>` (the analysis family's error convention).
fn or_exit_2<E: std::fmt::Display>(result: Result<(), E>) {
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(2);
    }
}

fn run_callers(query: String, file: Option<String>, limit: Option<usize>, json: bool) {
    commands::query::callers_command(
        commands::query::QueryOptions { cwd: cwd_string(), query, file, json },
        limit,
    );
}

fn run_refs(query: String, file: Option<String>, json: bool) {
    commands::query::refs_command(commands::query::QueryOptions { cwd: cwd_string(), query, file, json });
}

fn run_find(queries: Vec<String>, file: Option<String>, json: bool) {
    let cwd = cwd_string();
    if queries.len() == 1 {
        commands::query::find_command(commands::query::QueryOptions {
            cwd,
            query: queries.into_iter().next().unwrap_or_default(),
            file,
            json,
        });
    } else {
        commands::query::find_multi_command(cwd, queries, file, json);
    }
}

struct ContextArgs {
    entity: Option<String>,
    entities: Vec<String>,
    entity_id: Option<String>,
    file: Option<String>,
    budget: usize,
    hops: usize,
    json: bool,
    file_exts: Vec<String>,
    no_cache: bool,
    no_default_excludes: bool,
    headers: bool,
}

fn run_context(a: ContextArgs) {
    let cwd = cwd_string();
    if a.entities.is_empty() {
        context_command(ContextOptions {
            cwd,
            entity_name: a.entity,
            entity_id: a.entity_id,
            file_path: a.file,
            budget: a.budget,
            hops: a.hops,
            json: a.json,
            file_exts: a.file_exts,
            no_cache: a.no_cache,
            no_default_excludes: a.no_default_excludes,
            headers: a.headers,
        });
    } else {
        // Batch form: one packed context per named entity, every
        // other flag (budget included) applying to each. Resolution
        // failures refuse exactly like the single-entity form.
        for entity_name in a.entities {
            context_command(ContextOptions {
                cwd: cwd.clone(),
                entity_name: Some(entity_name),
                entity_id: None,
                file_path: a.file.clone(),
                budget: a.budget,
                hops: a.hops,
                json: a.json,
                file_exts: a.file_exts.clone(),
                no_cache: a.no_cache,
                no_default_excludes: a.no_default_excludes,
                headers: a.headers,
            });
        }
    }
}

fn run_log(entity: Option<String>, file: Option<String>, limit: usize, json: bool, verbose: bool) {
    let cwd = cwd_string();
    match entity {
        Some(entity) => log_command(LogOptions {
            cwd,
            entity_name: entity,
            file_path: file,
            limit,
            json,
            verbose,
        }),
        // No entity: repo-level history analytics (hotspots + co-changes).
        None => history_command(HistoryOptions {
            cwd,
            file_path: file,
            limit,
            json,
        }),
    }
}

fn run_blame(file: String, json: bool) {
    blame_command(BlameOptions {
        cwd: cwd_string(),
        file_path: file,
        json,
    });
}

fn run_dataflow(path: &str, json: bool, models: &[std::path::PathBuf], max_items: usize, witness: bool) {
    or_exit_2(commands::arch_diff::dataflow_command(path, json, models, max_items, witness));
}

struct ArchDiffArgs {
    range: Option<String>,
    json: bool,
    md: bool,
    view: bool,
    html: bool,
    from_json: Option<std::path::PathBuf>,
    laws: Vec<std::path::PathBuf>,
    models: Vec<std::path::PathBuf>,
    max_items: usize,
    budget: u64,
    max_memory: u64,
    include_examples: bool,
    scope: String,
    region_mb: u64,
}

fn run_arch_diff(a: ArchDiffArgs) {
    let ArchDiffArgs { range, json, md, view, html, from_json, laws, models, max_items, budget, max_memory, include_examples, scope, region_mb } = a;
    let range = range.unwrap_or_default();
    let format = if json && view {
        commands::arch_diff::Format::ViewJson
    } else if json {
        commands::arch_diff::Format::Json
    } else if view {
        commands::arch_diff::Format::View
    } else if html {
        commands::arch_diff::Format::Html
    } else if md {
        commands::arch_diff::Format::Markdown
    } else {
        commands::arch_diff::Format::Text
    };
    or_exit_2(commands::arch_diff::arch_diff_command(commands::arch_diff::ArchDiffOptions {
        cwd: cwd_string(),
        range,
        laws,
        models,
        format,
        max_items,
        budget: (budget > 0).then(|| std::time::Duration::from_secs(budget)),
        max_memory: (max_memory > 0).then(|| max_memory as usize * 1024 * 1024),
        include_examples,
        scope: commands::arch_diff::scope_of(&scope, max_memory, region_mb),
        from_json,
    }));
}

fn run_graph(path: String, json: bool, file_exts: Vec<String>, no_cache: bool, no_default_excludes: bool) {
    let cwd = if path == "." { cwd_string() } else { path };
    graph_command(GraphOptions {
        cwd,
        json,
        file_exts,
        no_cache,
        no_default_excludes,
    });
}

struct EntitiesArgs {
    paths: Vec<String>,
    json: bool,
    no_default_excludes: bool,
    file_exts: Vec<String>,
    only_kinds: Vec<String>,
    except_kinds: Vec<String>,
    text: Option<String>,
    signatures: bool,
}

fn run_entities(a: EntitiesArgs) {
    entities_command(EntitiesOptions {
        cwd: cwd_string(),
        paths: a.paths,
        json: a.json,
        no_default_excludes: a.no_default_excludes,
        file_exts: a.file_exts,
        only_kinds: a.only_kinds,
        except_kinds: a.except_kinds,
        text: a.text,
        signatures: a.signatures,
    });
}

fn run_telemetry(action: TelemetryAction) {
    match action {
        TelemetryAction::On => telemetry::set_mode("on"),
        TelemetryAction::Local => telemetry::set_mode("local"),
        TelemetryAction::Off => telemetry::set_mode("off"),
        TelemetryAction::Preview => telemetry::preview(),
    }
}

fn run_review(action: ReviewAction) {
    match action {
        ReviewAction::Listen { diff_id_or_url, dry_run } => {
            or_exit_1(commands::review::listen(&diff_id_or_url, dry_run));
        }
    }
}

fn run_completions(shell: clap_complete_command::Shell) {
    shell.generate(&mut Cli::command(), &mut std::io::stdout());
}

/// Windows gives the main thread a 1 MB stack, against 8 MB on Linux and macOS.
/// Parsing and dispatching this many commands in a debug build can exceed it,
/// so the CLI runs on a thread with the stack it gets everywhere else.
fn main() {
    let worker = std::thread::Builder::new()
        .name("sem".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(sem_main)
        .expect("spawn the sem main thread");
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

fn sem_main() {
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

            let cwd = directory.unwrap_or_else(cwd_string);

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
        Some(Commands::Promises { cmd }) => {
            if let commands::promises::PromisesCmd::Verify(v) = &cmd {
                alias::note("promises verify", "check --promises", v.json);
            }
            or_exit_2(commands::promises::run(cmd));
        }
        Some(Commands::System { cmd }) => {
            alias::note("system", "graph --system", commands::system::is_json(&cmd));
            or_exit_2(commands::system::run(cmd));
        }
        Some(Commands::Topology { cmd }) => {
            // Topology always prints JSON: never a notice.
            or_exit_2(commands::topology::run(cmd));
        }
        Some(Commands::ArchDiff { range, json, md, view, html, from_json, laws, models, max_items, budget, max_memory, include_examples, scope, region_mb }) => {
            alias::note("arch-diff", "certify --arch", json);
            run_arch_diff(ArchDiffArgs { range, json, md, view, html, from_json, laws, models, max_items, budget, max_memory, include_examples, scope, region_mb });
        }
        Some(Commands::Dataflow { path, json, models, max_items, witness }) => {
            alias::note("dataflow", "graph --dataflow", json || witness);
            run_dataflow(&path, json, &models, max_items, witness);
        }
        Some(Commands::Certify {
            range,
            arch,
            laws,
            json,
            html,
            view,
            md,
            max_items,
            max_chars,
            from_json,
            models,
            budget,
            max_memory,
            include_examples,
            scope,
            region_mb,
        }) => {
            if arch || html || view || md || from_json.is_some() {
                run_arch_diff(ArchDiffArgs { range, json, md, view, html, from_json, laws, models, max_items, budget, max_memory, include_examples, scope, region_mb });
            } else {
                or_exit_2(commands::certify::certify_command(commands::certify::CertifyOptions {
                    cwd: cwd_string(),
                    range: range.unwrap_or_default(),
                    laws,
                    json,
                    max_items,
                    max_chars,
                }));
            }
        }
        Some(Commands::Graph {
            path,
            modules,
            dataflow,
            witness,
            system,
            format,
            json,
            file_exts,
            no_cache,
            no_default_excludes,
            models,
            max_items,
            op,
        }) => {
            let json = resolve_json(format, json);
            match op {
                Some(GraphOp::Modules(cmd)) => {
                    if system || dataflow {
                        eprintln!("error: this is a --modules operation");
                        std::process::exit(2);
                    }
                    or_exit_2(commands::topology::run(cmd));
                }
                Some(GraphOp::System(cmd)) => {
                    if modules || dataflow {
                        eprintln!("error: this is a --system operation");
                        std::process::exit(2);
                    }
                    or_exit_2(commands::system::run(cmd));
                }
                None if modules => {
                    or_exit_2(commands::topology::run(commands::topology::TopologyCmd::Graph(
                        commands::topology::Common::at(&path),
                    )));
                }
                None if system => {
                    or_exit_2(commands::system::run(commands::system::SystemCmd::Deps {
                        path,
                        roots: Vec::new(),
                        json,
                    }));
                }
                None if dataflow => run_dataflow(&path, json, &models, max_items, witness),
                None => run_graph(path, json, file_exts, no_cache, no_default_excludes),
            }
        }
        Some(Commands::Blame { file, format, json }) => {
            let json = resolve_json(format, json);
            alias::note("blame", "history --blame", json);
            run_blame(file, json);
        }
        Some(Commands::Impact {
            entity,
            diff,
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
            let json = resolve_json(format, json);

            if let Some(range) = diff {
                or_exit_2(commands::impact_diff::impact_diff_command(commands::impact_diff::ImpactDiffOptions {
                    cwd: cwd_string(),
                    range,
                    mode,
                    json,
                    file_exts,
                    depth,
                    no_cache,
                    no_default_excludes,
                }));
                return;
            }

            impact_command(ImpactOptions {
                cwd: cwd_string(),
                entity_name: entity,
                entity_id,
                file_hint: file,
                json,
                file_exts,
                mode,
                depth,
                no_cache,
                no_default_excludes,
            });
        }
        Some(Commands::Find {
            queries,
            callers,
            refs,
            context,
            in_paths,
            file,
            json,
            format,
            limit,
            budget,
            hops,
            headers,
            entity_id,
            only_kinds,
            except_kinds,
            text,
            signatures,
            file_exts,
            no_cache,
            no_default_excludes,
        }) => {
            let json = resolve_json(format, json);
            // --file is the old spelling of a single --in.
            let mut in_paths = in_paths;
            in_paths.extend(file);
            let listing = queries.is_empty() && entity_id.is_none();
            if !listing && in_paths.len() > 1 {
                eprintln!("error: with a name, --in takes one file or directory");
                std::process::exit(2);
            }
            let scope = if listing { None } else { in_paths.first().cloned() };
            let one = |queries: Vec<String>, what: &str| -> String {
                if queries.len() != 1 {
                    eprintln!("error: {what} takes exactly one name");
                    std::process::exit(2);
                }
                queries.into_iter().next().unwrap_or_default()
            };
            if callers {
                run_callers(one(queries, "--callers"), scope, limit, json);
            } else if refs {
                run_refs(one(queries, "--refs"), scope, json);
            } else if context {
                let (entity, entities) = if queries.len() == 1 { (queries.into_iter().next(), Vec::new()) } else { (None, queries) };
                if entity.is_none() && entities.is_empty() && entity_id.is_none() {
                    eprintln!("error: --context needs a name or --entity-id");
                    std::process::exit(2);
                }
                run_context(ContextArgs {
                    entity,
                    entities,
                    entity_id,
                    file: scope,
                    budget: budget.unwrap_or(8000),
                    hops: hops.unwrap_or(0),
                    json,
                    file_exts,
                    no_cache,
                    no_default_excludes,
                    headers,
                });
            } else if listing {
                if in_paths.is_empty() && text.is_none() {
                    eprintln!("error: give a name to find (sem find parseConfig), or --in <path> to list the entities there");
                    std::process::exit(2);
                }
                run_entities(EntitiesArgs {
                    paths: in_paths,
                    json,
                    no_default_excludes,
                    file_exts,
                    only_kinds,
                    except_kinds,
                    text,
                    signatures,
                });
            } else if entity_id.is_some() && queries.is_empty() {
                eprintln!("error: --entity-id goes with --context (sem find --context --entity-id <id>)");
                std::process::exit(2);
            } else {
                run_find(queries, scope, json);
            }
        }
        Some(Commands::Callers { query, file, limit, json }) => {
            alias::note("callers", "find --callers", json);
            run_callers(query, file, limit, json);
        }
        Some(Commands::Refs { query, file, json }) => {
            alias::note("refs", "find --refs", json);
            run_refs(query, file, json);
        }
        Some(Commands::Grep {
            pattern,
            patterns,
            ignore_case,
            paths,
            files_with_matches,
            line_number: _,
            json,
        }) => {
            let cwd = cwd_string();
            // With `-e`, every positional is a path (rg semantics), so the
            // first positional clap parsed as `pattern` joins `paths`.
            let (pattern, paths) = if patterns.is_empty() {
                (pattern, paths)
            } else {
                (None, pattern.into_iter().chain(paths).collect())
            };
            let scope = commands::grep::Scope {
                paths,
                files_with_matches,
            };
            if patterns.len() > 1 {
                commands::grep::grep_multi_command(cwd, patterns, ignore_case, json, &scope);
            } else {
                // Positional pattern, or exactly one -e: the single form,
                // byte-identical to what it always produced.
                let single = pattern
                    .or_else(|| patterns.into_iter().next())
                    .unwrap_or_default();
                commands::grep::grep_command(commands::grep::GrepOptions {
                    cwd,
                    pattern: single,
                    case_insensitive: ignore_case,
                    json,
                    scope,
                });
            }
        }
        Some(Commands::Check { args, promises }) => {
            let (json, directory) = (args.json, args.directory.clone());
            let code = commands::check::run(args);
            if !promises {
                std::process::exit(code);
            }
            let proved = commands::check_promises::verify(directory.as_deref(), json);
            std::process::exit(commands::check_promises::combine(code, proved));
        }
        Some(Commands::History { entity, blame, file, limit, format, json, verbose }) => {
            let json = resolve_json(format, json);
            if blame {
                let Some(file) = entity.or(file) else {
                    eprintln!("error: --blame needs a file (sem history --blame src/config.ts)");
                    std::process::exit(2);
                };
                run_blame(file, json);
            } else {
                run_log(entity, file, limit, json, verbose);
            }
        }
        Some(Commands::Config { cmd }) => match cmd {
            ConfigCmd::Setup => or_exit_1(commands::setup::run()),
            ConfigCmd::Unsetup => or_exit_1(commands::setup::unsetup()),
            ConfigCmd::Telemetry { action } => run_telemetry(action),
            ConfigCmd::Completions { shell } => run_completions(shell),
            ConfigCmd::Update => or_exit_1(commands::update::run()),
            ConfigCmd::Stats => commands::stats::run(),
        },
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
            let json = resolve_json(format, json);
            alias::note("log", "history", json);
            run_log(entity, file, limit, json, verbose);
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
            signatures,
        }) => {
            let json = resolve_json(format, json);
            alias::note("entities", "find --in <path>", json);
            run_entities(EntitiesArgs {
                paths,
                json,
                no_default_excludes,
                file_exts,
                only_kinds,
                except_kinds,
                text,
                signatures,
            });
        }
        Some(Commands::Imports { path, format, json }) => {
            let json = resolve_json(format, json);
            imports_command(ImportsOptions { cwd: cwd_string(), path, json });
        }
        Some(Commands::Context {
            entity,
            entities,
            entity_id,
            file,
            budget,
            hops,
            format,
            json,
            file_exts,
            no_cache,
            no_default_excludes,
            headers,
        }) => {
            let json = resolve_json(format, json);
            alias::note("context", "find --context", json);
            run_context(ContextArgs {
                entity,
                entities,
                entity_id,
                file,
                budget,
                hops,
                json,
                file_exts,
                no_cache,
                no_default_excludes,
                headers,
            });
        }
        Some(Commands::Stats) => {
            alias::note("stats", "config stats", false);
            commands::stats::run();
        }
        Some(Commands::Mcp { resident, status, review }) => {
            if status {
                println!("{}", sem_mcp::shared_status());
                return;
            }
            if resident {
                // No-op: see the `resident` field's doc comment above.
                return;
            }
            let result = if review { sem_mcp::run_review() } else { sem_mcp::run() };
            if let Err(e) = result {
                eprintln!("{} {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        Some(Commands::Setup) => {
            alias::note("setup", "config setup", false);
            or_exit_1(commands::setup::run());
        }
        Some(Commands::Unsetup) => {
            alias::note("unsetup", "config unsetup", false);
            or_exit_1(commands::setup::unsetup());
        }
        Some(Commands::Login { key, endpoint }) => {
            alias::note("login", "cloud login", false);
            or_exit_1(commands::cloud::login(key, endpoint));
        }
        Some(Commands::Logout) => {
            alias::note("logout", "cloud logout", false);
            or_exit_1(commands::cloud::logout());
        }
        Some(Commands::Whoami) => {
            alias::note("whoami", "cloud whoami", false);
            or_exit_1(commands::cloud::whoami());
        }
        Some(Commands::Cloud { action }) => {
            let cwd = cwd_string();
            match action {
                CloudAction::Login { key, endpoint } => or_exit_1(commands::cloud::login(key, endpoint)),
                CloudAction::Logout => or_exit_1(commands::cloud::logout()),
                CloudAction::Whoami => or_exit_1(commands::cloud::whoami()),
                CloudAction::Review { action } => run_review(action),
                CloudAction::Xref { json } => or_exit_1(commands::cloud::xref(json)),
                CloudAction::Repos { json } => or_exit_1(commands::repos::run(json)),
                CloudAction::Enable => commands::consent::enable(&cwd),
                CloudAction::Disable => commands::consent::never(&cwd),
                CloudAction::Share => commands::consent::share(&cwd),
                CloudAction::List => {
                    alias::note("cloud list", "cloud repos", false);
                    or_exit_1(commands::repos::run(false))
                }
                CloudAction::Status => commands::consent::status(&cwd),
                CloudAction::Preview => commands::consent::preview(&cwd),
                CloudAction::Log => commands::consent::log(),
                CloudAction::Never => {
                    alias::note("cloud never", "cloud disable", false);
                    commands::consent::never(&cwd)
                }
                CloudAction::Forget => commands::consent::forget(&cwd),
            }
        }
        Some(Commands::Review { action }) => {
            alias::note("review", "cloud review", false);
            run_review(action);
        }
        Some(Commands::Telemetry { action }) => {
            alias::note("telemetry", "config telemetry", false);
            run_telemetry(action);
        }
        Some(Commands::Xref { json }) => {
            alias::note("xref", "cloud xref", json);
            or_exit_1(commands::cloud::xref(json));
        }
        Some(Commands::Repos { json }) => {
            alias::note("repos", "cloud repos", json);
            or_exit_1(commands::repos::run(json));
        }
        Some(Commands::Update) => {
            alias::note("update", "config update", false);
            or_exit_1(commands::update::run());
        }
        Some(Commands::Completions { shell }) => {
            alias::note("completions", "config completions", false);
            run_completions(shell);
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
                cwd: cwd_string(),
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
