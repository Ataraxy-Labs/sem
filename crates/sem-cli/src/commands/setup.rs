use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use colored::Colorize;
use serde_json::{json, Value};

#[cfg(unix)]
fn wrapper_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/bin/sem-diff-wrapper")
}

#[cfg(windows)]
fn wrapper_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".to_string());
    PathBuf::from(home).join(".local\\bin\\sem-diff-wrapper.bat")
}

#[cfg(unix)]
fn wrapper_script() -> String {
    "#!/bin/sh\n\
     # Wrapper for git diff.external: translates git's 7-arg format to sem diff\n\
     # Args: path old-file old-hex old-mode new-file new-hex new-mode\n\
     exec sem diff --label \"$1\" \"$2\" \"$5\"\n"
        .to_string()
}

#[cfg(windows)]
fn wrapper_script() -> String {
    "@echo off\r\n\
     rem Wrapper for git diff.external: translates git's 7-arg format to sem diff\r\n\
     rem Args: path old-file old-hex old-mode new-file new-hex new-mode\r\n\
     sem diff --label \"%~1\" \"%~2\" \"%~5\"\r\n"
        .to_string()
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(windows)]
fn set_executable(_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    // .bat files are executable by default on Windows
    Ok(())
}

fn diff_external_value() -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--global", "--get", "diff.external"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn is_configured_wrapper(value: &str, wrapper_path: &Path) -> bool {
    let value_path = Path::new(value);

    value
        == wrapper_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
        || value_path == wrapper_path
        || matches!(
            (
                std::fs::canonicalize(value_path),
                std::fs::canonicalize(wrapper_path)
            ),
            (Ok(value), Ok(wrapper)) if value == wrapper
        )
}

fn wrapper_is_owned(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|content| content == wrapper_script())
}

fn git_path(path: &str) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", path])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if resolved.is_empty() {
        return None;
    }

    let path = PathBuf::from(resolved);
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = wrapper_path();
    let dir = path.parent().unwrap();

    // Create wrapper directory if needed
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        println!("{} Created {}", "✓".green().bold(), dir.display());
    }

    // Write wrapper script
    fs::write(&path, wrapper_script())?;
    set_executable(&path)?;
    println!(
        "{} Created wrapper script at {}",
        "✓".green().bold(),
        path.display()
    );

    // Set diff.external globally
    let status = Command::new("git")
        .arg("config")
        .arg("--global")
        .arg("diff.external")
        .arg(&path)
        .status()?;
    if !status.success() {
        return Err("Failed to set diff.external in git config".into());
    }
    println!(
        "{} Set git config --global diff.external = {}",
        "✓".green().bold(),
        path.display(),
    );

    // Install pre-commit hook if we're in a git repo
    install_pre_commit_hook();

    // Install the session-default hooks for Claude Code: a warm resident graph
    // and prompt-time context injection, so structural queries are instant and
    // the code an agent would forage for arrives at turn zero.
    install_session_hooks();

    println!(
        "\n{} Running `git diff` in any repo will now use sem.",
        "Done!".green().bold()
    );
    println!("  Pre-commit hook shows entity-level blast radius of staged changes.");
    println!("  Claude Code sessions get a warm graph + prompt-time context (free, local).");
    println!("  sem-mcp server available for agent integration.");
    println!("  To revert, run: sem unsetup");

    Ok(())
}

/// Path to Claude Code's user settings file, where session hooks live.
fn claude_settings_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".claude").join("settings.json")
}

/// True for the exact hook commands `sem setup` installs. These substrings are
/// sem-specific (no other tool ships `mcp --resident` or `hook prompt-submit`),
/// so matching them will not remove a user's unrelated hooks.
fn is_sem_hook_command(cmd: &str) -> bool {
    cmd.contains("mcp --resident") || cmd.contains("hook prompt-submit")
}

/// True if a Claude Code hook entry is one sem installed (any of its `hooks`
/// commands is a sem hook command).
fn entry_is_sem_hook(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_sem_hook_command)
            })
        })
}

/// Add the two session hooks to a parsed settings object, preserving every
/// other key and every user-defined hook. Idempotent: returns the number of
/// hooks newly added (0 if both were already present).
fn add_session_hooks(root: &mut Value, resident_cmd: &str, prompt_cmd: &str) -> usize {
    let Some(obj) = root.as_object_mut() else {
        return 0;
    };
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return 0;
    };
    let mut added = 0;

    let session = hooks.entry("SessionStart").or_insert_with(|| json!([]));
    if let Some(arr) = session.as_array_mut() {
        if !arr.iter().any(entry_is_sem_hook) {
            arr.push(json!({
                "matcher": "",
                "hooks": [{ "type": "command", "command": resident_cmd }]
            }));
            added += 1;
        }
    }

    let prompt = hooks.entry("UserPromptSubmit").or_insert_with(|| json!([]));
    if let Some(arr) = prompt.as_array_mut() {
        if !arr.iter().any(entry_is_sem_hook) {
            arr.push(json!({
                "hooks": [{ "type": "command", "command": prompt_cmd }]
            }));
            added += 1;
        }
    }

    added
}

/// Remove only the sem-installed session hooks from a parsed settings object,
/// leaving every user hook and every other key intact. Empty hook arrays and an
/// empty `hooks` object are cleaned up so `add` then `remove` round-trips to the
/// original shape. Returns true if anything changed.
fn remove_session_hooks(root: &mut Value) -> bool {
    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    let hooks_empty;
    {
        let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
            return false;
        };
        for key in ["SessionStart", "UserPromptSubmit"] {
            let mut now_empty = false;
            if let Some(arr) = hooks.get_mut(key).and_then(|a| a.as_array_mut()) {
                let before = arr.len();
                arr.retain(|e| !entry_is_sem_hook(e));
                if arr.len() != before {
                    changed = true;
                }
                now_empty = arr.is_empty();
            }
            if now_empty {
                hooks.remove(key);
                changed = true;
            }
        }
        hooks_empty = hooks.is_empty();
    }
    if hooks_empty {
        obj.remove("hooks");
        changed = true;
    }
    changed
}

#[cfg(unix)]
fn install_session_hooks() {
    let sem = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "sem".to_string());
    // Detach the resident server so the SessionStart hook returns immediately.
    let resident = format!("nohup {sem} mcp --resident >/dev/null 2>&1 &");
    let prompt = format!("{sem} hook prompt-submit");

    let path = claude_settings_path();

    // Read + parse the existing settings. Parse failure means we do NOT touch
    // the file — a corrupt write here would break the user's Claude Code config.
    let mut root: Value = if path.exists() {
        match fs::read_to_string(&path) {
            Ok(s) if s.trim().is_empty() => json!({}),
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(_) => {
                    println!(
                        "{} {} is not valid JSON; leaving it untouched (session hooks skipped)",
                        "note:".yellow().bold(),
                        path.display()
                    );
                    return;
                }
            },
            Err(_) => return,
        }
    } else {
        json!({})
    };

    if !root.is_object() {
        println!(
            "{} {} is not a JSON object; leaving it untouched (session hooks skipped)",
            "note:".yellow().bold(),
            path.display()
        );
        return;
    }

    // Back up before the first modification.
    if path.exists() {
        let backup = path.with_extension("json.sem-backup");
        if !backup.exists() {
            let _ = fs::copy(&path, &backup);
        }
    }

    let added = add_session_hooks(&mut root, &resident, &prompt);
    if added == 0 {
        println!(
            "{} Claude Code session hooks already installed",
            "✓".green().bold()
        );
        return;
    }

    if let Some(dir) = path.parent() {
        if !dir.exists() {
            let _ = fs::create_dir_all(dir);
        }
    }
    if let Ok(s) = serde_json::to_string_pretty(&root) {
        if fs::write(&path, format!("{s}\n")).is_ok() {
            println!(
                "{} Installed Claude Code session hooks (warm graph + prompt-time context) in {}",
                "✓".green().bold(),
                path.display()
            );
        }
    }
}

#[cfg(windows)]
fn install_session_hooks() {
    println!(
        "{} Claude Code session hooks (warm graph + prompt-time context) are not installed on Windows yet; git diff integration is active.",
        "note:".yellow().bold()
    );
}

#[cfg(unix)]
fn remove_session_hooks_file() {
    let path = claude_settings_path();
    if !path.exists() {
        return;
    }
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut root: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };
    if !root.is_object() {
        return;
    }
    if remove_session_hooks(&mut root) {
        if let Ok(s) = serde_json::to_string_pretty(&root) {
            if fs::write(&path, format!("{s}\n")).is_ok() {
                println!("{} Removed Claude Code session hooks", "✓".green().bold());
            }
        }
    }
}

#[cfg(windows)]
fn remove_session_hooks_file() {}

#[cfg(test)]
mod session_hook_tests {
    use super::*;

    #[test]
    fn add_is_idempotent_and_preserves_other_keys() {
        let mut root = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo hi" }] }
                ]
            }
        });
        let n = add_session_hooks(&mut root, "nohup sem mcp --resident &", "sem hook prompt-submit");
        assert_eq!(n, 2, "both hooks added on a fresh config");
        assert_eq!(root["model"], "opus", "unrelated keys preserved");
        assert_eq!(root["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "echo hi");
        let n2 =
            add_session_hooks(&mut root, "nohup sem mcp --resident &", "sem hook prompt-submit");
        assert_eq!(n2, 0, "second install adds nothing (idempotent)");
    }

    #[test]
    fn remove_keeps_user_hooks_and_restores_shape() {
        let mut root = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo hi" }] }
                ]
            }
        });
        add_session_hooks(&mut root, "nohup sem mcp --resident &", "sem hook prompt-submit");
        assert!(remove_session_hooks(&mut root));
        assert_eq!(
            root["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "echo hi",
            "user hook survives removal"
        );
        assert!(
            root["hooks"].get("SessionStart").is_none(),
            "sem SessionStart hook removed and empty array cleaned"
        );
        assert!(root["hooks"].get("UserPromptSubmit").is_none());
    }

    #[test]
    fn add_then_remove_roundtrips_to_no_hooks() {
        let mut root = json!({});
        add_session_hooks(&mut root, "a mcp --resident", "a hook prompt-submit");
        remove_session_hooks(&mut root);
        assert!(
            root.as_object().unwrap().get("hooks").is_none(),
            "empty hooks object removed so shape matches the original"
        );
    }

    #[test]
    fn remove_from_config_without_sem_hooks_is_noop() {
        let mut root = json!({ "model": "x" });
        assert!(!remove_session_hooks(&mut root));
        assert_eq!(root["model"], "x");
    }
}

const SEM_HOOK_START: &str = "# === sem pre-commit hook ===";
const SEM_HOOK_END: &str = "# === end sem ===";

fn pre_commit_hook_section() -> String {
    format!(
        "{}\n\
         if command -v sem >/dev/null 2>&1; then\n\
         \x20   sem diff --staged 2>/dev/null\n\
         fi\n\
         {}\n",
        SEM_HOOK_START, SEM_HOOK_END
    )
}

fn resolve_pre_commit_hook_path() -> Option<PathBuf> {
    git_path("hooks/pre-commit")
}

fn install_pre_commit_hook() {
    let hook_path = match resolve_pre_commit_hook_path() {
        Some(p) => p,
        None => return, // Not in a git repo, skip
    };
    let hooks_dir = match hook_path.parent() {
        Some(d) => d,
        None => return,
    };

    if !hooks_dir.exists() {
        let _ = fs::create_dir_all(hooks_dir);
    }

    if hook_path.exists() {
        // Append sem section if not already present
        let existing = fs::read_to_string(&hook_path).unwrap_or_default();
        if existing.contains(SEM_HOOK_START) {
            println!(
                "{} Pre-commit hook already has sem section",
                "✓".green().bold()
            );
            return;
        }
        // Back up the existing hook
        let backup = hooks_dir.join("pre-commit.sem-backup");
        if !backup.exists() {
            if fs::copy(&hook_path, &backup).is_ok() {
                println!(
                    "{} Backed up existing hook to {}",
                    "✓".green().bold(),
                    backup.display()
                );
            }
        }
        let updated = format!("{}\n{}", existing.trim_end(), pre_commit_hook_section());
        if fs::write(&hook_path, updated).is_ok() {
            let _ = set_executable(&hook_path);
            println!(
                "{} Appended sem section to existing pre-commit hook",
                "✓".green().bold()
            );
        }
    } else {
        // Create new hook (exit 0 inside markers so unsetup cleans up fully)
        let content = format!("#!/bin/sh\n{}", pre_commit_hook_section());
        if fs::write(&hook_path, content).is_ok() {
            let _ = set_executable(&hook_path);
            println!(
                "{} Created pre-commit hook at {}",
                "✓".green().bold(),
                hook_path.display()
            );
        }
    }
}

pub fn unsetup() -> Result<(), Box<dyn std::error::Error>> {
    let path = wrapper_path();
    let existing_diff_external = diff_external_value();
    let wrapper_configured = existing_diff_external
        .as_deref()
        .is_some_and(|value| is_configured_wrapper(value, &path));

    match existing_diff_external {
        Some(value) if wrapper_configured => {
            let status = Command::new("git")
                .args(["config", "--global", "--unset", "diff.external"])
                .status()?;
            if status.success() {
                println!(
                    "{} Removed diff.external from global git config",
                    "✓".green().bold(),
                );
            }
        }
        Some(value) => {
            println!(
                "{} Leaving diff.external untouched ({})",
                "note:".yellow().bold(),
                value
            );
        }
        None => {
            println!(
                "{} diff.external was not set in global git config",
                "✓".green().bold(),
            );
        }
    }

    if path.exists() {
        if wrapper_configured && wrapper_is_owned(&path) {
            fs::remove_file(&path)?;
            println!(
                "{} Removed wrapper script at {}",
                "✓".green().bold(),
                path.display()
            );
        } else {
            println!(
                "{} leaving {} untouched (not owned by this sem install)",
                "note:".yellow().bold(),
                path.display()
            );
        }
    }

    // Remove pre-commit hook section
    remove_pre_commit_hook();

    // Remove the Claude Code session hooks (leaves any user hooks intact).
    remove_session_hooks_file();

    println!(
        "\n{} git diff restored to default behavior.",
        "Done!".green().bold()
    );

    Ok(())
}

fn remove_pre_commit_hook() {
    let hook_path = match resolve_pre_commit_hook_path() {
        Some(p) => p,
        None => return,
    };
    if !hook_path.exists() {
        return;
    }

    let content = match fs::read_to_string(&hook_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    if !content.contains(SEM_HOOK_START) {
        return;
    }

    // Remove the sem section
    let lines: Vec<&str> = content.lines().collect();
    let mut new_lines = Vec::new();
    let mut in_sem_section = false;

    for line in &lines {
        if line.contains(SEM_HOOK_START) {
            in_sem_section = true;
            continue;
        }
        if line.contains(SEM_HOOK_END) {
            in_sem_section = false;
            continue;
        }
        if !in_sem_section {
            new_lines.push(*line);
        }
    }

    let result = new_lines.join("\n");

    // Check if only boilerplate remains (shebang, exit 0, whitespace)
    let meaningful: Vec<&str> = result
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && t != "#!/bin/sh" && t != "#!/bin/bash" && t != "exit 0"
        })
        .collect();

    if meaningful.is_empty() {
        let _ = fs::remove_file(&hook_path);
        println!("{} Removed sem-only pre-commit hook", "✓".green().bold());
    } else {
        let _ = fs::write(&hook_path, format!("{}\n", result.trim_end()));
        println!(
            "{} Removed sem section from pre-commit hook",
            "✓".green().bold()
        );
    }
}
