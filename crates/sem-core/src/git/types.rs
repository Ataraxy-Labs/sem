use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum DiffScope {
    Working,
    Staged,
    Commit {
        sha: String,
    },
    Range {
        from: String,
        to: String,
    },
    /// Compare a ref's tree to the working directory (like `git diff <ref>`)
    RefToWorking {
        refspec: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl std::fmt::Display for FileStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileStatus::Added => write!(f, "added"),
            FileStatus::Modified => write!(f, "modified"),
            FileStatus::Deleted => write!(f, "deleted"),
            FileStatus::Renamed => write!(f, "renamed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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

/// A commit together with the file path that was active at that commit.
/// Used by `get_file_commits_follow_renames` to track files across renames.
#[derive(Debug, Clone)]
pub struct FileCommitInfo {
    pub commit: CommitInfo,
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameLineInfo {
    pub line_number: usize,
    pub commit_sha: Option<String>,
    pub author: String,
    pub author_time: Option<i64>,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::{FileChange, FileStatus};

    #[test]
    fn file_change_rejects_unknown_fields() {
        let err = serde_json::from_str::<FileChange>(
            r#"{"filePath":"b.ts","oldPath":"a.ts","status":"renamed"}"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown field `oldPath`"));
    }

    #[test]
    fn file_change_accepts_known_camel_case_fields() {
        let change = serde_json::from_str::<FileChange>(
            r#"{"filePath":"b.ts","oldFilePath":"a.ts","status":"renamed"}"#,
        )
        .unwrap();

        assert_eq!(change.file_path, "b.ts");
        assert_eq!(change.old_file_path.as_deref(), Some("a.ts"));
        assert_eq!(change.status, FileStatus::Renamed);
    }
}
