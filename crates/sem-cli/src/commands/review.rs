//! `sem review listen <diff-id-or-url>` — the one-command agent attach.
//!
//! Wraps the manual recipe documented in
//! `integrations/claude-review-listener/README.md` (join a sem-cloud review
//! as a live listener via `claude --plugin-dir ...`) into a single
//! subcommand: normalize the id/URL, resolve credentials exactly like every
//! other cloud command, validate the diff actually exists, locate the
//! bundled plugin, and exec `claude` with the right flags/env.
//!
//! Credential + endpoint resolution intentionally reuses
//! `sem_mcp::agent_review::AgentReviewConfig` (env-first: `SEM_CLOUD_URL` /
//! `SEM_API_KEY`, falling back to `~/.sem/credentials.json`) rather than
//! reimplementing it — this is the exact precedence the listener session's
//! own MCP tools (`join_review` / `wait_for_branch` / `reply_to_branch`)
//! resolve at runtime, so what this command validates is what the listener
//! will actually use.

use std::path::{Path, PathBuf};
use std::process::Command;

use colored::Colorize;
use sem_mcp::agent_review::{AgentReviewClient, AgentReviewConfig};

/// Default model for the listener session; overridable via `SEM_LISTENER_MODEL`.
const DEFAULT_MODEL: &str = "claude-opus-5";

/// Tool allowlist for the non-interactive `claude` session: the plugin's
/// three (now four) MCP review tools, namespaced per Claude Code's
/// plugin-bundled-MCP convention (`mcp__plugin_<plugin>_<server>__<tool>`,
/// with plugin "sem-review-listener" and server "sem-review" — see the
/// plugin's own README "Contract doubts" §1), plus the read tools the
/// listener protocol requires for investigating each question in-repo.
const ALLOWED_TOOLS: [&str; 4] = [
    "mcp__plugin_sem-review-listener_sem-review__*",
    "Read",
    "Grep",
    "Glob",
];

/// `sem review listen <diff-id-or-url> [--dry-run]`.
pub(crate) fn listen(
    diff_id_or_url: &str,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let diff_id = normalize_diff_id(diff_id_or_url)?;

    let config = AgentReviewConfig::from_env_or_credentials()?;
    let client = AgentReviewClient::new(config.clone());
    client.manifest(&diff_id).map_err(|e| {
        format!(
            "Could not find review '{diff_id}' on {} — {e}\n\
             Check the diff id/URL, and that SEM_CLOUD_URL/SEM_API_KEY (or `sem login`) point at \
             the right sem-cloud account.",
            config.base_url
        )
    })?;

    // Resolve every remaining env-derived input for this launch here, at the
    // command entry, right after the network validation above (preserving
    // the existing error precedence: "diff not found" still wins over "no
    // plugin dir" when both are wrong). `build_launch_plan` below is then a
    // pure function of its inputs — it never reads env itself.
    let plugin_dir = locate_plugin_dir()?;
    let model = listener_model();
    let plan = build_launch_plan(&diff_id, &config, &plugin_dir, &model);

    if dry_run {
        print_dry_run(&plan);
        return Ok(());
    }

    if find_claude_on_path().is_none() {
        return Err(format!(
            "`claude` was not found on PATH. Install the Claude Code CLI first \
             (https://code.claude.com/docs/en/get-started), then re-run:\n  \
             sem review listen {diff_id_or_url}"
        )
        .into());
    }

    println!(
        "{} diff {} with {} (model {})…",
        "Launching claude-review-listener for".bold(),
        diff_id.cyan(),
        "claude".bold(),
        plan.model,
    );

    exec_claude(&plan)
}

/// Everything needed to launch (or describe) a listener session: which
/// plugin dir, which model, the full `claude` argv, and the environment to
/// set on it. Assembled once by [`build_launch_plan`] — a pure function of
/// its inputs, with [`locate_plugin_dir`]/[`listener_model`] resolved by the
/// caller (`listen()`) beforehand — and consumed two ways: [`print_dry_run`]
/// renders exactly this plan instead of launching, and [`exec_claude`] execs
/// exactly this plan. Neither path can drift from the other because there is
/// only one place the plan is built.
#[derive(Debug)]
struct LaunchPlan {
    plugin_dir: PathBuf,
    model: String,
    /// Full argv passed to `claude` (not including the `claude` program name
    /// itself).
    args: Vec<String>,
    /// Environment variables set on the `claude` process, in display order.
    /// `SEM_API_KEY`'s value is masked by [`print_dry_run`], never here.
    env: Vec<(String, String)>,
}

fn build_launch_plan(
    diff_id: &str,
    config: &AgentReviewConfig,
    plugin_dir: &Path,
    model: &str,
) -> LaunchPlan {
    let prompt = join_prompt(diff_id);
    let args = assemble_args(plugin_dir, model, &prompt);
    let env = vec![
        ("SEM_CLOUD_URL".to_string(), config.base_url.clone()),
        ("SEM_API_KEY".to_string(), config.api_key.clone()),
        ("SEM_REVIEW_DIFF_ID".to_string(), diff_id.to_string()),
    ];

    LaunchPlan {
        plugin_dir: plugin_dir.to_path_buf(),
        model: model.to_string(),
        args,
        env,
    }
}

/// Accept either a bare diff id or a hosted review URL
/// (`.../diffs/{id}` or `.../diffs/{id}/canvas`, with any query/fragment)
/// and return just the id.
fn normalize_diff_id(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("expected a diff id or a hosted review URL, got an empty string".to_string());
    }

    let Some((_, after)) = trimmed.split_once("/diffs/") else {
        if trimmed.contains('/') {
            return Err(format!(
                "could not find a diff id in '{trimmed}' — expected a bare diff id, or a URL \
                 containing '/diffs/{{id}}'"
            ));
        }
        // No "/diffs/" and no slash at all: treat as a bare diff id.
        return Ok(trimmed.to_string());
    };

    let after = after.split(['?', '#']).next().unwrap_or(after);
    let id = after
        .split('/')
        .find(|s| !s.is_empty())
        .ok_or_else(|| format!("could not find a diff id in '{trimmed}'"))?;
    Ok(id.to_string())
}

fn listener_model() -> String {
    std::env::var("SEM_LISTENER_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

fn join_prompt(diff_id: &str) -> String {
    format!(
        "Join the sem-cloud review for diff {diff_id} using join_review, then follow the \
         listener protocol it returns. Never end the session yourself."
    )
}

fn assemble_args(plugin_dir: &Path, model: &str, prompt: &str) -> Vec<String> {
    let mut args = vec![
        "--plugin-dir".to_string(),
        plugin_dir.display().to_string(),
        "--model".to_string(),
        model.to_string(),
        "--allowedTools".to_string(),
    ];
    args.extend(ALLOWED_TOOLS.iter().map(|s| s.to_string()));
    args.push("-p".to_string());
    args.push(prompt.to_string());
    args
}

/// Locate the `claude-review-listener` plugin directory: an explicit
/// `SEM_LISTENER_PLUGIN_DIR` override wins outright (and must exist); else
/// walk up from the running `sem` binary looking for
/// `integrations/claude-review-listener` — true whether `sem` is a debug
/// build under `crates/target/debug/sem` or a release build, since either
/// way the plugin lives at a fixed number of ancestors above the binary in
/// a checkout of this repo. Errors with exactly what to set otherwise,
/// since a `sem` installed outside this repo (homebrew, npm, curl) has no
/// nearby plugin directory to find.
fn locate_plugin_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("SEM_LISTENER_PLUGIN_DIR") {
        let path = PathBuf::from(&dir);
        return if path.is_dir() {
            Ok(path)
        } else {
            Err(format!(
                "SEM_LISTENER_PLUGIN_DIR is set to '{dir}', but that path does not exist or is \
                 not a directory."
            ))
        };
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        for _ in 0..8 {
            let Some(d) = dir else { break };
            let candidate = d.join("integrations").join("claude-review-listener");
            if candidate
                .join(".claude-plugin")
                .join("plugin.json")
                .is_file()
            {
                return Ok(candidate);
            }
            dir = d.parent();
        }
    }

    Err(
        "Could not find the claude-review-listener plugin directory near the sem binary.\n\
         Set SEM_LISTENER_PLUGIN_DIR to its path, e.g.:\n  \
         export SEM_LISTENER_PLUGIN_DIR=/path/to/sem/integrations/claude-review-listener"
            .to_string(),
    )
}

/// Best-effort `claude` detection: scan `PATH` for an executable file named
/// `claude` (`claude.exe` on Windows). Not a guarantee it actually runs —
/// just enough to give a friendly install pointer instead of an opaque exec
/// failure in the common case (not installed at all).
fn find_claude_on_path() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn mask_secret(secret: &str) -> String {
    if secret.is_empty() {
        "(empty)".to_string()
    } else if secret.len() > 12 {
        format!("{}…{}", &secret[..6], &secret[secret.len() - 4..])
    } else {
        "****".to_string()
    }
}

fn shell_quote(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

fn print_dry_run(plan: &LaunchPlan) {
    println!("{}", "[dry-run] would launch:".bold());
    let quoted: Vec<String> = plan.args.iter().map(|a| shell_quote(a)).collect();
    println!("  claude {}", quoted.join(" "));
    println!();
    println!("{}", "environment:".bold());
    for (key, value) in &plan.env {
        let display_value = if key == "SEM_API_KEY" {
            mask_secret(value)
        } else {
            value.clone()
        };
        println!("  {key}={display_value}");
    }
    println!();
    println!("plugin dir: {}", plan.plugin_dir.display());
    println!("model: {}", plan.model);
}

#[cfg(unix)]
fn exec_claude(plan: &LaunchPlan) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;

    // On success this never returns: the process image is replaced in
    // place, so the listener session inherits this process's stdio/pid
    // directly instead of running as a child sem never releases.
    let err = Command::new("claude")
        .args(&plan.args)
        .envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .exec();
    Err(format!("failed to exec claude: {err}").into())
}

#[cfg(not(unix))]
fn exec_claude(plan: &LaunchPlan) -> Result<(), Box<dyn std::error::Error>> {
    // No process-replace exec on this platform: spawn, wait, and mirror the
    // child's exit code.
    let status = Command::new("claude")
        .args(&plan.args)
        .envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .status()
        .map_err(|e| format!("failed to launch claude: {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_bare_id() {
        assert_eq!(
            normalize_diff_id("4c1ca0f5-fbf8-4347-b0f0-244d1d91e92d").unwrap(),
            "4c1ca0f5-fbf8-4347-b0f0-244d1d91e92d"
        );
    }

    #[test]
    fn normalize_trims_whitespace_around_bare_id() {
        assert_eq!(normalize_diff_id("  abc123  ").unwrap(), "abc123");
    }

    #[test]
    fn normalize_extracts_id_from_plain_diffs_url() {
        assert_eq!(
            normalize_diff_id("https://sem-cloud.fly.dev/diffs/abc123").unwrap(),
            "abc123"
        );
    }

    #[test]
    fn normalize_strips_canvas_suffix() {
        assert_eq!(
            normalize_diff_id("https://sem-cloud.fly.dev/diffs/abc123/canvas").unwrap(),
            "abc123"
        );
    }

    #[test]
    fn normalize_strips_query_and_fragment() {
        assert_eq!(
            normalize_diff_id("https://sem-cloud.fly.dev/diffs/abc123?tab=files#L10").unwrap(),
            "abc123"
        );
        assert_eq!(
            normalize_diff_id("https://sem-cloud.fly.dev/diffs/abc123/canvas?x=1").unwrap(),
            "abc123"
        );
    }

    #[test]
    fn normalize_works_without_scheme() {
        assert_eq!(
            normalize_diff_id("sem-cloud.fly.dev/diffs/abc123/canvas").unwrap(),
            "abc123"
        );
    }

    #[test]
    fn normalize_rejects_empty_input() {
        assert!(normalize_diff_id("").is_err());
        assert!(normalize_diff_id("   ").is_err());
    }

    #[test]
    fn normalize_rejects_slash_bearing_input_without_diffs_segment() {
        let err = normalize_diff_id("https://example.com/some/other/path").unwrap_err();
        assert!(err.contains("/diffs/"));
    }

    #[test]
    fn mask_secret_never_leaks_a_short_key() {
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret(""), "(empty)");
    }

    #[test]
    fn mask_secret_keeps_ends_visible_for_a_long_key() {
        let masked = mask_secret("sem_1234567890abcdef");
        assert!(masked.starts_with("sem_12"));
        assert!(masked.ends_with("cdef"));
        assert!(!masked.contains("1234567890"));
    }

    #[test]
    fn assemble_args_matches_the_documented_invocation_shape() {
        let args = assemble_args(
            Path::new("/plugins/claude-review-listener"),
            "claude-opus-5",
            "hello",
        );
        assert_eq!(
            args,
            vec![
                "--plugin-dir",
                "/plugins/claude-review-listener",
                "--model",
                "claude-opus-5",
                "--allowedTools",
                "mcp__plugin_sem-review-listener_sem-review__*",
                "Read",
                "Grep",
                "Glob",
                "-p",
                "hello",
            ]
        );
    }

    #[test]
    fn join_prompt_carries_the_diff_id_and_never_end_instruction() {
        let prompt = join_prompt("abc123");
        assert!(prompt.contains("abc123"));
        assert!(prompt.contains("join_review"));
        assert!(prompt.contains("Never end the session yourself"));
    }

    /// `build_launch_plan` is the single source both `--dry-run` printing
    /// and the real `exec_claude` launch consume — this pins its shape so
    /// the two consumers can't silently drift from each other. It's a pure
    /// function of its arguments now (plugin dir / model resolved by the
    /// caller), so no env mutation is needed to exercise it.
    #[test]
    fn build_launch_plan_assembles_args_and_env_from_a_resolved_plugin_dir() {
        let dir = tempfile::tempdir().unwrap();

        let config = AgentReviewConfig {
            base_url: "https://sem-cloud.example".to_string(),
            api_key: "sem_test_key".to_string(),
        };
        let plan = build_launch_plan("abc123", &config, dir.path(), DEFAULT_MODEL);

        assert_eq!(plan.plugin_dir, dir.path());
        assert_eq!(plan.model, DEFAULT_MODEL);
        assert_eq!(
            plan.args,
            assemble_args(dir.path(), DEFAULT_MODEL, &join_prompt("abc123"))
        );
        assert_eq!(
            plan.env,
            vec![
                (
                    "SEM_CLOUD_URL".to_string(),
                    "https://sem-cloud.example".to_string()
                ),
                ("SEM_API_KEY".to_string(), "sem_test_key".to_string()),
                ("SEM_REVIEW_DIFF_ID".to_string(), "abc123".to_string()),
            ]
        );
    }

    // `SEM_LISTENER_PLUGIN_DIR` is process-global; serialize the tests that
    // mutate it so they can't interleave across cargo's parallel test
    // threads. (Missing-plugin-dir error coverage lives in
    // `locate_plugin_dir_errors_clearly_when_override_is_missing` below —
    // `build_launch_plan` no longer resolves the plugin dir itself.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn locate_plugin_dir_respects_explicit_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SEM_LISTENER_PLUGIN_DIR", dir.path());
        let found = locate_plugin_dir().unwrap();
        std::env::remove_var("SEM_LISTENER_PLUGIN_DIR");
        assert_eq!(found, dir.path());
    }

    #[test]
    fn locate_plugin_dir_errors_clearly_when_override_is_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEM_LISTENER_PLUGIN_DIR", "/definitely/not/a/real/path/xyz");
        let err = locate_plugin_dir().unwrap_err();
        std::env::remove_var("SEM_LISTENER_PLUGIN_DIR");
        assert!(err.contains("SEM_LISTENER_PLUGIN_DIR"));
    }
}
