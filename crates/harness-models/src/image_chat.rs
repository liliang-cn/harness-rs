//! [`ChatImageModel`] — image generation over an OpenAI-compatible
//! `/chat/completions` endpoint.
//!
//! This is the shape Gemini image models answer in when fronted by an
//! OpenAI-compatible gateway: the prompt goes in as an ordinary user message
//! and the picture comes back on `choices[0].message.images[]`, with `content`
//! left `null`.
//!
//! Verified 2026-08-07 against `gemini-3.1-flash-image` via cpa.superleo.app.
//!
//! ```ignore
//! use harness_models::ChatImageModel;
//! use harness_core::{ImageModel, ImageRequest, ImageRef};
//!
//! let m = ChatImageModel::with_key(base_url, "gemini-3.1-flash-image", key);
//! let cover = m.generate(&ImageRequest::new("a bear on a red bicycle")).await?;
//!
//! // Pages 2..N reuse page 1 as a reference so the bear stays the same bear.
//! let page2 = m.generate(
//!     &ImageRequest::new("the same bear waving goodbye")
//!         .with_references(vec![ImageRef::bytes(&cover[0].media_type, cover[0].bytes.clone())]),
//! ).await?;
//! ```

use crate::media_http::{self, Kind, truncate};
use crate::retry::with_retry_typed;
use async_trait::async_trait;
use harness_core::{GeneratedImage, ImageError, ImageModel, ImageRef, ImageRequest, b64};
use serde_json::{Value, json};

pub struct ChatImageModel {
    base_url: String,
    model: String,
    api_key: String,
    handle: String,
    client: reqwest::Client,
}

impl ChatImageModel {
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
            handle: format!("chat-image:{model}"),
            model,
            api_key: api_key.into(),
            client,
        }
    }

    /// Override the logical handle recorded alongside generated artifacts.
    pub fn with_handle(mut self, handle: impl Into<String>) -> Self {
        self.handle = handle.into();
        self
    }

    async fn call_once(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>, ImageError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = json!({
            "model": self.model,
            "messages": [{"role": "user", "content": build_content(req)}],
        });

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
            return Err(classify_http(status.as_u16(), &text));
        }

        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| ImageError::Provider(format!("parse: {e}; body: {}", truncate(&text))))?;

        // A 200 can still carry an error object — gateways in front of these
        // models routinely answer that way, and treating it as success yields
        // a baffling "no images" error two layers up.
        if let Some(err) = parsed.get("error") {
            return Err(classify_body(&err.to_string()));
        }

        let images = extract_images(&parsed);
        if images.is_empty() {
            return Err(ImageError::Provider(format!(
                "response carried no images; body: {}",
                truncate(&text)
            )));
        }
        Ok(images)
    }
}

/// Build the user message content: the prompt, plus any reference images as
/// vision parts. A bare string is used when there are no references, since
/// some gateways are pickier about the array form than the plain one.
fn build_content(req: &ImageRequest) -> Value {
    if req.references.is_empty() {
        return Value::String(req.prompt.clone());
    }
    let mut parts = vec![json!({"type": "text", "text": req.prompt})];
    for r in &req.references {
        let url = match r {
            ImageRef::Url(u) => u.clone(),
            other => {
                let (media_type, b64s) = other.as_base64().expect("non-Url refs carry a payload");
                format!("data:{media_type};base64,{b64s}")
            }
        };
        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
    }
    Value::Array(parts)
}

/// Pull every image out of `choices[].message.images[].image_url.url`.
///
/// Only inline `data:` URIs are taken here. A remote URL is skipped rather
/// than fetched because this function is synchronous; `call_once` reports
/// "no images" in that case rather than pretending. No provider in the
/// verified set does this — if one shows up, it needs its own adapter, not a
/// silent half-behaviour.
fn extract_images(parsed: &Value) -> Vec<GeneratedImage> {
    parsed
        .get("choices")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c.pointer("/message/images"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|img| img.pointer("/image_url/url").and_then(Value::as_str))
        .filter_map(b64::parse_data_uri)
        .map(|(media_type, bytes)| GeneratedImage { media_type, bytes })
        .collect()
}

/// Map an HTTP status + body onto [`ImageError`], via the shared classifier.
pub(crate) fn classify_http(status: u16, body: &str) -> ImageError {
    let msg = format!("HTTP {status}: {}", truncate(body));
    match media_http::classify(status, body) {
        Kind::RateLimited => ImageError::RateLimited(msg),
        Kind::Transient => ImageError::Transport(msg),
        Kind::Unsupported => ImageError::Unsupported(msg),
        Kind::Permanent => ImageError::Provider(msg),
    }
}

/// Classify an error object that arrived inside a 200 response.
fn classify_body(body: &str) -> ImageError {
    if media_http::looks_rate_limited(body) {
        ImageError::RateLimited(truncate(body))
    } else if media_http::looks_unsupported(body) {
        ImageError::Unsupported(truncate(body))
    } else {
        ImageError::Provider(truncate(body))
    }
}

#[async_trait]
impl ImageModel for ChatImageModel {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>, ImageError> {
        if req.prompt.trim().is_empty() {
            return Err(ImageError::BadInput("prompt is empty".into()));
        }

        let want = req.count();
        let mut out = Vec::with_capacity(want);
        // The chat channel has no `n`: one request yields one image. Loop
        // rather than silently returning fewer than asked for.
        while out.len() < want {
            let batch = with_retry_typed(
                "chat-image:generate",
                |e: &ImageError| matches!(e, ImageError::RateLimited(_) | ImageError::Transport(_)),
                || self.call_once(req),
            )
            .await?;
            out.extend(batch);
        }
        out.truncate(want);
        Ok(out)
    }

    fn handle(&self) -> &str {
        &self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_is_a_bare_string_without_references() {
        let req = ImageRequest::new("a bear");
        assert_eq!(build_content(&req), json!("a bear"));
    }

    #[test]
    fn references_become_vision_parts() {
        let req = ImageRequest::new("the same bear")
            .with_references(vec![ImageRef::bytes("image/png", b"foobar".to_vec())]);
        let c = build_content(&req);
        let arr = c.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,Zm9vYmFy");
    }

    #[test]
    fn url_references_pass_through_unencoded() {
        let req = ImageRequest::new("x")
            .with_references(vec![ImageRef::Url("https://e.com/a.png".into())]);
        let c = build_content(&req);
        assert_eq!(
            c.as_array().unwrap()[1]["image_url"]["url"],
            "https://e.com/a.png"
        );
    }

    /// Body shape captured verbatim from `gemini-3.1-flash-image` via
    /// cpa.superleo.app on 2026-08-07 — note `content: null`.
    #[test]
    fn extracts_image_from_real_response_shape() {
        let body: Value = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {"role":"assistant","content":null,"tool_calls":null,
                        "images":[{"type":"image_url","index":0,
                                   "image_url":{"url":"data:image/jpeg;base64,/9j/4AAQ"}}]},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 19, "completion_tokens": 1534}
            }"#,
        )
        .unwrap();
        let imgs = extract_images(&body);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].media_type, "image/jpeg");
        assert_eq!(&imgs[0].bytes[..4], &[0xff, 0xd8, 0xff, 0xe0]);
    }

    #[test]
    fn text_only_response_extracts_nothing() {
        let body: Value = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#,
        )
        .unwrap();
        assert!(extract_images(&body).is_empty());
    }

    /// Measured: Aliyun answers `Throttling.RateQuota` in ~0.14s. If that maps
    /// to `Provider`, the retry layer never engages and a recoverable stall
    /// becomes a failed job.
    #[test]
    fn aliyun_throttling_maps_to_rate_limited() {
        let body = r#"{"code":"Throttling.RateQuota","message":"Requests rate limit exceeded"}"#;
        assert!(matches!(
            classify_http(400, body),
            ImageError::RateLimited(_)
        ));
        assert!(matches!(classify_body(body), ImageError::RateLimited(_)));
    }

    #[test]
    fn http_statuses_map_to_the_right_variants() {
        assert!(matches!(
            classify_http(429, "slow down"),
            ImageError::RateLimited(_)
        ));
        assert!(matches!(
            classify_http(503, "bad gateway"),
            ImageError::Transport(_)
        ));
        assert!(matches!(
            classify_http(401, "bad key"),
            ImageError::Provider(_)
        ));
        assert!(matches!(
            classify_http(400, "Model x is not supported on /v1/images/generations"),
            ImageError::Unsupported(_)
        ));
    }

    #[tokio::test]
    async fn empty_prompt_is_rejected_without_a_call() {
        let m = ChatImageModel::with_key("http://127.0.0.1:1", "m", "k");
        let err = m.generate(&ImageRequest::new("   ")).await.unwrap_err();
        assert!(matches!(err, ImageError::BadInput(_)));
    }
}
