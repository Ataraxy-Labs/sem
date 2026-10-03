//! Data-flow tests over small synthetic repos. Each asserts the exact flows
//! (source class -> sink class between named functions), including that
//! a source and a sink with no data dependency produce none, and that data
//! handed to unresolved code is reported as an escape, not dropped.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use super::models::Models;
use crate::parser::graph::EntityGraph;
use crate::parser::plugins::create_default_registry;

fn run(files: &[(&str, &str)]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    for (p, src) in files {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, src).unwrap();
    }
    let paths: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    let registry = create_default_registry();
    let (_, entities) = EntityGraph::build(dir.path(), &paths, &registry);
    let known: BTreeSet<String> = paths.iter().cloned().collect();
    let resolve = move |from: &str, spec: &str| -> Option<String> {
        if !spec.starts_with('.') {
            return None;
        }
        let dir = Path::new(from).parent().unwrap_or(Path::new(""));
        let joined = dir.join(spec);
        let mut norm: Vec<String> = Vec::new();
        for c in joined.components() {
            match c.as_os_str().to_str().unwrap() {
                "." => {}
                ".." => {
                    norm.pop();
                }
                s => norm.push(s.to_string()),
            }
        }
        let base = norm.join("/");
        ["", ".ts", ".js", "/index.ts"].iter().map(|e| format!("{base}{e}")).find(|c| known.contains(c))
    };
    let a = super::analyze(dir.path(), &paths, &entities, &resolve, &Models::builtin());
    a.to_json()
}

/// `class entity -> class entity` for every flow.
fn flows(v: &Value, key: &str) -> BTreeSet<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            format!(
                "{} {} -> {} {}",
                f["source"]["class"].as_str().unwrap_or("?"),
                f["source"]["entity"].as_str().unwrap_or("?"),
                f["sink"]["class"].as_str().unwrap_or("?"),
                f["sink"]["entity"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn entity<'v>(v: &'v Value, name: &str) -> &'v Value {
    v["entities"].as_array().unwrap().iter().find(|e| e["name"] == name).unwrap_or_else(|| panic!("no entity {name}: {v:#}"))
}

#[test]
fn python_env_to_exec_in_one_function() {
    let v = run(&[(
        "app.py",
        "import os\nimport subprocess\n\ndef deploy():\n    target = os.environ.get('TARGET')\n    cmd = 'ship ' + target\n    subprocess.run(cmd, shell=True)\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env deploy -> exec deploy"]), "{v:#}");
}

#[test]
fn python_no_data_dependency_no_flow() {
    let v = run(&[(
        "app.py",
        "import os\nimport subprocess\n\ndef deploy():\n    target = os.environ.get('TARGET')\n    subprocess.run(['ls', '-l'])\n    return target\n",
    )]);
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
    // but the facts still say what it reads and runs
    let e = entity(&v, "deploy");
    assert!(e["reads"].as_array().unwrap().contains(&Value::from("env")));
    assert!(e["writes"].as_array().unwrap().contains(&Value::from("exec")));
}

#[test]
fn python_interprocedural_request_to_db() {
    let v = run(&[
        (
            "db.py",
            "import sqlite3\n\ndef find(name):\n    conn = sqlite3.connect('app.db')\n    cur = conn.cursor()\n    cur.execute(\"select * from u where n = '\" + name + \"'\")\n    return cur.fetchall()\n",
        ),
        (
            "web.py",
            "from flask import request\nfrom db import find\n\ndef lookup():\n    who = request.args.get('who')\n    rows = find(who)\n    return rows\n",
        ),
    ]);
    assert_eq!(flows(&v, "flows"), set(&["http-input lookup -> db find"]), "{v:#}");
    let f = &v["flows"][0];
    let path: Vec<&str> = f["path"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert!(path.iter().any(|s| s.contains("calls find")), "{path:?}");
    assert_eq!(f["interprocedural"], true);
}

#[test]
fn python_return_value_flows_up_to_caller() {
    let v = run(&[(
        "tool.py",
        "import os\nimport subprocess\n\ndef get_cmd():\n    return os.getenv('CMD')\n\ndef main():\n    subprocess.call(get_cmd())\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env get_cmd -> exec main"]), "{v:#}");
    let path = v["flows"][0]["path"].to_string();
    assert!(path.contains("returned from get_cmd"), "{path}");
}

#[test]
fn python_module_state_carries_a_source() {
    let v = run(&[(
        "cfg.py",
        "import os\nimport logging\n\nSECRET = os.getenv('SECRET')\nlog = logging.getLogger(__name__)\n\ndef report():\n    log.info('secret is %s', SECRET)\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env <module> -> log report"]), "{v:#}");
    assert!(v["flows"][0]["throughState"].as_str().unwrap().ends_with("cfg.py:SECRET"));
    let e = entity(&v, "report");
    assert!(e["reads"].as_array().unwrap().contains(&Value::from("global:cfg.py:SECRET")), "{e:#}");
}

#[test]
fn python_object_fields_are_one_location() {
    let v = run(&[(
        "svc.py",
        "import os\n\nclass Client:\n    def __init__(self):\n        self.token = os.environ['TOKEN']\n        self.n = 0\n\n    def debug(self):\n        print(self.n)\n",
    )]);
    // field-insensitive: writing token taints the object, so reading n does too
    assert_eq!(flows(&v, "flows"), set(&["env Client.__init__ -> log Client.debug"]), "{v:#}");
    let e = entity(&v, "Client.__init__");
    assert!(e["writes"].as_array().unwrap().contains(&Value::from("field:Client.token")), "{e:#}");
}

#[test]
fn python_unresolved_callee_is_an_escape_not_silence() {
    let v = run(&[(
        "job.py",
        "import os\n\ndef run(worker):\n    secret = os.environ['KEY']\n    worker.process(secret)\n",
    )]);
    assert!(flows(&v, "flows").is_empty());
    let esc = v["escapes"].as_array().unwrap();
    assert_eq!(esc.len(), 1, "{v:#}");
    assert_eq!(esc[0]["source"]["class"], "env");
    assert!(v["coverage"]["unknown"].as_u64().unwrap() >= 1);
}

#[test]
fn python_name_only_sink_hint_is_labeled() {
    let v = run(&[(
        "q.py",
        "from flask import request\n\ndef search(cur):\n    cur.execute('select ' + request.args['q'])\n",
    )]);
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
    assert_eq!(flows(&v, "possibleFlows"), set(&["http-input search -> db search"]), "{v:#}");
    assert_eq!(v["possibleFlows"][0]["sink"]["precision"], "name-only");
}

#[test]
fn python_sanitizer_stops_a_flow() {
    let v = run(&[(
        "n.py",
        "import os\n\ndef show():\n    n = int(os.environ['N'])\n    print(n)\n",
    )]);
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
}

#[test]
fn python_dynamic_features_are_unknown() {
    let v = run(&[(
        "d.py",
        "def call(obj, name, x):\n    fn = getattr(obj, name)\n    return fn(x)\n",
    )]);
    let e = entity(&v, "call");
    assert!(!e["dynamic"].as_array().unwrap().is_empty(), "{e:#}");
    assert!(v["coverage"]["dynamicMarkers"].as_u64().unwrap() >= 1);
}

#[test]
fn ts_express_handler_to_child_process() {
    let v = run(&[(
        "src/server.ts",
        "import express from 'express';\nimport { exec } from 'child_process';\n\nconst app = express();\napp.get('/run', (req, res) => {\n  const cmd = req.query.cmd;\n  exec(`sh -c ${cmd}`);\n  res.send('ok');\n});\n",
    )]);
    // `res.send('ok')` sends a constant: no flow into the response
    assert_eq!(flows(&v, "flows"), set(&["http-input <module> -> exec <module>"]), "{v:#}");
}

#[test]
fn ts_cross_file_import_call() {
    let v = run(&[
        ("src/util.ts", "import { execSync } from 'node:child_process';\n\nexport function run(c: string) {\n  return execSync(c);\n}\n"),
        ("src/main.ts", "import { run } from './util';\n\nexport function start() {\n  run(process.env.BOOT_CMD || 'true');\n}\n"),
    ]);
    assert_eq!(flows(&v, "flows"), set(&["env start -> exec run"]), "{v:#}");
}

#[test]
fn ts_untyped_member_call_is_unknown() {
    let v = run(&[("src/a.ts", "export function f(svc: any) {\n  svc.handle(process.env.X);\n}\n")]);
    assert!(flows(&v, "flows").is_empty());
    assert_eq!(v["escapes"].as_array().unwrap().len(), 1, "{v:#}");
}

#[test]
fn go_request_to_sql_and_env_to_exec() {
    let v = run(&[(
        "main.go",
        "package main\n\nimport (\n\t\"database/sql\"\n\t\"net/http\"\n\t\"os\"\n\t\"os/exec\"\n)\n\nfunc handler(db *sql.DB, w http.ResponseWriter, r *http.Request) {\n\tq := r.URL.Query().Get(\"q\")\n\tdb.Query(\"select \" + q)\n}\n\nfunc boot() {\n\tbin := os.Getenv(\"BIN\")\n\texec.Command(bin).Run()\n}\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["http-input handler -> db handler", "env boot -> exec boot"]), "{v:#}");
}

#[test]
fn go_error_results_carry_no_data() {
    let v = run(&[(
        "main.go",
        "package main\n\nimport (\n\t\"log\"\n\t\"os\"\n)\n\nfunc load(p string) []byte {\n\tdata, err := os.ReadFile(p)\n\tif err != nil {\n\t\tlog.Printf(\"read failed: %v\", err)\n\t}\n\tlog.Println(len(data))\n\treturn data\n}\n",
    )]);
    // `err` is a status; `len(data)` is sanitized: no file-read -> log
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
}

#[test]
fn rust_env_to_command_and_log() {
    let v = run(&[(
        "src/main.rs",
        "use std::process::Command;\n\nfn tool() -> String {\n    std::env::var(\"TOOL\").unwrap()\n}\n\nfn main() {\n    let t = tool();\n    println!(\"running {}\", t);\n    Command::new(t).status().unwrap();\n}\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env tool -> exec main", "env tool -> log main"]), "{v:#}");
}

#[test]
fn complexity_counts_decisions_and_nesting() {
    let v = run(&[(
        "c.py",
        "def f(xs, a, b):\n    for x in xs:\n        if x and a or b:\n            return 1\n        elif x:\n            return 2\n        else:\n            return 3\n    return 0\n",
    )]);
    let e = entity(&v, "f");
    // decisions: for, if, elif, `and`, `or` -> 5 + 1
    assert_eq!(e["cyclomatic"], 6, "{e:#}");
    // for +1; if +2 (nested); elif +1; else +1; `and`/`or` runs +2 -> 7
    assert_eq!(e["cognitive"], 7, "{e:#}");
}
