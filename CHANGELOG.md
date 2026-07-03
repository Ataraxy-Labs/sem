# Changelog

All notable changes to sem are documented in this file.

## [Unreleased]

## [0.18.0] - 2026-07-03

### Fixed

- **sem now works on repos using git's reftable ref storage** (`git init --ref-format=reftable`, git 2.45+). Previously every command died with libgit2's cryptic `unsupported extension name extensions.refstorage`. libgit2 can't read reftable refs, but the object database and index are unchanged, so GitBridge now tolerates the extension and routes just the ref resolutions (`HEAD`, refspecs, revwalk starts) through the git CLI while libgit2 keeps doing everything else by OID. Verified end to end on a real reftable repo: working/staged/commit/range diffs, blame, and per-file history all produce identical results to a files-backend repo. One residual gap: the cache freshness oracle's direct `git2::Repository::open` is `.ok()`-guarded, so on reftable repos it just skips the acceleration (correctness unaffected). Requires `git` on PATH for the ref lookups. Thanks @bengry for the report and clean repro (#451).

### Added

- **Unique-method-name call edges (dynamic languages).** Attribute calls on receivers of unknown type (`index.keep_levels(...)`) previously produced no graph edge, hiding real dependents and blinding `--tests`. In Python/Ruby — where receiver types are statically unknowable — a method name with exactly one definition repo-wide now resolves to it: one candidate, one edge; any ambiguity, no edge. Static languages keep precision-first resolution (an unresolved receiver there is deliberate: shadowed import, instance property).
- **`Parent::child` entity qualifiers.** `sem impact "Dataset::set_index"` and friends now work everywhere `Parent.child` does (graph and cached lookups).

- **`sem impact --tests` lexical fallback + fast-path fallthrough.** Graph edges miss tests that call a target through a module namespace (`xr.where(...)` resolves to no entity), so `--tests` could answer "No tests found" for a function with dozens of tests. Now: an empty tests answer from the sidecar or disk cache is treated as non-authoritative and falls through to the full path, which backstops zero graph edges with lexical reachability — test entities naming the target as a whole word — clearly labeled as weaker evidence. Found live: an agent's graph-selected verify loop (run only the tests that reach your change) went from 0 selected tests to a 56-test net on `xr.where`, versus the 1,900+ tests of whole-file runs.
- **Sharper `--pack` briefings.** Term ranking is now IDF-weighted (a term appearing in half the repo is worth almost nothing), `<details>` environment dumps in issue text are stripped before extraction, attribute accesses glued to receivers ("d2.loc") also emit their `.attr` suffixes as terms, and one of the three briefing slots goes to the top name-echo orient hit — for bugs where the issue names a surface API the culprit's body never mentions.

- **`sem orient --pack <tokens>`: turn-zero briefings from task text.** Feed orient a whole issue or task description and it returns a packed briefing — the top matching functions' bodies plus their immediate callers/callees — sized to the token budget. Ranking is body-term convergence: code-ish terms are extracted from the task text (flags, dotted names, identifiers) and entities are ranked by how many distinct terms their bodies contain, since issue vocabulary lives in bodies, not names. Built for prompt-time injection (the agent-side analog of the prompt-submit prefetch hook): the code an agent would spend its first turns foraging for arrives at turn zero. Honest calibration: on three ground-truth issues it put the exact target function first on two; issues that quote the tool's own output can still poison term extraction.

- **sem is published to the official MCP registry** as `io.github.Ataraxy-Labs/sem`, so MCP clients that browse the registry (VS Code, Cursor, Claude Code, and others) can discover and install the server directly. The release workflow now publishes each release to the registry via `mcp-publisher` (authenticated with GitHub OIDC, no extra secrets), backed by a `server.json` manifest and an `mcpName` field in the npm wrapper.

### Performance

- **Delta-fills: changed entities answer with a diff against the version your session saw.** The attention ledger now stores fill contents, so when a session re-asks about an entity that changed since its last look, the answer is an entity-level delta (`∆ alpha · changed since you read it … - x = 1 / + x = 42`) instead of the whole packed body. Measured end to end: a post-edit re-ask that previously re-sent the body now costs 2 diff lines. Completes the ledger's answer set — new entity: full fill; unchanged: one line; changed: delta; `SEM_FRESH=1` / `fresh: true` always forces the full re-pack. Deltas larger than 120 lines fall back to a full fill.

- **Attention ledger covers the MCP path.** `sem_context` (the tool agent sessions actually call) now runs through the same per-session fill ledger: an MCP server process serves exactly one session, so re-asks for unchanged entities collapse to one `≡ unchanged since you read it` line automatically — no environment variable needed. New optional `fresh: true` param forces a full re-send (for when context compaction dropped the earlier fill).

- **Attention ledger v1: repeated context fills collapse to one line.** The resident server now keeps a per-session ledger of every `context` fill it has emitted (entity id + content fingerprint). When the same session re-asks for an unchanged entity, the answer is a single `≡ unchanged since you read it` line instead of the full packed body — the body is already sitting in the asking model's context window, so re-sending it is pure token waste. Measured through the CLI socket path: 8,586 bytes first fill, 139 bytes on repeat (98.4% suppressed). Opt-in via `SEM_SESSION=<id>` in the environment; `SEM_FRESH=1` bypasses; anonymous calls are never suppressed. Any change to the target entity misses the fingerprint and re-sends in full. This is the first piece of the attention architecture (docs/attention-architecture.md): space (graph), time (commit index), attention (ledger).

- **`sem entities --text`**: entity-addressed text search from the CLI (the MCP tool already had it) — one line per hit (file, innermost entity, line, matched text) instead of whole bodies, served from the resident server's warm graph in milliseconds with a local-graph fallback. This is the token-cheap way for an agent to verify a call site or find a string: a body-level `sem context` costs hundreds of tokens where a text hit costs ~15.

- **Auto-resident server: every sem CLI query after the first answers in milliseconds, on any repo.** A socket miss now spawns `sem mcp --resident` (hidden plumbing) detached in the background: a server that holds the repo's graph warm and serves ONLY the per-repo unix socket, exiting on its own when idle for 30 minutes or when it loses the bind race to a live session. `sem context` and `sem orient` gain sidecar fast paths (impact already had one), so the full structural read loop runs against the warm graph: measured on a 30K-LOC repo, first `sem context` 0.68s cold (spawning the resident), then context/orient/impact all under 10ms. In a controlled agent benchmark (6 verified code-understanding questions, classic grep/read agent vs sem agent, identical prompts and batching guidance), the sem agent answered in 13s vs 37s with equal 6/6 correctness — 65% faster. `SEM_NO_AUTOWARM=1` disables the auto-spawn, `SEM_NO_SIDECAR=1` the fast path.

- **Token-efficient tool output**: the same answers at a fraction of the tokens the consuming model has to read (and pay for). (1) The context packer stops enumerating noise: related test entities are folded into per-role counts instead of packed as one-line "#[test]" stubs (unless the target itself is a test, when its test neighborhood is the question), bare attribute/comment signatures are skipped, and transitive tiers are capped at 25 entries per role with the remainder counted. What was dropped is stated explicitly in one line ("not packed: +64 direct dependents (64 tests) · sem_impact lists them"), so the signal survives at a fraction of the cost. Measured: `sem context` on a hot sem-core entity 7,272 to 4,113 tokens (-43%, 119 to 48 entries); on a hot weave entity 5,216 to 3,498 (-33%, 146 to 27 entries, the 119 test stubs now one line). (2) The `sem_entities` MCP tool renders compact per-line trees (name · type · lines, children indented, files as group headers) instead of pretty-printed JSON: measured 3.7x fewer tokens on a 115-entity file (5,028 to 1,361). Applies to the MCP path and query modes; CLI `--json` output is unchanged for scripts.

- **Semantic commit index (storage engine layer 2)**: history is now stored as entity deltas. Each commit is semantic-diffed against its first parent exactly once and persisted as entity-change rows in the cache (`commits` + `entity_changes` tables, sha-keyed and branch-agnostic); every later history query is a SQLite lookup plus a diff of only the commits git gained since. Measured on the sem repo: `sem log` over 500 commits drops from 4.46s to **0.03s** on the second query (~150x), with the first query as the one-time indexing pass and each new commit costing one incremental diff. Applies to `sem log` repo analytics (hotspots + co-change pairs) and the MCP `sem_log` tool; per-entity traces are unchanged. Aggregation is shared code between the live git walk and the store (`aggregate_history_analytics`), tested to produce identical hotspot/co-change output, so the two paths cannot drift. Merge-heavy history gets better semantics: each commit is attributed its own first-parent diff instead of a diff against its arbitrary revwalk neighbor, and merge commits contribute no changes of their own (their first-parent diff restates the merged-in commits, which index individually). File-filtered queries index full-repo diffs once and filter at aggregation, so the first filtered query costs more than the old pathspec-scoped walk but every later query on any filter is instant. Cache schema v9; existing caches rebuild automatically on first use.

- **CLI sidecar fast path**: `sem impact` now answers from the resident `sem mcp` server's warm graph via its unix socket before doing any local work — measured **4.5ms** end-to-end on a 158K-LOC repo, versus 22.4ms for the local cold path and 7.7ms for a ripgrep scan of the same repo: the full blast radius (callers, dependencies, depth-bounded transitive impact, affected tests) is now cheaper than a raw text match. Output is byte-identical to the local path (the sidecar ships serialized `EntityInfo`s that the CLI feeds to its existing printers; verified across all modes and `--json`). The fast path is an accelerator, never a requirement: bounded socket timeouts and silent fallback mean no resident server (or `SEM_NO_SIDECAR=1`, `--no-cache`, custom scopes, `.semignore`, `--entity-id`) just runs the normal local path. Server-side, the new sidecar `impact` op classifies affected tests only among the entities the impact BFS actually reached, instead of walking the whole corpus per call (6.8ms → 0.1ms on a 4.7K-entity graph).

### Fixed

- Internal: rustfmt line-wrap missed in the #445 sidecar change; no behavior change.

- The context packer's token estimator was undercounting real tokens 2-3x on dense code (words x 1.3 vs the ~4 chars/token reality: a context reported as 661 tokens measured ~2,400 real tokens), silently overshooting every budget. It now takes the max of the word- and character-based estimates, so nominal budgets match what the consuming model actually pays.

- The context packer's "not packed" summary line pluralizes roles correctly ("transitive dependencies", not "dependencys").

- Workspace version bumped to 0.16.0: `ContextResult` gained the public `omitted` field (a breaking change for struct-literal constructors, flagged by cargo-semver-checks), and 0.x semantics put breaking changes in the minor version.

- The docs site deploys through workflow-based GitHub Pages (`.github/workflows/docs-pages.yml`: upload `/docs` verbatim, deploy) instead of the legacy branch-based Jekyll builder, which began failing repo-wide with zero-duration "Page build failed" errors on commits that didn't touch docs — including on direct build requests via the Pages API. The site is pure static HTML, so the legacy builder added nothing but a failure mode; deploys now also skip entirely on commits that don't change `docs/`.

### Changed

- The GitHub Action's PR-comment footer now tells the reader what to do next — "add it to your repo in 2 minutes", linking to the action's install snippet — instead of only naming the tool. Every entity-diff comment is seen by all of a repo's collaborators; the footer is the loop that turns viewers into installs.

### Added

- **`sem repos`** — where your code is stored, in one command. Two inventories side by side: the **cloud account** (authoritative `GET /v1/repos`: every indexed repo with status, entity/file counts, last-indexed time, indexed commit, and any indexing error rendered inline) and **local storage** (every entity cache under the sem cache root with size on disk, entity count, cache kind, and the repo it was built from). `--json` for scripts. Listing the account also reconciles this machine's `~/.sem/repos.json` mirror with server truth — stale entries (a repo registered mid-index stays "pending, 0 entities" forever otherwise) were silently mis-routing the local-vs-cloud decision for impact/context queries. Caches are now stamped with their repo root at save time (`repo_root` in `cache_metadata`); caches built before this show as unlabeled and self-label on their next rebuild.

- Fish shell support, via the `tree-sitter-fish` grammar (gated behind the `lang-fish` feature in `grammar-all`). Extracts functions — including the config.fish pattern of definitions inside a top-level `if status is-interactive` block — and resolves fish call edges (a `command`'s name against repo functions, builtins excluded), so `sem impact` sees which fish functions call which. A `function` defined inside another function stays part of the outer entity's content, matching fish's runtime semantics (inner definitions become global, not lexical children). Previously `.fish` files fell back to generic line-based chunking with an unsupported-language warning. Thanks @thalys for the request (#433).

### Documentation

- **First-principles page on the docs site** (`docs/first-principles.html`, linked from every page's nav): four charts explaining why the recent latency work changes what an agent can afford to do, not just how fast it runs — the scan-vs-index crossover (a text scan pays per byte; residency removes the index's ~800ms hydrate floor, so the constant-time line wins at every repo size), the ~100ms human-perception threshold every new path now sits under, model turns as the real cost unit (3 → 2 → 1 inference turns per structural answer via one-call lookup, then prompt-time prefetch), and tokens per answer (the measured ~15% entity-tree-vs-JSON ratio). Measured numbers come from this changelog; model curves and turn timings are labeled illustrative on the page. Charts are dependency-free inline SVG with hover tooltips and a table view each.

### Performance

- **Content-store cache (storage engine layer 1)**: the entity cache no longer duplicates source text per entity. Each file's text is stored once (zstd) in a `file_contents` table, and any entity whose body is provably a byte slice of it (`content == file[start_byte..end_byte]`, verified at save time) stores NULL content and is re-sliced on load; unprovable entities (no spans, normalized endings) keep content inline. On a 139K-entity corpus (fresh cache both sides) this cut the cache 20% (269MB to 216MB; the content layer itself −58%, 80MB to 24MB inline + 10MB zstd), engaged for 77% of entities. Honest costs: warm full-content loads pay ~0.13s extra for decompress+slice on that corpus (0.38s to 0.51s); topology loads and the MCP server's in-memory hot path are unaffected, and cold build time is unchanged within noise (peak RSS ~−5%). Correctness gates: byte-identical graph vs the previous binary on the full corpus, byte-identical entity content round-trip (including multi-byte unicode and nested entities), and incremental saves keep the file store in sync with entity deletes. Cache schema v8 — existing caches rebuild automatically on first use.

### Added

- **Entity-addressed text search**: `sem_entities` takes a `text` parameter — an exact substring searched across entity bodies in the warm in-memory graph (no file reads). Hits come back addressed by the innermost enclosing entity (`file: entity (Lline): matched text`), ready to chain into `sem_context`/`sem_impact`, in ~20-30ms warm on an 85K-LOC repo. This retires the main remaining reason agents fell back to grep (strings, error messages, config keys); misses say honestly that comments between entities and non-code files are not covered.

### Performance

- Graph build: the scope resolver no longer allocates its debug resolution log (several owned strings per reference, discarded by every production path — only a bench consumed it), and edge dedup is index-based instead of cloning both entity IDs per edge into a hash set. Output is byte-identical (proven edge-for-edge on a 139K-entity build); ~1-3% fewer instructions retired. Groundwork toward #320/#322 — the remaining peak-memory work (entity content sharing, ID interning) is tracked there.

### Added

- **`sem hook prompt-submit`** (hidden plumbing): the prompt-time prefetch, compiled. Reads a Claude Code UserPromptSubmit event, extracts identifier-shaped tokens from the prompt (backticked, snake_case, CamelCase, qualified — never plain words), resolves them against the resident server's socket sidecar, and prints packed entity context for injection. **10ms end-to-end** (was ~40ms as a Python hook — interpreter startup and a git subprocess, both eliminated: repo root is found by walking to `.git` in-process). Silent on conversational prompts, slash commands, unknown names, or when no server is resident.

### Added

- The socket sidecar is unix-only (`cfg(unix)`): Windows builds skip it with a no-op and the prefetch hook falls back silently — the sidecar is an accelerator, never a requirement. (Fixes the Windows build break the sidecar introduced.)
- **Socket sidecar**: `sem mcp` now exposes the warm in-memory graph on a per-repo unix socket (`~/.sem/sock/<repo-hash>.sock`, one JSON line in, one out). Short-lived local callers — the prompt-prefetch hook, future CLI fast paths — get one-call entity context in single-digit milliseconds instead of paying a fresh process plus SQLite hydrate (~800ms). Stale sockets from dead servers are detected and taken over; the sidecar is a silent accelerator, never a requirement.

### Added

- **One-call lookup**: `sem_context`'s `file_path` is now optional. With only an `entity_name`, the entity is resolved across the whole repo (unique match proceeds; ambiguity returns a compact candidate list with the files; no match returns near-name suggestions) and the body plus callers/callees comes back in a single round-trip — one agent call where grep needs two (search, then read). Measured 26ms wall on a prewarmed server, name-only, unfamiliar repo.

### Performance

- The sem MCP server is now **local-first and prewarmed**. Cloud-first routing on `sem_impact`/`sem_context` cost a network round-trip on every call before the local answer (and carried the same wrong-entity risk gated in the CLI); it is now behind `SEM_MCP_CLOUD=1` until the server resolves name+file strictly. The server also builds the CWD repo's graph in the background at startup, so the agent's first structural query answers from memory. Measured on an 85K-LOC repo: warm `sem_context` runs in under 1ms wall (faster than a ripgrep scan of the same repo), and the first call dropped from 129ms cold to ~0 with prewarm.

### Removed

- Team presence was pulled from the `--badge` package before it shipped as a feature (product call: not a feature for now). The statusline no longer shows teammates and the hook sends nothing anywhere; the dormant server endpoints remain unadvertised.

### Added

- The `--badge` statusline is now **live at trigger time**: a PreToolUse hook flips the badge to an animated spinner with the entity name the moment the agent calls sem (`⊕ sem ⠹ impact validateToken…`), and the completed state (count, latency, savings) lands when the call finishes. The render hot path never touches the network (renders measured at ~20ms).

### Fixed

- The `sem context` / `sem_context` budget packer no longer starves the target while neighbors feast. Previously a target too big for the budget collapsed to its first line (2 tokens) while a single large dependency could consume the entire budget with its full body. The target now degrades gracefully — full body → head-truncated body (docstring, fields, leading code, with an explicit `… truncated: N more lines` marker) using up to ~70% of the budget → bare signature — and no neighbor may cost more tokens than the target itself did (budget/10 floor), oversized neighbors degrading to signatures. On the same query (a large class, budget 2000) the target went from 2 tokens to 1,398 and the answer-relevant attributes are now in the payload.

### Added

- **Entity-level history analytics**: `sem log` with no entity now analyzes recent repo history in one pass and reports **hotspots** (the most-changed code entities, with commit counts, distinct authors, and the last commit that touched each) and **co-change pairs** (entities that repeatedly change in the same commits, with a confidence score — "these two never change apart"). Counts are per commit, code entities only (doc headings, config properties, and lockfile chunks are excluded so the signal is about code), and bulk commits touching >50 entities are excluded from pair-counting to keep quadratic noise out. Same via MCP: `sem_log` without `entity_name`. `--file` scopes to one file; `--json` returns everything. This is the time axis a snapshot dependency graph cannot see: which code churns, and which code moves together.

### Changed

- `sem_impact` MCP results now render as a **blast-radius tree** (`◉` header, one `├─▶` branch per file, real callers first, all-test files sunk to the bottom, nothing elided) — expanding the tool widget is the live graph, no separate viewer process needed. The bundled skill also instructs agents to draw the blast radius as a small ASCII tree directly in their reply when an impact result drives the answer.

- `sem_impact` and `sem_context` MCP results now render as a compact entity tree instead of pretty-printed JSON: dependents/dependencies/transitive impact grouped one line per file, every entity name preserved, with the elapsed time and source in a footer. The same information lands in about 15% of the tokens, and the expanded tool widget in agent UIs reads at a glance (`⊕ entity · file`, `← 29 dependents · 10 files`, `⚡ 70 transitively affected`). Context entries keep their verbatim content under a per-entry header.

## [0.15.1] - 2026-07-01

### Added

- `npx @ataraxy-labs/sem-skill --badge` (opt-in) installs a live sem badge in the Claude Code statusline: it shows how many structural queries ran this session, the last command **and the entity it analyzed**, its latency, a sparkline of recent latencies, and a rotating stat (distinct entities analyzed, top command) (`⊕ sem ×12  impact validateToken 9ms  ▁▂▃▅▂  · 7 entities analyzed`). It is fed by a PostToolUse hook that catches sem via **both** the MCP tools and the `sem` CLI (Bash), and falls back to recent activity so the badge never stalls on "idle". Non-destructive: it backs up settings and never overwrites an existing statusline (it prints how to add the badge yourself instead).
- **GitHub Action** (`Ataraxy-Labs/sem/action`): entity-level semantic diff comments on pull requests. One sticky comment per PR showing which functions/classes/methods were added, modified, or deleted, updated in place on every push; cosmetic-only PRs (formatting/comments) are called out explicitly. Installs the prebuilt binary (~2s), needs no config or API keys, and never fails the build. sem's own PRs now dogfood it via `.github/workflows/pr-entity-diff.yml`.
- The savings meter now lives in the **statusline itself** — no extra process. The `--badge` badge always shows the live estimated time + tokens this session's sem calls saved vs grep+read (`⊕ sem ×5 diff · ≈ 4m · ≈ 25k tokens saved`), and when idle it shows the lifetime total (`⊕ sem idle · ≈ 3h · ≈ 190k tokens saved`). The PostToolUse hook is the single writer of the persisted lifetime tally (`~/.claude/sem-savings.json`), so the counter grows from real usage whether or not the live viewer is open. Estimates stay anchored to the measured benchmark and labelled `≈`.
- Live viewer for the `--badge` install: `~/.claude/sem-live.py` (run it in a spare terminal pane). It redraws an ASCII blast-radius graph each time sem runs — the analyzed entity, its direct callers (real ones surfaced, test fan-out collapsed), and the transitive count — plus a **savings meter**: a running, honestly-estimated tally of the grep+read round-trips, time, and tokens sem saved this session, and a lifetime counter persisted across sessions (`~/.claude/sem-savings.json`). Estimates are anchored to a measured benchmark and labelled `≈`. The badge hook now also records `--file` and cwd so the graph can be reconstructed.

### Fixed

- Repository discovery now tolerates Git worktrees that use the `extensions.relativeworktrees` config key, avoiding libgit2's unsupported-extension error when plain `git` can open the checkout.
- Cloud-backed `sem impact` / `sem context` no longer answer queries they can't answer correctly. Two gates added: `--no-cache` now always computes fresh locally (previously the cloud snapshot was served anyway), and **file-hinted queries (`--file`) stay local** — the cloud resolves entities by name with a silent name-only fallback, so for same-named entities (e.g. ten `fn run` command handlers) it could return the *wrong entity's* graph, and a stale cloud index could drop dependents that exist locally. Local resolution disambiguates exactly; the cloud path returns once the server resolves name+file strictly and exposes its indexed commit for a freshness check.
- Impact/dependency resolution now follows type-qualified associated calls (`Type::method()`) when the receiver is a known repo type, so a caller reached only through a static/associated path is no longer dropped from `sem impact`. Previously, e.g., a test helper calling `SemPlugin::detect_changes()` was invisible to the reverse-dependency graph, and its transitive callers were missing from the blast radius. Resolution stays precise: a bare module path (`foo::bar::baz()`) still does not bind to a same-name local function, and common associated names (`Type::new`, `::default`) are not guessed.

### Performance

- Faster graph hydrate on large repos. The public `EntityGraph` maps now use `rustc-hash` (FxHashMap) instead of std SipHash, matching the build's internal maps, and the SQLite cache sets read pragmas (`mmap_size`, `cache_size`, `temp_store=MEMORY`) on every connection. On a 200K-entity / 800K-edge graph this is about 9% faster to hydrate (0.42s to 0.39s, no overlap across repeats); negligible on small repos. Output is byte-identical.

## [0.15.0] - 2026-06-30

### Changed

- Whole-repo commands (`sem graph`) now skip the file-discovery walk when git proves the cache is fresh (HEAD unchanged and the working tree clean), serving the cached topology directly. On a 200K-file repo this is about 9x faster with git fsmonitor and about 4.5x faster without it; small repos and non-git repos are unchanged. The oracle only ever declines to accelerate, never serves stale results, and the `git status` check is time-bounded (`SEM_FRESHNESS_TIMEOUT_MS`) with `SEM_FRESHNESS=scan|git|auto` to override.

### Added

- `sem xref` lists cross-repo dependencies across your indexed repos: entities in one repo that depend on entities in another. A single-repo local graph can't see this, so it's a cloud feature (requires `sem login`) and is gated to the team/enterprise tier. Adds `cross_deps()` to the shared cloud client.
- `sem diff` now prints a one-line hint, when run interactively and logged out, that `sem login` reveals what your changes break across repos (a cross-repo question a local single-repo diff can't answer). It is heavily throttled (at most once a week), shown only on a terminal with real entity changes, and stays completely silent in CI, pipes, `--json`/non-terminal output, and for logged-in users.

### Performance

- Cache freshness checks now run the per-file `stat` + content-hash scan in parallel (rayon) instead of sequentially (#351). On touched-file cache hits over large repos, the freshness scan was the dominant remaining cost (~42ms of sequential filesystem/hash work on a 5K-file touched scenario); it now scales across cores. SQLite reads stay serial (the connection isn't shared across threads) and fingerprint-refresh writes remain serial and best-effort — only the pure filesystem+hash work is parallelized, so cache-hit validity is unchanged.

### Documentation

- The bundled `/sem` agent skill no longer hardcodes a language count. It said "31 languages", which went stale as grammars were added and disagreed with the README ("32") and the crate description ("28"); it now says "30+ languages" so it can't drift, and an en-dash was replaced with a hyphen.
- README: documented the optional cloud acceleration flow (`sem login` serves `impact`/`context`/`entities` from a warm pre-built graph for large repos; local is unchanged and `SEM_LOCAL=1` forces local), and added Lua to the supported-languages table.

### Added

- Lua support, via the `tree-sitter-lua` grammar (gated behind the `lang-lua` feature in `grammar-all`). Extracts global, `local`, table (`t.f`), and method (`t:f`) functions. Thanks @mmgeorge for the request (#393).
- `SemanticEntity` now carries optional `start_byte`/`end_byte` offsets, populated from the underlying tree-sitter node during code extraction. A consumer can slice the exact original bytes of an entity out of a file given only its `file_path` and span, without re-parsing. Persisted through the entity cache and surfaced in `sem entities --json`. Thanks to Thomas J. for the request.

### Added

- `npx @ataraxy-labs/sem-skill`: one-command setup of sem for coding agents. Installs the sem skill into `~/.claude/skills/` and registers the `sem mcp` server, so an agent uses sem (impact / context / orient / diff) over grep for structural questions without manual setup. Builds on the skill contributed in #376.

### Added

- An agent skill (`skills/sem/SKILL.md`) documenting sem's semantic diff, impact, blame, history, context, and graph workflows for coding agents. Thanks @linhlban150612 for the contribution (#376).
- `self-update` Cargo feature (on by default) gates the built-in `sem update` and the background update-available check. Distro and package-manager builds that own the binary's lifecycle can opt out with `cargo build --no-default-features`; `sem update` then prints a "update through your package manager" message instead of replacing the binary. Thanks @0323pin (pkgsrc/NetBSD) for the request (#390).

### Added

- `sem context --hops N` bounds the related entities to N graph hops from the target (instead of filling to the token budget), so you can ask for "the entity and just its immediate neighborhood." The `sem_context` MCP tool gains the same `hops` parameter. 0 (the default) keeps the existing unbounded, budget-driven behavior.

### Changed

- The `sem mcp` instructions now tell agents to read code with `sem_context` (which returns an entity's full source plus its callers/callees, addressed by name) rather than opening the file, reserving direct file reads for editing and non-code. Reading by entity is robust to line drift and arrives with the dependency context.

## [0.14.1] - 2026-06-23

### Fixed

- Release pipeline: the Intel macOS cross-build failed on `openssl-sys` (no target-arch OpenSSL when cross-compiling on Apple Silicon). It now builds OpenSSL from source via `--features vendored-openssl`, the same approach the Linux arm64 cross-build uses. 0.14.0's binaries never published because of this; 0.14.1 is the first release to ship binaries for every platform, including Intel macOS (#374).

## [0.14.0] - 2026-06-23

### Added

- `sem orient <query>` finds the entities most relevant to a query, structural code search for when you're dropped into an unfamiliar codebase and don't know the symbol name yet (e.g. `sem orient "where is the retry logic"`). Two-pass ranking: lexical score over entity name (subtoken + prefix/stem + substring), file path, and signature line, then a graph-centrality re-rank so a central, widely-used entity outranks a trivially-named helper. Results show the entity, its `file:line`, signature, and dependent count. `--json` and `--limit` supported. This is the structural counterpart to grep: grep finds text, orient finds the entity and how connected it is.
- The `sem_entities` MCP tool accepts a `query` parameter for the same intent search, so agents can find code by what it does (not just by name) without falling back to grep. The ranking is shared with the CLI (`sem_core::parser::orient`).
- `sem orient` down-weights entities in test files so implementation outranks an equivalently-named test. Test functions often match a query strongly by name, but the implementation is almost always what you want; tests stay findable, just below the real code.
- `sem entities` accepts `--only <kind>` and `--except <kind>` (both repeatable) to filter the listing by entity kind, e.g. `sem entities --only function --only struct` or `sem entities --except import`. The two flags are mutually exclusive. Because entity kinds are language-dependent, an unknown kind reports the kinds actually found in the scanned files rather than guessing a static list. Thanks @aleclarson for the request (#378).
- `SEM_WIDTH` sets the terminal-diff box width. sem's per-file box was a fixed 55 columns with no TTY attached, so it didn't match the surrounding pane when used as a pager (e.g. `lazygit`). Set `SEM_WIDTH=<columns>` to control it. Thanks @franky47 for the request (#380).

### Fixed

- The Intel macOS binary now builds reliably. The release built `x86_64-apple-darwin` on a native Intel `macos-13` runner, which GitHub is retiring, so the job could queue indefinitely and stall the whole release (0.13.1's binaries never published for this reason). It now cross-compiles on Apple Silicon `macos-14`, where runners are plentiful. 0.14.0 is the first release to ship Intel macOS binaries.

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
