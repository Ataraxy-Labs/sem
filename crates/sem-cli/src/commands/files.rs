use std::{collections::HashSet, io::Write, path::Path, process::Command};

use colored::Colorize;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

pub fn find_supported_files_in_path(
    root: &Path,
    scan_path: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Vec<String> {
    let mut files = Vec::new();

    let mut builder = ignore::WalkBuilder::new(scan_path);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    let semignore = root.join(".semignore");
    if semignore.exists() {
        builder.add_ignore(semignore);
    }

    if !no_default_excludes {
        let root = root.to_path_buf();
        builder.filter_entry(move |entry| {
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
            {
                return true;
            }

            let rel_path = file_path_for_entity(&root, entry.path());
            !is_default_excluded(&rel_path)
        });
    }

    let walker = builder.build();

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!(
                    "{} Cannot walk '{}': {}",
                    "error:".red().bold(),
                    scan_path.display(),
                    e
                );
                std::process::exit(1);
            }
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let rel_path = file_path_for_entity(root, path);
        if !no_default_excludes && is_default_excluded(&rel_path) {
            continue;
        }
        if is_hidden_path(&rel_path) {
            continue;
        }
        if !ext_filter.is_empty()
            && !ext_filter
                .iter()
                .any(|ext| rel_path.ends_with(ext.as_str()))
        {
            continue;
        }
        if is_probably_binary_path(&rel_path) {
            continue;
        }
        if !has_supported_plugin(path, &rel_path, registry, ext_filter) {
            continue;
        }
        files.push(rel_path);
    }

    files.sort();
    files
}

pub fn find_supported_files_from_git_index(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Option<Vec<String>> {
    if root.join(".semignore").exists() {
        return None;
    }

    let output = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let missing_from_worktree = git_missing_from_worktree_paths(root)?;
    let mut candidates = Vec::new();
    for raw_path in output.stdout.split(|byte| *byte == 0) {
        if raw_path.is_empty() {
            continue;
        }
        let rel_path = String::from_utf8_lossy(raw_path).replace('\\', "/");
        if missing_from_worktree.contains(rel_path.as_str()) {
            continue;
        }
        if rel_path.is_empty() || rel_path.ends_with('/') {
            continue;
        }
        if Path::new(&rel_path).is_absolute() {
            return None;
        }
        if !no_default_excludes && is_default_excluded(&rel_path) {
            continue;
        }
        if is_hidden_path(&rel_path) {
            continue;
        }
        candidates.push(rel_path);
    }

    let ignored = git_ignored_paths(root, &candidates)?;
    let mut files = Vec::new();
    for rel_path in candidates {
        if ignored.contains(rel_path.as_str()) {
            continue;
        }
        if !ext_filter.is_empty()
            && !ext_filter
                .iter()
                .any(|ext| rel_path.ends_with(ext.as_str()))
        {
            continue;
        }
        if is_probably_binary_path(&rel_path) {
            continue;
        }
        if !has_supported_plugin(&root.join(&rel_path), &rel_path, registry, ext_filter) {
            continue;
        }
        files.push(rel_path);
    }

    files.sort();
    files.dedup();
    Some(files)
}

fn git_missing_from_worktree_paths(root: &Path) -> Option<HashSet<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "--deleted"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut missing: HashSet<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).replace('\\', "/"))
        .collect();

    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "-v"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    for raw_entry in output.stdout.split(|byte| *byte == 0) {
        if raw_entry.len() < 3 || raw_entry[0] != b'S' || raw_entry[1] != b' ' {
            continue;
        }
        missing.insert(String::from_utf8_lossy(&raw_entry[2..]).replace('\\', "/"));
    }
    for path in git_non_file_symlink_paths(root)? {
        missing.insert(path);
    }

    Some(missing)
}

fn git_non_file_symlink_paths(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "-s"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut paths = Vec::new();
    for raw_entry in output.stdout.split(|byte| *byte == 0) {
        if !raw_entry.starts_with(b"120000 ") {
            continue;
        }
        let Some(tab_pos) = raw_entry.iter().position(|byte| *byte == b'\t') else {
            return None;
        };
        let path = String::from_utf8_lossy(&raw_entry[tab_pos + 1..]).replace('\\', "/");
        if !root.join(&path).is_file() {
            paths.push(path);
        }
    }
    Some(paths)
}

fn git_ignored_paths(root: &Path, paths: &[String]) -> Option<HashSet<String>> {
    if paths.is_empty() {
        return Some(HashSet::new());
    }

    let mut child = Command::new("git")
        .current_dir(root)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    {
        let stdin = child.stdin.as_mut()?;
        for path in paths {
            stdin.write_all(path.as_bytes()).ok()?;
            stdin.write_all(b"\0").ok()?;
        }
    }
    let output = child.wait_with_output().ok()?;
    if !matches!(output.status.code(), Some(0) | Some(1)) {
        return None;
    }

    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).replace('\\', "/"))
            .collect(),
    )
}

fn is_hidden_path(rel_path: &str) -> bool {
    rel_path
        .split('/')
        .any(|component| component.starts_with('.') && component.len() > 1)
}

fn has_supported_plugin(
    path: &Path,
    rel_path: &str,
    registry: &ParserRegistry,
    ext_filter: &[String],
) -> bool {
    if registry.get_explicit_plugin(rel_path).is_some() {
        return true;
    }

    if !ext_filter.is_empty() || Path::new(rel_path).extension().is_some() {
        return false;
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };

    registry.detect_plugin_from_content(&content).is_some()
}

pub fn file_path_for_entity(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::parser::plugins::create_default_registry;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> std::path::PathBuf {
        let name = format!(
            "sem-cli-files-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn scan_skips_binary_files_and_default_excludes() {
        let root = temp_dir();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            root.join("src/run"),
            "#!/usr/bin/env node\nfunction main() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/notes.weird"), "plain text\n").unwrap();
        fs::write(root.join("src/blob.weird"), b"abc\0def").unwrap();
        fs::write(root.join("src/icon.png"), b"\x89PNG\r\n").unwrap();
        fs::write(root.join("dist/generated.js"), "function generated() {}\n").unwrap();

        let registry = create_default_registry();
        let files = find_supported_files_in_path(&root, &root, &registry, &[], false);

        assert_eq!(
            files,
            vec!["src/main.rs".to_string(), "src/run".to_string()]
        );

        let files_with_generated = find_supported_files_in_path(&root, &root, &registry, &[], true);
        assert!(files_with_generated.contains(&"src/main.rs".to_string()));
        assert!(files_with_generated.contains(&"src/run".to_string()));
        assert!(files_with_generated.contains(&"dist/generated.js".to_string()));
        assert!(!files_with_generated.contains(&"src/notes.weird".to_string()));
        assert!(!files_with_generated.contains(&"src/blob.weird".to_string()));
        assert!(!files_with_generated.contains(&"src/icon.png".to_string()));

        let rs_files =
            find_supported_files_in_path(&root, &root, &registry, &[".rs".to_string()], true);
        assert_eq!(rs_files, vec!["src/main.rs".to_string()]);

        fs::remove_dir_all(root).unwrap();
    }
}
