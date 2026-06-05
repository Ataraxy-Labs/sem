use crate::parser::differ::{BinaryFileChange, DiffResult};
use serde::ser::{Serialize, SerializeSeq, SerializeStruct, Serializer};
use serde_json::Value;

struct DiffJsonEnvelope<'a> {
    result: &'a DiffResult,
    binary_changes: &'a [BinaryFileChange],
    include_binary_changes: bool,
}

impl Serialize for DiffJsonEnvelope<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let field_count = if self.include_binary_changes { 3 } else { 2 };
        let mut fields = serializer.serialize_struct("DiffJsonEnvelope", field_count)?;
        fields.serialize_field(
            "summary",
            &DiffJsonSummary {
                result: self.result,
                binary_count: self.binary_changes.len(),
                include_binary_count: self.include_binary_changes,
            },
        )?;
        fields.serialize_field("changes", &SemanticChangesJson(&self.result.changes))?;
        if self.include_binary_changes {
            fields.serialize_field("binaryChanges", &BinaryChangesJson(self.binary_changes))?;
        }
        fields.end()
    }
}

struct DiffJsonSummary<'a> {
    result: &'a DiffResult,
    binary_count: usize,
    include_binary_count: bool,
}

impl Serialize for DiffJsonSummary<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let field_count = if self.include_binary_count { 10 } else { 9 };
        let mut fields = serializer.serialize_struct("DiffJsonSummary", field_count)?;
        fields.serialize_field("fileCount", &(self.result.file_count + self.binary_count))?;
        fields.serialize_field("added", &self.result.added_count)?;
        fields.serialize_field("modified", &self.result.modified_count)?;
        fields.serialize_field("deleted", &self.result.deleted_count)?;
        fields.serialize_field("moved", &self.result.moved_count)?;
        fields.serialize_field("renamed", &self.result.renamed_count)?;
        fields.serialize_field("reordered", &self.result.reordered_count)?;
        if self.include_binary_count {
            fields.serialize_field("binary", &self.binary_count)?;
        }
        fields.serialize_field("orphan", &self.result.orphan_count)?;
        fields.serialize_field("total", &(self.result.changes.len() + self.binary_count))?;
        fields.end()
    }
}

struct SemanticChangesJson<'a>(&'a [crate::model::change::SemanticChange]);

impl Serialize for SemanticChangesJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for change in self.0 {
            sequence.serialize_element(&SemanticChangeJson(change))?;
        }
        sequence.end()
    }
}

struct SemanticChangeJson<'a>(&'a crate::model::change::SemanticChange);

impl Serialize for SemanticChangeJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let change = self.0;
        let mut fields = serializer.serialize_struct("SemanticChangeJson", 17)?;
        fields.serialize_field("entityId", &change.entity_id)?;
        fields.serialize_field("changeType", &change.change_type)?;
        fields.serialize_field("entityType", &change.entity_type)?;
        fields.serialize_field("entityName", &change.entity_name)?;
        fields.serialize_field("startLine", &change.start_line)?;
        fields.serialize_field("endLine", &change.end_line)?;
        fields.serialize_field("oldStartLine", &change.old_start_line)?;
        fields.serialize_field("oldEndLine", &change.old_end_line)?;
        fields.serialize_field("oldEntityName", &change.old_entity_name)?;
        fields.serialize_field("filePath", &change.file_path)?;
        fields.serialize_field("oldFilePath", &change.old_file_path)?;
        fields.serialize_field("oldParentId", &change.old_parent_id)?;
        fields.serialize_field("beforeContent", &change.before_content)?;
        fields.serialize_field("afterContent", &change.after_content)?;
        fields.serialize_field("commitSha", &change.commit_sha)?;
        fields.serialize_field("author", &change.author)?;
        fields.serialize_field("structuralChange", &change.structural_change)?;
        fields.end()
    }
}

struct BinaryChangesJson<'a>(&'a [BinaryFileChange]);

impl Serialize for BinaryChangesJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for change in self.0 {
            sequence.serialize_element(&BinaryChangeJson(change))?;
        }
        sequence.end()
    }
}

struct BinaryChangeJson<'a>(&'a BinaryFileChange);

impl Serialize for BinaryChangeJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let change = self.0;
        let mut fields = serializer.serialize_struct("BinaryChangeJson", 4)?;
        fields.serialize_field("changeType", "binary")?;
        fields.serialize_field("filePath", &change.file_path)?;
        fields.serialize_field("oldFilePath", &change.old_file_path)?;
        fields.serialize_field("fileStatus", &change.status)?;
        fields.end()
    }
}

fn estimate_json_capacity(result: &DiffResult, binary_changes: &[BinaryFileChange]) -> usize {
    let content_len = result
        .changes
        .iter()
        .map(|change| {
            change.before_content.as_ref().map_or(0, String::len)
                + change.after_content.as_ref().map_or(0, String::len)
        })
        .sum::<usize>();

    256 + content_len + result.changes.len() * 256 + binary_changes.len() * 128
}

pub fn diff_json_value(result: &DiffResult) -> Value {
    diff_json_value_inner(result, &[], false)
}

pub fn diff_json_value_with_binary_changes(
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
) -> Value {
    diff_json_value_inner(result, binary_changes, true)
}

fn diff_json_value_inner(
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
    include_binary_changes: bool,
) -> Value {
    serde_json::to_value(DiffJsonEnvelope {
        result,
        binary_changes,
        include_binary_changes,
    })
    .unwrap_or(Value::Null)
}

fn format_diff_json_inner(
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
    include_binary_changes: bool,
) -> String {
    let mut output = Vec::with_capacity(estimate_json_capacity(result, binary_changes));
    let envelope = DiffJsonEnvelope {
        result,
        binary_changes,
        include_binary_changes,
    };
    if serde_json::to_writer(&mut output, &envelope).is_err() {
        return String::new();
    }
    String::from_utf8(output).unwrap_or_default()
}

pub fn format_diff_json(result: &DiffResult) -> String {
    format_diff_json_inner(result, &[], false)
}

pub fn format_diff_json_with_binary_changes(
    result: &DiffResult,
    binary_changes: &[BinaryFileChange],
) -> String {
    format_diff_json_inner(result, binary_changes, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::change::{ChangeType, SemanticChange};
    use serde_json::json;

    #[test]
    fn diff_json_value_preserves_public_schema() {
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
        let formatted_value: Value =
            serde_json::from_str(&format_diff_json(&result)).expect("format should be valid json");
        assert_eq!(formatted_value, value);

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

    #[test]
    fn diff_json_value_with_binary_changes_matches_cli_envelope() {
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
            status: crate::git::types::FileStatus::Modified,
            old_file_path: None,
        }];

        let value = diff_json_value_with_binary_changes(&result, &binary_changes);
        let formatted_value: Value = serde_json::from_str(&format_diff_json_with_binary_changes(
            &result,
            &binary_changes,
        ))
        .expect("format should be valid json");
        assert_eq!(formatted_value, value);

        assert_eq!(
            value,
            json!({
                "summary": {
                    "fileCount": 1,
                    "added": 0,
                    "modified": 0,
                    "deleted": 0,
                    "moved": 0,
                    "renamed": 0,
                    "reordered": 0,
                    "binary": 1,
                    "orphan": 0,
                    "total": 1,
                },
                "changes": [],
                "binaryChanges": [{
                    "changeType": "binary",
                    "filePath": "pic.png",
                    "oldFilePath": null,
                    "fileStatus": "modified",
                }],
            })
        );
    }
}
