//! Regressions for settings-aware cache, glossary propagation and EPUB structure.

use std::sync::Mutex;

use async_trait::async_trait;
use babel_ebook::config::{GlossaryEntry, OutputMode};
use babel_ebook::html::{process_document, translate_text};
use babel_ebook::{BabelEbookError, Config, TranslateContext, TranslationCache, Translator};
use kuchiki::traits::TendrilSink;
use quick_xml::events::Event;

#[derive(Default)]
struct RecordingTranslator {
    requests: Mutex<Vec<(String, String, String)>>,
}

#[async_trait]
impl Translator for RecordingTranslator {
    fn name(&self) -> String {
        "recording:model".into()
    }
    fn max_output_tokens(&self) -> usize {
        2000
    }
    async fn translate(
        &self,
        text: &str,
        context: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        self.requests.lock().unwrap().push((
            text.into(),
            context.system_prompt.into(),
            context.target_lang.into(),
        ));
        Ok(text
            .replace("Hello", "你好")
            .replace("world", "世界")
            .replace("Dispatch log", "派遣紀錄")
            .replace("Station", "站點"))
    }
}

async fn translate(
    config: &Config,
    translator: &RecordingTranslator,
    cache: &TranslationCache,
    href: &str,
) -> String {
    translate_text(
        "Hello world",
        translator,
        &config.translation_options(),
        cache,
        0,
        href,
        None,
        None,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn cache_separates_languages_prompts_glossaries_and_request_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    cache.put("recording:model", "Hello world", "LEGACY", None);
    let translator = RecordingTranslator::default();
    let mut config = Config::default();
    assert_eq!(
        translate(&config, &translator, &cache, "ch1").await,
        "你好 世界"
    );
    translate(&config, &translator, &cache, "ch1").await;
    assert_eq!(translator.requests.lock().unwrap().len(), 1);
    config.target_lang = "zh-TW".into();
    translate(&config, &translator, &cache, "ch1").await;
    config.source_lang = "auto".into();
    translate(&config, &translator, &cache, "ch1").await;
    config.system_prompt = Some("Custom {source_lang} to {target_lang}".into());
    translate(&config, &translator, &cache, "ch1").await;
    config.glossary.push(GlossaryEntry {
        term: "Mercer".into(),
        translation: "默瑟".into(),
        context: Some("surname".into()),
    });
    translate(&config, &translator, &cache, "ch1").await;
    config.glossary[0].context = Some("another sense".into());
    translate(&config, &translator, &cache, "ch1").await;
    config
        .chapter_prompts
        .insert("ch1".into(), "Chapter override".into());
    translate(&config, &translator, &cache, "ch1").await;
    config.temperature = 0.7;
    translate(&config, &translator, &cache, "ch1").await;
    config.max_output_tokens = 3000;
    translate(&config, &translator, &cache, "ch1").await;
    let requests = translator.requests.lock().unwrap();
    assert_eq!(requests.len(), 9);
    assert!(requests[4].1.contains("Mercer => 默瑟 (context: surname)"));
    assert!(requests[6].1.contains("Chapter override"));
    assert!(requests[6].1.contains("Mercer => 默瑟"));
}

#[tokio::test]
async fn refine_cache_changes_with_refine_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let translator = RecordingTranslator::default();
    let mut config = Config {
        refine: true,
        ..Config::default()
    };
    translate(&config, &translator, &cache, "ch1").await;
    translate(&config, &translator, &cache, "ch1").await;
    assert_eq!(translator.requests.lock().unwrap().len(), 2);
    config.prompts.refine = "Different polish instruction".into();
    translate(&config, &translator, &cache, "ch1").await;
    assert_eq!(translator.requests.lock().unwrap().len(), 3);
}

#[test]
fn every_prompt_includes_glossary_context_and_language_placeholders() {
    let mut config = Config {
        target_lang: "zh-TW".into(),
        system_prompt: Some("{source_lang} -> {target_lang}".into()),
        ..Config::default()
    };
    config.glossary.push(GlossaryEntry {
        term: "Vale".into(),
        translation: "維爾".into(),
        context: Some("person, not a valley".into()),
    });
    config
        .chapter_prompts
        .insert("ch1".into(), "Chapter {target_lang}".into());
    for prompt in [
        config.system_prompt(),
        config.system_prompt_for_chapter("ch1"),
        config.refine_prompt(),
    ] {
        assert!(prompt.contains("zh-TW"));
        assert!(prompt.contains("Vale => 維爾 (context: person, not a valley)"));
        assert!(!prompt.contains("{target_lang}"));
        assert_eq!(prompt.matches("Vale =>").count(), 1);
    }
}

fn assert_xml(output: &str) {
    let mut reader = quick_xml::Reader::from_str(output);
    loop {
        match reader.read_event().expect("valid XHTML") {
            Event::Start(event) | Event::Empty(event) => {
                for attr in event.attributes() {
                    attr.expect("valid unique attributes")
                        .decode_and_unescape_value(reader.decoder())
                        .unwrap();
                }
            }
            Event::Text(text) => {
                text.unescape().unwrap();
            }
            Event::Eof => break,
            _ => (),
        }
    }
}

const XHTML: &str = r##"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Test</title><link rel="stylesheet" href="book.css" /></head><body><p id="ref">Hello <em>world</em><br />Hello <a href="#note" epub:type="noteref">[1]</a></p><p id="note"><a href="#ref">Hello world</a></p><table><caption>Dispatch log</caption><tr><th scope="col">Station</th><th>Station</th></tr><tr><td colspan="1">Hello world</td><td>27</td></tr></table><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><use xlink:href="#symbol" /></svg></body></html>"##;

#[tokio::test]
async fn all_output_modes_preserve_inline_markup_links_and_table_structure() {
    for mode in [
        OutputMode::Bilingual,
        OutputMode::TranslationOnly,
        OutputMode::Interleaved,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(dir.path().into());
        let config = Config {
            target_lang: "zh-TW".into(),
            output_mode: mode,
            ..Config::default()
        };
        let translator = RecordingTranslator::default();
        let output = process_document(
            XHTML.as_bytes(),
            &translator,
            &config.translation_options(),
            &cache,
            0,
            "ch1",
            None,
            None,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_xml(&output);
        let doc = kuchiki::parse_html().one(output.clone());
        assert_eq!(doc.select("tr").unwrap().count(), 2);
        for row in doc.select("tr").unwrap() {
            assert_eq!(
                row.as_node()
                    .children()
                    .filter(|n| n.as_element().is_some())
                    .count(),
                2
            );
        }
        assert_eq!(doc.select("caption").unwrap().count(), 1);
        assert!(doc
            .select_first("caption")
            .unwrap()
            .text_contents()
            .contains("派遣紀錄"));
        assert_eq!(doc.select("#ref").unwrap().count(), 1);
        assert_eq!(doc.select("#note").unwrap().count(), 1);
        assert_eq!(
            doc.select_first("th")
                .unwrap()
                .attributes
                .borrow()
                .get("scope"),
            Some("col")
        );
        let translated = doc
            .select("p[lang='zh-TW']")
            .unwrap()
            .find(|p| p.text_contents().contains("[1]"))
            .unwrap();
        assert_eq!(
            translated
                .as_node()
                .select_first("em")
                .unwrap()
                .text_contents(),
            "世界"
        );
        assert!(translated.as_node().select_first("br").is_ok());
        assert_eq!(
            translated
                .as_node()
                .select_first("a")
                .unwrap()
                .attributes
                .borrow()
                .get("href"),
            Some("#note")
        );
        assert!(!output.contains("[[BABEL:"));
        assert!(!translator
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.0 == "27"));
    }
}

struct BrokenMarkers;
struct InventedMarkers;

#[tokio::test]
async fn protected_markup_survives_chunking_and_refinement() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let config = Config {
        refine: true,
        max_output_tokens: 260,
        max_input_tokens: 2000,
        target_lang: "zh-TW".into(),
        ..Config::default()
    };
    let translator = RecordingTranslator::default();
    let html = format!(
        "<p>Hello <em>{}</em><br /><a href=\"#note\">Hello world</a></p>",
        "world ".repeat(90)
    );
    let output = process_document(
        html.as_bytes(),
        &translator,
        &config.translation_options(),
        &cache,
        0,
        "ch1",
        None,
        None,
    )
    .await
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert_xml(&output);
    let doc = kuchiki::parse_html().one(output);
    let paragraph = doc.select_first("p[lang='zh-TW']").unwrap();
    assert_eq!(paragraph.as_node().select("em").unwrap().count(), 1);
    assert_eq!(
        paragraph
            .as_node()
            .select_first("em")
            .unwrap()
            .text_contents()
            .matches("世界")
            .count(),
        90
    );
    assert!(paragraph.as_node().select_first("br").is_ok());
    assert_eq!(
        paragraph
            .as_node()
            .select_first("a")
            .unwrap()
            .attributes
            .borrow()
            .get("href"),
        Some("#note")
    );
    assert!(translator.requests.lock().unwrap().len() > 2);
}

#[tokio::test]
async fn translation_only_keeps_translated_attributes_on_the_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let config = Config {
        output_mode: OutputMode::TranslationOnly,
        translate_attributes: vec!["title".into()],
        ..Config::default()
    };
    let output = process_document(
        b"<p title=\"Hello world\">Hello world</p>",
        &RecordingTranslator::default(),
        &config.translation_options(),
        &cache,
        0,
        "ch1",
        None,
        None,
    )
    .await
    .unwrap();
    let doc = kuchiki::parse_html().one(String::from_utf8(output).unwrap());
    assert_eq!(
        doc.select_first("p")
            .unwrap()
            .attributes
            .borrow()
            .get("title"),
        Some("你好 世界")
    );
}

#[async_trait]
impl Translator for BrokenMarkers {
    fn name(&self) -> String {
        "broken".into()
    }
    fn max_output_tokens(&self) -> usize {
        2000
    }
    async fn translate(
        &self,
        text: &str,
        _: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        let markers = regex::Regex::new(r"\[\[BABEL:\d+:(OPEN|CLOSE|KEEP)\]\]").unwrap();
        Ok(markers
            .replace_all(text, "")
            .replace("Hello", "你好")
            .replace("world", "世界"))
    }
}

#[tokio::test]
async fn invented_formatting_in_fallback_is_rejected_and_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let config = Config::default();
    let result = process_document(
        b"<p>Hello <em>world</em></p>",
        &InventedMarkers,
        &config.translation_options(),
        &cache,
        0,
        "ch1",
        None,
        None,
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("formatting markers"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn checkpoint_signature_changes_with_settings_and_ignores_credentials() {
    use babel_ebook::checkpoint::CheckpointStore;
    let mut config = Config::default();
    let original = CheckpointStore::translation_signature(&config);
    config.api_key = Some("test-secret".into());
    config.output = "different.epub".into();
    assert_eq!(original, CheckpointStore::translation_signature(&config));
    config.system_prompt = Some("changed prompt".into());
    assert_ne!(original, CheckpointStore::translation_signature(&config));
    let previous = CheckpointStore::translation_signature(&config);
    config.glossary.push(GlossaryEntry {
        term: "Vale".into(),
        translation: "維爾".into(),
        context: None,
    });
    assert_ne!(previous, CheckpointStore::translation_signature(&config));
}

#[async_trait]
impl Translator for InventedMarkers {
    fn name(&self) -> String {
        "invented".into()
    }
    fn max_output_tokens(&self) -> usize {
        2000
    }
    async fn translate(
        &self,
        _: &str,
        _: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        Ok("[[BABEL:999:KEEP]]譯文".into())
    }
}

#[tokio::test]
async fn missing_markers_fall_back_without_losing_links_images_or_emphasis() {
    for refine in [false, true] {
        for mode in [
            OutputMode::Bilingual,
            OutputMode::TranslationOnly,
            OutputMode::Interleaved,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cache = TranslationCache::new(dir.path().into());
            let config = Config {
                refine,
                output_mode: mode,
                target_lang: "zh-TW".into(),
                ..Config::default()
            };
            let output = process_document(
                XHTML.as_bytes(),
                &BrokenMarkers,
                &config.translation_options(),
                &cache,
                0,
                "ch1",
                None,
                None,
            )
            .await
            .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert_xml(&output);
            assert!(!output.contains("[[BABEL:"));
            let doc = kuchiki::parse_html().one(output);
            assert_eq!(doc.select("#ref").unwrap().count(), 1);
            assert_eq!(doc.select("#note").unwrap().count(), 1);
            let translated = doc
                .select("p[lang='zh-TW']")
                .unwrap()
                .find(|p| p.text_contents().contains("[1]"))
                .unwrap();
            assert_eq!(
                translated
                    .as_node()
                    .select_first("em")
                    .unwrap()
                    .text_contents(),
                "世界"
            );
            assert!(translated.as_node().select_first("br").is_ok());
            assert_eq!(
                translated
                    .as_node()
                    .select_first("a")
                    .unwrap()
                    .attributes
                    .borrow()
                    .get("href"),
                Some("#note")
            );
        }
    }
}

#[tokio::test]
async fn empty_page_anchors_are_single_opaque_markers() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let translator = RecordingTranslator::default();
    let config = Config {
        output_mode: OutputMode::TranslationOnly,
        ..Config::default()
    };
    let output = process_document(
        b"<h1><a id='page_264'></a><a id='page_265'> </a>Hello world</h1><p>Hello <code>SECRET</code><img src='x.jpg' /></p>",
        &translator, &config.translation_options(), &cache, 0, "ch1", None, None
    ).await.unwrap();
    let doc = kuchiki::parse_html().one(String::from_utf8(output).unwrap());
    assert!(doc.select_first("#page_264").is_ok());
    assert!(doc.select_first("#page_265").is_ok());
    assert_eq!(doc.select_first("code").unwrap().text_contents(), "SECRET");
    assert_eq!(
        doc.select_first("img")
            .unwrap()
            .attributes
            .borrow()
            .get("src"),
        Some("x.jpg")
    );
    let requests = translator.requests.lock().unwrap();
    assert!(!requests[0].0.contains(":OPEN"));
    assert_eq!(requests[0].0.matches(":KEEP").count(), 2);
    assert!(requests.iter().all(|r| !r.0.contains("SECRET")));
}

#[tokio::test]
async fn cancellation_interrupts_an_in_flight_translation() {
    struct WaitingTranslator;
    #[async_trait]
    impl Translator for WaitingTranslator {
        fn name(&self) -> String {
            "waiting".into()
        }
        fn max_output_tokens(&self) -> usize {
            2000
        }
        async fn translate(
            &self,
            _: &str,
            _: &TranslateContext<'_>,
        ) -> Result<String, BabelEbookError> {
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let token = babel_ebook::CancellationToken::default();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    });
    let result = translate_text(
        "Hello world",
        &WaitingTranslator,
        &Config::default().translation_options(),
        &cache,
        0,
        "ch1",
        None,
        Some(&token),
    )
    .await;
    assert!(matches!(result, Err(BabelEbookError::Cancelled)));
}

#[tokio::test]
async fn many_marker_failures_add_at_most_one_request_per_chapter() {
    #[derive(Default)]
    struct CostTracker {
        calls: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Translator for CostTracker {
        fn name(&self) -> String {
            "cost-tracker".into()
        }
        fn max_output_tokens(&self) -> usize {
            2000
        }
        async fn translate(
            &self,
            text: &str,
            _: &TranslateContext<'_>,
        ) -> Result<String, BabelEbookError> {
            self.calls.lock().unwrap().push(text.into());
            if text.starts_with('[') && !text.starts_with("[[BABEL:") {
                let runs: Vec<String> = serde_json::from_str(text).unwrap();
                return Ok(serde_json::to_string(&runs).unwrap());
            }
            let re = regex::Regex::new(r"\[\[BABEL:\d+:(OPEN|CLOSE|KEEP)\]\]").unwrap();
            Ok(re.replace_all(text, "").into_owned())
        }
    }
    for refine in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(dir.path().into());
        let model = CostTracker::default();
        let config = Config {
            refine,
            ..Config::default()
        };
        let html = (0..40).map(|i| format!("<p>Unique paragraph {i} <em>emphasis {i}</em> and <a href='#n'>link {i}</a>.</p>")).collect::<String>();
        process_document(
            html.as_bytes(),
            &model,
            &config.translation_options(),
            &cache,
            0,
            "ch1",
            None,
            None,
        )
        .await
        .unwrap();
        let calls = model.calls.lock().unwrap();
        let base = if refine { 80 } else { 40 };
        assert_eq!(
            calls.len(),
            base + 1,
            "no per-run calls or repeated marker attempts"
        );
        assert_eq!(calls.iter().filter(|s| s.contains("[[BABEL:")).count(), 1);
    }
}
