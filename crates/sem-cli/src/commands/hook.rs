//! `sem hook prompt-submit` — the prompt-time prefetch, compiled.
//!
//! Reads a Claude Code UserPromptSubmit event from stdin, extracts the
//! identifier-shaped tokens the prompt names, and answers them with `sem find
//! --context` from the query index (tens of milliseconds) before the model's
//! first turn, so a task that names its code starts with that code in hand
//! instead of spending a turn finding it.
//!
//! Silent by design: no candidates, no repo, no socket, any error — print
//! nothing and exit 0. The hook must never disturb a prompt.

use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_ENTITIES: usize = 4;
/// Token budget for everything the prefetch injects.
const BUDGET: &str = "1500";

pub fn prompt_submit() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(event) = serde_json::from_str::<serde_json::Value>(&input) else {
        return;
    };
    let prompt = event.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    let cwd = event.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
    if prompt.is_empty() || prompt.starts_with('/') || cwd.is_empty() {
        return;
    }
    let Some(repo_root) = find_repo_root(Path::new(cwd)) else {
        return;
    };

    // Precise path: the prompt names entities (`backticked`, snake_case,
    // CamelCase). Resolve them by exact name against the resident socket —
    // cheapest and sharpest when the caller already knows the identifier.
    let names = candidates(prompt);
    let mut blocks: Vec<String> = Vec::new();
    let paths = mentioned_paths(prompt, &repo_root);
    if let Some(text) = index_lookup(&repo_root, &names, &paths) {
        blocks.push(text);
    }
    if !blocks.is_empty() {
        println!(
            "<sem-prefetch>\nThe prompt references code entities; sem resolved them ahead of time \
             (entity body + direct callers/callees). Use this instead of searching; verify only if \
             something looks stale.\n\n{}\n</sem-prefetch>",
            blocks.join("\n\n")
        );
        return;
    }
}

/// Walk up from `start` to the repo root (the directory holding `.git`).
/// No subprocess: this is the whole reason the git CLI isn't invoked.
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Identifier shapes worth prefetching: backticked, snake_case with an
/// underscore, CamelCase, or qualified (`a.b` / `a::b`). Plain lowercase words
/// are deliberately excluded — they would inject noise on every prompt.
fn candidates(prompt: &str) -> Vec<String> {
    use regex::Regex;
    let backtick = Regex::new(r"`([A-Za-z_][\w.:]{2,60})`").unwrap();
    let qualified = Regex::new(r"\b[A-Za-z_]\w*(?:\.|::)[A-Za-z_]\w+\b").unwrap();
    let snake = Regex::new(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b").unwrap();
    let camel = Regex::new(r"\b[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+\b").unwrap();

    let stop = [
        "claude_code",
        "pull_request",
        "github_com",
        "https_www",
        "TypeScript",
        "JavaScript",
    ];

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut push = |tok: &str| {
        if tok.len() >= 4 && !stop.contains(&tok) && seen.insert(tok.to_string()) {
            out.push(tok.to_string());
        }
    };
    for m in backtick.captures_iter(prompt) {
        push(m.get(1).unwrap().as_str());
    }
    for rx in [&qualified, &snake, &camel] {
        for m in rx.find_iter(prompt) {
            push(m.as_str());
        }
    }
    out.truncate(MAX_ENTITIES);
    out
}

/// `sem find <name> --context` against the repository's query index, one
/// child process per name, all in parallel and bounded to two seconds. A
/// name that is ambiguous is retried inside each file the prompt mentions.
/// Only names that resolve to one definition are kept; the hook adds only
/// what is certain.
fn index_lookup(repo_root: &Path, names: &[String], paths: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let budget = (1500 / names.len()).max(300).to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let run = |name: &str, scope: Option<&str>| -> Option<String> {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["find", name, "--context", "--budget", &budget]);
        if let Some(p) = scope {
            cmd.args(["--in", p]);
        }
        let mut child = cmd
            .current_dir(repo_root)
            .env("SEM_NO_TELEMETRY", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    return None;
                }
            }
        }
        let mut out = String::new();
        child.stdout.take()?.read_to_string(&mut out).ok()?;
        (out.starts_with("context for ") && out.contains("target:")).then(|| out.trim_end().to_string())
    };
    let found: Vec<String> = std::thread::scope(|scope| {
        let jobs: Vec<_> = names
            .iter()
            .map(|n| scope.spawn(move || run(n, None).or_else(|| paths.iter().find_map(|p| run(n, Some(p))))))
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok().flatten()).collect()
    });
    // one block per entity (a qualified and a bare name can find the same
    // one), within the overall budget at about four bytes a token
    let mut seen = std::collections::HashSet::new();
    let mut out = String::new();
    for block in found {
        let head = block.lines().next().unwrap_or("").split(" (budget").next().unwrap_or("").to_string();
        if !seen.insert(head) {
            continue;
        }
        let room = 6000usize.saturating_sub(out.len());
        if room < 400 {
            break;
        }
        let mut cut = block.len().min(room);
        while !block.is_char_boundary(cut) {
            cut -= 1;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&block[..cut]);
    }
    (!out.is_empty()).then_some(out)
}

/// Repository files the prompt names (`rich/text.py`), which disambiguate
/// a name defined in several places.
fn mentioned_paths(prompt: &str, repo_root: &Path) -> Vec<String> {
    let rx = regex::Regex::new(r"[\w./-]+/[\w.-]+\.\w{1,6}").unwrap();
    let mut out: Vec<String> = Vec::new();
    for m in rx.find_iter(prompt) {
        let p = m.as_str().trim_start_matches("./").to_string();
        if repo_root.join(&p).is_file() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_pick_identifier_shapes_and_skip_prose() {
        let c = candidates("why is compute_history_analytics slow in `RepoWatcher` sessions?");
        assert_eq!(
            c,
            vec![
                "RepoWatcher".to_string(),
                "compute_history_analytics".to_string()
            ]
        );
        assert!(candidates("ok great I love this, what should we do next?").is_empty());
    }

    #[test]
    fn candidates_cap() {
        let c = candidates("`alpha_one` `beta_two` `gamma_three` `delta_four` `eps_five`");
        assert_eq!(c.len(), MAX_ENTITIES);
    }
}
