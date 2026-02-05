use std::collections::HashMap;

use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

pub struct CsvParserPlugin;

impl SemanticParserPlugin for CsvParserPlugin {
    fn id(&self) -> &str {
        "csv"
    }

    fn extensions(&self) -> &[&str] {
        &[".csv", ".tsv"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return entities;
        }

        let is_tsv = file_path.ends_with(".tsv");
        let separator = if is_tsv { '\t' } else { ',' };

        let headers = parse_csv_line(lines[0], separator);

        for (i, &line) in lines.iter().enumerate().skip(1) {
            let cells = parse_csv_line(line, separator);
            let row_id = if cells.first().map_or(true, |c| c.is_empty()) {
                format!("row_{i}")
            } else {
                cells[0].clone()
            };
            let name = format!("row[{row_id}]");

            let mut metadata = HashMap::new();
            for (j, header) in headers.iter().enumerate() {
                metadata.insert(
                    header.clone(),
                    cells.get(j).cloned().unwrap_or_default(),
                );
            }

            entities.push(SemanticEntity {
                id: build_entity_id(file_path, "row", &name, None),
                file_path: file_path.to_string(),
                entity_type: "row".to_string(),
                name,
                parent_id: None,
                content_hash: content_hash(line),
                content: line.to_string(),
                start_line: i + 1,
                end_line: i + 1,
                metadata: Some(metadata),
            });
        }

        entities
    }
}

fn parse_csv_line(line: &str, separator: char) -> Vec<String> {
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = line.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if in_quotes {
            if ch == '"' && chars.get(i + 1) == Some(&'"') {
                current.push('"');
                i += 1;
            } else if ch == '"' {
                in_quotes = false;
            } else {
                current.push(ch);
            }
        } else if ch == '"' {
            in_quotes = true;
        } else if ch == separator {
            cells.push(current.trim().to_string());
            current = String::new();
        } else {
            current.push(ch);
        }
        i += 1;
    }
    cells.push(current.trim().to_string());
    cells
}
