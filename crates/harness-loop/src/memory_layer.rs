//! Long-term-memory wiring for [`crate::AgentLoop`].
//!
//! Two pieces, designed to be installed together:
//!
//! - [`MemoryGuide`] — at session start, calls [`Memory::recall`] with the
//!   current task description and pushes the top-K matches into
//!   `ctx.guides` as plain text. The model sees a "Relevant prior context"
//!   section in its system prompt before the very first model call.
//!
//! - [`MemoryWriter`] — captures every assistant text turn (via `PostModel`)
//!   and persists the *last* one as a [`MemoryEntry`] when the run finishes
//!   (`TaskCompleted`). This turns "this conversation produced an answer"
//!   into "future sessions can recall the answer".
//!
//! Both share an `Arc<dyn Memory>` so a single backend serves recall +
//! write. The trait is async; the writer hook uses `tokio::spawn` to commit
//! without blocking the loop.
//!
//! ## Wiring
//!
//! ```ignore
//! let mem: Arc<dyn Memory> = Arc::new(FileMemory::open("~/.harness/mem.jsonl")?);
//! let loop_ = AgentLoop::new(model)
//!     .with_guide(Arc::new(MemoryGuide::new(mem.clone()).with_top_k(5)))
//!     .with_hook(Arc::new(MemoryWriter::new(mem)));
//! ```

use async_trait::async_trait;
use harness_core::{
    Block, Context, Event, Execution, Guide, GuideError, GuideId, GuideScope, Hook, HookOutcome,
    Memory, MemoryEntry, World,
};
use std::sync::{Arc, Mutex, OnceLock};

/// Guide that recalls relevant prior memories and injects them as a
/// `Block::Text` into `ctx.guides` so the model sees them in the system
/// prompt for every iteration of this run.
pub struct MemoryGuide {
    memory: Arc<dyn Memory>,
    top_k: usize,
}

static MEMORY_GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static MEMORY_GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

impl MemoryGuide {
    /// Construct a guide that recalls up to 5 entries per session.
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory, top_k: 5 }
    }

    /// Override the number of memories recalled per session. Pick small —
    /// every recalled line spends prompt tokens.
    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }
}

#[async_trait]
impl Guide for MemoryGuide {
    fn id(&self) -> &GuideId {
        MEMORY_GUIDE_ID.get_or_init(|| "memory-recall".into())
    }
    fn kind(&self) -> Execution {
        // The recall *itself* is computational (keyword match / vector
        // lookup); the model later infers over the result.
        Execution::Computational
    }
    fn scope(&self) -> &GuideScope {
        MEMORY_GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        if self.top_k == 0 {
            return Ok(());
        }
        let hits = match self.memory.recall(&ctx.task.description, self.top_k).await {
            Ok(v) => v,
            Err(e) => {
                // Best-effort: a failing memory backend must not nuke the
                // task. Log and proceed with no recall.
                tracing::warn!(error = %e, "memory recall failed; proceeding without it");
                return Ok(());
            }
        };
        if hits.is_empty() {
            return Ok(());
        }
        let mut lines = String::from("Relevant prior context (from your long-term memory):");
        for (i, e) in hits.iter().enumerate() {
            lines.push_str(&format!("\n  {}. {}", i + 1, e.content.trim()));
        }
        ctx.guides.push(Block::Text(lines));
        Ok(())
    }
}

/// Hook that writes the final assistant text of every successful run back
/// into long-term memory.
///
/// Behaviour:
/// - On every `PostModel`, captures `out.text` into an internal slot.
/// - On `TaskCompleted`, takes the most recent captured text and writes it
///   as a `MemoryEntry` tagged with the source (defaults to `"session"`).
/// - On `SessionEnd` without a `TaskCompleted` (i.e. `BudgetExhausted`),
///   nothing is written — partial work shouldn't pollute long-term memory.
pub struct MemoryWriter {
    memory: Arc<dyn Memory>,
    last_text: Mutex<Option<String>>,
    source: String,
    tags: Vec<String>,
}

impl MemoryWriter {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            last_text: Mutex::new(None),
            source: "session".into(),
            tags: Vec::new(),
        }
    }

    /// Tag every persisted memory with the given source name (e.g.
    /// `"investor-bot"`, `"personal-assistant"`). Useful for multi-app
    /// memory stores.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn with_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }
}

impl Hook for MemoryWriter {
    fn name(&self) -> &str {
        "memory-writer"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PostModel { .. } | Event::TaskCompleted)
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        match ev {
            Event::PostModel { out } => {
                if let Some(text) = &out.text
                    && !text.trim().is_empty()
                    && let Ok(mut slot) = self.last_text.lock()
                {
                    *slot = Some(text.clone());
                }
            }
            Event::TaskCompleted => {
                let Some(text) = self.last_text.lock().ok().and_then(|mut g| g.take()) else {
                    return HookOutcome::Allow;
                };
                let entry = MemoryEntry::new(text)
                    .with_source(self.source.clone())
                    .with_tags(self.tags.clone());
                let mem = self.memory.clone();
                // Fire-and-forget: we're inside an async loop, so spawning
                // is safe and avoids blocking the next iteration.
                tokio::spawn(async move {
                    if let Err(e) = mem.write(entry).await {
                        tracing::warn!(error = %e, "memory write failed");
                    }
                });
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{ModelOutput, StopReason, Usage};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Test-only in-memory backend so we don't touch the filesystem.
    #[derive(Default)]
    struct VecMemory {
        store: Mutex<Vec<MemoryEntry>>,
    }
    #[async_trait]
    impl Memory for VecMemory {
        async fn recall(
            &self,
            query: &str,
            k: usize,
        ) -> Result<Vec<MemoryEntry>, harness_core::MemoryError> {
            let g = self.store.lock().unwrap();
            let q = query.to_lowercase();
            let mut hits: Vec<MemoryEntry> = g
                .iter()
                .filter(|e| {
                    let hay = e.content.to_lowercase();
                    q.split_whitespace().any(|t| hay.contains(t))
                })
                .cloned()
                .collect();
            hits.truncate(k);
            Ok(hits)
        }
        async fn write(&self, entry: MemoryEntry) -> Result<(), harness_core::MemoryError> {
            self.store.lock().unwrap().push(entry);
            Ok(())
        }
    }

    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[tokio::test]
    async fn writer_persists_last_text_on_task_completed() {
        let mem = Arc::new(VecMemory::default());
        let w = MemoryWriter::new(mem.clone()).with_source("test-app");
        let mut world = harness_context::default_world(std::env::temp_dir().join(format!(
            "harness-mw-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        )));

        let out = ModelOutput {
            text: Some("final answer X".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            reasoning: None,
        };
        let _ = w.fire(&Event::PostModel { out: &out }, &mut world);
        let _ = w.fire(&Event::TaskCompleted, &mut world);

        // The hook spawns; give the runtime a tick to drain.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let stored = mem.store.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, "final answer X");
        assert_eq!(stored[0].source.as_deref(), Some("test-app"));
    }

    #[tokio::test]
    async fn writer_skips_when_no_task_completed_fires() {
        let mem = Arc::new(VecMemory::default());
        let w = MemoryWriter::new(mem.clone());
        let mut world = harness_context::default_world(std::env::temp_dir().join(format!(
            "harness-mw-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        )));

        let out = ModelOutput {
            text: Some("partial".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            reasoning: None,
        };
        let _ = w.fire(&Event::PostModel { out: &out }, &mut world);
        // No TaskCompleted ⇒ nothing should be written.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(mem.store.lock().unwrap().is_empty());
    }
}
