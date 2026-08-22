//! An [`oxc`](https://oxc.rs)-backed [`FastExtractor`] for JS/TS.
//!
//! # Scope, and why it declines so freely
//!
//! `entity_extractor.rs`'s JS/TS rules are ~3,700 lines of tree-sitter
//! node-kind matching. This extractor does not reproduce all of them, and
//! does not try to. It models a named subset of constructs exactly, and
//! **declines the whole file** the moment it meets anything outside that
//! subset — a construct it does not model, a parse error, a duplicate entity
//! id it would have to disambiguate.
//!
//! Declining a file is free and safe (the caller falls back to tree-sitter),
//! and it is what makes the coverage number meaningful: on the files this
//! extractor *does* serve, it must be exactly right, and
//! [`crate::parser::diff_oracle`] decides whether it is. A partial answer on
//! every file would be far worse than an exact answer on some files, because
//! there would be no subset anyone could trust.
//!
//! # Identity: kappa here is parser-scoped, and that is a correction
//!
//! kappa is documented as "a parser-independent semantic identity",
//! spec'd so another parser's typed AST could reproduce it. Reimplementing it
//! here is that spec's first test, and it fails: the spec hashes
//! tree-sitter's `kind()` *strings* for internal
//! named nodes, so reproducing a value requires reproducing
//! tree-sitter-typescript's grammar shape node for node, which is the CST
//! fidelity already proved unreachable. The empirical
//! proof is cheap: the same four bytes of source at `.ts`, `.tsx`, `.js` and
//! `.jsx` yield **two** distinct kappa values today, because the TypeScript
//! and JavaScript grammars wrap parameters differently. A hash that changes
//! when the grammar changes is not parser-independent.
//!
//! So kappa here is computed by kappa's *canonicalization rule* — drop
//! comments, whitespace, and the punctuation a formatter owns — applied to a
//! token stream rather than to a CST, and is deliberately **parser-scoped**:
//! κ_oxc ≠ κ_treesitter for the same code, and that is fine precisely because
//! [`FastExtractor::identity`] is folded into every cache and facts key the
//! value can reach, so the two never meet. What must hold, and what the
//! oracle checks, is that the two agree on the *partition* — which entities
//! are semantically the same as which — not on the hash values.
//!
//! `structural_hash` gets the same token stream with the entity's own name
//! token removed, preserving the rename-insensitivity `model/identity.rs`'s
//! phase-2 match depends on.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrowFunctionExpression, CallExpression, Class, ClassElement, ExportAllDeclaration,
    ExportFromDeclaration, ExportNamedDeclaration, Expression, Function, FunctionBody,
    FunctionType, MethodDefinitionKind, ObjectProperty, PropertyKey, PropertyKind,
    TSEnumDeclaration, TSInterfaceDeclaration, TSModuleDeclaration, TSSignature,
    TSTypeAliasDeclaration, TSTypeLiteral, VariableDeclaration, VariableDeclarationKind,
};
use oxc_ast_visit::{walk, Visit};
use oxc_span::{SourceType, Span};
use oxc_syntax::scope::ScopeFlags;

use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::fast_extractor::{is_js_ts_path, FastExtractor};
use crate::utils::hash::content_hash;

/// Bump whenever this extractor's observable output can change.
const IDENTITY: &str = "oxc-0.143.0-r1";

pub struct OxcExtractor;

impl OxcExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OxcExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl FastExtractor for OxcExtractor {
    fn identity(&self) -> &str {
        IDENTITY
    }

    fn claims(&self, file_path: &str) -> bool {
        is_js_ts_path(file_path)
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<Vec<SemanticEntity>> {
        let source_type = source_type_for(file_path)?;
        let allocator = Allocator::default();
        let parsed = oxc_parser::Parser::new(&allocator, content, source_type).parse();
        // Any diagnostic at all is a decline. oxc recovers from many errors
        // and still yields a tree; a recovered tree is exactly the situation
        // where two extractors quietly disagree.
        if parsed.panicked || !parsed.diagnostics.is_empty() {
            return None;
        }
        let mut collector = Collector::new(file_path, content);
        collector.visit_program(&parsed.program);
        collector.finish()
    }
}

fn source_type_for(file_path: &str) -> Option<SourceType> {
    let lower = file_path.to_ascii_lowercase();
    if lower.ends_with(".ts") || lower.ends_with(".mts") || lower.ends_with(".cts") {
        Some(SourceType::ts())
    } else if lower.ends_with(".tsx") {
        Some(SourceType::tsx())
    } else if lower.ends_with(".jsx") {
        Some(SourceType::jsx())
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs") {
        Some(SourceType::mjs())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The canonical token stream (kappa and its rename-insensitive sibling)
// ---------------------------------------------------------------------------

/// Hash an entity's source slice as a canonical token stream.
///
/// This is canonicalization discipline applied to tokens instead
/// of to a CST: comments and whitespace are dropped because they are not
/// semantics; `;` and `,` are dropped because they are the two punctuation
/// marks a formatter (or ASI) legitimately adds and removes without changing
/// meaning — the exact churn kappa exists to be invariant to. Everything else
/// is kept, including brackets and braces (they carry grouping that dropping
/// would false-merge, e.g. `(a + b) * c` vs `a + b * c`) and including
/// keywords (strictly more discriminating than the CST rule, which drops them
/// and needs a curated per-node-kind table to claw the meaningful ones back).
///
/// `exclude` removes one byte range — the entity's own name token — which is
/// how the structural sibling stays rename-insensitive.
fn canonical_token_hash(source: &str, span: Span, exclude: Option<Span>) -> String {
    use xxhash_rust::xxh3::Xxh3;
    let mut hasher = Xxh3::new();
    let start = span.start as usize;
    let end = (span.end as usize).min(source.len());
    if start >= end {
        return format!("{:016x}", hasher.digest());
    }
    let bytes = source.as_bytes();
    let mut i = start;
    while i < end {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Comments
        if b == b'/' && i + 1 < end {
            if bytes[i + 1] == b'/' {
                while i < end && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < end && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(end);
                continue;
            }
        }
        // String / template literals are single tokens, kept whole.
        if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            let tok_start = i;
            i += 1;
            while i < end {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            emit(&mut hasher, source, tok_start, i.min(end), exclude);
            continue;
        }
        // Identifiers, keywords, numbers.
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80 {
            let tok_start = i;
            while i < end {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 {
                    i += 1;
                } else {
                    break;
                }
            }
            emit(&mut hasher, source, tok_start, i, exclude);
            continue;
        }
        // Formatter-owned punctuation.
        if b == b';' || b == b',' {
            i += 1;
            continue;
        }
        // Everything else is a one-byte symbolic token. Multi-byte operators
        // hash the same as their bytes in sequence, which preserves order and
        // therefore distinguishes `==` from `= =` only by whitespace — a
        // distinction no JS program can express, so it costs nothing.
        emit(&mut hasher, source, i, i + 1, exclude);
        i += 1;
    }
    format!("{:016x}", hasher.digest())
}

fn emit(
    hasher: &mut xxhash_rust::xxh3::Xxh3,
    source: &str,
    start: usize,
    end: usize,
    exclude: Option<Span>,
) {
    if let Some(ex) = exclude {
        if start >= ex.start as usize && end <= ex.end as usize {
            return;
        }
    }
    if let Some(text) = source.get(start..end) {
        hasher.update(text.as_bytes());
        hasher.update(&[0]);
    }
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// The traversal model this extractor reproduces, recovered from
/// `entity_extractor.rs::visit_node` and named here because it is not obvious
/// from the code and every rule below is load-bearing:
///
/// * **Suppression.** `JS_TS_SUPPRESSED_NESTED` drops `lexical_declaration` /
///   `variable_declaration` entities whose *enclosing entity or scope
///   boundary* is a function declaration, a method definition, an arrow
///   function, a function expression or a generator. So a `const` inside any
///   function body is not an entity — but a nested `function`/`class`
///   declaration inside that same body still is.
/// * **Promotion beats suppression.** `promote_js_ts_const_function` re-admits
///   a suppressed declarator whose initializer is a function or arrow, as a
///   `function` entity.
/// * **Restricted initializer descent.** Once a variable or class field *is*
///   an entity, `visit_node` stops its generic child traversal and descends
///   only into the initializer, and only when the initializer is an arrow, a
///   function expression, a generator, a class, or an object literal. So
///   `const x = wrap(function () { function inner() {} })` never yields
///   `inner`, because `wrap(...)` is not an initializer node.
/// * **Object literals.** Under an entity's initializer, both shorthand
///   methods and function-valued properties become `method` entities parented
///   to that entity. Anywhere else, only shorthand methods do.
struct Frame {
    parent: Option<String>,
    /// Whether a variable declaration seen here is suppressed.
    suppress_vars: bool,
    /// Whether an object literal seen here is the initializer of the entity
    /// named by `parent`, which is what promotes function-valued properties
    /// (not just shorthand methods) to entities.
    object_initializer: bool,
}

struct Collector<'s> {
    file_path: &'s str,
    source: &'s str,
    entities: Vec<SemanticEntity>,
    stack: Vec<Frame>,
    unmodelled: Option<&'static str>,
}

impl<'s> Collector<'s> {
    fn new(file_path: &'s str, source: &'s str) -> Self {
        Self {
            file_path,
            source,
            entities: Vec::new(),
            stack: vec![Frame {
                parent: None,
                suppress_vars: false,
                object_initializer: false,
            }],
            unmodelled: None,
        }
    }

    fn finish(self) -> Option<Vec<SemanticEntity>> {
        if let Some(_what) = self.unmodelled {
            return None;
        }
        // `entity_extractor.rs` disambiguates colliding ids (`f@L1#1`) and
        // rewrites children's parent ids to match. Rather than half-reproduce
        // that, decline any file that would need it: a divergence in the one
        // field every downstream map is keyed by is not worth the coverage.
        let mut ids: Vec<&str> = self.entities.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return None;
        }
        Some(self.entities)
    }

    fn unmodelled(&mut self, what: &'static str) {
        if self.unmodelled.is_none() {
            self.unmodelled = Some(what);
        }
    }

    fn frame(&self) -> &Frame {
        self.stack.last().expect("stack is never empty")
    }

    fn line_of(&self, byte: usize) -> usize {
        self.source
            .get(..byte.min(self.source.len()))
            .map(|s| s.bytes().filter(|b| *b == b'\n').count() + 1)
            .unwrap_or(1)
    }

    fn push_entity(&mut self, kind: &str, name: &str, span: Span, name_span: Span) -> String {
        let parent_id = self.frame().parent.clone();
        let id = build_entity_id(self.file_path, kind, name, parent_id.as_deref());
        let start = span.start as usize;
        let end = (span.end as usize).min(self.source.len());
        let content = self.source.get(start..end).unwrap_or("").to_string();
        self.entities.push(SemanticEntity {
            id: id.clone(),
            file_path: self.file_path.to_string(),
            entity_type: kind.to_string(),
            name: name.to_string(),
            parent_id,
            content_hash: content_hash(&content),
            structural_hash: Some(canonical_token_hash(self.source, span, Some(name_span))),
            kappa: Some(canonical_token_hash(self.source, span, None)),
            content,
            start_line: self.line_of(start),
            end_line: self.line_of(end.saturating_sub(1)),
            start_byte: Some(start),
            end_byte: Some(end),
            metadata: None,
        });
        id
    }

    fn enter(&mut self, parent: Option<String>, suppress_vars: bool, object_initializer: bool) {
        self.stack.push(Frame {
            parent,
            suppress_vars,
            object_initializer,
        });
    }

    fn leave(&mut self) {
        self.stack.pop();
    }
}

fn property_key_name(key: &PropertyKey<'_>, source: &str) -> Option<(String, Span)> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some((id.name.to_string(), id.span)),
        PropertyKey::PrivateIdentifier(id) => Some((format!("#{}", id.name), id.span)),
        PropertyKey::StringLiteral(lit) => Some((lit.value.to_string(), lit.span)),
        PropertyKey::NumericLiteral(lit) => Some((
            source
                .get(lit.span.start as usize..lit.span.end as usize)?
                .to_string(),
            lit.span,
        )),
        _ => None,
    }
}

/// tree-sitter's `public_field_definition` / `property_signature` /
/// `method_signature` nodes end at the member itself; oxc's spans run to the
/// terminator. Trimming trailing `;`/`,`/whitespace is what makes the two
/// agree — found by the oracle, which reported 144 entity divergences that
/// were all exactly one byte of `;`.
fn trim_member_span(source: &str, span: Span) -> Span {
    let bytes = source.as_bytes();
    let mut end = (span.end as usize).min(source.len());
    while end > span.start as usize {
        match bytes[end - 1] {
            b';' | b',' => end -= 1,
            b if b.is_ascii_whitespace() => end -= 1,
            _ => break,
        }
    }
    Span::new(span.start, end as u32)
}

/// `extract_js_test_call`: `describe`/`test`/`it`/`before*`/`after*` calls,
/// optionally through one member access (`describe.skip`, `it.each`).
fn test_call<'a>(call: &CallExpression<'a>, source: &str) -> Option<(String, &'static str, bool)> {
    let callee_name = match &call.callee {
        Expression::Identifier(id) => id.name.as_str(),
        Expression::StaticMemberExpression(m) => match &m.object {
            Expression::Identifier(id) => id.name.as_str(),
            _ => return None,
        },
        _ => return None,
    };
    let (kind, is_container) = match callee_name {
        "describe" => ("test_suite", true),
        "test" | "it" => ("test", false),
        "beforeEach" | "afterEach" | "beforeAll" | "afterAll" => ("test_hook", false),
        _ => return None,
    };
    let first = call.arguments.first();
    let name = match first {
        Some(Argument::StringLiteral(lit)) => lit.value.to_string(),
        Some(Argument::TemplateLiteral(t)) => source
            .get(t.span.start as usize..t.span.end as usize)?
            .trim_matches(|c: char| c == '`')
            .to_string(),
        _ => {
            if matches!(
                callee_name,
                "beforeEach" | "afterEach" | "beforeAll" | "afterAll"
            ) {
                callee_name.to_string()
            } else {
                return None;
            }
        }
    };
    Some((name, kind, is_container))
}

/// The second argument's body, when it is a function/arrow — the region
/// `visit_node` recurses into for a container test call.
fn test_callback_body<'a, 'b>(call: &'b CallExpression<'a>) -> Option<&'b FunctionBody<'a>> {
    match call.arguments.get(1)? {
        Argument::ArrowFunctionExpression(a) => match &a.body {
            oxc_ast::ast::ArrowFunctionBody::FunctionBody(b) => Some(b.as_ref()),
            _ => None,
        },
        Argument::FunctionExpression(f) => f.body.as_deref(),
        _ => None,
    }
}

fn is_function_initializer(expr: &Expression<'_>) -> bool {
    matches!(
        expr,
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
    )
}

/// The initializer kinds `visit_node` descends into (`is_js_ts_initializer_node`).
fn is_initializer_node(expr: &Expression<'_>) -> bool {
    matches!(
        expr,
        Expression::ArrowFunctionExpression(_)
            | Expression::FunctionExpression(_)
            | Expression::ClassExpression(_)
            | Expression::ObjectExpression(_)
    )
}

impl<'a> Visit<'a> for Collector<'_> {
    fn visit_ts_module_declaration(&mut self, it: &TSModuleDeclaration<'a>) {
        let _ = it;
        self.unmodelled("namespace / module declaration");
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        // `export { x } from './y'` synthesizes re-export entities
        // (`emit_js_ts_re_export_entities`); not modelled.
        if !it.specifiers.is_empty() {
            self.unmodelled("export specifier list");
            return;
        }
        walk::walk_export_named_declaration(self, it);
    }

    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        let _ = it;
        self.unmodelled("re-export with source");
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        let _ = it;
        self.unmodelled("export-all declaration");
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if it.body.is_none() {
            // TS overload signature / `declare function`. `visit_node`
            // suppresses these only when an implementation of the same name is
            // in scope, which needs the whole enclosing scope in hand.
            self.unmodelled("function without a body (overload or declare)");
            return;
        }
        let is_declaration = matches!(it.r#type, FunctionType::FunctionDeclaration);
        match (is_declaration, it.id.as_ref()) {
            (true, Some(id)) => {
                // An *entity* node: `visit_node` pushes only the children of
                // its `container_node_types` (here, the statement block), so
                // parameters and the return-type annotation are never
                // traversed. Walking the whole function would over-extract
                // property signatures out of type annotations.
                let entity_id = self.push_entity("function", &id.name, it.span, id.span);
                self.enter(Some(entity_id), true, false);
                if let Some(body) = it.body.as_ref() {
                    self.visit_function_body(body);
                }
                self.leave();
                return;
            }
            _ => {
                // A function *expression* is a scope boundary, not an entity,
                // so it keeps the generic traversal (parameters included).
                let parent = self.frame().parent.clone();
                self.enter(parent, true, false);
            }
        }
        walk::walk_function(self, it, flags);
        self.leave();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let parent = self.frame().parent.clone();
        self.enter(parent, true, false);
        walk::walk_arrow_function_expression(self, it);
        self.leave();
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        let entered = match it.id.as_ref() {
            Some(id) => {
                let entity_id = self.push_entity("class", &id.name, it.span, id.span);
                let suppress = self.frame().suppress_vars;
                self.enter(Some(entity_id), suppress, false);
                true
            }
            None => false,
        };
        for element in &it.body.body {
            match element {
                ClassElement::MethodDefinition(m) => {
                    if m.value.body.is_none() {
                        self.unmodelled("class method without a body (overload)");
                        continue;
                    }
                    let Some((name, name_span)) = property_key_name(&m.key, self.source) else {
                        self.unmodelled("computed class member key");
                        continue;
                    };
                    let kind = match m.kind {
                        MethodDefinitionKind::Get => "getter",
                        MethodDefinitionKind::Set => "setter",
                        _ => "method",
                    };
                    let entity_id = self.push_entity(kind, &name, m.span, name_span);
                    self.enter(Some(entity_id), true, false);
                    if let Some(body) = m.value.body.as_ref() {
                        self.visit_function_body(body);
                    }
                    self.leave();
                }
                ClassElement::PropertyDefinition(p) => {
                    let Some((name, name_span)) = property_key_name(&p.key, self.source) else {
                        self.unmodelled("computed class property key");
                        continue;
                    };
                    let span = trim_member_span(self.source, p.span);
                    let entity_id = self.push_entity("field", &name, span, name_span);
                    if let Some(init) = p.value.as_ref() {
                        self.descend_into_initializer(entity_id, init);
                    }
                }
                ClassElement::StaticBlock(_) => self.unmodelled("class static block"),
                ClassElement::AccessorProperty(_) => self.unmodelled("accessor property"),
                ClassElement::TSIndexSignature(_) => {}
            }
        }
        if entered {
            self.leave();
        }
    }

    fn visit_ts_interface_declaration(&mut self, it: &TSInterfaceDeclaration<'a>) {
        let entity_id = self.push_entity("interface", &it.id.name, it.span, it.id.span);
        self.enter(Some(entity_id), false, false);
        for member in &it.body.body {
            match member {
                TSSignature::TSPropertySignature(p) => {
                    let Some((name, name_span)) = property_key_name(&p.key, self.source) else {
                        self.unmodelled("computed property signature key");
                        continue;
                    };
                    let span = trim_member_span(self.source, p.span);
                    self.push_entity("property", &name, span, name_span);
                }
                TSSignature::TSMethodSignature(m) => {
                    let Some((name, name_span)) = property_key_name(&m.key, self.source) else {
                        self.unmodelled("computed method signature key");
                        continue;
                    };
                    let span = trim_member_span(self.source, m.span);
                    self.push_entity("method", &name, span, name_span);
                }
                TSSignature::TSIndexSignature(_)
                | TSSignature::TSCallSignatureDeclaration(_)
                | TSSignature::TSConstructSignatureDeclaration(_) => {}
            }
        }
        self.leave();
    }

    fn visit_ts_type_alias_declaration(&mut self, it: &TSTypeAliasDeclaration<'a>) {
        self.push_entity("type", &it.id.name, it.span, it.id.span);
    }

    fn visit_ts_enum_declaration(&mut self, it: &TSEnumDeclaration<'a>) {
        self.push_entity("enum", &it.id.name, it.span, it.id.span);
    }

    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        if matches!(
            it.kind,
            VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
        ) {
            self.unmodelled("using declaration");
            return;
        }
        let multi = it.declarations.len() > 1;
        // `promote_js_ts_const_function` promotes `const f = () => {}` to a
        // `function` entity, and *only* `const`: `let f = () => {}` stays a
        // `variable`, pinned by `test_let_assigned_arrow_stays_variable_typescript`.
        let const_decl = matches!(it.kind, VariableDeclarationKind::Const);
        for decl in &it.declarations {
            let promoted = const_decl && decl.init.as_ref().is_some_and(is_function_initializer);
            let Some(binding) = decl.id.get_binding_identifier() else {
                if self.frame().suppress_vars && !promoted {
                    // Suppressed: `visit_node` falls through to its *generic*
                    // child traversal, which reaches the type annotation as
                    // well as the initializer.
                    walk::walk_variable_declarator(self, decl);
                    continue;
                }
                self.unmodelled("destructuring declarator");
                continue;
            };
            if self.frame().suppress_vars && !promoted {
                walk::walk_variable_declarator(self, decl);
                continue;
            }
            let kind = if promoted { "function" } else { "variable" };

            let span = if multi { decl.span } else { it.span };
            let entity_id = self.push_entity(kind, &binding.name, span, binding.span);
            if let Some(init) = decl.init.as_ref() {
                self.descend_into_initializer(entity_id, init);
            }
        }
    }

    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        let promote =
            it.method || (self.frame().object_initializer && is_function_initializer(&it.value));
        if !promote {
            walk::walk_object_property(self, it);
            return;
        }
        let Some((name, name_span)) = property_key_name(&it.key, self.source) else {
            self.unmodelled("computed object-literal method key");
            return;
        };
        let kind = match it.kind {
            PropertyKind::Get => "getter",
            PropertyKind::Set => "setter",
            PropertyKind::Init => "method",
        };
        let entity_id = self.push_entity(kind, &name, it.span, name_span);
        self.enter(Some(entity_id), true, false);
        self.visit_expression(&it.value);
        self.leave();
    }

    /// `property_signature` / `method_signature` are entity node types
    /// wherever they occur, so a type literal inside a traversed region — a
    /// local's type annotation inside a function body, say — contributes
    /// entities parented to whatever entity encloses it.
    fn visit_ts_type_literal(&mut self, it: &TSTypeLiteral<'a>) {
        for member in &it.members {
            match member {
                TSSignature::TSPropertySignature(p) => {
                    let Some((name, name_span)) = property_key_name(&p.key, self.source) else {
                        self.unmodelled("computed type-literal property key");
                        continue;
                    };
                    let span = trim_member_span(self.source, p.span);
                    self.push_entity("property", &name, span, name_span);
                }
                TSSignature::TSMethodSignature(m) => {
                    let Some((name, name_span)) = property_key_name(&m.key, self.source) else {
                        self.unmodelled("computed type-literal method key");
                        continue;
                    };
                    let span = trim_member_span(self.source, m.span);
                    self.push_entity("method", &name, span, name_span);
                }
                TSSignature::TSIndexSignature(_)
                | TSSignature::TSCallSignatureDeclaration(_)
                | TSSignature::TSConstructSignatureDeclaration(_) => {}
            }
        }
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        let Some((name, kind, is_container)) = test_call(it, self.source) else {
            walk::walk_call_expression(self, it);
            return;
        };
        let name_span = Span::new(it.span.start, it.span.start);
        let entity_id = self.push_entity(kind, &name, it.span, name_span);
        if !is_container {
            // `visit_node` does not descend into a non-container test call.
            return;
        }
        let Some(body) = test_callback_body(it) else {
            return;
        };
        let suppress = self.frame().suppress_vars;
        self.enter(Some(entity_id), suppress, false);
        self.visit_function_body(body);
        self.leave();
    }
}

impl Collector<'_> {
    /// `visit_node`'s restricted initializer descent: an emitted variable or
    /// field entity stops the generic traversal and descends only into an
    /// initializer that is a function, arrow, class or object literal.
    fn descend_into_initializer(&mut self, entity_id: String, init: &Expression<'_>) {
        if !is_initializer_node(init) {
            return;
        }
        let is_object = matches!(init, Expression::ObjectExpression(_));
        self.enter(Some(entity_id), true, is_object);
        self.visit_expression(init);
        self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(path: &str, src: &str) -> Option<Vec<SemanticEntity>> {
        OxcExtractor::new().extract(path, src)
    }

    fn names(path: &str, src: &str) -> Vec<String> {
        extract(path, src)
            .expect("modelled")
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    #[test]
    fn a_parse_error_declines() {
        assert!(extract("a.ts", "function (").is_none());
    }

    #[test]
    fn an_unmodelled_construct_declines_the_whole_file() {
        let src = "export function ok() {}\nnamespace N { export const x = 1; }\n";
        assert!(extract("a.ts", src).is_none());
    }

    #[test]
    fn a_modelled_file_yields_entities_in_source_order() {
        let src = "export function alpha() {}\nexport class Beta { gamma() {} }\n";
        let entities = extract("a.ts", src).expect("modelled");
        let got: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(got, vec!["alpha", "Beta", "gamma"]);
        assert_eq!(entities[2].parent_id.as_deref(), Some("a.ts::class::Beta"));
    }

    #[test]
    fn locals_inside_a_function_body_are_suppressed() {
        let src = "export function outer() {\n  const local = 1;\n  function inner() {}\n}\n";
        assert_eq!(names("a.ts", src), vec!["outer", "inner"]);
    }

    #[test]
    fn locals_inside_an_arrow_body_are_suppressed_but_a_const_arrow_is_promoted() {
        let src = "export const run = () => {\n  const local = 1;\n  const helper = () => 2;\n};\n";
        assert_eq!(names("a.ts", src), vec!["run", "helper"]);
    }

    #[test]
    fn an_object_initializer_promotes_both_shorthand_and_arrow_properties() {
        let src = "export const api = {\n  read() {},\n  write: () => {},\n  size: 3,\n};\n";
        assert_eq!(names("a.ts", src), vec!["api", "read", "write"]);
    }

    #[test]
    fn a_non_initializer_call_is_not_descended_into() {
        let src = "export const wrapped = wrap(function () {\n  function hidden() {}\n});\n";
        assert_eq!(names("a.ts", src), vec!["wrapped"]);
    }

    #[test]
    fn kappa_is_invariant_to_trailing_commas_and_semicolons() {
        let a = extract("a.ts", "export function f(x: number) { return x + 1; }").unwrap();
        let b = extract(
            "a.ts",
            "export function f(\n  x: number,\n) {\n  return x + 1\n}",
        )
        .unwrap();
        assert_eq!(a[0].kappa, b[0].kappa);
    }

    #[test]
    fn kappa_changes_when_semantics_change() {
        let a = extract("a.ts", "export function f(x: number) { return x + 1; }").unwrap();
        let b = extract("a.ts", "export function f(x: number) { return x + 2; }").unwrap();
        assert_ne!(a[0].kappa, b[0].kappa);
    }

    #[test]
    fn the_structural_sibling_is_rename_insensitive_but_body_sensitive() {
        let a = extract("a.ts", "export function f(x: number) { return x + 1; }").unwrap();
        let renamed = extract("a.ts", "export function g(x: number) { return x + 1; }").unwrap();
        let changed = extract("a.ts", "export function f(x: number) { return x + 2; }").unwrap();
        assert_eq!(a[0].structural_hash, renamed[0].structural_hash);
        assert_ne!(a[0].structural_hash, changed[0].structural_hash);
        assert_ne!(a[0].kappa, renamed[0].kappa);
    }

    #[test]
    fn grouping_is_not_dropped_by_canonicalization() {
        let a = extract("a.ts", "export const v = (a + b) * c;").unwrap();
        let b = extract("a.ts", "export const v = a + b * c;").unwrap();
        assert_ne!(a[0].kappa, b[0].kappa);
    }

    #[test]
    fn multi_declarators_split_into_one_entity_each() {
        assert_eq!(
            names("a.ts", "export const first = 1, second = 2;"),
            vec!["first", "second"]
        );
    }
}
