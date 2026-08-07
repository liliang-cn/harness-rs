//! [`OpenAiImage`] — the standard `/v1/images/generations` shape.
//!
//! Use this for providers that implement OpenAI's images endpoint properly
//! (OpenAI itself, and gateways that route a real image model there). For
//! Gemini image models behind an OpenAI-compatible gateway, use
//! [`crate::ChatImageModel`] instead — they answer on `/chat/completions` and
//! the images endpoint rejects them outright.
//!
//! Handles both response encodings: `data[].b64_json` (OpenAI's default) and
//! `data[].url` (fetched here, so the caller gets bytes either way).

use crate::retry::with_retry_typed;
use async_trait::async_trait;
use harness_core::{GeneratedImage, ImageError, ImageModel, ImageRequest, b64};
use serde_json::{Value, json};

pub struct OpenAiImage {
    base_url: String,
    model: String,
    api_key: String,
    handle: String,
    client: reqwest::Client,
}

impl OpenAiImage {
    pub fn with_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let model = model.into();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("reqwest client builds");
        Self {
            base_url: base_url.into(),
            handle: format!("openai-image:{model}"),
            model,
            api_key: api_key.into(),
            client,
        }
    }

    pub fn with_handle(mut self, handle: impl Into<String>) -> Self {
        self.handle = handle.into();
        self
    }

    async fn call_once(&self, req: &ImageRequest) -> Result<Value, ImageError> {
        let url = format!("{}/images/generations", self.base_url.trim_end_matches('/'));
        let mut body = json!({
            "model": self.model,
            "prompt": req.prompt,
            "n": req.count(),
        });
        if let Some(size) = &req.size {
            body["size"] = json!(size);
        }

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ImageError::Transport(format!("{url}: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ImageError::Transport(format!("read body from {url}: {e}")))?;
        if !status.is_success() {
            return Err(crate::image_chat::classify_http(status.as_u16(), &text));
        }
        serde_json::from_str(&text).map_err(|e| ImageError::Provider(format!("parse: {e}")))
    }

    async fn fetch(&self, url: &str) -> Result<GeneratedImage, ImageError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ImageError::Transport(format!("fetch image: {e}")))?;
        if !resp.status().is_success() {
            return Err(ImageError::Transport(format!(
                "fetch image: HTTP {}",
                resp.status()
            )));
        }
        let media_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "image/png".to_string());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ImageError::Transport(format!("read image: {e}")))?;
        Ok(GeneratedImage {
            media_type,
            bytes: bytes.to_vec(),
        })
    }
}

/// Split `data[]` into decoded images and URLs still needing a fetch.
///
/// `b64_json` carries no MIME type, so the container is read off the decoded
/// bytes rather than assumed.
pub(crate) fn split_openai_data(parsed: &Value) -> (Vec<GeneratedImage>, Vec<String>) {
    let mut ready = Vec::new();
    let mut to_fetch = Vec::new();
    for item in parsed
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if let Some(b) = item.get("b64_json").and_then(Value::as_str)
            && let Ok(bytes) = b64::base64_decode(b)
        {
            ready.push(GeneratedImage {
                media_type: sniff_media_type(&bytes).to_string(),
                bytes,
            });
            continue;
        }
        if let Some(u) = item.get("url").and_then(Value::as_str) {
            to_fetch.push(u.to_string());
        }
    }
    (ready, to_fetch)
}

/// Identify a container from its magic bytes. Cheaper and more honest than
/// assuming: a caller writing `.png` onto JPEG bytes produces a file that some
/// tools open and others reject.
fn sniff_media_type(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, 0x50, 0x4e, 0x47, ..] => "image/png",
        [0xff, 0xd8, 0xff, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        _ => "application/octet-stream",
    }
}

#[async_trait]
impl ImageModel for OpenAiImage {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>, ImageError> {
        if req.prompt.trim().is_empty() {
            return Err(ImageError::BadInput("prompt is empty".into()));
        }
        if !req.references.is_empty() {
            // `/images/generations` has no reference-image input; that is
            // `/images/edits`, a different request shape. Rejecting beats
            // generating a confident-looking image that ignores the reference.
            return Err(ImageError::Unsupported(
                "/images/generations takes no reference images; use an edits-capable adapter"
                    .into(),
            ));
        }

        let parsed = with_retry_typed(
            "openai-image:generate",
            |e: &ImageError| matches!(e, ImageError::RateLimited(_) | ImageError::Transport(_)),
            || self.call_once(req),
        )
        .await?;

        let (mut images, to_fetch) = split_openai_data(&parsed);
        for url in to_fetch {
            images.push(self.fetch(&url).await?);
        }

        if images.len() != req.count() {
            return Err(ImageError::Provider(format!(
                "asked for {} image(s), provider returned {}",
                req.count(),
                images.len()
            )));
        }
        Ok(images)
    }

    fn handle(&self) -> &str {
        &self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_b64_json_and_sniffs_the_container() {
        // "/9j/4AAQ" → FF D8 FF E0 00 10, a JPEG header.
        let body: Value = serde_json::from_str(r#"{"data":[{"b64_json":"/9j/4AAQ"}]}"#).unwrap();
        let (ready, to_fetch) = split_openai_data(&body);
        assert!(to_fetch.is_empty());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].media_type, "image/jpeg");
        assert_eq!(&ready[0].bytes[..3], &[0xff, 0xd8, 0xff]);
    }

    #[test]
    fn queues_urls_for_fetching() {
        let body: Value =
            serde_json::from_str(r#"{"data":[{"url":"https://e.com/a.png"}]}"#).unwrap();
        let (ready, to_fetch) = split_openai_data(&body);
        assert!(ready.is_empty());
        assert_eq!(to_fetch, vec!["https://e.com/a.png"]);
    }

    #[test]
    fn sniffs_known_containers() {
        assert_eq!(
            sniff_media_type(&[0x89, 0x50, 0x4e, 0x47, 0x0d]),
            "image/png"
        );
        assert_eq!(sniff_media_type(&[0xff, 0xd8, 0xff, 0xe1]), "image/jpeg");
        assert_eq!(sniff_media_type(b"GIF89a"), "image/gif");
        assert_eq!(sniff_media_type(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(sniff_media_type(b"nope"), "application/octet-stream");
    }

    #[tokio::test]
    async fn references_are_refused_not_ignored() {
        let m = OpenAiImage::with_key("http://127.0.0.1:1", "gpt-image-1", "k");
        let req = ImageRequest::new("x")
            .with_references(vec![harness_core::ImageRef::Url("https://e/a.png".into())]);
        assert!(matches!(
            m.generate(&req).await.unwrap_err(),
            ImageError::Unsupported(_)
        ));
    }
}
