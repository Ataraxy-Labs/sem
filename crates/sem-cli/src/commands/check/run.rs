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
        Err(e) => {
            eprintln!("sem check --run: {e}");
            return 2;
        }
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
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        if !early.is_empty() {
            println!("── what went wrong ({} of {} lines):", early.len(), lines);
            for l in &early {
                println!("{l}");
            }
            println!();
        }
        if !tail.is_empty() {
            println!("── last {} lines:", tail.len());
            for l in &tail {
                println!("{l}");
            }
            println!();
        }
        println!("{verdict} · {secs:.1}s · {lines} lines of output");
    }
    code
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
