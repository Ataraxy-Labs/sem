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

- Java: resolve cross-file `receiver.method()` call edges. Local variable types were never recorded (the `Dog d = new Dog()` declaration type and `object_creation_expression` RHS were both ignored), class field types were never tracked (`init_strategy` was `None`), and `ClassName.staticMethod()` calls were dropped. As a result `sem impact`/`context` reported few or no cross-file dependents on Java code — an empty result was a false negative. Java scope-resolution recall on the test fixture rises from 27% to 100%; the common Spring field-injection pattern (`@Inject private FooService foo; ... foo.bar()`) now resolves.
