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

/// A hot entity in repo history: how often it changed, by how many people,
/// and when it last moved. The unit is commits (an entity changing twice in
/// one commit counts once), so counts compare cleanly across entities.
#[derive(Debug, Clone)]
pub struct HotEntity {
    pub entity_name: String,
    pub entity_type: String,
    pub file_path: String,
    /// Number of scanned commits in which this entity changed.
    pub commits: usize,
    /// Distinct commit authors who changed it.
    pub authors: usize,
    /// Short SHA of the most recent commit that changed it.
    pub last_short_sha: String,
}

/// Two entities that repeatedly change in the same commits. `confidence` is
/// `together / min(a_commits, b_commits)`: 1.0 means the rarer entity never
/// changes without the other.
#[derive(Debug, Clone)]
pub struct CoChangePair {
    pub a_name: String,
    pub a_file: String,
    pub b_name: String,
    pub b_file: String,
    pub together: usize,
    pub a_commits: usize,
    pub b_commits: usize,
    pub confidence: f64,
}

/// Entity-level history analytics for a repo: hotspots and co-change pairs,
/// computed in a single walk over recent commits.
#[derive(Debug, Clone)]
pub struct HistoryAnalytics {
    pub commits_scanned: usize,
    pub hotspots: Vec<HotEntity>,
    pub co_changes: Vec<CoChangePair>,
    /// Commits skipped for pair-counting because they touched more entities
    /// than `MAX_PAIR_ENTITIES` (bulk refactors/imports would otherwise drown
    /// the signal in quadratic noise). They still count toward hotspots.
    pub pair_commits_skipped: usize,
}

/// Commits touching more entities than this don't contribute co-change pairs.
const MAX_PAIR_ENTITIES: usize = 50;

/// History analytics track code entities only. Doc headings, config properties,
/// and lockfile chunks churn constantly and would bury the signal the analysis
/// exists for: which *code* is hot, and which code moves together.
fn is_code_entity(entity_type: &str) -> bool {
    matches!(
        entity_type,
        "function"
            | "method"
            | "constructor"
            | "init"
            | "init_declaration"
            | "class"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "interface"
            | "protocol"
            | "protocol_declaration"
            | "extension"
            | "object_declaration"
            | "companion_object"
            | "module"
            | "namespace"
            | "component"
            | "macro_definition"
            | "type"
            | "type_alias"
    )
}

/// Walk recent git history once and compute entity-level hotspots plus
/// co-change pairs. This is the "time axis" a snapshot dependency graph can't
/// see: which entities churn, and which move together.
pub fn compute_history_analytics(
    git: &GitBridge,
    registry: &ParserRegistry,
    file_path: Option<&str>,
    max_commits: usize,
) -> HistoryAnalytics {
    let commits = match git.get_log(max_commits + 1) {
        Ok(c) => c,
        Err(_) => return empty_analytics(),
    };
    if commits.len() < 2 {
        return empty_analytics();
    }

    type Key = (String, String, String); // (name, type, file)
    let mut per_entity: HashMap<Key, (usize, std::collections::HashSet<String>, String)> =
        HashMap::new();
    let mut pair_counts: HashMap<(Key, Key), usize> = HashMap::new();
    let mut pair_commits_skipped = 0usize;
    let pathspecs: Vec<String> = file_path.map(|f| vec![f.to_string()]).unwrap_or_default();

    let windows: Vec<_> = commits.windows(2).collect();
    let commits_scanned = windows.len();
    for window in windows {
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

        // Distinct entities changed in this commit (orphans and file-filtered
        // changes excluded; a commit counts once per entity).
        let mut changed: Vec<Key> = diff
            .changes
            .iter()
            .filter(|c| is_code_entity(&c.entity_type))
            .filter(|c| file_path.map_or(true, |fp| c.file_path == fp))
            .map(|c| {
                (
                    c.entity_name.clone(),
                    c.entity_type.clone(),
                    c.file_path.clone(),
                )
            })
            .collect();
        changed.sort();
        changed.dedup();

        for key in &changed {
            let entry = per_entity
                .entry(key.clone())
                .or_insert_with(|| (0, std::collections::HashSet::new(), String::new()));
            entry.0 += 1;
            entry.1.insert(newer.author.clone());
            if entry.2.is_empty() {
                // Commits walk newest-first, so the first sighting is the latest.
                entry.2 = newer.short_sha.clone();
            }
        }

        if changed.len() > MAX_PAIR_ENTITIES {
            pair_commits_skipped += 1;
            continue;
        }
        for i in 0..changed.len() {
            for j in (i + 1)..changed.len() {
                let (a, b) = (changed[i].clone(), changed[j].clone());
                *pair_counts.entry((a, b)).or_insert(0) += 1;
            }
        }
    }

    let mut hotspots: Vec<HotEntity> = per_entity
        .iter()
        .map(|((name, ty, file), (count, authors, last))| HotEntity {
            entity_name: name.clone(),
            entity_type: ty.clone(),
            file_path: file.clone(),
            commits: *count,
            authors: authors.len(),
            last_short_sha: last.clone(),
        })
        .collect();
    hotspots.sort_by(|a, b| {
        b.commits
            .cmp(&a.commits)
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.entity_name.cmp(&b.entity_name))
    });

    let mut co_changes: Vec<CoChangePair> = pair_counts
        .into_iter()
        .filter(|(_, together)| *together >= 2)
        .map(|((a, b), together)| {
            let a_commits = per_entity.get(&a).map(|e| e.0).unwrap_or(together);
            let b_commits = per_entity.get(&b).map(|e| e.0).unwrap_or(together);
            CoChangePair {
                a_name: a.0,
                a_file: a.2,
                b_name: b.0,
                b_file: b.2,
                together,
                a_commits,
                b_commits,
                confidence: together as f64 / a_commits.min(b_commits).max(1) as f64,
            }
        })
        .collect();
    co_changes.sort_by(|a, b| {
        b.together
            .cmp(&a.together)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.a_file.cmp(&b.a_file))
            .then_with(|| a.a_name.cmp(&b.a_name))
    });

    HistoryAnalytics {
        commits_scanned,
        hotspots,
        co_changes,
        pair_commits_skipped,
    }
}

fn empty_analytics() -> HistoryAnalytics {
    HistoryAnalytics {
        commits_scanned: 0,
        hotspots: Vec::new(),
        co_changes: Vec::new(),
        pair_commits_skipped: 0,
    }
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

    // entity key (id, name, type, file) -> count
    let mut churn: HashMap<(String, String, String, String), usize> = HashMap::new();

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
                change.entity_id.clone(),
                change.entity_name.clone(),
                change.entity_type.clone(),
                change.file_path.clone(),
            );
            *churn.entry(key).or_insert(0) += 1;
        }
    }

    let mut hotspots: Vec<EntityHotspot> = churn
        .into_iter()
        .map(
            |((_id, name, entity_type, file_path), count)| EntityHotspot {
                entity_name: name,
                entity_type,
                file_path,
                change_count: count,
            },
        )
        .collect();

    hotspots.sort_by(|a, b| b.change_count.cmp(&a.change_count));
    hotspots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::plugins::create_default_registry;
    use git2::{Oid, Repository, Signature};
    use std::path::Path;
    use tempfile::TempDir;

    fn commit_file(repo: &Repository, file_path: &str, contents: &str, message: &str) -> Oid {
        std::fs::write(repo.workdir().unwrap().join(file_path), contents).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file_path)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        match repo.head() {
            Ok(head) => {
                let parent = repo.find_commit(head.target().unwrap()).unwrap();
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
                    .unwrap()
            }
            Err(_) => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
                .unwrap(),
        }
    }

    #[test]
    fn history_analytics_counts_hotspots_and_co_changes() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        // c1: create both entities
        commit_file(
            &repo,
            "main.py",
            "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
            "c1",
        );
        // c2: modify alpha AND beta (co-change)
        commit_file(
            &repo,
            "main.py",
            "def alpha():\n    return 10\n\ndef beta():\n    return 20\n",
            "c2",
        );
        // c3: modify alpha only
        commit_file(
            &repo,
            "main.py",
            "def alpha():\n    return 100\n\ndef beta():\n    return 20\n",
            "c3",
        );
        // c4: modify alpha AND beta again (co-change x2)
        commit_file(
            &repo,
            "main.py",
            "def alpha():\n    return 1000\n\ndef beta():\n    return 2000\n",
            "c4",
        );

        let git = GitBridge::open(temp.path()).unwrap();
        let registry = create_default_registry();
        let analytics = compute_history_analytics(&git, &registry, None, 50);

        assert_eq!(analytics.commits_scanned, 3); // c2,c3,c4 diffs
        let alpha = analytics
            .hotspots
            .iter()
            .find(|h| h.entity_name == "alpha")
            .expect("alpha hotspot");
        assert_eq!(alpha.commits, 3);
        assert_eq!(alpha.authors, 1);
        assert!(!alpha.last_short_sha.is_empty());
        let beta = analytics
            .hotspots
            .iter()
            .find(|h| h.entity_name == "beta")
            .expect("beta hotspot");
        assert_eq!(beta.commits, 2);
        // alpha is hotter, so it sorts first
        assert_eq!(analytics.hotspots[0].entity_name, "alpha");

        let pair = analytics
            .co_changes
            .iter()
            .find(|p| {
                (p.a_name == "alpha" && p.b_name == "beta")
                    || (p.a_name == "beta" && p.b_name == "alpha")
            })
            .expect("alpha/beta co-change pair");
        assert_eq!(pair.together, 2);
        assert!((pair.confidence - 1.0).abs() < f64::EPSILON); // 2 / min(3,2)
    }

    #[test]
    fn history_analytics_on_empty_repo_is_empty() {
        let temp = TempDir::new().unwrap();
        Repository::init(temp.path()).unwrap();
        let git = GitBridge::open(temp.path());
        // A repo with no commits can't even open HEAD in some paths; if open
        // succeeds, analytics must come back empty rather than erroring.
        if let Ok(git) = git {
            let registry = create_default_registry();
            let analytics = compute_history_analytics(&git, &registry, None, 50);
            assert_eq!(analytics.commits_scanned, 0);
            assert!(analytics.hotspots.is_empty());
            assert!(analytics.co_changes.is_empty());
        }
    }
}
