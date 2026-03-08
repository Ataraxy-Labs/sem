//! PR-level semantic review: groups entity changes into API surface,
//! internal, and config/data categories, with approximate dependent counts
//! and risk assessment.

use std::collections::HashSet;

use serde::Serialize;

use crate::model::change::{ChangeType, SemanticChange};
use crate::parser::differ::DiffResult;
use crate::parser::graph::EntityGraph;

/// File extensions that are considered config/data rather than code.
const CONFIG_EXTENSIONS: &[&str] = &[
    ".json", ".yaml", ".yml", ".toml", ".csv", ".md", ".xml", ".ini", ".cfg",
    ".env", ".properties",
];

/// Entity types emitted by config/data parsers.
const CONFIG_ENTITY_TYPES: &[&str] = &["property", "key", "section", "heading", "row", "item"];

/// Classified change for review output.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewChange {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: String,
    pub file_path: String,
    pub change_type: ChangeType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_file_path: Option<String>,
    /// Short label for terminal display, e.g. "signature changed", "body only", "added".
    pub change_label: String,
    /// Approximate number of direct dependents (from graph).
    pub dependent_count: usize,
    /// Number of distinct files containing dependents.
    pub dependent_file_count: usize,
    /// Inline value diff for config properties (e.g. "5 → 20").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_diff: Option<String>,
    /// For deleted entities: names of entities that referenced this one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub was_referenced_by: Vec<String>,
}

/// The three review groups.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewResult {
    /// Modified/added public-facing entities with dependents across files.
    pub api_surface_changes: Vec<ReviewChange>,
    /// Body-only modifications, deletions, renames, and entities with no cross-file dependents.
    pub internal_changes: Vec<ReviewChange>,
    /// Changes to config/data files (JSON, YAML, TOML, etc.).
    pub config_changes: Vec<ReviewChange>,
    pub risk: RiskAssessment,
    pub summary: ReviewSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSummary {
    pub api_surface_count: usize,
    pub internal_count: usize,
    pub config_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "low"),
            RiskLevel::Medium => write!(f, "medium"),
            RiskLevel::High => write!(f, "high"),
        }
    }
}

/// Build a review from diff results + entity graph.
pub fn build_review(diff: &DiffResult, graph: &EntityGraph) -> ReviewResult {
    let mut api_surface = Vec::new();
    let mut internal = Vec::new();
    let mut config = Vec::new();

    for change in &diff.changes {
        let is_config = is_config_change(change);
        let rc = build_review_change(change, graph);

        if is_config {
            config.push(rc);
        } else if is_api_surface(&rc, change) {
            api_surface.push(rc);
        } else {
            internal.push(rc);
        }
    }

    // Sort each group: higher dependent count first, then by name.
    api_surface.sort_by(|a, b| b.dependent_count.cmp(&a.dependent_count).then(a.entity_name.cmp(&b.entity_name)));
    internal.sort_by(|a, b| a.entity_name.cmp(&b.entity_name));
    config.sort_by(|a, b| a.entity_name.cmp(&b.entity_name));

    let risk = assess_risk(&api_surface, &internal, &config);
    let summary = ReviewSummary {
        api_surface_count: api_surface.len(),
        internal_count: internal.len(),
        config_count: config.len(),
    };

    ReviewResult {
        api_surface_changes: api_surface,
        internal_changes: internal,
        config_changes: config,
        risk,
        summary,
    }
}

fn is_config_change(change: &SemanticChange) -> bool {
    let path = &change.file_path;
    if CONFIG_EXTENSIONS.iter().any(|ext| path.ends_with(ext)) {
        return true;
    }
    if CONFIG_ENTITY_TYPES.contains(&change.entity_type.as_str()) {
        return true;
    }
    false
}

/// An entity is considered API surface if it has cross-file dependents
/// and is being added or modified (not just deleted).
fn is_api_surface(rc: &ReviewChange, change: &SemanticChange) -> bool {
    match change.change_type {
        ChangeType::Added => rc.dependent_count > 0,
        ChangeType::Modified => rc.dependent_file_count > 0,
        ChangeType::Renamed | ChangeType::Moved => rc.dependent_count > 0,
        ChangeType::Deleted => false, // deleted entities go to internal
    }
}

fn build_review_change(change: &SemanticChange, graph: &EntityGraph) -> ReviewChange {
    // Find dependents from graph
    let (dependent_count, dependent_file_count, was_referenced_by) =
        compute_dependent_info(&change.entity_id, &change.file_path, graph);

    let change_label = match change.change_type {
        ChangeType::Added => "added".to_string(),
        ChangeType::Deleted => "deleted".to_string(),
        ChangeType::Moved => "moved".to_string(),
        ChangeType::Renamed => "renamed".to_string(),
        ChangeType::Modified => {
            if change.structural_change == Some(false) {
                "cosmetic".to_string()
            } else {
                "modified".to_string()
            }
        }
    };

    let value_diff = compute_value_diff(change);

    ReviewChange {
        entity_id: change.entity_id.clone(),
        entity_name: change.entity_name.clone(),
        entity_type: change.entity_type.clone(),
        file_path: change.file_path.clone(),
        change_type: change.change_type,
        old_file_path: change.old_file_path.clone(),
        change_label,
        dependent_count,
        dependent_file_count,
        value_diff,
        was_referenced_by,
    }
}

fn compute_dependent_info(
    entity_id: &str,
    entity_file: &str,
    graph: &EntityGraph,
) -> (usize, usize, Vec<String>) {
    let dependents = graph.get_dependents(entity_id);
    let count = dependents.len();

    let mut files: HashSet<&str> = HashSet::new();
    let mut names = Vec::new();
    for dep in &dependents {
        if dep.file_path != entity_file {
            files.insert(&dep.file_path);
        }
        names.push(dep.name.clone());
    }

    (count, files.len(), names)
}

/// For short config properties, show "old → new" inline.
fn compute_value_diff(change: &SemanticChange) -> Option<String> {
    if change.change_type != ChangeType::Modified {
        return None;
    }
    let before = change.before_content.as_deref()?;
    let after = change.after_content.as_deref()?;

    // Only for single-line values
    let before_val = extract_leaf_value(before);
    let after_val = extract_leaf_value(after);
    if let (Some(bv), Some(av)) = (before_val, after_val) {
        if bv.len() <= 60 && av.len() <= 60 {
            return Some(format!("{bv} → {av}"));
        }
    }
    None
}

/// Try to extract a simple value from a config property line.
fn extract_leaf_value(content: &str) -> Option<&str> {
    let trimmed = content.trim();
    // JSON-like "key": value or YAML key: value — grab everything after first ':'
    if let Some(pos) = trimmed.find(':') {
        let val = trimmed[pos + 1..].trim().trim_end_matches(',');
        if !val.is_empty() {
            return Some(val);
        }
    }
    // TOML key = value
    if let Some(pos) = trimmed.find('=') {
        let val = trimmed[pos + 1..].trim();
        if !val.is_empty() {
            return Some(val);
        }
    }
    // Whole content if short enough
    if trimmed.len() <= 80 {
        Some(trimmed)
    } else {
        None
    }
}

fn assess_risk(
    api_surface: &[ReviewChange],
    internal: &[ReviewChange],
    _config: &[ReviewChange],
) -> RiskAssessment {
    // High risk: any API surface change with >=10 dependents
    if let Some(highest) = api_surface.iter().max_by_key(|c| c.dependent_count) {
        if highest.dependent_count >= 10 {
            return RiskAssessment {
                level: RiskLevel::High,
                reason: format!(
                    "modified public {} `{}` with ~{} dependents",
                    highest.entity_type, highest.entity_name, highest.dependent_count
                ),
            };
        }
    }

    // Medium risk: any API surface change, or deletions with dependents
    if !api_surface.is_empty() {
        let max_deps = api_surface.iter().map(|c| c.dependent_count).max().unwrap_or(0);
        return RiskAssessment {
            level: RiskLevel::Medium,
            reason: format!(
                "{} API surface change{} (max ~{} dependents)",
                api_surface.len(),
                if api_surface.len() == 1 { "" } else { "s" },
                max_deps
            ),
        };
    }

    let deleted_with_refs: Vec<_> = internal
        .iter()
        .filter(|c| c.change_type == ChangeType::Deleted && !c.was_referenced_by.is_empty())
        .collect();
    if !deleted_with_refs.is_empty() {
        return RiskAssessment {
            level: RiskLevel::Medium,
            reason: format!(
                "{} deleted entit{} with existing references",
                deleted_with_refs.len(),
                if deleted_with_refs.len() == 1 { "y" } else { "ies" }
            ),
        };
    }

    // Low risk: internal-only or config-only
    RiskAssessment {
        level: RiskLevel::Low,
        reason: "internal/config changes only, no public API impact".to_string(),
    }
}
