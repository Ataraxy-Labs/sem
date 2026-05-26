use crate::parser::differ::DiffResult;
use serde_json::{json, Value};

pub fn diff_json_value(result: &DiffResult) -> Value {
    let changes: Vec<Value> = result
        .changes
        .iter()
        .map(|c| {
            json!({
                "entityId": c.entity_id,
                "changeType": c.change_type,
                "entityType": c.entity_type,
                "entityName": c.entity_name,
                "startLine": c.start_line,
                "endLine": c.end_line,
                "oldStartLine": c.old_start_line,
                "oldEndLine": c.old_end_line,
                "oldEntityName": c.old_entity_name,
                "filePath": c.file_path,
                "oldFilePath": c.old_file_path,
                "oldParentId": c.old_parent_id,
                "beforeContent": c.before_content,
                "afterContent": c.after_content,
                "commitSha": c.commit_sha,
                "author": c.author,
                "structuralChange": c.structural_change,
            })
        })
        .collect();

    json!({
        "summary": {
            "fileCount": result.file_count,
            "added": result.added_count,
            "modified": result.modified_count,
            "deleted": result.deleted_count,
            "moved": result.moved_count,
            "renamed": result.renamed_count,
            "reordered": result.reordered_count,
            "orphan": result.orphan_count,
            "total": result.changes.len(),
        },
        "changes": changes,
    })
}

pub fn format_diff_json(result: &DiffResult) -> String {
    serde_json::to_string(&diff_json_value(result)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::change::{ChangeType, SemanticChange};

    #[test]
    fn diff_json_value_matches_cli_envelope() {
        let result = DiffResult {
            changes: vec![SemanticChange {
                id: "internal-change-id".to_string(),
                entity_id: "src/lib.rs::function::foo".to_string(),
                change_type: ChangeType::Modified,
                entity_type: "function".to_string(),
                entity_name: "foo".to_string(),
                entity_line: 12,
                start_line: 12,
                end_line: 12,
                old_start_line: None,
                old_end_line: None,
                parent_name: Some("module".to_string()),
                file_path: "src/lib.rs".to_string(),
                old_entity_name: Some("bar".to_string()),
                old_file_path: Some("src/old.rs".to_string()),
                old_parent_id: Some("old-parent".to_string()),
                before_content: Some("fn bar() {}".to_string()),
                after_content: Some("fn foo() {}".to_string()),
                commit_sha: Some("abc123".to_string()),
                author: Some("Ada".to_string()),
                timestamp: Some("2026-05-26".to_string()),
                structural_change: Some(true),
            }],
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

        let value = diff_json_value(&result);

        assert_eq!(
            value,
            json!({
                "summary": {
                    "fileCount": 1,
                    "added": 0,
                    "modified": 1,
                    "deleted": 0,
                    "moved": 0,
                    "renamed": 0,
                    "reordered": 0,
                    "orphan": 0,
                    "total": 1,
                },
                "changes": [{
                    "entityId": "src/lib.rs::function::foo",
                    "changeType": "modified",
                    "entityType": "function",
                    "entityName": "foo",
                    "startLine": 12,
                    "endLine": 12,
                    "oldStartLine": null,
                    "oldEndLine": null,
                    "oldEntityName": "bar",
                    "filePath": "src/lib.rs",
                    "oldFilePath": "src/old.rs",
                    "oldParentId": "old-parent",
                    "beforeContent": "fn bar() {}",
                    "afterContent": "fn foo() {}",
                    "commitSha": "abc123",
                    "author": "Ada",
                    "structuralChange": true,
                }],
            })
        );
    }
}
