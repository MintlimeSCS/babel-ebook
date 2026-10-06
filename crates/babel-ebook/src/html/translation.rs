//! Low-level text translation with caching and optional refinement.

use crate::cache::TranslationCache;
use crate::chunking::{count_tokens, split_text_chunks};
use crate::config::TranslationOptions;
use crate::core::{BabelEbookError, CancellationToken, ProgressCallback};
use crate::translator::{TranslateContext, Translator};
use sha2::{Digest, Sha256};

use super::markup::{marker_regex, validate_markers, MARKUP_PROMPT};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub(super) struct FormattingState {
    structured: AtomicBool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentResponse {
    translations: Vec<String>,
}

fn decode_fragments(
    response: &str,
    wrapped: bool,
    plain: bool,
) -> Result<Vec<String>, BabelEbookError> {
    let response = response.trim();
    if plain {
        return Ok(vec![response.to_string()]);
    }
    // Accept a surrounding code fence without relaxing structural validation.
    let json = response.strip_prefix("```").map_or(response, |fenced| {
        fenced
            .split_once('\n')
            .and_then(|(_, body)| body.trim().strip_suffix("```"))
            .unwrap_or(response)
            .trim()
    });
    if wrapped {
        serde_json::from_str::<FragmentResponse>(json).map(|r| r.translations)
    } else {
        serde_json::from_str(json)
    }
    .map_err(|_| BabelEbookError::ApiError("Formatting response is invalid JSON for the required fragment contract; no further repair requests were sent".into()))
}
use super::progress::emit_chunk_progress;

async fn cancellable_translate(
    text: &str,
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    cancellation: Option<&CancellationToken>,
) -> Result<String, BabelEbookError> {
    if let Some(token) = cancellation {
        tokio::select! {
            biased;
            () = token.cancelled() => Err(BabelEbookError::Cancelled),
            result = translator.translate(text, context) => result,
        }
    } else {
        translator.translate(text, context).await
    }
}

/// Switch a chapter to one-call structured translation after its first marker
/// failure. This adds at most one repair request per chapter, never one request
/// per text run; subsequent marked paragraphs use structured translation first.
async fn translate_guarded(
    chunk: &str,
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    max_input_tokens: usize,
    cancellation: Option<&CancellationToken>,
    formatting: &FormattingState,
) -> Result<String, BabelEbookError> {
    if !marker_regex()
        .replace_all(chunk, "")
        .chars()
        .any(char::is_alphabetic)
    {
        return Ok(chunk.to_string());
    }
    let has_markers = marker_regex().is_match(chunk);
    if has_markers && translator.fragment_response_format(1).is_some() {
        // Native strict output avoids even the first failed marker request.
        return translate_structured(chunk, translator, context, max_input_tokens, cancellation)
            .await;
    }
    if !has_markers || !formatting.structured.load(Ordering::Relaxed) {
        let result = cancellable_translate(chunk, translator, context, cancellation).await?;
        if validate_markers(chunk, &result).is_ok() {
            return Ok(result);
        }
        if !has_markers {
            validate_markers(chunk, &result)?;
        }
        formatting.structured.store(true, Ordering::Relaxed);
        tracing::warn!("Model changed inline markers; using one structured request per paragraph for the rest of this chapter");
    }
    translate_structured(chunk, translator, context, max_input_tokens, cancellation).await
}

async fn translate_structured(
    chunk: &str,
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    max_input_tokens: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<String, BabelEbookError> {
    // Split locally, but send ALL meaningful text runs together in one request.
    // Markers, numeric-only runs, whitespace, code and images never reach the model.
    let mut runs = Vec::new();
    let mut markers = Vec::new();
    let mut end = 0;
    for marker in marker_regex().find_iter(chunk) {
        runs.push(&chunk[end..marker.start()]);
        markers.push(marker.as_str());
        end = marker.end();
    }
    runs.push(&chunk[end..]);
    let meaningful: Vec<_> = runs
        .iter()
        .filter(|text| text.chars().any(char::is_alphabetic))
        .map(|text| text.trim())
        .collect();
    if meaningful.is_empty() {
        return Ok(chunk.to_string());
    }
    let mut format = translator.fragment_response_format(meaningful.len());
    let plain_fragment = format.is_some() && meaningful.len() == 1;
    // A single text run needs no JSON or schema at all: translate its text
    // normally and restore surrounding elements locally, saving input tokens.
    if plain_fragment {
        format = None;
    }
    let payload = if plain_fragment {
        meaningful[0].to_string()
    } else {
        serde_json::to_string(&meaningful).expect("text runs serialize")
    };
    let output_rule = if format.is_some() {
        "Return ONLY a JSON object with a translations array."
    } else {
        "Return ONLY a JSON array."
    };
    let base_prompt = context.system_prompt.replace(MARKUP_PROMPT, "");
    let prompt = if plain_fragment {
        base_prompt
    } else {
        format!("{base_prompt}\nInput is an ordered JSON array of text fragments from one paragraph. Translate each fragment in context, including short fragments. {output_rule} Keep exactly {} nonempty strings in source order. Do not merge, omit or repeat text, or add commentary or code fences. Preserve fragment boundaries.", meaningful.len())
    };
    if count_tokens(&prompt)
        .saturating_add(count_tokens(&payload))
        .saturating_add(format.as_ref().map_or(0, |f| count_tokens(&f.to_string())))
        .saturating_add(32)
        > max_input_tokens
    {
        return Err(BabelEbookError::ApiError("Protected formatting repair exceeds the configured input-token budget; no extra request was sent".into()));
    }
    let repair_context = TranslateContext {
        system_prompt: &prompt,
        target_lang: context.target_lang,
    };
    let request = translator.translate_fragments(&payload, &repair_context, meaningful.len());
    let response = if plain_fragment {
        cancellable_translate(&payload, translator, &repair_context, cancellation).await?
    } else if let Some(token) = cancellation {
        tokio::select! {
            biased;
            () = token.cancelled() => return Err(BabelEbookError::Cancelled),
            result = request => result?,
        }
    } else {
        request.await?
    };
    let translated = decode_fragments(&response, format.is_some(), plain_fragment)?;
    if translated.len() != meaningful.len() {
        return Err(BabelEbookError::ApiError(format!("Formatting response returned {} fragments; expected {}. No further repair requests were sent", translated.len(), meaningful.len())));
    }
    if let Some(index) = translated.iter().position(|t| t.trim().is_empty()) {
        return Err(BabelEbookError::ApiError(format!("Formatting response returned an empty fragment at position {} of {}. No further repair requests were sent", index + 1, meaningful.len())));
    }
    let mut translated = translated.iter();
    let mut output = String::new();
    for (index, run) in runs.iter().enumerate() {
        if run.chars().any(char::is_alphabetic) {
            let text = translated.next().expect("validated run count").trim();
            validate_markers("", text)?;
            let start = run.len() - run.trim_start().len();
            let end = run.trim_end().len();
            output.push_str(&run[..start]);
            output.push_str(text);
            output.push_str(&run[end..]);
        } else {
            output.push_str(run);
        }
        if let Some(marker) = markers.get(index) {
            output.push_str(marker);
        }
    }
    validate_markers(chunk, &output)?;
    Ok(output)
}

fn cache_scope(
    translator: &dyn Translator,
    options: &TranslationOptions,
    prompt: &str,
    phase: &str,
) -> String {
    // v2 excludes legacy caches that were not scoped to language/prompt settings.
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
    translate_text_with_state(
        text,
        translator,
        options,
        cache,
        chapter_index,
        chapter_href,
        progress,
        cancellation,
        &FormattingState::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn translate_text_with_state(
    text: &str,
    translator: &dyn Translator,
    options: &TranslationOptions,
    cache: &TranslationCache,
    chapter_index: usize,
    chapter_href: &str,
    progress: Option<&dyn ProgressCallback>,
    cancellation: Option<&CancellationToken>,
    formatting: &FormattingState,
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
            let result = translate_guarded(
                chunk,
                translator,
                &context,
                options.max_input_tokens,
                cancellation,
                formatting,
            )
            .await?;
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
        let result = translate_guarded(
            chunk,
            translator,
            &context,
            options.max_input_tokens,
            cancellation,
            formatting,
        )
        .await?;
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
