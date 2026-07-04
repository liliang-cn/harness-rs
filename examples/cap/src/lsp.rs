//! A small but real LSP client: a **persistent session** that handshakes once,
//! keeps the language server warm, and re-checks a file on each edit via
//! `didOpen` / `didChange` — the way an editor does it, not a fresh spawn per
//! keystroke. CAP wraps one `LspSession` in a `Sensor` so IDE-grade diagnostics
//! feed back into the agent loop after every edit.
//!
//! Design:
//! - One background reader task drains the server's stdout, answers its
//!   requests (`workspace/configuration`, …) so it doesn't stall, and stores
//!   the latest `publishDiagnostics` per file.
//! - `diagnostics()` sends `didOpen` (first time) or `didChange` (subsequent),
//!   then waits for the *next* diagnostics batch for that file — so an empty
//!   result means "clean", not "not ready yet".
//!
//! This is our own implementation; the reusable, unit-tested core is the
//! JSON-RPC framing codec (`frame` / `parse_message`).

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

/// One diagnostic, flattened from the LSP shape.
#[derive(Debug, Clone)]
pub struct Diag {
    /// LSP severity: 1=Error, 2=Warning, 3=Information, 4=Hint.
    pub severity: u8,
    pub line: u32,
    pub character: u32,
    pub message: String,
    pub source: Option<String>,
}

/// Frame a JSON-RPC message with the LSP `Content-Length` header.
pub fn frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Parse the first complete framed message at the front of `buf`. Returns the
/// message and the number of bytes it consumed, or `None` if `buf` doesn't yet
/// hold a full message.
pub fn parse_message(buf: &[u8]) -> Option<(Value, usize)> {
    let sep = b"\r\n\r\n";
    let header_end = buf.windows(sep.len()).position(|w| w == sep)?;
    let headers = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut len: Option<usize> = None;
    for line in headers.split("\r\n") {
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            len = v.trim().parse().ok();
        }
    }
    let len = len?;
    let body_start = header_end + sep.len();
    if buf.len() < body_start + len {
        return None; // incomplete
    }
    let v = serde_json::from_slice(&buf[body_start..body_start + len]).ok()?;
    Some((v, body_start + len))
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "go" => "go",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" | "cxx" => "cpp",
        "java" => "java",
        _ => "plaintext",
    }
}

fn file_uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn diags_from(params: &Value) -> Vec<Diag> {
    let mut out = Vec::new();
    if let Some(arr) = params["diagnostics"].as_array() {
        for d in arr {
            out.push(Diag {
                severity: d["severity"].as_u64().unwrap_or(1) as u8,
                line: d["range"]["start"]["line"].as_u64().unwrap_or(0) as u32,
                character: d["range"]["start"]["character"].as_u64().unwrap_or(0) as u32,
                message: d["message"].as_str().unwrap_or("").to_string(),
                source: d["source"].as_str().map(|s| s.to_string()),
            });
        }
    }
    out
}

/// State shared between the session handle and its background reader task.
#[derive(Default)]
struct Shared {
    /// uri → latest diagnostics.
    diags: Mutex<HashMap<String, Vec<Diag>>>,
    /// uri → number of `publishDiagnostics` batches received (a monotonically
    /// increasing revision, used to detect a *fresh* batch after an edit).
    revs: Mutex<HashMap<String, u64>>,
}

/// A warm LSP server plus the plumbing to talk to it. Cheap to clone via `Arc`.
pub struct LspSession {
    stdin: Arc<Mutex<ChildStdin>>,
    shared: Arc<Shared>,
    versions: Mutex<HashMap<String, i64>>,
    _child: tokio::process::Child,
    _reader: tokio::task::JoinHandle<()>,
}

impl LspSession {
    /// Spawn `cmd` and complete the LSP handshake. The server stays alive until
    /// this session is dropped (it is killed on drop).
    pub async fn start(cmd: &[String], root: &Path) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(!cmd.is_empty(), "empty lsp command");
        let mut child = tokio::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let mut stdout = child.stdout.take().unwrap();
        let shared = Arc::new(Shared::default());

        // Kick off the handshake.
        stdin
            .lock()
            .await
            .write_all(&frame(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "processId": std::process::id(),
                    "rootUri": file_uri(root),
                    "capabilities": { "textDocument": {
                        "synchronization": { "dynamicRegistration": false },
                        "publishDiagnostics": { "relatedInformation": false }
                    }}
                }
            })))
            .await?;

        // Background reader: answers server requests, records diagnostics.
        let r_stdin = stdin.clone();
        let r_shared = shared.clone();
        let reader = tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 16384];
            loop {
                while let Some((msg, n)) = parse_message(&buf) {
                    buf.drain(..n);
                    handle_incoming(&msg, &r_stdin, &r_shared).await;
                }
                match stdout.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(k) => buf.extend_from_slice(&chunk[..k]),
                }
            }
        });

        Ok(Arc::new(Self {
            stdin,
            shared,
            versions: Mutex::new(HashMap::new()),
            _child: child,
            _reader: reader,
        }))
    }

    /// Re-check `file`: open it the first time, otherwise notify a change, then
    /// wait up to `wait` for the resulting diagnostics batch. An empty result
    /// means the server reported no problems.
    pub async fn diagnostics(&self, file: &Path, wait: Duration) -> Vec<Diag> {
        let uri = file_uri(file);
        let text = tokio::fs::read_to_string(file).await.unwrap_or_default();

        let notice = {
            let mut vers = self.versions.lock().await;
            match vers.get_mut(&uri) {
                Some(v) => {
                    *v += 1;
                    json!({
                        "jsonrpc": "2.0", "method": "textDocument/didChange",
                        "params": {
                            "textDocument": { "uri": uri, "version": *v },
                            "contentChanges": [ { "text": text } ]
                        }
                    })
                }
                None => {
                    vers.insert(uri.clone(), 1);
                    json!({
                        "jsonrpc": "2.0", "method": "textDocument/didOpen",
                        "params": { "textDocument": {
                            "uri": uri, "languageId": language_id(file),
                            "version": 1, "text": text
                        }}
                    })
                }
            }
        };

        let start_gen = self
            .shared
            .revs
            .lock()
            .await
            .get(&uri)
            .copied()
            .unwrap_or(0);
        self.stdin
            .lock()
            .await
            .write_all(&frame(&notice))
            .await
            .ok();

        // Poll for a diagnostics batch newer than the one we had before editing.
        let steps = (wait.as_millis() / 50).max(1);
        for _ in 0..steps {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let cur = self
                .shared
                .revs
                .lock()
                .await
                .get(&uri)
                .copied()
                .unwrap_or(0);
            if cur > start_gen {
                break;
            }
        }
        self.shared
            .diags
            .lock()
            .await
            .get(&uri)
            .cloned()
            .unwrap_or_default()
    }
}

/// React to one server→client message: complete the handshake, answer requests
/// so the server doesn't stall, and store diagnostics.
async fn handle_incoming(msg: &Value, stdin: &Arc<Mutex<ChildStdin>>, shared: &Arc<Shared>) {
    let method = msg.get("method").and_then(|m| m.as_str());
    let has_id = msg.get("id").is_some();

    if msg.get("id") == Some(&json!(1)) && method.is_none() {
        // initialize response → send `initialized`.
        let _ = stdin
            .lock()
            .await
            .write_all(&frame(&json!({
                "jsonrpc": "2.0", "method": "initialized", "params": {}
            })))
            .await;
    } else if has_id && method.is_some() {
        // A server→client *request* (e.g. gopls' workspace/configuration): it
        // blocks until answered. Reply with benign defaults.
        let result = if method == Some("workspace/configuration") {
            let items = msg["params"]["items"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(1);
            Value::Array(vec![json!({}); items])
        } else {
            Value::Null
        };
        let _ = stdin
            .lock()
            .await
            .write_all(&frame(&json!({
                "jsonrpc": "2.0", "id": msg["id"], "result": result
            })))
            .await;
    } else if method == Some("textDocument/publishDiagnostics")
        && let Some(uri) = msg["params"]["uri"].as_str()
    {
        let d = diags_from(&msg["params"]);
        shared.diags.lock().await.insert(uri.to_string(), d);
        *shared.revs.lock().await.entry(uri.to_string()).or_insert(0) += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_then_parse_roundtrips() {
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        let bytes = frame(&msg);
        let header = std::str::from_utf8(&bytes[..40]).unwrap();
        assert!(header.starts_with("Content-Length: "));
        let (parsed, n) = parse_message(&bytes).unwrap();
        assert_eq!(parsed, msg);
        assert_eq!(n, bytes.len());
    }

    #[test]
    fn parse_returns_none_when_incomplete() {
        let bytes = frame(&json!({"a": "bcdef"}));
        assert!(parse_message(&bytes[..bytes.len() - 3]).is_none());
        assert!(parse_message(b"Content-Length: 10\r\n").is_none());
    }

    #[test]
    fn parse_consumes_only_the_first_of_two_concatenated_messages() {
        let a = frame(&json!({"n": 1}));
        let b = frame(&json!({"n": 2}));
        let mut both = a.clone();
        both.extend_from_slice(&b);
        let (m1, n1) = parse_message(&both).unwrap();
        assert_eq!(m1["n"], 1);
        assert_eq!(n1, a.len());
        let (m2, _) = parse_message(&both[n1..]).unwrap();
        assert_eq!(m2["n"], 2);
    }

    #[test]
    fn diags_from_flattens_lsp_shape() {
        let params = json!({
            "uri": "file:///x.go",
            "diagnostics": [
                {"severity": 1, "message": "type error",
                 "source": "compiler",
                 "range": {"start": {"line": 4, "character": 8}}}
            ]
        });
        let d = diags_from(&params);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, 1);
        assert_eq!(d[0].line, 4);
        assert_eq!(d[0].character, 8);
        assert_eq!(d[0].source.as_deref(), Some("compiler"));
    }

    #[tokio::test]
    async fn start_fails_cleanly_for_a_missing_server() {
        let err = LspSession::start(
            &["cap-no-such-language-server-xyz".to_string()],
            std::path::Path::new("."),
        )
        .await;
        assert!(err.is_err(), "missing server binary must error, not panic");
    }
}
