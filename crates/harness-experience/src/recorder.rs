//! `ExperienceRecorder` — the one-stop wiring that turns runs into experience.
//!
//! It owns an [`ExperienceStore`] and a [`ToolTrace`], hands you the hook +
//! guide to install on an `AgentLoop`, and records the finished run as an
//! [`Episode`] (situation → tools captured by the trace → outcome).
//!
//! ```ignore
//! let recorder = ExperienceRecorder::new(memory);              // any Memory
//! let loop_ = AgentLoop::new(model)
//!     .with_hook(recorder.tool_trace_hook())                   // capture tools
//!     .with_guide(recorder.guide().with_top_k(3));             // recall + inject
//! let outcome = loop_.run(task.clone(), &mut world).await?;
//! recorder.record(&task.description, outcome_text).await;      // learn from it
//! ```

use crate::episode::Episode;
use crate::guide::ExperienceGuide;
use crate::store::ExperienceStore;
use crate::trace::ToolTrace;
use harness_core::{Hook, Memory};
use std::sync::Arc;

pub struct ExperienceRecorder {
    store: Arc<ExperienceStore>,
    trace: ToolTrace,
}

impl ExperienceRecorder {
    /// Build over any `Memory` backend (semantic for semantic recall).
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            store: Arc::new(ExperienceStore::new(memory)),
            trace: ToolTrace::new(),
        }
    }

    /// Build from a pre-configured store (e.g. with a custom source/tag).
    pub fn from_store(store: ExperienceStore) -> Self {
        Self {
            store: Arc::new(store),
            trace: ToolTrace::new(),
        }
    }

    /// The hook that captures tool calls — install with `AgentLoop::with_hook`.
    pub fn tool_trace_hook(&self) -> Arc<dyn Hook> {
        self.trace.hook()
    }

    /// The guide that recalls + injects past experience — install with
    /// `AgentLoop::with_guide`.
    pub fn guide(&self) -> ExperienceGuide {
        ExperienceGuide::new(self.store.clone())
    }

    /// The shared store (for direct record/recall or reuse).
    pub fn store(&self) -> &Arc<ExperienceStore> {
        &self.store
    }

    /// Record the just-finished run as an episode: `situation` + the tools the
    /// trace captured (drained) + `outcome`. Call once after `loop_.run`.
    pub async fn record(&self, situation: impl Into<String>, outcome: impl Into<String>) {
        let ep = Episode::new(situation, outcome).with_tools(self.trace.drain());
        if let Err(e) = self.store.record(&ep).await {
            tracing::warn!(error = %e, "experience record failed");
        }
    }
}
