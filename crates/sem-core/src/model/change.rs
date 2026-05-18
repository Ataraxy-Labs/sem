use serde::{Deserialize, Serialize};

fn is_zero(value: &usize) -> bool {
    *value == 0
}

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
    /// Inclusive end line for the primary entity side. For deleted changes this
    /// is the before-side span; for all other changes it is the after-side span.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub entity_end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_name: Option<String>,
    pub file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_entity_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_file_path: Option<String>,
    /// Before-side start line when a corresponding before entity exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_entity_line: Option<usize>,
    /// Inclusive before-side end line when a corresponding before entity exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_entity_end_line: Option<usize>,
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
}
