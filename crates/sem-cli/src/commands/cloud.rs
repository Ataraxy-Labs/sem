use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use colored::Colorize;
use serde::{Deserialize, Serialize};

use sem_core::git::bridge::GitBridge;
use sem_core::git::types::FileChange;
use sem_core::parser::differ::{BinaryFileChange, DiffResult};

use super::context::ContextOptions;
use super::entities::EntitiesOptions;
use super::impact::ImpactOptions;
use super::log::LogOptions;

const DEFAULT_ENDPOINT: &str = "https://sem-cloud.fly.dev";
const GITHUB_CLIENT_ID: &str = "Ov23lioE75FJYz4Mn7ZH";
const API_TIMEOUT_SECS: u64 = 10;
const REPO_CACHE_TTL_SECS: i64 = 86400; // 24 hours

// ─── Credentials ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct CloudCredentials {
    pub api_key: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
}

fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.into()
}

fn credentials_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join("credentials.json"))
}

fn save_credentials(creds: &CloudCredentials) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = credentials_path().ok_or("Could not determine home directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(creds)?;
    fs::write(&path, json)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(path)
}

/// Load stored cloud credentials, if any.
pub fn load_credentials() -> Option<CloudCredentials> {
    let path = credentials_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

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
    println!("  Logging in changes nothing else: no repo registered, no query sent.");
    println!(
        "  Turn cloud on per repo with {}.",
        "sem cloud enable".bold()
    );

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
    println!("  Logging in changes nothing else: no repo registered, no query sent.");
    println!(
        "  Turn cloud on per repo with {}.",
        "sem cloud enable".bold()
    );

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
                    "off — local only".dimmed(),
                    "(turn on with sem cloud enable)".dimmed()
                );
            }
        }
    }

    Ok(())
}

// ─── Cloud API Response Types ────────────────────────────────────────────

// Response types: all fields are deserialized from JSON but not all are
// directly read by Rust code (some are passed through to output).
#[allow(dead_code)]
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudEntityBrief {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudImpactResponse {
    pub dependencies: Vec<CloudEntityBrief>,
    pub dependents: Vec<CloudEntityBrief>,
    #[serde(default)]
    pub transitive_impact: Vec<CloudEntityBrief>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudContextEntry {
    pub entity_id: String,
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub estimated_tokens: usize,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudContextResponse {
    pub tokens_used: usize,
    #[serde(default)]
    pub truncated: bool,
    pub entries: Vec<CloudContextEntry>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudRepoResponse {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub clone_url: String,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub entity_count: Option<usize>,
    #[serde(default)]
    pub file_count: Option<usize>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudEntitiesResponse {
    pub entities: Vec<CloudEntityBrief>,
    #[serde(default)]
    pub total: usize,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudHistoryEntry {
    #[serde(default)]
    pub entity_id: String,
    pub entity_name: String,
    #[serde(default)]
    pub entity_type: String,
    #[serde(default)]
    pub file_path: String,
    pub change_type: String,
    pub commit_sha: String,
    #[serde(default)]
    pub commit_author: Option<String>,
    #[serde(default)]
    pub commit_message: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudHistoryResponse {
    pub changes: Vec<CloudHistoryEntry>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudDiffSnapshotResponse {
    pub id: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudCrossRepoEdge {
    pub from_repo_id: String,
    pub from_entity_id: String,
    pub to_repo_id: String,
    pub to_entity_id: String,
    #[serde(default)]
    pub ref_type: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CloudCrossDepsResponse {
    pub edges: Vec<CloudCrossRepoEdge>,
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub query_ms: u64,
}

// ─── Repo Cache ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RepoCacheEntry {
    pub repo_id: String,
    pub status: String,
    pub checked_at: String,
    #[serde(default)]
    pub entity_count: Option<usize>,
}

// Cloud-only routing: once a repo is enabled, EVERY graph command uses the
// cloud regardless of repo size. There is no size-based "small repo → local"
// heuristic. Local computation is reached only by an explicit escape hatch
// (`SEM_LOCAL=1` / `SEM_NO_NETWORK=1`) or as a fallback when the cloud is
// unreachable — never because a repo happens to be small.

fn repo_cache_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join("repos.json"))
}

fn load_repo_cache() -> Option<HashMap<String, RepoCacheEntry>> {
    let path = repo_cache_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_repo_cache(cache: &HashMap<String, RepoCacheEntry>) {
    let Some(path) = repo_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        path,
        serde_json::to_string_pretty(cache).unwrap_or_default(),
    );
}

fn current_timestamp() -> String {
    // Simple ISO 8601 timestamp without external deps
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

fn cache_entry_expired(entry: &RepoCacheEntry) -> bool {
    let checked: i64 = entry.checked_at.parse().unwrap_or(0);
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    now - checked > REPO_CACHE_TTL_SECS
}

// ─── URL Normalization ───────────────────────────────────────────────────

/// Normalize a git remote URL for matching against cloud repos.
/// Strips trailing `.git`, converts SSH `git@host:user/repo` to `https://host/user/repo`.
pub fn normalize_remote_url(url: &str) -> String {
    let mut normalized = url.to_string();

    // SSH → HTTPS: git@github.com:user/repo → https://github.com/user/repo
    if normalized.starts_with("git@") {
        if let Some(rest) = normalized.strip_prefix("git@") {
            normalized = rest.replacen(':', "/", 1);
            normalized = format!("https://{normalized}");
        }
    }

    // ssh://git@github.com/user/repo → https://github.com/user/repo
    if normalized.starts_with("ssh://git@") {
        normalized = normalized.replacen("ssh://git@", "https://", 1);
    }

    // Strip trailing .git
    if normalized.ends_with(".git") {
        normalized.truncate(normalized.len() - 4);
    }

    // Strip trailing slash
    if normalized.ends_with('/') {
        normalized.truncate(normalized.len() - 1);
    }

    normalized
}

/// Insert/update the local repo cache from a registration or resolution.
fn cache_repo(remote_url: &str, repo: &CloudRepoResponse) {
    let normalized = normalize_remote_url(remote_url);
    let mut cache = load_repo_cache().unwrap_or_default();
    cache.insert(
        normalized,
        RepoCacheEntry {
            repo_id: repo.id.clone(),
            status: repo.status.clone(),
            checked_at: current_timestamp(),
            entity_count: repo.entity_count,
        },
    );
    save_repo_cache(&cache);
}

/// Drop a repo's cache entry (e.g. after `sem cloud forget`).
pub fn evict_repo_cache_for(remote_url: &str) {
    let normalized = normalize_remote_url(remote_url);
    if let Some(mut cache) = load_repo_cache() {
        cache.remove(&normalized);
        save_repo_cache(&cache);
    }
}

/// Parse the GitHub `owner`/`repo` from a remote URL, if it is a github.com
/// remote. Returns `None` for non-GitHub or unparseable remotes.
fn github_owner_repo(remote_url: &str) -> Option<(String, String)> {
    let normalized = normalize_remote_url(remote_url);
    let rest = normalized.split("github.com/").nth(1)?;
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some((owner.to_string(), repo.to_string()))
}

// ─── CloudClient ─────────────────────────────────────────────────────────

pub struct CloudClient {
    creds: CloudCredentials,
    agent: ureq::Agent,
}

/// Print `(using sem cloud)` to stderr on the first cloud call per session.
static CLOUD_BANNER_SHOWN: AtomicBool = AtomicBool::new(false);

fn show_cloud_banner() {
    if !CLOUD_BANNER_SHOWN.swap(true, Ordering::Relaxed) {
        eprintln!("{}", "(using sem cloud)".dimmed());
    }
}

pub fn network_disabled() -> bool {
    std::env::var("SEM_NO_NETWORK").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Returns true if cloud routing is forced off: `SEM_LOCAL=1` or the
/// `SEM_NO_NETWORK` master kill switch.
pub fn is_local_forced() -> bool {
    std::env::var("SEM_LOCAL").ok().is_some_and(|v| v == "1") || network_disabled()
}

/// Credentials from `~/.sem/credentials.json`, or from `SEM_TOKEN` (+ optional
/// `SEM_CLOUD_ENDPOINT`) for CI where there's no interactive login.
fn credentials_or_env() -> Option<CloudCredentials> {
    if let Ok(token) = std::env::var("SEM_TOKEN") {
        if !token.is_empty() {
            let endpoint =
                std::env::var("SEM_CLOUD_ENDPOINT").unwrap_or_else(|_| default_endpoint());
            return Some(CloudCredentials {
                api_key: token,
                endpoint,
            });
        }
    }
    load_credentials()
}

impl CloudClient {
    /// Create a CloudClient from stored credentials or `SEM_TOKEN`.
    /// Returns None if not logged in, or if cloud is forced off.
    pub fn from_credentials() -> Option<Self> {
        if is_local_forced() {
            return None;
        }
        let creds = credentials_or_env()?;
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(API_TIMEOUT_SECS))
            .build();
        Some(Self { creds, agent })
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.creds.endpoint, path)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.creds.api_key)
    }

    // ── Repo resolution ──

    /// Look up a repo by its remote URL. Returns the repo_id.
    fn resolve_repo(&self, remote_url: &str) -> Result<String, Box<dyn std::error::Error>> {
        let normalized = normalize_remote_url(remote_url);

        // Check cache first. Entries that aren't "ready" yet (still cloning
        // or indexing) are re-fetched every time so status and entity count
        // converge once the server finishes.
        if let Some(cache) = load_repo_cache() {
            if let Some(entry) = cache.get(&normalized) {
                if entry.status == "ready" && !cache_entry_expired(entry) {
                    return Ok(entry.repo_id.clone());
                }
            }
        }

        // Call API: GET /v1/repos
        let resp: Vec<CloudRepoResponse> = self
            .agent
            .get(&self.api_url("/v1/repos"))
            .set("Authorization", &self.auth_header())
            .call()?
            .into_json()?;

        for repo in &resp {
            let repo_normalized = normalize_remote_url(&repo.clone_url);
            if repo_normalized == normalized {
                // Cache it
                let mut cache = load_repo_cache().unwrap_or_default();
                cache.insert(
                    normalized,
                    RepoCacheEntry {
                        repo_id: repo.id.clone(),
                        status: repo.status.clone(),
                        checked_at: current_timestamp(),
                        entity_count: repo.entity_count,
                    },
                );
                save_repo_cache(&cache);
                return Ok(repo.id.clone());
            }
        }

        Err("repo not found".into())
    }

    /// Register a new repo with the cloud, tagging its visibility so the server
    /// enforces the public-auto-sync / private-opt-in policy. Returns the response.
    fn register_repo(
        &self,
        remote_url: &str,
        visibility: &str,
    ) -> Result<CloudRepoResponse, Box<dyn std::error::Error>> {
        let resp: CloudRepoResponse = self
            .agent
            .post(&self.api_url("/v1/repos"))
            .set("Authorization", &self.auth_header())
            .send_json(serde_json::json!({ "cloneUrl": remote_url, "visibility": visibility }))?
            .into_json()?;
        Ok(resp)
    }

    /// Register a repo whose consent was just granted via `sem cloud enable`
    /// or `sem cloud share`, caching the result. Public entry for the consent
    /// commands.
    pub fn register(
        &self,
        remote_url: &str,
        visibility: &str,
    ) -> Result<CloudRepoResponse, Box<dyn std::error::Error>> {
        let repo = self.register_repo(remote_url, visibility)?;
        cache_repo(remote_url, &repo);
        Ok(repo)
    }

    /// Every repo indexed under this account (`sem cloud list`).
    pub fn list_repos(&self) -> Result<Vec<CloudRepoResponse>, Box<dyn std::error::Error>> {
        let resp = self
            .agent
            .get(&self.api_url("/v1/repos"))
            .set("Authorization", &self.auth_header())
            .call()?
            .into_json()?;
        Ok(resp)
    }

    /// Resolve and delete a repo from the cloud (`sem cloud forget`). Returns
    /// Ok(false) when the repo was never registered.
    pub fn forget_repo(&self, remote_url: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let repo_id = match self.resolve_repo(remote_url) {
            Ok(id) => id,
            Err(_) => return Ok(false),
        };
        self.agent
            .delete(&self.api_url(&format!("/v1/repos/{repo_id}")))
            .set("Authorization", &self.auth_header())
            .call()?;
        Ok(true)
    }

    /// Best-effort check that a repo is public, via the unauthenticated GitHub
    /// API. Returns true only when GitHub confirms `private: false`; private,
    /// missing, non-GitHub, or unreachable repos all return false. Used by
    /// `sem cloud enable` to steer private repos to the louder `share` flow.
    pub fn repo_is_public(&self, remote_url: &str) -> bool {
        let Some((owner, repo)) = github_owner_repo(remote_url) else {
            return false;
        };
        match self
            .agent
            .get(&format!("https://api.github.com/repos/{owner}/{repo}"))
            .set("User-Agent", "sem-cli")
            .set("Accept", "application/vnd.github+json")
            .call()
        {
            Ok(resp) => resp
                .into_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("private").and_then(|p| p.as_bool()))
                .map(|private| !private)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Resolve repo, or register if not found. Returns repo_id only if status
    /// is "ready". Callers gate on explicit consent (`consent::cloud_enabled_for`)
    /// before reaching here, so registration here is never silent.
    pub fn ensure_repo(&self, remote_url: &str) -> Result<String, Box<dyn std::error::Error>> {
        match self.resolve_repo(remote_url) {
            Ok(id) => Ok(id),
            Err(_) => {
                // Visibility follows the recorded consent: `sem cloud share`
                // marks a repo private; everything else registers as public.
                let visibility = match super::consent::state_for(remote_url) {
                    Some(super::consent::ConsentState::Shared) => "private",
                    _ => "public",
                };
                let repo = self.register_repo(remote_url, visibility)?;
                cache_repo(remote_url, &repo);

                if repo.status == "ready" {
                    Ok(repo.id)
                } else {
                    Err(format!("repo status is '{}', not ready yet", repo.status).into())
                }
            }
        }
    }

    // ── Per-command API methods ──

    pub fn impact(
        &self,
        repo_id: &str,
        entity: &str,
        file: &str,
    ) -> Result<CloudImpactResponse, Box<dyn std::error::Error>> {
        let resp = self
            .agent
            .post(&self.api_url(&format!("/v1/repos/{repo_id}/impact")))
            .set("Authorization", &self.auth_header())
            .send_json(serde_json::json!({
                "targetEntity": entity,
                "targetFile": file,
            }))?
            .into_json()?;
        Ok(resp)
    }

    pub fn context(
        &self,
        repo_id: &str,
        entity: &str,
        file: &str,
        budget: usize,
    ) -> Result<CloudContextResponse, Box<dyn std::error::Error>> {
        let resp = self
            .agent
            .post(&self.api_url(&format!("/v1/repos/{repo_id}/context")))
            .set("Authorization", &self.auth_header())
            .send_json(serde_json::json!({
                "targetEntity": entity,
                "targetFile": file,
                "tokenBudget": budget,
            }))?
            .into_json()?;
        Ok(resp)
    }

    pub fn entities(
        &self,
        repo_id: &str,
        file_path_filter: Option<&str>,
    ) -> Result<CloudEntitiesResponse, Box<dyn std::error::Error>> {
        // Server default limit is 100; request everything for whole-repo
        // listings, brief (no content) to keep the payload small.
        let mut url = format!("/v1/repos/{repo_id}/entities?limit=1000000&brief=true");
        if let Some(fp) = file_path_filter {
            url.push_str(&format!("&filePath={}", urlencoding_encode(fp)));
        }
        let resp: CloudEntitiesResponse = self
            .agent
            .get(&self.api_url(&url))
            .set("Authorization", &self.auth_header())
            .call()?
            .into_json()?;
        Ok(resp)
    }

    pub fn history(
        &self,
        repo_id: &str,
        file: Option<&str>,
        limit: usize,
    ) -> Result<CloudHistoryResponse, Box<dyn std::error::Error>> {
        // Server filters by entityId/filePath only; entity-name filtering
        // happens client-side. days=3650 ≈ full history.
        let mut url = format!("/v1/repos/{repo_id}/analytics/history?days=3650");
        if let Some(f) = file {
            url.push_str(&format!("&filePath={}", urlencoding_encode(f)));
        }
        if limit > 0 {
            url.push_str(&format!("&limit={limit}"));
        }
        let resp = self
            .agent
            .get(&self.api_url(&url))
            .set("Authorization", &self.auth_header())
            .call()?
            .into_json()?;
        Ok(resp)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upload_diff_snapshot(
        &self,
        remote_url: &str,
        head_sha: Option<&str>,
        label: Option<&str>,
        file_changes: &[FileChange],
        diff_result: &DiffResult,
        binary_changes: &[BinaryFileChange],
        relations: &serde_json::Value,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let resp: CloudDiffSnapshotResponse = self
            .agent
            .post(&self.api_url("/v1/diffs"))
            .set("Authorization", &self.auth_header())
            .send_json(serde_json::json!({
                "cloneUrl": remote_url,
                "headSha": head_sha,
                "label": label,
                "fileChanges": file_changes,
                "diffResult": diff_result,
                "binaryChanges": binary_changes,
                "relations": relations,
            }))?
            .into_json()?;
        Ok(resp.id)
    }

    pub fn diff_snapshot_url(&self, id: &str) -> String {
        if let Ok(template) = std::env::var("SEM_DIFF_VIEWER_URL") {
            if template.contains("{id}") {
                return template.replace("{id}", id);
            }
            return format!("{}/diffs/{id}", template.trim_end_matches('/'));
        }

        if self.creds.endpoint.contains("127.0.0.1") || self.creds.endpoint.contains("localhost") {
            format!("http://127.0.0.1:3000/diffs/{id}")
        } else {
            format!("https://ataraxy-labs.com/diffs/{id}")
        }
    }

    /// Cross-repo dependency edges across all of your indexed repos. This is
    /// cloud-only: it answers "what in my other repos depends on this," which a
    /// single-repo local graph can't see.
    pub fn cross_deps(&self) -> Result<CloudCrossDepsResponse, Box<dyn std::error::Error>> {
        let resp: CloudCrossDepsResponse = self
            .agent
            .get(&self.api_url("/v1/cross-deps"))
            .set("Authorization", &self.auth_header())
            .call()?
            .into_json()?;
        Ok(resp)
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

fn urlencoding_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

// ─── try_cloud_* helpers ─────────────────────────────────────────────────

/// Attempt to open GitBridge and get remote URL for cloud resolution.
fn cloud_git_context(cwd: &str) -> Option<(GitBridge, String)> {
    let git = GitBridge::open(Path::new(cwd)).ok()?;
    let remote = git.get_remote_url()?;
    Some((git, remote))
}

/// Cloud answers reflect the last *indexed commit*. When the working tree has
/// uncommitted changes those answers would be stale, so state-reflecting
/// commands (impact/context/entities) fall back to a full local computation:
/// clean tree → cloud, dirty tree → local. `git status --porcelain` empty
/// means clean; if we can't tell, assume dirty (the safe choice).
fn working_tree_clean(repo_root: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && o.stdout.is_empty())
        .unwrap_or(false)
}

/// Try to run `sem impact` via cloud. Returns Some(()) on success.
pub fn try_cloud_impact(opts: &ImpactOptions) -> Option<()> {
    // --tests needs test classification data the cloud API doesn't expose.
    if matches!(opts.mode, super::impact::ImpactMode::Tests) {
        return None;
    }
    // --no-cache means "compute fresh": serving a remote snapshot would violate it.
    if opts.no_cache {
        return None;
    }
    // A --file hint exists to disambiguate same-named entities, but the cloud
    // resolves by name with a silent name-only fallback when the file doesn't
    // match its index — which returns the WRONG entity's graph (repro: weave's
    // ten `fn run` command handlers; `--file .../apply.rs` came back with
    // bench.rs's dependencies). Until the server resolves name+file strictly,
    // file-hinted queries stay local, where disambiguation is exact.
    if opts.file_hint.is_some() {
        return None;
    }
    let client = CloudClient::from_credentials()?;
    let (git, remote) = cloud_git_context(&opts.cwd)?;
    // Cloud is off until the user runs `sem cloud enable`/`share` (or SEM_CLOUD=1).
    // Once enabled, every repo uses the cloud regardless of size.
    if !super::consent::cloud_enabled_for(&remote) {
        return None;
    }
    // Dirty working tree → the cloud's indexed-commit answer would be stale; go local.
    if !working_tree_clean(git.repo_root()) {
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

    super::consent::record_outbound(&remote, "impact", entity_name);
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
    // Same gates as try_cloud_impact: --no-cache means fresh local compute, and
    // a --file hint needs exact name+file resolution the cloud doesn't do yet.
    if opts.no_cache || opts.file_path.is_some() {
        return None;
    }
    let client = CloudClient::from_credentials()?;
    let (git, remote) = cloud_git_context(&opts.cwd)?;
    if !super::consent::cloud_enabled_for(&remote) {
        return None;
    }
    if !working_tree_clean(git.repo_root()) {
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

    super::consent::record_outbound(&remote, "context", entity_name);
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
    if !super::consent::cloud_enabled_for(&remote) {
        return None;
    }
    if !working_tree_clean(git.repo_root()) {
        return None;
    }
    // Whole-repo listings go to the cloud (any size). Subdirectory listings
    // stay local only because the cloud entities endpoint lists the whole repo
    // — that's a scope limit, not a size one.
    let normalized =
        super::normalize_repo_relative_path(Path::new(&opts.cwd), git.repo_root(), path_arg);
    if normalized != "." {
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

    super::consent::record_outbound(&remote, "entities", ".");
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
    if !super::consent::cloud_enabled_for(&remote) {
        return None;
    }
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

    super::consent::record_outbound(&remote, "log", &opts.entity_name);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_owner_repo_parses_remote_forms() {
        for url in [
            "https://github.com/Ataraxy-Labs/sem",
            "https://github.com/Ataraxy-Labs/sem.git",
            "git@github.com:Ataraxy-Labs/sem.git",
            "ssh://git@github.com/Ataraxy-Labs/sem",
        ] {
            assert_eq!(
                github_owner_repo(url),
                Some(("Ataraxy-Labs".to_string(), "sem".to_string())),
                "failed for {url}"
            );
        }
        // Non-GitHub or unparseable remotes are not auto-synced.
        assert_eq!(github_owner_repo("https://gitlab.com/a/b"), None);
        assert_eq!(github_owner_repo("not a url"), None);
    }
}
