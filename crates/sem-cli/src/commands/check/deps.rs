//! File dependencies for languages whose checkers scope by file: which files
//! can see a change, as a reverse closure over the names their imports (or
//! includes) mention.
//!
//! Matching is by name, not by resolution: a file depends on a changed file
//! when any name its imports mention is one of that file's names. That over-
//! approximates every real resolution (search paths, packages, re-exports
//! through `__init__` files are all covered by the closure), so a file outside
//! the closure provably cannot import what changed. A file whose imports
//! cannot be read statically mentions `*` and is always in the closure.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

pub(crate) const ANY: &str = "*";

pub(crate) struct Lang {
    /// The names a file's imports mention.
    pub mentions: fn(path: &str, text: &str) -> BTreeSet<String>,
    /// The names others use to import this file.
    pub names: fn(path: &str) -> Vec<String>,
}

/// Every file in `files` that can see a change to one of `changed` (git status
/// letter and path), including the changed files that still exist.
pub(crate) fn closure(root: &Path, files: &[String], changed: &[(char, String)], lang: &Lang) -> BTreeSet<String> {
    closure_with_cutoff(root, files, changed, &[], lang)
}

/// [`closure`], plus `quiet`: changed files whose interface did not change,
/// which are in the result themselves but reach no other file.
pub(crate) fn closure_with_cutoff(root: &Path, files: &[String], changed: &[(char, String)], quiet: &[String], lang: &Lang) -> BTreeSet<String> {
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    let mut out: BTreeSet<usize> = BTreeSet::new();
    let pos: HashMap<&str, usize> = files.iter().enumerate().map(|(i, f)| (f.as_str(), i)).collect();
    for (i, f) in files.iter().enumerate() {
        let Ok(bytes) = std::fs::read(root.join(f)) else {
            out.insert(i);
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let m = (lang.mentions)(f, &text);
        if m.contains(ANY) {
            out.insert(i);
        }
        for n in m {
            index.entry(n).or_default().push(i);
        }
    }
    let mut queue: Vec<String> = Vec::new();
    for (status, c) in changed {
        if let Some(&i) = pos.get(c.as_str()) {
            out.insert(i);
        }
        queue.extend((lang.names)(c));
        // an added or removed file can create or remove a package directory,
        // which changes what that package's name resolves to
        if *status != 'M' {
            queue.extend(dirs(c));
        }
    }
    for q in quiet {
        if let Some(&i) = pos.get(q.as_str()) {
            out.insert(i);
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    while let Some(n) = queue.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        for &i in index.get(&n).into_iter().flatten() {
            if out.insert(i) {
                queue.extend((lang.names)(&files[i]));
            }
        }
    }
    out.into_iter().map(|i| files[i].clone()).collect()
}

fn dirs(path: &str) -> impl Iterator<Item = String> + '_ {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts.into_iter().map(String::from)
}

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

fn stem(path: &str) -> &str {
    let leaf = path.rsplit('/').next().unwrap_or(path);
    leaf.split('.').next().unwrap_or(leaf)
}

/// Python: a module is imported by its stem; a package (`__init__`) by its
/// directory's name.
pub(crate) fn python_names(path: &str) -> Vec<String> {
    let s = stem(path);
    if s == "__init__" || s == "__main__" {
        dirs(path).last().into_iter().collect()
    } else {
        vec![s.to_string()]
    }
}

/// Python, static imports: the names in every `import` and `from ... import`
/// statement anywhere in the file (function bodies and `TYPE_CHECKING` blocks
/// included). A relative import also mentions the file's own packages.
pub(crate) fn python_imports(path: &str, text: &str) -> BTreeSet<String> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*(?:from[ \t]+([.\w]+)[ \t]+import[ \t]+(\([^)]*\)|[^\n#;]*)|import[ \t]+([^\n#;]*))").unwrap()
    });
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut out = BTreeSet::new();
    let mut relative = false;
    for c in RE.captures_iter(&joined) {
        if let Some(m) = c.get(1) {
            relative |= m.as_str().starts_with('.');
        }
        for g in [c.get(1), c.get(2), c.get(3)].into_iter().flatten() {
            out.extend(words(g.as_str()).filter(|w| *w != "as").map(String::from));
        }
    }
    if relative {
        out.extend(dirs(path));
    }
    out
}

/// Python, at run time: any name the file mentions at all (dynamic imports,
/// `importlib`, strings handed to subprocesses), plus its own packages, whose
/// `__init__` files load before it does.
pub(crate) fn python_runtime(path: &str, text: &str) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = words(text).map(String::from).collect();
    out.extend(dirs(path));
    out
}

pub(crate) const PYTHON: Lang = Lang { mentions: python_imports, names: python_names };
pub(crate) const PYTHON_RUNTIME: Lang = Lang { mentions: python_runtime, names: python_names };

/// C and C++: a file is included by its file name.
pub(crate) fn c_names(path: &str) -> Vec<String> {
    vec![path.rsplit('/').next().unwrap_or(path).to_string()]
}

/// C and C++: the file name of every `#include`, `#import` and `__has_include`
/// operand, and of every `#embed`. An include through a macro mentions `*`.
pub(crate) fn c_includes(_path: &str, text: &str) -> BTreeSet<String> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?m)^[ \t]*#[ \t]*(?:include|include_next|import|embed)[ \t]*([<"][^>"\n]*[>"]|[^\s/]+)|__has_include(?:_next)?\s*\(\s*([<"][^>"\n]*[>"])"#).unwrap()
    });
    let mut out = BTreeSet::new();
    for c in RE.captures_iter(text) {
        let Some(m) = c.get(1).or_else(|| c.get(2)) else { continue };
        let s = m.as_str();
        if !(s.starts_with('<') || s.starts_with('"')) {
            out.insert(ANY.to_string());
            continue;
        }
        let inner = &s[1..s.len().saturating_sub(1)];
        out.insert(inner.rsplit('/').next().unwrap_or(inner).to_string());
    }
    out
}

pub(crate) const C: Lang = Lang { mentions: c_includes, names: c_names };

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn python_import_names() {
        let src = "import os, a.b as c\nfrom .util import (\n  x,\n  y as z,\n)\ndef f():\n    from pkg.mod import thing  # lazy\n";
        let m = python_imports("app/core/x.py", src);
        for w in ["os", "a", "b", "c", "util", "x", "y", "z", "pkg", "mod", "thing", "app", "core"] {
            assert!(m.contains(w), "{w}");
        }
        assert!(!m.contains("as"));
        assert!(!m.contains("lazy"));
        assert_eq!(python_names("app/core/__init__.py"), vec!["core"]);
        assert_eq!(python_names("app/core/models.pyi"), vec!["models"]);
    }

    #[test]
    fn c_include_names() {
        let src = "#include <vector>\n#include \"lib/foo.h\"\n#  include MACRO_HDR\n#if __has_include(<bar.hpp>)\n#endif\n";
        assert_eq!(c_includes("a.cc", src), set(&["*", "bar.hpp", "foo.h", "vector"]));
    }

    #[test]
    fn closure_follows_mentions_transitively() {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, t: &str| {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        w("pkg/__init__.py", "from .models import User\n");
        w("pkg/models.py", "import dataclasses\n");
        w("app.py", "import pkg\n");
        w("other.py", "import json\n");
        let files: Vec<String> = ["pkg/__init__.py", "pkg/models.py", "app.py", "other.py"].iter().map(|s| s.to_string()).collect();
        let got = closure(d.path(), &files, &[('M', "pkg/models.py".to_string())], &PYTHON);
        assert_eq!(got, set(&["app.py", "pkg/__init__.py", "pkg/models.py"]));
    }
}
