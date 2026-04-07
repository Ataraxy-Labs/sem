use std::cell::RefCell;
use std::collections::HashMap;

use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

thread_local! {
    static ERB_PARSER: RefCell<tree_sitter::Parser> = RefCell::new({
        let mut p = tree_sitter::Parser::new();
        let lang: tree_sitter::Language = tree_sitter_embedded_template::LANGUAGE.into();
        let _ = p.set_language(&lang);
        p
    });
}

pub struct ErbParserPlugin;

impl SemanticParserPlugin for ErbParserPlugin {
    fn id(&self) -> &str {
        "erb"
    }

    fn extensions(&self) -> &[&str] {
        &[".erb"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Vec::new();
        }

        let mut entities = Vec::new();

        // Top-level template entity
        let template_name = extract_template_name(file_path);
        let template_id = build_entity_id(file_path, "template", &template_name, None);
        entities.push(SemanticEntity {
            id: template_id.clone(),
            file_path: file_path.to_string(),
            entity_type: "template".to_string(),
            name: template_name,
            parent_id: None,
            content: content.to_string(),
            content_hash: content_hash(content),
            structural_hash: None,
            start_line: 1,
            end_line: lines.len(),
            metadata: None,
        });

        // Parse with tree-sitter and extract tags
        let tags = ERB_PARSER.with(|parser| {
            let mut parser = parser.borrow_mut();
            match parser.parse(content.as_bytes(), None) {
                Some(tree) => extract_tags_from_tree(&tree, content),
                None => Vec::new(),
            }
        });

        let mut block_stack: Vec<ErbTag> = Vec::new();
        let mut name_counts: HashMap<String, usize> = HashMap::new();

        for tag in tags {
            match tag.kind {
                TagKind::BlockOpen => {
                    block_stack.push(tag);
                }
                TagKind::BlockClose => {
                    if let Some(opener) = block_stack.pop() {
                        let block_content =
                            lines[opener.start_line - 1..tag.end_line].join("\n");
                        let name = unique_name(&opener.name, &mut name_counts);
                        entities.push(SemanticEntity {
                            id: build_entity_id(
                                file_path,
                                "erb_block",
                                &name,
                                Some(&template_id),
                            ),
                            file_path: file_path.to_string(),
                            entity_type: "erb_block".to_string(),
                            name,
                            parent_id: Some(template_id.clone()),
                            content: block_content.clone(),
                            content_hash: content_hash(&block_content),
                            structural_hash: None,
                            start_line: opener.start_line,
                            end_line: tag.end_line,
                            metadata: None,
                        });
                    }
                }
                TagKind::Expression => {
                    let expr_content =
                        lines[tag.start_line - 1..tag.end_line].join("\n");
                    let name = unique_name(&tag.name, &mut name_counts);
                    entities.push(SemanticEntity {
                        id: build_entity_id(
                            file_path,
                            "erb_expression",
                            &name,
                            Some(&template_id),
                        ),
                        file_path: file_path.to_string(),
                        entity_type: "erb_expression".to_string(),
                        name,
                        parent_id: Some(template_id.clone()),
                        content: expr_content.clone(),
                        content_hash: content_hash(&expr_content),
                        structural_hash: None,
                        start_line: tag.start_line,
                        end_line: tag.end_line,
                        metadata: None,
                    });
                }
                // No separate Code variant needed; expressions cover all non-block tags
            }
        }

        entities
    }
}

// --- Internal types ---

#[derive(Debug)]
enum TagKind {
    BlockOpen,
    BlockClose,
    Expression,
}

#[derive(Debug)]
struct ErbTag {
    kind: TagKind,
    name: String,
    start_line: usize,
    end_line: usize,
}

// --- Helpers ---

fn extract_template_name(file_path: &str) -> String {
    let filename = file_path.rsplit('/').next().unwrap_or(file_path);
    filename.strip_suffix(".erb").unwrap_or(filename).to_string()
}

/// Walk the tree-sitter AST and classify each directive node.
fn extract_tags_from_tree(tree: &tree_sitter::Tree, source: &str) -> Vec<ErbTag> {
    let mut tags = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();

    for node in root.children(&mut cursor) {
        let start_line = node.start_position().row + 1; // 1-indexed
        let end_line = node.end_position().row + 1;

        match node.kind() {
            "directive" | "output_directive" => {
                if let Some(code_text) = code_child_text(&node, source) {
                    let trimmed = code_text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    if let Some(tag) = classify_code(trimmed, start_line, end_line) {
                        tags.push(tag);
                    }
                }
            }
            // comment_directive, content -> skip
            _ => {}
        }
    }

    tags
}

/// Classify a code snippet from inside an ERB tag.
/// Returns None for mid-block keywords (else, elsif, etc.) that should be skipped.
fn classify_code(trimmed: &str, start_line: usize, end_line: usize) -> Option<ErbTag> {
    let first_word = trimmed.split_whitespace().next().unwrap_or("");

    if first_word == "end" {
        Some(ErbTag {
            kind: TagKind::BlockClose,
            name: "end".to_string(),
            start_line,
            end_line,
        })
    } else if is_block_opener(trimmed) {
        Some(ErbTag {
            kind: TagKind::BlockOpen,
            name: truncate_name(trimmed),
            start_line,
            end_line,
        })
    } else if is_mid_block_keyword(first_word) {
        None
    } else {
        // Expression or standalone code
        Some(ErbTag {
            kind: TagKind::Expression,
            name: truncate_name(trimmed),
            start_line,
            end_line,
        })
    }
}

fn code_child_text<'a>(node: &tree_sitter::Node, source: &'a str) -> Option<&'a str> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "code" {
            return child.utf8_text(source.as_bytes()).ok();
        }
    }
    None
}

fn is_block_opener(content: &str) -> bool {
    let first_word = content.split_whitespace().next().unwrap_or("");
    if matches!(
        first_word,
        "if" | "unless" | "for" | "while" | "until" | "case" | "begin"
    ) {
        return true;
    }
    // Catch `.each do |item|`, `.times do`, etc.
    content.split_whitespace().any(|w| w == "do")
}

fn is_mid_block_keyword(word: &str) -> bool {
    matches!(word, "else" | "elsif" | "when" | "rescue" | "ensure")
}

fn truncate_name(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 60 {
        s.to_string()
    } else {
        let boundary = s.floor_char_boundary(57);
        format!("{}...", &s[..boundary])
    }
}

fn unique_name(base: &str, counts: &mut HashMap<String, usize>) -> String {
    let count = counts.entry(base.to_string()).or_insert(0);
    *count += 1;
    if *count > 1 {
        format!("{}#{}", base, count)
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_erb_extraction() {
        let erb = r#"<div class="container">
  <% if @user.admin? %>
    <h1>Admin Panel</h1>
    <%= @user.name %>
  <% else %>
    <p>Access denied</p>
  <% end %>

  <% @items.each do |item| %>
    <li><%= item.title %></li>
  <% end %>

  <%# This is a comment, should be skipped %>
  <% @count = @items.length %>
</div>
"#;
        let plugin = ErbParserPlugin;
        let entities = plugin.extract_entities(erb, "views/dashboard.html.erb");

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "ERB entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        // Template entity
        assert_eq!(entities[0].entity_type, "template");
        assert_eq!(entities[0].name, "dashboard.html");
        assert_eq!(entities[0].start_line, 1);

        // if block (lines 2-7)
        let if_block = entities.iter().find(|e| e.name == "if @user.admin?").unwrap();
        assert_eq!(if_block.entity_type, "erb_block");
        assert_eq!(if_block.start_line, 2);
        assert_eq!(if_block.end_line, 7);
        assert!(if_block.parent_id.is_some());

        // each block (lines 9-11)
        let each_block = entities
            .iter()
            .find(|e| e.name == "@items.each do |item|")
            .unwrap();
        assert_eq!(each_block.entity_type, "erb_block");
        assert_eq!(each_block.start_line, 9);
        assert_eq!(each_block.end_line, 11);

        // Expressions
        assert!(names.contains(&"@user.name"));
        assert!(names.contains(&"item.title"));
        let user_name = entities.iter().find(|e| e.name == "@user.name").unwrap();
        assert_eq!(user_name.entity_type, "erb_expression");
        assert_eq!(user_name.start_line, 4);

        // Standalone code shows as expression
        let code = entities
            .iter()
            .find(|e| e.name == "@count = @items.length")
            .unwrap();
        assert_eq!(code.entity_type, "erb_expression");
        assert_eq!(code.start_line, 14);

        // Comment should be skipped
        assert!(!names.iter().any(|n| n.contains("comment")));

        // else should be skipped (mid-block keyword)
        assert!(!names.iter().any(|n| *n == "else"));
    }

    #[test]
    fn test_erb_nested_blocks() {
        let erb = r#"<% if @show %>
  <% @items.each do |item| %>
    <%= item %>
  <% end %>
<% end %>
"#;
        let plugin = ErbParserPlugin;
        let entities = plugin.extract_entities(erb, "nested.html.erb");

        let blocks: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "erb_block")
            .collect();
        assert_eq!(blocks.len(), 2, "Should have 2 blocks: {:?}",
            blocks.iter().map(|b| &b.name).collect::<Vec<_>>());

        // Inner block (each) closes first
        let each = blocks.iter().find(|b| b.name.contains("each")).unwrap();
        assert_eq!(each.start_line, 2);
        assert_eq!(each.end_line, 4);

        // Outer block (if) closes second
        let if_block = blocks.iter().find(|b| b.name.contains("if")).unwrap();
        assert_eq!(if_block.start_line, 1);
        assert_eq!(if_block.end_line, 5);
    }

    #[test]
    fn test_erb_template_name() {
        assert_eq!(extract_template_name("views/best.html.erb"), "best.html");
        assert_eq!(extract_template_name("loading.erb"), "loading");
        assert_eq!(extract_template_name("app/views/_partial.html.erb"), "_partial.html");
    }

    #[test]
    fn test_erb_dash_variant() {
        // <%- is the whitespace-stripping variant, should produce blocks like <%
        let erb = r#"<header>
  <%- if @show %>
    <%= @title %>
  <%- else %>
    <p>nope</p>
  <%- end if %>
</header>
"#;
        let plugin = ErbParserPlugin;
        let entities = plugin.extract_entities(erb, "test.html.erb");

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!("Dash variant: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>());

        // <%- if %> ... <%- end if %> should create a block
        let if_block = entities.iter().find(|e| e.name == "if @show").unwrap();
        assert_eq!(if_block.entity_type, "erb_block");
        assert_eq!(if_block.start_line, 2);
        assert_eq!(if_block.end_line, 6);

        // else should be skipped
        assert!(!names.iter().any(|n| *n == "else"));
    }

    #[test]
    fn test_erb_duplicate_expressions() {
        let erb = r#"<%= @title %>
<%= @title %>
"#;
        let plugin = ErbParserPlugin;
        let entities = plugin.extract_entities(erb, "test.erb");

        let exprs: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "erb_expression")
            .collect();
        assert_eq!(exprs.len(), 2);
        assert_eq!(exprs[0].name, "@title");
        assert_eq!(exprs[1].name, "@title#2");
    }
}
