//! `sem update` — self-update to the latest GitHub release, plus a
//! non-blocking background check that nudges the user when behind.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use colored::Colorize;
use serde::{Deserialize, Serialize};

const REPO: &str = "Ataraxy-Labs/sem";
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;
/// Release binaries are ~15MB; refuse anything wildly larger.
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;
/// How often the background version check runs, and how often the
/// "new version available" hint may print.
const CHECK_INTERVAL_SECS: u64 = 24 * 3600;
const NOTIFY_INTERVAL_SECS: u64 = 24 * 3600;
const CHECK_TIMEOUT_SECS: u64 = 5;

// ─── Update notification ─────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct UpdateCheckState {
    #[serde(default)]
    latest_version: String,
    #[serde(default)]
    last_check: u64,
    #[serde(default)]
    last_notified: u64,
}

fn check_disabled() -> bool {
    let set = |var: &str| std::env::var(var).is_ok_and(|v| !v.is_empty() && v != "0");
    set("SEM_NO_UPDATE_CHECK") || set("DO_NOT_TRACK")
}

fn check_state_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join("update-check.json"))
}

fn load_check_state() -> UpdateCheckState {
    check_state_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_check_state(state: &UpdateCheckState) {
    let Some(path) = check_state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, serde_json::to_string(state).unwrap_or_default());
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Print a once-a-day hint when a newer release is known, and kick off a
/// detached background check when the cached answer is stale. Costs one
/// small file read; never touches the network in this process.
pub fn maybe_notify(command: &str) {
    if check_disabled() {
        return;
    }
    // Commands where an extra stderr line is unwanted noise.
    if matches!(command, "update" | "mcp" | "completions") {
        return;
    }

    let mut state = load_check_state();
    let now = now_secs();
    let mut dirty = false;

    if !state.latest_version.is_empty()
        && is_newer(&state.latest_version, env!("CARGO_PKG_VERSION"))
        && now.saturating_sub(state.last_notified) >= NOTIFY_INTERVAL_SECS
    {
        eprintln!(
            "{}",
            format!(
                "A new version of sem is available: v{} → v{}. Run `sem update` to upgrade.",
                env!("CARGO_PKG_VERSION"),
                state.latest_version
            )
            .dimmed()
        );
        state.last_notified = now;
        dirty = true;
    }

    if now.saturating_sub(state.last_check) >= CHECK_INTERVAL_SECS {
        // Stamp before spawning so concurrent runs don't spawn a checker each.
        state.last_check = now;
        dirty = true;
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg("__update-check")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    if dirty {
        save_check_state(&state);
    }
}

/// Hidden subcommand body: fetch the latest release tag and cache it.
/// Runs in its own process.
pub fn background_check() {
    if check_disabled() {
        return;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(CHECK_TIMEOUT_SECS))
        .build();
    let Ok(resp) = agent
        .get(&format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .set("User-Agent", "sem-cli")
        .set("Accept", "application/vnd.github+json")
        .call()
    else {
        return;
    };
    let Ok(release) = resp.into_json::<serde_json::Value>() else {
        return;
    };
    let Some(tag) = release["tag_name"].as_str() else {
        return;
    };

    let mut state = load_check_state();
    state.latest_version = tag.trim_start_matches('v').to_string();
    state.last_check = now_secs();
    save_check_state(&state);
}

// ─── sem update ──────────────────────────────────────────────────────────

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");

    // Homebrew owns its files; replacing them under brew's feet breaks
    // `brew upgrade` later. Defer to it.
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy();
    if exe_str.contains("/Cellar/") || exe_str.contains("/linuxbrew/") {
        println!(
            "sem was installed with Homebrew. Update it with:\n  {}",
            "brew upgrade sem".bold()
        );
        return Ok(());
    }

    eprint!("{}", "Checking for updates...".dimmed());
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build();

    let release: serde_json::Value = agent
        .get(&format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .set("User-Agent", "sem-cli")
        .set("Accept", "application/vnd.github+json")
        .call()?
        .into_json()?;

    let tag = release["tag_name"]
        .as_str()
        .ok_or("No tag_name in latest release")?;
    let latest = tag.trim_start_matches('v');
    eprintln!(" latest is v{latest}");

    if !is_newer(latest, current) {
        println!(
            "{} sem v{current} is already the latest version",
            "ok".green().bold()
        );
        return Ok(());
    }

    let artifact = artifact_name()?;
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{artifact}");

    println!(
        "Updating sem {} → {}",
        format!("v{current}").dimmed(),
        format!("v{latest}").bold()
    );

    // Download to a temp dir.
    let tmp = std::env::temp_dir().join(format!("sem-update-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let archive = tmp.join(&artifact);
    download(&agent, &url, &archive)?;

    // Best-effort checksum verification when a system sha tool exists.
    verify_checksum(&agent, tag, &artifact, &archive);

    // Extract. tar handles .tar.gz on macOS, Linux, and Windows 10+.
    let status = std::process::Command::new("tar")
        .arg("xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&tmp)
        .status()?;
    if !status.success() {
        cleanup(&tmp);
        return Err("Failed to extract release archive".into());
    }

    let new_binary = find_binary(&tmp).ok_or("No sem binary found in release archive")?;

    // Swap in place: move the running binary aside (allowed on Unix and
    // Windows), then move the new one into its path.
    let old = exe.with_extension("old");
    let _ = fs::remove_file(&old);
    if let Err(e) = fs::rename(&exe, &old) {
        cleanup(&tmp);
        return Err(format!(
            "Cannot replace {} ({e}). Try with elevated permissions, or reinstall:\n  curl -fsSL https://raw.githubusercontent.com/{REPO}/main/install.sh | sh",
            exe.display()
        )
        .into());
    }
    if let Err(e) = fs::rename(&new_binary, &exe) {
        // Roll back so the user still has a working sem.
        let _ = fs::rename(&old, &exe);
        cleanup(&tmp);
        return Err(format!("Failed to install new binary: {e}").into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&exe, fs::Permissions::from_mode(0o755));
    }

    let _ = fs::remove_file(&old); // fails harmlessly on Windows while running
    cleanup(&tmp);

    println!(
        "{} Updated to v{latest} ({})",
        "ok".green().bold(),
        exe.display()
    );
    Ok(())
}

/// Strict numeric semver comparison; non-numeric parts compare as 0.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> (u64, u64, u64) {
        let mut parts = v.split('.').map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(0)
        });
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    };
    parse(candidate) > parse(current)
}

fn artifact_name() -> Result<String, Box<dyn std::error::Error>> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => return Err(format!("No prebuilt binaries for OS '{other}'").into()),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => return Err(format!("No prebuilt binaries for arch '{other}'").into()),
    };
    Ok(format!("sem-{os}-{arch}.tar.gz"))
}

fn download(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let resp = agent.get(url).set("User-Agent", "sem-cli").call()?;
    let mut reader = resp.into_reader().take(MAX_DOWNLOAD_BYTES);
    let mut file = fs::File::create(dest)?;
    std::io::copy(&mut reader, &mut file)?;
    Ok(())
}

/// Verify the archive against the release's checksums.txt when shasum or
/// sha256sum is available. Hard-fails on mismatch; skips silently when no
/// tool or no checksum entry exists.
fn verify_checksum(agent: &ureq::Agent, tag: &str, artifact: &str, archive: &Path) {
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/checksums.txt");
    let Ok(resp) = agent.get(&url).set("User-Agent", "sem-cli").call() else {
        return;
    };
    let Ok(listing) = resp.into_string() else {
        return;
    };
    let Some(expected) = listing
        .lines()
        .find(|l| l.contains(artifact))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_lowercase)
    else {
        return;
    };

    let actual = ["sha256sum", "shasum"].iter().find_map(|tool| {
        let mut cmd = std::process::Command::new(tool);
        if *tool == "shasum" {
            cmd.args(["-a", "256"]);
        }
        cmd.arg(archive);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .map(str::to_lowercase)
    });

    if let Some(actual) = actual {
        if actual != expected {
            eprintln!(
                "{} checksum mismatch for {artifact} — aborting",
                "error:".red().bold()
            );
            std::process::exit(1);
        }
    }
}

fn find_binary(dir: &Path) -> Option<PathBuf> {
    let names = ["sem", "sem.exe"];
    // Top level first, then one level deep (archives may nest a directory).
    for name in names {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
    }
    for entry in fs::read_dir(dir).ok()?.flatten() {
        if entry.path().is_dir() {
            for name in names {
                let nested = entry.path().join(name);
                if nested.is_file() {
                    return Some(nested);
                }
            }
        }
    }
    None
}

fn cleanup(tmp: &Path) {
    let _ = fs::remove_dir_all(tmp);
}
