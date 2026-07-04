//! uv-style terminal progress: a spinner while sem works, then a dim
//! one-line summary. Strictly stderr, and only when stderr is a TTY, so it
//! never touches stdout (JSON, `git diff` replacement) and auto-disables for
//! pipes, CI, and MCP/agent sessions. Disable explicitly with SEM_NO_PROGRESS.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

/// Don't print a summary for work that finished faster than this — warm-cache
/// runs should stay silent and instant, like they do today.
const SUMMARY_MIN: Duration = Duration::from_millis(150);

/// Cross-promote other commands while the user waits. Shown dimmed under the
/// spinner, one at a time. Kept short (one line, no wrap) and never suggests the
/// command you're already running.
const TIPS: &[&str] = &[
    "sem impact <entity> shows everything that breaks if you change it",
    "sem context <entity> packs the right code into a token budget for your agent",
    "sem blame <file> shows who last touched each function, not each line",
    "sem log <entity> traces how one function evolved across commits",
    "sem entities <dir> lists functions and classes without opening files",
    "sem graph maps your repo's entity dependency graph",
    "Give your agent sem: claude mcp add sem -- sem mcp",
    "Stop rebuilding every run: sem login serves the graph warm from the cloud",
];

/// Only nudge toward the cloud after a build slow enough that the warm cloud
/// cache would clearly help. Small repos never trigger it.
const CLOUD_NUDGE_MIN: Duration = Duration::from_secs(3);

/// Pick a tip. Not cryptographic; just rotates by wall-clock nanoseconds so a
/// different one tends to show each run.
fn pick_tip() -> &'static str {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    TIPS[n % TIPS.len()]
}

fn enabled() -> bool {
    if std::env::var("SEM_NO_PROGRESS").is_ok_and(|v| !v.is_empty() && v != "0") {
        return false;
    }
    std::io::stderr().is_terminal()
}

/// A live spinner for one operation. No-op (and silent) when not on a TTY.
pub struct Progress {
    bar: Option<ProgressBar>,
    started: Instant,
}

impl Progress {
    /// Start a spinner with an initial message (e.g. "Building entity graph").
    pub fn start(message: &str) -> Self {
        let bar = if enabled() {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg}")
                    .unwrap()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]),
            );
            pb.enable_steady_tick(Duration::from_millis(80));
            // Show the work on the first line and a dim, rotating tip on the
            // second, like the hints under Claude Code's spinner.
            pb.set_message(format!(
                "{message}\n  {}",
                format!("Tip: {}", pick_tip()).dimmed()
            ));
            Some(pb)
        } else {
            None
        };
        Self {
            bar,
            started: Instant::now(),
        }
    }

    /// Update the spinner's message as the work moves through phases.
    /// Reserved for upcoming per-phase messages (Scanning → Parsing → ...).
    #[allow(dead_code)]
    pub fn set(&self, message: &str) {
        if let Some(bar) = &self.bar {
            bar.set_message(message.to_string());
        }
    }

    /// Clear the spinner and print a dim uv-style summary if the work took
    /// long enough to be worth reporting. `summary` should read like
    /// "1,240 entities, 86 files".
    pub fn done(self, summary: &str) {
        let elapsed = self.started.elapsed();
        if let Some(bar) = self.bar {
            bar.finish_and_clear();
            if elapsed >= SUMMARY_MIN {
                eprintln!(
                    "{} {} in {}",
                    "✓".green(),
                    summary,
                    fmt_duration(elapsed).dimmed()
                );
                maybe_cloud_nudge(elapsed);
            }
        }
    }

    /// Clear the spinner with no summary (e.g. an error path took over).
    #[allow(dead_code)]
    pub fn clear(self) {
        if let Some(bar) = self.bar {
            bar.finish_and_clear();
        }
    }
}

/// After a slow local build by a logged-out user, mention the cloud once. Timed
/// at the moment the wait was actually felt, with the real elapsed time as the
/// contrast. Honest (no inflated multiplier), throttled to once per day, and
/// only on a TTY (this only runs from `done()`, which is already TTY-gated).
fn maybe_cloud_nudge(elapsed: Duration) {
    if elapsed < CLOUD_NUDGE_MIN || logged_in() || !nudge_due_today() {
        return;
    }
    eprintln!(
        "  {} {}",
        "⚡".cyan(),
        format!(
            "You spent {} rebuilding this locally. On sem cloud it's served warm in milliseconds. Log in once: sem login",
            fmt_duration(elapsed)
        )
        .dimmed()
    );
}

fn sem_home() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(std::path::PathBuf::from(home).join(".sem"))
}

fn logged_in() -> bool {
    sem_home().is_some_and(|p| p.join("credentials.json").exists())
}

/// True at most once per day. Writes the marker only when returning true, so
/// the nudge shows once and then stays quiet.
fn nudge_due_today() -> bool {
    match sem_home() {
        Some(home) => nudge_due_for(&home),
        None => false,
    }
}

fn nudge_due_for(home: &std::path::Path) -> bool {
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    let marker = home.join(".cloud-nudge");
    let last = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    if last == Some(today) {
        return false;
    }
    let _ = std::fs::create_dir_all(home);
    let _ = std::fs::write(&marker, today.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_nudge_throttles_to_once_per_day() {
        let dir = std::env::temp_dir().join(format!("sem-nudge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // First call today: due. Second call today: throttled.
        assert!(nudge_due_for(&dir), "first call should be due");
        assert!(
            !nudge_due_for(&dir),
            "second call same day should be throttled"
        );
        // Simulate yesterday: nudge becomes due again.
        let yesterday = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 86_400
            - 1;
        std::fs::write(dir.join(".cloud-nudge"), yesterday.to_string()).unwrap();
        assert!(nudge_due_for(&dir), "a day later it should be due again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tips_are_present_and_short() {
        assert!(!TIPS.is_empty());
        // Keep tips to one terminal line so the spinner clears cleanly.
        for t in TIPS {
            assert!(t.len() < 90, "tip too long: {t}");
        }
    }
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

use colored::Colorize;
