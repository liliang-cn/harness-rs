//! Capture every conversation turn into a [`Memory`] as it happens.
//!
//! Turns are capped ([`DEFAULT_MAX_CHARS`]), consecutive verbatim repeats are
//! dropped, and each entry carries a TTL ([`DEFAULT_TTL_DAYS`]). A transcript is
//! a searchable copy of something the app already stores authoritatively, so it
//! is allowed to age out — and a backend that ignores `expires_ms` will keep it
//! forever, which is how a shared brain fills with `[list_dir]` echoes.
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
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
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
    /// Longest content kept per turn; longer is truncated with a marker.
    max_chars: usize,
    /// Hash of the last enqueued turn, to drop verbatim repeats.
    last: Mutex<Option<u64>>,
}

/// Default per-turn cap. A tool result is not a memory: a `read_file` of a
/// 13 KB file is 13 KB of transcript, and recall never wanted the tail of it.
pub const DEFAULT_MAX_CHARS: usize = 2_000;

impl TranscriptRecorder {
    /// Create a recorder for `session`, returning it plus the receiver its
    /// background writer drains.
    pub fn new(session: impl Into<String>) -> (Self, mpsc::UnboundedReceiver<CapturedTurn>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                session: session.into(),
                max_chars: DEFAULT_MAX_CHARS,
                last: Mutex::new(None),
            },
            rx,
        )
    }

    /// Override the per-turn character cap. `0` disables truncation — only do
    /// that for a backend you are happy to grow without bound.
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
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
        let content = truncate(content, self.max_chars);

        // An agent that lists the same directory on every iteration deposits
        // the same turn on every iteration. Verbatim repeats carry no new
        // information and crowd out real memories in recall, so drop a turn
        // identical to the one before it. Only consecutive repeats: the same
        // command run again after something else happened is a real event.
        let mut h = DefaultHasher::new();
        role.hash(&mut h);
        content.hash(&mut h);
        let digest = h.finish();
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            if *last == Some(digest) {
                return;
            }
            *last = Some(digest);
        }

        let _ = self.tx.send(CapturedTurn {
            session: self.session.clone(),
            role: role.into(),
            content,
        });
    }
}

/// Cut `s` to at most `max` characters, marking that it was cut. `max == 0`
/// means no limit. Respects char boundaries, so it is safe on UTF-8.
fn truncate(s: String, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        return s;
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [{} chars truncated]", s.chars().count() - max)
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
///
/// Transcripts are the biggest PII surface — tool results and model output flow
/// in verbatim. Redact at this boundary by handing in a redacting `memory`:
/// `spawn_transcript_writer(rx, Arc::new(RedactingMemory::new(cortex)))`
/// (see `harness_context::RedactingMemory`).
pub fn spawn_transcript_writer(
    rx: mpsc::UnboundedReceiver<CapturedTurn>,
    memory: Arc<dyn Memory>,
) -> tokio::task::JoinHandle<()> {
    spawn_transcript_writer_with_ttl(rx, memory, Some(DEFAULT_TTL_DAYS))
}

/// Default retention for a captured turn. A transcript is a *searchable copy* —
/// the authoritative one lives in the app's own store — so it should age out.
/// Without this every turn of every session accumulated forever: one shared
/// brain reached 70% raw tool-call echoes, six of which had ever been recalled.
pub const DEFAULT_TTL_DAYS: u32 = 30;

/// As [`spawn_transcript_writer`], with an explicit retention. `None` keeps
/// turns forever — choose it only when something else prunes the backend.
pub fn spawn_transcript_writer_with_ttl(
    mut rx: mpsc::UnboundedReceiver<CapturedTurn>,
    memory: Arc<dyn Memory>,
    ttl_days: Option<u32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(turn) = rx.recv().await {
            let mut entry = MemoryEntry::new(turn.content)
                .with_source("transcript")
                .with_tags([
                    format!("role:{}", turn.role),
                    format!("session:{}", turn.session),
                ]);
            if let Some(days) = ttl_days {
                entry = entry.with_ttl_days(days);
            }
            if let Err(e) = memory.write(entry).await {
                tracing::warn!(error = %e, "transcript write failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Action, MemoryError, ToolResult};
    use std::sync::Mutex;

    #[test]
    fn a_long_tool_result_is_truncated_with_a_marker() {
        let (rec, mut rx) = TranscriptRecorder::new("s");
        rec.note_user("x".repeat(DEFAULT_MAX_CHARS + 500));
        let turn = rx.try_recv().expect("one turn");
        assert!(turn.content.chars().count() < DEFAULT_MAX_CHARS + 100);
        assert!(
            turn.content.contains("500 chars truncated"),
            "{}",
            turn.content
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // Byte slicing would panic here; a Chinese transcript is the common case.
        let (rec, mut rx) = TranscriptRecorder::new("s");
        let rec = rec.with_max_chars(3);
        rec.note_user("你好世界啊");
        let turn = rx.try_recv().expect("one turn");
        assert!(turn.content.starts_with("你好世"));
        assert!(turn.content.contains("2 chars truncated"));
    }

    #[test]
    fn a_verbatim_repeat_of_the_previous_turn_is_dropped() {
        // The shared brain grew 49 identical `[list_dir]` memories from one
        // agent that re-listed the same directory every iteration.
        let (rec, mut rx) = TranscriptRecorder::new("s");
        rec.note_user("[list_dir] {\"path\":\".\"}");
        rec.note_user("[list_dir] {\"path\":\".\"}");
        rec.note_user("[list_dir] {\"path\":\".\"}");
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "repeats must not be enqueued");
    }

    #[test]
    fn a_repeat_after_something_else_is_kept() {
        // Only *consecutive* repeats are noise. The same command run again
        // after other work is a real event in the transcript.
        let (rec, mut rx) = TranscriptRecorder::new("s");
        rec.note_user("ls");
        rec.note_user("cat x");
        rec.note_user("ls");
        assert_eq!(rx.try_recv().unwrap().content, "ls");
        assert_eq!(rx.try_recv().unwrap().content, "cat x");
        assert_eq!(rx.try_recv().unwrap().content, "ls");
    }

    #[test]
    fn max_chars_zero_disables_truncation() {
        let (rec, mut rx) = TranscriptRecorder::new("s");
        let rec = rec.with_max_chars(0);
        let long = "y".repeat(50_000);
        rec.note_user(long.clone());
        assert_eq!(rx.try_recv().unwrap().content, long);
    }

    #[tokio::test]
    async fn written_turns_carry_a_ttl_by_default() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mem = Arc::new(CapMem::default());
        let h = spawn_transcript_writer(rx, mem.clone());
        tx.send(CapturedTurn {
            session: "s".into(),
            role: "tool".into(),
            content: "[ls] {}".into(),
        })
        .unwrap();
        drop(tx);
        h.await.unwrap();
        let got = mem.0.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert!(
            got[0].expires_ms.is_some(),
            "a transcript turn must expire on its own"
        );
    }

    #[tokio::test]
    async fn an_explicit_none_ttl_keeps_turns_forever() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mem = Arc::new(CapMem::default());
        let h = spawn_transcript_writer_with_ttl(rx, mem.clone(), None);
        tx.send(CapturedTurn {
            session: "s".into(),
            role: "tool".into(),
            content: "[ls] {}".into(),
        })
        .unwrap();
        drop(tx);
        h.await.unwrap();
        assert!(mem.0.lock().unwrap()[0].expires_ms.is_none());
    }

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
            ..Default::default()
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

#[cfg(test)]
mod export_surface {
    /// The two call sites in superleo reach these three through
    /// `harness_loop::experience::…`; a re-export that stops being public is a
    /// downstream break, not a local one, so pin the path here.
    #[test]
    fn superleo_facing_exports_resolve() {
        let _: u32 = crate::experience::DEFAULT_TTL_DAYS;
        let _ = crate::experience::spawn_transcript_writer_with_ttl;
        let (_rec, _rx) = crate::experience::TranscriptRecorder::new("s");
        let _: usize = crate::experience::DEFAULT_MAX_CHARS;
    }
}
