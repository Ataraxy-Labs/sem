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
    cache_path: PathBuf,
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
        let cache_path = std::env::temp_dir().join(format!(
            "sem-graph-json-cache-{}-{id}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp repo");
        fs::create_dir_all(&cache_path).expect("create temp cache");
        run_git(&path, &["init", "-q"]);
        run_git(&path, &["config", "user.name", "Test"]);
        run_git(&path, &["config", "user.email", "test@example.com"]);
        run_git(&path, &["config", "commit.gpgsign", "false"]);
        Self { path, cache_path }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        let _ = fs::remove_dir_all(&self.cache_path);
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

fn run_sem_graph_json_stdout_with_args(
    repo: &Path,
    args: &[&str],
    cache_path: Option<&Path>,
) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sem"));
    command.args(args).current_dir(repo);
    if let Some(cache_path) = cache_path {
        command.env("SEM_CACHE_DIR", cache_path);
    }
    let output = command.output().expect("run sem graph");

    assert!(
        output.status.success(),
        "sem graph failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");

    String::from_utf8(output.stdout).expect("graph json stdout")
}

fn run_sem_graph_json_stdout(repo: &Path) -> String {
    run_sem_graph_json_stdout_with_args(repo, &["graph", ".", "--json", "--no-cache"], None)
}

fn run_cached_sem_graph_json_stdout(repo: &TempRepo) -> String {
    run_sem_graph_json_stdout_with_args(
        &repo.path,
        &["graph", ".", "--json"],
        Some(&repo.cache_path),
    )
}

fn run_sem_graph_json(repo: &Path) -> Value {
    serde_json::from_str(&run_sem_graph_json_stdout(repo)).expect("parse graph json")
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

fn write_ambiguous_constructor_fixture(repo: &TempRepo, primary_prefix: &str) {
    fs::write(
        repo.path.join("a_primary.py"),
        format!(
            r#"{primary_prefix}
class Primary:
    def get(self):
        return True

def make_conn():
    return Primary()
"#
        ),
    )
    .expect("write primary fixture");
    fs::write(
        repo.path.join("holder.py"),
        r#"
class Holder:
    def __init__(self, conn):
        self.conn = conn

    def use(self):
        return self.conn.get()

def wire():
    Holder(make_conn())
"#,
    )
    .expect("write holder fixture");
    fs::write(
        repo.path.join("z_backup.py"),
        r#"
class Backup:
    def get(self):
        return False

def make_conn():
    return Backup()
"#,
    )
    .expect("write backup fixture");
}

fn assert_holder_uses_primary_get(graph_json: &Value) {
    let edges = graph_json["edges"].as_array().expect("edges array");

    assert!(
        edges.iter().any(|edge| {
            edge["fromEntity"]
                .as_str()
                .map_or(false, |from| from.contains("Holder::use"))
                && edge["toEntity"] == "a_primary.py::class::Primary::get"
                && edge["refType"] == "calls"
        }),
        "Holder.use should resolve conn.get to Primary.get: {edges:?}"
    );
    assert!(
        !edges.iter().any(|edge| {
            edge["fromEntity"]
                .as_str()
                .map_or(false, |from| from.contains("Holder::use"))
                && edge["toEntity"] == "z_backup.py::class::Backup::get"
        }),
        "Holder.use should not resolve conn.get to Backup.get: {edges:?}"
    );
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

#[test]
fn graph_json_is_stable_for_ambiguous_constructor_resolution() {
    let repo = TempRepo::new();
    write_ambiguous_constructor_fixture(&repo, "");
    run_git(&repo.path, &["add", "-A"]);
    run_git(&repo.path, &["commit", "-q", "-m", "init"]);

    let first_stdout = run_sem_graph_json_stdout(&repo.path);
    let first: Value = serde_json::from_str(&first_stdout).expect("parse first graph json");
    assert_holder_uses_primary_get(&first);

    for _ in 0..8 {
        assert_eq!(run_sem_graph_json_stdout(&repo.path), first_stdout);
    }
}

#[test]
fn graph_json_cached_incremental_matches_full_for_ambiguous_constructor_resolution() {
    let repo = TempRepo::new();
    write_ambiguous_constructor_fixture(&repo, "");
    run_git(&repo.path, &["add", "-A"]);
    run_git(&repo.path, &["commit", "-q", "-m", "init"]);

    let cached_full_stdout = run_cached_sem_graph_json_stdout(&repo);
    let uncached_full_stdout = run_sem_graph_json_stdout(&repo.path);
    assert_eq!(cached_full_stdout, uncached_full_stdout);

    write_ambiguous_constructor_fixture(&repo, "# force a cached incremental rebuild");

    let cached_incremental_stdout = run_cached_sem_graph_json_stdout(&repo);
    let uncached_after_change_stdout = run_sem_graph_json_stdout(&repo.path);
    assert_eq!(cached_incremental_stdout, uncached_after_change_stdout);

    let cached_incremental: Value =
        serde_json::from_str(&cached_incremental_stdout).expect("parse cached graph json");
    assert_holder_uses_primary_get(&cached_incremental);

    for _ in 0..4 {
        assert_eq!(
            run_cached_sem_graph_json_stdout(&repo),
            cached_incremental_stdout
        );
    }
}
