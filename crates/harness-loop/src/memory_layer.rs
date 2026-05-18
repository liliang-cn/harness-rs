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
    Memory, MemoryEntry, Model, Task, Turn, TurnRole, World,
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

/// Smarter alternative to [`MemoryWriter`] — distil the session's assistant
/// turns into 1..=`max_facts` atomic durable facts using a cheap
/// "synthesizer" model, instead of persisting the verbatim final answer.
///
/// Wire either `MemoryWriter` **or** `MemorySynthesizer`, not both —
/// `MemorySynthesizer` is a superset of the writer's behaviour with the
/// extra distillation step.
///
/// Behaviour:
/// - On `PostModel`, appends `out.text` (when present, non-empty) to an
///   internal buffer.
/// - On `TaskCompleted`, `tokio::spawn`s a synthesis task: calls
///   `synth_model.complete()` with a fixed prompt that asks for a JSON
///   array of `{content, tags}` objects, parses the response, and writes
///   each one via `Memory::write`.
/// - Model errors / parse failures fall back to saving the raw response
///   as a single entry tagged `"synth-raw"` so the session's information
///   isn't lost entirely.
/// - On `BudgetExhausted` (no `TaskCompleted` fires), nothing is written.
///
/// The synth model should be cheap (`deepseek-v4-flash`, `gpt-5-nano`, etc.).
/// Constructed independently from the main model so you can use a small
/// summariser even when the reasoning model is large.
pub struct MemorySynthesizer {
    memory: Arc<dyn Memory>,
    synth_model: Arc<dyn Model>,
    transcripts: Mutex<Vec<String>>,
    source: String,
    base_tags: Vec<String>,
    max_facts: usize,
    // JoinHandles of spawned synthesis tasks. The agent loop's owner can
    // `await flush_pending()` before exiting to guarantee that synth
    // completes before the process tears down its tokio runtime.
    pending: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl MemorySynthesizer {
    /// Construct a synthesizer that uses `synth_model` to distil the
    /// session into at most 3 facts.
    pub fn new(memory: Arc<dyn Memory>, synth_model: Arc<dyn Model>) -> Self {
        Self {
            memory,
            synth_model,
            transcripts: Mutex::new(Vec::new()),
            source: "session".into(),
            base_tags: Vec::new(),
            max_facts: 3,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Await all background synthesis tasks that have been kicked off so
    /// far. Call this before your process exits if you want to guarantee
    /// the last session's memory is on disk — otherwise the tokio runtime
    /// may be dropped while the spawn is mid-flight.
    pub async fn flush_pending(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = match self.pending.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => return,
        };
        for h in handles {
            let _ = h.await;
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn with_base_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.base_tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Cap how many facts the synthesizer is allowed to emit. Default 3.
    pub fn with_max_facts(mut self, n: usize) -> Self {
        self.max_facts = n.max(1);
        self
    }
}

#[derive(serde::Deserialize)]
struct SynthFact {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// Best-effort JSON-array extractor: tolerates markdown code fences and
/// leading/trailing prose around the JSON body.
fn extract_facts(raw: &str) -> Option<Vec<SynthFact>> {
    // Strip ```json ... ``` or ``` ... ``` fences if present.
    let stripped = raw.trim();
    let body = if let Some(rest) = stripped.strip_prefix("```json") {
        rest.trim_start_matches('\n')
            .rsplit_once("```")
            .map(|(b, _)| b)
            .unwrap_or(rest)
    } else if let Some(rest) = stripped.strip_prefix("```") {
        rest.trim_start_matches('\n')
            .rsplit_once("```")
            .map(|(b, _)| b)
            .unwrap_or(rest)
    } else {
        stripped
    };
    // Find first '[' and last ']' — JSON array.
    let start = body.find('[')?;
    let end = body.rfind(']')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Vec<SynthFact>>(&body[start..=end]).ok()
}

impl Hook for MemorySynthesizer {
    fn name(&self) -> &str {
        "memory-synthesizer"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PostModel { .. } | Event::TaskCompleted)
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        match ev {
            Event::PostModel { out } => {
                if let Some(text) = &out.text
                    && !text.trim().is_empty()
                    && let Ok(mut buf) = self.transcripts.lock()
                {
                    buf.push(text.clone());
                }
            }
            Event::TaskCompleted => {
                let transcript = match self.transcripts.lock() {
                    Ok(mut g) => std::mem::take(&mut *g).join("\n\n---\n\n"),
                    Err(_) => return HookOutcome::Allow,
                };
                if transcript.trim().is_empty() {
                    return HookOutcome::Allow;
                }
                let mem = self.memory.clone();
                let model = self.synth_model.clone();
                let source = self.source.clone();
                let base_tags = self.base_tags.clone();
                let max_facts = self.max_facts;
                let handle = tokio::spawn(async move {
                    distil_and_write(mem, model, source, base_tags, max_facts, transcript).await;
                });
                if let Ok(mut g) = self.pending.lock() {
                    g.push(handle);
                }
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}

async fn distil_and_write(
    memory: Arc<dyn Memory>,
    model: Arc<dyn Model>,
    source: String,
    base_tags: Vec<String>,
    max_facts: usize,
    transcript: String,
) {
    let prompt = format!(
        "Below is the assistant's turns from a completed agent session. \
         Extract 1 to {max_facts} DURABLE FACTS worth remembering for future sessions \
         (user preferences, decisions made, key findings, learned constraints — NOT \
         transient details like timestamps or one-off answers). \
         \n\nReturn ONLY a JSON array (no prose, no markdown fences) where each item is \
         {{\"content\": \"<one durable fact, 1-2 sentences>\", \"tags\": [\"<keyword>\", ...]}}. \
         Use 2-5 lowercase keyword tags per fact for retrieval. \
         If the session produced nothing durable, return [].\
         \n\n--- SESSION TRANSCRIPT ---\n{transcript}\n--- END TRANSCRIPT ---"
    );

    let mut ctx = Context::new(Task {
        description: prompt.clone(),
        source: None,
        deadline: None,
    });
    ctx.history.push(Turn {
        role: TurnRole::User,
        blocks: vec![Block::Text(prompt)],
    });

    let out = match model.complete(&ctx).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "memory synth model call failed; nothing persisted");
            return;
        }
    };
    let raw = out.text.unwrap_or_default();

    let mut wrote_any = false;
    if let Some(facts) = extract_facts(&raw) {
        for f in facts.into_iter().take(max_facts) {
            let content = f.content.trim().to_string();
            if content.is_empty() {
                continue;
            }
            let mut tags = base_tags.clone();
            tags.extend(f.tags);
            let entry = MemoryEntry::new(content)
                .with_source(source.clone())
                .with_tags(tags);
            if let Err(e) = memory.write(entry).await {
                tracing::warn!(error = %e, "memory synth write failed");
            } else {
                wrote_any = true;
            }
        }
    }

    if !wrote_any && !raw.trim().is_empty() {
        // Fallback: model returned something but we couldn't parse it.
        // Persist verbatim with a "synth-raw" tag so the operator can
        // grep it later, rather than silently dropping the session.
        let mut tags = base_tags;
        tags.push("synth-raw".into());
        let entry = MemoryEntry::new(raw.trim().to_string())
            .with_source(source)
            .with_tags(tags);
        if let Err(e) = memory.write(entry).await {
            tracing::warn!(error = %e, "memory synth-raw write failed");
        }
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

    #[tokio::test]
    async fn synthesizer_parses_clean_json_and_writes_atomic_facts() {
        use harness_models::{MockModel, MockResponse};

        let mem = Arc::new(VecMemory::default());
        let synth: Arc<dyn Model> = Arc::new(MockModel::new().script(MockResponse::text(
            r#"[
              {"content": "user prefers dark roast coffee, no sugar", "tags": ["coffee", "preferences"]},
              {"content": "user lives in Beijing (Asia/Shanghai tz)", "tags": ["location", "timezone"]}
            ]"#,
        )));
        let s = MemorySynthesizer::new(mem.clone(), synth).with_source("test");
        let mut world = harness_context::default_world(std::env::temp_dir().join(format!(
            "harness-ms-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        )));

        let out_a = ModelOutput {
            text: Some("I'll remember your coffee preference.".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            reasoning: None,
        };
        let out_b = ModelOutput {
            text: Some("Setting Beijing as your timezone.".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            reasoning: None,
        };
        let _ = s.fire(&Event::PostModel { out: &out_a }, &mut world);
        let _ = s.fire(&Event::PostModel { out: &out_b }, &mut world);
        let _ = s.fire(&Event::TaskCompleted, &mut world);

        for _ in 0..50 {
            if mem.store.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stored = mem.store.lock().unwrap().clone();
        assert_eq!(stored.len(), 2, "expected 2 atomic facts, got {stored:#?}");
        assert!(stored.iter().any(|e| e.content.contains("dark roast")));
        assert!(stored.iter().any(|e| e.content.contains("Beijing")));
        let coffee = stored
            .iter()
            .find(|e| e.content.contains("dark roast"))
            .unwrap();
        assert!(coffee.tags.contains(&"coffee".to_string()));
        assert_eq!(coffee.source.as_deref(), Some("test"));
    }

    #[tokio::test]
    async fn synthesizer_strips_markdown_fences_around_json() {
        use harness_models::{MockModel, MockResponse};

        let mem = Arc::new(VecMemory::default());
        let synth: Arc<dyn Model> = Arc::new(MockModel::new().script(MockResponse::text(
            "Here are the facts:\n```json\n[{\"content\":\"fact one\",\"tags\":[\"x\"]}]\n```\n",
        )));
        let s = MemorySynthesizer::new(mem.clone(), synth);
        let mut world = harness_context::default_world(std::env::temp_dir());

        let out = ModelOutput {
            text: Some("some chat".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            reasoning: None,
        };
        let _ = s.fire(&Event::PostModel { out: &out }, &mut world);
        let _ = s.fire(&Event::TaskCompleted, &mut world);

        for _ in 0..50 {
            if !mem.store.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stored = mem.store.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, "fact one");
    }

    #[tokio::test]
    async fn synthesizer_falls_back_to_synth_raw_when_json_unparseable() {
        use harness_models::{MockModel, MockResponse};

        let mem = Arc::new(VecMemory::default());
        let synth: Arc<dyn Model> = Arc::new(MockModel::new().script(MockResponse::text(
            "The user said they like coffee. I think that's important.",
        )));
        let s = MemorySynthesizer::new(mem.clone(), synth);
        let mut world = harness_context::default_world(std::env::temp_dir());

        let out = ModelOutput {
            text: Some("session chat".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            reasoning: None,
        };
        let _ = s.fire(&Event::PostModel { out: &out }, &mut world);
        let _ = s.fire(&Event::TaskCompleted, &mut world);

        for _ in 0..50 {
            if !mem.store.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stored = mem.store.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].tags.contains(&"synth-raw".to_string()));
        assert!(stored[0].content.contains("coffee"));
    }
}
