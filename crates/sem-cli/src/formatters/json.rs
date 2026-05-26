use sem_core::parser::differ::{BinaryFileChange, DiffResult};

pub fn format_json(result: &DiffResult, binary_changes: &[BinaryFileChange]) -> String {
    sem_core::format::json::format_diff_json_with_binary_changes(result, binary_changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::git::types::{FileChange, FileStatus};
    use sem_core::model::change::SemanticChange;
    use sem_core::parser::differ::{compute_semantic_diff, BinaryFileChange};
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

        let output: serde_json::Value = serde_json::from_str(&format_json(&result, &[])).unwrap();
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

    #[test]
    fn diff_json_includes_current_and_previous_line_spans() {
        let change: SemanticChange = serde_json::from_value(serde_json::json!({
            "id": "change::a.ts::function::foo",
            "entityId": "a.ts::function::foo",
            "changeType": "modified",
            "entityType": "function",
            "entityName": "foo",
            "entityLine": 7,
            "startLine": 7,
            "endLine": 9,
            "oldStartLine": 3,
            "oldEndLine": 5,
            "filePath": "a.ts",
            "beforeContent": "function foo() { return 1; }",
            "afterContent": "function foo() { return 999; }",
            "structuralChange": true
        }))
        .unwrap();

        let result = DiffResult {
            changes: vec![change],
            file_count: 1,
            added_count: 0,
            modified_count: 1,
            deleted_count: 0,
            moved_count: 0,
            renamed_count: 0,
            reordered_count: 0,
            orphan_count: 0,
            total_entities_before: 1,
            total_entities_after: 1,
        };

        let output: serde_json::Value = serde_json::from_str(&format_json(&result, &[])).unwrap();
        let change = &output["changes"][0];

        assert_eq!(change["startLine"], 7);
        assert_eq!(change["endLine"], 9);
        assert_eq!(change["oldStartLine"], 3);
        assert_eq!(change["oldEndLine"], 5);
    }

    #[test]
    fn json_includes_binary_changes_in_summary_and_binary_changes() {
        let result = DiffResult {
            changes: Vec::new(),
            file_count: 0,
            added_count: 0,
            modified_count: 0,
            deleted_count: 0,
            moved_count: 0,
            renamed_count: 0,
            reordered_count: 0,
            orphan_count: 0,
            total_entities_before: 0,
            total_entities_after: 0,
        };
        let binary_changes = vec![BinaryFileChange {
            file_path: "pic.png".to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
        }];

        let value: serde_json::Value =
            serde_json::from_str(&format_json(&result, &binary_changes)).unwrap();

        assert_eq!(value["summary"]["fileCount"], 1);
        assert_eq!(value["summary"]["binary"], 1);
        assert_eq!(value["summary"]["total"], 1);
        assert_eq!(value["changes"].as_array().unwrap().len(), 0);
        assert_eq!(value["binaryChanges"][0]["changeType"], "binary");
        assert_eq!(value["binaryChanges"][0]["filePath"], "pic.png");
        assert_eq!(value["binaryChanges"][0]["fileStatus"], "modified");
    }
}
