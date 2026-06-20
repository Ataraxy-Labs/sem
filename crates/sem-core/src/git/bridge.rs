use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use git2::{Blame, Delta, Diff, DiffFindOptions, DiffOptions, ErrorCode, Oid, Repository};
use thiserror::Error;

use super::types::BlameLineInfo;
use super::types::{CommitInfo, DiffScope, FileChange, FileCommitInfo, FileStatus};

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
    cwd: PathBuf,
}

impl GitBridge {
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let cwd = normalize_open_path(path)?;
        let repo = match Repository::discover(path) {
            Ok(repo) => repo,
            Err(error) if should_retry_with_command_line_safe_directory(&error, path) => {
                let _guard = owner_validation_lock()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let _owner_validation = OwnerValidationDisabled::new()?;
                let repo = Repository::discover(path);
                repo.map_err(map_git_error)?
            }
            Err(error) => return Err(map_git_error(error)),
        };
        let repo_root = repo.workdir().ok_or(GitError::NotARepo)?;
        let repo_root = fs::canonicalize(repo_root)?;
        Ok(Self {
            repo,
            repo_root,
            cwd,
        })
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Return the URL of the "origin" remote, if one exists.
    pub fn get_remote_url(&self) -> Option<String> {
        self.repo
            .find_remote("origin")
            .ok()
            .and_then(|r| r.url().map(String::from))
    }

    /// Resolve a refspec to its full commit SHA, if valid.
    pub fn resolve_ref_sha(&self, refspec: &str) -> Option<String> {
        self.repo
            .revparse_single(refspec)
            .ok()
            .and_then(|obj| obj.peel_to_commit().ok())
            .map(|c| c.id().to_string())
    }

    pub fn blame_file(&self, file_path: &Path) -> Result<Blame<'_>, GitError> {
        Ok(self.repo.blame_file(file_path, None)?)
    }

    pub fn blame_file_porcelain(&self, file_path: &Path) -> Result<Vec<BlameLineInfo>, GitError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("blame")
            .arg("--line-porcelain")
            .arg("--")
            .arg(file_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(git_command_error(if stderr.is_empty() {
                format!("git blame exited with {}", output.status)
            } else {
                stderr
            }));
        }

        let parsed = parse_blame_porcelain(&String::from_utf8_lossy(&output.stdout));
        if parsed.is_empty() && !output.stdout.is_empty() {
            return Err(git_command_error(
                "failed to parse git blame porcelain output".to_string(),
            ));
        }

        Ok(parsed)
    }

    pub fn commit_summary(&self, oid: Oid) -> Option<String> {
        self.repo
            .find_commit(oid)
            .ok()
            .and_then(|commit| commit.summary().map(String::from))
    }

    pub fn get_head_sha(&self) -> Result<String, GitError> {
        let head = self.repo.head()?;
        let oid = head
            .target()
            .ok_or_else(|| git2::Error::from_str("HEAD has no target"))?;
        Ok(oid.to_string())
    }

    /// Combined detect scope + get files in one call (fast path).
    /// Shows all changes from HEAD to the current working state by default.
    /// Use `--staged` for staged changes only.
    pub fn detect_and_get_files(
        &self,
        pathspecs: &[String],
    ) -> Result<(DiffScope, Vec<FileChange>), GitError> {
        // Show the full current working state, including staged changes.
        let mut working_files = self.get_working_diff_files(pathspecs)?;
        if !working_files.is_empty() {
            self.populate_contents(&mut working_files, &DiffScope::Working)?;
            return Ok((DiffScope::Working, working_files));
        }

        // Clean worktree = no changes
        Ok((DiffScope::Working, Vec::new()))
    }

    /// Get changed files for a specific scope
    pub fn get_changed_files(
        &self,
        scope: &DiffScope,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let mut files = match scope {
            DiffScope::Working => self.get_working_diff_files(pathspecs)?,
            DiffScope::Staged => self.get_staged_diff_files(pathspecs)?,
            DiffScope::Commit { sha } => self.get_commit_diff_files(sha, pathspecs)?,
            DiffScope::Range { from, to } => self.get_range_diff_files(from, to, pathspecs)?,
            DiffScope::RefToWorking { refspec } => {
                self.get_ref_to_working_diff_files(refspec, pathspecs)?
            }
        };

        // Filter .sem/ files
        files.retain(|f| !f.file_path.starts_with(".sem/"));

        self.populate_contents(&mut files, scope)?;
        Ok(files)
    }

    /// True when this repo uses a sparse checkout. libgit2 cannot read a
    /// sparse index (`unsupported mandatory extension: 'sdir'`), and even when
    /// the index is readable, its workdir diff reports sparse-excluded files as
    /// deleted. In both cases we route working/staged diffs through the git CLI,
    /// which understands sparse checkouts correctly.
    fn is_sparse_checkout(&self) -> bool {
        self.repo
            .config()
            .and_then(|cfg| cfg.get_bool("core.sparseCheckout"))
            .unwrap_or(false)
    }

    /// Get working-tree or staged changed files via the git CLI. Used for
    /// sparse checkouts where libgit2's index/workdir diff is unusable.
    /// Rename detection (-M) is on; contents are populated by the caller.
    ///
    /// `staged` selects `--cached` (HEAD vs index). Otherwise we diff against
    /// HEAD (not the bare worktree-vs-index `git diff`) to match sem's Working
    /// scope, which shows the full current state including staged changes.
    fn changed_files_via_cli(
        &self,
        staged: bool,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let has_head = self.repo.head().is_ok();
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.repo_root)
            .arg("diff")
            .arg("--name-status")
            .arg("-M")
            .arg("-z");
        if staged {
            command.arg("--cached");
        } else if has_head {
            // Full working state since HEAD (includes staged), matching the
            // libgit2 diff_tree_to_workdir_with_index path.
            command.arg("HEAD");
        }
        if !pathspecs.is_empty() {
            command.arg("--");
            for spec in self.normalize_pathspecs(pathspecs)? {
                command.arg(spec);
            }
        }

        let output = command.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(git_command_error(if stderr.is_empty() {
                format!("git diff exited with {}", output.status)
            } else {
                stderr
            }));
        }

        Ok(parse_name_status_z(&output.stdout))
    }

    pub fn get_staged_files_with_base_ref(
        &self,
        base: &str,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let mut files = self.get_staged_diff_files_with_base(base, pathspecs)?;
        files.retain(|f| !f.file_path.starts_with(".sem/"));

        let base_tree = self.resolve_tree(base)?;
        for file in files.iter_mut() {
            if file.status != FileStatus::Deleted {
                file.after_content = self.read_index_file(&file.file_path);
            }
            if file.status != FileStatus::Added {
                let path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                file.before_content = self.read_blob_from_tree(&base_tree, path);
            }
        }

        Ok(files)
    }

    /// Resolve the merge base between two refs
    pub fn resolve_merge_base(&self, ref1: &str, ref2: &str) -> Result<String, GitError> {
        let obj1 = self.repo.revparse_single(ref1)?;
        let obj2 = self.repo.revparse_single(ref2)?;
        let oid = self.repo.merge_base(obj1.id(), obj2.id())?;
        Ok(oid.to_string())
    }

    /// Check if a string resolves to a valid git revision
    pub fn is_valid_rev(&self, refspec: &str) -> bool {
        self.repo.revparse_single(refspec).is_ok()
    }

    fn make_diff_opts(&self, pathspecs: &[String]) -> Result<DiffOptions, GitError> {
        let mut opts = DiffOptions::new();
        for spec in self.normalize_pathspecs(pathspecs)? {
            opts.pathspec(spec.as_str());
        }
        Ok(opts)
    }

    fn normalize_pathspecs(&self, pathspecs: &[String]) -> Result<Vec<String>, GitError> {
        pathspecs
            .iter()
            .map(|spec| self.normalize_pathspec(spec))
            .collect()
    }

    fn normalize_pathspec(&self, spec: &str) -> Result<String, GitError> {
        if spec.is_empty() || spec.starts_with(':') {
            return Ok(spec.to_string());
        }

        let spec_path = Path::new(spec);
        let absolute = if spec_path.is_absolute() {
            normalize_absolute_pathspec(spec_path)
        } else {
            normalize_lexical(&self.cwd.join(spec_path))
        };

        let repo_root = normalize_lexical(&self.repo_root);
        let relative = absolute
            .strip_prefix(&repo_root)
            .map_err(|_| pathspec_outside_repo_error(spec, &self.repo_root))?;

        if relative.as_os_str().is_empty() {
            Ok(".".to_string())
        } else {
            relative
                .to_str()
                .map(|path| path.replace('\\', "/"))
                .ok_or_else(|| non_utf8_pathspec_error(spec))
        }
    }

    fn get_staged_diff_files(&self, pathspecs: &[String]) -> Result<Vec<FileChange>, GitError> {
        if self.is_sparse_checkout() {
            return self.changed_files_via_cli(true, pathspecs);
        }

        let head_tree = match self.repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit()?;
                Some(commit.tree()?)
            }
            Err(_) => None, // No commits yet
        };

        self.get_index_diff_files(head_tree.as_ref(), pathspecs)
    }

    fn get_staged_diff_files_with_base(
        &self,
        base: &str,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let base_tree = self.resolve_tree(base)?;
        self.get_index_diff_files(Some(&base_tree), pathspecs)
    }

    fn get_index_diff_files(
        &self,
        base_tree: Option<&git2::Tree<'_>>,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff =
            self.repo
                .diff_tree_to_index(base_tree, Some(&self.repo.index()?), Some(&mut opts))?;
        Self::detect_renames(&mut diff)?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_working_diff_files(&self, pathspecs: &[String]) -> Result<Vec<FileChange>, GitError> {
        if self.is_sparse_checkout() {
            // Sparse index is unreadable by libgit2, and its workdir diff would
            // mark sparse-excluded files as deleted. Ask git directly.
            return self.changed_files_via_cli(false, pathspecs);
        }

        let mut opts = self.make_diff_opts(pathspecs)?;
        opts.include_untracked(false);

        let head_tree = self.resolve_tree("HEAD").ok();
        let mut diff = match head_tree.as_ref() {
            Some(head_tree) => self
                .repo
                .diff_tree_to_workdir_with_index(Some(head_tree), Some(&mut opts))?,
            None => self.repo.diff_index_to_workdir(None, Some(&mut opts))?,
        };
        Self::detect_renames(&mut diff)?;
        self.apply_index_rename_map(
            self.diff_to_file_changes(&diff),
            head_tree.as_ref(),
            pathspecs,
        )
    }

    fn apply_index_rename_map(
        &self,
        mut files: Vec<FileChange>,
        base_tree: Option<&git2::Tree<'_>>,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let Some(base_tree) = base_tree else {
            return Ok(files);
        };

        let index_renames: Vec<FileChange> = self
            .get_index_diff_files(Some(base_tree), pathspecs)?
            .into_iter()
            .filter(|file| file.status == FileStatus::Renamed)
            .collect();

        for rename in index_renames {
            let Some(old_path) = rename.old_file_path.clone() else {
                continue;
            };
            let target_pos = files.iter().position(|file| {
                matches!(file.status, FileStatus::Added | FileStatus::Renamed)
                    && file.file_path == rename.file_path
            });
            let deleted_pos = files
                .iter()
                .position(|file| file.status == FileStatus::Deleted && file.file_path == old_path);

            if let (Some(target_pos), Some(deleted_pos)) = (target_pos, deleted_pos) {
                if files[target_pos].status == FileStatus::Renamed
                    && files[target_pos].old_file_path.as_deref() == Some(old_path.as_str())
                {
                    continue;
                }

                let target_file = files[target_pos].clone();
                let deleted_file = files[deleted_pos].clone();
                let displaced_deleted_path = if target_file.status == FileStatus::Renamed {
                    target_file
                        .old_file_path
                        .as_ref()
                        .filter(|path| *path != &old_path)
                        .cloned()
                } else {
                    None
                };

                files = files
                    .into_iter()
                    .enumerate()
                    .filter_map(|(idx, file)| {
                        if idx == target_pos || idx == deleted_pos {
                            None
                        } else {
                            Some(file)
                        }
                    })
                    .collect();
                let before_content = deleted_file
                    .before_content
                    .or_else(|| self.read_blob_from_tree(base_tree, &old_path));
                let after_content = target_file
                    .after_content
                    .or_else(|| self.read_working_file(&target_file.file_path));
                files.push(FileChange {
                    file_path: target_file.file_path,
                    status: FileStatus::Renamed,
                    old_file_path: Some(old_path),
                    before_content,
                    after_content,
                });
                if let Some(file_path) = displaced_deleted_path {
                    let before_content = self.read_blob_from_tree(base_tree, &file_path);
                    files.push(FileChange {
                        file_path,
                        status: FileStatus::Deleted,
                        old_file_path: None,
                        before_content,
                        after_content: None,
                    });
                }
            }
        }

        Ok(files)
    }

    fn get_commit_diff_files(
        &self,
        sha: &str,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let obj = self.repo.revparse_single(sha)?;
        let commit = obj.peel_to_commit()?;
        let tree = commit.tree()?;

        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };

        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff =
            self.repo
                .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
        Self::detect_renames(&mut diff)?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_range_diff_files(
        &self,
        from: &str,
        to: &str,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let from_obj = self.repo.revparse_single(from)?;
        let to_obj = self.repo.revparse_single(to)?;

        let from_tree = from_obj.peel_to_commit()?.tree()?;
        let to_tree = to_obj.peel_to_commit()?.tree()?;

        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff =
            self.repo
                .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut opts))?;
        Self::detect_renames(&mut diff)?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_ref_to_working_diff_files(
        &self,
        refspec: &str,
        pathspecs: &[String],
    ) -> Result<Vec<FileChange>, GitError> {
        let tree = self.resolve_tree(refspec)?;
        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff = self
            .repo
            .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts))?;
        Self::detect_renames(&mut diff)?;
        self.apply_index_rename_map(self.diff_to_file_changes(&diff), Some(&tree), pathspecs)
    }

    fn detect_renames(diff: &mut Diff) -> Result<(), GitError> {
        let mut opts = DiffFindOptions::new();
        opts.renames(true);
        diff.find_similar(Some(&mut opts))?;
        Ok(())
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

    fn bytes_look_binary(bytes: &[u8], complete: bool) -> bool {
        if bytes.iter().any(|byte| *byte == 0) {
            return true;
        }

        match std::str::from_utf8(bytes) {
            Ok(_) => false,
            Err(error) => complete || error.error_len().is_some(),
        }
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
                        let path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                        file.before_content = head_tree
                            .as_ref()
                            .and_then(|t| self.read_blob_from_tree(t, path));
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
                        let path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                        file.before_content = head_tree
                            .as_ref()
                            .and_then(|t| self.read_blob_from_tree(t, path));
                    }
                }
            }
            DiffScope::Commit { sha } => {
                // Resolve both trees once instead of per-file
                let after_tree = self.resolve_tree(sha)?;
                let before_tree = self.resolve_tree(&format!("{sha}~1")).ok();
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content = self.read_blob_from_tree(&after_tree, &file.file_path);
                    }
                    if file.status != FileStatus::Added {
                        let path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                        file.before_content = before_tree
                            .as_ref()
                            .and_then(|t| self.read_blob_from_tree(t, path));
                    }
                }
            }
            DiffScope::Range { from, to } => {
                let after_tree = self.resolve_tree(to)?;
                let before_tree = self.resolve_tree(from)?;
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content = self.read_blob_from_tree(&after_tree, &file.file_path);
                    }
                    if file.status != FileStatus::Added {
                        let path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                        file.before_content = self.read_blob_from_tree(&before_tree, path);
                    }
                }
            }
            DiffScope::RefToWorking { refspec } => {
                let before_tree = self.resolve_tree(refspec)?;
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content = self.read_working_file(&file.file_path);
                    }
                    if file.status != FileStatus::Added {
                        let path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                        file.before_content = self.read_blob_from_tree(&before_tree, path);
                    }
                }
            }
        }
        Ok(())
    }

    fn resolve_tree(&self, refspec: &str) -> Result<git2::Tree<'_>, GitError> {
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

    fn read_blob_from_tree(&self, tree: &git2::Tree, file_path: &str) -> Option<String> {
        let entry = tree.get_path(Path::new(file_path)).ok()?;
        let blob = self.repo.find_blob(entry.id()).ok()?;
        let bytes = blob.content();
        if blob.is_binary() || Self::bytes_look_binary(bytes, true) {
            return None;
        }
        std::str::from_utf8(bytes)
            .ok()
            .map(|s| Self::normalize_line_endings(s.to_string()))
    }

    fn read_working_file(&self, file_path: &str) -> Option<String> {
        let full_path = self.repo_root.join(file_path);
        let bytes = fs::read(full_path).ok()?;
        if Self::bytes_look_binary(&bytes, true) {
            return None;
        }
        String::from_utf8(bytes)
            .ok()
            .map(Self::normalize_line_endings)
    }

    fn read_index_file(&self, file_path: &str) -> Option<String> {
        // libgit2 cannot open a sparse index; fall back to the git CLI.
        let Ok(index) = self.repo.index() else {
            return self.read_index_file_cli(file_path);
        };
        let entry = index.get_path(Path::new(file_path), 0)?;
        let blob = self.repo.find_blob(entry.id).ok()?;
        let bytes = blob.content();
        if blob.is_binary() || Self::bytes_look_binary(bytes, true) {
            return None;
        }
        std::str::from_utf8(bytes)
            .ok()
            .map(|s| Self::normalize_line_endings(s.to_string()))
    }

    /// Read a file's staged (index) content via `git show :path`. Used when
    /// libgit2 cannot open the index (sparse checkouts).
    fn read_index_file_cli(&self, file_path: &str) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("show")
            .arg(format!(":{file_path}"))
            .output()
            .ok()?;
        if !output.status.success() || Self::bytes_look_binary(&output.stdout, true) {
            return None;
        }
        String::from_utf8(output.stdout)
            .ok()
            .map(Self::normalize_line_endings)
    }

    /// Read file content at a specific git ref (commit SHA, branch, tag, etc.)
    pub fn read_file_at_ref(
        &self,
        refspec: &str,
        file_path: &str,
    ) -> Result<Option<String>, GitError> {
        let tree = self.resolve_tree(refspec)?;
        Ok(self.read_blob_from_tree(&tree, file_path))
    }

    /// Get commits that modified a specific file, walking history from HEAD.
    /// Returns commits in reverse chronological order (newest first).
    pub fn get_file_commits(
        &self,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, GitError> {
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;

        let mut commits = Vec::new();
        let path = Path::new(file_path);

        for oid_result in revwalk {
            let oid = oid_result?;
            let commit = self.repo.find_commit(oid)?;
            let tree = commit.tree()?;

            // Check if this file exists in this commit's tree
            let file_in_commit = tree.get_path(path).ok().map(|e| e.id());

            // Compare with parent to see if the file changed
            let file_in_parent = if commit.parent_count() > 0 {
                commit
                    .parent(0)
                    .ok()
                    .and_then(|p| p.tree().ok())
                    .and_then(|t| t.get_path(path).ok().map(|e| e.id()))
            } else {
                None // No parent = initial commit, file was added
            };

            // Include if file changed between parent and this commit
            let changed = match (file_in_commit, file_in_parent) {
                (Some(cur), Some(prev)) => cur != prev, // content changed
                (Some(_), None) => true,                // file added
                (None, Some(_)) => true,                // file deleted
                (None, None) => false,                  // file not present in either
            };

            if changed {
                let sha = oid.to_string();
                commits.push(CommitInfo {
                    short_sha: sha[..7.min(sha.len())].to_string(),
                    sha,
                    author: commit.author().name().unwrap_or("unknown").to_string(),
                    date: commit.time().seconds().to_string(),
                    message: commit.message().unwrap_or("").to_string(),
                });

                if limit != 0 && commits.len() >= limit {
                    break;
                }
            }
        }

        Ok(commits)
    }

    /// Get commits that modified a specific file, following renames across history.
    /// Like `git log --follow`: when the tracked path disappears between commits,
    /// compute a diff with rename detection to find the old filename and continue.
    /// Returns commits in reverse chronological order (newest first).
    pub fn get_file_commits_follow_renames(
        &self,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<FileCommitInfo>, GitError> {
        match self.get_file_commits_follow_renames_cli(file_path, limit) {
            Ok(commits) if !commits.is_empty() => return Ok(commits),
            Ok(_) => {}
            Err(GitError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;

        let mut results = Vec::new();
        let mut tracked_path = file_path.to_string();

        for oid_result in revwalk {
            let oid = oid_result?;
            let commit = self.repo.find_commit(oid)?;
            let tree = commit.tree()?;

            let path = Path::new(&tracked_path);
            let file_in_commit = tree.get_path(path).ok().map(|e| e.id());

            let (parent_tree_opt, file_in_parent) = if commit.parent_count() > 0 {
                let parent = commit.parent(0)?;
                let ptree = parent.tree()?;
                let fip = ptree.get_path(path).ok().map(|e| e.id());
                (Some(ptree), fip)
            } else {
                (None, None)
            };

            let changed = match (file_in_commit, file_in_parent) {
                (Some(cur), Some(prev)) => cur != prev,
                (Some(_), None) => true,
                (None, Some(_)) => true,
                (None, None) => false,
            };

            if changed {
                let sha_str = oid.to_string();
                results.push(FileCommitInfo {
                    commit: CommitInfo {
                        short_sha: sha_str[..7.min(sha_str.len())].to_string(),
                        sha: sha_str,
                        author: commit.author().name().unwrap_or("unknown").to_string(),
                        date: commit.time().seconds().to_string(),
                        message: commit.message().unwrap_or("").to_string(),
                    },
                    file_path: tracked_path.clone(),
                });

                if limit != 0 && results.len() >= limit {
                    break;
                }
            }

            // When walking backward, the rename commit still contains the new
            // path. Detect that parent-side old path before the next iteration.
            let should_check_rename =
                parent_tree_opt.is_some() && (file_in_parent.is_none() || file_in_commit.is_none());
            if should_check_rename {
                let mut diff =
                    self.repo
                        .diff_tree_to_tree(parent_tree_opt.as_ref(), Some(&tree), None)?;
                let mut find_opts = DiffFindOptions::new();
                find_opts.renames(true);
                diff.find_similar(Some(&mut find_opts))?;

                let mut found_rename = false;
                for delta in diff.deltas() {
                    if delta.status() == Delta::Renamed {
                        let new_path = delta
                            .new_file()
                            .path()
                            .and_then(|p| p.to_str())
                            .unwrap_or("");
                        if new_path == tracked_path {
                            // The tracked file was renamed FROM old_path
                            let old_path = delta
                                .old_file()
                                .path()
                                .and_then(|p| p.to_str())
                                .unwrap_or("")
                                .to_string();
                            if !old_path.is_empty() {
                                tracked_path = old_path;
                                found_rename = true;
                                break;
                            }
                        }
                    }
                }

                if !found_rename && file_in_commit.is_none() {
                    // File truly deleted, stop tracking
                    break;
                }
            }
        }

        Ok(results)
    }

    fn get_file_commits_follow_renames_cli(
        &self,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<FileCommitInfo>, GitError> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.repo_root)
            .arg("log")
            .arg("--follow")
            .arg("--format=\x1e%H\x1f%an\x1f%at\x1f%s")
            .arg("--name-status");
        if limit != 0 {
            command.arg("-n").arg(limit.to_string());
        }
        command.arg("--").arg(file_path);

        let output = command.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(git_command_error(if stderr.is_empty() {
                format!("git log exited with {}", output.status)
            } else {
                stderr
            }));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut tracked_path = file_path.to_string();
        let mut commits = Vec::new();

        for record in stdout.split('\x1e') {
            let record = record.trim_start_matches('\n');
            if record.trim().is_empty() {
                continue;
            }

            let mut lines = record.lines();
            let Some(meta) = lines.next() else {
                continue;
            };
            let mut parts = meta.splitn(4, '\x1f');
            let Some(sha) = parts.next() else {
                continue;
            };
            let Some(author) = parts.next() else {
                continue;
            };
            let Some(date) = parts.next() else {
                continue;
            };
            let message = parts.next().unwrap_or_default();

            let commit_path = tracked_path.clone();
            let mut previous_path = None;
            for line in lines {
                let fields: Vec<&str> = line.split('\t').collect();
                if fields.len() >= 3 && fields[0].starts_with('R') && fields[2] == tracked_path {
                    previous_path = Some(fields[1].to_string());
                }
            }

            commits.push(FileCommitInfo {
                commit: CommitInfo {
                    short_sha: sha[..7.min(sha.len())].to_string(),
                    sha: sha.to_string(),
                    author: author.to_string(),
                    date: date.to_string(),
                    message: message.to_string(),
                },
                file_path: commit_path,
            });

            if let Some(previous_path) = previous_path {
                tracked_path = previous_path;
            }
        }

        Ok(commits)
    }

    /// Get all file paths changed in a single commit (vs its parent).
    /// Returns file paths from the new side of each delta.
    pub fn get_commit_changed_files(&self, sha: &str) -> Result<Vec<String>, GitError> {
        let obj = self.repo.revparse_single(sha)?;
        let commit = obj.peel_to_commit()?;
        let tree = commit.tree()?;
        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };
        let diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
        let mut paths = Vec::new();
        for delta in diff.deltas() {
            if let Some(p) = delta.new_file().path().and_then(|p| p.to_str()) {
                paths.push(p.to_string());
            }
            // Also include old path for deletions/renames
            if let Some(p) = delta.old_file().path().and_then(|p| p.to_str()) {
                if !paths.contains(&p.to_string()) {
                    paths.push(p.to_string());
                }
            }
        }
        Ok(paths)
    }

    pub fn get_log(&self, limit: usize) -> Result<Vec<CommitInfo>, GitError> {
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;

        let mut commits = Vec::new();
        for (i, oid_result) in revwalk.enumerate() {
            if limit != 0 && i >= limit {
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

/// Parse `git diff --name-status -M -z` output into FileChange entries.
/// Records are NUL-delimited; a rename/copy is a status token (R100/C75)
/// followed by old path then new path, others are status then one path.
fn parse_name_status_z(stdout: &[u8]) -> Vec<FileChange> {
    let text = String::from_utf8_lossy(stdout);
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    let mut files = Vec::new();

    while let Some(status) = fields.next() {
        let code = status.chars().next().unwrap_or(' ');
        let (file_change, _) = match code {
            'R' | 'C' => {
                let Some(old_path) = fields.next() else { break };
                let Some(new_path) = fields.next() else { break };
                (
                    FileChange {
                        file_path: new_path.to_string(),
                        status: FileStatus::Renamed,
                        old_file_path: Some(old_path.to_string()),
                        before_content: None,
                        after_content: None,
                    },
                    (),
                )
            }
            'A' | 'D' | 'M' | 'T' => {
                let Some(path) = fields.next() else { break };
                let status = match code {
                    'A' => FileStatus::Added,
                    'D' => FileStatus::Deleted,
                    _ => FileStatus::Modified,
                };
                (
                    FileChange {
                        file_path: path.to_string(),
                        status,
                        old_file_path: None,
                        before_content: None,
                        after_content: None,
                    },
                    (),
                )
            }
            _ => continue,
        };
        if !file_change.file_path.starts_with(".sem/") {
            files.push(file_change);
        }
    }

    files
}

fn parse_blame_porcelain(output: &str) -> Vec<BlameLineInfo> {
    let lines: Vec<&str> = output.lines().collect();
    let mut parsed = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let Some((raw_sha, line_number)) = parse_blame_header(lines[index]) else {
            index += 1;
            continue;
        };
        index += 1;

        let mut author = String::new();
        let mut author_time = None;
        let mut summary = String::new();

        while index < lines.len() {
            let line = lines[index];
            index += 1;

            if line.starts_with('\t') {
                break;
            } else if let Some(value) = line.strip_prefix("author ") {
                author = value.to_string();
            } else if let Some(value) = line.strip_prefix("author-time ") {
                author_time = value.parse::<i64>().ok();
            } else if let Some(value) = line.strip_prefix("summary ") {
                summary = value.to_string();
            }
        }

        let sha = raw_sha.trim_start_matches('^');
        let commit_sha = if sha.chars().all(|c| c == '0') {
            None
        } else {
            Some(sha.to_string())
        };

        if author.is_empty() {
            author = if commit_sha.is_none() {
                "Not Committed Yet".to_string()
            } else {
                "unknown".to_string()
            };
        }

        parsed.push(BlameLineInfo {
            line_number,
            commit_sha,
            author,
            author_time,
            summary,
        });
    }

    parsed.sort_by_key(|line| line.line_number);
    parsed
}

fn parse_blame_header(line: &str) -> Option<(&str, usize)> {
    let mut parts = line.split_whitespace();
    let sha = parts.next()?;
    if !is_blame_oid(sha) {
        return None;
    }
    parts.next()?;
    let final_line = parts.next()?.parse().ok()?;
    Some((sha, final_line))
}

fn is_blame_oid(value: &str) -> bool {
    let value = value.strip_prefix('^').unwrap_or(value);
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn git_command_error(message: String) -> GitError {
    GitError::Git2(git2::Error::from_str(&message))
}

fn map_git_error(error: git2::Error) -> GitError {
    if error.code() == ErrorCode::NotFound {
        GitError::NotARepo
    } else {
        GitError::Git2(error)
    }
}

fn should_retry_with_command_line_safe_directory(error: &git2::Error, path: &Path) -> bool {
    let safe_directories = command_line_safe_directories();
    should_retry_with_safe_directory(error, path, &safe_directories)
}

fn should_retry_with_safe_directory(
    error: &git2::Error,
    path: &Path,
    safe_directories: &[String],
) -> bool {
    error.code() == ErrorCode::Owner
        && nearest_git_root(path).is_some_and(|repo_root| {
            safe_directories.iter().any(|safe_directory| {
                safe_directory == "*" || paths_match(&repo_root, Path::new(safe_directory))
            })
        })
}

fn command_line_safe_directories() -> Vec<String> {
    let count = env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();

    (0..count)
        .filter_map(|index| {
            let key = env::var(format!("GIT_CONFIG_KEY_{index}")).ok()?;
            if key.eq_ignore_ascii_case("safe.directory") {
                env::var(format!("GIT_CONFIG_VALUE_{index}")).ok()
            } else {
                None
            }
        })
        .collect()
}

fn nearest_git_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_file() { path.parent()? } else { path };

    loop {
        if current.join(".git").exists() {
            return Some(fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf()));
        }

        current = current.parent()?;
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());

    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn owner_validation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct OwnerValidationDisabled;

impl OwnerValidationDisabled {
    fn new() -> Result<Self, GitError> {
        // libgit2 stores this as a process-global option; callers hold owner_validation_lock.
        unsafe { git2::opts::set_verify_owner_validation(false)? };
        Ok(Self)
    }
}

impl Drop for OwnerValidationDisabled {
    fn drop(&mut self) {
        // Restore the default before the owner-validation lock is released.
        unsafe {
            let _ = git2::opts::set_verify_owner_validation(true);
        }
    }
}

fn normalize_open_path(path: &Path) -> Result<PathBuf, GitError> {
    let canonical = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) if path.is_absolute() => normalize_lexical(path),
        Err(_) => normalize_lexical(&env::current_dir()?.join(path)),
    };

    Ok(if canonical.is_file() {
        canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(canonical)
    } else {
        canonical
    })
}

fn normalize_absolute_pathspec(path: &Path) -> PathBuf {
    let path = normalize_lexical(path);
    let Some(leaf) = path.file_name() else {
        return fs::canonicalize(&path).unwrap_or(path);
    };
    let mut trailing_components = vec![leaf.to_os_string()];

    let Some(parent) = path.parent() else {
        return path;
    };

    for ancestor in parent.ancestors() {
        if ancestor.exists() {
            let mut normalized =
                fs::canonicalize(ancestor).unwrap_or_else(|_| normalize_lexical(ancestor));
            for component in trailing_components.iter().rev() {
                normalized.push(component);
            }
            return normalized;
        }

        let Some(name) = ancestor.file_name() else {
            return path;
        };
        trailing_components.push(name.to_os_string());
    }

    path
}

fn pathspec_outside_repo_error(pathspec: &str, repo_root: &Path) -> GitError {
    GitError::Git2(git2::Error::from_str(&format!(
        "pathspec '{pathspec}' is outside repository '{}'",
        repo_root.display()
    )))
}

fn non_utf8_pathspec_error(pathspec: &str) -> GitError {
    GitError::Git2(git2::Error::from_str(&format!(
        "pathspec '{pathspec}' is not valid UTF-8 after normalization"
    )))
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::change::ChangeType;
    use crate::parser::differ::{collect_binary_file_changes, compute_semantic_diff};
    use crate::parser::plugins::create_default_registry;
    use git2::{ErrorClass, Oid, Repository, Signature};
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

    fn commit_binary_file(
        repo: &Repository,
        file_path: &str,
        contents: &[u8],
        message: &str,
    ) -> Oid {
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
    fn porcelain_blame_reports_uncommitted_lines() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(&repo, "a.py", "def foo():\n    return 1\n", "init");
        fs::write(temp.path().join("a.py"), "def foo():\n    return 2\n").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let blame = bridge.blame_file_porcelain(Path::new("a.py")).unwrap();

        assert!(blame[0].commit_sha.is_some());
        assert_eq!(blame[1].commit_sha, None);
        assert_eq!(blame[1].author, "Not Committed Yet");
    }

    #[test]
    fn sparse_checkout_does_not_report_excluded_files_as_deleted() {
        // Regression for #330: with a cone-mode sparse checkout, libgit2's
        // workdir diff sees sparse-excluded files as absent and reports them
        // deleted (and a true sparse index errors outright). We route through
        // the git CLI, which understands sparse checkouts.
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        fs::create_dir_all(temp.path().join("keep")).unwrap();
        fs::create_dir_all(temp.path().join("drop")).unwrap();
        commit_file(
            &repo,
            "keep/a.rs",
            "fn kept() { let x = 1; }\n",
            "init keep",
        );
        commit_file(&repo, "drop/b.rs", "fn dropped() {}\n", "init drop");

        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .args(args)
                .output()
                .expect("git")
        };
        // Enable cone-mode sparse checkout restricted to keep/.
        if !git(&["sparse-checkout", "init", "--cone"]).status.success() {
            return; // git too old for sparse-checkout; skip
        }
        git(&["sparse-checkout", "set", "keep"]);
        // Modify a file inside the cone.
        fs::write(temp.path().join("keep/a.rs"), "fn kept() { let x = 2; }\n").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Working, &[]).unwrap();

        // Only the in-cone modification; the sparse-excluded drop/b.rs must
        // NOT appear as deleted.
        assert_eq!(files.len(), 1, "got: {files:?}");
        assert_eq!(files[0].file_path, "keep/a.rs");
        assert_eq!(files[0].status, FileStatus::Modified);
        assert!(!files.iter().any(|f| f.file_path == "drop/b.rs"));
    }

    #[test]
    fn clean_worktree_does_not_fall_back_to_head_commit() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(
            &repo,
            "sample.ts",
            "export function a() {\n  return 1;\n}\n",
            "init",
        );
        commit_file(
            &repo,
            "sample.ts",
            "export function a() {\n  return 2;\n}\n",
            "change a",
        );

        let bridge = GitBridge::open(temp.path()).unwrap();
        let (scope, files) = bridge.detect_and_get_files(&[]).unwrap();

        assert!(matches!(scope, DiffScope::Working));
        assert!(files.is_empty());
    }

    #[test]
    fn owner_error_retries_for_command_line_safe_directory() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();

        let owner_error = git2::Error::new(ErrorCode::Owner, ErrorClass::Config, "owner mismatch");
        let safe_directories = [temp.path().to_string_lossy().to_string()];

        assert!(should_retry_with_safe_directory(
            &owner_error,
            temp.path(),
            &safe_directories,
        ));

        let other_directories = [temp.path().join("other").to_string_lossy().to_string()];
        assert!(!should_retry_with_safe_directory(
            &owner_error,
            temp.path(),
            &other_directories,
        ));

        let not_found_error =
            git2::Error::new(ErrorCode::NotFound, ErrorClass::Repository, "not found");
        assert!(!should_retry_with_safe_directory(
            &not_found_error,
            temp.path(),
            &["*".to_string()],
        ));
    }

    #[test]
    fn explicit_commit_scope_still_reads_head_commit_diff() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(
            &repo,
            "sample.ts",
            "export function a() {\n  return 1;\n}\n",
            "init",
        );
        let head_oid = commit_file(
            &repo,
            "sample.ts",
            "export function a() {\n  return 2;\n}\n",
            "change a",
        );

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge
            .get_changed_files(
                &DiffScope::Commit {
                    sha: head_oid.to_string(),
                },
                &[],
            )
            .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_path, "sample.ts");
        assert_eq!(files[0].status, FileStatus::Modified);
    }

    #[test]
    fn pathspecs_are_normalized_from_open_directory() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        fs::create_dir_all(temp.path().join("pkg")).unwrap();

        commit_file(&repo, "pkg/a.py", "def foo():\n    return 1\n", "init");
        fs::write(temp.path().join("pkg/a.py"), "def foo():\n    return 2\n").unwrap();

        let bridge = GitBridge::open(&temp.path().join("pkg")).unwrap();
        let relative_files = bridge
            .get_changed_files(&DiffScope::Working, &["a.py".to_string()])
            .unwrap();

        assert_eq!(relative_files.len(), 1);
        assert_eq!(relative_files[0].file_path, "pkg/a.py");

        let absolute_path = temp.path().join("pkg/a.py").to_string_lossy().to_string();
        let absolute_files = bridge
            .get_changed_files(&DiffScope::Working, &[absolute_path])
            .unwrap();

        assert_eq!(absolute_files.len(), 1);
        assert_eq!(absolute_files[0].file_path, "pkg/a.py");
    }

    #[test]
    fn absolute_deleted_pathspecs_are_normalized_from_existing_parent() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        fs::create_dir_all(temp.path().join("pkg")).unwrap();

        commit_file(
            &repo,
            "pkg/deleted.py",
            "def foo():\n    return 1\n",
            "init",
        );
        let absolute_path = temp
            .path()
            .join("pkg/deleted.py")
            .to_string_lossy()
            .to_string();
        fs::remove_file(temp.path().join("pkg/deleted.py")).unwrap();

        let bridge = GitBridge::open(&temp.path().join("pkg")).unwrap();
        let files = bridge
            .get_changed_files(&DiffScope::Working, &[absolute_path])
            .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_path, "pkg/deleted.py");
        assert_eq!(files[0].status, FileStatus::Deleted);
    }

    #[test]
    fn absolute_missing_pathspecs_preserve_trailing_component_order() {
        let temp = TempDir::new().unwrap();
        let existing_parent = temp.path().join("existing");
        fs::create_dir(&existing_parent).unwrap();

        let pathspec = existing_parent.join("missing").join("leaf.py");
        let normalized = normalize_absolute_pathspec(&pathspec);

        let mut expected = fs::canonicalize(&existing_parent).unwrap();
        expected.push("missing");
        expected.push("leaf.py");
        assert_eq!(normalized, expected);
    }

    #[test]
    fn absolute_pathspecs_outside_repo_are_rejected() {
        let repo_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();

        commit_file(&repo, "sample.py", "def foo():\n    return 1\n", "init");
        fs::write(
            repo_dir.path().join("sample.py"),
            "def foo():\n    return 2\n",
        )
        .unwrap();
        let outside_path = outside_dir.path().join("outside.py");
        fs::write(&outside_path, "def outside():\n    return 1\n").unwrap();

        let bridge = GitBridge::open(repo_dir.path()).unwrap();
        let err = bridge
            .get_changed_files(
                &DiffScope::Working,
                &[outside_path.to_string_lossy().to_string()],
            )
            .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("pathspec"));
        assert!(message.contains("is outside repository"));
    }

    #[test]
    fn working_binary_modification_is_reported_as_binary_change() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_binary_file(&repo, "pic.png", b"\0png-v1\0", "init");
        fs::write(temp.path().join("pic.png"), b"\0png-v2\0extra").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Working, &[]).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_path, "pic.png");
        assert_eq!(files[0].status, FileStatus::Modified);
        assert!(files[0].before_content.is_none());
        assert!(files[0].after_content.is_none());

        let binary_changes = collect_binary_file_changes(&files);
        let registry = create_default_registry();
        let result = compute_semantic_diff(&files, &registry, None, None);

        assert!(result.changes.is_empty());
        assert_eq!(result.file_count, 0);
        assert_eq!(binary_changes.len(), 1);
        assert_eq!(binary_changes[0].file_path, "pic.png");
        assert_eq!(binary_changes[0].status, FileStatus::Modified);
    }

    #[test]
    fn staged_binary_add_and_delete_are_reported_as_binary_changes() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        fs::write(temp.path().join("added.png"), b"\0added-binary\0").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("added.png")).unwrap();
        index.write().unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let added_files = bridge.get_changed_files(&DiffScope::Staged, &[]).unwrap();
        assert_eq!(added_files.len(), 1);
        assert_eq!(added_files[0].file_path, "added.png");
        assert_eq!(added_files[0].status, FileStatus::Added);
        assert!(added_files[0].before_content.is_none());
        assert!(added_files[0].after_content.is_none());
        let added_binary_changes = collect_binary_file_changes(&added_files);
        assert_eq!(added_binary_changes.len(), 1);
        assert_eq!(added_binary_changes[0].file_path, "added.png");

        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        commit_binary_file(&repo, "deleted.png", b"\0deleted-binary\0", "init");
        fs::remove_file(temp.path().join("deleted.png")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("deleted.png")).unwrap();
        index.write().unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let deleted_files = bridge.get_changed_files(&DiffScope::Staged, &[]).unwrap();
        assert_eq!(deleted_files.len(), 1);
        assert_eq!(deleted_files[0].file_path, "deleted.png");
        assert_eq!(deleted_files[0].status, FileStatus::Deleted);
        assert!(deleted_files[0].before_content.is_none());
        assert!(deleted_files[0].after_content.is_none());
        let deleted_binary_changes = collect_binary_file_changes(&deleted_files);
        assert_eq!(deleted_binary_changes.len(), 1);
        assert_eq!(deleted_binary_changes[0].file_path, "deleted.png");
    }

    #[test]
    fn partial_utf8_boundary_is_not_treated_as_binary() {
        assert!(!GitBridge::bytes_look_binary(&[0xe2, 0x82], false));
        assert!(GitBridge::bytes_look_binary(&[0xe2, 0x82], true));
    }

    #[test]
    fn staged_file_rename_is_reported_as_single_rename_with_old_contents() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let contents = "export function foo() {\n  return 1;\n}\n";
        commit_file(&repo, "old.ts", contents, "init");

        fs::rename(temp.path().join("old.ts"), temp.path().join("new.ts")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("old.ts")).unwrap();
        index.add_path(Path::new("new.ts")).unwrap();
        index.write().unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Staged, &[]).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].file_path, "new.ts");
        assert_eq!(files[0].old_file_path.as_deref(), Some("old.ts"));
        assert_eq!(files[0].before_content.as_deref(), Some(contents));
        assert_eq!(files[0].after_content.as_deref(), Some(contents));
    }

    #[test]
    fn staged_file_rename_with_edit_reports_single_moved_entity() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let before = "\
// shared header 01
// shared header 02
// shared header 03
// shared header 04
// shared header 05
// shared header 06
// shared header 07
// shared header 08
// shared header 09
// shared header 10
export function foo() {
  return alpha + beta + gamma;
}
";
        let after = before.replace("return alpha + beta + gamma;", "return one + two + three;");

        commit_file(&repo, "old.ts", before, "init");
        fs::rename(temp.path().join("old.ts"), temp.path().join("new.ts")).unwrap();
        fs::write(temp.path().join("new.ts"), &after).unwrap();

        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("old.ts")).unwrap();
        index.add_path(Path::new("new.ts")).unwrap();
        index.write().unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Staged, &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);

        let registry = create_default_registry();
        let result = compute_semantic_diff(&files, &registry, None, None);

        assert_eq!(result.added_count, 0);
        assert_eq!(result.deleted_count, 0);
        // `foo` is a compound Moved change whose body also changed, so it counts toward
        // both moved_count and modified_count.
        assert_eq!(result.modified_count, 1);
        assert_eq!(result.moved_count, 1);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Moved);
        assert_eq!(result.changes[0].entity_name, "foo");
        assert_eq!(result.changes[0].old_file_path.as_deref(), Some("old.ts"));
        assert_eq!(result.changes[0].structural_change, Some(true));
    }

    #[test]
    fn working_diff_preserves_staged_rename_with_unstaged_edit() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let before = "\
export function foo(x: number) {
  return x + 1;
}

export function bar(y: number) {
  return y * 2;
}
";
        let after = "\
export function foo(x: number) {
  return x + 42;
}

export function bar(y: number) {
  return y * 99;
}
";

        commit_file(&repo, "a.ts", before, "init");

        fs::rename(temp.path().join("a.ts"), temp.path().join("b.ts")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("a.ts")).unwrap();
        index.add_path(Path::new("b.ts")).unwrap();
        index.write().unwrap();

        fs::write(temp.path().join("b.ts"), after).unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let (scope, files) = bridge.detect_and_get_files(&[]).unwrap();

        assert!(matches!(scope, DiffScope::Working));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].file_path, "b.ts");
        assert_eq!(files[0].old_file_path.as_deref(), Some("a.ts"));
        assert_eq!(files[0].before_content.as_deref(), Some(before));
        assert_eq!(files[0].after_content.as_deref(), Some(after));

        let registry = create_default_registry();
        let result = compute_semantic_diff(&files, &registry, None, None);

        assert_eq!(result.added_count, 0);
        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.modified_count, 2);
        assert_eq!(result.moved_count, 2);
        assert_eq!(result.changes.len(), 2);
        assert!(result
            .changes
            .iter()
            .all(|change| change.change_type == ChangeType::Moved));
        assert!(result
            .changes
            .iter()
            .all(|change| change.old_file_path.as_deref() == Some("a.ts")));
        assert!(result
            .changes
            .iter()
            .all(|change| change.structural_change == Some(true)));
    }

    #[test]
    fn working_diff_uses_staged_rename_map_after_large_unstaged_rewrite() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let before_noise = (0..200)
            .map(|i| format!("// old filler {i} alpha beta gamma"))
            .collect::<Vec<_>>()
            .join("\n");
        let after_noise = (0..200)
            .map(|i| format!("// new filler {i} delta epsilon zeta"))
            .collect::<Vec<_>>()
            .join("\n");
        let before =
            format!("{before_noise}\nexport function foo(x: number) {{\n  return x + 1;\n}}\n");
        let after =
            format!("{after_noise}\nexport function foo(x: number) {{\n  return x + 42;\n}}\n");

        commit_file(&repo, "a.ts", &before, "init");

        fs::rename(temp.path().join("a.ts"), temp.path().join("b.ts")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("a.ts")).unwrap();
        index.add_path(Path::new("b.ts")).unwrap();
        index.write().unwrap();

        fs::write(temp.path().join("b.ts"), &after).unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let (scope, files) = bridge.detect_and_get_files(&[]).unwrap();

        assert!(matches!(scope, DiffScope::Working));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].file_path, "b.ts");
        assert_eq!(files[0].old_file_path.as_deref(), Some("a.ts"));
        assert_eq!(files[0].before_content.as_deref(), Some(before.as_str()));
        assert_eq!(files[0].after_content.as_deref(), Some(after.as_str()));

        let registry = create_default_registry();
        let result = compute_semantic_diff(&files, &registry, None, None);

        assert_eq!(result.added_count, 0);
        assert_eq!(result.deleted_count, 0);
        // Two changes: the rewritten comment block is a Modified orphan, and `foo` is a
        // compound Moved change whose body also changed, so it counts toward both
        // moved_count and modified_count.
        assert_eq!(result.modified_count, 2);
        assert_eq!(result.moved_count, 1);
        assert!(result
            .changes
            .iter()
            .any(|change| change.change_type == ChangeType::Moved && change.entity_name == "foo"));
    }

    #[test]
    fn explicit_ref_to_working_uses_index_rename_map_after_large_unstaged_rewrite() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let before_noise = (0..200)
            .map(|i| format!("// old filler {i} alpha beta gamma"))
            .collect::<Vec<_>>()
            .join("\n");
        let after_noise = (0..200)
            .map(|i| format!("// new filler {i} delta epsilon zeta"))
            .collect::<Vec<_>>()
            .join("\n");
        let before =
            format!("{before_noise}\nexport function foo(x: number) {{\n  return x + 1;\n}}\n");
        let after =
            format!("{after_noise}\nexport function foo(x: number) {{\n  return x + 42;\n}}\n");

        commit_file(&repo, "a.ts", &before, "init");

        fs::rename(temp.path().join("a.ts"), temp.path().join("b.ts")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("a.ts")).unwrap();
        index.add_path(Path::new("b.ts")).unwrap();
        index.write().unwrap();

        fs::write(temp.path().join("b.ts"), &after).unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge
            .get_changed_files(
                &DiffScope::RefToWorking {
                    refspec: "HEAD".to_string(),
                },
                &[],
            )
            .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].file_path, "b.ts");
        assert_eq!(files[0].old_file_path.as_deref(), Some("a.ts"));
        assert_eq!(files[0].before_content.as_deref(), Some(before.as_str()));
        assert_eq!(files[0].after_content.as_deref(), Some(after.as_str()));

        let registry = create_default_registry();
        let result = compute_semantic_diff(&files, &registry, None, None);

        assert_eq!(result.added_count, 0);
        assert_eq!(result.deleted_count, 0);
        // Two changes: the rewritten comment block is a Modified orphan, and `foo` is a
        // compound Moved change whose body also changed, so it counts toward both
        // moved_count and modified_count.
        assert_eq!(result.modified_count, 2);
        assert_eq!(result.moved_count, 1);
        assert!(result
            .changes
            .iter()
            .any(|change| change.change_type == ChangeType::Moved && change.entity_name == "foo"));
    }

    #[test]
    fn staged_rename_map_overrides_wrong_worktree_rename_pairing() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let a_before = "export function foo(x: number) {\n  return x + 1;\n}\n";
        let c_before = "export function foo(x: number) {\n  return x + 42;\n}\n";

        commit_file(&repo, "a.ts", a_before, "init a");
        commit_file(&repo, "c.ts", c_before, "init c");

        fs::rename(temp.path().join("a.ts"), temp.path().join("b.ts")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("a.ts")).unwrap();
        index.add_path(Path::new("b.ts")).unwrap();
        index.write().unwrap();

        fs::remove_file(temp.path().join("c.ts")).unwrap();
        fs::write(temp.path().join("b.ts"), c_before).unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let (scope, files) = bridge.detect_and_get_files(&[]).unwrap();

        assert!(matches!(scope, DiffScope::Working));
        let renamed = files
            .iter()
            .find(|file| {
                file.status == FileStatus::Renamed
                    && file.file_path == "b.ts"
                    && file.old_file_path.as_deref() == Some("a.ts")
            })
            .unwrap();
        assert_eq!(renamed.before_content.as_deref(), Some(a_before));
        assert_eq!(renamed.after_content.as_deref(), Some(c_before));

        let deleted = files
            .iter()
            .find(|file| file.status == FileStatus::Deleted && file.file_path == "c.ts")
            .unwrap();
        assert_eq!(deleted.before_content.as_deref(), Some(c_before));
        assert_eq!(deleted.after_content.as_deref(), None);
        assert!(!files.iter().any(|file| {
            file.status == FileStatus::Renamed
                && file.file_path == "b.ts"
                && file.old_file_path.as_deref() == Some("c.ts")
        }));
    }

    #[test]
    fn staged_diff_with_base_ref_compares_index_to_that_ref() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        let v1 = "def foo():\n    return 1\n";
        let v2 = "def foo():\n    return 2\n";
        let v3 = "def foo():\n    return 3\n";
        let v4 = "def foo():\n    return 4\n";

        commit_file(&repo, "a.py", v1, "init");
        commit_file(&repo, "a.py", v2, "second");
        fs::write(temp.path().join("a.py"), v3).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.py")).unwrap();
        index.write().unwrap();

        fs::write(temp.path().join("a.py"), v4).unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge
            .get_staged_files_with_base_ref("HEAD~1", &[])
            .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[0].file_path, "a.py");
        assert_eq!(files[0].before_content.as_deref(), Some(v1));
        assert_eq!(files[0].after_content.as_deref(), Some(v3));

        let registry = create_default_registry();
        let result = compute_semantic_diff(&files, &registry, None, None);

        assert_eq!(result.modified_count, 1);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Modified);
        assert_eq!(result.changes[0].entity_name, "foo");
    }

    #[test]
    fn crlf_only_difference_in_working_file_is_invisible() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(&repo, "sample.rs", "fn a() {}\n", "init");
        fs::write(temp.path().join("sample.rs"), "fn a() {}\r\n").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Working, &[]).unwrap();

        assert_eq!(
            files.len(),
            1,
            "expected git to detect the CRLF change as modified"
        );

        let before = files[0].before_content.as_deref().unwrap();
        let after = files[0].after_content.as_deref().unwrap();

        assert_eq!(
            before, after,
            "CRLF-only difference should be invisible after normalization"
        );
    }

    #[test]
    fn crlf_stored_in_blob_is_normalized_on_read() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        repo.config()
            .unwrap()
            .set_str("core.autocrlf", "false")
            .unwrap();
        commit_file(&repo, "sample.rs", "fn a() {}\r\n", "init");
        fs::write(temp.path().join("sample.rs"), "fn a() {}\r\nfn b() {}\r\n").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Working, &[]).unwrap();

        assert_eq!(files.len(), 1, "expected git to detect the modification");

        let before = files[0].before_content.as_deref().unwrap();
        assert!(
            !before.contains('\r'),
            "before_content read from CRLF blob should be normalized to LF"
        );
    }
}
