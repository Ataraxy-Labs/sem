//! `sem check` until the exact incremental verifier lands.
//!
//! The arguments mirror the verifier's own (`--base`, `--json`,
//! `--checkers`, `--full`, `--no-cache`, `--timeout`, `-C`), so wiring the
//! real implementation in is a swap of [`CheckArgs`] and [`run`]. Until then
//! `sem check` answers from the project's promises (`.sem/promises/*.json`):
//!
//! - `sem check` runs every promise (`sem promises check`); with `--base` it
//!   reports only violations in files changed since that revision;
//! - `sem check --promises` proves every promise can fail
//!   (`sem promises verify`);
//! - `--checkers` asks for a checker this build does not have.
//!
//! Exit codes match the verifier: 0 pass, 1 fail, 2 could not decide. With
//! nothing to check (no promises, no checkers) the verdict is 2, never a
//! vacuous pass: a landing gated on `sem check` must not publish because
//! nothing ran.

use std::path::Path;

use clap::{Args, Parser};

#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    /// Compare against this revision (default: HEAD): only files changed since
    /// it are reported. Example: sem check --base origin/main
    #[arg(long, value_name = "REV")]
    pub base: Option<String>,
    /// Print one JSON object. Example: sem check --json
    #[arg(long)]
    pub json: bool,
    /// Checkers to run, comma-separated (ts, lint, tests, go, cargo, cmd).
    /// Example: sem check --checkers ts,lint,tests
    #[arg(long, value_delimiter = ',', value_name = "LIST")]
    pub checkers: Vec<String>,
    /// Run every checker in full, not incrementally. Example: sem check --full
    #[arg(long, hide = true)]
    pub full: bool,
    /// Neither read nor write check states
    #[arg(long, hide = true)]
    pub no_cache: bool,
    /// Give up on any one tool run after this many seconds (verdict: could not decide)
    #[arg(long, default_value = "1800", hide = true)]
    pub timeout: u64,
    /// Run as if started in this directory. Example: sem check -C ../other-repo
    #[arg(short = 'C', long = "cwd", value_name = "DIR")]
    pub directory: Option<String>,
}

#[derive(Parser)]
#[command(name = "sem promises")]
struct Promises {
    #[command(subcommand)]
    cmd: super::promises::PromisesCmd,
}

fn promises(argv: &[&str]) -> i32 {
    let parsed =
        match Promises::try_parse_from(std::iter::once("sem promises").chain(argv.iter().copied()))
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("sem check: {e}");
                return 2;
            }
        };
    match super::promises::run(parsed.cmd) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// Runs `sem check`; returns the exit code (0 pass, 1 fail, 2 could not decide).
pub fn run(args: CheckArgs, verify_promises: bool) -> i32 {
    if let Some(dir) = &args.directory {
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("sem check: cannot enter {dir}: {e}");
            return 2;
        }
    }
    if !args.checkers.is_empty() {
        eprintln!(
            "sem check: the checkers ({}) are not in this build; run the project's own tools, \
             or `sem check` alone to verify the promises in .sem/promises",
            args.checkers.join(",")
        );
        return 2;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let root = super::repo_root_or_cwd(&cwd.to_string_lossy());
    if super::promises::discover(Path::new(&root)).is_empty() {
        eprintln!(
            "sem check: nothing to check: no checkers in this build and no promises in {}/.sem/promises",
            root.display()
        );
        return 2;
    }
    let mut argv: Vec<&str> = Vec::new();
    if verify_promises {
        argv.push("verify");
    } else {
        argv.push("check");
        if let Some(base) = &args.base {
            argv.extend(["--since", base.as_str()]);
        }
    }
    if args.json {
        argv.push("--json");
    }
    // `promises check` exits 1 itself when a promise is broken.
    promises(&argv)
}
