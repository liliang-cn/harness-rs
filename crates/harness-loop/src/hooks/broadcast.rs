//! [`BroadcastHook`] — the run's lifecycle as a live, multi-subscriber feed.
//!
//! [`crate::hooks::audit::AuditHook`] answers "what happened", durably, after the
//! fact. This answers "what is happening", now, to anyone listening: a web UI
//! streaming the answer token by token, a monitor watching tool calls, an
//! ops dashboard counting budget warnings. Same seam — the loop's lifecycle
//! [`Event`]s — different consumer contract:
//!
//! - **Fan-out, not a pipe.** `tokio::sync::broadcast`: every subscriber gets
//!   every message, subscribers come and go freely, none of them can see the
//!   others. (`harness-serve` streams over an `mpsc` because a chat response
//!   has exactly one reader; a feed does not.)
//! - **The loop is never blocked.** `broadcast::Sender::send` is synchronous
//!   and non-blocking; a slow subscriber overflows *its own* buffer and gets
//!   `RecvError::Lagged(n)` — it lost `n` messages, the loop lost nothing.
//! - **Best-effort by design.** That lag semantics is why this is a UI/
//!   monitoring feed and not an audit trail: compliance stays on `AuditHook`,
//!   whose sink never drops a line. Point a subscriber at your audit pipeline
//!   only as a live *mirror* of the authoritative trail.
//!
//! Events are projected to owned, serializable [`BroadcastEvent`]s (the
//! lifecycle [`Event`] borrows from the loop and cannot leave it). Payloads
//! are bounded on purpose: tool results are broadcast *as shaped for the
//! context* (the loop fires `PostToolUse` after its ceiling/spill guard), and
//! model images are omitted.
//!
//! ```ignore
//! use crate::hooks::broadcast::BroadcastHook;
//!
//! let hook = std::sync::Arc::new(BroadcastHook::new(1024));
//! let mut ui = hook.subscribe();        // → WebSocket / SSE
//! let mut ops = hook.subscribe();       // → metrics, alerting
//!
//! let agent = AgentLoop::new(model).with_hook(hook.clone());
//!
//! tokio::spawn(async move {
//!     loop {
//!         match ui.recv().await {
//!             Ok(ev) => send_to_client(serde_json::to_string(&ev).unwrap()),
//!             Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
//!                 tracing::warn!(missed = n, "ui subscriber lagged");
//!             }
//!             Err(_) => break, // hook dropped
//!         }
//!     }
//! });
//! ```

use harness_core::{Event, Hook, HookOutcome, World};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;

/// One lifecycle moment, owned and serializable — safe to hand to any
/// subscriber on any task, and already the JSON shape a frontend wants.
#[derive(Debug, Clone, Serialize)]
pub struct BroadcastEvent {
    /// Monotonic per-hook sequence number. Gaps tell a lagged subscriber
    /// exactly how much it missed.
    pub seq: u64,
    /// Milliseconds since the epoch, from the run's [`World`] clock.
    pub at_ms: i64,
    /// Conversation id, when the host stamped one on the `World`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Acting user, same source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The lifecycle event's name (`"PostModel"`, `"ModelTokenDelta"`, …).
    pub event: &'static str,
    /// Event-specific fields; see [`project`] for each shape.
    pub payload: Value,
}

/// Forwards lifecycle events into a `tokio::sync::broadcast` channel.
/// Construct once, [`subscribe`](Self::subscribe) as many times as you like,
/// attach to the loop with `with_hook`.
pub struct BroadcastHook {
    tx: broadcast::Sender<BroadcastEvent>,
    seq: AtomicU64,
    with_deltas: bool,
}

impl BroadcastHook {
    /// `capacity` is each subscriber's ring buffer: how far one may fall
    /// behind before it starts losing (its own) messages.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self {
            tx,
            seq: AtomicU64::new(0),
            with_deltas: true,
        }
    }

    /// Drop per-token `ModelTokenDelta` events from the feed. For consumers
    /// that only want turn-level events (audit mirrors, metrics), deltas are
    /// most of the volume and none of the signal.
    pub fn without_deltas(mut self) -> Self {
        self.with_deltas = false;
        self
    }

    /// A new independent subscriber. Receives events sent *after* this call.
    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastEvent> {
        self.tx.subscribe()
    }

    /// The sender half, for handing to a server task that wants to create
    /// subscribers later without holding the hook itself.
    pub fn sender(&self) -> broadcast::Sender<BroadcastEvent> {
        self.tx.clone()
    }
}

/// What each event contributes to the feed. `None` = not broadcast (inbound
/// context like `PreModel` is the model's business, and `Custom` data is the
/// host's — it can broadcast its own).
fn project(ev: &Event<'_>) -> Option<Value> {
    Some(match ev {
        // The one the question is usually about: every model return value.
        Event::PostModel { out } => json!({
            "text": out.text,
            "reasoning": out.reasoning,
            "tool_calls": out.tool_calls.iter().map(|c| json!({
                "id": c.id, "name": c.name, "args": c.args,
            })).collect::<Vec<_>>(),
            "usage": out.usage,
            "stop_reason": out.stop_reason,
            // images omitted: base64 payloads do not belong on a feed.
        }),
        Event::ModelTokenDelta { text } => json!({ "text": text }),
        Event::PreToolUse { action } => json!({
            "tool": action.tool, "call_id": action.call_id, "args": action.args,
        }),
        // Fired after the loop's result shaping, so this is bounded: the
        // context-sized payload, never the raw flood.
        Event::PostToolUse { action, result } => json!({
            "tool": action.tool, "call_id": action.call_id,
            "ok": result.ok, "content": result.content,
        }),
        Event::SessionStart { source } => json!({ "source": source }),
        Event::SessionEnd => json!({}),
        Event::TaskCompleted => json!({}),
        Event::SubagentStart { name } => json!({ "name": name }),
        Event::SubagentReport { status } => json!({ "status": status }),
        Event::PostCompact {
            stage,
            before,
            after,
        } => json!({
            "stage": format!("{stage:?}"), "tokens_before": before, "tokens_after": after,
        }),
        Event::BudgetWarning { ratio } => json!({ "ratio": ratio }),
        Event::Error { message } => json!({ "message": message }),
        Event::Heartbeat { iter } => json!({ "iter": iter }),
        _ => return None,
    })
}

impl Hook for BroadcastHook {
    fn name(&self) -> &str {
        "broadcast"
    }

    fn matches(&self, ev: &Event<'_>) -> bool {
        if !self.with_deltas && matches!(ev, Event::ModelTokenDelta { .. }) {
            return false;
        }
        // Cheap variant check; the projection itself runs in `fire`.
        project(ev).is_some()
    }

    fn fire(&self, ev: &Event<'_>, world: &mut World) -> HookOutcome {
        if let Some(payload) = project(ev) {
            // `send` fails only when nobody is subscribed — which is fine, a
            // feed with no listeners costs one refused send.
            let _ = self.tx.send(BroadcastEvent {
                seq: self.seq.fetch_add(1, Ordering::Relaxed),
                at_ms: world.clock.now_ms(),
                session: world.session.as_ref().map(|s| s.id.clone()),
                actor: world
                    .session
                    .as_ref()
                    .filter(|s| !s.actor.is_empty())
                    .map(|s| s.actor.clone()),
                event: ev.name(),
                payload,
            });
        }
        HookOutcome::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{ModelOutput, StopReason, ToolCall, Usage};

    fn world() -> World {
        let ws = std::env::temp_dir().join(format!("bcast-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        harness_context::default_world(&ws)
    }

    fn model_out() -> ModelOutput {
        ModelOutput {
            text: Some("the answer".into()),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                args: serde_json::json!({"path": "a.txt"}),
            }],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn every_subscriber_gets_every_model_return() {
        let hook = BroadcastHook::new(16);
        let mut ui = hook.subscribe();
        let mut audit = hook.subscribe();
        let mut w = world();

        let out = model_out();
        hook.fire(&Event::PostModel { out: &out }, &mut w);

        for rx in [&mut ui, &mut audit] {
            let ev = rx.recv().await.unwrap();
            assert_eq!(ev.event, "PostModel");
            assert_eq!(ev.payload["text"], "the answer");
            assert_eq!(ev.payload["tool_calls"][0]["name"], "read_file");
            assert_eq!(ev.payload["usage"]["input_tokens"], 10);
        }
    }

    #[tokio::test]
    async fn a_slow_subscriber_lags_alone_and_the_loop_never_blocks() {
        let hook = BroadcastHook::new(2); // tiny ring: lag is easy to force
        let mut slow = hook.subscribe();
        let mut w = world();

        let out = model_out();
        for _ in 0..5 {
            // Would deadlock here if send could block on a full subscriber.
            hook.fire(&Event::PostModel { out: &out }, &mut w);
        }

        // The slow reader is told exactly that it lagged — then keeps reading.
        match slow.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert_eq!(n, 3),
            other => panic!("expected Lagged, got {other:?}"),
        }
        assert_eq!(slow.recv().await.unwrap().seq, 3);
        assert_eq!(slow.recv().await.unwrap().seq, 4);
    }

    #[tokio::test]
    async fn without_deltas_keeps_turn_level_events_only() {
        let hook = BroadcastHook::new(16).without_deltas();
        assert!(!hook.matches(&Event::ModelTokenDelta { text: "x" }));
        let out = model_out();
        assert!(hook.matches(&Event::PostModel { out: &out }));
    }
}
