use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Moved,
    Renamed,
    Reordered,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeType::Added => write!(f, "added"),
            ChangeType::Modified => write!(f, "modified"),
            ChangeType::Deleted => write!(f, "deleted"),
            ChangeType::Moved => write!(f, "moved"),
            ChangeType::Renamed => write!(f, "renamed"),
            ChangeType::Reordered => write!(f, "reordered"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SemanticChange {
    pub id: String,
    pub entity_id: String,
    pub change_type: ChangeType,
    pub entity_type: String,
    pub entity_name: String,
    #[serde(default)]
    pub entity_line: usize,
    #[serde(default)]
    pub start_line: usize,
    #[serde(default)]
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_name: Option<String>,
    pub file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_entity_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Whether the AST structure changed (true) or only formatting/comments (false).
    /// None when structural hash is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structural_change: Option<bool>,
    /// For an `entry` change (`sem diff --entries`): which container literal
    /// and which key. `None` for every other change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryRef>,
}

/// The container and key of one element of a container literal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRef {
    /// The binding holding the literal, qualified by its enclosing entities:
    /// `ClickHouseParser.FUNCTIONS`, `Builtins`.
    pub container: String,
    /// The key as written, unquoted for string keys: `TOUNIXTIMESTAMP`,
    /// `exp.TimeToUnix`, or a spread's expression `parser.Parser.FUNCTIONS`.
    pub key: String,
    /// `string`, `identifier`, `attribute`, `number`, `constant`, `spread`,
    /// `field` (Go struct field), `name_field` (Go slice element named by its
    /// `Name:` field), `index` (positional), `computed`, or `expr`.
    pub key_kind: String,
}

impl SemanticChange {
    pub fn has_content_change(&self) -> bool {
        match (&self.before_content, &self.after_content) {
            (Some(before), Some(after)) => before != after,
            _ => false,
        }
    }
}
