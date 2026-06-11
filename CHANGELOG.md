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
