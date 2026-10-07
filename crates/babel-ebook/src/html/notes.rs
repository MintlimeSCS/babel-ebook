//! Conservative, offline repair of note references across the whole book.

use std::collections::{HashMap, HashSet};

use kuchiki::NodeRef;
use markup5ever::{namespace_url, ns};
use url::Url;

use crate::core::BabelEbookError;
use crate::epub::EpubBook;

use super::xhtml;

struct Document {
    href: String,
    root: NodeRef,
    chapter: Option<usize>,
    resource: Option<usize>,
    changed: bool,
}

fn attr(node: &NodeRef, key: &str) -> Option<String> {
    node.as_element()?
        .attributes
        .borrow()
        .get(key)
        .map(String::from)
}

fn set(node: &NodeRef, key: &str, value: &str) {
    if let Some(e) = node.as_element() {
        e.attributes.borrow_mut().insert(key, value.into());
    }
}

fn semantics(node: &NodeRef, token: &str) -> bool {
    attr(node, "epub:type").is_some_and(|s| s.split_whitespace().any(|v| v == token))
}

fn add_type(node: &NodeRef, token: &str) {
    if !semantics(node, token) {
        let existing = attr(node, "epub:type").unwrap_or_default();
        set(node, "epub:type", format!("{existing} {token}").trim());
    }
}

fn document_url(href: &str) -> Option<Url> {
    Url::parse("https://babel.invalid/").ok()?.join(href).ok()
}

fn destination(href: &str, link: &str) -> Option<(String, String)> {
    let base = document_url(href)?;
    let url = base.join(link).ok()?;
    if url.origin() != base.origin() || !url.query().unwrap_or_default().is_empty() {
        return None;
    }
    let fragment = url.fragment()?;
    // URL decoding is handled locally; never guess a damaged or truncated ID.
    let id = percent_decode(fragment)?;
    Some((url.path().to_string(), id))
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn local_link(from: &str, to: &str, id: &str) -> Option<String> {
    let base = document_url(from)?;
    let mut target = document_url(to)?;
    target.set_fragment(Some(id));
    base.make_relative(&target)
}

fn note_marker(node: &NodeRef) -> bool {
    let text = node.text_contents();
    let text = text.trim();
    !text.is_empty()
        && text.chars().count() <= 12
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || "*†‡§¶⁎⁑⁂①②③④⑤⑥⑦⑧⑨⑩[](). ".contains(c))
}

fn anchor_ids(node: &NodeRef) -> HashSet<String> {
    std::iter::once(node.clone())
        .chain(node.ancestors())
        .filter_map(|n| attr(&n, "id"))
        .collect()
}

fn translated_partner(node: &NodeRef) -> Option<NodeRef> {
    let tag = node.as_element()?.name.local.clone();
    [node.next_sibling(), node.previous_sibling()]
        .into_iter()
        .flatten()
        .find(|n| {
            n.as_element().is_some_and(|e| e.name.local == tag)
                && attr(n, "lang").is_some_and(|l| l.starts_with("zh"))
                && attr(n, "id").is_none()
        })
}

fn ensure_id(
    node: &NodeRef,
    prefix: &str,
    sequence: &mut usize,
    ids: &HashMap<String, Vec<(usize, NodeRef)>>,
) -> String {
    attr(node, "id").unwrap_or_else(|| {
        let generated = loop {
            *sequence += 1;
            let candidate = format!("{prefix}-{sequence}");
            if !ids.contains_key(&candidate) {
                break candidate;
            }
        };
        set(node, "id", &generated);
        generated
    })
}

fn href_path(href: &str) -> Result<String, BabelEbookError> {
    document_url(href)
        .map(|u| u.path().to_string())
        .ok_or_else(|| {
            BabelEbookError::Configuration(format!(
                "Invalid document href while checking notes: {href}"
            ))
        })
}

/// Add note semantics and return links only for explicit notes or reciprocal
/// short-symbol references. Ambiguous IDs and ordinary navigation stay intact.
#[allow(clippy::too_many_lines)]
pub fn repair_book_notes(book: &mut EpubBook) -> Result<usize, BabelEbookError> {
    let mut docs = Vec::new();
    for (i, c) in book.chapters.iter().enumerate() {
        let text = std::str::from_utf8(&c.content)
            .map_err(|e| BabelEbookError::Configuration(e.to_string()))?;
        docs.push(Document {
            href: c.href.clone(),
            root: xhtml::parse(text),
            chapter: Some(i),
            resource: None,
            changed: false,
        });
    }
    for (i, r) in book.resources.iter().enumerate() {
        if matches!(r.mime.as_str(), "application/xhtml+xml" | "text/html") {
            let text = std::str::from_utf8(&r.data)
                .map_err(|e| BabelEbookError::Configuration(e.to_string()))?;
            docs.push(Document {
                href: r.href.clone(),
                root: xhtml::parse(text),
                chapter: None,
                resource: Some(i),
                changed: false,
            });
        }
    }
    let mut ids: HashMap<(String, String), Vec<(usize, NodeRef)>> = HashMap::new();
    let mut global_ids: HashMap<String, Vec<(usize, NodeRef)>> = HashMap::new();
    let mut links = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        let path = href_path(&d.href)?;
        for n in d.root.descendants() {
            if let Some(id) = attr(&n, "id") {
                ids.entry((path.clone(), id.clone()))
                    .or_default()
                    .push((i, n.clone()));
                global_ids.entry(id).or_default().push((i, n.clone()));
            }
            if n.as_element().is_some_and(|e| e.name.local.as_ref() == "a") {
                if let Some(href) = attr(&n, "href") {
                    links.push((i, n, href));
                }
            }
        }
    }
    let mut repaired = 0;
    let mut sequence = 0;
    let mut moved: HashMap<(usize, String), NodeRef> = HashMap::new();
    // Establish legacy note relationships from references that retain source IDs
    // before marking their translated copies, whose inline IDs are removed.
    links.sort_by_key(|(_, reference, _)| anchor_ids(reference).is_empty());
    for (source_doc, reference, href) in links {
        if !semantics(&reference, "noteref")
            && attr(&reference, "role").as_deref() != Some("doc-noteref")
            && !(note_marker(&reference)
                && reference.ancestors().any(|n| {
                    n.as_element()
                        .is_some_and(|e| e.name.local.as_ref() == "sup")
                }))
        {
            continue;
        }
        let Some((path, id)) = destination(&docs[source_doc].href, &href) else {
            continue;
        };
        let matches = ids
            .get(&(path.clone(), id.clone()))
            .or_else(|| global_ids.get(&id));
        let Some(matches) = matches else { continue };
        if matches.len() != 1 {
            continue;
        }
        let (target_doc, target) = &matches[0];
        let target_doc = *target_doc;
        let mut note = target.clone();
        if !note.as_element().is_some_and(|e| {
            matches!(
                e.name.local.as_ref(),
                "p" | "aside" | "li" | "div" | "section"
            )
        }) {
            if let Some(parent) = note.ancestors().find(|n| {
                n.as_element()
                    .is_some_and(|e| matches!(e.name.local.as_ref(), "p" | "aside" | "li"))
            }) {
                note = parent;
            } else {
                continue;
            }
        }
        let source_path = href_path(&docs[source_doc].href)?;
        let source_ids = anchor_ids(&reference);
        let reciprocal = note.descendants().any(|n| {
            attr(&n, "href")
                .and_then(|h| destination(&docs[target_doc].href, &h))
                .is_some_and(|(p, i)| p == source_path && source_ids.contains(&i))
        });
        let explicit = moved.contains_key(&(target_doc, id.clone()))
            || semantics(&reference, "noteref")
            || attr(&reference, "role").as_deref() == Some("doc-noteref")
            || semantics(&note, "footnote")
            || semantics(&note, "endnote")
            || attr(&note, "role").as_deref() == Some("doc-footnote");
        if !explicit && !reciprocal {
            continue;
        }
        // In bilingual output the inherited note ID initially belongs to the
        // English copy. Move it to the adjacent Chinese note before linking.
        if let Some(existing) = moved.get(&(target_doc, id.clone())) {
            note = existing.clone();
        } else if note == *target {
            if let Some(partner) = translated_partner(&note) {
                if let Some(e) = note.as_element() {
                    e.attributes.borrow_mut().remove("id");
                }
                set(&partner, "id", &id);
                note = partner;
                moved.insert((target_doc, id.clone()), note.clone());
            }
        }
        // Make the actual note container the target, keeping an inner legacy
        // anchor intact when it already has a different container ID.
        let note_id = ensure_id(&note, "babel-note", &mut sequence, &global_ids);
        let ref_id = ensure_id(&reference, "babel-noteref", &mut sequence, &global_ids);
        if let Some(link) = local_link(&docs[source_doc].href, &docs[target_doc].href, &note_id) {
            set(&reference, "href", &link);
        }
        add_type(&reference, "noteref");
        set(&reference, "role", "doc-noteref");
        if !semantics(&note, "endnote") {
            add_type(&note, "footnote");
        }
        set(&note, "role", "doc-footnote");
        // A separate return link per reference supports repeated citations.
        let back = local_link(&docs[target_doc].href, &docs[source_doc].href, &ref_id).ok_or_else(
            || BabelEbookError::Configuration("Cannot resolve note return link".into()),
        )?;
        let exists = note
            .descendants()
            .any(|n| attr(&n, "href").as_deref() == Some(back.as_str()));
        if !exists {
            let a = NodeRef::new_element(
                markup5ever::QualName::new(None, ns!(html), "a".into()),
                None,
            );
            set(&a, "href", &back);
            set(&a, "epub:type", "backlink");
            set(&a, "role", "doc-backlink");
            a.append(NodeRef::new_text("↩"));
            note.append(NodeRef::new_text(" "));
            note.append(a);
        }
        docs[source_doc].changed = true;
        docs[target_doc].changed = true;
        repaired += 1;
    }
    for d in docs.into_iter().filter(|d| d.changed) {
        let bytes = xhtml::serialize(&d.root);
        if let Some(i) = d.chapter {
            book.chapters[i].content = bytes;
        } else if let Some(i) = d.resource {
            book.resources[i].data = bytes;
        }
    }
    Ok(repaired)
}
