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
        // tsconfig `paths` come first, as in the TypeScript compiler: they are
        // how a workspace maps its own packages to source without a build.
        if let Some(tp) = self.ws.ts_paths_for(from_file) {
            for c in tp.candidates(spec) {
                if let Target::File(f) = self.file_or_path(&c) {
                    return Target::File(f);
                }
            }
        }
        if let Some((idx, sub)) = self.ws.package_of_specifier(spec) {
            let pkg = &self.ws.packages[idx];
            let key = if sub.is_empty() { ".".to_string() } else { format!(".{sub}") };
            if let Some(exports) = &pkg.exports {
                return match exports_lookup(exports, &key) {
                    Some(t) => match self.in_package(idx, &t) {
                        Target::Package(_) => self.built_to_source(idx, exports, &key).unwrap_or(Target::Package(idx)),
                        found => found,
                    },
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

    /// An `exports` entry that points at build output that is not there
    /// (`./dist/types/core/src/x.d.ts`, `./dist/index.js`): the source file it
    /// was built from, re-rooted at its `src/` segment or with the first
    /// `dist|build|lib|out` segment replaced by `src`.
    fn built_to_source(&self, idx: usize, exports: &Value, key: &str) -> Option<Target> {
        let dir = &self.ws.packages[idx].dir;
        let targets = ["types", "import", "default"].iter().filter_map(|c| exports_lookup_with(exports, key, c));
        for t in targets {
            let t = normalize(&t);
            let stem = [".d.ts", ".d.mts", ".d.cts", ".js", ".mjs", ".cjs"]
                .iter()
                .find_map(|x| t.strip_suffix(x))
                .unwrap_or(&t)
                .to_string();
            let segs: Vec<&str> = stem.split('/').collect();
            let src_rel = if let Some(i) = segs.iter().position(|s| *s == "src") {
                Some(segs[i..].join("/"))
            } else {
                segs.iter()
                    .position(|s| matches!(*s, "dist" | "build" | "lib" | "out"))
                    .map(|i| segs[..i].iter().chain(["src"].iter()).chain(segs[i + 1..].iter()).copied().collect::<Vec<_>>().join("/"))
            };
            if let Some(rel) = src_rel {
                if let Target::File(f) = self.file_or_path(&normalize(&format!("{dir}/{rel}"))) {
                    return Some(Target::File(f));
                }
            }
        }
        None
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

/// `exports_lookup` preferring condition `cond` (then the usual order).
fn exports_lookup_with(exports: &Value, key: &str, cond: &str) -> Option<String> {
    let prefer = |v: &Value| -> Option<String> {
        match v {
            Value::Object(o) => o.get(cond).and_then(pick).or_else(|| pick(v)),
            _ => pick(v),
        }
    };
    match exports {
        Value::String(s) => (key == ".").then(|| s.clone()),
        Value::Object(o) if o.keys().all(|k| !k.starts_with('.')) => (key == ".").then(|| prefer(exports)).flatten(),
        Value::Object(o) => {
            if let Some(v) = o.get(key) {
                return prefer(v);
            }
            let mut best: Option<(usize, String)> = None;
            for (k, v) in o {
                let Some(star) = k.find('*') else { continue };
                let (pre, post) = (&k[..star], &k[star + 1..]);
                if key.len() >= pre.len() + post.len() && key.starts_with(pre) && key.ends_with(post) {
                    let mid = &key[pre.len()..key.len() - post.len()];
                    if let Some(t) = prefer(v) {
                        if best.as_ref().is_none_or(|(l, _)| pre.len() > *l) {
                            best = Some((pre.len(), t.replace('*', mid)));
                        }
                    }
                }
            }
            best.map(|(_, t)| t)
        }
        _ => None,
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{Resolver, Target};
    use crate::topology::workspace::{Discovery, Workspace};

    fn ws(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        for (p, text) in files {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, text).unwrap();
        }
        let exts = vec![".ts".to_string()];
        let w = Workspace::discover(d.path(), &Discovery { exclude_dirs: &[], skip_segments: &[], extensions: &exts });
        (d, w)
    }

    #[test]
    fn unbuilt_workspace_packages_resolve_to_source() {
        let pkg = r#"{"name":"@x/core","exports":{".":{"types":"./dist/types/core/src/index.d.ts","default":"./dist/prod/index.js"},"./*":{"types":"./dist/types/core/src/*.d.ts","default":"./dist/prod/index.js"}}}"#;
        let (_d, w) = ws(&[
            ("package.json", r#"{"workspaces":["packages/*"]}"#),
            ("packages/core/package.json", pkg),
            ("packages/core/src/index.ts", ""),
            ("packages/core/src/deep/util.ts", ""),
            ("packages/app/package.json", r#"{"name":"@x/app"}"#),
            ("packages/app/src/main.ts", ""),
        ]);
        let files: HashSet<String> = ["packages/core/src/index.ts", "packages/core/src/deep/util.ts", "packages/app/src/main.ts"].iter().map(|s| s.to_string()).collect();
        let r = Resolver { ws: &w, files: &files };
        assert_eq!(r.resolve("packages/app/src/main.ts", "@x/core"), Target::File("packages/core/src/index.ts".into()));
        assert_eq!(r.resolve("packages/app/src/main.ts", "@x/core/deep/util"), Target::File("packages/core/src/deep/util.ts".into()));
    }

    #[test]
    fn tsconfig_paths_win_over_package_exports() {
        let (_d, w) = ws(&[
            ("package.json", r#"{"workspaces":["packages/*"]}"#),
            ("tsconfig.json", "{\"compilerOptions\":{\"paths\":{\"@x/core/*\":[\"./packages/core/lib/*\"]}}} // jsonc"),
            ("packages/core/package.json", r#"{"name":"@x/core","exports":{"./*":"./dist/*.js"}}"#),
            ("packages/core/lib/a.ts", ""),
            ("packages/app/package.json", r#"{"name":"@x/app"}"#),
            ("packages/app/src/main.ts", ""),
        ]);
        let files: HashSet<String> = ["packages/core/lib/a.ts", "packages/app/src/main.ts"].iter().map(|s| s.to_string()).collect();
        let r = Resolver { ws: &w, files: &files };
        assert_eq!(r.resolve("packages/app/src/main.ts", "@x/core/a"), Target::File("packages/core/lib/a.ts".into()));
    }
}
