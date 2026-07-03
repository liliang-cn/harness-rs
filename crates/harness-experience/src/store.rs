//! `ExperienceStore` — persist and recall [`Episode`]s over any [`Memory`].
//!
//! The store is backend-agnostic: it writes each episode as a `MemoryEntry`
//! whose content is the episode's natural-language render (so keyword *and*
//! semantic backends can index it) and whose tags include an `experience`
//! marker plus one `tool:<name>` tag per tool used (for tool-based filtering).
//! Pair it with a **semantic** `Memory` (e.g. a CortexDB- or embeddings-backed
//! one) to get semantic recall; with a keyword backend recall is lexical.

use crate::episode::Episode;
use harness_core::{Memory, MemoryEntry, MemoryError};
use std::sync::Arc;

/// Default tag marking an entry as an experience episode.
pub const EXPERIENCE_TAG: &str = "experience";

pub struct ExperienceStore {
    memory: Arc<dyn Memory>,
    source: String,
    tag: String,
}

impl ExperienceStore {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            source: "experience".into(),
            tag: EXPERIENCE_TAG.into(),
        }
    }

    /// Tag entries with a source (e.g. the app / user id) for multi-tenant stores.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Override the marker tag (default `experience`).
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = tag.into();
        self
    }

    fn tags_for(&self, ep: &Episode) -> Vec<String> {
        let mut tags = vec![self.tag.clone()];
        tags.extend(ep.tools.iter().map(|t| format!("tool:{t}")));
        tags.extend(ep.tags.iter().cloned());
        tags
    }

    /// Persist one episode.
    pub async fn record(&self, ep: &Episode) -> Result<(), MemoryError> {
        let entry = MemoryEntry::new(ep.render())
            .with_source(self.source.clone())
            .with_tags(self.tags_for(ep));
        self.memory.write(entry).await
    }

    /// Recall up to `k` episodes most relevant to `situation`. Only entries
    /// carrying the marker tag are returned, reconstructed via [`Episode::parse`].
    pub async fn recall(&self, situation: &str, k: usize) -> Vec<Episode> {
        if k == 0 || situation.trim().is_empty() {
            return Vec::new();
        }
        // Over-fetch a little so tag-filtering doesn't starve the result.
        let hits = match self
            .memory
            .recall(situation, k.saturating_mul(2).max(k))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "experience recall failed");
                return Vec::new();
            }
        };
        hits.into_iter()
            .filter(|e| e.tags.iter().any(|t| t == &self.tag))
            .filter_map(|e| Episode::parse(&e.content))
            .take(k)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    struct VecMemory {
        store: Mutex<Vec<MemoryEntry>>,
    }
    #[async_trait]
    impl Memory for VecMemory {
        async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
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
        async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
            self.store.lock().unwrap().push(entry);
            Ok(())
        }
    }

    #[tokio::test]
    async fn record_then_recall_roundtrips_and_tags_tools() {
        let mem = Arc::new(VecMemory::default());
        let store = ExperienceStore::new(mem.clone()).with_source("t");
        store
            .record(
                &Episode::new(
                    "deploy the website to production",
                    "ran deploy, verified live",
                )
                .with_tools(["read_file", "shell"]),
            )
            .await
            .unwrap();

        // Stored with experience + tool tags.
        let stored = mem.store.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].tags.contains(&"experience".to_string()));
        assert!(stored[0].tags.contains(&"tool:shell".to_string()));

        // Recall by a similar situation.
        let hits = store.recall("deploy website production", 3).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tools, vec!["read_file", "shell"]);
    }

    #[tokio::test]
    async fn recall_ignores_non_experience_entries() {
        let mem = Arc::new(VecMemory::default());
        // A plain memory (no experience tag) that also matches the query.
        mem.write(MemoryEntry::new("deploy notes").with_tags(["misc"]))
            .await
            .unwrap();
        let store = ExperienceStore::new(mem.clone());
        assert!(store.recall("deploy", 5).await.is_empty());
    }
}
