use std::{
    fs,
    path::{Path, PathBuf},
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

fn git(repo: &Path, args: &[&str]) -> Output {
    assert_success(
        Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap(),
        &format!("git {}", args.join(" ")),
    )
}

fn init_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("a.ts"),
        "export function source() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("b.ts"),
        "import { source } from './a';\nexport function consume() { return source(); }\n",
    )
    .unwrap();
    fs::write(
        repo.join("c.ts"),
        "export function unrelated() { return 2; }\n",
    )
    .unwrap();
    git(repo, &["add", "a.ts", "b.ts", "c.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn init_topology_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("a.ts"),
        "export function source() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("b.ts"),
        "import { source } from './a';\nexport function consume() { return source(); }\n",
    )
    .unwrap();
    fs::write(
        repo.join("c.ts"),
        "import { consume } from './b';\nexport function transitive() { return consume(); }\n",
    )
    .unwrap();
    fs::write(
        repo.join("a.test.ts"),
        "import { source } from './a';\ntest('source works', () => source());\n",
    )
    .unwrap();
    git(repo, &["add", "a.ts", "b.ts", "c.ts", "a.test.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn init_side_effect_import_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(repo.join("a.ts"), "console.log('side effect');\n").unwrap();
    fs::write(
        repo.join("b.ts"),
        "import './a';\nexport function consume() { return 1; }\n",
    )
    .unwrap();
    git(repo, &["add", "a.ts", "b.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn init_missing_import_target_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("b.ts"),
        "import './optional';\nexport function consume() { return 1; }\n",
    )
    .unwrap();
    git(repo, &["add", "b.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn init_default_reexport_missing_target_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("barrel.ts"),
        "export { default } from './target';\n",
    )
    .unwrap();
    fs::write(
        repo.join("consumer.ts"),
        "import publicTarget from './barrel';\nexport function usePublicTarget() { return publicTarget(); }\n",
    )
    .unwrap();
    git(repo, &["add", "barrel.ts", "consumer.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn init_bare_import_target_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("b.ts"),
        "import { source } from 'source';\nexport function consume() { return source(); }\n",
    )
    .unwrap();
    git(repo, &["add", "b.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn init_python_missing_import_target_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("b.py"),
        "from optional import source\n\ndef consume():\n    return source()\n",
    )
    .unwrap();
    git(repo, &["add", "b.py"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

#[cfg(unix)]
fn init_symlink_source_repo(repo: &Path, symlink_target: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("a.ts"),
        "export function source() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("b.ts"),
        "import { source } from './a';\nexport function consume() { return source(); }\n",
    )
    .unwrap();
    fs::write(
        symlink_target,
        "export function linkedUnrelated() { return 2; }\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(symlink_target, repo.join("c.ts")).unwrap();
    git(repo, &["add", "a.ts", "b.ts", "c.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn find_cache_db(path: &Path) -> PathBuf {
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == "cache.db") {
            return path;
        }
        if path.is_dir() {
            let candidate = find_cache_db(&path);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::new()
}

fn mark_cache_as_topology_with_test_flags(cache_root: &Path) {
    let db_path = find_cache_db(cache_root);
    assert!(db_path.exists(), "cache db not found under {cache_root:?}");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let test_id: String = conn
        .query_row(
            "SELECT id FROM entities WHERE file_path = 'a.test.ts' AND entity_type = 'test' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute("DELETE FROM entity_flags", []).unwrap();
    conn.execute(
        "INSERT INTO entity_flags (entity_id, is_test) VALUES (?1, 1)",
        rusqlite::params![test_id],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO cache_metadata (key, value) VALUES ('cache_kind', 'topology')",
        [],
    )
    .unwrap();
}

fn mark_cache_as_topology_without_file_imports(cache_root: &Path) {
    let db_path = find_cache_db(cache_root);
    assert!(db_path.exists(), "cache db not found under {cache_root:?}");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute("DELETE FROM file_imports", []).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO cache_metadata (key, value) VALUES ('cache_kind', 'topology')",
        [],
    )
    .unwrap();
}

fn phase_names(output: &Output) -> Vec<String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let timings: serde_json::Value = serde_json::from_str(stderr.trim()).expect("timings json");
    timings["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|phase| phase["name"].as_str().unwrap().to_string())
        .collect()
}

fn rewrite_after_mtime_tick(path: &Path, content: &str) {
    let before = fs::metadata(path).unwrap().modified().unwrap();

    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(path, content).unwrap();
        if fs::metadata(path).unwrap().modified().unwrap() != before {
            return;
        }
    }

    panic!("mtime did not change for {}", path.display());
}

#[test]
fn impact_deps_no_cache_uses_direct_dependency_graph() {
    let repo = TempDir::new().unwrap();
    init_repo(repo.path());

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_TIMINGS", "json")
            .args([
                "impact",
                "consume",
                "--file",
                "b.ts",
                "--deps",
                "--json",
                "--no-cache",
            ])
            .output()
            .unwrap(),
        "impact deps",
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["entity"]["name"], "consume");
    assert_eq!(json["dependencies"][0]["name"], "source");

    let phases = phase_names(&output);
    assert!(phases
        .iter()
        .any(|phase| phase == "direct_dependency_graph_build"));
    assert!(!phases.iter().any(|phase| phase == "full_graph_build"));
}

#[test]
fn impact_deps_answers_from_the_index_on_the_second_run() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "cached impact deps",
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["entity"]["name"], "consume");
    assert_eq!(json["dependencies"][0]["name"], "source");

    let phases = phase_names(&output);
    assert!(phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(!phases.iter().any(|phase| phase == "file_discovery"));
    assert!(!phases.iter().any(|phase| phase == "cache_topology_load"));
    assert!(!phases.iter().any(|phase| phase == "full_graph_build"));
}

#[test]
fn impact_deps_answers_from_the_index_when_an_unrelated_file_changes() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    rewrite_after_mtime_tick(
        &repo.path().join("c.ts"),
        "export function unrelated() { return 3; }\n",
    );

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "cached impact deps after unrelated edit",
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["entity"]["name"], "consume");
    assert_eq!(json["dependencies"][0]["name"], "source");

    let phases = phase_names(&output);
    assert!(phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(!phases.iter().any(|phase| phase == "file_discovery"));
    assert!(!phases.iter().any(|phase| phase == "full_graph_build"));
}

#[test]
fn impact_deps_declines_the_index_when_an_imported_file_changes() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    rewrite_after_mtime_tick(
        &repo.path().join("a.ts"),
        "export function source() { return 3; }\n",
    );

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after imported edit",
    );

    let phases = phase_names(&output);
    assert!(phases.iter().any(|phase| phase == "file_discovery"));
    assert!(!phases.iter().any(|phase| phase == "index_impact_deps"));
}

#[test]
fn impact_deps_index_fast_path_folds_in_a_new_import_target() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_missing_import_target_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::write(
        repo.path().join("optional.ts"),
        "export function optional() { return 1; }\n",
    )
    .unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after import target appears",
    );

    // GUARANTEE (repaired from the characterization,
    //.3/). `b.ts`'s ref to `./optional` was
    // unresolved at the last index build (no entity to point at yet), so
    // `refs_of` never carried the edge and no *known* file's staleness
    // check could catch it. The membership sweep now sees `optional.ts` as
    // a brand-new file and declines the fast path — same "any new file →
    // decline to cold rebuild" mechanism `find`/`callers`/`refs` already use
    // — so the legacy (always-correct) path runs.
    //
    // `import './optional';` is a bare **side-effect** import: it binds no
    // name, so neither tier records a symbol-level `EntityRef` for it (the
    // graph model has no representation for "depends on a whole module with
    // no named symbol" — the same disclosed, out-of-scope gap
    // `impact_deps_index_fast_path_does_not_notice_a_side_effect_import_change`
    // pins). The dependency list is therefore `[]` either way; what this
    // test actually proves is that the fast path stops *asserting* freshness
    // it cannot prove and instead matches the authoritative answer exactly
    // (`SEM_NO_INDEX=1` gives the same `[]` — verified below), rather than
    // silently trusting a CSR that predates `optional.ts`'s existence.
    let phases = phase_names(&output);
    assert!(!phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(phases.iter().any(|phase| phase == "file_discovery"));

    let authoritative = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_NO_INDEX", "1")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "authoritative impact deps after import target appears",
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let authoritative_json: serde_json::Value =
        serde_json::from_slice(&authoritative.stdout).unwrap();
    assert_eq!(
        json, authoritative_json,
        "declined fast path must match the authoritative answer byte-for-byte"
    );
    assert_eq!(
        json["dependencies"].as_array().unwrap().len(),
        0,
        "a bare side-effect import binds no name, so both tiers agree on no dependencies"
    );
}

/// The change's exact repro, mirrored to the direction its own
/// wording names: "new file imports existing entity → dependents answer
/// includes it". `try_index_impact_dependents` has the same structural gap
/// `try_index_impact_deps` had — a brand-new file that calls an
/// already-indexed entity is a new edge `callers_of` cannot carry, and no
/// *known* file's staleness check can catch a file that didn't exist at the
/// last build.
#[test]
fn impact_dependents_index_fast_path_folds_in_a_new_caller_file() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args([
                "impact",
                "source",
                "--file",
                "a.ts",
                "--dependents",
                "--json",
            ])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    // A brand-new file that imports and calls the already-indexed `source`.
    fs::write(
        repo.path().join("newcaller.ts"),
        "import { source } from './a';\nexport function useSource() { return source(); }\n",
    )
    .unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args([
                "impact",
                "source",
                "--file",
                "a.ts",
                "--dependents",
                "--json",
            ])
            .output()
            .unwrap(),
        "impact dependents after new caller file appears",
    );

    let phases = phase_names(&output);
    assert!(!phases
        .iter()
        .any(|phase| phase == "index_impact_dependents"));
    assert!(phases.iter().any(|phase| phase == "file_discovery"));

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let dependents: Vec<&str> = json["dependents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(
        dependents.contains(&"useSource"),
        "the new caller file's entity must appear in source's dependents, got {json}"
    );
}

#[test]
fn impact_deps_index_fast_path_folds_in_a_new_default_reexport_target() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_default_reexport_missing_target_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args([
                "impact",
                "usePublicTarget",
                "--file",
                "consumer.ts",
                "--deps",
                "--json",
            ])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::write(
        repo.path().join("target.ts"),
        "export default function publicTarget() { return 1; }\n",
    )
    .unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args([
                "impact",
                "usePublicTarget",
                "--file",
                "consumer.ts",
                "--deps",
                "--json",
            ])
            .output()
            .unwrap(),
        "impact deps after default re-export target appears",
    );

    // GUARANTEE (repaired from the characterization,
    //.3/). Same repair as
    // `impact_deps_index_fast_path_folds_in_a_new_import_target`: the
    // membership sweep sees the new target file and declines the fast path,
    // so the legacy path re-resolves the import fresh.
    let phases = phase_names(&output);
    assert!(!phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(phases.iter().any(|phase| phase == "file_discovery"));

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let deps: Vec<&str> = json["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        deps,
        vec!["publicTarget"],
        "the new import target must appear in dependencies, got {json}"
    );
}

#[test]
fn impact_deps_index_fast_path_folds_in_a_new_bare_import_target() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_bare_import_target_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::write(
        repo.path().join("source.ts"),
        "export function source() { return 1; }\n",
    )
    .unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after bare import target appears",
    );

    // GUARANTEE (repaired from the characterization,
    //.3/). Same repair as
    // `impact_deps_index_fast_path_folds_in_a_new_import_target`: the
    // membership sweep sees the new target file and declines the fast path,
    // so the legacy path re-resolves the import fresh.
    let phases = phase_names(&output);
    assert!(!phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(phases.iter().any(|phase| phase == "file_discovery"));

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let deps: Vec<&str> = json["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        deps,
        vec!["source"],
        "the new import target must appear in dependencies, got {json}"
    );
}

#[test]
fn impact_deps_index_fast_path_folds_in_a_new_python_import_target() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_python_missing_import_target_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.py", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::write(
        repo.path().join("optional.py"),
        "def source():\n    return 1\n",
    )
    .unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.py", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after python import target appears",
    );

    // GUARANTEE (repaired from the characterization,
    //.3/). Same repair as
    // `impact_deps_index_fast_path_folds_in_a_new_import_target`: the
    // membership sweep sees the new target file and declines the fast path,
    // so the legacy path re-resolves the import fresh.
    let phases = phase_names(&output);
    assert!(!phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(phases.iter().any(|phase| phase == "file_discovery"));

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let deps: Vec<&str> = json["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        deps,
        vec!["source"],
        "the new import target must appear in dependencies, got {json}"
    );
}

#[test]
fn impact_deps_declines_the_index_outside_file_ext_scope() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .env("SEM_TIMINGS", "json")
        .args([
            "impact",
            "consume",
            "--file",
            "b.ts",
            "--deps",
            "--json",
            "--file-exts",
            ".tsx",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Entity 'consume' not found"));
}

#[test]
fn impact_deps_declines_the_index_after_semignore_excludes_target() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::write(repo.path().join(".semignore"), "*.ts\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .env("SEM_TIMINGS", "json")
        .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Entity 'consume' not found"));
}

#[test]
fn impact_deps_does_not_use_cache_first_for_unscoped_name_query() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::write(
        repo.path().join("d.ts"),
        "export function consume() { return 4; }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .env("SEM_TIMINGS", "json")
        .args(["impact", "consume", "--deps", "--json"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous"));
}

#[test]
fn impact_deps_answers_from_the_index_when_an_unrelated_source_file_is_deleted() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::remove_file(repo.path().join("c.ts")).unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after source file deletion",
    );

    let phases = phase_names(&output);
    assert!(phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(!phases.iter().any(|phase| phase == "file_discovery"));
}

#[test]
fn impact_deps_answers_from_the_index_when_an_unrelated_skip_worktree_file_is_missing() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    git(repo.path(), &["update-index", "--skip-worktree", "c.ts"]);
    fs::remove_file(repo.path().join("c.ts")).unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after missing skip-worktree source file",
    );

    let phases = phase_names(&output);
    assert!(phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(!phases.iter().any(|phase| phase == "file_discovery"));
}

#[cfg(unix)]
#[test]
fn impact_deps_answers_from_the_index_when_an_unrelated_symlink_target_is_missing() {
    let repo = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let symlink_target = external.path().join("linked.ts");
    init_symlink_source_repo(repo.path(), &symlink_target);

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    fs::remove_file(&symlink_target).unwrap();

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after missing symlink source target",
    );

    let phases = phase_names(&output);
    assert!(phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(!phases.iter().any(|phase| phase == "file_discovery"));
}

#[test]
fn impact_deps_index_fast_path_does_not_notice_a_side_effect_import_change() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_side_effect_import_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            // `SEM_BUILD_CACHE=1`: this warm-up exists to give the scaffolding
            // below a `cache.db` to doctor. `--deps`'s cold miss is
            // `CacheMissSavePolicy::IndexOnly` since and writes
            // `index.sem` only, which is the whole point of that change — so
            // the opt-in is what keeps this test testing what it is about
            // (the *index* tier's freshness characterization) instead of
            // silently becoming a test that the mirror still gets written.
            .env("SEM_BUILD_CACHE", "1")
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );
    mark_cache_as_topology_without_file_imports(cache.path());

    rewrite_after_mtime_tick(&repo.path().join("a.ts"), "console.log('changed');\n");

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TIMINGS", "json")
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "impact deps after side-effect import edit",
    );

    // CHARACTERIZATION, not an endorsement.
    // Until this change this test asserted the opposite, because it set
    // `SEM_NO_INDEX=1` and so exercised the SQLite tier's import-aware
    // freshness check (`has_fresh_dependency_impact_files`). That tier was
    // never reached in production: `try_index_impact_deps` has run *ahead* of
    // it since, and it proves freshness only over the entity's own
    // file and its known dependencies' files — a brand-new import *target*
    // touches neither, so the index answers, with the pre-change answer.
    // Deleting the SQLite tier did not cause this; it revealed it, and the
    // pre-change binary reproduces it identically. Pinned here so a future
    // repair flips the assertion deliberately rather than silently.
    let phases = phase_names(&output);
    assert!(phases.iter().any(|phase| phase == "index_impact_deps"));
    assert!(!phases.iter().any(|phase| phase == "file_discovery"));
}

#[test]
fn index_impact_file_hint_errors_match_graph_path() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "consume", "--file", "b.ts", "--deps", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    let missing = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .args(["impact", "missing", "--file", "b.ts", "--deps", "--json"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let missing_stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(missing_stderr.contains("Entity 'missing' not found"));
    assert!(!missing_stderr.contains("not found in file"));

    let wrong_file = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .args(["impact", "source", "--file", "b.ts", "--deps", "--json"])
        .output()
        .unwrap();
    assert!(!wrong_file.status.success());
    let wrong_file_stderr = String::from_utf8_lossy(&wrong_file.stderr);
    assert!(wrong_file_stderr.contains("Entity 'source' not found in file 'b.ts'"));
}

#[test]
fn impact_all_and_tests_match_no_cache_from_the_index() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_topology_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args(["impact", "source", "--file", "a.ts", "--json"])
            .output()
            .unwrap(),
        "warm impact cache",
    );
    mark_cache_as_topology_with_test_flags(cache.path());

    for extra_arg in [None, Some("--tests")] {
        let mut cached_args = vec!["impact", "source", "--file", "a.ts", "--json"];
        let mut no_cache_args = cached_args.clone();
        if let Some(extra_arg) = extra_arg {
            cached_args.push(extra_arg);
            no_cache_args.push(extra_arg);
        }
        no_cache_args.push("--no-cache");

        let cached = assert_success(
            Command::new(env!("CARGO_BIN_EXE_sem"))
                .current_dir(repo.path())
                .env("SEM_CACHE_DIR", cache.path())
                .env("SEM_TIMINGS", "json")
                .args(&cached_args)
                .output()
                .unwrap(),
            "cached topology impact",
        );
        let no_cache = assert_success(
            Command::new(env!("CARGO_BIN_EXE_sem"))
                .current_dir(repo.path())
                .args(&no_cache_args)
                .output()
                .unwrap(),
            "no-cache impact",
        );

        let cached_json: serde_json::Value = serde_json::from_slice(&cached.stdout).unwrap();
        let no_cache_json: serde_json::Value = serde_json::from_slice(&no_cache.stdout).unwrap();
        assert_eq!(cached_json, no_cache_json);

        let phases = phase_names(&cached);
        assert!(phases
            .iter()
            .any(|phase| phase == "index_impact_all" || phase == "index_impact_tests"));
        assert!(!phases.iter().any(|phase| phase == "full_graph_build"));
    }
}
