use babel_ebook::epub::{Chapter, EpubBook, EpubMetadata};
use babel_ebook::html::repair_book_notes;
use kuchiki::traits::TendrilSink;

fn book(parts: &[(&str, &str)]) -> EpubBook {
    EpubBook {
        metadata: EpubMetadata::default(),
        chapters: parts
            .iter()
            .map(|(href, html)| Chapter {
                href: href.to_string(),
                title: None,
                content: html.as_bytes().to_vec(),
            })
            .collect(),
        resources: vec![],
    }
}

#[test]
fn repairs_calibre_cross_file_notes_and_preserves_navigation() {
    let mut b = book(&[
        ("/text/main.xhtml", "<p>Text<sup><small id='old-ref'><a href='../notes/n.xhtml#note'>*</a></small></sup> <a href='../notes/n.xhtml#note'>Contents</a></p>"),
        ("/notes/n.xhtml", "<p id='note'><a href='../text/main.xhtml#old-ref'>*</a> Note text</p>")
    ]);
    assert_eq!(repair_book_notes(&mut b).unwrap(), 1);
    let main = kuchiki::parse_html().one(String::from_utf8(b.chapters[0].content.clone()).unwrap());
    let note = kuchiki::parse_html().one(String::from_utf8(b.chapters[1].content.clone()).unwrap());
    assert_eq!(main.select("a[role='doc-noteref']").unwrap().count(), 1);
    assert_eq!(
        main.select("a")
            .unwrap()
            .last()
            .unwrap()
            .attributes
            .borrow()
            .get("role"),
        None
    );
    assert_eq!(note.select("p[role='doc-footnote']").unwrap().count(), 1);
    assert_eq!(note.select("a[role='doc-backlink']").unwrap().count(), 1);
    assert_eq!(note.select("a[role='doc-noteref']").unwrap().count(), 0);
    assert_eq!(repair_book_notes(&mut b).unwrap(), 1);
    let second =
        kuchiki::parse_html().one(String::from_utf8(b.chapters[1].content.clone()).unwrap());
    assert_eq!(second.select("a[role='doc-backlink']").unwrap().count(), 1);
}

#[test]
fn bilingual_note_target_is_the_translated_copy() {
    let mut b = book(&[
        (
            "/main.xhtml",
            "<p><sup><a epub:type='noteref' href='note.xhtml#n'>1</a></sup></p>",
        ),
        (
            "/note.xhtml",
            "<p id='n' lang='en'>English note</p><p lang='zh-TW'>中文註解</p>",
        ),
    ]);
    repair_book_notes(&mut b).unwrap();
    let note = kuchiki::parse_html().one(String::from_utf8(b.chapters[1].content.clone()).unwrap());
    assert!(note
        .select_first("#n")
        .unwrap()
        .text_contents()
        .starts_with("中文註解"));
    assert_eq!(note.select("#n").unwrap().count(), 1);
}

#[test]
fn repairs_only_unique_explicit_note_destinations() {
    let mut b = book(&[
        ("/main.xhtml", "<p><a epub:type='noteref' href='missing.xhtml#n'>1</a> <a href='missing.xhtml#n'>Menu</a></p>"),
        ("/note.xhtml", "<p id='n'>Note</p>")
    ]);
    assert_eq!(repair_book_notes(&mut b).unwrap(), 1);
    let html = String::from_utf8(b.chapters[0].content.clone()).unwrap();
    assert!(html.contains("href=\"note.xhtml#n\""));
    assert!(html.contains("href=\"missing.xhtml#n\""));
    let mut ambiguous = book(&[
        (
            "/main.xhtml",
            "<a epub:type='noteref' href='missing.xhtml#n'>1</a>",
        ),
        ("/n1.xhtml", "<p id='n'>First</p>"),
        ("/n2.xhtml", "<p id='n'>Second</p>"),
    ]);
    assert_eq!(repair_book_notes(&mut ambiguous).unwrap(), 0);
}

#[test]
fn both_bilingual_reference_copies_open_the_chinese_note() {
    for translated_first in [false, true] {
        let english =
            "<p lang='en'>Text<sup><small id='r'><a href='note.xhtml#n'>*</a></small></sup></p>";
        let chinese =
            "<p lang='zh-TW'>譯文<sup><small><a href='note.xhtml#n'>*</a></small></sup></p>";
        let body = if translated_first {
            format!("{chinese}{english}")
        } else {
            format!("{english}{chinese}")
        };
        let mut b = book(&[("/main.xhtml", &body), ("/note.xhtml", "<p id='n' lang='en'><a href='main.xhtml#r'>*</a> English note</p><p lang='zh-TW'><a href='main.xhtml#r'>*</a> 中文註解</p>")]);
        assert_eq!(repair_book_notes(&mut b).unwrap(), 2);
        let doc =
            kuchiki::parse_html().one(String::from_utf8(b.chapters[0].content.clone()).unwrap());
        assert_eq!(doc.select("a[role='doc-noteref']").unwrap().count(), 2);
    }
}
