//! Where a module specifier lands: a source file in the repo, a workspace
//! package (when the file itself can't be pinned), or something outside.

use std::collections::HashSet;

use serde_json::Value;

use super::workspace::{bare_package_name_len, Workspace};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// A source file in the repo (repo-relative path).
    File(String),
    /// A repo path that is not a parsed source file (json, css, generated, missing).
    Path(String),
    /// A workspace package whose entry file could not be pinned.
    Package(usize),
    /// Outside the repo: an npm package name or a runtime builtin (`node:fs`).
    External(String),
}

const EXTS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".d.ts", ".js", ".jsx", ".mjs", ".cjs"];
const CONDITIONS: &[&str] = &["bun", "import", "module", "types", "default", "node", "require", "browser"];

pub struct Resolver<'a> {
    pub ws: &'a Workspace,
    pub files: &'a HashSet<String>,
}

impl<'a> Resolver<'a> {
    pub fn resolve(&self, from_file: &str, spec: &str) -> Target {
        if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
            let dir = parent(from_file);
            return self.file_or_path(&normalize(&format!("{dir}/{spec}")));
        }
        if spec.starts_with('#') {
            if let Some(owner) = self.ws.owner_of(from_file) {
                let pkg = &self.ws.packages[owner];
                if let Some(t) = pkg.imports.as_ref().and_then(|m| map_lookup(m, spec)) {
                    return self.in_package(owner, &t);
                }
            }
            return Target::External(spec.to_string());
        }
        if let Some((idx, sub)) = self.ws.package_of_specifier(spec) {
            let pkg = &self.ws.packages[idx];
            let key = if sub.is_empty() { ".".to_string() } else { format!(".{sub}") };
            if let Some(exports) = &pkg.exports {
                return match exports_lookup(exports, &key) {
                    Some(t) => self.in_package(idx, &t),
                    None => Target::Package(idx),
                };
            }
            if key == "." {
                if let Some(main) = &pkg.main {
                    return self.in_package(idx, main);
                }
                return match self.first_existing(&pkg.dir, "index") {
                    Some(f) => Target::File(f),
                    None => Target::Package(idx),
                };
            }
            return match self.file_or_path(&normalize(&format!("{}/{}", pkg.dir, &key[2..]))) {
                Target::File(f) => Target::File(f),
                _ => Target::Package(idx),
            };
        }
        let name = bare_package_name_len(spec).map(|n| &spec[..n]).unwrap_or(spec);
        Target::External(name.to_string())
    }

    fn in_package(&self, idx: usize, target: &str) -> Target {
        let dir = &self.ws.packages[idx].dir;
        match self.file_or_path(&normalize(&format!("{dir}/{target}"))) {
            Target::Path(_) => Target::Package(idx),
            t => t,
        }
    }

    fn file_or_path(&self, base: &str) -> Target {
        if self.files.contains(base) {
            return Target::File(base.to_string());
        }
        // `./x.js` names the TypeScript source `./x.ts` under ESM + TS conventions
        for (js, tss) in [(".js", &[".ts", ".tsx", ".d.ts"][..]), (".jsx", &[".tsx"][..]), (".mjs", &[".mts"][..]), (".cjs", &[".cts"][..])] {
            if let Some(stem) = base.strip_suffix(js) {
                for ts in tss {
                    let c = format!("{stem}{ts}");
                    if self.files.contains(&c) {
                        return Target::File(c);
                    }
                }
            }
        }
        if let Some(f) = self.first_existing(parent_or_self(base), leaf(base)) {
            return Target::File(f);
        }
        if let Some(f) = self.first_existing(base, "index") {
            return Target::File(f);
        }
        Target::Path(base.to_string())
    }

    fn first_existing(&self, dir: &str, stem: &str) -> Option<String> {
        EXTS.iter()
            .map(|e| if dir.is_empty() { format!("{stem}{e}") } else { format!("{dir}/{stem}{e}") })
            .find(|c| self.files.contains(c))
    }
}

/// `exports` map lookup for a subpath key (`.`, `./x`), with `*` patterns and conditions.
fn exports_lookup(exports: &Value, key: &str) -> Option<String> {
    match exports {
        Value::String(s) => (key == ".").then(|| s.clone()),
        Value::Object(o) if o.keys().all(|k| !k.starts_with('.')) => (key == ".").then(|| pick(exports)).flatten(),
        _ => map_lookup(exports, key),
    }
}

/// Subpath/imports-map lookup: exact key first, then the longest matching `*` pattern.
fn map_lookup(map: &Value, key: &str) -> Option<String> {
    let o = map.as_object()?;
    if let Some(v) = o.get(key) {
        return pick(v);
    }
    let mut best: Option<(usize, String)> = None;
    for (k, v) in o {
        let Some(star) = k.find('*') else { continue };
        let (pre, post) = (&k[..star], &k[star + 1..]);
        if key.len() >= pre.len() + post.len() && key.starts_with(pre) && key.ends_with(post) {
            let mid = &key[pre.len()..key.len() - post.len()];
            if let Some(t) = pick(v) {
                if best.as_ref().is_none_or(|(l, _)| pre.len() > *l) {
                    best = Some((pre.len(), t.replace('*', mid)));
                }
            }
        }
    }
    if let Some((_, t)) = best {
        return Some(t);
    }
    // legacy trailing-slash folder mappings: "#src/": "./src/"
    o.iter()
        .filter(|(k, _)| k.ends_with('/') && key.starts_with(k.as_str()))
        .max_by_key(|(k, _)| k.len())
        .and_then(|(k, v)| pick(v).map(|t| format!("{t}{}", &key[k.len()..])))
}

fn pick(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(pick),
        Value::Object(o) => CONDITIONS.iter().find_map(|c| o.get(*c).and_then(pick)),
        _ => None,
    }
}

pub fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

fn parent(p: &str) -> &str {
    p.rfind('/').map(|i| &p[..i]).unwrap_or("")
}

fn parent_or_self(p: &str) -> &str {
    parent(p)
}

fn leaf(p: &str) -> &str {
    p.rfind('/').map(|i| &p[i + 1..]).unwrap_or(p)
}
