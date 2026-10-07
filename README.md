# Sem

Understand code by functions, classes, and methods—not just changed lines.

Sem uses tree-sitter to extract code entities and relationships. Use it alongside Git to find code, inspect changes, and check their impact. [Weave](https://github.com/Ataraxy-Labs/weave) handles entity-level patches and merges.

## Install

```bash
brew install sem-cli
# or
cargo install sem-cli
```

[Other installation options](USAGE.md#install)

## Quickstart

Run inside your repository. No setup is needed for local queries.

```bash
sem diff                              # What changed?
sem find parse_config                 # Where is it defined?
sem find parse_config --context        # Read it with related code
sem find parse_config --callers        # Who calls it?
sem grep 'retry budget'                # Find text
sem impact parse_config --tests        # Which tests could be affected?
sem check                             # Run project checkers
sem certify main..HEAD                # Produce review evidence
```

## Commands

| Command | Purpose |
|---|---|
| `find` | Definitions, entity lists, callers, references, and source context |
| `grep` | Text search |
| `impact` | Affected dependencies, dependents, and tests |
| `graph` | Code relationships |
| `diff` | Entity-level changes |
| `check` | Project validation |
| `certify` | Review evidence |
| `history` | Entity history and blame |
| `config` | Git diff setup, stats, updates, and preferences |
| `cloud` | Optional cloud features |
| `mcp` | Agent tool server |

Run `sem <command> --help` for options. [Full reference](USAGE.md#commands)

## Use with agents

For Claude Code:

```bash
claude mcp add sem -- sem mcp
```

Other MCP clients can launch `sem mcp` over stdio in the repository.

Tools: `sem_find`, `sem_grep`, `sem_impact`, `sem_graph`, `sem_diff`, `sem_check`, `sem_certify`, and `sem_history`.

Developers use the CLI; agents use tools backed by the same capabilities. [Agent setup](USAGE.md#use-with-ai-agents-mcp)

## What to expect

- Code remains ordinary source files in Git.
- Language and relationship coverage varies; the graph is not a complete compiler model.
- Passing checks and review certificates do not prove every behavior correct.
- The local cache lives outside the repository. Cloud queries are opt-in.

## Learn more

[Languages](USAGE.md#what-it-parses) · [Git integration](USAGE.md#use-as-default-git-diff) · [Cloud consent](docs/cloud-consent.html) · [Architecture](USAGE.md#architecture) · [Contributing](USAGE.md#contributing) · [Releases](https://github.com/Ataraxy-Labs/sem/releases)

Part of [Ataraxy Labs](https://ataraxy-labs.com). See [LICENSE-MIT](LICENSE-MIT).
