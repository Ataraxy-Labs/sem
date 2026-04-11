use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use clap::ValueEnum;
use colored::Colorize;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};
use sem_core::parser::plugins::create_default_registry;

pub struct GrepOptions {
    pub cwd: String,
    pub pattern: Option<String>,
    pub content: bool,
    pub case_sensitive: bool,
    pub entity_types: Vec<String>,
    pub path_substring: Option<String>,
    pub tests: bool,
    pub depends_on: Vec<String>,
    pub ref_kind: Option<RefKind>,
    pub min_dependencies: Option<usize>,
    pub min_dependents: Option<usize>,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RefKind {
    Calls,
    Imports,
    Type,
}

impl RefKind {
    fn matches(self, ref_type: &RefType) -> bool {
        match self {
            Self::Calls => *ref_type == RefType::Calls,
            Self::Imports => *ref_type == RefType::Imports,
            Self::Type => *ref_type == RefType::TypeRef,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::Imports => "imports",
            Self::Type => "type",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepMatch {
    name: String,
    entity_type: String,
    file_path: String,
    start_line: usize,
    end_line: usize,
    parent_id: Option<String>,
    dependency_count: usize,
    dependent_count: usize,
    is_test: bool,
    matched_dependencies: Vec<String>,
}

pub fn grep_command(opts: GrepOptions) {
    validate_options(&opts);

    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let file_paths = super::graph::find_supported_files_public(root, &registry, &ext_filter);
    let (graph, all_entities) =
        super::graph::get_or_build_graph(root, &file_paths, &registry, opts.no_cache);
    let matches = find_matches(&graph, &all_entities, &opts);

    if opts.json {
        print_json(&matches, &opts);
    } else {
        print_terminal(&matches, &opts);
    }
}

fn validate_options(opts: &GrepOptions) {
    let has_selector = opts.pattern.is_some()
        || !opts.entity_types.is_empty()
        || opts.path_substring.is_some()
        || opts.tests
        || !opts.depends_on.is_empty()
        || opts.min_dependencies.is_some()
        || opts.min_dependents.is_some();

    if !has_selector {
        eprintln!(
            "{} Provide a pattern or at least one filter (`--type`, `--path`, `--tests`, `--depends-on`, `--min-dependencies`, `--min-dependents`)",
            "error:".red().bold(),
        );
        std::process::exit(1);
    }

    if opts.content && opts.pattern.is_none() {
        eprintln!("{} `--content` requires a pattern", "error:".red().bold(),);
        std::process::exit(1);
    }

    if opts.ref_kind.is_some() && opts.depends_on.is_empty() {
        eprintln!(
            "{} `--ref-kind` requires at least one `--depends-on` filter",
            "error:".red().bold(),
        );
        std::process::exit(1);
    }
}

fn find_matches(
    graph: &EntityGraph,
    all_entities: &[SemanticEntity],
    opts: &GrepOptions,
) -> Vec<GrepMatch> {
    let entities_by_id: HashMap<&str, &SemanticEntity> = all_entities
        .iter()
        .map(|entity| (entity.id.as_str(), entity))
        .collect();
    let test_ids = if opts.tests {
        graph.filter_test_entities(all_entities)
    } else {
        HashSet::new()
    };
    let outgoing_edges = build_outgoing_edges(&graph.edges);
    let mut matches = Vec::new();

    for entity in graph.entities.values() {
        let source = match entities_by_id.get(entity.id.as_str()) {
            Some(source) => *source,
            None => continue,
        };

        if !matches_pattern(
            entity,
            &source.content,
            opts.pattern.as_deref(),
            opts.content,
            opts.case_sensitive,
        ) {
            continue;
        }
        if !matches_type(&entity.entity_type, &opts.entity_types, opts.case_sensitive) {
            continue;
        }
        if let Some(path_substring) = opts.path_substring.as_deref() {
            if !matches_text(&entity.file_path, path_substring, opts.case_sensitive) {
                continue;
            }
        }
        if opts.tests && !test_ids.contains(&entity.id) {
            continue;
        }

        let dependency_count = graph
            .dependencies
            .get(entity.id.as_str())
            .map_or(0, Vec::len);
        let dependent_count = graph.dependents.get(entity.id.as_str()).map_or(0, Vec::len);

        if let Some(min_dependencies) = opts.min_dependencies {
            if dependency_count < min_dependencies {
                continue;
            }
        }
        if let Some(min_dependents) = opts.min_dependents {
            if dependent_count < min_dependents {
                continue;
            }
        }

        let matched_dependencies = if opts.depends_on.is_empty() {
            Vec::new()
        } else {
            find_matching_dependencies(
                graph,
                entity,
                &outgoing_edges,
                &opts.depends_on,
                opts.ref_kind,
                opts.case_sensitive,
            )
        };

        if !opts.depends_on.is_empty() && matched_dependencies.is_empty() {
            continue;
        }

        matches.push(GrepMatch {
            name: entity.name.clone(),
            entity_type: entity.entity_type.clone(),
            file_path: entity.file_path.clone(),
            start_line: entity.start_line,
            end_line: entity.end_line,
            parent_id: entity.parent_id.clone(),
            dependency_count,
            dependent_count,
            is_test: test_ids.contains(&entity.id),
            matched_dependencies,
        });
    }

    matches.sort_by(|left, right| {
        left.file_path
            .cmp(&right.file_path)
            .then(left.start_line.cmp(&right.start_line))
            .then(left.end_line.cmp(&right.end_line))
            .then(left.name.cmp(&right.name))
    });
    matches
}

fn build_outgoing_edges<'a>(edges: &'a [EntityRef]) -> HashMap<&'a str, Vec<&'a EntityRef>> {
    let mut outgoing: HashMap<&str, Vec<&EntityRef>> = HashMap::new();
    for edge in edges {
        outgoing
            .entry(edge.from_entity.as_str())
            .or_default()
            .push(edge);
    }
    outgoing
}

fn find_matching_dependencies(
    graph: &EntityGraph,
    entity: &EntityInfo,
    outgoing_edges: &HashMap<&str, Vec<&EntityRef>>,
    depends_on: &[String],
    ref_kind: Option<RefKind>,
    case_sensitive: bool,
) -> Vec<String> {
    let mut matched: HashSet<String> = HashSet::new();

    if let Some(edges) = outgoing_edges.get(entity.id.as_str()) {
        for edge in edges {
            if let Some(kind) = ref_kind {
                if !kind.matches(&edge.ref_type) {
                    continue;
                }
            }

            let target = match graph.entities.get(edge.to_entity.as_str()) {
                Some(target) => target,
                None => continue,
            };

            if depends_on
                .iter()
                .any(|pattern| matches_text(&target.name, pattern, case_sensitive))
            {
                matched.insert(target.name.clone());
            }
        }
    }

    let mut matched: Vec<_> = matched.into_iter().collect();
    matched.sort();
    matched
}

fn matches_pattern(
    entity: &EntityInfo,
    content: &str,
    pattern: Option<&str>,
    search_content: bool,
    case_sensitive: bool,
) -> bool {
    let Some(pattern) = pattern else {
        return true;
    };

    matches_text(&entity.name, pattern, case_sensitive)
        || (search_content && matches_text(content, pattern, case_sensitive))
}

fn matches_type(value: &str, filters: &[String], case_sensitive: bool) -> bool {
    filters.is_empty()
        || filters.iter().any(|filter| {
            if case_sensitive {
                value == filter
            } else {
                value.eq_ignore_ascii_case(filter)
            }
        })
}

fn matches_text(haystack: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        return haystack.contains(needle);
    }

    let needle_lower = needle.to_lowercase();
    if haystack.to_lowercase().contains(&needle_lower) {
        return true;
    }

    let normalized_needle = normalize_identifier_text(needle);
    !normalized_needle.is_empty()
        && normalize_identifier_text(haystack).contains(&normalized_needle)
}

fn normalize_identifier_text(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '_' | '-') {
            continue;
        }
        normalized.extend(ch.to_lowercase());
    }
    normalized
}

fn print_terminal(matches: &[GrepMatch], opts: &GrepOptions) {
    if matches.is_empty() {
        println!("{} {}", "grep:".green().bold(), "no matches".dimmed());
        return;
    }

    println!(
        "{} {}",
        "grep:".green().bold(),
        format!("{} matches", matches.len()).bold(),
    );

    if let Some(pattern) = opts.pattern.as_deref() {
        println!("  {} {}", "pattern".dimmed(), pattern.bold());
    }
    if !opts.entity_types.is_empty() {
        println!(
            "  {} {}",
            "types".dimmed(),
            opts.entity_types.join(", ").bold()
        );
    }
    if let Some(path_substring) = opts.path_substring.as_deref() {
        println!("  {} {}", "path".dimmed(), path_substring.bold());
    }
    if !opts.depends_on.is_empty() {
        let relation = opts.ref_kind.map_or("any ref", RefKind::as_str);
        println!(
            "  {} {} ({})",
            "depends on".dimmed(),
            opts.depends_on.join(", ").bold(),
            relation.dimmed(),
        );
    }
    println!();

    let mut by_file: BTreeMap<&str, Vec<&GrepMatch>> = BTreeMap::new();
    for matched in matches {
        by_file
            .entry(matched.file_path.as_str())
            .or_default()
            .push(matched);
    }

    for (file, entries) in by_file {
        println!("  {}", file.bold());
        for entry in entries {
            let mut suffix = format!(
                "L{}-{}, deps: {}, dependents: {}",
                entry.start_line, entry.end_line, entry.dependency_count, entry.dependent_count,
            );
            if entry.is_test {
                suffix.push_str(", test");
            }
            if !entry.matched_dependencies.is_empty() {
                suffix.push_str(&format!(", via: {}", entry.matched_dependencies.join(", ")));
            }

            println!(
                "    {} {} ({})",
                entry.entity_type.dimmed(),
                entry.name.bold(),
                suffix.dimmed(),
            );
        }
    }
}

fn print_json(matches: &[GrepMatch], opts: &GrepOptions) {
    let output = serde_json::json!({
        "summary": {
            "matches": matches.len(),
            "pattern": opts.pattern.as_deref(),
            "content": opts.content,
            "caseSensitive": opts.case_sensitive,
            "types": &opts.entity_types,
            "path": opts.path_substring.as_deref(),
            "tests": opts.tests,
            "dependsOn": &opts.depends_on,
            "refKind": opts.ref_kind.map(RefKind::as_str),
            "minDependencies": opts.min_dependencies,
            "minDependents": opts.min_dependents,
        },
        "matches": matches.iter().map(|entry| serde_json::json!({
            "name": &entry.name,
            "type": &entry.entity_type,
            "file": &entry.file_path,
            "lines": [entry.start_line, entry.end_line],
            "parentId": &entry.parent_id,
            "dependencyCount": entry.dependency_count,
            "dependentCount": entry.dependent_count,
            "isTest": entry.is_test,
            "matchedDependencies": &entry.matched_dependencies,
        })).collect::<Vec<_>>(),
    });

    println!("{}", serde_json::to_string(&output).unwrap());
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use sem_core::model::entity::SemanticEntity;
    use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};

    use super::{find_matches, matches_text, GrepOptions, RefKind};

    fn entity_info(
        id: &str,
        name: &str,
        entity_type: &str,
        file_path: &str,
        start_line: usize,
    ) -> EntityInfo {
        EntityInfo {
            id: id.to_string(),
            name: name.to_string(),
            entity_type: entity_type.to_string(),
            file_path: file_path.to_string(),
            parent_id: None,
            start_line,
            end_line: start_line + 2,
        }
    }

    fn semantic_entity(info: &EntityInfo, content: &str) -> SemanticEntity {
        SemanticEntity {
            id: info.id.clone(),
            file_path: info.file_path.clone(),
            entity_type: info.entity_type.clone(),
            name: info.name.clone(),
            parent_id: info.parent_id.clone(),
            content: content.to_string(),
            content_hash: format!("hash-{}", info.id),
            structural_hash: None,
            start_line: info.start_line,
            end_line: info.end_line,
            metadata: None,
        }
    }

    fn test_options() -> GrepOptions {
        GrepOptions {
            cwd: ".".to_string(),
            pattern: None,
            content: false,
            case_sensitive: false,
            entity_types: Vec::new(),
            path_substring: None,
            tests: false,
            depends_on: Vec::new(),
            ref_kind: None,
            min_dependencies: None,
            min_dependents: None,
            json: false,
            file_exts: Vec::new(),
            no_cache: false,
        }
    }

    fn fixture_graph() -> (EntityGraph, Vec<SemanticEntity>) {
        let exec = entity_info(
            "src/process.ts::function::exec",
            "exec",
            "function",
            "src/process.ts",
            1,
        );
        let login = entity_info(
            "src/auth.ts::function::login",
            "login",
            "function",
            "src/auth.ts",
            10,
        );
        let run_shell = entity_info(
            "src/shell.ts::function::runShell",
            "runShell",
            "function",
            "src/shell.ts",
            20,
        );
        let auth_controller = entity_info(
            "src/auth_controller.ts::class::AuthController",
            "AuthController",
            "class",
            "src/auth_controller.ts",
            5,
        );
        let login_test = entity_info(
            "tests/auth_test.rs::function::test_login",
            "test_login",
            "function",
            "tests/auth_test.rs",
            3,
        );

        let graph = EntityGraph::from_parts(
            HashMap::from([
                (exec.id.clone(), exec.clone()),
                (login.id.clone(), login.clone()),
                (run_shell.id.clone(), run_shell.clone()),
                (auth_controller.id.clone(), auth_controller.clone()),
                (login_test.id.clone(), login_test.clone()),
            ]),
            vec![
                EntityRef {
                    from_entity: run_shell.id.clone(),
                    to_entity: exec.id.clone(),
                    ref_type: RefType::Calls,
                },
                EntityRef {
                    from_entity: auth_controller.id.clone(),
                    to_entity: login.id.clone(),
                    ref_type: RefType::TypeRef,
                },
                EntityRef {
                    from_entity: login_test.id.clone(),
                    to_entity: login.id.clone(),
                    ref_type: RefType::Calls,
                },
            ],
        );

        let entities = vec![
            semantic_entity(&exec, "export function exec(command) { return command; }"),
            semantic_entity(
                &login,
                "pub fn login(user: &str) -> bool { !user.is_empty() }",
            ),
            semantic_entity(&run_shell, "function runShell() { return exec(\"ls\"); }"),
            semantic_entity(
                &auth_controller,
                "class AuthController extends login { handle() { return true; } }",
            ),
            semantic_entity(
                &login_test,
                "#[test]\nfn test_login() {\n    assert!(login(\"alice\"));\n}",
            ),
        ];

        (graph, entities)
    }

    #[test]
    fn grep_matches_name_by_default_and_content_when_requested() {
        let (graph, entities) = fixture_graph();

        let mut opts = test_options();
        opts.pattern = Some("exec".to_string());

        let name_only = find_matches(&graph, &entities, &opts);
        assert_eq!(name_only.len(), 1);
        assert_eq!(name_only[0].name, "exec");

        opts.content = true;
        let with_content = find_matches(&graph, &entities, &opts);
        let names: Vec<_> = with_content
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["exec", "runShell"]);
    }

    #[test]
    fn grep_matches_common_identifier_styles_by_default() {
        let (graph, entities) = fixture_graph();
        let mut opts = test_options();

        opts.pattern = Some("auth-controller".to_string());
        let kebab_to_pascal = find_matches(&graph, &entities, &opts);
        assert_eq!(kebab_to_pascal.len(), 1);
        assert_eq!(kebab_to_pascal[0].name, "AuthController");

        opts.pattern = Some("RUN_SHELL".to_string());
        let screaming_to_camel = find_matches(&graph, &entities, &opts);
        assert_eq!(screaming_to_camel.len(), 1);
        assert_eq!(screaming_to_camel[0].name, "runShell");
    }

    #[test]
    fn grep_case_sensitive_requires_literal_text() {
        let (graph, entities) = fixture_graph();
        let mut opts = test_options();
        opts.case_sensitive = true;
        opts.pattern = Some("RUN_SHELL".to_string());

        let matches = find_matches(&graph, &entities, &opts);
        assert!(matches.is_empty());

        opts.pattern = Some("runShell".to_string());
        let matches = find_matches(&graph, &entities, &opts);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "runShell");
    }

    #[test]
    fn smart_text_matching_is_not_sparse() {
        assert!(matches_text("authenticate_user", "authenticateUser", false));
        assert!(matches_text("AUTHENTICATE-USER", "authenticate_user", false));
        assert!(!matches_text("authenticate user", "authenticateUser", false));
        assert!(!matches_text("handle_mouse_event", "hm", false));
        assert!(!matches_text("authenticate_user", "authenticateUser", true));
    }

    #[test]
    fn grep_filters_by_dependency_name_and_reference_kind() {
        let (graph, entities) = fixture_graph();
        let mut opts = test_options();
        opts.depends_on = vec!["login".to_string()];

        let any_ref = find_matches(&graph, &entities, &opts);
        let names: Vec<_> = any_ref.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["AuthController", "test_login"]);

        opts.ref_kind = Some(RefKind::Calls);
        let calls_only = find_matches(&graph, &entities, &opts);
        assert_eq!(calls_only.len(), 1);
        assert_eq!(calls_only[0].name, "test_login");
        assert_eq!(calls_only[0].matched_dependencies, vec!["login"]);
    }

    #[test]
    fn grep_combines_test_and_graph_threshold_filters() {
        let (graph, entities) = fixture_graph();
        let mut opts = test_options();
        opts.tests = true;
        opts.depends_on = vec!["login".to_string()];
        opts.min_dependencies = Some(1);

        let matches = find_matches(&graph, &entities, &opts);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "test_login");
        assert!(matches[0].is_test);

        opts.tests = false;
        opts.depends_on.clear();
        opts.min_dependencies = None;
        opts.min_dependents = Some(2);

        let high_fan_in = find_matches(&graph, &entities, &opts);
        assert_eq!(high_fan_in.len(), 1);
        assert_eq!(high_fan_in[0].name, "login");
    }
}
