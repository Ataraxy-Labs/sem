//! Changelog generation: classifies entity-level changes into Keep-a-Changelog
//! categories (Breaking, Added, Changed, Removed, Internal) with semver suggestion.

use std::collections::HashSet;

use serde::Serialize;

use crate::git::types::CommitInfo;
use crate::model::change::ChangeType;
use crate::parser::differ::DiffResult;
use crate::parser::graph::EntityGraph;
use crate::parser::review::{self, ReviewChange};

/// Changelog category per Keep-a-Changelog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangelogCategory {
    Breaking,
    Added,
    Changed,
    Removed,
    Internal,
}

impl std::fmt::Display for ChangelogCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangelogCategory::Breaking => write!(f, "Breaking Changes"),
            ChangelogCategory::Added => write!(f, "Added"),
            ChangelogCategory::Changed => write!(f, "Changed"),
            ChangelogCategory::Removed => write!(f, "Removed"),
            ChangelogCategory::Internal => write!(f, "Internal"),
        }
    }
}

/// Semver bump suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SemverBump {
    Major,
    Minor,
    Patch,
}

impl std::fmt::Display for SemverBump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SemverBump::Major => write!(f, "MAJOR"),
            SemverBump::Minor => write!(f, "MINOR"),
            SemverBump::Patch => write!(f, "PATCH"),
        }
    }
}

/// A single changelog entry.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogEntry {
    pub category: ChangelogCategory,
    pub entity_name: String,
    pub entity_type: String,
    pub file_path: String,
    pub description: String,
    /// Approximate dependent count if relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependent_count: Option<usize>,
    /// Conventional commit type extracted from commit messages, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conventional_type: Option<String>,
}

/// Full changelog output.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogResult {
    pub date: String,
    pub breaking: Vec<ChangelogEntry>,
    pub added: Vec<ChangelogEntry>,
    pub changed: Vec<ChangelogEntry>,
    pub removed: Vec<ChangelogEntry>,
    pub internal: Vec<ChangelogEntry>,
    pub semver_suggestion: SemverBump,
    pub semver_reason: String,
    /// Conventional commit metadata extracted from the range.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conventional_commits: Vec<ConventionalCommit>,
}

/// Parsed conventional commit.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConventionalCommit {
    pub sha: String,
    pub commit_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub is_breaking: bool,
    pub description: String,
}

/// Build a changelog from diff results, entity graph, and optional commit messages.
pub fn build_changelog(
    diff: &DiffResult,
    graph: &EntityGraph,
    commits: &[CommitInfo],
    date: &str,
) -> ChangelogResult {
    let review = review::build_review(diff, graph);
    let conventional = parse_conventional_commits(commits);

    // Collect breaking commit scopes for cross-referencing
    let breaking_scopes: HashSet<String> = conventional
        .iter()
        .filter(|c| c.is_breaking)
        .filter_map(|c| c.scope.clone())
        .collect();
    let has_breaking_commits = conventional.iter().any(|c| c.is_breaking);

    let mut breaking = Vec::new();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut internal = Vec::new();

    // Classify API surface changes
    for rc in &review.api_surface_changes {
        let conv_type = find_conventional_type(&conventional, &rc.entity_name);
        let is_conventional_breaking = is_entity_breaking(&breaking_scopes, &rc.entity_name)
            || has_breaking_commits && rc.dependent_count >= 5;

        match rc.change_type {
            ChangeType::Added => {
                added.push(make_entry(rc, ChangelogCategory::Added, conv_type));
            }
            ChangeType::Deleted => {
                if rc.dependent_count > 0 {
                    breaking.push(make_deleted_entry(rc, true, conv_type.clone()));
                    removed.push(make_deleted_entry(rc, false, conv_type));
                } else {
                    removed.push(make_deleted_entry(rc, false, conv_type));
                }
            }
            ChangeType::Modified => {
                if is_conventional_breaking {
                    breaking.push(make_entry(rc, ChangelogCategory::Breaking, conv_type));
                } else {
                    changed.push(make_entry(rc, ChangelogCategory::Changed, conv_type));
                }
            }
            ChangeType::Renamed | ChangeType::Moved => {
                if rc.dependent_count > 0 {
                    breaking.push(make_renamed_entry(rc, conv_type));
                } else {
                    changed.push(make_entry(rc, ChangelogCategory::Changed, conv_type));
                }
            }
        }
    }

    // Classify internal changes
    for rc in &review.internal_changes {
        let conv_type = find_conventional_type(&conventional, &rc.entity_name);
        match rc.change_type {
            ChangeType::Deleted if !rc.was_referenced_by.is_empty() => {
                // Deleted with references — potential breakage, but internal
                removed.push(make_deleted_entry(rc, false, conv_type));
            }
            _ => {
                internal.push(make_entry(rc, ChangelogCategory::Internal, conv_type));
            }
        }
    }

    // Config changes go to Changed or Internal
    for rc in &review.config_changes {
        let conv_type = find_conventional_type(&conventional, &rc.entity_name);
        let entry = if let Some(ref vd) = rc.value_diff {
            ChangelogEntry {
                category: ChangelogCategory::Changed,
                entity_name: rc.entity_name.clone(),
                entity_type: rc.entity_type.clone(),
                file_path: rc.file_path.clone(),
                description: format!("`{}` {} in {}", rc.entity_name, vd, rc.file_path),
                dependent_count: None,
                conventional_type: conv_type,
            }
        } else {
            make_entry(rc, ChangelogCategory::Changed, conv_type)
        };
        changed.push(entry);
    }

    // Sort each section by entity name
    breaking.sort_by(|a, b| a.entity_name.cmp(&b.entity_name));
    added.sort_by(|a, b| a.entity_name.cmp(&b.entity_name));
    changed.sort_by(|a, b| a.entity_name.cmp(&b.entity_name));
    removed.sort_by(|a, b| a.entity_name.cmp(&b.entity_name));
    internal.sort_by(|a, b| a.entity_name.cmp(&b.entity_name));

    let (semver_suggestion, semver_reason) =
        compute_semver(&breaking, &added, &changed, &removed, &internal);

    ChangelogResult {
        date: date.to_string(),
        breaking,
        added,
        changed,
        removed,
        internal,
        semver_suggestion,
        semver_reason,
        conventional_commits: conventional,
    }
}

fn make_entry(
    rc: &ReviewChange,
    category: ChangelogCategory,
    conventional_type: Option<String>,
) -> ChangelogEntry {
    let description = match category {
        ChangelogCategory::Breaking => {
            if rc.dependent_count > 0 {
                format!(
                    "`{}` — {} {} changed ({} dependents — downstream callers will need updates)",
                    rc.entity_name, rc.entity_type, rc.change_label, rc.dependent_count
                )
            } else {
                format!("`{}` — {} {}", rc.entity_name, rc.entity_type, rc.change_label)
            }
        }
        ChangelogCategory::Added => {
            format!(
                "`{}` — new {} in {}",
                rc.entity_name, rc.entity_type, rc.file_path
            )
        }
        ChangelogCategory::Changed => {
            if let Some(ref vd) = rc.value_diff {
                format!("`{}` {} in {}", rc.entity_name, vd, rc.file_path)
            } else {
                format!(
                    "`{}` — {} {} in {}",
                    rc.entity_name, rc.entity_type, rc.change_label, rc.file_path
                )
            }
        }
        ChangelogCategory::Internal => {
            format!(
                "`{}` — {} {} in {}",
                rc.entity_name, rc.entity_type, rc.change_label, rc.file_path
            )
        }
        ChangelogCategory::Removed => {
            format!("`{}` — {} removed", rc.entity_name, rc.entity_type)
        }
    };

    ChangelogEntry {
        category,
        entity_name: rc.entity_name.clone(),
        entity_type: rc.entity_type.clone(),
        file_path: rc.file_path.clone(),
        description,
        dependent_count: if rc.dependent_count > 0 {
            Some(rc.dependent_count)
        } else {
            None
        },
        conventional_type,
    }
}

fn make_deleted_entry(
    rc: &ReviewChange,
    is_breaking: bool,
    conventional_type: Option<String>,
) -> ChangelogEntry {
    let category = if is_breaking {
        ChangelogCategory::Breaking
    } else {
        ChangelogCategory::Removed
    };

    let description = if !rc.was_referenced_by.is_empty() {
        let refs: Vec<&str> = rc.was_referenced_by.iter().take(3).map(|s| s.as_str()).collect();
        let suffix = if rc.was_referenced_by.len() > 3 {
            format!(" (+{} more)", rc.was_referenced_by.len() - 3)
        } else {
            String::new()
        };
        format!(
            "`{}` — {} deleted, was referenced by: {}{}",
            rc.entity_name,
            rc.entity_type,
            refs.join(", "),
            suffix
        )
    } else {
        format!("`{}` — {} deleted", rc.entity_name, rc.entity_type)
    };

    ChangelogEntry {
        category,
        entity_name: rc.entity_name.clone(),
        entity_type: rc.entity_type.clone(),
        file_path: rc.file_path.clone(),
        description,
        dependent_count: if rc.dependent_count > 0 {
            Some(rc.dependent_count)
        } else {
            None
        },
        conventional_type,
    }
}

fn make_renamed_entry(
    rc: &ReviewChange,
    conventional_type: Option<String>,
) -> ChangelogEntry {
    let description = if let Some(ref old_path) = rc.old_file_path {
        format!(
            "`{}` — {} renamed/moved from {} ({} dependents — imports will need updates)",
            rc.entity_name, rc.entity_type, old_path, rc.dependent_count
        )
    } else {
        format!(
            "`{}` — {} renamed ({} dependents — references will need updates)",
            rc.entity_name, rc.entity_type, rc.dependent_count
        )
    };

    ChangelogEntry {
        category: ChangelogCategory::Breaking,
        entity_name: rc.entity_name.clone(),
        entity_type: rc.entity_type.clone(),
        file_path: rc.file_path.clone(),
        description,
        dependent_count: Some(rc.dependent_count),
        conventional_type,
    }
}

/// Parse conventional commit prefixes from commit messages.
pub fn parse_conventional_commits(commits: &[CommitInfo]) -> Vec<ConventionalCommit> {
    commits
        .iter()
        .filter_map(|c| parse_single_conventional(&c.sha, &c.message))
        .collect()
}

fn parse_single_conventional(sha: &str, message: &str) -> Option<ConventionalCommit> {
    let first_line = message.lines().next()?.trim();

    // Pattern: type(scope)!: description  or  type!: description  or  type: description
    let (before_colon, description) = first_line.split_once(':')?;
    let before_colon = before_colon.trim();
    let description = description.trim().to_string();

    if description.is_empty() {
        return None;
    }

    let is_breaking_bang = before_colon.ends_with('!');
    let before_colon = before_colon.trim_end_matches('!');

    let (commit_type, scope) = if let Some(paren_start) = before_colon.find('(') {
        let type_str = &before_colon[..paren_start];
        let scope_str = before_colon
            .get(paren_start + 1..)?
            .trim_end_matches(')');
        (type_str.to_string(), Some(scope_str.to_string()))
    } else {
        (before_colon.to_string(), None)
    };

    // Validate: commit type should be a known conventional type
    let valid_types = [
        "feat", "fix", "docs", "style", "refactor", "perf", "test", "build",
        "ci", "chore", "revert",
    ];
    if !valid_types.contains(&commit_type.as_str()) {
        return None;
    }

    // Check for BREAKING CHANGE footer
    let is_breaking_footer = message.contains("BREAKING CHANGE:")
        || message.contains("BREAKING-CHANGE:");

    Some(ConventionalCommit {
        sha: sha[..7.min(sha.len())].to_string(),
        commit_type,
        scope,
        is_breaking: is_breaking_bang || is_breaking_footer,
        description,
    })
}

fn find_conventional_type(
    conventional: &[ConventionalCommit],
    entity_name: &str,
) -> Option<String> {
    // Try to match by scope first, then by description mention
    for cc in conventional {
        if let Some(ref scope) = cc.scope {
            if scope == entity_name || scope.contains(entity_name) {
                return Some(cc.commit_type.clone());
            }
        }
    }
    for cc in conventional {
        if cc.description.contains(entity_name) {
            return Some(cc.commit_type.clone());
        }
    }
    None
}

fn is_entity_breaking(breaking_scopes: &HashSet<String>, entity_name: &str) -> bool {
    breaking_scopes.contains(entity_name)
        || breaking_scopes.iter().any(|s| s.contains(entity_name))
}

fn compute_semver(
    breaking: &[ChangelogEntry],
    added: &[ChangelogEntry],
    _changed: &[ChangelogEntry],
    _removed: &[ChangelogEntry],
    _internal: &[ChangelogEntry],
) -> (SemverBump, String) {
    if !breaking.is_empty() {
        (
            SemverBump::Major,
            format!(
                "{} breaking change{} detected",
                breaking.len(),
                if breaking.len() == 1 { "" } else { "s" }
            ),
        )
    } else if !added.is_empty() {
        (
            SemverBump::Minor,
            format!(
                "new public API added, no breaking changes",
            ),
        )
    } else {
        (
            SemverBump::Patch,
            "no new public API, no breaking changes".to_string(),
        )
    }
}

/// Render changelog as Keep-a-Changelog markdown (no LLM).
pub fn render_markdown(result: &ChangelogResult, heading: &str) -> String {
    let mut lines = Vec::new();

    lines.push(format!("## {} — {}", heading, result.date));
    lines.push(String::new());

    if !result.breaking.is_empty() {
        lines.push("### Breaking Changes".to_string());
        for entry in &result.breaking {
            lines.push(format!("- {}", entry.description));
        }
        lines.push(String::new());
    }

    if !result.added.is_empty() {
        lines.push("### Added".to_string());
        for entry in &result.added {
            lines.push(format!("- {}", entry.description));
        }
        lines.push(String::new());
    }

    if !result.changed.is_empty() {
        lines.push("### Changed".to_string());
        for entry in &result.changed {
            lines.push(format!("- {}", entry.description));
        }
        lines.push(String::new());
    }

    if !result.removed.is_empty() {
        lines.push("### Removed".to_string());
        for entry in &result.removed {
            lines.push(format!("- {}", entry.description));
        }
        lines.push(String::new());
    }

    if !result.internal.is_empty() {
        // Collapse internal: show count + up to 5 items
        let total = result.internal.len();
        lines.push("### Internal".to_string());
        if total <= 5 {
            for entry in &result.internal {
                lines.push(format!("- {}", entry.description));
            }
        } else {
            for entry in result.internal.iter().take(5) {
                lines.push(format!("- {}", entry.description));
            }
            lines.push(format!("- ... and {} more internal changes", total - 5));
        }
        lines.push(String::new());
    }

    lines.push(format!(
        "Suggested version bump: {} ({})",
        result.semver_suggestion, result.semver_reason
    ));

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_conventional_feat() {
        let cc = parse_single_conventional("abc1234", "feat: add new login page").unwrap();
        assert_eq!(cc.commit_type, "feat");
        assert_eq!(cc.scope, None);
        assert!(!cc.is_breaking);
        assert_eq!(cc.description, "add new login page");
    }

    #[test]
    fn test_parse_conventional_with_scope() {
        let cc = parse_single_conventional("abc1234", "fix(auth): resolve token expiry").unwrap();
        assert_eq!(cc.commit_type, "fix");
        assert_eq!(cc.scope.as_deref(), Some("auth"));
        assert!(!cc.is_breaking);
    }

    #[test]
    fn test_parse_conventional_breaking_bang() {
        let cc = parse_single_conventional("abc1234", "feat!: redesign API").unwrap();
        assert!(cc.is_breaking);
    }

    #[test]
    fn test_parse_conventional_breaking_footer() {
        let cc = parse_single_conventional(
            "abc1234",
            "feat: change auth\n\nBREAKING CHANGE: removed legacy endpoint",
        )
        .unwrap();
        assert!(cc.is_breaking);
    }

    #[test]
    fn test_parse_conventional_not_conventional() {
        assert!(parse_single_conventional("abc1234", "Update readme").is_none());
        assert!(parse_single_conventional("abc1234", "Merge pull request #42").is_none());
    }

    #[test]
    fn test_semver_breaking() {
        let breaking = vec![ChangelogEntry {
            category: ChangelogCategory::Breaking,
            entity_name: "foo".into(),
            entity_type: "function".into(),
            file_path: "a.ts".into(),
            description: "broke it".into(),
            dependent_count: Some(5),
            conventional_type: None,
        }];
        let (bump, _) = compute_semver(&breaking, &[], &[], &[], &[]);
        assert_eq!(bump, SemverBump::Major);
    }

    #[test]
    fn test_semver_minor() {
        let added = vec![ChangelogEntry {
            category: ChangelogCategory::Added,
            entity_name: "bar".into(),
            entity_type: "function".into(),
            file_path: "b.ts".into(),
            description: "new thing".into(),
            dependent_count: None,
            conventional_type: None,
        }];
        let (bump, _) = compute_semver(&[], &added, &[], &[], &[]);
        assert_eq!(bump, SemverBump::Minor);
    }

    #[test]
    fn test_semver_patch() {
        let (bump, _) = compute_semver(&[], &[], &[], &[], &[]);
        assert_eq!(bump, SemverBump::Patch);
    }
}
