use crate::model::entity::{
    build_entity_id, build_entity_id_disambiguated_by_document, SemanticEntity,
};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;
use std::collections::HashMap;

pub struct YamlParserPlugin;

impl SemanticParserPlugin for YamlParserPlugin {
    fn id(&self) -> &str {
        "yaml"
    }

    fn extensions(&self) -> &[&str] {
        &[".yml", ".yaml"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        // Extract top-level keys with proper line ranges by scanning the source text.
        // A top-level key starts a line with no indentation (e.g. "key:" or "key: value").
        // Its range extends until the next top-level key or end of file.
        let lines: Vec<&str> = content.lines().collect();
        let top_level_keys = find_top_level_keys(&lines);

        if top_level_keys.is_empty() {
            // No top-level keys: treat the whole file as a single chunk so
            // changes to comment-only or marker-only YAML files are detected.
            if !content.trim().is_empty() {
                return vec![SemanticEntity {
                    id: build_entity_id(file_path, "chunk", "(document)", None),
                    file_path: file_path.to_string(),
                    entity_type: "chunk".to_string(),
                    name: "(document)".to_string(),
                    parent_id: None,
                    content_hash: content_hash(content),
                    structural_hash: None,

                    kappa: None,
                    content: content.to_string(),
                    start_line: 1,
                    end_line: lines.len(),
                    start_byte: None,
                    end_byte: None,
                    metadata: None,
                }];
            }
            return Vec::new();
        }

        // Determine entity types using serde_yaml for section vs property.
        let section_keys: std::collections::HashSet<String> =
            if let Ok(serde_yaml::Value::Mapping(mapping)) = serde_yaml::from_str(content) {
                mapping
                    .iter()
                    .filter(|(_, v)| v.is_mapping() || v.is_sequence())
                    .filter_map(|(k, _)| k.as_str().map(String::from))
                    .collect()
            } else {
                std::collections::HashSet::new()
            };

        // Base id per top-level key, computed before any disambiguation, so
        // colliding ids can be detected first — the overwhelmingly common
        // single-document (and non-colliding multi-document) case keeps the
        // plain id unchanged (semx-vlg/semx-kkk: a multi-document file's
        // same-named keys in different documents otherwise share one id,
        // making which entity's data — e.g. is_test — survives a
        // corpus-wide id collision depend on processing order).
        let base_ids: Vec<String> = top_level_keys
            .iter()
            .map(|tk| {
                let entity_type = if section_keys.contains(&tk.key) {
                    "section"
                } else {
                    "property"
                };
                build_entity_id(file_path, entity_type, &tk.key, None)
            })
            .collect();
        let mut base_id_counts: HashMap<&str, usize> = HashMap::new();
        for base_id in &base_ids {
            *base_id_counts.entry(base_id.as_str()).or_default() += 1;
        }

        let mut entities = Vec::new();

        // Capture preamble (comments, document markers) before the first key
        if top_level_keys[0].line > 1 {
            let preamble_end = trim_trailing_blanks_yaml(&lines, 1, top_level_keys[0].line);
            if preamble_end >= 1 {
                let preamble_content = lines[..preamble_end].join("\n");
                if !preamble_content.trim().is_empty() {
                    entities.push(SemanticEntity {
                        id: build_entity_id(file_path, "chunk", "(preamble)", None),
                        file_path: file_path.to_string(),
                        entity_type: "chunk".to_string(),
                        name: "(preamble)".to_string(),
                        parent_id: None,
                        content_hash: content_hash(&preamble_content),
                        structural_hash: None,

                        kappa: None,
                        content: preamble_content,
                        start_line: 1,
                        end_line: preamble_end,
                        start_byte: None,
                        end_byte: None,
                        metadata: None,
                    });
                }
            }
        }

        // Tracks how many times a given (doc-disambiguated) id has already
        // been assigned, for the rare residual case a document-index suffix
        // does not resolve: the same key declared twice within the *same*
        // document (already-invalid YAML, but the scanner must still hand
        // back unique ids rather than let a later collision go unnoticed).
        let mut assigned_id_counts: HashMap<String, usize> = HashMap::new();

        for (i, tk) in top_level_keys.iter().enumerate() {
            let end_line = if i + 1 < top_level_keys.len() {
                let next_start = top_level_keys[i + 1].line;
                trim_trailing_blanks_yaml(&lines, tk.line, next_start)
            } else {
                trim_trailing_blanks_yaml(&lines, tk.line, lines.len() + 1)
            };

            let entity_content = lines[tk.line - 1..end_line].join("\n");
            let is_section = section_keys.contains(&tk.key);
            let entity_type = if is_section { "section" } else { "property" };

            let base_id = &base_ids[i];
            let mut id = if base_id_counts[base_id.as_str()] > 1 {
                build_entity_id_disambiguated_by_document(
                    file_path,
                    entity_type,
                    &tk.key,
                    None,
                    tk.doc_index,
                )
            } else {
                base_id.clone()
            };
            let ordinal = assigned_id_counts.entry(id.clone()).or_default();
            if *ordinal > 0 {
                id = format!("{id}#{ordinal}");
            }
            *ordinal += 1;

            // Hash raw text so comment changes within a section are detected.
            entities.push(SemanticEntity {
                id,
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: tk.key.clone(),
                parent_id: None,
                content_hash: content_hash(&entity_content),
                structural_hash: None,

                kappa: None,
                content: entity_content,
                start_line: tk.line,
                end_line,
                start_byte: None,
                end_byte: None,
                metadata: None,
            });
        }

        entities
    }
}

struct TopLevelKey {
    key: String,
    line: usize, // 1-based
    /// Zero-based index of the `---`-separated document this key belongs
    /// to. Always 0 for a single-document file. See
    /// `build_entity_id_disambiguated_by_document`'s doc comment for why
    /// this exists — it lets colliding same-named keys in different
    /// documents get distinct, stable entity ids (semx-vlg/semx-kkk).
    doc_index: usize,
}

/// Find all top-level keys in the YAML source. A top-level key is a line
/// that starts with a non-space, non-comment character and contains a colon.
fn find_top_level_keys(lines: &[&str]) -> Vec<TopLevelKey> {
    let mut keys = Vec::new();
    let mut doc_index = 0usize;
    // Whether the current document has produced a key yet. A `---` marker
    // only starts a *new* document — and only then advances `doc_index` —
    // once the current one actually has content; this keeps a file's
    // conventional leading `---` (Kubernetes manifests, GitHub Actions,
    // Ansible playbooks, ...) from being counted as document 1 when it is
    // really the file's only (0th) document.
    let mut current_doc_has_key = false;
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        // A `---` line starts a new document (YAML document-start marker).
        // Every key seen from here on belongs to the next document index.
        if line.starts_with("---") {
            if current_doc_has_key {
                doc_index += 1;
                current_doc_has_key = false;
            }
            continue;
        }
        // Skip comments and document-end markers
        if line.starts_with('#') || line.starts_with("...") {
            continue;
        }
        // Extract the key (everything before the first ':')
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_string();
            if !key.is_empty() {
                keys.push(TopLevelKey {
                    key,
                    line: i + 1,
                    doc_index,
                });
                current_doc_has_key = true;
            }
        }
    }
    keys
}

fn trim_trailing_blanks_yaml(lines: &[&str], start: usize, next_start: usize) -> usize {
    let mut end = next_start - 1;
    while end > start {
        let trimmed = lines[end - 1].trim();
        if trimmed.is_empty() {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yaml_line_positions() {
        let content = "name: my-app\nversion: 1.0.0\nscripts:\n  build: tsc\n  test: jest\ndescription: a test app\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "config.yaml");

        assert_eq!(entities.len(), 4);

        assert_eq!(entities[0].name, "name");
        assert_eq!(entities[0].start_line, 1);
        assert_eq!(entities[0].end_line, 1);

        assert_eq!(entities[1].name, "version");
        assert_eq!(entities[1].start_line, 2);
        assert_eq!(entities[1].end_line, 2);

        assert_eq!(entities[2].name, "scripts");
        assert_eq!(entities[2].entity_type, "section");
        assert_eq!(entities[2].start_line, 3);
        assert_eq!(entities[2].end_line, 5);

        assert_eq!(entities[3].name, "description");
        assert_eq!(entities[3].start_line, 6);
        assert_eq!(entities[3].end_line, 6);
    }

    #[test]
    fn test_yaml_preamble() {
        let content = "# Config file\n---\nname: my-app\nversion: 1.0.0\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "config.yaml");

        assert_eq!(entities[0].name, "(preamble)");
        assert_eq!(entities[0].entity_type, "chunk");
        assert_eq!(entities[0].start_line, 1);

        assert_eq!(entities[1].name, "name");
        assert_eq!(entities[2].name, "version");
    }

    #[test]
    fn test_yaml_comment_only_file() {
        let content = "# Just a comment\n# Another line\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "notes.yaml");

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "(document)");
        assert_eq!(entities[0].entity_type, "chunk");
    }

    #[test]
    fn test_yaml_comment_changes_detected() {
        let content_a = "name: my-app\n# old comment\nversion: 1.0.0\n";
        let content_b = "name: my-app\n# new comment\nversion: 1.0.0\n";
        let plugin = YamlParserPlugin;
        let entities_a = plugin.extract_entities(content_a, "config.yaml");
        let entities_b = plugin.extract_entities(content_b, "config.yaml");

        // The "name" entity includes the comment line in its range, so
        // its content_hash should differ between versions.
        assert_ne!(entities_a[0].content_hash, entities_b[0].content_hash);
    }

    // ── semx-vlg / semx-kkk: multi-document id collisions ──────────────────

    #[test]
    fn test_yaml_multidoc_same_named_keys_get_distinct_ids() {
        // Two documents, each with a top-level "Args" section and a "name"
        // property — the pattern behind semx-kkk's llvm oracle failure (11
        // "Args" entities across documents in one fixture sharing one id).
        let content = "---\nname: foo\nArgs:\n  x: 1\n---\nname: bar\nArgs:\n  y: 2\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "fixtures/multidoc.yaml");

        let args: Vec<&SemanticEntity> = entities.iter().filter(|e| e.name == "Args").collect();
        assert_eq!(args.len(), 2, "expected one Args entity per document: {entities:?}");
        assert_ne!(
            args[0].id, args[1].id,
            "Args entities from different YAML documents must not collide on id \
             (semx-vlg/semx-kkk) — got the same id {:?} for both, so which \
             document's data (e.g. is_test) survives a corpus-wide id \
             collision would depend on processing order",
            args[0].id
        );

        let names: Vec<&SemanticEntity> = entities.iter().filter(|e| e.name == "name").collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0].id, names[1].id);

        // No two entities in the whole file may share an id.
        let mut ids: Vec<&str> = entities.iter().map(|e| e.id.as_str()).collect();
        let unique_before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            unique_before,
            "duplicate entity ids in a multi-document YAML file"
        );
    }

    #[test]
    fn test_yaml_multidoc_ids_are_deterministic_regardless_of_key_order() {
        // The same fixture, parsed twice, must produce the same ids both
        // times — id assignment is a pure function of the file's own
        // document/key structure, not of any external processing order.
        let content = "---\nname: foo\nArgs:\n  x: 1\n---\nname: bar\nArgs:\n  y: 2\n---\nname: baz\nArgs:\n  z: 3\n";
        let plugin = YamlParserPlugin;
        let first: Vec<String> = plugin
            .extract_entities(content, "fixtures/multidoc3.yaml")
            .into_iter()
            .map(|e| e.id)
            .collect();
        let second: Vec<String> = plugin
            .extract_entities(content, "fixtures/multidoc3.yaml")
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(first, second);

        let mut sorted = first.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), first.len(), "ids must be unique: {first:?}");
    }

    #[test]
    fn test_yaml_multidoc_noncolliding_keys_keep_plain_ids() {
        // A key that appears in only one document of a multi-document file
        // never collides, so its id must stay the same plain form a
        // single-document file would produce — the fix must not churn ids
        // for the common (non-colliding) case even inside a multi-doc file.
        let content = "---\nname: foo\nDescription: unique to doc0\n---\nname: bar\nArgs: {}\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "fixtures/multidoc2.yaml");

        let description = entities
            .iter()
            .find(|e| e.name == "Description")
            .expect("Description entity");
        assert_eq!(
            description.id,
            "fixtures/multidoc2.yaml::property::Description"
        );
    }

    #[test]
    fn test_yaml_single_document_ids_are_unchanged() {
        // Guards the single-document case — by far the common case — against
        // any churn from the multi-document disambiguation fix: ids stay
        // exactly the plain `file::type::key` form with no document suffix.
        let content = "name: my-app\nversion: 1.0.0\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "config.yaml");

        assert_eq!(entities[0].id, "config.yaml::property::name");
        assert_eq!(entities[1].id, "config.yaml::property::version");
    }

    #[test]
    fn test_yaml_leading_document_marker_is_still_document_zero() {
        // A single document that merely opens with the conventional leading
        // `---` (Kubernetes manifests, GitHub Actions, Ansible playbooks)
        // must not be miscounted as document 1 — it is the file's only
        // document, so ids stay the plain, undisambiguated form. (The lone
        // `---` line itself still becomes a `(preamble)` chunk entity, as it
        // does today — unrelated to document counting.)
        let content = "---\nname: my-app\nversion: 1.0.0\n";
        let plugin = YamlParserPlugin;
        let entities = plugin.extract_entities(content, "config.yaml");

        let name = entities.iter().find(|e| e.name == "name").expect("name entity");
        let version = entities
            .iter()
            .find(|e| e.name == "version")
            .expect("version entity");
        assert_eq!(name.id, "config.yaml::property::name");
        assert_eq!(version.id, "config.yaml::property::version");
    }
}
