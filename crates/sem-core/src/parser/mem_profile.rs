//! Opt-in, behavior-neutral memory attribution for `EntityGraph::build`,
//! gated by the `SEM_PROFILE_MEM=1` environment variable.
//!
//! Mirrors `resolve_profile.rs`'s discipline: every public function here is a
//! cheap no-op (a single cached env-var read) unless the variable is set, and
//! nothing in this module changes build output — it only *observes* the
//! approximate heap footprint of the major structures `build_incremental_core`
//! holds live at three phase boundaries (post-pass-1, peak-resolve,
//! post-build), plus the process's actual measured RSS at that instant so the
//! structure-by-structure estimate can be cross-checked against reality.
//!
//! The byte estimates are *approximate on purpose*: `String`/`Vec::capacity`
//! plus a per-entry hash-table overhead constant, not a true allocator-level
//! accounting (which would need `MallocStackLogging`/heaptrack). They are
//! good enough to rank structures by order of magnitude and to attribute a
//! blowout to a specific field (e.g. `SemanticEntity::content`) — that is
//! this instrumentation's whole job. "Memory
//! attribution " for the numbers this produced.

use std::collections::HashMap as StdHashMap;
use std::hash::BuildHasher;
use std::sync::OnceLock;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SEM_PROFILE_MEM").as_deref() == Ok("1"))
}

/// One named structure's byte count, reported at a checkpoint.
pub(crate) struct Entry {
    pub(crate) name: &'static str,
    pub(crate) bytes: usize,
}

pub(crate) fn entry(name: &'static str, bytes: usize) -> Entry {
    Entry { name, bytes }
}

/// Reads this process's *current* resident set size, best-effort, for
/// cross-checking the structure-by-structure estimate against reality.
/// `None` on platforms/errors where this isn't cheaply available — callers
/// must treat that as "not measured," not zero.
///
/// `pub` rather than `pub(crate)` because the facts plane's own phase
/// boundaries are orchestrated in `sem-cli` (`build_graph_with_facts_store`),
/// not here: found the plane costing +9.23 GB of peak RSS on
/// dotnet with no timer and no counter covering it, precisely because the
/// boundaries either side of it are outside this crate. Sampling is still
/// opt-in and still shells out at most a handful of times per build.
pub fn current_rss_bytes() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: usize = rest.trim().split_whitespace().next()?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        // No new crate dependency for this opt-in diagnostic: shell out to
        // `ps`, same source `/usr/bin/time -l`'s own peak-RSS figure in this
        // campaign's gates ultimately reads from. Only ever invoked when
        // `SEM_PROFILE_MEM=1`, at most a few times per build.
        let pid = std::process::id();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let kb: usize = text.trim().parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Prints one checkpoint's attribution table to stderr. No-op unless
/// `enabled()`. Callers should guard the (potentially expensive, O(corpus))
/// computation of `entries` behind `mem_profile::enabled()` themselves so
/// this instrumentation costs exactly one cached bool read when off.
pub(crate) fn checkpoint(label: &str, entries: &[Entry]) {
    if !enabled() {
        return;
    }
    let total: usize = entries.iter().map(|e| e.bytes).sum();
    eprintln!(
        "SEM_PROFILE_MEM[{label}] attributed_total={:.1}MB",
        mb(total)
    );
    for e in entries {
        eprintln!(
            "SEM_PROFILE_MEM[{label}]   {:<28} {:>10.1}MB",
            e.name,
            mb(e.bytes)
        );
    }
    if let Some(rss) = current_rss_bytes() {
        eprintln!(
            "SEM_PROFILE_MEM[{label}] process_rss={:.1}MB (attributed/rss={:.0}%)",
            mb(rss),
            100.0 * total as f64 / rss.max(1) as f64
        );
    } else {
        eprintln!("SEM_PROFILE_MEM[{label}] process_rss=<unavailable>");
    }
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// ---- structure-specific sizers ----
//
// Bespoke rather than blanket-trait for the two entity-shaped structs:
// `SemanticEntity`/`EntityInfo` both live in other modules and the point of
// this pass is to *split* `content` from the rest, which a single
// `approx_heap_bytes() -> usize` can't report on its own.

/// `(content_bytes, metadata_bytes)` — deliberately split because the
/// attribution question this change asks is "how much of `all_entities` is
/// `content` versus everything else." `content_bytes` is exact
/// (`.capacity()` summed); `metadata_bytes` covers every other `String`/
/// `Option<String>`/`metadata` field, also by `.capacity()`.
pub(crate) fn semantic_entities_bytes(
    entities: &[crate::model::entity::SemanticEntity],
) -> (usize, usize) {
    let mut content = 0usize;
    let mut meta = 0usize;
    for e in entities {
        content += e.content.capacity();
        meta += e.id.capacity()
            + e.file_path.capacity()
            + e.entity_type.capacity()
            + e.name.capacity()
            + e.content_hash.capacity();
        meta += e.structural_hash.as_ref().map_or(0, String::capacity);
        meta += e.kappa.as_ref().map_or(0, String::capacity);
        if let Some(md) = &e.metadata {
            // `BTreeMap` has no `capacity()` (no distinct allocation to
            // query, unlike a hash table) — `len()` is the same kind of
            // approximation this function already uses elsewhere.
            meta += md.len() * (std::mem::size_of::<String>() * 2 + 1);
            for (k, v) in md {
                meta += k.capacity() + v.capacity();
            }
        }
    }
    (content, meta + std::mem::size_of_val(entities))
}

pub(crate) fn entity_map_bytes<S: BuildHasher>(
    map: &StdHashMap<String, crate::parser::graph::EntityInfo, S>,
) -> usize {
    let table_overhead = map.capacity()
        * (std::mem::size_of::<String>()
            + std::mem::size_of::<crate::parser::graph::EntityInfo>()
            + 1);
    let entries: usize = map
        .iter()
        .map(|(k, v)| {
            k.capacity()
                + v.id.capacity()
                + v.name.capacity()
                + v.entity_type.capacity()
                + v.file_path.capacity()
                + v.parent_id.as_ref().map_or(0, String::capacity)
        })
        .sum();
    table_overhead + entries
}

/// `scope_consumed_words` shape: `entity_id -> {consumed word, ...}` — a
/// `HashSet`, not a `Vec`, so its per-entry overhead is a full hash-table
/// bucket per word, not just a pointer/len/cap triple. Alive from the end of
/// scope resolution (`resolve_scopes_in_file_chunks`/`resolve_with_scopes_
/// full`) through `resolve_references_with_file_indexes`, which is most of
/// pass 2 — and, before this change, never sized by this instrumentation.
pub(crate) fn string_to_string_set_map_bytes<S: BuildHasher, S2: BuildHasher>(
    map: &StdHashMap<String, std::collections::HashSet<String, S2>, S>,
) -> usize {
    let table_overhead = map.capacity()
        * (std::mem::size_of::<String>()
            + std::mem::size_of::<std::collections::HashSet<String, S2>>()
            + 1);
    let entries: usize = map
        .iter()
        .map(|(k, v)| {
            let set_overhead = v.capacity() * (std::mem::size_of::<String>() + 1);
            k.capacity() + set_overhead + v.iter().map(String::capacity).sum::<usize>()
        })
        .sum();
    table_overhead + entries
}

/// `symbol_table` shape: `name -> [entity_id, ...]`. Also used for
/// `import_scans`/similar `HashMap<String, Vec<String>>` structures.
pub(crate) fn string_to_string_vec_map_bytes<S: BuildHasher>(
    map: &StdHashMap<String, Vec<String>, S>,
) -> usize {
    let table_overhead =
        map.capacity() * (std::mem::size_of::<String>() + std::mem::size_of::<Vec<String>>() + 1);
    let entries: usize = map
        .iter()
        .map(|(k, v)| {
            k.capacity()
                + v.capacity() * std::mem::size_of::<String>()
                + v.iter().map(String::capacity).sum::<usize>()
        })
        .sum();
    table_overhead + entries
}

/// `class_members`/`owner_members` shape: `name -> [(name, entity_id), ...]`.
pub(crate) fn string_to_pair_vec_map_bytes<S: BuildHasher>(
    map: &StdHashMap<String, Vec<(String, String)>, S>,
) -> usize {
    let table_overhead = map.capacity()
        * (std::mem::size_of::<String>() + std::mem::size_of::<Vec<(String, String)>>() + 1);
    let entries: usize = map
        .iter()
        .map(|(k, v)| {
            k.capacity()
                + v.capacity() * std::mem::size_of::<(String, String)>()
                + v.iter()
                    .map(|(a, b)| a.capacity() + b.capacity())
                    .sum::<usize>()
        })
        .sum();
    table_overhead + entries
}

/// `entity_ranges` shape: `file_path -> [(start_line, end_line, entity_id), ...]`.
pub(crate) fn entity_ranges_bytes<S: BuildHasher>(
    map: &StdHashMap<String, Vec<(usize, usize, String)>, S>,
) -> usize {
    let table_overhead = map.capacity()
        * (std::mem::size_of::<String>() + std::mem::size_of::<Vec<(usize, usize, String)>>() + 1);
    let entries: usize = map
        .iter()
        .map(|(k, v)| {
            k.capacity()
                + v.capacity() * std::mem::size_of::<(usize, usize, String)>()
                + v.iter().map(|(_, _, id)| id.capacity()).sum::<usize>()
        })
        .sum();
    table_overhead + entries
}

/// bag-of-words's content map: `&path -> Cow<content>`. A
/// `Cow::Borrowed` entry costs the map slot only — the bytes belong to
/// `precomputed_facts`, already accounted separately — which is exactly the
/// copy `snapshot_bow_content` no longer makes on the chunked path.
pub(crate) fn cow_content_map_bytes<S: BuildHasher>(
    map: &StdHashMap<&str, std::borrow::Cow<'_, str>, S>,
) -> usize {
    let table_overhead =
        map.capacity() * (std::mem::size_of::<&str>() + std::mem::size_of::<String>() + 1);
    let entries: usize = map
        .values()
        .map(|v| match v {
            std::borrow::Cow::Owned(owned) => owned.capacity(),
            std::borrow::Cow::Borrowed(_) => 0,
        })
        .sum();
    table_overhead + entries
}

/// `import_table` shape: `(file_path, imported_name) -> target_entity_id`.
pub(crate) fn pair_key_string_map_bytes<S: BuildHasher>(
    map: &StdHashMap<(String, String), String, S>,
) -> usize {
    let table_overhead = map.capacity()
        * (std::mem::size_of::<(String, String)>() + std::mem::size_of::<String>() + 1);
    let entries: usize = map
        .iter()
        .map(|((a, b), v)| a.capacity() + b.capacity() + v.capacity())
        .sum();
    table_overhead + entries
}

pub(crate) fn precomputed_facts_bytes<S: BuildHasher>(
    map: &StdHashMap<String, crate::parser::scope_resolve::PrecomputedFileFacts, S>,
) -> usize {
    let table_overhead = map.capacity()
        * (std::mem::size_of::<String>()
            + std::mem::size_of::<crate::parser::scope_resolve::PrecomputedFileFacts>()
            + 1);
    let entries: usize = map
        .iter()
        .map(|(k, v)| k.capacity() + v.approx_heap_bytes())
        .sum();
    table_overhead + entries
}

/// Field-by-field breakdown of `precomputed_facts` (Stage 1): which
/// field actually dominates a MUL-admitted language's fast-path facts, rather
/// than only the aggregate `precomputed_facts` entry `checkpoint` already
/// prints. Deliberately its own diagnostic block, not fed through
/// `checkpoint`/its `attributed_total` — that entry's own `precomputed_facts`
/// row already counts every one of these bytes once via
/// `precomputed_facts_bytes`/`approx_heap_bytes`; folding this breakdown into
/// the same running total would double-count them against `process_rss`.
pub(crate) fn precomputed_facts_field_breakdown<S: BuildHasher>(
    label: &str,
    map: &StdHashMap<String, crate::parser::scope_resolve::PrecomputedFileFacts, S>,
) {
    if !enabled() {
        return;
    }
    let totals = map.values().fold(
        crate::parser::scope_resolve::PrecomputedFieldBytes::default(),
        |mut acc, facts| {
            acc.add(&facts.field_heap_bytes());
            acc
        },
    );
    let rows: [(&str, usize); 9] = [
        ("scopes", totals.scopes),
        ("entity_scope_maps", totals.entity_scope_maps),
        ("ast_refs", totals.ast_refs),
        ("return_type_map", totals.return_type_map),
        ("instance_attr_types", totals.instance_attr_types),
        ("init_params", totals.init_params),
        ("attr_to_param", totals.attr_to_param),
        ("import_stmts", totals.import_stmts),
        ("ctor_call_sites", totals.ctor_call_sites),
    ];
    eprintln!(
        "SEM_PROFILE_MEM[{label}] precomputed_facts_fields content={:.1}MB total={:.1}MB",
        mb(totals.content),
        mb(totals.total())
    );
    for (name, bytes) in rows {
        eprintln!(
            "SEM_PROFILE_MEM[{label}] precomputed_facts_fields   {:<20} {:>10.1}MB",
            name,
            mb(bytes)
        );
    }
}

/// Stage-0 instrument (interning-for-memory wave): decomposes
/// each candidate field's total duplicate string bytes into within-file
/// repeats (what a per-file string table, design (a), would reclaim) versus
/// cross-file repeats (the *additional* bytes a corpus-wide interner,
/// design (b), would reclaim on top of (a)). The decomposition is exact:
/// for a value `v` with per-file counts `c_1..c_k` (k = number of distinct
/// files containing `v`, 0 if absent), design (b)'s total saving is
/// `len(v) * (Σc_i - 1)`; design (a)'s saving (dedup within each file only)
/// is `len(v) * Σ(c_i - 1)`; the difference, `len(v) * (k - 1)`, is exactly
/// the cross-file share — so `within_file + cross_file == total_dup` by
/// construction, not by measurement error. No production behavior changes;
/// this only reads strings already held by `precomputed_facts`. Gated
/// behind `SEM_PROFILE_MEM=1` like every other function in this module —
/// callers must not compute the (potentially large) intermediate maps this
/// builds unless `enabled()`.
pub(crate) fn precomputed_facts_duplicate_split<S: BuildHasher>(
    label: &str,
    map: &StdHashMap<String, crate::parser::scope_resolve::PrecomputedFileFacts, S>,
) {
    if !enabled() {
        return;
    }
    type Extractor =
        for<'a> fn(&'a crate::parser::scope_resolve::PrecomputedFileFacts) -> Vec<&'a str>;
    let fields: [(&str, Extractor); 2] = [
        (
            "ast_refs",
            crate::parser::scope_resolve::PrecomputedFileFacts::ast_ref_intern_candidates,
        ),
        (
            "import_stmts",
            crate::parser::scope_resolve::PrecomputedFileFacts::import_stmt_intern_candidates,
        ),
    ];
    for (field_name, extract) in fields {
        let mut global_counts: StdHashMap<&str, u64> = StdHashMap::default();
        let mut within_file_dup_bytes: u64 = 0;
        let mut total_occurrences: u64 = 0;
        let mut total_occurrence_bytes: u64 = 0;

        for facts in map.values() {
            let strings = extract(facts);
            if strings.is_empty() {
                continue;
            }
            let mut local_counts: StdHashMap<&str, u64> = StdHashMap::default();
            for s in &strings {
                *local_counts.entry(s).or_insert(0) += 1;
                total_occurrences += 1;
                total_occurrence_bytes += s.len() as u64;
            }
            for (s, count) in &local_counts {
                if *count > 1 {
                    within_file_dup_bytes += s.len() as u64 * (*count - 1);
                }
                *global_counts.entry(s).or_insert(0) += *count;
            }
        }

        let mut total_dup_bytes: u64 = 0;
        let mut distinct: u64 = 0;
        for (s, count) in &global_counts {
            distinct += 1;
            if *count > 1 {
                total_dup_bytes += s.len() as u64 * (*count - 1);
            }
        }
        let cross_file_dup_bytes = total_dup_bytes.saturating_sub(within_file_dup_bytes);

        let within_pct = if total_dup_bytes > 0 {
            100.0 * within_file_dup_bytes as f64 / total_dup_bytes as f64
        } else {
            0.0
        };
        let cross_pct = if total_dup_bytes > 0 {
            100.0 * cross_file_dup_bytes as f64 / total_dup_bytes as f64
        } else {
            0.0
        };

        eprintln!(
            "SEM_PROFILE_MEM[{label}] dup_split field={field_name} occurrences={total_occurrences} \
             distinct={distinct} total_bytes={:.1}MB total_dup_bytes={:.1}MB \
             within_file_dup_bytes={:.1}MB ({within_pct:.1}%) cross_file_dup_bytes={:.1}MB ({cross_pct:.1}%)",
            mb(total_occurrence_bytes as usize),
            mb(total_dup_bytes as usize),
            mb(within_file_dup_bytes as usize),
            mb(cross_file_dup_bytes as usize),
        );
    }
}

/// `parsed_files` retained trees (`(path, content, tree)` triples, only ever
/// non-empty when `file_paths.len() <= PARSED_FILE_REUSE_LIMIT`). Tree size
/// itself is not walked (tree-sitter doesn't expose it cheaply) — this is
/// path+content only, which is the dominant, attributable term; the tree is
/// noted separately as an unattributed residual where this is used.
/// `graph.edges`: `Vec<EntityRef>` (`from_entity`/`to_entity` Strings + a
/// small enum tag).
pub(crate) fn entity_refs_bytes(edges: &[crate::parser::graph::EntityRef]) -> usize {
    edges
        .iter()
        .map(|e| e.from_entity.capacity() + e.to_entity.capacity())
        .sum::<usize>()
        + std::mem::size_of_val(edges)
}

pub(crate) fn parsed_files_content_bytes(parsed: &[(String, String, tree_sitter::Tree)]) -> usize {
    parsed
        .iter()
        .map(|(path, content, _tree)| path.capacity() + content.capacity())
        .sum::<usize>()
        + std::mem::size_of_val(parsed)
}
