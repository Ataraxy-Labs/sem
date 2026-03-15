use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sem_core::git::types::{FileChange, FileStatus};
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::plugins::create_default_registry;

const SMALL: &str = include_str!("../resources/svelte/small.svelte");
const MEDIUM: &str = include_str!("../resources/svelte/medium.svelte");
const LARGE: &str = include_str!("../resources/svelte/large.svelte");
const MODULE: &str = include_str!("../resources/svelte/module.svelte.ts");

fn bench_parse(c: &mut Criterion) {
    let registry = create_default_registry();
    let plugin = registry.get_plugin("App.svelte").unwrap();
    let module_plugin = registry.get_plugin("state.svelte.ts").unwrap();

    let mut group = c.benchmark_group("parse");

    group.bench_function("small", |b| {
        b.iter(|| plugin.extract_entities(black_box(SMALL), "App.svelte"))
    });

    group.bench_function("medium", |b| {
        b.iter(|| plugin.extract_entities(black_box(MEDIUM), "UserList.svelte"))
    });

    group.bench_function("large", |b| {
        b.iter(|| plugin.extract_entities(black_box(LARGE), "DataTable.svelte"))
    });

    group.bench_function("module", |b| {
        b.iter(|| module_plugin.extract_entities(black_box(MODULE), "state.svelte.ts"))
    });

    group.finish();
}

fn bench_diff(c: &mut Criterion) {
    let registry = create_default_registry();

    let after_small_edit = MEDIUM.replace(
        "onMount(fetchUsers);",
        "function resetFilters() {\n        searchTerm = '';\n        selectedRole = 'all';\n    }\n\n    onMount(fetchUsers);",
    );

    let changes = vec![FileChange {
        file_path: "UserList.svelte".to_string(),
        status: FileStatus::Modified,
        old_file_path: None,
        before_content: Some(MEDIUM.to_string()),
        after_content: Some(after_small_edit.clone()),
    }];

    let mut group = c.benchmark_group("diff");

    group.bench_function("small_edit", |b| {
        b.iter(|| compute_semantic_diff(black_box(&changes), &registry, None, None))
    });

    let after_structural = MEDIUM.replace(
        "<Footer />",
        "{#if users.length > 100}\n        <p class=\"warning\">Large dataset - consider filtering</p>\n    {/if}\n\n    <Footer />",
    );

    let changes_structural = vec![FileChange {
        file_path: "UserList.svelte".to_string(),
        status: FileStatus::Modified,
        old_file_path: None,
        before_content: Some(MEDIUM.to_string()),
        after_content: Some(after_structural),
    }];

    group.bench_function("structural_change", |b| {
        b.iter(|| compute_semantic_diff(black_box(&changes_structural), &registry, None, None))
    });

    let changes_multi = vec![
        FileChange {
            file_path: "UserList.svelte".to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(MEDIUM.to_string()),
            after_content: Some(after_small_edit),
        },
        FileChange {
            file_path: "DataTable.svelte".to_string(),
            status: FileStatus::Added,
            old_file_path: None,
            before_content: None,
            after_content: Some(LARGE.to_string()),
        },
        FileChange {
            file_path: "state.svelte.ts".to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(MODULE.to_string()),
            after_content: Some(MODULE.replace(
                "export function toggleSidebar",
                "export function closeSidebar() {\n    appState.update(s => ({ ...s, sidebarOpen: false }));\n}\n\nexport function toggleSidebar",
            )),
        },
    ];

    group.bench_function("multi_file", |b| {
        b.iter(|| compute_semantic_diff(black_box(&changes_multi), &registry, None, None))
    });

    group.finish();
}

criterion_group!(benches, bench_parse, bench_diff);
criterion_main!(benches);
