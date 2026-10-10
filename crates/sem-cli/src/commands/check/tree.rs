//! Git identities of what is checked: the base revision's tree, the working
//! tree as a tree id (uncommitted and untracked files included), and the
//! paths that differ between them.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub(crate) struct Rev {
    pub spec: String,
    pub commit: String,
    pub tree: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Head {
    /// HEAD's commit, when there is one.
    pub commit: Option<String>,
    /// The working tree's content as a git tree id.
    pub tree: String,
    /// The working tree differs from HEAD (tracked changes or untracked files).
    pub dirty: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Change {
    /// `A`, `M`, `D` or `T` (git's name-status letters; renames are split).
    pub status: char,
    pub path: String,
}

fn git(root: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut c = Command::new("git");
    c.args(args).current_dir(root);
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    let o = c.output().map_err(|e| format!("git {}: {e}", args[0]))?;
    if !o.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let o = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("git {}: {e}", args[0]))?;
    if !o.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(o.stdout)
}

pub(crate) fn toplevel(start: &Path) -> Option<PathBuf> {
    git(start, &["rev-parse", "--show-toplevel"], None)
        .ok()
        .map(|s| PathBuf::from(s.trim()))
}

pub(crate) fn rev(root: &Path, spec: &str) -> Option<Rev> {
    let commit = git(root, &["rev-parse", "--verify", "-q", &format!("{spec}^{{commit}}")], None).ok()?;
    let commit = commit.trim().to_string();
    let tree = git(root, &["rev-parse", "--verify", "-q", &format!("{commit}^{{tree}}")], None).ok()?;
    Some(Rev { spec: spec.to_string(), commit, tree: tree.trim().to_string() })
}

/// The first root commit reachable from HEAD: the same in every clone.
pub(crate) fn root_commit(root: &Path) -> Option<String> {
    let out = git(root, &["rev-list", "--max-parents=0", "HEAD"], None).ok()?;
    out.lines().last().map(|s| s.trim().to_string())
}

/// The working tree as a tree id. Clean: HEAD's tree. Otherwise a copy of the
/// index updated with every changed and untracked (not ignored) path, written
/// as a tree; the real index is never touched.
pub(crate) fn head(root: &Path) -> Result<Head, String> {
    let commit = git(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], None)
        .ok()
        .map(|s| s.trim().to_string());
    let status = git_bytes(
        root,
        &["-c", "core.quotepath=off", "status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"],
    )?;
    let mut paths: Vec<String> = Vec::new();
    for entry in status.split(|b| *b == 0) {
        if entry.len() > 3 {
            paths.push(String::from_utf8_lossy(&entry[3..]).to_string());
        }
    }
    if paths.is_empty() {
        if let Some(c) = &commit {
            let tree = git(root, &["rev-parse", &format!("{c}^{{tree}}")], None)?;
            return Ok(Head { commit, tree: tree.trim().to_string(), dirty: false });
        }
    }
    let index = PathBuf::from(git(root, &["rev-parse", "--path-format=absolute", "--git-path", "index"], None)?.trim());
    let scratch = super::util::Scratch::new().map_err(|e| e.to_string())?;
    let tmp = scratch.path("index");
    if index.exists() {
        std::fs::copy(&index, &tmp).map_err(|e| format!("copying the index: {e}"))?;
    }
    let mut c = Command::new("git");
    c.args(["update-index", "--add", "--remove", "-z", "--stdin"])
        .current_dir(root)
        .env("GIT_INDEX_FILE", &tmp)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("git update-index: {e}"))?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        for p in &paths {
            let _ = stdin.write_all(p.as_bytes());
            let _ = stdin.write_all(&[0]);
        }
    }
    let o = child.wait_with_output().map_err(|e| format!("git update-index: {e}"))?;
    if !o.status.success() {
        return Err(format!("git update-index: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    let tree = git(root, &["write-tree"], Some(&tmp))?;
    Ok(Head { commit, tree: tree.trim().to_string(), dirty: true })
}

/// Every file path in `tree`.
pub(crate) fn files(root: &Path, tree: &str) -> Result<Vec<String>, String> {
    let out = git_bytes(root, &["ls-tree", "-r", "-z", "--name-only", tree])?;
    Ok(out.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect())
}

/// The content of `path` in `tree`.
pub(crate) fn blob(root: &Path, tree: &str, path: &str) -> Result<String, String> {
    git_bytes(root, &["cat-file", "-p", &format!("{tree}:{path}")]).map(|b| String::from_utf8_lossy(&b).to_string())
}

/// Paths that differ between two trees.
pub(crate) fn diff(root: &Path, a: &str, b: &str) -> Result<Vec<Change>, String> {
    if a == b {
        return Ok(Vec::new());
    }
    let out = git_bytes(root, &["diff-tree", "-r", "--no-renames", "--name-status", "-z", a, b])?;
    let mut v = Vec::new();
    let mut it = out.split(|b| *b == 0).filter(|s| !s.is_empty());
    while let (Some(st), Some(p)) = (it.next(), it.next()) {
        v.push(Change {
            status: st.first().map(|c| *c as char).unwrap_or('M'),
            path: String::from_utf8_lossy(p).to_string(),
        });
    }
    Ok(v)
}
