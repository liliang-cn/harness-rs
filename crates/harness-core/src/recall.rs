//! Cross-session conversation recall.
//!
//! Where [`crate::Memory`] stores curated facts, `RecallStore` stores the raw
//! transcript of every session so the agent can later search what was actually
//! said ("what did the user ask three weeks ago"). Same open-harness promise:
//! operator-owned, transferable, inspectable.
//!
//! - Trait + types live here (dependency-light).
//! - Default file backend: [`harness_context::FileRecall`] (JSONL).
//! - FTS5 backend: the optional `harness-recall-sqlite` crate.
//!
//! ## Wiring
//! `AgentLoop::with_recall(store)` captures each turn into the store and
//! registers the `session_search` tool. Owner + session id are read from
//! `World.profile.extra["recall_owner"|"recall_session"]`.

use serde::{Deserialize, Serialize};

/// One transcript message in a recall session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecallMessage {
    /// Monotonic id within the session, assigned by the store on append.
    /// 0 on input.
    #[serde(default)]
    pub id: i64,
    /// "user" | "assistant" | "tool" | "system".
    pub role: String,
    /// Message text (assistant text, user prompt, or tool result body).
    pub content: String,
    /// For tool messages: the tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// For assistant messages: JSON-encoded tool-call array, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<String>,
    /// Milliseconds since unix epoch.
    pub ts_ms: i64,
}

impl RecallMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>, ts_ms: i64) -> Self {
        Self {
            id: 0,
            role: role.into(),
            content: content.into(),
            tool_name: None,
            tool_calls: None,
            ts_ms,
        }
    }
    pub fn with_tool_name(mut self, name: impl Into<String>) -> Self {
        self.tool_name = Some(name.into());
        self
    }
    pub fn with_tool_calls(mut self, calls: impl Into<String>) -> Self {
        self.tool_calls = Some(calls.into());
        self
    }
}

/// Metadata about one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionMeta {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// App-defined origin: "cli" | "web" | …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub started_at_ms: i64,
    #[serde(default)]
    pub message_count: i64,
}

impl SessionMeta {
    pub fn new(session_id: impl Into<String>, started_at_ms: i64) -> Self {
        Self {
            session_id: session_id.into(),
            title: None,
            source: None,
            started_at_ms,
            message_count: 0,
        }
    }
}

/// A search hit: the matched session plus enough surrounding messages for the
/// agent to orient (Hermes-style bookends + a window around the anchor).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionHit {
    pub session: SessionMeta,
    /// Excerpt with match markers (`>>>match<<<`).
    pub snippet: String,
    /// Id of the matched message.
    pub anchor_id: i64,
    /// First few messages of the session.
    pub bookend_start: Vec<RecallMessage>,
    /// ±window messages around the anchor.
    pub around: Vec<RecallMessage>,
    /// Last few messages of the session.
    pub bookend_end: Vec<RecallMessage>,
}

impl SessionHit {
    pub fn new(
        session: SessionMeta,
        snippet: String,
        anchor_id: i64,
        bookend_start: Vec<RecallMessage>,
        around: Vec<RecallMessage>,
        bookend_end: Vec<RecallMessage>,
    ) -> Self {
        Self {
            session,
            snippet,
            anchor_id,
            bookend_start,
            around,
            bookend_end,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecallError {
    #[error("recall io: {0}")]
    Io(String),
    #[error("recall backend: {0}")]
    Backend(String),
    #[error("recall serde: {0}")]
    Serde(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// Cross-session transcript store. All methods are owner-scoped: a given
/// `owner` can never see another owner's sessions.
#[async_trait::async_trait]
pub trait RecallStore: Send + Sync + 'static {
    /// Create/refresh the session row (idempotent).
    async fn ensure_session(
        &self,
        owner: &str,
        session_id: &str,
        meta: &SessionMeta,
    ) -> Result<(), RecallError>;

    /// Append one message; returns the assigned id.
    async fn append(
        &self,
        owner: &str,
        session_id: &str,
        msg: &RecallMessage,
    ) -> Result<i64, RecallError>;

    /// Discovery: top sessions matching `query`, with snippet + bookends.
    async fn search(
        &self,
        owner: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionHit>, RecallError>;

    /// Scroll: messages with id in `[around - window, around + window]`.
    async fn scroll(
        &self,
        owner: &str,
        session_id: &str,
        around: i64,
        window: usize,
    ) -> Result<Vec<RecallMessage>, RecallError>;

    /// Browse: the owner's most recent sessions, newest first.
    async fn recent(&self, owner: &str, limit: usize) -> Result<Vec<SessionMeta>, RecallError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_round_trip_through_serde() {
        let m = RecallMessage::new("assistant", "hello", 123).with_tool_calls("[]");
        let j = serde_json::to_string(&m).unwrap();
        let back: RecallMessage = serde_json::from_str(&j).unwrap();
        assert_eq!(back.role, "assistant");
        assert_eq!(back.tool_calls.as_deref(), Some("[]"));
        assert!(back.tool_name.is_none());

        let hit = SessionHit {
            session: SessionMeta::new("s1", 1),
            snippet: ">>>hi<<<".into(),
            anchor_id: 1,
            bookend_start: vec![m.clone()],
            around: vec![m.clone()],
            bookend_end: vec![m],
        };
        let j = serde_json::to_string(&hit).unwrap();
        assert!(j.contains("\"anchor_id\":1"));
    }
}
