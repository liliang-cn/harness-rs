# harness-recall — Cross-Session Recall (Framework Capability)

**Status:** Approved (brainstorming) → ready for plan
**Date:** 2026-05-30
**Layer:** harness-rs framework (NOT the dashboard product)

## Goal

Give any harness-rs application cross-session conversation recall — the agent
can search its own past sessions ("what did the user say three weeks ago") —
through a single builder call:

```rust
AgentLoop::new(model)
    .with_recall(store)   // capture every turn + auto-register the session_search tool
    // .auto_inject()     // optional: also surface top-k relevant past context at session start
```

`store` is any `Arc<dyn RecallStore>`. The zero-dependency default is
`FileRecall` (JSONL); apps that want FTS5-grade search opt into the
`harness-recall-sqlite` crate and pass `SqliteRecall` instead — same trait,
same call site.

## Design principles / constraints

- **The framework stays SQLite-free.** Confirmed: no framework crate depends on
  rusqlite/sqlite/sqlx today; persistence is file-based + open-format
  (`FileMemory` = append-only JSONL, `InMemoryKv`). Recall must not change that.
  rusqlite enters the build **only** if an app opts into `harness-recall-sqlite`.
- **Mirror the existing `Memory` layering** (`Memory` trait in harness-core →
  `FileMemory` impl in harness-context → tools in a tools crate).
- **Copy Hermes's recall design** for the SQLite impl: pure FTS5 (no vectors),
  trigram fallback for CJK, LIKE fallback for short CJK queries, BM25 ranking,
  `snippet()` highlighting, one `session_search` tool with three shapes
  (discovery / scroll / browse), tool-only by default (no auto-inject).
- **Capture is a first-class AgentLoop concern** (async), not a sync hook.
- **Best-effort:** a store error during capture logs a warning and never fails
  the agent turn.
- **Safe multi-tenancy by default:** every session is tagged with an `owner`
  read from `World.profile`; search is always scoped to that owner.

## Decisions locked in brainstorming

| Question | Decision |
|---|---|
| How is history captured? | AgentLoop first-class `.with_recall(store)` (async, per-turn persist) |
| Storage genericity | `RecallStore` trait + pluggable impls |
| Default impl | `FileRecall` (JSONL + lightweight scan), zero new deps |
| SQLite/FTS5 impl | optional crate `harness-recall-sqlite` |
| Retrieval model | `session_search` tool always; optional `RecallGuide` auto-inject, default off |
| Scoping | `owner` key from `World.profile.extra["recall_owner"]` (fallback `"default"`) |

## Architecture / crate layout

| Component | Crate | New deps |
|---|---|---|
| `RecallStore` trait + data types | **harness-core** | none |
| `FileRecall` default impl | **harness-context** (beside `FileMemory`) | none |
| `SessionSearchTool` + `RecallGuide` | **harness-loop** (so `.with_recall` wires capture + tool in one call) | none |
| `.with_recall()` builder + capture points | **harness-loop** | none (fallback session id from `clock.now_ms()` + atomic counter, no `uuid` dep) |
| `SqliteRecall` impl | **NEW crate `crates/harness-recall-sqlite`** | rusqlite (bundled, FTS5) |

Rationale for `SessionSearchTool`/`RecallGuide` living in harness-loop rather
than a separate `harness-tools-recall` crate: it lets `.with_recall(store)`
construct and register the tool itself (one-call DX) without harness-loop taking
a dependency on a tools crate. Both are storage-agnostic (operate through
`Arc<dyn RecallStore>`), so this does not couple the loop to any backend.

## `RecallStore` trait (harness-core)

```rust
#[async_trait]
pub trait RecallStore: Send + Sync + 'static {
    /// Create/refresh the session row (idempotent). Records started_at, source.
    async fn ensure_session(&self, owner: &str, session_id: &str, meta: &SessionMeta)
        -> Result<(), RecallError>;

    /// Append one message; returns the assigned message id (monotonic within session).
    async fn append(&self, owner: &str, session_id: &str, msg: &RecallMessage)
        -> Result<i64, RecallError>;

    /// Discovery: search the owner's messages, return top sessions with snippet + bookends.
    async fn search(&self, owner: &str, query: &str, limit: usize)
        -> Result<Vec<SessionHit>, RecallError>;

    /// Scroll: messages around an anchor id within one session (±window).
    async fn scroll(&self, owner: &str, session_id: &str, around: i64, window: usize)
        -> Result<Vec<RecallMessage>, RecallError>;

    /// Browse: the owner's most recent sessions.
    async fn recent(&self, owner: &str, limit: usize)
        -> Result<Vec<SessionMeta>, RecallError>;
}
```

### Data types (harness-core)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallMessage {
    pub id: i64,                       // 0 on input; assigned by the store
    pub role: String,                  // "user" | "assistant" | "tool" | "system"
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<String>,    // JSON-encoded tool-call array
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,        // app-defined: "cli" | "web" | ...
    pub started_at_ms: i64,
    pub message_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHit {
    pub session: SessionMeta,
    pub snippet: String,               // match-marked excerpt
    pub anchor_id: i64,                // matched message id
    pub bookend_start: Vec<RecallMessage>, // first ~3 messages of the session
    pub around: Vec<RecallMessage>,        // ±window around the anchor
    pub bookend_end: Vec<RecallMessage>,   // last ~3 messages of the session
}

#[derive(Debug, thiserror::Error)]
pub enum RecallError {
    #[error("recall io: {0}")] Io(String),
    #[error("recall backend: {0}")] Backend(String),
    #[error("not found: {0}")] NotFound(String),
}
```

The trait + types add no dependencies to harness-core beyond what it already
has (`async_trait`, `serde`, `thiserror`).

## `FileRecall` default impl (harness-context)

Append-only JSONL, mirroring `FileMemory`'s posture ("open-format, plain text;
linear scan is fine at kilobyte–MB scale").

**Layout** (under a root dir passed to `FileRecall::open(root)`):
```
<root>/<owner>/<session_id>.jsonl       # one RecallMessage per line (id = 1-based line number)
<root>/<owner>/<session_id>.meta.json   # SessionMeta sidecar (title/source/started/count)
```
`owner` and `session_id` are sanitized for filesystem safety (replace path
separators / control chars; cap length; hash if needed).

- `ensure_session`: create the `.meta.json` if absent (started_at, source).
- `append`: append a JSON line; id = new line number; bump `message_count` in
  the meta sidecar; returns the id.
- `search`: iterate the owner's session files, scan messages, score by
  lowercase token-overlap between query tokens and message content (count of
  distinct query tokens present, tie-broken by recency), take the top `limit`
  *sessions* (dedup by session), build `SessionHit` with a self-cut snippet
  (±~40 chars around the first matched token, match-marked) + bookends +
  ±window around the anchor.
- `scroll`: read the session file, return lines `[around-window, around+window]`.
- `recent`: list the owner's `.meta.json` files, sort by `started_at_ms` desc,
  take `limit`.

No FTS, no stemming — naive but correct and zero-dep. Apps at scale switch to
`SqliteRecall`.

## `SqliteRecall` impl (optional crate `harness-recall-sqlite`)

Faithful port of Hermes's recall storage. `SqliteRecall::open(path)` wraps an
`Arc<Mutex<rusqlite::Connection>>`; each async trait method locks and runs the
synchronous SQL (writes are fast; acceptable for the recall workload).

**Schema** (FTS5 from bundled rusqlite):
```sql
CREATE TABLE IF NOT EXISTS recall_sessions (
    owner        TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    title        TEXT,
    source       TEXT,
    started_at   INTEGER NOT NULL,
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
-- INSERT/UPDATE/DELETE triggers keep both FTS tables in sync with
-- (content || ' ' || tool_name || ' ' || tool_calls), exactly as Hermes does.
```

- `search`: BM25 over `recall_messages_fts` joined to messages + sessions,
  `WHERE recall_sessions.owner = ?`, `snippet(recall_messages_fts,0,'>>>','<<<','...',40)`,
  `ORDER BY rank`, dedup to top `limit` sessions, assemble bookends/around with
  follow-up `scroll`-style reads.
  - **CJK fallback:** if the query has ≥3 CJK chars, query `recall_messages_fts_trigram`.
  - **Short-CJK fallback:** if <3 chars / no FTS hits, `LIKE '%q%'` substring scan.
- `scroll`/`recent`: plain indexed SELECTs scoped by owner.

The owner scoping lives in the SQL `WHERE owner = ?`, so cross-tenant leakage is
structurally impossible.

## AgentLoop integration (harness-loop)

New fields:
```rust
pub recall: Option<Arc<dyn harness_core::RecallStore>>,
pub recall_auto_inject: bool,
```

Builder:
```rust
pub fn with_recall(mut self, store: Arc<dyn RecallStore>) -> Self {
    self.tools.insert(Arc::new(SessionSearchTool::new(store.clone())));
    self.recall = Some(store);
    self
}
pub fn auto_inject(mut self) -> Self {       // only meaningful after with_recall
    self.recall_auto_inject = true;
    self
}
```
When `recall_auto_inject` is set and a store is present, a `RecallGuide` is
included among the guides at run time.

**Owner + session id** are read from `world.profile.extra` at the start of
`run_built_context`:
- `owner` = `extra["recall_owner"].as_str()` or `"default"`.
- `session_id` = `extra["recall_session"].as_str()` or a freshly generated
  id `format!("sess-{ms}-{n}")` where `ms = world.clock.now_ms()` and `n` is a
  process-global `AtomicU64` counter (keeps harness-loop dep-free — no `uuid`).

This lets a multi-turn app (e.g. dashboard) map its own `user_id` →
`recall_owner` and `chat_session_id` → `recall_session`, so messages across
many `run()` calls accumulate in one recall session; single-shot agents get a
fresh session with zero config.

**Capture points** (all guarded by `if let Some(store) = &self.recall`, all
best-effort — `Err` → `tracing::warn!`, never propagate):
1. Before the iteration loop: `ensure_session(owner, session, meta{started_at = clock.now_ms, source})` and `append` the initial task as a `role="user"` message.
2. After `PostModel`: `append` the assistant message (`content` + serialized `tool_calls`).
3. After each tool result: `append` a `role="tool"` message (`tool_name`, `content`).

Capture writes are awaited inline in the loop (async); their latency is small
and bounded, and failures are swallowed.

## `SessionSearchTool` (harness-loop)

```rust
pub struct SessionSearchTool { store: Arc<dyn RecallStore>, schema: ToolSchema }
```
- `name` = `"session_search"`, `risk` = `ReadOnly`.
- Input schema (all optional): `query: string`, `session_id: string`,
  `around: integer`, `window: integer (default 5)`, `limit: integer (default 3)`.
- Dispatch:
  - `query` present → `store.search(owner, query, limit)` → `SessionHit[]`.
  - else `session_id` present → `store.scroll(owner, session_id, around, window)`.
  - else → `store.recent(owner, limit)`.
- `owner` is read from `world.profile.extra["recall_owner"]` (fallback `"default"`)
  — identical resolution to capture, so the tool can only ever see the caller's
  own sessions.
- Returns `ToolResult { ok: true, content: <json>, .. }` (the `SessionHit` /
  message / `SessionMeta` arrays serialized).

## `RecallGuide` (harness-loop, opt-in)

Implements `Guide`. On `apply` (session start): if the task has text, call
`store.search(owner, task_text, 3)` and inject a compact block of the top
snippets as context ("Possibly-relevant past context: …"). No-op when results
are empty. Off unless `.auto_inject()` was called. Tool-only remains the default
(prompt-cache friendly).

## Error handling

| Situation | Behavior |
|---|---|
| Store error during capture | `tracing::warn!`, continue the turn (never fail) |
| Store error inside `session_search` tool | return `ToolResult{ ok:false, content: error }` (agent sees it, keeps going) |
| Missing `recall_owner` in profile | use `"default"` |
| Missing `recall_session` in profile | generate `sess-{ms}-{n}` (single-shot session) |
| Empty query / no results | return empty results, not an error |
| `SqliteRecall` FTS5 unavailable | `open()` returns `RecallError::Backend` with a clear message (bundled rusqlite ships FTS5, so this is a guard, not a normal path) |

## Testing

A shared **contract test suite** runs against BOTH `FileRecall` and
`SqliteRecall` (a generic `async fn recall_contract(store: Arc<dyn RecallStore>)`):
- append → search round-trip (English token match).
- **owner isolation**: owner `a` appends; owner `b`'s `search`/`recent`/`scroll`
  return nothing from `a` (the privacy-critical test).
- `scroll` window bounds (around an anchor, clamped at session edges).
- `recent` ordering (newest first) + `limit`.
- `ensure_session` idempotency; `message_count` increments on append.

Impl-specific:
- `SqliteRecall`: CJK trigram search hits (≥3 Chinese chars); short-CJK `LIKE`
  fallback; BM25 ordering; `snippet()` markers present.
- `FileRecall`: substring/token-overlap hit; snippet self-cut; survives a
  malformed JSONL line (skips it, doesn't crash).

Loop-level:
- `AgentLoop::with_recall(store)` + a mock model that emits one assistant turn +
  one tool call → the store ends with the user task, the assistant message, and
  the tool message, all under the owner/session from `profile.extra`.
- `RecallGuide` injects when `search` returns hits and is absent without
  `.auto_inject()`.

## Out of scope (v1)

- Embeddings / vector search (Hermes itself is pure FTS5 — matched).
- Search-time LLM summarization of old sessions (Hermes's "summarization" is
  in-session trajectory compression, a separate concern).
- Cross-owner / admin search.
- Automatic pruning or compaction of old recall sessions.
- A `harness-tools-recall` separate crate (folded into harness-loop for the
  one-call DX; revisit only if the tool surface grows).

## Dogfood / reference consumer

After the capability lands, wire `examples/dashboard` to it as the reference:
map `user_id → recall_owner`, `chat_session_id → recall_session`, pass a
`SqliteRecall` (dashboard already uses rusqlite), and confirm the chat agent can
`session_search` its own prior conversations. This doubles as the usage example
in docs.
