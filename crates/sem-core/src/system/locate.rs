//! Find the installed source of each locked dependency.
//!
//! The ecosystem's own installer fetches (in a sandbox, scripts disabled);
//! this module only reads what it left behind and checks it against the
//! lockfile: a dependency is *located* when a directory holds exactly the
//! locked (name, version). Recognized layouts:
//!
//! - Python `site-packages` (`*.dist-info/METADATA` + `RECORD`),
//! - `node_modules/<name>/package.json`,
//! - the Go module cache (`<escaped path>@<version>/`),
//! - Cargo registry sources or `cargo vendor --versioned-dirs` (`<name>-<version>/`),
//! - standard libraries: CPython `Lib/`, `GOROOT/src`, Rust `library/`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::lockfiles::{normalize_pypi, Ecosystem, LockedDep};

/// What kind of tree a dependency root is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootKind {
    SitePackages,
    NodeModules,
    GoModCache,
    CargoSrc,
    PyStdlib,
    GoStdlib,
    RustStdlib,
}

impl RootKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "site-packages" | "py" => RootKind::SitePackages,
            "node-modules" | "npm" => RootKind::NodeModules,
            "gomodcache" | "go" => RootKind::GoModCache,
            "cargo" => RootKind::CargoSrc,
            "pystd" => RootKind::PyStdlib,
            "gostd" => RootKind::GoStdlib,
            "ruststd" => RootKind::RustStdlib,
            _ => return None,
        })
    }

    /// Link name under `.sem-system/links/`; Python roots keep the name
    /// `site-packages`, which marks a sys.path entry for the Python layout.
    pub fn link_name(&self) -> &'static str {
        match self {
            RootKind::SitePackages => "site-packages",
            RootKind::NodeModules => "node_modules",
            RootKind::GoModCache => "gomod",
            RootKind::CargoSrc => "cargo",
            RootKind::PyStdlib => "pystd",
            RootKind::GoStdlib => "gostd",
            RootKind::RustStdlib => "ruststd",
        }
    }

    pub fn is_stdlib(&self) -> bool {
        matches!(self, RootKind::PyStdlib | RootKind::GoStdlib | RootKind::RustStdlib)
    }
}

/// Where one locked dependency was found (or not).
#[derive(Clone, Debug, Serialize)]
pub struct Located {
    pub dep: LockedDep,
    /// Directory relative to its dependency root; `None` = not installed.
    pub dir: Option<String>,
    /// Source files (relative to the dependency root) the collector reads.
    pub files: Vec<String>,
}

/// A located standard library: all its source files.
#[derive(Clone, Debug, Serialize)]
pub struct Stdlib {
    pub kind: RootKind,
    pub files: Vec<String>,
}

const SKIP_DIRS: &[&str] = &[
    "tests", "test", "testdata", "testing", "benches", "examples", "example", "docs",
    "__pycache__", ".git", "node_modules",
];

fn walk(root: &Path, dir: &Path, ext: &[&str], skip_nested_go_mod: bool, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            if skip_nested_go_mod && path.join("go.mod").exists() {
                continue; // a different module
            }
            walk(root, &path, ext, skip_nested_go_mod, out);
        } else if ext.iter().any(|x| name.ends_with(x))
            && !name.ends_with("_test.go")
            && !name.ends_with(".d.ts")
        {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

/// Go module cache escaping: uppercase `X` becomes `!x`.
fn go_escape(path: &str) -> String {
    let mut s = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_uppercase() {
            s.push('!');
            s.push(c.to_ascii_lowercase());
        } else {
            s.push(c);
        }
    }
    s
}

/// Python: `site-packages` dist-info → (normalized name, version) → files.
fn index_site_packages(root: &Path) -> HashMap<(String, String), Vec<String>> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(root) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".dist-info") else { continue };
        let meta = std::fs::read_to_string(e.path().join("METADATA")).unwrap_or_default();
        let field = |k: &str| {
            meta.lines()
                .find_map(|l| l.strip_prefix(k))
                .map(|v| v.trim().to_string())
        };
        let (Some(n), Some(v)) = (field("Name:"), field("Version:")) else {
            let _ = stem;
            continue;
        };
        let record = std::fs::read_to_string(e.path().join("RECORD")).unwrap_or_default();
        let mut files: Vec<String> = record
            .lines()
            .filter_map(|l| l.split(',').next())
            .filter(|p| p.ends_with(".py") && !p.starts_with("..") && !p.contains(".dist-info/"))
            .filter(|p| {
                let first = p.split('/').next().unwrap_or("");
                !SKIP_DIRS.contains(&first)
                    && !p.split('/').any(|seg| seg == "tests" || seg == "test" || seg == "__pycache__")
            })
            .map(|s| s.to_string())
            .collect();
        files.sort();
        files.dedup();
        out.insert((normalize_pypi(&n), v), files);
    }
    out
}

/// Locate every locked dependency of `ecosystem` kinds served by `roots`.
pub fn locate(deps: &[LockedDep], roots: &[(RootKind, PathBuf)]) -> Vec<Located> {
    let mut site: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (k, r) in roots {
        if *k == RootKind::SitePackages {
            site.extend(index_site_packages(r));
        }
    }
    // Cargo: one version per crate name (the resolver binds crates by name);
    // keep the highest locked version when a lockfile holds several.
    let mut cargo_pick: BTreeMap<String, String> = BTreeMap::new();
    for d in deps.iter().filter(|d| d.ecosystem == Ecosystem::Cargo) {
        let e = cargo_pick.entry(d.name.clone()).or_insert_with(|| d.version.clone());
        if version_key(&d.version) > version_key(e) {
            *e = d.version.clone();
        }
    }
    deps.iter()
        .map(|dep| {
            let found: Option<(String, Vec<String>)> = match dep.ecosystem {
                Ecosystem::Pypi => site
                    .get(&(normalize_pypi(&dep.name), dep.version.clone()))
                    .map(|f| (String::new(), f.clone())),
                Ecosystem::Npm => roots
                    .iter()
                    .filter(|(k, _)| *k == RootKind::NodeModules)
                    .find_map(|(_, r)| {
                        let pj = r.join(&dep.name).join("package.json");
                        let v: serde_json::Value =
                            serde_json::from_str(&std::fs::read_to_string(pj).ok()?).ok()?;
                        (v.get("version")?.as_str()? == dep.version).then(|| (dep.name.clone(), Vec::new()))
                    }),
                Ecosystem::Go => roots
                    .iter()
                    .filter(|(k, _)| *k == RootKind::GoModCache)
                    .find_map(|(_, r)| {
                        let rel = format!("{}@{}", go_escape(&dep.name), dep.version);
                        let dir = r.join(&rel);
                        dir.is_dir().then(|| {
                            let mut files = Vec::new();
                            walk(r, &dir, &[".go"], true, &mut files);
                            (rel, files)
                        })
                    }),
                Ecosystem::Cargo => {
                    if cargo_pick.get(&dep.name) != Some(&dep.version) {
                        None
                    } else {
                        roots
                            .iter()
                            .filter(|(k, _)| *k == RootKind::CargoSrc)
                            .find_map(|(_, r)| {
                                let rel = format!("{}-{}", dep.name, dep.version);
                                let dir = r.join(&rel);
                                dir.join("Cargo.toml").exists().then(|| {
                                    let mut files = Vec::new();
                                    walk(r, &dir, &[".rs"], false, &mut files);
                                    (rel, files)
                                })
                            })
                    }
                }
            };
            match found {
                Some((dir, files)) => Located { dep: dep.clone(), dir: Some(dir), files },
                None => Located { dep: dep.clone(), dir: None, files: Vec::new() },
            }
        })
        .collect()
}

fn version_key(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap_or(0))
        .collect()
}

/// Source files of a standard library root.
pub fn stdlib_files(kind: RootKind, root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    match kind {
        RootKind::PyStdlib => {
            const SKIP: &[&str] = &[
                "test", "idlelib", "tkinter", "turtledemo", "lib2to3", "ensurepip", "site-packages",
                "dist-packages", "pydoc_data", "config-3", "__phello__",
            ];
            let Ok(rd) = std::fs::read_dir(root) else { return files };
            let mut es: Vec<_> = rd.flatten().collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let n = e.file_name().to_string_lossy().to_string();
                if SKIP.iter().any(|s| n.starts_with(s)) {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    walk(root, &p, &[".py"], false, &mut files);
                } else if n.ends_with(".py") {
                    files.push(n);
                }
            }
        }
        RootKind::GoStdlib => {
            let Ok(rd) = std::fs::read_dir(root) else { return files };
            let mut es: Vec<_> = rd.flatten().collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let n = e.file_name().to_string_lossy().to_string();
                if n == "cmd" || n == "testdata" {
                    continue;
                }
                if e.path().is_dir() {
                    walk(root, &e.path(), &[".go"], false, &mut files);
                }
            }
        }
        RootKind::RustStdlib => {
            for krate in ["core", "alloc", "std"] {
                walk(root, &root.join(krate).join("src"), &[".rs"], false, &mut files);
            }
        }
        _ => {}
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_escaping_and_versions() {
        assert_eq!(go_escape("github.com/BurntSushi/toml"), "github.com/!burnt!sushi/toml");
        assert!(version_key("1.10.0") > version_key("1.9.3"));
    }

    #[test]
    fn locates_site_packages_and_cargo() {
        let t = tempfile_dir("locate");
        let sp = t.join("site-packages");
        std::fs::create_dir_all(sp.join("Req_Lib-2.0.dist-info")).unwrap();
        std::fs::create_dir_all(sp.join("req_lib")).unwrap();
        std::fs::write(sp.join("Req_Lib-2.0.dist-info/METADATA"), "Name: Req_Lib\nVersion: 2.0\n").unwrap();
        std::fs::write(
            sp.join("Req_Lib-2.0.dist-info/RECORD"),
            "req_lib/__init__.py,sha256=x,1\nreq_lib/tests/t.py,,\n../../bin/x,,\n",
        )
        .unwrap();
        let cs = t.join("cargo");
        std::fs::create_dir_all(cs.join("foo-1.2.0/src")).unwrap();
        std::fs::write(cs.join("foo-1.2.0/Cargo.toml"), "[package]\nname=\"foo\"\n").unwrap();
        std::fs::write(cs.join("foo-1.2.0/src/lib.rs"), "pub fn f(){}\n").unwrap();
        let dep = |e: Ecosystem, n: &str, v: &str| LockedDep {
            ecosystem: e,
            name: n.into(),
            version: v.into(),
            lockfile: "x".into(),
        };
        let deps = vec![
            dep(Ecosystem::Pypi, "req-lib", "2.0"),
            dep(Ecosystem::Cargo, "foo", "1.2.0"),
            dep(Ecosystem::Cargo, "foo", "1.1.0"),
            dep(Ecosystem::Pypi, "missing", "1"),
        ];
        let got = locate(
            &deps,
            &[(RootKind::SitePackages, sp.clone()), (RootKind::CargoSrc, cs.clone())],
        );
        assert_eq!(got[0].files, ["req_lib/__init__.py"]);
        assert_eq!(got[1].files, ["foo-1.2.0/src/lib.rs"]);
        assert!(got[2].dir.is_none(), "only one version per crate name");
        assert!(got[3].dir.is_none());
        let _ = std::fs::remove_dir_all(&t);
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sem-system-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }
}
