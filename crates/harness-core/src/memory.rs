//! Long-term, cross-session memory.
//!
//! Short-term context lives in [`crate::Context`]; the [`crate::Compactor`]
//! keeps it within budget *within a single run*. Long-term memory is what
//! survives across runs — the dataset that turns a generic assistant into a
//! personalised one. Per Harrison Chase / Sarah Wooders: **memory is the
//! harness**. To keep the framework's "open harness" promise the memory layer
//! must be:
//!
//! - **owned by the operator** (no provider-side stateful APIs),
//! - **transferable** (a swap to a different model must not lose memory),
//! - **inspectable** (plain on-disk format, no opaque tokens).
//!
//! This module ships the trait + types. Concrete backends live in
//! [`harness_context::FileMemory`] (JSONL) and downstream crates.
//!
//! ## Wiring
//!
//! - A `MemoryGuide` from `harness-rs-loop` calls [`Memory::recall`] at the
//!   start of every session and injects the top-K matches into the system
//!   prompt.
//! - A `MemoryWriter` hook captures the final assistant text on
//!   `Event::TaskCompleted` and calls [`Memory::write`].
//! - Tools may use the same `Arc<dyn Memory>` to commit explicit facts mid-run.

use serde::{Deserialize, Serialize};

/// One persisted memory record.
///
/// Owned (no borrows) so it round-trips through serde and across .await
/// boundaries cleanly. Fields are intentionally minimal — apps that need
/// richer schemas can wrap this with their own struct and store JSON in
/// `content`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemoryEntry {
    /// Stable id assigned by the backend. Empty if the caller has not yet
    /// committed the entry.
    #[serde(default)]
    pub id: String,
    /// Free-form fact / summary text. This is what recall returns and what
    /// gets injected into a future system prompt.
    pub content: String,
    /// Optional keywords for cheap retrieval. Backends without semantic
    /// indexing fall back to keyword match across `content` + `tags`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Where the entry came from (session id, user, app name, …). Useful
    /// for debugging and for multi-tenant filtering.
    #[serde(default)]
    pub source: Option<String>,
    /// Milliseconds since unix epoch.
    pub created_ms: i64,
    /// Optional expiry time as milliseconds since unix epoch. `None` =
    /// retain indefinitely. Backends MUST filter expired entries out of
    /// `recall` and MAY drop them on a background compact pass.
    ///
    /// Use `with_ttl_days(N)` to set this relative to now. Use `None` for
    /// stable preferences / identity / long-term project context; use a
    /// finite TTL for ephemeral state (current task params, session-scoped
    /// preferences, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
}

impl MemoryEntry {
    /// Convenience constructor. The backend assigns `id` on write.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            content: content.into(),
            tags: Vec::new(),
            source: None,
            created_ms: 0,
            expires_ms: None,
        }
    }

    pub fn with_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Set `expires_ms` to "now + `days` days" using the system clock. For
    /// tests that need a fixed clock, set `expires_ms` directly.
    pub fn with_ttl_days(mut self, days: u32) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.expires_ms = Some(now_ms + (days as i64) * 86_400_000);
        self
    }

    /// Returns `true` if this entry has an `expires_ms` set and that
    /// timestamp is now in the past. Backends call this from `recall` and
    /// `compact` to skip stale entries.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        matches!(self.expires_ms, Some(t) if t <= now_ms)
    }
}

/// The open-memory primitive.
///
/// Implementations:
/// - **File-backed JSONL** ([`harness_context::FileMemory`]) — append-only,
///   keyword recall, no extra deps. Default for the bundled examples.
/// - Future: SQLite, sled, Postgres, vector-DB-backed semantic recall. Plug
///   in by implementing this trait; nothing else in the framework changes.
#[async_trait::async_trait]
pub trait Memory: Send + Sync {
    /// Return up to `k` entries most relevant to `query`, ordered by
    /// descending relevance. The query is typically the current task
    /// description; backends choose how to score (keyword, embedding, BM25…).
    /// Returning an empty `Vec` is fine and must not be treated as an error.
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError>;

    /// Persist `entry`. The backend assigns the `id` field; callers may
    /// leave it empty. Implementations must be safe to call concurrently
    /// from multiple tasks.
    async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError>;
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    #[error("memory io: {0}")]
    Io(String),
    #[error("memory backend: {0}")]
    Backend(String),
    #[error("memory serde: {0}")]
    Serde(String),
}
