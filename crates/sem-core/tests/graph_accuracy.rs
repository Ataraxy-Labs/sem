use std::path::Path;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;

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

#[test]
fn graph_accuracy_python() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Init git repo (EntityGraph::build requires it)
    std::process::Command::new("git").args(["init"]).current_dir(root).output().unwrap();
    std::process::Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(root).output().unwrap();
    std::process::Command::new("git").args(["config", "user.name", "Test"]).current_dir(root).output().unwrap();

    let files = copy_fixtures(&fixture_dir, root);

    std::process::Command::new("git").args(["add", "."]).current_dir(root).output().unwrap();
    std::process::Command::new("git").args(["commit", "-m", "init"]).current_dir(root).output().unwrap();

    let registry = create_default_registry();
    let file_refs: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    let (graph, _) = EntityGraph::build(root, &file_refs, &registry);

    let expected_edges: Vec<(&str, &str)> = vec![
        ("create_user", "User"),
        ("create_user", "get_connection"),
        ("create_user", "save_record"),
        ("create_admin", "Admin"),
        ("create_admin", "get_connection"),
        ("create_admin", "save_record"),
        ("list_users", "get_connection"),
        ("handle_signup", "create_user"),
        ("handle_admin_create", "create_admin"),
        ("handle_list", "list_users"),
    ];

    let false_positives: Vec<(&str, &str)> = vec![
        ("validate_request", "validate"),
        ("save_record", "create_user"),
        ("delete_record", "create_user"),
    ];

    let mut tp = 0;
    let mut fn_count = 0;
    for (from_pat, to_pat) in &expected_edges {
        let found = graph.edges.iter().any(|e| {
            e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat)
        });
        if found { tp += 1; } else { fn_count += 1; }
    }

    let mut fp = 0;
    for (from_pat, to_pat) in &false_positives {
        if graph.edges.iter().any(|e| {
            e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat)
        }) {
            fp += 1;
        }
    }

    let recall = tp as f64 / (tp + fn_count) as f64;

    eprintln!("Python: {}/{} recall ({:.0}%), {} FPs", tp, expected_edges.len(), recall * 100.0, fp);
    assert!(tp > 0, "Should find at least some expected edges");
}

#[test]
fn graph_accuracy_rust() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    std::process::Command::new("git").args(["init"]).current_dir(root).output().unwrap();
    std::process::Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(root).output().unwrap();
    std::process::Command::new("git").args(["config", "user.name", "Test"]).current_dir(root).output().unwrap();

    let files = copy_fixtures(&fixture_dir, root);

    std::process::Command::new("git").args(["add", "."]).current_dir(root).output().unwrap();
    std::process::Command::new("git").args(["commit", "-m", "init"]).current_dir(root).output().unwrap();

    let registry = create_default_registry();
    let file_refs: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    let (graph, _) = EntityGraph::build(root, &file_refs, &registry);

    let expected_edges: Vec<(&str, &str)> = vec![
        ("Parser::new", "Config"),
        ("Parser::parse", "Entity"),
        ("Parser::parse", "ParseError"),
        ("Parser::parse", "extract_entity"),
        ("extract_entity", "Entity"),
        ("validate_content", "ParseError"),
        ("main", "load_config"),
        ("main", "Parser"),
        ("main", "process_entities"),
        ("load_config", "Config"),
    ];

    let false_positives: Vec<(&str, &str)> = vec![
        ("Config", "Parser"),
        ("Entity", "extract_entity"),
    ];

    let mut tp = 0;
    let mut fn_count = 0;
    for (from_pat, to_pat) in &expected_edges {
        let found = graph.edges.iter().any(|e| {
            e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat)
        });
        if found { tp += 1; } else { fn_count += 1; }
    }

    let mut fp = 0;
    for (from_pat, to_pat) in &false_positives {
        if graph.edges.iter().any(|e| {
            e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat)
        }) {
            fp += 1;
        }
    }

    let recall = tp as f64 / (tp + fn_count) as f64;

    eprintln!("Rust: {}/{} recall ({:.0}%), {} FPs", tp, expected_edges.len(), recall * 100.0, fp);
    assert!(tp > 0, "Should find at least some expected edges");
}
