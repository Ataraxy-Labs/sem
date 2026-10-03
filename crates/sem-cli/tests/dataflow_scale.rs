//! Resource bounds of the data-flow engine on a synthetic deep call chain.
//!
//! The shape that exhausted memory on large Go services: a source passed
//! down a long chain of functions, each of which also hands its parameter
//! to code sem cannot resolve. When every summary carried the unresolved
//! calls reachable below it, each with its own witness path, memory grew
//! with the cube of the depth. Escapes are now resolved once at the end
//! by reachability, so the chain costs little.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn chain_repo(dir: &Path, depth: usize, per_file: usize) {
    let files = depth.div_ceil(per_file);
    for fi in 0..files {
        let mut src = String::new();
        for i in fi * per_file..((fi + 1) * per_file).min(depth) {
            let next = i + 1;
            if next < depth && next / per_file != fi {
                src += &format!("from chain_{} import f{next}\n", next / per_file);
            }
        }
        src += "\n";
        for i in fi * per_file..((fi + 1) * per_file).min(depth) {
            src += &format!("def f{i}(x, svc):\n    svc.load{i}(x)\n    svc.store{i}(x)\n");
            if i + 1 < depth {
                src += &format!("    return f{}(x, svc)\n", i + 1);
            }
            src += "\n";
        }
        fs::write(dir.join(format!("chain_{fi}.py")), src).unwrap();
    }
    fs::write(dir.join("main.py"), "import os\nfrom chain_0 import f0\n\ndef main(svc):\n    f0(os.environ['SEED'], svc)\n").unwrap();
}

fn rss_mb(pid: u32) -> Option<u64> {
    let o = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok().map(|kb| kb / 1024)
}

/// Run `sem dataflow --json` on `dir`; kill it past `max_mb` or `max_secs`.
/// Returns (peak MB, seconds, stdout) or the reason it was killed.
fn run_bounded(dir: &Path, max_mb: u64, max_secs: u64) -> Result<(u64, f64, String), String> {
    let out = dir.join("out.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sem"))
        .args(["dataflow", ".", "--json"])
        .current_dir(dir)
        .stdout(fs::File::create(&out).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .expect("run sem");
    let t0 = Instant::now();
    let mut peak = 0;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "sem dataflow failed");
            return Ok((peak, t0.elapsed().as_secs_f64(), fs::read_to_string(&out).unwrap()));
        }
        if let Some(m) = rss_mb(child.id()) {
            peak = peak.max(m);
            if m > max_mb {
                let _ = child.kill();
                return Err(format!("killed: {m} MB resident after {:.1}s", t0.elapsed().as_secs_f64()));
            }
        }
        if t0.elapsed() > Duration::from_secs(max_secs) {
            let _ = child.kill();
            return Err(format!("killed: over {max_secs}s (peak {peak} MB)"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_deep_chain_into_unresolved_code_stays_small() {
    let dir = tempfile::tempdir().unwrap();
    let depth = 600;
    chain_repo(dir.path(), depth, 50);
    let (peak, secs, out) = run_bounded(dir.path(), 1024, 120).unwrap_or_else(|e| panic!("{e}"));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    // every unresolved call below the source is still reported
    let escapes = v["escapes"].as_array().unwrap();
    assert_eq!(escapes.len(), depth, "one escape per function holding unresolved calls reached");
    assert!(escapes.iter().all(|e| e["source"]["class"] == "env"));
    eprintln!("depth {depth}: peak {peak} MB, {secs:.1}s");
    assert!(peak < 1024 && secs < 120.0);
}
