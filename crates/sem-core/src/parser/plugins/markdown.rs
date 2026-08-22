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

        // Fenced code blocks (``` or ~~~): a delimiter line toggles the
        // matching fence open/closed, and everything inside — including
        // `# ...` lines that look like headings — is body content.
        let mut in_backtick_fence = false;
        let mut in_tilde_fence = false;

        // Every non-heading line — a fence delimiter, in-fence content, or
        // ordinary body text — follows one rule: append to the section in
        // progress, or start a "(preamble)" section when no heading has been
        // seen yet. Expressed once here so the fence paths and the plain
        // path cannot drift apart (a leading fenced code block must not
        // silently erase the preamble).
        fn push_to_current_or_start_preamble(
            line: &str,
            line_num: usize,
            file_path: &str,
            sections: &mut Vec<Section>,
            current_section: &mut Option<usize>,
        ) {
            if let Some(index) = *current_section {
                sections[index].lines.push(line.to_string());
            } else if !line.trim().is_empty() {
                sections.push(Section {
                    level: 0,
                    name: "(preamble)".to_string(),
                    start_line: line_num,
                    lines: vec![line.to_string()],
                    base_id: build_entity_id(file_path, "preamble", "(preamble)", None),
                    parent_index: None,
                });
                *current_section = Some(sections.len() - 1);
            }
        }

        for (i, &line) in lines.iter().enumerate() {
            if line.trim().starts_with("```") {
                in_backtick_fence = !in_backtick_fence;
                push_to_current_or_start_preamble(
                    line,
                    i + 1,
                    file_path,
                    &mut sections,
                    &mut current_section,
                );
                continue;
            }
            if line.trim().starts_with("~~~") {
                in_tilde_fence = !in_tilde_fence;
                push_to_current_or_start_preamble(
                    line,
                    i + 1,
                    file_path,
                    &mut sections,
                    &mut current_section,
                );
                continue;
            }
            if in_backtick_fence || in_tilde_fence {
                push_to_current_or_start_preamble(
                    line,
                    i + 1,
                    file_path,
                    &mut sections,
                    &mut current_section,
                );
                continue;
            }

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
            } else {
                push_to_current_or_start_preamble(
                    line,
                    i + 1,
                    file_path,
                    &mut sections,
                    &mut current_section,
                );
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

                kappa: None,
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

    #[test]
    fn headings_inside_fenced_or_indented_code_blocks_are_ignored() {
        // A `# ...` line inside a fenced code block (backtick or tilde
        // fence) is code, not a markdown heading — it must not become an
        // entity, and it must not become the parent of the real headings
        // that follow it. An indented code block containing the same
        // pattern is included too, though the heading regex already
        // requires the line to start at column 0, so it was never
        // misdetected there.
        let content = "\
# Section

```bash
# fake heading one
```

~~~text
# fake heading two
~~~

    # fake heading three

## Real
";
        let plugin = MarkdownParserPlugin;
        let entities = plugin.extract_entities(content, "doc.md");

        let headings: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|entity| entity.entity_type == "heading")
            .collect();
        let names: Vec<&str> = headings.iter().map(|h| h.name.as_str()).collect();

        assert!(
            !names.iter().any(|name| name.starts_with("fake heading")),
            "headings inside fenced/indented code blocks must not be extracted as entities, got: {names:?}"
        );

        let section = headings
            .iter()
            .find(|h| h.name == "Section")
            .expect("Section heading");
        let real = headings
            .iter()
            .find(|h| h.name == "Real")
            .expect("Real heading");
        assert_eq!(
            real.parent_id.as_deref(),
            Some(section.id.as_str()),
            "Real's parent must be the preceding real heading (Section), not a fake heading found inside a code block"
        );
    }

    #[test]
    fn preamble_before_first_heading_survives_a_leading_fenced_code_block() {
        // A markdown file whose very first content is a fenced code block
        // (no heading before it) must still surface a "(preamble)" entity
        // covering everything before the first real heading, exactly as a
        // file that opens with plain prose does — the fence-skip added for
        // the fake-heading fix must not also erase the preamble that would
        // otherwise have been captured by accident.
        //
        // Goes through the registry (not the raw plugin) because start_byte/
        // end_byte are filled at the registry's extractor boundary, not by
        // this plugin itself — this also exercises that fill for the
        // preamble case.
        let content = "```\ncode line\n```\n\n## First\nbody\n";
        let registry = crate::parser::plugins::create_default_registry();
        let entities = registry.extract_entities("doc.md", content);

        let preamble =
            entities
                .iter()
                .find(|entity| entity.entity_type == "preamble")
                .unwrap_or_else(|| {
                    panic!(
                    "a leading fenced code block must still produce a (preamble) entity, got: {:?}",
                    entities.iter().map(|e| (&e.entity_type, &e.name)).collect::<Vec<_>>()
                )
                });
        assert_eq!(preamble.start_line, 1);
        assert_eq!(
            preamble.end_line, 4,
            "preamble must span up to the line before the first real heading"
        );
        assert_eq!(preamble.content, "```\ncode line\n```");
        assert_eq!(preamble.start_byte, Some(0));
        assert_eq!(preamble.end_byte, Some(17));

        let first = entities
            .iter()
            .find(|entity| entity.entity_type == "heading" && entity.name == "First")
            .expect("First heading");
        assert_eq!(first.start_line, 5);
        assert_eq!(first.end_line, 6);
        assert_eq!(first.start_byte, Some(19));
        assert_eq!(first.end_byte, Some(32));
        assert_eq!(first.parent_id, None);
    }
}
