//! `sem arch-diff` memory on a huge repository with a tiny change.
//!
//! Every agent edit goes through `arch-diff`, so its memory must follow the
//! change, not the repository. Whole-tree analysis held both trees' entity
//! graphs, parse trees and data flow at once: on 230-270 MB monorepos it
//! went past 6 GB before reporting anything. Above the size whose whole-tree
//! analysis would not fit `--max-memory`, arch-diff now analyzes the diff's
//! region (the changed files, their callers, importers and the definitions
//! they call) and says so.
//!
//! `SEM_BIN=<path>` runs another binary on the same repo (to show the old
//! one exceeds the bound); `SEM_SCALE_DIR=<dir>` generates the repo there and
//! keeps it.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const PACKAGES: usize = 500;
const MODULES: usize = 100;
/// The bound the test holds arch-diff to.
const MAX_MB: u64 = 1024;

fn git(repo: &Path, args: &[&str]) {
    let o = Command::new("git").current_dir(repo).args(args).output().expect("run git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

/// One module: four functions; `f0` calls the next module's `f0`, `f3` calls
/// the next package's `f1` (so every `f1` has a caller in another package).
fn module(p: usize, m: usize, changed: bool) -> String {
    let mut s = String::new();
    let (np, nm) = ((p + 1) % PACKAGES, m + 1);
    writeln!(s, "\"\"\"Module {m} of package {p}.\n\nA store and four steps of a pipeline: each step loads its input from the service,\nhands it to the next module's step, and records what it did. The service is\ninjected so that tests can replace it; nothing here touches the network or the\nfile system directly, and every external effect goes through `svc`.\n\"\"\"\n").unwrap();
    s += "import os\nimport subprocess\n";
    if nm < MODULES {
        writeln!(s, "from pkg_{p}.mod_{nm} import p{p}_m{nm}_f0").unwrap();
    }
    writeln!(s, "from pkg_{np}.mod_{m} import p{np}_m{m}_f1\n").unwrap();
    writeln!(s, "\nclass Store{p}x{m}:\n    def __init__(self, svc):\n        self.svc = svc\n        self.items = []\n").unwrap();
    writeln!(s, "    def put(self, key, value):\n        self.items.append((key, value))\n        return self.svc.save(key, value)\n").unwrap();
    writeln!(s, "\ndef p{p}_m{m}_f0(x, svc):\n    y = svc.load(x)\n    z = [i for i in range(10) if i % 2]").unwrap();
    if nm < MODULES {
        writeln!(s, "    return p{p}_m{nm}_f0(y, svc)\n").unwrap();
    } else {
        s += "    return z\n\n";
    }
    if changed {
        // a required parameter added: the caller in the previous package breaks
        writeln!(s, "\ndef p{p}_m{m}_f1(x, svc, mode):\n    total = 0\n    for i in range(x):\n        total += svc.weight(i, mode)\n    return total\n").unwrap();
        // and a new path from the environment into a command
        writeln!(s, "\ndef p{p}_m{m}_f2(svc):\n    cmd = os.environ['CMD']\n    subprocess.run(cmd, shell=True)\n    return svc.done()\n").unwrap();
    } else {
        writeln!(s, "\ndef p{p}_m{m}_f1(x, svc):\n    total = 0\n    for i in range(x):\n        total += svc.weight(i)\n    return total\n").unwrap();
        writeln!(s, "\ndef p{p}_m{m}_f2(svc):\n    cmd = 'true'\n    subprocess.run(cmd, shell=True)\n    return svc.done()\n").unwrap();
    }
    writeln!(s, "\ndef p{p}_m{m}_f3(x, svc):\n    store = Store{p}x{m}(svc)\n    store.put('k', x)\n    return p{np}_m{m}_f1(x, svc)\n").unwrap();
    s
}

fn huge_repo(r: &Path) {
    for p in 0..PACKAGES {
        let d = r.join(format!("pkg_{p}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("__init__.py"), "").unwrap();
        for m in 0..MODULES {
            fs::write(d.join(format!("mod_{m}.py")), module(p, m, false)).unwrap();
        }
    }
    for args in [&["init", "-q"][..], &["config", "user.email", "t@example.com"], &["config", "user.name", "T"]] {
        git(r, args);
    }
    git(r, &["add", "."]);
    git(r, &["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    fs::write(r.join("pkg_250/mod_50.py"), module(250, 50, true)).unwrap();
    git(r, &["-c", "commit.gpgsign=false", "commit", "-qam", "head"]);
}

fn rss_mb(pid: u32) -> Option<u64> {
    let o = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok().map(|kb| kb / 1024)
}

/// `sem arch-diff HEAD~1..HEAD --json`, killed past `max_mb` or `max_secs`.
fn run_bounded(dir: &Path, max_mb: u64, max_secs: u64) -> Result<(u64, f64, String), String> {
    let out = dir.join("..").join(format!("{}.json", dir.file_name().unwrap().to_string_lossy()));
    let bin = std::env::var("SEM_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_sem").to_string());
    let mut child = Command::new(bin)
        .args(["arch-diff", "HEAD~1..HEAD", "--json"])
        .current_dir(dir)
        .stdout(fs::File::create(&out).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .expect("run sem");
    let t0 = Instant::now();
    let mut peak = 0;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "sem arch-diff failed");
            return Ok((peak, t0.elapsed().as_secs_f64(), fs::read_to_string(&out).unwrap()));
        }
        if let Some(m) = rss_mb(child.id()) {
            peak = peak.max(m);
            if m > max_mb {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("killed: {m} MB resident after {:.1}s", t0.elapsed().as_secs_f64()));
            }
        }
        if t0.elapsed() > Duration::from_secs(max_secs) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("killed: over {max_secs}s (peak {peak} MB)"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_tiny_change_in_a_50k_file_repo_stays_under_a_gigabyte() {
    let tmp = tempfile::tempdir().unwrap();
    let r = std::env::var("SEM_SCALE_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| tmp.path().join("repo"));
    fs::create_dir_all(&r).unwrap();
    let t0 = Instant::now();
    huge_repo(&r);
    eprintln!("generated {} files in {:.1}s", PACKAGES * MODULES, t0.elapsed().as_secs_f64());
    let (peak, secs, out) = run_bounded(&r, MAX_MB, 300).unwrap_or_else(|e| panic!("{e}"));
    eprintln!("arch-diff: peak {peak} MB, {secs:.1}s");
    let v: Value = serde_json::from_str(&out).unwrap();

    // the repo is too big to analyze whole within --max-memory: diff-scoped, and said so
    assert_eq!(v["scope"]["mode"], "diff", "{}", v["scope"]);
    assert!(v["scope"]["filesInRepo"].as_u64().unwrap() >= (PACKAGES * MODULES) as u64);
    // the region: the changed module, its package, and the files naming what it defines
    assert!(v["scope"]["bytesAnalyzed"].as_u64().unwrap() * 20 < v["scope"]["bytesInRepo"].as_u64().unwrap(), "{}", v["scope"]);

    let findings = v["findings"].as_array().unwrap();
    // the broken caller in another package is found, exactly as a whole-tree run finds it
    let sig = findings
        .iter()
        .find(|f| f["kind"] == "signature-change" && f["data"]["entity"] == "p250_m50_f1")
        .unwrap_or_else(|| panic!("no signature change: {out}"));
    assert_eq!(sig["severity"], "high");
    let callers: Vec<&str> = sig["data"]["callers"].as_array().unwrap().iter().map(|c| c["file"].as_str().unwrap()).collect();
    assert_eq!(callers, ["pkg_249/mod_50.py"]);
    assert!(sig["data"]["callersNotSearched"].is_null(), "the callers of a unique name are searched in full");
    // and the new path from the environment into a command
    assert!(
        findings.iter().any(|f| f["kind"] == "new-data-path" && f["data"]["source"]["class"] == "env" && f["data"]["sink"]["class"] == "exec"),
        "no env -> exec path: {out}"
    );
    assert!(peak < MAX_MB);
}
