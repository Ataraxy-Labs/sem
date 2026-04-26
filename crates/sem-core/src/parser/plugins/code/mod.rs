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
    fn test_nested_functions_go() {
        // Go doesn't have named nested functions, but has nested type/var declarations
        let code = "package main\n\nfunc outer() {\n    var x int = 42\n    _ = x\n}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
    }
}
