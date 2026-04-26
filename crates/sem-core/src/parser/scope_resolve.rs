//! Scope-aware reference resolver using tree-sitter ASTs.
//!
//! Instead of bag-of-words tokenization (current graph.rs Pass 2), this module
//! walks the tree-sitter AST to find actual reference nodes (calls, attribute access)
//! and resolves them using scope chains. This gives compiler-like accuracy for
//! name resolution without needing a full language server.
//!
//! Key improvements over bag-of-words:
//! - Distinguishes definitions from references in the AST
//! - Resolves same-name entities via scope chains (no false collisions)
//! - Tracks variable types through assignments (x = Foo() → x.method → Foo.method)
//! - Uses AST structure, not string matching

use std::collections::HashMap;
use std::path::Path;

use crate::model::entity::SemanticEntity;
use crate::parser::graph::{EntityInfo, RefType};
use crate::parser::plugins::code::languages::{
    get_language_config, AssignmentStrategy, CallNodeStyle, ClassNameField, InitStrategy,
    ParamNameField, ScopeResolveConfig,
};

/// A scope in the scope tree. Scopes are nested: module -> class -> function -> block.
pub struct Scope {
    parent: Option<usize>,
    /// Definitions visible in this scope: name -> entity_id
    defs: HashMap<String, String>,
    /// Variable type bindings: var_name -> class_name (from `x = Foo()`)
    types: HashMap<String, String>,
    /// Unresolved call assignments: var_name -> function_name (from `x = func()`)
    /// These get resolved after return type analysis.
    pending_call_types: HashMap<String, String>,
    /// Which entity owns this scope (if any)
    owner_id: Option<String>,
    /// What kind of scope: "module", "class", "function"
    kind: &'static str,
}

/// Reference found in the AST
struct AstRef {
    /// The entity this reference is inside of
    from_entity_id: String,
    /// Kind of reference
    kind: AstRefKind,
}

enum AstRefKind {
    /// Bare name call: `foo()`
    Call(String),
    /// Attribute call: `x.method()`
    MethodCall { receiver: String, method: String },
    /// Bare name reference: `Foo` (type annotation, class instantiation)
    Name(String),
    /// Attribute access: `x.field`
    Attribute { receiver: String, attr: String },
}

/// Result of scope-aware resolution
pub struct ScopeResult {
    pub edges: Vec<(String, String, RefType)>,
    /// Debug info: which references were resolved and how
    pub resolution_log: Vec<ResolutionEntry>,
}

#[derive(Clone)]
pub struct ResolutionEntry {
    pub from_entity: String,
    pub reference: String,
    pub resolved_to: Option<String>,
    pub method: &'static str, // "scope_chain", "type_tracking", "import", "unresolved"
}

/// Resolve references using tree-sitter scope analysis.
///
/// For each file:
/// 1. Parse with tree-sitter
/// 2. Build a scope tree (module -> class -> function)
/// 3. Walk entity AST subtrees to find reference nodes
/// 4. Resolve each reference via scope chain + type tracking
pub fn resolve_with_scopes(
    root: &Path,
    file_paths: &[String],
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    pre_parsed: Option<Vec<(String, String, tree_sitter::Tree)>>,
) -> ScopeResult {
    let mut all_edges: Vec<(String, String, RefType)> = Vec::new();
    let mut log: Vec<ResolutionEntry> = Vec::new();

    // Build global lookups
    let mut symbol_table: HashMap<String, Vec<String>> = HashMap::new();
    for entity in all_entities {
        symbol_table
            .entry(entity.name.clone())
            .or_default()
            .push(entity.id.clone());
    }

    // class_name -> [(member_name, member_entity_id)]
    let mut class_members: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for entity in all_entities {
        if let Some(ref pid) = entity.parent_id {
            if let Some(parent) = entity_map.get(pid) {
                if matches!(
                    parent.entity_type.as_str(),
                    "class" | "struct" | "interface" | "impl"
                ) {
                    class_members
                        .entry(parent.name.clone())
                        .or_default()
                        .push((entity.name.clone(), entity.id.clone()));
                }
            }
        }
    }

    // Go: methods are declared at file level with receiver syntax, not inside structs.
    // Parse the receiver to populate class_members.
    for entity in all_entities {
        if entity.entity_type == "method" && entity.file_path.ends_with(".go") {
            if let Some(struct_name) = extract_go_receiver_type(&entity.content) {
                class_members
                    .entry(struct_name)
                    .or_default()
                    .push((entity.name.clone(), entity.id.clone()));
            }
        }
    }

    // Entity line ranges for mapping AST nodes back to entities
    let mut entity_ranges: HashMap<String, Vec<(usize, usize, String)>> = HashMap::new();
    for entity in all_entities {
        entity_ranges
            .entry(entity.file_path.clone())
            .or_default()
            .push((entity.start_line, entity.end_line, entity.id.clone()));
    }

    // Build import table from AST (not regex)
    let mut import_table: HashMap<(String, String), String> = HashMap::new();

    // Return type map: function_entity_id -> class_name (if function returns ClassName())
    let mut return_type_map: HashMap<String, String> = HashMap::new();

    // Instance attribute types: (class_name, attr_name) -> class_name_of_attr
    let mut instance_attr_types: HashMap<(String, String), String> = HashMap::new();

    // __init__ param info: class_name -> (ordered_params, attr_to_param mapping)
    // attr_to_param: attr_name -> param_name (for self.attr = param patterns)
    let mut init_params: HashMap<String, Vec<String>> = HashMap::new();
    let mut attr_to_param: HashMap<(String, String), String> = HashMap::new();

    // Merge pre-parsed trees with disk-parsed trees for missing files
    let mut owned_parsed_files: Vec<(String, String, tree_sitter::Tree)> = Vec::new();
    let pre_set: std::collections::HashSet<String> = if let Some(pp) = pre_parsed {
        let set = pp.iter().map(|(fp, _, _)| fp.clone()).collect();
        owned_parsed_files = pp;
        set
    } else {
        std::collections::HashSet::new()
    };
    // Parse any files not already in the pre-parsed set
    for file_path in file_paths {
        if pre_set.contains(file_path) {
            continue;
        }
        let full_path = root.join(file_path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
        let config = match get_language_config(ext) {
            Some(c) => c,
            None => continue,
        };
        let language = match (config.get_language)() {
            Some(l) => l,
            None => continue,
        };
        let mut parser = tree_sitter::Parser::new();
        let _ = parser.set_language(&language);
        if let Some(tree) = parser.parse(content.as_bytes(), None) {
            owned_parsed_files.push((file_path.clone(), content, tree));
        }
    }
    let parsed_files: &[(String, String, tree_sitter::Tree)] = &owned_parsed_files;

    // Pass 1: Scan ALL files for return types and instance attr types first
    // This ensures cross-file return type info is available during resolution
    for (file_path, content, tree) in parsed_files {
        let source = content.as_bytes();
        let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
        let config = match get_language_config(ext).and_then(|c| c.scope_resolve) {
            Some(c) => c,
            None => continue,
        };

        scan_return_types(
            tree.root_node(),
            file_path,
            all_entities,
            source,
            &mut return_type_map,
            config,
        );

        scan_init_self_attrs(
            tree.root_node(),
            file_path,
            all_entities,
            entity_map,
            source,
            &mut instance_attr_types,
            &mut init_params,
            &mut attr_to_param,
            config,
        );
    }

    // Pass 1b: Infer constructor parameter types from call sites
    // For `Transaction(get_connection())`, infer conn param has type Connection.
    // Then resolve self.conn = conn -> (Transaction, conn) -> Connection
    infer_constructor_param_types(
        parsed_files,
        &return_type_map,
        &init_params,
        &attr_to_param,
        &symbol_table,
        entity_map,
        &mut instance_attr_types,
    );

    // Pass 2: Build scopes, imports, and resolve references per file
    for (file_path, content, tree) in parsed_files {
        let source = content.as_bytes();
        let ext = file_path.rfind('.').map(|i| &file_path[i..]).unwrap_or("");
        let config = match get_language_config(ext).and_then(|c| c.scope_resolve) {
            Some(c) => c,
            None => continue,
        };

        let mut scopes: Vec<Scope> = vec![Scope {
            parent: None,
            defs: HashMap::new(),
            types: HashMap::new(),
            pending_call_types: HashMap::new(),
            owner_id: None,
            kind: "module",
        }];

        let mut entity_scope_map: HashMap<String, usize> = HashMap::new();
        let mut entity_inner_scope: HashMap<String, usize> = HashMap::new();

        if let Some(ranges) = entity_ranges.get(file_path.as_str()) {
            for (_start, _end, eid) in ranges {
                if let Some(info) = entity_map.get(eid) {
                    if info.parent_id.is_none() {
                        scopes[0].defs.insert(info.name.clone(), eid.clone());
                        entity_scope_map.insert(eid.clone(), 0);
                    }
                }
            }
        }

        build_scopes_from_ast(
            tree.root_node(),
            0,
            &mut scopes,
            &mut entity_scope_map,
            &mut entity_inner_scope,
            all_entities,
            entity_map,
            file_path,
            source,
            config,
        );

        extract_imports_from_ast(
            tree.root_node(),
            file_path,
            source,
            &symbol_table,
            entity_map,
            &mut import_table,
            &mut scopes,
            config,
        );

        // Resolve pending call types using the complete return type map
        inject_return_type_bindings(
            &entity_inner_scope,
            &mut scopes,
            &return_type_map,
            &import_table,
            file_path,
            entity_map,
        );

        let file_entities: Vec<&SemanticEntity> = all_entities
            .iter()
            .filter(|e| e.file_path == *file_path)
            .collect();

        for entity in &file_entities {
            // Use the entity's inner scope (where local vars live), not the definition scope
            let scope_idx = entity_inner_scope
                .get(&entity.id)
                .or_else(|| entity_scope_map.get(&entity.id))
                .copied()
                .unwrap_or(0);

            let refs = extract_ast_refs(
                tree.root_node(),
                entity,
                source,
                config,
            );

            for ast_ref in refs {
                let resolution = resolve_ref(
                    &ast_ref,
                    scope_idx,
                    &scopes,
                    &symbol_table,
                    &class_members,
                    &import_table,
                    &instance_attr_types,
                    entity_map,
                    file_path,
                    &entity.id,
                );

                if let Some((target_id, ref_type, method)) = resolution {
                    if target_id != entity.id {
                        let is_parent_child = entity
                            .parent_id
                            .as_ref()
                            .map_or(false, |pid| pid == &target_id || entity_map.get(&target_id).map_or(false, |t| t.parent_id.as_ref() == Some(&entity.id)));

                        if !is_parent_child {
                            all_edges.push((
                                entity.id.clone(),
                                target_id.clone(),
                                ref_type,
                            ));
                            log.push(ResolutionEntry {
                                from_entity: entity.id.clone(),
                                reference: ref_description(&ast_ref),
                                resolved_to: Some(target_id),
                                method,
                            });
                        }
                    }
                } else {
                    log.push(ResolutionEntry {
                        from_entity: entity.id.clone(),
                        reference: ref_description(&ast_ref),
                        resolved_to: None,
                        method: "unresolved",
                    });
                }
            }
        }
    }

    // Deduplicate edges
    let mut seen = std::collections::HashSet::new();
    all_edges.retain(|e| seen.insert((e.0.clone(), e.1.clone())));

    ScopeResult {
        edges: all_edges,
        resolution_log: log,
    }
}

fn ref_description(ast_ref: &AstRef) -> String {
    match &ast_ref.kind {
        AstRefKind::Call(name) => format!("{}()", name),
        AstRefKind::MethodCall { receiver, method } => format!("{}.{}()", receiver, method),
        AstRefKind::Name(name) => name.clone(),
        AstRefKind::Attribute { receiver, attr } => format!("{}.{}", receiver, attr),
    }
}

/// Build scope tree by walking the AST.
/// Creates class scopes and maps methods to them.
fn build_scopes_from_ast(
    node: tree_sitter::Node,
    current_scope: usize,
    scopes: &mut Vec<Scope>,
    entity_scope_map: &mut HashMap<String, usize>,
    entity_inner_scope: &mut HashMap<String, usize>,
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    file_path: &str,
    source: &[u8],
    config: &ScopeResolveConfig,
) {
    let kind = node.kind();

    // Class-like scope: config-driven
    let is_class_like = config.class_scope_nodes.contains(&kind);

    // Impl scope: config-driven (Rust impl_item, Swift extension)
    let is_impl = config.impl_scope_nodes.contains(&kind);

    if is_class_like || is_impl {
        let class_name = if is_impl {
            // Impl scope: extract type name from the impl node
            node.child_by_field_name("type")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
        } else {
            match &config.class_name_field {
                ClassNameField::Simple(field) => {
                    node.child_by_field_name(field)
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("")
                }
                ClassNameField::TypeSpec { spec_kind, field } => {
                    let mut name = "";
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        if child.kind() == *spec_kind {
                            name = child
                                .child_by_field_name(field)
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("");
                            break;
                        }
                    }
                    name
                }
                ClassNameField::ImplType(field) => {
                    node.child_by_field_name(field)
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("")
                }
            }
        };

        let class_entity = all_entities.iter().find(|e| {
            e.file_path == file_path
                && e.name == class_name
                && matches!(e.entity_type.as_str(), "class" | "struct" | "interface")
        });

        if let Some(ce) = class_entity {
            // Check if we already have a scope for this class (e.g. struct then impl)
            let existing_scope = entity_inner_scope.get(&ce.id).copied();

            let class_scope_idx = if let Some(idx) = existing_scope {
                idx
            } else {
                let idx = scopes.len();
                scopes.push(Scope {
                    parent: Some(current_scope),
                    defs: HashMap::new(),
                    types: HashMap::new(),
                    pending_call_types: HashMap::new(),
                    owner_id: Some(ce.id.clone()),
                    kind: "class",
                });
                entity_scope_map.insert(ce.id.clone(), current_scope);
                entity_inner_scope.insert(ce.id.clone(), idx);
                idx
            };

            // Register all child entities as defs in the class scope
            for entity in all_entities {
                if entity.parent_id.as_ref() == Some(&ce.id) {
                    scopes[class_scope_idx]
                        .defs
                        .insert(entity.name.clone(), entity.id.clone());
                    entity_scope_map.insert(entity.id.clone(), class_scope_idx);
                }
            }

            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                build_scopes_from_ast(
                    child,
                    class_scope_idx,
                    scopes,
                    entity_scope_map,
                    entity_inner_scope,
                    all_entities,
                    entity_map,
                    file_path,
                    source,
                    config,
                );
            }
            return;
        } else if !is_impl {
            // No matching entity, still recurse with a scope
            let class_scope_idx = scopes.len();
            scopes.push(Scope {
                parent: Some(current_scope),
                defs: HashMap::new(),
                types: HashMap::new(),
                pending_call_types: HashMap::new(),
                owner_id: None,
                kind: "class",
            });
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                build_scopes_from_ast(
                    child,
                    class_scope_idx,
                    scopes,
                    entity_scope_map,
                    entity_inner_scope,
                    all_entities,
                    entity_map,
                    file_path,
                    source,
                    config,
                );
            }
            return;
        }
    }

    // Function-like scope: config-driven
    let is_function_like = config.function_scope_nodes.contains(&kind);

    if is_function_like {
        let func_name = node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("");

        // External method (Go): place inside the receiver's struct scope
        let parent_scope = if config.external_method && kind == "method_declaration" {
            let receiver_type = node.utf8_text(source).ok()
                .and_then(|t| extract_go_receiver_type(t));
            if let Some(ref struct_name) = receiver_type {
                let found = scopes.iter().enumerate().find(|(_, s)| {
                    s.kind == "class" && s.owner_id.as_ref().map_or(false, |oid| {
                        entity_map.get(oid).map_or(false, |e| e.name == *struct_name)
                    })
                });
                found.map(|(idx, _)| idx).unwrap_or(current_scope)
            } else {
                current_scope
            }
        } else {
            current_scope
        };

        let func_scope_idx = scopes.len();
        scopes.push(Scope {
            parent: Some(parent_scope),
            defs: HashMap::new(),
            types: HashMap::new(),
            pending_call_types: HashMap::new(),
            owner_id: None,
            kind: "function",
        });

        let func_entity = all_entities.iter().find(|e| {
            e.file_path == file_path && e.name == func_name && {
                let line = node.start_position().row + 1;
                e.start_line <= line && line <= e.end_line
            }
        });

        if let Some(fe) = func_entity {
            scopes[func_scope_idx].owner_id = Some(fe.id.clone());
            entity_scope_map.entry(fe.id.clone()).or_insert(parent_scope);
            entity_inner_scope.insert(fe.id.clone(), func_scope_idx);
            // For external methods, also register in the struct's class scope defs
            if config.external_method && kind == "method_declaration" && parent_scope != current_scope {
                scopes[parent_scope].defs.insert(fe.name.clone(), fe.id.clone());
            }
        }

        scan_assignments(node, func_scope_idx, scopes, source, config);
        scan_function_params(node, func_scope_idx, scopes, source, config);

        // External method: add receiver parameter type binding
        // func (t *Transaction) Execute(...) -> types["t"] = "Transaction"
        if config.external_method && kind == "method_declaration" {
            if let Some(receiver) = node.child_by_field_name("receiver") {
                let mut rcursor = receiver.walk();
                for param in receiver.named_children(&mut rcursor) {
                    if param.kind() == "parameter_declaration" {
                        let param_name = param
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let param_type = param
                            .child_by_field_name("type")
                            .map(|n| extract_base_type(n, source))
                            .unwrap_or_default();
                        if !param_name.is_empty() && !param_type.is_empty() {
                            scopes[func_scope_idx]
                                .types
                                .insert(param_name.to_string(), param_type);
                        }
                    }
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            build_scopes_from_ast(
                child,
                func_scope_idx,
                scopes,
                entity_scope_map,
                entity_inner_scope,
                all_entities,
                entity_map,
                file_path,
                source,
                config,
            );
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        build_scopes_from_ast(
            child,
            current_scope,
            scopes,
            entity_scope_map,
            entity_inner_scope,
            all_entities,
            entity_map,
            file_path,
            source,
            config,
        );
    }
}

/// Scan for variable assignments and record type bindings.
fn scan_assignments(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
    config: &ScopeResolveConfig,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let ck = child.kind();

        // Check if this node matches an assignment rule
        for rule in config.assignment_rules {
            if ck == rule.node_kind {
                match rule.strategy {
                    AssignmentStrategy::LeftRight => {
                        scan_single_assignment(child, scope_idx, scopes, source);
                    }
                    AssignmentStrategy::Declarators => {
                        scan_ts_var_declaration(child, scope_idx, scopes, source);
                    }
                    AssignmentStrategy::PatternBased => {
                        scan_rust_let_declaration(child, scope_idx, scopes, source);
                    }
                    AssignmentStrategy::ShortVar => {
                        scan_go_short_var(child, scope_idx, scopes, source);
                    }
                    AssignmentStrategy::VarSpec => {
                        scan_go_var_declaration(child, scope_idx, scopes, source);
                    }
                }
            }
        }

        // Recurse into configured container nodes
        if config.assignment_recurse_into.contains(&ck) {
            scan_assignments(child, scope_idx, scopes, source, config);
        }
    }
}

/// Scan function parameter type annotations and add them as type bindings.
/// e.g. `def foo(shelter: Shelter)` -> types["shelter"] = "Shelter"
fn scan_function_params(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
    config: &ScopeResolveConfig,
) {
    let params_node = match node.child_by_field_name("parameters") {
        Some(p) => p,
        None => return,
    };

    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        for rule in config.param_rules {
            if child.kind() != rule.node_kind {
                continue;
            }

            let param_name = match &rule.name_field {
                ParamNameField::Simple(field) => {
                    child.child_by_field_name(field)
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("")
                }
                ParamNameField::WithFallback(field) => {
                    child.child_by_field_name(field)
                        .or_else(|| child.named_child(0).filter(|n| n.kind() == "identifier"))
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("")
                }
                ParamNameField::RustPattern => {
                    child.child_by_field_name("pattern")
                        .and_then(|n| {
                            if n.kind() == "identifier" {
                                n.utf8_text(source).ok()
                            } else if n.kind() == "mut_pattern" {
                                n.named_child(0).and_then(|c| c.utf8_text(source).ok())
                            } else if n.kind() == "reference_pattern" {
                                n.named_child(0).and_then(|c| {
                                    if c.kind() == "identifier" {
                                        c.utf8_text(source).ok()
                                    } else if c.kind() == "mut_pattern" {
                                        c.named_child(0).and_then(|cc| cc.utf8_text(source).ok())
                                    } else {
                                        None
                                    }
                                })
                            } else {
                                None
                            }
                        })
                        .unwrap_or("")
                }
            };

            if param_name.is_empty() || rule.skip_names.contains(&param_name) {
                continue;
            }

            if let Some(type_node) = child.child_by_field_name(rule.type_field) {
                let type_text = extract_base_type(type_node, source);
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    scopes[scope_idx]
                        .types
                        .insert(param_name.to_string(), type_text);
                }
            }
        }
    }
}

/// Python/TS: `x = Foo()` or `x = func()`
fn scan_single_assignment(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let assign = if node.kind() == "assignment" {
        node
    } else {
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        match children.into_iter().find(|c| c.kind() == "assignment" || c.kind() == "assignment_expression") {
            Some(a) => a,
            None => return,
        }
    };

    let left = match assign.child_by_field_name("left") {
        Some(l) => l,
        None => return,
    };
    let right = match assign.child_by_field_name("right") {
        Some(r) => r,
        None => return,
    };

    if left.kind() != "identifier" {
        return;
    }
    let var_name = match left.utf8_text(source) {
        Ok(n) => n.to_string(),
        Err(_) => return,
    };

    record_type_from_rhs(right, &var_name, scope_idx, scopes, source);
}

/// TS: `const x = new Foo()` or `const x: Type = ...` or `const x = func()`
fn scan_ts_var_declaration(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let var_name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .to_string();
            if var_name.is_empty() {
                continue;
            }

            // Check for explicit type annotation: `const x: Foo = ...`
            if let Some(type_ann) = child.child_by_field_name("type") {
                let type_text = extract_base_type(type_ann, source);
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    scopes[scope_idx]
                        .types
                        .insert(var_name.clone(), type_text);
                    continue;
                }
            }

            // Check RHS value
            if let Some(value) = child.child_by_field_name("value") {
                record_type_from_rhs(value, &var_name, scope_idx, scopes, source);
            }
        }
    }
}

/// Rust: `let x: Type = ...` or `let x = Foo::new()`
fn scan_rust_let_declaration(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let var_name = node
        .child_by_field_name("pattern")
        .and_then(|n| {
            // Pattern can be just an identifier or `mut x`
            if n.kind() == "identifier" {
                n.utf8_text(source).ok()
            } else if n.kind() == "mut_pattern" {
                n.named_child(0).and_then(|c| c.utf8_text(source).ok())
            } else {
                None
            }
        })
        .unwrap_or("")
        .to_string();

    if var_name.is_empty() {
        return;
    }

    // Check for explicit type annotation: `let x: Connection = ...`
    if let Some(type_node) = node.child_by_field_name("type") {
        let type_text = extract_base_type(type_node, source);
        if !type_text.is_empty()
            && type_text.chars().next().map_or(false, |c| c.is_uppercase())
        {
            scopes[scope_idx]
                .types
                .insert(var_name, type_text);
            return;
        }
    }

    // Check RHS value
    if let Some(value) = node.child_by_field_name("value") {
        record_type_from_rhs(value, &var_name, scope_idx, scopes, source);
    }
}

/// Go: `x := Foo{}` or `x := NewFoo()`
fn scan_go_short_var(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let left = match node.child_by_field_name("left") {
        Some(l) => l,
        None => return,
    };
    let right = match node.child_by_field_name("right") {
        Some(r) => r,
        None => return,
    };

    // left is expression_list, right is expression_list
    let var_name = if left.kind() == "expression_list" {
        left.named_child(0)
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("")
            .to_string()
    } else {
        left.utf8_text(source).unwrap_or("").to_string()
    };

    if var_name.is_empty() {
        return;
    }

    let rhs = if right.kind() == "expression_list" {
        match right.named_child(0) {
            Some(n) => n,
            None => return,
        }
    } else {
        right
    };

    record_type_from_rhs(rhs, &var_name, scope_idx, scopes, source);
}

/// Go: `var x Type = ...` or `var x = Foo{}`
fn scan_go_var_declaration(
    node: tree_sitter::Node,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "var_spec" {
            let var_name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .to_string();
            if var_name.is_empty() {
                // Try first named child as name
                if let Some(first) = child.named_child(0) {
                    if first.kind() == "identifier" {
                        let name = first.utf8_text(source).unwrap_or("").to_string();
                        if !name.is_empty() {
                            // Check for type child
                            if let Some(type_node) = child.child_by_field_name("type") {
                                let type_text = extract_base_type(type_node, source);
                                if !type_text.is_empty()
                                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                                {
                                    scopes[scope_idx].types.insert(name, type_text);
                                }
                            }
                        }
                    }
                }
                continue;
            }

            // Check for explicit type
            if let Some(type_node) = child.child_by_field_name("type") {
                let type_text = extract_base_type(type_node, source);
                if !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    scopes[scope_idx]
                        .types
                        .insert(var_name, type_text);
                    continue;
                }
            }

            // Check RHS value
            if let Some(value) = child.child_by_field_name("value") {
                let rhs = if value.kind() == "expression_list" {
                    value.named_child(0).unwrap_or(value)
                } else {
                    value
                };
                record_type_from_rhs(rhs, &var_name, scope_idx, scopes, source);
            }
        }
    }
}

/// Record type binding from a RHS expression (works for all languages).
/// Handles: constructor calls, new expressions, struct literals, function calls.
fn record_type_from_rhs(
    rhs: tree_sitter::Node,
    var_name: &str,
    scope_idx: usize,
    scopes: &mut Vec<Scope>,
    source: &[u8],
) {
    match rhs.kind() {
        // Python/Go: Foo() or func()
        "call" | "call_expression" => {
            let func_node = rhs
                .child_by_field_name("function")
                .or_else(|| rhs.named_child(0));
            if let Some(func) = func_node {
                if func.kind() == "identifier" {
                    let name = func.utf8_text(source).unwrap_or("");
                    if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                        scopes[scope_idx]
                            .types
                            .insert(var_name.to_string(), name.to_string());
                    } else {
                        scopes[scope_idx]
                            .pending_call_types
                            .insert(var_name.to_string(), name.to_string());
                    }
                }
                // Rust: Type::new() / Type::from() etc.
                if func.kind() == "scoped_identifier" {
                    let text = func.utf8_text(source).unwrap_or("");
                    let parts: Vec<&str> = text.split("::").collect();
                    if parts.len() >= 2 {
                        let type_name = parts[0];
                        let method_name = parts[parts.len() - 1];
                        if type_name.chars().next().map_or(false, |c| c.is_uppercase()) {
                            scopes[scope_idx]
                                .types
                                .insert(var_name.to_string(), type_name.to_string());
                        } else {
                            scopes[scope_idx]
                                .pending_call_types
                                .insert(var_name.to_string(), method_name.to_string());
                        }
                    }
                }
                // Go: package.NewFoo() or package.GetFoo()
                if func.kind() == "selector_expression" {
                    let field = func
                        .child_by_field_name("field")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    // Go convention: NewFoo() returns *Foo
                    if let Some(type_name) = field.strip_prefix("New") {
                        if !type_name.is_empty()
                            && type_name.chars().next().map_or(false, |c| c.is_uppercase())
                        {
                            scopes[scope_idx]
                                .types
                                .insert(var_name.to_string(), type_name.to_string());
                        }
                    } else if field.starts_with("Get") || field.chars().next().map_or(false, |c| c.is_uppercase()) {
                        // Other Go package functions: record for return type resolution
                        scopes[scope_idx]
                            .pending_call_types
                            .insert(var_name.to_string(), field.to_string());
                    }
                }
            }
        }
        // TS: new Foo()
        "new_expression" => {
            if let Some(constructor) = rhs.child_by_field_name("constructor") {
                let name = constructor.utf8_text(source).unwrap_or("");
                if !name.is_empty() {
                    scopes[scope_idx]
                        .types
                        .insert(var_name.to_string(), name.to_string());
                }
            }
        }
        // Go: Foo{} (composite_literal / struct literal)
        "composite_literal" => {
            if let Some(type_node) = rhs.child_by_field_name("type") {
                let name = type_node.utf8_text(source).unwrap_or("");
                if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                    scopes[scope_idx]
                        .types
                        .insert(var_name.to_string(), name.to_string());
                }
            }
        }
        _ => {}
    }
}

/// Extract the base type name from a type annotation node.
/// Strips pointers, references, generics to get just the type name.
fn extract_base_type(type_node: tree_sitter::Node, source: &[u8]) -> String {
    let text = type_node.utf8_text(source).unwrap_or("").trim().to_string();
    // Strip reference/pointer prefixes and mut keyword
    let text = text.trim_start_matches('&').trim_start_matches('*');
    let text = text.strip_prefix("mut ").unwrap_or(text).trim_start();
    // Strip generic parameters (angle brackets and Python-style square brackets)
    let text = if let Some(i) = text.find('<') {
        &text[..i]
    } else if let Some(i) = text.find('[') {
        &text[..i]
    } else {
        text
    };
    // Strip lifetime annotations for Rust
    let text = text.trim();
    // For type_annotation nodes in TS, strip the leading `: `
    let text = text.trim_start_matches(':').trim();
    text.to_string()
}

/// Parse Go receiver type from method content: `func (r *ReceiverType) Name(...)`
fn extract_go_receiver_type(content: &str) -> Option<String> {
    let after_func = content.strip_prefix("func")?.trim_start();
    let paren_start = after_func.find('(')?;
    let paren_end = after_func.find(')')?;
    let receiver_block = &after_func[paren_start + 1..paren_end];
    // Could be: "r ReceiverType", "r *ReceiverType", "*ReceiverType"
    let parts: Vec<&str> = receiver_block.split_whitespace().collect();
    let type_str = parts.last()?;
    let name = type_str.trim_start_matches('*');
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Scan function bodies/signatures for return types to build a return type map.
fn scan_return_types(
    node: tree_sitter::Node,
    file_path: &str,
    all_entities: &[SemanticEntity],
    source: &[u8],
    return_type_map: &mut HashMap<String, String>,
    config: &ScopeResolveConfig,
) {
    let kind = node.kind();

    let is_func = config.function_scope_nodes.contains(&kind);

    if is_func {
        let func_name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("");

        let func_entity = all_entities.iter().find(|e| {
            e.file_path == file_path && e.name == func_name && {
                let line = node.start_position().row + 1;
                e.start_line <= line && line <= e.end_line
            }
        });

        if let Some(fe) = func_entity {
            // Try explicit return type annotation first
            let ret_type = config.return_type_field.and_then(|field| {
                node.child_by_field_name(field)
                    .map(|n| extract_base_type(n, source))
                    .filter(|t| !t.is_empty() && t.chars().next().map_or(false, |c| c.is_uppercase()))
            });

            if let Some(rt) = ret_type {
                return_type_map.insert(fe.id.clone(), rt);
            } else {
                // Fall back to body heuristic: return ClassName()
                if let Some(ret_type) = find_return_constructor(node, source) {
                    return_type_map.insert(fe.id.clone(), ret_type);
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        scan_return_types(child, file_path, all_entities, source, return_type_map, config);
    }
}

/// Find `return ClassName()` patterns in a function body (heuristic fallback).
fn find_return_constructor(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "return_statement" {
            let mut inner_cursor = child.walk();
            for ret_child in child.named_children(&mut inner_cursor) {
                // Python: call, TS/Go: call_expression
                if ret_child.kind() == "call" || ret_child.kind() == "call_expression" {
                    if let Some(func) = ret_child.child_by_field_name("function") {
                        if func.kind() == "identifier" {
                            let name = func.utf8_text(source).unwrap_or("");
                            if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                                return Some(name.to_string());
                            }
                        }
                    }
                }
                // TS: new ClassName()
                if ret_child.kind() == "new_expression" {
                    if let Some(constructor) = ret_child.child_by_field_name("constructor") {
                        let name = constructor.utf8_text(source).unwrap_or("");
                        if !name.is_empty() {
                            return Some(name.to_string());
                        }
                    }
                }
                // Go: StructName{} (composite_literal)
                if ret_child.kind() == "composite_literal" {
                    if let Some(type_node) = ret_child.child_by_field_name("type") {
                        let name = type_node.utf8_text(source).unwrap_or("");
                        if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
        // Recurse into blocks
        let ck = child.kind();
        if ck == "block" || ck == "statement_block" {
            if let Some(ret_type) = find_return_constructor(child, source) {
                return Some(ret_type);
            }
        }
    }
    None
}

/// Scan for instance attribute types: __init__ self.attr patterns (Python/TS),
/// struct field declarations (Rust/Go).
fn scan_init_self_attrs(
    node: tree_sitter::Node,
    file_path: &str,
    all_entities: &[SemanticEntity],
    entity_map: &HashMap<String, EntityInfo>,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    init_params_map: &mut HashMap<String, Vec<String>>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
    config: &ScopeResolveConfig,
) {
    let kind = node.kind();

    match &config.init_strategy {
        InitStrategy::ConstructorBody { class_nodes, self_keyword, .. } => {
            if class_nodes.contains(&kind) {
                let class_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string();

                if !class_name.is_empty() {
                    // Determine lang for scan_class_for_init (it still needs it for TS field scanning)
                    let lang = if *self_keyword == "this" { "typescript" } else { "python" };
                    scan_class_for_init(node, &class_name, source, instance_attr_types, init_params_map, attr_to_param_map, lang);
                }
            }
        }
        InitStrategy::StructFields { struct_nodes } => {
            if struct_nodes.contains(&kind) {
                // Rust struct: extract field types directly
                if kind == "struct_item" {
                    let struct_name = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("")
                        .to_string();

                    if !struct_name.is_empty() {
                        scan_rust_struct_fields(node, &struct_name, source, instance_attr_types);
                    }
                }
                // Go: extract field types from type declarations
                if kind == "type_declaration" {
                    scan_go_struct_fields(node, source, instance_attr_types);
                }
            }
        }
        InitStrategy::None => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        scan_init_self_attrs(child, file_path, all_entities, entity_map, source, instance_attr_types, init_params_map, attr_to_param_map, config);
    }
}

/// Rust: extract field types from `struct Foo { conn: Connection, ... }`
fn scan_rust_struct_fields(
    node: tree_sitter::Node,
    struct_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "field_declaration_list" {
            let mut inner_cursor = child.walk();
            for field in child.named_children(&mut inner_cursor) {
                if field.kind() == "field_declaration" {
                    let field_name = field
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    let field_type = field
                        .child_by_field_name("type")
                        .map(|n| extract_base_type(n, source))
                        .unwrap_or_default();

                    if !field_name.is_empty()
                        && !field_type.is_empty()
                        && field_type.chars().next().map_or(false, |c| c.is_uppercase())
                    {
                        instance_attr_types.insert(
                            (struct_name.to_string(), field_name.to_string()),
                            field_type,
                        );
                    }
                }
            }
        }
    }
}

/// Go: extract field types from `type Foo struct { conn Connection; ... }`
fn scan_go_struct_fields(
    node: tree_sitter::Node,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_spec" {
            let struct_name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .to_string();

            if struct_name.is_empty() {
                continue;
            }

            // Look for struct_type child
            if let Some(type_node) = child.child_by_field_name("type") {
                if type_node.kind() == "struct_type" {
                    let mut fields_cursor = type_node.walk();
                    for field_list in type_node.named_children(&mut fields_cursor) {
                        if field_list.kind() == "field_declaration_list" {
                            let mut inner = field_list.walk();
                            for field in field_list.named_children(&mut inner) {
                                if field.kind() == "field_declaration" {
                                    // Go field: name type
                                    let field_name = field
                                        .child_by_field_name("name")
                                        .and_then(|n| n.utf8_text(source).ok())
                                        .unwrap_or("");
                                    let field_type = field
                                        .child_by_field_name("type")
                                        .map(|n| extract_base_type(n, source))
                                        .unwrap_or_default();

                                    if !field_name.is_empty()
                                        && !field_type.is_empty()
                                        && field_type.chars().next().map_or(false, |c| c.is_uppercase())
                                    {
                                        instance_attr_types.insert(
                                            (struct_name.clone(), field_name.to_string()),
                                            field_type,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn scan_class_for_init(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    init_params_map: &mut HashMap<String, Vec<String>>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
    lang: &str,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let ck = child.kind();

        // Python __init__
        if ck == "function_definition" {
            let name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if name == "__init__" {
                let params = extract_init_params(child, source);
                let ordered_params = extract_init_param_names_ordered(child, source);
                init_params_map.insert(class_name.to_string(), ordered_params);
                scan_init_body(child, class_name, &params, source, instance_attr_types, attr_to_param_map);
            }
        }

        // TS constructor
        if ck == "method_definition" && lang == "typescript" {
            let name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if name == "constructor" {
                // Scan for this.attr = param patterns
                scan_ts_constructor_body(child, class_name, source, instance_attr_types, init_params_map, attr_to_param_map);
            }
        }

        // TS: typed class field declarations `private conn: Connection`
        if (ck == "public_field_definition" || ck == "property_declaration" || ck == "field_definition") && lang == "typescript" {
            let field_name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if let Some(type_ann) = child.child_by_field_name("type") {
                let type_text = extract_base_type(type_ann, source);
                if !field_name.is_empty()
                    && !type_text.is_empty()
                    && type_text.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    instance_attr_types.insert(
                        (class_name.to_string(), field_name.to_string()),
                        type_text,
                    );
                }
            }
        }

        if ck == "block" || ck == "class_body" || ck == "statement_block" {
            scan_class_for_init(child, class_name, source, instance_attr_types, init_params_map, attr_to_param_map, lang);
        }
    }
}

/// TS: scan constructor body for `this.attr = param` patterns
fn scan_ts_constructor_body(
    node: tree_sitter::Node,
    class_name: &str,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    init_params_map: &mut HashMap<String, Vec<String>>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    // Extract constructor params
    let params = extract_init_params(node, source);
    let ordered_params = extract_init_param_names_ordered(node, source);
    init_params_map.insert(class_name.to_string(), ordered_params);

    // Scan body for this.X = param
    scan_init_body_this(node, class_name, &params, source, instance_attr_types, attr_to_param_map);
}

/// Scan constructor body for `this.attr = param` patterns (TS variant)
fn scan_init_body_this(
    node: tree_sitter::Node,
    class_name: &str,
    params: &HashMap<String, Option<String>>,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let ck = child.kind();
        if ck == "expression_statement" {
            // Look for assignment: this.X = Y
            let mut inner_cursor = child.walk();
            for inner in child.named_children(&mut inner_cursor) {
                if inner.kind() == "assignment_expression" {
                    if let Some(left) = inner.child_by_field_name("left") {
                        if left.kind() == "member_expression" {
                            let obj = left.child_by_field_name("object")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("");
                            let prop = left.child_by_field_name("property")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("");
                            if obj == "this" && !prop.is_empty() {
                                if let Some(right) = inner.child_by_field_name("right") {
                                    if right.kind() == "identifier" {
                                        let rhs_name = right.utf8_text(source).unwrap_or("");
                                        if params.contains_key(rhs_name) {
                                            attr_to_param_map.insert(
                                                (class_name.to_string(), prop.to_string()),
                                                rhs_name.to_string(),
                                            );
                                            if let Some(Some(type_hint)) = params.get(rhs_name) {
                                                instance_attr_types.insert(
                                                    (class_name.to_string(), prop.to_string()),
                                                    type_hint.clone(),
                                                );
                                            }
                                        }
                                    }
                                    if right.kind() == "new_expression" {
                                        if let Some(ctor) = right.child_by_field_name("constructor") {
                                            let name = ctor.utf8_text(source).unwrap_or("");
                                            if !name.is_empty() {
                                                instance_attr_types.insert(
                                                    (class_name.to_string(), prop.to_string()),
                                                    name.to_string(),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if ck == "statement_block" || ck == "block" {
            scan_init_body_this(child, class_name, params, source, instance_attr_types, attr_to_param_map);
        }
    }
}

/// Extract __init__ parameter names in order (excluding self).
fn extract_init_param_names_ordered(func_node: tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(params_node) = func_node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.named_children(&mut cursor) {
            let param_name = if child.kind() == "identifier" {
                child.utf8_text(source).unwrap_or("").to_string()
            } else if child.kind() == "typed_parameter" || child.kind() == "typed_default_parameter" {
                child.child_by_field_name("name")
                    .or_else(|| child.named_child(0))
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string()
            } else {
                continue;
            };
            if param_name != "self" && param_name != "cls" && !param_name.is_empty() {
                names.push(param_name);
            }
        }
    }
    names
}

fn extract_init_params(func_node: tree_sitter::Node, source: &[u8]) -> HashMap<String, Option<String>> {
    let mut params = HashMap::new();
    if let Some(params_node) = func_node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.named_children(&mut cursor) {
            let param_name = if child.kind() == "identifier" {
                child.utf8_text(source).unwrap_or("").to_string()
            } else if child.kind() == "typed_parameter" || child.kind() == "typed_default_parameter" {
                child.child_by_field_name("name")
                    .or_else(|| child.named_child(0))
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string()
            } else {
                continue;
            };
            if param_name != "self" && param_name != "cls" {
                // Check for type annotation
                let type_hint = child.child_by_field_name("type")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(|s| s.to_string());
                params.insert(param_name, type_hint);
            }
        }
    }
    params
}

fn scan_init_body(
    node: tree_sitter::Node,
    class_name: &str,
    params: &HashMap<String, Option<String>>,
    source: &[u8],
    instance_attr_types: &mut HashMap<(String, String), String>,
    attr_to_param_map: &mut HashMap<(String, String), String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "expression_statement" || child.kind() == "assignment" {
            let assign = if child.kind() == "assignment" {
                child
            } else {
                let mut inner_cursor = child.walk();
                let children: Vec<_> = child.named_children(&mut inner_cursor).collect();
                match children.into_iter().find(|c| c.kind() == "assignment") {
                    Some(a) => a,
                    None => continue,
                }
            };

            if let Some(left) = assign.child_by_field_name("left") {
                if left.kind() == "attribute" {
                    let obj = left.child_by_field_name("object")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    let attr = left.child_by_field_name("attribute")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");

                    if obj == "self" && !attr.is_empty() {
                        if let Some(right) = assign.child_by_field_name("right") {
                            if right.kind() == "identifier" {
                                let rhs_name = right.utf8_text(source).unwrap_or("");
                                // Record attr -> param mapping for later inference
                                if params.contains_key(rhs_name) {
                                    attr_to_param_map.insert(
                                        (class_name.to_string(), attr.to_string()),
                                        rhs_name.to_string(),
                                    );
                                }
                                // If param has type hint, directly set the type
                                if let Some(Some(type_hint)) = params.get(rhs_name) {
                                    instance_attr_types.insert(
                                        (class_name.to_string(), attr.to_string()),
                                        type_hint.clone(),
                                    );
                                }
                            }
                            if right.kind() == "call" {
                                if let Some(func) = right.child_by_field_name("function") {
                                    if func.kind() == "identifier" {
                                        let fname = func.utf8_text(source).unwrap_or("");
                                        if fname.chars().next().map_or(false, |c| c.is_uppercase()) {
                                            instance_attr_types.insert(
                                                (class_name.to_string(), attr.to_string()),
                                                fname.to_string(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if child.kind() == "block" {
            scan_init_body(child, class_name, params, source, instance_attr_types, attr_to_param_map);
        }
    }
}

/// Infer constructor parameter types by analyzing call sites across all files.
/// For `Transaction(get_connection())`, we know get_connection() returns Connection,
/// so Transaction.__init__'s conn param has type Connection,
/// and self.conn in Transaction has type Connection.
fn infer_constructor_param_types(
    parsed_files: &[(String, String, tree_sitter::Tree)],
    return_type_map: &HashMap<String, String>,
    init_params: &HashMap<String, Vec<String>>,
    attr_to_param: &HashMap<(String, String), String>,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    // Build func_name -> return_type lookup for quick access
    let mut func_name_returns: HashMap<String, String> = HashMap::new();
    for (eid, ret_type) in return_type_map {
        if let Some(info) = entity_map.get(eid) {
            func_name_returns.insert(info.name.clone(), ret_type.clone());
        }
    }

    // Scan all files for constructor call sites: ClassName(arg1, arg2, ...)
    for (_file_path, content, tree) in parsed_files {
        let source = content.as_bytes();
        scan_constructor_calls(
            tree.root_node(),
            source,
            &func_name_returns,
            init_params,
            attr_to_param,
            instance_attr_types,
        );
    }
}

fn scan_constructor_calls(
    node: tree_sitter::Node,
    source: &[u8],
    func_name_returns: &HashMap<String, String>,
    init_params: &HashMap<String, Vec<String>>,
    attr_to_param: &HashMap<(String, String), String>,
    instance_attr_types: &mut HashMap<(String, String), String>,
) {
    let kind = node.kind();

    if kind == "call" {
        if let Some(func) = node.child_by_field_name("function") {
            if func.kind() == "identifier" {
                let class_name = func.utf8_text(source).unwrap_or("");
                // Only process uppercase names (constructor calls)
                if class_name.chars().next().map_or(false, |c| c.is_uppercase()) {
                    if let Some(param_names) = init_params.get(class_name) {
                        // Extract argument types
                        if let Some(args_node) = node.child_by_field_name("arguments") {
                            let mut arg_idx = 0;
                            let mut args_cursor = args_node.walk();
                            for arg in args_node.named_children(&mut args_cursor) {
                                if arg_idx >= param_names.len() {
                                    break;
                                }
                                let param_name = &param_names[arg_idx];

                                // Try to infer the argument's type
                                let arg_type = infer_expr_type(arg, source, func_name_returns);

                                if let Some(at) = arg_type {
                                    // Check if any self.attr maps to this param
                                    for ((cn, attr), pn) in attr_to_param.iter() {
                                        if cn == class_name && pn == param_name {
                                            instance_attr_types
                                                .entry((cn.clone(), attr.clone()))
                                                .or_insert(at.clone());
                                        }
                                    }
                                }

                                arg_idx += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        scan_constructor_calls(child, source, func_name_returns, init_params, attr_to_param, instance_attr_types);
    }
}

/// Infer the type of an expression node.
fn infer_expr_type(
    node: tree_sitter::Node,
    source: &[u8],
    func_name_returns: &HashMap<String, String>,
) -> Option<String> {
    match node.kind() {
        "call" => {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "identifier" {
                    let name = func.utf8_text(source).unwrap_or("");
                    // Constructor call: Foo() -> type is Foo
                    if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                        return Some(name.to_string());
                    }
                    // Function call with known return type
                    if let Some(ret) = func_name_returns.get(name) {
                        return Some(ret.clone());
                    }
                }
            }
            None
        }
        "identifier" => {
            // Could be a variable, but we don't have scope info here
            None
        }
        _ => None,
    }
}

/// Resolve pending call types using the return type map.
/// For scopes with `x = func()` where func has a known return type, bind x to that type.
fn inject_return_type_bindings(
    _entity_inner_scope: &HashMap<String, usize>,
    scopes: &mut Vec<Scope>,
    return_type_map: &HashMap<String, String>,
    import_table: &HashMap<(String, String), String>,
    file_path: &str,
    entity_map: &HashMap<String, EntityInfo>,
) {
    // Build function name -> return type lookup
    let mut func_name_return_types: HashMap<String, String> = HashMap::new();
    for (eid, ret_type) in return_type_map {
        if let Some(info) = entity_map.get(eid) {
            func_name_return_types.insert(info.name.clone(), ret_type.clone());
        }
    }

    // Also resolve through imports: if `get_connection` is imported and has a known return type
    for ((fp, local_name), target_id) in import_table {
        if fp == file_path {
            if let Some(ret_type) = return_type_map.get(target_id) {
                func_name_return_types.insert(local_name.clone(), ret_type.clone());
            }
        }
    }

    // Resolve pending call types in all scopes
    for scope in scopes.iter_mut() {
        let resolved: Vec<(String, String)> = scope
            .pending_call_types
            .iter()
            .filter_map(|(var_name, func_name)| {
                func_name_return_types
                    .get(func_name)
                    .map(|ret_type| (var_name.clone(), ret_type.clone()))
            })
            .collect();

        for (var_name, ret_type) in resolved {
            scope.types.insert(var_name, ret_type);
        }
    }
}

/// Extract import statements from the AST.
fn extract_imports_from_ast(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
    config: &ScopeResolveConfig,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let ck = child.kind();
        let handled = match ck {
            "import_from_statement" => {
                extract_python_import(child, file_path, source, symbol_table, entity_map, import_table, scopes);
                true
            }
            "import_statement" if !config.self_keywords.contains(&"cls") => {
                // TS import_statement (not Python - Python uses import_from_statement)
                extract_ts_import(child, file_path, source, symbol_table, entity_map, import_table, scopes);
                true
            }
            "use_declaration" => {
                extract_rust_use(child, file_path, source, symbol_table, entity_map, import_table, scopes);
                true
            }
            "import_declaration" => {
                extract_go_import(child, file_path, source, symbol_table, entity_map, import_table, scopes);
                true
            }
            _ => false,
        };
        if !handled {
            extract_imports_from_ast(child, file_path, source, symbol_table, entity_map, import_table, scopes, config);
        }
    }
}

/// TS: `import { Foo, Bar } from './module'` or `import Foo from './module'`
fn extract_ts_import(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    // Extract the source module from the `from '...'` clause
    let source_path = node
        .child_by_field_name("source")
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("")
        .trim_matches(|c: char| c == '\'' || c == '"');

    let source_module = source_path
        .rsplit('/')
        .next()
        .unwrap_or(source_path);
    // Strip extensions
    let source_module = source_module
        .strip_suffix(".ts").or_else(|| source_module.strip_suffix(".js"))
        .or_else(|| source_module.strip_suffix(".tsx")).or_else(|| source_module.strip_suffix(".jsx"))
        .unwrap_or(source_module);

    if source_module.is_empty() {
        return;
    }

    // Walk children to find import clause
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_clause" {
            let mut clause_cursor = child.walk();
            for clause_child in child.named_children(&mut clause_cursor) {
                if clause_child.kind() == "named_imports" {
                    // { Foo, Bar as Baz }
                    let mut imports_cursor = clause_child.walk();
                    for spec in clause_child.named_children(&mut imports_cursor) {
                        if spec.kind() == "import_specifier" {
                            let original = spec
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("");
                            let local = spec
                                .child_by_field_name("alias")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or(original);

                            if !original.is_empty() {
                                resolve_import_name(original, local, source_module, file_path, symbol_table, entity_map, import_table, scopes);
                            }
                        }
                    }
                } else if clause_child.kind() == "identifier" {
                    // Default import: import Foo from './module'
                    let name = clause_child.utf8_text(source).unwrap_or("");
                    if !name.is_empty() {
                        resolve_import_name(name, name, source_module, file_path, symbol_table, entity_map, import_table, scopes);
                    }
                }
            }
        }
    }
}

/// Rust: `use crate::module::Name;` or `use crate::module::{A, B};`
/// Parse from the text of the use_declaration for reliability.
fn extract_rust_use(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    let text = node.utf8_text(source).unwrap_or("").trim().to_string();
    // Strip `use ` prefix and trailing `;`
    let text = text.strip_prefix("use ").unwrap_or(&text);
    let text = text.strip_prefix("pub use ").unwrap_or(text);
    let text = text.trim_end_matches(';').trim();

    // Strip crate/super/self prefix
    let text = text
        .strip_prefix("crate::")
        .or_else(|| text.strip_prefix("super::"))
        .or_else(|| text.strip_prefix("self::"))
        .unwrap_or(text);

    // Check for grouped import: module::{A, B, C}
    if let Some(brace_pos) = text.find("::{") {
        let module_path = &text[..brace_pos];
        let source_module = module_path.rsplit("::").next().unwrap_or(module_path);

        let names_part = &text[brace_pos + 3..];
        let names_part = names_part.trim_end_matches('}');

        for name_part in names_part.split(',') {
            let name_part = name_part.trim();
            if name_part.is_empty() {
                continue;
            }
            let (original, local) = if let Some(pos) = name_part.find(" as ") {
                (name_part[..pos].trim(), name_part[pos + 4..].trim())
            } else {
                (name_part, name_part)
            };
            if !original.is_empty() {
                resolve_import_name(original, local, source_module, file_path, symbol_table, entity_map, import_table, scopes);
            }
        }
    } else {
        // Simple import: module::Name
        let parts: Vec<&str> = text.split("::").collect();
        if parts.is_empty() {
            return;
        }
        let imported_name = parts.last().unwrap().trim();
        let (original, local) = if let Some(pos) = imported_name.find(" as ") {
            (&imported_name[..pos], imported_name[pos + 4..].trim())
        } else {
            (imported_name, imported_name)
        };
        let source_module = if parts.len() >= 2 {
            parts[parts.len() - 2]
        } else {
            parts[0]
        };
        if !original.is_empty() && !source_module.is_empty() {
            resolve_import_name(original, local, source_module, file_path, symbol_table, entity_map, import_table, scopes);
        }
    }
}

/// Go: `import ("module/path")` - maps package names to entities
fn extract_go_import(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_spec" || child.kind() == "import_spec_list" {
            extract_go_import_specs(child, file_path, source, symbol_table, entity_map, import_table, scopes);
        } else if child.kind() == "interpreted_string_literal" || child.kind() == "raw_string_literal" {
            let path = child.utf8_text(source).unwrap_or("")
                .trim_matches('"').trim_matches('`');
            let pkg_name = path.rsplit('/').next().unwrap_or(path);
            register_go_package_imports(pkg_name, file_path, symbol_table, entity_map, import_table, scopes);
        }
    }
}

fn extract_go_import_specs(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_spec" {
            let path_node = child.child_by_field_name("path")
                .or_else(|| child.named_child(0));
            if let Some(pn) = path_node {
                let path = pn.utf8_text(source).unwrap_or("")
                    .trim_matches('"').trim_matches('`');
                let pkg_name = path.rsplit('/').next().unwrap_or(path);
                register_go_package_imports(pkg_name, file_path, symbol_table, entity_map, import_table, scopes);
            }
        } else {
            extract_go_import_specs(child, file_path, source, symbol_table, entity_map, import_table, scopes);
        }
    }
}

fn register_go_package_imports(
    pkg_name: &str,
    file_path: &str,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    for (name, target_ids) in symbol_table {
        for target_id in target_ids {
            if let Some(entity) = entity_map.get(target_id) {
                let stem = entity.file_path.rsplit('/').next().unwrap_or(&entity.file_path);
                let stem = stem.strip_suffix(".go").unwrap_or(stem);
                if stem == pkg_name || entity.file_path.contains(&format!("{}/", pkg_name)) {
                    import_table.insert(
                        (file_path.to_string(), name.clone()),
                        target_id.clone(),
                    );
                    if !scopes.is_empty() {
                        scopes[0].defs.insert(name.clone(), target_id.clone());
                    }
                }
            }
        }
    }
}

/// Shared helper: resolve an imported name against the symbol table
fn resolve_import_name(
    original_name: &str,
    local_name: &str,
    source_module: &str,
    file_path: &str,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    if let Some(target_ids) = symbol_table.get(original_name) {
        let target = target_ids.iter().find(|id| {
            entity_map.get(*id).map_or(false, |e| {
                let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                let stem = stem
                    .strip_suffix(".py")
                    .or_else(|| stem.strip_suffix(".rs"))
                    .or_else(|| stem.strip_suffix(".ts"))
                    .or_else(|| stem.strip_suffix(".tsx"))
                    .or_else(|| stem.strip_suffix(".js"))
                    .or_else(|| stem.strip_suffix(".jsx"))
                    .or_else(|| stem.strip_suffix(".go"))
                    .unwrap_or(stem);
                stem == source_module
            })
        });

        if let Some(target_id) = target {
            import_table.insert(
                (file_path.to_string(), local_name.to_string()),
                target_id.clone(),
            );
            if !scopes.is_empty() {
                scopes[0]
                    .defs
                    .insert(local_name.to_string(), target_id.clone());
            }
        }
    }
}

fn extract_python_import(
    node: tree_sitter::Node,
    file_path: &str,
    source: &[u8],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
    scopes: &mut Vec<Scope>,
) {
    // import_from_statement has:
    //   module_name (dotted_name or relative_import)
    //   name fields (imported names)
    let module_node = node.child_by_field_name("module_name");
    let module_name = module_node
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("");

    let source_module = module_name
        .trim_start_matches('.')
        .rsplit('.')
        .next()
        .unwrap_or(module_name.trim_start_matches('.'));

    // Walk children to find imported names
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "dotted_name" || child.kind() == "aliased_import" {
            let (original, local) = if child.kind() == "aliased_import" {
                let orig = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                let alias = child
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or(orig);
                (orig, alias)
            } else {
                let name = child.utf8_text(source).unwrap_or("");
                (name, name)
            };

            if original.is_empty() {
                continue;
            }

            // Resolve against symbol table, preferring entities from the source module
            if let Some(target_ids) = symbol_table.get(original) {
                let target = target_ids.iter().find(|id| {
                    entity_map.get(*id).map_or(false, |e| {
                        let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                        let stem = stem
                            .strip_suffix(".py")
                            .or_else(|| stem.strip_suffix(".rs"))
                            .or_else(|| stem.strip_suffix(".ts"))
                            .or_else(|| stem.strip_suffix(".js"))
                            .unwrap_or(stem);
                        stem == source_module
                    })
                });

                if let Some(target_id) = target {
                    import_table.insert(
                        (file_path.to_string(), local.to_string()),
                        target_id.clone(),
                    );
                    // Also add to module scope definitions
                    if !scopes.is_empty() {
                        scopes[0]
                            .defs
                            .insert(local.to_string(), target_id.clone());
                    }
                }
            }
        }
    }
}

/// Extract AST references from an entity's line range.
fn extract_ast_refs(
    root: tree_sitter::Node,
    entity: &SemanticEntity,
    source: &[u8],
    config: &ScopeResolveConfig,
) -> Vec<AstRef> {
    let mut refs = Vec::new();
    let start_row = entity.start_line.saturating_sub(1); // 1-indexed to 0-indexed
    let end_row = entity.end_line; // exclusive

    collect_refs_in_range(root, start_row, end_row, &entity.id, &entity.name, source, &mut refs, config);
    refs
}

fn collect_refs_in_range(
    node: tree_sitter::Node,
    start_row: usize,
    end_row: usize,
    entity_id: &str,
    entity_name: &str,
    source: &[u8],
    refs: &mut Vec<AstRef>,
    config: &ScopeResolveConfig,
) {
    let node_start = node.start_position().row;
    let node_end = node.end_position().row;

    if node_end < start_row || node_start >= end_row {
        return;
    }

    let kind = node.kind();

    // Call nodes (e.g. "call", "call_expression", "method_invocation")
    if config.call_nodes.contains(&kind) {
        match &config.call_style {
            CallNodeStyle::FunctionField(field) => {
                if let Some(func) = node.child_by_field_name(field) {
                    extract_call_ref(func, entity_id, entity_name, source, refs, config);
                }
            }
            CallNodeStyle::DirectMethod { object_field, method_field } => {
                let method_name = node.child_by_field_name(method_field)
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                if !method_name.is_empty() && method_name != entity_name && !is_builtin(method_name, config) {
                    if let Some(obj_node) = node.child_by_field_name(object_field) {
                        let receiver = obj_node.utf8_text(source).unwrap_or("").to_string();
                        // Strip trailing dots/operators
                        let receiver = receiver.trim_end_matches('.').to_string();
                        refs.push(AstRef {
                            from_entity_id: entity_id.to_string(),
                            kind: AstRefKind::MethodCall { receiver, method: method_name.to_string() },
                        });
                    } else {
                        // Bare call (no object)
                        refs.push(AstRef {
                            from_entity_id: entity_id.to_string(),
                            kind: AstRefKind::Call(method_name.to_string()),
                        });
                    }
                }
            }
        }
        if let Some(args) = node.child_by_field_name("arguments") {
            let mut cursor = args.walk();
            for child in args.named_children(&mut cursor) {
                collect_refs_in_range(child, start_row, end_row, entity_id, entity_name, source, refs, config);
            }
        }
        return;
    }

    // New expression nodes (e.g. "new_expression", "object_creation_expression")
    if config.new_expr_nodes.contains(&kind) {
        if let Some(type_node) = node.child_by_field_name(config.new_expr_type_field) {
            let name = type_node.utf8_text(source).unwrap_or("");
            // For Java/C#, the type field might contain a full qualified name; take the last part
            let name = name.rsplit('.').next().unwrap_or(name);
            if !name.is_empty() && name != entity_name && !is_builtin(name, config) {
                refs.push(AstRef {
                    from_entity_id: entity_id.to_string(),
                    kind: AstRefKind::Call(name.to_string()),
                });
            }
        }
        if let Some(args) = node.child_by_field_name("arguments") {
            let mut cursor = args.walk();
            for child in args.named_children(&mut cursor) {
                collect_refs_in_range(child, start_row, end_row, entity_id, entity_name, source, refs, config);
            }
        }
        return;
    }

    // Composite literal nodes (e.g. Go "composite_literal")
    if config.composite_literal_nodes.contains(&kind) {
        if let Some(type_node) = node.child_by_field_name("type") {
            let name = type_node.utf8_text(source).unwrap_or("");
            if name.chars().next().map_or(false, |c| c.is_uppercase())
                && name != entity_name
                && !is_builtin(name, config)
            {
                refs.push(AstRef {
                    from_entity_id: entity_id.to_string(),
                    kind: AstRefKind::Call(name.to_string()),
                });
            }
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_refs_in_range(child, start_row, end_row, entity_id, entity_name, source, refs, config);
    }
}

/// Extract a call reference from a function/callee node (shared across languages)
fn extract_call_ref(
    func: tree_sitter::Node,
    entity_id: &str,
    entity_name: &str,
    source: &[u8],
    refs: &mut Vec<AstRef>,
    config: &ScopeResolveConfig,
) {
    let func_kind = func.kind();

    if func_kind == "identifier" {
        let name = func.utf8_text(source).unwrap_or("");
        if !name.is_empty() && name != entity_name && !is_builtin(name, config) {
            refs.push(AstRef {
                from_entity_id: entity_id.to_string(),
                kind: AstRefKind::Call(name.to_string()),
            });
        }
        return;
    }

    // Check config member_access patterns
    for ma in config.member_access {
        if func_kind == ma.node_kind {
            extract_member_call_ref(func, ma.object_field, ma.property_field, entity_id, source, refs);
            return;
        }
    }

    // Scoped call nodes (e.g. Rust "scoped_identifier" for Type::method)
    if config.scoped_call_nodes.contains(&func_kind) {
        let text = func.utf8_text(source).unwrap_or("");
        let parts: Vec<&str> = text.split("::").collect();
        if parts.len() >= 2 {
            let type_name = parts[parts.len() - 2];
            let method_name = parts[parts.len() - 1];
            if !type_name.is_empty() && !method_name.is_empty() {
                refs.push(AstRef {
                    from_entity_id: entity_id.to_string(),
                    kind: AstRefKind::Call(method_name.to_string()),
                });
                if type_name.chars().next().map_or(false, |c| c.is_uppercase())
                    && !is_builtin(type_name, config)
                {
                    refs.push(AstRef {
                        from_entity_id: entity_id.to_string(),
                        kind: AstRefKind::Call(type_name.to_string()),
                    });
                }
            }
        }
    }
}

/// Extract a member/method call from a node with object+property fields
fn extract_member_call_ref(
    node: tree_sitter::Node,
    object_field: &str,
    attr_field: &str,
    entity_id: &str,
    source: &[u8],
    refs: &mut Vec<AstRef>,
) {
    let obj = node
        .child_by_field_name(object_field)
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("");
    let attr = node
        .child_by_field_name(attr_field)
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("");
    if !obj.is_empty() && !attr.is_empty() {
        push_method_call_ref(obj, attr, entity_id, refs);
    }
}

fn push_method_call_ref(obj: &str, method: &str, entity_id: &str, refs: &mut Vec<AstRef>) {
    refs.push(AstRef {
        from_entity_id: entity_id.to_string(),
        kind: AstRefKind::MethodCall {
            receiver: obj.to_string(),
            method: method.to_string(),
        },
    });
}

/// Resolve a single reference against scopes and symbol tables.
fn resolve_ref(
    ast_ref: &AstRef,
    scope_idx: usize,
    scopes: &[Scope],
    symbol_table: &HashMap<String, Vec<String>>,
    class_members: &HashMap<String, Vec<(String, String)>>,
    import_table: &HashMap<(String, String), String>,
    instance_attr_types: &HashMap<(String, String), String>,
    entity_map: &HashMap<String, EntityInfo>,
    file_path: &str,
    from_entity_id: &str,
) -> Option<(String, RefType, &'static str)> {
    match &ast_ref.kind {
        AstRefKind::Call(name) => {
            // 1. Walk scope chain for the name
            if let Some(eid) = lookup_scope_chain(scope_idx, scopes, name) {
                if eid != from_entity_id {
                    return Some((eid, RefType::Calls, "scope_chain"));
                }
            }

            // 2. Check import table
            let key = (file_path.to_string(), name.clone());
            if let Some(target_id) = import_table.get(&key) {
                return Some((target_id.clone(), RefType::Calls, "import"));
            }

            // 3. Global symbol table fallback (constructor calls or cross-file functions)
            if let Some(target_ids) = symbol_table.get(name.as_str()) {
                let is_constructor = name.chars().next().map_or(false, |c| c.is_uppercase());
                let ref_type = if is_constructor { RefType::TypeRef } else { RefType::Calls };
                // Prefer same-file, then any
                let target = target_ids
                    .iter()
                    .find(|id| {
                        entity_map
                            .get(*id)
                            .map_or(false, |e| e.file_path == file_path)
                    })
                    .or_else(|| {
                        // For lowercase calls, only fall back to global if there's exactly one match
                        // (avoid ambiguous resolution that bag-of-words creates)
                        if is_constructor || target_ids.len() == 1 {
                            target_ids.first()
                        } else {
                            None
                        }
                    });
                if let Some(tid) = target {
                    return Some((tid.clone(), ref_type, "scope_chain"));
                }
            }

            None
        }

        AstRefKind::MethodCall { receiver, method } => {
            if receiver == "self" || receiver == "this" {
                // self.method() -> find in enclosing class
                let mut idx = scope_idx;
                loop {
                    if scopes[idx].kind == "class" {
                        if let Some(eid) = scopes[idx].defs.get(method.as_str()) {
                            return Some((eid.clone(), RefType::Calls, "scope_chain"));
                        }
                        break;
                    }
                    match scopes[idx].parent {
                        Some(p) => idx = p,
                        None => break,
                    }
                }
                return None;
            }

            // Handle chained self.attr.method() pattern
            // receiver is "self.X" where X is an instance attribute
            if receiver.starts_with("self.") || receiver.starts_with("this.") {
                let attr_name = &receiver[5..]; // strip "self." or "this."
                // Find the enclosing class name
                let class_name = find_enclosing_class(scope_idx, scopes, entity_map);
                if let Some(cn) = class_name {
                    // Look up instance attribute type
                    if let Some(attr_type) = instance_attr_types.get(&(cn, attr_name.to_string())) {
                        if let Some(members) = class_members.get(attr_type.as_str()) {
                            if let Some((_, mid)) = members.iter().find(|(n, _)| n == method) {
                                return Some((mid.clone(), RefType::Calls, "type_tracking"));
                            }
                        }
                    }
                }
            }

            // Handle chained var.field.method() pattern (e.g. Go receiver: t.Conn.Execute())
            if receiver.contains('.') && !receiver.starts_with("self.") && !receiver.starts_with("this.") {
                if let Some(dot_pos) = receiver.find('.') {
                    let var_part = &receiver[..dot_pos];
                    let field_part = &receiver[dot_pos + 1..];
                    if let Some(var_type) = lookup_type_in_scopes(scope_idx, scopes, var_part) {
                        if let Some(attr_type) = instance_attr_types.get(&(var_type, field_part.to_string())) {
                            if let Some(members) = class_members.get(attr_type.as_str()) {
                                if let Some((_, mid)) = members.iter().find(|(n, _)| n == method) {
                                    return Some((mid.clone(), RefType::Calls, "type_tracking"));
                                }
                            }
                        }
                    }
                }
            }

            // receiver.method() -> look up receiver type, then resolve method
            let receiver_type = lookup_type_in_scopes(scope_idx, scopes, receiver);

            if let Some(class_name) = receiver_type {
                if let Some(members) = class_members.get(class_name.as_str()) {
                    if let Some((_, mid)) = members.iter().find(|(n, _)| n == method) {
                        return Some((mid.clone(), RefType::Calls, "type_tracking"));
                    }
                }
            }

            // Fallback: check import table for the receiver
            let key = (file_path.to_string(), receiver.clone());
            if let Some(target_id) = import_table.get(&key) {
                if let Some(info) = entity_map.get(target_id) {
                    if matches!(info.entity_type.as_str(), "class" | "struct") {
                        if let Some(members) = class_members.get(&info.name) {
                            if let Some((_, mid)) =
                                members.iter().find(|(n, _)| n == method)
                            {
                                return Some((
                                    mid.clone(),
                                    RefType::Calls,
                                    "type_tracking",
                                ));
                            }
                        }
                    }
                }
            }

            // Go package-qualified call: package.Function()
            // Try the method name directly in the import table
            let key = (file_path.to_string(), method.clone());
            if let Some(target_id) = import_table.get(&key) {
                return Some((target_id.clone(), RefType::Calls, "import"));
            }

            None
        }

        AstRefKind::Name(name) => {
            if let Some(eid) = lookup_scope_chain(scope_idx, scopes, name) {
                return Some((eid, RefType::TypeRef, "scope_chain"));
            }
            let key = (file_path.to_string(), name.clone());
            if let Some(target_id) = import_table.get(&key) {
                return Some((target_id.clone(), RefType::Imports, "import"));
            }
            None
        }

        AstRefKind::Attribute { receiver, attr } => {
            let receiver_type = lookup_type_in_scopes(scope_idx, scopes, receiver);
            if let Some(class_name) = receiver_type {
                if let Some(members) = class_members.get(class_name.as_str()) {
                    if let Some((_, mid)) = members.iter().find(|(n, _)| n == attr) {
                        return Some((mid.clone(), RefType::Calls, "type_tracking"));
                    }
                }
            }
            None
        }
    }
}

/// Find the class name for the enclosing class scope.
fn find_enclosing_class(
    start_scope: usize,
    scopes: &[Scope],
    entity_map: &HashMap<String, EntityInfo>,
) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if scopes[idx].kind == "class" {
            if let Some(ref oid) = scopes[idx].owner_id {
                return entity_map.get(oid).map(|e| e.name.clone());
            }
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

/// Walk up the scope chain looking for a definition.
fn lookup_scope_chain(
    start_scope: usize,
    scopes: &[Scope],
    name: &str,
) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if let Some(eid) = scopes[idx].defs.get(name) {
            return Some(eid.clone());
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

/// Walk up the scope chain looking for a type binding.
fn lookup_type_in_scopes(
    start_scope: usize,
    scopes: &[Scope],
    var_name: &str,
) -> Option<String> {
    let mut idx = start_scope;
    loop {
        if let Some(type_name) = scopes[idx].types.get(var_name) {
            return Some(type_name.clone());
        }
        match scopes[idx].parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

fn is_builtin(name: &str, config: &ScopeResolveConfig) -> bool {
    // Common builtins across languages
    if matches!(name, "None" | "True" | "False" | "null" | "undefined" | "nil") {
        return true;
    }
    config.builtins.contains(&name)
}
