//! # harness-cortexdb — CortexDB as a harness `Memory`
//!
//! Implements [`harness_core::Memory`] over [CortexDB](https://github.com/liliang-cn/cortexdb)'s
//! MCP server: `recall` calls `memory_search` (semantic / lexical vector
//! retrieval) and `write` calls `memory_save`. Drop it in anywhere a
//! `Memory` is expected — the `MemoryGuide`, the experience layer
//! (`harness-experience`), a scheduler, etc. — to get **semantic recall** and a
//! brain that can be *shared* with other tools (Claude Code / Codex all use
//! `~/.cortexdb` by default).
//!
//! ```ignore
//! use harness_cortexdb::CortexdbMemory;
//! use std::sync::Arc;
//!
//! // Spawn CortexDB's MCP server; share the global brain (~/.cortexdb).
//! let mem = Arc::new(CortexdbMemory::connect_stdio("cortexdb-mcp-stdio", &[]).await?);
//! let recorder = harness_experience::ExperienceRecorder::new(mem); // semantic!
//! ```
//!
//! Harness `MemoryEntry` fields round-trip through CortexDB: `content` maps to
//! the memory content; `tags` + `source` are stored under CortexDB's
//! `metadata` and read back on recall.
//!
//! ## Record conversations, then distill them into the graph
//!
//! Pair the [`TranscriptRecorder`](harness_experience::TranscriptRecorder) hook
//! (turns → CortexDB) with periodic [`CortexdbMemory::consolidate`] (memories →
//! knowledge graph, server-side):
//!
//! ```ignore
//! let mem = Arc::new(CortexdbMemory::connect_stdio("cortexdb-mcp-stdio", &[]).await?
//!     .with_scope("session").with_namespace("myapp-chat"));
//!
//! // 1. capture every turn as it happens
//! let (recorder, rx) = harness_experience::TranscriptRecorder::new("sess-1");
//! harness_experience::spawn_transcript_writer(rx, mem.clone());
//! let loop_ = AgentLoop::new(model).with_hook(Arc::new(recorder));
//!
//! // 2. every 10 min, let CortexDB distill accumulated turns into the graph
//! let m = mem.clone();
//! tokio::spawn(async move {
//!     let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
//!     loop { tick.tick().await; let _ = m.consolidate(serde_json::Value::Null).await; }
//! });
//! ```

use async_trait::async_trait;
use harness_core::{Memory, MemoryEntry, MemoryError, Tool};
use harness_mcp_client::McpClient;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A [`Memory`] backed by CortexDB's MCP `memory_save` / `memory_search` tools.
pub struct CortexdbMemory {
    // Owns the MCP session so the CortexDB child process stays alive.
    _client: McpClient,
    save: Arc<dyn Tool>,
    search: Arc<dyn Tool>,
    /// Optional server-side distillation tool (`knowledge_memory_consolidate`),
    /// present only if the connected CortexDB exposes it. See [`consolidate`].
    consolidate: Option<Arc<dyn Tool>>,
    scope: String,
    namespace: String,
    user_id: Option<String>,
    seq: AtomicU64,
}

impl CortexdbMemory {
    /// Spawn `program args...` as CortexDB's MCP stdio server and adapt it.
    /// (e.g. `connect_stdio("cortexdb-mcp-stdio", &[])`.)
    pub async fn connect_stdio(program: &str, args: &[&str]) -> anyhow::Result<Self> {
        Self::from_client(McpClient::connect_stdio(program, args).await?)
    }

    /// Adapt an already-connected CortexDB MCP client.
    pub fn from_client(client: McpClient) -> anyhow::Result<Self> {
        let tools = client.tools();
        let find = |name: &str| tools.iter().find(|t| t.name() == name).cloned();
        let save = find("memory_save")
            .ok_or_else(|| anyhow::anyhow!("CortexDB MCP server exposes no `memory_save` tool"))?;
        let search = find("memory_search").ok_or_else(|| {
            anyhow::anyhow!("CortexDB MCP server exposes no `memory_search` tool")
        })?;
        // Optional — older/lighter CortexDB builds may not expose it.
        let consolidate = find("knowledge_memory_consolidate");
        Ok(Self {
            _client: client,
            save,
            search,
            consolidate,
            scope: "global".into(),
            namespace: "harness".into(),
            user_id: None,
            seq: AtomicU64::new(0),
        })
    }

    /// CortexDB memory scope: `global` (default, shared), `user`, or `session`.
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }
    /// CortexDB namespace (default `harness`). Isolates this app's memories.
    pub fn with_namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = ns.into();
        self
    }
    /// Scope memories to a user id (for `scope = user`).
    pub fn with_user_id(mut self, user: impl Into<String>) -> Self {
        self.user_id = Some(user.into());
        self
    }

    /// Whether the connected CortexDB exposes the graph-distillation tool.
    pub fn can_consolidate(&self) -> bool {
        self.consolidate.is_some()
    }

    /// Distill accumulated memories into CortexDB's **knowledge graph**
    /// (entities + relations), server-side, via `knowledge_memory_consolidate`.
    /// CortexDB does the extraction — harness just triggers it, so this costs no
    /// tokens on our side. Run it periodically (e.g. from `harness-scheduler`)
    /// after a batch of turns has been recorded.
    ///
    /// Operates on this instance's `scope`/`namespace`/`user_id`. The server
    /// **requires** a `reflect.recall.query`; this method supplies a broad
    /// default plus `promote_to_knowledge: true`, so `consolidate(Value::Null)`
    /// works out of the box. Override either via `extra_args`, e.g.:
    ///
    /// ```ignore
    /// mem.consolidate(serde_json::json!({
    ///     "reflect": { "recall": { "query": "past tasks and tools", "max_memory_items": 30 } }
    /// })).await?;
    /// ```
    ///
    /// **Quality note:** in pure-lexical mode (no embedder configured on the
    /// CortexDB side) entity extraction is poor — configure an embedder for a
    /// clean graph. Returns the raw tool result. Errors if the server doesn't
    /// expose the tool (check [`can_consolidate`](Self::can_consolidate)).
    pub async fn consolidate(&self, extra_args: Value) -> Result<Value, MemoryError> {
        let tool = self.consolidate.as_ref().ok_or_else(|| {
            MemoryError::Backend(
                "this CortexDB build exposes no `knowledge_memory_consolidate` tool".into(),
            )
        })?;
        let args = build_consolidate_args(
            &self.scope,
            &self.namespace,
            self.user_id.as_deref(),
            extra_args,
        );
        let mut world = harness_context::default_world(".");
        let res = tool
            .invoke(args, &mut world)
            .await
            .map_err(|e| MemoryError::Backend(e.to_string()))?;
        if !res.ok {
            return Err(MemoryError::Backend(format!(
                "knowledge_memory_consolidate: {}",
                res.content
            )));
        }
        Ok(res.content)
    }

    fn next_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("harness-{t}-{n}")
    }
}

#[async_trait]
impl Memory for CortexdbMemory {
    async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        let id = if entry.id.is_empty() {
            self.next_id()
        } else {
            entry.id.clone()
        };
        let args = build_save_args(
            &id,
            &entry,
            &self.scope,
            &self.namespace,
            self.user_id.as_deref(),
        );

        let mut world = harness_context::default_world(".");
        let res = self
            .save
            .invoke(args, &mut world)
            .await
            .map_err(|e| MemoryError::Backend(e.to_string()))?;
        if !res.ok {
            return Err(MemoryError::Backend(format!(
                "memory_save: {}",
                res.content
            )));
        }
        Ok(())
    }

    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        if k == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut args = json!({
            "query": query,
            "top_k": k,
            "scope": self.scope,
            "namespace": self.namespace,
        });
        if let Some(u) = &self.user_id {
            args["user_id"] = json!(u);
        }

        let mut world = harness_context::default_world(".");
        let res = self
            .search
            .invoke(args, &mut world)
            .await
            .map_err(|e| MemoryError::Backend(e.to_string()))?;
        if !res.ok {
            return Err(MemoryError::Backend(format!(
                "memory_search: {}",
                res.content
            )));
        }
        // MemorySearchResponse { results: [ { memory: MemoryRecord, score } ] }
        let results = res
            .content
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut out = Vec::with_capacity(results.len());
        for hit in &results {
            // The record is under `memory`; tolerate a flattened shape too.
            let rec = hit.get("memory").unwrap_or(hit);
            let content = rec
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if content.trim().is_empty() {
                continue;
            }
            let mut entry = MemoryEntry::new(content);
            if let Some(id) = rec.get("id").and_then(|v| v.as_str()) {
                entry.id = id.to_string();
            }
            if let Some(meta) = rec.get("metadata") {
                if let Some(tags) = meta.get("tags").and_then(|v| v.as_array()) {
                    entry.tags = tags
                        .iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect();
                }
                if let Some(src) = meta.get("source").and_then(|v| v.as_str()) {
                    entry.source = Some(src.to_string());
                }
            }
            out.push(entry);
        }
        Ok(out)
    }
}

/// Build the `memory_save` args for `entry`. Promotes `role:`/`session:` tags to
/// CortexDB's first-class `role` / `session_id` columns (so a transcript can be
/// queried and ordered by `session_id + role`, not just `json_extract` on
/// metadata); all remaining tags + `source` go into `metadata`.
fn build_save_args(
    id: &str,
    entry: &MemoryEntry,
    scope: &str,
    namespace: &str,
    user_id: Option<&str>,
) -> Value {
    let mut role: Option<String> = None;
    let mut session: Option<String> = None;
    let mut other_tags: Vec<String> = Vec::new();
    for t in &entry.tags {
        if let Some(r) = t.strip_prefix("role:") {
            role = Some(r.to_string());
        } else if let Some(s) = t.strip_prefix("session:") {
            session = Some(s.to_string());
        } else {
            other_tags.push(t.clone());
        }
    }

    let mut metadata = serde_json::Map::new();
    if !other_tags.is_empty() {
        metadata.insert("tags".into(), json!(other_tags));
    }
    if let Some(s) = &entry.source {
        metadata.insert("source".into(), json!(s));
    }

    let mut args = json!({
        "memory_id": id,
        "content": entry.content,
        "scope": scope,
        "namespace": namespace,
        "metadata": Value::Object(metadata),
    });
    if let Some(r) = role {
        args["role"] = json!(r);
    }
    if let Some(s) = session {
        args["session_id"] = json!(s);
    }
    if let Some(u) = user_id {
        args["user_id"] = json!(u);
    }
    args
}

/// Build the `knowledge_memory_consolidate` args: base scope/namespace/user_id,
/// merge caller `extra_args`, then fill the server-required `reflect.recall.query`
/// (+ `promote_to_knowledge`) defaults if the caller didn't supply them.
fn build_consolidate_args(
    scope: &str,
    namespace: &str,
    user_id: Option<&str>,
    extra_args: Value,
) -> Value {
    let mut args = json!({ "scope": scope, "namespace": namespace });
    if let Some(u) = user_id {
        args["user_id"] = json!(u);
    }
    if let Value::Object(extra) = extra_args
        && let Value::Object(base) = &mut args
    {
        base.extend(extra);
    }
    if args.get("reflect").is_none() {
        args["reflect"] = json!({
            "recall": {
                "query": "key facts, entities, decisions, tasks, and tools from recent conversations",
                "max_memory_items": 30
            }
        });
    }
    if args.get("promote_to_knowledge").is_none() {
        args["promote_to_knowledge"] = json!(true);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consolidate_null_fills_required_reflect_defaults() {
        let args = build_consolidate_args("global", "harness", Some("u1"), Value::Null);
        assert_eq!(args["scope"], "global");
        assert_eq!(args["user_id"], "u1");
        // Server-required reflect.recall.query is supplied.
        assert!(args["reflect"]["recall"]["query"].is_string());
        assert_eq!(args["reflect"]["recall"]["max_memory_items"], 30);
        assert_eq!(args["promote_to_knowledge"], true);
    }

    #[test]
    fn consolidate_extra_args_override_defaults() {
        let extra = json!({
            "reflect": { "recall": { "query": "custom", "max_memory_items": 5 } },
            "promote_to_knowledge": false
        });
        let args = build_consolidate_args("session", "app", None, extra);
        assert_eq!(args["reflect"]["recall"]["query"], "custom");
        assert_eq!(args["reflect"]["recall"]["max_memory_items"], 5);
        assert_eq!(args["promote_to_knowledge"], false);
        assert!(args.get("user_id").is_none());
    }

    #[test]
    fn role_and_session_promoted_to_native_params() {
        let entry = MemoryEntry::new("hi there")
            .with_source("transcript")
            .with_tags(["role:assistant", "session:sess-1", "topic:deploy"]);
        let args = build_save_args("m1", &entry, "session", "myapp", Some("u42"));

        // Promoted to first-class columns.
        assert_eq!(args["role"], "assistant");
        assert_eq!(args["session_id"], "sess-1");
        assert_eq!(args["user_id"], "u42");
        assert_eq!(args["scope"], "session");
        assert_eq!(args["namespace"], "myapp");

        // role:/session: stripped from metadata tags; unrelated tag kept.
        let tags = args["metadata"]["tags"].as_array().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0], "topic:deploy");
        assert_eq!(args["metadata"]["source"], "transcript");
    }

    #[test]
    fn no_role_session_tags_omits_native_params() {
        let entry = MemoryEntry::new("note").with_tags(["misc"]);
        let args = build_save_args("m2", &entry, "global", "harness", None);
        assert!(args.get("role").is_none());
        assert!(args.get("session_id").is_none());
        assert!(args.get("user_id").is_none());
        assert_eq!(args["metadata"]["tags"][0], "misc");
    }
}
