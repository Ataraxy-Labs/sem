//! Syntax facts the boundary models match against, one tree-sitter walk per
//! file: calls (callee text + literal arguments), decorators/attributes and
//! what they decorate, env-style member reads, string literals, module-level
//! dispatch tables (dict/map/object literals of function names) and classes
//! (for ORM table names).
//!
//! Language differences are node-kind names only; everything downstream is
//! language-neutral.

use serde::Serialize;
use tree_sitter::Node;

/// Source language family, by extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Py,
    Go,
    Rs,
    Ts,
    Js,
}

impl Lang {
    pub fn of(path: &str) -> Option<Lang> {
        let ext = path.rsplit_once('.').map(|(_, e)| e)?;
        Some(match ext {
            "py" => Lang::Py,
            "go" => Lang::Go,
            "rs" => Lang::Rs,
            "ts" | "tsx" | "mts" | "cts" if !path.ends_with(".d.ts") => Lang::Ts,
            "js" | "jsx" | "mjs" | "cjs" => Lang::Js,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::Py => "py",
            Lang::Go => "go",
            Lang::Rs => "rs",
            Lang::Ts => "ts",
            Lang::Js => "js",
        }
    }
}

/// A call argument, as far as syntax tells.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "t", content = "v", rename_all = "lowercase")]
pub enum Arg {
    /// A string literal with no interpolation.
    Str(String),
    /// A string built at runtime; the literal fragments are kept.
    Tmpl(String),
    /// An identifier or dotted path (`handler`, `views.index`).
    Name(String),
    /// Keyword argument `k=v` (Python) / object key (not expanded).
    Kw(String, Box<Arg>),
    /// Anything else, as (truncated) text.
    Expr(String),
}

impl Arg {
    pub fn literal(&self) -> Option<&str> {
        match self {
            Arg::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn text_fragments(&self) -> Option<&str> {
        match self {
            Arg::Str(s) | Arg::Tmpl(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CallFact {
    pub byte: usize,
    pub end: usize,
    /// Byte span of the callee expression.
    pub fn_span: (usize, usize),
    pub line: usize,
    /// Callee text, whitespace removed, argument lists collapsed to `()`.
    pub callee: String,
    pub args: Vec<Arg>,
    /// Set when the call is a decorator/attribute: the decorated item's
    /// name and 1-based line.
    pub decorates: Option<(String, usize)>,
    /// Enclosing class/impl name, if any.
    pub class: Option<String>,
    /// Decorators on the enclosing class (callee text + first literal arg).
    pub class_decorators: Vec<(String, Option<String>)>,
    pub macro_call: bool,
    /// The variable this call's result is assigned to (`X = f()`,
    /// `x := f()`, `const x = f()`), when it is the whole right-hand side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assign: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemberRead {
    pub byte: usize,
    pub line: usize,
    /// e.g. `process.env.PORT`, `os.environ["X"]`
    pub text: String,
    /// The key when literal.
    pub key: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StrLit {
    pub byte: usize,
    pub line: usize,
    pub text: String,
    pub interpolated: bool,
}

/// `NAME = {k: fn, ...}` / `var NAME = map[..]T{"k": fn}` / `const NAME = {k: fn}`.
#[derive(Clone, Debug, Serialize)]
pub struct TableFact {
    pub name: String,
    pub line: usize,
    /// (key text if literal, value name if a plain name)
    pub entries: Vec<(Option<String>, Option<String>)>,
    pub module_level: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClassFact {
    pub name: String,
    pub line: usize,
    pub bases: Vec<String>,
    pub decorators: Vec<(String, Option<String>)>,
    /// Literal class attributes (`__tablename__ = "x"`), and for Go the
    /// literal a `TableName()` method returns.
    pub attrs: Vec<(String, String)>,
    /// Python keyword args in the class header (`table=True`).
    pub header_kw: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FileScan {
    pub calls: Vec<CallFact>,
    pub members: Vec<MemberRead>,
    pub strings: Vec<StrLit>,
    pub tables: Vec<TableFact>,
    pub classes: Vec<ClassFact>,
}

fn ext_of(lang: Lang, path: &str) -> &'static str {
    match lang {
        Lang::Py => ".py",
        Lang::Go => ".go",
        Lang::Rs => ".rs",
        Lang::Ts if path.ends_with(".tsx") => ".tsx",
        Lang::Ts => ".ts",
        Lang::Js => ".js",
    }
}

pub fn parse(lang: Lang, path: &str, src: &str) -> Option<tree_sitter::Tree> {
    let cfg = crate::parser::plugins::code::languages::get_language_config(ext_of(lang, path))?;
    let mut p = tree_sitter::Parser::new();
    p.set_language(&(cfg.get_language)()?).ok()?;
    p.parse(src, None)
}

pub fn scan_file(path: &str, src: &str) -> Option<(Lang, FileScan)> {
    let lang = Lang::of(path)?;
    let tree = parse(lang, path, src)?;
    let mut s = Scanner { lang, src, out: FileScan::default(), class_stack: Vec::new() };
    s.visit(tree.root_node(), 0);
    Some((lang, s.out))
}

struct Scanner<'s> {
    lang: Lang,
    src: &'s str,
    out: FileScan,
    class_stack: Vec<(String, Vec<(String, Option<String>)>)>,
}

const MAX_TEXT: usize = 400;

/// Whitespace removed, truncated; argument lists kept.
fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).take(MAX_TEXT).collect()
}

/// Whitespace removed; argument lists collapsed to `()`. Grouping parens
/// (not after a name) keep their content: `(opts.client ?? client).post`.
fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_TEXT));
    // per open paren: is it an argument list (collapsed)?
    let mut stack: Vec<bool> = Vec::new();
    for c in s.chars() {
        let collapsing = stack.iter().any(|&x| x);
        match c {
            '(' => {
                let args = out
                    .chars()
                    .last()
                    .is_some_and(|p| p.is_alphanumeric() || matches!(p, '_' | ']' | ')' | '>' | '!' | '$'));
                if !collapsing {
                    out.push('(');
                }
                stack.push(args || collapsing);
            }
            ')' => {
                stack.pop();
                if !stack.iter().any(|&x| x) {
                    out.push(')');
                }
            }
            c if c.is_whitespace() => {}
            c if !collapsing => out.push(c),
            _ => {}
        }
        if out.len() >= MAX_TEXT {
            break;
        }
    }
    out
}

impl<'s> Scanner<'s> {
    fn text(&self, n: Node) -> &'s str {
        n.utf8_text(self.src.as_bytes()).unwrap_or("")
    }

    /// `X = call()` / `x := call()` / `const x = call()`: the variable name.
    fn assigned_to(&self, call: Node) -> Option<String> {
        let mut p = call.parent()?;
        if p.kind() == "expression_list" {
            p = p.parent()?; // Go: `x := f()` holds its right side in a list
        }
        let target = match p.kind() {
            "assignment" => p.child_by_field_name("left"),
            "short_var_declaration" | "assignment_statement" => p.child_by_field_name("left"),
            "variable_declarator" => p.child_by_field_name("name"),
            "let_declaration" => p.child_by_field_name("pattern"),
            _ => None,
        }?;
        let t = squash(self.text(target));
        (!t.is_empty() && !t.contains(',')).then_some(t)
    }

    fn visit(&mut self, n: Node, depth: usize) {
        if depth > 400 {
            return;
        }
        let kind = n.kind();
        let mut pushed_class = false;
        match (self.lang, kind) {
            (Lang::Py, "call") | (Lang::Go | Lang::Rs | Lang::Ts | Lang::Js, "call_expression")
                if n.parent().map(|p| p.kind()) != Some("decorator") =>
            {
                self.call(n, None, false)
            }
            (Lang::Ts | Lang::Js, "new_expression") => self.call(n, None, false),
            (Lang::Rs, "macro_invocation") => self.macro_call(n),
            (Lang::Py, "decorated_definition") => self.py_decorated(n),
            (Lang::Rs, "attribute_item") => self.rs_attribute(n),
            (Lang::Ts | Lang::Js, "decorator") => self.ts_decorator(n),
            (Lang::Py, "subscript") => self.py_subscript(n),
            (Lang::Ts | Lang::Js, "member_expression" | "subscript_expression") => self.ts_member(n),
            (Lang::Py, "string") => self.string(n),
            (Lang::Go, "interpreted_string_literal" | "raw_string_literal") => self.string(n),
            (Lang::Rs, "string_literal" | "raw_string_literal") => self.string(n),
            (Lang::Ts | Lang::Js, "string" | "template_string") => self.string(n),
            (Lang::Py, "assignment") => self.table(n, depth),
            (Lang::Go, "var_spec" | "const_spec" | "short_var_declaration") => self.table(n, depth),
            (Lang::Ts | Lang::Js, "variable_declarator") => self.table(n, depth),
            (Lang::Py, "class_definition") => {
                self.py_class(n);
                pushed_class = true;
            }
            (Lang::Ts | Lang::Js, "class_declaration" | "class") => {
                self.ts_class(n);
                pushed_class = true;
            }
            (Lang::Go, "type_spec") => self.go_type(n),
            (Lang::Go, "method_declaration") => self.go_method(n),
            (Lang::Rs, "impl_item") => {
                let name = n.child_by_field_name("type").map(|t| self.text(t).to_string()).unwrap_or_default();
                self.class_stack.push((name, Vec::new()));
                pushed_class = true;
            }
            _ => {}
        }
        let mut c = n.walk();
        let kids: Vec<Node> = n.children(&mut c).collect();
        for k in kids {
            self.visit(k, depth + 1);
        }
        if pushed_class {
            self.class_stack.pop();
        }
    }

    fn arg(&self, n: Node) -> Arg {
        match (self.lang, n.kind()) {
            (Lang::Py, "keyword_argument") => {
                let k = n.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
                let v = n.child_by_field_name("value").map(|x| self.arg(x)).unwrap_or(Arg::Expr(String::new()));
                Arg::Kw(k, Box::new(v))
            }
            (Lang::Py, "string") | (Lang::Ts | Lang::Js, "string" | "template_string")
            | (Lang::Go, "interpreted_string_literal" | "raw_string_literal")
            | (Lang::Rs, "string_literal" | "raw_string_literal") => {
                let (t, interp) = self.string_text(n);
                if interp {
                    Arg::Tmpl(t)
                } else {
                    Arg::Str(t)
                }
            }
            (Lang::Py, "concatenated_string") | (Lang::Py, "binary_operator")
            | (Lang::Ts | Lang::Js, "binary_expression") | (Lang::Go, "binary_expression") => {
                // "a" + x + "b": keep the literal fragments
                let mut frag = String::new();
                self.collect_strings(n, &mut frag, 0);
                if frag.is_empty() {
                    Arg::Expr(squash(self.text(n)))
                } else {
                    Arg::Tmpl(frag)
                }
            }
            (_, "identifier" | "attribute" | "member_expression" | "selector_expression"
                | "scoped_identifier" | "field_expression" | "dotted_name") => {
                Arg::Name(squash(self.text(n)))
            }
            _ => Arg::Expr(compact(self.text(n))),
        }
    }

    fn collect_strings(&self, n: Node, out: &mut String, depth: usize) {
        if depth > 50 {
            return;
        }
        if matches!(
            n.kind(),
            "string" | "template_string" | "interpreted_string_literal" | "raw_string_literal" | "string_literal"
        ) {
            out.push_str(&self.string_text(n).0);
            out.push(' ');
            return;
        }
        let mut c = n.walk();
        for k in n.children(&mut c) {
            self.collect_strings(k, out, depth + 1);
        }
    }

    /// Literal text of a string node, and whether it interpolates.
    fn string_text(&self, n: Node) -> (String, bool) {
        let raw = self.text(n);
        let mut interp = false;
        let mut c = n.walk();
        for k in n.children(&mut c) {
            if matches!(k.kind(), "interpolation" | "template_substitution") {
                interp = true;
            }
        }
        let t = match self.lang {
            Lang::Py => {
                let prefix_end = raw.find(['"', '\'']).unwrap_or(0);
                let prefix = raw[..prefix_end].to_ascii_lowercase();
                if prefix.contains('f') && raw.contains('{') {
                    interp = true;
                }
                let body = &raw[prefix_end..];
                let q = if body.starts_with("\"\"\"") || body.starts_with("'''") { 3 } else { 1 };
                if body.len() >= 2 * q { body[q..body.len() - q].to_string() } else { String::new() }
            }
            Lang::Rs => {
                let s = raw.trim_start_matches(['r', 'b']).trim_matches('#');
                s.trim_matches('"').to_string()
            }
            _ => {
                if raw.len() >= 2 {
                    raw[1..raw.len() - 1].to_string()
                } else {
                    String::new()
                }
            }
        };
        (t, interp)
    }

    fn string(&mut self, n: Node) {
        // skip docstrings? keep: SQL detection filters by shape.
        let (text, interpolated) = self.string_text(n);
        if text.len() < 6 {
            return;
        }
        self.out.strings.push(StrLit {
            byte: n.start_byte(),
            line: n.start_position().row + 1,
            text: text.chars().take(4000).collect(),
            interpolated,
        });
    }

    fn args_of(&self, n: Node) -> Vec<Arg> {
        let Some(a) = n.child_by_field_name("arguments") else { return Vec::new() };
        let mut c = a.walk();
        a.named_children(&mut c)
            .filter(|k| k.kind() != "comment")
            .map(|k| self.arg(k))
            .collect()
    }

    fn call(&mut self, n: Node, decorates: Option<(String, usize)>, macro_call: bool) {
        let f = n
            .child_by_field_name("function")
            .or_else(|| n.child_by_field_name("constructor"));
        let Some(f) = f else { return };
        let callee = squash(self.text(f));
        let args = self.args_of(n);
        let (class, class_decorators) = self
            .class_stack
            .last()
            .map(|(c, d)| (Some(c.clone()), d.clone()))
            .unwrap_or((None, Vec::new()));
        self.out.calls.push(CallFact {
            byte: n.start_byte(),
            end: n.end_byte(),
            fn_span: (f.start_byte(), f.end_byte()),
            line: n.start_position().row + 1,
            callee,
            args,
            decorates,
            class,
            class_decorators,
            macro_call,
            assign: self.assigned_to(n),
        });
    }

    fn macro_call(&mut self, n: Node) {
        let Some(m) = n.child_by_field_name("macro") else { return };
        let callee = format!("{}!", squash(self.text(m)));
        // arguments: literal strings among the token tree's direct children
        let mut args = Vec::new();
        let mut c = n.walk();
        for k in n.named_children(&mut c) {
            if k.kind() == "token_tree" {
                let mut c2 = k.walk();
                for t in k.named_children(&mut c2) {
                    args.push(match t.kind() {
                        "string_literal" | "raw_string_literal" => Arg::Str(self.string_text(t).0),
                        "identifier" => Arg::Name(self.text(t).to_string()),
                        _ => Arg::Expr(squash(self.text(t))),
                    });
                }
            }
        }
        self.out.calls.push(CallFact {
            byte: n.start_byte(),
            end: n.end_byte(),
            fn_span: (m.start_byte(), m.end_byte()),
            line: n.start_position().row + 1,
            callee,
            args,
            decorates: None,
            class: self.class_stack.last().map(|c| c.0.clone()),
            class_decorators: Vec::new(),
            macro_call: true,
            assign: None,
        });
    }

    fn py_decorated(&mut self, n: Node) {
        let Some(def) = n.child_by_field_name("definition") else { return };
        let name = def.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
        let line = def.start_position().row + 1;
        let mut c = n.walk();
        let decos: Vec<Node> = n.children(&mut c).filter(|k| k.kind() == "decorator").collect();
        for d in decos {
            let mut c2 = d.walk();
            let inner: Vec<Node> = d.named_children(&mut c2).collect();
            for e in inner {
                if e.kind() == "call" {
                    self.call(e, Some((name.clone(), line)), false);
                } else {
                    // bare decorator `@hookimpl`: record as a zero-arg call
                    let (class, class_decorators) = self
                        .class_stack
                        .last()
                        .map(|(c, d)| (Some(c.clone()), d.clone()))
                        .unwrap_or((None, Vec::new()));
                    self.out.calls.push(CallFact {
                        byte: e.start_byte(),
                        end: e.end_byte(),
                        fn_span: (e.start_byte(), e.end_byte()),
                        line: e.start_position().row + 1,
                        callee: squash(self.text(e)),
                        args: Vec::new(),
                        decorates: Some((name.clone(), line)),
                        class,
                        class_decorators,
                        macro_call: false,
                        assign: None,
                    });
                }
            }
        }
    }

    fn rs_attribute(&mut self, n: Node) {
        // #[get("/x")] fn handler(): the next sibling item is decorated
        let mut next = n.next_named_sibling();
        while let Some(s) = next {
            if s.kind() == "attribute_item" || s.kind() == "line_comment" {
                next = s.next_named_sibling();
            } else {
                break;
            }
        }
        let Some(item) = next else { return };
        let name = item.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
        let line = item.start_position().row + 1;
        let Some(attr) = n.named_child(0) else { return };
        let path = attr.named_child(0).map(|p| squash(self.text(p))).unwrap_or_default();
        let mut args = Vec::new();
        if let Some(tt) = attr.child_by_field_name("arguments") {
            let mut c = tt.walk();
            for t in tt.named_children(&mut c) {
                if matches!(t.kind(), "string_literal" | "raw_string_literal") {
                    args.push(Arg::Str(self.string_text(t).0));
                }
            }
        }
        self.out.calls.push(CallFact {
            byte: n.start_byte(),
            end: n.end_byte(),
            fn_span: (n.start_byte(), n.end_byte()),
            line: n.start_position().row + 1,
            callee: path,
            args,
            decorates: Some((name, line)),
            class: self.class_stack.last().map(|c| c.0.clone()),
            class_decorators: Vec::new(),
            macro_call: false,
            assign: None,
        });
    }

    fn ts_decorator(&mut self, n: Node) {
        // decorated member: the parent's next named sibling or the parent
        let target = n.parent().and_then(|p| {
            if p.kind() == "class_body" {
                let mut s = n.next_named_sibling();
                while let Some(x) = s {
                    if x.kind() == "decorator" {
                        s = x.next_named_sibling();
                    } else {
                        return Some(x);
                    }
                }
                None
            } else {
                Some(p)
            }
        });
        let Some(t) = target else { return };
        let name = t.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
        let line = t.start_position().row + 1;
        let Some(e) = n.named_child(0) else { return };
        if e.kind() == "call_expression" {
            self.call(e, Some((name, line)), false);
        }
    }

    fn py_subscript(&mut self, n: Node) {
        let Some(v) = n.child_by_field_name("value") else { return };
        let vt = squash(self.text(v));
        if !(vt.ends_with("environ") || vt.ends_with("config") || vt.ends_with("settings")) {
            return;
        }
        let key = n.child_by_field_name("subscript").and_then(|s| match s.kind() {
            "string" => Some(self.string_text(s).0),
            _ => None,
        });
        self.out.members.push(MemberRead {
            byte: n.start_byte(),
            line: n.start_position().row + 1,
            text: format!("{vt}[]"),
            key,
        });
    }

    fn ts_member(&mut self, n: Node) {
        let Some(obj) = n.child_by_field_name("object") else { return };
        let ot = squash(self.text(obj));
        if ot != "process.env" && ot != "import.meta.env" {
            return;
        }
        let key = if n.kind() == "member_expression" {
            n.child_by_field_name("property").map(|p| self.text(p).to_string())
        } else {
            n.child_by_field_name("index").and_then(|i| match i.kind() {
                "string" => Some(self.string_text(i).0),
                _ => None,
            })
        };
        self.out.members.push(MemberRead {
            byte: n.start_byte(),
            line: n.start_position().row + 1,
            text: format!("{ot}.*"),
            key,
        });
    }

    fn table(&mut self, n: Node, depth: usize) {
        let (name_node, value) = match self.lang {
            Lang::Py => (n.child_by_field_name("left"), n.child_by_field_name("right")),
            Lang::Ts | Lang::Js => (n.child_by_field_name("name"), n.child_by_field_name("value")),
            Lang::Go => {
                let name = n.child_by_field_name("name").or_else(|| n.child_by_field_name("left"));
                let value = n.child_by_field_name("value").or_else(|| n.child_by_field_name("right"));
                (name, value.and_then(|v| if v.kind() == "expression_list" { v.named_child(0) } else { Some(v) }))
            }
            _ => (None, None),
        };
        let (Some(nn), Some(v)) = (name_node, value) else { return };
        let name = squash(self.text(nn));
        let mut entries = Vec::new();
        match (self.lang, v.kind()) {
            (Lang::Py, "dictionary") | (Lang::Ts | Lang::Js, "object") => {
                let mut c = v.walk();
                for p in v.named_children(&mut c) {
                    if p.kind() != "pair" {
                        continue;
                    }
                    let k = p.child_by_field_name("key").map(|k| self.key_text(k));
                    let val = p.child_by_field_name("value").and_then(|x| self.name_of(x));
                    entries.push((k.flatten(), val));
                }
            }
            (Lang::Py, "list" | "tuple") | (Lang::Ts | Lang::Js, "array") => {
                let mut c = v.walk();
                for p in v.named_children(&mut c) {
                    entries.push((None, self.name_of(p)));
                }
            }
            (Lang::Go, "composite_literal") => {
                if let Some(body) = v.child_by_field_name("body") {
                    let mut c = body.walk();
                    for el in body.named_children(&mut c) {
                        if el.kind() == "keyed_element" {
                            let k = el.named_child(0).map(|k| self.key_text(k)).flatten();
                            let val = el.named_child(1).and_then(|x| self.name_of(x));
                            entries.push((k, val));
                        } else if el.kind() == "literal_element" {
                            entries.push((None, self.name_of(el)));
                        }
                    }
                }
            }
            _ => return,
        }
        if entries.is_empty() || entries.iter().all(|(_, v)| v.is_none()) {
            return;
        }
        self.out.tables.push(TableFact {
            name,
            line: n.start_position().row + 1,
            entries,
            module_level: depth <= 4,
        });
    }

    fn key_text(&self, k: Node) -> Option<String> {
        let k = if k.kind() == "literal_element" { k.named_child(0)? } else { k };
        match k.kind() {
            "string" | "interpreted_string_literal" | "raw_string_literal" => Some(self.string_text(k).0),
            "identifier" | "property_identifier" | "integer" | "number" => Some(self.text(k).to_string()),
            _ => None,
        }
    }

    fn name_of(&self, v: Node) -> Option<String> {
        let v = if v.kind() == "literal_element" { v.named_child(0)? } else { v };
        match v.kind() {
            "identifier" | "attribute" | "member_expression" | "selector_expression" | "dotted_name"
            | "scoped_identifier" | "field_expression" => Some(squash(self.text(v))),
            _ => None,
        }
    }

    fn py_class(&mut self, n: Node) {
        let name = n.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
        let mut bases = Vec::new();
        let mut header_kw = Vec::new();
        if let Some(sc) = n.child_by_field_name("superclasses") {
            let mut c = sc.walk();
            for b in sc.named_children(&mut c) {
                if b.kind() == "keyword_argument" {
                    let k = b.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
                    let v = b.child_by_field_name("value").map(|x| self.text(x).to_string()).unwrap_or_default();
                    header_kw.push((k, v));
                } else {
                    bases.push(squash(self.text(b)));
                }
            }
        }
        let mut attrs = Vec::new();
        if let Some(body) = n.child_by_field_name("body") {
            let mut c = body.walk();
            for st in body.named_children(&mut c) {
                let a = if st.kind() == "expression_statement" { st.named_child(0) } else { None };
                if let Some(a) = a.filter(|a| a.kind() == "assignment") {
                    let (Some(l), Some(r)) = (a.child_by_field_name("left"), a.child_by_field_name("right")) else {
                        continue;
                    };
                    if r.kind() == "string" {
                        attrs.push((self.text(l).to_string(), self.string_text(r).0));
                    }
                }
                // Django: class Meta: db_table = "x"
                if st.kind() == "class_definition"
                    && st.child_by_field_name("name").map(|x| self.text(x)) == Some("Meta")
                {
                    if let Some(mb) = st.child_by_field_name("body") {
                        let mut c2 = mb.walk();
                        for s2 in mb.named_children(&mut c2) {
                            if let Some(a) = s2.named_child(0).filter(|a| a.kind() == "assignment") {
                                if let (Some(l), Some(r)) = (a.child_by_field_name("left"), a.child_by_field_name("right")) {
                                    if r.kind() == "string" {
                                        attrs.push((format!("Meta.{}", self.text(l)), self.string_text(r).0));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let decorators = n
            .parent()
            .filter(|p| p.kind() == "decorated_definition")
            .map(|p| {
                let mut c = p.walk();
                p.children(&mut c)
                    .filter(|k| k.kind() == "decorator")
                    .map(|d| (squash(self.text(d)).trim_start_matches('@').to_string(), None))
                    .collect()
            })
            .unwrap_or_default();
        self.class_stack.push((name.clone(), Vec::new()));
        self.out.classes.push(ClassFact { name, line: n.start_position().row + 1, bases, decorators, attrs, header_kw });
    }

    fn ts_class(&mut self, n: Node) {
        let name = n.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
        let mut decorators = Vec::new();
        let mut c = n.walk();
        let mut decos: Vec<Node> = n.children(&mut c).filter(|k| k.kind() == "decorator").collect();
        // `export @Deco class` puts decorators on the export statement
        if let Some(p) = n.parent().filter(|p| p.kind() == "export_statement") {
            let mut c2 = p.walk();
            decos.extend(p.children(&mut c2).filter(|k| k.kind() == "decorator"));
        }
        for d in decos {
            if let Some(e) = d.named_child(0) {
                let (callee, first) = if e.kind() == "call_expression" {
                    let callee = e.child_by_field_name("function").map(|f| squash(self.text(f))).unwrap_or_default();
                    let first = self.args_of(e).first().and_then(|a| a.literal().map(|s| s.to_string()));
                    (callee, first)
                } else {
                    (squash(self.text(e)), None)
                };
                decorators.push((callee, first));
            }
        }
        let mut bases = Vec::new();
        let mut c3 = n.walk();
        for k in n.children(&mut c3) {
            if k.kind() == "class_heritage" {
                bases.push(squash(self.text(k)));
            }
        }
        self.class_stack.push((name.clone(), decorators.clone()));
        self.out.classes.push(ClassFact {
            name,
            line: n.start_position().row + 1,
            bases,
            decorators,
            attrs: Vec::new(),
            header_kw: Vec::new(),
        });
    }

    fn go_type(&mut self, n: Node) {
        let Some(t) = n.child_by_field_name("type").filter(|t| t.kind() == "struct_type") else { return };
        let name = n.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
        // embedded fields (gorm.Model) count as bases
        let mut bases = Vec::new();
        let body = self.text(t);
        for l in body.lines().skip(1) {
            let l = l.trim();
            if !l.is_empty() && !l.contains(' ') && !l.contains('\t') && l != "}" {
                bases.push(l.to_string());
            }
        }
        self.out.classes.push(ClassFact {
            name,
            line: n.start_position().row + 1,
            bases,
            decorators: Vec::new(),
            attrs: Vec::new(),
            header_kw: Vec::new(),
        });
    }

    fn go_method(&mut self, n: Node) {
        // func (T) TableName() string { return "x" }
        let name = n.child_by_field_name("name").map(|x| self.text(x)).unwrap_or("");
        if name != "TableName" {
            return;
        }
        let recv = n
            .child_by_field_name("receiver")
            .map(|r| self.text(r).trim_matches(|c| c == '(' || c == ')').to_string())
            .unwrap_or_default();
        let ty = recv.split_whitespace().last().unwrap_or("").trim_start_matches('*').to_string();
        let body = n.child_by_field_name("body").map(|b| self.text(b)).unwrap_or("");
        if let Some(i) = body.find('"') {
            if let Some(j) = body[i + 1..].find('"') {
                let lit = body[i + 1..i + 1 + j].to_string();
                self.out.classes.push(ClassFact {
                    name: ty,
                    line: n.start_position().row + 1,
                    bases: Vec::new(),
                    decorators: Vec::new(),
                    attrs: vec![("TableName".into(), lit)],
                    header_kw: Vec::new(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_calls_decorators_tables() {
        let src = r#"
import os
HANDLERS = {"a": do_a, "b": mod.do_b}

@app.route("/users/<int:id>", methods=["GET"])
def get_user(id):
    db.execute("SELECT * FROM users WHERE id = ?", (id,))
    k = os.environ["SECRET"]
    return HANDLERS[k](f"x{id}")

class User(Base):
    __tablename__ = "users"
"#;
        let (_, s) = scan_file("m.py", src).unwrap();
        let route = s.calls.iter().find(|c| c.callee == "app.route").unwrap();
        assert_eq!(route.decorates.as_ref().unwrap().0, "get_user");
        assert_eq!(route.args[0], Arg::Str("/users/<int:id>".into()));
        let ex = s.calls.iter().find(|c| c.callee == "db.execute").unwrap();
        assert!(ex.args[0].literal().unwrap().starts_with("SELECT"));
        assert_eq!(s.members[0].key.as_deref(), Some("SECRET"));
        assert_eq!(s.tables[0].name, "HANDLERS");
        assert_eq!(s.tables[0].entries[1], (Some("b".into()), Some("mod.do_b".into())));
        let tbl_call = s.calls.iter().find(|c| c.callee == "HANDLERS[k]").unwrap();
        assert!(matches!(tbl_call.args[0], Arg::Tmpl(_)));
        assert_eq!(s.classes[0].attrs, [("__tablename__".to_string(), "users".to_string())]);
    }

    #[test]
    fn squash_keeps_grouping_parens() {
        assert_eq!(squash("(options.client ?? client).post<X>({ url: '/a' })"), "(options.client??client).post<X>()");
        assert_eq!(squash("a.b(x, f(y)).c(z)"), "a.b().c()");
        assert_eq!(squash("HANDLERS[k](v)"), "HANDLERS[k]()");
    }

    #[test]
    fn go_ts_rust_shapes() {
        let go = "package m\nvar routes = map[string]Handler{\"a\": handleA}\nfunc f() { os.Getenv(\"PORT\"); r.GET(\"/x\", h) }\nfunc (User) TableName() string { return \"people\" }\n";
        let (_, s) = scan_file("m.go", go).unwrap();
        assert!(s.calls.iter().any(|c| c.callee == "os.Getenv" && c.args[0] == Arg::Str("PORT".into())));
        assert_eq!(s.tables[0].entries[0], (Some("a".into()), Some("handleA".into())));
        assert_eq!(s.classes[0].attrs[0].1, "people");
        let ts = "@Controller('articles')\nexport class A { @Get(':slug') find() { return process.env.PORT } }\nconst x = fetch(`/api/${id}`)\n";
        let (_, s) = scan_file("a.ts", ts).unwrap();
        let get = s.calls.iter().find(|c| c.callee == "Get").unwrap();
        assert_eq!(get.decorates.as_ref().unwrap().0, "find");
        assert_eq!(get.class_decorators[0], ("Controller".into(), Some("articles".into())));
        assert_eq!(s.members[0].key.as_deref(), Some("PORT"));
        assert!(matches!(s.calls.iter().find(|c| c.callee == "fetch").unwrap().args[0], Arg::Tmpl(_)));
        let rs = "#[get(\"/health\")]\nasync fn health() {}\nfn g() { let x = sqlx::query!(\"SELECT 1 FROM t\", a); std::env::var(\"K\"); }\n";
        let (_, s) = scan_file("a.rs", rs).unwrap();
        assert!(s.calls.iter().any(|c| c.callee == "get" && c.decorates.as_ref().unwrap().0 == "health"));
        assert!(s.calls.iter().any(|c| c.callee == "sqlx::query!" && c.args[0].literal().is_some()));
        assert!(s.calls.iter().any(|c| c.callee == "std::env::var"));
    }
}
