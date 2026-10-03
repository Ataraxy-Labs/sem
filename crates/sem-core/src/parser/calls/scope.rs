//! Stage 2: name resolution. Builds one scope table over every file's
//! [`FileFacts`] (declared items + imports per scope, wired into a module
//! tree by the language's [`Layout`]) and answers "what does path `a::b::c`
//! mean in scope `s`?" — exactly one of: repo definitions, `External` (the
//! first segment is not defined anywhere in the repo: std, prelude, a
//! dependency), or nothing (the path starts in the repo but does not lead to
//! a definition sem knows).

use std::cell::{Cell, RefCell};

use rustc_hash::FxHashMap as HashMap;

use super::ir::*;
use super::lang::Layout;

/// A definition a name can bind to. File-relative indices keep this `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Def {
    Fn(u32, u32),
    Type(u32, u32),
    Trait(u32, u32),
    /// A `const` / `static` item.
    Value(u32, u32),
    /// A module, by global scope id.
    Module(u32),
    /// Defined outside the repo.
    External,
}

/// The shared, immutable scope tables.
pub struct ScopeTables<'a> {
    pub files: &'a [&'a FileFacts],
    base: Vec<u32>,
    file_of: Vec<u32>,
    /// Block scopes fall back to their lexical parent; modules do not.
    lexical_parent: Vec<Option<u32>>,
    /// The scope a module is declared in (`super`).
    module_parent: Vec<Option<u32>>,
    is_block: Vec<bool>,
    /// Directory modules (packages).
    is_dir: Vec<bool>,
    items: Vec<HashMap<&'a str, Vec<Def>>>,
    uses: Vec<Vec<&'a UseDecl>>,
    crates: HashMap<String, u32>,
    /// Names declared privately in a scope, by global scope.
    private: rustc_hash::FxHashSet<(u32, &'a str)>,
    /// Modules each scope glob-imports (`use m::*`), resolved.
    glob_targets: HashMap<u32, Vec<u32>>,
    /// What a glob-imported module exposes to other modules: its public
    /// items and re-exports, and (not shadowed by those) what its public
    /// globs expose.
    surfaces_pub: HashMap<u32, Names<'a>>,
    /// What it exposes to its own descendants (`use super::*`): private
    /// names too.
    surfaces_all: HashMap<u32, Names<'a>>,
}

/// `(scope, name)` -> definitions; `None` while the lookup is in progress.
type Memo = RefCell<HashMap<(u32, Box<str>), Option<Vec<Def>>>>;

/// A per-thread view over [`ScopeTables`] with its own lookup memo.
pub struct Scopes<'t, 'a> {
    t: &'t ScopeTables<'a>,
    /// `None` marks a lookup in progress (glob imports can be cyclic).
    memo: Memo,
    /// Set when a lookup was cut short (cycle or depth limit): its result,
    /// and every enclosing one, may be incomplete and is not memoized, so
    /// answers never depend on lookup order.
    truncated: Cell<bool>,
}

const MAX_DEPTH: u32 = 16;

/// Definitions by name.
type Names<'a> = HashMap<&'a str, Vec<Def>>;

/// Add `defs` for `name` unless `own` already binds `name` in their
/// namespace (explicit names shadow glob-imported ones per namespace).
fn merge_shadowed<'a>(own: &mut Names<'a>, local: &Names<'a>, name: &'a str, defs: &[Def]) {
    let (has_value, has_type) = namespaces(local.get(name).map(Vec::as_slice).unwrap_or(&[]));
    let slot = own.entry(name).or_default();
    for d in defs {
        let (v, ty) = namespaces(std::slice::from_ref(d));
        if ((v && !has_value) || (ty && !has_type)) && !slot.contains(d) {
            slot.push(*d);
        }
    }
}

/// Surfaces of glob-imported modules: a module's own names plus, not
/// shadowed by them, everything its glob imports expose, transitively.
/// Tarjan's SCCs make one pass enough: a component's modules see the union
/// of the component's own names and of the surfaces it globs from.
fn propagate_globs<'a>(
    all: &[u32],
    globs: &HashMap<u32, Vec<u32>>,
    local: HashMap<u32, Names<'a>>,
) -> HashMap<u32, Names<'a>> {
    struct Tarjan<'g> {
        globs: &'g HashMap<u32, Vec<u32>>,
        index: HashMap<u32, (u32, u32)>, // (index, lowlink)
        stack: Vec<u32>,
        on_stack: HashMap<u32, bool>,
        next: u32,
        sccs: Vec<Vec<u32>>, // in reverse topological order
    }
    impl Tarjan<'_> {
        fn visit(&mut self, v: u32) {
            self.index.insert(v, (self.next, self.next));
            self.next += 1;
            self.stack.push(v);
            self.on_stack.insert(v, true);
            for &w in self.globs.get(&v).into_iter().flatten() {
                if !self.index.contains_key(&w) {
                    self.visit(w);
                    let low = self.index[&w].1.min(self.index[&v].1);
                    self.index.get_mut(&v).unwrap().1 = low;
                } else if self.on_stack.get(&w) == Some(&true) {
                    let low = self.index[&w].0.min(self.index[&v].1);
                    self.index.get_mut(&v).unwrap().1 = low;
                }
            }
            if self.index[&v].0 == self.index[&v].1 {
                let mut scc = Vec::new();
                while let Some(w) = self.stack.pop() {
                    self.on_stack.insert(w, false);
                    scc.push(w);
                    if w == v {
                        break;
                    }
                }
                self.sccs.push(scc);
            }
        }
    }
    let mut t = Tarjan {
        globs,
        index: HashMap::default(),
        stack: Vec::new(),
        on_stack: HashMap::default(),
        next: 0,
        sccs: Vec::new(),
    };
    for &v in all {
        if !t.index.contains_key(&v) {
            t.visit(v);
        }
    }
    let empty = Names::default();
    let mut surfaces: HashMap<u32, Names<'a>> = HashMap::default();
    for scc in t.sccs {
        // names flowing into the component: its members' own names, and
        // the (finished) surfaces of the modules it globs from
        let mut inflow: Names<'a> = Names::default();
        for &m in &scc {
            for (n, defs) in local.get(&m).unwrap_or(&empty) {
                merge_shadowed(&mut inflow, &empty, n, defs);
            }
            for g in globs.get(&m).into_iter().flatten() {
                if scc.contains(g) {
                    continue;
                }
                for (n, defs) in surfaces.get(g).unwrap_or(&empty) {
                    merge_shadowed(&mut inflow, &empty, n, defs);
                }
            }
        }
        for &m in &scc {
            let own = local.get(&m).unwrap_or(&empty);
            let mut names = own.clone();
            for (n, defs) in &inflow {
                merge_shadowed(&mut names, own, n, defs);
            }
            names.retain(|_, defs| !defs.is_empty());
            for defs in names.values_mut() {
                defs.sort_unstable(); // order-free, so rounds compare equal
                defs.dedup();
            }
            surfaces.insert(m, names);
        }
    }
    surfaces
}

/// Which namespaces some definitions occupy: (values, types/modules).
/// `External` could be either.
fn namespaces(defs: &[Def]) -> (bool, bool) {
    let value = defs
        .iter()
        .any(|d| matches!(d, Def::Fn(..) | Def::Value(..) | Def::External));
    let ty = defs.iter().any(|d| {
        matches!(
            d,
            Def::Type(..) | Def::Trait(..) | Def::Module(_) | Def::External
        )
    });
    (value, ty)
}

impl<'a> ScopeTables<'a> {
    pub fn view(&self) -> Scopes<'_, 'a> {
        Scopes {
            t: self,
            memo: RefCell::new(HashMap::default()),
            truncated: Cell::new(false),
        }
    }

    pub fn build(files: &'a [&'a FileFacts], layout: &'a Layout) -> Self {
        let mut base = Vec::with_capacity(files.len());
        let mut n = 0u32;
        for f in files {
            base.push(n);
            n += f.scopes.len() as u32;
        }
        let dir_base = n;
        n += layout.dirs.len() as u32;
        let total = n as usize;
        let mut file_of = vec![0u32; total];
        let mut lexical_parent = vec![None; total];
        let mut module_parent = vec![None; total];
        let mut is_block = vec![false; total];
        let mut is_dir = vec![false; total];
        for d in 0..layout.dirs.len() {
            is_dir[dir_base as usize + d] = true;
        }
        let mut items: Vec<HashMap<&'a str, Vec<Def>>> = vec![HashMap::default(); total];
        let mut private: rustc_hash::FxHashSet<(u32, &'a str)> = Default::default();
        let mut uses: Vec<Vec<&'a UseDecl>> = vec![Vec::new(); total];

        let mut child_file: HashMap<(usize, u32), usize> = HashMap::default();
        for (ci, p) in layout.parent_of.iter().enumerate() {
            if let Some(p) = p {
                child_file.insert(*p, ci);
            }
        }

        for (fi, f) in files.iter().enumerate() {
            let b = base[fi];
            for (li, s) in f.scopes.iter().enumerate() {
                let g = b + li as u32;
                file_of[g as usize] = fi as u32;
                if li == 0 {
                    // a file module's parent is the scope its `mod m;` sits in
                    module_parent[g as usize] = layout.parent_of[fi].and_then(|(pf, ps)| {
                        files[pf].scopes[ps as usize].parent.map(|pp| base[pf] + pp)
                    });
                    continue;
                }
                let parent = s.parent.map(|p| b + p);
                match &s.name {
                    None => {
                        is_block[g as usize] = true;
                        lexical_parent[g as usize] = parent;
                    }
                    Some(name) => {
                        let target = if s.out_of_line {
                            child_file.get(&(fi, li as u32)).map(|&cf| base[cf])
                        } else {
                            module_parent[g as usize] = parent;
                            Some(g)
                        };
                        if let (Some(p), Some(t)) = (parent, target) {
                            items[p as usize]
                                .entry(&**name)
                                .or_default()
                                .push(Def::Module(t));
                        }
                    }
                }
            }
            // Where scope `local` of this file keeps its items: a pooled
            // package file's top level lives in its directory's scope.
            let pooled_dir = match (&layout.orphan_dir[fi], layout.pooled.get(fi)) {
                (Some((d, _)), Some(true)) => Some(dir_base + *d as u32),
                _ => None,
            };
            let home = |local: u32| match (local, pooled_dir) {
                (0, Some(d)) => d,
                _ => b + local,
            };
            for (i, func) in f.fns.iter().enumerate() {
                if func.owner == Owner::Free {
                    items[home(func.scope) as usize]
                        .entry(&*func.name)
                        .or_default()
                        .push(Def::Fn(fi as u32, i as u32));
                }
            }
            for (i, t) in f.types.iter().enumerate() {
                items[home(t.scope) as usize]
                    .entry(&*t.name)
                    .or_default()
                    .push(Def::Type(fi as u32, i as u32));
            }
            for (i, t) in f.traits.iter().enumerate() {
                items[home(t.scope) as usize]
                    .entry(&*t.name)
                    .or_default()
                    .push(Def::Trait(fi as u32, i as u32));
            }
            for (i, v) in f.values.iter().enumerate() {
                items[home(v.scope) as usize]
                    .entry(&*v.name)
                    .or_default()
                    .push(Def::Value(fi as u32, i as u32));
            }
            for u in &f.uses {
                uses[(b + u.scope) as usize].push(u);
            }
            for (sc, name) in &f.private {
                private.insert((home(*sc), &**name));
            }
        }
        // directory modules (partial inputs): each lists its subdirectories
        // and its orphan files, by name
        for (d, (name, parent)) in layout.dirs.iter().enumerate() {
            let g = dir_base + d as u32;
            if let Some(p) = parent {
                let pg = dir_base + *p as u32;
                module_parent[g as usize] = Some(pg);
                items[pg as usize]
                    .entry(name.as_str())
                    .or_default()
                    .push(Def::Module(g));
            }
        }
        for (fi, dir) in layout.orphan_dir.iter().enumerate() {
            if let Some((d, name)) = dir {
                let g = dir_base + *d as u32;
                if layout.pooled.get(fi) == Some(&true) {
                    // the file scope only holds its imports; names fall
                    // back to the package
                    is_block[base[fi] as usize] = true;
                    lexical_parent[base[fi] as usize] = Some(g);
                    continue;
                }
                module_parent[base[fi] as usize] = Some(g);
                items[g as usize]
                    .entry(name.as_str())
                    .or_default()
                    .push(Def::Module(base[fi]));
            }
        }
        let crates = layout
            .crates
            .iter()
            .map(|(name, &fi)| (name.clone(), base[fi]))
            .chain(
                layout
                    .dir_crates
                    .iter()
                    .map(|(name, &d)| (name.clone(), dir_base + d as u32)),
            )
            .collect();
        let mut tables = ScopeTables {
            files,
            base,
            file_of,
            lexical_parent,
            module_parent,
            is_block,
            is_dir,
            items,
            uses,
            crates,
            private,
            glob_targets: HashMap::default(),
            surfaces_pub: HashMap::default(),
            surfaces_all: HashMap::default(),
        };
        tables.resolve_globs();
        tables
    }

    /// Glob imports as a fixed point (the usual import-resolution loop).
    /// Each round resolves every `use m::*` and every explicit import of a
    /// glob-imported module against the previous round's surfaces, then
    /// propagates glob re-exports to completion in one pass over the glob
    /// graph's strongly connected components (a cycle of `pub use x::*`
    /// shares one set of names). Rounds repeat only while explicit imports
    /// still resolve differently — usually two or three.
    fn resolve_globs(&mut self) {
        for _round in 0..8 {
            let (targets, surfaces_pub, surfaces_all) = {
                let view = self.view();
                let mut targets: HashMap<u32, Vec<u32>> = HashMap::default();
                let mut public_globs: HashMap<u32, Vec<u32>> = HashMap::default();
                for (s, uses) in self.uses.iter().enumerate() {
                    for u in uses.iter().filter(|u| u.glob) {
                        let (defs, used) = view.resolve_path(s as u32, &u.path.0);
                        if used != u.path.0.len() {
                            continue;
                        }
                        let mods = defs.iter().filter_map(|d| match d {
                            Def::Module(m) => Some(*m),
                            _ => None,
                        });
                        let mods: Vec<u32> = mods.collect();
                        for (map, take) in [(&mut targets, true), (&mut public_globs, u.public)] {
                            if take {
                                let entry = map.entry(s as u32).or_default();
                                entry.extend(mods.iter().copied());
                                entry.sort_unstable();
                                entry.dedup();
                            }
                        }
                    }
                }
                let mut all: Vec<u32> = targets.values().flatten().copied().collect();
                all.sort_unstable();
                all.dedup();
                // what each glob-imported module declares or imports by name,
                // all of it and just its public part
                let mut local_all: HashMap<u32, Names<'a>> = HashMap::default();
                let mut local_pub: HashMap<u32, Names<'a>> = HashMap::default();
                for &t in &all {
                    let mut names: Names<'a> = self.items[t as usize].clone();
                    let mut public: Names<'a> = names
                        .iter()
                        .filter(|(n, _)| !self.private.contains(&(t, **n)))
                        .map(|(n, d)| (*n, d.clone()))
                        .collect();
                    for u in self.uses[t as usize].iter().filter(|u| !u.glob) {
                        if let Some(n) = u.name.as_deref() {
                            let defs = view.declared_in(t, n, 0);
                            if u.public {
                                public.insert(n, defs.clone());
                            }
                            names.insert(n, defs);
                        }
                    }
                    local_all.insert(t, names);
                    local_pub.insert(t, public);
                }
                let surfaces_pub = propagate_globs(&all, &public_globs, local_pub);
                // a descendant sees its ancestor's private names; any other
                // importer, the public surface (final by now)
                let empty = Names::default();
                let mut ancestor_globs: HashMap<u32, Vec<u32>> = HashMap::default();
                for &t in &all {
                    let own = local_all.get(&t).cloned().unwrap_or_default();
                    let mut names = own.clone();
                    for &g in targets.get(&t).into_iter().flatten() {
                        if view.descends(t, g) {
                            ancestor_globs.entry(t).or_default().push(g);
                        } else {
                            for (n, defs) in surfaces_pub.get(&g).unwrap_or(&empty) {
                                merge_shadowed(&mut names, &own, n, defs);
                            }
                        }
                    }
                    local_all.insert(t, names);
                }
                let surfaces_all = propagate_globs(&all, &ancestor_globs, local_all);
                (targets, surfaces_pub, surfaces_all)
            };
            let changed = targets != self.glob_targets
                || surfaces_pub != self.surfaces_pub
                || surfaces_all != self.surfaces_all;
            self.glob_targets = targets;
            self.surfaces_pub = surfaces_pub;
            self.surfaces_all = surfaces_all;
            if !changed {
                break;
            }
        }
    }
}

impl<'t, 'a> Scopes<'t, 'a> {
    pub fn files(&self) -> &'a [&'a FileFacts] {
        self.t.files
    }

    pub fn global(&self, file: u32, local: u32) -> u32 {
        self.t.base[file as usize] + local
    }

    pub fn file_of(&self, scope: u32) -> u32 {
        self.t.file_of[scope as usize]
    }

    pub fn module_of(&self, mut s: u32) -> u32 {
        while self.t.is_block[s as usize] {
            match self.t.lexical_parent[s as usize] {
                Some(p) => s = p,
                None => break,
            }
        }
        s
    }

    /// `s` lies within module `ancestor` (or is it).
    pub fn descends(&self, s: u32, ancestor: u32) -> bool {
        let mut m = self.module_of(s);
        for _ in 0..64 {
            if m == ancestor {
                return true;
            }
            match self.t.module_parent[m as usize] {
                Some(p) => m = self.module_of(p),
                None => return false,
            }
        }
        false
    }

    fn super_of(&self, s: u32) -> Option<u32> {
        self.t.module_parent[self.module_of(s) as usize].map(|p| self.module_of(p))
    }

    fn crate_root(&self, s: u32) -> u32 {
        let mut m = self.module_of(s);
        let mut guard = 0;
        while let Some(p) = self.super_of(m) {
            m = p;
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        m
    }

    /// Resolve a single name as seen from scope `s` (walking out of block
    /// scopes), then as a crate name. Empty when the repo does not define it.
    pub fn lookup(&self, s: u32, name: &str) -> Vec<Def> {
        self.lookup_depth(s, name, 0)
    }

    fn lookup_depth(&self, s: u32, name: &str, depth: u32) -> Vec<Def> {
        let mut cur = s;
        loop {
            let d = self.lookup_in(cur, name, depth);
            if !d.is_empty() {
                return d;
            }
            match (
                self.t.is_block[cur as usize],
                self.t.lexical_parent[cur as usize],
            ) {
                (true, Some(p)) => cur = p,
                _ => break,
            }
        }
        match self.t.crates.get(name) {
            Some(&root) => vec![Def::Module(root)],
            None => Vec::new(),
        }
    }

    /// Names defined in or imported into exactly scope `s`: its items and
    /// explicit imports, else what its glob imports' surfaces provide.
    fn lookup_in(&self, s: u32, name: &str, depth: u32) -> Vec<Def> {
        if depth > MAX_DEPTH {
            self.truncated.set(true);
            return Vec::new();
        }
        let key = (s, Box::<str>::from(name));
        match self.memo.borrow().get(&key) {
            Some(Some(hit)) => return hit.clone(),
            Some(None) => {
                // re-entered through the path of an explicit import
                // (`mod m; use m::m;`): what the scope declares itself is
                // bound regardless; imports and globs are still in progress
                self.truncated.set(true);
                return self.t.items[s as usize]
                    .get(name)
                    .cloned()
                    .unwrap_or_default();
            }
            None => {}
        }
        self.memo.borrow_mut().insert(key.clone(), None);
        let outer_truncated = self.truncated.replace(false);

        // Explicit names shadow glob imports per namespace: a module `div`
        // declared here does not hide a glob-imported function `div`.
        let mut out = self.declared_in(s, name, depth);
        let (has_value, has_type) = namespaces(&out);
        if !(has_value && has_type) {
            for &t in self.t.glob_targets.get(&s).into_iter().flatten() {
                let surfaces = if self.descends(s, t) {
                    &self.t.surfaces_all
                } else {
                    &self.t.surfaces_pub
                };
                if let Some(defs) = surfaces.get(&t).and_then(|sf| sf.get(name)) {
                    out.extend(defs.iter().copied().filter(|d| {
                        let (v, ty) = namespaces(std::slice::from_ref(d));
                        (v && !has_value) || (ty && !has_type)
                    }));
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        let truncated = self.truncated.get();
        self.truncated.set(outer_truncated || truncated);
        if truncated {
            self.memo.borrow_mut().remove(&key);
        } else {
            self.memo.borrow_mut().insert(key, Some(out.clone()));
        }
        out
    }

    /// Items named `name` in scope `m` and its explicit imports of `name`
    /// (both: a module and a function may share a name, `mod m; use m::m;`).
    fn declared_in(&self, m: u32, name: &str, depth: u32) -> Vec<Def> {
        let mut out: Vec<Def> = self.t.items[m as usize]
            .get(name)
            .cloned()
            .unwrap_or_default();
        for u in &self.t.uses[m as usize] {
            if !u.glob && u.name.as_deref() == Some(name) {
                let (defs, used) = self.resolve_path_depth(m, &u.path.0, depth + 1);
                if defs.contains(&Def::External) {
                    out.push(Def::External);
                } else if used == u.path.0.len() {
                    out.extend(defs);
                }
            }
        }
        out
    }

    /// Resolve a path. Returns the definitions reached and how many segments
    /// were consumed: fewer than `segs.len()` means the walk stopped at a
    /// non-module (the rest are associated items) or failed (empty defs).
    pub fn resolve_path<S: AsRef<str>>(&self, s: u32, segs: &[S]) -> (Vec<Def>, usize) {
        self.resolve_path_depth(s, segs, 0)
    }

    fn resolve_path_depth<S: AsRef<str>>(
        &self,
        s: u32,
        segs: &[S],
        depth: u32,
    ) -> (Vec<Def>, usize) {
        if segs.is_empty() || depth > MAX_DEPTH {
            return (Vec::new(), 0);
        }
        let mut cur: Vec<Def> = match segs[0].as_ref() {
            "crate" => vec![Def::Module(self.crate_root(s))],
            "self" => vec![Def::Module(self.module_of(s))],
            "super" => self.super_of(s).map(Def::Module).into_iter().collect(),
            // the package the current module belongs to (Python `from . import`)
            "^" => {
                let m = self.module_of(s);
                if self.t.is_dir[m as usize] {
                    vec![Def::Module(m)]
                } else {
                    self.super_of(s).map(Def::Module).into_iter().collect()
                }
            }
            first => {
                let d = self.lookup_depth(s, first, depth);
                if d.is_empty() {
                    vec![Def::External]
                } else {
                    d
                }
            }
        };
        let mut i = 1;
        while i < segs.len() {
            let mods: Vec<u32> = cur
                .iter()
                .filter_map(|d| match d {
                    Def::Module(m) => Some(*m),
                    _ => None,
                })
                .collect();
            if mods.is_empty() {
                break;
            }
            let seg = segs[i].as_ref();
            let mut next = Vec::new();
            for m in mods {
                if seg == "super" {
                    next.extend(self.super_of(m).map(Def::Module));
                } else if seg == "self" {
                    next.push(Def::Module(m));
                } else {
                    next.extend(self.lookup_in(m, seg, depth + 1));
                }
            }
            next.sort_unstable();
            next.dedup();
            if next.is_empty() {
                return (Vec::new(), i);
            }
            cur = next;
            i += 1;
        }
        (cur, i)
    }
}
