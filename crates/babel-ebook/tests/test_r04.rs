//! Offline R04 regressions: malformed fragment recovery, bounded cost and resume.

use async_trait::async_trait;
use babel_ebook::{
    process_document, BabelEbookError, CancellationToken, Config, OutputMode, TranslateContext,
    TranslationCache, Translator,
};
use kuchiki::traits::TendrilSink;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};

struct FaultModel {
    strict: bool,
    bad_group_calls: usize,
    bad_reply: &'static str,
    group_calls: AtomicUsize,
    calls: Mutex<Vec<String>>,
    fail_last: AtomicBool,
    empty_plain: bool,
    provider_failure: bool,
    cancel_after_response: Option<CancellationToken>,
}

impl FaultModel {
    fn new(strict: bool, bad_group_calls: usize, bad_reply: &'static str) -> Self {
        Self {
            strict,
            bad_group_calls,
            bad_reply,
            group_calls: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
            fail_last: AtomicBool::new(false),
            empty_plain: false,
            provider_failure: false,
            cancel_after_response: None,
        }
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl Translator for FaultModel {
    fn name(&self) -> String {
        "r04-fault:model".into()
    }
    fn max_output_tokens(&self) -> usize {
        3000
    }
    fn fragment_response_format(&self, count: usize) -> Option<serde_json::Value> {
        self.strict
            .then(|| serde_json::json!({"type":"json_schema", "count":count}))
    }
    async fn translate(
        &self,
        text: &str,
        context: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        self.calls.lock().unwrap().push(text.into());
        if self.provider_failure || (self.fail_last.load(Ordering::SeqCst) && text.contains("LAST"))
        {
            return Err(BabelEbookError::ApiError(
                "simulated provider failure".into(),
            ));
        }
        if self.empty_plain {
            return Ok(" ".into());
        }
        assert!(!text.contains("[[BABEL:"));
        // A split singleton still sees the original paragraph for context.
        if text == "FIRST" || text == "LAST" {
            assert!(context
                .system_prompt
                .contains("Paragraph context (reference only)"));
        }
        Ok(text.into())
    }
    async fn translate_fragments(
        &self,
        text: &str,
        context: &TranslateContext<'_>,
        count: usize,
    ) -> Result<String, BabelEbookError> {
        self.calls.lock().unwrap().push(text.into());
        if self.provider_failure {
            return Err(BabelEbookError::ApiError(
                "simulated provider failure".into(),
            ));
        }
        assert!(context
            .system_prompt
            .contains("overrides prose-only output rules"));
        let runs: Vec<String> = serde_json::from_str(text).unwrap();
        assert_eq!(runs.len(), count);
        assert!(!text.contains("[[BABEL:"));
        let call = self.group_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(token) = &self.cancel_after_response {
            token.cancel();
        }
        if call <= self.bad_group_calls {
            return Ok(self.bad_reply.into());
        }
        Ok(if self.strict {
            serde_json::json!({"translations":runs}).to_string()
        } else {
            serde_json::to_string(&runs).unwrap()
        })
    }
}

fn config() -> Config {
    Config {
        target_lang: "zh-TW".into(),
        output_mode: OutputMode::TranslationOnly,
        max_input_tokens: 4000,
        system_prompt: Some(
            "Translate faithfully. The final output must contain only the translated text.".into(),
        ),
        ..Config::default()
    }
}

async fn run(
    html: &str,
    model: &FaultModel,
    cache: &TranslationCache,
    token: Option<&CancellationToken>,
) -> Result<String, BabelEbookError> {
    process_document(
        html.as_bytes(),
        model,
        &config().translation_options(),
        cache,
        0,
        "Act-13.xhtml",
        None,
        token,
    )
    .await
    .map(|bytes| String::from_utf8(bytes).unwrap())
}

#[tokio::test]
async fn malformed_count_json_and_empty_fragment_retry_once_then_cache() {
    for strict in [false, true] {
        for reply in if strict {
            [
                r#"{"translations":["one"]}"#,
                "not JSON",
                r#"{"translations":["one"," "]}"#,
            ]
        } else {
            [r#"["one"]"#, "not JSON", r#"["one"," "]"#]
        } {
            let dir = tempfile::tempdir().unwrap();
            let cache = TranslationCache::new(dir.path().into());
            let model = FaultModel::new(strict, 1, reply);
            let html = "<p>Before <em>emphasis</em>.</p>";
            let first = run(html, &model, &cache, None).await.unwrap();
            assert!(first.contains("<em>emphasis</em>"));
            assert_eq!(model.count(), 2);
            assert_eq!(run(html, &model, &cache, None).await.unwrap(), first);
            assert_eq!(
                model.count(),
                2,
                "the successful full paragraph must be reused"
            );
            let records: Vec<_> = std::fs::read_dir(dir.path().join("diagnostics-r04"))
                .unwrap()
                .collect();
            assert_eq!(records.len(), 1);
            let path = records[0].as_ref().unwrap().path();
            let record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(record["chapter_href"], "Act-13.xhtml");
            assert_eq!(record["selected_element"], 1);
            assert_eq!(record["expected_count"], 2);
            assert_eq!(record["response"], reply);
            assert!(record.get("system_prompt").is_none());
        }
    }
}

#[tokio::test]
async fn splitting_preserves_inline_elements_links_numbers_and_text_order() {
    for strict in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(dir.path().into());
        let model = FaultModel::new(strict, usize::MAX, "bad JSON");
        let html = "<p id='p1'>7:30 <span class='sc'>P.M.</span><a href='#note' id='ref'>the witness</a> arrived.<br/>123.</p><aside id='note'>note</aside>";
        let output = run(html, &model, &cache, None).await.unwrap();
        let before = kuchiki::parse_html().one(html);
        let after = kuchiki::parse_html().one(output.clone());
        assert_eq!(
            before.select_first("p").unwrap().text_contents(),
            after.select_first("p").unwrap().text_contents()
        );
        assert_eq!(after.select("#ref").unwrap().count(), 1);
        assert!(output.contains("href=\"#note\""));
        assert!(output.contains("class=\"sc\""));
        assert_eq!(after.select("br").unwrap().count(), 1);
        assert!(model.count() <= 9);
    }
}

#[tokio::test]
async fn fragment_recovery_stops_at_shared_nine_request_limit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let model = FaultModel::new(true, usize::MAX, r#"{"translations":[]}"#);
    let html = format!(
        "<p>{}</p>",
        (0..8)
            .map(|i| format!("<em>Fragment {i}</em>"))
            .collect::<String>()
    );
    let error = run(&html, &model, &cache, None).await.unwrap_err();
    assert!(error.to_string().contains("limit 9"), "{error}");
    assert!(error.to_string().contains("selected element 1"));
    assert_eq!(model.count(), 9);
}

#[tokio::test]
async fn empty_singleton_is_rejected_without_caching_an_incomplete_paragraph() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let mut bad = FaultModel::new(true, usize::MAX, r#"{"translations":[]}"#);
    bad.empty_plain = true;
    let html = "<p>Before <em>emphasis</em>.</p>";
    let error = run(html, &bad, &cache, None).await.unwrap_err();
    assert!(error.to_string().contains("empty fragment"));
    assert!(error.to_string().contains("diagnostic="));
    assert_eq!(bad.count(), 3);
    let good = FaultModel::new(true, 0, "");
    run(html, &good, &cache, None).await.unwrap();
    assert_eq!(
        good.count(),
        1,
        "failed root must never become a successful cache entry"
    );
}

#[tokio::test]
async fn successful_split_group_survives_failure_and_is_reused_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let model = FaultModel::new(true, usize::MAX, "bad JSON");
    model.fail_last.store(true, Ordering::SeqCst);
    let html = "<p>FIRST<em>LAST</em></p>";
    assert!(run(html, &model, &cache, None).await.is_err());
    assert_eq!(model.count(), 4);
    model.fail_last.store(false, Ordering::SeqCst);
    run(html, &model, &cache, None).await.unwrap();
    assert_eq!(
        model.count(),
        7,
        "retry root twice, reuse FIRST, request only LAST"
    );
    run(html, &model, &cache, None).await.unwrap();
    assert_eq!(model.count(), 7);
}

#[tokio::test]
async fn cancellation_and_provider_errors_do_not_trigger_format_retries() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let token = CancellationToken::default();
    let mut cancelled = FaultModel::new(true, usize::MAX, "bad JSON");
    cancelled.cancel_after_response = Some(token.clone());
    assert!(matches!(
        run(
            "<p>Before <em>emphasis</em></p>",
            &cancelled,
            &cache,
            Some(&token)
        )
        .await,
        Err(BabelEbookError::Cancelled)
    ));
    assert_eq!(cancelled.count(), 1);
    let mut failure = FaultModel::new(true, 0, "");
    failure.provider_failure = true;
    assert!(
        run("<p>Before <em>emphasis</em></p>", &failure, &cache, None)
            .await
            .is_err()
    );
    assert_eq!(failure.count(), 1);
}
