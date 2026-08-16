//! The new-file battery for `Complete` freshness (semx-ykf, `QUERY-INDEX.md`
//! §2/§10.6): black-box, CLI-level proof that the index-backed verbs
//! (`find`/`callers`/`refs`/`grep`) see a file created, deleted, or renamed
//! *after* the index was built — the membership blind spot the bead exists
//! to close. Every test primes an index first (any index-backed command
//! that finds nothing self-heals by writing one, per `commands::query`'s
//! module doc), mutates the filesystem, then re-runs the verb and asserts
//! on the answer, never on internal timing marks — this suite doesn't care
//! *how* the answer was produced (the fast concurrent sweep, or a fallback
//! to a cold rebuild), only that it is correct.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use tempfile::TempDir;

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: Output, context: &str) -> Output {
    assert!(
        output.status.success(),
        "{context} failed with status {:?}\n{}",
        output.status.code(),
        output_text(&output)
    );
    output
}

fn sem(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo)
        .env("SEM_CACHE_DIR", cache)
        .args(args)
        .output()
        .unwrap()
}

/// Prime an index for `repo` (`cache`'s per-repo `index.sem`) by running any
/// index-backed verb once — `commands::query`'s cold-build fallback always
/// self-heals by writing a fresh index (`QUERY-INDEX.md` §4.1's write-path
/// wiring), so a first call on an empty cache is exactly "build the index".
fn prime_index(repo: &Path, cache: &Path) {
    assert_success(
        sem(repo, cache, &["find", "unlikely_to_exist_xyz", "--json"]),
        "prime index",
    );
}

/// Wait until `dir`'s mtime has visibly moved past `before` — the POSIX
/// signal `Complete` freshness (`QUERY-INDEX.md` §2) leans on: creating,
/// deleting, or renaming a directory entry bumps the directory's own mtime.
/// Mirrors `impact_direct_deps.rs`'s `rewrite_after_mtime_tick`, but for a
/// directory rather than a file's content.
fn wait_for_dir_mtime_tick(dir: &Path, before: std::time::SystemTime) {
    for _ in 0..200 {
        if fs::metadata(dir).unwrap().modified().unwrap() != before {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("directory mtime never moved for {}", dir.display());
}

fn dir_mtime(dir: &Path) -> std::time::SystemTime {
    fs::metadata(dir).unwrap().modified().unwrap()
}

#[test]
fn find_sees_a_name_added_in_a_brand_new_file() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        repo.path().join("a.ts"),
        "export function existing() { return 1; }\n",
    )
    .unwrap();
    prime_index(repo.path(), cache.path());

    let before = dir_mtime(repo.path());
    fs::write(
        repo.path().join("brand_new.ts"),
        "export function freshlyAdded() { return 2; }\n",
    )
    .unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(
            repo.path(),
            cache.path(),
            &["find", "freshlyAdded", "--json"],
        ),
        "find in brand-new file",
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = json.as_array().expect("json array");
    assert_eq!(rows.len(), 1, "{}", output_text(&out));
    assert_eq!(rows[0]["name"], "freshlyAdded");
    assert_eq!(rows[0]["file"], "brand_new.ts");
}

#[test]
fn find_sees_a_name_added_in_a_brand_new_nested_directory() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        repo.path().join("a.ts"),
        "export function existing() { return 1; }\n",
    )
    .unwrap();
    prime_index(repo.path(), cache.path());

    let before = dir_mtime(repo.path());
    fs::create_dir_all(repo.path().join("src/deeply/nested")).unwrap();
    fs::write(
        repo.path().join("src/deeply/nested/new.ts"),
        "export function deeplyNestedFresh() { return 3; }\n",
    )
    .unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(
            repo.path(),
            cache.path(),
            &["find", "deeplyNestedFresh", "--json"],
        ),
        "find in brand-new nested directory",
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = json.as_array().expect("json array");
    assert_eq!(rows.len(), 1, "{}", output_text(&out));
    assert_eq!(rows[0]["file"], "src/deeply/nested/new.ts");
}

#[test]
fn callers_sees_a_caller_added_in_a_brand_new_file() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        repo.path().join("a.ts"),
        "export function source() { return 1; }\n",
    )
    .unwrap();
    prime_index(repo.path(), cache.path());

    let before = dir_mtime(repo.path());
    fs::write(
        repo.path().join("b.ts"),
        "import { source } from './a';\nexport function freshCaller() { return source(); }\n",
    )
    .unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(repo.path(), cache.path(), &["callers", "source", "--json"]),
        "callers of source after a new caller file appears",
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = json.as_array().expect("json array");
    assert_eq!(rows.len(), 1, "{}", output_text(&out));
    let related = rows[0]["related"].as_array().expect("related array");
    assert!(
        related.iter().any(|r| r["name"] == "freshCaller"),
        "expected freshCaller among callers, got {related:?}"
    );
}

#[test]
fn refs_sees_a_reference_added_in_a_brand_new_file() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        repo.path().join("a.ts"),
        "export function target() { return 1; }\n",
    )
    .unwrap();
    prime_index(repo.path(), cache.path());

    let before = dir_mtime(repo.path());
    fs::write(
        repo.path().join("b.ts"),
        "import { target } from './a';\nexport function freshRefs() { return target(); }\n",
    )
    .unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(repo.path(), cache.path(), &["refs", "freshRefs", "--json"]),
        "refs of a brand-new entity",
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = json.as_array().expect("json array");
    assert_eq!(rows.len(), 1, "{}", output_text(&out));
    let related = rows[0]["related"].as_array().expect("related array");
    assert!(
        related.iter().any(|r| r["name"] == "target"),
        "expected target among refs, got {related:?}"
    );
}

#[test]
fn find_no_longer_reports_a_deleted_files_entities() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        repo.path().join("a.ts"),
        "export function toBeDeleted() { return 1; }\n",
    )
    .unwrap();
    prime_index(repo.path(), cache.path());

    // Confirm the index actually knows it first — otherwise the assertion
    // below would pass vacuously.
    let before_delete = assert_success(
        sem(
            repo.path(),
            cache.path(),
            &["find", "toBeDeleted", "--json"],
        ),
        "find before deletion",
    );
    let before_json: serde_json::Value = serde_json::from_slice(&before_delete.stdout).unwrap();
    assert_eq!(before_json.as_array().unwrap().len(), 1);

    let before = dir_mtime(repo.path());
    fs::remove_file(repo.path().join("a.ts")).unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(
            repo.path(),
            cache.path(),
            &["find", "toBeDeleted", "--json"],
        ),
        "find after deletion",
    );
    // `find --json` prints `[]` (exit 0) for "no entity found" (see
    // `commands::query::render`) — a deleted definition must not survive.
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, serde_json::json!([]));
}

#[test]
fn find_follows_a_renamed_file_to_its_new_path() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        repo.path().join("old_name.ts"),
        "export function renamedFn() { return 1; }\n",
    )
    .unwrap();
    prime_index(repo.path(), cache.path());

    let before = dir_mtime(repo.path());
    fs::rename(
        repo.path().join("old_name.ts"),
        repo.path().join("new_name.ts"),
    )
    .unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(repo.path(), cache.path(), &["find", "renamedFn", "--json"]),
        "find after rename",
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = json.as_array().expect("json array");
    assert_eq!(rows.len(), 1, "{}", output_text(&out));
    assert_eq!(rows[0]["file"], "new_name.ts");
}

#[test]
fn grep_finds_text_in_a_brand_new_file() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(repo.path().join("a.ts"), "nothing interesting here\n").unwrap();
    // `sem grep`'s own cold path never writes an index (see
    // `commands::grep`'s doc) — prime it via `find` first, same as every
    // other test in this file, so the trigram-postings fast path is what
    // this test actually exercises.
    prime_index(repo.path(), cache.path());

    let before = dir_mtime(repo.path());
    fs::write(
        repo.path().join("fresh.ts"),
        "const needleInAFreshHaystack = 1;\n",
    )
    .unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(
            repo.path(),
            cache.path(),
            &["grep", "needleInAFreshHaystack", "--json"],
        ),
        "grep for text in a brand-new file",
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits = json["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1, "{}", output_text(&out));
    assert_eq!(hits[0]["file"], "fresh.ts");
}

#[test]
fn grep_does_not_return_a_hit_from_a_deleted_file() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        repo.path().join("gone.ts"),
        "const uniqueDoomedNeedle = 1;\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("stays.ts"),
        "const uniqueDoomedNeedle = 2; // still here\n",
    )
    .unwrap();
    prime_index(repo.path(), cache.path());

    let before = dir_mtime(repo.path());
    fs::remove_file(repo.path().join("gone.ts")).unwrap();
    wait_for_dir_mtime_tick(repo.path(), before);

    let out = assert_success(
        sem(
            repo.path(),
            cache.path(),
            &["grep", "uniqueDoomedNeedle", "--json"],
        ),
        "grep after a matching file was deleted",
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits = json["hits"].as_array().expect("hits array");
    let files: Vec<&str> = hits.iter().map(|h| h["file"].as_str().unwrap()).collect();
    assert_eq!(files, vec!["stays.ts"], "{}", output_text(&out));
}
