//! Behavior-neutral, zero-cost-off phase timing for `compute_semantic_diff`'s
//! internal work (the `sem diff` attribution campaign).
//!
//! `compute_semantic_diff` fans out per-file work across a rayon `par_iter`
//! (see `maybe_par_iter!` in `differ.rs`), so there is no single call stack
//! to wrap in an `Instant`. Instead each phase accumulates its own CPU-time
//! (wall-clock summed *per file*, not per invocation) into a process-global
//! atomic counter. Summed CPU-time across threads can exceed the wall-clock
//! time of the whole `compute_semantic_diff` call when the `parallel`
//! feature is on and multiple cores are busy at once — that's expected and
//! is exactly the signal that tells you how much parallelism is absorbing.
//!
//! Gated by the same `SEM_TIMINGS` convention `sem-cli`'s `timings::Timings`
//! uses, read once into a `OnceLock<bool>` so the steady-state cost when
//! timings are off is a single relaxed atomic-bool load per file per phase —
//! no `Instant::now()` calls happen at all when disabled.
//!
//! `sem-core` has no knowledge of `sem-cli`'s `Timings` type (wrong
//! dependency direction), so this module exposes plain accumulate/read/reset
//! functions; `sem-cli`'s diff command pulls the totals out via
//! [`diff_phase_timings`] after each `compute_semantic_diff` call and feeds
//! them into its own `Timings::record`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

pub(super) struct PhaseAccumulators {
    pub(super) extraction_ns: AtomicU64,
    pub(super) matching_ns: AtomicU64,
    pub(super) orphan_ns: AtomicU64,
}

pub(super) static PHASE_ACC: PhaseAccumulators = PhaseAccumulators {
    extraction_ns: AtomicU64::new(0),
    matching_ns: AtomicU64::new(0),
    orphan_ns: AtomicU64::new(0),
};

static ENABLED: OnceLock<bool> = OnceLock::new();
// Cheap escape hatch for tests/benches that want to force this off/on
// without touching the process env (OnceLock only resolves once).
static FORCE: AtomicBool = AtomicBool::new(false);
static FORCE_SET: AtomicBool = AtomicBool::new(false);

pub(super) fn phase_timings_enabled() -> bool {
    if FORCE_SET.load(Ordering::Relaxed) {
        return FORCE.load(Ordering::Relaxed);
    }
    *ENABLED.get_or_init(|| {
        let value = std::env::var("SEM_TIMINGS").unwrap_or_default();
        !matches!(value.as_str(), "" | "0" | "false" | "off")
    })
}

/// Test-only override, bypassing the `OnceLock`-cached env read.
#[cfg(test)]
pub fn set_forced_enabled_for_test(enabled: bool) {
    FORCE.store(enabled, Ordering::Relaxed);
    FORCE_SET.store(true, Ordering::Relaxed);
}

/// Zero the accumulators before a fresh top-level `compute_semantic_diff`
/// call so repeated invocations (tests, batch loops, `sem diff` calling it
/// once per process) don't sum across calls. `compute_semantic_diff` calls
/// this at its own top, before fanning out.
pub fn reset_diff_phase_timings() {
    PHASE_ACC.extraction_ns.store(0, Ordering::Relaxed);
    PHASE_ACC.matching_ns.store(0, Ordering::Relaxed);
    PHASE_ACC.orphan_ns.store(0, Ordering::Relaxed);
}

/// Per-phase CPU-time (milliseconds) accumulated across every file processed
/// by the most recent `compute_semantic_diff` call. Summed across threads —
/// see the module doc for why this can exceed wall-clock time.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiffPhaseTimings {
    /// `plugin.extract_entities` for both the before- and after-sides of
    /// every file, summed.
    pub extraction_ms: f64,
    /// `match_entities` (identity resolution + rename/move signature
    /// matching + intra-file reorder detection, all fused into one call)
    /// plus `suppress_redundant_parents`, summed across every file.
    pub matching_ms: f64,
    /// `detect_orphan_changes` (lines changed outside any entity span),
    /// summed across every file.
    pub orphan_ms: f64,
}

pub fn diff_phase_timings() -> DiffPhaseTimings {
    DiffPhaseTimings {
        extraction_ms: PHASE_ACC.extraction_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        matching_ms: PHASE_ACC.matching_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        orphan_ms: PHASE_ACC.orphan_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0,
    }
}

/// Run `f`, adding its wall-clock duration to `counter` — only when phase
/// timings are enabled. When disabled this is exactly `f()`: no
/// `Instant::now()` call, one relaxed atomic-bool load.
#[inline]
pub(super) fn timed<T>(counter: &AtomicU64, f: impl FnOnce() -> T) -> T {
    if !phase_timings_enabled() {
        return f();
    }
    let start = std::time::Instant::now();
    let out = f();
    counter.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    out
}
