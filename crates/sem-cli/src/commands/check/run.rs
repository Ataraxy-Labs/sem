//! `sem check --run '<cmd>'`: run one project command (a build, a test
//! suite) the way an agent should see it.
//!
//! Two things make long agent sessions slow that have nothing to do with the
//! code: a test that hangs until the shell's timeout, and a log of thousands
//! of lines read back into the model. The command runs in its own process
//! group under a watchdog that stops it once it has printed nothing for
//! `idle` seconds, and the answer is the lines that say what went wrong plus
//! the end of the output, not the whole log. The exit code is the command's;
//! a stopped command exits 124.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Lines that say what went wrong, in most build and test tools' words.
const SIGNAL: &[&str] = &[
    "error", "Error", "ERROR", "FAILED", "FAIL", "Failed", "failed", "failure", "FAILURE", "panicked", "Exception",
    "assert", "Traceback", "expected", "Caused by", "undefined", "cannot find", "not found", "warning: unused",
];
const TAIL: usize = 40;
const SIGNAL_MAX: usize = 80;

pub(crate) fn run(root: &Path, cmd: &str, idle: u64, timeout: u64, json: bool) -> i32 {
    let (code, text) = execute(root, cmd, idle, timeout, json);
    print!("{text}");
    code
}

/// Run `cmd` on the base revision instead of the working tree, in a side
/// checkout that persists between calls (`<git dir>/sem-base/<commit>`), so
/// the working tree and its build stay as they are. Its build directories
/// start as copy-on-write clones of the working tree's (instant on APFS,
/// btrfs and XFS), so the base build is incremental, and its answers are
/// cached per (base commit, command).
pub(crate) fn run_at_base(start: &Path, base: &str, cmd: &str, idle: u64, timeout: u64, json: bool, no_cache: bool) -> i32 {
    let Some(root) = super::tree::toplevel(start) else {
        eprintln!("sem check --at base: not in a git repository");
        return 2;
    };
    let Some(rev) = super::tree::rev(&root, base) else {
        eprintln!("sem check --at base: {base} is not a revision");
        return 2;
    };
    let git = |args: &[&str], dir: &Path| Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).output();
    let common = match git(&["rev-parse", "--path-format=absolute", "--git-common-dir"], &root) {
        Ok(o) if o.status.success() => std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()),
        _ => root.join(".git"),
    };
    let short = &rev.commit[..rev.commit.len().min(12)];
    let dir = common.join("sem-base").join(short);
    if !dir.join(".git").exists() {
        let _ = std::fs::create_dir_all(dir.parent().unwrap_or(&dir));
        match git(&["worktree", "add", "--detach", "--force", &dir.to_string_lossy(), &rev.commit], &root) {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                eprintln!("sem check --at base: {}", String::from_utf8_lossy(&o.stderr).trim());
                return 2;
            }
            Err(e) => {
                eprintln!("sem check --at base: {e}");
                return 2;
            }
        }
        warm(&root, &dir);
    }
    let cache = dir.join(".git-sem-run").join(format!("{}.txt", super::util::digest(&format!("{json}\0{cmd}"))));
    let header = format!("at base {short} ({}):\n", dir.display());
    if !no_cache {
        if let Ok(saved) = std::fs::read_to_string(&cache) {
            if let Some((code, text)) = saved.split_once('\n') {
                print!("{header}{text}(cached: this command already ran on this base)\n");
                return code.parse().unwrap_or(1);
            }
        }
    }
    let (code, text) = execute(&dir, cmd, idle, timeout, json);
    print!("{header}{text}");
    if code != 124 && !no_cache {
        let _ = std::fs::create_dir_all(cache.parent().unwrap_or(&dir));
        let _ = std::fs::write(&cache, format!("{code}\n{text}"));
    }
    code
}

/// Clone the working tree's build directories into the base checkout, so
/// the first base build reuses what is already built.
fn warm(root: &Path, dir: &Path) {
    const BUILD: &[&str] = &["target", "build", "node_modules", ".gradle", "out", "dist", ".venv", "venv", "vendor", "bin", "obj"];
    for name in BUILD {
        let (from, to) = (root.join(name), dir.join(name));
        if !from.is_dir() || to.exists() {
            continue;
        }
        let ok = if cfg!(target_os = "macos") {
            Command::new("cp").arg("-c").arg("-R").arg(&from).arg(&to).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
        } else {
            Command::new("cp").args(["--reflink=auto", "-a"]).arg(&from).arg(&to).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
        };
        if !ok {
            let _ = std::fs::remove_dir_all(&to);
        }
    }
}

/// Run `cmd` under the watchdog: (exit code, the answer to print).
fn execute(root: &Path, cmd: &str, idle: u64, timeout: u64, json: bool) -> (i32, String) {
    use std::fmt::Write;
    let start = Instant::now();
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return (2, format!("sem check --run: {e}\n")),
    };
    let (tx, rx) = mpsc::channel::<String>();
    for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)].into_iter().flatten() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut signal: Vec<(usize, String)> = Vec::new();
    let mut lines = 0usize;
    let mut last = Instant::now();
    let mut stopped: Option<String> = None;
    let mut closed = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                lines += 1;
                last = Instant::now();
                if SIGNAL.iter().any(|s| line.contains(s)) && signal.len() < SIGNAL_MAX && signal.last().is_none_or(|(_, l)| *l != line) {
                    signal.push((lines, line.clone()));
                }
                tail.push_back(line);
                if tail.len() > TAIL {
                    tail.pop_front();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => closed = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Ok(Some(_)) = child.try_wait() {
            if closed {
                break;
            }
            continue;
        }
        if closed {
            std::thread::sleep(Duration::from_millis(50));
        }
        let quiet = last.elapsed().as_secs();
        if idle > 0 && quiet >= idle {
            stopped = Some(format!("stopped: no output for {quiet}s, so it is probably hung"));
        } else if timeout > 0 && start.elapsed().as_secs() >= timeout {
            stopped = Some(format!("stopped: still running after {timeout}s"));
        }
        if stopped.is_some() {
            kill_group(&mut child);
            break;
        }
    }
    let status = child.wait().ok();
    let code = if stopped.is_some() { 124 } else { status.and_then(|s| s.code()).unwrap_or(1) };
    let secs = start.elapsed().as_secs_f64();

    // the signal lines not already in the tail, then the tail
    let tail_start = lines.saturating_sub(tail.len()) + 1;
    let early: Vec<&String> = signal.iter().filter(|(n, _)| *n < tail_start).map(|(_, l)| l).collect();
    let verdict = match (&stopped, code) {
        (Some(why), _) => why.clone(),
        (None, 0) => "pass".to_string(),
        (None, c) => format!("fail (exit {c})"),
    };
    let mut text = String::new();
    if json {
        let out = serde_json::json!({
            "command": cmd,
            "verdict": if stopped.is_some() { "stopped" } else if code == 0 { "pass" } else { "fail" },
            "exit": code,
            "seconds": (secs * 10.0).round() / 10.0,
            "lines": lines,
            "problems": early,
            "tail": tail,
            "note": stopped,
        });
        let _ = writeln!(text, "{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        if !early.is_empty() {
            let _ = writeln!(text, "── what went wrong ({} of {} lines):", early.len(), lines);
            for l in &early {
                let _ = writeln!(text, "{l}");
            }
            text.push('\n');
        }
        if !tail.is_empty() {
            let _ = writeln!(text, "── last {} lines:", tail.len());
            for l in &tail {
                let _ = writeln!(text, "{l}");
            }
            text.push('\n');
        }
        let _ = writeln!(text, "{verdict} · {secs:.1}s · {lines} lines of output");
    }
    (code, text)
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id().to_string();
        let quiet = |sig: &str| Command::new("kill").args([sig, &format!("-{pid}")]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = quiet("-TERM");
        std::thread::sleep(Duration::from_millis(500));
        let _ = quiet("-KILL");
    }
    let _ = child.kill();
}
