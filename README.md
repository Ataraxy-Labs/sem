# sem

Semantic version control. Entity-level diffs on top of Git.

Instead of *line 43 changed*, sem tells you *function validateToken was added in src/auth.ts*.

```
sem diff

┌─ src/auth/login.ts ──────────────────────────────────
│
│  ⊕ function  validateToken          [added]
│  ∆ function  authenticateUser       [modified]
│  ⊖ function  legacyAuth             [deleted]
│
└──────────────────────────────────────────────────────

┌─ config/database.yml ─────────────────────────────────
│
│  ∆ property  production.pool_size   [modified]
│    - 5
│    + 20
│
└──────────────────────────────────────────────────────

Summary: 1 added, 1 modified, 1 deleted across 2 files
```

## Install

Build from source (requires Rust):

```bash
git clone https://github.com/Ataraxy-Labs/sem
cd sem/crates
cargo install --path sem-cli
```

Or grab a binary from [GitHub Releases](https://github.com/Ataraxy-Labs/sem/releases).

## Usage

Works in any Git repo. No setup required.

```bash
# Semantic diff of working changes
sem diff

# Staged changes only
sem diff --staged

# Specific commit
sem diff --commit abc1234

# Commit range
sem diff --from HEAD~5 --to HEAD

# JSON output (for AI agents, CI pipelines)
sem diff --format json

# Read file changes from stdin (no git repo needed)
echo '[{"filePath":"src/main.rs","status":"modified","beforeContent":"...","afterContent":"..."}]' \
  | sem diff --stdin --format json

# Only specific file types
sem diff --file-exts .py .rs

# Entity dependency graph
sem graph

# Impact analysis (what breaks if this entity changes?)
sem impact validateToken

# Entity-level blame
sem blame src/auth.ts
```

## What it parses

13 programming languages with full entity extraction via tree-sitter:

| Language | Extensions | Entities |
|----------|-----------|----------|
| TypeScript | `.ts` `.tsx` | functions, classes, interfaces, types, enums, exports |
| JavaScript | `.js` `.jsx` `.mjs` `.cjs` | functions, classes, variables, exports |
| Python | `.py` | functions, classes, decorated definitions |
| Go | `.go` | functions, methods, types, vars, consts |
| Rust | `.rs` | functions, structs, enums, impls, traits, mods, consts |
| Java | `.java` | classes, methods, interfaces, enums, fields, constructors |
| C | `.c` `.h` | functions, structs, enums, unions, typedefs |
| C++ | `.cpp` `.cc` `.hpp` | functions, classes, structs, enums, namespaces, templates |
| C# | `.cs` | classes, methods, interfaces, enums, structs, properties |
| Ruby | `.rb` | methods, classes, modules |
| PHP | `.php` | functions, classes, methods, interfaces, traits, enums |
| Fortran | `.f90` `.f95` `.f` | functions, subroutines, modules, programs |

Plus structured data formats:

| Format | Extensions | Entities |
|--------|-----------|----------|
| JSON | `.json` | properties, objects (RFC 6901 paths) |
| YAML | `.yml` `.yaml` | sections, properties (dot paths) |
| TOML | `.toml` | sections, properties |
| CSV | `.csv` `.tsv` | rows (first column as identity) |
| Markdown | `.md` `.mdx` | heading-based sections |

Everything else falls back to chunk-based diffing.

## How matching works

Three-phase entity matching:

1. **Exact ID match** — same entity in before/after = modified or unchanged
2. **Structural hash match** — same AST structure, different name = renamed or moved (ignores whitespace/comments)
3. **Fuzzy similarity** — >80% token overlap = probable rename

This means sem detects renames and moves, not just additions and deletions. Structural hashing also distinguishes cosmetic changes (whitespace, formatting) from real logic changes.

## JSON output

```bash
sem diff --format json
```

```json
{
  "summary": {
    "fileCount": 2,
    "added": 1,
    "modified": 1,
    "deleted": 1,
    "total": 3
  },
  "changes": [
    {
      "entityId": "src/auth.ts::function::validateToken",
      "changeType": "added",
      "entityType": "function",
      "entityName": "validateToken",
      "filePath": "src/auth.ts"
    }
  ]
}
```

## As a library

sem-core can be used as a Rust library dependency:

```toml
[dependencies]
sem-core = { git = "https://github.com/Ataraxy-Labs/sem", version = "0.3" }
```

Used by [weave](https://github.com/Ataraxy-Labs/weave) (semantic merge driver) and [inspect](https://github.com/Ataraxy-Labs/inspect) (entity-level code review).

## Architecture

- **tree-sitter** for code parsing (native Rust, not WASM)
- **git2** for Git operations
- **rayon** for parallel file processing
- **xxhash** for structural hashing
- Plugin system for adding new languages and formats

## License

MIT OR Apache-2.0
