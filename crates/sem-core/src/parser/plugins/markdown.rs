use regex::Regex;

use crate::model::entity::{build_entity_id, SemanticEntity};
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
            parent_id: Option<String>,
        }

        let mut sections: Vec<Section> = Vec::new();
        let mut current_section: Option<Section> = None;
        let mut section_stack: Vec<(usize, String)> = Vec::new(); // (level, name)

        for (i, &line) in lines.iter().enumerate() {
            if let Some(caps) = heading_re.captures(line) {
                // Close previous section
                if let Some(sec) = current_section.take() {
                    sections.push(sec);
                }

                let level = caps[1].len();
                let name = caps[2].trim().to_string();

                // Find parent: pop headings with >= level
                while section_stack
                    .last()
                    .map_or(false, |(l, _)| *l >= level)
                {
                    section_stack.pop();
                }

                let parent_id = section_stack.last().map(|(_, parent_name)| {
                    build_entity_id(file_path, "heading", parent_name, None)
                });

                current_section = Some(Section {
                    level,
                    name: name.clone(),
                    start_line: i + 1,
                    lines: vec![line.to_string()],
                    parent_id,
                });

                section_stack.push((level, name));
            } else if let Some(ref mut sec) = current_section {
                sec.lines.push(line.to_string());
            } else {
                // Content before first heading — preamble
                if !line.trim().is_empty() {
                    if current_section.is_none() {
                        current_section = Some(Section {
                            level: 0,
                            name: "(preamble)".to_string(),
                            start_line: i + 1,
                            lines: vec![line.to_string()],
                            parent_id: None,
                        });
                    }
                }
            }
        }

        if let Some(sec) = current_section {
            sections.push(sec);
        }

        for section in &sections {
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
                id: build_entity_id(file_path, entity_type, &section.name, None),
                file_path: file_path.to_string(),
                entity_type: entity_type.to_string(),
                name: section.name.clone(),
                parent_id: section.parent_id.clone(),
                content_hash: content_hash(&section_content),
                content: section_content,
                start_line: section.start_line,
                end_line: section.start_line + section.lines.len() - 1,
                metadata: None,
            });
        }

        entities
    }
}
