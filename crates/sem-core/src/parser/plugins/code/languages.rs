use tree_sitter::Language;

#[allow(dead_code)]
pub struct LanguageConfig {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
    pub entity_node_types: &'static [&'static str],
    pub container_node_types: &'static [&'static str],
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

static TYPESCRIPT_CONFIG: LanguageConfig = LanguageConfig {
    id: "typescript",
    extensions: &[".ts"],
    entity_node_types: &[
        "function_declaration",
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
    container_node_types: &["class_body", "interface_body", "enum_body"],
    get_language: get_typescript,
};

static TSX_CONFIG: LanguageConfig = LanguageConfig {
    id: "tsx",
    extensions: &[".tsx"],
    entity_node_types: &[
        "function_declaration",
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
    container_node_types: &["class_body", "interface_body", "enum_body"],
    get_language: get_tsx,
};

static JAVASCRIPT_CONFIG: LanguageConfig = LanguageConfig {
    id: "javascript",
    extensions: &[".js", ".jsx", ".mjs", ".cjs"],
    entity_node_types: &[
        "function_declaration",
        "class_declaration",
        "export_statement",
        "lexical_declaration",
        "variable_declaration",
        "method_definition",
        "field_definition",
    ],
    container_node_types: &["class_body"],
    get_language: get_javascript,
};

static PYTHON_CONFIG: LanguageConfig = LanguageConfig {
    id: "python",
    extensions: &[".py"],
    entity_node_types: &[
        "function_definition",
        "class_definition",
        "decorated_definition",
    ],
    container_node_types: &["block"],
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
    container_node_types: &[],
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
    container_node_types: &["declaration_list"],
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
    container_node_types: &["class_body", "interface_body", "enum_body"],
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
    container_node_types: &[],
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
    container_node_types: &["field_declaration_list", "declaration_list"],
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
    container_node_types: &["declaration_list"],
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
    container_node_types: &["declaration_list", "enum_declaration_list"],
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
    container_node_types: &[],
    get_language: get_fortran,
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
        ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py", ".go", ".rs",
        ".java", ".c", ".h", ".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx",
        ".rb", ".cs", ".php", ".f90", ".f95", ".f03", ".f08", ".f", ".for",
    ];
    EXTENSIONS
}
