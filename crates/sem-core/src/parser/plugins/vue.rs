use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

use super::code::CodeParserPlugin;

pub struct VueParserPlugin;

impl SemanticParserPlugin for VueParserPlugin {
    fn id(&self) -> &str {
        "vue"
    }

    fn extensions(&self) -> &[&str] {
        &[".vue"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let blocks = extract_sfc_blocks(content);

        for block in &blocks {
            let entity = SemanticEntity {
                id: build_entity_id(file_path, "sfc_block", &block.name, None),
                file_path: file_path.to_string(),
                entity_type: "sfc_block".to_string(),
                name: block.name.clone(),
                parent_id: None,
                content_hash: content_hash(&block.full_content),
                structural_hash: None,
                content: block.full_content.clone(),
                start_line: block.start_line,
                end_line: block.end_line,
                metadata: None,
            };

            let block_id = entity.id.clone();
            entities.push(entity);

            // For <script> blocks, delegate to the TS/JS parser for inner entities
            if block.tag == "script" && !block.inner_content.is_empty() {
                let ext = if block.lang == "ts" || block.lang == "tsx" {
                    "script.ts"
                } else {
                    "script.js"
                };
                let virtual_path = format!("{}:{}", file_path, ext);
                let code_plugin = CodeParserPlugin;
                let inner = code_plugin.extract_entities(&block.inner_content, &virtual_path);

                for mut child in inner {
                    // Reparent: set file_path to the real .vue file, set parent to the script block
                    child.file_path = file_path.to_string();
                    child.parent_id = Some(block_id.clone());
                    // Adjust line numbers to be relative to the .vue file
                    child.start_line += block.inner_start_line - 1;
                    child.end_line += block.inner_start_line - 1;
                    // Rebuild ID with correct file_path
                    child.id = build_entity_id(
                        file_path,
                        &child.entity_type,
                        &child.name,
                        child.parent_id.as_deref(),
                    );
                    entities.push(child);
                }
            }
        }

        entities
    }
}

struct SfcBlock {
    tag: String,
    name: String,
    lang: String,
    full_content: String,
    inner_content: String,
    start_line: usize,
    end_line: usize,
    inner_start_line: usize,
}

fn extract_sfc_blocks(content: &str) -> Vec<SfcBlock> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    let mut script_count = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Match opening tags: <template>, <script>, <script setup>, <script lang="ts">, <style>, etc.
        if let Some(tag_info) = parse_opening_tag(trimmed) {
            let start_line = i + 1; // 1-indexed
            let closing_tag = format!("</{}>", tag_info.tag);
            let inner_start = i + 1;

            // Find closing tag
            let mut j = i + 1;
            while j < lines.len() {
                if lines[j].trim().starts_with(&closing_tag) {
                    break;
                }
                j += 1;
            }

            let end_line = if j < lines.len() { j + 1 } else { lines.len() };

            let full_content = lines[i..end_line].join("\n");
            let inner_content = if inner_start < j {
                lines[inner_start..j].join("\n")
            } else {
                String::new()
            };

            let name = if tag_info.tag == "script" {
                script_count += 1;
                if tag_info.setup {
                    "script setup".to_string()
                } else if script_count > 1 {
                    format!("script:{}", script_count)
                } else {
                    "script".to_string()
                }
            } else {
                tag_info.tag.clone()
            };

            blocks.push(SfcBlock {
                tag: tag_info.tag,
                name,
                lang: tag_info.lang,
                full_content,
                inner_content,
                start_line,
                end_line,
                inner_start_line: inner_start + 1, // 1-indexed
            });

            i = end_line;
        } else {
            i += 1;
        }
    }

    blocks
}

struct TagInfo {
    tag: String,
    lang: String,
    setup: bool,
}

fn parse_opening_tag(line: &str) -> Option<TagInfo> {
    let tags = ["template", "script", "style"];
    for tag in &tags {
        let prefix = format!("<{}", tag);
        if !line.starts_with(&prefix) {
            continue;
        }
        // Must be followed by '>', ' ', or nothing more (self-closing not typical for SFC)
        let rest = &line[prefix.len()..];
        if rest.is_empty() || rest.starts_with('>') || rest.starts_with(' ') {
            let lang = extract_attr(line, "lang").unwrap_or_default();
            let setup = line.contains("setup");
            return Some(TagInfo {
                tag: tag.to_string(),
                lang,
                setup,
            });
        }
    }
    None
}

fn extract_attr(tag_line: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    if let Some(start) = tag_line.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = tag_line[value_start..].find('"') {
            return Some(tag_line[value_start..value_start + end].to_string());
        }
    }
    // Also handle single quotes
    let pattern = format!("{}='", attr);
    if let Some(start) = tag_line.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = tag_line[value_start..].find('\'') {
            return Some(tag_line[value_start..value_start + end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vue_sfc_extraction() {
        let code = r#"<template>
  <div class="app">
    <h1>{{ message }}</h1>
  </div>
</template>

<script lang="ts">
import { defineComponent, ref } from 'vue'

export default defineComponent({
  name: 'App',
  setup() {
    const message = ref('Hello')
    return { message }
  }
})

function helper(x: number): number {
  return x * 2
}
</script>

<style scoped>
.app {
  color: red;
}
</style>
"#;
        let plugin = VueParserPlugin;
        let entities = plugin.extract_entities(code, "App.vue");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "Vue entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"template"),
            "Should find template block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"script"),
            "Should find script block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"style"),
            "Should find style block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"helper"),
            "Should find helper function from script, got: {:?}",
            names
        );
    }

    #[test]
    fn test_vue_script_setup() {
        let code = r#"<script setup lang="ts">
import { ref, computed } from 'vue'

const count = ref(0)

function increment() {
  count.value++
}

class Counter {
  value: number = 0
  increment() {
    this.value++
  }
}
</script>

<template>
  <button @click="increment">{{ count }}</button>
</template>
"#;
        let plugin = VueParserPlugin;
        let entities = plugin.extract_entities(code, "Counter.vue");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Vue setup entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"script setup"),
            "Should find script setup block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"template"),
            "Should find template block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"increment"),
            "Should find increment function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Counter"),
            "Should find Counter class, got: {:?}",
            names
        );

        // Functions inside script should be children of the script block
        let increment = entities.iter().find(|e| e.name == "increment").unwrap();
        assert!(
            increment.parent_id.is_some(),
            "increment should be child of script block"
        );
    }

    #[test]
    fn test_vue_line_numbers() {
        let code = "<template>\n  <div>hi</div>\n</template>\n\n<script lang=\"ts\">\nfunction hello() {\n  return 'hello'\n}\n</script>\n";
        let plugin = VueParserPlugin;
        let entities = plugin.extract_entities(code, "test.vue");

        let template = entities.iter().find(|e| e.name == "template").unwrap();
        assert_eq!(template.start_line, 1);
        assert_eq!(template.end_line, 3);

        let script = entities.iter().find(|e| e.name == "script").unwrap();
        assert_eq!(script.start_line, 5);
        assert_eq!(script.end_line, 9);

        // hello function is on lines 6-8 in the .vue file
        let hello = entities.iter().find(|e| e.name == "hello").unwrap();
        assert_eq!(hello.start_line, 6);
        assert_eq!(hello.end_line, 8);
    }
}
