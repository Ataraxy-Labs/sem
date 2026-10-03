//! Every module reference a JS/TS file makes, classified as runtime (value)
//! or compile-time-only (type), read with oxc.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, ExportAllDeclaration, ExportFromDeclaration, Expression,
    ImportDeclaration, ImportDeclarationSpecifier, ImportExpression, NewExpression, TSImportEqualsDeclaration,
    TSImportType, TSModuleReference,
};
use oxc_ast_visit::{walk, Visit};
use oxc_span::SourceType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    /// A runtime reference: the module is loaded when this one runs.
    Value,
    /// Erased at compile time (`import type`, `export type`, `import("x").T`).
    Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Form {
    Import,
    SideEffect,
    ReExport,
    ReExportAll,
    Dynamic,
    Require,
    TypeQuery,
    ImportEquals,
    /// `new Worker("./w")`, `new URL("./x", import.meta.url)`: a module loaded at runtime by URL.
    Worker,
    /// ``import(`./locales/${code}.json`)``: a relative dynamic import whose
    /// specifier is a template; the specifier is recorded as a pattern with
    /// `*` for each substitution (bundler dynamic-import-vars convention: a
    /// `*` never crosses a `/`).
    DynamicPattern,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModuleRef {
    pub specifier: String,
    pub kind: RefKind,
    pub form: Form,
    pub line: u32,
}

pub fn source_type_for(path: &str) -> Option<SourceType> {
    let p = path.to_ascii_lowercase();
    if p.ends_with(".d.ts") || p.ends_with(".ts") || p.ends_with(".mts") || p.ends_with(".cts") {
        Some(SourceType::ts())
    } else if p.ends_with(".tsx") {
        Some(SourceType::tsx())
    } else if p.ends_with(".jsx") {
        Some(SourceType::jsx())
    } else if p.ends_with(".mjs") || p.ends_with(".js") {
        Some(SourceType::mjs())
    } else if p.ends_with(".cjs") {
        Some(SourceType::cjs())
    } else {
        None
    }
}

/// All module references in `content`. Parse errors still yield whatever the
/// recovering parser produced; the caller learns about them via `errors`.
pub fn module_refs(path: &str, content: &str) -> (Vec<ModuleRef>, usize) {
    let Some(st) = source_type_for(path) else { return (Vec::new(), 0) };
    let allocator = Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, content, st).parse();
    let mut v = Collector { refs: Vec::new(), line_starts: line_starts(content) };
    v.visit_program(&parsed.program);
    (v.refs, parsed.diagnostics.len())
}

fn line_starts(s: &str) -> Vec<u32> {
    let mut v = vec![0u32];
    v.extend(s.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i as u32 + 1));
    v
}

struct Collector {
    refs: Vec<ModuleRef>,
    line_starts: Vec<u32>,
}

impl Collector {
    fn push(&mut self, specifier: &str, kind: RefKind, form: Form, at: u32) {
        let line = self.line_starts.partition_point(|&s| s <= at) as u32;
        self.refs.push(ModuleRef { specifier: specifier.to_string(), kind, form, line });
    }
}

fn is_import_meta_url(a: &Argument) -> bool {
    matches!(a, Argument::StaticMemberExpression(m) if m.property.name == "url" && matches!(m.object, Expression::ImportMeta(_)))
}

fn kind(is_type: bool) -> RefKind {
    if is_type { RefKind::Type } else { RefKind::Value }
}

impl<'a> Visit<'a> for Collector {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        let form = if it.specifiers.is_none() { Form::SideEffect } else { Form::Import };
        // `import type {..}` or `import { type A, type B }` (every specifier type-only) is erased;
        // `import {}` and side-effect imports still load the module.
        let all_type = it.import_kind.is_type()
            || it.specifiers.as_ref().is_some_and(|s| {
                !s.is_empty()
                    && s.iter().all(|sp| matches!(sp, ImportDeclarationSpecifier::ImportSpecifier(x) if x.import_kind.is_type()))
            });
        self.push(it.source.value.as_str(), kind(all_type), form, it.span.start);
    }

    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        let all_type = it.export_kind.is_type()
            || (!it.specifiers.is_empty() && it.specifiers.iter().all(|s| s.export_kind.is_type()));
        self.push(it.source.value.as_str(), kind(all_type), Form::ReExport, it.span.start);
        walk::walk_export_from_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.push(it.source.value.as_str(), kind(it.export_kind.is_type()), Form::ReExportAll, it.span.start);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        match &it.source {
            Expression::StringLiteral(s) => self.push(s.value.as_str(), RefKind::Value, Form::Dynamic, it.span.start),
            Expression::TemplateLiteral(t) if !t.expressions.is_empty() => {
                let pattern = t.quasis.iter().map(|q| q.value.raw.as_str()).collect::<Vec<_>>().join("*");
                if pattern.starts_with("./") || pattern.starts_with("../") {
                    self.push(&pattern, RefKind::Value, Form::DynamicPattern, it.span.start);
                }
            }
            _ => {}
        }
        walk::walk_import_expression(self, it);
    }

    fn visit_ts_import_type(&mut self, it: &TSImportType<'a>) {
        self.push(it.source.value.as_str(), RefKind::Type, Form::TypeQuery, it.span.start);
        walk::walk_ts_import_type(self, it);
    }

    fn visit_ts_import_equals_declaration(&mut self, it: &TSImportEqualsDeclaration<'a>) {
        if let TSModuleReference::ExternalModuleReference(r) = &it.module_reference {
            self.push(r.expression.value.as_str(), kind(it.import_kind.is_type()), Form::ImportEquals, it.span.start);
        }
        walk::walk_ts_import_equals_declaration(self, it);
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        if let Expression::Identifier(id) = &it.callee {
            let first = it.arguments.first();
            match (id.name.as_str(), first) {
                ("Worker" | "SharedWorker", Some(Argument::StringLiteral(s))) => {
                    self.push(s.value.as_str(), RefKind::Value, Form::Worker, it.span.start)
                }
                ("URL", Some(Argument::StringLiteral(s))) if it.arguments.len() == 2 && is_import_meta_url(&it.arguments[1]) => {
                    self.push(s.value.as_str(), RefKind::Value, Form::Worker, it.span.start)
                }
                _ => {}
            }
        }
        walk::walk_new_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Expression::Identifier(id) = &it.callee {
            if id.name == "require" && it.arguments.len() == 1 {
                if let Some(Argument::StringLiteral(s)) = it.arguments.first() {
                    self.push(s.value.as_str(), RefKind::Value, Form::Require, it.span.start);
                }
            }
        }
        walk::walk_call_expression(self, it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(src: &str) -> Vec<(String, RefKind, Form)> {
        module_refs("a.ts", src).0.into_iter().map(|r| (r.specifier, r.kind, r.form)).collect()
    }

    #[test]
    fn value_and_type_forms() {
        let r = refs(
            r#"import a from "./a";
import type { B } from "./b";
import { type C, d } from "./c";
import { type E } from "./e";
import "./side";
export { f } from "./f";
export type { G } from "./g";
export * from "./h";
export type * from "./i";
const j = await import("./j");
type K = import("./k").K;
const l = require("./l");
const w = new Worker("./w.ts");
const u = new URL("./u.ts", import.meta.url);
const v = new Worker(someVar);
const x = await import(`./locales/${code}.json`);
const y = await import(`${base}/x.js`);
"#,
        );
        use Form::*;
        use RefKind::*;
        assert_eq!(
            r,
            vec![
                ("./a".into(), Value, Import),
                ("./b".into(), Type, Import),
                ("./c".into(), Value, Import),
                ("./e".into(), Type, Import),
                ("./side".into(), Value, SideEffect),
                ("./f".into(), Value, ReExport),
                ("./g".into(), Type, ReExport),
                ("./h".into(), Value, ReExportAll),
                ("./i".into(), Type, ReExportAll),
                ("./j".into(), Value, Dynamic),
                ("./k".into(), Type, TypeQuery),
                ("./l".into(), Value, Require),
                ("./w.ts".into(), Value, Worker),
                ("./u.ts".into(), Value, Worker),
                ("./locales/*.json".into(), Value, DynamicPattern),
            ]
        );
    }
}
