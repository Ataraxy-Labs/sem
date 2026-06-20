use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use crate::parser::graph::EntityInfo;
use regex::Regex;

pub(crate) const JS_TS_EXTENSIONS: &[&str] =
    &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

pub(crate) fn is_js_ts_file(file_path: &str) -> bool {
    JS_TS_EXTENSIONS
        .iter()
        .any(|extension| file_path.ends_with(extension))
}

pub fn js_ts_import_source_files_from_content<P: AsRef<str>>(
    file_path: &str,
    content: &str,
    candidate_file_paths: &[P],
) -> Vec<String> {
    if !is_js_ts_file(file_path) {
        return Vec::new();
    }

    let mut imported_files = HashSet::new();
    for source in js_ts_import_sources(content) {
        if let Some(imported_file) =
            find_import_file(candidate_file_paths, source, file_path, JS_TS_EXTENSIONS)
        {
            imported_files.insert(imported_file.to_string());
        }
    }

    let mut imported_files: Vec<String> = imported_files.into_iter().collect();
    sort_import_candidate_files(&mut imported_files, JS_TS_EXTENSIONS);
    imported_files
}

pub fn js_ts_import_source_files_from_set<S>(
    file_path: &str,
    content: &str,
    candidate_file_paths: &HashSet<&str, S>,
) -> Vec<String>
where
    S: BuildHasher,
{
    if !is_js_ts_file(file_path) {
        return Vec::new();
    }

    let mut imported_files = HashSet::new();
    let mut sorted_candidates: Option<Vec<&str>> = None;
    for source in js_ts_import_sources(content) {
        if let Some(candidates) = import_file_candidates(file_path, source, JS_TS_EXTENSIONS) {
            if let Some(imported_file) = candidates
                .iter()
                .find(|candidate| candidate_file_paths.contains(candidate.as_str()))
            {
                imported_files.insert(imported_file.clone());
            }
            continue;
        }

        let source_module = import_stem(source);
        let candidates = sorted_candidates.get_or_insert_with(|| {
            let mut candidates: Vec<&str> = candidate_file_paths.iter().copied().collect();
            sort_import_candidate_files(&mut candidates, JS_TS_EXTENSIONS);
            candidates
        });
        if let Some(imported_file) = candidates
            .iter()
            .find(|path| file_stem(path) == source_module)
        {
            imported_files.insert((*imported_file).to_string());
        }
    }

    let mut imported_files: Vec<String> = imported_files.into_iter().collect();
    sort_import_candidate_files(&mut imported_files, JS_TS_EXTENSIONS);
    imported_files
}

pub fn js_ts_import_source_files_from_filesystem(
    root: &Path,
    file_path: &str,
    content: &str,
) -> Vec<String> {
    js_ts_import_source_files_from_filesystem_with_unscoped(root, file_path, content).0
}

pub fn js_ts_import_source_files_from_filesystem_with_unscoped(
    root: &Path,
    file_path: &str,
    content: &str,
) -> (Vec<String>, bool) {
    if !is_js_ts_file(file_path) {
        return (Vec::new(), false);
    }

    let mut imported_files = HashSet::new();
    let mut has_unscoped_imports = false;
    for source in js_ts_import_sources(content) {
        let Some(candidates) = import_file_candidates(file_path, source, JS_TS_EXTENSIONS) else {
            has_unscoped_imports = true;
            continue;
        };
        if let Some(imported_file) = candidates
            .iter()
            .find(|candidate| root.join(candidate).is_file())
        {
            imported_files.insert(imported_file.clone());
        }
    }

    let mut imported_files: Vec<String> = imported_files.into_iter().collect();
    sort_import_candidate_files(&mut imported_files, JS_TS_EXTENSIONS);
    (imported_files, has_unscoped_imports)
}

fn js_ts_import_sources(content: &str) -> Vec<&str> {
    static FROM_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\bfrom\s*['"]([^'"]+)['"]"#).unwrap());
    static SIDE_EFFECT_IMPORT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\bimport\s*['"]([^'"]+)['"]"#).unwrap());

    let mut sources = Vec::new();
    for cap in FROM_RE.captures_iter(content) {
        if let Some(source) = cap.get(1).map(|m| m.as_str()) {
            sources.push(source);
        }
    }
    for cap in SIDE_EFFECT_IMPORT_RE.captures_iter(content) {
        if let Some(source) = cap.get(1).map(|m| m.as_str()) {
            sources.push(source);
        }
    }
    sources
}

pub fn js_ts_has_default_re_export_from_content(content: &str) -> bool {
    static DEFAULT_REEXPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"export\s+(?:type\s+)?\{[^}]*\bdefault\b[^}]*\}\s*from\s*['"][^'"]+['"]"#)
            .unwrap()
    });

    DEFAULT_REEXPORT_RE.is_match(content)
}

pub(crate) fn js_ts_named_exports_from_content(content: &str) -> HashSet<String> {
    static NAMED_DECL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\bexport\s+(?:declare\s+)?(?:async\s+)?(?:abstract\s+)?(?:function\s*\*?|class|interface|type|enum)\s+([A-Za-z_$][\w$]*)",
        )
        .unwrap()
    });
    static VAR_DECL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bexport\s+(?:declare\s+)?(?:const|let|var)\s+([^;\n]+)").unwrap()
    });
    static SPECIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"export\s+(?:type\s+)?\{([^}]+)\}(?:\s*from\s*['"][^'"]+['"])?\s*;?"#).unwrap()
    });

    let mut names = HashSet::new();

    for cap in NAMED_DECL_RE.captures_iter(content) {
        names.insert(cap.get(1).unwrap().as_str().to_string());
    }

    for cap in VAR_DECL_RE.captures_iter(content) {
        for declarator in split_js_ts_var_declarators(cap.get(1).unwrap().as_str()) {
            if let Some(name) = js_ts_identifier_prefix(declarator) {
                names.insert(name.to_string());
            }
        }
    }

    for cap in SPECIFIER_RE.captures_iter(content) {
        for name_part in cap.get(1).unwrap().as_str().split(',') {
            if let Some(exported_name) = js_ts_export_specifier_name(name_part) {
                names.insert(exported_name.to_string());
            }
        }
    }

    names
}

fn split_js_ts_var_declarators(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;

    for (idx, ch) in input.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&input[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(&input[start..]);
    parts
}

fn js_ts_export_specifier_name(name_part: &str) -> Option<&str> {
    let name_part = name_part.trim();
    if name_part.is_empty() {
        return None;
    }

    let exported = if let Some(pos) = name_part.find(" as ") {
        &name_part[pos + 4..]
    } else {
        name_part
    };
    let exported = exported.trim();
    let exported = exported.strip_prefix("type ").unwrap_or(exported).trim();

    js_ts_identifier_prefix(exported)
}

fn js_ts_identifier_prefix(input: &str) -> Option<&str> {
    let input = input.trim_start();
    let mut chars = input.char_indices();
    let (_, first) = chars.next()?;
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return None;
    }

    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch == '_' || ch == '$' || ch.is_ascii_alphanumeric() {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    Some(&input[..end])
}

pub(crate) fn sort_import_candidate_files<P: AsRef<str>>(paths: &mut [P], extensions: &[&str]) {
    paths.sort_by(|left, right| {
        let left = left.as_ref();
        let right = right.as_ref();
        extension_priority(left, extensions)
            .cmp(&extension_priority(right, extensions))
            .then_with(|| left.cmp(right))
    });
}

fn extension_priority(file_path: &str, extensions: &[&str]) -> usize {
    extensions
        .iter()
        .position(|extension| file_path.ends_with(extension))
        .unwrap_or(extensions.len())
}

pub(crate) fn find_import_target<'a, S>(
    target_ids: &'a [String],
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
    entity_map: &HashMap<String, EntityInfo, S>,
) -> Option<&'a String>
where
    S: BuildHasher,
{
    let target_files: Vec<&str> = target_ids
        .iter()
        .filter_map(|id| entity_map.get(id).map(|entity| entity.file_path.as_str()))
        .collect();
    let target_file = find_import_file(&target_files, source_path, file_path, extensions)?;

    target_ids.iter().find(|id| {
        entity_map
            .get(*id)
            .map_or(false, |entity| entity.file_path == target_file)
    })
}

pub(crate) fn find_import_file<'a, P: AsRef<str>>(
    candidate_file_paths: &'a [P],
    source_path: &str,
    file_path: &str,
    extensions: &[&str],
) -> Option<&'a str> {
    if let Some(candidates) = import_file_candidates(file_path, source_path, extensions) {
        return candidates.iter().find_map(|candidate_path| {
            candidate_file_paths
                .iter()
                .map(AsRef::as_ref)
                .find(|path| *path == candidate_path.as_str())
        });
    }

    let source_module = import_stem(source_path);
    let mut candidates: Vec<&'a str> = candidate_file_paths.iter().map(AsRef::as_ref).collect();
    sort_import_candidate_files(&mut candidates, extensions);
    candidates
        .into_iter()
        .find(|path| file_stem(path) == source_module)
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
        .or_else(|| file_name.strip_suffix(".mts"))
        .or_else(|| file_name.strip_suffix(".cts"))
        .or_else(|| file_name.strip_suffix(".ts"))
        .or_else(|| file_name.strip_suffix(".tsx"))
        .or_else(|| file_name.strip_suffix(".mjs"))
        .or_else(|| file_name.strip_suffix(".cjs"))
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
            JS_TS_EXTENSIONS,
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
            JS_TS_EXTENSIONS,
            &entity_map,
        );

        assert_eq!(target, None);
    }

    #[test]
    fn js_ts_import_source_files_resolves_relative_imports_and_re_exports() {
        let candidates = vec![
            "src/a.ts".to_string(),
            "src/b.ts".to_string(),
            "src/c/index.ts".to_string(),
            "src/unused.ts".to_string(),
        ];
        let content = r#"
import { a } from './a';
import DefaultB from "./b";
import './c';
export { a as publicA } from './a';
"#;

        let imports = js_ts_import_source_files_from_content("src/main.ts", content, &candidates);

        assert_eq!(imports, vec!["src/a.ts", "src/b.ts", "src/c/index.ts"]);
    }

    #[test]
    fn js_ts_default_re_export_detection_handles_aliases() {
        assert!(js_ts_has_default_re_export_from_content(
            "export { default } from './target';"
        ));
        assert!(js_ts_has_default_re_export_from_content(
            "export { default as PublicTarget } from './target';"
        ));
        assert!(!js_ts_has_default_re_export_from_content(
            "export { value as PublicTarget } from './target';"
        ));
    }

    #[test]
    fn js_ts_import_source_files_from_filesystem_resolves_existing_relative_files() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("src/c")).unwrap();
        std::fs::write(root.path().join("src/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(root.path().join("src/c/index.ts"), "export const c = 1;\n").unwrap();
        let content = r#"
import { a } from './a';
import './c';
import './missing';
"#;

        let imports =
            js_ts_import_source_files_from_filesystem(root.path(), "src/main.ts", content);

        assert_eq!(imports, vec!["src/a.ts", "src/c/index.ts"]);
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
            JS_TS_EXTENSIONS,
            &entity_map,
        );

        assert_eq!(target, Some(&ids[0]));
    }

    #[test]
    fn bare_import_prefers_ordered_module_variant() {
        let ids = vec![
            "lib.js::function::helper".to_string(),
            "lib.ts::function::helper".to_string(),
        ];
        let entity_map = HashMap::from([
            (ids[0].clone(), entity("lib.js")),
            (ids[1].clone(), entity("lib.ts")),
        ]);

        let target = find_import_target(&ids, "lib", "consumer.ts", JS_TS_EXTENSIONS, &entity_map);

        assert_eq!(target, Some(&ids[1]));
    }

    #[test]
    fn explicit_relative_import_resolves_module_variants_before_same_named_ts() {
        for extension in [".mts", ".cts", ".mjs", ".cjs"] {
            let ids = vec![
                "src/config.ts::function::helper".to_string(),
                format!("src/deep/config{extension}::function::helper"),
            ];
            let entity_map = HashMap::from([
                (ids[0].clone(), entity("src/config.ts")),
                (
                    ids[1].clone(),
                    entity(&format!("src/deep/config{extension}")),
                ),
            ]);

            let target = find_import_target(
                &ids,
                &format!("./deep/config{extension}"),
                "src/main.ts",
                JS_TS_EXTENSIONS,
                &entity_map,
            );

            assert_eq!(target, Some(&ids[1]), "extension: {extension}");
        }
    }
}
