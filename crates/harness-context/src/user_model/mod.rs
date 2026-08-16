//! A persistent, incrementally-deepening model of **who the user is**.
//!
//! # What this is for
//!
//! `harness-rs` already has two kinds of long-term state, and neither one is a
//! portrait:
//!
//! - [`harness_core::Memory`] is flat semantic memory — facts in, facts out.
//!   Recall is per-query, so a preference only reaches the prompt when the
//!   current message happens to look like the sentence that recorded it. "User
//!   hates being asked clarifying questions" is exactly the sort of thing that
//!   never matches a query and therefore never gets applied.
//! - `harness-experience` stores episodes — what happened and how it went.
//!   That is a model of *tasks*, not of the person.
//!
//! What is missing is the thing a human colleague builds over months and
//! applies to every single interaction without being asked: your role, what you
//! already know, how you like to be talked to, what you have told them never to
//! do again, what you are working on. This module is that: a structured,
//! versioned [`UserModel`] that is injected into every prompt (cheaply), and
//! deepened occasionally (expensively, on an explicit trigger).
//!
//! # The four pieces
//!
//! | Piece | Cost | Runs |
//! |---|---|---|
//! | [`UserModelGuide`] | ~300 tokens | every turn |
//! | [`UserModelStore`] | one `Memory` read | once per session |
//! | [`UpdateTracker`] / [`UpdatePolicy`] | free | every turn |
//! | [`UserModelUpdater`] | one model call | every N turns / session end |
//!
//! The split is the whole design. Reading a portrait must be cheap enough to do
//! unconditionally; writing one must be rare enough that nobody notices the
//! bill. See [`updater`] for the trigger policy and the reasoning behind the
//! defaults.
//!
//! # Trust boundary
//!
//! The model proposes [`Observation`]s; [`UserModel::merge`] decides. Conflicts
//! resolve by a documented rule (recency-dominant with confidence hysteresis —
//! see [`portrait`]), beliefs carry provenance and decay with age, and an
//! explicit user retraction outranks everything. A model that hallucinates can
//! add a weak claim that ages out; it cannot rewrite the portrait.
//!
//! # Multi-tenancy
//!
//! Every entry point takes a [`UserId`]. There is no ambient current user, no
//! `Default` portrait, and the tenant check on load runs against the
//! deserialised record rather than trusting the storage backend's query
//! scoping. `UserModelGuide` is constructed per user, so the prompt for user A
//! has no code path that could render user B's portrait.
//!
//! # Wiring
//!
//! ```ignore
//! let store = Arc::new(UserModelStore::new(memory.clone()));
//! let updater = UserModelUpdater::new(model.clone());
//! let trackers = Arc::new(UpdateTrackers::new());
//! let policy = UpdatePolicy::default();
//!
//! // per session
//! let user = UserId::new(session.user_id());
//! let guide = Arc::new(UserModelGuide::new(store.clone(), user.clone()));
//! // ... register `guide` with the loop ...
//!
//! // per user turn
//! trackers.record_turn(&user, user_text.len());
//! if let Some(reason) = trackers.due(&user, &policy, now_ms) {
//!     let ev = Evidence::from_context(user.clone(), &ctx, 24, 6000)
//!         .with_source(format!("{session_id}:{reason:?}"));
//!     updater.refresh_stored(&store, &user, &ev).await?;
//!     trackers.mark_updated(&user, now_ms);
//!     guide.invalidate();
//! }
//! ```

pub mod guide;
pub mod portrait;
pub mod store;
pub mod updater;

pub use guide::UserModelGuide;
pub use portrait::*;
pub use store::{USER_MODEL_TAG, UserModelStore};
pub use updater::{TriggerReason, UpdatePolicy, UpdateTracker, UpdateTrackers, UserModelUpdater};

use harness_core::{Block, Context, MemoryError, ModelError, TurnRole};

/// Everything that can go wrong maintaining a portrait.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UserModelError {
    #[error("user model storage: {0}")]
    Memory(#[from] MemoryError),
    #[error("user model update call: {0}")]
    Model(#[from] ModelError),
    #[error("user model serde: {0}")]
    Serde(String),
    /// The model's reply was not a usable delta. Distinct from `Model` because
    /// it is retryable in a way a transport failure is not, and distinct from
    /// `Serde` because it is the *model's* fault, not the store's.
    #[error("user model delta parse: {0}")]
    Parse(String),
    /// Evidence for one user was handed to another user's portrait. Always a
    /// bug, never a transient — surfaced as an error rather than a log line
    /// because the failure mode is a cross-tenant leak.
    #[error("user model scope mismatch: portrait is `{portrait}`, evidence is `{evidence}`")]
    Scope { portrait: String, evidence: String },
}

impl From<serde_json::Error> for UserModelError {
    fn from(e: serde_json::Error) -> Self {
        UserModelError::Serde(e.to_string())
    }
}

/// The conversation slice an update round reads.
///
/// Carries its own `now_ms` so that a merge is reproducible: replay the same
/// evidence and you get the same portrait, decay included.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// Whose conversation this is. Checked against the portrait before any
    /// model call.
    pub user: UserId,
    /// Rendered `role: text` lines, most recent last.
    pub transcript: String,
    /// How many turns the transcript covers. Diagnostics only.
    pub turns: u32,
    /// The clock for this round.
    pub now_ms: i64,
    /// Session id or trigger name, stamped into the provenance of everything
    /// this round touches.
    pub source: Option<String>,
}

impl Evidence {
    pub fn new(user: UserId, transcript: impl Into<String>) -> Self {
        Self {
            user,
            transcript: transcript.into(),
            turns: 0,
            now_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            source: None,
        }
    }

    /// Pin the clock (tests, replays, backfills).
    pub fn at(mut self, now_ms: i64) -> Self {
        self.now_ms = now_ms;
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_turns(mut self, turns: u32) -> Self {
        self.turns = turns;
        self
    }

    /// Build evidence from the tail of a live [`Context`].
    ///
    /// Only user and assistant *text* is included. Tool calls and tool results
    /// are dropped on purpose: they are the largest thing in a typical context
    /// and they describe what the agent did, not who the user is — paying to
    /// send them would be paying for noise. Assistant text is kept because a
    /// preference is often only legible as a reply to something ("no, shorter").
    ///
    /// `max_turns` caps how far back to look and `max_chars` caps the total,
    /// dropping the *oldest* lines first.
    pub fn from_context(user: UserId, ctx: &Context, max_turns: usize, max_chars: usize) -> Self {
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0usize;
        let mut turns = 0u32;

        for turn in ctx.history.iter().rev().take(max_turns) {
            let role = match turn.role {
                TurnRole::User => "user",
                TurnRole::Assistant => "assistant",
                _ => continue,
            };
            let mut text = String::new();
            for b in &turn.blocks {
                if let Block::Text(t) = b {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(t.trim());
                }
            }
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            // One rambling turn should not evict the ten around it.
            let clipped: String = text.chars().take(600).collect();
            let line = format!("{role}: {clipped}");
            if used + line.chars().count() > max_chars {
                break;
            }
            used += line.chars().count();
            turns += 1;
            lines.push(line);
        }

        lines.reverse();
        Self::new(user, lines.join("\n")).with_turns(turns)
    }

    /// Length of the user-authored text in this evidence — what
    /// [`UpdateTracker::record_turn`] counts.
    pub fn user_chars(&self) -> usize {
        self.transcript
            .lines()
            .filter(|l| l.starts_with("user: "))
            .map(|l| l.chars().count())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContextExt;
    use harness_core::Task;

    #[test]
    fn evidence_from_context_keeps_recent_text_in_order() {
        let mut ctx = Context::new(Task {
            description: "t".into(),
            source: None,
            deadline: None,
        });
        ctx.push_user_text("I only write Rust these days");
        ctx.push_assistant_text("noted");
        ctx.push_tool_call("c1", "read_file", &serde_json::json!({"path": "x"}));
        ctx.push_user_text("and keep replies short");

        let ev = Evidence::from_context(UserId::new("alice"), &ctx, 10, 10_000);
        assert_eq!(ev.turns, 3, "tool turns are not evidence about the user");
        assert!(ev.transcript.starts_with("user: I only write Rust"));
        assert!(ev.transcript.ends_with("user: and keep replies short"));
        assert!(!ev.transcript.contains("read_file"));
    }

    #[test]
    fn evidence_drops_the_oldest_lines_under_a_char_cap() {
        let mut ctx = Context::new(Task {
            description: "t".into(),
            source: None,
            deadline: None,
        });
        ctx.push_user_text("oldest");
        ctx.push_user_text("newest");
        let ev = Evidence::from_context(UserId::new("alice"), &ctx, 10, 14);
        assert_eq!(ev.transcript, "user: newest");
    }
}
