//! The Chrome DevTools Protocol, as much of it as an agent needs.
//!
//! CDP is JSON-RPC-shaped but not JSON-RPC: a command is
//! `{"id":N,"method":"Page.navigate","params":{…},"sessionId":"…"}` and comes
//! back as either `{"id":N,"result":{…}}` or `{"id":N,"error":{"code":…,"message":…}}`.
//! Anything arriving *without* an `id` is an event
//! (`{"method":"Page.loadEventFired","params":{…}}`).
//!
//! Two properties of that shape drive the design here:
//!
//! 1. **Responses are not ordered.** `Page.navigate` on a slow site and
//!    `Runtime.evaluate` on a fast one will come back in the opposite order they
//!    were sent. So there is no "read the next message" API — every command
//!    registers its id in a table, and a single background pump matches replies
//!    to it. Getting this wrong does not fail loudly; it silently hands one
//!    command's result to another caller, which is the kind of bug that looks
//!    like "the browser is flaky".
//!
//! 2. **Events interleave with responses on the same socket.** A busy page
//!    emits them continuously, and an undrained socket eventually blocks the
//!    replies queued behind them. So the pump runs in its own task and keeps
//!    reading whether or not anyone is waiting, rather than reading on demand.
//!
//! The [`Demux`] type holds all of the correlation logic and touches no IO, so
//! the out-of-order, error, unknown-id and malformed cases are unit tests
//! rather than a stare-at-it-and-hope.

use crate::ws::{self, Message, WsWriter};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, broadcast, oneshot};

/// How long a single CDP command may take before we give up on it. Generous:
/// `Page.navigate` on a cold cache over a slow link is legitimately seconds.
/// Navigation-shaped calls pass their own longer budget.
pub(crate) const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub enum CdpError {
    /// The browser answered, and the answer was "no".
    Remote { code: i64, message: String },
    /// Socket died — almost always because Chrome did.
    Disconnected(String),
    /// We waited and nothing came back.
    Timeout { method: String, waited: Duration },
    /// The peer said something that is not CDP.
    Malformed(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Remote { code, message } => write!(f, "devtools error {code}: {message}"),
            CdpError::Disconnected(s) => write!(f, "devtools connection lost: {s}"),
            CdpError::Timeout { method, waited } => {
                write!(f, "`{method}` timed out after {}s", waited.as_secs())
            }
            CdpError::Malformed(s) => write!(f, "malformed devtools message: {s}"),
        }
    }
}

impl std::error::Error for CdpError {}

/// A CDP event, already routed away from command replies.
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    /// Present for events from an attached target rather than the browser itself.
    pub session_id: Option<String>,
    pub params: Value,
}

/// Command-id allocation and reply correlation. No IO, entirely synchronous —
/// which is the point: this is where the subtle bugs would live, so it is the
/// part that is trivially testable.
#[derive(Default)]
pub(crate) struct Demux {
    next_id: u64,
    pending: HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>,
}

/// What [`Demux::dispatch`] decided an inbound message was.
#[derive(Debug)]
pub(crate) enum Routed {
    /// Matched a waiting command; already delivered.
    Response,
    /// Not a reply — hand it to the event subscribers.
    Event(CdpEvent),
    /// A reply to a command nobody is waiting for any more. Not an error: a
    /// caller that timed out drops its receiver and the late answer lands here.
    Orphan(u64),
}

impl Demux {
    /// Reserve an id and the slot its answer will be delivered into.
    ///
    /// Ids must be unique for the life of the connection, not merely unique
    /// among *outstanding* commands — reusing an id whose caller timed out
    /// would deliver the stale reply to the new caller.
    pub(crate) fn register(&mut self) -> (u64, oneshot::Receiver<Result<Value, CdpError>>) {
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        (id, rx)
    }

    pub(crate) fn dispatch(&mut self, raw: &str) -> Result<Routed, CdpError> {
        let msg: Value = serde_json::from_str(raw)
            .map_err(|e| CdpError::Malformed(format!("{e}: {}", truncate_for_log(raw))))?;

        // `id` is the discriminator between reply and event; nothing else is.
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CdpError::Malformed(format!("neither id nor method: {}", truncate_for_log(raw)))
                })?
                .to_string();
            return Ok(Routed::Event(CdpEvent {
                method,
                session_id: msg
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                params: msg.get("params").cloned().unwrap_or_else(|| json!({})),
            }));
        };

        let Some(tx) = self.pending.remove(&id) else {
            return Ok(Routed::Orphan(id));
        };
        let outcome = if let Some(err) = msg.get("error") {
            Err(CdpError::Remote {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)")
                    .to_string(),
            })
        } else {
            Ok(msg.get("result").cloned().unwrap_or_else(|| json!({})))
        };
        // Receiver gone = caller timed out and walked away; nothing to do.
        let _ = tx.send(outcome);
        Ok(Routed::Response)
    }

    /// Fail every outstanding command. Called once when the socket dies, so
    /// callers get "connection lost" immediately instead of each waiting out
    /// its own timeout.
    pub(crate) fn fail_all(&mut self, why: &str) {
        for (_, tx) in self.pending.drain() {
            let _ = tx.send(Err(CdpError::Disconnected(why.to_string())));
        }
    }

    #[cfg(test)]
    pub(crate) fn outstanding(&self) -> usize {
        self.pending.len()
    }
}

fn truncate_for_log(s: &str) -> String {
    // Byte-index truncation would panic mid-character on a Chinese page title,
    // which has bitten this codebase before; count characters instead.
    let mut out: String = s.chars().take(200).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// A live CDP connection: one socket, one reader pump, many callers.
pub struct CdpClient {
    writer: Arc<Mutex<WsWriter>>,
    demux: Arc<std::sync::Mutex<Demux>>,
    events: broadcast::Sender<CdpEvent>,
    alive: Arc<AtomicBool>,
    pump: tokio::task::JoinHandle<()>,
}

impl CdpClient {
    pub async fn connect(ws_url: &str) -> Result<Self, CdpError> {
        let (mut reader, writer) = ws::connect(ws_url)
            .await
            .map_err(|e| CdpError::Disconnected(e.to_string()))?;

        let writer = Arc::new(Mutex::new(writer));
        let demux: Arc<std::sync::Mutex<Demux>> = Arc::new(std::sync::Mutex::new(Demux::default()));
        // Capacity is a ring: a slow `wait_for` subscriber that falls behind
        // gets Lagged rather than backpressuring the whole protocol. Events we
        // care about (load, lifecycle) are re-checked by polling anyway.
        let (events, _) = broadcast::channel(256);
        let alive = Arc::new(AtomicBool::new(true));

        let pump = tokio::spawn({
            let demux = demux.clone();
            let events = events.clone();
            let alive = alive.clone();
            let writer = writer.clone();
            async move {
                let why = loop {
                    match reader.next_message().await {
                        Ok(Message::Text(raw)) => {
                            let routed = {
                                let mut d = demux.lock().expect("demux mutex");
                                d.dispatch(&raw)
                            };
                            match routed {
                                Ok(Routed::Response) => {}
                                Ok(Routed::Event(ev)) => {
                                    // Err only means "no subscribers"; harmless.
                                    let _ = events.send(ev);
                                }
                                Ok(Routed::Orphan(id)) => {
                                    tracing::trace!(id, "late devtools reply, no waiter");
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "undecodable devtools message");
                                }
                            }
                        }
                        Ok(Message::Ping(payload)) => {
                            let mut w = writer.lock().await;
                            if w.send_pong(&payload).await.is_err() {
                                break "write failed answering ping".to_string();
                            }
                        }
                        Ok(Message::Close) => break "browser closed the connection".to_string(),
                        Err(e) => break e.to_string(),
                    }
                };
                alive.store(false, Ordering::SeqCst);
                demux.lock().expect("demux mutex").fail_all(&why);
                tracing::debug!(reason = %why, "devtools pump stopped");
            }
        });

        Ok(Self {
            writer,
            demux,
            events,
            alive,
            pump,
        })
    }

    /// False once the pump has seen the socket die — i.e. Chrome crashed, was
    /// killed by the OOM killer, or exited. Checked before every action so the
    /// session can be rebuilt instead of timing out command by command.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        self.call_with_timeout(method, params, session_id, DEFAULT_COMMAND_TIMEOUT)
            .await
    }

    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        if !self.is_alive() {
            return Err(CdpError::Disconnected("browser is not running".into()));
        }
        let (id, rx) = self.demux.lock().expect("demux mutex").register();
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            frame["sessionId"] = json!(sid);
        }
        let text = frame.to_string();

        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.send_text(&text).await {
                // Reclaim the slot; otherwise a failed send leaks a pending entry.
                self.demux.lock().expect("demux mutex").pending.remove(&id);
                return Err(CdpError::Disconnected(e.to_string()));
            }
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(CdpError::Disconnected("reply channel dropped".into())),
            Err(_) => {
                self.demux.lock().expect("demux mutex").pending.remove(&id);
                Err(CdpError::Timeout {
                    method: method.to_string(),
                    waited: timeout,
                })
            }
        }
    }

    /// Politely close, then stop the pump. Best-effort: the caller is usually
    /// about to kill the process anyway.
    pub async fn shutdown(&self) {
        {
            let mut w = self.writer.lock().await;
            let _ = w.send_close().await;
        }
        self.alive.store(false, Ordering::SeqCst);
        self.pump.abort();
    }
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        // Without this the pump task outlives the client and keeps a socket and
        // a 256-slot event ring alive for every session ever opened.
        self.pump.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replies_are_matched_by_id_not_by_arrival_order() {
        let mut d = Demux::default();
        let (id_a, rx_a) = d.register();
        let (id_b, rx_b) = d.register();
        let (id_c, rx_c) = d.register();
        assert_eq!((id_a, id_b, id_c), (1, 2, 3));

        // Answer them backwards, which is what a fast page + a slow page does.
        d.dispatch(&json!({"id": 3, "result": {"who": "c"}}).to_string())
            .unwrap();
        d.dispatch(&json!({"id": 1, "result": {"who": "a"}}).to_string())
            .unwrap();
        d.dispatch(&json!({"id": 2, "result": {"who": "b"}}).to_string())
            .unwrap();

        assert_eq!(rx_a.await.unwrap().unwrap()["who"], "a");
        assert_eq!(rx_b.await.unwrap().unwrap()["who"], "b");
        assert_eq!(rx_c.await.unwrap().unwrap()["who"], "c");
        assert_eq!(d.outstanding(), 0);
    }

    #[tokio::test]
    async fn ids_are_never_reused() {
        let mut d = Demux::default();
        let (first, _rx) = d.register();
        d.dispatch(&json!({"id": first, "result": {}}).to_string())
            .unwrap();
        let (second, _rx2) = d.register();
        assert_ne!(
            first, second,
            "a recycled id would deliver a stale reply to a new caller"
        );
    }

    #[tokio::test]
    async fn error_replies_become_errors() {
        let mut d = Demux::default();
        let (id, rx) = d.register();
        d.dispatch(
            &json!({"id": id, "error": {"code": -32000, "message": "Cannot find context"}})
                .to_string(),
        )
        .unwrap();
        match rx.await.unwrap() {
            Err(CdpError::Remote { code, message }) => {
                assert_eq!(code, -32000);
                assert!(message.contains("context"));
            }
            other => panic!("expected a remote error, got {other:?}"),
        }
    }

    #[test]
    fn messages_without_an_id_are_events() {
        let mut d = Demux::default();
        let routed = d
            .dispatch(
                &json!({
                    "method": "Page.frameNavigated",
                    "sessionId": "S1",
                    "params": {"frame": {"url": "https://example.com/"}}
                })
                .to_string(),
            )
            .unwrap();
        match routed {
            Routed::Event(ev) => {
                assert_eq!(ev.method, "Page.frameNavigated");
                assert_eq!(ev.session_id.as_deref(), Some("S1"));
                assert_eq!(ev.params["frame"]["url"], "https://example.com/");
            }
            other => panic!("expected an event, got {other:?}"),
        }
        // An event with no params at all must still route.
        assert!(matches!(
            d.dispatch(&json!({"method": "Page.loadEventFired"}).to_string())
                .unwrap(),
            Routed::Event(_)
        ));
    }

    #[test]
    fn a_late_reply_after_a_timeout_is_ignored_not_fatal() {
        let mut d = Demux::default();
        let (id, rx) = d.register();
        drop(rx); // caller timed out and left
        // Delivering into a dropped slot must not error…
        assert!(matches!(
            d.dispatch(&json!({"id": id, "result": {}}).to_string())
                .unwrap(),
            Routed::Response
        ));
        // …and a genuinely unknown id is an orphan, not a protocol failure.
        assert!(matches!(
            d.dispatch(&json!({"id": 9999, "result": {}}).to_string())
                .unwrap(),
            Routed::Orphan(9999)
        ));
    }

    #[test]
    fn garbage_is_reported_not_swallowed() {
        let mut d = Demux::default();
        assert!(matches!(
            d.dispatch("not json"),
            Err(CdpError::Malformed(_))
        ));
        assert!(matches!(
            d.dispatch(&json!({"params": {}}).to_string()),
            Err(CdpError::Malformed(_))
        ));
        // The log excerpt must survive a multi-byte boundary at char 200.
        let long = "登".repeat(500);
        let msg = match d.dispatch(&long) {
            Err(CdpError::Malformed(m)) => m,
            other => panic!("expected malformed, got {other:?}"),
        };
        assert!(msg.ends_with('…'));
    }

    #[tokio::test]
    async fn a_dead_socket_fails_everyone_waiting() {
        let mut d = Demux::default();
        let (_, rx1) = d.register();
        let (_, rx2) = d.register();
        d.fail_all("chrome exited with signal 9");
        for rx in [rx1, rx2] {
            match rx.await.unwrap() {
                Err(CdpError::Disconnected(m)) => assert!(m.contains("signal 9")),
                other => panic!("expected disconnected, got {other:?}"),
            }
        }
        assert_eq!(d.outstanding(), 0);
    }
}
