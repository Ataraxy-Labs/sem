# Large JS/TS Fixture Benchmark

This benchmark provides a public, repeatable TypeScript/JavaScript fixture for graph-build performance work. It can be paired with `SEM_TIMINGS=1` or `SEM_TIMINGS=json` to collect CLI phase timings while it measures cold cache, warm cache, and one-file incremental runs.

## Generate a Fixture

```bash
node scripts/large-js-fixture.mjs \
  --out /tmp/sem-large-js-fixture \
  --files 1000 \
  --entities-per-file 12 \
  --fanout 5 \
  --import-style-mix named:3,default:1,namespace:1,type:1 \
  --nested-depth 3 \
  --body-lines 40 \
  --language mixed
```

The generator writes a standalone fixture root with `src/`, `package.json`, `tsconfig.json`, `fixture-manifest.json`, and a marker file. Existing marked fixture roots are replaced for repeatability. Non-empty unmarked directories are refused unless `--force` is supplied.

## Time sem

```bash
node benchmarks/large-js-fixture/run.mjs \
  --sem crates/target/debug/sem \
  --files 1000 \
  --entities-per-file 12 \
  --fanout 5 \
  --import-style-mix named:3,default:1,namespace:1,type:1 \
  --nested-depth 3 \
  --body-lines 40 \
  --language mixed
```

The runner:

1. regenerates the fixture,
2. clears the benchmark cache root,
3. runs a cold-cache command,
4. runs the same command with a warm cache,
5. mutates one marker inside the first source file,
6. runs the same command again for one-file incremental rebuild timing.

By default it runs:

```bash
sem graph . --json --file-exts .ts .js
```

It writes machine-readable results to `benchmarks/large-js-fixture/.generated/results.json`. Use `--json-out <path>` to choose another path or `--no-json-out` to print only the terminal summary.

## Useful Knobs

- `--files`: source file count.
- `--entities-per-file`: top-level generated value entities in each source file.
- `--fanout`: number of neighboring files imported by each file.
- `--import-style-mix`: comma-separated style cycle with optional weights. Supported styles are `named`, `default`, `namespace`, `type`, and `side-effect`.
- `--nested-depth`: nested local function depth inside each top-level entity.
- `--body-lines`: extra executable lines in each top-level entity body.
- `--language`: `ts`, `js`, or `mixed`.
- `--command`: `graph`, `impact`, `context`, or `verify`.
- `--impact-mode`: `deps`, `dependents`, `tests`, or `all`. The default is `deps` so `impact` benchmarks exercise the topology-only path.
- `--timeout-ms`: per-run guard for commands under investigation.
- `--cache-dir`: cache root. Keep it outside `--out` so sem accepts the override.

The default output lives under `benchmarks/large-js-fixture/.generated/`. The generated fixture is initialized as its own Git repository by default so sem treats that directory as the repository root even when the fixture is inside this checkout.
