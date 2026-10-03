use std::collections::HashMap;
use std::path::Path;

use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, RefType};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::scope_resolve;

fn copy_fixtures(fixture_dir: &Path, target_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(fixture_dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        std::fs::copy(entry.path(), target_dir.join(&name)).unwrap();
        files.push(name);
    }
    files.sort();
    files
}

fn init_git(root: &Path) {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(root)
        .output()
        .unwrap();
}

/// Ground truth edges: (from_pattern, to_pattern) where patterns match entity names
/// These represent edges that SHOULD exist in the graph.
fn get_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.py → models.py
        ("create_dog", "Dog", "service calls Dog constructor"),
        ("create_dog", "validate", "service calls dog.validate()"),
        (
            "create_dog",
            "get_connection",
            "service calls get_connection()",
        ),
        ("create_cat", "Cat", "service calls Cat constructor"),
        ("create_cat", "validate", "service calls cat.validate()"),
        (
            "create_cat",
            "get_connection",
            "service calls get_connection()",
        ),
        (
            "transfer_animal",
            "Transaction",
            "service calls Transaction constructor",
        ),
        (
            "transfer_animal",
            "get_connection",
            "service calls get_connection()",
        ),
        ("transfer_animal", "execute", "txn.execute() on Transaction"),
        ("transfer_animal", "commit", "txn.commit() on Transaction"),
        ("transfer_animal", "add", "shelter.add() on Shelter"),
        (
            "list_animals",
            "get_connection",
            "service calls get_connection()",
        ),
        ("list_animals", "execute", "conn.execute() on Connection"),
        // handlers.py → service.py
        ("handle_create_dog", "create_dog", "handler calls service"),
        ("handle_create_cat", "create_cat", "handler calls service"),
        (
            "handle_transfer",
            "transfer_animal",
            "handler calls service",
        ),
        ("handle_transfer", "Shelter", "handler creates Shelter"),
        ("handle_transfer", "Dog", "handler creates Dog"),
        ("handle_transfer", "count", "shelter.count() on Shelter"),
        ("handle_list", "list_animals", "handler calls service"),
        // database.py internal
        (
            "Transaction::execute",
            "execute",
            "Transaction.execute calls self.conn.execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction.commit calls self.conn.commit",
        ),
    ]
}

/// Edges that should NOT exist (false positives the bag-of-words creates)
fn get_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // Dog.validate and Cat.validate are different entities. If create_dog resolves
        // to Cat.validate, that's a false positive.
        ("create_dog", "Cat", "create_dog shouldn't reference Cat"),
        ("create_cat", "Dog", "create_cat shouldn't reference Dog"),
        // validate() in handlers.py is a standalone function, not Dog.validate or Cat.validate
        // bag-of-words might link them
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        // Transaction.execute and Connection.execute are different.
        // transfer_animal calls txn.execute() not conn.execute() directly
        (
            "handle_create_dog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handle_create_cat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

fn edge_matches<S>(
    edges: &[(String, String)],
    entity_map: &HashMap<sem_core::model::entity_id::EntityId, EntityInfo, S>,
    from_pat: &str,
    to_pat: &str,
) -> bool
where
    S: std::hash::BuildHasher,
{
    edges.iter().any(|(from, to)| {
        let from_name = entity_map.get(from).map(|e| e.name.as_str()).unwrap_or("");
        let to_name = entity_map.get(to).map(|e| e.name.as_str()).unwrap_or("");

        // Handle qualified patterns like "Transaction::execute"
        let from_match = if from_pat.contains("::") {
            from.contains(from_pat) || from.contains(&from_pat.replace("::", "::method::"))
        } else {
            from_name == from_pat
        };

        let to_match = if to_pat.contains("::") {
            to.contains(to_pat) || to.contains(&to_pat.replace("::", "::method::"))
        } else {
            to_name == to_pat
        };

        from_match && to_match
    })
}

#[test]
fn swift_overloaded_calls_resolve_by_argument_label() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Example.swift"),
        r#"func load(id: Int) -> String { return "id" }

func load(name: String) -> String { return "name" }

func byId() -> String { return load(id: 1) }

func byName() -> String { return load(name: "x") }

func byAge() -> String { return load(age: 1) }
"#,
    )
    .unwrap();
    init_git(root);

    let registry = create_default_registry();
    let file_refs = vec!["Example.swift".to_string()];
    let (graph, _) = EntityGraph::build(root, &file_refs, &registry);

    let entity_id = |name: &str, start_line: usize| {
        graph
            .entities
            .values()
            .find(|entity| entity.name == name && entity.start_line == start_line)
            .unwrap_or_else(|| panic!("missing entity {name} at line {start_line}"))
            .id
            .clone()
    };

    let load_id = entity_id("load", 1);
    let load_name = entity_id("load", 3);
    let by_id = entity_id("byId", 5);
    let by_name = entity_id("byName", 7);
    let by_age = entity_id("byAge", 9);

    let has_edge = |from: &str, to: &str| {
        graph.edges.iter().any(|edge| {
            edge.from_entity == from && edge.to_entity == to && edge.ref_type == RefType::Calls
        })
    };

    assert!(has_edge(&by_id, &load_id), "byId should call load(id:)");
    assert!(
        !has_edge(&by_id, &load_name),
        "byId should not call load(name:)"
    );
    assert!(
        has_edge(&by_name, &load_name),
        "byName should call load(name:)"
    );
    assert!(
        !has_edge(&by_name, &load_id),
        "byName should not call load(id:)"
    );
    assert!(
        !has_edge(&by_age, &load_id),
        "byAge should not fall back to load(id:)"
    );
    assert!(
        !has_edge(&by_age, &load_name),
        "byAge should not fall back to load(name:)"
    );
}

#[test]
fn swift_overloaded_method_calls_resolve_by_argument_label() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Example.swift"),
        r#"struct Loader {
    func load(id: Int) -> String { return "id" }

    func load(name: String) -> String { return "name" }
}

func byId(loader: Loader) -> String { return loader.load(id: 1) }

func byName(loader: Loader) -> String { return loader.load(name: "x") }

func byAge(loader: Loader) -> String { return loader.load(age: 1) }
"#,
    )
    .unwrap();
    init_git(root);

    let registry = create_default_registry();
    let file_refs = vec!["Example.swift".to_string()];
    let (graph, _) = EntityGraph::build(root, &file_refs, &registry);

    let entity_id = |name: &str, start_line: usize| {
        graph
            .entities
            .values()
            .find(|entity| entity.name == name && entity.start_line == start_line)
            .unwrap_or_else(|| panic!("missing entity {name} at line {start_line}"))
            .id
            .clone()
    };

    let load_id = entity_id("load", 2);
    let load_name = entity_id("load", 4);
    let by_id = entity_id("byId", 7);
    let by_name = entity_id("byName", 9);
    let by_age = entity_id("byAge", 11);

    let has_edge = |from: &str, to: &str| {
        graph.edges.iter().any(|edge| {
            edge.from_entity == from && edge.to_entity == to && edge.ref_type == RefType::Calls
        })
    };

    assert!(has_edge(&by_id, &load_id), "byId should call load(id:)");
    assert!(
        !has_edge(&by_id, &load_name),
        "byId should not call load(name:)"
    );
    assert!(
        has_edge(&by_name, &load_name),
        "byName should call load(name:)"
    );
    assert!(
        !has_edge(&by_name, &load_id),
        "byName should not call load(id:)"
    );
    assert!(
        !has_edge(&by_age, &load_id),
        "byAge should not fall back to load(id:)"
    );
    assert!(
        !has_edge(&by_age, &load_name),
        "byAge should not fall back to load(name:)"
    );
}

#[test]
fn swift_overloaded_initializers_resolve_by_argument_label() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Example.swift"),
        r#"struct Token {
    init(id: Int) {}

    init(text: String) {}

    init(copyID: Int) { self.init(id: copyID) }

    init(copyText: String) { self.init(text: copyText) }

    init(copyAge: Double) { self.init(age: copyAge) }
}
"#,
    )
    .unwrap();
    init_git(root);

    let registry = create_default_registry();
    let file_refs = vec!["Example.swift".to_string()];
    let (graph, _) = EntityGraph::build(root, &file_refs, &registry);

    let entity_id = |start_line: usize| {
        graph
            .entities
            .values()
            .find(|entity| entity.name == "init" && entity.start_line == start_line)
            .unwrap_or_else(|| panic!("missing init at line {start_line}"))
            .id
            .clone()
    };

    let init_id = entity_id(2);
    let init_text = entity_id(4);
    let copy_id = entity_id(6);
    let copy_text = entity_id(8);
    let copy_age = entity_id(10);

    let has_edge = |from: &str, to: &str| {
        graph.edges.iter().any(|edge| {
            edge.from_entity == from && edge.to_entity == to && edge.ref_type == RefType::Calls
        })
    };

    assert!(has_edge(&copy_id, &init_id), "copyID should call init(id:)");
    assert!(
        !has_edge(&copy_id, &init_text),
        "copyID should not call init(text:)"
    );
    assert!(
        has_edge(&copy_text, &init_text),
        "copyText should call init(text:)"
    );
    assert!(
        !has_edge(&copy_text, &init_id),
        "copyText should not call init(id:)"
    );
    assert!(
        !has_edge(&copy_age, &init_id),
        "copyAge should not fall back to init(id:)"
    );
    assert!(
        !has_edge(&copy_age, &init_text),
        "copyAge should not fall back to init(text:)"
    );
}

// --- Per-language scope resolution tests ---

fn get_ts_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.ts -> models.ts
        ("createDog", "Dog", "service calls Dog constructor"),
        ("createDog", "validate", "service calls dog.validate()"),
        (
            "createDog",
            "getConnection",
            "service calls getConnection()",
        ),
        ("createCat", "Cat", "service calls Cat constructor"),
        ("createCat", "validate", "service calls cat.validate()"),
        (
            "createCat",
            "getConnection",
            "service calls getConnection()",
        ),
        (
            "transferAnimal",
            "Transaction",
            "service calls Transaction constructor",
        ),
        (
            "transferAnimal",
            "getConnection",
            "service calls getConnection()",
        ),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter.add() on Shelter"),
        (
            "listAnimals",
            "getConnection",
            "service calls getConnection()",
        ),
        ("listAnimals", "execute", "conn.execute() on Connection"),
        // handlers.ts -> service.ts
        ("handleCreateDog", "createDog", "handler calls service"),
        ("handleCreateCat", "createCat", "handler calls service"),
        ("handleTransfer", "transferAnimal", "handler calls service"),
        ("handleTransfer", "Shelter", "handler creates Shelter"),
        ("handleTransfer", "Dog", "handler creates Dog"),
        ("handleTransfer", "count", "shelter.count() on Shelter"),
        ("handleList", "listAnimals", "handler calls service"),
        // database.ts internal
        (
            "Transaction::execute",
            "execute",
            "Transaction.execute calls this.conn.execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction.commit calls this.conn.commit",
        ),
    ]
}

fn get_ts_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        (
            "handleCreateDog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handleCreateCat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

fn get_rust_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.rs -> models.rs
        ("create_dog", "Dog", "service calls Dog::new"),
        ("create_dog", "validate", "service calls dog.validate()"),
        (
            "create_dog",
            "get_connection",
            "service calls get_connection()",
        ),
        ("create_cat", "Cat", "service calls Cat::new"),
        ("create_cat", "validate", "service calls cat.validate()"),
        (
            "create_cat",
            "get_connection",
            "service calls get_connection()",
        ),
        (
            "transfer_animal",
            "Transaction",
            "service calls Transaction::new",
        ),
        (
            "transfer_animal",
            "get_connection",
            "service calls get_connection()",
        ),
        ("transfer_animal", "execute", "txn.execute() on Transaction"),
        ("transfer_animal", "commit", "txn.commit() on Transaction"),
        ("transfer_animal", "add", "shelter.add() on Shelter"),
        (
            "list_animals",
            "get_connection",
            "service calls get_connection()",
        ),
        ("list_animals", "execute", "conn.execute() on Connection"),
        // handlers.rs -> service.rs
        ("handle_create_dog", "create_dog", "handler calls service"),
        ("handle_create_cat", "create_cat", "handler calls service"),
        (
            "handle_transfer",
            "transfer_animal",
            "handler calls service",
        ),
        ("handle_transfer", "Shelter", "handler creates Shelter"),
        ("handle_transfer", "Dog", "handler creates Dog"),
        ("handle_transfer", "count", "shelter.count() on Shelter"),
        ("handle_list", "list_animals", "handler calls service"),
        // database.rs internal
        (
            "Transaction::execute",
            "execute",
            "Transaction.execute calls self.conn.execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction.commit calls self.conn.commit",
        ),
    ]
}

fn get_rust_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("create_dog", "Cat", "create_dog shouldn't reference Cat"),
        ("create_cat", "Dog", "create_cat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        (
            "handle_create_dog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handle_create_cat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

fn get_go_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.go -> models.go
        ("CreateDog", "NewDog", "service calls NewDog()"),
        ("CreateDog", "Validate", "service calls dog.Validate()"),
        (
            "CreateDog",
            "GetConnection",
            "service calls GetConnection()",
        ),
        ("CreateCat", "NewCat", "service calls NewCat()"),
        ("CreateCat", "Validate", "service calls cat.Validate()"),
        (
            "CreateCat",
            "GetConnection",
            "service calls GetConnection()",
        ),
        (
            "TransferAnimal",
            "NewTransaction",
            "service calls NewTransaction()",
        ),
        (
            "TransferAnimal",
            "GetConnection",
            "service calls GetConnection()",
        ),
        ("TransferAnimal", "Execute", "txn.Execute() on Transaction"),
        ("TransferAnimal", "Commit", "txn.Commit() on Transaction"),
        ("TransferAnimal", "Add", "shelter.Add() on Shelter"),
        (
            "ListAnimals",
            "GetConnection",
            "service calls GetConnection()",
        ),
        ("ListAnimals", "Execute", "conn.Execute() on Connection"),
        // handlers.go -> service.go
        ("HandleCreateDog", "CreateDog", "handler calls service"),
        ("HandleCreateCat", "CreateCat", "handler calls service"),
        ("HandleTransfer", "TransferAnimal", "handler calls service"),
        ("HandleTransfer", "NewShelter", "handler creates Shelter"),
        ("HandleTransfer", "NewDog", "handler creates Dog"),
        ("HandleTransfer", "Count", "shelter.Count() on Shelter"),
        ("HandleList", "ListAnimals", "handler calls service"),
        // database.go internal
        (
            "Transaction::Execute",
            "Execute",
            "Transaction.Execute calls Conn.Execute",
        ),
        (
            "Transaction::Commit",
            "Commit",
            "Transaction.Commit calls Conn.Commit",
        ),
    ]
}

fn get_go_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("CreateDog", "Cat", "CreateDog shouldn't reference Cat"),
        ("CreateCat", "Dog", "CreateCat shouldn't reference Dog"),
        ("Validate", "Dog", "standalone Validate != Dog.Validate"),
        ("Validate", "Cat", "standalone Validate != Cat.Validate"),
        (
            "HandleCreateDog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "HandleCreateCat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

/// Run scope resolve comparison for a given language fixture set
fn run_scope_resolve_for_lang(
    lang_name: &str,
    expected: &[(&str, &str, &str)],
    false_positives: &[(&str, &str, &str)],
) {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/scope_test/{}", lang_name));
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let files = copy_fixtures(&fixture_dir, root);
    init_git(root);

    let registry = create_default_registry();
    let file_refs: Vec<String> = files.iter().map(|f| f.to_string()).collect();

    // Run old resolver (bag-of-words)
    let (old_graph, _) = EntityGraph::build(root, &file_refs, &registry);
    let old_edges: Vec<(String, String)> = old_graph
        .edges
        .iter()
        .map(|e| (e.from_entity.to_string(), e.to_entity.to_string()))
        .collect();

    // Run new scope resolver
    let all_entities: Vec<SemanticEntity> = file_refs
        .iter()
        .filter_map(|file_path| {
            let full_path = root.join(file_path);
            let content = std::fs::read_to_string(&full_path).ok()?;
            let plugin = registry.get_plugin_with_content(file_path, &content)?;
            Some(plugin.extract_entities(&content, file_path))
        })
        .flatten()
        .collect();

    let entity_map: HashMap<sem_core::model::entity_id::EntityId, EntityInfo> = all_entities
        .iter()
        .map(|e| {
            (
                e.id.clone().into(),
                EntityInfo {
                    id: (e.id.clone()).into(),
                    name: e.name.clone(),
                    entity_type: e.entity_type.clone(),
                    file_path: e.file_path.clone(),
                    parent_id: e.parent_id.as_ref().map(Into::into),
                    start_line: e.start_line,
                    end_line: e.end_line,
                },
            )
        })
        .collect();

    let scope_result =
        scope_resolve::resolve_with_scopes(root, &file_refs, &all_entities, &entity_map, None);
    let new_edges: Vec<(String, String)> = scope_result
        .edges
        .iter()
        .map(|(from, to, _)| (from.clone(), to.clone()))
        .collect();

    // Debug: dump edges for languages that need config tuning
    if new_edges.is_empty() && !old_edges.is_empty() {
        eprintln!(
            "  DEBUG [{}]: old_edges={}, new_edges=0, entities={}",
            lang_name,
            old_edges.len(),
            all_entities.len()
        );
    }

    // Score
    let mut old_tp = 0;
    let mut old_fn = 0;
    let mut old_fp = 0;
    let mut new_tp = 0;
    let mut new_fn = 0;
    let mut new_fp = 0;

    for (from_pat, to_pat, desc) in expected {
        let old_found = edge_matches(&old_edges, &entity_map, from_pat, to_pat);
        let new_found = edge_matches(&new_edges, &entity_map, from_pat, to_pat);
        if old_found {
            old_tp += 1;
        } else {
            old_fn += 1;
        }
        if new_found {
            new_tp += 1;
        } else {
            new_fn += 1;
            eprintln!(
                "  MISSED [{}]: {} -> {} ({})",
                lang_name, from_pat, to_pat, desc
            );
        }
    }

    for (from_pat, to_pat, desc) in false_positives {
        let old_found = edge_matches(&old_edges, &entity_map, from_pat, to_pat);
        let new_found = edge_matches(&new_edges, &entity_map, from_pat, to_pat);
        if old_found {
            old_fp += 1;
        }
        if new_found {
            new_fp += 1;
            eprintln!(
                "  FALSE POSITIVE [{}]: {} -> {} ({})",
                lang_name, from_pat, to_pat, desc
            );
        }
    }

    let old_recall = if old_tp + old_fn > 0 {
        old_tp as f64 / (old_tp + old_fn) as f64
    } else {
        0.0
    };
    let new_recall = if new_tp + new_fn > 0 {
        new_tp as f64 / (new_tp + new_fn) as f64
    } else {
        0.0
    };

    eprintln!("\n=== {} Scope Resolution ===", lang_name);
    eprintln!(
        "Old (bag-of-words): {} TP, {} FN, {} FP | {:.0}% recall",
        old_tp,
        old_fn,
        old_fp,
        old_recall * 100.0
    );
    eprintln!(
        "New (scope-aware):  {} TP, {} FN, {} FP | {:.0}% recall",
        new_tp,
        new_fn,
        new_fp,
        new_recall * 100.0
    );
    eprintln!(
        "Total edges: old={}, new={}",
        old_edges.len(),
        new_edges.len()
    );

    // Report results - the scope resolver should be producing edges for supported languages
    if new_edges.is_empty() && !old_edges.is_empty() {
        eprintln!(
            "  NOTE [{}]: Scope resolver produced 0 edges (config may need tuning)",
            lang_name
        );
    }

    // Languages with complete expected coverage gate CI on the scored result.
    if lang_name == "swift" {
        assert_eq!(
            new_fn, 0,
            "{} scope resolver missed {} expected edges; see MISSED lines above",
            lang_name, new_fn
        );
        assert_eq!(
            new_fp, 0,
            "{} scope resolver produced {} forbidden edges; see FALSE POSITIVE lines above",
            lang_name, new_fp
        );
    }
}

#[test]
fn scope_resolve_typescript() {
    let expected = get_ts_expected_edges();
    let fp = get_ts_false_positive_edges();
    run_scope_resolve_for_lang("typescript", &expected, &fp);
}

#[test]
fn scope_resolve_rust() {
    let expected = get_rust_expected_edges();
    let fp = get_rust_false_positive_edges();
    run_scope_resolve_for_lang("rust", &expected, &fp);
}

#[test]
fn scope_resolve_go() {
    let expected = get_go_expected_edges();
    let fp = get_go_false_positive_edges();
    run_scope_resolve_for_lang("go", &expected, &fp);
}

/// Integration test: verify EntityGraph::build() uses scope resolver for Python
/// and produces the same high-quality edges as the standalone scope resolver.
#[test]
fn scope_resolve_integrated_graph() {
    let fixture_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scope_test/python");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let files = copy_fixtures(&fixture_dir, root);
    init_git(root);

    let registry = create_default_registry();
    let file_refs: Vec<String> = files.iter().map(|f| f.to_string()).collect();

    // Build the integrated graph (should use scope resolver for Python)
    let (graph, _) = EntityGraph::build(root, &file_refs, &registry);

    let graph_edges: Vec<(String, String)> = graph
        .edges
        .iter()
        .map(|e| (e.from_entity.to_string(), e.to_entity.to_string()))
        .collect();

    let entity_map = &graph.entities;

    // Verify expected edges are found
    let expected = get_expected_edges();
    let mut tp = 0;
    let mut missed = Vec::new();

    for (from_pat, to_pat, desc) in &expected {
        if edge_matches(&graph_edges, entity_map, from_pat, to_pat) {
            tp += 1;
        } else {
            missed.push(*desc);
        }
    }

    let recall = tp as f64 / expected.len() as f64;
    eprintln!("\n=== Integrated Graph (scope resolver) ===");
    eprintln!(
        "{} / {} expected edges found ({:.0}% recall)",
        tp,
        expected.len(),
        recall * 100.0
    );
    if !missed.is_empty() {
        eprintln!("Missed: {:?}", missed);
    }

    // Verify false positives are eliminated
    let false_positives = get_false_positive_edges();
    let mut fp = 0;
    for (from_pat, to_pat, desc) in &false_positives {
        if edge_matches(&graph_edges, entity_map, from_pat, to_pat) {
            eprintln!("FALSE POSITIVE: {}", desc);
            fp += 1;
        }
    }

    // The integrated graph should achieve at least 90% recall (scope resolver gets 95%)
    assert!(
        recall >= 0.90,
        "Integrated graph recall {:.0}% should be >= 90%. Missed: {:?}",
        recall * 100.0,
        missed
    );

    // Should have zero false positives from the known FP set
    assert_eq!(
        fp, 0,
        "Integrated graph should have 0 known false positives, got {}",
        fp
    );
}

// ═══════════════════════════════════════════════════════════════════
// New language benchmarks (Java, C#, C++, Ruby)
// ═══════════════════════════════════════════════════════════════════

fn get_java_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // Service.java -> Models.java
        ("createDog", "Dog", "service creates Dog"),
        ("createDog", "validate", "service calls dog.validate()"),
        (
            "createDog",
            "getConnection",
            "service calls DatabaseHelper.getConnection()",
        ),
        ("createCat", "Cat", "service creates Cat"),
        ("createCat", "validate", "service calls cat.validate()"),
        (
            "createCat",
            "getConnection",
            "service calls getConnection()",
        ),
        (
            "transferAnimal",
            "Transaction",
            "service creates Transaction",
        ),
        (
            "transferAnimal",
            "getConnection",
            "service calls getConnection()",
        ),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter.add() on Shelter"),
        (
            "listAnimals",
            "getConnection",
            "service calls getConnection()",
        ),
        ("listAnimals", "execute", "conn.execute() on Connection"),
        // Handlers.java -> Service.java
        ("handleCreateDog", "createDog", "handler calls service"),
        ("handleCreateCat", "createCat", "handler calls service"),
        ("handleTransfer", "transferAnimal", "handler calls service"),
        ("handleTransfer", "Shelter", "handler creates Shelter"),
        ("handleTransfer", "Dog", "handler creates Dog"),
        ("handleTransfer", "count", "shelter.count() on Shelter"),
        ("handleList", "listAnimals", "handler calls service"),
        // Database.java internal
        (
            "Transaction::execute",
            "execute",
            "Transaction.execute calls conn.execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction.commit calls conn.commit",
        ),
    ]
}

fn get_java_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        (
            "handleCreateDog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handleCreateCat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

#[test]
fn scope_resolve_java() {
    let expected = get_java_expected_edges();
    let fp = get_java_false_positive_edges();
    run_scope_resolve_for_lang("java", &expected, &fp);
}

fn get_csharp_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // Service.cs -> Models.cs
        ("CreateDog", "Dog", "service creates Dog"),
        ("CreateDog", "Validate", "service calls dog.Validate()"),
        (
            "CreateDog",
            "GetConnection",
            "service calls DatabaseHelper.GetConnection()",
        ),
        ("CreateCat", "Cat", "service creates Cat"),
        ("CreateCat", "Validate", "service calls cat.Validate()"),
        (
            "CreateCat",
            "GetConnection",
            "service calls GetConnection()",
        ),
        (
            "TransferAnimal",
            "Transaction",
            "service creates Transaction",
        ),
        (
            "TransferAnimal",
            "GetConnection",
            "service calls GetConnection()",
        ),
        ("TransferAnimal", "Execute", "txn.Execute() on Transaction"),
        ("TransferAnimal", "Commit", "txn.Commit() on Transaction"),
        ("TransferAnimal", "Add", "shelter.Add() on Shelter"),
        (
            "ListAnimals",
            "GetConnection",
            "service calls GetConnection()",
        ),
        ("ListAnimals", "Execute", "conn.Execute() on Connection"),
        // Handlers.cs -> Service.cs
        ("HandleCreateDog", "CreateDog", "handler calls service"),
        ("HandleCreateCat", "CreateCat", "handler calls service"),
        ("HandleTransfer", "TransferAnimal", "handler calls service"),
        ("HandleTransfer", "Shelter", "handler creates Shelter"),
        ("HandleTransfer", "Dog", "handler creates Dog"),
        ("HandleTransfer", "Count", "shelter.Count() on Shelter"),
        ("HandleList", "ListAnimals", "handler calls service"),
        // Database.cs internal
        (
            "Transaction::Execute",
            "Execute",
            "Transaction.Execute calls conn.Execute",
        ),
        (
            "Transaction::Commit",
            "Commit",
            "Transaction.Commit calls conn.Commit",
        ),
    ]
}

fn get_csharp_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("CreateDog", "Cat", "CreateDog shouldn't reference Cat"),
        ("CreateCat", "Dog", "CreateCat shouldn't reference Dog"),
        ("Validate", "Dog", "standalone Validate != Dog.Validate"),
        ("Validate", "Cat", "standalone Validate != Cat.Validate"),
        (
            "HandleCreateDog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "HandleCreateCat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

#[test]
fn scope_resolve_csharp() {
    let expected = get_csharp_expected_edges();
    let fp = get_csharp_false_positive_edges();
    run_scope_resolve_for_lang("csharp", &expected, &fp);
}

fn get_cpp_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.cpp -> models.hpp
        ("createDog", "Dog", "service creates Dog"),
        ("createDog", "validate", "service calls dog.validate()"),
        (
            "createDog",
            "getConnection",
            "service calls getConnection()",
        ),
        ("createCat", "Cat", "service creates Cat"),
        ("createCat", "validate", "service calls cat.validate()"),
        (
            "createCat",
            "getConnection",
            "service calls getConnection()",
        ),
        (
            "transferAnimal",
            "Transaction",
            "service creates Transaction",
        ),
        (
            "transferAnimal",
            "getConnection",
            "service calls getConnection()",
        ),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter->add() on Shelter"),
        (
            "listAnimals",
            "getConnection",
            "service calls getConnection()",
        ),
        ("listAnimals", "execute", "conn->execute() on Connection"),
        // handlers.cpp -> service.cpp
        ("handleCreateDog", "createDog", "handler calls service"),
        ("handleCreateCat", "createCat", "handler calls service"),
        ("handleTransfer", "transferAnimal", "handler calls service"),
        ("handleTransfer", "Shelter", "handler creates Shelter"),
        ("handleTransfer", "Dog", "handler creates Dog"),
        ("handleTransfer", "count", "shelter.count() on Shelter"),
        ("handleList", "listAnimals", "handler calls service"),
        // database.hpp internal
        (
            "Transaction::execute",
            "execute",
            "Transaction.execute calls conn->execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction.commit calls conn->commit",
        ),
    ]
}

fn get_cpp_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        (
            "handleCreateDog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handleCreateCat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

#[test]
fn scope_resolve_cpp() {
    let expected = get_cpp_expected_edges();
    let fp = get_cpp_false_positive_edges();
    run_scope_resolve_for_lang("cpp", &expected, &fp);
}

fn get_ruby_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.rb -> models.rb
        ("create_dog", "Dog", "service creates Dog"),
        ("create_dog", "validate", "service calls dog.validate"),
        (
            "create_dog",
            "get_connection",
            "service calls get_connection",
        ),
        ("create_cat", "Cat", "service creates Cat"),
        ("create_cat", "validate", "service calls cat.validate"),
        (
            "create_cat",
            "get_connection",
            "service calls get_connection",
        ),
        (
            "transfer_animal",
            "Transaction",
            "service creates Transaction",
        ),
        (
            "transfer_animal",
            "get_connection",
            "service calls get_connection",
        ),
        ("transfer_animal", "execute", "txn.execute on Transaction"),
        ("transfer_animal", "commit", "txn.commit on Transaction"),
        ("transfer_animal", "add", "shelter.add on Shelter"),
        (
            "list_animals",
            "get_connection",
            "service calls get_connection",
        ),
        ("list_animals", "execute", "conn.execute on Connection"),
        // handlers.rb -> service.rb
        ("handle_create_dog", "create_dog", "handler calls service"),
        ("handle_create_cat", "create_cat", "handler calls service"),
        (
            "handle_transfer",
            "transfer_animal",
            "handler calls service",
        ),
        ("handle_transfer", "Shelter", "handler creates Shelter"),
        ("handle_transfer", "Dog", "handler creates Dog"),
        ("handle_transfer", "count", "shelter.count on Shelter"),
        ("handle_list", "list_animals", "handler calls service"),
        // database.rb internal
        (
            "Transaction::execute",
            "execute",
            "Transaction#execute calls @conn.execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction#commit calls @conn.commit",
        ),
    ]
}

fn get_ruby_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("create_dog", "Cat", "create_dog shouldn't reference Cat"),
        ("create_cat", "Dog", "create_cat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog#validate"),
        ("validate", "Cat", "standalone validate != Cat#validate"),
        (
            "handle_create_dog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handle_create_cat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

#[test]
fn scope_resolve_ruby() {
    let expected = get_ruby_expected_edges();
    let fp = get_ruby_false_positive_edges();
    run_scope_resolve_for_lang("ruby", &expected, &fp);
}

// ═══════════════════════════════════════════════════════════════════
// Swift and Kotlin benchmarks
// ═══════════════════════════════════════════════════════════════════

fn get_swift_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.swift -> models.swift
        ("createDog", "Dog", "service creates Dog"),
        ("createDog", "validate", "service calls dog.validate()"),
        (
            "createDog",
            "getConnection",
            "service calls getConnection()",
        ),
        ("createCat", "Cat", "service creates Cat"),
        ("createCat", "validate", "service calls cat.validate()"),
        (
            "createCat",
            "getConnection",
            "service calls getConnection()",
        ),
        (
            "transferAnimal",
            "Transaction",
            "service creates Transaction",
        ),
        (
            "transferAnimal",
            "getConnection",
            "service calls getConnection()",
        ),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter.add() on Shelter"),
        (
            "listAnimals",
            "getConnection",
            "service calls getConnection()",
        ),
        ("listAnimals", "execute", "conn.execute() on Connection"),
        // handlers.swift -> service.swift
        ("handleCreateDog", "createDog", "handler calls service"),
        ("handleCreateCat", "createCat", "handler calls service"),
        ("handleTransfer", "transferAnimal", "handler calls service"),
        ("handleTransfer", "Shelter", "handler creates Shelter"),
        ("handleTransfer", "Dog", "handler creates Dog"),
        ("handleTransfer", "count", "shelter.count() on Shelter"),
        ("handleList", "listAnimals", "handler calls service"),
        // database.swift internal
        (
            "Transaction::execute",
            "execute",
            "Transaction.execute calls conn.execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction.commit calls conn.commit",
        ),
        (
            "Replicator::sync",
            "execute",
            "Replicator.sync calls primary.execute",
        ),
        (
            "Replicator::sync",
            "commit",
            "Replicator.sync calls backup.commit",
        ),
        (
            "AuditedTransaction::write",
            "commit",
            "AuditedTransaction.write calls conn.commit",
        ),
        (
            "AuditedTransaction::write",
            "record",
            "AuditedTransaction.write calls logger.record",
        ),
    ]
}

fn get_swift_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        (
            "handleCreateDog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handleCreateCat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

#[test]
fn scope_resolve_swift() {
    let expected = get_swift_expected_edges();
    let fp = get_swift_false_positive_edges();
    run_scope_resolve_for_lang("swift", &expected, &fp);
}

fn get_kotlin_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // Service.kt -> Models.kt
        ("createDog", "Dog", "service creates Dog"),
        ("createDog", "validate", "service calls dog.validate()"),
        (
            "createDog",
            "getConnection",
            "service calls getConnection()",
        ),
        ("createCat", "Cat", "service creates Cat"),
        ("createCat", "validate", "service calls cat.validate()"),
        (
            "createCat",
            "getConnection",
            "service calls getConnection()",
        ),
        (
            "transferAnimal",
            "Transaction",
            "service creates Transaction",
        ),
        (
            "transferAnimal",
            "getConnection",
            "service calls getConnection()",
        ),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter.add() on Shelter"),
        (
            "listAnimals",
            "getConnection",
            "service calls getConnection()",
        ),
        ("listAnimals", "execute", "conn.execute() on Connection"),
        // Handlers.kt -> Service.kt
        ("handleCreateDog", "createDog", "handler calls service"),
        ("handleCreateCat", "createCat", "handler calls service"),
        ("handleTransfer", "transferAnimal", "handler calls service"),
        ("handleTransfer", "Shelter", "handler creates Shelter"),
        ("handleTransfer", "Dog", "handler creates Dog"),
        ("handleTransfer", "count", "shelter.count() on Shelter"),
        ("handleList", "listAnimals", "handler calls service"),
        // Chained field access: registry.shelter (Registry.shelter: Shelter) then shelter.add()
        (
            "handleRegistry",
            "add",
            "shelter from registry.shelter, then add() on Shelter",
        ),
        // Database.kt internal
        (
            "Transaction::execute",
            "execute",
            "Transaction.execute calls conn.execute",
        ),
        (
            "Transaction::commit",
            "commit",
            "Transaction.commit calls conn.commit",
        ),
    ]
}

fn get_kotlin_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        (
            "handleCreateDog",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
        (
            "handleCreateCat",
            "Transaction",
            "handler doesn't use Transaction directly",
        ),
    ]
}

#[test]
fn scope_resolve_kotlin() {
    let expected = get_kotlin_expected_edges();
    let fp = get_kotlin_false_positive_edges();
    run_scope_resolve_for_lang("kotlin", &expected, &fp);
}

#[test]
fn rust_super_module_qualified_call_resolves() {
    // Regression: `super::module::func()` calls were dropped because the
    // resolver kept the `super::` prefix in the path and never matched the
    // real module, so impact under-reported the blast radius.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("commands")).unwrap();
    std::fs::write(
        root.join("commands/graph.rs"),
        "pub fn build_graph() -> i32 {\n    42\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("commands/context.rs"),
        "pub fn context_command() -> i32 {\n    super::graph::build_graph()\n}\n",
    )
    .unwrap();
    init_git(root);

    let registry = create_default_registry();
    let files = vec![
        "commands/graph.rs".to_string(),
        "commands/context.rs".to_string(),
    ];
    let (graph, _) = EntityGraph::build(root, &files, &registry);

    let id = |name: &str| {
        graph
            .entities
            .values()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("missing entity {name}"))
            .id
            .clone()
    };
    let caller = id("context_command");
    let callee = id("build_graph");

    assert!(
        graph.edges.iter().any(|e| {
            e.from_entity == caller && e.to_entity == callee && e.ref_type == RefType::Calls
        }),
        "context_command should call super::graph::build_graph; edges: {:?}",
        graph.edges
    );
}
