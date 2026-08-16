//! semx-5nc: micro-bench the cold resolve join's actual lookup shapes —
//! `symbol_table: HashMap<String, Vec<String>>` (name -> candidate ids) and
//! `entity_map: HashMap<String, EntityInfo>` (id -> info), the two tables
//! `resolve_ref` (`scope_resolve.rs`) walks for every reference in the
//! corpus — before proposing to replace their `String` keys with u32
//! interned symbols. This is the micro-benchmark 918f12a explicitly left
//! undone ("No micro-benchmark of `String` vs `Arc<str>` vs `u32` was run;
//! the choice is justified by struct size and allocation count" —
//! RESOLUTION-PROFILE.md's Interning section, the semx-4an second pass).
//!
//! ```sh
//! cargo bench -p sem-core --bench interning
//! ```
//!
//! # Data: calibrated to the real corpus, not guessed
//!
//! `examples/key_shape_probe.rs` walked the TypeScript monster
//! (`~/.cache/checkouts/github.com/microsoft/TypeScript`, 714,819 entities,
//! excludes applied — RESOLUTION-PROFILE.md's other sections' 454,541 figure
//! is a later, further-processed count) and measured the REAL key shapes:
//!
//! ```text
//! id_len   p50=87  p90=131 p99=181 max=1699 min=18
//! name_len p50=17  p90=28  p99=82  max=1650 min=0
//! bucket_size (symbol_table[name].len())  p50=1 p90=6 p99=71 max=25030
//!   mean=8.24  singleton_frac=0.555
//! INTERN_BUILD entities=714819 intern_build_ms=54.13 (75.72 ns/entity)
//! ```
//!
//! This bench does not check real Microsoft-repo identifier text into the
//! tree (that's third-party source, and the previous bead's own fixture
//! convention — `benches/common/mod.rs` — is "generated rather than checked
//! in" for exactly this reason: determinism without carrying real corpus
//! text). Instead it generates a synthetic corpus whose id/name length
//! distributions and symbol_table bucket-size distribution are calibrated
//! to hit the percentile anchors above via piecewise-linear interpolation —
//! disclosed as an approximation of the real shape, not a byte-for-byte
//! replay of it. The literal per-entity interner build cost above (measured
//! directly against the real corpus, not modeled) is the number that
//! answers "build cost at ~455k-715k entities"; this file's job is the
//! *lookup* shape, which is inherently a repeated-query benchmark criterion
//! is built for and a one-shot probe is not.

use std::collections::HashMap as StdHashMap;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use rustc_hash::FxHashMap;
use sem_core::parser::graph::EntityInfo;
use std::hint::black_box;

// ---------------------------------------------------------------------
// Deterministic corpus generation, calibrated to the real percentiles
// above. splitmix64 avoids adding a `rand` dependency this crate's other
// benches don't already have.
// ---------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gen_string(&mut self, len: usize) -> String {
        // Path/identifier-shaped alphabet, not uniform-random bytes — real
        // entity ids are `{file_path}::{kind}::{name}` chains, real names
        // are identifier text. Hash cost scales with byte length regardless
        // of alphabet, but this keeps the strings visually representative.
        const ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_/.";
        let mut s = String::with_capacity(len);
        let mut since_sep = 0u32;
        for _ in 0..len {
            if since_sep > 6 && self.next_f64() < 0.15 {
                s.push_str("::");
                since_sep = 0;
            } else {
                let idx = (self.next_u64() as usize) % ALPHABET.len();
                s.push(ALPHABET[idx] as char);
                since_sep += 1;
            }
        }
        s
    }
}

/// Piecewise-linear interpolation over measured (percentile, value) anchors.
fn interp(anchors: &[(f64, usize)], p: f64) -> usize {
    for w in anchors.windows(2) {
        let (p0, v0) = w[0];
        let (p1, v1) = w[1];
        if p <= p1 {
            if p1 <= p0 {
                return v0;
            }
            let t = (p - p0) / (p1 - p0);
            return (v0 as f64 + t * (v1 as f64 - v0 as f64)).round() as usize;
        }
    }
    anchors.last().unwrap().1
}

const ID_LEN_ANCHORS: &[(f64, usize)] = &[
    (0.0, 18),
    (0.5, 87),
    (0.9, 131),
    (0.99, 181),
    (0.999, 400),
    (1.0, 1699),
];
const NAME_LEN_ANCHORS: &[(f64, usize)] = &[
    (0.0, 3),
    (0.5, 17),
    (0.9, 28),
    (0.99, 82),
    (0.999, 300),
    (1.0, 1650),
];
// bucket_size(name) = |symbol_table[name]| — measured p50=1 (55.5%
// singleton), p90=6, p99=71, max=25030, mean=8.24.
const BUCKET_SIZE_ANCHORS: &[(f64, usize)] = &[
    (0.0, 1),
    (0.555, 1),
    (0.90, 6),
    (0.99, 71),
    (0.999, 600),
    (1.0, 25030),
];

/// One generated entity: its name (symbol_table key) and id (entity_map
/// key), plus a real `EntityInfo` value shaped like what a hit actually
/// returns (four Strings + Option<String> + two usizes — same struct
/// `resolve_ref` reads through `entity_map.get`).
struct Corpus {
    /// (name, id) pairs in generation order, grouped by name (so bucket i's
    /// members are contiguous) — this is symbol_table's insertion source.
    entries: Vec<(String, String)>,
    values: FxHashMap<String, EntityInfo>,
}

fn gen_corpus(target_entities: usize) -> Corpus {
    let mut rng = Rng(0xC0FFEE_1234_5678);
    let mut entries = Vec::with_capacity(target_entities);
    let mut values = FxHashMap::default();
    values.reserve(target_entities);

    let mut n = 0usize;
    let mut name_idx = 0usize;
    // Rank names by descending percentile so the biggest (rarest) bucket is
    // generated last, matching a right-skewed real distribution; stop once
    // we've generated roughly the target entity count.
    while n < target_entities {
        let p = name_idx as f64 / 60_000.0; // ~ the real corpus's unique-name count scale
        let bucket_size = interp(BUCKET_SIZE_ANCHORS, p.min(1.0)).max(1);
        let name_len = interp(NAME_LEN_ANCHORS, rng.next_f64()).max(1);
        let name = rng.gen_string(name_len);

        for _ in 0..bucket_size {
            let id_len = interp(ID_LEN_ANCHORS, rng.next_f64()).max(8);
            let id = rng.gen_string(id_len);
            let info = EntityInfo {
                id: id.clone(),
                name: name.clone(),
                entity_type: "function".to_string(),
                file_path: id[..id.len().min(40)].to_string(),
                parent_id: None,
                start_line: (n % 5000) + 1,
                end_line: (n % 5000) + 20,
            };
            values.insert(id.clone(), info);
            entries.push((name.clone(), id));
            n += 1;
            if n >= target_entities {
                break;
            }
        }
        name_idx += 1;
    }

    Corpus { entries, values }
}

// ---------------------------------------------------------------------
// Backend A: today's production shape — FxHashMap<String, _> for both
// tables (already rustc-hash, not std SipHash — see graph.rs:661).
// Backend B: std::collections::HashMap<String, _> (SipHash) — the naive
// baseline the bead's instructions asked to include explicitly.
// Backend C: build-scope u32 interning — entity_map becomes a flat
// Vec<EntityInfo> indexed by token, symbol_table's values become Vec<u32>.
// symbol_table's own key (the name) stays a string lookup in all three: a
// call-site name is a freshly-borrowed &str out of source text, never an
// already-interned token, so translating it costs one hash regardless of
// backend — interning cannot remove that hash, only the entity_map side.
// ---------------------------------------------------------------------

fn build_fx(
    corpus: &Corpus,
) -> (
    FxHashMap<String, Vec<String>>,
    FxHashMap<String, EntityInfo>,
) {
    let mut symbol_table: FxHashMap<String, Vec<String>> = FxHashMap::default();
    for (name, id) in &corpus.entries {
        symbol_table
            .entry(name.clone())
            .or_default()
            .push(id.clone());
    }
    (symbol_table, corpus.values.clone())
}

fn build_std(
    corpus: &Corpus,
) -> (
    StdHashMap<String, Vec<String>>,
    StdHashMap<String, EntityInfo>,
) {
    let mut symbol_table: StdHashMap<String, Vec<String>> = StdHashMap::new();
    let mut entity_map: StdHashMap<String, EntityInfo> = StdHashMap::new();
    for (name, id) in &corpus.entries {
        symbol_table
            .entry(name.clone())
            .or_default()
            .push(id.clone());
    }
    for (id, info) in &corpus.values {
        entity_map.insert(id.clone(), info.clone());
    }
    (symbol_table, entity_map)
}

/// Backend C's build: a build-scope interner (String -> u32) plus a flat
/// `Vec<EntityInfo>` entity table indexed by token, and `symbol_table`'s
/// values re-expressed as `Vec<u32>`.
fn build_interned(
    corpus: &Corpus,
) -> (
    FxHashMap<String, u32>,
    FxHashMap<String, Vec<u32>>,
    Vec<EntityInfo>,
) {
    let mut interner: FxHashMap<String, u32> = FxHashMap::default();
    let mut entity_table: Vec<EntityInfo> = Vec::with_capacity(corpus.values.len());
    // Deterministic token assignment: iterate entries in generation order so
    // every id gets a token exactly once, matching a real build's one-pass
    // pass-1 assignment.
    let mut symbol_table_u32: FxHashMap<String, Vec<u32>> = FxHashMap::default();
    for (name, id) in &corpus.entries {
        let token = *interner.entry(id.clone()).or_insert_with(|| {
            let info = corpus.values.get(id).unwrap().clone();
            entity_table.push(info);
            (entity_table.len() - 1) as u32
        });
        symbol_table_u32
            .entry(name.clone())
            .or_default()
            .push(token);
    }
    (interner, symbol_table_u32, entity_table)
}

// ---------------------------------------------------------------------
// Query workload: a realistic subset of (name) lookups drawn from the real
// bucket-size shape — most queries hit a singleton or small bucket (55.5%
// of names are singletons), a few hit the ~25k-wide hot buckets, matching
// `resolve_ref`'s actual traffic (most calls resolve to one obvious
// candidate; ambiguous/overloaded names are the expensive tail).
// ---------------------------------------------------------------------

fn gen_queries(corpus: &Corpus, m: usize) -> Vec<String> {
    let mut rng = Rng(0xABCD_EF01_2345_6789);
    let unique_names: Vec<&String> = {
        let mut seen = FxHashMap::default();
        let mut v = Vec::new();
        for (name, _) in &corpus.entries {
            if seen.insert(name.as_str(), ()).is_none() {
                v.push(name);
            }
        }
        v
    };
    (0..m)
        .map(|_| {
            let idx = (rng.next_u64() as usize) % unique_names.len();
            unique_names[idx].clone()
        })
        .collect()
}

// ---------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------

const N_ENTITIES: usize = 200_000; // scaled down from 455k/715k for a
                                   // tractable per-iteration bench; the
                                   // shape (length + bucket distributions)
                                   // is preserved, not the absolute count.
                                   // The literal 715k-entity build cost is
                                   // measured directly in
                                   // examples/key_shape_probe.rs instead.
const N_QUERIES: usize = 20_000;

fn bench_build(c: &mut Criterion) {
    let corpus = gen_corpus(N_ENTITIES);
    let mut group = c.benchmark_group("build");
    group.sample_size(20);

    group.bench_function("std_string_keyed", |b| {
        b.iter_batched(
            || &corpus,
            |c| black_box(build_std(c)),
            BatchSize::LargeInput,
        );
    });
    group.bench_function("fx_string_keyed_current", |b| {
        b.iter_batched(
            || &corpus,
            |c| black_box(build_fx(c)),
            BatchSize::LargeInput,
        );
    });
    group.bench_function("fx_u32_interned", |b| {
        b.iter_batched(
            || &corpus,
            |c| black_box(build_interned(c)),
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

/// The actual join `resolve_ref` performs: name -> symbol_table lookup ->
/// candidate id list -> entity_map.get(id) for (up to 4) candidates, same
/// shape as `resolve_ref`'s ambiguity-scan loops in `scope_resolve.rs`.
fn bench_join_lookup(c: &mut Criterion) {
    let corpus = gen_corpus(N_ENTITIES);
    let queries = gen_queries(&corpus, N_QUERIES);

    let (std_symtab, std_entities) = build_std(&corpus);
    let (fx_symtab, fx_entities) = build_fx(&corpus);
    let (interner, u32_symtab, u32_entities) = build_interned(&corpus);

    let mut group = c.benchmark_group("join_lookup");

    group.bench_function("std_string_keyed", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for name in &queries {
                if let Some(ids) = std_symtab.get(name.as_str()) {
                    for id in ids.iter().take(4) {
                        if let Some(info) = std_entities.get(id.as_str()) {
                            acc = acc.wrapping_add(info.start_line);
                        }
                    }
                }
            }
            black_box(acc)
        });
    });

    group.bench_function("fx_string_keyed_current", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for name in &queries {
                if let Some(ids) = fx_symtab.get(name.as_str()) {
                    for id in ids.iter().take(4) {
                        if let Some(info) = fx_entities.get(id.as_str()) {
                            acc = acc.wrapping_add(info.start_line);
                        }
                    }
                }
            }
            black_box(acc)
        });
    });

    group.bench_function("fx_u32_interned", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for name in &queries {
                // The name -> symbol_table_u32 hop is still a String hash —
                // interning does not change this side (see the comment
                // block above); only the id -> entity table hop becomes a
                // direct index instead of a second string hash.
                if let Some(tokens) = u32_symtab.get(name.as_str()) {
                    for &tok in tokens.iter().take(4) {
                        let info = &u32_entities[tok as usize];
                        acc = acc.wrapping_add(info.start_line);
                    }
                }
            }
            black_box((acc, interner.len()))
        });
    });

    group.finish();
}

/// Isolated id-only lookup — the exact question 918f12a deferred: is
/// `entity_map.get(id)` alone (no name hop in front of it) meaningfully
/// cheaper as a `u32` index than a `String` hash. Uses only the ids
/// already present in the corpus (a realistic "id already held in memory"
/// access, e.g. a parent_id/target_id field) rather than a fresh name
/// lookup.
fn bench_id_only_lookup(c: &mut Criterion) {
    let corpus = gen_corpus(N_ENTITIES);
    let (_, std_entities) = build_std(&corpus);
    let (_, fx_entities) = build_fx(&corpus);
    let (interner, _, u32_entities) = build_interned(&corpus);

    // ids to query: a deterministic sample of real ids from the corpus,
    // and their pre-resolved tokens for the u32 backend (representing an
    // id that arrived already-interned, e.g. from a symbol_table bucket).
    let mut rng = Rng(0x1357_9BDF_2468_ACE0);
    let sample_ids: Vec<String> = (0..N_QUERIES)
        .map(|_| {
            let idx = (rng.next_u64() as usize) % corpus.entries.len();
            corpus.entries[idx].1.clone()
        })
        .collect();
    let sample_tokens: Vec<u32> = sample_ids
        .iter()
        .map(|id| *interner.get(id).unwrap())
        .collect();

    let mut group = c.benchmark_group("id_only_lookup");

    group.bench_function("std_string_keyed", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for id in &sample_ids {
                if let Some(info) = std_entities.get(id.as_str()) {
                    acc = acc.wrapping_add(info.start_line);
                }
            }
            black_box(acc)
        });
    });

    group.bench_function("fx_string_keyed_current", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for id in &sample_ids {
                if let Some(info) = fx_entities.get(id.as_str()) {
                    acc = acc.wrapping_add(info.start_line);
                }
            }
            black_box(acc)
        });
    });

    group.bench_function("fx_u32_interned_token_in_hand", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for &tok in &sample_tokens {
                let info = &u32_entities[tok as usize];
                acc = acc.wrapping_add(info.start_line);
            }
            black_box(acc)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_build,
    bench_join_lookup,
    bench_id_only_lookup
);
criterion_main!(benches);
