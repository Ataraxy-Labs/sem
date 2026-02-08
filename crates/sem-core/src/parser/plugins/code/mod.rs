mod entity_extractor;
mod languages;

use crate::model::entity::SemanticEntity;
use crate::parser::plugin::SemanticParserPlugin;
use languages::{get_all_code_extensions, get_language_config};
use entity_extractor::extract_entities;

pub struct CodeParserPlugin;

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

        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&language).is_err() {
            return Vec::new();
        }

        let tree = match parser.parse(content.as_bytes(), None) {
            Some(t) => t,
            None => return Vec::new(),
        };

        extract_entities(&tree, file_path, config, content)
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
}
