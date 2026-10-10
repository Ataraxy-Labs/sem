//! Exact-or-unknown call graph.
//!
//! A pipeline of small stages, each a function of explicit inputs:
//!
//! 1. **lower** (per file, per language — [`lang::Lang::lower`]): syntax tree
//!    -> [`ir::FileFacts`]: declarations, local bindings and call sites.
//! 2. **scope** ([`scope::ScopeTables`]): all files' facts + the language's
//!    module [`lang::Layout`] -> "what does path `p` mean in scope `s`".
//! 3. **infer** ([`infer`]): expression -> type, from declared types only.
//! 4. **select** ([`select::Resolver::pick`]): site -> [`select::Pick`]:
//!    the set of repo definitions it binds to (one = exact, several = it
//!    reaches one of them), external, or *unknown* with a reason. Never a
//!    same-name guess.
//! 5. **classify** (here): picks -> [`EdgeKind`] edges between sem entities,
//!    plus one `Dispatch` edge from every trait method declaration to each
//!    impl of it, so reachability through a trait call reaches every impl.
//!
//! Only stage 1 and the data tables behind [`lang::Lang`] are
//! language-specific; a new language plugs in by implementing that trait.

pub mod fit;
pub mod go;
pub mod infer;
pub mod ir;
pub mod lang;
pub mod python;
pub mod rust;
pub mod scope;
pub mod select;
#[cfg(test)]
mod tests;

use std::path::Path as FsPath;

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;

use crate::model::entity::SemanticEntity;
use crate::parser::graph::{canonical_entity_id, EntityInfoMap, RefType, ResolvedEdge};
use ir::{FileFacts, SiteKind};
use lang::Lang;
use scope::ScopeTables;
use select::{ImplTables, Pick, Resolver};

/// The kinds of edge this pipeline produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// The caller invokes the callee.
    Calls,
    /// The caller mentions the callee as a value (`register(Self::handler)`).
    Refs,
    /// The source mentions a type, trait or module-level value.
    TypeRef,
    /// The source may run this target: a trait method declaration one of
    /// its implementations, a base method an override, a call with several
    /// possible targets each of them.
    Dispatch,
}

/// An edge between two sem entities, by id.
#[derive(Clone, Debug)]
pub struct CallEdge<'e> {
    pub from: &'e str,
    pub to: &'e str,
    pub kind: EdgeKind,
}

/// Per-outcome counts over all call sites (for diagnostics and tests).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub calls: usize,
    pub refs: usize,
    pub typerefs: usize,
    pub dispatch: usize,
    /// Call sites whose target is outside the repo.
    pub external: usize,
    /// Resolved to a repo definition sem has no entity for.
    pub no_entity: usize,
    pub unresolved: HashMap<&'static str, usize>,
    /// Every site's answer as a JSON line, with `SEM_CALLS_SITES` set (`sem
    /// system` sets it around its layer builds).
    pub sites: Vec<String>,
}

impl Stats {
    fn merge(&mut self, o: Stats) {
        self.calls += o.calls;
        self.refs += o.refs;
        self.typerefs += o.typerefs;
        self.dispatch += o.dispatch;
        self.external += o.external;
        self.no_entity += o.no_entity;
        for (k, v) in o.unresolved {
            *self.unresolved.entry(k).or_default() += v;
        }
        self.sites.extend(o.sites);
    }
}

/// The languages this pipeline owns call edges for, by file extension.
/// Adding a language = one row here plus its [`Lang`] implementation.
static LANGUAGES: &[(&[&str], &dyn Lang)] = &[
    (&[".rs"], &rust::RUST),
    (&[".go"], &go::GO),
    (&[".py"], &python::PYTHON),
];

/// The language front end for a file path, if the pipeline handles it.
pub fn language_for(path: &str) -> Option<&'static dyn Lang> {
    LANGUAGES
        .iter()
        .find(|(exts, _)| exts.iter().any(|e| path.ends_with(e)))
        .map(|(_, lang)| *lang)
}

/// Stages 1–5 for every file this pipeline handles, as graph edges.
///
/// Run as soon as all entities are known, so the stage-1 facts are dropped
/// before the rest of the graph build. Facts come from `facts` (lowered
/// during parsing when trees are not retained), else from a retained tree
/// in `parsed`, else by re-reading and parsing the file (its entities were
/// reused from a previous build).
pub(crate) fn resolve_call_edges(
    root: &FsPath,
    file_paths: &[String],
    entities: &[SemanticEntity],
    entity_map: &EntityInfoMap,
    mut facts: HashMap<String, std::sync::Arc<FileFacts>>,
    parsed: &[(String, String, tree_sitter::Tree)],
) -> Vec<ResolvedEdge> {
    let owned: Vec<&String> = file_paths
        .iter()
        .filter(|p| language_for(p).is_some())
        .collect();
    let started = std::time::Instant::now();
    // Per-outcome counts and the per-site dump: `sem system` turns these on
    // around its layer builds to measure each layer's unknown rate.
    let report = std::env::var_os("SEM_CALLS_STATS").is_some();
    let trees: HashMap<&str, (&str, &tree_sitter::Tree)> = parsed
        .iter()
        .map(|(p, src, tree)| (p.as_str(), (src.as_str(), tree)))
        .collect();
    let missing: Vec<&&String> = owned
        .iter()
        .filter(|p| !facts.contains_key(p.as_str()))
        .collect();
    #[cfg(feature = "parallel")]
    let lowered = missing.par_iter();
    #[cfg(not(feature = "parallel"))]
    let lowered = missing.iter();
    let lowered: Vec<(String, FileFacts)> = lowered
        .filter_map(|p| {
            let facts = match trees.get(p.as_str()) {
                Some((src, tree)) => lower_file(p, tree, src)?,
                None => lower_source(p, &std::fs::read_to_string(root.join(p.as_str())).ok()?)?,
            };
            Some((p.to_string(), facts))
        })
        .collect();
    facts.extend(
        lowered
            .into_iter()
            .map(|(p, f)| (p, std::sync::Arc::new(f))),
    );
    if report {
        eprintln!(
            "calls: lowered {} files late in {:?}",
            missing.len(),
            started.elapsed()
        );
    }

    let mut out = Vec::new();
    for (exts, lang) in LANGUAGES {
        let files: Vec<(&str, &FileFacts)> = owned
            .iter()
            .filter(|p| exts.iter().any(|e| p.ends_with(e)))
            .filter_map(|p| facts.get(p.as_str()).map(|f| (p.as_str(), &**f)))
            .collect();
        if files.is_empty() {
            continue;
        }
        let t = std::time::Instant::now();
        let (edges, stats) = resolve(root, *lang, &files, entities);
        if report {
            let Stats { sites, .. } = &stats;
            eprintln!(
                "calls[{}]: {:?} {:?}",
                exts.join(","),
                t.elapsed(),
                Stats {
                    sites: Vec::new(),
                    ..stats.clone()
                }
            );
            if let Some(path) = std::env::var_os("SEM_CALLS_SITES") {
                use std::io::Write;
                let mut out = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .expect("SEM_CALLS_SITES: cannot open");
                for line in sites {
                    let _ = writeln!(out, "{line}");
                }
            }
        }
        out.extend(edges.into_iter().map(|e| {
            (
                canonical_entity_id(entity_map, e.from),
                canonical_entity_id(entity_map, e.to),
                match e.kind {
                    EdgeKind::Calls => RefType::Calls,
                    EdgeKind::Refs => RefType::Refs,
                    EdgeKind::TypeRef => RefType::TypeRef,
                    EdgeKind::Dispatch => RefType::Dispatch,
                },
            )
        }));
    }
    out
}

/// Replace the call edges of files this pipeline handles with its own: the
/// other resolvers' `Calls` edges into functions from those files are
/// dropped (they include same-name guesses), as are their `TypeRef` edges
/// into functions (a function is not a type: those are call guesses by
/// capitalized name), and `call_edges` are added. Edges of other kinds and
/// from other languages are untouched.
pub(crate) fn apply_call_edges(
    entity_map: &EntityInfoMap,
    call_edges: Vec<ResolvedEdge>,
    edges: &mut Vec<ResolvedEdge>,
) {
    let owned = |id: &str, want_fn: bool| {
        entity_map.get(id).is_some_and(|e| {
            language_for(&e.file_path).is_some()
                && (!want_fn || FN_ENTITY_TYPES.contains(&e.entity_type.as_str()))
        })
    };
    edges.retain(|(from, to, rt)| {
        !(matches!(rt, RefType::Calls | RefType::TypeRef) && owned(from, false) && owned(to, true))
    });
    // first, so that deduplication keeps the pipeline's kind for a pair
    edges.splice(0..0, call_edges);
}

/// Stage 1 for one file.
pub fn lower_file(path: &str, tree: &tree_sitter::Tree, src: &str) -> Option<FileFacts> {
    language_for(path).map(|lang| lang.lower(tree, src))
}

/// Parse and lower a file from source text (for files whose tree is gone).
pub fn lower_source(path: &str, src: &str) -> Option<FileFacts> {
    let lang = language_for(path)?;
    let config = crate::parser::plugins::code::languages::get_language_config(
        path.rfind('.').map(|i| &path[i..]).unwrap_or(""),
    )?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&(config.get_language)()?).ok()?;
    let tree = parser.parse(src, None)?;
    Some(lang.lower(&tree, src))
}

/// Stages 2–5 over one language's files.
pub fn resolve<'e>(
    root: &FsPath,
    lang: &dyn Lang,
    files: &[(&str, &FileFacts)],
    entities: &'e [SemanticEntity],
) -> (Vec<CallEdge<'e>>, Stats) {
    let facts: Vec<&FileFacts> = files.iter().map(|(_, f)| *f).collect();
    let layout = lang.layout(root, files);
    let tables = ScopeTables::build(&facts, &layout);
    let impls = ImplTables::build(&facts, &tables.view(), lang);
    let ids = EntityIds::build(files, entities);
    let fn_ids = &ids.fns;
    let owners = owner_index(entities);
    let dump = std::env::var_os("SEM_CALLS_SITES").is_some();
    let no_owners = OwnerIndex::default();

    let hints = if lang.infer_params_from_calls() {
        Some(param_hints(lang, &facts, &tables, &impls))
    } else {
        None
    };
    let resolver = || {
        let mut r = Resolver::new(lang, &facts, tables.view(), &impls);
        r.param_hints = hints.as_ref();
        r
    };
    let resolve_one = |r: &mut Resolver, fi: usize| -> (Vec<CallEdge<'e>>, Stats) {
        let own = owners.get(files[fi].0).unwrap_or(&no_owners);
        classify_file(r, (fi as u32, files[fi].0), &facts, &ids, own, dump)
    };
    let indices: Vec<usize> = (0..files.len()).collect();
    #[cfg(feature = "parallel")]
    let per_file: Vec<(Vec<CallEdge<'e>>, Stats)> = indices
        .par_iter()
        .map_init(resolver, |r, &fi| resolve_one(r, fi))
        .collect();
    #[cfg(not(feature = "parallel"))]
    let per_file: Vec<(Vec<CallEdge<'e>>, Stats)> = {
        let mut r = resolver();
        indices.iter().map(|&fi| resolve_one(&mut r, fi)).collect()
    };

    let mut edges = Vec::new();
    let mut stats = Stats::default();
    for (e, s) in per_file {
        edges.extend(e);
        stats.merge(s);
    }
    // Dispatch: interface/trait method declaration -> each implementation;
    // with virtual methods, a base class's method -> each override.
    let mut pairs = dispatch_pairs(lang, &facts, &impls);
    if lang.virtual_methods() {
        pairs.extend(override_pairs(&resolver(), &facts, &impls));
    }
    for (decl, imp) in pairs {
        let id = |(f, i): FnRef| fn_ids.get(f as usize).and_then(|v| v.get(i as usize)).copied().flatten();
        let (from, to) = (id(decl), id(imp));
        if let (Some(from), Some(to)) = (from, to) {
            edges.push(CallEdge {
                from,
                to,
                kind: EdgeKind::Dispatch,
            });
            stats.dispatch += 1;
        }
    }
    edges.sort_by(|a, b| (a.from, a.to).cmp(&(b.from, b.to)));
    edges.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.kind == b.kind);
    (edges, stats)
}

type FnRef = (u32, u32);
/// `(file, fn, parameter)` and a type some call site passes there.
type ParamSeen = ((u32, u32, u32), infer::Ty);

/// Types for unannotated parameters from the call sites that reach them:
/// `(file, fn, param) -> T` when every resolved call passes a `T` there
/// (constructor calls reach `__init__`). One pass, no fixed point.
fn param_hints(
    lang: &dyn Lang,
    facts: &[&FileFacts],
    tables: &ScopeTables,
    impls: &ImplTables,
) -> HashMap<(u32, u32, u32), infer::Ty> {
    let per_file = |r: &mut Resolver, fi: usize| {
        let f = facts[fi];
        let mut out: Vec<ParamSeen> = Vec::new();
        for ca in &f.call_args {
            let cx = r.fn_cx(fi as u32, ca.func, ca.scope);
            let pick = r.pick(ca.call, &cx, ca.at, 0);
            let target = match (&pick, pick.single_fn()) {
                (_, Some((tf, ti, _))) => {
                    Some((tf, ti, matches!(f.expr(ca.call), ir::Expr::Method(..))))
                }
                // `Cls(x)` passes x to `Cls.__init__(self, x)`
                (Pick::Defs(d, Some(ty)), None) if matches!(d[..], [scope::Def::Type(..)]) => r
                    .method(ty, "__init__", 0)
                    .single_fn()
                    .map(|(tf, ti, _)| (tf, ti, true)),
                _ => None,
            };
            let Some((tf, ti, receiver_bound)) = target else {
                continue;
            };
            let decl = &facts[tf as usize].fns[ti as usize];
            // an unbound call `Cls.m(obj, x)` passes the receiver explicitly
            let skip = usize::from(decl.has_self && !receiver_bound);
            for (k, &arg) in f.list(ca.args).iter().enumerate().skip(skip) {
                let t = r.type_of(arg, &cx, ca.at, 0);
                out.push(((tf, ti, (k - skip) as u32), t));
            }
        }
        out
    };
    let indices: Vec<usize> = (0..facts.len()).collect();
    #[cfg(feature = "parallel")]
    let found: Vec<Vec<ParamSeen>> = indices
        .par_iter()
        .map_init(
            || Resolver::new(lang, facts, tables.view(), impls),
            |r, &fi| per_file(r, fi),
        )
        .collect();
    #[cfg(not(feature = "parallel"))]
    let found: Vec<Vec<ParamSeen>> = {
        let mut r = Resolver::new(lang, facts, tables.view(), impls);
        indices.iter().map(|&fi| per_file(&mut r, fi)).collect()
    };
    let mut agreed: HashMap<(u32, u32, u32), Option<infer::Ty>> = HashMap::default();
    for (key, t) in found.into_iter().flatten() {
        let slot = agreed.entry(key).or_insert_with(|| Some(t.clone()));
        if slot.as_ref() != Some(&t) || t == infer::Ty::Unknown {
            *slot = None; // a call site disagrees or is unknown
        }
    }
    agreed
        .into_iter()
        .filter_map(|(k, v)| v.map(|t| (k, t)))
        .collect()
}

/// `(base class method, override)` pairs: a call answered with the base's
/// method may run any subclass's override of it.
fn override_pairs(r: &Resolver, facts: &[&FileFacts], impls: &ImplTables) -> Vec<(FnRef, FnRef)> {
    let mut out = Vec::new();
    for (&(f, ty), impl_keys) in &impls.by_adt {
        if facts[f as usize].types[ty as usize].embeds.is_empty() {
            continue;
        }
        let t = infer::Ty::Adt(f, ty, Vec::new());
        for &(fi, ii) in impl_keys {
            for &m in &impls.impl_members[fi as usize][ii as usize] {
                let name = &facts[fi as usize].fns[m as usize].name;
                if let Some(Pick::Defs(defs, _)) = r.inherited(f, ty, &t, name, 0) {
                    for d in defs {
                        if let scope::Def::Fn(bf, bi) = d {
                            out.push(((bf, bi), (fi, m)));
                        }
                    }
                }
            }
        }
    }
    out
}

/// `(trait method declaration, implementing method)` pairs: nominal
/// (`impl Trait for T`) and, for structurally typed languages, every type
/// whose methods cover all of an interface's (Go).
fn dispatch_pairs(
    lang: &dyn Lang,
    facts: &[&FileFacts],
    impls: &ImplTables,
) -> Vec<(FnRef, FnRef)> {
    let name = |(f, i): FnRef| &*facts[f as usize].fns[i as usize].name;
    let mut out = Vec::new();
    for (&(tf, tt), impl_keys) in &impls.by_trait {
        for &(f, i) in impl_keys {
            for &m in &impls.impl_members[f as usize][i as usize] {
                let decl = impls.trait_members[tf as usize][tt as usize]
                    .iter()
                    .find(|&&d| name((tf, d)) == name((f, m)));
                if let Some(&d) = decl {
                    out.push(((tf, d), (f, m)));
                }
            }
        }
    }
    if !lang.structural_interfaces() {
        return out;
    }
    let mut methods: HashMap<(u32, u32), HashMap<&str, FnRef>> = HashMap::default();
    for (&ty, impl_keys) in &impls.by_adt {
        for &(f, i) in impl_keys {
            for &m in &impls.impl_members[f as usize][i as usize] {
                methods.entry(ty).or_default().insert(name((f, m)), (f, m));
            }
        }
    }
    let mut types_with: HashMap<&str, Vec<(u32, u32)>> = HashMap::default();
    for (ty, ms) in &methods {
        for n in ms.keys() {
            types_with.entry(n).or_default().push(*ty);
        }
    }
    for (tf, f) in facts.iter().enumerate() {
        for ti in 0..f.traits.len() {
            let own = &impls.trait_members[tf][ti];
            let all: Vec<FnRef> = impls
                .trait_closure(&[(tf as u32, ti as u32)])
                .into_iter()
                .flat_map(|(a, b)| {
                    impls.trait_members[a as usize][b as usize]
                        .iter()
                        .map(move |&d| (a, d))
                })
                .collect();
            let Some(rarest) = all
                .iter()
                .min_by_key(|d| types_with.get(name(**d)).map_or(0, Vec::len))
            else {
                continue;
            };
            for ty in types_with.get(name(*rarest)).into_iter().flatten() {
                let ms = &methods[ty];
                if all.iter().all(|d| ms.contains_key(name(*d))) {
                    for &d in own {
                        out.push(((tf as u32, d), ms[name((tf as u32, d))]));
                    }
                }
            }
        }
    }
    out
}

/// Stage 5 for one file: resolve every site and turn its answer into
/// edges — one per repo definition in the answer set.
fn classify_file<'e>(
    r: &mut Resolver,
    (fi, path): (u32, &str),
    facts: &[&FileFacts],
    ids: &EntityIds<'e>,
    owners: &OwnerIndex<'e>,
    dump: bool,
) -> (Vec<CallEdge<'e>>, Stats) {
    use scope::Def;
    let f = facts[fi as usize];
    let mut edges = Vec::new();
    let mut stats = Stats::default();
    let mut cxs: HashMap<(Option<u32>, u32), infer::FnCx> = HashMap::default();
    for site in &f.sites {
        let cx = cxs
            .entry((site.func, site.scope))
            .or_insert_with(|| r.fn_cx(fi, site.func, site.scope));
        let call = site.kind == SiteKind::Call;
        let pick = match r.pick(site.expr, cx, site.at, 0) {
            // calling a variable (or a module) calls a runtime value
            Pick::Defs(d, _)
                if call && !d.iter().any(|d| matches!(d, Def::Fn(..) | Def::Type(..))) =>
            {
                Pick::Unknown("calls a value")
            }
            pick => pick,
        };
        // a site in a function sem has no entity for (a const initializer)
        // belongs to the entity around it
        // (a file sem extracted no entities from has no id table: no owner)
        let from = site
            .func
            .and_then(|func| ids.fns.get(fi as usize)?.get(func as usize).copied().flatten())
            .or_else(|| owners.at_row(site.row));
        let mut answer: Vec<Option<&str>> = Vec::new();
        match &pick {
            Pick::Defs(defs, _) => {
                for d in defs {
                    let kind = match (d, call) {
                        (Def::Fn(..), false) => EdgeKind::Refs,
                        // one of several targets: the call may reach it
                        (Def::Fn(..) | Def::Type(..), true) if defs.len() > 1 => EdgeKind::Dispatch,
                        (Def::Fn(..) | Def::Type(..), true) => EdgeKind::Calls,
                        (Def::Type(..) | Def::Trait(..) | Def::Value(..), false) => {
                            EdgeKind::TypeRef
                        }
                        _ => continue,
                    };
                    let to = ids.of(*d);
                    answer.push(to);
                    match (from, to) {
                        (Some(from), Some(to)) => {
                            match kind {
                                EdgeKind::Calls | EdgeKind::Dispatch => stats.calls += 1,
                                EdgeKind::Refs => stats.refs += 1,
                                _ => stats.typerefs += 1,
                            }
                            edges.push(CallEdge { from, to, kind });
                        }
                        _ => stats.no_entity += 1,
                    }
                }
            }
            Pick::External(_) if call => stats.external += 1,
            Pick::Unknown(why) if call => *stats.unresolved.entry(why).or_default() += 1,
            _ => {}
        }
        if dump {
            let (defs, unknown) = match &pick {
                Pick::Defs(..) => (Some(answer), None),
                Pick::External(_) => (None, None),
                Pick::Unknown(why) => (None, Some(*why)),
            };
            stats.sites.push(
                serde_json::json!({
                    "file": path, "at": site.at, "call": call,
                    "defs": defs, "unknown": unknown,
                })
                .to_string(),
            );
        }
    }
    (edges, stats)
}

const FN_ENTITY_TYPES: &[&str] = &["function", "method"];

/// Sem's entity for each lowered declaration, by kind: same file, same
/// name, and the entity's line span contains the declaration row.
///
/// A method *signature* in a trait/interface often has no entity of its own
/// (Go interface methods, Rust required trait methods); it is represented by
/// its trait's entity, so calls through it and its dispatch edges survive.
struct EntityIds<'e> {
    fns: Vec<Vec<Option<&'e str>>>,
    types: Vec<Vec<Option<&'e str>>>,
    traits: Vec<Vec<Option<&'e str>>>,
    values: Vec<Vec<Option<&'e str>>>,
}

impl<'e> EntityIds<'e> {
    fn build(files: &[(&str, &FileFacts)], entities: &'e [SemanticEntity]) -> Self {
        let mut by_file: HashMap<&str, HashMap<&str, Vec<&SemanticEntity>>> = HashMap::default();
        for e in entities {
            by_file
                .entry(e.file_path.as_str())
                .or_default()
                .entry(e.name.as_str())
                .or_default()
                .push(e);
        }
        let per_file = |get: &dyn Fn(
            &HashMap<&str, Vec<&'e SemanticEntity>>,
            &FileFacts,
        ) -> Vec<Option<&'e str>>| {
            files
                .iter()
                .map(|(path, facts)| match by_file.get(path) {
                    Some(names) => get(names, facts),
                    None => Vec::new(),
                })
                .collect::<Vec<_>>()
        };
        EntityIds {
            fns: per_file(&|names, f| {
                f.fns
                    .iter()
                    .map(|d| {
                        find(names, &d.name, d.row, true).or_else(|| match d.owner {
                            ir::Owner::Trait(t) => {
                                let tr = &f.traits[t as usize];
                                find(names, &tr.name, tr.row, false)
                            }
                            _ => None,
                        })
                    })
                    .collect()
            }),
            types: per_file(&|names, f| {
                f.types
                    .iter()
                    .map(|d| find(names, &d.name, d.row, false))
                    .collect()
            }),
            traits: per_file(&|names, f| {
                f.traits
                    .iter()
                    .map(|d| find(names, &d.name, d.row, false))
                    .collect()
            }),
            values: per_file(&|names, f| {
                f.values
                    .iter()
                    .map(|d| find(names, &d.name, d.row, false))
                    .collect()
            }),
        }
    }

    fn of(&self, d: scope::Def) -> Option<&'e str> {
        use scope::Def;
        let (table, f, i) = match d {
            Def::Fn(f, i) => (&self.fns, f, i),
            Def::Type(f, i) => (&self.types, f, i),
            Def::Trait(f, i) => (&self.traits, f, i),
            Def::Value(f, i) => (&self.values, f, i),
            Def::Module(_) | Def::External => return None,
        };
        *table.get(f as usize)?.get(i as usize)?
    }
}

/// The innermost entity named `name` whose span holds 0-based `row`: a
/// function entity or (`fns` false) any other.
fn find<'e>(
    names: &HashMap<&str, Vec<&'e SemanticEntity>>,
    name: &str,
    row: u32,
    fns: bool,
) -> Option<&'e str> {
    let row = row as usize + 1;
    names
        .get(name)?
        .iter()
        .filter(|e| e.start_line <= row && row <= e.end_line)
        .filter(|e| FN_ENTITY_TYPES.contains(&e.entity_type.as_str()) == fns)
        .min_by_key(|e| e.end_line - e.start_line)
        .map(|e| e.id.as_str())
}

/// Innermost entity by row, for sites outside functions.
#[derive(Default)]
struct OwnerIndex<'e> {
    spans: Vec<(usize, usize, &'e str)>,
}

impl<'e> OwnerIndex<'e> {
    fn at_row(&self, row: u32) -> Option<&'e str> {
        let row = row as usize + 1;
        self.spans
            .iter()
            .filter(|(s, e, _)| *s <= row && row <= *e)
            .min_by_key(|(s, e, _)| e - s)
            .map(|(_, _, id)| *id)
    }
}

fn owner_index(entities: &[SemanticEntity]) -> HashMap<&str, OwnerIndex<'_>> {
    let mut out: HashMap<&str, OwnerIndex> = HashMap::default();
    for e in entities {
        if language_for(&e.file_path).is_some() {
            out.entry(e.file_path.as_str()).or_default().spans.push((
                e.start_line,
                e.end_line,
                e.id.as_str(),
            ));
        }
    }
    out
}

/// What one site resolved to, for consumers outside the graph build (the
/// data-flow engine). Same answers as the edges, with the external and
/// unknown outcomes kept instead of dropped.
#[derive(Clone, Debug, PartialEq)]
pub enum SiteAnswer {
    /// Repo definitions, as sem entity ids (functions, or a type for a
    /// constructor call). Several = the site reaches one of them.
    Defs(Vec<String>),
    /// A module-level value (`const`, `static`, Go `var`, a Python module
    /// assignment): `(file, name)`.
    Value(String, String),
    /// Outside the repo. For a method call, the receiver's type as the
    /// resolver knows it: its last path segment only (`DB`, `Session`).
    External(Option<String>),
    Unknown(&'static str),
}

/// Stages 2–4 for one language's files, answering every site: per file,
/// `(byte offset of the site's name, is a call, answer)`.
pub fn site_answers(
    root: &FsPath,
    lang: &dyn Lang,
    files: &[(&str, &FileFacts)],
    entities: &[SemanticEntity],
) -> Vec<Vec<(u32, bool, SiteAnswer)>> {
    use scope::Def;
    let facts: Vec<&FileFacts> = files.iter().map(|(_, f)| *f).collect();
    let layout = lang.layout(root, files);
    let tables = ScopeTables::build(&facts, &layout);
    let impls = ImplTables::build(&facts, &tables.view(), lang);
    let ids = EntityIds::build(files, entities);
    let hints = if lang.infer_params_from_calls() {
        Some(param_hints(lang, &facts, &tables, &impls))
    } else {
        None
    };
    let per_file = |r: &mut Resolver, fi: usize| -> Vec<(u32, bool, SiteAnswer)> {
        let f = facts[fi];
        let mut cxs: HashMap<(Option<u32>, u32), infer::FnCx> = HashMap::default();
        let mut out = Vec::with_capacity(f.sites.len());
        for site in &f.sites {
            let cx = cxs
                .entry((site.func, site.scope))
                .or_insert_with(|| r.fn_cx(fi as u32, site.func, site.scope));
            let call = site.kind == SiteKind::Call;
            let answer = match r.pick(site.expr, cx, site.at, 0) {
                Pick::Defs(d, _) if call && !d.iter().any(|d| matches!(d, Def::Fn(..) | Def::Type(..))) => {
                    SiteAnswer::Unknown("calls a value")
                }
                Pick::Defs(defs, _) => {
                    let value = defs.iter().find_map(|d| match d {
                        Def::Value(vf, vi) if !call => Some(SiteAnswer::Value(
                            files[*vf as usize].0.to_string(),
                            facts[*vf as usize].values[*vi as usize].name.to_string(),
                        )),
                        _ => None,
                    });
                    match value {
                        Some(v) => v,
                        None => {
                            let targets: Vec<String> = defs
                                .iter()
                                .filter(|d| matches!(d, Def::Fn(..) | Def::Type(..)))
                                .filter_map(|d| ids.of(*d).map(str::to_string))
                                .collect();
                            if targets.is_empty() {
                                SiteAnswer::Unknown("repo definition without an entity")
                            } else {
                                SiteAnswer::Defs(targets)
                            }
                        }
                    }
                }
                Pick::External(_) => {
                    let recv = match f.expr(site.expr) {
                        ir::Expr::Method(recv, _) => match r.type_of(recv, cx, site.at, 0) {
                            infer::Ty::Ext(name, _) => Some(name.to_string()),
                            _ => None,
                        },
                        _ => None,
                    };
                    SiteAnswer::External(recv)
                }
                Pick::Unknown(why) => SiteAnswer::Unknown(why),
            };
            out.push((site.at, call, answer));
        }
        out
    };
    let indices: Vec<usize> = (0..files.len()).collect();
    let make = || {
        let mut r = Resolver::new(lang, &facts, tables.view(), &impls);
        r.param_hints = hints.as_ref();
        r
    };
    #[cfg(feature = "parallel")]
    let out: Vec<Vec<(u32, bool, SiteAnswer)>> = indices.par_iter().map_init(make, |r, &fi| per_file(r, fi)).collect();
    #[cfg(not(feature = "parallel"))]
    let out: Vec<Vec<(u32, bool, SiteAnswer)>> = {
        let mut r = make();
        indices.iter().map(|&fi| per_file(&mut r, fi)).collect()
    };
    out
}

/// `(declaration or base method, implementation or override)` pairs as sem
/// entity ids: a call answered with the first may run the second.
pub fn dispatch_answers(root: &FsPath, lang: &dyn Lang, files: &[(&str, &FileFacts)], entities: &[SemanticEntity]) -> Vec<(String, String)> {
    let facts: Vec<&FileFacts> = files.iter().map(|(_, f)| *f).collect();
    let layout = lang.layout(root, files);
    let tables = ScopeTables::build(&facts, &layout);
    let impls = ImplTables::build(&facts, &tables.view(), lang);
    let ids = EntityIds::build(files, entities);
    let mut pairs = dispatch_pairs(lang, &facts, &impls);
    if lang.virtual_methods() {
        pairs.extend(override_pairs(&Resolver::new(lang, &facts, tables.view(), &impls), &facts, &impls));
    }
    pairs
        .into_iter()
        .filter_map(|(d, i)| Some((ids.fns[d.0 as usize][d.1 as usize]?.to_string(), ids.fns[i.0 as usize][i.1 as usize]?.to_string())))
        .collect()
}

/// Describe how every site in `target` resolves (debugging aid for the
/// call-graph harness): one line per site, `row: expr => pick`.
pub fn explain(root: &FsPath, files: &[(&str, &FileFacts)], target: &str) -> Vec<String> {
    let Some(lang) = language_for(target) else {
        return Vec::new();
    };
    let facts: Vec<&FileFacts> = files.iter().map(|(_, f)| *f).collect();
    let layout = lang.layout(root, files);
    let tables = ScopeTables::build(&facts, &layout);
    let impls = ImplTables::build(&facts, &tables.view(), lang);
    let r = Resolver::new(lang, &facts, tables.view(), &impls);
    let Some(fi) = files.iter().position(|(p, _)| *p == target) else {
        return Vec::new();
    };
    let mut out = vec![format!(
        "module parent: {:?}; crates: {}",
        layout.parent_of[fi].map(|(f, s)| (files[f].0, s)),
        layout.crates.len()
    )];
    for site in &facts[fi].sites {
        let cx = r.fn_cx(fi as u32, site.func, site.scope);
        let pick = r.pick(site.expr, &cx, site.at, 0);
        let shown = match &pick {
            Pick::Defs(defs, _) => defs
                .iter()
                .map(|d| match d {
                    scope::Def::Fn(f, i) => format!(
                        "FN {}::{}",
                        files[*f as usize].0, facts[*f as usize].fns[*i as usize].name
                    ),
                    d => format!("{d:?}"),
                })
                .collect::<Vec<_>>()
                .join(" | "),
            p => format!("{p:?}"),
        };
        let why = match pick {
            Pick::Unknown("unknown receiver type") => format!(
                "  [blocked at {}]",
                blocker(&r, facts[fi], site.expr, &cx, site.at)
            ),
            _ => String::new(),
        };
        out.push(format!(
            "{}: {} => {}{}",
            site.row + 1,
            render(facts[fi], site.expr),
            shown,
            why
        ));
    }
    out
}

/// A compact source-like rendering of a lowered expression.
fn render(f: &FileFacts, e: ir::ExprId) -> String {
    use ir::Expr::*;
    let list = |l: ir::Span| {
        f.list(l)
            .iter()
            .map(|x| render(f, *x))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match f.expr(e) {
        Path(p) | Struct(p) => f.path(p).join("::"),
        Field(r, n) => format!("{}.{}", render(f, r), f.sym(n)),
        Call(c) => format!("{}()", render(f, c)),
        Method(r, n) => format!("{}.{}()", render(f, r), f.sym(n)),
        Unwrap(x) => format!("{}?", render(f, x)),
        Deref(x) => format!("*{}", render(f, x)),
        Elem(x) => format!("elem({})", render(f, x)),
        Payload(x, c, n) => format!("{}#{}.{}", render(f, x), f.sym(c), f.sym(n)),
        Tuple(l) => format!("({})", list(l)),
        Ext(n, l) => format!("{}<{}>", f.sym(n), list(l)),
        Branches(l) => format!("branches[{}]", list(l)),
        Union(l) => format!("union[{}]", list(l)),
        Closure(x) => format!("|..| {}", render(f, x)),
        Super(_) => "super()".to_string(),
        Builtin(c, _) => render(f, c),
        At(_, x) => render(f, x),
        Typed(t) => format!("{:?}", f.type_pool[t as usize]),
        Param(k) => format!("param{k}"),
        Arg(c, pos, k) => format!("arg{k}@{pos}({})", render(f, c)),
        Unknown => "?".to_string(),
    }
}

/// The innermost sub-expression whose type is unknown although its parts'
/// types are known (or which has no parts): what blocks typing `e`.
fn blocker(r: &Resolver, f: &FileFacts, e: ir::ExprId, cx: &infer::FnCx, at: u32) -> String {
    use ir::Expr::*;
    let inner = match f.expr(e) {
        Method(x, _) | Field(x, _) | Unwrap(x) | Deref(x) | Elem(x) | Payload(x, _, _) => Some(x),
        _ => None,
    };
    if let Some(x) = inner {
        if r.type_of(x, cx, at, 0) == infer::Ty::Unknown {
            return blocker(r, f, x, cx, at);
        }
    }
    let kind = match f.expr(e) {
        Path(p) if p.len == 1 => match r.local(cx, f.path_pool[p.start as usize], at) {
            Some(li) => {
                let l = f.locals[li];
                match (l.ty, l.init) {
                    (Some(t), _) => format!(
                        "local:declared-type-unresolved {:?}",
                        f.type_pool[t as usize]
                    ),
                    (None, Some(i)) => return blocker(r, f, i, cx, l.at.saturating_sub(1)),
                    (None, None) => "local:untyped".to_string(),
                }
            }
            None => "path:not-a-local".to_string(),
        },
        Method(x, n) => format!("method {} on {:?}", f.sym(n), r.type_of(x, cx, at, 0)),
        Field(x, n) => format!("field {} on {:?}", f.sym(n), r.type_of(x, cx, at, 0)),
        Call(_) => format!("call {:?}", r.pick(e, cx, at, 0)),
        Arg(c, pos, k) => format!(
            "closure arg{k}@{pos} of {} => {:?}",
            render(f, c).rsplit('.').next().unwrap_or(""),
            r.pick(c, cx, at, 0)
        ),
        Branches(_) => "match/if arms".to_string(),
        other => format!("{other:?}"),
    };
    let mut k: String = kind.chars().take(120).collect();
    if k.len() == 120 {
        k.push('…');
    }
    k
}

/// Resolve `path` (`a::b::C`) from `target`'s file scope (debugging aid).
pub fn explain_path(
    root: &FsPath,
    files: &[(&str, &FileFacts)],
    target: &str,
    path: &str,
) -> String {
    let Some(lang) = language_for(target) else {
        return String::new();
    };
    let facts: Vec<&FileFacts> = files.iter().map(|(_, f)| *f).collect();
    let layout = lang.layout(root, files);
    let tables = ScopeTables::build(&facts, &layout);
    let view = tables.view();
    let Some(fi) = files.iter().position(|(p, _)| *p == target) else {
        return String::new();
    };
    let segs: Vec<&str> = path.split("::").collect();
    let mut out = String::new();
    for n in 1..=segs.len() {
        let (defs, used) = view.resolve_path(view.global(fi as u32, 0), &segs[..n]);
        let shown: Vec<String> = defs
            .iter()
            .map(|d| match d {
                scope::Def::Module(m) => format!("Module({})", files[view.file_of(*m) as usize].0),
                scope::Def::Type(f, t) => format!(
                    "Type({}:{})",
                    files[*f as usize].0,
                    facts[*f as usize].types[*t as usize].row + 1
                ),
                scope::Def::Fn(f, i) => format!(
                    "Fn({}:{})",
                    files[*f as usize].0,
                    facts[*f as usize].fns[*i as usize].row + 1
                ),
                d => format!("{d:?}"),
            })
            .collect();
        out.push_str(&format!(
            "{} => used {used}: {shown:?}\n",
            segs[..n].join("::")
        ));
    }
    out
}
