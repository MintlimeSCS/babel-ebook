//! Bounded recovery of ordered inline text fragments. Markup never leaves the app.

use std::fmt::Write;

use crate::cache::TranslationCache;
use crate::chunking::count_tokens;
use crate::core::{BabelEbookError, CancellationToken};
use crate::translator::{TranslateContext, Translator};

use super::markup::{validate_markers, MARKUP_PROMPT};
use super::translation::FormattingState;

pub(super) const MAX_REQUESTS: usize = 9;
const MAX_FRAGMENT_SPLIT_DEPTH: usize = 3;
const MAX_RESPONSE_CHARS: usize = 16_000;

#[derive(Default)]
pub(super) struct RequestBudget {
    pub used: usize,
    pub last_format_failure: Option<String>,
}

impl RequestBudget {
    pub fn take(&mut self) -> Result<(), BabelEbookError> {
        if self.used >= MAX_REQUESTS {
            return Err(BabelEbookError::ApiError(format!(
                "Recovery stopped after {} translation attempts (limit {MAX_REQUESTS})",
                self.used
            )));
        }
        self.used += 1;
        Ok(())
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentResponse {
    translations: Vec<String>,
}

fn decode(response: &str, wrapped: bool, plain: bool, count: usize) -> Result<Vec<String>, String> {
    let response = response.trim();
    let json = response.strip_prefix("```").map_or(response, |fenced| {
        fenced
            .split_once('\n')
            .and_then(|(_, body)| body.trim().strip_suffix("```"))
            .unwrap_or(response)
            .trim()
    });
    let translated: Vec<String> = if plain {
        vec![response.to_string()]
    } else if wrapped {
        serde_json::from_str::<FragmentResponse>(json)
            .map(|r| r.translations)
            .map_err(|_| {
                "Formatting response is invalid JSON for the required fragment contract".to_string()
            })?
    } else {
        serde_json::from_str(json).map_err(|_| {
            "Formatting response is invalid JSON for the required fragment contract".to_string()
        })?
    };
    if translated.len() != count {
        return Err(format!(
            "Formatting response returned {} fragments; expected {count}",
            translated.len()
        ));
    }
    if let Some(index) = translated.iter().position(|text| text.trim().is_empty()) {
        return Err(format!(
            "Formatting response returned an empty fragment at position {} of {count}",
            index + 1
        ));
    }
    for text in &translated {
        validate_markers("", text).map_err(|error| error.to_string())?;
    }
    Ok(translated)
}

struct Attempt {
    response: String,
    result: Result<Vec<String>, String>,
}

#[allow(clippy::too_many_arguments)]
async fn request(
    fragments: &[&str],
    paragraph_context: Option<&str>,
    retry: bool,
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    max_input_tokens: usize,
    cancellation: Option<&CancellationToken>,
    budget: &mut RequestBudget,
) -> Result<Attempt, BabelEbookError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(BabelEbookError::Cancelled);
    }
    let plain = fragments.len() == 1;
    let format = if plain {
        None
    } else {
        translator.fragment_response_format(fragments.len())
    };
    let payload = if plain {
        fragments[0].to_string()
    } else {
        serde_json::to_string(fragments).expect("fragments serialize")
    };
    let mut prompt = context.system_prompt.replace(MARKUP_PROMPT, "");
    if !plain {
        let shape = if format.is_some() {
            "a JSON object with a translations array"
        } else {
            "a JSON array"
        };
        write!(prompt,
            "\nFragment transport contract: This contract overrides prose-only output rules. Input is an ordered JSON array of text fragments from one paragraph. Return ONLY {shape} with exactly {} nonempty strings in source order. The strings contain only translated text. Translate short fragments in context. Never merge, omit, duplicate or add fragments, HTML, commentary or code fences. Preserve fragment boundaries.", fragments.len()
        ).expect("writing to a String cannot fail");
        if retry {
            prompt.push_str("\nThe previous response failed structural validation. Recheck the exact number of input fragments and return one nonempty translation per fragment.");
        }
    }
    if let Some(reference) = paragraph_context {
        prompt.push_str("\nParagraph context (reference only): ");
        prompt.push_str(reference);
        prompt.push_str("\nTranslate ONLY the supplied input fragment(s); do not repeat the surrounding context.");
    }
    if count_tokens(&prompt)
        .saturating_add(count_tokens(&payload))
        .saturating_add(format.as_ref().map_or(0, |f| count_tokens(&f.to_string())))
        .saturating_add(32)
        > max_input_tokens
    {
        return Err(BabelEbookError::ApiError("Protected formatting request exceeds the configured input-token budget; no extra request was sent".into()));
    }
    budget.take()?;
    let request_context = TranslateContext {
        system_prompt: &prompt,
        target_lang: context.target_lang,
    };
    let response = if plain {
        super::translation::cancellable_translate(
            &payload,
            translator,
            &request_context,
            cancellation,
        )
        .await?
    } else if let Some(token) = cancellation {
        tokio::select! {
            biased;
            () = token.cancelled() => return Err(BabelEbookError::Cancelled),
            result = translator.translate_fragments(&payload, &request_context, fragments.len()) => result?,
        }
    } else {
        translator
            .translate_fragments(&payload, &request_context, fragments.len())
            .await?
    };
    let result = decode(&response, format.is_some(), plain, fragments.len());
    Ok(Attempt { response, result })
}

/// Retry the original group once, then divide only at source fragment boundaries.
/// Successful groups are cached separately; an incomplete paragraph never succeeds.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn translate(
    fragments: &[&str],
    translator: &dyn Translator,
    context: &TranslateContext<'_>,
    max_input_tokens: usize,
    cancellation: Option<&CancellationToken>,
    budget: &mut RequestBudget,
    cache: &TranslationCache,
    scope: &str,
    location: &FormattingState,
) -> Result<Vec<String>, BabelEbookError> {
    let paragraph = serde_json::to_string(fragments).expect("fragments serialize");
    let group_scope = format!("{scope}-fragments-r04");
    let mut pending = vec![(0, fragments.len(), 0)];
    let mut output = Vec::with_capacity(fragments.len());
    let mut last_failure = String::new();
    while let Some((start, end, depth)) = pending.pop() {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(BabelEbookError::Cancelled);
        }
        let group = &fragments[start..end];
        let key = serde_json::json!([paragraph, start, end]).to_string();
        if depth > 0 {
            if let Some(cached) = cache.get_async(&group_scope, &key).await {
                if let Ok(translated) = decode(&cached, false, false, group.len()) {
                    output.extend(translated);
                    continue;
                }
            }
        }
        let mut complete = false;
        for attempt_index in 0..if depth == 0 && group.len() > 1 { 2 } else { 1 } {
            let attempt = request(
                group,
                (depth > 0).then_some(paragraph.as_str()),
                attempt_index > 0,
                translator,
                context,
                max_input_tokens,
                cancellation,
                budget,
            )
            .await
            .map_err(|error| match error {
                BabelEbookError::ApiError(message) if budget.used >= MAX_REQUESTS => {
                    BabelEbookError::ApiError(format!(
                        "{message}; last formatting failure: {last_failure}"
                    ))
                }
                other => other,
            })?;
            match attempt.result {
                Ok(translated) => {
                    if depth > 0 {
                        cache
                            .put_async(
                                &group_scope,
                                &key,
                                &serde_json::to_string(&translated)
                                    .expect("translations serialize"),
                                None,
                            )
                            .await;
                    }
                    output.extend(translated);
                    complete = true;
                    break;
                }
                Err(reason) => {
                    let parsed: Option<serde_json::Value> =
                        serde_json::from_str(attempt.response.trim()).ok();
                    let actual = parsed
                        .as_ref()
                        .and_then(|v| {
                            v.as_array().or_else(|| {
                                v.get("translations").and_then(serde_json::Value::as_array)
                            })
                        })
                        .map(Vec::len);
                    let record = serde_json::json!({
                        "revision": "R04", "provider_model": translator.name(),
                        "chapter_href": location.chapter_href, "selected_element": location.element_index,
                        "attempt": budget.used, "request_limit": MAX_REQUESTS,
                        "fragment_range": [start + 1, end], "split_depth": depth,
                        "expected_count": group.len(), "actual_count": actual,
                        "error": reason, "source_fragments": group,
                        "response": attempt.response.chars().take(MAX_RESPONSE_CHARS).collect::<String>(),
                        "response_truncated_in_log": attempt.response.chars().count() > MAX_RESPONSE_CHARS,
                    });
                    let path = cache.write_format_diagnostic(&record).await;
                    last_failure = format!(
                        "{reason}; fragments {}..{end}; attempts {}/{}{}",
                        start + 1,
                        budget.used,
                        MAX_REQUESTS,
                        path.as_ref()
                            .map_or_else(String::new, |p| format!("; diagnostic={}", p.display()))
                    );
                    budget.last_format_failure = Some(last_failure.clone());
                    tracing::warn!(error = %last_failure, "R04 rejected a formatting response; bounded recovery only");
                }
            }
        }
        if complete {
            continue;
        }
        if group.len() <= 1 || depth >= MAX_FRAGMENT_SPLIT_DEPTH {
            return Err(BabelEbookError::ApiError(format!(
                "{last_failure}; safe fragment recovery stopped"
            )));
        }
        let middle = start + group.len() / 2;
        pending.push((middle, end, depth + 1));
        pending.push((start, middle, depth + 1));
    }
    Ok(output)
}
