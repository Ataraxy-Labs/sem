//! Building an index image from a built graph.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::parser::facts_store::corpus_identity_salt;
use crate::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};

use super::format::{self, *};

/// Same convention `facts_store.rs`/`graph.rs`/`session.rs` use: fall back to
/// a serial iterator when the `parallel` feature (default-on) is off, so the
/// `wasm` build and any consumer that opts out of `rayon` still compile.
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

/// Sort in parallel where `rayon` is available, serially otherwise — the same
/// default-on/wasm-off convention `maybe_par_iter!` above encodes. Sound to
/// swap in exactly when the comparator is a *total* order on the slice's
/// elements (an unstable sort is then a function, not a choice): both call
/// sites below tie-break on a unique key (`EntityInfo::id`, which is
/// `graph.entities`' own map key, and the entity index itself), so the output
/// is byte-identical either way — which the image sha256 gate then witnesses.
macro_rules! maybe_par_sort_unstable_by {
    ($slice:expr, $cmp:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $slice.par_sort_unstable_by($cmp)
        }
        #[cfg(not(feature = "parallel"))]
        {
            $slice.sort_unstable_by($cmp)
        }
    }};
}

/// Section-level timing for the index image build, gated by
/// `SEM_PROFILE_CACHE=1` — the same `OnceLock<bool>` shape
/// `resolve_profile::enabled()` and `sem-cli`'s `cache_profile_mark` use, and
/// the same contract: zero cost when unset, never changes a written byte,
/// only what goes to stderr. Added because `build_image` was a
/// single `write_query_index_build_image` mark worth 0.6-7.0 s per giant with
/// no attribution inside it.
fn image_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SEM_PROFILE_CACHE").as_deref() == Ok("1"))
}

fn image_mark(phase: &str, t0: std::time::Instant) {
    if image_profile_enabled() {
        eprintln!(
            "INDEX_IMAGE_PHASE phase={phase} ms={:.2}",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// A file's freshness fingerprint at build time, in the same terms the build
/// plane already uses (`shared_cache::file_freshness` compares exactly these).
#[derive(Clone, Debug, Default)]
pub struct FileFingerprint {
    pub path: String,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    pub content_hash: u64,
}

/// One file's deduped trigram set, extracted once from that file's bytes.
///
/// The point of the type is *when* it is built, not what it holds: a caller
/// that already has a file's bytes in hand (`sem-cli`'s single corpus read) extracts this inside the same parallel closure that
/// read them and then drops the bytes — the deforestation step, so the
/// corpus's content is never simultaneously resident. Handing the writer
/// pre-extracted sets rather than a `contents` map is what makes that
/// possible; `TrigramSource` is the sum of the two ways to say it.
#[derive(Clone, Debug, Default)]
pub struct FileTrigrams(FxHashSet<u32>);

impl FileTrigrams {
    /// Every 3-byte window of `content`, deduped — the same function
    /// [`TrigramSource::Contents`] applies internally, exposed so a caller
    /// with the bytes already in hand can apply it there instead.
    pub fn extract(content: &[u8]) -> Self {
        Self(extract_trigrams(content))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Where `TRIGRAM`'s postings come from. The two arms are extensionally
/// equal — `Contents(c)` ≡ `PerFile(c.map(FileTrigrams::extract))` — and the
/// writer's output is identical either way (the `trigram_source_agree` invariant
/// test in this module witnesses exactly that). They differ only in who paid
/// for the extraction and how long the bytes had to live.
pub enum TrigramSource<'a> {
    Contents(&'a HashMap<String, Vec<u8>>),
    PerFile(&'a HashMap<String, FileTrigrams>),
}

/// A directory's freshness fingerprint at build time — the `Complete`
/// freshness tier's unit. `path` is
/// root-relative with no trailing slash, `""` for the repo root itself (the
/// repo root is always present so a corpus that starts empty still has one
/// directory whose mtime bumps the moment a first file is added).
#[derive(Clone, Debug, Default)]
pub struct DirFingerprint {
    pub path: String,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
}

/// Build the image. Pure: bytes in, bytes out, no filesystem, no clock. No
/// file content — `TRIGRAM` is written zero-length ("tier absent"),
/// exactly the pre-S3 skeleton contract.
pub fn build(graph: &EntityGraph, files: &[FileFingerprint]) -> Vec<u8> {
    build_image(
        graph,
        files,
        &[],
        TrigramSource::Contents(&HashMap::new()),
        None,
        None,
        corpus_identity_salt(),
    )
    .0
}

/// The content-and-directory-aware entry point (S3 +):
/// `contents` maps a file's path (matching `FileFingerprint.path`) to its raw
/// bytes, which is what lets the writer extract trigram postings without
/// re-reading the filesystem — the caller (`sem-cli`'s `write_query_index`)
/// already read every file once to compute `content_hash`, so this reuses
/// that read rather than adding a second one. Files with no entry in
/// `contents` still get a `FileRec` (unchanged from [`build`]) but
/// contribute nothing to `TRIGRAM` — the same "degrade, never fail"
/// discipline as a missing fingerprint (see the comment on `supplied`
/// below). `dirs` is every distinct ancestor directory of every file in
/// `files` (all levels, plus `""` for the repo root — see
/// [`DirFingerprint`]'s doc); the caller derives this from the same file
/// list it already has, so the writer stays a pure function of what it's
/// handed rather than deriving ancestors itself.
///
/// Returns the image bytes and the trigram build's own stats, so a caller
/// (`index_probe`) can report the added build cost honestly rather than
/// inferring it from a wall-clock delta around the whole function.
pub fn build_with_content_and_dirs(
    graph: &EntityGraph,
    files: &[FileFingerprint],
    dirs: &[DirFingerprint],
    contents: &HashMap<String, Vec<u8>>,
) -> (Vec<u8>, TrigramBuildStats) {
    build_image(
        graph,
        files,
        dirs,
        TrigramSource::Contents(contents),
        None,
        None,
        corpus_identity_salt(),
    )
}

/// [`build_with_content_and_dirs`] plus the corpus's test classification
///. `test_entity_ids` is the set `EntityGraph::
/// filter_test_entities_with_custom_dirs` produces — the *same* value the
/// SQLite cache already writes into `entity_flags.is_test`, handed in rather
/// than recomputed, because the predicate needs `SemanticEntity.content` and
/// this writer only ever sees `EntityInfo`.
///
/// `None` means "this caller cannot classify" and is not the same as "no
/// entity is a test": it clears [`format::FLAG_ENTITY_TESTS`], which makes
/// every test-shaped reader decline instead of reading zeros as answers.
/// `sem-cli`'s cold-build index write (`commands::query`) is exactly that
/// caller.
pub fn build_with_content_and_dirs_and_tests(
    graph: &EntityGraph,
    files: &[FileFingerprint],
    dirs: &[DirFingerprint],
    contents: &HashMap<String, Vec<u8>>,
    test_entity_ids: Option<&std::collections::HashSet<&str>>,
) -> (Vec<u8>, TrigramBuildStats) {
    build_image(
        graph,
        files,
        dirs,
        TrigramSource::Contents(contents),
        test_entity_ids,
        None,
        corpus_identity_salt(),
    )
}

/// The widest content-based entry point — everything
/// [`build_with_content_and_dirs_and_tests`] carries, plus each entity's byte
/// span (`EntityRec.start_byte`/`end_byte`, `FORMAT_VERSION` 3).
/// `entity_byte_spans` maps an entity id to `(start_byte, end_byte)` — the
/// same shape `test_entity_ids` established for a datum the writer needs but
/// `EntityInfo` doesn't carry (the predicate/span needs `SemanticEntity`,
/// which this writer only ever sees as the caller's pre-derived map, never as
/// bodies in `graph`). An id absent from the map, or a `None` map entirely,
/// writes [`format::NONE_U32`] for both fields — "no span recorded", not
/// "span is `[0, 0)`" — exactly [`format::entity`]'s documented "absent"
/// convention for `start_byte`/`end_byte`.
pub fn build_with_content_and_dirs_and_tests_and_spans(
    graph: &EntityGraph,
    files: &[FileFingerprint],
    dirs: &[DirFingerprint],
    contents: &HashMap<String, Vec<u8>>,
    test_entity_ids: Option<&std::collections::HashSet<&str>>,
    entity_byte_spans: Option<&HashMap<&str, (u32, u32)>>,
) -> (Vec<u8>, TrigramBuildStats) {
    build_image(
        graph,
        files,
        dirs,
        TrigramSource::Contents(contents),
        test_entity_ids,
        entity_byte_spans,
        corpus_identity_salt(),
    )
}

/// [`build_with_content_and_dirs_and_tests_and_spans`] fed **pre-extracted**
/// trigrams instead of raw content (pass L) —
/// the entry point `sem-cli`'s `write_query_index` uses.
///
/// The caller extracts each file's trigrams inside the same parallel closure
/// that read its bytes, and never materializes a whole-corpus `contents`
/// map — on linux that map was 1.5 GB held alongside the per-file sets this
/// function builds anyway. Identical output to the `contents` entry point by
/// construction: both arms of [`TrigramSource`] feed the same
/// [`build_image`].
pub fn build_with_trigrams_and_dirs_and_tests_and_spans(
    graph: &EntityGraph,
    files: &[FileFingerprint],
    dirs: &[DirFingerprint],
    trigrams: &HashMap<String, FileTrigrams>,
    test_entity_ids: Option<&std::collections::HashSet<&str>>,
    entity_byte_spans: Option<&HashMap<&str, (u32, u32)>>,
) -> (Vec<u8>, TrigramBuildStats) {
    build_image(
        graph,
        files,
        dirs,
        TrigramSource::PerFile(trigrams),
        test_entity_ids,
        entity_byte_spans,
        corpus_identity_salt(),
    )
}

/// The full entry point every wrapper above funnels into, and the one place
/// that takes an explicit `salt` rather than defaulting to
/// [`corpus_identity_salt`] — `index/mod.rs`'s and this module's own unit
/// tests use it directly (via this `pub(crate)` visibility) when they need a
/// fixed salt for byte-identical assertions across runs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_image(
    graph: &EntityGraph,
    files: &[FileFingerprint],
    dirs: &[DirFingerprint],
    trigram_source: TrigramSource<'_>,
    test_entity_ids: Option<&std::collections::HashSet<&str>>,
    entity_byte_spans: Option<&HashMap<&str, (u32, u32)>>,
    salt: u64,
) -> (Vec<u8>, TrigramBuildStats) {
    let __sort_t0 = std::time::Instant::now();
    let mut entities: Vec<&EntityInfo> = graph.entities.values().collect();
    maybe_par_sort_unstable_by!(entities, |a: &&EntityInfo, b: &&EntityInfo| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.entity_type.cmp(&b.entity_type))
            .then(a.name.cmp(&b.name))
            .then(a.id.cmp(&b.id))
    });
    image_mark("entity_collect_sort", __sort_t0);

    // Every file the graph mentions gets a FileRec, whether or not the caller
    // supplied a fingerprint: the file list is what lets the read path skip
    // discovery entirely, so a missing fingerprint must degrade to "always
    // re-verify", never to "this file does not exist".
    let __fileidx_t0 = std::time::Instant::now();
    let supplied: HashMap<&str, &FileFingerprint> =
        files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut file_paths: Vec<&str> = entities.iter().map(|e| e.file_path.as_str()).collect();
    file_paths.extend(files.iter().map(|f| f.path.as_str()));
    file_paths.sort_unstable();
    file_paths.dedup();
    let file_index: HashMap<&str, u32> = file_paths
        .iter()
        .enumerate()
        .map(|(i, path)| (*path, i as u32))
        .collect();

    let ids_elided = entities
        .iter()
        .all(|e| id_tail_of(&e.id, &e.file_path).is_some());

    let mut arena = Arena::default();
    let mut kinds = Interner::default();

    let entity_index: HashMap<&str, u32> = entities
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i as u32))
        .collect();
    image_mark("file_and_entity_index", __fileidx_t0);

    let __entbytes_t0 = std::time::Instant::now();
    let mut entity_bytes = Vec::with_capacity(entities.len() * ENTITY_REC_LEN);
    for entity in &entities {
        let id_text = if ids_elided {
            id_tail_of(&entity.id, &entity.file_path).unwrap_or(&entity.id)
        } else {
            entity.id.as_str()
        };
        let kind = kinds.intern(&entity.entity_type, &mut arena);
        let parent = entity
            .parent_id
            .as_deref()
            .and_then(|id| entity_index.get(id).copied())
            .unwrap_or(NONE_U32);
        let flags = match test_entity_ids {
            Some(ids) if ids.contains(entity.id.as_str()) => format::entity::FLAG_IS_TEST,
            _ => 0,
        };
        let (start_byte, end_byte) = entity_byte_spans
            .and_then(|spans| spans.get(entity.id.as_str()))
            .copied()
            .unwrap_or((NONE_U32, NONE_U32));
        format::entity::write(
            &mut entity_bytes,
            arena.push_short(id_text),
            arena.push_interned(&entity.name),
            kind,
            flags,
            file_index[entity.file_path.as_str()],
            entity.start_line as u32,
            entity.end_line as u32,
            parent,
            start_byte,
            end_byte,
        );
    }

    // NAMES: entity indices sorted by name bytes, so a name query is an
    // equal_range rather than a scan. The tie-break on
    // index keeps the table a deterministic function of the graph, which is
    // what the consistency oracle's byte-equality property needs.
    image_mark("entities_section", __entbytes_t0);
    let __names_t0 = std::time::Instant::now();
    let mut name_order: Vec<u32> = (0..entities.len() as u32).collect();
    maybe_par_sort_unstable_by!(name_order, |a: &u32, b: &u32| {
        entities[*a as usize]
            .name
            .cmp(&entities[*b as usize].name)
            .then(a.cmp(b))
    });
    let mut name_bytes = Vec::with_capacity(name_order.len() * NAME_REC_LEN);
    for index in &name_order {
        name_bytes.extend_from_slice(&index.to_le_bytes());
    }

    // FILES carry the entity range, which requires entities to be grouped by
    // file — guaranteed by the sort above.
    image_mark("names_section", __names_t0);
    let __files_t0 = std::time::Instant::now();
    let mut ranges: HashMap<u32, (u32, u32)> = HashMap::new();
    for (i, entity) in entities.iter().enumerate() {
        let file = file_index[entity.file_path.as_str()];
        let entry = ranges.entry(file).or_insert((i as u32, i as u32));
        entry.1 = i as u32 + 1;
    }
    let mut file_bytes = Vec::with_capacity(file_paths.len() * FILE_REC_LEN);
    for (i, path) in file_paths.iter().enumerate() {
        let (lo, hi) = ranges.get(&(i as u32)).copied().unwrap_or((0, 0));
        let fingerprint = supplied.get(path);
        format::file::write(
            &mut file_bytes,
            arena.push(path),
            fingerprint.map_or(0, |f| f.mtime_secs),
            fingerprint.map_or(0, |f| f.mtime_nanos) as i32,
            fingerprint.map_or(0, |f| f.content_hash),
            lo,
            hi,
        );
    }

    let mut kind_bytes = Vec::with_capacity(kinds.entries.len() * KIND_REC_LEN);
    for (off, len) in &kinds.entries {
        kind_bytes.extend_from_slice(&off.to_le_bytes());
        kind_bytes.extend_from_slice(&len.to_le_bytes());
    }

    // REFS (S2; typed edges added): a serialization of
    // `graph.edges` grouped by `from_entity` (forward — "refs", what an
    // entity calls) and by `to_entity` (reverse — "callers", who calls it),
    // keyed through the same `entity_index` used for `parent` above. Built
    // directly from `edges` rather than from `graph.dependencies`/
    // `dependents` (which drop `ref_type`) — every real `EntityGraph`
    // constructor (`from_parts`, and both resolve-path constructors in
    // `parser::graph`) builds `dependencies`/`dependents`/`edges` in one pass
    // over the same deduped, sorted edge list, so grouping `edges` here
    // reproduces the identical per-entity target order those maps already
    // have, while also carrying the kind. The index is a transport, not a
    // re-derivation: a mismatch here is a serialization bug, never a
    // resolution one ("extraction identity" obligation).
    image_mark("files_and_kinds_section", __files_t0);
    let refs_typed = entities.len() <= format::refs::MAX_ENTITIES;
    let __refs_t0 = std::time::Instant::now();
    let refs_bytes = build_refs_section(&entities, &entity_index, graph, refs_typed);
    image_mark("refs_section", __refs_t0);

    // TRIGRAM (S3): postings over **file** indices, built from
    // whatever `contents` the caller supplied. A file with no content entry
    // (fingerprint-only, or the caller opted out entirely via `build`)
    // contributes nothing — never a partial or guessed posting — which is
    // why `build` above passes an empty map and gets a zero-length section,
    // identical to the pre-S3 skeleton contract.
    let __trig_t0 = std::time::Instant::now();
    let (trigram_bytes, trigram_stats) = build_trigram_section(&file_paths, &trigram_source);
    image_mark("trigram_section", __trig_t0);

    // DIRS (`Complete` freshness —): sorted
    // by path bytes, same convention FILES uses, though the reader's
    // parallel-stat sweep doesn't need the ordering — sorting only buys
    // determinism for the consistency oracle's byte-equality property.
    // Callers may pass unsorted/duplicate `dirs`; dedup here rather than push
    // that obligation onto every caller.
    let __dirs_t0 = std::time::Instant::now();
    let mut dir_paths: Vec<&DirFingerprint> = dirs.iter().collect();
    dir_paths.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    dir_paths.dedup_by(|a, b| a.path == b.path);
    let mut dir_bytes = Vec::with_capacity(dir_paths.len() * DIR_REC_LEN);
    for d in &dir_paths {
        format::dir::write(
            &mut dir_bytes,
            arena.push(&d.path),
            d.mtime_secs,
            d.mtime_nanos as i32,
        );
    }

    image_mark("dirs_section", __dirs_t0);
    let __asm_t0 = std::time::Instant::now();
    let bytes = assemble(
        Header {
            format_version: FORMAT_VERSION,
            flags: (if ids_elided { FLAG_IDS_ELIDED } else { 0 })
                | (if refs_typed { FLAG_REFS_TYPED } else { 0 })
                | (if test_entity_ids.is_some() {
                    FLAG_ENTITY_TESTS
                } else {
                    0
                }),
            build_salt: salt,
            entity_count: entities.len() as u64,
            file_count: file_paths.len() as u64,
            dir_count: dir_paths.len() as u64,
            kind_count: kinds.entries.len() as u32,
            sections: [SectionRef::EMPTY; SECTION_COUNT],
        },
        arena.bytes,
        file_bytes,
        entity_bytes,
        name_bytes,
        refs_bytes,
        kind_bytes,
        trigram_bytes,
        dir_bytes,
    );
    image_mark("assemble", __asm_t0);
    (bytes, trigram_stats)
}

/// `REFS` section bytes: CSR in both directions (typed
/// postings added).
///
/// ```text
///  0  fwd_edge_count u32     8  fwd_offsets  (entity_count+1) x u32
///  4  rev_edge_count u32       fwd_targets   fwd_edge_count x u32
///                               rev_offsets  (entity_count+1) x u32
///                               rev_targets  rev_edge_count x u32
/// ```
/// `offsets[i]..offsets[i+1]` is a range of *indices into the targets array*
/// (standard CSR row-offsets, not byte offsets), so a row is `targets[lo..hi]`
/// after one extra indirection. Each `u32` target is `format::refs::pack`ed:
/// low 28 bits the target entity index, high 4 the edge's `ref_type` — no
/// delta-encoding, no new section, the contract's original default kept and
/// extended in place (see `format::refs`'s doc for the encoding trade).
///
/// `typed` is `false` only in the `MAX_ENTITIES`-overflow fallback (checked
/// by the caller): every target still packs correctly (kind `0`), but the
/// header's `FLAG_REFS_TYPED` bit tells readers not to trust it as a real
/// `Calls`.
fn build_refs_section(
    entities: &[&EntityInfo],
    entity_index: &HashMap<&str, u32>,
    graph: &EntityGraph,
    typed: bool,
) -> Vec<u8> {
    let n = entities.len();

    // Grouped directly from `graph.edges` (not `dependencies`/`dependents`,
    // which drop `ref_type`) — see the call site's comment for why this
    // reproduces the identical per-entity order those maps already have.
    // The forward and reverse halves read the same immutable inputs and share
    // no mutable state — separation-algebra disjointness, exactly the
    // condition already uses to license a `par_iter` —
    // so building each half is one closure and the two run under a `join`.
    // Each half still walks `entities` in the identical order it did before,
    // so both CSRs are byte-identical to the serial version. (:
    // `refs_section` was 0.8-1.4 s of a wholly serial image build.)
    let build_half = |key_of: fn(&EntityRef) -> (&str, &str)| {
        let mut adj: HashMap<&str, Vec<(&str, &RefType)>> =
            HashMap::with_capacity_and_hasher(n, Default::default());
        for e in &graph.edges {
            let (row, target) = key_of(e);
            adj.entry(row).or_default().push((target, &e.ref_type));
        }
        let mut offsets = Vec::with_capacity(n + 1);
        let mut targets: Vec<u32> = Vec::new();
        offsets.push(0u32);
        for entity in entities {
            if let Some(row) = adj.get(entity.id.as_str()) {
                for (other_id, ref_type) in row {
                    if let Some(&idx) = entity_index.get(*other_id) {
                        let kind = if typed { ref_kind_u8(ref_type) } else { 0 };
                        targets.push(format::refs::pack(idx, kind));
                    }
                }
            }
            offsets.push(targets.len() as u32);
        }
        (offsets, targets)
    };

    #[cfg(feature = "parallel")]
    let ((fwd_offsets, fwd_targets), (rev_offsets, rev_targets)) = rayon::join(
        || build_half(|e| (e.from_entity.as_str(), e.to_entity.as_str())),
        || build_half(|e| (e.to_entity.as_str(), e.from_entity.as_str())),
    );
    #[cfg(not(feature = "parallel"))]
    let ((fwd_offsets, fwd_targets), (rev_offsets, rev_targets)) = (
        build_half(|e| (e.from_entity.as_str(), e.to_entity.as_str())),
        build_half(|e| (e.to_entity.as_str(), e.from_entity.as_str())),
    );

    let mut out = Vec::with_capacity(
        8 + (fwd_offsets.len() + rev_offsets.len()) * 4
            + (fwd_targets.len() + rev_targets.len()) * 4,
    );
    out.extend_from_slice(&(fwd_targets.len() as u32).to_le_bytes());
    out.extend_from_slice(&(rev_targets.len() as u32).to_le_bytes());
    for o in &fwd_offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    for t in &fwd_targets {
        out.extend_from_slice(&t.to_le_bytes());
    }
    for o in &rev_offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    for t in &rev_targets {
        out.extend_from_slice(&t.to_le_bytes());
    }
    out
}

/// `RefType`'s packed 4-bit encoding — `format::refs`'s doc names this the
/// one place the mapping is defined; nothing outside `build_refs_section`
/// needs to agree with it except `reader::ref_kind_from_u8`, its inverse.
fn ref_kind_u8(ref_type: &RefType) -> u8 {
    match ref_type {
        RefType::Calls => 0,
        RefType::TypeRef => 1,
        RefType::Imports => 2,
    }
}

/// Whole-`TRIGRAM`-section budget (: "if the postings
/// exceed 40 MB the fallback is to index only files under a size cap and
/// declare the tier partial in `flags`, never to blow the budget silently").
/// This change's chosen fallback (see `stop_list_to_budget` below) is
/// stop-listing the highest-document-frequency trigrams rather than dropping
/// whole files, because a dropped file would silently make `sem grep` miss
/// real matches in that file — a correctness regression a caller has no way
/// to detect. Stop-listing degrades *selectivity* (queries needing a
/// stop-listed trigram fall back to scanning every file that also matches
/// the query's other trigrams, or to a full scan if it was the only one),
/// never *correctness*: `verify` (the real matcher) still runs over whatever
/// candidate set results, so a stop-listed trigram can only make a query
/// slower, never wrong.
pub const TRIGRAM_BUDGET_BYTES: usize = 40 * 1024 * 1024;

/// What `build_trigram_section` measured, for `index_probe`'s `WRITE` line
/// and reported build cost — a caller should never have to
/// infer this from a wall-clock delta around the whole image build.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrigramBuildStats {
    pub extract_ms: f64,
    pub merge_ms: f64,
    pub distinct_trigrams: usize,
    pub stopped_trigrams: usize,
    pub posting_total: usize,
    pub section_bytes: usize,
    pub files_indexed: usize,
}

/// Every 3-byte window of `content`, deduped — the unit `TRIGRAM` postings
/// are keyed on. Whole-byte-stream windows (not per-line): a cross-line match
/// still needs its trigrams findable, and `grep::required_trigrams` is what
/// decides whether a pattern can use this prefilter at all (
/// "the text tier can answer without the entity tier" boundary — this
/// function has no opinion on patterns, only on bytes).
fn extract_trigrams(content: &[u8]) -> FxHashSet<u32> {
    let mut set = FxHashSet::default();
    if content.len() < 3 {
        return set;
    }
    set.reserve(content.len() / 4);
    for w in content.windows(3) {
        set.insert(format::trigram::pack([w[0], w[1], w[2]]));
    }
    set
}

/// Build `TRIGRAM`'s bytes. `file_index` is the same path→index map the rest
/// of the writer uses, so postings and `FILES`/`ENTITIES` agree on what a
/// file index means without a second table. Files absent from `contents`
/// (fingerprint-only or the caller opted out) simply contribute no trigrams —
/// see the call site's comment.
fn build_trigram_section(
    file_paths: &[&str],
    source: &TrigramSource<'_>,
) -> (Vec<u8>, TrigramBuildStats) {
    build_trigram_section_with_budget(file_paths, source, TRIGRAM_BUDGET_BYTES)
}

/// [`build_trigram_section`] with an explicit budget — split out so tests can
/// force stop-listing deterministically with a tiny budget rather than
/// needing a corpus large enough to exceed 40 MB for real.
fn build_trigram_section_with_budget(
    file_paths: &[&str],
    source: &TrigramSource<'_>,
    budget_bytes: usize,
) -> (Vec<u8>, TrigramBuildStats) {
    let extract_started = std::time::Instant::now();
    // Indexed by file_index, so the merge below can iterate 0..file_count and
    // have every posting row come out already sorted ascending by
    // file index — no per-row sort needed.
    //
    // `PerFile` extracted these already (in the caller's own read closure);
    // it borrows them rather than cloning, which is the whole point of that
    // arm — a clone here would re-introduce the full-corpus copy the fused
    // read exists to delete.
    let empty: FxHashSet<u32> = FxHashSet::default();
    let extracted: Vec<FxHashSet<u32>> = match source {
        TrigramSource::Contents(contents) => maybe_par_iter!(file_paths)
            .map(|path| {
                contents
                    .get(*path)
                    .map(|bytes| extract_trigrams(bytes))
                    .unwrap_or_default()
            })
            .collect(),
        TrigramSource::PerFile(_) => Vec::new(),
    };
    let per_file: Vec<&FxHashSet<u32>> = match source {
        TrigramSource::Contents(_) => extracted.iter().collect(),
        TrigramSource::PerFile(sets) => file_paths
            .iter()
            .map(|path| sets.get(*path).map(|t| &t.0).unwrap_or(&empty))
            .collect(),
    };
    let extract_ms = extract_started.elapsed().as_secs_f64() * 1000.0;
    let files_indexed = per_file.iter().filter(|s| !s.is_empty()).count();

    let merge_started = std::time::Instant::now();
    let mut postings: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    for (file_idx, set) in per_file.iter().enumerate() {
        for &trigram in set.iter() {
            postings.entry(trigram).or_default().push(file_idx as u32);
        }
    }
    let mut entries: Vec<(u32, Vec<u32>)> = postings.into_iter().collect();
    let stopped = stop_list_to_budget(&mut entries, budget_bytes);
    entries.sort_unstable_by_key(|(key, _)| *key);
    let merge_ms = merge_started.elapsed().as_secs_f64() * 1000.0;

    let trigram_count = entries.len();
    let posting_total: usize = entries.iter().map(|(_, v)| v.len()).sum();

    if trigram_count == 0 {
        // Genuinely zero-length, not a header-only 12-byte section: a
        // zero-length section is what `SectionRef::is_absent` reads as
        // "tier absent", and no content (or content too short to have a
        // single trigram) must degrade to exactly that contract, not a
        // slightly-larger empty tier that behaves the same but breaks the
        // absence check other callers rely on.
        let stats = TrigramBuildStats {
            extract_ms,
            merge_ms,
            distinct_trigrams: 0,
            stopped_trigrams: 0,
            posting_total: 0,
            section_bytes: 0,
            files_indexed,
        };
        return (Vec::new(), stats);
    }

    let mut keys = Vec::with_capacity(trigram_count * 4);
    let mut offsets = Vec::with_capacity((trigram_count + 1) * 4);
    let mut targets = Vec::with_capacity(posting_total * 4);
    let mut running = 0u32;
    offsets.extend_from_slice(&running.to_le_bytes());
    for (key, files) in &entries {
        let flagged = if stopped.contains(key) {
            key | format::trigram::STOP_FLAG
        } else {
            *key
        };
        keys.extend_from_slice(&flagged.to_le_bytes());
        for f in files {
            targets.extend_from_slice(&f.to_le_bytes());
        }
        running += files.len() as u32;
        offsets.extend_from_slice(&running.to_le_bytes());
    }

    let mut out = Vec::with_capacity(8 + keys.len() + offsets.len() + targets.len());
    out.extend_from_slice(&(trigram_count as u32).to_le_bytes());
    out.extend_from_slice(&(posting_total as u32).to_le_bytes());
    out.extend_from_slice(&keys);
    out.extend_from_slice(&offsets);
    out.extend_from_slice(&targets);

    let stats = TrigramBuildStats {
        extract_ms,
        merge_ms,
        distinct_trigrams: trigram_count,
        stopped_trigrams: stopped.len(),
        posting_total,
        section_bytes: out.len(),
        files_indexed,
    };
    (out, stats)
}

/// Stop-list the highest-document-frequency trigrams until the section fits
/// `budget_bytes`, greedy-largest-first — the postings array is the only part
/// of the section whose size is corpus-dependent in an unbounded way (`KEYS`
/// and `OFFSETS` are `O(distinct trigrams)`, which the measured corpora put
/// in the hundreds of thousands, not tens of millions), so freeing the
/// biggest rows first reaches the budget in the fewest stop-listed trigrams.
/// Returns the set of stop-listed trigram values (before the `STOP_FLAG` is
/// folded into their `KEYS` entry) and clears their postings vectors in
/// place — a stop-listed trigram keeps its `KEYS` slot (so a reader can tell
/// "zero files" from "too common to store" — see the `trigram` format
/// module's doc) but stores an empty row.
fn stop_list_to_budget(entries: &mut [(u32, Vec<u32>)], budget_bytes: usize) -> FxHashSet<u32> {
    let mut stopped = FxHashSet::default();
    let fixed = |count: usize| 8 + count * 4 + (count + 1) * 4;
    let posting_total: usize = entries.iter().map(|(_, v)| v.len()).sum();
    let mut current_bytes = fixed(entries.len()) + posting_total * 4;
    if current_bytes <= budget_bytes {
        return stopped;
    }

    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_unstable_by_key(|&i| std::cmp::Reverse(entries[i].1.len()));
    for i in order {
        if current_bytes <= budget_bytes {
            break;
        }
        let freed = entries[i].1.len();
        if freed == 0 {
            continue;
        }
        current_bytes -= freed * 4;
        stopped.insert(entries[i].0);
        entries[i].1.clear();
    }
    stopped
}

/// Lay the sections out back to back, 8-byte aligned, and patch the header's
/// section table with the offsets that resulted.
#[allow(clippy::too_many_arguments)]
fn assemble(
    mut header: Header,
    strings: Vec<u8>,
    files: Vec<u8>,
    entities: Vec<u8>,
    names: Vec<u8>,
    refs: Vec<u8>,
    kinds: Vec<u8>,
    trigram: Vec<u8>,
    dirs: Vec<u8>,
) -> Vec<u8> {
    // `dirs` is EMPTY (zero-length) whenever the caller didn't supply any
    // `DirFingerprint`s — a zero-length section reads as "tier absent"
    // (`SectionRef::is_absent`), the same contract REFS/TRIGRAM already use,
    // so `Complete` freshness can land on some call sites (write_query_index)
    // without breaking others (`build`/`index_probe`'s dirs-less callers) or
    // bumping the format version.
    let mut body: Vec<(usize, Vec<u8>)> = vec![
        (SEC_STRINGS, strings),
        (SEC_FILES, files),
        (SEC_ENTITIES, entities),
        (SEC_NAMES, names),
        (SEC_REFS, refs),
        (SEC_TRIGRAM, trigram),
        (SEC_DIRS, dirs),
        (SEC_KINDS, kinds),
    ];

    if image_profile_enabled() {
        for (section, bytes) in &body {
            eprintln!("INDEX_IMAGE_SECTION sec={section} bytes={}", bytes.len());
        }
    }
    let mut out = vec![0u8; HEADER_LEN];
    for (section, bytes) in &mut body {
        out.resize(format::align_up(out.len()), 0);
        header.sections[*section] = SectionRef {
            offset: out.len() as u64,
            len: bytes.len() as u64,
        };
        out.append(bytes);
    }

    let mut head = Vec::with_capacity(HEADER_LEN);
    header.write(&mut head);
    out[..HEADER_LEN].copy_from_slice(&head);
    out
}

/// The measured-universal id rule: every id begins with `"<file_path>::"`.
/// Verified over all 454,528 entities of the TypeScript corpus before it was
/// adopted — and the two *plausible* rules it replaced were both false there.
/// Returning `None` for a violation is what lets the writer fall back to
/// whole ids for the entire image rather than guess.
fn id_tail_of<'a>(id: &'a str, file_path: &str) -> Option<&'a str> {
    id.strip_prefix(file_path)?.strip_prefix("::")
}

#[derive(Default)]
struct Arena {
    bytes: Vec<u8>,
    interned: HashMap<String, (u32, u16)>,
}

impl Arena {
    fn push(&mut self, text: &str) -> (u32, u32) {
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(text.as_bytes());
        (off, text.len() as u32)
    }

    /// For fields whose length is stored as `u16`. Over-long strings are
    /// truncated on a char boundary rather than corrupting the arena; at
    /// 64 KiB this is unreachable for names and id tails in practice.
    fn push_short(&mut self, text: &str) -> (u32, u16) {
        let text = clamp(text);
        let (off, len) = self.push(text);
        (off, len as u16)
    }

    /// Names repeat ~8× on average (454,528 entities, 56,552 distinct names),
    /// so interning them is the difference between 3.6 MB and 0.7 MB.
    fn push_interned(&mut self, text: &str) -> (u32, u16) {
        if let Some(hit) = self.interned.get(text) {
            return *hit;
        }
        let slot = self.push_short(text);
        self.interned.insert(text.to_string(), slot);
        slot
    }
}

#[derive(Default)]
struct Interner {
    entries: Vec<(u32, u32)>,
    seen: HashMap<String, u16>,
}

impl Interner {
    fn intern(&mut self, text: &str, arena: &mut Arena) -> u16 {
        if let Some(id) = self.seen.get(text) {
            return *id;
        }
        let id = self.entries.len() as u16;
        let slot = arena.push(text);
        self.entries.push(slot);
        self.seen.insert(text.to_string(), id);
        id
    }
}

fn clamp(text: &str) -> &str {
    if text.len() <= u16::MAX as usize {
        return text;
    }
    let mut end = u16::MAX as usize;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Temp file + rename, verbatim the `FactsStore::save` discipline: no lock
/// (a write is a whole-image replacement, so nothing can be lost-updated) and
/// no `fsync` (a torn image is a clean miss and the next build rewrites it).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod trigram_tests {
    use super::*;

    #[test]
    fn extract_trigrams_finds_every_window() {
        let set = extract_trigrams(b"abcd");
        assert_eq!(set.len(), 2); // "abc", "bcd"
        assert!(set.contains(&format::trigram::pack([b'a', b'b', b'c'])));
        assert!(set.contains(&format::trigram::pack([b'b', b'c', b'd'])));
    }

    #[test]
    fn extract_trigrams_of_short_content_is_empty() {
        assert!(extract_trigrams(b"ab").is_empty());
        assert!(extract_trigrams(b"").is_empty());
    }

    #[test]
    fn extract_trigrams_dedupes_within_a_file() {
        // "aaaa" has one distinct trigram ("aaa"), occurring at two offsets.
        assert_eq!(extract_trigrams(b"aaaa").len(), 1);
    }

    #[test]
    fn stop_list_leaves_entries_under_budget_untouched() {
        let mut entries = vec![(1u32, vec![0u32, 1, 2]), (2u32, vec![0u32])];
        let stopped = stop_list_to_budget(&mut entries, usize::MAX);
        assert!(stopped.is_empty());
        assert_eq!(entries[0].1, vec![0, 1, 2]);
    }

    #[test]
    fn stop_list_clears_the_biggest_rows_first_until_under_budget() {
        // Three trigrams with posting-list lengths 5, 3, 1. Budget forces
        // exactly the biggest one to be cleared.
        let mut entries = vec![
            (10u32, (0..5).collect::<Vec<u32>>()),
            (20u32, (0..3).collect::<Vec<u32>>()),
            (30u32, (0..1).collect::<Vec<u32>>()),
        ];
        let fixed = 8 + entries.len() * 4 + (entries.len() + 1) * 4;
        // Budget that fits everything except the 5-row entry (4 postings * 4B slack).
        let budget = fixed + (3 + 1) * 4;
        let stopped = stop_list_to_budget(&mut entries, budget);
        assert_eq!(stopped.len(), 1);
        assert!(stopped.contains(&10));
        assert!(entries.iter().find(|(k, _)| *k == 10).unwrap().1.is_empty());
        assert_eq!(entries.iter().find(|(k, _)| *k == 20).unwrap().1.len(), 3);
    }

    #[test]
    fn a_stop_listed_trigram_keeps_its_keys_slot_with_an_empty_row() {
        let mut contents: HashMap<String, Vec<u8>> = HashMap::new();
        // Every file shares "aaa" (forces it common); only "a.ts" has "xyz".
        contents.insert("a.ts".to_string(), b"aaa xyz".to_vec());
        contents.insert("b.ts".to_string(), b"aaa".to_vec());
        let file_paths = ["a.ts", "b.ts"];
        // A budget too small for "aaa"'s 2-file row but big enough for the
        // rest: fixed overhead for ~a dozen distinct trigrams here is small,
        // so pick a budget that's exactly the fixed cost (stops everything
        // with any postings).
        let (bytes, stats) =
            build_trigram_section_with_budget(&file_paths, &TrigramSource::Contents(&contents), 0);
        assert!(stats.stopped_trigrams > 0);
        assert_eq!(stats.posting_total, 0);
        let (count, total) = format::trigram::header(&bytes).unwrap();
        assert_eq!(total, 0);
        assert_eq!(format::trigram::expected_len(count, total), bytes.len());
        // Every key present must be flagged stop (posting_total is 0 but the
        // trigrams themselves are still known-present, not "absent").
        for slot in 0..count {
            let entry = format::trigram::key(&bytes, slot).unwrap();
            assert!(format::trigram::is_stop(entry));
        }
    }

    #[test]
    fn build_with_content_populates_trigram_postings() {
        use crate::parser::graph::{EntityGraph, EntityInfoMap};
        let graph = EntityGraph::from_parts(EntityInfoMap::default(), Vec::new());
        let mut contents: HashMap<String, Vec<u8>> = HashMap::new();
        contents.insert("a.ts".to_string(), b"needle".to_vec());
        contents.insert("b.ts".to_string(), b"haystack".to_vec());
        let files = vec![
            FileFingerprint {
                path: "a.ts".to_string(),
                ..Default::default()
            },
            FileFingerprint {
                path: "b.ts".to_string(),
                ..Default::default()
            },
        ];
        let (bytes, stats) = build_image(
            &graph,
            &files,
            &[],
            TrigramSource::Contents(&contents),
            None,
            None,
            0xF00D,
        );
        assert!(stats.distinct_trigrams > 0);
        let index = crate::index::QueryIndex::from_bytes_with_salt(bytes, 0xF00D).unwrap();
        assert!(index.has_trigram());
        let post = index.trigram_posting([b'n', b'e', b'e']);
        match post {
            crate::index::TrigramPosting::Present(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(index.file_path(files[0]), "a.ts");
            }
            other => panic!("expected Present, got {other:?}"),
        }
        // "hay" only occurs in b.ts.
        let post = index.trigram_posting([b'h', b'a', b'y']);
        match post {
            crate::index::TrigramPosting::Present(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(index.file_path(files[0]), "b.ts");
            }
            other => panic!("expected Present, got {other:?}"),
        }
        // A trigram that never occurs anywhere is Absent, not Stopped.
        let post = index.trigram_posting([b'z', b'z', b'z']);
        assert_eq!(post, crate::index::TrigramPosting::Absent);
    }

    #[test]
    fn build_without_content_writes_trigram_absent() {
        use crate::parser::graph::{EntityGraph, EntityInfoMap};
        let graph = EntityGraph::from_parts(EntityInfoMap::default(), Vec::new());
        let files = vec![FileFingerprint {
            path: "a.ts".to_string(),
            ..Default::default()
        }];
        let (bytes, _) = build_image(
            &graph,
            &files,
            &[],
            TrigramSource::Contents(&HashMap::new()),
            None,
            None,
            0xF00D,
        );
        let index = crate::index::QueryIndex::from_bytes_with_salt(bytes, 0xF00D).unwrap();
        assert!(!index.has_trigram());
    }
}
