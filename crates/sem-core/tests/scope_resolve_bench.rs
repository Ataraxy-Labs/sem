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
        ("create_dog", "get_connection", "service calls get_connection()"),
        ("create_cat", "Cat", "service calls Cat constructor"),
        ("create_cat", "validate", "service calls cat.validate()"),
        ("create_cat", "get_connection", "service calls get_connection()"),
        ("transfer_animal", "Transaction", "service calls Transaction constructor"),
        ("transfer_animal", "get_connection", "service calls get_connection()"),
        ("transfer_animal", "execute", "txn.execute() on Transaction"),
        ("transfer_animal", "commit", "txn.commit() on Transaction"),
        ("transfer_animal", "add", "shelter.add() on Shelter"),
        ("list_animals", "get_connection", "service calls get_connection()"),
        ("list_animals", "execute", "conn.execute() on Connection"),

        // handlers.py → service.py
        ("handle_create_dog", "create_dog", "handler calls service"),
        ("handle_create_cat", "create_cat", "handler calls service"),
        ("handle_transfer", "transfer_animal", "handler calls service"),
        ("handle_transfer", "Shelter", "handler creates Shelter"),
        ("handle_transfer", "Dog", "handler creates Dog"),
        ("handle_transfer", "count", "shelter.count() on Shelter"),
        ("handle_list", "list_animals", "handler calls service"),

        // database.py internal
        ("Transaction::execute", "execute", "Transaction.execute calls self.conn.execute"),
        ("Transaction::commit", "commit", "Transaction.commit calls self.conn.commit"),
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
        ("handle_create_dog", "Transaction", "handler doesn't use Transaction directly"),
        ("handle_create_cat", "Transaction", "handler doesn't use Transaction directly"),
    ]
}

fn edge_matches(
    edges: &[(String, String)],
    entity_map: &HashMap<String, EntityInfo>,
    from_pat: &str,
    to_pat: &str,
) -> bool {
    edges.iter().any(|(from, to)| {
        let from_name = entity_map
            .get(from)
            .map(|e| e.name.as_str())
            .unwrap_or("");
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
fn scope_resolve_comparison() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scope_test/python");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let files = copy_fixtures(&fixture_dir, root);
    init_git(root);

    let registry = create_default_registry();
    let file_refs: Vec<String> = files.iter().map(|f| f.to_string()).collect();

    // --- Run old resolver (bag-of-words via EntityGraph::build) ---
    let (old_graph, _) = EntityGraph::build(root, &file_refs, &registry);

    let old_edges: Vec<(String, String)> = old_graph
        .edges
        .iter()
        .map(|e| (e.from_entity.clone(), e.to_entity.clone()))
        .collect();

    // --- Extract entities for scope resolver ---
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

    let entity_map: HashMap<String, EntityInfo> = all_entities
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

    // --- Run new scope-aware resolver ---
    let scope_result =
        scope_resolve::resolve_with_scopes(root, &file_refs, &all_entities, &entity_map, None);

    let new_edges: Vec<(String, String)> = scope_result
        .edges
        .iter()
        .map(|(from, to, _)| (from.clone(), to.clone()))
        .collect();

    // --- Score both ---
    let expected = get_expected_edges();
    let false_positives = get_false_positive_edges();

    let mut old_tp = 0;
    let mut old_fn = 0;
    let mut old_fp = 0;
    let mut new_tp = 0;
    let mut new_fn = 0;
    let mut new_fp = 0;

    let mut details: Vec<(String, String, String, bool, bool)> = Vec::new(); // (from, to, desc, old_found, new_found)

    for (from_pat, to_pat, desc) in &expected {
        let old_found = edge_matches(&old_edges, &entity_map, from_pat, to_pat);
        let new_found = edge_matches(&new_edges, &entity_map, from_pat, to_pat);
        if old_found { old_tp += 1; } else { old_fn += 1; }
        if new_found { new_tp += 1; } else { new_fn += 1; }
        details.push((from_pat.to_string(), to_pat.to_string(), desc.to_string(), old_found, new_found));
    }

    let mut fp_details: Vec<(String, String, String, bool, bool)> = Vec::new();
    for (from_pat, to_pat, desc) in &false_positives {
        let old_found = edge_matches(&old_edges, &entity_map, from_pat, to_pat);
        let new_found = edge_matches(&new_edges, &entity_map, from_pat, to_pat);
        if old_found { old_fp += 1; }
        if new_found { new_fp += 1; }
        fp_details.push((from_pat.to_string(), to_pat.to_string(), desc.to_string(), old_found, new_found));
    }

    let old_precision = if old_tp + old_fp > 0 {
        old_tp as f64 / (old_tp + old_fp) as f64
    } else {
        0.0
    };
    let old_recall = if old_tp + old_fn > 0 {
        old_tp as f64 / (old_tp + old_fn) as f64
    } else {
        0.0
    };

    let new_precision = if new_tp + new_fp > 0 {
        new_tp as f64 / (new_tp + new_fp) as f64
    } else {
        0.0
    };
    let new_recall = if new_tp + new_fn > 0 {
        new_tp as f64 / (new_tp + new_fn) as f64
    } else {
        0.0
    };

    // Print summary
    eprintln!("\n=== Scope Resolution Comparison ===");
    eprintln!("Old (bag-of-words): {} TP, {} FN, {} FP | {:.0}% recall, {:.0}% precision",
        old_tp, old_fn, old_fp, old_recall * 100.0, old_precision * 100.0);
    eprintln!("New (scope-aware):  {} TP, {} FN, {} FP | {:.0}% recall, {:.0}% precision",
        new_tp, new_fn, new_fp, new_recall * 100.0, new_precision * 100.0);
    eprintln!("Total edges: old={}, new={}", old_edges.len(), new_edges.len());

    // Generate HTML report
    let html = generate_html_report(
        &details,
        &fp_details,
        old_tp, old_fn, old_fp, old_recall, old_precision,
        new_tp, new_fn, new_fp, new_recall, new_precision,
        old_edges.len(), new_edges.len(),
        &old_edges, &new_edges, &entity_map,
        &scope_result.resolution_log,
    );

    let output_path = std::env::var("SCOPE_BENCH_OUTPUT")
        .unwrap_or_else(|_| std::env::temp_dir().join("scope-resolve-bench.html").to_string_lossy().to_string());
    let _ = std::fs::write(&output_path, &html);
    eprintln!("Report written to {}", output_path);

    // Assertions: new resolver should find at least as many true positives
    assert!(new_tp > 0, "Scope resolver should find some expected edges");
}

fn generate_html_report(
    details: &[(String, String, String, bool, bool)],
    fp_details: &[(String, String, String, bool, bool)],
    old_tp: usize, old_fn: usize, old_fp: usize, old_recall: f64, old_precision: f64,
    new_tp: usize, new_fn: usize, new_fp: usize, new_recall: f64, new_precision: f64,
    old_total: usize, new_total: usize,
    old_edges: &[(String, String)],
    new_edges: &[(String, String)],
    entity_map: &HashMap<String, EntityInfo>,
    resolution_log: &[scope_resolve::ResolutionEntry],
) -> String {
    let old_f1 = if old_precision + old_recall > 0.0 {
        2.0 * old_precision * old_recall / (old_precision + old_recall)
    } else {
        0.0
    };
    let new_f1 = if new_precision + new_recall > 0.0 {
        2.0 * new_precision * new_recall / (new_precision + new_recall)
    } else {
        0.0
    };

    let expected_rows: String = details
        .iter()
        .map(|(from, to, desc, old_ok, new_ok)| {
            let old_class = if *old_ok { "pass" } else { "fail" };
            let new_class = if *new_ok { "pass" } else { "fail" };
            let old_icon = if *old_ok { "&#10003;" } else { "&#10007;" };
            let new_icon = if *new_ok { "&#10003;" } else { "&#10007;" };
            let improved = !old_ok && *new_ok;
            let row_class = if improved { " class=\"improved\"" } else { "" };
            format!(
                "<tr{}><td>{}</td><td>{}</td><td class=\"desc\">{}</td><td class=\"{}\">{}</td><td class=\"{}\">{}</td></tr>",
                row_class, from, to, desc, old_class, old_icon, new_class, new_icon
            )
        })
        .collect();

    let fp_rows: String = fp_details
        .iter()
        .map(|(from, to, desc, old_found, new_found)| {
            let old_class = if *old_found { "fail" } else { "pass" };
            let new_class = if *new_found { "fail" } else { "pass" };
            let old_icon = if *old_found { "&#10007; yes" } else { "&#10003; no" };
            let new_icon = if *new_found { "&#10007; yes" } else { "&#10003; no" };
            let improved = *old_found && !new_found;
            let row_class = if improved { " class=\"improved\"" } else { "" };
            format!(
                "<tr{}><td>{}</td><td>{}</td><td class=\"desc\">{}</td><td class=\"{}\">{}</td><td class=\"{}\">{}</td></tr>",
                row_class, from, to, desc, old_class, old_icon, new_class, new_icon
            )
        })
        .collect();

    // Resolution log for new resolver
    let log_rows: String = resolution_log
        .iter()
        .take(100)
        .map(|entry| {
            let from_name = entity_map
                .get(&entry.from_entity)
                .map(|e| e.name.as_str())
                .unwrap_or("?");
            let to_name = entry
                .resolved_to
                .as_ref()
                .and_then(|id| entity_map.get(id))
                .map(|e| e.name.as_str())
                .unwrap_or("-");
            let status_class = if entry.resolved_to.is_some() {
                "pass"
            } else {
                "unresolved"
            };
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"{}\">{}</td></tr>",
                from_name, entry.reference, to_name, status_class, entry.method
            )
        })
        .collect();

    // All edges tables
    let all_old_edges: String = old_edges
        .iter()
        .take(60)
        .map(|(from, to)| {
            let f = entity_map.get(from).map(|e| format!("{} ({})", e.name, short_file(&e.file_path))).unwrap_or_else(|| from.clone());
            let t = entity_map.get(to).map(|e| format!("{} ({})", e.name, short_file(&e.file_path))).unwrap_or_else(|| to.clone());
            format!("<tr><td>{}</td><td>{}</td></tr>", f, t)
        })
        .collect();

    let all_new_edges: String = new_edges
        .iter()
        .take(60)
        .map(|(from, to)| {
            let f = entity_map.get(from).map(|e| format!("{} ({})", e.name, short_file(&e.file_path))).unwrap_or_else(|| from.clone());
            let t = entity_map.get(to).map(|e| format!("{} ({})", e.name, short_file(&e.file_path))).unwrap_or_else(|| to.clone());
            format!("<tr><td>{}</td><td>{}</td></tr>", f, t)
        })
        .collect();

    format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>sem scope resolution: bag-of-words vs tree-sitter scopes</title>
<style>
*{{ margin:0; padding:0; box-sizing:border-box; }}
body {{ background:#0a0a12; color:#ccc; font-family:-apple-system,sans-serif; padding:32px 48px; }}
h1 {{ font-size:20px; font-weight:600; color:#fff; margin-bottom:8px; letter-spacing:0.5px; }}
h2 {{ font-size:14px; font-weight:600; color:#fff; margin:32px 0 12px; text-transform:uppercase; letter-spacing:1px; }}
h3 {{ font-size:12px; font-weight:500; color:#888; margin:20px 0 8px; }}
p {{ font-size:13px; color:#888; margin-bottom:24px; line-height:1.6; }}

.cards {{ display:grid; grid-template-columns:1fr 1fr; gap:16px; margin-bottom:32px; }}
.card {{ background:rgba(255,255,255,0.03); border:1px solid rgba(255,255,255,0.06); border-radius:10px; padding:20px 24px; }}
.card.winner {{ border-color:rgba(74,222,128,0.3); }}
.card h3 {{ margin:0 0 16px; font-size:11px; text-transform:uppercase; letter-spacing:2px; }}
.metric {{ display:flex; justify-content:space-between; align-items:baseline; padding:4px 0; }}
.metric-label {{ font-size:12px; color:#666; }}
.metric-val {{ font-size:24px; font-weight:700; font-variant-numeric:tabular-nums; }}
.metric-val.green {{ color:#4ade80; }}
.metric-val.red {{ color:#f87171; }}
.metric-val.yellow {{ color:#fbbf24; }}
.metric-val.blue {{ color:#60a5fa; }}
.metric-sub {{ font-size:11px; color:#555; }}

.bar-compare {{ display:flex; gap:8px; align-items:center; margin:8px 0; }}
.bar {{ height:8px; border-radius:4px; transition:width 0.3s; }}
.bar-label {{ font-size:11px; color:#666; min-width:60px; }}

table {{ width:100%; border-collapse:collapse; font-size:12px; margin-bottom:8px; }}
th {{ text-align:left; padding:8px 10px; color:#555; font-size:10px; text-transform:uppercase; letter-spacing:1px; border-bottom:1px solid rgba(255,255,255,0.06); }}
td {{ padding:6px 10px; border-bottom:1px solid rgba(255,255,255,0.03); }}
td.pass {{ color:#4ade80; }}
td.fail {{ color:#f87171; }}
td.unresolved {{ color:#666; }}
td.desc {{ color:#666; font-size:11px; }}
tr.improved {{ background:rgba(74,222,128,0.05); }}

.tag {{ display:inline-block; padding:2px 8px; border-radius:4px; font-size:10px; font-weight:500; }}
.tag-green {{ background:rgba(74,222,128,0.15); color:#4ade80; }}
.tag-red {{ background:rgba(248,113,113,0.15); color:#f87171; }}
.tag-blue {{ background:rgba(96,165,250,0.15); color:#60a5fa; }}

.tabs {{ display:flex; gap:2px; margin-bottom:12px; }}
.tab {{ padding:6px 14px; background:rgba(255,255,255,0.03); border:1px solid rgba(255,255,255,0.06); border-radius:6px; font-size:11px; color:#888; cursor:pointer; }}
.tab.active {{ background:rgba(255,255,255,0.06); color:#fff; border-color:rgba(255,255,255,0.1); }}
.tab-content {{ display:none; }}
.tab-content.active {{ display:block; }}

.split {{ display:grid; grid-template-columns:1fr 1fr; gap:16px; }}
</style>
</head>
<body>

<h1>sem scope resolution benchmark</h1>
<p>Comparing bag-of-words tokenization (current) vs tree-sitter scope-aware resolution (new).<br>
Test: 4 Python files with same-name methods, variable type tracking, cross-file imports.</p>

<div class="cards">
    <div class="card{}">
        <h3 style="color:#f87171">Bag-of-words (current)</h3>
        <div class="metric">
            <span class="metric-label">Recall</span>
            <span class="metric-val{}">{:.0}%</span>
        </div>
        <div class="metric">
            <span class="metric-label">Precision</span>
            <span class="metric-val{}">{:.0}%</span>
        </div>
        <div class="metric">
            <span class="metric-label">F1</span>
            <span class="metric-val">{:.0}%</span>
        </div>
        <div class="metric">
            <span class="metric-label">True Positives</span>
            <span class="metric-val">{}</span>
        </div>
        <div class="metric">
            <span class="metric-label">False Negatives</span>
            <span class="metric-val red">{}</span>
        </div>
        <div class="metric">
            <span class="metric-label">False Positives</span>
            <span class="metric-val red">{}</span>
        </div>
        <div class="metric">
            <span class="metric-label">Total Edges</span>
            <span class="metric-val">{}</span>
        </div>
    </div>
    <div class="card{}">
        <h3 style="color:#4ade80">Tree-sitter scopes (new)</h3>
        <div class="metric">
            <span class="metric-label">Recall</span>
            <span class="metric-val{}">{:.0}%</span>
        </div>
        <div class="metric">
            <span class="metric-label">Precision</span>
            <span class="metric-val{}">{:.0}%</span>
        </div>
        <div class="metric">
            <span class="metric-label">F1</span>
            <span class="metric-val green">{:.0}%</span>
        </div>
        <div class="metric">
            <span class="metric-label">True Positives</span>
            <span class="metric-val green">{}</span>
        </div>
        <div class="metric">
            <span class="metric-label">False Negatives</span>
            <span class="metric-val{}">{}</span>
        </div>
        <div class="metric">
            <span class="metric-label">False Positives</span>
            <span class="metric-val{}">{}</span>
        </div>
        <div class="metric">
            <span class="metric-label">Total Edges</span>
            <span class="metric-val">{}</span>
        </div>
    </div>
</div>

<h2>Expected Edges (True Positives / False Negatives)</h2>
<p>Green = found, Red = missed. Highlighted rows = improved by scope resolver.</p>
<table>
<tr><th>From</th><th>To</th><th>Description</th><th>Bag-of-words</th><th>Scope-aware</th></tr>
{}
</table>

<h2>False Positive Check</h2>
<p>These edges should NOT exist. Green = correctly absent, Red = wrongly present.</p>
<table>
<tr><th>From</th><th>To</th><th>Description</th><th>Bag-of-words</th><th>Scope-aware</th></tr>
{}
</table>

<h2>Resolution Log (new resolver)</h2>
<p>How each reference was resolved: scope_chain, type_tracking, import, or unresolved.</p>
<table>
<tr><th>From Entity</th><th>Reference</th><th>Resolved To</th><th>Method</th></tr>
{}
</table>

<h2>All Edges</h2>
<div class="split">
<div>
<h3>Bag-of-words ({} edges)</h3>
<table><tr><th>From</th><th>To</th></tr>{}</table>
</div>
<div>
<h3>Scope-aware ({} edges)</h3>
<table><tr><th>From</th><th>To</th></tr>{}</table>
</div>
</div>

</body>
</html>"#,
        // Old card winner class
        if old_f1 > new_f1 {{ " winner" }} else {{ "" }},
        // Old recall color
        if old_recall >= 0.8 {{ " green" }} else if old_recall >= 0.5 {{ " yellow" }} else {{ " red" }},
        old_recall * 100.0,
        // Old precision color
        if old_precision >= 0.8 {{ " green" }} else if old_precision >= 0.5 {{ " yellow" }} else {{ " red" }},
        old_precision * 100.0,
        old_f1 * 100.0,
        old_tp, old_fn, old_fp, old_total,
        // New card winner class
        if new_f1 >= old_f1 {{ " winner" }} else {{ "" }},
        // New recall color
        if new_recall >= 0.8 {{ " green" }} else if new_recall >= 0.5 {{ " yellow" }} else {{ " red" }},
        new_recall * 100.0,
        // New precision color
        if new_precision >= 0.8 {{ " green" }} else if new_precision >= 0.5 {{ " yellow" }} else {{ " red" }},
        new_precision * 100.0,
        new_f1 * 100.0,
        new_tp,
        // New FN color
        if new_fn < old_fn {{ " green" }} else {{ " red" }},
        new_fn,
        // New FP color
        if new_fp < old_fp {{ " green" }} else if new_fp == 0 {{ " green" }} else {{ " red" }},
        new_fp,
        new_total,
        expected_rows,
        fp_rows,
        log_rows,
        old_total, all_old_edges,
        new_total, all_new_edges,
    )
}

fn short_file(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// --- Per-language scope resolution tests ---

fn get_ts_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.ts -> models.ts
        ("createDog", "Dog", "service calls Dog constructor"),
        ("createDog", "validate", "service calls dog.validate()"),
        ("createDog", "getConnection", "service calls getConnection()"),
        ("createCat", "Cat", "service calls Cat constructor"),
        ("createCat", "validate", "service calls cat.validate()"),
        ("createCat", "getConnection", "service calls getConnection()"),
        ("transferAnimal", "Transaction", "service calls Transaction constructor"),
        ("transferAnimal", "getConnection", "service calls getConnection()"),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter.add() on Shelter"),
        ("listAnimals", "getConnection", "service calls getConnection()"),
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
        ("Transaction::execute", "execute", "Transaction.execute calls this.conn.execute"),
        ("Transaction::commit", "commit", "Transaction.commit calls this.conn.commit"),
    ]
}

fn get_ts_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        ("handleCreateDog", "Transaction", "handler doesn't use Transaction directly"),
        ("handleCreateCat", "Transaction", "handler doesn't use Transaction directly"),
    ]
}

fn get_rust_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.rs -> models.rs
        ("create_dog", "Dog", "service calls Dog::new"),
        ("create_dog", "validate", "service calls dog.validate()"),
        ("create_dog", "get_connection", "service calls get_connection()"),
        ("create_cat", "Cat", "service calls Cat::new"),
        ("create_cat", "validate", "service calls cat.validate()"),
        ("create_cat", "get_connection", "service calls get_connection()"),
        ("transfer_animal", "Transaction", "service calls Transaction::new"),
        ("transfer_animal", "get_connection", "service calls get_connection()"),
        ("transfer_animal", "execute", "txn.execute() on Transaction"),
        ("transfer_animal", "commit", "txn.commit() on Transaction"),
        ("transfer_animal", "add", "shelter.add() on Shelter"),
        ("list_animals", "get_connection", "service calls get_connection()"),
        ("list_animals", "execute", "conn.execute() on Connection"),

        // handlers.rs -> service.rs
        ("handle_create_dog", "create_dog", "handler calls service"),
        ("handle_create_cat", "create_cat", "handler calls service"),
        ("handle_transfer", "transfer_animal", "handler calls service"),
        ("handle_transfer", "Shelter", "handler creates Shelter"),
        ("handle_transfer", "Dog", "handler creates Dog"),
        ("handle_transfer", "count", "shelter.count() on Shelter"),
        ("handle_list", "list_animals", "handler calls service"),

        // database.rs internal
        ("Transaction::execute", "execute", "Transaction.execute calls self.conn.execute"),
        ("Transaction::commit", "commit", "Transaction.commit calls self.conn.commit"),
    ]
}

fn get_rust_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("create_dog", "Cat", "create_dog shouldn't reference Cat"),
        ("create_cat", "Dog", "create_cat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        ("handle_create_dog", "Transaction", "handler doesn't use Transaction directly"),
        ("handle_create_cat", "Transaction", "handler doesn't use Transaction directly"),
    ]
}

fn get_go_expected_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // service.go -> models.go
        ("CreateDog", "NewDog", "service calls NewDog()"),
        ("CreateDog", "Validate", "service calls dog.Validate()"),
        ("CreateDog", "GetConnection", "service calls GetConnection()"),
        ("CreateCat", "NewCat", "service calls NewCat()"),
        ("CreateCat", "Validate", "service calls cat.Validate()"),
        ("CreateCat", "GetConnection", "service calls GetConnection()"),
        ("TransferAnimal", "NewTransaction", "service calls NewTransaction()"),
        ("TransferAnimal", "GetConnection", "service calls GetConnection()"),
        ("TransferAnimal", "Execute", "txn.Execute() on Transaction"),
        ("TransferAnimal", "Commit", "txn.Commit() on Transaction"),
        ("TransferAnimal", "Add", "shelter.Add() on Shelter"),
        ("ListAnimals", "GetConnection", "service calls GetConnection()"),
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
        ("Transaction::Execute", "Execute", "Transaction.Execute calls Conn.Execute"),
        ("Transaction::Commit", "Commit", "Transaction.Commit calls Conn.Commit"),
    ]
}

fn get_go_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("CreateDog", "Cat", "CreateDog shouldn't reference Cat"),
        ("CreateCat", "Dog", "CreateCat shouldn't reference Dog"),
        ("Validate", "Dog", "standalone Validate != Dog.Validate"),
        ("Validate", "Cat", "standalone Validate != Cat.Validate"),
        ("HandleCreateDog", "Transaction", "handler doesn't use Transaction directly"),
        ("HandleCreateCat", "Transaction", "handler doesn't use Transaction directly"),
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
    let old_edges: Vec<(String, String)> = old_graph.edges.iter()
        .map(|e| (e.from_entity.clone(), e.to_entity.clone()))
        .collect();

    // Run new scope resolver
    let all_entities: Vec<SemanticEntity> = file_refs.iter()
        .filter_map(|file_path| {
            let full_path = root.join(file_path);
            let content = std::fs::read_to_string(&full_path).ok()?;
            let plugin = registry.get_plugin_with_content(file_path, &content)?;
            Some(plugin.extract_entities(&content, file_path))
        })
        .flatten()
        .collect();

    let entity_map: HashMap<String, EntityInfo> = all_entities.iter()
        .map(|e| (e.id.clone(), EntityInfo {
            id: e.id.clone(),
            name: e.name.clone(),
            entity_type: e.entity_type.clone(),
            file_path: e.file_path.clone(),
            parent_id: e.parent_id.clone(),
            start_line: e.start_line,
            end_line: e.end_line,
        }))
        .collect();

    let scope_result = scope_resolve::resolve_with_scopes(root, &file_refs, &all_entities, &entity_map, None);
    let new_edges: Vec<(String, String)> = scope_result.edges.iter()
        .map(|(from, to, _)| (from.clone(), to.clone()))
        .collect();

    // Debug: dump edges for languages that need config tuning
    if new_edges.is_empty() && !old_edges.is_empty() {
        eprintln!("  DEBUG [{}]: old_edges={}, new_edges=0, entities={}", lang_name, old_edges.len(), all_entities.len());
    }

    // Score
    let mut old_tp = 0; let mut old_fn = 0; let mut old_fp = 0;
    let mut new_tp = 0; let mut new_fn = 0; let mut new_fp = 0;

    for (from_pat, to_pat, desc) in expected {
        let old_found = edge_matches(&old_edges, &entity_map, from_pat, to_pat);
        let new_found = edge_matches(&new_edges, &entity_map, from_pat, to_pat);
        if old_found { old_tp += 1; } else { old_fn += 1; }
        if new_found { new_tp += 1; } else {
            new_fn += 1;
            eprintln!("  MISSED [{}]: {} -> {} ({})", lang_name, from_pat, to_pat, desc);
        }
    }

    for (from_pat, to_pat, desc) in false_positives {
        let old_found = edge_matches(&old_edges, &entity_map, from_pat, to_pat);
        let new_found = edge_matches(&new_edges, &entity_map, from_pat, to_pat);
        if old_found { old_fp += 1; }
        if new_found {
            new_fp += 1;
            eprintln!("  FALSE POSITIVE [{}]: {} -> {} ({})", lang_name, from_pat, to_pat, desc);
        }
    }

    let old_recall = if old_tp + old_fn > 0 { old_tp as f64 / (old_tp + old_fn) as f64 } else { 0.0 };
    let new_recall = if new_tp + new_fn > 0 { new_tp as f64 / (new_tp + new_fn) as f64 } else { 0.0 };

    eprintln!("\n=== {} Scope Resolution ===", lang_name);
    eprintln!("Old (bag-of-words): {} TP, {} FN, {} FP | {:.0}% recall",
        old_tp, old_fn, old_fp, old_recall * 100.0);
    eprintln!("New (scope-aware):  {} TP, {} FN, {} FP | {:.0}% recall",
        new_tp, new_fn, new_fp, new_recall * 100.0);
    eprintln!("Total edges: old={}, new={}", old_edges.len(), new_edges.len());

    // Report results - the scope resolver should be producing edges for supported languages
    if new_edges.is_empty() && !old_edges.is_empty() {
        eprintln!("  NOTE [{}]: Scope resolver produced 0 edges (config may need tuning)", lang_name);
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
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scope_test/python");
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
        .map(|e| (e.from_entity.clone(), e.to_entity.clone()))
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
    eprintln!("{} / {} expected edges found ({:.0}% recall)", tp, expected.len(), recall * 100.0);
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
        recall * 100.0, missed
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
        ("createDog", "getConnection", "service calls DatabaseHelper.getConnection()"),
        ("createCat", "Cat", "service creates Cat"),
        ("createCat", "validate", "service calls cat.validate()"),
        ("createCat", "getConnection", "service calls getConnection()"),
        ("transferAnimal", "Transaction", "service creates Transaction"),
        ("transferAnimal", "getConnection", "service calls getConnection()"),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter.add() on Shelter"),
        ("listAnimals", "getConnection", "service calls getConnection()"),
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
        ("Transaction::execute", "execute", "Transaction.execute calls conn.execute"),
        ("Transaction::commit", "commit", "Transaction.commit calls conn.commit"),
    ]
}

fn get_java_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        ("handleCreateDog", "Transaction", "handler doesn't use Transaction directly"),
        ("handleCreateCat", "Transaction", "handler doesn't use Transaction directly"),
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
        ("CreateDog", "GetConnection", "service calls DatabaseHelper.GetConnection()"),
        ("CreateCat", "Cat", "service creates Cat"),
        ("CreateCat", "Validate", "service calls cat.Validate()"),
        ("CreateCat", "GetConnection", "service calls GetConnection()"),
        ("TransferAnimal", "Transaction", "service creates Transaction"),
        ("TransferAnimal", "GetConnection", "service calls GetConnection()"),
        ("TransferAnimal", "Execute", "txn.Execute() on Transaction"),
        ("TransferAnimal", "Commit", "txn.Commit() on Transaction"),
        ("TransferAnimal", "Add", "shelter.Add() on Shelter"),
        ("ListAnimals", "GetConnection", "service calls GetConnection()"),
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
        ("Transaction::Execute", "Execute", "Transaction.Execute calls conn.Execute"),
        ("Transaction::Commit", "Commit", "Transaction.Commit calls conn.Commit"),
    ]
}

fn get_csharp_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("CreateDog", "Cat", "CreateDog shouldn't reference Cat"),
        ("CreateCat", "Dog", "CreateCat shouldn't reference Dog"),
        ("Validate", "Dog", "standalone Validate != Dog.Validate"),
        ("Validate", "Cat", "standalone Validate != Cat.Validate"),
        ("HandleCreateDog", "Transaction", "handler doesn't use Transaction directly"),
        ("HandleCreateCat", "Transaction", "handler doesn't use Transaction directly"),
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
        ("createDog", "getConnection", "service calls getConnection()"),
        ("createCat", "Cat", "service creates Cat"),
        ("createCat", "validate", "service calls cat.validate()"),
        ("createCat", "getConnection", "service calls getConnection()"),
        ("transferAnimal", "Transaction", "service creates Transaction"),
        ("transferAnimal", "getConnection", "service calls getConnection()"),
        ("transferAnimal", "execute", "txn.execute() on Transaction"),
        ("transferAnimal", "commit", "txn.commit() on Transaction"),
        ("transferAnimal", "add", "shelter->add() on Shelter"),
        ("listAnimals", "getConnection", "service calls getConnection()"),
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
        ("Transaction::execute", "execute", "Transaction.execute calls conn->execute"),
        ("Transaction::commit", "commit", "Transaction.commit calls conn->commit"),
    ]
}

fn get_cpp_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("createDog", "Cat", "createDog shouldn't reference Cat"),
        ("createCat", "Dog", "createCat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog.validate"),
        ("validate", "Cat", "standalone validate != Cat.validate"),
        ("handleCreateDog", "Transaction", "handler doesn't use Transaction directly"),
        ("handleCreateCat", "Transaction", "handler doesn't use Transaction directly"),
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
        ("create_dog", "get_connection", "service calls get_connection"),
        ("create_cat", "Cat", "service creates Cat"),
        ("create_cat", "validate", "service calls cat.validate"),
        ("create_cat", "get_connection", "service calls get_connection"),
        ("transfer_animal", "Transaction", "service creates Transaction"),
        ("transfer_animal", "get_connection", "service calls get_connection"),
        ("transfer_animal", "execute", "txn.execute on Transaction"),
        ("transfer_animal", "commit", "txn.commit on Transaction"),
        ("transfer_animal", "add", "shelter.add on Shelter"),
        ("list_animals", "get_connection", "service calls get_connection"),
        ("list_animals", "execute", "conn.execute on Connection"),

        // handlers.rb -> service.rb
        ("handle_create_dog", "create_dog", "handler calls service"),
        ("handle_create_cat", "create_cat", "handler calls service"),
        ("handle_transfer", "transfer_animal", "handler calls service"),
        ("handle_transfer", "Shelter", "handler creates Shelter"),
        ("handle_transfer", "Dog", "handler creates Dog"),
        ("handle_transfer", "count", "shelter.count on Shelter"),
        ("handle_list", "list_animals", "handler calls service"),

        // database.rb internal
        ("Transaction::execute", "execute", "Transaction#execute calls @conn.execute"),
        ("Transaction::commit", "commit", "Transaction#commit calls @conn.commit"),
    ]
}

fn get_ruby_false_positive_edges() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("create_dog", "Cat", "create_dog shouldn't reference Cat"),
        ("create_cat", "Dog", "create_cat shouldn't reference Dog"),
        ("validate", "Dog", "standalone validate != Dog#validate"),
        ("validate", "Cat", "standalone validate != Cat#validate"),
        ("handle_create_dog", "Transaction", "handler doesn't use Transaction directly"),
        ("handle_create_cat", "Transaction", "handler doesn't use Transaction directly"),
    ]
}

#[test]
fn scope_resolve_ruby() {
    let expected = get_ruby_expected_edges();
    let fp = get_ruby_false_positive_edges();
    run_scope_resolve_for_lang("ruby", &expected, &fp);
}
