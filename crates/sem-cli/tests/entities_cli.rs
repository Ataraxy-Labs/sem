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
