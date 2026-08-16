//! Red-green incremental resolution: per-file facts, resolve-time read sets, and
//! the invalidation rule that decides which files must be re-resolved after an
//! edit (semx-022).
//!
//! # The invariant this module exists to enforce
//!
//! A cached resolution result is valid only if the file's *own* facts **and**
//! everything its resolution actually read are unchanged (KAPPA.md's closure
//! reasoning: "kappa alone authorizes no hit"). Over-approximating the read set
//! is merely slow. Under-approximating it silently serves stale edges, which is
//! the one unforgivable failure — so every lookup that can reach another file's
//! data is recorded, and anything not recorded is handled by a whole-table guard
//! that forces a full re-resolve rather than a quiet reuse.
//!
//! # Shape
//!
//! * **Facts layer.** [`FileFacts`] is the content-addressed per-file bundle:
//!   content hash, extracted entities, and the tree-independent scope/ref facts
//!   (`PrecomputedFileFacts`) resolution needs. It is `serde`-serializable so the
//!   on-disk corpus bead (semx-9en) can persist it unchanged.
//! * **Read sets.** During resolution every file accumulates a [`ReadSet`]: the
//!   set of `(table, key)` pairs its resolution consulted, hashed to a `u64`.
//!   Recording happens at the lookup site, so a key is recorded whether the
//!   lookup hit or missed — a *miss* is a dependency too (a later edit that adds
//!   that key must invalidate the file).
//! * **Fingerprints.** After every build, each global table is fingerprinted as
//!   `key_hash -> value_hash` ([`TableFingerprints`]). Two builds' fingerprints
//!   diff to the exact set of changed keys.
//! * **Invalidation.** A file is RED if its own content changed, if it is new or
//!   deleted, or if its read set intersects the changed-key set. Because the
//!   global tables are rebuilt from *all* files' facts before the diff is taken,
//!   the changed-key set already reflects every edit transitively — no fixpoint
//!   iteration over file-to-file dependencies is needed, and the result is more
//!   precise than one (a file that reads a *different* key of a changed file
//!   stays GREEN).
//!
//! # Edge ownership invariant
//!
//! **An edge belongs to the file whose reference produced it.** Scope-resolution
//! edges are collected per file inside the pass-2 closure; bag-of-words edges are
//! collected per file inside the resolution closure. A GREEN file's edge list is
//! reused verbatim, in the same position in the merge order the cold build would
//! have produced it. A RED file's edge list is dropped whole and re-derived. No
//! edge is ever half-owned: nothing merges edges across files before the per-file
//! lists are captured.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::Xxh3;

use crate::model::entity::SemanticEntity;
use crate::parser::graph::RefType;

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// Every global (cross-file) structure resolution reads, tagged so one flat
/// fingerprint map can hold all of them without key collisions.
///
/// Adding a new cross-file table to the resolver **requires** adding it here and
/// recording its lookups; otherwise a change to it would not invalidate anyone.
/// The guard tables at the end exist for structures that are read through code
/// paths too diffuse to instrument key-by-key — they are fingerprinted whole and
/// any change forces every file RED.
// `SymbolTable` and `ClassMembers` name real structures in `PreBuiltLookups`;
// renaming them to satisfy `enum_variant_names` would make the mapping between
// a tag and the table it fingerprints harder to check, which is the one thing
// this enum has to make obvious.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Table {
    /// name -> [entity id] (`PreBuiltLookups::symbol_table`)
    SymbolTable = 1,
    /// owner type name -> [(member name, member id)] (`scope_class_members`)
    ClassMembers = 2,
    /// parent entity id -> [(member name, member id)] (`scope_owner_members`)
    OwnerMembers = 3,
    /// entity id -> `EntityInfo`
    EntityMap = 4,
    /// (class name, attribute name) -> type name
    InstanceAttrTypes = 5,
    /// function entity id -> return type name
    ReturnTypeMap = 6,
    /// function *name* -> return type name (deterministic per-name projection)
    FuncNameReturnTypes = 7,
    /// importing file path -> that file's whole import-table slice
    ImportsForFile = 8,
    /// go package name -> [(entity name, entity id)]
    GoPkgIndex = 11,
    /// bag-of-words: class name -> [(member name, member id)]
    BowClassMembers = 12,
    /// bag-of-words: (class name, file path) membership
    BowClassEntityFiles = 14,
    /// bag-of-words: (parent id, child id) membership
    BowParentChildPairs = 15,
    /// TS/JS export surface of one file, keyed by that file's own path:
    /// `(default export target id, sorted named exports, sorted top-level
    /// entities)` folded into one hash (`import_table_incremental`, semx-h1s).
    /// A default/namespace import elsewhere records a read of every path its
    /// module specifier could resolve to (hit or miss), so this file
    /// gaining, losing, or changing its default/named exports invalidates
    /// exactly the importers that could be affected — not the whole corpus.
    TsExportSurface = 16,
    /// Whole-table guard: swift call signatures (empty unless the repo has
    /// `.swift` sources, in which case any change forces a full re-resolve).
    GuardSwiftCallSignatures = 200,
    /// Whole-table guard: Python's bare `import module` / `import module as m`
    /// form (`register_namespace_import`, semx-kzy). That function scans *every*
    /// entry of `symbol_table`/`entity_map` looking for ones whose file matches
    /// the imported module — an unbounded read no `(table, key)` pair can name,
    /// because a symbol added anywhere in the corpus could start (or stop)
    /// matching. Recorded only by the one file whose AST actually contains this
    /// import form; every other file's read set never touches this key, so this
    /// guard costs nothing for files that don't use the pattern.
    GuardPyWildcardImport = 201,
}

impl Table {
    #[inline]
    fn tag(self) -> u8 {
        self as u8
    }

    /// Whether this table's contents are scoped to one resolution *chunk*
    /// rather than to the whole corpus.
    ///
    /// `resolve_with_scopes_full_inner` rebuilds the return-type and
    /// instance-attribute maps from only the files in the chunk it is currently
    /// resolving (see RESOLUTION-PROFILE.md's semx-6rd CUT-2 note: they are
    /// deliberately *not* hoisted, because hoisting them would change which
    /// functions' return types are visible across a chunk boundary). So the same
    /// key can legitimately hold different values in two different chunks, and a
    /// read set that did not distinguish them would compare a file's dependency
    /// against another chunk's answer. The chunk index is mixed into the key
    /// hash for these tables; corpora that never chunk use tag 0 throughout.
    #[inline]
    fn chunk_scoped(self) -> bool {
        matches!(
            self,
            Table::InstanceAttrTypes
                | Table::ReturnTypeMap
                | Table::FuncNameReturnTypes
                | Table::GuardSwiftCallSignatures
        )
    }
}

#[inline]
fn start(table: Table, scope_tag: u64) -> Xxh3 {
    let mut h = Xxh3::new();
    h.update(&[table.tag()]);
    if table.chunk_scoped() {
        h.update(&scope_tag.to_le_bytes());
    }
    h
}

/// Hash `(table, key…)` into the flat key space used by read sets and
/// fingerprints. The tag is mixed in first so two tables can never collide on a
/// shared key string.
#[inline]
pub(crate) fn key1(table: Table, scope_tag: u64, k: &str) -> u64 {
    let mut h = start(table, scope_tag);
    h.update(k.as_bytes());
    h.digest()
}

/// Two-part key (e.g. `(class name, attribute name)`).
#[inline]
pub(crate) fn key2(table: Table, scope_tag: u64, a: &str, b: &str) -> u64 {
    let mut h = start(table, scope_tag);
    h.update(a.as_bytes());
    h.update(&[0]);
    h.update(b.as_bytes());
    h.digest()
}

/// Whole-table guard key: one key standing for the entire table.
#[inline]
pub(crate) fn key_whole(table: Table, scope_tag: u64) -> u64 {
    let mut h = start(table, scope_tag);
    h.update(b"\x01whole-table-guard");
    h.digest()
}

// ---------------------------------------------------------------------------
// Read sets
// ---------------------------------------------------------------------------

/// The set of global-table keys one file's resolution consulted.
///
/// Recorded at the lookup site. Misses are recorded too: "key `k` was absent" is
/// as much a dependency as "key `k` held `v`", because a later edit that
/// *introduces* `k` changes the answer.
///
/// Cheap by construction — one `u64` per distinct key, no strings retained — so
/// holding one per file across a 40k-file corpus costs a few MiB.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadSet {
    keys: HashSet<u64>,
}

impl ReadSet {
    #[inline]
    pub(crate) fn record(&mut self, hashed_key: u64) {
        self.keys.insert(hashed_key);
    }

    /// Whether every key this file read still has the value it had last build.
    ///
    /// `prev` is the previous build's complete fingerprint map; `cur` is this
    /// build's, which must already cover every table this read set can name
    /// (the caller guarantees that by fingerprinting a stage's tables before
    /// evaluating that stage's read sets).
    ///
    /// A key absent from *both* maps was a miss last time and is still a miss:
    /// unchanged. A key present in exactly one is a genuine change — that is the
    /// case that catches "a new file introduced the symbol I failed to find".
    pub(crate) fn unchanged(&self, prev: &TableFingerprints, cur: &TableFingerprints) -> bool {
        self.keys.iter().all(|k| prev.get(*k) == cur.get(*k))
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// A read-set recorder handed to per-file resolution.
///
/// Disabled (`Recorder::off()`) on the cold, non-session build path, where every
/// call is one null check. The recorder is per file and never shared, so
/// parallel resolution needs no synchronization.
///
/// Recording is *unconditional at the site*: a lookup records its key before it
/// knows whether the lookup hit. That is deliberate — a miss is a dependency,
/// because a later edit that introduces the key changes the answer.
#[derive(Default)]
pub(crate) struct Recorder {
    set: Option<ReadSet>,
    scope_tag: u64,
}

impl Recorder {
    /// A recorder that discards everything. The cold path uses this so the call
    /// sites are identical in both modes and cannot drift apart.
    #[inline]
    pub(crate) fn off() -> Self {
        Self {
            set: None,
            scope_tag: 0,
        }
    }

    #[inline]
    pub(crate) fn on(scope_tag: u64) -> Self {
        Self {
            set: Some(ReadSet::default()),
            scope_tag,
        }
    }

    #[inline]
    pub(crate) fn enabled(&self) -> bool {
        self.set.is_some()
    }

    /// Record a lookup of `key` in `table`.
    #[inline]
    pub(crate) fn one(&mut self, table: Table, key: &str) {
        let tag = self.scope_tag;
        if let Some(s) = self.set.as_mut() {
            s.record(key1(table, tag, key));
        }
    }

    /// Record a lookup of the composite key `(a, b)` in `table`.
    #[inline]
    pub(crate) fn two(&mut self, table: Table, a: &str, b: &str) {
        let tag = self.scope_tag;
        if let Some(s) = self.set.as_mut() {
            s.record(key2(table, tag, a, b));
        }
    }

    /// Record a dependency on an entire whole-table guard (see e.g.
    /// [`Table::GuardSwiftCallSignatures`], [`Table::GuardPyWildcardImport`]):
    /// this file's result depends on the table's contents in a way too diffuse
    /// to name with individual keys, so any change to the guarded table at all
    /// must invalidate this file.
    #[inline]
    pub(crate) fn whole(&mut self, table: Table) {
        let tag = self.scope_tag;
        if let Some(s) = self.set.as_mut() {
            s.record(key_whole(table, tag));
        }
    }

    pub(crate) fn into_read_set(self) -> ReadSet {
        self.set.unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------------

/// `key_hash -> value_hash` for every entry of every instrumented global table.
///
/// Diffing two of these yields exactly the keys whose value changed, including
/// keys that appeared or disappeared (compared as `Option`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TableFingerprints {
    entries: HashMap<u64, u64>,
}

impl TableFingerprints {
    #[inline]
    pub(crate) fn put(&mut self, key: u64, value: u64) {
        self.entries.insert(key, value);
    }

    /// Drop a key entirely, so it reads back as "absent" rather than as its
    /// stale value. Used only by touched-key incremental fingerprinting
    /// (semx-4an): a table key that *disappeared* must fingerprint as absent,
    /// because [`ReadSet::unchanged`] compares `Option`s and a lingering stale
    /// value would keep a file GREEN whose lookup now misses.
    #[inline]
    pub(crate) fn remove(&mut self, key: u64) {
        self.entries.remove(&key);
    }

    #[inline]
    pub(crate) fn get(&self, key: u64) -> Option<u64> {
        self.entries.get(&key).copied()
    }

    /// Number of keys whose value differs between `self` (previous build) and
    /// `next`. Reported as a blast-radius statistic; invalidation itself is done
    /// per file against the two maps, never against a materialized diff.
    pub(crate) fn changed_key_count(&self, next: &TableFingerprints) -> usize {
        let mut changed = 0usize;
        for (k, v) in &next.entries {
            if self.entries.get(k) != Some(v) {
                changed += 1;
            }
        }
        for k in self.entries.keys() {
            if !next.entries.contains_key(k) {
                changed += 1;
            }
        }
        changed
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Accumulates fingerprints for one table without the caller having to spell out
/// the hashing convention at each site.
pub(crate) struct FingerprintSink<'a> {
    fp: &'a mut TableFingerprints,
    scope_tag: u64,
}

impl<'a> FingerprintSink<'a> {
    pub(crate) fn new(fp: &'a mut TableFingerprints, scope_tag: u64) -> Self {
        Self { fp, scope_tag }
    }

    #[inline]
    pub(crate) fn one(&mut self, table: Table, key: &str, value: u64) {
        self.fp.put(key1(table, self.scope_tag, key), value);
    }

    #[inline]
    pub(crate) fn two(&mut self, table: Table, a: &str, b: &str, value: u64) {
        self.fp.put(key2(table, self.scope_tag, a, b), value);
    }

    #[inline]
    pub(crate) fn whole(&mut self, table: Table, value: u64) {
        self.fp.put(key_whole(table, self.scope_tag), value);
    }
}

/// Hash a value into the fingerprint space. Values are hashed structurally (the
/// same bytes in the same order always hash the same) so an unchanged table
/// fingerprints identically across processes.
pub(crate) struct ValueHasher(Xxh3);

impl ValueHasher {
    #[inline]
    pub(crate) fn new() -> Self {
        Self(Xxh3::new())
    }

    #[inline]
    pub(crate) fn s(&mut self, v: &str) -> &mut Self {
        self.0.update(v.as_bytes());
        self.0.update(&[0]);
        self
    }

    #[inline]
    pub(crate) fn u(&mut self, v: usize) -> &mut Self {
        self.0.update(&(v as u64).to_le_bytes());
        self
    }

    #[inline]
    pub(crate) fn finish(&self) -> u64 {
        self.0.digest()
    }
}

/// Hash arbitrary bytes to a value fingerprint.
pub(crate) fn hash_str(v: &str) -> u64 {
    let mut h = Xxh3::new();
    h.update(v.as_bytes());
    h.digest()
}

// ---------------------------------------------------------------------------
// Per-file facts
// ---------------------------------------------------------------------------

/// Content hash of a file, the primary key of the facts layer.
pub fn content_hash(content: &str) -> u64 {
    content_hash_bytes(content.as_bytes())
}

/// [`content_hash`] over raw bytes, for a caller holding a file it has not
/// (yet) proven is UTF-8 — `sem-cli`'s single corpus read
/// (`SINGLE-PASS.md` §3), which must fingerprint every readable file for
/// `cache.db` while only UTF-8 files reach the index.
///
/// This is the same xxh3-64 `utils::hash::content_hash_bytes` renders as hex
/// for `cache.db`'s `files.content_hash` column: one number, two encodings
/// (`SINGLE-PASS.md` §1.3), not two hashes.
pub fn content_hash_bytes(content: &[u8]) -> u64 {
    let mut h = Xxh3::new();
    h.update(content);
    h.digest()
}

/// One file's cached resolution inputs and outputs.
///
/// Content-hash keyed: the same bytes at the same path always produce the same
/// facts, exactly like `parser::cache`'s extraction cache (whose keying
/// discipline this follows). `serde`-serializable so semx-9en can persist a
/// corpus of these without changing the shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFacts {
    pub path: String,
    pub content_hash: u64,
    /// Entities extracted from this file, in extraction order.
    pub entities: Vec<SemanticEntity>,
}

/// One file's scope-resolution output, reusable verbatim while the file is
/// GREEN.
///
/// See the edge-ownership invariant in this module's docs: these edges belong to
/// this file because this file's references produced them. `consumed_words` is
/// part of the output, not a side table — bag-of-words resolution reads it back
/// for the same entities, so reusing edges without reusing the words it took to
/// produce them would change the next stage's answer.
///
/// `serde`-serializable so `facts_store` (semx-9en) can persist a warm session's
/// per-file resolution outputs across a process restart, not just within one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CachedScopeResult {
    pub(crate) edges: Vec<(String, String, RefType)>,
    pub(crate) consumed_words: HashMap<String, HashSet<String>>,
    pub(crate) read_set: ReadSet,
}

/// One file's bag-of-words resolution output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CachedBowResult {
    pub(crate) edges: Vec<(String, String, RefType)>,
    pub(crate) read_set: ReadSet,
}

/// Everything the red-green cache holds for one file's *resolution* (as opposed
/// to its [`FileFacts`], which are its resolution *inputs*).
///
/// The two `_gen` stamps are the bookkeeping that lets one map serve as both the
/// previous build's cache and this build's (see [`Incremental::cache`]): a half
/// is *current* only if this build either rewrote it or re-validated it, and
/// [`Incremental::finish`] discards everything else. They are deliberately not
/// serialized — a snapshot loaded in a fresh process must earn its stamps from
/// that process's own first build, exactly like an entry the previous build
/// never touched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CachedFileResolution {
    pub(crate) scope: CachedScopeResult,
    pub(crate) bow: CachedBowResult,
    #[serde(skip)]
    scope_gen: u64,
    #[serde(skip)]
    bow_gen: u64,
    /// Whether the matching half was *reused* rather than rewritten, for the
    /// build named by its stamp. Meaningless — and cleared by
    /// [`Incremental::finish`] — for a half whose stamp is not current.
    #[serde(skip)]
    pub(crate) scope_reused: bool,
    #[serde(skip)]
    pub(crate) bow_reused: bool,
}

// ---------------------------------------------------------------------------
// Rebuild statistics
// ---------------------------------------------------------------------------

/// Blast-radius counters for one warm rebuild. Asserted on in tests: a leaf-file
/// touch must keep the RED set tiny, and a touch to a widely-imported file must
/// turn its true dependent set RED.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RebuildStats {
    /// Files whose own content changed (or that are new).
    pub files_seed_red: usize,
    /// Files RED in total, including those invalidated through the read set.
    pub files_red: usize,
    /// Files whose cached scope resolution was reused verbatim.
    pub files_green: usize,
    /// Files whose cached bag-of-words resolution was reused verbatim.
    pub files_green_bow: usize,
    /// Files removed from the corpus since the previous build.
    pub files_deleted: usize,
    /// Scope+bag-of-words edges served from the per-file cache.
    pub edges_reused: usize,
    /// Scope+bag-of-words edges recomputed for RED files.
    pub edges_rederived: usize,
    /// Global-table keys whose value changed between the two builds.
    pub changed_keys: usize,
    /// Wall time of the whole rebuild, milliseconds.
    pub rebuild_ms: f64,
}

// ---------------------------------------------------------------------------
// Session state shared with the build core
// ---------------------------------------------------------------------------

/// The mutable state a warm rebuild threads through `EntityGraph`'s build core.
///
/// Split out from [`crate::parser::session::GraphSession`] so the build core can
/// take a single `Option<&mut Incremental>` rather than a dozen parameters, and
/// so the cold path pays one `Option` check per stage.
///
/// **Reuse eligibility.** Files whose *detected* language is JS/TS, Python, Go,
/// or Rust may go GREEN (semx-kzy extended this past JS/TS-only, semx-022's
/// original scope — see [`crate::parser::import_resolution::
/// is_reuse_eligible_file`]). Every other language's resolution reaches
/// cross-file data through paths this crate does not yet attribute per file
/// (Swift call signatures, or a generic-table read never exercised by a
/// dedicated fixture). Rather than guess at their read sets, those files are
/// held permanently RED: slower, never wrong. See RESOLUTION-PROFILE.md's
/// "Universal GREEN eligibility" section for the full per-language verdict.
///
/// # Ownership: the cache is *moved* through a build, not copied
///
/// There is one per-file cache map, not a `prev` and a `next` (semx-4an). The
/// session hands its map over at the start of a build and takes it back at the
/// end; a GREEN file's entry is therefore **already** in the right place holding
/// exactly the right value, and reuse costs no write at all. Before this, a
/// GREEN file's edges were deep-cloned out of `prev` and deep-cloned back into
/// `next` — 39,058 files' worth of both on every warm rebuild of the TypeScript
/// monster — and then the whole of `prev` was freed.
///
/// What that move costs is an invariant that used to hold for free. When `next`
/// started empty, "there is an entry for `p`" meant "this build resolved `p`",
/// so a read set could only ever be compared against the fingerprints of the
/// build that *immediately* preceded it. A moved map can carry an entry across a
/// build that never looked at it (the file was deleted, or its per-file closure
/// produced nothing), and comparing that entry's read set against the next two
/// builds' fingerprints would silently miss a change that happened in between —
/// the stale-edge failure this module exists to prevent.
///
/// [`CachedFileResolution`]'s two generation stamps restore the invariant
/// directly: every stage stamps the halves it wrote or re-validated, and
/// [`finish`](Self::finish) drops every entry no half of this build claimed and
/// resets every half it did not. The map that comes out is entry-for-entry what
/// the freshly allocated `next` used to be.
pub(crate) struct Incremental<'a> {
    /// Files that must be re-resolved no matter what the read sets say: changed
    /// content, newly added, or not eligible for reuse at all.
    pub(crate) forced_red: &'a HashSet<String>,
    /// Previous build's global-table fingerprints.
    pub(crate) prev_fp: &'a TableFingerprints,
    /// Whether reuse is permitted at all. `false` on the first build (nothing to
    /// reuse) and whenever a coarse guard fired, in which case this run is a
    /// plain cold build that happens to populate the cache.
    pub(crate) reuse: bool,
    /// This build's fingerprints, accumulated stage by stage.
    pub(crate) cur_fp: TableFingerprints,
    /// Per-file resolution outputs: the previous build's on the way in, this
    /// build's on the way out. Private, because every read and every write has
    /// to go through the generation discipline described above.
    cache: HashMap<String, CachedFileResolution>,
    /// This build's stamp. Monotonic per session; never 0, so a `serde`-loaded
    /// entry (whose stamps default to 0) is never mistaken for a current one.
    generation: u64,
    /// How many files reused their cached scope / bag-of-words result. Counted
    /// rather than collected: which files those were is already recorded in the
    /// cache entries themselves (`scope_reused`/`bow_reused`), and materializing
    /// a `HashSet<String>` of them meant ~39k path allocations per build for a
    /// set whose only two consumers are a count and a membership test.
    green_scope: usize,
    green_bow: usize,
}

impl<'a> Incremental<'a> {
    pub(crate) fn new(
        forced_red: &'a HashSet<String>,
        cache: HashMap<String, CachedFileResolution>,
        prev_fp: &'a TableFingerprints,
        reuse: bool,
        generation: u64,
    ) -> Self {
        debug_assert!(generation > 0, "generation 0 marks a never-stamped entry");
        Self {
            forced_red,
            prev_fp,
            reuse: reuse && !prev_fp.is_empty(),
            cur_fp: TableFingerprints::default(),
            cache,
            generation,
            green_scope: 0,
            green_bow: 0,
        }
    }

    /// The previous build's cached resolution for `file_path`, if it has one.
    ///
    /// Every stage reads this *before* writing anything of its own for the same
    /// file, so what it returns is always the previous build's value — see
    /// [`keep_scope`](Self::keep_scope) for the one place that ordering is
    /// load-bearing.
    #[inline]
    pub(crate) fn cached(&self, file_path: &str) -> Option<&CachedFileResolution> {
        self.cache.get(file_path)
    }

    /// Record that `file_path`'s cached *scope* result was re-validated this
    /// build and must survive [`finish`](Self::finish) untouched. This is the
    /// whole GREEN write path: no edges, no words, no read set — the entry
    /// already holds them. If it somehow does not, the file simply loses its
    /// cache entry in [`finish`](Self::finish) and is RED next build: this can
    /// fail closed, never open.
    pub(crate) fn keep_scope(&mut self, file_path: &str) {
        if let Some(entry) = self.cache.get_mut(file_path) {
            entry.scope_gen = self.generation;
            entry.scope_reused = true;
            self.green_scope += 1;
        }
    }

    /// Store a freshly resolved scope result for `file_path`.
    pub(crate) fn put_scope(&mut self, file_path: &str, scope: CachedScopeResult) {
        let generation = self.generation;
        let entry = self.entry_mut(file_path);
        entry.scope = scope;
        entry.scope_gen = generation;
        entry.scope_reused = false;
    }

    /// [`keep_scope`](Self::keep_scope)'s bag-of-words half.
    pub(crate) fn keep_bow(&mut self, file_path: &str) {
        if let Some(entry) = self.cache.get_mut(file_path) {
            entry.bow_gen = self.generation;
            entry.bow_reused = true;
            self.green_bow += 1;
        }
    }

    /// [`put_scope`](Self::put_scope)'s bag-of-words half.
    pub(crate) fn put_bow(&mut self, file_path: &str, bow: CachedBowResult) {
        let generation = self.generation;
        let entry = self.entry_mut(file_path);
        entry.bow = bow;
        entry.bow_gen = generation;
        entry.bow_reused = false;
    }

    /// Whether `file_path`'s *scope* result was reused earlier in **this** build.
    ///
    /// This is bag-of-words' eligibility rule: it reads back the words scope
    /// resolution consumed for the very same entities, so mixing a fresh scope
    /// result with a stale bag-of-words result would answer a question neither
    /// pass was asked. The generation check is what makes "this build" mean this
    /// build and not some earlier one.
    pub(crate) fn scope_reused_this_build(&self, file_path: &str) -> bool {
        self.cache
            .get(file_path)
            .is_some_and(|entry| entry.scope_reused && entry.scope_gen == self.generation)
    }

    pub(crate) fn green_scope_count(&self) -> usize {
        self.green_scope
    }

    pub(crate) fn green_bow_count(&self) -> usize {
        self.green_bow
    }

    fn entry_mut(&mut self, file_path: &str) -> &mut CachedFileResolution {
        // One hash of the path on the hit path (every warm rebuild's common
        // case); the `to_string` is paid only when the file is genuinely new to
        // the cache.
        if !self.cache.contains_key(file_path) {
            self.cache
                .insert(file_path.to_string(), CachedFileResolution::default());
        }
        self.cache
            .get_mut(file_path)
            .expect("just inserted if absent")
    }

    /// Reduce the moved-through cache to exactly what a freshly allocated `next`
    /// would have held: an entry per file some stage of *this* build produced a
    /// result for, with any half this build did not produce reset to its
    /// default. Called once, after the last stage.
    ///
    /// Dropping an unclaimed entry is the safety half (its read set was recorded
    /// against fingerprints two builds old); resetting an unclaimed half is the
    /// parity half (a file whose scope resolved but whose bag-of-words produced
    /// nothing used to get a default `bow` from `entry().or_default()`, and the
    /// next build's reuse decision reads it).
    pub(crate) fn finish(&mut self) {
        let generation = self.generation;
        self.cache.retain(|_, entry| {
            let scope_current = entry.scope_gen == generation;
            let bow_current = entry.bow_gen == generation;
            if !scope_current && !bow_current {
                return false;
            }
            if !scope_current {
                entry.scope = CachedScopeResult::default();
                entry.scope_reused = false;
            }
            if !bow_current {
                entry.bow = CachedBowResult::default();
                entry.bow_reused = false;
            }
            true
        });
    }

    /// This build's per-file resolution outputs, to hand back to the session.
    /// Only meaningful after [`finish`](Self::finish).
    pub(crate) fn take_cache(&mut self) -> HashMap<String, CachedFileResolution> {
        std::mem::take(&mut self.cache)
    }

    /// This build's per-file resolution outputs, for the caller's statistics.
    /// Only meaningful after [`finish`](Self::finish).
    pub(crate) fn entries(&self) -> impl Iterator<Item = &CachedFileResolution> {
        self.cache.values()
    }

    /// Whether `file_path` may reuse its cached result for the stage whose read
    /// set is `read_set`.
    ///
    /// This is the whole invalidation rule in one place: forced-RED loses, a file
    /// with no cached entry loses, and otherwise every key the file read must
    /// still hold the value it held last build.
    pub(crate) fn may_reuse(&self, file_path: &str, read_set: &ReadSet) -> bool {
        self.reuse
            && !self.forced_red.contains(file_path)
            && read_set.unchanged(self.prev_fp, &self.cur_fp)
    }
}
