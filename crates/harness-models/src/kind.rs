//! The one entry point: pick a protocol family, pass `base_url + model +
//! key`. No hardcoded URLs, no per-vendor menu — there are exactly three
//! families, and you always supply the endpoint yourself.
//!
//! ```ignore
//! use harness_models::ApiKind;
//!
//! let m = ApiKind::OpenAI.build("https://dashscope.aliyuncs.com/compatible-mode/v1", "qwen3.7-plus", key);
//! let m = ApiKind::Anthropic.build("https://api.anthropic.com", "claude-opus-4-7", key);
//! let m = ApiKind::Gemini.build("https://generativelanguage.googleapis.com", "gemini-2.5-pro", key);
//! ```

use crate::{AnthropicNative, GeminiNative, LlmConfig, OpenAiCompat};
use harness_core::Model;
use std::sync::Arc;

/// The three wire protocols harness-models speaks. Pass one of these
/// together with `base_url + model + key` to [`ApiKind::build`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum ApiKind {
    /// OpenAI-compatible chat-completions (OpenAI, DeepSeek, Groq, Together,
    /// Ollama, DashScope, vLLM, … — anything OpenAI-shaped).
    OpenAI,
    /// Anthropic-native Messages API.
    Anthropic,
    /// Google Gemini-native generateContent API.
    Gemini,
}

impl ApiKind {
    /// Build a boxed model for this protocol from `base_url + model + key`.
    pub fn build(
        self,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Arc<dyn Model> {
        let base_url = base_url.into();
        let model = model.into();
        let api_key = api_key.into();
        match self {
            ApiKind::OpenAI => Arc::new(OpenAiCompat::with_key(base_url, model, api_key)),
            ApiKind::Anthropic => Arc::new(AnthropicNative::with_key(base_url, model, api_key)),
            ApiKind::Gemini => Arc::new(GeminiNative::with_key(base_url, model, api_key)),
        }
    }

    /// Build directly from a fully-specified [`LlmConfig`] (when you want to
    /// set the logical handle, context window via the adapter, etc.).
    pub fn build_config(self, cfg: LlmConfig) -> Arc<dyn Model> {
        match self {
            ApiKind::OpenAI => Arc::new(OpenAiCompat::new(cfg)),
            ApiKind::Anthropic => Arc::new(AnthropicNative::new(cfg)),
            ApiKind::Gemini => Arc::new(GeminiNative::new(cfg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_builds_a_model_with_the_given_url_and_model() {
        for (kind, prefix) in [
            (ApiKind::OpenAI, "openai-compat"),
            (ApiKind::Anthropic, "anthropic"),
            (ApiKind::Gemini, "gemini"),
        ] {
            let m = kind.build("https://example.test/v1", "some-model", "k");
            // The logical handle encodes the protocol + model; info() proves
            // the config flowed through.
            assert_eq!(m.info().model, "some-model");
            assert!(
                m.info().handle.starts_with(prefix),
                "{kind:?} handle `{}` should start with `{prefix}`",
                m.info().handle
            );
        }
    }
}
