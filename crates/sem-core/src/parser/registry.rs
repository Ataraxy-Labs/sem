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
        let ext = get_extension(file_path);
        if let Some(&idx) = self.extension_map.get(&ext) {
            return Some(self.plugins[idx].as_ref());
        }
        // Fallback plugin
        self.get_plugin_by_id("fallback")
    }

    pub fn get_plugin_by_id(&self, id: &str) -> Option<&dyn SemanticParserPlugin> {
        self.plugins.iter().find(|p| p.id() == id).map(|p| p.as_ref())
    }
}

fn get_extension(file_path: &str) -> String {
    Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default()
}
