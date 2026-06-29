use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use colored::Colorize;
use serde::Deserialize;

use sem_core::git::bridge::GitBridge;

// Transport, credentials, repo resolution, caching, and the metered graph API
// all live in the shared `sem-cloud-client` crate so the CLI and the MCP server
// route to the cloud identically. This file keeps only the CLI-facing pieces:
// the interactive login/logout/whoami flows and the colored output for each
// cloud-accelerated command.
pub use sem_cloud_client::load_credentials;
use sem_cloud_client::{
    credentials_path, default_endpoint, known_small_repo, load_repo_cache, normalize_remote_url,
    save_credentials, CloudClient, CloudCredentials, CloudEntityBrief, CloudHistoryEntry,
};

use super::context::ContextOptions;
use super::entities::EntitiesOptions;
use super::impact::ImpactOptions;
use super::log::LogOptions;

const GITHUB_CLIENT_ID: &str = "Ov23lioE75FJYz4Mn7ZH";

// ─── Cloud conversion nudge ─────────────────────────────────────────────────

/// After a `sem diff` that had real entity changes, print one dimmed line
/// suggesting `sem login` to see what those changes break across repos — a
/// cross-repo question a local single-repo diff cannot answer. Heavily
/// guard-railed so it can never become noise:
///   * silent when there were no entity changes,
///   * silent unless stderr is an interactive terminal (skips CI, pipes, agents),
///   * silent when already logged in,
///   * shown at most once a week (throttled via `~/.sem/.login_hint`).
///
/// Printed to stderr so it never pollutes stdout / piped / `--json` output.
pub fn maybe_suggest_cloud_after_diff(entity_changes: usize) {
    if entity_changes == 0 {
        return;
    }
    if !io::stderr().is_terminal() {
        return;
    }
    if load_credentials().is_some() {
        return;
    }
    if !login_hint_due() {
        return;
    }
    let noun = if entity_changes == 1 {
        "entity"
    } else {
        "entities"
    };
    eprintln!(
        "{} {} changed. {} to see what they break across your repos.",
        "↗".cyan(),
        format!("{entity_changes} {noun}").dimmed(),
        "sem login".cyan().bold(),
    );
    mark_login_hint_shown();
}

fn login_hint_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join(".login_hint"))
}

/// True if the hint hasn't been shown in the last week (or ever).
fn login_hint_due() -> bool {
    const THROTTLE_SECS: u64 = 7 * 24 * 3600;
    let Some(path) = login_hint_path() else {
        return false;
    };
    match fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(last) => now_secs().saturating_sub(last) >= THROTTLE_SECS,
        None => true,
    }
}

fn mark_login_hint_shown() {
    let Some(path) = login_hint_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(&path, now_secs().to_string());
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── sem login ────────────────────────────────────────────────────────────

pub fn login(
    api_key: Option<String>,
    endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let key = match api_key {
        Some(k) => k,
        None => {
            if let Some(creds) = load_credentials() {
                println!(
                    "{} Already logged in to {}",
                    "ok".green().bold(),
                    creds.endpoint
                );
                println!(
                    "  Run {} to log in with a different account.",
                    "sem logout".bold()
                );
                return Ok(());
            }
            return login_github(endpoint);
        }
    };

    if !key.starts_with("sk_live_") {
        eprintln!(
            "{} Key doesn't start with sk_live_ — are you sure this is correct?",
            "warning:".yellow().bold()
        );
    }

    let ep = endpoint.unwrap_or_else(default_endpoint);
    let creds = CloudCredentials {
        api_key: key,
        endpoint: ep.clone(),
    };

    let path = save_credentials(&creds)?;
    println!("{} Logged in to {}", "ok".green().bold(), ep);
    println!("  Credentials saved to {}", path.display());
    println!("  Cloud-accelerated commands are now active for registered repos.");

    Ok(())
}

// ─── sem login --github ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

pub fn login_github(endpoint: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let ep = endpoint.unwrap_or_else(default_endpoint);
    let client_id =
        std::env::var("SEM_GITHUB_CLIENT_ID").unwrap_or_else(|_| GITHUB_CLIENT_ID.into());

    let device_resp: DeviceCodeResponse = ureq::post("https://github.com/login/device/code")
        .set("Accept", "application/json")
        .send_form(&[
            ("client_id", &client_id),
            ("scope", &"user:email".to_string()),
        ])?
        .into_json()?;

    let interval = Duration::from_secs(device_resp.interval.unwrap_or(5));

    println!();
    println!(
        "  Open {} in your browser",
        device_resp.verification_uri.bold()
    );
    println!("  and enter code: {}", device_resp.user_code.cyan().bold());
    println!();

    let _ = open_url(&device_resp.verification_uri);

    eprint!("{}", "Waiting for authorization...".dimmed());
    io::stderr().flush()?;

    let access_token = loop {
        thread::sleep(interval);

        let resp: TokenResponse = ureq::post("https://github.com/login/oauth/access_token")
            .set("Accept", "application/json")
            .send_form(&[
                ("client_id", client_id.as_str()),
                ("device_code", &device_resp.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])?
            .into_json()?;

        match (resp.access_token, resp.error.as_deref()) {
            (Some(token), _) => break token,
            (_, Some("authorization_pending")) => {
                eprint!(".");
                io::stderr().flush()?;
                continue;
            }
            (_, Some("slow_down")) => {
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            (_, Some("expired_token")) => {
                eprintln!();
                return Err("Device code expired. Please try again.".into());
            }
            (_, Some("access_denied")) => {
                eprintln!();
                return Err("Authorization denied.".into());
            }
            (_, Some(err)) => {
                eprintln!();
                return Err(format!("GitHub error: {err}").into());
            }
            _ => continue,
        }
    };
    eprintln!(" {}", "authorized".green());

    let creds = CloudCredentials {
        api_key: access_token,
        endpoint: ep.clone(),
    };

    let path = save_credentials(&creds)?;
    println!("{} Logged in to {} via GitHub", "ok".green().bold(), ep);
    println!("  Credentials saved to {}", path.display());
    println!("  Cloud-accelerated commands are now active for registered repos.");

    Ok(())
}

fn open_url(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn()?;
    }
    Ok(())
}

// ─── sem logout ──────────────────────────────────────────────────────────

pub fn logout() -> Result<(), Box<dyn std::error::Error>> {
    let path = credentials_path().ok_or("Could not determine home directory")?;

    if path.exists() {
        fs::remove_file(&path)?;
        println!("{} Logged out — credentials removed", "ok".green().bold());
    } else {
        println!(
            "{} No credentials found — already logged out",
            "ok".green().bold()
        );
    }

    Ok(())
}

// ─── sem whoami ──────────────────────────────────────────────────────────

pub fn whoami() -> Result<(), Box<dyn std::error::Error>> {
    let creds = load_credentials().ok_or("Not logged in. Run: sem login")?;

    let masked = if creds.api_key.len() > 16 {
        format!(
            "{}...{}",
            &creds.api_key[..12],
            &creds.api_key[creds.api_key.len() - 4..]
        )
    } else {
        creds.api_key.clone()
    };

    println!("{} {}", "Endpoint:".bold(), creds.endpoint);
    println!("{} {}", "API Key: ".bold(), masked);

    // Show repo mapping if in a git repo
    if let Ok(git) = GitBridge::open(Path::new(".")) {
        if let Some(remote) = git.get_remote_url() {
            let normalized = normalize_remote_url(&remote);
            println!("{} {}", "Remote:  ".bold(), normalized);
            if let Some(cached) = load_repo_cache().and_then(|c| c.get(&normalized).cloned()) {
                println!(
                    "{} {} ({})",
                    "Repo ID: ".bold(),
                    cached.repo_id,
                    cached.status
                );
            } else {
                println!(
                    "{} {} {}",
                    "Repo ID: ".bold(),
                    "not registered".dimmed(),
                    "(registers automatically on first sem impact/context/log)".dimmed()
                );
            }
        }
    }

    Ok(())
}

// ─── Cloud banner ────────────────────────────────────────────────────────

/// Print `(using sem cloud)` to stderr on the first cloud call per session.
static CLOUD_BANNER_SHOWN: AtomicBool = AtomicBool::new(false);

fn show_cloud_banner() {
    if !CLOUD_BANNER_SHOWN.swap(true, Ordering::Relaxed) {
        eprintln!("{}", "(using sem cloud)".dimmed());
    }
}

/// Show cross-repo dependencies across all of your indexed repos. Cloud-only:
/// the local CLI only ever sees one repo, so "what in my other repos depends on
/// this" is a question only the cloud (which holds the graph across all your
/// repos) can answer. This is a reason to log in.
pub fn xref(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = CloudClient::from_credentials()
        .ok_or("Not logged in. Cross-repo dependencies are a sem cloud feature. Run: sem login")?;
    let resp = client.cross_deps()?;

    if json {
        let edges: Vec<serde_json::Value> = resp
            .edges
            .iter()
            .map(|e| {
                serde_json::json!({
                    "fromRepoId": e.from_repo_id,
                    "fromEntity": e.from_entity_id,
                    "toRepoId": e.to_repo_id,
                    "toEntity": e.to_entity_id,
                    "refType": e.ref_type,
                })
            })
            .collect();
        let out = serde_json::json!({ "edges": edges, "total": resp.total });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if resp.edges.is_empty() {
        println!("No cross-repo dependencies found.");
        println!(
            "{}",
            "Cross-repo edges appear once you have 2+ repos indexed in sem cloud.".dimmed()
        );
        return Ok(());
    }

    println!(
        "{} ({} edges, {}ms)",
        "Cross-repo dependencies".bold(),
        resp.total,
        resp.query_ms
    );
    for e in &resp.edges {
        let kind = if e.ref_type.is_empty() {
            String::new()
        } else {
            format!("  [{}]", e.ref_type)
        };
        println!(
            "  {} {} {}{}",
            e.from_entity_id,
            "→".dimmed(),
            e.to_entity_id.bold(),
            kind.dimmed()
        );
    }
    Ok(())
}

// ─── try_cloud_* helpers ─────────────────────────────────────────────────

/// Attempt to open GitBridge and get remote URL for cloud resolution.
fn cloud_git_context(cwd: &str) -> Option<(GitBridge, String)> {
    let git = GitBridge::open(Path::new(cwd)).ok()?;
    let remote = git.get_remote_url()?;
    Some((git, remote))
}

/// Try to run `sem impact` via cloud. Returns Some(()) on success.
pub fn try_cloud_impact(opts: &ImpactOptions) -> Option<()> {
    // --tests needs test classification data the cloud API doesn't expose.
    if matches!(opts.mode, super::impact::ImpactMode::Tests) {
        return None;
    }
    let client = CloudClient::from_credentials()?;
    let (git, remote) = cloud_git_context(&opts.cwd)?;
    // Small repos answer graph queries faster from the local cache.
    if known_small_repo(&remote) {
        return None;
    }
    let repo_id = client.ensure_repo(&remote).ok()?;
    let entity_name = opts.entity_name.as_deref()?;
    // Server resolves by name + repo-relative file, with name-only fallback
    // when the file is empty or doesn't match.
    let file_hint = opts
        .file_hint
        .as_deref()
        .map(|f| super::normalize_repo_relative_path(Path::new(&opts.cwd), git.repo_root(), f))
        .unwrap_or_default();
    let result = client.impact(&repo_id, entity_name, &file_hint).ok()?;

    show_cloud_banner();

    let deps_json = || -> Vec<serde_json::Value> {
        result.dependencies.iter().map(entity_brief_json).collect()
    };
    let dependents_json =
        || -> Vec<serde_json::Value> { result.dependents.iter().map(entity_brief_json).collect() };

    let print_deps_section = || {
        if !result.dependencies.is_empty() {
            println!("\n  {} {}", "→".blue(), "depends on:".dimmed());
            for dep in &result.dependencies {
                println!(
                    "    {} {} {} ({})",
                    "→".blue(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
    };
    let print_dependents_section = || {
        if !result.dependents.is_empty() {
            println!("\n  {} {}", "←".yellow(), "depended on by:".dimmed());
            for dep in &result.dependents {
                println!(
                    "    {} {} {} ({})",
                    "←".yellow(),
                    dep.entity_type.dimmed(),
                    dep.name.bold(),
                    dep.file_path.dimmed(),
                );
            }
        }
    };
    let print_header = || {
        println!(
            "{} {}{}",
            "⊕".green(),
            entity_name.bold(),
            if file_hint.is_empty() {
                String::new()
            } else {
                format!(" ({})", file_hint.dimmed())
            },
        );
    };

    match opts.mode {
        super::impact::ImpactMode::Deps => {
            if opts.json {
                let output = serde_json::json!({
                    "entity": { "name": entity_name, "file": file_hint },
                    "dependencies": deps_json(),
                });
                println!("{}", serde_json::to_string(&output).unwrap());
            } else {
                print_header();
                if result.dependencies.is_empty() {
                    println!("\n  {} {}", "✓".green().bold(), "No dependencies.".dimmed());
                } else {
                    print_deps_section();
                }
                println!();
            }
        }
        super::impact::ImpactMode::Dependents => {
            if opts.json {
                let output = serde_json::json!({
                    "entity": { "name": entity_name, "file": file_hint },
                    "dependents": dependents_json(),
                });
                println!("{}", serde_json::to_string(&output).unwrap());
            } else {
                print_header();
                if result.dependents.is_empty() {
                    println!("\n  {} {}", "✓".green().bold(), "No dependents.".dimmed());
                } else {
                    print_dependents_section();
                }
                println!();
            }
        }
        _ => {
            // ImpactMode::All (Tests already returned None above)
            if opts.json {
                let impact_json: Vec<serde_json::Value> = result
                    .transitive_impact
                    .iter()
                    .map(entity_brief_json)
                    .collect();
                let output = serde_json::json!({
                    "entity": { "name": entity_name, "file": file_hint },
                    "dependencies": deps_json(),
                    "dependents": dependents_json(),
                    "impact": {
                        "total": impact_json.len(),
                        "entities": impact_json,
                    },
                    "tests": [],
                });
                println!("{}", serde_json::to_string(&output).unwrap());
            } else {
                print_header();
                print_deps_section();
                print_dependents_section();

                if !result.transitive_impact.is_empty() {
                    println!(
                        "\n  {} {}",
                        "!".red().bold(),
                        format!(
                            "{} entities transitively affected:",
                            result.transitive_impact.len()
                        )
                        .red(),
                    );
                    for imp in &result.transitive_impact {
                        println!(
                            "    {} {} {} ({})",
                            "→".red(),
                            imp.entity_type.dimmed(),
                            imp.name.bold(),
                            imp.file_path.dimmed(),
                        );
                    }
                } else if result.dependencies.is_empty() && result.dependents.is_empty() {
                    println!(
                        "\n  {} {}",
                        "✓".green().bold(),
                        "No dependencies or dependents found.".dimmed()
                    );
                }

                println!();
            }
        }
    }

    Some(())
}

/// Try to run `sem context` via cloud.
pub fn try_cloud_context(opts: &ContextOptions) -> Option<()> {
    let client = CloudClient::from_credentials()?;
    let (git, remote) = cloud_git_context(&opts.cwd)?;
    // Small repos answer graph queries faster from the local cache.
    if known_small_repo(&remote) {
        return None;
    }
    let repo_id = client.ensure_repo(&remote).ok()?;
    let entity_name = opts.entity_name.as_deref()?;
    let file_path = opts
        .file_path
        .as_deref()
        .map(|f| super::normalize_repo_relative_path(Path::new(&opts.cwd), git.repo_root(), f))
        .unwrap_or_default();
    let result = client
        .context(&repo_id, entity_name, &file_path, opts.budget)
        .ok()?;

    show_cloud_banner();

    if opts.json {
        let entries: Vec<serde_json::Value> = result
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "entityId": e.entity_id,
                    "name": e.name,
                    "type": e.entity_type,
                    "file": e.file_path,
                    "role": e.role,
                    "tokens": e.estimated_tokens,
                    "content": e.content,
                })
            })
            .collect();
        let output = serde_json::json!({
            "entity": entity_name,
            "budget": opts.budget,
            "total_tokens": result.tokens_used,
            "truncated": result.truncated,
            "entries": entries,
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        println!(
            "{} {} (budget: {}, used: {})\n",
            "context for".green().bold(),
            entity_name.bold(),
            opts.budget,
            result.tokens_used,
        );

        let mut current_role = String::new();
        for entry in &result.entries {
            if entry.role != current_role {
                current_role.clone_from(&entry.role);
                let role_label = match current_role.as_str() {
                    "target" => "target".green().bold(),
                    "direct_dependency" => "direct dependencies".cyan().bold(),
                    "direct_dependent" => "direct dependents".yellow().bold(),
                    "transitive_dependency" => "transitive dependencies".blue().bold(),
                    "transitive_dependent" => "transitive dependents".dimmed().bold(),
                    _ => current_role.normal().bold(),
                };
                println!("  {}:", role_label);
            }

            let snippet: String = entry.content.lines().next().unwrap_or("").to_string();
            println!(
                "    {} {} ({}, ~{} tokens)",
                entry.entity_type.dimmed(),
                entry.name.bold(),
                entry.file_path.dimmed(),
                entry.estimated_tokens,
            );
            if !snippet.is_empty() {
                println!("      {}", snippet.dimmed());
            }
        }
    }

    Some(())
}

/// Try to run `sem entities` via cloud (whole-repo directory listing only).
pub fn try_cloud_entities(opts: &EntitiesOptions) -> Option<()> {
    // Only used for a single path arg (the whole-repo listing case); callers
    // skip this fast-path entirely when multiple paths are given.
    let path_arg = opts
        .paths
        .iter()
        .map(|p| p.trim())
        .find(|p| !p.is_empty())
        .unwrap_or(".");
    let full_path = if Path::new(path_arg).is_absolute() {
        PathBuf::from(path_arg)
    } else {
        Path::new(&opts.cwd).join(path_arg)
    };
    if full_path.is_file() {
        return None; // Single-file extraction stays local
    }

    let client = CloudClient::from_credentials()?;
    let (git, remote) = cloud_git_context(&opts.cwd)?;
    // Subdirectory listings parse few files — local wins those (measured
    // 46ms local vs 138ms cloud). Cloud only pays off for whole-repo
    // listings of large repos, where local re-parses everything.
    let normalized =
        super::normalize_repo_relative_path(Path::new(&opts.cwd), git.repo_root(), path_arg);
    if normalized != "." || known_small_repo(&remote) {
        return None;
    }
    let repo_id = client.ensure_repo(&remote).ok()?;
    let resp = client.entities(&repo_id, None).ok()?;
    let mut entities = resp.entities;
    entities.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.entity_type.cmp(&b.entity_type))
            .then(a.name.cmp(&b.name))
    });

    show_cloud_banner();

    if opts.json {
        let output: Vec<serde_json::Value> = entities
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "type": e.entity_type,
                    "start_line": e.start_line,
                    "end_line": e.end_line,
                    "parent_id": e.parent_id,
                    "file": e.file_path,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        println!("{} {}\n", "entities:".green().bold(), path_arg.bold());
        let mut current_file: Option<&str> = None;
        for entity in &entities {
            if current_file != Some(entity.file_path.as_str()) {
                current_file = Some(entity.file_path.as_str());
                println!("  {}", entity.file_path.bold());
            }
            println!(
                "    {} {} (L{}:{})",
                entity.entity_type.dimmed(),
                entity.name.bold(),
                entity.start_line.unwrap_or(0),
                entity.end_line.unwrap_or(0),
            );
        }
    }

    Some(())
}

/// Try to run `sem log` via cloud.
pub fn try_cloud_log(opts: &LogOptions) -> Option<()> {
    let client = CloudClient::from_credentials()?;
    let (git, remote) = cloud_git_context(&opts.cwd)?;
    let repo_id = client.ensure_repo(&remote).ok()?;
    let file_filter = opts
        .file_path
        .as_deref()
        .map(|f| super::normalize_repo_relative_path(Path::new(&opts.cwd), git.repo_root(), f));
    // The server filters by file path only; pull a generous window and
    // filter to the requested entity name client-side.
    let result = client
        .history(&repo_id, file_filter.as_deref(), 10000)
        .ok()?;

    let mut changes: Vec<&CloudHistoryEntry> = result
        .changes
        .iter()
        .filter(|e| e.entity_name == opts.entity_name)
        .collect();
    // Server returns newest-first; local prints oldest-first.
    changes.reverse();
    if opts.limit > 0 && changes.len() > opts.limit {
        changes.truncate(opts.limit);
    }

    if changes.is_empty() {
        return None; // Fall back to local if cloud has no history for this entity
    }

    show_cloud_banner();

    if opts.json {
        let json_entries: Vec<serde_json::Value> = changes
            .iter()
            .map(|e| {
                serde_json::json!({
                    "commit": {
                        "sha": e.commit_sha,
                        "author": e.commit_author.as_deref().unwrap_or(""),
                        "message": e.commit_message.as_deref().unwrap_or(""),
                        "date": e.created_at,
                    },
                    "change_type": e.change_type,
                    "file_path": e.file_path,
                })
            })
            .collect();
        let output = serde_json::json!({
            "entity": opts.entity_name,
            "file": changes.last().map(|e| e.file_path.as_str()).unwrap_or(""),
            "type": changes.first().map(|e| e.entity_type.as_str()).unwrap_or(""),
            "changes": json_entries,
        });
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        let entity_type = changes
            .first()
            .map(|e| e.entity_type.as_str())
            .unwrap_or("");
        let file_path = changes.last().map(|e| e.file_path.as_str()).unwrap_or("");

        println!(
            "{}",
            format!(
                "┌─ {} :: {} :: {}",
                file_path, entity_type, opts.entity_name
            )
            .bold()
        );
        println!("│");

        for entry in &changes {
            let short_sha = if entry.commit_sha.len() >= 7 {
                &entry.commit_sha[..7]
            } else {
                &entry.commit_sha
            };
            let msg = super::truncate_str(entry.commit_message.as_deref().unwrap_or(""), 50);
            println!(
                "│  {}  {}  {}  {}",
                short_sha.yellow(),
                entry.commit_author.as_deref().unwrap_or("unknown").cyan(),
                entry.change_type.dimmed(),
                msg,
            );
        }

        println!("│");
        println!("│  {}", format!("{} changes", changes.len()).dimmed());
        println!("└{}", "─".repeat(60));
    }

    Some(())
}

// ─── Helper to convert CloudEntityBrief to JSON ─────────────────────────

fn entity_brief_json(e: &CloudEntityBrief) -> serde_json::Value {
    serde_json::json!({
        "entityId": e.id,
        "name": e.name,
        "type": e.entity_type,
        "file": e.file_path,
    })
}
