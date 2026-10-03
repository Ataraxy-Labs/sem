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
    /// (`./dist/types/core/src/x.d.ts`, `./dist/esm/index.node.js`): the
    /// source file it was built from, re-rooted at its `src/` segment, or with
    /// the first `dist|build|lib|out` segment replaced by `src` (dropping
    /// format directories after it: `dist/esm/x.js` -> `src/x.ts`).
    /// Runtime conditions are tried before `types`, Node's first (`node`,
    /// then `import`, `module`, `default`, `require`), and an implementation
    /// beats a declaration file: calls resolve into function bodies.
    fn built_to_source(&self, idx: usize, exports: &Value, key: &str) -> Option<Target> {
        let dir = &self.ws.packages[idx].dir;
        let mut decl: Option<Target> = None;
        let targets = ["node", "import", "module", "default", "require", "types"].iter().filter_map(|c| exports_lookup_with(exports, key, c));
        for t in targets {
            let t = normalize(&t);
            let stem = [".d.ts", ".d.mts", ".d.cts", ".js", ".mjs", ".cjs"]
                .iter()
                .find_map(|x| t.strip_suffix(x))
                .unwrap_or(&t)
                .to_string();
            let segs: Vec<&str> = stem.split('/').collect();
            let mut rels: Vec<String> = Vec::new();
            if let Some(i) = segs.iter().position(|s| *s == "src") {
                rels.push(segs[i..].join("/"));
            } else if let Some(i) = segs.iter().position(|s| matches!(*s, "dist" | "build" | "lib" | "out")) {
                for skip in 0..segs.len().saturating_sub(i + 1) {
                    rels.push(segs[..i].iter().chain(["src"].iter()).chain(segs[i + 1 + skip..].iter()).copied().collect::<Vec<_>>().join("/"));
                }
            }
            for rel in rels {
                if let Target::File(f) = self.file_or_path(&normalize(&format!("{dir}/{rel}"))) {
                    if is_declaration(&f) {
                        decl.get_or_insert(Target::File(f));
                        continue;
                    }
                    return Some(Target::File(f));
                }
            }
            // a target that is itself a repo source (`types: ./src/index.d.ts`)
            if let Target::File(f) = self.file_or_path(&normalize(&format!("{dir}/{t}"))) {
                if !is_declaration(&f) {
                    return Some(Target::File(f));
                }
                decl.get_or_insert(Target::File(f));
            }
        }
        decl
    }

    fn in_package(&self, idx: usize, target: &str) -> Target {
        let dir = &self.ws.packages[idx].dir;
        match self.file_or_path(&normalize(&format!("{dir}/{target}"))) {
            Target::Path(_) => Target::Package(idx),
            t => t,
        }
    }

    /// Repo files a relative dynamic-import pattern (`./locales/*.json`,
    /// `*` within the last segment only) can load, resolved like any relative
    /// specifier. Patterns with a `*` in a directory segment expand to nothing.
    pub fn expand_pattern(&self, from_file: &str, pattern: &str) -> Vec<Target> {
        let full = normalize(&format!("{}/{pattern}", parent(from_file)));
        let (dir, leaf_pat) = (parent(&full), leaf(&full));
        if dir.contains('*') || leaf_pat.is_empty() {
            return Vec::new();
        }
        let Ok(rd) = std::fs::read_dir(self.ws.root.join(dir)) else { return Vec::new() };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| super::glob::matches(leaf_pat, n))
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|n| self.file_or_path(&if dir.is_empty() { n.clone() } else { format!("{dir}/{n}") }))
            .collect()
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

fn is_declaration(f: &str) -> bool {
    [".d.ts", ".d.mts", ".d.cts"].iter().any(|x| f.ends_with(x))
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
        let w = Workspace::discover(d.path(), &Discovery { exclude_dirs: &[], skip_segments: &[], extensions: &exts, root_package: false });
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
    fn unbuilt_exports_prefer_node_implementation_over_declarations() {
        let pkg = r#"{"name":"@x/conv","main":"./dist/esm/index.browser.js","exports":{".":{"browser":"./dist/esm/index.browser.js","node":"./dist/esm/index.node.js","import":"./dist/esm/index.browser.js","types":"./src/index.d.ts"}}}"#;
        let (_d, w) = ws(&[
            ("package.json", r#"{"workspaces":["packages/*","play"]}"#),
            ("packages/conv/package.json", pkg),
            ("packages/conv/src/index.d.ts", ""),
            ("packages/conv/src/index.node.ts", ""),
            ("packages/conv/src/index.browser.ts", ""),
            ("play/package.json", r#"{"name":"play"}"#),
            ("play/run.mjs", ""),
        ]);
        let files: HashSet<String> = ["packages/conv/src/index.d.ts", "packages/conv/src/index.node.ts", "packages/conv/src/index.browser.ts", "play/run.mjs"].iter().map(|s| s.to_string()).collect();
        let r = Resolver { ws: &w, files: &files };
        assert_eq!(r.resolve("play/run.mjs", "@x/conv"), Target::File("packages/conv/src/index.node.ts".into()));
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
