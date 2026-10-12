//! Failure-only local diagnostics. No credentials, headers or system prompt text.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_FILES: usize = 50;
const MAX_DIRECTORY_BYTES: u64 = 10 * 1024 * 1024;
static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default, Serialize)]
struct AttemptInfo {
    attempt: usize,
    source_split_depth: usize,
    fragment_split_depth: usize,
    fragment_range: Option<[usize; 2]>,
}

#[derive(Clone)]
pub struct DiagnosticContext {
    directory: PathBuf,
    chapter_href: String,
    selected_element: Option<usize>,
    source_chunk: usize,
    configured_input_limit: usize,
    phase: &'static str,
    state: Arc<Mutex<AttemptInfo>>,
}

tokio::task_local! {
    static CONTEXT: DiagnosticContext;
}

impl DiagnosticContext {
    pub fn new(
        directory: PathBuf,
        chapter_href: &str,
        selected_element: Option<usize>,
        source_chunk: usize,
        configured_input_limit: usize,
        phase: &'static str,
    ) -> Self {
        Self {
            directory,
            chapter_href: chapter_href.to_string(),
            selected_element,
            source_chunk,
            configured_input_limit,
            phase,
            state: Arc::new(Mutex::new(AttemptInfo::default())),
        }
    }
}

pub async fn capture<F: Future>(context: DiagnosticContext, future: F) -> F::Output {
    CONTEXT.scope(context, future).await
}

pub fn source_depth(depth: usize) {
    let _ = CONTEXT.try_with(|context| {
        if let Ok(mut state) = context.state.lock() {
            state.source_split_depth = depth;
            state.fragment_split_depth = 0;
            state.fragment_range = None;
        }
    });
}

pub fn fragment_range(start: usize, end: usize, depth: usize) {
    let _ = CONTEXT.try_with(|context| {
        if let Ok(mut state) = context.state.lock() {
            state.fragment_range = Some([start, end]);
            state.fragment_split_depth = depth;
        }
    });
}

#[derive(Clone)]
pub struct RequestDiagnostic {
    context: DiagnosticContext,
    attempt: AttemptInfo,
}

pub fn begin_request() -> Option<RequestDiagnostic> {
    CONTEXT
        .try_with(|context| {
            let mut state = context.state.lock().ok()?;
            state.attempt += 1;
            Some(RequestDiagnostic {
                context: context.clone(),
                attempt: state.clone(),
            })
        })
        .ok()
        .flatten()
}

fn bounded_text(text: &str, limit: usize) -> Value {
    let length = text.chars().count();
    if length <= limit {
        serde_json::json!({"text": text, "omitted_characters": 0})
    } else {
        let head: String = text.chars().take(limit / 2).collect();
        let tail: String = text
            .chars()
            .rev()
            .take(limit / 2)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        serde_json::json!({"head": head, "tail": tail, "omitted_characters": length - limit})
    }
}

/// Write only an already-received incomplete response. This never calls an API.
#[allow(clippy::too_many_arguments)]
pub async fn save_truncation(
    diagnostic: Option<&RequestDiagnostic>,
    request: &Value,
    system_prompt: &str,
    source: &str,
    body: &str,
    request_id: Option<&str>,
    http_attempt: u32,
) -> Option<PathBuf> {
    let diagnostic = diagnostic?;
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let response: Value = serde_json::from_str(body).ok()?;
    let content = response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    let record = serde_json::json!({
        "revision": "R05", "event": "output_truncated",
        "recorded_at_unix_ns": time.to_string(),
        "chapter_href": diagnostic.context.chapter_href,
        "selected_element": diagnostic.context.selected_element,
        "source_chunk": diagnostic.context.source_chunk,
        "phase": diagnostic.context.phase, "recovery": diagnostic.attempt,
        "http_attempt": http_attempt + 1, "request_id": request_id,
        "model_requested": request["model"], "model_returned": response["model"],
        "finish_reason": response["choices"][0]["finish_reason"],
        "usage": response["usage"],
        "configured_input_limit": diagnostic.context.configured_input_limit,
        "max_tokens": request["max_tokens"],
        "max_completion_tokens": request["max_completion_tokens"],
        "temperature": request["temperature"], "response_format": request["response_format"],
        "estimated_prompt_tokens": crate::chunking::count_tokens(system_prompt),
        "estimated_source_tokens": crate::chunking::count_tokens(source),
        "prompt_sha256": hex::encode(Sha256::digest(system_prompt.as_bytes())),
        "source": bounded_text(source, 16_000),
        "response_content_characters": content.chars().count(),
        "response_sha256": hex::encode(Sha256::digest(body.as_bytes())),
        "raw_response": bounded_text(body, 48_000),
        "partial_response_cached": false
    });
    let directory = &diagnostic.context.directory;
    let sequence = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!("{time}-{}-{}.json", std::process::id(), sequence));
    let bytes = serde_json::to_vec_pretty(&record).ok()?;
    if let Err(error) = tokio::fs::create_dir_all(directory).await {
        tracing::warn!(%error, "Could not create R05 diagnostic directory");
        return None;
    }
    if let Err(error) = tokio::fs::write(&path, bytes).await {
        tracing::warn!(%error, "Could not save R05 truncation diagnostic");
        return None;
    }
    if let Err(error) = prune(directory) {
        tracing::warn!(%error, "Could not prune old R05 diagnostics");
    }
    Some(path)
}

fn prune(directory: &Path) -> std::io::Result<()> {
    let mut files: Vec<_> = std::fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            entry
                .metadata()
                .ok()
                .map(|metadata| (entry.path(), metadata.len()))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut count = files.len();
    let mut bytes: u64 = files.iter().map(|(_, size)| size).sum();
    for (path, size) in files {
        if count <= MAX_FILES && bytes <= MAX_DIRECTORY_BYTES {
            break;
        }
        std::fs::remove_file(path)?;
        count -= 1;
        bytes = bytes.saturating_sub(size);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_utf8_keeps_both_ends_and_retention_is_bounded() {
        let text = "繁體中文".repeat(100);
        let record = bounded_text(&text, 20);
        assert_eq!(record["omitted_characters"], 380);
        assert_eq!(record["head"].as_str().unwrap().chars().count(), 10);
        assert_eq!(record["tail"].as_str().unwrap().chars().count(), 10);
        let dir = tempfile::tempdir().unwrap();
        for index in 0..55 {
            std::fs::write(dir.path().join(format!("{index:03}.json")), "{}").unwrap();
        }
        prune(dir.path()).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), MAX_FILES);
        assert!(!dir.path().join("000.json").exists());
    }
}
