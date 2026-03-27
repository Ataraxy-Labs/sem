use std::collections::HashMap;
use std::path::Path;

use super::plugin::SemanticParserPlugin;

pub struct ParserRegistry {
    plugins: Vec<Box<dyn SemanticParserPlugin>>,
    extension_map: HashMap<String, usize>, // ext → index into plugins
}

impl ParserRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            extension_map: HashMap::new(),
        }
    }

    pub fn register(&mut self, plugin: Box<dyn SemanticParserPlugin>) {
        let idx = self.plugins.len();
        for ext in plugin.extensions() {
            self.extension_map.insert(ext.to_string(), idx);
        }
        self.plugins.push(plugin);
    }

    pub fn get_plugin(&self, file_path: &str) -> Option<&dyn SemanticParserPlugin> {
        for ext in get_extensions(file_path) {
            if let Some(&idx) = self.extension_map.get(&ext) {
                return Some(self.plugins[idx].as_ref());
            }
        }
        // Fallback plugin
        self.get_plugin_by_id("fallback")
    }

    pub fn get_plugin_by_id(&self, id: &str) -> Option<&dyn SemanticParserPlugin> {
        self.plugins
            .iter()
            .find(|p| p.id() == id)
            .map(|p| p.as_ref())
    }
}

fn get_extensions(file_path: &str) -> Vec<String> {
    let Some(file_name) = Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return Vec::new();
    };

    let file_name = file_name.to_lowercase();
    let mut extensions = Vec::new();

    for (idx, ch) in file_name.char_indices() {
        if ch == '.' {
            extensions.push(file_name[idx..].to_string());
        }
    }

    extensions
}

#[cfg(test)]
mod tests {
    use crate::parser::plugins::create_default_registry;

    #[test]
    fn test_registry_matches_compound_svelte_typescript_suffix() {
        let registry = create_default_registry();
        let plugin = registry
            .get_plugin("src/routes/+page.svelte.ts")
            .expect("plugin should exist");

        assert_eq!(plugin.id(), "svelte");
    }

    #[test]
    fn test_registry_matches_compound_svelte_javascript_suffix() {
        let registry = create_default_registry();
        let plugin = registry
            .get_plugin("src/routes/+layout.svelte.js")
            .expect("plugin should exist");

        assert_eq!(plugin.id(), "svelte");
    }

    #[test]
    fn test_registry_matches_svelte_test_suffix() {
        let registry = create_default_registry();
        let plugin = registry
            .get_plugin("src/lib/multiplier.svelte.test.js")
            .expect("plugin should exist");

        assert_eq!(plugin.id(), "svelte");
    }

    #[test]
    fn test_registry_prefers_svelte_plugin_for_component_files() {
        let registry = create_default_registry();
        let plugin = registry
            .get_plugin("src/lib/Component.svelte")
            .expect("plugin should exist");

        assert_eq!(plugin.id(), "svelte");
    }
}
