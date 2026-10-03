//! Stages 3–4: resolve every call of the IR to a target, then propagate
//! labels to a fixpoint with per-function summaries.
//!
//! A *label* says where a value may come from: a source site (`Src`), a
//! parameter of the current function (`Param`), a piece of module/object
//! state (`State`), or the result of a call nobody can resolve (`Unk`).
//! Per function, a flow-insensitive pass computes the labels of every
//! local, return value and call argument. Its summary keeps what is not
//! yet concrete (parameters, state) so a caller can substitute its own
//! arguments; a source label that reaches a sink becomes a *flow* with a
//! witness path.
//!
//! Precision, stated plainly: flow-insensitive within a function (order of
//! statements ignored), field-insensitive (writing any field of an object
//! taints the object), context-insensitive summaries (one per function),
//! no aliasing beyond "same name / same object". These make it report
//! *more* flows than exist, never fewer — within what was resolved. What
//! was not resolved is never silent: every call with no known target is an
//! `Unk` origin, and data reaching one is reported as an escape.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use super::ir::*;
use super::models::{CbArg, Kind, Models};
use crate::parser::calls::SiteAnswer;

pub type FnRef = (u32, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Label {
    Src(u32),
    Param(u16),
    State(u32),
    Unk(u32),
}

type Labels = BTreeSet<Label>;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Resource {
    /// A module-level value: `(file, name)`.
    Global(String, String),
    /// Any field of an instance of class/type `owner` defined in `file`.
    Object(String, String),
}

impl Resource {
    pub fn show(&self) -> String {
        match self {
            Resource::Global(f, n) => format!("{f}:{n}"),
            Resource::Object(f, t) => format!("{f}:{t}.*"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SrcOrigin {
    pub class: String,
    pub at_fn: FnRef,
    pub row: u32,
    /// The qualified name that matched a model.
    pub via: String,
}

#[derive(Clone, Debug)]
pub struct UnkOrigin {
    pub at_fn: FnRef,
    pub row: u32,
    pub why: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Precision {
    /// The target is a resolved external name matched to a model.
    Resolved,
    /// The receiver's type is known only by its last path segment.
    TypeName,
    /// The receiver is untyped; only the method name matches a sink model.
    NameOnly,
}

#[derive(Clone, Debug)]
pub struct SinkSite {
    pub class: String,
    pub at_fn: FnRef,
    pub row: u32,
    pub via: String,
    pub precision: Precision,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub at_fn: FnRef,
    pub row: u32,
    pub what: String,
}

#[derive(Clone, Default, Debug)]
struct Summary {
    ret: Labels,
    /// Sinks reached by a parameter: `(sink site, Param)` -> path down.
    sinks: BTreeMap<(u32, Label), Vec<Step>>,
    /// Parameters written into state.
    writes: BTreeSet<(u32, Label)>,
    /// Parameters passed to calls with no known target.
    escapes: BTreeMap<(u32, Label), Vec<Step>>,
    /// Parameters reaching name-only sink hints.
    possible: BTreeMap<(u32, Label), Vec<Step>>,
}

impl Summary {
    fn same(&self, o: &Summary) -> bool {
        self.ret == o.ret
            && self.writes == o.writes
            && self.sinks.keys().eq(o.sinks.keys())
            && self.escapes.keys().eq(o.escapes.keys())
            && self.possible.keys().eq(o.possible.keys())
    }
}

/// A concrete finding: data from `src` reaches `sink`.
#[derive(Clone, Debug)]
pub struct Flow {
    pub src: u32,
    pub sink: u32,
    pub path: Vec<Step>,
    pub through_state: Option<u32>,
}

/// Source data passed into code nobody can resolve.
#[derive(Clone, Debug)]
pub struct Escape {
    pub src: u32,
    pub unk: u32,
    pub path: Vec<Step>,
}

/// Per-function facts: what it reads and writes, directly.
#[derive(Clone, Debug, Default)]
pub struct FnFacts {
    pub reads: BTreeSet<String>,
    pub writes: BTreeSet<String>,
    pub calls_repo: BTreeSet<FnRef>,
    pub unknown_calls: Vec<(u32, String)>,
    pub dynamic: Vec<(u32, String)>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Coverage {
    pub call_sites: usize,
    pub repo: usize,
    pub external_modeled: usize,
    pub external_unmodeled: usize,
    pub unknown: usize,
    pub unknown_by_reason: BTreeMap<String, usize>,
    pub dynamic_markers: usize,
    /// Joined to the call-graph pipeline's answer (Python, Go, Rust).
    pub pipeline_answered: usize,
}

pub struct Input<'a> {
    pub files: &'a [DfFile],
    /// Per file: pipeline answers by byte offset (empty for JS/TS).
    pub answers: &'a [HashMap<u32, (bool, SiteAnswer)>],
    /// Per file, per function: the sem entity id.
    pub fn_entity: &'a [Vec<Option<String>>],
    /// Sem entity id -> (file, type name) for class/type entities.
    pub type_entity: &'a HashMap<String, (u32, String)>,
    /// Per file: JS/TS import specifier -> repo file index.
    pub spec_file: &'a [HashMap<String, u32>],
    pub models: &'a Models,
}

pub struct Output {
    pub srcs: Vec<SrcOrigin>,
    pub unks: Vec<UnkOrigin>,
    pub states: Vec<Resource>,
    pub sinks: Vec<SinkSite>,
    pub flows: Vec<Flow>,
    /// Values produced by unresolved calls that reach a sink: `(unk, sink)`.
    pub unknown_flows: Vec<(u32, u32, Vec<Step>)>,
    pub escapes: Vec<Escape>,
    /// Name-only sink hints reached by source data.
    pub possible: Vec<Flow>,
    pub facts: Vec<Vec<FnFacts>>,
    pub coverage: Coverage,
    /// The fixpoint hit its budget: results are a partial under-approximation.
    pub incomplete: bool,
}

#[derive(Clone, Debug)]
enum Target {
    Repo(Vec<FnRef>, usize),
    External(String, Precision),
    ExternalTypeLast(String, String),
    Unknown(String),
}

struct Interner<K: std::hash::Hash + Eq> {
    ids: HashMap<K, u32>,
}

impl<K: std::hash::Hash + Eq + Clone> Interner<K> {
    fn new() -> Self {
        Interner { ids: HashMap::new() }
    }
    fn get<V>(&mut self, k: K, table: &mut Vec<V>, make: impl FnOnce() -> V) -> u32 {
        if let Some(&i) = self.ids.get(&k) {
            return i;
        }
        let i = table.len() as u32;
        table.push(make());
        self.ids.insert(k, i);
        i
    }
}

pub struct Engine<'a> {
    inp: Input<'a>,
    srcs: Vec<SrcOrigin>,
    src_ids: Interner<(FnRef, u32, String)>,
    unks: Vec<UnkOrigin>,
    unk_ids: Interner<(FnRef, u32)>,
    states: Vec<Resource>,
    state_ids: Interner<Resource>,
    sinks: Vec<SinkSite>,
    sink_ids: Interner<(FnRef, u32, String, Precision)>,
    summaries: Vec<Vec<Summary>>,
    callers: HashMap<FnRef, BTreeSet<FnRef>>,
    /// Labels written into each state, with the first writer.
    state_taint: HashMap<u32, BTreeMap<Label, (FnRef, u32)>>,
    /// How a source/unknown label first reached a function from a callee's
    /// return: `(fn, label) -> (callee, row of the call)`.
    arrive: HashMap<(FnRef, Label), (FnRef, u32)>,
    flows: BTreeMap<(u32, u32), Flow>,
    unknown_flows: BTreeMap<(u32, u32), Vec<Step>>,
    escapes: BTreeMap<(u32, u32), Escape>,
    possible: BTreeMap<(u32, u32), Flow>,
    /// State labels reaching sinks / unknown calls, resolved at the end.
    state_sinks: BTreeMap<(u32, u32), Vec<Step>>,
    state_escapes: BTreeMap<(u32, u32), Vec<Step>>,
    state_possible: BTreeMap<(u32, u32), Vec<Step>>,
    /// Qualified types of module values, per file; of object fields.
    global_types: Vec<HashMap<String, String>>,
    field_types: HashMap<(u32, String, String), String>,
    /// Top-level functions of each file by name; methods by (owner, name).
    fn_by_name: Vec<HashMap<String, Vec<u32>>>,
    methods: Vec<HashMap<(String, String), Vec<u32>>>,
    entity_fn: HashMap<String, FnRef>,
    /// Subclasses by base name, across the repo (JS/TS `this.m()` overrides).
    subclasses: HashMap<String, Vec<(u32, String)>>,
    facts: Vec<Vec<FnFacts>>,
    coverage: Coverage,
    collecting: bool,
    counting: bool,
}

/// Evaluation state of one function.
struct Cx {
    fr: FnRef,
    env: HashMap<String, Labels>,
    types: HashMap<String, String>,
    locals: HashSet<String>,
    results: Vec<Labels>,
    targets: Vec<Option<Target>>,
    summary: Summary,
}

const MAX_FN_ITER: usize = 16;
/// Sink classes a name-only hint may report.
const HINT_CLASSES: &[&str] = &["db", "exec", "template"];
const MAX_WORK: usize = 400_000;

impl<'a> Engine<'a> {
    pub fn new(inp: Input<'a>) -> Self {
        let n = inp.files.len();
        let mut fn_by_name = vec![HashMap::new(); n];
        let mut methods = vec![HashMap::new(); n];
        let mut subclasses: HashMap<String, Vec<(u32, String)>> = HashMap::new();
        let mut entity_fn = HashMap::new();
        for (fi, f) in inp.files.iter().enumerate() {
            for (i, d) in f.fns.iter().enumerate() {
                if d.is_module {
                    continue;
                }
                match &d.owner {
                    None => fn_by_name[fi].entry(d.name.clone()).or_insert_with(Vec::new).push(i as u32),
                    Some(o) => methods[fi].entry((o.clone(), d.name.clone())).or_insert_with(Vec::new).push(i as u32),
                }
            }
            for (c, bases) in &f.classes {
                for b in bases {
                    let b = b.rsplit('.').next().unwrap_or(b).to_string();
                    subclasses.entry(b).or_default().push((fi as u32, c.clone()));
                }
            }
            for (i, e) in inp.fn_entity[fi].iter().enumerate() {
                if let Some(e) = e {
                    entity_fn.entry(e.clone()).or_insert((fi as u32, i as u32));
                }
            }
        }
        let summaries = inp.files.iter().map(|f| vec![Summary::default(); f.fns.len()]).collect();
        let facts = inp.files.iter().map(|f| vec![FnFacts::default(); f.fns.len()]).collect();
        Engine {
            srcs: Vec::new(),
            src_ids: Interner::new(),
            unks: Vec::new(),
            unk_ids: Interner::new(),
            states: Vec::new(),
            state_ids: Interner::new(),
            sinks: Vec::new(),
            sink_ids: Interner::new(),
            summaries,
            callers: HashMap::new(),
            state_taint: HashMap::new(),
            arrive: HashMap::new(),
            flows: BTreeMap::new(),
            unknown_flows: BTreeMap::new(),
            escapes: BTreeMap::new(),
            possible: BTreeMap::new(),
            state_sinks: BTreeMap::new(),
            state_escapes: BTreeMap::new(),
            state_possible: BTreeMap::new(),
            global_types: vec![HashMap::new(); n],
            field_types: HashMap::new(),
            fn_by_name,
            methods,
            entity_fn,
            subclasses,
            facts,
            coverage: Coverage::default(),
            collecting: false,
            counting: false,
            inp,
        }
    }

    fn df(&self, fr: FnRef) -> &'a DfFn {
        &self.inp.files[fr.0 as usize].fns[fr.1 as usize]
    }

    fn file(&self, fr: FnRef) -> &'a DfFile {
        &self.inp.files[fr.0 as usize]
    }

    fn lang(&self, fr: FnRef) -> Lang {
        Lang::for_path(&self.file(fr).path).unwrap_or(Lang::Python)
    }

    pub fn run(mut self) -> Output {
        self.type_prepass();
        let all: Vec<FnRef> = self
            .inp
            .files
            .iter()
            .enumerate()
            .flat_map(|(fi, f)| (0..f.fns.len()).map(move |i| (fi as u32, i as u32)))
            .collect();
        let mut queue: VecDeque<FnRef> = all.iter().copied().collect();
        let mut queued: HashSet<FnRef> = all.iter().copied().collect();
        let mut work = 0usize;
        let mut incomplete = false;
        while let Some(fr) = queue.pop_front() {
            queued.remove(&fr);
            work += 1;
            if work > MAX_WORK {
                incomplete = true;
                break;
            }
            let s = self.analyze(fr);
            let changed = !self.summaries[fr.0 as usize][fr.1 as usize].same(&s);
            self.summaries[fr.0 as usize][fr.1 as usize] = s;
            if changed {
                for c in self.callers.get(&fr).cloned().unwrap_or_default() {
                    if queued.insert(c) {
                        queue.push_back(c);
                    }
                }
            }
        }
        // Final pass: facts and coverage, once per function.
        self.collecting = true;
        for &fr in &all {
            let _ = self.analyze(fr);
        }
        self.resolve_state();
        Output {
            srcs: self.srcs,
            unks: self.unks,
            states: self.states,
            sinks: self.sinks,
            flows: self.flows.into_values().collect(),
            unknown_flows: self.unknown_flows.into_iter().map(|((u, s), p)| (u, s, p)).collect(),
            escapes: self.escapes.into_values().collect(),
            possible: self.possible.into_values().collect(),
            facts: self.facts,
            coverage: self.coverage,
            incomplete,
        }
    }

    // ------------------------------------------------------------------ types

    /// Module values' and object fields' types, from assignments of calls
    /// whose model says what they return (`app = Flask()`,
    /// `self.db = sqlite3.connect(..)`). Two rounds so a type can build on
    /// another (`conn.cursor()`).
    fn type_prepass(&mut self) {
        for _ in 0..2 {
            for fi in 0..self.inp.files.len() {
                let f = &self.inp.files[fi];
                for (i, d) in f.fns.iter().enumerate() {
                    let fr = (fi as u32, i as u32);
                    let types = self.local_types(fr);
                    for s in &d.stmts {
                        let Stmt::Assign { to, from, .. } = s else { continue };
                        let Some(t) = self.val_type(fr, from, &types) else { continue };
                        for p in to {
                            match p {
                                Place::Name(n) | Place::Local(n) if d.is_module => {
                                    self.global_types[fi].insert(n.clone(), t.clone());
                                }
                                Place::Attr { base, chain, .. } if Some(base) == d.self_name.as_ref() => {
                                    if let (Some(owner), Some(field)) = (&d.owner, field_of(chain)) {
                                        self.field_types.insert((fi as u32, owner.clone(), field), t.clone());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    /// Declared parameter types and assignment-derived local types.
    fn local_types(&self, fr: FnRef) -> HashMap<String, String> {
        let d = self.df(fr);
        let mut types: HashMap<String, String> = HashMap::new();
        for p in &d.params {
            if let Some(t) = p.ty.as_deref().and_then(|t| self.qualify_type(fr, t)) {
                for n in p.name.split(',') {
                    types.insert(n.to_string(), t.clone());
                }
            }
        }
        for _ in 0..2 {
            for s in &d.stmts {
                let Stmt::Assign { to, from, .. } = s else { continue };
                if let Some(t) = self.val_type(fr, from, &types) {
                    for p in to {
                        if let Place::Local(n) | Place::Name(n) = p {
                            types.insert(n.clone(), t.clone());
                        }
                    }
                }
            }
        }
        // callback parameters with declared model types
        for c in &d.calls {
            if c.callbacks.is_empty() {
                continue;
            }
            for (cb, ty) in self.callback_facts(fr, c, &types) {
                if let (name, Some(t)) = (cb, ty.1) {
                    types.insert(name, t);
                }
            }
        }
        types
    }

    /// The qualified type of a value that is exactly one call (or one
    /// typed name).
    fn val_type(&self, fr: FnRef, v: &Val, types: &HashMap<String, String>) -> Option<String> {
        match (v.reads.as_slice(), v.calls.as_slice()) {
            ([], [c]) => self.call_type(fr, *c, types),
            ([r], []) if r.chain == r.base => types.get(&r.base).cloned(),
            _ => None,
        }
    }

    fn call_type(&self, fr: FnRef, ci: u32, types: &HashMap<String, String>) -> Option<String> {
        let d = self.df(fr);
        let c = d.calls.get(ci as usize)?;
        let q = match self.qualify(fr, c, types) {
            Target::External(q, _) => q,
            _ => return None,
        };
        let lang = self.lang(fr);
        if c.construct {
            return Some(q);
        }
        self.inp.models.lookup(lang, &q).into_iter().find_map(|m| match &m.kind {
            Kind::Returns(t) => Some(t.clone()),
            _ => None,
        })
    }

    /// A type as written (`*http.Request`, `web::Json<T>`, `Request`)
    /// qualified through the file's imports.
    fn qualify_type(&self, fr: FnRef, t: &str) -> Option<String> {
        let lang = self.lang(fr);
        let mut t = t.trim();
        loop {
            let before = t;
            t = t.trim_start_matches(['*', '&', '[', ']']).trim_start_matches("mut ").trim_start_matches("dyn ").trim_start_matches("impl ").trim();
            if let Some(x) = t.strip_prefix("Optional[") {
                t = x.trim_end_matches(']');
            }
            if t == before {
                break;
            }
        }
        let t = t.split(['<', '[', '|', ' ', '(']).next()?.trim_matches(|c| c == '"' || c == '\'');
        if t.is_empty() {
            return None;
        }
        let sep = if t.contains("::") { "::" } else { "." };
        let (base, rest) = match t.split_once(sep) {
            Some((b, r)) => (b, format!("{sep}{r}")),
            None => (t, String::new()),
        };
        let imp = self.file(fr).imports.iter().find(|i| i.local == base);
        match imp {
            Some(i) => Some(format!("{}{}", i.path, rest.replace(sep, lang.sep()))),
            None if lang == Lang::Go || lang == Lang::Rust => None,
            None => None,
        }
    }

    // ---------------------------------------------------------------- targets

    fn answer(&self, fr: FnRef, at: u32) -> Option<&'a SiteAnswer> {
        self.inp.answers.get(fr.0 as usize)?.get(&at).map(|(_, a)| a)
    }

    fn repo_targets(&self, ids: &[String]) -> Target {
        let mut fns = Vec::new();
        let mut opaque = 0;
        for id in ids {
            if let Some(&f) = self.entity_fn.get(id) {
                fns.push(f);
                continue;
            }
            if let Some((file, ty)) = self.inp.type_entity.get(id) {
                let ctors: Vec<FnRef> = ["__init__", "constructor", "new", "__new__"]
                    .iter()
                    .flat_map(|n| self.methods[*file as usize].get(&(ty.clone(), n.to_string())).cloned().unwrap_or_default())
                    .map(|i| (*file, i))
                    .collect();
                if ctors.is_empty() {
                    opaque += 1;
                } else {
                    fns.extend(ctors);
                }
                continue;
            }
            opaque += 1;
        }
        Target::Repo(fns, opaque)
    }

    fn import_of(&self, fr: FnRef, base: &str) -> Option<&'a Import> {
        self.file(fr).imports.iter().rev().find(|i| i.local == base)
    }

    /// What a call targets. Repo answers come from the call-graph pipeline
    /// (Python, Go, Rust) or, for JS/TS, from imports and same-file names
    /// only. External names are qualified through imports; anything else is
    /// unknown, with a reason.
    fn qualify(&self, fr: FnRef, c: &Call, types: &HashMap<String, String>) -> Target {
        let d = self.df(fr);
        let lang = self.lang(fr);
        let sep = lang.sep();
        let ans = self.answer(fr, c.at);
        let pipeline = !self.inp.answers[fr.0 as usize].is_empty() || lang != Lang::Ts;
        let from_pipeline = |ans: Option<&SiteAnswer>, fallback: Target| -> Target {
            match ans {
                Some(SiteAnswer::Defs(ids)) => self.repo_targets(ids),
                Some(SiteAnswer::Unknown(why)) => Target::Unknown((*why).to_string()),
                Some(SiteAnswer::External(Some(t))) => match &fallback {
                    Target::External(..) => fallback,
                    _ => Target::ExternalTypeLast(t.clone(), String::new()),
                },
                _ => fallback,
            }
        };
        match &c.callee {
            Callee::Dynamic => Target::Unknown("computed callee".into()),
            Callee::Method { recv, name, .. } => {
                let t = self.val_type(fr, recv, types);
                if let Some(t) = t {
                    return Target::External(format!("{t}{sep}{name}"), Precision::Resolved);
                }
                match ans {
                    Some(SiteAnswer::Defs(ids)) => self.repo_targets(ids),
                    Some(SiteAnswer::External(Some(t))) => Target::ExternalTypeLast(t.clone(), name.clone()),
                    Some(SiteAnswer::External(None)) => Target::External(format!("?{sep}{name}"), Precision::Resolved),
                    Some(SiteAnswer::Unknown(why)) => Target::Unknown(format!("{why}: .{name}()")),
                    _ => Target::Unknown(format!("untyped receiver: .{name}()")),
                }
            }
            Callee::Path { chain, base } => {
                let rest = &chain[base.len().min(chain.len())..];
                let method = chain.rsplit(['.', ':']).next().unwrap_or(chain);
                let is_self = d.self_name.as_deref() == Some(base.as_str());
                let base_plain = base.trim_end_matches('!');
                // 1. a local value's method
                if !is_self && d.params.iter().any(|p| p.name.split(',').any(|n| n == base)) || (!is_self && types.contains_key(base.as_str()) && !d.is_module) {
                    if let Some(t) = types.get(base.as_str()) {
                        if !rest.is_empty() {
                            return Target::External(format!("{t}{}", rest.replace('.', sep)), Precision::Resolved);
                        }
                    }
                }
                // a module value with a known type (`log = logging.getLogger()`)
                let bound_here = d.params.iter().any(|p| p.name.split(',').any(|n| n == base)) || types.contains_key(base.as_str());
                if !is_self && !bound_here && !rest.is_empty() && self.file(fr).globals.iter().any(|g| g == base) {
                    if let Some(t) = self.global_types[fr.0 as usize].get(base.as_str()) {
                        return Target::External(format!("{t}{}", rest.replace('.', sep)), Precision::Resolved);
                    }
                }
                // a field of self with a known type
                if is_self {
                    if let (Some(owner), Some((field, tail))) = (&d.owner, split_field(rest)) {
                        if let Some(t) = self.field_types.get(&(fr.0, owner.clone(), field)) {
                            if !tail.is_empty() {
                                return Target::External(format!("{t}{}", tail.replace('.', sep)), Precision::Resolved);
                            }
                        }
                    }
                }
                if pipeline && lang != Lang::Ts {
                    match ans {
                        Some(SiteAnswer::Defs(ids)) => return self.repo_targets(ids),
                        Some(SiteAnswer::Unknown(why)) => {
                            // an import the resolver could not follow into a
                            // library may still be a modeled name
                            if let Some(i) = self.import_of(fr, base_plain) {
                                let q = qualified(&i.path, rest, base, lang);
                                if !self.inp.models.lookup(lang, &q).is_empty() {
                                    return Target::External(q, Precision::Resolved);
                                }
                            }
                            return Target::Unknown(format!("{why}: {chain}()"));
                        }
                        Some(SiteAnswer::External(recv)) => {
                            if let Some(i) = self.import_of(fr, base_plain) {
                                return Target::External(qualified(&i.path, rest, base, lang), Precision::Resolved);
                            }
                            if let Some(t) = self.global_types[fr.0 as usize].get(base.as_str()) {
                                return Target::External(format!("{t}{}", rest.replace('.', sep)), Precision::Resolved);
                            }
                            if let Some(t) = recv {
                                return Target::ExternalTypeLast(t.clone(), method.to_string());
                            }
                            let local = d.params.iter().any(|p| p.name.split(',').any(|n| n == base)) || types.contains_key(base.as_str()) || is_self;
                            if local {
                                return Target::External(format!("?{sep}{method}"), Precision::Resolved);
                            }
                            return Target::External(builtin(lang, chain), Precision::Resolved);
                        }
                        _ => {}
                    }
                    // no pipeline answer (a macro, or a site it does not record)
                    if chain.ends_with('!') {
                        return match self.import_of(fr, base_plain) {
                            Some(i) => Target::External(format!("{}{}", i.path.trim_end_matches('!'), if rest.is_empty() { "!".to_string() } else { rest.to_string() }), Precision::Resolved),
                            None => Target::External(chain.clone(), Precision::Resolved),
                        };
                    }
                    if let Some(i) = self.import_of(fr, base_plain) {
                        return Target::External(qualified(&i.path, rest, base, lang), Precision::Resolved);
                    }
                    if let Some(t) = self.global_types[fr.0 as usize].get(base.as_str()) {
                        return Target::External(format!("{t}{}", rest.replace('.', sep)), Precision::Resolved);
                    }
                    return from_pipeline(None, Target::Unknown(format!("unresolved: {chain}()")));
                }
                // ---- JS/TS: imports and same-file names only
                if is_self {
                    if let (Some(owner), Some(m)) = (&d.owner, rest.strip_prefix('.')) {
                        if !m.contains('.') {
                            let mut fns: Vec<FnRef> = self.methods[fr.0 as usize]
                                .get(&(owner.clone(), m.to_string()))
                                .into_iter()
                                .flatten()
                                .map(|&i| (fr.0, i))
                                .collect();
                            // overrides in subclasses: `this.m()` may run them
                            for (sf, sc) in self.subclasses.get(owner).into_iter().flatten() {
                                fns.extend(self.methods[*sf as usize].get(&(sc.clone(), m.to_string())).into_iter().flatten().map(|&i| (*sf, i)));
                            }
                            if !fns.is_empty() {
                                return Target::Repo(fns, 0);
                            }
                        }
                    }
                    return Target::Unknown(format!("member of this: {chain}()"));
                }
                if let Some(t) = types.get(base.as_str()).or_else(|| self.global_types[fr.0 as usize].get(base.as_str())) {
                    if !rest.is_empty() {
                        return Target::External(format!("{t}{rest}"), Precision::Resolved);
                    }
                }
                let local = d.params.iter().any(|p| p.name.split(',').any(|n| n == base))
                    || d.stmts.iter().any(|s| matches!(s, Stmt::Assign { to, .. } if to.iter().any(|p| matches!(p, Place::Local(n) if n == base))));
                if local && !d.is_module {
                    return Target::Unknown(format!("untyped local: {chain}()"));
                }
                if let Some(i) = self.import_of(fr, base) {
                    if let Some(spec) = &i.spec {
                        if let Some(&tf) = self.inp.spec_file[fr.0 as usize].get(spec) {
                            let name = match (i.member.as_deref(), rest.strip_prefix('.')) {
                                (Some("default"), _) => None,
                                (Some(m), None) => Some(m.to_string()),
                                (None, Some(m)) if !m.contains('.') => Some(m.to_string()),
                                _ => None,
                            };
                            let fns: Vec<FnRef> = name
                                .and_then(|n| self.fn_by_name[tf as usize].get(&n))
                                .into_iter()
                                .flatten()
                                .map(|&i| (tf, i))
                                .collect();
                            if !fns.is_empty() {
                                return Target::Repo(fns, 0);
                            }
                            return Target::Unknown(format!("repo import not resolved to a function: {chain}()"));
                        }
                    }
                    return Target::External(format!("{}{}", i.path, rest), Precision::Resolved);
                }
                if rest.is_empty() {
                    if let Some(fns) = self.fn_by_name[fr.0 as usize].get(base) {
                        return Target::Repo(fns.iter().map(|&i| (fr.0, i)).collect(), 0);
                    }
                }
                if self.file(fr).globals.iter().any(|g| g == base) {
                    return Target::Unknown(format!("untyped module value: {chain}()"));
                }
                Target::External(chain.clone(), Precision::Resolved)
            }
        }
    }

    /// Callback parameters a model gives facts to: `(param name, (source
    /// class, type))`.
    fn callback_facts(&self, fr: FnRef, c: &Call, types: &HashMap<String, String>) -> Vec<(String, (Option<String>, Option<String>))> {
        let lang = self.lang(fr);
        let q = match self.qualify(fr, c, types) {
            Target::External(q, _) => q,
            _ => return Vec::new(),
        };
        let mut out = Vec::new();
        for m in self.inp.models.lookup(lang, &q) {
            let Kind::Callback(which, params) = &m.kind else { continue };
            let last = c.args.len().saturating_sub(1) as u32;
            for (idx, names) in &c.callbacks {
                let ok = match which {
                    CbArg::Each => true,
                    CbArg::Last => *idx == last,
                    CbArg::Index(i) => idx == i,
                };
                if !ok {
                    continue;
                }
                for p in params {
                    if let Some(n) = names.get(p.index as usize) {
                        out.push((n.clone(), (p.source.clone(), p.ty.clone())));
                    }
                }
            }
        }
        out
    }

    // ---------------------------------------------------------------- origins

    fn src(&mut self, fr: FnRef, at: u32, row: u32, class: &str, via: &str) -> Label {
        let key = (fr, at, class.to_string());
        let (class, via) = (class.to_string(), via.to_string());
        Label::Src(self.src_ids.get(key, &mut self.srcs, || SrcOrigin { class, at_fn: fr, row, via }))
    }

    fn unk(&mut self, fr: FnRef, at: u32, row: u32, why: &str) -> Label {
        let why = why.to_string();
        Label::Unk(self.unk_ids.get((fr, at), &mut self.unks, || UnkOrigin { at_fn: fr, row, why }))
    }

    fn state(&mut self, r: Resource) -> u32 {
        let k = r.clone();
        self.state_ids.get(k, &mut self.states, || r)
    }

    fn sink_site(&mut self, fr: FnRef, at: u32, row: u32, class: &str, via: &str, precision: Precision) -> u32 {
        let (c2, v2) = (class.to_string(), via.to_string());
        self.sink_ids.get((fr, at, class.to_string(), precision), &mut self.sinks, || SinkSite {
            class: c2,
            at_fn: fr,
            row,
            via: v2,
            precision,
        })
    }

    /// The path by which source/unknown label `l` reached `fr` through
    /// callee returns, starting at its origin.
    fn up_path(&self, fr: FnRef, l: Label) -> Vec<Step> {
        let (origin_fn, origin_row, what) = match l {
            Label::Src(o) => {
                let s = &self.srcs[o as usize];
                (s.at_fn, s.row, format!("source {} via {}", s.class, s.via))
            }
            Label::Unk(u) => {
                let s = &self.unks[u as usize];
                (s.at_fn, s.row, format!("unknown: {}", s.why))
            }
            _ => return Vec::new(),
        };
        let mut steps = Vec::new();
        let mut cur = fr;
        let mut guard = 0;
        while cur != origin_fn && guard < 64 {
            let Some(&(from, row)) = self.arrive.get(&(cur, l)) else { break };
            steps.push(Step { at_fn: cur, row, what: format!("returned from {}", self.fn_label(from)) });
            cur = from;
            guard += 1;
        }
        steps.push(Step { at_fn: origin_fn, row: origin_row, what });
        steps.reverse();
        steps
    }

    pub fn fn_label(&self, fr: FnRef) -> String {
        let d = self.df(fr);
        match &d.owner {
            Some(o) => format!("{o}.{}", d.name),
            None => d.name.clone(),
        }
    }

    // ---------------------------------------------------------------- analysis

    fn analyze(&mut self, fr: FnRef) -> Summary {
        let d = self.df(fr);
        let file = self.file(fr);
        let types = self.local_types(fr);
        let mut cx = Cx {
            fr,
            env: HashMap::new(),
            types,
            locals: HashSet::new(),
            results: vec![Labels::new(); d.calls.len()],
            targets: vec![None; d.calls.len()],
            summary: Summary::default(),
        };
        for (i, p) in d.params.iter().enumerate() {
            for n in p.name.split(',') {
                cx.env.entry(n.to_string()).or_default().insert(Label::Param(i as u16));
                cx.locals.insert(n.to_string());
            }
            // framework request objects
            if let Some(t) = cx.types.get(p.name.split(',').next().unwrap_or("")).cloned() {
                for m in self.inp.models.lookup(self.lang(fr), &t) {
                    if let Kind::ParamSource(class) = &m.kind {
                        let l = self.src(fr, u32::MAX - i as u32, d.row, class, &format!("parameter `{}: {t}`", p.name));
                        for n in p.name.split(',') {
                            cx.env.entry(n.to_string()).or_default().insert(l);
                        }
                    }
                }
            }
        }
        if let Some(s) = &d.self_name {
            cx.locals.insert(s.clone());
            if let Some(o) = &d.owner {
                let st = self.state(Resource::Object(file.path.clone(), o.clone()));
                cx.env.entry(s.clone()).or_default().insert(Label::State(st));
            }
        }
        // locals: every `Local` binding (at module level, those of inlined
        // closures — module state is `Name`)
        for s in &d.stmts {
            if let Stmt::Assign { to, .. } = s {
                for p in to {
                    if let Place::Local(n) = p {
                        if !d.is_module || !file.globals.contains(n) {
                            cx.locals.insert(n.clone());
                        }
                    }
                }
            }
        }
        // callback parameters (`app.get(path, (req, res) => ..)`)
        for (ci, c) in d.calls.iter().enumerate() {
            if c.callbacks.is_empty() {
                continue;
            }
            for (name, (source, _)) in self.callback_facts(fr, c, &cx.types) {
                if let Some(class) = source {
                    let via = format!("callback parameter `{name}`");
                    let l = self.src(fr, c.at.wrapping_add(1 + ci as u32 % 7), c.row, &class, &via);
                    cx.env.entry(name).or_default().insert(l);
                }
            }
        }
        for ci in 0..d.calls.len() {
            let t = self.qualify(fr, &d.calls[ci], &cx.types);
            cx.targets[ci] = Some(t);
        }
        for _ in 0..MAX_FN_ITER {
            let before: usize = cx.env.values().map(|s| s.len()).sum::<usize>() + cx.results.iter().map(|s| s.len()).sum::<usize>() + cx.summary.ret.len();
            for ci in 0..d.calls.len() {
                let r = self.eval_call(&mut cx, ci as u32);
                cx.results[ci].extend(r);
            }
            for s in &d.stmts {
                match s {
                    Stmt::Assign { to, from, row } => {
                        let l = self.labels(&mut cx, from, false);
                        for p in to {
                            self.assign(&mut cx, p, &l, *row);
                        }
                    }
                    Stmt::Eval(v) => {
                        let _ = self.labels(&mut cx, v, false);
                    }
                    Stmt::Return(v) => {
                        let l = self.labels(&mut cx, v, false);
                        cx.summary.ret.extend(l);
                    }
                }
            }
            let after: usize = cx.env.values().map(|s| s.len()).sum::<usize>() + cx.results.iter().map(|s| s.len()).sum::<usize>() + cx.summary.ret.len();
            if after == before {
                break;
            }
        }
        if self.collecting {
            self.counting = true;
            for ci in 0..d.calls.len() {
                let _ = self.eval_call(&mut cx, ci as u32);
            }
            self.counting = false;
            self.collect_facts(&mut cx);
        }
        cx.summary
    }

    /// Labels of a value; with `facts`, record reads.
    fn labels(&mut self, cx: &mut Cx, v: &Val, facts: bool) -> Labels {
        let d = self.df(cx.fr);
        let file = self.file(cx.fr);
        let lang = self.lang(cx.fr);
        let mut out = Labels::new();
        for r in &v.reads {
            let is_self = d.self_name.as_deref() == Some(r.base.as_str());
            if cx.locals.contains(&r.base) || cx.env.contains_key(&r.base) {
                if let Some(l) = cx.env.get(&r.base) {
                    out.extend(l.iter().copied());
                }
                if facts && is_self {
                    if let (Some(o), Some(f)) = (&d.owner, field_of(&r.chain)) {
                        self.facts[cx.fr.0 as usize][cx.fr.1 as usize].reads.insert(format!("field:{o}.{f}"));
                    }
                }
                if !is_self {
                    // a typed local's attribute can still be a modeled source
                    if let Some(t) = cx.types.get(&r.base) {
                        let rest = &r.chain[r.base.len()..];
                        let q = format!("{t}{}", rest.replace('.', lang.sep()));
                        self.read_models(cx, &q, r, &mut out, facts);
                    }
                }
                continue;
            }
            // module state
            let global = if d.is_module || file.globals.iter().any(|g| g == &r.base) && !cx.locals.contains(&r.base) {
                if file.globals.iter().any(|g| g == &r.base) {
                    Some(Resource::Global(file.path.clone(), r.base.clone()))
                } else {
                    None
                }
            } else {
                None
            };
            let global = global.or_else(|| match self.answer(cx.fr, r.at) {
                Some(SiteAnswer::Value(f, n)) => Some(Resource::Global(f.clone(), n.clone())),
                _ => None,
            });
            let global = global.or_else(|| {
                let i = self.import_of(cx.fr, &r.base)?;
                let tf = *self.inp.spec_file[cx.fr.0 as usize].get(i.spec.as_ref()?)?;
                let m = i.member.as_ref()?;
                let f = &self.inp.files[tf as usize];
                f.globals.iter().any(|g| g == m).then(|| Resource::Global(f.path.clone(), m.clone()))
            });
            if let Some(g) = global {
                if facts {
                    self.facts[cx.fr.0 as usize][cx.fr.1 as usize].reads.insert(format!("global:{}", g.show()));
                }
                let st = self.state(g);
                out.insert(Label::State(st));
            }
            // reads of modeled names (`os.environ`, `flask.request.args`)
            let q = match self.import_of(cx.fr, &r.base) {
                Some(i) => Some(qualified(&i.path, &r.chain[r.base.len()..], &r.base, lang)),
                None if !file.globals.iter().any(|g| g == &r.base) => Some(builtin(lang, &r.chain)),
                None => None,
            };
            if let Some(q) = q {
                self.read_models(cx, &q, r, &mut out, facts);
            }
        }
        for &c in &v.calls {
            out.extend(cx.results[c as usize].iter().copied());
        }
        out
    }

    fn read_models(&mut self, cx: &mut Cx, q: &str, r: &Read, out: &mut Labels, facts: bool) {
        let lang = self.lang(cx.fr);
        let ms: Vec<String> = self
            .inp
            .models
            .lookup(lang, q)
            .into_iter()
            .filter_map(|m| match &m.kind {
                Kind::Source(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        for class in ms {
            let l = self.src(cx.fr, r.at, r.row, &class, q);
            out.insert(l);
            if facts {
                self.facts[cx.fr.0 as usize][cx.fr.1 as usize].reads.insert(class);
            }
        }
    }

    fn assign(&mut self, cx: &mut Cx, p: &Place, l: &Labels, row: u32) {
        let d = self.df(cx.fr);
        let file = self.file(cx.fr);
        match p {
            Place::Local(n) if cx.locals.contains(n) => {
                cx.env.entry(n.clone()).or_default().extend(l.iter().copied());
            }
            Place::Local(n) | Place::Name(n) => {
                let is_global = !cx.locals.contains(n) && (d.is_module || file.globals.iter().any(|g| g == n));
                if is_global {
                    let st = self.state(Resource::Global(file.path.clone(), n.clone()));
                    for &x in l {
                        self.write_state(cx, st, x, row);
                    }
                    if d.is_module {
                        cx.env.entry(n.clone()).or_default().extend(l.iter().copied());
                    }
                } else {
                    cx.env.entry(n.clone()).or_default().extend(l.iter().copied());
                }
            }
            Place::Attr { base, .. } => {
                let is_self = d.self_name.as_deref() == Some(base.as_str());
                if is_self {
                    if let Some(o) = &d.owner {
                        let st = self.state(Resource::Object(file.path.clone(), o.clone()));
                        for &x in l {
                            self.write_state(cx, st, x, row);
                        }
                    }
                    cx.env.entry(base.clone()).or_default().extend(l.iter().copied());
                } else if cx.locals.contains(base) {
                    cx.env.entry(base.clone()).or_default().extend(l.iter().copied());
                } else if d.is_module || file.globals.iter().any(|g| g == base) {
                    let st = self.state(Resource::Global(file.path.clone(), base.clone()));
                    for &x in l {
                        self.write_state(cx, st, x, row);
                    }
                    cx.env.entry(base.clone()).or_default().extend(l.iter().copied());
                } else {
                    cx.env.entry(base.clone()).or_default().extend(l.iter().copied());
                }
            }
            Place::Unknown => {}
        }
    }

    fn write_state(&mut self, cx: &mut Cx, st: u32, l: Label, row: u32) {
        match l {
            Label::Param(_) => {
                cx.summary.writes.insert((st, l));
            }
            Label::State(s) if s == st => {}
            _ => {
                self.state_taint.entry(st).or_default().entry(l).or_insert((cx.fr, row));
            }
        }
    }

    fn record_sink(&mut self, cx: &mut Cx, site: u32, l: Label, path: Vec<Step>, possible: bool) {
        match l {
            Label::Src(o) => {
                let mut full = self.up_path(cx.fr, l);
                full.extend(path);
                let map = if possible { &mut self.possible } else { &mut self.flows };
                map.entry((o, site)).or_insert(Flow { src: o, sink: site, path: full, through_state: None });
            }
            Label::Unk(u) => {
                if !possible {
                    let mut full = self.up_path(cx.fr, l);
                    full.extend(path);
                    self.unknown_flows.entry((u, site)).or_insert(full);
                }
            }
            Label::State(r) => {
                let map = if possible { &mut self.state_possible } else { &mut self.state_sinks };
                map.entry((r, site)).or_insert(path);
            }
            Label::Param(_) => {
                let map = if possible { &mut cx.summary.possible } else { &mut cx.summary.sinks };
                map.entry((site, l)).or_insert(path);
            }
        }
    }

    fn record_escape(&mut self, cx: &mut Cx, unk: u32, l: Label, path: Vec<Step>) {
        match l {
            Label::Src(o) => {
                let mut full = self.up_path(cx.fr, l);
                full.extend(path);
                self.escapes.entry((o, unk)).or_insert(Escape { src: o, unk, path: full });
            }
            Label::State(r) => {
                self.state_escapes.entry((r, unk)).or_insert(path);
            }
            Label::Param(_) => {
                cx.summary.escapes.entry((unk, l)).or_insert(path);
            }
            Label::Unk(_) => {}
        }
    }

    fn eval_call(&mut self, cx: &mut Cx, ci: u32) -> Labels {
        let d = self.df(cx.fr);
        let c = &d.calls[ci as usize];
        let lang = self.lang(cx.fr);
        let args: Vec<Labels> = c.args.iter().map(|a| self.labels(cx, a, false)).collect();
        let kwargs: Vec<(String, Labels)> = c.kwargs.iter().map(|(k, a)| (k.clone(), self.labels(cx, a, false))).collect();
        // the receiver: the object whose method is called
        let recv: Labels = match &c.callee {
            Callee::Path { base, chain } if chain != base => {
                let v = Val { reads: vec![Read { base: base.clone(), chain: base.clone(), at: u32::MAX, row: c.row }], calls: Vec::new() };
                if cx.locals.contains(base) || cx.env.contains_key(base) {
                    cx.env.get(base).cloned().unwrap_or_default()
                } else if self.file(cx.fr).globals.iter().any(|g| g == base) {
                    self.labels(cx, &v, false)
                } else {
                    Labels::new()
                }
            }
            Callee::Method { recv, .. } => self.labels(cx, recv, false),
            _ => Labels::new(),
        };
        let mut all: Labels = args.iter().flatten().copied().collect();
        all.extend(kwargs.iter().flat_map(|(_, l)| l.iter().copied()));
        all.extend(recv.iter().copied());
        let target = cx.targets[ci as usize].clone().unwrap_or(Target::Unknown("unresolved".into()));
        let here = cx.fr;
        let step = |what: String| Step { at_fn: here, row: c.row, what };
        let mut ret = Labels::new();
        match target {
            Target::Repo(fns, opaque) => {
                if opaque > 0 {
                    ret.extend(all.iter().copied());
                }
                for g in fns {
                    self.callers.entry(g).or_default().insert(cx.fr);
                    let gd = self.df(g);
                    let s = self.summaries[g.0 as usize][g.1 as usize].clone();
                    let subst = |l: Label| -> Labels {
                        match l {
                            Label::Param(j) => {
                                if c.splat {
                                    return all.clone();
                                }
                                let mut o = args.get(j as usize).cloned().unwrap_or_default();
                                if let Some(p) = gd.params.get(j as usize) {
                                    for (k, v) in &kwargs {
                                        if p.name.split(',').any(|n| n == k) {
                                            o.extend(v.iter().copied());
                                        }
                                    }
                                }
                                o
                            }
                            other => [other].into_iter().collect(),
                        }
                    };
                    let callee = self.fn_label(g);
                    for &l in &s.ret {
                        for x in subst(l) {
                            if matches!(x, Label::Src(_) | Label::Unk(_)) && !matches!(l, Label::Param(_)) {
                                self.arrive.entry((cx.fr, x)).or_insert((g, c.row));
                            }
                            ret.insert(x);
                        }
                    }
                    for ((site, l), path) in &s.sinks {
                        for x in subst(*l) {
                            let mut p = vec![step(format!("calls {callee}"))];
                            p.extend(path.iter().cloned());
                            self.record_sink(cx, *site, x, p, false);
                        }
                    }
                    for ((site, l), path) in &s.possible {
                        for x in subst(*l) {
                            let mut p = vec![step(format!("calls {callee}"))];
                            p.extend(path.iter().cloned());
                            self.record_sink(cx, *site, x, p, true);
                        }
                    }
                    for ((u, l), path) in &s.escapes {
                        for x in subst(*l) {
                            let mut p = vec![step(format!("calls {callee}"))];
                            p.extend(path.iter().cloned());
                            self.record_escape(cx, *u, x, p);
                        }
                    }
                    for (st, l) in &s.writes {
                        for x in subst(*l) {
                            self.write_state(cx, *st, x, c.row);
                        }
                    }
                    if self.counting {
                        self.facts[cx.fr.0 as usize][cx.fr.1 as usize].calls_repo.insert(g);
                    }
                }
                if self.counting {
                    self.coverage.repo += 1;
                }
            }
            Target::External(..) | Target::ExternalTypeLast(..) => {
                let (models, via, prec) = match &target {
                    Target::External(q, prec) => (self.inp.models.lookup(lang, q), q.clone(), *prec),
                    Target::ExternalTypeLast(t, m) if !m.is_empty() => {
                        (self.inp.models.lookup_type_method(lang, t, m), format!("{t}.{m}"), Precision::TypeName)
                    }
                    Target::ExternalTypeLast(t, _) => (Vec::new(), t.clone(), Precision::TypeName),
                    _ => unreachable!(),
                };
                let models: Vec<Kind> = models.into_iter().map(|m| m.kind.clone()).collect();
                let sanitizer = models.iter().any(|k| matches!(k, Kind::Sanitizer));
                if !sanitizer {
                    ret.extend(all.iter().copied());
                }
                let mut modeled = false;
                for k in &models {
                    match k {
                        Kind::Source(class) => {
                            modeled = true;
                            let l = self.src(cx.fr, c.at, c.row, class, &via);
                            ret.insert(l);
                            if self.counting {
                                self.facts[cx.fr.0 as usize][cx.fr.1 as usize].reads.insert(class.clone());
                            }
                        }
                        Kind::Sink(class, which) => {
                            modeled = true;
                            let site = self.sink_site(cx.fr, c.at, c.row, class, &via, prec);
                            let hit: Labels = match which {
                                None => all.clone(),
                                Some(_) if c.splat => all.clone(),
                                Some(ix) => ix.iter().flat_map(|&i| args.get(i as usize).cloned().unwrap_or_default()).collect(),
                            };
                            for l in hit {
                                self.record_sink(cx, site, l, vec![step(format!("sink {class} via {via}"))], false);
                            }
                            if self.counting {
                                self.facts[cx.fr.0 as usize][cx.fr.1 as usize].writes.insert(class.clone());
                            }
                        }
                        Kind::Dynamic => {
                            modeled = true;
                            let u = self.unk(cx.fr, c.at, c.row, &format!("dynamic: {via}"));
                            ret.insert(u);
                            if let Label::Unk(uid) = u {
                                for &l in &all {
                                    self.record_escape(cx, uid, l, vec![step(format!("passed to {via}"))]);
                                }
                            }
                            if self.counting {
                                self.facts[cx.fr.0 as usize][cx.fr.1 as usize].dynamic.push((c.row, via.clone()));
                                self.coverage.dynamic_markers += 1;
                            }
                        }
                        Kind::Sanitizer | Kind::Returns(_) | Kind::Callback(..) => modeled = true,
                        Kind::ParamSource(_) => {}
                    }
                }
                if self.counting {
                    if modeled {
                        self.coverage.external_modeled += 1;
                    } else {
                        self.coverage.external_unmodeled += 1;
                    }
                }
            }
            Target::Unknown(why) => {
                let text = match &c.callee {
                    Callee::Path { chain, .. } => chain.clone(),
                    Callee::Method { name, .. } => format!(".{name}"),
                    Callee::Dynamic => "<computed>".into(),
                };
                let u = self.unk(cx.fr, c.at, c.row, &why);
                ret.extend(all.iter().copied());
                ret.insert(u);
                if let Label::Unk(uid) = u {
                    for &l in &all {
                        self.record_escape(cx, uid, l, vec![step(format!("passed to unresolved {text}()"))]);
                    }
                }
                // name-only hint: an untyped receiver's method named like a sink
                let method = text.rsplit(['.', ':']).next().unwrap_or(&text).to_string();
                if text.contains('.') || text.contains("::") {
                    let hints: Vec<String> = self
                        .inp
                        .models
                        .sink_methods(lang, &method)
                        .into_iter()
                        .filter_map(|m| match &m.kind {
                            // only injection-shaped sinks: `.Error()`/`.write()` on an
                            // untyped receiver says too little to be worth a hint
                            Kind::Sink(class, _) if HINT_CLASSES.contains(&class.as_str()) => Some(class.clone()),
                            _ => None,
                        })
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    for class in hints {
                        let site = self.sink_site(cx.fr, c.at, c.row, &class, &format!("?.{method}"), Precision::NameOnly);
                        for &l in &all {
                            self.record_sink(cx, site, l, vec![step(format!("possible {class} sink: {text}() on an untyped receiver"))], true);
                        }
                    }
                }
                if self.counting {
                    self.coverage.unknown += 1;
                    *self.coverage.unknown_by_reason.entry(reason_key(&why)).or_default() += 1;
                    self.facts[cx.fr.0 as usize][cx.fr.1 as usize].unknown_calls.push((c.row, why.clone()));
                }
            }
        }
        if self.counting {
            self.coverage.call_sites += 1;
            if self.answer(cx.fr, c.at).is_some() {
                self.coverage.pipeline_answered += 1;
            }
        }
        ret
    }

    /// Facts pass: reads of every value, writes of every place.
    fn collect_facts(&mut self, cx: &mut Cx) {
        let d = self.df(cx.fr);
        let file = self.file(cx.fr);
        let fr = cx.fr;
        for s in &d.stmts {
            match s {
                Stmt::Assign { to, from, .. } => {
                    let _ = self.labels(cx, from, true);
                    for p in to {
                        let w = match p {
                            Place::Attr { base, chain, .. } if d.self_name.as_deref() == Some(base.as_str()) => {
                                d.owner.as_ref().and_then(|o| field_of(chain).map(|f| format!("field:{o}.{f}")))
                            }
                            Place::Attr { base, .. } | Place::Name(base) | Place::Local(base)
                                if !cx.locals.contains(base) && (d.is_module || file.globals.iter().any(|g| g == base)) =>
                            {
                                Some(format!("global:{}:{}", file.path, base))
                            }
                            _ => None,
                        };
                        if let Some(w) = w {
                            self.facts[fr.0 as usize][fr.1 as usize].writes.insert(w);
                        }
                    }
                }
                Stmt::Eval(v) | Stmt::Return(v) => {
                    let _ = self.labels(cx, v, true);
                }
            }
        }
        for c in &d.calls {
            for a in c.args.iter().chain(c.kwargs.iter().map(|(_, v)| v)) {
                let _ = self.labels(cx, a, true);
            }
        }
        for dy in &d.dynamic {
            self.facts[fr.0 as usize][fr.1 as usize].dynamic.push((dy.row, dy.what.clone()));
            self.coverage.dynamic_markers += 1;
        }
    }

    /// State labels reaching sinks: every source ever written into that
    /// state (transitively through other state) reaches them.
    fn resolve_state(&mut self) {
        let closure = |eng: &Engine, r: u32| -> Vec<(Label, FnRef, u32, u32)> {
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            let mut stack = vec![r];
            while let Some(s) = stack.pop() {
                if !seen.insert(s) {
                    continue;
                }
                for (l, &(w, row)) in eng.state_taint.get(&s).into_iter().flatten() {
                    match l {
                        Label::State(s2) => stack.push(*s2),
                        _ => out.push((*l, w, row, s)),
                    }
                }
            }
            out
        };
        let sinks: Vec<((u32, u32), Vec<Step>, bool)> = self
            .state_sinks
            .iter()
            .map(|(k, v)| (*k, v.clone(), false))
            .chain(self.state_possible.iter().map(|(k, v)| (*k, v.clone(), true)))
            .collect();
        for ((r, site), path, possible) in sinks {
            for (l, writer, row, via_state) in closure(self, r) {
                let mut full = self.up_path(writer, l);
                full.push(Step { at_fn: writer, row, what: format!("writes {}", self.states[via_state as usize].show()) });
                full.extend(path.iter().cloned());
                match l {
                    Label::Src(o) => {
                        let map = if possible { &mut self.possible } else { &mut self.flows };
                        map.entry((o, site)).or_insert(Flow { src: o, sink: site, path: full, through_state: Some(r) });
                    }
                    Label::Unk(u) if !possible => {
                        self.unknown_flows.entry((u, site)).or_insert(full);
                    }
                    _ => {}
                }
            }
        }
        let escapes: Vec<((u32, u32), Vec<Step>)> = self.state_escapes.iter().map(|(k, v)| (*k, v.clone())).collect();
        for ((r, unk), path) in escapes {
            for (l, writer, row, via_state) in closure(self, r) {
                if let Label::Src(o) = l {
                    let mut full = self.up_path(writer, l);
                    full.push(Step { at_fn: writer, row, what: format!("writes {}", self.states[via_state as usize].show()) });
                    full.extend(path.iter().cloned());
                    self.escapes.entry((o, unk)).or_insert(Escape { src: o, unk, path: full });
                }
            }
        }
    }
}

/// `self.a.b` -> `a`.
fn field_of(chain: &str) -> Option<String> {
    let mut it = chain.split('.');
    it.next()?;
    it.next().map(|s| s.split('[').next().unwrap_or(s).to_string()).filter(|s| !s.is_empty())
}

/// `.a.b.c` -> (`a`, `.b.c`).
fn split_field(rest: &str) -> Option<(String, String)> {
    let r = rest.strip_prefix('.')?;
    match r.split_once('.') {
        Some((f, t)) => Some((f.to_string(), format!(".{t}"))),
        None => Some((r.to_string(), String::new())),
    }
}

/// An import's path joined with the rest of the chain.
fn qualified(path: &str, rest: &str, base: &str, lang: Lang) -> String {
    let bang = base.ends_with('!') && !rest.contains('!');
    let rest = match lang {
        Lang::Rust => rest.to_string(),
        _ => rest.to_string(),
    };
    format!("{path}{rest}{}", if bang { "!" } else { "" })
}

/// A name that is neither local, imported nor module state.
fn builtin(lang: Lang, chain: &str) -> String {
    match lang {
        Lang::Python => format!("builtins.{chain}"),
        _ => chain.to_string(),
    }
}

fn reason_key(why: &str) -> String {
    why.split(':').next().unwrap_or(why).trim().to_string()
}
