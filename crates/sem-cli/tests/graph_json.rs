use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

static TEMP_REPO_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let counter = TEMP_REPO_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sem-graph-json-test-{}-{id}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp repo");
        run_git(&path, &["init", "-q"]);
        run_git(&path, &["config", "user.name", "Test"]);
        run_git(&path, &["config", "user.email", "test@example.com"]);
        run_git(&path, &["config", "commit.gpgsign", "false"]);
        Self { path }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_sem_graph_json(repo: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(["graph", ".", "--json", "--no-cache"])
        .current_dir(repo)
        .output()
        .expect("run sem graph");

    assert!(
        output.status.success(),
        "sem graph failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");

    serde_json::from_slice(&output.stdout).expect("parse graph json")
}

fn sorted(values: &[String]) -> Vec<String> {
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted
}

fn edge_key(edge: &Value) -> String {
    format!(
        "{}\0{}\0{}",
        edge["fromEntity"].as_str().expect("edge fromEntity"),
        edge["toEntity"].as_str().expect("edge toEntity"),
        edge["refType"].as_str().expect("edge refType")
    )
}

#[test]
fn graph_json_entities_and_edges_are_stably_ordered() {
    let repo = TempRepo::new();
    fs::write(
        repo.path.join("a.py"),
        r#"
def one():
    return two()

def two():
    return three()

def three():
    return 3

def four():
    return one()
"#,
    )
    .expect("write fixture");
    run_git(&repo.path, &["add", "-A"]);
    run_git(&repo.path, &["commit", "-q", "-m", "init"]);

    let first = run_sem_graph_json(&repo.path);
    let entities = first["entities"].as_array().expect("entities array");
    let entity_ids = entities
        .iter()
        .map(|entity| entity["id"].as_str().expect("entity id").to_owned())
        .collect::<Vec<_>>();
    assert!(entity_ids.len() >= 4);
    assert_eq!(entity_ids, sorted(&entity_ids));

    let edges = first["edges"].as_array().expect("edges array");
    let edge_keys = edges.iter().map(edge_key).collect::<Vec<_>>();
    assert!(!edge_keys.is_empty());
    assert_eq!(edge_keys, sorted(&edge_keys));

    for _ in 0..4 {
        assert_eq!(run_sem_graph_json(&repo.path), first);
    }
}
