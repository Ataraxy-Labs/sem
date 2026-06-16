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
    git(repo, &["add", "a.ts", "b.ts"]);
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
                "--file-exts",
                ".ts",
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
fn impact_deps_uses_cached_sql_topology_query_on_second_run() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args([
                "impact",
                "consume",
                "--file",
                "b.ts",
                "--deps",
                "--json",
                "--file-exts",
                ".ts",
            ])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
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
                ".ts",
            ])
            .output()
            .unwrap(),
        "cached impact deps",
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["entity"]["name"], "consume");
    assert_eq!(json["dependencies"][0]["name"], "source");

    let phases = phase_names(&output);
    assert!(phases
        .iter()
        .any(|phase| phase == "cache_topology_impact_query"));
    assert!(!phases.iter().any(|phase| phase == "cache_topology_load"));
    assert!(!phases.iter().any(|phase| phase == "full_graph_build"));
}

#[test]
fn cached_impact_file_hint_errors_match_graph_path() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args([
                "impact",
                "consume",
                "--file",
                "b.ts",
                "--deps",
                "--json",
                "--file-exts",
                ".ts",
            ])
            .output()
            .unwrap(),
        "warm impact cache",
    );

    let missing = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .args([
            "impact",
            "missing",
            "--file",
            "b.ts",
            "--deps",
            "--json",
            "--file-exts",
            ".ts",
        ])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let missing_stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(missing_stderr.contains("Entity 'missing' not found"));
    assert!(!missing_stderr.contains("not found in file"));

    let wrong_file = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .args([
            "impact",
            "source",
            "--file",
            "b.ts",
            "--deps",
            "--json",
            "--file-exts",
            ".ts",
        ])
        .output()
        .unwrap();
    assert!(!wrong_file.status.success());
    let wrong_file_stderr = String::from_utf8_lossy(&wrong_file.stderr);
    assert!(wrong_file_stderr.contains("Entity 'source' not found in file 'b.ts'"));
}

#[test]
fn impact_all_and_tests_match_no_cache_from_topology_cache() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_topology_repo(repo.path());

    assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .args([
                "impact",
                "source",
                "--file",
                "a.ts",
                "--json",
                "--file-exts",
                ".ts",
            ])
            .output()
            .unwrap(),
        "warm impact cache",
    );
    mark_cache_as_topology_with_test_flags(cache.path());

    for extra_arg in [None, Some("--tests")] {
        let mut cached_args = vec![
            "impact",
            "source",
            "--file",
            "a.ts",
            "--json",
            "--file-exts",
            ".ts",
        ];
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
            .any(|phase| phase == "cache_topology_impact_query"));
        assert!(!phases.iter().any(|phase| phase == "full_graph_build"));
    }
}
