use std::path::Path;

use crate::parser::registry::ParserRegistry;

/// Directories to skip during recursive file discovery.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "venv",
    "__pycache__",
];

/// Find all files under `root` supported by the parser registry.
pub fn find_supported_files(
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
) -> Vec<String> {
    let mut files = Vec::new();
    walk_dir(root, root, registry, ext_filter, &mut files);
    files.sort();
    files
}

fn walk_dir(
    dir: &Path,
    root: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    files: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
        }
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            walk_dir(&path, root, registry, ext_filter, files);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().to_string();
            if !ext_filter.is_empty()
                && !ext_filter.iter().any(|ext| rel_str.ends_with(ext.as_str()))
            {
                continue;
            }
            if registry.get_plugin(&rel_str).is_some() {
                files.push(rel_str);
            }
        }
    }
}
