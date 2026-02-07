use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

pub struct FallbackParserPlugin;

const CHUNK_SIZE: usize = 20;

impl SemanticParserPlugin for FallbackParserPlugin {
    fn id(&self) -> &str {
        "fallback"
    }

    fn extensions(&self) -> &[&str] {
        &[]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let lines: Vec<&str> = content.lines().collect();
        let mut entities = Vec::new();

        let mut i = 0;
        while i < lines.len() {
            let end = (i + CHUNK_SIZE).min(lines.len());
            let chunk: Vec<&str> = lines[i..end].to_vec();
            let chunk_content = chunk.join("\n");
            let start_line = i + 1;
            let end_line = end;
            let name = format!("lines {start_line}-{end_line}");

            entities.push(SemanticEntity {
                id: build_entity_id(file_path, "chunk", &name, None),
                file_path: file_path.to_string(),
                entity_type: "chunk".to_string(),
                name,
                parent_id: None,
                content_hash: content_hash(&chunk_content),
                structural_hash: None,
                content: chunk_content,
                start_line,
                end_line,
                metadata: None,
            });

            i += CHUNK_SIZE;
        }

        entities
    }
}
