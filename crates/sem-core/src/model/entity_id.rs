//! Reference-counted, interned canonical entity identifiers.
//!
//! A handle occupies one pointer; equal live identifiers share one allocation.
//! Unlike a permanent global string arena, unused identifiers are reclaimed.
//! Ordering, borrowed lookup, JSON and SQLite use canonical text, never pointer
//! addresses: handles are deliberately not a persistent/versioned identity.

use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    borrow::{Borrow, Cow},
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, LazyLock, Mutex, Weak,
    },
};

// The primary map needs only a hash and a Weak pointer. Full text comparison
// below protects against hash collisions; a separate, normally empty overflow
// map avoids allocating a Vec for every canonical identifier.
#[derive(Default)]
struct Shard {
    primary: FxHashMap<u64, Weak<CanonicalId>>,
    collisions: FxHashMap<u64, Vec<Weak<CanonicalId>>>,
}

const SHARDS: usize = 64;
static POOL: LazyLock<[Mutex<Shard>; SHARDS]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(Shard::default())));
static LIVE: AtomicUsize = AtomicUsize::new(0);

struct CanonicalId {
    text: String,
    hash: u64,
}

fn shard(hash: u64) -> &'static Mutex<Shard> {
    &POOL[(hash >> 32) as usize & (SHARDS - 1)]
}

impl Drop for CanonicalId {
    fn drop(&mut self) {
        // Invoked by Arc exactly once, on the final strong reference. Removal
        // compares pointers: another thread may already have replaced an
        // expired Weak with a newly allocated instance of the same text.
        let mut pool = shard(self.hash).lock().unwrap_or_else(|e| e.into_inner());
        let is_primary = pool
            .primary
            .get(&self.hash)
            .is_some_and(|weak| std::ptr::eq(weak.as_ptr(), self));
        if is_primary {
            pool.primary.remove(&self.hash);
            if let Some(bucket) = pool.collisions.get_mut(&self.hash) {
                if let Some(next) = bucket.pop() {
                    pool.primary.insert(self.hash, next);
                }
            }
        } else if let Some(bucket) = pool.collisions.get_mut(&self.hash) {
            bucket.retain(|weak| !std::ptr::eq(weak.as_ptr(), self));
        }
        if pool.collisions.get(&self.hash).is_some_and(Vec::is_empty) {
            pool.collisions.remove(&self.hash);
        }
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

fn intern(value: Cow<'_, str>) -> EntityId {
    let mut hasher = FxHasher::default();
    value.as_ref().hash(&mut hasher);
    intern_hashed(value, hasher.finish())
}

fn intern_hashed(value: Cow<'_, str>, hash: u64) -> EntityId {
    // Keep upgraded collision candidates alive until AFTER releasing the
    // shard lock. Dropping the last Arc while holding that lock would recurse
    // into CanonicalId::drop and deadlock. The common path never allocates this
    // Vec, and a borrowed hit never allocates a String.
    let mut held = Vec::new();
    let mut pool = shard(hash).lock().unwrap_or_else(|e| e.into_inner());
    let primary = pool.primary.get(&hash).and_then(Weak::upgrade);
    let primary_alive = primary.is_some();
    if let Some(candidate) = primary {
        if candidate.text == value {
            drop(pool);
            return EntityId(candidate);
        }
        held.push(candidate);
    }
    if let Some(bucket) = pool.collisions.get(&hash) {
        for weak in bucket {
            if let Some(candidate) = weak.upgrade() {
                if candidate.text == value {
                    drop(pool);
                    drop(held);
                    return EntityId(candidate);
                }
                held.push(candidate);
            }
        }
    }
    let canonical = Arc::new(CanonicalId {
        text: value.into_owned(),
        hash,
    });
    LIVE.fetch_add(1, Ordering::Relaxed);
    if primary_alive {
        pool.collisions
            .entry(hash)
            .or_default()
            .push(Arc::downgrade(&canonical));
    } else {
        pool.primary.insert(hash, Arc::downgrade(&canonical));
    }
    drop(pool);
    drop(held);
    EntityId(canonical)
}

#[derive(Clone)]
pub struct EntityId(Arc<CanonicalId>);

impl EntityId {
    pub fn as_str(&self) -> &str {
        self.0.text.as_str()
    }
    pub(crate) fn as_string(&self) -> &String {
        &self.0.text
    }

    /// Number of canonical IDs currently retained across all live graphs and
    /// sessions in this process. Useful for profiling/reclamation diagnostics.
    pub fn live_count() -> usize {
        LIVE.load(Ordering::Relaxed)
    }
}

impl From<String> for EntityId {
    fn from(value: String) -> Self {
        intern(Cow::Owned(value))
    }
}
impl From<&str> for EntityId {
    fn from(value: &str) -> Self {
        intern(Cow::Borrowed(value))
    }
}
impl From<&String> for EntityId {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}
impl From<&EntityId> for EntityId {
    fn from(value: &EntityId) -> Self {
        value.clone()
    }
}
impl From<EntityId> for String {
    fn from(value: EntityId) -> Self {
        value.as_str().to_owned()
    }
}
impl Deref for EntityId {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl Borrow<str> for EntityId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl Borrow<String> for EntityId {
    fn borrow(&self) -> &String {
        &self.0.text
    }
}
impl AsRef<str> for EntityId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl PartialEq for EntityId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for EntityId {}
impl PartialEq<str> for EntityId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for EntityId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl PartialEq<String> for EntityId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<EntityId> for String {
    fn eq(&self, other: &EntityId) -> bool {
        self == other.as_str()
    }
}
impl PartialOrd for EntityId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for EntityId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}
impl Hash for EntityId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Must match Borrow<str>, not a pointer hash.
        self.as_str().hash(state);
    }
}
impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}
impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}
impl Serialize for EntityId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for EntityId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}
#[cfg(feature = "disk-cache")]
impl rusqlite::types::ToSql for EntityId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}
#[cfg(feature = "disk-cache")]
impl rusqlite::types::FromSql for EntityId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value.as_str().map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collisions_are_compared_by_text_and_survive_primary_removal() {
        let hash = 0x8b389ac033987da1;
        {
            let a = intern_hashed(Cow::Borrowed("collision/a"), hash);
            let b = intern_hashed(Cow::Borrowed("collision/b"), hash);
            let c = intern_hashed(Cow::Borrowed("collision/c"), hash);
            assert_ne!(a, b);
            assert_ne!(b, c);
            drop(a);
            assert_eq!(b, intern_hashed(Cow::Borrowed("collision/b"), hash));
            assert_eq!(c, intern_hashed(Cow::Borrowed("collision/c"), hash));
            let a = intern_hashed(Cow::Borrowed("collision/a"), hash);
            assert_ne!(a, b);
            drop(c);
            assert_eq!(a, intern_hashed(Cow::Borrowed("collision/a"), hash));
        }
        let pool = shard(hash).lock().unwrap();
        assert!(!pool.primary.contains_key(&hash));
        assert!(!pool.collisions.contains_key(&hash));
    }

    #[test]
    fn concurrent_collision_reinterning_and_final_drop_do_not_deadlock_or_leak() {
        let hash = 0x992338334abcdef1;
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..2000 {
                        let text = format!("racing/{}", i % 4);
                        let a = intern_hashed(Cow::Borrowed(&text), hash);
                        let b = intern_hashed(Cow::Borrowed(&text), hash);
                        assert_eq!(a, b);
                        assert_eq!(a.as_str(), text);
                        if i % 17 == 0 {
                            std::thread::yield_now();
                        }
                        drop(a);
                        assert_eq!(b.as_str(), text);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let pool = shard(hash).lock().unwrap();
        assert!(!pool.primary.contains_key(&hash));
        assert!(!pool.collisions.contains_key(&hash));
    }
}
