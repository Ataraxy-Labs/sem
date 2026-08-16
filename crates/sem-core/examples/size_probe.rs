//! A minimal binary that links the parser registry, used to measure what the
//! grammar feature set costs in binary size.
//!
//! ```sh
//! cargo build -p sem-core --release --example size_probe
//! cargo build -p sem-core --release --example size_probe \
//!     --no-default-features --features git,parallel,grammar-core
//! ```
//!
//! It parses a file so nothing gets dead-code-eliminated, then prints how long
//! registry construction took — the startup half of the same question.

use std::time::Instant;

fn main() {
    let started = Instant::now();
    let registry = sem_core::parser::plugins::create_default_registry();
    let build_time = started.elapsed();

    let path = std::env::args().nth(1);
    let (path, source) = match path {
        Some(p) => {
            let source = std::fs::read_to_string(&p).unwrap_or_default();
            (p, source)
        }
        None => (
            "probe.rs".to_string(),
            "pub fn probe() -> u8 { 42 }\n".to_string(),
        ),
    };

    let entities = registry.extract_entities(&path, &source);
    println!(
        "registry built in {build_time:?}; {} entities from {} ({} bytes)",
        entities.len(),
        path,
        source.len()
    );

    // Also the smallest end-to-end check of the cache tiers: run this twice
    // with SEM_PARSE_CACHE_DISK=1 and the second process should report a disk
    // hit, since its in-process cache starts empty.
    let stats = sem_core::parser::cache::stats();
    println!(
        "cache: enabled={} disk={} hits={} misses={} disk_hits={} disk_writes={} entries={} bytes={}",
        stats.enabled,
        stats.disk_enabled,
        stats.hits,
        stats.misses,
        stats.disk_hits,
        stats.disk_writes,
        stats.entries,
        stats.bytes
    );
}
