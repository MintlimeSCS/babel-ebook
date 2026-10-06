//! Offline regression: a mock model drops all formatting markers.
//! Usage: cargo run -p babel-ebook --example verify_epub_structure -- book.epub
//! This checks structure only; it does not produce a readable translation.
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use async_trait::async_trait;
use babel_ebook::config::OutputMode;
use babel_ebook::epub::read_epub;
use babel_ebook::html::process_document;
use babel_ebook::{BabelEbookError, Config, TranslateContext, TranslationCache, Translator};
use kuchiki::traits::TendrilSink;

struct DroppedMarkers(AtomicUsize);
#[async_trait]
impl Translator for DroppedMarkers {
    fn name(&self) -> String {
        "offline:drop-markers".into()
    }
    fn max_output_tokens(&self) -> usize {
        2000
    }
    async fn translate(
        &self,
        text: &str,
        _: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        let re =
            RE.get_or_init(|| regex::Regex::new(r"\[\[BABEL:\d+:(OPEN|CLOSE|KEEP)\]\]").unwrap());
        Ok(re.replace_all(text, "").into_owned())
    }
}

fn attrs(html: &str, selector: &str, attribute: &str) -> BTreeSet<String> {
    let doc = kuchiki::parse_html().one(html);
    doc.select(selector)
        .unwrap()
        .filter_map(|element| element.attributes.borrow().get(attribute).map(String::from))
        .collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("provide source EPUB path");
    let book = read_epub(Path::new(&path))?;
    let temporary = tempfile::tempdir()?;
    let cache = TranslationCache::new(temporary.path().into());
    let model = DroppedMarkers(AtomicUsize::new(0));
    let mut records = Vec::new();
    for refine in [false, true] {
        let config = Config {
            target_lang: "zh-TW".into(),
            output_mode: OutputMode::TranslationOnly,
            refine,
            ..Config::default()
        };
        for (index, chapter) in book.chapters.iter().enumerate() {
            let source = std::str::from_utf8(&chapter.content)?;
            let bytes = process_document(
                &chapter.content,
                &model,
                &config.translation_options(),
                &cache,
                index,
                &chapter.href,
                None,
                None,
            )
            .await?;
            let result = String::from_utf8(bytes)?;
            assert!(!result.contains("[[BABEL:"), "{}", chapter.href);
            let mut tags = std::collections::BTreeMap::<String, usize>::new();
            let mut reader = quick_xml::Reader::from_str(&result);
            loop {
                match reader.read_event()? {
                    quick_xml::events::Event::Eof => break,
                    quick_xml::events::Event::Text(text) => {
                        text.unescape()?;
                    }
                    quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) => {
                        *tags
                            .entry(String::from_utf8_lossy(e.local_name().as_ref()).into_owned())
                            .or_default() += 1;
                        for attr in e.attributes() {
                            attr?.decode_and_unescape_value(reader.decoder())?;
                        }
                    }
                    _ => (),
                }
            }
            for (selector, attribute) in [("[id]", "id"), ("a[href]", "href"), ("img[src]", "src")]
            {
                assert_eq!(
                    attrs(source, selector, attribute),
                    attrs(&result, selector, attribute),
                    "{}: {attribute}",
                    chapter.href
                );
            }
            let original = kuchiki::parse_html().one(source);
            for selector in [
                "em", "i", "strong", "small", "br", "img", "code", "pre", "table", "tr", "td", "th",
            ] {
                assert_eq!(
                    original.select(selector).unwrap().count(),
                    tags.get(selector).copied().unwrap_or_default(),
                    "{}: {selector}",
                    chapter.href
                );
            }
            records.push(serde_json::json!({"href": chapter.href, "refine": refine, "xml_and_structure": "passed"}));
        }
    }
    println!(
        "{}",
        serde_json::json!({"chapters": book.chapters.len(), "checks": records.len(), "mock_requests": model.0.load(Ordering::Relaxed), "results": records})
    );
    Ok(())
}
