use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::index::QueryIndex;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::context::build_context_result_bounded;
use sem_core::parser::graph::{EntityGraph, EntityInfoMap, EntityRef, RefType};

pub struct ContextOptions {
    pub cwd: String,
    pub entity_name: Option<String>,
    pub entity_id: Option<String>,
    pub file_path: Option<String>,
    pub budget: usize,
    /// Bound transitive related entities to this many graph hops (0 = unbounded).
    pub hops: usize,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

pub fn context_command(opts: ContextOptions) {
    let started = std::time::Instant::now();
    let mut timings = crate::timings::Timings::from_env("context");
    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(root, &ext_filter, opts.no_default_excludes);

    let file_path = opts
        .file_path
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), root, file));

    // The git-oracle subgraph fast path that used to sit here (deleted
    // semx-zvq, QUERY-INDEX.md §12.3) answered *differently* from the tier
    // it fronted — a bug, not a latency tier, so its removal left `sem
    // context` with no index tier at all rather than a repaired one. The
    // missing datum was precise: `EntityRec` carried `start_line`/`end_line`,
    // not the byte span `SemanticEntity.content` is sliced from, so the
    // image could not reconstruct an entity's body. semx-a3w adds that span
    // (`FORMAT_VERSION` 3) and reroutes here — `try_index_context` below —
    // onto index lookup + file-read-at-span, gated on the same whole-corpus
    // `corpus_is_fresh` proof the other transitive-closure reroutes
    // (`impact --all/--tests`, `sem graph`) use, and calling the *exact
    // same* `build_context_result_bounded`/`render_context` the authoritative
    // path calls below — so any case this reroute cannot answer with proven
    // confidence (an ambiguous/qualified/unresolved name, a stale or
    // membership-incomplete corpus, an entity whose byte span is absent) just
    // declines, never approximates, straight into the unchanged walk.
    if !opts.no_cache && try_index_context(&opts, &mut timings) {
        timings.finish();
        return;
    }

    // Cloud is a capability — cross-repo xref, repos with no local index/cache
    // (QUERY-INDEX.md §7 item 7) — not a latency tier, so it must not race a
    // local answer over the network; it only gets a turn once the index
    // reroute above has already declined.
    if super::cloud::try_cloud_context(&opts).is_some() {
        return;
    }

    let (graph, all_entities) = {
        let file_paths = super::graph::find_supported_files_with_options(
            root,
            &registry,
            &ext_filter,
            opts.no_default_excludes,
        );
        timings.mark("file_discovery");
        let prog = crate::progress::Progress::start_staged();
        let (graph, all_entities) = super::graph::get_or_build_graph(
            root,
            &file_paths,
            &registry,
            opts.no_cache,
            source_scope,
        );
        timings.mark("full_graph_build");
        prog.done(&format!(
            "{} entities, {} files",
            super::graph::fmt_count(graph.entities.len()),
            super::graph::fmt_count(file_paths.len())
        ));
        (graph, all_entities)
    };

    let entity = find_entity(
        &graph,
        opts.entity_name.as_deref(),
        opts.entity_id.as_deref(),
        file_path.as_deref(),
    );
    let context_result =
        build_context_result_bounded(&graph, &entity.id, &all_entities, opts.budget, opts.hops);
    timings.mark("cli_output_serialization");

    render_context(
        &entity.name,
        &entity.entity_type,
        &entity.id,
        opts.json,
        opts.budget,
        &context_result,
    );
    timings.finish();
    super::consent::maybe_cloud_tip(&opts.cwd, started.elapsed());
}

/// The output half of `context_command`, shared verbatim by the index
/// reroute (`try_index_context`, semx-a3w) and this always-correct walk —
/// one render function is what makes the two byte-identical by
/// construction rather than by two hand-kept-in-sync copies.
fn render_context(
    entity_name: &str,
    entity_type: &str,
    entity_id: &str,
    json: bool,
    budget: usize,
    context_result: &sem_core::parser::context::ContextResult,
) {
    if json {
        let output = serde_json::json!({
            "entity": entity_name,
            "entityId": entity_id,
            "budget": budget,
            "total_tokens": context_result.total_tokens,
            "truncated": context_result.truncated,
            "target_omitted": context_result.target_omitted,
            "omitted": context_result.omitted.iter().map(|t| serde_json::json!({
                "role": t.role,
                "entities": t.entities,
                "tests": t.tests,
            })).collect::<Vec<_>>(),
            "entries": context_result.entries.iter().map(|e| serde_json::json!({
                "entityId": e.entity_id,
                "name": e.entity_name,
                "type": e.entity_type,
                "file": e.file_path,
                "role": e.role,
                "tokens": e.estimated_tokens,
                "content": e.content,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        println!(
            "{} {} {} (budget: {}, used: {})\n",
            "context for".green().bold(),
            entity_type.dimmed(),
            entity_name.bold(),
            budget,
            context_result.total_tokens,
        );

        if context_result.target_omitted {
            println!(
                "  {}",
                "target omitted: signature exceeds token budget".dimmed()
            );
        }

        let mut current_role = String::new();
        for entry in &context_result.entries {
            if entry.role != current_role {
                current_role = entry.role.clone();
                let role_label = match current_role.as_str() {
                    "target" => "target".green().bold(),
                    "direct_dependency" => "direct dependencies".cyan().bold(),
                    "direct_dependent" => "direct dependents".yellow().bold(),
                    "transitive_dependency" => "transitive dependencies".blue().bold(),
                    "transitive_dependent" => "transitive dependents".dimmed().bold(),
                    _ => current_role.normal().bold(),
                };
                println!("  {}:", role_label);
            }

            println!(
                "    {} {} ({}, ~{} tokens)",
                entry.entity_type.dimmed(),
                entry.entity_name.bold(),
                entry.file_path.dimmed(),
                entry.estimated_tokens,
            );
            // The target is what you asked to read: print its full body. Related
            // entities stay a one-line signature so the context map stays scannable.
            if entry.role == "target" {
                for line in entry.content.lines() {
                    println!("      {line}");
                }
            } else {
                let snippet = entry.content.lines().next().unwrap_or("");
                if !snippet.is_empty() {
                    println!("      {}", snippet.dimmed());
                }
            }
        }

        if !context_result.omitted.is_empty() {
            let parts: Vec<String> = context_result
                .omitted
                .iter()
                .map(|t| {
                    let role = pluralize_role(&t.role);
                    if t.tests > 0 {
                        format!("+{} {} ({} tests)", t.entities, role, t.tests)
                    } else {
                        format!("+{} {}", t.entities, role)
                    }
                })
                .collect();
            println!(
                "\n  {}",
                format!("not packed: {} · sem impact lists them", parts.join(" · ")).dimmed()
            );
        }
    }
}

/// `parser::context::collect_reachable_related`'s own cap, duplicated here
/// (not imported — that constant is private, deliberately: it's an
/// implementation bound of the packer, not a public contract). Kept equal on
/// purpose: this reroute's own BFS below must visit *at least* as many nodes
/// as the packer's unbounded (`max_hops == 0`) walk ever could, so that the
/// subgraph handed to `build_context_result_bounded` is always a superset —
/// up to this same cap — of whatever the packer would touch, for *any*
/// `opts.hops`. A superset only adds unreached nodes/edges the packer's own
/// hop/budget bounding already ignores; it can never remove a node the
/// packer needs, which is what byte-identical output requires.
const MAX_VISITED: usize = 10_000;

/// Index-backed fast path for `sem context` (semx-a3w, QUERY-INDEX.md's
/// missing-datum bead following semx-zvq §12.3). Never approximates: every
/// branch that cannot *prove* its answer matches the authoritative
/// `build_context_result_bounded` walk returns `false` and the caller falls
/// straight into that walk, unchanged.
///
/// Strategy: resolve the target to one unambiguous index entity, prove the
/// whole corpus fresh (membership + content — a transitive closure has no
/// locality boundary, same reasoning `query::corpus_is_fresh`'s doc gives for
/// `impact --all/--tests` and `sem graph`), walk the `REFS` CSR in both
/// directions from the target up to [`MAX_VISITED`] to assemble a subgraph
/// that is provably a superset of whatever the packer could ever touch for
/// this target, read each of those entities' content by slicing its own file
/// at its indexed byte span (semx-a3w's new datum), and then hand that
/// subgraph plus real bodies to the **exact same**
/// `build_context_result_bounded`/`render_context` the authoritative path
/// calls — so the only way this reroute's output could differ from the
/// authoritative one is if the subgraph or the bodies are wrong, not because
/// any packing/rendering logic is duplicated.
fn try_index_context(opts: &ContextOptions, timings: &mut crate::timings::Timings) -> bool {
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return false;
    }
    let root = super::repo_root_or_cwd(&opts.cwd);
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(&root, &ext_filter, opts.no_default_excludes);
    if !matches!(source_scope, sem_mcp::cache::CacheSourceScope::Default) {
        return false;
    }
    let file_hint = opts
        .file_path
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), &root, file));
    let Some(idx) = super::query::open_index(&root) else {
        return false;
    };
    if !idx.has_refs() {
        return false;
    }

    let at = if let Some(id) = opts.entity_id.as_deref() {
        super::query::resolve_by_id_index(&idx, id)
    } else {
        let Some(name) = opts.entity_name.as_deref() else {
            return false;
        };
        // `find_entity` matches via `entity_matches_qualified`, which accepts
        // `Parent.child`/`Parent::child` addressing; `resolve_by_name_indices`
        // does not. Same decline `impact.rs`'s transitive reroute makes for
        // the identical reason: racing two resolvers can disagree, and a
        // decline is only ever slower, never wrong.
        if name.contains('.') || name.contains("::") {
            return false;
        }
        let candidates = super::query::resolve_by_name_indices(&idx, name, file_hint.as_deref());
        if candidates.len() != 1 {
            return false; // not found or ambiguous: legacy path reports it correctly
        }
        Some(candidates[0])
    };
    let Some(at) = at else {
        return false;
    };

    // Whole-corpus freshness (membership + content), not just the files this
    // particular closure touches: an edit in a file the walk never visits
    // can still add an edge *into* the closure the moment it's built, and a
    // brand-new file can be a new edge this CSR (built at the last index
    // write) has no way to represent — the identical reasoning
    // `query::corpus_is_fresh`'s own doc gives for `impact --all/--tests`
    // and `sem graph`, reused verbatim rather than re-derived.
    if !super::query::corpus_is_fresh(&idx, &root, &opts.cwd) {
        return false;
    }
    timings.mark("index_context_corpus_fresh");

    let Some((entities, edges)) = collect_subgraph(&idx, at) else {
        // Some entity in the closure has no byte span recorded — decline
        // rather than degrade (see `format::entity`'s doc on why a one-sided
        // or absent span can't be worked around at read time).
        return false;
    };
    timings.mark("index_context_bfs");
    timings.counter("index_context_closure_entities", entities.len() as u64);

    let Some(all_entities) = hydrate_contents(&root, &idx, &entities) else {
        // A file read or UTF-8 decode failed (e.g. a race with an edit that
        // `corpus_is_fresh` didn't catch, or a non-UTF-8 file slipping past
        // the writer's own `read_to_string` filter some other way). Decline;
        // the legacy path re-reads and re-extracts fresh.
        return false;
    };
    timings.mark("index_context_hydrate");

    let entity_infos: EntityInfoMap = entities
        .iter()
        .map(|&i| {
            let e = idx.entity(i).to_entity_info();
            (e.id.clone(), e)
        })
        .collect();
    let graph = EntityGraph::from_parts(entity_infos, edges);
    timings.mark("index_context_graph_build");

    let target = idx.entity(at);
    let target_id = target.id();
    let target_name = target.name().to_string();
    let target_type = target.entity_type().to_string();

    let context_result =
        build_context_result_bounded(&graph, &target_id, &all_entities, opts.budget, opts.hops);
    timings.mark("index_context_pack");
    render_context(
        &target_name,
        &target_type,
        &target_id,
        opts.json,
        opts.budget,
        &context_result,
    );
    true
}

/// Both directions' `REFS`-CSR walk from `at`, capped at [`MAX_VISITED`] —
/// mirrors `parser::context::collect_reachable_related`'s exact algorithm
/// (FIFO queue, visited set, hard cap) so that when the *real* packer later
/// runs that same algorithm against the `EntityGraph` built from this
/// function's output, it is walking data this function already proved
/// complete up to the identical cap. Returns `None` the instant any visited
/// entity has no byte span — the one condition this reroute cannot recover
/// from short of declining outright.
fn collect_subgraph(idx: &QueryIndex, at: usize) -> Option<(Vec<usize>, Vec<EntityRef>)> {
    idx.entity(at).byte_span()?;
    let mut all: Vec<usize> = vec![at];
    let mut all_set: HashSet<usize> = HashSet::from([at]);
    let mut edges: Vec<EntityRef> = Vec::new();
    // The forward and reverse walks start from the same node and can both
    // reach the same directed edge when the corpus has a cycle (mutual
    // recursion, circular imports): the forward pass records it as an
    // outgoing edge of some visited `P`, the reverse pass records the same
    // edge as an incoming edge of some visited `Q`. `graph.edges` in the
    // authoritative build has each logical edge exactly once, so this set
    // dedupes on `(from, to, ref_type)` before the edges are ever pushed —
    // `EntityRef`/`RefType` derive neither `Eq` nor `Hash`, hence the tuple
    // key rather than the struct itself.
    let mut seen_edges: HashSet<(String, String, u8)> = HashSet::new();
    let ref_type_key = |rt: &RefType| -> u8 {
        match rt {
            RefType::Calls => 0,
            RefType::Imports => 1,
            RefType::TypeRef => 2,
        }
    };

    for forward in [true, false] {
        let mut visited: HashSet<usize> = HashSet::from([at]);
        let mut queue: VecDeque<usize> = VecDeque::from([at]);
        let mut collected = 0usize;

        while let Some(current) = queue.pop_front() {
            if collected >= MAX_VISITED {
                break;
            }
            let neighbors = if forward {
                idx.refs_of_typed(current)
            } else {
                idx.callers_of_typed(current)
            };
            for (entity, ref_type) in neighbors {
                let n = entity.index();
                let (from_id, to_id) = if forward {
                    (idx.entity(current).id(), idx.entity(n).id())
                } else {
                    (idx.entity(n).id(), idx.entity(current).id())
                };
                if seen_edges.insert((from_id.clone(), to_id.clone(), ref_type_key(&ref_type))) {
                    edges.push(EntityRef {
                        from_entity: from_id,
                        to_entity: to_id,
                        ref_type,
                    });
                }
                if visited.insert(n) {
                    entity.byte_span()?;
                    if all_set.insert(n) {
                        all.push(n);
                    }
                    collected += 1;
                    if collected >= MAX_VISITED {
                        break;
                    }
                    queue.push_back(n);
                }
            }
        }
    }

    Some((all, edges))
}

/// File-read-at-span (semx-a3w's new datum), grouped by file so a file with
/// several collected entities is read once. `None` on any I/O or UTF-8
/// failure — the caller declines the whole reroute rather than serve a
/// partial/incorrect body, the same all-or-nothing discipline
/// `collect_subgraph` applies to missing byte spans.
fn hydrate_contents(
    root: &Path,
    idx: &QueryIndex,
    indices: &[usize],
) -> Option<Vec<SemanticEntity>> {
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for &i in indices {
        by_file
            .entry(idx.entity(i).file_path())
            .or_default()
            .push(i);
    }

    let mut out = Vec::with_capacity(indices.len());
    for (file_path, members) in by_file {
        let bytes = std::fs::read(root.join(file_path)).ok()?;
        for &i in &members {
            let e = idx.entity(i);
            let (start, end) = e.byte_span()?;
            let slice = bytes.get(start..end)?;
            let content = std::str::from_utf8(slice).ok()?.to_string();
            let info = e.to_entity_info();
            out.push(SemanticEntity {
                id: info.id,
                file_path: info.file_path,
                entity_type: info.entity_type,
                name: info.name,
                parent_id: info.parent_id,
                content,
                content_hash: String::new(),
                structural_hash: None,
                kappa: None,
                start_line: info.start_line,
                end_line: info.end_line,
                start_byte: Some(start),
                end_byte: Some(end),
                metadata: None,
            });
        }
    }
    Some(out)
}

fn find_entity<'a>(
    graph: &'a EntityGraph,
    name: Option<&str>,
    entity_id: Option<&str>,
    file_hint: Option<&str>,
) -> &'a sem_core::parser::graph::EntityInfo {
    // Direct lookup by entity ID
    if let Some(id) = entity_id {
        if let Some(e) = graph.entities.get(id) {
            return e;
        }
        eprintln!("{} Entity ID '{}' not found", "error:".red().bold(), id);
        std::process::exit(1);
    }

    let name = name.unwrap_or_else(|| {
        eprintln!(
            "{} Either entity name or --entity-id is required",
            "error:".red().bold()
        );
        std::process::exit(1);
    });

    let mut matching: Vec<_> = graph
        .entities
        .values()
        .filter(|e| super::entity_matches_qualified(graph, e, name))
        .collect();

    if matching.is_empty() {
        eprintln!("{} Entity '{}' not found", "error:".red().bold(), name);
        std::process::exit(1);
    }

    if let Some(file) = file_hint {
        let filtered: Vec<_> = matching
            .iter()
            .filter(|e| e.file_path == file)
            .copied()
            .collect();
        if filtered.len() == 1 {
            return filtered[0];
        }
        if filtered.is_empty() {
            eprintln!(
                "{} Entity '{}' not found in file '{}'",
                "error:".red().bold(),
                name,
                file
            );
            std::process::exit(1);
        }
        matching = filtered;
    }

    if matching.len() == 1 {
        return matching[0];
    }

    matching.sort_by_key(|e| (&e.file_path, e.start_line));
    eprintln!(
        "{} Entity name '{}' is ambiguous ({} matches). Specify --file or --entity-id:",
        "error:".red().bold(),
        name,
        matching.len()
    );
    for m in &matching {
        eprintln!(
            "  {} {} ({}:L{})",
            m.entity_type, m.id, m.file_path, m.start_line
        );
    }
    std::process::exit(1);
}

/// "direct_dependency" -> "direct dependencies", "direct_dependent" -> "direct dependents".
fn pluralize_role(role: &str) -> String {
    let spaced = role.replace('_', " ");
    if let Some(stem) = spaced.strip_suffix('y') {
        format!("{stem}ies")
    } else {
        format!("{spaced}s")
    }
}
