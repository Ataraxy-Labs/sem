//! Go front end: lowers a tree-sitter-go tree into [`FileFacts`], plus
//! Go's data for the shared stages: a package is a directory (its files'
//! top-level names are pooled), packages are imported by path (`go.mod`
//! module path + directory), methods hang off receiver types, embedded
//! struct fields promote methods, and interfaces are satisfied structurally.

use std::path::Path as FsPath;

use rustc_hash::FxHashMap as HashMap;
use tree_sitter::Node;

use super::ir::*;
use super::lang::{BuiltinRet, ClosureArg, Lang, Layout};

pub struct Go;

pub static GO: Go = Go;

impl Lang for Go {
    fn lower(&self, tree: &tree_sitter::Tree, src: &str) -> FileFacts {
        lower(tree, src)
    }

    fn layout(&self, root: &FsPath, files: &[(&str, &FileFacts)]) -> Layout {
        layout(root, files)
    }

    fn builtin_method(&self, _recv: &str, _method: &str) -> Option<BuiltinRet> {
        None
    }

    fn builtin_field(&self, _ty: &str, _field: &str) -> Option<BuiltinRet> {
        None
    }

    fn deref(&self, _ty: &str) -> Option<BuiltinRet> {
        None
    }

    fn variant_payload(&self, _ctor: &str, _field: &str) -> Option<BuiltinRet> {
        None
    }

    fn closure_param(&self, _method: &str, _pos: usize, _k: usize) -> Option<ClosureArg> {
        None
    }

    fn elem(&self, container: &str) -> Option<BuiltinRet> {
        match container {
            "slice" | "chan" => Some(BuiltinRet::Arg(0)),
            "map" => Some(BuiltinRet::Arg(1)),
            _ => None,
        }
    }

    fn structural_interfaces(&self) -> bool {
        true
    }

    fn self_value(&self) -> &'static str {
        "" // receivers are ordinary named parameters
    }

    fn self_type(&self) -> &'static str {
        ""
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
        callees: Default::default(),
    };
    cx.f.exprs.push(Expr::Unknown);
    cx.f.scopes.push(ScopeDecl {
        name: None,
        parent: None,
        out_of_line: false,
    });
    let root = tree.root_node();
    let mut c = root.walk();
    let kids: Vec<Node> = root.named_children(&mut c).collect();
    for n in kids {
        cx.top_level(n);
    }
    let mut f = cx.f;
    f.locals.sort_by_key(|l| (l.func, l.at));
    // unexported (lower-case) package-level names
    let private: Vec<(u32, Name)> = f
        .fns
        .iter()
        .filter(|d| d.owner == Owner::Free)
        .map(|d| d.name.clone())
        .chain(f.types.iter().map(|t| t.name.clone()))
        .chain(f.traits.iter().map(|t| t.name.clone()))
        .chain(f.values.iter().map(|v| v.name.clone()))
        .filter(|n| !n.starts_with(|c: char| c.is_uppercase()))
        .map(|n| (0, n))
        .collect();
    f.private = private;
    f
}

struct Lower<'a> {
    src: &'a str,
    f: FileFacts,
    interner: HashMap<&'a str, Sym>,
    memo: HashMap<usize, ExprId>,
    /// Callee expressions of call sites (by node id): not mentions.
    callees: rustc_hash::FxHashSet<usize>,
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

    fn typed(&mut self, t: TypeExpr) -> ExprId {
        self.f.type_pool.push(t);
        let i = self.f.type_pool.len() as u32 - 1;
        self.node(Expr::Typed(i))
    }

    fn type_idx(&mut self, t: TypeExpr) -> u32 {
        self.f.type_pool.push(t);
        self.f.type_pool.len() as u32 - 1
    }

    fn top_level(&mut self, n: Node<'a>) {
        match n.kind() {
            "import_declaration" => self.imports(n),
            "function_declaration" | "method_declaration" => self.function(n),
            "type_declaration" => {
                let mut c = n.walk();
                let specs: Vec<Node> = n.named_children(&mut c).collect();
                for s in specs {
                    if matches!(s.kind(), "type_spec" | "type_alias") {
                        self.type_spec(s);
                    }
                }
            }
            "var_declaration" | "const_declaration" => {
                let mut stack = vec![n];
                while let Some(x) = stack.pop() {
                    if matches!(x.kind(), "var_spec" | "const_spec") {
                        self.value_spec(x);
                        if let Some(v) = x.child_by_field_name("value") {
                            self.body(v, None);
                        }
                        continue;
                    }
                    let mut c = x.walk();
                    stack.extend(x.named_children(&mut c));
                }
            }
            _ => {}
        }
    }

    fn imports(&mut self, n: Node) {
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            if x.kind() != "import_spec" {
                let mut c = x.walk();
                stack.extend(x.named_children(&mut c));
                continue;
            }
            let Some(path) = x.child_by_field_name("path") else {
                continue;
            };
            let path = self.text(path).trim_matches('"').trim_matches('`');
            let alias = x.child_by_field_name("name").map(|a| self.text(a));
            let last = path.rsplit('/').next().unwrap_or(path);
            let (name, glob) = match alias {
                Some("_") => continue,
                Some(".") => (None, true),
                Some(a) => (Some(a), false),
                None => (Some(last), false),
            };
            self.f.uses.push(UseDecl {
                scope: 0,
                path: Path(vec![path.into()]),
                name: name.map(Into::into),
                glob,
                public: false,
            });
        }
    }

    fn type_spec(&mut self, n: Node<'a>) {
        let (Some(name), Some(t)) = (n.child_by_field_name("name"), n.child_by_field_name("type"))
        else {
            return;
        };
        let name: Name = self.text(name).into();
        let generics = self.generics(n.child_by_field_name("type_parameters"));
        self.type_mentions(t, None, &generics);
        match t.kind() {
            "interface_type" => {
                let idx = self.f.traits.len() as u32;
                let mut supertraits = Vec::new();
                let mut c = t.walk();
                let elems: Vec<Node> = t.named_children(&mut c).collect();
                for e in elems {
                    match e.kind() {
                        "method_elem" | "method_spec" => {
                            let Some(mname) = e.child_by_field_name("name") else {
                                continue;
                            };
                            let params = self.param_types(e.child_by_field_name("parameters"));
                            let ret = self.result(e.child_by_field_name("result"));
                            self.f.fns.push(FnDecl {
                                name: self.text(mname).into(),
                                row: row(e),
                                scope: 0,
                                owner: Owner::Trait(idx),
                                generics: Vec::new(),
                                params,
                                has_self: true,
                                enclosing: None,
                                ret,
                            });
                        }
                        "type_elem" | "constraint_elem" => {
                            let mut c = e.walk();
                            for k in e.named_children(&mut c) {
                                if let TypeExpr::Named { path, .. } = self.type_expr(k) {
                                    supertraits.push(path);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                self.f.traits.push(TraitDecl {
                    assoc: Vec::new(),
                    name,
                    row: row(n),
                    scope: 0,
                    generics,
                    supertraits,
                });
            }
            "struct_type" => {
                let mut fields = Vec::new();
                let mut embeds = Vec::new();
                let mut stack = vec![t];
                while let Some(x) = stack.pop() {
                    if x.kind() != "field_declaration" {
                        let mut c = x.walk();
                        stack.extend(x.named_children(&mut c));
                        continue;
                    }
                    let Some(ft) = x.child_by_field_name("type") else {
                        continue;
                    };
                    let ty = self.type_expr(ft);
                    let mut c = x.walk();
                    let names: Vec<Node> = x.children_by_field_name("name", &mut c).collect();
                    if names.is_empty() {
                        // embedded: the field is named after its type
                        if let Some(TypeExpr::Named { path, .. }) = strip_ref(&ty) {
                            fields.push((path.last().into(), ty.clone()));
                        }
                        embeds.push(ty);
                    } else {
                        for nm in names {
                            fields.push((self.text(nm).into(), ty.clone()));
                        }
                    }
                }
                self.f.types.push(TypeDecl {
                    name,
                    row: row(n),
                    scope: 0,
                    generics,
                    kind: TypeKind::Struct,
                    fields,
                    variants: Vec::new(),
                    embeds,
                    field_inits: Vec::new(),
                });
            }
            _ => {
                // `type A = B` aliases; `type A B` defines a new named type
                // with B's structure but its own (empty) method set
                let kind = if n.kind() == "type_alias" {
                    TypeKind::Alias(self.type_expr(t))
                } else {
                    TypeKind::Defined(self.type_expr(t))
                };
                self.f.types.push(TypeDecl {
                    name,
                    row: row(n),
                    scope: 0,
                    generics,
                    kind,
                    fields: Vec::new(),
                    variants: Vec::new(),
                    embeds: Vec::new(),
                    field_inits: Vec::new(),
                });
            }
        }
    }

    fn value_spec(&mut self, n: Node<'a>) {
        if let Some(t) = n.child_by_field_name("type") {
            self.type_mentions(t, None, &[]);
        }
        let ty = n.child_by_field_name("type").map(|t| self.type_expr(t));
        let mut c = n.walk();
        let names: Vec<Node> = n.children_by_field_name("name", &mut c).collect();
        let values: Vec<Node> = match n.child_by_field_name("value") {
            Some(v) if v.kind() == "expression_list" => {
                let mut c = v.walk();
                v.named_children(&mut c).collect()
            }
            Some(v) => vec![v],
            None => Vec::new(),
        };
        for (i, nm) in names.iter().enumerate() {
            let init = match (&ty, values.len() == names.len()) {
                (None, true) => Some(self.expr(values[i])),
                _ => None,
            };
            self.f.values.push(ValueDecl {
                name: self.text(*nm).into(),
                row: row(*nm),
                scope: 0,
                ty: ty.clone(),
                init,
            });
        }
    }

    fn generics(&self, tp: Option<Node>) -> Vec<Generic> {
        let mut out = Vec::new();
        let Some(tp) = tp else { return out };
        let mut c = tp.walk();
        for d in tp.named_children(&mut c) {
            let bounds: Vec<Path> = match d.child_by_field_name("type").map(|t| self.type_expr(t)) {
                Some(TypeExpr::Named { path, .. }) => vec![path],
                _ => Vec::new(),
            };
            let mut c2 = d.walk();
            for nm in d.children_by_field_name("name", &mut c2) {
                out.push(Generic {
                    name: self.text(nm).into(),
                    bounds: bounds.clone(),
                    sig: None,
                });
            }
        }
        out
    }

    /// Declared parameter types, one per name (`a, b int` is two).
    fn param_types(&self, ps: Option<Node>) -> Vec<TypeExpr> {
        let mut out = Vec::new();
        let Some(ps) = ps else { return out };
        let mut c = ps.walk();
        for p in ps.named_children(&mut c) {
            let Some(t) = p.child_by_field_name("type") else {
                continue;
            };
            let ty = self.type_expr(t);
            let ty = if p.kind() == "variadic_parameter_declaration" {
                TypeExpr::Slice(Box::new(ty))
            } else {
                ty
            };
            let mut c2 = p.walk();
            let n = p.children_by_field_name("name", &mut c2).count().max(1);
            out.extend(std::iter::repeat_n(ty, n));
        }
        out
    }

    /// A result list: one type, or a tuple for `(A, B)`.
    fn result(&self, r: Option<Node>) -> Option<TypeExpr> {
        let r = r?;
        if r.kind() != "parameter_list" {
            return Some(self.type_expr(r));
        }
        let tys = self.param_types(Some(r));
        match tys.len() {
            0 => None,
            1 => tys.into_iter().next(),
            _ => Some(TypeExpr::Tuple(tys)),
        }
    }

    fn function(&mut self, n: Node<'a>) {
        let Some(name) = n.child_by_field_name("name") else {
            return;
        };
        let func = self.f.fns.len() as u32;
        let (start, end) = (n.start_byte() as u32, n.end_byte() as u32);
        let generics = self.generics(n.child_by_field_name("type_parameters"));
        // a method: an impl block of its own for the receiver type
        let mut owner = Owner::Free;
        let mut has_self = false;
        if let Some(recv) = n.child_by_field_name("receiver") {
            let decl = recv.named_child(0);
            if let Some(t) = decl.and_then(|d| d.child_by_field_name("type")) {
                let self_ty = strip_ref(&self.type_expr(t))
                    .cloned()
                    .unwrap_or(TypeExpr::Unknown);
                let impl_generics = match &self_ty {
                    TypeExpr::Named { args, .. } => args
                        .iter()
                        .filter_map(|a| match a {
                            TypeExpr::Named { path, .. } if path.0.len() == 1 => Some(Generic {
                                name: path.0[0].clone(),
                                bounds: Vec::new(),
                                sig: None,
                            }),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let idx = self.f.impls.len() as u32;
                self.f.impls.push(ImplDecl {
                    scope: 0,
                    generics: impl_generics,
                    self_ty: self_ty.clone(),
                    trait_: None,
                    assoc_types: Vec::new(),
                });
                owner = Owner::Impl(idx);
                has_self = true;
                if let Some(rn) = decl.and_then(|d| d.child_by_field_name("name")) {
                    let rn = self.sym(self.text(rn));
                    let ty = self.type_idx(self_ty);
                    self.f.locals.push(Local {
                        func,
                        name: rn,
                        at: start,
                        until: end,
                        ty: Some(ty),
                        init: None,
                    });
                }
            }
        }
        let params = self.param_types(n.child_by_field_name("parameters"));
        self.bind_params(n.child_by_field_name("parameters"), func, start, end);
        self.bind_params(n.child_by_field_name("result"), func, start, end);
        let ret = self.result(n.child_by_field_name("result"));
        self.f.fns.push(FnDecl {
            name: self.text(name).into(),
            row: row(n),
            scope: 0,
            owner,
            generics,
            params,
            has_self,
            enclosing: None,
            ret,
        });
        for part in ["receiver", "parameters", "result"] {
            if let Some(p) = n.child_by_field_name(part) {
                self.type_mentions(p, Some(func), &[]);
            }
        }
        if let Some(body) = n.child_by_field_name("body") {
            self.body(body, Some(func));
        }
    }

    /// Named parameters (and named results) as typed locals.
    fn bind_params(&mut self, ps: Option<Node<'a>>, func: u32, at: u32, until: u32) {
        let Some(ps) = ps else { return };
        if ps.kind() != "parameter_list" {
            return;
        }
        let mut c = ps.walk();
        let params: Vec<Node> = ps.named_children(&mut c).collect();
        for p in params {
            let Some(t) = p.child_by_field_name("type") else {
                continue;
            };
            let ty = self.type_expr(t);
            let ty = if p.kind() == "variadic_parameter_declaration" {
                TypeExpr::Slice(Box::new(ty))
            } else {
                ty
            };
            let ty = self.type_idx(ty);
            let mut c2 = p.walk();
            let names: Vec<Node> = p.children_by_field_name("name", &mut c2).collect();
            for nm in names {
                let name = self.sym(self.text(nm));
                self.f.locals.push(Local {
                    func,
                    name,
                    at,
                    until,
                    ty: Some(ty),
                    init: None,
                });
            }
        }
    }

    /// Walk a function body (or a package-level initializer): locals and sites.
    fn body(&mut self, root: Node<'a>, func: Option<u32>) {
        // names resolve through the file scope (imports), then the package
        let scope = 0;
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            match (n.kind(), func) {
                ("short_var_declaration", Some(f)) => {
                    self.bind_list(
                        n.child_by_field_name("left"),
                        n.child_by_field_name("right"),
                        None,
                        n,
                        f,
                    );
                }
                ("var_spec", Some(f)) => {
                    let ty = n.child_by_field_name("type").map(|t| {
                        let te = self.type_expr(t);
                        self.type_idx(te)
                    });
                    let mut c = n.walk();
                    let names: Vec<Node> = n.children_by_field_name("name", &mut c).collect();
                    let values = n.child_by_field_name("value");
                    self.bind_names(&names, values, ty, n, f);
                }
                ("range_clause", Some(f)) => {
                    if let (Some(left), Some(right)) = (
                        n.child_by_field_name("left"),
                        n.child_by_field_name("right"),
                    ) {
                        let mut c = left.walk();
                        let names: Vec<Node> = left.named_children(&mut c).collect();
                        let over = self.expr(right);
                        let until = n.parent().map_or(n.end_byte(), |p| p.end_byte()) as u32;
                        // `for i, v := range xs`: v is an element
                        if let Some(v) = names.get(1) {
                            let init = self.node(Expr::Elem(over));
                            self.local(*v, None, Some(init), n.end_byte() as u32, until, f);
                        }
                        if let Some(k) = names.first() {
                            self.local(*k, None, None, n.end_byte() as u32, until, f);
                        }
                    }
                }
                ("type_switch_statement", Some(f)) => {
                    // `switch v := x.(type) { case T: .. }`: v is a T per case
                    if let Some(alias) = n
                        .child_by_field_name("alias")
                        .and_then(|a| a.named_child(0))
                    {
                        let mut c = n.walk();
                        let cases: Vec<Node> = n
                            .named_children(&mut c)
                            .filter(|k| k.kind() == "type_case")
                            .collect();
                        for case in cases {
                            let mut c2 = case.walk();
                            let types: Vec<Node> =
                                case.children_by_field_name("type", &mut c2).collect();
                            let ty = match types.as_slice() {
                                [t] => {
                                    let te = self.type_expr(*t);
                                    Some(self.type_idx(te))
                                }
                                _ => None,
                            };
                            self.local(
                                alias,
                                ty,
                                None,
                                case.start_byte() as u32,
                                case.end_byte() as u32,
                                f,
                            );
                        }
                    }
                }
                ("func_literal", Some(f)) => {
                    let (at, until) = (n.start_byte() as u32, n.end_byte() as u32);
                    self.bind_params(n.child_by_field_name("parameters"), f, at, until);
                }
                ("call_expression", _) => self.call_site(n, func, scope),
                (
                    "qualified_type" | "type_identifier" | "identifier" | "selector_expression",
                    _,
                ) if !self.callees.contains(&n.id()) && !binds(n) => {
                    self.mention(n, func);
                    if n.kind() == "qualified_type" {
                        continue;
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

    /// `a, b := x, y` / `a, err := f()`.
    fn bind_list(
        &mut self,
        left: Option<Node<'a>>,
        right: Option<Node<'a>>,
        ty: Option<u32>,
        stmt: Node<'a>,
        func: u32,
    ) {
        let Some(left) = left else { return };
        let mut c = left.walk();
        let names: Vec<Node> = left.named_children(&mut c).collect();
        self.bind_names(&names, right, ty, stmt, func);
    }

    fn bind_names(
        &mut self,
        names: &[Node<'a>],
        values: Option<Node<'a>>,
        ty: Option<u32>,
        stmt: Node<'a>,
        func: u32,
    ) {
        let vals: Vec<Node> = match values {
            Some(v) if v.kind() == "expression_list" => {
                let mut c = v.walk();
                v.named_children(&mut c).collect()
            }
            Some(v) => vec![v],
            None => Vec::new(),
        };
        let (at, until) = (stmt.end_byte() as u32, scope_end(stmt));
        for (i, nm) in names.iter().enumerate() {
            let init = if vals.len() == names.len() {
                Some(self.expr(vals[i]))
            } else if vals.len() == 1 {
                // a multi-value call: element i of its result tuple
                let whole = self.expr(vals[0]);
                let (empty, idx) = (
                    self.sym(""),
                    self.sym(POSITIONS.get(i).copied().unwrap_or("")),
                );
                Some(self.node(Expr::Payload(whole, empty, idx)))
            } else {
                None
            };
            self.local(*nm, ty, init, at, until, func);
        }
    }

    fn local(
        &mut self,
        n: Node<'a>,
        ty: Option<u32>,
        init: Option<ExprId>,
        at: u32,
        until: u32,
        func: u32,
    ) {
        if n.kind() != "identifier" {
            return;
        }
        let t = self.text(n);
        if t == "_" {
            return;
        }
        let name = self.sym(t);
        self.f.locals.push(Local {
            func,
            name,
            at,
            until,
            ty,
            init,
        });
    }

    fn call_site(&mut self, n: Node<'a>, func: Option<u32>, scope: u32) {
        let expr = self.expr(n);
        let name_node = match self.f.expr(expr) {
            Expr::Method(..) => n
                .child_by_field_name("function")
                .and_then(|c| c.child_by_field_name("field")),
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
            if matches!(callee.kind(), "generic_type" | "index_expression") {
                if let Some(inner) = callee.named_child(0) {
                    self.callees.insert(inner.id()); // `f[T](..)`
                }
            }
        }
    }

    /// A name mentioned other than as a callee: a type, a value, a function
    /// used as a value, `pkg.Name`.
    fn mention(&mut self, n: Node<'a>, func: Option<u32>) {
        let (expr, name) = match n.kind() {
            "qualified_type" => match (
                n.child_by_field_name("package"),
                n.child_by_field_name("name"),
            ) {
                (Some(p), Some(t)) => (self.path_expr(&[self.text(p), self.text(t)]), t),
                _ => return,
            },
            "selector_expression" => match n.child_by_field_name("field") {
                Some(f) => (self.expr(n), f),
                None => return,
            },
            _ => (self.path_expr(&[self.text(n)]), n),
        };
        self.f.sites.push(Site {
            func,
            scope: 0,
            row: row(name),
            at: name.start_byte() as u32,
            kind: SiteKind::Ref,
            expr,
        });
    }

    /// The types a declaration's signature mentions, except `generics`.
    fn type_mentions(&mut self, n: Node<'a>, func: Option<u32>, generics: &[Generic]) {
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            match x.kind() {
                "type_identifier" if generics.iter().any(|g| *g.name == *self.text(x)) => {}
                "type_identifier" | "qualified_type" => self.mention(x, func),
                _ => {
                    let mut c = x.walk();
                    stack.extend(x.named_children(&mut c));
                }
            }
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
            "selector_expression" => match (child("operand"), child("field")) {
                (Some(o), Some(f)) => {
                    let (o, f) = (self.expr(o), self.sym(self.text(f)));
                    self.node(Expr::Field(o, f))
                }
                _ => ExprId::UNKNOWN,
            },
            "call_expression" => {
                let Some(callee) = child("function") else {
                    return ExprId::UNKNOWN;
                };
                let callee =
                    if callee.kind() == "generic_type" || callee.kind() == "index_expression" {
                        callee.named_child(0).unwrap_or(callee)
                    } else {
                        callee
                    };
                match callee.kind() {
                    "selector_expression" => match (
                        callee.child_by_field_name("operand"),
                        callee.child_by_field_name("field"),
                    ) {
                        (Some(o), Some(f)) => {
                            let (o, f) = (self.expr(o), self.sym(self.text(f)));
                            self.node(Expr::Method(o, f))
                        }
                        _ => ExprId::UNKNOWN,
                    },
                    "identifier" => {
                        let name = self.text(callee);
                        match name {
                            // `new(T)` / `make(T, ..)` / conversions keep T
                            "new" | "make" => {
                                let t = child("arguments").and_then(|a| a.named_child(0));
                                match t {
                                    Some(t) => {
                                        let te = self.type_expr(t);
                                        self.typed(te)
                                    }
                                    None => ExprId::UNKNOWN,
                                }
                            }
                            // `append(xs, ..)` has the slice's type
                            "append" => child("arguments")
                                .and_then(|a| a.named_child(0))
                                .map(|a| self.expr(a))
                                .unwrap_or(ExprId::UNKNOWN),
                            _ => {
                                let p = self.path_expr(&[name]);
                                self.node(Expr::Call(p))
                            }
                        }
                    }
                    "parenthesized_expression" => {
                        // a conversion `(*T)(x)` or a call of a func value
                        ExprId::UNKNOWN
                    }
                    _ => ExprId::UNKNOWN,
                }
            }
            "composite_literal" => match child("type") {
                Some(t) => {
                    let te = self.type_expr(t);
                    self.typed(te)
                }
                None => ExprId::UNKNOWN,
            },
            "unary_expression" => child("operand")
                .map(|o| self.expr(o))
                .unwrap_or(ExprId::UNKNOWN),
            "parenthesized_expression" => n
                .named_child(0)
                .map(|e| self.expr(e))
                .unwrap_or(ExprId::UNKNOWN),
            "type_assertion_expression" | "type_conversion_expression" => match child("type") {
                Some(t) => {
                    let te = self.type_expr(t);
                    self.typed(te)
                }
                None => ExprId::UNKNOWN,
            },
            "index_expression" => match child("operand") {
                Some(o) => {
                    let o = self.expr(o);
                    self.node(Expr::Elem(o))
                }
                None => ExprId::UNKNOWN,
            },
            "slice_expression" => child("operand")
                .map(|o| self.expr(o))
                .unwrap_or(ExprId::UNKNOWN),
            "interpreted_string_literal" | "raw_string_literal" => self.typed(named("string")),
            _ => ExprId::UNKNOWN,
        }
    }

    fn type_expr(&self, n: Node) -> TypeExpr {
        match n.kind() {
            "type_identifier" | "identifier" | "package_identifier" => named(self.text(n)),
            "qualified_type" => match (
                n.child_by_field_name("package"),
                n.child_by_field_name("name"),
            ) {
                (Some(p), Some(t)) => TypeExpr::Named {
                    path: Path(vec![self.text(p).into(), self.text(t).into()]),
                    args: Vec::new(),
                },
                _ => TypeExpr::Unknown,
            },
            "generic_type" => {
                let base = n.child_by_field_name("type").map(|t| self.type_expr(t));
                let mut args = Vec::new();
                if let Some(ta) = n.child_by_field_name("type_arguments") {
                    let mut c = ta.walk();
                    for a in ta.named_children(&mut c) {
                        args.push(
                            self.type_expr(
                                a.named_child(0)
                                    .filter(|_| a.kind() == "type_elem")
                                    .unwrap_or(a),
                            ),
                        );
                    }
                }
                match base {
                    Some(TypeExpr::Named { path, .. }) => TypeExpr::Named { path, args },
                    _ => TypeExpr::Unknown,
                }
            }
            "pointer_type" => n
                .named_child(0)
                .map(|t| TypeExpr::Ref(Box::new(self.type_expr(t))))
                .unwrap_or(TypeExpr::Unknown),
            "slice_type" | "array_type" | "implicit_length_array_type" => n
                .child_by_field_name("element")
                .map(|t| TypeExpr::Slice(Box::new(self.type_expr(t))))
                .unwrap_or(TypeExpr::Unknown),
            "map_type" => match (n.child_by_field_name("key"), n.child_by_field_name("value")) {
                (Some(k), Some(v)) => TypeExpr::Named {
                    path: Path::single("map"),
                    args: vec![self.type_expr(k), self.type_expr(v)],
                },
                _ => TypeExpr::Unknown,
            },
            "channel_type" => n
                .child_by_field_name("value")
                .map(|t| TypeExpr::Named {
                    path: Path::single("chan"),
                    args: vec![self.type_expr(t)],
                })
                .unwrap_or(TypeExpr::Unknown),
            "function_type" => {
                let params = self.param_types(n.child_by_field_name("parameters"));
                let ret = self.result(n.child_by_field_name("result")).map(Box::new);
                TypeExpr::Fn(params, ret)
            }
            "parenthesized_type" => n
                .named_child(0)
                .map(|t| self.type_expr(t))
                .unwrap_or(TypeExpr::Unknown),
            "interface_type" => TypeExpr::Traits(Vec::new()),
            _ => TypeExpr::Unknown,
        }
    }
}

/// An identifier being declared or bound (`x :=`, parameters, `var x`,
/// struct-literal keys), not a use.
fn binds(n: Node) -> bool {
    let Some(mut p) = n.parent() else {
        return false;
    };
    let mut child = n;
    if p.kind() == "expression_list" {
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
        "short_var_declaration" | "range_clause" => is("left"),
        "type_switch_statement" => is("alias"),
        "var_spec"
        | "const_spec"
        | "parameter_declaration"
        | "variadic_parameter_declaration"
        | "type_parameter_declaration"
        | "type_spec"
        | "type_alias" => is("name"),
        "literal_element" => p
            .parent()
            .is_some_and(|k| k.kind() == "keyed_element" && k.named_child(0) == Some(p)),
        _ => false,
    }
}

const POSITIONS: [&str; 8] = ["0", "1", "2", "3", "4", "5", "6", "7"];

fn named(name: &str) -> TypeExpr {
    TypeExpr::Named {
        path: Path::single(name),
        args: Vec::new(),
    }
}

fn strip_ref(t: &TypeExpr) -> Option<&TypeExpr> {
    match t {
        TypeExpr::Ref(inner) => strip_ref(inner),
        TypeExpr::Named { .. } => Some(t),
        _ => None,
    }
}

/// A declaration is visible until the end of its enclosing block.
fn scope_end(n: Node) -> u32 {
    let mut cur = n.parent();
    while let Some(p) = cur {
        if matches!(
            p.kind(),
            "block"
                | "for_statement"
                | "if_statement"
                | "expression_case"
                | "type_case"
                | "communication_case"
                | "default_case"
        ) {
            return p.end_byte() as u32;
        }
        cur = p.parent();
    }
    u32::MAX
}

// ---------------------------------------------------------------------------
// Layout: a package per directory, imported by `go.mod` module path
// ---------------------------------------------------------------------------

fn layout(root: &FsPath, files: &[(&str, &FileFacts)]) -> Layout {
    let orphans: Vec<(usize, &str)> = files
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (i, *p))
        .collect();
    let mut layout = Layout {
        parent_of: vec![None; files.len()],
        pooled: vec![true; files.len()],
        ..Layout::default()
    };
    layout.add_directory_modules(&orphans, files.len());
    // directory index -> its path
    let mut paths: Vec<String> = Vec::with_capacity(layout.dirs.len());
    for (name, parent) in &layout.dirs {
        let p = match parent {
            Some(pi) if !paths[*pi].is_empty() => format!("{}/{}", paths[*pi], name),
            _ => name.clone(),
        };
        paths.push(p);
    }
    // every go.mod's module path prefixes the packages below it
    let mut modules: Vec<(String, String)> = Vec::new(); // (dir, module path)
    let mut seen: HashMap<String, ()> = HashMap::default();
    for dir in &paths {
        let mut d = dir.clone();
        loop {
            if seen.insert(d.clone(), ()).is_none() {
                if let Ok(text) = std::fs::read_to_string(root.join(&d).join("go.mod")) {
                    if let Some(m) = text.lines().find_map(|l| l.trim().strip_prefix("module ")) {
                        modules.push((d.clone(), m.trim().trim_matches('"').to_string()));
                    }
                }
            }
            if d.is_empty() {
                break;
            }
            d = d
                .rsplit_once('/')
                .map_or(String::new(), |(p, _)| p.to_string());
        }
    }
    for (i, dir) in paths.iter().enumerate() {
        // the nearest enclosing module
        let best = modules
            .iter()
            .filter(|(m, _)| m.is_empty() || dir == m || dir.starts_with(&format!("{m}/")))
            .max_by_key(|(m, _)| m.len());
        if let Some((mdir, mpath)) = best {
            let rel = dir
                .strip_prefix(mdir.as_str())
                .unwrap_or(dir)
                .trim_start_matches('/');
            let import = if rel.is_empty() {
                mpath.clone()
            } else {
                format!("{mpath}/{rel}")
            };
            layout.dir_crates.insert(import, i);
        }
    }
    layout
}
