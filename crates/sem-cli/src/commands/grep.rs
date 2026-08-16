//! `sem grep <pattern>` — the text tier (`sem_core::index::grep`, semx-az9).
//! Same shape as `commands::query`'s verbs: answer from the mmap index when
//! one exists and has a trigram tier, fall back to a fresh walk + full scan
//! otherwise (which then leaves an index for next time). rg-compatible
//! `file:line:text` output. See `QUERY-INDEX.md`'s trigram section for the
//! design, the budget outcome, and the measured `sem`-vs-`rg` table.

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
    let root = super::repo_root_or_cwd(&opts.cwd);
    let grep_opts = grep::GrepOptions {
        case_insensitive: opts.case_insensitive,
    };

    let registry = super::create_registry(&opts.cwd);
    let from_index = if std::env::var_os("SEM_NO_INDEX").is_none() {
        super::query::open_index(&root).map(|idx| {
            grep::search(
                &idx,
                &root,
                &opts.pattern,
                &grep_opts,
                |dir: &std::path::Path| {
                    super::files::find_supported_files_in_path(&root, dir, &registry, &[], false)
                },
            )
        })
    } else {
        None
    };

    let (hits, origin, candidate_files, total_files) = match from_index {
        Some(Ok(report)) => (
            report.hits,
            report.origin,
            report.candidate_files,
            report.total_files,
        ),
        Some(Err(e)) => fail(&e),
        // No usable index: the same cold-build fallback `commands::query`
        // uses, except there is no entity graph to build — a plain file walk
        // is all `full_scan` needs, and `write_query_index` is not called
        // here because the caller has not built a graph to derive one from
        // (a bare `sem grep` on an unindexed repo does not itself trigger a
        // corpus-level build — the next `graph`/`diff`/`impact`/`find` does).
        None => {
            let file_paths =
                super::graph::find_supported_files_with_options(&root, &registry, &[], false);
            match grep::full_scan(&root, &file_paths, &opts.pattern, &grep_opts) {
                Ok(hits) => {
                    let n = file_paths.len();
                    (hits, CandidateOrigin::FullScan, n, n)
                }
                Err(e) => fail(&e),
            }
        }
    };

    render(&hits, origin, candidate_files, total_files, opts.json);
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
