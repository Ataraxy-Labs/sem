//! Ad hoc performance probe for cold full-repo graph builds.
//!
//! Not part of the public API and not wired into any product code path.
//! Answers: on a cold `EntityGraph::build`, where does wall time go —
//! file walk/IO, tree-sitter parse, entity extraction, or pass-2
//! resolution (symbol table + import resolution + reference resolution)?
//!
//! Usage:
//!   cargo run --release --example perf_probe -- <repo_root>
//!
//! Prints tagged, greppable lines to stdout:
//!   WALK       — file discovery wall time + file count
//!   IO         — pure file-read wall time (parallel), no parsing
//!   PARSE_EXTRACT — tree-sitter parse + entity extraction wall time (parallel), pre-read content
//!   PASS1_ONLY — EntityGraph::build_direct_dependencies with a never-true
//!                predicate: read + parse + extract + symbol-table build,
//!                stops before import-table / resolution work even starts.
//!   PHASE_HOOK — BuildPhase::Parsing -> BuildPhase::Resolving -> done,
//!                timestamped against a full EntityGraph::build call.
//!   BUILD_TOTAL — total EntityGraph::build wall time, plus entity/edge counts.
//!   LANG_RATE  — per-extension raw tree-sitter parse rate and combined
//!                parse+extract rate (lines/sec, MB/sec), each measured as
//!                one parallel pass restricted to that extension's files.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rayon::prelude::*;
use sem_core::parser::graph::{
    clear_build_phase_hook, set_build_phase_hook, BuildPhase, EntityGraph,
};
use sem_core::parser::plugins::code::languages::get_language_config;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

fn make_registry(root: &Path) -> ParserRegistry {
    let mut registry = create_default_registry();
    registry.load_semrc(root);
    registry.load_gitattributes(root);
    registry
}

fn walk_files(root: &Path, registry: &ParserRegistry) -> Vec<String> {
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
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if is_default_excluded(&rel) {
            continue;
        }
        if is_probably_binary_path(&rel) {
            continue;
        }
        if registry.get_explicit_plugin(&rel).is_none() {
            continue;
        }
        files.push(rel);
    }
    files.sort();
    files
}

fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: perf_probe <repo_root> [label]");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[1]);
    let root = root.canonicalize().expect("bad root path");
    let label = args.get(2).cloned().unwrap_or_else(|| {
        root.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let registry = make_registry(&root);

    // ---- WALK ----
    let t0 = Instant::now();
    let file_paths = walk_files(&root, &registry);
    let walk_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "WALK label={label} files={} walk_ms={walk_ms:.2}",
        file_paths.len()
    );

    // ---- IO only (parallel read, no parsing) ----
    let t0 = Instant::now();
    let contents: Vec<(String, String)> = file_paths
        .par_iter()
        .filter_map(|p| {
            let full = root.join(p);
            std::fs::read_to_string(&full).ok().map(|c| (p.clone(), c))
        })
        .collect();
    let io_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let total_bytes: u64 = contents.iter().map(|(_, c)| c.len() as u64).sum();
    println!(
        "IO label={label} files={} bytes={total_bytes} io_ms={io_ms:.2}",
        contents.len()
    );

    // ---- PARSE_EXTRACT only (pure CPU: tree-sitter parse + AST-walk extraction on pre-read content) ----
    let t0 = Instant::now();
    let entity_count_pe: usize = contents
        .par_iter()
        .map(|(p, c)| {
            registry
                .extract_entities_with_tree(p, c)
                .map(|(ents, _tree)| ents.len())
                .unwrap_or(0)
        })
        .sum();
    let parse_extract_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "PARSE_EXTRACT label={label} files={} entities={entity_count_pe} parse_extract_ms={parse_extract_ms:.2}",
        contents.len()
    );

    // ---- PASS1_ONLY: build_direct_dependencies with a never-true predicate.
    // Reads (again, since build_direct_dependencies does its own IO), parses,
    // extracts, and builds the symbol table, then returns *before* import-table
    // building or any reference/scope resolution starts (see EntityGraph::
    // build_direct_dependencies early-return when `needs_resolution` is empty).
    let t0 = Instant::now();
    let (pass1_graph, pass1_entities) =
        EntityGraph::build_direct_dependencies(&root, &file_paths, &registry, |_| false);
    let pass1_only_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "PASS1_ONLY label={label} files={} entities={} edges={} pass1_only_ms={pass1_only_ms:.2}",
        file_paths.len(),
        pass1_entities.len(),
        pass1_graph.edges.len()
    );

    // ---- Full build with phase-hook timestamps ----
    let parse_ts: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let resolve_ts: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let hook_files: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    {
        let parse_ts = parse_ts.clone();
        let resolve_ts = resolve_ts.clone();
        let hook_files = hook_files.clone();
        set_build_phase_hook(Box::new(move |phase| match phase {
            BuildPhase::Parsing { files } => {
                *parse_ts.lock().unwrap() = Some(Instant::now());
                hook_files.store(files as u64, Ordering::Relaxed);
            }
            BuildPhase::Resolving => {
                *resolve_ts.lock().unwrap() = Some(Instant::now());
            }
        }));
    }
    let t_build0 = Instant::now();
    let (graph, entities) = EntityGraph::build(&root, &file_paths, &registry);
    let build_end = Instant::now();
    let build_total_ms = build_end.duration_since(t_build0).as_secs_f64() * 1000.0;
    clear_build_phase_hook();

    let parse_hook_ms = parse_ts
        .lock()
        .unwrap()
        .map(|t| t.duration_since(t_build0).as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let resolve_hook_ms = resolve_ts
        .lock()
        .unwrap()
        .map(|t| t.duration_since(t_build0).as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    // "pre-resolve" bucket = pass1 (parse+extract+io) + symbol-table build +
    // import-table build, because BuildPhase::Resolving fires only after
    // import-table construction in EntityGraph::build (verified by reading
    // graph.rs). "resolve" bucket = scope resolution + bag-of-words reference
    // resolution + edge assembly/dedupe/sort.
    let pre_resolve_ms = resolve_hook_ms - parse_hook_ms;
    let resolve_phase_ms = build_total_ms - resolve_hook_ms;
    // Derived: import-table build ~= pre_resolve_ms - pass1_only_ms (pass1_only
    // already covers io+parse+extract+symtable; pre_resolve additionally covers
    // import-table build). Clamped at 0 to absorb run-to-run noise.
    let import_table_ms = (pre_resolve_ms - pass1_only_ms).max(0.0);

    println!(
        "BUILD_TOTAL label={label} files={} entities={} edges={} build_total_ms={build_total_ms:.2} parse_hook_ms={parse_hook_ms:.2} resolve_hook_ms={resolve_hook_ms:.2} pre_resolve_ms={pre_resolve_ms:.2} resolve_phase_ms={resolve_phase_ms:.2} import_table_derived_ms={import_table_ms:.2}",
        file_paths.len(),
        entities.len(),
        graph.edges.len(),
    );

    // ---- LANG_RATE: per-extension raw tree-sitter parse rate vs combined
    // parse+extract rate, each as one parallel pass over just that ext's files. ----
    // Cap outlier files: a handful of pathological multi-MB generated/fixture
    // files (e.g. TypeScript's tests/baselines snapshot dumps) can dominate a
    // parallel wall-clock sum and mask the typical per-language rate. Report
    // both: the uncapped set (realistic "as the repo actually is") and, when
    // SEM_PERF_MAX_BYTES is set, a capped set for a fairer typical-file rate.
    let max_bytes: Option<usize> = std::env::var("SEM_PERF_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut by_ext: std::collections::BTreeMap<String, Vec<&(String, String)>> =
        std::collections::BTreeMap::new();
    for entry in &contents {
        if max_bytes.is_some_and(|cap| entry.1.len() > cap) {
            continue;
        }
        by_ext.entry(ext_of(&entry.0)).or_default().push(entry);
    }
    for (ext, files) in &by_ext {
        let n_files = files.len();
        if n_files < 3 {
            continue; // not enough files in this repo for a meaningful rate
        }
        let cfg = get_language_config(ext);
        let total_bytes: u64 = files.iter().map(|(_, c)| c.len() as u64).sum();
        let total_lines: u64 = files.iter().map(|(_, c)| c.lines().count() as u64).sum();

        let raw_parse_ms = if let Some(cfg) = cfg {
            if let Some(lang) = (cfg.get_language)() {
                let t0 = Instant::now();
                files.par_iter().for_each(|(_, c)| {
                    let mut parser = tree_sitter::Parser::new();
                    if parser.set_language(&lang).is_ok() {
                        let _ = parser.parse(c.as_bytes(), None);
                    }
                });
                Some(t0.elapsed().as_secs_f64() * 1000.0)
            } else {
                None
            }
        } else {
            None
        };

        let t0 = Instant::now();
        files.par_iter().for_each(|(p, c)| {
            let _ = registry.extract_entities_with_tree(p, c);
        });
        let combined_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let raw_str = raw_parse_ms
            .map(|ms| format!("{ms:.2}"))
            .unwrap_or_else(|| "NA".to_string());
        let raw_lines_per_sec = raw_parse_ms
            .filter(|ms| *ms > 0.0)
            .map(|ms| (total_lines as f64) / (ms / 1000.0))
            .unwrap_or(0.0);
        let combined_lines_per_sec = if combined_ms > 0.0 {
            (total_lines as f64) / (combined_ms / 1000.0)
        } else {
            0.0
        };
        let raw_mb_per_sec = raw_parse_ms
            .filter(|ms| *ms > 0.0)
            .map(|ms| (total_bytes as f64 / 1_000_000.0) / (ms / 1000.0))
            .unwrap_or(0.0);
        let combined_mb_per_sec = if combined_ms > 0.0 {
            (total_bytes as f64 / 1_000_000.0) / (combined_ms / 1000.0)
        } else {
            0.0
        };

        println!(
            "LANG_RATE label={label} ext={ext} files={n_files} bytes={total_bytes} lines={total_lines} raw_parse_ms={raw_str} combined_parse_extract_ms={combined_ms:.2} raw_lines_per_sec={raw_lines_per_sec:.0} combined_lines_per_sec={combined_lines_per_sec:.0} raw_mb_per_sec={raw_mb_per_sec:.3} combined_mb_per_sec={combined_mb_per_sec:.3}"
        );
    }

    println!("DONE label={label}");
}
