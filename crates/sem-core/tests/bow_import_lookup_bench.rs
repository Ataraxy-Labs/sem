use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use sem_core::parser::graph::EntityGraph;
use sem_core::parser::plugins::create_default_registry;

struct CountingAllocator;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn reset_allocations() {
    ALLOC_CALLS.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
}

fn allocation_snapshot() -> (usize, usize) {
    (
        ALLOC_CALLS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

#[test]
#[ignore]
fn bag_of_words_import_lookup_allocation_metric() {
    const IMPORTS: usize = 2_000;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let mut lib = String::new();
    for i in 0..IMPORTS {
        lib.push_str(&format!("export const value{i} = {i};\n"));
    }
    std::fs::write(root.join("lib.ts"), lib).unwrap();

    let import_names = (0..IMPORTS)
        .map(|i| format!("value{i}"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut main = format!("import {{ {import_names} }} from './lib';\n\n");
    for i in 0..IMPORTS {
        main.push_str(&format!(
            "export function caller{i}(other: any) {{ other.missing(); return value{i}; }}\n"
        ));
    }
    std::fs::write(root.join("main.ts"), main).unwrap();

    let registry = create_default_registry();
    let files = vec!["lib.ts".to_string(), "main.ts".to_string()];

    reset_allocations();
    let start = std::time::Instant::now();
    let (graph, _) = EntityGraph::build(root, &files, &registry);
    let elapsed = start.elapsed();
    let (alloc_calls, alloc_bytes) = allocation_snapshot();

    let resolved_import_edges = graph
        .edges
        .iter()
        .filter(|edge| {
            let from = graph.entities.get(&edge.from_entity);
            let to = graph.entities.get(&edge.to_entity);
            from.is_some_and(|entity| entity.file_path == "main.ts")
                && to.is_some_and(|entity| entity.file_path == "lib.ts")
        })
        .count();

    assert_eq!(resolved_import_edges, IMPORTS);
    eprintln!(
        "bow_import_lookup_metric imports={IMPORTS} entities={} edges={} resolved_import_edges={resolved_import_edges} alloc_calls={alloc_calls} alloc_bytes={alloc_bytes} elapsed_ms={:.3}",
        graph.entities.len(),
        graph.edges.len(),
        elapsed.as_secs_f64() * 1000.0,
    );
}
