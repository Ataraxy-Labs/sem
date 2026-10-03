//! Exact dependency versions from lockfiles.
//!
//! One reader per lockfile format; each returns `(ecosystem, name, version)`
//! rows and never guesses a version a lockfile does not state. Formats:
//! npm `package-lock.json` (v1-v3), `pnpm-lock.yaml`, `yarn.lock` (v1 and
//! berry), `bun.lock`, `uv.lock`, `poetry.lock`, `requirements*.txt` (only
//! `==` pins), `go.sum` (+ `go.mod` for directness), `Cargo.lock`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    Npm,
    Pypi,
    Go,
    Cargo,
}

impl Ecosystem {
    pub fn as_str(&self) -> &'static str {
        match self {
            Ecosystem::Npm => "npm",
            Ecosystem::Pypi => "pypi",
            Ecosystem::Go => "go",
            Ecosystem::Cargo => "cargo",
        }
    }
}

/// One locked dependency.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct LockedDep {
    pub ecosystem: Ecosystem,
    pub name: String,
    pub version: String,
    /// Lockfile it came from, relative to the repo root.
    pub lockfile: String,
}

/// Lockfile names this module reads.
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "uv.lock",
    "poetry.lock",
    "go.sum",
    "Cargo.lock",
];

fn is_lockfile(name: &str) -> bool {
    LOCKFILES.contains(&name)
        || (name.starts_with("requirements") && name.ends_with(".txt"))
}

/// Every lockfile under `root`, skipping dependency and build directories.
pub fn find_lockfiles(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if matches!(
                    name.as_str(),
                    "node_modules" | "target" | "vendor" | ".git" | ".sem-system" | ".venv"
                        | "venv" | "dist" | "build"
                ) {
                    continue;
                }
                stack.push(e.path());
            } else if is_lockfile(&name) {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

/// Parse one lockfile. Unknown names yield nothing.
pub fn parse_lockfile(root: &Path, path: &Path) -> Vec<LockedDep> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let rows: Vec<(Ecosystem, String, String)> = match name.as_str() {
        "package-lock.json" => npm_package_lock(&text),
        "pnpm-lock.yaml" => pnpm_lock(&text),
        "yarn.lock" => yarn_lock(&text),
        "bun.lock" => bun_lock(&text),
        "uv.lock" | "poetry.lock" => toml_packages(&text, Ecosystem::Pypi, true),
        "Cargo.lock" => toml_packages(&text, Ecosystem::Cargo, false),
        "go.sum" => go_sum(&text),
        n if n.starts_with("requirements") => requirements(&text),
        _ => Vec::new(),
    };
    let mut seen = BTreeSet::new();
    rows.into_iter()
        .filter(|r| seen.insert(r.clone()))
        .map(|(ecosystem, name, version)| LockedDep {
            ecosystem,
            name,
            version,
            lockfile: rel.clone(),
        })
        .collect()
}

/// All locked dependencies of a repo, deduplicated per (ecosystem, name, version).
pub fn locked_deps(root: &Path) -> Vec<LockedDep> {
    let mut all: BTreeMap<(Ecosystem, String, String), LockedDep> = BTreeMap::new();
    for lf in find_lockfiles(root) {
        for d in parse_lockfile(root, &lf) {
            all.entry((d.ecosystem.clone(), d.name.clone(), d.version.clone()))
                .or_insert(d);
        }
    }
    all.into_values().collect()
}

fn npm_package_lock(text: &str) -> Vec<(Ecosystem, String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(pkgs) = v.get("packages").and_then(|p| p.as_object()) {
        for (key, meta) in pkgs {
            let Some(idx) = key.rfind("node_modules/") else {
                continue; // the root package, or a workspace member
            };
            let name = &key[idx + "node_modules/".len()..];
            if meta.get("link").and_then(|l| l.as_bool()) == Some(true) {
                continue;
            }
            if let Some(ver) = meta.get("version").and_then(|v| v.as_str()) {
                out.push((Ecosystem::Npm, name.to_string(), ver.to_string()));
            }
        }
    } else if let Some(deps) = v.get("dependencies").and_then(|d| d.as_object()) {
        fn walk(
            deps: &serde_json::Map<String, serde_json::Value>,
            out: &mut Vec<(Ecosystem, String, String)>,
        ) {
            for (name, meta) in deps {
                if let Some(ver) = meta.get("version").and_then(|v| v.as_str()) {
                    out.push((Ecosystem::Npm, name.clone(), ver.to_string()));
                }
                if let Some(sub) = meta.get("dependencies").and_then(|d| d.as_object()) {
                    walk(sub, out);
                }
            }
        }
        walk(deps, &mut out);
    }
    out
}

/// `name@version` with an optional leading `@scope/`; strips pnpm peer
/// suffixes like `(react@18.0.0)` and berry protocols like `npm:`.
fn split_name_version(spec: &str) -> Option<(String, String)> {
    let spec = spec.trim().trim_start_matches('/');
    let spec = spec.split('(').next().unwrap_or(spec);
    let at = if let Some(rest) = spec.strip_prefix('@') {
        rest.find('@').map(|i| i + 1)?
    } else {
        spec.find('@')?
    };
    let name = &spec[..at];
    let ver = spec[at + 1..].trim_start_matches("npm:");
    if name.is_empty() || ver.is_empty() {
        return None;
    }
    Some((name.to_string(), ver.to_string()))
}

fn pnpm_lock(text: &str) -> Vec<(Ecosystem, String, String)> {
    let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for section in ["packages", "snapshots"] {
        if let Some(m) = v.get(section).and_then(|p| p.as_mapping()) {
            for k in m.keys() {
                if let Some((n, ver)) = k.as_str().and_then(split_name_version) {
                    // pnpm v5 used `/name/1.2.3`
                    out.push((Ecosystem::Npm, n, ver));
                }
            }
        }
    }
    out
}

fn yarn_lock(text: &str) -> Vec<(Ecosystem, String, String)> {
    let mut out = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') && line.ends_with(':') && !line.starts_with('#') {
            names = line
                .trim_end_matches(':')
                .split(", ")
                .filter_map(|s| {
                    let s = s.trim().trim_matches('"');
                    split_name_version(s).map(|(n, _)| n)
                })
                .collect();
            names.dedup();
        } else if let Some(v) = line
            .trim()
            .strip_prefix("version ")
            .or_else(|| line.trim().strip_prefix("version: "))
        {
            let v = v.trim().trim_matches('"').to_string();
            for n in names.drain(..) {
                out.push((Ecosystem::Npm, n, v.clone()));
            }
        }
    }
    out
}

fn bun_lock(text: &str) -> Vec<(Ecosystem, String, String)> {
    // JSONC with trailing commas: read the `"packages"` object entries
    // `"key": ["name@version", ...]` by pattern instead of a JSON parser.
    let re = regex::Regex::new(r#""[^"]+":\s*\[\s*"((?:@[^@"/]+/)?[^@"]+)@([^"]+)""#).unwrap();
    let start = text.find("\"packages\"").unwrap_or(0);
    re.captures_iter(&text[start..])
        .filter(|c| !c[2].starts_with("workspace:") && !c[2].starts_with("file:"))
        .map(|c| (Ecosystem::Npm, c[1].to_string(), c[2].to_string()))
        .collect()
}

fn toml_packages(text: &str, eco: Ecosystem, skip_local: bool) -> Vec<(Ecosystem, String, String)> {
    let Ok(v) = text.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(pkgs) = v.get("package").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    pkgs.iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            let version = p.get("version")?.as_str()?;
            let source = p.get("source");
            // Cargo: no `source` = workspace member. uv: editable/virtual = local.
            let local = match (&eco, source) {
                (Ecosystem::Cargo, None) => true,
                (Ecosystem::Cargo, Some(s)) => s.as_str().is_some_and(|s| s.starts_with("path+")),
                (_, Some(s)) if skip_local => {
                    s.get("editable").is_some() || s.get("virtual").is_some() || s.get("directory").is_some()
                }
                _ => false,
            };
            (!local).then(|| (eco.clone(), name.to_string(), version.to_string()))
        })
        .collect()
}

fn go_sum(text: &str) -> Vec<(Ecosystem, String, String)> {
    // A `mod ver/go.mod` line only pins the module's go.mod; a `mod ver` line
    // pins its source. Only the latter's source is part of the build.
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let m = it.next()?;
            let v = it.next()?;
            (!v.ends_with("/go.mod")).then(|| (Ecosystem::Go, m.to_string(), v.to_string()))
        })
        .collect()
}

fn requirements(text: &str) -> Vec<(Ecosystem, String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.split('#').next()?.split(';').next()?.trim();
            let (n, v) = l.split_once("==")?;
            let n = n.split('[').next()?.trim();
            let v = v.split_whitespace().next()?.trim_end_matches('\\').trim();
            (!n.is_empty() && !v.is_empty()).then(|| (Ecosystem::Pypi, n.to_string(), v.to_string()))
        })
        .collect()
}

/// PEP 503 name normalization.
pub fn normalize_pypi(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !last_sep {
                out.push('-');
            }
            last_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_v3_and_scoped_names() {
        let t = r#"{"packages":{"":{"name":"app"},"node_modules/a":{"version":"1.0.0"},
            "node_modules/@s/b":{"version":"2.1.0"},"node_modules/a/node_modules/c":{"version":"0.1.0"},
            "node_modules/w":{"resolved":"packages/w","link":true}}}"#;
        let mut r = npm_package_lock(t);
        r.sort();
        let names: Vec<_> = r.iter().map(|(_, n, v)| format!("{n}@{v}")).collect();
        assert_eq!(names, ["@s/b@2.1.0", "a@1.0.0", "c@0.1.0"]);
    }

    #[test]
    fn pnpm_and_yarn_and_bun() {
        let p = "lockfileVersion: '9.0'\npackages:\n  a@1.2.3:\n    resolution: {}\n  '@s/b@4.0.0(c@1.0.0)':\n    resolution: {}\n";
        let mut r: Vec<_> = pnpm_lock(p).into_iter().map(|(_, n, v)| format!("{n}@{v}")).collect();
        r.sort();
        assert_eq!(r, ["@s/b@4.0.0", "a@1.2.3"]);
        let y = "\"a@^1.0.0\", a@~1.0:\n  version \"1.0.5\"\n\n\"@s/b@npm:^2\":\n  version: 2.0.1\n";
        let mut r: Vec<_> = yarn_lock(y).into_iter().map(|(_, n, v)| format!("{n}@{v}")).collect();
        r.sort();
        assert_eq!(r, ["@s/b@2.0.1", "a@1.0.5"]);
        let b = "{\"workspaces\":{},\"packages\":{\"a\":[\"a@1.0.0\",\"\",{},\"sha\"],\"@s/b\":[\"@s/b@2.0.0\",\"\",{},\"x\"],}}";
        let mut r: Vec<_> = bun_lock(b).into_iter().map(|(_, n, v)| format!("{n}@{v}")).collect();
        r.sort();
        assert_eq!(r, ["@s/b@2.0.0", "a@1.0.0"]);
    }

    #[test]
    fn toml_go_requirements() {
        let c = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.1\"\nsource = \"registry+https://x\"\n";
        let r = toml_packages(c, Ecosystem::Cargo, false);
        assert_eq!(r, [(Ecosystem::Cargo, "serde".into(), "1.0.1".into())]);
        let u = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = { editable = \".\" }\n\n[[package]]\nname = \"idna\"\nversion = \"3.7\"\nsource = { registry = \"https://x\" }\n";
        assert_eq!(toml_packages(u, Ecosystem::Pypi, true).len(), 1);
        let g = "github.com/a/b v1.2.0 h1:x=\ngithub.com/a/b v1.2.0/go.mod h1:y=\ngithub.com/c/d v0.1.0/go.mod h1:z=\n";
        assert_eq!(go_sum(g), [(Ecosystem::Go, "github.com/a/b".into(), "v1.2.0".into())]);
        let q = "flask==3.0.0 ; python_version>'3'\n# c\nrequests[socks]==2.31.0\nx>=1\n";
        assert_eq!(requirements(q).len(), 2);
        assert_eq!(normalize_pypi("Flask_SQLAlchemy"), "flask-sqlalchemy");
    }
}
