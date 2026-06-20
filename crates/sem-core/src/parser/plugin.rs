use crate::model::entity::SemanticEntity;

pub trait SemanticParserPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn extensions(&self) -> &[&str];
    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity>;
    fn extract_entities_brief(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let mut entities = self.extract_entities(content, file_path);
        strip_entity_payloads(&mut entities);
        entities
    }
    /// Extract entities and optionally return the tree-sitter Tree for reuse.
    /// Default returns None for the tree (non-code plugins).
    fn extract_entities_with_tree(
        &self,
        content: &str,
        file_path: &str,
    ) -> (Vec<SemanticEntity>, Option<tree_sitter::Tree>) {
        (self.extract_entities(content, file_path), None)
    }
    fn structural_hash_content(&self, _content: &str, _file_path: &str) -> Option<String> {
        None
    }
    fn compute_similarity(&self, a: &SemanticEntity, b: &SemanticEntity) -> f64 {
        crate::model::identity::default_similarity(a, b)
    }
}

pub fn strip_entity_payloads(entities: &mut [SemanticEntity]) {
    for entity in entities {
        entity.content.clear();
        entity.content_hash.clear();
        entity.structural_hash = None;
    }
}
