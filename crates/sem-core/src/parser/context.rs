//! Context budgeting: pack optimal entity context into a token budget.
//! Priority: target entity > direct dependencies > direct dependents > transitive dependencies >
//! transitive dependents.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::model::entity::SemanticEntity;
use crate::parser::graph::{EntityAdjacencyMap, EntityGraph};

#[derive(Debug, Clone)]
pub struct ContextEntry {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: String,
    pub file_path: String,
    pub role: String,
    pub content: String,
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ContextResult {
    pub entries: Vec<ContextEntry>,
    pub total_tokens: usize,
    pub truncated: bool,
    pub target_omitted: bool,
}

/// Estimate token count from content. Rough heuristic: ~1.3 tokens per whitespace-separated word.
fn estimate_tokens(content: &str) -> usize {
    let words = content.split_whitespace().count();
    words * 13 / 10
}

/// Extract just the first line (signature) of an entity's content.
fn signature_only(content: &str) -> String {
    content.lines().next().unwrap_or("").to_string()
}

/// Build a context set for a target entity within a token budget.
///
/// Greedy knapsack by priority:
/// 1. Target entity (full content)
/// 2. Direct dependencies (full content, signature fallback)
/// 3. Direct dependents (full content, signature fallback)
/// 4. Transitive dependencies (signature only)
/// 5. Transitive dependents (signature only)
pub fn build_context(
    graph: &EntityGraph,
    entity_id: &str,
    all_entities: &[SemanticEntity],
    token_budget: usize,
) -> Vec<ContextEntry> {
    build_context_result(graph, entity_id, all_entities, token_budget).entries
}

/// Build a context set plus budget metadata for a target entity. Unbounded
/// transitive reach (capped only by the token budget).
pub fn build_context_result(
    graph: &EntityGraph,
    entity_id: &str,
    all_entities: &[SemanticEntity],
    token_budget: usize,
) -> ContextResult {
    build_context_result_bounded(graph, entity_id, all_entities, token_budget, 0)
}

/// Like [`build_context_result`], but bounds transitive related entities to
/// `max_hops` graph hops from the target (0 = unbounded). Lets callers ask for
/// "the entity and everything within N hops" instead of "fill the token budget".
pub fn build_context_result_bounded(
    graph: &EntityGraph,
    entity_id: &str,
    all_entities: &[SemanticEntity],
    token_budget: usize,
    max_hops: usize,
) -> ContextResult {
    // Build content lookup: entity_id -> SemanticEntity
    let entity_lookup: HashMap<&str, &SemanticEntity> =
        all_entities.iter().map(|e| (e.id.as_str(), e)).collect();

    let mut result = ContextResult::default();
    let mut included_ids = HashSet::new();

    // 1. Target entity. Keep the budget strict: if even the signature does not fit,
    // omit the target and return an empty result instead of overspending.
    if let Some(entity) = entity_lookup.get(entity_id) {
        let full_tokens = estimate_tokens(&entity.content);
        if full_tokens <= token_budget {
            push_entry(
                &mut result,
                entity,
                "target",
                entity.content.clone(),
                full_tokens,
                &mut included_ids,
            );
        } else {
            result.truncated = true;
            let sig = signature_only(&entity.content);
            let sig_tokens = estimate_tokens(&sig);
            if sig_tokens <= token_budget {
                push_entry(
                    &mut result,
                    entity,
                    "target",
                    sig,
                    sig_tokens,
                    &mut included_ids,
                );
            } else {
                // Strict context budget contract: no related entries are useful if the
                // requested target cannot be represented inside the budget.
                result.target_omitted = true;
                return result;
            }
        };
    }

    let direct_dependencies = graph.get_dependencies(entity_id);
    for dep_info in &direct_dependencies {
        add_full_or_signature(
            &mut result,
            &entity_lookup,
            dep_info.id.as_str(),
            "direct_dependency",
            token_budget,
            &mut included_ids,
        );
    }

    let direct_dependents = graph.get_dependents(entity_id);
    for dep_info in &direct_dependents {
        add_full_or_signature(
            &mut result,
            &entity_lookup,
            dep_info.id.as_str(),
            "direct_dependent",
            token_budget,
            &mut included_ids,
        );
    }

    let direct_dependency_ids: HashSet<&str> =
        direct_dependencies.iter().map(|d| d.id.as_str()).collect();
    let direct_dependent_ids: HashSet<&str> =
        direct_dependents.iter().map(|d| d.id.as_str()).collect();

    for dep_info in collect_reachable_related(graph, entity_id, &graph.dependencies, max_hops) {
        if direct_dependency_ids.contains(dep_info.id.as_str()) {
            continue;
        }
        add_signature(
            &mut result,
            &entity_lookup,
            dep_info.id.as_str(),
            "transitive_dependency",
            token_budget,
            &mut included_ids,
        );
    }

    for dep_info in collect_reachable_related(graph, entity_id, &graph.dependents, max_hops) {
        if direct_dependent_ids.contains(dep_info.id.as_str()) {
            continue;
        }
        add_signature(
            &mut result,
            &entity_lookup,
            dep_info.id.as_str(),
            "transitive_dependent",
            token_budget,
            &mut included_ids,
        );
    }

    result
}

fn push_entry(
    result: &mut ContextResult,
    entity: &SemanticEntity,
    role: &str,
    content: String,
    tokens: usize,
    included_ids: &mut HashSet<String>,
) {
    result.entries.push(ContextEntry {
        entity_id: entity.id.clone(),
        entity_name: entity.name.clone(),
        entity_type: entity.entity_type.clone(),
        file_path: entity.file_path.clone(),
        role: role.to_string(),
        content,
        estimated_tokens: tokens,
    });
    result.total_tokens += tokens;
    included_ids.insert(entity.id.clone());
}

fn add_full_or_signature(
    result: &mut ContextResult,
    entity_lookup: &HashMap<&str, &SemanticEntity>,
    entity_id: &str,
    role: &str,
    token_budget: usize,
    included_ids: &mut HashSet<String>,
) {
    if included_ids.contains(entity_id) {
        return;
    }

    let Some(entity) = entity_lookup.get(entity_id) else {
        return;
    };

    let full_tokens = estimate_tokens(&entity.content);
    if result.total_tokens + full_tokens <= token_budget {
        push_entry(
            result,
            entity,
            role,
            entity.content.clone(),
            full_tokens,
            included_ids,
        );
        return;
    }

    result.truncated = true;
    add_signature(
        result,
        entity_lookup,
        entity_id,
        role,
        token_budget,
        included_ids,
    );
}

fn add_signature(
    result: &mut ContextResult,
    entity_lookup: &HashMap<&str, &SemanticEntity>,
    entity_id: &str,
    role: &str,
    token_budget: usize,
    included_ids: &mut HashSet<String>,
) {
    if included_ids.contains(entity_id) {
        return;
    }

    let Some(entity) = entity_lookup.get(entity_id) else {
        return;
    };

    let sig = signature_only(&entity.content);
    let tokens = estimate_tokens(&sig);
    if result.total_tokens + tokens <= token_budget {
        push_entry(result, entity, role, sig, tokens, included_ids);
    } else {
        result.truncated = true;
    }
}

/// Collect related entities reachable from `entity_id`, excluding the starting
/// entity. `max_hops` of 0 means unbounded (capped only by MAX_VISITED);
/// otherwise the BFS stops expanding past that many hops.
fn collect_reachable_related<'a>(
    graph: &'a EntityGraph,
    entity_id: &str,
    relationships: &'a EntityAdjacencyMap,
    max_hops: usize,
) -> Vec<&'a crate::parser::graph::EntityInfo> {
    const MAX_VISITED: usize = 10_000;

    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<(&str, usize)> = VecDeque::new();
    let mut result = Vec::new();

    let start_key = match graph.entities.get_key_value(entity_id) {
        Some((key, _)) => key.as_str(),
        None => return result,
    };

    queue.push_back((start_key, 0));
    visited.insert(start_key);

    while let Some((current, depth)) = queue.pop_front() {
        if result.len() >= MAX_VISITED {
            break;
        }
        if max_hops > 0 && depth >= max_hops {
            continue;
        }

        if let Some(next_ids) = relationships.get(current) {
            for next_id in next_ids {
                if visited.insert(next_id.as_str()) {
                    if let Some(info) = graph.entities.get(next_id.as_str()) {
                        result.push(info);
                        if result.len() >= MAX_VISITED {
                            return result;
                        }
                    }
                    queue.push_back((next_id.as_str(), depth + 1));
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};
    use std::collections::HashMap;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello world"), 2); // 2 * 13 / 10 = 2
        assert_eq!(estimate_tokens("fn foo(a: i32, b: i32) -> bool {"), 10); // 8 words * 13 / 10 = 10
    }

    #[test]
    fn test_signature_only() {
        assert_eq!(
            signature_only("fn foo(a: i32) {\n    a + 1\n}"),
            "fn foo(a: i32) {"
        );
    }

    #[test]
    fn test_target_omitted_when_signature_exceeds_budget() {
        let entities = vec![entity(
            "a.py::function::helper_b",
            "helper_b",
            "def helper_b():\n    return 1",
        )];
        let graph = graph_from_entities(&entities, vec![]);

        let result = build_context_result(&graph, "a.py::function::helper_b", &entities, 1);

        assert!(result.entries.is_empty());
        assert_eq!(result.total_tokens, 0);
        assert!(result.truncated);
        assert!(result.target_omitted);
    }

    #[test]
    fn test_target_signature_respects_budget() {
        let entities = vec![entity(
            "a.py::function::helper_b",
            "helper_b",
            "def helper_b():\n    return expensive_value()",
        )];
        let graph = graph_from_entities(&entities, vec![]);

        let result = build_context_result(&graph, "a.py::function::helper_b", &entities, 2);

        assert_eq!(result.total_tokens, 2);
        assert!(result.truncated);
        assert!(!result.target_omitted);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].role, "target");
        assert_eq!(result.entries[0].content, "def helper_b():");
    }

    #[test]
    fn test_context_includes_dependencies_before_dependents() {
        let entities = vec![
            entity(
                "a.py::function::main",
                "main",
                "def main():\n    return helper_a() + helper_b()",
            ),
            entity(
                "a.py::function::helper_a",
                "helper_a",
                "def helper_a():\n    return leaf()",
            ),
            entity(
                "a.py::function::helper_b",
                "helper_b",
                "def helper_b():\n    return 2",
            ),
            entity("a.py::function::leaf", "leaf", "def leaf():\n    return 1"),
            entity(
                "a.py::class::Caller",
                "Caller",
                "class Caller:\n    def go(self):\n        return main()",
            ),
            entity(
                "a.py::class::Outer",
                "Outer",
                "class Outer:\n    def go(self):\n        return Caller().go()",
            ),
        ];
        let graph = graph_from_entities(
            &entities,
            vec![
                edge("a.py::function::main", "a.py::function::helper_a"),
                edge("a.py::function::main", "a.py::function::helper_b"),
                edge("a.py::function::helper_a", "a.py::function::leaf"),
                edge("a.py::class::Caller", "a.py::function::main"),
                edge("a.py::class::Outer", "a.py::class::Caller"),
            ],
        );

        let result = build_context_result(&graph, "a.py::function::main", &entities, 999);
        let roles_and_names: Vec<(&str, &str)> = result
            .entries
            .iter()
            .map(|entry| (entry.role.as_str(), entry.entity_name.as_str()))
            .collect();

        assert_eq!(
            roles_and_names,
            vec![
                ("target", "main"),
                ("direct_dependency", "helper_a"),
                ("direct_dependency", "helper_b"),
                ("direct_dependent", "Caller"),
                ("transitive_dependency", "leaf"),
                ("transitive_dependent", "Outer"),
            ]
        );
        assert!(!result.truncated);
        assert!(!result.target_omitted);
        assert!(result.total_tokens <= 999);
    }

    #[test]
    fn test_collect_transitive_caps_results() {
        let mut entities = Vec::new();
        let mut edges = Vec::new();

        for index in 0..=10_001 {
            let id = format!("a.py::function::helper_{index}");
            entities.push(entity(
                &id,
                &format!("helper_{index}"),
                "def helper():\n    return 1",
            ));
            if index > 0 {
                edges.push(edge(&format!("a.py::function::helper_{}", index - 1), &id));
            }
        }

        let graph = graph_from_entities(&entities, edges);
        let result =
            collect_reachable_related(&graph, "a.py::function::helper_0", &graph.dependencies, 0);

        assert_eq!(result.len(), 10_000);
    }

    #[test]
    fn collect_reachable_related_respects_max_hops() {
        // Chain: a -> b -> c -> d (each depends on the next).
        let ids = ["a.py::function::a", "a.py::function::b", "a.py::function::c", "a.py::function::d"];
        let entities: Vec<SemanticEntity> = ids
            .iter()
            .map(|id| entity(id, id.rsplit("::").next().unwrap(), "fn x() {}"))
            .collect();
        let edges = vec![
            edge(ids[0], ids[1]),
            edge(ids[1], ids[2]),
            edge(ids[2], ids[3]),
        ];
        let graph = graph_from_entities(&entities, edges);

        let hop1 = collect_reachable_related(&graph, ids[0], &graph.dependencies, 1);
        assert_eq!(hop1.len(), 1, "1 hop reaches only b");
        let hop2 = collect_reachable_related(&graph, ids[0], &graph.dependencies, 2);
        assert_eq!(hop2.len(), 2, "2 hops reach b and c");
        let unbounded = collect_reachable_related(&graph, ids[0], &graph.dependencies, 0);
        assert_eq!(unbounded.len(), 3, "unbounded reaches b, c, d");
    }

    fn entity(id: &str, name: &str, content: &str) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: "a.py".to_string(),
            entity_type: id.split("::").nth(1).unwrap_or("function").to_string(),
            name: name.to_string(),
            parent_id: None,
            content: content.to_string(),
            content_hash: String::new(),
            structural_hash: None,
            start_line: 1,
            end_line: content.lines().count(),
            metadata: None,
        }
    }

    fn edge(from_entity: &str, to_entity: &str) -> EntityRef {
        EntityRef {
            from_entity: from_entity.to_string(),
            to_entity: to_entity.to_string(),
            ref_type: RefType::Calls,
        }
    }

    fn graph_from_entities(entities: &[SemanticEntity], edges: Vec<EntityRef>) -> EntityGraph {
        let entity_infos: HashMap<String, EntityInfo> = entities
            .iter()
            .map(|entity| {
                (
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
                )
            })
            .collect();

        EntityGraph::from_parts(entity_infos, edges)
    }
}
