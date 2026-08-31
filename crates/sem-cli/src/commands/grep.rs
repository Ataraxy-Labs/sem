//! `sem grep <pattern>` — the text tier (`sem_core::index::grep`).
//! Same shape as `commands::query`'s verbs: answer from the mmap index when
//! one exists and has a trigram tier, fall back to a fresh walk + full scan
//! otherwise (which then leaves an index for next time). rg-compatible
//! `file:line:text` output.

use colored::Colorize;
use sem_core::index::grep::{self, CandidateOrigin, GrepHit};
use serde::Serialize;

pub struct GrepOptions {
    pub cwd: String,
    pub pattern: String,
    pub case_insensitive: bool,
    pub json: bool,
}

pub fn grep_command(opts: GrepOptions) {
    let (hits, origin, candidate_files, total_files) =
        match search_one(&opts.cwd, &opts.pattern, opts.case_insensitive) {
            Ok(parts) => parts,
            Err(e) => fail(&e),
        };
    render(&hits, origin, candidate_files, total_files, opts.json);
}

/// `sem grep -e p1 -e p2 …` — several patterns in one invocation, each
/// pattern's hits kept separate (rg-style repeated `-e`, except reported
/// per-pattern rather than merged). Every pattern runs through the exact
/// same index/full-scan tiers as a single `sem grep`. Exit codes match the
/// single form's conventions extended to the batch: 2 on the first invalid
/// pattern, 1 when every pattern produced zero hits, 0 otherwise.
pub fn grep_multi_command(cwd: String, patterns: Vec<String>, case_insensitive: bool, json: bool) {
    let mut per_pattern = Vec::with_capacity(patterns.len());
    for pattern in &patterns {
        match search_one(&cwd, pattern, case_insensitive) {
            Ok(parts) => per_pattern.push(parts),
            Err(e) => fail(&e),
        }
    }

    let any_hit = per_pattern.iter().any(|(hits, ..)| !hits.is_empty());
    if json {
        let results: Vec<serde_json::Value> = patterns
            .iter()
            .zip(&per_pattern)
            .map(|(pattern, (hits, origin, candidate_files, total_files))| {
                serde_json::json!({
                    "pattern": pattern,
                    "hits": hits
                        .iter()
                        .map(|h| serde_json::json!({
                            "file": h.file,
                            "line": h.line,
                            "text": h.text,
                        }))
                        .collect::<Vec<_>>(),
                    "candidate_files": candidate_files,
                    "total_files": total_files,
                    "origin": origin_label(*origin),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&results).unwrap_or_default());
    } else {
        for (i, (pattern, (hits, ..))) in patterns.iter().zip(&per_pattern).enumerate() {
            if i > 0 {
                println!();
            }
            println!("{}", format!("pattern \"{pattern}\":").dimmed());
            for hit in hits {
                println!(
                    "{}{}{}{}{}",
                    hit.file.magenta(),
                    ":".dimmed(),
                    hit.line.to_string().green(),
                    ":".dimmed(),
                    hit.text
                );
            }
            if hits.is_empty() {
                println!("{}", "  (no hits)".dimmed());
            }
        }
    }
    if !any_hit {
        std::process::exit(1);
    }
}

/// One pattern through the tiers: index-served when a usable trigram tier
/// exists, plain full scan otherwise. Extracted from `grep_command` so the
/// multi-pattern form runs the identical machinery per pattern.
#[allow(clippy::type_complexity)]
fn search_one(
    cwd: &str,
    pattern: &str,
    case_insensitive: bool,
) -> Result<(Vec<GrepHit>, CandidateOrigin, usize, usize), regex::Error> {
    let root = super::repo_root_or_cwd(cwd);
    let grep_opts = grep::GrepOptions { case_insensitive };

    let registry = super::create_registry(cwd);
    let from_index = if std::env::var_os("SEM_NO_INDEX").is_none() {
        super::query::open_index(&root).map(|idx| {
            grep::search(&idx, &root, pattern, &grep_opts, |dir: &std::path::Path| {
                super::files::find_supported_files_in_path(&root, dir, &registry, &[], false)
            })
        })
    } else {
        None
    };

    match from_index {
        Some(Ok(report)) => Ok((
            report.hits,
            report.origin,
            report.candidate_files,
            report.total_files,
        )),
        Some(Err(e)) => Err(e),
        // No usable index: the same cold-build fallback `commands::query`
        // uses, except there is no entity graph to build — a plain file walk
        // is all `full_scan` needs, and `write_query_index` is not called
        // here because the caller has not built a graph to derive one from
        // (a bare `sem grep` on an unindexed repo does not itself trigger a
        // corpus-level build — the next `graph`/`diff`/`impact`/`find` does).
        None => {
            let file_paths =
                super::graph::find_supported_files_with_options(&root, &registry, &[], false);
            let hits = grep::full_scan(&root, &file_paths, pattern, &grep_opts)?;
            let n = file_paths.len();
            Ok((hits, CandidateOrigin::FullScan, n, n))
        }
    }
}

fn fail(e: &regex::Error) -> ! {
    eprintln!("{} invalid pattern: {e}", "error:".red().bold());
    std::process::exit(2);
}

#[derive(Serialize)]
struct HitRow {
    file: String,
    line: usize,
    text: String,
}

#[derive(Serialize)]
struct Report {
    hits: Vec<HitRow>,
    candidate_files: usize,
    total_files: usize,
    origin: &'static str,
}

fn origin_label(origin: CandidateOrigin) -> &'static str {
    match origin {
        CandidateOrigin::Trigram => "trigram",
        CandidateOrigin::FullScan => "full_scan",
        CandidateOrigin::NoCandidates => "no_candidates",
    }
}

fn render(
    hits: &[GrepHit],
    origin: CandidateOrigin,
    candidate_files: usize,
    total_files: usize,
    json: bool,
) {
    if json {
        let report = Report {
            hits: hits
                .iter()
                .map(|h| HitRow {
                    file: h.file.clone(),
                    line: h.line,
                    text: h.text.clone(),
                })
                .collect(),
            candidate_files,
            total_files,
            origin: origin_label(origin),
        };
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        for hit in hits {
            println!(
                "{}{}{}{}{}",
                hit.file.magenta(),
                ":".dimmed(),
                hit.line.to_string().green(),
                ":".dimmed(),
                hit.text
            );
        }
        if let Ok(val) = std::env::var("SEM_GREP_STATS") {
            if val == "1" {
                eprintln!(
                    "note: {} candidate file(s) of {} scanned ({})",
                    candidate_files,
                    total_files,
                    origin_label(origin)
                );
            }
        }
    }
    if hits.is_empty() {
        std::process::exit(1);
    }
}
