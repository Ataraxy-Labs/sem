//! Entries of container literals as sub-entities (opt-in, `sem diff --entries`).
//!
//! A registry written as one literal, such as
//!
//! ```python
//! class ClickHouseParser(parser.Parser):
//!     FUNCTIONS = {**parser.Parser.FUNCTIONS, "TOUNIXTIMESTAMP": _build_to_unix, ...}
//! ```
//!
//! is a single entity to the default extractor, so adding one key reads as
//! "ClickHouseParser modified". With entries on, each element of the literal
//! becomes an `entry` entity under the binding that holds it, named
//! `FUNCTIONS["TOUNIXTIMESTAMP"]`, with the element's text as its content. The
//! diff then reports the entry that was added, modified or removed, and the
//! binding and class above it are suppressed when the entries explain them.
//!
//! What counts as a container literal:
//! - Python: a dict display bound by a module- or class-level assignment
//!   (annotated or not), also when the display is wrapped in a filtering
//!   comprehension (`{k: v for k, v in {...}.items() if ...}`) or in
//!   `dict(...)` / `MappingProxyType(...)`; the wrapper stays the binding's own
//!   text, so a changed filter reports the binding. `**spread` elements are entries too (key kind
//!   `spread`), so a changed spread is reported rather than hidden. Python
//!   module- and class-level assignments have no entity of their own by
//!   default; in this mode each one gets a `variable` entity, so a changed
//!   class flag (`FLAG = True` -> `False`) is reported by name as well.
//! - JavaScript/TypeScript: an object literal bound by a top-level
//!   `const`/`let`/`var` (also under `export`, `as const`, `satisfies`) or a
//!   class field. Members that already are entities (methods,
//!   function-valued properties) are left to those entities.
//! - Go: a composite literal bound by a top-level `var`. Map literals are
//!   keyed by their keys; struct literals by field name; slices and arrays of
//!   struct literals by the element's name field (`Name:`, `name:`, `ID:`,
//!   `Id:`, `Key:`, `key:` holding a string), else by position (`#3`).
//!
//! Values are opaque: a nested literal inside a value is part of that entry's
//! content, not entries of its own.

use std::collections::{BTreeMap, HashMap};

use tree_sitter::{Node, Tree};

use super::entity_extractor::compute_structural_hash_and_kappa;
use super::languages::LanguageConfig;
use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::utils::hash::content_hash;

/// Metadata keys set on every `entry` entity.
pub const META_KEY: &str = "entry.key";
pub const META_KEY_KIND: &str = "entry.keyKind";
pub const META_CONTAINER: &str = "entry.container";

/// Go element fields that name an element of a slice of structs.
const GO_NAME_FIELDS: &[&str] = &["Name", "name", "ID", "Id", "Key", "key"];

struct Binding<'t> {
    /// The whole statement/declarator the binding is written as.
    span: Node<'t>,
    /// Start of the declaration around it (`export const`, `var (`): a default
    /// entity starting here or later is the binding's own entity.
    stmt_start: usize,
    name: String,
    value: Option<Node<'t>>,
    /// Create a `variable` entity for the binding (Python only: bindings
    /// there have none by default).
    make_entity: bool,
}

struct Element<'t> {
    node: Node<'t>,
    key: String,
    kind: &'static str,
    display: String,
}

/// Append binding and entry entities for every container literal in `tree`.
/// `entities` must be the default extraction of the same source; entries hang
/// under those entities (or under a `variable` entity created here).
pub(crate) fn append_entry_entities(
    tree: &Tree,
    file_path: &str,
    config: &LanguageConfig,
    source: &str,
    entities: &mut Vec<SemanticEntity>,
) {
    let src = source.as_bytes();
    let mut bindings = Vec::new();
    match config.id {
        "python" => python_bindings(tree.root_node(), src, &mut bindings),
        "javascript" | "typescript" | "tsx" => js_bindings(tree.root_node(), src, &mut bindings),
        "go" => go_bindings(tree.root_node(), src, &mut bindings),
        _ => return,
    }
    if bindings.is_empty() {
        return;
    }

    // Spans of the default entities, to find each binding's owner.
    let spans: Vec<(usize, usize, usize)> = entities
        .iter()
        .enumerate()
        .filter_map(|(i, e)| Some((e.start_byte?, e.end_byte?, i)))
        .collect();
    let mut names_by_id: HashMap<String, (String, Option<String>)> = entities
        .iter()
        .map(|e| (e.id.clone(), (e.name.clone(), e.parent_id.clone())))
        .collect();

    let mut out: Vec<SemanticEntity> = Vec::new();
    for b in bindings {
        let (bs, be) = (b.span.start_byte(), b.span.end_byte());
        let stmt_start = b.stmt_start.min(bs);
        let inner = b.value.unwrap_or(b.span);
        // Smallest default entity containing the bound value.
        let owner = spans
            .iter()
            .filter(|(s, e, _)| *s <= inner.start_byte() && *e >= inner.end_byte())
            .min_by_key(|(s, e, _)| e - s)
            .map(|(s, _, i)| (*s, *i));
        let binding_id = match owner {
            // The owner starts inside the binding statement: it is the
            // binding's own entity (Go `var`, JS `const`, a TS class field).
            Some((s, i)) if s >= stmt_start => {
                if b.make_entity {
                    // An entity already sits inside a Python assignment;
                    // leave that statement to it.
                    continue;
                }
                entities[i].id.clone()
            }
            other => {
                if !b.make_entity {
                    continue;
                }
                let parent = other.map(|(_, i)| entities[i].id.clone());
                let content = node_text(b.span, src).to_string();
                let (sh, kappa) = compute_structural_hash_and_kappa(b.span, src);
                let id = build_entity_id(file_path, "variable", &b.name, parent.as_deref());
                names_by_id.insert(id.clone(), (b.name.clone(), parent.clone()));
                out.push(SemanticEntity {
                    id: id.clone(),
                    file_path: file_path.to_string(),
                    entity_type: "variable".to_string(),
                    name: b.name.clone(),
                    parent_id: parent.clone(),
                    content_hash: content_hash(&content),
                    structural_hash: Some(sh),
                    kappa: Some(kappa),
                    content,
                    start_line: b.span.start_position().row + 1,
                    end_line: b.span.end_position().row + 1,
                    start_byte: Some(bs),
                    end_byte: Some(be),
                    metadata: None,
                });
                id
            }
        };
        let Some(value) = b.value else { continue };
        let elements = match config.id {
            "python" => python_elements(value, src),
            "go" => go_elements(value, src),
            _ => js_elements(value, src),
        };
        if elements.is_empty() {
            continue;
        }
        // `A.B.NAME`: the binding's name under its enclosing entities (a Go
        // `var (...)` group is one entity for several names).
        let enclosing = names_by_id
            .get(&binding_id)
            .and_then(|(_, p)| p.clone())
            .map(|p| qualified_name(&p, &names_by_id))
            .unwrap_or_default();
        let container = if enclosing.is_empty() {
            b.name.clone()
        } else {
            format!("{enclosing}.{}", b.name)
        };
        for el in elements {
            let (s, e) = (el.node.start_byte(), el.node.end_byte());
            // A member that already is an entity (a JS method) stays that entity.
            if spans.iter().any(|(es, ee, _)| *es >= s && *ee <= e) {
                continue;
            }
            let name = format!("{}[{}]", b.name, el.display);
            let content = node_text(el.node, src).to_string();
            let (sh, kappa) = compute_structural_hash_and_kappa(el.node, src);
            let mut meta = BTreeMap::new();
            meta.insert(META_KEY.to_string(), el.key);
            meta.insert(META_KEY_KIND.to_string(), el.kind.to_string());
            meta.insert(META_CONTAINER.to_string(), container.clone());
            out.push(SemanticEntity {
                id: build_entity_id(file_path, "entry", &name, Some(&binding_id)),
                file_path: file_path.to_string(),
                entity_type: "entry".to_string(),
                name,
                parent_id: Some(binding_id.clone()),
                content_hash: content_hash(&content),
                structural_hash: Some(sh),
                kappa: Some(kappa),
                content,
                start_line: el.node.start_position().row + 1,
                end_line: el.node.end_position().row + 1,
                start_byte: Some(s),
                end_byte: Some(e),
                metadata: Some(meta),
            });
        }
    }
    entities.extend(out);
}

/// `A.B.NAME` from the parent chain of an entity id.
fn qualified_name(id: &str, names: &HashMap<String, (String, Option<String>)>) -> String {
    let mut parts = Vec::new();
    let mut cur = Some(id.to_string());
    let mut guard = 0;
    while let Some(c) = cur {
        guard += 1;
        if guard > 64 {
            break;
        }
        match names.get(&c) {
            Some((n, p)) => {
                if !n.is_empty() {
                    parts.push(n.clone());
                }
                cur = p.clone();
            }
            None => break,
        }
    }
    parts.reverse();
    parts.join(".")
}

fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut c = node.walk();
    node.named_children(&mut c).collect()
}

// ── Python ──────────────────────────────────────────────────────────────────

fn python_bindings<'t>(root: Node<'t>, src: &[u8], out: &mut Vec<Binding<'t>>) {
    fn walk<'t>(block: Node<'t>, src: &[u8], out: &mut Vec<Binding<'t>>) {
        for child in named_children(block) {
            match child.kind() {
                "expression_statement" => {
                    let kids = named_children(child);
                    let [assign] = kids.as_slice() else { continue };
                    if assign.kind() != "assignment" {
                        continue;
                    }
                    let Some(left) = assign.child_by_field_name("left") else { continue };
                    if left.kind() != "identifier" {
                        continue;
                    }
                    let value = assign.child_by_field_name("right").map(unwrap_python_value);
                    if value.is_none() {
                        continue; // a bare annotation `x: int` binds nothing
                    }
                    out.push(Binding {
                        span: child,
                        stmt_start: child.start_byte(),
                        name: node_text(left, src).to_string(),
                        value: value.and_then(|v| python_dict_literal(v, src)),
                        make_entity: true,
                    });
                }
                "class_definition" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        walk(body, src, out);
                    }
                }
                "decorated_definition" => {
                    if let Some(def) = child.child_by_field_name("definition") {
                        if def.kind() == "class_definition" {
                            if let Some(body) = def.child_by_field_name("body") {
                                walk(body, src, out);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    walk(root, src, out);
}

/// The dict display a binding's value is built from: the value itself, the
/// literal a comprehension filters (`{k: v for k, v in {...}.items() if ...}`),
/// or the one argument of `dict({...})` / `MappingProxyType({...})` /
/// `frozendict({...})` / `OrderedDict({...})`. The wrapper is part of the
/// binding's own text, so a changed filter still shows as the binding changing.
fn python_dict_literal<'t>(value: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
    match value.kind() {
        "dictionary" => Some(value),
        "dictionary_comprehension" => {
            let clauses: Vec<Node> = named_children(value)
                .into_iter()
                .filter(|c| c.kind() == "for_in_clause")
                .collect();
            let [clause] = clauses.as_slice() else { return None };
            let call = unwrap_python_value(clause.child_by_field_name("right")?);
            if call.kind() != "call" {
                return None;
            }
            let func = call.child_by_field_name("function")?;
            if func.kind() != "attribute" || node_text(func.child_by_field_name("attribute")?, src) != "items" {
                return None;
            }
            let obj = unwrap_python_value(func.child_by_field_name("object")?);
            (obj.kind() == "dictionary").then_some(obj)
        }
        "call" => {
            let func = value.child_by_field_name("function")?;
            let fname = node_text(func, src);
            let last = fname.rsplit('.').next().unwrap_or(fname);
            if !matches!(last, "dict" | "MappingProxyType" | "frozendict" | "OrderedDict") {
                return None;
            }
            let args = named_children(value.child_by_field_name("arguments")?);
            match args.as_slice() {
                [only] => {
                    let only = unwrap_python_value(*only);
                    (only.kind() == "dictionary").then_some(only)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn unwrap_python_value(mut node: Node) -> Node {
    while node.kind() == "parenthesized_expression" {
        match named_children(node).as_slice() {
            [inner] => node = *inner,
            _ => break,
        }
    }
    node
}

fn python_elements<'t>(dict: Node<'t>, src: &[u8]) -> Vec<Element<'t>> {
    let mut out = Vec::new();
    for child in named_children(dict) {
        match child.kind() {
            "pair" => {
                let Some(key) = child.child_by_field_name("key") else { continue };
                let (k, kind, display) = python_key(key, src);
                out.push(Element { node: child, key: k, kind, display });
            }
            "dictionary_splat" => {
                let inner = named_children(child)
                    .into_iter()
                    .next()
                    .map(|n| squash(node_text(n, src)))
                    .unwrap_or_default();
                out.push(Element {
                    node: child,
                    display: format!("**{inner}"),
                    key: inner,
                    kind: "spread",
                });
            }
            _ => {}
        }
    }
    out
}

fn python_key(key: Node, src: &[u8]) -> (String, &'static str, String) {
    match key.kind() {
        "string" => {
            if let Some(s) = plain_python_string(key, src) {
                let display = quote(&s);
                return (s, "string", display);
            }
            let t = squash(node_text(key, src));
            (t.clone(), "expr", t)
        }
        "attribute" => {
            let t = squash(node_text(key, src));
            (t.clone(), "attribute", t)
        }
        "identifier" => {
            let t = node_text(key, src).to_string();
            (t.clone(), "identifier", t)
        }
        "integer" | "float" => {
            let t = node_text(key, src).to_string();
            (t.clone(), "number", t)
        }
        "true" | "false" | "none" => {
            let t = node_text(key, src).to_string();
            (t.clone(), "constant", t)
        }
        _ => {
            let t = squash(node_text(key, src));
            (t.clone(), "expr", t)
        }
    }
}

/// The text of a Python string literal without prefix or interpolation, or
/// `None` when it has either (its value is then not the source text).
fn plain_python_string(node: Node, src: &[u8]) -> Option<String> {
    let mut content = String::new();
    for child in named_children(node) {
        match child.kind() {
            "string_start" => {
                let t = node_text(child, src);
                if !(t.starts_with('"') || t.starts_with('\'')) {
                    return None;
                }
            }
            "string_content" => content.push_str(node_text(child, src)),
            "string_end" => {}
            _ => return None,
        }
    }
    Some(content)
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ── JavaScript / TypeScript ─────────────────────────────────────────────────

fn js_bindings<'t>(root: Node<'t>, src: &[u8], out: &mut Vec<Binding<'t>>) {
    fn declaration<'t>(node: Node<'t>, stmt: usize, src: &[u8], out: &mut Vec<Binding<'t>>) {
        match node.kind() {
            "export_statement" => {
                if let Some(d) = node.child_by_field_name("declaration") {
                    declaration(d, stmt, src, out);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                for d in named_children(node) {
                    if d.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = d.child_by_field_name("name") else { continue };
                    if name.kind() != "identifier" {
                        continue;
                    }
                    let value = d.child_by_field_name("value").map(unwrap_js_value);
                    out.push(Binding {
                        span: d,
                        stmt_start: stmt,
                        name: node_text(name, src).to_string(),
                        value: value.filter(|v| v.kind() == "object"),
                        make_entity: false,
                    });
                }
            }
            "class_declaration" | "abstract_class_declaration" | "class" => {
                let Some(body) = node.child_by_field_name("body") else { return };
                for m in named_children(body) {
                    if !matches!(m.kind(), "public_field_definition" | "field_definition") {
                        continue;
                    }
                    let name = m
                        .child_by_field_name("name")
                        .or_else(|| m.child_by_field_name("property"));
                    let Some(name) = name else { continue };
                    let value = m.child_by_field_name("value").map(unwrap_js_value);
                    out.push(Binding {
                        span: m,
                        stmt_start: m.start_byte(),
                        name: node_text(name, src).to_string(),
                        value: value.filter(|v| v.kind() == "object"),
                        make_entity: false,
                    });
                }
            }
            _ => {}
        }
    }
    for child in named_children(root) {
        declaration(child, child.start_byte(), src, out);
    }
}

fn unwrap_js_value(mut node: Node) -> Node {
    loop {
        match node.kind() {
            "as_expression" | "satisfies_expression" | "non_null_expression"
            | "parenthesized_expression" | "type_assertion" => {
                let kids = named_children(node);
                // `x as T`: the expression is the first named child.
                match kids.first() {
                    Some(first) if first.kind() != node.kind() => node = *first,
                    _ => return node,
                }
            }
            _ => return node,
        }
    }
}

fn js_elements<'t>(object: Node<'t>, src: &[u8]) -> Vec<Element<'t>> {
    let mut out = Vec::new();
    for child in named_children(object) {
        match child.kind() {
            "pair" => {
                let Some(key) = child.child_by_field_name("key") else { continue };
                let (k, kind, display) = match key.kind() {
                    "property_identifier" | "private_property_identifier" => {
                        let t = node_text(key, src).to_string();
                        (t.clone(), "identifier", t)
                    }
                    "string" => {
                        let t = node_text(key, src);
                        let s = t
                            .get(1..t.len().saturating_sub(1))
                            .unwrap_or("")
                            .to_string();
                        let d = quote(&s);
                        (s, "string", d)
                    }
                    "number" => {
                        let t = node_text(key, src).to_string();
                        (t.clone(), "number", t)
                    }
                    "computed_property_name" => {
                        let inner = named_children(key)
                            .into_iter()
                            .next()
                            .map(|n| squash(node_text(n, src)))
                            .unwrap_or_default();
                        let kind = match named_children(key).first().map(|n| n.kind()) {
                            Some("string") => "string",
                            Some("member_expression") => "attribute",
                            Some("identifier") => "identifier",
                            _ => "computed",
                        };
                        (inner.clone(), kind, inner)
                    }
                    _ => {
                        let t = squash(node_text(key, src));
                        (t.clone(), "expr", t)
                    }
                };
                out.push(Element { node: child, key: k, kind, display });
            }
            "shorthand_property_identifier" => {
                let t = node_text(child, src).to_string();
                out.push(Element { node: child, key: t.clone(), kind: "identifier", display: t });
            }
            "spread_element" => {
                let inner = named_children(child)
                    .into_iter()
                    .next()
                    .map(|n| squash(node_text(n, src)))
                    .unwrap_or_default();
                out.push(Element {
                    node: child,
                    display: format!("...{inner}"),
                    key: inner,
                    kind: "spread",
                });
            }
            "method_definition" => {
                let Some(name) = child.child_by_field_name("name") else { continue };
                let t = squash(node_text(name, src));
                out.push(Element { node: child, key: t.clone(), kind: "identifier", display: t });
            }
            _ => {}
        }
    }
    out
}

// ── Go ──────────────────────────────────────────────────────────────────────

fn go_bindings<'t>(root: Node<'t>, src: &[u8], out: &mut Vec<Binding<'t>>) {
    fn spec<'t>(s: Node<'t>, stmt: usize, src: &[u8], out: &mut Vec<Binding<'t>>) {
        let Some(name) = s.child_by_field_name("name") else { return };
        let value = s.child_by_field_name("value").and_then(|v| {
            // `var a, b = x, y` binds positionally; take the literal for the
            // first name only when there is exactly one value.
            match named_children(v).as_slice() {
                [only] => go_composite(*only),
                _ => None,
            }
        });
        out.push(Binding {
            span: s,
            stmt_start: stmt,
            name: node_text(name, src).to_string(),
            value,
            make_entity: false,
        });
    }
    for decl in named_children(root) {
        if !matches!(decl.kind(), "var_declaration" | "const_declaration") {
            continue;
        }
        for child in named_children(decl) {
            match child.kind() {
                "var_spec" | "const_spec" => spec(child, decl.start_byte(), src, out),
                "var_spec_list" | "const_spec_list" => {
                    for s in named_children(child) {
                        if matches!(s.kind(), "var_spec" | "const_spec") {
                            spec(s, decl.start_byte(), src, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The composite literal an expression is, through `&T{...}` and `(...)`.
fn go_composite(node: Node) -> Option<Node> {
    match node.kind() {
        "composite_literal" => Some(node),
        "unary_expression" | "parenthesized_expression" => node
            .child_by_field_name("operand")
            .or_else(|| named_children(node).into_iter().next())
            .and_then(go_composite),
        _ => None,
    }
}

fn go_elements<'t>(lit: Node<'t>, src: &[u8]) -> Vec<Element<'t>> {
    let Some(body) = lit.child_by_field_name("body") else { return Vec::new() };
    let ty = lit.child_by_field_name("type").map(|t| t.kind()).unwrap_or("");
    let mut out = Vec::new();
    let mut index = 0usize;
    for child in named_children(body) {
        match child.kind() {
            "keyed_element" => {
                let Some(key) = child.child_by_field_name("key") else { continue };
                let key_inner = named_children(key).into_iter().next().unwrap_or(key);
                let (k, kind, display) = match key_inner.kind() {
                    "interpreted_string_literal" | "raw_string_literal" => {
                        let s = go_string(key_inner, src);
                        let d = quote(&s);
                        (s, "string", d)
                    }
                    "identifier" | "field_identifier" if ty != "map_type" => {
                        let t = node_text(key_inner, src).to_string();
                        (t.clone(), "field", t)
                    }
                    "identifier" => {
                        let t = node_text(key_inner, src).to_string();
                        (t.clone(), "identifier", t)
                    }
                    "selector_expression" | "qualified_type" => {
                        let t = squash(node_text(key_inner, src));
                        (t.clone(), "attribute", t)
                    }
                    "int_literal" | "float_literal" => {
                        let t = node_text(key_inner, src).to_string();
                        (t.clone(), "number", t)
                    }
                    _ => {
                        let t = squash(node_text(key_inner, src));
                        (t.clone(), "expr", t)
                    }
                };
                out.push(Element { node: child, key: k, kind, display });
            }
            "literal_element" => {
                let i = index;
                index += 1;
                let inner = named_children(child).into_iter().next();
                let name = inner.and_then(|n| go_element_name(n, src));
                match name {
                    Some(n) => {
                        let d = quote(&n);
                        out.push(Element { node: child, key: n, kind: "name_field", display: d });
                    }
                    None => out.push(Element {
                        node: child,
                        key: i.to_string(),
                        kind: "index",
                        display: format!("#{i}"),
                    }),
                }
            }
            _ => {}
        }
    }
    out
}

/// The string in an element's name field: `{Name: "abs", ...}`, `&T{Name: "abs"}`.
fn go_element_name(node: Node, src: &[u8]) -> Option<String> {
    let body = match node.kind() {
        "literal_value" => node,
        _ => go_composite(node)?.child_by_field_name("body")?,
    };
    for field in GO_NAME_FIELDS {
        for kv in named_children(body) {
            if kv.kind() != "keyed_element" {
                continue;
            }
            let key = kv.child_by_field_name("key")?;
            let key_inner = named_children(key).into_iter().next().unwrap_or(key);
            if node_text(key_inner, src) != *field {
                continue;
            }
            let val = kv.child_by_field_name("value")?;
            let val_inner = named_children(val).into_iter().next().unwrap_or(val);
            if matches!(
                val_inner.kind(),
                "interpreted_string_literal" | "raw_string_literal"
            ) {
                return Some(go_string(val_inner, src));
            }
        }
    }
    None
}

fn go_string(node: Node, src: &[u8]) -> String {
    let t = node_text(node, src);
    t.get(1..t.len().saturating_sub(1)).unwrap_or("").to_string()
}
