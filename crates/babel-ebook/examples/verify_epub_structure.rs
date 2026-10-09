//! Offline regression: a mock model drops all formatting markers.
//! Usage: cargo run -p babel-ebook --example verify_epub_structure -- book.epub [--strict]
//! This checks structure only; it does not produce a readable translation.
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use async_trait::async_trait;
use babel_ebook::config::OutputMode;
use babel_ebook::epub::read_epub;
use babel_ebook::html::process_document;
use babel_ebook::{BabelEbookError, Config, TranslateContext, TranslationCache, Translator};
use kuchiki::traits::TendrilSink;

struct DroppedMarkers {
    calls: AtomicUsize,
    strict_contract: Option<Box<dyn Translator>>,
    fail_next_group: AtomicBool,
}
#[async_trait]
impl Translator for DroppedMarkers {
    fn name(&self) -> String {
        "offline:drop-markers".into()
    }
    fn max_output_tokens(&self) -> usize {
        2000
    }
    fn fragment_response_format(&self, count: usize) -> Option<serde_json::Value> {
        self.strict_contract
            .as_ref()
            .and_then(|t| t.fragment_response_format(count))
    }
    async fn translate_fragments(
        &self,
        text: &str,
        context: &TranslateContext<'_>,
        count: usize,
    ) -> Result<String, BabelEbookError> {
        if self.fail_next_group.swap(false, Ordering::Relaxed) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            return Ok("invalid JSON response for offline recovery testing".into());
        }
        if self.strict_contract.is_none() {
            return self.translate(text, context).await;
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        let runs: Vec<String> = serde_json::from_str(text).unwrap();
        assert_eq!(runs.len(), count);
        Ok(serde_json::json!({"translations":runs}).to_string())
    }
    async fn translate(
        &self,
        text: &str,
        _: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
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
    let strict = std::env::args().any(|arg| arg == "--strict");
    let recovery = std::env::args().any(|arg| arg == "--recover");
    let args: Vec<_> = std::env::args().collect();
    let selected_model = args
        .windows(2)
        .find(|pair| pair[0] == "--model")
        .map_or("gpt-5.4-mini-2026-03-17", |pair| pair[1].as_str());
    let contract = if strict {
        let config = Config {
            api_key: Some("fake-offline-key".into()),
            model: selected_model.into(),
            ..Config::default()
        };
        // Consult the real provider's schema only. Translation remains mocked.
        let mut provider = babel_ebook::config::ProviderConfig::for_provider("openai");
        provider.default_model = config.model.clone();
        let translator = babel_ebook::get_translator("openai", Some(&provider), &config, false)?;
        assert!(translator.fragment_response_format(2).is_some());
        Some(translator)
    } else {
        None
    };
    let model = DroppedMarkers {
        calls: AtomicUsize::new(0),
        strict_contract: contract,
        fail_next_group: AtomicBool::new(false),
    };
    let mut records = Vec::new();
    for refine in [false, true] {
        let config = Config {
            target_lang: "zh-TW".into(),
            output_mode: OutputMode::TranslationOnly,
            refine,
            ..Config::default()
        };
        for (index, chapter) in book.chapters.iter().enumerate() {
            model.fail_next_group.store(recovery, Ordering::Relaxed);
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
            let translated = kuchiki::parse_html().one(result.clone());
            assert_eq!(
                original
                    .select_first("body")
                    .unwrap()
                    .text_contents()
                    .split_whitespace()
                    .collect::<Vec<_>>(),
                translated
                    .select_first("body")
                    .unwrap()
                    .text_contents()
                    .split_whitespace()
                    .collect::<Vec<_>>(),
                "{}: text order/completeness",
                chapter.href
            );
            for selector in [
                "p", "span", "em", "i", "strong", "small", "br", "img", "code", "pre", "table",
                "tr", "td", "th",
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
        serde_json::json!({"chapters": book.chapters.len(), "checks": records.len(), "strict": strict, "model": selected_model, "recovery_injected": recovery, "mock_requests": model.calls.load(Ordering::Relaxed), "results": records})
    );
    Ok(())
}
