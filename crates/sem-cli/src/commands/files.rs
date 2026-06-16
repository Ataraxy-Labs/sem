use std::path::Path;

use colored::Colorize;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

pub fn find_supported_files_in_path(
    root: &Path,
    scan_path: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Vec<String> {
    let mut files = Vec::new();

    let mut builder = ignore::WalkBuilder::new(scan_path);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    let semignore = root.join(".semignore");
    if semignore.exists() {
        builder.add_ignore(semignore);
    }

    if !no_default_excludes {
        let root = root.to_path_buf();
        builder.filter_entry(move |entry| {
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
            {
                return true;
            }

            let rel_path = file_path_for_entity(&root, entry.path());
            !is_default_excluded(&rel_path)
        });
    }

    let walker = builder.build();

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!(
                    "{} Cannot walk '{}': {}",
                    "error:".red().bold(),
                    scan_path.display(),
                    e
                );
                std::process::exit(1);
            }
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let rel_path = file_path_for_entity(root, path);
        if !no_default_excludes && is_default_excluded(&rel_path) {
            continue;
        }
        if is_hidden_path(&rel_path) {
            continue;
        }
        if !ext_filter.is_empty()
            && !ext_filter
                .iter()
                .any(|ext| rel_path.ends_with(ext.as_str()))
        {
            continue;
        }
        if is_probably_binary_path(&rel_path) {
            continue;
        }
        if !has_supported_plugin(path, &rel_path, registry, ext_filter) {
            continue;
        }
        files.push(rel_path);
    }

    files.sort();
    files
}

fn is_hidden_path(rel_path: &str) -> bool {
    rel_path
        .split('/')
        .any(|component| component.starts_with('.') && component.len() > 1)
}

fn has_supported_plugin(
    path: &Path,
    rel_path: &str,
    registry: &ParserRegistry,
    ext_filter: &[String],
) -> bool {
    if registry.get_explicit_plugin(rel_path).is_some() {
        return true;
    }

    if !ext_filter.is_empty() || Path::new(rel_path).extension().is_some() {
        return false;
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };

    registry.detect_plugin_from_content(&content).is_some()
}

pub fn file_path_for_entity(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sem_core::parser::plugins::create_default_registry;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> std::path::PathBuf {
        let name = format!(
            "sem-cli-files-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn scan_skips_binary_files_and_default_excludes() {
        let root = temp_dir();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("src/generated")).unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            root.join("src/run"),
            "#!/usr/bin/env node\nfunction main() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/notes.weird"), "plain text\n").unwrap();
        fs::write(root.join("src/blob.weird"), b"abc\0def").unwrap();
        fs::write(root.join("src/icon.png"), b"\x89PNG\r\n").unwrap();
        fs::write(
            root.join("src/generated/schema.ts"),
            "export function generatedSchema() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/api.generated.ts"),
            "export function generatedApi() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/styles.module.scss.d.ts"),
            "declare const styles: Record<string, string>;\nexport default styles;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/logo.svg.d.ts"),
            "declare const src: string;\nexport default src;\n",
        )
        .unwrap();
        fs::write(root.join("dist/generated.js"), "function generated() {}\n").unwrap();

        let registry = create_default_registry();
        let files = find_supported_files_in_path(&root, &root, &registry, &[], false);

        assert_eq!(
            files,
            vec!["src/main.rs".to_string(), "src/run".to_string()]
        );

        let files_with_generated = find_supported_files_in_path(&root, &root, &registry, &[], true);
        assert!(files_with_generated.contains(&"src/main.rs".to_string()));
        assert!(files_with_generated.contains(&"src/run".to_string()));
        assert!(files_with_generated.contains(&"src/generated/schema.ts".to_string()));
        assert!(files_with_generated.contains(&"src/api.generated.ts".to_string()));
        assert!(files_with_generated.contains(&"src/styles.module.scss.d.ts".to_string()));
        assert!(files_with_generated.contains(&"src/logo.svg.d.ts".to_string()));
        assert!(files_with_generated.contains(&"dist/generated.js".to_string()));
        assert!(!files_with_generated.contains(&"src/notes.weird".to_string()));
        assert!(!files_with_generated.contains(&"src/blob.weird".to_string()));
        assert!(!files_with_generated.contains(&"src/icon.png".to_string()));

        let rs_files =
            find_supported_files_in_path(&root, &root, &registry, &[".rs".to_string()], true);
        assert_eq!(rs_files, vec!["src/main.rs".to_string()]);

        fs::remove_dir_all(root).unwrap();
    }
}
