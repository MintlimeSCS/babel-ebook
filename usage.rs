//! Per-run translation usage. Token totals come exclusively from API responses.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// User-supplied USD rates for one specific model; no stale prices are assumed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsagePrices {
    /// Exact model to which these rates apply.
    pub model: String,
    /// Uncached input USD per million tokens.
    pub input: Option<f64>,
    /// Cached input USD per million tokens.
    pub cached_input: Option<f64>,
    /// Output USD per million tokens, including reasoning if reported.
    pub output: Option<f64>,
}
impl UsagePrices {
    pub(crate) fn valid(&self) -> bool {
        [self.input, self.cached_input, self.output]
            .into_iter()
            .flatten()
            .all(|p| p.is_finite() && p >= 0.0)
    }
}

/// Counters for this run only, including failed/truncated paid responses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    /// Provider and model identity, excluding credentials.
    pub provider_model: String,
    /// Physical translation HTTP send attempts (not health checks).
    pub api_calls: u64,
    /// Actual reported input tokens, cached tokens included.
    pub input_tokens: u64,
    /// Actual reported output tokens.
    pub output_tokens: u64,
    /// Provider-reported cached input tokens, a subset of input tokens.
    pub cached_input_tokens: u64,
    /// Valid local cache lookups consumed by this run.
    pub local_cache_hits: u64,
    /// Unsuccessful local cache lookups, including invalid entries.
    pub local_cache_misses: u64,
    /// Additional wire attempts due to transient HTTP failures.
    pub http_retries: u64,
    /// Additional recovery attempts after truncation or invalid merged results.
    pub recovery_retries: u64,
    /// Responses containing complete token usage.
    pub usage_responses: u64,
    /// Completed/failed/cancelled requests without complete usage; billing unknown.
    pub unreported_requests: u64,
    /// USD estimate for known usage only. None if required prices are unset.
    pub estimated_cost_usd: Option<f64>,
    /// Rates actually supplied by the user; bound to their specified model.
    pub prices: UsagePrices,
}

pub(crate) struct UsageCollector {
    snapshot: Mutex<UsageSnapshot>,
    updates: tokio::sync::watch::Sender<u64>,
}
tokio::task_local! {
    pub(crate) static CURRENT: Arc<UsageCollector>;
}
impl UsageCollector {
    pub(crate) fn new(identity: String, model: &str, prices: &UsagePrices) -> Arc<Self> {
        let (updates, _) = tokio::sync::watch::channel(0);
        let mut snapshot = UsageSnapshot {
            provider_model: identity,
            ..UsageSnapshot::default()
        };
        if prices.model == model && prices.valid() {
            snapshot.prices = prices.clone();
        }
        Arc::new(Self {
            snapshot: Mutex::new(snapshot),
            updates,
        })
    }
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.updates.subscribe()
    }
    pub(crate) fn snapshot(&self) -> UsageSnapshot {
        let mut s = self.snapshot.lock().expect("usage lock").clone();
        let p = &s.prices;
        let cached = s.cached_input_tokens.min(s.input_tokens);
        s.estimated_cost_usd = p.input.zip(p.output).and_then(|(input, output)| {
            let cached_price = if cached == 0 {
                Some(0.0)
            } else {
                p.cached_input
            }?;
            Some(
                ((s.input_tokens - cached) as f64 * input
                    + cached as f64 * cached_price
                    + s.output_tokens as f64 * output)
                    / 1_000_000.0,
            )
        });
        s
    }
    fn update(&self, f: impl FnOnce(&mut UsageSnapshot)) {
        f(&mut self.snapshot.lock().expect("usage lock"));
        self.updates
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}
pub(crate) fn cache_lookup(hit: bool) {
    let _ = CURRENT.try_with(|s| {
        s.update(|v| {
            if hit {
                v.local_cache_hits += 1;
            } else {
                v.local_cache_misses += 1;
            }
        })
    });
}
pub(crate) fn recovery_retry() {
    let _ = CURRENT.try_with(|s| s.update(|v| v.recovery_retries += 1));
}

/// A cancelled request remains counted and is explicitly marked as usage unknown.
pub(crate) struct RequestAttempt {
    collector: Option<Arc<UsageCollector>>,
    recorded: bool,
}
impl RequestAttempt {
    pub(crate) fn start(retry: bool, enabled: bool) -> Self {
        let collector = if enabled {
            CURRENT.try_with(Arc::clone).ok()
        } else {
            None
        };
        if let Some(s) = &collector {
            s.update(|v| {
                v.api_calls += 1;
                if retry {
                    v.http_retries += 1;
                }
            });
        }
        Self {
            collector,
            recorded: false,
        }
    }
    pub(crate) fn response(&mut self, body: &str) {
        if let Some(s) = &self.collector {
            let json: Value = serde_json::from_str(body).unwrap_or_default();
            let usage = &json["usage"];
            let input = usage["prompt_tokens"].as_u64();
            let output = usage["completion_tokens"].as_u64();
            s.update(|v| {
                if let (Some(input), Some(output)) = (input, output) {
                    v.input_tokens = v.input_tokens.saturating_add(input);
                    v.output_tokens = v.output_tokens.saturating_add(output);
                    let cached = usage["prompt_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(input);
                    v.cached_input_tokens = v.cached_input_tokens.saturating_add(cached);
                    v.usage_responses += 1;
                } else {
                    v.unreported_requests += 1;
                }
            });
        }
        self.recorded = true;
    }
}
impl Drop for RequestAttempt {
    fn drop(&mut self) {
        if !self.recorded {
            if let Some(s) = &self.collector {
                s.update(|v| v.unreported_requests += 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reports_usage_including_cached_and_truncated_responses() {
        let prices = UsagePrices {
            model: "test".into(),
            input: Some(2.0),
            cached_input: Some(0.5),
            output: Some(8.0),
        };
        let s = UsageCollector::new("openai:test".into(), "test", &prices);
        CURRENT.scope(s.clone(), async {
            let mut request = RequestAttempt::start(false, true);
            request.response(r#"{"usage":{"prompt_tokens":1000,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":400}},"choices":[{"finish_reason":"length"}]}"#);
            drop(request);
            drop(RequestAttempt::start(true, true));
            cache_lookup(true); cache_lookup(false); recovery_retry();
        }).await;
        let v = s.snapshot();
        assert_eq!(
            (
                v.api_calls,
                v.input_tokens,
                v.output_tokens,
                v.cached_input_tokens
            ),
            (2, 1000, 100, 400)
        );
        assert_eq!(
            (v.http_retries, v.recovery_retries, v.unreported_requests),
            (1, 1, 1)
        );
        assert!((v.estimated_cost_usd.unwrap() - 0.0022).abs() < 1e-12);
        let resumed = UsageCollector::new("openai:test".into(), "test", &prices);
        assert_eq!(resumed.snapshot().input_tokens, 0);
    }
    #[tokio::test]
    async fn isolates_runs_and_marks_missing_usage() {
        let a = UsageCollector::new("openai:a".into(), "a", &UsagePrices::default());
        let b = UsageCollector::new("openai:b".into(), "b", &UsagePrices::default());
        tokio::join!(
            CURRENT.scope(a.clone(), async {
                let mut r = RequestAttempt::start(false, true);
                r.response("{}");
            }),
            CURRENT.scope(b.clone(), async {
                cache_lookup(true);
            })
        );
        assert_eq!(a.snapshot().unreported_requests, 1);
        assert_eq!(a.snapshot().local_cache_hits, 0);
        assert_eq!(b.snapshot().api_calls, 0);
        assert_eq!(b.snapshot().local_cache_hits, 1);
        assert!(a.snapshot().estimated_cost_usd.is_none());
    }
    #[test]
    fn rejects_invalid_or_wrong_model_prices() {
        let p = UsagePrices {
            model: "a".into(),
            input: Some(f64::NAN),
            ..UsagePrices::default()
        };
        assert!(!p.valid());
        let p = UsagePrices {
            model: "a".into(),
            input: Some(1.0),
            output: Some(2.0),
            ..UsagePrices::default()
        };
        assert!(UsageCollector::new("openai:b".into(), "b", &p)
            .snapshot()
            .estimated_cost_usd
            .is_none());
    }
}
