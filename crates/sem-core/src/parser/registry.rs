#[cfg(feature = "parallel")]
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

use crate::model::entity::{build_entity_id, SemanticEntity};

macro_rules! maybe_par_iter {
    ($slice:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $slice.par_iter()
        }
        #[cfg(not(feature = "parallel"))]
        {
            $slice.iter()
        }
    }};
}
use super::plugin::{strip_entity_payloads, SemanticParserPlugin};

pub struct ParserRegistry {
    plugins: Vec<Box<dyn SemanticParserPlugin>>,
    extension_map: HashMap<String, usize>, // ext → index into plugins
    custom_ext_canonical: HashMap<String, String>, // ".mypy" → ".py" (custom → canonical)
    pub custom_test_dirs: Vec<String>,
}

impl ParserRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            extension_map: HashMap::new(),
            custom_ext_canonical: HashMap::new(),
            custom_test_dirs: Vec::new(),
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

    pub fn get_explicit_plugin(&self, file_path: &str) -> Option<&dyn SemanticParserPlugin> {
        for ext in get_extensions(file_path) {
            if let Some(&idx) = self.extension_map.get(&ext) {
                return Some(self.plugins[idx].as_ref());
            }
        }
        None
    }

    pub fn detect_plugin_from_content(&self, content: &str) -> Option<&dyn SemanticParserPlugin> {
        self.detect_from_shebang(content)
    }

    /// Try to detect language from shebang line when extension-based lookup fails.
    /// Call this as a fallback when file content is available.
    pub fn get_plugin_with_content(
        &self,
        file_path: &str,
        content: &str,
    ) -> Option<&dyn SemanticParserPlugin> {
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

    /// Register a custom extension mapping from a .semrc file.
    /// Maps an extension (e.g. ".inc") to an existing plugin by language name.
    pub fn add_extension_mapping(&mut self, ext: &str, language: &str) -> bool {
        let ext = if ext.starts_with('.') {
            ext.to_lowercase()
        } else {
            format!(".{}", ext.to_lowercase())
        };

        // Find plugin index by matching language name against known extensions
        let target_ext = LANG_MAPPING
            .iter()
            .find(|(kw, _)| *kw == language.to_lowercase())
            .map(|(_, e)| *e);

        if let Some(target) = target_ext {
            if let Some(&idx) = self.extension_map.get(target) {
                self.custom_ext_canonical
                    .insert(ext.clone(), target.to_string());
                self.extension_map.insert(ext, idx);
                return true;
            }
        }

        // Also try matching directly against registered extensions
        let direct_ext = format!(".{}", language.to_lowercase());
        if let Some(&idx) = self.extension_map.get(&direct_ext) {
            self.custom_ext_canonical.insert(ext.clone(), direct_ext);
            self.extension_map.insert(ext, idx);
            return true;
        }

        false
    }

    /// Load extension mappings from a .semrc file at the given root directory.
    /// File format (one mapping per line): `.ext = language`
    /// Example:
    ///   .inc = php
    ///   .j = json
    ///   .xyz = cpp
    pub fn load_semrc(&mut self, root: &Path) {
        let semrc_path = root.join(".semrc");
        if !semrc_path.exists() {
            return;
        }
        let content = match std::fs::read_to_string(&semrc_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                if key == "test-dirs" {
                    self.custom_test_dirs = value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                } else {
                    self.add_extension_mapping(key, value);
                }
            }
        }
    }

    /// Load extension mappings from `.gitattributes` at the given root directory.
    /// Parses `*.ext diff=language` and `*.ext linguist-language=Language` patterns.
    /// Only processes `*.ext` glob patterns (not path-based patterns).
    pub fn load_gitattributes(&mut self, root: &Path) {
        let ga_path = root.join(".gitattributes");
        if !ga_path.exists() {
            return;
        }
        let content = match std::fs::read_to_string(&ga_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let pattern = match parts.next() {
                Some(p) => p,
                None => continue,
            };
            // Only handle *.ext patterns
            let ext = match pattern.strip_prefix("*.") {
                Some(e) => e,
                None => continue,
            };
            // Already mapped (e.g. by .semrc which takes priority)
            let ext_key = format!(".{}", ext.to_lowercase());
            if self.custom_ext_canonical.contains_key(&ext_key) {
                continue;
            }
            // Look for diff= or linguist-language= attributes
            for attr in parts {
                if let Some(lang) = attr.strip_prefix("diff=") {
                    self.add_extension_mapping(ext, lang);
                    break;
                }
                if let Some(lang) = attr.strip_prefix("linguist-language=") {
                    self.add_extension_mapping(ext, lang);
                    break;
                }
            }
        }
    }

    /// Resolve custom extension mappings in a file path.
    /// E.g. if `.mypy` is mapped to `python` (canonical `.py`),
    /// `"utils.mypy"` becomes `"utils.py"`.
    pub fn resolve_file_path(&self, file_path: &str) -> Option<String> {
        let path = Path::new(file_path);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))?;

        let canonical = self.custom_ext_canonical.get(&ext)?;
        let stem = path.file_stem().and_then(|s| s.to_str())?;

        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(format!("{}/{}{}", parent.display(), stem, canonical))
        } else {
            Some(format!("{}{}", stem, canonical))
        }
    }

    /// Extract entities, transparently handling custom extension mappings.
    /// Uses the resolved path for language detection but restores the original
    /// file path in entity metadata (file_path, id, parent_id).
    pub fn extract_entities(&self, file_path: &str, content: &str) -> Vec<SemanticEntity> {
        let resolved = self.resolve_file_path(file_path);
        let detection_path = resolved.as_deref().unwrap_or(file_path);

        let plugin = match self.get_plugin_with_content(detection_path, content) {
            Some(p) => p,
            None => return Vec::new(),
        };

        let mut entities = plugin.extract_entities(content, detection_path);
        if let Some(ref rp) = resolved {
            fix_entity_paths(&mut entities, file_path, rp);
        }
        entities
    }

    /// Extract listing-only entities without retaining source content or hashes.
    pub fn extract_entities_brief(&self, file_path: &str, content: &str) -> Vec<SemanticEntity> {
        let resolved = self.resolve_file_path(file_path);
        let detection_path = resolved.as_deref().unwrap_or(file_path);

        let plugin = match self.get_plugin_with_content(detection_path, content) {
            Some(p) => p,
            None => return Vec::new(),
        };

        let mut entities = plugin.extract_entities_brief(content, detection_path);
        if let Some(ref rp) = resolved {
            fix_entity_paths(&mut entities, file_path, rp);
        }
        entities
    }

    /// Extract entities with tree, transparently handling custom extension mappings.
    pub fn extract_entities_with_tree(
        &self,
        file_path: &str,
        content: &str,
    ) -> Option<(Vec<SemanticEntity>, Option<tree_sitter::Tree>)> {
        let resolved = self.resolve_file_path(file_path);
        let detection_path = resolved.as_deref().unwrap_or(file_path);

        let plugin = self.get_plugin_with_content(detection_path, content)?;
        let (mut entities, tree) = plugin.extract_entities_with_tree(content, detection_path);
        if let Some(ref rp) = resolved {
            fix_entity_paths(&mut entities, file_path, rp);
        }
        Some((entities, tree))
    }

    /// Extract entities from multiple files in parallel.
    pub fn extract_all_entities(&self, root: &Path, file_paths: &[String]) -> Vec<SemanticEntity> {
        let mut entities: Vec<SemanticEntity> = maybe_par_iter!(file_paths)
            .flat_map(|fp| {
                let full = root.join(fp);
                let content = match std::fs::read_to_string(&full) {
                    Ok(c) => c,
                    Err(_) => return Vec::new(),
                };
                self.extract_entities(fp, &content)
            })
            .collect();
        resolve_go_method_parent_ids(&mut entities);
        entities
    }

    /// Extract listing-only entities from multiple files in parallel.
    pub fn extract_all_entities_brief(
        &self,
        root: &Path,
        file_paths: &[String],
    ) -> Vec<SemanticEntity> {
        let mut entities: Vec<SemanticEntity> = maybe_par_iter!(file_paths)
            .flat_map(|fp| {
                let full = root.join(fp);
                let content = match std::fs::read_to_string(&full) {
                    Ok(c) => c,
                    Err(_) => return Vec::new(),
                };
                if self.needs_content_for_listing_parent_repair(fp, &content) {
                    self.extract_entities(fp, &content)
                } else {
                    self.extract_entities_brief(fp, &content)
                }
            })
            .collect();
        resolve_go_method_parent_ids(&mut entities);
        strip_entity_payloads(&mut entities);
        entities
    }

    fn needs_content_for_listing_parent_repair(&self, file_path: &str, content: &str) -> bool {
        let resolved = self.resolve_file_path(file_path);
        let detection_path = resolved.as_deref().unwrap_or(file_path);
        detection_path.ends_with(".go")
            || detect_ext_from_content(content).as_deref() == Some(".go")
    }
}

pub fn resolve_go_method_parent_ids(entities: &mut [SemanticEntity]) {
    let mut types_by_package: HashMap<(String, String, String), String> = HashMap::new();

    for entity in entities.iter() {
        if !is_go_file(&entity.file_path) || !is_go_receiver_type_entity(entity) {
            continue;
        }

        let package_name = go_package_name(entity).unwrap_or("");

        types_by_package
            .entry((
                go_package_dir(&entity.file_path).to_string(),
                package_name.to_string(),
                entity.name.clone(),
            ))
            .or_insert_with(|| entity.id.clone());
    }

    for entity in entities.iter_mut() {
        if !is_go_file(&entity.file_path) || entity.entity_type != "method" {
            continue;
        }

        let package_name = go_package_name(entity).unwrap_or("");
        let Some(receiver_name) = extract_go_receiver_type_name(&entity.content) else {
            continue;
        };

        let key = (
            go_package_dir(&entity.file_path).to_string(),
            package_name.to_string(),
            receiver_name,
        );

        let Some(parent_id) = types_by_package.get(&key) else {
            continue;
        };

        if entity.parent_id.as_deref() == Some(parent_id.as_str()) {
            continue;
        }

        entity.parent_id = Some(parent_id.clone());
        entity.id = build_entity_id(
            &entity.file_path,
            &entity.entity_type,
            &entity.name,
            Some(parent_id),
        );
    }
}

fn is_go_file(file_path: &str) -> bool {
    file_path.ends_with(".go")
}

fn is_go_receiver_type_entity(entity: &SemanticEntity) -> bool {
    matches!(
        entity.entity_type.as_str(),
        "type" | "struct" | "class" | "interface"
    )
}

fn go_package_name(entity: &SemanticEntity) -> Option<&str> {
    entity
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("go.package"))
        .map(String::as_str)
}

fn go_package_dir(file_path: &str) -> &str {
    file_path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn extract_go_receiver_type_name(content: &str) -> Option<String> {
    let after_func = content.trim_start().strip_prefix("func")?.trim_start();
    let receiver = after_func.strip_prefix('(')?;
    let receiver_end = receiver.find(')')?;
    let receiver = receiver[..receiver_end].trim();
    if receiver.is_empty() {
        return None;
    }

    let receiver_type = receiver.split_whitespace().last().unwrap_or(receiver);

    let receiver_type = receiver_type.trim_start_matches('*').trim();
    let receiver_type = receiver_type
        .split_once('[')
        .map_or(receiver_type, |(name, _)| name)
        .trim();
    let receiver_type = receiver_type
        .rsplit_once('.')
        .map_or(receiver_type, |(_, name)| name)
        .trim();

    (!receiver_type.is_empty()).then(|| receiver_type.to_string())
}

/// Restore original file path in entities when a custom extension mapping was used.
fn fix_entity_paths(entities: &mut [SemanticEntity], original: &str, resolved: &str) {
    for entity in entities {
        entity.file_path = original.to_string();
        entity.id = entity.id.replace(resolved, original);
        if let Some(ref mut pid) = entity.parent_id {
            *pid = pid.replace(resolved, original);
        }
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
    ("shell", ".sh"),
    ("/sh", ".sh"),
    ("node", ".js"),
    ("javascript", ".js"),
    ("typescript", ".ts"),
    ("tsx", ".tsx"),
    ("swift", ".swift"),
    ("elixir", ".ex"),
    ("rust", ".rs"),
    ("go", ".go"),
    ("golang", ".go"),
    ("kotlin", ".kt"),
    ("dart", ".dart"),
    ("php", ".php"),
    ("java", ".java"),
    ("c", ".c"),
    ("cpp", ".cpp"),
    ("c++", ".cpp"),
    ("cs", ".cs"),
    ("csharp", ".cs"),
    ("c#", ".cs"),
    ("fortran", ".f90"),
    ("terraform", ".tf"),
    ("hcl", ".hcl"),
    ("ocaml", ".ml"),
    ("scala", ".scala"),
    ("haskell", ".hs"),
    ("elm", ".elm"),
    ("d", ".d"),
    ("dlang", ".d"),
    ("zig", ".zig"),
    ("xml", ".xml"),
    ("json", ".json"),
    ("yaml", ".yaml"),
    ("yml", ".yaml"),
    ("toml", ".toml"),
    ("markdown", ".md"),
    ("csv", ".csv"),
    ("eruby", ".erb"),
    ("erb", ".erb"),
    ("vue", ".vue"),
    ("svelte", ".svelte"),
];

/// Detect file extension from shebang line, vim modeline, or content heuristics.
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

    // Try content heuristics (first-line markers and early declarations)
    if let Some(ext) = detect_from_content_heuristics(content) {
        return Some(ext);
    }

    None
}

/// High-confidence content-based language detection.
/// Only uses markers with near-zero false-positive rates.
fn detect_from_content_heuristics(content: &str) -> Option<String> {
    let first_line = content.lines().next().unwrap_or("").trim();

    // PHP: opening tag is unambiguous
    if first_line.starts_with("<?php") || first_line.starts_with("<?PHP") {
        return Some(".php".to_string());
    }

    // XML/SVG/HTML: XML declaration or doctype
    if first_line.starts_with("<?xml") {
        return Some(".xml".to_string());
    }
    if first_line.starts_with("<!DOCTYPE") || first_line.starts_with("<!doctype") {
        return Some(".xml".to_string());
    }

    // Scan first ~20 lines for language-specific patterns
    for line in content.lines().take(20) {
        let trimmed = line.trim();

        // PHP: opening tag anywhere in early lines
        if trimmed.starts_with("<?php") || trimmed.starts_with("<?PHP") || trimmed == "<?=" {
            return Some(".php".to_string());
        }

        // C/C++: #include directive
        if trimmed.starts_with("#include ") || trimmed.starts_with("#include\t") {
            // Could be C or C++. Check for C++ indicators.
            if content.lines().take(30).any(|l| {
                let t = l.trim();
                t.starts_with("using namespace")
                    || t.starts_with("class ")
                    || t.starts_with("#include <iostream")
                    || t.starts_with("#include <vector")
                    || t.starts_with("#include <string>")
                    || t.starts_with("#include <memory")
            }) {
                return Some(".cpp".to_string());
            }
            return Some(".c".to_string());
        }

        // Java: package declaration with dots
        if trimmed.starts_with("package ") && trimmed.contains('.') && trimmed.ends_with(';') {
            return Some(".java".to_string());
        }

        // Go: package declaration without dots or semicolons
        if trimmed.starts_with("package ") && !trimmed.contains('.') && !trimmed.contains(';') {
            return Some(".go".to_string());
        }

        // Rust: common top-level declarations
        if (trimmed.starts_with("use std::") || trimmed.starts_with("use crate::"))
            && trimmed.ends_with(';')
        {
            return Some(".rs".to_string());
        }

        // Elixir: defmodule
        if trimmed.starts_with("defmodule ") {
            return Some(".ex".to_string());
        }

        // Kotlin: package with dots but no semicolon (Kotlin doesn't require semicolons)
        if trimmed.starts_with("package ") && trimmed.contains('.') && !trimmed.ends_with(';') {
            return Some(".kt".to_string());
        }

        // C#: using System or namespace with braces
        if trimmed.starts_with("using System") && trimmed.ends_with(';') {
            return Some(".cs".to_string());
        }
        if trimmed.starts_with("namespace ") && trimmed.ends_with('{') {
            // Could be C++ too, but C++ usually has #include before namespace
            // If we got here without matching #include, it's likely C#
            return Some(".cs".to_string());
        }

        // Swift: import Foundation/UIKit/SwiftUI
        if trimmed == "import Foundation"
            || trimmed == "import UIKit"
            || trimmed == "import SwiftUI"
            || trimmed == "import Combine"
        {
            return Some(".swift".to_string());
        }

        // Dart: import 'dart:
        if trimmed.starts_with("import 'dart:") || trimmed.starts_with("import \"dart:") {
            return Some(".dart".to_string());
        }

        // Scala: object/trait at top level
        if trimmed.starts_with("object ") || trimmed.starts_with("trait ") {
            return Some(".scala".to_string());
        }

        // Zig: const std = @import
        if trimmed.contains("@import(") {
            return Some(".zig".to_string());
        }

        // HCL/Terraform: resource/variable/terraform blocks
        if trimmed.starts_with("resource \"")
            || trimmed.starts_with("variable \"")
            || trimmed.starts_with("terraform {")
            || trimmed.starts_with("provider \"")
        {
            return Some(".tf".to_string());
        }

        // Fortran: program/module/subroutine (case-insensitive)
        let lower = trimmed.to_lowercase();
        if lower.starts_with("program ")
            || lower.starts_with("module ")
            || lower.starts_with("subroutine ")
            || lower == "implicit none"
        {
            // "module " could be Ruby, but Ruby uses "module X" without "implicit none"
            // Check for Fortran-specific follow-up
            if lower.starts_with("program ") || lower == "implicit none" {
                return Some(".f90".to_string());
            }
            if content
                .lines()
                .take(20)
                .any(|l| l.trim().to_lowercase() == "implicit none")
            {
                return Some(".f90".to_string());
            }
        }

        // Python: def/class at indentation level 0 with colon
        if (trimmed.starts_with("def ") || trimmed.starts_with("class "))
            && trimmed.ends_with(':')
            && line.starts_with(trimmed.chars().next().unwrap_or(' '))
        {
            return Some(".py".to_string());
        }

        // Ruby: require or module/class without colon (Python uses colon)
        if trimmed.starts_with("require '")
            || trimmed.starts_with("require \"")
            || trimmed.starts_with("require_relative ")
        {
            return Some(".rb".to_string());
        }

        // Perl: use strict/warnings, or variable declarations with sigils
        if trimmed == "use strict;"
            || trimmed == "use warnings;"
            || trimmed.starts_with("my $")
            || trimmed.starts_with("my @")
            || trimmed.starts_with("my %")
        {
            return Some(".pl".to_string());
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
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, content: &str) {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

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

    #[test]
    fn test_detect_php_from_opening_tag() {
        let registry = create_default_registry();
        let content = "<?php\nclass Vendor {\n    function get_name() { return $this->name; }\n}\n";
        let plugin = registry
            .get_plugin_with_content("vendor.inc2", content)
            .expect("should detect PHP");
        let entities = plugin.extract_entities(content, "vendor.inc2");
        assert!(entities.iter().any(|e| e.entity_type == "class"));
    }

    #[test]
    fn test_detect_c_from_include() {
        let registry = create_default_registry();
        let content =
            "#include <stdio.h>\n\nint main() {\n    printf(\"hello\");\n    return 0;\n}\n";
        let plugin = registry
            .get_plugin_with_content("main.xyz", content)
            .expect("should detect C");
        let entities = plugin.extract_entities(content, "main.xyz");
        assert!(entities.iter().any(|e| e.name == "main"));
    }

    #[test]
    fn test_detect_java_from_package() {
        let registry = create_default_registry();
        let content = "package com.example.app;\n\npublic class Main {\n    public static void main(String[] args) {}\n}\n";
        let plugin = registry
            .get_plugin_with_content("Main", content)
            .expect("should detect Java");
        let entities = plugin.extract_entities(content, "Main");
        assert!(entities.iter().any(|e| e.name == "Main"));
    }

    #[test]
    fn test_detect_go_from_package() {
        let registry = create_default_registry();
        let content =
            "package main\n\nimport \"fmt\"\n\nfunc hello() {\n    fmt.Println(\"hi\")\n}\n";
        let plugin = registry
            .get_plugin_with_content("main", content)
            .expect("should detect Go");
        let entities = plugin.extract_entities(content, "main");
        assert!(entities.iter().any(|e| e.name == "hello"));
    }

    #[test]
    fn test_detect_rust_from_use_std() {
        let registry = create_default_registry();
        let content =
            "use std::collections::HashMap;\n\nfn process() {\n    let m = HashMap::new();\n}\n";
        let plugin = registry
            .get_plugin_with_content("lib", content)
            .expect("should detect Rust");
        let entities = plugin.extract_entities(content, "lib");
        assert!(entities.iter().any(|e| e.name == "process"));
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn test_go_method_parent_resolves_across_files() {
        let registry = create_default_registry();
        let dir = TempDir::new().unwrap();
        write_file(&dir, "models.go", "package demo\n\ntype Service struct{}\n");
        write_file(
            &dir,
            "methods.go",
            "package demo\n\nfunc (s *Service) Run() {}\n",
        );

        let entities = registry.extract_all_entities(
            dir.path(),
            &["models.go".to_string(), "methods.go".to_string()],
        );
        let service = entities
            .iter()
            .find(|e| e.name == "Service" && e.file_path == "models.go")
            .expect("Service type should be extracted");
        let run = entities
            .iter()
            .find(|e| e.name == "Run" && e.file_path == "methods.go")
            .expect("Run method should be extracted");

        assert_eq!(run.parent_id.as_deref(), Some(service.id.as_str()));
        assert_eq!(run.id, format!("{}::Run", service.id));
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn test_brief_go_method_parent_resolves_across_files() {
        let registry = create_default_registry();
        let dir = TempDir::new().unwrap();
        write_file(&dir, "models.go", "package demo\n\ntype Service struct{}\n");
        write_file(
            &dir,
            "methods.go",
            "package demo\n\nfunc (s *Service) Run() {}\n",
        );

        let entities = registry.extract_all_entities_brief(
            dir.path(),
            &["models.go".to_string(), "methods.go".to_string()],
        );
        let service = entities
            .iter()
            .find(|e| e.name == "Service" && e.file_path == "models.go")
            .expect("Service type should be extracted");
        let run = entities
            .iter()
            .find(|e| e.name == "Run" && e.file_path == "methods.go")
            .expect("Run method should be extracted");

        assert_eq!(run.parent_id.as_deref(), Some(service.id.as_str()));
        assert!(entities.iter().all(|e| e.content.is_empty()));
        assert!(entities.iter().all(|e| e.content_hash.is_empty()));
        assert!(entities.iter().all(|e| e.structural_hash.is_none()));
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn test_go_method_parent_resolution_is_package_directory_scoped() {
        let registry = create_default_registry();
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "alpha/models.go",
            "package demo\n\ntype Service struct{}\n",
        );
        write_file(
            &dir,
            "alpha/methods.go",
            "package demo\n\nfunc (s *Service) Run() {}\n",
        );
        write_file(
            &dir,
            "beta/models.go",
            "package demo\n\ntype Service struct{}\n",
        );
        write_file(
            &dir,
            "beta/methods.go",
            "package demo\n\nfunc (s *Service) Run() {}\n",
        );

        let entities = registry.extract_all_entities(
            dir.path(),
            &[
                "alpha/models.go".to_string(),
                "alpha/methods.go".to_string(),
                "beta/models.go".to_string(),
                "beta/methods.go".to_string(),
            ],
        );

        let alpha_service = entities
            .iter()
            .find(|e| e.name == "Service" && e.file_path == "alpha/models.go")
            .expect("alpha Service type should be extracted");
        let beta_service = entities
            .iter()
            .find(|e| e.name == "Service" && e.file_path == "beta/models.go")
            .expect("beta Service type should be extracted");
        let alpha_run = entities
            .iter()
            .find(|e| e.name == "Run" && e.file_path == "alpha/methods.go")
            .expect("alpha Run method should be extracted");
        let beta_run = entities
            .iter()
            .find(|e| e.name == "Run" && e.file_path == "beta/methods.go")
            .expect("beta Run method should be extracted");

        assert_eq!(
            alpha_run.parent_id.as_deref(),
            Some(alpha_service.id.as_str())
        );
        assert_eq!(
            beta_run.parent_id.as_deref(),
            Some(beta_service.id.as_str())
        );
    }

    #[test]
    fn test_extension_takes_priority_over_heuristics() {
        let registry = create_default_registry();
        // Content looks like PHP but file has .py extension
        let content = "<?php\nclass Foo {}\n";
        let plugin = registry
            .get_plugin_with_content("script.py", content)
            .expect("should use Python parser");
        assert_eq!(plugin.id(), "code"); // Python uses code plugin, not PHP
    }

    #[test]
    fn test_load_semrc_parses_test_dirs() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, ".semrc", "test-dirs = e2e-tests, smoke, qa\n");
        let mut registry = create_default_registry();
        registry.load_semrc(dir.path());
        assert_eq!(registry.custom_test_dirs, vec!["e2e-tests", "smoke", "qa"]);
    }

    #[test]
    fn test_load_semrc_test_dirs_with_extension_mappings() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            ".semrc",
            ".inc = php\ntest-dirs = custom-tests\n.xyz = cpp\n",
        );
        let mut registry = create_default_registry();
        registry.load_semrc(dir.path());
        assert_eq!(registry.custom_test_dirs, vec!["custom-tests"]);
        assert!(registry.get_plugin("foo.inc").is_some());
        assert!(registry.get_plugin("bar.xyz").is_some());
    }

    #[test]
    fn test_load_semrc_skips_empty_test_dirs() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, ".semrc", "test-dirs = , ,  \n");
        let mut registry = create_default_registry();
        registry.load_semrc(dir.path());
        assert!(registry.custom_test_dirs.is_empty());
    }

    #[test]
    fn test_load_semrc_no_test_dirs_leaves_empty() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, ".semrc", ".inc = php\n");
        let mut registry = create_default_registry();
        registry.load_semrc(dir.path());
        assert!(registry.custom_test_dirs.is_empty());
    }

    #[test]
    fn test_custom_extension_mapping_extracts_entities() {
        let mut registry = create_default_registry();
        registry.add_extension_mapping(".mypy", "python");

        let content = "def hello():\n    print(\"hello world\")\n\nclass Calculator:\n    def multiply(self, a, b):\n        return a * b\n";
        let entities = registry.extract_entities("utils.mypy", content);

        assert!(
            !entities.is_empty(),
            "Should extract entities via custom mapping"
        );
        assert!(
            entities.iter().any(|e| e.name == "hello"),
            "Should find hello function"
        );
        assert!(
            entities.iter().any(|e| e.name == "Calculator"),
            "Should find Calculator class"
        );
        assert!(
            entities.iter().any(|e| e.name == "multiply"),
            "Should find multiply method"
        );

        // File path should preserve the original extension
        for entity in &entities {
            assert_eq!(
                entity.file_path, "utils.mypy",
                "Entity file_path should use original extension"
            );
            assert!(
                entity.id.starts_with("utils.mypy::"),
                "Entity ID should use original file path"
            );
        }
    }
}
