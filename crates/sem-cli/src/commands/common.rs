use std::path::Path;
use std::process;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::git::types::{DiffScope, FileChange};

/// Resolve file changes from git based on CLI options.
pub fn resolve_file_changes(
    git: &GitBridge,
    commit: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    staged: bool,
) -> Vec<FileChange> {
    // Build scope from options, or auto-detect.
    if let Some(sha) = commit {
        get_or_exit(git, &DiffScope::Commit { sha: sha.to_string() })
    } else if let (Some(f), Some(t)) = (from, to) {
        get_or_exit(git, &DiffScope::Range { from: f.to_string(), to: t.to_string() })
    } else if staged {
        get_or_exit(git, &DiffScope::Staged)
    } else {
        match git.detect_and_get_files() {
            Ok((_scope, files)) => files,
            Err(_) => {
                eprintln!("{}", "Error: Not inside a Git repository.".red());
                process::exit(1);
            }
        }
    }
}

fn get_or_exit(git: &GitBridge, scope: &DiffScope) -> Vec<FileChange> {
    match git.get_changed_files(scope) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("{} {e}", "Error:".red());
            process::exit(1);
        }
    }
}

/// Open a GitBridge or exit with an error.
pub fn open_git_or_exit(root: &Path) -> GitBridge {
    match GitBridge::open(root) {
        Ok(g) => g,
        Err(_) => {
            eprintln!("{}", "Error: Not inside a Git repository.".red());
            process::exit(1);
        }
    }
}

/// Normalize extension strings to have a leading dot.
pub fn normalize_exts(exts: &[String]) -> Vec<String> {
    exts.iter()
        .map(|e| {
            if e.starts_with('.') {
                e.clone()
            } else {
                format!(".{}", e)
            }
        })
        .collect()
}

/// Filter file changes by extension. `exts` should already be normalized via `normalize_exts`.
pub fn filter_by_exts(file_changes: Vec<FileChange>, exts: &[String]) -> Vec<FileChange> {
    if exts.is_empty() {
        return file_changes;
    }
    file_changes
        .into_iter()
        .filter(|fc| exts.iter().any(|ext| fc.file_path.ends_with(ext.as_str())))
        .collect()
}
