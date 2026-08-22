//! A long-lived graph build session: cold once, then warm rebuilds that redo
//! work only for changed files and their blast radius.
//!
//! # Why a session and not a free function
//!
//! Everything a warm rebuild reuses is *derived state*: per-file entities,
//! per-file scope facts, per-file edges, and the fingerprints that say whether
//! reusing them is still legal. Something has to own that between requests. The
//! sem-cloud server and the MCP server both hold one process across many
//! requests, so an in-process session is the right home for the in-memory
//! case. For a *fresh* process — a `sem` CLI invocation, which does not live
//! across requests — [`GraphSession::export_persisted`] and
//! [`GraphSession::warm_start`] are the disk-crossing pair
//! `crate::parser::facts_store::FactsStore` sits behind:
//! [`FileFacts`], `PrecomputedFileFacts`, and `CachedFileResolution` are all
//! `serde`-serializable for exactly this. [`GraphSession::export_facts`]
//! remains the narrower, entities-only export.
//!
//! # The contract
//!
//! A warm rebuild produces a graph **bit-identical** to a cold build of the same
//! file contents. That is not a hope the tests check; it is structural. There is
//! one build implementation ([`EntityGraph::build_incremental_core`]), a warm
//! rebuild runs every stage of it in the same order, and reuse happens *inside*
//! the two per-file resolution closures — so every merge, dedupe, sort and index
//! downstream sees exactly the input a cold build would have handed it. The
//! oracle tests exist to catch a mistake in the invalidation rule, not to
//! establish the equality in the first place.
//!
//! # What is conservative here
//!
//! * Only files whose detected language is JS/TS, Python, Go, or Rust may reuse
//! resolution results (extended this past JS/TS-only, 's
//!   original scope). Every other language is held permanently RED: slower,
//!   never wrong. See [`crate::parser::import_resolution::is_reuse_eligible_file`]
//! and "Universal GREEN eligibility" section for the
//!   full per-language verdict and why each remaining language stayed
//!   conservative.
//! * A change to the *file list* (an add or a delete) disables reuse entirely on
//!   corpora large enough to resolve in chunks, because chunk membership — and
//!   therefore the chunk-scoped return-type and instance-attribute maps every
//!   file resolves against — shifts underneath every file after the insertion
//!   point. Smaller corpora resolve in one pass and take adds and deletes
//!   incrementally.
//! * Every file the caller names in `changed_paths` is RED whether or not its
//!   bytes actually differ.

use std::path::{Path, PathBuf};

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::model::entity::SemanticEntity;
use crate::parser::facts_store::{
    FileFactsRef, PersistedFacts, PersistedFactsRef, PersistedFileRef,
};
use crate::parser::graph::{BuildCarry, CachedImportScan, EntityGraph, PARSED_FILE_REUSE_LIMIT};
use crate::parser::import_resolution::is_reuse_eligible_file;
use crate::parser::incremental::{
    content_hash, CachedFileResolution, FileFacts, Incremental, RebuildStats, TableFingerprints,
};
use crate::parser::registry::ParserRegistry;
use crate::parser::scope_resolve::PrecomputedFileFacts;

/// Same convention `graph.rs`/`scope_resolve.rs` already use: `rayon` under
/// the `parallel` feature, a plain sequential iterator otherwise (non-parallel
/// builds and wasm). [`GraphSession::warm_start`] uses this for its per-file
/// content-hash check — reading and hashing every file in a large corpus
/// serially would reintroduce exactly the kind of single-threaded loop
/// spent an entire fix phase eliminating from pass 1.
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

/// A graph build that can be refreshed cheaply after an edit.
pub struct GraphSession {
    root: PathBuf,
    file_paths: Vec<String>,
    graph: EntityGraph,
    all_entities: Vec<SemanticEntity>,
    /// `(path, start, len)` spans into `all_entities`, in build order. Lets the
    /// next rebuild hand a GREEN file's entities back by moving them.
    entity_spans: Vec<(String, usize, usize)>,
    content_hashes: HashMap<String, u64>,
    precomputed: HashMap<String, PrecomputedFileFacts>,
    resolution: HashMap<String, CachedFileResolution>,
    fingerprints: TableFingerprints,
    stats: RebuildStats,
    /// Per-file cached import scans (JS/TS only) —.
    import_scans: HashMap<String, CachedImportScan>,
    /// The import table itself, maintained in place across rebuilds instead
    /// of rebuilt whole —.
    import_table: HashMap<(String, String), String>,
    /// Every producing file's current set of `import_table` keys, so a
    /// changed file's stale entries can be removed without a full-table
    /// scan —.
    import_keys: HashMap<String, Vec<(String, String)>>,
    /// `symbol_table`/`class_members`/`owner_members`/`entity_ranges`,
    /// maintained in place across rebuilds instead of rebuilt whole every
    /// time —, generalizing the `import_table`/`import_keys`
    /// pattern above to `PreBuiltLookups`'s other tables. `entity_map` has
    /// no field of its own here: it round-trips through `graph.entities`
    /// instead (see `run()`), since that is already the field's permanent,
    /// public home.
    symbol_table: HashMap<String, Vec<String>>,
    class_members: HashMap<String, Vec<(String, String)>>,
    owner_members: HashMap<String, Vec<(String, String)>>,
    entity_ranges: HashMap<String, Vec<(usize, usize, String)>>,
    /// Bag-of-words' parent → child-position index, session-owned and
    /// maintained by the same function as the four above. Not
    /// persisted by `FactsStore`: it is derivable from the entities that store
    /// already holds, and re-deriving it once on the first rebuild after a
    /// warm start costs less than serializing ~450k byte spans.
    child_ranges: crate::parser::graph::ChildRangeIndex,
    /// The five corpus tables' fingerprints plus their Python wildcard-import
    /// XOR guard, carried across rebuilds so a warm rebuild updates only the
    /// keys it touched. Not persisted: `PersistedFacts` already
    /// carries the *whole* fingerprint map a warm start needs, and a fresh
    /// process re-derives this split copy on its first (whole-rebuild) build.
    corpus_fp: crate::parser::incremental::TableFingerprints,
    wildcard_guard: u64,
    /// Whether the five fields above (plus `graph.entities`) currently
    /// reflect the *whole* corpus, not just whatever an incremental step
    /// last touched — see `graph::BuildCarry::entity_lookups_primed`'s doc
    /// comment for the staleness hazard this guards against.
    entity_lookups_primed: bool,
    /// Build counter, incremented once per `run`. `resolution` is moved through
    /// each build rather than rebuilt, and this is what tells an
    /// entry that build produced from one it merely carried — see
    /// `incremental::Incremental`'s doc comment.
    generation: u64,
}

impl GraphSession {
    /// Cold build. Identical in output to [`EntityGraph::build`] — it *is*
    /// `EntityGraph::build`, run with a carry that records facts as it goes.
    pub fn build(root: &Path, file_paths: &[String], registry: &ParserRegistry) -> Self {
        let mut session = GraphSession {
            root: root.to_path_buf(),
            file_paths: file_paths.to_vec(),
            graph: EntityGraph::from_parts(HashMap::default(), Vec::new()),
            all_entities: Vec::new(),
            entity_spans: Vec::new(),
            content_hashes: HashMap::default(),
            precomputed: HashMap::default(),
            resolution: HashMap::default(),
            fingerprints: TableFingerprints::default(),
            stats: RebuildStats::default(),
            import_scans: HashMap::default(),
            import_table: HashMap::default(),
            import_keys: HashMap::default(),
            symbol_table: HashMap::default(),
            class_members: HashMap::default(),
            owner_members: HashMap::default(),
            entity_ranges: HashMap::default(),
            child_ranges: HashMap::default(),
            corpus_fp: crate::parser::incremental::TableFingerprints::default(),
            wildcard_guard: 0,
            entity_lookups_primed: false,
            generation: 0,
        };
        // Everything is dirty on a cold build, and nothing may be reused.
        let dirty: HashSet<String> = file_paths.iter().cloned().collect();
        session.run(file_paths, registry, &dirty, false);
        session
    }

    /// Warm rebuild after `changed_paths` were edited.
    ///
    /// `file_paths` is the corpus as it stands *now*: pass the same list to
    /// re-resolve after in-place edits, or a different one to add or remove
    /// files. `changed_paths` need not be a subset of it — paths that vanished
    /// are handled as deletions.
    pub fn rebuild(
        &mut self,
        file_paths: &[String],
        changed_paths: &[String],
        registry: &ParserRegistry,
    ) -> RebuildStats {
        let known: HashSet<&str> = file_paths.iter().map(String::as_str).collect();
        let previously_known: HashSet<&str> = self.file_paths.iter().map(String::as_str).collect();

        let mut dirty: HashSet<String> = changed_paths
            .iter()
            .filter(|p| known.contains(p.as_str()))
            .cloned()
            .collect();
        let mut added = 0usize;
        for path in file_paths {
            if !previously_known.contains(path.as_str()) {
                dirty.insert(path.clone());
                added += 1;
            }
        }
        let deleted = previously_known
            .iter()
            .filter(|p| !known.contains(*p))
            .count();

        // Chunked corpora resolve against chunk-scoped return-type and
        // instance-attribute maps, so an add or a delete shifts what every file
        // after it resolves against. Refuse reuse rather than reason about it.
        let chunked = file_paths.len() > PARSED_FILE_REUSE_LIMIT
            || self.file_paths.len() > PARSED_FILE_REUSE_LIMIT;
        let file_list_changed = added > 0 || deleted > 0;
        let nothing_to_reuse = self.fingerprints.is_empty();
        let reuse = !(nothing_to_reuse || (chunked && file_list_changed));

        let stats = self.run(file_paths, registry, &dirty, reuse);
        RebuildStats {
            files_seed_red: dirty.len(),
            files_deleted: deleted,
            ..stats
        }
    }

    fn run(
        &mut self,
        file_paths: &[String],
        registry: &ParserRegistry,
        dirty: &HashSet<String>,
        reuse: bool,
    ) -> RebuildStats {
        let t0 = std::time::Instant::now();
        let __session_prep_t0 = std::time::Instant::now();
        let known: HashSet<String> = file_paths.iter().cloned().collect();
        let eligible: HashSet<String> = file_paths
            .iter()
            .filter(|p| {
                is_reuse_eligible_file(
                    registry
                        .resolve_file_path(p)
                        .as_deref()
                        .unwrap_or(p.as_str()),
                )
            })
            .cloned()
            .collect();

        // Hand the previous build's entities back to the build core as movable
        // per-file groups. Cloning them instead would cost more than the
        // resolution the reuse saves — entity bodies carry their source text.
        let mut prev_entities: HashMap<String, Vec<SemanticEntity>> = HashMap::default();
        {
            let all = std::mem::take(&mut self.all_entities);
            let mut iter = all.into_iter();
            let spans = std::mem::take(&mut self.entity_spans);
            let mut consumed = 0usize;
            for (path, start, len) in spans {
                debug_assert_eq!(start, consumed, "entity spans must be contiguous");
                consumed += len;
                prev_entities.insert(path, iter.by_ref().take(len).collect());
            }
        }

        // *moved*, not borrowed. `Incremental` mutates this map in
        // place and hands it back below, so a GREEN file's cached edges,
        // consumed words and read set stay exactly where they are for the whole
        // build instead of being deep-cloned out of a `prev` map and back into a
        // `next` one. See `Incremental`'s own doc comment for the generation
        // discipline that keeps a moved-through entry from outliving its
        // validity.
        let prev_resolution = std::mem::take(&mut self.resolution);
        let prev_fingerprints = std::mem::take(&mut self.fingerprints);
        let mut precomputed = std::mem::take(&mut self.precomputed);
        let mut content_hashes = std::mem::take(&mut self.content_hashes);
        let mut import_scans = std::mem::take(&mut self.import_scans);
        let mut import_table = std::mem::take(&mut self.import_table);
        let mut import_keys = std::mem::take(&mut self.import_keys);
        // `entity_map` round-trips through `self.graph.entities` —
        // that is already its permanent home (`EntityGraph`'s public field),
        // so there is no separate session field to take it from/give it back
        // to; `build_incremental_core` moves the (possibly incrementally
        // maintained) map straight into the `EntityGraph` it returns, and
        // `self.graph = graph` below makes it authoritative again. The other
        // four tables have no equivalent public home, so they get session
        // fields of their own, taken and restored the same way as
        // `import_table`/`import_keys` above.
        let mut entity_map = std::mem::take(&mut self.graph.entities);
        let mut symbol_table = std::mem::take(&mut self.symbol_table);
        let mut class_members = std::mem::take(&mut self.class_members);
        let mut owner_members = std::mem::take(&mut self.owner_members);
        let mut entity_ranges = std::mem::take(&mut self.entity_ranges);
        let mut child_ranges = std::mem::take(&mut self.child_ranges);
        let mut corpus_fp = std::mem::take(&mut self.corpus_fp);
        let mut wildcard_guard = self.wildcard_guard;
        let mut entity_lookups_primed = self.entity_lookups_primed;

        self.generation += 1;
        let mut inc = Incremental::new(
            dirty,
            prev_resolution,
            &prev_fingerprints,
            reuse,
            self.generation,
        );
        let mut carry = BuildCarry {
            inc: &mut inc,
            dirty,
            known: &known,
            eligible: &eligible,
            prev_entities: &mut prev_entities,
            precomputed: &mut precomputed,
            content_hashes: &mut content_hashes,
            entity_spans: Vec::new(),
            import_scans: &mut import_scans,
            import_table: &mut import_table,
            import_keys: &mut import_keys,
            symbol_table: &mut symbol_table,
            entity_map: &mut entity_map,
            class_members: &mut class_members,
            owner_members: &mut owner_members,
            entity_ranges: &mut entity_ranges,
            child_ranges: &mut child_ranges,
            corpus_fp: &mut corpus_fp,
            wildcard_guard: &mut wildcard_guard,
            entity_lookups_primed: &mut entity_lookups_primed,
        };
        crate::parser::resolve_profile::set_session_prep_ns(__session_prep_t0.elapsed());
        let (graph, all_entities) =
            EntityGraph::build_incremental_core(&self.root, file_paths, registry, Some(&mut carry));
        let __session_post_t0 = std::time::Instant::now();
        let entity_spans = std::mem::take(&mut carry.entity_spans);

        // Reduces the moved-through cache to exactly the entries this build
        // produced — everything the stats loop, the reuse rule and
        // `export_persisted` below are entitled to see. Must run before any of
        // the three.
        inc.finish();

        let files_green = inc.green_scope_count();
        let mut edges_reused = 0usize;
        let mut edges_rederived = 0usize;
        for cached in inc.entries() {
            let scope_edges = cached.scope.edges.len();
            let bow_edges = cached.bow.edges.len();
            if cached.scope_reused {
                edges_reused += scope_edges;
            } else {
                edges_rederived += scope_edges;
            }
            if cached.bow_reused {
                edges_reused += bow_edges;
            } else {
                edges_rederived += bow_edges;
            }
        }
        let changed_keys = prev_fingerprints.changed_key_count(&inc.cur_fp);
        let green_bow = inc.green_bow_count();
        self.fingerprints = std::mem::take(&mut inc.cur_fp);
        self.resolution = inc.take_cache();

        self.root = self.root.clone();
        self.file_paths = file_paths.to_vec();
        // `graph.entities` already carries the (possibly incrementally
        // maintained) `entity_map` forward — see the taking side above.
        self.graph = graph;
        self.all_entities = all_entities;
        self.entity_spans = entity_spans;
        self.content_hashes = content_hashes;
        self.precomputed = precomputed;
        self.import_scans = import_scans;
        self.import_table = import_table;
        self.import_keys = import_keys;
        self.symbol_table = symbol_table;
        self.class_members = class_members;
        self.owner_members = owner_members;
        self.entity_ranges = entity_ranges;
        self.child_ranges = child_ranges;
        self.corpus_fp = corpus_fp;
        self.wildcard_guard = wildcard_guard;
        self.entity_lookups_primed = entity_lookups_primed;
        crate::parser::resolve_profile::set_session_post_ns(__session_post_t0.elapsed());
        // Explicit, and explicitly measured: the previous build's remaining
        // state — its fingerprint map and the RED files' old entities. Freeing
        // it is `O(corpus)` and used to happen silently at the end of this
        // function, past every timer in `resolve_profile` — see
        // `SESSION_DROP_NS`. The per-file resolution cache used to be freed here
        // too, and was the largest of the three; moved it through the
        // build instead of copying it, so there is nothing left of it to free.
        let __session_drop_t0 = std::time::Instant::now();
        drop(prev_fingerprints);
        drop(prev_entities);
        crate::parser::resolve_profile::set_session_drop_ns(__session_drop_t0.elapsed());

        let stats = RebuildStats {
            files_seed_red: dirty.len(),
            files_red: file_paths.len().saturating_sub(files_green),
            files_green,
            files_green_bow: green_bow,
            files_deleted: 0,
            edges_reused,
            edges_rederived,
            changed_keys,
            rebuild_ms: t0.elapsed().as_secs_f64() * 1000.0,
        };
        self.stats = stats.clone();
        stats
    }

    pub fn graph(&self) -> &EntityGraph {
        &self.graph
    }

    pub fn entities(&self) -> &[SemanticEntity] {
        &self.all_entities
    }

    pub fn file_paths(&self) -> &[String] {
        &self.file_paths
    }

    /// Statistics for the most recent build or rebuild.
    pub fn stats(&self) -> &RebuildStats {
        &self.stats
    }

    /// Files whose scope resolution was reused verbatim in the most recent
    /// rebuild. Exposed so callers (and the blast-radius tests) can check the
    /// RED set against a ground-truth reachability query.
    ///
    /// Derived from the cache rather than stored: every entry in `resolution`
    /// was written or re-validated by the most recent build (see
    /// `Incremental::finish`), so its own `scope_reused` flag is the answer.
    pub fn green_files(&self) -> HashSet<&str> {
        self.resolution
            .iter()
            .filter(|(_, cached)| cached.scope_reused)
            .map(|(path, _)| path.as_str())
            .collect()
    }

    /// The read set recorded for `file_path`'s scope resolution, if it has one.
    /// Exposed for tests that assert on blast radius.
    pub fn scope_read_set_len(&self, file_path: &str) -> Option<usize> {
        self.resolution
            .get(file_path)
            .map(|r| r.scope.read_set.len())
    }

    /// Export the per-file facts layer in its persistable form.
    ///
    /// Building it copies entity bodies, so call it when you mean to persist,
    /// not on a hot path.
    pub fn export_facts(&self) -> Vec<FileFacts> {
        let mut out = Vec::with_capacity(self.entity_spans.len());
        for (path, start, len) in &self.entity_spans {
            out.push(FileFacts {
                path: path.clone(),
                content_hash: self.content_hashes.get(path).copied().unwrap_or(0),
                entities: self.all_entities[*start..*start + *len].to_vec(),
            });
        }
        out
    }

    /// Export this session's *entire* facts layer in the cross-process
    /// persistable form `facts_store::FactsStore` writes to disk:
    /// [`export_facts`](Self::export_facts)'s entities, plus each file's
    /// precomputed scope facts and cached resolution (edges + read sets), plus
    /// the corpus-wide table fingerprints a future warm rebuild's read-set
    /// checks need. This is strictly more than `export_facts` — a fresh
    /// process can warm-start from it; a process that only had `export_facts`'
    /// entities could skip re-parsing but would still have to re-resolve
    /// everything, because it would have no cached edges or read sets to judge
    /// GREEN against.
    ///
    /// A **view borrowing this session**: building it is
    /// O(files) pointer work — no entity body, precomputed source text or
    /// cached edge list is copied, where it used to deep-clone all three into
    /// an owned `PersistedFacts` (a full second copy of the corpus, held at
    /// peak alongside the session that still owned the originals; one of the
    /// facts plane's measured RSS bands).
    /// `PersistedFacts` remains the deserialize type (`FactsStore::load` /
    /// [`Self::warm_start`]); this is the serialize shape both savers take.
    pub fn export_persisted(&self) -> PersistedFactsRef<'_> {
        let mut files = Vec::with_capacity(self.entity_spans.len());
        for (path, start, len) in &self.entity_spans {
            files.push(PersistedFileRef {
                facts: FileFactsRef {
                    path,
                    content_hash: self.content_hashes.get(path).copied().unwrap_or(0),
                    entities: &self.all_entities[*start..*start + *len],
                },
                precomputed: self.precomputed.get(path),
                resolution: self.resolution.get(path),
            });
        }
        PersistedFactsRef {
            fingerprints: &self.fingerprints,
            files,
        }
    }

    /// Cross-process warm start: build a session from a
    /// [`PersistedFacts`] snapshot loaded from disk by a *different* process
    /// than the one that saved it — the disk is the only channel between
    /// them, exactly like `facts_store::FactsStore`'s own oracle tests prove.
    ///
    /// Every file in `file_paths` has its *actual current* content read and
    /// hashed here; only a file whose hash matches what the snapshot recorded
    /// is eligible to reuse its cached entities/facts/resolution. Everything
    /// else — a changed file, a file the snapshot never saw, an unreadable
    /// file — is seeded RED, exactly as if this were `GraphSession::build`'s
    /// first sight of it. The result is then run through the same `run` a live
    /// session's `rebuild` uses, so the reuse decision downstream is made by
    /// the identical, already-oracle-tested `Incremental` machinery — this
    /// method's only job is reconstructing the "previous build" state that
    /// machinery expects, from disk instead of from `&mut self`.
    ///
    /// Does **not** restore the import table (`import_scans`/`import_table`):
    /// that is not part of what `FactsStore` persists (see
    /// "## Persisted facts" for the measurement that
    /// led to dropping it from the store). The first rebuild after a
    /// `warm_start` therefore always rebuilds the import table from scratch,
    /// even for a no-op corpus; every subsequent in-process `rebuild` after
    /// that is exactly as fast as today's in-process warm rebuild.
    pub fn warm_start(
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
        loaded: PersistedFacts,
    ) -> (Self, RebuildStats) {
        // Read + hash + look-up-in-`loaded` every file in parallel, then fold
        // the (small, per-file) results into the session's maps serially.
        // Reading 40k+ files one at a time here would reintroduce exactly the
        // serial-loop cost spent a whole fix phase eliminating from
        // pass 1 — this warm start must not reintroduce it on the way in.
        struct Reused {
            entities: Vec<SemanticEntity>,
            hash: u64,
            precomputed: Option<PrecomputedFileFacts>,
            resolution: Option<CachedFileResolution>,
        }
        enum CheckOutcome {
            Dirty,
            // Boxed: `Dirty` carries nothing, so an unboxed `Reused` would
            // make every element of `checked` below as large as the biggest
            // reused file's entities/facts, even for the (common, on a
            // freshly-added corpus) all-`Dirty` case.
            Reused(Box<Reused>),
        }

        let checked: Vec<CheckOutcome> = maybe_par_iter!(file_paths)
            .map(|path| {
                let full = root.join(path);
                let content = match std::fs::read_to_string(&full) {
                    Ok(c) => c,
                    // Unreadable now (deleted/permission race/binary since
                    // reclassified): can't verify a hash match, so treat like
                    // any other file the store never proved current. The
                    // ordinary pass-1 read surfaces (and handles) the same
                    // error again.
                    Err(_) => return CheckOutcome::Dirty,
                };
                let hash = content_hash(&content);
                match loaded.files.get(path.as_str()) {
                    Some(stored) if stored.facts.content_hash == hash => {
                        CheckOutcome::Reused(Box::new(Reused {
                            entities: stored.facts.entities.clone(),
                            hash,
                            precomputed: stored.precomputed.clone(),
                            resolution: stored.resolution.clone(),
                        }))
                    }
                    // Missing from the snapshot, or present with a stale
                    // hash: forced RED. Never served from a mismatched entry.
                    _ => CheckOutcome::Dirty,
                }
            })
            .collect();

        let mut all_entities: Vec<SemanticEntity> = Vec::new();
        let mut entity_spans: Vec<(String, usize, usize)> = Vec::new();
        let mut content_hashes: HashMap<String, u64> = HashMap::default();
        let mut precomputed: HashMap<String, PrecomputedFileFacts> = HashMap::default();
        let mut resolution: HashMap<String, CachedFileResolution> = HashMap::default();
        let mut dirty: HashSet<String> = HashSet::default();

        for (path, outcome) in file_paths.iter().zip(checked) {
            match outcome {
                CheckOutcome::Reused(reused) => {
                    let Reused {
                        entities,
                        hash,
                        precomputed: p,
                        resolution: r,
                    } = *reused;
                    let start = all_entities.len();
                    all_entities.extend(entities);
                    entity_spans.push((path.clone(), start, all_entities.len() - start));
                    content_hashes.insert(path.clone(), hash);
                    if let Some(p) = p {
                        precomputed.insert(path.clone(), p);
                    }
                    if let Some(r) = r {
                        resolution.insert(path.clone(), r);
                    }
                }
                CheckOutcome::Dirty => {
                    dirty.insert(path.clone());
                }
            }
        }

        let mut session = GraphSession {
            root: root.to_path_buf(),
            file_paths: file_paths.to_vec(),
            graph: EntityGraph::from_parts(HashMap::default(), Vec::new()),
            all_entities,
            entity_spans,
            content_hashes,
            precomputed,
            resolution,
            fingerprints: loaded.fingerprints,
            stats: RebuildStats::default(),
            import_scans: HashMap::default(),
            import_table: HashMap::default(),
            import_keys: HashMap::default(),
            // Not part of what `FactsStore` persists, same as `import_table`
            // above (see this function's doc comment) — the first rebuild
            // after a warm start always repopulates these from a whole
            // rebuild (`entity_lookups_primed: false`), exactly like the
            // import table's own first-rebuild-after-warm-start behavior.
            symbol_table: HashMap::default(),
            class_members: HashMap::default(),
            owner_members: HashMap::default(),
            entity_ranges: HashMap::default(),
            child_ranges: HashMap::default(),
            corpus_fp: crate::parser::incremental::TableFingerprints::default(),
            wildcard_guard: 0,
            entity_lookups_primed: false,
            generation: 0,
        };
        let stats = session.run(file_paths, registry, &dirty, true);
        let stats = RebuildStats {
            files_seed_red: dirty.len(),
            files_deleted: 0,
            ..stats
        };
        (session, stats)
    }

    /// Consume the session, returning the same pair [`EntityGraph::build`] does.
    pub fn into_parts(self) -> (EntityGraph, Vec<SemanticEntity>) {
        (self.graph, self.all_entities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::graph::RefType;
    use crate::parser::plugins::create_default_registry;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    /// The oracle's ground truth: entity ids, edge triples, and a hash of the
    /// sorted edge dump. Two builds agreeing on all three is what "bit-identical"
    /// means for this change.
    #[derive(Debug, PartialEq, Eq)]
    struct GraphFingerprint {
        entities: Vec<String>,
        edges: Vec<String>,
        edge_hash: u64,
    }

    fn fingerprint(graph: &EntityGraph, entities: &[SemanticEntity]) -> GraphFingerprint {
        let mut entity_ids: Vec<String> = entities.iter().map(|e| e.id.clone()).collect();
        entity_ids.sort();
        let mut edges: Vec<String> = graph
            .edges
            .iter()
            .map(|e| {
                let kind = match e.ref_type {
                    RefType::Calls => "calls",
                    RefType::TypeRef => "typeref",
                    RefType::Imports => "imports",
                };
                format!("{}\u{1f}{}\u{1f}{}", e.from_entity, e.to_entity, kind)
            })
            .collect();
        edges.sort();
        let mut hasher = DefaultHasher::new();
        for edge in &edges {
            edge.hash(&mut hasher);
        }
        GraphFingerprint {
            entities: entity_ids,
            edges,
            edge_hash: hasher.finish(),
        }
    }

    fn cold(root: &Path, file_paths: &[String]) -> GraphFingerprint {
        let registry = create_default_registry();
        let (graph, entities) = EntityGraph::build(root, file_paths, &registry);
        fingerprint(&graph, &entities)
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, body).expect("write");
    }

    /// A corpus built to stress read-set tracking specifically: a hub every file
    /// imports, a chain of re-exports, same-named classes in several files, a
    /// class whose method is reached through an inferred return type, and leaves
    /// that depend on nothing.
    ///
    /// Deliberately more than 8 files so the crate's `#[cfg(test)]`
    /// `PARSED_FILE_REUSE_LIMIT = 8` pushes it down the chunked resolution path —
    /// the same path a monorepo takes in release builds.
    fn write_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "src/hub.ts",
            "export class Hub {\n  ping(): string { return 'pong'; }\n  shared(): number { return 1; }\n}\nexport function makeHub(): Hub { return new Hub(); }\nexport const HUB_VERSION = 1;\n",
        );
        write(
            root,
            "src/mid.ts",
            "import { Hub, makeHub } from './hub';\nexport class Mid {\n  run(): string {\n    const h = makeHub();\n    return h.ping();\n  }\n}\nexport function makeMid(): Mid { return new Mid(); }\n",
        );
        // Two files declaring the same class name: the duplicate-name minefield.
        write(
            root,
            "src/dupe_a.ts",
            "export class Shape {\n  area(): number { return 1; }\n}\n",
        );
        write(
            root,
            "src/dupe_b.ts",
            "export class Shape {\n  area(): number { return 2; }\n}\n",
        );
        for i in 0..6 {
            write(
                root,
                &format!("src/leaf_{i}.ts"),
                &format!(
                    "import {{ Hub, makeHub }} from './hub';\nimport {{ makeMid }} from './mid';\nexport function leaf{i}(): string {{\n  const h = makeHub();\n  const m = makeMid();\n  return h.ping() + m.run();\n}}\nexport function solo{i}(): number {{ return {i}; }}\n"
                ),
            );
        }
        // Files that touch nothing shared: these must stay GREEN when the hub moves.
        for i in 0..4 {
            write(
                root,
                &format!("src/island_{i}.ts"),
                &format!(
                    "function helper{i}(): number {{ return {i}; }}\nexport function island{i}(): number {{ return helper{i}(); }}\n"
                ),
            );
        }
        let mut files: Vec<String> = walk(root);
        files.sort();
        files
    }

    fn walk(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        fn rec(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rec(&path, root, out);
                } else {
                    out.push(
                        path.strip_prefix(root)
                            .expect("strip")
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        rec(root, root, &mut out);
        out
    }

    /// The oracle: warm-rebuild to state B must equal a cold build of state B.
    fn assert_warm_matches_cold(
        label: &str,
        mutate: impl FnOnce(&Path) -> (Vec<String>, Vec<String>),
    ) -> RebuildStats {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files_a = write_fixture(root);
        let registry = create_default_registry();

        let mut session = GraphSession::build(root, &files_a, &registry);
        let warm_a = fingerprint(session.graph(), session.entities());
        assert_eq!(
            warm_a,
            cold(root, &files_a),
            "{label}: the session's own cold build must equal EntityGraph::build"
        );

        let (files_b, changed) = mutate(root);
        let stats = session.rebuild(&files_b, &changed, &registry);
        let warm_b = fingerprint(session.graph(), session.entities());
        let cold_b = cold(root, &files_b);

        assert_eq!(
            warm_b.entities,
            cold_b.entities,
            "{label}: entity set diverged (warm {} vs cold {})",
            warm_b.entities.len(),
            cold_b.entities.len()
        );
        assert_eq!(
            warm_b.edges,
            cold_b.edges,
            "{label}: edge set diverged (warm {} vs cold {})",
            warm_b.edges.len(),
            cold_b.edges.len()
        );
        assert_eq!(warm_b.edge_hash, cold_b.edge_hash, "{label}: edge hash");
        stats
    }

    #[test]
    fn oracle_no_change_at_all() {
        let stats = assert_warm_matches_cold("no-op", |root| (walk_sorted(root), Vec::new()));
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn oracle_append_a_function_to_a_leaf() {
        assert_warm_matches_cold("append-leaf", |root| {
            let path = "src/island_0.ts";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("export function appended(): number { return 42; }\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn oracle_change_a_signature_others_call() {
        assert_warm_matches_cold("change-signature", |root| {
            write(
                root,
                "src/hub.ts",
                "export class Hub {\n  ping(count: number): string { return 'pong' + count; }\n  shared(): number { return 1; }\n}\nexport function makeHub(): Hub { return new Hub(); }\nexport const HUB_VERSION = 2;\n",
            );
            (walk_sorted(root), vec!["src/hub.ts".to_string()])
        });
    }

    #[test]
    fn oracle_delete_an_entity_others_reference() {
        assert_warm_matches_cold("delete-entity", |root| {
            write(
                root,
                "src/hub.ts",
                "export class Hub {\n  shared(): number { return 1; }\n}\nexport function makeHub(): Hub { return new Hub(); }\n",
            );
            (walk_sorted(root), vec!["src/hub.ts".to_string()])
        });
    }

    #[test]
    fn oracle_add_a_new_file() {
        assert_warm_matches_cold("add-file", |root| {
            write(
                root,
                "src/newcomer.ts",
                "import { Hub, makeHub } from './hub';\nexport function newcomer(): string {\n  const h = makeHub();\n  return h.ping();\n}\n",
            );
            (walk_sorted(root), vec!["src/newcomer.ts".to_string()])
        });
    }

    #[test]
    fn oracle_delete_a_file() {
        assert_warm_matches_cold("delete-file", |root| {
            std::fs::remove_file(root.join("src/leaf_5.ts")).expect("rm");
            (walk_sorted(root), vec!["src/leaf_5.ts".to_string()])
        });
    }

    #[test]
    fn oracle_change_the_file_everything_imports() {
        assert_warm_matches_cold("hub-rewrite", |root| {
            write(
                root,
                "src/hub.ts",
                "export class Hub {\n  ping(): string { return 'PONG'; }\n  shared(): number { return 7; }\n  extra(): boolean { return true; }\n}\nexport function makeHub(): Hub { return new Hub(); }\nexport function makeOther(): Hub { return new Hub(); }\nexport const HUB_VERSION = 3;\n",
            );
            (walk_sorted(root), vec!["src/hub.ts".to_string()])
        });
    }

    #[test]
    fn oracle_change_a_duplicate_named_class() {
        assert_warm_matches_cold("duplicate-name", |root| {
            write(
                root,
                "src/dupe_a.ts",
                "export class Shape {\n  area(): number { return 1; }\n  perimeter(): number { return 4; }\n}\n",
            );
            (walk_sorted(root), vec!["src/dupe_a.ts".to_string()])
        });
    }

    #[test]
    fn oracle_repeated_rebuilds_stay_exact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);

        for round in 0..4 {
            let path = format!("src/leaf_{round}.ts");
            let mut body = std::fs::read_to_string(root.join(&path)).expect("read");
            body.push_str(&format!(
                "export function extra{round}(): number {{ return {round}; }}\n"
            ));
            write(root, &path, &body);
            session.rebuild(&files, std::slice::from_ref(&path), &registry);
            let warm = fingerprint(session.graph(), session.entities());
            assert_eq!(
                warm,
                cold(root, &files),
                "round {round}: warm diverged from cold"
            );
        }
    }

    /// Blast-radius honesty, checked against the only ground truth that
    /// actually matters.
    ///
    /// The tempting ground truth — "every file with an edge into the changed
    /// file must be RED" — is wrong, and wrong in the direction that would make
    /// this cache pointlessly conservative. Adding a method to `Hub` does not
    /// change how `leaf_0` resolves `makeHub`; `leaf_0` reads the import table
    /// and the symbol table, neither of which moved, so it is *correct* for it
    /// to stay GREEN even though it has an edge into `hub.ts`.
    ///
    /// The necessary condition is narrower and sharper: **every file whose edges
    /// differ between a cold build of state A and a cold build of state B must be
    /// RED.** A file that stays GREEN while its edges should have changed is
    /// exactly the stale-edge failure this change must never ship. That is what is
    /// asserted here, for several different mutations.
    #[test]
    fn every_file_whose_edges_change_is_red() {
        for (label, mutate) in mutations() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            let files = write_fixture(root);
            let registry = create_default_registry();

            let before = cold(root, &files);
            let mut session = GraphSession::build(root, &files, &registry);
            let changed = mutate(root);
            let after = cold(root, &files);
            let stats = session.rebuild(&files, &changed, &registry);

            let must_be_red = files_with_differing_edges(&before, &after);
            let green = session.green_files();
            for file in &must_be_red {
                assert!(
                    !green.contains(file.as_str()),
                    "{label}: {file}'s edges changed but it stayed GREEN — that is a stale edge. {stats:?}"
                );
            }
        }
    }

    type Mutation = (&'static str, fn(&Path) -> Vec<String>);

    fn mutations() -> Vec<Mutation> {
        vec![
            ("rename-an-export", |root| {
                write(
                    root,
                    "src/hub.ts",
                    "export class Hub {\n  ping(): string { return 'pong'; }\n}\nexport function spawnHub(): Hub { return new Hub(); }\n",
                );
                vec!["src/hub.ts".to_string()]
            }),
            ("add-a-method", |root| {
                write(
                    root,
                    "src/hub.ts",
                    "export class Hub {\n  ping(): string { return 'pong'; }\n  shared(): number { return 1; }\n  extra(): boolean { return true; }\n}\nexport function makeHub(): Hub { return new Hub(); }\nexport const HUB_VERSION = 1;\n",
                );
                vec!["src/hub.ts".to_string()]
            }),
            ("drop-a-method-others-call", |root| {
                write(
                    root,
                    "src/hub.ts",
                    "export class Hub {\n  shared(): number { return 1; }\n}\nexport function makeHub(): Hub { return new Hub(); }\n",
                );
                vec!["src/hub.ts".to_string()]
            }),
            ("retarget-the-middle-of-a-chain", |root| {
                write(
                    root,
                    "src/mid.ts",
                    "import { Hub } from './hub';\nexport class Mid {\n  run(): number { return 0; }\n}\nexport function makeMid(): Mid { return new Mid(); }\n",
                );
                vec!["src/mid.ts".to_string()]
            }),
            ("shadow-a-duplicate-name", |root| {
                write(
                    root,
                    "src/dupe_a.ts",
                    "export class Shape {\n  area(): number { return 1; }\n  ping(): string { return 'no'; }\n}\n",
                );
                vec!["src/dupe_a.ts".to_string()]
            }),
        ]
    }

    /// Files whose outgoing edge set differs between two builds. Edge ids start
    /// with the owning file path, which is the edge-ownership invariant made
    /// observable.
    fn files_with_differing_edges(a: &GraphFingerprint, b: &GraphFingerprint) -> Vec<String> {
        fn by_file(fp: &GraphFingerprint) -> HashMap<String, Vec<&String>> {
            let mut out: HashMap<String, Vec<&String>> = HashMap::default();
            for edge in &fp.edges {
                let file = edge.split("::").next().unwrap_or_default().to_string();
                out.entry(file).or_default().push(edge);
            }
            out
        }
        let (left, right) = (by_file(a), by_file(b));
        let mut files: Vec<String> = left.keys().chain(right.keys()).cloned().collect();
        files.sort();
        files.dedup();
        files
            .into_iter()
            .filter(|f| left.get(f) != right.get(f))
            .collect()
    }

    /// Precision, not just soundness: an edit must not turn the whole corpus RED.
    #[test]
    fn blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);

        let path = "src/island_0.ts";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("export function islandExtra(): number { return 9; }\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 3,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );
        assert!(
            leaf.files_green >= files.len() - 3,
            "a leaf touch should leave almost everything GREEN, got {leaf:?}"
        );

        // A structural rewrite of the file every other file imports.
        write(
            root,
            "src/hub.ts",
            "export class Hub {\n  ping(): string { return 'x'; }\n  extra(): boolean { return true; }\n}\nexport function spawnHub(): Hub { return new Hub(); }\n",
        );
        let hub = session.rebuild(&files, &["src/hub.ts".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..4 {
            let island = format!("src/island_{i}.ts");
            assert!(
                green.contains(island.as_str()),
                "{island} imports nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    fn walk_sorted(root: &Path) -> Vec<String> {
        let mut files = walk(root);
        files.sort();
        files
    }

    /// A fixture that extracts zero entities from a file proves nothing
    /// about that file's resolution -- so every per-language GREEN-
    /// eligibility fixture must show a positive entity count for
    /// every one of its files, not just a non-empty aggregate.
    fn assert_every_file_has_entities(entities: &[SemanticEntity], files: &[String]) {
        for f in files {
            let count = entities.iter().filter(|e| &e.file_path == f).count();
            assert!(
                count > 0,
                "{f} extracted zero entities -- a fixture that extracts nothing \
                 proves nothing about that file's resolution"
            );
        }
    }

    // -----------------------------------------------------------------
    // Incremental import-table maintenance
    // -----------------------------------------------------------------

    /// `write_fixture`'s corpus, layered with structures that specifically
    /// stress `build_import_table_incremental`'s two read sets:
    /// `Table::SymbolTable` (named imports/re-exports) and
    /// `Table::TsExportSurface` (default/namespace imports) —
    /// a default-export provider reached through a re-export stub, a
    /// namespace import, a three-file named re-export chain, and a consumer
    /// whose import is a miss until a later mutation adds the file it names.
    fn write_import_churn_fixture(root: &Path) -> Vec<String> {
        write_fixture(root);
        write(
            root,
            "src/provider.ts",
            "export default class Provider {\n  value(): number { return 1; }\n}\n",
        );
        write(
            root,
            "src/reexport_stub.ts",
            "export { default } from './provider';\n",
        );
        write(
            root,
            "src/default_consumer.ts",
            "import Provider from './reexport_stub';\nexport function useProvider(): number {\n  const p = new Provider();\n  return p.value();\n}\n",
        );
        write(
            root,
            "src/ns_consumer.ts",
            "import * as HubNS from './hub';\nexport function useNsHub(): string {\n  return HubNS.makeHub().ping();\n}\n",
        );
        write(
            root,
            "src/chain_a.ts",
            "export function chainValue(): number { return 1; }\n",
        );
        write(
            root,
            "src/chain_b.ts",
            "export { chainValue } from './chain_a';\n",
        );
        write(
            root,
            "src/chain_c.ts",
            "import { chainValue } from './chain_b';\nexport function useChain(): number { return chainValue(); }\n",
        );
        // Imports a name that does not exist anywhere yet — a miss the
        // import table must record as a dependency (see `ReadSet`'s doc
        // comment: "a miss is a dependency too"), so adding `late_helper.ts`
        // later is exactly the "add a file that exports a name others
        // import" oracle scenario.
        write(
            root,
            "src/late_consumer.ts",
            "import { lateHelper } from './late_helper';\nexport function useLate(): number {\n  return lateHelper();\n}\n",
        );
        walk_sorted(root)
    }

    /// Oracle + fingerprint-parity check for import-table churn scenarios:
    /// warm-rebuild to state B must equal a cold build of state B in every
    /// way `assert_warm_matches_cold` already checks (entities, edges, edge
    /// hash) *and* in the table fingerprints
    /// `build_import_table_incremental` maintains — including
    /// `Table::TsExportSurface`, new in this change. Fingerprint parity is the
    /// more direct claim: it is what keeps every future GREEN determination
    /// honest, independent of whether *this* scenario's edge set happens to
    /// expose a divergence.
    fn assert_import_churn_matches_cold(
        label: &str,
        mutate: impl FnOnce(&Path) -> (Vec<String>, Vec<String>),
    ) -> RebuildStats {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files_a = write_import_churn_fixture(root);
        let registry = create_default_registry();

        let mut session = GraphSession::build(root, &files_a, &registry);
        let warm_a = fingerprint(session.graph(), session.entities());
        assert_eq!(
            warm_a,
            cold(root, &files_a),
            "{label}: the session's own cold build must equal EntityGraph::build"
        );

        let (files_b, changed) = mutate(root);
        let stats = session.rebuild(&files_b, &changed, &registry);
        let warm_b = fingerprint(session.graph(), session.entities());
        let cold_b = cold(root, &files_b);

        assert_eq!(
            warm_b.entities,
            cold_b.entities,
            "{label}: entity set diverged (warm {} vs cold {})",
            warm_b.entities.len(),
            cold_b.entities.len()
        );
        assert_eq!(
            warm_b.edges,
            cold_b.edges,
            "{label}: edge set diverged (warm {} vs cold {})",
            warm_b.edges.len(),
            cold_b.edges.len()
        );
        assert_eq!(warm_b.edge_hash, cold_b.edge_hash, "{label}: edge hash");

        // Fingerprint parity, per this change's own gate: the incrementally
        // maintained session's table fingerprints must equal a *fresh cold
        // session's* on the same end state — not just the graph they imply.
        let cold_session = GraphSession::build(root, &files_b, &registry);
        assert_eq!(
            session.fingerprints, cold_session.fingerprints,
            "{label}: fingerprint(incremental) != fingerprint(whole-rebuild) — \
             the import table (or another table) diverged from a full rebuild \
             even though the graph happened to agree"
        );

        stats
    }

    #[test]
    fn oracle_change_an_import_list() {
        assert_import_churn_matches_cold("change-import-list", |root| {
            write(
                root,
                "src/default_consumer.ts",
                "import Provider from './reexport_stub';\nimport { Hub } from './hub';\nexport function useProvider(): number {\n  const p = new Provider();\n  const h: Hub | null = null;\n  return p.value() + (h === null ? 0 : 1);\n}\n",
            );
            (
                walk_sorted(root),
                vec!["src/default_consumer.ts".to_string()],
            )
        });
    }

    #[test]
    fn oracle_add_a_file_that_exports_a_name_others_import() {
        assert_import_churn_matches_cold("add-file-exporting-imported-name", |root| {
            write(
                root,
                "src/late_helper.ts",
                "export function lateHelper(): number { return 99; }\n",
            );
            (walk_sorted(root), vec!["src/late_helper.ts".to_string()])
        });
    }

    #[test]
    fn oracle_delete_a_re_export_stub() {
        assert_import_churn_matches_cold("delete-re-export-stub", |root| {
            std::fs::remove_file(root.join("src/reexport_stub.ts")).expect("rm");
            (walk_sorted(root), vec!["src/reexport_stub.ts".to_string()])
        });
    }

    #[test]
    fn oracle_retarget_an_import_chain() {
        assert_import_churn_matches_cold("retarget-import-chain", |root| {
            write(
                root,
                "src/chain_d.ts",
                "export function chainValue(): number { return 42; }\n",
            );
            write(
                root,
                "src/chain_b.ts",
                "export { chainValue } from './chain_d';\n",
            );
            (
                walk_sorted(root),
                vec!["src/chain_d.ts".to_string(), "src/chain_b.ts".to_string()],
            )
        });
    }

    #[test]
    fn oracle_import_churn_no_op_rebuild_has_fingerprint_parity() {
        let stats = assert_import_churn_matches_cold("import-churn-no-op", |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
    }

    #[test]
    fn oracle_default_export_target_changes_body_but_not_signature() {
        // The default-export chain's target keeps its name and shape but
        // changes its return value — this must still be picked up: an
        // importer's `Table::TsExportSurface` read only covers *identity*
        // (which entity id a default export names), and `Provider`'s id is
        // unchanged here, so this exercises the ordinary own-content path
        // for `provider.ts` while confirming the re-export stub and the
        // default-import consumer both still resolve correctly through it.
        assert_import_churn_matches_cold("default-export-body-change", |root| {
            write(
                root,
                "src/provider.ts",
                "export default class Provider {\n  value(): number { return 2; }\n}\n",
            );
            (walk_sorted(root), vec!["src/provider.ts".to_string()])
        });
    }

    // -----------------------------------------------------------------
    // Universal GREEN eligibility: per-language oracle fixtures
    // for the languages extended past JS/TS-only — Python (named imports,
    // the bare `import module` whole-table guard, and constructor-parameter
    // inference across files), Go (the package index and the
    // `resolve_go_method_parent_ids` cross-file receiver rewrite), and bash
    // (whitelisted with no dedicated cross-file machinery at all — its
    // `source`d calls resolve through the same generic `Table::SymbolTable`
    // fallback every language already shares). Each fixture reuses the exact
    // oracle shape `assert_warm_matches_cold`/`blast_radius_is_proportional_
    // to_the_edit` already established for JS/TS: warm-vs-cold bit-identical
    // (entities, edges, edge hash) plus a hub touch that actually REDs its
    // dependents while leaving unrelated islands GREEN.
    // -----------------------------------------------------------------

    /// Generic oracle used by every per-language fixture below: warm-rebuild
    /// to state B must equal a cold build of state B. Parametrized on the
    /// fixture builder so each language gets its own corpus without
    /// duplicating the assertion logic `assert_warm_matches_cold` already
    /// proved out for the JS/TS fixture.
    fn assert_warm_matches_cold_for(
        label: &str,
        build_fixture: impl FnOnce(&Path) -> Vec<String>,
        mutate: impl FnOnce(&Path) -> (Vec<String>, Vec<String>),
    ) -> RebuildStats {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files_a = build_fixture(root);
        let registry = create_default_registry();

        let mut session = GraphSession::build(root, &files_a, &registry);
        let warm_a = fingerprint(session.graph(), session.entities());
        assert_eq!(
            warm_a,
            cold(root, &files_a),
            "{label}: the session's own cold build must equal EntityGraph::build"
        );

        let (files_b, changed) = mutate(root);
        let stats = session.rebuild(&files_b, &changed, &registry);
        let warm_b = fingerprint(session.graph(), session.entities());
        let cold_b = cold(root, &files_b);

        assert_eq!(
            warm_b.entities,
            cold_b.entities,
            "{label}: entity set diverged (warm {} vs cold {})",
            warm_b.entities.len(),
            cold_b.entities.len()
        );
        assert_eq!(
            warm_b.edges,
            cold_b.edges,
            "{label}: edge set diverged (warm {} vs cold {})",
            warm_b.edges.len(),
            cold_b.edges.len()
        );
        assert_eq!(warm_b.edge_hash, cold_b.edge_hash, "{label}: edge hash");
        stats
    }

    // --- Python -------------------------------------------------------

    /// A hub every file imports (via `from pkg.hub import ...`), a same-package
    /// `Mid` that calls through it, a constructor-inference pair (`Widget`'s
    /// `__init__` stashes its `hub` argument as `self.hub`, and `factory.py`
    /// instantiates it as `Widget(Hub())` from a third file — exactly the
    /// cross-file constructor-parameter-type inference
    /// `infer_constructor_param_types`/`scan_constructor_calls` exists for), a
    /// bare `import pkg.hub` consumer (exercises `register_namespace_import`'s
    /// whole-table guard, `Table::GuardPyWildcardImport`), two files declaring
    /// the same class name, six leaves, and four islands.
    fn write_python_fixture(root: &Path) -> Vec<String> {
        write(root, "pkg/__init__.py", "");
        write(
            root,
            "pkg/hub.py",
            "class Hub:\n    def ping(self):\n        return 'pong'\n\n    def shared(self):\n        return 1\n\n\ndef make_hub():\n    return Hub()\n\n\nHUB_VERSION = 1\n",
        );
        write(
            root,
            "pkg/mid.py",
            "from pkg.hub import Hub, make_hub\n\n\nclass Mid:\n    def run(self):\n        h = make_hub()\n        return h.ping()\n\n\ndef make_mid():\n    return Mid()\n",
        );
        // Constructor-parameter-type inference across three files: `Widget`'s
        // shape (attr <- param) is declared here...
        write(
            root,
            "pkg/widget.py",
            "class Widget:\n    def __init__(self, hub):\n        self.hub = hub\n\n    def relay(self):\n        return self.hub.ping()\n",
        );
        // ...instantiated with a `Hub` here (the constructor-call site
        // `scan_constructor_calls` scans for)...
        write(
            root,
            "pkg/factory.py",
            "from pkg.hub import Hub\nfrom pkg.widget import Widget\n\n\ndef make_widget():\n    w = Widget(Hub())\n    return w\n",
        );
        // ...so that `Widget.relay`'s `self.hub.ping()` above (in a *fourth*
        // file's read set) only resolves correctly if `instance_attr_types`
        // learned `(Widget, hub) -> Hub` from `factory.py`'s call site.
        write(
            root,
            "pkg/dupe_a.py",
            "class Shape:\n    def area(self):\n        return 1\n",
        );
        write(
            root,
            "pkg/dupe_b.py",
            "class Shape:\n    def area(self):\n        return 2\n",
        );
        // Bare `import module` form: `register_namespace_import`'s
        // whole-table guard, not a bounded per-key read.
        write(
            root,
            "pkg/wildcard_consumer.py",
            "import pkg.hub\n\n\ndef use_wild():\n    h = pkg.hub.make_hub()\n    return h.ping()\n",
        );
        for i in 0..6 {
            write(
                root,
                &format!("pkg/leaf_{i}.py"),
                &format!(
                    "from pkg.hub import make_hub\nfrom pkg.mid import make_mid\n\n\ndef leaf{i}():\n    h = make_hub()\n    m = make_mid()\n    return h.ping() + m.run()\n\n\ndef solo{i}():\n    return {i}\n"
                ),
            );
        }
        for i in 0..4 {
            write(
                root,
                &format!("pkg/island_{i}.py"),
                &format!(
                    "def helper{i}():\n    return {i}\n\n\ndef island{i}():\n    return helper{i}()\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn python_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("python-no-op", write_python_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op Python rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn python_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("python-touch-leaf", write_python_fixture, |root| {
            let path = "pkg/island_0.py";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\n\ndef appended():\n    return 42\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn python_oracle_touch_the_ctor_inferred_hub() {
        // Rewrites `Hub` (imported by name and reached indirectly through
        // `Widget.relay`'s ctor-inferred `self.hub`) — must still agree with a
        // cold build, proving the ctor-inference read/write cycle survives a
        // warm rebuild, not just a cold one.
        assert_warm_matches_cold_for(
            "python-touch-ctor-inferred-hub",
            write_python_fixture,
            |root| {
                write(
                    root,
                    "pkg/hub.py",
                    "class Hub:\n    def ping(self):\n        return 'pong!'\n\n    def shared(self):\n        return 1\n\n    def extra(self):\n        return True\n\n\ndef make_hub():\n    return Hub()\n\n\nHUB_VERSION = 2\n",
                );
                (walk_sorted(root), vec!["pkg/hub.py".to_string()])
            },
        );
    }

    #[test]
    fn python_oracle_touch_the_wildcard_import_target() {
        // `wildcard_consumer.py`'s bare `import pkg.hub` depends on the whole
        // `Table::GuardPyWildcardImport` surface, not a bounded key — a hub
        // rewrite must still invalidate it correctly.
        assert_warm_matches_cold_for(
            "python-touch-wildcard-target",
            write_python_fixture,
            |root| {
                write(
                    root,
                    "pkg/hub.py",
                    "class Hub:\n    def ping(self):\n        return 'changed'\n\n    def shared(self):\n        return 1\n\n\ndef make_hub():\n    return Hub()\n\n\nHUB_VERSION = 3\n",
                );
                (walk_sorted(root), vec!["pkg/hub.py".to_string()])
            },
        );
    }

    #[test]
    fn python_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_python_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);

        let path = "pkg/island_0.py";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\n\ndef islandExtra():\n    return 9\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 3,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        // A structural rewrite of the hub every other file reaches, directly
        // or through ctor-inference — this must RED its true dependents.
        write(
            root,
            "pkg/hub.py",
            "class Hub:\n    def ping(self):\n        return 'x'\n\n    def extra(self):\n        return True\n\n\ndef spawn_hub():\n    return Hub()\n",
        );
        let hub = session.rebuild(&files, &["pkg/hub.py".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..4 {
            let island = format!("pkg/island_{i}.py");
            assert!(
                green.contains(island.as_str()),
                "{island} imports nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Go -------------------------------------------------------------

    /// `pkgA` spans two files (`hub.go`, `hub_extra.go`) so `Hub`'s two
    /// methods exercise `resolve_go_method_parent_ids`'s cross-file receiver
    /// rewrite — `Shared`'s `parent_id` is only known once both files'
    /// entities are merged. `mid.go` calls `MakeHub` from the same package
    /// (the `Table::SymbolTable` fallback, same-package). `main.go` and six
    /// leaves cross the package boundary via `import ".../pkgA"`, exercising
    /// `Table::GoPkgIndex`. Four islands touch nothing shared.
    fn write_go_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "pkgA/hub.go",
            "package pkgA\n\ntype Hub struct{}\n\nfunc (h *Hub) Ping() string {\n\treturn \"pong\"\n}\n\nfunc MakeHub() *Hub {\n\treturn &Hub{}\n}\n",
        );
        // `Shared` is declared in a *different* file than `Hub` itself — its
        // `parent_id` is only resolved once pass 1 sees both files.
        write(
            root,
            "pkgA/hub_extra.go",
            "package pkgA\n\nfunc (h *Hub) Shared() int {\n\treturn 1\n}\n",
        );
        write(
            root,
            "pkgA/mid.go",
            "package pkgA\n\nfunc Run() string {\n\th := MakeHub()\n\treturn h.Ping()\n}\n",
        );
        write(
            root,
            "main.go",
            "package main\n\nimport \"example.com/proj/pkgA\"\n\nfunc main() {\n\th := pkgA.MakeHub()\n\t_ = h.Ping()\n}\n",
        );
        for i in 0..6 {
            write(
                root,
                &format!("leaf_{i}.go"),
                &format!(
                    "package main\n\nimport \"example.com/proj/pkgA\"\n\nfunc Leaf{i}() string {{\n\th := pkgA.MakeHub()\n\treturn h.Ping()\n}}\n\nfunc Solo{i}() int {{\n\treturn {i}\n}}\n"
                ),
            );
        }
        for i in 0..4 {
            write(
                root,
                &format!("island_{i}.go"),
                &format!(
                    "package main\n\nfunc helper{i}() int {{\n\treturn {i}\n}}\n\nfunc Island{i}() int {{\n\treturn helper{i}()\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn go_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("go-no-op", write_go_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op Go rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn go_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("go-touch-leaf", write_go_fixture, |root| {
            let path = "island_0.go";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nfunc Appended() int { return 42 }\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn go_oracle_touch_the_pkg_index_hub() {
        // Rewrites `Hub`'s methods across both its files — `Table::GoPkgIndex`
        // and `resolve_go_method_parent_ids`'s cross-file receiver rewrite
        // both must survive a warm rebuild, not just a cold one.
        assert_warm_matches_cold_for("go-touch-pkg-index-hub", write_go_fixture, |root| {
            write(
                    root,
                    "pkgA/hub.go",
                    "package pkgA\n\ntype Hub struct{}\n\nfunc (h *Hub) Ping() string {\n\treturn \"pong!\"\n}\n\nfunc MakeHub() *Hub {\n\treturn &Hub{}\n}\n",
                );
            write(
                    root,
                    "pkgA/hub_extra.go",
                    "package pkgA\n\nfunc (h *Hub) Shared() int {\n\treturn 2\n}\n\nfunc (h *Hub) Extra() bool {\n\treturn true\n}\n",
                );
            (
                walk_sorted(root),
                vec!["pkgA/hub.go".to_string(), "pkgA/hub_extra.go".to_string()],
            )
        });
    }

    #[test]
    fn go_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_go_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);

        let path = "island_0.go";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nfunc IslandExtra() int { return 9 }\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 3,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        // A structural rewrite of the package every other file imports.
        write(
            root,
            "pkgA/hub.go",
            "package pkgA\n\ntype Hub struct{}\n\nfunc (h *Hub) Ping() string {\n\treturn \"x\"\n}\n\nfunc SpawnHub() *Hub {\n\treturn &Hub{}\n}\n",
        );
        let hub = session.rebuild(&files, &["pkgA/hub.go".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..4 {
            let island = format!("island_{i}.go");
            assert!(
                green.contains(island.as_str()),
                "{island} imports nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Kotlin (whitelisted, not attributed) ----------------------------

    /// No `import`-aware extraction exists for Kotlin at all (its
    /// `import_header` node kind is not one of the four
    /// `extract_imports_from_ast` recognizes) — every cross-file call here
    /// resolves through the same `Table::SymbolTable` bare-call fallback
    /// every language already shares, empirically confirmed (see
    /// `is_reuse_eligible_file`'s doc comment for the bash detour this
    /// avoided: bash's own call nodes are never even collected as refs, a
    /// pre-existing, unrelated extraction gap this change found and surfaced
    /// rather than routing around silently).
    fn write_kotlin_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "hub.kt",
            "fun ping(): String {\n    return \"pong\"\n}\n\nfun shared(): Int {\n    return 1\n}\n",
        );
        write(
            root,
            "mid.kt",
            "fun run(): String {\n    return ping()\n}\n",
        );
        for i in 0..6 {
            write(
                root,
                &format!("leaf_{i}.kt"),
                &format!(
                    "fun leaf{i}(): String {{\n    return ping()\n}}\n\nfun solo{i}(): Int {{\n    return {i}\n}}\n"
                ),
            );
        }
        for i in 0..4 {
            write(
                root,
                &format!("island_{i}.kt"),
                &format!(
                    "fun helper{i}(): Int {{\n    return {i}\n}}\n\nfun island{i}(): Int {{\n    return helper{i}()\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn kotlin_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("kotlin-no-op", write_kotlin_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op Kotlin rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn kotlin_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("kotlin-touch-leaf", write_kotlin_fixture, |root| {
            let path = "island_0.kt";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nfun appended(): Int {\n    return 42\n}\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn kotlin_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("kotlin-touch-hub", write_kotlin_fixture, |root| {
            write(
                    root,
                    "hub.kt",
                    "fun ping(): String {\n    return \"pong2\"\n}\n\nfun shared(): Int {\n    return 1\n}\n\nfun extra(): Boolean {\n    return true\n}\n",
                );
            (walk_sorted(root), vec!["hub.kt".to_string()])
        });
    }

    #[test]
    fn kotlin_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_kotlin_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);

        let path = "island_0.kt";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nfun islandExtra(): Int {\n    return 9\n}\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 3,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        // Rename the function every leaf/mid calls by bare name — Kotlin has
        // no per-symbol import table here, so the only table its cross-file
        // calls read is `Table::SymbolTable` keyed by the *called name
        // itself*; renaming it is what must propagate, not a body-only edit
        // (which — precision, not a gap — would correctly leave `ping`'s own
        // `SymbolTable` entry, and therefore every caller, untouched).
        write(
            root,
            "hub.kt",
            "fun pingRenamed(): String {\n    return \"x\"\n}\n\nfun extra(): Boolean {\n    return true\n}\n",
        );
        let hub = session.rebuild(&files, &["hub.kt".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..4 {
            let island = format!("island_{i}.kt");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Java (whitelisted, not attributed; import_declaration collides with
    // Go's extractor by name, verified to be a structural no-op) -----------

    /// A real `import` statement is included (`import java.util.List;`) to
    /// exercise the `import_declaration` name collision with Go's extractor
    /// empirically, not just by grep. Static methods (`Hub.ping()`) exercise
    /// `resolve_ref`'s "Static call" path (`Table::ClassMembers`, already
    /// generic and recorded) rather than requiring instance type inference.
    fn write_java_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "Hub.java",
            "import java.util.List;\n\npublic class Hub {\n    public static String ping() {\n        return \"pong\";\n    }\n\n    public static int shared() {\n        return 1;\n    }\n}\n",
        );
        write(
            root,
            "Mid.java",
            "public class Mid {\n    public static String run() {\n        return Hub.ping();\n    }\n}\n",
        );
        for i in 0..2 {
            write(
                root,
                &format!("Leaf{i}.java"),
                &format!(
                    "public class Leaf{i} {{\n    public static String leaf{i}() {{\n        return Hub.ping();\n    }}\n\n    public static int solo{i}() {{\n        return {i};\n    }}\n}}\n"
                ),
            );
        }
        for i in 0..2 {
            write(
                root,
                &format!("Island{i}.java"),
                &format!(
                    "public class Island{i} {{\n    private static int helper{i}() {{\n        return {i};\n    }}\n\n    public static int island{i}() {{\n        return helper{i}();\n    }}\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn java_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("java-no-op", write_java_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op Java rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn java_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("java-touch-leaf", write_java_fixture, |root| {
            let path = "Island0.java";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nclass Appended0 {\n    static int appended() { return 42; }\n}\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn java_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("java-touch-hub", write_java_fixture, |root| {
            write(
                root,
                "Hub.java",
                "import java.util.List;\n\npublic class Hub {\n    public static String ping() {\n        return \"pong2\";\n    }\n\n    public static int shared() {\n        return 1;\n    }\n\n    public static boolean extra() {\n        return true;\n    }\n}\n",
            );
            (walk_sorted(root), vec!["Hub.java".to_string()])
        });
    }

    #[test]
    fn java_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_java_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "Island0.java";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nclass IslandExtra0 {\n    static int extra() { return 9; }\n}\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 2,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        write(
            root,
            "Hub.java",
            "public class Hub {\n    public static String ping() {\n        return \"x\";\n    }\n\n    public static boolean extra() {\n        return true;\n    }\n}\n",
        );
        let hub = session.rebuild(&files, &["Hub.java".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..2 {
            let island = format!("Island{i}.java");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- C++ (whitelisted, not attributed; no node-kind collision) --------

    fn write_cpp_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "hub.cpp",
            "int ping() {\n    return 1;\n}\n\nint shared() {\n    return 2;\n}\n",
        );
        write(root, "mid.cpp", "int run() {\n    return ping();\n}\n");
        for i in 0..2 {
            write(
                root,
                &format!("leaf_{i}.cpp"),
                &format!(
                    "int leaf{i}() {{\n    return ping();\n}}\n\nint solo{i}() {{\n    return {i};\n}}\n"
                ),
            );
        }
        for i in 0..2 {
            write(
                root,
                &format!("island_{i}.cpp"),
                &format!(
                    "int helper{i}() {{\n    return {i};\n}}\n\nint island{i}() {{\n    return helper{i}();\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn cpp_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("cpp-no-op", write_cpp_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op C++ rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn cpp_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("cpp-touch-leaf", write_cpp_fixture, |root| {
            let path = "island_0.cpp";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nint appended() {\n    return 42;\n}\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn cpp_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("cpp-touch-hub", write_cpp_fixture, |root| {
            write(
                root,
                "hub.cpp",
                "int ping() {\n    return 9;\n}\n\nint shared() {\n    return 2;\n}\n\nint extra() {\n    return 3;\n}\n",
            );
            (walk_sorted(root), vec!["hub.cpp".to_string()])
        });
    }

    #[test]
    fn cpp_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_cpp_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "island_0.cpp";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nint islandExtra() {\n    return 9;\n}\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 2,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        // Rename the function every leaf/mid calls by bare name -- C++ has
        // no per-symbol import table, so the only table its cross-file
        // calls read is `Table::SymbolTable` keyed by the called name
        // itself; renaming it is what must propagate (a body-only edit
        // would correctly leave `ping`'s own `SymbolTable` entry, and every
        // caller, untouched -- precision, not a gap).
        write(
            root,
            "hub.cpp",
            "int pingRenamed() {\n    return 0;\n}\n\nint extra() {\n    return 3;\n}\n",
        );
        let hub = session.rebuild(&files, &["hub.cpp".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..2 {
            let island = format!("island_{i}.cpp");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- C# (whitelisted, not attributed; no node-kind collision) ---------

    fn write_csharp_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "Hub.cs",
            "public class Hub {\n    public static string Ping() {\n        return \"pong\";\n    }\n\n    public static int Shared() {\n        return 1;\n    }\n}\n",
        );
        write(
            root,
            "Mid.cs",
            "public class Mid {\n    public static string Run() {\n        return Hub.Ping();\n    }\n}\n",
        );
        for i in 0..2 {
            write(
                root,
                &format!("Leaf{i}.cs"),
                &format!(
                    "public class Leaf{i} {{\n    public static string Leaf{i}Fn() {{\n        return Hub.Ping();\n    }}\n\n    public static int Solo{i}() {{\n        return {i};\n    }}\n}}\n"
                ),
            );
        }
        for i in 0..2 {
            write(
                root,
                &format!("Island{i}.cs"),
                &format!(
                    "public class Island{i} {{\n    private static int Helper{i}() {{\n        return {i};\n    }}\n\n    public static int IslandFn{i}() {{\n        return Helper{i}();\n    }}\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn csharp_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("csharp-no-op", write_csharp_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op C# rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn csharp_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("csharp-touch-leaf", write_csharp_fixture, |root| {
            let path = "Island0.cs";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str(
                "\npublic class Appended0 {\n    public static int Appended() { return 42; }\n}\n",
            );
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn csharp_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("csharp-touch-hub", write_csharp_fixture, |root| {
            write(
                root,
                "Hub.cs",
                "public class Hub {\n    public static string Ping() {\n        return \"pong2\";\n    }\n\n    public static int Shared() {\n        return 1;\n    }\n\n    public static bool Extra() {\n        return true;\n    }\n}\n",
            );
            (walk_sorted(root), vec!["Hub.cs".to_string()])
        });
    }

    #[test]
    fn csharp_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_csharp_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "Island0.cs";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str(
            "\npublic class IslandExtra0 {\n    public static int Extra() { return 9; }\n}\n",
        );
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 2,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        write(
            root,
            "Hub.cs",
            "public class Hub {\n    public static string Ping() {\n        return \"x\";\n    }\n\n    public static bool Extra() {\n        return true;\n    }\n}\n",
        );
        let hub = session.rebuild(&files, &["Hub.cs".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..2 {
            let island = format!("Island{i}.cs");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Ruby (whitelisted, not attributed; no node-kind collision) -------

    /// `RUBY_SCOPE_CONFIG`'s `CallNodeStyle::DirectMethod` degrades a
    /// receiver-less `helper()` call to a plain `AstRefKind::Call` (no
    /// `"receiver"` field present), so free-method calls resolve through
    /// `Table::SymbolTable` exactly like Kotlin's plain functions.
    fn write_ruby_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "hub.rb",
            "def ping\n  \"pong\"\nend\n\ndef shared\n  1\nend\n",
        );
        write(root, "mid.rb", "def run\n  ping()\nend\n");
        for i in 0..2 {
            write(
                root,
                &format!("leaf_{i}.rb"),
                &format!("def leaf{i}\n  ping()\nend\n\ndef solo{i}\n  {i}\nend\n"),
            );
        }
        for i in 0..2 {
            write(
                root,
                &format!("island_{i}.rb"),
                &format!("def helper{i}\n  {i}\nend\n\ndef island{i}\n  helper{i}()\nend\n"),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn ruby_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("ruby-no-op", write_ruby_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op Ruby rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn ruby_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("ruby-touch-leaf", write_ruby_fixture, |root| {
            let path = "island_0.rb";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\ndef appended\n  42\nend\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn ruby_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("ruby-touch-hub", write_ruby_fixture, |root| {
            write(
                root,
                "hub.rb",
                "def ping\n  \"pong2\"\nend\n\ndef shared\n  1\nend\n\ndef extra\n  true\nend\n",
            );
            (walk_sorted(root), vec!["hub.rb".to_string()])
        });
    }

    #[test]
    fn ruby_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_ruby_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "island_0.rb";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\ndef island_extra\n  9\nend\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 2,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        write(
            root,
            "hub.rb",
            "def ping_renamed\n  \"x\"\nend\n\ndef extra\n  true\nend\n",
        );
        let hub = session.rebuild(&files, &["hub.rb".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..2 {
            let island = format!("island_{i}.rb");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- PHP (whitelisted; use_declaration collides with Rust's extractor,
    // proven to be a safe miss, not a silent no-op) -------------------------

    /// `mid.php` carries a real `use App\Utils\Helper;` statement so the
    /// `use_declaration` name collision with `extract_rust_use` (Rust's own
    /// extractor) is actually exercised, not just reasoned about: PHP's `\`
    /// namespace separator means `extract_rust_use`'s `"::"` split never
    /// fires, so it falls into the single-segment branch and looks up the
    /// entire backslash-joined path as one symbol name — always a miss
    /// (correctly recorded as a read, per `resolve_import_name`'s
    /// `Table::SymbolTable` read), never a false resolution.
    fn write_php_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "hub.php",
            "<?php\n\nfunction ping() {\n    return \"pong\";\n}\n\nfunction shared() {\n    return 1;\n}\n",
        );
        write(
            root,
            "mid.php",
            "<?php\n\nuse App\\Utils\\Helper;\n\nfunction run() {\n    return ping();\n}\n",
        );
        for i in 0..2 {
            write(
                root,
                &format!("leaf_{i}.php"),
                &format!(
                    "<?php\n\nfunction leaf{i}() {{\n    return ping();\n}}\n\nfunction solo{i}() {{\n    return {i};\n}}\n"
                ),
            );
        }
        for i in 0..2 {
            write(
                root,
                &format!("island_{i}.php"),
                &format!(
                    "<?php\n\nfunction helper{i}() {{\n    return {i};\n}}\n\nfunction island{i}() {{\n    return helper{i}();\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn php_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("php-no-op", write_php_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op PHP rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn php_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("php-touch-leaf", write_php_fixture, |root| {
            let path = "island_0.php";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nfunction appended() {\n    return 42;\n}\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn php_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("php-touch-hub", write_php_fixture, |root| {
            write(
                root,
                "hub.php",
                "<?php\n\nfunction ping() {\n    return \"pong2\";\n}\n\nfunction shared() {\n    return 1;\n}\n\nfunction extra() {\n    return true;\n}\n",
            );
            (walk_sorted(root), vec!["hub.php".to_string()])
        });
    }

    #[test]
    fn php_oracle_touch_the_use_target_stays_a_miss() {
        // `mid.php`'s `use App\Utils\Helper;` names a class that never
        // exists in this fixture -- confirming the miss stays a miss (no
        // false resolution appears) across a warm rebuild too, not just a
        // cold one.
        assert_warm_matches_cold_for("php-touch-use-target", write_php_fixture, |root| {
            write(
                root,
                "mid.php",
                "<?php\n\nuse App\\Utils\\Helper;\nuse App\\Utils\\OtherHelper;\n\nfunction run() {\n    return ping();\n}\n",
            );
            (walk_sorted(root), vec!["mid.php".to_string()])
        });
    }

    #[test]
    fn php_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_php_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "island_0.php";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nfunction islandExtra() {\n    return 9;\n}\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 2,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        write(
            root,
            "hub.php",
            "<?php\n\nfunction pingRenamed() {\n    return \"x\";\n}\n\nfunction extra() {\n    return true;\n}\n",
        );
        let hub = session.rebuild(&files, &["hub.php".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..2 {
            let island = format!("island_{i}.php");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Scala (whitelisted; same import_declaration-name collision as
    // Java, checked the same way and verified with a real import) ---------

    fn write_scala_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "Hub.scala",
            "object Hub {\n  def ping(): String = \"pong\"\n  def shared(): Int = 1\n}\n",
        );
        write(
            root,
            "Mid.scala",
            "import scala.collection.mutable.ListBuffer\n\nobject Mid {\n  def run(): String = Hub.ping()\n}\n",
        );
        for i in 0..2 {
            write(
                root,
                &format!("Leaf{i}.scala"),
                &format!(
                    "object Leaf{i} {{\n  def leaf{i}(): String = Hub.ping()\n  def solo{i}(): Int = {i}\n}}\n"
                ),
            );
        }
        for i in 0..2 {
            write(
                root,
                &format!("Island{i}.scala"),
                &format!(
                    "object Island{i} {{\n  def helper{i}(): Int = {i}\n  def island{i}(): Int = helper{i}()\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn scala_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("scala-no-op", write_scala_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op Scala rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn scala_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("scala-touch-leaf", write_scala_fixture, |root| {
            let path = "Island0.scala";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nobject Appended0 {\n  def appended(): Int = 42\n}\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn scala_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("scala-touch-hub", write_scala_fixture, |root| {
            write(
                root,
                "Hub.scala",
                "object Hub {\n  def ping(): String = \"pong2\"\n  def shared(): Int = 1\n  def extra(): Boolean = true\n}\n",
            );
            (walk_sorted(root), vec!["Hub.scala".to_string()])
        });
    }

    #[test]
    fn scala_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_scala_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "Island0.scala";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nobject IslandExtra0 {\n  def extra(): Int = 9\n}\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 2,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        // Rename the method every caller reaches through `Hub.ping()`'s
        // static-call path (`Table::ClassMembers` keyed by `"Hub"`) -- a
        // body-only edit would correctly leave callers untouched.
        write(
            root,
            "Hub.scala",
            "object Hub {\n  def pingRenamed(): String = \"x\"\n  def extra(): Boolean = true\n}\n",
        );
        let hub = session.rebuild(&files, &["Hub.scala".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..2 {
            let island = format!("Island{i}.scala");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Zig (whitelisted, not attributed; no node-kind collision) --------

    fn write_zig_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "hub.zig",
            "pub fn ping() i32 {\n    return 1;\n}\n\npub fn shared() i32 {\n    return 2;\n}\n",
        );
        write(
            root,
            "mid.zig",
            "pub fn run() i32 {\n    return ping();\n}\n",
        );
        for i in 0..2 {
            write(
                root,
                &format!("leaf_{i}.zig"),
                &format!(
                    "pub fn leaf{i}() i32 {{\n    return ping();\n}}\n\npub fn solo{i}() i32 {{\n    return {i};\n}}\n"
                ),
            );
        }
        for i in 0..2 {
            write(
                root,
                &format!("island_{i}.zig"),
                &format!(
                    "pub fn helper{i}() i32 {{\n    return {i};\n}}\n\npub fn island{i}() i32 {{\n    return helper{i}();\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn zig_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("zig-no-op", write_zig_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op Zig rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn zig_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("zig-touch-leaf", write_zig_fixture, |root| {
            let path = "island_0.zig";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\npub fn appended() i32 {\n    return 42;\n}\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn zig_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("zig-touch-hub", write_zig_fixture, |root| {
            write(
                root,
                "hub.zig",
                "pub fn ping() i32 {\n    return 9;\n}\n\npub fn shared() i32 {\n    return 2;\n}\n\npub fn extra() i32 {\n    return 3;\n}\n",
            );
            (walk_sorted(root), vec!["hub.zig".to_string()])
        });
    }

    #[test]
    fn zig_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_zig_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "island_0.zig";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\npub fn islandExtra() i32 {\n    return 9;\n}\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 2,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        // Rename the function every leaf/mid calls by bare name -- same
        // `Table::SymbolTable`-only mechanism as C++/bash/fish/PHP; a
        // body-only edit would correctly leave callers untouched.
        write(
            root,
            "hub.zig",
            "pub fn pingRenamed() i32 {\n    return 0;\n}\n\npub fn extra() i32 {\n    return 3;\n}\n",
        );
        let hub = session.rebuild(&files, &["hub.zig".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..2 {
            let island = format!("island_{i}.zig");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Dart (kept RED; the fixture proves *why*, not just that it's slow)

    /// Ordinary, idiomatic Dart -- block-bodied top-level functions calling
    /// each other by name across files -- produces **zero** `Calls` edges.
    /// `DART_SCOPE_CONFIG` (one of the "Tier 2 Minimal" configs) sets
    /// `call_nodes: &["function_expression_body"]`, which is an arrow-body
    /// node kind (`() => expr`), not a call-expression kind at all;
    /// `collect_all_file_refs`'s call-node branch never fires for a
    /// statement-body call like `return ping();`. This is a real
    /// entity/ref-extraction gap in Dart specifically (not a scope-
    /// resolution eligibility question), out of this change's scope to fix.
    /// Proven here rather than assumed: if this ever starts producing edges
    /// (e.g. `collect_all_file_refs` gains real call-expression support for
    /// Dart), this test will fail loudly and say so.
    #[test]
    fn dart_ordinary_calls_are_not_collected_as_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(
            root,
            "hub.dart",
            "String ping() {\n  return 'pong';\n}\n\nint shared() {\n  return 1;\n}\n",
        );
        write(root, "mid.dart", "String run() {\n  return ping();\n}\n");
        let files = walk_sorted(root);
        let registry = create_default_registry();
        let (graph, entities) = EntityGraph::build(root, &files, &registry);
        assert!(
            !entities.is_empty(),
            "Dart entity extraction itself works fine -- functions are extracted"
        );
        let call_edges: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.ref_type == RefType::Calls)
            .collect();
        assert!(
            call_edges.is_empty(),
            "expected zero Calls edges for ordinary block-bodied Dart calls \
             (DART_SCOPE_CONFIG's call_nodes only matches arrow bodies) -- \
             found {call_edges:?}. If this now fails, Dart's ref extraction \
             gained real call-expression support and the RED verdict here \
             needs revisiting."
        );
    }

    // --- Bash (whitelisted, not attributed;) --------------------

    /// Bash has no import mechanism at all -- no `extract_imports_from_ast`
    /// branch matches any bash node kind, and `source`d files aren't tracked
    /// as edges. Every cross-file call here resolves through the same
    /// generic `Table::SymbolTable` bare-call fallback Kotlin's fixture
    /// already proved out; this fixture exists to prove it for bash
    /// specifically, now that has fixed `extract_call_ref` to
    /// collect bash calls as refs in the first place.
    fn write_bash_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "hub.sh",
            "ping() {\n  echo pong\n}\n\nshared() {\n  echo 1\n}\n",
        );
        write(root, "mid.sh", "run() {\n  ping\n}\n");
        for i in 0..6 {
            write(
                root,
                &format!("leaf_{i}.sh"),
                &format!("leaf{i}() {{\n  ping\n}}\n\nsolo{i}() {{\n  echo {i}\n}}\n"),
            );
        }
        for i in 0..4 {
            write(
                root,
                &format!("island_{i}.sh"),
                &format!("helper{i}() {{\n  echo {i}\n}}\n\nisland{i}() {{\n  helper{i}\n}}\n"),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn bash_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("bash-no-op", write_bash_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op bash rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn bash_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("bash-touch-leaf", write_bash_fixture, |root| {
            let path = "island_0.sh";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nappended() {\n  echo 42\n}\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn bash_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("bash-touch-hub", write_bash_fixture, |root| {
            write(
                root,
                "hub.sh",
                "ping() {\n  echo pong2\n}\n\nshared() {\n  echo 1\n}\n\nextra() {\n  echo true\n}\n",
            );
            (walk_sorted(root), vec!["hub.sh".to_string()])
        });
    }

    #[test]
    fn bash_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_bash_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "island_0.sh";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nislandExtra() {\n  echo 9\n}\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 3,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        // Rename the function every leaf/mid calls by bare name -- bash has
        // no per-symbol import table, so the only table its cross-file calls
        // read is `Table::SymbolTable` keyed by the called name itself;
        // renaming it is what must propagate.
        write(
            root,
            "hub.sh",
            "pingRenamed() {\n  echo x\n}\n\nextra() {\n  echo true\n}\n",
        );
        let hub = session.rebuild(&files, &["hub.sh".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..4 {
            let island = format!("island_{i}.sh");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // --- Fish (whitelisted, not attributed;) ---------------------

    /// Fish, like bash, has no import mechanism `extract_imports_from_ast`
    /// recognizes -- cross-file calls resolve through the same generic
    /// `Table::SymbolTable` fallback bash's fixture above exercises.
    fn write_fish_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "hub.fish",
            "function ping\n    echo pong\nend\n\nfunction shared\n    echo 1\nend\n",
        );
        write(root, "mid.fish", "function run\n    ping\nend\n");
        for i in 0..6 {
            write(
                root,
                &format!("leaf_{i}.fish"),
                &format!(
                    "function leaf{i}\n    ping\nend\n\nfunction solo{i}\n    echo {i}\nend\n"
                ),
            );
        }
        for i in 0..4 {
            write(
                root,
                &format!("island_{i}.fish"),
                &format!(
                    "function helper{i}\n    echo {i}\nend\n\nfunction island{i}\n    helper{i}\nend\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn fish_oracle_no_op_rebuild_is_green() {
        let stats = assert_warm_matches_cold_for("fish-no-op", write_fish_fixture, |root| {
            (walk_sorted(root), Vec::new())
        });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op fish rebuild must reuse something: {stats:?}"
        );
    }

    #[test]
    fn fish_oracle_touch_a_leaf() {
        assert_warm_matches_cold_for("fish-touch-leaf", write_fish_fixture, |root| {
            let path = "island_0.fish";
            let mut body = std::fs::read_to_string(root.join(path)).expect("read");
            body.push_str("\nfunction appended\n    echo 42\nend\n");
            write(root, path, &body);
            (walk_sorted(root), vec![path.to_string()])
        });
    }

    #[test]
    fn fish_oracle_touch_the_hub() {
        assert_warm_matches_cold_for("fish-touch-hub", write_fish_fixture, |root| {
            write(
                root,
                "hub.fish",
                "function ping\n    echo pong2\nend\n\nfunction shared\n    echo 1\nend\n\nfunction extra\n    echo true\nend\n",
            );
            (walk_sorted(root), vec!["hub.fish".to_string()])
        });
    }

    #[test]
    fn fish_blast_radius_is_proportional_to_the_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let files = write_fish_fixture(root);
        let registry = create_default_registry();
        let mut session = GraphSession::build(root, &files, &registry);
        assert_every_file_has_entities(session.entities(), &files);

        let path = "island_0.fish";
        let mut body = std::fs::read_to_string(root.join(path)).expect("read");
        body.push_str("\nfunction islandExtra\n    echo 9\nend\n");
        write(root, path, &body);
        let leaf = session.rebuild(&files, &[path.to_string()], &registry);
        assert!(
            leaf.files_red <= 3,
            "a leaf touch should keep the RED set tiny, got {leaf:?}"
        );

        write(
            root,
            "hub.fish",
            "function pingRenamed\n    echo x\nend\n\nfunction extra\n    echo true\nend\n",
        );
        let hub = session.rebuild(&files, &["hub.fish".to_string()], &registry);
        assert!(
            hub.files_red > leaf.files_red,
            "a structural hub edit must invalidate more than a leaf edit: hub {hub:?} vs leaf {leaf:?}"
        );
        let green = session.green_files();
        for i in 0..4 {
            let island = format!("island_{i}.fish");
            assert!(
                green.contains(island.as_str()),
                "{island} calls nothing shared and must stay GREEN: {hub:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Bash/fish call-ref extraction bug: `extract_call_ref`'s
    // fast path only ever recognized `.kind()` of `"identifier"` /
    // `"simple_identifier"` / `"type_identifier"` for a callee node. Bash's
    // `command` node hands it a `command_name` wrapper (one level above the
    // real `word` leaf); fish's `command` node hands it a bare `word`
    // directly. Neither kind was recognized, so bash/fish calls were never
    // collected as `AstRefKind::Call` at all -- not a resolution-precision
    // gap, an *extraction* gap: the ref never existed to resolve, cold or
    // warm, in every graph ever built. See
    // "Universal GREEN eligibility" section, which found and documented
    // this without fixing it (out of that change's scope); is the
    // fix.
    // -----------------------------------------------------------------

    #[test]
    fn bash_calls_across_files_become_reference_edges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "f.sh", "#!/usr/bin/env bash\nfoo() {\n  helper\n}\n");
        write(
            root,
            "g.sh",
            "#!/usr/bin/env bash\nhelper() {\n  echo hi\n}\n",
        );
        let files = walk_sorted(root);
        let registry = create_default_registry();
        let (graph, entities) = EntityGraph::build(root, &files, &registry);
        assert!(
            !entities.is_empty(),
            "the fixture must extract entities before it can prove anything about edges"
        );
        let edges: Vec<(&String, &String, RefType)> = graph
            .edges
            .iter()
            .map(|e| (&e.from_entity, &e.to_entity, e.ref_type.clone()))
            .collect();
        let has_call_edge = graph.edges.iter().any(|e| {
            e.ref_type == RefType::Calls
                && e.from_entity.contains("foo")
                && e.to_entity.contains("helper")
        });
        assert!(
            has_call_edge,
            "foo() (f.sh) -> helper() (g.sh) must exist as a Calls edge; edges found: {edges:?}"
        );
    }

    #[test]
    fn fish_calls_across_files_become_reference_edges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "f.fish", "function foo\n    helper\nend\n");
        write(root, "g.fish", "function helper\n    echo hi\nend\n");
        let files = walk_sorted(root);
        let registry = create_default_registry();
        let (graph, entities) = EntityGraph::build(root, &files, &registry);
        assert!(
            !entities.is_empty(),
            "the fixture must extract entities before it can prove anything about edges"
        );
        let edges: Vec<(&String, &String, RefType)> = graph
            .edges
            .iter()
            .map(|e| (&e.from_entity, &e.to_entity, e.ref_type.clone()))
            .collect();
        let has_call_edge = graph.edges.iter().any(|e| {
            e.ref_type == RefType::Calls
                && e.from_entity.contains("foo")
                && e.to_entity.contains("helper")
        });
        assert!(
            has_call_edge,
            "foo (f.fish) -> helper (g.fish) must exist as a Calls edge; edges found: {edges:?}"
        );
    }

    /// The same bug class, found while auditing the other whitelist
    /// candidates for (not shell-family, but "any language whose
    /// call syntax the extractor special-cases", which mandate
    /// also covers): tree-sitter-php's plain-identifier node kind for a
    /// bare `ping()` call's callee is `"name"`, not `"identifier"` --
    /// `extract_call_ref`'s fast path didn't recognize that kind either, so
    /// PHP function calls were never collected as refs at all, cold or warm.
    #[test]
    fn php_calls_across_files_become_reference_edges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(
            root,
            "f.php",
            "<?php\n\nfunction foo() {\n    return helper();\n}\n",
        );
        write(
            root,
            "g.php",
            "<?php\n\nfunction helper() {\n    return 1;\n}\n",
        );
        let files = walk_sorted(root);
        let registry = create_default_registry();
        let (graph, entities) = EntityGraph::build(root, &files, &registry);
        assert!(
            !entities.is_empty(),
            "the fixture must extract entities before it can prove anything about edges"
        );
        let edges: Vec<(&String, &String, RefType)> = graph
            .edges
            .iter()
            .map(|e| (&e.from_entity, &e.to_entity, e.ref_type.clone()))
            .collect();
        let has_call_edge = graph.edges.iter().any(|e| {
            e.ref_type == RefType::Calls
                && e.from_entity.contains("foo")
                && e.to_entity.contains("helper")
        });
        assert!(
            has_call_edge,
            "foo() (f.php) -> helper() (g.php) must exist as a Calls edge; edges found: {edges:?}"
        );
    }

    // --- Swift whole-corpus GREEN-eligibility guard ---------

    /// A corpus with exactly one `.swift` file (never touched by any test
    /// below) plus a small TypeScript hub/leaf group -- the shape that
    /// flagged this change: vscode's fleet warm-reload measured 0/13,292 files
    /// GREEN on a zero-change reload, traced to `resolve_with_scopes_full_
    /// inner`'s old `swift_active = !swift_call_signatures.is_empty()` gate
    /// in `scope_resolve.rs`, which forced *every* eligible file RED simply
    /// because the corpus contained a `.swift` file (a colorize-test
    /// fixture, in vscode's case) -- never mind whether that file's
    /// signatures had actually changed since the previous build. See
    /// `scope_resolve.rs`'s `swift_signatures_changed` doc comment for the
    /// fix (compare the whole-table guard's fingerprint across builds,
    /// exactly like every other whole-table guard already does) and
    /// vscode case study for the measured numbers.
    fn write_swift_guard_fixture(root: &Path) -> Vec<String> {
        write(
            root,
            "Fixture.swift",
            "func load(id: Int) -> String { return \"id\" }\n\nfunc load(name: String) -> String { return \"name\" }\n",
        );
        write(
            root,
            "hub.ts",
            "export function ping(): string {\n  return 'pong';\n}\n",
        );
        for i in 0..4 {
            write(
                root,
                &format!("leaf_{i}.ts"),
                &format!(
                    "import {{ ping }} from './hub';\n\nexport function leaf{i}(): string {{\n  return ping();\n}}\n"
                ),
            );
        }
        walk_sorted(root)
    }

    #[test]
    fn swift_guard_no_op_rebuild_still_greens_unrelated_ts_files() {
        // The regression this change fixes: before it, a corpus's mere
        // *possession* of a `.swift` file (regardless of whether it ever
        // changed) permanently disabled reuse for every eligible file, so
        // `files_green` was always 0 here -- even on a no-op reload with
        // nothing to do with Swift at all.
        let stats =
            assert_warm_matches_cold_for("swift-guard-no-op", write_swift_guard_fixture, |root| {
                (walk_sorted(root), Vec::new())
            });
        assert_eq!(stats.files_seed_red, 0);
        assert!(
            stats.files_green > 0,
            "a no-op rebuild of a corpus with an untouched .swift file must still \
             reuse its unrelated TypeScript files' cached resolution: {stats:?}"
        );
    }

    #[test]
    fn swift_guard_touch_a_leaf_still_greens_the_rest() {
        // Same shape, but a leaf `.ts` file actually changes this time --
        // proving the guard's relaxation still scopes reuse to the blast
        // radius rather than merely tolerating the zero-change case above.
        assert_warm_matches_cold_for(
            "swift-guard-touch-leaf",
            write_swift_guard_fixture,
            |root| {
                let path = "leaf_0.ts";
                let mut body = std::fs::read_to_string(root.join(path)).expect("read");
                body.push_str("\nexport function appended(): number { return 42; }\n");
                write(root, path, &body);
                (walk_sorted(root), vec![path.to_string()])
            },
        );
    }

    #[test]
    fn swift_guard_changing_swift_signatures_still_reds_everything() {
        // The guard's actual job, unchanged by this change: when the Swift
        // call-signature table's *value* genuinely changes, every eligible
        // file must still lose its cache -- `resolve_ref`'s Swift-overload
        // branch is corpus-wide and not attributed to any one file's read
        // set, so a whole-table invalidation is the only sound mechanism.
        // `assert_warm_matches_cold_for` already proves warm == cold
        // bit-for-bit regardless of which files went RED to get there; this
        // additionally asserts the guard still actually fires -- nothing
        // stays falsely GREEN across a real Swift-signature edit, which is
        // the fail-toward-MISS half of this change's fix.
        let stats = assert_warm_matches_cold_for(
            "swift-guard-signature-change",
            write_swift_guard_fixture,
            |root| {
                let path = "Fixture.swift";
                write(
                    root,
                    path,
                    "func load(id: Int) -> String { return \"id\" }\n\nfunc load(label: String) -> String { return \"label\" }\n",
                );
                (walk_sorted(root), vec![path.to_string()])
            },
        );
        assert_eq!(
            stats.files_seed_red, 1,
            "only Fixture.swift was named as changed"
        );
        assert_eq!(
            stats.files_green, 0,
            "a changed Swift call-signature table must still force every \
             eligible file RED, not just the file named as changed: {stats:?}"
        );
    }
}
