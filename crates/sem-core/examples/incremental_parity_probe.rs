//! reproduce (or rule out) a lost edge between a cold build and a
//! warm incremental rebuild of the same tree, on a real corpus.
//!
//! Usage:
//!   cargo run --release --example incremental_parity_probe -- <repo_root> <mode> <out_dir>
//!
//! modes:
//!   noop       -- cold build, then rebuild with the same file list and an
//!                 empty changed_paths (nothing touched at all)
//!   touch:<F>  -- cold build, then rebuild with changed_paths=[F] but F's
//!                 on-disk content is untouched (a "touched, no content
//!                 change" seed-RED case)
//!   editrevert:<F> -- cold build, append then remove a trailing newline in
//!                 F between builds (content changes then reverts to the
//!                 original bytes), changed_paths=[F]
//!   addremove:<F> -- cold build, create a new file F with trivial content,
//!                 rebuild (F added), then delete F and rebuild again
//!                 (F removed) -- exercises the add+delete chunked-corpus path
//!   multiround:<N> -- cold build, then N rounds of: pick an evenly-spaced
//!                 file from the sorted corpus, pad it with ~2MB of a
//!                 trailing block comment (shifts byte-budget chunk
//!                 boundaries for every file after it without touching the
//!                 file list), rebuild with only that file dirty, and diff
//!                 the resulting warm graph against a fresh cold build of
//!                 the corpus *as it now stands on disk*. Edits accumulate
//!                 across rounds (not reverted) so chunk drift compounds.
//!                 Stops at the first round that diverges; writes that
//!                 round's cold/warm dumps and does not revert the edits (so
//!                 the corpus is left in the diverging state for follow-up).
//!
//! Writes <out_dir>/cold.edges and <out_dir>/warm.edges (sorted, same format
//! as edge_dump_probe) plus <out_dir>/summary.txt.

use std::io::Write;
use std::path::{Path, PathBuf};

use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::parser::session::GraphSession;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

fn make_registry(root: &Path) -> ParserRegistry {
    let mut registry = create_default_registry();
    registry.load_semrc(root);
    registry.load_gitattributes(root);
    registry
}

fn walk_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_default_excluded(&rel_str) {
            continue;
        }
        if is_probably_binary_path(&rel_str) {
            continue;
        }
        files.push(rel_str);
    }
    files.sort();
    files
}

fn dump_edges(graph: &EntityGraph, out_path: &Path) -> usize {
    let mut lines: Vec<String> = graph
        .edges
        .iter()
        .map(|e| format!("{}\t{:?}\t{}", e.from_entity, e.ref_type, e.to_entity))
        .collect();
    lines.sort();
    let mut f = std::fs::File::create(out_path).expect("create out file");
    for line in &lines {
        writeln!(f, "{line}").expect("write line");
    }
    lines.len()
}

/// A ~2MB block comment appended to a TS/JS-ish file. Large enough to shift
/// byte-budget chunk boundaries (budget is 20MB in release builds) without
/// being so large it dominates a single file's own chunk.
fn padding_comment(round: usize) -> String {
    let mut s = String::with_capacity(2 * 1024 * 1024 + 64);
    s.push_str(&format!("\n/* multiround pad round {round}\n"));
    let line = "x".repeat(78);
    while s.len() < 2 * 1024 * 1024 {
        s.push_str(&line);
        s.push('\n');
    }
    s.push_str("*/\n");
    s
}

fn run_multiround(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    rounds: usize,
    out_dir: &Path,
) {
    let t0 = std::time::Instant::now();
    let mut session = GraphSession::build(root, file_paths, registry);
    eprintln!("cold build: {:.1}s", t0.elapsed().as_secs_f64());
    dump_edges(session.graph(), &out_dir.join("round0_cold.edges"));

    // Evenly-spaced .ts/.tsx/.js targets so edits land in different regions
    // of the sorted file list (and therefore different chunks).
    let candidates: Vec<&String> = file_paths
        .iter()
        .filter(|p| p.ends_with(".ts") || p.ends_with(".tsx") || p.ends_with(".js"))
        .collect();
    assert!(candidates.len() >= rounds, "not enough candidate files");
    let stride = candidates.len() / rounds;

    for round in 0..rounds {
        let target = candidates[round * stride].clone();
        let full = root.join(&target);
        let mut body = std::fs::read_to_string(&full).unwrap_or_default();
        body.push_str(&padding_comment(round));
        std::fs::write(&full, &body).expect("write padded file");

        let tr = std::time::Instant::now();
        let stats = session.rebuild(file_paths, std::slice::from_ref(&target), registry);
        eprintln!(
            "round {round}: touched {target} ({} bytes appended), rebuild {:.2}s stats={:?}",
            body.len(),
            tr.elapsed().as_secs_f64(),
            stats
        );

        let tc = std::time::Instant::now();
        let (cold_graph, _) = EntityGraph::build(root, file_paths, registry);
        eprintln!("round {round}: fresh cold build {:.1}s", tc.elapsed().as_secs_f64());

        let warm_path = out_dir.join(format!("round{round}_warm.edges"));
        let cold_path = out_dir.join(format!("round{round}_cold.edges"));
        let warm_count = dump_edges(session.graph(), &warm_path);
        let cold_count = dump_edges(&cold_graph, &cold_path);

        let warm_text = std::fs::read_to_string(&warm_path).unwrap();
        let cold_text = std::fs::read_to_string(&cold_path).unwrap();
        if warm_text == cold_text {
            eprintln!("round {round}: MATCH ({cold_count} edges)");
            let _ = std::fs::remove_file(&warm_path);
            let _ = std::fs::remove_file(&cold_path);
        } else {
            eprintln!(
                "round {round}: DIVERGED! warm={warm_count} cold={cold_count} -- see {} vs {}",
                warm_path.display(),
                cold_path.display()
            );
            eprintln!("stopping at first divergence, edits left in place");
            return;
        }
    }
    eprintln!("all {rounds} rounds matched -- no divergence found");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root: PathBuf = args.next().expect("usage: <repo_root> <mode> <out_dir>").into();
    let mode = args.next().expect("usage: <repo_root> <mode> <out_dir>");
    let out_dir: PathBuf = args.next().expect("usage: <repo_root> <mode> <out_dir>").into();
    std::fs::create_dir_all(&out_dir).expect("mkdir out_dir");

    let registry = make_registry(&root);
    let file_paths = walk_files(&root);
    eprintln!("corpus: {} files at {}", file_paths.len(), root.display());

    if let Some(rest) = mode.strip_prefix("multiround:") {
        let rounds: usize = rest.parse().expect("multiround:<N>");
        run_multiround(&root, &file_paths, &registry, rounds, &out_dir);
        return;
    }

    let t0 = std::time::Instant::now();
    let mut session = GraphSession::build(&root, &file_paths, &registry);
    eprintln!("cold build: {:.1}s", t0.elapsed().as_secs_f64());
    let cold_count = dump_edges(session.graph(), &out_dir.join("cold.edges"));
    eprintln!("cold edges: {cold_count}");

    let (rebuild_files, changed): (Vec<String>, Vec<String>) = if let Some(rest) = mode.strip_prefix("touch:") {
        (file_paths.clone(), vec![rest.to_string()])
    } else if let Some(rest) = mode.strip_prefix("editrevert:") {
        let target = root.join(rest);
        let original = std::fs::read(&target).expect("read target");
        let mut edited = original.clone();
        edited.push(b'\n');
        std::fs::write(&target, &edited).expect("write edited");
        std::fs::write(&target, &original).expect("revert");
        (file_paths.clone(), vec![rest.to_string()])
    } else if let Some(rest) = mode.strip_prefix("addremove:") {
        let target = root.join(rest);
        std::fs::write(&target, "export function __semx_hpe_probe() { return 1; }\n")
            .expect("write new file");
        let mut added_files = file_paths.clone();
        added_files.push(rest.to_string());
        added_files.sort();
        let t1 = std::time::Instant::now();
        session.rebuild(&added_files, &[rest.to_string()], &registry);
        eprintln!("add rebuild: {:.1}s", t1.elapsed().as_secs_f64());
        std::fs::remove_file(&target).expect("remove new file");
        (file_paths.clone(), vec![rest.to_string()])
    } else if mode == "noop" {
        (file_paths.clone(), Vec::new())
    } else {
        panic!("unknown mode {mode}");
    };

    let t2 = std::time::Instant::now();
    let stats = session.rebuild(&rebuild_files, &changed, &registry);
    eprintln!("warm rebuild: {:.1}s stats={:?}", t2.elapsed().as_secs_f64(), stats);
    let warm_count = dump_edges(session.graph(), &out_dir.join("warm.edges"));
    eprintln!("warm edges: {warm_count}");

    let mut summary = std::fs::File::create(out_dir.join("summary.txt")).expect("summary");
    writeln!(summary, "mode={mode}").unwrap();
    writeln!(summary, "cold_edges={cold_count}").unwrap();
    writeln!(summary, "warm_edges={warm_count}").unwrap();
    writeln!(summary, "stats={stats:?}").unwrap();
}
