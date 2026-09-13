use std::{fs, process::Command};

#[test]
fn returned_graph_memory_is_opt_in_and_does_not_change_json() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("example.py"), "def callee():\n    pass\n\ndef caller():\n    callee()\n").unwrap();
    let run = |profile: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sem"));
        command.args(["graph", root.path().to_str().unwrap(), "--json", "--no-cache"])
            .env("SEM_CLOUD", "0").env("SEM_TELEMETRY", "0");
        if profile { command.env("SEM_PROFILE_MEM", "1"); }
        else { command.env_remove("SEM_PROFILE_MEM"); }
        let result = command.output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        result
    };
    let normal = run(false);
    let measured = run(true);
    assert!(!String::from_utf8_lossy(&normal.stderr).contains("SEM_PROFILE_MEM[graph-return]"));
    assert!(String::from_utf8_lossy(&measured.stderr).contains("SEM_PROFILE_MEM[graph-return] process_rss_bytes="));
    let normal: serde_json::Value = serde_json::from_slice(&normal.stdout).unwrap();
    let measured: serde_json::Value = serde_json::from_slice(&measured.stdout).unwrap();
    assert_eq!(normal, measured);
    assert_eq!(normal["stats"]["entityCount"], 2);
    assert_eq!(normal["stats"]["edgeCount"], 1);
}
