# sem — entity-level PR diff action

Comments an **entity-level semantic diff** on every pull request: which
functions, classes, and methods were added, modified, or deleted — instead of
raw line noise.

> ### ⊕ Entity-level changes
>
> #### src/auth.ts
>
> | Status | Type | Name |
> |--------|------|------|
> | Δ | function | validateToken |
> | + | function | refreshToken |
>
> **Summary:** 1 added, 1 modified across 1 file

One sticky comment per PR, updated in place on every push. If a PR turns out to
be cosmetic-only (formatting, comments, docs), the comment says exactly that —
often the fastest review signal there is.

## Usage

```yaml
# .github/workflows/entity-diff.yml
name: Entity diff
on: pull_request

permissions:
  contents: read
  pull-requests: write

jobs:
  entity-diff:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: Ataraxy-Labs/sem/action@v0.15.1
```

That's it. No config, no API keys. (Pin the tag for stability, or use
`@main` to track the latest.) The action installs the prebuilt `sem`
binary (~2s), diffs the PR's base and head at the entity level across 30+
languages (tree-sitter), and posts the comment.

## Inputs

| input | default | description |
|-------|---------|-------------|
| `github-token` | `${{ github.token }}` | token used to post the comment (needs `pull-requests: write`) |
| `sem-version` | `latest` | sem release tag to install, e.g. `v0.15.0` |

## Notes

- The action never fails your build: any sem error is a warning, not a red X.
- Comments cap at GitHub's size limit; very large PRs are truncated with the
  summary preserved.
- On pull requests from forks, the default token is read-only, so the comment
  is skipped with a warning (use `pull_request_target` with care if you need
  fork comments).
- Everything runs inside the runner. No code leaves the machine.

Part of [sem](https://github.com/Ataraxy-Labs/sem) — semantic version control:
entity-level `diff`, `blame`, `log`, `impact` on top of Git.
