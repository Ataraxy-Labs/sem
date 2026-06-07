# sem

Semantic version control CLI. Entity-level diffs for Git (functions, classes, methods instead of lines).

## Structure

Cargo workspace at `crates/`:
- `sem-core` — entity extraction engine, tree-sitter parsers, dependency graph
- `sem-cli` — CLI binary (`sem diff`, `sem graph`, `sem impact`, `sem blame`)

## Build & Test

```bash
cargo build --release -p sem-cli     # binary at target/release/sem
cargo test --workspace               # 134 tests
```

All cargo commands run from repo root. CI builds from `crates/` directory.

## Key Paths

- Parsers: `crates/sem-core/src/parser/plugins/` (24 language plugins + shebang detection)
- Commands: `crates/sem-cli/src/commands/`
- Entity model: `crates/sem-core/src/model/`
- Git integration: `crates/sem-core/src/git/`

## Adding a Language

1. Add tree-sitter grammar to `sem-core/Cargo.toml`
2. Create parser plugin in `sem-core/src/parser/plugins/`
3. Register in `sem-core/src/parser/mod.rs`
4. Add tests
5. See `CONTRIBUTING.md` for the full 7-step guide

## Conventions

- Entity granularity: functions, classes, methods, structs, traits, interfaces
- Methods have a `parentId` linking to their class
- `structural_hash` for matching entities across versions
- Release on tag push (`v*`)
- License: MIT OR Apache-2.0
