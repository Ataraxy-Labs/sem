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
        let all_names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Top-level variable declarations preserved
        assert!(top_level.contains(&"foo"), "got: {:?}", top_level);
        assert!(top_level.contains(&"bar"), "got: {:?}", top_level);
        assert!(top_level.contains(&"items"), "got: {:?}", top_level);

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
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

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
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"handler"), "got: {:?}", names);
        assert!(names.contains(&"Inner"), "got: {:?}", names);
        assert!(names.contains(&"make"), "got: {:?}", names);
        assert!(!names.contains(&"local"), "got: {:?}", names);
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
}
