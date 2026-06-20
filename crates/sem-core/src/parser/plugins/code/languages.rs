use std::collections::HashMap;

use tree_sitter::Language;

use crate::parser::graph::EntityInfo;

pub struct SuppressedNestedEntity {
    pub parent_entity_node_type: &'static str,
    pub child_entity_node_type: &'static str,
}

/// Strategy for stripping comments and string literals from source content.
/// Controls which stripping function `strip_for_language` dispatches to in graph.rs.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum StripStrategy {
    /// Standard stripper: handles `//`, `/* */`, and `#` line comments, plus string literals.
    Generic,
    /// Clojure: blank double-quoted strings only (preserves `#` for gensyms and reader macros),
    /// then strip `;` line comments in a second pass.
    Clojure,
}

#[allow(dead_code)]
pub struct LanguageConfig {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
    pub entity_node_types: &'static [&'static str],
    pub container_node_types: &'static [&'static str],
    pub call_entity_identifiers: &'static [&'static str],
    pub suppressed_nested_entities: &'static [SuppressedNestedEntity],
    /// Node types that introduce a new scope. The general (non-container) recursion
    /// in visit_node will not descend into these nodes, preventing local variables
    /// inside function bodies from being extracted as top-level entities.
    pub scope_boundary_types: &'static [&'static str],
    pub get_language: fn() -> Option<Language>,
    pub scope_resolve: Option<&'static ScopeResolveConfig>,
}

const CLOJURE_EXTRA_IDENT_CHARS: &[char] = &['-', '?', '!', '*', '='];

impl LanguageConfig {
    pub(crate) fn extra_ident_chars(&self) -> &'static [char] {
        match self.id {
            "clojure" | "edn" => CLOJURE_EXTRA_IDENT_CHARS,
            _ => &[],
        }
    }

    pub(crate) fn strip_strategy(&self) -> StripStrategy {
        match self.id {
            "clojure" | "edn" => StripStrategy::Clojure,
            _ => StripStrategy::Generic,
        }
    }

    pub(crate) fn has_slash_qualified_refs(&self) -> bool {
        self.id == "clojure"
    }

    pub(crate) fn extract_map_entries(&self) -> bool {
        self.id == "edn"
    }
}

// ─── Scope Resolve Config Types ───────────────────────────────────────────────

/// Configuration for scope-aware reference resolution.
/// Captures the AST node names and strategies that differ per language.
pub struct ScopeResolveConfig {
    /// AST node types that create class/struct scopes
    pub class_scope_nodes: &'static [&'static str],
    /// AST node types that create impl scopes (Rust impl_item, Swift extension)
    pub impl_scope_nodes: &'static [&'static str],
    /// AST node types that create function/method scopes
    pub function_scope_nodes: &'static [&'static str],
    /// How to extract the class name from a class scope node
    pub class_name_field: ClassNameField,

    /// Rules for scanning variable assignments to track types
    pub assignment_rules: &'static [AssignmentRule],
    /// Node types to recurse into when scanning assignments
    pub assignment_recurse_into: &'static [&'static str],

    /// Rules for extracting typed parameters from function signatures
    pub param_rules: &'static [ParamRule],

    /// Field name for return type annotation on function nodes (None = body heuristic only)
    pub return_type_field: Option<&'static str>,

    /// AST node types that represent function/method calls
    pub call_nodes: &'static [&'static str],
    /// How call nodes expose the callee. FunctionField = node has a "function" field containing
    /// an identifier or member_expression. DirectMethod = node has object+name fields directly.
    pub call_style: CallNodeStyle,
    /// AST node types for `new Foo()` expressions
    pub new_expr_nodes: &'static [&'static str],
    /// Field name on new-expression nodes that holds the type/constructor name.
    pub new_expr_type_field: &'static str,
    /// AST node types for struct/composite literals (Go `Foo{}`)
    pub composite_literal_nodes: &'static [&'static str],
    /// How member access / method calls are represented in the AST
    pub member_access: &'static [MemberAccess],
    /// Scoped identifier nodes (Rust `Type::method`)
    pub scoped_call_nodes: &'static [&'static str],

    /// Self/this keywords to recognize
    pub self_keywords: &'static [&'static str],

    /// Strategy for extracting instance attribute types
    pub init_strategy: InitStrategy,

    /// Import extraction function (the only truly per-language piece)
    pub import_extractor: Option<ImportExtractorFn>,

    /// Whether methods are declared externally with receiver types (Go-style)
    pub external_method: bool,

    /// Language builtins to skip during resolution
    pub builtins: &'static [&'static str],
}

/// How call nodes expose the callee/function.
pub enum CallNodeStyle {
    /// The call node has a field (e.g. "function") containing either an identifier
    /// (bare call) or a member_expression (method call). Python, TS, Rust, Go, C#, C++.
    FunctionField(&'static str),
    /// The call node directly has object (optional) + method name fields.
    /// Java: method_invocation(object, name). Ruby: call(receiver, method).
    DirectMethod {
        object_field: &'static str,
        method_field: &'static str,
    },
    /// The callee is the first named child of the call node (no field name).
    /// Swift: call_expression(simple_identifier|navigation_expression, call_suffix)
    /// Kotlin: call_expression(identifier|navigation_expression, value_arguments)
    FirstChild,
}

/// How to extract the class/struct name from a scope node.
pub enum ClassNameField {
    /// Simple field lookup: `node.child_by_field_name(field)`
    Simple(&'static str),
    /// Go-style: look for a child of type `spec_kind`, then get field `field` from it
    TypeSpec {
        spec_kind: &'static str,
        field: &'static str,
    },
    /// Rust impl: get name from `node.child_by_field_name(field)` (the "type" field)
    ImplType(&'static str),
}

/// A rule for scanning assignment nodes to extract type bindings.
pub struct AssignmentRule {
    pub node_kind: &'static str,
    pub strategy: AssignmentStrategy,
}

/// Strategy for extracting variable name and type from an assignment node.
pub enum AssignmentStrategy {
    /// Python/TS: `x = Foo()` - left/right fields on assignment node
    LeftRight,
    /// TS: `const x = new Foo()` - variable_declarator children
    Declarators,
    /// Rust: `let x: Type = value` - pattern + type + value fields
    PatternBased,
    /// Go: `x := Foo{}` - expression_list left/right
    ShortVar,
    /// Go: `var x Type = ...` - var_spec children
    VarSpec,
}

/// A rule for extracting typed parameters from function signatures.
pub struct ParamRule {
    pub node_kind: &'static str,
    pub name_field: ParamNameField,
    pub type_field: &'static str,
    pub skip_names: &'static [&'static str],
}

/// How to extract the parameter name.
pub enum ParamNameField {
    /// Simple field name: `child_by_field_name(field)`
    Simple(&'static str),
    /// Field with fallback to first named child if identifier
    WithFallback(&'static str),
    /// Rust pattern matching (identifier, mut_pattern, reference_pattern)
    RustPattern,
}

/// How member access (obj.field / obj.method()) is represented in the AST.
pub struct MemberAccess {
    pub node_kind: &'static str,
    pub object_field: &'static str,
    pub property_field: &'static str,
}

/// Strategy for extracting instance attribute types from class definitions.
pub enum InitStrategy {
    /// Python/TS: scan constructor body for self.attr = param patterns
    ConstructorBody {
        class_nodes: &'static [&'static str],
        init_names: &'static [&'static str],
        init_node_kind: &'static str,
        self_keyword: &'static str,
        access_kind: &'static str,
        obj_field: &'static str,
        prop_field: &'static str,
    },
    /// Rust/Go: extract field types directly from struct declarations
    StructFields {
        struct_nodes: &'static [&'static str],
    },
    /// Java/C#: extract field types from typed field declarations in the class body
    ClassFields {
        class_nodes: &'static [&'static str],
    },
    /// No instance attribute tracking
    None,
}

/// Function pointer type for import extraction.
pub type ImportExtractorFn = fn(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<crate::parser::scope_resolve::Scope>,
);

/// Import node kind + extractor function pair
pub struct ImportRule {
    pub node_kind: &'static str,
    pub extractor: ImportExtractorFn,
}

#[cfg(feature = "lang-typescript")]
fn get_typescript() -> Option<Language> {
    Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
}

#[cfg(feature = "lang-typescript")]
fn get_tsx() -> Option<Language> {
    Some(tree_sitter_typescript::LANGUAGE_TSX.into())
}

#[cfg(feature = "lang-javascript")]
fn get_javascript() -> Option<Language> {
    Some(tree_sitter_javascript::LANGUAGE.into())
}

#[cfg(feature = "lang-python")]
fn get_python() -> Option<Language> {
    Some(tree_sitter_python::LANGUAGE.into())
}

#[cfg(feature = "lang-go")]
fn get_go() -> Option<Language> {
    Some(tree_sitter_go::LANGUAGE.into())
}

#[cfg(feature = "lang-rust")]
fn get_rust() -> Option<Language> {
    Some(tree_sitter_rust::LANGUAGE.into())
}

#[cfg(feature = "lang-java")]
fn get_java() -> Option<Language> {
    Some(tree_sitter_java::LANGUAGE.into())
}

#[cfg(feature = "lang-c")]
fn get_c() -> Option<Language> {
    Some(tree_sitter_c::LANGUAGE.into())
}

#[cfg(feature = "lang-cpp")]
fn get_cpp() -> Option<Language> {
    Some(tree_sitter_cpp::LANGUAGE.into())
}

#[cfg(feature = "lang-ruby")]
fn get_ruby() -> Option<Language> {
    Some(tree_sitter_ruby::LANGUAGE.into())
}

#[cfg(feature = "lang-csharp")]
fn get_csharp() -> Option<Language> {
    Some(tree_sitter_c_sharp::LANGUAGE.into())
}

#[cfg(feature = "lang-php")]
fn get_php() -> Option<Language> {
    Some(tree_sitter_php::LANGUAGE_PHP.into())
}

#[cfg(feature = "lang-fortran")]
fn get_fortran() -> Option<Language> {
    Some(tree_sitter_fortran::LANGUAGE.into())
}

#[cfg(feature = "lang-swift")]
fn get_swift() -> Option<Language> {
    Some(tree_sitter_swift::LANGUAGE.into())
}

#[cfg(feature = "lang-elixir")]
fn get_elixir() -> Option<Language> {
    Some(tree_sitter_elixir::LANGUAGE.into())
}

#[cfg(feature = "lang-bash")]
fn get_bash() -> Option<Language> {
    Some(tree_sitter_bash::LANGUAGE.into())
}

#[cfg(feature = "lang-hcl")]
fn get_hcl() -> Option<Language> {
    Some(tree_sitter_hcl::LANGUAGE.into())
}

#[cfg(feature = "lang-kotlin")]
fn get_kotlin() -> Option<Language> {
    Some(tree_sitter_kotlin_ng::LANGUAGE.into())
}

#[cfg(feature = "lang-xml")]
fn get_xml() -> Option<Language> {
    Some(tree_sitter_xml::LANGUAGE_XML.into())
}

#[cfg(feature = "lang-dart")]
fn get_dart() -> Option<Language> {
    Some(tree_sitter_dart::LANGUAGE.into())
}

#[cfg(feature = "lang-perl")]
fn get_perl() -> Option<Language> {
    Some(ts_parser_perl::LANGUAGE.into())
}

#[cfg(feature = "lang-sql")]
fn get_sql() -> Option<Language> {
    Some(tree_sitter_sequel::LANGUAGE.into())
}

#[cfg(feature = "lang-ocaml")]
fn get_ocaml() -> Option<Language> {
    Some(tree_sitter_ocaml::LANGUAGE_OCAML.into())
}

#[cfg(feature = "lang-ocaml")]
fn get_ocaml_interface() -> Option<Language> {
    Some(tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE.into())
}

#[cfg(feature = "lang-scala")]
fn get_scala() -> Option<Language> {
    Some(tree_sitter_scala::LANGUAGE.into())
}

#[cfg(feature = "lang-zig")]
fn get_zig() -> Option<Language> {
    Some(tree_sitter_zig::LANGUAGE.into())
}

#[cfg(feature = "lang-nix")]
fn get_nix() -> Option<Language> {
    Some(tree_sitter_nix::LANGUAGE.into())
}

#[cfg(feature = "lang-haskell")]
fn get_haskell() -> Option<Language> {
    Some(tree_sitter_haskell::LANGUAGE.into())
}

#[cfg(feature = "lang-elm")]
fn get_elm() -> Option<Language> {
    Some(tree_sitter_elm::LANGUAGE.into())
}

#[cfg(any(feature = "lang-clojure", feature = "lang-edn"))]
fn get_clojure() -> Option<Language> {
    Some(tree_sitter_clojure_orchard::LANGUAGE.into())
}

#[cfg(feature = "lang-d")]
fn get_d() -> Option<Language> {
    Some(tree_sitter_d::LANGUAGE.into())
}

/// Inside JS/TS function bodies, suppress variable declarations so that local
/// variables are not extracted as nested entities. Inner function/class
/// declarations are still extracted for diff granularity.
const JS_TS_SUPPRESSED_NESTED: &[SuppressedNestedEntity] = &[
    SuppressedNestedEntity {
        parent_entity_node_type: "function_declaration",
        child_entity_node_type: "lexical_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "function_declaration",
        child_entity_node_type: "variable_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "generator_function_declaration",
        child_entity_node_type: "lexical_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "generator_function_declaration",
        child_entity_node_type: "variable_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "method_definition",
        child_entity_node_type: "lexical_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "method_definition",
        child_entity_node_type: "variable_declaration",
    },
    // Scope boundaries: suppress local variables inside arrow functions,
    // function expressions, and generator functions, while still allowing
    // inner class/function declarations to be extracted.
    SuppressedNestedEntity {
        parent_entity_node_type: "arrow_function",
        child_entity_node_type: "lexical_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "arrow_function",
        child_entity_node_type: "variable_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "function_expression",
        child_entity_node_type: "lexical_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "function_expression",
        child_entity_node_type: "variable_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "generator_function",
        child_entity_node_type: "lexical_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "generator_function",
        child_entity_node_type: "variable_declaration",
    },
];

const JS_TS_SCOPE_BOUNDARIES: &[&str] = &[
    "arrow_function",
    "function_expression",
    "generator_function",
];

/// Inside C function bodies, suppress `declaration` nodes so that block-local
/// variables are not extracted as nested entities. Inner type declarations are
/// still reached by traversal after the wrapper is skipped.
const C_SUPPRESSED_NESTED: &[SuppressedNestedEntity] = &[SuppressedNestedEntity {
    parent_entity_node_type: "function_definition",
    child_entity_node_type: "declaration",
}];

/// Inside C++ function-like bodies, suppress `declaration` nodes so that
/// block-local variables are not extracted as nested entities. Inner type
/// declarations are still reached by traversal after the wrapper is skipped.
const CPP_SUPPRESSED_NESTED: &[SuppressedNestedEntity] = &[
    SuppressedNestedEntity {
        parent_entity_node_type: "function_definition",
        child_entity_node_type: "declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "lambda_expression",
        child_entity_node_type: "declaration",
    },
];

const CPP_SCOPE_BOUNDARIES: &[&str] = &["lambda_expression"];

const SWIFT_SUPPRESSED_NESTED: &[SuppressedNestedEntity] = &[
    SuppressedNestedEntity {
        parent_entity_node_type: "function_declaration",
        child_entity_node_type: "property_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "init_declaration",
        child_entity_node_type: "property_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "deinit_declaration",
        child_entity_node_type: "property_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "subscript_declaration",
        child_entity_node_type: "property_declaration",
    },
    SuppressedNestedEntity {
        parent_entity_node_type: "property_declaration",
        child_entity_node_type: "property_declaration",
    },
];

#[cfg(feature = "lang-typescript")]
static TYPESCRIPT_CONFIG: LanguageConfig = LanguageConfig {
    id: "typescript",
    extensions: &[".ts", ".mts", ".cts"],
    entity_node_types: &[
        "function_declaration",
        "generator_function_declaration",
        "function_signature",
        "class_declaration",
        "abstract_class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
        "internal_module",
        "module",
        "export_statement",
        "lexical_declaration",
        "variable_declaration",
        "method_definition",
        "abstract_method_signature",
        "public_field_definition",
        "function_signature",
        "method_signature",
        "property_signature",
    ],
    container_node_types: &[
        "class_body",
        "interface_body",
        "enum_body",
        "statement_block",
    ],
    call_entity_identifiers: &[],
    suppressed_nested_entities: JS_TS_SUPPRESSED_NESTED,
    scope_boundary_types: JS_TS_SCOPE_BOUNDARIES,
    get_language: get_typescript,
    scope_resolve: Some(&TS_SCOPE_CONFIG),
};

#[cfg(feature = "lang-typescript")]
static TSX_CONFIG: LanguageConfig = LanguageConfig {
    id: "tsx",
    extensions: &[".tsx"],
    entity_node_types: &[
        "function_declaration",
        "generator_function_declaration",
        "function_signature",
        "class_declaration",
        "abstract_class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
        "internal_module",
        "module",
        "export_statement",
        "lexical_declaration",
        "variable_declaration",
        "method_definition",
        "abstract_method_signature",
        "public_field_definition",
        "function_signature",
        "method_signature",
        "property_signature",
    ],
    container_node_types: &[
        "class_body",
        "interface_body",
        "enum_body",
        "statement_block",
    ],
    call_entity_identifiers: &[],
    suppressed_nested_entities: JS_TS_SUPPRESSED_NESTED,
    scope_boundary_types: JS_TS_SCOPE_BOUNDARIES,
    get_language: get_tsx,
    scope_resolve: Some(&TS_SCOPE_CONFIG),
};

#[cfg(feature = "lang-javascript")]
static JAVASCRIPT_CONFIG: LanguageConfig = LanguageConfig {
    id: "javascript",
    extensions: &[".js", ".jsx", ".mjs", ".cjs", ".es6"],
    entity_node_types: &[
        "function_declaration",
        "generator_function_declaration",
        "class_declaration",
        "export_statement",
        "lexical_declaration",
        "variable_declaration",
        "method_definition",
        "field_definition",
    ],
    container_node_types: &["class_body", "statement_block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: JS_TS_SUPPRESSED_NESTED,
    scope_boundary_types: JS_TS_SCOPE_BOUNDARIES,
    get_language: get_javascript,
    scope_resolve: Some(&TS_SCOPE_CONFIG),
};

#[cfg(feature = "lang-python")]
static PYTHON_CONFIG: LanguageConfig = LanguageConfig {
    id: "python",
    extensions: &[".py", ".pyi"],
    entity_node_types: &[
        "function_definition",
        "class_definition",
        "decorated_definition",
    ],
    container_node_types: &["block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_python,
    scope_resolve: Some(&PYTHON_SCOPE_CONFIG),
};

#[cfg(feature = "lang-go")]
static GO_CONFIG: LanguageConfig = LanguageConfig {
    id: "go",
    extensions: &[".go"],
    entity_node_types: &[
        "function_declaration",
        "method_declaration",
        "type_declaration",
        "var_declaration",
        "const_declaration",
    ],
    container_node_types: &["block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_go,
    scope_resolve: Some(&GO_SCOPE_CONFIG),
};

#[cfg(feature = "lang-rust")]
static RUST_CONFIG: LanguageConfig = LanguageConfig {
    id: "rust",
    extensions: &[".rs"],
    entity_node_types: &[
        "function_item",
        "struct_item",
        "enum_item",
        "impl_item",
        "trait_item",
        "mod_item",
        "const_item",
        "static_item",
        "type_item",
        "macro_definition",
    ],
    container_node_types: &["declaration_list", "block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_rust,
    scope_resolve: Some(&RUST_SCOPE_CONFIG),
};

#[cfg(feature = "lang-java")]
static JAVA_CONFIG: LanguageConfig = LanguageConfig {
    id: "java",
    extensions: &[".java"],
    entity_node_types: &[
        "class_declaration",
        "method_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
        "field_declaration",
        "constructor_declaration",
        "annotation_type_declaration",
    ],
    container_node_types: &[
        "class_body",
        "interface_body",
        "enum_body",
        "record_body",
        "block",
    ],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_java,
    scope_resolve: Some(&JAVA_SCOPE_CONFIG),
};

#[cfg(feature = "lang-c")]
static C_CONFIG: LanguageConfig = LanguageConfig {
    id: "c",
    extensions: &[".c", ".h"],
    entity_node_types: &[
        "function_definition",
        "struct_specifier",
        "enum_specifier",
        "union_specifier",
        "type_definition",
        "declaration",
    ],
    container_node_types: &["compound_statement"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: C_SUPPRESSED_NESTED,
    scope_boundary_types: &[],
    get_language: get_c,
    scope_resolve: None,
};

#[cfg(feature = "lang-cpp")]
static CPP_CONFIG: LanguageConfig = LanguageConfig {
    id: "cpp",
    extensions: &[".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx"],
    entity_node_types: &[
        "function_definition",
        "class_specifier",
        "struct_specifier",
        "enum_specifier",
        "namespace_definition",
        "template_declaration",
        "declaration",
        "type_definition",
    ],
    container_node_types: &[
        "field_declaration_list",
        "declaration_list",
        "compound_statement",
    ],
    call_entity_identifiers: &[],
    suppressed_nested_entities: CPP_SUPPRESSED_NESTED,
    scope_boundary_types: CPP_SCOPE_BOUNDARIES,
    get_language: get_cpp,
    scope_resolve: Some(&CPP_SCOPE_CONFIG),
};

#[cfg(feature = "lang-ruby")]
static RUBY_CONFIG: LanguageConfig = LanguageConfig {
    id: "ruby",
    extensions: &[".rb"],
    entity_node_types: &["method", "singleton_method", "class", "module"],
    container_node_types: &["body_statement"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_ruby,
    scope_resolve: Some(&RUBY_SCOPE_CONFIG),
};

#[cfg(feature = "lang-csharp")]
static CSHARP_CONFIG: LanguageConfig = LanguageConfig {
    id: "csharp",
    extensions: &[".cs"],
    entity_node_types: &[
        "method_declaration",
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "struct_declaration",
        "record_declaration",
        "record_struct_declaration",
        "namespace_declaration",
        "property_declaration",
        "constructor_declaration",
        "field_declaration",
    ],
    container_node_types: &["declaration_list", "record_body", "block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_csharp,
    scope_resolve: Some(&CSHARP_SCOPE_CONFIG),
};

#[cfg(feature = "lang-php")]
static PHP_CONFIG: LanguageConfig = LanguageConfig {
    id: "php",
    extensions: &[".php", ".inc", ".phtml", ".module"],
    entity_node_types: &[
        "function_definition",
        "class_declaration",
        "method_declaration",
        "interface_declaration",
        "trait_declaration",
        "enum_declaration",
        "namespace_definition",
    ],
    container_node_types: &[
        "declaration_list",
        "enum_declaration_list",
        "compound_statement",
    ],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_php,
    scope_resolve: Some(&PHP_SCOPE_CONFIG),
};

#[cfg(feature = "lang-fortran")]
static FORTRAN_CONFIG: LanguageConfig = LanguageConfig {
    id: "fortran",
    extensions: &[".f90", ".f95", ".f03", ".f08", ".f", ".for"],
    entity_node_types: &[
        "function",
        "subroutine",
        "module",
        "program",
        "interface",
        "type_declaration",
    ],
    container_node_types: &["module", "program", "internal_procedures"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_fortran,
    scope_resolve: None,
};

#[cfg(feature = "lang-swift")]
static SWIFT_CONFIG: LanguageConfig = LanguageConfig {
    id: "swift",
    extensions: &[".swift"],
    entity_node_types: &[
        "function_declaration",
        "class_declaration",
        "protocol_declaration",
        "struct_declaration",
        "enum_declaration",
        "init_declaration",
        "deinit_declaration",
        "subscript_declaration",
        "typealias_declaration",
        "property_declaration",
        "operator_declaration",
        "associatedtype_declaration",
    ],
    container_node_types: &[
        "class_body",
        "protocol_body",
        "enum_class_body",
        "struct_body",
        "function_body",
        "computed_property",
    ],
    call_entity_identifiers: &[],
    suppressed_nested_entities: SWIFT_SUPPRESSED_NESTED,
    scope_boundary_types: &[],
    get_language: get_swift,
    scope_resolve: Some(&SWIFT_SCOPE_CONFIG),
};

#[cfg(feature = "lang-elixir")]
static ELIXIR_CONFIG: LanguageConfig = LanguageConfig {
    id: "elixir",
    extensions: &[".ex", ".exs"],
    entity_node_types: &[],
    container_node_types: &["do_block"],
    call_entity_identifiers: &[
        "defmodule",
        "def",
        "defp",
        "defmacro",
        "defmacrop",
        "defguard",
        "defguardp",
        "defprotocol",
        "defimpl",
        "defstruct",
        "defexception",
        "defdelegate",
    ],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_elixir,
    scope_resolve: None,
};

#[cfg(feature = "lang-bash")]
static BASH_CONFIG: LanguageConfig = LanguageConfig {
    id: "bash",
    extensions: &[".sh"],
    entity_node_types: &["function_definition"],
    container_node_types: &["compound_statement"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_bash,
    scope_resolve: Some(&BASH_SCOPE_CONFIG),
};

#[cfg(feature = "lang-hcl")]
static HCL_CONFIG: LanguageConfig = LanguageConfig {
    id: "hcl",
    extensions: &[".hcl", ".tf", ".tfvars"],
    entity_node_types: &["block", "attribute"],
    container_node_types: &["body"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[SuppressedNestedEntity {
        parent_entity_node_type: "block",
        child_entity_node_type: "attribute",
    }],
    scope_boundary_types: &[],
    get_language: get_hcl,
    scope_resolve: None,
};

#[cfg(feature = "lang-kotlin")]
static KOTLIN_CONFIG: LanguageConfig = LanguageConfig {
    id: "kotlin",
    extensions: &[".kt", ".kts"],
    entity_node_types: &[
        "function_declaration",
        "class_declaration",
        "object_declaration",
        "property_declaration",
        "companion_object",
        "secondary_constructor",
        "type_alias",
    ],
    container_node_types: &["class_body", "enum_class_body"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_kotlin,
    scope_resolve: Some(&KOTLIN_SCOPE_CONFIG),
};

#[cfg(feature = "lang-xml")]
static XML_CONFIG: LanguageConfig = LanguageConfig {
    id: "xml",
    extensions: &[
        ".xml", ".plist", ".svg", ".xhtml", ".csproj", ".fsproj", ".vbproj", ".props", ".targets",
        ".nuspec", ".resx", ".xaml", ".axml",
    ],
    entity_node_types: &["element"],
    container_node_types: &["content"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_xml,
    scope_resolve: None,
};

#[cfg(feature = "lang-dart")]
static DART_CONFIG: LanguageConfig = LanguageConfig {
    id: "dart",
    extensions: &[".dart"],
    entity_node_types: &[
        "class_declaration",
        "mixin_declaration",
        "extension_declaration",
        "extension_type_declaration",
        "enum_declaration",
        "type_alias",
        "class_member",
        "function_signature",
        "getter_signature",
        "setter_signature",
    ],
    container_node_types: &["class_body", "enum_body", "extension_body"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_dart,
    scope_resolve: Some(&DART_SCOPE_CONFIG),
};

#[cfg(feature = "lang-perl")]
static PERL_CONFIG: LanguageConfig = LanguageConfig {
    id: "perl",
    extensions: &[".pl", ".pm", ".t"],
    entity_node_types: &["subroutine_declaration_statement", "package_statement"],
    container_node_types: &["block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_perl,
    scope_resolve: None,
};

#[cfg(feature = "lang-sql")]
static SQL_CONFIG: LanguageConfig = LanguageConfig {
    id: "sql",
    extensions: &[".sql", ".psql", ".pgsql", ".ddl"],
    // DerekStride/tree-sitter-sql emits a dedicated create_* node per DDL
    // object; the object name lives in an `object_reference > identifier`
    // child, which the generic name extractor picks up.
    entity_node_types: &[
        "create_table",
        "create_view",
        "create_materialized_view",
        "create_function",
        "create_index",
        "create_type",
        "create_schema",
        "create_trigger",
        "create_sequence",
        "create_database",
    ],
    container_node_types: &[],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_sql,
    scope_resolve: None,
};

#[cfg(feature = "lang-ocaml")]
static OCAML_CONFIG: LanguageConfig = LanguageConfig {
    id: "ocaml",
    extensions: &[".ml"],
    entity_node_types: &[
        "value_definition",
        "module_definition",
        "module_type_definition",
        "type_definition",
        "exception_definition",
        "class_definition",
        "class_type_definition",
        "external",
    ],
    container_node_types: &["structure", "module_binding"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_ocaml,
    scope_resolve: None,
};

#[cfg(feature = "lang-ocaml")]
static OCAML_INTERFACE_CONFIG: LanguageConfig = LanguageConfig {
    id: "ocaml_interface",
    extensions: &[".mli"],
    entity_node_types: &[
        "value_specification",
        "module_definition",
        "module_type_definition",
        "type_definition",
        "exception_definition",
        "class_definition",
        "class_type_definition",
        "external",
    ],
    container_node_types: &["signature", "module_binding"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_ocaml_interface,
    scope_resolve: None,
};

#[cfg(feature = "lang-scala")]
static SCALA_CONFIG: LanguageConfig = LanguageConfig {
    id: "scala",
    extensions: &[".scala", ".sc", ".sbt", ".kojo", ".mill"],
    entity_node_types: &[
        "class_definition",
        "object_definition",
        "trait_definition",
        "enum_definition",
        "function_definition",
        "function_declaration",
        "val_definition",
        "given_definition",
        "extension_definition",
        "type_definition",
        "package_object",
    ],
    container_node_types: &["template_body", "enum_body", "with_template_body"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_scala,
    scope_resolve: Some(&SCALA_SCOPE_CONFIG),
};

#[cfg(feature = "lang-zig")]
static ZIG_CONFIG: LanguageConfig = LanguageConfig {
    id: "zig",
    extensions: &[".zig"],
    entity_node_types: &[
        "function_declaration",
        "test_declaration",
        "variable_declaration",
    ],
    container_node_types: &["block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[SuppressedNestedEntity {
        parent_entity_node_type: "function_declaration",
        child_entity_node_type: "variable_declaration",
    }],
    scope_boundary_types: &[],
    get_language: get_zig,
    scope_resolve: Some(&ZIG_SCOPE_CONFIG),
};

#[cfg(feature = "lang-nix")]
static NIX_CONFIG: LanguageConfig = LanguageConfig {
    id: "nix",
    extensions: &[".nix"],
    entity_node_types: &["binding", "inherit", "inherit_from"],
    container_node_types: &["binding_set"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_nix,
    scope_resolve: None,
};

#[cfg(feature = "lang-haskell")]
static HASKELL_CONFIG: LanguageConfig = LanguageConfig {
    id: "haskell",
    extensions: &[".hs"],
    entity_node_types: &[
        "function",        // top-level function definitions
        "signature",       // type signatures (e.g. foo :: Int -> Int)
        "data_type",       // data declarations
        "newtype",         // newtype declarations
        "class",           // type class declarations
        "instance",        // instance declarations
        "type_synomym",    // type aliases (note: typo in grammar)
        "foreign_import",  // FFI imports
        "foreign_export",  // FFI exports
        "pattern_synonym", // pattern synonyms
        "type_family",     // type families
        "data_family",     // data families
        "fixity",          // fixity declarations
    ],
    container_node_types: &["declarations", "class_body", "instance_body"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &["function"],
    get_language: get_haskell,
    scope_resolve: None,
};

#[cfg(feature = "lang-elm")]
static ELM_CONFIG: LanguageConfig = LanguageConfig {
    id: "elm",
    extensions: &[".elm"],
    entity_node_types: &[
        "value_declaration",
        "type_alias_declaration",
        "type_declaration",
        "port_annotation",
        "infix_declaration",
    ],
    container_node_types: &[],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &["value_declaration"],
    get_language: get_elm,
    scope_resolve: None,
};

// EDN (Extensible Data Notation) shares Clojure's syntax and grammar.
// Entities are top-level map entries (keyword key → value), extracted via the
// map_lit branch in visit_node when `LanguageConfig::extract_map_entries` is true.
// Non-map top-level forms (bare vecs, sets, lists) have no nameable identity
// and are not extracted.
#[cfg(feature = "lang-edn")]
static EDN_CONFIG: LanguageConfig = LanguageConfig {
    id: "edn",
    extensions: &[".edn"],
    entity_node_types: &[],
    container_node_types: &[],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_clojure,
    scope_resolve: None,
};

// Clojure is a Lisp: all definition forms are `list_lit` nodes whose first named
// child is a `sym_lit` identifying the macro (defn, ns, defrecord, etc.).
// The entity extractor handles these via the list_lit branch in visit_node.
#[cfg(feature = "lang-clojure")]
static CLOJURE_CONFIG: LanguageConfig = LanguageConfig {
    id: "clojure",
    extensions: &[".clj", ".cljs", ".cljc"],
    entity_node_types: &[],
    container_node_types: &[],
    call_entity_identifiers: &[
        "def",
        "defonce",
        "defn",
        "defn-",
        "defmacro",
        "defmulti",
        "defmethod",
        "defprotocol",
        "defrecord",
        "deftype",
        "definterface",
        "defstruct",
    ],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_clojure,
    scope_resolve: None,
};

#[cfg(feature = "lang-d")]
static D_CONFIG: LanguageConfig = LanguageConfig {
    id: "d",
    extensions: &[".d", ".di"],
    entity_node_types: &[
        "module_declaration",
        "function_declaration",
        "class_declaration",
        "struct_declaration",
        "interface_declaration",
        "union_declaration",
        "enum_declaration",
        "anonymous_enum_declaration",
        "template_declaration",
        "mixin_template_declaration",
        "constructor",
        "destructor",
        "postblit",
        "alias_declaration",
        "unittest_declaration",
        "variable_declaration",
        "manifest_constant",
        "auto_declaration",
    ],
    container_node_types: &["aggregate_body"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[
        SuppressedNestedEntity {
            parent_entity_node_type: "function_declaration",
            child_entity_node_type: "variable_declaration",
        },
        SuppressedNestedEntity {
            parent_entity_node_type: "function_declaration",
            child_entity_node_type: "auto_declaration",
        },
        SuppressedNestedEntity {
            parent_entity_node_type: "constructor",
            child_entity_node_type: "variable_declaration",
        },
        SuppressedNestedEntity {
            parent_entity_node_type: "destructor",
            child_entity_node_type: "variable_declaration",
        },
        SuppressedNestedEntity {
            parent_entity_node_type: "unittest_declaration",
            child_entity_node_type: "variable_declaration",
        },
        SuppressedNestedEntity {
            parent_entity_node_type: "unittest_declaration",
            child_entity_node_type: "auto_declaration",
        },
    ],
    scope_boundary_types: &["function_body", "block_statement"],
    get_language: get_d,
    scope_resolve: None,
};

// ─── Scope Resolve Configs for Supported Languages ────────────────────────────

static PYTHON_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["class_definition"],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_definition"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[
        AssignmentRule {
            node_kind: "assignment",
            strategy: AssignmentStrategy::LeftRight,
        },
        AssignmentRule {
            node_kind: "expression_statement",
            strategy: AssignmentStrategy::LeftRight,
        },
    ],
    assignment_recurse_into: &["block"],

    param_rules: &[
        ParamRule {
            node_kind: "typed_parameter",
            name_field: ParamNameField::WithFallback("name"),
            type_field: "type",
            skip_names: &["self", "cls"],
        },
        ParamRule {
            node_kind: "typed_default_parameter",
            name_field: ParamNameField::WithFallback("name"),
            type_field: "type",
            skip_names: &["self", "cls"],
        },
    ],

    return_type_field: None,

    call_nodes: &["call"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "attribute",
        object_field: "object",
        property_field: "attribute",
    }],
    scoped_call_nodes: &[],

    self_keywords: &["self", "cls"],

    init_strategy: InitStrategy::ConstructorBody {
        class_nodes: &["class_definition"],
        init_names: &["__init__"],
        init_node_kind: "function_definition",
        self_keyword: "self",
        access_kind: "attribute",
        obj_field: "object",
        prop_field: "attribute",
    },

    import_extractor: None, // set via import_rules
    external_method: false,

    builtins: &[
        "print",
        "len",
        "range",
        "str",
        "int",
        "float",
        "bool",
        "list",
        "dict",
        "set",
        "tuple",
        "type",
        "super",
        "isinstance",
        "issubclass",
        "getattr",
        "setattr",
        "hasattr",
        "delattr",
        "open",
        "input",
        "map",
        "filter",
        "zip",
        "enumerate",
        "sorted",
        "reversed",
        "min",
        "max",
        "sum",
        "any",
        "all",
        "abs",
        "round",
        "format",
        "repr",
        "id",
        "hash",
        "ValueError",
        "TypeError",
        "KeyError",
        "RuntimeError",
        "Exception",
        "StopIteration",
    ],
};

static TS_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["class_declaration", "abstract_class_declaration"],
    impl_scope_nodes: &[],
    function_scope_nodes: &[
        "function_declaration",
        "method_definition",
        "arrow_function",
    ],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[
        AssignmentRule {
            node_kind: "lexical_declaration",
            strategy: AssignmentStrategy::Declarators,
        },
        AssignmentRule {
            node_kind: "variable_declaration",
            strategy: AssignmentStrategy::Declarators,
        },
        AssignmentRule {
            node_kind: "expression_statement",
            strategy: AssignmentStrategy::LeftRight,
        },
    ],
    assignment_recurse_into: &["statement_block"],

    param_rules: &[
        ParamRule {
            node_kind: "required_parameter",
            name_field: ParamNameField::WithFallback("pattern"),
            type_field: "type",
            skip_names: &["this"],
        },
        ParamRule {
            node_kind: "optional_parameter",
            name_field: ParamNameField::WithFallback("pattern"),
            type_field: "type",
            skip_names: &["this"],
        },
    ],

    return_type_field: Some("return_type"),

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &["new_expression"],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "member_expression",
        object_field: "object",
        property_field: "property",
    }],
    scoped_call_nodes: &[],

    self_keywords: &["this"],

    init_strategy: InitStrategy::ConstructorBody {
        class_nodes: &["class_declaration", "abstract_class_declaration"],
        init_names: &["constructor"],
        init_node_kind: "method_definition",
        self_keyword: "this",
        access_kind: "member_expression",
        obj_field: "object",
        prop_field: "property",
    },

    import_extractor: None,
    external_method: false,

    builtins: &[
        "console",
        "parseInt",
        "parseFloat",
        "isNaN",
        "isFinite",
        "setTimeout",
        "setInterval",
        "clearTimeout",
        "clearInterval",
        "Promise",
        "Array",
        "Object",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "JSON",
        "Math",
        "Date",
        "RegExp",
        "Error",
        "TypeError",
        "RangeError",
        "Symbol",
        "Proxy",
        "Reflect",
        "String",
        "Number",
        "Boolean",
        "BigInt",
        "require",
        "module",
        "exports",
        "process",
        "Buffer",
        "global",
        "window",
        "document",
        "fetch",
        "Response",
        "Request",
        "Headers",
        "URL",
        "undefined",
        "encodeURIComponent",
        "decodeURIComponent",
        "encodeURI",
        "decodeURI",
        "AbortController",
        "TextEncoder",
        "TextDecoder",
        "Uint8Array",
        "Int8Array",
        "Float32Array",
        "ArrayBuffer",
        "DataView",
        "ReadableStream",
        "WritableStream",
        "Blob",
        "File",
        "FormData",
        "URLSearchParams",
        "Event",
        "EventTarget",
        "CustomEvent",
        "queueMicrotask",
        "structuredClone",
        "atob",
        "btoa",
        "crypto",
        "performance",
        "navigator",
    ],
};

static RUST_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["struct_item"],
    impl_scope_nodes: &["impl_item"],
    function_scope_nodes: &["function_item"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[AssignmentRule {
        node_kind: "let_declaration",
        strategy: AssignmentStrategy::PatternBased,
    }],
    assignment_recurse_into: &["block", "expression_statement"],

    param_rules: &[ParamRule {
        node_kind: "parameter",
        name_field: ParamNameField::RustPattern,
        type_field: "type",
        skip_names: &["self"],
    }],

    return_type_field: Some("return_type"),

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "field_expression",
        object_field: "value",
        property_field: "field",
    }],
    scoped_call_nodes: &["scoped_identifier"],

    self_keywords: &["self"],

    init_strategy: InitStrategy::StructFields {
        struct_nodes: &["struct_item"],
    },

    import_extractor: None,
    external_method: false,

    builtins: &[
        "println",
        "eprintln",
        "print",
        "eprint",
        "dbg",
        "format",
        "write",
        "writeln",
        "vec",
        "panic",
        "todo",
        "unimplemented",
        "unreachable",
        "assert",
        "assert_eq",
        "assert_ne",
        "debug_assert",
        "Some",
        "None",
        "Ok",
        "Err",
        "Box",
        "Vec",
        "String",
        "HashMap",
        "HashSet",
        "Arc",
        "Rc",
        "Mutex",
        "RwLock",
        "Cell",
        "RefCell",
        "Option",
        "Result",
        "Iterator",
        "IntoIterator",
        "Clone",
        "Copy",
        "Debug",
        "Display",
        "Default",
        "From",
        "Into",
        "TryFrom",
        "TryInto",
        "Send",
        "Sync",
        "Sized",
        "Unpin",
        "cfg",
        "derive",
        "include",
        "env",
    ],
};

static GO_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["type_declaration"],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_declaration", "method_declaration"],
    class_name_field: ClassNameField::TypeSpec {
        spec_kind: "type_spec",
        field: "name",
    },

    assignment_rules: &[
        AssignmentRule {
            node_kind: "short_var_declaration",
            strategy: AssignmentStrategy::ShortVar,
        },
        AssignmentRule {
            node_kind: "var_declaration",
            strategy: AssignmentStrategy::VarSpec,
        },
    ],
    assignment_recurse_into: &["block"],

    param_rules: &[ParamRule {
        node_kind: "parameter_declaration",
        name_field: ParamNameField::Simple("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("result"),

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &["composite_literal"],
    member_access: &[MemberAccess {
        node_kind: "selector_expression",
        object_field: "operand",
        property_field: "field",
    }],
    scoped_call_nodes: &[],

    self_keywords: &[],

    init_strategy: InitStrategy::StructFields {
        struct_nodes: &["type_declaration"],
    },

    import_extractor: None,
    external_method: true,

    builtins: &[
        "fmt",
        "log",
        "os",
        "io",
        "strings",
        "strconv",
        "bytes",
        "make",
        "len",
        "cap",
        "append",
        "copy",
        "delete",
        "close",
        "panic",
        "recover",
        "new",
        "print",
        "println",
        "error",
        "string",
        "int",
        "int8",
        "int16",
        "int32",
        "int64",
        "uint",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "float32",
        "float64",
        "complex64",
        "complex128",
        "bool",
        "byte",
        "rune",
        "uintptr",
        "Println",
        "Printf",
        "Sprintf",
        "Fprintf",
        "Errorf",
    ],
};

// ─── Tier 1 Scope Resolve Configs ─────────────────────────────────────────────

static JAVA_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &[
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
    ],
    impl_scope_nodes: &[],
    function_scope_nodes: &["method_declaration", "constructor_declaration"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[
        AssignmentRule {
            node_kind: "local_variable_declaration",
            strategy: AssignmentStrategy::Declarators,
        },
        AssignmentRule {
            node_kind: "expression_statement",
            strategy: AssignmentStrategy::LeftRight,
        },
    ],
    assignment_recurse_into: &["block"],

    param_rules: &[ParamRule {
        node_kind: "formal_parameter",
        name_field: ParamNameField::Simple("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("type"),

    call_nodes: &["method_invocation"],
    call_style: CallNodeStyle::DirectMethod {
        object_field: "object",
        method_field: "name",
    },
    new_expr_nodes: &["object_creation_expression"],
    new_expr_type_field: "type",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "method_invocation",
        object_field: "object",
        property_field: "name",
    }],
    scoped_call_nodes: &[],

    self_keywords: &["this"],

    init_strategy: InitStrategy::ClassFields {
        class_nodes: &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
    },
    import_extractor: None,
    external_method: false,

    builtins: &[
        "System",
        "String",
        "Integer",
        "Long",
        "Double",
        "Float",
        "Boolean",
        "Object",
        "Class",
        "Math",
        "Collections",
        "Arrays",
        "List",
        "Map",
        "Set",
        "ArrayList",
        "HashMap",
        "HashSet",
        "Optional",
        "Stream",
        "Exception",
        "RuntimeException",
        "NullPointerException",
        "println",
        "printf",
        "format",
    ],
};

static CSHARP_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &[
        "class_declaration",
        "interface_declaration",
        "struct_declaration",
        "enum_declaration",
    ],
    impl_scope_nodes: &[],
    function_scope_nodes: &["method_declaration", "constructor_declaration"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[
        AssignmentRule {
            node_kind: "local_declaration_statement",
            strategy: AssignmentStrategy::Declarators,
        },
        AssignmentRule {
            node_kind: "expression_statement",
            strategy: AssignmentStrategy::LeftRight,
        },
    ],
    assignment_recurse_into: &["block"],

    param_rules: &[ParamRule {
        node_kind: "parameter",
        name_field: ParamNameField::Simple("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("type"),

    call_nodes: &["invocation_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &["object_creation_expression"],
    new_expr_type_field: "type",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "member_access_expression",
        object_field: "expression",
        property_field: "name",
    }],
    scoped_call_nodes: &[],

    self_keywords: &["this"],

    init_strategy: InitStrategy::None,
    import_extractor: None,
    external_method: false,

    builtins: &[
        "Console",
        "String",
        "Int32",
        "Int64",
        "Double",
        "Boolean",
        "Object",
        "Math",
        "List",
        "Dictionary",
        "HashSet",
        "Task",
        "Async",
        "Exception",
        "ArgumentException",
        "WriteLine",
        "ReadLine",
        "ToString",
        "Equals",
    ],
};

static CPP_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["class_specifier", "struct_specifier"],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_definition"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[
        AssignmentRule {
            node_kind: "declaration",
            strategy: AssignmentStrategy::Declarators,
        },
        AssignmentRule {
            node_kind: "expression_statement",
            strategy: AssignmentStrategy::LeftRight,
        },
    ],
    assignment_recurse_into: &["compound_statement"],

    param_rules: &[ParamRule {
        node_kind: "parameter_declaration",
        name_field: ParamNameField::Simple("declarator"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("type"),

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &["new_expression"],
    new_expr_type_field: "type",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "field_expression",
        object_field: "argument",
        property_field: "field",
    }],
    scoped_call_nodes: &["qualified_identifier"],

    self_keywords: &["this"],

    init_strategy: InitStrategy::StructFields {
        struct_nodes: &["class_specifier", "struct_specifier"],
    },
    import_extractor: None,
    external_method: false,

    builtins: &[
        "std",
        "cout",
        "cin",
        "endl",
        "printf",
        "scanf",
        "malloc",
        "free",
        "string",
        "vector",
        "map",
        "set",
        "pair",
        "make_pair",
        "shared_ptr",
        "unique_ptr",
        "make_shared",
        "make_unique",
        "nullptr",
        "size_t",
        "int",
        "char",
        "double",
        "float",
        "bool",
    ],
};

static RUBY_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["class", "module"],
    impl_scope_nodes: &[],
    function_scope_nodes: &["method", "singleton_method"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[AssignmentRule {
        node_kind: "assignment",
        strategy: AssignmentStrategy::LeftRight,
    }],
    assignment_recurse_into: &["body_statement"],

    param_rules: &[],

    return_type_field: None,

    call_nodes: &["call"],
    call_style: CallNodeStyle::DirectMethod {
        object_field: "receiver",
        method_field: "method",
    },
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "call",
        object_field: "receiver",
        property_field: "method",
    }],
    scoped_call_nodes: &["scope_resolution"],

    self_keywords: &["self"],

    init_strategy: InitStrategy::None,
    import_extractor: None,
    external_method: false,

    builtins: &[
        "puts",
        "print",
        "p",
        "require",
        "require_relative",
        "include",
        "extend",
        "attr_accessor",
        "attr_reader",
        "attr_writer",
        "raise",
        "rescue",
        "yield",
        "block_given?",
        "Array",
        "Hash",
        "String",
        "Integer",
        "Float",
        "Symbol",
        "nil",
        "true",
        "false",
    ],
};

static KOTLIN_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &[
        "class_declaration",
        "object_declaration",
        "companion_object",
    ],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_declaration", "secondary_constructor"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[AssignmentRule {
        node_kind: "property_declaration",
        strategy: AssignmentStrategy::Declarators,
    }],
    assignment_recurse_into: &["statements", "block", "function_body"],

    // tree-sitter-kotlin-ng exposes `parameter` children positionally (no
    // `name`/`type` fields), so fall back to the first identifier child for the
    // name and let scan_function_params' user_type child-walk find the type.
    param_rules: &[ParamRule {
        node_kind: "parameter",
        name_field: ParamNameField::WithFallback("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("type"),

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FirstChild,
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "navigation_expression",
        object_field: "expression",
        property_field: "navigation_suffix",
    }],
    scoped_call_nodes: &[],

    self_keywords: &["this"],

    init_strategy: InitStrategy::ConstructorBody {
        class_nodes: &["class_declaration"],
        init_names: &["init"],
        init_node_kind: "anonymous_initializer",
        self_keyword: "this",
        access_kind: "navigation_expression",
        obj_field: "expression",
        prop_field: "navigation_suffix",
    },
    import_extractor: None,
    external_method: false,

    builtins: &[
        "println",
        "print",
        "listOf",
        "mapOf",
        "setOf",
        "arrayOf",
        "mutableListOf",
        "mutableMapOf",
        "mutableSetOf",
        "String",
        "Int",
        "Long",
        "Double",
        "Float",
        "Boolean",
        "Any",
        "Unit",
        "Nothing",
        "Pair",
        "Triple",
        "require",
        "check",
        "error",
        "TODO",
        "emptyList",
        "emptyMap",
        "emptySet",
        "lazy",
        "run",
        "let",
        "also",
        "apply",
        "with",
        "takeIf",
        "takeUnless",
        "Throwable",
        "Exception",
        "RuntimeException",
        "IllegalArgumentException",
        "IllegalStateException",
        "UnsupportedOperationException",
        "Regex",
        "Sequence",
        "Iterable",
        "Iterator",
        "coroutineScope",
        "launch",
        "async",
        "withContext",
        "runBlocking",
        "Flow",
        "StateFlow",
        "SharedFlow",
        "Dispatchers",
        "Job",
        "SupervisorJob",
        "CoroutineScope",
        "suspend",
        "Channel",
    ],
};

static PHP_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &[
        "class_declaration",
        "interface_declaration",
        "trait_declaration",
    ],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_definition", "method_declaration"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[AssignmentRule {
        node_kind: "expression_statement",
        strategy: AssignmentStrategy::LeftRight,
    }],
    assignment_recurse_into: &["compound_statement"],

    param_rules: &[ParamRule {
        node_kind: "simple_parameter",
        name_field: ParamNameField::Simple("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("return_type"),

    call_nodes: &["function_call_expression", "member_call_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &["object_creation_expression"],
    new_expr_type_field: "type",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "member_call_expression",
        object_field: "object",
        property_field: "name",
    }],
    scoped_call_nodes: &["scoped_call_expression"],

    self_keywords: &["$this"],

    init_strategy: InitStrategy::None,
    import_extractor: None,
    external_method: false,

    builtins: &[
        "echo",
        "print",
        "var_dump",
        "print_r",
        "isset",
        "unset",
        "empty",
        "array",
        "count",
        "strlen",
        "substr",
        "strpos",
        "is_null",
        "is_array",
        "is_string",
        "is_int",
        "Exception",
        "RuntimeException",
        "InvalidArgumentException",
    ],
};

static SWIFT_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &[
        "class_declaration",
        "protocol_declaration",
        "struct_declaration",
        "enum_declaration",
    ],
    impl_scope_nodes: &["extension_declaration"],
    function_scope_nodes: &[
        "function_declaration",
        "init_declaration",
        "deinit_declaration",
        "subscript_declaration",
        "property_declaration",
    ],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[AssignmentRule {
        node_kind: "property_declaration",
        strategy: AssignmentStrategy::Declarators,
    }],
    assignment_recurse_into: &[
        "function_body",
        "computed_property",
        "code_block",
        "statements",
        "if_statement",
        "guard_statement",
        "for_statement",
        "while_statement",
        "repeat_while_statement",
        "switch_statement",
    ],

    param_rules: &[ParamRule {
        node_kind: "parameter",
        name_field: ParamNameField::Simple("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("return_type"),

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FirstChild,
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "navigation_expression",
        object_field: "target",
        property_field: "suffix",
    }],
    scoped_call_nodes: &[],

    self_keywords: &["self"],

    init_strategy: InitStrategy::ConstructorBody {
        class_nodes: &["class_declaration", "struct_declaration"],
        init_names: &["init"],
        init_node_kind: "init_declaration",
        self_keyword: "self",
        access_kind: "navigation_expression",
        obj_field: "target",
        prop_field: "suffix",
    },
    import_extractor: None,
    external_method: false,

    builtins: &[
        "print",
        "debugPrint",
        "fatalError",
        "precondition",
        "assert",
        "String",
        "Int",
        "Double",
        "Float",
        "Bool",
        "Array",
        "Dictionary",
        "Set",
        "Optional",
        "Result",
        "Error",
        "NSError",
        "nil",
        "Any",
        "AnyObject",
        "Void",
        "Never",
        "Data",
        "URL",
        "URLRequest",
        "URLSession",
        "Codable",
        "Hashable",
        "Equatable",
        "Comparable",
        "Identifiable",
        "Task",
        "MainActor",
        "Sendable",
        "min",
        "max",
        "abs",
        "zip",
        "stride",
        "type",
        "DispatchQueue",
        "NotificationCenter",
        "UserDefaults",
        "NSObject",
        "Bundle",
        "FileManager",
        "true",
        "false",
    ],
};

static SCALA_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["class_definition", "object_definition", "trait_definition"],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_definition", "function_declaration"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[AssignmentRule {
        node_kind: "val_definition",
        strategy: AssignmentStrategy::Declarators,
    }],
    assignment_recurse_into: &["template_body"],

    param_rules: &[ParamRule {
        node_kind: "parameter",
        name_field: ParamNameField::Simple("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: Some("return_type"),

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "field_expression",
        object_field: "value",
        property_field: "field",
    }],
    scoped_call_nodes: &[],

    self_keywords: &["this"],

    init_strategy: InitStrategy::None,
    import_extractor: None,
    external_method: false,

    builtins: &[
        "println", "print", "require", "assert", "String", "Int", "Long", "Double", "Float",
        "Boolean", "List", "Map", "Set", "Seq", "Vector", "Option", "Some", "None", "Future",
        "Try", "Either", "Left", "Right",
    ],
};

// ─── Tier 2 Scope Resolve Configs (Minimal) ───────────────────────────────────

static DART_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &["class_declaration", "mixin_declaration", "enum_declaration"],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_signature", "method_signature"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[],
    assignment_recurse_into: &[],

    param_rules: &[ParamRule {
        node_kind: "formal_parameter",
        name_field: ParamNameField::Simple("name"),
        type_field: "type",
        skip_names: &[],
    }],

    return_type_field: None,

    call_nodes: &["function_expression_body"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[],
    scoped_call_nodes: &[],

    self_keywords: &["this"],

    init_strategy: InitStrategy::None,
    import_extractor: None,
    external_method: false,

    builtins: &[
        "print",
        "debugPrint",
        "String",
        "int",
        "double",
        "bool",
        "List",
        "Map",
        "Set",
        "Future",
        "Stream",
    ],
};

static ZIG_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &[],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_declaration", "test_declaration"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[],
    assignment_recurse_into: &[],

    param_rules: &[],

    return_type_field: None,

    call_nodes: &["call_expression"],
    call_style: CallNodeStyle::FunctionField("function"),
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[MemberAccess {
        node_kind: "field_expression",
        object_field: "object",
        property_field: "field",
    }],
    scoped_call_nodes: &[],

    self_keywords: &[],

    init_strategy: InitStrategy::None,
    import_extractor: None,
    external_method: false,

    builtins: &[
        "std",
        "print",
        "debug",
        "assert",
        "expect",
        "allocator",
        "mem",
        "testing",
    ],
};

static BASH_SCOPE_CONFIG: ScopeResolveConfig = ScopeResolveConfig {
    class_scope_nodes: &[],
    impl_scope_nodes: &[],
    function_scope_nodes: &["function_definition"],
    class_name_field: ClassNameField::Simple("name"),

    assignment_rules: &[],
    assignment_recurse_into: &[],

    param_rules: &[],

    return_type_field: None,

    call_nodes: &["command"],
    call_style: CallNodeStyle::FunctionField("name"),
    new_expr_nodes: &[],
    new_expr_type_field: "constructor",
    composite_literal_nodes: &[],
    member_access: &[],
    scoped_call_nodes: &[],

    self_keywords: &[],

    init_strategy: InitStrategy::None,
    import_extractor: None,
    external_method: false,

    builtins: &[
        "echo", "printf", "cd", "ls", "cat", "grep", "sed", "awk", "if", "then", "else", "fi",
        "for", "while", "do", "done", "exit", "return", "export", "source", "eval",
    ],
};

macro_rules! all_configs {
    () => {{
        &[
            #[cfg(feature = "lang-typescript")]
            &TYPESCRIPT_CONFIG,
            #[cfg(feature = "lang-typescript")]
            &TSX_CONFIG,
            #[cfg(feature = "lang-javascript")]
            &JAVASCRIPT_CONFIG,
            #[cfg(feature = "lang-python")]
            &PYTHON_CONFIG,
            #[cfg(feature = "lang-go")]
            &GO_CONFIG,
            #[cfg(feature = "lang-rust")]
            &RUST_CONFIG,
            #[cfg(feature = "lang-java")]
            &JAVA_CONFIG,
            #[cfg(feature = "lang-c")]
            &C_CONFIG,
            #[cfg(feature = "lang-cpp")]
            &CPP_CONFIG,
            #[cfg(feature = "lang-ruby")]
            &RUBY_CONFIG,
            #[cfg(feature = "lang-csharp")]
            &CSHARP_CONFIG,
            #[cfg(feature = "lang-php")]
            &PHP_CONFIG,
            #[cfg(feature = "lang-fortran")]
            &FORTRAN_CONFIG,
            #[cfg(feature = "lang-swift")]
            &SWIFT_CONFIG,
            #[cfg(feature = "lang-elixir")]
            &ELIXIR_CONFIG,
            #[cfg(feature = "lang-bash")]
            &BASH_CONFIG,
            #[cfg(feature = "lang-hcl")]
            &HCL_CONFIG,
            #[cfg(feature = "lang-kotlin")]
            &KOTLIN_CONFIG,
            #[cfg(feature = "lang-xml")]
            &XML_CONFIG,
            #[cfg(feature = "lang-dart")]
            &DART_CONFIG,
            #[cfg(feature = "lang-perl")]
            &PERL_CONFIG,
            #[cfg(feature = "lang-sql")]
            &SQL_CONFIG,
            #[cfg(feature = "lang-ocaml")]
            &OCAML_CONFIG,
            #[cfg(feature = "lang-ocaml")]
            &OCAML_INTERFACE_CONFIG,
            #[cfg(feature = "lang-scala")]
            &SCALA_CONFIG,
            #[cfg(feature = "lang-zig")]
            &ZIG_CONFIG,
            #[cfg(feature = "lang-nix")]
            &NIX_CONFIG,
            #[cfg(feature = "lang-haskell")]
            &HASKELL_CONFIG,
            #[cfg(feature = "lang-elm")]
            &ELM_CONFIG,
            #[cfg(feature = "lang-clojure")]
            &CLOJURE_CONFIG,
            #[cfg(feature = "lang-edn")]
            &EDN_CONFIG,
            #[cfg(feature = "lang-d")]
            &D_CONFIG,
        ]
    }};
}

static ALL_CONFIGS: &[&LanguageConfig] = all_configs!();

pub fn get_language_config(extension: &str) -> Option<&'static LanguageConfig> {
    ALL_CONFIGS
        .iter()
        .find(|c| c.extensions.contains(&extension))
        .copied()
}

pub fn get_all_code_extensions() -> &'static [&'static str] {
    // Derived from ALL_CONFIGS to avoid duplication drift.
    // When you add an extension to a LanguageConfig, it's automatically included here.
    static EXTENSIONS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
        ALL_CONFIGS
            .iter()
            .flat_map(|c| c.extensions.iter().copied())
            .collect()
    });
    &EXTENSIONS
}
