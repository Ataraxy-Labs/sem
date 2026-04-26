use std::collections::HashMap;
use std::path::Path;

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::scope_resolve;
use sem_core::parser::verify::{
    extract_param_info_ts, find_arity_mismatches, find_broken_callers, ArityMismatch,
};

fn extract_all_entities(root: &Path, files: &[&str]) -> Vec<SemanticEntity> {
    let registry = create_default_registry();
    let mut all = Vec::new();
    for fp in files {
        let full = root.join(fp);
        let content = std::fs::read_to_string(&full).unwrap();
        if let Some(plugin) = registry.get_plugin_with_content(fp, &content) {
            all.extend(plugin.extract_entities(&content, fp));
        }
    }
    all
}

fn build_graph_from_entities(
    root: &Path,
    files: &[&str],
    entities: &[SemanticEntity],
) -> EntityGraph {
    let entity_map: HashMap<String, EntityInfo> = entities
        .iter()
        .map(|e| {
            (
                e.id.clone(),
                EntityInfo {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    entity_type: e.entity_type.clone(),
                    file_path: e.file_path.clone(),
                    parent_id: e.parent_id.clone(),
                    start_line: e.start_line,
                    end_line: e.end_line,
                },
            )
        })
        .collect();

    let file_strs: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    let scope_result =
        scope_resolve::resolve_with_scopes(root, &file_strs, entities, &entity_map, None);

    let edges: Vec<EntityRef> = scope_result
        .edges
        .into_iter()
        .map(|(from, to, ref_type)| EntityRef {
            from_entity: from,
            to_entity: to,
            ref_type,
        })
        .collect();

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
        entities: entity_map,
        edges,
        dependents,
        dependencies,
    }
}

#[test]
fn verify_param_info_extraction() {
    // Python
    let info = extract_param_info_ts("def foo(a, b, c=3):\n    pass", "test.py").unwrap();
    assert_eq!(info.min_params, 2, "python min_params");
    assert_eq!(info.max_params, 3, "python max_params");
    assert!(!info.is_variadic, "python not variadic");

    // Python self excluded
    let info = extract_param_info_ts("def bar(self, x, y):\n    pass", "test.py").unwrap();
    assert_eq!(info.min_params, 2, "python self excluded");

    // Python variadic
    let info = extract_param_info_ts("def baz(a, *args):\n    pass", "test.py").unwrap();
    assert!(info.is_variadic, "python variadic");

    // TypeScript
    let info = extract_param_info_ts(
        "function greet(name: string, greeting?: string): void {}",
        "test.ts",
    )
    .unwrap();
    assert_eq!(info.min_params, 1, "ts min_params");
    assert_eq!(info.max_params, 2, "ts max_params");

    // Rust
    let info =
        extract_param_info_ts("fn process(&self, data: Vec<u8>) -> Result<()> {}", "test.rs")
            .unwrap();
    assert_eq!(info.min_params, 1, "rust self excluded");
    assert_eq!(info.max_params, 1, "rust max_params");

    // Go
    let info = extract_param_info_ts(
        "func handler(w http.ResponseWriter, r *http.Request) {}",
        "test.go",
    )
    .unwrap();
    assert_eq!(info.min_params, 2, "go params");
}

#[test]
fn verify_arity_mismatches_detected() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verify_test/python");
    let files = &["functions.py", "callers.py"];
    let entities = extract_all_entities(&root, files);
    let graph = build_graph_from_entities(&root, files, &entities);

    let mismatches = find_arity_mismatches(&graph, &entities);

    // Check that bad callers ARE flagged
    let flagged_callers: Vec<&str> = mismatches.iter().map(|m| m.caller_entity.as_str()).collect();

    // bad_caller_too_few calls create_user("alice") with 1 arg, expects 3
    assert!(
        mismatches
            .iter()
            .any(|m| m.caller_entity == "bad_caller_too_few"
                && m.callee_entity == "create_user"
                && m.actual_args == 1),
        "should flag too few args: {:?}",
        flagged_callers
    );

    // bad_caller_too_many calls delete_user(42, "extra") with 2 args, expects 1
    assert!(
        mismatches
            .iter()
            .any(|m| m.caller_entity == "bad_caller_too_many"
                && m.callee_entity == "delete_user"
                && m.actual_args == 2),
        "should flag too many args: {:?}",
        flagged_callers
    );

    // good_caller should NOT be flagged for create_user, delete_user, or find_users
    let good_caller_mismatches: Vec<&ArityMismatch> = mismatches
        .iter()
        .filter(|m| {
            m.caller_entity == "good_caller"
                && matches!(
                    m.callee_entity.as_str(),
                    "create_user" | "delete_user" | "find_users"
                )
        })
        .collect();
    assert!(
        good_caller_mismatches.is_empty(),
        "good_caller should not be flagged: {:?}",
        good_caller_mismatches
            .iter()
            .map(|m| format!("{}->{}({})", m.caller_entity, m.callee_entity, m.actual_args))
            .collect::<Vec<_>>()
    );

    // log_message is variadic, should NOT be flagged
    assert!(
        !mismatches
            .iter()
            .any(|m| m.callee_entity == "log_message"),
        "variadic function should not be flagged"
    );
}

#[test]
fn verify_broken_callers_from_signature_change() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verify_test/python");
    let files = &["functions.py", "callers.py"];

    // Old entities (current state)
    let old_entities = extract_all_entities(&root, files);

    // Simulate a signature change: create_user now takes 4 params instead of 3
    let mut new_entities = old_entities.clone();
    for e in &mut new_entities {
        if e.name == "create_user" {
            e.content = "def create_user(name, email, age, role):\n    pass".to_string();
        }
    }

    let new_graph = build_graph_from_entities(&root, files, &new_entities);
    let broken = find_broken_callers(&old_entities, &new_graph, &new_entities);

    // good_caller calls create_user with 3 args, but new signature expects 4
    assert!(
        broken
            .iter()
            .any(|m| m.caller_entity == "good_caller"
                && m.callee_entity == "create_user"
                && m.actual_args == 3
                && m.expected_min == 4),
        "should detect good_caller as broken after signature change: got {:?}",
        broken
            .iter()
            .map(|m| format!(
                "{}->{}({}/{})",
                m.caller_entity, m.callee_entity, m.actual_args, m.expected_min
            ))
            .collect::<Vec<_>>()
    );
}
