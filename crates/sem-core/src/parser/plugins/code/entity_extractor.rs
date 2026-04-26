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

    // OCaml: value_definition, module_definition, class_definition, and
    // class_type_definition can each contain multiple bindings via `... and ...`.
    // Extract each binding as a separate entity.
    if node_type == "value_definition" && config.entity_node_types.contains(&node_type) {
        let mut cursor = node.walk();
        let bindings: Vec<_> = node.named_children(&mut cursor)
            .filter(|c| c.kind() == "let_binding")
            .collect();
        if !bindings.is_empty() {
            for binding in bindings {
                let names = extract_ocaml_let_binding_names(binding, source);
                let entity_type = map_ocaml_let_binding(binding);
                let content_str = node_text(binding, source);
                let content = content_str.to_string();
                let struct_hash = compute_structural_hash(binding, source);
                for name in names {
                    let entity = SemanticEntity {
                        id: build_entity_id(file_path, entity_type, &name, parent_id),
                        file_path: file_path.to_string(),
                        entity_type: entity_type.to_string(),
                        name,
                        parent_id: parent_id.map(String::from),
                        content_hash: content_hash(&content),
                        structural_hash: Some(struct_hash.clone()),
                        content: content.clone(),
                        start_line: binding.start_position().row + 1,
                        end_line: binding.end_position().row + 1,
                        metadata: None,
                    };
                    entities.push(entity);
                }
            }
            return;
        }
    }

    if node_type == "module_definition" && config.entity_node_types.contains(&node_type) {
        let extracted = extract_ocaml_named_bindings(
            node, "module_binding", "module_name",
            map_node_type(node_type), file_path, parent_id, source, config, entities,
        );
        if extracted { return; }
    }

    if node_type == "class_definition" && config.entity_node_types.contains(&node_type) {
        let extracted = extract_ocaml_named_bindings(
            node, "class_binding", "class_name",
            map_node_type(node_type), file_path, parent_id, source, config, entities,
        );
        if extracted { return; }
    }

    if node_type == "class_type_definition" && config.entity_node_types.contains(&node_type) {
        let extracted = extract_ocaml_named_bindings(
            node, "class_type_binding", "class_type_name",
            map_node_type(node_type), file_path, parent_id, source, config, entities,
        );
        if extracted { return; }
    }

    if config.entity_node_types.contains(&node_type) {
        if let Some(name) = extract_name(node, source) {
            let name = qualify_hcl_name(&name, node_type, parent_id, suppression_context);
            let entity_type = map_entity_type(node, config);
            let should_skip = should_skip_entity(config, suppression_context, node_type);
            if !should_skip {
                // Go method_declaration: extract receiver type for parent linkage.
                // e.g. `func (t *Transaction) Execute(...)` -> parent is Transaction struct
                let effective_parent = if node_type == "method_declaration" && parent_id.is_none() {
                    extract_go_receiver_struct(node, source, file_path, entities)
                } else {
                    None
                };
                let parent_ref = effective_parent.as_deref().or(parent_id);

                // Dart top-level signatures are split from their body node.
                // When a sibling function_body exists, extend the entity to
                // cover the full definition so body changes are detected.
                let body = if config.id == "dart" { sibling_function_body(node) } else { None };
                let end_byte = body.map_or(node.end_byte(), |b| b.end_byte());
                let content = std::str::from_utf8(&source[node.start_byte()..end_byte])
                    .unwrap_or("")
                    .to_string();
                let end_line =
                    body.map_or(node.end_position().row + 1, |b| b.end_position().row + 1);
                let struct_hash = match body {
                    Some(b) => {
                        let sig = compute_structural_hash(node, source);
                        let bod = structural_hash(b, source);
                        content_hash(&format!("{}{}", sig, bod))
                    }
                    None => compute_structural_hash(node, source),
                };

                let entity = SemanticEntity {
                    id: build_entity_id(file_path, entity_type, &name, parent_ref),
                    file_path: file_path.to_string(),
                    entity_type: entity_type.to_string(),
                    name: name.clone(),
                    parent_id: parent_ref.map(String::from),
                    content_hash: content_hash(&content),
                    structural_hash: Some(struct_hash),
                    content,
                    start_line: node.start_position().row + 1,
                    end_line,
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

/// For Dart top-level function/getter/setter signatures, return the sibling
/// function_body node so the entity content can be extended to include it.
fn sibling_function_body(node: Node) -> Option<Node> {
    match node.kind() {
        "function_signature" | "getter_signature" | "setter_signature" => {
            let sibling = node.next_named_sibling()?;
            (sibling.kind() == "function_body").then_some(sibling)
        }
        _ => None,
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

    // Dart class_member: name is inside method_signature or declaration
    if node_type == "class_member" {
        return find_dart_class_member_name_range(node, _source);
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

    // OCaml: individual binding nodes (used when compute_structural_hash is called
    // on a binding directly, e.g., from the multi-binding extraction in visit_node)
    if node_type == "let_binding" {
        if let Some(pattern) = node.child_by_field_name("pattern") {
            return Some((pattern.start_byte(), pattern.end_byte()));
        }
    }

    if node_type == "module_binding" || node_type == "class_binding" || node_type == "class_type_binding" {
        let name_kind = match node_type {
            "module_binding" => "module_name",
            "class_binding" => "class_name",
            "class_type_binding" => "class_type_name",
            _ => unreachable!(),
        };
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == name_kind {
                return Some((child.start_byte(), child.end_byte()));
            }
        }
    }

    // OCaml: module_type_definition -> module_type_name
    if node_type == "module_type_definition" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "module_type_name" {
                return Some((child.start_byte(), child.end_byte()));
            }
        }
    }

    // OCaml and C type_definition
    if node_type == "type_definition" {
        // OCaml: type_definition -> type_binding -> field "name"
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "type_binding" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some((name_node.start_byte(), name_node.end_byte()));
                }
            }
        }
        // C type_definition falls through to the "declaration || type_definition" block below
    }

    // OCaml: exception_definition -> constructor_declaration -> constructor_name
    if node_type == "exception_definition" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "constructor_declaration" {
                let mut inner = child.walk();
                for inner_child in child.named_children(&mut inner) {
                    if inner_child.kind() == "constructor_name" {
                        return Some((inner_child.start_byte(), inner_child.end_byte()));
                    }
                }
            }
        }
    }

    // OCaml: external / value_specification -> value_name
    if node_type == "external" || node_type == "value_specification" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "value_name" || child.kind() == "parenthesized_operator" {
                return Some((child.start_byte(), child.end_byte()));
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

    // OCaml: module_type_definition -> module_type_name
    if node_type == "module_type_definition" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "module_type_name" {
                return Some(node_text(child, source).to_string());
            }
        }
    }

    // OCaml and C type_definition
    if node_type == "type_definition" {
        // OCaml: type_definition -> type_binding -> field "name" (type_constructor)
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "type_binding" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some(node_text(name_node, source).to_string());
                }
            }
        }
        // C type_definition (typedef): look for declarator
        if let Some(declarator) = node.child_by_field_name("declarator") {
            return extract_declarator_name(declarator, source);
        }
    }

    // For Dart class_member, the name is nested inside method_signature or declaration
    if node_type == "class_member" {
        return extract_dart_class_member_name(node, source);
    }

    // OCaml: exception_definition -> constructor_declaration -> constructor_name
    if node_type == "exception_definition" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "constructor_declaration" {
                let mut inner = child.walk();
                for inner_child in child.named_children(&mut inner) {
                    if inner_child.kind() == "constructor_name" {
                        return Some(node_text(inner_child, source).to_string());
                    }
                }
            }
        }
    }

    // OCaml: external / value_specification -> value_name
    if node_type == "external" || node_type == "value_specification" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "value_name" || child.kind() == "parenthesized_operator" {
                return Some(node_text(child, source).to_string());
            }
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

    // Fortran: wrapper nodes (function, subroutine, module, program, interface)
    // have their name on the _statement child node as a "name" kind node
    if node_type == "function"
        || node_type == "subroutine"
        || node_type == "module"
        || node_type == "program"
        || node_type == "interface"
    {
        let stmt_kind = format!("{}_statement", node_type);
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == stmt_kind {
                // Try field first, then look for "name" kind child
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some(node_text(name_node, source).to_string());
                }
                let mut inner = child.walk();
                for grandchild in child.named_children(&mut inner) {
                    if grandchild.kind() == "name" {
                        return Some(node_text(grandchild, source).to_string());
                    }
                }
            }
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
        "function_declaration"
        | "generator_function_declaration"
        | "function_definition"
        | "function_item"
        | "function_signature"
        | "subroutine_declaration_statement" => "function",
        "method_declaration" | "method_definition" | "method" | "singleton_method" => "method",
        "class_declaration" | "class_definition" | "class_specifier" => "class",
        "interface_declaration" => "interface",
        "type_alias_declaration" | "type_declaration" | "type_item" | "type_definition" | "type_alias" => "type",
        "enum_declaration" | "enum_item" | "enum_specifier" => "enum",
        "mixin_declaration" => "mixin",
        "extension_declaration" | "extension_type_declaration" => "extension",
        "getter_signature" => "getter",
        "setter_signature" => "setter",
        "struct_item" | "struct_specifier" | "struct_declaration" => "struct",
        "union_specifier" => "union",
        "impl_item" => "impl",
        "trait_item" => "trait",
        "mod_item" | "module" | "module_definition" | "namespace_definition" | "namespace_declaration" => "module",
        "package_statement" => "package",
        "export_statement" => "export",
        "lexical_declaration" | "variable_declaration" | "var_declaration" | "declaration" => "variable",
        "const_declaration" | "const_item" => "constant",
        "static_item" => "static",
        "value_specification" => "val",
        "module_type_definition" => "module_type",
        "exception_definition" => "exception",
        "class_type_definition" => "class_type",
        "external" => "external",
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

/// For Dart class_member, determine the entity type from the inner signature or declaration.
fn map_class_member_type(node: Node) -> &'static str {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "method_signature" => {
                let mut inner = child.walk();
                for sig in child.named_children(&mut inner) {
                    return match sig.kind() {
                        "function_signature" => "method",
                        "getter_signature" => "getter",
                        "setter_signature" => "setter",
                        "constructor_signature" | "factory_constructor_signature" => "constructor",
                        "operator_signature" => "method",
                        _ => continue,
                    };
                }
            }
            "declaration" => {
                let mut inner = child.walk();
                for sig in child.named_children(&mut inner) {
                    return match sig.kind() {
                        "function_signature" => "method",
                        "getter_signature" => "getter",
                        "setter_signature" => "setter",
                        "constructor_signature"
                        | "constant_constructor_signature"
                        | "factory_constructor_signature"
                        | "redirecting_factory_constructor_signature" => "constructor",
                        "operator_signature" => "method",
                        "initialized_identifier_list"
                        | "static_final_declaration_list"
                        | "identifier_list" => "field",
                        _ => continue,
                    };
                }
            }
            _ => {}
        }
    }
    "member"
}

fn map_entity_type(node: Node, config: &LanguageConfig) -> &'static str {
    match node.kind() {
        "decorated_definition" => map_decorated_type(node),
        "class_member" => map_class_member_type(node),
        _ => promote_zig_variable(node, config)
            .or_else(|| promote_js_ts_const_function(node, config))
            .unwrap_or_else(|| map_node_type(node.kind())),
    }
}

/// In Zig, `const Point = struct { ... }` is a variable_declaration whose RHS
/// is a struct/enum/union expression. Promote the entity type accordingly.
fn promote_zig_variable(node: Node, config: &LanguageConfig) -> Option<&'static str> {
    if config.id != "zig" || node.kind() != "variable_declaration" {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "struct_declaration" => return Some("struct"),
            "enum_declaration" => return Some("enum"),
            "union_declaration" => return Some("union"),
            "error_set_declaration" => return Some("type"),
            _ => {}
        }
    }
    None
}

fn promote_js_ts_const_function(node: Node, config: &LanguageConfig) -> Option<&'static str> {
    if !matches!(config.id, "typescript" | "tsx" | "javascript") {
        return None;
    }

    if node.kind() != "lexical_declaration" {
        return None;
    }

    let declaration_kind = node.child_by_field_name("kind")?;
    if declaration_kind.kind() != "const" {
        return None;
    }

    let mut cursor = node.walk();
    let declarator = node.named_children(&mut cursor).find(|child| child.kind() == "variable_declarator")?;
    let value = declarator.child_by_field_name("value")?;

    match value.kind() {
        "arrow_function" | "function_expression" | "generator_function" => Some("function"),
        _ => None,
    }
}

/// Dart constructor signatures use `field("name", seq(identifier, optional(".", identifier)))`,
/// so the "name" field label is shared by multiple identifier nodes. Collect them all and
/// join with "." to produce e.g. "Calculator.withDefault" for named constructors.
const DART_CONSTRUCTOR_SIG_KINDS: &[&str] = &[
    "constructor_signature",
    "constant_constructor_signature",
    "factory_constructor_signature",
    "redirecting_factory_constructor_signature",
];

fn extract_dart_constructor_full_name(sig: Node, source: &[u8]) -> Option<String> {
    let (start, end) = dart_constructor_name_byte_range(sig)?;
    std::str::from_utf8(&source[start..end]).ok().map(|s| s.to_string())
}

/// Byte range spanning all "name" field children of a Dart constructor signature,
/// covering the full `Calculator.withDefault` span including the dot.
fn dart_constructor_name_byte_range(sig: Node) -> Option<(usize, usize)> {
    let mut cursor = sig.walk();
    let mut start = None;
    let mut end = None;
    for n in sig.children_by_field_name("name", &mut cursor) {
        if start.is_none() {
            start = Some(n.start_byte());
        }
        end = Some(n.end_byte());
    }
    start.zip(end)
}

/// Walk a Dart `class_member` node's tree to find the name-bearing node,
/// then call `resolve` to convert it into the caller's desired type.
fn walk_dart_class_member<T>(
    node: Node,
    source: &[u8],
    resolve: impl Fn(Node, &[u8]) -> Option<T>,
) -> Option<T> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "method_signature" | "declaration" => {
                let mut inner = child.walk();
                for sig in child.named_children(&mut inner) {
                    if DART_CONSTRUCTOR_SIG_KINDS.contains(&sig.kind()) {
                        return resolve(sig, source);
                    }
                    if let Some(name_node) = sig.child_by_field_name("name") {
                        return resolve(name_node, source);
                    }
                    if sig.kind() == "operator_signature" {
                        return resolve(sig, source);
                    }
                    // Field declarations: name is one level deeper.
                    // Only the first identifier is captured (one entity per class_member node),
                    // so `abstract double x, y;` yields only `x`.
                    if sig.kind() == "initialized_identifier_list"
                        || sig.kind() == "static_final_declaration_list"
                    {
                        let mut deep = sig.walk();
                        for entry in sig.named_children(&mut deep) {
                            if let Some(name_node) = entry.child_by_field_name("name") {
                                return resolve(name_node, source);
                            }
                        }
                    }
                    // identifier_list has bare identifier children (no "name" field)
                    if sig.kind() == "identifier_list" {
                        let mut deep = sig.walk();
                        for entry in sig.named_children(&mut deep) {
                            if entry.kind() == "identifier" {
                                return resolve(entry, source);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_dart_class_member_name(node: Node, source: &[u8]) -> Option<String> {
    walk_dart_class_member(node, source, |found, src| {
        if DART_CONSTRUCTOR_SIG_KINDS.contains(&found.kind()) {
            return extract_dart_constructor_full_name(found, src);
        }
        if found.kind() == "operator_signature" {
            return found
                .child_by_field_name("operator")
                .map(|op| format!("operator {}", node_text(op, src)));
        }
        Some(node_text(found, src).to_string())
    })
}

fn find_dart_class_member_name_range(node: Node, source: &[u8]) -> Option<(usize, usize)> {
    walk_dart_class_member(node, source, |found, _src| {
        if DART_CONSTRUCTOR_SIG_KINDS.contains(&found.kind()) {
            return dart_constructor_name_byte_range(found);
        }
        if found.kind() == "operator_signature" {
            return found
                .child_by_field_name("operator")
                .map(|op| (op.start_byte(), op.end_byte()));
        }
        Some((found.start_byte(), found.end_byte()))
    })
}

/// For an OCaml let_binding node, check if it has parameters or a function body
/// to determine whether it's a "function" or a "value".
fn map_ocaml_let_binding(node: Node) -> &'static str {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "parameter" {
            return "function";
        }
    }
    // `let f = fun ...` or `let f = function ...`
    if let Some(body) = node.child_by_field_name("body") {
        if body.kind() == "fun_expression" || body.kind() == "function_expression" {
            return "function";
        }
    }
    "value"
}

/// Extract names from an OCaml let_binding node.
/// For simple bindings (`let x = ...`), returns `["x"]`.
/// For operator bindings (`let ( + ) = ...`), returns `["( + )"]`.
/// For destructured bindings (`let (a, b) = ...`), returns `["a", "b"]`.
fn extract_ocaml_let_binding_names(binding: Node, source: &[u8]) -> Vec<String> {
    let pattern = match binding.child_by_field_name("pattern") {
        Some(p) => p,
        None => return vec![],
    };
    if pattern.kind() == "value_name" || pattern.kind() == "parenthesized_operator" {
        return vec![node_text(pattern, source).to_string()];
    }
    // Destructured pattern: collect all value_name leaves
    let mut names = vec![];
    collect_value_names(pattern, source, &mut names);
    names
}

fn collect_value_names(node: Node, source: &[u8], names: &mut Vec<String>) {
    if node.kind() == "value_name" {
        names.push(node_text(node, source).to_string());
        return;
    }
    // Punned record field (`{ x; y }`) — field_pattern with no pattern field.
    // The bound name is the field_name itself.
    if node.kind() == "field_pattern" {
        if let Some(pattern) = node.child_by_field_name("pattern") {
            collect_value_names(pattern, source, names);
        } else {
            // Punned: extract the field_name from the field_path
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "field_path" {
                    let mut inner = child.walk();
                    for fc in child.named_children(&mut inner) {
                        if fc.kind() == "field_name" {
                            names.push(node_text(fc, source).to_string());
                        }
                    }
                }
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_value_names(child, source, names);
    }
}

/// Extract entities from OCaml multi-binding definitions (module, class, class type).
/// Each binding_kind child (e.g., "module_binding") gets its own entity.
/// Returns true if bindings were found and extracted.
#[allow(clippy::too_many_arguments)]
fn extract_ocaml_named_bindings(
    node: Node,
    binding_kind: &str,
    name_kind: &str,
    entity_type: &str,
    file_path: &str,
    parent_id: Option<&str>,
    source: &[u8],
    config: &LanguageConfig,
    entities: &mut Vec<SemanticEntity>,
) -> bool {
    let mut cursor = node.walk();
    let bindings: Vec<_> = node.named_children(&mut cursor)
        .filter(|c| c.kind() == binding_kind)
        .collect();
    if bindings.is_empty() {
        return false;
    }
    for binding in bindings {
        let mut inner = binding.walk();
        let name = binding.named_children(&mut inner)
            .find(|c| c.kind() == name_kind)
            .map(|c| node_text(c, source).to_string());
        if let Some(name) = name {
            let content_str = node_text(binding, source);
            let content = content_str.to_string();
            let struct_hash = compute_structural_hash(binding, source);
            let entity = SemanticEntity {
                id: build_entity_id(file_path, entity_type, &name, parent_id),
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: name.clone(),
                parent_id: parent_id.map(String::from),
                content_hash: content_hash(&content),
                structural_hash: Some(struct_hash),
                content,
                start_line: binding.start_position().row + 1,
                end_line: binding.end_position().row + 1,
                metadata: None,
            };

            let entity_id = entity.id.clone();
            entities.push(entity);

            // Visit container children for nested entities
            let mut container_cursor = binding.walk();
            for child in binding.named_children(&mut container_cursor) {
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
                            Some(node.kind()),
                        );
                    }
                }
            }
        }
    }
    true
}

/// For Go method_declaration nodes, extract the receiver struct type name
/// and find the matching struct entity ID to use as parent_id.
/// e.g. `func (t *Transaction) Execute(...)` -> finds Transaction's entity ID
fn extract_go_receiver_struct(
    node: Node,
    source: &[u8],
    file_path: &str,
    entities: &[SemanticEntity],
) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    // receiver is a parameter_list containing parameter_declaration(s)
    let mut cursor = receiver.walk();
    for param in receiver.named_children(&mut cursor) {
        if param.kind() == "parameter_declaration" {
            let type_node = param.child_by_field_name("type")?;
            let type_text = node_text(type_node, source);
            // Strip pointer: *Transaction -> Transaction
            let struct_name = type_text.trim_start_matches('*');
            if struct_name.is_empty() {
                return None;
            }
            // Find matching struct/type entity in the same file
            for e in entities.iter().rev() {
                if e.file_path == file_path
                    && e.name == struct_name
                    && matches!(e.entity_type.as_str(), "type" | "struct" | "class" | "interface")
                {
                    return Some(e.id.clone());
                }
            }
            // No struct entity found yet (might be in a different file), use synthetic ID
            return Some(format!("{}::type::{}", file_path, struct_name));
        }
    }
    None
}
