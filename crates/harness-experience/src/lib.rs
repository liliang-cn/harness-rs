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
//! Recall (reactive — reuse what worked):
//!
//! - [`Episode`] — one unit of experience (situation, tools, outcome).
//! - [`ToolTrace`] — a `Hook` that captures the tools a run calls, in order.
//! - [`ExperienceStore`] — record/recall episodes over any `Memory`.
//! - [`ExperienceGuide`] — recall similar episodes and inject them each turn.
//! - [`ExperienceRecorder`] — ties them together; hand it to an `AgentLoop`.
//!
//! Skills (proactive — turn experience into procedure):
//!
//! - [`SkillDistiller`] — after a complex run that worked, draft a reusable
//!   skill from it.
//! - [`SkillReviser`] — after a run that a skill misled, draft a fix for it.
//! - [`SkillUseTrace`] — record which skills a run followed (the reviser's
//!   trigger).
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
//!
//! ## The closed loop
//!
//! Recall alone plateaus: the agent keeps rediscovering the same approach from
//! the same three recalled episodes, and prompt space caps how much of that it
//! can carry. Distillation promotes a proven approach out of episodic memory
//! into a **skill** — a named, routable procedure that costs ~100 catalogue
//! tokens instead of a full episode, and that a human can read and correct.
//! Revision closes the loop the other way: when a skill misleads a run, the run
//! that exposed it is the evidence for the fix.
//!
//! Both are one model call, both run *after* the run they learn from (never on
//! its critical path), and neither writes to disk on its own — a multi-tenant
//! host has to scope the write per user, and may want a human to approve a
//! machine-authored skill before agents start following it.
//!
//! ```ignore
//! // After a run, with the episode in hand.
//! let existing = existing_skills_in(&user_skills_dir);
//! match distiller.distill(&episode, &existing).await? {
//!     Distillation::Drafted(draft) => queue_for_review(user_id, draft),
//!     Distillation::Skipped(why)   => tracing::debug!(?why, "not worth a skill"),
//! }
//!
//! // …and when a run that followed a skill went wrong.
//! let req = RevisionRequest::from_dir(&user_skills_dir, skill, episode, complaint)?;
//! if let Revision::Revised(fix) = reviser.revise(&req).await? {
//!     queue_for_review(user_id, *fix);
//! }
//! ```

mod distill;
mod episode;
mod guide;
mod llm;
mod recorder;
mod revise;
mod skill_use;
mod skillmd;
mod store;
mod trace;
mod transcript;

pub use distill::{
    DistillError, DistillPolicy, Distillation, ExistingSkill, GateDecision, SkillDistiller,
    SkillDraft, SkipReason, existing_skills_in,
};
pub use episode::Episode;
pub use guide::ExperienceGuide;
pub use recorder::ExperienceRecorder;
pub use revise::{
    EXPERIENCE_METADATA_KEY, RefusalReason, ReviseError, RevisePolicy, Revision, RevisionGate,
    RevisionLedger, RevisionRequest, SkillReviser, SkillRevision,
};
pub use skill_use::{SKILL_ACTIVATED_EVENT, SkillUseTrace};
pub use store::{EXPERIENCE_TAG, ExperienceStore};
pub use trace::ToolTrace;
pub use transcript::{CapturedTurn, TranscriptRecorder, spawn_transcript_writer};
