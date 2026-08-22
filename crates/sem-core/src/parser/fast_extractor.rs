//! Pluggable, parser-independent fast extractors.
//!
//! # What this is
//!
//! `sem`'s entity extraction is tree-sitter-shaped end to end: a concrete
//! syntax tree (CST) is parsed, then `entity_extractor.rs` walks it with
//! ~3,700 lines of per-grammar node-kind rules. measured
//! that a typed-AST parser (oxc) is 20-49x faster than that pipeline on real
//! TypeScript, and then declined to ship it, because the deliverable at the
//! time was *field-identical* output — including `structural_hash`, which is
//! defined by a CST walk and therefore unreachable from an AST by
//! construction.
//!
//! This module is the revival of that idea under a different, weaker, and
//! *sufficient* equivalence: two extractors are equivalent iff they produce
//! the same **user-visible review** — the same entity set (by kappa, name,
//! kind and span) and the same rendered `sem diff`. That equivalence is
//! decided empirically, per candidate extractor, by
//! [`crate::parser::diff_oracle`], which is the shipping gate. Nothing in
//! this module claims equivalence; it only provides the seam an extractor
//! plugs into and the identity that keeps two extractor generations from
//! contaminating each other's caches.
//!
//! # The contract
//!
//! A [`FastExtractor`] is asked for one file's entities, given only
//! `(file_path, content)` — the same inputs
//! [`SemanticParserPlugin::extract_entities`](crate::parser::plugin::SemanticParserPlugin::extract_entities)
//! gets, and the same inputs the parse cache content-addresses. It returns
//! `None` to decline, which is a **silent, safe fallback** to the
//! tree-sitter path: an unsupported construct, a parse error, or a syntax
//! dialect the extractor does not model is never an error, it is a decline.
//!
//! Entities it returns must be complete `SemanticEntity` values — the
//! downstream facts layer, the differ and the caches all read every field.
//! In particular `structural_hash` is load-bearing in two places the differ
//! depends on (`model/identity.rs`: the phase-2 fallback match, which
//! requires it to be *rename-insensitive*, and the `structuralChange`
//! cosmetic verdict), so an extractor that cannot produce a rename-insensitive
//! structural signal will diverge — and the oracle will say so.
//!
//! # Where the seam is, and where it deliberately is not
//!
//! The dispatch sits in
//! [`CodeParserPlugin::extract_entities`](crate::parser::plugins::code::CodeParserPlugin),
//! i.e. behind the *entities-only* extraction API. It deliberately does
//! **not** sit behind `extract_entities_with_tree`, whose callers (pass 1 of
//! `EntityGraph::build`, for both the `retain_parsed_files` arm and the JS/TS
//! `precompute_js_ts_file_facts` arm) need the `tree_sitter::Tree` itself,
//! not just the entities. A fast extractor has no tree to give them; routing
//! those callers through it would silently turn a tree-bearing call into a
//! treeless one and cost pass 2 a serial re-parse. Which callers can and
//! cannot be served is an integration question, answered per call site, with
//! the oracle as the arbiter — not a property of this trait.
//!
//! # Identity, and why it is not optional
//!
//! Every extractor carries a stable [`FastExtractor::identity`] string that
//! **must** change whenever its observable output can. That string is folded
//! into:
//!
//! * the in-process/on-disk parse cache key ([`crate::parser::cache`]), so a
//!   process that flips the switch mid-run cannot be served the other path's
//!   entities; and
//! * (at integration time) the facts-layer language salt, so a corpus entry
//!   written by one extractor generation can never satisfy a lookup made by
//!   another.
//!
//! "Cache-key reasoning" section is the argument for
//! this; it is honored here rather than deferred.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::model::entity::SemanticEntity;

/// A parser-independent entity extractor that can stand in for the
/// tree-sitter `code` plugin on the files it claims.
///
/// Implementors must be cheap to consult (`claims` is called per file) and
/// free of interior mutability that would make two runs over identical
/// inputs disagree — extraction is content-addressed by callers, so a
/// non-deterministic extractor produces silently stale reads, not visible
/// failures.
pub trait FastExtractor: Send + Sync {
    /// Stable identity, folded into every cache key this extractor's output
    /// can reach.
    ///
    /// Must change whenever the extractor's observable output can — a new
    /// parser version, a changed rule, a widened claim set. Treat it the way
    /// `facts_store.rs`'s `LANGUAGE_SALTS` table treats a grammar bump: a
    /// hand-maintained obligation, not a derived value.
    fn identity(&self) -> &str;

    /// Whether this extractor claims `file_path`. Called before `extract`;
    /// must not read the file.
    fn claims(&self, file_path: &str) -> bool;

    /// Extract this file's entities, or decline.
    ///
    /// `None` means "fall back to tree-sitter for this file", and is the
    /// required response to anything the extractor does not fully model:
    /// a parse error, an unsupported construct, a size or time budget. A
    /// decline is silent and always safe; a wrong answer is neither.
    fn extract(&self, file_path: &str, content: &str) -> Option<Vec<SemanticEntity>>;
}

/// An installed, immutable set of extractors plus their combined identity.
///
/// Built once and shared behind an `Arc` so the per-file dispatch never
/// allocates or takes a write lock.
pub struct FastExtractorSet {
    extractors: Vec<Box<dyn FastExtractor>>,
    identity: String,
}

impl FastExtractorSet {
    /// Combine `extractors` into an installable set. The combined identity is
    /// the `+`-joined member identities in registration order; an empty set
    /// has an empty identity and is indistinguishable from "no fast path".
    pub fn new(extractors: Vec<Box<dyn FastExtractor>>) -> Self {
        let identity = extractors
            .iter()
            .map(|e| e.identity().to_string())
            .collect::<Vec<_>>()
            .join("+");
        Self {
            extractors,
            identity,
        }
    }

    /// The `+`-joined identity of every member, in registration order.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Whether this set has no members (the default, and the state a build
    /// without the `oxc-fastpath` feature is always in).
    pub fn is_empty(&self) -> bool {
        self.extractors.is_empty()
    }

    fn try_extract(&self, file_path: &str, content: &str) -> Option<Vec<SemanticEntity>> {
        self.extractors
            .iter()
            .find(|e| e.claims(file_path))
            .and_then(|e| e.extract(file_path, content))
    }
}

fn installed() -> &'static RwLock<Option<Arc<FastExtractorSet>>> {
    static INSTALLED: OnceLock<RwLock<Option<Arc<FastExtractorSet>>>> = OnceLock::new();
    INSTALLED.get_or_init(|| RwLock::new(default_set()))
}

/// The compiled-in extractor set.
///
/// This is the *only* place a built-in extractor is registered, so "what runs
/// by default" is answerable by reading one function. It is `None` until an
/// extractor has passed [`crate::parser::diff_oracle`]; anything else is
/// installed explicitly by an embedder via [`install`].
fn default_set() -> Option<Arc<FastExtractorSet>> {
    #[cfg(feature = "oxc-fastpath")]
    {
        Some(Arc::new(FastExtractorSet::new(vec![Box::new(
            crate::parser::plugins::code::oxc_extractor::OxcExtractor::new(),
        )])))
    }
    #[cfg(not(feature = "oxc-fastpath"))]
    {
        None
    }
}

fn enabled_flag() -> &'static AtomicBool {
    static ENABLED: OnceLock<AtomicBool> = OnceLock::new();
    ENABLED.get_or_init(|| {
        // Compiled off ⇒ off, no env override: a build without the feature has
        // nothing to enable. Compiled on ⇒ still off unless `SEM_FASTPATH=1`.
        //
        // Opt-in rather than opt-out, because compiling the feature is a
        // build-system decision and enabling an unproven extractor is a
        // correctness decision, and those must not be the same switch. No
        // extractor has passed `diff_oracle` yet; until one does, turning the
        // feature on must change nothing observable.
        let compiled = cfg!(feature = "oxc-fastpath");
        let opted_in = matches!(
            std::env::var("SEM_FASTPATH").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        );
        AtomicBool::new(compiled && opted_in)
    })
}

/// Whether fast-path dispatch is currently active for this process.
pub fn enabled() -> bool {
    enabled_flag().load(Ordering::Relaxed)
}

/// Turn fast-path dispatch on or off for this process; returns the previous
/// state.
///
/// Process-global on purpose: extraction runs on rayon workers, so a
/// thread-local switch would not reach them. The oracle relies on this to run
/// two legs of the same pipeline in one process — which is only sound because
/// the parse cache keys on [`identity_salt`], so the legs cannot read each
/// other's entries.
pub fn set_enabled(on: bool) -> bool {
    enabled_flag().swap(on, Ordering::Relaxed)
}

/// Replace the installed extractor set; returns the previous one.
///
/// This is the constructor authority for the fast path — the only way an
/// embedder (or the oracle's mutation tests) introduces an extractor.
pub fn install(set: Option<Arc<FastExtractorSet>>) -> Option<Arc<FastExtractorSet>> {
    let mut guard = installed().write().unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *guard, set)
}

/// The identity of the currently installed set, or `None` when the fast path
/// is off or empty.
///
/// Callers fold this into cache keys and facts salts. `None` and
/// `Some("")` are deliberately not distinguishable to a caller: an empty set
/// must key identically to no set at all, so that installing an empty set
/// never invalidates a cache.
pub fn identity_salt() -> Option<String> {
    if !enabled() {
        return None;
    }
    let guard = installed().read().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(|s| s.identity().to_string())
}

/// Dispatch one file to the installed fast path, or `None` to fall back.
///
/// The disabled case costs one relaxed atomic load and no lock.
pub fn try_extract(file_path: &str, content: &str) -> Option<Vec<SemanticEntity>> {
    if !enabled() {
        return None;
    }
    let set = {
        let guard = installed().read().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    }?;
    if !set.extractors.iter().any(|e| e.claims(file_path)) {
        return None;
    }
    CLAIMED.fetch_add(1, Ordering::Relaxed);
    let served = set.try_extract(file_path, content);
    if served.is_some() {
        SERVED.fetch_add(1, Ordering::Relaxed);
    }
    served
}

static CLAIMED: AtomicU64 = AtomicU64::new(0);
static SERVED: AtomicU64 = AtomicU64::new(0);

/// `(claimed, served)` since the last [`reset_stats`]: how many extraction
/// requests an installed extractor claimed, and how many it actually answered
/// rather than declining.
///
/// The oracle reports these because a run in which nothing was served is
/// **vacuous**, not passing — an extractor that declines every file trivially
/// produces an identical diff, and calling that "equivalent" would be the
/// exact self-deception this whole gate exists to prevent.
pub fn stats() -> (u64, u64) {
    (
        CLAIMED.load(Ordering::Relaxed),
        SERVED.load(Ordering::Relaxed),
    )
}

/// Serializes tests that mutate the process-global switch or installed set.
///
/// These are genuinely global — extraction runs on rayon workers, so there is
/// no thread-local alternative — which means two tests touching them
/// concurrently interfere. Every such test in this crate takes this lock.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Zero the [`stats`] counters.
pub fn reset_stats() {
    CLAIMED.store(0, Ordering::Relaxed);
    SERVED.store(0, Ordering::Relaxed);
}

/// The extensions the JS/TS family covers, shared by every JS/TS fast
/// extractor and by the oracle's fixtures so the two cannot drift.
pub const JS_TS_EXTENSIONS: &[&str] =
    &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

/// Whether `file_path` is in the JS/TS family.
pub fn is_js_ts_path(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    JS_TS_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;
    impl FastExtractor for Stub {
        fn identity(&self) -> &str {
            "stub-1"
        }
        fn claims(&self, file_path: &str) -> bool {
            file_path.ends_with(".stub")
        }
        fn extract(&self, _file_path: &str, _content: &str) -> Option<Vec<SemanticEntity>> {
            Some(Vec::new())
        }
    }

    #[test]
    fn identity_of_a_set_joins_its_members() {
        let set = FastExtractorSet::new(vec![Box::new(Stub), Box::new(Stub)]);
        assert_eq!(set.identity(), "stub-1+stub-1");
        assert!(!set.is_empty());
    }

    #[test]
    fn an_empty_set_has_an_empty_identity() {
        let set = FastExtractorSet::new(Vec::new());
        assert!(set.is_empty());
        assert_eq!(set.identity(), "");
    }

    #[test]
    fn js_ts_paths_are_recognized_case_insensitively() {
        assert!(is_js_ts_path("a/b/c.ts"));
        assert!(is_js_ts_path("a/b/C.TSX"));
        assert!(is_js_ts_path("x.mts"));
        assert!(!is_js_ts_path("x.rs"));
        assert!(!is_js_ts_path("x.tsz"));
    }
}
