//! The language-neutral intermediate representation a language front end
//! lowers one file into. Everything downstream (scopes, type inference,
//! target selection) reads only these types, never a syntax tree, so a new
//! language plugs in by producing a [`FileFacts`] — nothing else changes.
//!
//! The IR is deliberately small: just enough of a file's *declarations*
//! (modules, imports, functions, types, traits, impls) and of each function's
//! *expressions in call position* to answer "which declaration does this
//! call site bind to?". Anything the front end cannot lower becomes
//! [`Expr::Unknown`] / [`TypeExpr::Unknown`], which downstream turns into an
//! explicit `Unresolved` outcome rather than a guess.
//!
//! Expressions are the high-volume part (every call site, every `let`), so
//! they live in a per-file arena: nodes refer to each other and to interned
//! identifiers by `u32` index, and a call chain's sites share its prefix.

pub type Name = Box<str>;

/// A `::`-separated path as written: `Foo`, `crate::a::b`, `Self::new`.
/// Generic arguments are not part of the path (they live on [`TypeExpr`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Path(pub Vec<Name>);

impl Path {
    pub fn single(name: &str) -> Self {
        Path(vec![name.into()])
    }
    pub fn last(&self) -> &str {
        self.0.last().map(|s| &**s).unwrap_or("")
    }
    pub fn first(&self) -> &str {
        self.0.first().map(|s| &**s).unwrap_or("")
    }
}

/// A type as written in source.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    Named {
        path: Path,
        args: Vec<TypeExpr>,
    },
    /// `&T`, `&mut T`, `*const T` — transparent for method lookup.
    Ref(Box<TypeExpr>),
    /// `dyn A + B` / `impl A + B`: only the trait bounds are known.
    Traits(Vec<Path>),
    /// `[T]`, `[T; N]`.
    Slice(Box<TypeExpr>),
    /// `(A, B)`
    Tuple(Vec<TypeExpr>),
    /// A callable: `Fn(A, B) -> R`, `impl FnOnce(A)`, `fn(A) -> R`.
    Fn(Vec<TypeExpr>, Option<Box<TypeExpr>>),
    /// One of several types: `A | B`, `Union[A, B]`.
    Union(Vec<TypeExpr>),
    Unknown,
}

impl TypeExpr {
    /// Some part is left to inference (`Vec<_>`).
    pub fn has_hole(&self) -> bool {
        match self {
            TypeExpr::Unknown => true,
            TypeExpr::Named { args, .. } | TypeExpr::Tuple(args) | TypeExpr::Union(args) => {
                args.iter().any(TypeExpr::has_hole)
            }
            TypeExpr::Ref(t) | TypeExpr::Slice(t) => t.has_hole(),
            TypeExpr::Traits(_) | TypeExpr::Fn(..) => false,
        }
    }
}

/// An expression: an index into [`FileFacts::exprs`]. `ExprId::UNKNOWN`
/// (slot 0) is [`Expr::Unknown`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExprId(pub u32);

impl ExprId {
    pub const UNKNOWN: ExprId = ExprId(0);
}

/// An identifier: an index into [`FileFacts::syms`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sym(pub u32);

/// `len` consecutive entries from `start` in one of the file's pools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub len: u32,
}

/// An expression, lowered only as far as type inference and call-target
/// selection need.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Expr {
    /// `x`, `self`, `Foo::bar`, `crate::m::f` (segments in `path_pool`).
    Path(Span),
    /// `recv.field`
    Field(ExprId, Sym),
    /// `callee(..)` — the callee is almost always a [`Expr::Path`].
    Call(ExprId),
    /// `recv.method(..)`
    Method(ExprId, Sym),
    /// `e?`
    Unwrap(ExprId),
    /// `*e`
    Deref(ExprId),
    /// What a destructuring pattern binds: field `field` (`0`, `1`, .. or a
    /// name) of the `ctor` variant/struct (last path segment; empty for a
    /// tuple) of the value `e`. `Some(x) = e` is `Payload(e, Some, 0)`.
    Payload(ExprId, Sym, Sym),
    /// The element type of an iterable (`for x in e`) or of an index (`e[i]`).
    Elem(ExprId),
    /// `(a, b)` (elements in `expr_pool`).
    Tuple(Span),
    /// An external generic type applied to expressions' types: `Some(x)`
    /// is `Ext(Option, [x])`, `vec![x]` is `Ext(Vec, [x])`.
    Ext(Sym, Span),
    /// `match`/`if` arms (each an [`Expr::At`]) of a statically typed
    /// language: all have one type, so whichever arm is typed gives it.
    Branches(Span),
    /// Alternatives that may differ in type (a dynamic language's
    /// `a if c else b`, `a or b`; `super()`'s bases): any one of them.
    Union(Span),
    /// A closure or lambda: calling it evaluates the body (an [`Expr::At`]).
    Closure(ExprId),
    /// `super()` in a method of this file's type `t`: its bases.
    Super(u32),
    /// A call of a builtin (`sorted(xs)`, `super()`): when the call names
    /// nothing in the repo, its value is the second expression's.
    Builtin(ExprId, ExprId),
    /// An expression evaluated at its own position (arm bindings in scope).
    At(u32, ExprId),
    /// `Foo { .. }`
    Struct(Span),
    /// A value whose type is written down (`e as T`, literals): an index
    /// into `type_pool`.
    Typed(u32),
    /// Parameter `k` of the closure passed as argument `pos` of `call`
    /// (typed from the callee's declared parameter type).
    Arg(ExprId, u8, u8),
    /// The current function's unannotated parameter `k` (typed, when the
    /// language opts in, by what every resolved call site passes).
    Param(u32),
    Unknown,
}

/// A generic parameter and its trait bounds (from `<T: A>` and `where T: A`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Generic {
    pub name: Name,
    pub bounds: Vec<Path>,
    /// A callable bound (`F: FnOnce(A) -> R`), as a [`TypeExpr::Fn`].
    pub sig: Option<TypeExpr>,
}

/// What a function is a member of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Free,
    Impl(u32),
    Trait(u32),
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: Name,
    /// 0-based row of the declaration's first line, used to map the
    /// declaration onto sem's entity for it.
    pub row: u32,
    pub scope: u32,
    pub owner: Owner,
    pub generics: Vec<Generic>,
    /// Declared types of the non-receiver parameters, in order.
    pub params: Vec<TypeExpr>,
    /// Takes `self`: method-call arguments start at `params[0]`.
    pub has_self: bool,
    /// The function this one is nested in, whose locals it can see (Python
    /// closures); `None` for top-level functions and languages whose nested
    /// functions capture nothing.
    pub enclosing: Option<u32>,
    pub ret: Option<TypeExpr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    /// struct / union: `fields` are its named (or positional `0`, `1`) fields.
    Struct,
    /// enum: `variants` are its variants.
    Enum,
    /// `type A<..> = T;`
    Alias(TypeExpr),
    /// A new type with `T`'s structure but its own methods (Go `type A T`):
    /// indexing and iterating it are `T`'s.
    Defined(TypeExpr),
}

#[derive(Clone, Debug)]
pub struct TypeDecl {
    pub name: Name,
    pub row: u32,
    pub scope: u32,
    pub generics: Vec<Generic>,
    pub kind: TypeKind,
    pub fields: Vec<(Name, TypeExpr)>,
    /// Enum variants with their payload fields (named, or `0`, `1`, ..).
    pub variants: Vec<(Name, Vec<(Name, TypeExpr)>)>,
    /// Embedded types whose methods are promoted (Go struct embedding), or
    /// base classes (Python).
    pub embeds: Vec<TypeExpr>,
    /// Attributes without a declared type, typed by an initializer
    /// (`self.x = Foo()` in a method): `(name, method, scope, initializer)`.
    pub field_inits: Vec<(Name, u32, u32, ExprId)>,
}

/// A module-level value (`const`, `static`, Go `var`): its declared type,
/// or else the initializer it is typed from.
#[derive(Clone, Debug)]
pub struct ValueDecl {
    pub name: Name,
    pub row: u32,
    pub scope: u32,
    pub ty: Option<TypeExpr>,
    pub init: Option<ExprId>,
}

#[derive(Clone, Debug)]
pub struct TraitDecl {
    pub name: Name,
    pub row: u32,
    pub scope: u32,
    pub generics: Vec<Generic>,
    pub supertraits: Vec<Path>,
    /// Associated types and their bounds (`type Item: Display;`).
    pub assoc: Vec<Generic>,
}

#[derive(Clone, Debug)]
pub struct ImplDecl {
    pub scope: u32,
    pub generics: Vec<Generic>,
    pub self_ty: TypeExpr,
    pub trait_: Option<Path>,
    /// `type Target = T;` inside `impl Deref for ..`, and other associated
    /// types, by name.
    pub assoc_types: Vec<(Name, TypeExpr)>,
}

/// A name scope. Scope 0 is the file itself. `named` scopes are modules
/// (`mod m { .. }` / `mod m;`); unnamed ones are function bodies, whose
/// lookups fall back to the enclosing scope.
#[derive(Clone, Debug)]
pub struct ScopeDecl {
    pub name: Option<Name>,
    pub parent: Option<u32>,
    /// `mod m;` — the body lives in another file.
    pub out_of_line: bool,
}

/// One imported name: `use a::b as c;` binds `c` to `a::b`; a glob
/// (`use a::*;`) binds every public name of `a`.
#[derive(Clone, Debug)]
pub struct UseDecl {
    pub scope: u32,
    pub path: Path,
    pub name: Option<Name>,
    pub glob: bool,
    /// Re-exported (`pub use`): visible to other modules' glob imports.
    pub public: bool,
}

/// A local binding (parameter, `let`, closure/`for`/pattern binding).
/// Visible to uses at byte offsets in `at..until` within function `func`.
#[derive(Clone, Copy, Debug)]
pub struct Local {
    pub func: u32,
    pub name: Sym,
    pub at: u32,
    pub until: u32,
    /// Declared type (index into `type_pool`).
    pub ty: Option<u32>,
    pub init: Option<ExprId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteKind {
    /// The expression is invoked.
    Call,
    /// A path used as a value (e.g. `register(Self::handler)`): an edge only
    /// when it names a function.
    Ref,
}

/// A call (or function-reference) site.
#[derive(Clone, Copy, Debug)]
pub struct Site {
    /// Enclosing function, if any (sites also occur in const initializers).
    pub func: Option<u32>,
    /// Innermost scope at the site.
    pub scope: u32,
    pub row: u32,
    pub at: u32,
    pub kind: SiteKind,
    /// `Expr::Call` / `Expr::Method` for calls, `Expr::Path` for refs.
    pub expr: ExprId,
}

/// Everything the pipeline needs from one file.
#[derive(Clone, Debug, Default)]
pub struct FileFacts {
    pub scopes: Vec<ScopeDecl>,
    pub uses: Vec<UseDecl>,
    pub fns: Vec<FnDecl>,
    pub types: Vec<TypeDecl>,
    pub traits: Vec<TraitDecl>,
    pub impls: Vec<ImplDecl>,
    pub values: Vec<ValueDecl>,
    /// Sorted by `(func, at)`.
    pub locals: Vec<Local>,
    pub sites: Vec<Site>,
    /// Expression arena; slot 0 is `Expr::Unknown`.
    pub exprs: Vec<Expr>,
    /// Interned identifiers.
    pub syms: Vec<Name>,
    /// Path segments of `Expr::Path` / `Expr::Struct`.
    pub path_pool: Vec<Sym>,
    /// Element lists of `Expr::Tuple` / `Expr::Ext` / `Expr::Branches`.
    pub expr_pool: Vec<ExprId>,
    /// Types of `Expr::Typed` and of typed locals.
    pub type_pool: Vec<TypeExpr>,
    /// Explicit generic arguments of a call (`f::<A, B>()`), by call node
    /// (ascending), as `type_pool` indices.
    pub turbofish: Vec<(ExprId, Vec<u32>)>,
    /// Returned expressions of functions without a declared return type
    /// (dynamic languages), by function (ascending).
    pub returns: Vec<Returned>,
    /// Positional arguments of calls (dynamic languages: typing parameters
    /// from call sites).
    pub call_args: Vec<CallArgs>,
    /// Loops `(func, start, end)` whose later bindings reach earlier uses
    /// (for [`super::lang::Lang::function_scoped_names`]).
    pub loops: Vec<(u32, u32, u32)>,
    /// Names a scope declares privately (Rust items without `pub`, Python
    /// `_names`, Go unexported names): glob imports see them only from
    /// within the declaring module's subtree.
    pub private: Vec<(u32, Name)>,
}

/// `return <value>` in function `func` (scope `scope`).
#[derive(Clone, Copy, Debug)]
pub struct Returned {
    pub func: u32,
    pub scope: u32,
    pub value: ExprId,
}

/// A call's positional arguments (in `expr_pool`) and where it occurs.
#[derive(Clone, Copy, Debug)]
pub struct CallArgs {
    pub call: ExprId,
    pub args: Span,
    pub func: Option<u32>,
    pub scope: u32,
    pub at: u32,
}

impl FileFacts {
    pub fn expr(&self, e: ExprId) -> Expr {
        self.exprs
            .get(e.0 as usize)
            .copied()
            .unwrap_or(Expr::Unknown)
    }

    pub fn sym(&self, s: Sym) -> &str {
        &self.syms[s.0 as usize]
    }

    /// The segments of a path span.
    pub fn path(&self, p: Span) -> Vec<&str> {
        self.path_pool[p.start as usize..(p.start + p.len) as usize]
            .iter()
            .map(|s| self.sym(*s))
            .collect()
    }

    /// The explicit generic arguments of call `e`, if any.
    pub fn turbofish(&self, e: ExprId) -> &[u32] {
        match self.turbofish.binary_search_by_key(&e.0, |(c, _)| c.0) {
            Ok(i) => &self.turbofish[i].1,
            Err(_) => &[],
        }
    }

    pub fn list(&self, l: Span) -> &[ExprId] {
        &self.expr_pool[l.start as usize..(l.start + l.len) as usize]
    }
}
