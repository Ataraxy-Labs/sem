<p align="center">
  <img src="assets/banner.svg" alt="sem" width="600" />
</p>

<p align="center">
  Instead of lines changed, sem tells you what entities changed: functions, methods, classes.
</p>

<p align="center">
  <a href="https://github.com/Ataraxy-Labs/sem/releases/latest"><img src="https://img.shields.io/github/v/release/Ataraxy-Labs/sem?color=blue&label=release" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-stable-orange" alt="Rust">
  <img src="https://img.shields.io/badge/tests-46_passing-brightgreen" alt="Tests">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-yellow" alt="License"></a>
  <img src="https://img.shields.io/badge/languages-20-blue" alt="Languages">
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
git clone https://github.com/antiartificial/sem
cd sem/crates
cargo install --path sem-cli
```

Or grab a binary from [GitHub Releases](https://github.com/antiartificial/sem/releases).

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

## Entity history

Track the full history of any function, class, or entity across your git history — following it through renames and moves.

```bash
sem log --entity authenticateUser

  src/auth/login.ts :: function :: authenticateUser

  a1b2c3d  2024-01-15  alice         [added]              add auth module
  d4e5f6a  2024-02-01  bob           [modified]           update session handling
  e7f8a9b  2024-03-10  carol         [signature changed]  parameters changed
  f0a1b2c  2024-04-05  bob           [renamed]            was: authenticate
  c3d4e5f  2024-05-20  alice         [modified]           formatting only

  5 events — 3 authors — 2024-01-15 to 2024-05-20
```

Most git tools lose track of an entity when it gets renamed or moved to another file. `sem log` follows it backward through the history automatically, so you get the full story in one view.

It also tells you whether a change touched the function signature (parameters added, removed, return type changed) or just the body. If someone only reformatted the code, it says so.

```bash
# If the name is ambiguous, sem tells you and suggests how to narrow it
sem log --entity login
# error: 'login' is ambiguous. Did you mean one of:
#   1. src/auth/login.ts :: function :: login
#   2. src/api/routes.ts :: function :: login
# Use --file <path> to disambiguate.

# Scope to a specific file
sem log --entity login --file src/auth/login.ts

# Limit to a commit range
sem log --entity login --from v1.0.0 --to v2.0.0

# JSON output
sem log --entity login --format json
```

## Semantic review

Get a structured review of any set of changes, grouped by what actually matters: API surface changes that affect callers, internal implementation details, and config/data updates.

```bash
sem review --from main --to HEAD

┌─ API Surface Changes ───────────────────────────────
│  ⊕ function  validateToken          [added]
│    0 dependents (new)
│  ∆ function  authenticateUser       [signature changed]
│    ~12 dependents across 4 files
│  ⊖ function  legacyAuth             [deleted]
│    ↳ was called by: loginHandler, refreshToken, adminAuth
└──────────────────────────────────────────────────────

┌─ Internal Changes ──────────────────────────────────
│  ∆ function  hashPassword           [body only]
│  ⊕ function  buildSessionKey        [added]
└──────────────────────────────────────────────────────

┌─ Config / Data Changes ─────────────────────────────
│  ∆ property  production.pool_size   [5 → 20]
└──────────────────────────────────────────────────────

Summary: 3 API surface, 2 internal, 1 config
Risk: high (breaking API change: deleted entity with dependents)
```

For each API surface change, it shows how many other entities depend on it. If you delete something that was referenced elsewhere, it flags exactly who was calling it. Risk is based on the actual dependency graph.

```bash
# Review staged changes before committing
sem review --staged

# Review a specific commit
sem review --commit abc1234

# JSON for CI integration
sem review --format json
```

## Changelog generation

Generate a changelog from a commit range, automatically categorized with a semver bump suggestion.

```bash
sem changelog --from v1.2.0 --to HEAD

## Unreleased — 2024-06-15

### Breaking Changes
  ! authenticateUser signature changed (parameters removed)

### Added
  + function validateToken in src/auth/login.ts

### Changed
  ~ function hashPassword body modified in src/auth/crypto.ts

### Removed
  - function legacyAuth deleted from src/auth/login.ts

Suggested version bump: MAJOR (breaking API change: deleted entity with dependents)
```

It uses the dependency graph to figure out whether a change is API-facing or internal, and suggests a semver bump based on what actually changed in the code — not just the commit message.

```bash
# Markdown for release notes
sem changelog --from v1.0.0 --to v2.0.0 --format markdown --heading "v2.0.0"

# JSON for automation
sem changelog --format json
```

## What it parses

20 programming languages with full entity extraction via tree-sitter:

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
| Swift | `.swift` | functions, classes, protocols, structs, enums, properties |
| Elixir | `.ex` `.exs` | modules, functions, macros, guards, protocols |
| Bash | `.sh` | functions |
| HCL/Terraform | `.hcl` `.tf` `.tfvars` | blocks, attributes (qualified names for nested blocks) |
| Kotlin | `.kt` `.kts` | classes, interfaces, objects, functions, properties, companion objects |
| Fortran | `.f90` `.f95` `.f` | functions, subroutines, modules, programs |
| Vue | `.vue` | template/script/style blocks + inner TS/JS entities |
| XML | `.xml` `.plist` `.svg` `.csproj` | elements (nested, tag-name identity) |

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
