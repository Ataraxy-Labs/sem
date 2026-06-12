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

- Kotlin: resolve method calls through typed receivers that the `tree-sitter-kotlin-ng` grammar exposes positionally (no `name`/`type` fields). Several scope-resolution paths used field names from the older grammar and silently produced no call edges. Fixed:
  - typed function parameters — `fun f(s: Scenario) { s.method() }`;
  - class field types from property declarations (`val conn: Connection`) and primary-constructor properties (`class Tx(val conn: Connection)`);
  - chained field access — `val s = container.scenario; s.method()` resolves `s` via the class field-type map;
  - declared and inferred return types (`fun get(): Connection` / `fun get() = Connection()`), so `val c = get(); c.method()` resolves.
  Kotlin scope-resolution recall on the test fixture rises from 82% to 100%. `sem context`/`impact`/`log` now find these callers.
