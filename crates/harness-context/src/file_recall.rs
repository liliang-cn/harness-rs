//! File-backed [`RecallStore`]: append-only JSONL transcripts, one directory
//! per owner. Open-format, greppable, operator-owned — same posture as
//! [`crate::FileMemory`]. Search is a linear token-overlap scan (no FTS), fine
//! at kilobyte–MB scale; apps at scale use `harness-recall-sqlite` instead.
//!
//! Layout under `root`:
//! ```text
//! <root>/<owner>/<session_id>.jsonl       one RecallMessage per line (id = line no.)
//! <root>/<owner>/<session_id>.meta.json   SessionMeta sidecar
//! ```

use harness_core::{RecallError, RecallMessage, RecallStore, SessionHit, SessionMeta};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct FileRecall {
    root: PathBuf,
    write_lock: Mutex<()>,
}

impl FileRecall {
    /// Open (or create) a recall root directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, RecallError> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|e| RecallError::Io(format!("create root {}: {e}", root.display())))?;
        Ok(Self {
            root,
            write_lock: Mutex::new(()),
        })
    }

    fn owner_dir(&self, owner: &str) -> PathBuf {
        self.root.join(sanitize(owner))
    }
    fn session_path(&self, owner: &str, session_id: &str) -> PathBuf {
        self.owner_dir(owner)
            .join(format!("{}.jsonl", sanitize(session_id)))
    }
    fn meta_path(&self, owner: &str, session_id: &str) -> PathBuf {
        self.owner_dir(owner)
            .join(format!("{}.meta.json", sanitize(session_id)))
    }

    fn read_messages(&self, owner: &str, session_id: &str) -> Vec<RecallMessage> {
        let path = self.session_path(owner, session_id);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for (i, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<RecallMessage>(line) {
                Ok(mut m) => {
                    m.id = (i + 1) as i64; // id = 1-based line number
                    out.push(m);
                }
                Err(err) => {
                    tracing::warn!(line = i + 1, error = %err, "recall line skipped");
                }
            }
        }
        out
    }

    fn read_meta(&self, owner: &str, session_id: &str) -> Option<SessionMeta> {
        let path = self.meta_path(owner, session_id);
        let s = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str::<SessionMeta>(&s).ok()
    }

    fn write_meta(&self, owner: &str, m: &SessionMeta) -> Result<(), RecallError> {
        let path = self.meta_path(owner, &m.session_id);
        let s = serde_json::to_string(m).map_err(|e| RecallError::Serde(e.to_string()))?;
        std::fs::write(&path, s).map_err(|e| RecallError::Io(e.to_string()))
    }
}

fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    };
    if cleaned.chars().count() > 120 {
        cleaned.chars().take(120).collect()
    } else {
        cleaned
    }
}

fn tokenise(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Cut a ~80-char snippet centred on the first matched token, marked `>>>…<<<`.
fn make_snippet(content: &str, q_tokens: &[String]) -> String {
    let lower = content.to_lowercase();
    let hit = q_tokens
        .iter()
        .filter_map(|t| lower.find(t.as_str()).map(|pos| (pos, t.len())))
        .min_by_key(|(pos, _)| *pos);
    match hit {
        Some((pos, len))
            if content.is_char_boundary(pos) && content.is_char_boundary(pos + len) =>
        {
            let start = pos.saturating_sub(40);
            let end = (pos + len + 40).min(content.len());
            let start = (0..=start)
                .rev()
                .find(|i| content.is_char_boundary(*i))
                .unwrap_or(0);
            let end = (end..=content.len())
                .find(|i| content.is_char_boundary(*i))
                .unwrap_or(content.len());
            let mut s = String::new();
            if start > 0 {
                s.push('…');
            }
            s.push_str(&content[start..pos]);
            s.push_str(">>>");
            s.push_str(&content[pos..pos + len]);
            s.push_str("<<<");
            s.push_str(&content[pos + len..end]);
            if end < content.len() {
                s.push('…');
            }
            s
        }
        _ => content.chars().take(80).collect(),
    }
}

#[async_trait::async_trait]
impl RecallStore for FileRecall {
    async fn ensure_session(
        &self,
        owner: &str,
        session_id: &str,
        meta: &SessionMeta,
    ) -> Result<(), RecallError> {
        let _g = self
            .write_lock
            .lock()
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        std::fs::create_dir_all(self.owner_dir(owner))
            .map_err(|e| RecallError::Io(e.to_string()))?;
        if self.read_meta(owner, session_id).is_none() {
            let mut m = meta.clone();
            m.session_id = session_id.to_string();
            self.write_meta(owner, &m)?;
        }
        Ok(())
    }

    async fn append(
        &self,
        owner: &str,
        session_id: &str,
        msg: &RecallMessage,
    ) -> Result<i64, RecallError> {
        let _g = self
            .write_lock
            .lock()
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        std::fs::create_dir_all(self.owner_dir(owner))
            .map_err(|e| RecallError::Io(e.to_string()))?;
        let line = serde_json::to_string(msg).map_err(|e| RecallError::Serde(e.to_string()))?;
        let path = self.session_path(owner, session_id);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| RecallError::Io(e.to_string()))?;
        writeln!(f, "{line}").map_err(|e| RecallError::Io(e.to_string()))?;
        // Derive the new message's id from its actual assigned line number.
        let msgs = self.read_messages(owner, session_id);
        let id = msgs.last().map(|m| m.id).unwrap_or(1);
        let mut meta = self
            .read_meta(owner, session_id)
            .unwrap_or_else(|| SessionMeta::new(session_id, msg.ts_ms));
        meta.message_count = msgs.len() as i64;
        let _ = self.write_meta(owner, &meta);
        Ok(id)
    }

    async fn search(
        &self,
        owner: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionHit>, RecallError> {
        let q = tokenise(query);
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let dir = self.owner_dir(owner);
        let mut hits: Vec<(u32, i64, SessionHit)> = Vec::new(); // (score, started_at, hit)
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let session_id = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let msgs = self.read_messages(owner, &session_id);
            if msgs.is_empty() {
                continue;
            }
            // Best-matching message in this session.
            let mut best: Option<(u32, &RecallMessage)> = None;
            for m in &msgs {
                let hay = m.content.to_lowercase();
                let score: u32 = q
                    .iter()
                    .map(|t| if hay.contains(t.as_str()) { 1 } else { 0 })
                    .sum();
                if score > 0 && best.map(|(s, _)| score > s).unwrap_or(true) {
                    best = Some((score, m));
                }
            }
            let Some((score, anchor)) = best else {
                continue;
            };
            let meta = self
                .read_meta(owner, &session_id)
                .unwrap_or_else(|| SessionMeta::new(&session_id, msgs[0].ts_ms));
            let started = meta.started_at_ms;
            let around: Vec<RecallMessage> = msgs
                .iter()
                .filter(|m| (m.id - anchor.id).abs() <= 5)
                .cloned()
                .collect();
            let bookend_start: Vec<RecallMessage> = msgs.iter().take(3).cloned().collect();
            let bookend_end: Vec<RecallMessage> =
                msgs.iter().rev().take(3).rev().cloned().collect();
            hits.push((
                score,
                started,
                SessionHit::new(
                    meta,
                    make_snippet(&anchor.content, &q),
                    anchor.id,
                    bookend_start,
                    around,
                    bookend_end,
                ),
            ));
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        Ok(hits.into_iter().take(limit).map(|(_, _, h)| h).collect())
    }

    async fn scroll(
        &self,
        owner: &str,
        session_id: &str,
        around: i64,
        window: usize,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        let msgs = self.read_messages(owner, session_id);
        let w = window as i64;
        Ok(msgs
            .into_iter()
            .filter(|m| (m.id - around).abs() <= w)
            .collect())
    }

    async fn recent(&self, owner: &str, limit: usize) -> Result<Vec<SessionMeta>, RecallError> {
        let dir = self.owner_dir(owner);
        let mut metas: Vec<SessionMeta> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json")
                    && p.to_string_lossy().ends_with(".meta.json")
                    && let Ok(s) = std::fs::read_to_string(&p)
                    && let Ok(m) = serde_json::from_str::<SessionMeta>(&s)
                {
                    metas.push(m);
                }
            }
        }
        metas.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));
        metas.truncate(limit);
        Ok(metas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);
    fn tmp_root() -> PathBuf {
        let pid = std::process::id();
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("harness-recall-test-{pid}-{nanos}-{n}"))
    }

    #[tokio::test]
    async fn append_then_search_and_scroll() {
        let root = tmp_root();
        let r = FileRecall::open(&root).unwrap();
        r.ensure_session("u1", "s1", &SessionMeta::new("s1", 100))
            .await
            .unwrap();
        r.append(
            "u1",
            "s1",
            &RecallMessage::new("user", "let us refactor the auth module", 100),
        )
        .await
        .unwrap();
        r.append(
            "u1",
            "s1",
            &RecallMessage::new("assistant", "sure, starting auth refactor", 101),
        )
        .await
        .unwrap();

        let hits = r.search("u1", "auth refactor", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session.session_id, "s1");
        assert!(hits[0].snippet.contains(">>>"));
        assert!(!hits[0].bookend_start.is_empty());

        let scrolled = r.scroll("u1", "s1", 1, 1).await.unwrap();
        assert!(scrolled.iter().any(|m| m.id == 1));

        let recent = r.recent("u1", 10).await.unwrap();
        assert_eq!(recent.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn malformed_line_skipped() {
        let root = tmp_root();
        let r = FileRecall::open(&root).unwrap();
        r.ensure_session("u1", "s1", &SessionMeta::new("s1", 1))
            .await
            .unwrap();
        // hand-write a bad line then a good one
        let path = r.session_path("u1", "s1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{bad\n{\"id\":0,\"role\":\"user\",\"content\":\"hello world\",\"ts_ms\":1}\n",
        )
        .unwrap();
        let hits = r.search("u1", "hello", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cjk_owner_and_unicode_content_do_not_panic() {
        let root = tmp_root();
        let r = FileRecall::open(&root).unwrap();
        let owner = "用户".repeat(50); // >120 bytes, multi-byte
        r.ensure_session(&owner, "s1", &SessionMeta::new("s1", 1))
            .await
            .unwrap();
        r.append(
            &owner,
            "s1",
            &RecallMessage::new("user", "İstanbul café 支付服务 refactor", 1),
        )
        .await
        .unwrap();
        // search must not panic on the mixed-case/Unicode snippet path
        let _ = r.search(&owner, "refactor", 5).await.unwrap();
        let _ = r.search(&owner, "İstanbul", 5).await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
