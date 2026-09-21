//! Anonymous command-usage telemetry — three modes, local by default.
//!
//! Modes (Go-toolchain model):
//!   • `local` (default) — command names are counted on this machine only and
//!     **never uploaded**. No network, ever.
//!   • `on` — counts are also uploaded to help improve sem.
//!   • `off` — nothing is recorded.
//!
//! Records only the command name, CLI version, and OS — never repo names,
//! paths, or file contents. Uploads carry an install id that is rederived every
//! day (`hash(local seed + day number)`), so a batch can be grouped with the
//! rest of that machine's day and with nothing before or after it. The seed
//! never leaves the machine and the id cannot be linked across days. Switch
//! modes with `sem telemetry on|local|off`. `SEM_NO_TELEMETRY=1`,
//! `DO_NOT_TRACK=1`, or `SEM_NO_NETWORK=1` force the safe behavior regardless.

use std::fs;
use std::io::{Read, Write};
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
    /// Random, machine-local seed for the rotating install id. Never uploaded
    /// itself — only `hash(seed + day)` is, and only in `on` mode.
    #[serde(default)]
    install_seed: Option<String>,
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

/// Seconds in a day — the rotation period for the install id.
const DAY_SECS: u64 = 86_400;

/// A random, machine-local seed. Prefers the OS entropy source; falls back to
/// process-specific values that differ between machines and runs.
fn generate_seed() -> String {
    // Exactly 16 bytes — `/dev/urandom` never reaches EOF, so reading the whole
    // "file" would never return.
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 16];
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&std::process::id(), &mut h);
    std::hash::Hash::hash(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        &mut h,
    );
    std::hash::Hash::hash(&std::env::current_exe().ok(), &mut h);
    let a = std::hash::Hasher::finish(&h);
    std::hash::Hash::hash(&a, &mut h);
    format!("{a:016x}{:016x}", std::hash::Hasher::finish(&h))
}

/// 128 bits of `hash(seed + day)`, as 32 hex chars. `DefaultHasher::new()` is
/// keyed with zeros (unlike `RandomState`), so the same seed and day give the
/// same id in every process — and a different one tomorrow.
fn install_id_for_day(seed: &str, day: u64) -> String {
    let half = |salt: u8| -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&salt, &mut h);
        std::hash::Hash::hash(seed, &mut h);
        std::hash::Hash::hash(&day, &mut h);
        std::hash::Hasher::finish(&h)
    };
    format!("{:016x}{:016x}", half(0), half(1))
}

/// Today's install id, creating and persisting the seed on first use.
fn current_install_id(state: &mut TelemetryState) -> String {
    let seed = match state.install_seed.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            let s = generate_seed();
            state.install_seed = Some(s.clone());
            save_state(state);
            s
        }
    };
    install_id_for_day(&seed, now_secs() / DAY_SECS)
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
    // Rotating daily install id: lets the server count active machines per day
    // without being able to follow one across days.
    let mut id_state = load_state();
    let install_id = current_install_id(&mut id_state);
    let body = serde_json::json!({ "installId": install_id, "events": events });

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
    if mode == Mode::On {
        let mut s = state;
        println!(
            "  batch install id for today: {}",
            current_install_id(&mut s)
        );
    }
    println!(
        "{}",
        "No repo names, paths, or file contents are ever included. The install id is \
         rederived daily from a seed that never leaves this machine, so batches cannot \
         be linked across days."
            .dimmed()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_id_is_stable_within_a_day_and_changes_the_next() {
        let seed = "0123456789abcdef0123456789abcdef";
        let day = 20_000_u64;
        assert_eq!(install_id_for_day(seed, day), install_id_for_day(seed, day));
        assert_ne!(
            install_id_for_day(seed, day),
            install_id_for_day(seed, day + 1)
        );
    }

    #[test]
    fn install_id_differs_between_machines() {
        let day = 20_000_u64;
        assert_ne!(
            install_id_for_day("seed-a", day),
            install_id_for_day("seed-b", day)
        );
    }

    #[test]
    fn install_id_does_not_leak_the_seed() {
        let seed = "0123456789abcdef0123456789abcdef";
        let id = install_id_for_day(seed, 20_000);
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!id.contains(seed));
    }

    #[test]
    fn generated_seeds_are_unique() {
        assert_ne!(generate_seed(), generate_seed());
    }
}
