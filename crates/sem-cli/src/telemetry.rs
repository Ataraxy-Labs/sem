//! Anonymous command-usage telemetry.
//!
//! Records only the command name, CLI version, and OS — never repo names,
//! paths, file contents, or user identity. Events spool to a local file and
//! flush in a detached child process so the command itself never waits on
//! the network. Disable with SEM_NO_TELEMETRY=1 or DO_NOT_TRACK=1.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const DEFAULT_ENDPOINT: &str = "https://sem-cloud.fly.dev";
/// Flush when the spool reaches this many events, or on the first event
/// after this many seconds since the last flush.
const FLUSH_AFTER_EVENTS: usize = 25;
const FLUSH_AFTER_SECS: u64 = 6 * 3600;
const FLUSH_TIMEOUT_SECS: u64 = 5;
/// Stop recording once the spool holds this many unsent events (e.g. a
/// permanently offline machine) — losing telemetry beats growing a file.
const SPOOL_MAX_EVENTS: usize = 500;
/// Minimum seconds between flush attempts, so offline runs don't spawn a
/// doomed child process on every command.
const FLUSH_RETRY_SECS: u64 = 600;

#[derive(Serialize, Deserialize, Default)]
struct TelemetryState {
    #[serde(default)]
    install_id: String,
    #[serde(default)]
    notice_shown: bool,
    #[serde(default)]
    last_flush: u64,
    #[serde(default)]
    last_flush_attempt: u64,
}

fn telemetry_disabled() -> bool {
    let set = |var: &str| std::env::var(var).is_ok_and(|v| !v.is_empty() && v != "0");
    set("SEM_NO_TELEMETRY") || set("DO_NOT_TRACK") || is_development_build()
}

/// True when this binary is a development build rather than a real install,
/// so our own work never pollutes usage data. Catches: debug builds
/// (`cargo run`, `cargo test`), and any binary run straight out of a Cargo
/// `target/` directory (`./crates/target/release/sem` during testing).
/// Installed binaries (cargo-binstall, Homebrew, install.sh, npm) never live
/// under a `target/{debug,release}/` path.
fn is_development_build() -> bool {
    if cfg!(debug_assertions) {
        return true;
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            Some(s.contains("/target/release/") || s.contains("/target/debug/"))
        })
        .unwrap_or(false)
}

fn sem_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem"))
}

fn state_path() -> Option<PathBuf> {
    Some(sem_dir()?.join("telemetry.json"))
}

fn spool_path() -> Option<PathBuf> {
    Some(sem_dir()?.join("telemetry-spool.jsonl"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_state() -> TelemetryState {
    state_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state: &TelemetryState) {
    let Some(path) = state_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, serde_json::to_string(state).unwrap_or_default());
}

/// Random-enough anonymous ID without an extra dependency: hash of
/// timestamp, pid, and home dir.
fn generate_install_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::process::id().hash(&mut h);
    std::env::var("HOME").unwrap_or_default().hash(&mut h);
    let a = h.finish();
    std::process::id().wrapping_mul(31).hash(&mut h);
    let b = h.finish();
    format!("{a:016x}{b:016x}")
}

/// Record one command invocation. Cheap (two small file ops); never blocks
/// on the network. Call once per CLI run before dispatch.
pub fn record(command: &str) {
    if telemetry_disabled() {
        return;
    }

    let mut state = load_state();
    if state.install_id.is_empty() {
        state.install_id = generate_install_id();
    }
    if !state.notice_shown {
        eprintln!(
            "sem collects anonymous usage data (command names only, never code or repo names). Set SEM_NO_TELEMETRY=1 to disable."
        );
        state.notice_shown = true;
        save_state(&state);
    }

    let Some(spool) = spool_path() else { return };

    // Cap the spool so a machine that can never reach the endpoint (air-gapped
    // CI, firewalled) doesn't grow this file forever.
    let event_count = fs::read_to_string(&spool)
        .map(|s| s.lines().count())
        .unwrap_or(0);
    if event_count < SPOOL_MAX_EVENTS {
        let event = serde_json::json!({
            "command": command,
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "ts": now_secs().to_string(),
        });
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spool)
        {
            let _ = writeln!(file, "{event}");
        }
    }

    // Decide whether a flush is due; if so, hand off to a detached child so
    // this process can exit immediately. Throttle attempts so repeated runs
    // without connectivity don't spawn a doomed child each time.
    let now = now_secs();
    let flush_due = (event_count + 1 >= FLUSH_AFTER_EVENTS
        || now.saturating_sub(state.last_flush) >= FLUSH_AFTER_SECS)
        && now.saturating_sub(state.last_flush_attempt) >= FLUSH_RETRY_SECS;

    if flush_due {
        state.last_flush_attempt = now;
        save_state(&state);
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg("__telemetry-flush")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

/// Hidden subcommand body: POST the spool to the telemetry endpoint.
/// Runs in its own process. Claims the spool via atomic rename so two
/// concurrent flushes can't send the same batch twice.
pub fn flush() {
    if telemetry_disabled() {
        return;
    }
    let Some(spool) = spool_path() else { return };
    let claimed = spool.with_extension("sending");
    if fs::rename(&spool, &claimed).is_err() {
        return; // nothing to send, or another flush already claimed it
    }
    let Ok(content) = fs::read_to_string(&claimed) else {
        return;
    };

    let events: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if events.is_empty() {
        let _ = fs::remove_file(&claimed);
        return;
    }

    let mut state = load_state();
    if state.install_id.is_empty() {
        state.install_id = generate_install_id();
    }

    // Report to the endpoint the user is logged into, falling back to the
    // public default.
    let endpoint = crate::commands::cloud::load_credentials()
        .map(|c| c.endpoint)
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(FLUSH_TIMEOUT_SECS))
        .build();
    let body = serde_json::json!({
        "installId": state.install_id,
        "events": events,
    });

    let sent = agent
        .post(&format!("{endpoint}/v1/telemetry"))
        .send_json(body)
        .is_ok();

    if sent {
        let _ = fs::remove_file(&claimed);
        state.last_flush = now_secs();
        save_state(&state);
    } else {
        // Put the events back so they're retried on a later flush. Append
        // (not overwrite) — new events may have spooled meanwhile.
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spool)
        {
            let _ = file.write_all(content.as_bytes());
        }
        let _ = fs::remove_file(&claimed);
    }
}
