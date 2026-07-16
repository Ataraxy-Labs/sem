use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;
use std::path::Path;

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

    let files = copy_fixtures(&fixture_dir, root);

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
        let found = graph
            .edges
            .iter()
            .any(|e| e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat));
        if found {
            tp += 1;
        } else {
            fn_count += 1;
        }
    }

    let mut fp = 0;
    for (from_pat, to_pat) in &false_positives {
        if graph
            .edges
            .iter()
            .any(|e| e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat))
        {
            fp += 1;
        }
    }

    let recall = tp as f64 / (tp + fn_count) as f64;

    eprintln!(
        "Python: {}/{} recall ({:.0}%), {} FPs",
        tp,
        expected_edges.len(),
        recall * 100.0,
        fp
    );
    assert!(tp > 0, "Should find at least some expected edges");
}

#[test]
fn graph_accuracy_rust() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

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

    let files = copy_fixtures(&fixture_dir, root);

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

    let false_positives: Vec<(&str, &str)> =
        vec![("Config", "Parser"), ("Entity", "extract_entity")];

    let mut tp = 0;
    let mut fn_count = 0;
    for (from_pat, to_pat) in &expected_edges {
        let found = graph
            .edges
            .iter()
            .any(|e| e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat));
        if found {
            tp += 1;
        } else {
            fn_count += 1;
        }
    }

    let mut fp = 0;
    for (from_pat, to_pat) in &false_positives {
        if graph
            .edges
            .iter()
            .any(|e| e.from_entity.contains(from_pat) && e.to_entity.contains(to_pat))
        {
            fp += 1;
        }
    }

    let recall = tp as f64 / (tp + fn_count) as f64;

    eprintln!(
        "Rust: {}/{} recall ({:.0}%), {} FPs",
        tp,
        expected_edges.len(),
        recall * 100.0,
        fp
    );
    assert!(tp > 0, "Should find at least some expected edges");
}

/// Regression for issue #471: building a graph over multiple Svelte components
/// in parallel triggered a SIGSEGV (invalid free) in the
/// tree-sitter-htmlx-svelte 0.1.8 grammar's scanner on Linux/glibc. Fixed by
/// bumping the grammar to 0.1.16. On macOS the bad free was tolerated by the
/// allocator, so this only fails on Linux/glibc CI; everywhere it at least
/// exercises the parallel Svelte parse path that used to crash.
#[test]
fn graph_svelte_parallel_no_crash_issue_471() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test"],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(root)
            .output()
            .unwrap();
    }

    // Enough components that the parallel (rayon) graph build parses several
    // .svelte files concurrently, which is where the grammar crashed.
    let component = r#"<script lang="ts">
  let count = 0;
  function handleLogoClick() {
    count += 1;
    console.log("clicked", count);
  }
</script>

<button on:click={handleLogoClick}>
  clicked {count} times
</button>

<style>
  button { color: blue; }
</style>
"#;
    let mut files = Vec::new();
    for i in 0..12 {
        let name = format!("Component{i}.svelte");
        std::fs::write(root.join(&name), component).unwrap();
        files.push(name);
    }

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

    let registry = create_default_registry();
    // Reaching past this call at all means no SIGSEGV during parallel parsing.
    let (graph, _) = EntityGraph::build(root, &files, &registry);

    // Sanity: the components were actually parsed, not silently skipped.
    assert!(
        graph.entities.values().any(|e| e.name == "handleLogoClick"),
        "expected handleLogoClick entity from parsed .svelte components"
    );
}
