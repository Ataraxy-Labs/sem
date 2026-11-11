use std::io::Write;
use std::process::{Command, Stdio};

fn run_sem_diff_stdin(input: &str, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sem"))
        .arg("diff")
        .arg("--stdin")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).unwrap();
    drop(stdin);

    child.wait_with_output().unwrap()
}

#[test]
fn diff_stdin_rejects_unknown_file_change_fields() {
    let output = run_sem_diff_stdin(
        r#"[{"filePath":"b.ts","oldPath":"a.ts","status":"renamed","beforeContent":"function foo(){}","afterContent":"function foo(){}"}]"#,
        &[],
    );

    assert!(!output.status.success());

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Error parsing stdin JSON"));
    assert!(stderr.contains("unknown field `oldPath`"));
}

#[test]
fn diff_stdin_accepts_old_file_path_for_renames() {
    let output = run_sem_diff_stdin(
        r#"[{"filePath":"b.ts","oldFilePath":"a.ts","status":"renamed","beforeContent":"function foo(){}","afterContent":"function foo(){}"}]"#,
        &["--format", "json"],
    );

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#""moved":1"#));
    assert!(stdout.contains(r#""filePath":"b.ts""#));
    assert!(stdout.contains(r#""oldFilePath":"a.ts""#));
}

#[test]
fn diff_stdin_entries_reports_the_added_key() {
    let input = serde_json::json!([{
        "filePath": "sqlglot/parsers/clickhouse.py",
        "status": "modified",
        "beforeContent": "class ClickHouseParser(parser.Parser):\n    FUNCTIONS = {\n        **parser.Parser.FUNCTIONS,\n        \"ANY\": exp.AnyValue.from_arg_list,\n    }\n",
        "afterContent": "class ClickHouseParser(parser.Parser):\n    FUNCTIONS = {\n        **parser.Parser.FUNCTIONS,\n        \"ANY\": exp.AnyValue.from_arg_list,\n        \"TOUNIXTIMESTAMP\": exp.TimeToUnix.from_arg_list,\n    }\n"
    }])
    .to_string();

    let plain = run_sem_diff_stdin(&input, &["--format", "json"]);
    assert!(plain.status.success());
    let plain: serde_json::Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert_eq!(plain["changes"][0]["entityType"], "class");
    assert!(plain["changes"][0].get("entry").is_none());

    let out = run_sem_diff_stdin(&input, &["--entries", "--format", "json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let changes = v["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{v}");
    assert_eq!(changes[0]["changeType"], "added");
    assert_eq!(changes[0]["entityType"], "entry");
    assert_eq!(changes[0]["entityName"], r#"FUNCTIONS["TOUNIXTIMESTAMP"]"#);
    assert_eq!(changes[0]["entry"]["container"], "ClickHouseParser.FUNCTIONS");
    assert_eq!(changes[0]["entry"]["key"], "TOUNIXTIMESTAMP");
    assert_eq!(changes[0]["entry"]["keyKind"], "string");
}
