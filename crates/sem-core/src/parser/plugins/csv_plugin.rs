use std::collections::BTreeMap;

use crate::model::entity::{build_entity_id, disambiguate_colliding_entity_ids, SemanticEntity};
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

            let mut metadata = BTreeMap::new();
            for (j, header) in headers.iter().enumerate() {
                metadata.insert(header.clone(), cells.get(j).cloned().unwrap_or_default());
            }

            entities.push(SemanticEntity {
                id: build_entity_id(file_path, "row", &name, None),
                file_path: file_path.to_string(),
                entity_type: "row".to_string(),
                name,
                parent_id: None,
                content_hash: content_hash(line),
                structural_hash: None,

                kappa: None,
                content: line.to_string(),
                start_line: i + 1,
                end_line: i + 1,
                start_byte: None,
                end_byte: None,
                metadata: Some(metadata),
            });
        }

        disambiguate_colliding_entity_ids(&mut entities);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// RED: two data rows that share the same first-cell value
    /// collide on `row[<value>]` because `csv_plugin` calls the plain
    /// `build_entity_id` with no disambiguation post-pass (unlike the code
    /// plugin's `entity_extractor`, which disambiguates colliding ids with
    /// an `@L{line}` suffix before returning). Both rows must survive
    /// extraction as distinct entities with distinct ids.
    #[test]
    fn duplicate_first_cell_rows_get_unique_entity_ids() {
        let content = "name,age\nalice,30\nalice,41\nbob,22\n";
        let plugin = CsvParserPlugin;
        let entities = plugin.extract_entities(content, "people.csv");

        assert_eq!(
            entities.len(),
            3,
            "expected one entity per data row; got: {:?}",
            entities.iter().map(|e| &e.id).collect::<Vec<_>>()
        );

        let ids: HashSet<&str> = entities.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids.len(),
            entities.len(),
            "duplicate entity ids for rows sharing a first-cell value: {:?}",
            entities.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
    }
}
