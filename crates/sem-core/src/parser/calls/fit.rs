//! Call-signature fit for Python: every call the exact-or-unknown resolver
//! binds to exactly one repo function must fit that function's current
//! signature (argument count, keyword names, required parameters).
//!
//! Python has no compiler to report a caller left behind when a function's
//! contract changes (a parameter added, removed, renamed or made required).
//! The callers are usually in other files, invisible from the edit. This
//! check reports them — and only them: a site is judged only when
//!
//! * the resolver answers with a single repo function (or a class whose
//!   `__init__` it pins), never a same-name guess;
//! * the target is undecorated (`@staticmethod` / `@classmethod` aside) —
//!   any other decorator may change the signature;
//! * a constructed class is undecorated, has no metaclass keyword and does
//!   not define `__new__`;
//! * the call has no `*args` / `**kwargs` splat (its arity is unknown).

use std::path::Path as FsPath;

use rustc_hash::FxHashMap as HashMap;
use tree_sitter::Node;

use super::ir::{Expr, FileFacts};
use super::lang::Lang;
use super::scope::{Def, ScopeTables};
use super::select::{ImplTables, Pick, Resolver};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    PosOnly,
    PosOrKw,
    KwOnly,
}

#[derive(Clone, Debug)]
struct Param {
    name: String,
    kind: Kind,
    default: bool,
}

#[derive(Clone, Debug)]
struct Sig {
    params: Vec<Param>,
    varargs: bool,
    varkw: bool,
    /// Decorators other than `staticmethod` / `classmethod`.
    other_decorators: bool,
    classmethod: bool,
    line: u32,
}

#[derive(Clone, Debug, Default)]
struct ClassInfo {
    opaque: bool,
}

#[derive(Clone, Debug)]
struct CallShape {
    positional: usize,
    keywords: Vec<String>,
    splat: bool,
    line: u32,
    col: u32,
    text: String,
}

/// A call that does not fit its target's signature.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Misfit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// The call's first line, trimmed.
    pub text: String,
    /// `path:line` of the target's `def`.
    pub target: String,
    pub target_file: String,
    pub problem: String,
}

/// Per-file syntax the IR does not keep: signatures by `def` row, classes
/// by `class` row, call shapes by the byte offset of the callee's name.
#[derive(Default)]
struct Syntax {
    sigs: HashMap<u32, Sig>,
    classes: HashMap<u32, ClassInfo>,
    calls: HashMap<u32, CallShape>,
}

fn text<'s>(n: Node, src: &'s str) -> &'s str {
    n.utf8_text(src.as_bytes()).unwrap_or("")
}

fn decorators<'s>(n: Node, src: &'s str) -> Vec<&'s str> {
    let Some(p) = n.parent().filter(|p| p.kind() == "decorated_definition") else { return Vec::new() };
    let mut c = p.walk();
    p.named_children(&mut c)
        .filter(|d| d.kind() == "decorator")
        .map(|d| text(d, src).trim_start_matches('@').trim())
        .collect()
}

fn signature(n: Node, src: &str) -> Sig {
    let decos = decorators(n, src);
    let bare = |d: &str| d.rsplit('.').next().unwrap_or(d).to_string();
    let classmethod = decos.iter().any(|d| bare(d) == "classmethod");
    let other_decorators = decos.iter().any(|d| !matches!(bare(d).as_str(), "staticmethod" | "classmethod"));
    let mut params = Vec::new();
    let (mut varargs, mut varkw, mut kw_only) = (false, false, false);
    if let Some(ps) = n.child_by_field_name("parameters") {
        let mut c = ps.walk();
        for p in ps.named_children(&mut c) {
            let inner = if p.kind() == "typed_parameter" { p.named_child(0).unwrap_or(p) } else { p };
            match inner.kind() {
                "list_splat_pattern" => {
                    varargs = true;
                    kw_only = true;
                }
                "dictionary_splat_pattern" => varkw = true,
                "keyword_separator" => kw_only = true,
                "positional_separator" => {
                    for q in params.iter_mut() {
                        let q: &mut Param = q;
                        q.kind = Kind::PosOnly;
                    }
                }
                "identifier" | "default_parameter" | "typed_default_parameter" => {
                    let name_node = if inner.kind() == "identifier" { Some(inner) } else { inner.child_by_field_name("name") };
                    let Some(name_node) = name_node else { continue };
                    params.push(Param {
                        name: text(name_node, src).to_string(),
                        kind: if kw_only { Kind::KwOnly } else { Kind::PosOrKw },
                        default: inner.kind() != "identifier",
                    });
                }
                _ => {}
            }
        }
    }
    Sig { params, varargs, varkw, other_decorators, classmethod, line: n.start_position().row as u32 + 1 }
}

fn class_info(n: Node, src: &str) -> ClassInfo {
    let decorated = n.parent().is_some_and(|p| p.kind() == "decorated_definition");
    let metaclass = n.child_by_field_name("superclasses").is_some_and(|s| {
        let mut c = s.walk();
        let found = s.named_children(&mut c).any(|a| a.kind() == "keyword_argument");
        found
    });
    let defines_new = n.child_by_field_name("body").is_some_and(|b| {
        let mut c = b.walk();
        let found = b.named_children(&mut c).any(|s| {
            let d = if s.kind() == "decorated_definition" { s.child_by_field_name("definition").unwrap_or(s) } else { s };
            d.kind() == "function_definition" && d.child_by_field_name("name").is_some_and(|nm| text(nm, src) == "__new__")
        });
        found
    });
    ClassInfo { opaque: decorated || metaclass || defines_new }
}

fn call_shape(n: Node, src: &str) -> Option<(u32, CallShape)> {
    let f = n.child_by_field_name("function")?;
    let name = if f.kind() == "attribute" { f.child_by_field_name("attribute")? } else { f };
    let args = n.child_by_field_name("arguments")?;
    let (mut positional, mut keywords, mut splat) = (0, Vec::new(), false);
    if args.kind() == "generator_expression" {
        positional = 1;
    } else {
        let mut c = args.walk();
        for a in args.named_children(&mut c) {
            match a.kind() {
                "keyword_argument" => {
                    if let Some(k) = a.child_by_field_name("name") {
                        keywords.push(text(k, src).to_string());
                    }
                }
                "list_splat" | "dictionary_splat" => splat = true,
                "comment" => {}
                _ => positional += 1,
            }
        }
    }
    let p = n.start_position();
    let first = text(n, src).lines().next().unwrap_or("").trim().chars().take(120).collect();
    Some((name.start_byte() as u32, CallShape { positional, keywords, splat, line: p.row as u32 + 1, col: p.column as u32 + 1, text: first }))
}

fn syntax(tree: &tree_sitter::Tree, src: &str) -> Syntax {
    let mut s = Syntax::default();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "function_definition" => {
                s.sigs.insert(n.start_position().row as u32, signature(n, src));
            }
            "class_definition" => {
                s.classes.insert(n.start_position().row as u32, class_info(n, src));
            }
            "call" => {
                if let Some((at, shape)) = call_shape(n, src) {
                    s.calls.insert(at, shape);
                }
            }
            _ => {}
        }
        let mut c = n.walk();
        stack.extend(n.children(&mut c));
    }
    s
}

/// Why `shape` does not fit `sig` (after dropping `skip` bound receivers), if it does not.
fn misfit(sig: &Sig, skip: usize, shape: &CallShape) -> Option<String> {
    let params = &sig.params[skip.min(sig.params.len())..];
    let positional: Vec<&Param> = params.iter().filter(|p| p.kind != Kind::KwOnly).collect();
    let mut problems = Vec::new();
    if shape.positional > positional.len() && !sig.varargs {
        problems.push(format!("takes at most {} positional argument(s), {} given", positional.len(), shape.positional));
    }
    let mut bound: Vec<&str> = positional.iter().take(shape.positional).map(|p| p.name.as_str()).collect();
    for k in &shape.keywords {
        match params.iter().find(|p| &p.name == k) {
            Some(p) if p.kind == Kind::PosOnly => {
                if !sig.varkw {
                    problems.push(format!("`{k}` is positional-only"));
                }
            }
            Some(_) if bound.contains(&k.as_str()) => problems.push(format!("got multiple values for `{k}`")),
            Some(_) => bound.push(k),
            None if sig.varkw => {}
            None => problems.push(format!("unexpected keyword argument `{k}`")),
        }
    }
    let missing: Vec<&str> = params.iter().filter(|p| !p.default && !bound.contains(&p.name.as_str())).map(|p| p.name.as_str()).collect();
    if !missing.is_empty() {
        problems.push(format!("missing required argument(s) {}", missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")));
    }
    (!problems.is_empty()).then(|| problems.join("; "))
}

/// Every resolvable call in `files` (repo-relative `.py` paths under
/// `root`) that does not fit its target's signature.
pub fn python_misfits(root: &FsPath, files: &[String]) -> Vec<Misfit> {
    let lang: &dyn Lang = &super::python::PYTHON;
    let config = crate::parser::plugins::code::languages::get_language_config(".py");
    let Some(language) = config.and_then(|c| (c.get_language)()) else { return Vec::new() };
    let parsed: Vec<(String, FileFacts, Syntax)> = files
        .iter()
        .filter_map(|p| {
            let src = std::fs::read_to_string(root.join(p)).ok()?;
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&language).ok()?;
            let tree = parser.parse(&src, None)?;
            Some((p.clone(), lang.lower(&tree, &src), syntax(&tree, &src)))
        })
        .collect();
    let named: Vec<(&str, &FileFacts)> = parsed.iter().map(|(p, f, _)| (p.as_str(), f)).collect();
    let facts: Vec<&FileFacts> = parsed.iter().map(|(_, f, _)| f).collect();
    let layout = lang.layout(root, &named);
    let tables = ScopeTables::build(&facts, &layout);
    let impls = ImplTables::build(&facts, &tables.view(), lang);
    let hints = super::param_hints(lang, &facts, &tables, &impls);
    let mut r = Resolver::new(lang, &facts, tables.view(), &impls);
    r.param_hints = Some(&hints);
    let mut overrides: HashMap<(u32, u32), Vec<(u32, u32)>> = HashMap::default();
    for (base, o) in super::override_pairs(&r, &facts, &impls) {
        overrides.entry(base).or_default().push(o);
    }
    let mut out = Vec::new();
    for (fi, (path, f, syn)) in parsed.iter().enumerate() {
        for ca in &f.call_args {
            let Some(shape) = syn.calls.get(&ca.at) else { continue };
            if shape.splat {
                continue;
            }
            let cx = r.fn_cx(fi as u32, ca.func, ca.scope);
            let pick = r.pick(ca.call, &cx, ca.at, 0);
            // `obj.m(..)` binds the receiver; `Cls.m(..)` (a class as receiver) does not
            let method_call = match f.expr(ca.call) {
                Expr::Method(recv, _) => !(matches!(f.expr(recv), Expr::Path(..) | Expr::Field(..))
                    && matches!(r.pick(recv, &cx, ca.at, 0),
                        Pick::Defs(ref d, _) if !d.is_empty() && d.iter().all(|x| matches!(x, Def::Type(..))))),
                _ => false,
            };
            let target = match (&pick, pick.single_fn()) {
                (_, Some((tf, ti, _))) => Some((tf, ti, method_call)),
                (Pick::Defs(d, Some(ty)), None) if matches!(d[..], [Def::Type(..)]) => {
                    let Def::Type(cf, ct) = d[0] else { continue };
                    let class_row = facts[cf as usize].types[ct as usize].row;
                    if parsed[cf as usize].2.classes.get(&class_row).is_none_or(|c| c.opaque) {
                        continue;
                    }
                    r.method(ty, "__init__", 0).single_fn().map(|(tf, ti, _)| (tf, ti, true))
                }
                _ => None,
            };
            let Some((tf, ti, receiver_bound)) = target else { continue };
            // a bound method call may run any override: judge it only if it
            // fits none of them (each undecorated and with a known signature)
            let mut candidates = vec![(tf, ti)];
            if receiver_bound {
                let mut i = 0;
                while i < candidates.len() {
                    for &o in overrides.get(&candidates[i]).into_iter().flatten() {
                        if !candidates.contains(&o) {
                            candidates.push(o);
                        }
                    }
                    i += 1;
                }
            }
            let mut first: Option<String> = None;
            let mut fits_one = false;
            for &(cf, ci) in &candidates {
                let d = &facts[cf as usize].fns[ci as usize];
                let Some(sig) = parsed[cf as usize].2.sigs.get(&d.row).filter(|s| !s.other_decorators) else {
                    fits_one = true; // unknown signature: cannot rule this target out
                    break;
                };
                // `obj.m(x)` / `Cls.cm(x)` bind the receiver; an unbound `Cls.m(obj, x)` passes it
                let skip = usize::from(d.has_self && (receiver_bound || sig.classmethod));
                match misfit(sig, skip, shape) {
                    None => {
                        fits_one = true;
                        break;
                    }
                    Some(p) => {
                        first.get_or_insert(p);
                    }
                }
            }
            if fits_one {
                continue;
            }
            let decl = &facts[tf as usize].fns[ti as usize];
            let Some(sig) = parsed[tf as usize].2.sigs.get(&decl.row) else { continue };
            if let Some(problem) = first {
                let target_file = parsed[tf as usize].0.clone();
                out.push(Misfit {
                    file: path.clone(),
                    line: shape.line,
                    col: shape.col,
                    text: shape.text.clone(),
                    target: format!("{target_file}:{}", sig.line),
                    target_file,
                    problem: format!("{}(): {problem}", decl.name),
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.file, a.line, a.col).cmp(&(&b.file, b.line, b.col)));
    out
}

#[cfg(test)]
mod tests {
    use super::python_misfits;

    fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<String>) {
        let d = tempfile::tempdir().unwrap();
        for (p, t) in files {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        }
        (d, files.iter().map(|(p, _)| p.to_string()).collect())
    }

    #[test]
    fn far_callers_of_a_changed_signature() {
        let (d, files) = repo(&[
            ("pkg/__init__.py", ""),
            ("pkg/util.py", "def fmt(value, width, *, fill=' '):\n    return str(value)\n\nclass Box:\n    def __init__(self, size):\n        self.size = size\n    def grow(self, by):\n        return self.size + by\n    @classmethod\n    def make(cls, size):\n        return cls(size)\n    @staticmethod\n    def unit():\n        return 1\n"),
            ("pkg/a.py", "from pkg.util import fmt as f, Box\n\ndef ok():\n    b = Box(3)\n    return f(1, 2, fill='x'), b.grow(1), Box.make(2), Box.unit(), Box.grow(b, 1)\n"),
            ("pkg/b.py", "from pkg import util\nfrom pkg.util import Box\n\ndef bad():\n    b = Box()\n    return util.fmt(1), util.fmt(1, 2, 3), util.fmt(1, 2, pad=0), b.grow(), Box.make(1, 2)\n\ndef unknown(g, *a):\n    return g(1), util.fmt(*a)\n"),
        ]);
        let got = python_misfits(d.path(), &files);
        let lines: Vec<String> = got.iter().map(|m| format!("{}:{} {}", m.file, m.line, m.problem)).collect();
        assert!(got.iter().all(|m| m.file == "pkg/b.py"), "{lines:#?}");
        let has = |s: &str| lines.iter().any(|l| l.contains(s));
        assert!(has("__init__(): missing required argument(s) `size`"), "{lines:#?}");
        assert!(has("fmt(): missing required argument(s) `width`"), "{lines:#?}");
        assert!(has("fmt(): takes at most 2 positional argument(s), 3 given"), "{lines:#?}");
        assert!(has("fmt(): unexpected keyword argument `pad`"), "{lines:#?}");
        assert!(has("grow(): missing required argument(s) `by`"), "{lines:#?}");
        assert!(has("make(): takes at most 1 positional argument(s), 2 given"), "{lines:#?}");
        assert_eq!(got.len(), 6, "{lines:#?}");
    }

    #[test]
    fn chained_receivers_and_overrides() {
        let (d, files) = repo(&[
            ("p/__init__.py", ""),
            ("p/base.py", "class Pattern:\n    def handle(self, m):\n        return m\n\nclass Inline(Pattern):\n    def handle(self, m, data):\n        return m\n\nclass Reader:\n    def __init__(self, b):\n        self.b = b\n    def read(self):\n        return self.b\n"),
            ("p/use.py", "from p.base import Pattern, Reader\nfrom p import base\n\ndef run(pattern: Pattern, m):\n    return pattern.handle(m, 1), Reader(b'x').read(), pattern.handle(), base.Reader.read(Reader(1))\n"),
        ]);
        let got = python_misfits(d.path(), &files);
        let lines: Vec<String> = got.iter().map(|m| format!("{}:{} {}", m.file, m.line, m.problem)).collect();
        assert_eq!(lines, vec!["p/use.py:5 handle(): missing required argument(s) `m`".to_string()], "{lines:#?}");
    }

    #[test]
    fn decorated_targets_and_opaque_classes_are_not_judged() {
        let (d, files) = repo(&[
            ("m/__init__.py", ""),
            ("m/x.py", "import functools\n\ndef deco(fn):\n    return fn\n\n@deco\ndef f(a):\n    return a\n\nclass K:\n    def __new__(cls, *a):\n        return object.__new__(cls)\n    def __init__(self, a):\n        pass\n\ndef use():\n    return f(), K()\n"),
        ]);
        assert!(python_misfits(d.path(), &files).is_empty());
    }
}
