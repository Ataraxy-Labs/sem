# Changelog

All notable changes to sem are documented in this file.

## [Unreleased]

### Added

- Start tracking project changes in `CHANGELOG.md`.
- Add a pull request check that asks contributors to include a changelog entry.
- `sem entities` accepts multiple file or directory path arguments.

### Changed

- Cloud sync only auto-registers repos that GitHub confirms are public. Private repos run locally unless you opt in with `SEM_SYNC_PRIVATE=1`.
- `install.sh` verifies the release archive against `checksums.txt` before installing.

### Fixed

- Kotlin: resolve method calls through typed function parameters (e.g. `fun f(s: Scenario) { s.method() }`). The `tree-sitter-kotlin-ng` grammar exposes `parameter` children positionally without `name`/`type` fields, so parameter types were never recorded and no call edges were produced. `sem context`/`impact`/`log` now find these callers.
