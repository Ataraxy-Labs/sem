use std::path::Path;
use std::process::Command;

/// Check if the given repo root is a Jujutsu repository.
pub fn is_jj_repo(root: &Path) -> bool {
    root.join(".jj").is_dir()
}

/// Resolve a jj revset to a git commit SHA using `jj log`.
/// Returns None if jj is not installed, the revset is invalid, or resolution fails.
pub fn resolve_jj_revset(revset: &str, root: &Path) -> Option<String> {
    let output = Command::new("jj")
        .args(["log", "--no-graph", "-T", "commit_id ++ \"\\n\"", "-r", revset])
        .current_dir(root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let sha = stdout.lines().next()?.trim().to_string();

    // Validate: must be a 40-char hex string
    if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha)
    } else {
        None
    }
}

/// Try to resolve a ref string via jj if we're in a jj repo.
/// Falls back to the original string if not a jj repo or resolution fails.
pub fn maybe_resolve_ref(refspec: &str, root: &Path) -> String {
    if !is_jj_repo(root) {
        return refspec.to_string();
    }

    // Skip refs that are already valid hex SHAs (no need to resolve)
    let trimmed = refspec.trim();
    if trimmed.len() >= 7
        && trimmed.len() <= 40
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
    {
        return refspec.to_string();
    }

    resolve_jj_revset(refspec, root).unwrap_or_else(|| refspec.to_string())
}
