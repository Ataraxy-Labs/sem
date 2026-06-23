# Changelog

All notable changes to sem are documented in this file.

## [Unreleased]

### Added

- `sem orient <query>` finds the entities most relevant to a query, structural code search for when you're dropped into an unfamiliar codebase and don't know the symbol name yet (e.g. `sem orient "where is the retry logic"`). Two-pass ranking: lexical score over entity name (subtoken + prefix/stem + substring), file path, and signature line, then a graph-centrality re-rank so a central, widely-used entity outranks a trivially-named helper. Results show the entity, its `file:line`, signature, and dependent count. `--json` and `--limit` supported. This is the structural counterpart to grep: grep finds text, orient finds the entity and how connected it is.
- The `sem_entities` MCP tool accepts a `query` parameter for the same intent search, so agents can find code by what it does (not just by name) without falling back to grep. The ranking is shared with the CLI (`sem_core::parser::orient`).
- `sem entities` accepts `--only <kind>` and `--except <kind>` (both repeatable) to filter the listing by entity kind, e.g. `sem entities --only function --only struct` or `sem entities --except import`. The two flags are mutually exclusive. Because entity kinds are language-dependent, an unknown kind reports the kinds actually found in the scanned files rather than guessing a static list. Thanks @aleclarson for the request (#378).

## [0.13.1] - 2026-06-23

### Added

- `sem impact` can answer direct dependency queries from a fresh SQL topology cache without rebuilding the entity graph.
- `sem entities` reports phase timings and listing counters when `SEM_TIMINGS` is enabled.
- Optional OSC8 terminal hyperlinks on entity names in `sem diff`, so a supporting terminal (kitty, WezTerm, iTerm2, Ghostty, ...) renders them clickable and can open the definition at `file:line`. Off by default; enable with `SEM_HYPERLINK` set to an editor preset (`vscode`, `cursor`, `windsurf`, `zed`, `idea`, `file`) or a raw URI template using `{file}` and `{line}` (e.g. `SEM_HYPERLINK="vscode://file/{file}:{line}"`). Strictly TTY-only, so pipes, JSON output, and MCP/agent sessions never see escape codes. Force off with `SEM_NO_HYPERLINKS=1`. Thanks @olejorgenb for the request (#381).

### Changed

- The `sem mcp` server now sends usage guidance to the agent instead of a bare tool list. The instructions tell the agent to prefer `sem_impact`/`sem_context`/`sem_entities` over grep/find for structural questions (what calls X, understand X, where is X) and to keep grep for text search and non-code files. Availability alone wasn't changing agent behavior; this biases agents toward the entity graph the moment the server connects, with no extra setup.
- `sem impact --deps` can reuse fresh caches when unrelated files change by validating the cached source set, hashes, and import metadata before falling back to a graph rebuild.
- `sem impact --deps` narrows cache freshness checks to the queried entity, direct dependencies, and relevant JavaScript/TypeScript imports when the query scope is explicit.
- Source scans skip default-excluded high-volume paths such as generated source directories, fixture/vendor/benchmark trees, generated file suffixes, CSS module declarations, and asset declarations; pass `--no-default-excludes` to include them.
- `sem entities` accepts `--file-exts` for large directory scans and avoids duplicate directory-listing post-processing.
- `sem entities` can list entities from a fresh SQLite topology cache instead of reparsing matching directory scans.
- `sem entities --json` streams rows to stdout instead of materializing an intermediate JSON value array.
- `sem entities` uses listing-only extraction so local listings do not retain source text or entity hashes.

### Fixed

- Intel macOS (`x86_64-apple-darwin`) is now built and published. The release matrix only produced Apple Silicon (`arm64`) macOS binaries, so Intel Mac users got a 404 from `install.sh` and "Unsupported platform darwin:x64" from npm. Added the `x86_64-apple-darwin` target to the release build and the `darwin:x64` mapping to the npm wrapper. Thanks @stark-bit for the report (#374).
- TOML array-of-tables entries no longer collapse to a single entity in `sem diff`. Repeated `[[array]]` headers all reduced to the same id (`...::property::array`), so appending an entry showed up as a modification of the previous one instead of an addition. Each `[[key]]` entry now gets an index-based identity (`key/0`, `key/1`, ...) and is hashed independently, mirroring the JSON array-index handling. This also stops a `[key]` table and a `[[key]]` array-of-tables with the same name from colliding. Thanks @Arpafaucon for the report and analysis (#362).

## [0.13.0] - 2026-06-16

### Fixed

- Kotlin: resolve method calls through typed receivers that the `tree-sitter-kotlin-ng` grammar exposes positionally (no `name`/`type` fields). Several scope-resolution paths used field names from the older grammar and silently produced no call edges. Fixed: typed function parameters (`fun f(s: Scenario) { s.method() }`); class field types from property declarations (`val conn: Connection`) and primary-constructor properties (`class Tx(val conn: Connection)`); chained field access (`val s = container.scenario; s.method()`); and declared/inferred return types, so `val c = get(); c.method()` resolves. `sem context`/`impact`/`log` now find these Kotlin callers. Thanks @mrsirrisrm.
- Java: name field entities by their declarator instead of their type. `private FooService fooService;` was extracted as an entity named `FooService` (its type) rather than `fooService`, because `field_declaration` has no `name` field and the generic fallback returned the first type identifier. This also collided class and field names in the symbol table. `sem entities`/`diff`/`log` now report the correct field name. Thanks @mrsirrisrm.
- Java: resolve cross-file `receiver.method()` call edges. Local variable types weren't recorded (`Dog d = new Dog()` and `object_creation_expression` RHS were ignored), class field types weren't tracked, and `ClassName.staticMethod()` calls were dropped, so `sem impact`/`context` reported few or no cross-file dependents on Java, a false negative that read as "safe to change." The Spring field-injection pattern (`@Inject private FooService foo; ... foo.bar()`) now resolves. Thanks @mrsirrisrm.
- On Windows, the MCP tools `sem_impact`, `sem_context`, and `sem_log` never resolved an entity: `resolve_file_path` returned OS-native (backslash) relative paths while graph entities store forward-slash `file_path`s, so the `(name, file_path)` match always failed. Relative paths are now emitted with forward slashes on all platforms. Thanks @Turntwo.

## [0.12.0] - 2026-06-15

### Added

- While the spinner is up, sem shows a rotating one-line tip about another useful command underneath it, like the hints under Claude Code's spinner. `sem diff` (the most-used command) now shows the spinner during its compute, so you learn about `sem impact`, `sem context`, `sem blame`, the MCP server, etc. while you wait. Strictly stderr and TTY-only, so it never touches output, pipes, JSON, or agent sessions, and disappears when the work finishes. Disable with `SEM_NO_PROGRESS=1`.
- After a slow local build (3s+) when you're logged out, sem prints one dim line with the time you just spent and notes that sem cloud serves the same graph warm in milliseconds. Throttled to once a day, TTY-only, and uses your real elapsed time (no inflated claims). Disable with `SEM_NO_PROGRESS=1`.
- The MCP server (`sem mcp`) now keeps its in-memory entity graph live with a background file watcher. Previously `sem_impact` and `sem_context` re-walked and re-stat'd the entire repo on every call just to check whether the cached graph was still fresh, which on a large repo is real per-call overhead. A watcher now tracks filesystem changes, so calls where nothing changed return the cached graph instantly (no walk, no stat), and an edit triggers an incremental rebuild of only the changed files. Your uncommitted edits are reflected without restarting the server. Disable with `SEM_NO_WATCH=1`.

### Changed

- Graph resolution now uses faster hash collections in hot paths to reduce graph build overhead.
- Scope resolution caches repeated reference lookups during graph builds to reduce redundant resolver work.
- Graph builds avoid retaining import scan source text after import extraction, reducing peak memory use.
- `sem context` now prints the full source of the target entity in the terminal. It previously showed only the first line, so reading a function meant falling back to `--json`. Related entities still show a one-line signature so the context map stays scannable.
- `sem context` and `sem impact` (CLI and the `sem_context` / `sem_impact` MCP tools) now accept `Class.method` (and `Outer.Inner.method`) to address a method by name, not only the bare method name or a full entity id.

### Fixed

- Fixed: `super::module::func()` calls were dropped from the entity graph, so `impact` and `context` under-reported the blast radius across modules. Multi-segment Rust path-prefixed calls (`super::`/`crate::`/`self::`) now resolve to the real entity.

## [0.11.1] - 2026-06-14

### Added

- `sem impact` now shows the uv-style progress spinner during the cold graph build (it's the most-used graph command). Same stderr/TTY gating as `graph` and `context`.

## [0.11.0] - 2026-06-14

### Added

- Progress spinner for slow graph builds. `sem graph` and `sem context` now show a uv-style spinner and a summary line (e.g. `135,298 entities, 7,743 files in 6.6s`) while building the entity graph. Strictly stderr and TTY-only, so pipes, JSON, and agent/MCP sessions are unaffected. Disable with SEM_NO_PROGRESS=1.

- SQL support (`.sql`, `.psql`, `.pgsql`, `.ddl`) via the official DerekStride/tree-sitter-sql grammar. Extracts tables, views, materialized views, functions, indexes, types, schemas, triggers, sequences, and databases. Thanks @robahtou for the request (#339).
- Start tracking project changes in `CHANGELOG.md`.
- Add a pull request check that asks contributors to include a changelog entry.
- `sem entities` accepts multiple file or directory path arguments.

### Changed

- Sparse checkouts now work. libgit2 cannot read a sparse index (`unsupported mandatory extension: 'sdir'`) and its workdir diff reported sparse-excluded files as deleted; sem now routes working and staged diffs through the git CLI when a sparse checkout is detected. Thanks for the report (#330).

- README now documents adding the MCP server to coding agents (`claude mcp add sem -- sem mcp`) and explains why `sem mcp` exists. The old section pointed at a separate `sem-mcp` binary; `sem mcp` ships in the main binary.

- `sem stats` now counts every diff, including runs that find no changes (previously those returned early and were never recorded, so `diffs performed` undercounted).

- Telemetry no longer records development builds (debug builds, or binaries run from a Cargo `target/` directory), so contributor and CI-of-our-own usage stays out of the numbers.

- Cloud sync only auto-registers repos that GitHub confirms are public. Private repos run locally unless you opt in with `SEM_SYNC_PRIVATE=1`.
- `install.sh` verifies the release archive against `checksums.txt` before installing.
- Switched the Perl grammar to the official `ts-parser-perl` crate (was the unattributed `tree-sitter-perl-next` copy). Properly attributed, correctly MIT-licensed, and includes upstream fixes: an infinite-loop hang on malformed input, better error recovery, and faster parsing. Thanks @rabbiveesh for the report (#355).

### Removed

- `sem verify` (function call-arity checker). It saw negligible use and overlapped with compilers/LSPs; removing it keeps the surface area focused.
