//! XML serialization for EPUB content (HTML serialization leaves void tags open).

use std::fmt::Write;

use kuchiki::traits::TendrilSink;
use kuchiki::{NodeData, NodeRef};
use regex::Regex;
use std::sync::OnceLock;

/// Expand empty XHTML inline elements before the HTML5 parser sees them.
/// HTML5 ignores the self-closing flag on anchors and can reconstruct their IDs
/// across subsequent paragraphs. XML and EPUB permit these empty anchors.
pub(super) fn parse(html: &str) -> NodeRef {
    static EMPTY: OnceLock<Regex> = OnceLock::new();
    static PROTECTED: OnceLock<Regex> = OnceLock::new();
    let pattern = EMPTY.get_or_init(|| Regex::new(r#"(?is)<(a|span|small|em|strong|i|b|p|div|sup|sub|li|section|aside|h[1-6]|td|th|caption)\b((?:[^"'<>]|"[^"]*"|'[^']*')*?)/\s*>"#).expect("empty XHTML pattern"));
    let protected = PROTECTED.get_or_init(|| {
        Regex::new(r"(?is)<!--.*?-->|<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>")
            .expect("protected HTML pattern")
    });
    let mut expanded = String::new();
    let mut end = 0;
    for block in protected.find_iter(html) {
        expanded.push_str(&pattern.replace_all(&html[end..block.start()], "<$1$2></$1>"));
        expanded.push_str(block.as_str());
        end = block.end();
    }
    expanded.push_str(&pattern.replace_all(&html[end..], "<$1$2></$1>"));
    kuchiki::parse_html().one(expanded)
}

pub fn serialize(doc: &NodeRef) -> Vec<u8> {
    let mut output = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    write_node(doc, &mut output, "");
    output.into_bytes()
}

fn escape(value: &str, attribute: bool) -> String {
    let value = value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if attribute {
        value
            .replace('"', "&quot;")
            .replace('\n', "&#10;")
            .replace('\r', "&#13;")
            .replace('\t', "&#9;")
    } else {
        value
    }
}

fn write_node(node: &NodeRef, output: &mut String, parent_namespace: &str) {
    match node.data() {
        NodeData::Element(element) => {
            let name = element.name.local.as_ref();
            let namespace = element.name.ns.as_ref();
            let attrs = element.attributes.borrow();
            let mut attribute_names = std::collections::HashSet::new();
            for (key, attr) in &attrs.map {
                attribute_names.insert(attr.prefix.as_ref().map_or_else(
                    || key.local.to_string(),
                    |prefix| format!("{prefix}:{}", key.local),
                ));
            }
            write!(output, "<{name}").expect("string write");
            if namespace != parent_namespace {
                write!(output, " xmlns=\"{}\"", escape(namespace, true)).expect("string write");
            }
            if name == "html" && !attribute_names.contains("xmlns:epub") {
                output.push_str(" xmlns:epub=\"http://www.idpf.org/2007/ops\"");
            }
            for (key, attr) in &attrs.map {
                if key.local.as_ref() == "xmlns" {
                    continue;
                }
                if let Some(prefix) = &attr.prefix {
                    if prefix.as_ref() != "xml"
                        && prefix.as_ref() != "xmlns"
                        && attribute_names.insert(format!("xmlns:{prefix}"))
                    {
                        write!(
                            output,
                            " xmlns:{prefix}=\"{}\"",
                            escape(key.ns.as_ref(), true)
                        )
                        .expect("string write");
                    }
                    write!(
                        output,
                        " {prefix}:{}=\"{}\"",
                        key.local,
                        escape(&attr.value, true)
                    )
                    .expect("string write");
                } else {
                    write!(output, " {}=\"{}\"", key.local, escape(&attr.value, true))
                        .expect("string write");
                }
            }
            if node.first_child().is_none()
                && (namespace != "http://www.w3.org/1999/xhtml"
                    || matches!(
                        name,
                        "area"
                            | "base"
                            | "br"
                            | "col"
                            | "embed"
                            | "hr"
                            | "img"
                            | "input"
                            | "link"
                            | "meta"
                            | "param"
                            | "source"
                            | "track"
                            | "wbr"
                    ))
            {
                output.push_str(" />");
            } else {
                output.push('>');
                for child in node.children() {
                    write_node(&child, output, namespace);
                }
                write!(output, "</{name}>").expect("string write");
            }
        }
        NodeData::Text(text) => output.push_str(&escape(&text.borrow(), false)),
        NodeData::Comment(comment) => {
            write!(output, "<!--{}-->", comment.borrow()).expect("string write");
        }
        NodeData::Doctype(_) => output.push_str("<!DOCTYPE html>"),
        NodeData::ProcessingInstruction(pi) => {
            let pi = pi.borrow();
            if pi.0 != "xml" {
                write!(output, "<?{} {}?>", pi.0, pi.1).expect("string write");
            }
        }
        NodeData::Document(_) | NodeData::DocumentFragment => {
            for child in node.children() {
                write_node(&child, output, parent_namespace);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_xhtml_expansion_preserves_script_strings_and_comments() {
        let doc = super::parse(
            "<p>Hello<a id='page'/> world</p><script>const tag = '<a/>';</script><!--<a/>-->",
        );
        assert_eq!(
            doc.select_first("script").unwrap().text_contents(),
            "const tag = '<a/>';"
        );
        assert_eq!(doc.select_first("#page").unwrap().text_contents(), "");
    }
}
