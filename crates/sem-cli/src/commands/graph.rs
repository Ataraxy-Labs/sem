use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::facts_store::{FactsCorpus, FactsStore, PersistedFacts};
use sem_core::parser::graph::{EntityGraph, EntityRef, RefType};
use sem_core::parser::registry::ParserRegistry;
use sem_core::parser::session::GraphSession;
use serde::ser::{SerializeMap, Serializer};

use crate::build_cache::DiskCache;
use crate::timings::Timings;
use sem_mcp::cache::{self as shared_cache, CacheSourceScope};

pub struct GraphOptions {
    pub cwd: String,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

pub fn graph_command(opts: GraphOptions) {
    let mut timings = Timings::from_env("graph");
    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let ext_filter = normalize_exts(&opts.file_exts);
    let source_scope = cache_source_scope(root, &ext_filter, opts.no_default_excludes);

    // Index fast path (semx-zvq): the whole-corpus dump, straight out of the
    // image — no walk, no SQL, no graph hydration. This is the *only*
    // discovery-skipping tier now: the git-oracle pair that used to sit here
    // (`oracle_fresh_topology`/`oracle_fresh_counts`) is deleted, on §1.4's
    // measurement that the oracle was strictly dominated — 157 ms of
    // shell-outs against the 12 ms parallel stat this path's freshness proof
    // costs, for a *weaker* guarantee (git says nothing about files it does
    // not track; this proves `Complete` membership plus per-file content).
    if !opts.no_cache && try_index_graph(&opts, root, source_scope) {
        timings.mark("index_fast_path");
        timings.finish();
        return;
    }

    let registry = super::create_registry(&root.to_string_lossy());
    let file_paths =
        find_supported_files_inner(root, &registry, &ext_filter, opts.no_default_excludes);
    timings.mark("file_discovery");

    let prog = crate::progress::Progress::start_staged();
    let graph = get_or_build_graph_topology_with_timings(
        root,
        &file_paths,
        &registry,
        opts.no_cache,
        source_scope,
        &mut timings,
    );
    prog.done(&format!(
        "{} entities, {} files",
        fmt_count(graph.entities.len()),
        fmt_count(file_paths.len())
    ));

    if opts.json {
        write_graph_json(&graph).unwrap();
        timings.mark("cli_output_serialization");
    } else {
        timings.mark("cli_output_serialization");
        println!(
            "{} {} entities, {} edges",
            "⊕".green(),
            graph.entities.len().to_string().bold(),
            graph.edges.len().to_string().bold(),
        );
    }
    timings.finish();
}

/// Format a count with thousands separators (1234 -> "1,234"), uv-style.
pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphStats {
    entity_count: usize,
    edge_count: usize,
}

fn write_graph_json(graph: &EntityGraph) -> serde_json::Result<()> {
    let mut entities = graph.entities.values().collect::<Vec<_>>();
    entities.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges = graph.edges.iter().collect::<Vec<_>>();
    edges.sort_by(compare_entity_refs);

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut serializer = serde_json::Serializer::new(&mut stdout);
    let mut map = (&mut serializer).serialize_map(Some(3))?;
    map.serialize_entry("entities", &entities)?;
    map.serialize_entry("edges", &edges)?;
    map.serialize_entry(
        "stats",
        &GraphStats {
            entity_count: graph.entities.len(),
            edge_count: graph.edges.len(),
        },
    )?;
    map.end()?;
    use std::io::Write;
    stdout.write_all(b"\n").map_err(serde_json::Error::io)
}

/// `sem graph` from the index (semx-zvq) — §12.2's second open reroute, and
/// the last query-plane caller of `write_graph_json_topology` /
/// `oracle_fresh_topology` / `oracle_fresh_counts`.
///
/// **Output-shape parity.** §12.1 named the blocker precisely: the JSON
/// serializes each edge's `ref_type`, "which the `REFS` CSR tier does not
/// carry". It does now (semx-zvq's FORMAT step), so the shape is
/// reproducible in full. What has to match, exactly:
///
/// - `{"entities":[…],"edges":[…],"stats":{"entityCount":N,"edgeCount":M}}`
///   plus a trailing newline — one JSON object, three keys, that order.
/// - entities: **every** entity, `EntityInfo`'s own serde (camelCase,
///   `parentId` omitted when absent), sorted by `id`. SQLite's `ORDER BY id`
///   is BINARY-collated and Rust's `str` ordering is the same byte order, so
///   one sort reproduces both the SQL path's and `write_graph_json`'s.
/// - edges: **forward only** — one row per `(from, to, ref_type)`, never the
///   reverse direction as well (§12.2 flagged double-counting as the risk) —
///   sorted by `(from_entity, to_entity, ref_type)` with
///   `calls < imports < typeref`. Emitting each entity's forward CSR row, in
///   entity-id order, produces exactly that: the row is already in
///   `(to, ref_type)` order (see `impact::try_index_impact_transitive`'s
///   derivation), so the outer id-order loop supplies the primary key.
/// - `edgeCount` is `REFS`' own forward posting count, which equals
///   `graph.edges.len()` unless an edge names an entity the graph does not
///   have — impossible by construction (edges are minted by resolving
///   against the entity map) and confirmed on both battery corpora, where
///   the SQL `COUNT(*)` and this number agree to the row.
///
/// Streamed, not materialized: the monster's dump is 714,819 entities and
/// ~900k edges, and the SQL path it replaces streams too, so building a
/// `Vec<EntityRef>` first would trade the one cost this tier exists to
/// remove. Only the id-order permutation is held in memory.
///
/// Declines (never a wrong answer, only "cannot answer fast"): a non-default
/// source scope, a missing/absent-tier image, or a corpus that cannot be
/// proven fresh whole (`query::corpus_is_fresh` — a whole-corpus answer
/// needs a whole-corpus proof, the same standard `has_fresh_topology_cache`
/// held the SQL path to).
fn try_index_graph(opts: &GraphOptions, root: &Path, source_scope: CacheSourceScope) -> bool {
    if !matches!(source_scope, CacheSourceScope::Default) {
        return false;
    }
    let Some(idx) = super::query::open_index(root) else {
        return false;
    };
    if !idx.has_refs() {
        return false;
    }
    if !super::query::corpus_is_fresh(&idx, root, &opts.cwd) {
        return false;
    }

    if !opts.json {
        println!(
            "{} {} entities, {} edges",
            "⊕".green(),
            idx.entity_count().to_string().bold(),
            idx.edge_count().to_string().bold(),
        );
        return true;
    }

    // Not `.is_ok()`: once the first byte is on stdout this path has committed
    // to answering, and falling through to another tier would append a second
    // JSON document to a half-written one. Fail the way the SQL streamer it
    // sits in front of fails — a message and a non-zero exit.
    if let Err(err) = write_graph_json_index(&idx) {
        eprintln!(
            "{} failed to stream graph JSON from the index: {}",
            "error:".red().bold(),
            err
        );
        std::process::exit(1);
    }
    true
}

/// The streaming half of [`try_index_graph`], byte-for-byte against
/// `DiskCache::write_graph_json_topology` (which is where the literal
/// `{"entities":[`, the `,` separators and the trailing
/// `],"stats":{…}}\n` come from — copied deliberately rather than
/// re-derived, so a future edit to one is an obvious edit to the other).
fn write_graph_json_index(idx: &sem_core::index::QueryIndex) -> std::io::Result<()> {
    use std::io::Write;

    let mut order: Vec<(String, usize)> = (0..idx.entity_count())
        .map(|at| (idx.entity(at).id(), at))
        .collect();
    order.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());

    writer.write_all(b"{\"entities\":[")?;
    for (position, (_, at)) in order.iter().enumerate() {
        if position > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut writer, &idx.entity(*at).to_entity_info())
            .map_err(std::io::Error::other)?;
    }

    writer.write_all(b"],\"edges\":[")?;
    let mut first = true;
    for (id, at) in &order {
        for (target, ref_type) in idx.refs_of_typed(*at) {
            if first {
                first = false;
            } else {
                writer.write_all(b",")?;
            }
            let edge = EntityRef {
                from_entity: id.clone(),
                to_entity: target.id(),
                ref_type,
            };
            serde_json::to_writer(&mut writer, &edge).map_err(std::io::Error::other)?;
        }
    }

    writeln!(
        writer,
        "],\"stats\":{{\"entityCount\":{},\"edgeCount\":{}}}}}",
        idx.entity_count(),
        idx.edge_count()
    )?;
    writer.flush()
}

fn compare_entity_refs(a: &&EntityRef, b: &&EntityRef) -> std::cmp::Ordering {
    a.from_entity
        .cmp(&b.from_entity)
        .then_with(|| a.to_entity.cmp(&b.to_entity))
        .then_with(|| ref_type_sort_key(&a.ref_type).cmp(&ref_type_sort_key(&b.ref_type)))
}

fn ref_type_sort_key(ref_type: &RefType) -> u8 {
    match ref_type {
        RefType::Calls => 0,
        RefType::Imports => 1,
        RefType::TypeRef => 2,
    }
}

/// Normalize extension strings: ensure each starts with '.'
pub fn normalize_exts(exts: &[String]) -> Vec<String> {
    exts.iter()
        .map(|e| {
            if e.starts_with('.') {
                e.clone()
            } else {
                format!(".{}", e)
            }
        })
        .collect()
}

/// Find all supported files in the repo (public for use by other commands).
pub fn find_supported_files_public(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
) -> Vec<String> {
    find_supported_files_with_options(root, registry, ext_filter, false)
}

pub fn find_supported_files_with_options(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Vec<String> {
    super::files::find_supported_files_in_path(
        root,
        root,
        registry,
        ext_filter,
        no_default_excludes,
    )
}

pub fn cache_source_scope(
    root: &Path,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> CacheSourceScope {
    if ext_filter.is_empty() && !no_default_excludes && !root.join(".semignore").exists() {
        CacheSourceScope::Default
    } else {
        CacheSourceScope::Custom
    }
}

/// The single place `sem-cli`'s graph/diff/impact/entities/context path falls
/// back to a cold `EntityGraph::build` after `DiskCache` misses (both full
/// and partial). Wired through `sem-core`'s on-disk facts store (semx-9en) so
/// a cold miss in *this* process can still warm-start from facts a *previous*
/// `sem` invocation left on disk, instead of always paying the full
/// parse+resolve cost `EntityGraph::build` pays from nothing. Also wired
/// through the machine-global cross-repo corpus (semx-2o8): a repo with *no*
/// local snapshot of its own (a fresh checkout — the case above still falls
/// all the way to a cold build) gets one more chance, filling in facts for
/// any file whose content a *different* repo on this machine already built,
/// before finally falling back to a genuinely cold build for what's left.
///
/// `DiskCache` (this file's `disk.load_with_source_scope`/`load_partial_...`,
/// above every call site of this function) is unrelated and untouched: it
/// caches the *finished graph*, keyed by this crate's own freshness/source-
/// scope rules. This function only replaces what happens on a miss from that
/// cache — it does not compete with it or change when it hits.
fn build_graph_with_facts_store(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
) -> (EntityGraph, Vec<SemanticEntity>) {
    let Some(store) = facts_store_for(root, no_cache) else {
        return EntityGraph::build(root, file_paths, registry);
    };
    facts_rss_mark("entry");
    let __facts_load_t0 = std::time::Instant::now();
    let local = store.load(root);
    crate::build_cache::cache_profile_mark("facts_local_load", __facts_load_t0);
    // Captured *before* `local` is potentially moved into `warm_start`
    // below — a cheap path->hash index (see `PersistedFacts::
    // content_hash_index`'s doc for why this, and not a clone of the whole
    // snapshot, is what survives to the `populate_delta` call at the
    // bottom).
    let local_hashes = local.as_ref().map(PersistedFacts::content_hash_index);

    // The corpus is only consulted when `local` has gaps (or is entirely
    // absent) — `merge_with_local` itself only pays for paths `local` never
    // saw, but skipping the call outright when `local` already covers every
    // path avoids even the bucket-grouping overhead on the common
    // already-warm rebuild, keeping that path byte-for-byte what it was
    // before this bead (see `facts_store.rs`'s "Cross-repo corpus" doc for
    // the measurement this protects).
    let corpus = facts_corpus_for(root, no_cache);
    let has_gap = match &local {
        None => true,
        Some(l) => file_paths.iter().any(|p| !l.files_contains(p)),
    };
    let __facts_merge_t0 = std::time::Instant::now();
    let merged = if has_gap {
        corpus.as_ref().map(|c| {
            // The lookup stats were computed and dropped on the floor here.
            // W4 (semx-431) needs them: `probed`/`hits` is the *definition* of
            // "known content" for this corpus, and without it a cold-build
            // measurement cannot say whether the machine already knew the
            // blobs it was reading. Printed only under `SEM_PROFILE_CACHE=1`;
            // the value is otherwise discarded exactly as before.
            let (facts, stats) = c.merge_with_local(root, file_paths, registry, local.as_ref());
            if std::env::var("SEM_PROFILE_CACHE").as_deref() == Ok("1") {
                eprintln!(
                    "FACTS_CORPUS probed={} hits={} shards_read={} bytes_read={}",
                    stats.probed, stats.hits, stats.shards_read, stats.bytes_read
                );
            }
            facts
        })
    } else {
        None
    };
    crate::build_cache::cache_profile_mark("facts_corpus_merge", __facts_merge_t0);
    facts_rss_mark("after_corpus_merge");

    let session = match merged.or(local) {
        Some(facts) => GraphSession::warm_start(root, file_paths, registry, facts).0,
        None => GraphSession::build(root, file_paths, registry),
    };
    facts_rss_mark("after_warm_start");

    let __facts_export_t0 = std::time::Instant::now();
    let exported = session.export_persisted();
    crate::build_cache::cache_profile_mark("facts_export_persisted", __facts_export_t0);
    facts_rss_mark("after_export_persisted");
    // Best-effort: a save failure (permissions, disk full, read-only cache
    // dir) must never fail the build this function exists to produce. The
    // store is a pure speed optimization — the next process just stays cold.
    let __facts_save_t0 = std::time::Instant::now();
    let _ = store.save(root, &exported);
    crate::build_cache::cache_profile_mark("facts_store_save", __facts_save_t0);
    let __facts_corpus_t0 = std::time::Instant::now();
    if let Some(corpus) = &corpus {
        let _ = corpus.populate_delta(local_hashes.as_ref(), &exported, registry);
    }
    crate::build_cache::cache_profile_mark("facts_corpus_populate_delta", __facts_corpus_t0);
    facts_rss_mark("after_populate_delta");
    session.into_parts()
}

/// Sample process RSS at one facts-plane phase boundary, under
/// `SEM_PROFILE_CACHE=1` (the same switch the phase timers beside these calls
/// already use).
///
/// W5 (semx-gbb) priced the facts plane at **+9.23 GB of peak RSS on dotnet**
/// and **+5.78 GB on linux** by differencing whole-process peaks with the
/// plane on and off — a number no timer and no counter inside `sem-core`
/// could attribute, because the plane's boundaries are here, in the CLI's
/// orchestration, and `warm_start`'s read+hash and per-file entity clone
/// happen *inside* `full_graph_build` where the save-plane timers cannot see
/// them. These marks turn that differencing into a direct reading: `entry` is
/// the ground level before any facts structure exists, and each later mark
/// names the boundary that grew it.
///
/// Sampling only, by construction — no allocation is changed, nothing is
/// freed differently, and `current_rss_bytes` returns `None` rather than
/// guessing where it cannot read cheaply.
fn facts_rss_mark(boundary: &str) {
    if std::env::var("SEM_PROFILE_CACHE").as_deref() != Ok("1") {
        return;
    }
    match sem_core::parser::mem_profile::current_rss_bytes() {
        Some(bytes) => eprintln!(
            "FACTS_RSS boundary={boundary} rss_mb={:.1}",
            bytes as f64 / (1024.0 * 1024.0)
        ),
        None => eprintln!("FACTS_RSS boundary={boundary} rss_mb=<unavailable>"),
    }
}

/// The facts store this process should use, or `None` when disabled (by
/// `--no-cache`/`no_cache`, by `SEM_FACTS_CACHE=0`, or because the per-repo
/// cache directory can't be determined for this root) — never an ambient
/// default `sem-core` reaches for on its own; this is the one place that
/// decides where `FactsStore::open` points.
fn facts_store_for(root: &Path, no_cache: bool) -> Option<FactsStore> {
    if no_cache || !env_flag("SEM_FACTS_CACHE", true) {
        return None;
    }
    let dir = match non_empty_env("SEM_FACTS_DIR") {
        Some(dir) => PathBuf::from(dir),
        // Lives inside the same per-repo cache directory `DiskCache` already
        // uses (`shared_cache::cache_dir_for_repo`) — matching sem's existing
        // per-repo cache convention rather than inventing a second one — in
        // its own `facts` subdirectory so the two stores' files never collide.
        None => shared_cache::cache_dir_for_repo(root)?.join("facts"),
    };
    Some(FactsStore::open(dir))
}

/// The machine-global cross-repo corpus this process should use, or `None`
/// when disabled — by `--no-cache`/`no_cache` or `SEM_FACTS_CACHE=0` (the
/// corpus is the other half of the same feature the per-repo store is, so it
/// shares that off-switch), by its own `SEM_FACTS_CORPUS=0`, or because the
/// per-repo cache directory can't be determined for this root (the corpus's
/// default location is derived from it — see below). Never an ambient
/// default `sem-core` reaches for on its own.
pub(crate) fn facts_corpus_for(root: &Path, no_cache: bool) -> Option<FactsCorpus> {
    if no_cache || !env_flag("SEM_FACTS_CACHE", true) || !env_flag("SEM_FACTS_CORPUS", true) {
        return None;
    }
    let dir = match non_empty_env("SEM_FACTS_CORPUS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => default_facts_corpus_dir(root)?,
    };
    Some(FactsCorpus::open(dir))
}

/// Default corpus location: the per-repo cache convention's own root,
/// generalized up two levels — `<cache_root>/sem/repos/<repo_key>/facts` is
/// where the per-repo store for *this* root lives (`facts_store_for` above);
/// stripping `<repo_key>` and `repos` leaves `<cache_root>/sem`, the
/// machine-global base every repo's per-repo cache already shares, and
/// `facts-corpus` is this bead's own sibling of `repos` under it — e.g.
/// `~/Library/Caches/sem/facts-corpus` on macOS, `~/.cache/sem/facts-corpus`
/// on Linux. Reusing `shared_cache::cache_dir_for_repo`'s own resolution
/// (rather than duplicating its XDG/platform-cache-dir logic here) means
/// this directory always agrees with wherever that function's env-var/OS
/// rules actually point, including `SEM_CACHE_DIR` overrides and test
/// sandboxes, without this file needing to know those rules itself.
fn default_facts_corpus_dir(root: &Path) -> Option<PathBuf> {
    let repo_cache_dir = shared_cache::cache_dir_for_repo(root)?; // .../sem/repos/<key>
    let repos_dir = repo_cache_dir.parent()?; // .../sem/repos
    let cache_base = repos_dir.parent()?; // .../sem
    Some(cache_base.join("facts-corpus"))
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no" | ""
        ),
        Err(_) => default,
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn find_supported_files_inner(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Vec<String> {
    find_supported_files_with_options(root, registry, ext_filter, no_default_excludes)
}

/// Build the entity graph + entities, using the disk cache when possible.
/// Tries: full cache hit → incremental rebuild (stale files only) → full rebuild.
pub fn get_or_build_graph(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    source_scope: CacheSourceScope,
) -> (EntityGraph, Vec<SemanticEntity>) {
    let mut timings = Timings::disabled("graph");
    get_or_build_graph_with_timings(
        root,
        file_paths,
        registry,
        no_cache,
        source_scope,
        &mut timings,
    )
}

pub fn get_or_build_graph_with_timings(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    source_scope: CacheSourceScope,
    timings: &mut Timings,
) -> (EntityGraph, Vec<SemanticEntity>) {
    get_or_build_graph_with_cache_policy(
        root,
        file_paths,
        registry,
        no_cache,
        CacheMissSavePolicy::Full,
        source_scope,
        timings,
    )
}

pub fn get_or_build_graph_with_topology_save_on_miss_with_timings(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    source_scope: CacheSourceScope,
    timings: &mut Timings,
) -> (EntityGraph, Vec<SemanticEntity>) {
    get_or_build_graph_with_cache_policy(
        root,
        file_paths,
        registry,
        no_cache,
        CacheMissSavePolicy::Topology,
        source_scope,
        timings,
    )
}

pub enum GraphWithTestData {
    Full(EntityGraph, Vec<SemanticEntity>),
    Topology {
        graph: EntityGraph,
        test_entity_ids: HashSet<String>,
    },
}

pub fn get_or_build_graph_with_test_data_and_topology_save_on_miss_with_timings(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    source_scope: CacheSourceScope,
    timings: &mut Timings,
) -> GraphWithTestData {
    if !no_cache {
        if let Ok(disk) = DiskCache::open_existing(root) {
            timings.mark("cache_open");
            if let Some((graph, entities)) =
                disk.load_with_source_scope(root, file_paths, source_scope)
            {
                timings.mark("cache_full_load");
                return GraphWithTestData::Full(graph, entities);
            }
            if let Some((graph, test_entity_ids)) = disk
                .load_graph_topology_with_test_ids_and_source_scope(root, file_paths, source_scope)
            {
                timings.mark("cache_topology_load");
                return GraphWithTestData::Topology {
                    graph,
                    test_entity_ids,
                };
            }

            if let Some(partial) =
                disk.load_partial_with_source_scope(root, file_paths, source_scope)
            {
                timings.mark("cache_partial_load");
                let (graph, entities, metadata) =
                    EntityGraph::build_incremental_with_metadata_and_import_candidates(
                        root,
                        &partial.stale_files,
                        file_paths,
                        partial.cached_entities,
                        partial.cached_edges,
                        partial.stale_file_entities,
                        Some(&partial.cached_importing_stale_files),
                        registry,
                    );
                timings.mark("incremental_graph_rebuild");
                let _ = crate::build_cache::save_incremental_with_index(
                    &disk,
                    root,
                    file_paths,
                    &partial.stale_files,
                    &graph,
                    &entities,
                    metadata.repaired_clean_entity_ids,
                    &metadata.recomputed_edge_source_ids,
                    &metadata.deleted_entity_ids,
                    source_scope,
                );
                timings.mark("cache_incremental_save");
                return GraphWithTestData::Full(graph, entities);
            }
        }
    }

    let (graph, entities) = build_graph_with_facts_store(root, file_paths, registry, no_cache);
    timings.mark("full_graph_build");

    if !no_cache {
        if let Ok(disk) = DiskCache::open(root) {
            let _ = crate::build_cache::save_topology_with_index(
                &disk,
                root,
                file_paths,
                &graph,
                &entities,
                &registry.custom_test_dirs,
                source_scope,
            );
            timings.mark("cache_topology_save");
        }
    }

    GraphWithTestData::Full(graph, entities)
}

/// What a cold miss writes. The three variants are the three artifact sets a
/// finished build can leave behind, in descending cost.
#[derive(Clone, Copy)]
enum CacheMissSavePolicy {
    /// `cache.db`'s full tables (entity bodies + the compressed content
    /// store) *and* `index.sem`. The only tier that can serve a later
    /// `load_with_source_scope` hydrate or a `load_partial` incremental
    /// rebuild, and the only one that costs 3.3-24.3 s of a giant's cold
    /// build (RESOLUTION-PROFILE.md W4 §1).
    Full,
    /// `cache.db`'s topology tables (no bodies, no content store) *and*
    /// `index.sem`.
    Topology,
    /// `index.sem` only — no SQLite connection is opened at all.
    IndexOnly,
}

/// `SEM_BUILD_CACHE=1` restores the pre-W4.5 behaviour on the
/// [`CacheMissSavePolicy::IndexOnly`] path: the corpus-shaped build writes
/// `cache.db` again, so `sem graph` on a *dirty* tree can take the
/// incremental rebuild instead of a full one.
///
/// Default off, on the census in RESOLUTION-PROFILE.md W4.5 §2: on this path
/// the SQL mirror has no reader. `sem graph` is answered from `index.sem`
/// (`try_index_graph`); W4's cache.db-deleted experiment measured ten of ten
/// verbs byte-identical without it and nine of ten equally fast; semx-4ex
/// closed the tenth (`impact --deps`/`--dependents` name-only) by routing it
/// onto the index. What survives is the incremental rebuild, and it is
/// reachable only *after* some earlier invocation has already paid the full
/// write — so on the giants the mirror charges 2.1-9.3 s of every cold build
/// to save ~1.4 s on each subsequent dirty `sem graph`. The verbs that
/// genuinely hydrate entity bodies (`sem context`'s index decline, `sem
/// entities --text`, `impact --tests`' lexical fallback, `sem diff --cloud`'s
/// relations pass) still use [`CacheMissSavePolicy::Full`] and still create
/// the mirror on their own first miss — this flag is only about whether the
/// *corpus-shaped* build speculatively creates it for them.
///
/// One env read per process (`OnceLock`), on a path that is about to do
/// seconds of work either way.
fn build_cache_opt_in() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("SEM_BUILD_CACHE", false))
}

fn get_or_build_graph_with_cache_policy(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    save_policy: CacheMissSavePolicy,
    source_scope: CacheSourceScope,
    timings: &mut Timings,
) -> (EntityGraph, Vec<SemanticEntity>) {
    if !no_cache {
        if let Ok(disk) = DiskCache::open_existing(root) {
            timings.mark("cache_open");
            // Try full cache hit
            if let Some(cached) = disk.load_with_source_scope(root, file_paths, source_scope) {
                timings.mark("cache_full_load");
                return cached;
            }

            // Try incremental: load clean cached data, rebuild only stale files
            if let Some(partial) =
                disk.load_partial_with_source_scope(root, file_paths, source_scope)
            {
                timings.mark("cache_partial_load");
                let (graph, entities, metadata) =
                    EntityGraph::build_incremental_with_metadata_and_import_candidates(
                        root,
                        &partial.stale_files,
                        file_paths,
                        partial.cached_entities,
                        partial.cached_edges,
                        partial.stale_file_entities,
                        Some(&partial.cached_importing_stale_files),
                        registry,
                    );
                timings.mark("incremental_graph_rebuild");
                let _ = crate::build_cache::save_incremental_with_index(
                    &disk,
                    root,
                    file_paths,
                    &partial.stale_files,
                    &graph,
                    &entities,
                    metadata.repaired_clean_entity_ids,
                    &metadata.recomputed_edge_source_ids,
                    &metadata.deleted_entity_ids,
                    source_scope,
                );
                timings.mark("cache_incremental_save");
                return (graph, entities);
            }
        }
    }

    // Full rebuild
    let (graph, entities) = build_graph_with_facts_store(root, file_paths, registry, no_cache);
    timings.mark("full_graph_build");

    if !no_cache {
        match save_policy {
            CacheMissSavePolicy::Full => {
                if let Ok(disk) = DiskCache::open(root) {
                    let _ = crate::build_cache::save_full_with_index(
                        &disk,
                        root,
                        file_paths,
                        &graph,
                        &entities,
                        &registry.custom_test_dirs,
                        source_scope,
                    );
                    timings.mark("cache_full_save");
                }
            }
            CacheMissSavePolicy::Topology => {
                if let Ok(disk) = DiskCache::open(root) {
                    let _ = crate::build_cache::save_topology_with_index(
                        &disk,
                        root,
                        file_paths,
                        &graph,
                        &entities,
                        &registry.custom_test_dirs,
                        source_scope,
                    );
                    timings.mark("cache_topology_save");
                }
            }
            // No `DiskCache::open` on this arm at all — the SQLite file is
            // never even created (semx-4ex).
            CacheMissSavePolicy::IndexOnly => {
                crate::build_cache::write_index_only(
                    root,
                    file_paths,
                    &graph,
                    &entities,
                    &registry.custom_test_dirs,
                );
                timings.mark("index_only_save");
            }
        }
    }

    (graph, entities)
}

/// The corpus-shaped build: `sem graph`, and `sem impact --deps/--dependents`
/// once their index tier has declined. Topology is all these callers ask for,
/// and on a miss they now leave behind exactly what answers them next time —
/// `index.sem`, and nothing else (semx-4ex, RESOLUTION-PROFILE.md W4.5).
///
/// Before this, the miss fell through to [`get_or_build_graph_with_timings`],
/// whose policy is [`CacheMissSavePolicy::Full`]: a topology-shaped request
/// paid for the full `cache.db` mirror — entity bodies, the compressed
/// content store, every index on both — which is the single largest item in a
/// giant's cold build and which nothing on this path ever reads back.
/// `SEM_BUILD_CACHE=1` ([`build_cache_opt_in`]) restores it for anyone who
/// wants the incremental rebuild on this path; the content-hydrating verbs
/// keep their own `Full` policy and are unaffected either way.
pub fn get_or_build_graph_topology_with_timings(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    source_scope: CacheSourceScope,
    timings: &mut Timings,
) -> EntityGraph {
    if !no_cache {
        if let Ok(disk) = DiskCache::open_existing(root) {
            timings.mark("cache_open");
            if let Some(graph) =
                disk.load_graph_topology_with_source_scope(root, file_paths, source_scope)
            {
                timings.mark("cache_topology_load");
                return graph;
            }
        }
    }

    let policy = if build_cache_opt_in() {
        CacheMissSavePolicy::Full
    } else {
        CacheMissSavePolicy::IndexOnly
    };
    let (graph, _entities) = get_or_build_graph_with_cache_policy(
        root,
        file_paths,
        registry,
        no_cache,
        policy,
        source_scope,
        timings,
    );
    graph
}

pub fn get_or_build_graph_topology_with_topology_save_on_miss_with_timings(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    source_scope: CacheSourceScope,
    timings: &mut Timings,
) -> EntityGraph {
    if !no_cache {
        if let Ok(disk) = DiskCache::open_existing(root) {
            timings.mark("cache_open");
            if let Some(graph) =
                disk.load_graph_topology_with_source_scope(root, file_paths, source_scope)
            {
                timings.mark("cache_topology_load");
                return graph;
            }
        }
    }

    let (graph, _entities) = get_or_build_graph_with_topology_save_on_miss_with_timings(
        root,
        file_paths,
        registry,
        no_cache,
        source_scope,
        timings,
    );
    graph
}

pub fn get_or_build_direct_dependency_graph_with_timings<F>(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    no_cache: bool,
    source_scope: CacheSourceScope,
    timings: &mut Timings,
    should_resolve: F,
) -> EntityGraph
where
    F: FnMut(&sem_core::parser::graph::EntityInfo) -> bool,
{
    if !no_cache {
        if let Ok(disk) = DiskCache::open_existing(root) {
            timings.mark("cache_open");
            if let Some(graph) =
                disk.load_graph_topology_with_source_scope(root, file_paths, source_scope)
            {
                timings.mark("cache_topology_load");
                return graph;
            }
        }
    }

    let (graph, _entities) =
        EntityGraph::build_direct_dependencies(root, file_paths, registry, should_resolve);
    timings.mark("direct_dependency_graph_build");
    graph
}
