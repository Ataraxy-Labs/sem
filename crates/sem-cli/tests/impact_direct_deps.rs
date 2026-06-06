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
