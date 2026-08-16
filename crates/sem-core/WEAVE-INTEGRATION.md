# Using this sem-core from weave

weave depends on `sem-core = "0.21.0"` from crates.io. This checkout is
sem-core 0.21.0 plus a content-addressed extraction cache, so weave can consume
it with a path override — no version bump, no API change on weave's side.

## Wiring it up

Add this to the **workspace root** `Cargo.toml` of weave (the `[patch]` section
only takes effect at the workspace root):

```toml
[patch.crates-io]
sem-core = { path = "../sem-weave-perf/crates/sem-core" }
```

Adjust the path to wherever this checkout lives relative to the weave
workspace. Nothing else changes: every `sem-core = "0.21.0"` line in
`crates/weave-*/Cargo.toml` stays as it is, and `cargo` reports the override on
the next build.

To go back to the published crate, delete the `[patch.crates-io]` block and run
`cargo update -p sem-core`.

### Is the override ABI-plausible?

Yes. This checkout is `v0.21.0` plus five commits, only one of which touches
sem-core at all (a `tree-sitter-htmlx-svelte` bump to stop a SIGSEGV). Every
public item weave uses — `create_default_registry`, `ParserRegistry`,
`SemanticParserPlugin::extract_entities`, `SemanticEntity` — is unchanged in
shape. The cache work is purely additive:

| Added | Kind |
|---|---|
| `sem_core::parser::cache` (module) | new |
| `parser::plugins::code::parse_tree` | private → `pub` |
| `parser::plugins::code::parse_tree_incremental` | new |
| `parser::plugins::code::language_config_for_content` | private → `pub` |
| `parser::plugins::code::extract_entities_from_tree` | new re-export |

No signature weave calls was changed, and no behaviour was changed: the cache
returns exactly what a fresh parse returns (`tests/parse_cache.rs` pins the
cached path against the uncached one over real fixture files).

## What weave should expect

weave's merge parses three blobs per file, all at the same path
(`crates/weave-core/src/v2/mod.rs:303-305`):

```rust
let base_all   = plugin.extract_entities(base, file_path);
let ours_all   = plugin.extract_entities(ours, file_path);
let theirs_all = plugin.extract_entities(theirs, file_path);
```

The cache key is `hash(plugin_id, file_path, content)`, so those three calls are
three distinct keys — a single cold merge is **not** helped. What is helped:

1. **Re-merging a blob already seen in this process.** Fleet drivers merge the
   same base repeatedly, and `weave-github` / `weave-driver` hold one
   long-lived `ParserRegistry` per process. The second and later merges of a
   given blob pay a hash + a `Vec` clone instead of a parse.
2. **`base` recurring across files/merges.** The same base blob is re-parsed on
   every merge attempt against it.
3. **Anything else in weave that re-extracts a blob it has already extracted**
   — e.g. `weave-cli`'s `entities_of` walking a repo that also gets merged.

### Measured

Apple silicon, `--release` (`lto = "thin"`, `codegen-units = 1`), criterion
medians, `--warm-up-time 1 --measurement-time 3 --sample-size 30`. Fixtures are
generated (`benches/common/mod.rs`) at three size points per language.

**Where a single extraction's time goes** (`split/*`):

| fixture | lines | entities | blob hash | tree-sitter parse | entity walk |
|---|---:|---:|---:|---:|---:|
| python-small | 53 | 6 | 32.7 ns | 84.2 µs (59%) | 58.3 µs (41%) |
| python-medium | 503 | 66 | 312 ns | 849 µs (58%) | 615 µs (42%) |
| python-large | 3968 | 528 | 2.49 µs | 6.74 ms (58%) | 4.95 ms (42%) |
| typescript-small | 55 | 12 | 30.5 ns | 75.7 µs (49%) | 78.8 µs (51%) |
| typescript-medium | 565 | 122 | 299 ns | 799 µs (49%) | 834 µs (51%) |
| typescript-large | 4492 | 969 | 2.40 µs | 6.45 ms (48%) | 7.09 ms (52%) |
| rust-small | 62 | 10 | 35.2 ns | 67.8 µs (50%) | 66.7 µs (50%) |
| rust-medium | 582 | 90 | 296 ns | 738 µs (50%) | 732 µs (50%) |
| rust-large | 4586 | 706 | 2.47 µs | 5.57 ms (49%) | 5.80 ms (51%) |

The parse and the walk are each about half. **Hashing the blob — the cache key —
is 0.02% of an extraction**, which is why the cache can be on by default: the
miss path pays essentially nothing for the lookup.

**End to end** (`extract/*`): `uncached` is the pre-cache code path, `miss`
forces a cold key on every iteration, `hit` is the warm repeat.

| fixture | uncached | miss (Δ) | hit | hit speedup |
|---|---:|---:|---:|---:|
| python-small | 145.4 µs | 145.5 µs (+0.1%) | 0.60 µs | 241x |
| python-medium | 1.47 ms | 1.48 ms (+0.7%) | 6.29 µs | 234x |
| python-large | 11.83 ms | 11.81 ms (−0.2%) | 57.7 µs | 205x |
| typescript-small | 157.9 µs | 157.6 µs (−0.2%) | 1.15 µs | 137x |
| typescript-medium | 1.65 ms | 1.65 ms (0.0%) | 11.6 µs | 143x |
| typescript-large | 13.35 ms | 13.82 ms (+3.5%) | 113.6 µs | 118x |
| rust-small | 138.2 µs | 138.9 µs (+0.5%) | 0.99 µs | 139x |
| rust-medium | 1.38 ms | 1.36 ms (−1.4%) | 8.86 µs | 156x |
| rust-large | 10.87 ms | 11.06 ms (+1.7%) | 75.8 µs | 143x |

First parse is unchanged: the miss path is within ±3.5% of the pre-cache path
across all nine fixtures. A hit is 0.6-114 µs — under 0.1 ms everywhere except
the 4.5kL/969-entity TypeScript fixture, where cloning ~4000 owned `String`s out
of the cached `Vec<SemanticEntity>` costs 114 µs. That clone is the hit-path
floor; the `Vec<SemanticEntity>` return type is what makes it unavoidable
without an API change.

**weave's call shape** (`merge_pattern/*`): three blobs of one file through
`ParserRegistry::extract_entities`.

| fixture | cold (3 parses) | warm (3 hits) | warm as % of cold |
|---|---:|---:|---:|
| python-small | 458 µs | 2.26 µs | 0.49% |
| python-medium | 4.55 ms | 20.0 µs | 0.44% |
| python-large | 37.0 ms | 180 µs | 0.49% |
| typescript-small | 489 µs | 3.96 µs | 0.81% |
| typescript-medium | 5.09 ms | 35.8 µs | 0.70% |
| typescript-large | 41.1 ms | 342 µs | 0.83% |
| rust-small | 427 µs | 3.26 µs | 0.76% |
| rust-medium | 4.01 ms | 25.0 µs | 0.62% |
| rust-large | 32.1 ms | 216 µs | 0.67% |

### Predicted effect (a prediction, not a promise)

Profiling attributes ~94% of an 11ms merge to parse + entity extraction. On
these benches, a warm re-extraction costs 0.4-0.8% of a cold one. So:

* **First merge of a blob: unchanged.** Measured overhead of the cache on the
  miss path is within noise of the pre-cache path (see `extract/uncached` vs
  `extract/miss` below) — the key hash is ~0.03% of a parse.
* **Repeat merge of the same three blobs: the parse term should nearly vanish**,
  leaving weave's own merge logic (the other ~6%) plus the clone cost. An 11ms
  merge whose 94% is parse would land near 0.7-1.0ms if all three blobs hit.
* **Mixed case (base hits, ours/theirs are new): roughly a third off** the parse
  term, so ~11ms → ~8ms.

Whether a real fleet run sees (2) or (3) depends entirely on blob repetition,
which weave — not sem-core — is in a position to measure. `cache::stats()`
reports `hits` / `misses` / `bytes` if weave wants to log the real ratio.

### Turning it off / tuning it

The cache is on by default and bounded at 64 MiB in-process. All of it is
environment-driven, read once per process:

| Variable | Default | Effect |
|---|---|---|
| `SEM_PARSE_CACHE` | `1` | `0` disables it entirely (the code path becomes exactly what it was) |
| `SEM_PARSE_CACHE_BYTES` | `67108864` | in-process budget in bytes |
| `SEM_PARSE_CACHE_DISK` | `0` | `1` enables the on-disk tier |
| `SEM_PARSE_CACHE_DIR` | `~/.cache/sem/parse` | where the on-disk tier lives |

The on-disk tier is **off by default** and deliberately so: it survives across
processes (useful for a fleet of short-lived `weave-driver` invocations on the
same blobs) but it pays a JSON round-trip, which for a small file can cost more
than the parse it avoids. Turn it on only where blob reuse spans processes.

## Optional: cut the binary by ~89%

Separately from the cache — `sem-core`'s default features build all 28
grammars, and the parse tables are large:

| Feature set | `size_probe` binary |
|---|---|
| `grammar-all` (default) | 69.6 MB |
| `grammar-core` (ts, js, py, go, rust, java) | 7.9 MB |

If weave only needs a subset of languages, declare it in each
`crates/weave-*/Cargo.toml`:

```toml
sem-core = { version = "0.21.0", default-features = false, features = [
    "git", "parallel", "grammar-core",
] }
```

Note this is a *weave* change, not something the `[patch]` override can do for
you — `[patch]` redirects the source, features come from the dependency
declaration. And it is a capability cut: `get_explicit_plugin` will start
returning `None` for the languages you dropped, so weave will hand those files
to git wholesale. Only do this if that is what you want.

There is nothing to gain at *runtime* from lazier grammar loading — see below.

## Lazy grammar loading: already lazy, nothing to reclaim

The 28 grammars were investigated as a startup cost. They are not one:

* `ALL_CONFIGS` is a `static &[&LanguageConfig]` — plain `.rodata`, no
  initializer, no `LazyLock`, no allocation.
* `LanguageConfig::get_language` is a `fn() -> Option<Language>` pointer, called
  only when a file of that language is parsed. Calling it costs **1.4 ns**
  (`startup/first_grammar_touch`) because it just wraps a pointer to the static
  parse tables.
* Building the whole registry costs **3.6 µs** (`startup/create_default_registry`)
  — 0.03% of a single 11 ms merge, and weave already builds it once per process
  behind a `LazyLock`.
* The grammar tables are demand-paged by the OS. A process that only ever parses
  TypeScript never faults in the Fortran tables.

So there is no "lazy init" left to add: initialization is already free, and the
remaining cost is bytes on disk and address space, which only a feature cut can
reclaim (see the previous section). Per-language feature flags already exist and
are the right lever; adding a runtime lazy-loading mechanism would buy nothing
and cost a `dlopen`-shaped problem.

The one number worth knowing: **linear-scan language lookup costs 58 ns** per
call (`startup/language_config_lookup`, scanning ~36 configs for an extension
match). At three lookups per merge that is invisible, but a caller doing it per
file in a large repo walk could reasonably hoist it.

## Running the benches

```sh
cargo bench -p sem-core --bench parse_profile   # phase split, cache hit/miss, merge pattern
cargo bench -p sem-core --bench incremental     # incremental reparse spike
```

`parse_profile` reports four groups:

* `split/{hash,parse,walk}` — where a single extraction's time goes.
* `extract/{uncached,miss,hit}` — `uncached` is the pre-cache code path,
  `miss` forces a cold key every iteration, `hit` is the warm repeat.
* `merge_pattern/{cold,warm}` — three blobs of one file through
  `ParserRegistry::extract_entities`, i.e. weave's actual call shape.
* `startup/*` — registry construction and grammar-table touch costs.
