//! Low-level text translation with caching and optional refinement.

use crate::cache::TranslationCache;
use crate::chunking::{count_tokens, split_text_chunks};
use crate::config::TranslationOptions;
use crate::core::{BabelEbookError, CancellationToken, ProgressCallback};
use crate::translator::{TranslateContext, Translator};
use sha2::{Digest, Sha256};

use super::fragments::{self, RequestBudget};
use super::markup::{marker_regex, validate_markers};

#[derive(Default)]
pub(super) struct FormattingState {
    pub chapter_href: String,
    pub element_index: Option<usize>,
}

use super::progress::emit_chunk_progress;

pub(super) async fn cancellable_translate(
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

/// Keep protected formatting local for every provider. A marked paragraph is
/// translated as text fragments with a shared, bounded recovery request budget.
#[allow(clippy::too_many_arguments)]
async fn translate_guarded(
    chunk: &str,
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    max_input_tokens: usize,
    cancellation: Option<&CancellationToken>,
    formatting: &FormattingState,
    cache: &TranslationCache,
    scope: &str,
    budget: &mut RequestBudget,
) -> Result<String, BabelEbookError> {
    if !marker_regex()
        .replace_all(chunk, "")
        .chars()
        .any(char::is_alphabetic)
    {
        return Ok(chunk.to_string());
    }
    if marker_regex().is_match(chunk) {
        return translate_structured(
            chunk,
            translator,
            context,
            max_input_tokens,
            cancellation,
            formatting,
            cache,
            scope,
            budget,
        )
        .await;
    }
    budget.take()?;
    let result = cancellable_translate(chunk, translator, context, cancellation).await?;
    validate_markers(chunk, &result)?;
    if result.trim().is_empty() {
        return Err(BabelEbookError::ApiError(
            "Translation returned empty text; no successful cache was written".into(),
        ));
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn translate_structured(
    chunk: &str,
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    max_input_tokens: usize,
    cancellation: Option<&CancellationToken>,
    formatting: &FormattingState,
    cache: &TranslationCache,
    scope: &str,
    budget: &mut RequestBudget,
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
    let translated = fragments::translate(
        &meaningful,
        translator,
        context,
        max_input_tokens,
        cancellation,
        budget,
        cache,
        scope,
        formatting,
    )
    .await?;
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

const MAX_SPLIT_DEPTH: usize = 3;
const MAX_RECOVERY_REQUESTS: usize = fragments::MAX_REQUESTS;
const MIN_SPLIT_TOKENS: usize = 32;

/// Choose an exact UTF-8 boundary near the middle, preferring sentence ends
/// and then whitespace.
/// Neither a protected marker nor a character may be cut in half. Both sides
/// must contain translatable text; their concatenation is exactly the source.
fn recovery_split(source: &str) -> Option<(String, String)> {
    if count_tokens(source) < MIN_SPLIT_TOKENS {
        return None;
    }
    let markers: Vec<_> = marker_regex().find_iter(source).collect();
    let middle = source.len() / 2;
    let valid = |position: usize| {
        position > 0
            && position < source.len()
            && position >= source.len() / 4
            && position <= source.len() * 3 / 4
            && !markers
                .iter()
                .any(|m| m.start() < position && position < m.end())
            && marker_regex()
                .replace_all(&source[..position], "")
                .chars()
                .any(char::is_alphabetic)
            && marker_regex()
                .replace_all(&source[position..], "")
                .chars()
                .any(char::is_alphabetic)
    };
    let candidates: Vec<_> = source
        .char_indices()
        .filter(|(position, _)| valid(*position))
        .collect();
    let position = candidates
        .iter()
        .filter(|(position, character)| {
            character.is_whitespace()
                && source[..*position]
                    .chars()
                    .next_back()
                    .is_some_and(|c| matches!(c, '.' | '!' | '?' | '。' | '！' | '？'))
        })
        .min_by_key(|(position, _)| position.abs_diff(middle))
        .or_else(|| {
            candidates
                .iter()
                .filter(|(_, character)| character.is_whitespace())
                .min_by_key(|(position, _)| position.abs_diff(middle))
        })
        .or_else(|| {
            candidates
                .iter()
                .min_by_key(|(position, _)| position.abs_diff(middle))
        })?
        .0;
    Some((source[..position].into(), source[position..].into()))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn translate_with_recovery(
    source: &str,
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    max_input_tokens: usize,
    cancellation: Option<&CancellationToken>,
    formatting: &FormattingState,
    cache: &TranslationCache,
    scope: &str,
) -> Result<String, BabelEbookError> {
    let mut budget = RequestBudget::default();
    let mut last_truncation = match translate_guarded(
        source,
        translator,
        context,
        max_input_tokens,
        cancellation,
        formatting,
        cache,
        scope,
        &mut budget,
    )
    .await
    {
        Ok(output) => return Ok(output),
        Err(BabelEbookError::OutputTruncated(message)) => message,
        Err(error) => return Err(error),
    };
    let Some((left, right)) = recovery_split(source) else {
        return Err(BabelEbookError::OutputTruncated(format!(
            "{last_truncation}; source is too small to split safely"
        )));
    };
    let recovery_scope = format!("{scope}-adaptive-v1");
    let mut pending = vec![(right, 1), (left, 1)];
    let mut output = String::new();
    while let Some((piece, depth)) = pending.pop() {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(BabelEbookError::Cancelled);
        }
        let translated = if let Some(cached) = valid_cached(cache, &recovery_scope, &piece).await {
            cached
        } else {
            if budget.used >= MAX_RECOVERY_REQUESTS {
                return Err(BabelEbookError::OutputTruncated(format!(
                    "{last_truncation}; stopped after {} translation attempts (limit {MAX_RECOVERY_REQUESTS}){}", budget.used,
                    budget.last_format_failure.as_ref().map_or_else(String::new, |failure| format!("; last formatting failure: {failure}"))
                )));
            }
            tracing::warn!(attempt = budget.used + 1, depth, source_tokens = count_tokens(&piece),
                "Retrying a truncated translation with a smaller source piece; output limit unchanged");
            match translate_guarded(
                &piece,
                translator,
                context,
                max_input_tokens,
                cancellation,
                formatting,
                cache,
                scope,
                &mut budget,
            )
            .await
            {
                Ok(translated) => {
                    validate_markers(&piece, &translated)?;
                    // Subpieces use a separate scope. A partially recovered
                    // paragraph never becomes a successful full-paragraph entry.
                    cache
                        .put_async(&recovery_scope, &piece, &translated, None)
                        .await;
                    translated
                }
                Err(BabelEbookError::OutputTruncated(message)) => {
                    last_truncation = message;
                    if depth < MAX_SPLIT_DEPTH {
                        if let Some((left, right)) = recovery_split(&piece) {
                            pending.push((right, depth + 1));
                            pending.push((left, depth + 1));
                            continue;
                        }
                    }
                    return Err(BabelEbookError::OutputTruncated(format!(
                        "{last_truncation}; smaller-piece recovery stopped at depth {depth} after {} attempts", budget.used
                    )));
                }
                Err(error) => return Err(error),
            }
        };
        // Restore source boundary whitespace instead of inserting spaces at
        // arbitrary character splits (important for Chinese and inline links).
        let start = piece.len() - piece.trim_start().len();
        let end = piece.trim_end().len();
        output.push_str(&piece[..start]);
        output.push_str(translated.trim());
        output.push_str(&piece[end..]);
    }
    validate_markers(source, &output)?;
    Ok(output)
}

async fn valid_cached(cache: &TranslationCache, scope: &str, source: &str) -> Option<String> {
    let cached = cache.get_async(scope, source).await?;
    if cached.trim().is_empty() || validate_markers(source, &cached).is_err() {
        tracing::warn!(
            "Ignoring an invalid cached translation; other successful cache entries are retained"
        );
        return None;
    }
    Some(cached)
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
        &FormattingState {
            chapter_href: chapter_href.to_string(),
            element_index: None,
        },
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
    let first_pass = if let Some(cached) = valid_cached(cache, &translate_name, text).await {
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
            if let Some(cached) = valid_cached(cache, &translate_name, chunk).await {
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
            let result = translate_with_recovery(
                chunk,
                translator,
                &context,
                options.max_input_tokens,
                cancellation,
                formatting,
                cache,
                &translate_name,
            )
            .await
            .map_err(|error| match error {
                BabelEbookError::OutputTruncated(message) => {
                    BabelEbookError::OutputTruncated(format!(
                        "{chapter_href}, source chunk {} of {chunk_total}: {message}",
                        chunk_index + 1
                    ))
                }
                BabelEbookError::ApiError(message) => BabelEbookError::ApiError(format!(
                    "{chapter_href}, source chunk {} of {chunk_total}: {message}",
                    chunk_index + 1
                )),
                other => other,
            })?;
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
        if let Some(cached) = valid_cached(cache, &refine_name, chunk).await {
            refined_parts.push(cached);
            continue;
        }

        let context = TranslateContext {
            system_prompt: &refine_prompt,
            target_lang,
        };
        let result = translate_with_recovery(
            chunk,
            translator,
            &context,
            options.max_input_tokens,
            cancellation,
            formatting,
            cache,
            &refine_name,
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
