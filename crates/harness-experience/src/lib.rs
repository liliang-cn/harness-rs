//! # harness-experience — learn from what you did before
//!
//! An **experience memory** layer for harness-rs. Where `harness-loop`'s memory
//! layer remembers *facts* ("the user likes dark roast"), this remembers
//! **episodes** — *how a situation was handled*:
//!
//! > situation faced  →  tools called to handle it  →  outcome
//!
//! Each run is recorded as an [`Episode`]; before the next run, episodes
//! similar to the current situation are recalled and injected, so the agent
//! can reuse an approach that worked ("last time I was asked to deploy, I read
//! the config then ran the deploy tool"). Recall is **semantic** when paired
//! with a semantic [`Memory`](harness_core::Memory) backend (e.g. a CortexDB-
//! or embeddings-backed one); with a keyword backend it's lexical.
//!
//! ## Pieces
//!
//! - [`Episode`] — one unit of experience (situation, tools, outcome).
//! - [`ToolTrace`] — a `Hook` that captures the tools a run calls, in order.
//! - [`ExperienceStore`] — record/recall episodes over any `Memory`.
//! - [`ExperienceGuide`] — recall similar episodes and inject them each turn.
//! - [`ExperienceRecorder`] — ties them together; hand it to an `AgentLoop`.
//!
//! ## Wiring
//!
//! ```ignore
//! use harness_experience::ExperienceRecorder;
//!
//! let recorder = ExperienceRecorder::new(memory);   // any Memory backend
//! let loop_ = AgentLoop::new(model)
//!     .with_hook(recorder.tool_trace_hook())         // capture tools used
//!     .with_guide(Arc::new(recorder.guide().with_top_k(3)));  // recall + inject
//! let outcome = loop_.run(task.clone(), &mut world).await?;
//! recorder.record(&task.description, outcome_text).await;     // learn from it
//! ```
//!
//! The layer is backend-agnostic on purpose: it owns the *structure* of
//! experience (episodes + tool traces + recall injection); the *semantics* of
//! recall come from whichever `Memory` you plug in.

mod episode;
mod guide;
mod recorder;
mod store;
mod trace;
mod transcript;

pub use episode::Episode;
pub use guide::ExperienceGuide;
pub use recorder::ExperienceRecorder;
pub use store::{EXPERIENCE_TAG, ExperienceStore};
pub use trace::ToolTrace;
pub use transcript::{CapturedTurn, TranscriptRecorder, spawn_transcript_writer};
