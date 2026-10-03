---
name: sem
description: Use sem for entity-level (function/class/method) code questions in any Git repo. Four questions, four verbs - where is it (sem find, sem grep), what does my change touch (sem impact), is it correct (sem check), what should a human review (sem certify). Trigger whenever the user asks where something is defined or called, what a change breaks or which tests to run, whether a change is correct, what changed in a commit or PR, or what to review; also use it proactively before refactors and in code review, where line-level git output is noisy.
license: MIT OR Apache-2.0
compatibility: Requires the sem CLI (https://github.com/Ataraxy-Labs/sem) on PATH and a Git repository
metadata:
  homepage: https://github.com/Ataraxy-Labs/sem
---

# sem

sem parses 30+ languages with tree-sitter and answers questions about entities
(functions, classes, methods) and the calls between them. Instead of "lines
43-51 changed" it says "function `validateToken` in `src/auth.ts` was
modified, and these 3 callers and 2 tests reach it". It works in any Git repo
with no setup. Answers are deterministic; when a caller set may be incomplete,
sem says so.

## Four questions, four verbs

| Question | CLI | MCP tool |
|---|---|---|
| Where is it? | `sem find NAME`, `sem grep TEXT` | `sem_find`, `sem_grep` |
| What does my change touch? | `sem impact NAME`, `sem impact --diff HEAD --tests` | `sem_impact` |
| Is it correct? | `sem check` | `sem_check` |
| What should a human review? | `sem certify main..HEAD` | `sem_certify` |

Also: `sem diff` (which entities changed), `sem graph` (how the code is
connected), `sem history` (how an entity changed; `--blame` for a file).
Every verb takes `--json`.

## Use the MCP tools first

If the agent has the sem MCP server (tools named `mcp__sem__*`), call those
instead of running `sem` in a shell. They return compact results and carry
`elapsed_ms`. If they are deferred, load them first rather than falling back
to Bash.

| Task | MCP call |
|---|---|
| where is `X` defined | `sem_find` with `query: "X"` |
| read and understand `X` | `sem_find` with `query: "X", mode: "context"`: body plus callers and callees, one call, no file needed |
| who calls `X` | `sem_find` with `mode: "callers"` |
| what `X` calls | `sem_find` with `mode: "refs"` |
| what is in this file or directory | `sem_find` with `in: "src/auth.ts"` |
| find code by intent, name unknown | `sem_find` with `intent: "where retries are scheduled"` |
| a string, error message, config key | `sem_grep`, or `sem_find` with `text` (hits named by entity) |
| what breaks / which tests to run | `sem_impact` (`mode: "tests"` for tests only) |
| is the change correct | `sem_check` |
| what to review in a range | `sem_certify` with `range: "main..HEAD"` |
| what changed | `sem_diff` |
| how `X` evolved / who changed it | `sem_history` (`blame: true` with `file_path` for a file) |

When you know or can guess the name, one `sem_find` call with mode
`context` replaces a grep followed by a file read. Ambiguous names return a
short candidate list; pass `in` only then.

## CLI

```bash
# where is it?
sem find validateToken                  # definition: type name file:line
sem find validateToken --callers        # who calls it
sem find validateToken --refs           # what it calls
sem find validateToken --context        # its code plus callers and callees, token-budgeted
sem find validateToken --context --budget 4000 --hops 1
sem find --in src/auth.ts               # every entity in a file or directory
sem grep "token expired"                # text, rg-style file:line:text

# what does my change touch?
sem impact validateToken                # dependents, deps, transitive impact, tests
sem impact validateToken --tests        # only the tests to run
sem impact --diff HEAD --tests          # tests for the uncommitted change
sem impact --diff main..HEAD --json     # one report per changed entity

# is it correct?
sem check                               # the project's compiler, type checker, linter, tests: exit 0 pass, 1 fail, 2 could not decide
sem check --checkers ts,tests           # only these checkers

# what should a human review?
sem certify main..HEAD                  # review certificate (markdown)
sem certify main..HEAD --arch           # the architecture view
sem certify main..HEAD --json

# more
sem diff                                # working tree changes, by entity
sem diff --from HEAD~5 --to HEAD --format json
sem graph --json                        # entity graph; --modules, --dataflow, --system for other layers
sem history validateToken               # how it evolved
sem history --blame src/auth.ts         # who last changed each entity
```

Older names (`sem callers`, `sem context`, `sem entities`, `sem log`,
`sem blame`, `sem arch-diff`, `sem topology`, ...) still work with the same
output.

## Draw the blast radius in your reply

When an impact result drives your answer (a refactor decision, a "what
breaks" question), render it as a small ASCII tree in the response:

```
◉ validateToken · src/auth.ts
│  8 direct → 23 transitive
├─▶ refreshToken        src/auth.ts
├─▶ loginHandler        src/routes/login.ts
├─▶ SessionMiddleware   src/middleware/session.ts
╰─▶ … +5 more (12 tests)
```

Real callers first, tests collapsed into a count, no invented entries: draw
only what the tool returned. Skip the drawing when impact was incidental.

## Make the leverage felt

`sem_find` (context mode) and `sem_impact` return `elapsed_ms`. When one sem
call replaces several grep/read steps, or catches something text search
cannot (a transitive caller in another file), say so in one terse clause,
e.g. `(sem_impact: 9ms, 2 transitive callers grep would miss)`. Once per
non-obvious win, never a sales pitch. If you fall back to grep or a file read
on a structural question, say why.

## Install check

```bash
sem --version   # confirm sem (not GNU Parallel's sem) is on PATH
```

If there's a conflict with GNU Parallel, add `alias sem="$HOME/.cargo/bin/sem"`
to the shell profile, or use `npx sem` / `bunx sem` if installed via npm/bun.
