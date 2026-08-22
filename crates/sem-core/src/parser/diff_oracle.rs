//! The diff-equivalence oracle: the shipping gate for any [`FastExtractor`].
//!
//! # Why the gate lives here and not at the hash level
//!
//! declined an oxc fast path because `structural_hash` is
//! defined by a walk over tree-sitter's *concrete* syntax tree — every
//! grammar production, including anonymous punctuation — and a typed AST has
//! no such nodes to walk. That is true and unfixable, and it is also the
//! wrong bar. `structural_hash` is not a product; it is one input to a
//! product. What a user sees is a `sem diff`: a set of entities, a change
//! classification per entity, and a rendered result.
//!
//! So this module defines equivalence *there*:
//!
//! > Two extractors are equivalent on a change set iff they produce
//! > (1) the same entity set — same id, kind, name, parent and span — on
//! > every side of every file, (2) in the same extraction order, (3) inducing
//! > the same kappa partition, (4) the same [`DiffResult`], and (5) the same
//! > rendered `sem diff --json` envelope.
//!
//! Each is a separate [`Layer`] so a failure names itself. (1)-(3) localize a
//! failure to one file and one entity; (4) and (5) are what actually ships.
//! (5) is a strict projection of (4), kept separate because the JSON envelope
//! deliberately omits fields (`id`, `entityLine`, `parentName`, `timestamp`,
//! `totalEntitiesBefore/After`), so a divergence that (4) sees and (5) does
//! not is a *real* divergence that happens to be invisible today — worth
//! knowing, not worth shipping blind.
//!
//! (3) compares kappa as an *equivalence relation*, not as hash values. That
//! is not a softening: kappa's values are defined by a walk over tree-sitter
//! node-kind strings, so they are grammar-shaped, and demanding value equality
//! across parsers would demand CST fidelity through the back door — the exact
//! thing proved unreachable. What kappa is *for* is deciding
//! which entities share a semantic identity; that is what gets compared.
//!
//! # Vacuity
//!
//! An extractor that declines every file trivially satisfies all three
//! layers. Every run therefore reports `claimed`/`served` counts from
//! [`fast_extractor::stats`], and [`OracleRun::verdict`] returns
//! [`Verdict::Vacuous`] — never [`Verdict::Equivalent`] — when nothing was
//! served. A gate that can be passed by doing nothing is not a gate.
//!
//! # Self-proof
//!
//! The oracle is only worth its verdict if it fails when it should. The
//! [`Mutation`] extractors below wrap the real tree-sitter extraction and
//! perturb it in one specific way each; the tests in this module assert that
//! [`Mutation::Faithful`] passes and every other mutation is caught, naming
//! which layer catches it. That is the mutation test for the gate itself.

use std::sync::Arc;

use serde::Serialize;

use crate::format::json::format_diff_json_with_binary_changes;
use crate::git::types::FileChange;
use crate::model::entity::{disambiguate_colliding_entity_ids, SemanticEntity};
use crate::parser::differ::{collect_binary_file_changes, compute_semantic_diff, DiffResult};
use crate::parser::fast_extractor::{self, FastExtractor, FastExtractorSet};
use crate::parser::registry::ParserRegistry;

// ---------------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------------

/// The identity of one entity, as the oracle compares it.
///
/// Deliberately excludes `content_hash`, `structural_hash` **and the kappa
/// value**: those are per-parser hash *conventions*, not user-visible facts,
/// and requiring their values to match is exactly the field-identity bar
/// proved unreachable. Their *behaviour* is covered
/// elsewhere and in full — `structural_hash` by every decision it drives
/// landing in the `DiffResult` layer, and kappa by the
/// [`Layer::KappaPartition`] check, which compares the equivalence relation
/// kappa induces rather than the hashes that encode it. (See
/// errata: kappa's values are grammar-shaped, so demanding value
/// equality across parsers demands CST fidelity through the back door.)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct EntityFingerprint {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: Option<usize>,
    pub end_byte: Option<usize>,
}

impl From<&SemanticEntity> for EntityFingerprint {
    fn from(e: &SemanticEntity) -> Self {
        Self {
            id: e.id.clone(),
            kind: e.entity_type.clone(),
            name: e.name.clone(),
            parent_id: e.parent_id.clone(),
            start_line: e.start_line,
            end_line: e.end_line,
            start_byte: e.start_byte,
            end_byte: e.end_byte,
        }
    }
}

fn fingerprints(entities: &[SemanticEntity]) -> Vec<EntityFingerprint> {
    entities.iter().map(EntityFingerprint::from).collect()
}

/// One `(file, side)` slot's extracted entities.
#[derive(Debug, Clone)]
struct SideEntities {
    file_path: String,
    side: &'static str,
    entities: Vec<EntityFingerprint>,
    /// This side's kappa values, positionally aligned with `entities`.
    kappas: Vec<Option<String>>,
}

/// Everything one leg of the oracle produced.
struct Snapshot {
    sides: Vec<SideEntities>,
    diff: DiffResult,
    rendered: String,
}

/// The equivalence relation kappa induces over every entity in a run,
/// expressed as positions so it can be compared across two extractors whose
/// hash *values* legitimately differ.
///
/// Each class is a sorted list of `(side_index, entity_index)`; the classes
/// themselves are sorted. Two extractors agree iff they group the same
/// entities together, whatever they call the groups.
fn kappa_partition(sides: &[SideEntities]) -> Vec<Vec<(usize, usize)>> {
    use std::collections::HashMap;
    let mut classes: HashMap<Option<&str>, Vec<(usize, usize)>> = HashMap::new();
    for (s, side) in sides.iter().enumerate() {
        for (e, kappa) in side.kappas.iter().enumerate() {
            classes.entry(kappa.as_deref()).or_default().push((s, e));
        }
    }
    let mut out: Vec<Vec<(usize, usize)>> = classes.into_values().collect();
    for class in &mut out {
        class.sort_unstable();
    }
    out.sort_unstable();
    out
}

// ---------------------------------------------------------------------------
// Divergences
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    /// The entity sets themselves disagree (id/kind/name/parent/span), as
    /// multisets — i.e. an entity is present on one leg and not the other.
    EntitySet,
    /// The entity multisets agree but their *order* does not. Invisible to
    /// `sem diff` (which sorts by line), load-bearing for the graph path:
    /// `precompute_js_ts_file_facts` requires "this file's own entities, in
    /// extraction order", and scope registration walks them in that order.
    EntityOrder,
    /// The equivalence relation kappa induces disagrees — two entities are
    /// the same identity on one leg and different identities on the other,
    /// or vice versa. Compared as a partition, never as hash values.
    KappaPartition,
    /// The computed `DiffResult` disagrees (counts, classifications, spans).
    DiffResult,
    /// The rendered `sem diff --json` envelope disagrees.
    RenderedJson,
}

#[derive(Debug, Clone, Serialize)]
pub struct Divergence {
    pub layer: Layer,
    /// Where it was found: a file path, a summary field, a change index.
    pub scope: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Every layer agreed *and* the fast path actually answered files.
    Equivalent,
    /// Every layer agreed, but nothing was served — proves nothing.
    Vacuous,
    /// At least one layer disagreed.
    Divergent,
}

/// One oracle run over one change set.
#[derive(Debug, Clone, Serialize)]
pub struct OracleRun {
    pub label: String,
    pub file_count: usize,
    pub entity_count: usize,
    pub claimed: u64,
    pub served: u64,
    pub divergences: Vec<Divergence>,
}

impl OracleRun {
    pub fn verdict(&self) -> Verdict {
        if !self.divergences.is_empty() {
            Verdict::Divergent
        } else if self.served == 0 {
            Verdict::Vacuous
        } else {
            Verdict::Equivalent
        }
    }

    /// One line, suitable for a probe's stdout.
    pub fn summary_line(&self) -> String {
        format!(
            "{:?} label={} files={} entities={} claimed={} served={} divergences={}",
            self.verdict(),
            self.label,
            self.file_count,
            self.entity_count,
            self.claimed,
            self.served,
            self.divergences.len()
        )
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Restores the fast-path switch on the way out, including on panic, so a
/// failed oracle run cannot leave the process in the other leg's state.
struct EnabledGuard(bool);

impl Drop for EnabledGuard {
    fn drop(&mut self) {
        fast_extractor::set_enabled(self.0);
    }
}

/// Run both legs of the oracle over one change set and compare them.
///
/// Leg A runs with the fast path forced off (the tree-sitter baseline); leg B
/// runs with it forced on. Both legs run in this process, sequentially. That
/// is only sound because [`crate::parser::cache`] folds
/// [`fast_extractor::identity_salt`] into its key, so leg B cannot be served
/// leg A's cached entities; without that, this function would silently
/// compare a snapshot against itself and always pass.
pub fn run(file_changes: &[FileChange], registry: &ParserRegistry, label: &str) -> OracleRun {
    let restore = EnabledGuard(fast_extractor::enabled());

    // Both legs must actually extract. The parse cache keys on the extractor
    // identity, so it cannot serve leg A's entities to leg B — but it *can*
    // serve leg B's own entities from an earlier oracle run in the same
    // process, which would leave the fast path un-consulted and make a real
    // run look vacuous. Clearing before each leg makes `served` an exact
    // count of files the fast path answered, not a count of cache misses.
    crate::parser::cache::clear();
    fast_extractor::set_enabled(false);
    let baseline = snapshot(file_changes, registry);

    crate::parser::cache::clear();
    fast_extractor::reset_stats();
    fast_extractor::set_enabled(true);
    let candidate = snapshot(file_changes, registry);
    let (claimed, served) = fast_extractor::stats();

    drop(restore);

    let divergences = compare(&baseline, &candidate);
    let entity_count = baseline.sides.iter().map(|s| s.entities.len()).sum();

    OracleRun {
        label: label.to_string(),
        file_count: file_changes.len(),
        entity_count,
        claimed,
        served,
        divergences,
    }
}

fn snapshot(file_changes: &[FileChange], registry: &ParserRegistry) -> Snapshot {
    let mut sides = Vec::new();
    for file in file_changes {
        let content_hint = file
            .after_content
            .as_deref()
            .or(file.before_content.as_deref())
            .unwrap_or("");
        let resolved = registry.resolve_file_path(&file.file_path);
        let detection_path = resolved.as_deref().unwrap_or(&file.file_path);
        let Some(plugin) = registry.get_plugin_with_content(detection_path, content_hint) else {
            continue;
        };

        if let Some(content) = file.before_content.as_deref() {
            let before_path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
            let before_resolved = registry.resolve_file_path(before_path);
            let before_detection = before_resolved.as_deref().unwrap_or(before_path);
            let mut extracted = plugin.extract_entities(content, before_detection);
            disambiguate_colliding_entity_ids(&mut extracted);
            sides.push(SideEntities {
                file_path: before_detection.to_string(),
                side: "before",
                entities: fingerprints(&extracted),
                kappas: extracted.iter().map(|e| e.kappa.clone()).collect(),
            });
        }
        if let Some(content) = file.after_content.as_deref() {
            let mut extracted = plugin.extract_entities(content, detection_path);
            disambiguate_colliding_entity_ids(&mut extracted);
            sides.push(SideEntities {
                file_path: detection_path.to_string(),
                side: "after",
                entities: fingerprints(&extracted),
                kappas: extracted.iter().map(|e| e.kappa.clone()).collect(),
            });
        }
    }

    let diff = compute_semantic_diff(file_changes, registry, None, None);
    let binary = collect_binary_file_changes(file_changes);
    let rendered = format_diff_json_with_binary_changes(&diff, &binary);

    Snapshot {
        sides,
        diff,
        rendered,
    }
}

fn compare(baseline: &Snapshot, candidate: &Snapshot) -> Vec<Divergence> {
    let mut out = Vec::new();
    compare_entity_sets(baseline, candidate, &mut out);
    compare_diff_results(baseline, candidate, &mut out);
    if baseline.rendered != candidate.rendered {
        out.push(Divergence {
            layer: Layer::RenderedJson,
            scope: "envelope".to_string(),
            detail: first_text_difference(&baseline.rendered, &candidate.rendered),
        });
    }
    out
}

fn compare_entity_sets(baseline: &Snapshot, candidate: &Snapshot, out: &mut Vec<Divergence>) {
    if baseline.sides.len() != candidate.sides.len() {
        out.push(Divergence {
            layer: Layer::EntitySet,
            scope: "sides".to_string(),
            detail: format!(
                "extracted {} file-sides on the baseline leg, {} on the candidate leg",
                baseline.sides.len(),
                candidate.sides.len()
            ),
        });
        return;
    }
    for (b, c) in baseline.sides.iter().zip(candidate.sides.iter()) {
        if b.entities == c.entities {
            continue;
        }
        let scope = format!("{} ({})", b.file_path, b.side);
        let mut bs = b.entities.clone();
        let mut cs = c.entities.clone();
        bs.sort_unstable();
        cs.sort_unstable();
        if bs == cs {
            out.push(Divergence {
                layer: Layer::EntityOrder,
                scope,
                detail: format!(
                    "same {} entities, different extraction order",
                    b.entities.len()
                ),
            });
            continue;
        }
        if b.entities.len() != c.entities.len() {
            out.push(Divergence {
                layer: Layer::EntitySet,
                scope: scope.clone(),
                detail: format!(
                    "{} entities on the baseline leg, {} on the candidate leg",
                    b.entities.len(),
                    c.entities.len()
                ),
            });
        }
        if let Some(only_baseline) = bs.iter().find(|e| !cs.contains(e)) {
            out.push(Divergence {
                layer: Layer::EntitySet,
                scope: scope.clone(),
                detail: format!("only the baseline leg produced {only_baseline:?}"),
            });
        }
        if let Some(only_candidate) = cs.iter().find(|e| !bs.contains(e)) {
            out.push(Divergence {
                layer: Layer::EntitySet,
                scope,
                detail: format!("only the candidate leg produced {only_candidate:?}"),
            });
        }
    }

    let bp = kappa_partition(&baseline.sides);
    let cp = kappa_partition(&candidate.sides);
    if bp != cp {
        let detail = match bp.len() == cp.len() {
            true => format!(
                "{} kappa classes on both legs, but grouping differs; first differing class: baseline {:?} vs candidate {:?}",
                bp.len(),
                bp.iter().zip(cp.iter()).find(|(a, b)| a != b).map(|(a, _)| a),
                bp.iter().zip(cp.iter()).find(|(a, b)| a != b).map(|(_, b)| b),
            ),
            false => format!(
                "{} kappa classes on the baseline leg, {} on the candidate leg",
                bp.len(),
                cp.len()
            ),
        };
        out.push(Divergence {
            layer: Layer::KappaPartition,
            scope: "run".to_string(),
            detail,
        });
    }
}

fn compare_diff_results(baseline: &Snapshot, candidate: &Snapshot, out: &mut Vec<Divergence>) {
    let b = &baseline.diff;
    let c = &candidate.diff;
    let counters: [(&str, usize, usize); 10] = [
        ("fileCount", b.file_count, c.file_count),
        ("added", b.added_count, c.added_count),
        ("modified", b.modified_count, c.modified_count),
        ("deleted", b.deleted_count, c.deleted_count),
        ("moved", b.moved_count, c.moved_count),
        ("renamed", b.renamed_count, c.renamed_count),
        ("reordered", b.reordered_count, c.reordered_count),
        ("orphan", b.orphan_count, c.orphan_count),
        (
            "totalEntitiesBefore",
            b.total_entities_before,
            c.total_entities_before,
        ),
        (
            "totalEntitiesAfter",
            b.total_entities_after,
            c.total_entities_after,
        ),
    ];
    for (name, bv, cv) in counters {
        if bv != cv {
            out.push(Divergence {
                layer: Layer::DiffResult,
                scope: format!("summary.{name}"),
                detail: format!("baseline {bv}, candidate {cv}"),
            });
        }
    }

    if b.changes.len() != c.changes.len() {
        out.push(Divergence {
            layer: Layer::DiffResult,
            scope: "changes".to_string(),
            detail: format!(
                "{} changes on the baseline leg, {} on the candidate leg",
                b.changes.len(),
                c.changes.len()
            ),
        });
    }
    for (i, (bc, cc)) in b.changes.iter().zip(c.changes.iter()).enumerate() {
        let bv = serde_json::to_value(bc).unwrap_or(serde_json::Value::Null);
        let cv = serde_json::to_value(cc).unwrap_or(serde_json::Value::Null);
        if bv != cv {
            out.push(Divergence {
                layer: Layer::DiffResult,
                scope: format!("changes[{i}] {}", bc.entity_id),
                detail: first_json_field_difference(&bv, &cv),
            });
            break;
        }
    }
}

fn first_json_field_difference(b: &serde_json::Value, c: &serde_json::Value) -> String {
    if let (Some(bo), Some(co)) = (b.as_object(), c.as_object()) {
        for (k, bv) in bo {
            let cv = co.get(k).unwrap_or(&serde_json::Value::Null);
            if bv != cv {
                return format!("field `{k}`: baseline {bv}, candidate {cv}");
            }
        }
    }
    "objects differ".to_string()
}

fn first_text_difference(b: &str, c: &str) -> String {
    let at = b
        .as_bytes()
        .iter()
        .zip(c.as_bytes().iter())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| b.len().min(c.len()));
    let window = 120;
    let start = at.saturating_sub(window / 2);
    format!(
        "first difference at byte {at}: baseline …{}… vs candidate …{}…",
        b.get(start..(start + window).min(b.len())).unwrap_or(""),
        c.get(start..(start + window).min(c.len())).unwrap_or("")
    )
}

// ---------------------------------------------------------------------------
// Mutation extractors — the oracle's own proof of life
// ---------------------------------------------------------------------------

/// A single, deliberate way of being wrong.
///
/// Each variant wraps the real tree-sitter extraction and perturbs exactly
/// one observable property, so that "the oracle catches X" is a statement
/// about X and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Return the tree-sitter result unchanged. The oracle must PASS — this
    /// is the false-positive control, and without it a permanently-failing
    /// oracle would look like a working one.
    Faithful,
    /// Drop the last entity of every file. Models an extractor that misses a
    /// construct.
    DropLastEntity,
    /// Shift every entity's end line by one. Models an off-by-one span rule.
    ShiftSpan,
    /// Suffix every entity name (and its id) with `_x`. Models a naming-rule
    /// mismatch.
    RenameEntities,
    /// Blank `structural_hash`. Models an extractor that cannot produce a
    /// rename-insensitive structural signal — the exact failure mode
    /// predicted for an AST-based path.
    DropStructuralHash,
    /// Blank `kappa`. Models an extractor that gets the diff right but loses
    /// the parser-independent identity the facts layer stores.
    DropKappa,
    /// Merge multi-declarator entities back into one. Models dropping the
    /// `const a = 1, b = 2` splitting rule specifically.
    MergeDeclarators,
}

impl Mutation {
    fn identity(self) -> &'static str {
        match self {
            Mutation::Faithful => "mutant-faithful",
            Mutation::DropLastEntity => "mutant-drop-last",
            Mutation::ShiftSpan => "mutant-shift-span",
            Mutation::RenameEntities => "mutant-rename",
            Mutation::DropStructuralHash => "mutant-drop-structural-hash",
            Mutation::DropKappa => "mutant-drop-kappa",
            Mutation::MergeDeclarators => "mutant-merge-declarators",
        }
    }

    /// Parse a `--mutate` argument.
    pub fn parse(s: &str) -> Option<Mutation> {
        Some(match s {
            "faithful" => Mutation::Faithful,
            "drop-last" => Mutation::DropLastEntity,
            "shift-span" => Mutation::ShiftSpan,
            "rename" => Mutation::RenameEntities,
            "drop-structural-hash" => Mutation::DropStructuralHash,
            "drop-kappa" => Mutation::DropKappa,
            "merge-declarators" => Mutation::MergeDeclarators,
            _ => return None,
        })
    }

    /// Whether this mutation must produce a divergence on *any* corpus that
    /// yielded at least one entity.
    ///
    /// `DropStructuralHash` only shows up once a matched entity's content
    /// actually changed (the cosmetic verdict is `None` otherwise), and
    /// `MergeDeclarators` only shows up in files that contain a
    /// multi-declarator statement. A corpus that exercises neither makes those
    /// mutations *inert*, which is a fact about the corpus, not a failure of
    /// the oracle — the unit tests in this module pin those two against a
    /// fixture built to contain the construct.
    pub fn always_observable(self) -> bool {
        matches!(
            self,
            Mutation::DropLastEntity
                | Mutation::ShiftSpan
                | Mutation::RenameEntities
                | Mutation::DropKappa
        )
    }

    /// Every mutation, for a probe that wants to sweep them.
    pub const ALL: &'static [Mutation] = &[
        Mutation::Faithful,
        Mutation::DropLastEntity,
        Mutation::ShiftSpan,
        Mutation::RenameEntities,
        Mutation::DropStructuralHash,
        Mutation::DropKappa,
        Mutation::MergeDeclarators,
    ];
}

/// A [`FastExtractor`] that produces the tree-sitter answer, then breaks it in
/// one named way. Test scaffolding for the oracle — never a shipping path.
pub struct MutatingExtractor {
    mutation: Mutation,
    /// When set, only paths containing this substring are claimed, so a test
    /// that flips the process-global switch cannot perturb another test's
    /// files.
    scope: Option<String>,
}

impl MutatingExtractor {
    pub fn new(mutation: Mutation) -> Self {
        Self {
            mutation,
            scope: None,
        }
    }

    pub fn scoped(mutation: Mutation, scope: &str) -> Self {
        Self {
            mutation,
            scope: Some(scope.to_string()),
        }
    }

    /// Install this mutant as the process's only fast extractor.
    pub fn install(self) -> Option<Arc<FastExtractorSet>> {
        fast_extractor::install(Some(Arc::new(FastExtractorSet::new(vec![Box::new(self)]))))
    }
}

impl FastExtractor for MutatingExtractor {
    fn identity(&self) -> &str {
        self.mutation.identity()
    }

    fn claims(&self, file_path: &str) -> bool {
        if !fast_extractor::is_js_ts_path(file_path) {
            return false;
        }
        match &self.scope {
            Some(s) => file_path.contains(s.as_str()),
            None => true,
        }
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<Vec<SemanticEntity>> {
        use crate::parser::plugin::SemanticParserPlugin;
        let plugin = crate::parser::plugins::code::CodeParserPlugin;
        let mut entities = plugin.extract_entities_with_tree(content, file_path).0;
        match self.mutation {
            Mutation::Faithful => {}
            Mutation::DropLastEntity => {
                entities.pop();
            }
            Mutation::ShiftSpan => {
                for e in &mut entities {
                    e.end_line += 1;
                }
            }
            Mutation::RenameEntities => {
                for e in &mut entities {
                    e.name.push_str("_x");
                    e.id.push_str("_x");
                }
            }
            Mutation::DropStructuralHash => {
                for e in &mut entities {
                    e.structural_hash = None;
                }
            }
            Mutation::DropKappa => {
                for e in &mut entities {
                    e.kappa = None;
                }
            }
            Mutation::MergeDeclarators => {
                let mut seen: std::collections::HashSet<(usize, usize)> =
                    std::collections::HashSet::new();
                entities
                    .retain(|e| seen.insert((e.start_line, e.end_line)) || e.parent_id.is_some());
            }
        }
        Some(entities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::FileStatus;
    use crate::parser::plugins::create_default_registry;

    use crate::parser::fast_extractor::TEST_LOCK as ORACLE_LOCK;

    const SCOPE: &str = "oracle_fixture";

    fn before_src() -> &'static str {
        r#"
export interface Shape {
  readonly kind: string;
  area(): number;
}

export class Circle implements Shape {
  readonly kind = 'circle';
  constructor(private radius: number) {}
  area(): number {
    return Math.PI * this.radius * this.radius;
  }
}

export function describeShape(shape: Shape): string {
  return `${shape.kind}: ${shape.area()}`;
}

const first = 1, second = 2;
export const scale = (s: Shape, by: number) => s.area() * by;
"#
    }

    fn after_src() -> &'static str {
        r#"
export interface Shape {
  readonly kind: string;
  area(): number;
  perimeter(): number;
}

export class Circle implements Shape {
  readonly kind = 'circle';
  constructor(private radius: number) {}
  area(): number {
    return Math.PI * this.radius * this.radius;
  }
  perimeter(): number {
    return 2 * Math.PI * this.radius;
  }
}

export function renderShape(shape: Shape): string {
  return `${shape.kind}: ${shape.area()}`;
}

const first = 1, second = 3;
export const scale = (s: Shape, by: number) => s.area() * by * 2;
"#
    }

    fn fixture() -> Vec<FileChange> {
        vec![FileChange {
            file_path: format!("src/{SCOPE}/shapes.ts"),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(before_src().to_string()),
            after_content: Some(after_src().to_string()),
        }]
    }

    fn run_with(mutation: Mutation) -> OracleRun {
        let previous = MutatingExtractor::scoped(mutation, SCOPE).install();
        let registry = create_default_registry();
        let out = run(&fixture(), &registry, mutation.identity());
        fast_extractor::install(previous);
        out
    }

    fn layers(run: &OracleRun) -> Vec<Layer> {
        let mut ls: Vec<Layer> = run.divergences.iter().map(|d| d.layer).collect();
        ls.dedup();
        ls
    }

    #[test]
    fn a_faithful_extractor_is_equivalent_and_not_vacuous() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_with(Mutation::Faithful);
        assert_eq!(
            run.verdict(),
            Verdict::Equivalent,
            "faithful extractor diverged: {:#?}",
            run.divergences
        );
        assert!(
            run.served >= 2,
            "expected both sides of the fixture to be served, got {}",
            run.served
        );
    }

    #[test]
    fn an_extractor_that_declines_everything_is_vacuous_not_equivalent() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        struct Decliner;
        impl FastExtractor for Decliner {
            fn identity(&self) -> &str {
                "decliner"
            }
            fn claims(&self, _file_path: &str) -> bool {
                false
            }
            fn extract(&self, _f: &str, _c: &str) -> Option<Vec<SemanticEntity>> {
                None
            }
        }
        let previous =
            fast_extractor::install(Some(Arc::new(FastExtractorSet::new(vec![Box::new(
                Decliner,
            )]))));
        let registry = create_default_registry();
        let run = run(&fixture(), &registry, "decliner");
        fast_extractor::install(previous);

        assert!(run.divergences.is_empty());
        assert_eq!(run.served, 0);
        assert_eq!(run.verdict(), Verdict::Vacuous);
    }

    #[test]
    fn a_dropped_entity_is_caught_at_every_layer() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_with(Mutation::DropLastEntity);
        assert_eq!(run.verdict(), Verdict::Divergent);
        assert!(layers(&run).contains(&Layer::EntitySet));
        assert!(layers(&run).contains(&Layer::DiffResult));
        assert!(layers(&run).contains(&Layer::RenderedJson));
    }

    #[test]
    fn a_shifted_span_is_caught() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_with(Mutation::ShiftSpan);
        assert_eq!(run.verdict(), Verdict::Divergent);
        assert!(layers(&run).contains(&Layer::EntitySet));
        assert!(layers(&run).contains(&Layer::RenderedJson));
    }

    #[test]
    fn a_renamed_entity_is_caught() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_with(Mutation::RenameEntities);
        assert_eq!(run.verdict(), Verdict::Divergent);
        assert!(layers(&run).contains(&Layer::EntitySet));
        assert!(layers(&run).contains(&Layer::RenderedJson));
    }

    #[test]
    fn a_dropped_structural_hash_is_caught_by_the_diff_layers_only() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_with(Mutation::DropStructuralHash);
        assert_eq!(
            run.verdict(),
            Verdict::Divergent,
            "structural_hash is load-bearing for the cosmetic verdict; losing it must not pass"
        );
        assert!(
            !layers(&run).contains(&Layer::EntitySet),
            "structural_hash is intentionally outside the entity fingerprint"
        );
        assert!(layers(&run).contains(&Layer::DiffResult));
        assert!(layers(&run).contains(&Layer::RenderedJson));
    }

    #[test]
    fn a_dropped_kappa_is_caught_by_the_partition_layer_only() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_with(Mutation::DropKappa);
        assert_eq!(
            run.verdict(),
            Verdict::Divergent,
            "kappa is the fast path's primary identity in the facts layer"
        );
        assert_eq!(
            layers(&run),
            vec![Layer::KappaPartition],
            "kappa is inert in today's differ and its value is outside the entity \
             fingerprint — only the partition layer can see it. This is the oracle's \
             resolution boundary, asserted so it cannot drift silently."
        );
    }

    #[test]
    fn a_merged_declarator_pair_is_caught() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_with(Mutation::MergeDeclarators);
        assert_eq!(run.verdict(), Verdict::Divergent);
        assert!(layers(&run).contains(&Layer::EntitySet));
    }

    #[test]
    fn every_mutation_but_faithful_is_caught() {
        let _g = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for m in Mutation::ALL {
            let run = run_with(*m);
            let expected = if *m == Mutation::Faithful {
                Verdict::Equivalent
            } else {
                Verdict::Divergent
            };
            assert_eq!(
                run.verdict(),
                expected,
                "mutation {m:?}: {:#?}",
                run.divergences
            );
        }
    }
}
