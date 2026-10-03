//! `sem check --promises`: after the checkers, prove every promise in
//! `.sem/promises` can fail (`sem promises verify`), and fold the two
//! verdicts into one exit code.

use std::path::Path;

use clap::Parser;

#[derive(Parser)]
#[command(name = "sem promises")]
struct Promises {
    #[command(subcommand)]
    cmd: super::promises::PromisesCmd,
}

/// Runs `sem promises verify` in `directory` (default: the current one).
/// Returns 0 when every promise is proved, 1 when one is vacuous or broken,
/// 2 when there are no promises or verification could not run.
pub fn verify(directory: Option<&str>, json: bool) -> i32 {
    if let Some(dir) = directory {
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("sem check --promises: cannot enter {dir}: {e}");
            return 2;
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let root = super::repo_root_or_cwd(&cwd.to_string_lossy());
    if super::promises::discover(Path::new(&root)).is_empty() {
        eprintln!("sem check --promises: no promises in {}/.sem/promises", root.display());
        return 2;
    }
    let mut argv = vec!["sem promises", "verify"];
    if json {
        argv.push("--json");
    }
    let parsed = match Promises::try_parse_from(argv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sem check --promises: {e}");
            return 2;
        }
    };
    // `promises verify` exits 1 itself when a promise fails verification.
    match super::promises::run(parsed.cmd) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// The worse of two verdicts: fail (1) over could not decide (2) over pass (0).
pub fn combine(a: i32, b: i32) -> i32 {
    if a == 1 || b == 1 {
        1
    } else if a == 0 && b == 0 {
        0
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::combine;

    #[test]
    fn fail_beats_undecided_beats_pass() {
        assert_eq!(combine(0, 0), 0);
        assert_eq!(combine(0, 2), 2);
        assert_eq!(combine(2, 0), 2);
        assert_eq!(combine(2, 1), 1);
        assert_eq!(combine(1, 0), 1);
    }
}
