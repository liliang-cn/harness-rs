//! Capture every conversation turn into a [`Memory`] as it happens.
//!
//! Backend-agnostic: it writes `MemoryEntry`s, so it lands in *any* `Memory`
//! (JSONL, SQLite recall, a CortexDB-backed brain, …). Pair with
//! `harness-cortexdb` and turns flow into CortexDB; schedule
//! [`CortexdbMemory::consolidate`](../../harness_cortexdb) periodically to
//! distill them into the knowledge graph.
//!
//! The framework's [`Hook`] is synchronous but `Memory::write` is async, so the
//! hook only *enqueues* onto a channel; a background task owns the `Memory` and
//! drains it. This keeps the agent loop non-blocking.
//!
//! ```ignore
//! let (recorder, rx) = TranscriptRecorder::new("sess-42");
//! spawn_transcript_writer(rx, memory.clone());     // background async writer
//! recorder.note_user(&user_input);                 // the user half-turn
//! AgentLoop::new(model)
//!     .with_hook(std::sync::Arc::new(recorder))    // assistant + tool turns
//!     .run(task, &mut world).await?;
//! ```

use harness_core::{Event, Hook, HookOutcome, Memory, MemoryEntry, ModelOutput, World};
use std::sync::Arc;
use tokio::sync::mpsc;

/// One captured turn, ready to persist. `role` is `user | assistant | tool`.
#[derive(Debug, Clone)]
pub struct CapturedTurn {
    pub session: String,
    pub role: String,
    pub content: String,
}

/// A [`Hook`] that enqueues assistant + tool turns onto a channel. Construct with
/// [`TranscriptRecorder::new`], which also returns the receiver to hand to
/// [`spawn_transcript_writer`].
pub struct TranscriptRecorder {
    tx: mpsc::UnboundedSender<CapturedTurn>,
    session: String,
}

impl TranscriptRecorder {
    /// Create a recorder for `session`, returning it plus the receiver its
    /// background writer drains.
    pub fn new(session: impl Into<String>) -> (Self, mpsc::UnboundedReceiver<CapturedTurn>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                session: session.into(),
            },
            rx,
        )
    }

    /// Record the user half-turn. The hook only sees the model's output and tool
    /// results, so the app enqueues the user's message explicitly (once, before
    /// or right after `run`).
    pub fn note_user(&self, text: impl Into<String>) {
        self.enqueue("user", text.into());
    }

    fn enqueue(&self, role: &str, content: String) {
        if content.trim().is_empty() {
            return;
        }
        let _ = self.tx.send(CapturedTurn {
            session: self.session.clone(),
            role: role.into(),
            content,
        });
    }
}

/// Pull the assistant's text out of a model output — its `text`, or the
/// reasoning channel when a thinking model left `text` empty.
fn assistant_text(out: &ModelOutput) -> String {
    out.text
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| out.reasoning.clone())
        .unwrap_or_default()
}

impl Hook for TranscriptRecorder {
    fn name(&self) -> &str {
        "transcript-recorder"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PostModel { .. } | Event::PostToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        match ev {
            Event::PostModel { out } => self.enqueue("assistant", assistant_text(out)),
            Event::PostToolUse { action, result } => {
                let body = serde_json::to_string(&result.content).unwrap_or_default();
                self.enqueue("tool", format!("[{}] {}", action.tool, body));
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}

/// Spawn the background writer: drains `rx` and persists each turn to `memory`.
/// `role` and `session` ride along as tags (`role:…`, `session:…`), so a
/// metadata-aware backend (e.g. CortexDB) can filter/aggregate on them.
pub fn spawn_transcript_writer(
    mut rx: mpsc::UnboundedReceiver<CapturedTurn>,
    memory: Arc<dyn Memory>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(turn) = rx.recv().await {
            let entry = MemoryEntry::new(turn.content)
                .with_source("transcript")
                .with_tags([
                    format!("role:{}", turn.role),
                    format!("session:{}", turn.session),
                ]);
            if let Err(e) = memory.write(entry).await {
                tracing::warn!(error = %e, "transcript write failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Action, MemoryError, StopReason, ToolResult, Usage};
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapMem(Mutex<Vec<MemoryEntry>>);
    #[async_trait::async_trait]
    impl Memory for CapMem {
        async fn recall(&self, _q: &str, _k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
            Ok(vec![])
        }
        async fn write(&self, e: MemoryEntry) -> Result<(), MemoryError> {
            self.0.lock().unwrap().push(e);
            Ok(())
        }
    }

    fn out(text: &str) -> ModelOutput {
        ModelOutput {
            text: Some(text.into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            reasoning: None,
        }
    }

    #[tokio::test]
    async fn captures_user_assistant_and_tool_turns_into_memory() {
        let (recorder, rx) = TranscriptRecorder::new("sess-1");
        let mem = Arc::new(CapMem::default());
        let handle = spawn_transcript_writer(rx, mem.clone());

        // user (explicit), assistant (PostModel), tool (PostToolUse)
        recorder.note_user("hello");
        let mut world = harness_context::default_world(std::env::temp_dir());
        let o = out("hi there");
        recorder.fire(&Event::PostModel { out: &o }, &mut world);
        let action = Action {
            tool: "read_file".into(),
            call_id: "c1".into(),
            args: serde_json::json!({"path": "x"}),
        };
        let result = ToolResult {
            ok: true,
            content: serde_json::json!({"content": "data"}),
            trace: None,
        };
        recorder.fire(
            &Event::PostToolUse {
                action: &action,
                result: &result,
            },
            &mut world,
        );

        // Close the channel so the writer task finishes, then join.
        drop(recorder);
        handle.await.unwrap();

        let stored = mem.0.lock().unwrap();
        assert_eq!(stored.len(), 3, "user + assistant + tool");
        let roles: Vec<&str> = stored
            .iter()
            .filter_map(|e| e.tags.iter().find(|t| t.starts_with("role:")))
            .map(|s| s.as_str())
            .collect();
        assert!(roles.contains(&"role:user"));
        assert!(roles.contains(&"role:assistant"));
        assert!(roles.contains(&"role:tool"));
        assert!(
            stored
                .iter()
                .all(|e| e.tags.iter().any(|t| t == "session:sess-1"))
        );
    }
}
