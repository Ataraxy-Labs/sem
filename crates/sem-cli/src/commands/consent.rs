//! Cloud consent state and the `sem cloud` command surface.
//!
//! Cloud is OFF until the user runs `sem cloud enable` (public repo) or
//! `sem cloud share` (private repo). No repo URL or query leaves the machine
//! until then. Consent is a command, never a prompt injected into a query.
//!
//! State lives in `~/.sem/cloud.json`; every outbound request is also appended
//! to a local ledger (`~/.sem/cloud-log.jsonl`) that `sem cloud log` prints.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use sem_core::git::bridge::GitBridge;

use super::cloud::{self, normalize_remote_url, CloudClient};

// ─── Consent state ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsentState {
    /// Public repo, cloud queries allowed.
    Enabled,
    /// Private repo, parsed index uploaded (explicit `sem cloud share`).
    Shared,
    /// User asked never to be prompted for this repo.
    Never,
}

impl ConsentState {
    fn as_str(self) -> &'static str {
        match self {
            ConsentState::Enabled => "enabled",
            ConsentState::Shared => "shared",
            ConsentState::Never => "never",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "enabled" => Some(ConsentState::Enabled),
            "shared" => Some(ConsentState::Shared),
            "never" => Some(ConsentState::Never),
            _ => None,
        }
    }

    /// Whether cloud serving is permitted for a repo in this state.
    pub fn is_active(self) -> bool {
        matches!(self, ConsentState::Enabled | ConsentState::Shared)
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct RepoConsent {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    at: String,
    /// Whether the passive discovery tip has already been shown for this repo.
    #[serde(default)]
    tipped: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct CloudConfig {
    #[serde(default)]
    repos: BTreeMap<String, RepoConsent>,
    /// Global suppression of the discovery tip (`sem cloud never` outside a repo).
    #[serde(default)]
    tip_suppressed: bool,
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join("cloud.json"))
}

fn load_config() -> CloudConfig {
    config_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &CloudConfig) {
    let Some(path) = config_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, serde_json::to_string_pretty(cfg).unwrap_or_default());
}

fn now_secs_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

// ─── Public consent queries (used by the cloud routing path) ────────────────

/// The stored consent state for a repo, if any decision has been recorded.
pub fn state_for(remote_url: &str) -> Option<ConsentState> {
    let normalized = normalize_remote_url(remote_url);
    load_config()
        .repos
        .get(&normalized)
        .and_then(|r| r.state.as_deref())
        .and_then(ConsentState::parse)
}

/// True when cloud serving is permitted for this repo: either the user enabled
/// or shared it, or `SEM_CLOUD=1` forces it on (the CI / code-reviewed path,
/// where consent lives in the environment rather than an interactive prompt).
pub fn cloud_enabled_for(remote_url: &str) -> bool {
    if std::env::var("SEM_CLOUD").is_ok_and(|v| v == "1") {
        return true;
    }
    state_for(remote_url).is_some_and(|s| s.is_active())
}

fn set_state(remote_url: &str, state: ConsentState) {
    let normalized = normalize_remote_url(remote_url);
    let mut cfg = load_config();
    let entry = cfg.repos.entry(normalized).or_default();
    entry.state = Some(state.as_str().to_string());
    entry.at = now_secs_string();
    save_config(&cfg);
}

fn clear_state(remote_url: &str) {
    let normalized = normalize_remote_url(remote_url);
    let mut cfg = load_config();
    cfg.repos.remove(&normalized);
    save_config(&cfg);
}

// ─── Outbound request ledger ────────────────────────────────────────────────

fn ledger_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join("cloud-log.jsonl"))
}

/// Append one outbound request to the local ledger. Best-effort; never fails a
/// command. `kind` is e.g. "impact"/"context"/"register"/"forget", `detail` is
/// the entity or file the query was about (never file contents).
pub fn record_outbound(remote_url: &str, kind: &str, detail: &str) {
    let Some(path) = ledger_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let line = serde_json::json!({
        "ts": now_secs_string(),
        "repo": normalize_remote_url(remote_url),
        "kind": kind,
        "detail": detail,
    });
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn git_remote(cwd: &str) -> Option<String> {
    let git = GitBridge::open(Path::new(cwd)).ok()?;
    git.get_remote_url()
}

/// A short human display name for a repo: `owner/repo` for GitHub, else the
/// last path segment of the normalized URL.
fn display_name(remote_url: &str) -> String {
    let normalized = normalize_remote_url(remote_url);
    if let Some(rest) = normalized.split("github.com/").nth(1) {
        return rest.to_string();
    }
    normalized
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(&normalized)
        .to_string()
}

fn read_line() -> String {
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().to_string()
}

fn require_login() -> Option<cloud::CloudCredentials> {
    match cloud::load_credentials() {
        Some(c) => Some(c),
        None => {
            eprintln!(
                "{} Not logged in. Run {} first.",
                "error:".red().bold(),
                "sem login".bold()
            );
            None
        }
    }
}

fn require_remote(cwd: &str) -> Option<String> {
    match git_remote(cwd) {
        Some(r) => Some(r),
        None => {
            eprintln!(
                "{} No git remote here. Cloud works on repos with a remote URL.",
                "error:".red().bold()
            );
            None
        }
    }
}

// ─── sem cloud enable ───────────────────────────────────────────────────────

pub fn enable(cwd: &str) {
    if require_login().is_none() {
        return;
    }
    let Some(remote) = require_remote(cwd) else {
        return;
    };
    let name = display_name(&remote);

    match state_for(&remote) {
        Some(ConsentState::Enabled) => {
            println!(
                "{} Cloud is already enabled for {}.",
                "ok".green().bold(),
                name.bold()
            );
            return;
        }
        Some(ConsentState::Shared) => {
            println!(
                "{} {} is already shared as a private repo. Cloud queries are on.",
                "ok".green().bold(),
                name.bold()
            );
            return;
        }
        _ => {}
    }

    let Some(client) = CloudClient::from_credentials() else {
        eprintln!(
            "{} Cloud is disabled by environment (SEM_LOCAL / SEM_NO_NETWORK).",
            "error:".red().bold()
        );
        return;
    };

    // `enable` is the public-repo path. If the repo isn't confirmably public,
    // steer the user to the louder private flow instead of silently uploading.
    if !client.repo_is_public(&remote) {
        eprintln!(
            "{} {} doesn't look like a public GitHub repo.",
            "note:".yellow().bold(),
            name.bold()
        );
        eprintln!(
            "  To upload a private repo's index, use {} (it asks for extra confirmation).",
            "sem cloud share".bold()
        );
        return;
    }

    println!(
        "Cloud queries send: {} It is stored to serve faster queries.",
        "this repo's URL + the entity/file names you query, linked to your account.".bold()
    );
    println!(
        "This is {} — it's tied to your sem account.",
        "not anonymous".yellow()
    );
    println!();
    println!("{}", "No query has left this machine yet.".green());
    println!();
    print!("Enable cloud queries for {}? [y/N] ", name.bold());
    let _ = io::stdout().flush();

    let answer = read_line().to_lowercase();
    if answer != "y" && answer != "yes" {
        println!("{} Staying local. Nothing was sent.", "ok".green().bold());
        return;
    }

    set_state(&remote, ConsentState::Enabled);
    record_outbound(&remote, "register", "public");

    println!(
        "{} Cloud enabled for {}. Registering…",
        "ok".green().bold(),
        name.bold()
    );
    match client.register(&remote, "public") {
        Ok(repo) => report_repo_status(&repo),
        Err(e) => eprintln!(
            "{} Registered consent locally, but cloud registration failed: {e}\n  It will retry on your next impact/context.",
            "warning:".yellow().bold()
        ),
    }
}

// ─── sem cloud share ────────────────────────────────────────────────────────

pub fn share(cwd: &str) {
    if require_login().is_none() {
        return;
    }
    let Some(remote) = require_remote(cwd) else {
        return;
    };
    let name = display_name(&remote);

    if matches!(state_for(&remote), Some(ConsentState::Shared)) {
        println!("{} {} is already shared.", "ok".green().bold(), name.bold());
        return;
    }

    println!(
        "Sharing a {} repo uploads its parsed index to sem cloud:",
        "PRIVATE".red().bold()
    );
    println!(
        "  • {} per tenant. {}: sem can technically read it.",
        "encrypted at rest, isolated".bold(),
        "NOT end-to-end".red().bold()
    );
    println!(
        "  • undo anytime — delete everything with {}.",
        "sem cloud forget".bold()
    );
    println!();
    print!("Type the repo name to confirm ({}): ", name.bold());
    let _ = io::stdout().flush();

    let typed = read_line();
    if typed != name {
        println!(
            "{} Names didn't match. Staying local. Nothing was sent.",
            "ok".green().bold()
        );
        return;
    }

    let Some(client) = CloudClient::from_credentials() else {
        eprintln!(
            "{} Cloud is disabled by environment (SEM_LOCAL / SEM_NO_NETWORK).",
            "error:".red().bold()
        );
        return;
    };

    set_state(&remote, ConsentState::Shared);
    record_outbound(&remote, "register", "private");

    println!(
        "{} Sharing {}. Registering…",
        "ok".green().bold(),
        name.bold()
    );
    match client.register(&remote, "private") {
        Ok(repo) => report_repo_status(&repo),
        Err(e) => eprintln!(
            "{} Recorded consent locally, but cloud registration failed: {e}",
            "warning:".yellow().bold()
        ),
    }
}

fn report_repo_status(repo: &cloud::CloudRepoResponse) {
    let status = if repo.status.is_empty() {
        "pending"
    } else {
        &repo.status
    };
    match status {
        "ready" => println!(
            "  {} indexed — impact/context now answer from the cloud.",
            "ready:".green().bold()
        ),
        _ => println!(
            "  {} indexing in the background; queries fall back to local until it's ready.",
            format!("{status}:").dimmed()
        ),
    }
}

// ─── sem cloud list ───────────────────────────────────────────────────────

/// Show every repo indexed under the logged-in account.
pub fn list(cwd: &str) {
    let Some(client) = CloudClient::from_credentials() else {
        eprintln!(
            "{} Not logged in (or cloud disabled). Run {} first.",
            "error:".red().bold(),
            "sem login".bold()
        );
        return;
    };

    let repos = match client.list_repos() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} couldn't list cloud repos: {e}", "error:".red().bold());
            return;
        }
    };

    if repos.is_empty() {
        println!(
            "{} No repos indexed in your account yet. Turn one on with {}.",
            "ok".green().bold(),
            "sem cloud enable".bold()
        );
        return;
    }

    // Which repo (if any) is the one we're standing in.
    let here = git_remote(cwd).map(|r| normalize_remote_url(&r));

    println!(
        "{}",
        format!("Repos indexed in your account · {}", repos.len()).bold()
    );
    for r in &repos {
        let status = match r.status.as_str() {
            "ready" => "ready".green().to_string(),
            "error" => "error".red().to_string(),
            other => other.yellow().to_string(),
        };
        let count = r
            .entity_count
            .map(|n| format!("{n} entities"))
            .unwrap_or_else(|| "—".to_string());
        let marker = if here.as_deref() == Some(&normalize_remote_url(&r.clone_url)) {
            "→ ".cyan().to_string()
        } else {
            "  ".to_string()
        };
        println!(
            "{}{}  {}  {}",
            marker,
            r.name.bold(),
            format!("[{status}]").dimmed(),
            count.dimmed()
        );
    }
    println!(
        "\n{}",
        "→ marks the repo in your current directory. Detail: sem cloud status".dimmed()
    );
}

// ─── sem cloud status ───────────────────────────────────────────────────────

pub fn status(cwd: &str) {
    match cloud::load_credentials() {
        Some(c) => {
            println!("{} {}", "Account:  ".bold(), "logged in".green());
            println!("{} {}", "Endpoint: ".bold(), c.endpoint);
        }
        None => println!(
            "{} {} (run {})",
            "Account:  ".bold(),
            "not logged in".dimmed(),
            "sem login".bold()
        ),
    }

    println!("{} {}", "Telemetry:".bold(), crate::telemetry::mode_label());

    if let Some(remote) = git_remote(cwd) {
        let name = display_name(&remote);
        let state = match state_for(&remote) {
            Some(ConsentState::Enabled) => "enabled (public)".green().to_string(),
            Some(ConsentState::Shared) => "shared (private)".green().to_string(),
            Some(ConsentState::Never) => "never (won't ask)".dimmed().to_string(),
            None => "off — local only".dimmed().to_string(),
        };
        println!("{} {} → {}", "This repo:".bold(), name, state);
    }

    let cfg = load_config();
    let active: Vec<(&String, &str)> = cfg
        .repos
        .iter()
        .filter_map(|(url, r)| {
            r.state
                .as_deref()
                .and_then(ConsentState::parse)
                .filter(|s| s.is_active())
                .map(|s| (url, s.as_str()))
        })
        .collect();
    if !active.is_empty() {
        println!("\n{}", "Cloud-enabled repos:".bold());
        for (url, st) in active {
            println!("  {} {}", url, format!("({st})").dimmed());
        }
    }
    println!(
        "\n{}",
        "Nothing is sent unless a repo above is enabled. `sem cloud preview` shows the exact payload."
            .dimmed()
    );
}

// ─── sem cloud preview ──────────────────────────────────────────────────────

pub fn preview(cwd: &str) {
    let creds = cloud::load_credentials();
    let endpoint = creds
        .as_ref()
        .map(|c| c.endpoint.clone())
        .unwrap_or_else(|| "https://sem-cloud.fly.dev".to_string());
    let acct = creds
        .as_ref()
        .map(|c| mask_key(&c.api_key))
        .unwrap_or_else(|| "<your account>".to_string());

    let Some(remote) = git_remote(cwd) else {
        eprintln!("{} No git remote here.", "error:".red().bold());
        return;
    };
    let name = display_name(&remote);
    let enabled = cloud_enabled_for(&remote);

    if enabled {
        println!("A cloud query for {} sends exactly:", name.bold());
    } else {
        println!(
            "Cloud is {} for {}. If you enabled it, a query would send exactly:",
            "OFF".dimmed(),
            name.bold()
        );
    }
    println!();
    println!("  POST {}/v1/repos/<id>/impact", endpoint);
    println!(
        "  Authorization: Bearer {}   {}",
        acct,
        "(your account)".dimmed()
    );
    println!("  {{");
    println!("    \"targetEntity\": \"<the entity you query>\",");
    println!("    \"targetFile\":   \"<its file path within the repo>\"");
    println!("  }}");
    println!();
    println!(
        "On enable, the repo's clone URL ({}) is sent once to register it.",
        normalize_remote_url(&remote).dimmed()
    );
    println!(
        "{}",
        "No file contents, no environment, no other repos are ever sent.".green()
    );
}

fn mask_key(key: &str) -> String {
    if key.len() > 12 {
        format!("{}…{}", &key[..8], &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}

// ─── sem cloud log ──────────────────────────────────────────────────────────

pub fn log() {
    let Some(path) = ledger_path() else {
        return;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        println!(
            "{} No cloud requests recorded yet — nothing has been sent.",
            "ok".green().bold()
        );
        return;
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        println!(
            "{} No cloud requests recorded yet — nothing has been sent.",
            "ok".green().bold()
        );
        return;
    }
    println!("{}", "Outbound cloud requests (most recent last):".bold());
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = v.get("ts").and_then(|x| x.as_str()).unwrap_or("");
        let repo = v.get("repo").and_then(|x| x.as_str()).unwrap_or("");
        let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("");
        let detail = v.get("detail").and_then(|x| x.as_str()).unwrap_or("");
        println!(
            "  {}  {}  {} {}",
            ts.dimmed(),
            kind.cyan(),
            repo,
            format!("· {detail}").dimmed()
        );
    }
}

// ─── sem cloud never ────────────────────────────────────────────────────────

pub fn never(cwd: &str) {
    if let Some(remote) = git_remote(cwd) {
        set_state(&remote, ConsentState::Never);
        println!(
            "{} Won't ask again for {}. Re-enable anytime with {}.",
            "ok".green().bold(),
            display_name(&remote).bold(),
            "sem cloud enable".bold()
        );
    } else {
        let mut cfg = load_config();
        cfg.tip_suppressed = true;
        save_config(&cfg);
        println!(
            "{} The cloud discovery tip is suppressed everywhere.",
            "ok".green().bold()
        );
    }
}

// ─── sem cloud forget ───────────────────────────────────────────────────────

pub fn forget(cwd: &str) {
    let Some(remote) = require_remote(cwd) else {
        return;
    };
    let name = display_name(&remote);

    print!(
        "Delete {}'s cloud index and unregister it? [y/N] ",
        name.bold()
    );
    let _ = io::stdout().flush();
    let answer = read_line().to_lowercase();
    if answer != "y" && answer != "yes" {
        println!("{} Kept. Nothing changed.", "ok".green().bold());
        return;
    }

    if let Some(client) = CloudClient::from_credentials() {
        match client.forget_repo(&remote) {
            Ok(true) => println!(
                "{} Deleted {} from the cloud.",
                "ok".green().bold(),
                name.bold()
            ),
            Ok(false) => println!(
                "{} {} wasn't registered in the cloud.",
                "ok".green().bold(),
                name.bold()
            ),
            Err(e) => eprintln!(
                "{} Cloud deletion failed: {e}\n  Local consent was still cleared.",
                "warning:".yellow().bold()
            ),
        }
    }

    clear_state(&remote);
    cloud::evict_repo_cache_for(&remote);
    record_outbound(&remote, "forget", "deleted");
}

// ─── Passive discovery tip ──────────────────────────────────────────────────

/// After a slow local index, show a one-time, zero-network tip that cloud
/// exists — but only when logged in, only for repos with no consent decision,
/// only once per repo, and never if globally suppressed. Printing this sends
/// nothing.
pub fn maybe_cloud_tip(cwd: &str, elapsed: std::time::Duration) {
    // Only worth suggesting after a genuinely slow local run.
    if elapsed < std::time::Duration::from_millis(1200) {
        return;
    }
    if cloud::load_credentials().is_none() || cloud::network_disabled() {
        return;
    }
    let Some(remote) = git_remote(cwd) else {
        return;
    };
    // Only GitHub-style remotes can be served today.
    if !normalize_remote_url(&remote).contains("github.com/") {
        return;
    }

    let normalized = normalize_remote_url(&remote);
    let mut cfg = load_config();
    if cfg.tip_suppressed {
        return;
    }
    if let Some(entry) = cfg.repos.get(&normalized) {
        // Already decided (enabled/shared/never) or already tipped.
        if entry.state.is_some() || entry.tipped {
            return;
        }
    }

    eprintln!();
    eprintln!(
        "{} sem cloud serves pre-built indexes — impact/context on large repos",
        "tip:".cyan().bold()
    );
    eprintln!("     answer in ~50ms instead of re-indexing locally. It's off; nothing");
    eprintln!("     was sent to print this.");
    eprintln!(
        "     enable: {}   ·   never show this again: {}",
        "sem cloud enable".bold(),
        "sem cloud never".bold()
    );

    cfg.repos.entry(normalized).or_default().tipped = true;
    save_config(&cfg);
}
