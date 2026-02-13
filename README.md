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

```bash
npm install -g @ataraxy-labs/sem
```

Or run directly:

```bash
npx @ataraxy-labs/sem diff
```

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

# Only include specific languages
sem diff --file-exts .py .rs
sem graph --file-exts .py

# Entity dependency graph
sem graph

# Impact analysis (what breaks if this entity changes?)
sem impact validateToken

# Entity-level blame
sem blame src/auth.ts
```

## What it parses

| Format | Extensions | Entities |
|--------|-----------|----------|
| TypeScript | `.ts` `.tsx` | functions, classes, interfaces, types, enums |
| JavaScript | `.js` `.jsx` `.mjs` `.cjs` | functions, classes, variables |
| Python | `.py` | functions, classes, decorated definitions |
| Go | `.go` | functions, methods, types, vars, consts |
| Rust | `.rs` | functions, structs, enums, impls, traits, mods |
| JSON | `.json` | properties, objects (RFC 6901 paths) |
| YAML | `.yml` `.yaml` | sections, properties (dot paths) |
| TOML | `.toml` | sections, properties |
| CSV | `.csv` `.tsv` | rows (first column as identity) |
| Markdown | `.md` `.mdx` | heading-based sections |

Everything else falls back to chunk-based diffing.

## How matching works

Three-phase entity matching:

1. **Exact ID match** — same entity in before/after → modified or unchanged
2. **Content hash match** — same content, different name → renamed or moved
3. **Fuzzy similarity** — >80% token overlap → probable rename

This means `sem` detects renames and moves, not just additions and deletions.

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

## SQL queries

```bash
sem init
sem log --store
sem query "SELECT change_type, count(*) as n FROM changes GROUP BY change_type"
```

```
change_type          │ n
─────────────────────────────────
added                │ 29
deleted              │ 2
modified             │ 7
```

## Architecture

- **tree-sitter** (native) for code parsing — not WASM
- **better-sqlite3** for storage — WAL mode, fast transactions
- **simple-git** for Git operations
- Plugin system — add your own parsers

## License

MIT
