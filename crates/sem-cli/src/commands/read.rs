//! `sem find --read`: read files the way an agent should get them.
//!
//! Numbered like `cat -n`, any text file, several in one call. Within one
//! agent session (`SEM_SESSION`, which the agent's hook sets) reads are
//! remembered under `<git common dir>/sem-run/reads`, so a file read again
//! answers in one line when unchanged and with a diff against the version
//! last read when it changed; `--fresh` prints it whole. Without a session
//! nothing is remembered: a new session never hears "unchanged" about a file
//! it has not seen.

use std::path::{Path, PathBuf};

fn store(root: &Path) -> Option<PathBuf> {
    let o = std::process::Command::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).current_dir(root).output().ok()?;
    o.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("sem-run").join("reads"))
}

fn digest(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn range(spec: &str) -> Option<(usize, Option<usize>)> {
    let s = spec.trim().trim_matches(['"', '\'']);
    match s.split_once('-') {
        Some((a, b)) => Some((a.trim().parse().ok()?, if b.trim().is_empty() { None } else { Some(b.trim().parse().ok()?) })),
        None => s.parse().ok().map(|n| (n, Some(n))),
    }
}

pub fn run(files: &[String], lines: Option<&str>, fresh: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session = std::env::var("SEM_SESSION").ok().filter(|s| !s.is_empty());
    let dir = session.as_ref().and_then(|s| store(&cwd).map(|d| d.join(digest(s))));
    if let Some(d) = &dir {
        let _ = std::fs::create_dir_all(d);
    }
    let span = match lines.map(range) {
        Some(None) => {
            eprintln!("--lines: use a range like 40-120 or 300-");
            return 2;
        }
        Some(Some(r)) => Some(r),
        None => None,
    };
    let mut code = 0;
    for f in files {
        let path = cwd.join(f);
        let text = match std::fs::read(&path) {
            Ok(b) if b.contains(&0) => {
                println!("── {f}: binary file, {} bytes\n", b.len());
                continue;
            }
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                println!("── {f}: {e}\n");
                code = 1;
                continue;
            }
        };
        let total = text.lines().count();
        let (from, to) = span.map(|(a, b)| (a.max(1), b.unwrap_or(total).min(total))).unwrap_or((1, total));
        let slice: String = text.lines().skip(from.saturating_sub(1)).take((to + 1).saturating_sub(from)).map(|l| format!("{l}\n")).collect();
        let what = if (from, to) == (1, total) { format!("{total} lines") } else { format!("lines {from}-{to} of {total}") };
        let abs = path.canonicalize().unwrap_or(path.clone());
        let key = dir.as_ref().map(|d| d.join(digest(&format!("{}\0{}", abs.display(), lines.unwrap_or("all")))));
        let prev = key.as_ref().and_then(|k| std::fs::read_to_string(k).ok());
        if let Some(k) = &key {
            let _ = std::fs::write(k, &slice);
        }
        match prev {
            Some(p) if !fresh && p == slice => {
                println!("── {f} · {what} · unchanged since you last read it (--fresh prints it)\n");
                continue;
            }
            Some(p) if !fresh => {
                let diff = similar::TextDiff::from_lines(p.as_str(), slice.as_str()).unified_diff().context_radius(2).header("as last read", "now").to_string();
                if diff.len() < slice.len() / 2 {
                    println!("── {f} · {what} · changed since you last read it:\n{diff}");
                    continue;
                }
            }
            _ => {}
        }
        println!("── {f} · {what}");
        for (i, l) in slice.lines().enumerate() {
            println!("{:>6}\t{l}", from + i);
        }
        println!();
    }
    code
}
