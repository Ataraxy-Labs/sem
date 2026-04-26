//! Context budgeting: pack optimal entity context into a token budget.
//! Priority: target entity (full) > direct dependents (full) > transitive (signature only).

use std::collections::HashMap;

use crate::model::entity::SemanticEntity;
use crate::parser::graph::EntityGraph;

#[derive(Debug, Clone)]
pub struct ContextEntry {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: String,
    pub file_path: String,
    pub role: String, // "target", "direct_dependent", "transitive_dependent"
    pub content: String,
    pub estimated_tokens: usize,
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
/// 2. Direct dependents (full content)
/// 3. Transitive dependents (signature only)
pub fn build_context(
    graph: &EntityGraph,
    entity_id: &str,
    all_entities: &[SemanticEntity],
    token_budget: usize,
) -> Vec<ContextEntry> {
    // Build content lookup: entity_id -> SemanticEntity
    let entity_lookup: HashMap<&str, &SemanticEntity> = all_entities
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();

    let mut entries = Vec::new();
    let mut tokens_used = 0usize;

    // 1. Target entity (full)
    if let Some(entity) = entity_lookup.get(entity_id) {
        let tokens = estimate_tokens(&entity.content);
        if tokens_used + tokens <= token_budget {
            entries.push(ContextEntry {
                entity_id: entity.id.clone(),
                entity_name: entity.name.clone(),
                entity_type: entity.entity_type.clone(),
                file_path: entity.file_path.clone(),
                role: "target".to_string(),
                content: entity.content.clone(),
                estimated_tokens: tokens,
            });
            tokens_used += tokens;
        }
    }

    // 2. Direct dependents (full content)
    let direct_deps = graph.get_dependents(entity_id);
    for dep_info in &direct_deps {
        if tokens_used >= token_budget {
            break;
        }
        if let Some(entity) = entity_lookup.get(dep_info.id.as_str()) {
            let tokens = estimate_tokens(&entity.content);
            if tokens_used + tokens <= token_budget {
                entries.push(ContextEntry {
                    entity_id: entity.id.clone(),
                    entity_name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    role: "direct_dependent".to_string(),
                    content: entity.content.clone(),
                    estimated_tokens: tokens,
                });
                tokens_used += tokens;
            }
        }
    }

    // 3. Transitive dependents (signature only)
    let all_impact = graph.impact_analysis(entity_id);
    let direct_ids: std::collections::HashSet<&str> =
        direct_deps.iter().map(|d| d.id.as_str()).collect();

    for dep_info in &all_impact {
        if tokens_used >= token_budget {
            break;
        }
        // Skip direct deps (already included with full content)
        if direct_ids.contains(dep_info.id.as_str()) {
            continue;
        }
        if let Some(entity) = entity_lookup.get(dep_info.id.as_str()) {
            let sig = signature_only(&entity.content);
            let tokens = estimate_tokens(&sig);
            if tokens_used + tokens <= token_budget {
                entries.push(ContextEntry {
                    entity_id: entity.id.clone(),
                    entity_name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    role: "transitive_dependent".to_string(),
                    content: sig,
                    estimated_tokens: tokens,
                });
                tokens_used += tokens;
            }
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
