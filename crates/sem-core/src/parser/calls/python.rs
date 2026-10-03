//! Python front end: lowers a tree-sitter-python tree into [`FileFacts`],
//! plus Python's data for the shared stages: a package is a directory
//! (`__init__.py` holds its own names), modules are imported absolutely
//! from top-level packages or relatively (`from . import x`), classes are
//! types whose methods live in an impl and whose bases are searched in
//! order, `self.x = ..` in methods types attributes, and annotations type
//! parameters, variables and returns. Unannotated, uninferable receivers
//! stay unresolved — there is no same-name guessing.

use std::path::Path as FsPath;

use rustc_hash::FxHashMap as HashMap;
use tree_sitter::Node;

use super::infer::TUPLE;
use super::ir::*;
use super::lang::{BuiltinRet, ClosureArg, Lang, Layout};

pub struct Python;

pub static PYTHON: Python = Python;

impl Lang for Python {
    fn lower(&self, tree: &tree_sitter::Tree, src: &str) -> FileFacts {
        lower(tree, src)
    }

    fn layout(&self, root: &FsPath, files: &[(&str, &FileFacts)]) -> Layout {
        let _ = root;
        layout(files)
    }

    fn builtin_method(&self, recv: &str, method: &str) -> Option<BuiltinRet> {
        const MAP: &[&str] = &[
            "dict",
            "Dict",
            "Mapping",
            "MutableMapping",
            "defaultdict",
            "OrderedDict",
        ];
        const SEQ: &[&str] = &["list", "List", "Sequence", "MutableSequence", "deque"];
        let map = MAP.contains(&recv);
        Some(match method {
            "get" | "pop" | "setdefault" | "__getitem__" if map => BuiltinRet::Arg(1),
            "values" if map => BuiltinRet::Wrap("list", 1),
            "keys" if map => BuiltinRet::Wrap("list", 0),
            "items" if map => BuiltinRet::WrapAll("dict_items"),
            "pop" | "__getitem__" if SEQ.contains(&recv) => BuiltinRet::Arg(0),
            "copy" => BuiltinRet::Same,
            _ => return None,
        })
    }

    fn builtin_field(&self, _ty: &str, _field: &str) -> Option<BuiltinRet> {
        None
    }

    fn deref(&self, ty: &str) -> Option<BuiltinRet> {
        match ty {
            "Final" | "ClassVar" | "Annotated" => Some(BuiltinRet::Arg(0)),
            // a class object has its class's (class)methods
            "type" | "Type" => Some(BuiltinRet::Arg(0)),
            _ => None,
        }
    }

    fn variant_payload(&self, _ctor: &str, _field: &str) -> Option<BuiltinRet> {
        None
    }

    fn closure_param(&self, _method: &str, _pos: usize, _k: usize) -> Option<ClosureArg> {
        None
    }

    fn elem(&self, container: &str) -> Option<BuiltinRet> {
        match container {
            "list" | "List" | "set" | "Set" | "frozenset" | "FrozenSet" | "Sequence"
            | "MutableSequence" | "Iterable" | "Iterator" | "Collection" | "dict" | "Dict"
            | "Mapping" | "MutableMapping" | "tuple" | "Tuple" | "deque" | "Generator"
            | "AbstractSet" | "slice" => Some(BuiltinRet::Arg(0)),
            "dict_items" => Some(BuiltinRet::WrapAll(TUPLE)),
            _ => None,
        }
    }

    fn ordered_bases(&self) -> bool {
        true
    }

    fn infer_params_from_calls(&self) -> bool {
        true
    }

    fn function_scoped_names(&self) -> bool {
        true
    }

    fn virtual_methods(&self) -> bool {
        true
    }

    fn neutral_base(&self, ty: &str) -> bool {
        matches!(ty, "Generic" | "Protocol" | "ABC" | "object")
    }

    fn self_value(&self) -> &'static str {
        "" // `self` is an ordinary, typed first parameter
    }

    fn self_type(&self) -> &'static str {
        "Self"
    }

    fn call_result(&self, ty: &str) -> Option<BuiltinRet> {
        // calling a class object (`type[X]`) makes an X
        matches!(ty, "type" | "Type").then_some(BuiltinRet::Arg(0))
    }
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

pub fn lower(tree: &tree_sitter::Tree, src: &str) -> FileFacts {
    let mut cx = Lower {
        src,
        f: FileFacts::default(),
        interner: HashMap::default(),
        memo: HashMap::default(),
        classes: Vec::new(),
        funcs: Vec::new(),
        callees: Default::default(),
        annotated: HashMap::default(),
    };
    cx.f.exprs.push(Expr::Unknown);
    cx.f.scopes.push(ScopeDecl {
        name: None,
        parent: None,
        out_of_line: false,
    });
    cx.block(tree.root_node(), 0, None);
    let mut f = cx.f;
    f.locals.sort_by_key(|l| (l.func, l.at));
    f.returns.sort_by_key(|r| r.func);
    // `_name`s are private to their module (not exported by `import *`)
    for u in &mut f.uses {
        if u.name.as_deref().is_some_and(|n| n.starts_with('_')) {
            u.public = false;
        }
    }
    let private: Vec<(u32, Name)> = f
        .fns
        .iter()
        .filter(|d| d.owner == Owner::Free)
        .map(|d| (d.scope, d.name.clone()))
        .chain(f.types.iter().map(|t| (t.scope, t.name.clone())))
        .chain(f.values.iter().map(|v| (v.scope, v.name.clone())))
        .filter(|(_, n)| n.starts_with('_'))
        .collect();
    f.private = private;
    f
}

/// The class whose body is being lowered.
struct ClassCx {
    ty: u32,
    imp: u32,
    name: Name,
}

struct Lower<'a> {
    src: &'a str,
    f: FileFacts,
    interner: HashMap<&'a str, Sym>,
    memo: HashMap<usize, ExprId>,
    classes: Vec<ClassCx>,
    /// Functions whose bodies are being lowered (innermost last).
    funcs: Vec<u32>,
    /// Callee expressions of call sites (by node id): not mentions.
    callees: rustc_hash::FxHashSet<usize>,
    /// Annotated variables' types (`type_pool` index), by function and name.
    annotated: HashMap<(u32, &'a str), u32>,
}

fn row(n: Node) -> u32 {
    n.start_position().row as u32
}

impl<'a> Lower<'a> {
    fn text(&self, n: Node) -> &'a str {
        self.src.get(n.start_byte()..n.end_byte()).unwrap_or("")
    }

    fn sym(&mut self, s: &'a str) -> Sym {
        if let Some(&id) = self.interner.get(s) {
            return id;
        }
        let id = Sym(self.f.syms.len() as u32);
        self.f.syms.push(s.into());
        self.interner.insert(s, id);
        id
    }

    fn node(&mut self, e: Expr) -> ExprId {
        if e == Expr::Unknown {
            return ExprId::UNKNOWN;
        }
        self.f.exprs.push(e);
        ExprId(self.f.exprs.len() as u32 - 1)
    }

    fn path_expr(&mut self, segs: &[&'a str]) -> ExprId {
        let start = self.f.path_pool.len() as u32;
        for s in segs {
            let id = self.sym(s);
            self.f.path_pool.push(id);
        }
        self.node(Expr::Path(Span {
            start,
            len: segs.len() as u32,
        }))
    }

    /// `cast(T, v)`: a T.
    fn cast(&mut self, args: Option<Node<'a>>) -> ExprId {
        match args.and_then(|a| a.named_child(0)) {
            Some(t) => {
                let te = self.type_expr(t);
                self.typed(te)
            }
            None => ExprId::UNKNOWN,
        }
    }

    /// Any one of `alts` (unknown when there are none).
    fn alternatives(&mut self, alts: &[ExprId]) -> ExprId {
        if alts.is_empty() {
            return ExprId::UNKNOWN;
        }
        let l = self.list(alts);
        self.node(Expr::Union(l))
    }

    /// An outside generic type applied to expressions' types.
    fn ext(&mut self, name: &'a str, args: &[ExprId]) -> ExprId {
        let (name, l) = (self.sym(name), self.list(args));
        self.node(Expr::Ext(name, l))
    }

    /// A builtin that iterates its arguments (see [`iteration`]), as a
    /// list of what it yields.
    fn iteration(&mut self, shape: Iteration, args: Option<Node<'a>>) -> ExprId {
        let args: Vec<Node> = match args {
            Some(a) => {
                let mut c = a.walk();
                a.named_children(&mut c)
                    .filter(|k| k.kind() != "keyword_argument")
                    .collect()
            }
            None => Vec::new(),
        };
        let mut elems: Vec<ExprId> = Vec::new();
        for a in &args {
            let e = self.expr(*a);
            elems.push(self.node(Expr::Elem(e)));
        }
        let item = match shape {
            Iteration::Same => match elems.first() {
                Some(&e) => e,
                None => return ExprId::UNKNOWN,
            },
            Iteration::Next => return elems.first().copied().unwrap_or(ExprId::UNKNOWN),
            Iteration::Enumerate => {
                let int = self.typed(named("int"));
                let tuple = [int, elems.first().copied().unwrap_or(ExprId::UNKNOWN)];
                let l = self.list(&tuple);
                self.node(Expr::Tuple(l))
            }
            Iteration::Zip => {
                let l = self.list(&elems);
                self.node(Expr::Tuple(l))
            }
        };
        self.ext("list", &[item])
    }

    fn list(&mut self, items: &[ExprId]) -> Span {
        let start = self.f.expr_pool.len() as u32;
        self.f.expr_pool.extend_from_slice(items);
        Span {
            start,
            len: items.len() as u32,
        }
    }

    fn typed(&mut self, t: TypeExpr) -> ExprId {
        let i = self.type_idx(t);
        self.node(Expr::Typed(i))
    }

    fn type_idx(&mut self, t: TypeExpr) -> u32 {
        self.f.type_pool.push(t);
        self.f.type_pool.len() as u32 - 1
    }

    /// Statements of a module / class / function body. `func` is the
    /// enclosing function; `scope` where definitions are declared.
    fn block(&mut self, n: Node<'a>, scope: u32, func: Option<u32>) {
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            match x.kind() {
                "function_definition" | "decorated_definition" | "class_definition" => {
                    self.block_item(x, scope)
                }
                "import_statement" | "import_from_statement" | "future_import_statement" => {
                    self.import(x, scope)
                }
                "expression_statement" if func.is_none() => {
                    // module-level assignments: typed values
                    if let Some(a) = x.named_child(0).filter(|a| a.kind() == "assignment") {
                        self.module_value(a, scope);
                    }
                    self.body(x, None, scope);
                }
                _ if func.is_none() && is_compound(x.kind()) => {
                    // `if TYPE_CHECKING:` / `try:` at module level
                    let base = stack.len();
                    let mut c = x.walk();
                    stack.extend(x.named_children(&mut c));
                    stack[base..].reverse();
                }
                "block" | "module" => {
                    let base = stack.len();
                    let mut c = x.walk();
                    stack.extend(x.named_children(&mut c));
                    stack[base..].reverse();
                }
                _ => {
                    if func.is_none() {
                        self.body(x, None, scope);
                    }
                }
            }
        }
    }

    fn import(&mut self, n: Node<'a>, scope: u32) {
        if n.kind() == "import_statement" {
            let mut c = n.walk();
            let names: Vec<Node> = n.children_by_field_name("name", &mut c).collect();
            for nm in names {
                match nm.kind() {
                    "dotted_name" => {
                        // `import a.b.c` binds `a`
                        let segs = self.dotted(nm);
                        if let Some(first) = segs.first() {
                            self.f.uses.push(UseDecl {
                                scope,
                                path: Path(vec![(*first).into()]),
                                name: Some((*first).into()),
                                glob: false,
                                public: true,
                            });
                        }
                    }
                    "aliased_import" => {
                        let (Some(p), Some(a)) = (
                            nm.child_by_field_name("name"),
                            nm.child_by_field_name("alias"),
                        ) else {
                            continue;
                        };
                        let segs = self.dotted(p);
                        self.f.uses.push(UseDecl {
                            scope,
                            path: Path(segs.iter().map(|s| (*s).into()).collect()),
                            name: Some(self.text(a).into()),
                            glob: false,
                            public: true,
                        });
                    }
                    _ => {}
                }
            }
            return;
        }
        // from <module> import <names>
        let Some(module) = n.child_by_field_name("module_name") else {
            return;
        };
        let mut base: Vec<Name> = Vec::new();
        match module.kind() {
            "relative_import" => {
                let mut c = module.walk();
                for k in module.named_children(&mut c) {
                    match k.kind() {
                        "import_prefix" => {
                            let dots = self.text(k).chars().filter(|c| *c == '.').count();
                            base.push("^".into());
                            base.extend(std::iter::repeat_n(
                                Name::from("super"),
                                dots.saturating_sub(1),
                            ));
                        }
                        "dotted_name" => base.extend(self.dotted(k).into_iter().map(Into::into)),
                        _ => {}
                    }
                }
            }
            _ => base.extend(self.dotted(module).into_iter().map(Into::into)),
        }
        let mut c = n.walk();
        let kids: Vec<Node> = n.named_children(&mut c).collect();
        for k in kids {
            if k.id() == module.id() {
                continue;
            }
            match k.kind() {
                "wildcard_import" => self.f.uses.push(UseDecl {
                    scope,
                    path: Path(base.clone()),
                    name: None,
                    glob: true,
                    public: true,
                }),
                "dotted_name" => {
                    let segs = self.dotted(k);
                    let mut path = base.clone();
                    path.extend(segs.iter().map(|s| Name::from(*s)));
                    let name = segs.last().copied().unwrap_or("");
                    self.f.uses.push(UseDecl {
                        scope,
                        path: Path(path),
                        name: Some(name.into()),
                        glob: false,
                        public: true,
                    });
                }
                "aliased_import" => {
                    let (Some(p), Some(a)) = (
                        k.child_by_field_name("name"),
                        k.child_by_field_name("alias"),
                    ) else {
                        continue;
                    };
                    let mut path = base.clone();
                    path.extend(self.dotted(p).into_iter().map(Name::from));
                    self.f.uses.push(UseDecl {
                        scope,
                        path: Path(path),
                        name: Some(self.text(a).into()),
                        glob: false,
                        public: true,
                    });
                }
                _ => {}
            }
        }
    }

    fn dotted(&self, n: Node) -> Vec<&'a str> {
        match n.kind() {
            "dotted_name" => {
                let mut c = n.walk();
                n.named_children(&mut c).map(|k| self.text(k)).collect()
            }
            "identifier" => vec![self.text(n)],
            _ => Vec::new(),
        }
    }

    /// `X = Foo()` / `X: Foo = ..` at module level: a typed module value.
    fn module_value(&mut self, a: Node<'a>, scope: u32) {
        let Some(left) = a
            .child_by_field_name("left")
            .filter(|l| l.kind() == "identifier")
        else {
            return;
        };
        let ty = a.child_by_field_name("type").map(|t| self.type_expr(t));
        let init = match (&ty, a.child_by_field_name("right")) {
            (None, Some(r)) => Some(self.expr(r)),
            _ => None,
        };
        self.f.values.push(ValueDecl {
            name: self.text(left).into(),
            row: row(left),
            scope,
            ty,
            init,
        });
    }

    fn class(&mut self, n: Node<'a>, scope: u32) {
        let Some(name) = n.child_by_field_name("name") else {
            return;
        };
        let name: Name = self.text(name).into();
        let mut embeds = Vec::new();
        if let Some(sup) = n.child_by_field_name("superclasses") {
            self.body(sup, self.funcs.last().copied(), scope);
            let mut c = sup.walk();
            for b in sup.named_children(&mut c) {
                if b.kind() != "keyword_argument" {
                    embeds.push(self.type_expr(b)); // `Base`, `mod.Base`, `Base[T]`
                }
            }
        }
        let ty = self.f.types.len() as u32;
        self.f.types.push(TypeDecl {
            name: name.clone(),
            row: row(n),
            scope,
            generics: Vec::new(),
            kind: TypeKind::Struct,
            fields: Vec::new(),
            variants: Vec::new(),
            embeds,
            field_inits: Vec::new(),
        });
        let imp = self.f.impls.len() as u32;
        self.f.impls.push(ImplDecl {
            scope,
            generics: Vec::new(),
            self_ty: named(&name),
            trait_: None,
            assoc_types: Vec::new(),
        });
        self.classes.push(ClassCx { ty, imp, name });
        if let Some(body) = n.child_by_field_name("body") {
            let mut c = body.walk();
            let stmts: Vec<Node> = body.named_children(&mut c).collect();
            for s in stmts {
                match s.kind() {
                    "function_definition" | "decorated_definition" | "class_definition" => {
                        self.block_item(s, scope)
                    }
                    "expression_statement" => {
                        // class attribute `x: T = ..` / `x = ..`
                        if let Some(a) = s.named_child(0).filter(|a| a.kind() == "assignment") {
                            if let (Some(l), Some(t)) = (
                                a.child_by_field_name("left")
                                    .filter(|l| l.kind() == "identifier"),
                                a.child_by_field_name("type"),
                            ) {
                                let te = self.type_expr(t);
                                let fname: Name = self.text(l).into();
                                self.f.types[ty as usize].fields.push((fname, te));
                            }
                        }
                        self.body(s, None, scope);
                    }
                    _ => self.body(s, None, scope),
                }
            }
        }
        self.classes.pop();
    }

    fn block_item(&mut self, n: Node<'a>, scope: u32) {
        match n.kind() {
            "function_definition" => self.function(n, &[], scope),
            "class_definition" => self.class(n, scope),
            "decorated_definition" => {
                let mut c = n.walk();
                let decorators: Vec<Node> = n
                    .named_children(&mut c)
                    .filter(|d| d.kind() == "decorator")
                    .collect();
                // decorators run in the enclosing scope
                let func = self.funcs.last().copied();
                for d in &decorators {
                    self.body(*d, func, scope);
                }
                let decorators: Vec<&'a str> = decorators
                    .iter()
                    .filter_map(|d| d.named_child(0))
                    .map(|d| self.text(d))
                    .collect();
                if let Some(def) = n.child_by_field_name("definition") {
                    match def.kind() {
                        "function_definition" => self.function(def, &decorators, scope),
                        "class_definition" => self.class(def, scope),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn function(&mut self, n: Node<'a>, decorators: &[&str], scope: u32) {
        let Some(name) = n.child_by_field_name("name") else {
            return;
        };
        // `@overload` stubs are rebound by the implementation that follows
        if decorators
            .iter()
            .any(|d| d.rsplit('.').next() == Some("overload"))
        {
            return;
        }
        let func = self.f.fns.len() as u32;
        let (start, end) = (n.start_byte() as u32, n.end_byte() as u32);
        // a method: directly in a class body (the innermost class context
        // is current only while its body is lowered, not in nested fns)
        let directly_in_class = n
            .parent()
            .and_then(|p| {
                if p.kind() == "decorated_definition" {
                    p.parent()
                } else {
                    Some(p)
                }
            })
            .and_then(|b| b.parent())
            .is_some_and(|c| c.kind() == "class_definition");
        let in_class: Option<(u32, u32, Name)> = self
            .classes
            .last()
            .filter(|_| directly_in_class)
            .map(|c| (c.ty, c.imp, c.name.clone()));
        let is_static = decorators.contains(&"staticmethod");
        let (owner, class_ty) = match &in_class {
            Some((_, imp, name)) => (Owner::Impl(*imp), Some(named(name))),
            None => (Owner::Free, None),
        };
        let has_self = class_ty.is_some() && !is_static;
        let mut params = Vec::new();
        if let Some(ps) = n.child_by_field_name("parameters") {
            let mut c = ps.walk();
            let kids: Vec<Node> = ps
                .named_children(&mut c)
                .filter(|p| p.kind() != "comment")
                .collect();
            for (i, p) in kids.into_iter().enumerate() {
                let (pname, ty) = match p.kind() {
                    "identifier" => (Some(p), None),
                    "typed_parameter" => (p.named_child(0), p.child_by_field_name("type")),
                    "default_parameter" | "typed_default_parameter" => {
                        (p.child_by_field_name("name"), p.child_by_field_name("type"))
                    }
                    _ => (None, None),
                };
                let Some(pname) = pname.filter(|n| n.kind() == "identifier") else {
                    continue;
                };
                let mut init = None;
                let te = if i == 0 && has_self {
                    class_ty.clone()
                } else {
                    let te = ty.map(|t| self.type_expr(t)).unwrap_or(TypeExpr::Unknown);
                    if te == TypeExpr::Unknown {
                        init = Some(self.node(Expr::Param(params.len() as u32)));
                    }
                    params.push(te.clone());
                    Some(te).filter(|t| *t != TypeExpr::Unknown)
                };
                let ty = te.map(|t| self.type_idx(t));
                let name = self.sym(self.text(pname));
                self.f.locals.push(Local {
                    func,
                    name,
                    at: start,
                    until: end,
                    ty,
                    init,
                });
            }
        }
        let ret = n
            .child_by_field_name("return_type")
            .map(|t| self.type_expr(t));
        self.f.fns.push(FnDecl {
            name: self.text(name).into(),
            row: row(n),
            scope,
            owner,
            generics: Vec::new(),
            params,
            has_self,
            enclosing: self.funcs.last().copied(),
            ret,
        });
        // annotations and defaults
        for part in ["parameters", "return_type"] {
            if let Some(p) = n.child_by_field_name(part) {
                self.body(p, Some(func), scope);
            }
        }
        if let Some(body) = n.child_by_field_name("body") {
            // Python functions see the enclosing function / module, not the
            // class body
            let body_scope = self.f.scopes.len() as u32;
            self.f.scopes.push(ScopeDecl {
                name: None,
                parent: Some(scope),
                out_of_line: false,
            });
            let self_class = in_class.map(|(ty, ..)| ty).filter(|_| has_self);
            let self_name = if has_self { self.first_param(n) } else { None };
            self.funcs.push(func);
            self.body_with(body, Some(func), body_scope, self_class.zip(self_name));
            self.funcs.pop();
        }
    }

    fn first_param(&self, f: Node<'a>) -> Option<&'a str> {
        let ps = f.child_by_field_name("parameters")?;
        let mut c = ps.walk();
        let p = ps.named_children(&mut c).find(|p| p.kind() != "comment")?;
        let n = if p.kind() == "identifier" {
            p
        } else {
            p.named_child(0)?
        };
        Some(self.text(n))
    }

    fn body(&mut self, n: Node<'a>, func: Option<u32>, scope: u32) {
        self.body_with(n, func, scope, None);
    }

    /// Walk a function body: locals, attribute initializers of `self`, call
    /// sites, and nested definitions.
    fn body_with(
        &mut self,
        root: Node<'a>,
        func: Option<u32>,
        scope: u32,
        this: Option<(u32, &'a str)>,
    ) {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if n.id() != root.id() {
                match n.kind() {
                    "function_definition" | "decorated_definition" | "class_definition" => {
                        self.block_item(n, scope);
                        continue;
                    }
                    "import_statement" | "import_from_statement" => {
                        self.import(n, scope);
                        continue;
                    }
                    _ => {}
                }
            }
            match n.kind() {
                "assignment" => {
                    let (left, right) = (
                        n.child_by_field_name("left"),
                        n.child_by_field_name("right"),
                    );
                    let ty = n.child_by_field_name("type").map(|t| self.type_expr(t));
                    // `a = b = v`: both are v
                    let mut value = right;
                    while let Some(v) = value.filter(|v| v.kind() == "assignment") {
                        value = v.child_by_field_name("right");
                    }
                    let init = value.map(|r| self.expr(r));
                    if let Some(left) = left {
                        self.assign(left, ty, init, n, func, scope, this);
                    }
                }
                "while_statement" => {
                    if let Some(f) = func {
                        self.f
                            .loops
                            .push((f, n.start_byte() as u32, n.end_byte() as u32));
                    }
                }
                "for_statement" | "for_in_clause" => {
                    if let (Some(f), "for_statement") = (func, n.kind()) {
                        self.f
                            .loops
                            .push((f, n.start_byte() as u32, n.end_byte() as u32));
                    }
                    if let (Some(left), Some(right), Some(f)) = (
                        n.child_by_field_name("left"),
                        n.child_by_field_name("right"),
                        func,
                    ) {
                        let over = self.expr(right);
                        let init = self.node(Expr::Elem(over));
                        let until = if n.kind() == "for_in_clause" {
                            n.parent().map_or(n.end_byte(), |p| p.end_byte())
                        } else {
                            n.end_byte()
                        } as u32;
                        let at = if n.kind() == "for_in_clause" {
                            n.parent().map_or(n.start_byte(), |p| p.start_byte())
                        } else {
                            n.start_byte()
                        } as u32;
                        self.bind(left, None, Some(init), at, until, f);
                    }
                }
                // `with cm as v:` binds `cm.__enter__()`; `except E as e:`
                // binds an E (any of `(A, B)`)
                "as_pattern" => {
                    if let (Some(alias), Some(value), Some(f)) =
                        (n.child_by_field_name("alias"), n.named_child(0), func)
                    {
                        let target = alias.named_child(0).unwrap_or(alias);
                        let (mut ty, mut init) = (None, None);
                        match n.parent().map(|p| p.kind()) {
                            Some("with_item") => {
                                let (cm, enter) = (self.expr(value), self.sym("__enter__"));
                                init = Some(self.node(Expr::Method(cm, enter)));
                            }
                            Some("except_clause") => {
                                let te = match value.kind() {
                                    "tuple" => {
                                        let mut c = value.walk();
                                        let kids: Vec<Node> =
                                            value.named_children(&mut c).collect();
                                        TypeExpr::Union(
                                            kids.into_iter().map(|k| self.type_expr(k)).collect(),
                                        )
                                    }
                                    _ => self.type_expr(value),
                                };
                                ty = Some(self.type_idx(te));
                            }
                            _ => {}
                        }
                        let until = scope_end(n);
                        self.bind(target, ty, init, n.end_byte() as u32, until, f);
                    }
                }
                "lambda" => {
                    if let (Some(ps), Some(f)) = (n.child_by_field_name("parameters"), func) {
                        let mut c = ps.walk();
                        let kids: Vec<Node> = ps.named_children(&mut c).collect();
                        for p in kids {
                            let (at, until) = (n.start_byte() as u32, n.end_byte() as u32);
                            self.bind(p, None, None, at, until, f);
                        }
                    }
                }
                "call" => self.call_site(n, func, scope),
                "identifier" | "attribute" if !self.callees.contains(&n.id()) && !binds(n) => {
                    let expr = self.expr(n);
                    let name = n.child_by_field_name("attribute").unwrap_or(n);
                    self.f.sites.push(Site {
                        func,
                        scope,
                        row: row(name),
                        at: name.start_byte() as u32,
                        kind: SiteKind::Ref,
                        expr,
                    });
                }
                "return_statement" => {
                    if let (Some(v), Some(f)) = (n.named_child(0), func) {
                        let value = self.expr(v);
                        self.f.returns.push(Returned {
                            func: f,
                            scope,
                            value,
                        });
                    }
                }
                _ => {}
            }
            let base = stack.len();
            let mut c = n.walk();
            stack.extend(n.named_children(&mut c));
            stack[base..].reverse();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assign(
        &mut self,
        left: Node<'a>,
        ty: Option<TypeExpr>,
        init: Option<ExprId>,
        stmt: Node<'a>,
        func: Option<u32>,
        scope: u32,
        this: Option<(u32, &'a str)>,
    ) {
        match left.kind() {
            "attribute" => {
                // `self.x = ..` in a method types attribute x of the class
                let (Some(obj), Some(attr)) = (
                    left.child_by_field_name("object"),
                    left.child_by_field_name("attribute"),
                ) else {
                    return;
                };
                let (Some((class, self_name)), Some(f)) = (this, func) else {
                    return;
                };
                if obj.kind() != "identifier" || self.text(obj) != self_name {
                    return;
                }
                let fname: Name = self.text(attr).into();
                let decl = &mut self.f.types[class as usize];
                if decl.fields.iter().any(|(n, _)| *n == fname)
                    || decl.field_inits.iter().any(|(n, ..)| *n == fname)
                {
                    return;
                }
                match (ty, init) {
                    (Some(te), _) => decl.fields.push((fname, te)),
                    (None, Some(e)) => decl.field_inits.push((fname, f, scope, e)),
                    _ => {}
                }
            }
            _ => {
                let Some(f) = func else { return };
                // a variable keeps the type it was annotated with once
                let ty = match (ty, left.kind()) {
                    (Some(t), "identifier") => {
                        let t = self.type_idx(t);
                        self.annotated.insert((f, self.text(left)), t);
                        Some(t)
                    }
                    (Some(t), _) => Some(self.type_idx(t)),
                    (None, "identifier") => self.annotated.get(&(f, self.text(left))).copied(),
                    (None, _) => None,
                };
                self.bind(left, ty, init, stmt.end_byte() as u32, scope_end(stmt), f);
            }
        }
    }

    /// Bind assignment / loop targets: a name, or a tuple of targets taking
    /// the matching element of the value.
    fn bind(
        &mut self,
        target: Node<'a>,
        ty: Option<u32>,
        init: Option<ExprId>,
        at: u32,
        until: u32,
        func: u32,
    ) {
        match target.kind() {
            "identifier" => {
                let name = self.sym(self.text(target));
                self.f.locals.push(Local {
                    func,
                    name,
                    at,
                    until,
                    ty,
                    init,
                });
            }
            "pattern_list" | "tuple_pattern" | "list_pattern" | "expression_list" | "tuple" => {
                let mut c = target.walk();
                let kids: Vec<Node> = target.named_children(&mut c).collect();
                for (i, k) in kids.into_iter().enumerate() {
                    let payload = match (init, POSITIONS.get(i)) {
                        (Some(e), Some(p)) => {
                            let (empty, idx) = (self.sym(""), self.sym(p));
                            Some(self.node(Expr::Payload(e, empty, idx)))
                        }
                        _ => None,
                    };
                    self.bind(k, None, payload, at, until, func);
                }
            }
            "typed_parameter" | "default_parameter" | "typed_default_parameter" => {
                let name = target
                    .child_by_field_name("name")
                    .or_else(|| target.named_child(0));
                if let Some(n) = name {
                    self.bind(n, ty, init, at, until, func);
                }
            }
            _ => {}
        }
    }

    fn call_site(&mut self, n: Node<'a>, func: Option<u32>, scope: u32) {
        let expr = match self.expr(n) {
            e => match self.f.expr(e) {
                Expr::Builtin(call, _) => call,
                _ => e,
            },
        };
        let name_node = match self.f.expr(expr) {
            Expr::Method(..) => n
                .child_by_field_name("function")
                .and_then(|c| c.child_by_field_name("attribute")),
            Expr::Call(_) => n.child_by_field_name("function"),
            _ => None,
        };
        let Some(name_node) = name_node else { return };
        self.f.sites.push(Site {
            func,
            scope,
            row: row(name_node),
            at: name_node.start_byte() as u32,
            kind: SiteKind::Call,
            expr,
        });
        if let Some(callee) = n.child_by_field_name("function") {
            self.callees.insert(callee.id());
        }
        // positional arguments, for typing the callee's parameters
        if let Some(args) = n.child_by_field_name("arguments") {
            let mut c = args.walk();
            let positional: Vec<Node> = args
                .named_children(&mut c)
                .take_while(|a| {
                    !matches!(
                        a.kind(),
                        "keyword_argument" | "list_splat" | "dictionary_splat"
                    )
                })
                .collect();
            let ids: Vec<ExprId> = positional.into_iter().map(|a| self.expr(a)).collect();
            let args = self.list(&ids);
            self.f.call_args.push(CallArgs {
                call: expr,
                args,
                func,
                scope,
                at: name_node.start_byte() as u32,
            });
        }
    }

    fn expr(&mut self, n: Node<'a>) -> ExprId {
        if let Some(&e) = self.memo.get(&n.id()) {
            return e;
        }
        let e = self.expr_uncached(n);
        self.memo.insert(n.id(), e);
        e
    }

    fn expr_uncached(&mut self, n: Node<'a>) -> ExprId {
        let child = |f: &str| n.child_by_field_name(f);
        match n.kind() {
            "identifier" => {
                let t = self.text(n);
                self.path_expr(&[t])
            }
            "attribute" => match (child("object"), child("attribute")) {
                (Some(o), Some(a)) => {
                    let (o, a) = (self.expr(o), self.sym(self.text(a)));
                    self.node(Expr::Field(o, a))
                }
                _ => ExprId::UNKNOWN,
            },
            "call" => {
                let Some(callee) = child("function") else {
                    return ExprId::UNKNOWN;
                };
                match callee.kind() {
                    "attribute" => match (
                        callee.child_by_field_name("object"),
                        callee.child_by_field_name("attribute"),
                    ) {
                        (Some(o), Some(a)) => {
                            let (o, name) = (self.expr(o), self.text(a));
                            let s = self.sym(name);
                            let call = self.node(Expr::Method(o, s));
                            if name != "cast" {
                                return call;
                            }
                            // `typing.cast(T, v)`
                            let typed = self.cast(child("arguments"));
                            self.node(Expr::Builtin(call, typed))
                        }
                        _ => ExprId::UNKNOWN,
                    },
                    "identifier" => {
                        let name = self.text(callee);
                        if name == "super" {
                            // `super()`: whichever base comes next in the MRO
                            return match self.classes.last() {
                                Some(c) => self.node(Expr::Super(c.ty)),
                                None => ExprId::UNKNOWN,
                            };
                        }
                        let args = n.child_by_field_name("arguments");
                        let p = self.path_expr(&[name]);
                        let call = self.node(Expr::Call(p));
                        // builtins, typed by what they do when the name is
                        // not the repo's own
                        let typed = match name {
                            "cast" => self.cast(args),
                            _ => match iteration(name) {
                                Some(shape) => self.iteration(shape, args),
                                None => return call,
                            },
                        };
                        self.node(Expr::Builtin(call, typed))
                    }
                    _ => ExprId::UNKNOWN,
                }
            }
            // `v[k]` is `v.__getitem__(k)`
            "subscript" => match child("value") {
                Some(v) => {
                    let (v, m) = (self.expr(v), self.sym("__getitem__"));
                    self.node(Expr::Method(v, m))
                }
                None => ExprId::UNKNOWN,
            },
            "parenthesized_expression" | "await" => n
                .named_child(0)
                .map(|e| self.expr(e))
                .unwrap_or(ExprId::UNKNOWN),
            // `a if cond else b`, `a or b`: either operand
            "conditional_expression" | "boolean_operator" => {
                let mut c = n.walk();
                let kids: Vec<Node> = n.named_children(&mut c).collect();
                let operands = match n.kind() {
                    "conditional_expression" => [kids.first().copied(), kids.get(2).copied()],
                    _ => [child("left"), child("right")],
                };
                let arms: Vec<ExprId> = operands
                    .into_iter()
                    .flatten()
                    .into_iter()
                    .map(|k| {
                        let e = self.expr(k);
                        self.node(Expr::At(k.start_byte() as u32, e))
                    })
                    .collect();
                self.alternatives(&arms)
            }
            "lambda" => match child("body") {
                Some(b) => {
                    let e = self.expr(b);
                    let body = self.node(Expr::At(b.start_byte() as u32, e));
                    self.node(Expr::Closure(body))
                }
                None => ExprId::UNKNOWN,
            },
            "string" | "concatenated_string" => self.typed(named("str")),
            // a list of its elements' types; a comprehension of its body's
            "list" => {
                let mut c = n.walk();
                let kids: Vec<Node> = n.named_children(&mut c).collect();
                let elems: Vec<ExprId> = kids.into_iter().map(|k| self.expr(k)).collect();
                let elem = self.alternatives(&elems);
                self.ext("list", &[elem])
            }
            "list_comprehension" => match child("body") {
                Some(b) => {
                    let e = self.expr(b);
                    let elem = self.node(Expr::At(b.start_byte() as u32, e));
                    self.ext("list", &[elem])
                }
                None => ExprId::UNKNOWN,
            },
            "dictionary" | "dictionary_comprehension" => self.typed(named("dict")),
            "set" | "set_comprehension" => self.typed(named("set")),
            _ => ExprId::UNKNOWN,
        }
    }

    /// An annotation as a type: `Foo`, `mod.Foo`, `Optional[Foo]`,
    /// `list[Foo]`, `"Foo"`, `Foo | None`.
    fn type_expr(&self, n: Node) -> TypeExpr {
        match n.kind() {
            "type" => n
                .named_child(0)
                .map(|t| self.type_expr(t))
                .unwrap_or(TypeExpr::Unknown),
            "identifier" if self.text(n) == "None" => TypeExpr::Union(Vec::new()),
            "identifier" => named(self.text(n)),
            "attribute" => match self.attr_path(n) {
                // `typing.Self`
                Some(p) if p.last() == Some(&"Self") => named("Self"),
                Some(p) => TypeExpr::Named {
                    path: Path(p.into_iter().map(Into::into).collect()),
                    args: Vec::new(),
                },
                None => TypeExpr::Unknown,
            },
            "generic_type" | "subscript" => {
                let base = n.named_child(0).map(|b| self.type_expr(b));
                let mut args = Vec::new();
                let mut c = n.walk();
                for a in n.named_children(&mut c).skip(1) {
                    if a.kind() == "type_parameter" {
                        // `dict[str, T]`: every argument
                        let mut c = a.walk();
                        args.extend(a.named_children(&mut c).map(|t| self.type_expr(t)));
                    } else {
                        args.push(self.type_expr(a));
                    }
                }
                let Some(TypeExpr::Named { path, .. }) = base else {
                    return TypeExpr::Unknown;
                };
                match path.last() {
                    "tuple" | "Tuple" if path.0.len() == 1 => TypeExpr::Tuple(args),
                    // `None` has no attributes: an optional value is used as
                    // its non-None type
                    "Optional" => args.into_iter().next().unwrap_or(TypeExpr::Unknown),
                    "Union" => TypeExpr::Union(args),
                    // `Callable[[A], R]`: calling it gives an R
                    "Callable" => TypeExpr::Fn(Vec::new(), args.pop().map(Box::new)),
                    _ => TypeExpr::Named { path, args },
                }
            }
            "none" => TypeExpr::Union(Vec::new()),
            "string" => {
                // a forward reference: "Foo" / "mod.Foo"
                let text = self.text(n).trim_matches(|c| c == '"' || c == '\'');
                if text
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                    && !text.is_empty()
                {
                    TypeExpr::Named {
                        path: Path(text.split('.').map(Into::into).collect()),
                        args: Vec::new(),
                    }
                } else {
                    TypeExpr::Unknown
                }
            }
            // `A | B | None`
            "binary_operator" | "union_type" => {
                let mut c = n.walk();
                let kids: Vec<Node> = n.named_children(&mut c).collect();
                TypeExpr::Union(kids.into_iter().map(|k| self.type_expr(k)).collect())
            }
            _ => TypeExpr::Unknown,
        }
    }

    fn attr_path(&self, n: Node) -> Option<Vec<&'a str>> {
        match n.kind() {
            "identifier" => Some(vec![self.text(n)]),
            "attribute" => {
                let mut p = self.attr_path(n.child_by_field_name("object")?)?;
                p.push(self.text(n.child_by_field_name("attribute")?));
                Some(p)
            }
            _ => None,
        }
    }
}

/// An identifier (or attribute) that is assigned, bound or a name slot —
/// not a use: assignment and loop targets, parameters, keyword names, the
/// attribute name in `a.b`.
fn binds(n: Node) -> bool {
    let mut child = n;
    let Some(mut p) = n.parent() else {
        return false;
    };
    // targets nest: `a, (b, c) = ..`, `for i, x in ..`
    while matches!(
        p.kind(),
        "pattern_list" | "tuple_pattern" | "list_pattern" | "tuple" | "list" | "expression_list"
    ) {
        child = p;
        match p.parent() {
            Some(pp) => p = pp,
            None => return false,
        }
    }
    let is = |field: &str| {
        p.child_by_field_name(field)
            .is_some_and(|c| c.id() == child.id())
    };
    match p.kind() {
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => is("left"),
        "keyword_argument"
        | "named_expression"
        | "default_parameter"
        | "typed_default_parameter" => is("name"),
        "attribute" => is("attribute"),

        "typed_parameter"
        | "parameters"
        | "lambda_parameters"
        | "as_pattern_target"
        | "global_statement"
        | "nonlocal_statement"
        | "list_splat_pattern"
        | "dictionary_splat_pattern"
        | "keyword_pattern" => true,
        _ => false,
    }
}

/// What a builtin that iterates its first argument(s) yields.
#[derive(Clone, Copy)]
enum Iteration {
    /// The argument's elements (`sorted(xs)`, `list(xs)`).
    Same,
    /// `(i, x)` pairs.
    Enumerate,
    /// Tuples of each argument's elements.
    Zip,
    /// One element: `next(it)`.
    Next,
}

fn iteration(builtin: &str) -> Option<Iteration> {
    Some(match builtin {
        "sorted" | "reversed" | "list" | "tuple" | "set" | "frozenset" | "iter" => Iteration::Same,
        "enumerate" => Iteration::Enumerate,
        "zip" => Iteration::Zip,
        "next" => Iteration::Next,
        _ => return None,
    })
}

const POSITIONS: [&str; 8] = ["0", "1", "2", "3", "4", "5", "6", "7"];

fn is_compound(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement"
            | "elif_clause"
            | "else_clause"
            | "try_statement"
            | "except_clause"
            | "finally_clause"
            | "with_statement"
            | "block"
    )
}

fn named(name: &str) -> TypeExpr {
    TypeExpr::Named {
        path: Path::single(name),
        args: Vec::new(),
    }
}

/// A binding is visible until the end of its enclosing function.
fn scope_end(n: Node) -> u32 {
    let mut cur = n.parent();
    while let Some(p) = cur {
        if matches!(p.kind(), "function_definition" | "lambda") {
            return p.end_byte() as u32;
        }
        cur = p.parent();
    }
    u32::MAX
}

// ---------------------------------------------------------------------------
// Layout: packages are directories; top-level packages are importable
// ---------------------------------------------------------------------------

fn layout(files: &[(&str, &FileFacts)]) -> Layout {
    let orphans: Vec<(usize, &str)> = files
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (i, *p))
        .collect();
    let mut layout = Layout {
        parent_of: vec![None; files.len()],
        pooled: files
            .iter()
            .map(|(p, _)| p.ends_with("__init__.py"))
            .collect(),
        pooled_uses: files
            .iter()
            .map(|(p, _)| p.ends_with("__init__.py"))
            .collect(),
        ..Layout::default()
    };
    layout.add_directory_modules(&orphans, files.len());
    // directories that are packages (hold an __init__.py)
    let mut is_pkg = vec![false; layout.dirs.len()];
    for (fi, (p, _)) in files.iter().enumerate() {
        if p.ends_with("__init__.py") {
            if let Some((d, _)) = &layout.orphan_dir[fi] {
                is_pkg[*d] = true;
            }
        }
    }
    // a top-level package: a package whose parent directory is not one
    let mut counts: HashMap<String, usize> = HashMap::default();
    let mut tops: Vec<(String, usize)> = Vec::new();
    for (d, (name, parent)) in layout.dirs.iter().enumerate() {
        if is_pkg[d] && !parent.is_some_and(|p| is_pkg[p]) {
            *counts.entry(name.clone()).or_default() += 1;
            tops.push((name.clone(), d));
        }
    }
    for (name, d) in tops {
        if counts[&name] == 1 {
            layout.dir_crates.insert(name, d);
        }
    }
    // PEP 420: a directory directly on the import path is importable as a
    // namespace package. The input root is on it, and so is any directory
    // named `site-packages` / `dist-packages` (an installed-dependency root).
    let on_path = |p: &Option<usize>| match p {
        Some(0) => true,
        Some(p) => matches!(layout.dirs[*p].0.as_str(), "site-packages" | "dist-packages"),
        None => false,
    };
    let namespace: Vec<(String, usize)> = layout
        .dirs
        .iter()
        .enumerate()
        .filter(|(d, (_, parent))| on_path(parent) && !is_pkg[*d])
        .map(|(d, (name, _))| (name.clone(), d))
        .collect();
    for (name, d) in namespace {
        if !layout.dir_crates.contains_key(&name) && !counts.contains_key(&name) {
            layout.dir_crates.insert(name, d);
        }
    }
    for (d, (name, parent)) in layout.dirs.iter().enumerate() {
        if *parent == Some(0) && !layout.dir_crates.contains_key(name) && !counts.contains_key(name)
        {
            layout.dir_crates.insert(name.clone(), d);
        }
    }
    // top-level modules (scripts, test helpers) next to no package
    let mut stems: HashMap<String, Vec<usize>> = HashMap::default();
    for (fi, (p, _)) in files.iter().enumerate() {
        if let Some((d, stem)) = &layout.orphan_dir[fi] {
            if !is_pkg[*d] && stem != "__init__" {
                stems.entry(stem.clone()).or_default().push(fi);
            }
        }
        let _ = p;
    }
    for (stem, fis) in stems {
        if let ([fi], false) = (fis.as_slice(), layout.dir_crates.contains_key(&stem)) {
            layout.crates.insert(stem, *fi);
        }
    }
    layout
}
