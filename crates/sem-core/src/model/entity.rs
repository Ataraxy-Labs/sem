use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticEntity {
    pub id: String,
    pub file_path: String,
    pub entity_type: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub content: String,
    pub content_hash: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

pub fn build_entity_id(
    file_path: &str,
    entity_type: &str,
    name: &str,
    parent_id: Option<&str>,
) -> String {
    match parent_id {
        Some(pid) => format!("{file_path}::{pid}::{name}"),
        None => format!("{file_path}::{entity_type}::{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_entity_id_no_parent() {
        assert_eq!(
            build_entity_id("src/main.ts", "function", "hello", None),
            "src/main.ts::function::hello"
        );
    }

    #[test]
    fn test_build_entity_id_with_parent() {
        let id = build_entity_id("src/main.ts", "method", "greet", Some("MyClass"));
        assert_eq!(id, "src/main.ts::MyClass::greet");
    }
}
