//! Provider endpoint constants.
//!
//! That's all this module is — just URLs. Pass the model name yourself when
//! you construct the adapter; we don't enumerate it for you.
//!
//! ```ignore
//! use harness_models::{OpenAiCompat, AnthropicNative, providers::*};
//!
//! // OpenAI-compatible — any endpoint, you pick the model
//! let m = OpenAiCompat::with_key(DEEPSEEK, "deepseek-v4-pro", key);
//! let m = OpenAiCompat::with_key(OPENAI,   "gpt-5",           key);
//! let m = OpenAiCompat::with_key(GROQ,     "llama-3.3-70b",   key);
//! let m = OpenAiCompat::with_key(OLLAMA,   "qwen2.5-coder:7b", "");
//!
//! // Anthropic-native — URL is hardcoded inside the adapter
//! let m = AnthropicNative::with_key("claude-opus-4-7", key);
//! ```

pub const ANTHROPIC: &str = "https://api.anthropic.com";
pub const OPENAI:    &str = "https://api.openai.com/v1";
pub const DEEPSEEK:  &str = "https://api.deepseek.com";
pub const GROQ:      &str = "https://api.groq.com/openai/v1";
pub const TOGETHER:  &str = "https://api.together.xyz/v1";
pub const OLLAMA:    &str = "http://127.0.0.1:11434/v1";
