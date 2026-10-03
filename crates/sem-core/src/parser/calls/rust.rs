//! Rust front end: lowers a tree-sitter-rust tree into [`FileFacts`], plus
//! the Rust-specific *data* the language-neutral stages consult
//! ([`RUST`]: which roots are external, how std methods transform types,
//! which types auto-deref) and the crate/module layout rule.

use std::path::Path as FsPath;

use rustc_hash::FxHashMap as HashMap;
use tree_sitter::Node;

use super::infer::TUPLE;
use super::ir::*;
use super::lang::{BuiltinRet, ClosureArg, Lang, Layout};

pub struct Rust;

pub static RUST: Rust = Rust;

impl Lang for Rust {
    fn lower(&self, tree: &tree_sitter::Tree, src: &str) -> FileFacts {
        lower(tree, src)
    }

    fn layout(&self, root: &FsPath, files: &[(&str, &FileFacts)]) -> Layout {
        layout(root, files)
    }

    fn builtin_method(&self, recv: &str, method: &str) -> Option<BuiltinRet> {
        builtin_method(recv, method)
    }

    fn builtin_field(&self, ty: &str, field: &str) -> Option<BuiltinRet> {
        match (ty, field) {
            ("Range" | "RangeInclusive", "start" | "end") => Some(BuiltinRet::Arg(0)),
            (TUPLE, "0") => Some(BuiltinRet::Arg(0)),
            (TUPLE, "1") => Some(BuiltinRet::Arg(1)),
            (TUPLE, "2") => Some(BuiltinRet::Arg(2)),
            _ => None,
        }
    }

    fn deref(&self, ty: &str) -> Option<BuiltinRet> {
        match ty {
            "Box" | "Rc" | "Arc" | "Ref" | "RefMut" | "MutexGuard" | "RwLockReadGuard"
            | "RwLockWriteGuard" | "ManuallyDrop" | "Pin" | "Cow" => Some(BuiltinRet::Arg(0)),
            "String" => Some(BuiltinRet::Named("str")),
            "Vec" => Some(BuiltinRet::Wrap("slice", 0)),
            _ => None,
        }
    }

    fn variant_payload(&self, ctor: &str, field: &str) -> Option<BuiltinRet> {
        match (ctor, field) {
            ("Some" | "Ok" | "Ready" | "Break", "0") => Some(BuiltinRet::Arg(0)),
            ("Err", "0") => Some(BuiltinRet::Arg(1)),
            _ => None,
        }
    }

    fn closure_param(&self, method: &str, pos: usize, k: usize) -> Option<ClosureArg> {
        closure_param(method, pos, k)
    }

    fn elem(&self, container: &str) -> Option<BuiltinRet> {
        match container {
            "Vec" | "slice" | "VecDeque" | "HashSet" | "BTreeSet" | "Option" | "IndexSet"
            | "SmallVec" | "array" | "Iter" | "Result" | "FxHashSet" => Some(BuiltinRet::Arg(0)),
            _ => None,
        }
    }

    fn iterates_as(&self) -> Option<(&'static str, &'static str, &'static str)> {
        Some(("Iterator", "Item", "Iter"))
    }

    fn self_value(&self) -> &'static str {
        "self"
    }

    fn self_type(&self) -> &'static str {
        "Self"
    }
}

/// How a std method call transforms its receiver's type (repo definitions
/// always win; this only applies when the method is not found in the repo).
fn builtin_method(recv: &str, method: &str) -> Option<BuiltinRet> {
    use BuiltinRet::*;
    let recv = match recv {
        "FxHashMap" | "BTreeMap" | "IndexMap" | "TreeMap" | "HashMap" => "HashMap",
        "FxHashSet" | "BTreeSet" | "IndexSet" | "TreeSet" | "HashSet" => "HashSet",
        "SmallVec" | "ArrayVec" | "VecDeque" | "Vec" => "Vec",
        other => other,
    };
    Some(match (recv, method) {
        (
            "Option" | "Result",
            "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default"
            | "unwrap_unchecked",
        ) => Arg(0),
        ("Result", "unwrap_err" | "expect_err") => Arg(1),
        (
            "Option" | "Result",
            "as_ref" | "as_mut" | "as_deref" | "as_deref_mut" | "take" | "cloned" | "copied" | "or"
            | "or_else" | "filter" | "replace",
        ) => Same,
        ("Result", "ok") => Wrap("Option", 0),
        ("Option", "ok_or" | "ok_or_else") => Wrap("Result", 0),
        (
            "Vec" | "slice" | "VecDeque" | "array",
            "first" | "last" | "get" | "get_mut" | "first_mut" | "last_mut" | "pop" | "front"
            | "back" | "front_mut" | "back_mut" | "pop_front" | "pop_back",
        ) => Wrap("Option", 0),
        ("HashMap" | "BTreeMap" | "IndexMap", "get" | "get_mut" | "remove") => Wrap("Option", 1),
        ("HashMap" | "BTreeMap" | "IndexMap", "entry") => WrapAll("Entry"),
        ("Entry", "or_default" | "or_insert" | "or_insert_with" | "or_insert_with_key") => Arg(1),
        ("Mutex" | "RwLock", "lock" | "read" | "write") => Arg(0),
        ("RefCell", "borrow" | "borrow_mut") => Arg(0),
        ("Cell", "get") => Arg(0),
        (
            "Vec" | "slice" | "VecDeque" | "HashSet" | "BTreeSet" | "Option" | "array" | "SmallVec"
            | "IndexSet",
            "iter" | "iter_mut" | "into_iter" | "drain",
        ) => Wrap("Iter", 0),
        ("HashMap" | "BTreeMap" | "IndexMap", "values" | "values_mut" | "into_values") => {
            Wrap("Iter", 1)
        }
        ("HashMap" | "BTreeMap" | "IndexMap", "keys" | "into_keys") => Wrap("Iter", 0),
        (
            "Iter",
            "peekable" | "rev" | "skip" | "take" | "filter" | "chain" | "cloned" | "copied"
            | "fuse" | "step_by" | "skip_while" | "take_while" | "inspect" | "into_iter" | "iter",
        ) => Same,
        (
            "Iter",
            "next" | "peek" | "last" | "nth" | "find" | "min" | "max" | "next_back" | "min_by_key"
            | "max_by_key" | "min_by" | "max_by" | "peek_mut" | "nth_back",
        ) => Wrap("Option", 0),
        (_, "clone" | "to_owned" | "borrow" | "borrow_mut" | "by_ref") => Same,
        (_, "to_string") => Named("String"),
        (_, "deref" | "deref_mut" | "as_ref" | "as_mut") => Arg(0),
        _ => return None,
    })
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
        typed: HashMap::default(),
        type_slots: HashMap::default(),
        closure_params: HashMap::default(),
        callees: Default::default(),
    };
    cx.f.exprs.push(Expr::Unknown); // ExprId::UNKNOWN
    cx.f.scopes.push(ScopeDecl {
        name: None,
        parent: None,
        out_of_line: false,
    });
    cx.items(tree.root_node(), 0, Owner::Free);
    let mut f = cx.f;
    f.locals.sort_by_key(|l| (l.func, l.at));
    f.turbofish.sort_by_key(|(e, _)| e.0);
    f.exprs.shrink_to_fit();
    f.syms.shrink_to_fit();
    f.path_pool.shrink_to_fit();
    f.expr_pool.shrink_to_fit();
    f.sites.shrink_to_fit();
    f.locals.shrink_to_fit();
    f
}

struct Lower<'a> {
    src: &'a str,
    f: FileFacts,
    interner: HashMap<&'a str, Sym>,
    /// Lowered expressions by syntax node id, so a call chain's sites (and
    /// a `let`'s initializer and its uses) share one arena node.
    memo: HashMap<usize, ExprId>,
    /// `Expr::Typed` nodes by type name, for literals.
    typed: HashMap<&'static str, ExprId>,
    /// `type_pool` slots by the type's source text (`&str`, `&mut Self`, ..).
    type_slots: HashMap<&'a str, u32>,
    /// Closure arguments, by node id: the call they are passed to and their
    /// argument position (their parameters are typed from the callee).
    closure_params: HashMap<usize, (ExprId, u8)>,
    /// Callee paths of call sites (by node id): not mentions of their own.
    callees: rustc_hash::FxHashSet<usize>,
}

struct TokenChain<'a> {
    expr: ExprId,
    /// Each call in the chain with the token naming the callee.
    calls: Vec<(ExprId, Node<'a>)>,
    /// Argument trees, to be scanned for nested calls.
    args: Vec<Node<'a>>,
    end: usize,
}

/// Positional field names (`0`, `1`, ..) for tuple patterns and fields.
const POSITIONS: [&str; 12] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"];

fn row(n: Node) -> u32 {
    n.start_position().row as u32
}

impl<'a> Lower<'a> {
    fn text(&self, n: Node) -> &'a str {
        self.src.get(n.start_byte()..n.end_byte()).unwrap_or("")
    }

    fn name(&self, n: Node, field: &str) -> Option<Name> {
        n.child_by_field_name(field).map(|c| self.text(c).into())
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

    fn path_span(&mut self, segs: &[&'a str]) -> Span {
        let start = self.f.path_pool.len() as u32;
        for s in segs {
            let id = self.sym(s);
            self.f.path_pool.push(id);
        }
        Span {
            start,
            len: segs.len() as u32,
        }
    }

    fn list(&mut self, items: &[ExprId]) -> Span {
        let start = self.f.expr_pool.len() as u32;
        self.f.expr_pool.extend_from_slice(items);
        Span {
            start,
            len: items.len() as u32,
        }
    }

    fn typed_named(&mut self, name: &'static str, reference: bool) -> ExprId {
        if let Some(&e) = self.typed.get(name) {
            return e;
        }
        let t = if reference {
            TypeExpr::Ref(Box::new(named(name)))
        } else {
            named(name)
        };
        let ti = self.type_idx(t);
        let e = self.node(Expr::Typed(ti));
        self.typed.insert(name, e);
        e
    }

    fn type_idx(&mut self, t: TypeExpr) -> u32 {
        self.f.type_pool.push(t);
        self.f.type_pool.len() as u32 - 1
    }

    /// The `type_pool` slot for a written type, shared by identical text.
    fn type_slot(&mut self, n: Node<'a>) -> u32 {
        let text = self.text(n);
        if let Some(&i) = self.type_slots.get(text) {
            return i;
        }
        let te = self.type_expr(n);
        let i = self.type_idx(te);
        self.type_slots.insert(text, i);
        i
    }

    fn items(&mut self, container: Node<'a>, scope: u32, owner: Owner) {
        let mut cursor = container.walk();
        let children: Vec<Node> = container.named_children(&mut cursor).collect();
        for child in children {
            self.item(child, scope, owner);
        }
    }

    /// Lower one item; returns false when `n` is not an item.
    fn item(&mut self, n: Node<'a>, scope: u32, owner: Owner) -> bool {
        if owner == Owner::Free
            && matches!(
                n.kind(),
                "function_item"
                    | "struct_item"
                    | "union_item"
                    | "enum_item"
                    | "trait_item"
                    | "type_item"
                    | "const_item"
                    | "static_item"
                    | "mod_item"
            )
            && !is_pub(n)
        {
            if let Some(name) = self.name(n, "name") {
                self.f.private.push((scope, name));
            }
        }
        match n.kind() {
            "function_item" | "function_signature_item" => self.function(n, scope, owner),
            "impl_item" => {
                let generics = self.generics(n);
                self.signature_mentions(n, None, scope, &generics);
                let self_ty = n
                    .child_by_field_name("type")
                    .map(|t| self.type_expr(t))
                    .unwrap_or(TypeExpr::Unknown);
                let trait_ = n.child_by_field_name("trait").and_then(|t| self.path(t));
                let mut assoc_types = Vec::new();
                if let Some(body) = n.child_by_field_name("body") {
                    let mut c = body.walk();
                    for it in body.named_children(&mut c) {
                        if it.kind() == "type_item" {
                            if let (Some(name), Some(t)) =
                                (self.name(it, "name"), it.child_by_field_name("type"))
                            {
                                assoc_types.push((name, self.type_expr(t)));
                            }
                        }
                    }
                }
                let idx = self.f.impls.len() as u32;
                self.f.impls.push(ImplDecl {
                    scope,
                    generics,
                    self_ty,
                    trait_,
                    assoc_types,
                });
                if let Some(body) = n.child_by_field_name("body") {
                    self.items(body, scope, Owner::Impl(idx));
                }
            }
            "trait_item" => {
                let Some(name) = self.name(n, "name") else {
                    return true;
                };
                let supertraits = n
                    .child_by_field_name("bounds")
                    .map(|b| self.bounds(b))
                    .unwrap_or_default();
                let idx = self.f.traits.len() as u32;
                let generics = self.generics(n);
                self.signature_mentions(n, None, scope, &generics);
                let mut assoc = Vec::new();
                if let Some(body) = n.child_by_field_name("body") {
                    let mut c = body.walk();
                    for it in body.named_children(&mut c) {
                        if let ("associated_type", Some(name)) = (it.kind(), self.name(it, "name"))
                        {
                            let bounds = it.child_by_field_name("bounds");
                            assoc.push(Generic {
                                name,
                                bounds: bounds.map(|b| self.bounds(b)).unwrap_or_default(),
                                sig: None,
                            });
                        }
                    }
                }
                self.f.traits.push(TraitDecl {
                    name,
                    row: row(n),
                    scope,
                    generics,
                    supertraits,
                    assoc,
                });
                if let Some(body) = n.child_by_field_name("body") {
                    self.items(body, scope, Owner::Trait(idx));
                }
            }
            "struct_item" | "union_item" | "enum_item" => {
                let Some(name) = self.name(n, "name") else {
                    return true;
                };
                let mut fields = Vec::new();
                let mut variants = Vec::new();
                if let Some(body) = n.child_by_field_name("body") {
                    if n.kind() == "enum_item" {
                        let mut c = body.walk();
                        for v in body.named_children(&mut c) {
                            if v.kind() != "enum_variant" {
                                continue;
                            }
                            if let Some(vname) = self.name(v, "name") {
                                let payload = v
                                    .child_by_field_name("body")
                                    .map(|b| self.field_list(b))
                                    .unwrap_or_default();
                                variants.push((vname, payload));
                            }
                        }
                    } else {
                        fields = self.field_list(body);
                    }
                }
                let kind = if n.kind() == "enum_item" {
                    TypeKind::Enum
                } else {
                    TypeKind::Struct
                };
                let generics = self.generics(n);
                self.signature_mentions(n, None, scope, &generics);
                self.f.types.push(TypeDecl {
                    name,
                    row: row(n),
                    scope,
                    generics,
                    kind,
                    fields,
                    variants,
                    embeds: Vec::new(),
                    field_inits: Vec::new(),
                });
            }
            "type_item" => {
                if owner != Owner::Free {
                    return true; // associated type: recorded on the impl
                }
                if let (Some(name), Some(t)) = (self.name(n, "name"), n.child_by_field_name("type"))
                {
                    let generics = self.generics(n);
                    self.signature_mentions(t, None, scope, &generics);
                    let aliased = self.type_expr(t);
                    self.f.types.push(TypeDecl {
                        name,
                        row: row(n),
                        scope,
                        generics,
                        kind: TypeKind::Alias(aliased),
                        fields: Vec::new(),
                        variants: Vec::new(),
                        embeds: Vec::new(),
                        field_inits: Vec::new(),
                    });
                }
            }
            "mod_item" => {
                let Some(name) = self.name(n, "name") else {
                    return true;
                };
                let body = n.child_by_field_name("body");
                let idx = self.f.scopes.len() as u32;
                self.f.scopes.push(ScopeDecl {
                    name: Some(name),
                    parent: Some(scope),
                    out_of_line: body.is_none(),
                });
                if let Some(body) = body {
                    self.items(body, idx, Owner::Free);
                }
            }
            "use_declaration" => {
                if let Some(arg) = n.child_by_field_name("argument") {
                    self.use_tree(arg, Vec::new(), scope, is_pub(n));
                }
            }
            "extern_crate_declaration" => {
                if let Some(name) = self.name(n, "name") {
                    let alias = self.name(n, "alias").unwrap_or_else(|| name.clone());
                    self.f.uses.push(UseDecl {
                        scope,
                        path: Path(vec![name]),
                        name: Some(alias),
                        glob: false,
                        public: is_pub(n),
                    });
                }
            }
            "const_item" | "static_item" => {
                if let Some(t) = n.child_by_field_name("type") {
                    self.signature_mentions(t, None, scope, &[]);
                }
                if let (Some(name), Some(t), Owner::Free) =
                    (self.name(n, "name"), n.child_by_field_name("type"), owner)
                {
                    let ty = Some(self.type_expr(t));
                    self.f.values.push(ValueDecl {
                        name,
                        row: row(n),
                        scope,
                        ty,
                        init: None,
                    });
                }
                if let Some(v) = n.child_by_field_name("value") {
                    // the initializer is a function of its own: its
                    // closures bind locals (`LazyLock::new(|| ..)`)
                    let func = self.f.fns.len() as u32;
                    self.f.fns.push(FnDecl {
                        name: "".into(),
                        row: row(n),
                        scope,
                        owner: Owner::Free,
                        generics: Vec::new(),
                        params: Vec::new(),
                        has_self: false,
                        enclosing: None,
                        ret: None,
                    });
                    self.body(v, Some(func), scope);
                }
            }
            "foreign_mod_item" => {
                if let Some(body) = n.child_by_field_name("body") {
                    self.items(body, scope, Owner::Free);
                }
            }
            "declaration_list" => self.items(n, scope, owner),
            _ => return false,
        }
        true
    }

    /// Named (`{ a: A }`) or positional (`(A, B)` -> `0`, `1`) fields.
    fn field_list(&self, body: Node) -> Vec<(Name, TypeExpr)> {
        let mut out = Vec::new();
        let mut c = body.walk();
        let mut pos = 0;
        for fd in body.named_children(&mut c) {
            match fd.kind() {
                "field_declaration" => {
                    if let (Some(fname), Some(t)) =
                        (self.name(fd, "name"), fd.child_by_field_name("type"))
                    {
                        out.push((fname, self.type_expr(t)));
                    }
                }
                "attribute_item" | "visibility_modifier" | "line_comment" | "block_comment" => {}
                _ => {
                    out.push((pos.to_string().into(), self.type_expr(fd)));
                    pos += 1;
                }
            }
        }
        out
    }

    fn function(&mut self, n: Node<'a>, scope: u32, owner: Owner) {
        let Some(name) = self.name(n, "name") else {
            return;
        };
        let generics = self.generics(n);
        let func = self.f.fns.len() as u32;
        let (start, end) = (n.start_byte() as u32, n.end_byte() as u32);
        let mut param_types = Vec::new();
        let mut has_self = false;
        if let Some(ps) = n.child_by_field_name("parameters") {
            let mut c = ps.walk();
            let params: Vec<Node> = ps.named_children(&mut c).collect();
            for p in params {
                match p.kind() {
                    "self_parameter" => {
                        has_self = true;
                        let ty = match self.type_slots.get("Self") {
                            Some(&i) => i,
                            None => {
                                let i = self.type_idx(named("Self"));
                                self.type_slots.insert("Self", i);
                                i
                            }
                        };
                        let name = self.sym("self");
                        self.f.locals.push(Local {
                            func,
                            name,
                            at: start,
                            until: end,
                            ty: Some(ty),
                            init: None,
                        });
                    }
                    "parameter" => {
                        let ty = p.child_by_field_name("type").map(|t| self.type_slot(t));
                        param_types.push(match ty {
                            Some(t) => self.f.type_pool[t as usize].clone(),
                            None => TypeExpr::Unknown,
                        });
                        if let Some(pat) = p.child_by_field_name("pattern") {
                            self.bind(pat, ty, None, start, end, func);
                        }
                    }
                    _ => {}
                }
            }
        }
        let ret = n
            .child_by_field_name("return_type")
            .map(|t| self.type_expr(t));
        self.f.fns.push(FnDecl {
            name,
            row: row(n),
            scope,
            owner,
            generics,
            params: param_types,
            has_self,
            enclosing: None,
            ret,
        });
        self.signature_mentions(n, Some(func), scope, &[]);
        if let Some(body) = n.child_by_field_name("body") {
            let body_scope = self.f.scopes.len() as u32;
            self.f.scopes.push(ScopeDecl {
                name: None,
                parent: Some(scope),
                out_of_line: false,
            });
            self.body(body, Some(func), body_scope);
        }
    }

    /// Walk an expression region, collecting locals and call sites. Items
    /// nested in the region are lowered into `scope`.
    fn body(&mut self, root: Node<'a>, func: Option<u32>, scope: u32) {
        // one cursor walks the region depth-first; `visit` says whether to
        // descend (items and macro trees are handled whole)
        let mut cursor = root.walk();
        let mut descend = self.visit(root, root, func, scope);
        loop {
            if !(descend && cursor.goto_first_child()) {
                loop {
                    if cursor.goto_next_sibling() {
                        break;
                    }
                    if !cursor.goto_parent() {
                        return;
                    }
                }
            }
            let n = cursor.node();
            descend = n.is_named() && self.visit(n, root, func, scope);
        }
    }

    /// One node of [`Self::body`]'s walk; false to skip its children.
    fn visit(&mut self, n: Node<'a>, root: Node<'a>, func: Option<u32>, scope: u32) -> bool {
        {
            if n.id() != root.id() && self.item(n, scope, Owner::Free) {
                return false;
            }
            match n.kind() {
                "let_declaration" => {
                    if let (Some(pat), Some(f)) = (n.child_by_field_name("pattern"), func) {
                        let ty = n.child_by_field_name("type").map(|t| self.type_slot(t));
                        let init = n.child_by_field_name("value").map(|v| self.expr(v));
                        self.bind(pat, ty, init, n.end_byte() as u32, scope_end(n), f);
                    }
                }
                "closure_expression" => {
                    if let (Some(ps), Some(f)) = (n.child_by_field_name("parameters"), func) {
                        let passed_to = self.closure_params.remove(&n.id());
                        let mut c = ps.walk();
                        let params: Vec<Node> = ps.named_children(&mut c).collect();
                        for (k, p) in params.into_iter().enumerate() {
                            let init = passed_to
                                .map(|(call, pos)| self.node(Expr::Arg(call, pos, k as u8)));
                            let (pat, ty) = if p.kind() == "parameter" {
                                let ty = p.child_by_field_name("type").map(|t| self.type_slot(t));
                                (p.child_by_field_name("pattern"), ty)
                            } else {
                                (Some(p), None)
                            };
                            if let Some(pat) = pat {
                                let (at, until) = (n.start_byte() as u32, n.end_byte() as u32);
                                self.bind(pat, ty, init, at, until, f);
                            }
                        }
                    }
                }
                "for_expression" => {
                    if let (Some(pat), Some(v), Some(f)) = (
                        n.child_by_field_name("pattern"),
                        n.child_by_field_name("value"),
                        func,
                    ) {
                        let value = self.expr(v);
                        let init = self.node(Expr::Elem(value));
                        let at = n
                            .child_by_field_name("body")
                            .map_or(n.start_byte(), |b| b.start_byte());
                        self.bind(pat, None, Some(init), at as u32, n.end_byte() as u32, f);
                    }
                }
                "let_condition" => {
                    if let (Some(pat), Some(v), Some(f)) = (
                        n.child_by_field_name("pattern"),
                        n.child_by_field_name("value"),
                        func,
                    ) {
                        let init = self.expr(v);
                        let until = conditional_end(n);
                        self.bind(pat, None, Some(init), n.end_byte() as u32, until, f);
                    }
                }
                "match_expression" => {
                    if let (Some(v), Some(body), Some(f)) = (
                        n.child_by_field_name("value"),
                        n.child_by_field_name("body"),
                        func,
                    ) {
                        let scrutinee = self.expr(v);
                        let mut c = body.walk();
                        let arms: Vec<Node> = body.named_children(&mut c).collect();
                        for arm in arms {
                            if let Some(pat) = arm
                                .child_by_field_name("pattern")
                                .and_then(|mp| mp.named_child(0))
                            {
                                let (at, until) = (arm.start_byte() as u32, arm.end_byte() as u32);
                                self.bind(pat, None, Some(scrutinee), at, until, f);
                            }
                        }
                    }
                }
                "call_expression" => {
                    self.call_site(n, func, scope);
                }
                "identifier"
                | "scoped_identifier"
                | "type_identifier"
                | "scoped_type_identifier" => {
                    let binding = n.kind() == "identifier"
                        && binds(n)
                        && !self.text(n).starts_with(char::is_uppercase);
                    if !binding && !self.callees.contains(&n.id()) {
                        self.mention(n, func, scope);
                    }
                    return false;
                }
                "attribute_item" | "inner_attribute_item" | "lifetime" | "label" => return false,
                "macro_invocation" => {
                    if let Some(tt) = n
                        .named_children(&mut n.walk())
                        .find(|c| c.kind() == "token_tree")
                    {
                        self.macro_sites(tt, func, scope);
                    }
                    return false;
                }
                _ => {}
            }
        }
        true
    }

    fn call_site(&mut self, n: Node<'a>, func: Option<u32>, scope: u32) {
        let expr = self.expr(n);
        let name_node = match self.f.expr(expr) {
            Expr::Method(..) => n
                .child_by_field_name("function")
                .map(unwrap_generic)
                .and_then(|c| c.child_by_field_name("field")),
            Expr::Call(_) => n
                .child_by_field_name("function")
                .map(unwrap_generic)
                .map(|c| c.child_by_field_name("name").unwrap_or(c)),
            _ => return,
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
        if let Some(callee) = n.child_by_field_name("function").map(unwrap_generic) {
            self.callees.insert(callee.id());
        }
        // a closure argument's parameters are typed from the callee
        let Some(args) = n.child_by_field_name("arguments") else {
            return;
        };
        let mut c = args.walk();
        let arg_nodes: Vec<Node> = args.named_children(&mut c).collect();
        for (pos, a) in arg_nodes.iter().enumerate() {
            if a.kind() == "closure_expression" {
                self.closure_params.insert(a.id(), (expr, pos as u8));
            }
        }
    }

    /// A path mentioned other than as a callee: a type, a value, a
    /// function used as a value.
    fn mention(&mut self, n: Node<'a>, func: Option<u32>, scope: u32) {
        let Some(segs) = self.path_segs(n) else {
            return;
        };
        let span = self.path_span(&segs);
        let expr = self.node(Expr::Path(span));
        let name = n.child_by_field_name("name").unwrap_or(n);
        self.f.sites.push(Site {
            func,
            scope,
            row: row(name),
            at: name.start_byte() as u32,
            kind: SiteKind::Ref,
            expr,
        });
    }

    /// The types an item's signature mentions (its body is walked on its
    /// own), except its own generic parameters.
    fn signature_mentions(
        &mut self,
        item: Node<'a>,
        func: Option<u32>,
        scope: u32,
        generics: &[Generic],
    ) {
        let mut stack = vec![item];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "type_identifier" | "scoped_type_identifier" => {
                    if !generics.iter().any(|g| *g.name == *self.text(n)) {
                        self.mention(n, func, scope);
                    }
                }
                "block" | "declaration_list" | "attribute_item" => {}
                _ => {
                    let mut c = n.walk();
                    stack.extend(n.named_children(&mut c));
                }
            }
        }
    }

    /// Macro arguments are unparsed token trees. Recover the call chains in
    /// them (`a.b(..).c(..)`, `f(..)`, `T::f(..)`, `x?`) with a tiny postfix
    /// parser over the tokens — enough for `assert!(x.is_ok())`,
    /// `format!("{}", self.name())`, `vec![Foo::new()]`.
    fn macro_sites(&mut self, tt: Node<'a>, func: Option<u32>, scope: u32) {
        let mut c = tt.walk();
        let toks: Vec<Node> = tt.children(&mut c).collect();
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i];
            if t.kind() == "token_tree" {
                self.macro_sites(t, func, scope);
                i += 1;
                continue;
            }
            match self.token_chain(&toks, i) {
                Some(chain) => {
                    for (expr, name_node) in chain.calls {
                        self.f.sites.push(Site {
                            func,
                            scope,
                            row: row(name_node),
                            at: name_node.start_byte() as u32,
                            kind: SiteKind::Call,
                            expr,
                        });
                    }
                    for tree in chain.args {
                        self.macro_sites(tree, func, scope);
                    }
                    i = chain.end.max(i + 1);
                }
                None => i += 1,
            }
        }
    }

    /// Parse a postfix chain (`a::b(..).c(..)?.d`) starting at token `i`.
    fn token_chain(&mut self, toks: &[Node<'a>], i: usize) -> Option<TokenChain<'a>> {
        let is_paren =
            |n: Node| n.kind() == "token_tree" && n.child(0).is_some_and(|c| c.kind() == "(");
        let t = *toks.get(i)?;
        let starts_chain = is_ident_token(t, self.src)
            && !(i > 0 && matches!(toks[i - 1].kind(), "." | "::" | "$" | "'"));
        // `name!(..)` is a nested macro: its tree is recursed separately
        if !starts_chain || toks.get(i + 1).is_some_and(|n| n.kind() == "!") {
            return None;
        }
        let mut chain = TokenChain {
            expr: ExprId::UNKNOWN,
            calls: Vec::new(),
            args: Vec::new(),
            end: i + 1,
        };
        let mut segs: Vec<&'a str> = vec![self.text(t)];
        let mut last = t;
        let mut j = i + 1;
        while j + 1 < toks.len() && toks[j].kind() == "::" && is_ident_token(toks[j + 1], self.src)
        {
            segs.push(self.text(toks[j + 1]));
            last = toks[j + 1];
            j += 2;
        }
        let span = self.path_span(&segs);
        let mut expr = self.node(Expr::Path(span));
        let mut is_path = true;
        loop {
            if j < toks.len() && is_paren(toks[j]) {
                if is_path {
                    expr = self.node(Expr::Call(expr));
                    chain.calls.push((expr, last));
                    is_path = false;
                }
                chain.args.push(toks[j]);
                j += 1;
            } else if j + 1 < toks.len()
                && toks[j].kind() == "."
                && is_ident_token(toks[j + 1], self.src)
            {
                let name = self.text(toks[j + 1]);
                let name_node = toks[j + 1];
                is_path = false;
                if name == "await" {
                    j += 2;
                    continue;
                }
                let name = self.sym(name);
                if j + 2 < toks.len() && is_paren(toks[j + 2]) {
                    expr = self.node(Expr::Method(expr, name));
                    chain.calls.push((expr, name_node));
                    chain.args.push(toks[j + 2]);
                    j += 3;
                } else {
                    expr = self.node(Expr::Field(expr, name));
                    j += 2;
                }
            } else if j < toks.len() && toks[j].kind() == "?" {
                expr = self.node(Expr::Unwrap(expr));
                is_path = false;
                j += 1;
            } else {
                break;
            }
        }
        chain.expr = expr;
        chain.end = j;
        Some(chain)
    }

    /// The first element expression of a bracketed token tree (`vec![a, b]`).
    fn first_token_expr(&mut self, tt: Node<'a>) -> ExprId {
        let mut c = tt.walk();
        let toks: Vec<Node> = tt.children(&mut c).collect();
        self.token_chain(&toks, 1)
            .map(|c| c.expr)
            .unwrap_or(ExprId::UNKNOWN)
    }

    /// Bind the names a pattern introduces. Destructuring binds each name to
    /// the matching field of the value (`Some(x)` binds the payload); names
    /// with nothing to bind them to are untyped (they still shadow).
    fn bind(
        &mut self,
        pat: Node<'a>,
        ty: Option<u32>,
        init: Option<ExprId>,
        at: u32,
        until: u32,
        func: u32,
    ) {
        match pat.kind() {
            "identifier" => {
                let name = self.text(pat);
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    return; // unit struct / const pattern, not a binding
                }
                let name = self.sym(name);
                self.f.locals.push(Local {
                    func,
                    name,
                    at,
                    until,
                    ty,
                    init,
                });
            }
            "mut_pattern" | "ref_pattern" | "reference_pattern" => {
                if let Some(inner) =
                    pat.named_child((pat.named_child_count() as u32).saturating_sub(1))
                {
                    self.bind(inner, ty, init, at, until, func);
                }
            }
            "tuple_pattern" | "tuple_struct_pattern" | "struct_pattern" => {
                // destructuring: each sub-pattern binds a field of the value
                let init = init.or_else(|| ty.map(|t| self.node(Expr::Typed(t))));
                let ctor_node = pat.child_by_field_name("type");
                let ctor_name = ctor_node
                    .and_then(|t| self.path_segs(t))
                    .and_then(|segs| segs.last().copied())
                    .unwrap_or("");
                let ctor = self.sym(ctor_name);
                let mut c = pat.walk();
                let subs: Vec<Node> = pat
                    .named_children(&mut c)
                    .filter(|s| Some(s.id()) != ctor_node.map(|t| t.id()))
                    .collect();
                let mut pos = 0;
                for sub in subs {
                    let (field, sub_pat) = match sub.kind() {
                        "field_pattern" => {
                            let Some(fname) = sub.child_by_field_name("name") else {
                                continue;
                            };
                            (self.text(fname), sub.child_by_field_name("pattern"))
                        }
                        "remaining_field_pattern" | "line_comment" | "block_comment" => continue,
                        _ => {
                            let Some(p) = POSITIONS.get(pos) else {
                                continue;
                            };
                            pos += 1;
                            (*p, Some(sub))
                        }
                    };
                    let field = self.sym(field);
                    let payload = init.map(|e| self.node(Expr::Payload(e, ctor, field)));
                    match sub_pat {
                        Some(sp) => self.bind(sp, None, payload, at, until, func),
                        // shorthand `{ a }` binds `a`
                        None => self.f.locals.push(Local {
                            func,
                            name: field,
                            at,
                            until,
                            ty: None,
                            init: payload,
                        }),
                    }
                }
            }
            "captured_pattern" => {
                let mut c = pat.walk();
                let kids: Vec<Node> = pat.named_children(&mut c).collect();
                for k in kids {
                    self.bind(k, ty, init, at, until, func);
                }
            }
            "or_pattern" => {
                if let Some(first) = pat.named_child(0) {
                    self.bind(first, ty, init, at, until, func);
                }
            }
            _ => {
                let mut names = Vec::new();
                self.pattern_names(pat, &mut names);
                for name in names {
                    let name = self.sym(name);
                    self.f.locals.push(Local {
                        func,
                        name,
                        at,
                        until,
                        ty: None,
                        init: None,
                    });
                }
            }
        }
    }

    fn pattern_names(&self, pat: Node, out: &mut Vec<&'a str>) {
        let mut stack = vec![pat];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "identifier" => {
                    let t = self.text(n);
                    if !t.chars().next().is_some_and(|c| c.is_uppercase()) {
                        out.push(t);
                    }
                }
                "scoped_identifier" | "type_identifier" | "scoped_type_identifier" => {}
                "field_pattern" => match n.child_by_field_name("pattern") {
                    Some(p) => stack.push(p),
                    None => {
                        if let Some(nm) = n.child_by_field_name("name") {
                            out.push(self.text(nm));
                        }
                    }
                },
                "tuple_struct_pattern" | "struct_pattern" => {
                    let ty = n.child_by_field_name("type").map(|t| t.id());
                    let mut c = n.walk();
                    for k in n.named_children(&mut c) {
                        if Some(k.id()) != ty {
                            stack.push(k);
                        }
                    }
                }
                _ => {
                    let mut c = n.walk();
                    for k in n.named_children(&mut c) {
                        stack.push(k);
                    }
                }
            }
        }
    }

    /// Lower an expression (memoized per syntax node).
    fn expr(&mut self, n: Node<'a>) -> ExprId {
        if let Some(&e) = self.memo.get(&n.id()) {
            return e;
        }
        let e = self.expr_uncached(n);
        self.memo.insert(n.id(), e);
        e
    }

    fn expr_uncached(&mut self, n: Node<'a>) -> ExprId {
        let child = |field: &str| n.child_by_field_name(field);
        match n.kind() {
            "identifier" | "self" | "scoped_identifier" => match self.path_segs(n) {
                Some(segs) => {
                    let span = self.path_span(&segs);
                    self.node(Expr::Path(span))
                }
                None => ExprId::UNKNOWN,
            },
            "field_expression" => match (child("value"), child("field")) {
                (Some(v), Some(f)) => {
                    let (v, f) = (self.expr(v), self.sym(self.text(f)));
                    self.node(Expr::Field(v, f))
                }
                _ => ExprId::UNKNOWN,
            },
            "call_expression" => {
                let e = self.call_expr(n);
                let targs = child("function")
                    .filter(|f| f.kind() == "generic_function")
                    .and_then(|f| f.child_by_field_name("type_arguments"));
                if let (Some(targs), true) = (targs, e != ExprId::UNKNOWN) {
                    let mut c = targs.walk();
                    let kids: Vec<Node> = targs.named_children(&mut c).collect();
                    let tys: Vec<u32> = kids
                        .into_iter()
                        .filter(|a| !matches!(a.kind(), "lifetime" | "type_binding"))
                        .map(|a| self.type_slot(a))
                        .collect();
                    self.f.turbofish.push((e, tys));
                }
                e
            }
            "generic_function" => child("function")
                .map(|f| self.expr(f))
                .unwrap_or(ExprId::UNKNOWN),
            "try_expression" => match n.named_child(0) {
                Some(e) => {
                    let e = self.expr(e);
                    self.node(Expr::Unwrap(e))
                }
                None => ExprId::UNKNOWN,
            },
            "reference_expression" => child("value")
                .map(|e| self.expr(e))
                .unwrap_or(ExprId::UNKNOWN),
            "unary_expression" => match n.named_child(0) {
                Some(e) if n.child(0).is_some_and(|op| op.kind() == "*") => {
                    let e = self.expr(e);
                    self.node(Expr::Deref(e))
                }
                _ => ExprId::UNKNOWN,
            },
            "parenthesized_expression" | "await_expression" => n
                .named_child(0)
                .map(|e| self.expr(e))
                .unwrap_or(ExprId::UNKNOWN),
            "struct_expression" => match child("name").and_then(|t| self.path_segs(t)) {
                Some(segs) => {
                    let span = self.path_span(&segs);
                    self.node(Expr::Struct(span))
                }
                None => ExprId::UNKNOWN,
            },
            "type_cast_expression" => match child("type") {
                Some(t) => {
                    let ti = self.type_slot(t);
                    self.node(Expr::Typed(ti))
                }
                None => ExprId::UNKNOWN,
            },
            "string_literal" | "raw_string_literal" => self.typed_named("str", true),
            "index_expression" => match n.named_child(0) {
                Some(e) => {
                    let e = self.expr(e);
                    self.node(Expr::Elem(e))
                }
                None => ExprId::UNKNOWN,
            },
            "tuple_expression" => {
                let mut c = n.walk();
                let kids: Vec<Node> = n.named_children(&mut c).collect();
                let elems: Vec<ExprId> = kids.into_iter().map(|e| self.expr(e)).collect();
                let l = self.list(&elems);
                self.node(Expr::Tuple(l))
            }
            "block" => self
                .block_tail(n)
                .map(|t| self.expr(t))
                .unwrap_or(ExprId::UNKNOWN),
            "closure_expression" => match child("body") {
                Some(b) => {
                    let e = self.expr(b);
                    let body = self.node(Expr::At(b.start_byte() as u32, e));
                    self.node(Expr::Closure(body))
                }
                None => ExprId::UNKNOWN,
            },
            "match_expression" => {
                let mut arms = Vec::new();
                if let Some(body) = child("body") {
                    let mut c = body.walk();
                    let arm_nodes: Vec<Node> = body.named_children(&mut c).collect();
                    for arm in arm_nodes {
                        if let Some(v) = arm.child_by_field_name("value") {
                            let e = self.expr(v);
                            arms.push(self.node(Expr::At(v.start_byte() as u32, e)));
                        }
                    }
                }
                let l = self.list(&arms);
                self.node(Expr::Branches(l))
            }
            "if_expression" => {
                let mut arms = Vec::new();
                let alt = child("alternative").and_then(|a| a.named_child(0));
                for b in [child("consequence"), alt].into_iter().flatten() {
                    let e = self.expr(b);
                    arms.push(self.node(Expr::At(b.start_byte() as u32, e)));
                }
                let l = self.list(&arms);
                self.node(Expr::Branches(l))
            }
            "macro_invocation" => match child("macro").map(|m| self.text(m)) {
                Some("format") => self.typed_named("String", false),
                Some("vec") => {
                    let first = n
                        .named_children(&mut n.walk())
                        .find(|c| c.kind() == "token_tree")
                        .map(|tt| self.first_token_expr(tt))
                        .unwrap_or(ExprId::UNKNOWN);
                    let (v, args) = (self.sym("Vec"), self.list(&[first]));
                    self.node(Expr::Ext(v, args))
                }
                _ => ExprId::UNKNOWN,
            },
            _ => ExprId::UNKNOWN,
        }
    }

    /// A call: `Method` / `Call`, or for identity-like and wrapping std
    /// calls, an expression of their result type.
    fn call_expr(&mut self, n: Node<'a>) -> ExprId {
        let child = |field: &str| n.child_by_field_name(field);
        let Some(callee) = child("function").map(unwrap_generic) else {
            return ExprId::UNKNOWN;
        };
        if callee.kind() == "field_expression" {
            let (Some(v), Some(f)) = (
                callee.child_by_field_name("value"),
                callee.child_by_field_name("field"),
            ) else {
                return ExprId::UNKNOWN;
            };
            if f.kind() != "field_identifier" {
                return ExprId::UNKNOWN;
            }
            let (v, f) = (self.expr(v), self.sym(self.text(f)));
            return self.node(Expr::Method(v, f));
        }
        if !matches!(callee.kind(), "identifier" | "scoped_identifier" | "self") {
            return ExprId::UNKNOWN;
        }
        let Some(segs) = self.path_segs(callee) else {
            return ExprId::UNKNOWN;
        };
        let first_arg = |this: &mut Self| {
            child("arguments")
                .and_then(|a| a.named_child(0))
                .map(|a| this.expr(a))
                .unwrap_or(ExprId::UNKNOWN)
        };
        if is_identity_call(&segs) {
            // `mem::take(&mut x)`, `Arc::clone(&x)`: x's type
            return first_arg(self);
        }
        match wrapper_ctor(&segs) {
            // `Some(x)`, `Box::new(x)`: the wrapper of x's type
            Some(w) => {
                let arg = first_arg(self);
                let (w, args) = (self.sym(w), self.list(&[arg]));
                self.node(Expr::Ext(w, args))
            }
            None => {
                let span = self.path_span(&segs);
                let p = self.node(Expr::Path(span));
                self.node(Expr::Call(p))
            }
        }
    }

    /// The value-producing last expression of a block, if any.
    fn block_tail(&self, block: Node<'a>) -> Option<Node<'a>> {
        let last = block.named_child((block.named_child_count() as u32).checked_sub(1)?)?;
        match last.kind() {
            "expression_statement"
            | "let_declaration"
            | "line_comment"
            | "block_comment"
            | "empty_statement" => None,
            k if k.ends_with("_item") => None,
            _ => Some(last),
        }
    }

    /// Flatten a path-like node into its segments (generic args dropped;
    /// `<T as Trait>::f` becomes `Trait::f`).
    fn path_segs(&self, n: Node) -> Option<Vec<&'a str>> {
        let mut segs = Vec::new();
        self.push_path(n, &mut segs)?;
        Some(segs)
    }

    fn path(&self, n: Node) -> Option<Path> {
        self.path_segs(n)
            .map(|segs| Path(segs.into_iter().map(Into::into).collect()))
    }

    fn push_path(&self, n: Node, out: &mut Vec<&'a str>) -> Option<()> {
        match n.kind() {
            "identifier" | "type_identifier" | "self" | "super" | "crate" | "primitive_type"
            | "field_identifier" => {
                out.push(self.text(n));
                Some(())
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                if let Some(p) = n.child_by_field_name("path") {
                    self.push_path(p, out)?;
                }
                out.push(self.text(n.child_by_field_name("name")?));
                Some(())
            }
            "generic_type" | "generic_type_with_turbofish" => {
                self.push_path(n.child_by_field_name("type")?, out)
            }
            "bracketed_type" => {
                let inner = n.named_child(0)?;
                if inner.kind() == "qualified_type" {
                    self.push_path(inner.child_by_field_name("alias")?, out)
                } else {
                    self.push_path(inner, out)
                }
            }
            _ => None,
        }
    }

    fn type_expr(&self, n: Node) -> TypeExpr {
        match n.kind() {
            // `Vec<_>`: left to inference
            "type_identifier" if self.text(n) == "_" => TypeExpr::Unknown,
            "type_identifier" | "scoped_type_identifier" | "primitive_type" => match self.path(n) {
                Some(path) => TypeExpr::Named {
                    path,
                    args: Vec::new(),
                },
                None => TypeExpr::Unknown,
            },
            "generic_type" => {
                let Some(path) = n.child_by_field_name("type").and_then(|t| self.path(t)) else {
                    return TypeExpr::Unknown;
                };
                let mut args = Vec::new();
                if let Some(ta) = n.child_by_field_name("type_arguments") {
                    let mut c = ta.walk();
                    for a in ta.named_children(&mut c) {
                        if !matches!(a.kind(), "lifetime" | "type_binding" | "block") {
                            args.push(self.type_expr(a));
                        }
                    }
                }
                TypeExpr::Named { path, args }
            }
            "reference_type" | "pointer_type" => n
                .child_by_field_name("type")
                .map(|t| TypeExpr::Ref(Box::new(self.type_expr(t))))
                .unwrap_or(TypeExpr::Unknown),
            "dynamic_type" | "abstract_type" | "bounded_type" => {
                let bounds = if n.kind() == "bounded_type" {
                    Some(n)
                } else {
                    n.child_by_field_name("trait")
                };
                let mut paths = Vec::new();
                if let Some(b) = bounds {
                    if let Some(sig) = self.fn_bound(b) {
                        return sig;
                    }
                    self.collect_bounds(b, &mut paths);
                }
                TypeExpr::Traits(paths)
            }
            "function_type" => self.fn_sig(n),
            "tuple_type" => {
                let mut c = n.walk();
                TypeExpr::Tuple(
                    n.named_children(&mut c)
                        .map(|t| self.type_expr(t))
                        .collect(),
                )
            }
            "array_type" => n
                .child_by_field_name("element")
                .map(|t| TypeExpr::Slice(Box::new(self.type_expr(t))))
                .unwrap_or(TypeExpr::Unknown),
            _ => TypeExpr::Unknown,
        }
    }

    fn collect_bounds(&self, n: Node, out: &mut Vec<Path>) {
        match n.kind() {
            "bounded_type" | "trait_bounds" => {
                let mut c = n.walk();
                for k in n.named_children(&mut c) {
                    self.collect_bounds(k, out);
                }
            }
            "higher_ranked_trait_bound" => {
                if let Some(t) = n.child_by_field_name("type") {
                    self.collect_bounds(t, out);
                }
            }
            "abstract_type" | "dynamic_type" => {
                if let Some(t) = n.child_by_field_name("trait") {
                    self.collect_bounds(t, out);
                }
            }
            "lifetime" | "removed_trait_bound" | "function_type" => {}
            _ => {
                if let Some(p) = self.path(n) {
                    out.push(p);
                }
            }
        }
    }

    /// `Fn(A, B) -> R` (also `fn(A) -> R`) as a [`TypeExpr::Fn`].
    fn fn_sig(&self, n: Node) -> TypeExpr {
        let mut params = Vec::new();
        if let Some(ps) = n.child_by_field_name("parameters") {
            let mut c = ps.walk();
            for p in ps.named_children(&mut c) {
                let t = if p.kind() == "parameter" {
                    p.child_by_field_name("type")
                } else {
                    Some(p)
                };
                params.push(t.map(|t| self.type_expr(t)).unwrap_or(TypeExpr::Unknown));
            }
        }
        let ret = n
            .child_by_field_name("return_type")
            .map(|r| Box::new(self.type_expr(r)));
        TypeExpr::Fn(params, ret)
    }

    /// The callable signature among a bound list (`FnOnce(A) + Send`).
    fn fn_bound(&self, n: Node) -> Option<TypeExpr> {
        match n.kind() {
            "function_type" => Some(self.fn_sig(n)),
            "bounded_type" | "trait_bounds" => {
                let mut c = n.walk();
                let kids: Vec<Node> = n.named_children(&mut c).collect();
                kids.into_iter().find_map(|k| self.fn_bound(k))
            }
            "higher_ranked_trait_bound" => {
                n.child_by_field_name("type").and_then(|t| self.fn_bound(t))
            }
            "abstract_type" | "dynamic_type" => n
                .child_by_field_name("trait")
                .and_then(|t| self.fn_bound(t)),
            _ => None,
        }
    }

    fn bounds(&self, n: Node) -> Vec<Path> {
        let mut out = Vec::new();
        self.collect_bounds(n, &mut out);
        out
    }

    fn generics(&self, item: Node) -> Vec<Generic> {
        let mut out: Vec<Generic> = Vec::new();
        if let Some(tp) = item.child_by_field_name("type_parameters") {
            let mut c = tp.walk();
            for p in tp.named_children(&mut c) {
                let (name, bounds) = match p.kind() {
                    "type_identifier" => (Some(p), None),
                    "constrained_type_parameter" => (
                        p.child_by_field_name("left")
                            .filter(|l| l.kind() == "type_identifier"),
                        p.child_by_field_name("bounds"),
                    ),
                    "type_parameter" => (
                        p.child_by_field_name("name"),
                        p.child_by_field_name("bounds"),
                    ),
                    "optional_type_parameter" => match p.child_by_field_name("name") {
                        Some(n) if n.kind() == "constrained_type_parameter" => (
                            n.child_by_field_name("left"),
                            n.child_by_field_name("bounds"),
                        ),
                        n => (n, None),
                    },
                    _ => (None, None),
                };
                if let Some(name) = name {
                    out.push(Generic {
                        name: self.text(name).into(),
                        bounds: bounds.map(|b| self.bounds(b)).unwrap_or_default(),
                        sig: bounds.and_then(|b| self.fn_bound(b)),
                    });
                }
            }
        }
        let mut c = item.walk();
        let where_clause = item.children(&mut c).find(|k| k.kind() == "where_clause");
        if let Some(wc) = where_clause {
            let mut c = wc.walk();
            for pred in wc.named_children(&mut c) {
                let (Some(left), Some(b)) = (
                    pred.child_by_field_name("left"),
                    pred.child_by_field_name("bounds"),
                ) else {
                    continue;
                };
                if left.kind() != "type_identifier" {
                    continue;
                }
                let name = self.text(left);
                let bounds = self.bounds(b);
                let sig = self.fn_bound(b);
                match out.iter_mut().find(|g| &*g.name == name) {
                    Some(g) => {
                        g.bounds.extend(bounds);
                        g.sig = g.sig.take().or(sig);
                    }
                    None => out.push(Generic {
                        name: name.into(),
                        bounds,
                        sig,
                    }),
                }
            }
        }
        out
    }

    fn use_tree(&mut self, n: Node, prefix: Vec<Name>, scope: u32, public: bool) {
        let (path, name, glob) = match n.kind() {
            "identifier" | "crate" | "self" | "super" | "scoped_identifier" => {
                let Some(p) = self.path(n) else { return };
                (p.0, None, false)
            }
            "use_as_clause" => {
                let (Some(p), Some(alias)) = (
                    n.child_by_field_name("path").and_then(|p| self.path(p)),
                    n.child_by_field_name("alias"),
                ) else {
                    return;
                };
                let alias = self.text(alias);
                if alias == "_" {
                    return;
                }
                (p.0, Some(alias.into()), false)
            }
            "use_wildcard" => {
                let p = n.named_child(0).and_then(|p| self.path(p));
                (p.map(|p| p.0).unwrap_or_default(), None, true)
            }
            "scoped_use_list" => {
                let mut path = prefix;
                if let Some(p) = n.child_by_field_name("path").and_then(|p| self.path(p)) {
                    path.extend(p.0);
                }
                if let Some(list) = n.child_by_field_name("list") {
                    self.use_tree(list, path, scope, public);
                }
                return;
            }
            "use_list" => {
                let mut c = n.walk();
                let kids: Vec<Node> = n.named_children(&mut c).collect();
                for k in kids {
                    self.use_tree(k, prefix.clone(), scope, public);
                }
                return;
            }
            _ => return,
        };
        let mut full = prefix;
        full.extend(path);
        if full.last().is_some_and(|l| &**l == "self") {
            full.pop();
        }
        let name = match (glob, name) {
            (true, _) => None,
            (false, Some(alias)) => Some(alias),
            (false, None) => match full.last() {
                Some(last) => Some(last.clone()),
                None => return,
            },
        };
        self.f.uses.push(UseDecl {
            scope,
            path: Path(full),
            name,
            glob,
            public,
        });
    }
}

/// An identifier a pattern binds (`let x`, `|x|`, `Some(x)`), not one it
/// mentions (`Some`, a constant).
fn binds(n: Node) -> bool {
    let Some(p) = n.parent() else { return false };
    let is = |field: &str| {
        p.child_by_field_name(field)
            .is_some_and(|c| c.id() == n.id())
    };
    !is("type")
        && (p.kind().ends_with("_pattern") || p.kind() == "closure_parameters" || is("pattern"))
}

fn unwrap_generic(n: Node) -> Node {
    if n.kind() == "generic_function" {
        n.child_by_field_name("function").unwrap_or(n)
    } else {
        n
    }
}

/// Std calls whose result has their first argument's type.
fn is_identity_call(segs: &[&str]) -> bool {
    matches!(
        segs,
        [.., "mem", "replace" | "take"] | ["Arc" | "Rc" | "Clone", "clone"]
    )
}

/// Constructors that wrap their argument: `Some(x): Option<X>`.
fn wrapper_ctor(segs: &[&str]) -> Option<&'static str> {
    Some(match segs {
        ["Some"] => "Option",
        ["Ok"] => "Result",
        ["Box", "new" | "pin"] => "Box",
        ["Rc", "new"] => "Rc",
        ["Arc", "new"] => "Arc",
        ["RefCell", "new"] => "RefCell",
        ["Cell", "new"] => "Cell",
        ["Mutex", "new"] => "Mutex",
        ["RwLock", "new"] => "RwLock",
        _ => return None,
    })
}

/// Parameter `k` of a closure passed as argument `pos` to std method
/// `method` (e.g. `iter.map(|x| ..)`: `x` is an element).
fn closure_param(method: &str, pos: usize, k: usize) -> Option<ClosureArg> {
    Some(match (method, pos, k) {
        (
            "map"
            | "filter"
            | "for_each"
            | "any"
            | "all"
            | "find"
            | "position"
            | "filter_map"
            | "flat_map"
            | "take_while"
            | "skip_while"
            | "inspect"
            | "max_by_key"
            | "min_by_key"
            | "sort_by_key"
            | "sort_unstable_by_key"
            | "sort_by_cached_key"
            | "retain"
            | "find_map"
            | "partition"
            | "and_then"
            | "is_some_and"
            | "is_ok_and"
            | "is_none_or"
            | "map_while"
            | "dedup_by_key"
            | "try_for_each"
            | "rposition",
            0,
            0,
        ) => ClosureArg::Elem,
        (
            "sort_by" | "sort_unstable_by" | "max_by" | "min_by" | "dedup_by" | "is_sorted_by",
            0,
            0 | 1,
        ) => ClosureArg::Elem,
        ("fold" | "try_fold", 1, 1) => ClosureArg::Elem,
        ("map_or" | "map_or_else", 1, 0) => ClosureArg::Elem,
        ("map_err" | "or_else" | "unwrap_or_else" | "inspect_err", 0, 0) => {
            ClosureArg::Variant("Err")
        }
        _ => return None,
    })
}

/// Declared with any `pub` visibility.
fn is_pub(n: Node) -> bool {
    let mut c = n.walk();
    let found = n
        .children(&mut c)
        .any(|k| k.kind() == "visibility_modifier");
    found
}

/// Inside a token tree, keywords (`self`, `default`, `crate`, ..) are their
/// own token kinds; any word-shaped token can start or continue a path.
fn is_ident_token(n: Node, src: &str) -> bool {
    if n.kind() == "identifier" {
        return true;
    }
    if n.is_named() && n.kind() != "self" && n.kind() != "crate" && n.kind() != "super" {
        return false; // literals, nested trees, metavariables
    }
    let t = src.get(n.start_byte()..n.end_byte()).unwrap_or("");
    let mut chars = t.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// A `let` condition binds through the `if`/`while` (or match guard) it
/// is part of, including across `&&`-chained conditions.
fn conditional_end(n: Node) -> u32 {
    let mut cur = n.parent();
    while let Some(p) = cur {
        if matches!(p.kind(), "if_expression" | "while_expression" | "match_arm") {
            return p.end_byte() as u32;
        }
        if !matches!(
            p.kind(),
            "let_chain" | "binary_expression" | "parenthesized_expression" | "condition"
        ) {
            return p.end_byte() as u32;
        }
        cur = p.parent();
    }
    n.end_byte() as u32
}

/// A `let` is visible until the end of the block that contains it.
fn scope_end(n: Node) -> u32 {
    n.parent().map_or(u32::MAX, |p| p.end_byte() as u32)
}

fn named(name: &str) -> TypeExpr {
    TypeExpr::Named {
        path: Path::single(name),
        args: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Layout: files -> module tree, crate names -> crate roots
// ---------------------------------------------------------------------------

/// Rust's module tree comes from `mod m;` declarations: a file declaring
/// `mod m;` owns `m.rs` / `m/mod.rs` next to it (next to its own stem
/// directory when it is not itself a `mod.rs`/crate root). Files nobody
/// declares are crate roots. A crate root that is a Cargo package's library
/// target is importable by the package's (underscored) name.
fn layout(root: &FsPath, files: &[(&str, &FileFacts)]) -> Layout {
    let by_path: HashMap<&str, usize> = files
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (*p, i))
        .collect();
    let mut parent_of: Vec<Option<(usize, u32)>> = vec![None; files.len()];
    for (fi, (path, facts)) in files.iter().enumerate() {
        let dir = dir_of(path);
        let stem = stem_of(path);
        // `mod m;` in `a/mod.rs`/`lib.rs`/`main.rs` lives in `a/`; in `a/x.rs`
        // it lives in `a/x/` — unless `a/x.rs` is itself a crate root (a
        // `[lib] path`, a test/example/bin target), whose children sit in `a/`.
        let bases: Vec<String> = if matches!(stem, "mod" | "lib" | "main") {
            vec![dir.to_string()]
        } else {
            vec![join(dir, stem), dir.to_string()]
        };
        for (si, s) in facts.scopes.iter().enumerate() {
            if !s.out_of_line {
                continue;
            }
            let Some(name) = &s.name else { continue };
            // nested inline modules contribute their names to the directory
            let mut segs = vec![name.to_string()];
            let mut p = s.parent;
            while let Some(pi) = p {
                if pi == 0 {
                    break;
                }
                if let Some(n) = &facts.scopes[pi as usize].name {
                    segs.push(n.to_string());
                }
                p = facts.scopes[pi as usize].parent;
            }
            segs.reverse();
            let rel = segs.join("/");
            let found = bases.iter().find_map(|base| {
                [
                    format!("{}.rs", join(base, &rel)),
                    format!("{}/mod.rs", join(base, &rel)),
                ]
                .into_iter()
                .find_map(|cand| by_path.get(cand.as_str()).copied())
            });
            if let Some(ci) = found {
                if parent_of[ci].is_none() && ci != fi {
                    parent_of[ci] = Some((fi, si as u32));
                }
            }
        }
    }
    let crates = crate_names(root, files, &parent_of);
    // Files no `mod` declaration reaches and that are not crate roots by
    // Cargo convention: the input is a partial file set; fall back to
    // directory modules.
    let orphans: Vec<(usize, &str)> = files
        .iter()
        .enumerate()
        .filter(|(fi, (path, _))| parent_of[*fi].is_none() && !is_target_root(path, &crates, *fi))
        .map(|(fi, (path, _))| (fi, *path))
        .collect();
    let mut layout = Layout {
        parent_of,
        crates,
        ..Layout::default()
    };
    layout.add_directory_modules(&orphans, files.len());
    layout
}

/// Crate roots by Cargo convention.
fn is_target_root(path: &str, crates: &HashMap<String, usize>, fi: usize) -> bool {
    let dir = dir_of(path);
    matches!(stem_of(path), "lib" | "main" | "build")
        || crates.values().any(|&c| c == fi)
        || ["tests", "examples", "benches", "bin"]
            .iter()
            .any(|d| dir == *d || dir.ends_with(&format!("/{d}")))
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn stem_of(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".rs").unwrap_or(file)
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// Extern crate names -> library root files. A name is bound by the path
/// dependencies Cargo manifests declare (`foo = { path = "crates/foo" }`,
/// incl. `[workspace.dependencies]`): the repository root's manifest is
/// authoritative, other manifests fill in names it does not bind (a nested
/// workspace, e.g. a test fixture, may reuse a name). Remaining names bind
/// by package name when exactly one package has it. A name two manifests
/// bind differently, with nothing to decide between them, binds nothing.
fn crate_names(
    root: &FsPath,
    files: &[(&str, &FileFacts)],
    parent_of: &[Option<(usize, u32)>],
) -> HashMap<String, usize> {
    let mut manifests: HashMap<String, Option<Manifest>> = HashMap::default();
    let mut lib_of_dir: HashMap<String, usize> = HashMap::default();
    let mut by_package_name: HashMap<String, Vec<usize>> = HashMap::default();
    for (fi, (path, _)) in files.iter().enumerate() {
        if parent_of[fi].is_some() {
            continue;
        }
        let mut dir = dir_of(path).to_string();
        loop {
            let m = manifests
                .entry(dir.clone())
                .or_insert_with(|| read_manifest(root, &dir));
            if let Some(Manifest {
                name: Some(name),
                lib_path,
                ..
            }) = m
            {
                if join(&dir, lib_path) == *path {
                    lib_of_dir.insert(dir.clone(), fi);
                    by_package_name.entry(name.clone()).or_default().push(fi);
                }
                break;
            }
            if dir.is_empty() {
                break;
            }
            dir = dir_of(&dir).to_string();
        }
    }
    manifests
        .entry(String::new())
        .or_insert_with(|| read_manifest(root, ""));
    let bind = |dirs: &mut dyn Iterator<Item = (&String, &Manifest)>| {
        let mut bound: HashMap<String, Option<usize>> = HashMap::default();
        for (dir, m) in dirs {
            for (key, dep_path) in &m.path_deps {
                if let Some(&fi) = lib_of_dir.get(&normalize(&join(dir, dep_path))) {
                    let slot = bound.entry(key.clone()).or_insert(Some(fi));
                    if *slot != Some(fi) {
                        *slot = None; // two different crates under one name
                    }
                }
            }
        }
        bound
    };
    let root_bound = bind(
        &mut manifests
            .iter()
            .filter_map(|(d, m)| Some((d, m.as_ref()?)))
            .filter(|(d, _)| d.is_empty()),
    );
    let other_bound = bind(
        &mut manifests
            .iter()
            .filter_map(|(d, m)| Some((d, m.as_ref()?)))
            .filter(|(d, _)| !d.is_empty()),
    );
    let mut crates: HashMap<String, usize> = HashMap::default();
    for (k, v) in root_bound {
        if let Some(fi) = v {
            crates.insert(k, fi);
        }
    }
    for (k, v) in other_bound {
        if let (Some(fi), false) = (v, crates.contains_key(&k)) {
            crates.insert(k, fi);
        }
    }
    for (name, fis) in by_package_name {
        if let [fi] = fis.as_slice() {
            crates.entry(name).or_insert(*fi);
        }
    }
    crates
}

/// What `sem` reads from a `Cargo.toml`.
#[derive(Clone)]
struct Manifest {
    /// Library crate name (`[lib] name`, else `[package] name`, underscored).
    name: Option<String>,
    /// Library root relative to the manifest directory.
    lib_path: String,
    /// `(extern crate name, relative path)` of path dependencies.
    path_deps: Vec<(String, String)>,
}

fn read_manifest(root: &FsPath, dir: &str) -> Option<Manifest> {
    let text = std::fs::read_to_string(root.join(dir).join("Cargo.toml")).ok()?;
    let mut section = "";
    let mut package_name = None;
    let mut lib_name = None;
    let mut lib_path = None;
    let mut path_deps = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line;
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if section.contains("dependencies") && v.starts_with('{') {
            if let Some(path) = inline_value(v, "path") {
                path_deps.push((k.replace('-', "_"), path.to_string()));
            }
            continue;
        }
        let v = v.trim_matches('"').to_string();
        match (section, k) {
            ("[package]", "name") => package_name = Some(v),
            ("[lib]", "name") => lib_name = Some(v),
            ("[lib]", "path") => lib_path = Some(v),
            _ => {}
        }
    }
    Some(Manifest {
        name: lib_name.or(package_name).map(|n| n.replace('-', "_")),
        lib_path: lib_path.unwrap_or_else(|| "src/lib.rs".to_string()),
        path_deps,
    })
}

/// `key = "value"` inside an inline table `{ .. }`.
fn inline_value<'t>(table: &'t str, key: &str) -> Option<&'t str> {
    let inner = table.trim_start_matches('{').trim_end_matches('}');
    inner.split(',').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"'))
    })
}

/// Resolve `.` and `..` segments of a relative path.
fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
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
