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
use sem_core::parser::graph::EntityInfo;
use sem_core::parser::registry::ParserRegistry;
use serde::Serialize;


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
                let root = super::repo_root_or_cwd(&cwd);
                if let Some(idx) = open_index(&root) {
                    for m in super::qualified::near_matches(&idx, query, file.as_deref(), 5) {
                        eprintln!("    near: {} {} {}:{}  ({})", m.entity_type.dimmed(), m.qualified_name.bold(), m.file, m.start_line, m.why);
                    }
                }
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
    if answer.defs.is_empty() {
        report_miss(&opts.cwd, &opts.query, opts.file.as_deref(), opts.json, Verb::Callers);
        return;
    }
    let root = super::repo_root_or_cwd(&opts.cwd);
    let idx = open_index(&root);
    // Registrations of one dispatcher (`@dispatch`, `@f.register`,
    // `@overload`) share a name by design; `--file` cannot pick one when they
    // live in one file. They are one callable: answer once, for the group.
    let mut group: Vec<EntityInfo> = Vec::new();
    if answer.defs.len() > 1 && is_dispatch_group(&root, &answer.defs) {
        group = answer.defs.clone();
        let mut seen = std::collections::HashSet::new();
        let merged: Vec<EntityInfo> = answer
            .related
            .iter()
            .flatten()
            .filter(|e| seen.insert(e.id.to_string()))
            .filter(|e| !group.iter().any(|g| g.id == e.id))
            .cloned()
            .collect();
        answer.defs.truncate(1);
        answer.related = vec![merged];
    }
    if answer.defs.len() > 1 {
        refuse_ambiguous(&answer.defs, &opts);
    }

    let verdict = caller_verdict(&root, idx.as_ref(), &answer.defs[0], &answer.related[0], &group);

    let mut hidden = 0;
    if let Some(cap) = limit {
        for related in &mut answer.related {
            if related.len() > cap {
                hidden += related.len() - cap;
                related.truncate(cap);
            }
        }
    }
    if opts.json {
        let def = &answer.defs[0];
        let row = serde_json::json!({
            "entity": to_row(def),
            "related": answer.related[0].iter().map(to_row).collect::<Vec<_>>(),
            "complete": verdict.complete,
            "incomplete_because": verdict.incomplete_because,
            "checked": verdict.checked,
            "possible_callers": verdict.possible_callers,
            "possible_caller_sites": verdict.possible_caller_sites,
            "dispatch_registrations": group.iter().map(to_row).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string(&vec![row]).unwrap_or_default());
        return;
    }
    let def = &answer.defs[0];
    println!("{} {} {}:{}", def.entity_type.dimmed(), def.name.bold(), def.file_path, def.start_line);
    if !group.is_empty() {
        let mut by_file: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
        for g in &group {
            by_file.entry(g.file_path.as_str()).or_default().push(format!("L{}", g.start_line));
        }
        let at: Vec<String> = by_file.iter().map(|(f, ls)| format!("{f} {}", ls.join(","))).collect();
        println!("  one dispatcher, {} registrations ({}); callers below are the dispatcher's", group.len(), at.join("; "));
    }
    let related = &answer.related[0];
    if related.is_empty() {
        if verdict.complete {
            println!("  (callers: none)");
        } else {
            println!("  (callers: none resolved by the static graph; see below: this is NOT a proof of no callers)");
        }
    }
    for row in related {
        println!("  {} {} {}:{}", row.entity_type.dimmed(), row.name, row.file_path, row.start_line);
    }
    if hidden > 0 {
        println!("{}", format!("  … {hidden} more (raise --limit)").dimmed());
    }
    print!("{}", super::completeness::render_text(&verdict, limit.unwrap_or(25), "  "));
}

/// Same-named definitions that are all registrations of one dispatcher:
/// every one carries the same `@dispatch(...)` / `@multimethod` decorator
/// (multipledispatch keys its default namespace by function name), or the
/// same `@X.register` (one singledispatch `X`). Routes, signal receivers and
/// `@overload` stubs are not one callable and are not grouped.
fn is_dispatch_group(root: &Path, defs: &[EntityInfo]) -> bool {
    let name = &defs[0].name;
    if !defs.iter().all(|d| &d.name == name) {
        return false;
    }
    let mut cache: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut keys: Vec<Option<String>> = Vec::new();
    for d in defs {
        let src = cache
            .entry(d.file_path.as_str())
            .or_insert_with(|| std::fs::read_to_string(root.join(&d.file_path)).unwrap_or_default());
        let key = super::completeness::decorators(src, d.start_line).iter().find_map(|dec| dispatch_key(dec));
        keys.push(key);
    }
    keys[0].is_some() && keys.iter().all(|k| k == &keys[0])
}

fn dispatch_key(decorator: &str) -> Option<String> {
    let head = decorator.trim_start_matches('@').split('(').next()?.trim();
    let last = head.rsplit('.').next().unwrap_or(head);
    match last {
        "dispatch" | "multimethod" | "multidispatch" => Some(format!("dispatch:{head}")),
        "register" if head.contains('.') => Some(format!("register:{head}")),
        _ => None,
    }
}

/// The completeness verdict for `def`'s caller set (`resolved`), from the
/// source text of the corpus the index covers. `group`: the other
/// registrations of the same dispatcher, whose spans are not callers.
pub(crate) fn caller_verdict(
    root: &Path,
    idx: Option<&QueryIndex>,
    def: &EntityInfo,
    resolved: &[EntityInfo],
    group: &[EntityInfo],
) -> super::completeness::Verdict {
    use super::completeness as c;
    let def_src = std::fs::read_to_string(root.join(&def.file_path)).unwrap_or_default();
    let mut decorators = c::decorators(&def_src, def.start_line);
    // one entry per registration (two may carry the same decorator text)
    for g in group.iter().filter(|g| g.id != def.id) {
        if let Ok(src) = std::fs::read_to_string(root.join(&g.file_path)) {
            decorators.extend(c::decorators(&src, g.start_line));
        }
    }
    // An index whose text tier predates an edit would hide the edited
    // files' mentions from the scan: use it only while every file is fresh.
    let idx = idx.filter(|idx| !any_content_stale(idx, root));
    let registry = super::create_registry(&root.to_string_lossy());
    let read = |f: &str| std::fs::read_to_string(root.join(f)).unwrap_or_default();
    // Without an index (`SEM_NO_INDEX=1`, a cold path): the same scan over
    // every supported file, entities re-extracted per file on demand.
    let all_files: std::cell::OnceCell<Vec<String>> = std::cell::OnceCell::new();
    let files_containing = |needle: &str| -> Vec<String> {
        let mut files: Vec<String> = match idx {
            Some(idx) => index::grep::search(
                idx,
                root,
                &regex::escape(needle),
                &index::grep::GrepOptions { case_insensitive: false },
                |dir: &Path| super::files::find_supported_files_in_path(root, dir, &registry, &[], false),
            )
            .map(|r| r.hits.into_iter().map(|h| h.file).collect())
            .unwrap_or_default(),
            None => all_files
                .get_or_init(|| super::graph::find_supported_files_with_options(root, &registry, &[], false))
                .iter()
                .filter(|f| read(f).contains(needle))
                .cloned()
                .collect(),
        };
        files.sort();
        files.dedup();
        files
    };
    let extracted: std::cell::RefCell<std::collections::HashMap<String, Vec<EntityInfo>>> = Default::default();
    let enclosing = |file: &str, line: usize| -> Option<c::Enclosing> {
        if let Some(idx) = idx {
            let ents = idx.entities_in_file(file);
            let e = ents.iter().filter(|e| e.start_line() <= line && line <= e.end_line()).min_by_key(|e| e.end_line() - e.start_line())?;
            let owners = super::qualified::index_owners(idx, e.index());
            return Some(c::Enclosing { display: super::qualified::display_name(&owners, e.name()), entity_type: e.entity_type().to_string(), start_line: e.start_line() });
        }
        let mut cache = extracted.borrow_mut();
        let all = cache.entry(file.to_string()).or_insert_with(|| reextract_file(&registry, root, file));
        let e = all.iter().filter(|e| e.start_line <= line && line <= e.end_line).min_by_key(|e| e.end_line - e.start_line)?;
        let owners = super::qualified::list_owners(all, e);
        Some(c::Enclosing { display: super::qualified::display_name(&owners, &e.name), entity_type: e.entity_type.clone(), start_line: e.start_line })
    };
    let owner_name = match idx {
        Some(idx) => idx.entities_in_file(&def.file_path).into_iter().find(|e| e.id() == def.id.as_str()).and_then(|e| {
            super::qualified::index_owners(idx, e.index()).first().map(|s| s.to_string())
        }),
        None => {
            let all = reextract_file(&registry, root, &def.file_path);
            all.iter().find(|e| e.id == def.id).and_then(|e| super::qualified::list_owners(&all, e).first().map(|s| s.to_string()))
        }
    };
    let target = c::Target {
        name: &def.name,
        file: &def.file_path,
        span: (def.start_line, def.end_line),
        decorators,
        owner: owner_name.as_deref(),
    };
    let mut resolved_spans: Vec<(String, usize, usize)> =
        resolved.iter().map(|e| (e.file_path.clone(), e.start_line, e.end_line)).collect();
    resolved_spans.extend(group.iter().map(|g| (g.file_path.clone(), g.start_line, g.end_line)));

    const MAX_FILES: usize = 4000;
    let mut not_checked = None;
    let scan_corpus = |name: &str, not_checked: &mut Option<String>| -> (Vec<c::Mention>, usize) {
        let mut files = files_containing(name);
        if files.len() > MAX_FILES {
            *not_checked = Some(format!("`{name}` appears in {} files; only the first {MAX_FILES} were classified", files.len()));
            files.truncate(MAX_FILES);
        }
        let n = files.len();
        let ms = files.iter().flat_map(|f| c::scan_file(f, &read(f), name)).collect();
        (ms, n)
    };
    let (mentions, files_scanned) = scan_corpus(&def.name, &mut not_checked);
    let alias_mentions = c::alias_hop(&mentions, &def.name, |alias, scope| match scope {
        Some(f) => c::scan_file(f, &read(f), alias),
        None => scan_corpus(alias, &mut None).0,
    });
    let mut attr_files = files_containing("getattr(");
    attr_files.extend(files_containing("hasattr("));
    attr_files.sort();
    attr_files.dedup();
    let dynamic: Vec<(String, String, usize)> = attr_files
        .iter()
        .flat_map(|f| c::dynamic_prefixes(f, &read(f)).into_iter().map(move |(p, l)| (p, f.clone(), l)))
        .collect();
    c::assess(&target, &mentions, &alias_mentions, &resolved_spans, &dynamic, files_scanned, not_checked, enclosing)
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
        let root = super::repo_root_or_cwd(&opts.cwd);
        let idx = open_index(&root);
        for def in defs {
            let shown = idx
                .as_ref()
                .and_then(|idx| {
                    let at = idx.entities_in_file(&def.file_path).into_iter().find(|e| e.id() == def.id.as_str())?.index();
                    Some(super::qualified::display_name(&super::qualified::index_owners(idx, at), &def.name))
                })
                .unwrap_or_else(|| def.name.clone());
            eprintln!(
                "  {} {} {}:{}   (retry: `{}` or `{}@{}`)",
                def.entity_type.dimmed(),
                shown.bold(),
                def.file_path,
                def.start_line,
                shown,
                def.name,
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
        id: e.id.to_string(),
        name: e.name.clone(),
        entity_type: e.entity_type.clone(),
        file: e.file_path.clone(),
        start_line: e.start_line,
        end_line: e.end_line,
    }
}

fn run(opts: QueryOptions, verb: Verb) {
    let answer = resolve(&opts, verb);
    if answer.defs.is_empty() {
        report_miss(&opts.cwd, &opts.query, opts.file.as_deref(), opts.json, verb);
        return;
    }
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
/// Name lookup must also inspect changed files that did not previously match:
/// an edit can introduce a new name. It repairs definition rows locally rather
/// than rebuilding topology. A transitive walk additionally needs fresh edges
/// from the whole corpus, so its gate proves membership and content together.
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

    if verb == Verb::Callers && any_content_stale(idx, root) {
        // A caller added to a file that had no edge to the definition is in
        // neither the CSR row nor the files this answer stats: an edited
        // file anywhere can hold a new caller. Decline to the cold build,
        // which rewrites the index, rather than serve a caller set the edit
        // may have outdated.
        return None;
    }

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
            .flat_map(|path| reextract_matching(&registry, root, path, &opts.query))
            .filter(|e| opts.file.as_deref().is_none_or(|f| super::qualified::in_scope(&e.file_path, f)))
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

    if verb == Verb::Find {
        use rayon::prelude::*;
        // A rename or a new declaration in an existing file is absent from
        // NAMES. Checking only files of existing hits silently misses it.
        // Stat the indexed corpus, but parse only changed files: no topology
        // rebuild or whole-graph serialization is needed for definitions.
        let files = idx.all_file_paths();
        let stale: Vec<_> = files
            .par_iter()
            .copied()
            .filter(|path| opts.file.as_deref().is_none_or(|file| super::qualified::in_scope(path, file)))
            .filter(|path| file_is_stale(idx, root, path))
            .collect();
        defs.retain(|entity| !stale.iter().any(|path| entity.file_path == *path));
        let fresh: Vec<EntityInfo> = stale
            .par_iter()
            .flat_map_iter(|path| reextract_matching(&registry, root, path, &opts.query))
            .collect();
        defs.extend(fresh);
        return Some(Answer {
            defs,
            related: Vec::new(),
        });
    }

    // Definition-side freshness: content-local, repaired in place.
    let def_files: Vec<String> = defs.iter().map(|e| e.file_path.clone()).collect();
    for path in dedup(def_files) {
        if file_is_stale(idx, root, &path) {
            let fresh = reextract_matching(&registry, root, &path, &opts.query);
            defs.retain(|e| e.file_path != path);
            defs.extend(
                fresh
                    .into_iter()
                    .filter(|e| opts.file.as_deref().is_none_or(|f| super::qualified::in_scope(&e.file_path, f))),
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
    // One cold build serves every verb: go through the shared graph cache
    // (full save + a complete index with test flags and byte spans), so the
    // `sem context` / `sem impact` that usually follow a first `sem find`
    // answer from the index instead of paying for a second full build.
    let source_scope = super::graph::cache_source_scope(root, &[], false);
    let (graph, _entities) =
        super::graph::get_or_build_graph(root, &file_paths, &registry, false, source_scope);

    let exact: Vec<EntityInfo> = graph
        .entities
        .values()
        .filter(|e| matches_query(e, &opts.query))
        .filter(|e| opts.file.as_deref().is_none_or(|f| super::qualified::in_scope(&e.file_path, f)))
        .cloned()
        .collect();
    let defs = if exact.is_empty() {
        let q = super::qualified::parse(&opts.query);
        let hits: Vec<EntityInfo> = graph
            .entities
            .values()
            .filter(|e| super::entity_matches_qualified(&graph, e, &opts.query))
            .filter(|e| opts.file.as_deref().is_none_or(|f| super::qualified::in_scope(&e.file_path, f)))
            .cloned()
            .collect();
        super::qualified::narrow_by_line(&q, hits, |e| (e.start_line, e.end_line))
    } else {
        exact
    };

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

    // Self-heal: `get_or_build_graph` above already saved the cache and wrote
    // a complete index, so every subsequent query is served from it.
    Answer { defs, related }
}

fn resolve_defs(idx: &QueryIndex, query: &str, file: Option<&str>) -> Vec<EntityInfo> {
    resolve_by_name_indices(idx, query, file)
        .into_iter()
        .map(|at| idx.entity(at).to_entity_info())
        .collect()
}

/// Re-extract one file and keep the entities the (possibly qualified)
/// query names — the owner chain comes from the same re-extract.
fn reextract_matching(registry: &ParserRegistry, root: &Path, path: &str, query: &str) -> Vec<EntityInfo> {
    let all = reextract_file(registry, root, path);
    let exact: Vec<EntityInfo> = all.iter().filter(|e| super::entity_matches_query(e, query)).cloned().collect();
    if !exact.is_empty() {
        return exact;
    }
    let q = super::qualified::parse(query);
    let hits: Vec<EntityInfo> = all
        .iter()
        .filter(|e| {
            let owners = super::qualified::list_owners(&all, e);
            super::qualified::matches(&q, &e.name, &e.entity_type, &owners, &e.file_path, (e.start_line, e.end_line))
        })
        .cloned()
        .collect();
    super::qualified::narrow_by_line(&q, hits, |e| (e.start_line, e.end_line))
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
        .filter(|e| file.is_none_or(|f| super::qualified::in_scope(e.file_path(), f)))
        .map(index::Entity::index)
        .collect();
    if hits.is_empty() && query.contains("::") {
        if let Some(at) = resolve_by_id_index(idx, query) {
            hits.push(at);
        }
    }
    if hits.is_empty() {
        // `Class.method`, `module.func`, `pkg.mod.Class.method`, `name@line`
        hits = super::qualified::resolve_index(idx, query, file);
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

/// Any indexed file whose content changed since the index was written
/// (a stat each, a read only where the mtime disagrees).
pub(crate) fn any_content_stale(idx: &QueryIndex, root: &Path) -> bool {
    use rayon::prelude::*;
    idx.all_file_paths().par_iter().any(|path| file_is_stale(idx, root, path))
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
            id: (e.id).into(),
            name: e.name,
            entity_type: e.entity_type,
            file_path: e.file_path,
            parent_id: e.parent_id.map(Into::into),
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

/// A miss: say so, then the nearest entities the index knows (same member
/// under another owner, case/punctuation variants, small typos), so a wrong
/// guess costs one retry instead of a switch back to grep. Exit 1.
fn report_miss(cwd: &str, query: &str, file: Option<&str>, json: bool, verb: Verb) {
    if json && verb == Verb::Find {
        // `find --json` keeps its contract: a miss is `[]`, exit 0
        // (pi's sem_find and the transaction server parse it as an array).
        println!("[]");
        return;
    }
    let root = super::repo_root_or_cwd(cwd);
    let near = open_index(&root)
        .map(|idx| super::qualified::near_matches(&idx, query, file, 8))
        .unwrap_or_default();
    if json {
        let out = serde_json::json!({ "resolved": false, "query": query, "near_matches": near });
        println!("{}", serde_json::to_string(&out).unwrap_or_default());
    } else {
        eprintln!("{} no entity named '{}'{}", "error:".red().bold(), query,
            file.map(|f| format!(" in {f}")).unwrap_or_default());
        if near.is_empty() {
            eprintln!("  no near match either; a non-definition (attribute, local, string key) is not an entity: try `sem grep '{}'`",
                super::qualified::parse(query).bare());
        } else {
            eprintln!("  near matches (retry with one of these names, or --file):");
            for m in &near {
                eprintln!("    {} {} {}:{}  ({})", m.entity_type.dimmed(), m.qualified_name.bold(), m.file, m.start_line, m.why);
            }
        }
    }
    std::process::exit(1);
}

fn render(answer: &Answer, verb: Verb, json: bool, query: &str) {
    if answer.defs.is_empty() {
        if json && verb == Verb::Find {
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
