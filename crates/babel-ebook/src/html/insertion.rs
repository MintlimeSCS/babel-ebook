//! Translation insertion helpers for rewriting the DOM after translation.

use kuchiki::{Attribute, ExpandedName, NodeRef};
use markup5ever::{namespace_url, ns, QualName};

use crate::config::{OutputMode, TranslationOptions};

pub fn insert_translation(
    node: &NodeRef,
    name: &QualName,
    translated: &NodeRef,
    options: &TranslationOptions,
) {
    let source = if options.source_lang == "auto" {
        "en"
    } else {
        &options.source_lang
    };
    match name.local.as_ref() {
        "td" | "th" | "caption" | "figcaption" => insert_contained_translation(
            node,
            translated,
            &options.target_lang,
            source,
            options.output_mode,
        ),
        "li" => insert_li_translation(
            node,
            translated,
            &options.target_lang,
            source,
            options.output_mode,
            options.preserve_classes,
        ),
        _ => insert_generic_translation(
            node,
            name,
            translated,
            &options.target_lang,
            source,
            options.output_mode,
            options.preserve_classes,
        ),
    }
}

/// Set the `lang` attribute on `node` to `value`.
pub fn set_lang(node: &NodeRef, value: &str) {
    if let Some(data) = node.as_element() {
        data.attributes.borrow_mut().insert("lang", value.into());
    }
}

/// Insert a translation for a generic (non-`<li>`) element according to the
/// configured output mode.
pub fn insert_generic_translation(
    node: &NodeRef,
    name: &QualName,
    translated: &NodeRef,
    target_lang: &str,
    source_lang: &str,
    mode: OutputMode,
    preserve_classes: bool,
) {
    let translated_element =
        NodeRef::new_element(QualName::new(None, ns!(html), name.local.clone()), None);
    append_content(&translated_element, translated);
    copy_element_attributes(node, &translated_element);
    if !preserve_classes {
        translated_element
            .as_element()
            .expect("element")
            .attributes
            .borrow_mut()
            .remove("class");
    }
    if mode != OutputMode::TranslationOnly {
        translated_element
            .as_element()
            .expect("element")
            .attributes
            .borrow_mut()
            .remove("id");
    }
    set_lang(&translated_element, target_lang);

    match mode {
        OutputMode::Bilingual => {
            // Translated element first, original element second.
            node.insert_before(translated_element);
            set_lang(node, source_lang);
        }
        OutputMode::TranslationOnly => {
            // Replace the original element with the translated element.
            node.insert_before(translated_element);
            node.detach();
        }
        OutputMode::Interleaved => {
            // Replace the original element with [original clone, translated].
            let original_clone = clone_subtree(node);
            if preserve_classes {
                copy_element_attributes(node, &original_clone);
            }
            set_lang(&original_clone, source_lang);
            // Insert original_clone first, then translated, then remove the original.
            node.insert_before(original_clone);
            node.insert_before(translated_element);
            node.detach();
        }
    }
}

/// Insert a translation for a `<li>` element while preserving the list item
/// parent structure.
#[allow(unused_variables)]
pub fn insert_li_translation(
    node: &NodeRef,
    translated: &NodeRef,
    target_lang: &str,
    source_lang: &str,
    mode: OutputMode,
    preserve_classes: bool,
) {
    match mode {
        OutputMode::Bilingual => {
            let translated_p =
                NodeRef::new_element(QualName::new(None, ns!(html), "p".into()), None);
            append_content(&translated_p, translated);
            set_lang(&translated_p, target_lang);
            node.prepend(translated_p);

            if let Some(data) = node.as_element() {
                let mut attrs = data.attributes.borrow_mut();
                let existing = attrs.get("class").map(String::from).unwrap_or_default();
                let new_class = if existing.is_empty() {
                    "bilingual-li".to_string()
                } else {
                    format!("{existing} bilingual-li")
                };
                attrs.insert("class", new_class);
            }
            set_lang(node, source_lang);
        }
        OutputMode::TranslationOnly => {
            // Replace the original content with the translated text.
            while let Some(child) = node.first_child() {
                child.detach();
            }
            append_content(node, translated);
            set_lang(node, target_lang);
        }
        OutputMode::Interleaved => {
            // Original content first, then the translated paragraph.
            let translated_p =
                NodeRef::new_element(QualName::new(None, ns!(html), "p".into()), None);
            append_content(&translated_p, translated);
            set_lang(&translated_p, target_lang);
            node.append(translated_p);
            set_lang(node, source_lang);
        }
    }
}

/// Copy element attributes from `source` to `target`.
///
/// Used when `preserve_classes` is enabled so that translated clones
/// keep the original styling hooks.
fn copy_element_attributes(source: &NodeRef, target: &NodeRef) {
    if let (Some(src), Some(tgt)) = (source.as_element(), target.as_element()) {
        let src_attrs = src.attributes.borrow();
        let mut tgt_attrs = tgt.attributes.borrow_mut();
        for (name, attr) in &src_attrs.map {
            tgt_attrs.map.insert(name.clone(), attr.clone());
        }
    }
}

/// Create a deep clone of `node` and its descendants.
pub(super) fn clone_subtree(node: &NodeRef) -> NodeRef {
    match node.data() {
        kuchiki::NodeData::Element(data) => {
            let attributes: Vec<(ExpandedName, Attribute)> = data
                .attributes
                .borrow()
                .map
                .iter()
                .map(|(name, attr)| {
                    (
                        name.clone(),
                        Attribute {
                            prefix: attr.prefix.clone(),
                            value: attr.value.clone(),
                        },
                    )
                })
                .collect();
            let cloned = NodeRef::new_element(data.name.clone(), attributes);
            for child in node.children() {
                cloned.append(clone_subtree(&child));
            }
            cloned
        }
        kuchiki::NodeData::Text(text) => NodeRef::new_text(text.borrow().clone()),
        kuchiki::NodeData::Comment(text) => NodeRef::new_comment(text.borrow().clone()),
        kuchiki::NodeData::ProcessingInstruction(pi) => {
            let (target, data) = pi.borrow().clone();
            NodeRef::new_processing_instruction(target, data)
        }
        kuchiki::NodeData::Doctype(doctype) => NodeRef::new_doctype(
            doctype.name.clone(),
            doctype.public_id.clone(),
            doctype.system_id.clone(),
        ),
        kuchiki::NodeData::Document(_) => NodeRef::new_document(),
        kuchiki::NodeData::DocumentFragment => {
            let cloned = NodeRef::new(kuchiki::NodeData::DocumentFragment);
            for child in node.children() {
                cloned.append(clone_subtree(&child));
            }
            cloned
        }
    }
}

fn append_content(target: &NodeRef, content: &NodeRef) {
    while let Some(child) = content.first_child() {
        target.append(child);
    }
}

/// Keep bilingual text inside the original cell/caption rather than duplicating it.
pub fn insert_contained_translation(
    node: &NodeRef,
    translated: &NodeRef,
    target_lang: &str,
    source_lang: &str,
    mode: OutputMode,
) {
    if mode == OutputMode::TranslationOnly {
        while let Some(child) = node.first_child() {
            child.detach();
        }
        append_content(node, translated);
        set_lang(node, target_lang);
        return;
    }
    let wrapper = if node
        .as_element()
        .is_some_and(|e| e.name.local.as_ref() == "caption")
    {
        "span"
    } else {
        "div"
    };
    let original = NodeRef::new_element(QualName::new(None, ns!(html), wrapper.into()), None);
    while let Some(child) = node.first_child() {
        original.append(child);
    }
    set_lang(&original, source_lang);
    let translation = NodeRef::new_element(QualName::new(None, ns!(html), wrapper.into()), None);
    append_content(&translation, translated);
    set_lang(&translation, target_lang);
    if mode == OutputMode::Bilingual {
        node.append(translation);
        if wrapper == "span" {
            node.append(NodeRef::new_element(
                QualName::new(None, ns!(html), "br".into()),
                None,
            ));
        }
        node.append(original);
    } else {
        node.append(original);
        if wrapper == "span" {
            node.append(NodeRef::new_element(
                QualName::new(None, ns!(html), "br".into()),
                None,
            ));
        }
        node.append(translation);
    }
}
