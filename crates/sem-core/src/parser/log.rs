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
        crate::utils::files::find_supported_files(repo_root, registry, file_exts)
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
    let mut tracked_name = entity.name.clone();
    let mut tracked_file = entity.file_path.clone();
    let mut events: Vec<EntityLogEvent> = Vec::new();

    git.for_each_commit(from_ref, to_ref, |commit| {
        // 1. Cheap: get changed file paths only (no content).
        let diff_files = match git.get_commit_diff_files(&commit.sha) {
            Ok(f) => f,
            Err(_) => return true, // skip this commit, continue walking
        };

        // 2. Filter to files touching the tracked path.
        let relevant: Vec<&FileChange> = diff_files
            .iter()
            .filter(|fc| {
                fc.file_path == tracked_file
                    || fc.old_file_path.as_deref() == Some(tracked_file.as_str())
            })
            .collect();

        if relevant.is_empty() {
            return true; // continue
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
        for change in &diff.changes {
            if change.entity_name != tracked_name {
                continue;
            }

            let date = crate::utils::date::format_unix_date(commit.date);
            let first_line = commit
                .message
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();

            let (event_type, description) = match change.change_type {
                ChangeType::Added => (EntityEventType::Added, first_line),
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
                ChangeType::Modified => classify_modification(change, &first_line),
            };

            events.push(EntityLogEvent {
                sha: commit.sha.clone(),
                short_sha: commit.short_sha.clone(),
                date,
                author: commit.author.clone(),
                event_type,
                description,
            });

            // Stop walking if entity was just created.
            if change.change_type == ChangeType::Added {
                return false;
            }
            break; // one event per entity per commit
        }

        true // continue walking
    })
    .map_err(|e| format!("failed to walk commits: {e}"))?;

    // Commits were newest-first; reverse so output is oldest-first.
    events.reverse();

    Ok(EntityLogResult {
        entity_name: entity.name.clone(),
        entity_type: entity.entity_type.clone(),
        file_path: entity.file_path.clone(),
        events,
    })
}

fn classify_modification(
    change: &crate::model::change::SemanticChange,
    first_line: &str,
) -> (EntityEventType, String) {
    if let (Some(ref before), Some(ref after)) =
        (&change.before_content, &change.after_content)
    {
        let sig = analyze_signature_change(before, after, &change.file_path);
        match sig {
            SignatureChangeKind::BodyOnly | SignatureChangeKind::NotApplicable => {
                let desc = if change.structural_change == Some(false) {
                    "formatting only".to_string()
                } else {
                    first_line.to_string()
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
            first_line.to_string()
        };
        (EntityEventType::Modified, desc)
    }
}

