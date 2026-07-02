use std::collections::HashSet;
use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::git::types::CommitInfo;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::registry::ParserRegistry;

use super::truncate_str;

pub struct LogOptions {
    pub cwd: String,
    pub entity_name: String,
    pub file_path: Option<String>,
    pub limit: usize,
    pub json: bool,
    pub verbose: bool,
}

#[derive(Debug)]
enum EntityChangeType {
    Added,
    ModifiedLogic,
    ModifiedCosmetic,
    Deleted,
    Moved,
    Renamed,
}

impl EntityChangeType {
    fn label(&self) -> &str {
        match self {
            EntityChangeType::Added => "added",
            EntityChangeType::ModifiedLogic => "modified (logic)",
            EntityChangeType::ModifiedCosmetic => "modified (cosmetic)",
            EntityChangeType::Deleted => "deleted",
            EntityChangeType::Moved => "moved",
            EntityChangeType::Renamed => "renamed",
        }
    }

    fn label_colored(&self) -> colored::ColoredString {
        match self {
            EntityChangeType::Added => "added".green(),
            EntityChangeType::ModifiedLogic => "modified (logic)".yellow(),
            EntityChangeType::ModifiedCosmetic => "modified (cosmetic)".dimmed(),
            EntityChangeType::Deleted => "deleted".red(),
            EntityChangeType::Moved => "moved".blue(),
            EntityChangeType::Renamed => "renamed".cyan(),
        }
    }
}

struct LogEntry {
    sha: String,
    short_sha: String,
    author: String,
    date: String,
    message: String,
    change_type: EntityChangeType,
    content: Option<String>,
    prev_content: Option<String>,
    file_path: Option<String>,
    prev_file_path: Option<String>,
}

#[derive(Clone)]
struct EntityOccurrence {
    commit_index: usize,
    file_path: String,
    entity: SemanticEntity,
}

pub struct HistoryOptions {
    pub cwd: String,
    pub file_path: Option<String>,
    pub limit: usize,
    pub json: bool,
}

/// `sem log` with no entity: repo-level history analytics. The time axis a
/// snapshot dependency graph can't see — which entities churn (hotspots) and
/// which change together (co-change pairs).
pub fn history_command(opts: HistoryOptions) {
    let root = Path::new(&opts.cwd);
    let registry = super::create_registry(&opts.cwd);
    let bridge = match GitBridge::open(root) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{} {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
    };

    let limit = if opts.limit == 0 { 500 } else { opts.limit };
    // Semantic commit index first: each commit is diffed once, ever; later
    // queries are lookups plus a delta of new commits. Falls back to the
    // live walk only when the cache is unusable.
    let analytics = sem_mcp::cache::history_analytics_from_store(
        bridge.repo_root(),
        &bridge,
        &registry,
        opts.file_path.as_deref(),
        limit,
    )
    .unwrap_or_else(|| {
        sem_core::parser::hotspot::compute_history_analytics(
            &bridge,
            &registry,
            opts.file_path.as_deref(),
            limit,
        )
    });

    if opts.json {
        let hotspots: Vec<serde_json::Value> = analytics
            .hotspots
            .iter()
            .map(|h| {
                serde_json::json!({
                    "entity": h.entity_name, "type": h.entity_type,
                    "file": h.file_path, "commits": h.commits,
                    "authors": h.authors, "lastSha": h.last_short_sha,
                })
            })
            .collect();
        let co_changes: Vec<serde_json::Value> = analytics
            .co_changes
            .iter()
            .map(|p| {
                serde_json::json!({
                    "a": {"entity": p.a_name, "file": p.a_file, "commits": p.a_commits},
                    "b": {"entity": p.b_name, "file": p.b_file, "commits": p.b_commits},
                    "together": p.together, "confidence": p.confidence,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "commitsScanned": analytics.commits_scanned,
                "hotspots": hotspots,
                "coChanges": co_changes,
                "pairCommitsSkipped": analytics.pair_commits_skipped,
            }))
            .unwrap_or_default()
        );
        return;
    }

    if analytics.commits_scanned == 0 {
        println!("{}", "no history to analyze (need at least 2 commits)".dimmed());
        return;
    }

    println!(
        "{} {}",
        "⊕ repo history".green().bold(),
        format!("· last {} commits", analytics.commits_scanned).dimmed()
    );

    if analytics.hotspots.is_empty() {
        println!("\n  {}", "no entity changes found".dimmed());
        return;
    }

    println!("\n  {}", "hotspots — most-changed entities".bold());
    for h in analytics.hotspots.iter().take(15) {
        let authors = if h.authors == 1 {
            "1 author".to_string()
        } else {
            format!("{} authors", h.authors)
        };
        println!(
            "  {:>3}  {}  {} {} {}",
            format!("{}×", h.commits).yellow(),
            truncate_str(&h.entity_name, 40),
            authors.dimmed(),
            truncate_str(&h.file_path, 44).dimmed(),
            format!("last {}", h.last_short_sha).dimmed(),
        );
    }
    if analytics.hotspots.len() > 15 {
        println!(
            "  {}",
            format!("… {} more (use --json for all)", analytics.hotspots.len() - 15).dimmed()
        );
    }

    if !analytics.co_changes.is_empty() {
        println!(
            "\n  {}",
            "co-changes — entities that change together".bold()
        );
        for p in analytics.co_changes.iter().take(12) {
            let cross_file = p.a_file != p.b_file;
            let files = if cross_file {
                format!(
                    "{} ↔ {}",
                    truncate_str(&p.a_file, 30),
                    truncate_str(&p.b_file, 30)
                )
            } else {
                truncate_str(&p.a_file, 44).to_string()
            };
            println!(
                "  {:>3}  {} {} {}  {}",
                format!("{}×", p.together).yellow(),
                truncate_str(&p.a_name, 28).cyan(),
                "↔".dimmed(),
                truncate_str(&p.b_name, 28).cyan(),
                format!("{:.0}% · {}", p.confidence * 100.0, files).dimmed(),
            );
        }
        if analytics.co_changes.len() > 12 {
            println!(
                "  {}",
                format!(
                    "… {} more pairs (use --json for all)",
                    analytics.co_changes.len() - 12
                )
                .dimmed()
            );
        }
    }

    if analytics.pair_commits_skipped > 0 {
        println!(
            "\n  {}",
            format!(
                "note: {} bulk commits (>50 entities) excluded from co-change pairing",
                analytics.pair_commits_skipped
            )
            .dimmed()
        );
    }
}

pub fn log_command(opts: LogOptions) {
    if super::cloud::try_cloud_log(&opts).is_some() {
        return;
    }

    let root = Path::new(&opts.cwd);
    let registry = super::create_registry(&opts.cwd);

    let bridge = match GitBridge::open(root) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{} {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
    };

    // Resolve file path when possible. Historical entities need --file so the
    // search can stay bounded to the relevant file history.
    let file_path = match opts.file_path {
        Some(fp) => Some(fp),
        None => match find_entity_file(root, &registry, &opts.entity_name) {
            FindResult::Found(fp) => Some(fp),
            FindResult::Ambiguous(files) => {
                eprintln!(
                    "{} Entity '{}' found in multiple files:",
                    "error:".red().bold(),
                    opts.entity_name
                );
                for f in &files {
                    eprintln!("  {}", f);
                }
                eprintln!("\nUse --file to disambiguate.");
                std::process::exit(1);
            }
            FindResult::NotFound => {
                eprintln!(
                    "{} Entity '{}' not found in the working tree. Use --file to search historical entities.",
                    "error:".red().bold(),
                    opts.entity_name
                );
                std::process::exit(1);
            }
        },
    };

    let repo_root = bridge.repo_root();
    let abs_cwd = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let abs_repo = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());

    let git_file_path = file_path.as_ref().map(|fp| {
        if abs_cwd != abs_repo {
            let prefix = abs_cwd.strip_prefix(&abs_repo).unwrap_or(Path::new(""));
            prefix.join(fp).to_string_lossy().to_string()
        } else {
            fp.clone()
        }
    });

    if let Some(fp) = &file_path {
        let file_content_hint = std::fs::read_to_string(root.join(fp)).unwrap_or_default();
        let resolved_fp = registry.resolve_file_path(fp);
        let detection_fp = resolved_fp.as_deref().unwrap_or(fp);
        if registry
            .get_plugin_with_content(detection_fp, &file_content_hint)
            .is_none()
        {
            eprintln!("{} Unsupported file type: {}", "error:".red().bold(), fp);
            std::process::exit(1);
        }
    }

    let history_limit = history_limit_with_baseline(opts.limit);
    let mut commits = if git_file_path.is_some() {
        let path = git_file_path.as_deref().unwrap();
        match bridge.get_file_commits_follow_renames(path, history_limit) {
            Ok(file_commits) if !file_commits.is_empty() => {
                file_commits.into_iter().map(|info| info.commit).collect()
            }
            Ok(_) => match bridge.get_log(history_limit) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{} Failed to get history: {}", "error:".red().bold(), e);
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!(
                    "{} Failed to get file history: {}",
                    "error:".red().bold(),
                    e
                );
                std::process::exit(1);
            }
        }
    } else {
        match bridge.get_log(history_limit) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{} Failed to get history: {}", "error:".red().bold(), e);
                std::process::exit(1);
            }
        }
    };

    if commits.is_empty() {
        eprintln!("{} No commits found", "warning:".yellow().bold());
        return;
    }
    commits.reverse();

    let total_commits = scanned_commit_count(commits.len(), opts.limit);
    let scanned_commits = scanned_commit_shas(&commits, opts.limit);
    let Some(seed) = find_seed_occurrence(
        &bridge,
        &registry,
        &commits,
        &opts.entity_name,
        git_file_path.as_deref(),
    ) else {
        eprintln!(
            "{} Entity '{}' not found in scanned history{}",
            "error:".red().bold(),
            opts.entity_name,
            git_file_path
                .as_ref()
                .map(|path| format!(" of {path}"))
                .unwrap_or_default()
        );
        std::process::exit(1);
    };

    let entity_type = seed.entity.entity_type.clone();
    let mut entries = trace_back_to_origin(&bridge, &registry, &commits, seed.clone());
    entries.extend(trace_forward_from_seed(&bridge, &registry, &commits, seed));
    if let Some(scanned_commits) = &scanned_commits {
        entries.retain(|entry| scanned_commits.contains(&entry.sha));
    }

    let first_seen = entries.first().map(|e| e.date.clone()).unwrap_or_default();
    // Use the last file the entity was seen in for the header
    let display_file = entries
        .iter()
        .rev()
        .find_map(|e| e.file_path.as_ref())
        .map(String::as_str)
        .or(git_file_path.as_deref())
        .unwrap_or("");
    // Check if entity ever moved between files
    let was_file = entries.iter().find_map(|e| {
        if matches!(e.change_type, EntityChangeType::Moved) {
            e.prev_file_path.as_ref().cloned()
        } else {
            None
        }
    });

    if opts.json {
        print_json(
            &opts.entity_name,
            display_file,
            &entity_type,
            &entries,
            opts.verbose,
        );
    } else {
        print_terminal(
            &opts.entity_name,
            display_file,
            was_file.as_deref(),
            &entity_type,
            &entries,
            total_commits,
            &first_seen,
            opts.verbose,
        );
    }
}

fn history_limit_with_baseline(limit: usize) -> usize {
    if limit == 0 {
        0
    } else {
        limit.saturating_add(1)
    }
}

fn scanned_commit_count(commit_count: usize, limit: usize) -> usize {
    if limit == 0 {
        commit_count
    } else {
        commit_count.min(limit)
    }
}

fn scanned_commit_shas(commits: &[CommitInfo], limit: usize) -> Option<HashSet<String>> {
    if limit == 0 || commits.len() <= limit {
        return None;
    }

    Some(
        commits[commits.len() - limit..]
            .iter()
            .map(|commit| commit.sha.clone())
            .collect(),
    )
}

fn find_seed_occurrence(
    bridge: &GitBridge,
    registry: &ParserRegistry,
    commits: &[CommitInfo],
    entity_name: &str,
    file_path: Option<&str>,
) -> Option<EntityOccurrence> {
    for (index, commit) in commits.iter().enumerate().rev() {
        let paths = match file_path {
            Some(path) => vec![path.to_string()],
            None => bridge
                .get_commit_changed_files(&commit.sha)
                .unwrap_or_default(),
        };

        for path in paths {
            if let Some(entity) =
                entity_by_name_at_ref(bridge, registry, &commit.sha, &path, entity_name)
            {
                return Some(EntityOccurrence {
                    commit_index: index,
                    file_path: path,
                    entity,
                });
            }
        }
    }

    None
}

fn trace_back_to_origin(
    bridge: &GitBridge,
    registry: &ParserRegistry,
    commits: &[CommitInfo],
    seed: EntityOccurrence,
) -> Vec<LogEntry> {
    let mut current = seed;
    let mut entries = Vec::new();

    for child_index in (1..=current.commit_index).rev() {
        let child_commit = &commits[child_index];
        let parent_commit = &commits[child_index - 1];
        let changed_paths = bridge
            .get_commit_changed_files(&child_commit.sha)
            .unwrap_or_default();

        let previous = find_related_entity_at_ref(
            bridge,
            registry,
            &parent_commit.sha,
            &current.file_path,
            &current.entity.name,
            current.entity.structural_hash.as_deref(),
            &changed_paths,
        );

        let Some(previous) = previous else {
            entries.push(added_entry(child_commit, &current));
            entries.reverse();
            return entries;
        };

        if let Some(entry) = transition_entry(child_commit, &previous, &current) {
            entries.push(entry);
        }

        current = EntityOccurrence {
            commit_index: child_index - 1,
            ..previous
        };
    }

    entries.push(added_entry(&commits[current.commit_index], &current));
    entries.reverse();
    entries
}

fn trace_forward_from_seed(
    bridge: &GitBridge,
    registry: &ParserRegistry,
    commits: &[CommitInfo],
    seed: EntityOccurrence,
) -> Vec<LogEntry> {
    let mut current = seed;
    let mut entries = Vec::new();

    for child_index in current.commit_index + 1..commits.len() {
        let child_commit = &commits[child_index];
        let changed_paths = bridge
            .get_commit_changed_files(&child_commit.sha)
            .unwrap_or_default();

        let next = find_related_entity_at_ref(
            bridge,
            registry,
            &child_commit.sha,
            &current.file_path,
            &current.entity.name,
            current.entity.structural_hash.as_deref(),
            &changed_paths,
        );

        let Some(next) = next else {
            entries.push(deleted_entry(child_commit, &current));
            break;
        };

        if let Some(entry) = transition_entry(child_commit, &current, &next) {
            entries.push(entry);
        }

        current = EntityOccurrence {
            commit_index: child_index,
            ..next
        };
    }

    entries
}

fn find_related_entity_at_ref(
    bridge: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    preferred_file: &str,
    entity_name: &str,
    structural_hash: Option<&str>,
    changed_paths: &[String],
) -> Option<EntityOccurrence> {
    let paths = candidate_paths(preferred_file, changed_paths);

    for path in &paths {
        if let Some(entity) = entity_by_name_at_ref(bridge, registry, sha, path, entity_name) {
            return Some(EntityOccurrence {
                commit_index: 0,
                file_path: path.clone(),
                entity,
            });
        }
    }

    let structural_hash = structural_hash?;
    for path in &paths {
        if let Some(entity) =
            entity_by_structural_hash_at_ref(bridge, registry, sha, path, structural_hash)
        {
            return Some(EntityOccurrence {
                commit_index: 0,
                file_path: path.clone(),
                entity,
            });
        }
    }

    None
}

fn candidate_paths(preferred_file: &str, changed_paths: &[String]) -> Vec<String> {
    let mut paths = vec![preferred_file.to_string()];
    for path in changed_paths {
        if !paths.iter().any(|existing| existing == path) {
            paths.push(path.clone());
        }
    }
    paths
}

fn entity_by_name_at_ref(
    bridge: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    file_path: &str,
    entity_name: &str,
) -> Option<SemanticEntity> {
    entities_at_ref(bridge, registry, sha, file_path)
        .into_iter()
        .find(|entity| entity.name == entity_name)
}

fn entity_by_structural_hash_at_ref(
    bridge: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    file_path: &str,
    structural_hash: &str,
) -> Option<SemanticEntity> {
    entities_at_ref(bridge, registry, sha, file_path)
        .into_iter()
        .find(|entity| entity.structural_hash.as_deref() == Some(structural_hash))
}

fn entities_at_ref(
    bridge: &GitBridge,
    registry: &ParserRegistry,
    sha: &str,
    file_path: &str,
) -> Vec<SemanticEntity> {
    bridge
        .read_file_at_ref(sha, file_path)
        .ok()
        .flatten()
        .map(|content| registry.extract_entities(file_path, &content))
        .unwrap_or_default()
}

fn transition_entry(
    commit: &CommitInfo,
    before: &EntityOccurrence,
    after: &EntityOccurrence,
) -> Option<LogEntry> {
    let file_changed = before.file_path != after.file_path;
    let name_changed = before.entity.name != after.entity.name;
    let content_changed = before.entity.content_hash != after.entity.content_hash;

    let change_type = if file_changed {
        EntityChangeType::Moved
    } else if name_changed {
        EntityChangeType::Renamed
    } else if content_changed {
        if structural_changed(&before.entity, &after.entity) {
            EntityChangeType::ModifiedLogic
        } else {
            EntityChangeType::ModifiedCosmetic
        }
    } else {
        return None;
    };

    Some(LogEntry {
        sha: commit.sha.clone(),
        short_sha: commit.short_sha.clone(),
        author: commit.author.clone(),
        date: chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0)),
        message: commit.message.lines().next().unwrap_or("").to_string(),
        change_type,
        content: Some(after.entity.content.clone()),
        prev_content: Some(before.entity.content.clone()),
        file_path: Some(after.file_path.clone()),
        prev_file_path: if file_changed {
            Some(before.file_path.clone())
        } else {
            None
        },
    })
}

fn structural_changed(before: &SemanticEntity, after: &SemanticEntity) -> bool {
    match (&before.structural_hash, &after.structural_hash) {
        (Some(before), Some(after)) => before != after,
        _ => true,
    }
}

fn added_entry(commit: &CommitInfo, occurrence: &EntityOccurrence) -> LogEntry {
    LogEntry {
        sha: commit.sha.clone(),
        short_sha: commit.short_sha.clone(),
        author: commit.author.clone(),
        date: chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0)),
        message: commit.message.lines().next().unwrap_or("").to_string(),
        change_type: EntityChangeType::Added,
        content: Some(occurrence.entity.content.clone()),
        prev_content: None,
        file_path: Some(occurrence.file_path.clone()),
        prev_file_path: None,
    }
}

fn deleted_entry(commit: &CommitInfo, occurrence: &EntityOccurrence) -> LogEntry {
    LogEntry {
        sha: commit.sha.clone(),
        short_sha: commit.short_sha.clone(),
        author: commit.author.clone(),
        date: chrono_lite_format(commit.date.parse::<i64>().unwrap_or(0)),
        message: commit.message.lines().next().unwrap_or("").to_string(),
        change_type: EntityChangeType::Deleted,
        content: None,
        prev_content: Some(occurrence.entity.content.clone()),
        file_path: Some(occurrence.file_path.clone()),
        prev_file_path: None,
    }
}

fn print_terminal(
    entity_name: &str,
    file_path: &str,
    was_file: Option<&str>,
    entity_type: &str,
    entries: &[LogEntry],
    total_commits: usize,
    first_seen: &str,
    verbose: bool,
) {
    let header = if let Some(prev) = was_file {
        format!(
            "┌─ {} :: {} :: {}  (was: {})",
            file_path, entity_type, entity_name, prev
        )
    } else {
        format!("┌─ {} :: {} :: {}", file_path, entity_type, entity_name)
    };
    println!("{}", header.bold());
    println!("│");

    let max_author_len = entries.iter().map(|e| e.author.len()).max().unwrap_or(6);
    let max_change_len = entries
        .iter()
        .map(|e| e.change_type.label().len())
        .max()
        .unwrap_or(10);

    for entry in entries {
        let msg_short = truncate_str(&entry.message, 50);

        println!(
            "│  {}  {:<max_author$}  {}  {:<max_change$}  {}",
            entry.short_sha.yellow(),
            entry.author.cyan(),
            entry.date.dimmed(),
            entry.change_type.label_colored(),
            msg_short,
            max_author = max_author_len,
            max_change = max_change_len,
        );

        // Show file transition for Moved entries
        if matches!(entry.change_type, EntityChangeType::Moved) {
            if let Some(new_fp) = &entry.file_path {
                println!("│    {}", format!("→ moved to {}", new_fp).blue());
            }
        }

        // Show rename info
        if matches!(entry.change_type, EntityChangeType::Renamed) {
            println!("│    {}", "→ entity renamed (structural hash match)".cyan());
        }

        if verbose {
            if let (Some(prev), Some(cur)) = (&entry.prev_content, &entry.content) {
                print_inline_diff(prev, cur);
            } else if let Some(cur) = &entry.content {
                for line in cur.lines() {
                    println!("│    {}", format!("+ {}", line).green());
                }
                println!("│");
            }
        }
    }

    println!("│");
    println!(
        "│  {}",
        format!(
            "{} changes across {} commits (first seen: {})",
            entries.len(),
            total_commits,
            first_seen
        )
        .dimmed()
    );
    println!("└{}", "─".repeat(60));
}

fn print_inline_diff(before: &str, after: &str) {
    use similar::TextDiff;

    let diff = TextDiff::from_lines(before, after);
    let mut has_changes = false;

    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Delete => {
                has_changes = true;
                print!("│    {}", format!("- {}", change).red());
            }
            similar::ChangeTag::Insert => {
                has_changes = true;
                print!("│    {}", format!("+ {}", change).green());
            }
            similar::ChangeTag::Equal => {} // skip unchanged lines in verbose diff
        }
    }

    if has_changes {
        println!("│");
    }
}

fn print_json(
    entity_name: &str,
    file_path: &str,
    entity_type: &str,
    entries: &[LogEntry],
    verbose: bool,
) {
    let json_entries: Vec<_> = entries
        .iter()
        .map(|e| {
            let mut obj = serde_json::json!({
                "commit": {
                    "sha": e.sha,
                    "author": e.author,
                    "date": e.date,
                    "message": e.message,
                },
                "change_type": e.change_type.label(),
                "structural_change": matches!(e.change_type, EntityChangeType::ModifiedLogic | EntityChangeType::Added),
            });

            if let Some(fp) = &e.file_path {
                obj["file_path"] = serde_json::Value::String(fp.clone());
            }
            if let Some(pfp) = &e.prev_file_path {
                obj["prev_file_path"] = serde_json::Value::String(pfp.clone());
            }

            if verbose {
                if let Some(content) = &e.content {
                    obj["after_content"] = serde_json::Value::String(content.clone());
                }
                if let Some(prev) = &e.prev_content {
                    obj["before_content"] = serde_json::Value::String(prev.clone());
                }
            }

            obj
        })
        .collect();

    let output = serde_json::json!({
        "entity": entity_name,
        "file": file_path,
        "type": entity_type,
        "changes": json_entries,
    });

    println!("{}", serde_json::to_string(&output).unwrap());
}

enum FindResult {
    Found(String),
    Ambiguous(Vec<String>),
    NotFound,
}

fn find_entity_file(
    root: &Path,
    registry: &sem_core::parser::registry::ParserRegistry,
    entity_name: &str,
) -> FindResult {
    let ext_filter: Vec<String> = vec![];
    let files = super::graph::find_supported_files_public(root, registry, &ext_filter);
    let mut found_in: Vec<String> = Vec::new();

    for file_path in &files {
        let full_path = root.join(file_path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let entities = registry.extract_entities(file_path, &content);
        if entities.iter().any(|e| e.name == entity_name) {
            found_in.push(file_path.clone());
        }
    }

    match found_in.len() {
        0 => FindResult::NotFound,
        1 => FindResult::Found(found_in.into_iter().next().unwrap()),
        _ => FindResult::Ambiguous(found_in),
    }
}

/// Simple timestamp formatting without external deps.
fn chrono_lite_format(unix_seconds: i64) -> String {
    let days = unix_seconds / 86400;
    let mut y = 1970i64;
    let mut remaining_days = days;

    loop {
        let year_days = if is_leap(y) { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        y += 1;
    }

    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut m = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md {
            m = i;
            break;
        }
        remaining_days -= md;
    }

    let d = remaining_days + 1;
    format!("{:04}-{:02}-{:02}", y, m + 1, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::parser::plugins::create_default_registry;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(repo: &TempDir, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo.path())
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn commit_all(repo: &TempDir, message: &str, timestamp: i64) {
        git(repo, &["add", "-A"]);
        let status = Command::new("git")
            .current_dir(repo.path())
            .env("GIT_AUTHOR_DATE", format!("@{timestamp}"))
            .env("GIT_COMMITTER_DATE", format!("@{timestamp}"))
            .args(["commit", "-q", "-m", message])
            .status()
            .unwrap();
        assert!(status.success(), "git commit failed");
    }

    fn rename_history_repo() -> TempDir {
        let repo = TempDir::new().unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "test"]);

        fs::write(repo.path().join("a.py"), "def original(): return 1\n").unwrap();
        commit_all(&repo, "v1", 946684800);

        fs::write(repo.path().join("a.py"), "def renamed_func(): return 1\n").unwrap();
        commit_all(&repo, "v2: rename function", 946771200);

        git(&repo, &["mv", "a.py", "b.py"]);
        commit_all(&repo, "v3: move file", 946857600);

        fs::write(repo.path().join("b.py"), "def renamed_func(): return 2\n").unwrap();
        commit_all(&repo, "v4: modify body", 946944000);

        repo
    }

    fn log_entries(repo: &TempDir, entity_name: &str, file_path: &str) -> Vec<LogEntry> {
        let bridge = GitBridge::open(repo.path()).unwrap();
        let registry = create_default_registry();
        let mut commits = bridge.get_log(0).unwrap();
        commits.reverse();
        let seed = find_seed_occurrence(&bridge, &registry, &commits, entity_name, Some(file_path))
            .unwrap();

        let mut entries = trace_back_to_origin(&bridge, &registry, &commits, seed.clone());
        entries.extend(trace_forward_from_seed(&bridge, &registry, &commits, seed));
        entries
    }

    #[test]
    fn log_traces_entity_and_file_renames_from_current_name() {
        let repo = rename_history_repo();
        let entries = log_entries(&repo, "renamed_func", "b.py");
        let labels: Vec<_> = entries
            .iter()
            .map(|entry| entry.change_type.label())
            .collect();

        assert_eq!(
            labels,
            vec!["added", "renamed", "moved", "modified (logic)"]
        );
        assert_eq!(entries[0].file_path.as_deref(), Some("a.py"));
        assert_eq!(entries[2].prev_file_path.as_deref(), Some("a.py"));
        assert_eq!(entries[2].file_path.as_deref(), Some("b.py"));
    }

    #[test]
    fn log_traces_forward_from_historical_name() {
        let repo = rename_history_repo();
        let entries = log_entries(&repo, "original", "a.py");
        let labels: Vec<_> = entries
            .iter()
            .map(|entry| entry.change_type.label())
            .collect();

        assert_eq!(
            labels,
            vec!["added", "renamed", "moved", "modified (logic)"]
        );
        assert_eq!(
            entries.last().and_then(|e| e.file_path.as_deref()),
            Some("b.py")
        );
    }
}
