use crate::config::Config;
use crate::types::{Entity, ParseError};

pub struct Parser {
    config: Config,
}

impl Parser {
    pub fn new(config: Config) -> Self {
        Parser { config }
    }

    pub fn parse(&self, content: &str) -> Result<Vec<Entity>, ParseError> {
        let mut entities = Vec::new();
        for line in content.lines() {
            if let Some(entity) = extract_entity(line) {
                entities.push(entity);
            }
        }
        Ok(entities)
    }

    pub fn is_debug(&self) -> bool {
        self.config.debug
    }
}

fn extract_entity(line: &str) -> Option<Entity> {
    if line.starts_with("fn ") || line.starts_with("pub fn ") {
        Some(Entity {
            name: line.to_string(),
            kind: "function".to_string(),
        })
    } else {
        None
    }
}

pub fn validate_content(content: &str) -> Result<(), ParseError> {
    if content.is_empty() {
        return Err(ParseError::Empty);
    }
    Ok(())
}
