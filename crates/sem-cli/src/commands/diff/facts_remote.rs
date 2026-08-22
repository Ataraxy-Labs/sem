//! Cross-machine facts service client (cloud half): sem-cloud's
//! `POST /v1/facts/{query,download,put}` API from the CLI side. See
//! sem-cloud's `docs/architecture/FACTS-SERVICE.md` for the protocol this
//! wraps (key scheme, consent, wire format, trust boundary).
//!
//! # Scope — deliberately narrow
//!
//! This module extracts and shares facts for exactly the files a
//! `sem diff` touched, via its own small, self-contained extraction
//! (`ParserRegistry::extract_all_entities`, scoped to just those files) —
//! independent of the
//! full-repo relations graph (`relations.rs`'s `build_relations_from_graph`,
//! which needs the *whole* repo's entities to find cross-file callers/
//! callees, and consults `graph.rs`'s own local `FactsStore`/`FactsCorpus`
//! caches already).
//!
//! # Closing Phase B's last mile
//!
//! Known files' downloaded payloads used to be decoded purely to verify the
//! wire round trip — nothing downstream consumed the entities, because
//! sem-core had no public way to seed `GraphSession`/`FactsCorpus` from
//! externally-sourced `FileFacts` (`PersistedFacts::new`/`CorpusFile` were
//! crate-private, and `FactsCorpus` had no insert-shaped method for facts it
//! didn't derive from a local read+hash itself). `FactsCorpus::ingest_remote`
//! (`sem_core::parser::facts_store`) closes that gap: every known file this
//! module downloads is now decoded into a `RemoteFact` (payload plus the key
//! it was fetched *under*, echoed back per-record by the server — see
//! `FACTS-SERVICE.md`'s `FactRecord` shape) and ingested into the same local
//! `FactsCorpus` `commands/graph.rs`'s `build_graph_with_facts_store` already
//! consults on every full-repo build. `ingest_remote` re-validates each
//! record's claimed key against its own payload before writing anything —
//! this module never trusts a downloaded record's key at face value (see
//! `ingest_remote`'s doc for why: the corpus is machine-global, so one
//! mis-keyed entry would poison every future repo's lookups, not just this
//! diff's). A file whose downloaded record fails that check is simply never
//! written to the corpus — the next full-repo build's `merge_with_local`
//! treats it as an ordinary miss and falls back to local extraction,
//! correct, not poisoned.
//!
//! # Flow
//!
//! Called once from `cloud_upload.rs`, after the diff is already computed
//! (so touched file paths + a live `CloudClient` exist):
//!
//! 1. [`sync`] hashes every touched file (cheap — no parsing yet) and
//!    batch-queries the cloud (`query_facts`) — synchronous, since it's a
//!    single small request.
//! 2. Known files: fetched via `download_facts`, decoded, key-validated, and
//!    ingested into the local `FactsCorpus` (`ingest_downloaded_facts`) — so
//!    a *later* full-repo relations/graph build for this same repo (or any
//!    other repo on this machine sharing the file's content, via the corpus)
//!    genuinely skips re-extracting them.
//! 3. Unknown files: extracted locally (`ParserRegistry::extract_all_entities`)
//!    and uploaded (`put_facts`) — "extract only misses."
//!
//! Steps 2–3 run on a background thread that [`sync`] waits for, bounded by
//! [`BACKGROUND_WAIT`] — **not** a truly detached fire-and-forget thread:
//! see that constant's doc for why a Rust process exiting kills every
//! still-running thread regardless, making an unbounded detach a
//! correctness bug (silently-dropped uploads racing the CLI's own exit),
//! not just a style choice. A PUT rejection (or any network failure) is a
//! debug-level log line, never a build failure or a printed warning — this
//! is best-effort corpus contribution, not part of the diff command's
//! contract. `SEM_FACTS_REMOTE=0` disables the whole module; it is also
//! silently inert whenever `cloud_upload::DiffCloudContext` itself is
//! `None` (no remote, no consent, `SEM_LOCAL`/`SEM_NO_NETWORK`, not logged
//! in) — this module never re-derives that decision, it is only
//! ever called from inside the branch that already made it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::facts_store::{effective_language_salt, RemoteFact, FACTS_SCHEMA_VERSION};
use sem_core::parser::incremental::{content_hash, FileFacts};
use sem_core::parser::plugins::code::languages::get_language_config;
use sem_core::parser::registry::ParserRegistry;
use serde::{Deserialize, Serialize};

use super::super::cloud::CloudClient;

/// `SEM_FACTS_REMOTE=0` disables this whole module — checked once, here,
/// never re-checked deeper in the call chain.
fn enabled() -> bool {
    std::env::var("SEM_FACTS_REMOTE").ok().as_deref() != Some("0")
}

// ---------------------------------------------------------------------------
// Wire types (mirrors sem-cloud's src/facts.rs — see FACTS-SERVICE.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FactKey {
    pub(crate) relative_path: String,
    pub(crate) content_hash: String,
    pub(crate) language_salt: String,
    pub(crate) schema_version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FactsQueryResponse {
    pub(crate) known_indices: Vec<usize>,
    #[allow(dead_code)] // index-complement of known_indices; kept for wire fidelity
    pub(crate) unknown_indices: Vec<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RejectedFact {
    #[allow(dead_code)]
    pub(crate) index: usize,
    pub(crate) reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PutFactsResponse {
    pub(crate) accepted: Vec<usize>,
    pub(crate) rejected: Vec<RejectedFact>,
}

// ---------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------
//
// this used to hand-mirror `sem-core/src/parser/facts_store.rs`'s
// `LANGUAGE_SALTS` table and its `language_salt`/`producer_language_salt`/
// `effective_language_salt` functions verbatim (per the cloud-tier
// handoff comment, which offered mirroring or taking the salt verbatim from
// the uploading client). That duplication had no real justification: unlike
// `examples/facts_corpus_probe.rs` (which deliberately simulates an
// independent external client for the `ingest_remote` wire-protocol test),
// this crate already depends on `sem-core` via a path dependency, version-
// locked to the exact same build — there is no wire boundary between them to
// simulate. `effective_language_salt` is now consumed directly, so there is
// exactly one hand-maintained `LANGUAGE_SALTS` table in the whole workspace.

/// Same detection sem-core's own (crate-private) `facts_store::detect_language_id`
/// uses, built from the same PUBLIC primitives
/// (`plugins::code::languages::get_language_config` +
/// `ParserRegistry::get_explicit_plugin`), so it agrees byte-for-byte with
/// what sem-core's local `FactsCorpus` would compute for the same file —
/// this calls sem-core's real language-detection table rather than
/// approximating it.
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

/// Read `root/relative_path`, hash it, and build its `FactKey`. `None` for
/// anything unreadable (deleted, binary-that-fails-utf8, permissions) —
/// same clean-miss discipline sem-core's own facts layer uses throughout.
fn build_key(
    root: &Path,
    relative_path: &str,
    registry: &ParserRegistry,
) -> Option<(FactKey, u64)> {
    let content = std::fs::read_to_string(root.join(relative_path)).ok()?;
    let hash = content_hash(&content);
    let lang_id = detect_language_id(relative_path, registry);
    let key = FactKey {
        relative_path: relative_path.to_string(),
        content_hash: hash.to_string(),
        language_salt: effective_language_salt(&lang_id),
        schema_version: FACTS_SCHEMA_VERSION,
    };
    Some((key, hash))
}

// ---------------------------------------------------------------------------
// Payload encode/decode — mirrors sem-core's public FileFacts shape
// ---------------------------------------------------------------------------

/// CBOR-encodes `{ path, content_hash, entities }`, the same field names
/// sem-core's public `FileFacts` struct serializes as (see
/// FACTS-SERVICE.md's "Wire format" section) — without depending on that
/// (crate-private-to-construct) type itself.
#[derive(Serialize)]
struct WireFileFacts<'a> {
    path: &'a str,
    content_hash: u64,
    entities: &'a [SemanticEntity],
}

fn encode_payload(path: &str, hash: u64, entities: &[SemanticEntity]) -> Vec<u8> {
    let mut buf = Vec::new();
    let facts = WireFileFacts {
        path,
        content_hash: hash,
        entities,
    };
    // Best-effort: an encode failure just means this one file is skipped,
    // never a build failure — see module docs.
    let _ = ciborium::into_writer(&facts, &mut buf);
    buf
}

#[derive(Deserialize)]
struct WireFileFactsOwned {
    path: String,
    #[allow(dead_code)]
    content_hash: u64,
    entities: Vec<SemanticEntity>,
}

fn decode_payload(payload: &[u8]) -> Option<(String, Vec<SemanticEntity>)> {
    let facts: WireFileFactsOwned = ciborium::from_reader(payload).ok()?;
    Some((facts.path, facts.entities))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PutRequestRecord<'a> {
    relative_path: &'a str,
    content_hash: String,
    language_salt: &'a str,
    schema_version: u32,
    #[serde(with = "serde_bytes")]
    payload: Vec<u8>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PutRequest<'a> {
    clone_url: &'a str,
    facts: Vec<PutRequestRecord<'a>>,
}

/// Bounded wait for the background upload (see `sync`'s doc for why this
/// exists at all): a Rust process that returns from `main` kills every
/// still-running thread immediately, detached or not — an OS thread does
/// NOT keep the process alive on its own. A truly fire-and-forget
/// `thread::spawn` with the `JoinHandle` dropped would race `sem diff`'s own
/// exit and could be killed mid-upload before a single byte reaches the
/// server. This bound is what turns "fire-and-forget" into "best-effort,
/// bounded" instead of "usually silently does nothing": generous for the
/// small, single-diff-sized batch this module ever uploads (never a
/// full-repo pass), tight enough that an unreachable cloud can't meaningfully
/// delay the command's exit. `ureq`'s own per-request timeout
/// (`API_TIMEOUT_SECS`, `commands/cloud.rs`) already bounds any single HTTP
/// call to 10s; this is the ceiling on the whole query+download+extract+put
/// sequence together.
const BACKGROUND_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Batch-query the cloud for every touched file in `changed_files`
/// (synchronous — one small request), then run the rest (download+verify
/// known files, extract+upload unknown ones) on a background thread that
/// this call waits for, bounded by [`BACKGROUND_WAIT`] (see its doc for
/// why an unbounded detach would be a correctness bug, not just a style
/// choice). See module docs for the full flow and its scope.
pub(crate) fn sync(
    root: &Path,
    remote_url: &str,
    registry: &ParserRegistry,
    client: CloudClient,
    changed_files: &[String],
) {
    if !enabled() {
        return;
    }

    let mut keyed: Vec<(String, FactKey, u64)> = Vec::new();
    let mut seen = HashSet::new();
    for path in changed_files {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Some((key, hash)) = build_key(root, path, registry) {
            keyed.push((path.clone(), key, hash));
        }
    }
    if keyed.is_empty() {
        return;
    }

    let keys: Vec<FactKey> = keyed.iter().map(|(_, k, _)| k.clone()).collect();
    let Ok(query) = client.query_facts(&keys) else {
        return; // best-effort: unreachable cloud, treat as "nothing known"
    };
    let known: HashSet<usize> = query.known_indices.into_iter().collect();

    let root_owned = root.to_path_buf();
    let cwd = root.to_string_lossy().to_string();
    let remote_url = remote_url.to_string();

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("sem-facts-remote".into())
        .spawn(move || {
            run_background(root_owned, cwd, remote_url, client, keyed, known);
            let _ = done_tx.send(());
        });
    if spawned.is_ok() {
        let _ = done_rx.recv_timeout(BACKGROUND_WAIT);
    }
}

fn run_background(
    root: PathBuf,
    cwd: String,
    remote_url: String,
    client: CloudClient,
    keyed: Vec<(String, FactKey, u64)>,
    known: HashSet<usize>,
) {
    let registry = super::super::create_registry(&cwd);

    // Known files: download, decode, key-validate, and ingest into the
    // local `FactsCorpus` — the corpus `commands/graph.rs`'s
    // `build_graph_with_facts_store` already consults on every full-repo
    // build, so a later build for this repo (or any other repo on this
    // machine sharing the file's content) genuinely skips re-extracting
    // these files. See module docs for the full flow.
    let known_keys: Vec<FactKey> = keyed
        .iter()
        .enumerate()
        .filter(|(i, _)| known.contains(i))
        .map(|(_, (_, k, _))| k.clone())
        .collect();
    if !known_keys.is_empty() {
        if let Ok(bytes) = client.download_facts(&known_keys) {
            let remote_facts = decode_download_response(&bytes);
            ingest_downloaded_facts(&root, &registry, remote_facts);
        }
    }

    // Unknown files: extract only these ("extract only misses" — see module
    // docs), then upload whatever came out.
    let novel: Vec<&(String, FactKey, u64)> = keyed
        .iter()
        .enumerate()
        .filter(|(i, _)| !known.contains(i))
        .map(|(_, x)| x)
        .collect();
    if novel.is_empty() {
        return;
    }

    // The real, full-fidelity extraction primitive (content, hashes, kappa,
    // byte spans included) -- not EntityGraph::build's lighter EntityInfo
    // shape, which drops all of that. "Extract only misses": this call only
    // ever runs over `novel_paths`, never the files the query step already
    // confirmed the cloud knows.
    let novel_paths: Vec<String> = novel.iter().map(|(p, _, _)| p.clone()).collect();
    let extracted = registry.extract_all_entities(&root, &novel_paths);
    let mut entities_by_file: std::collections::HashMap<String, Vec<SemanticEntity>> =
        std::collections::HashMap::new();
    for e in extracted {
        entities_by_file
            .entry(e.file_path.clone())
            .or_default()
            .push(e);
    }

    let mut records = Vec::new();
    for (path, key, hash) in &novel {
        let Some(entities) = entities_by_file.get(path) else {
            continue; // no entities extracted (e.g. an empty/unsupported file) -- nothing to share
        };
        let payload = encode_payload(path, *hash, entities);
        if payload.is_empty() {
            continue; // encode failed -- best-effort skip
        }
        records.push(PutRequestRecord {
            relative_path: path,
            content_hash: key.content_hash.clone(),
            language_salt: &key.language_salt,
            schema_version: key.schema_version,
            payload,
        });
    }
    if records.is_empty() {
        return;
    }

    let req = PutRequest {
        clone_url: &remote_url,
        facts: records,
    };
    let mut body = Vec::new();
    if ciborium::into_writer(&req, &mut body).is_err() {
        return;
    }
    // Fire-and-forget: a rejection or network failure is never surfaced as
    // a warning (this is best-effort corpus contribution, not part of the
    // diff command's contract) — see module docs. `SEM_FACTS_DEBUG=1` opts
    // into a stderr line per rejected fact, for debugging the protocol
    // itself; off by default, same "quiet unless asked" convention as
    // sem-cli's other `SEM_*` diagnostic knobs.
    if let Ok(resp) = client.put_facts(&body) {
        if std::env::var("SEM_FACTS_DEBUG").is_ok_and(|v| v == "1") {
            eprintln!(
                "sem facts: {} accepted, {} rejected",
                resp.accepted.len(),
                resp.rejected.len()
            );
            for r in &resp.rejected {
                eprintln!("sem facts: rejected — {}", r.reason);
            }
        }
    }
}

/// Decode a `/v1/facts/download` response body into [`RemoteFact`]s ready
/// for [`FactsCorpus::ingest_remote`]. Each `FactRecord` echoes back the key
/// it was stored/served under (`relativePath`/`contentHash`/`languageSalt`/
/// `schemaVersion` — see `FACTS-SERVICE.md`'s "Wire format") *alongside* its
/// opaque `payload` — that per-record echoed key, not the keys this process
/// originally queried with, is what becomes `RemoteFact::claimed_*`, so a
/// server that (bug or malice) returned records out of request order still
/// gets validated against the key it actually claims, not one this process
/// merely hoped lined up positionally.
///
/// Best-effort throughout: a record whose `payload` fails to decode, or
/// whose `content_hash` text fails to parse as `u64`, is simply skipped —
/// the same "never crash, never trust, worst case is a missed corpus
/// contribution" discipline every other best-effort path in this module
/// already uses. `ingest_remote` is the layer that catches a record that
/// *does* decode but whose claimed key disagrees with its own payload.
fn decode_download_response(bytes: &[u8]) -> Vec<RemoteFact> {
    #[derive(Deserialize)]
    struct DownloadBody {
        #[allow(dead_code)]
        truncated: bool,
        facts: Vec<DownloadedRecord>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DownloadedRecord {
        relative_path: String,
        content_hash: String,
        language_salt: String,
        schema_version: u32,
        #[serde(with = "serde_bytes")]
        payload: Vec<u8>,
    }
    let Ok(body) = ciborium::from_reader::<DownloadBody, _>(bytes) else {
        return Vec::new();
    };
    body.facts
        .into_iter()
        .filter_map(|rec| {
            let claimed_content_hash: u64 = rec.content_hash.parse().ok()?;
            let (path, entities) = decode_payload(&rec.payload)?;
            Some(RemoteFact {
                facts: FileFacts {
                    content_hash: claimed_content_hash,
                    path,
                    entities,
                },
                precomputed: None,
                claimed_relative_path: rec.relative_path,
                claimed_content_hash,
                claimed_language_salt: rec.language_salt,
                claimed_schema_version: rec.schema_version,
            })
        })
        .collect()
}

/// Open this repo's local `FactsCorpus` (same resolution `commands/graph.rs`'s
/// `facts_corpus_for` uses — off via `--no-cache`/`SEM_FACTS_CACHE=0`/
/// `SEM_FACTS_CORPUS=0` exactly like every other corpus write) and ingest
/// `facts` into it. Best-effort: an ingestion I/O failure is never surfaced
/// as a warning (same "quiet unless asked" convention `SEM_FACTS_DEBUG`
/// gates everywhere else in this module) — the corpus is a pure speed
/// optimization, never part of this command's contract.
fn ingest_downloaded_facts(root: &Path, registry: &ParserRegistry, facts: Vec<RemoteFact>) {
    if facts.is_empty() {
        return;
    }
    let Some(corpus) = super::super::graph::facts_corpus_for(root, false) else {
        return;
    };
    let debug = std::env::var("SEM_FACTS_DEBUG").is_ok_and(|v| v == "1");
    match corpus.ingest_remote(registry, facts) {
        Ok(outcome) => {
            if debug {
                eprintln!(
                    "sem facts: ingested {} downloaded fact(s) into local corpus, {} rejected",
                    outcome.accepted.files_written,
                    outcome.rejected.len()
                );
                for (path, err) in &outcome.rejected {
                    eprintln!("sem facts: ingest rejected {path}: {err}");
                }
            }
        }
        Err(e) => {
            if debug {
                eprintln!("sem facts: ingest_remote failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::model::entity::SemanticEntity;

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

    /// Builds a `/v1/facts/download` response body exactly like
    /// sem-cloud's `routes/facts.rs` does (`FACTS-SERVICE.md`'s "Wire
    /// format"), so this test exercises the same shape a real server sends,
    /// not a simplified stand-in.
    fn download_body(records: &[(&str, u64, &str, u32, &[u8])]) -> Vec<u8> {
        #[derive(Serialize)]
        struct WireRecord<'a> {
            #[serde(rename = "relativePath")]
            relative_path: &'a str,
            #[serde(rename = "contentHash")]
            content_hash: String,
            #[serde(rename = "languageSalt")]
            language_salt: &'a str,
            #[serde(rename = "schemaVersion")]
            schema_version: u32,
            #[serde(with = "serde_bytes")]
            payload: &'a [u8],
        }
        #[derive(Serialize)]
        struct WireBody<'a> {
            truncated: bool,
            facts: Vec<WireRecord<'a>>,
        }
        let facts = records
            .iter()
            .map(|(path, hash, salt, schema, payload)| WireRecord {
                relative_path: path,
                content_hash: hash.to_string(),
                language_salt: salt,
                schema_version: *schema,
                payload,
            })
            .collect();
        let mut buf = Vec::new();
        ciborium::into_writer(
            &WireBody {
                truncated: false,
                facts,
            },
            &mut buf,
        )
        .unwrap();
        buf
    }

    #[test]
    fn decode_download_response_round_trips_a_well_formed_record() {
        let hash = content_hash("export const a = 1;\n");
        let payload = encode_payload("a.ts", hash, &[entity("a.ts", "a")]);
        let body = download_body(&[("a.ts", hash, "ts-0.23", FACTS_SCHEMA_VERSION, &payload)]);

        let facts = decode_download_response(&body);
        assert_eq!(facts.len(), 1);
        let f = &facts[0];
        assert_eq!(f.facts.path, "a.ts");
        assert_eq!(f.facts.content_hash, hash);
        assert_eq!(f.claimed_relative_path, "a.ts");
        assert_eq!(f.claimed_content_hash, hash);
        assert_eq!(f.claimed_language_salt, "ts-0.23");
        assert_eq!(f.claimed_schema_version, FACTS_SCHEMA_VERSION);
    }

    #[test]
    fn decode_download_response_skips_a_record_with_unparseable_content_hash() {
        let payload = encode_payload("a.ts", 1, &[entity("a.ts", "a")]);
        #[derive(Serialize)]
        struct WireRecord<'a> {
            #[serde(rename = "relativePath")]
            relative_path: &'a str,
            #[serde(rename = "contentHash")]
            content_hash: &'a str,
            #[serde(rename = "languageSalt")]
            language_salt: &'a str,
            #[serde(rename = "schemaVersion")]
            schema_version: u32,
            #[serde(with = "serde_bytes")]
            payload: &'a [u8],
        }
        #[derive(Serialize)]
        struct WireBody<'a> {
            truncated: bool,
            facts: Vec<WireRecord<'a>>,
        }
        let body_bytes = {
            let mut buf = Vec::new();
            ciborium::into_writer(
                &WireBody {
                    truncated: false,
                    facts: vec![WireRecord {
                        relative_path: "a.ts",
                        content_hash: "not-a-number",
                        language_salt: "ts-0.23",
                        schema_version: FACTS_SCHEMA_VERSION,
                        payload: &payload,
                    }],
                },
                &mut buf,
            )
            .unwrap();
            buf
        };
        assert!(decode_download_response(&body_bytes).is_empty());
    }

    #[test]
    fn decode_download_response_is_a_clean_miss_on_garbage_bytes() {
        assert!(decode_download_response(b"not cbor at all \xff\x00").is_empty());
    }

    #[test]
    fn ingest_downloaded_facts_flows_into_the_local_corpus_and_ingest_rejects_a_tampered_record() {
        // Full loop: a well-formed downloaded record must land in the same
        // local `FactsCorpus` `commands/graph.rs` consults, key-validated —
        // a tampered record (claimed content_hash disagreeing with the
        // payload's own) must be rejected and never appear in the corpus.
        let corpus_dir = tempfile::tempdir().expect("corpus dir");
        std::env::set_var("SEM_FACTS_CORPUS_DIR", corpus_dir.path());
        std::env::set_var("SEM_FACTS_CACHE", "1");
        std::env::set_var("SEM_FACTS_CORPUS", "1");

        let repo = tempfile::tempdir().expect("repo dir");
        let registry = crate::commands::create_registry(&repo.path().to_string_lossy());

        let good_hash = content_hash("export const a = 1;\n");
        let good_payload = encode_payload("a.ts", good_hash, &[entity("a.ts", "a")]);
        let bad_hash = content_hash("export const b = 2;\n");
        let bad_payload = encode_payload("b.ts", bad_hash, &[entity("b.ts", "b")]);
        let body = download_body(&[
            (
                "a.ts",
                good_hash,
                "ts-0.23-u16-exportspan",
                FACTS_SCHEMA_VERSION,
                &good_payload,
            ),
            // Tampered: claimed content_hash disagrees with the payload's
            // own embedded content_hash for b.ts.
            (
                "b.ts",
                good_hash,
                "ts-0.23-u16-exportspan",
                FACTS_SCHEMA_VERSION,
                &bad_payload,
            ),
        ]);

        let facts = decode_download_response(&body);
        assert_eq!(facts.len(), 2, "both records decode fine on their own");
        ingest_downloaded_facts(repo.path(), &registry, facts);

        // Verify through the same public surface `commands/graph.rs`'s
        // full-repo build consults (`FactsCorpus::get` is crate-private to
        // sem-core) — write the real local files so `merge_with_local`'s own
        // probe pass can read+hash them, then check which paths hit.
        std::fs::write(repo.path().join("a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(repo.path().join("b.ts"), "export const b = 2;\n").unwrap();
        let corpus =
            crate::commands::graph::facts_corpus_for(repo.path(), false).expect("corpus enabled");
        let (merged, stats) = corpus.merge_with_local(
            repo.path(),
            &["a.ts".to_string(), "b.ts".to_string()],
            &registry,
            None,
        );
        assert_eq!(
            stats.hits, 1,
            "only the well-formed a.ts record should have reached the corpus"
        );
        assert!(
            merged.files_contains("a.ts"),
            "the well-formed record must be ingested and reusable"
        );
        assert!(
            !merged.files_contains("b.ts"),
            "the tampered record must never reach the corpus — falls back to extraction"
        );

        std::env::remove_var("SEM_FACTS_CORPUS_DIR");
        std::env::remove_var("SEM_FACTS_CACHE");
        std::env::remove_var("SEM_FACTS_CORPUS");
    }
}
