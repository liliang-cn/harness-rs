//! Model trait adapters.
//!
//! Every provider is configured with the same 4-field [`LlmConfig`]:
//!
//! - `name` — user-chosen logical handle (e.g. "prod-fast", "dev-strong")
//! - `base_url` — endpoint root (e.g. `https://api.deepseek.com`)
//! - `api_key` — bearer credential
//! - `model` — wire-protocol model id (e.g. `deepseek-v4-pro`)
//!
//! Pre-configured constructors live in [`providers`].

pub mod anthropic;
pub mod config;
pub mod gemini;
pub mod mock;
pub mod openai_compat;
pub mod providers;
pub mod retry;

pub use anthropic::*;
pub use config::*;
pub use gemini::*;
pub use mock::*;
pub use openai_compat::*;
