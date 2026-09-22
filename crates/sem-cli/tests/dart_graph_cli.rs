use serde_json::Value;
use std::{fs, process::Command};

#[test]
fn dart_callers_and_refs_stay_language_local_on_cold_and_warm_index() {
    let repo = tempfile::tempdir().unwrap();
    for (path, content) in [
        ("dart/greeter.dart", "class Greeter {\n String hello(String name) => name;\n}\n"),
        ("dart/app.dart", "import 'greeter.dart';\nvoid run() {\n final g = Greeter();\n print(g.hello('world'));\n}\n"),
        ("ts/greeter.ts", "export class Greeter { hello(name: string) { return name; } }"),
        ("ts/app.ts", "import { Greeter } from './greeter';\nexport function run() { const g = new Greeter(); g.hello('world'); }"),
    ] {
        let dest = repo.path().join(path);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(dest, content).unwrap();
    }
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_LOCAL", "1")
            .env("DO_NOT_TRACK", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    // First invocation builds the graph; subsequent queries reuse its index.
    for _ in 0..2 {
        for (target, caller) in [
            ("dart/greeter.dart", "dart/app.dart"),
            ("ts/greeter.ts", "ts/app.ts"),
        ] {
            let rows = run(&["callers", "hello", "--file", target, "--json"]);
            let related = rows[0]["related"].as_array().unwrap();
            assert_eq!(related.len(), 1, "{rows}");
            assert_eq!(related[0]["file"], caller, "{rows}");
        }
        let rows = run(&["refs", "run", "--file", "dart/app.dart", "--json"]);
        let related = rows[0]["related"].as_array().unwrap();
        assert!(related.iter().any(|e| e["name"] == "Greeter"), "{rows}");
        assert!(related.iter().any(|e| e["name"] == "hello"), "{rows}");
        assert!(
            related.iter().all(|e| e["file"] == "dart/greeter.dart"),
            "{rows}"
        );
    }
}
