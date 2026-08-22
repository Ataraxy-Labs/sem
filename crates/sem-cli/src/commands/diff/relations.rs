//! The local caller/callee "relations" pass for a diff: given the changed
//! entities in a [`DiffResult`], look up their callers/callees in the repo's
//! dependency graph and shape them into the JSON blob the cloud diff
//! snapshot payload expects.
//!
//! This pass is wall-clock budgeted (see [`relations_time_budget`]) because
//! a cold cache on a very large repo means a full-repo graph build, which
//! can outlast any reasonable wait. [`build_changed_entity_relations`] is
//! the single entry point `diff::cloud_upload` calls; everything else here
//! is private support for it.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use sem_core::git::bridge::GitBridge;
use sem_core::parser::differ::DiffResult;

/// `true` when the relations payload has nothing to report — either no
/// callers/callees were found, or the budgeted pass timed out (in which case
/// [`build_changed_entity_relations`] has already printed the degradation
/// warning).
pub(super) fn relations_is_empty(relations: &serde_json::Value) -> bool {
    relations.as_object().is_none_or(|m| m.is_empty())
}

/// Build a `{ "<file>::<name>": { callers, callees } }` map of real graph
/// relations for each changed entity, for the cloud diff snapshot payload.
///
/// Best-effort: reuses the cached `get_or_build_graph` path (never forces a full
/// rebuild when a cache exists) and swallows every failure — including panics —
/// so it can neither break nor visibly slow `sem diff`. On any failure it
/// returns an empty object and relations are simply omitted.
///
/// The pass is also wall-clock bounded (see [`relations_time_budget`]). A cold
/// cache on a very large repo means a full-repo graph build, which can outlast
/// any reasonable wait; when that happens the snapshot uploads without relations
/// and the degradation is reported on stderr rather than silently stalling the
/// command after its output has already been printed.
pub(super) fn build_changed_entity_relations(
    cwd: &str,
    file_exts: &[String],
    result: &DiffResult,
    budget_override_ms: Option<u64>,
) -> serde_json::Value {
    let empty = || serde_json::Value::Object(serde_json::Map::new());

    // Own every input so the work can run on a detached thread: the budget is
    // only enforceable if we can stop waiting on a pass we cannot interrupt.
    let input = RelationsInput {
        cwd: cwd.to_string(),
        file_exts: file_exts.to_vec(),
        changed: result
            .changes
            .iter()
            .map(|c| (c.file_path.clone(), c.entity_name.clone()))
            .collect(),
    };

    let (tx, rx) = std::sync::mpsc::channel();
    if std::thread::Builder::new()
        .name("sem-relations".into())
        .spawn(move || {
            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                build_relations_from_graph(&input)
            }))
            .ok()
            .flatten();
            let _ = tx.send(built);
        })
        .is_err()
    {
        return empty();
    }

    let (budget, source) = relations_time_budget(cwd, budget_override_ms);
    match rx.recv_timeout(budget) {
        Ok(Some(relations)) => relations,
        Ok(None) => empty(),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            eprintln!("{}", format_budget_timeout_warning(budget, &source));
            empty()
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => empty(),
    }
}

/// Owned inputs for the relations pass — everything it needs from the diff.
struct RelationsInput {
    cwd: String,
    file_exts: Vec<String>,
    /// `(file_path, entity_name)` for every changed entity, in diff order.
    changed: Vec<(String, String)>,
}

/// Where a chosen [`relations_time_budget`] value came from — surfaced in the
/// degradation warning so users see *why* that particular number was picked.
enum RelationsBudgetSource {
    /// `SEM_RELATIONS_BUDGET_MS` was set and always wins verbatim.
    EnvOverride,
    /// Adaptive default, scaled from a tracked-file-count estimate.
    Adaptive { tracked_files: Option<usize> },
}

impl RelationsBudgetSource {
    fn describe(&self) -> String {
        match self {
            RelationsBudgetSource::EnvOverride => "SEM_RELATIONS_BUDGET_MS override".to_string(),
            RelationsBudgetSource::Adaptive {
                tracked_files: Some(n),
            } => format!("adaptive default for ~{n} tracked files"),
            RelationsBudgetSource::Adaptive { tracked_files: None } => {
                "adaptive default, repo size unknown".to_string()
            }
        }
    }
}

/// The message printed to stderr when the relations pass exceeds its budget.
/// Split out from [`build_changed_entity_relations`] so the exact text —
/// including the adaptive-budget source description — is unit-testable
/// without needing a real timeout.
fn format_budget_timeout_warning(
    budget: std::time::Duration,
    source: &RelationsBudgetSource,
) -> String {
    format!(
        "warning: cloud snapshot uploaded without caller/callee relations — \
         cross-referencing this repo exceeded the {}s budget ({}) (cold graph cache on a very large repo). \
         Run `sem graph` once to warm the cache, or raise SEM_RELATIONS_BUDGET_MS.",
        budget.as_secs(),
        source.describe(),
    )
}

/// Wall-clock ceiling for the relations pass. Repos that finish normally are
/// nowhere near this; the budget only bites on cold full-repo graph builds.
///
/// `override_ms` — resolved once from `SEM_RELATIONS_BUDGET_MS` at the diff
/// command's cloud-upload edge (`cloud_upload::DiffCloudContext::resolve`),
/// not read here — always wins verbatim when present: no curve, no clamping,
/// whatever value the user gave.
///
/// Otherwise the default is adaptive to repo size, via a cheap tracked-file
/// count (see [`GitBridge::tracked_file_count`], an in-process read of the
/// git index — no shell-out, no working-tree walk). See
/// [`adaptive_relations_budget_secs`] for the curve and its breakpoints.
fn relations_time_budget(
    cwd: &str,
    override_ms: Option<u64>,
) -> (std::time::Duration, RelationsBudgetSource) {
    if let Some(ms) = override_ms {
        return (
            std::time::Duration::from_millis(ms),
            RelationsBudgetSource::EnvOverride,
        );
    }

    let tracked_files = estimate_tracked_file_count(cwd);
    let secs = adaptive_relations_budget_secs(tracked_files.unwrap_or(0) as u64);
    (
        std::time::Duration::from_secs(secs),
        RelationsBudgetSource::Adaptive { tracked_files },
    )
}

/// Cheap repo-size estimate used only to pick a budget, never for correctness.
///
/// Prefers the git index via [`GitBridge::tracked_file_count`] (equivalent to
/// `git ls-files | wc -l`, done in-process with git2 — effectively O(1), no
/// working-tree walk). Falls back to the same tracked-path enumeration the
/// relations pass itself uses ([`super::graph::find_supported_files_with_options`])
/// only if the git index can't be read at all (e.g. a corrupt or unusual repo).
fn estimate_tracked_file_count(cwd: &str) -> Option<usize> {
    if let Some(count) = GitBridge::open(Path::new(cwd))
        .ok()
        .and_then(|git| git.tracked_file_count())
    {
        return Some(count);
    }

    let root = Path::new(cwd);
    let registry = super::super::create_registry(cwd);
    let file_paths =
        super::super::graph::find_supported_files_with_options(root, &registry, &[], false);
    (!file_paths.is_empty()).then_some(file_paths.len())
}

/// The adaptive relations-budget curve, in seconds, as a function of tracked
/// file count. Pure and unit-tested in isolation from env/git.
///
/// Breakpoints (deliberately round numbers):
///   - `<= 10_000` tracked files: **90s floor** — unchanged from the fixed
///     default this replaces, so small/medium repos see no behavior change.
///   - `10_000..100_000` tracked files: linear ramp from 90s up to 480s.
///   - `>= 100_000` tracked files: **480s (8min) cap** — large/mono-repos get
///     enough time for a cold full-repo graph build without hanging forever.
fn adaptive_relations_budget_secs(tracked_files: u64) -> u64 {
    const FLOOR_SECS: u64 = 90;
    const CAP_SECS: u64 = 8 * 60;
    const FLOOR_FILES: u64 = 10_000;
    const CAP_FILES: u64 = 100_000;

    if tracked_files <= FLOOR_FILES {
        return FLOOR_SECS;
    }
    if tracked_files >= CAP_FILES {
        return CAP_SECS;
    }

    let frac = (tracked_files - FLOOR_FILES) as f64 / (CAP_FILES - FLOOR_FILES) as f64;
    FLOOR_SECS + ((CAP_SECS - FLOOR_SECS) as f64 * frac).round() as u64
}

fn build_relations_from_graph(input: &RelationsInput) -> Option<serde_json::Value> {
    let git = GitBridge::open(Path::new(&input.cwd)).ok()?;
    let root = git.repo_root().to_path_buf();
    let root = root.as_path();
    let registry = super::super::create_registry(&root.to_string_lossy());
    let ext_filter = super::super::graph::normalize_exts(&input.file_exts);
    let source_scope = super::super::graph::cache_source_scope(root, &ext_filter, false);
    let file_paths = super::super::graph::find_supported_files_with_options(
        root,
        &registry,
        &ext_filter,
        false,
    );
    // no_cache = false: reuse the disk cache; do not force a full rebuild.
    let (graph, _entities) =
        super::super::graph::get_or_build_graph(root, &file_paths, &registry, false, source_scope);

    // Set of (file_path, name) for every changed entity — used to flag
    // whether a related entity is itself in the diff.
    let changed: HashSet<(&str, &str)> = input
        .changed
        .iter()
        .map(|(file, name)| (file.as_str(), name.as_str()))
        .collect();

    let related_json = |e: &sem_core::parser::graph::EntityInfo| {
        serde_json::json!({
            "name": e.name,
            "file": e.file_path,
            "entityType": e.entity_type,
            "inDiff": changed.contains(&(e.file_path.as_str(), e.name.as_str())),
        })
    };

    // Index entities by (file_path, name) once instead of rescanning every
    // entity in the repo for every changed entity. The old nested scan was
    // O(changes x entities): on a repo the size of microsoft/TypeScript that is
    // hundreds of millions of string comparisons. `None` marks a key that more
    // than one entity claims, preserving the "skip when ambiguous" rule exactly.
    let mut by_key: HashMap<(&str, &str), Option<&sem_core::parser::graph::EntityInfo>> =
        HashMap::with_capacity(graph.entities.len());
    for entity in graph.entities.values() {
        by_key
            .entry((entity.file_path.as_str(), entity.name.as_str()))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(entity));
    }

    let mut relations = serde_json::Map::new();
    for (file_path, entity_name) in &input.changed {
        // Match graph entities by file_path + name; skip when ambiguous.
        let Some(Some(entity)) = by_key.get(&(file_path.as_str(), entity_name.as_str())) else {
            continue;
        };

        let callers: Vec<serde_json::Value> = graph
            .get_dependents(&entity.id)
            .into_iter()
            .take(6)
            .map(related_json)
            .collect();
        let callees: Vec<serde_json::Value> = graph
            .get_dependencies(&entity.id)
            .into_iter()
            .take(6)
            .map(related_json)
            .collect();

        if callers.is_empty() && callees.is_empty() {
            continue;
        }

        relations.insert(
            format!("{file_path}::{entity_name}"),
            serde_json::json!({ "callers": callers, "callees": callees }),
        );
    }

    Some(serde_json::Value::Object(relations))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_below_and_at_breakpoint() {
        assert_eq!(adaptive_relations_budget_secs(0), 90);
        assert_eq!(adaptive_relations_budget_secs(1_000), 90);
        assert_eq!(adaptive_relations_budget_secs(10_000), 90);
    }

    #[test]
    fn cap_at_and_above_breakpoint() {
        assert_eq!(adaptive_relations_budget_secs(100_000), 480);
        assert_eq!(adaptive_relations_budget_secs(1_000_000), 480);
    }

    #[test]
    fn midpoint_is_roughly_halfway() {
        // Halfway between 10k and 100k files -> halfway between 90s and 480s.
        let mid = adaptive_relations_budget_secs(55_000);
        assert_eq!(mid, 90 + (480 - 90) / 2);
    }

    #[test]
    fn monotonically_non_decreasing_across_the_ramp() {
        let mut prev = 0;
        for files in (0..=120_000).step_by(1_000) {
            let secs = adaptive_relations_budget_secs(files);
            assert!(secs >= prev, "budget decreased at {files} files");
            assert!((90..=480).contains(&secs));
            prev = secs;
        }
    }

    /// Regression coverage for the degradation warning users actually see
    /// on a timeout: the adaptive-budget case must name both the seconds
    /// waited and *why* that number was chosen ("adaptive default for ~N
    /// tracked files"), not just print a bare number.
    #[test]
    fn timeout_warning_names_the_adaptive_budget_and_tracked_file_count() {
        let warning = format_budget_timeout_warning(
            std::time::Duration::from_secs(90),
            &RelationsBudgetSource::Adaptive {
                tracked_files: Some(42_000),
            },
        );

        assert!(
            warning.contains("exceeded the 90s budget"),
            "expected the budget seconds in the warning: {warning}"
        );
        assert!(
            warning.contains("adaptive default for ~42000 tracked files"),
            "expected the adaptive-budget source description: {warning}"
        );
    }

    #[test]
    fn timeout_warning_reports_unknown_repo_size_when_estimate_is_unavailable() {
        let warning = format_budget_timeout_warning(
            std::time::Duration::from_secs(90),
            &RelationsBudgetSource::Adaptive { tracked_files: None },
        );

        assert!(
            warning.contains("adaptive default, repo size unknown"),
            "expected the repo-size-unknown source description: {warning}"
        );
    }

    #[test]
    fn timeout_warning_names_the_env_override_when_budget_was_forced() {
        let warning = format_budget_timeout_warning(
            std::time::Duration::from_millis(5000),
            &RelationsBudgetSource::EnvOverride,
        );

        assert!(
            warning.contains("exceeded the 5s budget (SEM_RELATIONS_BUDGET_MS override)"),
            "expected the env-override source description: {warning}"
        );
    }
}
