use std::path::Path;

use serde::Serialize;

use crate::git::bridge::GitBridge;
use crate::git::types::{FileChange, FileStatus};
use crate::model::change::ChangeType;
use crate::parser::differ::compute_semantic_diff;
use crate::parser::registry::ParserRegistry;
use crate::parser::signature::{analyze_signature_change, SignatureChangeKind};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityLogResult {
    pub entity_name: String,
    pub entity_type: String,
    pub file_path: String,
    pub events: Vec<EntityLogEvent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityLogEvent {
    pub sha: String,
    pub short_sha: String,
    pub date: String,
    pub author: String,
    pub event_type: EntityEventType,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityEventType {
    Added,
    Modified,
    Deleted,
    Renamed { old_name: String },
    Moved { old_file_path: String },
    SignatureChanged { detail: String },
}

// ---------------------------------------------------------------------------
// Entity resolution
// ---------------------------------------------------------------------------

pub struct EntityMatch {
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
}

pub enum EntityResolutionError {
    NotFound,
    Ambiguous(Vec<EntityMatch>),
}

/// Scan working-directory files to locate a single entity by name (or full ID).
pub fn resolve_entity(
    registry: &ParserRegistry,
    repo_root: &Path,
    entity_name: &str,
    file_filter: Option<&str>,
    file_exts: &[String],
) -> Result<EntityMatch, EntityResolutionError> {
    let files = if let Some(filter) = file_filter {
        // Only look in the specified file.
        if registry.get_plugin(filter).is_some() {
            vec![filter.to_string()]
        } else {
            return Err(EntityResolutionError::NotFound);
        }
    } else {
        find_files(repo_root, registry, file_exts)
    };

    let is_full_id = entity_name.contains("::");
    let mut matches: Vec<EntityMatch> = Vec::new();

    for rel_path in &files {
        let abs = repo_root.join(rel_path);
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let plugin = match registry.get_plugin(rel_path) {
            Some(p) => p,
            None => continue,
        };
        let entities = plugin.extract_entities(&content, rel_path);
        for ent in entities {
            let matched = if is_full_id {
                ent.id == entity_name
            } else {
                ent.name == entity_name
            };
            if matched {
                // Full IDs are globally unique — short-circuit immediately.
                if is_full_id {
                    return Ok(EntityMatch {
                        name: ent.name,
                        entity_type: ent.entity_type,
                        file_path: ent.file_path,
                    });
                }
                matches.push(EntityMatch {
                    name: ent.name,
                    entity_type: ent.entity_type,
                    file_path: ent.file_path,
                });
            }
        }
    }

    match matches.len() {
        0 => Err(EntityResolutionError::NotFound),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => Err(EntityResolutionError::Ambiguous(matches)),
    }
}

// ---------------------------------------------------------------------------
// File discovery (mirrors graph.rs approach)
// ---------------------------------------------------------------------------

fn find_files(root: &Path, registry: &ParserRegistry, file_exts: &[String]) -> Vec<String> {
    let mut files = Vec::new();
    walk_dir(root, root, registry, file_exts, &mut files);
    files
}

fn walk_dir(
    dir: &Path,
    root: &Path,
    registry: &ParserRegistry,
    file_exts: &[String],
    files: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.')
                || name == "node_modules"
                || name == "target"
                || name == "vendor"
                || name == "__pycache__"
            {
                continue;
            }
            walk_dir(&path, root, registry, file_exts, files);
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if !file_exts.is_empty()
                && !file_exts.iter().any(|ext| rel.ends_with(ext.as_str()))
            {
                continue;
            }
            if registry.get_plugin(&rel).is_some() {
                files.push(rel);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Build entity log (history across commits)
// ---------------------------------------------------------------------------

pub fn build_entity_log(
    git: &GitBridge,
    registry: &ParserRegistry,
    entity: &EntityMatch,
    from_ref: Option<&str>,
    to_ref: Option<&str>,
    follow_renames: bool,
) -> Result<EntityLogResult, String> {
    let commits = git
        .walk_commits(from_ref, to_ref)
        .map_err(|e| format!("failed to walk commits: {e}"))?;

    let mut tracked_name = entity.name.clone();
    let mut tracked_file = entity.file_path.clone();
    let mut events: Vec<EntityLogEvent> = Vec::new();

    for commit in &commits {
        // 1. Cheap: get changed file paths only (no content).
        let diff_files = match git.get_commit_diff_files(&commit.sha) {
            Ok(f) => f,
            Err(_) => continue,
        };

        // 2. Filter to files touching the tracked path.
        let relevant: Vec<&FileChange> = diff_files
            .iter()
            .filter(|fc| {
                fc.file_path == tracked_file
                    || fc.old_file_path.as_deref() == Some(&tracked_file)
            })
            .collect();

        if relevant.is_empty() {
            continue;
        }

        // 3. Resolve both trees once per commit, then read blobs.
        let after_tree = git.resolve_tree(&commit.sha).ok();
        let before_tree = git.resolve_tree(&format!("{}~1", commit.sha)).ok();

        let mut hydrated: Vec<FileChange> = Vec::with_capacity(relevant.len());
        for fc in &relevant {
            let before_path = fc.old_file_path.as_deref().unwrap_or(&fc.file_path);
            let after_content = if fc.status != FileStatus::Deleted {
                after_tree
                    .as_ref()
                    .and_then(|t| git.read_blob_from_tree(t, &fc.file_path))
            } else {
                None
            };
            let before_content = if fc.status != FileStatus::Added {
                before_tree
                    .as_ref()
                    .and_then(|t| git.read_blob_from_tree(t, before_path))
            } else {
                None
            };
            hydrated.push(FileChange {
                file_path: fc.file_path.clone(),
                status: fc.status.clone(),
                old_file_path: fc.old_file_path.clone(),
                before_content,
                after_content,
            });
        }

        // 4. Compute semantic diff for these files.
        let diff = compute_semantic_diff(
            &hydrated,
            registry,
            Some(&commit.sha),
            Some(&commit.author),
        );

        // 5. Find the change that matches our tracked entity.
        let mut found_event: Option<EntityLogEvent> = None;
        let mut was_added = false;

        for change in &diff.changes {
            if change.entity_name != tracked_name {
                continue;
            }

            let date = format_unix_date(&commit.date);
            let first_line = commit
                .message
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();

            let (event_type, description) = match change.change_type {
                ChangeType::Added => {
                    was_added = true;
                    (EntityEventType::Added, first_line)
                }
                ChangeType::Deleted => (EntityEventType::Deleted, first_line),
                ChangeType::Renamed => {
                    let old_name = change.old_entity_name.clone().unwrap_or_default();
                    let desc = format!("was: {old_name}");
                    let evt = EntityEventType::Renamed {
                        old_name: old_name.clone(),
                    };
                    if follow_renames {
                        tracked_name = old_name;
                    }
                    (evt, desc)
                }
                ChangeType::Moved => {
                    let old_path = change.old_file_path.clone().unwrap_or_default();
                    let desc = format!("from: {old_path}");
                    let evt = EntityEventType::Moved {
                        old_file_path: old_path.clone(),
                    };
                    if follow_renames {
                        tracked_file = old_path;
                    }
                    (evt, desc)
                }
                ChangeType::Modified => {
                    // Run signature analysis when both sides are available.
                    if let (Some(ref before), Some(ref after)) =
                        (&change.before_content, &change.after_content)
                    {
                        let sig =
                            analyze_signature_change(before, after, &change.file_path);
                        match sig {
                            SignatureChangeKind::BodyOnly
                            | SignatureChangeKind::NotApplicable => {
                                let desc = if change.structural_change == Some(false) {
                                    "formatting only".to_string()
                                } else {
                                    first_line
                                };
                                (EntityEventType::Modified, desc)
                            }
                            _ => {
                                let label = sig.label().to_string();
                                (
                                    EntityEventType::SignatureChanged {
                                        detail: label.clone(),
                                    },
                                    label,
                                )
                            }
                        }
                    } else {
                        let desc = if change.structural_change == Some(false) {
                            "formatting only".to_string()
                        } else {
                            first_line
                        };
                        (EntityEventType::Modified, desc)
                    }
                }
            };

            found_event = Some(EntityLogEvent {
                sha: commit.sha.clone(),
                short_sha: commit.short_sha.clone(),
                date,
                author: commit.author.clone(),
                event_type,
                description,
            });
            break; // one event per entity per commit
        }

        if let Some(evt) = found_event {
            events.push(evt);
        }

        if was_added {
            break; // entity didn't exist before this commit
        }
    }

    // Commits were newest-first; reverse so output is oldest-first.
    events.reverse();

    Ok(EntityLogResult {
        entity_name: entity.name.clone(),
        entity_type: entity.entity_type.clone(),
        file_path: entity.file_path.clone(),
        events,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_unix_date(timestamp_str: &str) -> String {
    let secs: i64 = timestamp_str.parse().unwrap_or(0);
    let days = secs / 86400;
    let mut y: i64 = 1970;
    let mut remaining = days;
    loop {
        let yd = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < yd {
            break;
        }
        remaining -= yd;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for (i, &md) in mdays.iter().enumerate() {
        if remaining < md {
            m = i + 1;
            break;
        }
        remaining -= md;
    }
    let d = remaining + 1;
    format!("{y:04}-{m:02}-{d:02}")
}
