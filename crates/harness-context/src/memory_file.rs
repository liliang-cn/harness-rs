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
use std::sync::{Arc, Mutex};

/// JSONL-backed memory store.
pub struct FileMemory {
    path: PathBuf,
    // We serialise file writes via this Mutex so concurrent tools don't
    // interleave half-written lines. Reads stat+read the whole file on each
    // recall — fine for the kilobyte-scale memories these JSONL stores
    // realistically hold.
    write_lock: Mutex<()>,
    /// Last parse, keyed by the file's `(len, mtime)`. See [`Self::read_all`].
    cache: Mutex<Option<(CacheStamp, Arc<Indexed>)>>,
}

/// `(byte length, mtime)` — what we compare to decide a re-parse is needed.
type CacheStamp = (u64, Option<std::time::SystemTime>);

/// Parsed entries plus the lower-cased `content + tags` each one is matched
/// against.
///
/// Scoring used to build that haystack inside the loop — one `to_lowercase()`
/// allocation per entry per recall. At 50 000 entries that is 50 000 `String`
/// allocations on *every model iteration*, and it, not JSON parsing, was where
/// the time actually went: caching the parse alone bought 20%, caching the
/// haystack bought the rest. Held parallel to `entries` rather than inside
/// `MemoryEntry` so the public type stays what it is on disk.
struct Indexed {
    entries: Vec<MemoryEntry>,
    hay: Vec<String>,
}

impl Indexed {
    fn build(entries: Vec<MemoryEntry>) -> Self {
        let hay = entries
            .iter()
            .map(|e| {
                let mut h = e.content.to_lowercase();
                if !e.tags.is_empty() {
                    h.push(' ');
                    h.push_str(&e.tags.join(" ").to_lowercase());
                }
                h
            })
            .collect();
        Self { entries, hay }
    }
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
            cache: Mutex::new(None),
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

    /// Parsed entries, reusing the last parse when the file has not changed.
    ///
    /// `MemoryGuide` calls `recall` on **every model iteration**, and recall
    /// re-read and re-parsed the entire JSONL each time. Measured at 50 000
    /// entries that is 41 ms per iteration for memory and 58 ms for experience
    /// — half a second of pure JSON parsing on a five-round tool turn, for a
    /// file that almost never changes mid-conversation.
    ///
    /// The cache key is `(len, mtime)`. Not a hash: hashing means reading the
    /// bytes, which is most of the cost we are avoiding. Not mtime alone:
    /// filesystems with coarse mtime granularity could hide an append that
    /// happened inside the same tick, and length catches exactly that, this
    /// store being append-only. A rewrite that preserves both length and mtime
    /// would be missed — `expire_now` bumps the file's length in practice, and
    /// the alternative is paying 99 ms per iteration forever to defend against
    /// a case that requires deliberately forging the metadata.
    fn read_all(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        Ok(self.read_shared()?.entries.clone())
    }

    /// The same parse, shared rather than copied.
    ///
    /// `recall` reads every entry and keeps at most `k`, so handing it an owned
    /// `Vec` meant cloning 50 000 `MemoryEntry` — tens of thousands of `String`
    /// allocations — to then throw all but five away. That copy, not the JSON
    /// parsing, was the bulk of the cost the cache was supposed to remove; the
    /// first version of this cache measured no faster because it simply moved
    /// the clone rather than eliminating it. Callers that genuinely need
    /// ownership still get it via [`Self::read_all`].
    fn read_shared(&self) -> Result<Arc<Indexed>, MemoryError> {
        let stamp = std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        if let Some(stamp) = &stamp {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached_stamp, entries)) = cache.as_ref()
                && cached_stamp == stamp
            {
                // Arc clone: one refcount bump, no entry copying.
                return Ok(entries.clone());
            }
        }

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
        // Re-stat rather than trusting the stamp taken before the read: a
        // write landing between the two would otherwise cache the new bytes
        // under the old stamp and pin stale entries until the *next* change.
        let shared = Arc::new(Indexed::build(out));
        if let Ok(m) = std::fs::metadata(&self.path) {
            let fresh: CacheStamp = (m.len(), m.modified().ok());
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((fresh, shared.clone()));
        }
        Ok(shared)
    }
}

#[async_trait::async_trait]
impl Memory for FileMemory {
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        // Shared, not owned: nothing below needs to mutate an entry, and only
        // the k survivors are ever cloned.
        let all = self.read_shared()?;
        if all.entries.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let now_ms = now_ms();
        // Carry the pre-lowercased haystack alongside each live entry, so the
        // scoring loop below allocates nothing at all.
        let live: Vec<(&MemoryEntry, &str)> = all
            .entries
            .iter()
            .zip(all.hay.iter())
            .filter(|(e, _)| !e.is_expired(now_ms))
            .map(|(e, h)| (e, h.as_str()))
            .collect();
        if live.is_empty() {
            return Ok(Vec::new());
        }

        let q_tokens = tokenise(query);
        if q_tokens.is_empty() {
            // No tokens to match on; fall back to most-recent-first so the
            // model still gets *some* useful signal.
            let mut recent: Vec<&MemoryEntry> = live.into_iter().map(|(e, _)| e).collect();
            recent.sort_by_key(|e| std::cmp::Reverse(e.created_ms));
            recent.truncate(k);
            return Ok(recent.into_iter().cloned().collect());
        }

        // Score = number of distinct query tokens that appear in
        // (content + tags). Cheap, no deps. Ties broken by recency.
        let mut scored: Vec<(u32, &MemoryEntry)> = live
            .into_iter()
            .map(|(e, hay)| {
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

/// Split text into match tokens.
///
/// Two grammars, because one does not fit both scripts. Space-delimited text
/// splits on non-alphanumerics, as ever. Han text has no spaces, and han
/// characters *are* `is_alphanumeric()`, so that rule alone turns a whole
/// sentence into a single token — and scoring is `hay.contains(token)`, which
/// then only ever matches a byte-identical string. Every recall a
/// Chinese-speaking user made scored zero and returned nothing, quietly: the
/// empty-token recency fallback did not fire either, because there *was* a
/// token, it just could not match. See
/// `a_chinese_question_recalls_a_chinese_memory`.
///
/// Han runs are therefore emitted as **character bigrams** — the standard cheap
/// stand-in for a segmenter, and the same shape the SQLite recall backend uses.
/// A single-character run is emitted whole (a one-character query is rare, and
/// dropping it would be a second silent hole). Bigrams do make matching
/// fuzzier: "咖啡" matches inside "咖啡馆" and "喝咖啡", which is what we want,
/// at the cost of occasional overlap between unrelated words. Scoring counts
/// distinct matching tokens and sorts by that count, so a genuinely relevant
/// entry still outranks an incidental one-bigram collision.
fn tokenise(s: &str) -> HashSet<String> {
    let lowered = s.to_lowercase();
    let mut out: HashSet<String> = HashSet::new();
    // A "word" here is a maximal alphanumeric run; the han handling happens
    // inside it, since han and latin can sit in one run with no separator
    // ("Rust好用" is a single alphanumeric span).
    for word in lowered.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let mut run: Vec<char> = Vec::new();
        let mut latin = String::new();
        // Flush helpers keep the two scripts from contaminating each other's
        // tokens when they are adjacent.
        let flush_han = |run: &mut Vec<char>, out: &mut HashSet<String>| {
            match run.len() {
                0 => {}
                1 => {
                    out.insert(run[0].to_string());
                }
                _ => {
                    for pair in run.windows(2) {
                        out.insert(pair.iter().collect());
                    }
                }
            }
            run.clear();
        };
        let flush_latin = |latin: &mut String, out: &mut HashSet<String>| {
            if latin.len() >= 3 {
                out.insert(std::mem::take(latin));
            } else {
                latin.clear();
            }
        };
        for c in word.chars() {
            if is_han(c) {
                flush_latin(&mut latin, &mut out);
                run.push(c);
            } else {
                flush_han(&mut run, &mut out);
                latin.push(c);
            }
        }
        flush_han(&mut run, &mut out);
        flush_latin(&mut latin, &mut out);
    }
    out
}

/// CJK ideographs, the ranges that actually occur in Chinese and Japanese text.
///
/// Kana and hangul are deliberately excluded: they are far closer to
/// alphabetic scripts, and bigramming them would add noise without fixing a
/// matching failure the existing rule does not already handle.
fn is_han(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF     // CJK Unified Ideographs
        | 0x3400..=0x4DBF   // Extension A
        | 0xF900..=0xFAFF   // Compatibility Ideographs
        | 0x20000..=0x2A6DF // Extension B
    )
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

    /// A natural Chinese question must find a Chinese memory.
    ///
    /// It did not. `tokenise` split on `!is_alphanumeric()`, and han characters
    /// ARE alphanumeric, so a space-free sentence collapsed into a single token
    /// that `hay.contains()` could only match against a byte-identical string.
    /// Every recall for a Chinese-speaking user scored zero and returned
    /// nothing — silently, since the empty-token recency fallback never fired
    /// (there *was* a token, it just matched nothing). The feature looked
    /// present and did nothing, which is the worst way for it to be broken.
    #[tokio::test]
    async fn a_chinese_question_recalls_a_chinese_memory() {
        let p = tmp();
        let m = FileMemory::open(&p).unwrap();
        for fact in [
            "用户喜欢喝手冲咖啡，不加糖",
            "用户住在成都，时区是 Asia/Shanghai",
            "用户对花生过敏",
        ] {
            m.write(MemoryEntry::new(fact)).await.unwrap();
        }

        // Not a substring of any stored fact — the whole point.
        let hits = m.recall("我平时喝什么咖啡？", 3).await.unwrap();
        assert!(
            hits.iter().any(|e| e.content.contains("手冲咖啡")),
            "coffee memory not recalled; got {:?}",
            hits.iter().map(|e| &e.content).collect::<Vec<_>>()
        );

        // And it must still discriminate: an unrelated question must not drag
        // the whole store back. Bigrams make matching fuzzier, so this is the
        // guard against the fix turning recall into "return everything".
        let unrelated = m.recall("帮我订一张去北京的机票", 3).await.unwrap();
        assert!(
            !unrelated.iter().any(|e| e.content.contains("过敏")),
            "unrelated query matched the allergy note: {unrelated:?}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn tokenise_splits_han_into_bigrams_and_leaves_latin_alone() {
        let t = tokenise("手冲咖啡");
        assert!(t.contains("手冲"), "{t:?}");
        assert!(t.contains("冲咖"), "{t:?}");
        assert!(t.contains("咖啡"), "{t:?}");
        // Latin behaviour is unchanged, including the tiny-token filter.
        let l = tokenise("the quick brown fox is a");
        assert!(l.contains("quick") && l.contains("brown"));
        assert!(!l.contains("is") && !l.contains("a"));
        // Mixed text yields both kinds.
        let mx = tokenise("用户喜欢 Rust");
        assert!(mx.contains("rust"), "{mx:?}");
        assert!(mx.contains("用户"), "{mx:?}");
    }

    /// The cache must never outlive the write that invalidates it.
    ///
    /// This is the failure mode a `(len, mtime)` cache actually risks: an
    /// append inside the same mtime tick as the read that populated the cache.
    /// A user says "记一下我对花生过敏" and asks about it in the next breath —
    /// if that lands in one tick and length were not part of the key, the
    /// recall would come back empty and the agent would say it never heard it.
    #[tokio::test]
    async fn a_write_is_visible_to_the_very_next_recall() {
        let p = tmp();
        let m = FileMemory::open(&p).unwrap();
        m.write(MemoryEntry::new("first fact about coffee"))
            .await
            .unwrap();
        assert_eq!(m.recall("coffee", 10).await.unwrap().len(), 1);

        // Same tick as the recall above, on any realistic filesystem.
        m.write(MemoryEntry::new("second fact about coffee"))
            .await
            .unwrap();
        assert_eq!(
            m.recall("coffee", 10).await.unwrap().len(),
            2,
            "cache served a stale parse across an append"
        );

        // And across the rewrite paths, which change content without appending.
        let id = m.recall("coffee", 10).await.unwrap()[0].id.clone();
        assert!(m.delete_by_id(&id).unwrap());
        assert_eq!(
            m.recall("coffee", 10).await.unwrap().len(),
            1,
            "cache served a stale parse across a delete"
        );

        m.delete_all().unwrap();
        assert!(
            m.recall("coffee", 10).await.unwrap().is_empty(),
            "cache served a stale parse across delete_all"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// An unchanged file must not be re-parsed.
    ///
    /// Proved positively rather than by timing: rewrite the file with
    /// *different* content of the *same* length and restore the mtime, so the
    /// cache key is byte-identical to what it was. A recall that still returns
    /// the old content can only have come from the cache. (A test that just
    /// calls recall twice and compares would pass whether or not the cache
    /// exists, which is why it is not written that way.)
    #[tokio::test]
    async fn an_unchanged_stamp_serves_the_cached_parse() {
        let p = tmp();
        let m = FileMemory::open(&p).unwrap();
        m.write(MemoryEntry::new("aaaa")).await.unwrap();
        assert_eq!(m.recall("aaaa", 10).await.unwrap().len(), 1);

        let before = std::fs::metadata(&p).unwrap();
        let original = std::fs::read_to_string(&p).unwrap();
        // Same byte length, different content.
        let swapped = original.replace("aaaa", "bbbb");
        assert_eq!(swapped.len(), original.len());
        std::fs::write(&p, &swapped).unwrap();
        // Put the clock back so (len, mtime) is exactly what was cached.
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_accessed(before.accessed().unwrap())
                .set_modified(before.modified().unwrap()),
        )
        .unwrap();
        drop(f);

        assert_eq!(
            m.recall("aaaa", 10).await.unwrap().len(),
            1,
            "the file did not change by its stamp, so the cache should have answered"
        );
        assert!(
            m.recall("bbbb", 10).await.unwrap().is_empty(),
            "re-read a file whose stamp was unchanged — the cache is not being used"
        );
        let _ = std::fs::remove_file(&p);
    }
}
