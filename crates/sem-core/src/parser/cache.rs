//! Content-addressed cache for parser entity extraction.
//!
//! # Why
//!
//! Entity extraction is a pure function of `(plugin_id, file_path, content)`:
//! the same three inputs always produce the same `Vec<SemanticEntity>`. That
//! makes the extraction result *content-addressable* — the cache key **is** the
//! hash of the inputs, so there is no invalidation problem. A changed file
//! hashes differently and simply misses.
//!
//! Callers that re-extract the same blob repeatedly get a large win. The
//! motivating case is `weave`, which parses base/ours/theirs versions of every
//! conflicted file on every merge, and in a fleet re-parses the *same* blob
//! across many merges.
//!
//! # Shape
//!
//! * Process-local, **on by default**, size-bounded (64 MiB by default).
//! * Sharded (8 ways) so parallel graph builds don't serialize on one mutex.
//! * Two-generation (`hot`/`cold`) eviction: an O(1) LRU approximation. When
//!   `hot` would exceed half the shard budget the generations rotate and the
//!   old `cold` is dropped. Anything touched in the current generation
//!   survives. A single result too big for one generation (by default 4 MiB of
//!   entities, i.e. a file far larger than any this is meant for) is simply not
//!   admitted rather than evicting everything around it.
//! * Optional on-disk tier, **off by default** (`SEM_PARSE_CACHE_DISK=1`),
//!   under `$SEM_PARSE_CACHE_DIR`, `$XDG_CACHE_HOME/sem/parse`, or
//!   `~/.cache/sem/parse`.
//!
//! # Configuration (environment)
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `SEM_PARSE_CACHE` | `1` | `0`/`off`/`false`/`no` disables the cache entirely |
//! | `SEM_PARSE_CACHE_BYTES` | `67108864` | in-process budget, bytes (`0` disables) |
//! | `SEM_PARSE_CACHE_DISK` | `0` | `1`/`on`/`true`/`yes` enables the on-disk tier |
//! | `SEM_PARSE_CACHE_DIR` | `~/.cache/sem/parse` | on-disk tier location |
//!
//! Environment is read once, on first use.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use xxhash_rust::xxh3::Xxh3;

use crate::model::entity::SemanticEntity;

const SHARDS: usize = 8;
const DEFAULT_CAPACITY_BYTES: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct Config {
    enabled: bool,
    capacity_bytes: usize,
    disk_dir: Option<PathBuf>,
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "0" | "off" | "false" | "no" | "" => false,
            _ => true,
        },
        Err(_) => default,
    }
}

fn default_disk_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SEM_PARSE_CACHE_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg).join("sem").join("parse"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|h| PathBuf::from(h).join(".cache").join("sem").join("parse"))
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let capacity_bytes = std::env::var("SEM_PARSE_CACHE_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_CAPACITY_BYTES);
        let enabled = env_flag("SEM_PARSE_CACHE", true) && capacity_bytes > 0;
        let disk_dir = if env_flag("SEM_PARSE_CACHE_DISK", false) {
            default_disk_dir()
        } else {
            None
        };
        Config {
            enabled,
            capacity_bytes,
            disk_dir,
        }
    })
}

/// Whether the in-process cache is active for this process.
pub fn is_enabled() -> bool {
    config().enabled
}

/// Whether the optional on-disk tier is active for this process.
pub fn is_disk_enabled() -> bool {
    config().disk_dir.is_some()
}

// ---------------------------------------------------------------------------
// Keying
// ---------------------------------------------------------------------------

/// Content-address the extraction inputs.
///
/// `file_path` participates because entity IDs and `file_path` fields are built
/// from it — the same bytes at a different path legitimately produce different
/// entities. `plugin_id` participates because two plugins can claim the same
/// path (e.g. `.svelte` handled by the svelte plugin vs the code plugin).
///
/// The installed fast extractor's identity
/// ([`crate::parser::fast_extractor::identity_salt`]) participates for the
/// same reason `plugin_id` does: two extractors can claim the same path and
/// legitimately produce different entities. Without it, flipping the fast
/// path on or off within one process — or between two runs sharing the
/// on-disk tier (`SEM_PARSE_CACHE_DISK=1`) — would serve one path's entities
/// to a caller expecting the other's, which is precisely the silent
/// divergence this module's "`extract` **must** be a pure function of its
/// inputs" contract exists to prevent. `None` (fast path off or empty)
/// contributes nothing, so a build without a fast path keys exactly as it
/// always did.
fn key_for(plugin_id: &str, file_path: &str, content: &str) -> u128 {
    let mut hasher = Xxh3::new();
    hasher.update(plugin_id.as_bytes());
    hasher.update(&[0]);
    if let Some(identity) = crate::parser::fast_extractor::identity_salt() {
        hasher.update(identity.as_bytes());
    }
    hasher.update(&[0]);
    hasher.update(file_path.as_bytes());
    hasher.update(&[0]);
    hasher.update(content.as_bytes());
    hasher.digest128()
}

/// A `structural_hash_content` result is keyed in its own namespace: it is a
/// different function of the same inputs.
fn structural_key_for(plugin_id: &str, file_path: &str, content: &str) -> u128 {
    let mut hasher = Xxh3::new();
    hasher.update(b"structural\0");
    hasher.update(plugin_id.as_bytes());
    hasher.update(&[0]);
    hasher.update(file_path.as_bytes());
    hasher.update(&[0]);
    hasher.update(content.as_bytes());
    hasher.digest128()
}

// ---------------------------------------------------------------------------
// Size accounting
// ---------------------------------------------------------------------------

fn entity_bytes(e: &SemanticEntity) -> usize {
    let mut n = std::mem::size_of::<SemanticEntity>();
    n += e.id.len()
        + e.file_path.len()
        + e.entity_type.len()
        + e.name.len()
        + e.content.len()
        + e.content_hash.len();
    n += e.parent_id.as_ref().map_or(0, |s| s.len());
    n += e.structural_hash.as_ref().map_or(0, |s| s.len());
    if let Some(meta) = &e.metadata {
        // 48 bytes/slot is a rough HashMap overhead allowance.
        n += meta
            .iter()
            .map(|(k, v)| k.len() + v.len() + 48)
            .sum::<usize>();
    }
    n
}

fn entities_bytes(entities: &[SemanticEntity]) -> usize {
    // 32 bytes/entry covers the map slot + Arc header amortized over the entry.
    32 + entities.iter().map(entity_bytes).sum::<usize>()
}

// ---------------------------------------------------------------------------
// Shard
// ---------------------------------------------------------------------------

struct Entry<T> {
    value: T,
    bytes: usize,
}

struct Shard<T> {
    hot: HashMap<u128, Entry<T>>,
    cold: HashMap<u128, Entry<T>>,
    hot_bytes: usize,
    cold_bytes: usize,
    budget: usize,
}

impl<T: Clone> Shard<T> {
    fn new(budget: usize) -> Self {
        Self {
            hot: HashMap::new(),
            cold: HashMap::new(),
            hot_bytes: 0,
            cold_bytes: 0,
            budget,
        }
    }

    /// The most one generation may hold. Two generations are resident, so this
    /// is what keeps the shard total under `budget`.
    fn generation_limit(&self) -> usize {
        self.budget / 2
    }

    fn get(&mut self, key: u128) -> Option<T> {
        if let Some(entry) = self.hot.get(&key) {
            return Some(entry.value.clone());
        }
        // Promote from the previous generation so a still-live entry survives
        // the next rotation. It is already out of `cold`, so a rotation here
        // cannot drop it.
        let entry = self.cold.remove(&key)?;
        self.cold_bytes = self.cold_bytes.saturating_sub(entry.bytes);
        let value = entry.value.clone();
        self.make_room(entry.bytes);
        self.hot_bytes += entry.bytes;
        self.hot.insert(key, entry);
        Some(value)
    }

    fn insert(&mut self, key: u128, value: T, bytes: usize) {
        // An entry that cannot fit in one generation is never worth holding:
        // admitting it would evict everything on every insert.
        if bytes > self.generation_limit() {
            return;
        }
        if let Some(old) = self.cold.remove(&key) {
            self.cold_bytes = self.cold_bytes.saturating_sub(old.bytes);
        }
        if let Some(old) = self.hot.remove(&key) {
            self.hot_bytes = self.hot_bytes.saturating_sub(old.bytes);
        }
        self.make_room(bytes);
        self.hot_bytes += bytes;
        self.hot.insert(key, Entry { value, bytes });
    }

    /// Two-generation eviction: rotate *before* an insert that would overflow
    /// the hot generation, so each generation stays within `generation_limit`
    /// and the shard total therefore stays within `budget`.
    fn make_room(&mut self, incoming: usize) {
        if self.hot_bytes + incoming <= self.generation_limit() {
            return;
        }
        self.cold = std::mem::take(&mut self.hot);
        self.cold_bytes = self.hot_bytes;
        self.hot_bytes = 0;
    }

    fn clear(&mut self) {
        self.hot.clear();
        self.cold.clear();
        self.hot_bytes = 0;
        self.cold_bytes = 0;
    }

    fn len(&self) -> usize {
        self.hot.len() + self.cold.len()
    }

    fn bytes(&self) -> usize {
        self.hot_bytes + self.cold_bytes
    }
}

// ---------------------------------------------------------------------------
// Global tables
// ---------------------------------------------------------------------------

type EntityShards = [Mutex<Shard<Arc<Vec<SemanticEntity>>>>; SHARDS];
type StructuralShards = [Mutex<Shard<Arc<str>>>; SHARDS];

/// Everything the cache owns: the two sharded stores and the hit/miss
/// counters. Production runs one process-global instance ([`global`]); tests
/// can bind a private one to their thread ([`install_test_instance`]) so
/// concurrently running tests cannot evict each other's entries or move each
/// other's counters.
struct Cache {
    entities: EntityShards,
    structural: StructuralShards,
    hits: AtomicU64,
    misses: AtomicU64,
    disk_hits: AtomicU64,
    disk_writes: AtomicU64,
}

impl Cache {
    fn new() -> Self {
        let entity_budget = config().capacity_bytes / SHARDS;
        // Structural hashes are tiny; a small slice of the budget is plenty.
        let structural_budget = (config().capacity_bytes / 64 / SHARDS).max(64 * 1024);
        Self {
            entities: std::array::from_fn(|_| Mutex::new(Shard::new(entity_budget.max(1)))),
            structural: std::array::from_fn(|_| Mutex::new(Shard::new(structural_budget))),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            disk_writes: AtomicU64::new(0),
        }
    }

    fn entity_shard(&self, key: u128) -> &Mutex<Shard<Arc<Vec<SemanticEntity>>>> {
        &self.entities[shard_index(key)]
    }

    fn structural_shard(&self, key: u128) -> &Mutex<Shard<Arc<str>>> {
        &self.structural[shard_index(key)]
    }
}

fn global() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::new)
}

thread_local! {
    /// The calling thread's private [`Cache`], installed by
    /// [`install_test_instance`]. Leaked, so reads are a pointer copy.
    static INSTANCE: Cell<Option<&'static Cache>> = const { Cell::new(None) };
}

/// Run `f` against the cache this thread is bound to: the process-global
/// instance, or the private one from [`install_test_instance`] when installed.
fn with<R>(f: impl FnOnce(&Cache) -> R) -> R {
    match INSTANCE.with(Cell::get) {
        Some(cache) => f(cache),
        None => f(global()),
    }
}

/// Test support: give the calling thread its own private cache instance.
///
/// Every entry point of this module routes through the thread's bound
/// instance, so a test that calls this sees its own entries and counters
/// alone: siblings sharing the global instance can neither evict its entries
/// nor inflate its counts. Production never calls this and keeps the single
/// process-global instance.
#[doc(hidden)]
pub fn install_test_instance() {
    let leaked: &'static Cache = Box::leak(Box::new(Cache::new()));
    INSTANCE.with(|slot| slot.set(Some(leaked)));
}

#[inline]
fn shard_index(key: u128) -> usize {
    (key >> 120) as usize % SHARDS
}

/// A snapshot of cache counters and occupancy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub enabled: bool,
    pub disk_enabled: bool,
    /// In-process hits (includes entries promoted from the disk tier).
    pub hits: u64,
    pub misses: u64,
    /// Hits that were served by the on-disk tier rather than memory.
    pub disk_hits: u64,
    pub disk_writes: u64,
    pub entries: usize,
    pub bytes: usize,
    pub capacity_bytes: usize,
}

/// Snapshot the cache counters. Cheap; intended for tests and diagnostics.
pub fn stats() -> CacheStats {
    with(|cache| {
        let cfg = config();
        let mut entries = 0;
        let mut bytes = 0;
        if cfg.enabled {
            for shard in cache.entities.iter() {
                let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
                entries += shard.len();
                bytes += shard.bytes();
            }
        }
        CacheStats {
            enabled: cfg.enabled,
            disk_enabled: cfg.disk_dir.is_some(),
            hits: cache.hits.load(Ordering::Relaxed),
            misses: cache.misses.load(Ordering::Relaxed),
            disk_hits: cache.disk_hits.load(Ordering::Relaxed),
            disk_writes: cache.disk_writes.load(Ordering::Relaxed),
            entries,
            bytes,
            capacity_bytes: cfg.capacity_bytes,
        }
    })
}

/// Drop every in-process entry. Does not touch the on-disk tier.
pub fn clear() {
    if !config().enabled {
        return;
    }
    with(|cache| {
        for shard in cache.entities.iter() {
            shard.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        for shard in cache.structural.iter() {
            shard.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    });
}

/// Zero the hit/miss counters. Intended for benchmarks and tests.
pub fn reset_stats() {
    with(|cache| {
        cache.hits.store(0, Ordering::Relaxed);
        cache.misses.store(0, Ordering::Relaxed);
        cache.disk_hits.store(0, Ordering::Relaxed);
        cache.disk_writes.store(0, Ordering::Relaxed);
    });
}

// ---------------------------------------------------------------------------
// Disk tier
// ---------------------------------------------------------------------------

fn disk_path(dir: &Path, key: u128) -> PathBuf {
    let hex = format!("{key:032x}");
    // Two-level fan-out keeps any single directory small.
    dir.join(&hex[0..2]).join(format!("{hex}.json"))
}

fn disk_load(dir: &Path, key: u128) -> Option<Vec<SemanticEntity>> {
    let bytes = std::fs::read(disk_path(dir, key)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn disk_store(cache: &Cache, dir: &Path, key: u128, entities: &[SemanticEntity]) {
    let path = disk_path(dir, key);
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_vec(entities) else {
        return;
    };
    // Write-then-rename so a concurrent reader never observes a partial file.
    // The temp name carries a pid and a per-process counter, so two threads (or
    // two `sem` processes) writing the same key never share a scratch file.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        cache.disk_writes.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// The cached-extraction entry point
// ---------------------------------------------------------------------------

/// Return the extraction result for `(plugin_id, file_path, content)`, running
/// `extract` only on a miss.
///
/// The key is the content hash of the inputs, so there is no invalidation: a
/// different input is a different key. `extract` **must** be a pure function of
/// its inputs — that is the whole contract.
///
/// When the cache is disabled this is exactly `extract()` plus a branch.
pub fn get_or_extract<F>(
    plugin_id: &str,
    file_path: &str,
    content: &str,
    extract: F,
) -> Vec<SemanticEntity>
where
    F: FnOnce() -> Vec<SemanticEntity>,
{
    let cfg = config();
    if !cfg.enabled {
        return extract();
    }

    with(|cache| {
        let key = key_for(plugin_id, file_path, content);
        let shard = cache.entity_shard(key);

        if let Some(hit) = shard.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
            cache.hits.fetch_add(1, Ordering::Relaxed);
            return (*hit).clone();
        }

        if let Some(dir) = cfg.disk_dir.as_deref() {
            if let Some(entities) = disk_load(dir, key) {
                cache.disk_hits.fetch_add(1, Ordering::Relaxed);
                cache.hits.fetch_add(1, Ordering::Relaxed);
                let bytes = entities_bytes(&entities);
                let arc = Arc::new(entities);
                shard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, Arc::clone(&arc), bytes);
                return (*arc).clone();
            }
        }

        cache.misses.fetch_add(1, Ordering::Relaxed);
        let entities = extract();
        let bytes = entities_bytes(&entities);
        if let Some(dir) = cfg.disk_dir.as_deref() {
            disk_store(cache, dir, key, &entities);
        }
        shard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::new(entities.clone()), bytes);
        entities
    })
}

/// Cached wrapper for `SemanticParserPlugin::structural_hash_content`, which
/// otherwise re-parses the file from scratch.
pub fn get_or_structural_hash<F>(
    plugin_id: &str,
    file_path: &str,
    content: &str,
    compute: F,
) -> Option<String>
where
    F: FnOnce() -> Option<String>,
{
    let cfg = config();
    if !cfg.enabled {
        return compute();
    }

    with(|cache| {
        let key = structural_key_for(plugin_id, file_path, content);
        let shard = cache.structural_shard(key);

        if let Some(hit) = shard.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
            cache.hits.fetch_add(1, Ordering::Relaxed);
            // An empty marker means "the plugin returned None"; an empty *hash* is
            // a real result and is stored as "\0".
            return match &*hit {
                "" => None,
                "\0" => Some(String::new()),
                s => Some(s.to_string()),
            };
        }

        cache.misses.fetch_add(1, Ordering::Relaxed);
        let computed = compute();
        let stored: Arc<str> = match computed.as_deref() {
            None => Arc::from(""),
            Some("") => Arc::from("\0"),
            Some(s) => Arc::from(s),
        };
        let bytes = 32 + stored.len() + 16;
        shard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, stored, bytes);
        computed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(name: &str) -> SemanticEntity {
        SemanticEntity {
            id: format!("f.rs::function::{name}"),
            file_path: "f.rs".to_string(),
            entity_type: "function".to_string(),
            name: name.to_string(),
            parent_id: None,
            content: "fn x() {}".to_string(),
            content_hash: "deadbeef".to_string(),
            structural_hash: None,

            kappa: None,
            start_line: 1,
            end_line: 1,
            start_byte: None,
            end_byte: None,
            metadata: None,
        }
    }

    // Tests share one process-global cache and run concurrently, so each test
    // uses its own file paths rather than calling `clear()`, which would wipe
    // a sibling test's entry mid-run.

    #[test]
    fn hit_returns_the_same_result_without_recomputing() {
        let calls = std::cell::Cell::new(0);
        let run = || {
            get_or_extract("test", "hit.rs", "fn a() {}", || {
                calls.set(calls.get() + 1);
                vec![entity("a")]
            })
        };
        let first = run();
        let second = run();
        assert_eq!(calls.get(), 1, "second call must be served from cache");
        assert_eq!(first.len(), second.len());
        assert_eq!(first[0].id, second[0].id);
    }

    #[test]
    fn different_content_is_a_different_key() {
        let a = get_or_extract("test", "content.rs", "fn a() {}", || vec![entity("a")]);
        let b = get_or_extract("test", "content.rs", "fn b() {}", || vec![entity("b")]);
        assert_eq!(a[0].name, "a");
        assert_eq!(b[0].name, "b");
    }

    #[test]
    fn different_path_is_a_different_key() {
        let a = get_or_extract("test", "path_a.rs", "fn a() {}", || vec![entity("a")]);
        let b = get_or_extract("test", "path_b.rs", "fn a() {}", || vec![entity("b")]);
        assert_eq!(a[0].name, "a");
        assert_eq!(b[0].name, "b");
    }

    #[test]
    fn mutating_a_returned_vec_does_not_corrupt_the_cache() {
        let mut first = get_or_extract("test", "m.rs", "fn m() {}", || vec![entity("m")]);
        first[0].name = "clobbered".to_string();
        first.clear();
        let second = get_or_extract("test", "m.rs", "fn m() {}", || {
            panic!("should have hit the cache")
        });
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].name, "m");
    }

    #[test]
    fn oversized_entries_are_not_admitted() {
        let mut shard: Shard<Arc<Vec<SemanticEntity>>> = Shard::new(1024);
        shard.insert(1, Arc::new(vec![]), 4096);
        assert_eq!(shard.len(), 0);
    }

    #[test]
    fn rotation_keeps_the_shard_under_budget() {
        let mut shard: Shard<Arc<Vec<SemanticEntity>>> = Shard::new(1000);
        for k in 0..100u128 {
            shard.insert(k, Arc::new(vec![]), 100);
            assert!(
                shard.bytes() <= 1000,
                "resident bytes {} exceeded budget",
                shard.bytes()
            );
        }
    }

    #[test]
    fn promotion_from_cold_survives_the_next_rotation() {
        let mut shard: Shard<Arc<Vec<SemanticEntity>>> = Shard::new(1000);
        shard.insert(7, Arc::new(vec![entity("keep")]), 100);
        // Fill hot until 7 is pushed into cold.
        for k in 100..106u128 {
            shard.insert(k, Arc::new(vec![]), 100);
        }
        assert!(shard.get(7).is_some(), "cold entry should still be found");
        assert!(shard.hot.contains_key(&7), "hit should promote into hot");
    }

    #[test]
    fn disk_tier_round_trips_entities() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdefu128;
        let entities = vec![entity("disk_a"), entity("disk_b")];

        assert!(disk_load(dir.path(), key).is_none(), "cold dir must miss");
        disk_store(&Cache::new(), dir.path(), key, &entities);

        let loaded = disk_load(dir.path(), key).expect("stored entry must load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "disk_a");
        assert_eq!(loaded[1].id, entities[1].id);
        // No scratch files left behind by the write-then-rename. Match on the
        // file name only — the tempdir's own name contains "tmp".
        let stragglers: Vec<_> = walk_files(dir.path())
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".tmp."))
            })
            .collect();
        assert!(stragglers.is_empty(), "left temp files: {stragglers:?}");
    }

    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_files(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn structural_hash_none_and_empty_round_trip_distinctly() {
        let none = get_or_structural_hash("test", "n.rs", "aaa", || None);
        assert_eq!(none, None);
        assert_eq!(
            get_or_structural_hash("test", "n.rs", "aaa", || panic!("cached")),
            None
        );

        let empty = get_or_structural_hash("test", "e.rs", "bbb", || Some(String::new()));
        assert_eq!(empty, Some(String::new()));
        assert_eq!(
            get_or_structural_hash("test", "e.rs", "bbb", || panic!("cached")),
            Some(String::new())
        );

        let real = get_or_structural_hash("test", "r.rs", "ccc", || Some("abc123".to_string()));
        assert_eq!(real, Some("abc123".to_string()));
        assert_eq!(
            get_or_structural_hash("test", "r.rs", "ccc", || panic!("cached")),
            Some("abc123".to_string())
        );
    }
}
