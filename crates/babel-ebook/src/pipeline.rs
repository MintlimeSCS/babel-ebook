//! Ordered, bounded-concurrency translation pipeline.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cache::TranslationCache;
use crate::checkpoint::{ChapterCheckpoint, ChapterStatus, Checkpoint, CheckpointStore};
use crate::config::Config;
use crate::core::{BabelEbookError, CancellationToken, ProgressCallback, ProgressEvent};
use crate::epub::{Chapter, EpubBook};
use crate::html::process_document;
use crate::translator::Translator;

/// Dependencies shared across pipeline stages.
///
/// Bundling these references reduces the argument surface of
/// `run_ordered_pipeline` and makes the data flow explicit.
pub struct PipelineContext<'a> {
    /// Translator used to convert text.
    pub translator: &'a dyn Translator,
    /// Runtime configuration.
    pub config: &'a Config,
    /// Translation cache.
    pub cache: &'a TranslationCache,
    /// Optional progress callback.
    pub progress: Option<&'a dyn ProgressCallback>,
    /// Optional cooperative cancellation token.
    pub cancellation: Option<&'a CancellationToken>,
}

/// Result of running the ordered pipeline.
pub struct PipelineResult {
    /// `(href, error)` pairs for chapters that failed to translate.
    pub failures: Vec<(String, BabelEbookError)>,
    /// Updated chapters in original spine order.
    pub chapters: Vec<Chapter>,
}

/// Translate chapters in index order while allowing `config.concurrency`
/// concurrent requests.
///
/// Results are applied to `book.chapters` in the original spine order so that
/// resume positions remain stable.
///
/// The returned future is not `Send` because `kuchiki` uses `Rc` internally.
/// Callers that need a `Send` future should run the work on a local runtime.
#[allow(clippy::future_not_send)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub async fn run_ordered_pipeline(
    book: &mut EpubBook,
    indices: Vec<usize>,
    context: &PipelineContext<'_>,
    checkpoint_store: Option<&CheckpointStore>,
    job_id: Option<&str>,
    source_hash: &str,
) -> Result<PipelineResult, BabelEbookError> {
    if indices.is_empty() {
        return Ok(PipelineResult {
            failures: Vec::new(),
            chapters: book.chapters.clone(),
        });
    }

    let job_id = resolve_job_id(checkpoint_store, job_id, context.config);
    let signature = CheckpointStore::translation_signature(context.config);
    let mut checkpoint = build_checkpoint(
        book,
        &indices,
        checkpoint_store,
        &job_id,
        source_hash,
        &signature,
    );
    checkpoint.source_path = context.config.source.to_string_lossy().into_owned();
    let completed = restore_completed_chapters(book, &indices, &checkpoint, context.progress);
    let pending_indices: Vec<usize> = indices
        .into_iter()
        .filter(|i| !completed.contains(i))
        .collect();

    let checkpoint = Arc::new(tokio::sync::Mutex::new(checkpoint));
    let semaphore = Arc::new(Semaphore::new(context.config.concurrency.max(1)));
    let mut futures = FuturesUnordered::new();

    for &index in &pending_indices {
        ensure_not_cancelled(context.cancellation)?;

        let href = book.chapters[index].href.clone();
        let chapter_content = book.chapters[index].content.clone();
        let options = context.config.translation_options();
        let semaphore = Arc::clone(&semaphore);
        futures.push(async move {
            let result = match acquire_permit(semaphore).await {
                Ok(permit) => {
                    emit_progress(
                        context.progress,
                        ProgressEvent::ChapterStarted {
                            index,
                            href: href.clone(),
                        },
                    );
                    let output = if context
                        .cancellation
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        Err(BabelEbookError::Cancelled)
                    } else {
                        process_document(
                            &chapter_content,
                            context.translator,
                            &options,
                            context.cache,
                            index,
                            &href,
                            context.progress,
                            context.cancellation,
                        )
                        .await
                    };
                    drop(permit);
                    output
                }
                Err(err) => Err(err),
            };

            Ok::<(usize, Result<Vec<u8>, BabelEbookError>), BabelEbookError>((index, result))
        });
    }

    let mut failures = Vec::new();
    while let Some(item) = futures.next().await {
        let (index, result) = item?;
        // Persist one completed result at a time. Concurrent snapshots must not
        // overwrite newer checkpoints or race on the store's temporary file.
        update_checkpoint_entry(&checkpoint, index, &result, checkpoint_store, &job_id).await;
        if matches!(result, Err(BabelEbookError::Cancelled)) {
            return Err(BabelEbookError::Cancelled);
        }
        // Emit events on arrival, even when an earlier chapter is still running.
        // Assigning by spine index preserves book order independently of events.
        match result {
            Ok(chapter_content) => {
                book.chapters[index].content = chapter_content;
                emit_progress(
                    context.progress,
                    ProgressEvent::ChapterFinished {
                        index,
                        href: book.chapters[index].href.clone(),
                    },
                );
            }
            Err(err) => {
                let href = book.chapters[index].href.clone();
                emit_progress(
                    context.progress,
                    ProgressEvent::Failed {
                        index,
                        href: href.clone(),
                        error: err.to_string(),
                    },
                );
                failures.push((index, href, err));
            }
        }
        ensure_not_cancelled(context.cancellation)?;
    }
    failures.sort_by_key(|(index, _, _)| *index);

    if let Some(store) = checkpoint_store {
        let cp_to_save = {
            let cp = checkpoint.lock().await;
            cp.clone()
        };
        if let Err(err) = store.save_async(&cp_to_save).await {
            tracing::warn!(job_id, error = %err, "failed to save final checkpoint");
        }
    }

    Ok(PipelineResult {
        failures: failures
            .into_iter()
            .map(|(_, href, err)| (href, err))
            .collect(),
        chapters: book.chapters.clone(),
    })
}

fn ensure_not_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), BabelEbookError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(BabelEbookError::Cancelled);
    }
    Ok(())
}

fn resolve_job_id(
    checkpoint_store: Option<&CheckpointStore>,
    job_id: Option<&str>,
    config: &Config,
) -> String {
    if checkpoint_store.is_some() {
        job_id.map_or_else(
            || {
                CheckpointStore::generate_job_id(
                    &config.source,
                    &config.target_lang,
                    config.output_mode,
                    &config.provider,
                    &config.model,
                )
            },
            ToString::to_string,
        )
    } else {
        job_id.unwrap_or("").to_string()
    }
}

fn build_checkpoint(
    book: &EpubBook,
    indices: &[usize],
    checkpoint_store: Option<&CheckpointStore>,
    job_id: &str,
    source_hash: &str,
    signature: &str,
) -> Checkpoint {
    let loaded = checkpoint_store.and_then(|store| store.load(job_id));
    let mut checkpoint = if let Some(cp) = loaded {
        if cp.source_hash != source_hash || cp.translation_signature != signature {
            tracing::warn!(
                job_id,
                stored_hash = %cp.source_hash,
                current_hash = %source_hash,
                "source or translation settings changed; ignoring existing checkpoint"
            );
            Checkpoint {
                job_id: job_id.to_string(),
                source_hash: source_hash.to_string(),
                translation_signature: signature.to_string(),
                source_path: String::new(),
                chapters: Vec::new(),
            }
        } else {
            cp
        }
    } else {
        Checkpoint {
            job_id: job_id.to_string(),
            source_hash: source_hash.to_string(),
            translation_signature: signature.to_string(),
            source_path: String::new(),
            chapters: Vec::new(),
        }
    };

    let existing_indices: HashSet<usize> = checkpoint.chapters.iter().map(|c| c.index).collect();
    for &index in indices {
        if !existing_indices.contains(&index) {
            checkpoint.chapters.push(ChapterCheckpoint {
                index,
                href: book.chapters[index].href.clone(),
                status: ChapterStatus::Pending,
                content: None,
                error: None,
            });
        }
    }
    checkpoint.chapters.sort_by_key(|c| c.index);
    checkpoint
}

fn restore_completed_chapters(
    book: &mut EpubBook,
    indices: &[usize],
    checkpoint: &Checkpoint,
    progress: Option<&dyn ProgressCallback>,
) -> HashSet<usize> {
    let completed: HashSet<usize> = checkpoint
        .chapters
        .iter()
        .filter(|c| c.status == ChapterStatus::Completed)
        .map(|c| c.index)
        .collect();
    for &index in indices {
        if completed.contains(&index) {
            if let Some(content) = checkpoint
                .chapters
                .iter()
                .find(|c| c.index == index)
                .and_then(|c| c.content.clone())
            {
                book.chapters[index].content = content;
                emit_progress(
                    progress,
                    ProgressEvent::ChapterStarted {
                        index,
                        href: book.chapters[index].href.clone(),
                    },
                );
                emit_progress(
                    progress,
                    ProgressEvent::ChapterFinished {
                        index,
                        href: book.chapters[index].href.clone(),
                    },
                );
            }
        }
    }
    completed
}

async fn update_checkpoint_entry(
    checkpoint: &Arc<tokio::sync::Mutex<Checkpoint>>,
    index: usize,
    result: &Result<Vec<u8>, BabelEbookError>,
    store: Option<&CheckpointStore>,
    job_id: &str,
) {
    let cp_to_save = {
        let mut cp = checkpoint.lock().await;
        if let Some(entry) = cp.chapters.iter_mut().find(|c| c.index == index) {
            match result {
                Ok(content) => {
                    entry.status = ChapterStatus::Completed;
                    entry.content = Some(content.clone());
                    entry.error = None;
                }
                Err(err) => {
                    entry.status = ChapterStatus::Failed;
                    entry.error = Some(err.to_string());
                }
            }
        }
        cp.clone()
    };

    if let Some(store) = store {
        if let Err(err) = store.save_async(&cp_to_save).await {
            tracing::warn!(job_id, error = %err, "failed to save checkpoint");
        }
    }
}

async fn acquire_permit(
    semaphore: Arc<Semaphore>,
) -> Result<OwnedSemaphorePermit, BabelEbookError> {
    semaphore
        .acquire_owned()
        .await
        .map_err(|err| BabelEbookError::Anyhow(anyhow::anyhow!("semaphore closed: {err}")))
}

fn emit_progress(progress: Option<&dyn ProgressCallback>, event: ProgressEvent) {
    if let Some(callback) = progress {
        callback.on_progress(event);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::config::{Config, OutputMode, PromptTemplates, TranslationScope, TranslationStyle};
    use crate::epub::{Chapter, EpubBook, EpubMetadata};
    use crate::translator::{TranslateContext, Translator};
    use async_trait::async_trait;

    struct DummyTranslator;

    #[async_trait]
    impl Translator for DummyTranslator {
        fn name(&self) -> String {
            "dummy".into()
        }

        fn max_output_tokens(&self) -> usize {
            1000
        }

        async fn translate(
            &self,
            text: &str,
            _ctx: &TranslateContext<'_>,
        ) -> Result<String, BabelEbookError> {
            Ok(format!("[{}]", text.trim()))
        }
    }

    fn make_book(contents: Vec<&str>) -> EpubBook {
        EpubBook {
            metadata: EpubMetadata::default(),
            chapters: contents
                .into_iter()
                .enumerate()
                .map(|(i, c)| Chapter {
                    href: format!("ch{i:02}.xhtml"),
                    title: None,
                    content: format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?><html xmlns="http://www.w3.org/1999/xhtml"><body><p>{c}</p></body></html>"#
                    )
                    .into_bytes(),
                })
                .collect(),
            resources: vec![],
        }
    }

    fn make_config() -> Config {
        Config {
            source: PathBuf::default(),
            output: PathBuf::default(),
            provider: "dummy".into(),
            api_key: None,
            base_url: None,
            model: "dummy".into(),
            concurrency: 2,
            max_input_tokens: 4000,
            max_output_tokens: 2000,
            cache_dir: std::env::temp_dir().join(format!("test-cache-{}", std::process::id())),
            checkpoint_dir: std::env::temp_dir()
                .join(format!("test-checkpoint-{}", std::process::id())),
            resume_job_id: None,
            temperature: 0.3,
            source_lang: "en".into(),
            target_lang: "zh-CN".into(),
            skip_doc_patterns: vec![],
            translate_tags: vec!["p".into()],
            system_prompt: None,
            dry_run: false,
            verbose: false,
            provider_config: None,
            providers: HashMap::default(),
            output_mode: OutputMode::TranslationOnly,
            translation_scope: TranslationScope::default(),
            style: TranslationStyle::default(),
            chapter_prompts: HashMap::default(),
            prompts: PromptTemplates::default(),
            glossary: vec![],
            exclude_selectors: vec![],
            translate_attributes: vec![],
            preserve_classes: false,
            output_font: None,
            refine: false,
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn fast_completion_and_failure_are_visible_before_slow_chapter_finishes() {
        struct GatedTranslator(Arc<tokio::sync::Notify>);
        #[async_trait]
        impl Translator for GatedTranslator {
            fn name(&self) -> String {
                "r02-gated".into()
            }
            fn max_output_tokens(&self) -> usize {
                3000
            }
            async fn translate(
                &self,
                text: &str,
                _: &TranslateContext<'_>,
            ) -> Result<String, BabelEbookError> {
                if text == "alpha" {
                    self.0.notified().await;
                }
                if text == "charlie" {
                    return Err(BabelEbookError::ApiError("mock failure".into()));
                }
                Ok(format!("[{text}]"))
            }
        }
        struct LiveCallback {
            release: Arc<tokio::sync::Notify>,
            terminals: AtomicUsize,
            events: std::sync::Mutex<Vec<ProgressEvent>>,
        }
        impl ProgressCallback for LiveCallback {
            fn on_progress(&self, event: ProgressEvent) {
                let fast = matches!(
                    event,
                    ProgressEvent::ChapterFinished { index: 1, .. }
                        | ProgressEvent::Failed { index: 2, .. }
                );
                self.events.lock().unwrap().push(event);
                if fast && self.terminals.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                    self.release.notify_one();
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut config = make_config();
        config.concurrency = 3;
        let cache = TranslationCache::new(dir.path().join("cache"));
        let store = CheckpointStore::new(dir.path().join("checkpoints")).unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let translator = GatedTranslator(release.clone());
        let callback = LiveCallback {
            release,
            terminals: AtomicUsize::new(0),
            events: std::sync::Mutex::new(Vec::new()),
        };
        let context = PipelineContext {
            translator: &translator,
            config: &config,
            cache: &cache,
            progress: Some(&callback),
            cancellation: None,
        };
        let mut book = make_book(vec!["alpha", "bravo", "charlie"]);
        let failed_original = book.chapters[2].content.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            run_ordered_pipeline(
                &mut book,
                vec![0, 1, 2],
                &context,
                Some(&store),
                Some("live"),
                "hash",
            ),
        )
        .await
        .expect("terminal events must release the slow chapter before all futures end")
        .unwrap();
        assert_eq!(result.failures.len(), 1);
        let events = callback.events.into_inner().unwrap();
        let slow_end = events
            .iter()
            .position(|e| matches!(e, ProgressEvent::ChapterFinished { index: 0, .. }))
            .unwrap();
        assert!(
            events
                .iter()
                .position(|e| matches!(e, ProgressEvent::ChapterFinished { index: 1, .. }))
                .unwrap()
                < slow_end
        );
        assert!(
            events
                .iter()
                .position(|e| matches!(e, ProgressEvent::Failed { index: 2, .. }))
                .unwrap()
                < slow_end
        );
        assert!(String::from_utf8_lossy(&book.chapters[0].content).contains("[alpha]"));
        assert!(String::from_utf8_lossy(&book.chapters[1].content).contains("[bravo]"));
        assert_eq!(book.chapters[2].content, failed_original);
        let checkpoint = store.load("live").unwrap();
        assert_eq!(
            checkpoint
                .chapters
                .iter()
                .filter(|c| c.status == ChapterStatus::Completed)
                .count(),
            2
        );
        assert_eq!(checkpoint.chapters[2].status, ChapterStatus::Failed);
    }

    #[tokio::test]
    async fn r01_resume_reuses_twenty_seven_completed_chapters_and_only_requests_failed_one() {
        struct ResumeTranslator(AtomicUsize);
        #[async_trait]
        impl Translator for ResumeTranslator {
            fn name(&self) -> String {
                "r02-resume".into()
            }
            fn max_output_tokens(&self) -> usize {
                3000
            }
            async fn translate(
                &self,
                text: &str,
                _: &TranslateContext<'_>,
            ) -> Result<String, BabelEbookError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(format!("[{text}]"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(dir.path().join("checkpoints")).unwrap();
        let config = make_config();
        let mut book = make_book(vec!["remaining chapter"; 28]);
        let signature = CheckpointStore::translation_signature(&config);
        let chapters: Vec<_> = (0..28)
            .map(|index| ChapterCheckpoint {
                index,
                href: book.chapters[index].href.clone(),
                status: if index == 15 {
                    ChapterStatus::Failed
                } else {
                    ChapterStatus::Completed
                },
                content: if index == 15 {
                    None
                } else {
                    Some(format!("<p>R01 completed {index}</p>").into_bytes())
                },
                error: if index == 15 {
                    Some("old truncation".into())
                } else {
                    None
                },
            })
            .collect();
        let original = chapters.clone();
        store
            .save(&Checkpoint {
                job_id: "resume-27".into(),
                source_hash: "hash".into(),
                translation_signature: signature.clone(),
                source_path: String::new(),
                chapters,
            })
            .unwrap();
        let translator = ResumeTranslator(AtomicUsize::new(0));
        let cache = TranslationCache::new(dir.path().join("cache"));
        let context = PipelineContext {
            translator: &translator,
            config: &config,
            cache: &cache,
            progress: None,
            cancellation: None,
        };
        let result = run_ordered_pipeline(
            &mut book,
            (0..28).collect(),
            &context,
            Some(&store),
            Some("resume-27"),
            "hash",
        )
        .await
        .unwrap();
        assert!(result.failures.is_empty());
        assert_eq!(translator.0.load(Ordering::SeqCst), 1);
        for entry in original.iter().filter(|entry| entry.index != 15) {
            assert_eq!(
                book.chapters[entry.index].content,
                *entry.content.as_ref().unwrap()
            );
        }
        let checkpoint = store.load("resume-27").unwrap();
        assert_eq!(checkpoint.translation_signature, signature);
        assert!(checkpoint
            .chapters
            .iter()
            .all(|entry| entry.status == ChapterStatus::Completed));
    }

    #[tokio::test]
    async fn pipeline_skips_completed_chapters() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(dir.path().to_path_buf()).unwrap();
        let mut book = make_book(vec!["alpha", "beta", "gamma"]);
        let mut config = make_config();
        config.checkpoint_dir = dir.path().join("checkpoints");
        let job_id = CheckpointStore::generate_job_id(
            &config.source,
            &config.target_lang,
            config.output_mode,
            &config.provider,
            &config.model,
        );
        // Pre-populate checkpoint: chapter 0 completed.
        store
            .save(&Checkpoint {
                job_id: job_id.clone(),
                source_hash: "hash".into(),
                translation_signature: CheckpointStore::translation_signature(&config),
                source_path: config.source.to_string_lossy().into_owned(),
                chapters: vec![
                    ChapterCheckpoint {
                        index: 0,
                        href: "ch00.xhtml".into(),
                        status: ChapterStatus::Completed,
                        content: Some(b"<p>DONE</p>".to_vec()),
                        error: None,
                    },
                    ChapterCheckpoint {
                        index: 1,
                        href: "ch01.xhtml".into(),
                        status: ChapterStatus::Pending,
                        content: None,
                        error: None,
                    },
                    ChapterCheckpoint {
                        index: 2,
                        href: "ch02.xhtml".into(),
                        status: ChapterStatus::Pending,
                        content: None,
                        error: None,
                    },
                ],
            })
            .unwrap();

        let cache = TranslationCache::new(config.cache_dir.clone());
        let context = PipelineContext {
            translator: &DummyTranslator,
            config: &config,
            cache: &cache,
            progress: None,
            cancellation: None,
        };
        let result = run_ordered_pipeline(
            &mut book,
            vec![0, 1, 2],
            &context,
            Some(&store),
            Some(&job_id),
            "hash",
        )
        .await
        .unwrap();
        assert!(result.failures.is_empty());
        assert!(String::from_utf8_lossy(&book.chapters[0].content).contains("DONE"));
        assert!(String::from_utf8_lossy(&book.chapters[1].content).contains("[beta]"));
        assert!(String::from_utf8_lossy(&book.chapters[2].content).contains("[gamma]"));
    }

    #[tokio::test]
    async fn pipeline_stops_before_scheduling_when_cancelled() {
        let mut book = make_book(vec!["alpha", "beta"]);
        let config = make_config();
        let cache = TranslationCache::new(config.cache_dir.clone());
        let cancellation = CancellationToken::default();
        cancellation.cancel();

        let context = PipelineContext {
            translator: &DummyTranslator,
            config: &config,
            cache: &cache,
            progress: None,
            cancellation: Some(&cancellation),
        };
        let result = run_ordered_pipeline(&mut book, vec![0, 1], &context, None, None, "").await;
        let Err(err) = result else {
            panic!("expected cancelled pipeline");
        };

        assert!(matches!(err, BabelEbookError::Cancelled));
    }

    #[test]
    fn pipeline_preserves_order() {
        let handle = thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread runtime");

            rt.block_on(async {
                let mut book = make_book(vec!["alpha", "bravo", "charlie"]);
                let config = make_config();
                let cache = TranslationCache::new(config.cache_dir.clone());
                let context = PipelineContext {
                    translator: &DummyTranslator,
                    config: &config,
                    cache: &cache,
                    progress: None,
                    cancellation: None,
                };
                let result =
                    run_ordered_pipeline(&mut book, vec![0, 1, 2], &context, None, None, "")
                        .await
                        .unwrap();
                assert!(result.failures.is_empty());

                let texts: Vec<String> = book
                    .chapters
                    .iter()
                    .map(|c| String::from_utf8_lossy(&c.content).to_string())
                    .collect();
                assert!(texts[0].contains("[alpha]"));
                assert!(texts[1].contains("[bravo]"));
                assert!(texts[2].contains("[charlie]"));
            });
        });
        handle.join().expect("test thread panicked");
    }

    #[tokio::test]
    async fn concurrency_three_runs_three_requests_and_never_exceeds_the_limit() {
        struct BarrierTranslator {
            active: AtomicUsize,
            max_active: AtomicUsize,
            calls: AtomicUsize,
            barrier: tokio::sync::Barrier,
        }
        #[async_trait]
        impl Translator for BarrierTranslator {
            fn name(&self) -> String {
                "r02-concurrency-three".into()
            }
            fn max_output_tokens(&self) -> usize {
                3000
            }
            async fn translate(
                &self,
                text: &str,
                _: &TranslateContext<'_>,
            ) -> Result<String, BabelEbookError> {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(active, Ordering::SeqCst);
                self.calls.fetch_add(1, Ordering::SeqCst);
                // Each batch can finish only after three requests have entered.
                // Sequential scheduling therefore times out rather than passing.
                self.barrier.wait().await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(format!("[{text}]"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut config = make_config();
        config.concurrency = 3;
        let translator = BarrierTranslator {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            barrier: tokio::sync::Barrier::new(3),
        };
        let cache = TranslationCache::new(dir.path().join("cache"));
        let context = PipelineContext {
            translator: &translator,
            config: &config,
            cache: &cache,
            progress: None,
            cancellation: None,
        };
        let texts = vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
        let mut book = make_book(texts.clone());
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            run_ordered_pipeline(&mut book, (0..6).collect(), &context, None, None, ""),
        )
        .await
        .expect("three concurrent requests must reach the barrier in both batches")
        .unwrap();
        assert!(result.failures.is_empty());
        assert_eq!(translator.calls.load(Ordering::SeqCst), 6);
        assert_eq!(translator.max_active.load(Ordering::SeqCst), 3);
        assert_eq!(translator.active.load(Ordering::SeqCst), 0);
        for (chapter, text) in book.chapters.iter().zip(texts) {
            assert!(String::from_utf8_lossy(&chapter.content).contains(&format!("[{text}]")));
        }
    }

    #[test]
    fn pipeline_respects_concurrency() {
        struct CountingTranslator {
            active: Arc<AtomicUsize>,
            max_active: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl Translator for CountingTranslator {
            fn name(&self) -> String {
                "counting".into()
            }

            fn max_output_tokens(&self) -> usize {
                1000
            }

            async fn translate(
                &self,
                text: &str,
                _ctx: &TranslateContext<'_>,
            ) -> Result<String, BabelEbookError> {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                let mut max = self.max_active.load(Ordering::SeqCst);
                while max < active {
                    match self.max_active.compare_exchange_weak(
                        max,
                        active,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(current) => max = current,
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(format!("[{}]", text.trim()))
            }
        }

        let handle = thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread runtime");

            rt.block_on(async {
                let mut book = make_book(vec!["alpha", "bravo", "charlie", "delta", "echo"]);
                let mut config = make_config();
                config.concurrency = 2;
                let active = Arc::new(AtomicUsize::new(0));
                let max_active = Arc::new(AtomicUsize::new(0));
                let translator = CountingTranslator {
                    active: Arc::clone(&active),
                    max_active: Arc::clone(&max_active),
                };
                let cache = TranslationCache::new(config.cache_dir.clone());
                let context = PipelineContext {
                    translator: &translator,
                    config: &config,
                    cache: &cache,
                    progress: None,
                    cancellation: None,
                };
                let result =
                    run_ordered_pipeline(&mut book, vec![0, 1, 2, 3, 4], &context, None, None, "")
                        .await
                        .unwrap();
                assert!(result.failures.is_empty());
                // The exact observed concurrency depends on the Tokio scheduler; the
                // important invariant is that it never exceeds the configured limit.
                assert!(
                    max_active.load(Ordering::SeqCst) <= 2,
                    "concurrency exceeded limit: {}",
                    max_active.load(Ordering::SeqCst)
                );
            });
        });
        handle.join().expect("test thread panicked");
    }

    #[tokio::test]
    async fn concurrent_chapters_complete_without_deadlock() {
        struct SlowTranslator;

        #[async_trait]
        impl Translator for SlowTranslator {
            fn name(&self) -> String {
                "slow".into()
            }

            fn max_output_tokens(&self) -> usize {
                1000
            }

            async fn translate(
                &self,
                text: &str,
                _ctx: &TranslateContext<'_>,
            ) -> Result<String, BabelEbookError> {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(format!("[{}]", text.trim()))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(dir.path().to_path_buf()).unwrap();
        let mut book = make_book(vec!["alpha", "bravo", "charlie", "delta", "echo"]);
        let mut config = make_config();
        config.concurrency = 3;
        config.checkpoint_dir = dir.path().join("checkpoints");
        let cache = TranslationCache::new(config.cache_dir.clone());

        let context = PipelineContext {
            translator: &SlowTranslator,
            config: &config,
            cache: &cache,
            progress: None,
            cancellation: None,
        };
        let result = run_ordered_pipeline(
            &mut book,
            vec![0, 1, 2, 3, 4],
            &context,
            Some(&store),
            None,
            "hash",
        )
        .await
        .unwrap();

        assert!(result.failures.is_empty());
        let expected = ["alpha", "bravo", "charlie", "delta", "echo"];
        for (i, chapter) in book.chapters.iter().enumerate() {
            let text = String::from_utf8_lossy(&chapter.content);
            assert!(text.contains(&format!("[{}]", expected[i])));
        }
    }

    #[tokio::test]
    async fn checkpoint_file_updated_after_chapters_complete() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(dir.path().to_path_buf()).unwrap();
        let mut book = make_book(vec!["alpha", "beta", "gamma"]);
        let mut config = make_config();
        config.checkpoint_dir = dir.path().join("checkpoints");
        let cache = TranslationCache::new(config.cache_dir.clone());

        let context = PipelineContext {
            translator: &DummyTranslator,
            config: &config,
            cache: &cache,
            progress: None,
            cancellation: None,
        };
        let result = run_ordered_pipeline(
            &mut book,
            vec![0, 1, 2],
            &context,
            Some(&store),
            None,
            "hash",
        )
        .await
        .unwrap();

        assert!(result.failures.is_empty());

        let job_id = resolve_job_id(Some(&store), None, &config);
        let checkpoint = store
            .load(&job_id)
            .expect("checkpoint should exist on disk");
        assert_eq!(checkpoint.chapters.len(), 3);
        for entry in &checkpoint.chapters {
            assert_eq!(entry.status, ChapterStatus::Completed);
            assert!(entry.content.is_some());
        }
    }
}
