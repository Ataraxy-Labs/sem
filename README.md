<p align="center">
  <img src="assets/banner.svg" alt="sem" width="600" />
</p>

<p align="center">
  Instead of lines changed, sem tells you what entities changed: functions, methods, classes.
</p>

<p align="center">
  <a href="https://github.com/Ataraxy-Labs/sem/releases/latest"><img src="https://img.shields.io/github/v/release/Ataraxy-Labs/sem?color=blue&label=release" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-stable-orange" alt="Rust">
  <img src="https://img.shields.io/badge/tests-97_passing-brightgreen" alt="Tests">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-yellow" alt="License"></a>
  <img src="https://img.shields.io/badge/languages-21-blue" alt="Languages">
</p>

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
brew install sem-cli
```

Or build from source (requires Rust):

```bash
git clone https://github.com/Ataraxy-Labs/sem
cd sem/crates
cargo install --path sem-cli
```

Or grab a binary from [GitHub Releases](https://github.com/Ataraxy-Labs/sem/releases).

Or run via Docker:

```bash
docker build -t sem .
docker run --rm -it -u "$(id -u):$(id -g)" -v "$(pwd):/repo" sem diff
```

## Usage

Works in any Git repo. No setup required. Also works outside Git for arbitrary file comparison.

```bash
# Semantic diff of working changes
sem diff

# Staged changes only
sem diff --staged

# Specific commit
sem diff --commit abc1234

# Commit range
sem diff --from HEAD~5 --to HEAD

# Plain text output (git status style)
sem diff --format plain

# JSON output (for AI agents, CI pipelines)
sem diff --format json

# Compare any two files (no git repo needed)
sem diff file1.ts file2.ts

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

## Use as default Git diff

Replace `git diff` output with entity-level diffs. Agents and humans get sem output automatically without changing any commands.

```bash
# Set sem as your git diff tool
git config --global diff.external sem-diff-wrapper

# Create the wrapper script
echo '#!/bin/sh
sem diff "$2" "$5"' > ~/.local/bin/sem-diff-wrapper
chmod +x ~/.local/bin/sem-diff-wrapper
```

Now `git diff` shows entity-level changes instead of line-level. No prompts, no agent configuration needed. Everything that calls `git diff` gets sem output automatically.

To disable and go back to normal git diff:

```bash
git config --global --unset diff.external
```

## What it parses

21 programming languages with full entity extraction via tree-sitter:

| Language | Extensions | Entities |
|----------|-----------|----------|
| TypeScript | `.ts` `.tsx`, `.mts`, `.cts` | functions, classes, interfaces, types, enums, exports |
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
| Swift | `.swift` | functions, classes, protocols, structs, enums, properties |
| Elixir | `.ex` `.exs` | modules, functions, macros, guards, protocols |
| Bash | `.sh` | functions |
| HCL/Terraform | `.hcl` `.tf` `.tfvars` | blocks, attributes (qualified names for nested blocks) |
| Kotlin | `.kt` `.kts` | classes, interfaces, objects, functions, properties, companion objects |
| Fortran | `.f90` `.f95` `.f` | functions, subroutines, modules, programs |
| Vue | `.vue` | template/script/style blocks + inner TS/JS entities |
| XML | `.xml` `.plist` `.svg` `.csproj` | elements (nested, tag-name identity) |
| ERB | `.erb` `.html.erb` | blocks, expressions, code tags |
| Svelte | `.svelte` `.svelte.js` `.svelte.ts` | component blocks + rune JS/TS modules |

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

## Contributing

Want to add a new language? See [CONTRIBUTING.md](CONTRIBUTING.md) for a step-by-step guide.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=Ataraxy-Labs/sem&type=Date)](https://star-history.com/#Ataraxy-Labs/sem&Date)

## License

MIT OR Apache-2.0
