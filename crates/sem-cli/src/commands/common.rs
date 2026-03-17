use std::path::Path;
use std::process;

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
    if let Some(sha) = commit {
        let scope = DiffScope::Commit { sha: sha.to_string() };
        match git.get_changed_files(&scope) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else if let (Some(f), Some(t)) = (from, to) {
        let scope = DiffScope::Range {
            from: f.to_string(),
            to: t.to_string(),
        };
        match git.get_changed_files(&scope) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else if staged {
        let scope = DiffScope::Staged;
        match git.get_changed_files(&scope) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        }
    } else {
        match git.detect_and_get_files() {
            Ok((_scope, files)) => files,
            Err(_) => {
                eprintln!("\x1b[31mError: Not inside a Git repository.\x1b[0m");
                process::exit(1);
            }
        }
    }
}

/// Open a GitBridge or exit with an error.
pub fn open_git_or_exit(root: &Path) -> GitBridge {
    match GitBridge::open(root) {
        Ok(g) => g,
        Err(_) => {
            eprintln!("\x1b[31mError: Not inside a Git repository.\x1b[0m");
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

/// Filter file changes by extension.
pub fn filter_by_exts(file_changes: Vec<FileChange>, exts: &[String]) -> Vec<FileChange> {
    if exts.is_empty() {
        return file_changes;
    }
    let normalized = normalize_exts(exts);
    file_changes
        .into_iter()
        .filter(|fc| normalized.iter().any(|ext| fc.file_path.ends_with(ext.as_str())))
        .collect()
}
