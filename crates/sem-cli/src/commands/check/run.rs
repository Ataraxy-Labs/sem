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

/// Commands that only filter the output piped into them.
const FILTERS: &[&str] = &["head", "tail", "grep", "egrep", "fgrep", "rg", "sed", "awk", "cut", "sort", "uniq", "wc", "tr", "cat", "less", "more", "column", "tee"];

/// `cmd` as (core command, output filters piped after it). `go test ./...
/// 2>&1 | grep FAIL | head` is `go test ./...` filtered by `grep FAIL |
/// head`: the core is what runs and is remembered, and each request applies
/// its own filters to the remembered output. A compound command (`;`, `&&`,
/// `||`, `&`, newlines) is its own core.
pub(crate) fn split_filters(cmd: &str) -> (String, Option<String>) {
    let b = cmd.as_bytes();
    let (mut quote, mut pipes, mut compound) = (None::<u8>, Vec::new(), false);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(b'"') if c == b'\\' => i += 1,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' => quote = Some(c),
                b'\\' => i += 1,
                b'|' if b.get(i + 1) == Some(&b'|') => {
                    compound = true;
                    i += 1;
                }
                b'|' => pipes.push(i),
                b';' | b'\n' => compound = true,
                b'&' if b.get(i + 1) == Some(&b'&') => {
                    compound = true;
                    i += 1;
                }
                b'&' if i > 0 && b[i - 1] != b'>' && b.get(i + 1) != Some(&b'>') => compound = true,
                _ => {}
            },
        }
        i += 1;
    }
    let strip = |s: &str| s.trim().trim_end_matches("2>&1").trim().to_string();
    if compound || pipes.is_empty() {
        return (if compound { cmd.trim().to_string() } else { strip(cmd) }, None);
    }
    let mut bounds = vec![0];
    bounds.extend(pipes.iter().map(|p| p + 1));
    let stages: Vec<&str> = bounds.iter().enumerate().map(|(k, &s)| &cmd[s..pipes.get(k).copied().unwrap_or(cmd.len())]).collect();
    let is_filter = |st: &str| st.split_whitespace().next().is_some_and(|w| FILTERS.contains(&w));
    let mut first = stages.len();
    while first > 1 && is_filter(stages[first - 1]) {
        first -= 1;
    }
    if first == stages.len() {
        return (cmd.trim().to_string(), None);
    }
    let core = stages[..first].join("|");
    let filters = stages[first..].iter().map(|s| s.trim()).collect::<Vec<_>>().join(" | ");
    (strip(&core), Some(filters))
}

/// One finished run of a core command: what is remembered per tree.
#[derive(serde::Serialize, serde::Deserialize)]
struct Raw {
    code: i32,
    stopped: Option<String>,
    secs: f64,
    lines: Vec<String>,
}

/// Answer from a finished run on this exact tree, join one in progress, or
/// run the core command and remember its whole output; then apply this
/// request's own filters to it.
fn memoized(start: &Path, cmd: &str, idle: u64, timeout: u64, json: bool, no_cache: bool, quiet: bool) -> (i32, String) {
    let (core, filters) = split_filters(cmd);
    let root = super::tree::toplevel(start);
    let dir = root.as_deref().and_then(run_dir);
    let tree = root.as_deref().and_then(|r| super::tree::head(r).ok()).map(|h| h.tree);
    let (Some(root), Some(dir), Some(tree)) = (root, dir, tree) else {
        let raw = execute(start, &core, idle, timeout);
        return render(&raw, cmd, filters.as_deref(), json, None);
    };
    let _ = std::fs::create_dir_all(&dir);
    let rel = start.strip_prefix(&root).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    if !quiet {
        let _ = std::fs::write(dir.join("last"), format!("{rel}\n{core}"));
    }
    let key = super::util::digest(&format!("{tree}\0{rel}\0{core}"));
    let (done, lock) = (dir.join(format!("{key}.json")), dir.join(format!("{key}.lock")));
    if !no_cache {
        // a run of this command on this tree is in progress: join it
        let waited = Instant::now();
        while let Ok(pid) = std::fs::read_to_string(&lock) {
            if !alive(pid.trim()) || waited.elapsed().as_secs() > timeout.max(1) {
                let _ = std::fs::remove_file(&lock);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Some(raw) = std::fs::read_to_string(&done).ok().and_then(|t| serde_json::from_str::<Raw>(&t).ok()) {
            let note = if waited.elapsed().as_millis() > 100 {
                format!("(`{core}` was already running on this exact tree, started when the last edit landed; waited {:.1}s for it)", waited.elapsed().as_secs_f64())
            } else {
                format!("(`{core}` already ran on this exact tree; answered from that run. --no-cache reruns it)")
            };
            return render(&raw, cmd, filters.as_deref(), json, Some(&note));
        }
    }
    let _ = std::fs::write(&lock, std::process::id().to_string());
    let raw = execute(start, &core, idle, timeout);
    // remember it only if the tree did not change while it ran, and it ended
    let same = super::tree::head(&root).ok().is_some_and(|h| h.tree == tree);
    if same && raw.stopped.is_none() {
        if let Ok(t) = serde_json::to_string(&raw) {
            let _ = std::fs::write(&done, t);
        }
    }
    let _ = std::fs::remove_file(&lock);
    render(&raw, cmd, filters.as_deref(), json, None)
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
    let (core, filters) = split_filters(cmd);
    let raw = execute(&dir, &core, idle, timeout);
    let (code, text) = render(&raw, cmd, filters.as_deref(), json, None);
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

/// Run `core` under the watchdog, keeping its whole output (both streams).
fn execute(root: &Path, core: &str, idle: u64, timeout: u64) -> Raw {
    const KEEP: usize = 20_000;
    let start = Instant::now();
    let bash = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"].into_iter().find(|b| Path::new(b).exists());
    let mut command = Command::new(bash.unwrap_or("sh"));
    command.arg("-c").arg(core).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Raw { code: 2, stopped: None, secs: 0.0, lines: vec![format!("sem check --run: {e}")] },
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
    let mut lines: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let mut last = Instant::now();
    let mut stopped: Option<String> = None;
    let mut closed = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                last = Instant::now();
                if lines.len() >= KEEP {
                    // keep the first half and a rolling end
                    lines.remove(KEEP / 2);
                    dropped += 1;
                }
                lines.push(line);
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
    if dropped > 0 {
        lines.insert(KEEP / 2, format!("... {dropped} lines not kept ..."));
    }
    Raw { code, stopped, secs: start.elapsed().as_secs_f64(), lines }
}

/// The answer to print: the request's own filters applied to the output, or
/// the lines that say what went wrong plus the end; then the verdict, which
/// is always the core command's.
fn render(raw: &Raw, cmd: &str, filters: Option<&str>, json: bool, note: Option<&str>) -> (i32, String) {
    use std::fmt::Write;
    let verdict = match (&raw.stopped, raw.code) {
        (Some(why), _) => why.clone(),
        (None, 0) => "pass".to_string(),
        (None, c) => format!("fail (exit {c})"),
    };
    let mut text = String::new();
    let shown: Vec<String> = match filters {
        Some(f) => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(f).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
            match c.spawn() {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        use std::io::Write as _;
                        let body = raw.lines.join("\n") + "\n";
                        std::thread::spawn(move || {
                            let _ = stdin.write_all(body.as_bytes());
                        });
                    }
                    child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect()).unwrap_or_default()
                }
                Err(_) => raw.lines.clone(),
            }
        }
        None => Vec::new(),
    };
    let tail: Vec<&String> = raw.lines.iter().rev().take(TAIL).rev().collect();
    let tail_start = raw.lines.len().saturating_sub(tail.len());
    let mut problems: Vec<&String> = Vec::new();
    for l in &raw.lines[..tail_start] {
        if problems.len() < SIGNAL_MAX && SIGNAL.iter().any(|s| l.contains(s)) && problems.last().is_none_or(|p| *p != l) {
            problems.push(l);
        }
    }
    if json {
        let out = serde_json::json!({
            "command": cmd,
            "verdict": if raw.stopped.is_some() { "stopped" } else if raw.code == 0 { "pass" } else { "fail" },
            "exit": raw.code,
            "seconds": (raw.secs * 10.0).round() / 10.0,
            "lines": raw.lines.len(),
            "output": if filters.is_some() { serde_json::json!(shown) } else { serde_json::json!({"problems": problems, "tail": tail}) },
            "note": note.or(raw.stopped.as_deref()),
        });
        let _ = writeln!(text, "{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return (raw.code, text);
    }
    if filters.is_some() {
        for l in &shown {
            let _ = writeln!(text, "{l}");
        }
        if !shown.is_empty() {
            text.push('\n');
        }
    } else {
        if !problems.is_empty() {
            let _ = writeln!(text, "── what went wrong ({} of {} lines):", problems.len(), raw.lines.len());
            for l in &problems {
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
    }
    let _ = writeln!(text, "{verdict} · {:.1}s · {} lines of output", raw.secs, raw.lines.len());
    if let Some(n) = note {
        let _ = writeln!(text, "{n}");
    }
    (raw.code, text)
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

#[cfg(test)]
mod tests {
    use super::split_filters;

    #[test]
    fn filters_split_from_the_command_they_filter() {
        let s = |c: &str| split_filters(c);
        assert_eq!(s("go test ./... 2>&1 | tail -15"), ("go test ./...".into(), Some("tail -15".into())));
        assert_eq!(s("cargo test | grep -E 'FAIL|panic' | head -20"), ("cargo test".into(), Some("grep -E 'FAIL|panic' | head -20".into())));
        assert_eq!(s("npx vitest run"), ("npx vitest run".into(), None));
        assert_eq!(s("go vet ./... && go test ./... | tail"), ("go vet ./... && go test ./... | tail".into(), None));
        assert_eq!(s("make 2>&1 | python3 parse.py"), ("make 2>&1 | python3 parse.py".into(), None));
    }
}
