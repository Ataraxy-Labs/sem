//! `sem hook prompt-submit` — the prompt-time prefetch, compiled.
//!
//! Reads a Claude Code UserPromptSubmit event from stdin, extracts
//! identifier-shaped tokens from the prompt, resolves them against the
//! resident sem MCP server's socket sidecar (warm in-memory graph,
//! single-digit ms), and prints the packed entity context to stdout — which
//! the harness injects into the model's context before it starts thinking.
//!
//! Silent by design: no candidates, no repo, no socket, any error — print
//! nothing and exit 0. The hook must never disturb a prompt.

use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_ENTITIES: usize = 2;
const BUDGET: usize = 900;

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
    for name in &names {
        if let Some(text) = socket_lookup(&repo_root, name) {
            blocks.push(text);
        }
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

    // Plain-English fallback: most real tasks ("fix the empty-name bug") name no
    // entity at all, so the precise path resolves nothing and the hook used to
    // stay silent. Instead, orient on the whole prompt — a tight, IDF-ranked
    // briefing of the code most likely relevant — so plain-language tasks skip
    // the search too. Gated on a substantial prompt; orient self-gates (prints
    // nothing) when no code matches, so casual chatter injects nothing.
    if prompt.len() >= 40 && is_code_task(prompt) {
        if let Some(brief) = orient_fallback(&repo_root, prompt) {
            println!(
                "<sem-prefetch>\nsem located the code most likely relevant to this task ahead of \
                 time (top entities, body + neighbors). Start from these instead of grepping; \
                 verify only if something looks off.\n\n{}\n</sem-prefetch>",
                brief
            );
        }
    }
}

/// Does this prompt look like a coding or code-understanding task? Keeps the
/// orient fallback from firing on meta/chat ("explain the philosophy", "are we
/// faster") — which have no code signal — while still catching plain-English
/// bug reports and navigation questions that name no entity.
fn is_code_task(prompt: &str) -> bool {
    let p = prompt.to_lowercase();
    // Multi-word signals: substring is fine, they're specific.
    const PHRASES: &[&str] = &[
        "does not", "doesn't", "not work", "add a ", "add an ", "add support",
        "should return", "should raise", "should not",
        "where is", "how does", "which function", "the code that", "where does",
        "the function", "the method", "the class", "responsible for",
    ];
    if PHRASES.iter().any(|s| p.contains(s)) {
        return true;
    }
    // Single-word signals: whole-word match, so "raise" doesn't fire on
    // "fundraise" or "fail" on "detail".
    const WORDS: &[&str] = &[
        "fix", "bug", "error", "errors", "fail", "fails", "crash", "exception",
        "exceptions", "raise", "raises", "broken", "implement", "refactor",
        "rename", "handle", "validate", "parse", "regression", "traceback",
        "throw", "throws", "assert",
    ];
    p.split(|c: char| !c.is_alphanumeric())
        .any(|w| WORDS.contains(&w))
}

/// Plain-English fallback: run `orient --pack` on the whole prompt to get a
/// tight briefing of the most relevant code. Fast on a cached repo (tens of
/// ms); hard-capped so a cold or huge repo can never stall a prompt.
fn orient_fallback(repo_root: &Path, prompt: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mut child = std::process::Command::new(exe)
        .args(["orient", "--pack", "1500", prompt])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let out = out.trim();
    // "(no entities matched the task text)" or empty → nothing useful to inject.
    if out.is_empty() || out.starts_with('(') {
        return None;
    }
    Some(out.to_string())
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

/// One-call context from the resident server's socket sidecar. None on any
/// failure — the caller stays silent (no slow fallback at prompt time; a
/// missing server just means no prefetch this prompt).
#[cfg(unix)]
fn socket_lookup(repo_root: &Path, name: &str) -> Option<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let path = sem_mcp::sidecar::socket_path_for(repo_root)?;
    let mut stream = UnixStream::connect(&path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(250)))
        .ok()?;
    let req = serde_json::json!({ "op": "context", "name": name, "budget": BUDGET, "hops": 1 });
    stream.write_all(req.to_string().as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let resp: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        resp.get("text").and_then(|v| v.as_str()).map(String::from)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn socket_lookup(_repo_root: &Path, _name: &str) -> Option<String> {
    None
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
    fn candidates_cap_at_two() {
        let c = candidates("`alpha_one` `beta_two` `gamma_three`");
        assert_eq!(c.len(), 2);
    }
}
