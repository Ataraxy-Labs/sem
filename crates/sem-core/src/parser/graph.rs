//! Entity dependency graph — cross-file reference extraction.
//!
//! Implements a two-pass approach inspired by arXiv:2601.08773 (Reliable Graph-RAG):
//! Pass 1: Extract all entities, build a symbol table (name → entity ID).
//! Pass 2: For each entity, extract identifier references from its AST subtree,
//!         resolve them against the symbol table to create edges.
//!
//! This enables impact analysis: "if I change entity X, what else is affected?"

use std::collections::HashMap;
use std::path::Path;

use crate::model::entity::SemanticEntity;
use crate::parser::registry::ParserRegistry;

/// A reference from one entity to another.
#[derive(Debug, Clone)]
pub struct EntityRef {
    pub from_entity: String,
    pub to_entity: String,
    pub ref_type: RefType,
}

/// Type of reference between entities.
#[derive(Debug, Clone, PartialEq)]
pub enum RefType {
    /// Function/method call
    Calls,
    /// Type reference (extends, implements, field type)
    TypeRef,
    /// Import/use statement reference
    Imports,
}

/// A complete entity dependency graph for a set of files.
#[derive(Debug)]
pub struct EntityGraph {
    /// All entities indexed by ID
    pub entities: HashMap<String, EntityInfo>,
    /// Edges: from_entity → [(to_entity, ref_type)]
    pub edges: Vec<EntityRef>,
    /// Reverse index: entity_id → entities that reference it
    pub dependents: HashMap<String, Vec<String>>,
    /// Forward index: entity_id → entities it references
    pub dependencies: HashMap<String, Vec<String>>,
}

/// Minimal entity info stored in the graph.
#[derive(Debug, Clone)]
pub struct EntityInfo {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
}

impl EntityGraph {
    /// Build an entity graph from a set of files.
    ///
    /// Pass 1: Extract all entities from all files using the parser registry.
    /// Pass 2: For each entity, find identifier tokens and resolve them against
    ///         the symbol table to create reference edges.
    pub fn build(
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
    ) -> Self {
        // Pass 1: Extract all entities and build symbol table
        let mut all_entities: Vec<SemanticEntity> = Vec::new();

        for file_path in file_paths {
            let full_path = root.join(file_path);
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let plugin = match registry.get_plugin(file_path) {
                Some(p) => p,
                None => continue,
            };

            let entities = plugin.extract_entities(&content, file_path);
            all_entities.extend(entities);
        }

        // Build symbol table: name → entity IDs (can be multiple with same name)
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::new();
        let mut entity_map: HashMap<String, EntityInfo> = HashMap::new();

        for entity in &all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());

            entity_map.insert(
                entity.id.clone(),
                EntityInfo {
                    id: entity.id.clone(),
                    name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    start_line: entity.start_line,
                    end_line: entity.end_line,
                },
            );
        }

        // Pass 2: Extract references from entity content
        let mut edges: Vec<EntityRef> = Vec::new();
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::new();

        for entity in &all_entities {
            // Extract identifiers from entity content
            let refs = extract_references_from_content(&entity.content, &entity.name);

            for ref_name in refs {
                if let Some(target_ids) = symbol_table.get(&ref_name) {
                    // Resolve: prefer same-file entities, skip self-references
                    let target = target_ids
                        .iter()
                        .find(|id| {
                            *id != &entity.id
                                && entity_map
                                    .get(*id)
                                    .map_or(false, |e| e.file_path == entity.file_path)
                        })
                        .or_else(|| target_ids.iter().find(|id| *id != &entity.id));

                    if let Some(target_id) = target {
                        let ref_type = infer_ref_type(&entity.content, &ref_name);
                        edges.push(EntityRef {
                            from_entity: entity.id.clone(),
                            to_entity: target_id.clone(),
                            ref_type,
                        });
                        dependents
                            .entry(target_id.clone())
                            .or_default()
                            .push(entity.id.clone());
                        dependencies
                            .entry(entity.id.clone())
                            .or_default()
                            .push(target_id.clone());
                    }
                }
            }
        }

        EntityGraph {
            entities: entity_map,
            edges,
            dependents,
            dependencies,
        }
    }

    /// Get entities that depend on the given entity (reverse deps).
    pub fn get_dependents(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependents
            .get(entity_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get entities that the given entity depends on (forward deps).
    pub fn get_dependencies(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependencies
            .get(entity_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Impact analysis: if the given entity changes, what else might be affected?
    /// Returns all transitive dependents (breadth-first).
    pub fn impact_analysis(&self, entity_id: &str) -> Vec<&EntityInfo> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut result = Vec::new();

        queue.push_back(entity_id.to_string());
        visited.insert(entity_id.to_string());

        while let Some(current) = queue.pop_front() {
            if let Some(deps) = self.dependents.get(&current) {
                for dep in deps {
                    if visited.insert(dep.clone()) {
                        if let Some(info) = self.entities.get(dep) {
                            result.push(info);
                        }
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        result
    }
}

/// Extract identifier references from entity content using simple token analysis.
/// This is a lightweight alternative to full AST reference resolution —
/// it tokenizes the content and finds identifiers that could be references to other entities.
fn extract_references_from_content(content: &str, own_name: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Simple tokenizer: split on non-alphanumeric/underscore boundaries
    for word in content.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let word = word.trim();
        if word.is_empty() || word == own_name {
            continue;
        }
        // Skip keywords, literals, very short names (likely loop vars)
        if is_keyword(word) || word.len() < 2 {
            continue;
        }
        // Skip if starts with lowercase and is < 3 chars (likely variable)
        if word.starts_with(|c: char| c.is_lowercase()) && word.len() < 3 {
            continue;
        }
        // Must start with a letter or underscore (valid identifier)
        if !word.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            continue;
        }
        if seen.insert(word.to_string()) {
            refs.push(word.to_string());
        }
    }

    refs
}

/// Infer reference type from context.
fn infer_ref_type(content: &str, ref_name: &str) -> RefType {
    // Check if it's a function call: name followed by (
    let call_pattern = format!("{}(", ref_name);
    if content.contains(&call_pattern) {
        return RefType::Calls;
    }
    // Check if it's in an import/use context
    if content.contains("import ") || content.contains("use ") {
        return RefType::Imports;
    }
    // Default to type reference
    RefType::TypeRef
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        // Common across languages
        "if" | "else" | "for" | "while" | "do" | "switch" | "case" | "break"
            | "continue" | "return" | "try" | "catch" | "finally" | "throw"
            | "new" | "delete" | "typeof" | "instanceof" | "in" | "of"
            | "true" | "false" | "null" | "undefined" | "void" | "this"
            | "super" | "class" | "extends" | "implements" | "interface"
            | "enum" | "const" | "let" | "var" | "function" | "async"
            | "await" | "yield" | "import" | "export" | "default" | "from"
            | "as" | "static" | "public" | "private" | "protected"
            | "abstract" | "final" | "override"
            // Rust
            | "fn" | "pub" | "mod" | "use" | "struct" | "impl" | "trait"
            | "where" | "type" | "self" | "Self" | "mut" | "ref" | "match"
            | "loop" | "move" | "unsafe" | "extern" | "crate" | "dyn"
            // Python
            | "def" | "elif" | "except" | "raise" | "with"
            | "pass" | "lambda" | "nonlocal" | "global" | "assert"
            | "True" | "False" | "and" | "or" | "not" | "is"
            // Go
            | "func" | "package" | "range" | "select" | "chan" | "go"
            | "defer" | "map" | "make" | "append" | "len" | "cap"
            // Types (primitives)
            | "string" | "number" | "boolean" | "int" | "float" | "double"
            | "bool" | "char" | "byte" | "i8" | "i16" | "i32" | "i64"
            | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "usize"
            | "isize" | "str" | "String" | "Vec" | "Option" | "Result"
            | "Box" | "Arc" | "Rc" | "HashMap" | "HashSet" | "Some"
            | "Ok" | "Err"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_references() {
        let content = "function processData(input) {\n  const result = validateInput(input);\n  return transform(result);\n}";
        let refs = extract_references_from_content(content, "processData");
        assert!(refs.contains(&"validateInput".to_string()));
        assert!(refs.contains(&"transform".to_string()));
        assert!(!refs.contains(&"processData".to_string())); // self excluded
    }

    #[test]
    fn test_extract_references_skips_keywords() {
        let content = "function foo() { if (true) { return false; } }";
        let refs = extract_references_from_content(content, "foo");
        assert!(!refs.contains(&"if".to_string()));
        assert!(!refs.contains(&"true".to_string()));
        assert!(!refs.contains(&"return".to_string()));
        assert!(!refs.contains(&"false".to_string()));
    }

    #[test]
    fn test_infer_ref_type_call() {
        assert_eq!(
            infer_ref_type("validateInput(data)", "validateInput"),
            RefType::Calls,
        );
    }

    #[test]
    fn test_infer_ref_type_type() {
        assert_eq!(
            infer_ref_type("let x: MyType = something", "MyType"),
            RefType::TypeRef,
        );
    }
}
