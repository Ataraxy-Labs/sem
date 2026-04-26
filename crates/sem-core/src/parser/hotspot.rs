//! Hotspot analysis: find entities that change most frequently across git history.
//! High-churn entities are statistically more likely to contain bugs.

use std::collections::HashMap;

use crate::git::bridge::GitBridge;
use crate::git::types::DiffScope;
use crate::parser::differ::compute_semantic_diff;
use crate::parser::registry::ParserRegistry;

#[derive(Debug, Clone)]
pub struct EntityHotspot {
    pub entity_name: String,
    pub entity_type: String,
    pub file_path: String,
    pub change_count: usize,
}

/// Walk git history and count how often each entity appears in semantic diffs.
///
/// - `file_path`: if Some, only track changes to entities in this file
/// - `max_commits`: maximum number of commits to walk (default 50)
///
/// Returns hotspots sorted by change_count descending.
pub fn compute_hotspots(
    git: &GitBridge,
    registry: &ParserRegistry,
    file_path: Option<&str>,
    max_commits: usize,
) -> Vec<EntityHotspot> {
    let commits = match git.get_log(max_commits + 1) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    if commits.len() < 2 {
        return Vec::new();
    }

    // entity key (name, type, file) -> count
    let mut churn: HashMap<(String, String, String), usize> = HashMap::new();

    let pathspecs: Vec<String> = file_path.map(|f| vec![f.to_string()]).unwrap_or_default();

    // Compare consecutive commit pairs
    for window in commits.windows(2) {
        let newer = &window[0];
        let older = &window[1];

        let scope = DiffScope::Range {
            from: older.sha.clone(),
            to: newer.sha.clone(),
        };

        let file_changes = match git.get_changed_files(&scope, &pathspecs) {
            Ok(fc) => fc,
            Err(_) => continue,
        };

        let diff = compute_semantic_diff(&file_changes, registry, Some(&newer.sha), None);

        for change in &diff.changes {
            // Filter to target file if specified
            if let Some(fp) = file_path {
                if change.file_path != fp {
                    continue;
                }
            }

            let key = (
                change.entity_name.clone(),
                change.entity_type.clone(),
                change.file_path.clone(),
            );
            *churn.entry(key).or_insert(0) += 1;
        }
    }

    let mut hotspots: Vec<EntityHotspot> = churn
        .into_iter()
        .map(|((name, entity_type, file_path), count)| EntityHotspot {
            entity_name: name,
            entity_type,
            file_path,
            change_count: count,
        })
        .collect();

    hotspots.sort_by(|a, b| b.change_count.cmp(&a.change_count));
    hotspots
}
