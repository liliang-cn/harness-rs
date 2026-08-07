//! Optional image-generation trait. **Strictly opt-in** — nothing in `Model`,
//! `AgentLoop`, `Hook`, `Guide`, `Sensor`, or `Memory` references this. Code
//! that generates illustrations holds an `Arc<dyn ImageModel>` explicitly;
//! everything else compiles without ever touching this module.
//!
//! Implementations live in `harness-models` (`ChatImageModel`, `OpenAiImage`,
//! `DashScopeImage`).
//!
//! # Output convention: bytes, always
//!
//! Providers return generated images in at least three shapes — an inline
//! `data:image/jpeg;base64,…` URI on the chat channel, a `b64_json` field, or
//! a remote URL that **expires**. Adapters normalise all of them to owned
//! bytes before returning. A caller never learns which transport its provider
//! used, and never holds a handle that can rot.
//!
//! That last part is the whole point: an expiring URL handed to application
//! code is a bug that surfaces a day later, in production, as a broken image.

use crate::b64::base64_encode;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// An image fed *into* generation.
///
/// This is the mechanism behind character consistency across an illustrated
/// sequence: generate page one, then pass it back as a reference for pages
/// two onward so the same character keeps the same face.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ImageRef {
    /// Raw bytes plus a MIME type like `"image/png"`. Adapters encode as the
    /// provider requires.
    Bytes { media_type: String, bytes: Vec<u8> },
    /// Already-encoded standard base64, paired with a MIME type. Saves a
    /// re-encode when the bytes came off the wire in this shape.
    Base64 { media_type: String, base64: String },
    /// A URL the *provider* fetches. Only usable with providers that accept
    /// remote references; others must reject it rather than guess.
    Url(String),
}

impl ImageRef {
    /// Build a reference from raw bytes.
    pub fn bytes(media_type: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        ImageRef::Bytes {
            media_type: media_type.into(),
            bytes: bytes.into(),
        }
    }

    /// Standard-base64 payload for this reference, or `None` for [`ImageRef::Url`]
    /// (nothing local to encode). Lets an adapter render `Bytes` and `Base64`
    /// through one arm.
    pub fn as_base64(&self) -> Option<(&str, std::borrow::Cow<'_, str>)> {
        match self {
            ImageRef::Bytes { media_type, bytes } => {
                Some((media_type, base64_encode(bytes).into()))
            }
            ImageRef::Base64 { media_type, base64 } => Some((media_type, base64.as_str().into())),
            ImageRef::Url(_) => None,
        }
    }
}

/// What to generate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageRequest {
    pub prompt: String,
    /// Provider-native size string (`"1024x1024"`, `"1024*1024"` — they differ,
    /// deliberately not normalised). `None` uses the provider default.
    pub size: Option<String>,
    /// How many images to return. `0` is treated as `1`.
    pub n: u8,
    /// Reference images. An adapter whose provider cannot accept references
    /// MUST return [`ImageError::Unsupported`] rather than dropping them —
    /// a silently ignored reference yields a plausible-looking *wrong* image,
    /// which is the hardest kind of failure to notice.
    pub references: Vec<ImageRef>,
}

impl ImageRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        ImageRequest {
            prompt: prompt.into(),
            size: None,
            n: 1,
            references: Vec::new(),
        }
    }

    pub fn with_size(mut self, size: impl Into<String>) -> Self {
        self.size = Some(size.into());
        self
    }

    pub fn with_n(mut self, n: u8) -> Self {
        self.n = n;
        self
    }

    pub fn with_references(mut self, references: Vec<ImageRef>) -> Self {
        self.references = references;
        self
    }

    /// Requested count, with `0` normalised to `1`.
    pub fn count(&self) -> usize {
        self.n.max(1) as usize
    }
}

/// A generated image, fully materialised.
///
/// `bytes` serialises as standard base64 so a `ModelOutput` carrying images
/// round-trips through session recording and deterministic replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedImage {
    /// MIME type, e.g. `"image/jpeg"`.
    pub media_type: String,
    #[serde(with = "crate::b64::serde_bytes_b64")]
    pub bytes: Vec<u8>,
}

impl GeneratedImage {
    pub fn new(media_type: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        GeneratedImage {
            media_type: media_type.into(),
            bytes: bytes.into(),
        }
    }
}

/// Failures from [`ImageModel::generate`]. Kept separate from `ModelError` for
/// the same reason `EmbedError` is: the surface differs (no thinking, no
/// tools, no streaming) and adapters should not reach across modules.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImageError {
    /// Network / DNS / TLS / timeout.
    Transport(String),
    /// Non-2xx response or a body that did not contain an image.
    Provider(String),
    /// Caller passed something ungeneratable (empty prompt, `n` above the
    /// provider's ceiling).
    BadInput(String),
    /// Provider or model does not support a requested feature — reference
    /// images, `n > 1`, a particular size.
    Unsupported(String),
    /// Rate limit or quota exhaustion. Separate from `Provider` because the
    /// correct response is to back off and retry, not to fail the task.
    RateLimited(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Transport(s) => write!(f, "image transport: {s}"),
            ImageError::Provider(s) => write!(f, "image provider: {s}"),
            ImageError::BadInput(s) => write!(f, "image bad input: {s}"),
            ImageError::Unsupported(s) => write!(f, "image unsupported: {s}"),
            ImageError::RateLimited(s) => write!(f, "image rate limited: {s}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Producer of images from a text prompt.
///
/// Adapters MUST:
/// - Return exactly [`ImageRequest::count`] images, or an error. Never fewer,
///   silently.
/// - Return fully materialised bytes — decode base64, fetch remote URLs. A
///   caller must never receive an expiring handle.
/// - Reject rather than ignore unsupported inputs (see [`ImageRequest::references`]).
/// - Map provider rate-limit responses to [`ImageError::RateLimited`], so
///   retry layers engage instead of failing the task.
#[async_trait]
pub trait ImageModel: Send + Sync + 'static {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>, ImageError>;

    /// Human-readable identifier, e.g. `"cpa:gemini-3.1-flash-image"`. Used in
    /// logs and to tag stored artifacts, so a model swap is detectable after
    /// the fact.
    fn handle(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_normalises_zero_to_one() {
        assert_eq!(ImageRequest::new("x").with_n(0).count(), 1);
        assert_eq!(ImageRequest::new("x").count(), 1);
        assert_eq!(ImageRequest::new("x").with_n(4).count(), 4);
    }

    #[test]
    fn image_ref_bytes_and_base64_agree() {
        let raw = ImageRef::bytes("image/png", b"foobar".to_vec());
        let (mt, b64) = raw.as_base64().unwrap();
        assert_eq!(mt, "image/png");
        assert_eq!(b64.as_ref(), "Zm9vYmFy");

        let pre = ImageRef::Base64 {
            media_type: "image/png".into(),
            base64: "Zm9vYmFy".into(),
        };
        assert_eq!(pre.as_base64().unwrap().1.as_ref(), b64.as_ref());
    }

    #[test]
    fn image_ref_url_has_no_local_payload() {
        assert!(
            ImageRef::Url("https://x/a.png".into())
                .as_base64()
                .is_none()
        );
    }

    #[test]
    fn generated_image_round_trips_through_json() {
        // Non-UTF8 bytes on purpose: a JPEG header is not valid text, and a
        // naive String-based encoding would corrupt it here.
        let img = GeneratedImage::new("image/jpeg", vec![0xff, 0xd8, 0xff, 0xe0, 0x00]);
        let json = serde_json::to_string(&img).unwrap();
        assert!(
            json.contains("\"/9j/4AA=\""),
            "expected base64 payload, got {json}"
        );
        assert_eq!(serde_json::from_str::<GeneratedImage>(&json).unwrap(), img);
    }
}
