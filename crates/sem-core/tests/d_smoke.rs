#[cfg(feature = "lang-d")]
mod d_lang {
    use sem_core::parser::plugins::create_default_registry;

    #[test]
    fn d_extracts_all_entity_types() {
        let registry = create_default_registry();
        let d_code = r#"module mypkg.mymod;

import std.stdio;

enum Color {
    Red,
    Green,
    Blue,
}

struct Point {
    int x;
    int y;
}

union Value {
    int i;
    float f;
}

interface Greeter {
    string greet();
}

class HelloGreeter : Greeter {
    string name;

    this(string name) {
        this.name = name;
    }

    ~this() {}

    string greet() {
        return "Hello, " ~ name;
    }
}

template Repeat(T, size_t N) {
    alias Repeat = T[N];
}

mixin template Logger() {
    void log(string s) { writeln(s); }
}

int add(int a, int b) {
    return a + b;
}

void main() {
    auto g = new HelloGreeter("world");
    writeln(g.greet());
}

unittest {
    assert(add(1, 2) == 3);
}
"#;

        let entities = registry.extract_entities("hello.d", d_code);
        assert!(!entities.is_empty(), "Should extract entities from D code");

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "D entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"Color"),
            "Should find enum Color, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Point"),
            "Should find struct Point, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Value"),
            "Should find union Value, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Greeter"),
            "Should find interface Greeter, got: {:?}",
            names
        );
        assert!(
            names.contains(&"HelloGreeter"),
            "Should find class HelloGreeter, got: {:?}",
            names
        );
        assert!(
            names.contains(&"greet"),
            "Should find method greet, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Repeat"),
            "Should find template Repeat, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Logger"),
            "Should find mixin template Logger, got: {:?}",
            names
        );
        assert!(
            names.contains(&"add"),
            "Should find function add, got: {:?}",
            names
        );
        assert!(
            names.contains(&"main"),
            "Should find function main, got: {:?}",
            names
        );

        for entity in &entities {
            assert_eq!(entity.file_path, "hello.d");
        }
    }

    #[test]
    fn d_local_variables_not_extracted_as_top_level() {
        let registry = create_default_registry();
        let d_code = r#"module test;

int compute(int x) {
    int local = x * 2;
    auto another = local + 1;
    return another;
}
"#;

        let entities = registry.extract_entities("test.d", d_code);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("D entities (locals): {:?}", names);

        assert!(
            names.contains(&"compute"),
            "Should find top-level compute, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"local"),
            "Should not extract local variable 'local', got: {:?}",
            names
        );
        assert!(
            !names.contains(&"another"),
            "Should not extract local variable 'another', got: {:?}",
            names
        );
    }
}
