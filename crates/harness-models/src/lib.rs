//! Model trait adapters.
//!
//! Every provider is configured with the same 4-field [`LlmConfig`]:
//!
//! - `name` — user-chosen logical handle (e.g. "prod-fast", "dev-strong")
//! - `base_url` — endpoint root (e.g. `https://api.deepseek.com`)
//! - `api_key` — bearer credential
//! - `model` — wire-protocol model id (e.g. `deepseek-v4-pro`)
//!
//! There are exactly three protocol families — pass `base_url + api_key +
//! model` to whichever matches your endpoint:
//!
//! Or, the single entry point — pass the protocol family plus the same three
//! fields: [`ApiKind::build`]`(kind, base_url, model, key)`.
//!
//! - **OpenAI-compatible** ([`ApiKind::OpenAI`]) — OpenAI, DeepSeek, Groq,
//!   Together, Ollama, DashScope, vLLM, … any OpenAI-shaped endpoint.
//! - **Anthropic-native** ([`ApiKind::Anthropic`]) — the Messages API.
//! - **Gemini-native** ([`ApiKind::Gemini`]) — the generateContent API.
//!
//! You always supply `base_url` yourself — there are no hardcoded vendor URLs.

pub mod anthropic;
pub mod config;
pub mod embed_gemini;
pub mod embed_ollama;
pub mod embed_openai;
pub mod gemini;
pub mod image_chat;
pub mod image_dashscope;
pub mod image_openai;
pub mod kind;
pub(crate) mod media_http;
pub mod mock;
pub mod openai_compat;
pub mod retry;
pub mod router;
pub mod speech_dashscope;
pub mod speech_openai;

pub use anthropic::*;
pub use config::*;
pub use embed_gemini::*;
pub use kind::*;
// `embed_ollama` shares `DEFAULT_MODEL` / `DEFAULT_DIM` names with
// `embed_gemini`; re-export only the adapter type to avoid a glob clash.
pub use embed_ollama::OllamaEmbed;
// Same reason as `embed_ollama`: shares `DEFAULT_MODEL` / `DEFAULT_DIM`.
pub use embed_openai::OpenAiEmbed;
pub use gemini::*;
pub use image_chat::*;
pub use image_dashscope::*;
pub use image_openai::*;
pub use mock::*;
pub use openai_compat::*;
pub use router::*;
pub use speech_dashscope::*;
pub use speech_openai::*;

/// Append an HTTP chunk to a text buffer without losing a character that
/// straddles the boundary.
///
/// HTTP chunks land wherever the network puts them. With CJK that is usually
/// inside a character — three bytes each means two of every three split points
/// fall within one. Decoding a chunk on its own therefore fails routinely, and
/// the obvious `if let Ok(s) = from_utf8(&bytes)` discards the WHOLE chunk when
/// it does: not a mangled glyph, every byte of it, with nothing logged.
///
/// `tail` carries the incomplete trailing bytes to the next call.
pub fn push_utf8_chunk(buf: &mut String, tail: &mut Vec<u8>, bytes: &[u8]) {
    let mut pending = std::mem::take(tail);
    pending.extend_from_slice(bytes);
    match std::str::from_utf8(&pending) {
        Ok(s) => buf.push_str(s),
        Err(e) => {
            let good = e.valid_up_to();
            // Safety: `valid_up_to` is by definition the length of the valid prefix.
            buf.push_str(unsafe { std::str::from_utf8_unchecked(&pending[..good]) });
            match e.error_len() {
                // Truncated at the end — the rest of this character is still in
                // flight. Hold it for the next chunk.
                None => tail.extend_from_slice(&pending[good..]),
                // Genuinely invalid bytes: skip them rather than stall forever.
                Some(bad) => {
                    tracing::warn!(bytes = bad, "invalid utf-8 in stream; skipping");
                    tail.extend_from_slice(&pending[good + bad..]);
                }
            }
        }
    }
}

/// The question a grounded search asks on the caller's behalf.
///
/// Shared so every provider phrases it the same way: facts with their numbers
/// and dates, the source URLs, and an explicit "I could not find it" instead of
/// a guess — a search that quietly invents a number is worse than no search.
pub fn grounding_prompt(query: &str) -> String {
    format!(
        "Search the web and answer: {query}\n\nGive the facts with their numbers and dates, \
         say when the data is from, and list the source URLs. If you cannot find it, say so \
         plainly — do not guess."
    )
}

#[doc(hidden)]
pub fn __grounding_task(query: &str) -> harness_core::Task {
    harness_core::Task {
        description: format!("web search: {query}"),
        source: None,
        deadline: None,
    }
}

#[cfg(test)]
mod utf8_chunk_tests {
    use super::push_utf8_chunk;

    #[test]
    fn a_character_split_across_chunks_survives() {
        let text = "你好世界情绪: 开心";
        let bytes = text.as_bytes();
        // Every possible split, including the two-in-three that land inside a
        // character.
        for at in 0..=bytes.len() {
            let mut buf = String::new();
            let mut tail = Vec::new();
            push_utf8_chunk(&mut buf, &mut tail, &bytes[..at]);
            push_utf8_chunk(&mut buf, &mut tail, &bytes[at..]);
            assert_eq!(buf, text, "split at byte {at}");
            assert!(tail.is_empty(), "nothing left over at {at}");
        }
    }

    #[test]
    fn one_byte_at_a_time_still_arrives_whole() {
        let text = "情绪: 开心 — mixed ascii and 中文";
        let mut buf = String::new();
        let mut tail = Vec::new();
        for b in text.as_bytes() {
            push_utf8_chunk(&mut buf, &mut tail, &[*b]);
        }
        assert_eq!(buf, text);
        assert!(tail.is_empty());
    }

    #[test]
    fn genuinely_invalid_bytes_are_skipped_rather_than_stalling_the_stream() {
        let mut buf = String::new();
        let mut tail = Vec::new();
        // 0xFF can never start a character.
        push_utf8_chunk(&mut buf, &mut tail, b"ok\xff");
        push_utf8_chunk(&mut buf, &mut tail, "后面还有".as_bytes());
        assert!(buf.starts_with("ok"), "got {buf:?}");
        assert!(buf.ends_with("后面还有"), "the stream carries on: {buf:?}");
    }
}
