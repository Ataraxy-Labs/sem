use std::{collections::HashSet, path::Path};

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::index;
use sem_core::parser::graph::{EntityGraph, EntityInfo};
use sem_mcp::cache::CacheSourceScope;

use crate::timings::Timings;

pub struct ImpactOptions {
    pub cwd: String,
    pub entity_name: Option<String>,
    pub entity_id: Option<String>,
    pub file_hint: Option<String>,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub mode: ImpactMode,
    pub depth: usize,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

#[derive(Clone, Copy)]
pub enum ImpactMode {
    All,
    Deps,
    Dependents,
    Tests,
}

/// Shared output shape for every `sem impact` tier — the index fast paths
/// (`try_index_impact_deps` and friends) and the `EntityGraph`-hydrate
/// fallback both build one of these. Moved out of `build_cache` (semx-gpu
/// census, QUERY-INDEX.md §15): neither this struct nor
/// `CACHED_TEST_IMPACT_LIMIT` below depend on `DiskCache` — this file is
/// their only producer and only consumer, so `build_cache.rs` carrying them
/// was surface with no build-plane reason to live there.
pub struct CachedImpactResult {
    pub entity: EntityInfo,
    pub dependencies: Vec<EntityInfo>,
    pub dependents: Vec<EntityInfo>,
    pub impact: Vec<(EntityInfo, usize)>,
    pub tests: Vec<EntityInfo>,
    pub tests_truncated: bool,
}

pub(crate) const CACHED_TEST_IMPACT_LIMIT: usize = 10_000;

/// The one query shape every index fast path in this file must hand back to
/// the legacy resolver: a *qualified* entity address.
///
/// `find_entity` below resolves `Parent.child` / `Parent::child` by joining
/// through `parent_id` (`commands::entity_matches_qualified`);
/// `query::resolve_by_name_indices` has no parent join and instead treats a
/// `::` query as a possible *entity id*. So for a name carrying either
/// separator the two resolvers are different functions and may name different
/// entities — decline and let `find_entity` answer. For every other name they
/// are the same function (`entity_matches_qualified`'s qualifier clause can
/// only fire on a separator), which is precisely why the fast paths need no
/// `--entity-id`/`--file` gate to be safe on a name-only query: the
/// `candidates.len() != 1` ambiguity check is the whole guarantee (semx-4ex).
fn is_qualified_name(name: &str) -> bool {
    name.contains('.') || name.contains("::")
}

const LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD: usize = 20_000;

/// Run a graph build behind a uv-style spinner, then clear it and print a
/// summary before the result is used (so the spinner never interleaves with
/// command output). `count` extracts the entity count for the summary line.
fn build_with_spinner<T>(
    file_count: usize,
    build: impl FnOnce() -> T,
    count: impl FnOnce(&T) -> usize,
) -> T {
    let prog = crate::progress::Progress::start_staged();
    let result = build();
    prog.done(&format!(
        "{} entities, {} files",
        super::graph::fmt_count(count(&result)),
        super::graph::fmt_count(file_count)
    ));
    result
}

pub fn impact_command(opts: ImpactOptions) {
    // Built before the fast paths, not after, so an index-served answer is
    // observable under `SEM_TIMINGS` as its own phase rather than as an empty
    // report. The integration suite distinguishes tiers by exactly this.
    let mut timings = Timings::from_env("impact");

    // The index-backed Deps fast path goes first: it used to bypass the
    // sidecar (QUERY-INDEX.md §7 item 5, now deleted — semx-woe) and still
    // bypasses the SQLite `query_impact_topology` fast path (§7 item 4)
    // entirely, for the entity-scoped `--deps` shape those two existed to
    // serve. It answers `false` (never a wrong answer) for anything it
    // isn't confident about, and everything below is unchanged for that
    // case (semx-gis item 2).
    if try_index_impact_deps(&opts, &mut timings) {
        return;
    }
    // Same fast path, reverse direction (semx-zvq, QUERY-INDEX.md §12.1's
    // "impact.rs non---deps modes" reroute — the `Dependents` half of it:
    // like `Deps`, it is a direct-edge, depth-1, no-BFS shape (§7's table
    // has no `depth` involvement for either), so it mirrors
    // `try_index_impact_deps` exactly but for `callers_of` instead of
    // `refs_of`. `All`/`Tests` need a transitive multi-hop walk
    // (`impact_entities`'s BFS) this bead did not reroute — see that
    // function's doc and QUERY-INDEX.md §12's table entry for why.
    if try_index_impact_dependents(&opts, &mut timings) {
        return;
    }
    // The transitive half (semx-zvq): `All` and `Tests`, the two modes that
    // need `impact_entities`' multi-hop BFS rather than one CSR row. See
    // `try_index_impact_transitive` for the ordering-parity argument and
    // QUERY-INDEX.md §12.2's derivation.
    if try_index_impact_transitive(&opts, &mut timings) {
        return;
    }

    let started = std::time::Instant::now();
    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());

    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(root, &ext_filter, opts.no_default_excludes);
    let file_hint = opts
        .file_hint
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), root, file));
    // Cloud is a capability — cross-repo xref, repos with no local index/cache
    // (QUERY-INDEX.md §7 item 7) — not a latency tier, so it must not race a
    // local answer over the network. It used to run before even the
    // entity-scoped local fast path above; now it only gets a turn once both
    // local fast paths (index-backed Deps, cache-first entity scope) have
    // already declined, and before the slow walk+cold-build fallback below.
    if super::cloud::try_cloud_impact(&opts).is_some() {
        return;
    }

    let file_paths = super::graph::find_supported_files_with_options(
        root,
        &registry,
        &ext_filter,
        opts.no_default_excludes,
    );
    timings.mark("file_discovery");

    match opts.mode {
        ImpactMode::Deps => {
            let graph = build_with_spinner(
                file_paths.len(),
                || {
                    if opts.no_cache || file_paths.len() > LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD {
                        let entity_name = opts.entity_name.clone();
                        let entity_id = opts.entity_id.clone();
                        let file_hint_for_match = file_hint.clone();
                        super::graph::get_or_build_direct_dependency_graph_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                            move |entity| {
                                if let Some(id) = entity_id.as_deref() {
                                    return entity.id == id;
                                }
                                let Some(name) = entity_name.as_deref() else {
                                    return false;
                                };
                                if file_hint_for_match
                                    .as_deref()
                                    .is_some_and(|file| entity.file_path != file)
                                {
                                    return false;
                                }
                                super::entity_matches_query(entity, name)
                            },
                        )
                    } else {
                        super::graph::get_or_build_graph_topology_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    }
                },
                |g| g.entities.len(),
            );
            let entity = find_entity(
                &graph,
                opts.entity_name.as_deref(),
                opts.entity_id.as_deref(),
                file_hint.as_deref(),
            );
            timings.mark("entity_lookup");
            print_deps(&graph, entity, opts.json);
            timings.mark("cli_output_serialization");
        }
        ImpactMode::Dependents => {
            let graph = build_with_spinner(
                file_paths.len(),
                || {
                    if file_paths.len() > LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD {
                        super::graph::get_or_build_graph_topology_with_topology_save_on_miss_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    } else {
                        super::graph::get_or_build_graph_topology_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    }
                },
                |g| g.entities.len(),
            );
            let entity = find_entity(
                &graph,
                opts.entity_name.as_deref(),
                opts.entity_id.as_deref(),
                file_hint.as_deref(),
            );
            timings.mark("entity_lookup");
            print_dependents(&graph, entity, opts.json);
            timings.mark("cli_output_serialization");
        }
        ImpactMode::Tests | ImpactMode::All => {
            if file_paths.len() > LARGE_IMPACT_CACHE_MISS_FILE_THRESHOLD {
                let graph_data = build_with_spinner(
                    file_paths.len(),
                    || {
                        super::graph::get_or_build_graph_with_test_data_and_topology_save_on_miss_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    },
                    |gd| match gd {
                        super::graph::GraphWithTestData::Full(g, _) => g.entities.len(),
                        super::graph::GraphWithTestData::Topology { graph, .. } => {
                            graph.entities.len()
                        }
                    },
                );
                match graph_data {
                    super::graph::GraphWithTestData::Full(graph, all_entities) => {
                        let entity = find_entity(
                            &graph,
                            opts.entity_name.as_deref(),
                            opts.entity_id.as_deref(),
                            file_hint.as_deref(),
                        );
                        timings.mark("entity_lookup");
                        match opts.mode {
                            ImpactMode::Tests => print_tests(
                                &graph,
                                entity,
                                &all_entities,
                                opts.json,
                                &registry.custom_test_dirs,
                            ),
                            ImpactMode::All => print_all(
                                &graph,
                                entity,
                                &all_entities,
                                opts.json,
                                opts.depth,
                                &registry.custom_test_dirs,
                            ),
                            _ => unreachable!(),
                        }
                    }
                    super::graph::GraphWithTestData::Topology {
                        graph,
                        test_entity_ids,
                    } => {
                        let entity = find_entity(
                            &graph,
                            opts.entity_name.as_deref(),
                            opts.entity_id.as_deref(),
                            file_hint.as_deref(),
                        );
                        timings.mark("entity_lookup");
                        match opts.mode {
                            ImpactMode::Tests => {
                                print_tests_with_ids(&graph, entity, &test_entity_ids, opts.json)
                            }
                            ImpactMode::All => print_all_with_ids(
                                &graph,
                                entity,
                                &test_entity_ids,
                                opts.json,
                                opts.depth,
                            ),
                            _ => unreachable!(),
                        }
                    }
                }
            } else {
                let (graph, all_entities) = build_with_spinner(
                    file_paths.len(),
                    || {
                        super::graph::get_or_build_graph_with_timings(
                            root,
                            &file_paths,
                            &registry,
                            opts.no_cache,
                            source_scope,
                            &mut timings,
                        )
                    },
                    |(g, _)| g.entities.len(),
                );
                let entity = find_entity(
                    &graph,
                    opts.entity_name.as_deref(),
                    opts.entity_id.as_deref(),
                    file_hint.as_deref(),
                );
                timings.mark("entity_lookup");
                match opts.mode {
                    ImpactMode::Tests => print_tests(
                        &graph,
                        entity,
                        &all_entities,
                        opts.json,
                        &registry.custom_test_dirs,
                    ),
                    ImpactMode::All => print_all(
                        &graph,
                        entity,
                        &all_entities,
                        opts.json,
                        opts.depth,
                        &registry.custom_test_dirs,
                    ),
                    _ => unreachable!(),
                }
            }
            timings.mark("cli_output_serialization");
        }
    }
    timings.finish();
    super::consent::maybe_cloud_tip(&opts.cwd, started.elapsed());
}

/// Index-backed fast path for `sem impact --deps <entity>` (semx-gis item 2).
/// Gated on Deps mode and the default source scope, so it never answers a
/// query shape the index doesn't cover — a custom `--file-exts`/
/// `--no-default-excludes` scope isn't recorded in the image, and no bead has
/// extended the format to carry one. Any uncertainty — index absent, zero or
/// more than one match, a stale related file — returns `false` and the
/// caller falls through to the unchanged legacy path unchanged below.
///
/// **The `--entity-id`-or-`--file`-required gate is gone (semx-4ex).** It was
/// inherited verbatim from the SQLite `query_dependency_impact_topology` fast
/// path this replaced, and it was redundant with the ambiguity check below:
/// `resolve_by_name_indices` narrows by file only when a hint is given, and
/// the `candidates.len() != 1` decline is what actually keeps a name-only
/// query from being answered against the wrong entity. For a name carrying
/// neither `.` nor `::` the two resolvers are the *same function* —
/// `entity_matches_qualified`'s extra clause needs a qualifier separator to
/// fire, so it degenerates to `entity_matches_query`, which is exactly what
/// `resolve_by_name_indices` computes — and a set of size 1 on one side is a
/// set of size 1 on the other. `try_index_impact_transitive` (semx-zvq) has
/// always resolved name-only queries this way; this restores the same rule
/// here, which is what leaves the incremental rebuild as `cache.db`'s only
/// remaining reader on this verb (RESOLUTION-PROFILE.md W4 §2, W4.5).
///
/// A name carrying `.` or `::` declines outright, the rule
/// `try_index_impact_transitive` already applies for the same reason: the
/// legacy `find_entity` resolves `Parent.child`/`Parent::child` through
/// `parent_id` (`entity_matches_qualified`) and `resolve_by_name_indices`
/// instead falls back to *entity-id* resolution for a `::` query, so the two
/// can disagree on which entity a qualified name names. Declining is only
/// ever slower, never wrong.
fn try_index_impact_deps(opts: &ImpactOptions, timings: &mut Timings) -> bool {
    if opts.no_cache || !matches!(opts.mode, ImpactMode::Deps) {
        return false;
    }
    // Cheapest possible decline for the `SEM_NO_INDEX=1` A/B/isolation
    // escape hatch: no git-root resolution, no scope computation, nothing
    // beyond an env read, so a test or measurement that opts out of the
    // index pays as close to zero perturbation as this fast path can offer.
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return false;
    }
    let root = super::repo_root_or_cwd(&opts.cwd);
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(&root, &ext_filter, opts.no_default_excludes);
    if !matches!(source_scope, CacheSourceScope::Default) {
        return false;
    }
    let file_hint = opts
        .file_hint
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), &root, file));
    let Some(idx) = super::query::open_index(&root) else {
        return false;
    };

    let at = if let Some(id) = opts.entity_id.as_deref() {
        super::query::resolve_by_id_index(&idx, id)
    } else {
        let Some(name) = opts.entity_name.as_deref() else {
            return false;
        };
        if is_qualified_name(name) {
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

    // Membership sweep (semx-dev, QUERY-INDEX.md §13's `Complete` tier),
    // run concurrently with the resolution above's freshness work exactly
    // like `commands::query::index_answer` already does for `find`/
    // `callers`/`refs` (semx-ykf) — the *same* mechanism, not a second one.
    // The bug this closes: a `b.ts` that already said `import './optional'`
    // has an unresolved ref at build time (no entity in `optional.ts` to
    // point at yet), so `refs_of(at)` never carried an edge to it, and
    // `optional.ts` is neither `entity`'s own file nor a *known* dependency's
    // file — the per-file `touched` check below is structurally blind to a
    // dependency that doesn't exist yet. `refs_of(at)` itself may now be
    // incomplete for the same reason `callers`/`refs` decline on any new
    // file: a new file can supply a new edge this CSR (built at the last
    // full index write) has no way to represent. Decline to the legacy path,
    // which re-walks and re-resolves `entity`'s imports fresh and so folds
    // the new target's refs in "for free" (QUERY-INDEX.md §13.2's own
    // phrase for the identical `callers`/`refs` case).
    let registry = super::create_registry(&opts.cwd);
    let (resolved, complete) = rayon::join(
        || {
            let entity = idx.entity(at).to_entity_info();
            let dependencies: Vec<EntityInfo> = idx
                .refs_of(at)
                .iter()
                .map(index::Entity::to_entity_info)
                .collect();

            // Verified freshness (§5.1) over the entity's own file plus every
            // direct dependency's file. A stale dependency target means
            // `refs_of(at)` may itself be wrong (the edit could add/remove a
            // call), which a per-file re-extract of the *target* can't
            // repair — that needs re-resolving `entity`'s own references.
            // Bail to the cold/legacy path rather than risk serving a stale
            // edge (the same non-local-staleness call `commands::query`
            // makes for `callers`/`refs`).
            let mut touched: Vec<String> = std::iter::once(entity.file_path.clone())
                .chain(dependencies.iter().map(|d| d.file_path.clone()))
                .collect();
            touched.sort_unstable();
            touched.dedup();
            let stale = touched
                .iter()
                .any(|path| super::query::is_file_stale(&idx, &root, path));
            (entity, dependencies, stale)
        },
        || {
            index::complete_check(&idx, &root, |dir: &Path| {
                super::files::find_supported_files_in_path(&root, dir, &registry, &[], false)
            })
        },
    );
    let (entity, dependencies, touched_stale) = resolved;
    // A *new* file is the only membership signal this fast path must
    // decline on (semx-dev): it can supply an edge the CSR predates and so
    // has no way to represent. An unrelated *deletion* elsewhere in the
    // corpus cannot manufacture a new edge, so it is deliberately not a
    // decline reason here — the pre-existing `_answers_from_the_index_
    // when_an_unrelated_..._is_deleted/missing` tests pin exactly that
    // contract, and `complete.is_clean()` (used verbatim by `corpus_is_
    // fresh` for the transitive reroutes) would over-decline it.
    if touched_stale || complete.inconclusive || !complete.new_files.is_empty() {
        return false;
    }

    let result = CachedImpactResult {
        entity,
        dependencies,
        dependents: Vec::new(),
        impact: Vec::new(),
        tests: Vec::new(),
        tests_truncated: false,
    };
    timings.mark("index_impact_deps");
    print_cached_result(&result, opts.mode, opts.json, opts.depth);
    timings.finish();
    true
}

/// Reverse-direction mirror of [`try_index_impact_deps`] (semx-zvq): direct
/// callers of an entity, via `callers_of` instead of `refs_of`. `Dependents`
/// has the identical direct-edge, depth-1 shape `Deps` does (`print_cached_
/// dependents` takes no `depth` parameter, same as `print_cached_deps`), so
/// nothing here needed the typed CSR added in this bead's FORMAT step —
/// `refs_of`/`callers_of` (untyped) already carried what this shape needs.
/// The typed accessors matter for a future `--ref-type` filter on `impact`,
/// not for this reroute.
///
/// Carries the same gate closure as [`try_index_impact_deps`] (semx-4ex):
/// this function inherited the identical `--entity-id`-or-`--file` decline by
/// being written as that one's mirror, so it inherited the same redundancy.
/// Leaving it here would have left `sem impact --dependents <name>` as a
/// second name-only shape falling through to the `EntityGraph` hydrate — i.e.
/// a second `cache.db` reader — which would falsify the very census that
/// justifies making the write conditional.
fn try_index_impact_dependents(opts: &ImpactOptions, timings: &mut Timings) -> bool {
    if opts.no_cache || !matches!(opts.mode, ImpactMode::Dependents) {
        return false;
    }
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return false;
    }
    let root = super::repo_root_or_cwd(&opts.cwd);
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(&root, &ext_filter, opts.no_default_excludes);
    if !matches!(source_scope, CacheSourceScope::Default) {
        return false;
    }
    let file_hint = opts
        .file_hint
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), &root, file));
    let Some(idx) = super::query::open_index(&root) else {
        return false;
    };

    let at = if let Some(id) = opts.entity_id.as_deref() {
        super::query::resolve_by_id_index(&idx, id)
    } else {
        let Some(name) = opts.entity_name.as_deref() else {
            return false;
        };
        if is_qualified_name(name) {
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

    // Same membership-sweep discipline as `try_index_impact_deps` (semx-dev),
    // mirrored to the reverse direction: a brand-new file that *calls* an
    // already-indexed entity is a new edge `callers_of(at)` cannot carry
    // (the CSR predates the file), and no per-file staleness check below can
    // catch it because the new file isn't "entity's own" or a *known*
    // caller's file. Same mechanism as `try_index_impact_deps`, not a second
    // one.
    let registry = super::create_registry(&opts.cwd);
    let (resolved, complete) = rayon::join(
        || {
            let entity = idx.entity(at).to_entity_info();
            let dependents: Vec<EntityInfo> = idx
                .callers_of(at)
                .iter()
                .map(index::Entity::to_entity_info)
                .collect();

            // Same verified-freshness discipline as `try_index_impact_deps`,
            // mirrored to the reverse direction: a stale *caller* file means
            // `callers_of(at)` may itself be wrong (the edit could
            // add/remove a call to `entity`), which a per-file re-extract of
            // the caller can't repair on its own — bail to the legacy path
            // rather than serve a possibly-stale edge set.
            let mut touched: Vec<String> = std::iter::once(entity.file_path.clone())
                .chain(dependents.iter().map(|d| d.file_path.clone()))
                .collect();
            touched.sort_unstable();
            touched.dedup();
            let stale = touched
                .iter()
                .any(|path| super::query::is_file_stale(&idx, &root, path));
            (entity, dependents, stale)
        },
        || {
            index::complete_check(&idx, &root, |dir: &Path| {
                super::files::find_supported_files_in_path(&root, dir, &registry, &[], false)
            })
        },
    );
    let (entity, dependents, touched_stale) = resolved;
    // A *new* file is the only membership signal this fast path must
    // decline on (semx-dev): it can supply an edge the CSR predates and so
    // has no way to represent. An unrelated *deletion* elsewhere in the
    // corpus cannot manufacture a new edge, so it is deliberately not a
    // decline reason here — the pre-existing `_answers_from_the_index_
    // when_an_unrelated_..._is_deleted/missing` tests pin exactly that
    // contract, and `complete.is_clean()` (used verbatim by `corpus_is_
    // fresh` for the transitive reroutes) would over-decline it.
    if touched_stale || complete.inconclusive || !complete.new_files.is_empty() {
        return false;
    }

    let result = CachedImpactResult {
        entity,
        dependencies: Vec::new(),
        dependents,
        impact: Vec::new(),
        tests: Vec::new(),
        tests_truncated: false,
    };
    timings.mark("index_impact_dependents");
    print_cached_result(&result, opts.mode, opts.json, opts.depth);
    timings.finish();
    true
}

/// Index-backed fast path for `sem impact` in `All` and `Tests` mode
/// (semx-zvq) — the transitive reroute §12.2 left open, replacing
/// `query_fresh_impact_topology`'s recursive SQL walk.
///
/// **Ordering parity.** The bar is byte-identical output, and the legacy
/// order is not incidental — it is spelled out in three `ORDER BY` clauses.
/// Derived first, reproduced second (QUERY-INDEX.md §12.3 carries the full
/// derivation):
///
/// - `direct_dependencies`: `ORDER BY edges.to_entity, edges.ref_type`, one
///   row per *edge*, `ref_type` compared as TEXT (`calls` < `imports` <
///   `typeref`).
/// - `dependent_ids_for`: `ORDER BY to_entity, from_entity, ref_type`,
///   regrouped and emitted in **frontier order**, so a BFS layer is the
///   concatenation, over the previous layer in order, of each node's callers
///   in `(caller_id, ref_type)` order, first-seen only.
/// - `impact_ids`: layer-at-a-time, `max_depth == 0` unlimited, `max_count`
///   cutting mid-layer.
///
/// The CSR reproduces the first two *structurally*, not by re-sorting:
/// `graph.edges` is globally sorted by `(from_entity, to_entity,
/// ref_type_sort_key)` (`parser::graph::sort_entity_refs`) with
/// `Calls < Imports < TypeRef` — the same order SQLite's BINARY collation
/// gives those three strings — and `build_refs_section` groups that sorted
/// vector without disturbing it. A forward row is therefore already in
/// `direct_dependencies`' order; a reverse row, whose members share a
/// `to_entity` and so are ordered by the sort's *primary* key, is already in
/// `dependent_ids_for`'s. Only the layering below is written out here.
///
/// **Test classification** is not re-derived either: `is_test` rides in the
/// image (`FLAG_ENTITY_TESTS`), packed at build time from the very
/// `filter_test_entities_with_custom_dirs` call that fills the SQL cache's
/// `entity_flags` — so "what is a test" cannot drift between the two tiers.
/// An image without that flag declines, mirroring the SQL path's own
/// `test_flags_computed` gate.
///
/// **Freshness** is whole-corpus (`query::corpus_is_fresh`), not
/// per-answer-file like the `Deps`/`Dependents` reroutes: a transitive
/// closure has no locality: an edit in a file the walk never visits can add
/// an edge *into* it. That is exactly the guarantee `has_fresh_cache` gave
/// the SQL path, kept rather than quietly weakened.
///
/// `Tests` declines on an empty result rather than printing it — the same
/// "empty tests from the cache is not authoritative" fallthrough
/// `try_cached_impact_query` already performs, which is what routes such a
/// query to the legacy path's lexical (`word_hit`) fallback. `All` has no
/// such rule and prints an empty test list, again matching the SQL path.
fn try_index_impact_transitive(opts: &ImpactOptions, timings: &mut Timings) -> bool {
    if opts.no_cache || !matches!(opts.mode, ImpactMode::All | ImpactMode::Tests) {
        return false;
    }
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return false;
    }
    let root = super::repo_root_or_cwd(&opts.cwd);
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(&root, &ext_filter, opts.no_default_excludes);
    if !matches!(source_scope, CacheSourceScope::Default) {
        return false;
    }
    let file_hint = opts
        .file_hint
        .as_deref()
        .map(|file| super::normalize_repo_relative_path(Path::new(&opts.cwd), &root, file));
    let Some(idx) = super::query::open_index(&root) else {
        return false;
    };
    if !idx.has_refs() || !idx.has_test_flags() {
        return false;
    }

    let at = if let Some(id) = opts.entity_id.as_deref() {
        super::query::resolve_by_id_index(&idx, id)
    } else {
        let Some(name) = opts.entity_name.as_deref() else {
            return false;
        };
        if is_qualified_name(name) {
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

    if !super::query::corpus_is_fresh(&idx, &root, &opts.cwd) {
        return false;
    }

    let entity = idx.entity(at).to_entity_info();

    // `test_impact_entities`: unlimited depth, capped one past the limit so
    // the cap itself is detectable, truncated, then filtered to tests in BFS
    // order. Same three steps, same constants.
    let mut test_walk = index_impact_ids(&idx, at, 0, Some(CACHED_TEST_IMPACT_LIMIT + 1));
    let tests_truncated = test_walk.len() > CACHED_TEST_IMPACT_LIMIT;
    if tests_truncated {
        test_walk.truncate(CACHED_TEST_IMPACT_LIMIT);
    }
    let tests: Vec<EntityInfo> = test_walk
        .into_iter()
        .map(|(index, _)| idx.entity(index))
        .filter(index::Entity::is_test)
        .map(|e| e.to_entity_info())
        .collect();

    if matches!(opts.mode, ImpactMode::Tests) {
        if tests.is_empty() {
            // Not authoritative — the graph can miss tests that reach the
            // target through a module namespace. The legacy path's lexical
            // fallback needs entity bodies, which the image does not carry.
            return false;
        }
        let result = CachedImpactResult {
            entity,
            dependencies: Vec::new(),
            dependents: Vec::new(),
            impact: Vec::new(),
            tests,
            tests_truncated,
        };
        timings.mark("index_impact_tests");
        print_cached_result(&result, opts.mode, opts.json, opts.depth);
        timings.finish();
        return true;
    }

    let dependencies: Vec<EntityInfo> = idx
        .refs_of(at)
        .iter()
        .map(index::Entity::to_entity_info)
        .collect();
    let impact: Vec<(EntityInfo, usize)> = index_impact_ids(&idx, at, opts.depth, None)
        .into_iter()
        .map(|(index, depth)| (idx.entity(index).to_entity_info(), depth))
        .collect();
    let dependents: Vec<EntityInfo> = impact
        .iter()
        .filter(|(_, depth)| *depth == 1)
        .map(|(entity, _)| entity.clone())
        .collect();

    let result = CachedImpactResult {
        entity,
        dependencies,
        dependents,
        impact,
        tests,
        tests_truncated,
    };
    timings.mark("index_impact_all");
    print_cached_result(&result, opts.mode, opts.json, opts.depth);
    timings.finish();
    true
}

/// `DiskCache::impact_ids`, over the reverse CSR instead of a recursive SQL
/// walk. Line-for-line the same traversal — same layer-at-a-time frontier,
/// same `max_depth == 0` means unlimited, same "`max_count` returns
/// immediately, mid-layer" cutoff, same first-seen dedup — because the
/// output order is part of the contract, not an implementation detail. The
/// only representational change is that `visited` keys on the entity's index
/// rather than its id string, which cannot affect ordering: it is the same
/// set membership test, just cheaper.
fn index_impact_ids(
    idx: &index::QueryIndex,
    at: usize,
    max_depth: usize,
    max_count: Option<usize>,
) -> Vec<(usize, usize)> {
    let mut visited: HashSet<usize> = HashSet::new();
    visited.insert(at);
    let mut frontier = vec![at];
    let mut result: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0usize;

    while !frontier.is_empty() {
        if max_depth > 0 && depth >= max_depth {
            break;
        }
        let next_depth = depth + 1;
        let mut next_frontier = Vec::new();
        for node in &frontier {
            for caller in idx.callers_of(*node) {
                let caller = caller.index();
                if visited.insert(caller) {
                    result.push((caller, next_depth));
                    next_frontier.push(caller);
                    if max_count.is_some_and(|limit| result.len() >= limit) {
                        return result;
                    }
                }
            }
        }
        frontier = next_frontier;
        depth = next_depth;
    }

    result
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
        // Multiple matches even within the file — fall through to ambiguity error
        matching = filtered;
    }

    if matching.len() == 1 {
        return matching[0];
    }

    // Multiple matches — report ambiguity
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

fn entity_json(e: &sem_core::parser::graph::EntityInfo) -> serde_json::Value {
    serde_json::json!({
        "entityId": e.id, "name": e.name, "type": e.entity_type,
        "file": e.file_path, "lines": [e.start_line, e.end_line],
    })
}

fn entity_list_json(entities: &[&sem_core::parser::graph::EntityInfo]) -> Vec<serde_json::Value> {
    entities.iter().map(|e| entity_json(*e)).collect()
}

fn owned_entity_list_json(entities: &[EntityInfo]) -> Vec<serde_json::Value> {
    entities.iter().map(entity_json).collect()
}

fn print_entity_header(e: &sem_core::parser::graph::EntityInfo) {
    println!(
        "{} {} {} ({}:{}–{})",
        "⊕".green(),
        e.entity_type.dimmed(),
        e.name.bold(),
        e.file_path.dimmed(),
        e.start_line,
        e.end_line,
    );
}

fn print_cached_result(result: &CachedImpactResult, mode: ImpactMode, json: bool, depth: usize) {
    match mode {
        ImpactMode::Deps => {
            print_cached_deps(&result.entity, &result.dependencies, json);
        }
        ImpactMode::Dependents => {
            print_cached_dependents(&result.entity, &result.dependents, json);
        }
        ImpactMode::Tests => {
            print_cached_tests(&result.entity, &result.tests, result.tests_truncated, json);
        }
        ImpactMode::All => {
            print_cached_all(result, json, depth);
        }
    }
}

/// Shared by [`print_cached_deps`] (cached-result path, owned `EntityInfo`s)
/// and [`print_deps`] (live-graph path, `graph.get_dependencies`'s borrowed
/// `EntityInfo`s) — the two bodies were byte-identical modulo which owned/
/// borrowed slice supplied `deps`.
fn print_impact_deps(entity: &EntityInfo, deps: &[&EntityInfo], json: bool) {
    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependencies": entity_list_json(deps),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if deps.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No dependencies.".dimmed());
        } else {
            println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
            for dep in deps {
                println!(
                    "    {} {} {} ({})",
                    "→".blue(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
        println!();
    }
}

fn print_cached_deps(entity: &EntityInfo, deps: &[EntityInfo], json: bool) {
    let deps: Vec<&EntityInfo> = deps.iter().collect();
    print_impact_deps(entity, &deps, json);
}

/// Shared by [`print_cached_dependents`] and [`print_dependents`] — same
/// relationship as [`print_impact_deps`], for the dependents direction.
fn print_impact_dependents(entity: &EntityInfo, dependents: &[&EntityInfo], json: bool) {
    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependents": entity_list_json(dependents),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if dependents.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No dependents.".dimmed());
        } else {
            println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
            for dep in dependents {
                println!(
                    "    {} {} {} ({})",
                    "←".yellow(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
        println!();
    }
}

fn print_cached_dependents(entity: &EntityInfo, dependents: &[EntityInfo], json: bool) {
    let dependents: Vec<&EntityInfo> = dependents.iter().collect();
    print_impact_dependents(entity, &dependents, json);
}

fn print_cached_tests(entity: &EntityInfo, tests: &[EntityInfo], truncated: bool, json: bool) {
    if json {
        let mut output = serde_json::json!({
            "entity": entity_json(entity),
            "tests": owned_entity_list_json(tests),
        });
        if truncated {
            output
                .as_object_mut()
                .unwrap()
                .insert("testsTruncated".to_string(), serde_json::json!(true));
        }
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if tests.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No tests found.".dimmed());
        } else {
            println!(
                "\n  {} {}",
                "⚡".yellow(),
                format!("{} tests affected:", tests.len()).bold()
            );
            let mut by_file: std::collections::HashMap<&str, Vec<_>> =
                std::collections::HashMap::new();
            for test in tests {
                by_file
                    .entry(test.file_path.as_str())
                    .or_default()
                    .push(test);
            }
            let mut files: Vec<_> = by_file.keys().copied().collect();
            files.sort();
            for file in files {
                println!("    {}", file.bold());
                let mut entities = by_file[file].clone();
                entities.sort_by_key(|test| test.start_line);
                for test in entities {
                    println!(
                        "      {} {} (L{}–{})",
                        test.entity_type.dimmed(),
                        test.name.bold(),
                        test.start_line,
                        test.end_line,
                    );
                }
            }
        }
        print_cached_tests_truncation_warning(truncated);
        println!();
    }
}

fn print_cached_tests_truncation_warning(truncated: bool) {
    if truncated {
        println!(
            "\n  {} {}",
            "warning:".yellow().bold(),
            "Cached test impact reached its traversal limit; results may be incomplete.".yellow()
        );
    }
}

fn print_cached_all(result: &CachedImpactResult, json: bool, depth: usize) {
    if json {
        let impact_entities: Vec<serde_json::Value> = result
            .impact
            .iter()
            .map(|(entity, depth)| {
                let mut value = entity_json(entity);
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("depth".to_string(), serde_json::json!(depth));
                value
            })
            .collect();
        let mut output = serde_json::json!({
            "entity": entity_json(&result.entity),
            "dependencies": owned_entity_list_json(&result.dependencies),
            "dependents": owned_entity_list_json(&result.dependents),
            "impact": {
                "depth": depth,
                "total": result.impact.len(),
                "entities": impact_entities,
            },
            "tests": owned_entity_list_json(&result.tests),
        });
        if result.tests_truncated {
            output
                .as_object_mut()
                .unwrap()
                .insert("testsTruncated".to_string(), serde_json::json!(true));
        }
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    print_entity_header(&result.entity);

    if !result.dependencies.is_empty() {
        println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
        for dep in &result.dependencies {
            println!(
                "    {} {} {} ({})",
                "→".blue(),
                dep.entity_type.dimmed(),
                dep.name.bold(),
                dep.file_path.dimmed(),
            );
        }
    }

    if !result.dependents.is_empty() {
        println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
        for dep in &result.dependents {
            println!(
                "    {} {} {} ({})",
                "←".yellow(),
                dep.entity_type.dimmed(),
                dep.name.bold(),
                dep.file_path.dimmed(),
            );
        }
    }

    if result.impact.is_empty() {
        println!(
            "\n  {} {}",
            "✓".green().bold(),
            "No other entities are affected by changes to this entity.".dimmed()
        );
    } else {
        let max_depth_seen = result
            .impact
            .iter()
            .map(|(_, depth)| *depth)
            .max()
            .unwrap_or(0);
        let depth_label = if depth == 0 {
            "unlimited".to_string()
        } else {
            format!("depth {}", depth)
        };
        println!(
            "\n  {} {}",
            "!".red().bold(),
            format!(
                "{} entities transitively affected ({}):",
                result.impact.len(),
                depth_label
            )
            .red(),
        );

        for current_depth in 1..=max_depth_seen {
            let at_depth: Vec<_> = result
                .impact
                .iter()
                .filter(|(_, depth)| *depth == current_depth)
                .map(|(entity, _)| entity)
                .collect();
            if at_depth.is_empty() {
                continue;
            }

            let label = if current_depth == 1 {
                "Direct dependents".to_string()
            } else {
                format!("Depth {}", current_depth)
            };
            println!("\n    {} ({})", label.bold(), at_depth.len());
            for entity in at_depth {
                println!(
                    "      {} {} {} ({}:L{})",
                    "→".red(),
                    entity.entity_type.dimmed(),
                    entity.name.bold(),
                    entity.file_path.dimmed(),
                    entity.start_line,
                );
            }
        }
    }

    if !result.tests.is_empty() {
        println!(
            "\n  {} {}",
            "⚡".yellow(),
            format!("{} tests affected:", result.tests.len()).bold()
        );
        for test in &result.tests {
            println!(
                "    {} {} ({})",
                test.entity_type.dimmed(),
                test.name.bold(),
                test.file_path.dimmed(),
            );
        }
    }
    print_cached_tests_truncation_warning(result.tests_truncated);

    println!();
}

fn print_deps(graph: &EntityGraph, entity: &sem_core::parser::graph::EntityInfo, json: bool) {
    let deps = graph.get_dependencies(&entity.id);
    print_impact_deps(entity, &deps, json);
}

fn print_dependents(graph: &EntityGraph, entity: &sem_core::parser::graph::EntityInfo, json: bool) {
    let dependents = graph.get_dependents(&entity.id);
    print_impact_dependents(entity, &dependents, json);
}

fn print_tests(
    graph: &EntityGraph,
    entity: &EntityInfo,
    all_entities: &[sem_core::model::entity::SemanticEntity],
    json: bool,
    custom_test_dirs: &[String],
) {
    let tests = graph.test_impact_with_custom_dirs(&entity.id, all_entities, custom_test_dirs);
    if !tests.is_empty() {
        print_tests_result(entity, &tests, json);
        return;
    }
    // Graph edges can miss tests that call the target through a module
    // namespace ("xr.where(...)"): the attribute call resolves to no entity.
    // Fall back to lexical reachability — test bodies naming the entity as a
    // word — and say so, since it is weaker evidence than a call edge.
    let test_ids = graph.filter_test_entities_with_custom_dirs(all_entities, custom_test_dirs);
    let owned: Vec<EntityInfo> = all_entities
        .iter()
        .filter(|e| test_ids.contains(e.id.as_str()) && word_hit(&e.content, &entity.name))
        .map(|e| EntityInfo {
            id: e.id.clone(),
            name: e.name.clone(),
            entity_type: e.entity_type.clone(),
            file_path: e.file_path.clone(),
            parent_id: e.parent_id.clone(),
            start_line: e.start_line,
            end_line: e.end_line,
        })
        .collect();
    if !owned.is_empty() && !json {
        println!(
            "{}",
            "  (no call-graph edges reach tests; lexical fallback — test bodies naming the entity)"
                .dimmed()
        );
    }
    let refs: Vec<&EntityInfo> = owned.iter().collect();
    print_tests_result(entity, &refs, json);
}

/// True when `name` appears in `body` as a whole word (not as a substring of
/// a longer identifier).
pub(crate) fn word_hit(body: &str, name: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = body[start..].find(name) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !body[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = abs + name.len();
        let after_ok = after >= body.len()
            || !body[after..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if before_ok && after_ok {
            return true;
        }
        start = abs + name.len();
    }
    false
}

fn print_tests_with_ids(
    graph: &EntityGraph,
    entity: &EntityInfo,
    test_entity_ids: &HashSet<String>,
    json: bool,
) {
    let tests = test_impact_from_ids(graph, &entity.id, test_entity_ids);
    print_tests_result(entity, &tests, json);
}

fn print_tests_result(entity: &EntityInfo, tests: &[&EntityInfo], json: bool) {
    if json {
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "tests": entity_list_json(tests),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);
        if tests.is_empty() {
            println!("\n  {} {}", "✓".green().bold(), "No tests found.".dimmed());
        } else {
            println!(
                "\n  {} {}",
                "⚡".yellow(),
                format!("{} tests affected:", tests.len()).bold()
            );
            let mut by_file: std::collections::HashMap<&str, Vec<_>> =
                std::collections::HashMap::new();
            for t in tests {
                by_file.entry(t.file_path.as_str()).or_default().push(t);
            }
            let mut files: Vec<_> = by_file.keys().copied().collect();
            files.sort();
            for file in files {
                println!("    {}", file.bold());
                let mut entities = by_file[file].clone();
                entities.sort_by_key(|e| e.start_line);
                for t in entities {
                    println!(
                        "      {} {} (L{}–{})",
                        t.entity_type.dimmed(),
                        t.name.bold(),
                        t.start_line,
                        t.end_line,
                    );
                }
            }
        }
        println!();
    }
}

fn print_all(
    graph: &EntityGraph,
    entity: &EntityInfo,
    all_entities: &[sem_core::model::entity::SemanticEntity],
    json: bool,
    depth: usize,
    custom_test_dirs: &[String],
) {
    let tests = graph.test_impact_with_custom_dirs(&entity.id, all_entities, custom_test_dirs);
    print_all_with_tests(graph, entity, &tests, json, depth);
}

fn print_all_with_ids(
    graph: &EntityGraph,
    entity: &EntityInfo,
    test_entity_ids: &HashSet<String>,
    json: bool,
    depth: usize,
) {
    let tests = test_impact_from_ids(graph, &entity.id, test_entity_ids);
    print_all_with_tests(graph, entity, &tests, json, depth);
}

fn test_impact_from_ids<'a>(
    graph: &'a EntityGraph,
    entity_id: &str,
    test_entity_ids: &HashSet<String>,
) -> Vec<&'a EntityInfo> {
    graph
        .impact_analysis(entity_id)
        .into_iter()
        .filter(|info| test_entity_ids.contains(&info.id))
        .collect()
}

fn print_all_with_tests(
    graph: &EntityGraph,
    entity: &EntityInfo,
    tests: &[&EntityInfo],
    json: bool,
    depth: usize,
) {
    let deps = graph.get_dependencies(&entity.id);
    let dependents = graph.get_dependents(&entity.id);
    let impact_bounded = graph.impact_analysis_bounded(&entity.id, depth);

    if json {
        let impact_entities: Vec<serde_json::Value> = impact_bounded
            .iter()
            .map(|(e, d)| {
                let mut v = entity_json(e);
                v.as_object_mut()
                    .unwrap()
                    .insert("depth".to_string(), serde_json::json!(d));
                v
            })
            .collect();
        let output = serde_json::json!({
            "entity": entity_json(entity),
            "dependencies": entity_list_json(&deps),
            "dependents": entity_list_json(&dependents),
            "impact": {
                "depth": depth,
                "total": impact_bounded.len(),
                "entities": impact_entities,
            },
            "tests": entity_list_json(tests),
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        print_entity_header(entity);

        // Dependencies
        if !deps.is_empty() {
            println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
            for dep in &deps {
                println!(
                    "    {} {} {} ({})",
                    "→".blue(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }

        // Dependents
        if !dependents.is_empty() {
            println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
            for dep in &dependents {
                println!(
                    "    {} {} {} ({})",
                    "←".yellow(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }

        // Transitive impact grouped by depth
        if impact_bounded.is_empty() {
            println!(
                "\n  {} {}",
                "✓".green().bold(),
                "No other entities are affected by changes to this entity.".dimmed()
            );
        } else {
            let max_depth_seen = impact_bounded.iter().map(|(_, d)| *d).max().unwrap_or(0);
            let depth_label = if depth == 0 {
                "unlimited".to_string()
            } else {
                format!("depth {}", depth)
            };
            println!(
                "\n  {} {}",
                "!".red().bold(),
                format!(
                    "{} entities transitively affected ({}):",
                    impact_bounded.len(),
                    depth_label
                )
                .red(),
            );

            for d in 1..=max_depth_seen {
                let at_depth: Vec<_> = impact_bounded
                    .iter()
                    .filter(|(_, dd)| *dd == d)
                    .map(|(e, _)| *e)
                    .collect();
                if at_depth.is_empty() {
                    continue;
                }

                let label = if d == 1 {
                    "Direct dependents".to_string()
                } else {
                    format!("Depth {}", d)
                };
                println!("\n    {} ({})", label.bold(), at_depth.len());
                for imp in &at_depth {
                    println!(
                        "      {} {} {} ({}:L{})",
                        "→".red(),
                        imp.entity_type.dimmed(),
                        imp.name.bold(),
                        imp.file_path.dimmed(),
                        imp.start_line,
                    );
                }
            }
        }

        // Tests
        if !tests.is_empty() {
            println!(
                "\n  {} {}",
                "⚡".yellow(),
                format!("{} tests affected:", tests.len()).bold()
            );
            for t in tests {
                println!(
                    "    {} {} ({})",
                    t.entity_type.dimmed(),
                    t.name.bold(),
                    t.file_path.dimmed(),
                );
            }
        }

        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::index_impact_ids;
    use sem_core::index::{self, QueryIndex};
    use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityInfoMap, EntityRef, RefType};

    fn entity(id: &str, file: &str, name: &str) -> EntityInfo {
        EntityInfo {
            id: id.to_string(),
            name: name.to_string(),
            entity_type: "function".to_string(),
            file_path: file.to_string(),
            parent_id: None,
            start_line: 1,
            end_line: 1,
        }
    }

    fn edge(from: &str, to: &str) -> EntityRef {
        EntityRef {
            from_entity: from.to_string(),
            to_entity: to.to_string(),
            ref_type: RefType::Calls,
        }
    }

    fn index_of(entities: Vec<EntityInfo>, edges: Vec<EntityRef>) -> QueryIndex {
        let map: EntityInfoMap = entities.into_iter().map(|e| (e.id.clone(), e)).collect();
        let graph = EntityGraph::from_parts(map, edges);
        let bytes = index::build(&graph, &[]);
        QueryIndex::from_bytes(bytes).expect("valid image")
    }

    fn at_of(idx: &QueryIndex, id: &str) -> usize {
        (0..idx.entity_count())
            .find(|at| idx.entity(*at).id() == id)
            .expect("entity present")
    }

    fn ids(idx: &QueryIndex, walk: Vec<(usize, usize)>) -> Vec<(String, usize)> {
        walk.into_iter()
            .map(|(at, depth)| (idx.entity(at).id(), depth))
            .collect()
    }

    /// The index-side mirror of `cache::tests::
    /// query_impact_topology_preserves_bfs_frontier_order`, on the identical
    /// fixture: the point is that layer 2 emits `z_mid` *before* `a_mid`
    /// because layer 1 emitted `b_parent` before `c_parent` — frontier order,
    /// not alphabetical. Any traversal that sorted per layer, or that used a
    /// hash-ordered container anywhere, fails this.
    fn bfs_fixture() -> (QueryIndex, usize) {
        let idx = index_of(
            vec![
                entity("root.rs::function::root", "root.rs", "root"),
                entity("b_parent.rs::function::b_parent", "b_parent.rs", "b_parent"),
                entity("c_parent.rs::function::c_parent", "c_parent.rs", "c_parent"),
                entity("z_mid.rs::function::z_mid", "z_mid.rs", "z_mid"),
                entity("a_mid.rs::function::a_mid", "a_mid.rs", "a_mid"),
                entity("z_leaf.rs::function::z_leaf", "z_leaf.rs", "z_leaf"),
                entity("a_leaf.rs::function::a_leaf", "a_leaf.rs", "a_leaf"),
            ],
            vec![
                edge("b_parent.rs::function::b_parent", "root.rs::function::root"),
                edge("c_parent.rs::function::c_parent", "root.rs::function::root"),
                edge(
                    "z_mid.rs::function::z_mid",
                    "b_parent.rs::function::b_parent",
                ),
                edge(
                    "a_mid.rs::function::a_mid",
                    "c_parent.rs::function::c_parent",
                ),
                edge("z_leaf.rs::function::z_leaf", "z_mid.rs::function::z_mid"),
                edge("a_leaf.rs::function::a_leaf", "a_mid.rs::function::a_mid"),
            ],
        );
        let at = at_of(&idx, "root.rs::function::root");
        (idx, at)
    }

    #[test]
    fn index_impact_ids_preserves_the_sql_paths_bfs_frontier_order() {
        let (idx, at) = bfs_fixture();
        assert_eq!(
            ids(&idx, index_impact_ids(&idx, at, 3, None)),
            vec![
                ("b_parent.rs::function::b_parent".to_string(), 1),
                ("c_parent.rs::function::c_parent".to_string(), 1),
                ("z_mid.rs::function::z_mid".to_string(), 2),
                ("a_mid.rs::function::a_mid".to_string(), 2),
                ("z_leaf.rs::function::z_leaf".to_string(), 3),
                ("a_leaf.rs::function::a_leaf".to_string(), 3),
            ]
        );
    }

    #[test]
    fn index_impact_ids_stops_at_max_depth_and_treats_zero_as_unlimited() {
        let (idx, at) = bfs_fixture();
        let bounded = ids(&idx, index_impact_ids(&idx, at, 2, None));
        assert_eq!(bounded.len(), 4);
        assert!(bounded.iter().all(|(_, depth)| *depth <= 2));
        assert_eq!(ids(&idx, index_impact_ids(&idx, at, 0, None)).len(), 6);
    }

    /// `impact_ids` returns the moment `max_count` is reached, *mid-layer* —
    /// it does not finish the layer first. `test_impact_entities` depends on
    /// that: it asks for `LIMIT + 1` precisely so a full result is detectable
    /// as truncation, which only works if the cutoff is exact. No corpus in
    /// the battery reaches the real 10,000 limit, so the boundary is proven
    /// here instead.
    #[test]
    fn index_impact_ids_cuts_off_exactly_at_max_count_mid_layer() {
        let (idx, at) = bfs_fixture();
        assert_eq!(ids(&idx, index_impact_ids(&idx, at, 0, Some(1))).len(), 1);
        assert_eq!(
            ids(&idx, index_impact_ids(&idx, at, 0, Some(3))),
            vec![
                ("b_parent.rs::function::b_parent".to_string(), 1),
                ("c_parent.rs::function::c_parent".to_string(), 1),
                ("z_mid.rs::function::z_mid".to_string(), 2),
            ]
        );
        // Asking for more than exists returns everything, which is how
        // `tests_truncated` reads `false`.
        assert_eq!(ids(&idx, index_impact_ids(&idx, at, 0, Some(7))).len(), 6);
    }

    /// A cycle must terminate and must not re-emit, and a diamond must emit
    /// its shared node once, at the depth it was *first* reached — the
    /// `visited.insert` contract, which is the only thing standing between
    /// this walk and an infinite loop on real corpora (they have cycles).
    #[test]
    fn index_impact_ids_visits_each_entity_once_through_cycles_and_diamonds() {
        let idx = index_of(
            vec![
                entity("a.rs::function::a", "a.rs", "a"),
                entity("b.rs::function::b", "b.rs", "b"),
                entity("c.rs::function::c", "c.rs", "c"),
                entity("d.rs::function::d", "d.rs", "d"),
            ],
            vec![
                edge("b.rs::function::b", "a.rs::function::a"),
                edge("c.rs::function::c", "a.rs::function::a"),
                edge("d.rs::function::d", "b.rs::function::b"),
                edge("d.rs::function::d", "c.rs::function::c"),
                edge("a.rs::function::a", "d.rs::function::d"),
            ],
        );
        let at = at_of(&idx, "a.rs::function::a");
        assert_eq!(
            ids(&idx, index_impact_ids(&idx, at, 0, None)),
            vec![
                ("b.rs::function::b".to_string(), 1),
                ("c.rs::function::c".to_string(), 1),
                ("d.rs::function::d".to_string(), 2),
            ]
        );
    }
}
