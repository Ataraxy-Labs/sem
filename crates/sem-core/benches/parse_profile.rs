//! Where the time goes in `extract_entities`, and what the content-addressed
//! cache changes.
//!
//! Run everything:
//!
//! ```sh
//! cargo bench -p sem-core --bench parse_profile
//! ```
//!
//! The `split/*` group attributes a single extraction across its three phases:
//! hashing the blob (the cache key), tree-sitter parsing, and the entity walk.
//! The `extract/*` group compares the three end-to-end paths that matter:
//! the pre-cache code path, a forced cache miss, and a cache hit.

mod common;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use sem_core::parser::cache;
use sem_core::parser::plugin::SemanticParserPlugin;
use sem_core::parser::plugins::code::{
    extract_entities_from_tree, language_config_for_content, parse_tree, CodeParserPlugin,
};
use sem_core::parser::plugins::create_default_registry;

/// Attribute one extraction across hash / parse / walk.
fn bench_split(c: &mut Criterion) {
    let plugin = CodeParserPlugin;
    let mut group = c.benchmark_group("split");

    for (size, fixture) in common::all() {
        let id = format!("{}-{}", fixture.id, size);
        let config = language_config_for_content(&fixture.source, &fixture.path)
            .unwrap_or_else(|| panic!("no language config for {}", fixture.path));
        let tree = parse_tree(config, &fixture.source).expect("parse failed");
        let entity_count = plugin
            .extract_entities_with_tree(&fixture.source, &fixture.path)
            .0
            .len();
        eprintln!(
            "fixture {id}: {} lines, {} bytes, {entity_count} entities",
            fixture.lines(),
            fixture.source.len()
        );

        group.throughput(Throughput::Bytes(fixture.source.len() as u64));

        // Phase 1: the cache key. This is the only cost a hit cannot avoid.
        group.bench_with_input(BenchmarkId::new("hash", &id), &fixture, |b, f| {
            b.iter(|| black_box(xxhash_rust::xxh3::xxh3_128(f.source.as_bytes())));
        });

        // Phase 2: tree-sitter parse, reusing the thread-local Parser as the
        // real code path does.
        group.bench_with_input(BenchmarkId::new("parse", &id), &fixture, |b, f| {
            b.iter(|| black_box(parse_tree(config, &f.source)));
        });

        // Phase 3: the entity-extraction walk over an already-built tree.
        group.bench_with_input(BenchmarkId::new("walk", &id), &fixture, |b, f| {
            b.iter(|| {
                black_box(extract_entities_from_tree(
                    &tree, &f.path, config, &f.source,
                ))
            });
        });
    }
    group.finish();
}

/// The three end-to-end paths: pre-cache, forced miss, hit.
fn bench_extract(c: &mut Criterion) {
    let plugin = CodeParserPlugin;
    let mut group = c.benchmark_group("extract");

    for (size, fixture) in common::all() {
        let id = format!("{}-{}", fixture.id, size);
        group.throughput(Throughput::Bytes(fixture.source.len() as u64));

        // The code path as it was before the cache existed: parse + walk, no
        // lookup, no insert. This is the ±5% baseline for "first parse".
        group.bench_with_input(BenchmarkId::new("uncached", &id), &fixture, |b, f| {
            b.iter(|| black_box(plugin.extract_entities_with_tree(&f.source, &f.path).0));
        });

        // Every iteration misses: `clear` runs outside the timed region, so
        // this is parse + walk + key hash + insert + the clone handed back.
        group.bench_with_input(BenchmarkId::new("miss", &id), &fixture, |b, f| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    cache::clear();
                    let start = std::time::Instant::now();
                    black_box(plugin.extract_entities(&f.source, &f.path));
                    total += start.elapsed();
                }
                total
            });
        });

        // Warm: the case weave hits on every repeat merge of the same blob.
        plugin.extract_entities(&fixture.source, &fixture.path);
        group.bench_with_input(BenchmarkId::new("hit", &id), &fixture, |b, f| {
            b.iter(|| black_box(plugin.extract_entities(&f.source, &f.path)));
        });
    }
    group.finish();
}

/// A merge's parse workload: base, ours, theirs — three versions of one file,
/// two of which differ from base by a single edit region.
fn bench_merge_pattern(c: &mut Criterion) {
    let registry = create_default_registry();
    let mut group = c.benchmark_group("merge_pattern");

    for (size, fixture) in common::all() {
        let id = format!("{}-{}", fixture.id, size);
        // A third distinct blob, still valid in every fixture language.
        let theirs = format!("{}\n", fixture.edited);
        let sides = [
            fixture.source.clone(),
            fixture.edited.clone(),
            theirs.clone(),
        ];
        group.throughput(Throughput::Bytes(
            sides.iter().map(|s| s.len() as u64).sum::<u64>(),
        ));

        // Cold: nothing has been seen before, as on the first merge touching
        // this file.
        group.bench_with_input(BenchmarkId::new("cold", &id), &sides, |b, sides| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    cache::clear();
                    let start = std::time::Instant::now();
                    for side in sides {
                        black_box(registry.extract_entities(&fixture.path, side));
                    }
                    total += start.elapsed();
                }
                total
            });
        });

        // Warm: the same three blobs re-merged, as across a fleet.
        for side in &sides {
            registry.extract_entities(&fixture.path, side);
        }
        group.bench_with_input(BenchmarkId::new("warm", &id), &sides, |b, sides| {
            b.iter(|| {
                for side in sides {
                    black_box(registry.extract_entities(&fixture.path, side));
                }
            });
        });
    }
    group.finish();
}

/// What the 28-language grammar tables cost before the first parse.
fn bench_startup(c: &mut Criterion) {
    let mut group = c.benchmark_group("startup");

    group.bench_function("create_default_registry", |b| {
        b.iter(|| black_box(create_default_registry()));
    });

    // Language lookup is a linear scan of a static table of ~36 configs.
    let py = common::python(common::SMALL);
    group.bench_function("language_config_lookup", |b| {
        b.iter(|| black_box(language_config_for_content(&py.source, &py.path)));
    });

    // First touch of a grammar: builds the tree-sitter Language from the
    // static parse tables. Amortized to nothing after the first file.
    group.bench_function("first_grammar_touch", |b| {
        let config = language_config_for_content(&py.source, &py.path).unwrap();
        b.iter(|| black_box((config.get_language)()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_split,
    bench_extract,
    bench_merge_pattern,
    bench_startup
);
criterion_main!(benches);
