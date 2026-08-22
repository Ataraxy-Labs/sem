//! Zero-copy reads over an index image. See `QUERY-INDEX.md` §5.
//!
//! Nothing here allocates per entity. `lookup` returns a range of indices and
//! the accessors hand back `&str` borrowed straight from the mapping, so the
//! cost of a query is the cost of the pages the answer touches — which is the
//! answer-cost invariant expressed in the type rather than in a comment.

use std::borrow::Cow;
use std::path::Path;

use super::format::{self, *};

/// Normalizes a caller-supplied path/prefix key to the form `FILES` always
/// stores it in — repo-relative, forward-slash-separated (every writer call
/// site normalizes before an entry ever reaches this image; see
/// `crate::index::writer`). The key-comparing reader methods
/// (`files_under`, `entities_in_file`, `file_fingerprint`) route through
/// this so a caller that hands over an OS-native (backslash) path — which
/// happens on Windows whenever a `Path::to_string_lossy()` result reaches
/// this API without its own separator conversion — still gets a correct
/// match instead of a silent miss (semx-q344).
fn normalize_index_key(key: &str) -> Cow<'_, str> {
    if key.contains('\\') {
        Cow::Owned(key.replace('\\', "/"))
    } else {
        Cow::Borrowed(key)
    }
}

/// Byte length of the `REFS` section's sub-header (`fwd_edge_count` +
/// `rev_edge_count`, each `u32`). See `writer::build_refs_section`.
const REFS_SUB_HEADER_LEN: usize = 8;

enum Backing {
    Owned(Vec<u8>),
    #[cfg(all(not(target_arch = "wasm32"), feature = "mmap"))]
    Mapped(memmap2::Mmap),
}

impl Backing {
    fn bytes(&self) -> &[u8] {
        match self {
            Backing::Owned(bytes) => bytes,
            #[cfg(all(not(target_arch = "wasm32"), feature = "mmap"))]
            Backing::Mapped(map) => map,
        }
    }
}

pub struct QueryIndex {
    backing: Backing,
    header: Header,
}

/// The result of probing `TRIGRAM` for one 3-byte trigram. See the
/// `format::trigram` module doc for why `Absent` and `Stopped` must never be
/// conflated: one is a correctness signal (definitely zero files), the other
/// is a selectivity signal (no information, never "zero").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrigramPosting {
    /// This trigram occurs in zero files in the corpus. Safe to use as a
    /// hard "no candidates" answer for any pattern requiring it.
    Absent,
    /// Too common to store a posting list for (budget), or the tier is
    /// absent entirely. Provides no filtering information.
    Stopped,
    /// Ascending, deduped file indices whose content contains this trigram.
    Present(Vec<u32>),
}

impl QueryIndex {
    /// `mmap` the image. `None` — never an error — for absent, truncated,
    /// stale-salt, or garbage files: the query plane is strictly optional and
    /// a miss just means the caller uses the build plane (§3.5).
    #[cfg(all(not(target_arch = "wasm32"), feature = "mmap"))]
    pub fn open(path: &Path) -> Option<QueryIndex> {
        Self::open_with_salt(path, crate::parser::facts_store::corpus_identity_salt())
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "mmap"))]
    pub fn open_with_salt(path: &Path, salt: u64) -> Option<QueryIndex> {
        let file = std::fs::File::open(path).ok()?;
        // SAFETY: the image is replaced only by rename (§3.6), so this inode
        // is immutable for the lifetime of the mapping — a later writer
        // creates a new inode and leaves ours intact until it is dropped.
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        Self::new(Backing::Mapped(map), salt)
    }

    /// The dependency-free constructor: the whole reader is defined over
    /// `&[u8]`, so the format is testable without a filesystem and stays
    /// available on `wasm32` where `mmap` does not exist.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<QueryIndex> {
        Self::from_bytes_with_salt(bytes, crate::parser::facts_store::corpus_identity_salt())
    }

    pub fn from_bytes_with_salt(bytes: Vec<u8>, salt: u64) -> Option<QueryIndex> {
        Self::new(Backing::Owned(bytes), salt)
    }

    fn new(backing: Backing, salt: u64) -> Option<QueryIndex> {
        let header = Header::read(backing.bytes(), salt)?;
        let index = QueryIndex { backing, header };
        // Sections must be whole multiples of their record size, or a later
        // index computation could read across a boundary. Checked once here
        // so the accessors below never have to.
        let counts_agree = index.section(SEC_ENTITIES).len() as u64
            == index.header.entity_count * ENTITY_REC_LEN as u64
            && index.section(SEC_NAMES).len() as u64
                == index.header.entity_count * NAME_REC_LEN as u64
            && index.section(SEC_FILES).len() as u64
                == index.header.file_count * FILE_REC_LEN as u64
            && index.section(SEC_KINDS).len() as u64
                == index.header.kind_count as u64 * KIND_REC_LEN as u64
            // DIRS is either absent (zero-length — "Complete unavailable on
            // this image", semx-ykf) or a well-formed array of exactly
            // dir_count records — same "tier absent or exactly right, never
            // torn" discipline REFS/TRIGRAM use.
            && (index.section(SEC_DIRS).is_empty()
                || index.section(SEC_DIRS).len() as u64
                    == index.header.dir_count * DIR_REC_LEN as u64);
        counts_agree
            .then_some(index)
            .filter(QueryIndex::refs_section_is_sound)
            .filter(QueryIndex::trigram_section_is_sound)
    }

    /// `REFS` is either absent (zero-length — "tier not built yet", §9) or a
    /// well-formed CSR whose declared edge counts account for every byte in
    /// the section. A torn or truncated REFS section is a clean miss on the
    /// whole image, same discipline as `counts_agree` above, not a panic the
    /// first time a caller reaches for `callers_of`/`refs_of`.
    fn refs_section_is_sound(&self) -> bool {
        let refs = self.section(SEC_REFS);
        if refs.is_empty() {
            return true;
        }
        let n = self.header.entity_count as usize;
        let Some(fwd) = format::u32_at(refs, 0) else {
            return false;
        };
        let Some(rev) = format::u32_at(refs, 4) else {
            return false;
        };
        let expected = 8usize
            .saturating_add((n + 1).saturating_mul(4).saturating_mul(2))
            .saturating_add(
                (fwd as usize)
                    .saturating_add(rev as usize)
                    .saturating_mul(4),
            );
        refs.len() == expected
    }

    /// `false` until S2 lands a real image (skeleton images write REFS
    /// zero-length, §9's "tier absent" contract).
    pub fn has_refs(&self) -> bool {
        !self.header.sections[SEC_REFS].is_absent()
    }

    /// `false` until a build supplies file content (skeleton images and any
    /// image built via `build` write `TRIGRAM` zero-length — S3's "tier
    /// absent" contract, same shape as `has_refs`).
    /// `grep::search` checks this first and goes straight to a full scan
    /// rather than probing per-trigram against an empty tier.
    pub fn has_trigram(&self) -> bool {
        !self.header.sections[SEC_TRIGRAM].is_absent()
    }

    /// `TRIGRAM` is either absent (zero-length) or a well-formed CSR whose
    /// declared `posting_total` accounts for every byte in the section —
    /// same discipline as `refs_section_is_sound`, so a torn or truncated
    /// trigram tier is a clean miss on the whole image, never a panic the
    /// first time `grep` reaches for a posting.
    fn trigram_section_is_sound(&self) -> bool {
        let section = self.section(SEC_TRIGRAM);
        if section.is_empty() {
            return true;
        }
        let Some((count, total)) = format::trigram::header(section) else {
            return false;
        };
        section.len() == format::trigram::expected_len(count, total)
    }

    /// The result of probing one trigram's posting row. `Absent` and
    /// `Stopped` are deliberately distinct — see the `format::trigram`
    /// module doc for why conflating them would be a correctness bug, not
    /// just an imprecision.
    pub fn trigram_posting(&self, trigram: [u8; 3]) -> TrigramPosting {
        let section = self.section(SEC_TRIGRAM);
        if section.is_empty() {
            // Tier not built at all: this is neither "zero files" nor "known
            // to be too common" — it is "no information". Callers must treat
            // it exactly like `Stopped` (no filtering), which `Stopped`'s
            // contract already guarantees, so reusing it here needs no third
            // variant. `grep::search` short-circuits on `has_trigram()`
            // before ever reaching this path in practice.
            return TrigramPosting::Stopped;
        }
        let Some((count, _total)) = format::trigram::header(section) else {
            return TrigramPosting::Stopped;
        };
        let value = format::trigram::pack(trigram);
        let lo = partition_point(count, |slot| {
            format::trigram::key(section, slot)
                .map(format::trigram::value_of)
                .unwrap_or(u32::MAX)
                < value
        });
        let Some(entry) = (lo < count)
            .then(|| format::trigram::key(section, lo))
            .flatten()
        else {
            return TrigramPosting::Absent;
        };
        if format::trigram::value_of(entry) != value {
            return TrigramPosting::Absent;
        }
        if format::trigram::is_stop(entry) {
            return TrigramPosting::Stopped;
        }
        let Some((row_lo, row_hi)) = format::trigram::row(section, count, lo) else {
            return TrigramPosting::Stopped;
        };
        let files: Vec<u32> = (row_lo..row_hi)
            .filter_map(|i| format::trigram::target(section, count, i))
            .collect();
        TrigramPosting::Present(files)
    }

    /// Every file the index knows about, in `FILES` order (ascending file
    /// index) — the candidate set for a pattern whose trigrams gave no
    /// usable prefilter (`grep`'s documented worst case, rg-equivalent).
    pub fn all_file_paths(&self) -> Vec<&str> {
        (0..self.header.file_count as u32)
            .map(|i| self.file_path(i))
            .collect()
    }

    /// Every file under `prefix`, a root-relative directory path already
    /// normalized to end in `/` (or `""` for the whole repo) — the
    /// query-path file-discovery reroute, `QUERY-INDEX.md` §7 item 1.
    /// `FILES` is sorted by path (§3.2, the same key-table property
    /// `entities_in_file`/`file_fingerprint` already rely on), so every file
    /// under a directory is one contiguous slot range: a `partition_point`
    /// for the range start, then a linear `starts_with` scan to its end. No
    /// separate directory index is needed, which is why this lands without a
    /// `DIRS` section.
    ///
    /// **`Verified`-level only** (§2's freshness split): this proves the
    /// returned set is exactly what the index's own `FILES` table contains
    /// under `prefix` — it does **not** prove no file was added under
    /// `prefix` since the index was last built (that is `Complete`'s job,
    /// which needs the still-reserved `DIRS` section, §9, not built by this
    /// capability). A file created after the index's last build is invisible
    /// here until the next full rebuild repairs the image — the same
    /// membership tradeoff `sem find`/`callers`/`refs` (S2) already ship
    /// with for a name that exists only in a brand-new file. Callers that
    /// need corpus-wide membership freshness must not use this path.
    pub fn files_under(&self, prefix: &str) -> Vec<&str> {
        let prefix = normalize_index_key(prefix);
        let files = self.section(SEC_FILES);
        let count = self.header.file_count as usize;
        if prefix.is_empty() {
            return (0..count as u32).map(|i| self.file_path(i)).collect();
        }
        let key = prefix.as_bytes();
        let lo = partition_point(count, |slot| self.file_path_bytes(files, slot) < key);
        let mut out = Vec::new();
        let mut slot = lo;
        while slot < count {
            let path = self.file_path(slot as u32);
            if !path.as_bytes().starts_with(key) {
                break;
            }
            out.push(path);
            slot += 1;
        }
        out
    }

    /// Direct dependencies of entity `index` — what it calls/references.
    /// Forward CSR row; empty (not a panic) when the tier is absent or the
    /// index is out of range. Each posting's packed kind bits (`format::
    /// refs::kind_of`) are discarded here — use [`Self::refs_of_typed`] for
    /// `ref_type`.
    pub fn refs_of(&self, index: usize) -> Vec<Entity<'_>> {
        self.csr_row(index, true)
    }

    /// Direct callers of entity `index` — who calls/references it. Reverse
    /// CSR row, `EntityGraph::dependents`' vocabulary carried into the image.
    pub fn callers_of(&self, index: usize) -> Vec<Entity<'_>> {
        self.csr_row(index, false)
    }

    /// Same as [`Self::refs_of`], but pairs each target with its `ref_type`
    /// (semx-zvq). If [`Self::refs_are_typed`] is `false` (the
    /// `format::refs::MAX_ENTITIES`-overflow fallback), every kind reads
    /// `RefType::Calls` regardless of the edge's real kind — callers that
    /// need typed edges must check `refs_are_typed` first, exactly like
    /// `has_refs`/`has_trigram` gate the untyped tiers.
    pub fn refs_of_typed(&self, index: usize) -> Vec<(Entity<'_>, crate::parser::graph::RefType)> {
        self.csr_row_typed(index, true)
    }

    /// Same as [`Self::callers_of`], typed. See [`Self::refs_of_typed`].
    pub fn callers_of_typed(
        &self,
        index: usize,
    ) -> Vec<(Entity<'_>, crate::parser::graph::RefType)> {
        self.csr_row_typed(index, false)
    }

    /// Whether `REFS`' packed kind bits are trustworthy on this image. `true`
    /// on every image built today (no measured corpus is within three orders
    /// of magnitude of `format::refs::MAX_ENTITIES`, see that constant's
    /// doc); `false` only for the fail-safe overflow fallback, or for any
    /// image built before semx-zvq — which `format_version`'s bump to `2`
    /// already makes unreadable by this code at all (`Header::read` rejects
    /// it), so in practice this only ever reads `false` for the overflow
    /// case, never a stale-format one.
    pub fn refs_are_typed(&self) -> bool {
        self.header.flags & FLAG_REFS_TYPED != 0
    }

    /// Whether [`Entity::is_test`] is an answer on this image rather than an
    /// uncomputed zero (semx-zvq). `false` on any image whose writer had no
    /// classification to hand in — `commands::query`'s cold-build write, or
    /// any pre-semx-zvq image — and a caller that needs test-shaped answers
    /// must check this first, exactly as `has_refs`/`has_trigram`/
    /// `refs_are_typed` gate their own tiers. This is the index's analogue of
    /// the SQLite cache's `test_flags_computed` metadata key, and it exists
    /// for the same reason that key does: an EMPTY test set is only
    /// trustworthy if somebody actually looked.
    pub fn has_test_flags(&self) -> bool {
        self.header.flags & FLAG_ENTITY_TESTS != 0
    }

    fn csr_row(&self, at: usize, forward: bool) -> Vec<Entity<'_>> {
        self.csr_row_raw(at, forward)
            .into_iter()
            .map(|packed| self.entity(format::refs::target_of(packed) as usize))
            .collect()
    }

    fn csr_row_typed(
        &self,
        at: usize,
        forward: bool,
    ) -> Vec<(Entity<'_>, crate::parser::graph::RefType)> {
        self.csr_row_raw(at, forward)
            .into_iter()
            .map(|packed| {
                let entity = self.entity(format::refs::target_of(packed) as usize);
                let kind = ref_kind_from_u8(format::refs::kind_of(packed));
                (entity, kind)
            })
            .collect()
    }

    /// The raw packed `u32` postings for one CSR row — `format::refs::pack`ed
    /// (target index in the low 28 bits, `ref_type` in the high 4), unpacked
    /// by `csr_row`/`csr_row_typed` above depending on whether the caller
    /// wants the kind.
    fn csr_row_raw(&self, at: usize, forward: bool) -> Vec<u32> {
        let section = self.section(SEC_REFS);
        let n = self.header.entity_count as usize;
        if section.is_empty() || at >= n {
            return Vec::new();
        }
        let fwd_edges = format::u32_at(section, 0).unwrap_or(0) as usize;
        let fwd_offsets_at = REFS_SUB_HEADER_LEN;
        let fwd_targets_at = fwd_offsets_at + (n + 1) * 4;
        let rev_offsets_at = fwd_targets_at + fwd_edges * 4;
        let rev_targets_at = rev_offsets_at + (n + 1) * 4;
        let (offsets_at, targets_at) = if forward {
            (fwd_offsets_at, fwd_targets_at)
        } else {
            (rev_offsets_at, rev_targets_at)
        };
        let lo = format::u32_at(section, offsets_at + at * 4).unwrap_or(0) as usize;
        let hi = format::u32_at(section, offsets_at + (at + 1) * 4).unwrap_or(0) as usize;
        (lo..hi)
            .filter_map(|slot| format::u32_at(section, targets_at + slot * 4))
            .collect()
    }

    pub fn entity_count(&self) -> usize {
        self.header.entity_count as usize
    }

    /// Total forward postings in `REFS` — the corpus's edge count, read from
    /// the section's sub-header rather than by summing rows (semx-zvq, for
    /// `sem graph`'s `stats.edgeCount` and its counts-only terminal output).
    /// `0` when the tier is absent, the same "tier absent reads as nothing"
    /// contract `refs_of` uses.
    pub fn edge_count(&self) -> usize {
        let section = self.section(SEC_REFS);
        if section.is_empty() {
            return 0;
        }
        format::u32_at(section, 0).unwrap_or(0) as usize
    }

    pub fn file_count(&self) -> usize {
        self.header.file_count as usize
    }

    /// Every entity carrying `name`, as a contiguous index range. This is the
    /// whole point of the sorted `NAMES` table: the answer's shape *is* the
    /// storage's shape, so producing it costs a binary search and nothing
    /// else. Names repeat ~8× on average in real corpora.
    pub fn lookup(&self, name: &str) -> Vec<Entity<'_>> {
        let names = self.section(SEC_NAMES);
        let count = self.header.entity_count as usize;
        let key = name.as_bytes();

        let lo = partition_point(count, |slot| self.name_at(names, slot) < key);
        let mut out = Vec::new();
        let mut slot = lo;
        while slot < count && self.name_at(names, slot) == key {
            out.push(self.entity(self.name_order(names, slot) as usize));
            slot += 1;
        }
        out
    }

    /// Entities of one file, as a slice of the entity table — no search, by
    /// construction (`FileRec.entity_lo..entity_hi`, §3.2).
    pub fn entities_in_file(&self, path: &str) -> Vec<Entity<'_>> {
        let path = normalize_index_key(path);
        let files = self.section(SEC_FILES);
        let count = self.header.file_count as usize;
        let key = path.as_bytes();
        let at = partition_point(count, |slot| self.file_path_bytes(files, slot) < key);
        if at >= count || self.file_path_bytes(files, at) != key {
            return Vec::new();
        }
        let rec = &files[at * FILE_REC_LEN..(at + 1) * FILE_REC_LEN];
        let lo = format::file::entity_lo(rec).unwrap_or(0) as usize;
        let hi = format::file::entity_hi(rec).unwrap_or(0) as usize;
        (lo..hi).map(|i| self.entity(i)).collect()
    }

    pub fn entity(&self, index: usize) -> Entity<'_> {
        Entity { index, owner: self }
    }

    /// The stored freshness fingerprint for `path` — `None` if the index has
    /// never seen this file. This is what Verified freshness (§5.1) stats
    /// against: a caller compares this against the file's *current* mtime,
    /// falling through to content-hash comparison only on a difference,
    /// exactly as `shared_cache::file_freshness` does for the build plane.
    pub fn file_fingerprint(&self, path: &str) -> Option<super::writer::FileFingerprint> {
        let path = normalize_index_key(path);
        let files = self.section(SEC_FILES);
        let count = self.header.file_count as usize;
        let key = path.as_bytes();
        let at = partition_point(count, |slot| self.file_path_bytes(files, slot) < key);
        if at >= count || self.file_path_bytes(files, at) != key {
            return None;
        }
        let rec = &files[at * FILE_REC_LEN..(at + 1) * FILE_REC_LEN];
        Some(super::writer::FileFingerprint {
            path: path.into_owned(),
            mtime_secs: format::file::mtime_secs(rec)?,
            mtime_nanos: format::file::mtime_nanos(rec)?,
            content_hash: format::file::content_hash(rec)?,
        })
    }

    /// `true` once a build supplies directory fingerprints (`write_query_index`,
    /// semx-ykf). `false` for any image built via the dirs-less writer entry
    /// point (`build`) or a pre-semx-ykf image — `Complete` freshness is
    /// simply unavailable on such an image, same "tier absent" contract
    /// `has_refs`/`has_trigram` already use.
    pub fn has_dirs(&self) -> bool {
        !self.header.sections[SEC_DIRS].is_absent()
    }

    pub fn dir_count(&self) -> usize {
        self.header.dir_count as usize
    }

    /// Every directory the index knows about — the `Complete` sweep's stat
    /// set, alongside [`Self::all_file_paths`]. `""` denotes the repo root.
    pub fn all_dir_paths(&self) -> Vec<&str> {
        let dirs = self.section(SEC_DIRS);
        (0..self.header.dir_count as usize)
            .map(|slot| self.dir_path_at(dirs, slot))
            .collect()
    }

    fn dir_path_at(&self, dirs: &[u8], slot: usize) -> &str {
        let rec = &dirs[slot * DIR_REC_LEN..(slot + 1) * DIR_REC_LEN];
        let (off, len) = format::dir::path(rec).unwrap_or((0, 0));
        self.str_at(off, len)
    }

    /// The stored freshness fingerprint for directory `path` — `None` if the
    /// index has never seen this directory (either `Complete` was never
    /// built for this image, or the directory genuinely postdates the last
    /// build). Binary search: `DIRS` is sorted by path bytes at write time
    /// (`writer::build_with_content_and_dirs`), same convention `FILES`
    /// uses.
    pub fn dir_fingerprint(&self, path: &str) -> Option<super::writer::DirFingerprint> {
        let dirs = self.section(SEC_DIRS);
        let count = self.header.dir_count as usize;
        let key = path.as_bytes();
        let at = partition_point(count, |slot| self.dir_path_at(dirs, slot).as_bytes() < key);
        if at >= count || self.dir_path_at(dirs, at).as_bytes() != key {
            return None;
        }
        let rec = &dirs[at * DIR_REC_LEN..(at + 1) * DIR_REC_LEN];
        Some(super::writer::DirFingerprint {
            path: path.to_string(),
            mtime_secs: format::dir::mtime_secs(rec)?,
            mtime_nanos: format::dir::mtime_nanos(rec)?,
        })
    }

    fn section(&self, which: usize) -> &[u8] {
        let section = self.header.sections[which];
        let start = section.offset as usize;
        &self.backing.bytes()[start..start + section.len as usize]
    }

    fn str_at(&self, off: usize, len: usize) -> &str {
        let strings = self.section(SEC_STRINGS);
        strings
            .get(off..off + len)
            .and_then(|raw| std::str::from_utf8(raw).ok())
            .unwrap_or("")
    }

    fn entity_rec(&self, index: usize) -> &[u8] {
        let entities = self.section(SEC_ENTITIES);
        &entities[index * ENTITY_REC_LEN..(index + 1) * ENTITY_REC_LEN]
    }

    fn name_order(&self, names: &[u8], slot: usize) -> u32 {
        format::u32_at(names, slot * NAME_REC_LEN).unwrap_or(0)
    }

    fn name_at(&self, names: &[u8], slot: usize) -> &[u8] {
        let rec = self.entity_rec(self.name_order(names, slot) as usize);
        let (off, len) = format::entity::name(rec).unwrap_or((0, 0));
        self.str_at(off, len).as_bytes()
    }

    fn file_path_bytes(&self, files: &[u8], slot: usize) -> &[u8] {
        let rec = &files[slot * FILE_REC_LEN..(slot + 1) * FILE_REC_LEN];
        let (off, len) = format::file::path(rec).unwrap_or((0, 0));
        self.str_at(off, len).as_bytes()
    }

    /// Public because `grep` needs to turn a `TRIGRAM` posting's file indices
    /// (and the full-scan fallback's `0..file_count`) back into paths it can
    /// open. Out-of-range is `""`, never a panic — same discipline as every
    /// other accessor in this file.
    pub fn file_path(&self, index: u32) -> &str {
        let files = self.section(SEC_FILES);
        let slot = index as usize;
        if slot >= self.header.file_count as usize {
            return "";
        }
        let rec = &files[slot * FILE_REC_LEN..(slot + 1) * FILE_REC_LEN];
        let (off, len) = format::file::path(rec).unwrap_or((0, 0));
        self.str_at(off, len)
    }

    fn kind(&self, id: u16) -> &str {
        let kinds = self.section(SEC_KINDS);
        let slot = id as usize;
        if slot >= self.header.kind_count as usize {
            return "";
        }
        let off = format::u32_at(kinds, slot * KIND_REC_LEN).unwrap_or(0) as usize;
        let len = format::u32_at(kinds, slot * KIND_REC_LEN + 4).unwrap_or(0) as usize;
        self.str_at(off, len)
    }
}

/// A borrowed view of one entity. Every field is read on demand from the
/// mapping; nothing is materialized until asked for, and `id()` is the only
/// accessor that allocates (it has to — the id is stored with its file-path
/// prefix elided, §3.4).
pub struct Entity<'a> {
    index: usize,
    owner: &'a QueryIndex,
}

impl<'a> Entity<'a> {
    /// This entity's slot in `ENTITIES` — the same index `refs_of`/
    /// `callers_of` and CSR targets address, so a caller can chain a name
    /// lookup into a REFS query without re-deriving the index.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Materialize the borrowed view as an owned `EntityInfo` — the shape the
    /// CLI's non-index paths (`EntityGraph`, `CachedImpactResult`) already
    /// speak, so an index answer can be handed to unchanged print code.
    pub fn to_entity_info(&self) -> crate::parser::graph::EntityInfo {
        crate::parser::graph::EntityInfo {
            id: self.id(),
            name: self.name().to_string(),
            entity_type: self.entity_type().to_string(),
            file_path: self.file_path().to_string(),
            parent_id: self.parent_id(),
            start_line: self.start_line(),
            end_line: self.end_line(),
        }
    }

    pub fn name(&self) -> &'a str {
        let rec = self.owner.entity_rec(self.index);
        let (off, len) = format::entity::name(rec).unwrap_or((0, 0));
        self.owner.str_at(off, len)
    }

    pub fn entity_type(&self) -> &'a str {
        let rec = self.owner.entity_rec(self.index);
        self.owner.kind(format::entity::kind(rec).unwrap_or(0))
    }

    pub fn file_path(&self) -> &'a str {
        let rec = self.owner.entity_rec(self.index);
        self.owner.file_path(format::entity::file(rec).unwrap_or(0))
    }

    pub fn start_line(&self) -> usize {
        let rec = self.owner.entity_rec(self.index);
        format::entity::start_line(rec).unwrap_or(0) as usize
    }

    pub fn end_line(&self) -> usize {
        let rec = self.owner.entity_rec(self.index);
        format::entity::end_line(rec).unwrap_or(0) as usize
    }

    /// The entity's byte span in its file (semx-a3w, `FORMAT_VERSION` 3),
    /// `None` if the writer had none to record — see [`format::entity`]'s
    /// doc for why that's a per-record, not per-image, signal. `context.rs`'s
    /// index reroute is the only consumer today: it must decline (not
    /// degrade) the instant either half of a needed entity's span is absent,
    /// because there is no line-granularity fallback that stays
    /// byte-identical with the authoritative extractor's byte-sliced
    /// `SemanticEntity.content`.
    pub fn start_byte(&self) -> Option<usize> {
        let rec = self.owner.entity_rec(self.index);
        match format::entity::start_byte(rec).unwrap_or(NONE_U32) {
            NONE_U32 => None,
            v => Some(v as usize),
        }
    }

    pub fn end_byte(&self) -> Option<usize> {
        let rec = self.owner.entity_rec(self.index);
        match format::entity::end_byte(rec).unwrap_or(NONE_U32) {
            NONE_U32 => None,
            v => Some(v as usize),
        }
    }

    /// Both halves of the byte span, only when both are present — the shape
    /// every span consumer actually wants (a one-sided span is meaningless).
    pub fn byte_span(&self) -> Option<(usize, usize)> {
        Some((self.start_byte()?, self.end_byte()?))
    }

    /// Whether this entity was classified a test at build time. Meaningless
    /// (always `false`) unless [`QueryIndex::has_test_flags`] — see there.
    pub fn is_test(&self) -> bool {
        let rec = self.owner.entity_rec(self.index);
        format::entity::flags(rec).unwrap_or(0) & format::entity::FLAG_IS_TEST != 0
    }

    pub fn parent_index(&self) -> Option<usize> {
        let rec = self.owner.entity_rec(self.index);
        match format::entity::parent(rec).unwrap_or(NONE_U32) {
            NONE_U32 => None,
            parent => Some(parent as usize),
        }
    }

    pub fn parent_id(&self) -> Option<String> {
        self.parent_index().map(|at| self.owner.entity(at).id())
    }

    pub fn id(&self) -> String {
        let rec = self.owner.entity_rec(self.index);
        let (off, len) = format::entity::id_tail(rec).unwrap_or((0, 0));
        let tail = self.owner.str_at(off, len);
        if self.owner.header.flags & FLAG_IDS_ELIDED == 0 {
            return tail.to_string();
        }
        let file = self.file_path();
        let mut id = String::with_capacity(file.len() + 2 + tail.len());
        id.push_str(file);
        id.push_str("::");
        id.push_str(tail);
        id
    }
}

/// Inverse of `writer::ref_kind_u8`. Any encoding outside 0/1/2 (never
/// produced by the writer today — `format::refs::MAX_KIND` reserves 3-15 for
/// future `RefType` variants, none of which exist yet) falls back to
/// `Calls`, the same "never a panic, never a corrupt read" discipline every
/// other reader fallback in this module follows.
fn ref_kind_from_u8(kind: u8) -> crate::parser::graph::RefType {
    use crate::parser::graph::RefType;
    match kind {
        1 => RefType::TypeRef,
        2 => RefType::Imports,
        _ => RefType::Calls,
    }
}

/// `slice::partition_point` over an index range rather than a slice, because
/// the "elements" are computed (a `NAMES` slot dereferences into `ENTITIES`
/// and then into the string arena) rather than stored contiguously.
fn partition_point(count: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}
