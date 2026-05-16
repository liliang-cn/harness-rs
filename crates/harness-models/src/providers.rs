//! Pre-built [`LlmConfig`] constructors for common providers.
//!
//! These set sensible defaults for `base_url` and a recommended `model`. They
//! return a `LlmConfig` so the caller is free to wrap it in any adapter (today
//! `OpenAiCompat`; future: Anthropic-native, local-llama, etc.).

use crate::LlmConfig;

/// DeepSeek's fast/cheap tier.
pub fn deepseek_flash(api_key: impl Into<String>) -> LlmConfig {
    LlmConfig::new(
        "deepseek-flash",
        "https://api.deepseek.com",
        api_key,
        "deepseek-v4-flash",
    )
}

/// DeepSeek's high-quality tier.
pub fn deepseek_pro(api_key: impl Into<String>) -> LlmConfig {
    LlmConfig::new(
        "deepseek-pro",
        "https://api.deepseek.com",
        api_key,
        "deepseek-v4-pro",
    )
}

/// Local Ollama (or any OpenAI-compatible self-hosted) endpoint.
pub fn ollama(model: impl Into<String>) -> LlmConfig {
    LlmConfig::new("ollama-local", "http://127.0.0.1:43511/v1", "", model)
}

/// Anthropic Sonnet 4.6 — production default for most coding tasks.
pub fn anthropic_sonnet_46(api_key: impl Into<String>) -> LlmConfig {
    LlmConfig::new(
        "anthropic-sonnet-4-6",
        "https://api.anthropic.com",
        api_key,
        "claude-sonnet-4-6",
    )
}

/// Anthropic Opus 4.7 — highest-quality reasoning.
pub fn anthropic_opus_47(api_key: impl Into<String>) -> LlmConfig {
    LlmConfig::new(
        "anthropic-opus-4-7",
        "https://api.anthropic.com",
        api_key,
        "claude-opus-4-7",
    )
}

/// Anthropic Haiku 4.5 — fast/cheap tier.
pub fn anthropic_haiku_45(api_key: impl Into<String>) -> LlmConfig {
    LlmConfig::new(
        "anthropic-haiku-4-5",
        "https://api.anthropic.com",
        api_key,
        "claude-haiku-4-5-20251001",
    )
}
