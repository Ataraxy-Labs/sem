//! `sem diff --stdin --entries --json`, exactly as land invokes it: adding one key to a dict
//! literal is `added entry FUNCTIONS["X"]`, never the enclosing class or binding as modified.

use std::io::Write;
use std::process::{Command, Stdio};

fn sem_entries_json(input: &serde_json::Value) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(["diff", "--stdin", "--entries", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn summary(v: &serde_json::Value) -> Vec<(String, String, String, String)> {
    let mut rows: Vec<_> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let s = |k: &str| c[k].as_str().unwrap_or_default().to_string();
            (s("filePath"), s("changeType"), s("entityType"), s("entityName"))
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn dict_literal_addition_is_an_added_entry_not_a_modified_class() {
    let input = serde_json::json!([
        {
            "filePath": "dialects/duck.py",
            "status": "modified",
            "beforeContent": "class Parser(base.Parser):\n    FUNCTIONS = {\n        **base.Parser.FUNCTIONS,\n        \"ANY\": exp.AnyValue.from_arg_list,\n    }\n\n    def parse(self):\n        return 1\n",
            "afterContent": "class Parser(base.Parser):\n    FUNCTIONS = {\n        **base.Parser.FUNCTIONS,\n        \"ANY\": exp.AnyValue.from_arg_list,\n        \"X\": exp.X.from_arg_list,\n    }\n\n    def parse(self):\n        return 1\n"
        },
        {
            "filePath": "registry.py",
            "status": "modified",
            "beforeContent": "FUNCTIONS = {\n    \"a\": 1,\n}\n",
            "afterContent": "FUNCTIONS = {\n    \"a\": 1,\n    \"X\": 2,\n}\n"
        },
        {
            "filePath": "registry.ts",
            "status": "modified",
            "beforeContent": "export const FUNCTIONS = {\n  a: 1,\n};\n",
            "afterContent": "export const FUNCTIONS = {\n  a: 1,\n  X: 2,\n};\n"
        }
    ]);
    let v = sem_entries_json(&input);
    let rows = summary(&v);
    let row = |f: &str, n: &str| (f.to_string(), "added".to_string(), "entry".to_string(), n.to_string());
    assert_eq!(
        rows,
        vec![
            row("dialects/duck.py", r#"FUNCTIONS["X"]"#),
            row("registry.py", r#"FUNCTIONS["X"]"#),
            row("registry.ts", "FUNCTIONS[X]"),
        ],
        "{v}"
    );
    let duck = v["changes"].as_array().unwrap().iter().find(|c| c["filePath"] == "dialects/duck.py").unwrap();
    assert_eq!(duck["entry"]["container"], "Parser.FUNCTIONS", "{v}");
    assert_eq!(duck["entry"]["key"], "X", "{v}");
    assert_eq!(duck["entry"]["keyKind"], "string", "{v}");
    assert!(
        !v["changes"].as_array().unwrap().iter().any(|c| c["changeType"] == "modified"),
        "no class or binding is reported modified: {v}"
    );
}
