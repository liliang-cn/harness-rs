//! Minimal standard-base64 (RFC 4648, with padding). Kept dependency-free so
//! `harness-core` stays lean.
//!
//! Three callers now share this: [`crate::Block::Image`] / [`crate::Block::Audio`]
//! encode media *into* a request, and the image/speech adapters decode media
//! *out of* a response (providers hand back `data:image/jpeg;base64,…`).

/// Standard-base64 encode, with padding.
pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Standard-base64 decode. Tolerates missing padding and embedded ASCII
/// whitespace (providers wrap long payloads); rejects any other stray byte
/// rather than silently producing truncated media.
///
/// Also accepts the URL-safe alphabet (`-` / `_`), because some providers
/// return URL-safe base64 without saying so and a decoder that quietly
/// mangles those two characters yields a corrupt-but-plausible image.
pub fn base64_decode(input: &str) -> Result<Vec<u8>, Base64Error> {
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for (i, b) in input.bytes().enumerate() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            other => return Err(Base64Error(i, other)),
        };
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Ok(out)
}

/// A byte that is not valid base64, with its offset. Carries the offending
/// byte so a truncated-payload bug is diagnosable from the log line alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Base64Error(pub usize, pub u8);

impl std::fmt::Display for Base64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid base64 byte {:#04x} at offset {}",
            self.1, self.0
        )
    }
}

impl std::error::Error for Base64Error {}

/// Split a `data:` URI into its media type and decoded bytes.
///
/// Providers return generated images as `data:image/jpeg;base64,/9j/4AAQ…`
/// (verified: CPA gateway, `gemini-3.1-flash-image`). Every adapter that
/// touches an OpenAI-compatible image response needs exactly this, so it
/// lives here instead of being re-derived per adapter.
///
/// Returns `None` when `s` is not a base64 `data:` URI — callers treat that
/// as "this is a plain URL, go fetch it".
pub fn parse_data_uri(s: &str) -> Option<(String, Vec<u8>)> {
    let rest = s.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let meta = meta.strip_suffix(";base64")?;
    let media_type = if meta.is_empty() {
        "application/octet-stream".to_string()
    } else {
        meta.to_string()
    };
    base64_decode(payload).ok().map(|b| (media_type, b))
}

/// `#[serde(with = ...)]` adapter storing a `Vec<u8>` as a base64 string.
///
/// Media payloads must survive session recording and deterministic replay.
/// Serde's default for `Vec<u8>` is a JSON array of integers — correct, but it
/// inflates a 1 MB JPEG into roughly 4 MB of text in every recorded session.
pub mod serde_bytes_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::base64_encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        super::base64_decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn decode_matches_known_vectors() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(
                base64_decode(&base64_encode(s.as_bytes())).unwrap(),
                s.as_bytes()
            );
        }
    }

    #[test]
    fn decode_round_trips_all_byte_values() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
    }

    #[test]
    fn decode_tolerates_missing_padding_and_whitespace() {
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("Zg").unwrap(), b"f"); // no padding
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn decode_accepts_url_safe_alphabet() {
        // 0xfb 0xff encodes as "+/8=" standard, "-_8=" url-safe.
        assert_eq!(
            base64_decode("+/8=").unwrap(),
            base64_decode("-_8=").unwrap()
        );
    }

    #[test]
    fn decode_rejects_stray_bytes() {
        // A '!' is not whitespace and not in either alphabet: erroring beats
        // returning a short buffer that looks like a valid-but-corrupt image.
        assert_eq!(base64_decode("Zm9v!YmFy"), Err(Base64Error(4, b'!')));
    }

    #[test]
    fn data_uri_splits_media_type_and_bytes() {
        let (mt, bytes) = parse_data_uri("data:image/jpeg;base64,Zm9vYmFy").unwrap();
        assert_eq!(mt, "image/jpeg");
        assert_eq!(bytes, b"foobar");
    }

    #[test]
    fn data_uri_rejects_plain_urls() {
        assert!(parse_data_uri("https://example.com/a.png").is_none());
        assert!(parse_data_uri("data:image/png,notbase64").is_none());
    }
}
