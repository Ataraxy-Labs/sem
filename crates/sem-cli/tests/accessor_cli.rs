use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn git(repo: &TempDir, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo.path())
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn sem(repo: &TempDir, home: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("HOME", home.path())
        .args(args)
        .output()
        .expect("sem should run")
}

#[test]
fn context_and_impact_accept_type_qualified_accessor_queries() {
    let repo = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    git(&repo, &["init", "-q"]);

    fs::write(
        repo.path().join("box.ts"),
        r#"export class Box {
  private _v = 0;
  get value(): number { return this._v; }
  set value(n: number) { this._v = n; }
}
"#,
    )
    .unwrap();

    let context = sem(
        &repo,
        &home,
        &[
            "context",
            "getter value",
            "--file",
            "box.ts",
            "--json",
            "--no-cache",
        ],
    );
    assert!(
        context.status.success(),
        "sem context failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&context.stdout),
        String::from_utf8_lossy(&context.stderr)
    );
    let context_json: serde_json::Value = serde_json::from_slice(&context.stdout).unwrap();
    assert_eq!(context_json["entity"].as_str(), Some("value"));
    assert_eq!(
        context_json["entries"][0]["type"].as_str(),
        Some("getter"),
        "{context_json:?}"
    );
    assert!(
        context_json["entityId"]
            .as_str()
            .is_some_and(|id| id.contains("::value@L3")),
        "{context_json:?}"
    );

    let impact = sem(
        &repo,
        &home,
        &[
            "impact",
            "setter value",
            "--file",
            "box.ts",
            "--json",
            "--no-cache",
        ],
    );
    assert!(
        impact.status.success(),
        "sem impact failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&impact.stdout),
        String::from_utf8_lossy(&impact.stderr)
    );
    let impact_json: serde_json::Value = serde_json::from_slice(&impact.stdout).unwrap();
    assert_eq!(impact_json["entity"]["type"].as_str(), Some("setter"));
    assert!(
        impact_json["entity"]["entityId"]
            .as_str()
            .is_some_and(|id| id.contains("::value@L4")),
        "{impact_json:?}"
    );
}
