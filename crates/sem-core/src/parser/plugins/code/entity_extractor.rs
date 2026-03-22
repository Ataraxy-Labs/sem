use tree_sitter::{Node, Tree};

use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::utils::hash::{content_hash, structural_hash, structural_hash_excluding_range};
use super::languages::LanguageConfig;

pub fn extract_entities(
    tree: &Tree,
    file_path: &str,
    config: &LanguageConfig,
    source_code: &str,
) -> Vec<SemanticEntity> {
    let mut entities = Vec::new();
    visit_node(
        tree.root_node(),
        file_path,
        config,
        &mut entities,
        None,
        source_code.as_bytes(),
        None,
    );
    entities
}

fn visit_node(
    node: Node,
    file_path: &str,
    config: &LanguageConfig,
    entities: &mut Vec<SemanticEntity>,
    parent_id: Option<&str>,
    source: &[u8],
    suppression_context: Option<&str>,
) {
    let node_type = node.kind();

    // Handle call-based entities (Elixir: def, defmodule, etc.)
    if node_type == "call" && !config.call_entity_identifiers.is_empty() {
        if let Some((name, entity_type)) = extract_call_entity(node, config, source) {
            let content_str = node_text(node, source);
            let content = content_str.to_string();
            let struct_hash = compute_structural_hash(node, source);
            let entity = SemanticEntity {
                id: build_entity_id(file_path, entity_type, &name, parent_id),
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: name.clone(),
                parent_id: parent_id.map(String::from),
                content_hash: content_hash(&content),
                structural_hash: Some(struct_hash),
                content,
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                metadata: None,
            };

            let entity_id = entity.id.clone();
            entities.push(entity);

            // Visit container children for nested entities (defs inside defmodule)
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if config.container_node_types.contains(&child.kind()) {
                    let mut inner_cursor = child.walk();
                    for nested in child.named_children(&mut inner_cursor) {
                        visit_node(
                            nested,
                            file_path,
                            config,
                            entities,
                            Some(&entity_id),
                            source,
                            suppression_context,
                        );
                    }
                }
            }
            return;
        }
    }

    if config.entity_node_types.contains(&node_type) {
        if let Some(name) = extract_name(node, source) {
            let name = qualify_hcl_name(&name, node_type, parent_id, suppression_context);
            let entity_type = if node_type == "decorated_definition" {
                map_decorated_type(node)
            } else {
                map_node_type(node_type)
            };
            let should_skip = should_skip_entity(config, suppression_context, node_type);
            if !should_skip {
                let content_str = node_text(node, source);
                let content = content_str.to_string();

                let struct_hash = compute_structural_hash(node, source);
                let entity = SemanticEntity {
                    id: build_entity_id(file_path, entity_type, &name, parent_id),
                    file_path: file_path.to_string(),
                    entity_type: entity_type.to_string(),
                    name: name.clone(),
                    parent_id: parent_id.map(String::from),
                    content_hash: content_hash(&content),
                    structural_hash: Some(struct_hash),
                    content,
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    metadata: None,
                };

                let entity_id = entity.id.clone();
                entities.push(entity);

                // Visit children for nested entities (methods inside classes, etc.)
                let next_suppression_context = Some(node_type);
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if config.container_node_types.contains(&child.kind()) {
                        let mut inner_cursor = child.walk();
                        for nested in child.named_children(&mut inner_cursor) {
                            visit_node(
                                nested,
                                file_path,
                                config,
                                entities,
                                Some(&entity_id),
                                source,
                                next_suppression_context,
                            );
                        }
                    }
                }

                // For variable declarations, also traverse into initializers
                // that are scope boundaries (arrow functions, function expressions)
                // so that inner class/function declarations are extracted.
                if node_type == "lexical_declaration" || node_type == "variable_declaration" {
                    let mut vd_cursor = node.walk();
                    for child in node.named_children(&mut vd_cursor) {
                        if child.kind() == "variable_declarator" {
                            if let Some(value) = child.child_by_field_name("value") {
                                if config.scope_boundary_types.contains(&value.kind()) {
                                    visit_node(
                                        value,
                                        file_path,
                                        config,
                                        entities,
                                        Some(&entity_id),
                                        source,
                                        Some(value.kind()),
                                    );
                                }
                            }
                        }
                    }
                }

                return;
            }
        }
    }

    // For export statements, look inside for the actual declaration
    if node_type == "export_statement" {
        if let Some(declaration) = node.child_by_field_name("declaration") {
            visit_node(declaration, file_path, config, entities, parent_id, source, suppression_context);
            return;
        }
    }

    // Recurse into children. When we enter a scope boundary (e.g. arrow
    // functions, function expressions) we propagate the boundary's node type
    // as the suppression context so that suppressed_nested_entities rules
    // filter out local variable declarations while still allowing inner
    // class/function declarations to be extracted.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let child_enclosing = if config.scope_boundary_types.contains(&child.kind()) {
            Some(child.kind())
        } else {
            suppression_context
        };
        visit_node(
            child,
            file_path,
            config,
            entities,
            parent_id,
            source,
            child_enclosing,
        );
    }
}

/// Compute the structural hash for an entity, excluding the name token so that
/// renames of otherwise identical entities produce the same hash.
fn compute_structural_hash(node: Node, source: &[u8]) -> String {
    match find_name_byte_range(node, source) {
        Some((start, end)) => structural_hash_excluding_range(node, source, start, end),
        None => structural_hash(node, source),
    }
}

/// Find the byte range of the name node, mirroring extract_name() logic.
/// Returns (start_byte, end_byte) of the name token to exclude from hashing.
fn find_name_byte_range(node: Node, _source: &[u8]) -> Option<(usize, usize)> {
    // Try 'name' field first (works for most languages)
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some((name_node.start_byte(), name_node.end_byte()));
    }

    let node_type = node.kind();

    // Variable/lexical declarations: name is inside variable_declarator
    if node_type == "lexical_declaration" || node_type == "variable_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                if let Some(decl_name) = child.child_by_field_name("name") {
                    return Some((decl_name.start_byte(), decl_name.end_byte()));
                }
            }
        }
    }

    // Go var/const/type declarations: name is inside var_spec/const_spec/type_spec.
    // Grouped forms like `var (...)` wrap specs in var_spec_list/type_spec_list.
    if node_type == "var_declaration"
        || node_type == "const_declaration"
        || node_type == "type_declaration"
    {
        let spec_kinds = ["var_spec", "const_spec", "type_spec"];
        let list_kinds = ["var_spec_list", "type_spec_list"];
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if spec_kinds.contains(&child.kind()) {
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some((name_node.start_byte(), name_node.end_byte()));
                }
            }
            if list_kinds.contains(&child.kind()) {
                let mut inner = child.walk();
                for spec in child.named_children(&mut inner) {
                    if spec_kinds.contains(&spec.kind()) {
                        if let Some(name_node) = spec.child_by_field_name("name") {
                            return Some((name_node.start_byte(), name_node.end_byte()));
                        }
                    }
                }
            }
        }
    }

    // Decorated definitions (Python): look at the inner definition
    if node_type == "decorated_definition" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "function_definition" || child.kind() == "class_definition" {
                if let Some(inner_name) = child.child_by_field_name("name") {
                    return Some((inner_name.start_byte(), inner_name.end_byte()));
                }
            }
        }
    }

    // C/C++ function_definition: name is inside declarator
    if node_type == "function_definition" {
        if let Some(declarator) = node.child_by_field_name("declarator") {
            return find_declarator_name_range(declarator);
        }
    }

    // C++ template_declaration
    if node_type == "template_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() != "template_parameter_list" {
                if let Some(name) = child.child_by_field_name("name") {
                    return Some((name.start_byte(), name.end_byte()));
                }
                if let Some(declarator) = child.child_by_field_name("declarator") {
                    return find_declarator_name_range(declarator);
                }
            }
        }
    }

    // C declarations
    if node_type == "declaration" || node_type == "type_definition" {
        if let Some(declarator) = node.child_by_field_name("declarator") {
            return find_declarator_name_range(declarator);
        }
    }

    // Fallback: first identifier child
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return Some((child.start_byte(), child.end_byte()));
        }
    }

    None
}

/// Find the byte range of the name within a C-style declarator chain.
fn find_declarator_name_range(node: Node) -> Option<(usize, usize)> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" => {
            Some((node.start_byte(), node.end_byte()))
        }
        "qualified_identifier" | "scoped_identifier" => {
            Some((node.start_byte(), node.end_byte()))
        }
        "pointer_declarator" | "function_declarator" | "array_declarator"
        | "parenthesized_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                find_declarator_name_range(inner)
            } else {
                let mut cursor = node.walk();
                let result = node
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                    .map(|c| (c.start_byte(), c.end_byte()));
                result
            }
        }
        _ => {
            if let Some(name) = node.child_by_field_name("name") {
                return Some((name.start_byte(), name.end_byte()));
            }
            let mut cursor = node.walk();
            let result = node
                .named_children(&mut cursor)
                .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                .map(|c| (c.start_byte(), c.end_byte()));
            result
        }
    }
}

fn extract_name(node: Node, source: &[u8]) -> Option<String> {
    // Try 'name' field first (works for most languages)
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(name_node, source).to_string());
    }

    // For variable/lexical declarations, try to get the declarator name
    let node_type = node.kind();

    // For Rust impl blocks, construct unique name from trait + type
    // e.g. "impl Display for Foo" -> "Display for Foo", "impl Foo" -> "Foo"
    if node_type == "impl_item" {
        let trait_node = node.child_by_field_name("trait");
        let type_node = node.child_by_field_name("type");
        match (trait_node, type_node) {
            (Some(trait_n), Some(type_n)) => {
                return Some(format!(
                    "{} for {}",
                    node_text(trait_n, source),
                    node_text(type_n, source)
                ));
            }
            (None, Some(type_n)) => {
                return Some(node_text(type_n, source).to_string());
            }
            _ => {} // fall through to generic fallback
        }
    }

    if node_type == "lexical_declaration" || node_type == "variable_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                if let Some(decl_name) = child.child_by_field_name("name") {
                    return Some(node_text(decl_name, source).to_string());
                }
            }
        }
    }

    // For Go var/const/type declarations, name is inside var_spec/const_spec/type_spec child.
    // Grouped forms like `var (...)` wrap specs in var_spec_list/type_spec_list.
    if node_type == "var_declaration"
        || node_type == "const_declaration"
        || node_type == "type_declaration"
    {
        let spec_kinds = ["var_spec", "const_spec", "type_spec"];
        let list_kinds = ["var_spec_list", "type_spec_list"];
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if spec_kinds.contains(&child.kind()) {
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some(node_text(name_node, source).to_string());
                }
            }
            // Grouped form: var (...) / type (...)
            if list_kinds.contains(&child.kind()) {
                let mut inner = child.walk();
                for spec in child.named_children(&mut inner) {
                    if spec_kinds.contains(&spec.kind()) {
                        if let Some(name_node) = spec.child_by_field_name("name") {
                            return Some(node_text(name_node, source).to_string());
                        }
                    }
                }
            }
        }
    }

    // For decorated definitions (Python), look at the inner definition
    if node_type == "decorated_definition" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "function_definition" || child.kind() == "class_definition" {
                if let Some(inner_name) = child.child_by_field_name("name") {
                    return Some(node_text(inner_name, source).to_string());
                }
            }
        }
    }

    // For C/C++ function_definition, the name is inside the declarator
    if node_type == "function_definition" {
        if let Some(declarator) = node.child_by_field_name("declarator") {
            return extract_declarator_name(declarator, source);
        }
    }

    // For C++ template_declaration, look at the inner declaration
    if node_type == "template_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let kind = child.kind();
            if kind != "template_parameter_list" {
                // The inner declaration (class, function, etc.)
                if let Some(name) = child.child_by_field_name("name") {
                    return Some(node_text(name, source).to_string());
                }
                if let Some(declarator) = child.child_by_field_name("declarator") {
                    return extract_declarator_name(declarator, source);
                }
            }
        }
    }

    // For C++ namespace_definition
    if node_type == "namespace_definition" {
        if let Some(name_node) = node.child_by_field_name("name") {
            return Some(node_text(name_node, source).to_string());
        }
    }

    // For C++ class_specifier
    if node_type == "class_specifier" {
        if let Some(name_node) = node.child_by_field_name("name") {
            return Some(node_text(name_node, source).to_string());
        }
    }

    // For C# property_declaration, namespace_declaration, struct_declaration
    if node_type == "property_declaration" || node_type == "namespace_declaration" || node_type == "struct_declaration" {
        if let Some(name_node) = node.child_by_field_name("name") {
            return Some(node_text(name_node, source).to_string());
        }
    }

    // For C declarations (global vars, function prototypes), extract the declarator name
    if node_type == "declaration" {
        if let Some(declarator) = node.child_by_field_name("declarator") {
            // Could be a plain identifier, pointer_declarator, function_declarator, etc.
            return extract_declarator_name(declarator, source);
        }
    }

    // For C struct/enum/union specifiers, try the 'name' field
    if node_type == "struct_specifier"
        || node_type == "enum_specifier"
        || node_type == "union_specifier"
    {
        if let Some(name_node) = node.child_by_field_name("name") {
            return Some(node_text(name_node, source).to_string());
        }
    }

    // For C type_definition (typedef), look for the type name
    if node_type == "type_definition" {
        if let Some(declarator) = node.child_by_field_name("declarator") {
            return extract_declarator_name(declarator, source);
        }
    }

    // For XML elements, extract tag name from STag or EmptyElemTag
    if node_type == "element" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "STag" || child.kind() == "EmptyElemTag" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some(node_text(name_node, source).to_string());
                }
                // Fallback: first Name child
                let mut inner = child.walk();
                for inner_child in child.named_children(&mut inner) {
                    if inner_child.kind() == "Name" {
                        return Some(node_text(inner_child, source).to_string());
                    }
                }
            }
        }
    }

    // For HCL blocks, combine block type with labels (e.g., resource.cloudflare_record.dns)
    if node_type == "block" {
        let mut parts = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "identifier" => parts.push(node_text(child, source).to_string()),
                "string_lit" => {
                    let text = node_text(child, source);
                    parts.push(text.trim_matches('"').to_string());
                }
                _ => break, // stop at body or other non-label nodes
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("."));
        }
    }

    // Fallback: first identifier child
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return Some(node_text(child, source).to_string());
        }
    }

    None
}

// Prefix nested HCL block names with their parent entity name for flat output.
fn qualify_hcl_name(
    name: &str,
    node_type: &str,
    parent_id: Option<&str>,
    suppression_context: Option<&str>,
) -> String {
    if node_type != "block" || suppression_context != Some("block") {
        return name.to_string();
    }

    match parent_id.and_then(parent_entity_name_from_id) {
        Some(parent_name) => format!("{parent_name}.{name}"),
        None => name.to_string(),
    }
}

// Extract the entity name portion from an entity id.
fn parent_entity_name_from_id(parent_id: &str) -> Option<&str> {
    parent_id.rsplit("::").next()
}

// Apply language-specific nested entity suppression rules from config.
fn should_skip_entity(
    config: &LanguageConfig,
    suppression_context: Option<&str>,
    node_type: &str,
) -> bool {
    config.suppressed_nested_entities.iter().any(|rule| {
        suppression_context == Some(rule.parent_entity_node_type)
            && node_type == rule.child_entity_node_type
    })
}

/// Extract the name from a C declarator (handles pointer_declarator, function_declarator, etc.)
fn extract_declarator_name(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" => Some(node_text(node, source).to_string()),
        "qualified_identifier" | "scoped_identifier" => {
            // For C++ qualified names like ClassName::method, return the full qualified name
            Some(node_text(node, source).to_string())
        }
        "pointer_declarator"
        | "function_declarator"
        | "array_declarator"
        | "parenthesized_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                extract_declarator_name(inner, source)
            } else {
                let mut cursor = node.walk();
                let result = node
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                    .map(|c| node_text(c, source).to_string());
                result
            }
        }
        _ => {
            if let Some(name) = node.child_by_field_name("name") {
                return Some(node_text(name, source).to_string());
            }
            let mut cursor = node.walk();
            let result = node
                .named_children(&mut cursor)
                .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                .map(|c| node_text(c, source).to_string());
            result
        }
    }
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn map_node_type(tree_sitter_type: &str) -> &str {
    match tree_sitter_type {
        "function_declaration" | "function_definition" | "function_item" => "function",
        "method_declaration" | "method_definition" | "method" | "singleton_method" => "method",
        "class_declaration" | "class_definition" | "class_specifier" => "class",
        "interface_declaration" => "interface",
        "type_alias_declaration" | "type_declaration" | "type_item" | "type_definition" => "type",
        "enum_declaration" | "enum_item" | "enum_specifier" => "enum",
        "struct_item" | "struct_specifier" | "struct_declaration" => "struct",
        "union_specifier" => "union",
        "impl_item" => "impl",
        "trait_item" => "trait",
        "mod_item" | "module" | "namespace_definition" | "namespace_declaration" => "module",
        "export_statement" => "export",
        "lexical_declaration" | "variable_declaration" | "var_declaration" | "declaration" => "variable",
        "const_declaration" | "const_item" => "constant",
        "static_item" => "static",
        "decorated_definition" => "decorated_definition",
        "constructor_declaration" => "constructor",
        "field_declaration" | "public_field_definition" | "field_definition" => "field",
        "property_declaration" => "property",
        "annotation_type_declaration" => "annotation",
        "template_declaration" => "template",
        other => other,
    }
}

/// Extract entity info from a call node (Elixir macros like def, defmodule, etc.)
fn extract_call_entity(node: Node, config: &LanguageConfig, source: &[u8]) -> Option<(String, &'static str)> {
    let target = node.child_by_field_name("target")?;
    if target.kind() != "identifier" {
        return None;
    }
    let keyword = node_text(target, source);

    if !config.call_entity_identifiers.contains(&keyword) {
        return None;
    }

    let entity_type = match keyword {
        "defmodule" => "module",
        "def" | "defp" | "defdelegate" => "function",
        "defmacro" | "defmacrop" => "macro",
        "defguard" | "defguardp" => "guard",
        "defprotocol" => "protocol",
        "defimpl" => "impl",
        "defstruct" => "struct",
        "defexception" => "exception",
        _ => return None,
    };

    // Get arguments node (child by kind, not field name)
    let mut cursor = node.walk();
    let args = node.named_children(&mut cursor).find(|c| c.kind() == "arguments")?;

    let name = match keyword {
        "defmodule" | "defprotocol" => extract_first_alias_or_identifier(args, source)?,
        "defimpl" => {
            let base = extract_first_alias_or_identifier(args, source)?;
            if let Some(target) = extract_keyword_value(args, "for", source) {
                format!("{} for {}", base, target)
            } else {
                base
            }
        }
        "defstruct" => "__struct__".to_string(),
        "defexception" => "__exception__".to_string(),
        _ => {
            // def, defp, defmacro, defguard, defdelegate
            // First arg is a call (fn with params), identifier (arity-0),
            // or binary_operator (defguard with when clause)
            let mut cursor = args.walk();
            let first_arg = args.named_children(&mut cursor).next()?;
            extract_fn_name_from_arg(first_arg, source)?
        }
    };

    Some((name, entity_type))
}

/// Extract function name from a def/defp/defmacro/defguard argument.
/// Handles: call (fn with params), identifier (arity-0), binary_operator (defguard when clause)
fn extract_fn_name_from_arg(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "call" => {
            if let Some(fn_target) = node.child_by_field_name("target") {
                Some(node_text(fn_target, source).to_string())
            } else {
                let mut c = node.walk();
                let id = node.named_children(&mut c)
                    .find(|n| n.kind() == "identifier")?;
                Some(node_text(id, source).to_string())
            }
        }
        "identifier" => Some(node_text(node, source).to_string()),
        "binary_operator" => {
            // defguard is_positive(x) when ... -> left side has the actual call/identifier
            let left = node.child_by_field_name("left")?;
            extract_fn_name_from_arg(left, source)
        }
        _ => None,
    }
}

fn extract_first_alias_or_identifier(args: Node, source: &[u8]) -> Option<String> {
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        match child.kind() {
            "alias" => return Some(node_text(child, source).to_string()),
            "identifier" => return Some(node_text(child, source).to_string()),
            _ => {}
        }
    }
    None
}

fn extract_keyword_value(args: Node, key: &str, source: &[u8]) -> Option<String> {
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if child.kind() == "keywords" {
            let mut kw_cursor = child.walk();
            for pair in child.named_children(&mut kw_cursor) {
                if pair.kind() == "pair" {
                    if let Some(pair_key) = pair.child_by_field_name("key") {
                        let key_text = node_text(pair_key, source).trim();
                        if key_text == format!("{}:", key) || key_text == key {
                            if let Some(pair_value) = pair.child_by_field_name("value") {
                                return Some(node_text(pair_value, source).to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// For Python decorated_definition, check the inner node to determine the real type.
fn map_decorated_type(node: Node) -> &'static str {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_definition" => return "class",
            "function_definition" => return "function",
            _ => {}
        }
    }
    "function"
}
