//! [`DashScopeImage`] — Qwen image models over the native DashScope protocol.
//!
//! # Status: shipped, not live-verified
//!
//! The request path is confirmed. On 2026-08-07, against a Model Studio
//! endpoint, `qwen-image-3.0` on `/api/v1/services/aigc/text2image/image-synthesis`
//! returned `InvalidParameter: url error` — the wrong route — while
//! `/api/v1/services/aigc/multimodal-generation/generation` accepted the
//! request and answered `Throttling.RateQuota` in ~0.14 s. That is a hard
//! account quota, not a rejection of the request, so the route is right and
//! only the quota is missing. Async submission is refused outright on that key
//! (`current user api does not support asynchronous calls`), so this adapter
//! is synchronous only.
//!
//! The **response** parsing is therefore inferred rather than observed. It is
//! modelled on the sibling TTS endpoint, which shares the same
//! `multimodal-generation` envelope and *is* verified — see
//! [`crate::DashScopeSpeech`]. Both `output.results[]` and the
//! `output.choices[].message.content[]` chat-style envelope are accepted,
//! because that pair is what the envelope is documented to vary between.
//!
//! Until someone runs it against an unthrottled key, prefer
//! [`crate::ChatImageModel`], which is verified end to end.

use crate::media_http::{self, Kind, truncate};
use crate::retry::with_retry_typed;
use async_trait::async_trait;
use harness_core::{GeneratedImage, ImageError, ImageModel, ImageRef, ImageRequest, b64};
use serde_json::{Value, json};

pub struct DashScopeImage {
    base_url: String,
    model: String,
    api_key: String,
    handle: String,
    client: reqwest::Client,
}

impl DashScopeImage {
    /// `base_url` is the endpoint root. A `/compatible-mode/v1` suffix is
    /// stripped — the native routes live beside it, not under it.
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
            base_url: normalize_base(&base_url.into()),
            handle: format!("dashscope-image:{model}"),
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
        let url = format!(
            "{}/api/v1/services/aigc/multimodal-generation/generation",
            self.base_url
        );
        let mut content = vec![json!({"text": req.prompt})];
        for r in &req.references {
            let image = match r {
                ImageRef::Url(u) => u.clone(),
                other => {
                    let (mt, b64s) = other.as_base64().expect("non-Url refs carry a payload");
                    format!("data:{mt};base64,{b64s}")
                }
            };
            content.push(json!({"image": image}));
        }

        let mut body = json!({
            "model": self.model,
            "input": {"messages": [{"role": "user", "content": content}]},
        });
        let mut params = serde_json::Map::new();
        if let Some(size) = &req.size {
            params.insert("size".into(), json!(size));
        }
        if req.count() > 1 {
            params.insert("n".into(), json!(req.count()));
        }
        if !params.is_empty() {
            body["parameters"] = Value::Object(params);
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
            let msg = format!("HTTP {}: {}", status.as_u16(), truncate(&text));
            return Err(match media_http::classify(status.as_u16(), &text) {
                Kind::RateLimited => ImageError::RateLimited(msg),
                Kind::Transient => ImageError::Transport(msg),
                Kind::Unsupported => ImageError::Unsupported(msg),
                Kind::Permanent => ImageError::Provider(msg),
            });
        }

        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| ImageError::Provider(format!("parse: {e}; body: {}", truncate(&text))))?;

        // DashScope reports failures with a top-level `code` inside a 200.
        if let Some(code) = parsed.get("code").and_then(Value::as_str) {
            let msg = truncate(&text);
            return Err(if media_http::looks_rate_limited(code) {
                ImageError::RateLimited(msg)
            } else if code.contains("InvalidParameter") || code.contains("AccessDenied") {
                ImageError::Unsupported(msg)
            } else {
                ImageError::Provider(msg)
            });
        }
        Ok(parsed)
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

fn normalize_base(url: &str) -> String {
    let s = url.trim_end_matches('/');
    for suffix in ["/compatible-mode/v1", "/compatible-mode", "/v1"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            return stripped.trim_end_matches('/').to_string();
        }
    }
    s.to_string()
}

/// Collect image references out of either envelope shape, decoding inline data
/// URIs and queueing remote URLs.
pub(crate) fn split_dashscope_images(parsed: &Value) -> (Vec<GeneratedImage>, Vec<String>) {
    let mut urls: Vec<String> = Vec::new();

    // Shape A: output.results[].url
    if let Some(rs) = parsed.pointer("/output/results").and_then(Value::as_array) {
        urls.extend(
            rs.iter()
                .filter_map(|r| r.get("url").and_then(Value::as_str))
                .map(str::to_string),
        );
    }
    // Shape B: output.choices[].message.content[].image
    if let Some(cs) = parsed.pointer("/output/choices").and_then(Value::as_array) {
        for c in cs {
            let Some(parts) = c.pointer("/message/content").and_then(Value::as_array) else {
                continue;
            };
            urls.extend(
                parts
                    .iter()
                    .filter_map(|p| p.get("image").and_then(Value::as_str))
                    .map(str::to_string),
            );
        }
    }

    let mut ready = Vec::new();
    let mut to_fetch = Vec::new();
    for u in urls {
        match b64::parse_data_uri(&u) {
            Some((media_type, bytes)) => ready.push(GeneratedImage { media_type, bytes }),
            None => to_fetch.push(u),
        }
    }
    (ready, to_fetch)
}

#[async_trait]
impl ImageModel for DashScopeImage {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>, ImageError> {
        if req.prompt.trim().is_empty() {
            return Err(ImageError::BadInput("prompt is empty".into()));
        }

        let parsed = with_retry_typed(
            "dashscope-image:generate",
            |e: &ImageError| matches!(e, ImageError::RateLimited(_) | ImageError::Transport(_)),
            || self.call_once(req),
        )
        .await?;

        let (mut images, to_fetch) = split_dashscope_images(&parsed);
        for url in to_fetch {
            images.push(self.fetch(&url).await?);
        }
        if images.is_empty() {
            return Err(ImageError::Provider(
                "response carried no images".to_string(),
            ));
        }
        images.truncate(req.count());
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
    fn base_url_normalisation_matches_the_speech_adapter() {
        assert_eq!(
            normalize_base("https://x.maas.aliyuncs.com/compatible-mode/v1"),
            "https://x.maas.aliyuncs.com"
        );
    }

    #[test]
    fn reads_the_results_envelope() {
        let body: Value = serde_json::from_str(
            r#"{"output":{"results":[{"url":"https://oss/a.png?Expires=1"}]}}"#,
        )
        .unwrap();
        let (ready, to_fetch) = split_dashscope_images(&body);
        assert!(ready.is_empty());
        assert_eq!(to_fetch, vec!["https://oss/a.png?Expires=1"]);
    }

    #[test]
    fn reads_the_chat_style_envelope() {
        let body: Value = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant",
                "content":[{"image":"https://oss/b.png"}]}}]}}"#,
        )
        .unwrap();
        let (_, to_fetch) = split_dashscope_images(&body);
        assert_eq!(to_fetch, vec!["https://oss/b.png"]);
    }

    #[test]
    fn inline_data_uris_decode_without_a_fetch() {
        let body: Value = serde_json::from_str(
            r#"{"output":{"results":[{"url":"data:image/png;base64,Zm9v"}]}}"#,
        )
        .unwrap();
        let (ready, to_fetch) = split_dashscope_images(&body);
        assert!(to_fetch.is_empty());
        assert_eq!(ready[0].bytes, b"foo");
    }

    #[tokio::test]
    async fn empty_prompt_is_rejected_without_a_call() {
        let m = DashScopeImage::with_key("http://127.0.0.1:1", "qwen-image-3.0", "k");
        assert!(matches!(
            m.generate(&ImageRequest::new("")).await.unwrap_err(),
            ImageError::BadInput(_)
        ));
    }
}
