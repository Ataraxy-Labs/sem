//! Library and framework models, as data.
//!
//! A model file is JSON: `{ "language": "python"|"ts"|"go"|"rust",
//! "models": [ { "match": <name or [names]>, "kind": <kind>, ... } ] }`.
//! Names are qualified the way the engine qualifies a call or read (see
//! each built-in file's `doc`); a trailing `.*` (`::*` for Rust) matches any
//! continuation. Kinds:
//!
//! - `source` `{class}`: the call's result (or the read value) carries data
//!   of `class`.
//! - `sink` `{class, args?}`: data reaching argument `args` (all when
//!   omitted, and the receiver of a method) is a flow into `class`.
//! - `returns` `{type}`: the call returns a value of the qualified `type`
//!   (so later method calls on it can match models).
//! - `param-source` `{class}`: a function parameter declared with this
//!   type carries `class` (framework request objects).
//! - `callback` `{arg: "each"|"last"|<index>, params: [{index, source?,
//!   type?}]}`: closures passed at `arg` get these parameter facts
//!   (`app.get(path, (req, res) => ..)`).
//! - `handler` `{class, params?, arg?, except_types?}`: a function
//!   registered with this name receives `class` data in its parameters
//!   (all, or the indices in `params`): one *decorated* with it
//!   (`@mcp.tool()`, `@app.route(..)`), or, with `arg`, one passed by name
//!   at argument `arg` of a call to it (`path("x/", views.show)`). A
//!   parameter declared with a type in `except_types` is not input (an
//!   injected framework context), nor is the first parameter of a
//!   definition also decorated with a name in `injects_first`
//!   (`@click.pass_context`), nor one whose default is a call with
//!   arguments to a name in `except_defaults` (`db = Depends(get_db)`). With `methods` (names, a trailing `*` a
//!   prefix) the matched name is a framework base class instead: the
//!   methods so named of a repo class deriving from it (directly or through
//!   repo classes) are the handlers (Home Assistant `async_step_*` flow
//!   steps).
//! - `invoke` `{arg}`: the call runs the function passed at argument
//!   `arg` with the arguments after it and every keyword argument
//!   (`asyncio.to_thread(f, x, y=z)` calls `f(x, y=z)`). A repo function
//!   named there is analyzed as called; the result still carries the
//!   arguments' data as any unmodeled call's does.
//! - `dynamic`: the call defeats static tracking (reflection, `eval`): an
//!   explicit unknown.
//! - `sanitizer`: the result carries no data of its arguments (`int(x)`).
//!
//! Anything not modeled that is outside the repo *propagates*: its result
//! carries whatever its arguments and receiver carry. That is the sound
//! default; models only add sources and sinks, or remove taint
//! (`sanitizer`).
//!
//! The format is small on purpose so that a model can be drafted by an
//! agent and checked mechanically: [`validate`] checks shape, classes and
//! duplicates; a later check can confirm every `match` names a real symbol
//! in the library's source.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::ir::Lang;

/// `tool-input`: arguments of a tool call from an agent / MCP client.
pub const SOURCE_CLASSES: &[&str] = &["http-input", "tool-input", "env", "file-read", "db-read", "net-input", "cli-input"];
/// `file-path`: data naming a file to open, read, serve or delete (path
/// traversal); `file-write`: data written into a file.
pub const SINK_CLASSES: &[&str] = &["exec", "db", "net-send", "log", "template", "file-write", "file-path", "http-response"];

#[derive(Clone, Debug, PartialEq)]
pub enum CbArg {
    Each,
    Last,
    Index(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CbParam {
    pub index: u32,
    pub source: Option<String>,
    pub ty: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Handler {
    pub class: String,
    pub params: Option<Vec<u32>>,
    pub arg: Option<u32>,
    pub except_types: Vec<String>,
    /// Decorators that inject the definition's first parameter.
    pub injects_first: Vec<String>,
    /// Method names (trailing `*` = prefix): the model names a base class.
    pub methods: Vec<String>,
    /// Calls whose result, as a parameter default, is injected.
    pub except_defaults: Vec<String>,
}

impl Handler {
    /// Is `name` one of this base-class handler's methods?
    pub fn names_method(&self, name: &str) -> bool {
        self.methods.iter().any(|m| match m.strip_suffix('*') {
            Some(p) => name.starts_with(p),
            None => m == name,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Source(String),
    Handler(Handler),
    Sink(String, Option<Vec<u32>>),
    Returns(String),
    ParamSource(String),
    Callback(CbArg, Vec<CbParam>),
    Invoke(u32),
    Dynamic,
    Sanitizer,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub pattern: String,
    pub kind: Kind,
    /// Where it came from (`builtin:python`, a file path).
    pub origin: String,
}

#[derive(Default)]
struct Index {
    models: Vec<Model>,
    exact: HashMap<String, Vec<usize>>,
    prefix: Vec<(String, usize)>,
    /// `(type's last segment, method)` -> models of that method, for
    /// receivers whose type the call graph knows only by its last segment.
    by_type_method: HashMap<(String, String), Vec<usize>>,
    /// Method names of sink models (for name-only hints on untyped receivers).
    sink_methods: HashMap<String, Vec<usize>>,
}

pub struct Models {
    langs: HashMap<Lang, Index>,
}

const BUILTIN: &[(&str, &str)] = &[
    ("builtin:python", include_str!("models/python.json")),
    ("builtin:ts", include_str!("models/ts.json")),
    ("builtin:go", include_str!("models/go.json")),
    ("builtin:rust", include_str!("models/rust.json")),
];

fn lang_of(s: &str) -> Option<Lang> {
    Some(match s {
        "python" => Lang::Python,
        "ts" | "js" | "typescript" | "javascript" => Lang::Ts,
        "go" => Lang::Go,
        "rust" => Lang::Rust,
        _ => return None,
    })
}

/// Shape errors of one model file (empty = valid).
pub fn validate(v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let Some(lang) = v["language"].as_str() else {
        return vec!["missing `language`".into()];
    };
    if lang_of(lang).is_none() {
        errs.push(format!("unknown language `{lang}`"));
    }
    let Some(ms) = v["models"].as_array() else {
        errs.push("missing `models` array".into());
        return errs;
    };
    let mut seen: HashSet<String> = HashSet::new();
    for (i, m) in ms.iter().enumerate() {
        let names: Vec<&str> = match &m["match"] {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if names.is_empty() || names.iter().any(|n| n.trim().is_empty()) {
            errs.push(format!("models[{i}]: `match` must be a non-empty name or list of names"));
        }
        let kind = m["kind"].as_str().unwrap_or("");
        let class = m["class"].as_str();
        match kind {
            "source" | "param-source" => match class {
                Some(c) if SOURCE_CLASSES.contains(&c) => {}
                c => errs.push(format!("models[{i}]: source class {c:?} is not one of {SOURCE_CLASSES:?}")),
            },
            "sink" => {
                match class {
                    Some(c) if SINK_CLASSES.contains(&c) => {}
                    c => errs.push(format!("models[{i}]: sink class {c:?} is not one of {SINK_CLASSES:?}")),
                }
                if let Some(a) = m.get("args") {
                    if !a.as_array().is_some_and(|a| a.iter().all(|x| x.as_u64().is_some())) {
                        errs.push(format!("models[{i}]: `args` must be a list of argument indices"));
                    }
                }
            }
            "handler" => {
                if !class.is_some_and(|c| SOURCE_CLASSES.contains(&c)) {
                    errs.push(format!("models[{i}]: handler class {class:?} is not one of {SOURCE_CLASSES:?}"));
                }
                if let Some(a) = m.get("params") {
                    if !a.as_array().is_some_and(|a| a.iter().all(|x| x.as_u64().is_some())) {
                        errs.push(format!("models[{i}]: handler `params` must be a list of parameter indices"));
                    }
                }
                for key in ["except_types", "injects_first", "methods", "except_defaults"] {
                    if m.get(key).is_some_and(|a| !a.as_array().is_some_and(|a| a.iter().all(Value::is_string))) {
                        errs.push(format!("models[{i}]: handler `{key}` must be a list of names"));
                    }
                }
                if m.get("methods").is_some() && m.get("arg").is_some() {
                    errs.push(format!("models[{i}]: handler `methods` and `arg` exclude each other"));
                }
                if m.get("arg").is_some_and(|a| a.as_u64().is_none()) {
                    errs.push(format!("models[{i}]: handler `arg` must be an argument index"));
                }
            }
            "returns" => {
                if m["type"].as_str().is_none_or(str::is_empty) {
                    errs.push(format!("models[{i}]: `returns` needs a `type`"));
                }
            }
            "callback" => {
                let ok_arg = matches!(m["arg"].as_str(), Some("each" | "last")) || m["arg"].as_u64().is_some();
                if !ok_arg {
                    errs.push(format!("models[{i}]: callback `arg` must be \"each\", \"last\" or an index"));
                }
                match m["params"].as_array() {
                    Some(ps) if !ps.is_empty() => {
                        for p in ps {
                            if p["index"].as_u64().is_none() {
                                errs.push(format!("models[{i}]: callback param needs an `index`"));
                            }
                            if let Some(s) = p["source"].as_str() {
                                if !SOURCE_CLASSES.contains(&s) {
                                    errs.push(format!("models[{i}]: callback source class `{s}` unknown"));
                                }
                            }
                        }
                    }
                    _ => errs.push(format!("models[{i}]: callback needs non-empty `params`")),
                }
            }
            "invoke" => {
                if m["arg"].as_u64().is_none() {
                    errs.push(format!("models[{i}]: invoke `arg` must be an argument index"));
                }
            }
            "dynamic" | "sanitizer" => {}
            k => errs.push(format!("models[{i}]: unknown kind `{k}`")),
        }
        for n in names {
            let key = format!("{n}\u{0}{kind}\u{0}{}", class.unwrap_or(""));
            if !seen.insert(key) {
                errs.push(format!("models[{i}]: duplicate model `{n}` ({kind} {})", class.unwrap_or("")));
            }
        }
    }
    errs
}

fn parse_kind(m: &Value) -> Option<Kind> {
    let class = || m["class"].as_str().map(str::to_string);
    Some(match m["kind"].as_str()? {
        "source" => Kind::Source(class()?),
        "param-source" => Kind::ParamSource(class()?),
        "handler" => Kind::Handler(Handler {
            class: class()?,
            params: m["params"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as u32)).collect()),
            arg: m["arg"].as_u64().map(|x| x as u32),
            except_types: m["except_types"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            except_defaults: m["except_defaults"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            methods: m["methods"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            injects_first: m["injects_first"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        }),
        "sink" => Kind::Sink(
            class()?,
            m["args"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as u32)).collect()),
        ),
        "returns" => Kind::Returns(m["type"].as_str()?.to_string()),
        "callback" => Kind::Callback(
            match (m["arg"].as_str(), m["arg"].as_u64()) {
                (Some("each"), _) => CbArg::Each,
                (Some("last"), _) => CbArg::Last,
                (_, Some(i)) => CbArg::Index(i as u32),
                _ => return None,
            },
            m["params"]
                .as_array()?
                .iter()
                .filter_map(|p| {
                    Some(CbParam {
                        index: p["index"].as_u64()? as u32,
                        source: p["source"].as_str().map(str::to_string),
                        ty: p["type"].as_str().map(str::to_string),
                    })
                })
                .collect(),
        ),
        "invoke" => Kind::Invoke(m["arg"].as_u64()? as u32),
        "dynamic" => Kind::Dynamic,
        "sanitizer" => Kind::Sanitizer,
        _ => return None,
    })
}

fn split_type_method(lang: Lang, q: &str) -> Option<(String, String)> {
    let sep = lang.sep();
    let (ty, method) = q.rsplit_once(sep)?;
    let last = ty.rsplit(sep).next()?.rsplit('/').next()?;
    // `a.b.Type.method`: only when the segment before the method is a type
    // (capitalized) — module functions are not methods.
    if !last.chars().next().is_some_and(char::is_uppercase) && lang != Lang::Python {
        return None;
    }
    Some((last.to_string(), method.to_string()))
}

impl Models {
    /// The built-in models only.
    pub fn builtin() -> Models {
        let mut m = Models { langs: HashMap::new() };
        for (origin, text) in BUILTIN {
            let v: Value = serde_json::from_str(text).expect("built-in model JSON");
            let errs = validate(&v);
            assert!(errs.is_empty(), "{origin}: {errs:?}");
            m.add(&v, origin);
        }
        m
    }

    /// Add one validated model file.
    pub fn add(&mut self, v: &Value, origin: &str) {
        let Some(lang) = v["language"].as_str().and_then(lang_of) else { return };
        let idx = self.langs.entry(lang).or_default();
        for m in v["models"].as_array().into_iter().flatten() {
            let Some(kind) = parse_kind(m) else { continue };
            let names: Vec<String> = match &m["match"] {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            for n in names {
                let i = idx.models.len();
                idx.models.push(Model { pattern: n.clone(), kind: kind.clone(), origin: origin.to_string() });
                let star = format!("{}*", lang.sep());
                match n.strip_suffix(&star) {
                    Some(p) => idx.prefix.push((format!("{p}{}", lang.sep()), i)),
                    None => {
                        idx.exact.entry(n.clone()).or_default().push(i);
                        if let Some(k) = split_type_method(lang, &n) {
                            if matches!(kind, Kind::Sink(..) | Kind::Source(_) | Kind::Returns(_) | Kind::Callback(..) | Kind::Sanitizer | Kind::Dynamic | Kind::Invoke(_)) {
                                idx.by_type_method.entry(k.clone()).or_default().push(i);
                            }
                            if matches!(kind, Kind::Sink(..)) {
                                idx.sink_methods.entry(k.1).or_default().push(i);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Models whose name is `q` (or a prefix pattern of it).
    pub fn lookup(&self, lang: Lang, q: &str) -> Vec<&Model> {
        let Some(idx) = self.langs.get(&lang) else { return Vec::new() };
        let mut out: Vec<&Model> = idx.exact.get(q).into_iter().flatten().map(|&i| &idx.models[i]).collect();
        for (p, i) in &idx.prefix {
            if q.starts_with(p.as_str()) {
                out.push(&idx.models[*i]);
            }
        }
        out
    }

    /// Models of method `method` on a type known only by its last path
    /// segment (the call graph's view of an external receiver type).
    pub fn lookup_type_method(&self, lang: Lang, ty_last: &str, method: &str) -> Vec<&Model> {
        let Some(idx) = self.langs.get(&lang) else { return Vec::new() };
        idx.by_type_method
            .get(&(ty_last.to_string(), method.to_string()))
            .into_iter()
            .flatten()
            .map(|&i| &idx.models[i])
            .collect()
    }

    /// Sink models whose method name is `method` (name-only hints).
    pub fn sink_methods(&self, lang: Lang, method: &str) -> Vec<&Model> {
        let Some(idx) = self.langs.get(&lang) else { return Vec::new() };
        idx.sink_methods.get(method).into_iter().flatten().map(|&i| &idx.models[i]).collect()
    }

    /// Every `handler` model of a language.
    pub fn handlers(&self, lang: Lang) -> Vec<&Model> {
        let Some(idx) = self.langs.get(&lang) else { return Vec::new() };
        idx.models.iter().filter(|m| matches!(m.kind, Kind::Handler(_))).collect()
    }

    pub fn count(&self) -> usize {
        self.langs.values().map(|i| i.models.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_models_validate_and_index() {
        let m = Models::builtin();
        assert!(m.count() > 100);
        let hits = m.lookup(Lang::Python, "os.environ.get");
        assert!(hits.iter().any(|h| h.kind == Kind::Source("env".into())), "{hits:?}");
        let hits = m.lookup(Lang::Go, "os/exec.Command");
        assert!(hits.iter().any(|h| matches!(&h.kind, Kind::Sink(c, _) if c == "exec")));
        assert!(!m.lookup_type_method(Lang::Go, "DB", "Query").is_empty());
    }

    #[test]
    fn validate_rejects_bad_models() {
        let v: Value = serde_json::json!({ "language": "python", "models": [
            { "match": "a.b", "kind": "sink", "class": "nope" },
            { "match": "a.c", "kind": "returns" },
            { "match": "a.d", "kind": "source", "class": "env" },
            { "match": "a.d", "kind": "source", "class": "env" },
            { "match": [], "kind": "dynamic" },
            { "match": "a.e", "kind": "bogus" }
        ]});
        let errs = validate(&v);
        assert!(errs.iter().any(|e| e.contains("sink class")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("needs a `type`")));
        assert!(errs.iter().any(|e| e.contains("duplicate")));
        assert!(errs.iter().any(|e| e.contains("non-empty")));
        assert!(errs.iter().any(|e| e.contains("unknown kind")));
    }
}
