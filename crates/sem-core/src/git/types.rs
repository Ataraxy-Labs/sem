use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum DiffScope {
    Working,
    Staged,
    Commit { sha: String },
    Range { from: String, to: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub file_path: String,
    pub status: FileStatus,
    #[serde(default)]
    pub old_file_path: Option<String>,
    #[serde(default)]
    pub before_content: Option<String>,
    #[serde(default)]
    pub after_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub date: String,
    pub message: String,
}
