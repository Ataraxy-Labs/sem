//! The check-state store: content-addressed, shared by every checkout of a
//! project, bounded, least-recently-used eviction.
//!
//! `<sem cache root>/check/<checker>/<key>.state`, `key` = the git tree id the
//! state was computed at plus the checker's configuration fingerprint. A read
//! refreshes the entry's mtime; every write evicts the oldest entries until the
//! store is within `SEM_CHECK_CACHE_MAX_MB` (default 512) and
//! `SEM_CHECK_CACHE_MAX_ENTRIES` (default 256). `SEM_CHECK_CACHE_DIR` moves it.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) struct Store {
    pub dir: PathBuf,
    max_bytes: u64,
    max_entries: usize,
}

fn env_u64(k: &str) -> Option<u64> {
    std::env::var(k).ok().and_then(|v| v.trim().parse().ok())
}

impl Store {
    pub fn open(repo_root: &Path) -> Option<Store> {
        let dir = match std::env::var("SEM_CHECK_CACHE_DIR").ok().filter(|s| !s.is_empty()) {
            Some(d) => PathBuf::from(d),
            None => sem_core::persist::disk_cache::cache_dir_for_repo(repo_root)?
                .parent()?
                .join("check"),
        };
        std::fs::create_dir_all(&dir).ok()?;
        Some(Store {
            dir,
            max_bytes: env_u64("SEM_CHECK_CACHE_MAX_MB").unwrap_or(512) * 1024 * 1024,
            max_entries: env_u64("SEM_CHECK_CACHE_MAX_ENTRIES").unwrap_or(256) as usize,
        })
    }

    fn entry(&self, checker: &str, key: &str) -> PathBuf {
        self.dir.join(checker).join(format!("{key}.state"))
    }

    /// The state at `key`, its mtime refreshed (the LRU clock).
    pub fn get(&self, checker: &str, key: &str) -> Option<PathBuf> {
        let p = self.entry(checker, key);
        if !p.is_file() {
            return None;
        }
        if let Ok(f) = std::fs::File::options().append(true).open(&p) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(p)
    }

    /// Move `file` in as the state at `key`, then evict.
    pub fn put(&self, checker: &str, key: &str, file: &Path) -> std::io::Result<PathBuf> {
        let p = self.entry(checker, key);
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::rename(file, &tmp).is_err() {
            std::fs::copy(file, &tmp)?;
        }
        std::fs::rename(&tmp, &p)?;
        self.evict();
        Ok(p)
    }

    /// Remember `key` as the newest state of `project` (for self-validating checkers).
    pub fn set_latest(&self, checker: &str, project: &str, key: &str) {
        let p = self.dir.join(checker).join(format!("latest-{project}"));
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, key).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    pub fn latest(&self, checker: &str, project: &str) -> Option<PathBuf> {
        let key = std::fs::read_to_string(self.dir.join(checker).join(format!("latest-{project}"))).ok()?;
        self.get(checker, key.trim())
    }

    /// Remove least recently used states until within both bounds.
    pub fn evict(&self) {
        let mut all: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        for d in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if !d.path().is_dir() || d.file_name() == "helpers" {
                continue;
            }
            for e in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "state") {
                    if let Ok(m) = e.metadata() {
                        all.push((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), p));
                    }
                }
            }
        }
        all.sort_by_key(|(t, _, _)| *t);
        let mut bytes: u64 = all.iter().map(|(_, n, _)| n).sum();
        let mut count = all.len();
        for (_, n, p) in all {
            if bytes <= self.max_bytes && count <= self.max_entries {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                bytes -= n;
                count -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Store;

    #[test]
    fn evicts_least_recently_used_beyond_the_entry_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store { dir: tmp.path().to_path_buf(), max_bytes: u64::MAX, max_entries: 2 };
        for k in ["a", "b", "c"] {
            let f = tmp.path().join(format!("in-{k}"));
            std::fs::write(&f, k).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            if k == "c" {
                // touch "a": now "b" is the least recently used
                assert!(s.get("ts", "a").is_some());
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            s.put("ts", k, &f).unwrap();
        }
        assert!(s.get("ts", "a").is_some());
        assert!(s.get("ts", "b").is_none());
        assert!(s.get("ts", "c").is_some());
    }

    #[test]
    fn evicts_beyond_the_byte_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store { dir: tmp.path().to_path_buf(), max_bytes: 10, max_entries: 100 };
        for k in ["a", "b"] {
            let f = tmp.path().join(format!("in-{k}"));
            std::fs::write(&f, "123456").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            s.put("lint", k, &f).unwrap();
        }
        assert!(s.get("lint", "a").is_none());
        assert!(s.get("lint", "b").is_some());
    }
}
