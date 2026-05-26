use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::parser::graph::EntityInfo;

pub(crate) fn find_import_target<'a>(
    target_ids: &'a [String],
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    entity_map: &HashMap<String, EntityInfo>,
) -> Option<&'a String> {
    if let Some(candidates) = import_file_candidates(file_path, source_path, extensions) {
        return candidates.iter().find_map(|candidate_path| {
            target_ids.iter().find(|id| {
                entity_map
                    .get(*id)
                    .map_or(false, |e| e.file_path == *candidate_path)
            })
        });
    }

    let source_module = import_stem(source_path);
    target_ids.iter().find(|id| {
        entity_map
            .get(*id)
            .map_or(false, |e| file_stem(&e.file_path) == source_module)
    })
}

pub(crate) fn import_source_matches_file(
    importing_file_path: &str,
    source_path: &str,
    extensions: &[&str],
    candidate_file_path: &str,
) -> bool {
    import_file_candidates(importing_file_path, source_path, extensions).map_or_else(
        || file_stem(candidate_file_path) == import_stem(source_path),
        |paths| paths.iter().any(|path| path == candidate_file_path),
    )
}

fn import_file_candidates(
    file_path: &str,
    source_path: &str,
    extensions: &[&str],
) -> Option<Vec<String>> {
    let source_path = source_path.trim();
    if source_path.is_empty() {
        return None;
    }

    let module_path = if source_path.starts_with('.')
        && !source_path.starts_with("./")
        && !source_path.starts_with("../")
    {
        python_relative_module_path(file_path, source_path)?
    } else if source_path.starts_with("./") || source_path.starts_with("../") {
        let base_dir = Path::new(file_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        normalize_repo_path(base_dir.join(source_path))?
    } else if extensions.len() == 1 && extensions[0] == ".py" && source_path.contains('.') {
        normalize_repo_path(PathBuf::from(source_path.replace('.', "/")))?
    } else {
        return None;
    };

    Some(module_candidates(&module_path, extensions))
}

fn python_relative_module_path(file_path: &str, source_path: &str) -> Option<String> {
    let dot_count = source_path.chars().take_while(|c| *c == '.').count();
    if dot_count == 0 {
        return None;
    }

    let mut base = PathBuf::from(
        Path::new(file_path)
            .parent()
            .unwrap_or_else(|| Path::new("")),
    );
    for _ in 1..dot_count {
        base = base.parent()?.to_path_buf();
    }

    let remainder = source_path[dot_count..].replace('.', "/");
    if remainder.is_empty() {
        normalize_repo_path(base)
    } else {
        normalize_repo_path(base.join(remainder))
    }
}

fn module_candidates(module_path: &str, extensions: &[&str]) -> Vec<String> {
    let mut candidates = Vec::new();
    let known_ext = extensions.iter().find(|ext| module_path.ends_with(**ext));

    if let Some(_ext) = known_ext {
        candidates.push(module_path.to_string());
    } else {
        for ext in extensions {
            candidates.push(format!("{module_path}{ext}"));
        }
        for ext in extensions {
            candidates.push(format!("{module_path}/index{ext}"));
        }
    }

    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));
    candidates
}

fn normalize_repo_path(path: PathBuf) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

fn import_stem(source_path: &str) -> &str {
    let source_path = source_path.trim_start_matches('.');
    let source_path = source_path.rsplit('/').next().unwrap_or(source_path);
    let stem = file_stem(source_path);
    if stem == source_path {
        source_path.rsplit('.').next().unwrap_or(source_path)
    } else {
        stem
    }
}

fn file_stem(file_path: &str) -> &str {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    file_name
        .strip_suffix(".py")
        .or_else(|| file_name.strip_suffix(".rs"))
        .or_else(|| file_name.strip_suffix(".ts"))
        .or_else(|| file_name.strip_suffix(".tsx"))
        .or_else(|| file_name.strip_suffix(".js"))
        .or_else(|| file_name.strip_suffix(".jsx"))
        .or_else(|| file_name.strip_suffix(".go"))
        .unwrap_or(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(file_path: &str) -> EntityInfo {
        EntityInfo {
            id: format!("{file_path}::function::helper"),
            name: "helper".to_string(),
            entity_type: "function".to_string(),
            file_path: file_path.to_string(),
            parent_id: None,
            start_line: 1,
            end_line: 1,
        }
    }

    #[test]
    fn explicit_relative_import_prefers_exact_extension() {
        let ids = vec![
            "src/util.js::function::helper".to_string(),
            "src/util.ts::function::helper".to_string(),
        ];
        let entity_map = HashMap::from([
            (ids[0].clone(), entity("src/util.js")),
            (ids[1].clone(), entity("src/util.ts")),
        ]);

        let target = find_import_target(
            &ids,
            "./util.ts",
            "src/main.ts",
            &[".ts", ".tsx", ".js", ".jsx"],
            &entity_map,
        );

        assert_eq!(target, Some(&ids[1]));
    }

    #[test]
    fn explicit_relative_import_requires_exact_extension() {
        let ids = vec!["src/util.js::function::helper".to_string()];
        let entity_map = HashMap::from([(ids[0].clone(), entity("src/util.js"))]);

        let target = find_import_target(
            &ids,
            "./util.ts",
            "src/main.ts",
            &[".ts", ".tsx", ".js", ".jsx"],
            &entity_map,
        );

        assert_eq!(target, None);
    }

    #[test]
    fn absolute_python_import_uses_dotted_path() {
        let ids = vec![
            "src/a/util.py::function::helper".to_string(),
            "src/b/util.py::function::helper".to_string(),
        ];
        let entity_map = HashMap::from([
            (ids[0].clone(), entity("src/a/util.py")),
            (ids[1].clone(), entity("src/b/util.py")),
        ]);

        let target = find_import_target(&ids, "src.b.util", "src/main.py", &[".py"], &entity_map);

        assert_eq!(target, Some(&ids[1]));
    }

    #[test]
    fn bare_import_with_extension_uses_file_stem() {
        let ids = vec!["src/util.ts::function::helper".to_string()];
        let entity_map = HashMap::from([(ids[0].clone(), entity("src/util.ts"))]);

        let target = find_import_target(
            &ids,
            "util.ts",
            "src/main.ts",
            &[".ts", ".tsx", ".js", ".jsx"],
            &entity_map,
        );

        assert_eq!(target, Some(&ids[0]));
    }
}
