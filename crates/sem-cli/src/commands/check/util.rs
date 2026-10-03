//! Process running with a time limit, scratch directories, digests.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A temporary directory removed when dropped.
pub(crate) struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> std::io::Result<Scratch> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("sem-check-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d)?;
        Ok(Scratch(d))
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) struct Ran {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.status.success()
    }
    /// The last `n` lines of stdout and stderr together.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let mut lines: Vec<String> = self
            .stdout
            .lines()
            .chain(self.stderr.lines())
            .map(str::to_string)
            .collect();
        if lines.len() > n {
            lines.drain(..lines.len() - n);
        }
        lines
    }
}

/// Run `cmd` to completion or until `limit`, whichever is first. The child is
/// its own process group, so a timeout stops everything it started.
pub(crate) fn run(mut cmd: Command, limit: Duration) -> Result<Ran, String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let what = format!("{:?}", cmd.get_program());
    let mut child = cmd.spawn().map_err(|e| format!("could not start {what}: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let to = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let te = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if t0.elapsed() > limit => {
                #[cfg(unix)]
                unsafe {
                    libc_kill(-(child.id() as i32));
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} did not finish within {}s", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("waiting for {what}: {e}")),
        }
    };
    let stdout = String::from_utf8_lossy(&to.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&te.join().unwrap_or_default()).to_string();
    Ok(Ran { status, stdout, stderr })
}

#[cfg(unix)]
unsafe fn libc_kill(pgid: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pgid, 9);
}

/// A shell command run in `dir`.
pub(crate) fn sh(dir: &Path, script: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script).current_dir(dir);
    c
}

/// Git's blob id of `text` (the digest used throughout the certificate).
pub(crate) fn digest(text: &str) -> String {
    git2::Oid::hash_object(git2::ObjectType::Blob, text.as_bytes())
        .map(|o| o.to_string())
        .unwrap_or_default()
}

/// FNV-1a 64, hex: a short fingerprint of a checker's configuration.
pub(crate) fn fingerprint(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0u8)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{h:016x}")
}

/// The Node binary to run helpers with (`SEM_CHECK_NODE`, else `node`).
pub(crate) fn node() -> String {
    std::env::var("SEM_CHECK_NODE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "node".into())
}

/// Write an embedded helper script once, under a name that changes with its
/// content, and return its path.
pub(crate) fn helper(store_dir: Option<&Path>, name: &str, body: &str) -> Result<PathBuf, String> {
    let dir = match store_dir {
        Some(d) => d.join("helpers"),
        None => std::env::temp_dir().join("sem-check-helpers"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let p = dir.join(format!("{name}-{}.cjs", fingerprint(&[body])));
    if !p.exists() {
        let tmp = dir.join(format!(".{name}-{}.tmp", std::process::id()));
        std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(p)
}

/// The first `bin` found in `root/node_modules/.bin` or any ancestor's.
pub(crate) fn node_bin(root: &Path, bin: &str) -> Option<PathBuf> {
    let mut d = Some(root);
    while let Some(dir) = d {
        let p = dir.join("node_modules/.bin").join(bin);
        if p.exists() {
            return Some(p);
        }
        d = dir.parent();
    }
    None
}

/// `name`'s package directory as Node would find it from `root`.
pub(crate) fn node_package(root: &Path, name: &str) -> Option<PathBuf> {
    let mut d = Some(root);
    while let Some(dir) = d {
        let p = dir.join("node_modules").join(name);
        if p.join("package.json").exists() {
            return Some(p);
        }
        d = dir.parent();
    }
    None
}

pub(crate) fn package_version(pkg_dir: &Path) -> Option<String> {
    let t = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&t).ok()?;
    v["version"].as_str().map(String::from)
}

/// Does `path` match any glob (`*` within a segment, `**` across segments)?
pub(crate) fn glob_any(globs: &[String], path: &str) -> bool {
    globs.iter().any(|g| glob(g, path))
}

pub(crate) fn glob(pat: &str, path: &str) -> bool {
    fn m(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p.starts_with(b"**") {
            let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
            if rest.is_empty() || m(rest, s) {
                return true;
            }
            return (0..s.len()).any(|i| s[i] == b'/' && m(rest, &s[i + 1..]));
        }
        match p[0] {
            b'*' => {
                let mut i = 0;
                loop {
                    if m(&p[1..], &s[i..]) {
                        return true;
                    }
                    if i >= s.len() || s[i] == b'/' {
                        return false;
                    }
                    i += 1;
                }
            }
            b'?' => !s.is_empty() && s[0] != b'/' && m(&p[1..], &s[1..]),
            c => !s.is_empty() && s[0] == c && m(&p[1..], &s[1..]),
        }
    }
    m(pat.as_bytes(), path.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::glob;

    #[test]
    fn globs() {
        assert!(glob("**/*.md", "a/b/c.md"));
        assert!(glob("**/*.md", "c.md"));
        assert!(glob(".github/**", ".github/workflows/x.yml"));
        assert!(glob("vitest.config.*", "vitest.config.ts"));
        assert!(!glob("vitest.config.*", "a/vitest.config.ts"));
        assert!(glob("**/vitest.config.*", "a/vitest.config.ts"));
        assert!(glob("docs/**", "docs/x/y.png"));
        assert!(!glob("docs/**", "src/docs.ts"));
        assert!(glob("LICENSE*", "LICENSE-MIT"));
        assert!(glob("**/.env*", "pkg/.env.local"));
    }
}
