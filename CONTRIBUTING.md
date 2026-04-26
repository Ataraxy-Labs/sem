# Contributing to sem

## Adding a New Language

sem uses [tree-sitter](https://tree-sitter.github.io/tree-sitter/) grammars to extract semantic entities (functions, classes, etc.) from source code. Adding a new language is straightforward: you define a config struct and add a cargo dependency. No parser code needed.

This guide walks through the process step by step.

### Overview

All language support lives in two files:

- `crates/sem-core/Cargo.toml` (tree-sitter grammar dependency)
- `crates/sem-core/src/parser/plugins/code/languages.rs` (language config)

Each language gets a `LanguageConfig` that tells sem which AST node types represent code entities.

### Step 1: Add the tree-sitter dependency

Add the grammar crate to `crates/sem-core/Cargo.toml`:

```toml
[dependencies]
tree-sitter-scala = "0.23"
```

Most grammars are published on crates.io as `tree-sitter-{lang}`. Check [crates.io](https://crates.io/search?q=tree-sitter) for the latest version. The `0.23` series works with tree-sitter `0.26`.

### Step 2: Add a getter function

In `languages.rs`, add a function that returns the tree-sitter `Language`:

```rust
fn get_scala() -> Option<Language> {
    Some(tree_sitter_scala::LANGUAGE.into())
}
```

Some crates export the language differently. Check the crate's docs. Common patterns:

- `tree_sitter_python::LANGUAGE` (most languages)
- `tree_sitter_typescript::LANGUAGE_TYPESCRIPT` (when a crate has multiple grammars)
- `tree_sitter_php::LANGUAGE_PHP` (same)

### Step 3: Define the language config

Add a static config in `languages.rs`:

```rust
static SCALA_CONFIG: LanguageConfig = LanguageConfig {
    id: "scala",
    extensions: &[".scala", ".sc"],
    entity_node_types: &[
        "function_definition",
        "class_definition",
        "object_definition",
        "trait_definition",
        "val_definition",
        "var_definition",
        "type_definition",
    ],
    container_node_types: &["template_body", "block"],
    call_entity_identifiers: &[],
    suppressed_nested_entities: &[],
    get_language: get_scala,
};
```

### Step 4: Register the config

Add a reference to the `ALL_CONFIGS` array:

```rust
static ALL_CONFIGS: &[&LanguageConfig] = &[
    // ... existing configs ...
    &SCALA_CONFIG,
];
```

### Step 5: Register the extensions

Add all file extensions to `get_all_code_extensions()`:

```rust
static EXTENSIONS: &[&str] = &[
    // ... existing extensions ...
    ".scala", ".sc",
];
```

### Step 6: Add a test

Add a test in `crates/sem-core/src/parser/plugins/code/mod.rs`:

```rust
#[test]
fn test_scala_entity_extraction() {
    let code = r#"
class UserService {
  def getUsers(): List[User] = {
    db.findAll()
  }
}

object AppConfig {
  val version = "1.0"
}

trait Repository[T] {
  def findById(id: String): Option[T]
}
"#;
    let plugin = CodeParserPlugin;
    let entities = plugin.extract_entities(code, "UserService.scala");
    let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
    eprintln!("Scala entities: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());

    assert!(names.contains(&"UserService"), "got: {:?}", names);
    assert!(names.contains(&"AppConfig"), "got: {:?}", names);
    assert!(names.contains(&"Repository"), "got: {:?}", names);
}
```

### Step 7: Run tests

```bash
cd crates && cargo test
```

All existing tests plus your new test should pass.

## LanguageConfig Fields

### `entity_node_types`

The tree-sitter AST node types that represent top-level code entities. These are the things sem tracks: functions, classes, interfaces, etc.

For most languages, this is all you need. Examples:

| Language | Node types |
|----------|-----------|
| Python | `function_definition`, `class_definition`, `decorated_definition` |
| Rust | `function_item`, `struct_item`, `enum_item`, `impl_item`, `trait_item` |
| Go | `function_declaration`, `method_declaration`, `type_declaration` |

### `container_node_types`

AST nodes that can contain nested entities. When sem finds a container, it looks inside for child entities and sets up parent-child relationships.

For example, in Java a `class_body` contains method declarations. Setting `container_node_types: &["class_body"]` lets sem extract methods as children of the class.

Common containers: `block`, `class_body`, `declaration_list`, `compound_statement`.

### `call_entity_identifiers`

For languages where entities are defined via function calls rather than syntax. Elixir is the primary example:

```elixir
defmodule MyApp do    # "defmodule" is a call, not a keyword
  def greet(name) do  # "def" is a call
    "Hello #{name}"
  end
end
```

Set `entity_node_types` to `&[]` and list the call identifiers instead:

```rust
call_entity_identifiers: &["defmodule", "def", "defp", "defmacro", ...],
```

Most languages don't need this. Leave it as `&[]`.

### `suppressed_nested_entities`

Prevents double-extraction when a child entity type shouldn't be extracted inside a parent entity type. Used by HCL to suppress nested `attribute` nodes inside `block` nodes (since the block already captures that content).

```rust
suppressed_nested_entities: &[SuppressedNestedEntity {
    parent_entity_node_type: "block",
    child_entity_node_type: "attribute",
}],
```

Most languages don't need this. Leave it as `&[]`.

## Finding the Right Node Types

The hardest part is figuring out which AST node types your language uses. Here's how:

### Option 1: Tree-sitter Playground

Go to [tree-sitter.github.io/tree-sitter/playground](https://tree-sitter.github.io/tree-sitter/playground). Paste some sample code and look at the parse tree. The node type names in the tree are exactly what you put in `entity_node_types`.

### Option 2: Check the grammar repo

Every tree-sitter grammar has a `grammar.js` or `src/node-types.json` in its repo. Search for the node types you need. The GitHub repos are usually at `tree-sitter/tree-sitter-{lang}` or `tree-sitter-grammars/tree-sitter-{lang}`.

### Option 3: Use `tree-sitter parse`

If you have the tree-sitter CLI installed:

```bash
tree-sitter parse sample.scala
```

This prints the full AST with node types.

### Tips

- Start with the obvious ones: `function_definition`, `class_definition`, etc.
- Use `eprintln!` in your test to see what entities are extracted. The existing tests all do this.
- If something isn't extracted, the node type name is probably different. Check the AST.
- If too many things are extracted, you may be including container nodes or low-level syntax.

## Questions?

Open an issue or check the existing language configs in `languages.rs` for reference. The simplest configs (Python, Bash) are good starting points. The Elixir config shows the call-based approach.
