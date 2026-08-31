//! `sem find` / `sem callers` / `sem refs` — query verbs that answer directly
//! from the mmap query index (`sem_core::index`), never the five doomed
//! layers (query-path file discovery, the git
//! freshness oracle, the per-file corpus scan, the SQLite answer-from-SQL
//! fast paths, the resident sidecar). Budget: cold process, <10ms on the
//! monster (per-verb table;).
//!
//! Fallback discipline: index missing,
//! truncated, or salt-mismatched → exactly ONE fallback, the cold build path
//! (`EntityGraph::build` over a fresh file walk), which then writes a fresh
//! index so the *next* call hits. Never the SQLite fast paths, never the
//! sidecar — this module does not import `build_cache::DiskCache` or
//! `commands::sidecar` at all, which is the bypass expressed as an absent
//! dependency rather than a runtime check. `SEM_NO_INDEX=1` forces the same
//! cold-build path unconditionally, for A/B measurement against it.
//!
//! Freshness: after resolving an answer, this module stats exactly
//! the files the answer touches. A stale *definition* file (the entity's own
//! file — content-local, per the "extraction identity" obligation) is
//! repaired in-memory by re-extracting just that one file. A stale *related*
//! file (a caller/ref target's file) is a non-local concern — resolving it
//! correctly needs the two-pass symbol table, not a one-file re-extract — so
//! this module does not attempt a partial patch there; it falls through to
//! the same cold-build path used for a missing index, which is always
//! correct and self-heals the on-disk image as a side effect. Patch choice:
//! repairs are in-memory only for the answer being served now; nothing
//! is appended to an on-disk `index.log` (not implemented by this change — see
//! the change's final report) — the index is instead left to `write_query_index`
//! at the next corpus build, which every `graph`/`diff`/`impact` build and
//! `GraphSession` warm rebuild already triggers (`build_cache.rs`).

use std::path::{Path, PathBuf};

use colored::Colorize;
use sem_core::index::{self, QueryIndex};
use sem_core::parser::graph::{EntityGraph, EntityInfo};
use sem_core::parser::registry::ParserRegistry;
use serde::Serialize;

use crate::build_cache::write_query_index;

pub struct QueryOptions {
    pub cwd: String,
    pub query: String,
    pub file: Option<String>,
    pub json: bool,
}

/// `sem find name1 name2 …` — several lookups in one invocation. Each name
/// resolves independently through the exact same index/fallback machinery as
/// a single `sem find`; a miss on one name never affects the others. Exit
/// code is 1 only when *every* name missed (a batch where anything resolved
/// is a success, matching the per-name independence contract).
pub fn find_multi_command(cwd: String, queries: Vec<String>, file: Option<String>, json: bool) {
    let mut answers: Vec<(String, Answer)> = Vec::with_capacity(queries.len());
    for query in queries {
        let opts = QueryOptions {
            cwd: cwd.clone(),
            query: query.clone(),
            file: file.clone(),
            json,
        };
        let answer = resolve(&opts, Verb::Find);
        answers.push((query, answer));
    }

    let any_hit = answers.iter().any(|(_, a)| !a.defs.is_empty());
    if json {
        let rows: Vec<serde_json::Value> = answers
            .iter()
            .map(|(query, answer)| {
                serde_json::json!({
                    "query": query,
                    "matches": answer.defs.iter().map(to_row).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows).unwrap_or_default());
    } else {
        for (query, answer) in &answers {
            if answer.defs.is_empty() {
                eprintln!("{} no entity named '{}'", "error:".red().bold(), query);
                continue;
            }
            for def in &answer.defs {
                println!(
                    "{} {} {}:{}",
                    def.entity_type.dimmed(),
                    def.name.bold(),
                    def.file_path,
                    def.start_line
                );
            }
        }
    }
    if !any_hit {
        std::process::exit(1);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Verb {
    Find,
    Callers,
    Refs,
}

pub fn find_command(opts: QueryOptions) {
    run(opts, Verb::Find);
}

/// `sem callers` answers about exactly one entity, so an ambiguous name is
/// refused with the full candidate list rather than answered many times over
/// (`refs` keeps the old show-everything behavior — its answer reads fine
/// per-def; a caller list only means something once you know *whose* callers
/// it is). `limit` caps the caller rows shown; text mode says how many were
/// held back, json is just the capped list the caller asked for.
pub fn callers_command(opts: QueryOptions, limit: Option<usize>) {
    let mut answer = resolve(&opts, Verb::Callers);
    if answer.defs.len() > 1 {
        refuse_ambiguous(&answer.defs, &opts);
    }

    let mut hidden = 0;
    if let Some(cap) = limit {
        for related in &mut answer.related {
            if related.len() > cap {
                hidden += related.len() - cap;
                related.truncate(cap);
            }
        }
    }
    render(&answer, Verb::Callers, opts.json, &opts.query);
    if hidden > 0 && !opts.json {
        println!("{}", format!("  … {hidden} more (raise --limit)").dimmed());
    }
}

/// The callers-verb refusal: every candidate definition listed, exit 1.
/// Text mode reports on stderr like the no-match case; json emits
/// `{"resolved": false, "candidates": [...]}` so an agent can pick a file
/// and retry without re-running discovery.
fn refuse_ambiguous(defs: &[EntityInfo], opts: &QueryOptions) -> ! {
    if opts.json {
        let out = serde_json::json!({
            "resolved": false,
            "candidates": defs.iter().map(to_row).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string(&out).unwrap_or_default());
    } else {
        eprintln!(
            "{} '{}' matches {} definitions; pass --file (or a \"type name\" query) to pick one:",
            "error:".red().bold(),
            opts.query,
            defs.len()
        );
        for def in defs {
            eprintln!(
                "  {} {} {}:{}",
                def.entity_type.dimmed(),
                def.name.bold(),
                def.file_path,
                def.start_line
            );
        }
    }
    std::process::exit(1);
}

pub fn refs_command(opts: QueryOptions) {
    run(opts, Verb::Refs);
}

#[derive(Serialize)]
struct DefRow {
    id: String,
    name: String,
    #[serde(rename = "type")]
    entity_type: String,
    file: String,
    start_line: usize,
    end_line: usize,
}

#[derive(Serialize)]
struct RelatedRow {
    entity: DefRow,
    related: Vec<DefRow>,
}

fn to_row(e: &EntityInfo) -> DefRow {
    DefRow {
        id: e.id.clone(),
        name: e.name.clone(),
        entity_type: e.entity_type.clone(),
        file: e.file_path.clone(),
        start_line: e.start_line,
        end_line: e.end_line,
    }
}

fn run(opts: QueryOptions, verb: Verb) {
    let answer = resolve(&opts, verb);
    render(&answer, verb, opts.json, &opts.query);
}

/// Resolution half of `run`, shared with the multi-query form: index answer
/// when fresh, cold build otherwise — byte-identical behavior to what `run`
/// always did, just separated from rendering.
fn resolve(opts: &QueryOptions, verb: Verb) -> Answer {
    let root = super::repo_root_or_cwd(&opts.cwd);

    if std::env::var_os("SEM_NO_INDEX").is_some() {
        cold_build_answer(&root, opts, verb)
    } else {
        match index::QueryIndex::open(&index_path(&root)) {
            Some(idx) => match index_answer(&idx, &root, opts, verb) {
                Some(answer) => answer,
                None => cold_build_answer(&root, opts, verb),
            },
            None => cold_build_answer(&root, opts, verb),
        }
    }
}

fn index_path(root: &Path) -> PathBuf {
    sem_mcp::cache::cache_dir_for_repo(root)
        .map(|dir| dir.join(index::INDEX_FILE_NAME))
        .unwrap_or_else(|| root.join(".sem-missing-cache-dir"))
}

/// Open the repo's index, honoring `SEM_NO_INDEX=1`. Shared by the other
/// reroutes (`commands::entities`'s single-file path, `commands::impact`'s
/// entity-scoped Deps fast path) so every index-backed query verb agrees on
/// what "the index" means and how to opt out of it.
pub(crate) fn open_index(root: &Path) -> Option<QueryIndex> {
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return None;
    }
    QueryIndex::open(&index_path(root))
}

/// Verified freshness for one file: `true` means the index's stored
/// fingerprint no longer matches disk (content-hash-confirmed, not just
/// mtime) or the index has never seen this file at all.
pub(crate) fn is_file_stale(idx: &QueryIndex, root: &Path, path: &str) -> bool {
    file_is_stale(idx, root, path)
}

/// Whole-corpus freshness, proven from the index alone — the index-side
/// analogue of `cache::DiskCache::has_fresh_cache`, and the gate every
/// *corpus-shaped* index answer needs (: `sem impact --all/--tests`'
/// transitive walk, `sem graph`'s whole-repo dump, `sem context`'s subgraph).
///
/// The entity-scoped verbs (`find`/`callers`/`refs`/`impact --deps`) get away
/// with proving only the files their own answer touches, because an edit
/// anywhere else cannot change *their* answer. A transitive walk has no such
/// boundary: an edit in a file the walk never visits can add an edge *into*
/// the closure, so the only honest gate is the one the SQL path already
/// used — the whole corpus, membership and content both.
///
/// - membership: `index::complete_check` (`Complete` tier), which needs
///   `DIRS`; an image without it is refused outright rather than trusted,
///   because `complete_check` over an empty `DIRS` finds no drifted
///   directories and would report a *false* clean.
/// - content: `file_is_stale` over every file the image knows, in parallel —
///   a stat each, and a read only where an mtime already disagrees.
///
/// Both halves run as sibling `rayon` tasks for the same reason
/// `index_answer` runs its two: they touch disjoint sections and disjoint
/// stat sets, so the wall time is the max rather than the sum.
pub(crate) fn corpus_is_fresh(idx: &QueryIndex, root: &Path, cwd: &str) -> bool {
    if !idx.has_dirs() {
        return false;
    }
    let registry = super::create_registry(cwd);
    let (complete, content_fresh) = rayon::join(
        || {
            index::complete_check(idx, root, |dir: &Path| {
                super::files::find_supported_files_in_path(root, dir, &registry, &[], false)
            })
        },
        || {
            use rayon::prelude::*;
            let files = idx.all_file_paths();
            !files.par_iter().any(|path| file_is_stale(idx, root, path))
        },
    );
    complete.is_clean() && content_fresh
}

struct Answer {
    defs: Vec<EntityInfo>,
    /// One related-set per resolved def, in the same order as `defs`
    /// (only populated for `Callers`/`Refs`).
    related: Vec<Vec<EntityInfo>>,
}

/// Resolve `opts.query` against the index, repair any content-staleness the
/// answer touches, and prove membership-freshness against the whole
/// corpus (`Complete` tier) — closing the blind spot where a
/// name that exists only in a brand-new file was invisible until the next
/// full build. `None` means "cannot answer from the index as-is": either the
/// existing non-local (related-file) content staleness the verified path
/// already declines on, or the same decline extended to *any* membership
/// change touching `Callers`/`Refs` (a new file could be a new edge, which
/// this change cannot patch into `REFS`'s CSR without the two-pass symbol
/// table it deliberately doesn't have — the same limitation applied to
/// membership instead of content), or an inconclusive sweep (fail-toward-MISS: an
/// unresolved race is treated as "prove it the slow way", never as "assume
/// nothing changed"). Every case falls through to `cold_build_answer`, which
/// is always correct because it walks the corpus fresh.
///
/// The membership sweep (`index::complete_check`) runs **concurrently** with
/// the existing verified-answer resolution (`rayon::join`) — they are
/// independent (one only touches `ENTITIES`/`NAMES`/`REFS`, the other only
/// touches `FILES`/`DIRS` plus, on a hit, one bounded directory re-walk), so
/// the answer materializes on one thread while the sweep confirms corpus
/// membership on another, and the verb's wall time is `max` of the two, not
/// their sum.
fn index_answer(idx: &QueryIndex, root: &Path, opts: &QueryOptions, verb: Verb) -> Option<Answer> {
    let registry = super::create_registry(&opts.cwd);
    let (primary, complete) = rayon::join(
        || index_answer_verified(idx, root, opts, verb),
        || {
            index::complete_check(idx, root, |dir: &Path| {
                super::files::find_supported_files_in_path(root, dir, &registry, &[], false)
            })
        },
    );

    if complete.inconclusive {
        return None;
    }

    let mut answer = primary?;

    if !complete.new_files.is_empty() {
        if verb != Verb::Find {
            // A new file might introduce an edge this fast path has no way
            // to represent (REFS is a CSR built at the last full build; a
            // new file was never in it). Decline rather than risk serving a
            // caller/ref set that's silently missing the new file's edges.
            return None;
        }
        let matched: Vec<EntityInfo> = complete
            .new_files
            .iter()
            .flat_map(|path| reextract_file(&registry, root, path))
            .filter(|e| matches_query(e, &opts.query))
            .filter(|e| opts.file.as_deref().is_none_or(|f| e.file_path == f))
            .collect();
        answer.defs.extend(matched);
    }

    Some(answer)
}

/// The prior verified-only answer: content-freshness only, no
/// membership proof. Kept as its own function so `index_answer` can run it
/// concurrently with the `Complete` sweep rather than serially after it.
fn index_answer_verified(
    idx: &QueryIndex,
    root: &Path,
    opts: &QueryOptions,
    verb: Verb,
) -> Option<Answer> {
    let registry = super::create_registry(&opts.cwd);
    let mut defs = resolve_defs(idx, &opts.query, opts.file.as_deref());

    // Definition-side freshness: content-local, repaired in place.
    let def_files: Vec<String> = defs.iter().map(|e| e.file_path.clone()).collect();
    for path in dedup(def_files) {
        if file_is_stale(idx, root, &path) {
            let fresh = reextract_file(&registry, root, &path);
            defs.retain(|e| e.file_path != path);
            defs.extend(
                fresh
                    .into_iter()
                    .filter(|e| matches_query(e, &opts.query))
                    .filter(|e| opts.file.as_deref().is_none_or(|f| e.file_path == f)),
            );
        }
    }

    if verb == Verb::Find {
        return Some(Answer {
            defs,
            related: Vec::new(),
        });
    }

    let mut related = Vec::with_capacity(defs.len());
    for def in &defs {
        let Some(hit) = idx.lookup(&def.name).into_iter().find(|e| e.id() == def.id) else {
            // The def was just repaired in-memory and isn't in the index's
            // NAMES table under its (possibly new) identity — its related
            // set can't be served from CSR postings computed for the old
            // content. Non-local; hand off to a full rebuild.
            return None;
        };
        let rows: Vec<EntityInfo> = match verb {
            Verb::Callers => idx.callers_of(hit.index()),
            Verb::Refs => idx.refs_of(hit.index()),
            Verb::Find => unreachable!(),
        }
        .iter()
        .map(index::Entity::to_entity_info)
        .collect();

        // Related-side freshness: a stale caller/ref target's file means the
        // CSR row for `def` may itself be wrong (the edit could have added or
        // removed a call), which a per-file re-extract of the *target* can't
        // fix — that requires re-resolving `def`'s own references. Bail to
        // the cold-build fallback rather than serve a possibly-wrong edge.
        for path in dedup(rows.iter().map(|e| e.file_path.clone()).collect()) {
            if file_is_stale(idx, root, &path) {
                return None;
            }
        }
        related.push(rows);
    }

    Some(Answer { defs, related })
}

fn cold_build_answer(root: &Path, opts: &QueryOptions, verb: Verb) -> Answer {
    let registry = super::create_registry(&opts.cwd);
    let file_paths = super::graph::find_supported_files_with_options(root, &registry, &[], false);
    let (graph, _entities) = EntityGraph::build(root, &file_paths, &registry);

    let defs: Vec<EntityInfo> = graph
        .entities
        .values()
        .filter(|e| matches_query(e, &opts.query))
        .filter(|e| opts.file.as_deref().is_none_or(|f| e.file_path == f))
        .cloned()
        .collect();

    let related = if verb == Verb::Find {
        Vec::new()
    } else {
        defs.iter()
            .map(|def| {
                let map = match verb {
                    Verb::Callers => graph.dependents(),
                    Verb::Refs => graph.dependencies(),
                    Verb::Find => unreachable!(),
                };
                map.get(def.id.as_str())
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| graph.entities.get(id).cloned())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect()
    };

    // Self-heal: a repo that reaches this fallback now has a fresh index for
    // every subsequent query (the change's item 4).
    // `None` for the test classification: this path builds topology only and
    // has no `SemanticEntity` bodies, which `is_test_entity` needs. The image
    // it writes therefore carries no `FLAG_ENTITY_TESTS`, and `sem impact
    // --tests`/`--all`'s index fast path declines on it rather than reading
    // an uncomputed field as "no tests". Same reasoning for `None`
    // byte spans: no bodies in scope, so `sem context`'s index
    // reroute declines on this image exactly like the test-flag readers do.
    write_query_index(root, &file_paths, &graph, None, None, None);

    Answer { defs, related }
}

fn resolve_defs(idx: &QueryIndex, query: &str, file: Option<&str>) -> Vec<EntityInfo> {
    resolve_by_name_indices(idx, query, file)
        .into_iter()
        .map(|at| idx.entity(at).to_entity_info())
        .collect()
}

fn matches_query(e: &EntityInfo, query: &str) -> bool {
    super::entity_matches_query(e, query)
}

/// Name/"type name" resolution, as entity-table indices rather than owned
/// `EntityInfo` — what `commands::impact`'s Deps fast path needs, since it
/// has to chain the result into `refs_of`/`callers_of` without re-deriving
/// the index position. `resolve_defs` above is this plus materialization,
/// for callers (`find`/`callers`/`refs`) that only need the owned rows.
pub(crate) fn resolve_by_name_indices(
    idx: &QueryIndex,
    query: &str,
    file: Option<&str>,
) -> Vec<usize> {
    let (name, want_type) = match query.split_once(' ') {
        Some((t, n)) if !t.is_empty() && !n.is_empty() => (n, Some(t)),
        _ => (query, None),
    };
    let mut hits: Vec<usize> = idx
        .lookup(name)
        .iter()
        .filter(|e| want_type.is_none_or(|t| e.entity_type() == t))
        .filter(|e| file.is_none_or(|f| e.file_path() == f))
        .map(index::Entity::index)
        .collect();
    if hits.is_empty() && query.contains("::") {
        if let Some(at) = resolve_by_id_index(idx, query) {
            hits.push(at);
        }
    }
    hits
}

/// Reconstruct the file path an id was minted under by trying each `"::"`
/// prefix as a candidate, shortest first, and confirming against `FILES`
/// (`QueryIndex::file_fingerprint`, an exact-match binary search — never a
/// guess left unconfirmed). Needed because the index has no id→entity
/// section (`NAMES` tier is name-keyed only); this is
/// the CLI-side substitute, correct by construction because a false-positive
/// candidate simply fails the final id equality check and the search moves
/// on to the next `"::"` boundary.
pub(crate) fn resolve_by_id_index(idx: &QueryIndex, id: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = id[search_from..].find("::") {
        let split_at = search_from + rel;
        let candidate = &id[..split_at];
        if !candidate.is_empty() && idx.file_fingerprint(candidate).is_some() {
            if let Some(hit) = idx
                .entities_in_file(candidate)
                .into_iter()
                .find(|e| e.id() == id)
            {
                return Some(hit.index());
            }
        }
        search_from = split_at + 2;
        if search_from >= id.len() {
            break;
        }
    }
    None
}

fn file_is_stale(idx: &QueryIndex, root: &Path, path: &str) -> bool {
    let Some(fp) = idx.file_fingerprint(path) else {
        return true;
    };
    let full = root.join(path);
    let Some((secs, nanos)) = sem_mcp::cache::file_mtime_parts(&full) else {
        return true; // deleted or unreadable
    };
    if secs == fp.mtime_secs && nanos as u32 == fp.mtime_nanos {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(&full) else {
        return true;
    };
    sem_core::parser::incremental::content_hash(&content) != fp.content_hash
}

fn reextract_file(registry: &ParserRegistry, root: &Path, path: &str) -> Vec<EntityInfo> {
    let Ok(content) = std::fs::read_to_string(root.join(path)) else {
        return Vec::new();
    };
    registry
        .extract_entities_brief(path, &content)
        .into_iter()
        .map(|e| EntityInfo {
            id: e.id,
            name: e.name,
            entity_type: e.entity_type,
            file_path: e.file_path,
            parent_id: e.parent_id,
            start_line: e.start_line,
            end_line: e.end_line,
        })
        .collect()
}

fn dedup(mut paths: Vec<String>) -> Vec<String> {
    paths.sort_unstable();
    paths.dedup();
    paths
}

fn render(answer: &Answer, verb: Verb, json: bool, query: &str) {
    if answer.defs.is_empty() {
        if json {
            println!("[]");
        } else {
            eprintln!("{} no entity named '{}'", "error:".red().bold(), query);
            std::process::exit(1);
        }
        return;
    }

    if verb == Verb::Find {
        if json {
            let rows: Vec<DefRow> = answer.defs.iter().map(to_row).collect();
            println!("{}", serde_json::to_string(&rows).unwrap_or_default());
        } else {
            for def in &answer.defs {
                println!(
                    "{} {} {}:{}",
                    def.entity_type.dimmed(),
                    def.name.bold(),
                    def.file_path,
                    def.start_line
                );
            }
        }
        return;
    }

    let label = if verb == Verb::Callers {
        "callers"
    } else {
        "refs"
    };
    if json {
        let rows: Vec<RelatedRow> = answer
            .defs
            .iter()
            .zip(&answer.related)
            .map(|(def, related)| RelatedRow {
                entity: to_row(def),
                related: related.iter().map(to_row).collect(),
            })
            .collect();
        println!("{}", serde_json::to_string(&rows).unwrap_or_default());
        return;
    }
    for (def, related) in answer.defs.iter().zip(&answer.related) {
        println!(
            "{} {} {}:{}",
            def.entity_type.dimmed(),
            def.name.bold(),
            def.file_path,
            def.start_line
        );
        if related.is_empty() {
            println!("  ({label}: none)");
        }
        for row in related {
            println!(
                "  {} {} {}:{}",
                row.entity_type.dimmed(),
                row.name,
                row.file_path,
                row.start_line
            );
        }
    }
}
