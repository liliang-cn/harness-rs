//! Optional text-to-speech trait. **Strictly opt-in** — nothing in `Model`,
//! `AgentLoop`, `Hook`, `Guide`, `Sensor`, or `Memory` references this.
//!
//! Implementations live in `harness-models` (`DashScopeSpeech`, `OpenAiSpeech`).
//!
//! # Output convention: bytes, always
//!
//! As with [`crate::image`], adapters return owned audio bytes. Some providers
//! answer with a signed URL that expires (DashScope's is good for ~24 h);
//! those adapters fetch before returning. Handing application code a URL that
//! rots is not an abstraction, it is a deferred outage.
//!
//! # No silent format substitution
//!
//! If the caller asks for MP3 and the provider only emits WAV, the adapter
//! returns [`SpeechError::Unsupported`]. It does not hand back WAV bytes under
//! an MP3 assumption — that failure surfaces much later, as a file some player
//! refuses to open, with nothing pointing back at the cause.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Container format for synthesised audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AudioFormat {
    /// Uncompressed PCM in a RIFF container. The most widely supported, and
    /// the only thing some providers emit.
    #[default]
    Wav,
    Mp3,
    Opus,
}

impl AudioFormat {
    /// MIME type for this container.
    pub fn media_type(self) -> &'static str {
        match self {
            AudioFormat::Wav => "audio/wav",
            AudioFormat::Mp3 => "audio/mpeg",
            AudioFormat::Opus => "audio/opus",
        }
    }

    /// Conventional file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Wav => "wav",
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Opus => "opus",
        }
    }
}

/// What to say, and how.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRequest {
    pub text: String,
    /// Provider-native voice id (`"Cherry"`, `"alloy"`). Deliberately not
    /// normalised across providers: there is no honest mapping, and a
    /// wrong-but-plausible voice is worse than an explicit provider-specific
    /// string.
    pub voice: String,
    /// Provider-native language hint (`"English"`, `"Chinese"`). `None` lets
    /// the provider auto-detect.
    pub language: Option<String>,
    pub format: AudioFormat,
}

impl SpeechRequest {
    pub fn new(text: impl Into<String>) -> Self {
        SpeechRequest {
            text: text.into(),
            voice: String::new(),
            language: None,
            format: AudioFormat::default(),
        }
    }

    pub fn voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = voice.into();
        self
    }

    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    pub fn format(mut self, format: AudioFormat) -> Self {
        self.format = format;
        self
    }
}

/// Synthesised audio, fully materialised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechAudio {
    /// MIME type, e.g. `"audio/wav"`.
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl SpeechAudio {
    pub fn new(media_type: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        SpeechAudio {
            media_type: media_type.into(),
            bytes: bytes.into(),
        }
    }
}

/// Failures from [`SpeechModel::synthesize`].
#[derive(Debug)]
#[non_exhaustive]
pub enum SpeechError {
    /// Network / DNS / TLS / timeout — including a failure to fetch a
    /// provider-returned audio URL.
    Transport(String),
    /// Non-2xx response, or a body with no audio in it.
    Provider(String),
    /// Caller passed something unspeakable (empty text, missing voice where
    /// the provider requires one).
    BadInput(String),
    /// Provider cannot honour a requested parameter — a format it does not
    /// emit, an unknown voice.
    Unsupported(String),
    /// Rate limit or quota exhaustion. Retry, do not fail.
    RateLimited(String),
}

impl fmt::Display for SpeechError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpeechError::Transport(s) => write!(f, "speech transport: {s}"),
            SpeechError::Provider(s) => write!(f, "speech provider: {s}"),
            SpeechError::BadInput(s) => write!(f, "speech bad input: {s}"),
            SpeechError::Unsupported(s) => write!(f, "speech unsupported: {s}"),
            SpeechError::RateLimited(s) => write!(f, "speech rate limited: {s}"),
        }
    }
}

impl std::error::Error for SpeechError {}

/// Turns text into audio.
///
/// Adapters MUST:
/// - Return audio in `req.format`, or [`SpeechError::Unsupported`]. Never a
///   different format under the requested one's name.
/// - Return fully materialised bytes. An adapter whose provider answers with a
///   URL fetches it before returning.
/// - Treat empty `text` as [`SpeechError::BadInput`], not as a provider call.
/// - Map rate-limit responses to [`SpeechError::RateLimited`].
#[async_trait]
pub trait SpeechModel: Send + Sync + 'static {
    async fn synthesize(&self, req: &SpeechRequest) -> Result<SpeechAudio, SpeechError>;

    /// Human-readable identifier, e.g. `"maas:qwen3-tts-flash"`.
    ///
    /// Callers that cache synthesised audio should key on
    /// `(handle(), voice, format, text)` — not on text alone. Two voices
    /// saying the same sentence are different artifacts, and a cache that
    /// conflates them serves the wrong narrator.
    fn handle(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_media_types_and_extensions() {
        assert_eq!(AudioFormat::Wav.media_type(), "audio/wav");
        assert_eq!(AudioFormat::Mp3.media_type(), "audio/mpeg");
        assert_eq!(AudioFormat::Opus.extension(), "opus");
    }

    #[test]
    fn default_format_is_wav() {
        assert_eq!(SpeechRequest::new("hi").format, AudioFormat::Wav);
    }

    #[test]
    fn builder_sets_fields() {
        let r = SpeechRequest::new("Little Bear rode down the hill.")
            .voice("Cherry")
            .language("English")
            .format(AudioFormat::Mp3);
        assert_eq!(r.voice, "Cherry");
        assert_eq!(r.language.as_deref(), Some("English"));
        assert_eq!(r.format, AudioFormat::Mp3);
    }
}
