//! Shared HTTP helpers for translator providers.
//!
//! This module centralises the duplicated pieces of the provider translators:
//! reqwest client construction, timeout values, retry logic with exponential
//! backoff, JSON response parsing, and HTTP error formatting. Provider-specific
//! files keep only their base URLs, request body shapes, and response extraction
//! paths.

use crate::core::BabelEbookError;
use async_openai::config::{Config, OpenAIConfig};
use async_openai::Client;
use reqwest::{header::HeaderMap, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

/// Timeout for a single translation request.
pub const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Timeout for inexpensive metadata calls such as health checks and model lists.
pub const META_TIMEOUT: Duration = Duration::from_secs(10);
/// Number of retries applied consistently across all HTTP providers.
pub const MAX_RETRIES: u32 = 3;

/// Build the shared `reqwest::Client` used for provider metadata calls.
pub fn build_reqwest_client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Format an HTTP error response in a consistent way across providers.
pub fn format_http_error(provider: &str, status: StatusCode, body: &str) -> BabelEbookError {
    let body = body.trim();
    if body.is_empty() {
        BabelEbookError::ApiError(format!("{provider} HTTP error: {status}"))
    } else {
        BabelEbookError::ApiError(format!("{provider} HTTP error {status}: {body}"))
    }
}

/// Execute an async operation, retrying on failure with exponential backoff.
///
/// The `operation` closure is called fresh for each attempt so that request
/// bodies and futures can be reconstructed. After `MAX_RETRIES` failed attempts
/// the last error is returned wrapped in a provider-scoped message.
pub async fn with_retry<F, Fut, T>(
    provider: &str,
    operation_name: &str,
    operation: F,
) -> Result<T, BabelEbookError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, BabelEbookError>>,
{
    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                last_error = Some(e);
                if attempt == MAX_RETRIES {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt))).await;
            }
        }
    }

    let last_error = last_error.expect("loop always assigns an error before exiting");
    Err(BabelEbookError::ApiError(format!(
        "{provider} {operation_name} failed after {MAX_RETRIES} retries: {last_error}"
    )))
}

/// Extract a list of model identifiers from a JSON array.
///
/// `array_field` names the top-level field holding the array (e.g. `"data"` or
/// `"models"`) and `id_field` names the field containing the model id (e.g.
/// `"id"` or `"name"`).
pub fn parse_model_list(json: &Value, array_field: &str, id_field: &str) -> Vec<String> {
    json[array_field]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m[id_field].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Build a chat request, using the completion-token limit for GPT-5 models.
fn build_chat_completion_request(
    model: &str,
    system_prompt: &str,
    text: &str,
    max_tokens: u32,
    temperature: f32,
) -> Value {
    let is_gpt5 = model == "gpt-5" || model.starts_with("gpt-5-") || model.starts_with("gpt-5.");
    let mut request = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": text},
        ],
    });
    if is_gpt5 {
        request["max_completion_tokens"] = max_tokens.into();
        // Omit sampling controls for GPT-5 to avoid model-specific restrictions.
    } else {
        request["max_tokens"] = max_tokens.into();
        request["temperature"] = temperature.into();
    }
    request
}

/// Translate a chunk using an OpenAI-compatible chat completion endpoint.
///
/// This helper builds the standard system/user message request, applies the
/// translate timeout, retries on failure, and extracts the first choice content.
#[allow(clippy::significant_drop_tightening)] // The queue lock intentionally covers HTTP and retries.
pub async fn openai_compatible_translate(
    client: &Client<OpenAIConfig>,
    model: &str,
    system_prompt: &str,
    text: &str,
    max_tokens: u32,
    temperature: f32,
    provider_name: &str,
) -> Result<String, BabelEbookError> {
    let request =
        build_chat_completion_request(model, system_prompt, text, max_tokens, temperature);
    // One gate per endpoint/model across chapters and jobs. Keeping the gate
    // through retries prevents every concurrent chapter from hitting the same
    // exhausted token bucket. No credentials are used as map keys or persisted.
    let gate = request_gate(&client.config().url("/chat/completions"), model);
    let budget = crate::chunking::count_tokens(system_prompt)
        .saturating_add(crate::chunking::count_tokens(text))
        .saturating_add(max_tokens as usize)
        .saturating_add(32);
    let mut schedule = gate.lock().await;
    let http_client = build_reqwest_client();
    for attempt in 0..=RATE_LIMIT_RETRIES {
        if schedule.next_send.saturating_duration_since(Instant::now()) > MAX_RATE_LIMIT_WAIT {
            return Err(BabelEbookError::ApiError(format!(
                "{provider_name} rate-limit wait exceeds five minutes; retry the job later"
            )));
        }
        tokio::time::sleep_until(schedule.next_send).await;
        let response = http_client
            .post(client.config().url("/chat/completions"))
            .headers(client.config().headers())
            .json(&request)
            .timeout(TRANSLATE_TIMEOUT)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if attempt >= MAX_RETRIES {
                    return Err(BabelEbookError::ApiError(error.to_string()));
                }
                schedule.defer(Duration::from_secs(2_u64.pow(attempt)));
                continue;
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        schedule.observe(&headers, budget);
        let body = response.text().await.map_err(|error| {
            BabelEbookError::ApiError(format!("{provider_name} response read failed: {error}"))
        })?;
        if status.is_success() {
            return extract_chat_content(&body, provider_name);
        }
        let error = format_http_error(provider_name, status, &body);
        if status == StatusCode::TOO_MANY_REQUESTS {
            // Billing/quota failures cannot be solved by repeatedly waiting.
            if is_quota_error(&body) {
                return Err(error);
            }
            let delay = rate_limit_delay(&headers, &body, attempt);
            // Do not shorten a server-provided wait. Very long waits are returned
            // as errors so a job cannot appear frozen for hours.
            schedule.defer(delay);
            if attempt == RATE_LIMIT_RETRIES || delay > MAX_RATE_LIMIT_WAIT {
                return Err(error);
            }
            tracing::warn!(
                provider = provider_name,
                wait_seconds = delay.as_secs_f64(),
                "Rate limit reached; pausing shared request queue before retry"
            );
        } else if (status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT)
            && attempt < MAX_RETRIES
        {
            schedule.defer(Duration::from_secs(2_u64.pow(attempt)));
        } else {
            // Authentication, unsupported parameters and other permanent 4xx
            // errors fail immediately, without SDK or outer retry multiplication.
            return Err(error);
        }
    }
    unreachable!("bounded retry loop returns on its last attempt")
}

const RATE_LIMIT_RETRIES: u32 = MAX_RETRIES;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(300);

type SharedSchedule = Arc<tokio::sync::Mutex<RequestSchedule>>;

struct RequestSchedule {
    next_send: Instant,
}

impl RequestSchedule {
    fn defer(&mut self, delay: Duration) {
        self.next_send = self.next_send.max(Instant::now() + delay);
    }

    fn observe(&mut self, headers: &HeaderMap, budget: usize) {
        let budget = f64::from(u32::try_from(budget).unwrap_or(u32::MAX));
        // Smooth the request rate once the server advertises limits, rather
        // than using all available tokens immediately at the start of a minute.
        for (limit, remaining, reset, needed) in [
            (
                "x-ratelimit-limit-tokens",
                "x-ratelimit-remaining-tokens",
                "x-ratelimit-reset-tokens",
                budget,
            ),
            (
                "x-ratelimit-limit-project-tokens",
                "x-ratelimit-remaining-project-tokens",
                "x-ratelimit-reset-project-tokens",
                budget,
            ),
            (
                "x-ratelimit-limit-requests",
                "x-ratelimit-remaining-requests",
                "x-ratelimit-reset-requests",
                1.0,
            ),
        ] {
            if let Some(capacity) = header_number(headers, limit).filter(|v| *v > 0.0) {
                self.defer(Duration::from_secs_f64(
                    (60.0 * needed / capacity * 1.1).min(300.0),
                ));
            }
            if header_number(headers, remaining).is_some_and(|available| available < needed) {
                if let Some(wait) = header_duration(headers, reset) {
                    self.defer(wait + Duration::from_millis(250));
                }
            }
        }
    }
}

fn request_gate(endpoint: &str, model: &str) -> SharedSchedule {
    static GATES: OnceLock<Mutex<HashMap<(String, String), SharedSchedule>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("request gate map");
    gates
        .entry((endpoint.into(), model.into()))
        .or_insert_with(|| {
            Arc::new(tokio::sync::Mutex::new(RequestSchedule {
                next_send: Instant::now(),
            }))
        })
        .clone()
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn parse_duration(value: &str) -> Option<Duration> {
    static DURATION: OnceLock<regex::Regex> = OnceLock::new();
    if let Ok(seconds) = value.trim().parse::<f64>() {
        return (seconds.is_finite() && (0.0..=86400.0).contains(&seconds))
            .then(|| Duration::from_secs_f64(seconds));
    }
    let pattern = DURATION
        .get_or_init(|| regex::Regex::new(r"(\d+(?:\.\d+)?)(ms|s|m|h)").expect("duration regex"));
    let mut end = 0;
    let mut seconds = 0.0;
    for captures in pattern.captures_iter(value.trim()) {
        let token = captures.get(0)?;
        if token.start() != end {
            return None;
        }
        let number: f64 = captures[1].parse().ok()?;
        seconds = number.mul_add(
            match &captures[2] {
                "ms" => 0.001,
                "s" => 1.0,
                "m" => 60.0,
                _ => 3600.0,
            },
            seconds,
        );
        end = token.end();
    }
    (end > 0 && end == value.trim().len() && seconds.is_finite() && seconds <= 86400.0)
        .then(|| Duration::from_secs_f64(seconds))
}

fn header_duration(headers: &HeaderMap, name: &str) -> Option<Duration> {
    parse_duration(headers.get(name)?.to_str().ok()?)
}

fn rate_limit_delay(headers: &HeaderMap, body: &str, attempt: u32) -> Duration {
    static WAIT: OnceLock<regex::Regex> = OnceLock::new();
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            parse_duration(v).or_else(|| {
                httpdate::parse_http_date(v)
                    .ok()
                    .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
            })
        });
    let milliseconds = header_number(headers, "retry-after-ms")
        .filter(|v| *v <= 86_400_000.0)
        .map(|v| Duration::from_secs_f64(v / 1000.0));
    let json: Value = serde_json::from_str(body).unwrap_or_default();
    let message = json["error"]["message"].as_str().unwrap_or(body);
    let pattern = WAIT.get_or_init(|| {
        regex::Regex::new(r"(?i)try again in ([0-9.]+(?:ms|s|m|h))").expect("wait regex")
    });
    let body_wait = pattern
        .captures(message)
        .and_then(|c| parse_duration(&c[1]));
    let wait = [
        retry_after,
        milliseconds,
        body_wait,
        header_duration(headers, "x-ratelimit-reset-tokens"),
        header_duration(headers, "x-ratelimit-reset-project-tokens"),
        header_duration(headers, "x-ratelimit-reset-requests"),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or_else(|| Duration::from_secs(5 * 2_u64.pow(attempt.min(5))));
    wait.max(Duration::from_millis(500)) + Duration::from_millis(250)
}

fn is_quota_error(body: &str) -> bool {
    let json: Value = serde_json::from_str(body).unwrap_or_default();
    ["code", "type"].iter().any(|field| {
        matches!(
            json["error"][field].as_str(),
            Some("insufficient_quota" | "billing_hard_limit_reached")
        )
    })
}

fn extract_chat_content(body: &str, provider: &str) -> Result<String, BabelEbookError> {
    let response: Value =
        serde_json::from_str(body).map_err(|error| BabelEbookError::ApiError(error.to_string()))?;
    let choice = &response["choices"][0];
    if choice["finish_reason"] == "content_filter"
        || choice["message"]["refusal"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    {
        return Err(BabelEbookError::ApiError(format!(
            "{provider} declined this translation; no partial response was cached"
        )));
    }
    if choice["finish_reason"] == "length" {
        return Err(BabelEbookError::ApiError(format!(
            "{provider} output reached its token limit and was truncated; increase maximum output tokens or reduce chunk size. No truncated translation was cached."
        )));
    }
    let content = choice["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim();
    if content.is_empty() {
        return Err(BabelEbookError::ApiError(format!(
            "{provider} API returned empty content"
        )));
    }
    Ok(content.to_string())
}

/// Perform a lightweight health check against an OpenAI-compatible `/models`
/// endpoint.
pub async fn openai_compatible_health_check(
    config: &OpenAIConfig,
    provider_name: &str,
) -> Result<(), BabelEbookError> {
    let client = build_reqwest_client();
    let url = config.url("/models");
    let response = client
        .get(&url)
        .headers(config.headers())
        .timeout(META_TIMEOUT)
        .send()
        .await
        .map_err(|e| BabelEbookError::ApiError(e.to_string()))?;

    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = response.text().await.unwrap_or_default();
        Err(format_http_error(provider_name, status, &body))
    }
}

/// List models from an OpenAI-compatible `/models` endpoint.
pub async fn openai_compatible_list_models(
    config: &OpenAIConfig,
    provider_name: &str,
) -> Result<Vec<String>, BabelEbookError> {
    let client = build_reqwest_client();
    let url = config.url("/models");
    let response = client
        .get(&url)
        .headers(config.headers())
        .timeout(META_TIMEOUT)
        .send()
        .await
        .map_err(|e| BabelEbookError::ApiError(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format_http_error(provider_name, status, &body));
    }

    let json: Value = response.json().await.map_err(|e| {
        BabelEbookError::ApiError(format!("failed to parse {provider_name} models: {e}"))
    })?;

    Ok(parse_model_list(&json, "data", "id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt5_requests_serialize_with_completion_limit_only() {
        for model in [
            "gpt-5",
            "gpt-5-mini",
            "gpt-5.4-mini",
            "gpt-5.4-mini-2026-03-17",
        ] {
            let request =
                build_chat_completion_request(model, "Translate faithfully.", "Hello.", 3000, 0.2);
            let json = serde_json::to_value(request).unwrap();
            assert_eq!(json["model"], model);
            assert_eq!(json["max_completion_tokens"], 3000);
            assert!(json.get("max_tokens").is_none());
            assert!(json.get("temperature").is_none());
            assert_eq!(json["messages"][0]["content"], "Translate faithfully.");
            assert_eq!(json["messages"][1]["content"], "Hello.");
        }
    }

    #[test]
    fn other_models_keep_legacy_request_parameters() {
        for model in ["gpt-4o-mini", "deepseek-chat", "llama3", "gpt-50-test"] {
            let request = build_chat_completion_request(model, "Translate.", "Hello.", 3000, 0.2);
            let json = serde_json::to_value(request).unwrap();
            assert_eq!(json["max_tokens"], 3000);
            assert!(json.get("max_completion_tokens").is_none());
            assert!(json["temperature"].is_number());
        }
    }

    #[test]
    fn format_http_error_includes_body() {
        let err = format_http_error("TestProvider", StatusCode::BAD_REQUEST, "bad request");
        let msg = err.to_string();
        assert!(msg.contains("TestProvider HTTP error 400 Bad Request: bad request"));
    }

    #[test]
    fn format_http_error_omits_body_when_empty() {
        let err = format_http_error("TestProvider", StatusCode::INTERNAL_SERVER_ERROR, "   ");
        let msg = err.to_string();
        assert!(msg.contains("TestProvider HTTP error: 500 Internal Server Error"));
        assert!(!msg.contains("Internal Server Error:"));
    }

    #[test]
    fn parse_model_list_extracts_ids() {
        let json = serde_json::json!({
            "data": [
                {"id": "model-a"},
                {"id": "model-b"},
            ]
        });
        assert_eq!(
            parse_model_list(&json, "data", "id"),
            vec!["model-a", "model-b"]
        );
    }

    #[test]
    fn parse_model_list_uses_custom_fields() {
        let json = serde_json::json!({
            "models": [
                {"name": "llama3"},
                {"name": "qwen2"},
            ]
        });
        assert_eq!(
            parse_model_list(&json, "models", "name"),
            vec!["llama3", "qwen2"]
        );
    }

    #[test]
    fn parse_model_list_returns_empty_on_bad_shape() {
        let cases = [
            serde_json::json!({}),
            serde_json::json!({"data": "not-an-array"}),
            serde_json::json!({"data": [{"name": "missing-id"}]}),
        ];
        for case in cases {
            assert!(parse_model_list(&case, "data", "id").is_empty());
        }
    }

    #[tokio::test]
    async fn with_retry_succeeds_without_retries() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let result = with_retry("Provider", "op", {
            let counter = counter.clone();
            move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok::<_, BabelEbookError>("done")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "done");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn with_retry_retries_then_succeeds() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let result = with_retry("Provider", "op", {
            let counter = counter.clone();
            move || {
                let counter = counter.clone();
                async move {
                    let attempt = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt < 2 {
                        Err(BabelEbookError::ApiError("transient".into()))
                    } else {
                        Ok::<_, BabelEbookError>("recovered")
                    }
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "recovered");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn with_retry_gives_up_after_max_retries() {
        let result = with_retry("Provider", "op", || async {
            Err::<(), _>(BabelEbookError::ApiError("always fails".into()))
        })
        .await;
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Provider op failed after 3 retries"));
        assert!(msg.contains("always fails"));
    }

    #[test]
    fn duration_parser_handles_openai_units_and_rejects_invalid_values() {
        for (value, ms) in [
            ("415ms", 415),
            ("1.5s", 1500),
            ("1m2.5s", 62_500),
            ("2h", 7_200_000),
        ] {
            assert_eq!(parse_duration(value).unwrap().as_millis(), ms);
        }
        for value in ["", "NaN", "inf", "-1", "oops1s", "1sXXX", "999999h"] {
            assert!(parse_duration(value).is_none(), "{value}");
        }
    }

    #[test]
    fn rate_limit_delay_uses_headers_body_and_bounded_backoff() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "2".parse().unwrap());
        headers.insert("retry-after-ms", "2500".parse().unwrap());
        headers.insert("x-ratelimit-reset-tokens", "1m2s".parse().unwrap());
        assert_eq!(
            rate_limit_delay(&headers, "{}", 0),
            Duration::from_millis(62_250)
        );
        assert_eq!(
            rate_limit_delay(
                &HeaderMap::new(),
                r#"{"error":{"message":"Please try again in 415ms."}}"#,
                0
            ),
            Duration::from_millis(750)
        );
        assert_eq!(
            rate_limit_delay(&HeaderMap::new(), "{}", 1),
            Duration::from_millis(10_250)
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(20))
                .parse()
                .unwrap(),
        );
        assert!(rate_limit_delay(&headers, "{}", 0) >= Duration::from_secs(19));
    }

    #[test]
    fn quota_truncation_and_refusal_are_rejected() {
        assert!(is_quota_error(r#"{"error":{"code":"insufficient_quota"}}"#));
        assert!(!is_quota_error(
            r#"{"error":{"code":"rate_limit_exceeded"}}"#
        ));
        assert!(extract_chat_content(
            r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}"#,
            "OpenAI"
        )
        .unwrap_err()
        .to_string()
        .contains("truncated"));
        assert!(extract_chat_content(
            r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"content_filter"}]}"#,
            "OpenAI"
        )
        .is_err());
        assert_eq!(
            extract_chat_content(
                r#"{"choices":[{"message":{"content":" 完整譯文 "},"finish_reason":"stop"}]}"#,
                "OpenAI"
            )
            .unwrap(),
            "完整譯文"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_paces_tokens_and_observes_exhausted_request_bucket() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit-tokens", "200000".parse().unwrap());
        headers.insert("x-ratelimit-remaining-tokens", "100".parse().unwrap());
        headers.insert("x-ratelimit-reset-tokens", "2s".parse().unwrap());
        let start = Instant::now();
        let mut schedule = RequestSchedule { next_send: start };
        schedule.observe(&headers, 4000);
        assert_eq!(
            schedule.next_send.duration_since(start),
            Duration::from_millis(2250)
        );
        headers.insert("x-ratelimit-remaining-tokens", "10000".parse().unwrap());
        let mut schedule = RequestSchedule { next_send: start };
        schedule.observe(&headers, 4000);
        assert!(schedule.next_send.duration_since(start).as_secs_f64() >= 1.31);
    }

    type RecordedCalls = Arc<Mutex<Vec<(Instant, Value)>>>;

    async fn mock_chat_server(
        responses: Vec<(u16, &'static str, String)>,
    ) -> (String, RecordedCalls, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let calls: RecordedCalls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let handle = tokio::spawn(async move {
            for (status, headers, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let (header_end, content_length) = loop {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let len: usize = header
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        break (end + 4, len);
                    }
                };
                while bytes.len() < header_end + content_length {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                recorded.lock().unwrap().push((
                    Instant::now(),
                    serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .unwrap(),
                ));
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (url, calls, handle)
    }

    fn successful_chat() -> String {
        r#"{"choices":[{"message":{"content":"你好"},"finish_reason":"stop"}]}"#.into()
    }

    #[tokio::test]
    async fn concurrent_chapters_share_429_wait_and_keep_gpt5_parameters() {
        let (url, calls, server) = mock_chat_server(vec![
            (429, "Retry-After: 0.1\r\n", r#"{"error":{"code":"rate_limit_exceeded","message":"Please try again in 100ms."}}"#.into()),
            (200, "", successful_chat()),
            (200, "", successful_chat()),
        ]).await;
        let client = Client::with_config(
            OpenAIConfig::new()
                .with_api_key("fake-key")
                .with_api_base(url),
        );
        let make = || {
            openai_compatible_translate(
                &client,
                "gpt-5.4-mini",
                "Translate",
                "Hello",
                2000,
                0.2,
                "OpenAI",
            )
        };
        let (a, b) = tokio::join!(make(), make());
        assert_eq!(a.unwrap(), "你好");
        assert_eq!(b.unwrap(), "你好");
        server.await.unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].0.duration_since(calls[0].0) >= Duration::from_millis(740));
        for (_, request) in calls.iter() {
            assert_eq!(request["max_completion_tokens"], 2000);
            assert!(request.get("max_tokens").is_none());
            assert!(request.get("temperature").is_none());
        }
    }

    #[tokio::test]
    async fn quota_and_bad_requests_fail_without_retrying() {
        for (status, error) in [
            (429, r#"{"error":{"code":"insufficient_quota"}}"#),
            (400, r#"{"error":{"code":"unsupported_parameter"}}"#),
            (401, r#"{"error":{"code":"invalid_api_key"}}"#),
        ] {
            let (url, calls, server) = mock_chat_server(vec![(status, "", error.into())]).await;
            let client = Client::with_config(
                OpenAIConfig::new()
                    .with_api_key("fake-key")
                    .with_api_base(url),
            );
            assert!(openai_compatible_translate(
                &client,
                "gpt-4o-mini",
                "Translate",
                "Hello",
                2000,
                0.2,
                "OpenAI"
            )
            .await
            .is_err());
            server.await.unwrap();
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
    }
}
