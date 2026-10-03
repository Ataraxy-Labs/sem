//! Stage 4: target selection. Given a call expression, pick the repo
//! definitions it can invoke — or say precisely why not ([`Pick`]). Three
//! rules cover every call form:
//!
//! * a **path** (`f`, `m::f`, `Type::f`, `Trait::f`, `Self::f`) is resolved
//!   by the scope stage; a trailing segment after a type/trait is an
//!   associated function, looked up like a method on that type;
//! * a **method** `recv.m` is looked up on `type_of(recv)`:
//!   inherent impls, then trait impls, then methods the type's traits
//!   declare (default bodies), then through `Deref`;
//! * a receiver known only by its **trait bounds** (generics, `dyn`,
//!   `Self` in a trait) binds to the trait's declaration of the method.
//!
//! Never a same-name guess: when the rules leave several candidates the
//! answer is all of them (the site reaches one); an unknown receiver type
//! yields [`Pick::Unknown`].

use std::cell::RefCell;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::infer::{apply_builtin, resolve_type, traits_of, FnCx, Ty, TyEnv};
use super::ir::*;
use super::lang::Lang;
use super::scope::{Def, Scopes};

/// The outcome of resolving one site: a set of repo definitions, nothing
/// in the repo, or unknown.
#[derive(Clone, Debug, PartialEq)]
pub enum Pick {
    /// The repo definitions the site binds to: exactly one, or several of
    /// which it reaches one (`#[cfg]` twins, conditional definitions, impls
    /// told apart only by argument types). With the type they were selected
    /// through: the receiver (instantiating `Self` and impl generics in a
    /// method's return type), or the type a constructor call builds.
    Defs(Vec<Def>, Option<Ty>),
    /// Defined outside the repo; carries the result type when known.
    External(Option<Ty>),
    /// Not statically knowable (a closure or function value, an untyped
    /// receiver), and why.
    Unknown(&'static str),
}

impl Pick {
    pub fn func(f: u32, i: u32, through: Option<Ty>) -> Pick {
        Pick::Defs(vec![Def::Fn(f, i)], through)
    }

    fn fns(fns: Vec<(u32, u32)>, through: Option<Ty>) -> Pick {
        Pick::Defs(
            fns.into_iter().map(|(f, i)| Def::Fn(f, i)).collect(),
            through,
        )
    }

    /// The single function this pick is, if it is exactly one.
    pub fn single_fn(&self) -> Option<(u32, u32, Option<&Ty>)> {
        match self {
            Pick::Defs(d, t) => match d.as_slice() {
                [Def::Fn(f, i)] => Some((*f, *i, t.as_ref())),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Impl blocks indexed by the type / trait they are for, plus each impl's
/// and trait's member functions.
#[derive(Default)]
pub struct ImplTables {
    pub by_adt: HashMap<(u32, u32), Vec<(u32, u32)>>,
    by_ext: HashMap<Box<str>, Vec<(u32, u32)>>,
    /// Impls of traits from outside the repo (`impl Display for T`), by
    /// the trait's last path segment.
    by_ext_trait: HashMap<Box<str>, Vec<(u32, u32)>>,
    /// Blanket impls (`impl<T: Bound> Trait for T`).
    blanket: Vec<(u32, u32)>,
    pub by_trait: HashMap<(u32, u32), Vec<(u32, u32)>>,
    impl_trait: HashMap<(u32, u32), (u32, u32)>,

    pub impl_members: Vec<Vec<Vec<u32>>>,
    pub trait_members: Vec<Vec<Vec<u32>>>,
    supertraits: HashMap<(u32, u32), Vec<(u32, u32)>>,
}

impl ImplTables {
    pub fn build(files: &[&FileFacts], scopes: &Scopes, lang: &dyn Lang) -> Self {
        let mut t = ImplTables::default();
        for (fi, f) in files.iter().enumerate() {
            let fi = fi as u32;
            let mut impl_members = vec![Vec::new(); f.impls.len()];
            let mut trait_members = vec![Vec::new(); f.traits.len()];
            for (i, func) in f.fns.iter().enumerate() {
                match func.owner {
                    Owner::Impl(k) => impl_members[k as usize].push(i as u32),
                    Owner::Trait(k) => trait_members[k as usize].push(i as u32),
                    Owner::Free => {}
                }
            }
            t.impl_members.push(impl_members);
            t.trait_members.push(trait_members);
            for (ti, tr) in f.traits.iter().enumerate() {
                let supers = traits_of(scopes, &tr.supertraits, scopes.global(fi, tr.scope));
                if !supers.is_empty() {
                    t.supertraits.insert((fi, ti as u32), supers);
                }
            }
            for (ii, imp) in f.impls.iter().enumerate() {
                let key = (fi, ii as u32);
                let scope = scopes.global(fi, imp.scope);
                let env = TyEnv {
                    scope,
                    generics: &imp.generics,
                    self_ty: None,
                    subst: &[],
                };
                let self_ty = resolve_type(scopes, lang, &imp.self_ty, &env, 0);
                let trait_ = imp.trait_.as_ref().and_then(|p| {
                    let (defs, used) = scopes.resolve_path(scope, &p.0);
                    if used != p.0.len() {
                        return None;
                    }
                    defs.into_iter().find_map(|d| match d {
                        Def::Trait(f, t) => Some((f, t)),
                        _ => None,
                    })
                });
                match (trait_, &imp.trait_) {
                    (Some(tr), _) => {
                        t.impl_trait.insert(key, tr);
                        t.by_trait.entry(tr).or_default().push(key);
                    }
                    (None, Some(p)) => t.by_ext_trait.entry(p.last().into()).or_default().push(key),
                    (None, None) => {}
                }

                match &self_ty {
                    Ty::Adt(f, ty, _) => t.by_adt.entry((*f, *ty)).or_default().push(key),
                    Ty::Ext(name, _) => t.by_ext.entry(name.clone()).or_default().push(key),
                    // `impl<T: B> Tr for T`; not the forwarding `for &T`
                    Ty::Param(_) if !matches!(imp.self_ty, TypeExpr::Ref(_)) => t.blanket.push(key),
                    _ => {}
                }
            }
        }
        t
    }

    /// The `Deref::Target` of a repo type (`recv`), if it has one, with the
    /// impl's type parameters instantiated from `recv`.
    pub fn deref_target(&self, r: &Resolver, recv: &Ty) -> Option<Ty> {
        self.assoc_type(r, recv, "Deref", "Target")
    }

    /// Associated type `assoc` of a repo type's impl of the trait named
    /// `trait_` (`Iterator::Item`, `Deref::Target`), instantiated from
    /// `recv`'s type arguments.
    pub fn assoc_type(&self, r: &Resolver, recv: &Ty, trait_: &str, assoc: &str) -> Option<Ty> {
        let Ty::Adt(f, ty, _) = recv else { return None };
        let (imp, te) = self.by_adt.get(&(*f, *ty))?.iter().find_map(|&(fi, ii)| {
            let imp = &r.files[fi as usize].impls[ii as usize];
            if imp.trait_.as_ref()?.last() != trait_ {
                return None;
            }
            let (_, te) = imp.assoc_types.iter().find(|(n, _)| &**n == assoc)?;
            Some(((fi, imp), te))
        })?;
        let (fi, imp) = imp;
        let subst = impl_subst(imp, recv);
        let env = TyEnv {
            scope: r.scopes.global(fi, imp.scope),
            generics: &imp.generics,
            self_ty: Some(recv),
            subst: &subst,
        };
        Some(resolve_type(&r.scopes, r.lang, te, &env, 1))
    }

    /// A trait and all its (transitive) supertraits.
    pub fn trait_closure(&self, roots: &[(u32, u32)]) -> Vec<(u32, u32)> {
        let mut seen: HashSet<(u32, u32)> = HashSet::default();
        let mut out = Vec::new();
        let mut stack: Vec<(u32, u32)> = roots.to_vec();
        while let Some(t) = stack.pop() {
            if seen.insert(t) {
                out.push(t);
                if let Some(s) = self.supertraits.get(&t) {
                    stack.extend(s.iter().copied());
                }
            }
        }
        out
    }
}

/// The impl's type parameters instantiated from a receiver type:
/// `impl<T> Wrapper<T>` on `Wrapper<Foo>` gives `T = Foo`.
pub fn impl_subst(imp: &ImplDecl, recv: &Ty) -> Vec<(Name, Ty)> {
    let mut subst = Vec::new();
    if let (Ty::Adt(_, _, args) | Ty::Ext(_, args), TypeExpr::Named { args: iargs, .. }) =
        (recv, &imp.self_ty)
    {
        for (ia, a) in iargs.iter().zip(args) {
            if let TypeExpr::Named { path, args: none } = ia {
                if none.is_empty()
                    && path.0.len() == 1
                    && imp.generics.iter().any(|g| g.name == path.0[0])
                    && *a != Ty::Unknown
                {
                    subst.push((path.0[0].clone(), a.clone()));
                }
            }
        }
    }
    subst
}

/// One thread's resolver: shared tables + per-thread memos.
pub struct Resolver<'t, 'a> {
    pub lang: &'a dyn Lang,
    pub files: &'a [&'a FileFacts],
    pub scopes: Scopes<'t, 'a>,
    pub impls: &'t ImplTables,
    pub(super) local_memo: RefCell<HashMap<(u32, u32), Ty>>,
    /// Types of call expressions, by `(file, expr)`.
    pub(super) expr_memo: RefCell<HashMap<(u32, u32), Ty>>,
    /// Types of unannotated parameters `(file, fn, param)` agreed on by
    /// every resolved call site (see [`super::param_hints`]).
    pub(super) param_hints: Option<&'t HashMap<(u32, u32, u32), Ty>>,
}

impl<'t, 'a> Resolver<'t, 'a> {
    pub fn new(
        lang: &'a dyn Lang,
        files: &'a [&'a FileFacts],
        scopes: Scopes<'t, 'a>,
        impls: &'t ImplTables,
    ) -> Self {
        Resolver {
            lang,
            files,
            scopes,
            impls,
            local_memo: RefCell::new(HashMap::default()),
            expr_memo: RefCell::new(HashMap::default()),
            param_hints: None,
        }
    }

    /// Resolve a call (`Expr::Call` / `Expr::Method`) or a name used as a
    /// value (`Expr::Path` / `Expr::Field`).
    pub fn pick(&self, e: ExprId, cx: &FnCx, at: u32, depth: u32) -> Pick {
        let f = self.files[cx.file as usize];
        match f.expr(e) {
            Expr::Call(callee) => match f.expr(callee) {
                Expr::Path(p) => self.path(p, cx, at, depth),
                _ => Pick::Unknown("callee is not a path"),
            },
            // `recv.name` as a value: a method (bound or not), a module member
            Expr::Method(recv, name) | Expr::Field(recv, name) => {
                // `pkg.F()` (Go): a qualified call, not a method
                if let Some(mut path) = self.module_path(recv, cx, at) {
                    path.push(f.sym(name));
                    return self.path_segs(&path, cx, depth);
                }
                if self.external_receiver(recv, cx, at) {
                    return Pick::External(None);
                }
                let t = self.type_of(recv, cx, at, depth + 1);
                self.method(&t, f.sym(name), depth + 1)
            }
            Expr::Path(p) => self.path(p, cx, at, depth),
            _ => Pick::Unknown("not a call"),
        }
    }

    fn path(&self, p: Span, cx: &FnCx, at: u32, depth: u32) -> Pick {
        let f = self.files[cx.file as usize];
        let segs = f.path(p);
        if segs.len() == 1 && self.local(cx, f.path_pool[p.start as usize], at).is_some() {
            return Pick::Unknown("runtime value");
        }
        if segs.len() == 1 && cx.generics.iter().any(|g| *g.name == *segs[0]) {
            return Pick::Unknown("generic parameter");
        }
        self.path_segs(&segs, cx, depth)
    }

    /// Resolve a (non-local) path to its target.
    fn path_segs(&self, segs: &[&str], cx: &FnCx, depth: u32) -> Pick {
        let first = segs[0];
        if segs.len() == 2 {
            // `Self::f`
            if first == self.lang.self_type() {
                return match &cx.self_ty {
                    Some(t) => self.method(t, segs[1], depth + 1),
                    None => Pick::Unknown("Self outside an impl"),
                };
            }
        }
        // `T::f` on a generic parameter; longer paths through generics or
        // `Self` (`S::Error::f`) go through associated types sem does not model.
        if segs.len() >= 2 {
            if let Some(g) = cx.generics.iter().find(|g| &*g.name == first) {
                if segs.len() > 2 {
                    return Pick::Unknown("associated type path");
                }
                let t = Ty::Param(traits_of(&self.scopes, &g.bounds, cx.scope));
                return self.method(&t, segs[1], depth + 1);
            }
            if first == self.lang.self_type() {
                return Pick::Unknown("associated type path");
            }
        }
        let (mut defs, used) = self.scopes.resolve_path(cx.scope, segs);
        if defs.contains(&Def::External) {
            // a repo definition wins over a same-named import from outside
            // (e.g. an imported macro next to a local fn)
            defs.retain(|d| *d != Def::External);
            if defs.is_empty()
                || used < segs.len()
                    && !defs
                        .iter()
                        .any(|d| matches!(d, Def::Type(..) | Def::Trait(..)))
            {
                return self.external_path(segs, depth);
            }
        }
        if defs.is_empty() {
            return Pick::Unknown("path not found");
        }
        if used == segs.len() {
            // one namespace: functions, else types (constructors), else the
            // rest (values, traits, modules)
            let of = |keep: fn(&Def) -> bool| -> Vec<Def> {
                defs.iter().copied().filter(|d| keep(d)).collect()
            };
            let fns = of(|d| matches!(d, Def::Fn(..)));
            if !fns.is_empty() {
                return Pick::Defs(fns, None);
            }
            let types = of(|d| matches!(d, Def::Type(..)));
            if !types.is_empty() {
                return Pick::Defs(types, Some(self.path_type(&segs[..used], cx, depth)));
            }
            return Pick::Defs(defs, None);
        }
        if used + 1 == segs.len() {
            let member = segs[used];
            if defs.iter().any(|d| matches!(d, Def::Type(..))) {
                let ty = self.path_type(&segs[..used], cx, depth);
                if let Ty::Adt(f, t, _) = &ty {
                    let decl = &self.files[*f as usize].types[*t as usize];
                    if decl.variants.iter().any(|(n, _)| &**n == member) {
                        return Pick::Defs(vec![Def::Type(*f, *t)], Some(ty));
                    }
                }
                return match self.method(&ty, member, depth + 1) {
                    // `Type::new()` of a type without such a fn in the
                    // repo: derived/std trait function returning Self.
                    Pick::External(None) | Pick::Unknown(_)
                        if matches!(member, "default" | "from" | "new" | "clone") =>
                    {
                        Pick::External(Some(ty))
                    }
                    pick => pick,
                };
            }
            if let Some(Def::Trait(f, t)) = defs.iter().find(|d| matches!(d, Def::Trait(..))) {
                return self.trait_member(&[(*f, *t)], member, None);
            }
        }
        Pick::Unknown("path not found")
    }

    /// The type a type path names (aliases expanded).
    pub fn path_type(&self, segs: &[&str], cx: &FnCx, depth: u32) -> Ty {
        let te = TypeExpr::Named {
            path: Path(segs.iter().map(|s| (*s).into()).collect()),
            args: Vec::new(),
        };
        resolve_type(&self.scopes, self.lang, &te, &cx.env(), depth + 1)
    }

    /// A path whose root is outside the repo (`Vec::new`, `std::mem::take`,
    /// `io::Error::custom_ext_fn` from a repo extension trait).
    fn external_path(&self, segs: &[&str], depth: u32) -> Pick {
        if segs.len() >= 2 {
            let ty = segs[segs.len() - 2];
            let f = segs[segs.len() - 1];
            if ty.chars().next().is_some_and(|c| c.is_uppercase()) {
                let ext = Ty::Ext(ty.into(), Vec::new());
                let p = self.method(&ext, f, depth + 1);
                if !matches!(p, Pick::External(_)) {
                    return p;
                }
                // `Default::default()`, `From::from(x)`: which impl runs
                // depends on types sem does not know, and the repo has some
                if self.outside_trait_has(ty, f) {
                    return Pick::Unknown("an outside trait's repo impls may be chosen");
                }
                if matches!(
                    f,
                    "new" | "default" | "from" | "with_capacity" | "new_in" | "from_iter"
                ) {
                    return Pick::External(Some(ext));
                }
            }
        }
        Pick::External(None)
    }

    /// Methods declared by the given traits (or their supertraits).
    fn trait_member(&self, traits: &[(u32, u32)], name: &str, recv: Option<&Ty>) -> Pick {
        let mut hits: Vec<(u32, u32)> = Vec::new();
        for (f, t) in self.impls.trait_closure(traits) {
            for &fi in &self.impls.trait_members[f as usize][t as usize] {
                if &*self.files[f as usize].fns[fi as usize].name == name {
                    hits.push((f, fi));
                }
            }
        }
        hits.sort_unstable();
        hits.dedup();
        if hits.is_empty() {
            return Pick::External(None);
        }
        Pick::fns(hits, recv.cloned())
    }

    /// Resolve method `name` on a receiver of type `t`.
    pub fn method(&self, t: &Ty, name: &str, depth: u32) -> Pick {
        if depth > super::infer::MAX_DEPTH {
            return Pick::Unknown("too deep");
        }
        match t {
            Ty::Adt(f, ty, _) => {
                let impls = self
                    .impls
                    .by_adt
                    .get(&(*f, *ty))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let (inherent, traits): (Vec<_>, Vec<_>) = impls.iter().partition(|k| {
                    !self.impls.impl_trait.contains_key(k)
                        && self.files[k.0 as usize].impls[k.1 as usize]
                            .trait_
                            .is_none()
                });
                // inherent methods shadow trait methods; several hits in a
                // group are `#[cfg]` twins or impls told apart by argument
                // types (`From<A>`, `From<B>`)
                for group in [&inherent, &traits] {
                    let hits = self.members_named(group, name);
                    if !hits.is_empty() {
                        return Pick::fns(hits, Some(t.clone()));
                    }
                }
                // provided (default) methods of the repo traits the type implements
                let implemented: Vec<(u32, u32)> = traits
                    .iter()
                    .filter_map(|k| self.impls.impl_trait.get(k).copied())
                    .collect();
                if !implemented.is_empty() {
                    let p = self.trait_member(&implemented, name, Some(t));
                    if !matches!(p, Pick::External(_)) {
                        return p;
                    }
                }
                if let Some(target) = self.impls.deref_target(self, t) {
                    return self.through_deref(self.method(&target, name, depth + 1));
                }
                if let Some(p) = self.inherited(*f, *ty, t, name, depth) {
                    return p;
                }
                if self.blanket_has(name) {
                    return Pick::Unknown("a blanket impl may apply");
                }
                // a repo iterator has the std iterator's adapters
                if let Some((tr, assoc, std)) = self.lang.iterates_as() {
                    if let Some(item) = self.impls.assoc_type(self, t, tr, assoc) {
                        return self.method(&Ty::Ext(std.into(), vec![item]), name, depth + 1);
                    }
                }
                match self.lang.builtin_method("", name) {
                    Some(rule) => Pick::External(Some(apply_builtin(rule, t))),
                    None => Pick::Unknown("method not found on repo type"),
                }
            }
            Ty::Param(traits) => {
                if traits.is_empty() {
                    return Pick::Unknown("receiver has no known bounds");
                }
                match self.trait_member(traits, name, Some(t)) {
                    Pick::External(None) => match self.lang.builtin_method("", name) {
                        Some(rule) => Pick::External(Some(apply_builtin(rule, t))),
                        None => Pick::External(None),
                    },
                    p => p,
                }
            }
            Ty::Ext(n, args) => {
                if let Some(keys) = self.impls.by_ext.get(n) {
                    let hits = self.members_named(&keys.iter().collect::<Vec<_>>(), name);
                    // repo traits for an outside type are a closed set; an
                    // outside trait also has the outside world's impls
                    if hits.iter().any(|&h| self.of_outside_trait(h)) {
                        return Pick::Unknown("an outside trait's impls compete");
                    }
                    if !hits.is_empty() {
                        return Pick::fns(hits, Some(t.clone()));
                    }
                    // provided methods of the repo traits it implements
                    let implemented: Vec<(u32, u32)> = keys
                        .iter()
                        .filter_map(|k| self.impls.impl_trait.get(k).copied())
                        .collect();
                    let p = self.trait_member(&implemented, name, Some(t));
                    if !implemented.is_empty() && !matches!(p, Pick::External(_)) {
                        return p;
                    }
                }
                if self.blanket_has(name) {
                    return Pick::Unknown("a blanket impl may apply");
                }
                if let Some(rule) = self.lang.builtin_method(n, name) {
                    return Pick::External(Some(apply_builtin(rule, t)));
                }
                if let Some(rule) = self.lang.deref(n) {
                    let inner = apply_builtin(rule, &Ty::Ext(n.clone(), args.clone()));
                    if inner != Ty::Unknown {
                        let p = self.through_deref(self.method(&inner, name, depth + 1));
                        if !matches!(p, Pick::External(_)) {
                            return p;
                        }
                    }
                }
                Pick::External(None)
            }
            // one of several types: the union of each one's answer
            Ty::Union(ts) => {
                let mut defs: Vec<Def> = Vec::new();
                let mut outside = 0;
                for t in ts {
                    match self.method(t, name, depth + 1) {
                        Pick::Defs(d, _) => defs.extend(d),
                        Pick::External(_) => outside += 1,
                        p @ Pick::Unknown(_) => return p,
                    }
                }
                match (defs.is_empty(), outside) {
                    (true, _) => Pick::External(None),
                    (false, 0) => {
                        defs.sort_unstable();
                        defs.dedup();
                        Pick::Defs(defs, None)
                    }
                    _ => Pick::Unknown("a union member is outside the repo"),
                }
            }
            Ty::Fn(_) => Pick::External(None),
            Ty::Super(f, ty) => self
                .inherited(*f, *ty, t, name, depth)
                .unwrap_or(Pick::Unknown("method not found on repo type")),
            Ty::Unknown => Pick::Unknown("unknown receiver type"),
        }
    }

    /// A method of embedded types (Go) / base classes (Python) of type
    /// `(f, ty)`: whichever wins (the shallowest embedding, the first in
    /// the MRO) is the nearest hit through one of them, so the answer is
    /// every base's nearest hit — unless a base that may define it too
    /// (outside the repo, or with such a base itself) comes first. `None`
    /// when no base has it.
    pub fn inherited(&self, f: u32, ty: u32, t: &Ty, name: &str, depth: u32) -> Option<Pick> {
        let decl = &self.files[f as usize].types[ty as usize];
        let env = TyEnv {
            scope: self.scopes.global(f, decl.scope),
            generics: &decl.generics,
            self_ty: None,
            subst: &[],
        };
        let mut found: Vec<Def> = Vec::new();
        let mut opaque = false;
        for e in &decl.embeds {
            let et = resolve_type(&self.scopes, self.lang, e, &env, depth + 1);
            if matches!(&et, Ty::Ext(n, _) if self.lang.neutral_base(n)) {
                continue;
            }
            match self.method(&et, name, depth + 1) {
                Pick::Defs(..) if opaque => {
                    return Some(Pick::Unknown("a base outside the repo may define it"));
                }
                Pick::Defs(d, _) => found.extend(d),
                // absent from a base whose whole ancestry is in the repo
                Pick::Unknown("method not found on repo type") => {}
                _ => opaque = true,
            }
        }
        // without an order (Go), an outer embedding may be shallower
        if opaque && (found.is_empty() || !self.lang.ordered_bases()) {
            return Some(Pick::Unknown("a base outside the repo may define it"));
        }
        if found.is_empty() {
            return None;
        }
        found.sort_unstable();
        found.dedup();
        Some(Pick::Defs(found, Some(t.clone())))
    }

    /// A method found through a smart pointer / `Deref`: when it implements
    /// an outside trait (`fmt`, `clone`, `serialize`), the wrapper itself
    /// may implement (or derive) that trait and win.
    fn through_deref(&self, p: Pick) -> Pick {
        match &p {
            Pick::Defs(d, _)
                if d.iter()
                    .any(|d| matches!(*d, Def::Fn(f, i) if self.of_outside_trait((f, i)))) =>
            {
                Pick::Unknown("the wrapper may implement this trait itself")
            }
            _ => p,
        }
    }

    /// Function `(file, fn)` is a member of an impl of a trait from outside
    /// the repo.
    fn of_outside_trait(&self, (f, i): (u32, u32)) -> bool {
        match self.files[f as usize].fns[i as usize].owner {
            Owner::Impl(k) => {
                self.files[f as usize].impls[k as usize].trait_.is_some()
                    && !self.impls.impl_trait.contains_key(&(f, k))
            }
            _ => false,
        }
    }

    /// A blanket impl (`impl<T: B> Trait for T`) has a method `name`, or
    /// its trait provides one.
    fn blanket_has(&self, name: &str) -> bool {
        let keys: Vec<&(u32, u32)> = self.impls.blanket.iter().collect();
        if !self.members_named(&keys, name).is_empty() {
            return true;
        }
        let traits: Vec<(u32, u32)> = keys
            .iter()
            .filter_map(|k| self.impls.impl_trait.get(k).copied())
            .collect();
        !traits.is_empty() && !matches!(self.trait_member(&traits, name, None), Pick::External(_))
    }

    /// Some repo impl of the outside trait named `name` has a member `f`.
    fn outside_trait_has(&self, name: &str, f: &str) -> bool {
        let keys = self
            .impls
            .by_ext_trait
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        !self
            .members_named(&keys.iter().collect::<Vec<_>>(), f)
            .is_empty()
    }

    fn members_named(&self, impls: &[&(u32, u32)], name: &str) -> Vec<(u32, u32)> {
        let mut hits = Vec::new();
        for &&(f, i) in impls {
            for &fi in &self.impls.impl_members[f as usize][i as usize] {
                if &*self.files[f as usize].fns[fi as usize].name == name {
                    hits.push((f, fi));
                }
            }
        }
        hits
    }
}
