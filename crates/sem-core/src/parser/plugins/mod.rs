pub mod code;
pub mod json;
pub mod yaml;
pub mod toml_plugin;
pub mod csv_plugin;
pub mod markdown;
pub mod fallback;

use crate::parser::registry::ParserRegistry;

pub fn create_default_registry() -> ParserRegistry {
    let mut registry = ParserRegistry::new();

    registry.register(Box::new(json::JsonParserPlugin));
    registry.register(Box::new(code::CodeParserPlugin));
    registry.register(Box::new(yaml::YamlParserPlugin));
    registry.register(Box::new(toml_plugin::TomlParserPlugin));
    registry.register(Box::new(csv_plugin::CsvParserPlugin));
    registry.register(Box::new(markdown::MarkdownParserPlugin));
    // Fallback must be last
    registry.register(Box::new(fallback::FallbackParserPlugin));

    registry
}
