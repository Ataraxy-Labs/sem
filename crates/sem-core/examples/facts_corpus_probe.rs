//! Cross-repo proof + timing probe for the machine-global facts corpus
//! (Phase B's local tier). Companion to `facts_probe.rs` (the
//! per-repo tier's own cross-process oracle) — this probe proves the *new*
//! claim: identical file content at the same relative path, in two
//! *different* repo roots on this machine, shares extracted facts, without
//! ever touching cached resolution edges (see `facts_store.rs`'s "Cross-repo
//! corpus" doc section for why that scope boundary is deliberate).
//!
//! Not part of the public API and not wired into any product code path.
//!
//! Usage:
//!   cargo run --release --example facts_corpus_probe -- \
//!       populate <repo_a_root> <corpus_dir> [label]
//!   cargo run --release --example facts_corpus_probe -- \
//!       consume <repo_b_root> <corpus_dir> [label]
//!
//! `populate` cold-builds `repo_a_root` and writes every file's facts into
//! the corpus at `corpus_dir` (as a real `sem` invocation would, via
//! `FactsCorpus::populate_delta` with `previous = None`).
//!
//! `consume` treats `repo_b_root` as a *fresh checkout with no local
//! FactsStore snapshot of its own* (`local = None`, matching a repo's first
//! build) and:
//!   1. Merges against the corpus (`FactsCorpus::merge_with_local`), then
//!      warm-starts a session from the result — this is the "corpus-assisted
//!      build" whose wall time and reuse counts get reported.
//!   2. Separately cold-builds the same tree with `EntityGraph::build` (no
//!      corpus involved at all) and fingerprints it.
//!   3. Asserts (a) the corpus-assisted build's fingerprint equals the cold
//! build's — proof (a) from the change: bit-identical graph vs a
//!      no-corpus cold build.
//!   4. Reports `files probed`/`files hit` from the merge stats — proof (b):
//!      cross-repo reuse actually happened (`hits > 0` whenever the two
//!      repos share content, which the caller controls by choice of
//!      `repo_b_root`).
//!   5. Reports the wall-time delta between the corpus-assisted build and
//!      the cold build — proof (c): measured time saving.
//!   6. Runs the negative proof in-process: picks one file the corpus did
//!      hit, writes its exact bytes to a *new*, never-before-seen relative
//!      path inside `repo_b_root`, and asserts a merge against just that one
//!      new path misses the corpus — same content, different path, must not
//!      share.
//!
//! # `remote-populate` / `remote-consume`: the cloud-ingestion path
//!
//! `populate`/`consume` above prove the *local* corpus tier (`populate_delta`,
//! a process deriving facts from its own read+hash pass). They do not exercise
//! `FactsCorpus::ingest_remote` at all — the API this change adds for facts that
//! arrived over a network, from a process that never touched `repo_b_root`'s
//! files itself. `remote-populate`/`remote-consume` extend this probe's
//! existing two-machine shape (two independent roots + a shared corpus dir
//! standing in for "two machines") to cover that path instead:
//!
//!   cargo run --release --example facts_corpus_probe -- \
//!       remote-populate <repo_a_root> <wire_file> [label]
//!   cargo run --release --example facts_corpus_probe -- \
//!       remote-consume <repo_b_root> <corpus_dir> <wire_file> [label] [--tamper]
//!
//! `remote-populate` cold-builds `repo_a_root` and writes every file's facts
//! to `<wire_file>` in the exact shape sem-cloud's `/v1/facts/download`
//! response uses (`FACTS-SERVICE.md`'s `FactRecord`: an echoed
//! `(relativePath, contentHash, languageSalt, schemaVersion)` key alongside an
//! opaque CBOR `payload`) — this file stands in for "what machine A uploaded
//! and the cloud served back," so `remote-consume` never touches
//! `repo_a_root` at all, only this one blob.
//!
//! `remote-consume` treats `repo_b_root` as a fresh checkout with a **brand
//! new, never-populated `corpus_dir`** (proving this is genuinely the
//! ingestion path, not `populate_delta` under another name) and:
//!   1. Decodes `<wire_file>` into `RemoteFact`s exactly like `sem-cli`'s
//!      `facts_remote.rs::decode_download_response` does, optionally
//!      tampering one record first (`--tamper`: mutates the claimed
//!      `contentHash` so it disagrees with the payload's own embedded hash —
//! the mismatched-key-vs-content shape this change's tamper proof targets).
//!   2. Calls `FactsCorpus::ingest_remote` and reports accepted/rejected
//!      counts and reasons — proof (c) starts here: a tampered fact must be
//!      rejected with a typed `IngestError`, never silently written.
//!   3. Merges the (now-populated-via-ingestion) corpus against `local = None`
//!      and warm-starts — proof (b): `corpus_hits`/`files_reused_directly`
//!      show every accepted file skipped re-extraction, and (with `--tamper`)
//!      the rejected file's `corpus_hits` count is one less than without it.
//!   4. Cold-builds the same tree and asserts bit-identical — proof (a),
//!      including under `--tamper`: the rejected file simply falls back to
//!      local extraction, so the final graph is unaffected (correct, not
//!      poisoned) even though ingestion refused one of its inputs.

use std::path::{Path, PathBuf};
use std::time::Instant;

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::facts_store::{FactsCorpus, IngestError, RemoteFact, FACTS_SCHEMA_VERSION};
use sem_core::parser::graph::{EntityGraph, RefType};
use sem_core::parser::incremental::FileFacts;
use sem_core::parser::plugins::code::languages::get_language_config;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::parser::session::GraphSession;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};
use serde::{Deserialize, Serialize};

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
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if is_default_excluded(&rel) || is_probably_binary_path(&rel) {
            continue;
        }
        if registry.get_explicit_plugin(&rel).is_none() {
            continue;
        }
        files.push(rel);
    }
    files.sort();
    files
}

struct Fingerprint {
    entities: usize,
    edges: usize,
    edge_hash: u64,
}

fn fingerprint(graph: &EntityGraph, entities: &[SemanticEntity]) -> Fingerprint {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

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
    let mut h = DefaultHasher::new();
    for edge in &edges {
        edge.hash(&mut h);
    }
    Fingerprint {
        entities: entities.len(),
        edges: graph.edges.len(),
        edge_hash: h.finish(),
    }
}

fn cmd_populate(root: &Path, corpus_dir: &Path, label: &str) {
    let registry = make_registry(root);
    let files = walk_files(root, &registry);

    let build_t0 = Instant::now();
    let session = GraphSession::build(root, &files, &registry);
    let build_ms = build_t0.elapsed().as_secs_f64() * 1000.0;
    let fp = fingerprint(session.graph(), session.entities());

    let exported = session.export_persisted();
    let corpus = FactsCorpus::open(corpus_dir);

    let populate_t0 = Instant::now();
    let stats = corpus
        .populate_delta(None, &exported, &registry)
        .expect("populate_delta");
    let populate_ms = populate_t0.elapsed().as_secs_f64() * 1000.0;

    let corpus_bytes: u64 = std::fs::read_dir(corpus_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);

    println!(
        "POPULATE label={label} files={} entities={} edges={} edge_hash={:016x} build_ms={build_ms:.2} populate_ms={populate_ms:.2} files_written={} shards_written={} corpus_bytes={corpus_bytes}",
        files.len(),
        fp.entities,
        fp.edges,
        fp.edge_hash,
        stats.files_written,
        stats.shards_written,
    );
}

fn cmd_consume(root: &Path, corpus_dir: &Path, label: &str) {
    let registry = make_registry(root);
    let files = walk_files(root, &registry);
    let corpus = FactsCorpus::open(corpus_dir);

    // Corpus-assisted build: repo B is treated as a fresh checkout with no
    // local FactsStore snapshot of its own (`local = None`).
    let merge_t0 = Instant::now();
    let (merged, stats) = corpus.merge_with_local(root, &files, &registry, None);
    let merge_ms = merge_t0.elapsed().as_secs_f64() * 1000.0;
    // Captured before `merged` moves into `warm_start` below — the negative
    // proof at the bottom needs one path the corpus actually hit.
    let a_hit_path: Option<String> = files.iter().find(|p| merged.files_contains(p)).cloned();

    let warm_t0 = Instant::now();
    let (session, rebuild_stats) = if merged.file_count() > 0 {
        GraphSession::warm_start(root, &files, &registry, merged)
    } else {
        (
            GraphSession::build(root, &files, &registry),
            Default::default(),
        )
    };
    let warm_ms = warm_t0.elapsed().as_secs_f64() * 1000.0;
    let warm_total_ms = merge_ms + warm_ms;
    let warm = fingerprint(session.graph(), session.entities());

    // Cold build of the identical tree, no corpus involved — the baseline
    // proof (a) is judged against.
    let cold_t0 = Instant::now();
    let (cold_graph, cold_entities) = EntityGraph::build(root, &files, &registry);
    let cold_ms = cold_t0.elapsed().as_secs_f64() * 1000.0;
    let cold = fingerprint(&cold_graph, &cold_entities);

    let files_reused_directly = files.len().saturating_sub(rebuild_stats.files_seed_red);

    println!(
        "CONSUME label={label} files={} probed={} corpus_hits={} files_reused_directly={} merge_ms={merge_ms:.2} warm_start_ms={warm_ms:.2} warm_total_ms={warm_total_ms:.2} cold_ms={cold_ms:.2} time_saved_ms={:.2} time_saved_pct={:.1}",
        files.len(),
        stats.probed,
        stats.hits,
        files_reused_directly,
        cold_ms - warm_total_ms,
        if cold_ms > 0.0 {
            (cold_ms - warm_total_ms) / cold_ms * 100.0
        } else {
            0.0
        },
    );

    println!(
        "ORACLE label={label} {}",
        if warm.edge_hash == cold.edge_hash
            && warm.edges == cold.edges
            && warm.entities == cold.entities
        {
            "ok".to_string()
        } else {
            format!(
                "MISMATCH warm(entities={} edges={} hash={:016x}) vs cold(entities={} edges={} hash={:016x})",
                warm.entities, warm.edges, warm.edge_hash, cold.entities, cold.edges, cold.edge_hash
            )
        }
    );

    // Negative proof: same content at a DIFFERENT relative path must not
    // falsely share. Pick a file the corpus actually hit, copy its bytes to
    // a brand-new relative path inside repo B's tree, and confirm a merge
    // against just that path misses.
    if let Some(hit_path) = a_hit_path {
        let source_bytes = std::fs::read(root.join(&hit_path)).expect("read hit file");
        let renamed_rel = format!("__semx2o8_negative_probe__/{hit_path}");
        let renamed_full = root.join(&renamed_rel);
        std::fs::create_dir_all(renamed_full.parent().unwrap()).expect("mkdir negative probe dir");
        std::fs::write(&renamed_full, &source_bytes).expect("write negative probe file");

        let (renamed_merge, renamed_stats) =
            corpus.merge_with_local(root, &[renamed_rel.clone()], &registry, None);
        let negative_ok = renamed_stats.probed == 1
            && renamed_stats.hits == 0
            && !renamed_merge.files_contains(&renamed_rel);

        let _ = std::fs::remove_dir_all(root.join("__semx2o8_negative_probe__"));

        println!(
            "NEGATIVE label={label} same_content_path={hit_path} renamed_path={renamed_rel} probed={} hits={} {}",
            renamed_stats.probed,
            renamed_stats.hits,
            if negative_ok { "ok" } else { "FAIL: falsely shared across paths" },
        );
    } else {
        println!("NEGATIVE label={label} SKIPPED: no corpus hit to base the negative proof on");
    }
}

// =============================================================================
// remote-populate / remote-consume: the ingest_remote path
// =============================================================================

/// Mirrors `sem-core/src/parser/facts_store.rs::LANGUAGE_SALTS` verbatim —
/// deliberately, unlike `sem-cli`'s `facts_remote.rs` (retired that
/// mirror: it is the same crate graph, version-locked to this `sem-core` via
/// a path dependency, so it now imports the real
/// `facts_store::effective_language_salt` instead of hand-copying the
/// table). This probe's `remote-populate`/`remote-consume` commands
/// specifically exercise `ingest_remote`'s server-side salt *validation* —
/// they simulate an independent uploading client computing its own salt for
/// a file, so the ingest side can reject or accept it against sem-core's own
/// answer. Calling `facts_store::effective_language_salt` here directly
/// would make that check tautological (a value compared to itself). So this
/// copy is kept, but guarded: [`language_salts_match_sem_core_table`] below
/// asserts byte-equality against the real table on every test run, turning
/// "can silently drift" into "fails a build the moment it drifts."
const LANGUAGE_SALTS: &[(&str, &str)] = &[
    ("typescript", "ts-0.23-u16"),
    ("tsx", "ts-0.23-u16"),
    ("javascript", "ts-0.23-u16"),
    ("python", "ts-0.23-mp4"),
    ("go", "ts-0.23-mp5-dm5t-bpn2"),
    ("rust", "ts-0.23-mp2"),
    ("java", "ts-0.23-mp3"),
    ("c", "ts-0.23"),
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
    ("csv", "handrolled-1"),
    ("json", "handrolled-1"),
    ("yaml", "handrolled-2"),
    ("markdown", "handrolled-1"),
    ("latex", "handrolled-1"),
    ("vue", "handrolled-1"),
    ("fallback", "handrolled-1"),
];
const DEFAULT_LANGUAGE_SALT: &str = "unmapped-1";

fn language_salt(lang_id: &str) -> &'static str {
    // Mirrors `facts_store::producer_language_salt` / `resolve_gated_salt`:
    // any language registered in `MUL_RUNTIME_GATES` (MUL phase 1's C#/C++
    // precomputes, MUL phase 2's Go/Java/Rust/Python precomputes — C++ and
    // Python joined the table on 2026-08-22, demoted from their prior
    // unconditional admissions once the ceiling was redefined against peak
    // memory footprint) has a run-time producer switch, so the salt this
    // probe predicts has to follow it, or the oracle reports a miss against
    // a corpus entry that is in fact correct. Consulting the real table
    // replaces what used to be this file's own independent
    // `if lang_id == "csharp"` copy — a second hand-written branch that
    // would have needed updating in lockstep with `facts_store.rs`'s, and
    // wouldn't have been, for any future gated language neither file's
    // author remembered to touch.
    if let Some(gate) = sem_core::parser::scope_resolve::MUL_RUNTIME_GATES
        .iter()
        .find(|g| g.lang_id == lang_id)
    {
        if !sem_core::parser::scope_resolve::mul_precompute_admits(lang_id) {
            return gate.pre_switch_salt;
        }
    }
    LANGUAGE_SALTS
        .iter()
        .find(|(id, _)| *id == lang_id)
        .map(|(_, salt)| *salt)
        .unwrap_or(DEFAULT_LANGUAGE_SALT)
}

fn detect_language_id(file_path: &str, registry: &ParserRegistry) -> String {
    if let Some(file_name) = Path::new(file_path).file_name().and_then(|n| n.to_str()) {
        let lower = file_name.to_lowercase();
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

/// One `FactRecord` in the wire shape `FACTS-SERVICE.md` documents for
/// `/v1/facts/download` — the echoed key alongside the opaque payload.
/// `content_hash` travels as decimal text on the real wire (JSON-number
/// precision); this probe keeps that convention even though it never touches
/// JSON, so `remote-consume`'s parse step exercises the same code shape
/// `sem-cli`'s real client does.
#[derive(Serialize, Deserialize)]
struct WireFactRecord {
    relative_path: String,
    content_hash: String,
    language_salt: String,
    schema_version: u32,
    payload: Vec<u8>,
}

fn cmd_remote_populate(root: &Path, wire_file: &Path, label: &str) {
    let registry = make_registry(root);
    let files = walk_files(root, &registry);

    let build_t0 = Instant::now();
    let session = GraphSession::build(root, &files, &registry);
    let build_ms = build_t0.elapsed().as_secs_f64() * 1000.0;
    let fp = fingerprint(session.graph(), session.entities());

    // "Upload": encode every file's real, locally-derived facts into the
    // wire shape a cloud download response would hand back — this machine
    // never touches repo B; `<wire_file>` is the only channel between them.
    // `export_facts` (unlike `export_persisted`) returns plain, public
    // `FileFacts` — the only shape a real cloud protocol carries anyway (no
    // `precomputed`/`resolution`, which sem-core's local corpus never
    // shares cross-repo either — see `facts_store.rs`'s "What is NOT shared
    // cross-repo" doc).
    let exported: std::collections::HashMap<String, FileFacts> = session
        .export_facts()
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();
    let mut records: Vec<WireFactRecord> = Vec::with_capacity(files.len());
    for path in &files {
        let Some(facts) = exported.get(path.as_str()) else {
            continue; // e.g. an unreadable/unsupported file — nothing to share
        };
        let lang_id = detect_language_id(path, &registry);
        let payload = {
            let mut buf = Vec::new();
            ciborium::into_writer(facts, &mut buf).expect("encode FileFacts payload");
            buf
        };
        records.push(WireFactRecord {
            relative_path: path.clone(),
            content_hash: facts.content_hash.to_string(),
            language_salt: language_salt(&lang_id).to_string(),
            schema_version: FACTS_SCHEMA_VERSION,
            payload,
        });
    }

    let mut buf = Vec::new();
    ciborium::into_writer(&records, &mut buf).expect("encode wire file");
    std::fs::write(wire_file, &buf).expect("write wire file");

    println!(
        "REMOTE_POPULATE label={label} files={} entities={} edges={} edge_hash={:016x} build_ms={build_ms:.2} records_written={} wire_bytes={}",
        files.len(),
        fp.entities,
        fp.edges,
        fp.edge_hash,
        records.len(),
        buf.len(),
    );
}

fn cmd_remote_consume(root: &Path, corpus_dir: &Path, wire_file: &Path, label: &str, tamper: bool) {
    // A brand-new, never-populated corpus dir — this proves ingest_remote is
    // the path under test, not populate_delta wearing a different name.
    assert!(
        !corpus_dir.exists(),
        "remote-consume expects a fresh corpus_dir that has never been populated \
         (got an existing path: {corpus_dir:?}) — pass a directory that does not exist yet"
    );

    let registry = make_registry(root);
    let files = walk_files(root, &registry);

    let bytes = std::fs::read(wire_file).expect("read wire file");
    let mut records: Vec<WireFactRecord> =
        ciborium::from_reader(bytes.as_slice()).expect("decode wire file");

    let mut tampered_path: Option<String> = None;
    if tamper {
        let rec = records
            .iter_mut()
            .next()
            .expect("wire file must have at least one record to tamper");
        tampered_path = Some(rec.relative_path.clone());
        // Mismatched key vs content: claim a content_hash that disagrees
        // with the payload's own embedded FileFacts::content_hash —
        // exactly the tamper shape this change's E2E proof targets.
        let bumped: u64 = rec.content_hash.parse::<u64>().unwrap_or(0).wrapping_add(1);
        rec.content_hash = bumped.to_string();
    }

    let remote_facts: Vec<RemoteFact> = records
        .into_iter()
        .filter_map(|rec| {
            let claimed_content_hash: u64 = rec.content_hash.parse().ok()?;
            let facts: FileFacts = ciborium::from_reader(rec.payload.as_slice()).ok()?;
            Some(RemoteFact {
                facts,
                precomputed: None,
                claimed_relative_path: rec.relative_path,
                claimed_content_hash,
                claimed_language_salt: rec.language_salt,
                claimed_schema_version: rec.schema_version,
            })
        })
        .collect();
    let claimed_count = remote_facts.len();

    let corpus = FactsCorpus::open(corpus_dir);
    let ingest_t0 = Instant::now();
    let outcome = corpus
        .ingest_remote(&registry, remote_facts)
        .expect("ingest_remote");
    let ingest_ms = ingest_t0.elapsed().as_secs_f64() * 1000.0;

    println!(
        "INGEST label={label} claimed={claimed_count} accepted={} rejected={} ingest_ms={ingest_ms:.2}",
        outcome.accepted.files_written,
        outcome.rejected.len(),
    );
    for (path, err) in &outcome.rejected {
        println!("INGEST_REJECTED label={label} path={path} reason={err}");
    }

    if tamper {
        let path = tampered_path.expect("tamper flag implies a tampered path");
        let rejected_the_tampered_one = outcome
            .rejected
            .iter()
            .any(|(p, e)| p == &path && matches!(e, IngestError::ContentHashMismatch { .. }));
        println!(
            "TAMPER label={label} path={path} {}",
            if rejected_the_tampered_one && outcome.rejected.len() == 1 {
                "ok: rejected with ContentHashMismatch, nothing else rejected".to_string()
            } else {
                format!(
                    "FAIL: expected exactly one ContentHashMismatch rejection for {path}, got {:?}",
                    outcome.rejected
                )
            }
        );
    }

    // Corpus-assisted build: repo B is a fresh checkout with no local
    // FactsStore snapshot (`local = None`) — every hit below came from
    // ingest_remote, never from a local read+hash pass.
    let merge_t0 = Instant::now();
    let (merged, stats) = corpus.merge_with_local(root, &files, &registry, None);
    let merge_ms = merge_t0.elapsed().as_secs_f64() * 1000.0;

    let warm_t0 = Instant::now();
    let (session, rebuild_stats) = if merged.file_count() > 0 {
        GraphSession::warm_start(root, &files, &registry, merged)
    } else {
        (
            GraphSession::build(root, &files, &registry),
            Default::default(),
        )
    };
    let warm_ms = warm_t0.elapsed().as_secs_f64() * 1000.0;
    let warm = fingerprint(session.graph(), session.entities());
    let files_reused_directly = files.len().saturating_sub(rebuild_stats.files_seed_red);

    let (cold_graph, cold_entities) = EntityGraph::build(root, &files, &registry);
    let cold = fingerprint(&cold_graph, &cold_entities);

    println!(
        "REMOTE_CONSUME label={label} files={} probed={} corpus_hits={} files_reused_directly={} merge_ms={merge_ms:.2} warm_start_ms={warm_ms:.2}",
        files.len(),
        stats.probed,
        stats.hits,
        files_reused_directly,
    );

    println!(
        "ORACLE label={label} {}",
        if warm.edge_hash == cold.edge_hash
            && warm.edges == cold.edges
            && warm.entities == cold.entities
        {
            "ok".to_string()
        } else {
            format!(
                "MISMATCH warm(entities={} edges={} hash={:016x}) vs cold(entities={} edges={} hash={:016x})",
                warm.entities, warm.edges, warm.edge_hash, cold.entities, cold.edges, cold.edge_hash
            )
        }
    );
}

const USAGE: &str = "usage:\n  \
    facts_corpus_probe populate <repo_a_root> <corpus_dir> [label]\n  \
    facts_corpus_probe consume <repo_b_root> <corpus_dir> [label]\n  \
    facts_corpus_probe remote-populate <repo_a_root> <wire_file> [label]\n  \
    facts_corpus_probe remote-consume <repo_b_root> <corpus_dir> <wire_file> [label] [--tamper]";

fn default_label(root: &Path) -> String {
    root.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("{USAGE}");
        std::process::exit(1);
    }
    let mode = args[1].as_str();

    match mode {
        "populate" | "consume" => {
            if args.len() < 4 {
                eprintln!("{USAGE}");
                std::process::exit(1);
            }
            let root = PathBuf::from(&args[2]).canonicalize().expect("bad root");
            let corpus_dir = PathBuf::from(&args[3]);
            let label = args.get(4).cloned().unwrap_or_else(|| default_label(&root));
            if mode == "populate" {
                cmd_populate(&root, &corpus_dir, &label);
            } else {
                cmd_consume(&root, &corpus_dir, &label);
            }
        }
        "remote-populate" => {
            if args.len() < 4 {
                eprintln!("{USAGE}");
                std::process::exit(1);
            }
            let root = PathBuf::from(&args[2]).canonicalize().expect("bad root");
            let wire_file = PathBuf::from(&args[3]);
            let label = args.get(4).cloned().unwrap_or_else(|| default_label(&root));
            cmd_remote_populate(&root, &wire_file, &label);
        }
        "remote-consume" => {
            if args.len() < 5 {
                eprintln!("{USAGE}");
                std::process::exit(1);
            }
            let root = PathBuf::from(&args[2]).canonicalize().expect("bad root");
            let corpus_dir = PathBuf::from(&args[3]);
            let wire_file = PathBuf::from(&args[4]);
            let rest = &args[5..];
            let tamper = rest.iter().any(|a| a == "--tamper");
            let label = rest
                .iter()
                .find(|a| *a != "--tamper")
                .cloned()
                .unwrap_or_else(|| default_label(&root));
            cmd_remote_consume(&root, &corpus_dir, &wire_file, &label, tamper);
        }
        other => {
            eprintln!("unknown mode {other:?}\n{USAGE}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LANGUAGE_SALTS;

    /// this probe's `LANGUAGE_SALTS` is a deliberate, independent
    /// copy (see the doc comment on the const above for why it must stay a
    /// real second computation rather than calling
    /// `facts_store::effective_language_salt` directly). "Deliberate" must
    /// not mean "silently allowed to drift" — this is the guard: it fails
    /// the moment this table and sem-core's real one disagree on any
    /// language, catching exactly the hand-bump-discipline lapse the salt
    /// table's own doc comment warns about (bump a grammar dependency or a
    /// hand-rolled plugin's extraction shape, forget to bump the salt here).
    #[test]
    fn language_salts_match_sem_core_table() {
        let real = sem_core::parser::facts_store::LANGUAGE_SALTS;
        assert_eq!(
            LANGUAGE_SALTS, real,
            "this probe's LANGUAGE_SALTS mirror has drifted from \
             sem_core::parser::facts_store::LANGUAGE_SALTS — bump this \
             table to match (see the doc comment on this file's \
             LANGUAGE_SALTS for why it is a deliberate independent copy \
             rather than a direct import)"
        );
    }
}
