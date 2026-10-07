//! Offline regressions for bounded truncation recovery and R01 cache reuse.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

use async_trait::async_trait;
use babel_ebook::{
    process_document, translate_text, BabelEbookError, CancellationToken, Config, OutputMode,
    TranslateContext, TranslationCache, Translator,
};
use sha2::{Digest, Sha256};

struct LengthTranslator {
    limit: usize,
    calls: Mutex<Vec<String>>,
    fail_right: AtomicBool,
    always_truncate: bool,
    cancellation: Option<CancellationToken>,
    invalid_fragments: bool,
}

impl LengthTranslator {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            calls: Mutex::new(Vec::new()),
            fail_right: AtomicBool::new(false),
            always_truncate: false,
            cancellation: None,
            invalid_fragments: false,
        }
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn respond(&self, text: &str, fragments: bool) -> Result<String, BabelEbookError> {
        self.calls.lock().unwrap().push(text.into());
        if self.count() == 2 {
            if let Some(token) = &self.cancellation {
                token.cancel();
            }
        }
        if self.always_truncate || text.chars().count() > self.limit {
            return Err(BabelEbookError::OutputTruncated(
                "Mock output was truncated".into(),
            ));
        }
        if self.fail_right.load(Ordering::SeqCst) && text.contains("TARGETFAIL") {
            return Err(BabelEbookError::ApiError("temporary test failure".into()));
        }
        if fragments {
            let runs: Vec<String> = serde_json::from_str(text).unwrap();
            Ok(serde_json::json!({"translations": if self.invalid_fragments { vec![] } else { runs }}).to_string())
        } else {
            Ok(text.into())
        }
    }
}

#[async_trait]
impl Translator for LengthTranslator {
    fn name(&self) -> String {
        "r02-length:model".into()
    }
    fn max_output_tokens(&self) -> usize {
        3000
    }
    fn fragment_response_format(&self, _count: usize) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "json_schema"}))
    }
    async fn translate(
        &self,
        text: &str,
        _context: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        self.respond(text, false)
    }
    async fn translate_fragments(
        &self,
        text: &str,
        _context: &TranslateContext<'_>,
        _count: usize,
    ) -> Result<String, BabelEbookError> {
        self.respond(text, true)
    }
}

fn config() -> Config {
    Config {
        source_lang: "en".into(),
        target_lang: "zh-TW".into(),
        max_input_tokens: 4000,
        max_output_tokens: 3000,
        temperature: 0.2,
        system_prompt: Some("Translate only.".into()),
        output_mode: OutputMode::TranslationOnly,
        ..Config::default()
    }
}

fn source() -> String {
    (1..=16)
        .map(|i| format!("Sentence {i} contains evidence and careful investigation. "))
        .collect::<String>()
        .trim()
        .into()
}

async fn run(
    text: &str,
    translator: &LengthTranslator,
    cache: &TranslationCache,
    config: &Config,
    cancellation: Option<&CancellationToken>,
) -> Result<String, BabelEbookError> {
    translate_text(
        text,
        translator,
        &config.translation_options(),
        cache,
        15,
        "OEBPS/Text/Act-13.xhtml",
        None,
        cancellation,
    )
    .await
}

#[tokio::test]
async fn truncation_splits_once_recombines_in_order_and_reuses_complete_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let text = source();
    let translator = LengthTranslator::new(text.chars().count() / 2 + 20);
    assert_eq!(
        run(&text, &translator, &cache, &config(), None)
            .await
            .unwrap(),
        text
    );
    assert_eq!(translator.count(), 3);
    assert_eq!(
        run(&text, &translator, &cache, &config(), None)
            .await
            .unwrap(),
        text
    );
    assert_eq!(translator.count(), 3, "completed paragraph must be reused");
}

#[tokio::test]
async fn failed_recovery_retains_valid_subpieces_without_caching_partial_paragraph() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let text = format!("{} TARGETFAIL.", source());
    let translator = LengthTranslator::new(text.chars().count() / 2 + 20);
    translator.fail_right.store(true, Ordering::SeqCst);
    assert!(matches!(
        run(&text, &translator, &cache, &config(), None).await,
        Err(BabelEbookError::ApiError(_))
    ));
    assert_eq!(translator.count(), 3);
    translator.fail_right.store(false, Ordering::SeqCst);
    assert_eq!(
        run(&text, &translator, &cache, &config(), None)
            .await
            .unwrap(),
        text
    );
    assert_eq!(
        translator.count(),
        5,
        "retry root and failed right; reuse validated left"
    );
}

#[tokio::test]
async fn persistent_truncation_is_bounded_and_reports_chapter() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let mut translator = LengthTranslator::new(0);
    translator.always_truncate = true;
    let error = run(&source(), &translator, &cache, &config(), None)
        .await
        .unwrap_err();
    assert!(matches!(error, BabelEbookError::OutputTruncated(_)));
    assert!(error.to_string().contains("Act-13.xhtml"));
    assert!(error.to_string().contains("stopped"));
    assert!(translator.count() <= 9);
    let before = translator.count();
    assert!(run(&source(), &translator, &cache, &config(), None)
        .await
        .is_err());
    assert!(
        translator.count() > before,
        "failed full source must not be cached"
    );
}

#[tokio::test]
async fn broad_recovery_tree_stops_at_nine_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let text = source();
    let translator = LengthTranslator::new(text.chars().count() / 8 + 25);
    let error = run(&text, &translator, &cache, &config(), None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("limit 9"));
    assert_eq!(translator.count(), 9);
}

#[tokio::test]
async fn invalid_fragment_contract_during_recovery_fails_without_format_retries() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let content = (0..8)
        .map(|i| format!("<em>Fragment {i} has evidence from investigators. </em>"))
        .collect::<String>();
    let html = format!("<html><body><p>{content}</p></body></html>");
    let mut translator = LengthTranslator::new(250);
    translator.invalid_fragments = true;
    let error = process_document(
        html.as_bytes(),
        &translator,
        &config().translation_options(),
        &cache,
        15,
        "Act-13.xhtml",
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, BabelEbookError::ApiError(_)));
    assert!(error.to_string().contains("fragments"));
    assert_eq!(translator.count(), 2);
}

#[tokio::test]
async fn cancellation_interrupts_recovery_before_more_pieces_are_requested() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let text = source();
    let token = CancellationToken::default();
    let mut translator = LengthTranslator::new(text.chars().count() / 2 + 20);
    translator.cancellation = Some(token.clone());
    assert!(matches!(
        run(&text, &translator, &cache, &config(), Some(&token)).await,
        Err(BabelEbookError::Cancelled)
    ));
    assert_eq!(translator.count(), 2);
}

#[tokio::test]
async fn recovery_keeps_unicode_inline_links_and_marker_order_in_all_output_modes() {
    for mode in [
        OutputMode::TranslationOnly,
        OutputMode::Bilingual,
        OutputMode::Interleaved,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(dir.path().into());
        let content = "Réunion 留尼旺島 馬爾地夫 evidence. ".repeat(12);
        let html = format!(
            r##"<html><body><p id="ref">{content}<em>{content}</em><a href="#note">link</a></p><p id="note">Note.</p></body></html>"##
        );
        let translator = LengthTranslator::new(content.chars().count() + 60);
        let mut config = config();
        config.output_mode = mode;
        let output = process_document(
            html.as_bytes(),
            &translator,
            &config.translation_options(),
            &cache,
            15,
            "Act-13.xhtml",
            None,
            None,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let mut xml = quick_xml::Reader::from_str(&output);
        while !matches!(xml.read_event().unwrap(), quick_xml::events::Event::Eof) {}
        assert!(output.contains("Réunion 留尼旺島 馬爾地夫"));
        assert!(output.contains("<em>"));
        assert!(output.contains("href=\"#note\""));
        assert!(!output.contains("[[BABEL:"));
        assert!(translator
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.contains("[[BABEL:")));
        assert!(
            translator.count() > 2,
            "formatted paragraph must exercise recovery"
        );
    }
}

#[tokio::test]
async fn refinement_uses_same_bounded_recovery_with_a_separate_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let text = source();
    let translator = LengthTranslator::new(text.chars().count() / 2 + 20);
    let mut config = config();
    config.refine = true;
    assert_eq!(
        run(&text, &translator, &cache, &config, None)
            .await
            .unwrap(),
        text
    );
    assert_eq!(
        translator.count(),
        6,
        "translation and refinement each recover independently"
    );
    run(&text, &translator, &cache, &config, None)
        .await
        .unwrap();
    assert_eq!(translator.count(), 6);
}

#[tokio::test]
async fn compatible_r01_full_text_cache_is_reused_without_a_request() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let config = config();
    let mut translator = LengthTranslator::new(0);
    translator.always_truncate = true;
    // The R01 v2 key contract, independent of R02's private helper.
    let settings = serde_json::json!([
        translator.cache_identity(),
        config.source_lang,
        config.target_lang,
        config.system_prompt_for_chapter("Act-13.xhtml"),
        4000,
        3000,
        config.temperature,
        3000,
        "translate"
    ]);
    let scope = format!(
        "translation-v2-{}",
        hex::encode(Sha256::digest(settings.to_string().as_bytes()))
    );
    cache.put(&scope, "Existing paragraph", "R01 的已完成譯文", None);
    assert_eq!(
        run("Existing paragraph", &translator, &cache, &config, None)
            .await
            .unwrap(),
        "R01 的已完成譯文"
    );
    assert_eq!(translator.count(), 0);
}
