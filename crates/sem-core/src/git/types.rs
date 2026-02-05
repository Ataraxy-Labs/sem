use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum DiffScope {
    Working,
    Staged,
    Commit { sha: String },
    Range { from: String, to: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub file_path: String,
    pub status: FileStatus,
    pub old_file_path: Option<String>,
    pub before_content: Option<String>,
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
