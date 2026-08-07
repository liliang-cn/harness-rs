//! [`OpenAiSpeech`] — the standard `/v1/audio/speech` shape.
//!
//! The simplest of the transports: the audio bytes come back as the response
//! body, no envelope, no URL, nothing to decode.
//!
//! Note that "OpenAI-compatible" gateways frequently do *not* implement this
//! route — an Aliyun Model Studio endpoint returned a bare 404 for it on
//! 2026-08-07 while serving `/chat/completions` fine. For that provider use
//! [`crate::DashScopeSpeech`].

use crate::media_http::{self, Kind, truncate};
use crate::retry::with_retry_typed;
use async_trait::async_trait;
use harness_core::{AudioFormat, SpeechAudio, SpeechError, SpeechModel, SpeechRequest};
use serde_json::json;

pub struct OpenAiSpeech {
    base_url: String,
    model: String,
    api_key: String,
    handle: String,
    /// Steers delivery ("read this like a bedtime story"). Supported by
    /// `gpt-4o-mini-tts` and later; older models ignore it.
    instructions: Option<String>,
    client: reqwest::Client,
}

impl OpenAiSpeech {
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
            base_url: base_url.into(),
            handle: format!("openai-speech:{model}"),
            model,
            api_key: api_key.into(),
            instructions: None,
            client,
        }
    }

    pub fn with_handle(mut self, handle: impl Into<String>) -> Self {
        self.handle = handle.into();
        self
    }

    /// Delivery instructions, e.g. `"Read gently, like a bedtime story."`.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    async fn call_once(&self, req: &SpeechRequest) -> Result<Vec<u8>, SpeechError> {
        let url = format!("{}/audio/speech", self.base_url.trim_end_matches('/'));
        let fmt = wire_format(req.format).ok_or_else(|| {
            SpeechError::Unsupported(format!("no wire name for format {:?}", req.format))
        })?;
        let mut body = json!({
            "model": self.model,
            "input": req.text,
            "voice": req.voice,
            "response_format": fmt,
        });
        if let Some(i) = &self.instructions {
            body["instructions"] = json!(i);
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
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let msg = format!("HTTP {}: {}", status.as_u16(), truncate(&text));
            return Err(match media_http::classify(status.as_u16(), &text) {
                Kind::RateLimited => SpeechError::RateLimited(msg),
                Kind::Transient => SpeechError::Transport(msg),
                Kind::Unsupported => SpeechError::Unsupported(msg),
                Kind::Permanent => SpeechError::Provider(msg),
            });
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SpeechError::Transport(format!("read audio: {e}")))?;
        if bytes.is_empty() {
            return Err(SpeechError::Provider("empty audio body".into()));
        }
        Ok(bytes.to_vec())
    }
}

/// The API's name for a container, or `None` if this adapter has no mapping.
///
/// `AudioFormat` is `#[non_exhaustive]`, so a future variant lands here as
/// `None` and surfaces as `Unsupported`. A catch-all arm defaulting to `"wav"`
/// would instead ship the wrong container silently.
fn wire_format(f: AudioFormat) -> Option<&'static str> {
    match f {
        AudioFormat::Wav => Some("wav"),
        AudioFormat::Mp3 => Some("mp3"),
        AudioFormat::Opus => Some("opus"),
        _ => None,
    }
}

#[async_trait]
impl SpeechModel for OpenAiSpeech {
    async fn synthesize(&self, req: &SpeechRequest) -> Result<SpeechAudio, SpeechError> {
        if req.text.trim().is_empty() {
            return Err(SpeechError::BadInput("text is empty".into()));
        }
        if req.voice.trim().is_empty() {
            return Err(SpeechError::BadInput(
                "voice is required (e.g. \"alloy\")".into(),
            ));
        }

        let bytes = with_retry_typed(
            "openai-speech:synthesize",
            |e: &SpeechError| matches!(e, SpeechError::RateLimited(_) | SpeechError::Transport(_)),
            || self.call_once(req),
        )
        .await?;

        Ok(SpeechAudio::new(req.format.media_type(), bytes))
    }

    fn handle(&self) -> &str {
        &self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_formats_are_the_names_the_api_expects() {
        assert_eq!(wire_format(AudioFormat::Wav), Some("wav"));
        assert_eq!(wire_format(AudioFormat::Mp3), Some("mp3"));
        assert_eq!(wire_format(AudioFormat::Opus), Some("opus"));
    }

    #[tokio::test]
    async fn missing_voice_is_rejected_without_a_call() {
        // The API 400s on a missing voice; catching it here costs nothing and
        // names the actual problem.
        let m = OpenAiSpeech::with_key("http://127.0.0.1:1", "gpt-4o-mini-tts", "k");
        let err = m
            .synthesize(&SpeechRequest::new("hello"))
            .await
            .unwrap_err();
        assert!(matches!(err, SpeechError::BadInput(_)));
    }

    #[tokio::test]
    async fn empty_text_is_rejected_without_a_call() {
        let m = OpenAiSpeech::with_key("http://127.0.0.1:1", "gpt-4o-mini-tts", "k");
        let err = m
            .synthesize(&SpeechRequest::new(" ").voice("alloy"))
            .await
            .unwrap_err();
        assert!(matches!(err, SpeechError::BadInput(_)));
    }
}
