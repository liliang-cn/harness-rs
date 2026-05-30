//! Gemini embeddings adapter — implements [`harness_core::Embedder`] for
//! `text-embedding-004` (default) and any future Gemini embedding model.
//!
//! Wire format: <https://ai.google.dev/api/embeddings#method:-models.batchembedcontents>
//!
//! Optional, opt-in. The chat adapters in this crate do not reference this
//! module; users that want vector search wire `GeminiEmbed` themselves.

use harness_core::{EmbedError, Embedder};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default model — Gemini's current GA embedding model. Configurable
/// output dim (768 / 1536 / 3072); we pin to 768 for storage thrift.
pub const DEFAULT_MODEL: &str = "gemini-embedding-001";
pub const DEFAULT_DIM: usize = 768;

/// Gemini embeddings client. Constructed once and shared via `Arc<dyn Embedder>`.
pub struct GeminiEmbed {
    model: String,
    api_key: String,
    handle: String,
    dim: usize,
    client: reqwest::Client,
}

impl GeminiEmbed {
    /// Build with a Gemini API key and the default `text-embedding-004` model.
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self::with_model(DEFAULT_MODEL.to_string(), api_key.into(), DEFAULT_DIM)
    }

    /// Build with an explicit model id and dimensionality. Pass `dim` matching
    /// the model — `text-embedding-004` = 768. Mismatches won't fail at this
    /// layer; they surface later when callers read `dim()` to allocate buffers.
    pub fn with_model(model: String, api_key: String, dim: usize) -> Self {
        let handle = format!("gemini:{model}");
        Self {
            model,
            api_key,
            handle,
            dim,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[derive(Serialize)]
struct BatchReq<'a> {
    requests: Vec<EmbedRequest<'a>>,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: String,
    content: Content<'a>,
    /// Gemini truncates/PCA's the model's native dim down to this size.
    /// Required for `gemini-embedding-001`; ignored by older models.
    #[serde(
        rename = "outputDimensionality",
        skip_serializing_if = "Option::is_none"
    )]
    output_dim: Option<usize>,
}

#[derive(Serialize)]
struct Content<'a> {
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct Part<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct BatchResp {
    embeddings: Vec<EmbeddingValue>,
}

#[derive(Deserialize)]
struct EmbeddingValue {
    values: Vec<f32>,
}

#[derive(Deserialize)]
struct ProviderError {
    error: ProviderErrorBody,
}

#[derive(Deserialize)]
struct ProviderErrorBody {
    message: String,
}

#[async_trait::async_trait]
impl Embedder for GeminiEmbed {
    async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        // Gemini accepts up to 100 requests per batch. Larger callers split
        // upstream so we don't silently truncate.
        if inputs.len() > 100 {
            return Err(EmbedError::BadInput(format!(
                "batch too large: {} (max 100 per Gemini batchEmbedContents)",
                inputs.len()
            )));
        }

        let model_path = format!("models/{}", self.model);
        let dim = Some(self.dim);
        let req = BatchReq {
            requests: inputs
                .iter()
                .map(|t| EmbedRequest {
                    model: model_path.clone(),
                    content: Content {
                        parts: vec![Part { text: t }],
                    },
                    output_dim: dim,
                })
                .collect(),
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:batchEmbedContents?key={}",
            self.model, self.api_key
        );

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

        let parsed: BatchResp = serde_json::from_str(&body).map_err(|e| {
            EmbedError::Provider(format!(
                "decode: {e} — body: {}",
                body.chars().take(400).collect::<String>()
            ))
        })?;

        if parsed.embeddings.len() != inputs.len() {
            return Err(EmbedError::Provider(format!(
                "expected {} embeddings, got {}",
                inputs.len(),
                parsed.embeddings.len()
            )));
        }

        Ok(parsed.embeddings.into_iter().map(|e| e.values).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn handle(&self) -> &str {
        &self.handle
    }
}
