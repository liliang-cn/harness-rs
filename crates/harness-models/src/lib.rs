//! Model trait adapters.
//!
//! Every provider is configured with the same 4-field [`LlmConfig`]:
//!
//! - `name` — user-chosen logical handle (e.g. "prod-fast", "dev-strong")
//! - `base_url` — endpoint root (e.g. `https://api.deepseek.com`)
//! - `api_key` — bearer credential
//! - `model` — wire-protocol model id (e.g. `deepseek-v4-pro`)
//!
//! There are exactly three protocol families — pass `base_url + api_key +
//! model` to whichever matches your endpoint:
//!
//! Or, the single entry point — pass the protocol family plus the same three
//! fields: [`ApiKind::build`]`(kind, base_url, model, key)`.
//!
//! - **OpenAI-compatible** ([`ApiKind::OpenAI`]) — OpenAI, DeepSeek, Groq,
//!   Together, Ollama, DashScope, vLLM, … any OpenAI-shaped endpoint.
//! - **Anthropic-native** ([`ApiKind::Anthropic`]) — the Messages API.
//! - **Gemini-native** ([`ApiKind::Gemini`]) — the generateContent API.
//!
//! You always supply `base_url` yourself — there are no hardcoded vendor URLs.

pub mod anthropic;
pub mod config;
pub mod embed_gemini;
pub mod embed_ollama;
pub mod gemini;
pub mod kind;
pub mod mock;
pub mod openai_compat;
pub mod retry;

pub use anthropic::*;
pub use config::*;
pub use embed_gemini::*;
pub use kind::*;
// `embed_ollama` shares `DEFAULT_MODEL` / `DEFAULT_DIM` names with
// `embed_gemini`; re-export only the adapter type to avoid a glob clash.
pub use embed_ollama::OllamaEmbed;
pub use gemini::*;
pub use mock::*;
pub use openai_compat::*;
