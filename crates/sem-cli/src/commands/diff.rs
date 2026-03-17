use std::io::Read;
use std::path::Path;
use std::process;
use std::time::Instant;

use sem_core::git::types::FileChange;
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::plugins::create_default_registry;

use super::common::{self, open_git_or_exit};
use crate::formatters::{json::format_json, terminal::format_terminal};

pub struct DiffOptions {
    pub cwd: String,
    pub format: OutputFormat,
    pub staged: bool,
    pub commit: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub stdin: bool,
    pub profile: bool,
    pub file_exts: Vec<String>,
    pub files: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Terminal,
    Json,
}

pub fn diff_command(opts: DiffOptions) {
    let total_start = Instant::now();

    let t0 = Instant::now();
    let (file_changes, from_stdin) = if opts.files.len() == 2 {
        // Compare two arbitrary files: sem diff file1.ts file2.ts
        let path_a = Path::new(&opts.files[0]);
        let path_b = Path::new(&opts.files[1]);

        let content_a = std::fs::read_to_string(path_a).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError reading {}: {e}\x1b[0m", path_a.display());
            process::exit(1);
        });
        let content_b = std::fs::read_to_string(path_b).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError reading {}: {e}\x1b[0m", path_b.display());
            process::exit(1);
        });

        let change = FileChange {
            file_path: opts.files[1].clone(),
            old_file_path: None,
            status: sem_core::git::types::FileStatus::Modified,
            before_content: Some(content_a),
            after_content: Some(content_b),
        };
        (vec![change], false)
    } else if opts.files.len() == 1 {
        eprintln!("\x1b[31mError: provide two files to compare, or none for git diff.\x1b[0m");
        process::exit(1);
    } else if opts.stdin {
        // Read FileChange[] from stdin — no git repo needed
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError reading stdin: {e}\x1b[0m");
            process::exit(1);
        });
        let changes: Vec<FileChange> = serde_json::from_str(&input).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError parsing stdin JSON: {e}\x1b[0m");
            process::exit(1);
        });
        (changes, true)
    } else {
        let git = open_git_or_exit(Path::new(&opts.cwd));
        let file_changes = common::resolve_file_changes(
            &git,
            opts.commit.as_deref(),
            opts.from.as_deref(),
            opts.to.as_deref(),
            opts.staged,
        );
        (file_changes, false)
    };
    let git_diff_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let file_changes = common::filter_by_exts(file_changes, &opts.file_exts);

    if file_changes.is_empty() {
        println!("\x1b[2mNo changes detected.\x1b[0m");
        return;
    }

    let t2 = Instant::now();
    let registry = create_default_registry();
    let registry_ms = t2.elapsed().as_secs_f64() * 1000.0;

    let t3 = Instant::now();
    let result = compute_semantic_diff(&file_changes, &registry, None, None);
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
        eprintln!("\x1b[2m  input ({})  {git_diff_ms:>8.2}ms\x1b[0m",
            if from_stdin { "stdin" } else { "git" });
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
