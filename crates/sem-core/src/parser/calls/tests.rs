//! Pipeline tests over small in-memory Rust crates: each asserts the exact
//! set of call targets a snippet resolves to — including that unknowable
//! targets produce *no* edge rather than a same-name guess.

use std::collections::BTreeSet;

use super::ir::{FileFacts, SiteKind};
use super::scope::{Def, ScopeTables};
use super::select::{ImplTables, Pick, Resolver};
use super::{language_for, lower_source};

/// `caller -> file:callee` for every call (`ref caller -> ..` for function
/// references) that resolves to repo functions.
fn edges(files: &[(&str, &str)]) -> BTreeSet<String> {
    let facts: Vec<(String, FileFacts)> = files
        .iter()
        .map(|(p, src)| (p.to_string(), lower_source(p, src).expect("rust file")))
        .collect();
    let named: Vec<(&str, &FileFacts)> = facts.iter().map(|(p, f)| (p.as_str(), f)).collect();
    let refs: Vec<&FileFacts> = facts.iter().map(|(_, f)| f).collect();
    let lang = language_for(files[0].0).unwrap();
    let layout = lang.layout(std::path::Path::new("/nonexistent"), &named);
    let tables = ScopeTables::build(&refs, &layout);
    let impls = ImplTables::build(&refs, &tables.view(), lang);
    let r = Resolver::new(lang, &refs, tables.view(), &impls);
    let mut out = BTreeSet::new();
    for (fi, f) in refs.iter().enumerate() {
        for site in &f.sites {
            let cx = r.fn_cx(fi as u32, site.func, site.scope);
            let targets: Vec<(u32, u32)> = match r.pick(site.expr, &cx, site.at, 0) {
                Pick::Defs(defs, _) => defs
                    .into_iter()
                    .filter_map(|d| match d {
                        Def::Fn(f, i) => Some((f, i)),
                        _ => None,
                    })
                    .collect(),
                _ => continue,
            };
            let caller = site
                .func
                .map(|c| f.fns[c as usize].name.to_string())
                .unwrap_or_default();
            for (tf, ti) in targets {
                let callee = format!(
                    "{}:{}",
                    named[tf as usize].0, refs[tf as usize].fns[ti as usize].name
                );
                let prefix = if site.kind == SiteKind::Ref {
                    "ref "
                } else {
                    ""
                };
                out.insert(format!("{prefix}{caller} -> {callee}"));
            }
        }
    }
    out
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn typed_locals_builders_and_return_types() {
    let got = edges(&[(
        "src/lib.rs",
        "
pub struct Builder { n: u32 }
pub struct Searcher { b: Builder }
impl Builder {
    pub fn new() -> Builder { Builder { n: 0 } }
    pub fn line(&mut self, n: u32) -> &mut Builder { self }
    pub fn build(&self) -> Searcher { Searcher { b: Builder::new() } }
}
impl Searcher {
    pub fn search(&self) -> Result<usize, String> { Ok(self.b.n as usize) }
}
fn run() {
    let s = Builder::new().line(1).build();
    let n = s.search().unwrap();
    let t: Searcher = make();
    t.search();
}
fn make() -> Searcher { Builder::new().build() }
",
    )]);
    assert_eq!(
        got,
        set(&[
            "build -> src/lib.rs:new",
            "make -> src/lib.rs:build",
            "make -> src/lib.rs:new",
            "run -> src/lib.rs:build",
            "run -> src/lib.rs:line",
            "run -> src/lib.rs:make",
            "run -> src/lib.rs:new",
            "run -> src/lib.rs:search",
        ])
    );
}

#[test]
fn unknowable_targets_produce_no_edge() {
    // `Vec::new`, `drop`, `x.len()` on an unknown receiver, and `Default::
    // default` must not bind to same-named repo functions.
    let got = edges(&[(
        "src/lib.rs",
        "
pub struct Store;
impl Store { pub fn new() -> Store { Store } pub fn len(&self) -> usize { 0 } }
impl Drop for Store { fn drop(&mut self) {} }
fn run(x: Vec<u8>, y: impl Iterator) {
    let v = Vec::new();
    drop(v);
    x.len();
    y.len();
    let d: Store = Default::default();
}
",
    )]);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn module_paths_imports_super_and_self_recursion() {
    let got = edges(&[
        ("src/lib.rs", "mod a;\nmod util;\nuse crate::a::b;\nfn top() { b::leaf(); a::b::leaf(); crate::util::helper(); top(); }\n"),
        ("src/a.rs", "pub mod b;\n"),
        ("src/a/b.rs", "pub fn leaf() { super::super::util::helper(); }\n"),
        ("src/util.rs", "pub fn helper() {}\npub fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }\n"),
    ]);
    assert_eq!(
        got,
        set(&[
            "fact -> src/util.rs:fact",
            "leaf -> src/util.rs:helper",
            "top -> src/a/b.rs:leaf",
            "top -> src/lib.rs:top",
            "top -> src/util.rs:helper",
        ])
    );
}

#[test]
fn generic_receivers_bind_to_the_trait_declaration() {
    let got = edges(&[(
        "src/lib.rs",
        "
pub trait Matcher { fn find(&self) -> bool; fn is_match(&self) -> bool { self.find() } }
pub struct Re;
impl Matcher for Re { fn find(&self) -> bool { true } }
fn generic<M: Matcher>(m: &M) -> bool { m.find() }
fn dynamic(m: &dyn Matcher) -> bool { m.is_match() }
fn concrete(r: Re) -> bool { r.find() && r.is_match() }
",
    )]);
    assert_eq!(
        got,
        set(&[
            "concrete -> src/lib.rs:find",
            "concrete -> src/lib.rs:is_match",
            "dynamic -> src/lib.rs:is_match",
            "generic -> src/lib.rs:find",
            "is_match -> src/lib.rs:find",
        ])
    );
}

#[test]
fn function_references_are_refs_not_calls() {
    let got = edges(&[(
        "src/lib.rs",
        "
pub struct View;
impl View {
    fn on_click(&mut self) {}
    fn render(&self) { register(Self::on_click); register(helper); }
}
fn register<F>(f: F) {}
fn helper() {}
",
    )]);
    assert_eq!(
        got,
        set(&[
            "ref render -> src/lib.rs:helper",
            "ref render -> src/lib.rs:on_click",
            "render -> src/lib.rs:register",
        ])
    );
}

#[test]
fn closure_parameters_are_typed_from_the_callee_signature() {
    let got = edges(&[(
        "src/lib.rs",
        "
pub struct Entity<T> { t: T }
impl<T> Entity<T> {
    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> R { todo!() }
    pub fn read(&self) -> &T { &self.t }
}
pub struct Editor { buffer: Entity<Buffer> }
pub struct Buffer;
impl Buffer { pub fn snapshot(&self) {} }
impl Editor { pub fn refresh(&mut self) {} }
fn run(e: Entity<Editor>, items: Vec<Buffer>) {
    e.update(|this| { this.refresh(); this.buffer.read().snapshot(); });
    items.iter().for_each(|b| b.snapshot());
}
",
    )]);
    assert_eq!(
        got,
        set(&[
            "run -> src/lib.rs:read",
            "run -> src/lib.rs:refresh",
            "run -> src/lib.rs:snapshot",
            "run -> src/lib.rs:update",
        ])
    );
}

#[test]
fn patterns_payloads_and_macros() {
    let got = edges(&[(
        "src/lib.rs",
        "
pub struct Req;
impl Req { pub fn is_ok(&self) -> bool { true } }
pub enum Strategy { Ext(Req), Other }
impl Strategy {
    fn check(&self) -> bool {
        match self { Strategy::Ext(r) => r.is_ok(), Strategy::Other => false }
    }
}
fn run(o: Option<Req>, pair: (Req, u8)) {
    if let Some(r) = o { assert!(r.is_ok()); }
    let (a, _) = pair;
    a.is_ok();
}
",
    )]);
    assert_eq!(
        got,
        set(&["check -> src/lib.rs:is_ok", "run -> src/lib.rs:is_ok"])
    );
}

#[test]
fn cfg_alternatives_all_receive_the_edge() {
    let got = edges(&[(
        "src/lib.rs",
        "
#[cfg(unix)]
fn imp() {}
#[cfg(not(unix))]
fn imp() {}
fn run() { imp(); }
",
    )]);
    assert_eq!(got, set(&["run -> src/lib.rs:imp"]));
    // both alternatives are targets
    let facts = lower_source(
        "src/lib.rs",
        "#[cfg(unix)]\nfn imp() {}\n#[cfg(not(unix))]\nfn imp() {}\n",
    )
    .unwrap();
    assert_eq!(facts.fns.len(), 2);
}

#[test]
fn partial_inputs_fall_back_to_directory_modules() {
    // No lib.rs declares `mod parser;`: the files are still reachable by path.
    let got = edges(&[
        ("parser/graph.rs", "use crate::parser::scope;\nfn caller() { scope::resolve(); super::scope::resolve(); }\n"),
        ("parser/scope.rs", "pub fn resolve() {}\n"),
    ]);
    assert_eq!(got, set(&["caller -> parser/scope.rs:resolve"]));
}

#[test]
fn gpui_shaped_context_callbacks() {
    let got = edges(&[(
        "src/lib.rs",
        "
pub struct Context<'a, T> { t: &'a T }
pub struct Entity<T> { t: T }
impl<'a, T: 'static> Context<'a, T> {
    pub fn subscribe<T2, E>(
        &mut self,
        entity: &Entity<T2>,
        mut on_event: impl FnMut(&mut T, Entity<T2>, &E, &mut Context<T>) + 'static,
    ) -> u32 { 0 }
}
pub struct Editor;
impl Editor {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.subscribe(&cx.t, move |this, store, event, cx| { this.refresh(); });
        Editor
    }
    fn refresh(&mut self) {}
}
",
    )]);
    assert!(got.contains("new -> src/lib.rs:refresh"), "{got:?}");
}

#[test]
fn go_packages_methods_embedding_and_multi_returns() {
    let got = edges(&[
        (
            "cmd.go",
            "package cli\n\ntype Base struct{}\nfunc (b *Base) Root() *Command { return nil }\n\ntype Command struct {\n\tBase\n\tparent *Command\n}\nfunc (c *Command) Name() string { return \"\" }\nfunc New() (*Command, error) { return &Command{}, nil }\n",
        ),
        (
            "run.go",
            "package cli\n\nimport \"fmt\"\n\nvar std = &Command{}\n\nfunc Run() {\n\tc, err := New()\n\tc.Name()\n\tc.Root()\n\tc.parent.Name()\n\tstd.Name()\n\tfmt.Println(err)\n}\n",
        ),
    ]);
    assert_eq!(
        got,
        set(&[
            "Run -> cmd.go:Name",
            "Run -> cmd.go:New",
            "Run -> cmd.go:Root",
        ])
    );
}

#[test]
fn python_unions_bases_and_context_managers() {
    let got = edges(&[(
        "pkg/m.py",
        "
class Base:
    def fail(self): pass
    def info(self): return {}
class A(Base):
    def run(self): return 1
    def info(self):
        self.fail()
        return super().info()
class B(Base):
    def run(self): return 2
class Ctx:
    def __enter__(self) -> 'Ctx': return self
    def scope(self): pass
def pick(x: A | B | None, c: Ctx, d: dict[str, A]):
    x.run()
    with c as entered:
        entered.scope()
    d['k'].run()
    for i, a in enumerate(d.values()):
        a.run()
    y = A() if x else B()
    y.run()
",
    )]);
    assert_eq!(
        got,
        set(&[
            "info -> pkg/m.py:fail",
            "info -> pkg/m.py:info",
            "pick -> pkg/m.py:run",
            "pick -> pkg/m.py:scope",
        ])
    );
}

#[test]
fn outside_traits_blanket_impls_and_wrappers_stay_unknown() {
    // `io::Error::from` may be std's own impl, `Default::default` any
    // repo or std impl, a boxed value's `fmt` the Box's, and `shout` a
    // blanket impl's: none of them is claimed as a single repo target.
    let got = edges(&[(
        "src/lib.rs",
        "
use std::fmt;
pub struct E;
impl From<E> for std::io::Error { fn from(e: E) -> Self { todo!() } }
impl Default for E { fn default() -> Self { E } }
impl fmt::Display for E { fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { Ok(()) } }
pub trait Shout { fn shout(&self) {} }
impl<T: fmt::Display> Shout for T {}
fn run(b: Box<E>, f: &mut fmt::Formatter, s: String, e: std::io::Error) {
    std::io::Error::from(e);
    let d: E = Default::default();
    b.fmt(f);
    s.shout();
}
",
    )]);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn closures_and_partial_annotations_are_typed() {
    let got = edges(&[(
        "src/lib.rs",
        "
pub struct W;
impl W { pub fn run(&self) {} }
fn make() -> W { W }
fn go() {
    let m = || make();
    m().run();
    let ws: Vec<_> = vec![make()];
    for w in ws { w.run(); }
}
",
    )]);
    assert_eq!(got, set(&["go -> src/lib.rs:make", "go -> src/lib.rs:run"]));
}
