//! [`OpenAiEmbed`] — the standard `/v1/embeddings` shape.
//!
//! Wire format: <https://platform.openai.com/docs/api-reference/embeddings>
//!
//! The one most gateways implement, and the one neither [`crate::GeminiEmbed`]
//! nor [`crate::OllamaEmbed`] speaks: Gemini answers on `:batchEmbedContents`
//! and Ollama on `/api/embed`, so a plain OpenAI-compatible endpoint had no
//! adapter here at all.
//!
//! Optional, opt-in. Nothing else in this crate references it.

use crate::media_http::{self, Kind, truncate};
use crate::retry::with_retry_typed;
use async_trait::async_trait;
use harness_core::{EmbedError, Embedder};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// A common default. Override for anything else — the dimension is not
/// discoverable from the API before the first call, so it is declared.
pub const DEFAULT_MODEL: &str = "text-embedding-3-small";
pub const DEFAULT_DIM: usize = 1536;

pub struct OpenAiEmbed {
    base_url: String,
    model: String,
    api_key: String,
    handle: String,
    dim: usize,
    client: reqwest::Client,
}

impl OpenAiEmbed {
    /// Build against `base_url` (ending at `/v1`) with an explicit model and
    /// dimensionality.
    ///
    /// `dim` is declared rather than probed because callers allocate against
    /// it and a stored vector's length is part of the schema — a silent change
    /// after a model swap is a corpus that no longer compares with itself.
    /// [`Embedder::embed`] checks every vector it returns against this and
    /// fails loudly on a mismatch.
    pub fn with_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        dim: usize,
    ) -> Self {
        let model = model.into();
        Self {
            base_url: base_url.into(),
            handle: format!("openai-embed:{model}"),
            model,
            api_key: api_key.into(),
            dim,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("reqwest client builds"),
        }
    }

    pub fn with_handle(mut self, handle: impl Into<String>) -> Self {
        self.handle = handle.into();
        self
    }

    async fn call_once(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&json!({ "model": self.model, "input": inputs }))
            .send()
            .await
            .map_err(|e| EmbedError::Transport(format!("{url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let msg = format!("HTTP {}: {}", status.as_u16(), truncate(&text));
            return Err(match media_http::classify(status.as_u16(), &text) {
                Kind::Transient | Kind::RateLimited => EmbedError::Transport(msg),
                _ => EmbedError::Provider(msg),
            });
        }

        let body: Response = resp
            .json()
            .await
            .map_err(|e| EmbedError::Provider(format!("unreadable body: {e}")))?;

        // Sorted by index, not taken in arrival order. The field exists
        // because the API does not promise the array is ordered, and a corpus
        // silently embedded under the wrong text is not detectable later.
        let mut data = body.data;
        data.sort_by_key(|d| d.index);
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Datum>,
}

#[derive(Deserialize)]
struct Datum {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

#[async_trait]
impl Embedder for OpenAiEmbed {
    async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        if inputs.iter().any(|s| s.trim().is_empty()) {
            // Providers differ on this: some 400, some return a zero vector
            // that ranks equally against everything. Neither is useful.
            return Err(EmbedError::BadInput("an input is empty".into()));
        }

        let out = with_retry_typed(
            "openai-embed:embed",
            |e: &EmbedError| matches!(e, EmbedError::Transport(_)),
            || self.call_once(inputs),
        )
        .await?;

        if out.len() != inputs.len() {
            return Err(EmbedError::Provider(format!(
                "asked for {} vectors, got {}",
                inputs.len(),
                out.len()
            )));
        }
        if let Some(v) = out.iter().find(|v| v.len() != self.dim) {
            return Err(EmbedError::Provider(format!(
                "expected {}-dimensional vectors, got {}",
                self.dim,
                v.len()
            )));
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn handle(&self) -> &str {
        &self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedder() -> OpenAiEmbed {
        OpenAiEmbed::with_key("http://127.0.0.1:1/v1", "m", "k", 4)
    }

    #[tokio::test]
    async fn an_empty_batch_costs_no_call() {
        // The trait requires it, and the endpoint is unreachable here — so a
        // call would fail rather than return empty.
        assert_eq!(embedder().embed(&[]).await.unwrap(), Vec::<Vec<f32>>::new());
    }

    #[tokio::test]
    async fn an_empty_string_is_refused_before_the_call() {
        // Providers differ: some 400, some hand back a zero vector that ranks
        // equally against everything in the corpus.
        for bad in ["", "   ", "\n"] {
            assert!(matches!(
                embedder().embed(&[bad]).await,
                Err(EmbedError::BadInput(_))
            ));
            assert!(matches!(
                embedder().embed(&["fine", bad]).await,
                Err(EmbedError::BadInput(_))
            ));
        }
    }

    #[test]
    fn the_handle_names_the_model_so_stored_vectors_can_be_told_apart() {
        // Two models' vectors are not comparable, so a corpus has to record
        // which one produced it.
        let e = OpenAiEmbed::with_key("http://x/v1", "embeddinggemma:latest", "k", 768);
        assert_eq!(e.handle(), "openai-embed:embeddinggemma:latest");
        assert_eq!(e.dim(), 768);
        assert_eq!(e.with_handle("custom").handle(), "custom");
    }

    #[test]
    fn a_reply_is_ordered_by_index_not_by_arrival() {
        // The API does not promise the array is ordered, and a corpus embedded
        // under the wrong text is not detectable after the fact.
        let body: Response = serde_json::from_str(
            r#"{"data":[{"index":1,"embedding":[9.0]},{"index":0,"embedding":[1.0]}]}"#,
        )
        .unwrap();
        let mut data = body.data;
        data.sort_by_key(|d| d.index);
        assert_eq!(
            data.into_iter().map(|d| d.embedding).collect::<Vec<_>>(),
            vec![vec![1.0], vec![9.0]]
        );
    }
}
