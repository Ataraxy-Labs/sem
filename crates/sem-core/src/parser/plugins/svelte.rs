use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use svelte_syntax::ast::common::{ScriptContext, Span as AstSpan};
use svelte_syntax::ast::modern::{self, Alternate, Fragment, Node};
use svelte_syntax::{SourceId, SourceText, parse_modern_root, parse_svelte};
use tree_sitter::Node as TsNode;

use crate::model::entity::{SemanticEntity, build_entity_id};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::{content_hash, structural_hash};

use super::code::CodeParserPlugin;

pub struct SvelteParserPlugin;

impl SemanticParserPlugin for SvelteParserPlugin {
    fn id(&self) -> &str {
        "svelte"
    }

    fn extensions(&self) -> &[&str] {
        &[
            ".svelte",
            ".svelte.js",
            ".svelte.ts",
            ".svelte.test.js",
            ".svelte.test.ts",
            ".svelte.spec.js",
            ".svelte.spec.ts",
        ]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        if !is_svelte_component_path(file_path) {
            return extract_svelte_module_entities(content, file_path);
        }

        let cst = match parse_svelte(SourceText::new(SourceId::new(0), content, None)) {
            Ok(cst) => cst,
            Err(_) => return Vec::new(),
        };
        let root = match parse_modern_root(content) {
            Ok(root) => root,
            Err(_) => return Vec::new(),
        };

        SvelteLowerer::new(content, file_path, cst).lower_root(&root)
    }
}

#[derive(Clone, Copy)]
enum SvelteEntityKind {
    ModuleFile,
    InstanceScript,
    ModuleScript,
    Style,
    Fragment,
    Element,
    Snippet,
    IfBlock,
    EachBlock,
    KeyBlock,
    AwaitBlock,
    Component,
    SlotElement,
    HeadElement,
    BodyElement,
    WindowElement,
    DocumentElement,
    DynamicComponentElement,
    DynamicElementElement,
    SelfElement,
    FragmentElement,
    BoundaryElement,
    TitleElement,
    RenderTag,
    HtmlTag,
    ConstTag,
    DebugTag,
    ExpressionTag,
}

impl fmt::Display for SvelteEntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ModuleFile => "svelte_module",
            Self::InstanceScript => "svelte_instance_script",
            Self::ModuleScript => "svelte_module_script",
            Self::Style => "svelte_style",
            Self::Fragment => "svelte_fragment",
            Self::Element => "svelte_element",
            Self::Snippet => "svelte_snippet",
            Self::IfBlock => "svelte_if_block",
            Self::EachBlock => "svelte_each_block",
            Self::KeyBlock => "svelte_key_block",
            Self::AwaitBlock => "svelte_await_block",
            Self::Component => "svelte_component",
            Self::SlotElement => "svelte_slot_element",
            Self::HeadElement => "svelte_head",
            Self::BodyElement => "svelte_body",
            Self::WindowElement => "svelte_window",
            Self::DocumentElement => "svelte_document",
            Self::DynamicComponentElement => "svelte_component_dynamic",
            Self::DynamicElementElement => "svelte_element_dynamic",
            Self::SelfElement => "svelte_self",
            Self::FragmentElement => "svelte_fragment_element",
            Self::BoundaryElement => "svelte_boundary",
            Self::TitleElement => "svelte_title_element",
            Self::RenderTag => "svelte_render_tag",
            Self::HtmlTag => "svelte_html_tag",
            Self::ConstTag => "svelte_const_tag",
            Self::DebugTag => "svelte_debug_tag",
            Self::ExpressionTag => "svelte_expression_tag",
        })
    }
}

impl SvelteEntityKind {
    fn metadata_kind(self) -> &'static str {
        match self {
            Self::ModuleFile => "module",
            Self::InstanceScript => "instance_script",
            Self::ModuleScript => "module_script",
            Self::Style => "style",
            Self::Fragment => "fragment",
            Self::Element => "element",
            Self::Snippet => "snippet",
            Self::IfBlock => "if",
            Self::EachBlock => "each",
            Self::KeyBlock => "key",
            Self::AwaitBlock => "await",
            Self::Component => "component",
            Self::SlotElement => "slot",
            Self::HeadElement => "head",
            Self::BodyElement => "body",
            Self::WindowElement => "window",
            Self::DocumentElement => "document",
            Self::DynamicComponentElement => "dynamic_component",
            Self::DynamicElementElement => "dynamic_element",
            Self::SelfElement => "self",
            Self::FragmentElement => "fragment_element",
            Self::BoundaryElement => "boundary",
            Self::TitleElement => "title_element",
            Self::RenderTag => "render",
            Self::HtmlTag => "html",
            Self::ConstTag => "const",
            Self::DebugTag => "debug",
            Self::ExpressionTag => "expression",
        }
    }
}

struct SvelteEntity {
    file_path: String,
    entity_type: String,
    name: String,
    parent_id: Option<String>,
    content: String,
    structural_hash: Option<String>,
    start_line: usize,
    end_line: usize,
    metadata: Option<HashMap<String, String>>,
}

impl From<SvelteEntity> for SemanticEntity {
    fn from(value: SvelteEntity) -> Self {
        SemanticEntity {
            id: build_entity_id(
                &value.file_path,
                &value.entity_type,
                &value.name,
                value.parent_id.as_deref(),
            ),
            file_path: value.file_path,
            entity_type: value.entity_type,
            name: value.name,
            parent_id: value.parent_id,
            content_hash: content_hash(&value.content),
            structural_hash: value.structural_hash,
            content: value.content,
            start_line: value.start_line,
            end_line: value.end_line,
            metadata: value.metadata,
        }
    }
}

struct SvelteLowerer<'a> {
    source: &'a str,
    file_path: &'a str,
    cst: svelte_syntax::cst::Document<'a>,
    entities: Vec<SemanticEntity>,
}

impl<'a> SvelteLowerer<'a> {
    fn new(source: &'a str, file_path: &'a str, cst: svelte_syntax::cst::Document<'a>) -> Self {
        Self {
            source,
            file_path,
            cst,
            entities: Vec::new(),
        }
    }

    fn lower_root(mut self, root: &modern::Root) -> Vec<SemanticEntity> {
        let mut script_counts = HashMap::<String, usize>::new();
        if root.scripts.is_empty() {
            if let Some(script) = root.instance.as_ref() {
                let name = disambiguate_name("script", &mut script_counts);
                self.lower_script(script, name);
            }
            if let Some(script) = root.module.as_ref() {
                let name = disambiguate_name("script module", &mut script_counts);
                self.lower_script(script, name);
            }
        } else {
            for script in &root.scripts {
                let base_name = match script.context {
                    ScriptContext::Default => "script",
                    ScriptContext::Module => "script module",
                };
                let name = disambiguate_name(base_name, &mut script_counts);
                self.lower_script(script, name);
            }
        }

        let mut style_counts = HashMap::<String, usize>::new();
        if root.styles.is_empty() {
            if let Some(style) = root.css.as_ref() {
                let name = disambiguate_name("style", &mut style_counts);
                self.lower_style(style, name);
            }
        } else {
            for style in &root.styles {
                let name = disambiguate_name("style", &mut style_counts);
                self.lower_style(style, name);
            }
        }
        if let Some(fragment_id) = self.lower_fragment_entity(&root.fragment, None, "fragment") {
            self.lower_fragment_nodes(&root.fragment, &fragment_id);
        }

        self.entities
    }

    fn lower_script(&mut self, script: &modern::Script, name: String) {
        let kind = match script.context {
            ScriptContext::Default => SvelteEntityKind::InstanceScript,
            ScriptContext::Module => SvelteEntityKind::ModuleScript,
        };

        let mut metadata = base_metadata(kind);
        metadata.insert(
            "svelte.context".to_string(),
            match script.context {
                ScriptContext::Default => "default".to_string(),
                ScriptContext::Module => "module".to_string(),
            },
        );

        let open_tag = slice(self.source, script.start, script.content_start);
        if let Some(lang) = extract_attr(&open_tag, "lang") {
            metadata.insert("svelte.lang".to_string(), lang);
        }

        let entity = self.make_entity(
            kind,
            name,
            None,
            script.start,
            script.end,
            self.exact_structural_hash(script.start, script.end),
            Some(metadata),
        );
        let block_id = entity.id.clone();
        let virtual_path = script_virtual_path(self.file_path, &open_tag);
        let inner_content = slice(self.source, script.content_start, script.content_end);
        let inner_start_line = line_number_at_offset(self.source, script.content_start);

        self.entities.push(entity);

        if !inner_content.trim().is_empty() {
            let code_plugin = CodeParserPlugin;
            let inner = code_plugin.extract_entities(&inner_content, &virtual_path);

            for mut child in inner {
                child.file_path = self.file_path.to_string();
                child.parent_id = Some(block_id.clone());
                child.start_line += inner_start_line - 1;
                child.end_line += inner_start_line - 1;
                child.id = build_entity_id(
                    self.file_path,
                    &child.entity_type,
                    &child.name,
                    child.parent_id.as_deref(),
                );
                self.entities.push(child);
            }
        }
    }

    fn lower_style(&mut self, style: &modern::Css, name: String) {
        let entity = self.make_entity(
            SvelteEntityKind::Style,
            name,
            None,
            style.start,
            style.end,
            self.exact_structural_hash(style.start, style.end),
            Some(base_metadata(SvelteEntityKind::Style)),
        );
        self.entities.push(entity);
    }

    fn lower_fragment_entity(
        &mut self,
        fragment: &Fragment,
        parent_id: Option<String>,
        name: &str,
    ) -> Option<String> {
        let (start, end) = fragment_bounds(fragment)?;
        let entity = self.make_entity(
            SvelteEntityKind::Fragment,
            name.to_string(),
            parent_id,
            start,
            end,
            self.fragment_structural_hash(fragment),
            Some(base_metadata(SvelteEntityKind::Fragment)),
        );
        let id = entity.id.clone();
        self.entities.push(entity);
        Some(id)
    }

    fn lower_fragment_nodes(&mut self, fragment: &Fragment, parent_id: &str) {
        for node in &fragment.nodes {
            self.lower_node(node, parent_id);
        }
    }

    fn lower_node(&mut self, node: &Node, parent_id: &str) {
        match node {
            Node::IfBlock(block) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::IfBlock,
                    format!("if@{}", line_number_at_offset(self.source, block.start)),
                    parent_id,
                    block.start,
                    block.end,
                );
                self.lower_fragment_nodes(&block.consequent, &id);
                if let Some(alternate) = block.alternate.as_deref() {
                    match alternate {
                        Alternate::Fragment(fragment) => self.lower_fragment_nodes(fragment, &id),
                        Alternate::IfBlock(elseif_block) => {
                            self.lower_node(&Node::IfBlock(elseif_block.clone()), &id)
                        }
                    }
                }
            }
            Node::EachBlock(block) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::EachBlock,
                    format!("each@{}", line_number_at_offset(self.source, block.start)),
                    parent_id,
                    block.start,
                    block.end,
                );
                self.lower_fragment_nodes(&block.body, &id);
                if let Some(fallback) = block.fallback.as_ref() {
                    self.lower_fragment_nodes(fallback, &id);
                }
            }
            Node::KeyBlock(block) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::KeyBlock,
                    format!("key@{}", line_number_at_offset(self.source, block.start)),
                    parent_id,
                    block.start,
                    block.end,
                );
                self.lower_fragment_nodes(&block.fragment, &id);
            }
            Node::AwaitBlock(block) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::AwaitBlock,
                    format!("await@{}", line_number_at_offset(self.source, block.start)),
                    parent_id,
                    block.start,
                    block.end,
                );
                if let Some(pending) = block.pending.as_ref() {
                    self.lower_fragment_nodes(pending, &id);
                }
                if let Some(then_fragment) = block.then.as_ref() {
                    self.lower_fragment_nodes(then_fragment, &id);
                }
                if let Some(catch_fragment) = block.catch.as_ref() {
                    self.lower_fragment_nodes(catch_fragment, &id);
                }
            }
            Node::SnippetBlock(block) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::Snippet,
                    format!(
                        "snippet@{}",
                        line_number_at_offset(self.source, block.start)
                    ),
                    parent_id,
                    block.start,
                    block.end,
                );
                self.lower_fragment_nodes(&block.body, &id);
            }
            Node::RegularElement(node) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::Element,
                    format!(
                        "{}@{}",
                        node.name,
                        line_number_at_offset(self.source, node.start)
                    ),
                    parent_id,
                    node.start,
                    node.end,
                );
                self.lower_fragment_nodes(&node.fragment, &id);
            }
            Node::Component(node) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::Component,
                    format!(
                        "{}@{}",
                        node.name,
                        line_number_at_offset(self.source, node.start)
                    ),
                    parent_id,
                    node.start,
                    node.end,
                );
                self.lower_fragment_nodes(&node.fragment, &id);
            }
            Node::SlotElement(node) => {
                let id = self.push_node_entity(
                    SvelteEntityKind::SlotElement,
                    format!(
                        "{}@{}",
                        node.name,
                        line_number_at_offset(self.source, node.start)
                    ),
                    parent_id,
                    node.start,
                    node.end,
                );
                self.lower_fragment_nodes(&node.fragment, &id);
            }
            Node::SvelteHead(node) => self.lower_special_element(
                SvelteEntityKind::HeadElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteBody(node) => self.lower_special_element(
                SvelteEntityKind::BodyElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteWindow(node) => self.lower_special_element(
                SvelteEntityKind::WindowElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteDocument(node) => self.lower_special_element(
                SvelteEntityKind::DocumentElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteComponent(node) => self.lower_special_element(
                SvelteEntityKind::DynamicComponentElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteElement(node) => self.lower_special_element(
                SvelteEntityKind::DynamicElementElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteSelf(node) => self.lower_special_element(
                SvelteEntityKind::SelfElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteFragment(node) => self.lower_special_element(
                SvelteEntityKind::FragmentElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::SvelteBoundary(node) => self.lower_special_element(
                SvelteEntityKind::BoundaryElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::TitleElement(node) => self.lower_special_element(
                SvelteEntityKind::TitleElement,
                &node.name,
                &node.fragment,
                node.start,
                node.end,
                parent_id,
            ),
            Node::RenderTag(node) => {
                self.push_node_entity(
                    SvelteEntityKind::RenderTag,
                    format!("render@{}", line_number_at_offset(self.source, node.start)),
                    parent_id,
                    node.start,
                    node.end,
                );
            }
            Node::HtmlTag(node) => {
                self.push_node_entity(
                    SvelteEntityKind::HtmlTag,
                    format!("html@{}", line_number_at_offset(self.source, node.start)),
                    parent_id,
                    node.start,
                    node.end,
                );
            }
            Node::ConstTag(node) => {
                self.push_node_entity(
                    SvelteEntityKind::ConstTag,
                    format!("const@{}", line_number_at_offset(self.source, node.start)),
                    parent_id,
                    node.start,
                    node.end,
                );
            }
            Node::DebugTag(node) => {
                self.push_node_entity(
                    SvelteEntityKind::DebugTag,
                    format!("debug@{}", line_number_at_offset(self.source, node.start)),
                    parent_id,
                    node.start,
                    node.end,
                );
            }
            Node::ExpressionTag(node) => {
                self.push_node_entity(
                    SvelteEntityKind::ExpressionTag,
                    format!(
                        "expression@{}",
                        line_number_at_offset(self.source, node.start)
                    ),
                    parent_id,
                    node.start,
                    node.end,
                );
            }
            Node::Text(_) | Node::Comment(_) => {}
        }
    }

    fn lower_special_element(
        &mut self,
        kind: SvelteEntityKind,
        name: &str,
        fragment: &Fragment,
        start: usize,
        end: usize,
        parent_id: &str,
    ) {
        let id = self.push_node_entity(
            kind,
            format!("{name}@{}", line_number_at_offset(self.source, start)),
            parent_id,
            start,
            end,
        );
        self.lower_fragment_nodes(fragment, &id);
    }

    fn push_node_entity(
        &mut self,
        kind: SvelteEntityKind,
        name: String,
        parent_id: &str,
        start: usize,
        end: usize,
    ) -> String {
        let entity = self.make_entity(
            kind,
            name,
            Some(parent_id.to_string()),
            start,
            end,
            self.exact_structural_hash(start, end),
            Some(base_metadata(kind)),
        );
        let id = entity.id.clone();
        self.entities.push(entity);
        id
    }

    fn make_entity(
        &self,
        kind: SvelteEntityKind,
        name: String,
        parent_id: Option<String>,
        start: usize,
        end: usize,
        structural_hash: Option<String>,
        metadata: Option<HashMap<String, String>>,
    ) -> SemanticEntity {
        SvelteEntity {
            file_path: self.file_path.to_string(),
            entity_type: kind.to_string(),
            name,
            parent_id,
            content: slice(self.source, start, end),
            structural_hash,
            start_line: line_number_at_offset(self.source, start),
            end_line: line_number_at_offset(self.source, end.saturating_sub(1)),
            metadata,
        }
        .into()
    }

    fn fragment_structural_hash(&self, fragment: &Fragment) -> Option<String> {
        let mut parts = Vec::new();

        for node in &fragment.nodes {
            if let Some(hash) = self.node_structural_hash(node) {
                parts.push(hash);
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(content_hash(&format!("fragment:{}", parts.join("|"))))
        }
    }

    fn node_structural_hash(&self, node: &Node) -> Option<String> {
        match node {
            Node::Comment(_) => None,
            Node::Text(node) => {
                let normalized = node.data.split_whitespace().collect::<Vec<_>>().join(" ");
                if normalized.is_empty() {
                    None
                } else {
                    Some(content_hash(&format!("text:{normalized}")))
                }
            }
            _ => self.exact_structural_hash(node.start(), node.end()),
        }
    }

    fn exact_structural_hash(&self, start: usize, end: usize) -> Option<String> {
        let node = exact_node_for_range(self.cst.root_node(), start, end)?;
        Some(structural_hash(node, self.source.as_bytes()))
    }
}

fn extract_svelte_module_entities(content: &str, file_path: &str) -> Vec<SemanticEntity> {
    let lang = if file_path.to_ascii_lowercase().ends_with(".ts") {
        "ts"
    } else {
        "js"
    };

    let mut metadata = base_metadata(SvelteEntityKind::ModuleFile);
    metadata.insert("svelte.lang".to_string(), lang.to_string());

    let module_entity: SemanticEntity = SvelteEntity {
        file_path: file_path.to_string(),
        entity_type: SvelteEntityKind::ModuleFile.to_string(),
        name: "module".to_string(),
        parent_id: None,
        content: content.to_string(),
        structural_hash: None,
        start_line: 1,
        end_line: last_line_number(content),
        metadata: Some(metadata),
    }
    .into();

    let module_id = module_entity.id.clone();
    let code_plugin = CodeParserPlugin;
    let mut entities = vec![module_entity];

    for mut child in code_plugin.extract_entities(content, file_path) {
        child.parent_id = Some(module_id.clone());
        child.id = build_entity_id(
            file_path,
            &child.entity_type,
            &child.name,
            child.parent_id.as_deref(),
        );
        entities.push(child);
    }

    entities
}

fn base_metadata(kind: SvelteEntityKind) -> HashMap<String, String> {
    HashMap::from([("svelte.kind".to_string(), kind.metadata_kind().to_string())])
}

fn exact_node_for_range(root: TsNode<'_>, start: usize, end: usize) -> Option<TsNode<'_>> {
    let mut node = root.descendant_for_byte_range(start, end)?;

    loop {
        if node.start_byte() == start && node.end_byte() == end {
            return Some(node);
        }

        node = node.parent()?;
    }
}

fn fragment_bounds(fragment: &Fragment) -> Option<(usize, usize)> {
    let start = fragment.nodes.first()?.start();
    let end = fragment.nodes.last()?.end();
    Some((start, end))
}

fn slice(source: &str, start: usize, end: usize) -> String {
    let start = start.min(source.len());
    let end = end.min(source.len());
    if start >= end {
        String::new()
    } else {
        source.get(start..end).unwrap_or_default().to_string()
    }
}

fn line_number_at_offset(source: &str, offset: usize) -> usize {
    let bounded = offset.min(source.len());
    source[..bounded].bytes().filter(|b| *b == b'\n').count() + 1
}

fn last_line_number(source: &str) -> usize {
    if source.is_empty() {
        1
    } else {
        line_number_at_offset(source, source.len().saturating_sub(1))
    }
}

fn script_virtual_path(file_path: &str, opening_tag: &str) -> String {
    let ext = match extract_attr(opening_tag, "lang")
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ts") | Some("typescript") => "script.ts",
        _ => "script.js",
    };
    format!("{file_path}:{ext}")
}

fn extract_attr(tag_text: &str, attr: &str) -> Option<String> {
    let double = format!(r#"{attr}=""#);
    if let Some(start) = tag_text.find(&double) {
        let value_start = start + double.len();
        if let Some(end) = tag_text[value_start..].find('"') {
            return Some(tag_text[value_start..value_start + end].to_string());
        }
    }

    let single = format!("{attr}='");
    if let Some(start) = tag_text.find(&single) {
        let value_start = start + single.len();
        if let Some(end) = tag_text[value_start..].find('\'') {
            return Some(tag_text[value_start..value_start + end].to_string());
        }
    }

    None
}

fn disambiguate_name(base_name: &str, counts: &mut HashMap<String, usize>) -> String {
    let count = counts.entry(base_name.to_string()).or_insert(0);
    *count += 1;

    if *count == 1 {
        base_name.to_string()
    } else {
        format!("{base_name}:{}", *count)
    }
}

fn is_svelte_component_path(file_path: &str) -> bool {
    Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(".svelte")
                && !name.ends_with(".svelte.js")
                && !name.ends_with(".svelte.ts")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_svelte_extraction() {
        let code = r#"<script lang="ts">
export function hello() {
  return "hello";
}
</script>

<script context="module">
export class Counter {
  increment() {
    return 1;
  }
}
</script>

<style>
h1 { color: red; }
</style>

{#snippet greet(name: string)}
  <h1>{hello()} {name}</h1>
{/snippet}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Component.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"script"),
            "Should find instance script block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"script module"),
            "Should find module script block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"style"),
            "Should find style block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"fragment"),
            "Should find fragment entity, got: {:?}",
            names
        );
        assert!(
            names.contains(&"hello"),
            "Should find script export, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Counter"),
            "Should find module class, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("snippet@")),
            "Should find snippet block, got: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_line_numbers() {
        let code = r#"<script lang="ts">
function hello() {
  return "hello";
}
</script>

<div>{hello()}</div>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Hello.svelte");

        let script = entities
            .iter()
            .find(|entity| entity.name == "script")
            .unwrap();
        assert_eq!(script.start_line, 1);
        assert_eq!(script.end_line, 5);

        let fragment = entities
            .iter()
            .find(|entity| entity.name == "fragment")
            .unwrap();
        assert_eq!(fragment.start_line, 5);
        assert_eq!(fragment.end_line, 7);

        let hello = entities
            .iter()
            .find(|entity| entity.name == "hello")
            .unwrap();
        assert_eq!(hello.start_line, 2);
        assert_eq!(hello.end_line, 4);
    }

    #[test]
    fn test_svelte_fragment_nodes() {
        let code = r#"<svelte:head>
  <title>Hello</title>
</svelte:head>

{#if visible}
  <Widget />
{/if}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "FragmentNodes.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"fragment"),
            "Should find fragment, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("svelte:head@")),
            "Should find svelte:head entity, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("if@")),
            "Should find if block entity, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("Widget@")),
            "Should find component entity, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("title@")),
            "Should find title element entity, got: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_markup_only_file() {
        let code = r#"<svelte:options runes={true} />
<div class="app">
  {#if visible}
    <p>Hello</p>
  {/if}
</div>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "MarkupOnly.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"fragment"),
            "Should find fragment, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("if@")),
            "Should find if block in fragment, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("div@")),
            "Should find regular element entity, got: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_tag_comment_is_non_structural() {
        let before = r#"<div class="app"></div>"#;
        let after = r#"<div // Svelte 5 tag comment
class="app"></div>"#;
        let plugin = SvelteParserPlugin;
        let before_entities = plugin.extract_entities(before, "Commented.svelte");
        let after_entities = plugin.extract_entities(after, "Commented.svelte");

        let before_div = before_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_element")
            .unwrap();
        let after_div = after_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_element")
            .unwrap();

        assert_ne!(before_div.content_hash, after_div.content_hash);
        assert_eq!(before_div.structural_hash, after_div.structural_hash);

        let before_fragment = before_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_fragment")
            .unwrap();
        let after_fragment = after_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_fragment")
            .unwrap();

        assert_ne!(before_fragment.content_hash, after_fragment.content_hash);
        assert_eq!(
            before_fragment.structural_hash,
            after_fragment.structural_hash
        );
    }

    #[test]
    fn test_svelte_block_tag_comment_is_non_structural() {
        let before = r#"<div class="app"></div>"#;
        let after = r#"<div /* Svelte 5 tag comment */
class="app"></div>"#;
        let plugin = SvelteParserPlugin;
        let before_entities = plugin.extract_entities(before, "Commented.svelte");
        let after_entities = plugin.extract_entities(after, "Commented.svelte");

        let before_div = before_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_element")
            .unwrap();
        let after_div = after_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_element")
            .unwrap();

        assert_ne!(before_div.content_hash, after_div.content_hash);
        assert_eq!(before_div.structural_hash, after_div.structural_hash);

        let before_fragment = before_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_fragment")
            .unwrap();
        let after_fragment = after_entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_fragment")
            .unwrap();

        assert_ne!(before_fragment.content_hash, after_fragment.content_hash);
        assert_eq!(
            before_fragment.structural_hash,
            after_fragment.structural_hash
        );
    }

    #[test]
    fn test_svelte_typescript_module_extension_creates_module_entity() {
        let code = r#"export function createCounter(step: number) {
    let count = $state(0);
    return {
        increment() {
            count += step;
        }
    };
}"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "state.svelte.ts");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();
        let module = entities
            .iter()
            .find(|entity| entity.name == "module")
            .unwrap();

        assert!(
            names.contains(&"createCounter"),
            "Expected TS entities, got: {:?}",
            names
        );
        assert_eq!(module.entity_type, "svelte_module");
        assert!(
            module.parent_id.is_none(),
            "Top-level module entity should not have a parent"
        );
        let create_counter = entities
            .iter()
            .find(|entity| entity.name == "createCounter")
            .unwrap();
        assert_eq!(
            create_counter.parent_id.as_deref(),
            Some(module.id.as_str())
        );
    }

    #[test]
    fn test_svelte_test_extension_creates_module_entity() {
        let code = r#"export function createMultiplier(k) {
    return function apply(value) {
        return value * k;
    };
}"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "multiplier.svelte.test.js");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"module"),
            "Expected module entity, got: {:?}",
            names
        );
        assert!(
            names.contains(&"createMultiplier"),
            "Expected JS entities from .svelte.test.js file, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"fragment"),
            "Svelte module files should not synthesize a fragment, got: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_fixture_svelte_head_from_svelte_repo() {
        let code = r#"<svelte:head>
	<title>Hello world!</title>
	<meta name="description" content="This is where the description goes for SEO" />
</svelte:head>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Head.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();
        let head = entities
            .iter()
            .find(|entity| entity.name.starts_with("svelte:head@"))
            .unwrap();

        assert!(
            names.contains(&"fragment"),
            "Expected fragment extraction, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("svelte:head@")),
            "Expected svelte:head entity, got: {:?}",
            names
        );
        assert_eq!(head.entity_type, "svelte_head");
    }

    #[test]
    fn test_svelte_fixture_script_multiple_from_svelte_repo() {
        let code = r#"<script>
	REPLACEME
</script>
<style>
	SHOULD NOT BE REPLACED
</style>
<script>
	REPLACEMETOO
</script>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Scripts.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"script"),
            "Expected instance script block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"script module") || names.contains(&"style"),
            "Expected multiple top-level block entities, got: {:?}",
            names
        );
        assert!(
            names.contains(&"style"),
            "Expected style block, got: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_fixture_snippets_from_svelte_repo() {
        let code = r#"<script lang="ts"></script>

{#snippet foo(msg: string)}
	<p>{msg}</p>
{/snippet}

{@render foo(msg)}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Snippets.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"script"),
            "Expected script block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"fragment"),
            "Expected fragment entity, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("snippet@")),
            "Expected snippet block, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("render@")),
            "Expected render tag, got: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_fixture_svelte_window_from_svelte_repo() {
        let code = r#"<script>
	function handleKeydown(event) {
		alert(`pressed the ${event.key} key`);
	}
</script>

<svelte:window onkeydown={handleKeydown} />
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Window.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();
        let window = entities
            .iter()
            .find(|entity| entity.name.starts_with("svelte:window@"))
            .unwrap();

        assert!(
            names.contains(&"script"),
            "Expected script block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"handleKeydown"),
            "Expected extracted script function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"fragment"),
            "Expected fragment entity, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("svelte:window@")),
            "Expected svelte:window entity, got: {:?}",
            names
        );
        assert_eq!(window.entity_type, "svelte_window");
    }

    #[test]
    fn test_svelte_fixture_if_block_from_svelte_repo() {
        let code = r#"{#if foo}bar{/if}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "IfBlock.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"fragment"),
            "Expected fragment extraction, got: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("if@")),
            "Expected if block extraction, got: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_fixture_svelte_options_from_svelte_repo() {
        let code = r#"<svelte:options runes={true} namespace="html" css="injected" customElement="my-custom-element" />
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Options.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["fragment"],
            "Expected fragment-only extraction, got: {:?}",
            names
        );
    }
}
