//! A WebSocket client, just big enough to carry the DevTools Protocol.
//!
//! ## Why hand-written
//!
//! CDP is only reachable over WebSocket, so *something* has to speak RFC 6455.
//! The usual answer is `tokio-tungstenite`, which arrives with `tungstenite`,
//! `http`, `httparse`, `sha1`, `rand`, `data-encoding`, `byteorder` and a
//! `utf-8` crate. That is a lot of supply chain for one loopback socket whose
//! peer is a process we spawned ourselves, on a workspace that keeps its
//! dependency list short on purpose.
//!
//! What we actually need is narrow enough to be boring:
//!
//! - one connection, to `127.0.0.1`, so **no TLS**;
//! - the peer is Chrome, so **no server role**, no extensions, no
//!   `permessage-deflate` negotiation;
//! - CDP is JSON, so **text frames** plus continuation, ping and close.
//!
//! That is the ~200 lines below. The parts worth being careful about are the
//! ones with a history of getting people: the three payload-length encodings
//! (a base64 screenshot goes straight past the 64 KiB 16-bit form into the
//! 64-bit one), client-to-server masking, and the fact that a `read()` returns
//! *bytes*, not frames — so the decoder must be a pure function over a buffer
//! that reports "not yet" without consuming anything. It is, and that is what
//! makes it unit-testable without a socket.
//!
//! ## What is deliberately not implemented
//!
//! We do not verify `Sec-WebSocket-Accept`. That header exists so a browser
//! cannot be tricked into treating a non-WebSocket server as one; here the
//! server is a child process at an address it wrote into a file inside a
//! directory we created, and a wrong peer fails at the first JSON parse
//! anyway. Implementing SHA-1 to check it would be ceremony, not security.

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Refuse a frame claiming to be larger than this. Chrome's screenshots are the
/// big ones (base64 PNG of a full page) and land in the low megabytes; 64 MiB
/// is far above anything legitimate and well below "allocate until the box dies".
const MAX_FRAME: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum WsError {
    Io(io::Error),
    /// The peer sent something that is not a frame we can act on.
    Protocol(String),
    /// Orderly or abrupt end of stream.
    Closed,
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Io(e) => write!(f, "websocket io: {e}"),
            WsError::Protocol(s) => write!(f, "websocket protocol: {s}"),
            WsError::Closed => write!(f, "websocket closed"),
        }
    }
}

impl From<io::Error> for WsError {
    fn from(e: io::Error) -> Self {
        WsError::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpCode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl OpCode {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x0 => OpCode::Continuation,
            0x1 => OpCode::Text,
            0x2 => OpCode::Binary,
            0x8 => OpCode::Close,
            0x9 => OpCode::Ping,
            0xA => OpCode::Pong,
            _ => return None,
        })
    }
    fn to_u8(self) -> u8 {
        match self {
            OpCode::Continuation => 0x0,
            OpCode::Text => 0x1,
            OpCode::Binary => 0x2,
            OpCode::Close => 0x8,
            OpCode::Ping => 0x9,
            OpCode::Pong => 0xA,
        }
    }
    fn is_control(self) -> bool {
        matches!(self, OpCode::Close | OpCode::Ping | OpCode::Pong)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub fin: bool,
    pub opcode: OpCode,
    pub payload: Vec<u8>,
}

/// A whole application message, control frames already reassembled away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message {
    Text(String),
    /// Payload to echo back in a pong. The reader cannot write, so it hands
    /// this out and lets the owner of the write half answer.
    Ping(Vec<u8>),
    Close,
}

/// Serialise one client frame. Client frames are always masked (RFC 6455 §5.3)
/// — a server may close the connection on an unmasked one, and Chrome does.
pub(crate) fn encode_frame(opcode: OpCode, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let n = payload.len();
    let mut out = Vec::with_capacity(n + 14);
    out.push(0x80 | opcode.to_u8()); // FIN set: we never fragment outbound.
    if n < 126 {
        out.push(0x80 | n as u8);
    } else if n <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// Try to pull one frame off the front of `buf`.
///
/// `Ok(None)` means "need more bytes" and must leave `buf` untouched — the
/// single most important property here, because TCP will hand us half a frame
/// as often as it hands us three.
pub(crate) fn decode_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let b0 = buf[0];
    let b1 = buf[1];
    if b0 & 0x70 != 0 {
        // RSV bits set without a negotiated extension. We negotiate none, so
        // this is either a bug or a different protocol.
        return Err(WsError::Protocol(format!("reserved bits set: {b0:#04x}")));
    }
    let fin = b0 & 0x80 != 0;
    let opcode = OpCode::from_u8(b0 & 0x0f)
        .ok_or_else(|| WsError::Protocol(format!("unknown opcode {:#x}", b0 & 0x0f)))?;
    let masked = b1 & 0x80 != 0;
    let short_len = (b1 & 0x7f) as u64;

    let mut cursor = 2usize;
    let len = match short_len {
        126 => {
            if buf.len() < cursor + 2 {
                return Ok(None);
            }
            let v = u16::from_be_bytes([buf[cursor], buf[cursor + 1]]) as u64;
            cursor += 2;
            v
        }
        127 => {
            if buf.len() < cursor + 8 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[cursor..cursor + 8]);
            cursor += 8;
            u64::from_be_bytes(b)
        }
        n => n,
    };
    if opcode.is_control() && (len > 125 || !fin) {
        // Control frames are never fragmented and never long (§5.5).
        return Err(WsError::Protocol(
            "oversized or fragmented control frame".into(),
        ));
    }
    if len > MAX_FRAME {
        return Err(WsError::Protocol(format!(
            "frame of {len} bytes exceeds cap"
        )));
    }
    let mask = if masked {
        if buf.len() < cursor + 4 {
            return Ok(None);
        }
        let m = [
            buf[cursor],
            buf[cursor + 1],
            buf[cursor + 2],
            buf[cursor + 3],
        ];
        cursor += 4;
        Some(m)
    } else {
        None
    };
    let len = len as usize;
    if buf.len() < cursor + len {
        return Ok(None);
    }
    let mut payload = buf[cursor..cursor + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload,
        },
        cursor + len,
    )))
}

/// Reassembles frames into messages. Split out from the socket so the
/// fragmentation rules can be tested by feeding it frames directly.
#[derive(Default)]
pub(crate) struct Assembler {
    /// Payload of a fragmented message in progress, plus whether it began as text.
    partial: Option<(Vec<u8>, bool)>,
}

impl Assembler {
    pub(crate) fn push(&mut self, frame: Frame) -> Result<Option<Message>, WsError> {
        match frame.opcode {
            // Control frames may arrive *between* the fragments of a data
            // message, so they must not disturb `partial`.
            OpCode::Ping => return Ok(Some(Message::Ping(frame.payload))),
            OpCode::Pong => return Ok(None),
            OpCode::Close => return Ok(Some(Message::Close)),
            _ => {}
        }
        let (mut acc, is_text) = match frame.opcode {
            OpCode::Continuation => self
                .partial
                .take()
                .ok_or_else(|| WsError::Protocol("continuation with nothing to continue".into()))?,
            OpCode::Text | OpCode::Binary => {
                if self.partial.is_some() {
                    return Err(WsError::Protocol(
                        "new data frame while a fragmented message is open".into(),
                    ));
                }
                (Vec::new(), frame.opcode == OpCode::Text)
            }
            _ => unreachable!("control opcodes returned above"),
        };
        acc.extend_from_slice(&frame.payload);
        if !frame.fin {
            self.partial = Some((acc, is_text));
            return Ok(None);
        }
        if !is_text {
            // CDP is JSON over text frames. A binary frame is not something we
            // can act on, and silently dropping it would hang a pending command.
            return Err(WsError::Protocol("unexpected binary message".into()));
        }
        String::from_utf8(acc)
            .map(|s| Some(Message::Text(s)))
            .map_err(|e| WsError::Protocol(format!("text frame is not utf-8: {e}")))
    }
}

/// Read half: bytes in, messages out.
pub(crate) struct WsReader {
    inner: OwnedReadHalf,
    buf: Vec<u8>,
    asm: Assembler,
}

impl WsReader {
    pub(crate) async fn next_message(&mut self) -> Result<Message, WsError> {
        loop {
            // Drain everything already buffered before touching the socket:
            // one read commonly yields several CDP messages at once.
            while let Some((frame, used)) = decode_frame(&self.buf)? {
                self.buf.drain(..used);
                if let Some(msg) = self.asm.push(frame)? {
                    return Ok(msg);
                }
            }
            let mut chunk = [0u8; 16 * 1024];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                return Err(WsError::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Write half. Held behind a mutex so the command sender and the pong responder
/// can share it without interleaving two frames on the wire.
pub(crate) struct WsWriter {
    inner: OwnedWriteHalf,
}

impl WsWriter {
    pub(crate) async fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        let frame = encode_frame(OpCode::Text, text.as_bytes(), mask_key());
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }

    pub(crate) async fn send_pong(&mut self, payload: &[u8]) -> Result<(), WsError> {
        let frame = encode_frame(OpCode::Pong, payload, mask_key());
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }

    pub(crate) async fn send_close(&mut self) -> Result<(), WsError> {
        // 1000 "normal closure", big-endian, as the first two payload bytes.
        let frame = encode_frame(OpCode::Close, &1000u16.to_be_bytes(), mask_key());
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }
}

/// Open a WebSocket to `host:port` and upgrade at `path`.
///
/// `ws_url` is the `ws://127.0.0.1:<port>/devtools/browser/<uuid>` that Chrome
/// published; we only support the plaintext scheme because DevTools never
/// speaks TLS on loopback.
pub(crate) async fn connect(ws_url: &str) -> Result<(WsReader, WsWriter), WsError> {
    let rest = ws_url
        .strip_prefix("ws://")
        .ok_or_else(|| WsError::Protocol(format!("not a ws:// url: {ws_url}")))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let stream = TcpStream::connect(authority).await?;
    // CDP is a request/response chat; Nagle would add 40ms to every command.
    let _ = stream.set_nodelay(true);
    let (read_half, mut write_half) = stream.into_split();

    let key = handshake_key();
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    write_half.write_all(req.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = WsReader {
        inner: read_half,
        buf: Vec::with_capacity(8 * 1024),
        asm: Assembler::default(),
    };

    // Read exactly up to the end of the response headers. Anything after the
    // blank line is already the first WebSocket frame and must stay buffered.
    let head_end = loop {
        if let Some(i) = find_subslice(&reader.buf, b"\r\n\r\n") {
            break i + 4;
        }
        if reader.buf.len() > 64 * 1024 {
            return Err(WsError::Protocol(
                "upgrade response headers too large".into(),
            ));
        }
        let mut chunk = [0u8; 4096];
        let n = reader.inner.read(&mut chunk).await?;
        if n == 0 {
            return Err(WsError::Protocol("connection closed during upgrade".into()));
        }
        reader.buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&reader.buf[..head_end]).to_string();
    reader.buf.drain(..head_end);

    let status_ok = head
        .lines()
        .next()
        .map(|l| l.contains(" 101"))
        .unwrap_or(false);
    let upgraded = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("upgrade:")
            && l.to_ascii_lowercase().contains("websocket")
    });
    if !status_ok || !upgraded {
        let first = head.lines().next().unwrap_or("").trim().to_string();
        return Err(WsError::Protocol(format!(
            "upgrade refused (`{first}`) — the endpoint is not a DevTools WebSocket"
        )));
    }

    Ok((reader, WsWriter { inner: write_half }))
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A 16-byte nonce, base64'd, for `Sec-WebSocket-Key`.
///
/// The RFC wants this unpredictable so a cache cannot be poisoned into
/// replaying an upgrade. There is no cache on a loopback socket to a child
/// process, so a time-seeded mixer is enough and saves a `rand` dependency.
fn handshake_key() -> String {
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let v = next_random().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    harness_core::b64::base64_encode(&bytes)
}

fn mask_key() -> [u8; 4] {
    let v = next_random().to_le_bytes();
    [v[0], v[1], v[2], v[3]]
}

/// splitmix64 over a process-wide counter seeded from the clock. Not a CSPRNG,
/// and does not need to be — see the two call sites above.
fn next_random() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let seed = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let seed = seed
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip the client mask so a test can read a frame it just encoded.
    fn roundtrip(opcode: OpCode, payload: &[u8]) -> Frame {
        let bytes = encode_frame(opcode, payload, [0xAA, 0xBB, 0xCC, 0xDD]);
        let (frame, used) = decode_frame(&bytes).unwrap().expect("complete frame");
        assert_eq!(used, bytes.len(), "decoder consumed the wrong length");
        frame
    }

    #[test]
    fn encodes_all_three_length_forms() {
        // 7-bit
        let f = roundtrip(OpCode::Text, b"hi");
        assert_eq!(f.payload, b"hi");
        assert!(f.fin);

        // 16-bit: 200 bytes is past the 125 inline cap.
        let mid = vec![b'x'; 200];
        assert_eq!(roundtrip(OpCode::Text, &mid).payload, mid);

        // 64-bit: past u16::MAX, the path a base64 screenshot takes.
        let big = vec![b'y'; 70_000];
        let bytes = encode_frame(OpCode::Binary, &big, [1, 2, 3, 4]);
        assert_eq!(bytes[1] & 0x7f, 127, "should have used the 64-bit length");
        assert_eq!(roundtrip(OpCode::Binary, &big).payload.len(), 70_000);
    }

    #[test]
    fn client_frames_are_masked() {
        let bytes = encode_frame(OpCode::Text, b"secret", [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(bytes[1] & 0x80, 0x80, "MASK bit must be set");
        // The literal must not appear on the wire.
        assert!(find_subslice(&bytes, b"secret").is_none());
    }

    #[test]
    fn decoder_asks_for_more_instead_of_guessing() {
        let bytes = encode_frame(OpCode::Text, &vec![b'z'; 500], [9, 9, 9, 9]);
        // Every strict prefix must be "not yet", and must not consume anything.
        for cut in 0..bytes.len() {
            assert_eq!(
                decode_frame(&bytes[..cut]).unwrap(),
                None,
                "prefix of {cut} bytes decoded as a whole frame"
            );
        }
        assert!(decode_frame(&bytes).unwrap().is_some());
    }

    #[test]
    fn decoder_handles_several_frames_in_one_buffer() {
        let mut buf = Vec::new();
        for s in ["one", "two", "three"] {
            buf.extend_from_slice(&encode_frame(OpCode::Text, s.as_bytes(), [7, 7, 7, 7]));
        }
        let mut got = Vec::new();
        let mut rest = &buf[..];
        while let Some((f, used)) = decode_frame(rest).unwrap() {
            got.push(String::from_utf8(f.payload).unwrap());
            rest = &rest[used..];
        }
        assert_eq!(got, ["one", "two", "three"]);
        assert!(rest.is_empty());
    }

    #[test]
    fn assembler_joins_fragments() {
        let mut asm = Assembler::default();
        assert_eq!(
            asm.push(Frame {
                fin: false,
                opcode: OpCode::Text,
                payload: b"{\"id\":".to_vec()
            })
            .unwrap(),
            None
        );
        // A ping in the middle of a fragmented message is legal and must not
        // clobber what has been accumulated.
        assert_eq!(
            asm.push(Frame {
                fin: true,
                opcode: OpCode::Ping,
                payload: b"pp".to_vec()
            })
            .unwrap(),
            Some(Message::Ping(b"pp".to_vec()))
        );
        assert_eq!(
            asm.push(Frame {
                fin: false,
                opcode: OpCode::Continuation,
                payload: b"7,\"result\"".to_vec()
            })
            .unwrap(),
            None
        );
        let msg = asm
            .push(Frame {
                fin: true,
                opcode: OpCode::Continuation,
                payload: b":{}}".to_vec(),
            })
            .unwrap();
        assert_eq!(msg, Some(Message::Text("{\"id\":7,\"result\":{}}".into())));
    }

    #[test]
    fn assembler_rejects_broken_fragmentation() {
        let mut asm = Assembler::default();
        assert!(
            asm.push(Frame {
                fin: true,
                opcode: OpCode::Continuation,
                payload: vec![]
            })
            .is_err(),
            "continuation with no opener must error"
        );

        let mut asm = Assembler::default();
        asm.push(Frame {
            fin: false,
            opcode: OpCode::Text,
            payload: b"a".to_vec(),
        })
        .unwrap();
        assert!(
            asm.push(Frame {
                fin: true,
                opcode: OpCode::Text,
                payload: b"b".to_vec()
            })
            .is_err(),
            "interleaved data message must error"
        );
    }

    #[test]
    fn multibyte_text_survives_a_split_across_frames() {
        // The split lands inside the UTF-8 encoding of 登, which is exactly how
        // a per-frame `from_utf8` would break on a Chinese page.
        let s = "点击登录".as_bytes();
        let cut = 5;
        let mut asm = Assembler::default();
        asm.push(Frame {
            fin: false,
            opcode: OpCode::Text,
            payload: s[..cut].to_vec(),
        })
        .unwrap();
        let msg = asm
            .push(Frame {
                fin: true,
                opcode: OpCode::Continuation,
                payload: s[cut..].to_vec(),
            })
            .unwrap();
        assert_eq!(msg, Some(Message::Text("点击登录".into())));
    }

    #[test]
    fn rejects_protocol_violations() {
        // Reserved bits set.
        assert!(decode_frame(&[0xF1, 0x00]).is_err());
        // Unknown opcode.
        assert!(decode_frame(&[0x83, 0x00]).is_err());
        // Fragmented control frame.
        assert!(decode_frame(&[0x09, 0x00]).is_err());
        // Control frame with a 126-byte payload claim.
        assert!(decode_frame(&[0x89, 126, 0x01, 0x00]).is_err());
    }

    #[test]
    fn rejects_absurd_frame_length() {
        let mut hdr = vec![0x82, 127];
        hdr.extend_from_slice(&u64::MAX.to_be_bytes());
        match decode_frame(&hdr) {
            Err(WsError::Protocol(m)) => assert!(m.contains("exceeds cap"), "{m}"),
            other => panic!("expected a cap error, got {other:?}"),
        }
    }

    #[test]
    fn nonces_differ_between_calls() {
        let a = handshake_key();
        let b = handshake_key();
        assert_ne!(a, b);
        // 16 bytes base64 with padding.
        assert_eq!(a.len(), 24);
    }
}
