//! `compilerOptions.paths` from tsconfig files: how a monorepo maps its own
//! package names to source (`"@acme/core": ["./packages/core/src/index.ts"]`)
//! so imports resolve without building the packages first.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use super::resolve::normalize;

/// The path mappings in effect for files under `dir`.
#[derive(Debug, Clone, Default)]
pub struct TsPaths {
    /// Repo-relative directory of the tsconfig whose files these apply to.
    pub dir: String,
    /// Repo-relative directory the mapped paths are relative to (`baseUrl`,
    /// else the directory of the tsconfig that declares `paths`).
    pub base: String,
    /// `(pattern, targets)`; a pattern holds at most one `*`.
    /// Shared by every tsconfig that inherits the same table (a monorepo's
    /// packages all extend one base with thousands of aliases).
    pub paths: Arc<Vec<(String, Vec<String>)>>,
}

impl TsPaths {
    /// Repo-relative candidate paths for `spec`, in the order TypeScript tries
    /// them (exact pattern first, else the longest-prefix `*` pattern).
    pub fn candidates(&self, spec: &str) -> Vec<String> {
        let join = |t: &str| normalize(&if self.base.is_empty() { t.to_string() } else { format!("{}/{t}", self.base) });
        if let Some((_, ts)) = self.paths.iter().find(|(k, _)| k == spec) {
            return ts.iter().map(|t| join(t)).collect();
        }
        let mut best: Option<(usize, &str, &Vec<String>)> = None;
        for (k, ts) in self.paths.iter() {
            let Some(star) = k.find('*') else { continue };
            let (pre, post) = (&k[..star], &k[star + 1..]);
            if spec.len() >= pre.len() + post.len() && spec.starts_with(pre) && spec.ends_with(post) && best.is_none_or(|(l, _, _)| pre.len() > l) {
                best = Some((pre.len(), &spec[pre.len()..spec.len() - post.len()], ts));
            }
        }
        best.map(|(_, mid, ts)| ts.iter().map(|t| join(&t.replace('*', mid))).collect()).unwrap_or_default()
    }
}

/// tsconfig text is JSONC: drop comments, then trailing commas; strings stay intact.
pub fn strip_jsonc(text: &str) -> String {
    let no_comments = scan(text, true);
    scan(&no_comments, false)
}

/// One pass over `text` copying strings verbatim; drops comments when
/// `comments`, else drops commas that only precede `}` / `]`.
fn scan(text: &str, comments: bool) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let s = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                out.push_str(&text[s..i]);
            }
            b'/' if comments && b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if comments && b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b',' if !comments => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if !matches!(b.get(j), Some(b'}') | Some(b']')) {
                    out.push(',');
                }
                i += 1;
            }
            _ => {
                let s = i;
                i += 1;
                while i < b.len() && !matches!(b[i], b'"' | b'/' | b',') {
                    i += 1;
                }
                out.push_str(&text[s..i]);
            }
        }
    }
    out
}

fn read(root: &Path, rel: &str) -> Option<Value> {
    serde_json::from_str(&strip_jsonc(&fs::read_to_string(root.join(rel)).ok()?)).ok()
}

fn parent(p: &str) -> &str {
    p.rfind('/').map(|i| &p[..i]).unwrap_or("")
}

/// `paths` and `baseUrl` of the tsconfig at repo-relative `rel`, following
/// relative `extends` (a later config's own settings win).
type Effective = (Option<(String, Arc<Vec<(String, Vec<String>)>>)>, Option<String>);

fn effective(root: &Path, rel: &str, depth: usize, memo: &mut HashMap<String, Effective>) -> Effective {
    if let Some(e) = memo.get(rel) {
        return e.clone();
    }
    let e = effective_uncached(root, rel, depth, memo);
    memo.insert(rel.to_string(), e.clone());
    e
}

fn effective_uncached(root: &Path, rel: &str, depth: usize, memo: &mut HashMap<String, Effective>) -> Effective {
    let Some(v) = read(root, rel) else { return (None, None) };
    let dir = parent(rel);
    let (mut paths, mut base_url) = (None, None);
    if depth < 8 {
        let ext: Vec<String> = match &v["extends"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        };
        for e in ext.iter().filter(|e| e.starts_with('.')) {
            let mut target = normalize(&format!("{dir}/{e}"));
            if !target.ends_with(".json") {
                target.push_str(".json");
            }
            let (p, b) = effective(root, &target, depth + 1, memo);
            paths = p.or(paths);
            base_url = b.or(base_url);
        }
    }
    let co = &v["compilerOptions"];
    if let Some(b) = co["baseUrl"].as_str() {
        base_url = Some(normalize(&format!("{dir}/{b}")));
    }
    if let Some(o) = co["paths"].as_object() {
        let table = o
            .iter()
            .map(|(k, t)| (k.clone(), t.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()))
            .collect();
        paths = Some((dir.to_string(), Arc::new(table)));
    }
    (paths, base_url)
}

/// Path mappings of the tsconfig.json in each of `dirs` (repo-relative; `""`
/// is the root) that declares or inherits `paths`.
pub fn discover(root: &Path, dirs: &[&str]) -> Vec<TsPaths> {
    let mut out = Vec::new();
    let mut memo = HashMap::new();
    for dir in dirs {
        let rel = if dir.is_empty() { "tsconfig.json".to_string() } else { format!("{dir}/tsconfig.json") };
        if !root.join(&rel).is_file() {
            continue;
        }
        let (paths, base_url) = effective(root, &rel, 0, &mut memo);
        let Some((decl_dir, table)) = paths else { continue };
        out.push(TsPaths { dir: dir.to_string(), base: base_url.unwrap_or(decl_dir), paths: table });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{discover, strip_jsonc};

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let t = "{ // c\n \"a\": \"x//y\", /* b */ \"b\": [1, 2,],\n}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(v["a"], "x//y");
        assert_eq!(v["b"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn paths_with_extends_and_base_url() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("pkg/a")).unwrap();
        std::fs::write(d.path().join("tsconfig.base.json"), r#"{"compilerOptions":{"baseUrl":".","paths":{"@x/*":["pkg/*/src"],"@x/a":["pkg/a/src/index.ts"]}}}"#).unwrap();
        std::fs::write(d.path().join("tsconfig.json"), "{ \"extends\": \"./tsconfig.base\", // inherit\n}").unwrap();
        std::fs::write(d.path().join("pkg/a/tsconfig.json"), r#"{"compilerOptions":{"strict":true}}"#).unwrap();
        let all = discover(d.path(), &["", "pkg/a"]);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].candidates("@x/a"), vec!["pkg/a/src/index.ts"]);
        assert_eq!(all[0].candidates("@x/b"), vec!["pkg/b/src"]);
        assert!(all[0].candidates("react").is_empty());
    }
}
