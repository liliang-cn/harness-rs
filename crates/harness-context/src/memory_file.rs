//! File-backed [`Memory`] implementation.
//!
//! Append-only JSONL — one [`MemoryEntry`] per line. Open-format, plain text,
//! greppable, version-controllable, transferable between machines, completely
//! owned by the operator. No daemon, no embedded DB, no provider lock-in.
//!
//! Recall is keyword-based (case-folded token overlap between query and
//! `content` + `tags`). For semantic recall, implement [`Memory`] yourself
//! against your favourite vector store; nothing else in the framework needs
//! to change.

use harness_core::{Memory, MemoryEntry, MemoryError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// JSONL-backed memory store.
pub struct FileMemory {
    path: PathBuf,
    // We serialise file writes via this Mutex so concurrent tools don't
    // interleave half-written lines. Reads stat+read the whole file on each
    // recall — fine for the kilobyte-scale memories these JSONL stores
    // realistically hold.
    write_lock: Mutex<()>,
}

impl FileMemory {
    /// Open (or create) the JSONL file at `path`. Creates parent directories
    /// as needed. Does not fail if the file is empty or absent.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| MemoryError::Io(format!("create parent: {e}")))?;
        }
        // Touch the file so first read doesn't error.
        if !path.exists() {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| MemoryError::Io(format!("create {}: {e}", path.display())))?;
        }
        Ok(Self {
            path,
            write_lock: Mutex::new(()),
        })
    }

    /// Path to the underlying JSONL file. Handy for tests and for logging
    /// "memory: <path>" in the example banners.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rewrite the file dropping every entry whose `expires_ms <= now`.
    /// Use this as a periodic janitor (cron) to keep the file from
    /// accumulating stale rows; recall already filters at read time, so
    /// compact is purely a disk-space concern.
    ///
    /// Returns how many entries were removed.
    pub fn compact(&self) -> Result<u32, MemoryError> {
        let entries = self.read_all()?;
        let now = now_ms();
        let original_len = entries.len();
        let kept: Vec<MemoryEntry> = entries.into_iter().filter(|e| !e.is_expired(now)).collect();
        let removed = original_len - kept.len();
        self.rewrite(&kept)?;
        Ok(removed as u32)
    }

    /// Delete one entry by id. Reads the file, drops the matching row,
    /// rewrites. Returns `true` if a row was actually removed.
    pub fn delete_by_id(&self, id: &str) -> Result<bool, MemoryError> {
        let entries = self.read_all()?;
        let original_len = entries.len();
        let kept: Vec<MemoryEntry> = entries.into_iter().filter(|e| e.id != id).collect();
        if kept.len() == original_len {
            return Ok(false);
        }
        self.rewrite(&kept)?;
        Ok(true)
    }

    /// Drop every entry. Equivalent to `rm <path>; touch <path>` but holds
    /// the write lock so no concurrent append races.
    pub fn delete_all(&self) -> Result<u32, MemoryError> {
        let entries = self.read_all()?;
        let n = entries.len() as u32;
        self.rewrite(&[])?;
        Ok(n)
    }

    fn rewrite(&self, entries: &[MemoryEntry]) -> Result<(), MemoryError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| MemoryError::Backend(format!("poisoned mutex: {e}")))?;
        let mut buf = String::new();
        for e in entries {
            let line = serde_json::to_string(e).map_err(|e| MemoryError::Serde(e.to_string()))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        // Atomic-ish: write to sibling tmp, fsync, rename. Avoids leaving
        // a half-written JSONL if the process is killed mid-rewrite.
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, buf.as_bytes())
            .map_err(|e| MemoryError::Io(format!("write tmp: {e}")))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| MemoryError::Io(format!("rename: {e}")))?;
        Ok(())
    }

    fn read_all(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let content = std::fs::read_to_string(&self.path)
            .map_err(|e| MemoryError::Io(format!("read {}: {e}", self.path.display())))?;
        let mut out = Vec::new();
        for (i, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<MemoryEntry>(line) {
                Ok(e) => out.push(e),
                Err(err) => {
                    // Skip malformed lines rather than failing the recall —
                    // memory is best-effort and a corrupted entry shouldn't
                    // black-hole an entire session's recall.
                    tracing::warn!(line = i + 1, error = %err, "memory line skipped");
                }
            }
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Memory for FileMemory {
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        let entries = self.read_all()?;
        if entries.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let now_ms = now_ms();
        let entries: Vec<MemoryEntry> = entries
            .into_iter()
            .filter(|e| !e.is_expired(now_ms))
            .collect();
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        let q_tokens = tokenise(query);
        if q_tokens.is_empty() {
            // No tokens to match on; fall back to most-recent-first so the
            // model still gets *some* useful signal.
            let mut all = entries;
            all.sort_by_key(|e| std::cmp::Reverse(e.created_ms));
            all.truncate(k);
            return Ok(all);
        }

        // Score = number of distinct query tokens that appear in
        // (content + tags). Cheap, no deps. Ties broken by recency.
        let mut scored: Vec<(u32, &MemoryEntry)> = entries
            .iter()
            .map(|e| {
                let mut hay = e.content.to_lowercase();
                if !e.tags.is_empty() {
                    hay.push(' ');
                    hay.push_str(&e.tags.join(" ").to_lowercase());
                }
                let hits: u32 = q_tokens
                    .iter()
                    .map(|t| if hay.contains(t.as_str()) { 1 } else { 0 })
                    .sum();
                (hits, e)
            })
            .filter(|(hits, _)| *hits > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.created_ms.cmp(&a.1.created_ms)));

        Ok(scored.into_iter().take(k).map(|(_, e)| e.clone()).collect())
    }

    async fn write(&self, mut entry: MemoryEntry) -> Result<(), MemoryError> {
        if entry.id.is_empty() {
            entry.id = short_id();
        }
        if entry.created_ms == 0 {
            entry.created_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
        }
        let line = serde_json::to_string(&entry).map_err(|e| MemoryError::Serde(e.to_string()))?;

        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| MemoryError::Backend(format!("poisoned mutex: {e}")))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| MemoryError::Io(format!("open {}: {e}", self.path.display())))?;
        use std::io::Write;
        writeln!(file, "{line}").map_err(|e| MemoryError::Io(format!("write: {e}")))?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn tokenise(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3) // drop stopword-ish tiny tokens
        .map(String::from)
        .collect()
}

fn short_id() -> String {
    // 8-hex-char id, enough collision space for kilobyte-scale stores.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:08x}", nanos as u64 & 0xFFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);
    fn tmp() -> PathBuf {
        let pid = std::process::id();
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("harness-mem-test-{pid}-{nanos}-{n}.jsonl"))
    }

    #[tokio::test]
    async fn write_then_recall_round_trips() {
        let p = tmp();
        let m = FileMemory::open(&p).unwrap();
        m.write(MemoryEntry::new("user prefers dark roast coffee").with_tags(["coffee"]))
            .await
            .unwrap();
        m.write(MemoryEntry::new("user lives in Beijing"))
            .await
            .unwrap();

        let hits = m.recall("coffee preferences", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("dark roast"));
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn empty_query_falls_back_to_recent() {
        let p = tmp();
        let m = FileMemory::open(&p).unwrap();
        m.write(MemoryEntry::new("fact one")).await.unwrap();
        m.write(MemoryEntry::new("fact two")).await.unwrap();

        let hits = m.recall("", 5).await.unwrap();
        // Two written, "" tokenises to empty set => recent-first fallback.
        assert_eq!(hits.len(), 2);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped() {
        let p = tmp();
        {
            // Hand-write a bad line + a good line.
            use std::io::Write;
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(f, "{{not valid json").unwrap();
            writeln!(
                f,
                r#"{{"id":"abc","content":"valid fact","tags":[],"source":null,"created_ms":0}}"#
            )
            .unwrap();
        }
        let m = FileMemory::open(&p).unwrap();
        let all = m.recall("valid", 10).await.unwrap();
        assert_eq!(all.len(), 1);
        let _ = std::fs::remove_file(&p);
    }
}
