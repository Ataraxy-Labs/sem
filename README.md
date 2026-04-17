<p align="center">
  <img src="assets/banner.svg" alt="sem" width="600" />
</p>

<p align="center">
  <strong>Semantic version control built on Git.</strong><br>
  Instead of lines changed, sem tells you what entities changed: functions, methods, classes.
</p>

<p align="center">
  <a href="https://ataraxy-labs.com/blogs/code-is-not-text">Why sem?</a> ·
  <a href="#install">Install</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#mcp-server">MCP Server</a> ·
  <a href="https://github.com/Ataraxy-Labs/sem/releases/latest">Releases</a>
</p>

<p align="center">
  <a href="https://github.com/Ataraxy-Labs/sem/releases/latest"><img src="https://img.shields.io/github/v/release/Ataraxy-Labs/sem?color=blue&label=release" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-stable-orange" alt="Rust">
  <img src="https://img.shields.io/badge/tests-133_passing-brightgreen" alt="Tests">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-yellow" alt="License"></a>
  <img src="https://img.shields.io/badge/languages-26-blue" alt="Languages">
</p>

sem is a semantic version control tool that works on top of Git. It parses your code with tree-sitter, extracts every function, class, and method as an entity, and diffs at the entity level instead of lines. This means you see "function `blahh` was modified" instead of "lines x-y changed."

It works in any Git repo with no setup.

<p align="center">
  <img src="assets/terminal.svg" alt="sem diff" width="800" />
</p>

## Install

```bash
brew install sem-cli
```

Or install the npm wrapper into `node_modules`:

```bash
npm install --save-dev @ataraxy-labs/sem
```

With Bun, trust the package so its `postinstall` script can download the binary:

```bash
bun add -d @ataraxy-labs/sem
bun pm trust @ataraxy-labs/sem
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

## Commands

Works in any Git repo. No setup required. Also works outside Git for arbitrary file comparison.

### sem diff

Entity-level diff with rename detection, structural hashing, and word-level inline highlights.

```bash
# Semantic diff of working changes
sem diff

# Staged changes only
sem diff --staged

# Specific commit
sem diff --commit abc1234

# Commit range
sem diff --from HEAD~5 --to HEAD

# Verbose mode (word-level inline diffs for each entity)
sem diff -v

# Plain text output (git status style)
sem diff --format plain

# JSON output (for AI agents, CI pipelines)
sem diff --format json

# Markdown output (for PRs, reports)
sem diff --format markdown

# Compare any two files (no git repo needed)
sem diff file1.ts file2.ts

# Read file changes from stdin (no git repo needed)
echo '[{"filePath":"src/main.rs","status":"modified","beforeContent":"...","afterContent":"..."}]' \
  | sem diff --stdin --format json

# Only specific file types
sem diff --file-exts .py .rs
```

### sem impact

Cross-file dependency graph shows what breaks if an entity changes.

```bash
# Full impact analysis
sem impact authenticateUser

# Direct dependencies only
sem impact authenticateUser --deps

# Direct dependents only
sem impact authenticateUser --dependents

# Affected tests only
sem impact authenticateUser --tests

# JSON output
sem impact authenticateUser --json

# Disambiguate by file
sem impact authenticateUser --file src/auth.ts
```

### sem blame

Entity-level blame showing who last modified each function, class, or method.

```bash
sem blame src/auth.ts

# JSON output
sem blame src/auth.ts --json
```

### sem log

Track how a single entity evolved through git history.

```bash
sem log authenticateUser

# Verbose mode (show content diff between versions)
sem log authenticateUser -v

# Limit commits scanned
sem log authenticateUser --limit 20

# JSON output
sem log authenticateUser --json
```

### sem entities

List all entities in a file with their types and line ranges.

```bash
sem entities src/auth.ts

# JSON output
sem entities src/auth.ts --json
```

### sem context

Token-budgeted context for LLMs: the entity, its dependencies, and its dependents, fitted to a token budget.

```bash
sem context authenticateUser

# Custom token budget
sem context authenticateUser --budget 4000

# JSON output
sem context authenticateUser --json
```

## Use as default Git diff

Replace `git diff` output with entity-level diffs. Agents and humans get sem output automatically without changing any commands.

```bash
sem setup
```

Now `git diff` shows entity-level changes instead of line-level. No prompts, no agent configuration needed. Everything that calls `git diff` gets sem output automatically. Also installs a pre-commit hook that shows entity-level blast radius of staged changes.

To disable and go back to normal git diff:

```bash
sem unsetup
```

## What it parses

26 programming languages with full entity extraction via tree-sitter:

| Language | Extensions | Entities |
|----------|-----------|----------|
| TypeScript | `.ts` `.tsx` `.mts` `.cts`  | functions, classes, interfaces, types, enums, exports |
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
| Perl | `.pl` `.pm` `.t` | subroutines, packages |
| Dart | `.dart` | classes, mixins, extensions, enums, type aliases, functions |
| OCaml | `.ml` `.mli` | values, modules, types, classes, externals |
| Scala | `.scala` `.sc` `.sbt` | classes, objects, traits, enums, functions, vals, extensions |
| Zig | `.zig` | functions, tests, variables |

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

## MCP Server

sem includes an MCP server with 6 tools for AI agents: `sem_entities`, `sem_diff`, `sem_blame`, `sem_impact`, `sem_log`, `sem_context`. These mirror the CLI commands exactly.

```json
{
  "mcpServers": {
    "sem": {
      "command": "sem-mcp"
    }
  }
}
```

Install the MCP binary:

```bash
cd sem/crates
cargo install --path sem-mcp
```

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
