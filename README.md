> **Part of the [Ataraxy Labs](https://ataraxy-labs.com) stack** — agent-native infrastructure for software development. See also: [weave](https://ataraxy-labs.com/weave) (entity-level git merge driver) · [inspect](https://github.com/Ataraxy-Labs/inspect) (semantic code review) · [opensessions](https://github.com/Ataraxy-Labs/opensessions) (tmux sidebar for coding agents).
>
> Read the manifesto: https://ataraxy-labs.com/#thesis · Essays: https://ataraxy-labs.com/blogs · LLMs: https://ataraxy-labs.com/llms.txt

<p align="center">
  <img src="assets/banner.svg" alt="sem" width="600" />
</p>

<p align="center">
  <a href="https://trendshift.io/repositories/25348" target="_blank"><img src="https://trendshift.io/api/badge/repositories/25348" alt="Ataraxy-Labs%2Fsem | Trendshift" style="width: 250px; height: 55px;" width="250" height="55"/></a>
</p>

<p align="center">
  <strong>Semantic version control built on Git.</strong><br>
  Instead of lines changed, sem tells you what entities changed: functions, methods, classes.
</p>

<p align="center">
  <a href="https://ataraxy-labs.com/blogs/code-is-not-text">Why sem?</a> ·
  <a href="#install">Install</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#use-with-ai-agents-mcp">Agents (MCP)</a> ·
  <a href="https://github.com/Ataraxy-Labs/sem/releases/latest">Releases</a>
</p>

<p align="center">
  <a href="https://github.com/Ataraxy-Labs/sem/releases/latest"><img src="https://img.shields.io/github/v/release/Ataraxy-Labs/sem?color=blue&label=release" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-stable-orange" alt="Rust">
  <img src="https://img.shields.io/badge/tests-133_passing-brightgreen" alt="Tests">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-yellow" alt="License"></a>
  <img src="https://img.shields.io/badge/languages-31-blue" alt="Languages">
</p>

sem is a semantic version control tool that works on top of Git. It parses your code with tree-sitter, extracts every function, class, and method as an entity, and diffs at the entity level instead of lines. This means you see "function `blahh` was modified" instead of "lines x-y changed."

It works in any Git repo with no setup.

<p align="center">
  <img src="assets/terminal.svg" alt="sem diff" width="800" />
</p>

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Ataraxy-Labs/sem/main/install.sh | sh
```

Or via Homebrew:

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

Once installed, update to the latest release any time:

```bash
sem update
```

Or build from source (requires Rust):

```bash
cargo install --git https://github.com/Ataraxy-Labs/sem sem-cli
```

Or grab a binary from [GitHub Releases](https://github.com/Ataraxy-Labs/sem/releases).

Or run via Docker:

```bash
docker build -t sem .
docker run --rm -it -u "$(id -u):$(id -g)" -v "$(pwd):/repo" sem diff
```

## Name conflict with GNU Parallel

GNU Parallel ships a `sem` binary (`/usr/bin/sem`) as a symlink to `parallel`. If you have both installed, they'll collide. Run `sem --version` to check which one you're using. ([#77](https://github.com/Ataraxy-Labs/sem/issues/77))

**Quick fixes:**

```bash
# Option 1: alias in your shell profile (~/.bashrc, ~/.zshrc)
alias sem="$HOME/.cargo/bin/sem"

# Option 2: make sure cargo bin comes first in PATH
export PATH="$HOME/.cargo/bin:$PATH"

# Option 3: if installed via Homebrew
export PATH="$(brew --prefix)/bin:$PATH"
```

If you installed via npm/bun, the binary lives in `node_modules/.bin/sem` and is invoked through `npx sem` or `bunx sem`, which avoids the conflict entirely.

## Commands

Works in any Git repo. No setup required. Also works outside Git for arbitrary file comparison.

sem stores its SQLite entity cache outside the repository, under the OS cache directory by default. Set `SEM_CACHE_DIR=/path/to/cache` to override the cache root; repo-local overrides are ignored so cache files do not dirty the working tree.

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

# Include default-excluded paths such as generated, fixture, vendor, benchmark, and build trees
sem impact authenticateUser --no-default-excludes
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

List all entities under a file or directory path. No path is the same as `.`.

```bash
sem entities

sem entities .

sem entities src/auth.ts

# JSON output
sem entities --json
sem entities src/auth.ts --json

# Include default-excluded paths such as generated, fixture, vendor, benchmark, and build trees
sem entities --no-default-excludes
```

### sem context

Token-budgeted context for LLMs: the entity, its dependencies, and its dependents, fitted to a strict content token budget.
When the target signature itself does not fit, JSON output reports `target_omitted: true`.

```bash
sem context authenticateUser

# Custom token budget
sem context authenticateUser --budget 4000

# JSON output
sem context authenticateUser --json

# Include default-excluded paths such as generated, fixture, vendor, benchmark, and build trees
sem context authenticateUser --no-default-excludes
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

31 programming languages with full entity extraction via tree-sitter:

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
| Nix | `.nix` | bindings, inherit declarations |
| Haskell | `.hs` | functions, signatures, data types, newtypes, classes, instances, type synonyms |
| Elm | `.elm` | value declarations, type aliases, type declarations, port annotations, infix declarations |
| Clojure | `.clj` `.cljs` `.cljc` | vars, functions, macros, multimethods, protocols, records, types |
| D | `.d` `.di` | modules, functions, classes, structs, interfaces, unions, enums, templates, aliases, unittests |
| Zig | `.zig` | functions, tests, variables |
| SQL | `.sql` `.psql` `.pgsql` `.ddl` | tables, views, functions, indexes, types, schemas, triggers, sequences |

Plus structured data formats:

| Format | Extensions | Entities |
|--------|-----------|----------|
| JSON | `.json` | properties, objects (RFC 6901 paths) |
| YAML | `.yml` `.yaml` | sections, properties (dot paths) |
| TOML | `.toml` | sections, properties |
| EDN | `.edn` | top-level map entries (keyword keys) |
| CSV | `.csv` `.tsv` | rows (first column as identity) |
| Markdown | `.md` `.mdx` | heading-based sections |

Everything else falls back to chunk-based diffing.

### Custom extensions and extensionless files

For files with non-standard extensions, create a `.semrc` in your project root:

```
.xyz = cpp
.j = json
.mypy = python
```

sem also reads `.gitattributes` patterns (`diff=` and `linguist-language=`) if you already have those set up. `.semrc` takes priority when both define the same extension.

For files with no extension at all, sem detects the language automatically from content (imports, declarations, shebang lines, vim modelines). This covers 19 languages with no config needed.

## How matching works

Three-phase entity matching:

1. **Exact ID match** — same entity in before/after = modified or unchanged
2. **Structural hash match** — same AST structure, different name = renamed or moved (ignores whitespace/comments)
3. **Fuzzy similarity** — >80% token overlap = probable rename

This means sem detects renames and moves, not just additions and deletions. Structural hashing also distinguishes cosmetic changes (whitespace, formatting) from real logic changes.

## Use with AI agents (MCP)

`sem mcp` starts a [Model Context Protocol](https://modelcontextprotocol.io) server over stdin/stdout. It's not a command you run and read yourself: it's a server your coding agent launches in the background so it can ask sem questions while it works. That's the reason `mcp` lives alongside the normal commands. The agent gets 6 tools, all entity-level: `sem_impact`, `sem_context`, `sem_diff`, `sem_entities`, `sem_blame`, `sem_log`.

Why an agent wants these: instead of reading whole files and burning tokens, it can ask "what breaks if I change `submitOrder`" (`sem_impact`) or "give me just the context to refactor this function" (`sem_context`) and get a precise answer from the dependency graph.

Add it once, then talk to your agent normally. It calls the tools on its own.

**Claude Code:**

```bash
claude mcp add sem -- sem mcp
```

**Cursor, Claude Desktop, or any client with an `mcpServers` config:**

```json
{
  "mcpServers": {
    "sem": {
      "command": "sem",
      "args": ["mcp"]
    }
  }
}
```

If `sem` isn't on the agent's PATH, use the absolute path to the binary. No separate install is needed: `sem mcp` ships in the same binary as every other command.

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
    "moved": 0,
    "renamed": 0,
    "reordered": 0,
    "binary": 0,
    "orphan": 0,
    "total": 3
  },
  "changes": [
    {
      "entityId": "src/auth.ts::function::validateToken",
      "changeType": "added",
      "entityType": "function",
      "entityName": "validateToken",
      "startLine": 12,
      "endLine": 18,
      "oldStartLine": null,
      "oldEndLine": null,
      "filePath": "src/auth.ts"
    }
  ],
  "binaryChanges": []
}
```

The named change-type buckets (`added`, `modified`, `deleted`, `moved`, `renamed`, `reordered`) always sum to `total`. `orphan` is a cross-cutting metadata count for module-level changes, and those changes are already included in the named change-type buckets.

## As a library

sem-core can be used as a Rust library dependency:

```toml
[dependencies]
sem-core = { git = "https://github.com/Ataraxy-Labs/sem", version = "0.5" }
```

Used by [weave](https://github.com/Ataraxy-Labs/weave) (semantic merge driver) and [inspect](https://github.com/Ataraxy-Labs/inspect) (entity-level code review).

## Architecture

- **tree-sitter** for code parsing (native Rust, not WASM)
- **git2** for Git operations
- **rayon** for parallel file processing
- **xxhash** for structural hashing
- Plugin system for adding new languages and formats

## Telemetry

sem collects anonymous usage data: the command name (e.g. `diff`, `impact`), CLI version, and operating system. Nothing else — no code, file paths, repo names, or user identity. Events are batched locally and sent in the background, so commands never wait on the network.

Disable it any time:

```bash
export SEM_NO_TELEMETRY=1   # or DO_NOT_TRACK=1
```

## Contributing

Want to add a new language? See [CONTRIBUTING.md](CONTRIBUTING.md) for a step-by-step guide.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=Ataraxy-Labs/sem&type=Date)](https://star-history.com/#Ataraxy-Labs/sem&Date)

## License

MIT OR Apache-2.0
