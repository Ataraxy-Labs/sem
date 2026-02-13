# sem (Rust)

Semantic version control CLI. Entity-level diffs, blame, dependency graphs, and impact analysis on top of Git.

Git shows you *line 43 changed*. Sem shows you *function validateToken was modified in src/auth.ts*.

## Why

Git's line-based model doesn't match how developers think. You don't care that lines 12-18 changed. You care that `validateToken` was modified and `legacyAuth` was deleted. This matters even more when agents are making changes, because agents need to reason about *what* changed, not *where* in the file.

## Commands

```bash
# Entity-level diff
sem diff

# Entity-level blame (who last touched each function/class)
sem blame src/auth.ts

# Cross-file dependency graph
sem graph

# Impact analysis (if this entity changes, what else is affected?)
sem impact validateToken

# Filter to specific languages in a multi-language repo
sem graph --file-exts .py
sem diff --file-exts .py .rs
sem impact validateToken --file-exts .py
```

## Languages

11 tree-sitter parser plugins: TypeScript, TSX, JavaScript, Python, Go, Rust, Java, C, C++, Ruby, C#.

Falls back to chunk-based diffing for unsupported file types.

## Architecture

Cargo workspace with two crates:

```
sem-core/    # Library: entity extraction, structural hashing, semantic diff,
             # dependency graph, impact analysis, git bridge
sem-cli/     # Binary: diff, blame, graph, impact commands
```

### sem-core

The library that weave, agenthub, effect-system, agent-lint, unified-build, and agent-bench all depend on.

- **Parser registry** with 11 language plugins via tree-sitter
- **Structural hashing** (AST-normalized, ignores whitespace/comments)
- **Semantic diff** with 3-phase entity matching (exact ID, content hash, fuzzy similarity)
- **Cosmetic vs structural** change detection
- **Entity dependency graph** (cross-file, call/reference edges)
- **Impact analysis** (transitive BFS through dependency graph)
- **Git bridge** for reading file contents at any ref

## Build

```bash
cargo build --release
# Binary at target/release/sem
```

## Tests

```bash
cargo test
# 25 tests
```

## License

MIT
