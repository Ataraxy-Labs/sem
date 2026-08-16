//! Diagnostic probe for semx-jo1: which large files' parse duration hovers
//! near `PARSE_TIME_BUDGET` (10s)? Walks a repo root, finds every file whose
//! content exceeds `LARGE_FILE_BUDGET_THRESHOLD` (the size gate that engages
//! the budget mechanism at all), parses each one sequentially (no
//! contention, no chunking -- an isolated per-file baseline) with the plain
//! unconditional `parse_tree`, and reports (duration, byte_len, max_line_len,
//! path) sorted slowest-first. Not part of the public example surface this
//! bead ships -- a one-off measurement to find semx-4w1's "borderline file"
//! before designing semx-jo1's deterministic replacement gate.
//!
//! Usage: cargo run --release --example parse_time_probe -- <repo_root>

use std::path::PathBuf;
use std::time::Instant;

use sem_core::parser::plugins::code::{language_config_for_content, parse_tree};

fn max_line_len(content: &str) -> usize {
    content
        .as_bytes()
        .split(|&b| b == b'\n')
        .map(|line| line.len())
        .max()
        .unwrap_or(0)
}

fn main() {
    let root: PathBuf = std::env::args()
        .nth(1)
        .expect("usage: parse_time_probe <root>")
        .into();
    const LARGE_FILE_BUDGET_THRESHOLD: u64 = 128 * 1024;

    let mut results: Vec<(std::time::Duration, u64, usize, String)> = Vec::new();
    let mut walker = vec![root.clone()];
    let mut scanned = 0usize;
    while let Some(dir) = walker.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                    continue;
                }
                walker.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() <= LARGE_FILE_BUDGET_THRESHOLD {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let Some(config) = language_config_for_content(&content, &rel) else {
                continue;
            };
            scanned += 1;
            let mll = max_line_len(&content);
            let t0 = Instant::now();
            let tree = parse_tree(config, &content);
            let dt = t0.elapsed();
            eprintln!(
                "{:>8.3}s  {:>10} bytes  maxline={:>10}  {}  parsed={}",
                dt.as_secs_f64(),
                content.len(),
                mll,
                rel,
                tree.is_some()
            );
            results.push((dt, content.len() as u64, mll, rel));
        }
    }
    results.sort_by(|a, b| b.0.cmp(&a.0));
    println!(
        "\n=== sorted slowest-first ({} large files scanned) ===",
        scanned
    );
    for (dt, len, mll, path) in results.iter().take(30) {
        println!(
            "{:>8.3}s  {:>10} bytes  maxline={:>10}  {}",
            dt.as_secs_f64(),
            len,
            mll,
            path
        );
    }
}
