use serde::{Deserialize, Serialize};

/// Uniform 4-field LLM configuration consumed by every model adapter.
///
/// The shape is intentionally minimal — anything more elaborate (rate limits,
/// retry policy, etc.) belongs on the adapter struct, not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// User-chosen logical handle. Surfaces in logs, metrics, and selectors.
    pub name:     String,
    /// API root (e.g. `https://api.deepseek.com`). No trailing slash required.
    pub base_url: String,
    /// Bearer token. Read from env in user code; never hard-code.
    pub api_key:  String,
    /// Wire-protocol model id (e.g. `deepseek-v4-pro`, `gpt-5.1`, `claude-opus-4-7`).
    pub model:    String,
}

impl LlmConfig {
    /// Build a config inline.
    ///
    /// ```ignore
    /// LlmConfig::new("prod-flash", "https://api.deepseek.com", env::var("DEEPSEEK_API_KEY")?, "deepseek-v4-flash")
    /// ```
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            name:     name.into(),
            base_url: base_url.into(),
            api_key:  api_key.into(),
            model:    model.into(),
        }
    }
}
