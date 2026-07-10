//! Redact-only memory decorator for persistence boundaries.
//!
//! `RedactingMemory` wraps any `Arc<dyn Memory>` and scrubs PII out of every
//! entry's content on `write` — but, unlike [`GuardedMemory`](crate::GuardedMemory),
//! it **never drops** a record. That's the right trade-off for transcript /
//! experience capture (`harness-experience`), where losing a whole turn would
//! blow a hole in the conversation, but a raw card number must never reach the
//! store (and from there, a CortexDB knowledge graph).
//!
//! The biggest PII leak in an agent is usually the persistence path: full
//! transcripts and tool results streaming into long-term memory. Wrap the
//! `Memory` those writers target and the leak closes in one place — both the
//! transcript writer and the episode store write through the same trait:
//!
//! ```ignore
//! use harness_context::RedactingMemory;
//! // cortex: Arc<dyn Memory> backed by CortexDB
//! let safe: Arc<dyn Memory> = Arc::new(RedactingMemory::new(cortex));
//! spawn_transcript_writer(rx, safe.clone());   // every turn is scrubbed
//! let store = ExperienceStore::new(safe);       // episodes too
//! ```
//!
//! Default policy is [`Redactor::new`] ([`Policy::default`](harness_redact::Policy::default)):
//! cards masked to the last 4, emails / phones labelled, monetary amounts kept
//! (a transcript legitimately discusses prices — we don't blank them out).
//! Swap it with [`with_redactor`](RedactingMemory::with_redactor).

use async_trait::async_trait;
use harness_core::{Memory, MemoryEntry, MemoryError};
use harness_redact::Redactor;
use std::sync::Arc;

/// Wraps any `Arc<dyn Memory>` and redacts PII on `write` without ever dropping
/// the entry. `recall` is pass-through.
pub struct RedactingMemory {
    inner: Arc<dyn Memory>,
    redactor: Redactor,
}

impl RedactingMemory {
    /// Wrap `inner` with the default redaction policy (mask cards, label
    /// email/phone, keep money).
    pub fn new(inner: Arc<dyn Memory>) -> Self {
        Self {
            inner,
            redactor: Redactor::new(),
        }
    }

    /// Use a custom redactor (detector set / policy). Note: any span whose
    /// action is `Block` is still only *labelled* here — `RedactingMemory`
    /// never drops the entry, it just redacts the value. Use
    /// [`GuardedMemory`](crate::GuardedMemory) when you want block-to-drop.
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }
}

#[async_trait]
impl Memory for RedactingMemory {
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.inner.recall(query, k).await
    }

    async fn write(&self, mut entry: MemoryEntry) -> Result<(), MemoryError> {
        let redaction = self.redactor.scrub(&entry.content);
        if redaction.changed() {
            tracing::debug!(
                spans = redaction.spans.len(),
                "redacting memory: scrubbed PII before persist"
            );
            entry.content = redaction.text;
        }
        self.inner.write(entry).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct VecMemory(Mutex<Vec<MemoryEntry>>);
    #[async_trait]
    impl Memory for VecMemory {
        async fn recall(&self, _q: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
            Ok(self.0.lock().unwrap().iter().take(k).cloned().collect())
        }
        async fn write(&self, e: MemoryEntry) -> Result<(), MemoryError> {
            self.0.lock().unwrap().push(e);
            Ok(())
        }
    }

    #[tokio::test]
    async fn scrubs_but_never_drops() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = RedactingMemory::new(inner.clone());
        mem.write(MemoryEntry::new(
            "tool read file: contact a@b.com, card 4111111111111111",
        ))
        .await
        .unwrap();
        let all = inner.recall("", 10).await.unwrap();
        assert_eq!(all.len(), 1, "entry is kept, not dropped");
        assert!(all[0].content.contains("<EMAIL>"));
        assert!(all[0].content.contains("************1111"));
        assert!(!all[0].content.contains("a@b.com"));
        assert!(!all[0].content.contains("4111111111111111"));
    }

    #[tokio::test]
    async fn money_is_kept_in_transcripts() {
        // Unlike GuardedMemory's memory_hygiene policy, a transcript keeps prices.
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = RedactingMemory::new(inner.clone());
        mem.write(MemoryEntry::new("the plan costs $20 per month"))
            .await
            .unwrap();
        let all = inner.recall("", 10).await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].content.contains("$20"));
    }

    #[tokio::test]
    async fn clean_text_passes_through_unchanged() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = RedactingMemory::new(inner.clone());
        mem.write(MemoryEntry::new("user prefers dark mode"))
            .await
            .unwrap();
        let all = inner.recall("", 10).await.unwrap();
        assert_eq!(all[0].content, "user prefers dark mode");
    }

    #[tokio::test]
    async fn tags_and_source_survive_redaction() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = RedactingMemory::new(inner.clone());
        mem.write(
            MemoryEntry::new("email a@b.com")
                .with_source("transcript")
                .with_tags(["role:tool", "session:s1"]),
        )
        .await
        .unwrap();
        let all = inner.recall("", 10).await.unwrap();
        assert_eq!(all[0].source.as_deref(), Some("transcript"));
        assert!(all[0].tags.contains(&"role:tool".to_string()));
        assert!(all[0].content.contains("<EMAIL>"));
    }
}
