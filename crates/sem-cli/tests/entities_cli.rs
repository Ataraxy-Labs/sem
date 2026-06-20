use std::{collections::HashMap, fs, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn run_sem_entities_json(repo: &TempDir) -> (Value, Value) {
    run_sem_entities_json_with_args(repo, &["entities", ".", "--json"])
}

fn run_sem_entities_json_with_args(repo: &TempDir, args: &[&str]) -> (Value, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .env("SEM_TIMINGS", "json")
        .args(args)
        .output()
        .expect("run sem entities");

    assert!(
        output.status.success(),
        "sem entities failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = serde_json::from_slice(&output.stdout).expect("entities stdout json");
    let stderr = String::from_utf8(output.stderr).expect("timings stderr utf8");
    let timings = serde_json::from_str(stderr.trim()).expect("timings stderr json");
    (stdout, timings)
}

fn run_sem_entities_json_value_with_args(repo: &TempDir, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .args(args)
        .output()
        .expect("run sem entities");

    assert!(
        output.status.success(),
        "sem entities failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");

    serde_json::from_slice(&output.stdout).expect("entities stdout json")
}

fn run_sem_graph_json(repo: &TempDir, cache: &TempDir) {
    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .env("SEM_CACHE_DIR", cache.path())
        .args(["graph", ".", "--json"])
        .output()
        .expect("run sem graph");

    assert!(
        output.status.success(),
        "sem graph failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_cached_sem_entities_json(repo: &TempDir, cache: &TempDir) -> (Value, Value) {
    run_cached_sem_entities_json_with_args(repo, cache, &["entities", ".", "--json"])
}

fn run_cached_sem_entities_json_with_args(
    repo: &TempDir,
    cache: &TempDir,
    args: &[&str],
) -> (Value, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("DO_NOT_TRACK", "1")
        .env("SEM_LOCAL", "1")
        .env("SEM_CACHE_DIR", cache.path())
        .env("SEM_TIMINGS", "json")
        .args(args)
        .output()
        .expect("run sem entities");

    assert!(
        output.status.success(),
        "sem entities failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = serde_json::from_slice(&output.stdout).expect("entities stdout json");
    let stderr = String::from_utf8(output.stderr).expect("timings stderr utf8");
    let timings = serde_json::from_str(stderr.trim()).expect("timings stderr json");
    (stdout, timings)
}

#[test]
fn entities_json_emits_timings_and_counters() {
    let repo = TempDir::new().expect("temp repo");
    fs::write(
        repo.path().join("a.ts"),
        "export function alpha() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("b.ts"),
        "export const beta = () => alpha();\n",
    )
    .unwrap();

    let (entities, timings) = run_sem_entities_json(&repo);
    let entities = entities.as_array().expect("entities array");
    assert!(entities.iter().any(|entity| entity["name"] == "alpha"));
    assert!(entities.iter().any(|entity| entity["name"] == "beta"));

    assert_eq!(timings["command"], "entities");
    let phase_names = timings["phases"]
        .as_array()
        .expect("phases array")
        .iter()
        .map(|phase| phase["name"].as_str().expect("phase name"))
        .collect::<Vec<_>>();
    for expected in [
        "path_args",
        "file_discovery",
        "extract_entities",
        "sort_dedup",
        "output_serialization",
    ] {
        assert!(
            phase_names.contains(&expected),
            "missing phase {expected}; got {phase_names:?}"
        );
    }

    let counters = timings["counters"]
        .as_array()
        .expect("counters array")
        .iter()
        .map(|counter| {
            (
                counter["name"].as_str().expect("counter name"),
                counter["value"].as_u64().expect("counter value"),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(counters["input_paths"], 1);
    assert_eq!(counters["input_dirs"], 1);
    assert_eq!(counters["input_files"], 2);
    assert_eq!(counters["input_file_args"], 0);
    assert_eq!(counters["processed_files"], 2);
    assert_eq!(counters["discovered_files"], 2);
    assert_eq!(counters["entities"], entities.len() as u64);
    assert!(counters["json_bytes"] > 0);
}

#[test]
fn entities_json_counts_explicit_file_inputs_separately() {
    let repo = TempDir::new().expect("temp repo");
    fs::write(
        repo.path().join("a.ts"),
        "export function alpha() { return 1; }\n",
    )
    .unwrap();

    let (entities, timings) =
        run_sem_entities_json_with_args(&repo, &["entities", "a.ts", "--json"]);
    let entities = entities.as_array().expect("entities array");
    assert!(entities.iter().any(|entity| entity["name"] == "alpha"));

    let counters = timings["counters"]
        .as_array()
        .expect("counters array")
        .iter()
        .map(|counter| {
            (
                counter["name"].as_str().expect("counter name"),
                counter["value"].as_u64().expect("counter value"),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(counters["input_paths"], 1);
    assert_eq!(counters["input_dirs"], 0);
    assert_eq!(counters["input_files"], 1);
    assert_eq!(counters["input_file_args"], 1);
    assert_eq!(counters["processed_files"], 1);
    assert_eq!(counters["discovered_files"], 0);
    assert_eq!(counters["entities"], entities.len() as u64);
}

#[test]
fn entities_file_exts_filter_directory_scans() {
    let repo = TempDir::new().expect("temp repo");
    fs::write(
        repo.path().join("a.ts"),
        "export function alpha() { return 1; }\n",
    )
    .unwrap();
    fs::write(repo.path().join("config.json"), r#"{"ignored": true}"#).unwrap();

    let entities = run_sem_entities_json_value_with_args(
        &repo,
        &["entities", ".", "--json", "--file-exts", ".ts"],
    );
    let entities = entities.as_array().expect("entities array");

    assert!(entities.iter().any(|entity| entity["name"] == "alpha"));
    assert!(
        entities.iter().all(|entity| entity["file"]
            .as_str()
            .is_some_and(|file| file.ends_with(".ts"))),
        "expected only .ts entities, got {entities:?}"
    );

    let bare_ext_entities = run_sem_entities_json_value_with_args(
        &repo,
        &["entities", ".", "--json", "--file-exts", "ts"],
    );
    assert_eq!(bare_ext_entities, Value::Array(entities.clone()));
}

#[test]
fn entities_uses_fresh_topology_cache() {
    let repo = TempDir::new().expect("temp repo");
    let cache = TempDir::new().expect("temp cache");
    fs::write(
        repo.path().join("a.ts"),
        "export function alpha() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("b.ts"),
        "export function beta() { return alpha(); }\n",
    )
    .unwrap();

    run_sem_graph_json(&repo, &cache);
    let (entities, timings) = run_cached_sem_entities_json(&repo, &cache);
    let entities = entities.as_array().expect("entities array");
    assert!(entities.iter().any(|entity| entity["name"] == "alpha"));
    assert!(entities.iter().any(|entity| entity["name"] == "beta"));

    let phase_names = timings["phases"]
        .as_array()
        .expect("phases array")
        .iter()
        .map(|phase| phase["name"].as_str().expect("phase name"))
        .collect::<Vec<_>>();
    assert!(
        phase_names.contains(&"cache_entities_query"),
        "expected cache hit phase, got {phase_names:?}"
    );
    assert!(
        !phase_names.contains(&"extract_entities"),
        "cache hit should not parse files; got {phase_names:?}"
    );
    assert!(
        !phase_names.contains(&"sort_dedup"),
        "streamed cache hit should not materialize and sort entities; got {phase_names:?}"
    );

    let counters = timings["counters"]
        .as_array()
        .expect("counters array")
        .iter()
        .map(|counter| {
            (
                counter["name"].as_str().expect("counter name"),
                counter["value"].as_u64().expect("counter value"),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(counters["cached_entities"], entities.len() as u64);
}

#[test]
fn entities_uses_whole_repo_cache_for_subdirectory_listing() {
    let repo = TempDir::new().expect("temp repo");
    let cache = TempDir::new().expect("temp cache");
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(
        repo.path().join("src").join("a.ts"),
        "export function alpha() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("tests").join("b.ts"),
        "export function beta() { return 2; }\n",
    )
    .unwrap();

    run_sem_graph_json(&repo, &cache);
    let (entities, timings) =
        run_cached_sem_entities_json_with_args(&repo, &cache, &["entities", "src", "--json"]);
    let entities = entities.as_array().expect("entities array");
    assert!(entities.iter().any(|entity| entity["name"] == "alpha"));
    assert!(!entities.iter().any(|entity| entity["name"] == "beta"));
    assert!(
        entities.iter().all(|entity| entity["file"]
            .as_str()
            .is_some_and(|file| file.starts_with("src/"))),
        "expected only src entities, got {entities:?}"
    );

    let phase_names = timings["phases"]
        .as_array()
        .expect("phases array")
        .iter()
        .map(|phase| phase["name"].as_str().expect("phase name"))
        .collect::<Vec<_>>();
    assert!(
        phase_names.contains(&"cache_entities_query"),
        "expected cache hit phase, got {phase_names:?}"
    );
    assert!(
        !phase_names.contains(&"extract_entities"),
        "cache hit should not parse files; got {phase_names:?}"
    );

    let counters = timings["counters"]
        .as_array()
        .expect("counters array")
        .iter()
        .map(|counter| {
            (
                counter["name"].as_str().expect("counter name"),
                counter["value"].as_u64().expect("counter value"),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(counters["cached_entities"], entities.len() as u64);
    assert_eq!(counters["input_files"], 1);
    assert_eq!(counters["discovered_files"], 1);
}
