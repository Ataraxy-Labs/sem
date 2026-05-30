use sem_core::parser::differ::DiffResult;

pub fn format_json(result: &DiffResult) -> String {
    sem_core::format::json::format_diff_json(result)
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
    fn summary_change_type_buckets_sum_to_total_with_orphans() {
        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[modified_file(
                "svc.py",
                "def foo():\n    return 1\n",
                "# just a comment\n",
            )],
            &registry,
            None,
            None,
        );

        let output: serde_json::Value = serde_json::from_str(&format_json(&result)).unwrap();
        let summary = &output["summary"];
        let bucket_total = summary["added"].as_u64().unwrap()
            + summary["modified"].as_u64().unwrap()
            + summary["deleted"].as_u64().unwrap()
            + summary["moved"].as_u64().unwrap()
            + summary["renamed"].as_u64().unwrap()
            + summary["reordered"].as_u64().unwrap();

        assert_eq!(bucket_total, summary["total"].as_u64().unwrap());
        assert_eq!(summary["orphan"], 1);
    }
}
