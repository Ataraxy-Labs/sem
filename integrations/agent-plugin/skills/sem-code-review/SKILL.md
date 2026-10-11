---
name: sem-code-review
description: Find named code entities, inspect callers, and review changed functions with the locally installed Ataraxy Sem CLI. Use for code discovery or Git change review when Sem is available.
---

# Code discovery and review with Sem

Use the existing terminal tool in the user's chosen repository. Check `sem --version`
first: this skill targets Ataraxy Sem 0.27.0, not GNU Parallel's `sem` executable.
If it is missing or incompatible, explain the prerequisite instead of installing
software or changing the user's tool configuration automatically.

For local-only operation, prefix each Sem invocation with
`SEM_NO_NETWORK=1 SEM_NO_TELEMETRY=1`. These settings prevent Sem cloud access
and telemetry even if the user enabled them elsewhere. They are not an OS sandbox.

Choose the smallest query that answers the question (examples use `parse_config`):

```sh
SEM_NO_NETWORK=1 SEM_NO_TELEMETRY=1 sem find parse_config --json
SEM_NO_NETWORK=1 SEM_NO_TELEMETRY=1 sem find parse_config --callers --json
SEM_NO_NETWORK=1 SEM_NO_TELEMETRY=1 sem find parse_config --context --budget 3000 --json
SEM_NO_NETWORK=1 SEM_NO_TELEMETRY=1 sem diff --json
SEM_NO_NETWORK=1 SEM_NO_TELEMETRY=1 sem impact parse_config --tests --json
```

Several known names can share one definition query: `sem find parse_config load_config --json`.
For a requested commit range use `sem diff BASE..HEAD --json`, substituting the
actual refs. Do not invent a base branch or treat untracked files as part of the diff;
check the host's Git status tool when those matter.

Read bodies or surrounding source only when the task needs them. Disambiguate
duplicate names with `--in` for find or `--file` for impact. Static caller/test
results can be incomplete; an empty result is not proof that no caller or test
exists. Use ordinary text search and file reads for missing relationships,
configuration, unsupported syntax, or omitted context.

For review, report changed entities, affected callers, and unresolved questions
with source locations. Structure does not prove runtime correctness. Keep edits
and test execution within the user's request and the host's existing approval
flow. Do not run `sem check` merely to inspect a diff: it executes repository
compiler, linter and test commands. A review request does not authorize those
commands, Git configuration changes, commits, or a merge.
