use crate::model::entity::SemanticEntity;

pub trait SemanticParserPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn extensions(&self) -> &[&str];
    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity>;
    fn compute_similarity(&self, a: &SemanticEntity, b: &SemanticEntity) -> f64 {
        crate::model::identity::default_similarity(a, b)
    }
}
