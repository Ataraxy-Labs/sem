pub mod blame;
pub mod cloud;
pub mod context;
pub mod diff;
pub mod entities;
pub mod files;
pub mod graph;
pub mod impact;
pub mod log;
pub mod setup;
pub mod stats;
pub mod update;

use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use std::path::{Component, Path, PathBuf};

use sem_core::git::bridge::GitBridge;

/// Create a parser registry with extension mappings loaded from `cwd`.
/// Loads `.semrc` first (takes priority), then `.gitattributes` as fallback.
pub fn create_registry(cwd: &str) -> ParserRegistry {
    let mut registry = create_default_registry();
    let root = Path::new(cwd);
    registry.load_semrc(root);
    registry.load_gitattributes(root);
    registry
}

pub fn repo_root_or_cwd(cwd: &str) -> PathBuf {
    GitBridge::open(Path::new(cwd))
        .map(|git| git.repo_root().to_path_buf())
        .unwrap_or_else(|_| Path::new(cwd).to_path_buf())
}

pub fn normalize_repo_relative_path(cwd: &Path, repo_root: &Path, path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    if path.starts_with(':') {
        return path.to_string();
    }

    let path = Path::new(path);
    let cwd_base = normalize_existing_prefix(cwd).unwrap_or_else(|| normalize_lexical(cwd));
    let repo_root_base =
        normalize_existing_prefix(repo_root).unwrap_or_else(|| normalize_lexical(repo_root));
    let absolute = if path.is_absolute() {
        normalize_lexical(path)
    } else {
        normalize_lexical(&cwd_base.join(path))
    };

    let repo_root = normalize_lexical(&repo_root_base);
    let Ok(relative) = absolute.strip_prefix(&repo_root) else {
        return absolute.to_string_lossy().replace('\\', "/");
    };

    if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn normalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }

    let mut missing = Vec::new();
    let mut current = path;

    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            missing.push(name.to_os_string());
        }
        if let Ok(mut canonical) = std::fs::canonicalize(parent) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(normalize_lexical(&canonical));
        }
        current = parent;
    }

    None
}

pub fn entity_matches_query(entity: &sem_core::parser::graph::EntityInfo, query: &str) -> bool {
    if entity.name == query {
        return true;
    }

    let Some((entity_type, name)) = split_type_qualified_query(query) else {
        return false;
    };

    entity.entity_type == entity_type && entity.name == name
}

/// Like `entity_matches_query`, but also resolves `Class.method` (or
/// `Outer.Inner.method`) addressing: `entity` matches `Parent.child` when its
/// own name is `child` and its parent entity is named `Parent`. Needs the graph
/// to look up the parent. Agents reach for `Class.method` naturally, so every
/// entity-addressing command should accept it.
pub fn entity_matches_qualified(
    graph: &sem_core::parser::graph::EntityGraph,
    entity: &sem_core::parser::graph::EntityInfo,
    query: &str,
) -> bool {
    if entity_matches_query(entity, query) {
        return true;
    }
    if let Some((parent_part, child_part)) = query.rsplit_once('.') {
        if entity.name == child_part {
            if let Some(pid) = &entity.parent_id {
                if let Some(parent) = graph.entities.get(pid) {
                    return parent.name == parent_part;
                }
            }
        }
    }
    false
}

fn split_type_qualified_query(query: &str) -> Option<(&str, &str)> {
    let (entity_type, name) = query.split_once(' ')?;
    if entity_type.is_empty() || name.is_empty() {
        return None;
    }

    Some((entity_type, name))
}

/// Truncate a string to `max_chars` Unicode scalar values (codepoints), appending "..." if
/// truncated. Safe for multibyte encodings (CJK, simple emoji). Note: does not split on grapheme
/// cluster boundaries — ZWJ emoji sequences may render incorrectly at the truncation point.
///
/// If `max_chars <= 3`, no ellipsis is appended (no room); the string is simply truncated.
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars <= 3 {
        return s.chars().take(max_chars).collect();
    }
    // Use char_indices to find the byte boundary in a single pass
    let mut last_boundary = 0;
    let mut truncate_boundary = 0;
    let mut count = 0;
    for (i, c) in s.char_indices() {
        count += 1;
        if count == max_chars - 3 {
            truncate_boundary = i + c.len_utf8();
        }
        if count == max_chars {
            last_boundary = i + c.len_utf8();
            break;
        }
    }
    if count < max_chars {
        // String fits within max_chars — return as-is
        s.to_string()
    } else if s[last_boundary..].is_empty() {
        // Exactly max_chars — return as-is
        s.to_string()
    } else {
        // String exceeds max_chars — truncate with ellipsis
        format!("{}...", &s[..truncate_boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        entity_matches_query, normalize_existing_prefix, normalize_lexical,
        normalize_repo_relative_path, truncate_str,
    };
    use sem_core::parser::graph::EntityInfo;
    use std::path::Path;

    fn entity(entity_type: &str, name: &str) -> EntityInfo {
        EntityInfo {
            id: format!("a.ts::{entity_type}::{name}"),
            name: name.to_string(),
            entity_type: entity_type.to_string(),
            file_path: "a.ts".to_string(),
            parent_id: None,
            start_line: 1,
            end_line: 1,
        }
    }

    #[test]
    fn entity_query_matches_exact_name() {
        let entity = entity("function", "getter value");

        assert!(entity_matches_query(&entity, "getter value"));
    }

    #[test]
    fn entity_query_matches_type_qualified_name() {
        let entity = entity("getter", "value");

        assert!(entity_matches_query(&entity, "getter value"));
        assert!(!entity_matches_query(&entity, "setter value"));
        assert!(!entity_matches_query(&entity, "method value"));
    }

    #[test]
    fn ascii_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn ascii_exact_length_unchanged() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn ascii_truncated_with_ellipsis() {
        // 6 chars > max 5, so take 2 chars + "..."
        assert_eq!(truncate_str("abcdef", 5), "ab...");
    }

    #[test]
    fn cjk_short_string_unchanged() {
        assert_eq!(truncate_str("日本語", 10), "日本語");
    }

    #[test]
    fn cjk_truncated_at_char_boundary() {
        // This was the original bug — byte-index slicing panicked on CJK chars.
        // "bff側でwebsocketエラーが頻発している問題を修正" is 28 chars
        let msg = "bff側でwebsocketエラーが頻発している問題を修正";
        let result = truncate_str(msg, 15);
        // 15 - 3 = 12 chars kept + "..."
        assert_eq!(result.chars().count(), 15);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn emoji_truncated_at_char_boundary() {
        let msg = "🎉🚀✨ feat: add new feature with celebration";
        let result = truncate_str(msg, 10);
        // 10 - 3 = 7 chars kept + "..."
        assert_eq!(result.chars().count(), 10);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn mixed_cjk_ascii_truncation() {
        // Reproduces the exact scenario that caused the original panic:
        // byte-index slicing at 37 landed inside '頻' (bytes 36..39)
        let msg = ":bug: bff側でwebsocketエラーが頻発している問題を修正";
        // 35 chars, truncate at 20 to force truncation
        let result = truncate_str(msg, 20);
        assert_eq!(result.chars().count(), 20);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn empty_string() {
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn max_chars_zero() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn max_chars_one() {
        assert_eq!(truncate_str("hello", 1), "h");
    }

    #[test]
    fn max_chars_three_with_longer_string() {
        // Boundary: max_chars == 3, string is longer → no room for "...", just take 3 chars
        assert_eq!(truncate_str("hello", 3), "hel");
    }

    #[test]
    fn max_chars_four_triggers_ellipsis() {
        // max_chars == 4, string is longer → take 1 char + "..."
        assert_eq!(truncate_str("hello", 4), "h...");
    }

    #[test]
    fn normalize_repo_relative_path_handles_absolute_paths() {
        let cwd = Path::new("/repo/sub");
        let repo_root = Path::new("/repo");

        let normalized = normalize_repo_relative_path(cwd, repo_root, "/repo/sub/foo.py");

        assert_eq!(normalized, "sub/foo.py");
    }

    #[test]
    fn normalize_repo_relative_path_handles_parent_components() {
        let cwd = Path::new("/repo/sub/nested");
        let repo_root = Path::new("/repo");

        let normalized = normalize_repo_relative_path(cwd, repo_root, "../foo.py");

        assert_eq!(normalized, "sub/foo.py");
    }

    #[test]
    fn normalize_repo_relative_path_keeps_repo_root_dot_as_all_paths() {
        let cwd = Path::new("/repo");
        let repo_root = Path::new("/repo");

        let normalized = normalize_repo_relative_path(cwd, repo_root, ".");

        assert_eq!(normalized, ".");
    }

    #[test]
    fn normalize_repo_relative_path_treats_empty_path_as_dot() {
        let cwd = Path::new("/repo/sub");
        let repo_root = Path::new("/repo");

        let normalized = normalize_repo_relative_path(cwd, repo_root, "");

        assert_eq!(normalized, ".");
    }

    #[test]
    fn normalize_repo_relative_path_converts_subdir_dot_to_subdir() {
        let cwd = Path::new("/repo/sub");
        let repo_root = Path::new("/repo");

        let normalized = normalize_repo_relative_path(cwd, repo_root, ".");

        assert_eq!(normalized, "sub");
    }

    #[test]
    fn normalize_repo_relative_path_leaves_magic_pathspecs_unchanged() {
        let cwd = Path::new("/repo/sub");
        let repo_root = Path::new("/repo");

        let normalized = normalize_repo_relative_path(cwd, repo_root, ":(glob)**/*.py");

        assert_eq!(normalized, ":(glob)**/*.py");
    }

    #[test]
    fn normalize_repo_relative_path_returns_normalized_absolute_path_outside_repo() {
        use std::fs;

        let repo_root =
            std::env::temp_dir().join(format!("sem-normalize-outside-test-{}", std::process::id()));
        let cwd = repo_root.join("sub");
        fs::create_dir_all(&cwd).expect("create cwd");
        let outside_path = cwd.join("../../outside.py");
        let expected = normalize_existing_prefix(&outside_path)
            .unwrap_or_else(|| normalize_lexical(&outside_path))
            .to_string_lossy()
            .replace('\\', "/");

        let normalized = normalize_repo_relative_path(&cwd, &repo_root, "../../outside.py");

        assert_eq!(normalized, expected);
        fs::remove_dir_all(repo_root).expect("remove temp dir");
    }

    #[cfg(unix)]
    #[test]
    fn normalize_repo_relative_path_handles_symlinked_cwd() {
        use std::fs;
        use std::os::unix::fs::symlink;
        use std::time::{SystemTime, UNIX_EPOCH};

        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let temp = std::env::temp_dir().join(format!(
            "sem-normalize-repo-relative-test-{}-{id}",
            std::process::id()
        ));
        let repo_root = temp.join("repo");
        let real_subdir = repo_root.join("sub");
        let symlinked_cwd = temp.join("linked-sub");
        fs::create_dir_all(&real_subdir).expect("create real cwd");
        symlink(&real_subdir, &symlinked_cwd).expect("create symlinked cwd");

        let normalized = normalize_repo_relative_path(&symlinked_cwd, &repo_root, "foo.py");

        assert_eq!(normalized, "sub/foo.py");
        fs::remove_dir_all(temp).expect("remove temp dir");
    }

    #[cfg(unix)]
    #[test]
    fn normalize_repo_relative_path_resolves_missing_cwd_through_symlinked_repo_root() {
        use std::fs;
        use std::os::unix::fs::symlink;
        use std::time::{SystemTime, UNIX_EPOCH};

        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let temp = std::env::temp_dir().join(format!(
            "sem-normalize-missing-cwd-test-{}-{id}",
            std::process::id()
        ));
        let repo_root = temp.join("repo");
        let symlinked_repo_root = temp.join("linked-repo");
        fs::create_dir_all(&repo_root).expect("create repo root");
        symlink(&repo_root, &symlinked_repo_root).expect("create symlinked repo root");
        let missing_cwd = symlinked_repo_root.join("missing");

        let normalized = normalize_repo_relative_path(&missing_cwd, &repo_root, "foo.py");

        assert_eq!(normalized, "missing/foo.py");
        fs::remove_dir_all(temp).expect("remove temp dir");
    }
}
