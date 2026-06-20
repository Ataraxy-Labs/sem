//! Filesystem watcher that keeps the MCP server's in-memory entity graph hot.
//!
//! Without this, every whole-repo tool call (`sem_impact`, `sem_context`) walks
//! the tree and stats every source file just to decide whether the cached graph
//! is still fresh. On a large repo that is tens of milliseconds of pure overhead
//! per call, and the incremental rebuild then happens *on* the call.
//!
//! The watcher flips that around. A background OS file watcher (FSEvents on
//! macOS, inotify on Linux) records which files changed since the last build and
//! bumps a generation counter. When nothing has changed, a tool call returns the
//! cached graph with no walk and no stat storm. When something has changed, only
//! then do we re-walk / rebuild. This is the same model rust-analyzer and
//! tsserver use: trust the watcher, fall back to a full check when it is absent.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use sem_core::utils::scan::is_default_excluded;

/// Whether file watching is enabled. On by default; opt out with `SEM_NO_WATCH`.
pub fn watch_enabled() -> bool {
    !std::env::var("SEM_NO_WATCH").is_ok_and(|v| !v.is_empty() && v != "0")
}

#[derive(Default)]
struct Pending {
    /// Repo-relative paths whose content changed since the last drain.
    changed: HashSet<String>,
    /// A create / remove / rename happened, so the *set* of files may have
    /// changed and the next rebuild must re-walk the tree.
    needs_rewalk: bool,
}

/// A snapshot of pending changes, taken when a rebuild is about to happen.
pub struct Drained {
    pub generation: u64,
    pub needs_rewalk: bool,
    #[allow(dead_code)]
    pub changed: Vec<String>,
}

/// Watches a repo root and accumulates change notifications. Cheap to poll.
pub struct RepoWatcher {
    generation: Arc<AtomicU64>,
    pending: Arc<Mutex<Pending>>,
    _watcher: RecommendedWatcher,
}

impl RepoWatcher {
    /// Start watching `repo_root` recursively. Errors if the OS watcher can't
    /// be created (caller should fall back to the stat-based path).
    pub fn start(repo_root: &Path) -> Result<Self, notify::Error> {
        let generation = Arc::new(AtomicU64::new(0));
        let pending = Arc::new(Mutex::new(Pending::default()));
        // Canonicalize so event paths (which the OS reports in canonical form,
        // e.g. /private/var on macOS) strip cleanly against the watched root.
        let root = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf());

        let cb_gen = generation.clone();
        let cb_pending = pending.clone();
        let cb_root = root.clone();

        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                let (structural, content) = classify(&event.kind);
                if !structural && !content {
                    return;
                }

                let mut changed_any = false;
                {
                    let mut pend = match cb_pending.lock() {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    for path in &event.paths {
                        let Some(rel) = repo_relative(&cb_root, path) else {
                            continue;
                        };
                        if rel.is_empty() || is_noise(&rel) {
                            continue;
                        }
                        if structural {
                            pend.needs_rewalk = true;
                        }
                        pend.changed.insert(rel);
                        changed_any = true;
                    }
                }
                if changed_any {
                    cb_gen.fetch_add(1, Ordering::SeqCst);
                }
            })?;

        watcher.watch(&root, RecursiveMode::Recursive)?;

        Ok(Self {
            generation,
            pending,
            _watcher: watcher,
        })
    }

    /// Capture the current generation and clear pending changes. Any event that
    /// races in after this only bumps the generation again, so the next poll
    /// rebuilds — at worst a redundant rebuild, never a stale read.
    pub fn drain(&self) -> Drained {
        let generation = self.generation.load(Ordering::SeqCst);
        let mut pend = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let changed: Vec<String> = pend.changed.drain().collect();
        let needs_rewalk = pend.needs_rewalk;
        pend.needs_rewalk = false;
        Drained {
            generation,
            needs_rewalk,
            changed,
        }
    }
}

/// Map a `notify` event kind to (is_structural, is_content_change).
fn classify(kind: &EventKind) -> (bool, bool) {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => (true, false),
        EventKind::Modify(ModifyKind::Name(_)) => (true, false),
        EventKind::Modify(_) => (false, true),
        _ => (false, false),
    }
}

fn repo_relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Skip churny paths so routine git/build activity doesn't force rebuilds.
fn is_noise(rel: &str) -> bool {
    rel == ".git" || rel.starts_with(".git/") || rel.contains("/.git/") || is_default_excluded(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, RemoveKind, RenameMode};

    #[test]
    fn git_internal_writes_are_noise() {
        // git churns these constantly; they must not force graph rebuilds.
        assert!(is_noise(".git"));
        assert!(is_noise(".git/index"));
        assert!(is_noise(".git/refs/heads/main"));
        assert!(is_noise("submodule/.git/HEAD"));
    }

    #[test]
    fn real_source_edits_are_not_noise() {
        assert!(!is_noise("src/main.rs"));
        assert!(!is_noise("crates/sem-mcp/src/watch.rs"));
    }

    #[test]
    fn classify_distinguishes_structural_from_content() {
        // Create / remove / rename change the file set -> structural.
        assert_eq!(
            classify(&EventKind::Create(CreateKind::File)),
            (true, false)
        );
        assert_eq!(
            classify(&EventKind::Remove(RemoveKind::File)),
            (true, false)
        );
        assert_eq!(
            classify(&EventKind::Modify(ModifyKind::Name(RenameMode::Both))),
            (true, false)
        );
        // Plain content edits don't change the file set.
        assert_eq!(
            classify(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            (false, true)
        );
        // Access events are irrelevant.
        assert_eq!(
            classify(&EventKind::Access(notify::event::AccessKind::Read)),
            (false, false)
        );
    }
}
