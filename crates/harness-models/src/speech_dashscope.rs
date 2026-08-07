//! [`DashScopeSpeech`] — text-to-speech via Aliyun DashScope / Model Studio.
//!
//! # Why this does not use the OpenAI-compatible endpoint
//!
//! It cannot. Measured 2026-08-07 against a Model Studio MaaS endpoint:
//!
//! - `/compatible-mode/v1/audio/speech` → **404**. The compat shim only serves
//!   `/chat/completions` and `/models`.
//! - `qwen3-tts-flash` through `/compatible-mode/v1/chat/completions` fails
//!   with `Due to invalid text, invalid audio was returned` for *every*
//!   parameter arrangement tried (`audio.voice`, top-level `voice`,
//!   `modalities`, array-form content, `language_type`, stream on and off).
//!   The sibling model `qwen-tts-2025-05-22` gives the real reason:
//!   `Field required: input.text` — the shim never maps `messages` onto
//!   `input.text`.
//!
//! So this adapter speaks the **native** DashScope protocol:
//! `POST /api/v1/services/aigc/multimodal-generation/generation` with
//! `{"model":…, "input":{"text":…, "voice":…}}`. That worked on the first try
//! and returns 24 kHz mono PCM WAV.
//!
//! # The URL that expires
//!
//! DashScope answers with `output.audio.url` — a signed OSS link with
//! `expires_at` roughly 24 hours out. This adapter downloads it before
//! returning, per the [`SpeechModel`] contract. Handing that URL to a caller
//! would look like it worked and break a day later.

use crate::media_http::{self, Kind, truncate};
use crate::retry::with_retry_typed;
use async_trait::async_trait;
use harness_core::{AudioFormat, SpeechAudio, SpeechError, SpeechModel, SpeechRequest};
use serde_json::{Value, json};

/// Default voice. Qwen's voices are named, not coded; `Cherry` is the general
/// -purpose one and reads English fine (verified).
pub const DEFAULT_VOICE: &str = "Cherry";

pub struct DashScopeSpeech {
    base_url: String,
    model: String,
    api_key: String,
    handle: String,
    client: reqwest::Client,
}

impl DashScopeSpeech {
    /// `base_url` is the endpoint root — the part before `/api/v1/…` or
    /// `/compatible-mode/…`. A `/compatible-mode/v1` suffix is tolerated and
    /// stripped, since that is the URL these endpoints are usually handed out
    /// as and silently posting to the wrong path is a confusing 404.
    pub fn with_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let model = model.into();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .expect("reqwest client builds");
        Self {
            base_url: normalize_base(&base_url.into()),
            handle: format!("dashscope-speech:{model}"),
            model,
            api_key: api_key.into(),
            client,
        }
    }

    pub fn with_handle(mut self, handle: impl Into<String>) -> Self {
        self.handle = handle.into();
        self
    }

    async fn synth_once(&self, req: &SpeechRequest) -> Result<String, SpeechError> {
        let url = format!(
            "{}/api/v1/services/aigc/multimodal-generation/generation",
            self.base_url
        );
        let voice = if req.voice.trim().is_empty() {
            DEFAULT_VOICE
        } else {
            req.voice.as_str()
        };
        let mut body = json!({
            "model": self.model,
            "input": {"text": req.text, "voice": voice},
        });
        if let Some(lang) = &req.language {
            body["parameters"] = json!({"language_type": lang});
        }

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| SpeechError::Transport(format!("{url}: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| SpeechError::Transport(format!("read body from {url}: {e}")))?;
        if !status.is_success() {
            return Err(classify_http(status.as_u16(), &text));
        }

        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| SpeechError::Provider(format!("parse: {e}; body: {}", truncate(&text))))?;

        // DashScope reports failures inside a 200 body by way of a top-level
        // `code`. Treating that as success loses the reason entirely.
        if let Some(code) = parsed.get("code").and_then(Value::as_str) {
            return Err(classify_code(code, &text));
        }

        parsed
            .pointer("/output/audio/url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                SpeechError::Provider(format!("no audio url in response: {}", truncate(&text)))
            })
    }

    /// Download the signed OSS object. Unauthenticated on purpose: the
    /// signature is in the query string, and attaching the DashScope key to an
    /// object-store request would leak it to a host named by the response.
    async fn fetch_audio(&self, url: &str) -> Result<Vec<u8>, SpeechError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| SpeechError::Transport(format!("fetch audio: {e}")))?;
        if !resp.status().is_success() {
            return Err(SpeechError::Transport(format!(
                "fetch audio: HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SpeechError::Transport(format!("read audio: {e}")))?;
        if bytes.is_empty() {
            return Err(SpeechError::Provider("audio url returned 0 bytes".into()));
        }
        Ok(bytes.to_vec())
    }
}

/// Strip a `/compatible-mode/v1` (or bare `/v1`) suffix and any trailing slash.
fn normalize_base(url: &str) -> String {
    let s = url.trim_end_matches('/');
    for suffix in ["/compatible-mode/v1", "/compatible-mode", "/v1"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            return stripped.trim_end_matches('/').to_string();
        }
    }
    s.to_string()
}

fn classify_http(status: u16, body: &str) -> SpeechError {
    let msg = format!("HTTP {status}: {}", truncate(body));
    match media_http::classify(status, body) {
        Kind::RateLimited => SpeechError::RateLimited(msg),
        Kind::Transient => SpeechError::Transport(msg),
        Kind::Unsupported => SpeechError::Unsupported(msg),
        Kind::Permanent => SpeechError::Provider(msg),
    }
}

/// Classify a DashScope failure that arrived inside a 200 body.
///
/// `InvalidParameter` and `AccessDenied` are terminal here and both mean "this
/// route cannot do that": the first is what a wrong endpoint returns
/// (`url error, please check url`), the second is what a key without async
/// permission returns. Retrying either just burns time.
fn classify_code(code: &str, body: &str) -> SpeechError {
    let msg = truncate(body);
    if media_http::looks_rate_limited(code) || media_http::looks_rate_limited(body) {
        return SpeechError::RateLimited(msg);
    }
    if code.contains("InvalidParameter") || code.contains("AccessDenied") {
        return SpeechError::Unsupported(msg);
    }
    SpeechError::Provider(msg)
}

#[async_trait]
impl SpeechModel for DashScopeSpeech {
    async fn synthesize(&self, req: &SpeechRequest) -> Result<SpeechAudio, SpeechError> {
        if req.text.trim().is_empty() {
            return Err(SpeechError::BadInput("text is empty".into()));
        }
        // Qwen TTS emits WAV. Returning those bytes for an MP3 request would
        // produce a file that players reject, far from here and with nothing
        // pointing back at this decision.
        if req.format != AudioFormat::Wav {
            return Err(SpeechError::Unsupported(format!(
                "{} emits WAV only; {:?} was requested",
                self.model, req.format
            )));
        }

        let url = with_retry_typed(
            "dashscope-speech:synthesize",
            |e: &SpeechError| matches!(e, SpeechError::RateLimited(_) | SpeechError::Transport(_)),
            || self.synth_once(req),
        )
        .await?;

        let bytes = with_retry_typed(
            "dashscope-speech:fetch",
            |e: &SpeechError| matches!(e, SpeechError::Transport(_)),
            || self.fetch_audio(&url),
        )
        .await?;

        Ok(SpeechAudio::new(AudioFormat::Wav.media_type(), bytes))
    }

    fn handle(&self) -> &str {
        &self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalisation_strips_compat_suffixes() {
        // The endpoint is usually handed out with the compat suffix already on
        // it; posting the native path underneath that gives a bare 404.
        assert_eq!(
            normalize_base("https://x.maas.aliyuncs.com/compatible-mode/v1"),
            "https://x.maas.aliyuncs.com"
        );
        assert_eq!(
            normalize_base("https://x.maas.aliyuncs.com/compatible-mode/v1/"),
            "https://x.maas.aliyuncs.com"
        );
        assert_eq!(
            normalize_base("https://x.maas.aliyuncs.com"),
            "https://x.maas.aliyuncs.com"
        );
    }

    /// Response shape captured verbatim from `qwen3-tts-flash` on 2026-08-07.
    #[test]
    fn parses_audio_url_from_real_response_shape() {
        let body: Value = serde_json::from_str(
            r#"{"output":{"audio":{"data":"","expires_at":1786182913,
                "id":"audio_a28cc856","url":"http://dashscope-result-bj.oss-cn-beijing.aliyuncs.com/x.wav?Expires=1"},
                "finish_reason":"stop"},"usage":{"characters":42}}"#,
        )
        .unwrap();
        let url = body.pointer("/output/audio/url").and_then(Value::as_str);
        assert!(url.unwrap().ends_with("x.wav?Expires=1"));
    }

    #[test]
    fn error_codes_map_to_the_right_variants() {
        assert!(matches!(
            classify_code("Throttling.RateQuota", "rate limit exceeded"),
            SpeechError::RateLimited(_)
        ));
        assert!(matches!(
            classify_code("InvalidParameter", "url error"),
            SpeechError::Unsupported(_)
        ));
        assert!(matches!(
            classify_code(
                "AccessDenied",
                "current user api does not support asynchronous calls"
            ),
            SpeechError::Unsupported(_)
        ));
        assert!(matches!(
            classify_http(500, "boom"),
            SpeechError::Transport(_)
        ));
    }

    #[tokio::test]
    async fn empty_text_is_rejected_without_a_call() {
        let m = DashScopeSpeech::with_key("http://127.0.0.1:1", "qwen3-tts-flash", "k");
        let err = m
            .synthesize(&SpeechRequest::new("  \n "))
            .await
            .unwrap_err();
        assert!(matches!(err, SpeechError::BadInput(_)));
    }

    #[tokio::test]
    async fn non_wav_format_is_refused_rather_than_substituted() {
        let m = DashScopeSpeech::with_key("http://127.0.0.1:1", "qwen3-tts-flash", "k");
        let err = m
            .synthesize(&SpeechRequest::new("hello").format(AudioFormat::Mp3))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SpeechError::Unsupported(_)),
            "must not hand back WAV bytes under an MP3 request"
        );
    }
}
