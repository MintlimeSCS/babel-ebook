//! XML serialization for EPUB content (HTML serialization leaves void tags open).

use std::fmt::Write;

use kuchiki::{NodeData, NodeRef};

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
            if node.first_child().is_none() {
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
