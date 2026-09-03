//! SurrealDB backend for [`harness_core::Memory`] and [`harness_core::RecallStore`].
//!
//! One store, both traits, three ways to run it:
//!
//! | endpoint             | what it is                                   | feature          |
//! |----------------------|----------------------------------------------|------------------|
//! | `mem://`             | in-process, gone when dropped (tests)        | `surreal`        |
//! | `surrealkv://<dir>`  | in-process, a directory on disk, no server   | `surreal`        |
//! | `ws://host:port`     | a running `surreal start`                    | `surreal-remote` |
//!
//! Recall is BM25 full-text (SurrealDB's own index) with a bigram/trigram
//! index beside it so CJK queries work without a segmenter — the same split
//! `harness-recall-sqlite` makes between FTS5 and its trigram table. Hand the
//! store an [`Embedder`] and memory recall becomes hybrid: an HNSW cosine
//! index over the embeddings, fused with the text ranking by reciprocal rank.
//! Without an embedder nothing vector-related is defined or queried.
//!
//! ```no_run
//! # async fn demo() -> Result<(), harness_core::MemoryError> {
//! use harness_memory::surreal::{SurrealConfig, SurrealMemory};
//! let mem = SurrealMemory::open(SurrealConfig::on_disk("/var/lib/myapp/memory")).await?;
//! mem.write(harness_core::MemoryEntry::new("user prefers dark mode")).await?;
//! # Ok(()) }
//! # use harness_core::Memory;
//! ```
//!
//! Row shapes are plain objects with a `mid`/`seq` field, so the tables stay
//! readable from `surreal sql` and any other client; nothing here depends on
//! SurrealDB-side schema beyond the indexes.

use async_trait::async_trait;
use harness_core::{
    Embedder, EmbedderExt, Memory, MemoryEntry, MemoryError, RecallError, RecallMessage,
    RecallStore, SessionHit, SessionMeta,
};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

/// Where and how to open the store. Build with the constructors, refine with
/// the builder methods, hand to [`SurrealMemory::open`].
#[derive(Clone)]
pub struct SurrealConfig {
    /// `mem://`, `surrealkv://<dir>`, `ws://host:port`, `wss://…`.
    pub endpoint: String,
    pub namespace: String,
    pub database: String,
    /// Root credentials for a remote server, `(username, password)`. Ignored
    /// by the embedded engines.
    pub root: Option<(String, String)>,
    /// Enables vector recall. The index dimension is taken from
    /// [`Embedder::dim`]; the embedder's [`Embedder::handle`] tags every row it
    /// embeds so a later model swap never compares vectors across models.
    pub embedder: Option<Arc<dyn Embedder>>,
}

impl std::fmt::Debug for SurrealConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurrealConfig")
            .field("endpoint", &self.endpoint)
            .field("namespace", &self.namespace)
            .field("database", &self.database)
            .field("root", &self.root.as_ref().map(|(u, _)| u))
            .field(
                "embedder",
                &self.embedder.as_ref().map(|e| e.handle().to_string()),
            )
            .finish()
    }
}

impl SurrealConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            namespace: "harness".into(),
            database: "memory".into(),
            root: None,
            embedder: None,
        }
    }

    /// In-process, nothing on disk. For tests and throwaway agents.
    pub fn in_memory() -> Self {
        Self::new("mem://")
    }

    /// In-process, persisted under `dir` (SurrealKV). No server to run.
    pub fn on_disk(dir: impl AsRef<std::path::Path>) -> Self {
        Self::new(format!("surrealkv://{}", dir.as_ref().display()))
    }

    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = ns.into();
        self
    }

    pub fn database(mut self, db: impl Into<String>) -> Self {
        self.database = db.into();
        self
    }

    pub fn root(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.root = Some((username.into(), password.into()));
        self
    }

    pub fn embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }
}

/// The store. Cheap to clone (the connection is shared).
#[derive(Clone)]
pub struct SurrealMemory {
    db: Surreal<Any>,
    embedder: Option<Arc<dyn Embedder>>,
}

const MEMORY: &str = "memory";
const SESSION: &str = "session";
const MESSAGE: &str = "message";

/// Reciprocal-rank-fusion constant; the standard 60 from the original paper.
const RRF_K: f64 = 60.0;

fn me(e: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(e.to_string())
}

fn re(e: impl std::fmt::Display) -> RecallError {
    RecallError::Backend(e.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Time-prefixed so ids sort by creation; a per-process random suffix so two
/// writers in the same millisecond do not collide.
fn short_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    format!("{:x}-{:08x}", now_ms(), h.finish() as u32)
}

fn count_cjk(s: &str) -> usize {
    s.chars()
        .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
        .count()
}

/// Which full-text index a query should go to. Three or more CJK characters
/// and the word tokenizer would see one opaque token; the n-gram index is the
/// one that can match inside it.
fn text_field(query: &str) -> &'static str {
    if count_cjk(query) >= 3 {
        "hay_cjk"
    } else {
        "hay"
    }
}

/// Cut a highlighted haystack down to what the agent needs to see: the first
/// marked match with some room either side.
fn trim_snippet(hl: &str, radius: usize) -> String {
    let chars: Vec<char> = hl.chars().collect();
    let at = hl.find(">>>").map(|b| hl[..b].chars().count()).unwrap_or(0);
    let lo = at.saturating_sub(radius);
    let hi = (at + radius * 2).min(chars.len());
    let mut s: String = chars[lo..hi].iter().collect();
    if lo > 0 {
        s.insert(0, '…');
    }
    if hi < chars.len() {
        s.push('…');
    }
    s
}

/// Mark the first occurrence of `needle` in `text` the way the full-text
/// highlighter would; used where the n-gram index (no highlighting) matched.
fn mark_first(text: &str, needle: &str) -> String {
    let needle = needle.trim();
    match (!needle.is_empty()).then(|| text.find(needle)).flatten() {
        Some(b) => format!(
            "{}>>>{}<<<{}",
            &text[..b],
            needle,
            &text[b + needle.len()..]
        ),
        None => text.to_string(),
    }
}

fn hay_of(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

impl SurrealMemory {
    /// Connect, select namespace/database, define the schema (idempotent).
    pub async fn open(cfg: SurrealConfig) -> Result<Self, MemoryError> {
        let db = Self::connect(&cfg.endpoint).await?;
        if let Some((username, password)) = &cfg.root {
            db.signin(surrealdb::opt::auth::Root {
                username: username.clone(),
                password: password.clone(),
            })
            .await
            .map_err(|e| me(format!("signin: {e}")))?;
        }
        db.use_ns(cfg.namespace.as_str())
            .use_db(cfg.database.as_str())
            .await
            .map_err(|e| me(format!("use {}/{}: {e}", cfg.namespace, cfg.database)))?;
        let store = Self {
            db,
            embedder: cfg.embedder,
        };
        store.init_schema().await?;
        Ok(store)
    }

    /// Dropping a store does not release an on-disk engine synchronously: the
    /// SDK shuts the datastore down on a background task after the last handle
    /// goes, and there is no way to await it. Reopening the same directory in
    /// the same process right after a drop therefore hits SurrealKV's lock
    /// file for a moment. Wait it out — but only for that error, and not for
    /// long: a lock held by another process is a real conflict.
    async fn connect(endpoint: &str) -> Result<Surreal<Any>, MemoryError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match surrealdb::engine::any::connect(endpoint).await {
                Ok(db) => return Ok(db),
                Err(e)
                    if e.to_string().contains("already locked")
                        && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(me(format!("connect {endpoint}: {e}"))),
            }
        }
    }

    /// [`SurrealConfig::in_memory`] opened.
    pub async fn in_memory() -> Result<Self, MemoryError> {
        Self::open(SurrealConfig::in_memory()).await
    }

    /// The underlying client, for queries this crate has no method for.
    pub fn client(&self) -> &Surreal<Any> {
        &self.db
    }

    pub fn embedder(&self) -> Option<&Arc<dyn Embedder>> {
        self.embedder.as_ref()
    }

    async fn init_schema(&self) -> Result<(), MemoryError> {
        // Two analyzers: words for everything the `class` tokenizer can split,
        // n-grams for scripts it cannot. Each table indexes the same haystack
        // twice, once per analyzer, under two field names — the planner picks
        // an index by field, so the query chooses the analyzer by naming the
        // field.
        let ddl = format!(
            r#"
            DEFINE ANALYZER IF NOT EXISTS hx_words TOKENIZERS class FILTERS lowercase, ascii, snowball(english);
            DEFINE ANALYZER IF NOT EXISTS hx_ngram TOKENIZERS class FILTERS lowercase, ngram(2, 3);

            DEFINE TABLE IF NOT EXISTS {MEMORY} SCHEMALESS;
            DEFINE INDEX IF NOT EXISTS {MEMORY}_created ON {MEMORY} FIELDS created_ms;
            DEFINE INDEX IF NOT EXISTS {MEMORY}_hay ON {MEMORY} FIELDS hay FULLTEXT ANALYZER hx_words BM25 HIGHLIGHTS;
            DEFINE INDEX IF NOT EXISTS {MEMORY}_hay_cjk ON {MEMORY} FIELDS hay_cjk FULLTEXT ANALYZER hx_ngram BM25;

            DEFINE TABLE IF NOT EXISTS {SESSION} SCHEMALESS;
            DEFINE INDEX IF NOT EXISTS {SESSION}_owner ON {SESSION} FIELDS owner, started_at_ms;

            DEFINE TABLE IF NOT EXISTS {MESSAGE} SCHEMALESS;
            DEFINE INDEX IF NOT EXISTS {MESSAGE}_seq ON {MESSAGE} FIELDS owner, session_id, seq;
            DEFINE INDEX IF NOT EXISTS {MESSAGE}_hay ON {MESSAGE} FIELDS hay FULLTEXT ANALYZER hx_words BM25 HIGHLIGHTS;
            DEFINE INDEX IF NOT EXISTS {MESSAGE}_hay_cjk ON {MESSAGE} FIELDS hay_cjk FULLTEXT ANALYZER hx_ngram BM25;
            "#
        );
        self.db.query(ddl).await.map_err(me)?.check().map_err(me)?;

        if let Some(emb) = &self.embedder {
            // The dimension is part of the index name: swapping to a model of
            // another size defines a second index instead of failing on the
            // first, and rows are tagged with the model handle so recall only
            // ever compares like with like.
            let dim = emb.dim();
            let ddl = format!(
                "DEFINE INDEX IF NOT EXISTS {MEMORY}_vec_{dim} ON {MEMORY} FIELDS embedding \
                 HNSW DIMENSION {dim} DIST COSINE TYPE F32;"
            );
            self.db.query(ddl).await.map_err(me)?.check().map_err(me)?;
        }
        Ok(())
    }

    async fn rows(&self, sql: impl Into<String>, vars: Json) -> Result<Vec<Json>, MemoryError> {
        let mut res = self
            .db
            .query(sql.into())
            .bind(vars)
            .await
            .map_err(me)?
            .check()
            .map_err(me)?;
        res.take::<Vec<Json>>(0).map_err(me)
    }

    /// `true` if a row was removed.
    pub async fn delete_by_id(&self, id: &str) -> Result<bool, MemoryError> {
        let rows = self
            .rows(
                format!("DELETE type::record(\"{MEMORY}\", $id) RETURN BEFORE;"),
                json!({ "id": id }),
            )
            .await?;
        Ok(!rows.is_empty())
    }

    /// Number of rows removed.
    pub async fn delete_all(&self) -> Result<u32, MemoryError> {
        let rows = self
            .rows(format!("DELETE {MEMORY} RETURN BEFORE;"), json!({}))
            .await?;
        Ok(rows.len() as u32)
    }

    /// Drop entries whose TTL has passed. Recall already skips them; this
    /// reclaims the space.
    pub async fn compact(&self) -> Result<u32, MemoryError> {
        let rows = self
            .rows(
                format!(
                    "DELETE {MEMORY} WHERE expires_ms != NONE AND expires_ms <= $now RETURN BEFORE;"
                ),
                json!({ "now": now_ms() }),
            )
            .await?;
        Ok(rows.len() as u32)
    }

    async fn text_recall(
        &self,
        query: &str,
        n: usize,
        now: i64,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let field = text_field(query);
        let rows = self
            .rows(
                format!(
                    "SELECT mid AS id, content, tags, source, created_ms, expires_ms, \
                     search::score(1) AS score FROM {MEMORY} \
                     WHERE {field} @1,OR@ $q AND (expires_ms = NONE OR expires_ms > $now) \
                     ORDER BY score DESC, created_ms DESC LIMIT {n};"
                ),
                json!({ "q": query, "now": now }),
            )
            .await?;
        if !rows.is_empty() {
            return rows.into_iter().map(entry_from).collect();
        }
        // Nothing tokenised (a one-character CJK query, punctuation…): a plain
        // substring scan, most recent first, so the model still gets a signal.
        let rows = self
            .rows(
                format!(
                    "SELECT mid AS id, content, tags, source, created_ms, expires_ms FROM {MEMORY} \
                     WHERE string::contains(string::lowercase(hay), string::lowercase($q)) \
                     AND (expires_ms = NONE OR expires_ms > $now) \
                     ORDER BY created_ms DESC LIMIT {n};"
                ),
                json!({ "q": query.trim(), "now": now }),
            )
            .await?;
        rows.into_iter().map(entry_from).collect()
    }

    async fn vector_recall(
        &self,
        emb: &Arc<dyn Embedder>,
        query: &str,
        n: usize,
        now: i64,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let vec = emb
            .embed_one(query)
            .await
            .map_err(|e| me(format!("embed query: {e}")))?;
        let ef = (n * 4).clamp(40, 400);
        let rows = self
            .rows(
                format!(
                    "SELECT mid AS id, content, tags, source, created_ms, expires_ms, \
                     vector::distance::knn() AS dist FROM {MEMORY} \
                     WHERE embedder = $h AND (expires_ms = NONE OR expires_ms > $now) \
                     AND embedding <|{n},{ef}|> $v \
                     ORDER BY dist ASC;"
                ),
                json!({ "h": emb.handle(), "now": now, "v": vec }),
            )
            .await?;
        rows.into_iter().map(entry_from).collect()
    }
}

fn entry_from(row: Json) -> Result<MemoryEntry, MemoryError> {
    serde_json::from_value(row).map_err(|e| MemoryError::Serde(e.to_string()))
}

/// Reciprocal rank fusion of two ranked lists keyed by id. Items in both
/// lists rise above items in one; ties fall back to recency.
fn fuse(lists: Vec<Vec<MemoryEntry>>, k: usize) -> Vec<MemoryEntry> {
    let mut score: HashMap<String, f64> = HashMap::new();
    let mut by_id: HashMap<String, MemoryEntry> = HashMap::new();
    for list in lists {
        for (rank, e) in list.into_iter().enumerate() {
            *score.entry(e.id.clone()).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            by_id.entry(e.id.clone()).or_insert(e);
        }
    }
    let mut out: Vec<MemoryEntry> = by_id.into_values().collect();
    out.sort_by(|a, b| {
        let sa = score.get(&a.id).copied().unwrap_or(0.0);
        let sb = score.get(&b.id).copied().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.created_ms.cmp(&a.created_ms))
    });
    out.truncate(k);
    out
}

#[async_trait]
impl Memory for SurrealMemory {
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        if k == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let now = now_ms();
        let Some(emb) = &self.embedder else {
            return self.text_recall(query, k, now).await;
        };
        // Over-fetch each side so fusion has something to fuse.
        let n = k * 2;
        let text = self.text_recall(query, n, now).await?;
        let vectors = self.vector_recall(emb, query, n, now).await?;
        Ok(fuse(vec![text, vectors], k))
    }

    async fn write(&self, mut entry: MemoryEntry) -> Result<(), MemoryError> {
        if entry.id.is_empty() {
            entry.id = short_id();
        }
        if entry.created_ms == 0 {
            entry.created_ms = now_ms();
        }
        let tags = entry.tags.join(" ");
        let hay = hay_of(&[&entry.content, &tags]);
        let mut doc = json!({
            "mid": entry.id,
            "content": entry.content,
            "tags": entry.tags,
            "created_ms": entry.created_ms,
            "hay": hay,
            "hay_cjk": hay,
        });
        // Absent, not null: `expires_ms = NONE` is what recall filters on.
        if let Some(s) = &entry.source {
            doc["source"] = json!(s);
        }
        if let Some(t) = entry.expires_ms {
            doc["expires_ms"] = json!(t);
        }
        if let Some(emb) = &self.embedder {
            let v = emb
                .embed_one(&hay)
                .await
                .map_err(|e| me(format!("embed entry: {e}")))?;
            doc["embedding"] = json!(v);
            doc["embedder"] = json!(emb.handle());
        }
        self.rows(
            format!("UPSERT type::record(\"{MEMORY}\", $id) CONTENT $doc;"),
            json!({ "id": entry.id, "doc": doc }),
        )
        .await?;
        Ok(())
    }
}

// ── cross-session recall ────────────────────────────────────────────────

const MSG_COLS: &str = "seq AS id, role, content, tool_name, tool_calls, ts_ms";
const META_COLS: &str = "session_id, title, source, started_at_ms, message_count";

fn msg_from(row: Json) -> Result<RecallMessage, RecallError> {
    serde_json::from_value(row).map_err(|e| RecallError::Serde(e.to_string()))
}

fn meta_from(row: Json) -> Result<SessionMeta, RecallError> {
    serde_json::from_value(row).map_err(|e| RecallError::Serde(e.to_string()))
}

impl SurrealMemory {
    async fn rrows(&self, sql: impl Into<String>, vars: Json) -> Result<Vec<Json>, RecallError> {
        self.rows(sql, vars).await.map_err(re)
    }

    async fn messages(
        &self,
        owner: &str,
        session_id: &str,
        cond: &str,
        order: &str,
        vars: Json,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        let mut vars = vars;
        vars["owner"] = json!(owner);
        vars["sid"] = json!(session_id);
        let rows = self
            .rrows(
                format!(
                    "SELECT {MSG_COLS} FROM {MESSAGE} \
                     WHERE owner = $owner AND session_id = $sid {cond} {order};"
                ),
                vars,
            )
            .await?;
        rows.into_iter().map(msg_from).collect()
    }

    async fn read_window(
        &self,
        owner: &str,
        session_id: &str,
        lo: i64,
        hi: i64,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        self.messages(
            owner,
            session_id,
            "AND seq >= $lo AND seq <= $hi",
            "ORDER BY seq ASC",
            json!({ "lo": lo, "hi": hi }),
        )
        .await
    }

    async fn read_first(
        &self,
        owner: &str,
        session_id: &str,
        n: usize,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        self.messages(
            owner,
            session_id,
            "",
            &format!("ORDER BY seq ASC LIMIT {n}"),
            json!({}),
        )
        .await
    }

    async fn read_last(
        &self,
        owner: &str,
        session_id: &str,
        n: usize,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        let mut v = self
            .messages(
                owner,
                session_id,
                "",
                &format!("ORDER BY seq DESC LIMIT {n}"),
                json!({}),
            )
            .await?;
        v.reverse();
        Ok(v)
    }

    async fn meta_of(
        &self,
        owner: &str,
        session_id: &str,
    ) -> Result<Option<SessionMeta>, RecallError> {
        let rows = self
            .rrows(
                format!("SELECT {META_COLS} FROM type::record(\"{SESSION}\", [$owner, $sid]);"),
                json!({ "owner": owner, "sid": session_id }),
            )
            .await?;
        rows.into_iter().next().map(meta_from).transpose()
    }

    /// Ranked `(seq, session_id, snippet)` anchors for `query`, best first.
    async fn anchors(
        &self,
        owner: &str,
        query: &str,
        n: usize,
    ) -> Result<Vec<(i64, String, String)>, RecallError> {
        let field = text_field(query);
        // Only the word index carries highlights; the n-gram side gets its
        // markers from a substring scan of the plain content.
        let snippet = if field == "hay" {
            "search::highlight('>>>', '<<<', 1)"
        } else {
            "content"
        };
        let mut rows = self
            .rrows(
                format!(
                    "SELECT seq, session_id, {snippet} AS snippet, search::score(1) AS score \
                     FROM {MESSAGE} WHERE owner = $owner AND {field} @1@ $q \
                     ORDER BY score DESC, seq DESC LIMIT {n};"
                ),
                json!({ "owner": owner, "q": query }),
            )
            .await?;
        if rows.is_empty() {
            rows = self
                .rrows(
                    format!(
                        "SELECT seq, session_id, content AS snippet FROM {MESSAGE} \
                         WHERE owner = $owner AND string::contains(hay, $q) \
                         ORDER BY seq DESC LIMIT {n};"
                    ),
                    json!({ "owner": owner, "q": query.trim() }),
                )
                .await?;
        }
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let seq = r.get("seq")?.as_i64()?;
                let sid = r.get("session_id")?.as_str()?.to_string();
                let raw = r.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
                let marked = if raw.contains(">>>") {
                    raw.to_string()
                } else {
                    mark_first(raw, query)
                };
                Some((seq, sid, trim_snippet(&marked, 60)))
            })
            .collect())
    }
}

#[async_trait]
impl RecallStore for SurrealMemory {
    async fn ensure_session(
        &self,
        owner: &str,
        session_id: &str,
        meta: &SessionMeta,
    ) -> Result<(), RecallError> {
        // UPSERT creates or leaves alone; `??` keeps whatever an earlier call
        // already recorded, including the running message count.
        self.rrows(
            format!(
                "UPSERT type::record(\"{SESSION}\", [$owner, $sid]) SET \
                 owner = $owner, session_id = $sid, \
                 title = title ?? $title, source = source ?? $source, \
                 started_at_ms = started_at_ms ?? $started, \
                 message_count = message_count ?? 0;"
            ),
            json!({
                "owner": owner, "sid": session_id,
                "title": meta.title, "source": meta.source,
                "started": meta.started_at_ms,
            }),
        )
        .await?;
        Ok(())
    }

    async fn append(
        &self,
        owner: &str,
        session_id: &str,
        msg: &RecallMessage,
    ) -> Result<i64, RecallError> {
        let bump = format!(
            "UPDATE type::record(\"{SESSION}\", [$owner, $sid]) SET message_count += 1 RETURN message_count;"
        );
        let vars = json!({ "owner": owner, "sid": session_id });
        let mut rows = self.rrows(bump.clone(), vars.clone()).await?;
        if rows.is_empty() {
            // Appending to a session nobody declared: declare it, dated from
            // the message, rather than lose the message.
            self.ensure_session(owner, session_id, &SessionMeta::new(session_id, msg.ts_ms))
                .await?;
            rows = self.rrows(bump, vars).await?;
        }
        let seq = rows
            .first()
            .and_then(|r| r.get("message_count"))
            .and_then(|v| v.as_i64())
            .ok_or_else(|| re("session counter missing"))?;

        let hay = hay_of(&[
            &msg.content,
            msg.tool_name.as_deref().unwrap_or(""),
            msg.tool_calls.as_deref().unwrap_or(""),
        ]);
        let mut doc = json!({
            "seq": seq,
            "owner": owner,
            "session_id": session_id,
            "role": msg.role,
            "content": msg.content,
            "ts_ms": msg.ts_ms,
            "hay": hay,
            "hay_cjk": hay,
        });
        if let Some(t) = &msg.tool_name {
            doc["tool_name"] = json!(t);
        }
        if let Some(c) = &msg.tool_calls {
            doc["tool_calls"] = json!(c);
        }
        self.rrows(
            format!("CREATE type::record(\"{MESSAGE}\", [$owner, $sid, $seq]) CONTENT $doc;"),
            json!({ "owner": owner, "sid": session_id, "seq": seq, "doc": doc }),
        )
        .await?;
        Ok(seq)
    }

    async fn search(
        &self,
        owner: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionHit>, RecallError> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let anchors = self.anchors(owner, query, limit * 5).await?;
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for (anchor_id, session_id, snippet) in anchors {
            if !seen.insert(session_id.clone()) {
                continue;
            }
            let Some(meta) = self.meta_of(owner, &session_id).await? else {
                continue;
            };
            let around = self
                .read_window(owner, &session_id, anchor_id - 5, anchor_id + 5)
                .await?;
            let bookend_start = self.read_first(owner, &session_id, 3).await?;
            let bookend_end = self.read_last(owner, &session_id, 3).await?;
            hits.push(SessionHit::new(
                meta,
                snippet,
                anchor_id,
                bookend_start,
                around,
                bookend_end,
            ));
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    async fn scroll(
        &self,
        owner: &str,
        session_id: &str,
        around: i64,
        window: usize,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        let w = window as i64;
        self.read_window(owner, session_id, around - w, around + w)
            .await
    }

    async fn recent(&self, owner: &str, limit: usize) -> Result<Vec<SessionMeta>, RecallError> {
        let rows = self
            .rrows(
                format!(
                    "SELECT {META_COLS} FROM {SESSION} WHERE owner = $owner \
                     ORDER BY started_at_ms DESC LIMIT {limit};"
                ),
                json!({ "owner": owner }),
            )
            .await?;
        rows.into_iter().map(meta_from).collect()
    }
}

/// The delete half of the memory tools (`ForgetMemoryTool` in harness-tools).
#[async_trait]
impl harness_core::MemoryDelete for SurrealMemory {
    async fn delete_by_id(&self, id: &str) -> Result<bool, String> {
        SurrealMemory::delete_by_id(self, id)
            .await
            .map_err(|e| e.to_string())
    }
    async fn delete_all(&self) -> Result<u32, String> {
        SurrealMemory::delete_all(self)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::EmbedError;

    async fn fresh() -> SurrealMemory {
        SurrealMemory::in_memory().await.expect("open mem://")
    }

    fn entry(content: &str, tags: &[&str]) -> MemoryEntry {
        let mut e = MemoryEntry::new(content);
        e.tags = tags.iter().map(|t| t.to_string()).collect();
        e
    }

    #[tokio::test]
    async fn recall_ranks_the_entry_that_matches_most_words_first() {
        let m = fresh().await;
        m.write(entry("user prefers dark mode in the editor", &["ui"]))
            .await
            .unwrap();
        m.write(entry(
            "payment service deploys on fridays",
            &["ops", "payment"],
        ))
        .await
        .unwrap();
        m.write(entry("the payment team uses rust", &["team"]))
            .await
            .unwrap();

        let got = m.recall("payment deploy", 5).await.unwrap();
        assert_eq!(
            got.len(),
            2,
            "OR semantics: both payment entries, not the ui one"
        );
        assert!(
            got[0].content.contains("deploys"),
            "the one matching both words leads: {got:?}"
        );
        assert!(!got[0].id.is_empty(), "backend assigned an id");
    }

    #[tokio::test]
    async fn cjk_queries_match_inside_unsegmented_text() {
        let m = fresh().await;
        m.write(entry("我们明天要上线支付服务", &[])).await.unwrap();
        m.write(entry("周五团建", &[])).await.unwrap();

        let got = m.recall("支付服务", 5).await.unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].content.contains("支付"));

        // Below the n-gram floor: the substring scan still finds it.
        let got = m.recall("团", 5).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].content, "周五团建");
    }

    #[tokio::test]
    async fn expired_entries_are_skipped_and_compact_drops_them() {
        let m = fresh().await;
        let mut dead = entry("old password hint", &[]);
        dead.expires_ms = Some(1); // 1970
        m.write(dead).await.unwrap();
        let mut live = entry("current password hint", &[]);
        live.expires_ms = Some(now_ms() + 60_000);
        m.write(live).await.unwrap();
        m.write(entry("permanent password policy", &[]))
            .await
            .unwrap();

        let got = m.recall("password", 10).await.unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|e| !e.content.starts_with("old")));

        assert_eq!(m.compact().await.unwrap(), 1);
        assert_eq!(m.compact().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn blank_query_or_zero_k_is_empty_not_an_error() {
        let m = fresh().await;
        m.write(entry("something", &[])).await.unwrap();
        assert!(m.recall("   ", 5).await.unwrap().is_empty());
        assert!(m.recall("something", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_id_written_twice_is_one_row_with_the_new_content() {
        let m = fresh().await;
        let mut e = entry("first draft", &[]);
        e.id = "note-1".into();
        m.write(e.clone()).await.unwrap();
        e.content = "second draft".into();
        m.write(e).await.unwrap();

        let got = m.recall("draft", 10).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "note-1");
        assert_eq!(got[0].content, "second draft");
    }

    #[tokio::test]
    async fn delete_by_id_and_delete_all() {
        let m = fresh().await;
        let mut e = entry("to be forgotten", &[]);
        e.id = "x".into();
        m.write(e).await.unwrap();
        m.write(entry("also forgotten", &[])).await.unwrap();

        assert!(m.delete_by_id("x").await.unwrap());
        assert!(!m.delete_by_id("x").await.unwrap());
        assert_eq!(m.delete_all().await.unwrap(), 1);
        assert!(m.recall("forgotten", 5).await.unwrap().is_empty());
    }

    /// Axis-per-topic embedder: anything about cats lands on one axis,
    /// dogs on another, everything else on a third. Cosine distance then
    /// says "kitten" is a cat without either word being shared.
    struct TopicEmbed;

    #[async_trait]
    impl Embedder for TopicEmbed {
        async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(inputs
                .iter()
                .map(|s| {
                    let s = s.to_lowercase();
                    if s.contains("cat") || s.contains("kitten") || s.contains("feline") {
                        vec![1.0, 0.0, 0.0]
                    } else if s.contains("dog") || s.contains("puppy") {
                        vec![0.0, 1.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0]
                    }
                })
                .collect())
        }
        fn dim(&self) -> usize {
            3
        }
        fn handle(&self) -> &str {
            "test:topic-3"
        }
    }

    #[tokio::test]
    async fn with_an_embedder_recall_is_hybrid() {
        let m = SurrealMemory::open(SurrealConfig::in_memory().embedder(Arc::new(TopicEmbed)))
            .await
            .unwrap();
        m.write(entry("the cat sleeps on the radiator", &[]))
            .await
            .unwrap();
        m.write(entry("the dog chases the ball", &[]))
            .await
            .unwrap();
        m.write(entry("quarterly budget review", &[]))
            .await
            .unwrap();

        // No shared word: only the vector side can find this.
        let got = m.recall("kitten", 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].content.contains("cat"), "{got:?}");

        // Shared word AND right axis: fusion puts the dog first.
        let got = m.recall("puppy ball", 2).await.unwrap();
        assert!(got[0].content.contains("dog"), "{got:?}");
    }

    #[tokio::test]
    async fn on_disk_store_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("harness-surreal-{}", short_id()));
        {
            let m = SurrealMemory::open(SurrealConfig::on_disk(&dir))
                .await
                .unwrap();
            m.write(entry("persisted across restarts", &[]))
                .await
                .unwrap();
        }
        let m = SurrealMemory::open(SurrealConfig::on_disk(&dir))
            .await
            .unwrap();
        let got = m.recall("persisted", 5).await.unwrap();
        assert_eq!(got.len(), 1);
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn recall_store_conformance() {
        let m = fresh().await;
        harness_core::recall_contract(Arc::new(m)).await;
    }

    #[tokio::test]
    async fn recall_search_highlights_and_isolates_owners_for_cjk_too() {
        let m = fresh().await;
        m.ensure_session("u1", "s1", &SessionMeta::new("s1", 1))
            .await
            .unwrap();
        m.append(
            "u1",
            "s1",
            &RecallMessage::new("user", "deploy the payment service tomorrow", 1),
        )
        .await
        .unwrap();
        m.append(
            "u1",
            "s1",
            &RecallMessage::new("user", "我们明天要上线支付服务", 2),
        )
        .await
        .unwrap();

        let en = m.search("u1", "payment deploy", 5).await.unwrap();
        assert_eq!(en.len(), 1);
        assert!(en[0].snippet.contains(">>>"), "{}", en[0].snippet);

        let zh = m.search("u1", "支付服务", 5).await.unwrap();
        assert_eq!(zh.len(), 1);
        assert_eq!(zh[0].anchor_id, 2);
        assert!(
            zh[0].snippet.contains(">>>支付服务<<<"),
            "{}",
            zh[0].snippet
        );

        assert!(m.search("u2", "支付服务", 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_without_ensure_session_still_lands() {
        let m = fresh().await;
        let id = m
            .append("u1", "adhoc", &RecallMessage::new("user", "hello there", 7))
            .await
            .unwrap();
        assert_eq!(id, 1);
        let recent = m.recent("u1", 5).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].started_at_ms, 7);
        assert_eq!(recent[0].message_count, 1);
    }

    #[test]
    fn snippet_trimming_keeps_the_marked_match() {
        let long = format!("{}>>>hit<<<{}", "a".repeat(200), "b".repeat(200));
        let s = trim_snippet(&long, 20);
        assert!(s.contains(">>>hit<<<"));
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert!(s.chars().count() < 80);
        assert_eq!(mark_first("xx支付服务yy", "支付服务"), "xx>>>支付服务<<<yy");
    }

    #[test]
    fn fusion_prefers_items_on_both_lists() {
        let mk = |id: &str, ts: i64| {
            let mut e = MemoryEntry::new(id);
            e.id = id.into();
            e.created_ms = ts;
            e
        };
        let text = vec![mk("a", 1), mk("b", 2)];
        let vecs = vec![mk("c", 3), mk("b", 2)];
        let out = fuse(vec![text, vecs], 3);
        assert_eq!(out[0].id, "b");
    }
}
