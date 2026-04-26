use std::fs;
use std::path::PathBuf;
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
     exec sem diff \"$2\" \"$5\"\n"
        .to_string()
}

#[cfg(windows)]
fn wrapper_script() -> String {
    "@echo off\r\n\
     rem Wrapper for git diff.external: translates git's 7-arg format to sem diff\r\n\
     rem Args: path old-file old-hex old-mode new-file new-hex new-mode\r\n\
     sem diff \"%~2\" \"%~5\"\r\n"
        .to_string()
}

#[cfg(unix)]
fn wrapper_name() -> &'static str {
    "sem-diff-wrapper"
}

#[cfg(windows)]
fn wrapper_name() -> &'static str {
    "sem-diff-wrapper.bat"
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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = wrapper_path();
    let dir = path.parent().unwrap();

    // Create wrapper directory if needed
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        println!(
            "{} Created {}",
            "✓".green().bold(),
            dir.display()
        );
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
        .args(["config", "--global", "diff.external", wrapper_name()])
        .status()?;
    if !status.success() {
        return Err("Failed to set diff.external in git config".into());
    }
    println!(
        "{} Set git config --global diff.external = {}",
        "✓".green().bold(),
        wrapper_name(),
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

fn resolve_hooks_dir() -> Option<PathBuf> {
    // Respect core.hooksPath if set
    let hooks_path = Command::new("git")
        .args(["config", "core.hooksPath"])
        .output();

    if let Ok(output) = &hooks_path {
        if output.status.success() {
            let custom = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !custom.is_empty() {
                let p = PathBuf::from(&custom);
                // Could be relative to repo root
                if p.is_absolute() {
                    return Some(p);
                }
                // Resolve relative to working tree
                if let Ok(wt) = Command::new("git")
                    .args(["rev-parse", "--show-toplevel"])
                    .output()
                {
                    if wt.status.success() {
                        let root = String::from_utf8_lossy(&wt.stdout).trim().to_string();
                        return Some(PathBuf::from(root).join(custom));
                    }
                }
            }
        }
    }

    // Default: .git/hooks
    let git_dir = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output();

    match git_dir {
        Ok(output) if output.status.success() => {
            let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Some(PathBuf::from(dir).join("hooks"))
        }
        _ => None,
    }
}

fn install_pre_commit_hook() {
    let hooks_dir = match resolve_hooks_dir() {
        Some(d) => d,
        None => return, // Not in a git repo, skip
    };

    if !hooks_dir.exists() {
        let _ = fs::create_dir_all(&hooks_dir);
    }

    let hook_path = hooks_dir.join("pre-commit");

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
    // Unset diff.external
    let status = Command::new("git")
        .args(["config", "--global", "--unset", "diff.external"])
        .status()?;
    if status.success() {
        println!(
            "{} Removed diff.external from global git config",
            "✓".green().bold(),
        );
    } else {
        println!(
            "{} diff.external was not set in global git config",
            "✓".green().bold(),
        );
    }

    // Remove wrapper script
    let path = wrapper_path();
    if path.exists() {
        fs::remove_file(&path)?;
        println!(
            "{} Removed wrapper script at {}",
            "✓".green().bold(),
            path.display()
        );
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
    let hooks_dir = match resolve_hooks_dir() {
        Some(d) => d,
        None => return,
    };

    let hook_path = hooks_dir.join("pre-commit");
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
        println!(
            "{} Removed sem-only pre-commit hook",
            "✓".green().bold()
        );
    } else {
        let _ = fs::write(&hook_path, format!("{}\n", result.trim_end()));
        println!(
            "{} Removed sem section from pre-commit hook",
            "✓".green().bold()
        );
    }
}
