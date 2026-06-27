//! Ollama embeddings adapter — implements [`harness_core::Embedder`] against a
//! local Ollama server's OpenAI-compatible `/v1/embeddings` endpoint.
//!
//! Wire format: <https://github.com/ollama/ollama/blob/main/docs/openai.md#v1embeddings>
//!
//! Optional, opt-in. The chat adapters in this crate do not reference this
//! module; users that want fully-local vector search wire `OllamaEmbed`
//! themselves. Pair it with
//! `OpenAiCompat::with_key("http://127.0.0.1:11434/v1", ..)` for an offline
//! chat + embeddings stack.

use harness_core::{EmbedError, Embedder};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default model — Google's `embeddinggemma`, served by Ollama. 768-dim.
pub const DEFAULT_MODEL: &str = "embeddinggemma";
pub const DEFAULT_DIM: usize = 768;

/// Ollama embeddings client. Constructed once and shared via `Arc<dyn Embedder>`.
pub struct OllamaEmbed {
    /// OpenAI-compat root, e.g. `http://127.0.0.1:11434/v1` (trailing slash trimmed).
    base_url: String,
    model: String,
    handle: String,
    dim: usize,
    client: reqwest::Client,
}

impl OllamaEmbed {
    /// Build against an OpenAI-compat `base_url` (e.g.
    /// `http://127.0.0.1:11434/v1`) using the default `embeddinggemma` model
    /// at 768-dim.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_model(base_url.into(), DEFAULT_MODEL.to_string(), DEFAULT_DIM)
    }

    /// Build with an explicit model id and dimensionality. Pass `dim` matching
    /// the model — `embeddinggemma` = 768. Mismatches won't fail at this layer;
    /// they surface later when callers read `dim()` to allocate buffers.
    pub fn with_model(base_url: String, model: String, dim: usize) -> Self {
        let handle = format!("ollama:{model}");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            handle,
            dim,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[derive(Serialize)]
struct EmbedReq<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbedResp {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

#[derive(Deserialize)]
struct ProviderError {
    error: ProviderErrorInner,
}

#[derive(Deserialize)]
struct ProviderErrorInner {
    message: String,
}

#[async_trait::async_trait]
impl Embedder for OllamaEmbed {
    async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/embeddings", self.base_url);
        let req = EmbedReq {
            model: &self.model,
            input: inputs.to_vec(),
        };

        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| EmbedError::Transport(e.to_string()))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| EmbedError::Transport(e.to_string()))?;
        if !status.is_success() {
            // Best-effort surfacing of the structured provider error.
            let msg = serde_json::from_str::<ProviderError>(&body)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| body.chars().take(400).collect());
            return Err(EmbedError::Provider(format!("HTTP {status}: {msg}")));
        }

        let mut parsed: EmbedResp = serde_json::from_str(&body).map_err(|e| {
            EmbedError::Provider(format!(
                "decode: {e} — body: {}",
                body.chars().take(400).collect::<String>()
            ))
        })?;

        if parsed.data.len() != inputs.len() {
            return Err(EmbedError::Provider(format!(
                "expected {} embeddings, got {}",
                inputs.len(),
                parsed.data.len()
            )));
        }

        // OpenAI-compat responses carry an `index`; sort to guarantee order.
        parsed.data.sort_by_key(|d| d.index);
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn handle(&self) -> &str {
        &self.handle
    }
}
