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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallArgCount {
    actual_args: usize,
    line_offset: usize,
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
        for entity in registry.extract_entities(fp, &content) {
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

        for actual in count_all_call_args(caller_content, &callee.name) {
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

        for actual in count_all_call_args(caller_content, &callee.name) {
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
        ".js" | ".jsx" | ".mjs" | ".cjs" => "javascript",
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
                    max_params += 1;
                    if !has_js_ts_default_value(child) {
                        min_params += 1;
                    }
                } else if kind == "optional_parameter" {
                    max_params += 1;
                } else if kind == "rest_pattern" {
                    is_variadic = true;
                }
            }
            "javascript" => {
                if kind == "rest_pattern" {
                    is_variadic = true;
                } else if matches!(kind, "identifier" | "formal_parameter" | "assignment_pattern") {
                    max_params += 1;
                    if !has_js_ts_default_value(child) {
                        min_params += 1;
                    }
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
                    let type_node = child.child_by_field_name("type");
                    let type_text = type_node.and_then(|n| n.utf8_text(source).ok()).unwrap_or("");
                    let param_text = child.utf8_text(source).unwrap_or("");
                    if type_text.starts_with("...") || param_text.contains("...") {
                        is_variadic = true;
                    } else {
                        let count = count_go_parameter_declaration_arity(child);
                        min_params += count;
                        max_params += count;
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

fn has_js_ts_default_value(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    let has_assignment_child = node
        .named_children(&mut cursor)
        .any(|child| child.kind() == "assignment_pattern");
    node.kind() == "assignment_pattern"
        || node.child_by_field_name("value").is_some()
        || has_assignment_child
}

fn count_go_parameter_declaration_arity(node: tree_sitter::Node) -> usize {
    let mut name_cursor = node.walk();
    let field_names = node
        .children_by_field_name("name", &mut name_cursor)
        .count();
    if field_names > 0 {
        return field_names;
    }

    let type_range = match node.child_by_field_name("type") {
        Some(type_node) => (type_node.start_byte(), type_node.end_byte()),
        None => return 1,
    };
    let mut cursor = node.walk();
    let identifier_names = node
        .named_children(&mut cursor)
        .filter(|child| {
            child.kind() == "identifier" && type_range != (child.start_byte(), child.end_byte())
        })
        .count();
    if identifier_names > 0 {
        identifier_names
    } else {
        1
    }
}

fn find_first_function(root: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
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
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            worklist.push(child);
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
    count_call_arg_sites_ts(caller_content, callee_name, file_path)
        .into_iter()
        .next()
        .map(|site| site.actual_args)
}

fn count_call_arg_sites_ts(
    caller_content: &str,
    callee_name: &str,
    file_path: &str,
) -> Vec<CallArgCount> {
    let ext = match file_path.rfind('.').map(|i| &file_path[i..]) {
        Some(ext) => ext,
        None => return Vec::new(),
    };
    let config = match get_language_config(ext) {
        Some(config) => config,
        None => return Vec::new(),
    };
    let language = match (config.get_language)() {
        Some(language) => language,
        None => return Vec::new(),
    };

    let mut parser = tree_sitter::Parser::new();
    let _ = parser.set_language(&language);
    let tree = match parser.parse(caller_content.as_bytes(), None) {
        Some(tree) => tree,
        None => return Vec::new(),
    };

    find_call_arg_counts(tree.root_node(), caller_content.as_bytes(), callee_name)
}

fn find_call_arg_counts(
    root: tree_sitter::Node,
    source: &[u8],
    callee_name: &str,
) -> Vec<CallArgCount> {
    let mut sites = Vec::new();
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        let kind = node.kind();

        if kind == "call" || kind == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
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
                    if let Some(args) = node.child_by_field_name("arguments") {
                        let mut actual_args = 0;
                        let mut cursor = args.walk();
                        for child in args.named_children(&mut cursor) {
                            // Skip comment nodes
                            if !child.kind().contains("comment") {
                                actual_args += 1;
                            }
                        }
                        sites.push(CallArgCount {
                            actual_args,
                            line_offset: node.start_position().row,
                        });
                    }
                }
            }
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            worklist.push(child);
        }
    }
    sites
}

/// Names too common/ambiguous for reliable arity checking (constructors, builtins).
const AMBIGUOUS_NAMES: &[&str] = &[
    "new", "constructor", "toString", "valueOf", "init", "__init__",
    "apply", "call", "bind", "get", "set", "run", "execute", "create",
];

/// Path components that indicate test/fixture files (not production source).
const TEST_PATH_MARKERS: &[&str] = &[
    "test", "tests", "spec", "specs", "fixtures", "fixture",
    "benchmarks", "benchmark", "__tests__", "__mocks__",
];

fn is_test_or_fixture_path(path: &str) -> bool {
    path.split('/').any(|component| TEST_PATH_MARKERS.contains(&component))
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

    // Build name → count map to detect ambiguous names
    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for e in all_entities {
        if matches!(e.entity_type.as_str(), "function" | "method" | "arrow_function") {
            *name_counts.entry(&e.name).or_insert(0) += 1;
        }
    }

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

        // Skip ambiguous/common names where name-only matching is unreliable
        if AMBIGUOUS_NAMES.contains(&callee_info.name.as_str()) {
            continue;
        }

        // Skip callee names shared by multiple entities (overloads, trait impls)
        if name_counts.get(callee_info.name.as_str()).copied().unwrap_or(0) > 1 {
            continue;
        }

        // Skip test/fixture files
        if is_test_or_fixture_path(&callee_info.file_path) {
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

        // Skip callers in test/fixture files
        if is_test_or_fixture_path(&caller.file_path) {
            continue;
        }

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

        for call_site in count_call_arg_sites_ts(&caller.content, &callee.name, &caller.file_path) {
            if call_site.actual_args < param_info.min_params
                || call_site.actual_args > param_info.max_params
            {
                mismatches.push(ArityMismatch {
                    caller_entity: caller.name.clone(),
                    callee_entity: callee.name.clone(),
                    expected_min: param_info.min_params,
                    expected_max: param_info.max_params,
                    actual_args: call_site.actual_args,
                    file_path: caller.file_path.clone(),
                    line: caller.start_line + call_site.line_offset,
                    is_variadic: false,
                });
            }
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

        for call_site in count_call_arg_sites_ts(&caller.content, &callee.name, &caller.file_path) {
            if call_site.actual_args < new_info.min_params
                || call_site.actual_args > new_info.max_params
            {
                mismatches.push(ArityMismatch {
                    caller_entity: caller.name.clone(),
                    callee_entity: callee.name.clone(),
                    expected_min: new_info.min_params,
                    expected_max: new_info.max_params,
                    actual_args: call_site.actual_args,
                    file_path: caller.file_path.clone(),
                    line: caller.start_line + call_site.line_offset,
                    is_variadic: false,
                });
            }
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
#[cfg(test)]
fn count_call_args(content: &str, callee_name: &str) -> Option<usize> {
    count_all_call_args(content, callee_name).into_iter().next()
}

fn count_all_call_args(content: &str, callee_name: &str) -> Vec<usize> {
    let bytes = content.as_bytes();
    let name_bytes = callee_name.as_bytes();
    let mut search_start = 0;
    let mut counts = Vec::new();

    while let Some(rel_pos) = content[search_start..].find(callee_name) {
        let pos = search_start + rel_pos;
        let after = pos + name_bytes.len();

        let is_boundary = pos == 0 || {
            let prev = bytes[pos - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_'
        };

        let mut next_search_start = pos + 1;
        if is_boundary && after < bytes.len() && bytes[after] == b'(' {
            let args_start_index = after + 1;
            let args_start = &content[args_start_index..];
            if let Some(close) = find_matching_paren(args_start) {
                let args_str = args_start[..close].trim();
                if args_str.is_empty() {
                    counts.push(0);
                } else {
                    counts.push(count_top_level_commas(args_str) + 1);
                }
                next_search_start = args_start_index + close + 1;
            } else {
                next_search_start = after;
            }
        }

        search_start = next_search_start;
        while search_start < content.len() && !content.is_char_boundary(search_start) {
            search_start += 1;
        }
    }

    counts
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
    fn test_count_all_call_args() {
        assert_eq!(count_all_call_args("foo(1, 2); foo(1);", "foo"), vec![2, 1]);
    }

    #[test]
    fn test_count_all_call_args_resumes_after_unclosed_candidate() {
        assert_eq!(count_all_call_args("foo(\nfoo(1, 2)", "foo"), vec![2]);
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
    fn test_extract_param_info_typescript_default_parameter() {
        let info = extract_param_info_ts(
            "function foo(a: number, b = 1): number { return a + b; }",
            "test.ts",
        )
        .unwrap();
        assert_eq!(info.min_params, 1);
        assert_eq!(info.max_params, 2);
        assert!(!info.is_variadic);
    }

    #[test]
    fn test_extract_param_info_javascript_default_parameter() {
        let info =
            extract_param_info_ts("function foo(a, b = 1) { return a + b; }", "test.js").unwrap();
        assert_eq!(info.min_params, 1);
        assert_eq!(info.max_params, 2);
        assert!(!info.is_variadic);
    }

    #[test]
    fn test_extract_param_info_javascript_required_parameters() {
        let info = extract_param_info_ts("function foo(a, b) { return a + b; }", "test.js")
            .unwrap();
        assert_eq!(info.min_params, 2);
        assert_eq!(info.max_params, 2);
        assert!(!info.is_variadic);
    }

    #[test]
    fn test_extract_param_info_typescript_arrow_default_parameter() {
        let info = extract_param_info_ts(
            "const foo = (a: number, b = 1): number => a + b;",
            "test.ts",
        )
        .unwrap();
        assert_eq!(info.min_params, 1);
        assert_eq!(info.max_params, 2);
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
    fn test_extract_param_info_go_grouped_params() {
        let info = extract_param_info_ts(
            "func foo(a, b int, c string) int { return a + b }",
            "test.go",
        )
        .unwrap();
        assert_eq!(info.min_params, 3);
        assert_eq!(info.max_params, 3);
    }

    #[test]
    fn test_extract_param_info_go_unnamed_params() {
        let info = extract_param_info_ts(
            "func foo(int, string) bool { return true }",
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

    #[test]
    fn test_count_call_arg_sites_ts_repeated_calls() {
        let sites =
            count_call_arg_sites_ts("def bar():\n    foo(1, 2)\n    foo(1)\n", "foo", "test.py");
        assert_eq!(
            sites,
            vec![
                CallArgCount {
                    actual_args: 2,
                    line_offset: 1,
                },
                CallArgCount {
                    actual_args: 1,
                    line_offset: 2,
                },
            ]
        );
    }
}
