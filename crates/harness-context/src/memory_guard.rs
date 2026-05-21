//! Memory write-time guards: dedup + sensitivity filter.
//!
//! `GuardedMemory` wraps any `Arc<dyn Memory>` and runs two cheap checks
//! before letting `write` through to the inner store:
//!
//! 1. **Sensitivity gate** — regex match against a configurable set of
//!    patterns (credit card numbers, emails, phone, monetary amounts). If
//!    the entry content trips any pattern, the write is silently dropped
//!    (logged at info level). Defaults aim for "do no harm in personal /
//!    financial contexts"; turn them off with `.without_default_sensitivity()`.
//!
//! 2. **Dedup** — calls `inner.recall(entry.content, 5)` and compares each
//!    candidate's content tokens against the new entry's tokens. If the
//!    Jaccard similarity exceeds `dedup_threshold` (default 0.6) for ANY
//!    candidate, the write is dropped — the existing entry already covers
//!    this fact, no need to inflate the file.
//!
//! `recall` and the underlying file ops are pass-through.
//!
//! Layered design — apply on top of `FileMemory` (or any other backend):
//!
//! ```ignore
//! let file_mem = FileMemory::open(path)?;
//! let memory: Arc<dyn Memory> = Arc::new(
//!     GuardedMemory::new(Arc::new(file_mem))
//!         .with_blocked_substring("password")
//!         .with_dedup_threshold(0.55)
//! );
//! ```

use async_trait::async_trait;
use harness_core::{Memory, MemoryEntry, MemoryError};
use regex::Regex;
use std::collections::HashSet;
use std::sync::Arc;

/// Wraps any `Arc<dyn Memory>` and adds dedup + sensitivity filtering on
/// `write`. `recall` is pass-through.
pub struct GuardedMemory {
    inner: Arc<dyn Memory>,
    sensitivity_patterns: Vec<Regex>,
    blocked_substrings: Vec<String>,
    dedup_threshold: f32,
    dedup_recall_k: usize,
}

impl GuardedMemory {
    /// Wrap `inner` with the default sensitivity patterns (credit-card-like
    /// 13-19 digit runs, emails, common monetary patterns) and a dedup
    /// threshold of 0.6 Jaccard token overlap.
    pub fn new(inner: Arc<dyn Memory>) -> Self {
        Self {
            inner,
            sensitivity_patterns: default_sensitivity_patterns(),
            blocked_substrings: Vec::new(),
            dedup_threshold: 0.6,
            dedup_recall_k: 5,
        }
    }

    /// Drop the default sensitivity patterns — useful for tests or when
    /// callers know they're storing already-redacted content.
    pub fn without_default_sensitivity(mut self) -> Self {
        self.sensitivity_patterns.clear();
        self
    }

    /// Add an extra regex pattern. If the entry content matches ANY
    /// pattern, the write is dropped.
    pub fn with_sensitivity_pattern(mut self, pat: impl AsRef<str>) -> Result<Self, regex::Error> {
        self.sensitivity_patterns.push(Regex::new(pat.as_ref())?);
        Ok(self)
    }

    /// Add a literal substring to the block-list (case-insensitive contains).
    /// Cheaper than a regex; use for app-specific terms that should never
    /// hit memory (e.g. `"password"`, `"内部秘钥"`).
    pub fn with_blocked_substring(mut self, s: impl Into<String>) -> Self {
        self.blocked_substrings.push(s.into().to_lowercase());
        self
    }

    /// Override the Jaccard token-overlap threshold above which an entry is
    /// considered a duplicate of an existing one. Range [0.0, 1.0]; default
    /// 0.6. Set to 1.0 to require exact match, 0.0 to disable dedup.
    pub fn with_dedup_threshold(mut self, t: f32) -> Self {
        self.dedup_threshold = t.clamp(0.0, 1.0);
        self
    }

    /// How many candidates to fetch from `recall` for dedup comparison.
    /// Default 5. Increase if your store gets large and recall miss rate
    /// is high; decrease for tiny stores.
    pub fn with_dedup_recall_k(mut self, k: usize) -> Self {
        self.dedup_recall_k = k.max(1);
        self
    }

    fn is_sensitive(&self, content: &str) -> bool {
        let lower = content.to_lowercase();
        if self.blocked_substrings.iter().any(|s| lower.contains(s)) {
            return true;
        }
        self.sensitivity_patterns.iter().any(|r| r.is_match(content))
    }

    async fn is_duplicate(&self, entry: &MemoryEntry) -> bool {
        if self.dedup_threshold <= 0.0 {
            return false;
        }
        let cands = match self.inner.recall(&entry.content, self.dedup_recall_k).await {
            Ok(v) => v,
            Err(_) => return false,
        };
        let new_tokens = jaccard_tokens(&entry.content);
        if new_tokens.is_empty() {
            return false;
        }
        for c in cands {
            let cand_tokens = jaccard_tokens(&c.content);
            if jaccard(&new_tokens, &cand_tokens) >= self.dedup_threshold {
                return true;
            }
        }
        false
    }
}

#[async_trait]
impl Memory for GuardedMemory {
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.inner.recall(query, k).await
    }

    async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        if self.is_sensitive(&entry.content) {
            tracing::info!(
                content_preview = %entry.content.chars().take(40).collect::<String>(),
                "guarded memory: dropping sensitive entry"
            );
            return Ok(());
        }
        if self.is_duplicate(&entry).await {
            tracing::info!(
                content_preview = %entry.content.chars().take(40).collect::<String>(),
                "guarded memory: dropping duplicate entry"
            );
            return Ok(());
        }
        self.inner.write(entry).await
    }
}

fn default_sensitivity_patterns() -> Vec<Regex> {
    [
        // 13-19 consecutive digits (credit card-ish — covers most PANs)
        r"\b\d{13,19}\b",
        // Emails
        r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
        // Chinese mainland mobile (1 + 10 digits)
        r"\b1[3-9]\d{9}\b",
        // Common monetary amount mentions — ¥1234.56, $1234, USD 1234,
        // CNY 1234.56 (account flows that should live in the txns table,
        // not in long-term memory)
        r"[¥$€£₹]\s?\d+(?:[.,]\d+)?",
        r"\b(?:USD|CNY|EUR|RMB|HKD|JPY)\s?\d+(?:[.,]\d+)?\b",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
}

fn jaccard_tokens(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(String::from)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 { 0.0 } else { inter / union }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Memory;
    use std::sync::Mutex;

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
        ) -> Result<Vec<MemoryEntry>, MemoryError> {
            // Mimic FileMemory: substring-contains scoring against lowercased
            // content + tags. Plain Jaccard exact-token wouldn't substring-
            // match CJK content (where the whole string is one big token).
            let g = self.store.lock().unwrap();
            let q_tokens = jaccard_tokens(query);
            if q_tokens.is_empty() {
                return Ok(g.iter().take(k).cloned().collect());
            }
            let mut scored: Vec<(u32, &MemoryEntry)> = g
                .iter()
                .map(|e| {
                    let hay = e.content.to_lowercase();
                    let hits: u32 = q_tokens
                        .iter()
                        .map(|t| if hay.contains(t.as_str()) { 1 } else { 0 })
                        .sum();
                    (hits, e)
                })
                .filter(|(hits, _)| *hits > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            Ok(scored.into_iter().take(k).map(|(_, e)| e.clone()).collect())
        }
        async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
            self.store.lock().unwrap().push(entry);
            Ok(())
        }
    }

    #[tokio::test]
    async fn sensitive_credit_card_is_dropped() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = GuardedMemory::new(inner.clone());
        mem.write(MemoryEntry::new("user's card is 4111111111111111 expiry 12/30"))
            .await
            .unwrap();
        let all = inner.recall("card", 10).await.unwrap();
        assert!(all.is_empty(), "credit-card-like content should be dropped");
    }

    #[tokio::test]
    async fn sensitive_email_is_dropped() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = GuardedMemory::new(inner.clone());
        mem.write(MemoryEntry::new("user's email is ll_faw@hotmail.com"))
            .await
            .unwrap();
        let all = inner.recall("email", 10).await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn monetary_amounts_are_dropped() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = GuardedMemory::new(inner.clone());
        mem.write(MemoryEntry::new("用户记录了一笔 ¥199 火锅消费")).await.unwrap();
        mem.write(MemoryEntry::new("user spent USD 250 on Claude Code")).await.unwrap();
        let all = inner.recall("user", 10).await.unwrap();
        assert!(all.is_empty(), "monetary patterns should be filtered: {all:?}");
    }

    #[tokio::test]
    async fn durable_preferences_pass_through() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = GuardedMemory::new(inner.clone());
        mem.write(MemoryEntry::new("用户偏好使用微信支付餐饮类支出")).await.unwrap();
        mem.write(MemoryEntry::new("user prefers concise replies in Slack style"))
            .await
            .unwrap();
        let all = inner.recall("用户", 10).await.unwrap();
        assert_eq!(all.len(), 1, "preference about 用户 should be kept");
    }

    #[tokio::test]
    async fn duplicate_is_dropped() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = GuardedMemory::new(inner.clone()).with_dedup_threshold(0.6);
        mem.write(MemoryEntry::new(
            "user prefers concise replies written in Slack style",
        ))
        .await
        .unwrap();
        // Near-duplicate phrasing → tokens overlap ≥ 0.6 → should be dropped.
        mem.write(MemoryEntry::new("user prefers concise replies in Slack tone"))
            .await
            .unwrap();
        let all = inner.recall("user", 10).await.unwrap();
        assert_eq!(all.len(), 1, "near-duplicate should not double-store: {all:?}");
    }

    #[tokio::test]
    async fn blocked_substring_works() {
        let inner: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let mem = GuardedMemory::new(inner.clone()).with_blocked_substring("password");
        mem.write(MemoryEntry::new("user's password reset is hunter2"))
            .await
            .unwrap();
        let all = inner.recall("password", 10).await.unwrap();
        assert!(all.is_empty());
    }
}
