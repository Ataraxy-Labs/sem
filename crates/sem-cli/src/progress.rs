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
            pb.set_message(message.to_string());
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
            }
        }
    }

    /// Clear the spinner with no summary (e.g. an error path took over).
    pub fn clear(self) {
        if let Some(bar) = self.bar {
            bar.finish_and_clear();
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
