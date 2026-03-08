use std::fs;
use std::path::{Path, PathBuf};

use git2::{
    Delta, Diff, DiffOptions, ErrorCode, Repository, StatusOptions,
};
use thiserror::Error;

use super::types::{CommitInfo, DiffScope, FileChange, FileStatus};

#[derive(Error, Debug)]
pub enum GitError {
    #[error("not a git repository")]
    NotARepo,
    #[error("git error: {0}")]
    Git2(#[from] git2::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct GitBridge {
    repo: Repository,
    repo_root: PathBuf,
}

impl GitBridge {
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let repo = Repository::discover(path).map_err(|e| {
            if e.code() == ErrorCode::NotFound {
                GitError::NotARepo
            } else {
                GitError::Git2(e)
            }
        })?;
        let repo_root = repo
            .workdir()
            .ok_or(GitError::NotARepo)?
            .to_path_buf();
        Ok(Self { repo, repo_root })
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn get_head_sha(&self) -> Result<String, GitError> {
        let head = self.repo.head()?;
        let oid = head.target().ok_or_else(|| {
            git2::Error::from_str("HEAD has no target")
        })?;
        Ok(oid.to_string())
    }

    /// Combined detect scope + get files in one call (fast path)
    pub fn detect_and_get_files(&self) -> Result<(DiffScope, Vec<FileChange>), GitError> {
        // Check for staged changes
        let staged_files = self.get_staged_diff_files()?;
        if !staged_files.is_empty() {
            let mut files = staged_files;
            self.populate_contents(&mut files, &DiffScope::Staged)?;
            return Ok((DiffScope::Staged, files));
        }

        // Check for working tree changes + untracked
        let mut working_files = self.get_working_diff_files()?;
        let untracked = self.get_untracked_files()?;
        working_files.extend(untracked);

        if !working_files.is_empty() {
            self.populate_contents(&mut working_files, &DiffScope::Working)?;
            return Ok((DiffScope::Working, working_files));
        }

        // A clean worktree should report no live changes.
        Ok((DiffScope::Working, Vec::new()))
    }

    /// Get changed files for a specific scope
    pub fn get_changed_files(&self, scope: &DiffScope) -> Result<Vec<FileChange>, GitError> {
        let mut files = match scope {
            DiffScope::Working => {
                let mut files = self.get_working_diff_files()?;
                let untracked = self.get_untracked_files()?;
                files.extend(untracked);
                files
            }
            DiffScope::Staged => self.get_staged_diff_files()?,
            DiffScope::Commit { sha } => self.get_commit_diff_files(sha)?,
            DiffScope::Range { from, to } => self.get_range_diff_files(from, to)?,
        };

        // Filter .sem/ files
        files.retain(|f| !f.file_path.starts_with(".sem/"));

        self.populate_contents(&mut files, scope)?;
        Ok(files)
    }

    fn get_staged_diff_files(&self) -> Result<Vec<FileChange>, GitError> {
        let head_tree = match self.repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit()?;
                Some(commit.tree()?)
            }
            Err(_) => None, // No commits yet
        };

        let diff = self.repo.diff_tree_to_index(
            head_tree.as_ref(),
            Some(&self.repo.index()?),
            None,
        )?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_working_diff_files(&self) -> Result<Vec<FileChange>, GitError> {
        let mut opts = DiffOptions::new();
        opts.include_untracked(false);

        let diff = self.repo.diff_index_to_workdir(None, Some(&mut opts))?;
        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_untracked_files(&self) -> Result<Vec<FileChange>, GitError> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .exclude_submodules(true);

        let statuses = self.repo.statuses(Some(&mut opts))?;
        let mut files = Vec::new();

        for entry in statuses.iter() {
            if entry.status().contains(git2::Status::WT_NEW) {
                if let Some(path) = entry.path() {
                    if !path.starts_with(".sem/") {
                        files.push(FileChange {
                            file_path: path.to_string(),
                            status: FileStatus::Added,
                            old_file_path: None,
                            before_content: None,
                            after_content: None,
                        });
                    }
                }
            }
        }

        Ok(files)
    }

    pub fn get_commit_diff_files(&self, sha: &str) -> Result<Vec<FileChange>, GitError> {
        let obj = self.repo.revparse_single(sha)?;
        let commit = obj.peel_to_commit()?;
        let tree = commit.tree()?;

        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };

        let diff = self.repo.diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&tree),
            None,
        )?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_range_diff_files(&self, from: &str, to: &str) -> Result<Vec<FileChange>, GitError> {
        let from_obj = self.repo.revparse_single(from)?;
        let to_obj = self.repo.revparse_single(to)?;

        let from_tree = from_obj.peel_to_commit()?.tree()?;
        let to_tree = to_obj.peel_to_commit()?.tree()?;

        let diff = self.repo.diff_tree_to_tree(
            Some(&from_tree),
            Some(&to_tree),
            None,
        )?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn diff_to_file_changes(&self, diff: &Diff) -> Vec<FileChange> {
        let mut files = Vec::new();

        for delta in diff.deltas() {
            let (status, file_path, old_file_path) = match delta.status() {
                Delta::Added => {
                    let path = delta
                        .new_file()
                        .path()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .to_string();
                    (FileStatus::Added, path, None)
                }
                Delta::Deleted => {
                    let path = delta
                        .old_file()
                        .path()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .to_string();
                    (FileStatus::Deleted, path, None)
                }
                Delta::Modified => {
                    let path = delta
                        .new_file()
                        .path()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .to_string();
                    (FileStatus::Modified, path, None)
                }
                Delta::Renamed => {
                    let new_path = delta
                        .new_file()
                        .path()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .to_string();
                    let old_path = delta
                        .old_file()
                        .path()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .to_string();
                    (FileStatus::Renamed, new_path, Some(old_path))
                }
                _ => continue,
            };

            if !file_path.starts_with(".sem/") {
                files.push(FileChange {
                    file_path,
                    status,
                    old_file_path,
                    before_content: None,
                    after_content: None,
                });
            }
        }

        files
    }

    fn populate_contents(
        &self,
        files: &mut [FileChange],
        scope: &DiffScope,
    ) -> Result<(), GitError> {
        match scope {
            DiffScope::Working => {
                // Resolve HEAD tree once for all before_content reads
                let head_tree = self.resolve_tree("HEAD").ok();
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content = self.read_working_file(&file.file_path);
                    }
                    if file.status != FileStatus::Added {
                        file.before_content = head_tree
                            .as_ref()
                            .and_then(|t| self.read_blob_from_tree(t, &file.file_path));
                    }
                }
            }
            DiffScope::Staged => {
                let head_tree = self.resolve_tree("HEAD").ok();
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content = self
                            .read_index_file(&file.file_path)
                            .or_else(|| self.read_working_file(&file.file_path));
                    }
                    if file.status != FileStatus::Added {
                        file.before_content = head_tree
                            .as_ref()
                            .and_then(|t| self.read_blob_from_tree(t, &file.file_path));
                    }
                }
            }
            DiffScope::Commit { sha } => {
                // Resolve both trees once instead of per-file
                let after_tree = self.resolve_tree(sha)?;
                let before_tree = self.resolve_tree(&format!("{sha}~1")).ok();
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content =
                            self.read_blob_from_tree(&after_tree, &file.file_path);
                    }
                    if file.status != FileStatus::Added {
                        file.before_content = before_tree
                            .as_ref()
                            .and_then(|t| self.read_blob_from_tree(t, &file.file_path));
                    }
                }
            }
            DiffScope::Range { from, to } => {
                let after_tree = self.resolve_tree(to)?;
                let before_tree = self.resolve_tree(from)?;
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content =
                            self.read_blob_from_tree(&after_tree, &file.file_path);
                    }
                    if file.status != FileStatus::Added {
                        let path = file
                            .old_file_path
                            .as_deref()
                            .unwrap_or(&file.file_path);
                        file.before_content =
                            self.read_blob_from_tree(&before_tree, path);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn resolve_tree(&self, refspec: &str) -> Result<git2::Tree<'_>, GitError> {
        let obj = self.repo.revparse_single(refspec)?;
        let commit = obj.peel_to_commit()?;
        Ok(commit.tree()?)
    }

    fn normalize_line_endings(s: String) -> String {
        if s.contains('\r') {
            s.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            s
        }
    }

    pub fn read_blob_from_tree(&self, tree: &git2::Tree, file_path: &str) -> Option<String> {
        let entry = tree.get_path(Path::new(file_path)).ok()?;
        let blob = self.repo.find_blob(entry.id()).ok()?;
        std::str::from_utf8(blob.content())
            .ok()
            .map(|s| Self::normalize_line_endings(s.to_string()))
    }

    fn read_working_file(&self, file_path: &str) -> Option<String> {
        let full_path = self.repo_root.join(file_path);
        fs::read_to_string(full_path)
            .ok()
            .map(Self::normalize_line_endings)
    }

    fn read_index_file(&self, file_path: &str) -> Option<String> {
        let index = self.repo.index().ok()?;
        let entry = index.get_path(Path::new(file_path), 0)?;
        let blob = self.repo.find_blob(entry.id).ok()?;
        std::str::from_utf8(blob.content())
            .ok()
            .map(|s| Self::normalize_line_endings(s.to_string()))
    }


    /// Walk commits from `to` (or HEAD) backward, optionally stopping at `from`.
    pub fn walk_commits(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<CommitInfo>, GitError> {
        let mut revwalk = self.repo.revwalk()?;

        match to {
            Some(to_ref) => {
                let to_oid = self.repo.revparse_single(to_ref)?.peel_to_commit()?.id();
                revwalk.push(to_oid)?;
            }
            None => {
                revwalk.push_head()?;
            }
        }

        if let Some(from_ref) = from {
            let from_oid = self
                .repo
                .revparse_single(from_ref)?
                .peel_to_commit()?
                .id();
            revwalk.hide(from_oid)?;
        }

        let mut commits = Vec::new();
        for oid_result in revwalk {
            let oid = oid_result?;
            let commit = self.repo.find_commit(oid)?;
            let sha = oid.to_string();
            commits.push(CommitInfo {
                short_sha: sha[..7.min(sha.len())].to_string(),
                sha,
                author: commit.author().name().unwrap_or("unknown").to_string(),
                date: commit.time().seconds().to_string(),
                message: commit.message().unwrap_or("").to_string(),
            });
        }

        Ok(commits)
    }

    /// Get commit log for a range (from exclusive, to inclusive).
    pub fn get_log_range(&self, from: &str, to: &str) -> Result<Vec<CommitInfo>, GitError> {
        self.walk_commits(Some(from), Some(to))
    }

    pub fn get_log(&self, limit: usize) -> Result<Vec<CommitInfo>, GitError> {
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;

        let mut commits = Vec::new();
        for (i, oid_result) in revwalk.enumerate() {
            if i >= limit {
                break;
            }
            let oid = oid_result?;
            let commit = self.repo.find_commit(oid)?;
            let sha = oid.to_string();
            commits.push(CommitInfo {
                short_sha: sha[..7.min(sha.len())].to_string(),
                sha,
                author: commit.author().name().unwrap_or("unknown").to_string(),
                date: commit.time().seconds().to_string(),
                message: commit.message().unwrap_or("").to_string(),
            });
        }

        Ok(commits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Oid, Repository, Signature};
    use tempfile::TempDir;

    fn commit_file(repo: &Repository, file_path: &str, contents: &str, message: &str) -> Oid {
        fs::write(repo.workdir().unwrap().join(file_path), contents).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file_path)).unwrap();
        index.write().unwrap();

        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();

        match repo.head() {
            Ok(head) => {
                let parent = repo.find_commit(head.target().unwrap()).unwrap();
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
                    .unwrap()
            }
            Err(_) => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
                .unwrap(),
        }
    }

    #[test]
    fn clean_worktree_does_not_fall_back_to_head_commit() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(&repo, "sample.ts", "export function a() {\n  return 1;\n}\n", "init");
        commit_file(
            &repo,
            "sample.ts",
            "export function a() {\n  return 2;\n}\n",
            "change a",
        );

        let bridge = GitBridge::open(temp.path()).unwrap();
        let (scope, files) = bridge.detect_and_get_files().unwrap();

        assert!(matches!(scope, DiffScope::Working));
        assert!(files.is_empty());
    }

    #[test]
    fn explicit_commit_scope_still_reads_head_commit_diff() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(&repo, "sample.ts", "export function a() {\n  return 1;\n}\n", "init");
        let head_oid = commit_file(
            &repo,
            "sample.ts",
            "export function a() {\n  return 2;\n}\n",
            "change a",
        );

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge
            .get_changed_files(&DiffScope::Commit {
                sha: head_oid.to_string(),
            })
            .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_path, "sample.ts");
        assert_eq!(files[0].status, FileStatus::Modified);
    }

    #[test]
    fn crlf_only_difference_in_working_file_is_invisible() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(&repo, "sample.rs", "fn a() {}\n", "init");
        fs::write(temp.path().join("sample.rs"), "fn a() {}\r\n").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Working).unwrap();

        assert_eq!(files.len(), 1, "expected git to detect the CRLF change as modified");

        let before = files[0].before_content.as_deref().unwrap();
        let after = files[0].after_content.as_deref().unwrap();

        assert_eq!(before, after, "CRLF-only difference should be invisible after normalization");
    }

    #[test]
    fn crlf_stored_in_blob_is_normalized_on_read() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        repo.config().unwrap().set_str("core.autocrlf", "false").unwrap();
        commit_file(&repo, "sample.rs", "fn a() {}\r\n", "init");
        fs::write(temp.path().join("sample.rs"), "fn a() {}\r\nfn b() {}\r\n").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Working).unwrap();

        assert_eq!(files.len(), 1, "expected git to detect the modification");

        let before = files[0].before_content.as_deref().unwrap();
        assert!(!before.contains('\r'), "before_content read from CRLF blob should be normalized to LF");
    }
}
