//! Stage 3: type inference — just enough to type a method receiver.
//! `type_of` evaluates an [`Expr`] to a [`Ty`] from declared types
//! (parameters, `let` annotations, struct fields, return types) and the
//! language's builtin-method data; anything else is `Ty::Unknown`.

use super::ir::*;
use super::lang::{BuiltinRet, ClosureArg, Lang};
use super::scope::{Def, Scopes};
use super::select::{impl_subst, Pick, Resolver};

/// A resolved type. Indices are `(file, index)` into that file's facts.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    /// A repo struct/enum with its type arguments.
    Adt(u32, u32, Vec<Ty>),
    /// Only trait bounds are known: a generic parameter, `dyn`/`impl Trait`,
    /// or `Self` inside a trait.
    Param(Vec<(u32, u32)>),
    /// A type defined outside the repo, by its last path segment.
    Ext(Box<str>, Vec<Ty>),
    /// One of several types (`A | B`, either arm of `x if c else y`).
    Union(Vec<Ty>),
    /// A callable (closure, function value, `Fn(A) -> R`): what calling
    /// it returns.
    Fn(Box<Ty>),
    /// The bases of repo type `(file, type)`, in order: Python's `super()`.
    Super(u32, u32),
    Unknown,
}

impl Ty {
    /// The union of `members`: nested unions flattened, duplicates merged;
    /// unknown if any member is.
    pub fn union(members: impl IntoIterator<Item = Ty>) -> Ty {
        let mut out: Vec<Ty> = Vec::new();
        for t in members {
            match t {
                Ty::Unknown => return Ty::Unknown,
                Ty::Union(ts) => {
                    for t in ts {
                        if !out.contains(&t) {
                            out.push(t);
                        }
                    }
                }
                t if !out.contains(&t) => out.push(t),
                _ => {}
            }
        }
        match out.len() {
            1 => out.pop().unwrap(),
            _ => Ty::Union(out), // empty: no value at all (Python's `None`)
        }
    }
}

/// Where a [`TypeExpr`] was written: the names in scope there.
pub struct TyEnv<'e> {
    pub scope: u32,
    pub generics: &'e [Generic],
    pub self_ty: Option<&'e Ty>,
    pub subst: &'e [(Name, Ty)],
}

/// Recursion bound for typing; a builder chain costs a few levels a link.
pub const MAX_DEPTH: u32 = 64;

/// Tuples are modelled as an external type whose arguments are the elements.
pub const TUPLE: &str = "(tuple)";

/// Resolve a written type to a [`Ty`].
pub fn resolve_type(
    scopes: &Scopes,
    lang: &dyn Lang,
    te: &TypeExpr,
    env: &TyEnv,
    depth: u32,
) -> Ty {
    if depth > MAX_DEPTH {
        return Ty::Unknown;
    }
    match te {
        TypeExpr::Named { path, args } => {
            if path.0.len() == 1 {
                let n = path.first();
                if n == lang.self_type() {
                    return env.self_ty.cloned().unwrap_or(Ty::Unknown);
                }
                if let Some((_, t)) = env.subst.iter().find(|(k, _)| &**k == n) {
                    return t.clone();
                }
                if let Some(g) = env.generics.iter().find(|g| &*g.name == n) {
                    return match &g.sig {
                        // `F: Fn(A) -> R`
                        Some(sig) => resolve_type(scopes, lang, sig, env, depth + 1),
                        None => Ty::Param(traits_of(scopes, &g.bounds, env.scope)),
                    };
                }
            } else if path.0.len() == 2 {
                // `T::Assoc` / `Self::Assoc` (in a trait): what the bounds'
                // declaration of `Assoc` is bounded by
                let owner = if path.first() == lang.self_type() {
                    match env.self_ty {
                        Some(Ty::Param(traits)) => Some(traits.clone()),
                        _ => None,
                    }
                } else {
                    env.generics
                        .iter()
                        .find(|g| *g.name == *path.first())
                        .map(|g| traits_of(scopes, &g.bounds, env.scope))
                };
                if let Some(traits) = owner {
                    return assoc_bound(scopes, &traits, path.last());
                }
                if path.first() == lang.self_type() {
                    return Ty::Unknown; // an impl's `Self::Assoc`
                }
            } else if path.first() == lang.self_type()
                || env.generics.iter().any(|g| *g.name == *path.first())
            {
                return Ty::Unknown;
            }
            let targs: Vec<Ty> = args
                .iter()
                .map(|a| resolve_type(scopes, lang, a, env, depth + 1))
                .collect();
            let (defs, used) = scopes.resolve_path(env.scope, &path.0);
            if used == path.0.len() {
                let mut types = defs.iter().filter_map(|d| match d {
                    Def::Type(f, t) => Some((*f, *t)),
                    _ => None,
                });
                if let Some((f, t)) = types.next() {
                    if types.next().is_some() {
                        return Ty::Unknown; // ambiguous
                    }
                    let decl = &scopes.files()[f as usize].types[t as usize];
                    if let TypeKind::Alias(aliased) = &decl.kind {
                        let subst: Vec<(Name, Ty)> = decl
                            .generics
                            .iter()
                            .map(|g| g.name.clone())
                            .zip(targs)
                            .collect();
                        let alias_env = TyEnv {
                            scope: scopes.global(f, decl.scope),
                            generics: &decl.generics,
                            self_ty: None,
                            subst: &subst,
                        };
                        return resolve_type(scopes, lang, aliased, &alias_env, depth + 1);
                    }
                    return Ty::Adt(f, t, targs);
                }
                if let Some(Def::Trait(f, t)) = defs.iter().find(|d| matches!(d, Def::Trait(..))) {
                    return Ty::Param(vec![(*f, *t)]);
                }
            }
            if defs.contains(&Def::External) {
                return Ty::Ext(path.last().into(), targs);
            }
            Ty::Unknown
        }
        TypeExpr::Ref(inner) => resolve_type(scopes, lang, inner, env, depth + 1),
        TypeExpr::Traits(paths) => Ty::Param(traits_of(scopes, paths, env.scope)),
        TypeExpr::Slice(inner) => Ty::Ext(
            "slice".into(),
            vec![resolve_type(scopes, lang, inner, env, depth + 1)],
        ),
        TypeExpr::Fn(_, ret) => Ty::Fn(Box::new(match ret {
            Some(r) => resolve_type(scopes, lang, r, env, depth + 1),
            None => Ty::Ext(TUPLE.into(), Vec::new()),
        })),
        TypeExpr::Union(members) => Ty::union(
            members
                .iter()
                .map(|m| resolve_type(scopes, lang, m, env, depth + 1)),
        ),
        TypeExpr::Tuple(elems) => Ty::Ext(
            TUPLE.into(),
            elems
                .iter()
                .map(|e| resolve_type(scopes, lang, e, env, depth + 1))
                .collect(),
        ),
        TypeExpr::Unknown => Ty::Unknown,
    }
}

/// Associated type `name` as declared by one of `traits`: known by its
/// bounds (`type Captures: Captures`).
fn assoc_bound(scopes: &Scopes, traits: &[(u32, u32)], name: &str) -> Ty {
    for &(f, t) in traits {
        let decl = &scopes.files()[f as usize].traits[t as usize];
        if let Some(a) = decl.assoc.iter().find(|a| *a.name == *name) {
            return Ty::Param(traits_of(scopes, &a.bounds, scopes.global(f, decl.scope)));
        }
    }
    Ty::Unknown
}

pub fn traits_of(scopes: &Scopes, bounds: &[Path], scope: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for b in bounds {
        let (defs, used) = scopes.resolve_path(scope, &b.0);
        if used != b.0.len() {
            continue;
        }
        for d in defs {
            if let Def::Trait(f, t) = d {
                out.push((f, t));
            }
        }
    }
    out
}

/// Apply a builtin-method rule to the receiver type.
pub fn apply_builtin(rule: BuiltinRet, recv: &Ty) -> Ty {
    let args: &[Ty] = match recv {
        Ty::Adt(_, _, a) | Ty::Ext(_, a) => a,
        _ => &[],
    };
    match rule {
        BuiltinRet::Same => recv.clone(),
        BuiltinRet::Arg(i) => args.get(i).cloned().unwrap_or(Ty::Unknown),
        BuiltinRet::Wrap(w, i) => {
            Ty::Ext(w.into(), vec![args.get(i).cloned().unwrap_or(Ty::Unknown)])
        }
        BuiltinRet::Named(n) => Ty::Ext(n.into(), Vec::new()),
        BuiltinRet::WrapAll(w) => Ty::Ext(w.into(), args.to_vec()),
    }
}

/// Everything in scope inside one function: where it is, its generics, and
/// what `Self`/`self` mean.
pub struct FnCx {
    pub file: u32,
    pub func: Option<u32>,
    /// Global scope id at the site.
    pub scope: u32,
    pub self_ty: Option<Ty>,
    pub generics: Vec<Generic>,
}

impl FnCx {
    pub fn env(&self) -> TyEnv<'_> {
        TyEnv {
            scope: self.scope,
            generics: &self.generics,
            self_ty: self.self_ty.as_ref(),
            subst: &[],
        }
    }
}

impl<'t, 'a> Resolver<'t, 'a> {
    pub fn fn_cx(&self, file: u32, func: Option<u32>, local_scope: u32) -> FnCx {
        let f = self.files[file as usize];
        let scope = self.scopes.global(file, local_scope);
        let Some(fi) = func else {
            return FnCx {
                file,
                func,
                scope,
                self_ty: None,
                generics: Vec::new(),
            };
        };
        let decl = &f.fns[fi as usize];
        let mut generics = decl.generics.clone();
        let self_ty = match decl.owner {
            Owner::Free => None,
            Owner::Impl(i) => {
                let imp = &f.impls[i as usize];
                generics.extend(imp.generics.iter().cloned());
                Some(self.impl_self_ty(file, i))
            }
            Owner::Trait(t) => {
                generics.extend(f.traits[t as usize].generics.iter().cloned());
                Some(Ty::Param(vec![(file, t)]))
            }
        };
        FnCx {
            file,
            func,
            scope,
            self_ty,
            generics,
        }
    }

    pub fn impl_self_ty(&self, file: u32, imp: u32) -> Ty {
        let decl = &self.files[file as usize].impls[imp as usize];
        let env = TyEnv {
            scope: self.scopes.global(file, decl.scope),
            generics: &decl.generics,
            self_ty: None,
            subst: &[],
        };
        resolve_type(&self.scopes, self.lang, &decl.self_ty, &env, 0)
    }

    /// The type an expression evaluates to, as seen at byte `at`.
    pub fn type_of(&self, e: ExprId, cx: &FnCx, at: u32, depth: u32) -> Ty {
        // a call's type is memoized (a chain's links are shared by its
        // sites) and so computed with a fresh depth budget; the
        // placeholder cuts cycles through return types
        let f = self.files[cx.file as usize];
        if !matches!(
            f.expr(e),
            Expr::Call(_) | Expr::Method(..) | Expr::Builtin(..)
        ) {
            return self.type_uncached(e, cx, at, depth);
        }
        let key = (cx.file, e.0);
        if let Some(t) = self.expr_memo.borrow().get(&key) {
            return t.clone();
        }
        self.expr_memo.borrow_mut().insert(key, Ty::Unknown);
        let t = self.type_uncached(e, cx, at, 0);
        self.expr_memo.borrow_mut().insert(key, t.clone());
        t
    }

    fn type_uncached(&self, e: ExprId, cx: &FnCx, at: u32, depth: u32) -> Ty {
        if depth > MAX_DEPTH {
            return Ty::Unknown;
        }
        let f = self.files[cx.file as usize];
        let ty = |e: ExprId, at: u32| self.type_of(e, cx, at, depth + 1);
        match f.expr(e) {
            Expr::Path(p) if p.len == 1 => {
                let sym = f.path_pool[p.start as usize];
                if let Some(li) = self.local(cx, sym, at) {
                    return self.variable_type(cx, li, at, depth + 1);
                }
                if f.sym(sym) == self.lang.self_value() {
                    return cx.self_ty.clone().unwrap_or(Ty::Unknown);
                }
                self.value_type(&f.path(p), cx, depth)
            }
            Expr::Path(p) => self.value_type(&f.path(p), cx, depth),
            Expr::Field(recv, field) => {
                // `pkg.Value` (Go): a qualified value, not a field
                if let Some(mut path) = self.module_path(recv, cx, at) {
                    path.push(f.sym(field));
                    return self.value_type(&path, cx, depth);
                }
                self.field_type(&ty(recv, at), f.sym(field), depth + 1)
            }
            Expr::Call(_) | Expr::Method(..) => {
                let pick = self.pick(e, cx, at, depth + 1);
                let targs: Vec<Ty> = f
                    .turbofish(e)
                    .iter()
                    .map(|&t| {
                        let te = &f.type_pool[t as usize];
                        resolve_type(&self.scopes, self.lang, te, &cx.env(), depth + 1)
                    })
                    .collect();
                match pick {
                    // calling a value: a closure, a function-typed variable
                    // or field, a class object
                    Pick::Unknown(_) => {
                        let callee = match f.expr(e) {
                            Expr::Call(c) => ty(c, at),
                            Expr::Method(r, n) => self.field_type(&ty(r, at), f.sym(n), depth + 1),
                            _ => Ty::Unknown,
                        };
                        self.call_result(&callee)
                    }
                    pick => self.pick_type(&pick, &targs, depth + 1),
                }
            }
            Expr::Closure(body) => Ty::Fn(Box::new(ty(body, at))),
            Expr::Super(t) => Ty::Super(cx.file, t),
            Expr::Builtin(call, typed) => match self.pick(call, cx, at, depth + 1) {
                Pick::External(_) => ty(typed, at),
                _ => ty(call, at),
            },
            Expr::Union(l) => Ty::union(f.list(l).iter().map(|&x| ty(x, at))),
            Expr::Unwrap(inner) => match ty(inner, at) {
                Ty::Ext(n, args) if matches!(&*n, "Option" | "Result" | "Poll") => {
                    args.into_iter().next().unwrap_or(Ty::Unknown)
                }
                _ => Ty::Unknown,
            },
            Expr::Elem(inner) => self.elem_type(ty(inner, at)),
            Expr::Deref(inner) => {
                // references are transparent in `Ty`, so `*` peels one smart
                // pointer / `Deref` layer (or is a no-op on a plain reference)
                let t = ty(inner, at);
                self.deref_once(&t).unwrap_or(t)
            }
            Expr::Tuple(l) => Ty::Ext(TUPLE.into(), f.list(l).iter().map(|&x| ty(x, at)).collect()),
            Expr::Ext(name, l) => Ty::Ext(
                f.sym(name).into(),
                f.list(l).iter().map(|&x| ty(x, at)).collect(),
            ),
            Expr::Branches(l) => f
                .list(l)
                .iter()
                .map(|&x| ty(x, at))
                .find(|t| *t != Ty::Unknown)
                .unwrap_or(Ty::Unknown),
            Expr::At(pos, inner) => ty(inner, pos),
            Expr::Payload(inner, ctor, field) => {
                self.payload_type(&ty(inner, at), f.sym(ctor), f.sym(field), depth + 1)
            }
            Expr::Struct(p) => self.path_type(&f.path(p), cx, depth),
            Expr::Param(k) => match (self.param_hints, cx.func) {
                (Some(h), Some(func)) => h.get(&(cx.file, func, k)).cloned().unwrap_or(Ty::Unknown),
                _ => Ty::Unknown,
            },
            Expr::Arg(call, pos, k) => {
                self.closure_arg_type(call, pos as usize, k as usize, cx, at, depth)
            }
            Expr::Typed(t) => resolve_type(
                &self.scopes,
                self.lang,
                &f.type_pool[t as usize],
                &cx.env(),
                depth + 1,
            ),
            Expr::Unknown => Ty::Unknown,
        }
    }

    /// `recv` names a module rather than a value — a package qualifier
    /// (`pkg.F`, Go) or a dotted module path (`a.util.f`, Python): its
    /// segments.
    pub fn module_path(&self, recv: ExprId, cx: &FnCx, at: u32) -> Option<Vec<&'a str>> {
        let f = self.files[cx.file as usize];
        let segs = match f.expr(recv) {
            Expr::Path(p) if p.len == 1 => {
                let sym = f.path_pool[p.start as usize];
                if self.local(cx, sym, at).is_some() {
                    return None;
                }
                vec![f.sym(sym)]
            }
            Expr::Field(inner, name) => {
                let mut segs = self.module_path(inner, cx, at)?;
                segs.push(f.sym(name));
                segs
            }
            _ => return None,
        };
        let (defs, used) = self.scopes.resolve_path(cx.scope, &segs);
        (used == segs.len() && defs.iter().any(|d| matches!(d, Def::Module(_)))).then_some(segs)
    }

    /// `recv` is a bare non-local name bound only outside the repo (an
    /// imported external package): calls through it are external.
    pub fn external_receiver(&self, recv: ExprId, cx: &FnCx, at: u32) -> bool {
        let f = self.files[cx.file as usize];
        let Expr::Path(p) = f.expr(recv) else {
            return false;
        };
        if p.len != 1 {
            return false;
        }
        let sym = f.path_pool[p.start as usize];
        if self.local(cx, sym, at).is_some() || f.sym(sym) == self.lang.self_value() {
            return false;
        }
        self.scopes.lookup(cx.scope, f.sym(sym)) == [Def::External]
    }

    /// The declared type of a `const` / `static` a path names.
    fn value_type(&self, segs: &[&str], cx: &FnCx, depth: u32) -> Ty {
        let (defs, used) = self.scopes.resolve_path(cx.scope, segs);
        if used + 1 == segs.len() {
            // `Enum::Unit`: a value of the enum
            return match defs.iter().find(|d| matches!(d, Def::Type(..))) {
                Some(Def::Type(f, t))
                    if self.files[*f as usize].types[*t as usize]
                        .variants
                        .iter()
                        .any(|(v, _)| **v == *segs[used]) =>
                {
                    self.path_type(&segs[..used], cx, depth)
                }
                _ => Ty::Unknown,
            };
        }
        if used != segs.len() {
            return Ty::Unknown;
        }
        if defs.iter().any(|d| matches!(d, Def::Type(..))) {
            return self.path_type(segs, cx, depth); // a unit struct
        }
        let mut values = defs.iter().filter_map(|d| match d {
            Def::Value(f, i) => Some((*f, *i)),
            _ => None,
        });
        match (values.next(), values.next()) {
            (Some((f, i)), None) => {
                let decl = &self.files[f as usize].values[i as usize];
                match (&decl.ty, decl.init) {
                    (Some(ty), _) => {
                        let env = TyEnv {
                            scope: self.scopes.global(f, decl.scope),
                            generics: &[],
                            self_ty: None,
                            subst: &[],
                        };
                        resolve_type(&self.scopes, self.lang, ty, &env, depth + 1)
                    }
                    (None, Some(init)) => {
                        let vcx = self.fn_cx(f, None, decl.scope);
                        self.type_of(init, &vcx, u32::MAX, depth + 1)
                    }
                    _ => Ty::Unknown,
                }
            }
            _ => Ty::Unknown,
        }
    }

    /// The result type of a resolved call.
    /// For several targets, the union of their result types.
    pub fn pick_type(&self, pick: &Pick, targs: &[Ty], depth: u32) -> Ty {
        match pick {
            Pick::Defs(defs, through) => Ty::union(defs.iter().map(|d| match d {
                Def::Fn(f, i) => self.fn_ret(*f, *i, through.as_ref(), targs, depth),
                Def::Type(..) => through.clone().unwrap_or(Ty::Unknown),
                _ => Ty::Unknown,
            })),
            Pick::External(Some(t)) => t.clone(),
            _ => Ty::Unknown,
        }
    }

    /// What calling a value of type `t` returns.
    fn call_result(&self, t: &Ty) -> Ty {
        match t {
            Ty::Fn(r) => (**r).clone(),
            Ty::Ext(n, _) => match self.lang.call_result(n) {
                Some(rule) => apply_builtin(rule, t),
                None => Ty::Unknown,
            },
            Ty::Union(ts) => Ty::union(ts.iter().map(|t| self.call_result(t))),
            _ => Ty::Unknown,
        }
    }

    /// The declared return type of fn `(file, idx)`, with `Self` and the
    /// impl's generic parameters instantiated from `owner` when known.
    pub fn fn_ret(&self, file: u32, idx: u32, owner: Option<&Ty>, targs: &[Ty], depth: u32) -> Ty {
        let decl = &self.files[file as usize].fns[idx as usize];
        let Some(ret) = &decl.ret else {
            return self.returned_type(file, idx, depth);
        };
        let (generics, mut subst, self_ty) = self.callee_env(file, idx, owner);
        // `f::<A, B>()` instantiates f's own type parameters, in order
        for (g, t) in decl.generics.iter().zip(targs) {
            if *t != Ty::Unknown {
                subst.push((g.name.clone(), t.clone()));
            }
        }
        let env = TyEnv {
            scope: self.scopes.global(file, decl.scope),
            generics: &generics,
            self_ty: self_ty.as_ref(),
            subst: &subst,
        };
        resolve_type(&self.scopes, self.lang, ret, &env, depth + 1)
    }

    /// The type an undeclared-return function returns: what its `return`
    /// expressions evaluate to, when they all agree.
    fn returned_type(&self, file: u32, idx: u32, depth: u32) -> Ty {
        let f = self.files[file as usize];
        let lo = f.returns.partition_point(|r| r.func < idx);
        let hi = f.returns.partition_point(|r| r.func <= idx);
        let mut agreed = Ty::Unknown;
        for r in &f.returns[lo..hi] {
            let cx = self.fn_cx(file, Some(idx), r.scope);
            match self.type_of(r.value, &cx, u32::MAX, depth + 1) {
                Ty::Unknown => {}
                t if agreed == Ty::Unknown => agreed = t,
                t if t == agreed => {}
                _ => return Ty::Unknown,
            }
        }
        agreed
    }

    /// What a callee's signature is resolved against: its generics (own +
    /// impl/trait), the impl's type parameters instantiated from the
    /// receiver type `owner`, and `Self`.
    fn callee_env(
        &self,
        file: u32,
        idx: u32,
        owner: Option<&Ty>,
    ) -> (Vec<Generic>, Vec<(Name, Ty)>, Option<Ty>) {
        let f = self.files[file as usize];
        let decl = &f.fns[idx as usize];
        let mut generics = decl.generics.clone();
        let mut subst: Vec<(Name, Ty)> = Vec::new();
        let self_ty = match decl.owner {
            Owner::Free => None,
            Owner::Impl(i) => {
                let imp = &f.impls[i as usize];
                generics.extend(imp.generics.iter().cloned());
                let own = match owner {
                    Some(t @ Ty::Adt(..)) | Some(t @ Ty::Ext(..)) => t.clone(),
                    _ => self.impl_self_ty(file, i),
                };
                subst = impl_subst(imp, &own);
                Some(own)
            }
            Owner::Trait(t) => {
                generics.extend(f.traits[t as usize].generics.iter().cloned());
                Some(owner.cloned().unwrap_or(Ty::Param(vec![(file, t)])))
            }
        };
        (generics, subst, self_ty)
    }

    /// Parameter `k` of a closure passed as argument `pos` of `call`: from
    /// the callee's declared parameter type (`impl FnOnce(&mut T) -> R`,
    /// `F: Fn(A)`), or the language's data for std adapters.
    fn closure_arg_type(
        &self,
        call: ExprId,
        pos: usize,
        k: usize,
        cx: &FnCx,
        at: u32,
        depth: u32,
    ) -> Ty {
        let f = self.files[cx.file as usize];
        let pick = self.pick(call, cx, at, depth + 1);
        if let Some((tf, ti, owner)) = pick.single_fn() {
            let decl = &self.files[tf as usize].fns[ti as usize];
            // `Type::method(recv, ..)` passes the receiver as argument 0
            let is_method_call = matches!(f.expr(call), Expr::Method(..));
            let pos = if decl.has_self && !is_method_call {
                match pos.checked_sub(1) {
                    Some(p) => p,
                    None => return Ty::Unknown,
                }
            } else {
                pos
            };
            let Some(param) = decl.params.get(pos) else {
                return Ty::Unknown;
            };
            let (generics, subst, self_ty) = self.callee_env(tf, ti, owner);
            let sig = match param {
                TypeExpr::Ref(inner) => &**inner,
                t => t,
            };
            let sig = match sig {
                TypeExpr::Named { path, .. } if path.0.len() == 1 => generics
                    .iter()
                    .find(|g| g.name == path.0[0])
                    .and_then(|g| g.sig.as_ref())
                    .unwrap_or(sig),
                _ => sig,
            };
            let TypeExpr::Fn(params, _) = sig else {
                return Ty::Unknown;
            };
            let Some(p) = params.get(k) else {
                return Ty::Unknown;
            };
            let env = TyEnv {
                scope: self.scopes.global(tf, decl.scope),
                generics: &generics,
                self_ty: self_ty.as_ref(),
                subst: &subst,
            };
            return resolve_type(&self.scopes, self.lang, p, &env, depth + 1);
        }
        match pick {
            Pick::External(_) => {
                let Expr::Method(recv, name) = f.expr(call) else {
                    return Ty::Unknown;
                };
                match self.lang.closure_param(f.sym(name), pos, k) {
                    Some(ClosureArg::Elem) => self.elem_type(self.type_of(recv, cx, at, depth + 1)),
                    Some(ClosureArg::Variant(v)) => {
                        let t = self.type_of(recv, cx, at, depth + 1);
                        self.payload_type(&t, v, "0", depth + 1)
                    }
                    None => Ty::Unknown,
                }
            }
            _ => Ty::Unknown,
        }
    }

    /// The element type of a container / iterator / `Option` / `Result`.
    fn elem_type(&self, t: Ty) -> Ty {
        match &t {
            Ty::Adt(f, ti, _) => {
                let decl = &self.files[*f as usize].types[*ti as usize];
                if let TypeKind::Defined(under) = &decl.kind {
                    let env = TyEnv {
                        scope: self.scopes.global(*f, decl.scope),
                        generics: &decl.generics,
                        self_ty: None,
                        subst: &[],
                    };
                    return self.elem_type(resolve_type(&self.scopes, self.lang, under, &env, 1));
                }
                match self.lang.iterates_as() {
                    Some((tr, assoc, _)) => self
                        .impls
                        .assoc_type(self, &t, tr, assoc)
                        .unwrap_or(Ty::Unknown),
                    None => Ty::Unknown,
                }
            }
            Ty::Ext(n, _) => match self.lang.elem(n) {
                Some(rule) => apply_builtin(rule, &t),
                None => Ty::Unknown,
            },
            Ty::Union(ts) => Ty::union(ts.iter().map(|t| self.elem_type(t.clone()))),
            _ => Ty::Unknown,
        }
    }

    /// One auto-deref step, if the type has one.
    pub fn deref_once(&self, t: &Ty) -> Option<Ty> {
        match t {
            Ty::Ext(n, _) => self.lang.deref(n).map(|rule| apply_builtin(rule, t)),
            Ty::Adt(..) => self.impls.deref_target(self, t),
            _ => None,
        }
    }

    /// The type a destructuring pattern binds for `field` of `ctor`.
    fn payload_type(&self, t: &Ty, ctor: &str, field: &str, depth: u32) -> Ty {
        if ctor.is_empty() {
            // tuple pattern
            return match (t, field.parse::<usize>()) {
                (Ty::Ext(n, elems), Ok(i)) if &**n == TUPLE => {
                    elems.get(i).cloned().unwrap_or(Ty::Unknown)
                }
                _ => Ty::Unknown,
            };
        }
        if let Some(rule) = self.lang.variant_payload(ctor, field) {
            return apply_builtin(rule, t);
        }
        match t {
            Ty::Adt(f, ti, args) => {
                let decl = &self.files[*f as usize].types[*ti as usize];
                match &decl.kind {
                    TypeKind::Enum => {
                        let Some((_, fields)) = decl.variants.iter().find(|(v, _)| &**v == ctor)
                        else {
                            return Ty::Unknown;
                        };
                        let Some((_, te)) = fields.iter().find(|(n, _)| &**n == field) else {
                            return Ty::Unknown;
                        };
                        let subst: Vec<(Name, Ty)> = decl
                            .generics
                            .iter()
                            .map(|g| g.name.clone())
                            .zip(args.iter().cloned())
                            .filter(|(_, t)| *t != Ty::Unknown)
                            .collect();
                        let env = TyEnv {
                            scope: self.scopes.global(*f, decl.scope),
                            generics: &decl.generics,
                            self_ty: Some(t),
                            subst: &subst,
                        };
                        resolve_type(&self.scopes, self.lang, te, &env, depth + 1)
                    }
                    TypeKind::Struct => self.field_type(t, field, depth + 1),
                    TypeKind::Alias(_) | TypeKind::Defined(_) => Ty::Unknown,
                }
            }
            _ => Ty::Unknown,
        }
    }

    pub fn field_type(&self, t: &Ty, field: &str, depth: u32) -> Ty {
        if depth > MAX_DEPTH {
            return Ty::Unknown;
        }
        match t {
            Ty::Union(ts) => Ty::union(ts.iter().map(|t| self.field_type(t, field, depth + 1))),
            Ty::Adt(f, ti, args) => {
                let decl = &self.files[*f as usize].types[*ti as usize];
                if decl.kind != TypeKind::Struct {
                    return Ty::Unknown;
                }
                let Some((_, te)) = decl.fields.iter().find(|(n, _)| &**n == field) else {
                    if let Some(&(_, func, scope, init)) =
                        decl.field_inits.iter().find(|(n, ..)| &**n == field)
                    {
                        let icx = self.fn_cx(*f, Some(func), scope);
                        return self.type_of(init, &icx, u32::MAX, depth + 1);
                    }
                    if let Some(target) = self.impls.deref_target(self, t) {
                        return self.field_type(&target, field, depth + 1);
                    }
                    // inherited / promoted attributes
                    let env = TyEnv {
                        scope: self.scopes.global(*f, decl.scope),
                        generics: &decl.generics,
                        self_ty: None,
                        subst: &[],
                    };
                    for e in &decl.embeds {
                        let et = resolve_type(&self.scopes, self.lang, e, &env, depth + 1);
                        let ft = self.field_type(&et, field, depth + 1);
                        if ft != Ty::Unknown {
                            return ft;
                        }
                    }
                    return Ty::Unknown;
                };
                let subst: Vec<(Name, Ty)> = decl
                    .generics
                    .iter()
                    .map(|g| g.name.clone())
                    .zip(args.iter().cloned())
                    .filter(|(_, t)| *t != Ty::Unknown)
                    .collect();
                let env = TyEnv {
                    scope: self.scopes.global(*f, decl.scope),
                    generics: &decl.generics,
                    self_ty: Some(t),
                    subst: &subst,
                };
                resolve_type(&self.scopes, self.lang, te, &env, depth + 1)
            }
            Ty::Ext(name, _) if self.lang.builtin_field(name, field).is_some() => {
                apply_builtin(self.lang.builtin_field(name, field).unwrap(), t)
            }
            Ty::Ext(name, args) => match self.lang.deref(name) {
                Some(rule) => self.field_type(
                    &apply_builtin(rule, &Ty::Ext(name.clone(), args.clone())),
                    field,
                    depth + 1,
                ),
                None => Ty::Unknown,
            },
            _ => Ty::Unknown,
        }
    }

    /// The local binding `name` visible at `at` in the current function,
    /// else (for closures over them) in its enclosing functions.
    pub fn local(&self, cx: &FnCx, name: Sym, at: u32) -> Option<usize> {
        let f = self.files[cx.file as usize];
        let mut func = cx.func?;
        for _ in 0..8 {
            let locals = &f.locals;
            let lo = locals.partition_point(|l| l.func < func);
            let hi = locals.partition_point(|l| l.func <= func);
            let hit = (lo..hi).rev().find(|&i| {
                let l = &locals[i];
                l.at <= at && at < l.until && l.name == name
            });
            if hit.is_some() {
                return hit;
            }
            // a nested function sees its enclosing function's locals (the
            // site lies inside the enclosing function's body too)
            func = f.fns[func as usize].enclosing?;
        }
        None
    }

    /// The type of the variable whose latest binding before `at` is local
    /// `li`. Where a name is one variable per function (Python), every
    /// binding that can reach `at` contributes: those before it, and those
    /// after it in a loop around it.
    fn variable_type(&self, cx: &FnCx, li: usize, at: u32, depth: u32) -> Ty {
        if !self.lang.function_scoped_names() {
            return self.local_type(cx, li, depth);
        }
        let f = self.files[cx.file as usize];
        let l = f.locals[li];
        let lo = f.locals.partition_point(|b| b.func < l.func);
        let hi = f.locals.partition_point(|b| b.func <= l.func);
        let in_loop = |b: u32| {
            f.loops
                .iter()
                .any(|&(func, s, e)| func == l.func && s <= at && at < e && s <= b && b < e)
        };
        Ty::union((lo..hi).filter_map(|i| {
            let b = f.locals[i];
            let reaches = b.name == l.name && at < b.until && (b.at <= at || in_loop(b.at));
            reaches.then(|| self.local_type(cx, i, depth))
        }))
    }

    /// A binding's own type — memoized, so computed with a fresh depth
    /// budget (the memo's placeholder cuts cycles).
    fn local_type(&self, cx: &FnCx, idx: usize, _depth: u32) -> Ty {
        let depth = 0;
        let key = (cx.file, idx as u32);
        if let Some(t) = self.local_memo.borrow().get(&key) {
            return t.clone();
        }
        self.local_memo.borrow_mut().insert(key, Ty::Unknown);
        let f = self.files[cx.file as usize];
        let l = f.locals[idx];
        let declared = l.ty.map(|t| &f.type_pool[t as usize]);
        let init = || match l.init {
            Some(init) => self.type_of(init, cx, l.at.saturating_sub(1), depth + 1),
            None => Ty::Unknown,
        };
        let t = match declared {
            // a partial annotation (`Vec<_>`) defers to the initializer
            Some(te) if te.has_hole() => match init() {
                Ty::Unknown => resolve_type(&self.scopes, self.lang, te, &cx.env(), depth + 1),
                t => t,
            },
            Some(te) => resolve_type(&self.scopes, self.lang, te, &cx.env(), depth + 1),
            None => init(),
        };
        self.local_memo.borrow_mut().insert(key, t.clone());
        t
    }
}
