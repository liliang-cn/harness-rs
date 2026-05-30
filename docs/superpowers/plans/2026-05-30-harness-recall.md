# harness-recall Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in cross-session recall capability to the harness-rs framework so any app gets `AgentLoop::new(model).with_recall(store)` → the agent captures every turn and can search its own past sessions, owner-scoped.

**Architecture:** `RecallStore` trait + data types in harness-core (zero new deps). A file-based default `FileRecall` (JSONL) in harness-context, mirroring `FileMemory`. `SessionSearchTool` + opt-in `RecallGuide` + `.with_recall()`/`.auto_inject()` builder + best-effort capture points in harness-loop. A separate **optional** crate `harness-recall-sqlite` carries the only rusqlite dependency (FTS5/trigram/LIKE, faithful Hermes port). A shared contract test suite runs against both impls.

**Tech Stack:** Rust, async-trait, serde/serde_json, thiserror (all already in harness-core); rusqlite (bundled, FTS5) only in the new optional crate.

**Spec:** `docs/superpowers/specs/2026-05-30-harness-recall-design.md`

**Conventions (verified against the codebase):**
- Crate `[package] name` is `harness-rs-<x>`; directory + workspace member is `crates/harness-<x>`; deps are referenced by the workspace alias `harness-core = { workspace = true }`.
- harness-core modules: each file gets `pub mod <m>;` + `pub use <m>::*;` in `crates/harness-core/src/lib.rs`.
- `Tool` trait: `fn name(&self)->&str; fn schema(&self)->&ToolSchema; fn risk(&self)->ToolRisk; async fn invoke(&self, args: serde_json::Value, world: &mut World) -> Result<ToolResult, ToolError>`. `ToolResult { ok, content, trace }`. `ToolError::{InvalidArgs{name,reason}, Exec(String)}`.
- `World.profile.extra` is `BTreeMap<String, serde_json::Value>`. `world.clock.now_ms() -> i64`.
- State-bearing tools (hold an `Arc<dyn …>`) are constructed at wiring time, NOT via `#[tool]` (see `RememberThisTool`).
- Run tests from repo root: `cargo test -p <crate> <filter>`. Whole workspace: `cargo test`.
- NO Co-Authored-By / AI attribution in commits (user rule).

---

### Task 1: `RecallStore` trait + data types (harness-core)

**Files:**
- Create: `crates/harness-core/src/recall.rs`
- Modify: `crates/harness-core/src/lib.rs` (add `pub mod recall;` + `pub use recall::*;`)

- [ ] **Step 1: Create the module with types + trait**

Create `crates/harness-core/src/recall.rs`:

```rust
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
```

- [ ] **Step 2: Wire the module into lib.rs**

In `crates/harness-core/src/lib.rs`, add `pub mod recall;` after `pub mod profile;` and `pub use recall::*;` after `pub use profile::*;`.

- [ ] **Step 3: Add a serde round-trip test**

Append to `crates/harness-core/src/recall.rs`:

```rust
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
```

- [ ] **Step 4: Build + test**

Run: `cargo test -p harness-rs-core recall`
Expected: `types_round_trip_through_serde` PASS; crate builds (no new deps).

- [ ] **Step 5: Commit**

```bash
git add crates/harness-core/src/recall.rs crates/harness-core/src/lib.rs
git commit -m "feat(harness-core): RecallStore trait + recall data types"
```

---

### Task 2: `FileRecall` default backend (harness-context)

**Files:**
- Create: `crates/harness-context/src/file_recall.rs`
- Modify: `crates/harness-context/src/lib.rs` (add `pub mod file_recall;` + `pub use file_recall::*;`)

- [ ] **Step 1: Write the implementation**

Create `crates/harness-context/src/file_recall.rs`:

```rust
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
use std::path::{Path, PathBuf};
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
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let cleaned = if cleaned.is_empty() { "_".to_string() } else { cleaned };
    if cleaned.len() > 120 {
        cleaned[..120].to_string()
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
        Some((pos, len)) => {
            let start = pos.saturating_sub(40);
            let end = (pos + len + 40).min(content.len());
            // Snap to char boundaries.
            let start = (start..=pos).rev().find(|i| content.is_char_boundary(*i)).unwrap_or(pos);
            let end = (pos + len..=end).find(|i| content.is_char_boundary(*i)).unwrap_or(content.len());
            let mut s = String::new();
            if start > 0 { s.push_str("…"); }
            s.push_str(&content[start..pos]);
            s.push_str(">>>");
            s.push_str(&content[pos..pos + len]);
            s.push_str("<<<");
            s.push_str(&content[pos + len..end]);
            if end < content.len() { s.push_str("…"); }
            s
        }
        None => content.chars().take(80).collect(),
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
        let _g = self.write_lock.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
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
        let _g = self.write_lock.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
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
        // Count lines for the new id + bump meta.
        let id = self.read_messages(owner, session_id).len() as i64;
        let mut meta = self
            .read_meta(owner, session_id)
            .unwrap_or_else(|| SessionMeta::new(session_id, msg.ts_ms));
        meta.message_count = id;
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
            let session_id = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let msgs = self.read_messages(owner, &session_id);
            if msgs.is_empty() {
                continue;
            }
            // Best-matching message in this session.
            let mut best: Option<(u32, &RecallMessage)> = None;
            for m in &msgs {
                let hay = m.content.to_lowercase();
                let score: u32 = q.iter().map(|t| if hay.contains(t.as_str()) { 1 } else { 0 }).sum();
                if score > 0 && best.map(|(s, _)| score > s).unwrap_or(true) {
                    best = Some((score, m));
                }
            }
            let Some((score, anchor)) = best else { continue };
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
            let bookend_end: Vec<RecallMessage> = msgs.iter().rev().take(3).rev().cloned().collect();
            hits.push((
                score,
                started,
                SessionHit {
                    session: meta,
                    snippet: make_snippet(&anchor.content, &q),
                    anchor_id: anchor.id,
                    bookend_start,
                    around,
                    bookend_end,
                },
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
                {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        if let Ok(m) = serde_json::from_str::<SessionMeta>(&s) {
                            metas.push(m);
                        }
                    }
                }
            }
        }
        metas.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));
        metas.truncate(limit);
        Ok(metas)
    }
}
```

- [ ] **Step 2: Wire into lib.rs**

In `crates/harness-context/src/lib.rs`, add `pub mod file_recall;` after `pub mod memory_file;` and `pub use file_recall::*;` after `pub use memory_file::*;`.

- [ ] **Step 3: Write a basic round-trip + malformed-line test**

Append to `crates/harness-context/src/file_recall.rs`:

```rust
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
        r.ensure_session("u1", "s1", &SessionMeta::new("s1", 100)).await.unwrap();
        r.append("u1", "s1", &RecallMessage::new("user", "let us refactor the auth module", 100)).await.unwrap();
        r.append("u1", "s1", &RecallMessage::new("assistant", "sure, starting auth refactor", 101)).await.unwrap();

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
        r.ensure_session("u1", "s1", &SessionMeta::new("s1", 1)).await.unwrap();
        // hand-write a bad line then a good one
        let path = r.session_path("u1", "s1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{bad\n{\"id\":0,\"role\":\"user\",\"content\":\"hello world\",\"ts_ms\":1}\n").unwrap();
        let hits = r.search("u1", "hello", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
```

- [ ] **Step 4: Build + test**

Run: `cargo test -p harness-rs-context file_recall`
Expected: both tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-context/src/file_recall.rs crates/harness-context/src/lib.rs
git commit -m "feat(harness-context): FileRecall — JSONL cross-session recall backend"
```

---

### Task 3: `SessionSearchTool` + `RecallGuide` (harness-loop)

**Files:**
- Create: `crates/harness-loop/src/recall_layer.rs`
- Modify: `crates/harness-loop/src/lib.rs` (add `pub mod recall_layer;` + `pub use recall_layer::*;` near the other `pub mod`/`pub use` at lines 11-21)

- [ ] **Step 1: Write the tool + guide**

Create `crates/harness-loop/src/recall_layer.rs`:

```rust
//! Cross-session recall wiring for [`crate::AgentLoop`].
//!
//! - [`SessionSearchTool`] — LLM-callable search over the recall store, three
//!   shapes (discovery / scroll / browse). Owner is read from
//!   `World.profile.extra["recall_owner"]` so it can only see the caller's own
//!   sessions.
//! - [`RecallGuide`] — optional. At session start, searches the store with the
//!   task description and injects the top snippets. Off unless `.auto_inject()`.

use async_trait::async_trait;
use harness_core::{
    Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, RecallStore, Tool,
    ToolError, ToolResult, ToolRisk, ToolSchema, World,
};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

/// Read the recall owner from the world profile (fallback "default").
pub fn recall_owner(world: &World) -> String {
    world
        .profile
        .extra
        .get("recall_owner")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string()
}

// ───── session_search tool ────────────────────────────────────────────────

pub struct SessionSearchTool {
    store: Arc<dyn RecallStore>,
    schema: ToolSchema,
}

impl SessionSearchTool {
    pub fn new(store: Arc<dyn RecallStore>) -> Self {
        Self {
            store,
            schema: ToolSchema {
                name: "session_search".into(),
                description: "Search your own past sessions, or scroll inside one. \
                    Three shapes: (1) pass `query` to find relevant past sessions \
                    (returns snippet + surrounding messages); (2) pass `session_id` + \
                    `around` to scroll messages near a point in a session; (3) pass \
                    nothing to list your most recent sessions."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search text. Shape 1 (discovery)."},
                        "session_id": {"type": "string", "description": "Scroll within this session. Shape 2."},
                        "around": {"type": "integer", "description": "Anchor message id for scroll. Shape 2."},
                        "window": {"type": "integer", "default": 5, "description": "± messages around the anchor."},
                        "limit": {"type": "integer", "default": 3, "minimum": 1, "maximum": 20}
                    }
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        let owner = recall_owner(world);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(3).min(20) as usize;

        let result = if let Some(q) = args.get("query").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            match self.store.search(&owner, q, limit).await {
                Ok(hits) => json!({"mode": "discover", "query": q, "count": hits.len(), "results": hits}),
                Err(e) => return Ok(err_result(e)),
            }
        } else if let Some(sid) = args.get("session_id").and_then(|v| v.as_str()) {
            let around = args.get("around").and_then(|v| v.as_i64()).unwrap_or(0);
            let window = args.get("window").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            match self.store.scroll(&owner, sid, around, window).await {
                Ok(msgs) => json!({"mode": "scroll", "session_id": sid, "messages": msgs}),
                Err(e) => return Ok(err_result(e)),
            }
        } else {
            match self.store.recent(&owner, limit).await {
                Ok(sessions) => json!({"mode": "browse", "sessions": sessions}),
                Err(e) => return Ok(err_result(e)),
            }
        };
        Ok(ToolResult { ok: true, content: result, trace: None })
    }
}

fn err_result(e: harness_core::RecallError) -> ToolResult {
    ToolResult {
        ok: false,
        content: json!({"error": e.to_string()}),
        trace: None,
    }
}

// ───── RecallGuide (opt-in auto-inject) ───────────────────────────────────

const RECALL_MARKER: &str = "[recall]\n";

pub struct RecallGuide {
    store: Arc<dyn RecallStore>,
    top_k: usize,
}

static RECALL_GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static RECALL_GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

impl RecallGuide {
    pub fn new(store: Arc<dyn RecallStore>) -> Self {
        Self { store, top_k: 3 }
    }
    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }
}

#[async_trait]
impl Guide for RecallGuide {
    fn id(&self) -> &GuideId {
        RECALL_GUIDE_ID.get_or_init(|| "recall".to_string())
    }
    fn kind(&self) -> Execution {
        Execution::Inferential
    }
    fn scope(&self) -> &GuideScope {
        RECALL_GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, world: &World) -> Result<(), GuideError> {
        let owner = recall_owner(world);
        let query = ctx.task.description.clone();
        let hits = self.store.search(&owner, &query, self.top_k).await.unwrap_or_default();
        if hits.is_empty() {
            return Ok(());
        }
        let mut text = String::from(RECALL_MARKER);
        text.push_str("Possibly-relevant context from your past sessions:\n");
        for h in &hits {
            text.push_str(&format!("- ({}) {}\n", h.session.session_id, h.snippet));
        }
        ctx.guides.push(Block::Text(text));
        Ok(())
    }
}
```

- [ ] **Step 2: Wire into lib.rs**

In `crates/harness-loop/src/lib.rs`, in the module block (lines 11-21) add `pub mod recall_layer;` and `pub use recall_layer::*;`.

- [ ] **Step 3: Verify `ctx.guides` is `Vec<Block>` and `Block::Text` exists**

Run: `grep -n "pub guides" crates/harness-core/src/context.rs && grep -n "enum Block" -A 8 crates/harness-core/src/context.rs`
Expected: `ctx.guides: Vec<Block>` and `Block::Text(String)` — matching how `MemoryGuide` pushes (`crates/harness-loop/src/memory_layer.rs`). If the field/variant names differ, adapt the `apply` body to match exactly what `MemoryGuide::apply` does.

- [ ] **Step 4: Build + a tool dispatch test**

Append to `crates/harness-loop/src/recall_layer.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::{default_world, FileRecall};
    use harness_core::{RecallMessage, SessionMeta};

    fn tmp_root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-recall-tool-{}-{nanos}-{n}", std::process::id()))
    }

    #[tokio::test]
    async fn tool_discovery_scoped_to_owner() {
        let root = tmp_root();
        let store: Arc<dyn RecallStore> = Arc::new(FileRecall::open(&root).unwrap());
        store.ensure_session("alice", "s1", &SessionMeta::new("s1", 1)).await.unwrap();
        store.append("alice", "s1", &RecallMessage::new("user", "deploy the payment service", 1)).await.unwrap();

        let tool = SessionSearchTool::new(store.clone());
        // World whose profile owner is "alice"
        let mut world = default_world(".");
        world.profile.extra.insert("recall_owner".into(), serde_json::json!("alice"));
        let out = tool.invoke(serde_json::json!({"query": "payment deploy"}), &mut world).await.unwrap();
        assert!(out.ok);
        assert_eq!(out.content["count"], 1);

        // A different owner sees nothing.
        let mut bob = default_world(".");
        bob.profile.extra.insert("recall_owner".into(), serde_json::json!("bob"));
        let out2 = tool.invoke(serde_json::json!({"query": "payment deploy"}), &mut bob).await.unwrap();
        assert_eq!(out2.content["count"], 0);

        let _ = std::fs::remove_dir_all(&root);
    }
}
```

This requires harness-context as a dev-dependency of harness-loop. Add to `crates/harness-loop/Cargo.toml` under `[dev-dependencies]`: `harness-context = { workspace = true }` (check the exact alias used elsewhere — it is `harness-context`).

- [ ] **Step 5: Run + commit**

Run: `cargo test -p harness-rs-loop recall_layer`
Expected: `tool_discovery_scoped_to_owner` PASS.

```bash
git add crates/harness-loop/src/recall_layer.rs crates/harness-loop/src/lib.rs crates/harness-loop/Cargo.toml
git commit -m "feat(harness-loop): SessionSearchTool (3 shapes, owner-scoped) + opt-in RecallGuide"
```

---

### Task 4: `.with_recall()` builder + capture points (harness-loop)

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` (AgentLoop struct fields, builder methods, capture in `run_built_context`)

- [ ] **Step 1: Add fields + builder methods**

In `crates/harness-loop/src/lib.rs`, add two fields to `struct AgentLoop<M>` (after `pub streaming: bool,`):

```rust
    /// Optional cross-session recall store. When set, the loop captures every
    /// turn and the `session_search` tool is registered. See `with_recall`.
    pub recall: Option<Arc<dyn harness_core::RecallStore>>,
    /// When true (and `recall` is set), a `RecallGuide` auto-injects top-k
    /// past context at session start.
    pub recall_auto_inject: bool,
```

In `AgentLoop::new`, initialise them (after `streaming: false,`):
```rust
            recall: None,
            recall_auto_inject: false,
```

Add builder methods (after `with_macro_hooks`):
```rust
    /// Enable cross-session recall: capture every turn into `store` and
    /// register the `session_search` tool. Owner + session id are read from
    /// `world.profile.extra["recall_owner"|"recall_session"]` at run time.
    pub fn with_recall(mut self, store: Arc<dyn harness_core::RecallStore>) -> Self {
        self.tools.insert(Arc::new(crate::SessionSearchTool::new(store.clone())));
        self.recall = Some(store);
        self
    }

    /// After `with_recall`, also auto-inject top-k relevant past context at
    /// session start (off by default — tool-only is prompt-cache friendly).
    pub fn auto_inject(mut self) -> Self {
        self.recall_auto_inject = true;
        self
    }
```

- [ ] **Step 2: Add the session-id counter + a capture helper**

Near the bottom of `crates/harness-loop/src/lib.rs` (beside `PATCH_SEQ`), add:

```rust
/// Monotonic counter for fallback recall session ids (no `uuid` dep).
static RECALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
```

Inside `impl<M: Model> AgentLoop<M>`, add a best-effort capture helper:

```rust
    /// Best-effort append to the recall store. Never fails the turn.
    async fn recall_append(&self, owner: &str, session: &str, msg: harness_core::RecallMessage) {
        if let Some(store) = &self.recall {
            if let Err(e) = store.append(owner, session, &msg).await {
                tracing::warn!(error = %e, "recall append failed");
            }
        }
    }
```

- [ ] **Step 3: Wire capture into `run_built_context`**

In `run_built_context`, immediately after the `SessionStart` hook fires (after line 303, before the guides loop), derive owner/session and ensure the session + inject the optional guide:

```rust
        // ── recall: resolve owner/session, ensure the session row ──
        let (recall_owner, recall_session) = if self.recall.is_some() {
            use std::sync::atomic::Ordering;
            let owner = world
                .profile
                .extra
                .get("recall_owner")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            let session = world
                .profile
                .extra
                .get("recall_session")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    format!("sess-{}-{}", world.clock.now_ms(), RECALL_SEQ.fetch_add(1, Ordering::SeqCst))
                });
            if let Some(store) = &self.recall {
                let meta = harness_core::SessionMeta::new(&session, world.clock.now_ms());
                if let Err(e) = store.ensure_session(&owner, &session, &meta).await {
                    tracing::warn!(error = %e, "recall ensure_session failed");
                }
            }
            (owner, session)
        } else {
            (String::new(), String::new())
        };
```

Add the auto-inject guide alongside the existing guide loop. Replace the guides loop header so a transient `RecallGuide` participates when enabled. The simplest non-invasive way: build a local list of guide refs:

```rust
        // Existing guides + optional transient RecallGuide.
        let recall_guide: Option<Arc<dyn Guide>> = if self.recall_auto_inject {
            self.recall.clone().map(|s| Arc::new(crate::RecallGuide::new(s)) as Arc<dyn Guide>)
        } else {
            None
        };
        let all_guides: Vec<&Arc<dyn Guide>> =
            self.guides.iter().chain(recall_guide.iter()).collect();
        for g in &all_guides {
            if g.scope().matches(&ctx.task) {
                self.hooks.fire(&Event::PreGuide { guide: g.id() }, world);
                g.apply(&mut ctx, world).await?;
                self.hooks.fire(&Event::PostGuide { guide: g.id() }, world);
            }
        }
```

(Delete the original `for g in &self.guides { … g.apply … }` block — the new block replaces it. Leave the later `apply_before_iter` loop over `self.guides` unchanged; the RecallGuide only injects on `apply`.)

After the user-task push (`ctx.history.push(Turn { role: User … })`, ~line 313-316), capture the user message:

```rust
        if self.recall.is_some() {
            self.recall_append(
                &recall_owner,
                &recall_session,
                harness_core::RecallMessage::new("user", ctx.task.description.clone(), world.clock.now_ms()),
            )
            .await;
        }
```

After `ctx.push_model_output(&out);` (~line 361), capture the assistant message:

```rust
            if self.recall.is_some() {
                let calls = if out.tool_calls.is_empty() {
                    None
                } else {
                    serde_json::to_string(&out.tool_calls).ok()
                };
                let mut m = harness_core::RecallMessage::new(
                    "assistant",
                    out.text.clone().unwrap_or_default(),
                    world.clock.now_ms(),
                );
                m.tool_calls = calls;
                self.recall_append(&recall_owner, &recall_session, m).await;
            }
```

After each tool result is pushed to history (after the `ctx.history.push(Turn { role: Tool, … ToolResult … })` at ~line 416-422), capture the tool message:

```rust
                if self.recall.is_some() {
                    let body = serde_json::to_string(&result.content).unwrap_or_default();
                    self.recall_append(
                        &recall_owner,
                        &recall_session,
                        harness_core::RecallMessage::new("tool", body, world.clock.now_ms())
                            .with_tool_name(action.tool.clone()),
                    )
                    .await;
                }
```

Ensure `Guide` and `RecallMessage`/`SessionMeta` are in scope (they are via the existing `harness_core::{…}` import at line 24-28; add `Guide` is already imported; add `RecallStore` is referenced via full path so no import needed; `Block` already imported).

- [ ] **Step 4: Write a capture test with a mock model**

Create `crates/harness-loop/tests/recall_capture.rs`:

```rust
//! End-to-end: with_recall captures user/assistant/tool messages under the
//! owner+session from world.profile.extra.

use async_trait::async_trait;
use harness_context::{default_world, FileRecall};
use harness_core::{
    Context, Model, ModelError, ModelOutput, RecallStore, StopReason, ToolCall, Usage,
};
use harness_loop::AgentLoop;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Mock: turn 0 emits one tool call; turn 1 emits final text.
struct MockModel {
    turn: AtomicU32,
}
#[async_trait]
impl Model for MockModel {
    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t == 0 {
            Ok(ModelOutput {
                text: Some("calling tool".into()),
                tool_calls: vec![ToolCall { id: "c1".into(), name: "noop".into(), args: serde_json::json!({}) }],
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                reasoning: None,
            })
        } else {
            Ok(ModelOutput {
                text: Some("done".into()),
                tool_calls: vec![],
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
                reasoning: None,
            })
        }
    }
}

fn tmp_root() -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("harness-recall-cap-{}-{nanos}-{n}", std::process::id()))
}

#[tokio::test]
async fn with_recall_captures_the_conversation() {
    let root = tmp_root();
    let store: Arc<dyn RecallStore> = Arc::new(FileRecall::open(&root).unwrap());
    let loop_ = AgentLoop::new(MockModel { turn: AtomicU32::new(0) }).with_recall(store.clone());

    let mut world = default_world(".");
    world.profile.extra.insert("recall_owner".into(), serde_json::json!("u9"));
    world.profile.extra.insert("recall_session".into(), serde_json::json!("conv1"));

    let task = harness_core::Task {
        description: "remember the alpha protocol".into(),
        source: None,
        deadline: None,
    };
    let _ = loop_.run(task, &mut world).await.unwrap();

    // The session now holds: user task, assistant turn(s), tool result.
    let hits = store.search("u9", "alpha protocol", 5).await.unwrap();
    assert_eq!(hits.len(), 1, "user message should be searchable");
    let scrolled = store.scroll("u9", "conv1", 1, 50).await.unwrap();
    let roles: Vec<&str> = scrolled.iter().map(|m| m.role.as_str()).collect();
    assert!(roles.contains(&"user"));
    assert!(roles.contains(&"assistant"));
    assert!(roles.contains(&"tool"));

    let _ = std::fs::remove_dir_all(&root);
}
```

Add a `noop` tool for the mock to call: since the mock calls a tool named `noop` that isn't registered, the loop's `dispatch` returns an error result — that's fine, the tool message is still captured. (Confirm by reading the loop: an unknown tool yields a `ToolResult{ok:false,…}` which is still pushed + captured. If `dispatch` instead errors out the run, register a tiny no-op tool in the test via `.with_tool`.) Verify behavior; if needed, add a trivial `#[tool]`-free no-op tool struct in the test file and `.with_tool(Arc::new(NoopTool))`.

`crates/harness-loop/Cargo.toml` `[dev-dependencies]` needs `harness-context`, `harness-core`, `tokio`, `async-trait`, `serde_json` (most already present; add what's missing).

- [ ] **Step 5: Run + commit**

Run: `cargo test -p harness-rs-loop --test recall_capture` and `cargo test -p harness-rs-loop`
Expected: capture test PASS; existing loop tests still PASS.

```bash
git add crates/harness-loop/src/lib.rs crates/harness-loop/tests/recall_capture.rs crates/harness-loop/Cargo.toml
git commit -m "feat(harness-loop): AgentLoop .with_recall/.auto_inject + best-effort turn capture"
```

---

### Task 5: `harness-recall-sqlite` optional crate (FTS5/trigram/LIKE)

**Files:**
- Create: `crates/harness-recall-sqlite/Cargo.toml`
- Create: `crates/harness-recall-sqlite/src/lib.rs`
- Modify: root `Cargo.toml` (add `"crates/harness-recall-sqlite"` to `[workspace] members`)

- [ ] **Step 1: Create the crate manifest**

Create `crates/harness-recall-sqlite/Cargo.toml`:

```toml
[package]
name = "harness-rs-recall-sqlite"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
description = "SQLite (FTS5 + trigram) backend for harness-rs cross-session recall. The only recall backend that pulls rusqlite; opt in when you want FTS-grade search."

[lib]

[dependencies]
harness-core = { workspace = true }
async-trait  = { workspace = true }
serde_json   = { workspace = true }
tracing      = { workspace = true }
rusqlite     = { version = "0.32", features = ["bundled"] }

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

Add `"crates/harness-recall-sqlite",` to the `members` list in the root `Cargo.toml` (next to `"crates/harness-daemon",`).

- [ ] **Step 2: Implement `SqliteRecall`**

Create `crates/harness-recall-sqlite/src/lib.rs`:

```rust
//! SQLite (FTS5) backend for [`harness_core::RecallStore`]. Faithful port of
//! Hermes Agent's recall storage: FTS5 BM25 + `snippet()`, a trigram FTS table
//! for CJK, and a LIKE fallback for short CJK queries. Owner scoping is a SQL
//! `WHERE owner = ?`, so cross-tenant leakage is structurally impossible.
//!
//! `rusqlite` runs synchronously behind an `Arc<Mutex<Connection>>`; the async
//! trait methods lock and run the SQL inline (recall writes are small + fast).

use async_trait::async_trait;
use harness_core::{RecallError, RecallMessage, RecallStore, SessionHit, SessionMeta};
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub struct SqliteRecall {
    conn: Mutex<Connection>,
}

impl SqliteRecall {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, RecallError> {
        let conn = Connection::open(path).map_err(|e| RecallError::Backend(e.to_string()))?;
        Self::init(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self, RecallError> {
        let conn = Connection::open_in_memory().map_err(|e| RecallError::Backend(e.to_string()))?;
        Self::init(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn init(conn: &Connection) -> Result<(), RecallError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS recall_sessions (
                owner         TEXT NOT NULL,
                session_id    TEXT NOT NULL,
                title         TEXT,
                source        TEXT,
                started_at    INTEGER NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (owner, session_id)
            );
            CREATE INDEX IF NOT EXISTS idx_recall_sessions_owner
                ON recall_sessions(owner, started_at DESC);

            CREATE TABLE IF NOT EXISTS recall_messages (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                owner       TEXT NOT NULL,
                session_id  TEXT NOT NULL,
                role        TEXT NOT NULL,
                content     TEXT,
                tool_name   TEXT,
                tool_calls  TEXT,
                ts_ms       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_recall_messages_session
                ON recall_messages(owner, session_id, id);

            CREATE VIRTUAL TABLE IF NOT EXISTS recall_messages_fts USING fts5(content);
            CREATE VIRTUAL TABLE IF NOT EXISTS recall_messages_fts_trigram
                USING fts5(content, tokenize='trigram');

            CREATE TRIGGER IF NOT EXISTS recall_fts_insert AFTER INSERT ON recall_messages BEGIN
                INSERT INTO recall_messages_fts(rowid, content)
                    VALUES (new.id, COALESCE(new.content,'') || ' ' || COALESCE(new.tool_name,'') || ' ' || COALESCE(new.tool_calls,''));
                INSERT INTO recall_messages_fts_trigram(rowid, content)
                    VALUES (new.id, COALESCE(new.content,'') || ' ' || COALESCE(new.tool_name,'') || ' ' || COALESCE(new.tool_calls,''));
            END;
            CREATE TRIGGER IF NOT EXISTS recall_fts_delete AFTER DELETE ON recall_messages BEGIN
                DELETE FROM recall_messages_fts WHERE rowid = old.id;
                DELETE FROM recall_messages_fts_trigram WHERE rowid = old.id;
            END;
            "#,
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        Ok(())
    }
}

fn row_to_msg(r: &rusqlite::Row<'_>) -> rusqlite::Result<RecallMessage> {
    Ok(RecallMessage {
        id: r.get("id")?,
        role: r.get("role")?,
        content: r.get::<_, Option<String>>("content")?.unwrap_or_default(),
        tool_name: r.get("tool_name")?,
        tool_calls: r.get("tool_calls")?,
        ts_ms: r.get("ts_ms")?,
    })
}

fn count_cjk(s: &str) -> usize {
    s.chars().filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c)).count()
}

impl SqliteRecall {
    fn read_window(
        conn: &Connection,
        owner: &str,
        session_id: &str,
        lo: i64,
        hi: i64,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, tool_name, tool_calls, ts_ms FROM recall_messages
                 WHERE owner=?1 AND session_id=?2 AND id BETWEEN ?3 AND ?4 ORDER BY id",
            )
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![owner, session_id, lo, hi], row_to_msg)
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))
    }

    fn meta_of(conn: &Connection, owner: &str, session_id: &str) -> Option<SessionMeta> {
        conn.query_row(
            "SELECT session_id, title, source, started_at, message_count FROM recall_sessions WHERE owner=?1 AND session_id=?2",
            params![owner, session_id],
            |r| {
                Ok(SessionMeta {
                    session_id: r.get(0)?,
                    title: r.get(1)?,
                    source: r.get(2)?,
                    started_at_ms: r.get(3)?,
                    message_count: r.get(4)?,
                })
            },
        )
        .ok()
    }
}

#[async_trait]
impl RecallStore for SqliteRecall {
    async fn ensure_session(&self, owner: &str, session_id: &str, meta: &SessionMeta) -> Result<(), RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        conn.execute(
            "INSERT OR IGNORE INTO recall_sessions(owner, session_id, title, source, started_at, message_count)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![owner, session_id, meta.title, meta.source, meta.started_at_ms],
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn append(&self, owner: &str, session_id: &str, msg: &RecallMessage) -> Result<i64, RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        conn.execute(
            "INSERT INTO recall_messages(owner, session_id, role, content, tool_name, tool_calls, ts_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![owner, session_id, msg.role, msg.content, msg.tool_name, msg.tool_calls, msg.ts_ms],
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE recall_sessions SET message_count = message_count + 1 WHERE owner=?1 AND session_id=?2",
            params![owner, session_id],
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        Ok(id)
    }

    async fn search(&self, owner: &str, query: &str, limit: usize) -> Result<Vec<SessionHit>, RecallError> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        let cjk = count_cjk(query);

        // Choose the matcher: trigram for CJK≥3, FTS5 for ASCII, LIKE for short.
        // Each yields rows of (msg_id, session_id, content, ts_ms) ranked best-first.
        let anchors: Vec<(i64, String)> = if cjk >= 3 {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.session_id FROM recall_messages_fts_trigram f
                 JOIN recall_messages m ON m.id = f.rowid
                 WHERE f.content MATCH ?1 AND m.owner = ?2
                 ORDER BY rank LIMIT ?3",
            ).map_err(|e| RecallError::Backend(e.to_string()))?;
            stmt.query_map(params![query, owner, (limit * 5) as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| RecallError::Backend(e.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))?
        } else {
            // Try FTS5; fall back to LIKE if it errors (e.g. odd punctuation) or returns nothing.
            let fts: rusqlite::Result<Vec<(i64, String)>> = (|| {
                let mut stmt = conn.prepare(
                    "SELECT m.id, m.session_id FROM recall_messages_fts f
                     JOIN recall_messages m ON m.id = f.rowid
                     WHERE f.content MATCH ?1 AND m.owner = ?2
                     ORDER BY rank LIMIT ?3",
                )?;
                let v = stmt.query_map(params![query, owner, (limit * 5) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(v)
            })();
            let v = fts.unwrap_or_default();
            if v.is_empty() {
                let like = format!("%{}%", query.trim());
                let mut stmt = conn.prepare(
                    "SELECT id, session_id FROM recall_messages
                     WHERE owner=?1 AND content LIKE ?2 ORDER BY id DESC LIMIT ?3",
                ).map_err(|e| RecallError::Backend(e.to_string()))?;
                stmt.query_map(params![owner, like, (limit * 5) as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| RecallError::Backend(e.to_string()))?
                    .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))?
            } else {
                v
            }
        };

        // Dedup to top sessions, build hits.
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for (anchor_id, session_id) in anchors {
            if !seen.insert(session_id.clone()) {
                continue;
            }
            let Some(meta) = Self::meta_of(&conn, owner, &session_id) else { continue };
            let around = Self::read_window(&conn, owner, &session_id, anchor_id - 5, anchor_id + 5)?;
            let max_id = meta.message_count;
            let bookend_start = Self::read_window(&conn, owner, &session_id, 1, 3)?;
            let bookend_end = Self::read_window(&conn, owner, &session_id, (max_id - 2).max(1), max_id)?;
            // snippet via FTS5 snippet() on the anchor row's content
            let snippet: String = conn
                .query_row(
                    "SELECT snippet(recall_messages_fts, 0, '>>>', '<<<', '…', 12)
                     FROM recall_messages_fts WHERE rowid = ?1",
                    params![anchor_id],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| {
                    around.iter().find(|m| m.id == anchor_id).map(|m| m.content.chars().take(80).collect()).unwrap_or_default()
                });
            hits.push(SessionHit { session: meta, snippet, anchor_id, bookend_start, around, bookend_end });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    async fn scroll(&self, owner: &str, session_id: &str, around: i64, window: usize) -> Result<Vec<RecallMessage>, RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        let w = window as i64;
        Self::read_window(&conn, owner, session_id, around - w, around + w)
    }

    async fn recent(&self, owner: &str, limit: usize) -> Result<Vec<SessionMeta>, RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, title, source, started_at, message_count FROM recall_sessions
                 WHERE owner=?1 ORDER BY started_at DESC LIMIT ?2",
            )
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![owner, limit as i64], |r| {
                Ok(SessionMeta {
                    session_id: r.get(0)?,
                    title: r.get(1)?,
                    source: r.get(2)?,
                    started_at_ms: r.get(3)?,
                    message_count: r.get(4)?,
                })
            })
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))
    }
}
```

- [ ] **Step 3: Add a CJK + smoke test**

Append to `crates/harness-recall-sqlite/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fts_english_and_cjk_search() {
        let r = SqliteRecall::open_in_memory().unwrap();
        r.ensure_session("u1", "s1", &SessionMeta::new("s1", 1)).await.unwrap();
        r.append("u1", "s1", &RecallMessage::new("user", "deploy the payment service tomorrow", 1)).await.unwrap();
        r.append("u1", "s1", &RecallMessage::new("user", "我们明天要上线支付服务", 2)).await.unwrap();

        let en = r.search("u1", "payment deploy", 5).await.unwrap();
        assert_eq!(en.len(), 1);
        assert!(en[0].snippet.contains(">>>"));

        let zh = r.search("u1", "支付服务", 5).await.unwrap();
        assert_eq!(zh.len(), 1, "trigram CJK search should hit");
    }
}
```

- [ ] **Step 4: Build + test**

Run: `cargo test -p harness-rs-recall-sqlite`
Expected: `fts_english_and_cjk_search` PASS. Confirm bundled rusqlite ships FTS5 (it does by default).

- [ ] **Step 5: Commit**

```bash
git add crates/harness-recall-sqlite Cargo.toml
git commit -m "feat(harness-recall-sqlite): SQLite FTS5/trigram recall backend (optional crate)"
```

---

### Task 6: Shared contract test suite (both backends) + privacy test

**Files:**
- Create: `crates/harness-core/src/recall_testkit.rs`
- Modify: `crates/harness-core/src/lib.rs` (add `pub mod recall_testkit;` + `pub use recall_testkit::*;`)
- Create: `crates/harness-context/tests/recall_contract.rs`
- Create: `crates/harness-recall-sqlite/tests/recall_contract.rs`

- [ ] **Step 1: Write the reusable contract**

Create `crates/harness-core/src/recall_testkit.rs`:

```rust
//! Reusable conformance suite for [`crate::RecallStore`] backends. Each backend
//! crate calls `recall_contract(store)` from a `#[tokio::test]` so all impls are
//! held to identical behaviour — including the privacy-critical owner isolation.

use crate::{RecallMessage, RecallStore, SessionMeta};
use std::sync::Arc;

/// Run the full contract against a fresh, empty `store`.
pub async fn recall_contract(store: Arc<dyn RecallStore>) {
    // ── append + search round-trip ──
    store.ensure_session("alice", "s1", &SessionMeta::new("s1", 100)).await.unwrap();
    store.append("alice", "s1", &RecallMessage::new("user", "refactor the auth module today", 100)).await.unwrap();
    store.append("alice", "s1", &RecallMessage::new("assistant", "starting the auth refactor now", 101)).await.unwrap();
    store.append("alice", "s1", &RecallMessage::new("tool", "edited auth.rs", 102).with_tool_name("edit")).await.unwrap();

    let hits = store.search("alice", "auth refactor", 5).await.unwrap();
    assert_eq!(hits.len(), 1, "search should find the session");
    assert_eq!(hits[0].session.session_id, "s1");
    assert!(!hits[0].bookend_start.is_empty(), "hit carries bookends");

    // ── scroll window ──
    let scrolled = store.scroll("alice", "s1", 2, 1).await.unwrap();
    assert!(scrolled.iter().all(|m| (m.id - 2).abs() <= 1));
    assert!(scrolled.iter().any(|m| m.id == 2));

    // ── recent ordering ──
    store.ensure_session("alice", "s2", &SessionMeta::new("s2", 200)).await.unwrap();
    store.append("alice", "s2", &RecallMessage::new("user", "a newer session", 200)).await.unwrap();
    let recent = store.recent("alice", 10).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].session_id, "s2", "newest first");

    // ── OWNER ISOLATION (privacy-critical) ──
    let bob_search = store.search("bob", "auth refactor", 5).await.unwrap();
    assert!(bob_search.is_empty(), "bob must not see alice's sessions");
    let bob_recent = store.recent("bob", 10).await.unwrap();
    assert!(bob_recent.is_empty(), "bob has no sessions");
    let bob_scroll = store.scroll("bob", "s1", 1, 5).await.unwrap();
    assert!(bob_scroll.is_empty(), "bob cannot scroll alice's session");

    // ── empty query is not an error ──
    let empty = store.search("alice", "", 5).await.unwrap();
    assert!(empty.is_empty());
}
```

Wire into `crates/harness-core/src/lib.rs`: `pub mod recall_testkit;` + `pub use recall_testkit::*;`.

> Note: this ships in the normal (non-test) build so downstream backend crates
> can call it from their `tests/`. It depends only on the trait + types already
> in this crate — no test-only deps leak into harness-core.

- [ ] **Step 2: FileRecall contract test**

Create `crates/harness-context/tests/recall_contract.rs`:

```rust
use harness_context::FileRecall;
use harness_core::{recall_contract, RecallStore};
use std::sync::Arc;

#[tokio::test]
async fn file_recall_satisfies_contract() {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("harness-recall-contract-file-{}-{nanos}", std::process::id()));
    let store: Arc<dyn RecallStore> = Arc::new(FileRecall::open(&root).unwrap());
    recall_contract(store).await;
    let _ = std::fs::remove_dir_all(&root);
}
```

`crates/harness-context/Cargo.toml` `[dev-dependencies]`: ensure `tokio` (features macros, rt-multi-thread) is present.

- [ ] **Step 3: SqliteRecall contract test**

Create `crates/harness-recall-sqlite/tests/recall_contract.rs`:

```rust
use harness_core::{recall_contract, RecallStore};
use harness_recall_sqlite::SqliteRecall;
use std::sync::Arc;

#[tokio::test]
async fn sqlite_recall_satisfies_contract() {
    let store: Arc<dyn RecallStore> = Arc::new(SqliteRecall::open_in_memory().unwrap());
    recall_contract(store).await;
}
```

(The crate's lib name is `harness_recall_sqlite` — confirm with `cargo test -p harness-rs-recall-sqlite` import resolution; the `[package] name` is `harness-rs-recall-sqlite` but the crate's Rust path is the lib name, which defaults to the package name with hyphens→underscores: `harness_rs_recall_sqlite`. Use whichever the compiler reports; adjust the `use` accordingly. To make it predictable, set `[lib] name = "harness_recall_sqlite"` in the crate manifest.)

> Action: add `name = "harness_recall_sqlite"` under `[lib]` in
> `crates/harness-recall-sqlite/Cargo.toml` so the import path is stable.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test -p harness-rs-context recall_contract` and `cargo test -p harness-rs-recall-sqlite recall_contract`
Expected: both `*_satisfies_contract` PASS (identical behaviour, incl. owner isolation).

- [ ] **Step 5: Full workspace build + test**

Run: `cargo build` then `cargo test`
Expected: clean build; all recall tests green; nothing else regressed.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-core/src/recall_testkit.rs crates/harness-core/src/lib.rs \
        crates/harness-context/tests/recall_contract.rs \
        crates/harness-recall-sqlite/tests/recall_contract.rs \
        crates/harness-context/Cargo.toml crates/harness-recall-sqlite/Cargo.toml
git commit -m "test(harness-recall): shared RecallStore contract suite incl. owner-isolation, both backends"
```

---

## Final verification (after all tasks)

- [ ] `cargo build` — clean (framework core still has zero SQLite; rusqlite appears only under `harness-recall-sqlite`).
- [ ] `cargo test` — all green.
- [ ] `cargo tree -p harness-rs-core | grep -i rusqlite` → no output (core stays SQLite-free).
- [ ] `cargo tree -p harness-rs-loop | grep -i rusqlite` → no output (loop stays SQLite-free).
- [ ] Dispatch a final code-reviewer over the whole branch.

## Notes for the implementer

- **Framework SQLite-free invariant:** only `crates/harness-recall-sqlite` may depend on rusqlite. If any step tempts you to add rusqlite to core/context/loop, stop — that violates the design.
- **Capture is best-effort:** every recall write in the loop must swallow errors with `tracing::warn!`. A recall failure must never fail the agent turn.
- **Owner resolution is identical** in capture (`run_built_context`) and in the tool (`recall_owner`) — both read `world.profile.extra["recall_owner"]`, fallback `"default"`. Keep them in sync.
- **`ctx.guides` / `Block::Text`:** mirror exactly what `crates/harness-loop/src/memory_layer.rs` (`MemoryGuide::apply`) does; if the field/variant differs from this plan, follow the real `MemoryGuide`.
- **Crate name vs lib path:** set `[lib] name = "harness_recall_sqlite"` so the contract test's `use harness_recall_sqlite::SqliteRecall;` resolves.
- **Commits:** no Co-Authored-By / AI attribution.
