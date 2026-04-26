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

    /// Try to detect language from shebang line when extension-based lookup fails.
    /// Call this as a fallback when file content is available.
    pub fn get_plugin_with_content(&self, file_path: &str, content: &str) -> Option<&dyn SemanticParserPlugin> {
        // Try extension first
        for ext in get_extensions(file_path) {
            if let Some(&idx) = self.extension_map.get(&ext) {
                return Some(self.plugins[idx].as_ref());
            }
        }
        // Try shebang detection
        if let Some(plugin) = self.detect_from_shebang(content) {
            return Some(plugin);
        }
        // Fallback plugin
        self.get_plugin_by_id("fallback")
    }

    fn detect_from_shebang(&self, content: &str) -> Option<&dyn SemanticParserPlugin> {
        if let Some(ext) = detect_ext_from_content(content) {
            if let Some(&idx) = self.extension_map.get(ext.as_str()) {
                return Some(self.plugins[idx].as_ref());
            }
        }
        None
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

const LANG_MAPPING: &[(&str, &str)] = &[
    ("perl", ".pl"),
    ("python", ".py"),
    ("ruby", ".rb"),
    ("bash", ".sh"),
    ("/sh", ".sh"),
    ("node", ".js"),
    ("javascript", ".js"),
    ("typescript", ".ts"),
    ("swift", ".swift"),
    ("elixir", ".ex"),
    ("rust", ".rs"),
    ("go", ".go"),
    ("kotlin", ".kt"),
    ("dart", ".dart"),
    ("php", ".php"),
    ("java", ".java"),
    ("c", ".c"),
    ("cpp", ".cpp"),
    ("cs", ".cs"),
    ("csharp", ".cs"),
    ("fortran", ".f90"),
    ("terraform", ".tf"),
    ("hcl", ".hcl"),
    ("ocaml", ".ml"),
    ("eruby", ".erb"),
    ("vue", ".vue"),
    ("svelte", ".svelte"),
];

/// Detect file extension from shebang line or vim modeline.
pub fn detect_ext_from_content(content: &str) -> Option<String> {
    // Try shebang (first line)
    if let Some(first_line) = content.lines().next() {
        if first_line.starts_with("#!") {
            let shebang = first_line.to_lowercase();
            for (keyword, ext) in LANG_MAPPING {
                if shebang.contains(keyword) {
                    return Some(ext.to_string());
                }
            }
        }
    }

    // Try vim modeline (first 5 or last 5 lines)
    // Formats: `vim: ft=perl`, `vim: filetype=perl`, `vim: set ft=perl`
    let lines: Vec<&str> = content.lines().collect();
    let check_lines = lines.iter().take(5).chain(lines.iter().rev().take(5));
    for line in check_lines {
        if let Some(ft) = extract_vim_filetype(line) {
            let ft_lower = ft.to_lowercase();
            for (keyword, ext) in LANG_MAPPING {
                if ft_lower == *keyword {
                    return Some(ext.to_string());
                }
            }
        }
    }

    None
}

fn extract_vim_filetype(line: &str) -> Option<&str> {
    // Match patterns: `vim: ft=X`, `vim: filetype=X`, `vim: set ft=X`
    let line = line.trim();
    let vim_idx = line.find("vim:")?;
    let after_vim = &line[vim_idx + 4..];

    for token in after_vim.split_whitespace() {
        if let Some(val) = token.strip_prefix("ft=") {
            return Some(val.trim_end_matches(':'));
        }
        if let Some(val) = token.strip_prefix("filetype=") {
            return Some(val.trim_end_matches(':'));
        }
    }
    None
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

    #[test]
    fn test_registry_matches_typescript_module_suffix() {
        let registry = create_default_registry();
        let plugin = registry
            .get_plugin("src/lib/index.mts")
            .expect("plugin should exist");

        assert_eq!(plugin.id(), "code");
    }

    #[test]
    fn test_registry_matches_typescript_commonjs_suffix() {
        let registry = create_default_registry();
        let plugin = registry
            .get_plugin("src/lib/index.cts")
            .expect("plugin should exist");

        assert_eq!(plugin.id(), "code");
    }
}
