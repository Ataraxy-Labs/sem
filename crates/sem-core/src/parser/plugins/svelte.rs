use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use tree_sitter::{Node as TsNode, Parser};

use crate::model::entity::{build_entity_id, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::{content_hash, structural_hash};

use super::code::CodeParserPlugin;

const SVELTE_KIND_KEY: &str = "svelte.kind";
const SVELTE_CONTEXT_KEY: &str = "svelte.context";
const SVELTE_LANG_KEY: &str = "svelte.lang";

thread_local! {
    static SVELTE_PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_htmlx_svelte::language())
            .expect("failed to load Svelte grammar");
        parser
    });
}

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
        match classify_svelte_file(file_path) {
            Some(SvelteFileKind::Module { lang }) => {
                return extract_svelte_module_entities(content, file_path, lang);
            }
            Some(SvelteFileKind::Component) => {}
            None => return Vec::new(),
        }

        let tree = match SVELTE_PARSER
            .with(|parser| parser.borrow_mut().parse(content.as_bytes(), None))
        {
            Some(tree) => tree,
            None => return Vec::new(),
        };

        let root = tree.root_node();
        SvelteLowerer::new(content, file_path).lower_document(root)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScriptBlockContext {
    Default,
    Module,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScriptLanguage {
    JavaScript,
    TypeScript,
}

impl ScriptLanguage {
    fn from_attr(lang: Option<&str>) -> Self {
        match lang {
            Some(lang)
                if lang.eq_ignore_ascii_case("ts")
                    || lang.eq_ignore_ascii_case("tsx")
                    || lang.eq_ignore_ascii_case("typescript") =>
            {
                Self::TypeScript
            }
            _ => Self::JavaScript,
        }
    }

    fn from_svelte_module_path(file_path: &str) -> Self {
        if ends_with_ignore_ascii_case(file_path, ".svelte.ts")
            || ends_with_ignore_ascii_case(file_path, ".svelte.test.ts")
            || ends_with_ignore_ascii_case(file_path, ".svelte.spec.ts")
        {
            Self::TypeScript
        } else {
            Self::JavaScript
        }
    }

    fn metadata_value(self) -> &'static str {
        match self {
            Self::JavaScript => "js",
            Self::TypeScript => "ts",
        }
    }

    fn virtual_script_extension(self) -> &'static str {
        match self {
            Self::JavaScript => "script.js",
            Self::TypeScript => "script.ts",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SvelteFileKind {
    Component,
    Module { lang: ScriptLanguage },
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
    OptionsElement,
    TitleElement,
}

impl fmt::Display for SvelteEntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SvelteEntityKind {
    fn as_str(self) -> &'static str {
        match self {
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
            Self::OptionsElement => "svelte_options",
            Self::TitleElement => "svelte_title_element",
        }
    }

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
            Self::OptionsElement => "options",
            Self::TitleElement => "title_element",
        }
    }
}

struct ReparentContext<'a> {
    file_path: &'a str,
    parent_id: &'a str,
    start_line_offset: usize,
}

struct SvelteLowerer<'a> {
    source: &'a str,
    source_bytes: &'a [u8],
    file_path: &'a str,
    entities: Vec<SemanticEntity>,
}

impl<'a> SvelteLowerer<'a> {
    fn new(source: &'a str, file_path: &'a str) -> Self {
        Self {
            source,
            source_bytes: source.as_bytes(),
            file_path,
            entities: Vec::new(),
        }
    }

    fn lower_document(mut self, root: TsNode<'_>) -> Vec<SemanticEntity> {
        if root.kind() != "document" {
            return Vec::new();
        }

        let mut script_counts = HashMap::<&'static str, usize>::new();
        let mut style_counts = HashMap::<&'static str, usize>::new();
        let mut fragment_nodes = Vec::new();
        let mut cursor = root.walk();

        for node in root.named_children(&mut cursor) {
            match self.top_level_node_kind(node) {
                TopLevelNodeKind::Script => {
                    let context = self.script_context(node);
                    let base_name = match context {
                        ScriptBlockContext::Default => "script",
                        ScriptBlockContext::Module => "script module",
                    };
                    let name = disambiguate_name(base_name, &mut script_counts);
                    self.lower_script(node, name, context);
                }
                TopLevelNodeKind::Style => {
                    let name = disambiguate_name("style", &mut style_counts);
                    self.lower_style(node, name);
                }
                TopLevelNodeKind::Other => fragment_nodes.push(node),
            }
        }

        if let Some(fragment_id) = self.lower_fragment_entity(&fragment_nodes, None, "fragment") {
            for node in fragment_nodes {
                self.lower_node(node, &fragment_id);
            }
        }

        self.entities
    }

    fn lower_script(&mut self, node: TsNode<'_>, name: String, context: ScriptBlockContext) {
        let kind = match context {
            ScriptBlockContext::Default => SvelteEntityKind::InstanceScript,
            ScriptBlockContext::Module => SvelteEntityKind::ModuleScript,
        };

        let mut metadata = base_metadata(kind);
        metadata.insert(
            SVELTE_CONTEXT_KEY.to_string(),
            match context {
                ScriptBlockContext::Default => "default".to_string(),
                ScriptBlockContext::Module => "module".to_string(),
            },
        );

        let lang = ScriptLanguage::from_attr(self.element_attribute_value(node, "lang"));
        metadata.insert(
            SVELTE_LANG_KEY.to_string(),
            lang.metadata_value().to_string(),
        );

        let entity = self.make_entity(
            kind,
            name,
            None,
            node,
            Some(structural_hash(node, self.source_bytes)),
            Some(metadata),
        );
        let block_id = entity.id.clone();

        self.entities.push(entity);

        let Some(raw_text) = element_raw_text_node(node) else {
            return;
        };

        let inner_content = text_for_node(self.source, raw_text).unwrap_or_default();
        if !inner_content.trim().is_empty() {
            let virtual_path = script_virtual_path(self.file_path, lang);
            let code_plugin = CodeParserPlugin;
            let inner = code_plugin.extract_entities(inner_content, &virtual_path);
            self.reparent_entities(
                inner,
                ReparentContext {
                    file_path: self.file_path,
                    parent_id: &block_id,
                    start_line_offset: self.node_start_line(raw_text) - 1,
                },
            );
        }
    }

    fn lower_style(&mut self, node: TsNode<'_>, name: String) {
        let entity = self.make_entity(
            SvelteEntityKind::Style,
            name,
            None,
            node,
            Some(structural_hash(node, self.source_bytes)),
            Some(base_metadata(SvelteEntityKind::Style)),
        );
        self.entities.push(entity);
    }

    fn lower_fragment_entity<'tree>(
        &mut self,
        nodes: &[TsNode<'tree>],
        parent_id: Option<String>,
        name: &str,
    ) -> Option<String> {
        if !nodes
            .iter()
            .any(|node| self.is_substantive_fragment_node(*node))
        {
            return None;
        }

        let first = *nodes.first()?;
        let last = *nodes.last()?;
        let entity = self.make_ranged_entity(
            SvelteEntityKind::Fragment,
            name.to_string(),
            parent_id,
            first.start_byte(),
            last.end_byte(),
            self.node_start_line(first),
            self.node_end_line(last),
            self.fragment_structural_hash(nodes),
            Some(base_metadata(SvelteEntityKind::Fragment)),
        );
        let id = entity.id.clone();
        self.entities.push(entity);
        Some(id)
    }

    fn lower_markup_children(&mut self, node: TsNode<'_>, parent_id: &str) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if is_semantic_child(child) {
                self.lower_node(child, parent_id);
            }
        }
    }

    fn reparent_entities(&mut self, entities: Vec<SemanticEntity>, context: ReparentContext<'_>) {
        let parent_id = context.parent_id.to_string();
        for mut entity in entities {
            entity.file_path.clear();
            entity.file_path.push_str(context.file_path);
            entity.parent_id = Some(parent_id.clone());
            entity.start_line += context.start_line_offset;
            entity.end_line += context.start_line_offset;
            entity.id = build_entity_id(
                context.file_path,
                &entity.entity_type,
                &entity.name,
                Some(context.parent_id),
            );
            self.entities.push(entity);
        }
    }

    fn lower_node(&mut self, node: TsNode<'_>, parent_id: &str) {
        match node.kind() {
            "if_block" => self.lower_if_block(node, parent_id),
            "each_block" => self.lower_each_block(node, parent_id),
            "key_block" => self.lower_key_block(node, parent_id),
            "await_block" => self.lower_await_block(node, parent_id),
            "snippet_block" => self.lower_snippet_block(node, parent_id),
            "element" => self.lower_element(node, parent_id),
            _ => {}
        }
    }

    fn lower_if_block(&mut self, node: TsNode<'_>, parent_id: &str) {
        let id = self.push_node_entity(
            SvelteEntityKind::IfBlock,
            self.line_named("if", node),
            parent_id,
            node,
        );

        let mut else_ifs = Vec::new();
        let mut else_clause = None;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if is_semantic_child(child) {
                self.lower_node(child, &id);
            }

            match child.kind() {
                "else_if_clause" => else_ifs.push(child),
                "else_clause" => else_clause = Some(child),
                _ => {}
            }
        }
        self.lower_else_if_chain(&else_ifs, else_clause, &id);
    }

    fn lower_else_if_chain<'tree>(
        &mut self,
        clauses: &[TsNode<'tree>],
        else_clause: Option<TsNode<'tree>>,
        parent_id: &str,
    ) {
        if let Some((first, rest)) = clauses.split_first() {
            let entity = self.make_entity(
                SvelteEntityKind::IfBlock,
                self.line_named("if", *first),
                Some(parent_id.to_string()),
                *first,
                Some(structural_hash(*first, self.source_bytes)),
                Some(base_metadata(SvelteEntityKind::IfBlock)),
            );
            let id = entity.id.clone();
            self.entities.push(entity);

            self.lower_markup_children(*first, &id);
            self.lower_else_if_chain(rest, else_clause, &id);
        } else if let Some(else_clause) = else_clause {
            self.lower_markup_children(else_clause, parent_id);
        }
    }

    fn lower_each_block(&mut self, node: TsNode<'_>, parent_id: &str) {
        let id = self.push_node_entity(
            SvelteEntityKind::EachBlock,
            self.line_named("each", node),
            parent_id,
            node,
        );

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if is_semantic_child(child) {
                self.lower_node(child, &id);
            }

            if child.kind() == "else_clause" {
                self.lower_markup_children(child, &id);
                break;
            }
        }
    }

    fn lower_key_block(&mut self, node: TsNode<'_>, parent_id: &str) {
        self.lower_container_node(SvelteEntityKind::KeyBlock, "key", node, parent_id);
    }

    fn lower_await_block(&mut self, node: TsNode<'_>, parent_id: &str) {
        let id = self.push_node_entity(
            SvelteEntityKind::AwaitBlock,
            self.line_named("await", node),
            parent_id,
            node,
        );

        if let Some(pending) = node.child_by_field_name("pending") {
            self.lower_markup_children(pending, &id);
        }
        if let Some(shorthand_children) = node.child_by_field_name("shorthand_children") {
            self.lower_markup_children(shorthand_children, &id);
        }

        let mut cursor = node.walk();
        for branch in node.named_children(&mut cursor) {
            if branch.kind() != "await_branch" {
                continue;
            }
            if let Some(children) = branch.child_by_field_name("children") {
                self.lower_markup_children(children, &id);
            }
        }
    }

    fn lower_snippet_block(&mut self, node: TsNode<'_>, parent_id: &str) {
        self.lower_container_node(SvelteEntityKind::Snippet, "snippet", node, parent_id);
    }

    fn lower_element(&mut self, node: TsNode<'_>, parent_id: &str) {
        let Some(tag_name) = self.element_tag_name(node) else {
            return;
        };

        match classify_element_kind(tag_name) {
            ElementLowering::Ignore => self.lower_markup_children(node, parent_id),
            ElementLowering::Kind(kind) => {
                let id =
                    self.push_node_entity(kind, self.line_named(tag_name, node), parent_id, node);
                self.lower_markup_children(node, &id);
            }
        }
    }

    fn push_node_entity(
        &mut self,
        kind: SvelteEntityKind,
        name: String,
        parent_id: &str,
        node: TsNode<'_>,
    ) -> String {
        let entity = self.make_entity(
            kind,
            name,
            Some(parent_id.to_string()),
            node,
            Some(structural_hash(node, self.source_bytes)),
            Some(base_metadata(kind)),
        );
        let id = entity.id.clone();
        self.entities.push(entity);
        id
    }

    fn lower_container_node(
        &mut self,
        kind: SvelteEntityKind,
        label: &'static str,
        node: TsNode<'_>,
        parent_id: &str,
    ) {
        let id = self.push_node_entity(kind, self.line_named(label, node), parent_id, node);
        self.lower_markup_children(node, &id);
    }

    fn make_entity(
        &self,
        kind: SvelteEntityKind,
        name: String,
        parent_id: Option<String>,
        node: TsNode<'_>,
        structural_hash: Option<String>,
        metadata: Option<HashMap<String, String>>,
    ) -> SemanticEntity {
        self.make_ranged_entity(
            kind,
            name,
            parent_id,
            node.start_byte(),
            node.end_byte(),
            self.node_start_line(node),
            self.node_end_line(node),
            structural_hash,
            metadata,
        )
    }

    fn make_ranged_entity(
        &self,
        kind: SvelteEntityKind,
        name: String,
        parent_id: Option<String>,
        start: usize,
        end: usize,
        start_line: usize,
        end_line: usize,
        structural_hash: Option<String>,
        metadata: Option<HashMap<String, String>>,
    ) -> SemanticEntity {
        let entity_type = kind.as_str().to_string();
        let content = text_for_byte_range(self.source, start, end).to_string();
        SemanticEntity {
            id: build_entity_id(self.file_path, &entity_type, &name, parent_id.as_deref()),
            file_path: self.file_path.to_string(),
            entity_type,
            name,
            parent_id,
            content_hash: content_hash(&content),
            structural_hash,
            content,
            start_line,
            end_line,
            metadata,
        }
    }

    fn node_start_line(&self, node: TsNode<'_>) -> usize {
        node.start_position().row + 1
    }

    fn node_end_line(&self, node: TsNode<'_>) -> usize {
        let end = node.end_byte();
        if end <= node.start_byte() {
            return self.node_start_line(node);
        }

        let end_position = node.end_position();
        if self.source_bytes.get(end - 1) == Some(&b'\n') {
            end_position.row
        } else {
            end_position.row + 1
        }
    }

    fn line_named(&self, prefix: &str, node: TsNode<'_>) -> String {
        format!("{prefix}@{}", self.node_start_line(node))
    }

    fn fragment_structural_hash<'tree>(&self, nodes: &[TsNode<'tree>]) -> Option<String> {
        let mut parts = Vec::new();

        for node in nodes {
            if let Some(hash) = self.node_structural_hash(*node) {
                parts.push(hash);
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(content_hash(&format!("fragment:{}", parts.join("|"))))
        }
    }

    fn node_structural_hash(&self, node: TsNode<'_>) -> Option<String> {
        match node.kind() {
            "comment" | "line_comment" | "block_comment" | "tag_comment" => None,
            "text" => {
                let normalized =
                    normalize_text(text_for_node(self.source, node).unwrap_or_default());
                if normalized.is_empty() {
                    None
                } else {
                    Some(content_hash(&format!("text:{normalized}")))
                }
            }
            _ => Some(structural_hash(node, self.source_bytes)),
        }
    }

    fn is_substantive_fragment_node(&self, node: TsNode<'_>) -> bool {
        match node.kind() {
            "comment" | "line_comment" | "block_comment" | "tag_comment" => false,
            "text" => {
                !normalize_text(text_for_node(self.source, node).unwrap_or_default()).is_empty()
            }
            _ => true,
        }
    }

    fn element_tag_name<'tree>(&self, node: TsNode<'tree>) -> Option<&'a str> {
        let tag = element_tag_node(node)?;
        let name = tag.child_by_field_name("name")?;
        text_for_node(self.source, name)
    }

    fn element_attribute_value<'tree>(&self, node: TsNode<'tree>, attr: &str) -> Option<&'a str> {
        let tag = element_tag_node(node)?;
        tag_attribute_value(tag, attr, self.source)
    }

    fn element_has_attribute(&self, node: TsNode<'_>, attr: &str) -> bool {
        let Some(tag) = element_tag_node(node) else {
            return false;
        };

        tag_has_attribute(tag, attr, self.source)
    }

    fn script_context(&self, node: TsNode<'_>) -> ScriptBlockContext {
        if self
            .element_attribute_value(node, "context")
            .map(|value| value.eq_ignore_ascii_case("module"))
            .unwrap_or(false)
            || self.element_has_attribute(node, "module")
        {
            ScriptBlockContext::Module
        } else {
            ScriptBlockContext::Default
        }
    }

    fn top_level_node_kind(&self, node: TsNode<'_>) -> TopLevelNodeKind {
        if node.kind() != "element" {
            return TopLevelNodeKind::Other;
        }

        match self.element_tag_name(node) {
            Some(name) if name.eq_ignore_ascii_case("script") => TopLevelNodeKind::Script,
            Some(name) if name.eq_ignore_ascii_case("style") => TopLevelNodeKind::Style,
            _ => TopLevelNodeKind::Other,
        }
    }
}

fn extract_svelte_module_entities(
    content: &str,
    file_path: &str,
    lang: ScriptLanguage,
) -> Vec<SemanticEntity> {
    let mut metadata = base_metadata(SvelteEntityKind::ModuleFile);
    metadata.insert(
        SVELTE_LANG_KEY.to_string(),
        lang.metadata_value().to_string(),
    );

    let entity_type = SvelteEntityKind::ModuleFile.as_str().to_string();
    let module_entity = SemanticEntity {
        id: build_entity_id(file_path, &entity_type, "module", None),
        file_path: file_path.to_string(),
        entity_type,
        name: "module".to_string(),
        parent_id: None,
        content_hash: content_hash(content),
        structural_hash: None,
        content: content.to_string(),
        start_line: 1,
        end_line: last_line_number(content),
        metadata: Some(metadata),
    };

    let module_id = module_entity.id.clone();
    let code_plugin = CodeParserPlugin;
    let mut entities = vec![module_entity];

    for mut child in code_plugin.extract_entities(content, file_path) {
        child.parent_id = Some(module_id.clone());
        child.id = build_entity_id(file_path, &child.entity_type, &child.name, Some(&module_id));
        entities.push(child);
    }

    entities
}

fn base_metadata(kind: SvelteEntityKind) -> HashMap<String, String> {
    HashMap::from([(
        SVELTE_KIND_KEY.to_string(),
        kind.metadata_kind().to_string(),
    )])
}

#[derive(Clone, Copy)]
enum ElementLowering {
    Ignore,
    Kind(SvelteEntityKind),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TopLevelNodeKind {
    Script,
    Style,
    Other,
}

fn classify_element_kind(tag_name: &str) -> ElementLowering {
    if let Some(local_name) = tag_name.strip_prefix("svelte:") {
        return match local_name {
            "head" => ElementLowering::Kind(SvelteEntityKind::HeadElement),
            "body" => ElementLowering::Kind(SvelteEntityKind::BodyElement),
            "window" => ElementLowering::Kind(SvelteEntityKind::WindowElement),
            "document" => ElementLowering::Kind(SvelteEntityKind::DocumentElement),
            "component" => ElementLowering::Kind(SvelteEntityKind::DynamicComponentElement),
            "element" => ElementLowering::Kind(SvelteEntityKind::DynamicElementElement),
            "self" => ElementLowering::Kind(SvelteEntityKind::SelfElement),
            "fragment" => ElementLowering::Kind(SvelteEntityKind::FragmentElement),
            "boundary" => ElementLowering::Kind(SvelteEntityKind::BoundaryElement),
            "options" => ElementLowering::Kind(SvelteEntityKind::OptionsElement),
            _ => ElementLowering::Ignore,
        };
    }

    match tag_name {
        "slot" => ElementLowering::Kind(SvelteEntityKind::SlotElement),
        "title" => ElementLowering::Kind(SvelteEntityKind::TitleElement),
        _ if is_component_tag(tag_name) => ElementLowering::Kind(SvelteEntityKind::Component),
        _ => ElementLowering::Kind(SvelteEntityKind::Element),
    }
}

fn is_component_tag(tag_name: &str) -> bool {
    tag_name
        .chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
}

fn is_semantic_child(node: TsNode<'_>) -> bool {
    matches!(
        node.kind(),
        "if_block" | "each_block" | "await_block" | "key_block" | "snippet_block" | "element"
    )
}

fn element_tag_node<'tree>(node: TsNode<'tree>) -> Option<TsNode<'tree>> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "start_tag" | "self_closing_tag"));
    result
}

fn element_raw_text_node<'tree>(node: TsNode<'tree>) -> Option<TsNode<'tree>> {
    let mut cursor = node.walk();
    let raw_text = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "raw_text");
    raw_text
}

fn tag_has_attribute(tag: TsNode<'_>, attr: &str, source: &str) -> bool {
    let mut cursor = tag.walk();
    let has_attribute = tag.named_children(&mut cursor).any(|child| {
        child.kind() == "attribute"
            && child
                .child_by_field_name("name")
                .and_then(|name| text_for_node(source, name))
                .map(|name| name.eq_ignore_ascii_case(attr))
                .unwrap_or(false)
    });
    has_attribute
}

fn tag_attribute_value<'a>(tag: TsNode<'_>, attr: &str, source: &'a str) -> Option<&'a str> {
    let mut cursor = tag.walk();
    for child in tag.named_children(&mut cursor) {
        if child.kind() != "attribute" {
            continue;
        }

        let Some(name) = child.child_by_field_name("name") else {
            continue;
        };
        if !text_for_node(source, name)
            .map(|name| name.eq_ignore_ascii_case(attr))
            .unwrap_or(false)
        {
            continue;
        }

        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        return simple_attribute_value(value, source);
    }

    None
}

fn simple_attribute_value<'a>(node: TsNode<'_>, source: &'a str) -> Option<&'a str> {
    match node.kind() {
        "attribute_value" => text_for_node(source, node),
        "quoted_attribute_value" | "unquoted_attribute_value" => {
            let mut cursor = node.walk();
            let attribute_value = node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "attribute_value")
                .and_then(|child| text_for_node(source, child));
            attribute_value
        }
        _ => None,
    }
}

fn text_for_node<'a>(source: &'a str, node: TsNode<'_>) -> Option<&'a str> {
    Some(text_for_byte_range(
        source,
        node.start_byte(),
        node.end_byte(),
    ))
    .filter(|text| !text.is_empty())
}

fn text_for_byte_range(source: &str, start: usize, end: usize) -> &str {
    let start = start.min(source.len());
    let end = end.min(source.len());
    if start >= end {
        ""
    } else {
        source.get(start..end).unwrap_or_default()
    }
}

fn last_line_number(source: &str) -> usize {
    if source.is_empty() {
        1
    } else {
        source.lines().count().max(1)
    }
}

fn script_virtual_path(file_path: &str, lang: ScriptLanguage) -> String {
    format!("{file_path}:{}", lang.virtual_script_extension())
}

fn normalize_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut saw_text = false;
    let mut pending_space = false;

    for part in text.split_whitespace() {
        if saw_text && pending_space {
            normalized.push(' ');
        }
        normalized.push_str(part);
        saw_text = true;
        pending_space = true;
    }

    normalized
}

fn classify_svelte_file(file_path: &str) -> Option<SvelteFileKind> {
    let name = Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())?;

    if ends_with_ignore_ascii_case(name, ".svelte")
        && !ends_with_ignore_ascii_case(name, ".svelte.js")
        && !ends_with_ignore_ascii_case(name, ".svelte.ts")
        && !ends_with_ignore_ascii_case(name, ".svelte.test.js")
        && !ends_with_ignore_ascii_case(name, ".svelte.test.ts")
        && !ends_with_ignore_ascii_case(name, ".svelte.spec.js")
        && !ends_with_ignore_ascii_case(name, ".svelte.spec.ts")
    {
        Some(SvelteFileKind::Component)
    } else if ends_with_ignore_ascii_case(name, ".svelte.js")
        || ends_with_ignore_ascii_case(name, ".svelte.ts")
        || ends_with_ignore_ascii_case(name, ".svelte.test.js")
        || ends_with_ignore_ascii_case(name, ".svelte.test.ts")
        || ends_with_ignore_ascii_case(name, ".svelte.spec.js")
        || ends_with_ignore_ascii_case(name, ".svelte.spec.ts")
    {
        Some(SvelteFileKind::Module {
            lang: ScriptLanguage::from_svelte_module_path(name),
        })
    } else {
        None
    }
}

fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
    value
        .get(value.len().saturating_sub(suffix.len())..)
        .map(|tail| tail.eq_ignore_ascii_case(suffix))
        .unwrap_or(false)
}

fn disambiguate_name<'a>(base_name: &'a str, counts: &mut HashMap<&'a str, usize>) -> String {
    let count = counts.entry(base_name).or_insert(0);
    *count += 1;

    if *count == 1 {
        base_name.into()
    } else {
        format!("{base_name}:{}", *count)
    }
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
            "missing fragment entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("svelte:head@")),
            "missing svelte:head entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("if@")),
            "missing if-block entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("Widget@")),
            "missing component entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("title@")),
            "missing title entity: {:?}",
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
            "missing fragment entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("svelte:options@")),
            "missing svelte:options entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("if@")),
            "missing if-block entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("div@")),
            "missing element entity: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_tag_comments_are_non_structural() {
        let before = r#"<div class="app"></div>"#;
        let plugin = SvelteParserPlugin;

        for after in [
            r#"<div // Svelte 5 tag comment
class="app"></div>"#,
            r#"<div /* Svelte 5 tag comment */
class="app"></div>"#,
        ] {
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
            "missing TypeScript entities: {:?}",
            names
        );
        assert_eq!(module.entity_type, "svelte_module");
        assert!(
            module.parent_id.is_none(),
            "module entity should not have a parent"
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
            "missing module entity: {:?}",
            names
        );
        assert!(
            names.contains(&"createMultiplier"),
            "missing JavaScript entities: {:?}",
            names
        );
        assert!(
            !names.contains(&"fragment"),
            "unexpected fragment entity for module file: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_head() {
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
            "missing fragment entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("svelte:head@")),
            "missing svelte:head entity: {:?}",
            names
        );
        assert_eq!(head.entity_type, "svelte_head");
    }

    #[test]
    fn test_svelte_multiple_scripts() {
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
            "missing script block: {:?}",
            names
        );
        assert!(
            names.contains(&"script module") || names.contains(&"style"),
            "missing top-level block entities: {:?}",
            names
        );
        assert!(names.contains(&"style"), "missing style block: {:?}", names);
    }

    #[test]
    fn test_svelte_snippet() {
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
            "missing script block: {:?}",
            names
        );
        assert!(
            names.contains(&"fragment"),
            "missing fragment entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("snippet@")),
            "missing snippet block: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("p@")),
            "missing rendered content: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_window() {
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
            "missing script block: {:?}",
            names
        );
        assert!(
            names.contains(&"handleKeydown"),
            "missing extracted function: {:?}",
            names
        );
        assert!(
            names.contains(&"fragment"),
            "missing fragment entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("svelte:window@")),
            "missing svelte:window entity: {:?}",
            names
        );
        assert_eq!(window.entity_type, "svelte_window");
    }

    #[test]
    fn test_svelte_if_block() {
        let code = r#"{#if foo}bar{/if}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "IfBlock.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();

        assert!(
            names.contains(&"fragment"),
            "missing fragment entity: {:?}",
            names
        );
        assert!(
            names.iter().any(|name| name.starts_with("if@")),
            "missing if-block entity: {:?}",
            names
        );
    }

    #[test]
    fn test_svelte_options() {
        let code = r#"<svelte:options runes={true} namespace="html" css="injected" customElement="my-custom-element" />
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Options.svelte");
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();
        let options = entities
            .iter()
            .find(|entity| entity.entity_type == "svelte_options")
            .expect("expected svelte:options entity");

        assert!(
            names.iter().any(|name| name.starts_with("svelte:options@")),
            "missing svelte:options entity: {:?}",
            names
        );
        assert_eq!(
            options
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("svelte.kind"))
                .map(String::as_str),
            Some("options")
        );
        assert_eq!(options.content.trim(), code.trim());
    }

    #[test]
    fn test_svelte_each_block_extraction() {
        let code = r#"<script>
let items = $state(['a', 'b', 'c']);
</script>

{#each items as item, i (item)}
  <li>{i}: {item}</li>
{:else}
  <p>No items</p>
{/each}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Each.svelte");

        let each = entities
            .iter()
            .find(|e| e.entity_type == "svelte_each_block")
            .expect("missing each block");
        assert!(each.name.starts_with("each@"));
        assert_eq!(each.start_line, 5);
        assert_eq!(each.end_line, 9);

        let fragment = entities
            .iter()
            .find(|e| e.entity_type == "svelte_fragment")
            .unwrap();
        assert_eq!(each.parent_id.as_deref(), Some(fragment.id.as_str()));

        let li = entities
            .iter()
            .find(|e| e.name.starts_with("li@"))
            .expect("missing li element inside each block");
        assert_eq!(li.parent_id.as_deref(), Some(each.id.as_str()));

        let p = entities
            .iter()
            .find(|e| e.name.starts_with("p@"))
            .expect("missing fallback element inside each block");
        assert_eq!(
            p.parent_id.as_deref(),
            Some(each.id.as_str()),
            "fallback element should be parented to the each block"
        );
    }

    #[test]
    fn test_svelte_key_block_extraction() {
        let code = r#"{#key value}
  <Widget />
{/key}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Key.svelte");

        let key = entities
            .iter()
            .find(|e| e.entity_type == "svelte_key_block")
            .expect("missing key block");
        assert!(key.name.starts_with("key@"));
        assert_eq!(key.start_line, 1);
        assert_eq!(key.end_line, 3);

        let widget = entities
            .iter()
            .find(|e| e.entity_type == "svelte_component" && e.name.starts_with("Widget@"))
            .expect("missing component inside key block");
        assert_eq!(widget.parent_id.as_deref(), Some(key.id.as_str()));
    }

    #[test]
    fn test_svelte_await_block_extraction() {
        let code = r#"{#await promise}
  <p>Loading...</p>
{:then value}
  <p>{value}</p>
{:catch error}
  <p>{error.message}</p>
{/await}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Await.svelte");

        let await_block = entities
            .iter()
            .find(|e| e.entity_type == "svelte_await_block")
            .expect("missing await block");
        assert!(await_block.name.starts_with("await@"));
        assert_eq!(await_block.start_line, 1);
        assert_eq!(await_block.end_line, 7);

        let ps: Vec<_> = entities
            .iter()
            .filter(|e| e.name.starts_with("p@"))
            .collect();
        assert_eq!(ps.len(), 3, "expected content from all await branches");
        for p in &ps {
            assert_eq!(
                p.parent_id.as_deref(),
                Some(await_block.id.as_str()),
                "await branch content should be parented to the await block"
            );
        }
    }

    #[test]
    fn test_svelte_nested_if_else_chain() {
        let code = r#"{#if a}
  <p>A</p>
{:else if b}
  <p>B</p>
{:else}
  <p>C</p>
{/if}
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "IfElse.svelte");

        let ifs: Vec<_> = entities
            .iter()
            .filter(|e| e.entity_type == "svelte_if_block")
            .collect();
        assert_eq!(ifs.len(), 2, "expected both if and else-if blocks");

        let outer_if = &ifs[0];
        let inner_if = &ifs[1];
        assert_eq!(
            inner_if.parent_id.as_deref(),
            Some(outer_if.id.as_str()),
            "else-if block should be nested under the outer if block"
        );

        let ps: Vec<_> = entities
            .iter()
            .filter(|e| e.name.starts_with("p@"))
            .collect();
        assert_eq!(ps.len(), 3, "expected content from each branch");
    }

    #[test]
    fn test_svelte_structural_hash_stable_across_whitespace() {
        let compact = r#"<div class="app"><span>hello</span></div>"#;
        let spaced = r#"<div class="app">
  <span>hello</span>
</div>"#;

        let plugin = SvelteParserPlugin;
        let compact_entities = plugin.extract_entities(compact, "Compact.svelte");
        let spaced_entities = plugin.extract_entities(spaced, "Spaced.svelte");

        let compact_div = compact_entities
            .iter()
            .find(|e| e.entity_type == "svelte_element" && e.name.starts_with("div@"))
            .unwrap();
        let spaced_div = spaced_entities
            .iter()
            .find(|e| e.entity_type == "svelte_element" && e.name.starts_with("div@"))
            .unwrap();

        assert_ne!(
            compact_div.content_hash, spaced_div.content_hash,
            "content hash should change when source text changes"
        );
        assert_eq!(
            compact_div.structural_hash, spaced_div.structural_hash,
            "structural hash should be stable across whitespace changes"
        );
    }

    #[test]
    fn test_svelte_content_hash_changes_on_logic_change() {
        let before = r#"<script>
function add(a, b) { return a + b; }
</script>
"#;
        let after = r#"<script>
function add(a, b) { return a * b; }
</script>
"#;
        let plugin = SvelteParserPlugin;
        let before_entities = plugin.extract_entities(before, "Calc.svelte");
        let after_entities = plugin.extract_entities(after, "Calc.svelte");

        let before_add = before_entities.iter().find(|e| e.name == "add").unwrap();
        let after_add = after_entities.iter().find(|e| e.name == "add").unwrap();

        assert_ne!(
            before_add.content_hash, after_add.content_hash,
            "function content hash should change with new logic"
        );
        assert_eq!(before_add.entity_type, "function");
        assert_eq!(after_add.entity_type, "function");

        let before_script = before_entities
            .iter()
            .find(|e| e.entity_type == "svelte_instance_script")
            .unwrap();
        let after_script = after_entities
            .iter()
            .find(|e| e.entity_type == "svelte_instance_script")
            .unwrap();
        assert_ne!(
            before_script.content_hash, after_script.content_hash,
            "script content hash should change with new logic"
        );
    }

    #[test]
    fn test_svelte_entity_parent_hierarchy() {
        let code = r#"<script lang="ts">
export function greet(name: string) {
  return `Hello ${name}`;
}
</script>

<main>
  <section>
    <p>{greet("world")}</p>
  </section>
</main>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "App.svelte");

        let script = entities
            .iter()
            .find(|e| e.entity_type == "svelte_instance_script")
            .unwrap();
        assert!(
            script.parent_id.is_none(),
            "script block should be top-level"
        );

        let greet = entities.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(
            greet.parent_id.as_deref(),
            Some(script.id.as_str()),
            "function should be parented to the script block"
        );
        assert_eq!(greet.entity_type, "function");

        let fragment = entities
            .iter()
            .find(|e| e.entity_type == "svelte_fragment")
            .unwrap();
        assert!(fragment.parent_id.is_none(), "fragment should be top-level");

        let main_el = entities
            .iter()
            .find(|e| e.name.starts_with("main@"))
            .unwrap();
        assert_eq!(main_el.parent_id.as_deref(), Some(fragment.id.as_str()));

        let section = entities
            .iter()
            .find(|e| e.name.starts_with("section@"))
            .unwrap();
        assert_eq!(section.parent_id.as_deref(), Some(main_el.id.as_str()));
    }

    #[test]
    fn test_svelte_metadata_fields() {
        let code = r#"<script lang="ts" context="module">
export const VERSION = "1.0";
</script>

<script lang="ts">
let count = $state(0);
</script>

<style>
div { color: red; }
</style>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Meta.svelte");

        let module_script = entities
            .iter()
            .find(|e| e.entity_type == "svelte_module_script")
            .unwrap();
        let meta = module_script.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("svelte.kind").map(|s| s.as_str()),
            Some("module_script")
        );
        assert_eq!(
            meta.get("svelte.context").map(|s| s.as_str()),
            Some("module")
        );
        assert_eq!(meta.get("svelte.lang").map(|s| s.as_str()), Some("ts"));

        let instance_script = entities
            .iter()
            .find(|e| e.entity_type == "svelte_instance_script")
            .unwrap();
        let meta = instance_script.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("svelte.context").map(|s| s.as_str()),
            Some("default")
        );
        assert_eq!(meta.get("svelte.lang").map(|s| s.as_str()), Some("ts"));

        let style = entities
            .iter()
            .find(|e| e.entity_type == "svelte_style")
            .unwrap();
        let meta = style.metadata.as_ref().unwrap();
        assert_eq!(meta.get("svelte.kind").map(|s| s.as_str()), Some("style"));
    }

    #[test]
    fn test_svelte_rune_declarations_in_script() {
        let code = r#"<script lang="ts">
let count = $state(0);
let doubled = $derived(count * 2);

$effect(() => {
  console.log(count);
});

function increment() {
  count++;
}
</script>

<button onclick={increment}>{count} (doubled: {doubled})</button>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Runes.svelte");

        let script_children: Vec<_> = entities
            .iter()
            .filter(|e| {
                e.parent_id
                    .as_ref()
                    .map(|pid| {
                        entities
                            .iter()
                            .any(|p| p.id == *pid && p.entity_type == "svelte_instance_script")
                    })
                    .unwrap_or(false)
            })
            .collect();

        let child_names: Vec<&str> = script_children.iter().map(|e| e.name.as_str()).collect();
        assert!(
            child_names.contains(&"count"),
            "missing count variable: {:?}",
            child_names
        );
        assert!(
            child_names.contains(&"doubled"),
            "missing doubled variable: {:?}",
            child_names
        );
        assert!(
            child_names.contains(&"increment"),
            "missing increment function: {:?}",
            child_names
        );
    }

    #[test]
    fn test_svelte_component_with_children() {
        let code = r#"<Dialog>
  <h2>Title</h2>
  <p>Content</p>
</Dialog>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Composed.svelte");

        let dialog = entities
            .iter()
            .find(|e| e.entity_type == "svelte_component" && e.name.starts_with("Dialog@"))
            .expect("missing Dialog component");

        let h2 = entities
            .iter()
            .find(|e| e.name.starts_with("h2@"))
            .expect("missing h2 inside Dialog");
        assert_eq!(
            h2.parent_id.as_deref(),
            Some(dialog.id.as_str()),
            "h2 should be parented to Dialog"
        );

        let p = entities
            .iter()
            .find(|e| e.name.starts_with("p@"))
            .expect("missing p inside Dialog");
        assert_eq!(p.parent_id.as_deref(), Some(dialog.id.as_str()));
    }

    #[test]
    fn test_svelte_module_file_lang_detection() {
        let ts_code = "export const API_URL: string = 'https://example.com';";
        let js_code = "export const API_URL = 'https://example.com';";

        let plugin = SvelteParserPlugin;
        let ts_entities = plugin.extract_entities(ts_code, "config.svelte.ts");
        let js_entities = plugin.extract_entities(js_code, "config.svelte.js");

        let ts_module = ts_entities
            .iter()
            .find(|e| e.entity_type == "svelte_module")
            .unwrap();
        let ts_meta = ts_module.metadata.as_ref().unwrap();
        assert_eq!(ts_meta.get("svelte.lang").map(|s| s.as_str()), Some("ts"));

        let js_module = js_entities
            .iter()
            .find(|e| e.entity_type == "svelte_module")
            .unwrap();
        let js_meta = js_module.metadata.as_ref().unwrap();
        assert_eq!(js_meta.get("svelte.lang").map(|s| s.as_str()), Some("js"));
    }

    #[test]
    fn test_svelte_empty_component_produces_no_fragment() {
        let code = "";
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Empty.svelte");
        assert!(
            entities.is_empty(),
            "empty component should produce no entities: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_svelte_body_and_document() {
        let code = r#"<svelte:body onscroll={() => {}} />
<svelte:document onfullscreenchange={() => {}} />
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Special.svelte");

        let body = entities
            .iter()
            .find(|e| e.entity_type == "svelte_body")
            .expect("missing svelte:body");
        assert!(body.name.starts_with("svelte:body@"));

        let doc = entities
            .iter()
            .find(|e| e.entity_type == "svelte_document")
            .expect("missing svelte:document");
        assert!(doc.name.starts_with("svelte:document@"));
    }

    #[test]
    fn test_svelte_multiple_scripts_disambiguation() {
        let code = r#"<script>
let a = 1;
</script>
<script>
let b = 2;
</script>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "Multi.svelte");

        let scripts: Vec<_> = entities
            .iter()
            .filter(|e| e.entity_type == "svelte_instance_script")
            .collect();
        assert_eq!(scripts.len(), 2, "expected both script blocks");
        assert_ne!(
            scripts[0].name, scripts[1].name,
            "script block names should be disambiguated"
        );
        assert_eq!(scripts[0].name, "script");
        assert_eq!(scripts[1].name, "script:2");
    }

    #[test]
    fn test_svelte_entity_id_format() {
        let code = r#"<script>
function hello() {}
</script>

<div>text</div>
"#;
        let plugin = SvelteParserPlugin;
        let entities = plugin.extract_entities(code, "src/routes/+page.svelte");

        let script = entities
            .iter()
            .find(|e| e.entity_type == "svelte_instance_script")
            .unwrap();
        assert!(
            script.id.contains("src/routes/+page.svelte"),
            "entity id should include file path: {}",
            script.id
        );
        assert!(
            script.id.contains("svelte_instance_script"),
            "entity id should include entity type: {}",
            script.id
        );

        let hello = entities.iter().find(|e| e.name == "hello").unwrap();
        assert!(
            hello.id.contains("hello"),
            "child entity id should include entity name: {}",
            hello.id
        );
        assert!(
            hello.parent_id.is_some(),
            "script-extracted function should have a parent id"
        );
    }

    use crate::git::types::{FileChange, FileStatus};
    use crate::model::change::ChangeType;
    use crate::parser::differ::compute_semantic_diff;
    use crate::parser::plugins::create_default_registry;

    #[test]
    fn test_svelte_diff_new_file_all_entities_added() {
        let after = r#"<script>
  let count = $state(0);
</script>

<button onclick={() => count++}>{count}</button>"#;

        let registry = create_default_registry();
        let result = compute_semantic_diff(
            &[FileChange {
                file_path: "src/routes/+page.svelte".to_string(),
                status: FileStatus::Added,
                old_file_path: None,
                before_content: None,
                after_content: Some(after.to_string()),
            }],
            &registry,
            Some("abc123"),
            Some("test-author"),
        );

        assert!(result.added_count > 0, "expected added entities");
        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.modified_count, 0);
        assert_eq!(result.file_count, 1);

        assert!(
            result
                .changes
                .iter()
                .all(|c| c.change_type == ChangeType::Added),
            "all changes should be added for a new file: {:?}",
            result
                .changes
                .iter()
                .map(|c| (&c.entity_name, &c.change_type))
                .collect::<Vec<_>>()
        );

        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_name == "script" && c.entity_type == "svelte_instance_script"),
            "expected script entity: {:?}",
            result
                .changes
                .iter()
                .map(|c| (&c.entity_name, &c.entity_type))
                .collect::<Vec<_>>()
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_name == "count" && c.entity_type == "variable"),
            "expected count variable: {:?}",
            result
                .changes
                .iter()
                .map(|c| (&c.entity_name, &c.entity_type))
                .collect::<Vec<_>>()
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.entity_name == "button@5" && c.entity_type == "svelte_element"),
            "expected button@5 element: {:?}",
            result
                .changes
                .iter()
                .map(|c| (&c.entity_name, &c.entity_type))
                .collect::<Vec<_>>()
        );
        for c in &result.changes {
            assert_eq!(c.commit_sha.as_deref(), Some("abc123"));
            assert_eq!(c.author.as_deref(), Some("test-author"));
            assert_eq!(c.file_path, "src/routes/+page.svelte");
        }
    }

}
