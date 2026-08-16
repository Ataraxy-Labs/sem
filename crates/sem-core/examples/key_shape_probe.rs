//! semx-5nc: dump the REAL key shapes the cold resolve join hashes on.
//!
//! `RESOLUTION-PROFILE.md`'s "Resolver tie-break contract" section documents
//! `symbol_table: HashMap<String, Vec<String>>` (name -> candidate ids) and
//! `entity_map: HashMap<String, EntityInfo>` (id -> info) as the two tables
//! `resolve_ref`'s join walks on every reference in the corpus. This probe
//! builds one real corpus once (`EntityGraph::build`, same call
//! `mem_single_probe` uses) and, from the returned `entities: EntityInfoMap`
//! (id -> EntityInfo), reconstructs:
//!
//!   - the id-string length distribution (entity_map's key)
//!   - the name-string length distribution and per-name collision (bucket
//!     size) distribution (symbol_table's key and value-length, since
//!     `symbol_table[name]` is exactly the ids of entities named `name` —
//!     RESOLUTION-PROFILE.md line ~5081)
//!
//! and writes a sample of real (name, id) pairs to a flat file so
//! `benches/interning.rs` can replay the exact real distribution instead of
//! a synthetic guess.
//!
//! Usage:
//!   cargo run --release --example key_shape_probe -- <repo_root> <out_file> [sample_n]

use std::collections::HashMap as StdHashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
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

fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn main() {
    let root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: key_shape_probe <repo_root> <out_file> [sample_n]");
    let out_path: PathBuf = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .expect("usage: key_shape_probe <repo_root> <out_file> [sample_n]");
    let sample_n: usize = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);

    let registry = make_registry(&root);
    let file_paths = walk_files(&root);

    let t0 = std::time::Instant::now();
    let (graph, _entities) = EntityGraph::build(&root, &file_paths, &registry);
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // semx-5nc: the interner's own build cost, measured directly against the
    // real corpus (not modeled). This clones every real id string once
    // (unavoidable — a build-scope interner has to own its keys) and inserts
    // into a fresh FxHashMap<String, u32>, i.e. exactly the extra pass a
    // build-scope interner would add on top of today's `entity_map` build.
    // This is a pessimistic *upper bound*: a real integration would populate
    // the interner inline during pass 1 instead of as a wholly separate pass
    // over an already-built map, but the wholly-separate-pass number is the
    // honest one to report since that is what was actually measured.
    let t1 = std::time::Instant::now();
    let mut interner: rustc_hash::FxHashMap<String, u32> =
        rustc_hash::FxHashMap::with_capacity_and_hasher(graph.entities.len(), Default::default());
    for (i, id) in graph.entities.keys().enumerate() {
        interner.insert(id.clone(), i as u32);
    }
    let intern_build_ms = t1.elapsed().as_secs_f64() * 1000.0;
    println!(
        "INTERN_BUILD entities={} intern_build_ms={intern_build_ms:.2} ({:.2} ns/entity)",
        graph.entities.len(),
        intern_build_ms * 1_000_000.0 / graph.entities.len() as f64,
    );
    std::hint::black_box(&interner);

    // entity_map key shape: the id string.
    let mut id_lens: Vec<usize> = graph.entities.keys().map(|id| id.len()).collect();
    id_lens.sort_unstable();

    // symbol_table key/value shape, reconstructed exactly per
    // RESOLUTION-PROFILE.md's stated equivalence: symbol_table[name] is
    // precisely the ids of every entity named `name`.
    let mut by_name: StdHashMap<&str, Vec<&str>> = StdHashMap::new();
    for info in graph.entities.values() {
        by_name
            .entry(info.name.as_str())
            .or_default()
            .push(info.id.as_str());
    }
    let mut name_lens: Vec<usize> = by_name.keys().map(|n| n.len()).collect();
    name_lens.sort_unstable();
    let mut bucket_sizes: Vec<usize> = by_name.values().map(|v| v.len()).collect();
    bucket_sizes.sort_unstable();

    println!(
        "KEY_SHAPE root={} entities={} unique_names={} build_ms={build_ms:.1}",
        root.display(),
        graph.entities.len(),
        by_name.len(),
    );
    println!(
        "id_len   p50={} p90={} p99={} max={} min={}",
        percentile(&id_lens, 0.50),
        percentile(&id_lens, 0.90),
        percentile(&id_lens, 0.99),
        id_lens.last().copied().unwrap_or(0),
        id_lens.first().copied().unwrap_or(0),
    );
    println!(
        "name_len p50={} p90={} p99={} max={} min={}",
        percentile(&name_lens, 0.50),
        percentile(&name_lens, 0.90),
        percentile(&name_lens, 0.99),
        name_lens.last().copied().unwrap_or(0),
        name_lens.first().copied().unwrap_or(0),
    );
    println!(
        "bucket_size (symbol_table[name].len()) p50={} p90={} p99={} max={} mean={:.2} singleton_frac={:.3}",
        percentile(&bucket_sizes, 0.50),
        percentile(&bucket_sizes, 0.90),
        percentile(&bucket_sizes, 0.99),
        bucket_sizes.last().copied().unwrap_or(0),
        bucket_sizes.iter().sum::<usize>() as f64 / bucket_sizes.len().max(1) as f64,
        bucket_sizes.iter().filter(|&&b| b == 1).count() as f64 / bucket_sizes.len().max(1) as f64,
    );

    // Dump a sample of real (name, id) pairs — one line per entity,
    // tab-separated, capped at sample_n so the bench fixture file stays a
    // manageable size while remaining a real, unmodified sample (first N in
    // entity_map iteration order — order is unspecified/arbitrary per the
    // FxHashMap comment at graph.rs:661, not cherry-picked).
    let mut out = std::io::BufWriter::new(std::fs::File::create(&out_path).unwrap());
    for (i, info) in graph.entities.values().enumerate() {
        if i >= sample_n {
            break;
        }
        writeln!(out, "{}\t{}", info.name, info.id).unwrap();
    }
    out.flush().unwrap();
    println!(
        "wrote {} sample (name, id) pairs to {}",
        sample_n.min(graph.entities.len()),
        out_path.display()
    );
}
