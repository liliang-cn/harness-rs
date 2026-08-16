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

    /// Record a pre-built episode, filling `tools` from the trace when the
    /// caller left them empty (and draining it either way, so the next run
    /// starts clean).
    ///
    /// Prefer this over [`record`](Self::record) when you also know whether the
    /// run succeeded and which skills it followed — those are the two fields
    /// [`crate::SkillDistiller`] and [`crate::SkillReviser`] gate on, and
    /// `record` has no way to express them:
    ///
    /// ```ignore
    /// recorder
    ///     .record_episode(
    ///         Episode::new(&task.description, outcome_text)
    ///             .with_success(outcome.is_ok())
    ///             .with_skills(skill_trace.drain()),
    ///     )
    ///     .await;
    /// ```
    /// Returns the episode as recorded — with `tools` filled in. The trace is
    /// private and draining is one-shot, so a caller that also wants to hand
    /// this run to [`crate::SkillDistiller`] (whose gate counts tool calls) has
    /// no other way to see what it did. Ignoring the return value is fine.
    pub async fn record_episode(&self, mut ep: Episode) -> Episode {
        let traced = self.trace.drain();
        if ep.tools.is_empty() {
            ep.tools = traced;
        }
        if let Err(e) = self.store.record(&ep).await {
            tracing::warn!(error = %e, "experience record failed");
        }
        ep
    }
}
