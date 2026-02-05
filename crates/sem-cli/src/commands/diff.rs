use std::path::Path;
use std::process;

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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Terminal,
    Json,
}

pub fn diff_command(opts: DiffOptions) {
    let git = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(g) => g,
        Err(_) => {
            eprintln!("\x1b[31mError: Not inside a Git repository.\x1b[0m");
            process::exit(1);
        }
    };

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

    if file_changes.is_empty() {
        println!("\x1b[2mNo changes detected.\x1b[0m");
        return;
    }

    let registry = create_default_registry();
    let commit_sha = match &scope {
        DiffScope::Commit { sha } => Some(sha.as_str()),
        _ => None,
    };

    let result = compute_semantic_diff(&file_changes, &registry, commit_sha, None);

    match opts.format {
        OutputFormat::Json => println!("{}", format_json(&result)),
        OutputFormat::Terminal => println!("{}", format_terminal(&result)),
    }
}
