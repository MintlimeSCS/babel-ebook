//! Offline integration regressions for grouping, fallback and protected XHTML.
use async_trait::async_trait;
use babel_ebook::{
    process_document, BabelEbookError, CancellationToken, Config, OutputMode, TranslateContext,
    TranslationCache, Translator,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
struct Mock {
    calls: AtomicUsize,
    batches: AtomicUsize,
    merged_batches: AtomicUsize,
    failure: &'static str,
    cancel: Option<CancellationToken>,
    inputs: Mutex<Vec<String>>,
}
impl Mock {
    fn new(failure: &'static str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            batches: AtomicUsize::new(0),
            merged_batches: AtomicUsize::new(0),
            failure,
            cancel: None,
            inputs: Mutex::new(vec![]),
        }
    }
}
#[async_trait]
impl Translator for Mock {
    fn name(&self) -> String {
        "merge:test".into()
    }
    fn max_output_tokens(&self) -> usize {
        6000
    }
    async fn translate(
        &self,
        text: &str,
        _: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(text.into());
        if let Ok(parts) = serde_json::from_str::<Vec<String>>(text) {
            return Ok(serde_json::to_string(
                &parts.iter().map(|s| format!("譯{s}")).collect::<Vec<_>>(),
            )
            .unwrap());
        }
        Ok(format!("譯{text}"))
    }
    async fn translate_fragments(
        &self,
        text: &str,
        _: &TranslateContext<'_>,
        _: usize,
    ) -> Result<String, BabelEbookError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.batches.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(text.into());
        let parts: Vec<String> = serde_json::from_str(text).unwrap();
        // R02 also uses translate_fragments to preserve inline markup. Only
        // the R03 paragraph envelope identifies a paragraph-merge request.
        if parts.iter().any(|s| s.starts_with("[[BABEL_P:")) {
            self.merged_batches.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(token) = &self.cancel {
            token.cancel();
            return Err(BabelEbookError::Cancelled);
        }
        if self.failure == "truncated" {
            return Err(BabelEbookError::OutputTruncated("mock".into()));
        }
        let mut rows: Vec<String> = parts
            .iter()
            .map(|s| {
                if s.starts_with("[[BABEL_P:") {
                    let (id, source) = s.split_once("]]").unwrap();
                    format!("{id}]]譯{source}")
                } else {
                    format!("譯{s}")
                }
            })
            .collect();
        match self.failure {
            "missing" => {
                rows.pop();
            }
            "reordered" => rows.reverse(),
            "duplicate" => {
                rows[1] = rows[0].clone();
            }
            "empty" => rows[0].clear(),
            "bad-json" => return Ok("broken JSON".into()),
            _ => {}
        }
        Ok(serde_json::to_string(&rows).unwrap())
    }
}
fn options(mode: OutputMode) -> babel_ebook::TranslationOptions {
    let mut c = Config::default();
    c.output_mode = mode;
    c.source_lang = "en".into();
    c.target_lang = "zh-TW".into();
    c.system_prompt = Some("Translate faithfully to Traditional Chinese".into());
    c.max_output_tokens = 6000;
    c.paragraph_merge.enabled = true;
    c.translation_options()
}
async fn run(
    html: &str,
    m: &Mock,
    o: &babel_ebook::TranslationOptions,
    c: &TranslationCache,
    cancel: Option<&CancellationToken>,
) -> Result<String, BabelEbookError> {
    process_document(html.as_bytes(), m, o, c, 0, "chapter.xhtml", None, cancel)
        .await
        .map(|s| String::from_utf8(s).unwrap())
}
#[tokio::test]
async fn merges_preserves_order_modes_and_reuses_individual_cache() {
    for mode in [
        OutputMode::TranslationOnly,
        OutputMode::Bilingual,
        OutputMode::Interleaved,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(dir.path().into());
        let m = Mock::new("");
        let mut o = options(mode);
        let html="<html><body><p>First source</p> \n<p>Second source</p><p>Third source</p></body></html>";
        let output = run(html, &m, &o, &cache, None).await.unwrap();
        assert_eq!(m.calls.load(Ordering::SeqCst), 1);
        assert_eq!(m.batches.load(Ordering::SeqCst), 1);
        assert_eq!(m.merged_batches.load(Ordering::SeqCst), 1);
        assert!(output.find("譯First source").unwrap() < output.find("譯Second source").unwrap());
        assert!(output.find("譯Second source").unwrap() < output.find("譯Third source").unwrap());
        assert!(!output.contains("[[BABEL_P"));
        o.paragraph_merge.enabled = false;
        let again = run(html, &m, &o, &cache, None).await.unwrap();
        assert_eq!(m.calls.load(Ordering::SeqCst), 1);
        assert_eq!(output, again);
    }
}
#[tokio::test]
async fn invalid_or_truncated_batch_falls_back_without_polluting_cache() {
    for failure in [
        "missing",
        "reordered",
        "duplicate",
        "empty",
        "bad-json",
        "truncated",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(dir.path().into());
        let m = Mock::new(failure);
        let o = options(OutputMode::TranslationOnly);
        let output = run(
            "<html><body><p>First source</p><p>Second source</p><p>Third source</p></body></html>",
            &m,
            &o,
            &cache,
            None,
        )
        .await
        .unwrap();
        assert_eq!(m.calls.load(Ordering::SeqCst), 4, "{failure}");
        for s in ["First", "Second", "Third"] {
            assert!(output.contains(&format!("譯{s} source")));
        }
        assert!(!output.contains("[[BABEL"));
    }
}
#[tokio::test]
async fn boundaries_links_and_notes_keep_original_structure() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let m = Mock::new("");
    let o = options(OutputMode::TranslationOnly);
    let html="<html xmlns='http://www.w3.org/1999/xhtml'><body><p>First source</p><h2>Boundary heading</h2><p>Second source</p><p id='reference'>Linked <a href='#note'>source</a></p><section class='footnotes'><p id='note'>Note <a href='#reference'>back</a></p></section><p>Third source</p></body></html>";
    let output = run(html, &m, &o, &cache, None).await.unwrap();
    assert_eq!(m.merged_batches.load(Ordering::SeqCst), 0);
    // The two paragraphs containing links still use the original structured
    // translation path; neither is grouped with a neighbouring paragraph.
    assert_eq!(m.batches.load(Ordering::SeqCst), 2);
    for attr in [
        "id=\"reference\"",
        "href=\"#note\"",
        "id=\"note\"",
        "href=\"#reference\"",
    ] {
        assert!(output.contains(attr), "{attr}");
    }
    let mut xml = quick_xml::Reader::from_str(&output);
    loop {
        match xml.read_event().unwrap() {
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
}
#[tokio::test]
async fn cancellation_does_not_cache_partial_group_or_start_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let token = CancellationToken::default();
    let mut m = Mock::new("");
    m.cancel = Some(token.clone());
    let o = options(OutputMode::TranslationOnly);
    assert!(matches!(
        run(
            "<html><body><p>First source</p><p>Second source</p></body></html>",
            &m,
            &o,
            &cache,
            Some(&token)
        )
        .await,
        Err(BabelEbookError::Cancelled)
    ));
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
#[tokio::test]
async fn insufficient_budget_disabled_and_refinement_use_original_path() {
    for setting in [0, 1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(dir.path().into());
        let m = Mock::new("");
        let mut o = options(OutputMode::TranslationOnly);
        match setting {
            0 => o.paragraph_merge.enabled = false,
            1 => o.max_output_tokens = 200,
            2 => o.refine = true,
            _ => {}
        }
        run(
            "<html><body><p>First source</p><p>Second source</p></body></html>",
            &m,
            &o,
            &cache,
            None,
        )
        .await
        .unwrap();
        assert_eq!(m.batches.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn cached_middle_paragraph_is_not_sent_again_or_bridged() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranslationCache::new(dir.path().into());
    let m = Mock::new("");
    let mut o = options(OutputMode::TranslationOnly);
    o.paragraph_merge.enabled = false;
    run(
        "<html><body><p>Second source</p></body></html>",
        &m,
        &o,
        &cache,
        None,
    )
    .await
    .unwrap();
    o.paragraph_merge.enabled = true;
    run(
        "<html><body><p>First source</p><p>Second source</p><p>Third source</p></body></html>",
        &m,
        &o,
        &cache,
        None,
    )
    .await
    .unwrap();
    assert_eq!(m.calls.load(Ordering::SeqCst), 3);
    assert_eq!(m.batches.load(Ordering::SeqCst), 0);
}
