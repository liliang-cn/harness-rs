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
        Ok(Self {
            _client: client,
            save,
            search,
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
        let mut metadata = serde_json::Map::new();
        if !entry.tags.is_empty() {
            metadata.insert("tags".into(), json!(entry.tags));
        }
        if let Some(s) = &entry.source {
            metadata.insert("source".into(), json!(s));
        }
        let mut args = json!({
            "memory_id": id,
            "content": entry.content,
            "scope": self.scope,
            "namespace": self.namespace,
            "metadata": Value::Object(metadata),
        });
        if let Some(u) = &self.user_id {
            args["user_id"] = json!(u);
        }

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
