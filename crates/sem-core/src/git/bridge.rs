use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use git2::{Blame, Delta, Diff, DiffFindOptions, DiffOptions, ErrorCode, Oid, Repository};
use thiserror::Error;

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

    pub fn blame_file(&self, file_path: &Path) -> Result<Blame<'_>, GitError> {
        Ok(self.repo.blame_file(file_path, None)?)
    }

    pub fn commit_summary(&self, oid: Oid) -> Option<String> {
        self.repo
            .find_commit(oid)
            .ok()
            .and_then(|commit| commit.summary().map(String::from))
    }

    pub fn get_head_sha(&self) -> Result<String, GitError> {
        let head = self.repo.head()?;
        let oid = head.target().ok_or_else(|| {
            git2::Error::from_str("HEAD has no target")
        })?;
        Ok(oid.to_string())
    }

    /// Combined detect scope + get files in one call (fast path).
    /// Matches `git diff` behavior: shows working tree changes by default.
    /// Use `--staged` for staged changes only.
    pub fn detect_and_get_files(&self, pathspecs: &[String]) -> Result<(DiffScope, Vec<FileChange>), GitError> {
        // Show working tree changes (unstaged), matching git diff behavior
        let mut working_files = self.get_working_diff_files(pathspecs)?;
        if !working_files.is_empty() {
            self.populate_contents(&mut working_files, &DiffScope::Working)?;
            return Ok((DiffScope::Working, working_files));
        }

        // Clean worktree = no changes
        Ok((DiffScope::Working, Vec::new()))
    }

    /// Get changed files for a specific scope
    pub fn get_changed_files(&self, scope: &DiffScope, pathspecs: &[String]) -> Result<Vec<FileChange>, GitError> {
        let mut files = match scope {
            DiffScope::Working => {
                self.get_working_diff_files(pathspecs)?
            }
            DiffScope::Staged => self.get_staged_diff_files(pathspecs)?,
            DiffScope::Commit { sha } => self.get_commit_diff_files(sha, pathspecs)?,
            DiffScope::Range { from, to } => self.get_range_diff_files(from, to, pathspecs)?,
            DiffScope::RefToWorking { refspec } => self.get_ref_to_working_diff_files(refspec, pathspecs)?,
        };

        // Filter .sem/ files
        files.retain(|f| !f.file_path.starts_with(".sem/"));

        self.populate_contents(&mut files, scope)?;
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
        let relative =
            absolute
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
        let head_tree = match self.repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit()?;
                Some(commit.tree()?)
            }
            Err(_) => None, // No commits yet
        };

        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff = self.repo.diff_tree_to_index(
            head_tree.as_ref(),
            Some(&self.repo.index()?),
            Some(&mut opts),
        )?;
        Self::detect_renames(&mut diff)?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_working_diff_files(&self, pathspecs: &[String]) -> Result<Vec<FileChange>, GitError> {
        let mut opts = self.make_diff_opts(pathspecs)?;
        opts.include_untracked(false);

        let mut diff = self.repo.diff_index_to_workdir(None, Some(&mut opts))?;
        Self::detect_renames(&mut diff)?;
        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_commit_diff_files(&self, sha: &str, pathspecs: &[String]) -> Result<Vec<FileChange>, GitError> {
        let obj = self.repo.revparse_single(sha)?;
        let commit = obj.peel_to_commit()?;
        let tree = commit.tree()?;

        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };

        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff = self.repo.diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&tree),
            Some(&mut opts),
        )?;
        Self::detect_renames(&mut diff)?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_range_diff_files(&self, from: &str, to: &str, pathspecs: &[String]) -> Result<Vec<FileChange>, GitError> {
        let from_obj = self.repo.revparse_single(from)?;
        let to_obj = self.repo.revparse_single(to)?;

        let from_tree = from_obj.peel_to_commit()?.tree()?;
        let to_tree = to_obj.peel_to_commit()?.tree()?;

        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff = self.repo.diff_tree_to_tree(
            Some(&from_tree),
            Some(&to_tree),
            Some(&mut opts),
        )?;
        Self::detect_renames(&mut diff)?;

        Ok(self.diff_to_file_changes(&diff))
    }

    fn get_ref_to_working_diff_files(&self, refspec: &str, pathspecs: &[String]) -> Result<Vec<FileChange>, GitError> {
        let tree = self.resolve_tree(refspec)?;
        let mut opts = self.make_diff_opts(pathspecs)?;
        let mut diff = self.repo.diff_tree_to_workdir_with_index(
            Some(&tree),
            Some(&mut opts),
        )?;
        Self::detect_renames(&mut diff)?;
        Ok(self.diff_to_file_changes(&diff))
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
                        let path = file
                            .old_file_path
                            .as_deref()
                            .unwrap_or(&file.file_path);
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
                        let path = file
                            .old_file_path
                            .as_deref()
                            .unwrap_or(&file.file_path);
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
                        file.after_content =
                            self.read_blob_from_tree(&after_tree, &file.file_path);
                    }
                    if file.status != FileStatus::Added {
                        let path = file
                            .old_file_path
                            .as_deref()
                            .unwrap_or(&file.file_path);
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
            DiffScope::RefToWorking { refspec } => {
                let before_tree = self.resolve_tree(refspec)?;
                for file in files.iter_mut() {
                    if file.status != FileStatus::Deleted {
                        file.after_content = self.read_working_file(&file.file_path);
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


    /// Read file content at a specific git ref (commit SHA, branch, tag, etc.)
    pub fn read_file_at_ref(&self, refspec: &str, file_path: &str) -> Result<Option<String>, GitError> {
        let tree = self.resolve_tree(refspec)?;
        Ok(self.read_blob_from_tree(&tree, file_path))
    }

    /// Get commits that modified a specific file, walking history from HEAD.
    /// Returns commits in reverse chronological order (newest first).
    pub fn get_file_commits(&self, file_path: &str, limit: usize) -> Result<Vec<CommitInfo>, GitError> {
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;
        revwalk.set_sorting(git2::Sort::TIME)?;

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
                commit.parent(0)
                    .ok()
                    .and_then(|p| p.tree().ok())
                    .and_then(|t| t.get_path(path).ok().map(|e| e.id()))
            } else {
                None // No parent = initial commit, file was added
            };

            // Include if file changed between parent and this commit
            let changed = match (file_in_commit, file_in_parent) {
                (Some(cur), Some(prev)) => cur != prev,  // content changed
                (Some(_), None) => true,                   // file added
                (None, Some(_)) => true,                   // file deleted
                (None, None) => false,                     // file not present in either
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

                if commits.len() >= limit {
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
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;
        revwalk.set_sorting(git2::Sort::TIME)?;

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

                if results.len() >= limit {
                    break;
                }
            }

            // If the file is not in this commit's tree, check if it was renamed.
            // Compute diff between parent and this commit with rename detection.
            if file_in_commit.is_none() && parent_tree_opt.is_some() {
                let mut diff = self.repo.diff_tree_to_tree(
                    parent_tree_opt.as_ref(),
                    Some(&tree),
                    None,
                )?;
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

                if !found_rename {
                    // File truly deleted, stop tracking
                    break;
                }
            }
        }

        Ok(results)
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
        let diff = self.repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
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

fn should_retry_with_safe_directory(error: &git2::Error, path: &Path, safe_directories: &[String]) -> bool {
    error.code() == ErrorCode::Owner
        && nearest_git_root(path).is_some_and(|repo_root| {
            safe_directories.iter().any(|safe_directory| {
                safe_directory == "*"
                    || paths_match(&repo_root, Path::new(safe_directory))
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
    let mut current = if path.is_file() {
        path.parent()?
    } else {
        path
    };

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
    use crate::parser::differ::compute_semantic_diff;
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
        let (scope, files) = bridge.detect_and_get_files(&[]).unwrap();

        assert!(matches!(scope, DiffScope::Working));
        assert!(files.is_empty());
    }

    #[test]
    fn owner_error_retries_for_command_line_safe_directory() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();

        let owner_error = git2::Error::new(
            ErrorCode::Owner,
            ErrorClass::Config,
            "owner mismatch",
        );
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

        let not_found_error = git2::Error::new(
            ErrorCode::NotFound,
            ErrorClass::Repository,
            "not found",
        );
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
            }, &[])
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
        let after = before.replace(
            "return alpha + beta + gamma;",
            "return one + two + three;",
        );

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
        assert_eq!(result.moved_count, 1);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].change_type, ChangeType::Moved);
        assert_eq!(result.changes[0].entity_name, "foo");
        assert_eq!(result.changes[0].old_file_path.as_deref(), Some("old.ts"));
    }

    #[test]
    fn crlf_only_difference_in_working_file_is_invisible() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();

        commit_file(&repo, "sample.rs", "fn a() {}\n", "init");
        fs::write(temp.path().join("sample.rs"), "fn a() {}\r\n").unwrap();

        let bridge = GitBridge::open(temp.path()).unwrap();
        let files = bridge.get_changed_files(&DiffScope::Working, &[]).unwrap();

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
        let files = bridge.get_changed_files(&DiffScope::Working, &[]).unwrap();

        assert_eq!(files.len(), 1, "expected git to detect the modification");

        let before = files[0].before_content.as_deref().unwrap();
        assert!(!before.contains('\r'), "before_content read from CRLF blob should be normalized to LF");
    }
}
