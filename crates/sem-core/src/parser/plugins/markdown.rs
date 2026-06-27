use regex::Regex;
use std::collections::HashMap;

use crate::model::entity::{build_entity_id, build_entity_id_disambiguated, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

pub struct MarkdownParserPlugin;

impl SemanticParserPlugin for MarkdownParserPlugin {
    fn id(&self) -> &str {
        "markdown"
    }

    fn extensions(&self) -> &[&str] {
        &[".md", ".mdx"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        let heading_re = Regex::new(r"^(#{1,6})\s+(.+)").unwrap();

        struct Section {
            level: usize,
            name: String,
            start_line: usize,
            lines: Vec<String>,
            base_id: String,
            parent_index: Option<usize>,
        }

        let mut sections: Vec<Section> = Vec::new();
        let mut current_section: Option<usize> = None;
        let mut section_stack: Vec<(usize, usize)> = Vec::new(); // (level, section index)

        for (i, &line) in lines.iter().enumerate() {
            if let Some(caps) = heading_re.captures(line) {
                let level = caps[1].len();
                let name = caps[2].trim().to_string();

                // Find parent: pop headings with >= level
                while section_stack.last().map_or(false, |(l, _)| *l >= level) {
                    section_stack.pop();
                }

                let parent_index = section_stack.last().map(|(_, index)| *index);

                sections.push(Section {
                    level,
                    name: name.clone(),
                    start_line: i + 1,
                    lines: vec![line.to_string()],
                    base_id: build_entity_id(file_path, "heading", &name, None),
                    parent_index,
                });
                let section_index = sections.len() - 1;

                current_section = Some(section_index);
                section_stack.push((level, section_index));
            } else if let Some(index) = current_section {
                sections[index].lines.push(line.to_string());
            } else {
                // Content before first heading — preamble
                if !line.trim().is_empty() {
                    if current_section.is_none() {
                        sections.push(Section {
                            level: 0,
                            name: "(preamble)".to_string(),
                            start_line: i + 1,
                            lines: vec![line.to_string()],
                            base_id: build_entity_id(file_path, "preamble", "(preamble)", None),
                            parent_index: None,
                        });
                        current_section = Some(sections.len() - 1);
                    }
                }
            }
        }

        let mut id_counts: HashMap<&str, usize> = HashMap::new();
        for section in &sections {
            *id_counts.entry(section.base_id.as_str()).or_default() += 1;
        }

        let section_ids: Vec<String> = sections
            .iter()
            .map(|section| {
                if id_counts[section.base_id.as_str()] > 1 {
                    let entity_type = if section.level == 0 {
                        "preamble"
                    } else {
                        "heading"
                    };
                    build_entity_id_disambiguated(
                        file_path,
                        entity_type,
                        &section.name,
                        None,
                        section.start_line,
                    )
                } else {
                    section.base_id.clone()
                }
            })
            .collect();

        for (index, section) in sections.iter().enumerate() {
            let section_content = section.lines.join("\n").trim().to_string();
            if section_content.is_empty() {
                continue;
            }

            let entity_type = if section.level == 0 {
                "preamble"
            } else {
                "heading"
            };

            entities.push(SemanticEntity {
                id: section_ids[index].clone(),
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: section.name.clone(),
                parent_id: section
                    .parent_index
                    .map(|parent_index| section_ids[parent_index].clone()),
                content_hash: content_hash(&section_content),
                structural_hash: None,
                content: section_content,
                start_line: section.start_line,
                end_line: section.start_line + section.lines.len() - 1,
                start_byte: None,
                end_byte: None,
                metadata: None,
            });
        }

        entities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_heading_keeps_legacy_id() {
        let content = "# Overview\n\nbody\n";
        let plugin = MarkdownParserPlugin;
        let entities = plugin.extract_entities(content, "doc.md");

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].id, "doc.md::heading::Overview");
    }

    #[test]
    fn duplicate_heading_names_get_line_disambiguated_ids() {
        let content = "# Same Title\n\nfirst body\n\n# Same Title\n\nsecond body\n";
        let plugin = MarkdownParserPlugin;
        let entities = plugin.extract_entities(content, "doc.md");

        let headings: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|entity| entity.entity_type == "heading")
            .collect();

        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].id, "doc.md::heading::Same Title@L1");
        assert_eq!(headings[1].id, "doc.md::heading::Same Title@L5");
        assert_ne!(headings[0].content_hash, headings[1].content_hash);
    }

    #[test]
    fn duplicate_parent_headings_disambiguate_child_parent_ids() {
        let content = "# Release\n## Fixed\nfirst fix\n# Release\n## Fixed\nsecond fix\n";
        let plugin = MarkdownParserPlugin;
        let entities = plugin.extract_entities(content, "CHANGELOG.md");

        let fixed_sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|entity| entity.name == "Fixed")
            .collect();

        assert_eq!(fixed_sections.len(), 2);
        assert_eq!(
            fixed_sections[0].parent_id.as_deref(),
            Some("CHANGELOG.md::heading::Release@L1")
        );
        assert_eq!(
            fixed_sections[1].parent_id.as_deref(),
            Some("CHANGELOG.md::heading::Release@L4")
        );
    }

    #[test]
    fn duplicate_child_headings_under_unique_parents_keep_distinct_parents() {
        let content = "# Product A\n## Usage\nfirst usage\n# Product B\n## Usage\nsecond usage\n";
        let plugin = MarkdownParserPlugin;
        let entities = plugin.extract_entities(content, "README.md");

        let usage_sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|entity| entity.name == "Usage")
            .collect();

        assert_eq!(usage_sections.len(), 2);
        assert_eq!(usage_sections[0].id, "README.md::heading::Usage@L2");
        assert_eq!(usage_sections[1].id, "README.md::heading::Usage@L5");
        assert_eq!(
            usage_sections[0].parent_id.as_deref(),
            Some("README.md::heading::Product A")
        );
        assert_eq!(
            usage_sections[1].parent_id.as_deref(),
            Some("README.md::heading::Product B")
        );
    }
}
