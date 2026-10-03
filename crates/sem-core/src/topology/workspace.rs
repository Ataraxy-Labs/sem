//! JS/TS workspace discovery: the packages a monorepo declares, their
//! manifests, and the source files each one owns.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::glob;

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    /// Repo-relative directory, `/`-separated, no trailing slash.
    pub dir: String,
    pub exports: Option<Value>,
    pub imports: Option<Value>,
    pub main: Option<String>,
    pub deps: BTreeSet<String>,
    pub dev_deps: BTreeSet<String>,
    pub peer_deps: BTreeSet<String>,
    pub optional_deps: BTreeSet<String>,
    /// No package.json: name inferred from `<scope-dir>/<name-dir>`.
    pub inferred: bool,
}

#[derive(Debug, Clone)]
pub struct Discovery<'a> {
    /// Globs over repo-relative workspace directories to leave out.
    pub exclude_dirs: &'a [String],
    /// Directory names never descended into (build output, vendored deps).
    pub skip_segments: &'a [String],
    /// Source extensions to scan.
    pub extensions: &'a [String],
}

#[derive(Debug, Default)]
pub struct Workspace {
    pub root: PathBuf,
    pub packages: Vec<Package>,
    /// Package name -> index into `packages`.
    pub by_name: BTreeMap<String, usize>,
    /// tsconfig `paths` of the root and of each package directory.
    pub ts_paths: Vec<super::tsconfig::TsPaths>,
}

impl Workspace {
    pub fn discover(root: &Path, opt: &Discovery) -> Workspace {
        let root = root.to_path_buf();
        let mut dirs = Vec::new();
        for pat in workspace_patterns(&root) {
            expand(&root, "", &pat.split('/').collect::<Vec<_>>(), &mut dirs);
        }
        dirs.sort();
        dirs.dedup();
        let mut ws = Workspace { root: root.clone(), ..Default::default() };
        for dir in dirs {
            if opt.exclude_dirs.iter().any(|g| glob::matches(g, &dir))
                || dir.split('/').any(|s| opt.skip_segments.iter().any(|k| k == s))
            {
                continue;
            }
            let pkg = read_package(&root, &dir).or_else(|| infer_package(&root, &dir, opt.extensions));
            let Some(pkg) = pkg else { continue };
            if ws.by_name.contains_key(&pkg.name) {
                continue;
            }
            ws.by_name.insert(pkg.name.clone(), ws.packages.len());
            ws.packages.push(pkg);
        }
        let dirs: Vec<&str> = std::iter::once("").chain(ws.packages.iter().map(|p| p.dir.as_str())).collect();
        ws.ts_paths = super::tsconfig::discover(&ws.root, &dirs);
        ws
    }

    /// The tsconfig `paths` in effect for a repo-relative file (deepest tsconfig wins).
    pub fn ts_paths_for(&self, rel_path: &str) -> Option<&super::tsconfig::TsPaths> {
        self.ts_paths
            .iter()
            .filter(|t| t.dir.is_empty() || rel_path.starts_with(&format!("{}/", t.dir)))
            .max_by_key(|t| t.dir.len())
    }

    /// Every source file under a package directory (not following symlinks,
    /// skipping dot-entries and `skip_segments`), repo-relative and sorted.
    pub fn source_files(&self, opt: &Discovery) -> Vec<String> {
        let mut out = Vec::new();
        for p in &self.packages {
            walk(&self.root, &p.dir, opt, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    /// The workspace package whose directory contains `rel_path` (deepest wins).
    pub fn owner_of(&self, rel_path: &str) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (i, p) in self.packages.iter().enumerate() {
            let inside = rel_path == p.dir || (rel_path.len() > p.dir.len() && rel_path.starts_with(&p.dir) && rel_path.as_bytes()[p.dir.len()] == b'/');
            if inside && best.is_none_or(|(_, len)| p.dir.len() > len) {
                best = Some((i, p.dir.len()));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Split a bare specifier into (workspace package, subpath) if it names one.
    pub fn package_of_specifier<'a>(&self, spec: &'a str) -> Option<(usize, &'a str)> {
        let name_len = bare_package_name_len(spec)?;
        let idx = *self.by_name.get(&spec[..name_len])?;
        Some((idx, &spec[name_len..]))
    }
}

/// Length of the package-name prefix of a bare specifier (`@a/b/c` -> 4 of `@a/b`).
pub fn bare_package_name_len(spec: &str) -> Option<usize> {
    if spec.is_empty() || spec.starts_with('.') || spec.starts_with('/') || spec.starts_with('#') {
        return None;
    }
    let mut parts = spec.splitn(3, '/');
    let first = parts.next()?;
    if first.starts_with('@') {
        match parts.next() {
            Some(second) => Some(first.len() + 1 + second.len()),
            None => Some(first.len()),
        }
    } else {
        Some(first.len())
    }
}

fn walk(root: &Path, dir: &str, opt: &Discovery, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(root.join(dir)) else { return };
    for e in entries.flatten() {
        let Ok(name) = e.file_name().into_string() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let rel = format!("{dir}/{name}");
        if ft.is_dir() {
            if !opt.skip_segments.iter().any(|s| s == &name) {
                walk(root, &rel, opt, out);
            }
        } else if opt.extensions.iter().any(|x| name.ends_with(x.as_str())) {
            out.push(rel);
        }
    }
}

fn workspace_patterns(root: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(root.join("package.json")) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    let arr = match v.get("workspaces") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Object(o)) => o.get("packages").and_then(|p| p.as_array().cloned()).unwrap_or_default(),
        _ => Vec::new(),
    };
    arr.iter().filter_map(|s| s.as_str().map(|s| s.trim_end_matches('/').to_string())).collect()
}

fn expand(root: &Path, prefix: &str, segs: &[&str], out: &mut Vec<String>) {
    let Some((seg, rest)) = segs.split_first() else {
        if !prefix.is_empty() {
            out.push(prefix.to_string());
        }
        return;
    };
    if !seg.contains('*') {
        let next = join(prefix, seg);
        if root.join(&next).is_dir() {
            expand(root, &next, rest, out);
        }
        return;
    }
    let base = if prefix.is_empty() { root.to_path_buf() } else { root.join(prefix) };
    let Ok(entries) = fs::read_dir(&base) else { return };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    for n in names {
        if glob::matches(seg, &n) {
            expand(root, &join(prefix, &n), rest, out);
        }
    }
}

fn join(prefix: &str, seg: &str) -> String {
    if prefix.is_empty() { seg.to_string() } else { format!("{prefix}/{seg}") }
}

fn keys(v: &Value, field: &str) -> BTreeSet<String> {
    v.get(field).and_then(|d| d.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default()
}

fn read_package(root: &Path, dir: &str) -> Option<Package> {
    let text = fs::read_to_string(root.join(dir).join("package.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    Some(Package {
        name,
        dir: dir.to_string(),
        exports: v.get("exports").cloned(),
        imports: v.get("imports").cloned(),
        main: v.get("module").or_else(|| v.get("main")).or_else(|| v.get("types")).and_then(|m| m.as_str()).map(str::to_string),
        deps: keys(&v, "dependencies"),
        dev_deps: keys(&v, "devDependencies"),
        peer_deps: keys(&v, "peerDependencies"),
        optional_deps: keys(&v, "optionalDependencies"),
        inferred: false,
    })
}

/// A matched workspace directory without a manifest still counts when its
/// `src/` holds TypeScript: it is named after its last two path segments.
fn infer_package(root: &Path, dir: &str, _exts: &[String]) -> Option<Package> {
    let src = root.join(dir).join("src");
    let has_ts = fs::read_dir(&src).ok()?.flatten().any(|e| {
        let n = e.file_name().to_string_lossy().to_string();
        [".ts", ".tsx", ".mts", ".cts"].iter().any(|x| n.ends_with(x))
    });
    if !has_ts {
        return None;
    }
    let mut segs: Vec<&str> = dir.rsplitn(3, '/').collect();
    segs.truncate(2);
    segs.reverse();
    let name = if segs.len() == 2 && segs[0].starts_with('@') { format!("{}/{}", segs[0], segs[1]) } else { segs.last()?.to_string() };
    Some(Package {
        name,
        dir: dir.to_string(),
        exports: None,
        imports: None,
        main: None,
        deps: BTreeSet::new(),
        dev_deps: BTreeSet::new(),
        peer_deps: BTreeSet::new(),
        optional_deps: BTreeSet::new(),
        inferred: true,
    })
}
