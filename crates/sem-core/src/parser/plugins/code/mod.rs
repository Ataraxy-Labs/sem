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
