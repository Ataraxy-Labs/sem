//! `sem check --run '<cmd>'`: run one project command (a build, a test
//! suite) the way an agent should see it.
//!
//! Two things make long agent sessions slow that have nothing to do with the
//! code: a test that hangs until the shell's timeout, and a log of thousands
//! of lines read back into the model. The command runs in its own process
//! group under a watchdog that stops it once it has printed nothing for
//! `idle` seconds, and the answer is the lines that say what went wrong plus
//! the end of the output, not the whole log. The exit code is the command's
//! (pipes included: a failing test piped through `tail` still fails); a
//! stopped command exits 124.
//!
//! Runs are keyed by the working tree's content (a git tree id, untracked
//! files included) and the command. The same command on the same tree is
//! answered from the first run, and a run already in progress for that key,
//! such as one weave started speculatively right after an edit, is joined
//! instead of started again. That is how test time overlaps the model's
//! thinking: the run starts when the edit lands, not when the agent asks.

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

pub(crate) fn run(root: &Path, cmd: &str, idle: u64, timeout: u64, json: bool, no_cache: bool, quiet: bool) -> i32 {
    let (code, text) = memoized(root, cmd, idle, timeout, json, no_cache, quiet);
    if quiet {
        return code;
    }
    print!("{text}");
    if !json {
        if let Some(hint) = scope_hint(root, cmd) {
            println!("{hint}");
        }
    }
    code
}

/// `<git common dir>/sem-run`: run results, runs in progress, the last command.
pub(crate) fn run_dir(root: &Path) -> Option<std::path::PathBuf> {
    let o = Command::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).current_dir(root).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("sem-run"))
}

fn alive(pid: &str) -> bool {
    Command::new("kill").args(["-0", pid]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Answer from a finished run on this exact tree, join one in progress, or
/// run the command and remember the answer.
fn memoized(start: &Path, cmd: &str, idle: u64, timeout: u64, json: bool, no_cache: bool, quiet: bool) -> (i32, String) {
    let root = super::tree::toplevel(start);
    let dir = root.as_deref().and_then(run_dir);
    let tree = root.as_deref().and_then(|r| super::tree::head(r).ok()).map(|h| h.tree);
    let (Some(root), Some(dir), Some(tree)) = (root, dir, tree) else {
        return execute(start, cmd, idle, timeout, json);
    };
    let _ = std::fs::create_dir_all(&dir);
    let rel = start.strip_prefix(&root).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    if !quiet {
        let _ = std::fs::write(dir.join("last"), format!("{rel}\n{cmd}"));
    }
    let key = super::util::digest(&format!("{tree}\0{rel}\0{json}\0{cmd}"));
    let (done, lock) = (dir.join(format!("{key}.done")), dir.join(format!("{key}.lock")));
    if !no_cache {
        // a run of this command on this tree is in progress: join it
        let waited = Instant::now();
        while let Ok(pid) = std::fs::read_to_string(&lock) {
            if !alive(pid.trim()) || waited.elapsed().as_secs() > timeout.max(1) {
                let _ = std::fs::remove_file(&lock);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Ok(saved) = std::fs::read_to_string(&done) {
            if let Some((code, text)) = saved.split_once('\n') {
                let joined = waited.elapsed().as_millis() > 150;
                let note = if joined {
                    "(this exact tree was already being checked, started when the last edit landed; waited for it)\n"
                } else {
                    "(answered from an earlier run of this command on this exact tree; --no-cache reruns it)\n"
                };
                return (code.parse().unwrap_or(1), format!("{text}{note}"));
            }
        }
    }
    let _ = std::fs::write(&lock, std::process::id().to_string());
    let (code, text) = execute(start, cmd, idle, timeout, json);
    // remember it only if the tree did not change while it ran, and it ended
    let same = super::tree::head(&root).ok().is_some_and(|h| h.tree == tree);
    if same && code != 124 {
        let _ = std::fs::write(&done, format!("{code}\n{text}"));
    }
    let _ = std::fs::remove_file(&lock);
    (code, text)
}

/// When `cmd` builds a whole multi-module project but only some modules
/// changed (against HEAD, untracked files included), the command that
/// builds just those: the next build cycle costs a module, not the project.
fn scope_hint(start: &Path, cmd: &str) -> Option<String> {
    let root = super::tree::toplevel(start)?;
    let out = Command::new("git").args(["status", "--porcelain=v1", "--untracked-files=all", "--no-renames"]).current_dir(&root).output().ok()?;
    let changed: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.get(3..)).map(|p| p.trim_matches('"').to_string()).collect();
    if changed.is_empty() {
        return None;
    }
    let files = super::tree::files(&root, "HEAD").unwrap_or_default();
    // the nearest ancestor directory holding `manifest`, for every changed file
    let modules_of = |manifest: &[&str]| -> (Vec<String>, usize) {
        let dirs: std::collections::BTreeSet<String> = files
            .iter()
            .filter(|f| manifest.iter().any(|m| f.rsplit('/').next() == Some(m)))
            .map(|f| f.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default())
            .collect();
        let mut hit = std::collections::BTreeSet::new();
        for c in &changed {
            let mut d = c.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
            loop {
                if dirs.contains(&d) {
                    if !d.is_empty() {
                        hit.insert(d.clone());
                    }
                    break;
                }
                match d.rsplit_once('/') {
                    Some((p, _)) => d = p.to_string(),
                    None if !d.is_empty() => d = String::new(),
                    None => break,
                }
            }
        }
        (hit.into_iter().collect(), dirs.len())
    };
    let words: Vec<&str> = cmd.split(|c: char| c.is_whitespace() || c == '\'' || c == '"').collect();
    let has = |w: &str| words.contains(&w);
    let pick = |mods: &[String], total: usize, render: &dyn Fn(&[String]) -> String| -> Option<String> {
        (!mods.is_empty() && total > 2 && mods.len() * 2 <= total).then(|| {
            format!("hint: only {} of {total} modules changed ({}); `{}` rebuilds just those", mods.len(), mods.join(", "), render(mods))
        })
    };
    if (has("mvn") || has("./mvnw") || has("mvnw")) && !has("-pl") && !has("--projects") {
        let (mods, total) = modules_of(&["pom.xml"]);
        return pick(&mods, total, &|m| format!("{} -pl {}", cmd.trim(), m.join(",")));
    }
    if (has("gradle") || has("./gradlew") || has("gradlew")) && !words.iter().any(|w| w.starts_with(':')) {
        let (mods, total) = modules_of(&["build.gradle", "build.gradle.kts"]);
        return pick(&mods, total, &|m| m.iter().map(|d| format!(":{}:build", d.replace('/', ":"))).collect::<Vec<_>>().join(" "));
    }
    if has("cargo") && !has("-p") && !has("--package") && !has("--workspace") {
        let (mods, total) = modules_of(&["Cargo.toml"]);
        let names: Vec<String> = mods
            .iter()
            .filter_map(|d| {
                let toml = std::fs::read_to_string(root.join(d).join("Cargo.toml")).ok()?;
                let pkg = toml.split("[package]").nth(1)?;
                let line = pkg.lines().find(|l| l.trim_start().starts_with("name"))?;
                Some(line.split('=').nth(1)?.trim().trim_matches('"').to_string())
            })
            .collect();
        return pick(&names, total, &|m| format!("{} {}", cmd.trim(), m.iter().map(|n| format!("-p {n}")).collect::<Vec<_>>().join(" ")));
    }
    None
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
    // bash with pipefail when there is one, so `cmd | tail` keeps cmd's failure
    let bash = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"].into_iter().find(|b| Path::new(b).exists());
    let mut command = match bash {
        Some(b) => {
            let mut c = Command::new(b);
            c.args(["-o", "pipefail", "-c", cmd]);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(cmd);
            c
        }
    };
    command.current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
