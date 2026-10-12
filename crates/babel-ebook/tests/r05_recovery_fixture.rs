//! Optional offline check against the user's original EPUB and restored JSON.
//! Fixture bytes are never modified or packaged with the source distribution.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use babel_ebook::checkpoint::{ChapterStatus, Checkpoint, CheckpointStore};
use babel_ebook::pipeline::{run_ordered_pipeline, PipelineContext};
use babel_ebook::{BabelEbookError, Config, TranslateContext, TranslationCache, Translator};

struct OfflineFailure(AtomicUsize);

#[async_trait]
impl Translator for OfflineFailure {
    fn name(&self) -> String {
        "offline-r05-fixture".into()
    }
    fn max_output_tokens(&self) -> usize {
        8000
    }
    async fn translate(
        &self,
        _: &str,
        _: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(BabelEbookError::ApiError(
            "intentional offline chapter failure".into(),
        ))
    }
}

#[tokio::test]
#[ignore = "requires BABEL_R05_SOURCE_FIXTURE and BABEL_R05_RECOVERY_FIXTURE"]
async fn actual_restored_book_keeps_forty_chapters_when_only_chapter_twelve_fails() {
    let source = std::path::PathBuf::from(std::env::var("BABEL_R05_SOURCE_FIXTURE").unwrap());
    let fixture = std::path::PathBuf::from(std::env::var("BABEL_R05_RECOVERY_FIXTURE").unwrap());
    let fixture_bytes = std::fs::read(&fixture).unwrap();
    let mut cp: Checkpoint = serde_json::from_slice(&fixture_bytes).unwrap();
    assert_eq!(
        cp.source_hash,
        CheckpointStore::source_hash(&source).unwrap()
    );
    assert_eq!(cp.chapters.len(), 41);
    assert_eq!(
        cp.chapters
            .iter()
            .filter(|entry| entry.status == ChapterStatus::Completed)
            .count(),
        40
    );
    let original = cp.chapters.clone();
    let source_book = babel_ebook::read_input_book(&source).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        source,
        provider: "offline-fixture".into(),
        model: "offline-fixture".into(),
        max_input_tokens: 6000,
        max_output_tokens: 8000,
        system_prompt: Some("Translate only".into()),
        ..Config::default()
    };
    // The local mock has different settings from the user's real provider.
    // Stamp only this temporary copy; actual legacy signature matching is
    // covered by the separate settings-mismatch regression tests.
    cp.translation_signature = CheckpointStore::translation_signature(&config);
    cp.compatibility_signature.clear();
    let store = CheckpointStore::new(dir.path().join("checkpoints")).unwrap();
    store.save(&cp).unwrap();
    let cache = TranslationCache::new(dir.path().join("cache"));
    let translator = OfflineFailure(AtomicUsize::new(0));
    for input in [6000, 4000] {
        config.max_input_tokens = input;
        let mut book = source_book.clone();
        let indices = (0..book.chapters.len()).collect();
        let context = PipelineContext {
            translator: &translator,
            config: &config,
            cache: &cache,
            progress: None,
            cancellation: None,
        };
        let result = run_ordered_pipeline(
            &mut book,
            indices,
            &context,
            Some(&store),
            Some(&cp.job_id),
            &cp.source_hash,
        )
        .await
        .unwrap();
        assert_eq!(result.restored.len(), 40);
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].0.ends_with("_c12_r1.htm"));
        let saved = store.load(&cp.job_id).unwrap();
        for entry in original
            .iter()
            .filter(|entry| entry.status == ChapterStatus::Completed)
        {
            assert_eq!(saved.chapters[entry.index].content, entry.content);
        }
    }
    assert_eq!(translator.0.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read(fixture).unwrap(), fixture_bytes);
}
