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

/// Entities deliberately not packed for a role: tests (their one-line
/// signature is pure noise; `sem impact --tests` lists them properly) and
/// past-the-cap transitive tails. The count preserves the signal ("covered
/// by 39 tests", "81 more transitive dependents") at one line instead of
/// one line each.
#[derive(Debug, Clone, Default)]
pub struct OmittedTail {
    pub role: String,
    pub entities: usize,
    pub tests: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ContextResult {
    pub entries: Vec<ContextEntry>,
    pub total_tokens: usize,
    pub truncated: bool,
    pub target_omitted: bool,
    pub omitted: Vec<OmittedTail>,
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

/// Head-truncate content to fit `cap_tokens`, keeping whole leading lines and
/// appending an explicit truncation marker (which must itself fit the cap).
/// Returns None when not even the first line plus the marker fits — callers
/// then fall back to the bare signature.
fn truncate_head(content: &str, cap_tokens: usize) -> Option<(String, usize)> {
    let total_lines = content.lines().count();
    let mut kept = String::new();
    let mut kept_lines = 0usize;
    for line in content.lines() {
        let candidate = if kept.is_empty() {
            line.to_string()
        } else {
            format!("{kept}\n{line}")
        };
        let marker = format!(
            "\n… truncated: {} more lines",
            total_lines.saturating_sub(kept_lines + 1)
        );
        if estimate_tokens(&format!("{candidate}{marker}")) > cap_tokens {
            break;
        }
        kept = candidate;
        kept_lines += 1;
    }
    if kept.is_empty() || kept_lines == total_lines {
        return None;
    }
    let out = format!(
        "{kept}\n… truncated: {} more lines",
        total_lines - kept_lines
    );
    let tokens = estimate_tokens(&out);
    Some((out, tokens))
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

    // 1. Target entity — it gets the budget FIRST, and degrades gracefully:
    // full body → head-truncated body (up to ~70% of the budget) → bare
    // signature → omitted. The target is what the caller asked about; it must
    // never starve while neighbors feast (previously a too-big target fell
    // straight to its first line and dependencies consumed the whole budget).
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
            let head_cap = (token_budget * 7 / 10).max(sig_tokens);
            if let Some((head, head_tokens)) = truncate_head(&entity.content, head_cap) {
                push_entry(
                    &mut result,
                    entity,
                    "target",
                    head,
                    head_tokens,
                    &mut included_ids,
                );
            } else if sig_tokens <= token_budget {
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

    // No single neighbor may cost more than the target itself did (with a
    // budget/10 floor so a tiny target still allows useful neighbor bodies).
    // Oversized neighbors degrade to signatures instead of dominating.
    let neighbor_full_cap = result.total_tokens.max(token_budget / 10);

    // Tests are counted, not packed: a related test's one-line signature is
    // pure noise ("#[test]"), while "covered by 39 tests" is one line of real
    // signal ("sem impact --tests" lists them properly). The exception is
    // when the target itself is a test — then its test neighborhood IS the
    // question. Transitive tiers are additionally capped: past the cap the
    // marginal signature stops paying for its tokens.
    const MAX_TRANSITIVE_ENTRIES: usize = 25;
    let target_is_test = entity_lookup
        .get(entity_id)
        .map(|e| is_test_entity(e))
        .unwrap_or(false);
    let mut tails: Vec<OmittedTail> = Vec::new();

    let direct_dependencies = graph.get_dependencies(entity_id);
    for dep_info in &direct_dependencies {
        if !target_is_test && lookup_is_test(&entity_lookup, dep_info.id.as_str()) {
            tally(&mut tails, "direct_dependency", true);
            continue;
        }
        add_full_or_signature(
            &mut result,
            &entity_lookup,
            dep_info.id.as_str(),
            "direct_dependency",
            token_budget,
            neighbor_full_cap,
            &mut included_ids,
        );
    }

    let direct_dependents = graph.get_dependents(entity_id);
    for dep_info in &direct_dependents {
        if !target_is_test && lookup_is_test(&entity_lookup, dep_info.id.as_str()) {
            tally(&mut tails, "direct_dependent", true);
            continue;
        }
        add_full_or_signature(
            &mut result,
            &entity_lookup,
            dep_info.id.as_str(),
            "direct_dependent",
            token_budget,
            neighbor_full_cap,
            &mut included_ids,
        );
    }

    let direct_dependency_ids: HashSet<&str> =
        direct_dependencies.iter().map(|d| d.id.as_str()).collect();
    let direct_dependent_ids: HashSet<&str> =
        direct_dependents.iter().map(|d| d.id.as_str()).collect();

    for (role, relationships, direct_ids) in [
        (
            "transitive_dependency",
            &graph.dependencies,
            &direct_dependency_ids,
        ),
        (
            "transitive_dependent",
            &graph.dependents,
            &direct_dependent_ids,
        ),
    ] {
        let mut packed = 0usize;
        for dep_info in collect_reachable_related(graph, entity_id, relationships, max_hops) {
            if direct_ids.contains(dep_info.id.as_str()) {
                continue;
            }
            let Some(entity) = entity_lookup.get(dep_info.id.as_str()) else {
                continue;
            };
            if !target_is_test && is_test_entity(entity) {
                tally(&mut tails, role, true);
                continue;
            }
            if packed >= MAX_TRANSITIVE_ENTRIES || is_stub_signature(&entity.content) {
                tally(&mut tails, role, false);
                continue;
            }
            let before = result.entries.len();
            add_signature(
                &mut result,
                &entity_lookup,
                dep_info.id.as_str(),
                role,
                token_budget,
                &mut included_ids,
            );
            if result.entries.len() > before {
                packed += 1;
            }
        }
    }

    result.omitted = tails;
    result
}

/// A test entity by name, attribute, or location. Used to fold tests into
/// per-role counts instead of packing their noise signatures.
fn is_test_entity(entity: &SemanticEntity) -> bool {
    let head = entity.content.trim_start();
    entity.name.starts_with("test_")
        || entity.name == "tests"
        || head.starts_with("#[test]")
        || head.starts_with("#[tokio::test]")
        || head.starts_with("#[cfg(test)]")
        || entity.file_path.contains("/tests/")
        || entity.file_path.ends_with("_test.go")
        || entity.file_path.ends_with(".test.ts")
        || entity.file_path.ends_with(".test.js")
        || entity.file_path.ends_with(".spec.ts")
        || entity.file_path.ends_with(".spec.js")
        || entity.file_path.ends_with("_test.py")
        || std::path::Path::new(&entity.file_path)
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.starts_with("test_"))
}

fn lookup_is_test(entity_lookup: &HashMap<&str, &SemanticEntity>, entity_id: &str) -> bool {
    entity_lookup
        .get(entity_id)
        .is_some_and(|e| is_test_entity(e))
}

/// A signature line that carries no information on its own (a bare attribute,
/// decorator, or comment opener) — not worth a packed entry.
fn is_stub_signature(content: &str) -> bool {
    let sig = content.lines().next().unwrap_or("").trim();
    sig.is_empty()
        || sig.starts_with("#[")
        || sig.starts_with("@")
        || sig.starts_with("//")
        || sig.starts_with("/*")
}

fn tally(tails: &mut Vec<OmittedTail>, role: &str, is_test: bool) {
    if let Some(tail) = tails.iter_mut().find(|t| t.role == role) {
        tail.entities += 1;
        if is_test {
            tail.tests += 1;
        }
    } else {
        tails.push(OmittedTail {
            role: role.to_string(),
            entities: 1,
            tests: usize::from(is_test),
        });
    }
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
    full_cap: usize,
    included_ids: &mut HashSet<String>,
) {
    if included_ids.contains(entity_id) {
        return;
    }

    let Some(entity) = entity_lookup.get(entity_id) else {
        return;
    };

    let full_tokens = estimate_tokens(&entity.content);
    if full_tokens <= full_cap && result.total_tokens + full_tokens <= token_budget {
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
        let ids = [
            "a.py::function::a",
            "a.py::function::b",
            "a.py::function::c",
            "a.py::function::d",
        ];
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
            start_byte: None,
            end_byte: None,
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
        let entity_infos: crate::parser::graph::EntityInfoMap = entities
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

    #[test]
    fn big_target_gets_head_truncated_majority_of_budget() {
        // A target too big for the budget must degrade to a head-truncated
        // body (with an explicit marker), not collapse to its first line.
        let body = (0..200)
            .map(|i| format!("    line_{i} = compute_{i}()"))
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!("def big_target():\n{body}");
        let entities = vec![entity("a.py::function::big_target", "big_target", &content)];
        let graph = graph_from_entities(&entities, vec![]);

        let result = build_context_result(&graph, "a.py::function::big_target", &entities, 100);

        assert!(!result.target_omitted);
        assert!(result.truncated);
        assert_eq!(result.entries.len(), 1);
        let target = &result.entries[0];
        assert!(target.content.lines().count() > 5, "more than the bare signature");
        assert!(target.content.contains("… truncated:"), "explicit marker");
        assert!(
            target.estimated_tokens >= 50 && target.estimated_tokens <= 100,
            "target consumes the majority of the budget, got {}",
            target.estimated_tokens
        );
    }

    #[test]
    fn oversized_neighbor_degrades_to_signature_not_full_body() {
        // A neighbor may never cost more than the target did: the giant
        // dependency gets a signature, the small one keeps its full body.
        let giant_body = (0..300)
            .map(|i| format!("    g_{i} = {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entities = vec![
            entity("a.py::function::small_target", "small_target", "def small_target():\n    return helper() + giant()"),
            entity("a.py::function::giant", "giant", &format!("def giant():\n{giant_body}")),
            entity("a.py::function::helper", "helper", "def helper():\n    return 42"),
        ];
        let graph = graph_from_entities(
            &entities,
            vec![
                edge("a.py::function::small_target", "a.py::function::giant"),
                edge("a.py::function::small_target", "a.py::function::helper"),
            ],
        );

        let result =
            build_context_result(&graph, "a.py::function::small_target", &entities, 3000);

        let giant = result
            .entries
            .iter()
            .find(|e| e.entity_name == "giant")
            .expect("giant included");
        assert_eq!(
            giant.content.lines().count(),
            1,
            "giant neighbor degraded to signature, got {} lines",
            giant.content.lines().count()
        );
        let helper = result
            .entries
            .iter()
            .find(|e| e.entity_name == "helper")
            .expect("helper included");
        assert!(helper.content.contains("return 42"), "small neighbor keeps full body");
    }
}
