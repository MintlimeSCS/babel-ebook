//! Low-level text translation with caching and optional refinement.

use crate::cache::TranslationCache;
use crate::chunking::{count_tokens, split_text_chunks};
use crate::config::TranslationOptions;
use crate::core::{BabelEbookError, CancellationToken, ProgressCallback};
use crate::translator::{TranslateContext, Translator};
use sha2::{Digest, Sha256};

use super::markup::{marker_regex, validate_markers};
use super::progress::emit_chunk_progress;

fn cache_scope(
    translator: &dyn Translator,
    options: &TranslationOptions,
    prompt: &str,
    phase: &str,
) -> String {
    // v2 deliberately ignores legacy cache files, which were not language/prompt scoped.
    // Only translation settings are hashed; credentials never enter cache keys or files.
    let settings = serde_json::json!([
        translator.cache_identity(),
        options.source_lang,
        options.target_lang,
        prompt,
        options.max_input_tokens,
        options.max_output_tokens,
        options.temperature,
        translator.max_output_tokens(),
        phase
    ]);
    format!(
        "translation-v2-{}",
        hex::encode(Sha256::digest(settings.to_string().as_bytes()))
    )
}

pub(super) fn split_preserving_markers(text: &str, limit: usize) -> Vec<String> {
    if count_tokens(text) <= limit {
        return vec![text.to_string()];
    }
    if !marker_regex().is_match(text) {
        return split_text_chunks(text, limit);
    }
    let mut pieces = Vec::new();
    let mut end = 0;
    for token in marker_regex().find_iter(text) {
        pieces.extend(split_text_chunks(&text[end..token.start()], limit));
        pieces.push(token.as_str().to_string());
        end = token.end();
    }
    pieces.extend(split_text_chunks(&text[end..], limit));
    let mut chunks = Vec::new();
    let mut current = String::new();
    for piece in pieces {
        if piece.is_empty() {
            continue;
        }
        if !current.is_empty() && count_tokens(&format!("{current} {piece}")) > limit {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&piece);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Translate `text`, using caching and chunking as needed.
///
/// Scopes cache entries to effective request settings, protects formatting
/// markers across chunks and retains line breaks returned by the translator.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub async fn translate_text(
    text: &str,
    translator: &dyn Translator,
    options: &TranslationOptions,
    cache: &TranslationCache,
    chapter_index: usize,
    chapter_href: &str,
    progress: Option<&dyn ProgressCallback>,
    cancellation: Option<&CancellationToken>,
) -> Result<String, BabelEbookError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(BabelEbookError::Cancelled);
    }
    let system_prompt = options.system_prompt_for_chapter(chapter_href);
    let translate_name = cache_scope(translator, options, &system_prompt, "translate");
    // First pass. When refinement is disabled we can return a cached full-text
    // result immediately; otherwise the cached translation still needs to be
    // polished.
    let first_pass = if let Some(cached) = cache.get_async(&translate_name, text).await {
        if !options.refine {
            return Ok(cached);
        }
        cached
    } else {
        let max_source = options.max_source_tokens_for_prompt(&system_prompt);
        let target_lang = &options.target_lang;

        let chunks = split_preserving_markers(text, max_source);
        let chunk_total = chunks.len();
        let mut translated_parts = Vec::with_capacity(chunk_total);
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(BabelEbookError::Cancelled);
            }
            emit_chunk_progress(
                progress,
                chapter_index,
                chapter_href,
                chunk_index,
                chunk_total,
                false,
            );
            if let Some(cached) = cache.get_async(&translate_name, chunk).await {
                translated_parts.push(cached);
                emit_chunk_progress(
                    progress,
                    chapter_index,
                    chapter_href,
                    chunk_index,
                    chunk_total,
                    true,
                );
                continue;
            }

            let context = TranslateContext {
                system_prompt: &system_prompt,
                target_lang,
            };
            let result = translator.translate(chunk, &context).await?;
            validate_markers(chunk, &result)?;
            let tokens = count_tokens(chunk) + count_tokens(&result);
            cache
                .put_async(&translate_name, chunk, &result, Some(tokens))
                .await;
            translated_parts.push(result);
            emit_chunk_progress(
                progress,
                chapter_index,
                chapter_href,
                chunk_index,
                chunk_total,
                true,
            );
        }

        let result = translated_parts.join(" ").trim().to_string();
        validate_markers(text, &result)?;
        cache.put_async(&translate_name, text, &result, None).await;
        result
    };

    if !options.refine {
        return Ok(first_pass);
    }

    // Optional second-pass refinement using a separate cache namespace. Refine
    // progress is not reported as chunks because the number of refine chunks is
    // not known until the first pass completes; keeping chapter progress tied to
    // the first-pass source chunks gives a stable, monotonically increasing bar.
    let max_refine_source = options.max_refine_source_tokens();
    let refine_prompt = options.refine_prompt();
    let target_lang = &options.target_lang;
    let refine_name = cache_scope(translator, options, &refine_prompt, "refine");

    let chunks = split_preserving_markers(&first_pass, max_refine_source);
    let mut refined_parts = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(BabelEbookError::Cancelled);
        }
        if let Some(cached) = cache.get_async(&refine_name, chunk).await {
            refined_parts.push(cached);
            continue;
        }

        let context = TranslateContext {
            system_prompt: &refine_prompt,
            target_lang,
        };
        let result = translator.translate(chunk, &context).await?;
        validate_markers(chunk, &result)?;
        let tokens = count_tokens(chunk) + count_tokens(&result);
        cache
            .put_async(&refine_name, chunk, &result, Some(tokens))
            .await;
        refined_parts.push(result);
    }

    let refined = refined_parts.join(" ").trim().to_string();
    validate_markers(&first_pass, &refined)?;
    Ok(refined)
}
