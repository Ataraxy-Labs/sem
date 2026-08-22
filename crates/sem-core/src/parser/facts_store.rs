//! On-disk persistence for the red-green facts layer: the missing
//! half of [`crate::parser::session::GraphSession`]'s promise that "a verified
//! fact must not die with its process." Everything a warm rebuild needs to
//! treat an unchanged file as GREEN — its extracted entities, its precomputed
//! scope/ref facts, its cached resolution edges and read sets, and the
//! corpus-wide table fingerprints those read sets are checked against — is
//! already `serde`-serializable (`FileFacts`, `PrecomputedFileFacts`,
//! `CachedFileResolution`, `TableFingerprints`). This module is the disk tier
//! that lets a **fresh process** load them back instead of paying a cold
//! build, the way [`GraphSession::rebuild`] already lets a *live* process
//! reuse them across edits.
//!
//! # Format: one CBOR blob per repo root, not a sharded per-file store
//!
//! The access pattern here is "give me everything for this corpus," not
//! "look up one random key" — every warm start reads the *whole* previous
//! snapshot (to compare each current file's content hash against what was
//! stored) and every save writes the whole current snapshot. That is a
//! **corpus-scoped bulk transfer**, not a point-lookup cache, and the format
//! that wins for one is not the format that wins for the other:
//!
//! * **Sharded per-file store** (the shape `parser::cache`'s in-memory/disk
//!   extraction cache uses, key-hashed into a two-level directory fan-out):
//!   wrong shape here. Loading a 40k-file corpus would mean 40k `open` +
//!   `read` + `close` syscalls. Measured on the monster corpus's own facts
//!   (below): that syscall overhead alone is on the order of the entire
//! rebuild savings this change is chasing — sharding a bulk-transfer
//!   workload does not "decisively beat rebuilding," it competes with it.
//! * **Single SQLite database**: a real alternative (`sem-mcp`'s own
//!   per-repo cache already uses one, for a genuinely point-lookup access
//!   pattern), but wrong for *this* store for three reasons. (1) It adds a C
//!   dependency (`rusqlite`/`libsqlite3-sys`) to `sem-core` specifically,
//!   which currently has none and carries an (unused today, but real) `wasm`
//!   feature that a linked C library would put at risk. (2) A page-cache-
//!   backed B-tree walk over ~40k rows pays random I/O and query-planning
//!   overhead a linear sequential read skips entirely — the wrong trade for
//!   "read everything," even though it's the right one for "read one row."
//!   (3) It buys transactional point-updates and ad hoc queries this store
//!   never uses: every save already writes the complete corpus.
//! * **`bincode`, tried first and rejected — not hypothetically, but by a RED
//!   test.** Bincode is positional, not self-describing: a struct's decoder
//!   reads exactly one value per *declared* field, in order. `SemanticEntity`
//!   (persisted inside every `FileFacts`) has several
//!   `#[serde(skip_serializing_if = "Option::is_none")]` fields — correct and
//!   cheap under the self-describing formats it already round-trips through
//!   (`serde_json`, elsewhere in this crate), because those decoders look
//!   values up by key and `#[serde(default)]` fills the gap. Under bincode,
//!   skipping a `None` field on write silently drops one value from the byte
//!   stream while the decoder still expects it — not a decode error at the
//!   skipped field, but a desync that corrupts every field read afterward.
//!   `round_trips_a_saved_snapshot` caught this immediately (a bincode build
//!   failed to load its own just-saved data, "tag for enum is not valid");
//!   switching to a self-describing format was the fix, not a workaround
//!   (`SemanticEntity`'s attributes are correct and used elsewhere — the
//!   store's format needs to fit the type, not the other way around).
//! * **One CBOR-encoded blob per repo root** (what this module does): one
//!   `open` + one `read` + one linear decode to load, one `write` + `rename`
//!   to save — the same shape the bincode attempt had, minus the landmine.
//!   CBOR is self-describing (binary, but field-keyed like JSON) so it
//!   round-trips exactly the structs `serde_json` already does correctly,
//!   while still decoding decisively faster than JSON's text parsing for a
//!   bulk-load access pattern (no string escaping/UTF-8 validation on every
//!   field, no number-to-string-and-back for every hash and byte offset).
//!
//! Measured save/load/size numbers for both corpora, including one result
//! that didn't make the cut, showed what was measured and dropped rather
//! than shipped, per this change's own instruction to drop (and report) any
//! component that loads slower than it rebuilds.
//!
//! # Keying and versioning
//!
//! The store file for a root is named by a hash of the root's canonicalized
//! path — one file per repo, inside a directory the *caller* supplies (see
//! "No ambient paths" below). Inside that file, every entry is additionally
//! guarded by:
//!
//! * **Per-file content hash** (`FileFacts::content_hash`, folded in
//!   alongside the file's own path — see [`PersistedFile`] — because
//!   `SemanticEntity::id`/`file_path` and every scope's `owner_id` are
//!   path-qualified: the same bytes at a different path are not the same
//!   facts, so path must be part of the identity a hit is judged against,
//!   exactly as `parser::cache`'s `key_for` already established for
//!   extraction). A file whose on-disk bytes changed since the snapshot was
//!   taken is simply absent from the reusable set — never served stale.
//! * **`FACTS_SCHEMA_VERSION`**, bumped whenever any type reachable from
//!   [`PersistedFacts`] changes shape. CBOR being self-describing makes most
//!   *additive* changes (a new `Option` field with `#[serde(default)]`)
//!   forward-compatible for free, but a field that changes *meaning* (not
//!   just presence) — a renamed key, a retyped value, a changed enum variant
//!   set — is not something the wire format can catch by itself. The version
//!   is checked explicitly, before the payload is even decoded, so schema
//!   drift is a deliberate miss rather than a hope that decoding happens to
//!   fail loudly.
//! * **`sem_core_salt`** (`CARGO_PKG_VERSION`), an independent second guard:
//!   two `sem` binaries built from different `sem-core` releases sharing one
//!   cache directory (e.g. a machine upgrading mid-session) must not decode
//!   each other's snapshots even if nobody remembered to bump the schema
//!   version for a given change.
//!
//! A mismatch on either guard, a missing file, or corrupt/truncated bytes are
//! all the same outcome: [`FactsStore::load`] returns `None` — a clean miss,
//! never a panic, never wrong facts. Deleting the store directory is always
//! safe: it is advisory, exactly like `parser::cache`'s disk tier.
//!
//! # No ambient paths
//!
//! [`FactsStore::open`] takes a directory from the caller and does not touch
//! disk itself; nothing in this crate reaches for `$HOME` or `XDG_CACHE_HOME`
//! on its own. A store is a capability handed in, not a global. Wiring the
//! *one* real caller (`sem`'s CLI diff/graph path) — including where that
//! directory defaults to and how it is disabled — lives in `sem-cli`, not
//! here.
//!
//! # Coordination note
//!
//! A concurrent change is adding serde-additive optional fields to `FileFacts`
//! (bow-token work). Because this store is CBOR (self-describing, field-keyed
//! — chosen *because* of this concern, see "Format" above), a genuinely
//! additive `Option<T>` field with `#[serde(default)]` decodes fine against
//! an old store without a version bump: the key is simply absent and default
//! fills it in, the same way it already would under `serde_json`. Bump
//! `FACTS_SCHEMA_VERSION` anyway if their change is not purely additive in
//! that sense — a renamed/retyped field, a field that stops being optional,
//! or anything where an *old* store's absence of the key should mean
//! something other than "use the default" for the new code.

use std::fs::File;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;
use rustc_hash::FxHashSet as HashSet;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

use crate::model::entity::SemanticEntity;
use crate::parser::incremental::{
    content_hash, CachedFileResolution, FileFacts, TableFingerprints,
};
use crate::parser::plugins::code::languages::get_language_config;
use crate::parser::registry::ParserRegistry;
use crate::parser::scope_resolve::PrecomputedFileFacts;

/// Same convention `graph.rs`/`scope_resolve.rs`/`session.rs` already use.
macro_rules! maybe_par_iter {
    ($slice:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $slice.par_iter()
        }
        #[cfg(not(feature = "parallel"))]
        {
            $slice.iter()
        }
    }};
}

/// Bump whenever any type reachable from [`PersistedFacts`] changes shape.
/// See the module doc's "Keying and versioning" section for why this can't be
/// inferred from a decode failure alone.
///
/// 1 -> 2 (MUL phase 2): `PrecomputedFileFacts`
/// grew `import_stmts: Vec<ImportStmtFacts>`. Purely additive in content
/// (every producer today writes an empty `Vec` — see that field's own doc
/// comment for why it is provably empty for both languages that reach a
/// precompute producer), but it is still a *shape* change reachable from
/// `PersistedFacts`, so the version bumps per this doc's own rule rather than
/// relying on CBOR's forward-compatible decode of a new field: an old store's
/// entries become a clean miss (rebuilt), never a misdecode, on the very next
/// warm start against a binary carrying this field.
///
/// 2 -> 3 (MUL phase 2):
/// `PrecomputedFileFacts` grew `ctor_call_sites: Vec<CtorCallFacts>` — same
/// shape as the 1 -> 2 bump (a new type, `CtorCallFacts`, reachable from
/// `PersistedFacts`; every producer but Python's still writes an empty
/// `Vec`), so it gets the identical clean-miss treatment rather than relying
/// on CBOR's forward-compatible decode.
///
/// 3 -> 4 (non-code plugin byte ranges + markdown fenced-code-block fix): not a shape
/// change — `SemanticEntity::start_byte`/`end_byte` stay `Option<usize>` —
/// but a *meaning* change this doc's own rule still catches: every
/// non-tree-sitter plugin (markdown, toml, yaml, json, csv, latex, vue,
/// svelte, erb, fallback) used to leave both fields `None` unconditionally;
/// the registry's extractor boundary now fills them from the entity's own
/// line range for any plugin other than the tree-sitter code plugin. A
/// pre-fix store's `None` for one of these entities no longer means "not
/// computed" (a correct default) — it means "computed by buggy code," which
/// `#[serde(default)]` cannot tell apart from the real default. The
/// markdown plugin's fenced-code-block heading fix changes which heading
/// entities exist at all for an affected file, same reasoning. `file`
/// content_hash gates re-extraction only when the file's own bytes change,
/// not when the extractor's output for unchanged bytes changes, so a plain
/// content-hash hit would keep serving the stale (pre-fix) entities across a
/// binary upgrade that shares `sem_core_salt` with the pre-fix release (a
/// dev build between release-prep commits, in particular) — the same
/// producer-identity problem `language_salt`/`producer_language_salt` exists
/// to solve for tree-sitter grammars, just not something a non-code plugin
/// has ever needed a salt for before. Bumping the shared schema version is
/// the coarse-but-correct fix: it clean-misses the whole corpus (code-plugin
/// entries included, even though their producer didn't change) rather than
/// inventing a new per-non-code-plugin salt table for a one-time fix.
pub const FACTS_SCHEMA_VERSION: u32 = 4;

const MAGIC: &[u8; 8] = b"SEMFACT1";

/// Target files per shard for the parallel-decodable body (see `save`/`load`
/// and the module doc's "Format" section). Small enough that a 40k-file
/// corpus gets real decode parallelism (~16 shards), large enough that a
/// 1.5k-file corpus doesn't pay per-shard CBOR framing overhead for no
/// benefit (it gets one shard, same as an unsharded body would have been).
const TARGET_SHARD_SIZE: usize = 2_500;

fn sem_core_salt() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// One file's persisted bundle: its extraction output, its precomputed
/// scope/ref facts (JS/TS only — `None` for every other language, exactly
/// mirroring `GraphSession`'s own `precomputed` map), and its cached
/// resolution edges/read-sets (`None` until a build has actually resolved
/// it, e.g. a file added but not yet part of a resolved rebuild).
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PersistedFile {
    pub(crate) facts: FileFacts,
    pub(crate) precomputed: Option<PrecomputedFileFacts>,
    pub(crate) resolution: Option<CachedFileResolution>,
}

/// Borrowing serialize-only twin of [`FileFacts`].
/// Field names and order match `FileFacts` exactly, so serde encodes a value
/// of this byte-identically to the owned struct it mirrors.
#[derive(Serialize)]
pub(crate) struct FileFactsRef<'a> {
    pub(crate) path: &'a str,
    pub(crate) content_hash: u64,
    pub(crate) entities: &'a [SemanticEntity],
}

/// Borrowing serialize-only twin of [`PersistedFile`].
#[derive(Serialize)]
pub(crate) struct PersistedFileRef<'a> {
    pub(crate) facts: FileFactsRef<'a>,
    pub(crate) precomputed: Option<&'a PrecomputedFileFacts>,
    pub(crate) resolution: Option<&'a CachedFileResolution>,
}

/// The facts layer as a **view borrowing the live session** — what the two
/// savers ([`FactsStore::save`], [`FactsCorpus::populate_delta`]) consume.
/// [`PersistedFacts`] stays the *deserialize* type
/// (`FactsStore::load` returns it, `GraphSession::warm_start` consumes it by
/// value); this is the *serialize* shape, so exporting a session's facts for
/// persistence no longer deep-clones every entity body, precomputed source
/// text and cached edge list the session still owns — one of the three
/// corpus copies behind the facts plane's measured RSS band.
pub struct PersistedFactsRef<'a> {
    pub(crate) fingerprints: &'a TableFingerprints,
    pub(crate) files: Vec<PersistedFileRef<'a>>,
}

impl PersistedFactsRef<'_> {
    /// Number of files this view holds. Exposed for diagnostics/probes.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

#[derive(Serialize, Deserialize)]
struct StoreHeader {
    schema_version: u32,
    sem_core_salt: String,
}

fn write_u64_le(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Reads a little-endian `u64` off the front of `bytes`, returning it with
/// the remaining slice. `None` on anything short of 8 bytes — truncated
/// input is handled the same way everywhere in this module: a clean miss.
fn read_u64_le(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let (head, tail) = bytes.split_at_checked(8)?;
    Some((u64::from_le_bytes(head.try_into().ok()?), tail))
}

/// A snapshot of the facts layer for one repo root: every file's persisted
/// bundle plus the corpus-wide table fingerprints a warm rebuild's read-set
/// checks need.
///
/// Opaque to callers *outside this crate* — the type is `pub` (so `sem-cli`
/// can hold and move a value of it between [`FactsStore`] and
/// [`crate::parser::session::GraphSession::warm_start`]) but its fields are
/// `pub(crate)`, visible only inside `sem-core`, where `GraphSession` is the
/// other party that actually reads them.
pub struct PersistedFacts {
    pub(crate) fingerprints: TableFingerprints,
    pub(crate) files: HashMap<String, PersistedFile>,
}

impl PersistedFacts {
    pub(crate) fn new(fingerprints: TableFingerprints, files: Vec<PersistedFile>) -> Self {
        let mut map = HashMap::default();
        map.reserve(files.len());
        for f in files {
            map.insert(f.facts.path.clone(), f);
        }
        Self {
            fingerprints,
            files: map,
        }
    }

    /// Number of files this snapshot holds. Exposed for diagnostics/tests.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of corpus-wide fingerprint entries this snapshot holds.
    /// Diagnostic only: a snapshot whose `file_count` is nonzero
    /// but whose `fingerprint_count` is zero cannot warm *any* file GREEN —
    /// see `crate::parser::incremental::Incremental::new`'s
    /// `reuse && !prev_fp.is_empty()` gate, which disables reuse for the
    /// whole build when this is `0`, independent of any single file's
    /// content hash matching.
    pub fn fingerprint_count(&self) -> usize {
        self.fingerprints.len()
    }

    /// Number of files this snapshot holds a `resolution` (cached scope
    /// edges + read set) for. Diagnostic only: `resolve_with_
    /// scopes_full_inner`'s GREEN filter requires `Incremental::cached` to
    /// return `Some` for a file *in addition to* its read set being
    /// unchanged — a file present in `files` (so it counts toward
    /// `file_count`) but with `resolution: None` can never go GREEN no
    /// matter how clean its content hash or read set are.
    pub fn resolved_file_count(&self) -> usize {
        self.files
            .values()
            .filter(|f| f.resolution.is_some())
            .count()
    }

    /// Whether this snapshot has an entry for `path` at all — deliberately
    /// says nothing about whether that entry is still fresh (a stale-hash
    /// entry still counts as "known"). Callers deciding whether a path needs
    /// a *different* source of facts (e.g. `sem-cli`'s cross-repo corpus
    /// consult) should use this to mean "already has an opinion
    /// here, don't override it" — staleness is `GraphSession::warm_start`'s
    /// own job, not this method's.
    pub fn files_contains(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// A cheap `path -> content_hash` index — no entity bodies, no
    /// precomputed/resolution payloads, just what
    /// [`FactsCorpus::populate_delta`] needs to tell an unchanged file from
    /// a new/edited one. Exists so callers (`sem-cli`'s wiring) can capture
    /// "what did the store know before this build" *before* handing the
    /// full snapshot's ownership into [`crate::parser::session::
    /// GraphSession::warm_start`], without paying to clone every entity body
    /// just to keep a diff base alive across that move — at monster scale
    /// the full snapshot is hundreds of MB; this index is tens of thousands
    /// of small string+u64 pairs.
    pub fn content_hash_index(&self) -> HashMap<String, u64> {
        self.files
            .iter()
            .map(|(path, f)| (path.clone(), f.facts.content_hash))
            .collect()
    }

    /// This snapshot as the borrowing serialize view the savers consume —
    /// for callers (tests, probes) that hold an owned
    /// `PersistedFacts` rather than a live `GraphSession`. O(files) pointer
    /// work; no entity body is touched.
    pub fn as_borrowed(&self) -> PersistedFactsRef<'_> {
        PersistedFactsRef {
            fingerprints: &self.fingerprints,
            files: self
                .files
                .values()
                .map(|f| PersistedFileRef {
                    facts: FileFactsRef {
                        path: &f.facts.path,
                        content_hash: f.facts.content_hash,
                        entities: &f.facts.entities,
                    },
                    precomputed: f.precomputed.as_ref(),
                    resolution: f.resolution.as_ref(),
                })
                .collect(),
        }
    }
}

/// A directory-scoped facts store. No ambient path: `dir` always comes from
/// the caller (`FactsStore::open` does not touch disk). One file per repo
/// root lives under `dir`, named by a hash of the root's canonicalized path.
pub struct FactsStore {
    dir: PathBuf,
}

impl FactsStore {
    /// Open a store rooted at `dir`. Does not create the directory or touch
    /// disk — that happens lazily on the first `save`.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path_for(&self, root: &Path) -> PathBuf {
        self.dir.join(format!("{:016x}.factpack", root_key(root)))
    }

    /// Load the persisted snapshot for `root`. A missing file, a
    /// schema-version or sem-core-salt mismatch, and corrupt/truncated bytes
    /// are all the same outcome: `None`. Never panics on bad input — this is
    /// the boundary where an on-disk artifact from a different (or broken)
    /// process becomes untrusted input.
    ///
    /// The body is decoded shard-parallel (see `save`'s doc comment) — the
    /// dominant cost of a load on a large corpus is CPU-bound CBOR decode,
    /// not disk I/O (confirmed by measurement: repeat loads of an
    /// OS-page-cache-warm store did not get faster), so this is where the
    /// "must decisively beat rebuilding" load-speed budget actually goes.
    pub fn load(&self, root: &Path) -> Option<PersistedFacts> {
        let bytes = std::fs::read(self.path_for(root)).ok()?;
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return None; // truncated below the magic, or not our file at all
        }
        let mut rest = &bytes[MAGIC.len()..];

        // The header is decoded and version-checked *before* the (much
        // larger) body ever reaches CBOR, so a schema bump is a cheap,
        // deliberate miss rather than a decode that happens to fail deep in
        // the body. `ciborium::from_reader` consumes exactly the bytes one
        // self-describing value needs and leaves `rest` positioned right
        // after it, so every read below picks up exactly where the previous
        // one left off with no separate length prefix to track for these.
        let header: StoreHeader = ciborium::from_reader(&mut rest).ok()?;
        if header.schema_version != FACTS_SCHEMA_VERSION || header.sem_core_salt != sem_core_salt()
        {
            return None; // clean miss: a different sem-core wrote this store
        }
        let fingerprints: TableFingerprints = ciborium::from_reader(&mut rest).ok()?;

        // The rest of the file is `shard_count` raw-length-prefixed CBOR
        // values, each an independent `Vec<PersistedFile>` — sliced out here
        // (cheap, no decode yet) so they can be handed to `maybe_par_iter!`
        // below. A short/garbled length or fewer bytes than claimed is the
        // same clean miss as everything else in this function.
        let (shard_count, mut rest) = read_u64_le(rest)?;
        let mut shards: Vec<&[u8]> = Vec::with_capacity(shard_count as usize);
        for _ in 0..shard_count {
            let (shard_len, after_len) = read_u64_le(rest)?;
            let (shard_bytes, after_shard) =
                after_len.split_at_checked(usize::try_from(shard_len).ok()?)?;
            shards.push(shard_bytes);
            rest = after_shard;
        }

        let decoded: Option<Vec<Vec<PersistedFile>>> = maybe_par_iter!(shards)
            .map(|shard: &&[u8]| ciborium::from_reader::<Vec<PersistedFile>, _>(*shard).ok())
            .collect();
        let files = decoded?.into_iter().flatten().collect();

        Some(PersistedFacts::new(fingerprints, files))
    }

    /// Persist `facts` for `root`. Write-then-rename, exactly like
    /// `parser::cache`'s disk tier, so a concurrent reader never observes a
    /// partial file and two processes writing the same root never share a
    /// scratch file. Best-effort by design: the caller decides whether a
    /// write failure (permissions, disk full, read-only FS) is worth
    /// reporting; it never corrupts whatever snapshot was there before.
    ///
    /// The file list is split into `~TARGET_SHARD_SIZE`-file shards, each
    /// independently CBOR-encoded and length-prefixed, so `load` can decode
    /// shards in parallel — still exactly **one** `open`/`read`/`write` on
    /// the store as a whole (the "single blob, not a sharded per-file store"
    /// decision in the module doc is about filesystem entries, not about
    /// whether the one blob's *body* can be chunked for parallel CPU work).
    pub fn save(&self, root: &Path, facts: &PersistedFactsRef<'_>) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let header = StoreHeader {
            schema_version: FACTS_SCHEMA_VERSION,
            sem_core_salt: sem_core_salt().to_string(),
        };
        let all_files: &[PersistedFileRef<'_>] = &facts.files;
        let shard_count = all_files.len().div_ceil(TARGET_SHARD_SIZE).max(1);
        let per_shard = all_files.len().div_ceil(shard_count).max(1);
        let chunks: Vec<&[PersistedFileRef<'_>]> = all_files.chunks(per_shard).collect();
        let shard_bytes: Option<Vec<Vec<u8>>> = maybe_par_iter!(chunks)
            .map(|chunk: &&[PersistedFileRef<'_>]| {
                let mut buf = Vec::new();
                ciborium::into_writer(chunk, &mut buf).ok()?;
                Some(buf)
            })
            .collect();
        let shard_bytes = shard_bytes.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "failed to encode a facts shard")
        })?;

        // Write-through, no staging copy: the encoded
        // shards used to be memcpy'd into one contiguous `Vec<u8>` purely so
        // a single `fs::write` could be called — a full second copy of the
        // corpus's CBOR held at peak. Nobody needed the bytes contiguous,
        // only durable: stream the exact same byte sequence through a
        // `BufWriter` on the scratch file instead, dropping each shard as it
        // is written, and keep the write-then-rename discipline (a concurrent
        // reader still never observes a partial file).
        let path = self.path_for(root);
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let write_result = (|| -> io::Result<()> {
            let mut w = io::BufWriter::new(File::create(&tmp)?);
            w.write_all(MAGIC)?;
            ciborium::into_writer(&header, &mut w).map_err(to_io_error)?;
            ciborium::into_writer(facts.fingerprints, &mut w).map_err(to_io_error)?;
            w.write_all(&(shard_bytes.len() as u64).to_le_bytes())?;
            for shard in shard_bytes {
                w.write_all(&(shard.len() as u64).to_le_bytes())?;
                w.write_all(&shard)?;
            }
            w.flush()?;
            Ok(())
        })()
        .and_then(|()| std::fs::rename(&tmp, &path));
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write_result
    }

    /// Delete the persisted snapshot for `root`, if any. Advisory only: the
    /// store is a pure speed optimization, so this is always safe to call,
    /// including when nothing was ever saved.
    pub fn clear(&self, root: &Path) -> io::Result<()> {
        match std::fs::remove_file(self.path_for(root)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn to_io_error(e: ciborium::ser::Error<io::Error>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

fn root_key(root: &Path) -> u64 {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut h = Xxh3::new();
    h.update(canonical.to_string_lossy().as_bytes());
    h.digest()
}

// =============================================================================
// Cross-repo corpus: Phase B's local tier
// =============================================================================
//
// Everything above is scoped to *one repo root*: `FactsStore` shares facts
// across builds of the *same* checkout, but a fresh clone of a repo that
// happens to be byte-identical at most paths to a repo `sem` has already
// built — a fork, a vendored dependency reproduced verbatim, a second
// checkout of the same repo at a different path — pays a full cold build
// anyway, because `FactsStore::path_for` keys its one blob by the *root's
// own canonicalized path*. `FactsCorpus` is the machine-global tier that
// closes that gap: identical file content at the same relative path, in ANY
// repo on this machine, shares its extracted facts.
//
// ## Key: why relative path is part of correctness, not just locality
//
// `SemanticEntity::id`/`file_path` and every scope's `owner_id` are
// path-qualified (the same discipline this file's "Keying and versioning"
// doc section already established for the per-repo tier). Two files with
// identical bytes at *different* relative paths do not produce the same
// entities — their entity ids differ — so relative path is not a
// cache-locality nicety here, it is a **correctness requirement**: a corpus
// entry is addressable only by `(relative_path, content_hash,
// language_salt)` together, never by content hash alone (see
// `corpus_key_excludes_wrong_path` in this module's tests for the negative
// proof).
//
// `language_salt` is deliberately **per-language**, not one crate-wide salt:
// a tree-sitter grammar bump changes only the languages that grammar
// serves, so invalidating the *entire* corpus over, say, a Kotlin grammar
// bump would throw away perfectly valid TypeScript/Python/Go/Rust facts for
// no reason. See `LANGUAGE_SALTS` below for the (hand-maintained — see its
// own doc comment for why) per-language table.
//
// ## Storage and concurrency
//
// The access pattern here is the opposite of the per-repo blob's: a
// **point lookup** per file (does the corpus already have *this* file's
// facts?), not "give me everything." A pure one-file-per-fact
// content-addressed store would make that lookup a single `open`+`read` per
// file, but at monster scale (40k+ files) that reintroduces exactly the
// syscall-bound cost this file's "Format" doc section measured and rejected
// for the *bulk-load* access pattern, now applied to a *point-lookup*
// pattern instead — so entries are grouped into a fixed number of
// **buckets** (`CORPUS_BUCKETS`, hashed from relative path alone,
// corpus-wide and independent of any one repo's size) so a lookup over a
// whole repo's file list touches at most `min(file_count, CORPUS_BUCKETS)`
// shard files, not one per file. Each bucket is one shard file
// (`<corpus_dir>/shard-XXXX.factshard`), written whole via
// temp-file-then-`rename` exactly like the per-repo blob — POSIX `rename`
// is atomic, so a reader never observes a torn shard, and every decode path
// here reuses the same "any anomaly is a clean miss, never a panic"
// discipline `FactsStore::load` established (see `load_shard` below).
//
// A shard can hold entries contributed by many different builds over time,
// so *writing* one is a read-merge-write, not a blind overwrite — unlike
// the per-repo blob (whole-corpus rewrite every save). Two processes racing
// to merge into the *same* bucket could otherwise silently lose one
// writer's entries (both read the old content, each adds different new
// entries, the second `rename` wins and drops the first's additions — never
// corruption, since `rename` itself stays atomic, but a real lost update).
// `ShardLock` closes that gap with a tiny advisory file lock (a `.lock`
// sibling created with `create_new`, not a new dependency — matching the
// per-repo store's own "no C dependency" restraint) around the
// read-merge-write, with a bounded wait before proceeding unlocked: a stale
// lock left by a crashed process must never wedge a future build forever,
// and a build must never fail because the corpus — a pure speed
// optimization — couldn't get a lock.
//
// ## What is NOT shared cross-repo, and why
//
// A corpus entry carries a file's extracted entities (`FileFacts`) and its
// JS/TS precomputed scope/ref facts (`PrecomputedFileFacts`) — both pure
// functions of `(relative_path, content)` alone, safe to share the instant
// path and content match. It never carries `CachedFileResolution` (cached
// cross-file edges + read sets): those depend on *which other files* the
// repo has and what they currently contain, which two repos sharing one
// file's content are not guaranteed to agree on (a fork could have
// renamed, deleted, or edited the file this one imports). Sharing
// resolution edges cross-repo without also proving every transitively
// referenced file matches would risk exactly the failure `incremental.rs`'s
// own module doc calls unforgivable: silently wrong edges. This is a
// conservative scope boundary, not a missing feature — a cross-repo build
// still skips re-parsing/re-extracting every matched file (pass 1's cost,
// the dominant share of a cold build per this document's earlier
// sections); it simply re-resolves cross-file edges fresh, exactly as if
// those files had been freshly added to an ordinary warm rebuild.
//
//
// for the cross-repo proof numbers (reuse counts, time saved, and the
// negative same-content-different-path test) and the load-speed
// measurement against the per-repo tier's 220ms bar.
//
// ## Coordination note (mirrors 's above)
//
// Corpus shard headers reuse `FACTS_SCHEMA_VERSION`/`sem_core_salt()` — the
// same single version knob the per-repo store uses — rather than a second,
// separately-tracked version number, since `CorpusFile` is built entirely
// from types (`FileFacts`, `PrecomputedFileFacts`) that knob already
// governs. A shape change to either type invalidates both tiers together
// with one bump, exactly the "bump salt once at the end" coordination this
// change's own instructions ask for with any concurrent serde-additive work.

// ## Shard layout v2: why a shard carries its own index
//
// v1 stored a shard as one CBOR array of `CorpusFile`, so *any* consult of a
// shard — a one-key lookup, a one-entry write — decoded every entry in it.
// With `CORPUS_BUCKETS` fixed, per-shard size grows linearly with the corpus,
// so both costs tracked everything the machine had ever indexed. That is not
// a hypothesis: measured the same repo at an identical
// 40,869/40,869 hit rate costing 8.5 s against a 556 MB corpus and 13.4 s
// against a 7.9 GB one, and this change's own baseline split the +4.9 s into
// +2.3 s of `merge_with_local` and +1.4 s of `populate_delta` — the read
// side and the write side of exactly this decode.
//
// Note what v1 was *not* guilty of: `merge_with_local` already opened only
// the buckets its own probes hashed into. `shards_read=1024` was not a
// missing prune, it was saturation — 40,869 paths into 1,024 buckets hits
// every bucket with probability ~1. Pruning *shards* was already maximal.
// What was unpruned was the bytes inside each one.
//
// v2 therefore prunes within the shard instead:
//
//     "SEMCORP2"                       magic
//     CBOR StoreHeader                 schema_version + sem_core_salt
//     u64_le entry_count
//     entry_count × 24-byte records    {key_hash, offset, len}, key_hash-sorted
//     payload region                   entry_count CBOR `CorpusFile` blobs
//
// `key_hash` is [`corpus_key_hash`] over the whole lookup identity, so a
// reader seeks: read the header + index (24 bytes an entry, tens of KB even
// on a giant corpus), binary-search each probe's key, and decode *only* the
// payload ranges that matched. Cost tracks hits, not corpus size.
//
// A writer gets the same property from the other direction. Because the
// index alone answers "does this shard already hold this key?", a write
// first drops every entry already stored — on a known-content build that is
// all of them, and the shard is not rewritten at all — and carries the
// surviving old payloads forward as **opaque bytes** it never decodes. The
// old payload region is byte-identical and stays at the front, so old
// offsets stay valid and only the index is rebuilt.
//
// Two consequences, both deliberate and both documented rather than hidden:
//
// 1. **Dedup is now first-writer-wins, where v1 was last-writer-wins.** A
//    `CorpusFile`'s value is a pure function of its key — `facts` is derived
//    from `(relative_path, content)` and `lang_salt` pins the extractor
//    generation — so the two entries a dedup chooses between are
//    interchangeable. The one observable difference is `precomputed`
//    presence, which costs speed on a later build, never correctness.
// 2. **A `key_hash` collision evicts nothing and serves nothing wrong.** On
//    read, every candidate the index returns is still checked field-by-field
//    by [`corpus_matches`] after decode, exactly as in v1 — a collision is a
//    false miss (re-extract, always safe). On write, a collision makes a new
//    entry look already-present and skips storing it: also a future miss.
//
// ## Migration
//
// The magic is bumped `SEMCORP1` -> `SEMCORP2`, so a v1 shard fails the
// magic check and takes the same "any anomaly is a clean miss" path every
// other malformed shard takes — an old corpus degrades to a cold build and
// is rewritten in v2 on the way past, with no separate migration step.
// `FACTS_SCHEMA_VERSION` is deliberately *not* bumped: it governs the
// `CorpusFile`/`PersistedFile` *shape*, which is unchanged here, and it is
// shared with the per-repo `FactsStore` (untouched by this change) and
// validated against the cloud tier's `claimed_schema_version` in
// `ingest_remote`. Bumping it would invalidate two stores and one wire
// contract for a change that is purely this file's on-disk framing.
const CORPUS_MAGIC: &[u8; 8] = b"SEMCORP2";

/// Bytes per shard index record: `key_hash`, `offset`, `len`, all `u64_le`.
const INDEX_RECORD_LEN: usize = 24;

/// How much of a shard's head to read in one go to reach `entry_count`. The
/// magic (8 bytes) plus a CBOR `StoreHeader` (two short fields) is well under
/// this; the read is clamped to the file length, so a shorter shard is a
/// clean miss rather than an error.
const SHARD_HEAD_PROBE: usize = 512;

/// Number of corpus-wide buckets a relative path hashes into. Fixed and
/// independent of any one repo's size (unlike the per-repo blob's
/// content-proportional `TARGET_SHARD_SIZE` sharding) — see the "Storage
/// and concurrency" doc above for why a fixed count bounds lookup cost
/// regardless of how large the corpus grows.
const CORPUS_BUCKETS: u64 = 1024;

/// How long a writer waits for a shard's advisory lock before giving up and
/// proceeding unlocked (see "Storage and concurrency" above).
const SHARD_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-language grammar/extractor salt (see the "Key" doc above). Hand-
/// maintained, mirroring `sem-core/Cargo.toml`'s pinned
/// `tree-sitter-*`/`ts-parser-*` dependency versions for languages the
/// generic tree-sitter `code` plugin handles (`languages.rs`'s
/// `LanguageConfig::id`), plus a synthetic version tag for the hand-rolled
/// (non-tree-sitter) plugins. **Not** derived automatically: Rust has no
/// stable way to read a dependency's resolved version at compile time
/// without a build script this crate doesn't carry. Whoever bumps a
/// grammar dependency in `Cargo.toml` — or changes a hand-rolled plugin's
/// extraction shape — must bump this table's entry for that language too;
/// this is the same hand-bump discipline `FACTS_SCHEMA_VERSION` already
/// uses, just scoped per language instead of crate-wide. Forgetting to bump
/// an entry means that language's stale facts could be served from the
/// corpus across the grammar bump — a real, documented risk of this design,
/// not a silently-solved problem.
///
/// The bump is not only for grammar/extractor changes: the same salt-bump discipline
/// generalizes it to *any* change in what a language's producer puts into
/// `PrecomputedFileFacts` for a `content_hash` this table's salt already
/// covers — because corpus dedup is first-writer-wins, a warm corpus entry
/// written by the old producer is never regenerated by a build that merely
/// upgrades `sem-core`, it is silently reused. `typescript`/`tsx`/`javascript`
/// were bumped (grammar-unchanged, hence the suffix rather than a new
/// tree-sitter version) by a fix that corrected
/// `precompute_js_ts_file_facts`'s `scopes[0].defs` seed order (extraction
/// order → `entity_ranges` order): a real but narrow
/// producer-output change (only 10+-way same-line, same-span, same-name
/// top-level collisions can differ), still enough to require invalidating
/// any warm corpus entry for these three languages.
///
/// `cpp`/`csharp` were bumped (as part of MUL Phase 1) for
/// the same reason at a larger scale: before this change, pass 1's chunked
/// closure (`graph.rs`) never precomputed facts for these two languages at
/// all -- every `.cs`/`.cpp` `CorpusFile` ever written carries
/// `precomputed: None`. This change makes pass 1 attempt
/// `precompute_scope_resolvable_file_facts` for both (gated per file by
/// `TREELESS`), so a file that previously got
/// `None` can now get `Some`. Corpus dedup is first-writer-wins,
/// so without this bump every pre-existing `.cs`/`.cpp` corpus entry would
/// silently keep denying the new facts a slot forever -- exactly the hazard
/// the salt-bump discipline exists to prevent. Python/Go/Java/Rust are intentionally *not* bumped here: this
/// change's `graph.rs` admission test dispatches only `csharp`/`cpp` to the
/// new precompute function (those four families are NO-GO as-is), so their producer output is
/// unchanged.
///
/// `yaml` was bumped `handrolled-1` -> `handrolled-2`:
/// a multi-document YAML file's same-named top-level keys in different
/// documents used to collapse onto one `build_entity_id` output (a real
/// oracle failure — 11 "Args" entities sharing one id in an llvm fixture,
/// TESTS_ORACLE), making which document's data (including
/// `is_test`) a corpus-wide id collision kept depend on processing order.
/// The fix (`plugins/yaml.rs`) disambiguates colliding ids by document index
/// via `build_entity_id_disambiguated_by_document`, which changes the `id`
/// field of every `SemanticEntity` a multi-document YAML file with a
/// colliding key produces — and `FileFacts.entities` carries that `id`
/// verbatim into the corpus, so an old-salt entry for such a file is exactly
/// the same hazard. Single-document files (and multi-document files with no
/// colliding key) keep the same ids, but the bump has no cheaper-than-whole-
/// language granularity, so it covers all `yaml`/`.yml` entries.
///
/// This table is `pub` so `sem-cli`'s `facts_remote.rs` — the
/// same crate graph, version-locked to this `sem-core` via a path
/// dependency — consumes it directly instead of hand-mirroring a second
/// copy that can silently drift from this one.
/// `examples/facts_corpus_probe.rs` keeps its own copy deliberately (see
/// that file's doc comment on its mirror): it is simulating an independent
/// uploading client for the `ingest_remote` wire-protocol test, so computing
/// the salt from this same table there would make that test tautological.
/// That mirror instead carries its own test asserting byte-equality against
/// this table, so drift between the two is a build failure, not a silent
/// divergence.
pub const LANGUAGE_SALTS: &[(&str, &str)] = &[
    // Entity byte spans for exported TS/JS declarations (export function,
    // export default class, export const, ...) used to start_byte right
    // after the `export`/`export default` keywords while start_line already
    // pointed at that same line -- a byte-precise read of an entity and a
    // line-based read of the same entity disagreed about where it began.
    // Fixed by extending the span backward to the wrapping
    // `export_statement` node's own start. This changes the
    // `start_byte`/`start_line` fields `FileFacts.entities` carries into the
    // corpus for every exported declaration in these three languages, so a
    // pre-fix corpus entry would silently keep serving the wrong span
    // forever (dedup is first-writer-wins) without this bump.
    ("typescript", "ts-0.23-u16-exportspan"),
    ("tsx", "ts-0.23-u16-exportspan"),
    ("javascript", "ts-0.23-u16-exportspan"),
    // MUL: python's producer now emits populated
    // import_stmts (Field 10) *and* ctor_call_sites (Field 11) — bumped from
    // "ts-0.23" following rust's/go's/java's salt-bump precedent. Shipped
    // unconditionally (maxRSS -7.95%/-7.80% on home-assistant/core, a
    // net decrease) and reconfirmed by a follow-up re-check (maxRSS -1.63% median, still
    // negative), but a 2026-08-22 re-verification found peak memory footprint —
    // the now-corrected ceiling metric — reads +25.29-27.44% against the
    // same +15% ceiling, above it. Demoted to gated behind `SEM_MUL_PYTHON`
    // (`MUL_RUNTIME_GATES`'s "python" row carries the pre-switch salt,
    // "ts-0.23"); this table's salt is unchanged — it now serves as the
    // switched-*on* salt, C++'s/rust's/java's shape (go's too, until Go was
    // admitted unconditionally and removed its switch
    // entirely — see below).
    ("python", "ts-0.23-mp4"),
    // MUL: go's producer now emits populated
    // import_stmts (Field 10) — bumped from "ts-0.23" following the same
    // salt-bump discipline as rust's mp2 bump. Gated behind `SEM_MUL_GO`
    // (`MUL_RUNTIME_GATES`'s "go" row carries the pre-switch salt,
    // "ts-0.23"), and must STAY gated: its memory check passed but
    // `edge_dump_probe` found a real, deterministic correctness regression
    // on kubernetes (not bit-identical ON vs OFF) — see
    // `mul_precompute_admits`'s doc comment.
    //
    // mp3 -> mp5: fixed half of that regression —
    // `GoImport::packages` now carries each spec's *full* import path
    // instead of a bare last-`/`-segment reduction, so `register_go_
    // package_imports` can disambiguate same-named packages by declaring
    // directory (kubernetes has dozens of directories literally named
    // `v1`). Producer-visible (the stored `packages` strings' content
    // changed) but not a shape change (`Vec<String>` throughout), so this
    // is a salt-bump case, not a `FACTS_SCHEMA_VERSION` bump — same
    // category as this table's other content-only producer bumps. Kept
    // even though the switch stays off in production (must not be flipped
    // — see below): a stale mp3-salted entry from a local SEM_MUL_GO=1
    // debugging session must not silently answer a post-fix lookup.
    // Correctness is *closer* but not closed: fixing this collision
    // collapsed a large class of cross-package false-positive edges
    // (kubernetes's own OFF-path edge count dropped ~9%, all confirmed
    // spurious), but `edge_dump_probe` ON vs OFF is still not
    // bit-identical — a second, still-unfixed mechanism (see
    // `mul_precompute_admits`'s doc comment) means the switch just must
    // not be flipped in production until that regression is root-caused
    // and fixed.
    //
    // mp5 -> mp5-dm5t: fixed the mechanism `mul_precompute_
    // admits`'s doc comment above named as "(2), not fixed" — id-staleness.
    // `registry::resolve_go_method_parent_ids` rewrites a cross-file Go
    // method's `id`/`parent_id`, but ran *after* pass 1 had already keyed
    // that file's `PrecomputedFileFacts.entity_scope_map`/
    // `entity_inner_scope`/`return_type_map` by the pre-rewrite id — a
    // pass-2 lookup by the post-rewrite id missed, silently defaulting to
    // scope 0 (`ENTITY_SCOPE_LOOKUP`'s honest-miss counter: kubernetes
    // fallback_pct 27.10% -> 0.00%). The fix re-keys this build's fresh
    // facts for exactly the files the rewrite touched
    // (`GoParentsResolved::rekeyed_ids`/`rekeyed_files`,
    // `PrecomputedFileFacts::rekey_entity_ids`) immediately after the
    // rewrite runs, and additionally cascades a rewritten method's new id
    // down through every descendant whose `parent_id` embedded the old id
    // as a literal prefix (`build_entity_id`'s own contract) — previously
    // only the method's own id/parent_id were rewritten, leaving nested
    // locals' `parent_id` dangling. Both are `SemanticEntity`/
    // `PrecomputedFileFacts` content changes for `.go` files specifically
    // (`is_go_file` guards every mutation both fixes make), hence this
    // entry's bump, not a table-wide one — kept even though the switch
    // stays off in production, same discipline as every prior bump on
    // this entry. `edge_dump_probe` ON vs OFF on kubernetes: bit-identical
    // (0-line diff, was 30,795) for the method-id-rewrite mechanism this
    // fix targets. A second, independent, much smaller mechanism (the
    // registration-gap species is chasing, unrelated to Go's id
    // rewrite) is still open at this point — see that change.
    //
    // mp5-dm5t -> mp5-dm5t-bpn2 (+ the Go memory-check work):
    // closed the registration-gap species named just above
    // (function-nested entities never entering any scope's `.defs`) for
    // every language at once, including Go — `ENTITY_SCOPE_LOOKUP`'s
    // `fallback_pct` collapsed 14.01% -> 0.00% on kubernetes. Combined
    // with the id-rekey fix above, both known correctness species were
    // closed — but the Go memory-check work's own precondition check (before
    // trusting that "closed" claim enough to admit Go) found a third,
    // inverted one: `edge_dump_probe` ON vs OFF on kubernetes was *still*
    // not bit-identical (331,120 vs 331,117), 3 dangling edges pointing at
    // ids no entity held. Root cause: `PrecomputedFileFacts::rekey_entity_
    // ids` (the mp5-dm5t fix above) rekeyed `entity_scope_map`/
    // `entity_inner_scope`/`return_type_map`'s keys but never revisited
    // `Scope::defs`' values or `Scope::owner_id` — the two other places a
    // `Scope` caches an entity id, both populated by the same registration
    // loops reinstated. Fixed by walking `self.scopes` too.
    // Producer-visible (`.defs`/`owner_id` values a corpus-cached
    // `PrecomputedFileFacts` carries can now differ from a pre-fix build's)
    // but not a shape change, so it's a bump case again. `edge_dump_probe`
    // ON vs OFF on kubernetes: bit-identical (331,117 edges both sides).
    // Go's correctness blocker chain is now fully closed, and its memory
    // check (three order-swapped pairs, kubernetes, both `/usr/bin/time -l`
    // fields) cleared the +15% ceiling (+6.78% to +8.46% peak footprint,
    // maxRSS flat) — so unlike every prior bump on this entry, this one
    // ships with the switch removed, not kept off: Go is admitted
    // unconditionally, [`crate::parser::scope_resolve::MUL_RUNTIME_GATES`]
    // no longer carries a "go" row, and this table's salt is what every
    // build now writes under, not a switched-*on* value waiting for a
    // switch. Bumped regardless of that, per the same discipline: a
    // pre-bpn2 corpus entry (written under a local `SEM_MUL_GO=1`
    // debugging session before this fix existed) must not silently answer
    // a post-fix lookup now that the enriched path runs on every build.
    ("go", "ts-0.23-mp5-dm5t-bpn2"),
    // MUL: rust's producer now emits populated
    // import_stmts (Field 10) — bumped from "ts-0.23" following
    // the same salt-bump precedent. Shipped unconditionally
    // (+11.16%/+11.28% against the +15% ceiling) but a later
    // same-binary re-verification found the ceiling actually busted
    // (+17.7-19.6%) — demoted to gated behind `SEM_MUL_RUST`
    // (`MUL_RUNTIME_GATES`'s "rust" row carries the pre-switch salt,
    // "ts-0.23"); this table's salt is unchanged — it now serves as the
    // switched-*on* salt, C#'s/java's shape (go's too, until Go was
    // admitted unconditionally — see above).
    ("rust", "ts-0.23-mp2"),
    // MUL: java's imports classify as GoImport
    // and are now descriptor-dispatched too — the same salt-bump
    // as go's original one (go's own entry is now several bumps further —
    // see above). Correctness is clean (bit-identical edge_dump_probe on
    // elasticsearch, full oracle battery) but it busted its own +15%
    // peak-RSS ceiling (+20.97%/+21.01%, both pairs) — gated behind
    // `SEM_MUL_JAVA`, pre-switch salt "ts-0.23", C#'s shape.
    ("java", "ts-0.23-mp3"),
    ("c", "ts-0.23"),
    // MUL phase 1: C++'s producer's Field-10-era bump. Shipped
    // unconditionally (+5.8%/+6.5% against the +15% ceiling,
    // without a corrected `--no-cache`/fresh-`SEM_CACHE_DIR` protocol) but
    // a 2026-08-22 corrected-protocol re-verification found both
    // fields bust the ceiling on llvm-project (maxRSS +19.98-21.02%,
    // footprint +26.33-28.11%). Demoted to gated behind `SEM_MUL_CPP`
    // (`MUL_RUNTIME_GATES`'s "cpp" row carries the pre-switch salt,
    // "ts-0.23"); this table's salt is unchanged — it now serves as the
    // switched-*on* salt, C#'s/rust's/java's/python's shape (go's too,
    // until Go was admitted unconditionally — see above).
    ("cpp", "ts-0.23-mp1"),
    ("ruby", "ts-0.23"),
    ("csharp", "ts-0.23-mp1"),
    ("php", "ts-0.23"),
    ("fortran", "ts-0.5"),
    ("swift", "ts-0.7"),
    ("elixir", "ts-0.3"),
    ("bash", "ts-0.23"),
    ("hcl", "ts-1.1"),
    ("kotlin", "ts-1.1"),
    ("xml", "ts-0.7"),
    ("dart", "ts-0.2.0"),
    ("perl", "ts-1.1"),
    ("sql", "ts-0.3"),
    ("ocaml", "ts-0.24"),
    ("ocaml_interface", "ts-0.24"),
    ("scala", "ts-0.26"),
    ("zig", "ts-1.1.2"),
    ("nix", "ts-0.3"),
    ("haskell", "ts-0.23"),
    ("elm", "ts-5.9"),
    ("edn", "ts-0.2.5"),
    ("clojure", "ts-0.2.5"),
    ("d", "ts-0.8.2"),
    ("lua", "ts-0.5.0"),
    ("fish", "ts-3.6"),
    ("svelte", "ts-0.1.7"),
    ("erb", "ts-0.25"),
    ("toml", "handrolled-1"),
    // csv/json/vue were bumped `handrolled-1` -> `handrolled-2`,
    // the same salt-bump species as yaml's bump below: only the code plugin's
    // tree-sitter extractor disambiguated colliding entity ids within one
    // file (an `@L{line}` suffix); every other hand-rolled plugin called
    // `build_entity_id` with no post-pass, so two CSV rows sharing a
    // first-cell value, two JSON object entries sharing a top-level key, or
    // two `<style>` blocks in one Vue SFC (a common real pattern —
    // `<style scoped>` next to a plain `<style>`) produced the same id and
    // silently collided in `EntityGraph::build`'s `entity_map`. The fix
    // (`model/entity.rs`'s `disambiguate_colliding_entity_ids`, the same
    // function the code plugin already used, now also run at every
    // extractor boundary — `registry.rs`, `differ.rs`, `diff_oracle.rs` —
    // plus directly inside `csv_plugin.rs`/`json.rs`) changes the `id` field
    // only for files that actually collide; a well-formed file with no
    // colliding names keeps the same id (verified: bit-identical entity ids
    // before/after on a real corpus with no colliding CSV/JSON/Vue content).
    // `toml`/`fallback`/`erb` are not bumped: TOML's own parser already
    // rejects duplicate keys before an id is ever built, fallback's chunk
    // names are monotonic line ranges that cannot repeat, and erb already
    // disambiguates block/expression names itself (`unique_name`) — none of
    // the three can produce a colliding id, pre- or post-fix.
    ("csv", "handrolled-2"),
    ("json", "handrolled-2"),
    ("yaml", "handrolled-2"),
    ("markdown", "handrolled-1"),
    ("latex", "handrolled-1"),
    ("vue", "handrolled-2"),
    ("fallback", "handrolled-1"),
];

/// Salt for any language id not (yet) in `LANGUAGE_SALTS` — conservative
/// but safe: unmapped languages still only ever share among themselves,
/// never with a mapped one, and adding a table entry later invalidates only
/// that one language's prior corpus entries (its salt string changes).
const DEFAULT_LANGUAGE_SALT: &str = "unmapped-1";

fn language_salt(lang_id: &str) -> &'static str {
    LANGUAGE_SALTS
        .iter()
        .find(|(id, _)| *id == lang_id)
        .map(|(_, salt)| *salt)
        .unwrap_or(DEFAULT_LANGUAGE_SALT)
}

/// [`language_salt`], corrected for a producer switch that is decided at run
/// time rather than by the table — every language registered in
/// [`crate::parser::scope_resolve::MUL_RUNTIME_GATES`] is gated behind an
/// opt-in env var because its precompute path costs more peak memory than
/// the +15% ceiling allows: C#; Java (measured over its ceiling); Rust
/// (+17.7-19.6% against +15%, after an earlier +11% reading had shipped it
/// unconditionally and a re-verification found it worse); and C++/Python,
/// both measured against peak memory footprint (compressed-page-aware)
/// rather than plain maxRSS — C++'s maxRSS itself busts the ceiling too at
/// +19.98-21.02%, Python's maxRSS stays negative but its footprint reads
/// +25.29-27.44% — see
/// [`crate::parser::scope_resolve::mul_precompute_admits`]). Go's
/// precompute was gated too (memory was fine but its edges weren't
/// bit-identical on kubernetes, a correctness regression) until that
/// regression was closed and the memory check cleared on the corrected
/// metric too — it is unconditional now, the only precompute language that
/// is, and has no row in this table.
///
/// The salt names **the producer that wrote the entry**, so a switch that
/// changes the producer has to move the salt with it — in *both* directions.
/// With a registered language's switch off, pass 1 emits `precomputed: None`
/// exactly as a pre-switch binary does, so the honest salt is the gate's
/// `pre_switch_salt`: the two builds then share corpus entries, which is
/// correct (their output is identical) and is what makes "off" a true revert
/// rather than a fresh cache generation. With the switch on, the table's
/// current salt isolates the richer entries from those `None`s — without
/// this, first-writer-wins would let a switched-off build's `None` entries
/// permanently deny the facts a slot — the same hazard the salt-bump
/// discipline exists to prevent, applied to a switch instead of a version.
///
/// The lookup itself is [`resolve_gated_salt`], kept separate so it is
/// testable against a synthetic gate — this function's own inputs
/// (`MUL_RUNTIME_GATES`, `mul_precompute_admits`) are process-global and, for
/// the switch, cached in a `OnceLock` for the process's lifetime, so no test
/// can flip them mid-run to prove the mechanism generalizes beyond the one
/// language currently registered.
///
/// [`corpus_identity_salt`] deliberately does *not* track any of this: it
/// stamps sibling artifacts (the query index) whose content — entities,
/// edges, edge hashes — measured bit-identical either way, so a
/// memory switch must not invalidate them.
fn producer_language_salt(lang_id: &str) -> &'static str {
    resolve_gated_salt(
        crate::parser::scope_resolve::MUL_RUNTIME_GATES,
        crate::parser::scope_resolve::mul_precompute_admits,
        lang_id,
        language_salt(lang_id),
    )
}

/// Pure core of [`producer_language_salt`]: given a table of runtime-gated
/// languages, an admission predicate, and the table's own (post-switch)
/// salt for `lang_id`, decide which salt an entry for `lang_id` must be
/// keyed under right now. Decoupled from the real global gate table and the
/// real (env-var-cached) admission function precisely so a test can inject a
/// synthetic gate — see `facts_store::corpus_tests::resolve_gated_salt_generalizes_beyond_csharp`,
/// which proves this handles a *second* runtime-gated language, something the
/// single hand-written `if lang_id == "csharp"` branch this replaces never
/// could ("structural, not remembered" ask).
fn resolve_gated_salt(
    gates: &[crate::parser::scope_resolve::MulRuntimeGate],
    admits: impl Fn(&str) -> bool,
    lang_id: &str,
    table_salt: &'static str,
) -> &'static str {
    match gates.iter().find(|g| g.lang_id == lang_id) {
        Some(gate) if !admits(lang_id) => gate.pre_switch_salt,
        _ => table_salt,
    }
}

/// The salt a corpus entry is actually written and looked up under: the
/// per-language grammar salt, plus the identity of whatever fast extractor
/// produced (or would produce) the entities.
///
/// Two extractor generations must never satisfy each other's lookups. They
/// legitimately disagree on `structural_hash` conventions, on kappa values
/// (which are grammar-shaped, a known limitation), and, while a fast
/// extractor is still being proven, on entity sets. The corpus already
/// isolates by grammar version with exactly this mechanism, so extractor
/// identity belongs in the same string rather than in a second version knob:
/// one comparison, one invalidation story, and the cross-machine tiers
/// inherit it for free because `ingest_remote` validates the claimed salt
/// against this function's output.
///
/// With no fast extractor installed — the default, and every build without
/// the `oxc-fastpath` feature — this is byte-identical to
/// [`language_salt`], so no existing corpus entry is invalidated.
///
/// `pub` so `sem-cli`'s `facts_remote.rs` computes the exact same
/// key sem-core's own `FactsCorpus` would, including the MUL-phase-1
/// producer-switch correction and the fast-extractor identity suffix,
/// without re-deriving either from a second copy.
pub fn effective_language_salt(lang_id: &str) -> String {
    salt_with_extractor(
        producer_language_salt(lang_id),
        crate::parser::fast_extractor::identity_salt().as_deref(),
    )
}

/// One number standing for "which extractor semantics produced these
/// entities": the schema version, the crate salt, every per-language grammar
/// salt, and the fast-extractor identity, folded in table order.
///
/// It lives here rather than in the consumer because [`LANGUAGE_SALTS`] is
/// this module's table — a caller that folded it itself would silently stop
/// tracking new entries. Sibling artifacts derived from the same extraction
/// stamp this instead of introducing a second version
/// knob to forget to bump.
pub(crate) fn corpus_identity_salt() -> u64 {
    let mut h = Xxh3::new();
    h.update(&FACTS_SCHEMA_VERSION.to_le_bytes());
    h.update(sem_core_salt().as_bytes());
    h.update(DEFAULT_LANGUAGE_SALT.as_bytes());
    for (lang, salt) in LANGUAGE_SALTS {
        h.update(lang.as_bytes());
        h.update(salt.as_bytes());
    }
    if let Some(identity) = crate::parser::fast_extractor::identity_salt() {
        h.update(identity.as_bytes());
    }
    h.digest()
}

/// The pure half of [`effective_language_salt`], so the composition rule is
/// testable without touching the process-global extractor switch.
fn salt_with_extractor(base: &str, identity: Option<&str>) -> String {
    match identity {
        Some(identity) => format!("{base}+{identity}"),
        None => base.to_string(),
    }
}

/// The fine-grained per-language id this module keys the corpus by.
/// Prefers `languages.rs`'s `LanguageConfig::id` (distinguishes every
/// tree-sitter language that the generic `code` plugin's single
/// `SemanticParserPlugin::id() == "code"` would otherwise collapse into one
/// salt) and falls back to the matched plugin's own id for everything else
/// (markdown/json/yaml/... and `fallback`).
fn detect_language_id(file_path: &str, registry: &ParserRegistry) -> String {
    if let Some(file_name) = Path::new(file_path).file_name().and_then(|n| n.to_str()) {
        let lower = file_name.to_lowercase();
        // Longest-suffix-first (".d.ts" must match before ".ts"), mirroring
        // `registry.rs`'s own `get_extensions` convention.
        let dot_positions: Vec<usize> = lower
            .char_indices()
            .filter(|&(_, c)| c == '.')
            .map(|(i, _)| i)
            .collect();
        for idx in dot_positions {
            if let Some(cfg) = get_language_config(&lower[idx..]) {
                return cfg.id.to_string();
            }
        }
    }
    registry
        .get_explicit_plugin(file_path)
        .map(|p| p.id().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn corpus_bucket(relative_path: &str) -> u64 {
    let mut h = Xxh3::new();
    h.update(relative_path.as_bytes());
    h.digest() % CORPUS_BUCKETS
}

/// The *within-shard* key: a hash of the full lookup identity, not just the
/// path `corpus_bucket` partitions on. This is what a v2 shard's index is
/// sorted and searched by (see the "Shard layout v2" doc above). Fields are
/// separated by a zero byte so `("a/b", h, "s")` and `("a", h, "b/s")` cannot
/// hash alike through concatenation.
///
/// A collision is never a wrong answer: read still verifies every candidate
/// with [`corpus_matches`], write still only ever declines to *add* an entry.
fn corpus_key_hash(relative_path: &str, content_hash: u64, lang_salt: &str) -> u64 {
    let mut h = Xxh3::new();
    h.update(relative_path.as_bytes());
    h.update(&[0]);
    h.update(&content_hash.to_le_bytes());
    h.update(&[0]);
    h.update(lang_salt.as_bytes());
    h.digest()
}

/// One v2 index record: where a key's CBOR payload lives inside the shard's
/// payload region (`offset` is relative to the region's start, not the file's).
#[derive(Clone, Copy)]
struct ShardEntry {
    key_hash: u64,
    offset: u64,
    len: u64,
}

/// A v2 shard's index, read without touching the payload region.
struct ShardIndex {
    /// `key_hash`-sorted, so [`Self::candidates`] is a binary search.
    entries: Vec<ShardEntry>,
    /// File offset the payload region starts at.
    payload_start: u64,
    /// Total payload bytes, i.e. the region's length.
    payload_len: u64,
    /// Bytes actually read off disk to obtain this index — head probe plus
    /// index records. Reported through [`CorpusLookupStats::bytes_read`].
    bytes_read: u64,
}

impl ShardIndex {
    /// Every record with this exact `key_hash` — normally zero or one, more
    /// only under a collision, which the caller still resolves by decoding
    /// and checking each candidate.
    fn candidates(&self, key_hash: u64) -> &[ShardEntry] {
        let lo = self.entries.partition_point(|e| e.key_hash < key_hash);
        let hi = self.entries.partition_point(|e| e.key_hash <= key_hash);
        &self.entries[lo..hi]
    }
}

/// `true` iff `file` is genuinely the entry for `(relative_path,
/// content_hash, lang_salt)` — checked field-by-field on every candidate
/// rather than trusted from bucket placement alone, so a hash collision in
/// `corpus_bucket` (vanishingly unlikely, but the frame rule's "under-
/// approximating is the one unforgivable failure" applies here too) can
/// only ever surface as a false miss (falls back to extraction — always
/// safe), never a wrong-facts hit.
fn corpus_matches(
    file: &CorpusFile,
    relative_path: &str,
    content_hash: u64,
    lang_salt: &str,
) -> bool {
    file.facts.path == relative_path
        && file.facts.content_hash == content_hash
        && file.lang_salt == lang_salt
}

/// One file's cross-repo-shareable bundle: extracted entities and JS/TS
/// precomputed scope facts, deliberately **never** cached resolution (see
/// "What is NOT shared cross-repo" above). `lang_salt` is carried alongside
/// `facts` so a lookup can verify it directly rather than re-deriving it
/// from `facts.path`'s extension — cheaper, and robust to a future
/// path/extension-detection change not being retroactively applied to
/// already-written shards (an old entry's stored salt always reflects the
/// salt scheme that actually wrote it, so it simply stops matching new
/// lookups instead of silently reinterpreting itself under a new scheme).
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CorpusFile {
    pub(crate) facts: FileFacts,
    pub(crate) precomputed: Option<PrecomputedFileFacts>,
    pub(crate) lang_salt: String,
}

/// Borrowing serialize-only twin of [`CorpusFile`].
///
/// [`FactsCorpus::populate_delta`] used to deep-clone every changed file's
/// `facts` (entity bodies included) and `precomputed` (the file's whole
/// source text) into an owned [`CorpusFile`] whose only consumer was the
/// shard serializer — on a true-cold giant build that clone was total and
/// transient, one of the three corpus copies behind the facts plane's
/// measured RSS band. serde encodes
/// `&T` byte-identically to `T` and field order below matches `CorpusFile`
/// exactly, so shard bytes are unchanged — gated by `facts_corpus_probe`'s
/// oracles and the shard-byte tests in this module.
///
/// `lang_salt` stays owned: it is freshly derived per call
/// ([`effective_language_salt`] returns an owned `String`), never cloned out
/// of corpus-sized state.
#[derive(Serialize)]
pub(crate) struct CorpusFileRef<'a> {
    pub(crate) facts: FileFactsRef<'a>,
    pub(crate) precomputed: Option<&'a PrecomputedFileFacts>,
    pub(crate) lang_salt: String,
}

impl<'a> CorpusFileRef<'a> {
    fn of(file: &'a CorpusFile) -> Self {
        CorpusFileRef {
            facts: FileFactsRef {
                path: &file.facts.path,
                content_hash: file.facts.content_hash,
                entities: &file.facts.entities,
            },
            precomputed: file.precomputed.as_ref(),
            lang_salt: file.lang_salt.clone(),
        }
    }
}

/// Diagnostics from one [`FactsCorpus::merge_with_local`] call. Exposed for
/// the cross-repo proof probe (`examples/facts_corpus_probe.rs`) and tests;
/// never consulted by any correctness logic.
#[derive(Debug, Default, Clone, Copy)]
pub struct CorpusLookupStats {
    /// Files `local` had no entry for at all — the only files this call
    /// ever touches disk for (see "Storage and concurrency" above).
    pub probed: usize,
    /// Of `probed`, how many matched a corpus entry.
    pub hits: usize,
    /// Distinct shard files opened to answer `probed`. Saturates at
    /// `CORPUS_BUCKETS` on any repo with more files than buckets — see
    /// `bytes_read`, which is the number that actually tracks work.
    pub shards_read: usize,
    /// Bytes actually read off disk to answer `probed`: every shard's header
    /// and index, plus only the payload ranges the index said could hold a
    /// probed key. Under the v1 layout this was necessarily the
    /// whole corpus; it is now proportional to hits, which is what makes a
    /// build's cost independent of how much unrelated content the machine has
    /// stored.
    pub bytes_read: u64,
}

/// Diagnostics from one [`FactsCorpus::populate_delta`] call.
#[derive(Debug, Default, Clone, Copy)]
pub struct CorpusPopulateStats {
    /// Files actually written into the corpus this call: new or changed
    /// since `previous` (see `populate_delta`'s doc for why unchanged/GREEN
    /// files are skipped) **and** not already stored under the same key
    /// (— a second build of the same content re-derives the same
    /// entries and the shard index already holds them, so nothing is
    /// rewritten and this reads 0).
    pub files_written: usize,
    /// Distinct shard files rewritten. Shards whose every candidate entry was
    /// already stored are not counted, because they are not touched.
    pub shards_written: usize,
}

// =============================================================================
// External ingestion: the cloud tier's local trust boundary
// =============================================================================
//
// Everything above this point ever writes a `CorpusFile` this *same* process
// derived from its own local read+hash pass (`populate_delta`) — trustworthy
// by construction, because the process that computed `facts.path`/
// `facts.content_hash`/`lang_salt` is the same process about to store them
// under that key. A cloud client (`sem-cli`'s `facts_remote.rs`, 's
// cloud half) breaks that assumption: it has a `FileFacts` payload decoded
// off the wire, plus the `(relative_path, content_hash, language_salt,
// schema_version)` key it fetched/queried that payload *under* — and no
// guarantee the two agree. A compromised, buggy, or merely out-of-sync
// server could hand back entities for the wrong file, or a downloader could
// mis-plumb which payload pairs with which key. Because the corpus is
// machine-global (`FactsCorpus` is shared across every repo `sem` ever
// builds on this machine, not scoped to the download that populated it), a
// single mis-keyed entry here does not just corrupt one build — it sits in
// a shard waiting to serve *any future repo's* lookup for that key, which is
// exactly the "silently wrong edges" failure the frame rule calls
// unforgivable. `ingest_remote` is the checkpoint that stands between "facts
// arrived over the network" and "facts a shard will ever serve."

/// One externally-sourced file's facts, ready for
/// [`FactsCorpus::ingest_remote`] — the shape a cloud client (`sem-cli`'s
/// `facts_remote.rs`) has in hand after a successful download: the file's
/// own extracted facts as decoded from the wire (`facts`, `precomputed`),
/// plus the key the client fetched/queried it *under* (`claimed_*`).
/// `ingest_remote` never trusts `claimed_*` on its own — every field is
/// re-checked against `facts` itself (and, for the salt, against this
/// process's own `LANGUAGE_SALTS` table) before a single byte reaches a
/// shard. `precomputed` is `None` for every fact sourced from today's cloud
/// protocol (`FACTS-SERVICE.md`'s wire format carries only entities, not
/// JS/TS scope facts); the field exists so a `RemoteFact` is shaped
/// identically to a locally-derived [`PersistedFile`]/[`CorpusFile`] the
/// moment a future protocol version does carry it, without a second
/// ingestion path.
#[derive(Clone)]
pub struct RemoteFact {
    pub facts: FileFacts,
    pub precomputed: Option<PrecomputedFileFacts>,
    pub claimed_relative_path: String,
    pub claimed_content_hash: u64,
    pub claimed_language_salt: String,
    pub claimed_schema_version: u32,
}

/// Why [`FactsCorpus::ingest_remote`] refused one [`RemoteFact`]. Every
/// variant names the exact field disagreement between the key a fact claims
/// to have been fetched under and what the fact's own payload (or this
/// process's own tables) actually say — never collapsed into one generic
/// "invalid fact" reason, because the frame rule's "under-approximating is
/// the one unforgivable failure" means a caller must be able to tell
/// "genuinely unknown, fall back to extraction" apart from "known, safe to
/// reuse," and a precise reason is what makes that distinction auditable
/// (`SEM_FACTS_DEBUG`-style diagnostics, or this change's tamper-rejection
/// proof) rather than merely asserted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IngestError {
    /// The claimed `relative_path` disagrees with the payload's own
    /// `FileFacts::path` — the wire key and the wire body were fetched (or
    /// paired) inconsistently.
    #[error("claimed relative_path {claimed:?} does not match the fact's own path {actual:?}")]
    PathMismatch { claimed: String, actual: String },
    /// The claimed `content_hash` disagrees with the payload's own
    /// `FileFacts::content_hash` — exactly the tamper shape this change's E2E
    /// proof exercises: a payload whose entities were computed for
    /// different bytes than the key advertises.
    #[error(
        "claimed content_hash {claimed} does not match the fact's own content_hash {actual} \
         (path {path:?})"
    )]
    ContentHashMismatch {
        path: String,
        claimed: u64,
        actual: u64,
    },
    /// The claimed `language_salt` disagrees with what this process's own
    /// `LANGUAGE_SALTS` table computes for the path — either the sending
    /// client's salt table is stale relative to this one, or the key was
    /// tampered with.
    #[error(
        "claimed language_salt {claimed:?} does not match this process's own salt {actual:?} \
         for path {path:?}"
    )]
    LanguageSaltMismatch {
        path: String,
        claimed: String,
        actual: String,
    },
    /// The claimed `schema_version` is not this process's
    /// `FACTS_SCHEMA_VERSION` — a fact shaped for a different `sem-core`
    /// release must never be decoded into this one's corpus, exactly like a
    /// local store/shard load's own version guard (see this module's
    /// "Keying and versioning" doc).
    #[error(
        "claimed schema_version {claimed} does not match this process's FACTS_SCHEMA_VERSION \
         {actual} (path {path:?})"
    )]
    SchemaVersionMismatch {
        path: String,
        claimed: u32,
        actual: u32,
    },
}

/// Outcome of one [`FactsCorpus::ingest_remote`] call. A batch of
/// externally-sourced facts is never all-or-nothing — one tampered or
/// stale-keyed entry must not sink every other genuinely-good fact in the
/// same batch — but a rejection is never silently absorbed either: `rejected`
/// pairs every refused fact's path with the exact [`IngestError`] that sank
/// it, so a caller can log/report it (`SEM_FACTS_DEBUG`-style) rather than
/// have it vanish.
#[derive(Debug, Default)]
pub struct IngestOutcome {
    /// Same counters [`FactsCorpus::populate_delta`] reports, for whatever
    /// passed validation.
    pub accepted: CorpusPopulateStats,
    pub rejected: Vec<(String, IngestError)>,
}

/// A tiny advisory lock for one shard's read-merge-write, released on drop
/// (including on panic/unwind, so one poisoned build never wedges the next
/// writer). See "Storage and concurrency" above for why a bounded wait
/// beats blocking forever.
struct ShardLock {
    path: PathBuf,
    held: bool,
}

impl ShardLock {
    fn acquire(shard_path: &Path) -> Self {
        let path = shard_path.with_extension("factshard.lock");
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Self { path, held: true },
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > SHARD_LOCK_TIMEOUT {
                        // Best-effort: proceed unlocked rather than wedge a
                        // build forever on a stale lock from a crashed
                        // process. Worst case is a lost update (never
                        // corruption — writes are still temp-then-rename).
                        return Self { path, held: false };
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return Self { path, held: false },
            }
        }
    }
}

impl Drop for ShardLock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A machine-global, cross-repo facts corpus. No ambient path,
/// same discipline as [`FactsStore`]: `dir` always comes from the caller —
/// wiring the *one* real caller (`sem-cli`'s graph/diff path), including
/// where the directory defaults to and how it is disabled, lives in
/// `sem-cli`, not here.
pub struct FactsCorpus {
    dir: PathBuf,
}

impl FactsCorpus {
    /// Open a corpus rooted at `dir`. Does not create the directory or
    /// touch disk — that happens lazily on the first `populate_delta`.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn shard_path(&self, bucket: u64) -> PathBuf {
        self.dir.join(format!("shard-{bucket:04}.factshard"))
    }

    /// `true` only when this corpus directory is *provably* empty — does
    /// not exist, or exists with zero entries. One cheap `read_dir` that
    /// short-circuits on its first entry, never a full listing.
    ///
    /// `merge_with_local`'s dominant cost on a never-populated
    /// corpus (a machine's first-ever build, or a fresh
    /// `SEM_FACTS_CORPUS_DIR`) was never the shard-open path — ws6's
    /// D11 measured that directly and declined both a presence manifest
    /// and blaming shard opens. It is the probe-construction pass itself:
    /// a full `read_to_string` + content hash + salt lookup for every
    /// candidate file, paid unconditionally before a single shard is ever
    /// consulted, entirely wasted when the corpus holds nothing a probe
    /// could possibly match. This check proves "nothing to match" in
    /// O(1) disk operations instead of O(candidate files), so
    /// `merge_with_local` can skip that pass outright. Any entry at all —
    /// a real shard, a stray lock file, anything — is treated as "might
    /// have data" and falls through to the exact, unchanged probe path:
    /// this only ever short-circuits the case where a hit is impossible
    /// by construction, never a case where one might exist.
    fn is_definitely_empty(&self) -> bool {
        match std::fs::read_dir(&self.dir) {
            Ok(mut entries) => entries.next().is_none(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            // Some other I/O condition (permission denied, a transient
            // mount error) — can't prove emptiness, so don't claim it;
            // fall through to the full path, which handles an unreadable
            // corpus gracefully on its own (each `open_shard` call simply
            // misses).
            Err(_) => false,
        }
    }

    /// Open one shard and read *only* its header and index — never the
    /// payload region. Any anomaly — missing file, bad magic, schema
    /// mismatch, a v1 shard, truncated/garbage bytes — is a clean miss
    /// (`None`), never a panic, exactly like `FactsStore::load`'s discipline.
    ///
    /// This is the function that makes a corpus consult cost what the caller
    /// asked for rather than what the machine has stored: everything below
    /// reaches the payload only through the ranges this index hands back.
    fn open_shard(&self, bucket: u64) -> Option<(File, ShardIndex)> {
        let mut file = File::open(self.shard_path(bucket)).ok()?;
        let file_len = file.metadata().ok()?.len();

        let head_len = SHARD_HEAD_PROBE.min(usize::try_from(file_len).ok()?);
        let mut head = vec![0u8; head_len];
        file.read_exact(&mut head).ok()?;
        if head.len() < CORPUS_MAGIC.len() || &head[..CORPUS_MAGIC.len()] != CORPUS_MAGIC {
            return None;
        }
        let mut rest: &[u8] = &head[CORPUS_MAGIC.len()..];
        let header: StoreHeader = ciborium::from_reader(&mut rest).ok()?;
        if header.schema_version != FACTS_SCHEMA_VERSION || header.sem_core_salt != sem_core_salt()
        {
            return None;
        }
        let (entry_count, _) = read_u64_le(rest)?;
        // Where `rest` starts inside `head`, plus the 8 bytes `entry_count`
        // itself occupies.
        let index_start = (head.len() - rest.len() + 8) as u64;
        let index_len = usize::try_from(entry_count)
            .ok()?
            .checked_mul(INDEX_RECORD_LEN)?;

        let mut index_bytes = vec![0u8; index_len];
        file.seek(SeekFrom::Start(index_start)).ok()?;
        file.read_exact(&mut index_bytes).ok()?;

        let mut entries: Vec<ShardEntry> = Vec::with_capacity(index_len / INDEX_RECORD_LEN);
        for rec in index_bytes.chunks_exact(INDEX_RECORD_LEN) {
            entries.push(ShardEntry {
                key_hash: u64::from_le_bytes(rec[0..8].try_into().ok()?),
                offset: u64::from_le_bytes(rec[8..16].try_into().ok()?),
                len: u64::from_le_bytes(rec[16..24].try_into().ok()?),
            });
        }
        // Written sorted; re-sorted only if a shard on disk somehow is not,
        // because `candidates`' binary search would otherwise silently
        // under-report (a false miss is safe, but a cheap check is cheaper
        // than the lost hits).
        if entries.windows(2).any(|w| w[0].key_hash > w[1].key_hash) {
            entries.sort_by_key(|e| e.key_hash);
        }

        let payload_start = index_start + index_len as u64;
        let payload_len = file_len.saturating_sub(payload_start);
        // Every recorded range must lie inside the payload region — a
        // truncated or doctored shard must not be able to make a read run off
        // the end.
        if entries
            .iter()
            .any(|e| e.offset.saturating_add(e.len) > payload_len)
        {
            return None;
        }
        Some((
            file,
            ShardIndex {
                entries,
                payload_start,
                payload_len,
                bytes_read: head.len() as u64 + index_len as u64,
            },
        ))
    }

    /// Read the payload bytes for `wanted` out of an already-open shard,
    /// returning them in the same order and the byte count actually read.
    ///
    /// Entries written by one build land contiguously in the payload region,
    /// so `wanted` is offset-sorted first and *exactly adjacent* ranges are
    /// coalesced into a single read. No over-read: a gap ends the run.
    fn read_payloads(
        file: &mut File,
        index: &ShardIndex,
        wanted: &[(usize, ShardEntry)],
    ) -> (Vec<(usize, Vec<u8>)>, u64) {
        let mut order: Vec<(usize, ShardEntry)> = wanted.to_vec();
        order.sort_by_key(|(_, e)| e.offset);

        let mut out: Vec<(usize, Vec<u8>)> = Vec::with_capacity(order.len());
        let mut bytes_read = 0u64;
        let mut i = 0usize;
        while i < order.len() {
            let start = order[i].1.offset;
            let mut end = start + order[i].1.len;
            let mut j = i + 1;
            while j < order.len() && order[j].1.offset == end {
                end += order[j].1.len;
                j += 1;
            }
            let run_len = match usize::try_from(end - start) {
                Ok(n) => n,
                Err(_) => break,
            };
            let mut buf = vec![0u8; run_len];
            if file
                .seek(SeekFrom::Start(index.payload_start + start))
                .is_err()
                || file.read_exact(&mut buf).is_err()
            {
                // Clean miss for this run, exactly like a decode failure.
                i = j;
                continue;
            }
            bytes_read += run_len as u64;
            for (probe_idx, entry) in &order[i..j] {
                let lo = (entry.offset - start) as usize;
                let hi = lo + entry.len as usize;
                out.push((*probe_idx, buf[lo..hi].to_vec()));
            }
            i = j;
        }
        (out, bytes_read)
    }

    /// Decode every entry in one shard. Only `get` (the single-key test
    /// helper) and the tests use this — the production read path is
    /// [`Self::open_shard`] plus [`Self::read_payloads`], which is the whole
    /// point of the v2 layout. Any anomaly is a clean miss (empty vec).
    #[cfg_attr(not(test), allow(dead_code))]
    fn load_shard(&self, bucket: u64) -> Vec<CorpusFile> {
        let Some((mut file, index)) = self.open_shard(bucket) else {
            return Vec::new();
        };
        let wanted: Vec<(usize, ShardEntry)> = index.entries.iter().copied().enumerate().collect();
        let (payloads, _) = Self::read_payloads(&mut file, &index, &wanted);
        payloads
            .into_iter()
            .filter_map(|(_, bytes)| ciborium::from_reader::<CorpusFile, _>(&bytes[..]).ok())
            .collect()
    }

    fn encode_corpus_file(file: &CorpusFileRef<'_>) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        ciborium::into_writer(file, &mut bytes).map_err(to_io_error)?;
        Ok(bytes)
    }

    /// The fixed head of a v2 shard: magic, header, `entry_count`.
    fn encode_shard_head(entry_count: usize) -> io::Result<Vec<u8>> {
        let header = StoreHeader {
            schema_version: FACTS_SCHEMA_VERSION,
            sem_core_salt: sem_core_salt().to_string(),
        };
        let mut bytes = Vec::with_capacity(CORPUS_MAGIC.len() + 128);
        bytes.extend_from_slice(CORPUS_MAGIC);
        ciborium::into_writer(&header, &mut bytes).map_err(to_io_error)?;
        write_u64_le(&mut bytes, entry_count as u64);
        Ok(bytes)
    }

    fn encode_shard_index(entries: &[ShardEntry]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(entries.len() * INDEX_RECORD_LEN);
        for e in entries {
            write_u64_le(&mut bytes, e.key_hash);
            write_u64_le(&mut bytes, e.offset);
            write_u64_le(&mut bytes, e.len);
        }
        bytes
    }

    /// Write `parts`, concatenated, to the shard for `bucket` via
    /// temp-file-then-`rename`. Taking parts rather than one buffer lets the
    /// write path stream a carried-forward payload region straight from the
    /// buffer it was read into, with no second copy.
    fn write_shard_atomic_parts(&self, bucket: u64, parts: &[&[u8]]) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.shard_path(bucket);
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = path.with_extension(format!(
            "factshard.tmp.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        // Write-then-`rename`, with no `fsync`: exactly the durability the v1
        // `std::fs::write` + `rename` pair had. A shard is a pure speed
        // optimization, so a shard lost to a power cut is a future cache miss
        // and nothing worse — and an `fsync` per shard measured 4,100 ms
        // against 400 ms across 1,024 shards on the monster, which is a real
        // cost for no guarantee this file needs.
        let result = (|| -> io::Result<()> {
            let mut f = File::create(&tmp)?;
            for part in parts {
                f.write_all(part)?;
            }
            drop(f);
            std::fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Point lookup for one `(relative_path, content_hash, language_salt)`.
    /// Used directly by tests and the negative-path proof;
    /// [`Self::merge_with_local`] is the batched, bucketed entry point real
    /// callers should use — this one exists for direct single-key
    /// assertions (see `corpus_tests` below), not as a hot path.
    #[cfg_attr(not(test), allow(dead_code))]
    fn get(&self, relative_path: &str, content_hash: u64, lang_salt: &str) -> Option<CorpusFile> {
        self.load_shard(corpus_bucket(relative_path))
            .into_iter()
            .find(|f| corpus_matches(f, relative_path, content_hash, lang_salt))
    }

    /// Build a [`PersistedFacts`] combining `local` (if any — wins whenever
    /// it already knows a path, stale or not: staleness is
    /// `GraphSession::warm_start`'s own job, not this function's) with
    /// cross-repo corpus hits for every path `local` has never seen.
    ///
    /// The read+hash pass below only ever touches paths absent from
    /// `local`, so a repo whose local snapshot already knows every path
    /// (the common warm-rebuild case) pays nothing extra beyond `local`
    /// itself — this is what keeps the per-repo tier's ~220ms monster
    /// load-speed bar unregressed (see
    /// "Cross-repo corpus" section for the measurement). Callers should
    /// only invoke this when `local` is `None` or may have gaps; a repo
    /// whose local store is known complete should skip this call entirely
    /// and use `local` directly.
    pub fn merge_with_local(
        &self,
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
        local: Option<&PersistedFacts>,
    ) -> (PersistedFacts, CorpusLookupStats) {
        let unknown: Vec<String> = file_paths
            .iter()
            .filter(|p| match local {
                None => true,
                Some(l) => !l.files.contains_key(p.as_str()),
            })
            .cloned()
            .collect();

        // a corpus directory with zero entries can never produce
        // a hit — skip the whole probe-construction pass (the dominant
        // cost; see `is_definitely_empty`'s doc) rather than pay it just to
        // discover that. `stats.probed` still reports `unknown.len()` (its
        // documented meaning is "candidates local didn't already have," not
        // "candidates this call actually read"), and the returned
        // `PersistedFacts` is exactly what the full path below would build
        // with an empty `hits` map — this is that same result, reached
        // without touching disk for any of `unknown`.
        if !unknown.is_empty() && self.is_definitely_empty() {
            let mut files: Vec<PersistedFile> = Vec::new();
            if let Some(l) = local {
                files.extend(l.files.values().cloned());
            }
            let fingerprints = local.map(|l| l.fingerprints.clone()).unwrap_or_default();
            let stats = CorpusLookupStats {
                probed: unknown.len(),
                hits: 0,
                shards_read: 0,
                bytes_read: 0,
            };
            return (PersistedFacts::new(fingerprints, files), stats);
        }

        struct Probe {
            path: String,
            hash: u64,
            salt: String,
            bucket: u64,
        }
        let probes: Vec<Probe> = maybe_par_iter!(unknown)
            .filter_map(|path: &String| {
                let full = root.join(path.as_str());
                let content = std::fs::read_to_string(&full).ok()?;
                let hash = content_hash(&content);
                let lang_id = detect_language_id(path, registry);
                let salt = effective_language_salt(&lang_id);
                let bucket = corpus_bucket(path);
                Some(Probe {
                    path: path.clone(),
                    hash,
                    salt,
                    bucket,
                })
            })
            .collect();

        let mut by_bucket: HashMap<u64, Vec<usize>> = HashMap::default();
        for (i, p) in probes.iter().enumerate() {
            by_bucket.entry(p.bucket).or_default().push(i);
        }
        // Already maximal as a *shard* prune: with 1,024 buckets and a giant's
        // file list every bucket is hit, which is why `shards_read` reads 1024
        // and always did. The pruning that matters happens inside each shard,
        // below — the index says which byte ranges can hold these probes, and
        // nothing else in the shard is ever read (see "Shard layout v2").
        let buckets: Vec<u64> = by_bucket.keys().copied().collect();
        let per_bucket: Vec<(usize, u64, Vec<(usize, CorpusFile)>)> = maybe_par_iter!(buckets)
            .map(|&bucket: &u64| {
                let Some(idxs) = by_bucket.get(&bucket) else {
                    return (0, 0, Vec::new());
                };
                let Some((mut file, index)) = self.open_shard(bucket) else {
                    return (1, 0, Vec::new());
                };
                let mut wanted: Vec<(usize, ShardEntry)> = Vec::with_capacity(idxs.len());
                for &i in idxs {
                    let p = &probes[i];
                    let key = corpus_key_hash(&p.path, p.hash, &p.salt);
                    for entry in index.candidates(key) {
                        wanted.push((i, *entry));
                    }
                }
                if wanted.is_empty() {
                    return (1, index.bytes_read, Vec::new());
                }
                let (payloads, payload_bytes) = Self::read_payloads(&mut file, &index, &wanted);
                let mut found: Vec<(usize, CorpusFile)> = Vec::with_capacity(payloads.len());
                for (i, bytes) in payloads {
                    let Ok(f) = ciborium::from_reader::<CorpusFile, _>(&bytes[..]) else {
                        continue;
                    };
                    let p = &probes[i];
                    // The index matched a hash; this is still the same
                    // field-by-field check v1 did, so a collision can only be
                    // a false miss.
                    if corpus_matches(&f, &p.path, p.hash, &p.salt) {
                        found.push((i, f));
                    }
                }
                (1, index.bytes_read + payload_bytes, found)
            })
            .collect();

        let mut hits: HashMap<String, CorpusFile> = HashMap::default();
        let mut shards_read = 0usize;
        let mut bytes_read = 0u64;
        for (opened, bytes, found) in per_bucket {
            shards_read += opened;
            bytes_read += bytes;
            for (i, f) in found {
                hits.insert(probes[i].path.clone(), f);
            }
        }

        let stats = CorpusLookupStats {
            probed: probes.len(),
            hits: hits.len(),
            shards_read,
            bytes_read,
        };

        let mut files: Vec<PersistedFile> = Vec::new();
        if let Some(l) = local {
            files.extend(l.files.values().cloned());
        }
        for f in hits.into_values() {
            files.push(PersistedFile {
                facts: f.facts,
                precomputed: f.precomputed,
                resolution: None,
            });
        }

        let fingerprints = local.map(|l| l.fingerprints.clone()).unwrap_or_default();
        (PersistedFacts::new(fingerprints, files), stats)
    }

    /// Write every file in `current` whose content changed (or is new)
    /// relative to `previous` into the corpus — never a file that was
    /// already unchanged relative to `previous`, since (for any repo that
    /// has been built with the corpus enabled before) that file's corpus
    /// entry almost certainly already exists from a prior `populate_delta`
    /// call. This bounds populate cost to the delta, not the whole corpus,
    /// on every build after the first for a given repo.
    ///
    /// `previous` takes [`PersistedFacts::content_hash_index`]'s cheap
    /// `path -> content_hash` shape rather than a full `PersistedFacts` on
    /// purpose: a caller (`sem-cli`'s wiring) that already had to move its
    /// only owned `PersistedFacts` into `GraphSession::warm_start` shouldn't
    /// have to clone the whole thing — entity bodies and all — just to keep
    /// a diff base alive until this call.
    ///
    /// Best-effort, exactly like `FactsStore::save`: a write failure here
    /// must never fail the build this corpus exists to speed up — callers
    /// should treat a returned `Err` the same way `FactsStore::save`'s
    /// callers do (log or ignore, never propagate as a build failure).
    pub fn populate_delta(
        &self,
        previous: Option<&HashMap<String, u64>>,
        current: &PersistedFactsRef<'_>,
        registry: &ParserRegistry,
    ) -> io::Result<CorpusPopulateStats> {
        // Serialize-from-references: no `CorpusFile`
        // clone — the shard writer only ever needs to *encode* these, and
        // `current` outlives the call.
        let changed: Vec<CorpusFileRef<'_>> = current
            .files
            .iter()
            .filter(|f| match previous {
                None => true,
                Some(p) => p.get(f.facts.path) != Some(&f.facts.content_hash),
            })
            .map(|f| {
                let lang_id = detect_language_id(f.facts.path, registry);
                CorpusFileRef {
                    facts: FileFactsRef {
                        path: f.facts.path,
                        content_hash: f.facts.content_hash,
                        entities: f.facts.entities,
                    },
                    precomputed: f.precomputed,
                    lang_salt: effective_language_salt(&lang_id),
                }
            })
            .collect();
        self.write_corpus_files(changed)
    }

    /// Ingest externally-sourced facts (: sem-cli's cloud download,
    /// `facts_remote.rs`) into this corpus, key-validated first — see this
    /// section's module doc for why validation happens here rather than
    /// being left to whatever consults the corpus later. Unlike
    /// [`Self::populate_delta`] (which only ever writes facts *this*
    /// process derived itself from a local read+hash pass, trustworthy by
    /// construction), every [`RemoteFact`] here arrived over a network
    /// boundary this process cannot fully trust.
    ///
    /// A rejected fact never reaches a shard — it is reported in
    /// [`IngestOutcome::rejected`] with the precise [`IngestError`] that
    /// sank it, and the caller's next build simply treats that file as a
    /// corpus miss, falling back to local extraction exactly as if the
    /// download had never happened (correct, not poisoned). Accepted facts
    /// are merged into their buckets with the exact same read-merge-write
    /// discipline `populate_delta` uses — once written, an ingested
    /// [`CorpusFile`] is indistinguishable from one this process derived
    /// locally; [`Self::merge_with_local`] and every downstream
    /// `GraphSession::warm_start` consult treat it identically, including
    /// that consult's own independent re-read-and-hash of the *local* file,
    /// which is the second trust boundary this validation does not replace.
    pub fn ingest_remote(
        &self,
        registry: &ParserRegistry,
        facts: Vec<RemoteFact>,
    ) -> io::Result<IngestOutcome> {
        let mut rejected: Vec<(String, IngestError)> = Vec::new();
        let mut good: Vec<CorpusFile> = Vec::new();

        for rf in facts {
            let RemoteFact {
                facts,
                precomputed,
                claimed_relative_path,
                claimed_content_hash,
                claimed_language_salt,
                claimed_schema_version,
            } = rf;
            let path = facts.path.clone();

            if claimed_relative_path != facts.path {
                rejected.push((
                    path,
                    IngestError::PathMismatch {
                        claimed: claimed_relative_path,
                        actual: facts.path,
                    },
                ));
                continue;
            }
            if claimed_content_hash != facts.content_hash {
                rejected.push((
                    path,
                    IngestError::ContentHashMismatch {
                        path: facts.path,
                        claimed: claimed_content_hash,
                        actual: facts.content_hash,
                    },
                ));
                continue;
            }
            if claimed_schema_version != FACTS_SCHEMA_VERSION {
                rejected.push((
                    path,
                    IngestError::SchemaVersionMismatch {
                        path: facts.path,
                        claimed: claimed_schema_version,
                        actual: FACTS_SCHEMA_VERSION,
                    },
                ));
                continue;
            }
            let lang_id = detect_language_id(&facts.path, registry);
            let expected_salt = effective_language_salt(&lang_id);
            if claimed_language_salt != expected_salt {
                rejected.push((
                    path,
                    IngestError::LanguageSaltMismatch {
                        path: facts.path,
                        claimed: claimed_language_salt,
                        actual: expected_salt.to_string(),
                    },
                ));
                continue;
            }

            good.push(CorpusFile {
                facts,
                precomputed,
                lang_salt: expected_salt.to_string(),
            });
        }

        let accepted = self.write_corpus_files(good.iter().map(CorpusFileRef::of).collect())?;
        Ok(IngestOutcome { accepted, rejected })
    }

    /// Bucket, then read-merge-write, `files` into their shards — the
    /// shared tail of [`Self::populate_delta`] (facts this process derived
    /// itself) and [`Self::ingest_remote`] (facts validated from an
    /// external source): once a `CorpusFile` exists, both callers write it
    /// identically, with the same `ShardLock`-serialized read-merge-write
    /// and atomic rename `populate_delta` always used.
    fn write_corpus_files(&self, files: Vec<CorpusFileRef<'_>>) -> io::Result<CorpusPopulateStats> {
        let mut by_bucket: HashMap<u64, Vec<CorpusFileRef<'_>>> = HashMap::default();
        for f in files {
            let bucket = corpus_bucket(f.facts.path);
            by_bucket.entry(bucket).or_default().push(f);
        }

        let buckets: Vec<u64> = by_bucket.keys().copied().collect();
        let results: Vec<io::Result<(usize, usize)>> = maybe_par_iter!(buckets)
            .map(|&bucket: &u64| {
                let new_files = &by_bucket[&bucket];
                let shard_path = self.shard_path(bucket);
                let _lock = ShardLock::acquire(&shard_path);

                // Index only — the existing entries' payloads are never
                // decoded, only carried forward as bytes (see "Shard layout
                // v2"). Re-read under the lock, so a writer that waited for
                // another writer merges against what that writer left.
                let existing = self.open_shard(bucket);
                let mut known: HashSet<u64> = existing
                    .as_ref()
                    .map(|(_, index)| index.entries.iter().map(|e| e.key_hash).collect())
                    .unwrap_or_default();

                // Anything this shard already holds is not written again.
                // On a known-content build that is every file, `fresh` is
                // empty, and the shard is left untouched — which is what
                // makes populate cost track new content rather than corpus
                // size.
                let mut fresh: Vec<(u64, Vec<u8>)> = Vec::new();
                for f in new_files {
                    let key = corpus_key_hash(f.facts.path, f.facts.content_hash, &f.lang_salt);
                    if !known.insert(key) {
                        continue;
                    }
                    fresh.push((key, Self::encode_corpus_file(f)?));
                }
                if fresh.is_empty() {
                    return Ok((0, 0));
                }
                let written = fresh.len();

                // The old payload region stays byte-identical and stays at
                // the front, so every old record's offset survives untouched
                // and only the index is rebuilt.
                let (carried_payload, mut records) = match existing {
                    Some((mut file, index)) => {
                        let len = usize::try_from(index.payload_len).unwrap_or(0);
                        let mut buf = vec![0u8; len];
                        if file.seek(SeekFrom::Start(index.payload_start)).is_err()
                            || file.read_exact(&mut buf).is_err()
                        {
                            // Unreadable payload: treat the shard as absent
                            // rather than write a file with dangling ranges.
                            (Vec::new(), Vec::new())
                        } else {
                            (buf, index.entries.clone())
                        }
                    }
                    None => (Vec::new(), Vec::new()),
                };

                let mut fresh_payload: Vec<u8> = Vec::new();
                let mut offset = carried_payload.len() as u64;
                for (key_hash, payload) in &fresh {
                    records.push(ShardEntry {
                        key_hash: *key_hash,
                        offset,
                        len: payload.len() as u64,
                    });
                    offset += payload.len() as u64;
                    fresh_payload.extend_from_slice(payload);
                }
                records.sort_by_key(|e| e.key_hash);

                let head = Self::encode_shard_head(records.len())?;
                let index_bytes = Self::encode_shard_index(&records);
                self.write_shard_atomic_parts(
                    bucket,
                    &[&head, &index_bytes, &carried_payload, &fresh_payload],
                )?;
                Ok((written, 1))
            })
            .collect();

        let mut files_written = 0usize;
        let mut shards_written = 0usize;
        for r in results {
            let (files, shards) = r?;
            files_written += files;
            shards_written += shards;
        }
        Ok(CorpusPopulateStats {
            files_written,
            shards_written,
        })
    }

    /// Delete every shard in the corpus. Advisory only, exactly like
    /// `FactsStore::clear` — always safe, including when nothing was ever
    /// written.
    pub fn clear_all(&self) -> io::Result<()> {
        match std::fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "factshard") {
                        let _ = std::fs::remove_file(path);
                    }
                }
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::entity::SemanticEntity;
    use crate::parser::incremental::content_hash;

    fn entity(path: &str, name: &str) -> SemanticEntity {
        SemanticEntity {
            id: format!("{path}::function::{name}"),
            file_path: path.to_string(),
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

    fn sample_facts() -> PersistedFacts {
        let files = vec![
            PersistedFile {
                facts: FileFacts {
                    path: "a.ts".to_string(),
                    content_hash: content_hash("export const a = 1;"),
                    entities: vec![entity("a.ts", "a")],
                },
                precomputed: None,
                resolution: None,
            },
            PersistedFile {
                facts: FileFacts {
                    path: "b.ts".to_string(),
                    content_hash: content_hash("export const b = 2;"),
                    entities: vec![entity("b.ts", "b")],
                },
                precomputed: None,
                resolution: None,
            },
        ];
        let mut fingerprints = TableFingerprints::default();
        // Exercise a non-default fingerprint map, not just the empty default.
        let _ = &mut fingerprints;
        PersistedFacts::new(fingerprints, files)
    }

    #[test]
    fn round_trips_a_saved_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());

        assert!(
            store.load(root.path()).is_none(),
            "nothing saved yet must miss cleanly"
        );

        let facts = sample_facts();
        store.save(root.path(), &facts.as_borrowed()).expect("save");

        let loaded = store.load(root.path()).expect("load after save");
        assert_eq!(loaded.file_count(), 2);
        assert_eq!(
            loaded.files.get("a.ts").unwrap().facts.entities[0].name,
            "a"
        );
        assert_eq!(
            loaded.files.get("b.ts").unwrap().facts.content_hash,
            content_hash("export const b = 2;")
        );
    }

    #[test]
    fn missing_store_file_is_a_clean_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        assert!(store.load(root.path()).is_none());
    }

    /// `TableFingerprints.entries` is a `HashMap<u64, u64>` written
    /// through `#[derive(Serialize)]`, which encodes the map in its own
    /// iteration order. `FxHashMap`'s hasher is fixed-seed — the same insertion
    /// *sequence* always lands the same key set in the same buckets — but a
    /// real build's corpus-wide fingerprints are folded from per-file/per-table
    /// work whose *completion order* depends on scheduling (thread count,
    /// work-stealing), not on key order. Two builds of the identical logical
    /// corpus can insert the identical (key, value) pairs in different
    /// sequences and land colliding keys in different buckets, producing
    /// byte-different CBOR for facts that are, by every `PartialEq`, the same.
    ///
    /// This directly reproduces the mechanism the change's evidence names
    /// ("same binary, same corpus, different factpack bytes across runs on
    /// HA/monster ... entity-level map serialization order") without needing
    /// the real HA/monster corpora: insertion order alone is the variable
    /// under test, isolated from parallelism's own nondeterminism.
    #[test]
    fn factpack_fingerprint_bytes_are_independent_of_insertion_order() {
        let n: u64 = 4000;
        let value_of = |k: u64| k.wrapping_mul(2_654_435_761);

        let mut forward = TableFingerprints::default();
        for k in 0..n {
            forward.put(k, value_of(k));
        }

        // A different, still-total insertion sequence over the exact same key
        // set — standing in for a different thread-completion order over one
        // build's corpus-wide tables. A xorshift-driven Fisher-Yates shuffle,
        // not just a coarse reordering, so the insertion sequence is genuinely
        // decorrelated from key order.
        let mut order: Vec<u64> = (0..n).collect();
        let mut state: u64 = 0x9E3779B97F4A7C15;
        for i in (1..order.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = (state % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        let mut interleaved = TableFingerprints::default();
        for k in order {
            interleaved.put(k, value_of(k));
        }

        assert_eq!(
            forward, interleaved,
            "sanity: both maps hold the same logical (key, value) pairs"
        );

        let mut forward_bytes = Vec::new();
        ciborium::into_writer(&forward, &mut forward_bytes).expect("encode forward");
        let mut interleaved_bytes = Vec::new();
        ciborium::into_writer(&interleaved, &mut interleaved_bytes).expect("encode interleaved");

        assert_eq!(
            forward_bytes, interleaved_bytes,
            "identical fingerprint content serialized to different CBOR \
             bytes depending on insertion order alone — this is the byte-determinism \
             the facts corpus's content-addressed dedup and the perf parity gates \
             both rely on"
        );
    }

    #[test]
    fn schema_version_mismatch_is_a_clean_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        store
            .save(root.path(), &sample_facts().as_borrowed())
            .expect("save");

        // Hand-craft a store file with a wrong schema version, the way an
        // older/newer sem-core build would have written it.
        let path = store.path_for(root.path());
        let bytes = std::fs::read(&path).expect("read written file");
        let mut rest = &bytes[MAGIC.len()..];
        let before_len = rest.len();
        let _: StoreHeader = ciborium::from_reader(&mut rest).unwrap();
        let consumed = before_len - rest.len();
        let bad_header = StoreHeader {
            schema_version: FACTS_SCHEMA_VERSION + 1,
            sem_core_salt: sem_core_salt().to_string(),
        };
        let mut rebuilt = Vec::new();
        rebuilt.extend_from_slice(MAGIC);
        ciborium::into_writer(&bad_header, &mut rebuilt).unwrap();
        rebuilt.extend_from_slice(&bytes[MAGIC.len() + consumed..]);
        std::fs::write(&path, &rebuilt).expect("overwrite");

        assert!(
            store.load(root.path()).is_none(),
            "a schema-version bump must be a clean miss, not a panic or wrong facts"
        );
    }

    #[test]
    fn salt_mismatch_is_a_clean_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        store
            .save(root.path(), &sample_facts().as_borrowed())
            .expect("save");

        let path = store.path_for(root.path());
        let bytes = std::fs::read(&path).expect("read written file");
        let mut rest = &bytes[MAGIC.len()..];
        let before_len = rest.len();
        let _: StoreHeader = ciborium::from_reader(&mut rest).unwrap();
        let consumed = before_len - rest.len();
        let bad_header = StoreHeader {
            schema_version: FACTS_SCHEMA_VERSION,
            sem_core_salt: "0.0.0-different".to_string(),
        };
        let mut rebuilt = Vec::new();
        rebuilt.extend_from_slice(MAGIC);
        ciborium::into_writer(&bad_header, &mut rebuilt).unwrap();
        rebuilt.extend_from_slice(&bytes[MAGIC.len() + consumed..]);
        std::fs::write(&path, &rebuilt).expect("overwrite");

        assert!(
            store.load(root.path()).is_none(),
            "a sem-core version salt mismatch must be a clean miss"
        );
    }

    #[test]
    fn truncated_file_is_a_clean_miss_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        store
            .save(root.path(), &sample_facts().as_borrowed())
            .expect("save");

        let path = store.path_for(root.path());
        let bytes = std::fs::read(&path).expect("read written file");
        // Truncate to a handful of bytes past the magic — short enough that
        // even the header struct can't fully decode.
        let truncated = &bytes[..(MAGIC.len() + 2).min(bytes.len())];
        std::fs::write(&path, truncated).expect("truncate");

        assert!(
            store.load(root.path()).is_none(),
            "truncated bytes must be a clean miss, never a panic"
        );
    }

    #[test]
    fn garbage_bytes_are_a_clean_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        let path = store.path_for(root.path());
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(&path, b"not a facts store at all, just noise \x00\x01\xff").unwrap();

        assert!(store.load(root.path()).is_none());
    }

    #[test]
    fn deleting_the_store_directory_is_always_safe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        store
            .save(root.path(), &sample_facts().as_borrowed())
            .expect("save");
        assert!(store.load(root.path()).is_some());

        std::fs::remove_dir_all(dir.path()).expect("rm -rf the whole store");
        assert!(
            store.load(root.path()).is_none(),
            "deleted store must miss cleanly, not error"
        );
        // And a fresh save recreates it without any special recovery step.
        store
            .save(root.path(), &sample_facts().as_borrowed())
            .expect("save after delete");
        assert!(store.load(root.path()).is_some());
    }

    #[test]
    fn clear_is_advisory_and_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        // Clearing a store that was never saved is a no-op, not an error.
        store.clear(root.path()).expect("clear on empty store");

        store
            .save(root.path(), &sample_facts().as_borrowed())
            .expect("save");
        store.clear(root.path()).expect("clear");
        assert!(store.load(root.path()).is_none());
        // Clearing twice is still fine.
        store.clear(root.path()).expect("clear again");
    }

    #[test]
    fn different_roots_do_not_collide() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_a = tempfile::tempdir().expect("root a");
        let root_b = tempfile::tempdir().expect("root b");
        let store = FactsStore::open(dir.path());

        store
            .save(root_a.path(), &sample_facts().as_borrowed())
            .expect("save a");
        assert!(
            store.load(root_b.path()).is_none(),
            "root b must not see root a's snapshot"
        );
    }

    #[test]
    fn no_temp_files_survive_a_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root tempdir");
        let store = FactsStore::open(dir.path());
        store
            .save(root.path(), &sample_facts().as_borrowed())
            .expect("save");

        let stragglers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.contains(".tmp.")))
            .collect();
        assert!(stragglers.is_empty(), "left temp files: {stragglers:?}");
    }
}

#[cfg(test)]
mod corpus_tests {
    /// The corpus key includes the installed fast extractor's identity
    /// (`effective_language_salt`), which is process-global — so any test that
    /// writes or reads a corpus entry has to be serialized against the tests
    /// that install a fast extractor.
    fn salt_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::parser::fast_extractor::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    use super::*;
    use crate::model::entity::SemanticEntity;
    use crate::parser::plugins::create_default_registry;

    fn entity(path: &str, name: &str) -> SemanticEntity {
        SemanticEntity {
            id: format!("{path}::function::{name}"),
            file_path: path.to_string(),
            entity_type: "function".to_string(),
            name: name.to_string(),
            parent_id: None,
            content: "function x() {}".to_string(),
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

    fn persisted_file(path: &str, source: &str) -> PersistedFile {
        PersistedFile {
            facts: FileFacts {
                path: path.to_string(),
                content_hash: content_hash(source),
                entities: vec![entity(path, "f")],
            },
            precomputed: None,
            resolution: None,
        }
    }

    fn registry() -> ParserRegistry {
        create_default_registry()
    }

    #[test]
    fn corpus_key_excludes_wrong_path() {
        let _salt_guard = salt_guard();
        // Same content, different relative path: entity ids are
        // path-qualified (see this module's "Key" doc), so a corpus entry
        // for one path must never answer a lookup for another, even with an
        // identical content hash.
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let source = "export function shared() { return 1; }";

        corpus
            .populate_delta(
                None,
                &PersistedFacts::new(
                    TableFingerprints::default(),
                    vec![persisted_file("a.ts", source)],
                )
                .as_borrowed(),
                &registry(),
            )
            .expect("populate");

        let hash = content_hash(source);
        assert!(
            corpus.get("a.ts", hash, "ts-0.23-u16-exportspan").is_some(),
            "the path that was actually populated must hit"
        );
        assert!(
            corpus.get("b.ts", hash, "ts-0.23-u16-exportspan").is_none(),
            "identical content at a different path must miss — path is part of the key, \
             not just a locality hint"
        );
    }

    #[test]
    fn effective_salt_is_unchanged_with_no_fast_extractor_installed() {
        let _salt_guard = salt_guard();
        // The default state, and every build without a fast-path feature: the
        // suffix must contribute nothing, or landing this code would have
        // invalidated every existing corpus entry on disk.
        //
        // Every language registered in `MUL_RUNTIME_GATES` is a deliberate
        // exception, pinned in both directions by
        // `csharp_salt_tracks_the_mul_phase1_switch` below: its table entry
        // describes the producer that only runs when its switch is on, which
        // by default it is not. Skipping by table membership (rather than a
        // hardcoded `"csharp"`) means this test keeps covering every
        // *ungated* language automatically as `MUL_RUNTIME_GATES` grows.
        for (id, salt) in LANGUAGE_SALTS.iter() {
            if crate::parser::scope_resolve::MUL_RUNTIME_GATES
                .iter()
                .any(|g| g.lang_id == *id)
            {
                continue;
            }
            assert_eq!(&effective_language_salt(id), salt);
        }
        assert_eq!(
            effective_language_salt("no-such-language"),
            DEFAULT_LANGUAGE_SALT
        );
    }

    #[test]
    fn csharp_salt_tracks_the_mul_phase1_switch() {
        let _salt_guard = salt_guard();
        // The salt names the producer that wrote the entry. MUL phase 1's C#
        // precompute is a *run-time* switch (memory-gated per
        // "dotnet stays GATED"), so unlike every other
        // row in the table, `csharp`'s effective salt is not a constant — and
        // it must move with the switch in both directions, or first-writer-wins
        // corpus dedup lets one mode's entries permanently answer the other's
        // lookups.
        //
        // Walks every registered gate rather than hardcoding "csharp" — this
        // test now covers whatever `MUL_RUNTIME_GATES` grows to (MUL phase
        // 2/3), not just today's one entry.
        for gate in crate::parser::scope_resolve::MUL_RUNTIME_GATES {
            let admitted = crate::parser::scope_resolve::mul_precompute_admits(gate.lang_id);
            let table_salt = language_salt(gate.lang_id);
            let expected = if admitted {
                table_salt
            } else {
                gate.pre_switch_salt
            };
            assert_eq!(effective_language_salt(gate.lang_id), expected);
            assert_ne!(
                gate.pre_switch_salt, table_salt,
                "{}'s pre-switch salt and the table's current salt must stay \
                 distinct, or the switch isolates nothing",
                gate.lang_id
            );
        }

        // 2026-08-22: every phase-1/phase-2 language is gated now — C++
        // and Python joined `MUL_RUNTIME_GATES` when the ceiling was
        // redefined against peak memory footprint, so both are covered by
        // the loop above like every other gated language; no unconditional
        // survivor remains to special-case here.
    }

    #[test]
    fn resolve_gated_salt_generalizes_beyond_csharp() {
        // `resolve_gated_salt` is what makes fix structural rather
        // than remembered: before it existed, `producer_language_salt` was a
        // single hand-written `if lang_id == "csharp"` branch that could not
        // have honored a second gated language without a second hand-written
        // branch. This test proves the generalized function does — with a
        // synthetic gate, since the real `MUL_RUNTIME_GATES` has only one row
        // today and the real switch is cached in a `OnceLock` for the whole
        // process (can't be flipped mid test to prove the same point live).
        let gates = [crate::parser::scope_resolve::MulRuntimeGate {
            lang_id: "kotlin",
            pre_switch_salt: "ts-1.1",
        }];
        let table_salt = "ts-1.1-newgen";

        assert_eq!(
            resolve_gated_salt(&gates, |_| false, "kotlin", table_salt),
            "ts-1.1",
            "switch off: entries a switched-off build wrote must stay \
             reachable under the pre-switch salt"
        );
        assert_eq!(
            resolve_gated_salt(&gates, |_| true, "kotlin", table_salt),
            table_salt,
            "switch on: the richer producer's entries must isolate under the \
             table's current salt, never collide with the pre-switch entries"
        );
        // A language absent from the gate table is untouched by it, whatever
        // `admits` says — the table, not the predicate, decides applicability.
        assert_eq!(
            resolve_gated_salt(&gates, |_| false, "python", "ts-0.23"),
            "ts-0.23"
        );
    }

    /// The mechanism this whole module exists to prevent, proven directly:
    /// at a *fixed* key, first-writer-wins denies a later, richer write for
    /// identical content forever — the corpus is stuck at the first writer's
    /// capability level ("silently denies producer upgrades").
    /// This is intentional, documented behavior (an anti-poisoning stance,
    /// not a bug) — the reason `producer_language_salt`/`MUL_RUNTIME_GATES`
    /// exist is to make sure a real capability change moves the *key*, not
    /// to make same-key overwrites acceptable.
    #[test]
    fn weak_entry_at_a_fixed_key_permanently_denies_a_later_strong_write() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let source = "class C {}";
        let hash = content_hash(source);
        let fixed_salt = "ts-0.23-mp1";

        let weak = CorpusFile {
            facts: FileFacts {
                path: "C.cs".to_string(),
                content_hash: hash,
                entities: vec![entity("C.cs", "C")],
            },
            precomputed: None,
            lang_salt: fixed_salt.to_string(),
        };
        corpus
            .write_corpus_files(vec![CorpusFileRef::of(&weak)])
            .expect("write weak");

        let strong = CorpusFile {
            precomputed: Some(
                crate::parser::scope_resolve::dummy_precomputed_facts_for_test("richer"),
            ),
            ..weak.clone()
        };
        corpus
            .write_corpus_files(vec![CorpusFileRef::of(&strong)])
            .expect("write strong (denied)");

        let served = corpus
            .get("C.cs", hash, fixed_salt)
            .expect("a hit at this key");
        assert!(
            served.precomputed.is_none(),
            "first-writer-wins must still be serving the weak entry at an \
             unchanged key — this is the exact denial reported here, and \
             is why a real capability change must move the salt"
        );
    }

    /// The fix, proven end-to-end: a producer-capability upgrade (simulated
    /// via `MUL_RUNTIME_GATES`'s registered csharp gate, the same mechanism
    /// pass 1's real switch drives) moves the *key*, so the weak write and
    /// the strong write never collide — and a switched-on reader gets the
    /// strong facts instead of being denied them forever.
    #[test]
    fn mul_runtime_gate_upgrade_reaches_the_switched_on_reader() {
        let _salt_guard = salt_guard();
        let gate = crate::parser::scope_resolve::MUL_RUNTIME_GATES
            .iter()
            .find(|g| g.lang_id == "csharp")
            .expect("csharp is the one registered MUL runtime gate today");
        let on_salt = language_salt("csharp");

        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let source = "class C {}";
        let hash = content_hash(source);

        // A switched-off build wrote this file's facts with no precompute,
        // under the pre-switch salt.
        let weak = CorpusFile {
            facts: FileFacts {
                path: "C.cs".to_string(),
                content_hash: hash,
                entities: vec![entity("C.cs", "C")],
            },
            precomputed: None,
            lang_salt: gate.pre_switch_salt.to_string(),
        };
        corpus
            .write_corpus_files(vec![CorpusFileRef::of(&weak)])
            .expect("write weak (pre-switch)");

        // A switched-on build computes richer facts for identical content,
        // under the table's current (post-switch) salt.
        let strong = CorpusFile {
            facts: weak.facts.clone(),
            precomputed: Some(
                crate::parser::scope_resolve::dummy_precomputed_facts_for_test("richer"),
            ),
            lang_salt: on_salt.to_string(),
        };
        corpus
            .write_corpus_files(vec![CorpusFileRef::of(&strong)])
            .expect("write strong (isolated key, not denied)");

        // The switched-off salt still safely serves the pre-switch build's
        // own entry — untouched, never corrupted by the upgrade.
        let off_hit = corpus
            .get("C.cs", hash, gate.pre_switch_salt)
            .expect("pre-switch lookup still hits its own entry");
        assert!(off_hit.precomputed.is_none());

        // The switched-on salt serves the strong facts: the upgrade reached
        // the reader instead of being denied by the pre-switch entry.
        let on_hit = corpus
            .get("C.cs", hash, on_salt)
            .expect("post-switch lookup hits the upgraded entry");
        assert!(
            on_hit.precomputed.is_some(),
            "a producer upgrade must reach a switched-on reader, not be \
             denied by a switched-off entry at a different key"
        );
    }

    #[test]
    fn effective_salt_isolates_extractor_generations() {
        let _salt_guard = salt_guard();
        // Two extractor generations must never satisfy each other's lookups:
        // they legitimately disagree on structural_hash conventions, on kappa
        // values (grammar-shaped, a known limitation) and, while one is
        // still being proven, on entity sets.
        let plain = salt_with_extractor("ts-0.23", None);
        let gen_a = salt_with_extractor("ts-0.23", Some("oxc-0.143.0-r1"));
        let gen_b = salt_with_extractor("ts-0.23", Some("oxc-0.144.0-r1"));
        assert_eq!(plain, "ts-0.23");
        assert_ne!(plain, gen_a);
        assert_ne!(gen_a, gen_b);
    }

    #[test]
    fn corpus_isolates_by_language_salt() {
        let _salt_guard = salt_guard();
        // Simulates a grammar bump for one language: an entry written under
        // an old salt must not answer a lookup under a new salt, and a
        // *different* language's entries must be untouched (per-language
        // isolation is the whole point of a per-language salt, not one
        // crate-wide version).
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let source = "print('hi')";
        let hash = content_hash(source);

        corpus
            .populate_delta(
                None,
                &PersistedFacts::new(
                    TableFingerprints::default(),
                    vec![persisted_file("a.py", source)],
                )
                .as_borrowed(),
                &registry(),
            )
            .expect("populate");

        // Python is gated (`SEM_MUL_PYTHON`, off by default since the 2026-08-22 demotion) — the
        // effective salt this default-settings populate wrote under is the
        // gate's pre-switch salt, not `LANGUAGE_SALTS`'s raw table entry
        // ("ts-0.23-mp4", the switched-*on* salt). Deriving it via
        // `effective_language_salt` rather than hardcoding keeps this test
        // honest about whichever state the switch is in.
        let py_salt = effective_language_salt("python");
        assert!(corpus.get("a.py", hash, &py_salt).is_some());
        assert!(
            corpus.get("a.py", hash, "ts-0.24-bumped").is_none(),
            "a grammar-bump salt change must miss the old entry"
        );
        // A different language's entry sharing the same bucket space is
        // simply a different key; unaffected by the above.
        assert!(corpus.get("a.ts", hash, &py_salt).is_none());
    }

    #[test]
    fn yaml_salt_bump_denies_the_old_handrolled_1_entry() {
        // the yaml plugin's id fix (multi-document files
        // no longer collide same-named keys in different documents onto one
        // id) is a producer-visible change to FileFacts.entities' `id`
        // field, so the salt-bump discipline requires the `yaml` salt to move — it did,
        // `handrolled-1` -> `handrolled-2`. This is the concrete, per-
        // language instance of `corpus_isolates_by_language_salt`'s general
        // proof: an entry a pre-fix binary wrote (under `handrolled-1`) must
        // be a clean miss against a post-fix binary's lookup (under
        // `language_salt("yaml")`, now `handrolled-2`), never silently
        // served stale ids.
        let _salt_guard = salt_guard();
        const YAML_PRE_VLG_SALT: &str = "handrolled-1";
        assert_ne!(
            language_salt("yaml"),
            YAML_PRE_VLG_SALT,
            "the table's current yaml salt must differ from the pre-fix one, \
             or the bump isolates nothing"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let source = "name: my-app\nversion: 1.0.0\n";
        let hash = content_hash(source);
        let entities = vec![entity("config.yaml", "name")];

        // A pre-fix binary's entry, written under the old salt.
        // `populate_delta` always computes *today's* salt for a `.yaml`
        // file, so simulating an old binary's write means going one level
        // below it, to the same `write_corpus_files` populate_delta itself
        // calls, with an explicit stale `lang_salt`.
        corpus
            .write_corpus_files(vec![CorpusFileRef {
                facts: FileFactsRef {
                    path: "config.yaml",
                    content_hash: hash,
                    entities: &entities,
                },
                precomputed: None,
                lang_salt: YAML_PRE_VLG_SALT.to_string(),
            }])
            .expect("populate");

        assert!(
            corpus.get("config.yaml", hash, YAML_PRE_VLG_SALT).is_some(),
            "sanity: the pre-fix entry itself must be readable under its own salt"
        );
        assert!(
            corpus
                .get("config.yaml", hash, language_salt("yaml"))
                .is_none(),
            "a post-fix build must treat the old-salt yaml entry as a miss, \
             never serve its stale ids"
        );
    }

    #[test]
    fn corrupted_shard_is_a_clean_miss_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        let bucket = corpus_bucket("a.ts");
        std::fs::write(
            corpus.shard_path(bucket),
            b"not a shard, just noise \x00\x01\xff",
        )
        .unwrap();

        assert!(corpus.get("a.ts", 1, "ts-0.23").is_none());
        assert!(corpus.load_shard(bucket).is_empty());
    }

    #[test]
    fn missing_corpus_dir_is_a_clean_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus_dir = dir.path().join("never-created");
        let corpus = FactsCorpus::open(&corpus_dir);
        assert!(corpus.get("a.ts", 1, "ts-0.23").is_none());
    }

    #[test]
    fn cross_repo_reuse_via_merge_with_local() {
        let _salt_guard = salt_guard();
        // The core cross-repo claim at unit scale: repo A populates the
        // corpus, repo B (a fresh process with no local store of its own —
        // `local: None`) merges against the corpus and gets A's facts back
        // for the path they share, without ever touching repo A's tree.
        let corpus_dir = tempfile::tempdir().expect("corpus dir");
        let corpus = FactsCorpus::open(corpus_dir.path());
        let repo_a = tempfile::tempdir().expect("repo a");
        let repo_b = tempfile::tempdir().expect("repo b");
        let source = "export function shared() { return 42; }\n";
        std::fs::write(repo_a.path().join("shared.ts"), source).unwrap();
        std::fs::write(repo_b.path().join("shared.ts"), source).unwrap();
        // A file unique to B — must not spuriously hit anything from A.
        std::fs::write(repo_b.path().join("only_in_b.ts"), "export const x = 1;\n").unwrap();

        let reg = registry();
        corpus
            .populate_delta(
                None,
                &PersistedFacts::new(
                    TableFingerprints::default(),
                    vec![persisted_file("shared.ts", source)],
                )
                .as_borrowed(),
                &reg,
            )
            .expect("populate from repo a");

        let file_paths = vec!["shared.ts".to_string(), "only_in_b.ts".to_string()];
        let (merged, stats) = corpus.merge_with_local(repo_b.path(), &file_paths, &reg, None);

        assert_eq!(
            stats.probed, 2,
            "both of B's files are unknown to `local` (None)"
        );
        assert_eq!(stats.hits, 1, "only shared.ts should hit the corpus");
        assert!(merged.files.contains_key("shared.ts"));
        assert!(
            !merged.files.contains_key("only_in_b.ts"),
            "a file with no corpus match must simply be absent, not a false hit"
        );
    }

    /// migration story: a shard left behind by the v1 layout must
    /// read as a clean miss, not a decode of the wrong shape — the same path
    /// every other unreadable shard takes, so an old corpus degrades to a
    /// cold build and is rewritten in v2 on the way past.
    #[test]
    fn a_v1_shard_is_a_clean_miss() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();

        // Exactly what v1 wrote: old magic, same header, then one CBOR array
        // of `CorpusFile` where v2 expects `entry_count` + an index.
        let file = CorpusFile {
            facts: persisted_file("a.ts", "export const a = 1;\n").facts,
            precomputed: None,
            lang_salt: effective_language_salt("typescript"),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"SEMCORP1");
        ciborium::into_writer(
            &StoreHeader {
                schema_version: FACTS_SCHEMA_VERSION,
                sem_core_salt: sem_core_salt().to_string(),
            },
            &mut bytes,
        )
        .unwrap();
        ciborium::into_writer(&vec![file], &mut bytes).unwrap();
        let bucket = corpus_bucket("a.ts");
        std::fs::write(corpus.shard_path(bucket), &bytes).unwrap();

        assert!(
            corpus.load_shard(bucket).is_empty(),
            "a v1 shard must not decode under v2"
        );
        assert!(corpus
            .get(
                "a.ts",
                content_hash("export const a = 1;\n"),
                "ts-0.23-u16-exportspan"
            )
            .is_none());

        // ...and writing past it leaves a readable v2 shard: the old corpus
        // heals rather than wedging.
        let reg = registry();
        corpus
            .populate_delta(
                None,
                &PersistedFacts::new(
                    TableFingerprints::default(),
                    vec![persisted_file("a.ts", "export const a = 1;\n")],
                )
                .as_borrowed(),
                &reg,
            )
            .expect("populate over a v1 shard");
        assert_eq!(corpus.load_shard(bucket).len(), 1);
    }

    /// The write half of corpus-size independence: re-populating content the
    /// corpus already holds writes nothing and touches no shard. This is what
    /// takes `populate_delta` off the corpus-size curve — known-content
    /// scenario re-derives every entry with no local snapshot to diff
    /// against, and under v1 that rewrote every shard it touched.
    #[test]
    fn repopulating_known_content_writes_nothing() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let facts = PersistedFacts::new(
            TableFingerprints::default(),
            vec![
                persisted_file("a.ts", "export const a = 1;\n"),
                persisted_file("b.ts", "export const b = 2;\n"),
            ],
        );

        let first = corpus
            .populate_delta(None, &facts.as_borrowed(), &reg)
            .expect("first");
        assert_eq!(first.files_written, 2);
        assert_eq!(first.shards_written, 2);

        // Same content, same `previous: None` — the caller cannot tell this
        // is a repeat, but the shard index can.
        let second = corpus
            .populate_delta(None, &facts.as_borrowed(), &reg)
            .expect("second");
        assert_eq!(
            second.files_written, 0,
            "content already stored must not be written again"
        );
        assert_eq!(
            second.shards_written, 0,
            "a shard with nothing new must not be rewritten"
        );

        // And the corpus still serves both files.
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(repo.path().join("b.ts"), "export const b = 2;\n").unwrap();
        let (_, stats) = corpus.merge_with_local(
            repo.path(),
            &["a.ts".to_string(), "b.ts".to_string()],
            &reg,
            None,
        );
        assert_eq!(stats.hits, 2, "both entries must survive the no-op write");
    }

    /// The read half, stated as the invariant the change exists to establish: what a
    /// lookup reads off disk tracks the entries it asked for, not the size of
    /// the corpus around them. Under v1 this was false by construction — a
    /// shard was one CBOR array, so touching it decoded all of it.
    #[test]
    fn lookup_bytes_do_not_grow_with_unrelated_corpus_content() {
        let _salt_guard = salt_guard();
        let reg = registry();
        let repo = tempfile::tempdir().expect("repo");
        let wanted_source = "export const wanted = 1;\n";
        std::fs::write(repo.path().join("wanted.ts"), wanted_source).unwrap();
        let wanted = || {
            PersistedFacts::new(
                TableFingerprints::default(),
                vec![persisted_file("wanted.ts", wanted_source)],
            )
        };

        // A corpus holding only the entry under test.
        let small_dir = tempfile::tempdir().expect("small");
        let small = FactsCorpus::open(small_dir.path());
        small
            .populate_delta(None, &wanted().as_borrowed(), &reg)
            .expect("small");

        // The same entry, plus a pile of unrelated content deliberately
        // forced into the *same* bucket, so this is not a bucket-pruning
        // result: it is pruning inside one shard.
        let big_dir = tempfile::tempdir().expect("big");
        let big = FactsCorpus::open(big_dir.path());
        big.populate_delta(None, &wanted().as_borrowed(), &reg)
            .expect("big seed");
        let bucket = corpus_bucket("wanted.ts");
        let mut noise: Vec<PersistedFile> = Vec::new();
        let mut i = 0usize;
        while noise.len() < 200 {
            let path = format!("noise{i}.ts");
            i += 1;
            if corpus_bucket(&path) != bucket {
                continue;
            }
            // Entries far larger than the one under test (a corpus entry
            // stores extracted entities, not source), so any read that
            // touched them would show up immediately.
            let mut fat = persisted_file(&path, &format!("export const n{i} = 1;\n"));
            fat.facts.entities = (0..64).map(|k| entity(&path, &format!("f{k}"))).collect();
            noise.push(fat);
        }
        big.populate_delta(
            None,
            &PersistedFacts::new(TableFingerprints::default(), noise).as_borrowed(),
            &reg,
        )
        .expect("big noise");

        let paths = vec!["wanted.ts".to_string()];
        let (_, small_stats) = small.merge_with_local(repo.path(), &paths, &reg, None);
        let (_, big_stats) = big.merge_with_local(repo.path(), &paths, &reg, None);

        assert_eq!(small_stats.hits, 1);
        assert_eq!(big_stats.hits, 1, "the same entry must still be found");
        assert_eq!(
            small_stats.shards_read, big_stats.shards_read,
            "same probe, same buckets — the difference under test is what is read inside them"
        );

        // The shard on the `big` side is >800 KB; v1 would have read every
        // byte of it. The index grows (24 bytes an entry) and the payload
        // read does not.
        let big_shard_len = std::fs::metadata(big.shard_path(bucket)).unwrap().len();
        assert!(
            big_shard_len > 800_000,
            "the noise must actually be on disk: {big_shard_len}"
        );
        // The only terms allowed to grow are the index (24 bytes an entry)
        // and the fixed head probe, which the tiny shard is too short to pay
        // in full.
        let allowed_growth = 200 * INDEX_RECORD_LEN as u64 + SHARD_HEAD_PROBE as u64;
        assert!(
            big_stats.bytes_read <= small_stats.bytes_read + allowed_growth,
            "a far larger shard must not cost more than its index: \
             small={} big={} shard_len={big_shard_len}",
            small_stats.bytes_read,
            big_stats.bytes_read
        );
        assert!(
            big_stats.bytes_read < big_shard_len / 4,
            "must read a small fraction of the shard: {} of {big_shard_len}",
            big_stats.bytes_read
        );
    }

    #[test]
    fn merge_with_local_never_overrides_a_known_local_path() {
        let _salt_guard = salt_guard();
        // `local` wins whenever it already knows a path — the corpus only
        // fills genuine gaps, never contests a path `local` already has an
        // opinion on (even a stale one; staleness is `warm_start`'s job).
        let corpus_dir = tempfile::tempdir().expect("corpus dir");
        let corpus = FactsCorpus::open(corpus_dir.path());
        let repo = tempfile::tempdir().expect("repo");
        let source = "export const a = 1;\n";
        std::fs::write(repo.path().join("a.ts"), source).unwrap();

        let reg = registry();
        // Populate the corpus with a *different* facts bundle under the
        // same path+hash (different entity name) than what `local` holds,
        // to make an accidental override observable.
        let mut corpus_file = persisted_file("a.ts", source);
        corpus_file.facts.entities[0].name = "from_corpus".to_string();
        corpus
            .populate_delta(
                None,
                &PersistedFacts::new(TableFingerprints::default(), vec![corpus_file]).as_borrowed(),
                &reg,
            )
            .expect("populate");

        let mut local_file = persisted_file("a.ts", source);
        local_file.facts.entities[0].name = "from_local".to_string();
        let local = PersistedFacts::new(TableFingerprints::default(), vec![local_file]);

        let (merged, stats) =
            corpus.merge_with_local(repo.path(), &["a.ts".to_string()], &reg, Some(&local));
        assert_eq!(
            stats.probed, 0,
            "a.ts is already known to `local`; never probed"
        );
        assert_eq!(
            merged.files.get("a.ts").unwrap().facts.entities[0].name,
            "from_local",
            "local's entry must win, not the corpus's"
        );
    }

    #[test]
    fn populate_delta_skips_unchanged_files() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let source = "export const a = 1;\n";
        let previous = PersistedFacts::new(
            TableFingerprints::default(),
            vec![persisted_file("a.ts", source)],
        );
        let current = PersistedFacts::new(
            TableFingerprints::default(),
            vec![
                persisted_file("a.ts", source),                  // unchanged
                persisted_file("b.ts", "export const b = 2;\n"), // new
            ],
        );
        let previous_index = previous.content_hash_index();
        let stats = corpus
            .populate_delta(Some(&previous_index), &current.as_borrowed(), &reg)
            .expect("populate");
        assert_eq!(
            stats.files_written, 1,
            "only the new/changed file should be written, not the unchanged one"
        );
    }

    #[test]
    fn concurrent_writers_do_not_corrupt_and_both_survive() {
        let _salt_guard = salt_guard();
        // Two threads populate the SAME shard bucket concurrently (a
        // deliberately forced collision, standing in for two `sem`
        // processes racing on the same machine-global corpus). Assert (a)
        // every shard the corpus wrote is still cleanly decodable
        // afterward — corruption is impossible by construction
        // (temp-file-then-rename) — and (b) thanks to `ShardLock`
        // serializing the read-merge-write, no writer's entries are lost.
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus_dir = dir.path().to_path_buf();

        // Find enough distinct single-file paths landing in bucket 0 to
        // give both threads real, non-overlapping work in the same shard.
        let mut in_bucket_0: Vec<String> = Vec::new();
        for i in 0.. {
            let candidate = format!("f{i}.ts");
            if corpus_bucket(&candidate) == 0 {
                in_bucket_0.push(candidate);
                if in_bucket_0.len() >= 20 {
                    break;
                }
            }
        }
        let (half_a, half_b) = in_bucket_0.split_at(10);
        let half_a = half_a.to_vec();
        let half_b = half_b.to_vec();

        let make_persisted = |paths: &[String]| {
            let files: Vec<PersistedFile> = paths
                .iter()
                .map(|p| persisted_file(p, &format!("export const v = '{p}';\n")))
                .collect();
            PersistedFacts::new(TableFingerprints::default(), files)
        };
        let current_a = make_persisted(&half_a);
        let current_b = make_persisted(&half_b);

        let dir_a = corpus_dir.clone();
        let dir_b = corpus_dir.clone();
        let t1 = std::thread::spawn(move || {
            let corpus = FactsCorpus::open(&dir_a);
            let reg = create_default_registry();
            corpus
                .populate_delta(None, &current_a.as_borrowed(), &reg)
                .expect("populate a")
        });
        let t2 = std::thread::spawn(move || {
            let corpus = FactsCorpus::open(&dir_b);
            let reg = create_default_registry();
            corpus
                .populate_delta(None, &current_b.as_borrowed(), &reg)
                .expect("populate b")
        });
        t1.join().expect("thread a");
        t2.join().expect("thread b");

        let corpus = FactsCorpus::open(&corpus_dir);
        // (a) no corruption: bucket 0's shard must still decode cleanly.
        let shard = corpus.load_shard(0);
        assert!(
            !shard.is_empty(),
            "the shard must be loadable and non-empty after two concurrent writers"
        );
        // (b) no lost update: both writers' entries must be present.
        for p in half_a.iter().chain(half_b.iter()) {
            let source = format!("export const v = '{p}';\n");
            let hash = content_hash(&source);
            assert!(
                corpus.get(p, hash, "ts-0.23-u16-exportspan").is_some(),
                "entry for {p} lost under concurrent writers"
            );
        }
    }

    // -------------------------------------------------------------------
    // ingest_remote: external ingestion + key validation
    // -------------------------------------------------------------------

    fn remote_fact(path: &str, source: &str, lang_salt: &str) -> RemoteFact {
        RemoteFact {
            facts: FileFacts {
                path: path.to_string(),
                content_hash: content_hash(source),
                entities: vec![entity(path, "f")],
            },
            precomputed: None,
            claimed_relative_path: path.to_string(),
            claimed_content_hash: content_hash(source),
            claimed_language_salt: lang_salt.to_string(),
            claimed_schema_version: FACTS_SCHEMA_VERSION,
        }
    }

    #[test]
    fn ingest_remote_accepts_a_correctly_keyed_fact_and_it_becomes_a_normal_hit() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let source = "export function shared() { return 1; }";

        let outcome = corpus
            .ingest_remote(
                &reg,
                vec![remote_fact("a.ts", source, "ts-0.23-u16-exportspan")],
            )
            .expect("ingest");
        assert_eq!(outcome.accepted.files_written, 1);
        assert!(outcome.rejected.is_empty());

        // Indistinguishable from a locally-populated entry: a plain `get`
        // hits it, and `merge_with_local` returns it as an ordinary corpus
        // hit — the same path a downstream `warm_start` consults.
        let hash = content_hash(source);
        assert!(corpus.get("a.ts", hash, "ts-0.23-u16-exportspan").is_some());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("a.ts"), source).unwrap();
        let (merged, stats) =
            corpus.merge_with_local(repo.path(), &["a.ts".to_string()], &reg, None);
        assert_eq!(
            stats.hits, 1,
            "ingested fact must serve a merge_with_local hit"
        );
        assert!(merged.files.contains_key("a.ts"));
    }

    #[test]
    fn ingest_remote_rejects_path_mismatch() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let source = "export const a = 1;\n";

        let mut fact = remote_fact("a.ts", source, "ts-0.23-u16-exportspan");
        fact.claimed_relative_path = "b.ts".to_string(); // tamper: wrong key
        let outcome = corpus.ingest_remote(&reg, vec![fact]).expect("ingest");

        assert_eq!(outcome.accepted.files_written, 0, "must not be written");
        assert_eq!(outcome.rejected.len(), 1);
        assert!(matches!(
            outcome.rejected[0].1,
            IngestError::PathMismatch { .. }
        ));
        assert!(corpus
            .get("a.ts", content_hash(source), "ts-0.23-u16-exportspan")
            .is_none());
        assert!(corpus
            .get("b.ts", content_hash(source), "ts-0.23-u16-exportspan")
            .is_none());
    }

    #[test]
    fn ingest_remote_rejects_content_hash_mismatch() {
        let _salt_guard = salt_guard();
        // The tamper shape this change's E2E proof cares about most: a
        // payload whose entities disagree with the hash the key claims —
        // exactly what a compromised/buggy server response could look like.
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let source = "export const a = 1;\n";

        let mut fact = remote_fact("a.ts", source, "ts-0.23-u16-exportspan");
        fact.claimed_content_hash = content_hash("totally different bytes");
        let outcome = corpus.ingest_remote(&reg, vec![fact]).expect("ingest");

        assert_eq!(outcome.accepted.files_written, 0);
        assert_eq!(outcome.rejected.len(), 1);
        assert!(matches!(
            outcome.rejected[0].1,
            IngestError::ContentHashMismatch { .. }
        ));
        assert!(corpus
            .get("a.ts", content_hash(source), "ts-0.23-u16-exportspan")
            .is_none());
    }

    #[test]
    fn ingest_remote_rejects_language_salt_mismatch() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let source = "export const a = 1;\n";

        // "ts-0.23-u16-exportspan" is the real salt for .ts; claim a stale/wrong one.
        let fact = remote_fact("a.ts", source, "ts-0.24-stale");
        let outcome = corpus.ingest_remote(&reg, vec![fact]).expect("ingest");

        assert_eq!(outcome.accepted.files_written, 0);
        assert_eq!(outcome.rejected.len(), 1);
        assert!(matches!(
            outcome.rejected[0].1,
            IngestError::LanguageSaltMismatch { .. }
        ));
    }

    #[test]
    fn ingest_remote_rejects_schema_version_mismatch() {
        let _salt_guard = salt_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let source = "export const a = 1;\n";

        let mut fact = remote_fact("a.ts", source, "ts-0.23-u16-exportspan");
        fact.claimed_schema_version = FACTS_SCHEMA_VERSION + 1;
        let outcome = corpus.ingest_remote(&reg, vec![fact]).expect("ingest");

        assert_eq!(outcome.accepted.files_written, 0);
        assert_eq!(outcome.rejected.len(), 1);
        assert!(matches!(
            outcome.rejected[0].1,
            IngestError::SchemaVersionMismatch { .. }
        ));
    }

    #[test]
    fn ingest_remote_batch_is_not_all_or_nothing() {
        let _salt_guard = salt_guard();
        // One tampered fact in a batch must not sink the others — each is
        // validated independently.
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = FactsCorpus::open(dir.path());
        let reg = registry();
        let good_source = "export const a = 1;\n";
        let bad_source = "export const b = 2;\n";

        let good = remote_fact("a.ts", good_source, "ts-0.23-u16-exportspan");
        let mut bad = remote_fact("b.ts", bad_source, "ts-0.23-u16-exportspan");
        bad.claimed_content_hash = content_hash("tampered");

        let outcome = corpus.ingest_remote(&reg, vec![good, bad]).expect("ingest");
        assert_eq!(outcome.accepted.files_written, 1);
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].0, "b.ts");
        assert!(corpus
            .get("a.ts", content_hash(good_source), "ts-0.23-u16-exportspan")
            .is_some());
        assert!(corpus
            .get("b.ts", content_hash(bad_source), "ts-0.23-u16-exportspan")
            .is_none());
    }
}
