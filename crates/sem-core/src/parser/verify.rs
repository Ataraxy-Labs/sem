//! Contract verification: check that callers pass the correct number of
//! arguments to callees. Uses tree-sitter AST for accurate param/arg counting.

use std::collections::HashMap;
use std::path::Path;

use crate::model::entity::SemanticEntity;
use crate::parser::graph::{EntityGraph, RefType};
use crate::parser::plugins::code::languages::get_language_config;
use crate::parser::registry::ParserRegistry;

#[derive(Debug, Clone)]
pub struct ContractViolation {
    pub entity_name: String,
    pub file_path: String,
    pub expected_params: usize,
    pub caller_name: String,
    pub caller_file: String,
    pub actual_args: usize,
}

/// Result of tree-sitter based parameter analysis.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub min_params: usize,
    pub max_params: usize,
    pub is_variadic: bool,
}

/// Arity mismatch found across the dependency graph.
#[derive(Debug, Clone)]
pub struct ArityMismatch {
    pub caller_entity: String,
    pub callee_entity: String,
    pub expected_min: usize,
    pub expected_max: usize,
    pub actual_args: usize,
    pub file_path: String,
    pub line: usize,
    pub is_variadic: bool,
}

/// Verify function call contracts across the codebase.
pub fn verify_contracts(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    target_file: Option<&str>,
) -> Vec<ContractViolation> {
    let (graph, _) = EntityGraph::build(root, file_paths, registry);

    let mut content_map: HashMap<String, String> = HashMap::new();
    for fp in file_paths {
        let full = root.join(fp);
        let content = match std::fs::read_to_string(&full) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let plugin = match registry.get_plugin_with_content(fp, &content) {
            Some(p) => p,
            None => continue,
        };
        for entity in plugin.extract_entities(&content, fp) {
            content_map.insert(entity.id.clone(), entity.content.clone());
        }
    }

    let mut violations = Vec::new();

    for edge in &graph.edges {
        if edge.ref_type != RefType::Calls {
            continue;
        }

        let callee = match graph.entities.get(&edge.to_entity) {
            Some(e) => e,
            None => continue,
        };

        if let Some(tf) = target_file {
            if callee.file_path != tf {
                continue;
            }
        }

        if !matches!(
            callee.entity_type.as_str(),
            "function" | "method" | "arrow_function"
        ) {
            continue;
        }

        let callee_content = match content_map.get(&edge.to_entity) {
            Some(c) => c,
            None => continue,
        };

        let caller = match graph.entities.get(&edge.from_entity) {
            Some(e) => e,
            None => continue,
        };

        let caller_content = match content_map.get(&edge.from_entity) {
            Some(c) => c,
            None => continue,
        };

        let expected = extract_param_count(callee_content);
        if expected == 0 {
            continue;
        }

        if let Some(actual) = count_call_args(caller_content, &callee.name) {
            if actual != expected {
                violations.push(ContractViolation {
                    entity_name: callee.name.clone(),
                    file_path: callee.file_path.clone(),
                    expected_params: expected,
                    caller_name: caller.name.clone(),
                    caller_file: caller.file_path.clone(),
                    actual_args: actual,
                });
            }
        }
    }

    violations
}

/// Like `verify_contracts`, but accepts a pre-built graph + entities.
pub fn verify_contracts_with_graph(
    graph: &EntityGraph,
    all_entities: &[SemanticEntity],
    target_file: Option<&str>,
) -> Vec<ContractViolation> {
    let content_map: HashMap<String, String> = all_entities
        .iter()
        .map(|e| (e.id.clone(), e.content.clone()))
        .collect();

    let mut violations = Vec::new();

    for edge in &graph.edges {
        if edge.ref_type != RefType::Calls {
            continue;
        }

        let callee = match graph.entities.get(&edge.to_entity) {
            Some(e) => e,
            None => continue,
        };

        if let Some(tf) = target_file {
            if callee.file_path != tf {
                continue;
            }
        }

        if !matches!(
            callee.entity_type.as_str(),
            "function" | "method" | "arrow_function"
        ) {
            continue;
        }

        let callee_content = match content_map.get(&edge.to_entity) {
            Some(c) => c,
            None => continue,
        };

        let caller = match graph.entities.get(&edge.from_entity) {
            Some(e) => e,
            None => continue,
        };

        let caller_content = match content_map.get(&edge.from_entity) {
            Some(c) => c,
            None => continue,
        };

        let expected = extract_param_count(callee_content);
        if expected == 0 {
            continue;
        }

        if let Some(actual) = count_call_args(caller_content, &callee.name) {
            if actual != expected {
                violations.push(ContractViolation {
                    entity_name: callee.name.clone(),
                    file_path: callee.file_path.clone(),
                    expected_params: expected,
                    caller_name: caller.name.clone(),
                    caller_file: caller.file_path.clone(),
                    actual_args: actual,
                });
            }
        }
    }

    violations
}

// ─── Tree-sitter based arity analysis ───────────────────────────────────────

fn lang_from_ext(ext: &str) -> &'static str {
    match ext {
        ".py" | ".pyi" => "python",
        ".ts" | ".tsx" | ".mts" | ".cts" => "typescript",
        ".js" | ".jsx" | ".mjs" | ".cjs" => "typescript",
        ".rs" => "rust",
        ".go" => "go",
        _ => "unknown",
    }
}

/// Extract parameter info from entity content using tree-sitter.
pub fn extract_param_info_ts(content: &str, file_path: &str) -> Option<ParamInfo> {
    let ext = file_path.rfind('.').map(|i| &file_path[i..])?;
    let lang = lang_from_ext(ext);
    if lang == "unknown" {
        return None;
    }
    let config = get_language_config(ext)?;
    let language = (config.get_language)()?;

    let mut parser = tree_sitter::Parser::new();
    let _ = parser.set_language(&language);
    let tree = parser.parse(content.as_bytes(), None)?;

    extract_param_info_from_node(tree.root_node(), content.as_bytes(), lang)
}

fn extract_param_info_from_node(
    root: tree_sitter::Node,
    source: &[u8],
    lang: &str,
) -> Option<ParamInfo> {
    // Find the first function-like node
    let func_node = find_first_function(root)?;
    let params_node = func_node.child_by_field_name("parameters")?;

    let mut min_params = 0usize;
    let mut max_params = 0usize;
    let mut is_variadic = false;

    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        let kind = child.kind();
        match lang {
            "python" => {
                if kind == "identifier" {
                    let name = child.utf8_text(source).unwrap_or("");
                    if name == "self" || name == "cls" {
                        continue;
                    }
                    min_params += 1;
                    max_params += 1;
                } else if kind == "typed_parameter" {
                    let name = child
                        .child_by_field_name("name")
                        .or_else(|| child.named_child(0))
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    if name == "self" || name == "cls" {
                        continue;
                    }
                    min_params += 1;
                    max_params += 1;
                } else if kind == "default_parameter" || kind == "typed_default_parameter" {
                    max_params += 1;
                } else if kind == "list_splat_pattern" || kind == "dictionary_splat_pattern" {
                    is_variadic = true;
                }
            }
            "typescript" => {
                if kind == "required_parameter" {
                    min_params += 1;
                    max_params += 1;
                } else if kind == "optional_parameter" {
                    max_params += 1;
                } else if kind == "rest_pattern" {
                    is_variadic = true;
                }
            }
            "rust" => {
                if kind == "parameter" {
                    let pat = child
                        .child_by_field_name("pattern")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    // Skip self/&self/&mut self
                    let base = pat.trim_start_matches('&').trim();
                    let base = base.strip_prefix("mut ").unwrap_or(base).trim();
                    if base == "self" {
                        continue;
                    }
                    min_params += 1;
                    max_params += 1;
                } else if kind == "self_parameter" {
                    continue;
                }
            }
            "go" => {
                if kind == "parameter_declaration" {
                    // Check for variadic: ...Type
                    let type_text = child
                        .child_by_field_name("type")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    if type_text.starts_with("...") {
                        is_variadic = true;
                    } else {
                        min_params += 1;
                        max_params += 1;
                    }
                }
            }
            _ => {}
        }
    }

    Some(ParamInfo {
        min_params,
        max_params,
        is_variadic,
    })
}

fn find_first_function(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let kind = node.kind();
    if matches!(
        kind,
        "function_definition"
            | "function_item"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "arrow_function"
    ) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(f) = find_first_function(child) {
            return Some(f);
        }
    }
    None
}

/// Count call arguments at a specific call site using tree-sitter.
pub fn count_call_args_ts(
    caller_content: &str,
    callee_name: &str,
    file_path: &str,
) -> Option<usize> {
    let ext = file_path.rfind('.').map(|i| &file_path[i..])?;
    let config = get_language_config(ext)?;
    let language = (config.get_language)()?;

    let mut parser = tree_sitter::Parser::new();
    let _ = parser.set_language(&language);
    let tree = parser.parse(caller_content.as_bytes(), None)?;

    find_call_arg_count(tree.root_node(), caller_content.as_bytes(), callee_name)
}

fn find_call_arg_count(
    node: tree_sitter::Node,
    source: &[u8],
    callee_name: &str,
) -> Option<usize> {
    let kind = node.kind();

    if kind == "call" || kind == "call_expression" {
        let func = node.child_by_field_name("function")?;
        let func_name = match func.kind() {
            "identifier" => func.utf8_text(source).unwrap_or(""),
            "attribute" | "member_expression" | "field_expression" => func
                .child_by_field_name("attribute")
                .or_else(|| func.child_by_field_name("property"))
                .or_else(|| func.child_by_field_name("field"))
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or(""),
            "selector_expression" => func
                .child_by_field_name("field")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or(""),
            "scoped_identifier" => {
                let text = func.utf8_text(source).unwrap_or("");
                text.rsplit("::").next().unwrap_or("")
            }
            _ => "",
        };

        if func_name == callee_name {
            let args = node.child_by_field_name("arguments")?;
            let mut count = 0;
            let mut cursor = args.walk();
            for child in args.named_children(&mut cursor) {
                // Skip comment nodes
                if !child.kind().contains("comment") {
                    count += 1;
                }
            }
            return Some(count);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(count) = find_call_arg_count(child, source, callee_name) {
            return Some(count);
        }
    }
    None
}

/// Find arity mismatches across all Calls edges in the graph.
pub fn find_arity_mismatches(
    graph: &EntityGraph,
    all_entities: &[SemanticEntity],
) -> Vec<ArityMismatch> {
    let entity_by_id: HashMap<&str, &SemanticEntity> = all_entities
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();

    // Cache param info per callee entity
    let mut param_cache: HashMap<String, Option<ParamInfo>> = HashMap::new();

    let mut mismatches = Vec::new();

    for edge in &graph.edges {
        if edge.ref_type != RefType::Calls {
            continue;
        }

        let callee_info = match graph.entities.get(&edge.to_entity) {
            Some(e) => e,
            None => continue,
        };

        if !matches!(
            callee_info.entity_type.as_str(),
            "function" | "method" | "arrow_function"
        ) {
            continue;
        }

        let callee = match entity_by_id.get(edge.to_entity.as_str()) {
            Some(e) => *e,
            None => continue,
        };

        let caller = match entity_by_id.get(edge.from_entity.as_str()) {
            Some(e) => *e,
            None => continue,
        };

        // Get callee param info (cached)
        let param_info = param_cache
            .entry(callee.id.clone())
            .or_insert_with(|| extract_param_info_ts(&callee.content, &callee.file_path))
            .clone();

        let param_info = match param_info {
            Some(pi) => pi,
            None => continue,
        };

        // Skip variadic functions
        if param_info.is_variadic {
            continue;
        }

        // Count call args using tree-sitter
        let actual = match count_call_args_ts(
            &caller.content,
            &callee.name,
            &caller.file_path,
        ) {
            Some(a) => a,
            None => continue,
        };

        if actual < param_info.min_params || actual > param_info.max_params {
            mismatches.push(ArityMismatch {
                caller_entity: caller.name.clone(),
                callee_entity: callee.name.clone(),
                expected_min: param_info.min_params,
                expected_max: param_info.max_params,
                actual_args: actual,
                file_path: caller.file_path.clone(),
                line: caller.start_line,
                is_variadic: false,
            });
        }
    }

    mismatches
}

/// Find callers broken by signature changes between old and new entities.
/// Compares param counts of functions that exist in both old and new,
/// then checks if any callers in new_graph pass the wrong arg count.
pub fn find_broken_callers(
    old_entities: &[SemanticEntity],
    new_graph: &EntityGraph,
    new_entities: &[SemanticEntity],
) -> Vec<ArityMismatch> {
    // Build old param info map: entity_id -> ParamInfo
    let old_params: HashMap<String, Option<ParamInfo>> = old_entities
        .iter()
        .filter(|e| matches!(e.entity_type.as_str(), "function" | "method" | "arrow_function"))
        .map(|e| (e.id.clone(), extract_param_info_ts(&e.content, &e.file_path)))
        .collect();

    // Build new entity lookup
    let new_by_id: HashMap<&str, &SemanticEntity> = new_entities
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();

    // Find entities whose param counts changed
    let mut changed_entities: Vec<&str> = Vec::new();
    for new_entity in new_entities {
        if !matches!(new_entity.entity_type.as_str(), "function" | "method" | "arrow_function") {
            continue;
        }
        let new_info = match extract_param_info_ts(&new_entity.content, &new_entity.file_path) {
            Some(pi) => pi,
            None => continue,
        };
        if let Some(Some(old_info)) = old_params.get(&new_entity.id) {
            if old_info.min_params != new_info.min_params
                || old_info.max_params != new_info.max_params
            {
                changed_entities.push(&new_entity.id);
            }
        }
    }

    if changed_entities.is_empty() {
        return Vec::new();
    }

    // Check all callers of changed entities
    let mut mismatches = Vec::new();

    for edge in &new_graph.edges {
        if edge.ref_type != RefType::Calls {
            continue;
        }
        if !changed_entities.contains(&edge.to_entity.as_str()) {
            continue;
        }

        let callee = match new_by_id.get(edge.to_entity.as_str()) {
            Some(e) => *e,
            None => continue,
        };
        let caller = match new_by_id.get(edge.from_entity.as_str()) {
            Some(e) => *e,
            None => continue,
        };

        let new_info = match extract_param_info_ts(&callee.content, &callee.file_path) {
            Some(pi) => pi,
            None => continue,
        };

        if new_info.is_variadic {
            continue;
        }

        let actual = match count_call_args_ts(&caller.content, &callee.name, &caller.file_path) {
            Some(a) => a,
            None => continue,
        };

        if actual < new_info.min_params || actual > new_info.max_params {
            mismatches.push(ArityMismatch {
                caller_entity: caller.name.clone(),
                callee_entity: callee.name.clone(),
                expected_min: new_info.min_params,
                expected_max: new_info.max_params,
                actual_args: actual,
                file_path: caller.file_path.clone(),
                line: caller.start_line,
                is_variadic: false,
            });
        }
    }

    mismatches
}

// ─── String-based helpers (kept for backward compatibility) ──────────────────

/// Extract param count from the first line of a function/method.
fn extract_param_count(content: &str) -> usize {
    let first_line = content.lines().next().unwrap_or("");

    let open = match first_line.find('(') {
        Some(i) => i,
        None => return 0,
    };

    let after_open = &first_line[open + 1..];
    let close = match find_matching_paren(after_open) {
        Some(i) => i,
        None => return 0,
    };

    let params_str = after_open[..close].trim();
    if params_str.is_empty() {
        return 0;
    }

    count_top_level_commas(params_str) + 1
}

/// Count arguments at a call site: find `callee_name(...)` in content and count args.
fn count_call_args(content: &str, callee_name: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let name_bytes = callee_name.as_bytes();
    let mut search_start = 0;

    while let Some(rel_pos) = content[search_start..].find(callee_name) {
        let pos = search_start + rel_pos;
        let after = pos + name_bytes.len();

        let is_boundary = pos == 0 || {
            let prev = bytes[pos - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_'
        };

        if is_boundary && after < bytes.len() && bytes[after] == b'(' {
            let args_start = &content[after + 1..];
            if let Some(close) = find_matching_paren(args_start) {
                let args_str = args_start[..close].trim();
                if args_str.is_empty() {
                    return Some(0);
                }
                return Some(count_top_level_commas(args_str) + 1);
            }
        }

        search_start = pos + 1;
        while search_start < content.len() && !content.is_char_boundary(search_start) {
            search_start += 1;
        }
    }

    None
}

fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn count_top_level_commas(s: &str) -> usize {
    let mut depth = 0i32;
    let mut count = 0;
    for ch in s.chars() {
        match ch {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_param_count_basic() {
        assert_eq!(extract_param_count("function foo(a, b, c) {"), 3);
        assert_eq!(extract_param_count("function foo() {"), 0);
        assert_eq!(extract_param_count("def bar(self, x):"), 2);
        assert_eq!(extract_param_count("fn baz(a: i32) -> bool {"), 1);
    }

    #[test]
    fn test_extract_param_count_nested() {
        assert_eq!(extract_param_count("function foo(a, fn(x, y), c) {"), 3);
    }

    #[test]
    fn test_count_call_args() {
        assert_eq!(count_call_args("let x = foo(1, 2, 3);", "foo"), Some(3));
        assert_eq!(count_call_args("foo()", "foo"), Some(0));
        assert_eq!(count_call_args("bar(1)", "foo"), None);
        assert_eq!(count_call_args("foo(a, b)", "foo"), Some(2));
    }

    #[test]
    fn test_count_call_args_multibyte_utf8() {
        assert_eq!(count_call_args("let café = foo(1, 2);", "foo"), Some(2));
        assert_eq!(count_call_args("let É = 1; bar(x)", "bar"), Some(1));
        assert_eq!(count_call_args("// 日本語コメント\nfoo(a, b, c)", "foo"), Some(3));
    }

    #[test]
    fn test_extract_param_info_python() {
        let info = extract_param_info_ts(
            "def foo(a, b, c=3):\n    pass",
            "test.py",
        )
        .unwrap();
        assert_eq!(info.min_params, 2);
        assert_eq!(info.max_params, 3);
        assert!(!info.is_variadic);
    }

    #[test]
    fn test_extract_param_info_python_self() {
        let info = extract_param_info_ts(
            "def foo(self, a, b):\n    pass",
            "test.py",
        )
        .unwrap();
        assert_eq!(info.min_params, 2);
        assert_eq!(info.max_params, 2);
    }

    #[test]
    fn test_extract_param_info_python_variadic() {
        let info = extract_param_info_ts(
            "def foo(a, *args, **kwargs):\n    pass",
            "test.py",
        )
        .unwrap();
        assert!(info.is_variadic);
    }

    #[test]
    fn test_extract_param_info_typescript() {
        let info = extract_param_info_ts(
            "function foo(a: number, b: string, c?: boolean): void {}",
            "test.ts",
        )
        .unwrap();
        assert_eq!(info.min_params, 2);
        assert_eq!(info.max_params, 3);
        assert!(!info.is_variadic);
    }

    #[test]
    fn test_extract_param_info_rust() {
        let info = extract_param_info_ts(
            "fn foo(&self, a: i32, b: String) -> bool { true }",
            "test.rs",
        )
        .unwrap();
        assert_eq!(info.min_params, 2);
        assert_eq!(info.max_params, 2);
    }

    #[test]
    fn test_extract_param_info_go() {
        let info = extract_param_info_ts(
            "func foo(a string, b int) error { return nil }",
            "test.go",
        )
        .unwrap();
        assert_eq!(info.min_params, 2);
        assert_eq!(info.max_params, 2);
    }

    #[test]
    fn test_count_call_args_ts() {
        let count = count_call_args_ts(
            "function bar() { foo(1, 2, 3); }",
            "foo",
            "test.ts",
        );
        assert_eq!(count, Some(3));
    }

    #[test]
    fn test_count_call_args_ts_method() {
        let count = count_call_args_ts(
            "function bar() { obj.foo(1, 2); }",
            "foo",
            "test.ts",
        );
        assert_eq!(count, Some(2));
    }
}
