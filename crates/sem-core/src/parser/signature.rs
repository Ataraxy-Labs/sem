//! Signature-level change detection: re-parses before/after entity content
//! with tree-sitter to extract parameter lists and classify whether a change
//! is breaking (params removed/reordered), non-breaking (params added with
//! defaults), or body-only.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use tree_sitter::{Language, Node, Parser};

use crate::parser::plugins::code::languages::get_language_config;

/// Classification of a signature change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureChangeKind {
    /// Only the function body changed; signature is identical.
    BodyOnly,
    /// Parameters were added but none removed/reordered (non-breaking).
    ParamsAdded { added: Vec<String> },
    /// Parameters were removed (breaking).
    ParamsRemoved { removed: Vec<String> },
    /// Parameters were reordered (breaking).
    ParamsReordered,
    /// Return type changed.
    ReturnTypeChanged { before: String, after: String },
    /// Signature changed but we couldn't determine how (fallback).
    Unknown,
    /// Not a function/method — no signature to compare.
    NotApplicable,
}

impl SignatureChangeKind {
    pub fn is_breaking(&self) -> bool {
        matches!(
            self,
            SignatureChangeKind::ParamsRemoved { .. }
                | SignatureChangeKind::ParamsReordered
        )
    }

    pub fn label(&self) -> &str {
        match self {
            SignatureChangeKind::BodyOnly => "body only",
            SignatureChangeKind::ParamsAdded { .. } => "parameter added",
            SignatureChangeKind::ParamsRemoved { .. } => "parameter removed",
            SignatureChangeKind::ParamsReordered => "parameters reordered",
            SignatureChangeKind::ReturnTypeChanged { .. } => "return type changed",
            SignatureChangeKind::Unknown => "signature changed",
            SignatureChangeKind::NotApplicable => "modified",
        }
    }
}

// Thread-local parser cache.
thread_local! {
    static SIG_PARSER_CACHE: RefCell<HashMap<String, Parser>> = RefCell::new(HashMap::new());
}

/// Analyze the signature change between before and after entity content.
/// `file_path` is used to determine the language.
pub fn analyze_signature_change(
    before_content: &str,
    after_content: &str,
    file_path: &str,
) -> SignatureChangeKind {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    let config = match get_language_config(&ext) {
        Some(c) => c,
        None => return SignatureChangeKind::NotApplicable,
    };

    let language = match (config.get_language)() {
        Some(lang) => lang,
        None => return SignatureChangeKind::NotApplicable,
    };

    let before_tree = parse_content(before_content, &language, &ext);
    let after_tree = parse_content(after_content, &language, &ext);

    let (before_tree, after_tree) = match (before_tree, after_tree) {
        (Some(b), Some(a)) => (b, a),
        _ => return SignatureChangeKind::Unknown,
    };

    let before_root = before_tree.root_node();
    let after_root = after_tree.root_node();

    // Find the first function-like node in each tree
    let before_fn = find_function_node(before_root);
    let after_fn = find_function_node(after_root);

    let (before_fn, after_fn) = match (before_fn, after_fn) {
        (Some(b), Some(a)) => (b, a),
        _ => return SignatureChangeKind::NotApplicable,
    };

    // Extract parameter names
    let before_params = extract_param_names(before_fn, before_content.as_bytes());
    let after_params = extract_param_names(after_fn, after_content.as_bytes());

    // Extract return types if available
    let before_ret = extract_return_type(before_fn, before_content.as_bytes());
    let after_ret = extract_return_type(after_fn, after_content.as_bytes());

    // Compare signatures
    if before_params == after_params {
        // Parameters are identical — check return type
        if before_ret != after_ret {
            if let (Some(br), Some(ar)) = (before_ret, after_ret) {
                return SignatureChangeKind::ReturnTypeChanged {
                    before: br,
                    after: ar,
                };
            }
        }
        // Same params, same return type (or both absent) → body only
        return SignatureChangeKind::BodyOnly;
    }

    // Check for removed params
    let removed: Vec<String> = before_params
        .iter()
        .filter(|p| !after_params.contains(p))
        .cloned()
        .collect();

    if !removed.is_empty() {
        return SignatureChangeKind::ParamsRemoved { removed };
    }

    // Check for added params (all old params still present)
    let added: Vec<String> = after_params
        .iter()
        .filter(|p| !before_params.contains(p))
        .cloned()
        .collect();

    if !added.is_empty() {
        // Verify the existing params are still in the same order
        let before_order: Vec<&String> = before_params
            .iter()
            .filter(|p| after_params.contains(p))
            .collect();
        let after_order: Vec<&String> = after_params
            .iter()
            .filter(|p| before_params.contains(p))
            .collect();

        if before_order == after_order {
            return SignatureChangeKind::ParamsAdded { added };
        } else {
            return SignatureChangeKind::ParamsReordered;
        }
    }

    // Same set of param names but different order
    SignatureChangeKind::ParamsReordered
}

fn parse_content(content: &str, language: &Language, ext: &str) -> Option<tree_sitter::Tree> {
    SIG_PARSER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = cache.entry(ext.to_string()).or_insert_with(|| {
            let mut p = Parser::new();
            let _ = p.set_language(language);
            p
        });
        parser.parse(content.as_bytes(), None)
    })
}

/// Find the first function-like AST node in the tree.
fn find_function_node(node: Node) -> Option<Node> {
    let kind = node.kind();

    // Direct function node types across languages
    const FUNCTION_KINDS: &[&str] = &[
        // TS/JS/Python
        "function_declaration",
        "function_definition",
        "method_definition",
        "arrow_function",
        // Rust
        "function_item",
        // Go
        "method_declaration",
        // Java/C#/PHP
        "method_declaration",
        "constructor_declaration",
        // Swift
        "function_declaration",
        "init_declaration",
    ];

    if FUNCTION_KINDS.contains(&kind) {
        return Some(node);
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_function_node(child) {
            return Some(found);
        }
    }

    None
}

/// Extract parameter names from a function-like node.
fn extract_param_names(func_node: Node, source: &[u8]) -> Vec<String> {
    let mut params = Vec::new();

    // Try common field names for parameter lists
    let param_list = func_node
        .child_by_field_name("parameters")
        .or_else(|| func_node.child_by_field_name("formal_parameters"))
        .or_else(|| {
            // C/C++: parameters are inside the declarator
            func_node
                .child_by_field_name("declarator")
                .and_then(|d| d.child_by_field_name("parameters"))
        });

    let param_list = match param_list {
        Some(pl) => pl,
        None => return params,
    };

    let mut cursor = param_list.walk();
    for child in param_list.named_children(&mut cursor) {
        if let Some(name) = extract_single_param_name(child, source) {
            params.push(name);
        }
    }

    params
}

/// Extract the name from a single parameter node.
fn extract_single_param_name(node: Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();

    // Skip non-parameter nodes (commas, parentheses, etc.)
    match kind {
        // Direct parameter types across languages
        "required_parameter"
        | "optional_parameter"
        | "formal_parameter"
        | "parameter"
        | "parameter_declaration"
        | "identifier"
        | "simple_parameter"
        | "typed_parameter"
        | "typed_default_parameter"
        | "default_parameter"
        | "list_splat_parameter"
        | "dictionary_splat_parameter"
        | "keyword_parameter"
        | "variadic_parameter" => {}
        // Rust-specific
        "self_parameter" => {
            return Some("self".to_string());
        }
        _ => return None,
    }

    // If the node is itself an identifier, that's the name
    if kind == "identifier" {
        return Some(node_text(node, source).to_string());
    }

    // Try 'name' field
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(name_node, source).to_string());
    }

    // Try 'pattern' field (Rust, TS destructuring)
    if let Some(pattern) = node.child_by_field_name("pattern") {
        if pattern.kind() == "identifier" {
            return Some(node_text(pattern, source).to_string());
        }
        // For Rust, pattern might be a reference_pattern
        if let Some(inner) = pattern.child_by_field_name("name") {
            return Some(node_text(inner, source).to_string());
        }
        // Fallback: use the pattern text
        return Some(node_text(pattern, source).to_string());
    }

    // Try 'declarator' field (C/C++)
    if let Some(declarator) = node.child_by_field_name("declarator") {
        return Some(extract_declarator_identifier(declarator, source));
    }

    // Fallback: first identifier child
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(node_text(child, source).to_string());
        }
    }

    // Last resort: whole node text (for very simple cases)
    let text = node_text(node, source).trim().to_string();
    if !text.is_empty() && text.len() < 50 {
        Some(text)
    } else {
        None
    }
}

/// Extract identifier from C/C++ declarator chain.
fn extract_declarator_identifier(node: Node, source: &[u8]) -> String {
    match node.kind() {
        "identifier" => node_text(node, source).to_string(),
        "pointer_declarator" | "reference_declarator" | "array_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                extract_declarator_identifier(inner, source)
            } else {
                node_text(node, source).to_string()
            }
        }
        _ => node_text(node, source).to_string(),
    }
}

/// Extract return type annotation if present.
fn extract_return_type(func_node: Node, source: &[u8]) -> Option<String> {
    // Try 'return_type' field (TS, Rust, Go, Swift)
    if let Some(ret) = func_node.child_by_field_name("return_type") {
        return Some(node_text(ret, source).trim().to_string());
    }
    // Try 'type' field (some grammars use this for return type)
    // Java/C uses 'type' on the function node itself for the return type
    if let Some(ret) = func_node.child_by_field_name("type") {
        return Some(node_text(ret, source).trim().to_string());
    }
    None
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_body_only_change() {
        let before = "function greet(name: string): string { return 'hello ' + name; }";
        let after = "function greet(name: string): string { return 'hi ' + name; }";
        let result = analyze_signature_change(before, after, "test.ts");
        assert_eq!(result, SignatureChangeKind::BodyOnly);
    }

    #[test]
    fn test_param_added() {
        let before = "function greet(name: string): string { return name; }";
        let after =
            "function greet(name: string, formal: boolean): string { return name; }";
        let result = analyze_signature_change(before, after, "test.ts");
        match result {
            SignatureChangeKind::ParamsAdded { added } => {
                assert_eq!(added, vec!["formal"]);
            }
            other => panic!("Expected ParamsAdded, got {:?}", other),
        }
    }

    #[test]
    fn test_param_removed() {
        let before =
            "function greet(name: string, formal: boolean): string { return name; }";
        let after = "function greet(name: string): string { return name; }";
        let result = analyze_signature_change(before, after, "test.ts");
        match result {
            SignatureChangeKind::ParamsRemoved { removed } => {
                assert_eq!(removed, vec!["formal"]);
            }
            other => panic!("Expected ParamsRemoved, got {:?}", other),
        }
    }

    #[test]
    fn test_params_reordered() {
        let before = "function greet(a: string, b: number): void {}";
        let after = "function greet(b: number, a: string): void {}";
        let result = analyze_signature_change(before, after, "test.ts");
        assert_eq!(result, SignatureChangeKind::ParamsReordered);
    }

    #[test]
    fn test_python_param_added() {
        let before = "def greet(name):\n    return name";
        let after = "def greet(name, formal=True):\n    return name";
        let result = analyze_signature_change(before, after, "test.py");
        match result {
            SignatureChangeKind::ParamsAdded { added } => {
                assert!(added.contains(&"formal".to_string()));
            }
            other => panic!("Expected ParamsAdded, got {:?}", other),
        }
    }

    #[test]
    fn test_python_param_removed() {
        let before = "def greet(name, formal):\n    return name";
        let after = "def greet(name):\n    return name";
        let result = analyze_signature_change(before, after, "test.py");
        assert!(result.is_breaking());
    }

    #[test]
    fn test_rust_body_only() {
        let before = "fn greet(name: &str) -> String { format!(\"hello {}\", name) }";
        let after = "fn greet(name: &str) -> String { format!(\"hi {}\", name) }";
        let result = analyze_signature_change(before, after, "test.rs");
        assert_eq!(result, SignatureChangeKind::BodyOnly);
    }

    #[test]
    fn test_rust_param_added() {
        let before = "fn greet(name: &str) -> String { name.to_string() }";
        let after = "fn greet(name: &str, loud: bool) -> String { name.to_string() }";
        let result = analyze_signature_change(before, after, "test.rs");
        match result {
            SignatureChangeKind::ParamsAdded { added } => {
                assert!(added.contains(&"loud".to_string()));
            }
            other => panic!("Expected ParamsAdded, got {:?}", other),
        }
    }

    #[test]
    fn test_go_param_removed() {
        let before = "func greet(name string, age int) string { return name }";
        let after = "func greet(name string) string { return name }";
        let result = analyze_signature_change(before, after, "test.go");
        assert!(result.is_breaking(), "Expected breaking, got {:?}", result);
    }

    #[test]
    fn test_java_body_only() {
        let before = "public void greet(String name) { System.out.println(name); }";
        let after =
            "public void greet(String name) { System.out.println(\"Hello \" + name); }";
        let result = analyze_signature_change(before, after, "Test.java");
        assert_eq!(result, SignatureChangeKind::BodyOnly);
    }

    #[test]
    fn test_not_a_function() {
        let before = "const x = 42;";
        let after = "const x = 99;";
        let result = analyze_signature_change(before, after, "test.ts");
        assert_eq!(result, SignatureChangeKind::NotApplicable);
    }

    #[test]
    fn test_unsupported_extension() {
        let result = analyze_signature_change("a", "b", "test.xyz");
        assert_eq!(result, SignatureChangeKind::NotApplicable);
    }
}
