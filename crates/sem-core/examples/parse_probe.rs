//! parse-ceiling probe: where does parse wall-clock actually go,
//! and is it work-bound or span-bound?
//!
//! Not part of the public API and not wired into any product code path. It
//! answers four questions `perf_probe` cannot, because `perf_probe` only times
//! whole phases:
//!
//!   1. **Brent's theorem.** `T_P >= max(T_1/P, T_inf)`. Times every file
//!      individually during one parallel pass, so `T_1` (sum of per-file cost),
//!      `T_inf` (the single most expensive file) and the achieved wall are all
//!      measured on the same run. Utilization = `T_1 / (P * wall)`.
//!   2. **Scheduling.** Simulates greedy list scheduling over `P` machines at
//!      the measured per-file costs, in corpus order versus longest-first
//!      (Graham's LPT), to price a reorder before writing one.
//!   3. **The chunk barrier.** `resolve_scopes_in_file_chunks` re-parses inside
//!      a 20 MiB byte-budget partition with a join between chunks; simulates
//!      that makespan against the flat one at the same costs.
//!   4. **The reparse's used/unused split.** The chunked pass-2 re-parse reads
//!      and parses every file of every chunk that holds at least one
//!      scope-resolvable file, but the per-file scope loop declines any file
//!      whose language has no `scope_resolve` config. Reports both sides in
//!      files, bytes and measured parse cost.
//!
//! Usage: parse_probe <repo_root> [label]

/// Production binaries (`sem`, `sem-mcp`) install mimalloc as their global
/// allocator; a plain example gets macOS's system allocator instead. Since the
/// whole question here is whether parse is work-bound or contention-bound, the
/// probe has to run on the allocator the product runs on, or it measures the
/// wrong ceiling.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use sem_core::parser::plugins::code::languages::get_language_config;
use sem_core::parser::plugins::code::{is_pathological_large_file, parse_tree};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

/// Same partition `graph.rs`'s `chunk_files_by_byte_budget` computes, at the
/// same 20 MiB budget, over the same (sorted) file list.
const SCOPE_RESOLVE_BYTE_BUDGET: u64 = 20 * 1024 * 1024;

fn walk_files(root: &Path, registry: &ParserRegistry) -> Vec<String> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    for entry in builder.build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if is_default_excluded(&rel) || is_probably_binary_path(&rel) {
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

fn ext_of(path: &str) -> &str {
    path.rfind('.').map(|i| &path[i..]).unwrap_or("")
}

fn scope_resolvable(path: &str) -> bool {
    get_language_config(ext_of(path))
        .and_then(|c| c.scope_resolve)
        .is_some()
}

/// Greedy list scheduling over `p` machines: makespan of `costs` in the given
/// order. This is exactly what a work-stealing pool achieves when every task is
/// indivisible, so it is the right model for one-file-per-task parsing.
fn makespan(costs: &[u64], p: usize) -> u64 {
    let mut machines = vec![0u64; p];
    for &c in costs {
        let m = machines
            .iter()
            .enumerate()
            .min_by_key(|(_, load)| **load)
            .map(|(i, _)| i)
            .unwrap();
        machines[m] += c;
    }
    machines.into_iter().max().unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: parse_probe <repo_root> [label]");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[1]).canonicalize().expect("bad root");
    let label = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| root.file_name().unwrap().to_string_lossy().to_string());
    let p = std::thread::available_parallelism().map_or(1, |n| n.get());

    let mut registry = create_default_registry();
    registry.load_semrc(&root);
    registry.load_gitattributes(&root);
    let files = walk_files(&root, &registry);

    // ---- the chunk partition, byte budget, metadata sizes (as the chunker) ----
    let sizes: Vec<u64> = files
        .par_iter()
        .map(|f| {
            std::fs::metadata(root.join(f))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .collect();
    let mut chunks: Vec<std::ops::Range<usize>> = Vec::new();
    let (mut start, mut acc) = (0usize, 0u64);
    for (i, size) in sizes.iter().enumerate() {
        if i > start && acc + size > SCOPE_RESOLVE_BYTE_BUDGET {
            chunks.push(start..i);
            start = i;
            acc = 0;
        }
        acc += size;
    }
    if start < files.len() {
        chunks.push(start..files.len());
    }
    // A chunk is visited only if it holds at least one scope-resolvable file.
    let visited: Vec<std::ops::Range<usize>> = chunks
        .iter()
        .filter(|r| files[(*r).clone()].iter().any(|f| scope_resolvable(f)))
        .cloned()
        .collect();

    let mut n_sr = 0usize;
    let mut b_sr = 0u64;
    let mut n_nsr = 0usize;
    let mut b_nsr = 0u64;
    for r in &visited {
        for i in r.clone() {
            if scope_resolvable(&files[i]) {
                n_sr += 1;
                b_sr += sizes[i];
            } else {
                n_nsr += 1;
                b_nsr += sizes[i];
            }
        }
    }
    println!(
        "PP_CORPUS label={label} p={p} files={} bytes={} chunks={} visited_chunks={} \
reparse_used_files={n_sr} reparse_used_bytes={b_sr} reparse_unused_files={n_nsr} reparse_unused_bytes={b_nsr}",
        files.len(),
        sizes.iter().sum::<u64>(),
        chunks.len(),
        visited.len(),
    );

    // ---- read once (parallel), then time each file's parse individually ----
    let contents: Vec<(String, String)> = files
        .par_iter()
        .filter_map(|f| {
            std::fs::read_to_string(root.join(f))
                .ok()
                .map(|c| (f.clone(), c))
        })
        .collect();

    // Pass A: production pass-1 shape (parse + entity extraction), per-file timed.
    let t0 = Instant::now();
    let mut cost_extract: Vec<(u64, usize, String)> = contents
        .par_iter()
        .map(|(f, c)| {
            let t = Instant::now();
            let _ = registry.extract_entities_with_tree(f, c);
            (t.elapsed().as_nanos() as u64, c.len(), f.clone())
        })
        .collect();
    let wall_extract = t0.elapsed().as_nanos() as u64;

    // Pass B: the pass-2 re-parse shape (parse only, no extraction), same files.
    let t0 = Instant::now();
    let cost_parse: Vec<(u64, usize, String)> = contents
        .par_iter()
        .map(|(f, c)| {
            let t = Instant::now();
            if let Some(cfg) = get_language_config(ext_of(f)) {
                if !is_pathological_large_file(c) {
                    let _ = parse_tree(cfg, c);
                }
            }
            (t.elapsed().as_nanos() as u64, c.len(), f.clone())
        })
        .collect();
    let wall_parse = t0.elapsed().as_nanos() as u64;

    // Pass C: the same pass-1 work, dispatched longest-file-first. Prices the
    // reorder for real instead of simulating it, and charges it for the
    // `metadata` sweep a production implementation would need to learn the
    // sizes (pass 1 is handed paths, not sizes).
    let t_stat = Instant::now();
    let _restat: Vec<u64> = files
        .par_iter()
        .map(|f| std::fs::metadata(root.join(f)).map(|m| m.len()).unwrap_or(0))
        .collect();
    let stat_ns = t_stat.elapsed().as_nanos() as u64;
    // All three orders go through the same index indirection, so "path" is the
    // control that separates a scheduling effect from an access-pattern one.
    let mut walls: Vec<(&str, u64)> = Vec::new();
    for which in ["path", "desc", "asc", "path", "asc", "path", "asc"] {
        let mut order: Vec<usize> = (0..contents.len()).collect();
        match which {
            "desc" => order.sort_unstable_by_key(|i| std::cmp::Reverse(contents[*i].1.len())),
            "asc" => order.sort_unstable_by_key(|i| contents[*i].1.len()),
            _ => {}
        }
        let t0 = Instant::now();
        let _: Vec<usize> = order
            .par_iter()
            .map(|i| {
                let (f, c) = &contents[*i];
                registry
                    .extract_entities_with_tree(f, c)
                    .map_or(0, |(e, _)| e.len())
            })
            .collect();
        walls.push((which, t0.elapsed().as_nanos() as u64));
    }
    let wall_lpt_real = walls[1].1;
    let series = |tag: &str| -> String {
        let mut v: Vec<f64> = walls
            .iter()
            .filter(|(w, _)| *w == tag)
            .map(|(_, ns)| *ns as f64 / 1e6)
            .collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v.iter().map(|x| format!("{x:.1}")).collect::<Vec<_>>().join("/")
    };
    println!(
        "PP_ORDER label={label} path_ms={} desc_ms={} asc_ms={}",
        series("path"),
        series("desc"),
        series("asc"),
    );

    // Pass D: does the shipped thread-local parser cache still earn its keep?
    // Same parse, but a fresh `tree_sitter::Parser` per file.
    let t0 = Instant::now();
    contents.par_iter().for_each(|(f, c)| {
        if let Some(cfg) = get_language_config(ext_of(f)) {
            if !is_pathological_large_file(c) {
                if let Some(lang) = (cfg.get_language)() {
                    let mut parser = tree_sitter::Parser::new();
                    if parser.set_language(&lang).is_ok() {
                        let _ = parser.parse(c.as_bytes(), None);
                    }
                }
            }
        }
    });
    let wall_fresh_parser = t0.elapsed().as_nanos() as u64;

    // Pass E: the language-config lookup path alone, per file, nothing parsed.
    let t0 = Instant::now();
    let hits: usize = contents
        .par_iter()
        .map(|(f, _)| usize::from(get_language_config(ext_of(f)).is_some()))
        .sum();
    let wall_lookup = t0.elapsed().as_nanos() as u64;
    println!(
        "PP_SQUEEZE label={label} lpt_real_ms={:.1} stat_sweep_ms={:.1} fresh_parser_ms={:.1} \
cached_parser_ms={:.1} config_lookup_ms={:.2} config_hits={hits}",
        wall_lpt_real as f64 / 1e6,
        stat_ns as f64 / 1e6,
        wall_fresh_parser as f64 / 1e6,
        wall_parse as f64 / 1e6,
        wall_lookup as f64 / 1e6,
    );

    for (name, wall, costs) in [
        ("parse_extract", wall_extract, &cost_extract),
        ("parse_only", wall_parse, &cost_parse),
    ] {
        let t1: u64 = costs.iter().map(|(ns, _, _)| ns).sum();
        let span = costs.iter().map(|(ns, _, _)| *ns).max().unwrap_or(0);
        let bytes: u64 = costs.iter().map(|(_, b, _)| *b as u64).sum();
        let flat: Vec<u64> = costs.iter().map(|(ns, _, _)| *ns).collect();
        let mut lpt = flat.clone();
        lpt.sort_unstable_by(|a, b| b.cmp(a));
        println!(
            "PP_BRENT label={label} pass={name} wall_ms={:.1} t1_ms={:.1} span_ms={:.1} \
work_over_p_ms={:.1} util={:.3} sched_order_ms={:.1} sched_lpt_ms={:.1} mb_per_s_1core={:.1}",
            wall as f64 / 1e6,
            t1 as f64 / 1e6,
            span as f64 / 1e6,
            (t1 as f64 / p as f64) / 1e6,
            t1 as f64 / (p as f64 * wall as f64),
            makespan(&flat, p) as f64 / 1e6,
            makespan(&lpt, p) as f64 / 1e6,
            (bytes as f64 / 1e6) / (t1 as f64 / 1e9),
        );
    }

    // ---- top files by cost (the span, named) ----
    cost_extract.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (ns, bytes, f) in cost_extract.iter().take(8) {
        println!(
            "PP_TOP label={label} ms={:.1} kb={} file={f}",
            *ns as f64 / 1e6,
            bytes / 1024
        );
    }

    // ---- chunked (pass-2) simulation at the measured parse-only costs ----
    let cost_by_file: std::collections::HashMap<&str, u64> = cost_parse
        .iter()
        .map(|(ns, _, f)| (f.as_str(), *ns))
        .collect();
    let mut chunked_all = 0u64;
    let mut chunked_used_only = 0u64;
    let mut flat_all: Vec<u64> = Vec::new();
    let mut flat_used: Vec<u64> = Vec::new();
    for r in &visited {
        let mut c_all: Vec<u64> = Vec::new();
        let mut c_used: Vec<u64> = Vec::new();
        for i in r.clone() {
            let ns = cost_by_file.get(files[i].as_str()).copied().unwrap_or(0);
            c_all.push(ns);
            flat_all.push(ns);
            if scope_resolvable(&files[i]) {
                c_used.push(ns);
                flat_used.push(ns);
            }
        }
        chunked_all += makespan(&c_all, p);
        chunked_used_only += makespan(&c_used, p);
    }
    // The reparse's own work split: files a visited chunk actually re-parses
    // (a language config exists) that the scope loop will then decline.
    let mut t1_used = 0u64;
    let mut t1_unused = 0u64;
    let mut n_unused_parsed = 0usize;
    let mut b_unused_parsed = 0u64;
    for r in &visited {
        for i in r.clone() {
            let f = files[i].as_str();
            let ns = cost_by_file.get(f).copied().unwrap_or(0);
            if scope_resolvable(f) {
                t1_used += ns;
            } else {
                t1_unused += ns;
                if get_language_config(ext_of(f)).is_some() {
                    n_unused_parsed += 1;
                    b_unused_parsed += sizes[i];
                }
            }
        }
    }
    println!(
        "PP_SPLIT label={label} t1_used_ms={:.1} t1_unused_ms={:.1} unused_over_p_ms={:.1} \
unused_parsed_files={n_unused_parsed} unused_parsed_bytes={b_unused_parsed}",
        t1_used as f64 / 1e6,
        t1_unused as f64 / 1e6,
        (t1_unused as f64 / p as f64) / 1e6,
    );

    println!(
        "PP_CHUNKSIM label={label} chunked_all_ms={:.1} chunked_used_only_ms={:.1} \
flat_all_ms={:.1} flat_used_ms={:.1}",
        chunked_all as f64 / 1e6,
        chunked_used_only as f64 / 1e6,
        makespan(&flat_all, p) as f64 / 1e6,
        makespan(&flat_used, p) as f64 / 1e6,
    );
    println!("PP_DONE label={label}");
}
