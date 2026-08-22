//! `sem context`'s index-backed fast path (byte-span
//! change following). The old git-oracle subgraph fast path
//! answered *differently* from the authoritative walk and was deleted rather
//! than repaired, leaving `context` with no index tier at all; this reroute
//! adds one back, gated so hard that a divergence should be structurally
//! impossible: the reroute calls the exact same
//! `build_context_result_bounded`/render code the authoritative path calls,
//! over a subgraph it proves complete up to the packer's own cap.

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

/// `leaf` <- `mid` <- `top`, plus an unrelated `other.ts` — enough shape to
/// exercise direct + transitive dependencies/dependents in both directions.
fn init_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::write(
        repo.join("leaf.ts"),
        "export function leaf() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("mid.ts"),
        "import { leaf } from './leaf';\nexport function mid() { return leaf() + 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("top.ts"),
        "import { mid } from './mid';\nexport function top() { return mid() + 1; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("other.ts"),
        "export function unrelated() { return 99; }\n",
    )
    .unwrap();
    git(repo, &["add", "."]);
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

fn run_context(repo: &TempDir, cache: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(repo.path())
        .env("SEM_CACHE_DIR", cache.path())
        .env("SEM_TIMINGS", "json")
        .args(["context"])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn context_second_call_answers_from_the_index_and_matches_authoritative() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());

    // Cold: no index yet (or one with no byte spans), walks and hydrates,
    // and — because this build has `SemanticEntity` bodies in scope — warms
    // the index with byte spans for next time.
    let cold = assert_success(
        run_context(&repo, &cache, &["mid", "--json"]),
        "cold context",
    );
    assert!(phase_names(&cold).contains(&"full_graph_build".to_string()));

    // Warm: same query should now answer from the index.
    let warm = assert_success(
        run_context(&repo, &cache, &["mid", "--json"]),
        "warm context",
    );
    let phases = phase_names(&warm);
    assert!(
        phases.iter().any(|p| p.starts_with("index_context")),
        "expected an index_context* phase, got {phases:?}"
    );
    assert!(!phases.contains(&"full_graph_build".to_string()));

    // Byte-identical against the always-authoritative path.
    let authoritative = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo.path())
            .env("SEM_CACHE_DIR", cache.path())
            .env("SEM_NO_INDEX", "1")
            .args(["context", "mid", "--json"])
            .output()
            .unwrap(),
        "authoritative context",
    );
    assert_eq!(
        warm.stdout, authoritative.stdout,
        "index-served context must match the authoritative walk byte-for-byte"
    );

    let warm_json: serde_json::Value = serde_json::from_slice(&warm.stdout).unwrap();
    let roles: Vec<&str> = warm_json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        vec!["target", "direct_dependency", "direct_dependent"],
        "mid's target/leaf/top shape, got {roles:?}"
    );
}

#[test]
fn context_matches_authoritative_in_text_mode_and_with_a_budget() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());
    assert_success(run_context(&repo, &cache, &["top", "--json"]), "warm cache");

    for extra in [vec![], vec!["--budget", "5"], vec!["--hops", "1"]] {
        let mut args = vec!["top"];
        args.extend(extra.iter().copied());
        let warm = assert_success(run_context(&repo, &cache, &args), "warm context");
        let phases = phase_names(&warm);
        assert!(
            phases.iter().any(|p| p.starts_with("index_context")),
            "{args:?}: expected index_context*, got {phases:?}"
        );

        let authoritative = assert_success(
            Command::new(env!("CARGO_BIN_EXE_sem"))
                .current_dir(repo.path())
                .env("SEM_CACHE_DIR", cache.path())
                .env("SEM_NO_INDEX", "1")
                .args(["context"])
                .args(&args)
                .output()
                .unwrap(),
            "authoritative context",
        );
        assert_eq!(
            warm.stdout, authoritative.stdout,
            "{args:?}: index-served context must match the authoritative walk"
        );
    }
}

#[test]
fn context_declines_the_index_for_an_ambiguous_name() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());
    // Give `leaf.ts` a second `mid`-named entity so the bare name is ambiguous.
    fs::write(
        repo.path().join("other.ts"),
        "export function unrelated() { return 99; }\nexport function mid() { return 2; }\n",
    )
    .unwrap();
    assert_success(run_context(&repo, &cache, &["top", "--json"]), "warm cache");

    // `mid` now names two entities: the index fast path (single-match only)
    // must decline and let the legacy path report the ambiguity.
    let output = run_context(&repo, &cache, &["mid", "--json"]);
    assert!(!output.status.success());
    let phases = phase_names_from_dry(&output);
    assert!(
        !phases.iter().any(|p| p.starts_with("index_context")),
        "ambiguous name must not be answered from the index, got {phases:?}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous"));
}

/// `phase_names` expects the timings JSON to be the *only* thing on stderr;
/// the ambiguity-decline path also prints a human error there, so this pulls
/// just the JSON line back out.
fn phase_names_from_dry(output: &Output) -> Vec<String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let Some(line) = stderr.lines().find(|l| l.trim_start().starts_with('{')) else {
        return Vec::new();
    };
    let timings: serde_json::Value = serde_json::from_str(line).expect("timings json line");
    timings["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|phase| phase["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn context_index_sees_a_new_file_that_calls_the_target() {
    let repo = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    init_repo(repo.path());
    assert_success(
        run_context(&repo, &cache, &["leaf", "--json"]),
        "warm cache",
    );

    // A brand-new file that calls `leaf` — the membership sweep
    // (`corpus_is_fresh`, same mechanism `impact --all`/`sem graph` use)
    // must catch this and decline the fast path rather than serve a
    // dependents list missing the new caller.
    fs::write(
        repo.path().join("newcaller.ts"),
        "import { leaf } from './leaf';\nexport function newcaller() { return leaf() + 1; }\n",
    )
    .unwrap();

    let output = assert_success(
        run_context(&repo, &cache, &["leaf", "--json"]),
        "context after new caller file appears",
    );
    let phases = phase_names(&output);
    assert!(
        !phases.iter().any(|p| p.starts_with("index_context")),
        "a brand-new file must force a decline to the legacy path, got {phases:?}"
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"newcaller"),
        "the new caller must appear in leaf's context, got {names:?}"
    );
}
