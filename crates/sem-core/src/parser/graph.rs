//! Entity dependency graph — cross-file reference extraction.
//!
//! Implements a two-pass approach inspired by arXiv:2601.08773 (Reliable Graph-RAG):
//! Pass 1: Extract all entities, build a symbol table (name → entity ID).
//! Pass 2: For each entity, extract identifier references from its AST subtree,
//!         resolve them against the symbol table to create edges.
//!
//! This enables impact analysis: "if I change entity X, what else is affected?"

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::git::types::{FileChange, FileStatus};
use crate::model::entity::SemanticEntity;
use crate::parser::registry::ParserRegistry;
use crate::parser::scope_resolve;

/// A reference from one entity to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityRef {
    pub from_entity: String,
    pub to_entity: String,
    pub ref_type: RefType,
}

/// Type of reference between entities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefType {
    /// Function/method call
    Calls,
    /// Type reference (extends, implements, field type)
    TypeRef,
    /// Import/use statement reference
    Imports,
}

/// A complete entity dependency graph for a set of files.
#[derive(Debug)]
pub struct EntityGraph {
    /// All entities indexed by ID
    pub entities: HashMap<String, EntityInfo>,
    /// Edges: from_entity → [(to_entity, ref_type)]
    pub edges: Vec<EntityRef>,
    /// Reverse index: entity_id → entities that reference it
    pub dependents: HashMap<String, Vec<String>>,
    /// Forward index: entity_id → entities it references
    pub dependencies: HashMap<String, Vec<String>>,
}

/// Minimal entity info stored in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityInfo {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}

impl EntityGraph {
    /// Reconstruct an EntityGraph from pre-loaded parts (e.g. from a cache).
    pub fn from_parts(entities: HashMap<String, EntityInfo>, edges: Vec<EntityRef>) -> Self {
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::new();
        for edge in &edges {
            dependents
                .entry(edge.to_entity.clone())
                .or_default()
                .push(edge.from_entity.clone());
            dependencies
                .entry(edge.from_entity.clone())
                .or_default()
                .push(edge.to_entity.clone());
        }
        EntityGraph {
            entities,
            edges,
            dependents,
            dependencies,
        }
    }

    /// Build an entity graph from a set of files.
    ///
    /// Pass 1: Extract all entities from all files using the parser registry.
    /// Pass 2: For each entity, find identifier tokens and resolve them against
    ///         the symbol table to create reference edges.
    pub fn build(
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
    ) -> (Self, Vec<SemanticEntity>) {
        // Pass 1: Extract all entities in parallel (file I/O + tree-sitter parsing)
        // Also collect (file_path, content, tree) for scope_resolve reuse
        let per_file: Vec<(Vec<SemanticEntity>, Option<(String, String, tree_sitter::Tree)>)> = file_paths
            .par_iter()
            .filter_map(|file_path| {
                let full_path = root.join(file_path);
                let content = std::fs::read_to_string(&full_path).ok()?;
                let plugin = registry.get_plugin_with_content(file_path, &content)?;
                let (entities, tree) = plugin.extract_entities_with_tree(&content, file_path);
                let parsed = tree.map(|t| (file_path.clone(), content, t));
                Some((entities, parsed))
            })
            .collect();

        let mut all_entities: Vec<SemanticEntity> = Vec::new();
        let mut parsed_files: Vec<(String, String, tree_sitter::Tree)> = Vec::new();
        for (entities, parsed) in per_file {
            all_entities.extend(entities);
            if let Some(p) = parsed {
                parsed_files.push(p);
            }
        }

        // Build symbol table: name → entity IDs (can be multiple with same name)
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::with_capacity(all_entities.len());
        let mut entity_map: HashMap<String, EntityInfo> = HashMap::with_capacity(all_entities.len());

        for entity in &all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());

            entity_map.insert(
                entity.id.clone(),
                EntityInfo {
                    id: entity.id.clone(),
                    name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    parent_id: entity.parent_id.clone(),
                    start_line: entity.start_line,
                    end_line: entity.end_line,
                },
            );
        }

        // Build parent-child set for skipping class→method self-edges
        let parent_child_pairs: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id.as_ref().map(|pid| (pid.as_str(), e.id.as_str()))
            })
            .collect();

        // Build set of (class_id, child_method_name) so classes skip refs to their own methods
        let class_child_names: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id.as_ref().map(|pid| (pid.as_str(), e.name.as_str()))
            })
            .collect();

        // Build class-related maps for dot-chain resolution
        // class_entity_names: all class/struct/interface entity names
        let class_entity_names: HashSet<&str> = all_entities
            .iter()
            .filter(|e| matches!(e.entity_type.as_str(), "class" | "struct" | "interface" | "class_type"))
            .map(|e| e.name.as_str())
            .collect();

        // id_to_name: quick lookup for parent name resolution
        let id_to_name: HashMap<&str, &str> = all_entities
            .iter()
            .map(|e| (e.id.as_str(), e.name.as_str()))
            .collect();

        // enclosing_class: entity_id → class_name (for self/this resolution)
        // class_members: class_name → [(member_name, member_entity_id)]
        let mut enclosing_class: HashMap<&str, &str> = HashMap::new();
        let mut class_members: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();

        for entity in &all_entities {
            if let Some(ref pid) = entity.parent_id {
                if let Some(&parent_name) = id_to_name.get(pid.as_str()) {
                    if class_entity_names.contains(parent_name) {
                        enclosing_class.insert(entity.id.as_str(), parent_name);
                        class_members
                            .entry(parent_name)
                            .or_default()
                            .push((entity.name.as_str(), entity.id.as_str()));
                    }
                }
            }
        }

        // Build import table: (file_path, imported_name) → target entity ID
        // e.g. ("io_handler.py", "validate") → "core.py::function::validate"
        let import_table = build_import_table(root, file_paths, &symbol_table, &entity_map);

        // Run scope-aware resolver for supported languages (reuse pre-parsed trees)
        let has_scope_lang = file_paths.iter().any(|f| {
            let ext = f.rfind('.').map(|i| &f[i..]).unwrap_or("");
            crate::parser::plugins::code::languages::get_language_config(ext)
                .and_then(|c| c.scope_resolve)
                .is_some()
        });
        let (scope_edges, scope_resolved_entities) = if has_scope_lang {
            let result = scope_resolve::resolve_with_scopes(root, file_paths, &all_entities, &entity_map, Some(parsed_files));
            let resolved_entity_ids: HashSet<String> = result.edges.iter()
                .map(|(from, _, _)| from.clone())
                .collect();
            (result.edges, resolved_entity_ids)
        } else {
            (vec![], HashSet::new())
        };

        // Pass 2: Extract references in parallel, then resolve against symbol table
        // Phase 1: Dot-chain resolution (precise self.X, this.X, ClassName.X)
        // Phase 2: Bag-of-words resolution (existing logic, skipping consumed words)
        // Skip entities already resolved by scope resolver (Python files)
        let resolved_refs: Vec<(String, String, RefType)> = all_entities
            .par_iter()
            .flat_map(|entity| {
                // Skip entities already resolved by scope resolver
                if scope_resolved_entities.contains(&entity.id) {
                    return vec![];
                }

                let mut entity_edges = Vec::new();
                let mut consumed_words: HashSet<String> = HashSet::new();

                // Phase 1: Dot-chain resolution
                let stripped = strip_comments_and_strings(&entity.content);
                let dot_chains = extract_dot_chains(&stripped);

                for (receiver, member) in &dot_chains {
                    if *receiver == "self" || *receiver == "this" {
                        // self.B / this.B: resolve to sibling method in enclosing class
                        if let Some(class_name) = enclosing_class.get(entity.id.as_str()) {
                            if let Some(members) = class_members.get(class_name) {
                                for (n, tid) in members {
                                    if *n == *member && *tid != entity.id.as_str() {
                                        entity_edges.push((
                                            entity.id.clone(),
                                            tid.to_string(),
                                            RefType::Calls,
                                        ));
                                        consumed_words.insert(member.to_string());
                                        break;
                                    }
                                }
                            }
                        }
                    } else if class_entity_names.contains(*receiver) {
                        // ClassName.B: resolve to class member
                        if let Some(members) = class_members.get(*receiver) {
                            for (n, tid) in members {
                                if *n == *member {
                                    entity_edges.push((
                                        entity.id.clone(),
                                        tid.to_string(),
                                        RefType::Calls,
                                    ));
                                    consumed_words.insert(member.to_string());
                                    consumed_words.insert(receiver.to_string());
                                    break;
                                }
                            }
                        }
                    }
                    // Unresolved chains fall through to bag-of-words below
                }

                // Phase 2: Bag-of-words resolution (skip words consumed by dot-chains)
                let refs = extract_references_from_content(&entity.content, &entity.name);
                for ref_name in refs {
                    if consumed_words.contains(ref_name) {
                        continue;
                    }

                    // Skip references to names that are this class's own methods
                    if class_child_names.contains(&(entity.id.as_str(), ref_name)) {
                        continue;
                    }

                    // Check import table first: if this file imports this name,
                    // resolve to the import target instead of global symbol table
                    let import_key = (entity.file_path.clone(), ref_name.to_string());
                    if let Some(import_target_id) = import_table.get(&import_key) {
                        if import_target_id != &entity.id
                            && !parent_child_pairs.contains(&(entity.id.as_str(), import_target_id.as_str()))
                            && !parent_child_pairs.contains(&(import_target_id.as_str(), entity.id.as_str()))
                        {
                            let ref_type = infer_ref_type(&entity.content, &ref_name);
                            entity_edges.push((
                                entity.id.clone(),
                                import_target_id.clone(),
                                ref_type,
                            ));
                        }
                        continue;
                    }

                    if let Some(target_ids) = symbol_table.get(ref_name) {
                        // Without an import, only resolve to entities in the same file.
                        // Cross-file resolution is handled by the import table above.
                        let target = target_ids
                            .iter()
                            .find(|id| {
                                *id != &entity.id
                                    && entity_map
                                        .get(*id)
                                        .map_or(false, |e| e.file_path == entity.file_path)
                            });

                        if let Some(target_id) = target {
                            // Skip parent-child edges (class -> own method)
                            if parent_child_pairs.contains(&(entity.id.as_str(), target_id.as_str()))
                                || parent_child_pairs.contains(&(target_id.as_str(), entity.id.as_str()))
                            {
                                continue;
                            }
                            let ref_type = infer_ref_type(&entity.content, &ref_name);
                            entity_edges.push((
                                entity.id.clone(),
                                target_id.clone(),
                                ref_type,
                            ));
                        }
                    }
                }
                entity_edges
            })
            .collect();

        // Merge scope edges with bag-of-words edges, deduplicating
        let mut all_resolved: Vec<(String, String, RefType)> = scope_edges;
        all_resolved.extend(resolved_refs);
        let mut seen_edges: HashSet<(String, String)> = HashSet::new();
        all_resolved.retain(|e| seen_edges.insert((e.0.clone(), e.1.clone())));

        // Build edge indexes from resolved references
        let mut edges: Vec<EntityRef> = Vec::with_capacity(all_resolved.len());
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::new();

        for (from_entity, to_entity, ref_type) in all_resolved {
            dependents
                .entry(to_entity.clone())
                .or_default()
                .push(from_entity.clone());
            dependencies
                .entry(from_entity.clone())
                .or_default()
                .push(to_entity.clone());
            edges.push(EntityRef {
                from_entity,
                to_entity,
                ref_type,
            });
        }

        let graph = EntityGraph {
            entities: entity_map,
            edges,
            dependents,
            dependencies,
        };

        (graph, all_entities)
    }

    /// Incrementally build an entity graph: reparse only stale files, reuse cached data for clean files.
    ///
    /// Uses the same full 3-phase resolution (scope + dot-chain + bag-of-words) as `build()`,
    /// but only runs it for entities in stale files + clean entities whose cached edges
    /// pointed into stale files (they need re-resolution since their targets may have changed).
    pub fn build_incremental(
        root: &Path,
        stale_files: &[String],
        all_file_paths: &[String],
        cached_entities: Vec<SemanticEntity>,
        cached_edges: Vec<EntityRef>,
        stale_file_cached_entities: Vec<SemanticEntity>,
        registry: &ParserRegistry,
    ) -> (Self, Vec<SemanticEntity>) {
        // Build set of stale file paths for quick lookup
        let stale_set: HashSet<&str> = stale_files.iter().map(|s| s.as_str()).collect();

        // Parse stale files in parallel to get new entities + trees
        let per_file: Vec<(Vec<SemanticEntity>, Option<(String, String, tree_sitter::Tree)>)> = stale_files
            .par_iter()
            .filter_map(|file_path| {
                let full_path = root.join(file_path);
                let content = std::fs::read_to_string(&full_path).ok()?;
                let plugin = registry.get_plugin_with_content(file_path, &content)?;
                let (entities, tree) = plugin.extract_entities_with_tree(&content, file_path);
                let parsed = tree.map(|t| (file_path.clone(), content, t));
                Some((entities, parsed))
            })
            .collect();

        let mut new_entities: Vec<SemanticEntity> = Vec::new();
        let mut parsed_files: Vec<(String, String, tree_sitter::Tree)> = Vec::new();
        for (entities, parsed) in per_file {
            new_entities.extend(entities);
            if let Some(p) = parsed {
                parsed_files.push(p);
            }
        }

        // Entity-level diffing: compare new stale-file entities against cached versions
        // Build content_hash lookup from cached stale-file entities
        let cached_hashes: HashMap<&str, &str> = stale_file_cached_entities
            .iter()
            .map(|e| (e.id.as_str(), e.content_hash.as_str()))
            .collect();

        // Classify new stale-file entities
        let mut truly_changed_ids: HashSet<String> = HashSet::new();
        let mut content_clean_ids: HashSet<String> = HashSet::new();
        for entity in &new_entities {
            match cached_hashes.get(entity.id.as_str()) {
                Some(old_hash) if *old_hash == entity.content_hash.as_str() => {
                    content_clean_ids.insert(entity.id.clone());
                }
                _ => {
                    // Hash differs or entity is new
                    truly_changed_ids.insert(entity.id.clone());
                }
            }
        }

        // Detect deleted entities: in cached stale but not in new
        let new_entity_ids: HashSet<&str> = new_entities.iter().map(|e| e.id.as_str()).collect();
        let deleted_ids: HashSet<&str> = stale_file_cached_entities
            .iter()
            .filter(|e| !new_entity_ids.contains(e.id.as_str()))
            .map(|e| e.id.as_str())
            .collect();

        // Merge: cached (clean) entities + new (stale) entities
        let all_entities: Vec<SemanticEntity> = cached_entities
            .into_iter()
            .chain(new_entities.into_iter())
            .collect();

        // Find affected clean entities: only care about edges pointing to truly_changed/deleted
        let mut affected_clean_ids: HashSet<String> = HashSet::new();
        for edge in &cached_edges {
            let to_truly_changed = truly_changed_ids.contains(&edge.to_entity)
                || deleted_ids.contains(edge.to_entity.as_str());
            if to_truly_changed && !stale_set.contains(
                all_entities.iter()
                    .find(|e| e.id == edge.from_entity)
                    .map(|e| e.file_path.as_str())
                    .unwrap_or("")
            ) {
                affected_clean_ids.insert(edge.from_entity.clone());
            }
        }

        // Collect all stale entity IDs (for edge filtering)
        let stale_entity_ids: HashSet<&str> = all_entities
            .iter()
            .filter(|e| stale_set.contains(e.file_path.as_str()))
            .map(|e| e.id.as_str())
            .collect();

        // Keep edges where:
        // - Both endpoints are clean files AND from_entity is not affected, OR
        // - From a content_clean stale entity whose targets are also clean/content_clean
        let kept_edges: Vec<EntityRef> = cached_edges
            .into_iter()
            .filter(|e| {
                let from_stale = stale_entity_ids.contains(e.from_entity.as_str());
                let to_stale = stale_entity_ids.contains(e.to_entity.as_str());

                if !from_stale && !to_stale && !affected_clean_ids.contains(&e.from_entity) {
                    // Both clean, from not affected
                    return true;
                }
                if content_clean_ids.contains(&e.from_entity)
                    && !truly_changed_ids.contains(&e.to_entity)
                    && !deleted_ids.contains(e.to_entity.as_str())
                    && !affected_clean_ids.contains(&e.from_entity)
                {
                    // From content_clean stale entity, target not truly changed
                    return true;
                }
                false
            })
            .collect();

        // Set of entity IDs that need resolution: truly_changed + affected clean
        // (content_clean stale entities keep their cached edges)
        let needs_resolution: HashSet<&str> = all_entities
            .iter()
            .filter(|e| {
                truly_changed_ids.contains(&e.id)
                    || affected_clean_ids.contains(&e.id)
            })
            .map(|e| e.id.as_str())
            .collect();

        // Now run the same resolution logic as build() but only for entities in needs_resolution.
        // We still need the full context (symbol table, import table, etc.) from ALL entities.

        // Build symbol table from all entities
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::with_capacity(all_entities.len());
        let mut entity_map: HashMap<String, EntityInfo> = HashMap::with_capacity(all_entities.len());

        for entity in &all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());
            entity_map.insert(
                entity.id.clone(),
                EntityInfo {
                    id: entity.id.clone(),
                    name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    parent_id: entity.parent_id.clone(),
                    start_line: entity.start_line,
                    end_line: entity.end_line,
                },
            );
        }

        // Build parent-child set
        let parent_child_pairs: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id.as_ref().map(|pid| (pid.as_str(), e.id.as_str()))
            })
            .collect();

        let class_child_names: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id.as_ref().map(|pid| (pid.as_str(), e.name.as_str()))
            })
            .collect();

        let class_entity_names: HashSet<&str> = all_entities
            .iter()
            .filter(|e| matches!(e.entity_type.as_str(), "class" | "struct" | "interface" | "class_type"))
            .map(|e| e.name.as_str())
            .collect();

        let id_to_name: HashMap<&str, &str> = all_entities
            .iter()
            .map(|e| (e.id.as_str(), e.name.as_str()))
            .collect();

        let mut enclosing_class: HashMap<&str, &str> = HashMap::new();
        let mut class_members: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();

        for entity in &all_entities {
            if let Some(ref pid) = entity.parent_id {
                if let Some(&parent_name) = id_to_name.get(pid.as_str()) {
                    if class_entity_names.contains(parent_name) {
                        enclosing_class.insert(entity.id.as_str(), parent_name);
                        class_members
                            .entry(parent_name)
                            .or_default()
                            .push((entity.name.as_str(), entity.id.as_str()));
                    }
                }
            }
        }

        // Build import table from ALL files (imports may reference stale entities)
        let import_table = build_import_table(root, all_file_paths, &symbol_table, &entity_map);

        // Run scope-aware resolver only on files that need resolution
        let resolve_file_paths: Vec<String> = all_file_paths
            .iter()
            .filter(|f| {
                // Include file if any entity in needs_resolution belongs to it
                stale_set.contains(f.as_str()) || all_entities.iter().any(|e| {
                    e.file_path == **f && affected_clean_ids.contains(&e.id)
                })
            })
            .cloned()
            .collect();

        let has_scope_lang = resolve_file_paths.iter().any(|f| {
            let ext = f.rfind('.').map(|i| &f[i..]).unwrap_or("");
            crate::parser::plugins::code::languages::get_language_config(ext)
                .and_then(|c| c.scope_resolve)
                .is_some()
        });
        let (scope_edges, scope_resolved_entities) = if has_scope_lang {
            // Pass pre-parsed stale-file trees; scope_resolve reads affected clean files from disk
            let resolve_set: HashSet<&str> = resolve_file_paths.iter().map(|s| s.as_str()).collect();
            let relevant_parsed: Vec<(String, String, tree_sitter::Tree)> = parsed_files
                .into_iter()
                .filter(|(fp, _, _)| resolve_set.contains(fp.as_str()))
                .collect();
            let pre = if relevant_parsed.is_empty() { None } else { Some(relevant_parsed) };
            let result = scope_resolve::resolve_with_scopes(root, &resolve_file_paths, &all_entities, &entity_map, pre);
            let resolved_entity_ids: HashSet<String> = result.edges.iter()
                .map(|(from, _, _)| from.clone())
                .collect();
            (result.edges, resolved_entity_ids)
        } else {
            (vec![], HashSet::new())
        };

        // Resolve references only for entities in needs_resolution
        let resolved_refs: Vec<(String, String, RefType)> = all_entities
            .par_iter()
            .filter(|e| needs_resolution.contains(e.id.as_str()))
            .flat_map(|entity| {
                if scope_resolved_entities.contains(&entity.id) {
                    return vec![];
                }

                let mut entity_edges = Vec::new();
                let mut consumed_words: HashSet<String> = HashSet::new();

                // Phase 1: Dot-chain resolution
                let stripped = strip_comments_and_strings(&entity.content);
                let dot_chains = extract_dot_chains(&stripped);

                for (receiver, member) in &dot_chains {
                    if *receiver == "self" || *receiver == "this" {
                        if let Some(class_name) = enclosing_class.get(entity.id.as_str()) {
                            if let Some(members) = class_members.get(class_name) {
                                for (n, tid) in members {
                                    if *n == *member && *tid != entity.id.as_str() {
                                        entity_edges.push((
                                            entity.id.clone(),
                                            tid.to_string(),
                                            RefType::Calls,
                                        ));
                                        consumed_words.insert(member.to_string());
                                        break;
                                    }
                                }
                            }
                        }
                    } else if class_entity_names.contains(*receiver) {
                        if let Some(members) = class_members.get(*receiver) {
                            for (n, tid) in members {
                                if *n == *member {
                                    entity_edges.push((
                                        entity.id.clone(),
                                        tid.to_string(),
                                        RefType::Calls,
                                    ));
                                    consumed_words.insert(member.to_string());
                                    consumed_words.insert(receiver.to_string());
                                    break;
                                }
                            }
                        }
                    }
                }

                // Phase 2: Bag-of-words resolution
                let refs = extract_references_from_content(&entity.content, &entity.name);
                for ref_name in refs {
                    if consumed_words.contains(ref_name) {
                        continue;
                    }
                    if class_child_names.contains(&(entity.id.as_str(), ref_name)) {
                        continue;
                    }

                    let import_key = (entity.file_path.clone(), ref_name.to_string());
                    if let Some(import_target_id) = import_table.get(&import_key) {
                        if import_target_id != &entity.id
                            && !parent_child_pairs.contains(&(entity.id.as_str(), import_target_id.as_str()))
                            && !parent_child_pairs.contains(&(import_target_id.as_str(), entity.id.as_str()))
                        {
                            let ref_type = infer_ref_type(&entity.content, &ref_name);
                            entity_edges.push((
                                entity.id.clone(),
                                import_target_id.clone(),
                                ref_type,
                            ));
                        }
                        continue;
                    }

                    if let Some(target_ids) = symbol_table.get(ref_name) {
                        let target = target_ids
                            .iter()
                            .find(|id| {
                                *id != &entity.id
                                    && entity_map
                                        .get(*id)
                                        .map_or(false, |e| e.file_path == entity.file_path)
                            });

                        if let Some(target_id) = target {
                            if parent_child_pairs.contains(&(entity.id.as_str(), target_id.as_str()))
                                || parent_child_pairs.contains(&(target_id.as_str(), entity.id.as_str()))
                            {
                                continue;
                            }
                            let ref_type = infer_ref_type(&entity.content, &ref_name);
                            entity_edges.push((
                                entity.id.clone(),
                                target_id.clone(),
                                ref_type,
                            ));
                        }
                    }
                }
                entity_edges
            })
            .collect();

        // Merge scope edges + bag-of-words edges + kept cached edges
        let mut all_resolved: Vec<(String, String, RefType)> = scope_edges;
        all_resolved.extend(resolved_refs);
        let mut seen_edges: HashSet<(String, String)> = HashSet::new();
        all_resolved.retain(|e| seen_edges.insert((e.0.clone(), e.1.clone())));

        // Build final edge list: kept edges + newly resolved edges
        let mut edges: Vec<EntityRef> = Vec::with_capacity(kept_edges.len() + all_resolved.len());
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::new();

        // Track all edge pairs for dedup
        let mut all_edge_pairs: HashSet<(String, String)> = HashSet::new();

        // Add kept cached edges
        for edge in kept_edges {
            all_edge_pairs.insert((edge.from_entity.clone(), edge.to_entity.clone()));
            dependents
                .entry(edge.to_entity.clone())
                .or_default()
                .push(edge.from_entity.clone());
            dependencies
                .entry(edge.from_entity.clone())
                .or_default()
                .push(edge.to_entity.clone());
            edges.push(edge);
        }

        // Add newly resolved edges, dedup against kept edges
        for (from_entity, to_entity, ref_type) in all_resolved {
            if !all_edge_pairs.insert((from_entity.clone(), to_entity.clone())) {
                continue;
            }
            dependents
                .entry(to_entity.clone())
                .or_default()
                .push(from_entity.clone());
            dependencies
                .entry(from_entity.clone())
                .or_default()
                .push(to_entity.clone());
            edges.push(EntityRef {
                from_entity,
                to_entity,
                ref_type,
            });
        }

        let graph = EntityGraph {
            entities: entity_map,
            edges,
            dependents,
            dependencies,
        };

        (graph, all_entities)
    }

    /// Get entities that depend on the given entity (reverse deps).
    pub fn get_dependents(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependents
            .get(entity_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get entities that the given entity depends on (forward deps).
    pub fn get_dependencies(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependencies
            .get(entity_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Impact analysis: if the given entity changes, what else might be affected?
    /// Returns all transitive dependents (breadth-first), capped at 10k.
    pub fn impact_analysis(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.impact_analysis_capped(entity_id, 10_000)
    }

    /// Impact analysis with a cap on maximum nodes visited.
    /// Returns transitive dependents up to the cap. Uses borrowed strings.
    pub fn impact_analysis_capped(&self, entity_id: &str, max_visited: usize) -> Vec<&EntityInfo> {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        let mut result = Vec::new();

        let start_key = match self.entities.get_key_value(entity_id) {
            Some((k, _)) => k.as_str(),
            None => return result,
        };

        queue.push_back(start_key);
        visited.insert(start_key);

        while let Some(current) = queue.pop_front() {
            if result.len() >= max_visited {
                break;
            }
            if let Some(deps) = self.dependents.get(current) {
                for dep in deps {
                    if visited.insert(dep.as_str()) {
                        if let Some(info) = self.entities.get(dep.as_str()) {
                            result.push(info);
                        }
                        queue.push_back(dep.as_str());
                        if result.len() >= max_visited {
                            break;
                        }
                    }
                }
            }
        }

        result
    }

    /// Count transitive dependents without collecting them (faster for large graphs).
    /// Uses borrowed strings to avoid allocation overhead.
    pub fn impact_count(&self, entity_id: &str, max_count: usize) -> usize {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        let mut count = 0;

        // We need entity_id to live long enough; look it up in our entities map
        let start_key = match self.entities.get_key_value(entity_id) {
            Some((k, _)) => k.as_str(),
            None => return 0,
        };

        queue.push_back(start_key);
        visited.insert(start_key);

        while let Some(current) = queue.pop_front() {
            if count >= max_count {
                break;
            }
            if let Some(deps) = self.dependents.get(current) {
                for dep in deps {
                    if visited.insert(dep.as_str()) {
                        count += 1;
                        queue.push_back(dep.as_str());
                        if count >= max_count {
                            break;
                        }
                    }
                }
            }
        }

        count
    }

    /// Filter entities to those that look like tests.
    /// Uses name heuristics, file path patterns, and content patterns.
    pub fn filter_test_entities(&self, entities: &[crate::model::entity::SemanticEntity]) -> HashSet<String> {
        let mut test_ids = HashSet::new();
        for entity in entities {
            if is_test_entity(entity) {
                test_ids.insert(entity.id.clone());
            }
        }
        test_ids
    }

    /// Impact analysis filtered to test entities only.
    /// Returns transitive dependents that are test functions/methods.
    pub fn test_impact(
        &self,
        entity_id: &str,
        all_entities: &[crate::model::entity::SemanticEntity],
    ) -> Vec<&EntityInfo> {
        let test_ids = self.filter_test_entities(all_entities);
        let impact = self.impact_analysis(entity_id);
        impact
            .into_iter()
            .filter(|info| test_ids.contains(&info.id))
            .collect()
    }

    /// Incrementally update the graph from a set of changed files.
    ///
    /// Instead of rebuilding the entire graph, this only re-extracts entities
    /// from changed files and re-resolves their references. This is faster
    /// than a full rebuild when only a few files changed.
    ///
    /// For each changed file:
    /// - Deleted: remove all entities from that file, prune edges
    /// - Added/Modified: remove old entities, extract new ones, rebuild references
    /// - Renamed: update file paths in entity info
    pub fn update_from_changes(
        &mut self,
        changed_files: &[FileChange],
        root: &Path,
        registry: &ParserRegistry,
    ) {
        let mut affected_files: HashSet<String> = HashSet::new();
        let mut new_entities: Vec<SemanticEntity> = Vec::new();

        for change in changed_files {
            affected_files.insert(change.file_path.clone());
            if let Some(ref old_path) = change.old_file_path {
                affected_files.insert(old_path.clone());
            }

            match change.status {
                FileStatus::Deleted => {
                    self.remove_entities_for_file(&change.file_path);
                }
                FileStatus::Renamed => {
                    // Update file paths for renamed files
                    if let Some(ref old_path) = change.old_file_path {
                        self.remove_entities_for_file(old_path);
                    }
                    // Extract entities from the new file
                    if let Some(entities) = self.extract_file_entities(
                        &change.file_path,
                        change.after_content.as_deref(),
                        root,
                        registry,
                    ) {
                        new_entities.extend(entities);
                    }
                }
                FileStatus::Added | FileStatus::Modified => {
                    // Remove old entities for this file
                    self.remove_entities_for_file(&change.file_path);
                    // Extract new entities
                    if let Some(entities) = self.extract_file_entities(
                        &change.file_path,
                        change.after_content.as_deref(),
                        root,
                        registry,
                    ) {
                        new_entities.extend(entities);
                    }
                }
            }
        }

        // Add new entities to the entity map
        for entity in &new_entities {
            self.entities.insert(
                entity.id.clone(),
                EntityInfo {
                    id: entity.id.clone(),
                    name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    parent_id: entity.parent_id.clone(),
                    start_line: entity.start_line,
                    end_line: entity.end_line,
                },
            );
        }

        // Rebuild the global symbol table from all current entities
        let symbol_table = self.build_symbol_table();

        // Re-resolve references for new entities
        for entity in &new_entities {
            self.resolve_entity_references(entity, &symbol_table);
        }

        // Also re-resolve references for entities in OTHER files that might
        // reference entities in changed files (their targets may have changed)
        let changed_entity_names: HashSet<String> = new_entities
            .iter()
            .map(|e| e.name.clone())
            .collect();

        // Find entities in unchanged files that reference any changed entity name
        let entities_to_recheck: Vec<String> = self
            .entities
            .values()
            .filter(|e| !affected_files.contains(&e.file_path))
            .filter(|e| {
                self.dependencies
                    .get(&e.id)
                    .map_or(false, |deps| {
                        deps.iter().any(|dep_id| {
                            self.entities
                                .get(dep_id)
                                .map_or(false, |dep| changed_entity_names.contains(&dep.name))
                        })
                    })
            })
            .map(|e| e.id.clone())
            .collect();

        // We don't have the full SemanticEntity for unchanged files, so we skip
        // deep re-resolution here. The forward/reverse indexes are already updated
        // by remove_entities_for_file and resolve_entity_references.
        // For entities that had dangling references (their target was deleted),
        // the edges were already pruned.
        let _ = entities_to_recheck; // acknowledge but don't act on for now
    }

    /// Extract entities from a file, using provided content or reading from disk.
    fn extract_file_entities(
        &self,
        file_path: &str,
        content: Option<&str>,
        root: &Path,
        registry: &ParserRegistry,
    ) -> Option<Vec<SemanticEntity>> {
        let content = if let Some(c) = content {
            c.to_string()
        } else {
            let full_path = root.join(file_path);
            std::fs::read_to_string(&full_path).ok()?
        };

        let plugin = registry.get_plugin_with_content(file_path, &content)?;

        Some(plugin.extract_entities(&content, file_path))
    }

    /// Remove all entities belonging to a specific file and prune their edges.
    fn remove_entities_for_file(&mut self, file_path: &str) {
        // Collect entity IDs to remove
        let ids_to_remove: Vec<String> = self
            .entities
            .values()
            .filter(|e| e.file_path == file_path)
            .map(|e| e.id.clone())
            .collect();

        let id_set: HashSet<&str> = ids_to_remove.iter().map(|s| s.as_str()).collect();

        // Remove from entity map
        for id in &ids_to_remove {
            self.entities.remove(id);
        }

        // Remove edges involving these entities
        self.edges
            .retain(|e| !id_set.contains(e.from_entity.as_str()) && !id_set.contains(e.to_entity.as_str()));

        // Clean up dependency/dependent indexes
        for id in &ids_to_remove {
            // Remove forward deps
            if let Some(deps) = self.dependencies.remove(id) {
                // Also remove from reverse index
                for dep in &deps {
                    if let Some(dependents) = self.dependents.get_mut(dep) {
                        dependents.retain(|d| d != id);
                    }
                }
            }
            // Remove reverse deps
            if let Some(deps) = self.dependents.remove(id) {
                // Also remove from forward index
                for dep in &deps {
                    if let Some(dependencies) = self.dependencies.get_mut(dep) {
                        dependencies.retain(|d| d != id);
                    }
                }
            }
        }
    }

    /// Build a symbol table from all current entities.
    fn build_symbol_table(&self) -> HashMap<String, Vec<String>> {
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::new();
        for entity in self.entities.values() {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());
        }
        symbol_table
    }

    /// Resolve references for a single entity against the symbol table.
    fn resolve_entity_references(
        &mut self,
        entity: &SemanticEntity,
        symbol_table: &HashMap<String, Vec<String>>,
    ) {
        let refs = extract_references_from_content(&entity.content, &entity.name);

        for ref_name in refs {
            if let Some(target_ids) = symbol_table.get(ref_name) {
                let target = target_ids
                    .iter()
                    .find(|id| {
                        *id != &entity.id
                            && self
                                .entities
                                .get(*id)
                                .map_or(false, |e| e.file_path == entity.file_path)
                    })
                    .or_else(|| target_ids.iter().find(|id| *id != &entity.id));

                if let Some(target_id) = target {
                    let ref_type = infer_ref_type(&entity.content, &ref_name);
                    self.edges.push(EntityRef {
                        from_entity: entity.id.clone(),
                        to_entity: target_id.clone(),
                        ref_type,
                    });
                    self.dependents
                        .entry(target_id.clone())
                        .or_default()
                        .push(entity.id.clone());
                    self.dependencies
                        .entry(entity.id.clone())
                        .or_default()
                        .push(target_id.clone());
                }
            }
        }
    }
}

/// Check if an entity looks like a test based on name, file path, and content patterns.
fn is_test_entity(entity: &crate::model::entity::SemanticEntity) -> bool {
    let name = &entity.name;
    let path = &entity.file_path;
    let content = &entity.content;

    // Name patterns
    if name.starts_with("test_") || name.starts_with("Test") || name.ends_with("_test") || name.ends_with("Test") {
        return true;
    }
    if name.starts_with("it_") || name.starts_with("describe_") || name.starts_with("spec_") {
        return true;
    }

    // File path patterns
    let path_lower = path.to_lowercase();
    let in_test_file = path_lower.contains("/test/")
        || path_lower.contains("/tests/")
        || path_lower.contains("/spec/")
        || path_lower.contains("_test.")
        || path_lower.contains(".test.")
        || path_lower.contains("_spec.")
        || path_lower.contains(".spec.");

    // Content patterns (test annotations/decorators)
    let has_test_marker = content.contains("#[test]")
        || content.contains("#[cfg(test)]")
        || content.contains("@Test")
        || content.contains("@pytest")
        || content.contains("@test")
        || content.contains("describe(")
        || content.contains("it(")
        || content.contains("test(");

    in_test_file && has_test_marker
}

/// Build import table: maps (file_path, imported_name) → target entity ID.
///
/// Parses `from X import Y` / `import X` / `use X` style statements from entity content
/// and resolves Y to the entity it refers to in the symbol table.
fn build_import_table(
    root: &Path,
    file_paths: &[String],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> HashMap<(String, String), String> {
    let mut import_table: HashMap<(String, String), String> = HashMap::new();

    for file_path in file_paths {
        let full_path = root.join(file_path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Join multi-line imports into single logical lines
        // e.g. "from .cookies import (\n    foo,\n    bar,\n)" -> "from .cookies import foo, bar"
        let mut logical_lines: Vec<String> = Vec::new();
        let mut current_line = String::new();
        let mut in_parens = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if in_parens {
                // Strip parentheses and comments
                let clean = trimmed.trim_end_matches(|c: char| c == ')' || c == ',');
                let clean = clean.split('#').next().unwrap_or(clean).trim();
                if !clean.is_empty() && clean != "(" {
                    current_line.push_str(", ");
                    current_line.push_str(clean);
                }
                if trimmed.contains(')') {
                    in_parens = false;
                    logical_lines.push(std::mem::take(&mut current_line));
                }
            } else if trimmed.starts_with("from ") && trimmed.contains(" import ") {
                if trimmed.contains('(') && !trimmed.contains(')') {
                    // Multi-line import starts
                    in_parens = true;
                    // Take everything before the paren
                    let before_paren = trimmed.split('(').next().unwrap_or(trimmed);
                    current_line = before_paren.trim().to_string();
                    // Also grab anything after the paren on this line
                    if let Some(after) = trimmed.split('(').nth(1) {
                        let after = after.trim().trim_end_matches(')').trim();
                        if !after.is_empty() {
                            current_line.push(' ');
                            current_line.push_str(after);
                        }
                    }
                } else {
                    logical_lines.push(trimmed.to_string());
                }
            }
        }

        for logical_line in &logical_lines {
            if let Some(rest) = logical_line.strip_prefix("from ") {
                // Find " import " or " import," (multi-line imports join with comma)
                let import_match = rest.find(" import ")
                    .map(|pos| (pos, 8))
                    .or_else(|| rest.find(" import,").map(|pos| (pos, 8)));
                if let Some((import_pos, skip)) = import_match {
                    let module_path = &rest[..import_pos];
                    let names_str = &rest[import_pos + skip..];

                    let source_module = module_path
                        .trim_start_matches('.')
                        .rsplit('.')
                        .next()
                        .unwrap_or(module_path.trim_start_matches('.'));

                    for name_part in names_str.split(',') {
                        let name_part = name_part.trim();
                        let imported_name = name_part.split_whitespace().next().unwrap_or(name_part);
                        // Strip trailing parens/punctuation
                        let imported_name = imported_name.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
                        if imported_name.is_empty() {
                            continue;
                        }

                        if let Some(target_ids) = symbol_table.get(imported_name) {
                            let target = target_ids.iter().find(|id| {
                                entity_map.get(*id).map_or(false, |e| {
                                    let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                                    let stem = stem.strip_suffix(".py")
                                        .or_else(|| stem.strip_suffix(".ts"))
                                        .or_else(|| stem.strip_suffix(".js"))
                                        .or_else(|| stem.strip_suffix(".rs"))
                                        .unwrap_or(stem);
                                    stem == source_module
                                })
                            });
                            if let Some(target_id) = target {
                                import_table.insert(
                                    (file_path.clone(), imported_name.to_string()),
                                    target_id.clone(),
                                );
                            }
                        }
                    }
                }
            }
        }

        // JS/TS imports: import { foo, bar as baz } from './module'
        //                import Foo from './module'
        let is_js_ts = file_path.ends_with(".js") || file_path.ends_with(".ts")
            || file_path.ends_with(".jsx") || file_path.ends_with(".tsx");

        if is_js_ts {
            static JS_NAMED_RE: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r#"import\s*\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#).unwrap()
            });
            static JS_DEFAULT_RE: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r#"import\s+(?:type\s+)?([A-Za-z_]\w*)\s+from\s*['"]([^'"]+)['"]"#).unwrap()
            });

            for cap in JS_NAMED_RE.captures_iter(&content) {
                let names_str = cap.get(1).unwrap().as_str();
                let module_path = cap.get(2).unwrap().as_str();
                let source_module = module_path.rsplit('/').next().unwrap_or(module_path);
                let source_module = strip_js_ext(source_module);

                for name_part in names_str.split(',') {
                    let name_part = name_part.trim();
                    if name_part.is_empty() { continue; }

                    // Handle "foo as bar" aliases and "type foo" prefixes
                    let (original_name, local_name) = if let Some(pos) = name_part.find(" as ") {
                        let orig = name_part[..pos].trim();
                        let local = name_part[pos + 4..].trim();
                        let orig = orig.strip_prefix("type ").unwrap_or(orig);
                        (orig, local)
                    } else {
                        let name = name_part.strip_prefix("type ").unwrap_or(name_part);
                        (name, name)
                    };

                    if original_name.is_empty() || local_name.is_empty() { continue; }

                    if let Some(target_ids) = symbol_table.get(original_name) {
                        let target = target_ids.iter().find(|id| {
                            entity_map.get(*id).map_or(false, |e| {
                                let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                                let stem = strip_file_ext(stem);
                                stem == source_module
                            })
                        });
                        if let Some(target_id) = target {
                            import_table.insert(
                                (file_path.clone(), local_name.to_string()),
                                target_id.clone(),
                            );
                        }
                    }
                }
            }

            for cap in JS_DEFAULT_RE.captures_iter(&content) {
                let local_name = cap.get(1).unwrap().as_str();
                let module_path = cap.get(2).unwrap().as_str();
                let source_module = module_path.rsplit('/').next().unwrap_or(module_path);
                let source_module = strip_js_ext(source_module);

                if let Some(target_ids) = symbol_table.get(local_name) {
                    let target = target_ids.iter().find(|id| {
                        entity_map.get(*id).map_or(false, |e| {
                            let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                            let stem = strip_file_ext(stem);
                            stem == source_module
                        })
                    });
                    if let Some(target_id) = target {
                        import_table.insert(
                            (file_path.clone(), local_name.to_string()),
                            target_id.clone(),
                        );
                    }
                }
            }
        }

        // Rust imports: use crate::module::Name; / use crate::module::{A, B};
        // Also: use super::module::Name; / use self::module::Name;
        let is_rust = file_path.ends_with(".rs");
        if is_rust {
            static RUST_USE_SIMPLE_RE: LazyLock<Regex> = LazyLock::new(|| {
                // use crate::config::Config;
                // use super::types::Entity;
                // use config::Config;  (bare module path in binary crates)
                Regex::new(r"(?m)^\s*use\s+(?:(?:crate|super|self)::)?([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)\s*;").unwrap()
            });
            static RUST_USE_GROUP_RE: LazyLock<Regex> = LazyLock::new(|| {
                // use crate::types::{Entity, ParseError};
                // use types::{Entity, ParseError};  (bare module path)
                Regex::new(r"(?m)^\s*use\s+(?:(?:crate|super|self)::)?([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)::\{([^}]+)\}\s*;").unwrap()
            });

            // Build a map: module_name -> list of file paths whose stem matches
            // For "use crate::config::Config", module is "config", name is "Config"
            for cap in RUST_USE_SIMPLE_RE.captures_iter(&content) {
                let full_path_str = cap.get(1).unwrap().as_str();
                let parts: Vec<&str> = full_path_str.split("::").collect();
                if parts.is_empty() { continue; }

                // Last part is the imported name, everything before is the module path
                let imported_name = parts[parts.len() - 1];
                // The module is the second-to-last part, or the first if only one part
                let source_module = if parts.len() >= 2 {
                    parts[parts.len() - 2]
                } else {
                    parts[0]
                };

                resolve_rust_import(
                    file_path, imported_name, source_module,
                    symbol_table, entity_map, &mut import_table,
                );
            }

            for cap in RUST_USE_GROUP_RE.captures_iter(&content) {
                let module_path = cap.get(1).unwrap().as_str();
                let names_str = cap.get(2).unwrap().as_str();

                // source_module is the last segment of the module path
                let source_module = module_path.rsplit("::").next().unwrap_or(module_path);

                for name_part in names_str.split(',') {
                    let name_part = name_part.trim();
                    // Handle "Name as Alias"
                    let (original, local) = if let Some(pos) = name_part.find(" as ") {
                        (&name_part[..pos], name_part[pos + 4..].trim())
                    } else {
                        (name_part, name_part)
                    };
                    let original = original.trim();
                    let local = local.trim();
                    if original.is_empty() || local.is_empty() { continue; }

                    resolve_rust_import(
                        file_path, original, source_module,
                        symbol_table, entity_map, &mut import_table,
                    );
                    // If aliased, also map the local name
                    if local != original {
                        if let Some(target) = import_table.get(&(file_path.clone(), original.to_string())).cloned() {
                            import_table.insert(
                                (file_path.clone(), local.to_string()),
                                target,
                            );
                        }
                    }
                }
            }
        }

        // Go imports: import "module/path" or import ( "module/path" )
        // Go uses the last path component as the package name
        let is_go = file_path.ends_with(".go");
        if is_go {
            static GO_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r#"(?m)"([^"]+)""#).unwrap()
            });

            // Only look in import blocks
            let import_section = extract_go_import_section(&content);
            for cap in GO_IMPORT_RE.captures_iter(&import_section) {
                let import_path = cap.get(1).unwrap().as_str();
                let pkg_name = import_path.rsplit('/').next().unwrap_or(import_path);

                // Map all entities from files matching this package name
                for (name, target_ids) in symbol_table.iter() {
                    for target_id in target_ids {
                        if let Some(entity) = entity_map.get(target_id) {
                            let stem = entity.file_path.rsplit('/').next().unwrap_or(&entity.file_path);
                            let stem = strip_file_ext(stem);
                            // Go: file stem or directory matches package name
                            if stem == pkg_name || entity.file_path.contains(&format!("{}/", pkg_name)) {
                                import_table.insert(
                                    (file_path.clone(), name.clone()),
                                    target_id.clone(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    import_table
}

/// Resolve a Rust import: find the target entity in the symbol table
/// by matching the imported name against entities in files whose stem matches source_module.
fn resolve_rust_import(
    file_path: &str,
    imported_name: &str,
    source_module: &str,
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
    import_table: &mut HashMap<(String, String), String>,
) {
    if let Some(target_ids) = symbol_table.get(imported_name) {
        let target = target_ids.iter().find(|id| {
            entity_map.get(*id).map_or(false, |e| {
                let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                let stem = strip_file_ext(stem);
                stem == source_module
            })
        });
        if let Some(target_id) = target {
            import_table.insert(
                (file_path.to_string(), imported_name.to_string()),
                target_id.clone(),
            );
        }
    }
}

/// Extract Go import section (everything inside import blocks).
fn extract_go_import_section(content: &str) -> String {
    let mut result = String::new();
    let mut in_import_block = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("import (") {
            in_import_block = true;
            continue;
        }
        if trimmed.starts_with("import \"") || trimmed.starts_with("import `") {
            result.push_str(trimmed);
            result.push('\n');
            continue;
        }
        if in_import_block {
            if trimmed == ")" {
                in_import_block = false;
            } else {
                result.push_str(trimmed);
                result.push('\n');
            }
        }
    }
    result
}

/// Strip JS/TS extensions from a module name.
fn strip_js_ext(s: &str) -> &str {
    s.strip_suffix(".js")
        .or_else(|| s.strip_suffix(".ts"))
        .or_else(|| s.strip_suffix(".jsx"))
        .or_else(|| s.strip_suffix(".tsx"))
        .unwrap_or(s)
}

/// Strip common file extensions from a filename.
fn strip_file_ext(s: &str) -> &str {
    s.strip_suffix(".py")
        .or_else(|| s.strip_suffix(".ts"))
        .or_else(|| s.strip_suffix(".js"))
        .or_else(|| s.strip_suffix(".tsx"))
        .or_else(|| s.strip_suffix(".jsx"))
        .or_else(|| s.strip_suffix(".rs"))
        .unwrap_or(s)
}

/// Strip comments and string literals from content to avoid false references.
/// Returns a new string with comments/docstrings replaced by spaces.
fn strip_comments_and_strings(content: &str) -> String {
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut result = vec![b' '; len];
    let mut i = 0;

    while i < len {
        // Triple-quoted strings (Python docstrings)
        if i + 2 < len && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
            i += 3;
            while i + 2 < len {
                if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    i += 3;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if i + 2 < len && bytes[i] == b'\'' && bytes[i + 1] == b'\'' && bytes[i + 2] == b'\'' {
            i += 3;
            while i + 2 < len {
                if bytes[i] == b'\'' && bytes[i + 1] == b'\'' && bytes[i + 2] == b'\'' {
                    i += 3;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Double-quoted strings
        if bytes[i] == b'"' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' { i += 2; continue; }
                if bytes[i] == b'"' { i += 1; break; }
                i += 1;
            }
            continue;
        }
        // Single-quoted strings
        if bytes[i] == b'\'' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' { i += 2; continue; }
                if bytes[i] == b'\'' { i += 1; break; }
                i += 1;
            }
            continue;
        }
        // Python/Ruby single-line comments
        if bytes[i] == b'#' {
            while i < len && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        // C-style single-line comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        // C-style block comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' { i += 2; break; }
                i += 1;
            }
            continue;
        }
        // Regular code: copy through
        result[i] = bytes[i];
        i += 1;
    }

    String::from_utf8_lossy(&result).into_owned()
}

/// Extract dot-chains (receiver.member) from content for precise resolution.
/// Returns unique (receiver, member) pairs found in the content.
fn extract_dot_chains<'a>(content: &'a str) -> Vec<(&'a str, &'a str)> {
    static DOT_CHAIN_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b([A-Za-z_]\w*)\.([A-Za-z_]\w*)").unwrap()
    });

    let mut chains = Vec::new();
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for cap in DOT_CHAIN_RE.captures_iter(content) {
        let receiver = cap.get(1).unwrap().as_str();
        let member = cap.get(2).unwrap().as_str();
        if seen.insert((receiver, member)) {
            chains.push((receiver, member));
        }
    }
    chains
}

/// Extract identifier references from entity content using simple token analysis.
/// Strips comments and strings first to avoid false positives from docstrings.
/// Returns borrowed slices from the stripped content.
fn extract_references_from_content<'a>(content: &'a str, own_name: &str) -> Vec<&'a str> {
    // We need to figure out which words appear only in comments/strings vs real code.
    // Strategy: strip comments/strings, then only accept words that appear in the stripped version.
    let stripped = strip_comments_and_strings(content);
    let stripped_words: HashSet<&str> = stripped
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();

    let mut refs = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for word in content.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if word.is_empty() || word == own_name {
            continue;
        }
        if is_keyword(word) || word.len() < 2 {
            continue;
        }
        // Skip very short lowercase identifiers (likely local vars: i, x, a, ok, id, etc.)
        if word.starts_with(|c: char| c.is_lowercase()) && word.len() < 3 {
            continue;
        }
        if !word.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            continue;
        }
        // Skip common local variable names that create false graph edges
        if is_common_local_name(word) {
            continue;
        }
        // Skip words that only appear in comments/strings
        if !stripped_words.contains(word) {
            continue;
        }
        if seen.insert(word) {
            refs.push(word);
        }
    }

    refs
}

static COMMON_LOCAL_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "result", "results", "data", "config", "value", "values",
        "item", "items", "input", "output", "args", "opts",
        "name", "path", "file", "line", "count", "index",
        "temp", "prev", "next", "curr", "current", "node",
        "left", "right", "root", "head", "tail", "body",
        "text", "content", "source", "target", "entry",
        "error", "errors", "message", "response", "request",
        "context", "state", "props", "event", "handler",
        "callback", "options", "params", "query", "list",
        "base", "info", "meta", "kind", "mode", "flag",
        "size", "length", "width", "height", "start", "stop",
        "begin", "done", "found", "status", "code", "test",
    ].into_iter().collect()
});

/// Names that are overwhelmingly local variables, not entity references.
/// These create massive false-positive edges in the dependency graph.
fn is_common_local_name(word: &str) -> bool {
    COMMON_LOCAL_NAMES.contains(word)
}

/// Infer reference type from context using word-boundary-aware matching.
fn infer_ref_type(content: &str, ref_name: &str) -> RefType {
    // Check if it's a function call: ref_name followed by ( with word boundary before.
    // Avoids format! allocation by finding ref_name and checking the next char.
    let bytes = content.as_bytes();
    let name_bytes = ref_name.as_bytes();
    let mut search_start = 0;
    while let Some(rel_pos) = content[search_start..].find(ref_name) {
        let pos = search_start + rel_pos;
        let after = pos + name_bytes.len();
        // Check next char is '('
        if after < bytes.len() && bytes[after] == b'(' {
            // Verify word boundary before
            let is_boundary = pos == 0 || {
                let prev = bytes[pos - 1];
                !prev.is_ascii_alphanumeric() && prev != b'_'
            };
            if is_boundary {
                return RefType::Calls;
            }
        }
        // Advance past pos to the next char boundary to avoid slicing inside a multi-byte UTF-8 char.
        search_start = pos + 1;
        while search_start < content.len() && !content.is_char_boundary(search_start) {
            search_start += 1;
        }
    }

    // Check if it's in an import/use statement (line-level, not substring)
    for line in content.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("import ") || trimmed.starts_with("use ")
            || trimmed.starts_with("from ") || trimmed.starts_with("require("))
            && trimmed.contains(ref_name)
        {
            return RefType::Imports;
        }
    }

    // Default to type reference
    RefType::TypeRef
}

static KEYWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // Common across languages
        "if", "else", "for", "while", "do", "switch", "case", "break",
        "continue", "return", "try", "catch", "finally", "throw",
        "new", "delete", "typeof", "instanceof", "in", "of",
        "true", "false", "null", "undefined", "void", "this",
        "super", "class", "extends", "implements", "interface",
        "enum", "const", "let", "var", "function", "async",
        "await", "yield", "import", "export", "default", "from",
        "as", "static", "public", "private", "protected",
        "abstract", "final", "override",
        // Rust
        "fn", "pub", "mod", "use", "struct", "impl", "trait",
        "where", "type", "self", "Self", "mut", "ref", "match",
        "loop", "move", "unsafe", "extern", "crate", "dyn",
        // Python
        "def", "elif", "except", "raise", "with",
        "pass", "lambda", "nonlocal", "global", "assert",
        "True", "False", "and", "or", "not", "is",
        // Go
        "func", "package", "range", "select", "chan", "go",
        "defer", "map", "make", "append", "len", "cap",
        // C/C++
        "auto", "register", "volatile", "sizeof", "typedef",
        "template", "typename", "namespace", "virtual", "inline",
        "constexpr", "nullptr", "noexcept", "explicit", "friend",
        "operator", "using", "cout", "endl", "cerr", "cin",
        "printf", "scanf", "malloc", "free", "NULL", "include",
        "ifdef", "ifndef", "endif", "define", "pragma",
        // Ruby
        "end", "then", "elsif", "unless", "until",
        "begin", "rescue", "ensure", "when", "require",
        "attr_accessor", "attr_reader", "attr_writer",
        "puts", "nil", "module", "defined",
        // C#
        "internal", "sealed", "readonly",
        "partial", "delegate", "event", "params", "out",
        "object", "decimal", "sbyte", "ushort", "uint",
        "ulong", "nint", "nuint", "dynamic",
        "get", "set", "value", "init", "record",
        // Types (primitives)
        "string", "number", "boolean", "int", "float", "double",
        "bool", "char", "byte", "i8", "i16", "i32", "i64",
        "u8", "u16", "u32", "u64", "f32", "f64", "usize",
        "isize", "str", "String", "Vec", "Option", "Result",
        "Box", "Arc", "Rc", "HashMap", "HashSet", "Some",
        "Ok", "Err",
    ].into_iter().collect()
});

fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{FileChange, FileStatus};
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, ParserRegistry) {
        let dir = TempDir::new().unwrap();
        let registry = crate::parser::plugins::create_default_registry();
        (dir, registry)
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_incremental_add_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // Start with one file
        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\n");

        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 2);

        // Add a new file
        write_file(root, "c.ts", "export function baz() { return foo(); }\n");
        graph.update_from_changes(
            &[FileChange {
                file_path: "c.ts".into(),
                status: FileStatus::Added,
                old_file_path: None,
                before_content: None,
                after_content: None, // will read from disk
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 3);
        assert!(graph.entities.contains_key("c.ts::function::baz"));
        // baz references foo
        let baz_deps = graph.get_dependencies("c.ts::function::baz");
        assert!(
            baz_deps.iter().any(|d| d.name == "foo"),
            "baz should depend on foo. Deps: {:?}",
            baz_deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_incremental_delete_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\n");

        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 2);

        // Delete b.ts
        graph.update_from_changes(
            &[FileChange {
                file_path: "b.ts".into(),
                status: FileStatus::Deleted,
                old_file_path: None,
                before_content: None,
                after_content: None,
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 1);
        assert!(!graph.entities.contains_key("b.ts::function::bar"));
        // foo's dependency on bar should be pruned
        let foo_deps = graph.get_dependencies("a.ts::function::foo");
        assert!(
            foo_deps.is_empty(),
            "foo's deps should be empty after bar deleted. Deps: {:?}",
            foo_deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_incremental_modify_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\nexport function baz() { return 2; }\n");

        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 3);

        // Modify a.ts to call baz instead of bar
        write_file(root, "a.ts", "export function foo() { return baz(); }\n");
        graph.update_from_changes(
            &[FileChange {
                file_path: "a.ts".into(),
                status: FileStatus::Modified,
                old_file_path: None,
                before_content: None,
                after_content: None,
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 3);
        // foo should now depend on baz, not bar
        let foo_deps = graph.get_dependencies("a.ts::function::foo");
        let dep_names: Vec<&str> = foo_deps.iter().map(|d| d.name.as_str()).collect();
        assert!(dep_names.contains(&"baz"), "foo should depend on baz after modification. Deps: {:?}", dep_names);
        assert!(!dep_names.contains(&"bar"), "foo should no longer depend on bar. Deps: {:?}", dep_names);
    }

    #[test]
    fn test_incremental_with_content() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return 1; }\n");
        let (mut graph, _) = EntityGraph::build(root, &["a.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 1);

        // Add file with content provided directly (no disk read needed)
        graph.update_from_changes(
            &[FileChange {
                file_path: "b.ts".into(),
                status: FileStatus::Added,
                old_file_path: None,
                before_content: None,
                after_content: Some("export function bar() { return foo(); }\n".into()),
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 2);
        let bar_deps = graph.get_dependencies("b.ts::function::bar");
        assert!(bar_deps.iter().any(|d| d.name == "foo"));
    }

    #[test]
    fn test_extract_references() {
        let content = "function processData(input) {\n  const result = validateInput(input);\n  return transform(result);\n}";
        let refs = extract_references_from_content(content, "processData");
        assert!(refs.contains(&"validateInput"));
        assert!(refs.contains(&"transform"));
        assert!(!refs.contains(&"processData")); // self excluded
    }

    #[test]
    fn test_extract_references_skips_keywords() {
        let content = "function foo() { if (true) { return false; } }";
        let refs = extract_references_from_content(content, "foo");
        assert!(!refs.contains(&"if"));
        assert!(!refs.contains(&"true"));
        assert!(!refs.contains(&"return"));
        assert!(!refs.contains(&"false"));
    }

    #[test]
    fn test_infer_ref_type_call() {
        assert_eq!(
            infer_ref_type("validateInput(data)", "validateInput"),
            RefType::Calls,
        );
    }

    #[test]
    fn test_infer_ref_type_type() {
        assert_eq!(
            infer_ref_type("let x: MyType = something", "MyType"),
            RefType::TypeRef,
        );
    }

    #[test]
    fn test_infer_ref_type_multibyte_utf8() {
        // Ensure no panic when content contains multi-byte UTF-8 characters
        assert_eq!(
            infer_ref_type("let café = foo(x)", "foo"),
            RefType::Calls,
        );
        assert_eq!(
            infer_ref_type("class HandicapfrPublicationFieldsEnum:\n    É = 1\n    bar()", "bar"),
            RefType::Calls,
        );
        // No match should not panic either
        assert_eq!(
            infer_ref_type("// 日本語コメント\nlet x = 1", "missing"),
            RefType::TypeRef,
        );
    }

    #[test]
    fn test_dot_chain_self_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "service.py", "\
class MyService:
    def process(self):
        return self.validate()

    def validate(self):
        return True
");

        let (graph, _) = EntityGraph::build(root, &["service.py".into()], &registry);

        // process should have an edge to validate via self.validate()
        let process_id = graph.entities.keys()
            .find(|id| id.contains("process"))
            .expect("process entity should exist");
        let deps = graph.get_dependencies(process_id);
        assert!(
            deps.iter().any(|d| d.name == "validate"),
            "process should depend on validate via self.validate(). Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_dot_chain_this_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "service.ts", "\
class UserService {
    process() {
        return this.validate();
    }
    validate() {
        return true;
    }
}
");

        let (graph, _) = EntityGraph::build(root, &["service.ts".into()], &registry);

        let process_id = graph.entities.keys()
            .find(|id| id.contains("process"))
            .expect("process entity should exist");
        let deps = graph.get_dependencies(process_id);
        assert!(
            deps.iter().any(|d| d.name == "validate"),
            "process should depend on validate via this.validate(). Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_dot_chain_class_static() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "utils.ts", "\
class MathUtils {
    static compute() { return 1; }
}
function caller() { return MathUtils.compute(); }
");

        let (graph, _) = EntityGraph::build(root, &["utils.ts".into()], &registry);

        let caller_id = graph.entities.keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter().any(|d| d.name == "compute"),
            "caller should depend on compute via MathUtils.compute(). Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_js_ts_import_resolution() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "helper.ts", "\
export function helper() { return 1; }
");
        write_file(root, "main.ts", "\
import { helper } from './helper';
export function main() { return helper(); }
");

        let (graph, _) = EntityGraph::build(
            root,
            &["helper.ts".into(), "main.ts".into()],
            &registry,
        );

        let main_id = graph.entities.keys()
            .find(|id| id.contains("main"))
            .expect("main entity should exist");
        let deps = graph.get_dependencies(main_id);
        assert!(
            deps.iter().any(|d| d.name == "helper"),
            "main should depend on helper via JS import. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_dot_chain_no_false_edges() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // Two classes with same method name "process".
        // self.process() in ClassA should NOT create edge to ClassB::process.
        write_file(root, "a.py", "\
class ClassA:
    def run(self):
        return self.process()

    def process(self):
        return 1
");
        write_file(root, "b.py", "\
class ClassB:
    def process(self):
        return 2
");

        let (graph, _) = EntityGraph::build(
            root,
            &["a.py".into(), "b.py".into()],
            &registry,
        );

        let run_id = graph.entities.keys()
            .find(|id| id.contains("run"))
            .expect("run entity should exist");
        let deps = graph.get_dependencies(run_id);
        // Should have edge to ClassA::process, NOT ClassB::process
        for dep in &deps {
            if dep.name == "process" {
                assert!(
                    dep.file_path == "a.py",
                    "run's process dep should be in a.py, not {}",
                    dep.file_path
                );
            }
        }
    }

    #[test]
    fn test_dot_chain_fallback() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // someVar.unknownMethod() - "someVar" is not a class,
        // so the chain is unresolved and words fall through to bag-of-words.
        // "helper" should still resolve via bag-of-words.
        write_file(root, "app.ts", "\
export function helper() { return 1; }
export function caller() {
    const val = helper();
    return val;
}
");

        let (graph, _) = EntityGraph::build(root, &["app.ts".into()], &registry);

        let caller_id = graph.entities.keys()
            .find(|id| id.contains("caller"))
            .expect("caller entity should exist");
        let deps = graph.get_dependencies(caller_id);
        assert!(
            deps.iter().any(|d| d.name == "helper"),
            "caller should still resolve helper via bag-of-words. Deps: {:?}",
            deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

}
