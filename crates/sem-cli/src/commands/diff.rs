use std::path::Path;
use std::process;
use std::time::Instant;

use sem_core::git::bridge::GitBridge;
use sem_core::git::types::DiffScope;
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::plugins::create_default_registry;

use crate::formatters::{json::format_json, terminal::format_terminal};

pub struct DiffOptions {
    pub cwd: String,
    pub format: OutputFormat,
    pub staged: bool,
    pub commit: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub profile: bool,
    pub file_exts: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Terminal,
    Json,
}

pub fn diff_command(opts: DiffOptions) {
    let total_start = Instant::now();

    let t0 = Instant::now();
    let git = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(g) => g,
        Err(_) => {
            eprintln!("\x1b[31mError: Not inside a Git repository.\x1b[0m");
            process::exit(1);
        }
    };
    let git_open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    let (scope, file_changes) = if let Some(ref sha) = opts.commit {
        let scope = DiffScope::Commit { sha: sha.clone() };
        match git.get_changed_files(&scope) {
            Ok(files) => (scope, files),
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else if let (Some(ref from), Some(ref to)) = (&opts.from, &opts.to) {
        let scope = DiffScope::Range {
            from: from.clone(),
            to: to.clone(),
        };
        match git.get_changed_files(&scope) {
            Ok(files) => (scope, files),
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else if opts.staged {
        let scope = DiffScope::Staged;
        match git.get_changed_files(&scope) {
            Ok(files) => (scope, files),
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else {
        match git.detect_and_get_files() {
            Ok((scope, files)) => (scope, files),
            Err(_) => {
                eprintln!("\x1b[31mError: Not inside a Git repository.\x1b[0m");
                process::exit(1);
            }
        }
    };
    let git_diff_ms = t1.elapsed().as_secs_f64() * 1000.0;

    // Filter by file extensions if specified
    let file_changes = if opts.file_exts.is_empty() {
        file_changes
    } else {
        let exts: Vec<String> = opts.file_exts.iter().map(|e| {
            if e.starts_with('.') { e.clone() } else { format!(".{}", e) }
        }).collect();
        file_changes.into_iter().filter(|fc| {
            exts.iter().any(|ext| fc.file_path.ends_with(ext.as_str()))
        }).collect()
    };

    if file_changes.is_empty() {
        println!("\x1b[2mNo changes detected.\x1b[0m");
        return;
    }

    let t2 = Instant::now();
    let registry = create_default_registry();
    let registry_ms = t2.elapsed().as_secs_f64() * 1000.0;

    let t3 = Instant::now();
    let commit_sha = match &scope {
        DiffScope::Commit { sha } => Some(sha.as_str()),
        _ => None,
    };
    let result = compute_semantic_diff(&file_changes, &registry, commit_sha, None);
    let parse_diff_ms = t3.elapsed().as_secs_f64() * 1000.0;

    let t4 = Instant::now();
    let output = match opts.format {
        OutputFormat::Json => format_json(&result),
        OutputFormat::Terminal => format_terminal(&result),
    };
    let format_ms = t4.elapsed().as_secs_f64() * 1000.0;

    println!("{output}");

    if opts.profile {
        let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
        eprintln!();
        eprintln!("\x1b[2m── Profile ──────────────────────────────────\x1b[0m");
        eprintln!("\x1b[2m  git2 open repo      {git_open_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  git diff + content   {git_diff_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  registry init        {registry_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  parse + match        {parse_diff_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  format output        {format_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  ─────────────────────────────────\x1b[0m");
        eprintln!("\x1b[2m  total                {total_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  files: {}  entities: {}  changes: {}\x1b[0m",
            file_changes.len(), result.changes.len(),
            result.added_count + result.modified_count + result.deleted_count + result.moved_count + result.renamed_count);
        eprintln!("\x1b[2m─────────────────────────────────────────────\x1b[0m");
    }
}
