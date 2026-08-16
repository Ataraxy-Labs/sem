//! The diff-equivalence oracle, run against real git history.
//!
//! Picks the most recent commits that touch the target extensions, replays
//! each one as a synthetic PR through the *full* `sem diff` pipeline twice —
//! once with the fast path off, once with it on — and asserts the entity
//! sets, the `DiffResult`, and the rendered `sem diff --json` envelope are
//! identical. See `crates/sem-core/src/parser/diff_oracle.rs` for what
//! "identical" means and why it is defined there rather than at the
//! `structural_hash` level.
//!
//! ```text
//! cargo run --release --example diff_oracle -- <repo_root> [options]
//!
//!   --commits N        how many qualifying commits to replay (default 20)
//!   --skip N           skip the N most recent qualifying commits (default 0)
//!   --exts .ts,.tsx    extensions a commit must touch to qualify
//!                      (default: the JS/TS family)
//!   --mutate KIND      install a mutation extractor instead of the built-in
//!                      fast path: faithful | drop-last | shift-span |
//!                      rename | drop-structural-hash | drop-kappa |
//!                      merge-declarators | all
//!   --label L          label for the emitted lines (default: repo dir name)
//!   --max-files N      skip commits touching more than N qualifying files
//!                      (default 400)
//!   --verbose          print every divergence, not just the first three
//! ```
//!
//! Exit status is 1 if any commit is `Divergent`, or if every commit is
//! `Vacuous` (nothing served ⇒ nothing proved).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sem_core::git::bridge::GitBridge;
use sem_core::git::types::DiffScope;
use sem_core::parser::diff_oracle::{self, MutatingExtractor, Mutation, OracleRun};
use sem_core::parser::fast_extractor;
use sem_core::parser::plugins::create_default_registry;

struct Args {
    root: PathBuf,
    commits: usize,
    skip: usize,
    exts: Vec<String>,
    mutate: Option<Vec<Mutation>>,
    label: String,
    max_files: usize,
    verbose: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let root = PathBuf::from(
        it.next()
            .ok_or("usage: diff_oracle <repo_root> [options]")?,
    );
    let label = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let mut args = Args {
        root,
        commits: 20,
        skip: 0,
        exts: fast_extractor::JS_TS_EXTENSIONS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        mutate: None,
        label,
        max_files: 400,
        verbose: false,
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--commits" => args.commits = next_usize(&mut it, "--commits")?,
            "--skip" => args.skip = next_usize(&mut it, "--skip")?,
            "--max-files" => args.max_files = next_usize(&mut it, "--max-files")?,
            "--label" => args.label = it.next().ok_or("--label needs a value")?,
            "--verbose" => args.verbose = true,
            "--exts" => {
                let raw = it.next().ok_or("--exts needs a value")?;
                args.exts = raw
                    .split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--mutate" => {
                let raw = it.next().ok_or("--mutate needs a value")?;
                args.mutate = Some(if raw == "all" {
                    Mutation::ALL.to_vec()
                } else {
                    vec![Mutation::parse(&raw).ok_or(format!("unknown mutation `{raw}`"))?]
                });
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    Ok(args)
}

fn next_usize(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize, String> {
    it.next()
        .ok_or_else(|| format!("{flag} needs a value"))?
        .parse()
        .map_err(|_| format!("{flag} needs a number"))
}

/// Most recent non-merge commits whose diff touches at least one qualifying
/// file, newest first.
fn qualifying_commits(
    root: &Path,
    exts: &[String],
    want: usize,
    skip: usize,
    max_files: usize,
) -> Result<Vec<String>, String> {
    let repo = git2::Repository::open(root).map_err(|e| format!("open {root:?}: {e}"))?;
    let mut revwalk = repo.revwalk().map_err(|e| e.to_string())?;
    revwalk.push_head().map_err(|e| e.to_string())?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    let mut skipped = 0usize;
    for oid in revwalk.take(20_000) {
        if out.len() >= want {
            break;
        }
        let Ok(oid) = oid else { continue };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        if commit.parent_count() != 1 {
            continue;
        }
        let Ok(parent) = commit.parent(0) else {
            continue;
        };
        let (Ok(tree), Ok(parent_tree)) = (commit.tree(), parent.tree()) else {
            continue;
        };
        let Ok(diff) = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None) else {
            continue;
        };
        let mut hits = 0usize;
        for delta in diff.deltas() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if exts.iter().any(|e| path.ends_with(e.as_str())) {
                hits += 1;
            }
        }
        if hits == 0 || hits > max_files {
            continue;
        }
        if skipped < skip {
            skipped += 1;
            continue;
        }
        out.push(oid.to_string());
    }
    Ok(out)
}

fn print_run(run: &OracleRun, verbose: bool) {
    println!("ORACLE {}", run.summary_line());
    let show = if verbose { run.divergences.len() } else { 3 };
    for d in run.divergences.iter().take(show) {
        println!("   [{:?}] {} :: {}", d.layer, d.scope, d.detail);
    }
    if !verbose && run.divergences.len() > show {
        println!("   … {} more (use --verbose)", run.divergences.len() - show);
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let commits = match qualifying_commits(
        &args.root,
        &args.exts,
        args.commits,
        args.skip,
        args.max_files,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if commits.is_empty() {
        eprintln!("no qualifying commits found in {:?}", args.root);
        std::process::exit(2);
    }

    let bridge = match GitBridge::open(&args.root) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("git open failed: {e}");
            std::process::exit(2);
        }
    };
    let registry = create_default_registry();

    // Materialize every commit's change set once, so a mutation sweep costs
    // git work once instead of once per mutation.
    let mut change_sets = Vec::new();
    for sha in &commits {
        match bridge.get_changed_files(&DiffScope::Commit { sha: sha.clone() }, &[]) {
            Ok(mut files) => {
                files.retain(|f| {
                    let p = f.file_path.to_ascii_lowercase();
                    args.exts.iter().any(|e| p.ends_with(e.as_str()))
                });
                if !files.is_empty() {
                    change_sets.push((sha.clone(), files));
                }
            }
            Err(e) => eprintln!("skipping {}: {e}", &sha[..8.min(sha.len())]),
        }
    }

    let legs: Vec<Option<Mutation>> = match &args.mutate {
        Some(ms) => ms.iter().copied().map(Some).collect(),
        None => vec![None],
    };

    let mut failed = false;
    for leg in legs {
        let previous = leg.map(|m| MutatingExtractor::new(m).install());
        let leg_label = match leg {
            Some(m) => format!("{}/{:?}", args.label, m),
            None => format!("{}/builtin", args.label),
        };

        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for (sha, files) in &change_sets {
            let label = format!("{}@{}", leg_label, &sha[..8.min(sha.len())]);
            let run = diff_oracle::run(files, &registry, &label);
            *tally.entry(format!("{:?}", run.verdict())).or_default() += 1;
            print_run(&run, args.verbose);
        }
        if let Some(previous) = previous {
            fast_extractor::install(previous);
        }

        let divergent = *tally.get("Divergent").unwrap_or(&0);
        let equivalent = *tally.get("Equivalent").unwrap_or(&0);
        let vacuous = *tally.get("Vacuous").unwrap_or(&0);
        println!(
            "ORACLE_TOTAL leg={leg_label} commits={} equivalent={equivalent} vacuous={vacuous} divergent={divergent}",
            change_sets.len()
        );

        // A mutation sweep inverts the expectation for every mutation but
        // `Faithful`: the oracle is *supposed* to diverge, and a mutation that
        // slips through is the failure — unless the mutation is one this
        // corpus simply never exercises, which is reported as INERT rather
        // than laundered into either a pass or a fail.
        match leg {
            None | Some(Mutation::Faithful) => {
                if divergent > 0 || equivalent == 0 {
                    failed = true;
                    println!("ORACLE_LEG_FAILED leg={leg_label} expected=equivalent");
                }
            }
            Some(m) if divergent > 0 => {
                println!("ORACLE_LEG_CAUGHT leg={leg_label} mutation={m:?}");
            }
            Some(m) if m.always_observable() => {
                failed = true;
                println!("ORACLE_LEG_FAILED leg={leg_label} mutation={m:?} escaped_undetected");
            }
            Some(m) => {
                println!("ORACLE_LEG_INERT leg={leg_label} mutation={m:?} not_exercised_by_corpus");
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
}
