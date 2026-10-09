//! `OpenAI` / OpenAI-compatible translator provider.

use crate::core::BabelEbookError;
use crate::translator::http_common::{
    openai_compatible_health_check, openai_compatible_list_models, openai_compatible_translate,
    openai_compatible_translate_with_format,
};
use crate::translator::{TranslateContext, Translator};
use async_openai::config::{Config as _, OpenAIConfig};
use async_trait::async_trait;

const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Translator using the `OpenAI` API or an OpenAI-compatible endpoint.
pub struct OpenAiTranslator {
    client: async_openai::Client<OpenAIConfig>,
    model: String,
    max_tokens: usize,
    temperature: f32,
}

impl OpenAiTranslator {
    /// Create a new `OpenAI` translator.
    pub fn new(
        api_key: String,
        model: Option<String>,
        base_url: Option<String>,
        max_tokens: usize,
        temperature: f32,
    ) -> Self {
        let mut config = OpenAIConfig::default().with_api_key(api_key);
        if let Some(url) = base_url {
            config = config.with_api_base(url);
        }
        Self {
            client: async_openai::Client::with_config(config),
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            max_tokens,
            temperature,
        }
    }

    fn config(&self) -> &OpenAIConfig {
        self.client.config()
    }

    fn output_limit(&self) -> Result<u32, BabelEbookError> {
        u32::try_from(self.max_tokens).map_err(|_| {
            BabelEbookError::Configuration(format!(
                "max_tokens {} exceeds u32::MAX",
                self.max_tokens
            ))
        })
    }
}

#[async_trait]
impl Translator for OpenAiTranslator {
    fn name(&self) -> String {
        format!("openai:{}", self.model)
    }

    fn max_output_tokens(&self) -> usize {
        self.max_tokens
    }

    fn cache_identity(&self) -> String {
        serde_json::json!([self.name(), self.config().api_base(), self.temperature]).to_string()
    }

    fn fragment_response_format(&self, count: usize) -> Option<serde_json::Value> {
        // Both selected model families support Structured Outputs on the native
        // endpoint. Unknown, fine-tuned and proxy models keep their protocol.
        let native = self.config().api_base().trim_end_matches('/') == "https://api.openai.com/v1";
        let supported = self.model == "gpt-4.1-mini"
            || self.model == "gpt-4.1-mini-2025-04-14"
            || self.model == "gpt-5.4"
            || self.model.strip_prefix("gpt-5.4-").is_some_and(|suffix| {
                suffix == "mini"
                    || suffix.starts_with("mini-20")
                    || suffix == "nano"
                    || suffix.starts_with("nano-20")
                    || suffix.starts_with("20")
            });
        (native && supported).then(|| fragment_format(count))
    }

    async fn translate_fragments(
        &self,
        text: &str,
        context: &TranslateContext<'_>,
        count: usize,
    ) -> Result<String, BabelEbookError> {
        openai_compatible_translate_with_format(
            &self.client,
            &self.model,
            context.system_prompt,
            text,
            self.output_limit()?,
            self.temperature,
            "OpenAI",
            self.fragment_response_format(count),
        )
        .await
    }

    async fn health_check(&self) -> Result<(), BabelEbookError> {
        openai_compatible_health_check(self.config(), "OpenAI").await
    }

    async fn list_models(&self) -> Result<Vec<String>, BabelEbookError> {
        openai_compatible_list_models(self.config(), "OpenAI").await
    }

    async fn translate(
        &self,
        text: &str,
        context: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        openai_compatible_translate(
            &self.client,
            &self.model,
            context.system_prompt,
            text,
            self.output_limit()?,
            self.temperature,
            "OpenAI",
        )
        .await
    }
}

fn fragment_format(count: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "paragraph_fragments",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "translations": {
                        "type": "array",
                        "minItems": count,
                        "maxItems": count,
                        "items": {"type": "string", "pattern": "\\S"}
                    }
                },
                "required": ["translations"],
                "additionalProperties": false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_fragments_are_limited_to_supported_native_models() {
        for model in [
            "gpt-4.1-mini",
            "gpt-4.1-mini-2025-04-14",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.4-mini-2026-03-17",
        ] {
            let translator =
                OpenAiTranslator::new("fake".into(), Some(model.into()), None, 2000, 0.3);
            let format = translator.fragment_response_format(5).unwrap();
            assert_eq!(format["json_schema"]["strict"], true);
            let array = &format["json_schema"]["schema"]["properties"]["translations"];
            assert_eq!(array["minItems"], 5);
            assert_eq!(array["maxItems"], 5);
            assert_eq!(array["items"]["pattern"], "\\S");
        }
        for (model, base) in [
            ("gpt-4.1-mini-unknown", None),
            ("gpt-4.1-mini", Some("https://proxy.example/v1".into())),
            ("gpt-5.4-chat-latest", None),
            ("ft:gpt-5.4-mini:custom", None),
            ("gpt-5.4-mini", Some("https://proxy.example/v1".into())),
        ] {
            assert!(
                OpenAiTranslator::new("fake".into(), Some(model.into()), base, 2000, 0.3)
                    .fragment_response_format(5)
                    .is_none()
            );
        }
    }

    #[test]
    fn cache_identity_tracks_model_endpoint_and_temperature_but_not_credentials() {
        let make = |key: &str, model: &str, endpoint: &str, temperature| {
            OpenAiTranslator::new(
                key.into(),
                Some(model.into()),
                Some(endpoint.into()),
                2000,
                temperature,
            )
        };
        let original = make("first-key", "model-a", "https://example.org/v1", 0.3).cache_identity();
        assert_eq!(
            original,
            make("second-key", "model-a", "https://example.org/v1", 0.3).cache_identity()
        );
        assert_ne!(
            original,
            make("first-key", "model-b", "https://example.org/v1", 0.3).cache_identity()
        );
        assert_ne!(
            original,
            make("first-key", "model-a", "https://other.example.org/v1", 0.3).cache_identity()
        );
        assert_ne!(
            original,
            make("first-key", "model-a", "https://example.org/v1", 0.7).cache_identity()
        );
    }

    #[test]
    fn new_uses_defaults() {
        let translator = OpenAiTranslator::new("fake-key".into(), None, None, 2000, 0.3);
        assert_eq!(translator.name(), "openai:gpt-4o-mini");
        assert_eq!(translator.max_output_tokens(), 2000);
    }

    #[tokio::test]
    async fn list_models_returns_api_error_for_unreachable_endpoint() {
        let translator = OpenAiTranslator::new(
            "fake-key".to_string(),
            None,
            Some("http://localhost:0".to_string()),
            2000,
            0.3,
        );
        let err = translator.list_models().await.unwrap_err();
        assert!(matches!(err, BabelEbookError::ApiError(_)));
    }

    #[tokio::test]
    async fn max_tokens_exceeds_u32_max_fails_fast() {
        let oversized: usize = u32::MAX as usize + 1;
        let translator = OpenAiTranslator::new("fake-key".into(), None, None, oversized, 0.3);
        let context = TranslateContext {
            system_prompt: "translate to {target_lang}",
            target_lang: "zh-CN",
        };
        let err = translator
            .translate("hello", &context)
            .await
            .expect_err("max_tokens > u32::MAX should fail immediately");

        assert!(
            matches!(err, BabelEbookError::Configuration(_)),
            "expected configuration error, got {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("max_tokens") && msg.contains("u32::MAX"),
            "error message should describe the configuration problem: {msg}"
        );
        assert!(
            !matches!(err, BabelEbookError::ApiError(_)),
            "configuration error must not be wrapped as an API failure"
        );
    }
}
