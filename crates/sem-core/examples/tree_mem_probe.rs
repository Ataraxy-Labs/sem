//! Isolated tree-sitter memory multiplier probe (semx-4w1).
//!
//! Question: how many bytes of process RSS does one live `tree_sitter::Tree`
//! cost per byte of source it was parsed from? `EntityGraph::build`'s
//! chunked resolution path (`resolve_scopes_in_file_chunks`,
//! `SCOPE_RESOLVE_FILE_CHUNK_SIZE = 5,000`) holds up to 5,000 such trees
//! *simultaneously* per chunk for any language beyond `PARSED_FILE_REUSE_
//! LIMIT` without a `PrecomputedFileFacts` fast path (every language except
//! JS/TS) — see `resolve_with_scopes_full_inner`'s `owned_parsed_files`.
//! `SEM_PROFILE_MEM=1`'s "post-pass-1"/"peak-resolve"/"post-build"
//! checkpoints only attribute ~20-30% of measured peak RSS on dotnet-runtime;
//! this probe tests whether the unattributed majority is exactly this —
//! `tree_sitter::Tree`'s own C-library memory, which the Rust bindings don't
//! expose a size API for, so it cannot be attributed via `.capacity()`
//! walking the way every other structure in this bead's instrumentation is.
//!
//! Reads every `.cs` file under the given root, parses each with the same
//! grammar/config `sem-core` uses, and holds *all* the resulting trees +
//! their source strings live in one `Vec` — the worst case the chunked path
//! bounds to 5,000 files at a time, done here at full-corpus scale to make
//! the multiplier easy to see against `/usr/bin/time -l`.
//!
//! Usage:
//!   /usr/bin/time -l cargo run --release --example tree_mem_probe -- <repo_root> cs

use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: tree_mem_probe <repo_root> <ext (no dot)>");
    let ext = std::env::args().nth(2).unwrap_or_else(|| "cs".to_string());
    let dotext = format!(".{ext}");

    let mut paths = Vec::new();
    let mut builder = ignore::WalkBuilder::new(&root);
    builder.hidden(true).git_ignore(true);
    for entry in builder.build().flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().map(|e| e == ext.as_str()).unwrap_or(false) {
            paths.push(path.to_path_buf());
        }
    }
    println!("found {} .{ext} files", paths.len());

    let language = sem_core::parser::plugins::code::languages::get_language_config(&dotext)
        .and_then(|c| (c.get_language)())
        .expect("no tree-sitter language registered for this extension");

    let t0 = Instant::now();
    let mut total_bytes: u64 = 0;
    let mut held: Vec<(String, tree_sitter::Tree)> = Vec::with_capacity(paths.len());
    for path in &paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        total_bytes += content.len() as u64;
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&language).is_err() {
            continue;
        }
        if let Some(tree) = parser.parse(&content, None) {
            held.push((content, tree));
        }
    }
    let elapsed = t0.elapsed();
    println!(
        "parsed+held {} trees, {:.1}MB source, in {:.2}s",
        held.len(),
        total_bytes as f64 / (1024.0 * 1024.0),
        elapsed.as_secs_f64()
    );
    // Keep everything alive through process exit so `/usr/bin/time -l`'s
    // peak-RSS sample reflects this held set, not a post-drop process.
    std::hint::black_box(&held);
}
