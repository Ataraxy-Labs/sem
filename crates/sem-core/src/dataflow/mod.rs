//! Data flow: who reads and writes what, and whether data from a source
//! reaches a sink.
//!
//! One engine over a small shared IR, like the call graph:
//!
//! 1. **lower** ([`lower`]): per file, syntax tree -> [`ir::DfFile`]: per
//!    function its assignments, calls, returns and field/attribute
//!    accesses, each with the names its value is computed from.
//! 2. **answer** (the call-graph pipeline, [`crate::parser::calls::site_answers`]):
//!    every call and name site of a Python / Go / Rust file resolved to repo
//!    definitions, a module value, an external, or unknown-with-a-reason.
//!    JS/TS has no such pipeline: its calls resolve through imports and
//!    same-file names only, everything else is unknown.
//! 3. **qualify + models** ([`models`]): an external call or read is named
//!    through the file's imports (`subprocess.run`, `os/exec.Command`) and
//!    matched against declarative models of sources, sinks and types.
//! 4. **propagate** ([`engine`]): labels to a fixpoint with per-function
//!    summaries; source labels reaching sink arguments become flows with
//!    witness paths. Data reaching a call with no known target is reported
//!    as an escape into unknown code, never dropped.

pub mod engine;
pub mod ir;
pub mod lower;
pub mod models;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;

#[cfg(feature = "parallel")]
use rayon::prelude::*;
use serde_json::{json, Value};

use crate::model::entity::SemanticEntity;
use crate::parser::calls::{self, ir::FileFacts, SiteAnswer};
use engine::{Engine, FnRef, Input, Output, Step};
use ir::{DfFile, Lang};
use models::Models;

pub const PRECISION: &str = "may-flow: flow-insensitive within a function, field-insensitive (a write to any field taints the object), one summary per function (context-insensitive), no alias analysis; sources and sinks only where a model names them. Over-approximates within what was resolved; unresolved calls are reported as unknown, not dropped";

/// How much of an [`Analysis`] to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonDetail {
    /// Everything: witness paths of escapes and unknown-origin flows, and
    /// per-function transitive facts.
    Full,
    /// Without those (they grow with sources x unresolved calls and with
    /// functions x reachable functions); flows keep their paths.
    Compact,
}

/// Everything the engine computed, with enough context to render it.
pub struct Analysis {
    pub files: Vec<DfFile>,
    pub fn_entity: Vec<Vec<Option<String>>>,
    pub out: Output,
}

/// Analyze `file_paths` (repo-relative) under `root`. `entities` are sem's
/// entities for those files; `resolve_spec(from_file, specifier)` maps a
/// JS/TS import specifier to a repo file, if it is one.
pub fn analyze(
    root: &Path,
    file_paths: &[String],
    entities: &[SemanticEntity],
    resolve_spec: &(dyn Fn(&str, &str) -> Option<String> + Sync),
    models: &Models,
) -> Analysis {
    analyze_until(root, file_paths, entities, resolve_spec, models, Limits::default())
}

/// Resource limits of one analysis. Past either, propagation stops and
/// the result is marked `incomplete`: a partial under-approximation of
/// the flows (facts of functions the final pass did not reach are empty).
#[derive(Clone, Copy, Debug, Default)]
pub struct Limits {
    /// Wall-clock deadline.
    pub deadline: Option<std::time::Instant>,
    /// Resident memory of the whole process, in bytes.
    pub max_rss_bytes: Option<usize>,
}

/// [`analyze`] within [`Limits`].
pub fn analyze_until(
    root: &Path,
    file_paths: &[String],
    entities: &[SemanticEntity],
    resolve_spec: &(dyn Fn(&str, &str) -> Option<String> + Sync),
    models: &Models,
    limits: Limits,
) -> Analysis {
    let paths: Vec<&String> = file_paths.iter().filter(|p| Lang::for_path(p).is_some()).collect();
    // --max-memory holds during lowering too (every file's IR is kept for
    // the fixpoint): past it, no further file is lowered and the result is
    // incomplete
    let lowered_count = std::sync::atomic::AtomicUsize::new(0);
    let cut = std::sync::atomic::AtomicBool::new(false);
    let lowered = |p: &&String| -> Option<(DfFile, Option<FileFacts>)> {
        use std::sync::atomic::Ordering::Relaxed;
        if cut.load(Relaxed) {
            return None;
        }
        if let Some(max) = limits.max_rss_bytes {
            if lowered_count.fetch_add(1, Relaxed) % 256 == 255 && crate::parser::mem_profile::current_rss_bytes().is_some_and(|r| r > max) {
                cut.store(true, Relaxed);
                return None;
            }
        }
        let src = std::fs::read_to_string(root.join(p.as_str())).ok()?;
        let lang = Lang::for_path(p)?;
        // bundled / minified JS (`.min.js`, a line over 1000 chars) is a
        // shipped asset, not the repo's code
        if lang == Lang::Ts && (p.ends_with(".min.js") || src.lines().any(|l| l.len() > 1000)) {
            return None;
        }
        let tree = lower::parse(p, &src)?;
        let mut df = lower::lower(lang, p, &tree, &src);
        let facts = calls::lower_file(p, &tree, &src);
        if let Some(f) = &facts {
            // Python / Go / Rust imports: the call-graph lowering's `use`s
            for u in &f.uses {
                if u.glob {
                    continue;
                }
                let Some(name) = &u.name else { continue };
                let path = u.path.0.iter().map(|s| &**s).collect::<Vec<_>>().join(lang.sep());
                df.imports.push(ir::Import { local: name.to_string(), path, spec: None, member: None });
            }
        }
        Some((df, facts))
    };
    #[cfg(feature = "parallel")]
    let per: Vec<(DfFile, Option<FileFacts>)> = paths.par_iter().filter_map(lowered).collect();
    #[cfg(not(feature = "parallel"))]
    let per: Vec<(DfFile, Option<FileFacts>)> = paths.iter().filter_map(lowered).collect();
    let (files, facts): (Vec<DfFile>, Vec<Option<FileFacts>>) = per.into_iter().unzip();
    let index: HashMap<&str, usize> = files.iter().enumerate().map(|(i, f)| (f.path.as_str(), i)).collect();

    // stage 2: the call-graph pipeline's answers, per language
    let mut answers: Vec<HashMap<u32, (bool, SiteAnswer)>> = vec![HashMap::new(); files.len()];
    let mut dispatch: HashMap<String, Vec<String>> = HashMap::new();
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, f) in facts.iter().enumerate() {
        if f.is_some() {
            let ext = files[i].path.rsplit('.').next().unwrap_or("");
            groups.entry(ext).or_default().push(i);
        }
    }
    for idx in groups.values() {
        let Some(lang) = calls::language_for(&files[idx[0]].path) else { continue };
        let group: Vec<(&str, &FileFacts)> = idx.iter().map(|&i| (files[i].path.as_str(), facts[i].as_ref().unwrap())).collect();
        let res = calls::site_answers(root, lang, &group, entities);
        for (k, per_file) in res.into_iter().enumerate() {
            answers[idx[k]] = per_file.into_iter().map(|(at, call, a)| (at, (call, a))).collect();
        }
        for (d, i) in calls::dispatch_answers(root, lang, &group, entities) {
            dispatch.entry(d).or_default().push(i);
        }
    }
    drop(facts);

    // sem entities for functions and types
    let mut by_file: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    for e in entities {
        by_file.entry(e.file_path.as_str()).or_default().push(e);
    }
    let fn_entity: Vec<Vec<Option<String>>> = files
        .iter()
        .map(|f| {
            f.fns
                .iter()
                .map(|d| {
                    if d.is_module {
                        return None;
                    }
                    let row = d.row as usize + 1;
                    by_file
                        .get(f.path.as_str())?
                        .iter()
                        .filter(|e| e.name == d.name && e.start_line <= row && row <= e.end_line)
                        .min_by_key(|e| (!matches!(e.entity_type.as_str(), "function" | "method"), e.end_line - e.start_line))
                        .map(|e| e.id.clone())
                })
                .collect()
        })
        .collect();
    let mut type_entity: HashMap<String, (u32, String)> = HashMap::new();
    for e in entities {
        if matches!(e.entity_type.as_str(), "class" | "struct" | "type" | "interface" | "enum") {
            if let Some(&fi) = index.get(e.file_path.as_str()) {
                type_entity.insert(e.id.clone(), (fi as u32, e.name.clone()));
            }
        }
    }
    let spec_file: Vec<HashMap<String, u32>> = files
        .iter()
        .map(|f| {
            f.imports
                .iter()
                .filter_map(|i| {
                    let spec = i.spec.as_ref()?;
                    let target = resolve_spec(&f.path, spec)?;
                    Some((spec.clone(), *index.get(target.as_str())? as u32))
                })
                .collect()
        })
        .collect();

    let out = Engine::new(Input {
        files: &files,
        answers: &answers,
        fn_entity: &fn_entity,
        type_entity: &type_entity,
        spec_file: &spec_file,
        dispatch: &dispatch,
        models,
        limits,
    })
    .run();
    let mut out = out;
    if cut.into_inner() {
        out.incomplete = true;
        out.incomplete_why = Some("memory budget");
    }
    Analysis { files, fn_entity, out }
}

impl Analysis {
    fn fn_name(&self, fr: FnRef) -> String {
        let d = &self.files[fr.0 as usize].fns[fr.1 as usize];
        match &d.owner {
            Some(o) => format!("{o}.{}", d.name),
            None => d.name.clone(),
        }
    }

    /// `file::Owner.name`: stable across trees (no line numbers).
    pub fn fn_key(&self, fr: FnRef) -> String {
        format!("{}::{}", self.files[fr.0 as usize].path, self.fn_name(fr))
    }

    fn step(&self, s: &Step) -> String {
        format!("{}:{} {}: {}", self.files[s.at_fn.0 as usize].path, s.row + 1, self.fn_name(s.at_fn), s.what)
    }

    fn end(&self, fr: FnRef, row: u32) -> Value {
        json!({
            "file": self.files[fr.0 as usize].path,
            "line": row + 1,
            "entity": self.fn_name(fr),
            "entityId": self.fn_entity[fr.0 as usize][fr.1 as usize],
        })
    }

    /// A flow's identity across trees.
    pub fn flow_key(&self, src: u32, sink: u32) -> String {
        let s = &self.out.srcs[src as usize];
        let k = &self.out.sinks[sink as usize];
        format!("{} {} [{}] -> {} {} [{}]", s.class, self.fn_key(s.at_fn), s.via, k.class, self.fn_key(k.at_fn), k.via)
    }

    fn flow_json(&self, f: &engine::Flow) -> Value {
        let s = &self.out.srcs[f.src as usize];
        let k = &self.out.sinks[f.sink as usize];
        let mut src = self.end(s.at_fn, s.row);
        src["class"] = json!(s.class);
        src["via"] = json!(s.via);
        let mut sink = self.end(k.at_fn, k.row);
        sink["class"] = json!(k.class);
        sink["via"] = json!(k.via);
        sink["precision"] = json!(k.precision);
        json!({
            "key": self.flow_key(f.src, f.sink),
            "source": src,
            "sink": sink,
            "throughState": f.through_state.map(|r| self.out.states[r as usize].show()),
            // the call's own result reaching its own argument (`q = read(q)`
            // seen flow-insensitively): not a path between two sites
            "selfPath": s.at_fn == k.at_fn && s.at == k.at,
            "interprocedural": s.at_fn != k.at_fn,
            "path": f.path.iter().map(|s| self.step(s)).collect::<Vec<_>>(),
        })
    }

    /// Per-function facts, direct and transitive (through resolved calls).
    pub fn entities_json(&self) -> Vec<Value> {
        self.entities_json_with(true)
    }

    /// Per-function facts; the transitive closure (a walk per function)
    /// only with `transitive`.
    pub fn entities_json_with(&self, transitive: bool) -> Vec<Value> {
        let facts = &self.out.facts;
        let mut out = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            for (i, d) in f.fns.iter().enumerate() {
                let fr = (fi as u32, i as u32);
                let x = &facts[fi][i];
                // transitive closure over resolved repo calls
                let mut reads: BTreeSet<String> = BTreeSet::new();
                let mut writes: BTreeSet<String> = BTreeSet::new();
                let mut unknown = 0usize;
                let mut seen = std::collections::HashSet::new();
                let mut q = VecDeque::from([fr]);
                if !transitive {
                    q.clear();
                }
                while let Some(u) = q.pop_front() {
                    if !seen.insert(u) || seen.len() > 5000 {
                        continue;
                    }
                    let y = &facts[u.0 as usize][u.1 as usize];
                    reads.extend(y.reads.iter().filter(|r| !r.starts_with("field:")).cloned());
                    writes.extend(y.writes.iter().filter(|r| !r.starts_with("field:")).cloned());
                    unknown += y.unknown_calls.len() + y.dynamic.len();
                    q.extend(y.calls_repo.iter().copied());
                }
                if d.is_module && x.reads.is_empty() && x.writes.is_empty() && x.unknown_calls.is_empty() {
                    continue;
                }
                out.push(json!({
                    "key": self.fn_key(fr),
                    "entityId": self.fn_entity[fi][i],
                    "name": self.fn_name(fr),
                    "file": f.path,
                    "line": d.row + 1,
                    "module": d.is_module,
                    "cyclomatic": d.cyclomatic,
                    "cognitive": d.cognitive,
                    "reads": x.reads,
                    "writes": x.writes,
                    "unknownCalls": x.unknown_calls.len(),
                    "unknownCallees": x.unknown_callees.iter().map(|(r, n)| json!([r + 1, n])).collect::<Vec<_>>(),
                    "dynamic": x.dynamic.iter().map(|(r, w)| format!("{}: {w}", r + 1)).collect::<Vec<_>>(),
                }));
                if transitive {
                    out.last_mut().unwrap()["transitive"] = json!({ "reads": reads, "writes": writes, "unknownCallsReachable": unknown, "functionsReachable": seen.len() });
                }
            }
        }
        out
    }

    pub fn to_json(&self) -> Value {
        self.to_json_with(JsonDetail::Full)
    }

    /// An escape's identity across trees.
    pub fn escape_key(&self, e: &engine::Escape) -> String {
        let s = &self.out.srcs[e.src as usize];
        let uo = &self.out.unks[e.unk as usize];
        format!("{} {} [{}] -> unknown {}", s.class, self.fn_key(s.at_fn), s.via, self.fn_key(uo.at_fn))
    }

    /// Hashes of every escape's key, to compare two trees without
    /// rendering hundreds of thousands of escapes.
    pub fn escape_keys(&self) -> std::collections::HashSet<u64> {
        use std::hash::{Hash, Hasher};
        self.out
            .escapes
            .iter()
            .map(|e| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                self.escape_key(e).hash(&mut h);
                h.finish()
            })
            .collect()
    }

    /// The escapes whose key hash is not in `known`, rendered with paths.
    pub fn escapes_not_in(&self, known: &std::collections::HashSet<u64>) -> Vec<Value> {
        use std::hash::{Hash, Hasher};
        let mut seen = BTreeSet::new();
        self.out
            .escapes
            .iter()
            .filter(|e| {
                let k = self.escape_key(e);
                let mut h = std::collections::hash_map::DefaultHasher::new();
                k.hash(&mut h);
                !known.contains(&h.finish()) && seen.insert(k)
            })
            .map(|e| self.escape_json(e, true))
            .collect()
    }

    fn escape_json(&self, e: &engine::Escape, path: bool) -> Value {
        let o = &self.out;
        let s = &o.srcs[e.src as usize];
        let uo = &o.unks[e.unk as usize];
        json!({
            "key": self.escape_key(e),
            "source": { "class": s.class, "file": self.files[s.at_fn.0 as usize].path, "line": s.row + 1, "entity": self.fn_name(s.at_fn), "via": s.via },
            "unknown": { "file": self.files[uo.at_fn.0 as usize].path, "line": uo.row + 1, "entity": self.fn_name(uo.at_fn), "why": uo.why },
            "path": if path { o.escape_path(e).iter().map(|s| self.step(s)).collect::<Vec<_>>() } else { Vec::new() },
        })
    }

    pub fn to_json_with(&self, detail: JsonDetail) -> Value {
        let full = detail == JsonDetail::Full;
        let o = &self.out;
        let cov = &o.coverage;
        let rate = if cov.call_sites > 0 { cov.unknown as f64 / cov.call_sites as f64 } else { 0.0 };
        let mut flows: Vec<Value> = o.flows.iter().map(|f| self.flow_json(f)).collect();
        flows.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
        let mut possible: Vec<Value> = o.possible.iter().map(|f| self.flow_json(f)).collect();
        possible.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
        let unknown_flows: Vec<Value> = o
            .unknown_flows
            .iter()
            .map(|(u, s, p)| {
                let uo = &o.unks[*u as usize];
                let k = &o.sinks[*s as usize];
                json!({
                    "key": format!("unknown {} -> {} {} [{}]", self.fn_key(uo.at_fn), k.class, self.fn_key(k.at_fn), k.via),
                    "unknown": { "file": self.files[uo.at_fn.0 as usize].path, "line": uo.row + 1, "entity": self.fn_name(uo.at_fn), "why": uo.why },
                    "sink": { "class": k.class, "file": self.files[k.at_fn.0 as usize].path, "line": k.row + 1, "entity": self.fn_name(k.at_fn), "via": k.via },
                    "path": if full { p.iter().map(|s| self.step(s)).collect::<Vec<_>>() } else { Vec::new() },
                })
            })
            .collect();
        // compact: escapes are only counted (see `escape_keys`); they number
        // sources x unresolved calls
        let escapes: Vec<Value> = if full { o.escapes.iter().map(|e| self.escape_json(e, true)).collect() } else { Vec::new() };
        json!({
            "precision": PRECISION,
            "incomplete": o.incomplete,
            "incompleteWhy": o.incomplete_why,
            "coverage": {
                "files": self.files.len(),
                "functions": self.files.iter().map(|f| f.fns.len()).sum::<usize>(),
                "callSites": cov.call_sites,
                "resolvedToRepo": cov.repo,
                "externalModeled": cov.external_modeled,
                "externalUnmodeled": cov.external_unmodeled,
                "unknown": cov.unknown,
                "unknownRate": (rate * 1000.0).round() / 1000.0,
                "unknownByReason": cov.unknown_by_reason,
                "dynamicMarkers": cov.dynamic_markers,
                "pipelineAnswered": cov.pipeline_answered,
                "sourceSites": o.srcs.len(),
                "sinkSites": o.sinks.iter().filter(|s| s.precision != engine::Precision::NameOnly).count(),
            },
            "flows": flows,
            "unknownFlows": unknown_flows,
            // a state holding values from unresolved calls that reaches a sink
            "unknownThroughState": o.unknown_through_state.iter().map(|(r, s)| {
                let k = &o.sinks[*s as usize];
                json!({ "state": o.states[*r as usize].show(), "sink": { "class": k.class, "file": self.files[k.at_fn.0 as usize].path, "line": k.row + 1, "entity": self.fn_name(k.at_fn), "via": k.via } })
            }).collect::<Vec<_>>(),
            "escapes": escapes,
            "escapeCount": o.escapes.len(),
            "possibleFlows": possible,
            "entities": self.entities_json_with(full),
        })
    }
}
