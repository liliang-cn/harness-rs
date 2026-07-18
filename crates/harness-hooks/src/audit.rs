//! [`AuditHook`] — a compliance-oriented audit trail for agent runs.
//!
//! Where `harness-experience` records a run so the agent can *learn* from it,
//! this records a run so a *human* (compliance, a manager, an auditor) can later
//! answer: **who** asked **what**, **what the AI answered**, and **which data
//! sources it touched**. For the SMB process/policy/BI deployment this framework
//! targets, that trail is a hard requirement, not a nicety.
//!
//! It's an ordinary [`Hook`]: attach it to the loop and it writes one JSON line
//! per event to an [`AuditSink`]. Identity (`actor`, `session`) is read from
//! [`Context::metadata`](harness_core::Context) — the serving layer stamps it
//! per request:
//!
//! ```ignore
//! use harness_hooks::audit::{AuditHook, JsonlAuditSink, ACTOR_KEY, SESSION_KEY};
//! use harness_redact::Redactor;
//! use std::sync::Arc;
//!
//! let sink = Arc::new(JsonlAuditSink::new("/var/lib/myapp/audit.jsonl")?);
//! // Redact PII out of the trail (optional but recommended for logs at rest).
//! let hook = Arc::new(AuditHook::new(sink).with_redactor(Redactor::new()));
//!
//! // Per request, in the serving layer:
//! ctx.metadata.insert(ACTOR_KEY.into(), "alice@sales".into());
//! ctx.metadata.insert(SESSION_KEY.into(), session_id.into());
//! ```
//!
//! Records captured: `request` (actor + question), `response` (answer + token
//! usage), `tool_use` (tool + args + ok — the data-source touch), `session_end`.

use harness_core::{Event, Hook, HookOutcome, World};
use harness_redact::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// `Context.metadata` key for the acting user's identity (e.g. `alice@sales`).
pub const ACTOR_KEY: &str = "audit.actor";
/// `Context.metadata` key for the session/conversation id.
pub const SESSION_KEY: &str = "audit.session";
/// `Context.metadata` key for the per-request correlation id. The same id keys
/// the OTel trace and the replay recording, so an audit line points straight at
/// the full trace and a re-runnable recording of that exact request.
pub const REQUEST_KEY: &str = "audit.request";

/// One line in the audit trail. Serialized as a single JSON object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    /// RFC 3339 local timestamp.
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Per-request correlation id (ties this line to its trace + replay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// `request` | `response` | `tool_use` | `session_end`.
    pub kind: String,
    /// Event-specific payload.
    pub detail: Value,
}

/// Where audit records go. Implement this to ship to a DB, syslog, SIEM, etc.
pub trait AuditSink: Send + Sync + 'static {
    fn record(&self, rec: &AuditRecord);
}

/// Appends one JSON object per line to a file, opened in append mode so the
/// trail survives restarts. Cheap and grep-able — the default for single-machine
/// deployments.
pub struct JsonlAuditSink {
    file: Mutex<std::fs::File>,
}

impl JsonlAuditSink {
    /// Open (creating if needed) `path` for appending audit records.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

impl AuditSink for JsonlAuditSink {
    fn record(&self, rec: &AuditRecord) {
        // Audit logging must never break the run: a poisoned lock or IO error is
        // swallowed (and surfaced via `tracing`) rather than propagated.
        match (self.file.lock(), serde_json::to_string(rec)) {
            (Ok(mut f), Ok(line)) => {
                if let Err(e) = writeln!(f, "{line}") {
                    tracing::warn!(target: "harness.audit", error = %e, "audit write failed");
                }
            }
            _ => tracing::warn!(target: "harness.audit", "audit record dropped"),
        }
    }
}

/// Identity captured from the first model call of a run.
#[derive(Default, Clone)]
struct Who {
    actor: Option<String>,
    session: Option<String>,
    request: Option<String>,
}

/// Emits an [`AuditRecord`] per lifecycle event of interest. Build one per run
/// (the serving layer does this per request), attach with `.with_hook(...)`.
pub struct AuditHook {
    sink: Arc<dyn AuditSink>,
    redactor: Option<Redactor>,
    who: Mutex<Who>,
    request_logged: Mutex<bool>,
}

impl AuditHook {
    pub fn new(sink: Arc<dyn AuditSink>) -> Self {
        Self {
            sink,
            redactor: None,
            who: Mutex::new(Who::default()),
            request_logged: Mutex::new(false),
        }
    }

    /// Scrub PII out of recorded text/args before it hits the sink. Recommended
    /// for trails stored at rest. Without it, records are verbatim.
    pub fn with_redactor(mut self, r: Redactor) -> Self {
        self.redactor = Some(r);
        self
    }

    fn scrub_str(&self, s: &str) -> String {
        match &self.redactor {
            Some(r) => r.scrub(s).text,
            None => s.to_string(),
        }
    }

    /// Recursively scrub string leaves of a JSON value (tool args may embed PII).
    fn scrub_value(&self, v: &Value) -> Value {
        match (&self.redactor, v) {
            (Some(r), Value::String(s)) => Value::String(r.scrub(s).text),
            (_, Value::Array(a)) => Value::Array(a.iter().map(|x| self.scrub_value(x)).collect()),
            (_, Value::Object(o)) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), self.scrub_value(x)))
                    .collect(),
            ),
            _ => v.clone(),
        }
    }

    fn emit(&self, kind: &str, detail: Value) {
        let who = self.who.lock().unwrap().clone();
        self.sink.record(&AuditRecord {
            ts: now_rfc3339(),
            actor: who.actor,
            session: who.session,
            request: who.request,
            kind: kind.to_string(),
            detail,
        });
    }
}

/// Wall-clock timestamp for the trail. Real time is what an auditor needs.
fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Process-local monotonic tail so ids are unique even within the same millisecond.
static REQUEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint a per-request correlation id: `req-<unix_millis>-<counter>`. Stamp it
/// into [`REQUEST_KEY`] on the request's `Context.metadata` and the same id keys
/// the audit trail, the OTel trace, and any replay recording — one value ties all
/// three together. The serving layer does this automatically; call it directly
/// when you drive an `AgentLoop` yourself.
pub fn new_request_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = REQUEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("req-{millis}-{n}")
}

impl Hook for AuditHook {
    fn name(&self) -> &str {
        "audit"
    }

    fn matches(&self, _ev: &Event<'_>) -> bool {
        true
    }

    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        match ev {
            Event::PreModel { ctx } => {
                // Capture identity once, from request metadata.
                {
                    let mut who = self.who.lock().unwrap();
                    if who.actor.is_none() {
                        who.actor = ctx
                            .metadata
                            .get(ACTOR_KEY)
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if who.session.is_none() {
                        who.session = ctx
                            .metadata
                            .get(SESSION_KEY)
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if who.request.is_none() {
                        who.request = ctx
                            .metadata
                            .get(REQUEST_KEY)
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
                // Log the question once per run (the task description).
                let mut logged = self.request_logged.lock().unwrap();
                if !*logged {
                    *logged = true;
                    let question = self.scrub_str(&ctx.task.description);
                    self.emit("request", json!({ "question": question }));
                }
            }
            Event::PostModel { out } => {
                let answer = out.text.as_deref().map(|t| self.scrub_str(t));
                self.emit(
                    "response",
                    json!({
                        "answer": answer,
                        "input_tokens": out.usage.input_tokens,
                        "output_tokens": out.usage.output_tokens,
                        "tool_calls": out.tool_calls.len(),
                    }),
                );
            }
            Event::PostToolUse { action, result } => {
                self.emit(
                    "tool_use",
                    json!({
                        "tool": action.tool,
                        "args": self.scrub_value(&action.args),
                        "ok": result.ok,
                    }),
                );
            }
            Event::SessionEnd => {
                self.emit("session_end", json!({}));
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}

// ---------------------------------------------------------------------------
// Tamper-evident trail: hash-chained sink
// ---------------------------------------------------------------------------

/// One envelope in a hash-chained trail: the record plus the chain fields that
/// make deletion / edit / reordering detectable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainedRecord {
    /// 0-based position in the chain.
    pub seq: u64,
    /// The previous envelope's `hash` (`"genesis"` for the first).
    pub prev: String,
    /// `sha256(prev + json(record))`, hex-encoded.
    pub hash: String,
    /// The audited record.
    pub record: AuditRecord,
}

/// Result of [`verify_chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerification {
    /// True if every link verifies and the sequence is unbroken.
    pub ok: bool,
    /// How many envelopes were checked (up to the first break).
    pub checked: usize,
    /// The `seq` at which verification failed, if any.
    pub broken_at: Option<u64>,
}

/// hex(`sha256(concat(parts))`).
fn sha256_hex(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
    }
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Read the last envelope's `(hash, seq)` from an existing trail, if any.
fn read_chain_tail(path: &Path) -> std::io::Result<Option<(String, u64)>> {
    match std::fs::read_to_string(path) {
        Ok(content) => match content.lines().rfind(|l| !l.trim().is_empty()) {
            Some(line) => {
                let rec: ChainedRecord = serde_json::from_str(line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some((rec.hash, rec.seq)))
            }
            None => Ok(None),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Tamper-evident [`AuditSink`]: each record is wrapped in a [`ChainedRecord`]
/// whose `hash` folds in the previous record's hash. Deleting, editing, or
/// reordering any line breaks the chain, which [`verify_chain`] detects. On
/// restart it resumes from the file's last hash, so the chain spans restarts.
///
/// This is integrity (tamper-*evidence*), not secrecy — pair with
/// [`AuditHook::with_redactor`] to also keep PII out of the trail, and store the
/// file on append-only / WORM media for stronger guarantees.
pub struct HashChainSink {
    state: Mutex<ChainState>,
}

struct ChainState {
    file: std::fs::File,
    prev: String,
    seq: u64,
}

impl HashChainSink {
    /// Open (creating if needed) `path`, resuming the chain from its tail.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        let (prev, seq) = match read_chain_tail(path)? {
            Some((hash, last_seq)) => (hash, last_seq + 1),
            None => ("genesis".to_string(), 0),
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            state: Mutex::new(ChainState { file, prev, seq }),
        })
    }
}

impl AuditSink for HashChainSink {
    fn record(&self, rec: &AuditRecord) {
        let Ok(mut st) = self.state.lock() else {
            tracing::warn!(target: "harness.audit", "audit record dropped (poisoned lock)");
            return;
        };
        let Ok(body) = serde_json::to_string(rec) else {
            tracing::warn!(target: "harness.audit", "audit record dropped (serialize)");
            return;
        };
        let hash = sha256_hex(&[&st.prev, &body]);
        let envelope = ChainedRecord {
            seq: st.seq,
            prev: st.prev.clone(),
            hash: hash.clone(),
            record: rec.clone(),
        };
        match serde_json::to_string(&envelope) {
            Ok(line) if writeln!(st.file, "{line}").is_ok() => {
                st.prev = hash;
                st.seq += 1;
            }
            _ => tracing::warn!(target: "harness.audit", "audit chain write failed"),
        }
    }
}

/// Walk a hash-chained trail and confirm every link. Detects any deletion,
/// edit, or reordering. A missing file verifies as an empty (ok) chain.
pub fn verify_chain(path: impl AsRef<Path>) -> std::io::Result<ChainVerification> {
    let content = match std::fs::read_to_string(path.as_ref()) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ChainVerification {
                ok: true,
                checked: 0,
                broken_at: None,
            });
        }
        Err(e) => return Err(e),
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut prev = "genesis".to_string();
    for (i, line) in lines.iter().enumerate() {
        let env: ChainedRecord = serde_json::from_str(line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let body = serde_json::to_string(&env.record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let recomputed = sha256_hex(&[&prev, &body]);
        // seq must equal position (catches deletion/reorder), prev must match the
        // running hash (catches reorder), hash must recompute (catches edits).
        if env.seq != i as u64 || env.prev != prev || env.hash != recomputed {
            return Ok(ChainVerification {
                ok: false,
                checked: i,
                broken_at: Some(env.seq),
            });
        }
        prev = env.hash;
    }
    Ok(ChainVerification {
        ok: true,
        checked: lines.len(),
        broken_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Action, Context, ModelOutput, StopReason, Task, ToolResult, Usage};

    /// Collects records in memory for assertions.
    #[derive(Default)]
    struct VecSink(Mutex<Vec<AuditRecord>>);
    impl AuditSink for VecSink {
        fn record(&self, rec: &AuditRecord) {
            self.0.lock().unwrap().push(rec.clone());
        }
    }

    fn world() -> World {
        harness_context::default_world(std::env::temp_dir().join("audit-test"))
    }

    fn ctx(question: &str, actor: &str) -> Context {
        let mut c = Context::new(Task {
            description: question.into(),
            source: None,
            deadline: None,
        });
        c.metadata.insert(ACTOR_KEY.into(), actor.into());
        c.metadata.insert(SESSION_KEY.into(), "sess-1".into());
        c.metadata.insert(REQUEST_KEY.into(), "req-42".into());
        c
    }

    #[test]
    fn records_request_response_and_tool_use_with_identity() {
        let sink = Arc::new(VecSink::default());
        let hook = AuditHook::new(sink.clone());
        let mut w = world();

        let c = ctx("what is our refund policy?", "alice@sales");
        hook.fire(&Event::PreModel { ctx: &c }, &mut w);
        // A second PreModel must NOT duplicate the request line.
        hook.fire(&Event::PreModel { ctx: &c }, &mut w);

        let out = ModelOutput {
            text: Some("30 days".into()),
            tool_calls: vec![],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 5,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
            reasoning: None,
        };
        hook.fire(&Event::PostModel { out: &out }, &mut w);

        let action = Action {
            tool: "policy_search".into(),
            call_id: "c1".into(),
            args: json!({ "q": "refund" }),
        };
        let result = ToolResult {
            ok: true,
            content: json!("..."),
            trace: None,
        };
        hook.fire(
            &Event::PostToolUse {
                action: &action,
                result: &result,
            },
            &mut w,
        );
        hook.fire(&Event::SessionEnd, &mut w);

        let recs = sink.0.lock().unwrap();
        let kinds: Vec<&str> = recs.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["request", "response", "tool_use", "session_end"]
        );
        // Identity flows onto every record.
        assert!(
            recs.iter()
                .all(|r| r.actor.as_deref() == Some("alice@sales"))
        );
        assert!(recs.iter().all(|r| r.session.as_deref() == Some("sess-1")));
        // The correlation id rides on every record (ties to trace + replay).
        assert!(recs.iter().all(|r| r.request.as_deref() == Some("req-42")));
        assert_eq!(recs[0].detail["question"], "what is our refund policy?");
        assert_eq!(recs[1].detail["answer"], "30 days");
        assert_eq!(recs[2].detail["tool"], "policy_search");
    }

    #[test]
    fn redactor_scrubs_pii_from_the_trail() {
        let sink = Arc::new(VecSink::default());
        let hook = AuditHook::new(sink.clone()).with_redactor(Redactor::new());
        let mut w = world();

        let out = ModelOutput {
            text: Some("reach me at ll_faw@hotmail.com".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            reasoning: None,
        };
        hook.fire(&Event::PostModel { out: &out }, &mut w);

        let recs = sink.0.lock().unwrap();
        let answer = recs[0].detail["answer"].as_str().unwrap();
        assert!(
            !answer.contains("ll_faw@hotmail.com"),
            "email must be redacted: {answer}"
        );
    }

    fn rec(kind: &str) -> AuditRecord {
        AuditRecord {
            ts: "t".into(),
            actor: Some("a".into()),
            session: Some("s".into()),
            request: Some("r".into()),
            kind: kind.into(),
            detail: json!({ "k": kind }),
        }
    }

    #[test]
    fn hash_chain_verifies_and_survives_restart() {
        let path = std::env::temp_dir().join(format!("audit-chain-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let sink = HashChainSink::new(&path).unwrap();
            sink.record(&rec("request"));
            sink.record(&rec("response"));
        } // drop → reopen below resumes the chain

        // Reopen (simulates a restart) and append more.
        {
            let sink = HashChainSink::new(&path).unwrap();
            sink.record(&rec("session_end"));
        }

        let v = verify_chain(&path).unwrap();
        assert!(v.ok, "intact chain must verify: {v:?}");
        assert_eq!(v.checked, 3);
        assert_eq!(v.broken_at, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hash_chain_detects_tampering() {
        let path = std::env::temp_dir().join(format!("audit-tamper-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let sink = HashChainSink::new(&path).unwrap();
            sink.record(&rec("request"));
            sink.record(&rec("response"));
            sink.record(&rec("session_end"));
        }

        // Edit the middle record's payload in place (classic cover-up).
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
        lines[1] = lines[1].replace("\"response\"", "\"totally-different\"");
        std::fs::write(&path, lines.join("\n")).unwrap();

        let v = verify_chain(&path).unwrap();
        assert!(!v.ok, "tampered chain must fail verification");
        assert_eq!(v.broken_at, Some(1), "break should be flagged at seq 1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hash_chain_detects_deletion() {
        let path = std::env::temp_dir().join(format!("audit-del-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let sink = HashChainSink::new(&path).unwrap();
            sink.record(&rec("request"));
            sink.record(&rec("response"));
            sink.record(&rec("session_end"));
        }

        // Delete the middle line — seq/prev continuity breaks.
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        std::fs::write(&path, format!("{}\n{}\n", lines[0], lines[2])).unwrap();

        let v = verify_chain(&path).unwrap();
        assert!(!v.ok, "chain with a deleted record must fail");
        let _ = std::fs::remove_file(&path);
    }
}
