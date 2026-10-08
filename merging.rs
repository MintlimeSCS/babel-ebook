//! Conservative batching that restores every paragraph locally and commits atomically.
use super::{
    insertion::insert_translation,
    markup::ProtectedContent,
    progress::emit_chunk_progress,
    selection::{
        is_inside_excluded_subtree, is_inside_skipped_parent, is_translatable_text, node_ptr,
        normalize_text,
    },
    translation,
};
use crate::chunking::count_tokens;
use crate::{
    BabelEbookError, CancellationToken, ProgressCallback, TranslateContext, TranslationCache,
    TranslationOptions, Translator,
};
use kuchiki::{ElementData, NodeDataRef, NodeRef};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

fn eligible(
    element: &NodeDataRef<ElementData>,
    skip: &HashSet<*const kuchiki::Node>,
    options: &TranslationOptions,
) -> bool {
    let node = element.as_node();
    if !matches!(element.name.local.as_ref(), "p" | "div")
        || !options.translation_scope.body
        || skip.contains(&node_ptr(node))
        || is_inside_excluded_subtree(node, skip)
        || is_inside_skipped_parent(node)
        || !node.children().all(|c| c.as_text().is_some())
    {
        return false;
    }
    if element.name.local.as_ref() == "div" {
        let attrs = element.attributes.borrow();
        let class = attrs.get("class").unwrap_or("").to_lowercase();
        if [
            "title",
            "heading",
            "chapter",
            "toc",
            "caption",
            "copyright",
            "isbn",
            "tp-",
        ]
        .iter()
        .any(|name| class.contains(name))
        {
            return false;
        }
    }
    // Any id, link, semantic note attribute or translatable attribute uses R02's old path.
    if element
        .attributes
        .borrow()
        .map
        .keys()
        .any(|name| !matches!(name.local.as_ref(), "class" | "lang"))
    {
        return false;
    }
    if node.ancestors().any(|ancestor| {
        ancestor.as_element().is_some_and(|e| {
            !matches!(
                e.name.local.as_ref(),
                "p" | "div" | "section" | "body" | "html"
            ) || e.attributes.borrow().map.iter().any(|(name, attr)| {
                matches!(name.local.as_ref(), "type" | "role")
                    || (name.local.as_ref() == "class"
                        && attr.value.to_lowercase().contains("note"))
            })
        })
    }) {
        return false;
    }
    let text = normalize_text(node);
    is_translatable_text(&text)
        && !text.contains("[[BABEL")
        && count_tokens(&text) <= options.paragraph_merge.max_paragraph_tokens
}

fn adjacent(left: &NodeRef, right: &NodeRef) -> bool {
    let mut sibling = left.next_sibling();
    while let Some(node) = sibling {
        if node_ptr(&node) == node_ptr(right) {
            return true;
        }
        if !node.as_text().is_some_and(|t| t.borrow().trim().is_empty()) {
            return false;
        }
        sibling = node.next_sibling();
    }
    false
}

struct GroupRequest {
    prompt: String,
    payload: String,
    ids: Vec<String>,
    wrapped: bool,
}
fn request(
    sources: &[String],
    translator: &dyn Translator,
    options: &TranslationOptions,
    href: &str,
) -> Option<GroupRequest> {
    let fingerprint = hex::encode(Sha256::digest(
        serde_json::to_string(sources).ok()?.as_bytes(),
    ));
    let ids: Vec<_> = (0..sources.len())
        .map(|i| format!("[[BABEL_P:{}:{i}]]", &fingerprint[..12]))
        .collect();
    let items: Vec<_> = sources
        .iter()
        .zip(&ids)
        .map(|(s, id)| format!("{id}{s}"))
        .collect();
    let payload = serde_json::to_string(&items).ok()?;
    let format = translator.fragment_response_format(sources.len());
    let rule = if format.is_some() {
        "a JSON object with a translations array"
    } else {
        "a JSON array"
    };
    let prompt = format!("{}\nRequest envelope: the input is an ordered JSON array of {} independent adjacent paragraphs. Translate every paragraph fully using the translation rules above. Return ONLY {rule}, containing exactly {} strings in source order. Each string MUST start with its original [[BABEL_P:...]] identifier, followed by only that paragraph's translation. Copy each identifier exactly once. Never combine, omit or duplicate paragraphs. No commentary or HTML. The envelope identifiers are removed locally and never enter the book.", options.system_prompt_for_chapter(href), sources.len(), sources.len());
    let schema_tokens = format.as_ref().map_or(0, |s| count_tokens(&s.to_string()));
    // Includes effective V2.7/glossary prompt, JSON overhead, schema and safety margin.
    if count_tokens(&prompt)
        .saturating_add(count_tokens(&payload))
        .saturating_add(schema_tokens)
        .saturating_add(100)
        > options.max_input_tokens
    {
        return None;
    }
    let source_tokens: usize = sources.iter().map(|s| count_tokens(s)).sum();
    // Reserve conservative 3x translation growth plus envelope and reasoning headroom.
    let output_budget = source_tokens
        .saturating_mul(3)
        .saturating_add(count_tokens(&serde_json::to_string(&ids).ok()?))
        .saturating_add(256);
    if output_budget
        > options
            .max_output_tokens
            .min(translator.max_output_tokens())
    {
        return None;
    }
    Some(GroupRequest {
        prompt,
        payload,
        ids,
        wrapped: format.is_some(),
    })
}

fn validate(
    response: &str,
    req: &GroupRequest,
    sources: &[String],
) -> Result<Vec<String>, BabelEbookError> {
    let rows = translation::decode_fragments(response, req.wrapped, false)?;
    super::merge_validation::validate_rows(&rows, &req.ids, sources)
        .map_err(|message| BabelEbookError::ApiError(message.into()))
}

#[allow(
    clippy::too_many_arguments,
    clippy::future_not_send,
    clippy::too_many_lines
)]
pub(super) async fn try_merge(
    elements: &[NodeDataRef<ElementData>],
    skip: &HashSet<*const kuchiki::Node>,
    translator: &dyn Translator,
    options: &TranslationOptions,
    cache: &TranslationCache,
    index: usize,
    href: &str,
    progress: Option<&dyn ProgressCallback>,
    cancellation: Option<&CancellationToken>,
) -> Result<usize, BabelEbookError> {
    if !options.paragraph_merge.enabled || options.refine || elements.len() < 2 {
        return Ok(0);
    }
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(BabelEbookError::Cancelled);
    }
    let mut sources = Vec::new();
    for element in elements
        .iter()
        .take(options.paragraph_merge.max_paragraphs.clamp(2, 8))
    {
        if element.name.local != elements[0].name.local || !eligible(element, skip, options) {
            break;
        }
        if !sources.is_empty()
            && !adjacent(elements[sources.len() - 1].as_node(), element.as_node())
        {
            break;
        }
        sources.push(normalize_text(element.as_node()));
    }
    while sources.len() >= 2 && request(&sources, translator, options, href).is_none() {
        sources.pop();
    }
    if sources.len() < 2 {
        return Ok(0);
    }
    let scope = translation::cache_scope(
        translator,
        options,
        &options.system_prompt_for_chapter(href),
        "translate",
    );
    let mut translations = Vec::with_capacity(sources.len());
    for source in &sources {
        translations.push(translation::valid_cached(cache, &scope, source).await);
    }
    // Never batch across cached paragraphs; partition remaining adjacent misses.
    let mut position = 0;
    while position < sources.len() {
        if translations[position].is_some() {
            position += 1;
            continue;
        }
        let mut end = position + 1;
        while end < sources.len() && translations[end].is_none() {
            end += 1;
        }
        let req = if end - position >= 2 {
            request(&sources[position..end], translator, options, href)
        } else {
            None
        };
        let merged = if let Some(req) = req {
            let context = TranslateContext {
                system_prompt: &req.prompt,
                target_lang: &options.target_lang,
            };
            let future = translator.translate_fragments(&req.payload, &context, end - position);
            let response = if let Some(token) = cancellation {
                tokio::select! { biased; () = token.cancelled() => return Err(BabelEbookError::Cancelled), result = future => result }
            } else {
                future.await
            };
            match response {
                Ok(response) => validate(&response, &req, &sources[position..end]),
                Err(BabelEbookError::Cancelled) => return Err(BabelEbookError::Cancelled),
                Err(e) => Err(e),
            }
            .map(Some)
        } else {
            Ok(None)
        };
        if let Err(error) = &merged {
            tracing::warn!(%error, "Merged response rejected; retrying original paragraphs individually");
        }
        match merged {
            Ok(Some(outputs)) => {
                // Validate all paragraphs before caching any merged output.
                for (offset, output) in outputs.into_iter().enumerate() {
                    let i = position + offset;
                    cache.put_async(&scope, &sources[i], &output, None).await;
                    translations[i] = Some(output);
                }
            }
            Ok(None) | Err(_) => {
                let attempted_merge = end - position >= 2;
                for i in position..end {
                    if attempted_merge {
                        crate::usage::recovery_retry();
                    }
                    // The old R02 path includes its own chunk limits, marker checks,
                    // bounded truncation recovery and cache writes.
                    let output = translation::translate_text(
                        &sources[i],
                        translator,
                        options,
                        cache,
                        index,
                        href,
                        None,
                        cancellation,
                    )
                    .await?;
                    translations[i] = Some(output);
                }
            }
        }
        position = end;
    }
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(BabelEbookError::Cancelled);
    }
    // Stage restoration for every result before changing the DOM.
    let restored: Result<Vec<_>, _> = elements
        .iter()
        .zip(&translations)
        .map(|(element, text)| {
            ProtectedContent::from_node(element.as_node(), options)?.restore(
                text.as_deref().expect("all translated"),
                options.output_mode,
            )
        })
        .collect();
    let restored = restored?;
    for (i, (element, content)) in elements.iter().zip(restored).enumerate() {
        emit_chunk_progress(progress, index, href, i, sources.len(), false);
        insert_translation(element.as_node(), &element.name, &content, options);
        emit_chunk_progress(progress, index, href, i, sources.len(), true);
    }
    Ok(sources.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn req() -> GroupRequest {
        GroupRequest {
            prompt: String::new(),
            payload: String::new(),
            ids: vec!["[[BABEL_P:test:0]]".into(), "[[BABEL_P:test:1]]".into()],
            wrapped: false,
        }
    }
    #[test]
    fn validates_and_restores_each_identifier() {
        let s = vec!["first source".into(), "second source".into()];
        assert_eq!(
            validate(
                r#"["[[BABEL_P:test:0]]第一段","[[BABEL_P:test:1]]第二段"]"#,
                &req(),
                &s
            )
            .unwrap(),
            vec!["第一段", "第二段"]
        );
        for bad in [
            r#"[]"#,
            r#"["[[BABEL_P:test:1]]第二段","[[BABEL_P:test:0]]第一段"]"#,
            r#"["[[BABEL_P:test:0]]同樣","[[BABEL_P:test:1]]同樣"]"#,
            r#"["[[BABEL_P:test:0]]","[[BABEL_P:test:1]]第二段"]"#,
            r#"["[[BABEL_P:test:0]]<a>第一段</a>","[[BABEL_P:test:1]]第二段"]"#,
            "invalid JSON",
        ] {
            assert!(validate(bad, &req(), &s).is_err());
        }
    }
}
