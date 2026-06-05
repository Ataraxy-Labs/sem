use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("sem-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    dir
}

fn run_sem_json(dir: &PathBuf, home: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .output()
        .expect("sem should run")
}

fn run_git(dir: &PathBuf, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git {} failed\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(dir: &PathBuf) {
    run_git(dir, &["init", "-q"]);
    run_git(dir, &["config", "user.email", "a@b.co"]);
    run_git(dir, &["config", "user.name", "a"]);
}

fn commit_all(dir: &PathBuf, message: &str) {
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-qm", message]);
}

#[test]
fn ref_plus_path_positional_arg_filters_to_pathspec() {
    let dir = temp_dir("ref-plus-path-positional");
    let home = temp_dir("ref-plus-path-positional-home");
    init_repo(&dir);
    fs::write(dir.join("app.py"), "def foo():\n    return 1\n")
        .expect("source file should be written");
    fs::write(dir.join("other.py"), "def bar():\n    return 1\n")
        .expect("source file should be written");
    run_git(&dir, &["add", "."]);
    run_git(&dir, &["commit", "-qm", "c1"]);

    fs::write(dir.join("app.py"), "def foo():\n    return 2\n")
        .expect("source file should be written");
    fs::write(dir.join("other.py"), "def bar():\n    return 2\n")
        .expect("source file should be written");

    let output = run_sem_json(&dir, &home, &["diff", "HEAD", "app.py", "--format", "json"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert!(!changes.is_empty(), "{json:?}");
    assert!(
        changes
            .iter()
            .all(|change| change["filePath"].as_str() == Some("app.py")),
        "{changes:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn ref_plus_missing_path_reports_an_error() {
    let dir = temp_dir("ref-plus-missing-path");
    let home = temp_dir("ref-plus-missing-path-home");
    init_repo(&dir);
    fs::write(dir.join("app.py"), "def foo():\n    return 1\n")
        .expect("source file should be written");
    run_git(&dir, &["add", "."]);
    run_git(&dir, &["commit", "-qm", "c1"]);

    let output = run_sem_json(
        &dir,
        &home,
        &["diff", "HEAD", "missing.py", "--format", "json"],
    );
    assert!(
        !output.status.success(),
        "sem unexpectedly succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing.py"), "{stderr}");

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn two_ref_positional_args_remain_range() {
    let dir = temp_dir("two-ref-positional-range");
    let home = temp_dir("two-ref-positional-range-home");
    init_repo(&dir);
    fs::write(dir.join("app.py"), "def foo():\n    return 1\n")
        .expect("source file should be written");
    run_git(&dir, &["add", "."]);
    run_git(&dir, &["commit", "-qm", "c1"]);
    fs::write(dir.join("app.py"), "def foo():\n    return 2\n")
        .expect("source file should be written");
    run_git(&dir, &["commit", "-qam", "c2"]);
    fs::write(dir.join("app.py"), "def foo():\n    return 3\n")
        .expect("source file should be written");

    let output = run_sem_json(&dir, &home, &["diff", "HEAD~1", "HEAD", "--format", "json"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    let change = changes
        .iter()
        .find(|change| change["filePath"].as_str() == Some("app.py"))
        .expect("app.py change should be present");
    assert!(
        change["afterContent"]
            .as_str()
            .is_some_and(|content| content.contains("return 2")),
        "{change:?}"
    );
    assert!(
        !change["afterContent"]
            .as_str()
            .is_some_and(|content| content.contains("return 3")),
        "{change:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn ambiguous_single_name_warns_and_uses_pathspec() {
    let dir = temp_dir("ambiguous-single-name");
    let home = temp_dir("ambiguous-single-name-home");
    init_repo(&dir);
    fs::write(dir.join("app.py"), "def foo():\n    return 1\n")
        .expect("source file should be written");
    run_git(&dir, &["add", "."]);
    run_git(&dir, &["commit", "-qm", "c1"]);
    fs::write(dir.join("app.py"), "def foo():\n    return 2\n")
        .expect("source file should be written");
    run_git(&dir, &["commit", "-qam", "c2"]);
    run_git(&dir, &["branch", "app.py", "HEAD~1"]);
    fs::write(dir.join("app.py"), "def foo():\n    return 3\n")
        .expect("source file should be written");

    let output = run_sem_json(&dir, &home, &["diff", "app.py", "--format", "json"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("both a git revision and a path")
            && stderr.contains("treating it as a pathspec"),
        "{stderr}"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert!(!changes.is_empty(), "{json:?}");
    assert!(
        changes
            .iter()
            .all(|change| change["filePath"].as_str() == Some("app.py")),
        "{changes:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn cross_language_file_compare_uses_each_side_path() {
    let dir = temp_dir("cross-language-file-compare");
    let home = temp_dir("cross-language-file-compare-home");
    fs::write(
        dir.join("a.ts"),
        "function foo(x: number) { return x + 1; }\n",
    )
    .expect("source file should be written");
    fs::write(dir.join("b.py"), "def foo(x): return x + 1\n")
        .expect("target file should be written");

    let output = run_sem_json(&dir, &home, &["diff", "a.ts", "b.py", "--format", "json"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("different languages"), "{stderr}");
    assert!(stderr.contains("TypeScript"), "{stderr}");
    assert!(stderr.contains("Python"), "{stderr}");

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");

    let deleted = changes
        .iter()
        .find(|change| change["changeType"].as_str() == Some("deleted"))
        .expect("deleted TypeScript change should be present");
    assert_eq!(deleted["filePath"].as_str(), Some("a.ts"));
    assert!(
        deleted["entityId"]
            .as_str()
            .is_some_and(|entity_id| entity_id.starts_with("a.ts::")),
        "{deleted:?}"
    );
    assert!(
        deleted["beforeContent"]
            .as_str()
            .is_some_and(|content| content.contains("function foo")),
        "{deleted:?}"
    );
    assert!(deleted["afterContent"].is_null(), "{deleted:?}");

    let added = changes
        .iter()
        .find(|change| change["changeType"].as_str() == Some("added"))
        .expect("added Python change should be present");
    assert_eq!(added["filePath"].as_str(), Some("b.py"));
    assert!(
        added["entityId"]
            .as_str()
            .is_some_and(|entity_id| entity_id.starts_with("b.py::")),
        "{added:?}"
    );
    assert!(added["beforeContent"].is_null(), "{added:?}");
    assert!(
        added["afterContent"]
            .as_str()
            .is_some_and(|content| content.contains("def foo")),
        "{added:?}"
    );
    assert!(
        !changes.iter().any(|change| {
            change["filePath"].as_str() == Some("b.py")
                && change["beforeContent"]
                    .as_str()
                    .is_some_and(|content| content.contains("function foo"))
        }),
        "{changes:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn same_line_overload_deletion_is_reported() {
    let dir = temp_dir("same-line-overload-deletion");
    let home = temp_dir("same-line-overload-deletion-home");
    init_repo(&dir);
    fs::write(
        dir.join("over.ts"),
        "function f(a: number): void {}; function f(a: string): void {}\n",
    )
    .expect("source file should be written");
    commit_all(&dir, "init");

    let graph_output = run_sem_json(&dir, &home, &["graph", "--json", "--no-cache"]);
    assert!(
        graph_output.status.success(),
        "sem graph failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&graph_output.stdout),
        String::from_utf8_lossy(&graph_output.stderr)
    );
    let graph_json: serde_json::Value =
        serde_json::from_slice(&graph_output.stdout).expect("graph stdout should be json");
    assert_eq!(graph_json["stats"]["entityCount"].as_u64(), Some(2));

    fs::write(dir.join("over.ts"), "function f(a: number): void {};\n")
        .expect("source file should be updated");
    let output = run_sem_json(&dir, &home, &["diff", "over.ts", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["summary"]["deleted"].as_u64(), Some(1), "{json:?}");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    let deleted = changes
        .iter()
        .find(|change| change["changeType"].as_str() == Some("deleted"))
        .expect("deleted overload should be present");
    assert_eq!(deleted["entityName"].as_str(), Some("f"));
    assert!(
        deleted["entityId"]
            .as_str()
            .is_some_and(|entity_id| entity_id.ends_with("@L1#2")),
        "{deleted:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn same_line_overload_deletion_with_survivor_edit_is_reported() {
    let dir = temp_dir("same-line-overload-delete-edit");
    let home = temp_dir("same-line-overload-delete-edit-home");
    init_repo(&dir);
    fs::write(
        dir.join("over.ts"),
        "function f(a: number): void {}; function f(a: string): void {}\n",
    )
    .expect("source file should be written");
    commit_all(&dir, "init");

    fs::write(
        dir.join("over.ts"),
        "function f(a: number): void { console.log(a); }\n",
    )
    .expect("source file should be updated");
    let output = run_sem_json(&dir, &home, &["diff", "over.ts", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["summary"]["modified"].as_u64(), Some(1), "{json:?}");
    assert_eq!(json["summary"]["deleted"].as_u64(), Some(1), "{json:?}");
    assert_eq!(json["summary"]["total"].as_u64(), Some(2), "{json:?}");

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn same_line_overload_insertion_with_survivor_edit_is_reported() {
    let dir = temp_dir("same-line-overload-insert-edit");
    let home = temp_dir("same-line-overload-insert-edit-home");
    init_repo(&dir);
    fs::write(
        dir.join("over.ts"),
        "function f(): void { return oldValue + stableThing; }\n",
    )
    .expect("source file should be written");
    commit_all(&dir, "init");

    fs::write(
        dir.join("over.ts"),
        "function f(): void { totallyDifferentAlphaBetaGamma(); }; function f(): void { return oldValue + stableThing + changedThing; }\n",
    )
    .expect("source file should be updated");
    let output = run_sem_json(&dir, &home, &["diff", "over.ts", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["summary"]["modified"].as_u64(), Some(1), "{json:?}");
    assert_eq!(json["summary"]["added"].as_u64(), Some(1), "{json:?}");
    assert_eq!(json["summary"]["total"].as_u64(), Some(2), "{json:?}");

    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    let modified = changes
        .iter()
        .find(|change| change["changeType"].as_str() == Some("modified"))
        .expect("modified survivor should be present");
    let added = changes
        .iter()
        .find(|change| change["changeType"].as_str() == Some("added"))
        .expect("added duplicate should be present");
    assert!(
        modified["entityId"]
            .as_str()
            .is_some_and(|entity_id| entity_id.ends_with("@L1#2")),
        "{modified:?}"
    );
    assert!(
        added["entityId"]
            .as_str()
            .is_some_and(|entity_id| entity_id.ends_with("@L1#1")),
        "{added:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn same_line_edit_does_not_report_unchanged_entities_as_reordered() {
    let dir = temp_dir("same-line-edit-no-reorder");
    let home = temp_dir("same-line-edit-no-reorder-home");
    init_repo(&dir);
    fs::write(
        dir.join("min.js"),
        "function a(){return 1} function b(){return 2} function c(){return 3} function d(){return 4}\n",
    )
    .expect("source file should be written");
    commit_all(&dir, "init");

    fs::write(
        dir.join("min.js"),
        "function a(){return 1} function b(){return 2} function c(){return 999} function d(){return 4}\n",
    )
    .expect("source file should be updated");
    let output = run_sem_json(&dir, &home, &["diff", "min.js", "--json"]);
    assert!(
        output.status.success(),
        "sem diff failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["summary"]["modified"].as_u64(), Some(1), "{json:?}");
    assert_eq!(json["summary"]["reordered"].as_u64(), Some(0), "{json:?}");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["changeType"].as_str(), Some("modified"));
    assert_eq!(changes[0]["entityName"].as_str(), Some("c"));

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn same_language_file_compare_keeps_modified_target_namespace() {
    let dir = temp_dir("same-language-file-compare");
    let home = temp_dir("same-language-file-compare-home");
    fs::write(dir.join("a.ts"), "function foo() { return 1; }\n")
        .expect("source file should be written");
    fs::write(dir.join("b.ts"), "function foo() { return 2; }\n")
        .expect("target file should be written");

    let output = run_sem_json(&dir, &home, &["diff", "a.ts", "b.ts", "--format", "json"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("different languages"), "{stderr}");

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert_eq!(changes.len(), 1, "{changes:?}");
    let change = &changes[0];
    assert_eq!(change["changeType"].as_str(), Some("modified"));
    assert_eq!(change["filePath"].as_str(), Some("b.ts"));
    assert!(
        change["entityId"]
            .as_str()
            .is_some_and(|entity_id| entity_id.starts_with("b.ts::")),
        "{change:?}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn trailing_format_requires_value_before_another_flag() {
    let dir = temp_dir("trailing-format-missing-value");
    let home = temp_dir("trailing-format-missing-value-home");
    fs::write(dir.join("a.ts"), "function foo() { return 1; }\n")
        .expect("source file should be written");
    fs::write(dir.join("b.ts"), "function foo() { return 2; }\n")
        .expect("target file should be written");

    let output = run_sem_json(&dir, &home, &["diff", "a.ts", "b.ts", "--format", "--json"]);
    assert!(
        !output.status.success(),
        "sem unexpectedly succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--format") && stderr.contains("value"),
        "expected --format value error, got: {stderr}"
    );

    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(home);
}
