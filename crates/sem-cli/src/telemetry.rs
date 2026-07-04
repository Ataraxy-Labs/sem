//! Anonymous command-usage telemetry — three modes, local by default.
//!
//! Modes (Go-toolchain model):
//!   • `local` (default) — command names are counted on this machine only and
//!     **never uploaded**. No network, ever.
//!   • `on` — counts are also uploaded to help improve sem.
//!   • `off` — nothing is recorded.
//!
//! Records only the command name, CLI version, and OS — never repo names,
//! paths, file contents, or any identifier (there is no install ID). Switch
//! modes with `sem telemetry on|local|off`. `SEM_NO_TELEMETRY=1`,
//! `DO_NOT_TRACK=1`, or `SEM_NO_NETWORK=1` force the safe behavior regardless.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const DEFAULT_ENDPOINT: &str = "https://sem-cloud.fly.dev";
/// In `on` mode, flush when the spool reaches this many events, or on the
/// first event after this many seconds since the last flush.
const FLUSH_AFTER_EVENTS: usize = 25;
const FLUSH_AFTER_SECS: u64 = 6 * 3600;
const FLUSH_TIMEOUT_SECS: u64 = 5;
/// Stop recording once the spool holds this many events — bounds the local
/// file on a machine that never uploads.
const SPOOL_MAX_EVENTS: usize = 500;
/// Minimum seconds between flush attempts so offline runs don't spawn a doomed
/// child on every command.
const FLUSH_RETRY_SECS: u64 = 600;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Local,
    On,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "off" | "0" | "false" => Some(Mode::Off),
            "local" => Some(Mode::Local),
            "on" | "1" | "true" => Some(Mode::On),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Local => "local",
            Mode::On => "on",
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct TelemetryState {
    /// "off" | "local" | "on"; None = undecided (treated as the default).
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    notice_shown: bool,
    #[serde(default)]
    last_flush: u64,
    #[serde(default)]
    last_flush_attempt: u64,
}

/// `SEM_NO_TELEMETRY` / `DO_NOT_TRACK` hard-disable recording. Dev builds never
/// record so our own work doesn't pollute usage data.
fn force_off() -> bool {
    let set = |var: &str| std::env::var(var).is_ok_and(|v| !v.is_empty() && v != "0");
    set("SEM_NO_TELEMETRY") || set("DO_NOT_TRACK") || is_development_build()
}

/// The effective mode: env override > stored mode > default (`local`).
fn effective_mode(state: &TelemetryState) -> Mode {
    if force_off() {
        return Mode::Off;
    }
    if let Ok(v) = std::env::var("SEM_TELEMETRY") {
        if let Some(m) = Mode::parse(&v) {
            return m;
        }
    }
    state
        .mode
        .as_deref()
        .and_then(Mode::parse)
        .unwrap_or(Mode::Local)
}

/// True when this binary is a development build rather than a real install, so
/// our own work never pollutes usage data. Catches debug builds and any binary
/// run straight out of a Cargo `target/` directory.
fn is_development_build() -> bool {
    if cfg!(debug_assertions) {
        return true;
    }
    std::env::current_exe()
        .ok()
        .map(|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            s.contains("/target/release/") || s.contains("/target/debug/")
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

fn spool_event_count() -> usize {
    spool_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Record one command invocation. Cheap (small file ops); never blocks on the
/// network. In `local` mode (the default) nothing is ever uploaded. Call once
/// per CLI run before dispatch.
pub fn record(command: &str) {
    let mut state = load_state();
    let mode = effective_mode(&state);
    if mode == Mode::Off {
        return;
    }

    // First-run notice comes BEFORE any datum is recorded — the first event is
    // only ever written on a later run, after the user has seen the notice and
    // had the chance to change modes. Nothing has been recorded or sent yet.
    if !state.notice_shown {
        match mode {
            Mode::Local => eprintln!(
                "sem keeps anonymous usage stats (command names only — never code or repo names) \
                 on this machine. Nothing is uploaded. Run `sem telemetry on` to share them, or \
                 `sem telemetry off` to disable."
            ),
            Mode::On => eprintln!(
                "sem collects anonymous usage data (command names only — never code or repo names). \
                 Run `sem telemetry off` to disable."
            ),
            Mode::Off => unreachable!(),
        }
        state.notice_shown = true;
        save_state(&state);
        return;
    }

    let Some(spool) = spool_path() else { return };

    // Bound the spool so a machine that never uploads (local mode, or air-gapped
    // CI) doesn't grow the file forever.
    let event_count = spool_event_count();
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

    // Only `on` mode (and only when the network isn't disabled) ever uploads.
    if mode != Mode::On || crate::commands::cloud::network_disabled() {
        return;
    }

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

/// Hidden subcommand body: POST the spool to the telemetry endpoint. Runs in
/// its own process. Only uploads in `on` mode. Claims the spool via atomic
/// rename so two concurrent flushes can't send the same batch twice.
pub fn flush() {
    let state = load_state();
    if effective_mode(&state) != Mode::On || crate::commands::cloud::network_disabled() {
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

    let endpoint = crate::commands::cloud::load_credentials()
        .map(|c| c.endpoint)
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(FLUSH_TIMEOUT_SECS))
        .build();
    // No install ID — just the anonymous event batch.
    let body = serde_json::json!({ "events": events });

    let sent = agent
        .post(&format!("{endpoint}/v1/telemetry"))
        .send_json(body)
        .is_ok();

    if sent {
        let _ = fs::remove_file(&claimed);
        let mut state = load_state();
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

/// `sem telemetry on|local|off`: persist the mode and confirm.
pub fn set_mode(mode: &str) {
    use colored::Colorize;
    let Some(m) = Mode::parse(mode) else {
        eprintln!(
            "{} unknown mode '{mode}' (use on, local, or off)",
            "error:".red().bold()
        );
        return;
    };
    let mut state = load_state();
    state.mode = Some(m.as_str().to_string());
    // Choosing a mode is itself acknowledgement; don't re-show the notice.
    state.notice_shown = true;
    save_state(&state);

    match m {
        Mode::On => {
            println!(
                "{} Telemetry is {} — command names are uploaded to help improve sem.",
                "ok".green().bold(),
                "on".green()
            );
            // Send whatever has accumulated locally now.
            if !crate::commands::cloud::network_disabled() {
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
        Mode::Local => println!(
            "{} Telemetry is {} — stats stay on this machine; nothing is uploaded.",
            "ok".green().bold(),
            "local".cyan()
        ),
        Mode::Off => {
            println!(
                "{} Telemetry is {} — nothing is recorded.",
                "ok".green().bold(),
                "off".dimmed()
            );
            // Drop anything already spooled.
            if let Some(spool) = spool_path() {
                let _ = fs::remove_file(spool);
            }
        }
    }
}

/// Short label for `sem cloud status`.
pub fn mode_label() -> String {
    let state = load_state();
    match effective_mode(&state) {
        Mode::On => "on (uploading anonymous command names)".to_string(),
        Mode::Local => "local (on this machine only)".to_string(),
        Mode::Off => "off".to_string(),
    }
}

/// `sem telemetry preview`: show the mode and exactly what is recorded.
pub fn preview() {
    use colored::Colorize;
    let state = load_state();
    let mode = effective_mode(&state);
    println!("{} {}", "Telemetry mode:".bold(), mode_label());
    let count = spool_event_count();
    println!("{} {count} event(s) recorded locally", "Spooled:".bold());
    if mode == Mode::On {
        println!("These are uploaded as anonymous batches like:");
    } else {
        println!("If enabled (`sem telemetry on`), these would upload as anonymous batches like:");
    }
    println!(
        "  {{ \"command\": \"impact\", \"version\": \"{}\", \"os\": \"{}\", \"ts\": \"…\" }}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    println!(
        "{}",
        "No repo names, paths, file contents, or identifiers are ever included.".dimmed()
    );
}
