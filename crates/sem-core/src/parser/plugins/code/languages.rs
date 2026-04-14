use tree_sitter::Language;

pub struct SuppressedNestedEntity {
    pub parent_entity_node_type: &'static str,
    pub child_entity_node_type: &'static str,
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
}

fn get_typescript() -> Option<Language> {
    Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
}

fn get_tsx() -> Option<Language> {
    Some(tree_sitter_typescript::LANGUAGE_TSX.into())
}

fn get_javascript() -> Option<Language> {
    Some(tree_sitter_javascript::LANGUAGE.into())
}

fn get_python() -> Option<Language> {
    Some(tree_sitter_python::LANGUAGE.into())
}

fn get_go() -> Option<Language> {
    Some(tree_sitter_go::LANGUAGE.into())
}

fn get_rust() -> Option<Language> {
    Some(tree_sitter_rust::LANGUAGE.into())
}

fn get_java() -> Option<Language> {
    Some(tree_sitter_java::LANGUAGE.into())
}

fn get_c() -> Option<Language> {
    Some(tree_sitter_c::LANGUAGE.into())
}

fn get_cpp() -> Option<Language> {
    Some(tree_sitter_cpp::LANGUAGE.into())
}

fn get_ruby() -> Option<Language> {
    Some(tree_sitter_ruby::LANGUAGE.into())
}

fn get_csharp() -> Option<Language> {
    Some(tree_sitter_c_sharp::LANGUAGE.into())
}

fn get_php() -> Option<Language> {
    Some(tree_sitter_php::LANGUAGE_PHP.into())
}

fn get_fortran() -> Option<Language> {
    Some(tree_sitter_fortran::LANGUAGE.into())
}

fn get_swift() -> Option<Language> {
    Some(tree_sitter_swift::LANGUAGE.into())
}

fn get_elixir() -> Option<Language> {
    Some(tree_sitter_elixir::LANGUAGE.into())
}

fn get_bash() -> Option<Language> {
    Some(tree_sitter_bash::LANGUAGE.into())
}

fn get_hcl() -> Option<Language> {
    Some(tree_sitter_hcl::LANGUAGE.into())
}

fn get_kotlin() -> Option<Language> {
    Some(tree_sitter_kotlin_ng::LANGUAGE.into())
}

fn get_xml() -> Option<Language> {
    Some(tree_sitter_xml::LANGUAGE_XML.into())
}

fn get_dart() -> Option<Language> {
    Some(tree_sitter_dart::LANGUAGE.into())
}

fn get_perl() -> Option<Language> {
    Some(tree_sitter_perl_next::LANGUAGE.into())
}

fn get_ocaml() -> Option<Language> {
    Some(tree_sitter_ocaml::LANGUAGE_OCAML.into())
}

fn get_ocaml_interface() -> Option<Language> {
    Some(tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE.into())
}

fn get_scala() -> Option<Language> {
    Some(tree_sitter_scala::LANGUAGE.into())
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

static TYPESCRIPT_CONFIG: LanguageConfig = LanguageConfig {
    id: "typescript",
    extensions: &[".ts", ".mts", ".cts"],
    entity_node_types: &[
        "function_declaration",
        "generator_function_declaration",
        "class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
        "export_statement",
        "lexical_declaration",
        "variable_declaration",
        "method_definition",
        "public_field_definition",
    ],
    container_node_types: &["class_body", "interface_body", "enum_body", "statement_block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: JS_TS_SUPPRESSED_NESTED,
    scope_boundary_types: JS_TS_SCOPE_BOUNDARIES,
    get_language: get_typescript,
};

static TSX_CONFIG: LanguageConfig = LanguageConfig {
    id: "tsx",
    extensions: &[".tsx"],
    entity_node_types: &[
        "function_declaration",
        "generator_function_declaration",
        "class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
        "export_statement",
        "lexical_declaration",
        "variable_declaration",
        "method_definition",
        "public_field_definition",
    ],
    container_node_types: &["class_body", "interface_body", "enum_body", "statement_block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: JS_TS_SUPPRESSED_NESTED,
    scope_boundary_types: JS_TS_SCOPE_BOUNDARIES,
    get_language: get_tsx,
};

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
};

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
};

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
};

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
    ],
    container_node_types: &["declaration_list", "block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_rust,
};

static JAVA_CONFIG: LanguageConfig = LanguageConfig {
    id: "java",
    extensions: &[".java"],
    entity_node_types: &[
        "class_declaration",
        "method_declaration",
        "interface_declaration",
        "enum_declaration",
        "field_declaration",
        "constructor_declaration",
        "annotation_type_declaration",
    ],
    container_node_types: &["class_body", "interface_body", "enum_body", "block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_java,
};

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
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_c,
};

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
    container_node_types: &["field_declaration_list", "declaration_list", "compound_statement"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_cpp,
};

static RUBY_CONFIG: LanguageConfig = LanguageConfig {
    id: "ruby",
    extensions: &[".rb"],
    entity_node_types: &[
        "method",
        "singleton_method",
        "class",
        "module",
    ],
    container_node_types: &["body_statement"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_ruby,
};

static CSHARP_CONFIG: LanguageConfig = LanguageConfig {
    id: "csharp",
    extensions: &[".cs"],
    entity_node_types: &[
        "method_declaration",
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "struct_declaration",
        "namespace_declaration",
        "property_declaration",
        "constructor_declaration",
        "field_declaration",
    ],
    container_node_types: &["declaration_list", "block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_csharp,
};

static PHP_CONFIG: LanguageConfig = LanguageConfig {
    id: "php",
    extensions: &[".php"],
    entity_node_types: &[
        "function_definition",
        "class_declaration",
        "method_declaration",
        "interface_declaration",
        "trait_declaration",
        "enum_declaration",
        "namespace_definition",
    ],
    container_node_types: &["declaration_list", "enum_declaration_list", "compound_statement"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_php,
};

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
};

static SWIFT_CONFIG: LanguageConfig = LanguageConfig {
    id: "swift",
    extensions: &[".swift"],
    entity_node_types: &[
        "function_declaration",
        "class_declaration",
        "protocol_declaration",
        "init_declaration",
        "deinit_declaration",
        "subscript_declaration",
        "typealias_declaration",
        "property_declaration",
        "operator_declaration",
        "associatedtype_declaration",
    ],
    container_node_types: &["class_body", "protocol_body", "enum_class_body", "function_body"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_swift,
};

static ELIXIR_CONFIG: LanguageConfig = LanguageConfig {
    id: "elixir",
    extensions: &[".ex", ".exs"],
    entity_node_types: &[],
    container_node_types: &["do_block"],
    call_entity_identifiers: &[
        "defmodule", "def", "defp", "defmacro", "defmacrop",
        "defguard", "defguardp", "defprotocol", "defimpl",
        "defstruct", "defexception", "defdelegate",
    ],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_elixir,
};

static BASH_CONFIG: LanguageConfig = LanguageConfig {
    id: "bash",
    extensions: &[".sh"],
    entity_node_types: &["function_definition"],
    container_node_types: &["compound_statement"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_bash,
};

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
};

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
};

static XML_CONFIG: LanguageConfig = LanguageConfig {
    id: "xml",
    extensions: &[".xml", ".plist", ".svg", ".xhtml", ".csproj", ".fsproj", ".vbproj", ".props", ".targets", ".nuspec", ".resx", ".xaml", ".axml"],
    entity_node_types: &["element"],
    container_node_types: &["content"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_xml,
};

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
};
  
static PERL_CONFIG: LanguageConfig = LanguageConfig {
    id: "perl",
    extensions: &[".pl", ".pm", ".t"],
    entity_node_types: &[
        "subroutine_declaration_statement",
        "package_statement",
    ],
    container_node_types: &["block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    scope_boundary_types: &[],
    get_language: get_perl,
};

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
};

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
};

static SCALA_CONFIG: LanguageConfig = LanguageConfig {
    id: "scala",
    extensions: &[".scala", ".sc"],
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
};

static ALL_CONFIGS: &[&LanguageConfig] = &[
    &TYPESCRIPT_CONFIG,
    &TSX_CONFIG,
    &JAVASCRIPT_CONFIG,
    &PYTHON_CONFIG,
    &GO_CONFIG,
    &RUST_CONFIG,
    &JAVA_CONFIG,
    &C_CONFIG,
    &CPP_CONFIG,
    &RUBY_CONFIG,
    &CSHARP_CONFIG,
    &PHP_CONFIG,
    &FORTRAN_CONFIG,
    &SWIFT_CONFIG,
    &ELIXIR_CONFIG,
    &BASH_CONFIG,
    &HCL_CONFIG,
    &KOTLIN_CONFIG,
    &XML_CONFIG,
    &DART_CONFIG,
    &PERL_CONFIG,
    &OCAML_CONFIG,
    &OCAML_INTERFACE_CONFIG,
    &SCALA_CONFIG,
];

pub fn get_language_config(extension: &str) -> Option<&'static LanguageConfig> {
    ALL_CONFIGS
        .iter()
        .find(|c| c.extensions.contains(&extension))
        .copied()
}

pub fn get_all_code_extensions() -> &'static [&'static str] {
    // All unique extensions across all language configs
    static EXTENSIONS: &[&str] = &[
        ".ts",".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".py", ".pyi", ".go", ".rs", ".java", ".c", ".h",
        ".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx", ".rb", ".cs", ".php", ".f90", ".f95", ".f03",
        ".f08", ".f", ".for", ".swift", ".ex", ".exs", ".sh", ".hcl", ".tf", ".tfvars",
        ".kt", ".kts",
        ".xml", ".plist", ".svg", ".xhtml", ".csproj", ".fsproj", ".vbproj", ".props", ".targets",
        ".nuspec", ".resx", ".xaml", ".axml",
        ".dart",
        ".pl", ".pm", ".t",
        ".ml", ".mli",
        ".scala", ".sc",
    ];
    EXTENSIONS
}
