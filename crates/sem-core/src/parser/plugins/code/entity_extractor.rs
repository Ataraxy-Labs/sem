use tree_sitter::{Node, Tree};

use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::utils::hash::{content_hash, structural_hash};
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
) {
    let node_type = node.kind();

    if config.entity_node_types.contains(&node_type) {
        if let Some(name) = extract_name(node, source) {
            let entity_type = map_node_type(node_type);
            let content = node_text(node, source);

            let struct_hash = structural_hash(node, source);
            let entity = SemanticEntity {
                id: build_entity_id(file_path, &entity_type, &name, parent_id),
                file_path: file_path.to_string(),
                entity_type: entity_type.clone(),
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
                        );
                    }
                }
            }
            return;
        }
    }

    // For export statements, look inside for the actual declaration
    if node_type == "export_statement" {
        if let Some(declaration) = node.child_by_field_name("declaration") {
            visit_node(declaration, file_path, config, entities, parent_id, source);
            return;
        }
    }

    // Recurse into top-level children
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_node(child, file_path, config, entities, parent_id, source);
    }
}

fn extract_name(node: Node, source: &[u8]) -> Option<String> {
    // Try 'name' field first (works for most languages)
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(name_node, source));
    }

    // For variable/lexical declarations, try to get the declarator name
    let node_type = node.kind();
    if node_type == "lexical_declaration" || node_type == "variable_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                if let Some(decl_name) = child.child_by_field_name("name") {
                    return Some(node_text(decl_name, source));
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
                    return Some(node_text(inner_name, source));
                }
            }
        }
    }

    // For C function_definition, the name is inside the declarator
    if node_type == "function_definition" {
        if let Some(declarator) = node.child_by_field_name("declarator") {
            return extract_declarator_name(declarator, source);
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
    if node_type == "struct_specifier" || node_type == "enum_specifier" || node_type == "union_specifier" {
        if let Some(name_node) = node.child_by_field_name("name") {
            return Some(node_text(name_node, source));
        }
    }

    // For C type_definition (typedef), look for the type name
    if node_type == "type_definition" {
        if let Some(declarator) = node.child_by_field_name("declarator") {
            return extract_declarator_name(declarator, source);
        }
    }

    // Fallback: first identifier child
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return Some(node_text(child, source));
        }
    }

    None
}

/// Extract the name from a C declarator (handles pointer_declarator, function_declarator, etc.)
fn extract_declarator_name(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(node_text(node, source)),
        "pointer_declarator" | "function_declarator" | "array_declarator" | "parenthesized_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                extract_declarator_name(inner, source)
            } else {
                let mut cursor = node.walk();
                let result = node.named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                    .map(|c| node_text(c, source));
                result
            }
        }
        _ => {
            if let Some(name) = node.child_by_field_name("name") {
                return Some(node_text(name, source));
            }
            let mut cursor = node.walk();
            let result = node.named_children(&mut cursor)
                .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                .map(|c| node_text(c, source));
            result
        }
    }
}

fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

fn map_node_type(tree_sitter_type: &str) -> String {
    match tree_sitter_type {
        "function_declaration" | "function_definition" | "function_item" => "function",
        "method_declaration" | "method_definition" => "method",
        "class_declaration" | "class_definition" => "class",
        "interface_declaration" => "interface",
        "type_alias_declaration" | "type_declaration" | "type_item" | "type_definition" => "type",
        "enum_declaration" | "enum_item" | "enum_specifier" => "enum",
        "struct_item" | "struct_specifier" => "struct",
        "union_specifier" => "union",
        "impl_item" => "impl",
        "trait_item" => "trait",
        "mod_item" => "module",
        "export_statement" => "export",
        "lexical_declaration" | "variable_declaration" | "var_declaration" | "declaration" => "variable",
        "const_declaration" | "const_item" => "constant",
        "static_item" => "static",
        "decorated_definition" => "function",
        "constructor_declaration" => "constructor",
        "field_declaration" => "field",
        "annotation_type_declaration" => "annotation",
        other => return other.to_string(),
    }
    .to_string()
}
