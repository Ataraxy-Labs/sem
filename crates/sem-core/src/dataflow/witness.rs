//! Witness tasks: what a runner needs to *demonstrate* a static flow.
//!
//! A static flow is a may-fact (see [`super::PRECISION`]). A witness is a
//! concrete execution that carries a fresh canary from the declared source
//! site to the declared sink site. This module only describes each flow as
//! a task with exact byte spans; it executes nothing:
//!
//! - **source contract**: where the runner injects the canary. Either a
//!   parameter (`kind: "param"`, injected at the start of the function body
//!   that binds it) or an expression (`kind: "expr"`, the source call/read
//!   wrapped so its value becomes the canary).
//! - **sink contract**: the sink call's argument spans; the runner wraps
//!   each argument in an observer.
//! - **path functions**: every function on the witness path, with its body
//!   insertion point and parameter names, so the runner can probe that the
//!   canary really passed through them.
//!
//! A task whose spans cannot be found is listed under `skipped` with the
//! reason: it can never become CONFIRMED.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::{json, Value};
use tree_sitter::Node;

use super::engine::{FnRef, Step};
use super::ir::Lang;
use super::Analysis;

const FN_KINDS: &[&str] = &[
    // python
    "function_definition",
    "lambda",
    // ts / js
    "function_declaration",
    "function_expression",
    "function",
    "arrow_function",
    "method_definition",
    "generator_function_declaration",
    "generator_function",
];

const CALL_KINDS: &[&str] = &["call", "call_expression", "new_expression"];

struct Src {
    text: String,
    tree: tree_sitter::Tree,
}

fn kids<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}

fn span(n: Node) -> Value {
    json!([n.start_byte(), n.end_byte()])
}

fn walk<'t>(n: Node<'t>, f: &mut dyn FnMut(Node<'t>)) {
    f(n);
    for c in kids(n) {
        walk(c, f);
    }
}

/// Binder identifiers of a parameter / pattern node (defaults and type
/// annotations skipped).
fn binders(n: Node, src: &str, out: &mut Vec<String>) {
    match n.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            let t = n.utf8_text(src.as_bytes()).unwrap_or("");
            if t != "this" && t != "self" && t != "cls" {
                out.push(t.to_string());
            }
        }
        "type_annotation" | "type" | "comment" | "decorator" => {}
        // `a = 1` (python default_parameter / ts assignment_pattern): only the name
        "default_parameter" | "typed_default_parameter" => {
            if let Some(x) = n.child_by_field_name("name") {
                binders(x, src, out);
            }
        }
        "assignment_pattern" => {
            if let Some(x) = n.child_by_field_name("left") {
                binders(x, src, out);
            }
        }
        // `{ a: b }`: the binder is the value
        "pair_pattern" => {
            if let Some(x) = n.child_by_field_name("value") {
                binders(x, src, out);
            }
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(x) = n.child_by_field_name("pattern") {
                binders(x, src, out);
            }
        }
        "typed_parameter" => {
            if let Some(x) = kids(n).into_iter().next() {
                binders(x, src, out);
            }
        }
        _ => {
            for c in kids(n) {
                binders(c, src, out);
            }
        }
    }
}

fn params_of(f: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = f.child_by_field_name("parameters") {
        binders(p, src, &mut out);
    } else if let Some(p) = f.child_by_field_name("parameter") {
        // `x => ..`
        binders(p, src, &mut out);
    }
    out
}

/// The function's name as written: its `name` field, or the variable /
/// property it is assigned to.
fn fn_name(f: Node, src: &str) -> Option<String> {
    let t = |n: Node| n.utf8_text(src.as_bytes()).ok().map(str::to_string);
    if let Some(n) = f.child_by_field_name("name") {
        return t(n);
    }
    let p = f.parent()?;
    match p.kind() {
        "variable_declarator" => p.child_by_field_name("name").and_then(t),
        "pair" => p.child_by_field_name("key").and_then(t),
        "assignment_expression" => p.child_by_field_name("left").and_then(t).map(|s| s.rsplit('.').next().unwrap_or("").to_string()),
        _ => None,
    }
}

/// Where a runner inserts code at the start of a function's body.
fn body_info(f: Node, _src: &str) -> Value {
    let Some(b) = f.child_by_field_name("body") else { return Value::Null };
    match b.kind() {
        // python: the first statement (the runner inserts a line before it
        // at its column, or `x = ..; ` inline when it shares the def line)
        "block" => {
            let first = kids(b).into_iter().find(|c| c.kind() != "comment");
            match first {
                Some(s) => json!({
                    "kind": "py-block",
                    "at": s.start_byte(),
                    "col": s.start_position().column,
                    "sameLine": s.start_position().row == f.start_position().row,
                    "lineStart": s.start_byte() - s.start_position().column,
                }),
                None => Value::Null,
            }
        }
        "statement_block" => json!({ "kind": "ts-block", "at": b.start_byte() + 1 }),
        // `x => expr`: the runner rewrites to `x => (probe, expr)`
        _ => json!({ "kind": "ts-expr", "span": span(b) }),
    }
}

fn fn_nodes<'t>(root: Node<'t>) -> Vec<Node<'t>> {
    let mut v = Vec::new();
    walk(root, &mut |n| {
        if FN_KINDS.contains(&n.kind()) {
            v.push(n);
        }
    });
    v
}

/// The syntax node of a function starting on `row` (0-based), preferring
/// one named `name` and, if `binds` is set, one with that parameter.
fn find_fn<'t>(root: Node<'t>, src: &str, row: u32, name: Option<&str>, binds: Option<&str>) -> Option<Node<'t>> {
    let mut best: Option<(i32, Node)> = None;
    for f in fn_nodes(root) {
        let r = f.start_position().row as u32;
        // a decorated python def: the IR row is the `def`; a TS arrow
        // assigned to a const may start a little after its declarator row;
        // a framework procedure's handler (tRPC `.mutation(async ({input})
        // => ..)`) starts after the builder chain that names the source
        let window = if binds.is_some() { 60 } else { 3 };
        if r < row || r > row + window {
            continue;
        }
        let mut score = -((r - row) as i32) * 4;
        if let (Some(n), Some(m)) = (name, fn_name(f, src)) {
            if n == m {
                score += 40;
            }
        }
        if let Some(p) = binds {
            if params_of(f, src).iter().any(|x| x == p) {
                score += 400;
            } else {
                continue;
            }
        }
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, f));
        }
    }
    best.map(|(_, f)| f)
}

/// The smallest call node whose callee contains byte `at`.
fn find_call(root: Node, at: usize) -> Option<Node> {
    let mut best: Option<Node> = None;
    walk(root, &mut |n| {
        if !CALL_KINDS.contains(&n.kind()) {
            return;
        }
        let callee = n.child_by_field_name("function").or_else(|| n.child_by_field_name("constructor"));
        if let Some(c) = callee {
            if c.start_byte() <= at && at < c.end_byte() && best.is_none_or(|b| n.end_byte() - n.start_byte() < b.end_byte() - b.start_byte()) {
                best = Some(n);
            }
        }
    });
    best
}

/// The argument spans of a call: positional arguments, and the value of
/// each keyword argument (splats are skipped: `splat` is set).
fn call_args(call: Node, src: &str) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    let mut splat = false;
    let Some(al) = call.child_by_field_name("arguments") else { return (out, splat) };
    let mut pos = 0;
    for a in kids(al) {
        match a.kind() {
            "comment" => {}
            "keyword_argument" => {
                if let Some(v) = a.child_by_field_name("value") {
                    let name = a.child_by_field_name("name").and_then(|n| n.utf8_text(src.as_bytes()).ok()).unwrap_or("");
                    out.push(json!({ "keyword": name, "span": span(v) }));
                }
            }
            "list_splat" | "dictionary_splat" | "spread_element" => splat = true,
            _ => {
                out.push(json!({ "index": pos, "span": span(a) }));
                pos += 1;
            }
        }
    }
    (out, splat)
}

/// The expression a value source at byte `at` stands for: the read or
/// call there, grown while it is the object of an access, the callee of a
/// call, or the value of a subscript (`os.environ.get("X")`,
/// `process.argv[2]`, `request.args["q"]`).
fn source_expr(root: Node, at: usize) -> Option<Node> {
    let mut n = root.named_descendant_for_byte_range(at, at + 1)?;
    loop {
        let Some(p) = n.parent() else { break };
        let is_object = |field: &str| p.child_by_field_name(field).is_some_and(|c| c.id() == n.id());
        let grow = match p.kind() {
            "attribute" | "member_expression" => is_object("object") || p.child_by_field_name("attribute").or(p.child_by_field_name("property")).is_some_and(|c| c.id() == n.id()),
            "call" | "call_expression" => is_object("function"),
            "subscript" => is_object("value"),
            "subscript_expression" => is_object("object"),
            "await" | "await_expression" | "non_null_expression" => true,
            _ => false,
        };
        if !grow {
            break;
        }
        n = p;
    }
    Some(n)
}

/// The parameter a parameter-source names: "parameter `x` of a ..",
/// "parameter `x: T`", "callback parameter `req`", "tRPC procedure `input`".
fn param_of_via(via: &str) -> Option<String> {
    let i = via.find('`')?;
    let rest = &via[i + 1..];
    let j = rest.find('`')?;
    let name = rest[..j].split(':').next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn is_param_source(via: &str) -> bool {
    via.contains("parameter `") || via.starts_with("tRPC procedure") || via.contains("procedure `")
}

impl Analysis {
    /// Witness tasks for every non-self flow (see the module doc).
    pub fn witness_json(&self, root: &Path) -> Value {
        let mut cache: HashMap<u32, Option<Src>> = HashMap::new();
        let files = &self.files;
        let load = |cache: &mut HashMap<u32, Option<Src>>, fi: u32| -> bool {
            cache
                .entry(fi)
                .or_insert_with(|| {
                    let p = &files[fi as usize].path;
                    let text = std::fs::read_to_string(root.join(p)).ok()?;
                    let tree = super::lower::parse(p, &text)?;
                    Some(Src { text, tree })
                })
                .is_some()
        };
        let o = &self.out;
        let mut tasks = Vec::new();
        let mut skipped = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for f in &o.flows {
            let s = &o.srcs[f.src as usize];
            let k = &o.sinks[f.sink as usize];
            let key = self.flow_key(f.src, f.sink);
            let lang = Lang::for_path(&self.files[k.at_fn.0 as usize].path);
            let skip = |why: &str| json!({ "key": key, "why": why });
            if s.at_fn == k.at_fn && s.at == k.at {
                skipped.push(skip("self path: the call's own result reaching its own argument"));
                continue;
            }
            if !matches!(lang, Some(Lang::Python | Lang::Ts)) {
                skipped.push(skip("language not supported by the witness runner (python, ts/js only)"));
                continue;
            }
            // one task per (source site, sink site): several flow keys can
            // name the same pair through different paths
            if !seen.insert((s.at_fn, s.at, k.at_fn, k.at)) {
                continue;
            }
            for st in f.path.iter() {
                load(&mut cache, st.at_fn.0);
            }
            if !load(&mut cache, s.at_fn.0) || !load(&mut cache, k.at_fn.0) {
                skipped.push(skip("source or sink file does not parse"));
                continue;
            }
            // sink contract
            let ks = cache[&k.at_fn.0].as_ref().unwrap();
            let Some(call) = find_call(ks.tree.root_node(), k.at as usize) else {
                skipped.push(skip("sink call not found at its byte offset"));
                continue;
            };
            let (args, splat) = call_args(call, &ks.text);
            if args.is_empty() {
                skipped.push(skip("sink call has no observable argument"));
                continue;
            }
            // source contract
            let ss = cache[&s.at_fn.0].as_ref().unwrap();
            let sfile = &self.files[s.at_fn.0 as usize];
            let source = if is_param_source(&s.via) {
                let Some(p) = param_of_via(&s.via) else {
                    skipped.push(skip("parameter source without a parameter name"));
                    continue;
                };
                let d = &sfile.fns[s.at_fn.1 as usize];
                let name = (!d.is_module).then_some(d.name.as_str());
                let Some(fnode) = find_fn(ss.tree.root_node(), &ss.text, s.row, name, Some(&p)) else {
                    skipped.push(skip("function binding the source parameter not found"));
                    continue;
                };
                let body = body_info(fnode, &ss.text);
                if body.is_null() {
                    skipped.push(skip("source function has no body"));
                    continue;
                }
                json!({ "kind": "param", "param": p, "fnSpan": span(fnode), "fnLine": fnode.start_position().row + 1, "body": body })
            } else {
                let Some(e) = source_expr(ss.tree.root_node(), s.at as usize) else {
                    skipped.push(skip("source expression not found at its byte offset"));
                    continue;
                };
                json!({ "kind": "expr", "span": span(e), "text": e.utf8_text(ss.text.as_bytes()).unwrap_or("").chars().take(200).collect::<String>() })
            };
            // path functions, in path order, without the module pseudo-function
            let mut fns: BTreeMap<FnRef, usize> = BTreeMap::new();
            let mut order = Vec::new();
            for st in f.path.iter() {
                let Step { at_fn, .. } = st;
                if self.files[at_fn.0 as usize].fns[at_fn.1 as usize].is_module {
                    continue;
                }
                if !fns.contains_key(at_fn) {
                    fns.insert(*at_fn, order.len());
                    order.push(*at_fn);
                }
            }
            let mut path_fns = Vec::new();
            for fr in order {
                let file = &self.files[fr.0 as usize];
                let d = &file.fns[fr.1 as usize];
                let node = if cache.get(&fr.0).is_some_and(|x| x.is_some()) {
                    let x = cache[&fr.0].as_ref().unwrap();
                    find_fn(x.tree.root_node(), &x.text, d.row, Some(&d.name), None).map(|n| (body_info(n, &x.text), params_of(n, &x.text), n.start_position().row + 1))
                } else {
                    None
                };
                let (body, params, line) = node.unwrap_or((Value::Null, Vec::new(), d.row as usize + 1));
                path_fns.push(json!({
                    "file": file.path,
                    "name": d.name,
                    "owner": d.owner,
                    "line": line,
                    "key": self.fn_key(fr),
                    "params": params,
                    "body": body,
                    "role": if fr == s.at_fn { "source" } else if fr == k.at_fn { "sink" } else { "hop" },
                }));
            }
            let mut j = self.flow_json(f);
            let id = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (s.at_fn, s.at, k.at_fn, k.at, &key).hash(&mut h);
                format!("w{:012x}", h.finish() & 0xffff_ffff_ffff)
            };
            j["id"] = json!(id);
            j["lang"] = json!(lang.unwrap().as_str());
            j["source"]["contract"] = source;
            j["source"]["file"] = json!(sfile.path);
            j["sink"]["contract"] = json!({
                "callSpan": span(call),
                "callText": call.utf8_text(ks.text.as_bytes()).unwrap_or("").chars().take(200).collect::<String>(),
                "args": args,
                "splat": splat,
            });
            j["pathFunctions"] = json!(path_fns);
            tasks.push(j);
        }
        json!({
            "witnessVersion": 1,
            "about": "Witness-generation tasks: one per (source site, sink site) static flow. A task is a CLAIM to be demonstrated by execution, not a finding. CONFIRMED needs a passing witness; everything else stays POSSIBLE (static) or UNKNOWN.",
            "precision": super::PRECISION,
            "incomplete": o.incomplete,
            "tasks": tasks,
            "skipped": skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_name_from_via() {
        assert_eq!(param_of_via("parameter `country` of a mcp.server.fastmcp.FastMCP.tool handler").as_deref(), Some("country"));
        assert_eq!(param_of_via("parameter `req: Request`").as_deref(), Some("req"));
        assert_eq!(param_of_via("tRPC procedure `input` (.mutation)").as_deref(), Some("input"));
        assert!(is_param_source("tRPC procedure `input` (.mutation)"));
        assert!(!is_param_source("os.getenv"));
    }

    #[test]
    fn spans_for_python_call_and_param() {
        let src = "import os\n\ndef f(a, b=1):\n    \"\"\"doc\"\"\"\n    return open(os.path.join('x', a), 'w')\n";
        let tree = super::super::lower::parse("m.py", src).unwrap();
        let at = src.find("open").unwrap();
        let call = find_call(tree.root_node(), at).unwrap();
        assert!(call.utf8_text(src.as_bytes()).unwrap().starts_with("open("));
        let (args, _) = call_args(call, src);
        assert_eq!(args.len(), 2);
        let f = find_fn(tree.root_node(), src, 2, Some("f"), Some("a")).unwrap();
        assert_eq!(params_of(f, src), vec!["a", "b"]);
        let b = body_info(f, src);
        assert_eq!(b["kind"], "py-block");
        assert_eq!(b["col"], 4);
        let e = source_expr(tree.root_node(), src.find("os.path").unwrap()).unwrap();
        assert_eq!(e.utf8_text(src.as_bytes()).unwrap(), "os.path.join('x', a)");
    }

    #[test]
    fn spans_for_ts_destructured_closure_param() {
        let src = "const r = router({\n  start: p.input(z).mutation(async ({ input }) => {\n    await go(input.id);\n  }),\n});\nconst g = (x) => fetch(x, { method: 'HEAD' });\n";
        let tree = super::super::lower::parse("m.ts", src).unwrap();
        let f = find_fn(tree.root_node(), src, 1, None, Some("input")).unwrap();
        assert_eq!(f.kind(), "arrow_function");
        assert_eq!(body_info(f, src)["kind"], "ts-block");
        let g = find_fn(tree.root_node(), src, 5, Some("g"), None).unwrap();
        assert_eq!(body_info(g, src)["kind"], "ts-expr");
        let call = find_call(tree.root_node(), src.find("fetch").unwrap()).unwrap();
        assert_eq!(call_args(call, src).0.len(), 2);
        let e = source_expr(tree.root_node(), src.find("go(").unwrap()).unwrap();
        // an awaited source call is wrapped with its `await`: the value is the result
        assert_eq!(e.utf8_text(src.as_bytes()).unwrap(), "await go(input.id)");
    }
}
