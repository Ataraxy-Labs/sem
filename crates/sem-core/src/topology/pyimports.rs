//! Python imports of repo modules that resolve to no file.
//!
//! Only what can be decided from the tree: a relative import (`from .x
//! import y`) names a module next to the importing file, and an absolute
//! import whose top package is one of the repo's own packages names a
//! module inside it. Either must exist as a `.py`/`.pyi`/`.pyx`/extension
//! file, a package directory or a namespace directory. Imports guarded by
//! `try:` (the `except ImportError` idiom) or an `if` (`TYPE_CHECKING`,
//! version checks) are optional by construction and skipped; an import of
//! a third-party top package is not the repo's to check.

use std::collections::BTreeSet;
use std::path::Path;

use tree_sitter::Node;

/// `(importing file, module as written)` for every import of a repo module
/// that resolves to nothing under `root`.
pub fn broken_imports(root: &Path, files: &[String]) -> BTreeSet<(String, String)> {
    broken_imports_in(root, files, None)
}

/// [`broken_imports`] of the files in `check` only (all when `None`); the
/// import roots still come from every file.
pub fn broken_imports_in(root: &Path, files: &[String], check: Option<&std::collections::HashSet<String>>) -> BTreeSet<(String, String)> {
    let py: Vec<&String> = files.iter().filter(|f| f.ends_with(".py")).collect();
    // import roots: the repo root and every directory holding a top-level
    // package (a package directory whose parent is not a package)
    let is_pkg = |d: &Path| d.join("__init__.py").is_file();
    let mut roots: BTreeSet<String> = BTreeSet::from([String::new()]);
    for f in &py {
        let mut dir = Path::new(f.as_str()).parent();
        while let Some(d) = dir {
            if d.as_os_str().is_empty() || !is_pkg(&root.join(d)) {
                break;
            }
            let parent = d.parent().unwrap_or(Path::new(""));
            if parent.as_os_str().is_empty() || !is_pkg(&root.join(parent)) {
                roots.insert(parent.to_string_lossy().to_string());
                break;
            }
            dir = Some(parent);
        }
    }
    let mut out = BTreeSet::new();
    for f in py.into_iter().filter(|f| check.is_none_or(|c| c.contains(f.as_str()))) {
        let Ok(src) = std::fs::read_to_string(root.join(f)) else { continue };
        let Some(tree) = crate::dataflow::lower::parse(f, &src) else { continue };
        let mut specs = Vec::new();
        collect(tree.root_node(), &src, &mut specs);
        for (module, names) in specs {
            if !module.is_empty() && module.chars().all(|c| c == '.') {
                // `from . import a, b`: one check (and one report) per name
                for n in names {
                    if !resolves(root, &roots, f, &module, std::slice::from_ref(&n)) {
                        out.insert((f.clone(), format!("{module}{n}")));
                    }
                }
            } else if !resolves(root, &roots, f, &module, &names) {
                out.insert((f.clone(), module));
            }
        }
    }
    out
}

/// `(module, imported names)` of every unguarded import statement.
fn collect(n: Node, src: &str, out: &mut Vec<(String, Vec<String>)>) {
    let text = |n: Node| n.utf8_text(src.as_bytes()).unwrap_or("").to_string();
    match n.kind() {
        // optional imports: `try: import x except ImportError`, `if TYPE_CHECKING:`
        "try_statement" | "if_statement" => return,
        "import_from_statement" => {
            if let Some(m) = n.child_by_field_name("module_name") {
                let mut names = Vec::new();
                let mut c = n.walk();
                for x in n.children_by_field_name("name", &mut c) {
                    let x = if x.kind() == "aliased_import" { x.child_by_field_name("name").unwrap_or(x) } else { x };
                    names.push(text(x));
                }
                out.push((text(m).split_whitespace().collect(), names));
            }
            return;
        }
        "import_statement" => {
            let mut c = n.walk();
            for x in n.children_by_field_name("name", &mut c) {
                let x = if x.kind() == "aliased_import" { x.child_by_field_name("name").unwrap_or(x) } else { x };
                out.push((text(x), Vec::new()));
            }
            return;
        }
        _ => {}
    }
    let mut c = n.walk();
    for k in n.named_children(&mut c) {
        collect(k, src, out);
    }
}

/// A module file, package or namespace directory at `base` (no extension).
fn module_at(base: &Path) -> bool {
    base.is_dir()
        || ["py", "pyi", "pyx", "pxd", "so", "pyd"].iter().any(|e| base.with_extension(e).is_file())
        || base.parent().and_then(|d| std::fs::read_dir(d).ok()).is_some_and(|rd| {
            let stem = base.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            // compiled extensions: `name.cpython-312-darwin.so`
            rd.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with(&format!("{stem}.")) && (n.ends_with(".so") || n.ends_with(".pyd"))
            })
        })
}

fn resolves(root: &Path, roots: &BTreeSet<String>, file: &str, module: &str, names: &[String]) -> bool {
    let dots = module.chars().take_while(|c| *c == '.').count();
    let rest = &module[dots..];
    if dots > 0 {
        // relative: `.` is the importing file's package, each further dot a parent
        let mut dir = Path::new(file).parent().unwrap_or(Path::new("")).to_path_buf();
        for _ in 1..dots {
            if !dir.pop() {
                return true; // above the repo: not ours to decide
            }
        }
        let base = root.join(&dir);
        if rest.is_empty() {
            // `from . import a, b`: each a submodule, or a name the package defines
            let init = std::fs::read_to_string(base.join("__init__.py")).ok();
            return names.iter().all(|n| {
                n == "*" || module_at(&base.join(n)) || init.as_deref().is_some_and(|t| t.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == n))
            });
        }
        return module_at(&base.join(rest.replace('.', "/")));
    }
    // absolute: only a module under one of the repo's own top packages
    let top = rest.split('.').next().unwrap_or(rest);
    let homes: Vec<&String> = roots.iter().filter(|r| root.join(r.as_str()).join(top).join("__init__.py").is_file()).collect();
    if homes.is_empty() {
        return true;
    }
    homes.iter().any(|r| module_at(&root.join(r.as_str()).join(rest.replace('.', "/"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        for (p, s) in files {
            let path = dir.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, s).unwrap();
        }
        (dir, files.iter().map(|(p, _)| p.to_string()).collect())
    }

    #[test]
    fn missing_relative_and_repo_absolute_modules_are_broken() {
        let (d, files) = tree(&[
            ("app/__init__.py", "VERSION = 1\n"),
            ("app/util.py", "X = 1\n"),
            ("app/main.py", "from .util import X\nfrom .constants import HEADER\nfrom app.settings import DEBUG\nfrom app.util import X as Y\nfrom . import VERSION\nfrom . import helpers\nimport requests\nfrom flask import Flask\n"),
            ("app/opt.py", "try:\n    from .fast import speedup\nexcept ImportError:\n    speedup = None\nfrom typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from .types import T\n"),
            ("src/lib/__init__.py", ""),
            ("src/lib/core.py", "from lib.missing import thing\nfrom lib.core import x\n"),
        ]);
        let b = broken_imports(d.path(), &files);
        let got: Vec<(&str, &str)> = b.iter().map(|(f, m)| (f.as_str(), m.as_str())).collect();
        assert_eq!(
            got,
            vec![("app/main.py", ".constants"), ("app/main.py", ".helpers"), ("app/main.py", "app.settings"), ("src/lib/core.py", "lib.missing")],
            "{b:#?}"
        );
    }
}
