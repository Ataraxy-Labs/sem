//! Stage 1: syntax tree -> [`DfFile`]. One walker for all four languages;
//! the differences are node-kind names, kept in the small `match`es below.
//!
//! The walker is conservative in one direction only: everything a value
//! *could* be computed from is kept (all identifiers under an expression,
//! except type annotations and keyword names), and closures are inlined
//! into the function that creates them (their parameters become that
//! function's locals, their `return`s the closure's value). Constructs it
//! does not understand become [`Callee::Dynamic`] or a [`Dynamic`] marker.

use tree_sitter::Node;

use super::ir::*;

pub fn lower(lang: Lang, path: &str, tree: &tree_sitter::Tree, src: &str) -> DfFile {
    let mut l = Lower {
        lang,
        src,
        file: DfFile {
            path: path.to_string(),
            ..Default::default()
        },
        ret_redirect: Vec::new(),
        declared_globals: Vec::new(),
    };
    let root = tree.root_node();
    l.file.fns.push(DfFn {
        name: "<module>".into(),
        row: 0,
        end_row: root.end_position().row as u32,
        is_module: true,
        ..Default::default()
    });
    l.top(root, None);
    let (c, g) = complexity(lang, root, true);
    l.file.fns[0].cyclomatic = c;
    l.file.fns[0].cognitive = g;
    l.file.globals.sort();
    l.file.globals.dedup();
    l.file
}

/// Parse and lower a file from source text.
pub fn lower_source(path: &str, src: &str) -> Option<DfFile> {
    let lang = Lang::for_path(path)?;
    let tree = parse(path, src)?;
    Some(lower(lang, path, &tree, src))
}

pub fn parse(path: &str, src: &str) -> Option<tree_sitter::Tree> {
    let ext = path.rfind('.').map(|i| &path[i..]).unwrap_or("");
    let config = crate::parser::plugins::code::languages::get_language_config(ext)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&(config.get_language)()?).ok()?;
    parser.parse(src, None)
}

fn kids<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}

fn row(n: Node) -> u32 {
    n.start_position().row as u32
}

struct Lower<'s> {
    lang: Lang,
    src: &'s str,
    file: DfFile,
    /// Inside an inlined closure: where its `return`s go.
    ret_redirect: Vec<Val>,
    /// Python `global x` names of the function being lowered.
    declared_globals: Vec<String>,
}

impl<'s> Lower<'s> {
    fn text(&self, n: Node) -> &'s str {
        n.utf8_text(self.src.as_bytes()).unwrap_or("")
    }

    fn fnm(&mut self, fi: usize) -> &mut DfFn {
        &mut self.file.fns[fi]
    }

    // ---------------------------------------------------------------- top level

    /// Declarations at module / class level. Statements that are not
    /// declarations belong to the module pseudo-function (0).
    fn top(&mut self, n: Node, owner: Option<&str>) {
        for k in kids(n) {
            self.top_item(k, owner);
        }
    }

    fn top_item(&mut self, k: Node, owner: Option<&str>) {
        use Lang::*;
        match (self.lang, k.kind()) {
            (Python, "function_definition") => self.function(k, owner, &[]),
            (Python, "decorated_definition") => {
                let decos: Vec<Node> = kids(k).into_iter().filter(|d| d.kind() == "decorator").collect();
                for d in &decos {
                    self.stmt(*d, 0);
                }
                let names: Vec<String> = decos.iter().map(|d| self.text(*d).to_string()).collect();
                if let Some(def) = k.child_by_field_name("definition") {
                    match def.kind() {
                        "function_definition" => self.function(def, owner, &names),
                        _ => self.top_item(def, owner),
                    }
                }
            }
            (Python, "class_definition") => {
                let name = k.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
                let bases: Vec<String> = k
                    .child_by_field_name("superclasses")
                    .map(|s| kids(s).into_iter().map(|b| self.text(b).to_string()).collect())
                    .unwrap_or_default();
                self.file.classes.push((name.clone(), bases));
                if let Some(b) = k.child_by_field_name("body") {
                    for item in kids(b) {
                        match item.kind() {
                            "function_definition" | "decorated_definition" | "class_definition" => {
                                self.top_item(item, Some(&name))
                            }
                            // class attributes: class-level state, written by the module
                            _ => self.stmt(item, 0),
                        }
                    }
                }
            }
            (Ts, "function_declaration" | "generator_function_declaration") => self.function(k, owner, &[]),
            (Ts, "export_statement") => {
                if let Some(d) = k.child_by_field_name("declaration") {
                    self.top_item(d, owner);
                } else {
                    for c in kids(k) {
                        if !matches!(c.kind(), "export_clause" | "string") {
                            self.top_item(c, owner);
                        }
                    }
                }
            }
            (Ts, "class_declaration" | "abstract_class_declaration" | "class") => {
                let name = k.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
                let mut bases = Vec::new();
                for c in kids(k) {
                    if c.kind() == "class_heritage" {
                        for h in kids(c) {
                            if h.kind() == "extends_clause" {
                                if let Some(v) = h.child_by_field_name("value") {
                                    bases.push(self.text(v).to_string());
                                }
                            }
                        }
                    }
                }
                self.file.classes.push((name.clone(), bases));
                if let Some(b) = k.child_by_field_name("body") {
                    for item in kids(b) {
                        match item.kind() {
                            "method_definition" => self.function(item, Some(&name), &[]),
                            "public_field_definition" | "field_definition" => {
                                let v = item.child_by_field_name("value");
                                match v {
                                    Some(v) if is_lambda(self.lang, v.kind()) => {
                                        let nm = item
                                            .child_by_field_name("name")
                                            .or_else(|| item.child_by_field_name("property"))
                                            .map(|x| self.text(x).to_string())
                                            .unwrap_or_default();
                                        self.function_node(v, nm, k, Some(&name), &[]);
                                    }
                                    Some(v) => {
                                        let val = self.val(v, 0);
                                        self.fnm(0).stmts.push(Stmt::Eval(val));
                                    }
                                    None => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            (Ts, "lexical_declaration" | "variable_declaration") => {
                for d in kids(k) {
                    if d.kind() != "variable_declarator" {
                        continue;
                    }
                    let (Some(nm), v) = (d.child_by_field_name("name"), d.child_by_field_name("value")) else {
                        continue;
                    };
                    if nm.kind() == "identifier" {
                        if let Some(v) = v.filter(|v| is_lambda(self.lang, v.kind())) {
                            self.function_node(v, self.text(nm).to_string(), d, owner, &[]);
                            continue;
                        }
                        self.file.globals.push(self.text(nm).to_string());
                    } else {
                        for b in self.binders(nm) {
                            self.file.globals.push(b);
                        }
                    }
                    self.stmt(d, 0);
                }
            }
            (Go, "function_declaration") => self.function(k, owner, &[]),
            (Go, "method_declaration") => {
                let recv_ty = k
                    .child_by_field_name("receiver")
                    .and_then(|r| kids(r).into_iter().next())
                    .and_then(|p| p.child_by_field_name("type"))
                    .map(|t| self.text(t).trim_start_matches('*').split('[').next().unwrap_or("").to_string());
                self.function(k, recv_ty.as_deref(), &[]);
            }
            (Go, "var_declaration" | "const_declaration") => {
                self.collect_go_globals(k);
                self.stmt(k, 0);
            }
            (Rust, "function_item") => self.function(k, owner, &[]),
            (Rust, "impl_item") => {
                let ty = k.child_by_field_name("type").map(|t| {
                    let t = self.text(t);
                    t.split('<').next().unwrap_or(t).rsplit("::").next().unwrap_or(t).to_string()
                });
                if let Some(b) = k.child_by_field_name("body") {
                    self.top(b, ty.as_deref());
                }
            }
            (Rust, "trait_item") => {
                let ty = k.child_by_field_name("name").map(|t| self.text(t).to_string());
                if let Some(b) = k.child_by_field_name("body") {
                    self.top(b, ty.as_deref());
                }
            }
            (Rust, "mod_item") => {
                if let Some(b) = k.child_by_field_name("body") {
                    self.top(b, None);
                }
            }
            (Rust, "static_item" | "const_item") => {
                if let Some(nm) = k.child_by_field_name("name") {
                    self.file.globals.push(self.text(nm).to_string());
                }
                self.stmt(k, 0);
            }
            (Python, "expression_statement") => {
                // module-level assignments declare module state
                for c in kids(k) {
                    if matches!(c.kind(), "assignment" | "augmented_assignment") {
                        if let Some(l) = c.child_by_field_name("left") {
                            for b in self.binders(l) {
                                self.file.globals.push(b);
                            }
                        }
                    }
                }
                self.stmt(k, 0);
            }
            (Ts, "import_statement") => self.ts_import(k),
            (Python, "import_statement" | "import_from_statement" | "future_import_statement") => {}
            (Go, "import_declaration" | "package_clause") => {}
            (Rust, "use_declaration" | "extern_crate_declaration" | "attribute_item" | "struct_item" | "enum_item" | "type_item") => {}
            _ => self.stmt(k, 0),
        }
    }

    fn collect_go_globals(&mut self, n: Node) {
        for c in kids(n) {
            match c.kind() {
                "var_spec" | "const_spec" => {
                    for nm in kids(c) {
                        if nm.kind() == "identifier" {
                            self.file.globals.push(self.text(nm).to_string());
                        }
                    }
                }
                "var_spec_list" | "const_spec_list" => self.collect_go_globals(c),
                _ => {}
            }
        }
    }

    fn function(&mut self, n: Node, owner: Option<&str>, decorators: &[String]) {
        let name = n.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
        let is_static = decorators.iter().any(|d| d.contains("staticmethod"));
        self.function_node(n, name, n, owner, if is_static { &["static"] } else { &[] });
    }

    /// A function whose parameters/body are on `f` and whose span is `span`.
    fn function_node(&mut self, f: Node, name: String, span: Node, owner: Option<&str>, flags: &[&str]) {
        let fi = self.file.fns.len();
        let mut df = DfFn {
            name,
            row: row(span),
            end_row: span.end_position().row as u32,
            owner: owner.map(str::to_string),
            ..Default::default()
        };
        let mut params = self.params(f);
        match self.lang {
            Lang::Python if owner.is_some() && !flags.contains(&"static") && !params.is_empty() => {
                df.self_name = Some(params.remove(0).name);
            }
            Lang::Ts if owner.is_some() => df.self_name = Some("this".into()),
            Lang::Go => {
                if let Some(r) = f.child_by_field_name("receiver") {
                    if let Some(p) = self.params_of_list(r).into_iter().next() {
                        df.self_name = Some(p.name);
                    }
                }
            }
            Lang::Rust => {
                if let Some(ps) = f.child_by_field_name("parameters") {
                    if kids(ps).iter().any(|p| p.kind() == "self_parameter") {
                        df.self_name = Some("self".into());
                    }
                }
            }
            _ => {}
        }
        df.params = params;
        self.file.fns.push(df);
        let saved = std::mem::take(&mut self.declared_globals);
        if let Some(body) = f.child_by_field_name("body") {
            self.body(body, fi);
            let (c, g) = complexity(self.lang, body, false);
            self.file.fns[fi].cyclomatic = c;
            self.file.fns[fi].cognitive = g;
        }
        self.declared_globals = saved;
    }

    /// A function body: statements, and the value of an expression body
    /// (arrow functions) or a Rust tail expression, returned.
    fn body(&mut self, body: Node, fi: usize) {
        match (self.lang, body.kind()) {
            (Lang::Ts, k) if k != "statement_block" => {
                let v = self.val(body, fi);
                self.ret(v, fi);
            }
            (Lang::Python, k) if k != "block" => {
                // lambda body
                let v = self.val(body, fi);
                self.ret(v, fi);
            }
            (Lang::Rust, "block") => {
                let v = self.block_val(body, fi);
                self.ret(v, fi);
            }
            (Lang::Rust, _) => {
                let v = self.val(body, fi);
                self.ret(v, fi);
            }
            _ => self.stmt(body, fi),
        }
    }

    fn ret(&mut self, v: Val, fi: usize) {
        match self.ret_redirect.last_mut() {
            Some(r) => r.extend(v),
            None => self.fnm(fi).stmts.push(Stmt::Return(v)),
        }
    }

    // ---------------------------------------------------------------- params

    fn params(&mut self, f: Node) -> Vec<Param> {
        let Some(ps) = f
            .child_by_field_name("parameters")
            .or_else(|| f.child_by_field_name("parameter"))
        else {
            return Vec::new();
        };
        if ps.kind() == "identifier" {
            // `x => ..`
            return vec![Param { name: self.text(ps).to_string(), ty: None }];
        }
        self.params_of_list(ps)
    }

    fn params_of_list(&mut self, ps: Node) -> Vec<Param> {
        let mut out = Vec::new();
        for p in kids(ps) {
            let ty = p.child_by_field_name("type").map(|t| {
                let t = self.text(t);
                t.trim_start_matches(':').trim().to_string()
            });
            match (self.lang, p.kind()) {
                (_, "identifier") => out.push(Param { name: self.text(p).into(), ty: None }),
                (Lang::Python, "typed_parameter") => {
                    if let Some(id) = kids(p).into_iter().find(|c| c.kind() == "identifier") {
                        out.push(Param { name: self.text(id).into(), ty });
                    }
                }
                (Lang::Python, "default_parameter" | "typed_default_parameter") => {
                    if let Some(nm) = p.child_by_field_name("name") {
                        out.push(Param { name: self.text(nm).into(), ty });
                    }
                }
                (Lang::Python, "list_splat_pattern" | "dictionary_splat_pattern") => {
                    for b in self.binders(p) {
                        out.push(Param { name: b, ty: None });
                    }
                }
                (Lang::Ts, "required_parameter" | "optional_parameter") => {
                    let pat = p.child_by_field_name("pattern").unwrap_or(p);
                    let names = self.binders(pat);
                    // a destructured parameter is one argument
                    let name = if names.len() == 1 { names[0].clone() } else { names.join(",") };
                    out.push(Param { name, ty });
                }
                (Lang::Ts, _) => {
                    let names = self.binders(p);
                    if !names.is_empty() {
                        out.push(Param { name: names.join(","), ty: None });
                    }
                }
                (Lang::Go, "parameter_declaration" | "variadic_parameter_declaration") => {
                    let names: Vec<Node> = kids(p).into_iter().filter(|c| c.kind() == "identifier").collect();
                    if names.is_empty() {
                        out.push(Param { name: "_".into(), ty: ty.clone() });
                    }
                    for nm in names {
                        out.push(Param { name: self.text(nm).into(), ty: ty.clone() });
                    }
                }
                (Lang::Rust, "parameter") => {
                    let pat = p.child_by_field_name("pattern").unwrap_or(p);
                    let names = self.binders(pat);
                    out.push(Param { name: names.join(","), ty });
                }
                (Lang::Rust, "self_parameter") => {}
                (Lang::Rust, "closure_parameters") => out.extend(self.params_of_list(p)),
                _ => {}
            }
        }
        out
    }

    /// Names a pattern / assignment target binds.
    fn binders(&self, n: Node) -> Vec<String> {
        let mut out = Vec::new();
        self.binders_into(n, &mut out);
        out
    }

    fn binders_into(&self, n: Node, out: &mut Vec<String>) {
        match n.kind() {
            "identifier" | "shorthand_property_identifier_pattern" | "shorthand_property_identifier" => {
                out.push(self.text(n).to_string())
            }
            // types, field names, default values: not binders
            "type" | "type_annotation" | "type_identifier" | "field_identifier" | "property_identifier"
            | "scoped_identifier" | "scoped_type_identifier" | "generic_type" | "primitive_type" => {}
            "pair_pattern" => {
                if let Some(v) = n.child_by_field_name("value") {
                    self.binders_into(v, out);
                }
            }
            "assignment_pattern" | "object_assignment_pattern" => {
                if let Some(l) = n.child_by_field_name("left") {
                    self.binders_into(l, out);
                }
            }
            "field_pattern" => match n.child_by_field_name("pattern") {
                Some(p) => self.binders_into(p, out),
                None => {
                    if let Some(nm) = n.child_by_field_name("name") {
                        out.push(self.text(nm).to_string());
                    }
                }
            },
            _ => {
                for k in kids(n) {
                    self.binders_into(k, out);
                }
            }
        }
    }

    // ---------------------------------------------------------------- imports

    fn ts_import(&mut self, n: Node) {
        let Some(src) = n.child_by_field_name("source") else { return };
        let spec = self.text(src).trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string();
        let base = spec.strip_prefix("node:").unwrap_or(&spec).to_string();
        for c in kids(n) {
            if c.kind() != "import_clause" {
                continue;
            }
            for x in kids(c) {
                match x.kind() {
                    "identifier" => self.file.imports.push(Import {
                        local: self.text(x).into(),
                        path: base.clone(),
                        spec: Some(spec.clone()),
                        member: Some("default".into()),
                    }),
                    "namespace_import" => {
                        if let Some(id) = kids(x).into_iter().find(|i| i.kind() == "identifier") {
                            self.file.imports.push(Import {
                                local: self.text(id).into(),
                                path: base.clone(),
                                spec: Some(spec.clone()),
                                member: None,
                            });
                        }
                    }
                    "named_imports" => {
                        for s in kids(x) {
                            if s.kind() != "import_specifier" {
                                continue;
                            }
                            let Some(nm) = s.child_by_field_name("name") else { continue };
                            let local = s.child_by_field_name("alias").unwrap_or(nm);
                            let member = self.text(nm).to_string();
                            self.file.imports.push(Import {
                                local: self.text(local).into(),
                                path: format!("{base}.{member}"),
                                spec: Some(spec.clone()),
                                member: Some(member),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// `const x = require('m')` / `const {a} = require('m')`: an import.
    fn ts_require(&mut self, decl: Node) -> bool {
        let (Some(nm), Some(v)) = (decl.child_by_field_name("name"), decl.child_by_field_name("value")) else {
            return false;
        };
        if v.kind() != "call_expression" {
            return false;
        }
        let Some(f) = v.child_by_field_name("function") else { return false };
        if self.text(f) != "require" {
            return false;
        }
        let Some(arg) = v.child_by_field_name("arguments").and_then(|a| kids(a).into_iter().next()) else {
            return false;
        };
        if arg.kind() != "string" {
            return false;
        }
        let spec = self.text(arg).trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string();
        let base = spec.strip_prefix("node:").unwrap_or(&spec).to_string();
        match nm.kind() {
            "identifier" => self.file.imports.push(Import {
                local: self.text(nm).into(),
                path: base,
                spec: Some(spec),
                member: None,
            }),
            "object_pattern" => {
                for p in kids(nm) {
                    let (member, local) = match p.kind() {
                        "shorthand_property_identifier_pattern" => (self.text(p).to_string(), self.text(p).to_string()),
                        "pair_pattern" => {
                            let (Some(k), Some(v)) = (p.child_by_field_name("key"), p.child_by_field_name("value")) else {
                                continue;
                            };
                            (self.text(k).to_string(), self.text(v).to_string())
                        }
                        _ => continue,
                    };
                    self.file.imports.push(Import {
                        local,
                        path: format!("{base}.{member}"),
                        spec: Some(spec.clone()),
                        member: Some(member),
                    });
                }
            }
            _ => return false,
        }
        true
    }

    // ---------------------------------------------------------------- statements

    fn stmt(&mut self, n: Node, fi: usize) {
        use Lang::*;
        let kind = n.kind();
        match (self.lang, kind) {
            (_, "comment" | "line_comment" | "block_comment") => {}
            // nested named functions: inlined as a closure bound to the name
            (Python, "function_definition") | (Ts, "function_declaration" | "generator_function_declaration") | (Rust, "function_item") => {
                let v = self.lambda(n, fi);
                if let Some(nm) = n.child_by_field_name("name") {
                    let r = row(n);
                    let name = self.text(nm).to_string();
                    self.fnm(fi).stmts.push(Stmt::Assign { to: vec![Place::Local(name)], from: v, row: r });
                }
            }
            (Python, "decorated_definition") => {
                for k in kids(n) {
                    self.stmt(k, fi);
                }
            }
            (Python, "class_definition") | (Ts, "class_declaration") => {
                // a class nested in a function: its methods inline into it
                if let Some(b) = n.child_by_field_name("body") {
                    for item in kids(b) {
                        self.stmt(item, fi);
                    }
                }
            }
            (Ts, "method_definition") => {
                let _ = self.lambda(n, fi);
            }
            (Python, "global_statement" | "nonlocal_statement") => {
                for k in kids(n) {
                    if k.kind() == "identifier" {
                        self.declared_globals.push(self.text(k).to_string());
                    }
                }
            }
            (Python, "assignment" | "augmented_assignment") | (Ts, "assignment_expression" | "augmented_assignment_expression") | (Rust, "assignment_expression" | "compound_assignment_expr") => {
                let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) else {
                    // annotated declaration without value (`x: int`)
                    return;
                };
                let mut v = self.val(r, fi);
                if kind.contains("augmented") || kind.contains("compound") {
                    v.extend(self.val(l, fi));
                }
                let to = self.places(l, fi, self.lang == Python);
                self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
            }
            (Python, "named_expression") => {
                let _ = self.val(n, fi);
            }
            (Ts, "variable_declarator") => {
                if self.ts_require(n) {
                    return;
                }
                let Some(nm) = n.child_by_field_name("name") else { return };
                let v = match n.child_by_field_name("value") {
                    Some(v) => self.val(v, fi),
                    None => Val::default(),
                };
                let to = self.binders(nm).into_iter().map(|b| self.decl_place(b, fi)).collect();
                self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
            }
            (Go, "short_var_declaration" | "assignment_statement") => {
                let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) else {
                    return;
                };
                let ls = kids(l);
                let rs = kids(r);
                let decl = kind == "short_var_declaration";
                if ls.len() == rs.len() && ls.len() > 1 {
                    for (a, b) in ls.iter().zip(rs.iter()) {
                        let v = self.val(*b, fi);
                        let to = if decl { self.binders(*a).into_iter().map(|x| self.decl_place(x, fi)).collect() } else { self.places(*a, fi, false) };
                        self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
                    }
                } else {
                    let mut v = self.val(r, fi);
                    if self.text(n).contains("+=") || self.text(n).contains("|=") {
                        v.extend(self.val(l, fi));
                    }
                    let mut to: Vec<Place> = if decl { self.binders(l).into_iter().map(|x| self.decl_place(x, fi)).collect() } else { self.places(l, fi, false) };
                    // `x, err := f()`: by Go convention the trailing error
                    // result is a status, not the call's data
                    if ls.len() > 1 && rs.len() == 1 {
                        if let Some(Place::Local(e) | Place::Name(e)) = to.last() {
                            if e == "err" || e.ends_with("Err") {
                                let status = to.pop().expect("last");
                                self.fnm(fi).stmts.push(Stmt::Assign { to: vec![status], from: Val::default(), row: row(n) });
                            }
                        }
                    }
                    self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
                }
            }
            (Go, "var_spec" | "const_spec") => {
                let names: Vec<String> = kids(n).into_iter().filter(|c| c.kind() == "identifier").map(|c| self.text(c).to_string()).collect();
                let v = match n.child_by_field_name("value") {
                    Some(v) => self.val(v, fi),
                    None => Val::default(),
                };
                let to = names.into_iter().map(|b| self.decl_place(b, fi)).collect();
                self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
            }
            (Rust, "let_declaration") => {
                let Some(p) = n.child_by_field_name("pattern") else { return };
                let v = match n.child_by_field_name("value") {
                    Some(v) => self.val(v, fi),
                    None => Val::default(),
                };
                if let Some(alt) = n.child_by_field_name("alternative") {
                    let _ = self.block_val(alt, fi);
                }
                let to = self.binders(p).into_iter().map(|b| self.decl_place(b, fi)).collect();
                self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
            }
            (Rust, "static_item" | "const_item") => {
                if let (Some(nm), Some(v)) = (n.child_by_field_name("name"), n.child_by_field_name("value")) {
                    let v = self.val(v, fi);
                    let name = self.text(nm).to_string();
                    self.fnm(fi).stmts.push(Stmt::Assign { to: vec![Place::Name(name)], from: v, row: row(n) });
                }
            }
            (Python | Ts | Go, "return_statement") | (Rust, "return_expression") | (Python, "yield") | (Ts, "yield_expression") => {
                let v = match kids(n).into_iter().next() {
                    Some(e) => self.val(e, fi),
                    None => Val::default(),
                };
                self.ret(v, fi);
            }
            // loops bind their variable to an element of the iterable
            (Python, "for_statement") | (Ts, "for_in_statement") => {
                let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) else {
                    return;
                };
                let v = self.val(r, fi);
                let to = self.places(l, fi, true);
                self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
                if let Some(b) = n.child_by_field_name("body") {
                    self.stmt(b, fi);
                }
                if let Some(b) = n.child_by_field_name("alternative") {
                    self.stmt(b, fi);
                }
            }
            (Go, "range_clause") => {
                let Some(r) = n.child_by_field_name("right") else { return };
                let v = self.val(r, fi);
                if let Some(l) = n.child_by_field_name("left") {
                    let to = self.binders(l).into_iter().map(|b| self.decl_place(b, fi)).collect();
                    self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
                } else {
                    self.fnm(fi).stmts.push(Stmt::Eval(v));
                }
            }
            (Rust, "for_expression") => {
                if let (Some(p), Some(v)) = (n.child_by_field_name("pattern"), n.child_by_field_name("value")) {
                    let v = self.val(v, fi);
                    let to = self.binders(p).into_iter().map(|b| self.decl_place(b, fi)).collect();
                    self.fnm(fi).stmts.push(Stmt::Assign { to, from: v, row: row(n) });
                }
                if let Some(b) = n.child_by_field_name("body") {
                    let _ = self.block_val(b, fi);
                }
            }
            (Python, "with_statement") => {
                for c in kids(n) {
                    if c.kind() == "with_clause" {
                        for item in kids(c) {
                            let Some(v) = item.child_by_field_name("value") else { continue };
                            if v.kind() == "as_pattern" {
                                let vals = kids(v);
                                if let Some(e) = vals.first() {
                                    let val = self.val(*e, fi);
                                    let to = match v.child_by_field_name("alias") {
                                        Some(a) => self.places(a, fi, true),
                                        None => Vec::new(),
                                    };
                                    self.fnm(fi).stmts.push(Stmt::Assign { to, from: val, row: row(item) });
                                }
                            } else {
                                let val = self.val(v, fi);
                                self.fnm(fi).stmts.push(Stmt::Eval(val));
                            }
                        }
                    } else {
                        self.stmt(c, fi);
                    }
                }
            }
            (Python, "except_clause") => {
                // `except E as e`: e is an exception object (not modeled)
                for c in kids(n) {
                    if c.kind() == "block" {
                        self.stmt(c, fi);
                    }
                }
            }
            (Python, "expression_statement") => {
                for c in kids(n) {
                    match c.kind() {
                        "assignment" | "augmented_assignment" | "yield" => self.stmt(c, fi),
                        _ => {
                            let v = self.val(c, fi);
                            self.fnm(fi).stmts.push(Stmt::Eval(v));
                        }
                    }
                }
            }
            (Ts, "expression_statement") => {
                for c in kids(n) {
                    match c.kind() {
                        "assignment_expression" | "augmented_assignment_expression" | "yield_expression" => self.stmt(c, fi),
                        _ => {
                            let v = self.val(c, fi);
                            self.fnm(fi).stmts.push(Stmt::Eval(v));
                        }
                    }
                }
            }
            (Go, "expression_statement" | "go_statement" | "defer_statement" | "send_statement" | "inc_statement" | "dec_statement") => {
                let v = self.val(n, fi);
                self.fnm(fi).stmts.push(Stmt::Eval(v));
            }
            (Rust, "expression_statement") => {
                for c in kids(n) {
                    self.rust_expr_stmt(c, fi);
                }
            }
            (Rust, "block") => {
                let v = self.block_val(n, fi);
                self.fnm(fi).stmts.push(Stmt::Eval(v));
            }
            (Python, "if_statement" | "while_statement" | "elif_clause" | "match_statement" | "case_clause" | "assert_statement" | "raise_statement" | "delete_statement" | "print_statement") => {
                for c in kids(n) {
                    if is_py_expr(c.kind()) {
                        let v = self.val(c, fi);
                        self.fnm(fi).stmts.push(Stmt::Eval(v));
                    } else {
                        self.stmt(c, fi);
                    }
                }
            }
            (Ts, "if_statement" | "while_statement" | "do_statement" | "switch_statement" | "throw_statement" | "for_statement" | "parenthesized_expression" | "switch_case") => {
                for c in kids(n) {
                    if is_ts_stmt(c.kind()) {
                        self.stmt(c, fi);
                    } else {
                        let v = self.val(c, fi);
                        self.fnm(fi).stmts.push(Stmt::Eval(v));
                    }
                }
            }
            (Go, "if_statement" | "for_statement" | "expression_switch_statement" | "type_switch_statement" | "select_statement" | "for_clause" | "expression_case" | "communication_case" | "type_case" | "default_case") => {
                for c in kids(n) {
                    if is_go_stmt(c.kind()) {
                        self.stmt(c, fi);
                    } else {
                        let v = self.val(c, fi);
                        self.fnm(fi).stmts.push(Stmt::Eval(v));
                    }
                }
            }
            (Python, _) if is_py_expr(kind) => {
                let v = self.val(n, fi);
                self.fnm(fi).stmts.push(Stmt::Eval(v));
            }
            (Rust, _) if is_rust_expr(kind) => self.rust_expr_stmt(n, fi),
            (Ts, _) if !is_ts_stmt(kind) && kind.ends_with("expression") => {
                let v = self.val(n, fi);
                self.fnm(fi).stmts.push(Stmt::Eval(v));
            }
            (Go, _) if kind.ends_with("expression") => {
                let v = self.val(n, fi);
                self.fnm(fi).stmts.push(Stmt::Eval(v));
            }
            _ => {
                for c in kids(n) {
                    self.stmt(c, fi);
                }
            }
        }
    }

    fn rust_expr_stmt(&mut self, c: Node, fi: usize) {
        match c.kind() {
            "assignment_expression" | "compound_assignment_expr" | "return_expression" | "for_expression" => self.stmt(c, fi),
            _ => {
                let v = self.val(c, fi);
                self.fnm(fi).stmts.push(Stmt::Eval(v));
            }
        }
    }

    /// A Rust block's statements are lowered; its value is its tail
    /// expression's.
    fn block_val(&mut self, b: Node, fi: usize) -> Val {
        if b.kind() != "block" {
            return self.val(b, fi);
        }
        let items = kids(b);
        let mut v = Val::default();
        for (i, it) in items.iter().enumerate() {
            let last = i + 1 == items.len();
            if last && is_rust_expr(it.kind()) && it.kind() != "expression_statement" {
                v = self.val(*it, fi);
            } else {
                self.stmt(*it, fi);
            }
        }
        v
    }

    fn decl_place(&self, name: String, fi: usize) -> Place {
        if self.file.fns[fi].is_module && self.ret_redirect.is_empty() {
            Place::Name(name)
        } else {
            Place::Local(name)
        }
    }

    /// Assignment targets. `py_decl`: a plain name assignment binds a local
    /// (Python), unless declared `global`.
    fn places(&mut self, l: Node, fi: usize, py_decl: bool) -> Vec<Place> {
        match l.kind() {
            "identifier" => {
                let name = self.text(l).to_string();
                if self.declared_globals.contains(&name) || !py_decl {
                    vec![Place::Name(name)]
                } else {
                    vec![self.decl_place(name, fi)]
                }
            }
            "attribute" | "member_expression" | "selector_expression" | "field_expression" | "subscript" | "subscript_expression" | "index_expression" => {
                // the index expression's own calls still run
                for f in ["subscript", "index"] {
                    if let Some(ix) = l.child_by_field_name(f) {
                        let v = self.val(ix, fi);
                        self.fnm(fi).stmts.push(Stmt::Eval(v));
                    }
                }
                match self.object_base(l) {
                    Some((base, at)) => vec![Place::Attr { base, chain: self.text(l).to_string(), at }],
                    None => {
                        let v = self.val(l, fi);
                        self.fnm(fi).stmts.push(Stmt::Eval(v));
                        vec![Place::Unknown]
                    }
                }
            }
            "pattern_list" | "tuple_pattern" | "list_pattern" | "expression_list" | "tuple" | "list" | "array_pattern" | "object_pattern" | "parenthesized_expression" | "list_splat_pattern" => {
                let mut out = Vec::new();
                for k in kids(l) {
                    out.extend(self.places(k, fi, py_decl));
                }
                out
            }
            "unary_expression" | "pointer_expression" | "dereference_expression" => {
                // `*p = v` writes through p
                let mut out = Vec::new();
                for k in kids(l) {
                    out.extend(self.places(k, fi, false));
                }
                out
            }
            "self" | "this" => vec![Place::Name(self.text(l).to_string())],
            _ => {
                let names = self.binders(l);
                if names.is_empty() {
                    vec![Place::Unknown]
                } else {
                    names.into_iter().map(|n| if py_decl { self.decl_place(n, fi) } else { Place::Name(n) }).collect()
                }
            }
        }
    }

    /// The first identifier of an access chain through attributes / fields
    /// / indexing (`a` in `a.b[c].d`), with its offset.
    fn object_base(&self, n: Node) -> Option<(String, u32)> {
        match n.kind() {
            "identifier" | "self" | "this" | "shorthand_property_identifier" => {
                Some((self.text(n).to_string(), n.start_byte() as u32))
            }
            "attribute" | "member_expression" | "selector_expression" | "field_expression" | "subscript" | "subscript_expression" | "index_expression" | "parenthesized_expression" | "non_null_expression" => {
                let obj = ["object", "value", "operand"]
                    .iter()
                    .find_map(|f| n.child_by_field_name(f))
                    .or_else(|| kids(n).into_iter().next())?;
                self.object_base(obj)
            }
            _ => None,
        }
    }

    /// `(base, chain, offset of the last segment)` if `n` is a plain name
    /// or dotted/path access chain (`os.environ`, `std::fs::read`).
    fn chain(&self, n: Node) -> Option<(String, String, u32)> {
        match n.kind() {
            "identifier" | "self" | "this" | "super" | "crate" | "type_identifier" | "shorthand_property_identifier" => {
                let t = self.text(n).to_string();
                Some((t.clone(), t, n.start_byte() as u32))
            }
            "attribute" => {
                let (b, c, _) = self.chain(n.child_by_field_name("object")?)?;
                let a = n.child_by_field_name("attribute")?;
                Some((b, format!("{c}.{}", self.text(a)), a.start_byte() as u32))
            }
            "member_expression" => {
                let (b, c, _) = self.chain(n.child_by_field_name("object")?)?;
                let a = n.child_by_field_name("property")?;
                Some((b, format!("{c}.{}", self.text(a)), a.start_byte() as u32))
            }
            "selector_expression" => {
                let (b, c, _) = self.chain(n.child_by_field_name("operand")?)?;
                let a = n.child_by_field_name("field")?;
                Some((b, format!("{c}.{}", self.text(a)), a.start_byte() as u32))
            }
            "field_expression" => {
                let (b, c, _) = self.chain(n.child_by_field_name("value")?)?;
                let a = n.child_by_field_name("field")?;
                Some((b, format!("{c}.{}", self.text(a)), a.start_byte() as u32))
            }
            "scoped_identifier" => {
                let a = n.child_by_field_name("name")?;
                match n.child_by_field_name("path") {
                    Some(p) => {
                        let (b, c, _) = self.chain(p)?;
                        Some((b, format!("{c}::{}", self.text(a)), a.start_byte() as u32))
                    }
                    None => Some((self.text(a).to_string(), self.text(a).to_string(), a.start_byte() as u32)),
                }
            }
            "generic_function" | "generic_type" => self.chain(n.child_by_field_name("function").or_else(|| n.child_by_field_name("type"))?),
            "parenthesized_expression" | "non_null_expression" => {
                let inner = kids(n);
                if inner.len() == 1 {
                    self.chain(inner[0])
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // ---------------------------------------------------------------- values

    fn read(&self, n: Node) -> Option<Read> {
        let (base, chain, at) = self.chain(n)?;
        Some(Read { base, chain, at, row: row(n) })
    }

    /// The inputs of expression `n`; calls inside are lowered on the way.
    fn val(&mut self, n: Node, fi: usize) -> Val {
        use Lang::*;
        let kind = n.kind();
        let mut v = Val::default();
        match (self.lang, kind) {
            (_, "identifier" | "self" | "this" | "shorthand_property_identifier") => {
                if let Some(r) = self.read(n) {
                    v.reads.push(r);
                }
            }
            (_, "attribute" | "member_expression" | "selector_expression" | "field_expression" | "scoped_identifier") => {
                match self.read(n) {
                    Some(r) => v.reads.push(r),
                    None => {
                        for f in ["object", "value", "operand"] {
                            if let Some(o) = n.child_by_field_name(f) {
                                v.extend(self.val(o, fi));
                            }
                        }
                    }
                }
                if self.lang == Ts {
                    if let Some(p) = n.child_by_field_name("property") {
                        if p.kind() != "property_identifier" && p.kind() != "private_property_identifier" {
                            v.extend(self.val(p, fi));
                        }
                    }
                }
            }
            (Ts, "subscript_expression") => {
                // obj[key]: a computed member read
                if let Some(o) = n.child_by_field_name("object") {
                    v.extend(self.val(o, fi));
                }
                if let Some(i) = n.child_by_field_name("index") {
                    v.extend(self.val(i, fi));
                }
            }
            (Python, "call") | (Ts, "call_expression" | "new_expression") | (Go, "call_expression") | (Rust, "call_expression" | "macro_invocation") => {
                let c = self.call(n, fi);
                v.calls.push(c);
            }
            (_, k) if is_lambda(self.lang, k) => v = self.lambda(n, fi),
            (Python, "named_expression") => {
                let val = n.child_by_field_name("value").map(|x| self.val(x, fi)).unwrap_or_default();
                if let Some(nm) = n.child_by_field_name("name") {
                    let name = self.text(nm).to_string();
                    let p = self.decl_place(name, fi);
                    self.fnm(fi).stmts.push(Stmt::Assign { to: vec![p], from: val.clone(), row: row(n) });
                }
                v = val;
            }
            (Python, "keyword_argument") | (Ts, "pair") => {
                if let Some(x) = n.child_by_field_name("value") {
                    v = self.val(x, fi);
                }
                if let Some(k) = n.child_by_field_name("key").filter(|k| k.kind() == "computed_property_name") {
                    v.extend(self.val(k, fi));
                }
            }
            (Python, "for_in_clause") => {
                let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) else {
                    return v;
                };
                let val = self.val(r, fi);
                let to = self.places(l, fi, true);
                self.fnm(fi).stmts.push(Stmt::Assign { to, from: val, row: row(n) });
            }
            (Python, "list_comprehension" | "set_comprehension" | "generator_expression" | "dictionary_comprehension") => {
                // bindings first, then the element
                for c in kids(n) {
                    if c.kind() == "for_in_clause" {
                        let _ = self.val(c, fi);
                    }
                }
                for c in kids(n) {
                    if c.kind() != "for_in_clause" {
                        v.extend(self.val(c, fi));
                    }
                }
            }
            (Rust, "block") => v = self.block_val(n, fi),
            (Rust, "if_expression") => {
                for c in kids(n) {
                    match c.kind() {
                        "block" => v.extend(self.block_val(c, fi)),
                        "else_clause" => {
                            for e in kids(c) {
                                v.extend(self.val(e, fi));
                            }
                        }
                        "let_condition" => self.rust_let_cond(c, fi),
                        "let_chain" => {
                            for lc in kids(c) {
                                if lc.kind() == "let_condition" {
                                    self.rust_let_cond(lc, fi);
                                } else {
                                    let e = self.val(lc, fi);
                                    self.fnm(fi).stmts.push(Stmt::Eval(e));
                                }
                            }
                        }
                        _ => {
                            let e = self.val(c, fi);
                            self.fnm(fi).stmts.push(Stmt::Eval(e));
                        }
                    }
                }
            }
            (Rust, "match_expression") => {
                let scrut = n.child_by_field_name("value").map(|x| self.val(x, fi)).unwrap_or_default();
                if let Some(body) = n.child_by_field_name("body") {
                    for arm in kids(body) {
                        if arm.kind() != "match_arm" {
                            continue;
                        }
                        if let Some(p) = arm.child_by_field_name("pattern") {
                            let to = self.binders(p).into_iter().map(|b| self.decl_place(b, fi)).collect();
                            self.fnm(fi).stmts.push(Stmt::Assign { to, from: scrut.clone(), row: row(arm) });
                        }
                        if let Some(val) = arm.child_by_field_name("value") {
                            v.extend(self.block_val(val, fi));
                        }
                    }
                }
            }
            (Rust, "while_expression" | "loop_expression") => {
                for c in kids(n) {
                    match c.kind() {
                        "block" => {
                            let _ = self.block_val(c, fi);
                        }
                        "let_condition" => self.rust_let_cond(c, fi),
                        _ => {
                            let e = self.val(c, fi);
                            self.fnm(fi).stmts.push(Stmt::Eval(e));
                        }
                    }
                }
            }
            (Rust, "for_expression" | "return_expression" | "assignment_expression" | "compound_assignment_expr") => {
                self.stmt(n, fi);
            }
            (Rust, "let_condition") => self.rust_let_cond(n, fi),
            (Rust, "struct_expression") => {
                if let Some(b) = n.child_by_field_name("body") {
                    for f in kids(b) {
                        match f.kind() {
                            "field_initializer" => {
                                if let Some(x) = f.child_by_field_name("value") {
                                    v.extend(self.val(x, fi));
                                }
                            }
                            _ => v.extend(self.val(f, fi)),
                        }
                    }
                }
            }
            (Ts, "assignment_expression" | "augmented_assignment_expression") | (Python, "assignment") => {
                self.stmt(n, fi);
                if let Some(r) = n.child_by_field_name("right") {
                    // value of an assignment expression: re-read the target
                    if let Some(l) = n.child_by_field_name("left") {
                        v.extend(self.val_no_calls(l));
                    }
                    let _ = r;
                }
            }
            // literal leaves and type syntax carry no data inputs
            (_, k) if is_inert(k) => {}
            _ => {
                for c in kids(n) {
                    v.extend(self.val(c, fi));
                }
            }
        }
        v
    }

    /// Reads of an already-lowered target (no call is lowered twice).
    fn val_no_calls(&self, n: Node) -> Val {
        let mut v = Val::default();
        if let Some((base, at)) = self.object_base(n) {
            v.reads.push(Read { chain: base.clone(), base, at, row: row(n) });
        }
        v
    }

    fn rust_let_cond(&mut self, c: Node, fi: usize) {
        let (Some(p), Some(x)) = (c.child_by_field_name("pattern"), c.child_by_field_name("value")) else {
            return;
        };
        let val = self.val(x, fi);
        let to = self.binders(p).into_iter().map(|b| self.decl_place(b, fi)).collect();
        self.fnm(fi).stmts.push(Stmt::Assign { to, from: val, row: row(c) });
    }

    /// Inline a closure / nested function into `fi`: its parameters become
    /// locals, its body is lowered here, and its returns are its value.
    fn lambda(&mut self, n: Node, fi: usize) -> Val {
        let params = self.params(n);
        for p in &params {
            for nm in p.name.split(',') {
                let place = self.decl_place(nm.to_string(), fi);
                self.fnm(fi).stmts.push(Stmt::Assign { to: vec![place], from: Val::default(), row: row(n) });
            }
        }
        self.ret_redirect.push(Val::default());
        if let Some(b) = n.child_by_field_name("body") {
            self.body(b, fi);
        }
        self.ret_redirect.pop().unwrap_or_default()
    }

    fn lambda_params(&mut self, n: Node) -> Vec<String> {
        self.params(n).into_iter().flat_map(|p| p.name.split(',').map(str::to_string).collect::<Vec<_>>()).collect()
    }

    fn call(&mut self, n: Node, fi: usize) -> u32 {
        use Lang::*;
        let r = row(n);
        // Rust macros: `name!(tokens)`: a call of `name!` with every
        // identifier in the token tree as input.
        if n.kind() == "macro_invocation" {
            let name = n.child_by_field_name("macro").map(|m| self.text(m).to_string()).unwrap_or_default();
            let at = n.start_byte() as u32;
            let mut arg = Val::default();
            for c in kids(n) {
                if c.kind() == "token_tree" {
                    self.token_idents(c, &mut arg);
                }
            }
            let base = name.split("::").next().unwrap_or("").to_string();
            let f = self.fnm(fi);
            f.calls.push(Call {
                callee: Callee::Path { chain: format!("{name}!"), base: format!("{base}!") },
                args: vec![arg],
                kwargs: Vec::new(),
                splat: false,
                callbacks: Vec::new(),
                construct: false,
                at,
                row: r,
            });
            return (f.calls.len() - 1) as u32;
        }
        let construct = n.kind() == "new_expression";
        let fnode = n.child_by_field_name(if construct { "constructor" } else { "function" });
        let (callee, at) = match fnode {
            Some(f) => match self.chain(f) {
                Some((base, chain, at)) => (Callee::Path { chain, base }, at),
                None => {
                    let (obj, name) = match (self.lang, f.kind()) {
                        (Python, "attribute") => (f.child_by_field_name("object"), f.child_by_field_name("attribute")),
                        (Ts, "member_expression") => (f.child_by_field_name("object"), f.child_by_field_name("property")),
                        (Go, "selector_expression") => (f.child_by_field_name("operand"), f.child_by_field_name("field")),
                        (Rust, "field_expression") => (f.child_by_field_name("value"), f.child_by_field_name("field")),
                        _ => (None, None),
                    };
                    match (obj, name) {
                        (Some(o), Some(nm)) => {
                            let recv = self.val(o, fi);
                            (
                                Callee::Method { recv, recv_chain: None, name: self.text(nm).to_string() },
                                nm.start_byte() as u32,
                            )
                        }
                        _ => {
                            // `fns[i](x)`, `(a || b)(x)`, `f()(x)`: the value called is unknown
                            let _ = self.val(f, fi);
                            let what = self.text(f).chars().take(60).collect::<String>();
                            self.fnm(fi).dynamic.push(Dynamic { row: r, what: format!("call of a computed value `{what}`") });
                            (Callee::Dynamic, f.start_byte() as u32)
                        }
                    }
                }
            },
            None => (Callee::Dynamic, n.start_byte() as u32),
        };
        let mut args = Vec::new();
        let mut kwargs = Vec::new();
        let mut splat = false;
        let mut callbacks = Vec::new();
        if let Some(al) = n.child_by_field_name("arguments") {
            for a in kids(al) {
                match a.kind() {
                    "keyword_argument" => {
                        let name = a.child_by_field_name("name").map(|x| self.text(x).to_string()).unwrap_or_default();
                        let val = self.val(a, fi);
                        kwargs.push((name, val));
                    }
                    "list_splat" | "dictionary_splat" | "spread_element" | "variadic_argument" => {
                        splat = true;
                        let val = self.val(a, fi);
                        args.push(val);
                    }
                    "comment" => {}
                    k => {
                        if is_lambda(self.lang, k) {
                            let ps = self.lambda_params(a);
                            callbacks.push((args.len() as u32, ps));
                        }
                        let val = self.val(a, fi);
                        args.push(val);
                    }
                }
            }
            if self.lang == Go && self.text(al).trim_end_matches(')').trim_end().ends_with("...") {
                splat = true;
            }
        }
        // Python decorators-as-calls and JS tagged templates have no argument list.
        let f = self.fnm(fi);
        f.calls.push(Call { callee, args, kwargs, splat, callbacks, construct, at, row: r });
        (f.calls.len() - 1) as u32
    }

    fn token_idents(&self, n: Node, v: &mut Val) {
        for c in kids(n) {
            match c.kind() {
                "identifier" | "self" => v.reads.push(Read {
                    base: self.text(c).to_string(),
                    chain: self.text(c).to_string(),
                    at: c.start_byte() as u32,
                    row: row(c),
                }),
                "token_tree" => self.token_idents(c, v),
                _ => {}
            }
        }
    }
}

fn is_lambda(lang: Lang, k: &str) -> bool {
    match lang {
        Lang::Python => k == "lambda",
        Lang::Ts => matches!(k, "arrow_function" | "function_expression" | "function" | "generator_function"),
        Lang::Go => k == "func_literal",
        Lang::Rust => k == "closure_expression",
    }
}

fn is_inert(k: &str) -> bool {
    matches!(
        k,
        "string_content" | "escape_sequence" | "integer" | "float" | "true" | "false" | "none" | "null" | "undefined"
            | "number" | "integer_literal" | "float_literal" | "boolean_literal" | "char_literal" | "raw_string_literal"
            | "interpreted_string_literal" | "rune_literal" | "int_literal" | "float_literal_go" | "nil" | "iota"
            | "type" | "type_annotation" | "type_identifier" | "type_arguments" | "primitive_type" | "generic_type"
            | "predefined_type" | "comment" | "line_comment" | "block_comment" | "regex" | "property_identifier"
            | "field_identifier" | "string_fragment" | "keyword_identifier" | "type_parameters" | "as" | "satisfies_expression_type"
            | "string_start" | "string_end" | "lifetime" | "attribute_item" | "decorator"
    )
}

fn is_py_expr(k: &str) -> bool {
    matches!(
        k,
        "call" | "attribute" | "identifier" | "subscript" | "binary_operator" | "boolean_operator" | "comparison_operator"
            | "not_operator" | "unary_operator" | "conditional_expression" | "await" | "parenthesized_expression"
            | "named_expression" | "list" | "tuple" | "dictionary" | "set" | "string" | "concatenated_string"
            | "list_comprehension" | "generator_expression" | "dictionary_comprehension" | "set_comprehension" | "lambda"
    )
}

fn is_ts_stmt(k: &str) -> bool {
    k.ends_with("statement") || k.ends_with("declaration") || matches!(k, "statement_block" | "else_clause" | "switch_body" | "switch_case" | "switch_default" | "catch_clause" | "finally_clause" | "variable_declarator")
}

fn is_go_stmt(k: &str) -> bool {
    k.ends_with("statement") || k.ends_with("declaration") || matches!(k, "block" | "statement_list" | "for_clause" | "range_clause" | "expression_case" | "communication_case" | "type_case" | "default_case")
}

fn is_rust_expr(k: &str) -> bool {
    k.ends_with("expression") || k.ends_with("_literal") || matches!(k, "identifier" | "macro_invocation" | "scoped_identifier" | "self" | "unit_expression" | "block" | "try_expression" | "await_expression")
}

// ---------------------------------------------------------------- complexity

/// `(cyclomatic, cognitive)` of a function body. Cyclomatic is McCabe's
/// decisions + 1. Cognitive follows the published rules in simplified
/// form: +1 per break in linear flow (if, loop, catch, switch, ternary),
/// plus the nesting depth at which it occurs; +1 per `else`/`elif`; +1 per
/// run of boolean operators. `module`: stop at nested function definitions
/// (they are measured on their own).
pub fn complexity(lang: Lang, body: Node, module: bool) -> (u32, u32) {
    let mut cyc = 1;
    let mut cog = 0;
    walk_cx(lang, body, 0, module, &mut cyc, &mut cog, None);
    (cyc, cog)
}

fn walk_cx(lang: Lang, n: Node, nest: u32, module: bool, cyc: &mut u32, cog: &mut u32, parent_op: Option<&str>) {
    use Lang::*;
    let k = n.kind();
    let named_fn = matches!(
        (lang, k),
        (Python, "function_definition" | "class_definition") | (Ts, "function_declaration" | "method_definition" | "class_declaration") | (Go, "function_declaration" | "method_declaration") | (Rust, "function_item" | "impl_item" | "trait_item" | "mod_item")
    );
    if module && named_fn {
        return;
    }
    let structural = matches!(
        (lang, k),
        (Python, "if_statement" | "for_statement" | "while_statement" | "except_clause" | "conditional_expression" | "match_statement")
            | (Ts, "if_statement" | "for_statement" | "for_in_statement" | "while_statement" | "do_statement" | "catch_clause" | "ternary_expression" | "switch_statement")
            | (Go, "if_statement" | "for_statement" | "expression_switch_statement" | "type_switch_statement" | "select_statement")
            | (Rust, "if_expression" | "while_expression" | "loop_expression" | "for_expression" | "match_expression")
    );
    let decision = matches!(
        (lang, k),
        (Python, "if_statement" | "elif_clause" | "for_statement" | "while_statement" | "except_clause" | "conditional_expression" | "case_clause" | "for_in_clause" | "if_clause")
            | (Ts, "if_statement" | "for_statement" | "for_in_statement" | "while_statement" | "do_statement" | "catch_clause" | "ternary_expression" | "switch_case")
            | (Go, "if_statement" | "for_statement" | "expression_case" | "type_case" | "communication_case")
            | (Rust, "if_expression" | "while_expression" | "loop_expression" | "for_expression" | "match_arm")
    );
    let else_like = matches!((lang, k), (Python, "elif_clause" | "else_clause") | (Ts | Rust, "else_clause"));
    // boolean operator runs
    let op = bool_op(lang, n);
    if decision {
        *cyc += 1;
    }
    if let Some(o) = op {
        *cyc += 1;
        if parent_op != Some(o) {
            *cog += 1;
        }
    }
    if structural {
        *cog += 1 + nest;
    } else if else_like {
        *cog += 1;
    }
    // Go `else` is the `alternative` field of an if_statement
    if lang == Go && k == "if_statement" && n.child_by_field_name("alternative").is_some_and(|a| a.kind() == "block") {
        *cog += 1;
    }
    let inner = if structural || is_lambda(lang, k) { nest + 1 } else { nest };
    for c in kids(n) {
        // an else-if chain does not nest deeper
        let child_nest = if (else_like || (lang == Go && k == "if_statement")) && matches!(c.kind(), "if_statement" | "if_expression") {
            nest
        } else {
            inner
        };
        walk_cx(lang, c, child_nest, module, cyc, cog, op.or(if matches!(k, "parenthesized_expression") { parent_op } else { None }));
    }
}

fn bool_op(lang: Lang, n: Node) -> Option<&'static str> {
    match (lang, n.kind()) {
        (Lang::Python, "boolean_operator") => {
            let op = n.child_by_field_name("operator")?;
            Some(if op.kind() == "and" { "and" } else { "or" })
        }
        (_, "binary_expression") => {
            let op = n.child_by_field_name("operator")?;
            match op.kind() {
                "&&" => Some("&&"),
                "||" => Some("||"),
                "??" => Some("??"),
                _ => None,
            }
        }
        _ => None,
    }
}
