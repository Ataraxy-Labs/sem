use sem_core::parser::differ::DiffResult;
use serde_json::json;

pub fn format_json(result: &DiffResult) -> String {
    let changes: Vec<serde_json::Value> = result
        .changes
        .iter()
        .map(|c| {
            json!({
                "entityId": c.entity_id,
                "changeType": c.change_type,
                "entityType": c.entity_type,
                "entityName": c.entity_name,
                "filePath": c.file_path,
                "oldFilePath": c.old_file_path,
                "beforeContent": c.before_content,
                "afterContent": c.after_content,
                "commitSha": c.commit_sha,
                "author": c.author,
            })
        })
        .collect();

    let output = json!({
        "summary": {
            "fileCount": result.file_count,
            "added": result.added_count,
            "modified": result.modified_count,
            "deleted": result.deleted_count,
            "moved": result.moved_count,
            "renamed": result.renamed_count,
            "total": result.changes.len(),
        },
        "changes": changes,
    });

    serde_json::to_string_pretty(&output).unwrap_or_default()
}
