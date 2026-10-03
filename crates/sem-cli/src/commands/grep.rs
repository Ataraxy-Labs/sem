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
    pub scope: Scope,
}

/// Output scoping shared by the single and multi-pattern forms: rg-style
/// trailing paths (hits outside them are dropped) and `-l`.
#[derive(Default)]
pub struct Scope {
    pub paths: Vec<String>,
    pub files_with_matches: bool,
}

impl Scope {
    /// Keep hits under one of the requested paths (repo-relative prefixes),
    /// then collapse to one row per file under `-l`.
    fn apply(&self, cwd: &str, hits: Vec<GrepHit>) -> Vec<GrepHit> {
        let hits = if self.paths.is_empty() {
            hits
        } else {
            let root = super::repo_root_or_cwd(cwd);
            let prefixes: Vec<String> = self
                .paths
                .iter()
                .map(|p| super::normalize_repo_relative_path(std::path::Path::new(cwd), &root, p))
                .map(|p| p.trim_end_matches('/').to_string())
                .collect();
            hits.into_iter()
                .filter(|h| {
                    prefixes.iter().any(|p| {
                        p == "." || p.is_empty() || h.file == *p || h.file.starts_with(&format!("{p}/"))
                    })
                })
                .collect()
        };
        if !self.files_with_matches {
            return hits;
        }
        let mut seen = std::collections::HashSet::new();
        hits.into_iter().filter(|h| seen.insert(h.file.clone())).collect()
    }
}

pub fn grep_command(opts: GrepOptions) {
    let (hits, origin, candidate_files, total_files) =
        match search_one(&opts.cwd, &opts.pattern, opts.case_insensitive) {
            Ok(parts) => parts,
            Err(e) => fail(&e),
        };
    let hits = opts.scope.apply(&opts.cwd, hits);
    render(&hits, origin, candidate_files, total_files, opts.json, opts.scope.files_with_matches);
}

/// `sem grep -e p1 -e p2 …` — several patterns in one invocation, each
/// pattern's hits kept separate (rg-style repeated `-e`, except reported
/// per-pattern rather than merged). Every pattern runs through the exact
/// same index/full-scan tiers as a single `sem grep`. Exit codes match the
/// single form's conventions extended to the batch: 2 on the first invalid
/// pattern, 1 when every pattern produced zero hits, 0 otherwise.
pub fn grep_multi_command(
    cwd: String,
    patterns: Vec<String>,
    case_insensitive: bool,
    json: bool,
    scope: &Scope,
) {
    let mut per_pattern = Vec::with_capacity(patterns.len());
    for pattern in &patterns {
        match search_one(&cwd, pattern, case_insensitive) {
            Ok((hits, origin, candidates, total)) => {
                per_pattern.push((scope.apply(&cwd, hits), origin, candidates, total))
            }
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
                    "coverage": "eligible_text_files_best_effort; ignore_hidden_binary_and_default_exclusions_apply",
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
                print_hit(hit, scope.files_with_matches);
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

    let file_paths = super::files::find_search_files(&root);
    let from_index = if std::env::var_os("SEM_NO_INDEX").is_none() {
        super::query::open_index(&root).map(|idx| {
            let indexed: std::collections::HashSet<_> = (0..idx.file_count())
                .map(|i| idx.file_path(i as u32).to_string())
                .collect();
            // The structural index cannot contain every text file. Search the
            // unindexed remainder even when no directory mtime has changed.
            let extra: Vec<_> = file_paths
                .iter()
                .filter(|file| !indexed.contains(*file))
                .cloned()
                .collect();
            let mut report = grep::search(&idx, &root, pattern, &grep_opts, |_| Vec::new())?;
            report
                .hits
                .retain(|hit| file_paths.binary_search(&hit.file).is_ok());
            report
                .hits
                .extend(grep::full_scan(&root, &extra, pattern, &grep_opts)?);
            report
                .hits
                .sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
            report
                .hits
                .dedup_by(|a, b| a.file == b.file && a.line == b.line);
            report.candidate_files += extra.len();
            report.total_files = file_paths.len();
            Ok::<_, regex::Error>(report)
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
    coverage: &'static str,
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

fn print_hit(hit: &GrepHit, file_only: bool) {
    if file_only {
        println!("{}", hit.file.magenta());
        return;
    }
    println!(
        "{}{}{}{}{}",
        hit.file.magenta(),
        ":".dimmed(),
        hit.line.to_string().green(),
        ":".dimmed(),
        hit.text
    );
}

fn render(
    hits: &[GrepHit],
    origin: CandidateOrigin,
    candidate_files: usize,
    total_files: usize,
    json: bool,
    file_only: bool,
) {
    if json {
        let report = Report {
            coverage:
                "eligible_text_files_best_effort; ignore_hidden_binary_and_default_exclusions_apply",
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
            print_hit(hit, file_only);
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
