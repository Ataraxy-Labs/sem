use tree_sitter::{Node, Tree};

use super::languages::LanguageConfig;
use crate::model::entity::{
    build_entity_id, build_entity_id_disambiguated, build_entity_id_disambiguated_with_ordinal,
    SemanticEntity,
};
use crate::utils::hash::{content_hash, structural_hash, structural_hash_excluding_range};
use std::collections::{HashMap, HashSet};

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

    recover_swift_conditional_compilation_containers(
        tree.root_node(),
        file_path,
        config,
        &mut entities,
        source_code.as_bytes(),
    );

    if config.id == "go" {
        attach_go_package_metadata(tree.root_node(), source_code.as_bytes(), &mut entities);
    }

    disambiguate_colliding_entity_ids(&mut entities);

    entities
}

type IdRewrites = HashMap<String, Vec<(usize, String)>>;

fn disambiguate_colliding_entity_ids(entities: &mut [SemanticEntity]) {
    if !has_duplicate_entity_ids(entities) {
        return;
    }

    // Each pass can expose child ID collisions at the next descendant level.
    // The entity count bounds the maximum number of parent-child propagation steps.
    for _ in 0..=entities.len() {
        let rewrites = disambiguate_current_entity_ids(entities);
        if rewrites.is_empty() {
            assert_unique_entity_ids(entities);
            return;
        }

        propagate_parent_id_rewrites(entities, rewrites);
    }

    assert_unique_entity_ids(entities);
}

fn has_duplicate_entity_ids(entities: &[SemanticEntity]) -> bool {
    let mut seen = HashSet::with_capacity(entities.len());
    for entity in entities {
        if !seen.insert(entity.id.as_str()) {
            return true;
        }
    }
    false
}

fn disambiguate_current_entity_ids(entities: &mut [SemanticEntity]) -> IdRewrites {
    let mut id_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, entity) in entities.iter().enumerate() {
        id_indices.entry(entity.id.clone()).or_default().push(i);
    }

    let mut rewrites: IdRewrites = HashMap::new();
    for (_id, indices) in &id_indices {
        if indices.len() > 1 {
            let mut indices = indices.clone();
            indices.sort_unstable();

            let mut line_counts: HashMap<usize, usize> = HashMap::new();
            for &idx in &indices {
                *line_counts.entry(entities[idx].start_line).or_default() += 1;
            }

            let mut line_ordinals: HashMap<usize, usize> = HashMap::new();
            for &idx in &indices {
                let e = &entities[idx];
                let new_id = if line_counts[&e.start_line] > 1 {
                    let ordinal = line_ordinals.entry(e.start_line).or_default();
                    *ordinal += 1;
                    build_entity_id_disambiguated_with_ordinal(
                        &e.file_path,
                        &e.entity_type,
                        &e.name,
                        e.parent_id.as_deref(),
                        e.start_line,
                        *ordinal,
                    )
                } else {
                    build_entity_id_disambiguated(
                        &e.file_path,
                        &e.entity_type,
                        &e.name,
                        e.parent_id.as_deref(),
                        e.start_line,
                    )
                };
                let old_id = std::mem::replace(&mut entities[idx].id, new_id.clone());
                if old_id != new_id {
                    rewrites.entry(old_id).or_default().push((idx, new_id));
                }
            }
        }
    }

    rewrites
}

fn propagate_parent_id_rewrites(entities: &mut [SemanticEntity], mut rewrites: IdRewrites) {
    while !rewrites.is_empty() {
        let mut child_rewrites: IdRewrites = HashMap::new();

        for child_idx in 0..entities.len() {
            let Some(parent_id) = entities[child_idx].parent_id.clone() else {
                continue;
            };
            let Some(candidates) = rewrites.get(&parent_id) else {
                continue;
            };
            let Some(new_parent_id) = select_rewritten_parent_id(entities, child_idx, candidates)
            else {
                continue;
            };

            entities[child_idx].parent_id = Some(new_parent_id.clone());
            let new_child_id = build_entity_id(
                &entities[child_idx].file_path,
                &entities[child_idx].entity_type,
                &entities[child_idx].name,
                Some(&new_parent_id),
            );
            let old_child_id = std::mem::replace(&mut entities[child_idx].id, new_child_id.clone());
            if old_child_id != new_child_id {
                child_rewrites
                    .entry(old_child_id)
                    .or_default()
                    .push((child_idx, new_child_id));
            }
        }

        rewrites = child_rewrites;
    }
}

fn select_rewritten_parent_id(
    entities: &[SemanticEntity],
    child_idx: usize,
    candidates: &[(usize, String)],
) -> Option<String> {
    let child = &entities[child_idx];
    let mut best: Option<((u8, u8, u8, usize, usize), String)> = None;

    for (parent_idx, parent_id) in candidates {
        if *parent_idx == child_idx {
            continue;
        }
        let parent = &entities[*parent_idx];
        let same_file_rank = if parent.file_path == child.file_path {
            0
        } else {
            1
        };
        let before_rank = if *parent_idx < child_idx { 0 } else { 1 };
        let line_span_contains_child =
            parent.start_line <= child.start_line && child.end_line <= parent.end_line;
        let line_span_differs =
            (parent.start_line, parent.end_line) != (child.start_line, child.end_line);
        let contains_rank = if line_span_contains_child && line_span_differs {
            0
        } else {
            1
        };
        let distance = parent_idx.abs_diff(child_idx);
        let span = parent.end_line.saturating_sub(parent.start_line);
        let key = (same_file_rank, contains_rank, before_rank, distance, span);

        if match best.as_ref() {
            Some((best_key, _)) => key < *best_key,
            None => true,
        } {
            best = Some((key, parent_id.clone()));
        }
    }

    best.map(|(_, parent_id)| parent_id)
}

fn assert_unique_entity_ids(entities: &[SemanticEntity]) {
    let mut seen = HashSet::with_capacity(entities.len());
    for entity in entities {
        assert!(
            seen.insert(entity.id.as_str()),
            "duplicate semantic entity id generated: {}",
            entity.id
        );
    }
}

fn attach_go_package_metadata(root: Node, source: &[u8], entities: &mut [SemanticEntity]) {
    let Some(package_name) = extract_go_package_name(root, source) else {
        return;
    };

    for entity in entities {
        entity
            .metadata
            .get_or_insert_with(HashMap::new)
            .insert("go.package".to_string(), package_name.clone());
    }
}

fn extract_go_package_name(root: Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "package_clause" {
            continue;
        }

        if let Some(name) = child.child_by_field_name("name") {
            return Some(node_text(name, source).to_string());
        }

        let mut package_cursor = child.walk();
        for package_child in child.named_children(&mut package_cursor) {
            if matches!(package_child.kind(), "package_identifier" | "identifier") {
                return Some(node_text(package_child, source).to_string());
            }
        }
    }

    None
}

fn visit_node(
    root: Node,
    file_path: &str,
    config: &LanguageConfig,
    entities: &mut Vec<SemanticEntity>,
    root_parent_id: Option<&str>,
    source: &[u8],
    root_suppression: Option<&str>,
) {
    // Iterative worklist to avoid stack overflow on deeply nested ASTs.
    // Fixes: https://github.com/Ataraxy-Labs/sem/issues/103
    // Each entry: (node, parent_id, suppression_context)
    let mut worklist: Vec<(Node, Option<String>, Option<String>)> = vec![(
        root,
        root_parent_id.map(str::to_owned),
        root_suppression.map(str::to_owned),
    )];
    let mut ts_implementation_names_by_scope: HashMap<(usize, usize), HashSet<String>> =
        HashMap::new();

    while let Some((node, pid_owned, sup_owned)) = worklist.pop() {
        let parent_id = pid_owned.as_deref();
        let suppression_context = sup_owned.as_deref();
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
                        let nested: Vec<_> = child.named_children(&mut inner_cursor).collect();
                        for n in nested.into_iter().rev() {
                            worklist.push((n, Some(entity_id.clone()), sup_owned.clone()));
                        }
                    }
                }
                continue;
            }
        }

        // EDN map-entry extraction: treat each keyword key–value pair in a top-level
        // map_lit as a named "entry" entity. Values are kept as opaque content and are
        // not recursed into, so nested maps don't leak inner entries as entities.
        if node_type == "map_lit" && config.extract_map_entries() {
            let mut cursor = node.walk();
            // Skip comment and dis_expr nodes: they are named children of map_lit
            // in tree-sitter-clojure but are not actual forms. Without this filter,
            // a comment inside a map shifts the key/value pairing by one slot,
            // causing the entry after the comment to be misidentified as a key.
            let children: Vec<_> = node
                .named_children(&mut cursor)
                .filter(|n| !matches!(n.kind(), "comment" | "dis_expr"))
                .collect();
            let mut i = 0;
            while i + 1 < children.len() {
                let key = children[i];
                let value = children[i + 1];
                i += 2;
                let name = match key.kind() {
                    "kwd_lit" | "sym_lit" => node_text(key, source).to_string(),
                    _ => continue,
                };
                let content = std::str::from_utf8(&source[key.start_byte()..value.end_byte()])
                    .unwrap_or("")
                    .to_string();
                let struct_hash = compute_structural_hash(value, source);
                let entity = SemanticEntity {
                    id: build_entity_id(file_path, "entry", &name, parent_id),
                    file_path: file_path.to_string(),
                    entity_type: "entry".to_string(),
                    name,
                    parent_id: parent_id.map(String::from),
                    content_hash: content_hash(&content),
                    structural_hash: Some(struct_hash),
                    content,
                    start_line: key.start_position().row + 1,
                    end_line: value.end_position().row + 1,
                    metadata: None,
                };
                entities.push(entity);
            }
            continue;
        }

        // Handle Clojure-style list form entities: list_lit whose first sym_lit child
        // is a defining keyword (defn, ns, defrecord, etc.).
        // Gated on call_entity_identifiers being non-empty, mirroring the `call` branch
        // used for Elixir. The config's call_entity_identifiers list is the whitelist.
        if node_type == "list_lit" && !config.call_entity_identifiers.is_empty() {
            if let Some((name, entity_type)) = extract_list_form_entity(node, config, source) {
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
                entities.push(entity);
            }
            // Always skip children: in practice, Clojure definitions are always top-level
            // list forms, nesting defn would be so unidiomatic it's not worth checking for.
            continue;
        }

        // OCaml: value_definition, module_definition, class_definition, and
        // class_type_definition can each contain multiple bindings via `... and ...`.
        // Extract each binding as a separate entity.
        if node_type == "value_definition" && config.entity_node_types.contains(&node_type) {
            let mut cursor = node.walk();
            let bindings: Vec<_> = node
                .named_children(&mut cursor)
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
                continue;
            }
        }

        if node_type == "module_definition" && config.entity_node_types.contains(&node_type) {
            let extracted = extract_ocaml_named_bindings(
                node,
                "module_binding",
                "module_name",
                map_node_type(node_type),
                file_path,
                parent_id,
                source,
                config,
                entities,
            );
            if extracted {
                continue;
            }
        }

        if node_type == "class_definition" && config.entity_node_types.contains(&node_type) {
            let extracted = extract_ocaml_named_bindings(
                node,
                "class_binding",
                "class_name",
                map_node_type(node_type),
                file_path,
                parent_id,
                source,
                config,
                entities,
            );
            if extracted {
                continue;
            }
        }

        if node_type == "class_type_definition" && config.entity_node_types.contains(&node_type) {
            let extracted = extract_ocaml_named_bindings(
                node,
                "class_type_binding",
                "class_type_name",
                map_node_type(node_type),
                file_path,
                parent_id,
                source,
                config,
                entities,
            );
            if extracted {
                continue;
            }
        }

        // TypeScript/JS multi-declarator: `const a = 1, b = 2` should produce
        // one entity per declarator instead of collapsing to the first name (#149).
        if (node_type == "lexical_declaration" || node_type == "variable_declaration")
            && matches!(config.id, "typescript" | "tsx" | "javascript")
            && config.entity_node_types.contains(&node_type)
        {
            let mut cursor = node.walk();
            let declarators: Vec<_> = node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "variable_declarator")
                .collect();
            if declarators.len() > 1 {
                let skip_declaration = should_skip_entity(config, suppression_context, node_type);
                let mut initializer_children = Vec::new();
                for declarator in &declarators {
                    let emitted_entity_id =
                        if let Some(name_node) = declarator.child_by_field_name("name") {
                            let entity_type =
                                map_js_ts_declarator_entity_type(node, *declarator, config);
                            if !skip_declaration || entity_type == "function" {
                                let name = node_text(name_node, source).to_string();
                                let content = node_text(*declarator, source).to_string();
                                let struct_hash = compute_structural_hash(*declarator, source);
                                let entity = SemanticEntity {
                                    id: build_entity_id(file_path, entity_type, &name, parent_id),
                                    file_path: file_path.to_string(),
                                    entity_type: entity_type.to_string(),
                                    name,
                                    parent_id: parent_id.map(String::from),
                                    content_hash: content_hash(&content),
                                    structural_hash: Some(struct_hash),
                                    content,
                                    start_line: declarator.start_position().row + 1,
                                    end_line: declarator.end_position().row + 1,
                                    metadata: None,
                                };

                                let entity_id = entity.id.clone();
                                entities.push(entity);
                                Some(entity_id)
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                    // Suppressed local declarators do not have an entity of
                    // their own, so their initializer is traversed under the
                    // surrounding parent, matching the single-declarator path.
                    let initializer_parent = emitted_entity_id.as_deref().or(parent_id);
                    initializer_children.extend(js_ts_initializer_children(
                        config,
                        *declarator,
                        initializer_parent,
                    ));
                }
                // The worklist is LIFO; push in reverse so initializers are
                // visited in source order.
                for initializer_child in initializer_children.into_iter().rev() {
                    worklist.push(initializer_child);
                }
                // The multi-declarator branch has already emitted each
                // declarator and queued initializer traversal. Continuing here
                // prevents the declaration node from also emitting only the
                // first declarator through the generic path below.
                continue;
            }
        }

        if node_type == "property_declaration"
            && config.id == "swift"
            && config.entity_node_types.contains(&node_type)
        {
            let bindings = extract_swift_property_bindings(node, source);
            if bindings.len() > 1 {
                let should_skip = should_skip_entity(config, suppression_context, node_type);
                if !should_skip {
                    for binding in bindings {
                        let entity_type = map_entity_type(node, config);
                        entities.push(SemanticEntity {
                            id: build_entity_id(file_path, entity_type, &binding.name, parent_id),
                            file_path: file_path.to_string(),
                            entity_type: entity_type.to_string(),
                            name: binding.name,
                            parent_id: parent_id.map(String::from),
                            content_hash: content_hash(&binding.content),
                            structural_hash: Some(binding.structural_hash),
                            content: binding.content,
                            start_line: binding.start_line,
                            end_line: binding.end_line,
                            metadata: None,
                        });
                    }
                    continue;
                }
            }
        }

        // Go grouped declarations: `var ( a = 1; b = 2 )` should produce
        // one entity per spec instead of collapsing to the first (#149).
        if (node_type == "var_declaration"
            || node_type == "const_declaration"
            || node_type == "type_declaration")
            && config.id == "go"
            && config.entity_node_types.contains(&node_type)
        {
            let spec_kinds = ["var_spec", "const_spec", "type_spec"];
            let list_kinds = ["var_spec_list", "type_spec_list"];
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            let mut specs: Vec<Node> = Vec::new();
            for child in &children {
                if spec_kinds.contains(&child.kind()) {
                    specs.push(*child);
                } else if list_kinds.contains(&child.kind()) {
                    let mut inner = child.walk();
                    for spec in child.named_children(&mut inner) {
                        if spec_kinds.contains(&spec.kind()) {
                            specs.push(spec);
                        }
                    }
                }
            }
            if specs.len() > 1 {
                let should_skip = should_skip_entity(config, suppression_context, node_type);
                if !should_skip {
                    for spec in &specs {
                        if let Some(name_node) = spec.child_by_field_name("name") {
                            let name = node_text(name_node, source).to_string();
                            let entity_type = map_entity_type(node, config);
                            let content = node_text(*spec, source).to_string();
                            let struct_hash = compute_structural_hash(*spec, source);
                            let entity = SemanticEntity {
                                id: build_entity_id(file_path, entity_type, &name, parent_id),
                                file_path: file_path.to_string(),
                                entity_type: entity_type.to_string(),
                                name,
                                parent_id: parent_id.map(String::from),
                                content_hash: content_hash(&content),
                                structural_hash: Some(struct_hash),
                                content,
                                start_line: spec.start_position().row + 1,
                                end_line: spec.end_position().row + 1,
                                metadata: None,
                            };
                            entities.push(entity);
                        }
                    }
                }
                continue;
            }
        }

        // JS/TS test call expressions: describe("name", () => {}), test(...), it(...), etc.
        if node_type == "call_expression"
            && matches!(config.id, "typescript" | "tsx" | "javascript")
        {
            if let Some((test_name, test_entity_type, is_container)) =
                extract_js_test_call(node, source)
            {
                let content_str = node_text(node, source);
                let content = content_str.to_string();
                let struct_hash = compute_structural_hash(node, source);
                let entity = SemanticEntity {
                    id: build_entity_id(file_path, test_entity_type, &test_name, parent_id),
                    file_path: file_path.to_string(),
                    entity_type: test_entity_type.to_string(),
                    name: test_name.clone(),
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

                if is_container {
                    // Recurse into the callback body to extract nested test entities
                    if let Some(callback) = find_test_callback(node) {
                        if let Some(body) = callback.child_by_field_name("body") {
                            let mut cursor = body.walk();
                            let nested: Vec<_> = body.named_children(&mut cursor).collect();
                            for n in nested.into_iter().rev() {
                                worklist.push((n, Some(entity_id.clone()), sup_owned.clone()));
                            }
                        }
                    }
                }
                continue;
            }
        }

        if should_skip_ts_overload_signature(
            node,
            config,
            source,
            &mut ts_implementation_names_by_scope,
        ) {
            continue;
        }

        if let Some((name, value)) =
            extract_js_ts_object_function_pair(node, config, source, suppression_context)
        {
            let content = node_text(node, source).to_string();
            let struct_hash = compute_structural_hash(node, source);
            let entity = SemanticEntity {
                id: build_entity_id(file_path, "method", &name, parent_id),
                file_path: file_path.to_string(),
                entity_type: "method".to_string(),
                name,
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
            worklist.push((value, Some(entity_id), Some(value.kind().to_string())));
            continue;
        }

        if config.entity_node_types.contains(&node_type) {
            if let Some(name) = extract_name(node, source) {
                let name = qualify_hcl_name(&name, node_type, parent_id, suppression_context);
                let entity_type = map_entity_type(node, config);
                let should_skip = should_skip_entity(config, suppression_context, node_type)
                    && promote_js_ts_const_function(node, config).is_none();
                if !should_skip {
                    // Go method_declaration: extract receiver type for parent linkage.
                    // e.g. `func (t *Transaction) Execute(...)` -> parent is Transaction struct
                    let effective_parent =
                        if node_type == "method_declaration" && parent_id.is_none() {
                            extract_go_receiver_struct(node, source, file_path, entities)
                        } else {
                            None
                        };
                    let parent_ref = effective_parent.as_deref().or(parent_id);

                    // Dart top-level signatures are split from their body node.
                    // When a sibling function_body exists, extend the entity to
                    // cover the full definition so body changes are detected.
                    let body = if config.id == "dart" {
                        sibling_function_body(node)
                    } else {
                        None
                    };
                    let end_byte = body.map_or(node.end_byte(), |b| b.end_byte());
                    let end_line =
                        body.map_or(node.end_position().row + 1, |b| b.end_position().row + 1);

                    // Extend start backward to include outer attributes (e.g. Rust
                    // #[derive(...)], #[cfg(...)], #[test]) so attribute changes
                    // are captured as part of the entity diff.
                    let (start_byte, start_line) = preceding_attributes_start(node, config).map_or(
                        (node.start_byte(), node.start_position().row + 1),
                        |(sb, sr)| (sb, sr + 1),
                    );

                    let content = std::str::from_utf8(&source[start_byte..end_byte])
                        .unwrap_or("")
                        .to_string();
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
                        start_line,
                        end_line,
                        metadata: None,
                    };

                    let entity_id = entity.id.clone();
                    entities.push(entity);

                    // Visit children for nested entities (methods inside classes, etc.)
                    let next_suppression = Some(node_type.to_string());
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        if config.container_node_types.contains(&child.kind()) {
                            let mut inner_cursor = child.walk();
                            let nested: Vec<_> = child.named_children(&mut inner_cursor).collect();
                            for n in nested.into_iter().rev() {
                                worklist.push((
                                    n,
                                    Some(entity_id.clone()),
                                    next_suppression.clone(),
                                ));
                            }
                        }
                    }

                    // For JS/TS variable declarations and class fields, traverse
                    // initializers that can contain nested entity declarations.
                    if node_type == "lexical_declaration" || node_type == "variable_declaration" {
                        let mut vd_cursor = node.walk();
                        for child in node.named_children(&mut vd_cursor) {
                            if child.kind() == "variable_declarator" {
                                push_js_ts_initializer_children(
                                    &mut worklist,
                                    config,
                                    child,
                                    &entity_id,
                                );
                            }
                        }
                    } else if node_type == "public_field_definition"
                        || node_type == "field_definition"
                    {
                        push_js_ts_initializer_children(&mut worklist, config, node, &entity_id);
                    }

                    continue;
                }
            }
        }

        if node_type == "export_statement"
            && matches!(config.id, "typescript" | "tsx" | "javascript")
            && node.child_by_field_name("source").is_some()
        {
            if emit_js_ts_re_export_entities(node, file_path, parent_id, source, entities) {
                continue;
            }
        }

        // For export statements, look inside for the actual declaration
        if node_type == "export_statement" {
            if let Some(declaration) = node.child_by_field_name("declaration") {
                worklist.push((declaration, pid_owned, sup_owned));
                continue;
            }
        }

        // Visit all named children. When we enter a scope boundary (e.g. arrow
        // functions, function expressions) we propagate the boundary's node type
        // as the suppression context so that suppressed_nested_entities rules
        // filter out local variable declarations while still allowing inner
        // class/function declarations to be extracted.
        // Children are pushed in reverse order so left-to-right processing is
        // preserved when popping from the worklist.
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            let child_enclosing = if config.scope_boundary_types.contains(&child.kind()) {
                Some(child.kind().to_string())
            } else {
                sup_owned.clone()
            };
            worklist.push((child, pid_owned.clone(), child_enclosing));
        }
    }
}

fn emit_js_ts_re_export_entities(
    node: Node,
    file_path: &str,
    parent_id: Option<&str>,
    source: &[u8],
    entities: &mut Vec<SemanticEntity>,
) -> bool {
    let source_path = node
        .child_by_field_name("source")
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("")
        .trim_matches(|c: char| c == '\'' || c == '"');
    if source_path.is_empty() {
        return false;
    }

    let mut emitted = false;
    let mut worklist = vec![node];
    while let Some(current) = worklist.pop() {
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            if child.kind() == "export_specifier" {
                let original = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(clean_js_ts_export_name)
                    .unwrap_or_default();
                let local = child
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(clean_js_ts_export_name)
                    .unwrap_or_else(|| original.clone());

                if local.is_empty() {
                    continue;
                }

                let content = node_text(node, source).to_string();
                let mut metadata = HashMap::new();
                metadata.insert("export.source".to_string(), source_path.to_string());
                if !original.is_empty() {
                    metadata.insert("export.original".to_string(), original);
                }

                entities.push(SemanticEntity {
                    id: build_entity_id(file_path, "export", &local, parent_id),
                    file_path: file_path.to_string(),
                    entity_type: "export".to_string(),
                    name: local,
                    parent_id: parent_id.map(String::from),
                    content_hash: content_hash(&content),
                    structural_hash: Some(compute_structural_hash(child, source)),
                    content,
                    start_line: child.start_position().row + 1,
                    end_line: child.end_position().row + 1,
                    metadata: Some(metadata),
                });
                emitted = true;
            } else {
                worklist.push(child);
            }
        }
    }

    emitted
}

fn clean_js_ts_export_name(name: &str) -> String {
    name.trim()
        .strip_prefix("type ")
        .unwrap_or(name.trim())
        .to_string()
}

#[derive(Clone)]
struct RecoveredSwiftContainer {
    name: String,
    entity_type: &'static str,
    start_byte: usize,
    name_start_byte: usize,
    name_end_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
}

// tree-sitter-swift 0.7 fails to keep class-like declarations intact when a
// body contains #if/#else/#endif. Recover that container from the ERROR node
// and reparent declarations that the normal walk extracted as file-scope.
fn recover_swift_conditional_compilation_containers(
    root: Node,
    file_path: &str,
    config: &LanguageConfig,
    entities: &mut Vec<SemanticEntity>,
    source: &[u8],
) {
    if config.id != "swift" {
        return;
    }

    let mut recovered = Vec::new();
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        if node.kind() == "ERROR" {
            if let Some(container) = parse_swift_recovered_container(node, source) {
                if !recovered.iter().any(|existing: &RecoveredSwiftContainer| {
                    existing.start_byte == container.start_byte
                }) {
                    recovered.push(container);
                }
            }
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            worklist.push(child);
        }
    }

    if recovered.is_empty() {
        return;
    }

    for entity in entities.iter_mut() {
        if entity.parent_id.is_some() {
            continue;
        }

        let Some(container) = recovered
            .iter()
            .filter(|container| {
                entity.start_line > container.start_line && entity.end_line <= container.end_line
            })
            .min_by_key(|container| container.end_line - container.start_line)
        else {
            continue;
        };

        let parent_id = build_entity_id(file_path, container.entity_type, &container.name, None);
        entity.parent_id = Some(parent_id.clone());
        entity.id = build_entity_id(
            &entity.file_path,
            &entity.entity_type,
            &entity.name,
            Some(&parent_id),
        );
    }

    for container in recovered {
        let already_extracted = entities.iter().any(|entity| {
            entity.name == container.name
                && entity.entity_type == container.entity_type
                && entity.start_line == container.start_line
        });
        if already_extracted {
            continue;
        }

        let content = std::str::from_utf8(&source[container.start_byte..container.end_byte])
            .unwrap_or("")
            .to_string();
        let name_range = container
            .name_start_byte
            .checked_sub(container.start_byte)
            .zip(container.name_end_byte.checked_sub(container.start_byte));
        let struct_hash = match name_range {
            Some((name_start, name_end)) => {
                recovered_swift_structural_hash(&content, name_start, name_end)
            }
            None => recovered_swift_structural_hash(&content, 0, 0),
        };
        entities.push(SemanticEntity {
            id: build_entity_id(file_path, container.entity_type, &container.name, None),
            file_path: file_path.to_string(),
            entity_type: container.entity_type.to_string(),
            name: container.name,
            parent_id: None,
            content_hash: content_hash(&content),
            structural_hash: Some(struct_hash),
            content,
            start_line: container.start_line,
            end_line: container.end_line,
            metadata: None,
        });
    }

    entities.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then_with(|| b.end_line.cmp(&a.end_line))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn parse_swift_recovered_container(node: Node, source: &[u8]) -> Option<RecoveredSwiftContainer> {
    let start_byte = node.start_byte();
    let source_from_node = std::str::from_utf8(&source[start_byte..]).ok()?;
    let open_brace_offset = source_from_node.find('{')?;
    let header = &source_from_node[..open_brace_offset];
    let (entity_type, name, name_start, name_end) = parse_swift_type_header(header)?;

    let open_brace = start_byte + open_brace_offset;
    let end_byte = find_matching_brace(source, open_brace)?;
    let closing_brace_byte = end_byte.checked_sub(1)?;
    let content = std::str::from_utf8(&source[start_byte..end_byte]).ok()?;
    if !swift_contains_conditional_directive(content) {
        return None;
    }

    Some(RecoveredSwiftContainer {
        name,
        entity_type,
        start_byte,
        name_start_byte: start_byte + name_start,
        name_end_byte: start_byte + name_end,
        end_byte,
        start_line: node.start_position().row + 1,
        end_line: line_number_for_byte(source, closing_brace_byte),
    })
}

fn parse_swift_type_header(header: &str) -> Option<(&'static str, String, usize, usize)> {
    let mut offset = 0;
    while let Some((word, _, end)) = next_swift_word(header, offset) {
        if let Some(entity_type) = swift_declaration_keyword_type(word) {
            let (name, name_start, name_end) =
                swift_name_after_declaration_keyword(header, word, end)?;
            return Some((entity_type, name.to_string(), name_start, name_end));
        }
        offset = end;
    }
    None
}

fn swift_name_after_declaration_keyword<'a>(
    header: &'a str,
    keyword: &str,
    offset: usize,
) -> Option<(&'a str, usize, usize)> {
    if keyword != "extension" {
        return next_swift_word(header, offset);
    }

    let mut start = offset;
    while start < header.len() {
        let ch = header[start..].chars().next()?;
        if !ch.is_whitespace() {
            break;
        }
        start += ch.len_utf8();
    }

    let mut angle_depth = 0usize;
    let mut end = start;
    while end < header.len() {
        let ch = header[end..].chars().next()?;
        if ch == '<' {
            angle_depth += 1;
        } else if ch == '>' {
            angle_depth = angle_depth.saturating_sub(1);
        } else if angle_depth == 0 && (ch.is_whitespace() || ch == ':' || ch == '{') {
            break;
        }
        end += ch.len_utf8();
    }

    let trimmed = header[start..end].trim_end();
    let trimmed_end = start + trimmed.len();
    (start < trimmed_end).then_some((&header[start..trimmed_end], start, trimmed_end))
}

fn next_swift_word(input: &str, mut offset: usize) -> Option<(&str, usize, usize)> {
    while offset < input.len() {
        let ch = input[offset..].chars().next()?;
        if is_swift_identifier_start(ch) {
            break;
        }
        offset += ch.len_utf8();
    }

    let start = offset;
    while offset < input.len() {
        let ch = input[offset..].chars().next()?;
        if !is_swift_identifier_continue(ch) {
            break;
        }
        offset += ch.len_utf8();
    }

    (start < offset).then_some((&input[start..offset], start, offset))
}

fn is_swift_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_swift_identifier_continue(ch: char) -> bool {
    is_swift_identifier_start(ch) || ch.is_ascii_digit()
}

fn swift_declaration_keyword_type(keyword: &str) -> Option<&'static str> {
    match keyword {
        "actor" => Some("actor"),
        "class" => Some("class"),
        "enum" => Some("enum"),
        "extension" => Some("extension"),
        "protocol" => Some("protocol"),
        "struct" => Some("struct"),
        _ => None,
    }
}

fn swift_contains_conditional_directive(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim_start();
        line == "#if"
            || line.starts_with("#if ")
            || line.starts_with("#if\t")
            || line.starts_with("#if(")
            || line == "#else"
            || line.starts_with("#elseif ")
            || line.starts_with("#elseif\t")
            || line == "#endif"
    })
}

fn find_matching_brace(source: &[u8], open_brace: usize) -> Option<usize> {
    if source.get(open_brace) != Some(&b'{') {
        return None;
    }

    let mut depth = 1usize;
    let mut i = open_brace + 1;
    while i < source.len() {
        if source[i] == b'/' && source.get(i + 1) == Some(&b'/') {
            i = skip_swift_line_comment(source, i + 2);
            continue;
        }

        if source[i] == b'/' && source.get(i + 1) == Some(&b'*') {
            i = skip_swift_block_comment(source, i + 2);
            continue;
        }

        if let Some(string_start) = swift_string_literal_start(source, i) {
            i = skip_swift_string_literal(source, string_start);
            continue;
        }

        match source[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    // Return the byte after the closing brace for use as a slice end.
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }

    None
}

fn skip_swift_line_comment(source: &[u8], mut i: usize) -> usize {
    while i < source.len() && source[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_swift_block_comment(source: &[u8], mut i: usize) -> usize {
    while i + 1 < source.len() {
        if source[i] == b'*' && source[i + 1] == b'/' {
            return i + 2;
        }
        i += 1;
    }
    source.len()
}

#[derive(Clone, Copy)]
struct SwiftStringLiteralStart {
    body_start: usize,
    hash_count: usize,
    is_multiline: bool,
}

const SWIFT_STRING_INTERPOLATION_NESTING_LIMIT: usize = 256;

fn swift_string_literal_start(source: &[u8], start: usize) -> Option<SwiftStringLiteralStart> {
    let mut quote = start;
    let mut hash_count = 0usize;
    while source.get(quote) == Some(&b'#') {
        hash_count += 1;
        quote += 1;
    }

    if source.get(quote) != Some(&b'"') {
        return None;
    }

    let is_multiline = source.get(quote + 1) == Some(&b'"') && source.get(quote + 2) == Some(&b'"');
    let body_start = quote + if is_multiline { 3 } else { 1 };
    Some(SwiftStringLiteralStart {
        body_start,
        hash_count,
        is_multiline,
    })
}

fn skip_swift_string_literal(source: &[u8], start: SwiftStringLiteralStart) -> usize {
    skip_swift_string_literal_with_depth(source, start, 0)
}

fn skip_swift_string_literal_with_depth(
    source: &[u8],
    start: SwiftStringLiteralStart,
    interpolation_depth: usize,
) -> usize {
    if start.is_multiline {
        skip_swift_multiline_string(
            source,
            start.body_start,
            start.hash_count,
            interpolation_depth,
        )
    } else {
        skip_swift_string(
            source,
            start.body_start,
            start.hash_count,
            interpolation_depth,
        )
    }
}

fn skip_swift_multiline_string(
    source: &[u8],
    mut i: usize,
    hash_count: usize,
    interpolation_depth: usize,
) -> usize {
    while i < source.len() {
        if let Some(interpolation_start) = swift_interpolation_start(source, i, hash_count) {
            i = skip_swift_interpolation(source, interpolation_start, interpolation_depth + 1);
            continue;
        }

        if let Some(escaped_start) = swift_escape_payload_start(source, i, hash_count) {
            i = (escaped_start + 1).min(source.len());
            continue;
        }

        if swift_string_closing_at(source, i, hash_count, 3) {
            return i + 3 + hash_count;
        }

        i += 1;
    }
    source.len()
}

fn skip_swift_string(
    source: &[u8],
    mut i: usize,
    hash_count: usize,
    interpolation_depth: usize,
) -> usize {
    while i < source.len() {
        if let Some(interpolation_start) = swift_interpolation_start(source, i, hash_count) {
            i = skip_swift_interpolation(source, interpolation_start, interpolation_depth + 1);
            continue;
        }

        if let Some(escaped_start) = swift_escape_payload_start(source, i, hash_count) {
            i = (escaped_start + 1).min(source.len());
            continue;
        }

        if swift_string_closing_at(source, i, hash_count, 1) {
            return i + 1 + hash_count;
        }

        i += 1;
    }
    source.len()
}

fn skip_swift_interpolation(source: &[u8], mut i: usize, interpolation_depth: usize) -> usize {
    if interpolation_depth > SWIFT_STRING_INTERPOLATION_NESTING_LIMIT {
        return source.len();
    }

    let mut depth = 1usize;
    while i < source.len() {
        if source[i] == b'/' && source.get(i + 1) == Some(&b'/') {
            i = skip_swift_line_comment(source, i + 2);
            continue;
        }

        if source[i] == b'/' && source.get(i + 1) == Some(&b'*') {
            i = skip_swift_block_comment(source, i + 2);
            continue;
        }

        if let Some(string_start) = swift_string_literal_start(source, i) {
            i = skip_swift_string_literal_with_depth(source, string_start, interpolation_depth);
            continue;
        }

        match source[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                let Some(new_depth) = depth.checked_sub(1) else {
                    return source.len();
                };
                depth = new_depth;
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    source.len()
}

fn swift_interpolation_start(source: &[u8], backslash: usize, hash_count: usize) -> Option<usize> {
    let payload_start = swift_escape_payload_start(source, backslash, hash_count)?;
    (source.get(payload_start) == Some(&b'(')).then_some(payload_start + 1)
}

fn swift_escape_payload_start(source: &[u8], backslash: usize, hash_count: usize) -> Option<usize> {
    if source.get(backslash) != Some(&b'\\') {
        return None;
    }

    let mut i = backslash + 1;
    for _ in 0..hash_count {
        if source.get(i) != Some(&b'#') {
            return None;
        }
        i += 1;
    }
    (i < source.len()).then_some(i)
}

fn swift_string_closing_at(
    source: &[u8],
    quote: usize,
    hash_count: usize,
    quote_count: usize,
) -> bool {
    for offset in 0..quote_count {
        if source.get(quote + offset) != Some(&b'"') {
            return false;
        }
    }
    for hash_offset in 0..hash_count {
        if source.get(quote + quote_count + hash_offset) != Some(&b'#') {
            return false;
        }
    }
    true
}

fn line_number_for_byte(source: &[u8], byte: usize) -> usize {
    source[..byte.min(source.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

fn recovered_swift_structural_hash(
    content: &str,
    exclude_start: usize,
    exclude_end: usize,
) -> String {
    let (before, after) = if exclude_start <= exclude_end
        && exclude_end <= content.len()
        && content.is_char_boundary(exclude_start)
        && content.is_char_boundary(exclude_end)
    {
        (&content[..exclude_start], &content[exclude_end..])
    } else {
        (content, "")
    };

    let mut words = before.split_whitespace().collect::<Vec<_>>();
    words.extend(after.split_whitespace());
    let normalized = words.join(" ");
    content_hash(&normalized)
}

/// For languages with outer attributes/annotations that are sibling nodes
/// (e.g. Rust `#[derive(...)]`, `#[cfg(...)]`), walk backward to find the
/// earliest preceding attribute so the entity span includes them.
/// Returns (start_byte, start_row) of the first attribute if any found.
fn preceding_attributes_start(node: Node, config: &LanguageConfig) -> Option<(usize, usize)> {
    let attr_kind = match config.id {
        "rust" => "attribute_item",
        _ => return None,
    };

    let mut earliest_start_byte = node.start_byte();
    let mut earliest_start_row = node.start_position().row;
    let mut found = false;
    let mut current = node;

    while let Some(prev) = current.prev_named_sibling() {
        if prev.kind() == attr_kind {
            earliest_start_byte = prev.start_byte();
            earliest_start_row = prev.start_position().row;
            found = true;
            current = prev;
        } else {
            break;
        }
    }

    found.then_some((earliest_start_byte, earliest_start_row))
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
    let node_type = node.kind();

    if node_type == "operator_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "custom_operator" || child.kind() == "simple_identifier" {
                return Some((child.start_byte(), child.end_byte()));
            }
        }
    }

    if node_type == "subscript_declaration" || node_type == "deinit_declaration" {
        return None;
    }

    // Try 'name' field first (works for most languages)
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some((name_node.start_byte(), name_node.end_byte()));
    }

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

    // Nix bindings: name is in the attrpath field
    if node_type == "binding" {
        if let Some(attrpath) = node.child_by_field_name("attrpath") {
            return Some((attrpath.start_byte(), attrpath.end_byte()));
        }
    }

    // OCaml: individual binding nodes (used when compute_structural_hash is called
    // on a binding directly, e.g., from the multi-binding extraction in visit_node)
    if node_type == "let_binding" {
        if let Some(pattern) = node.child_by_field_name("pattern") {
            return Some((pattern.start_byte(), pattern.end_byte()));
        }
    }

    if node_type == "module_binding"
        || node_type == "class_binding"
        || node_type == "class_type_binding"
    {
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

    if node_type == "pair" {
        if let Some(key) = node.child_by_field_name("key") {
            if js_ts_object_key_name(key, _source).is_some() {
                return Some((key.start_byte(), key.end_byte()));
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
fn find_declarator_name_range(mut node: Node) -> Option<(usize, usize)> {
    loop {
        match node.kind() {
            "identifier" | "type_identifier" | "field_identifier" => {
                return Some((node.start_byte(), node.end_byte()));
            }
            "qualified_identifier" | "scoped_identifier" => {
                return Some((node.start_byte(), node.end_byte()));
            }
            "pointer_declarator"
            | "function_declarator"
            | "array_declarator"
            | "parenthesized_declarator" => {
                if let Some(inner) = node.child_by_field_name("declarator") {
                    node = inner;
                    continue;
                }
                let mut cursor = node.walk();
                return node
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                    .map(|c| (c.start_byte(), c.end_byte()));
            }
            _ => {
                if let Some(name) = node.child_by_field_name("name") {
                    return Some((name.start_byte(), name.end_byte()));
                }
                let mut cursor = node.walk();
                return node
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                    .map(|c| (c.start_byte(), c.end_byte()));
            }
        }
    }
}

fn extract_name(node: Node, source: &[u8]) -> Option<String> {
    let node_type = node.kind();

    if node_type == "subscript_declaration" {
        return Some("subscript".to_string());
    }

    if node_type == "deinit_declaration" {
        return Some("deinit".to_string());
    }

    if node_type == "operator_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "custom_operator" || child.kind() == "simple_identifier" {
                return Some(node_text(child, source).to_string());
            }
        }
    }

    // Elm value_declaration: name is inside the function_declaration_left child
    // e.g. "update msg model = ..." -> "update"
    if node_type == "value_declaration" {
        if let Some(left) = node.child_by_field_name("functionDeclarationLeft") {
            let mut cursor = left.walk();
            for child in left.named_children(&mut cursor) {
                if child.kind() == "lower_case_identifier" {
                    return Some(node_text(child, source).to_string());
                }
            }
        }
    }

    // Elm infix_declaration: name is the operator field
    // e.g. "infix right 5 (|>) = apR" -> "|>"
    if node_type == "infix_declaration" {
        if let Some(op) = node.child_by_field_name("operator") {
            return Some(node_text(op, source).to_string());
        }
    }

    // Haskell instance declarations: construct name from class name + type patterns
    // e.g. "instance Eq Bool where" -> "Eq Bool"
    // Must be before the generic 'name' field lookup since instance has a 'name'
    // field that only captures the class name, not the full instance head.
    if node_type == "instance" {
        let name_node = node.child_by_field_name("name");
        let patterns_node = node.child_by_field_name("patterns");
        match (name_node, patterns_node) {
            (Some(name), Some(patterns)) => {
                return Some(format!(
                    "{} {}",
                    node_text(name, source),
                    node_text(patterns, source)
                ));
            }
            (Some(name), None) => {
                // Check for unnamed children (infix or parens) that hold type info
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    let kind = child.kind();
                    if kind == "infix" || kind == "parens" {
                        return Some(format!(
                            "{} {}",
                            node_text(name, source),
                            node_text(child, source)
                        ));
                    }
                }
                return Some(node_text(name, source).to_string());
            }
            _ => {}
        }
    }

    // Try 'name' field first (works for most languages)
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(name_node, source).to_string());
    }

    // For variable/lexical declarations, try to get the declarator name
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
    if node_type == "property_declaration"
        || node_type == "namespace_declaration"
        || node_type == "struct_declaration"
    {
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

    // Nix bindings: name comes from attrpath field, which contains identifier children.
    // Join multiple identifiers with dots for nested paths (e.g., "services.nginx.enable").
    if node_type == "binding" {
        if let Some(attrpath) = node.child_by_field_name("attrpath") {
            let mut parts = Vec::new();
            let mut cursor = attrpath.walk();
            for child in attrpath.children(&mut cursor) {
                if child.kind() == "identifier" || child.kind() == "string_expression" {
                    if let Ok(text) = child.utf8_text(source) {
                        parts.push(text.trim_matches('"').to_string());
                    }
                }
            }
            if !parts.is_empty() {
                return Some(parts.join("."));
            }
        }
        return None;
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

fn should_skip_ts_overload_signature(
    node: Node,
    config: &LanguageConfig,
    source: &[u8],
    implementation_names_by_scope: &mut HashMap<(usize, usize), HashSet<String>>,
) -> bool {
    if !matches!(config.id, "typescript" | "tsx") || node.kind() != "function_signature" {
        return false;
    }

    let Some(signature_name) = extract_name(node, source) else {
        return false;
    };

    let anchor = match node.parent() {
        Some(parent) if parent.kind() == "export_statement" => parent,
        _ => node,
    };

    let Some(scope) = anchor.parent() else {
        return false;
    };

    let scope_key = (scope.start_byte(), scope.end_byte());
    let implementation_names = implementation_names_by_scope
        .entry(scope_key)
        .or_insert_with(|| collect_ts_function_implementation_names(scope, source));

    implementation_names.contains(&signature_name)
}

fn collect_ts_function_implementation_names(scope: Node, source: &[u8]) -> HashSet<String> {
    let mut cursor = scope.walk();
    scope
        .named_children(&mut cursor)
        .filter_map(|sibling| ts_function_declaration_name(sibling, source))
        .collect()
}

fn ts_function_declaration_name(node: Node, source: &[u8]) -> Option<String> {
    let declaration = if node.kind() == "export_statement" {
        node.child_by_field_name("declaration")?
    } else {
        node
    };

    (declaration.kind() == "function_declaration")
        .then(|| extract_name(declaration, source))
        .flatten()
}

/// Extract the name from a C declarator (handles pointer_declarator, function_declarator, etc.)
fn extract_declarator_name(mut node: Node, source: &[u8]) -> Option<String> {
    loop {
        match node.kind() {
            "identifier" | "type_identifier" | "field_identifier" => {
                return Some(node_text(node, source).to_string())
            }
            "qualified_identifier" | "scoped_identifier" => {
                // For C++ qualified names like ClassName::method, return the full qualified name
                return Some(node_text(node, source).to_string());
            }
            "pointer_declarator"
            | "function_declarator"
            | "array_declarator"
            | "parenthesized_declarator" => {
                if let Some(inner) = node.child_by_field_name("declarator") {
                    node = inner;
                    continue;
                }
                let mut cursor = node.walk();
                return node
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                    .map(|c| node_text(c, source).to_string());
            }
            _ => {
                if let Some(name) = node.child_by_field_name("name") {
                    return Some(node_text(name, source).to_string());
                }
                let mut cursor = node.walk();
                return node
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
                    .map(|c| node_text(c, source).to_string());
            }
        }
    }
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

struct SwiftPropertyBinding {
    name: String,
    content: String,
    structural_hash: String,
    start_line: usize,
    end_line: usize,
}

struct SwiftRawPropertyBinding {
    name: String,
    segment_start_byte: usize,
    segment_end_byte: usize,
    has_type: bool,
    has_value: bool,
    type_text: Option<String>,
}

fn extract_swift_property_bindings(node: Node, source: &[u8]) -> Vec<SwiftPropertyBinding> {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    let prefix_end = children
        .iter()
        .find(|child| child.kind() == "value_binding_pattern")
        .map(|child| child.end_byte())
        .unwrap_or(node.start_byte());
    let prefix = source
        .get(node.start_byte()..prefix_end)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or("")
        .trim();

    let mut name_nodes = Vec::new();
    for index in 0..node.child_count() {
        if node.field_name_for_child(index as u32) == Some("name") {
            if let Some(child) = node.child(index as u32) {
                name_nodes.push(child);
            }
        }
    }

    if name_nodes.len() <= 1 {
        return Vec::new();
    }

    let mut raw_bindings = Vec::new();
    for (index, name_node) in name_nodes.iter().enumerate() {
        let next_name_start = name_nodes.get(index + 1).map(|next| next.start_byte());
        let segment_end_byte = next_name_start
            .and_then(|next_start| {
                children
                    .iter()
                    .find(|child| {
                        child.kind() == ","
                            && child.start_byte() >= name_node.end_byte()
                            && child.start_byte() < next_start
                    })
                    .map(|child| child.start_byte())
            })
            .unwrap_or_else(|| {
                if let Some(next_start) = next_name_start {
                    next_start
                } else {
                    let mut end_byte = name_node.end_byte();
                    for child_index in 0..node.child_count() {
                        let Some(child) = node.child(child_index as u32) else {
                            continue;
                        };
                        if child.start_byte() < name_node.end_byte() {
                            continue;
                        }
                        let field_name = node.field_name_for_child(child_index as u32);
                        if field_name == Some("type")
                            || matches!(field_name, Some("value") | Some("computed_value"))
                            || child.kind() == "type_annotation"
                        {
                            end_byte = end_byte.max(child.end_byte());
                        }
                    }
                    end_byte
                }
            });

        let mut has_type = false;
        let mut has_value = false;
        let mut type_text = None;
        for child_index in 0..node.child_count() {
            let Some(child) = node.child(child_index as u32) else {
                continue;
            };
            if child.start_byte() < name_node.end_byte() || child.start_byte() >= segment_end_byte {
                continue;
            }
            let field_name = node.field_name_for_child(child_index as u32);
            if field_name == Some("type") || child.kind() == "type_annotation" {
                has_type = true;
                let text = node_text(child, source).trim();
                if !text.is_empty() {
                    type_text = Some(text.to_string());
                }
            }
            if matches!(field_name, Some("value") | Some("computed_value")) {
                has_value = true;
            }
        }

        raw_bindings.push(SwiftRawPropertyBinding {
            name: node_text(*name_node, source).to_string(),
            segment_start_byte: name_node.start_byte(),
            segment_end_byte,
            has_type,
            has_value,
            type_text,
        });
    }

    raw_bindings
        .iter()
        .enumerate()
        .map(|(index, binding)| {
            let mut segment = source
                .get(binding.segment_start_byte..binding.segment_end_byte)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .unwrap_or("")
                .trim()
                .to_string();

            if !binding.has_type && !binding.has_value {
                if let Some(type_text) = raw_bindings
                    .iter()
                    .skip(index + 1)
                    .find_map(|next| next.type_text.as_deref())
                {
                    segment.push_str(type_text);
                }
            }

            let mut content = prefix.to_string();
            if !content.is_empty() && !segment.is_empty() {
                content.push(' ');
            }
            let name_offset = content.len();
            content.push_str(&segment);
            let name_end_offset = name_offset + binding.name.len();

            SwiftPropertyBinding {
                name: binding.name.clone(),
                structural_hash: recovered_swift_structural_hash(
                    &content,
                    name_offset,
                    name_end_offset,
                ),
                content,
                start_line: line_number_for_byte(source, binding.segment_start_byte),
                end_line: line_number_for_byte(source, binding.segment_end_byte),
            }
        })
        .collect()
}

fn map_node_type(tree_sitter_type: &str) -> &str {
    match tree_sitter_type {
        "function_declaration"
        | "generator_function_declaration"
        | "function_definition"
        | "function_item"
        | "function_signature"
        | "subroutine_declaration_statement" => "function",
        "method_declaration"
        | "method_definition"
        | "method"
        | "singleton_method"
        | "method_signature"
        | "abstract_method_signature"
        | "operator_signature" => "method",
        "class_declaration"
        | "abstract_class_declaration"
        | "class_definition"
        | "class_specifier"
        | "class" => "class",
        "interface_declaration" => "interface",
        "protocol_declaration" => "protocol",
        "init_declaration" => "init",
        "deinit_declaration" => "deinit",
        "subscript_declaration" => "subscript",
        "type_alias_declaration"
        | "typealias_declaration"
        | "type_declaration"
        | "type_item"
        | "type_definition"
        | "type_alias"
        | "type_synomym"
        | "type_family" => "type",
        "associatedtype_declaration" => "associatedtype",
        "operator_declaration" => "operator",
        "enum_declaration" | "enum_item" | "enum_specifier" | "enum_definition" => "enum",
        "mixin_declaration" => "mixin",
        "extension_declaration" | "extension_type_declaration" => "extension",
        "getter_signature" => "getter",
        "setter_signature" => "setter",
        "record_declaration" | "record_struct_declaration" => "record",
        "struct_item" | "struct_specifier" | "struct_declaration" => "struct",
        "union_specifier" => "union",
        "impl_item" => "impl",
        "trait_item" => "trait",
        "mod_item"
        | "module"
        | "module_definition"
        | "namespace_definition"
        | "namespace_declaration"
        | "package_object" => "module",
        "object_definition" => "object",
        "trait_definition" => "trait",
        "val_definition" => "val",
        "given_definition" => "given",
        "extension_definition" => "extension",
        "package_statement" => "package",
        "export_statement" => "export",
        "lexical_declaration" | "variable_declaration" | "var_declaration" | "declaration" => {
            "variable"
        }
        "const_declaration" | "const_item" => "constant",
        "signature" => "signature",
        "instance" => "instance",
        "data_type" | "newtype" | "data_family" => "type",
        "pattern_synonym" => "pattern",
        "fixity" => "fixity",
        "binding" => "binding",
        "inherit" | "inherit_from" => "inherit",
        "static_item" => "static",
        "value_specification" => "val",
        "module_type_definition" => "module_type",
        "exception_definition" => "exception",
        "class_type_definition" => "class_type",
        "external" | "foreign_import" | "foreign_export" => "external",
        "decorated_definition" => "decorated_definition",
        "constructor_declaration" => "constructor",
        "field_declaration" | "public_field_definition" | "field_definition" => "field",
        "property_declaration" | "property_signature" => "property",
        "annotation_type_declaration" => "annotation",
        "template_declaration" => "template",
        other => other,
    }
}

/// Extract entity from a Clojure-style list form (list_lit where first sym_lit is a def keyword).
/// Returns (name, entity_type) or None if the list head is not in config.call_entity_identifiers.
fn extract_list_form_entity<'a>(
    node: Node<'a>,
    config: &LanguageConfig,
    source: &[u8],
) -> Option<(String, &'static str)> {
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor);

    let head = children.next()?;
    if head.kind() != "sym_lit" {
        return None;
    }
    let keyword = node_text(head, source);
    if !config.call_entity_identifiers.contains(&keyword) {
        return None;
    }
    let entity_type = match keyword {
        "defn" | "defn-" => "function",
        "defmacro" => "macro",
        "defmulti" => "multimethod",
        "defmethod" => "method",
        "defprotocol" => "protocol",
        "defrecord" => "record",
        "deftype" => "type",
        "definterface" => "interface",
        "defstruct" => "struct",
        "def" | "defonce" => "var",
        // Keyword is in call_entity_identifiers but has no entity-type mapping here.
        // This is a config/code mismatch — add an arm above when adding a new keyword.
        _ => {
            debug_assert!(false, "unhandled call_entity_identifier: {keyword}");
            return None;
        }
    };

    let name_node = children.next()?;
    // In this grammar, `^:private my-fn` is a single sym_lit whose `name` field child
    // (sym_name) holds only the bare symbol. Use that to strip metadata annotations.
    let base_name = clojure_sym_name(name_node, source).to_string();

    // defmethod: include the dispatch value so (defmethod area :circle ...) and
    // (defmethod area :rectangle ...) get distinct stable names instead of colliding.
    let name = if keyword == "defmethod" {
        if let Some(dispatch_node) = children.next() {
            format!("{}/{}", base_name, node_text(dispatch_node, source))
        } else {
            base_name
        }
    } else {
        base_name
    };

    Some((name, entity_type))
}

/// Extract the bare symbol text from a Clojure `sym_lit`, ignoring any metadata prefix.
/// `^:private my-fn` is a sym_lit whose `name` field is a sym_name holding "my-fn".
fn clojure_sym_name<'a>(node: Node<'a>, source: &'a [u8]) -> &'a str {
    if node.kind() == "sym_lit" {
        if let Some(name_child) = node.child_by_field_name("name") {
            return node_text(name_child, source);
        }
    }
    node_text(node, source)
}

/// Extract entity info from a call node (Elixir macros like def, defmodule, etc.)
fn extract_call_entity(
    node: Node,
    config: &LanguageConfig,
    source: &[u8],
) -> Option<(String, &'static str)> {
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
    let args = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "arguments")?;

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
fn extract_fn_name_from_arg(mut node: Node, source: &[u8]) -> Option<String> {
    loop {
        match node.kind() {
            "call" => {
                return if let Some(fn_target) = node.child_by_field_name("target") {
                    Some(node_text(fn_target, source).to_string())
                } else {
                    let mut c = node.walk();
                    let id = node
                        .named_children(&mut c)
                        .find(|n| n.kind() == "identifier")?;
                    Some(node_text(id, source).to_string())
                };
            }
            "identifier" => return Some(node_text(node, source).to_string()),
            "binary_operator" => {
                // defguard is_positive(x) when ... -> left side has the actual call/identifier
                node = node.child_by_field_name("left")?;
                continue;
            }
            _ => return None,
        }
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
            "method_declaration" => {
                if let Some(sig) = child.child_by_field_name("signature") {
                    let mut inner = sig.walk();
                    for inner_sig in sig.named_children(&mut inner) {
                        return match inner_sig.kind() {
                            "function_signature" => "method",
                            "getter_signature" => "getter",
                            "setter_signature" => "setter",
                            "constructor_signature" | "factory_constructor_signature" => {
                                "constructor"
                            }
                            "operator_signature" => "method",
                            _ => continue,
                        };
                    }
                }
            }
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
        "method_definition" => map_js_ts_accessor_method_type(node, config)
            .unwrap_or_else(|| map_node_type(node.kind())),
        "class_declaration" if config.id == "swift" => {
            swift_class_declaration_type(node).unwrap_or_else(|| map_node_type(node.kind()))
        }
        // C/C++ declarations with a function_declarator are function prototypes,
        // not variables (#152).
        "declaration" if matches!(config.id, "c" | "cpp") && has_function_declarator(node) => {
            "function"
        }
        _ => promote_zig_variable(node, config)
            .or_else(|| promote_js_ts_const_function(node, config))
            .unwrap_or_else(|| map_node_type(node.kind())),
    }
}

fn map_js_ts_accessor_method_type(node: Node, config: &LanguageConfig) -> Option<&'static str> {
    if !matches!(config.id, "typescript" | "tsx" | "javascript") {
        return None;
    }

    let name = node.child_by_field_name("name")?;
    let mut accessor_type = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() >= name.start_byte() {
            break;
        }
        if child.is_extra() {
            continue;
        }
        match child.kind() {
            "get" => accessor_type = Some("getter"),
            "set" => accessor_type = Some("setter"),
            _ => {}
        }
    }

    accessor_type
}

fn swift_class_declaration_type(node: Node) -> Option<&'static str> {
    let declaration_kind = node.child_by_field_name("declaration_kind")?;
    swift_declaration_keyword_type(declaration_kind.kind())
}

fn map_js_ts_declarator_entity_type(
    declaration: Node,
    declarator: Node,
    config: &LanguageConfig,
) -> &'static str {
    promote_js_ts_const_declarator_function(declaration, declarator, config)
        .unwrap_or_else(|| map_node_type(declaration.kind()))
}

/// Check whether a C/C++ `declaration` node contains a `function_declarator`
/// descendant, indicating it is a function prototype rather than a variable.
fn has_function_declarator(node: Node) -> bool {
    if let Some(declarator) = node.child_by_field_name("declarator") {
        let mut current = declarator;
        loop {
            if current.kind() == "function_declarator" {
                return true;
            }
            if let Some(inner) = current.child_by_field_name("declarator") {
                current = inner;
            } else {
                break;
            }
        }
    }
    false
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
    let declarator = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "variable_declarator")?;
    promote_js_ts_const_declarator_function(node, declarator, config)
}

fn promote_js_ts_const_declarator_function(
    declaration: Node,
    declarator: Node,
    config: &LanguageConfig,
) -> Option<&'static str> {
    if !matches!(config.id, "typescript" | "tsx" | "javascript") {
        return None;
    }

    if declaration.kind() != "lexical_declaration" {
        return None;
    }

    let declaration_kind = declaration.child_by_field_name("kind")?;
    if declaration_kind.kind() != "const" {
        return None;
    }

    let value = declarator.child_by_field_name("value")?;

    match value.kind() {
        "arrow_function" | "function_expression" | "generator_function" => Some("function"),
        _ => None,
    }
}

fn push_js_ts_initializer_children<'tree>(
    worklist: &mut Vec<(Node<'tree>, Option<String>, Option<String>)>,
    config: &LanguageConfig,
    node: Node<'tree>,
    entity_id: &str,
) {
    let initializer_children = js_ts_initializer_children(config, node, Some(entity_id));
    for initializer_child in initializer_children.into_iter().rev() {
        worklist.push(initializer_child);
    }
}

fn js_ts_initializer_children<'tree>(
    config: &LanguageConfig,
    node: Node<'tree>,
    parent_id: Option<&str>,
) -> Vec<(Node<'tree>, Option<String>, Option<String>)> {
    if !matches!(config.id, "typescript" | "tsx" | "javascript") {
        return Vec::new();
    }

    let Some(value) = js_ts_initializer_value(config, node) else {
        return Vec::new();
    };

    if value.kind() == "object" {
        return js_ts_object_initializer_children(value, parent_id);
    }

    vec![(
        value,
        parent_id.map(String::from),
        Some(value.kind().to_string()),
    )]
}

fn js_ts_object_initializer_children<'tree>(
    object: Node<'tree>,
    parent_id: Option<&str>,
) -> Vec<(Node<'tree>, Option<String>, Option<String>)> {
    let mut cursor = object.walk();
    object
        .named_children(&mut cursor)
        .filter_map(|child| {
            let suppression_context = if child.kind() == "method_definition" {
                Some(child.kind().to_string())
            } else if js_ts_pair_function_value(child).is_some() {
                Some("object".to_string())
            } else {
                None
            };

            suppression_context.map(|suppression_context| {
                (
                    child,
                    parent_id.map(String::from),
                    Some(suppression_context),
                )
            })
        })
        .collect()
}

fn js_ts_initializer_value<'tree>(
    config: &LanguageConfig,
    node: Node<'tree>,
) -> Option<Node<'tree>> {
    if let Some(value) = node.child_by_field_name("value") {
        return is_js_ts_initializer_node(config, value).then_some(value);
    }

    if !matches!(node.kind(), "public_field_definition" | "field_definition") {
        return None;
    }

    let mut cursor = node.walk();
    let initializer = node
        .named_children(&mut cursor)
        .find(|child| is_js_ts_initializer_node(config, *child));
    initializer
}

fn is_js_ts_initializer_node(config: &LanguageConfig, node: Node) -> bool {
    config.scope_boundary_types.contains(&node.kind()) || matches!(node.kind(), "class" | "object")
}

fn extract_js_ts_object_function_pair<'tree>(
    node: Node<'tree>,
    config: &LanguageConfig,
    source: &[u8],
    suppression_context: Option<&str>,
) -> Option<(String, Node<'tree>)> {
    if !matches!(config.id, "typescript" | "tsx" | "javascript")
        || suppression_context != Some("object")
    {
        return None;
    }

    let value = js_ts_pair_function_value(node)?;
    let key = node.child_by_field_name("key")?;
    Some((js_ts_object_key_name(key, source)?, value))
}

fn js_ts_pair_function_value(node: Node) -> Option<Node> {
    if node.kind() != "pair" {
        return None;
    }

    let value = node.child_by_field_name("value")?;
    matches!(
        value.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    )
    .then_some(value)
}

fn js_ts_object_key_name(key: Node, source: &[u8]) -> Option<String> {
    let text = node_text(key, source).trim();
    if text.is_empty() || text.starts_with('[') {
        return None;
    }

    let name = match key.kind() {
        "string" | "template_string" => text
            .trim_matches('"')
            .trim_matches('\'')
            .trim_matches('`')
            .to_string(),
        _ => text.to_string(),
    };

    if name.is_empty() || (key.kind() == "template_string" && name.contains("${")) {
        return None;
    }

    Some(name)
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
    std::str::from_utf8(&source[start..end])
        .ok()
        .map(|s| s.to_string())
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
            "method_declaration" => {
                if let Some(sig) = child.child_by_field_name("signature") {
                    let mut inner = sig.walk();
                    for inner_sig in sig.named_children(&mut inner) {
                        if DART_CONSTRUCTOR_SIG_KINDS.contains(&inner_sig.kind()) {
                            return resolve(inner_sig, source);
                        }
                        if let Some(name_node) = inner_sig.child_by_field_name("name") {
                            return resolve(name_node, source);
                        }
                        if inner_sig.kind() == "operator_signature" {
                            return resolve(inner_sig, source);
                        }
                    }
                }
            }
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

fn collect_value_names(root: Node, source: &[u8], names: &mut Vec<String>) {
    let mut worklist = vec![root];
    while let Some(node) = worklist.pop() {
        if node.kind() == "value_name" {
            names.push(node_text(node, source).to_string());
            continue;
        }
        // Punned record field (`{ x; y }`) — field_pattern with no pattern field.
        // The bound name is the field_name itself.
        if node.kind() == "field_pattern" {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                worklist.push(pattern);
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
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            worklist.push(child);
        }
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
    let bindings: Vec<_> = node
        .named_children(&mut cursor)
        .filter(|c| c.kind() == binding_kind)
        .collect();
    if bindings.is_empty() {
        return false;
    }
    for binding in bindings {
        let mut inner = binding.walk();
        let name = binding
            .named_children(&mut inner)
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
                    && matches!(
                        e.entity_type.as_str(),
                        "type" | "struct" | "class" | "interface"
                    )
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

/// Check if a JS/TS call_expression is a test framework call (describe, test, it, etc.).
/// Returns (name, entity_type, is_container) if matched.
fn extract_js_test_call(node: Node, source: &[u8]) -> Option<(String, &'static str, bool)> {
    let func = node.child_by_field_name("function")?;
    let (callee_name, _modifier) = match func.kind() {
        "identifier" => {
            let name = node_text(func, source);
            (name, None)
        }
        "member_expression" => {
            // describe.skip, test.only, it.each, etc.
            let obj = func.child_by_field_name("object")?;
            let prop = func.child_by_field_name("property")?;
            if obj.kind() != "identifier" {
                return None;
            }
            (node_text(obj, source), Some(node_text(prop, source)))
        }
        _ => return None,
    };

    let (entity_type, is_container) = match callee_name {
        "describe" => ("test_suite", true),
        "test" | "it" => ("test", false),
        "beforeEach" | "afterEach" | "beforeAll" | "afterAll" => ("test_hook", false),
        _ => return None,
    };

    // Extract the test name from the first string argument
    let mut cursor = node.walk();
    let args = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "arguments")?;

    let mut args_cursor = args.walk();
    let first_arg = args.named_children(&mut args_cursor).next()?;

    let test_name = match first_arg.kind() {
        "string" | "template_string" => {
            let text = node_text(first_arg, source);
            text.trim_matches(|c: char| c == '\'' || c == '"' || c == '`')
                .to_string()
        }
        _ => {
            // Hooks like beforeEach don't need a string name
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

    Some((test_name, entity_type, is_container))
}

/// Find the callback function argument in a test call (the second argument).
fn find_test_callback(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    let args = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "arguments")?;
    let mut args_cursor = args.walk();
    let mut children = args.named_children(&mut args_cursor);
    children.next(); // skip the first argument (name string)
    let callback = children.next()?;
    if matches!(
        callback.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    ) {
        Some(callback)
    } else {
        None
    }
}
