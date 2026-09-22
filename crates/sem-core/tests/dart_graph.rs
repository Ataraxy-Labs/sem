#![cfg(all(feature = "lang-dart", feature = "lang-typescript"))]
use sem_core::parser::{graph::EntityGraph, plugins::create_default_registry};

fn graph(files: &[(&str, &str)]) -> EntityGraph {
    let root = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for (path, content) in files {
        let dest = root.path().join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, content).unwrap();
        paths.push(path.to_string());
    }
    EntityGraph::build(root.path(), &paths, &create_default_registry()).0
}

const GREETER: &str = "class Greeter {\n  String hello(String name) => name;\n}\n";
const APP: &str =
    "import 'greeter.dart';\nvoid run() {\n final g = Greeter();\n print(g.hello('world'));\n}\n";

#[test]
fn imported_owner_wins_over_same_named_class() {
    let g = graph(&[
        ("a.ts", "export class Greeter { hello() {} }"),
        ("b.ts", "export class Greeter { hello() {} }"),
        ("app.ts", "import { Greeter } from './b';\nexport function run() { const g = new Greeter(); g.hello(); }"),
    ]);
    let deps = g.get_dependencies(&id(&g, "app.ts", "run"));
    assert!(
        deps.iter()
            .any(|e| e.name == "hello" && e.file_path == "b.ts"),
        "{deps:?}"
    );
    assert!(!deps.iter().any(|e| e.file_path == "a.ts"), "{deps:?}");
}

#[test]
fn javascript_typescript_interop_is_preserved() {
    let g = graph(&[
        ("greeter.js", "export class Greeter { hello() {} }"),
        ("app.ts", "import { Greeter } from './greeter';\nexport function run() { const g = new Greeter(); g.hello(); }"),
    ]);
    assert!(g
        .get_dependencies(&id(&g, "app.ts", "run"))
        .iter()
        .any(|e| e.name == "hello" && e.file_path == "greeter.js"));
}

fn id(g: &EntityGraph, file: &str, name: &str) -> String {
    g.entities
        .values()
        .find(|e| e.file_path == file && e.name == name)
        .unwrap_or_else(|| panic!("missing {file} {name}"))
        .id
        .to_string()
}

#[test]
fn dart_constructor_and_member_edges() {
    let g = graph(&[("greeter.dart", GREETER), ("app.dart", APP)]);
    let run = id(&g, "app.dart", "run");
    let deps = g.get_dependencies(&run);
    assert!(
        deps.iter().any(|e| e.name == "Greeter"),
        "constructor: {deps:?}"
    );
    assert!(deps.iter().any(|e| e.name == "hello"), "member: {deps:?}");
    let hello = id(&g, "greeter.dart", "hello");
    assert!(g
        .impact_analysis(&hello)
        .iter()
        .any(|e| e.id.as_str() == run));
}

#[test]
fn dart_explicit_new_and_const_constructor_edges() {
    for expression in ["new Greeter()", "const Greeter()"] {
        let source = APP.replace("Greeter()", expression);
        let g = graph(&[("greeter.dart", GREETER), ("app.dart", &source)]);
        let deps = g.get_dependencies(&id(&g, "app.dart", "run"));
        assert!(
            deps.iter().any(|e| e.name == "Greeter"),
            "{expression}: {deps:?}"
        );
        assert!(
            deps.iter().any(|e| e.name == "hello"),
            "{expression}: {deps:?}"
        );
    }
}

#[test]
fn dart_method_local_bindings_do_not_leak_to_other_methods() {
    let g = graph(&[("app.dart", "class Greeter {\n String hello() => 'hi';\n}\nclass App {\n void first() { final g = Greeter(); g.hello(); }\n void second(dynamic g) { g.hello(); }\n}\n")]);
    let callers = g.get_dependents(&id(&g, "app.dart", "hello"));
    assert!(callers.iter().any(|e| e.name == "first"), "{callers:?}");
    assert!(!callers.iter().any(|e| e.name == "second"), "{callers:?}");
}

#[test]
fn dart_and_typescript_same_names_do_not_cross_languages() {
    let g = graph(&[
        ("dart/greeter.dart", GREETER), ("dart/app.dart", APP),
        ("ts/greeter.ts", "export class Greeter { hello(name: string): string { return name; } }"),
        ("ts/app.ts", "import { Greeter } from './greeter';\nexport function run() { const g = new Greeter(); console.log(g.hello('world')); }"),
    ]);
    for (file, app) in [
        ("dart/greeter.dart", "dart/app.dart"),
        ("ts/greeter.ts", "ts/app.ts"),
    ] {
        let hello = id(&g, file, "hello");
        let callers = g.get_dependents(&hello);
        assert!(
            callers.iter().any(|e| e.file_path == app),
            "missing {app}: {callers:?}"
        );
        assert!(
            callers.iter().all(|e| e.file_path == app),
            "wrong caller: {callers:?}"
        );
    }
}

#[test]
fn dart_block_and_arrow_calls_and_typed_parameters() {
    let g = graph(&[("app.dart", "class Greeter {\n String hello() => 'hi';\n}\nvoid helper() {}\nvoid block() { helper(); }\nvoid arrow() => helper();\nvoid typed(Greeter g) { g.hello(); }\n")]);
    for name in ["block", "arrow"] {
        assert!(g
            .get_dependencies(&id(&g, "app.dart", name))
            .iter()
            .any(|e| e.name == "helper"));
    }
    assert!(g
        .get_dependencies(&id(&g, "app.dart", "typed"))
        .iter()
        .any(|e| e.name == "hello"));
}
