use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use colored::Colorize;

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

    println!(
        "\n{} Running `git diff` in any repo will now use sem.",
        "Done!".green().bold()
    );
    println!("  Pre-commit hook shows entity-level blast radius of staged changes.");
    println!("  sem-mcp server available for agent integration.");
    println!("  To revert, run: sem unsetup");

    Ok(())
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
