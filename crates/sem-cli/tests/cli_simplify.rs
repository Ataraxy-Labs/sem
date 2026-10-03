//! The simplified command line: core verbs, flags for their variations, and
//! every old command name kept as a hidden alias with identical output.
//!
//! Golden outputs under `tests/fixtures/cli_simplify/` were recorded from the
//! binary before the change, on the fixture repo `fixture()` builds (fixed
//! author, dates and contents, so commit ids are stable). Each old invocation
//! must still produce its golden byte for byte, after two normalizations:
//! the fixture's temporary path becomes `<REPO>`, and JSON `elapsedMs` /
//! `elapsed_ms` timings are dropped. Each new spelling must produce the same
//! golden as the old command it replaces.
//!
//! Regenerate the goldens with an old binary:
//!   SEM_GOLDEN_BIN=/path/to/old/sem SEM_UPDATE_GOLDENS=1 cargo test -p sem-cli --test cli_simplify goldens

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn git(repo: &Path, args: &[&str], date: Option<&str>) {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo).args(args);
    if let Some(d) = date {
        cmd.env("GIT_AUTHOR_DATE", d).env("GIT_COMMITTER_DATE", d);
    }
    let out = cmd.output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A small Python repo with a test, two commits.
fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let r = dir.path();
    git(r, &["init", "-q"], None);
    git(r, &["config", "user.email", "t@example.com"], None);
    git(r, &["config", "user.name", "T"], None);
    git(r, &["config", "commit.gpgsign", "false"], None);
    fs::create_dir_all(r.join("src")).unwrap();
    fs::create_dir_all(r.join("tests")).unwrap();
    fs::write(
        r.join("src/config.py"),
        "def parse_config(text):\n    return dict(line.split(\"=\") for line in text.splitlines())\n\n\ndef load(path):\n    with open(path) as f:\n        return parse_config(f.read())\n",
    )
    .unwrap();
    fs::write(
        r.join("tests/test_config.py"),
        "from src.config import parse_config\n\n\ndef test_parse_config():\n    assert parse_config(\"a=1\") == {\"a\": \"1\"}\n",
    )
    .unwrap();
    git(r, &["add", "-A"], None);
    git(r, &["commit", "-qm", "init"], Some("@1700000000"));
    fs::write(
        r.join("src/config.py"),
        "def parse_config(text):\n    pairs = [line.split(\"=\", 1) for line in text.splitlines() if line]\n    return dict(pairs)\n\n\ndef load(path):\n    with open(path) as f:\n        return parse_config(f.read())\n\n\ndef main():\n    print(load(\"app.cfg\"))\n",
    )
    .unwrap();
    git(r, &["add", "-A"], None);
    git(r, &["commit", "-qm", "change"], Some("@1700000100"));
    dir
}

fn golden_bin() -> PathBuf {
    std::env::var_os("SEM_GOLDEN_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_sem")))
}

fn sem_with(bin: &Path, repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(bin)
        .current_dir(repo)
        .args(args)
        .env("SEM_CACHE_DIR", cache)
        .env("SEM_TELEMETRY", "off")
        .env("SEM_NO_PROGRESS", "1")
        .env("SEM_NO_NETWORK", "1")
        .env_remove("SEM_REVIEW_DIFF_ID")
        .env_remove("SEM_NO_DEPRECATION")
        .output()
        .expect("run sem")
}

fn sem(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    sem_with(Path::new(env!("CARGO_BIN_EXE_sem")), repo, cache, args)
}

fn strip_timings(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(m) => {
            m.remove("elapsedMs");
            m.remove("elapsed_ms");
            for x in m.values_mut() {
                strip_timings(x);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip_timings),
        _ => {}
    }
}

/// stdout with the repo path replaced and JSON timings dropped. JSON output
/// (one document, or one per line) is re-serialized pretty.
fn normalize(out: &[u8], repo: &Path) -> String {
    let mut text = String::from_utf8_lossy(out).to_string();
    let canon = repo.canonicalize().unwrap();
    for p in [
        canon.to_string_lossy().to_string(),
        repo.to_string_lossy().to_string(),
    ] {
        text = text.replace(&p, "<REPO>");
    }
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) {
        strip_timings(&mut v);
        return serde_json::to_string_pretty(&v).unwrap() + "\n";
    }
    let lines: Vec<&str> = text.lines().collect();
    if !lines.is_empty()
        && lines
            .iter()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
    {
        return lines
            .iter()
            .map(|l| {
                let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
                strip_timings(&mut v);
                serde_json::to_string(&v).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
    }
    text
}

/// (golden name, old invocation, new spellings that must print the same)
type Case = (
    &'static str,
    &'static [&'static str],
    &'static [&'static [&'static str]],
);

const CASES: &[Case] = &[
    ("find_json", &["find", "parse_config", "--json"], &[]),
    ("find_text", &["find", "parse_config"], &[]),
    (
        "find_batch_json",
        &["find", "parse_config", "load", "--json"],
        &[],
    ),
    (
        "find_file_json",
        &["find", "parse_config", "--file", "src/config.py", "--json"],
        &[&["find", "parse_config", "--in", "src/config.py", "--json"]],
    ),
    (
        "callers_json",
        &["callers", "parse_config", "--json"],
        &[&["find", "parse_config", "--callers", "--json"]],
    ),
    (
        "callers_text",
        &["callers", "parse_config"],
        &[&["find", "parse_config", "--callers"]],
    ),
    (
        "callers_limit_json",
        &["callers", "parse_config", "--limit", "1", "--json"],
        &[&[
            "find",
            "parse_config",
            "--callers",
            "--limit",
            "1",
            "--json",
        ]],
    ),
    (
        "refs_json",
        &["refs", "load", "--json"],
        &[&["find", "load", "--refs", "--json"]],
    ),
    (
        "refs_text",
        &["refs", "load"],
        &[&["find", "load", "--refs"]],
    ),
    (
        "entities_src_json",
        &["entities", "src", "--json"],
        &[&["find", "--in", "src", "--json"]],
    ),
    (
        "entities_file_json",
        &["entities", "--json", "src/config.py"],
        &[&["find", "--in", "src/config.py", "--json"]],
    ),
    (
        "entities_text",
        &["entities", "src/config.py"],
        &[&["find", "--in", "src/config.py"]],
    ),
    (
        "entities_noexcl_json",
        &[
            "entities",
            "src/config.py",
            "--json",
            "--no-default-excludes",
        ],
        &[&[
            "find",
            "--in",
            "src/config.py",
            "--json",
            "--no-default-excludes",
        ]],
    ),
    (
        "entities_only_json",
        &["entities", "src", "--only", "function", "--json"],
        &[&["find", "--in", "src", "--only", "function", "--json"]],
    ),
    (
        "entities_text_search_json",
        &["entities", "--text", "splitlines", "--json"],
        &[&["find", "--text", "splitlines", "--json"]],
    ),
    (
        "context_json",
        &["context", "parse_config", "--json"],
        &[&["find", "parse_config", "--context", "--json"]],
    ),
    (
        "context_text",
        &["context", "parse_config"],
        &[&["find", "parse_config", "--context"]],
    ),
    (
        "context_by_id_json",
        &[
            "context",
            "--entity-id",
            "src/config.py::function::parse_config",
            "--file",
            "src/config.py",
            "--budget",
            "2000",
            "--hops",
            "1",
            "--format",
            "json",
        ],
        &[&[
            "find",
            "--context",
            "--entity-id",
            "src/config.py::function::parse_config",
            "--in",
            "src/config.py",
            "--budget",
            "2000",
            "--hops",
            "1",
            "--json",
        ]],
    ),
    (
        "context_batch_json",
        &[
            "context",
            "--entity",
            "parse_config",
            "--entity",
            "load",
            "--json",
        ],
        &[&["find", "parse_config", "load", "--context", "--json"]],
    ),
    (
        "log_json",
        &["log", "parse_config", "--json"],
        &[&["history", "parse_config", "--json"]],
    ),
    (
        "log_limit_json",
        &["log", "parse_config", "--json", "--limit", "1"],
        &[&["history", "parse_config", "--json", "--limit", "1"]],
    ),
    (
        "log_repo_json",
        &["log", "--json"],
        &[&["history", "--json"]],
    ),
    (
        "log_text",
        &["log", "parse_config"],
        &[&["history", "parse_config"]],
    ),
    (
        "blame_json",
        &["blame", "src/config.py", "--json"],
        &[&["history", "--blame", "src/config.py", "--json"]],
    ),
    ("graph_json", &["graph", "--json"], &[]),
    (
        "graph_noexcl_json",
        &["graph", "--json", "--no-default-excludes"],
        &[&["graph", ".", "--json", "--no-default-excludes"]],
    ),
    (
        "impact_json",
        &["impact", "parse_config", "--json", "--no-default-excludes"],
        &[],
    ),
    (
        "impact_dependents_json",
        &[
            "impact",
            "parse_config",
            "--file",
            "src/config.py",
            "--json",
            "--dependents",
            "--no-default-excludes",
        ],
        &[],
    ),
    (
        "impact_by_id_json",
        &[
            "impact",
            "--entity-id",
            "src/config.py::function::parse_config",
            "--json",
            "--dependents",
            "--no-default-excludes",
        ],
        &[],
    ),
    (
        "impact_tests_json",
        &["impact", "parse_config", "--tests", "--json"],
        &[],
    ),
    ("diff_ref_json", &["diff", "HEAD~1", "--json"], &[]),
    ("diff_range_json", &["diff", "HEAD~1..HEAD", "--json"], &[]),
    ("grep_json", &["grep", "parse_config", "--json"], &[]),
    ("grep_text", &["grep", "parse_config"], &[]),
    ("certify_json", &["certify", "HEAD~1..HEAD", "--json"], &[]),
    ("certify_md", &["certify", "HEAD~1..HEAD"], &[]),
    (
        "arch_diff_json",
        &["arch-diff", "HEAD~1..HEAD", "--json"],
        &[&["certify", "HEAD~1..HEAD", "--arch", "--json"]],
    ),
    (
        "arch_diff_view",
        &["arch-diff", "HEAD~1..HEAD", "--view"],
        &[
            &["certify", "HEAD~1..HEAD", "--arch", "--view"],
            &["certify", "HEAD~1..HEAD", "--view"],
        ],
    ),
    (
        "dataflow_json",
        &["dataflow", "--json"],
        &[&["graph", "--dataflow", "--json"]],
    ),
    ("dataflow_text", &["dataflow"], &[&["graph", "--dataflow"]]),
];

#[test]
fn goldens_old_invocations_and_their_new_spellings() {
    let update = std::env::var_os("SEM_UPDATE_GOLDENS").is_some();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli_simplify");
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let mut failures = Vec::new();
    for (name, old, news) in CASES {
        let path = dir.join(format!("{name}.out"));
        if update {
            let out = sem_with(&golden_bin(), repo.path(), cache.path(), old);
            fs::create_dir_all(&dir).unwrap();
            fs::write(&path, normalize(&out.stdout, repo.path())).unwrap();
            continue;
        }
        let golden = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing golden {}", path.display()));
        for argv in std::iter::once(*old).chain(news.iter().copied()) {
            let out = sem(repo.path(), cache.path(), argv);
            let got = normalize(&out.stdout, repo.path());
            if got != golden {
                failures.push(format!("sem {} differs from golden {name}:\n--- golden\n{golden}\n--- got\n{got}\nstderr: {}", argv.join(" "), String::from_utf8_lossy(&out.stderr)));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The exact `sem` command lines the pi agent tools run (pi/src). Each must
/// still succeed and print JSON in the shape its caller parses.
#[test]
fn agent_tool_invocations_keep_working() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let abs = repo.path().canonicalize().unwrap().join("src/config.py");
    let abs = abs.to_string_lossy().to_string();
    let invocations: Vec<Vec<&str>> = vec![
        vec!["find", "parse_config", "--json"],
        vec!["callers", "parse_config", "--json"],
        vec!["grep", "parse_config", "--json"],
        vec!["entities", "src/config.py", "--json"],
        vec!["entities", "--json", &abs],
        vec![
            "entities",
            "src/config.py",
            "--json",
            "--no-default-excludes",
        ],
        vec![
            "context",
            "--entity-id",
            "src/config.py::function::parse_config",
            "--file",
            "src/config.py",
            "--budget",
            "1500",
            "--hops",
            "1",
            "--format",
            "json",
        ],
        vec!["impact", "parse_config", "--json", "--no-default-excludes"],
        vec![
            "impact",
            "parse_config",
            "--file",
            &abs,
            "--json",
            "--no-default-excludes",
        ],
        vec![
            "impact",
            "--entity-id",
            "src/config.py::function::parse_config",
            "--json",
            "--dependents",
            "--no-default-excludes",
        ],
        vec![
            "impact",
            "parse_config",
            "--file",
            &abs,
            "--json",
            "--dependents",
            "--no-default-excludes",
        ],
        vec!["diff", "--json"],
        vec!["diff", "HEAD~1", "--json"],
        vec!["log", "parse_config", "--json"],
        vec!["log", "parse_config", "--json", "--limit", "1"],
        vec!["log", "--json"],
        vec!["graph", "--json", "--no-default-excludes"],
        vec!["graph", "--json"],
    ];
    for argv in invocations {
        let out = sem(repo.path(), cache.path(), &argv);
        assert!(
            out.status.success(),
            "sem {argv:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        let parses = serde_json::from_str::<serde_json::Value>(&text).is_ok()
            || (!text.trim().is_empty()
                && text
                    .lines()
                    .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()));
        assert!(parses, "sem {argv:?} did not print JSON:\n{text}");
        assert!(
            out.stderr.is_empty() || !String::from_utf8_lossy(&out.stderr).contains("note:"),
            "sem {argv:?} printed a notice"
        );
    }
}

#[test]
fn old_names_print_no_notice_when_piped_or_in_json_mode() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    for argv in [
        &["callers", "parse_config"][..],
        &["callers", "parse_config", "--json"],
        &["refs", "load"],
        &["entities", "src"],
        &["context", "parse_config", "--format", "json"],
        &["log", "parse_config"],
        &["blame", "src/config.py"],
        &["dataflow"],
        &["arch-diff", "HEAD~1..HEAD", "--md"],
        &["telemetry", "preview"],
        &["stats"],
    ] {
        let out = sem(repo.path(), cache.path(), argv);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !err.contains("is now `sem"),
            "sem {argv:?} printed a notice when piped: {err}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("is now `sem"),
            "notice on stdout for {argv:?}"
        );
    }
}

/// Under a terminal (via `script`), a human gets the one-line notice on
/// stderr, and still none in JSON mode.
#[test]
fn old_names_note_the_new_spelling_at_a_terminal() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_sem");
    let tty = |args: &str| -> Option<String> {
        let line = format!("{bin} {args}");
        let mut cmd = Command::new("script");
        if cfg!(target_os = "macos") {
            cmd.args(["-q", "/dev/null", "sh", "-c", &line]);
        } else {
            cmd.args(["-qec", &line, "/dev/null"]);
        }
        let out = cmd
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_TELEMETRY", "off")
            .env_remove("SEM_NO_DEPRECATION")
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let Some(text) = tty("callers parse_config") else {
        return; // no `script` here
    };
    if text.trim().is_empty() {
        return; // `script` could not allocate a terminal
    }
    assert!(
        text.contains("note: `sem callers` is now `sem find --callers`"),
        "{text}"
    );
    assert_eq!(text.matches("is now `sem").count(), 1, "{text}");
    let json = tty("callers parse_config --json").unwrap();
    assert!(!json.contains("is now `sem"), "{json}");
    let new = tty("find parse_config --callers").unwrap();
    assert!(!new.contains("is now `sem"), "{new}");
}

#[test]
fn top_level_help_lists_only_the_core_verbs_and_a_quickstart() {
    let out = Command::new(env!("CARGO_BIN_EXE_sem"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    let listed: Vec<&str> = help
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(
        listed,
        [
            "find", "grep", "impact", "check", "certify", "diff", "graph", "history", "cloud",
            "config", "mcp", "help"
        ]
    );
    let quickstart = help.split("QUICKSTART").nth(1).expect("QUICKSTART section");
    for (question, verb) in [
        ("where is it?", "sem find"),
        ("where is it?", "sem grep"),
        ("what does my change touch?", "sem impact"),
        ("is it correct?", "sem check"),
        ("what should a human review?", "sem certify"),
    ] {
        let line = quickstart
            .lines()
            .find(|l| l.contains(question))
            .unwrap_or_else(|| panic!("no line for {question}"));
        assert!(line.contains(verb), "{question} -> {verb}: {line}");
    }
}

#[test]
fn groups_list_their_verbs() {
    let help = |args: &[&str]| {
        String::from_utf8_lossy(
            &Command::new(env!("CARGO_BIN_EXE_sem"))
                .args(args)
                .output()
                .unwrap()
                .stdout,
        )
        .to_string()
    };
    let cloud = help(&["cloud", "--help"]);
    for v in [
        "login", "logout", "whoami", "review", "xref", "repos", "enable", "disable",
    ] {
        assert!(
            cloud.lines().any(|l| l.trim_start().starts_with(v)),
            "cloud lists {v}:\n{cloud}"
        );
    }
    for hidden in ["share", "forget", "never"] {
        assert!(
            !cloud.lines().any(|l| l.trim_start().starts_with(hidden)),
            "cloud hides {hidden}"
        );
    }
    let config = help(&["config", "--help"]);
    for v in [
        "setup",
        "unsetup",
        "telemetry",
        "completions",
        "update",
        "stats",
    ] {
        assert!(
            config.lines().any(|l| l.trim_start().starts_with(v)),
            "config lists {v}:\n{config}"
        );
    }
}

#[test]
fn completions_and_telemetry_answer_the_same_under_config() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let old = sem(repo.path(), cache.path(), &["completions", "bash"]);
    let new = sem(
        repo.path(),
        cache.path(),
        &["config", "completions", "bash"],
    );
    assert!(old.status.success() && !old.stdout.is_empty());
    assert_eq!(old.stdout, new.stdout);
    let script = String::from_utf8_lossy(&new.stdout);
    assert!(
        script.contains("history") && script.contains("certify"),
        "completions know the new verbs"
    );
    let old = sem(repo.path(), cache.path(), &["telemetry", "preview"]);
    let new = sem(
        repo.path(),
        cache.path(),
        &["config", "telemetry", "preview"],
    );
    assert_eq!(old.stdout, new.stdout);
}

#[test]
fn find_refuses_ambiguous_mode_input() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let out = sem(repo.path(), cache.path(), &["find"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--in <path>"));
    let out = sem(repo.path(), cache.path(), &["find", "a", "b", "--callers"]);
    assert_eq!(out.status.code(), Some(2));
    let out = sem(
        repo.path(),
        cache.path(),
        &["find", "a", "--callers", "--refs"],
    );
    assert!(!out.status.success(), "--callers and --refs are exclusive");
    let out = sem(repo.path(), cache.path(), &["find", "a", "--limit", "3"]);
    assert!(!out.status.success(), "--limit needs --callers");
}

#[test]
fn impact_diff_reports_each_changed_entity_in_impact_shape() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let out = sem(
        repo.path(),
        cache.path(),
        &["impact", "--diff", "HEAD~1..HEAD", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let docs: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let names: Vec<&str> = docs
        .iter()
        .map(|d| d["entity"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["parse_config", "main"]);
    // each one is exactly `sem impact --entity-id <id> --json`
    let single = sem(
        repo.path(),
        cache.path(),
        &[
            "impact",
            "--entity-id",
            "src/config.py::function::parse_config",
            "--json",
        ],
    );
    assert_eq!(
        normalize(text.lines().next().unwrap().as_bytes(), repo.path()),
        normalize(&single.stdout, repo.path())
    );

    // one ref: that ref against the working tree
    let out = sem(
        repo.path(),
        cache.path(),
        &["impact", "--diff", "HEAD", "--json"],
    );
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "a clean tree changes nothing");

    let out = sem(
        repo.path(),
        cache.path(),
        &["impact", "--diff", "HEAD~1", "--tests", "--json"],
    );
    let docs: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(docs[0]["tests"][0]["name"], "test_parse_config");
    assert_eq!(docs[1]["noTestReaches"], true);
}

#[test]
fn impact_diff_tests_in_a_js_workspace_is_the_module_graph_selection() {
    let dir = TempDir::new().unwrap();
    let r = dir.path();
    git(r, &["init", "-q"], None);
    git(r, &["config", "user.email", "t@example.com"], None);
    git(r, &["config", "user.name", "T"], None);
    git(r, &["config", "commit.gpgsign", "false"], None);
    fs::create_dir_all(r.join("src")).unwrap();
    fs::write(
        r.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        r.join("src/math.ts"),
        "export function add(a: number, b: number) { return a + b; }\n",
    )
    .unwrap();
    fs::write(
        r.join("src/math.test.ts"),
        "import { add } from './math';\ntest('add', () => expect(add(1, 2)).toBe(3));\n",
    )
    .unwrap();
    fs::write(
        r.join("src/other.test.ts"),
        "test('other', () => expect(1).toBe(1));\n",
    )
    .unwrap();
    git(r, &["add", "-A"], None);
    git(r, &["commit", "-qm", "init"], Some("@1700000000"));
    fs::write(
        r.join("src/math.ts"),
        "export function add(a: number, b: number) { return b + a; }\n",
    )
    .unwrap();
    let cache = TempDir::new().unwrap();
    let out = sem(
        r,
        cache.path(),
        &["impact", "--diff", "HEAD", "--tests", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let new: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let old = sem(
        r,
        cache.path(),
        &[
            "topology",
            "affected-tests",
            "--root-package",
            "src/math.ts",
        ],
    );
    let old: serde_json::Value = serde_json::from_slice(&old.stdout).unwrap();
    assert_eq!(
        new, old,
        "the same selection as the module graph's affected-tests"
    );
    let text = new.to_string();
    assert!(text.contains("src/math.test.ts"), "{text}");
    assert!(!text.contains("src/other.test.ts"), "{text}");
}

#[test]
fn check_verdicts_from_promises() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    // nothing to check is "could not decide", never a pass
    let out = sem(repo.path(), cache.path(), &["check"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = sem(
        repo.path(),
        cache.path(),
        &["check", "--checkers", "ts,lint"],
    );
    assert_eq!(out.status.code(), Some(2));

    let p = repo.path().join(".sem/promises");
    fs::create_dir_all(&p).unwrap();
    fs::write(
        p.join("no-print.json"),
        r#"{"laws": [{"id": "no-print", "promise": "library code does not print",
            "forbidPattern": {"from": "src/**/*.py", "query": "((call function: (identifier) @_f) @print (#eq? @_f \"print\"))"},
            "mutation": {"file": "src/config.py", "append": "\nprint(1)\n"}}]}"#,
    )
    .unwrap();
    let out = sem(repo.path(), cache.path(), &["check", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "main() prints: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let old = sem(repo.path(), cache.path(), &["promises", "check", "--json"]);
    assert_eq!(
        out.stdout, old.stdout,
        "sem check reports what promises check reports"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["broken"], 1);

    let verify_new = sem(
        repo.path(),
        cache.path(),
        &["check", "--promises", "--json"],
    );
    let verify_old = sem(repo.path(), cache.path(), &["promises", "verify", "--json"]);
    assert_eq!(verify_new.stdout, verify_old.stdout);
    assert_eq!(verify_new.status.code(), verify_old.status.code());
}

#[test]
fn graph_layers_route_to_their_engines() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let new = sem(repo.path(), cache.path(), &["graph", "--system", "--json"]);
    let old = sem(repo.path(), cache.path(), &["system", "deps", "--json"]);
    assert!(
        old.status.success(),
        "{}",
        String::from_utf8_lossy(&old.stderr)
    );
    assert_eq!(new.stdout, old.stdout);
    let new = sem(
        repo.path(),
        cache.path(),
        &["graph", "--system", "deps", "--json"],
    );
    assert_eq!(new.stdout, old.stdout);
    let new = sem(
        repo.path(),
        cache.path(),
        &["graph", "--modules", "metrics", "--root-package"],
    );
    let old = sem(
        repo.path(),
        cache.path(),
        &["topology", "metrics", "--root-package"],
    );
    assert_eq!(new.stdout, old.stdout);
    let out = sem(repo.path(), cache.path(), &["graph", "--system", "metrics"]);
    assert!(
        !out.status.success(),
        "a modules operation under --system is refused"
    );
    let out = sem(repo.path(), cache.path(), &["graph", "--witness"]);
    assert!(!out.status.success(), "--witness needs --dataflow");
}

#[test]
fn history_blame_needs_a_file() {
    let repo = fixture();
    let cache = TempDir::new().unwrap();
    let out = sem(repo.path(), cache.path(), &["history", "--blame"]);
    assert_eq!(out.status.code(), Some(2));
    let a = sem(
        repo.path(),
        cache.path(),
        &["history", "--blame", "--file", "src/config.py", "--json"],
    );
    let b = sem(
        repo.path(),
        cache.path(),
        &["blame", "src/config.py", "--json"],
    );
    assert_eq!(a.stdout, b.stdout);
}

/// Speak MCP over `sem mcp`'s stdio: initialize, then each request in turn.
fn mcp(repo: &Path, args: &[&str], requests: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_sem"))
        .arg("mcp")
        .args(args)
        .current_dir(repo)
        .env("SEM_MCP_NO_SHARED", "1")
        .env("SEM_TELEMETRY", "off")
        .env("SEM_NO_WATCH", "1")
        .env_remove("SEM_REVIEW_DIFF_ID")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |v: serde_json::Value| writeln!(stdin, "{v}").unwrap();
    let mut read = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    send(
        serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
    );
    read();
    send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let mut replies = Vec::new();
    for (i, r) in requests.iter().enumerate() {
        let mut r = r.clone();
        r["jsonrpc"] = "2.0".into();
        r["id"] = (i + 1).into();
        send(r);
        replies.push(read());
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    replies
}

fn tool_text(reply: &serde_json::Value) -> String {
    reply["result"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mcp_lists_the_core_verbs_and_still_answers_old_tool_names() {
    let repo = fixture();
    let call = |name: &str, arguments: serde_json::Value| serde_json::json!({"method": "tools/call", "params": {"name": name, "arguments": arguments}});
    let replies = mcp(
        repo.path(),
        &[],
        &[
            serde_json::json!({"method": "tools/list"}),
            call(
                "sem_find",
                serde_json::json!({"query": "parse_config", "format": "json"}),
            ),
            call(
                "sem_find",
                serde_json::json!({"query": "parse_config", "mode": "callers"}),
            ),
            call("sem_callers", serde_json::json!({"query": "parse_config"})),
            call("sem_find", serde_json::json!({"in": "src/config.py"})),
            call("sem_entities", serde_json::json!({"path": "src/config.py"})),
            call(
                "sem_find",
                serde_json::json!({"query": "load", "mode": "refs", "format": "json"}),
            ),
            call(
                "sem_history",
                serde_json::json!({"blame": true, "file_path": "src/config.py"}),
            ),
            call(
                "sem_blame",
                serde_json::json!({"file_path": "src/config.py"}),
            ),
            call("sem_check", serde_json::json!({})),
            call(
                "sem_certify",
                serde_json::json!({"range": "HEAD~1..HEAD", "format": "json"}),
            ),
            call(
                "sem_find",
                serde_json::json!({"query": "parse_config", "mode": "context", "token_budget": 500}),
            ),
            call(
                "sem_context",
                serde_json::json!({"entity_name": "parse_config", "token_budget": 500, "fresh": true}),
            ),
            call(
                "sem_find",
                serde_json::json!({"intent": "parse config text"}),
            ),
            call(
                "sem_entities",
                serde_json::json!({"query": "parse config text"}),
            ),
        ],
    );
    let names: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "sem_find",
            "sem_grep",
            "sem_impact",
            "sem_check",
            "sem_certify",
            "sem_diff",
            "sem_graph",
            "sem_history"
        ]
    );
    for t in replies[0]["result"]["tools"].as_array().unwrap() {
        let d = t["description"].as_str().unwrap();
        assert!(
            !d.is_empty() && d.len() < 700,
            "{} has a short description ({} chars)",
            t["name"],
            d.len()
        );
    }
    let found: serde_json::Value = serde_json::from_str(&tool_text(&replies[1])).unwrap();
    assert_eq!(found[0]["file"], "src/config.py");
    assert_eq!(
        tool_text(&replies[2]),
        tool_text(&replies[3]),
        "mode callers is sem_callers"
    );
    assert_eq!(
        tool_text(&replies[4]),
        tool_text(&replies[5]),
        "`in` with no query is sem_entities"
    );
    let refs: serde_json::Value = serde_json::from_str(&tool_text(&replies[6])).unwrap();
    assert!(refs.to_string().contains("parse_config"), "{refs}");
    assert_eq!(
        tool_text(&replies[7]),
        tool_text(&replies[8]),
        "history blame is sem_blame"
    );
    let check = tool_text(&replies[9]);
    assert!(check.contains("exit 2 (could not decide)"), "{check}");
    assert_eq!(
        replies[9]["result"]["isError"], false,
        "a verdict is a result, not a tool error"
    );
    let cert: serde_json::Value = serde_json::from_str(
        tool_text(&replies[10])
            .lines()
            .take_while(|l| !l.starts_with("exit "))
            .collect::<Vec<_>>()
            .join("\n")
            .as_str(),
    )
    .unwrap();
    assert_eq!(cert["summary"]["filesChanged"], 1);
    let strip = |s: String| {
        s.lines()
            .filter(|l| !l.contains("elapsed"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        strip(tool_text(&replies[11])),
        strip(tool_text(&replies[12])),
        "mode context is sem_context"
    );
    let strip_ms = |s: String| {
        s.lines()
            .filter(|l| !l.contains("ms"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        strip_ms(tool_text(&replies[13])),
        strip_ms(tool_text(&replies[14])),
        "intent is sem_entities' ranked query"
    );
}

#[test]
fn mcp_review_sessions_also_list_the_listener_tools() {
    let repo = fixture();
    let replies = mcp(
        repo.path(),
        &["--review"],
        &[serde_json::json!({"method": "tools/list"})],
    );
    let names: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "sem_find",
            "sem_grep",
            "sem_impact",
            "sem_check",
            "sem_certify",
            "sem_diff",
            "sem_graph",
            "sem_history",
            "join_review",
            "wait_for_branch",
            "reply_to_branch",
            "list_open_branches"
        ]
    );
}
