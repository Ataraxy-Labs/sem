use sem_core::model::change::ChangeType;
use sem_core::parser::differ::{BinaryFileChange, DiffResult};

pub mod json;
pub mod markdown;
pub mod plain;
pub mod terminal;

pub(crate) fn binary_display_name(change: &BinaryFileChange) -> String {
    match change.old_file_path.as_deref() {
        Some(old_path) if old_path != change.file_path => {
            format!("{old_path} -> {}", change.file_path)
        }
        _ => change.file_path.clone(),
    }
}

pub(crate) fn has_reportable_changes(
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
) -> bool {
    !result.changes.is_empty() || !binary_changes.is_empty()
}

pub(crate) fn file_count(result: &DiffResult, binary_changes: &[BinaryFileChange]) -> usize {
    result.file_count + binary_changes.len()
}

pub(crate) fn orphan_summary_parts(result: &DiffResult) -> Vec<String> {
    let mut added = 0;
    let mut modified = 0;
    let mut deleted = 0;
    let mut moved = 0;
    let mut renamed = 0;
    let mut reordered = 0;

    for change in result.changes.iter().filter(|c| c.entity_type == "orphan") {
        match change.change_type {
            ChangeType::Added => added += 1,
            ChangeType::Modified => modified += 1,
            ChangeType::Deleted => deleted += 1,
            ChangeType::Moved => moved += 1,
            ChangeType::Renamed => renamed += 1,
            ChangeType::Reordered => reordered += 1,
        }
    }

    [
        (added, "added"),
        (modified, "modified"),
        (deleted, "deleted"),
        (moved, "moved"),
        (renamed, "renamed"),
        (reordered, "reordered"),
    ]
    .into_iter()
    .filter_map(|(count, label)| {
        if count == 0 {
            None
        } else {
            let noun = if count == 1 { "orphan" } else { "orphans" };
            Some(format!("{count} {label} {noun}"))
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::git::types::{FileChange, FileStatus};
    use sem_core::parser::differ::compute_semantic_diff;
    use sem_core::parser::plugins::create_default_registry;

    fn modified_file(path: &str, before: &str, after: &str) -> FileChange {
        FileChange {
            file_path: path.to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(before.to_string()),
            after_content: Some(after.to_string()),
        }
    }

    #[test]
    fn orphan_summary_parts_partition_orphans_by_change_type() {
        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[
                modified_file(
                    "added.py",
                    "def foo():\n    return 1\n",
                    "# just a comment\n",
                ),
                modified_file(
                    "modified.py",
                    "# old comment\n\ndef bar():\n    return 2\n",
                    "# new comment\n\ndef bar():\n    return 2\n",
                ),
            ],
            &registry,
            None,
            None,
        );

        assert_eq!(
            orphan_summary_parts(&result),
            vec![
                "1 added orphan".to_string(),
                "1 modified orphan".to_string()
            ]
        );
    }
}
