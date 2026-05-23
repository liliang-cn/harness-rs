//! Optional embeddings trait. **Strictly opt-in** — nothing in `Model`,
//! `AgentLoop`, `Hook`, `Guide`, `Sensor`, or `Memory` references this. Code
//! that wants semantic search / vector recall holds an `Arc<dyn Embedder>`
//! explicitly; everything else compiles without ever touching this module.
//!
//! Implementations live in `harness-models` (e.g. `GeminiEmbed`,
//! `OpenAiEmbed`). Local/embedded backends (BGE via fastembed-rs etc.) can
//! be added later without changing this trait.
//!
//! Output convention: each input string maps 1:1 to a `Vec<f32>` of length
//! `dim()`. Vectors are returned **unnormalised**; callers that want cosine
//! similarity should L2-normalise both sides themselves (one pass over the
//! vector), or use a helper.

use async_trait::async_trait;
use std::fmt;

/// Failures from an `Embedder::embed` call. Kept separate from `ModelError`
/// because the surfaces differ (no thinking, no tools, no streaming) and we
/// don't want adapters reaching across modules.
#[derive(Debug)]
#[non_exhaustive]
pub enum EmbedError {
    /// Network / DNS / TLS / timeout — anything reqwest can throw.
    Transport(String),
    /// Provider returned a non-2xx response or malformed body.
    Provider(String),
    /// Caller passed something unembeddable (empty input list, oversize batch
    /// for the provider). Surfaced rather than truncating silently.
    BadInput(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbedError::Transport(s) => write!(f, "embed transport: {s}"),
            EmbedError::Provider(s) => write!(f, "embed provider: {s}"),
            EmbedError::BadInput(s) => write!(f, "embed bad input: {s}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Producer of fixed-dimension float vectors for input text. Batched.
///
/// Adapters MUST:
/// - Return exactly `inputs.len()` vectors, in the same order.
/// - Each vector MUST be exactly `dim()` long.
/// - Treat empty `inputs` as `Ok(Vec::new())` (no provider call).
#[async_trait]
pub trait Embedder: Send + Sync + 'static {
    /// Embed a batch of strings. Empty input → empty output, no provider call.
    async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Output dimensionality. Constant per adapter instance.
    fn dim(&self) -> usize;

    /// Human-readable identifier, e.g. `"gemini:text-embedding-004"`. Used
    /// in logs and to tag stored vectors so the schema can detect a dim
    /// change after a model swap.
    fn handle(&self) -> &str;
}

/// Convenience: one-shot single-string embed. Default impl wraps `embed`.
#[async_trait]
pub trait EmbedderExt: Embedder {
    async fn embed_one(&self, input: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed(&[input]).await?;
        out.pop()
            .ok_or_else(|| EmbedError::Provider("empty result for single input".into()))
    }
}

impl<T: Embedder + ?Sized> EmbedderExt for T {}

/// Mutate `v` in place to unit length (L2). No-op on zero vector. Callers
/// that want cosine similarity should normalise both query and corpus
/// once, then use dot product.
pub fn l2_normalize(v: &mut [f32]) {
    let mut s = 0.0f32;
    for &x in v.iter() {
        s += x * x;
    }
    if s <= 0.0 {
        return;
    }
    let inv = 1.0 / s.sqrt();
    for x in v.iter_mut() {
        *x *= inv;
    }
}

/// Plain dot product. With both vectors L2-normalised this equals cosine
/// similarity. Bounded by ±1 in that case; outside that for raw vectors.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut s = 0.0f32;
    for i in 0..n {
        s += a[i] * b[i];
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_unit_length() {
        let mut v = vec![3.0f32, 4.0];
        l2_normalize(&mut v);
        let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((len - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalize_zero_noop() {
        let mut v = vec![0.0f32, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0]);
    }

    #[test]
    fn dot_matches_naive() {
        let a = [1.0f32, 2.0, 3.0];
        let b = [4.0f32, 5.0, 6.0];
        assert!((dot(&a, &b) - (1.0 * 4.0 + 2.0 * 5.0 + 3.0 * 6.0)).abs() < 1e-6);
    }

    #[test]
    fn cosine_via_normalized_dot() {
        let mut a = vec![3.0f32, 4.0];
        let mut b = vec![4.0f32, 3.0];
        l2_normalize(&mut a);
        l2_normalize(&mut b);
        let cos = dot(&a, &b);
        // (3*4+4*3)/((5)(5)) = 24/25 = 0.96
        assert!((cos - 0.96).abs() < 1e-4);
    }
}
