use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rusqlite::{params, Connection};
use sem_mcp::cache::CACHE_SCHEMA_VERSION;

const PREVIOUS_CACHE_SCHEMA_VERSION: i32 = 1;

fn sem_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sem")
}

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

fn assert_failure(output: Output, context: &str) -> Output {
    assert!(
        !output.status.success(),
        "{context} unexpectedly succeeded\n{}",
        output_text(&output)
    );
    output
}

fn run_git(repo: &Path, args: &[&str]) -> Output {
    assert_success(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap(),
        &format!("git {}", args.join(" ")),
    )
}

fn run_sem(repo: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(sem_bin());
    command.args(args).current_dir(repo).env("NO_COLOR", "1");
    command.output().unwrap()
}

fn init_repo(repo: &Path) {
    assert_success(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo)
            .output()
            .unwrap(),
        "git init",
    );
    run_git(repo, &["config", "user.email", "t@t.com"]);
    run_git(repo, &["config", "user.name", "test"]);
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

fn assert_verify_reports_target_mismatch(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""caller": "use_it""#), "{stdout}");
    assert!(stdout.contains(r#""callee": "target""#), "{stdout}");
    assert!(stdout.contains(r#""expected_min": 1"#), "{stdout}");
    assert!(stdout.contains(r#""expected_max": 1"#), "{stdout}");
    assert!(stdout.contains(r#""actual_args": 3"#), "{stdout}");
}

fn file_mtime_parts(path: &Path) -> (i64, i64) {
    let mtime = fs::metadata(path).unwrap().modified().unwrap();
    let dur = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_secs() as i64, dur.subsec_nanos() as i64)
}

fn seed_v1_bad_full_cache(repo: &Path) {
    assert!(CACHE_SCHEMA_VERSION > PREVIOUS_CACHE_SCHEMA_VERSION);

    let cache_dir = repo.join(".sem");
    fs::create_dir_all(&cache_dir).unwrap();
    let conn = Connection::open(cache_dir.join("cache.db")).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA user_version = {PREVIOUS_CACHE_SCHEMA_VERSION};
         CREATE TABLE files (
             path TEXT PRIMARY KEY,
             mtime_secs INTEGER NOT NULL,
             mtime_nanos INTEGER NOT NULL
         );
         CREATE TABLE entities (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             entity_type TEXT NOT NULL,
             file_path TEXT NOT NULL,
             start_line INTEGER NOT NULL,
             end_line INTEGER NOT NULL,
             content TEXT NOT NULL,
             content_hash TEXT NOT NULL,
             structural_hash TEXT,
             parent_id TEXT,
             metadata_json TEXT
         );
         CREATE TABLE edges (
             from_entity TEXT NOT NULL,
             to_entity TEXT NOT NULL,
             ref_type TEXT NOT NULL
         );"
    ))
    .unwrap();

    let mut files = conn
        .prepare("INSERT INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)")
        .unwrap();
    for file in ["a.py", "b.py"] {
        let (secs, nanos) = file_mtime_parts(&repo.join(file));
        files.execute(params![file, secs, nanos]).unwrap();
    }
    drop(files);

    let mut entities = conn
        .prepare(
            "INSERT INTO entities (
                id, name, entity_type, file_path, start_line, end_line,
                content, content_hash, structural_hash, parent_id, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL)",
        )
        .unwrap();
    entities
        .execute(params![
            "a.py::function::use_it",
            "use_it",
            "function",
            "a.py",
            1_i64,
            2_i64,
            "def use_it():\n    return target(1, 2, 3)\n",
            "old-use-it"
        ])
        .unwrap();
    entities
        .execute(params![
            "b.py::function::other",
            "other",
            "function",
            "b.py",
            1_i64,
            2_i64,
            "def other():\n    return 0\n",
            "old-other"
        ])
        .unwrap();
    entities
        .execute(params![
            "b.py::function::target",
            "target",
            "function",
            "b.py",
            4_i64,
            5_i64,
            "def target(a):\n    return a\n",
            "old-target"
        ])
        .unwrap();
}

#[test]
fn verify_incremental_cache_rechecks_clean_callers_for_new_callees() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();

    init_repo(repo);

    fs::write(repo.join("b.py"), "def other():\n    return 0\n").unwrap();
    fs::write(
        repo.join("a.py"),
        "def use_it():\n    return target(1, 2, 3)\n",
    )
    .unwrap();
    run_git(repo, &["add", "a.py", "b.py"]);
    run_git(repo, &["commit", "-q", "-m", "init"]);

    assert_success(run_sem(repo, &["graph", "--json"]), "sem graph");

    rewrite_after_mtime_tick(
        &repo.join("b.py"),
        "def other():\n    return 0\n\n\ndef target(a):\n    return a\n",
    );

    let cached = assert_failure(run_sem(repo, &["verify", "--json"]), "sem verify --json");
    assert_verify_reports_target_mismatch(&cached);

    let uncached = assert_failure(
        run_sem(repo, &["verify", "--json", "--no-cache"]),
        "sem verify --json --no-cache",
    );
    assert_verify_reports_target_mismatch(&uncached);
}

#[test]
fn verify_rebuilds_v1_full_cache_hits() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();

    init_repo(repo);

    fs::write(
        repo.join("a.py"),
        "def use_it():\n    return target(1, 2, 3)\n",
    )
    .unwrap();
    fs::write(
        repo.join("b.py"),
        "def other():\n    return 0\n\n\ndef target(a):\n    return a\n",
    )
    .unwrap();
    run_git(repo, &["add", "a.py", "b.py"]);
    run_git(repo, &["commit", "-q", "-m", "init"]);

    seed_v1_bad_full_cache(repo);

    let output = assert_failure(run_sem(repo, &["verify", "--json"]), "sem verify --json");
    assert_verify_reports_target_mismatch(&output);
}
