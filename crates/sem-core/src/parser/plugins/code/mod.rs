mod entity_extractor;
mod languages;

use std::cell::RefCell;
use std::collections::HashMap;

use crate::model::entity::SemanticEntity;
use crate::parser::plugin::SemanticParserPlugin;
use languages::{get_all_code_extensions, get_language_config};
use entity_extractor::extract_entities;

pub struct CodeParserPlugin;

// Thread-local parser cache: one Parser per language per thread.
// Avoids creating a new Parser for every file during parallel graph builds.
thread_local! {
    static PARSER_CACHE: RefCell<HashMap<&'static str, tree_sitter::Parser>> = RefCell::new(HashMap::new());
}

impl SemanticParserPlugin for CodeParserPlugin {
    fn id(&self) -> &str {
        "code"
    }

    fn extensions(&self) -> &[&str] {
        get_all_code_extensions()
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let ext = std::path::Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_default();

        let config = match get_language_config(&ext) {
            Some(c) => c,
            None => return Vec::new(),
        };

        let language = match (config.get_language)() {
            Some(lang) => lang,
            None => return Vec::new(),
        };

        PARSER_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let parser = cache.entry(config.id).or_insert_with(|| {
                let mut p = tree_sitter::Parser::new();
                let _ = p.set_language(&language);
                p
            });

            let tree = match parser.parse(content.as_bytes(), None) {
                Some(t) => t,
                None => return Vec::new(),
            };

            extract_entities(&tree, file_path, config, content)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_java_entity_extraction() {
        let code = r#"
package com.example;

import java.util.List;

public class UserService {
    private String name;

    public UserService(String name) {
        this.name = name;
    }

    public List<User> getUsers() {
        return db.findAll();
    }

    public void createUser(User user) {
        db.save(user);
    }
}

interface Repository<T> {
    T findById(String id);
    List<T> findAll();
}

enum Status {
    ACTIVE,
    INACTIVE,
    DELETED
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "UserService.java");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("Java entities: {:?}", names.iter().zip(types.iter()).collect::<Vec<_>>());

        assert!(names.contains(&"UserService"), "Should find class UserService, got: {:?}", names);
        assert!(names.contains(&"Repository"), "Should find interface Repository, got: {:?}", names);
        assert!(names.contains(&"Status"), "Should find enum Status, got: {:?}", names);
    }

    #[test]
    fn test_java_nested_methods() {
        let code = r#"
public class Calculator {
    public int add(int a, int b) {
        return a + b;
    }

    public int subtract(int a, int b) {
        return a - b;
    }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Calculator.java");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("Java nested: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type, &e.parent_id)).collect::<Vec<_>>());

        assert!(names.contains(&"Calculator"), "Should find Calculator class");
        assert!(names.contains(&"add"), "Should find add method, got: {:?}", names);
        assert!(names.contains(&"subtract"), "Should find subtract method, got: {:?}", names);

        // Methods should have Calculator as parent
        let add = entities.iter().find(|e| e.name == "add").unwrap();
        assert!(add.parent_id.is_some(), "add should have parent_id");
    }

    #[test]
    fn test_c_entity_extraction() {
        let code = r#"
#include <stdio.h>

struct Point {
    int x;
    int y;
};

enum Color {
    RED,
    GREEN,
    BLUE
};

typedef struct {
    char name[50];
    int age;
} Person;

void greet(const char* name) {
    printf("Hello, %s!\n", name);
}

int add(int a, int b) {
    return a + b;
}

int main() {
    greet("world");
    return 0;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.c");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("C entities: {:?}", names.iter().zip(types.iter()).collect::<Vec<_>>());

        assert!(names.contains(&"greet"), "Should find greet function, got: {:?}", names);
        assert!(names.contains(&"add"), "Should find add function, got: {:?}", names);
        assert!(names.contains(&"main"), "Should find main function, got: {:?}", names);
        assert!(names.contains(&"Point"), "Should find Point struct, got: {:?}", names);
        assert!(names.contains(&"Color"), "Should find Color enum, got: {:?}", names);
    }

    #[test]
    fn test_cpp_entity_extraction() {
        let code = "namespace math {\nclass Vector3 {\npublic:\n    float length() const { return 0; }\n};\n}\nvoid greet() {}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.cpp");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"math"), "got: {:?}", names);
        assert!(names.contains(&"Vector3"), "got: {:?}", names);
        assert!(names.contains(&"greet"), "got: {:?}", names);
    }

    #[test]
    fn test_ruby_entity_extraction() {
        let code = "module Auth\n  class User\n    def greet\n      \"hi\"\n    end\n  end\nend\ndef helper(x)\n  x * 2\nend\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "auth.rb");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Auth"), "got: {:?}", names);
        assert!(names.contains(&"User"), "got: {:?}", names);
        assert!(names.contains(&"helper"), "got: {:?}", names);
    }

    #[test]
    fn test_csharp_entity_extraction() {
        let code = "namespace MyApp {\npublic class User {\n    public string GetName() { return \"\"; }\n}\npublic enum Role { Admin, User }\n}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Models.cs");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"MyApp"), "got: {:?}", names);
        assert!(names.contains(&"User"), "got: {:?}", names);
        assert!(names.contains(&"Role"), "got: {:?}", names);
    }

    #[test]
    fn test_swift_entity_extraction() {
        let code = r#"
import Foundation

class UserService {
    var name: String

    init(name: String) {
        self.name = name
    }

    func getUsers() -> [User] {
        return db.findAll()
    }
}

struct Point {
    var x: Double
    var y: Double
}

enum Status {
    case active
    case inactive
    case deleted
}

protocol Repository {
    associatedtype Item
    func findById(id: String) -> Item?
    func findAll() -> [Item]
}

func helper(x: Int) -> Int {
    return x * 2
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "UserService.swift");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("Swift entities: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());

        assert!(names.contains(&"UserService"), "Should find class UserService, got: {:?}", names);
        assert!(names.contains(&"Point"), "Should find struct Point, got: {:?}", names);
        assert!(names.contains(&"Status"), "Should find enum Status, got: {:?}", names);
        assert!(names.contains(&"Repository"), "Should find protocol Repository, got: {:?}", names);
        assert!(names.contains(&"helper"), "Should find function helper, got: {:?}", names);
    }

    #[test]
    fn test_elixir_entity_extraction() {
        let code = r#"
defmodule MyApp.Accounts do
  def create_user(attrs) do
    %User{}
    |> User.changeset(attrs)
    |> Repo.insert()
  end

  defp validate(attrs) do
    # private helper
    :ok
  end

  defmacro is_admin(user) do
    quote do
      unquote(user).role == :admin
    end
  end

  defguard is_positive(x) when is_integer(x) and x > 0
end

defprotocol Printable do
  def to_string(data)
end

defimpl Printable, for: Integer do
  def to_string(i), do: Integer.to_string(i)
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "accounts.ex");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("Elixir entities: {:?}", names.iter().zip(types.iter()).collect::<Vec<_>>());

        assert!(names.contains(&"MyApp.Accounts"), "Should find module, got: {:?}", names);
        assert!(names.contains(&"create_user"), "Should find def, got: {:?}", names);
        assert!(names.contains(&"validate"), "Should find defp, got: {:?}", names);
        assert!(names.contains(&"is_admin"), "Should find defmacro, got: {:?}", names);
        assert!(names.contains(&"Printable"), "Should find defprotocol, got: {:?}", names);

        // Verify nesting: create_user should have MyApp.Accounts as parent
        let create_user = entities.iter().find(|e| e.name == "create_user").unwrap();
        assert!(create_user.parent_id.is_some(), "create_user should be nested under module");
    }

    #[test]
    fn test_bash_entity_extraction() {
        let code = r#"#!/bin/bash

greet() {
    echo "Hello, $1!"
}

function deploy {
    echo "deploying..."
}

# not a function
echo "main script"
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "deploy.sh");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("Bash entities: {:?}", names.iter().zip(types.iter()).collect::<Vec<_>>());

        assert!(names.contains(&"greet"), "Should find greet(), got: {:?}", names);
        assert!(names.contains(&"deploy"), "Should find function deploy, got: {:?}", names);
        assert_eq!(entities.len(), 2, "Should only find functions, got: {:?}", names);
    }

    #[test]
    fn test_typescript_entity_extraction() {
        // Existing language should still work
        let code = r#"
export function hello(): string {
    return "hello";
}

export class Greeter {
    greet(name: string): string {
        return `Hello, ${name}!`;
    }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"hello"), "Should find hello function");
        assert!(names.contains(&"Greeter"), "Should find Greeter class");
    }

    #[test]
    fn test_module_typescript_entity_extraction() {
        let code = r#"
export function hello(): string {
    return "hello";
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.mts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"hello"), "Should find hello function");
    }

    #[test]
    fn test_commonjs_typescript_entity_extraction() {
        let code = r#"
export class Greeter {
    greet(name: string): string {
        return `Hello, ${name}!`;
    }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.cts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"Greeter"), "Should find Greeter class");
        assert!(names.contains(&"greet"), "Should find greet method");
    }

    #[test]
    fn test_nested_functions_typescript() {
        let code = r#"
function outer() {
    function inner() {
        return 42;
    }
    return inner();
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("Nested TS: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type, &e.parent_id)).collect::<Vec<_>>());

        assert!(names.contains(&"outer"), "Should find outer, got: {:?}", names);
        assert!(names.contains(&"inner"), "Should find inner, got: {:?}", names);

        let inner = entities.iter().find(|e| e.name == "inner").unwrap();
        assert!(inner.parent_id.is_some(), "inner should have parent_id");
    }

    #[test]
    fn test_nested_functions_python() {
        let code = "def outer():\n    def inner():\n        return 42\n    return inner()\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.py");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
        assert!(names.contains(&"inner"), "got: {:?}", names);

        let inner = entities.iter().find(|e| e.name == "inner").unwrap();
        assert!(inner.parent_id.is_some(), "inner should have parent_id");
    }

    #[test]
    fn test_nested_functions_rust() {
        let code = "fn outer() {\n    fn inner() -> i32 {\n        42\n    }\n    inner();\n}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.rs");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
        assert!(names.contains(&"inner"), "got: {:?}", names);

        let inner = entities.iter().find(|e| e.name == "inner").unwrap();
        assert!(inner.parent_id.is_some(), "inner should have parent_id");
    }

    #[test]
    fn test_rust_impl_blocks_unique_names() {
        let code = r#"
trait Greeting {
    fn greet(&self) -> String;
}

struct Person;
struct Robot;
struct Cat;

impl Greeting for Person {
    fn greet(&self) -> String { "Hello".to_string() }
}

impl Greeting for Robot {
    fn greet(&self) -> String { "Beep".to_string() }
}

impl Greeting for Cat {
    fn greet(&self) -> String { "Meow".to_string() }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "impls.rs");
        let impl_entities: Vec<&_> = entities.iter()
            .filter(|e| e.entity_type == "impl")
            .collect();
        let names: Vec<&str> = impl_entities.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(impl_entities.len(), 3, "Should find 3 impl blocks, got: {:?}", names);
        assert!(names.contains(&"Greeting for Person"), "got: {:?}", names);
        assert!(names.contains(&"Greeting for Robot"), "got: {:?}", names);
        assert!(names.contains(&"Greeting for Cat"), "got: {:?}", names);
    }

    #[test]
    fn test_nested_functions_go() {
        // Go doesn't have named nested functions, but has nested type/var declarations
        let code = "package main\n\nfunc outer() {\n    var x int = 42\n    _ = x\n}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
    }

    #[test]
    fn test_renamed_function_same_structural_hash() {
        let code_a = "def get_card():\n    return db.query('cards')\n";
        let code_b = "def get_card_1():\n    return db.query('cards')\n";

        let plugin = CodeParserPlugin;
        let entities_a = plugin.extract_entities(code_a, "a.py");
        let entities_b = plugin.extract_entities(code_b, "b.py");

        assert_eq!(entities_a.len(), 1, "Should find one entity in a");
        assert_eq!(entities_b.len(), 1, "Should find one entity in b");
        assert_eq!(entities_a[0].name, "get_card");
        assert_eq!(entities_b[0].name, "get_card_1");

        // Structural hash should match since only the name differs
        assert_eq!(
            entities_a[0].structural_hash, entities_b[0].structural_hash,
            "Renamed function with identical body should have same structural_hash"
        );

        // Content hash should differ (it includes the name)
        assert_ne!(
            entities_a[0].content_hash, entities_b[0].content_hash,
            "Content hash should differ since raw content includes the name"
        );
    }

    #[test]
    fn test_hcl_entity_extraction() {
        let code = r#"
region = "eu-west-1"

variable "image_id" {
  type = string
}

resource "aws_instance" "web" {
  ami = var.image_id

  lifecycle {
    create_before_destroy = true
  }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.tf");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("HCL entities: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type, &e.parent_id)).collect::<Vec<_>>());

        assert!(names.contains(&"region"), "Should find top-level attribute, got: {:?}", names);
        assert!(names.contains(&"variable.image_id"), "Should find variable block, got: {:?}", names);
        assert!(names.contains(&"resource.aws_instance.web"), "Should find resource block, got: {:?}", names);
        assert!(
            names.contains(&"resource.aws_instance.web.lifecycle"),
            "Should find nested lifecycle block with qualified name, got: {:?}",
            names
        );
        assert!(!names.contains(&"ami"), "Should skip nested attributes inside blocks, got: {:?}", names);
        assert!(
            !names.contains(&"create_before_destroy"),
            "Should skip nested attributes inside nested blocks, got: {:?}",
            names
        );

        let lifecycle = entities
            .iter()
            .find(|e| e.name == "resource.aws_instance.web.lifecycle")
            .unwrap();
        assert!(lifecycle.parent_id.is_some(), "lifecycle should be nested under resource");
        assert!(types.contains(&"attribute"), "Should preserve attribute entity type for top-level attributes");
    }

    #[test]
    fn test_kotlin_entity_extraction() {
        let code = r#"
class UserService {
    val name: String = ""

    fun greet(): String {
        return "Hello, $name"
    }

    companion object {
        fun create(): UserService = UserService()
    }
}

interface Repository {
    fun findById(id: Int): Any?
}

object AppConfig {
    val version = "1.0"
}

fun topLevel(x: Int): Int = x * 2
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "App.kt");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("Kotlin entities: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert!(names.contains(&"UserService"), "got: {:?}", names);
        assert!(names.contains(&"greet"), "got: {:?}", names);
        assert!(names.contains(&"Repository"), "got: {:?}", names);
        assert!(names.contains(&"findById"), "got: {:?}", names);
        assert!(names.contains(&"AppConfig"), "got: {:?}", names);
        assert!(names.contains(&"topLevel"), "got: {:?}", names);
    }

    #[test]
    fn test_xml_entity_extraction() {
        let code = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>my-app</artifactId>
    <dependencies>
        <dependency>
            <groupId>junit</groupId>
            <artifactId>junit</artifactId>
        </dependency>
    </dependencies>
    <build>
        <plugins>
            <plugin>
                <groupId>org.apache.maven</groupId>
            </plugin>
        </plugins>
    </build>
</project>
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "pom.xml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("XML entities: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert!(names.contains(&"project"), "got: {:?}", names);
        assert!(names.contains(&"dependencies"), "got: {:?}", names);
        assert!(names.contains(&"build"), "got: {:?}", names);
    }

    #[test]
    fn test_arrow_callback_scope_boundary_typescript() {
        // Arrow function callbacks: locals are suppressed, but inner
        // class/function declarations are still extracted. Nested callbacks
        // also suppress their locals.
        let code = r#"
const activeQueues = [
  { queue: queues.fooQueue, processor: foo.process },
];

activeQueues.forEach((handler: any) => {
  const queue = handler.queue;
  let retries = 0;

  class QueueHandler {
    handle() { return queue; }
  }

  function createHandler() {
    return new QueueHandler();
  }

  queue.process((job) => {
    const orderId = job.data.orderId;
    return orderId;
  });
});

function handleFailure(job: any, err: any) {
  console.error('failed', err);
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "process.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let top_level: Vec<&str> = entities
            .iter()
            .filter(|e| e.parent_id.is_none())
            .map(|e| e.name.as_str())
            .collect();

        // Top-level entities preserved
        assert!(top_level.contains(&"activeQueues"), "got: {:?}", top_level);
        assert!(top_level.contains(&"handleFailure"), "got: {:?}", top_level);

        // Declarations inside callback extracted
        assert!(names.contains(&"QueueHandler"), "got: {:?}", names);
        assert!(names.contains(&"handle"), "got: {:?}", names);
        assert!(names.contains(&"createHandler"), "got: {:?}", names);

        // Locals inside callbacks suppressed
        assert!(!names.contains(&"queue"), "got: {:?}", names);
        assert!(!names.contains(&"retries"), "got: {:?}", names);
        assert!(!names.contains(&"orderId"), "got: {:?}", names);
    }

    #[test]
    fn test_top_level_iife_wrapper_still_extracts_typescript_entities() {
        let code = r#"
function factory() {
  class Foo {
    method(): number {
      return 1;
    }
  }

  function bar(): Foo {
    return new Foo();
  }
}

factory();
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "wrapped.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"factory"),
            "Should find top-level wrapper function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Foo"),
            "Should find class inside top-level wrapper, got: {:?}",
            names
        );
        assert!(
            names.contains(&"bar"),
            "Should find function inside top-level wrapper, got: {:?}",
            names
        );
    }

    #[test]
    fn test_top_level_iife_still_extracts_typescript_entities() {
        let code = r#"
(() => {
  class Foo {
    method(): number {
      return 1;
    }
  }

  function bar(): Foo {
    return new Foo();
  }
})();
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "iife.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"Foo"),
            "Should find class inside top-level IIFE, got: {:?}",
            names
        );
        assert!(
            names.contains(&"bar"),
            "Should find function inside top-level IIFE, got: {:?}",
            names
        );
    }

    #[test]
    fn test_function_locals_not_extracted_as_nested_entities_typescript() {
        let code = r#"
export default function foo() {
  const x = 1;
  return x;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "default-export.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"foo"),
            "Should find exported function, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"x"),
            "Local inside function should not be extracted as an entity, got: {:?}",
            names
        );
    }

    #[test]
    fn test_function_expression_scope_boundary_typescript() {
        // Function expressions: assigned to variables, or used as callback
        // arguments. Locals are suppressed in all cases.
        let code = r#"
const foo = function namedExpr(x: number) {
  const inner = x + 1;
  return inner;
};

const bar = function(y: number) {
  const local = y * 2;
  return local;
};

const items = [1, 2, 3];

items.forEach(function process(item) {
  const doubled = item * 2;
  console.log(doubled);
});
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "funexpr.ts");
        let top_level: Vec<&str> = entities
            .iter()
            .filter(|e| e.parent_id.is_none())
            .map(|e| e.name.as_str())
            .collect();
        let find = |name: &str| entities.iter().find(|e| e.name == name).unwrap();
        let all_names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Top-level declarations preserved, and const-assigned function
        // expressions are promoted from variable to function.
        assert!(top_level.contains(&"foo"), "got: {:?}", top_level);
        assert!(top_level.contains(&"bar"), "got: {:?}", top_level);
        assert!(top_level.contains(&"items"), "got: {:?}", top_level);
        assert_eq!(find("foo").entity_type, "function");
        assert_eq!(find("bar").entity_type, "function");
        assert_eq!(find("items").entity_type, "variable");

        // Locals inside function expressions suppressed
        assert!(!all_names.contains(&"inner"), "got: {:?}", all_names);
        assert!(!all_names.contains(&"local"), "got: {:?}", all_names);
        assert!(!all_names.contains(&"doubled"), "got: {:?}", all_names);

        // Named function expression used as callback argument not extracted
        assert!(!top_level.contains(&"process"), "got: {:?}", top_level);
    }

    #[test]
    fn test_variable_assigned_arrow_extracts_inner_entities() {
        // Arrow function assigned to a variable: inner class/function
        // declarations should be extracted, locals should be suppressed.
        let code = r#"
const handler = () => {
  class Inner {
    run() { return 1; }
  }

  function make() {
    return new Inner();
  }

  const local = 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "assigned.ts");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(handler.entity_type, "function");
        assert!(names.contains(&"handler"), "got: {:?}", names);
        assert!(names.contains(&"Inner"), "got: {:?}", names);
        assert!(names.contains(&"run"), "got: {:?}", names);
        assert!(names.contains(&"make"), "got: {:?}", names);
        assert!(!names.contains(&"local"), "got: {:?}", names);
    }

    #[test]
    fn test_variable_assigned_function_expression_extracts_inner_entities() {
        // Function expression assigned to a variable: same behavior.
        let code = r#"
const handler = function() {
  class Inner {}
  function make() { return new Inner(); }
  const local = 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "funexpr-inner.ts");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(handler.entity_type, "function");
        assert!(names.contains(&"handler"), "got: {:?}", names);
        assert!(names.contains(&"Inner"), "got: {:?}", names);
        assert!(names.contains(&"make"), "got: {:?}", names);
        assert!(!names.contains(&"local"), "got: {:?}", names);
    }

    #[test]
    fn test_let_assigned_arrow_stays_variable_typescript() {
        let code = r#"
let handler = () => {
  return 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "let-assigned.ts");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();

        assert_eq!(handler.entity_type, "variable");
    }

    #[test]
    fn test_const_assigned_arrow_promoted_to_function_javascript() {
        let code = r#"
const handler = () => {
  return 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "handler.js");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();

        assert_eq!(handler.entity_type, "function");
    }

    #[test]
    fn test_go_var_declaration() {
        let code = r#"package featuremgmt

type FeatureFlag struct {
	Name        string
	Description string
	Stage       string
}

var standardFeatureFlags = []FeatureFlag{
	{
		Name:        "panelTitleSearch",
		Description: "Search for dashboards using panel title",
		Stage:       "PublicPreview",
	},
}

func GetFlags() []FeatureFlag {
	return standardFeatureFlags
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "flags.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("Go entities: {:?}", names.iter().zip(types.iter()).collect::<Vec<_>>());

        assert!(names.contains(&"FeatureFlag"), "Should find type FeatureFlag, got: {:?}", names);
        assert!(names.contains(&"standardFeatureFlags"), "Should find var standardFeatureFlags, got: {:?}", names);
        assert!(names.contains(&"GetFlags"), "Should find func GetFlags, got: {:?}", names);
    }

    #[test]
    fn test_go_grouped_var_declaration() {
        let code = r#"package test

var (
	simple = 42
	flags = []string{"a", "b"}
)

const (
	x = 1
	y = 2
)

func main() {}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("Go grouped entities: {:?}", names.iter().zip(types.iter()).collect::<Vec<_>>());

        assert!(names.contains(&"flags") || names.contains(&"simple"), "Should find grouped var, got: {:?}", names);
        assert!(names.contains(&"x"), "Should find grouped const x, got: {:?}", names);
        assert!(names.contains(&"main"), "Should find func main, got: {:?}", names);
    }

    #[test]
    fn test_dart_entity_extraction() {
        let code = r#"
import 'dart:math';

class Calculator {
  final String name;

  Calculator(this.name);

  Calculator.withDefault() : name = 'default';

  factory Calculator.create(String name) {
    return Calculator(name);
  }

  int add(int a, int b) {
    return a + b;
  }

  int get doubleAdd => add(1, 1) * 2;

  set label(String value) {
    // no-op
  }

  int operator +(Calculator other) {
    return 0;
  }
}

mixin Loggable {
  void log(String message) {
    print(message);
  }
}

extension StringExt on String {
  bool get isBlank => trim().isEmpty;
}

enum Status {
  active,
  inactive;

  String display() => name.toUpperCase();
}

typedef Callback = void Function(int);

int add(int a, int b) {
  return a + b;
}

extension type Wrapper(int value) implements int {}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "calculator.dart");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Dart entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        // Top-level declarations
        assert!(names.contains(&"Calculator"), "Should find class, got: {:?}", names);
        assert!(names.contains(&"Loggable"), "Should find mixin, got: {:?}", names);
        assert!(names.contains(&"StringExt"), "Should find extension, got: {:?}", names);
        assert!(names.contains(&"Status"), "Should find enum, got: {:?}", names);
        assert!(names.contains(&"Callback"), "Should find typedef, got: {:?}", names);
        assert!(names.contains(&"add"), "Should find top-level function, got: {:?}", names);
        assert!(names.contains(&"Wrapper"), "Should find extension type, got: {:?}", names);

        // Class members with correct types
        let add_method = entities.iter().find(|e| e.name == "add" && e.parent_id.is_some());
        assert!(add_method.is_some(), "Should find add method inside Calculator");
        assert_eq!(add_method.unwrap().entity_type, "method");

        // Named constructor gets distinct name from unnamed constructor
        let unnamed_ctor = entities.iter().find(|e| e.name == "Calculator" && e.entity_type == "constructor");
        assert!(unnamed_ctor.is_some(), "Should find unnamed constructor");
        let named_ctor = entities.iter().find(|e| e.name == "Calculator.withDefault");
        assert!(named_ctor.is_some(), "Should find named constructor Calculator.withDefault, got: {:?}", names);
        assert_eq!(named_ctor.unwrap().entity_type, "constructor");
        assert_ne!(unnamed_ctor.unwrap().id, named_ctor.unwrap().id, "Named and unnamed constructors must have different entity IDs");

        // Factory constructor
        let factory_ctor = entities.iter().find(|e| e.name == "Calculator.create");
        assert!(factory_ctor.is_some(), "Should find factory constructor Calculator.create, got: {:?}", names);
        assert_eq!(factory_ctor.unwrap().entity_type, "constructor");

        // Getter, setter, operator
        let getter = entities.iter().find(|e| e.name == "doubleAdd");
        assert!(getter.is_some(), "Should find getter doubleAdd");
        assert_eq!(getter.unwrap().entity_type, "getter");

        let setter = entities.iter().find(|e| e.name == "label");
        assert!(setter.is_some(), "Should find setter label");
        assert_eq!(setter.unwrap().entity_type, "setter");

        let operator = entities.iter().find(|e| e.name == "operator +");
        assert!(operator.is_some(), "Should find operator +");
        assert_eq!(operator.unwrap().entity_type, "method");

        // Mixin members have parent
        let log_method = entities.iter().find(|e| e.name == "log");
        assert!(log_method.is_some(), "Should find log in Loggable");
        assert!(log_method.unwrap().parent_id.is_some(), "log should have parent_id");

        // Entity type mapping
        let callback = entities.iter().find(|e| e.name == "Callback").unwrap();
        assert_eq!(callback.entity_type, "type", "typedef should map to 'type'");

        let loggable = entities.iter().find(|e| e.name == "Loggable").unwrap();
        assert_eq!(loggable.entity_type, "mixin");

        let ext = entities.iter().find(|e| e.name == "StringExt").unwrap();
        assert_eq!(ext.entity_type, "extension");

        let wrapper = entities.iter().find(|e| e.name == "Wrapper").unwrap();
        assert_eq!(wrapper.entity_type, "extension");
    }

    #[test]
    fn test_dart_top_level_function_includes_body() {
        let code = r#"
int add(int a, int b) {
  return a + b;
}

String greet(String name) => 'Hello, $name!';
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "funcs.dart");
        eprintln!(
            "Dart top-level: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.content))
                .collect::<Vec<_>>()
        );

        let add_fn = entities.iter().find(|e| e.name == "add").unwrap();
        assert!(
            add_fn.content.contains("return a + b"),
            "Top-level function content should include the body, got: {:?}",
            add_fn.content
        );

        let greet_fn = entities.iter().find(|e| e.name == "greet").unwrap();
        assert!(
            greet_fn.content.contains("Hello"),
            "Expression body should be included, got: {:?}",
            greet_fn.content
        );

        // Body changes should produce different content_hash
        let code_v2 = r#"
int add(int a, int b) {
  return a * b;
}

String greet(String name) => 'Hello, $name!';
"#;
        let entities_v2 = plugin.extract_entities(code_v2, "funcs.dart");
        let add_v2 = entities_v2.iter().find(|e| e.name == "add").unwrap();
        assert_ne!(
            add_fn.content_hash, add_v2.content_hash,
            "Body change should produce different content_hash"
        );

        // Unchanged function should keep the same hash
        let greet_v2 = entities_v2.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(
            greet_fn.content_hash, greet_v2.content_hash,
            "Unchanged function should keep the same content_hash"
        );
    }

    #[test]
    fn test_dart_renamed_named_constructor_same_structural_hash() {
        let code_a = r#"
class Foo {
  Foo.fromJson(Map<String, dynamic> json) {
    print(json);
  }
}
"#;
        let code_b = r#"
class Foo {
  Foo.fromMap(Map<String, dynamic> json) {
    print(json);
  }
}
"#;
        let plugin = CodeParserPlugin;
        let entities_a = plugin.extract_entities(code_a, "a.dart");
        let entities_b = plugin.extract_entities(code_b, "b.dart");

        let ctor_a = entities_a.iter().find(|e| e.name == "Foo.fromJson").unwrap();
        let ctor_b = entities_b.iter().find(|e| e.name == "Foo.fromMap").unwrap();

        assert_eq!(
            ctor_a.structural_hash, ctor_b.structural_hash,
            "Renamed named constructor with identical body should have same structural_hash"
        );
        assert_ne!(
            ctor_a.content_hash, ctor_b.content_hash,
            "Content hash should differ since raw content includes the name"
        );
    }

    #[test]
    fn test_dart_top_level_getter_setter() {
        let code = r#"
int _value = 0;

int get currentValue {
  return _value;
}

set currentValue(int v) {
  _value = v;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "accessors.dart");
        eprintln!(
            "Dart top-level accessors: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.content))
                .collect::<Vec<_>>()
        );

        let getter = entities.iter().find(|e| e.name == "currentValue" && e.entity_type == "getter");
        assert!(getter.is_some(), "Should find top-level getter, got: {:?}",
            entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert!(
            getter.unwrap().content.contains("return _value"),
            "Top-level getter content should include the body"
        );
        assert!(getter.unwrap().parent_id.is_none(), "Top-level getter should have no parent");

        // tree-sitter-dart 0.1.0 parses top-level setters as function_signature
        // (treating `set` as a type_identifier). setter_signature is only
        // produced inside class_member → method_signature.
        let setter = entities.iter().find(|e| e.name == "currentValue" && e.entity_type == "function");
        assert!(setter.is_some(), "Should find top-level setter as function, got: {:?}",
            entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert!(
            setter.unwrap().content.contains("_value = v"),
            "Top-level setter content should include the body"
        );
    }

    #[test]
    fn test_dart_field_entity_type() {
        let code = r#"
class Config {
  final String name;
  static const int maxRetries = 3;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "config.dart");
        eprintln!(
            "Dart fields: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        let name_field = entities.iter().find(|e| e.name == "name" && e.parent_id.is_some());
        assert!(name_field.is_some(), "Should find field 'name', got: {:?}",
            entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert_eq!(name_field.unwrap().entity_type, "field");

        let max_retries = entities.iter().find(|e| e.name == "maxRetries");
        assert!(max_retries.is_some(), "Should find field 'maxRetries', got: {:?}",
            entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert_eq!(max_retries.unwrap().entity_type, "field");
    }

    #[test]
    fn test_dart_identifier_list_fields() {
        // identifier_list produces bare identifier children (no "name" field),
        // unlike initialized_identifier_list which wraps each in an
        // initialized_identifier node with a "name" field.
        let code = r#"
abstract class Shape {
  abstract double x, y;
  abstract String label;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "shape.dart");
        eprintln!(
            "Dart identifier_list fields: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        let x_field = entities.iter().find(|e| e.name == "x");
        assert!(x_field.is_some(), "Should find field 'x' from identifier_list, got: {:?}",
            entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert_eq!(x_field.unwrap().entity_type, "field");
        assert!(x_field.unwrap().parent_id.is_some(), "field 'x' should be nested under Shape");

        let label_field = entities.iter().find(|e| e.name == "label");
        assert!(label_field.is_some(), "Should find field 'label' from single-element identifier_list, got: {:?}",
            entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());
        assert_eq!(label_field.unwrap().entity_type, "field");
    }

    #[test]
    fn test_ocaml_entity_extraction() {
        let code = r#"
type color = Red | Green | Blue

type point = {
  x : float;
  y : float;
}

exception Not_found of string

let greet name =
  Printf.printf "Hello, %s!\n" name

let add a b = a + b

let version = "1.0"

let color_to_string = function
  | Red -> "red"
  | Blue -> "blue"

let inc = fun x -> x + 1

module MyModule = struct
  let helper x = x * 2
end

module type Printable = sig
  val to_string : 'a -> string
end

external caml_input : in_channel -> bytes -> int -> int -> int = "caml_input"

class point_class x_init = object
  val mutable x = x_init
  method get_x = x
end

class type measurable = object
  method measure : float
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "example.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("OCaml entities: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        assert_eq!(find("color").entity_type, "type");
        assert_eq!(find("point").entity_type, "type");
        assert_eq!(find("Not_found").entity_type, "exception");
        assert_eq!(find("greet").entity_type, "function");
        assert_eq!(find("add").entity_type, "function");
        assert_eq!(find("version").entity_type, "value");
        assert_eq!(find("color_to_string").entity_type, "function");
        assert_eq!(find("inc").entity_type, "function");
        assert_eq!(find("MyModule").entity_type, "module");
        assert_eq!(find("Printable").entity_type, "module_type");
        assert_eq!(find("caml_input").entity_type, "external");
        assert_eq!(find("point_class").entity_type, "class");
        assert_eq!(find("measurable").entity_type, "class_type");
    }

    #[test]
    fn test_ocaml_nested_module_entities() {
        let code = r#"
module Outer = struct
  let x = 42

  module Inner = struct
    let y = 0
  end
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("OCaml nested: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type, &e.parent_id)).collect::<Vec<_>>());

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        let outer = find("Outer");
        let x = find("x");
        let inner = find("Inner");
        let y = find("y");

        assert_eq!(outer.entity_type, "module");
        assert_eq!(x.entity_type, "value");
        assert_eq!(inner.entity_type, "module");
        assert_eq!(y.entity_type, "value");

        assert!(x.parent_id.as_ref().is_some_and(|p| p == &outer.id), "x should be nested under Outer");
        assert!(inner.parent_id.as_ref().is_some_and(|p| p == &outer.id), "Inner should be nested under Outer");
        assert!(y.parent_id.as_ref().is_some_and(|p| p == &inner.id), "y should be nested under Inner");
    }

    #[test]
    fn test_ocaml_interface_entity_extraction() {
        let code = r#"
type t

val create : string -> t
val to_string : t -> string

exception Invalid_input of string

module type Serializable = sig
  val serialize : t -> string
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "example.mli");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("OCaml interface entities: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        assert_eq!(find("t").entity_type, "type");
        assert_eq!(find("create").entity_type, "val");
        assert_eq!(find("to_string").entity_type, "val");
        assert_eq!(find("Invalid_input").entity_type, "exception");
        assert_eq!(find("Serializable").entity_type, "module_type");
    }

    #[test]
    fn test_ocaml_mutual_recursion_let() {
        let code = r#"
let rec even n = (n = 0) || odd (n - 1)
and odd n = (n <> 0) && even (n - 1)

let rec ping x = pong (x - 1)
and pong x = if x <= 0 then 0 else ping (x - 1)
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "mutual.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("OCaml mutual let: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        assert_eq!(find("even").entity_type, "function");
        assert_eq!(find("odd").entity_type, "function");
        assert_eq!(find("ping").entity_type, "function");
        assert_eq!(find("pong").entity_type, "function");
    }

    #[test]
    fn test_ocaml_mutual_recursion_module() {
        let code = r#"
module rec A : sig val x : int end = struct
  let x = B.y + 1
end
and B : sig val y : int end = struct
  let y = 0
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "mutual_mod.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("OCaml mutual module: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type, &e.parent_id)).collect::<Vec<_>>());

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        let a = find("A");
        let b = find("B");
        assert_eq!(a.entity_type, "module");
        assert_eq!(b.entity_type, "module");

        let x = find("x");
        let y = find("y");
        assert!(x.parent_id.as_ref().is_some_and(|p| p == &a.id), "x should be nested under A");
        assert!(y.parent_id.as_ref().is_some_and(|p| p == &b.id), "y should be nested under B");
    }

    #[test]
    fn test_ocaml_destructured_let() {
        let code = r#"
let (a, b) = (1, 2)

let { x; y } = point

let simple = 42
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "destruct.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("OCaml destructured: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        assert_eq!(find("a").entity_type, "value");
        assert_eq!(find("b").entity_type, "value");
        assert_eq!(find("x").entity_type, "value");
        assert_eq!(find("y").entity_type, "value");
        assert_eq!(find("simple").entity_type, "value");
    }

    #[test]
    fn test_ocaml_mutual_recursion_class() {
        let code = r#"
class foo = object
  method x = 1
end
and bar = object
  method y = 2
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "classes.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!("OCaml mutual class: {:?}", entities.iter().map(|e| (&e.name, &e.entity_type)).collect::<Vec<_>>());

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        assert_eq!(find("foo").entity_type, "class");
        assert_eq!(find("bar").entity_type, "class");
    }

    #[test]
    fn test_perl_entity_extraction() {
        let code = r#"package Foo::Bar;

use strict;
use warnings;

sub hello {
    my ($self, $name) = @_;
    print "Hello, $name!\n";
}

sub _private_helper {
    return 42;
}

1;
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Foo/Bar.pm");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"Foo::Bar"), "got: {:?}", names);
        assert!(names.contains(&"hello"), "got: {:?}", names);
        assert!(names.contains(&"_private_helper"), "got: {:?}", names);

        let find = |name: &str| entities.iter().find(|e| e.name == name)
            .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names));

        assert_eq!(find("Foo::Bar").entity_type, "package");
        assert_eq!(find("hello").entity_type, "function");
        assert_eq!(find("_private_helper").entity_type, "function");
    }
}
