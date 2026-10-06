//! Protect original inline elements while translating a whole paragraph.

use std::fmt::Write;
use std::sync::OnceLock;

use kuchiki::{NodeData, NodeRef};
use regex::Regex;

use crate::config::{OutputMode, TranslationOptions};
use crate::core::BabelEbookError;

use super::insertion::clone_subtree;
use super::selection::{build_skip_set, node_ptr, SKIPPED_ANCESTORS};

pub const MARKUP_PROMPT: &str = "\nFormatting rule: Tokens of the form [[BABEL:number:OPEN]], [[BABEL:number:CLOSE]], and [[BABEL:number:KEEP]] protect original formatting, links, and line breaks. Copy every token exactly once, unchanged and in its original order. Translate only the surrounding text. Do not add HTML or explanations. These tokens must also survive refinement.";

pub fn marker_regex() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"\[\[BABEL:(\d+):(OPEN|CLOSE|KEEP)\]\]").expect("marker regex"))
}

pub fn validate_markers(source: &str, translated: &str) -> Result<(), BabelEbookError> {
    let expected: Vec<_> = marker_regex()
        .find_iter(source)
        .map(|m| m.as_str())
        .collect();
    let actual: Vec<_> = marker_regex()
        .find_iter(translated)
        .map(|m| m.as_str())
        .collect();
    if expected != actual || translated.matches("[[BABEL:").count() != actual.len() {
        return Err(BabelEbookError::ApiError(
            "The model changed protected EPUB formatting markers. Retry this chapter; no malformed translation was cached.".into(),
        ));
    }
    Ok(())
}

pub struct ProtectedContent {
    pub text: String,
    templates: Vec<NodeRef>,
}

impl ProtectedContent {
    pub fn from_node(
        node: &NodeRef,
        options: &TranslationOptions,
    ) -> Result<Self, BabelEbookError> {
        if node.text_contents().contains("[[BABEL:") {
            return Err(BabelEbookError::Configuration(
                "Source contains reserved EPUB formatting markers".into(),
            ));
        }
        let skip = build_skip_set(node, &options.exclude_selectors);
        let mut content = Self {
            text: String::new(),
            templates: Vec::new(),
        };
        for child in node.children() {
            content.encode(&child, &skip);
        }
        Ok(content)
    }

    fn encode(&mut self, node: &NodeRef, skip: &std::collections::HashSet<*const kuchiki::Node>) {
        if let NodeData::Text(text) = node.data() {
            self.text.push_str(&text.borrow());
            return;
        }
        let id = self.templates.len();
        self.templates.push(node.clone());
        let opaque = node.as_element().is_none_or(|e| {
            matches!(e.name.local.as_ref(), "br" | "img" | "hr" | "svg" | "math")
                || SKIPPED_ANCESTORS.contains(&e.name.local.as_ref())
                || skip.contains(&node_ptr(node))
        }) || node.text_contents().trim().is_empty();
        if opaque {
            write!(self.text, "[[BABEL:{id}:KEEP]]").expect("string write");
        } else {
            write!(self.text, "[[BABEL:{id}:OPEN]]").expect("string write");
            for child in node.children() {
                self.encode(&child, skip);
            }
            write!(self.text, "[[BABEL:{id}:CLOSE]]").expect("string write");
        }
    }

    pub const fn has_markup(&self) -> bool {
        !self.templates.is_empty()
    }

    pub fn restore(&self, translated: &str, mode: OutputMode) -> Result<NodeRef, BabelEbookError> {
        validate_markers(&self.text, translated)?;
        let root = NodeRef::new(NodeData::DocumentFragment);
        let mut stack = vec![root.clone()];
        let mut end = 0;
        for captures in marker_regex().captures_iter(translated) {
            let token = captures.get(0).expect("token");
            stack
                .last()
                .expect("parent")
                .append(NodeRef::new_text(&translated[end..token.start()]));
            let id: usize = captures[1].parse().expect("validated marker id");
            match &captures[2] {
                "OPEN" => {
                    let element = clone_subtree(&self.templates[id]);
                    while let Some(child) = element.first_child() {
                        child.detach();
                    }
                    stack.last().expect("parent").append(element.clone());
                    stack.push(element);
                }
                "CLOSE" => {
                    stack.pop();
                }
                _ => stack
                    .last()
                    .expect("parent")
                    .append(clone_subtree(&self.templates[id])),
            }
            end = token.end();
        }
        stack
            .last()
            .expect("parent")
            .append(NodeRef::new_text(&translated[end..]));
        if mode != OutputMode::TranslationOnly {
            for node in root.descendants() {
                if let Some(element) = node.as_element() {
                    element.attributes.borrow_mut().remove("id");
                }
            }
        }
        Ok(root)
    }
}
