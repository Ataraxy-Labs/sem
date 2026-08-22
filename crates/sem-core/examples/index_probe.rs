//! Query-index probe: writes an index from a cold graph build, checks the
//! consistency oracle's Property 1 against that graph, and measures the
//! cold-process name lookup that has to come in under 10 ms.
//!
//! Not part of the public API and not wired into any product code path — the
//! sibling of `perf_probe` (build plane) and `incr_probe` (red-green), for the
//! query plane. See `QUERY-INDEX.md` §1 and §6.
//!
//! Usage:
//!   cargo run --release --example index_probe -- write  <repo_root> <index_path>
//!   cargo run --release --example index_probe -- lookup <index_path> <name>...
//!   cargo run --release --example index_probe -- refs   <index_path> callers|deps <name>...
//!   cargo run --release --example index_probe -- grep   <index_path> <repo_root> <pattern>
//!
//! Tagged, greppable stdout lines:
//!   BUILD      — cold EntityGraph::build wall time, entity/file counts
//!   READ       — parallel read of every file's bytes, feeding TRIGRAM (and
//!                the fingerprints) — the "how long did S3 add" number
//!   WRITE      — index build + atomic write wall time, image bytes, and the
//!                trigram build's own extract/merge sub-costs
//!   SECTION    — per-section bytes, and each as a share of the image
//!   ORACLE     — Property 1: index.lookup(name) ≡ graph's answer, ∀ name
//!   REFS_ORACLE — S2's Property 1 analogue: callers/refs ≡ graph's
//!                 dependents/dependencies, ∀ entity (QUERY-INDEX.md §6),
//!                 extended semx-zvq to also check every edge's `ref_type`:
//!                 refs_of_typed/callers_of_typed ≡ graph.edges' kinds, ∀
//!                 entity, ∀ edge
//!   FILES_ORACLE — §7 item 1's Property 1 analogue: `files_under(prefix)` ≡
//!                 a plain walk restricted to `prefix`, on a fresh index, for
//!                 the whole repo and every top-level directory
//!   TRIGRAM_ORACLE — S3's Property 1 analogue: for a pattern battery,
//!                 `grep::search` (postings-narrowed) ≡ `grep::full_scan`
//!                 (ground truth), same file:line set (QUERY-INDEX.md §9)
//!   MUTATION   — corrupt one real posting, confirm TRIGRAM_ORACLE catches it
//!   OPEN       — mmap + header validation wall time (cold process)
//!   LOOKUP     — per-name lookup wall time and hit count (cold process)
//!   REFS       — per-name callers/refs lookup wall time and hit count
//!   GREP       — one grep query's wall time, hit/candidate counts, origin
//!   COLD_TOTAL — process-entry to last answer, the number under budget

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use sem_core::index::grep::{self, CandidateOrigin, GrepOptions};
use sem_core::index::{self, QueryIndex};
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

/// `grep::search`'s `Complete`-sweep walker (semx-ykf): every index this
/// probe builds goes through `index::build_with_content_and_dirs_and_tests`
/// (no `DirFingerprint`s — `DIRS` is absent), so `complete_check` never
/// observes a drifted directory here and this closure is never actually
/// invoked; it exists only to satisfy `search`'s signature.
fn no_walk(_dir: &Path) -> Vec<String> {
    Vec::new()
}

fn main() {
    let entered = Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("write") if args.len() >= 3 => write_mode(Path::new(&args[1]), Path::new(&args[2])),
        Some("lookup") if args.len() >= 3 => lookup_mode(Path::new(&args[1]), &args[2..], entered),
        Some("refs") if args.len() >= 4 => {
            refs_mode(Path::new(&args[1]), &args[2], &args[3..], entered)
        }
        Some("grep") if args.len() >= 4 => grep_mode(
            Path::new(&args[1]),
            Path::new(&args[2]),
            &args[3..].join(" "),
            entered,
        ),
        _ => {
            eprintln!("usage: index_probe write  <repo_root> <index_path>");
            eprintln!("       index_probe lookup <index_path> <name>...");
            eprintln!("       index_probe refs   <index_path> callers|deps <name>...");
            eprintln!("       index_probe grep   <index_path> <repo_root> <pattern>");
            std::process::exit(2);
        }
    }
}

fn write_mode(root: &Path, index_path: &Path) {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let registry = make_registry(&root);

    let files = walk_files(&root, &registry);
    let started = Instant::now();
    let (graph, entities) = EntityGraph::build(&root, &files, &registry);
    println!(
        "BUILD ms={:.1} entities={} files={}",
        ms(started),
        graph.entities.len(),
        files.len()
    );

    // One parallel read per file feeds both the fingerprint's mtime (used
    // here; content_hash stays 0 — the probe doesn't re-hash the corpus, see
    // §8's honesty note) and TRIGRAM's byte content (S3, semx-az9) — same
    // read `sem-cli`'s `write_query_index` does in production, so this
    // number is the real added cost, not a probe-only shortcut.
    let started = Instant::now();
    #[cfg(feature = "parallel")]
    let rows: Vec<(String, Vec<u8>)> = files
        .par_iter()
        .filter_map(|p| read_one(&root, p))
        .collect();
    #[cfg(not(feature = "parallel"))]
    let rows: Vec<(String, Vec<u8>)> = files.iter().filter_map(|p| read_one(&root, p)).collect();
    let read_ms = ms(started);
    println!(
        "READ ms={:.1} files_read={} bytes_read={}",
        read_ms,
        rows.len(),
        rows.iter().map(|(_, b)| b.len()).sum::<usize>()
    );

    let fingerprints: Vec<index::FileFingerprint> = rows
        .iter()
        .map(|(path, _)| {
            let full = root.join(path);
            let meta = std::fs::metadata(&full).ok();
            let mtime = meta.as_ref().and_then(|m| m.modified().ok());
            let since = mtime.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
            index::FileFingerprint {
                path: path.clone(),
                mtime_secs: since.map_or(0, |d| d.as_secs() as i64),
                mtime_nanos: since.map_or(0, |d| d.subsec_nanos()),
                content_hash: 0,
            }
        })
        .collect();
    let contents: HashMap<String, Vec<u8>> = rows.into_iter().collect();

    // The test classification the writer packs into `EntityRec.flags`
    // (semx-zvq) — the same call `sem-cli`'s `save_topology`/
    // `save_with_test_dirs` make before handing the set to
    // `write_query_index`, so the probe exercises the production shape rather
    // than a probe-only `None`.
    let test_ids = graph.filter_test_entities_with_custom_dirs(&entities, &[]);
    let started = Instant::now();
    let (bytes, trigram_stats) = index::build_with_content_and_dirs_and_tests(
        &graph,
        &fingerprints,
        &[],
        &contents,
        Some(&test_ids),
    );
    let build_ms = ms(started);
    let started = Instant::now();
    index::write_atomic(index_path, &bytes).expect("write index");
    println!(
        "WRITE build_ms={:.1} write_ms={:.1} image_bytes={} image_mb={:.1}",
        build_ms,
        ms(started),
        bytes.len(),
        bytes.len() as f64 / 1e6
    );
    println!(
        "TRIGRAM_BUILD extract_ms={:.1} merge_ms={:.1} distinct_trigrams={} \
         stopped_trigrams={} posting_total={} section_bytes={} section_mb={:.1} \
         files_indexed={}",
        trigram_stats.extract_ms,
        trigram_stats.merge_ms,
        trigram_stats.distinct_trigrams,
        trigram_stats.stopped_trigrams,
        trigram_stats.posting_total,
        trigram_stats.section_bytes,
        trigram_stats.section_bytes as f64 / 1e6,
        trigram_stats.files_indexed,
    );
    report_sections(&bytes);

    let bytes_for_mutation = bytes.clone();
    let index = oracle(&graph, bytes);
    files_oracle(&files, &index);
    tests_oracle(&entities, &index);
    trigram_oracle(&index, &root, &contents);
    mutation_test(&index, &bytes_for_mutation, &root, &contents);
    refs_mutation_test(&graph, &bytes_for_mutation);
}

fn read_one(root: &Path, path: &str) -> Option<(String, Vec<u8>)> {
    std::fs::read(root.join(path))
        .ok()
        .map(|bytes| (path.to_string(), bytes))
}

fn report_sections(bytes: &[u8]) {
    use sem_core::index::format::*;
    let names = [
        (SEC_STRINGS, "STRINGS"),
        (SEC_FILES, "FILES"),
        (SEC_ENTITIES, "ENTITIES"),
        (SEC_NAMES, "NAMES"),
        (SEC_REFS, "REFS"),
        (SEC_TRIGRAM, "TRIGRAM"),
        (SEC_DIRS, "DIRS(reserved)"),
        (SEC_KINDS, "KINDS"),
    ];
    // Read the header back through the public path, salt-agnostically, by
    // asking for the salt the image itself claims.
    let claimed = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let Some(header) = Header::read(bytes, claimed) else {
        println!("SECTION unreadable");
        return;
    };
    for (index, label) in names {
        let len = header.sections[index].len;
        println!(
            "SECTION {label:<18} bytes={len:>12} share={:.1}%",
            100.0 * len as f64 / bytes.len().max(1) as f64
        );
    }
}

/// Property 1 of the consistency oracle (`QUERY-INDEX.md` §6): for every
/// distinct name in the graph, the index's answer equals the graph's answer,
/// as a set of ids and field-by-field on each row.
fn oracle(graph: &EntityGraph, bytes: Vec<u8>) -> QueryIndex {
    let started = Instant::now();
    let index = QueryIndex::from_bytes(bytes).expect("image reads back");

    let mut by_name: std::collections::HashMap<&str, Vec<&sem_core::parser::graph::EntityInfo>> =
        std::collections::HashMap::new();
    for entity in graph.entities.values() {
        by_name
            .entry(entity.name.as_str())
            .or_default()
            .push(entity);
    }

    let mut checked = 0usize;
    let mut mismatched = 0usize;
    for (name, expected) in &by_name {
        let hits = index.lookup(name);
        let mut want: Vec<&str> = expected.iter().map(|e| e.id.as_str()).collect();
        want.sort_unstable();
        let mut got: Vec<String> = hits.iter().map(|e| e.id()).collect();
        got.sort();
        if got != want {
            mismatched += 1;
            if mismatched <= 3 {
                println!("ORACLE_MISMATCH name={name:?} want={want:?} got={got:?}");
            }
            continue;
        }
        // Field-level equality on every row, not just the id set.
        let want_rows: std::collections::BTreeSet<(String, String, String, usize, usize)> =
            expected
                .iter()
                .map(|e| {
                    (
                        e.id.clone(),
                        e.entity_type.clone(),
                        e.file_path.clone(),
                        e.start_line,
                        e.end_line,
                    )
                })
                .collect();
        let got_rows: std::collections::BTreeSet<(String, String, String, usize, usize)> = hits
            .iter()
            .map(|e| {
                (
                    e.id(),
                    e.entity_type().to_string(),
                    e.file_path().to_string(),
                    e.start_line(),
                    e.end_line(),
                )
            })
            .collect();
        if want_rows != got_rows {
            mismatched += 1;
            if mismatched <= 3 {
                println!("ORACLE_FIELD_MISMATCH name={name:?}");
            }
        }
        checked += 1;
    }
    println!(
        "ORACLE names={} checked={} mismatched={} ms={:.1} verdict={}",
        by_name.len(),
        checked,
        mismatched,
        ms(started),
        if mismatched == 0 { "PASS" } else { "FAIL" }
    );
    if mismatched != 0 {
        std::process::exit(1);
    }

    if !refs_oracle(graph, &index) {
        std::process::exit(1);
    }
    index
}

/// `RefType`'s packed encoding, stable label for oracle comparison — mirrors
/// `writer::ref_kind_u8` (private to `sem-core`, so the oracle re-derives its
/// own copy from the public `RefType` rather than reaching into a private
/// function; if these two ever disagreed on a mapping, `REFS_ORACLE` would
/// simply misreport a mismatch's kind, never miss a real one, since
/// `refs_of_typed` decodes through the writer's own `format::refs::pack` —
/// this label only has to be *consistent*, not equal to the writer's u8s).
fn ref_type_label(rt: &sem_core::parser::graph::RefType) -> &'static str {
    use sem_core::parser::graph::RefType;
    match rt {
        RefType::Calls => "calls",
        RefType::TypeRef => "typeref",
        RefType::Imports => "imports",
    }
}

/// Outcome of one exhaustive pass of [`refs_check`] — kept separate from
/// printing/exit-code decisions so the same O(n) check can back both the
/// gating `REFS_ORACLE` line and [`refs_mutation_test`]'s corruption proof.
struct RefsCheckStats {
    checked: usize,
    mismatched: usize,
    kind_mismatched: usize,
    refs_typed: bool,
}

/// S2's Property 1 analogue (`QUERY-INDEX.md` §6, extended for semx-gis, and
/// again for semx-zvq's typed `REFS`): for every entity in the graph,
/// `index.refs_of`/`callers_of` equals the graph's own
/// `dependencies`/`dependents` maps, as a sorted id multiset — **and**
/// `index.refs_of_typed`/`callers_of_typed` equals `graph.edges`' own
/// `ref_type` for every one of those edges, as a sorted `(id, kind)`
/// multiset.
///
/// semx-o0x: entity→index-slot resolution used to go through
/// `index.lookup(&entity.name)` (a NAMES binary search) followed by a linear
/// `.find()` over that name's whole bucket, once per graph entity. That
/// reads as O(n) at a glance, but the real cost is `Σ over names x of
/// count(x)²` — every entity sharing a hub name (`test_setup`,
/// `async_setup`, `visit`, operator overloads, …) re-scans that name's
/// *entire* bucket. Measured on home-assistant/core (318,638 entities,
/// 95,580 distinct names, avg bucket ~3.3): 586.7M cumulative bucket-scan
/// iterations — a 1841x blowup over entity count, not the ~3x an O(n) walk
/// would produce. On the TypeScript monster (714,819 entities) the ratio
/// climbed to 2806x, tracking wall time (24.5s → 100.5s, a 4.1x jump for
/// only 2.24x more entities) far more tightly than entity count does. This
/// is the documented llvm pathology (semx-o0x): ref-dense, hub-heavy
/// corpora pay quadratic-shaped cost for a lookup path a *different*,
/// already-exhaustive check (`oracle()` above, over every *distinct* name)
/// had already fully proven correct — this function's own name-bucket walk
/// added zero incremental NAMES coverage, only cost.
///
/// The fix: resolve every entity's index slot through one id→slot map built
/// in a single O(n) pass over the index's own `ENTITIES` table, instead of
/// re-deriving a name bucket per entity. Coverage is unchanged and still
/// exhaustive — every entity, every edge, both directions, both id and
/// typed-kind agreement — only the resolution step's complexity class
/// drops, from `Σ count(x)²` to `O(n)`.
fn refs_check(graph: &EntityGraph, index: &QueryIndex) -> RefsCheckStats {
    let mut checked = 0usize;
    let mut mismatched = 0usize;
    let mut kind_mismatched = 0usize;

    // Ground truth for the typed check, grouped straight from `graph.edges`
    // — the same source `writer::build_refs_section` reads, so this is an
    // independent re-derivation, not a re-use of the writer's own grouping.
    let mut want_fwd_typed: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    let mut want_rev_typed: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    for e in &graph.edges {
        want_fwd_typed
            .entry(e.from_entity.as_str())
            .or_default()
            .push((e.to_entity.as_str(), ref_type_label(&e.ref_type)));
        want_rev_typed
            .entry(e.to_entity.as_str())
            .or_default()
            .push((e.from_entity.as_str(), ref_type_label(&e.ref_type)));
    }

    let refs_typed = index.refs_are_typed();

    // One pass over the index's own ENTITIES table (O(n)) replaces the old
    // per-entity NAMES-bucket rescan (Σ count(name)²). This is a plain
    // id→slot map, not a second NAMES table — it re-derives nothing NAMES
    // already answers, it just avoids asking NAMES the same question
    // `entity_count()` separate times.
    let mut id_to_index: HashMap<String, usize> = HashMap::with_capacity(index.entity_count());
    for slot in 0..index.entity_count() {
        id_to_index.insert(index.entity(slot).id(), slot);
    }

    for entity in graph.entities.values() {
        let Some(&at) = id_to_index.get(entity.id.as_str()) else {
            mismatched += 1;
            if mismatched <= 3 {
                println!("REFS_ORACLE_MISS id={:?}", entity.id);
            }
            continue;
        };

        let mut want_deps: Vec<&str> = graph
            .dependencies()
            .get(entity.id.as_str())
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        want_deps.sort_unstable();
        let mut got_deps: Vec<String> = index.refs_of(at).iter().map(|e| e.id()).collect();
        got_deps.sort();
        let deps_ok = got_deps == want_deps;

        let mut want_callers: Vec<&str> = graph
            .dependents()
            .get(entity.id.as_str())
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        want_callers.sort_unstable();
        let mut got_callers: Vec<String> = index.callers_of(at).iter().map(|e| e.id()).collect();
        got_callers.sort();
        let callers_ok = got_callers == want_callers;

        if !deps_ok || !callers_ok {
            mismatched += 1;
            if mismatched <= 3 {
                println!(
                    "REFS_ORACLE_MISMATCH id={:?} deps_ok={deps_ok} callers_ok={callers_ok}",
                    entity.id
                );
            }
        }

        // semx-zvq: kind check, only meaningful when the image is typed
        // (`FLAG_REFS_TYPED`) — an untyped fallback image (never produced by
        // any corpus measured against this design, §format::refs::
        // MAX_ENTITIES) is a documented degraded mode, not a mismatch.
        if refs_typed {
            let mut want_deps_typed = want_fwd_typed
                .get(entity.id.as_str())
                .cloned()
                .unwrap_or_default();
            want_deps_typed.sort_unstable();
            let mut got_deps_typed: Vec<(String, &'static str)> = index
                .refs_of_typed(at)
                .into_iter()
                .map(|(e, k)| (e.id(), ref_type_label(&k)))
                .collect();
            got_deps_typed.sort();
            let deps_typed_ok = got_deps_typed
                == want_deps_typed
                    .iter()
                    .map(|(id, k)| (id.to_string(), *k))
                    .collect::<Vec<_>>();

            let mut want_callers_typed = want_rev_typed
                .get(entity.id.as_str())
                .cloned()
                .unwrap_or_default();
            want_callers_typed.sort_unstable();
            let mut got_callers_typed: Vec<(String, &'static str)> = index
                .callers_of_typed(at)
                .into_iter()
                .map(|(e, k)| (e.id(), ref_type_label(&k)))
                .collect();
            got_callers_typed.sort();
            let callers_typed_ok = got_callers_typed
                == want_callers_typed
                    .iter()
                    .map(|(id, k)| (id.to_string(), *k))
                    .collect::<Vec<_>>();

            if !deps_typed_ok || !callers_typed_ok {
                kind_mismatched += 1;
                if kind_mismatched <= 3 {
                    println!(
                        "REFS_ORACLE_KIND_MISMATCH id={:?} deps_typed_ok={deps_typed_ok} callers_typed_ok={callers_typed_ok}",
                        entity.id
                    );
                }
            }
        }
        checked += 1;
    }

    RefsCheckStats {
        checked,
        mismatched,
        kind_mismatched,
        refs_typed,
    }
}

/// Runs [`refs_check`], prints the gating `REFS_ORACLE` line, and reports
/// pass/fail — exiting is the caller's call (so [`refs_mutation_test`] can
/// reuse the same check on a deliberately corrupted image without dying).
fn refs_oracle(graph: &EntityGraph, index: &QueryIndex) -> bool {
    let started = Instant::now();
    let stats = refs_check(graph, index);
    let ok = stats.mismatched == 0 && stats.kind_mismatched == 0;
    println!(
        "REFS_ORACLE entities={} checked={} mismatched={} kind_mismatched={} typed={} ms={:.1} verdict={}",
        graph.entities.len(),
        stats.checked,
        stats.mismatched,
        stats.kind_mismatched,
        stats.refs_typed,
        ms(started),
        if ok { "PASS" } else { "FAIL" }
    );
    ok
}

/// `QUERY-INDEX.md` §7 item 1's Property 1 analogue: `QueryIndex::files_under`
/// must equal a plain walk restricted to the same prefix, on a just-built
/// (fresh) index — the exact guarantee `entities.rs`'s directory-listing
/// reroute leans on. Checks the whole repo (`prefix=""`) and every top-level
/// directory found by the same walk `write_mode` fed into the graph build, so
/// this is self-consistent with `ORACLE`/`REFS_ORACLE` above rather than a
/// second, independently-scoped walk.
/// `TESTS_ORACLE` (semx-zvq): every entity's packed `FLAG_IS_TEST` bit must
/// agree with `parser::graph::is_test_entity` evaluated independently against
/// the `SemanticEntity` (body included) the build produced — the same
/// predicate the SQLite cache's `entity_flags.is_test` column stores, which
/// is what `sem impact --tests`/`--all`'s index reroute must reproduce
/// exactly to stay byte-identical with the SQL path it displaces.
fn tests_oracle(entities: &[sem_core::model::entity::SemanticEntity], index: &QueryIndex) {
    let started = Instant::now();
    if !index.has_test_flags() {
        println!("TESTS_ORACLE checked=0 mismatched=0 verdict=SKIP reason=flag_absent");
        return;
    }
    let mut by_id: std::collections::HashMap<&str, bool> =
        std::collections::HashMap::with_capacity(entities.len());
    for entity in entities {
        by_id.insert(
            entity.id.as_str(),
            sem_core::parser::graph::is_test_entity(entity, &[]),
        );
    }

    let mut mismatched = 0usize;
    let mut tests_seen = 0usize;
    for at in 0..index.entity_count() {
        let entity = index.entity(at);
        let got = entity.is_test();
        if got {
            tests_seen += 1;
        }
        let id = entity.id();
        // An id the build's `SemanticEntity` list doesn't carry cannot be
        // cross-checked; `EntityGraph::build` returns both from one pass, so
        // this should never fire — count it as a mismatch rather than skip it
        // silently if it ever does.
        match by_id.get(id.as_str()) {
            Some(want) if *want == got => {}
            _ => {
                mismatched += 1;
                if mismatched <= 3 {
                    println!("TESTS_ORACLE_MISMATCH id={id:?} got={got}");
                }
            }
        }
    }

    println!(
        "TESTS_ORACLE checked={} tests={} mismatched={} ms={:.1} verdict={}",
        index.entity_count(),
        tests_seen,
        mismatched,
        ms(started),
        if mismatched == 0 { "PASS" } else { "FAIL" }
    );
    if mismatched != 0 {
        std::process::exit(1);
    }
}

fn files_oracle(files: &[String], index: &QueryIndex) {
    let started = Instant::now();
    let mut mismatched = 0usize;

    let mut want_all: Vec<&str> = files.iter().map(String::as_str).collect();
    want_all.sort_unstable();
    let mut got_all = index.files_under("");
    got_all.sort_unstable();
    if got_all != want_all {
        mismatched += 1;
        println!(
            "FILES_ORACLE_MISMATCH prefix=\"\" want_count={} got_count={}",
            want_all.len(),
            got_all.len()
        );
    }

    let mut top_dirs: Vec<String> = files
        .iter()
        .filter_map(|f| f.split_once('/').map(|(dir, _)| format!("{dir}/")))
        .collect();
    top_dirs.sort_unstable();
    top_dirs.dedup();

    let mut checked = 1usize; // the whole-repo case above
    for prefix in &top_dirs {
        let mut want: Vec<&str> = files
            .iter()
            .filter(|f| f.starts_with(prefix.as_str()))
            .map(String::as_str)
            .collect();
        want.sort_unstable();
        let mut got = index.files_under(prefix);
        got.sort_unstable();
        checked += 1;
        if got != want {
            mismatched += 1;
            if mismatched <= 3 {
                println!(
                    "FILES_ORACLE_MISMATCH prefix={prefix:?} want_count={} got_count={}",
                    want.len(),
                    got.len()
                );
            }
        }
    }

    println!(
        "FILES_ORACLE prefixes_checked={} mismatched={} ms={:.1} verdict={}",
        checked,
        mismatched,
        ms(started),
        if mismatched == 0 { "PASS" } else { "FAIL" }
    );
    if mismatched != 0 {
        std::process::exit(1);
    }
}

/// The battery `QUERY-INDEX.md`'s trigram section asks for: common
/// identifier, rare identifier, two-word phrase, regex with a literal core,
/// and a pattern with no usable trigrams (forces the full-scan fallback).
/// Generic across languages deliberately — this runs on both the monster
/// (TypeScript) and `sem-core` itself (Rust), and the oracle property holds
/// whether a pattern gets 0 hits or thousands.
///
/// The last entry (`createProgram`) is TypeScript-specific, added
/// deliberately so at least one battery pattern is guaranteed to land
/// `Trigram`-origin *with real hits* on the monster: `return` and
/// `[A-Za-z]+Error` are both stop-listed there (§11.2 of QUERY-INDEX.md) and
/// `candidate files` has zero real occurrences in TypeScript's own source,
/// which — as `mutation_test` discovered empirically — makes a posting
/// corruption unobservable (a candidate set change with no true positives to
/// lose produces no hit-level divergence). `return` already covers this need
/// on `sem-core` (small corpus, nothing gets stop-listed), so together the
/// two corpora always have at least one usable (Trigram, hits>0) pattern.
const PATTERN_BATTERY: &[(&str, &str)] = &[
    ("common_identifier", "return"),
    ("rare_identifier", "TRIGRAM_BUDGET_BYTES"),
    ("two_word_phrase", "candidate files"),
    ("regex_literal_core", "[A-Za-z]+Error"),
    ("no_usable_trigram", "ab"),
    ("common_identifier_ts", "createProgram"),
];

/// S3's Property 1 analogue (`QUERY-INDEX.md` §6/§9, semx-az9's obligation):
/// for every pattern in `PATTERN_BATTERY`, `grep::search` (postings-narrowed,
/// candidate-verified) must equal `grep::full_scan` (ground truth — never
/// touches TRIGRAM) as the same `(file, line)` set. `search` is also
/// re-verified against `full_scan` over `contents` directly (not just
/// `full_scan`'s own disk re-read) so a bug shared between `search`'s and
/// `full_scan`'s common `verify_file` helper cannot hide a real divergence —
/// though the primary claim under test is candidate-set correctness, not
/// `verify_file` itself (QUERY-INDEX.md's trigram section documents this
/// distinction).
fn trigram_oracle(index: &QueryIndex, root: &Path, contents: &HashMap<String, Vec<u8>>) {
    let started = Instant::now();
    let all_files: Vec<String> = contents.keys().cloned().collect();
    let mut mismatched = 0usize;
    let mut rows = Vec::new();

    for (label, pattern) in PATTERN_BATTERY {
        let opts = GrepOptions::default();
        let served = grep::search(index, root, pattern, &opts, no_walk).expect("valid pattern");
        let truth = grep::full_scan(root, &all_files, pattern, &opts).expect("valid pattern");

        let mut got: Vec<(String, usize)> = served
            .hits
            .iter()
            .map(|h| (h.file.clone(), h.line))
            .collect();
        got.sort();
        let mut want: Vec<(String, usize)> =
            truth.iter().map(|h| (h.file.clone(), h.line)).collect();
        want.sort();

        let ok = got == want;
        if !ok {
            mismatched += 1;
            println!(
                "TRIGRAM_ORACLE_MISMATCH pattern={label:?} got_hits={} want_hits={}",
                got.len(),
                want.len()
            );
        }
        rows.push((
            label,
            pattern,
            served.origin,
            served.candidate_files,
            got.len(),
            ok,
        ));
    }

    for (label, pattern, origin, candidates, hits, ok) in &rows {
        println!(
            "TRIGRAM_ORACLE_ROW pattern={label:?} text={pattern:?} origin={:?} \
             candidate_files={candidates} total_files={} hits={hits} ok={ok}",
            origin,
            index.file_count(),
        );
    }
    println!(
        "TRIGRAM_ORACLE patterns={} mismatched={} ms={:.1} verdict={}",
        PATTERN_BATTERY.len(),
        mismatched,
        ms(started),
        if mismatched == 0 { "PASS" } else { "FAIL" }
    );
    if mismatched != 0 {
        std::process::exit(1);
    }
}

/// Mutation test (bead item 3): corrupt one real posting — flip a single
/// `TARGETS` entry for a trigram one of `PATTERN_BATTERY`'s patterns
/// *actually consults* to point at the wrong file — and confirm
/// `TRIGRAM_ORACLE`'s comparison (`search` vs `full_scan`) actually
/// diverges. This is not a defense (the index has no corruption recovery
/// beyond the section-soundness check at `open`); it is proof the oracle has
/// teeth — that a wrong posting produces a wrong *answer*, not just a wrong
/// byte nobody would notice.
///
/// Picking the trigram to corrupt is less trivial than "the pattern's first
/// three bytes": a pattern's candidate resolution ANDs several trigrams and
/// short-circuits the moment one of them is `Absent` (§`candidate_files`),
/// so corrupting a trigram the query never actually reaches (e.g. because a
/// *different*, alphabetically-earlier trigram in the same pattern was
/// already `Absent` and the whole alternative was declared impossible
/// first) produces no observable divergence — a false "the oracle missed
/// it" reading that is actually "this mutation was inert". This was caught
/// empirically: mutating `TRIGRAM_BUDGET_BYTES`'s `"TRI"` on the monster
/// left `search` unchanged, because a *different* required trigram of that
/// same pattern (`"BUD"`) is genuinely absent from the corpus and the query
/// never got far enough to consult `"TRI"`'s postings at all. The fix is to
/// only pick a pattern whose live `origin` is already `Trigram` (so its
/// postings intersection provably completed rather than short-circuiting)
/// and then corrupt one of *that* pattern's `Present` trigrams specifically.
fn mutation_test(
    index: &QueryIndex,
    bytes: &[u8],
    root: &Path,
    contents: &HashMap<String, Vec<u8>>,
) {
    let started = Instant::now();
    let opts = GrepOptions::default();
    let all_paths = index.all_file_paths();

    // Second empirical fix: even a `Trigram`-origin, `Present`-posting
    // corruption can be *unobservable* if the corrupted row entry never
    // belonged to a file that truly matches the pattern — verify_file()
    // re-checks live bytes regardless of candidate-set membership, so
    // wrongly adding or removing a file with zero real occurrences produces
    // no hit-level difference (found on the monster: corrupting a `did`
    // posting for `candidate files`, a pattern with 0 real hits in
    // TypeScript's own source, changed nothing observable). The fix:
    // require a pattern with `origin == Trigram` **and** at least one real
    // hit, then corrupt specifically the posting entry for one of that
    // hit's own files — guaranteeing the corruption drops a true positive
    // out of the candidate set, which `search` cannot silently recover from.
    let candidate = PATTERN_BATTERY.iter().find_map(|(_, pattern)| {
        let report = grep::search(index, root, pattern, &opts, no_walk).ok()?;
        if report.origin != CandidateOrigin::Trigram || report.hits.is_empty() {
            return None;
        }
        let hit_file = &report.hits[0].file;
        let file_index = all_paths.iter().position(|p| p == hit_file)? as u32;
        let trigrams = grep::required_trigrams(pattern);
        trigrams.into_iter().find_map(|t| {
            corrupt_posting_for_file(bytes, t, file_index)
                .map(|mutated| (*pattern, t, file_index, mutated))
        })
    });
    let Some((common_pattern, trigram, corrupted_file, mutated)) = candidate else {
        println!("MUTATION skipped=no_battery_pattern_had_a_provable_true_positive");
        return;
    };
    let Some(corrupt_index) = QueryIndex::from_bytes(mutated) else {
        // A corrupt posting that instead broke section soundness enough to
        // fail `open` is caught even earlier than the oracle — also a pass.
        println!("MUTATION verdict=PASS caught_at=open");
        return;
    };

    let all_files: Vec<String> = contents.keys().cloned().collect();
    let opts = GrepOptions::default();
    let served =
        grep::search(&corrupt_index, root, common_pattern, &opts, no_walk).expect("valid pattern");
    let truth = grep::full_scan(root, &all_files, common_pattern, &opts).expect("valid pattern");
    let mut got: Vec<(String, usize)> = served
        .hits
        .iter()
        .map(|h| (h.file.clone(), h.line))
        .collect();
    got.sort();
    let mut want: Vec<(String, usize)> = truth.iter().map(|h| (h.file.clone(), h.line)).collect();
    want.sort();

    // Sanity: the *uncorrupted* index must have agreed with truth already
    // (TRIGRAM_ORACLE just proved this), so any divergence here is
    // attributable to the mutation, not a pre-existing bug.
    let clean_served =
        grep::search(index, root, common_pattern, &opts, no_walk).expect("valid pattern");
    let mut clean_got: Vec<(String, usize)> = clean_served
        .hits
        .iter()
        .map(|h| (h.file.clone(), h.line))
        .collect();
    clean_got.sort();
    debug_assert_eq!(
        clean_got, want,
        "clean index must agree with truth before mutating"
    );

    let caught = got != want;
    println!(
        "MUTATION pattern={common_pattern:?} trigram={:?} corrupted_file={:?} ms={:.1} verdict={}",
        String::from_utf8_lossy(&trigram),
        all_paths.get(corrupted_file as usize).unwrap_or(&""),
        ms(started),
        if caught { "PASS" } else { "FAIL" }
    );
    if !caught {
        std::process::exit(1);
    }
}

/// Flip the specific `TARGETS` entry for `target_file` within `trigram`'s
/// posting row to a different file index, keeping the section's declared
/// length identical — the corruption a byte-level bit-flip on disk would
/// produce, not a structural truncation (`a_torn_trigram_section_is_a_clean_miss`
/// already covers that case). Targeting a specific file (rather than "the
/// row's first entry", tried first and found to produce unobservable
/// mutations — see `mutation_test`'s doc) is what guarantees the corruption
/// actually changes `search`'s answer: `target_file` is chosen by the caller
/// to be a file that *genuinely* matches the pattern, so dropping it from
/// this trigram's row necessarily drops it from the AND-intersection that
/// produces the candidate set. Returns `None` if `trigram`'s row doesn't
/// contain `target_file` at all, or has no room to pick a distinct
/// replacement — the caller treats that as "try a different trigram", not a
/// failure.
fn corrupt_posting_for_file(bytes: &[u8], trigram: [u8; 3], target_file: u32) -> Option<Vec<u8>> {
    use sem_core::index::format::{self, SEC_TRIGRAM};
    let header = format::Header::read(bytes, u64::from_le_bytes(bytes[16..24].try_into().ok()?))?;
    let section_ref = header.sections[SEC_TRIGRAM];
    if section_ref.is_absent() {
        return None;
    }
    let section =
        &bytes[section_ref.offset as usize..(section_ref.offset + section_ref.len) as usize];
    let (count, _total) = format::trigram::header(section)?;
    let value = format::trigram::pack(trigram);
    let mut slot = None;
    for s in 0..count {
        let key = format::trigram::key(section, s)?;
        if format::trigram::value_of(key) == value && !format::trigram::is_stop(key) {
            slot = Some(s);
            break;
        }
    }
    let slot = slot?;
    let (lo, hi) = format::trigram::row(section, count, slot)?;
    let at = (lo..hi).find(|&i| format::trigram::target(section, count, i) == Some(target_file))?;

    let file_count = header.file_count as u32;
    let replacement = (0..file_count).find(|&f| f != target_file)?;

    let mut mutated = bytes.to_vec();
    let target_off = section_ref.offset as usize + format::trigram::targets_at(count) + at * 4;
    mutated[target_off..target_off + 4].copy_from_slice(&replacement.to_le_bytes());
    Some(mutated)
}

/// The `REFS` section's sub-header length (`fwd_edge_count` +
/// `rev_edge_count`, each `u32`) — mirrors `reader::REFS_SUB_HEADER_LEN`,
/// private to `sem-core`. Duplicated here for the same reason
/// `corrupt_posting_for_file` duplicates `TRIGRAM`'s row layout: a probe
/// corrupting raw bytes has to know the wire format, and `format::refs`
/// deliberately stays dependency-free (`pack`/`target_of`/`kind_of` only —
/// no row/offset helpers), so this constant is the layout knowledge a real
/// reader never needs past `reader.rs` itself.
const REFS_SUB_HEADER_LEN: usize = 8;

/// Flips one forward-CSR posting's target entity, keeping its packed
/// `ref_type` bits intact (`format::refs::pack`) — semx-o0x's negative for
/// `REFS_ORACLE`: proof the now-O(n) [`refs_check`] still catches a
/// corrupted posting exhaustively, the same "wrong answer, not a wrong byte
/// nobody would notice" shape `corrupt_posting_for_file` proves for
/// `TRIGRAM`. Picks the first entity with a non-empty forward row and
/// rewrites its first posting to point at a different (but still in-range)
/// entity.
fn corrupt_refs_posting(bytes: &[u8]) -> Option<Vec<u8>> {
    use sem_core::index::format::{self, SEC_REFS};
    let header = format::Header::read(bytes, u64::from_le_bytes(bytes[16..24].try_into().ok()?))?;
    let section_ref = header.sections[SEC_REFS];
    if section_ref.is_absent() {
        return None;
    }
    let section =
        &bytes[section_ref.offset as usize..(section_ref.offset + section_ref.len) as usize];
    let n = header.entity_count as usize;
    if n < 2 {
        return None; // no distinct replacement target exists
    }
    let fwd_offsets_at = REFS_SUB_HEADER_LEN;
    let fwd_targets_at = fwd_offsets_at + (n + 1) * 4;

    let at = (0..n).find(|&at| {
        let lo = format::u32_at(section, fwd_offsets_at + at * 4).unwrap_or(0);
        let hi = format::u32_at(section, fwd_offsets_at + (at + 1) * 4).unwrap_or(0);
        hi > lo
    })?;
    let slot = format::u32_at(section, fwd_offsets_at + at * 4)? as usize;

    let packed = format::u32_at(section, fwd_targets_at + slot * 4)?;
    let target = format::refs::target_of(packed);
    let kind = format::refs::kind_of(packed);
    let replacement = (target + 1) % n as u32;
    let mutated_packed = format::refs::pack(replacement, kind);

    let mut mutated = bytes.to_vec();
    let target_off = section_ref.offset as usize + fwd_targets_at + slot * 4;
    mutated[target_off..target_off + 4].copy_from_slice(&mutated_packed.to_le_bytes());
    Some(mutated)
}

/// semx-o0x negative: corrupt one `REFS` posting and confirm the (now O(n))
/// `refs_check` still screams — proof the complexity fix didn't trade away
/// exhaustiveness. Mirrors `mutation_test`'s TRIGRAM proof shape: mutate,
/// rebuild a `QueryIndex` over the corrupted bytes, run the exact check the
/// gate runs, and require a mismatch.
fn refs_mutation_test(graph: &EntityGraph, bytes: &[u8]) {
    let started = Instant::now();
    let Some(mutated) = corrupt_refs_posting(bytes) else {
        println!("REFS_MUTATION skipped=no_refs_row_to_corrupt");
        return;
    };
    let Some(corrupt_index) = QueryIndex::from_bytes(mutated) else {
        // A corrupt posting that instead broke section soundness enough to
        // fail `open` is caught even earlier than the oracle — also a pass.
        println!("REFS_MUTATION verdict=PASS caught_at=open");
        return;
    };
    let stats = refs_check(graph, &corrupt_index);
    let caught = stats.mismatched != 0 || stats.kind_mismatched != 0;
    println!(
        "REFS_MUTATION ms={:.1} mismatched={} kind_mismatched={} verdict={}",
        ms(started),
        stats.mismatched,
        stats.kind_mismatched,
        if caught { "PASS" } else { "FAIL" }
    );
    if !caught {
        std::process::exit(1);
    }
}

fn grep_mode(index_path: &Path, root: &Path, pattern: &str, entered: Instant) {
    let started = Instant::now();
    let Some(index) = QueryIndex::open(index_path) else {
        eprintln!("index absent or stale: {}", index_path.display());
        std::process::exit(1);
    };
    println!(
        "OPEN ms={:.3} entities={} files={} has_trigram={}",
        ms(started),
        index.entity_count(),
        index.file_count(),
        index.has_trigram()
    );

    let started = Instant::now();
    let opts = GrepOptions::default();
    let report = match grep::search(&index, root, pattern, &opts, no_walk) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("invalid pattern: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "GREP pattern={pattern:?} ms={:.3} hits={} candidate_files={} total_files={} origin={:?}",
        ms(started),
        report.hits.len(),
        report.candidate_files,
        report.total_files,
        report.origin
    );
    for hit in report.hits.iter().take(3) {
        println!("  {}:{}:{}", hit.file, hit.line, hit.text);
    }
    println!("COLD_TOTAL ms={:.3}", ms(entered));
}

fn lookup_mode(index_path: &Path, names: &[String], entered: Instant) {
    let started = Instant::now();
    let Some(index) = QueryIndex::open(index_path) else {
        eprintln!("index absent or stale: {}", index_path.display());
        std::process::exit(1);
    };
    println!(
        "OPEN ms={:.3} entities={} files={}",
        ms(started),
        index.entity_count(),
        index.file_count()
    );

    for name in names {
        let started = Instant::now();
        let hits = index.lookup(name);
        // Materialize the answer — an unread `Entity` has touched no pages, so
        // timing the search alone would be measuring the wrong thing.
        let rendered: Vec<String> = hits
            .iter()
            .map(|e| {
                format!(
                    "{}\t{}\t{}:{}",
                    e.entity_type(),
                    e.id(),
                    e.file_path(),
                    e.start_line()
                )
            })
            .collect();
        println!(
            "LOOKUP name={name:?} ms={:.3} hits={}",
            ms(started),
            rendered.len()
        );
        for row in rendered.iter().take(3) {
            println!("  {row}");
        }
    }
    println!("COLD_TOTAL ms={:.3}", ms(entered));
}

/// Cold-process callers/refs latency, mirroring `lookup_mode`: OPEN once,
/// then time each name's name-lookup + CSR-row materialization together,
/// because that pair is exactly what `sem callers`/`sem refs` pay per query.
fn refs_mode(index_path: &Path, direction: &str, names: &[String], entered: Instant) {
    let started = Instant::now();
    let Some(index) = QueryIndex::open(index_path) else {
        eprintln!("index absent or stale: {}", index_path.display());
        std::process::exit(1);
    };
    println!(
        "OPEN ms={:.3} entities={} files={} has_refs={}",
        ms(started),
        index.entity_count(),
        index.file_count(),
        index.has_refs()
    );

    for name in names {
        let started = Instant::now();
        let hits = index.lookup(name);
        let rows: Vec<String> = hits
            .iter()
            .flat_map(|entity| {
                let related = if direction == "callers" {
                    index.callers_of(entity.index())
                } else {
                    index.refs_of(entity.index())
                };
                related.iter().map(|e| e.id()).collect::<Vec<_>>()
            })
            .collect();
        println!(
            "REFS name={name:?} direction={direction} ms={:.3} defs={} hits={}",
            ms(started),
            hits.len(),
            rows.len()
        );
        for row in rows.iter().take(3) {
            println!("  {row}");
        }
    }
    println!("COLD_TOTAL ms={:.3}", ms(entered));
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn make_registry(root: &Path) -> ParserRegistry {
    let mut registry = create_default_registry();
    registry.load_semrc(root);
    registry.load_gitattributes(root);
    registry
}

fn walk_files(root: &Path, registry: &ParserRegistry) -> Vec<String> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    for entry in builder.build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().to_string();
        if is_default_excluded(&relative) || is_probably_binary_path(&relative) {
            continue;
        }
        if registry.get_plugin(&relative).is_none() {
            continue;
        }
        files.push(relative);
    }
    files.sort();
    files
}
