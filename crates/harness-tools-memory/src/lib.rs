//! LLM-facing memory tools for harness-rs agents.
//!
//! Three tools the LLM can call directly:
//!
//! - **`remember_this(content, tags?, ttl_days?)`** — store a fact the
//!   user explicitly asked to remember. Bypasses the auto-synthesizer's
//!   judgment about durability; if the user said "记住 X" / "remember Y",
//!   this is the strongest signal possible.
//! - **`forget_memory(id)`** — drop one entry by id. Pair with the
//!   inspection UI so users can clean up wrong facts.
//! - **`list_memories(query?, k?)`** — recall and surface what the agent
//!   currently knows about the user. Useful when the user asks "what do
//!   you remember about me?" so the model doesn't have to guess.
//!
//! All three are **state-bearing** (they hold an `Arc<dyn Memory>`), so
//! they're constructed at agent-loop wiring time rather than via the
//! `#[tool]` macro. For multi-tenant apps, the typical pattern is to
//! construct fresh tools per-request with a per-user `Memory` instance.
//!
//! # Example
//!
//! ```ignore
//! let mem: Arc<dyn Memory> = Arc::new(GuardedMemory::new(Arc::new(
//!     FileMemory::open(memory_path_for_user(&uid))?
//! )));
//! let mut loop_ = AgentLoop::new(model)
//!     .with_tool(Arc::new(RememberThisTool::new(mem.clone())))
//!     .with_tool(Arc::new(ForgetMemoryTool::new(mem.clone())))
//!     .with_tool(Arc::new(ListMemoriesTool::new(mem.clone())));
//! ```

use async_trait::async_trait;
use harness_core::{Memory, MemoryEntry, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
use std::sync::Arc;

// ───── remember_this ─────────────────────────────────────────────────────

/// Tool: store a fact the user explicitly asked the agent to remember.
pub struct RememberThisTool {
    memory: Arc<dyn Memory>,
    schema: ToolSchema,
    source: String,
}

impl RememberThisTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self::with_source(memory, "user-explicit")
    }

    /// Set the `source` tag written into each stored entry. Useful for
    /// multi-app installations sharing one memory file (rare, but supported).
    pub fn with_source(memory: Arc<dyn Memory>, source: impl Into<String>) -> Self {
        Self {
            memory,
            source: source.into(),
            schema: ToolSchema {
                name: "remember_this".into(),
                description: "Store a long-term fact the user explicitly asked you to remember. \
                              Trigger words: \"记住\" / \"以后\" / \"默认\" / \"remember\" / \
                              \"from now on\" / \"my preference is\". Each call writes ONE \
                              entry — don't batch unrelated facts."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "content":  {"type": "string", "description": "The fact, 1-2 sentences, written about the user / project / preference."},
                        "tags":     {"type": "array", "items": {"type": "string"}, "description": "2-5 lowercase keyword tags for retrieval."},
                        "ttl_days": {"type": "integer", "description": "Days to retain. Omit for permanent (stable preferences). Use 7-30 for short-lived task context."}
                    },
                    "required": ["content"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for RememberThisTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Destructive
    }
    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "remember_this".into(),
                reason: "content required".into(),
            })?
            .trim()
            .to_string();
        if content.is_empty() {
            return Err(ToolError::InvalidArgs {
                name: "remember_this".into(),
                reason: "content must not be empty".into(),
            });
        }
        let tags: Vec<String> = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();
        let mut entry = MemoryEntry::new(content.clone())
            .with_source(self.source.clone())
            .with_tags(tags.clone());
        if let Some(days) = args.get("ttl_days").and_then(|v| v.as_u64())
            && days > 0
        {
            entry = entry.with_ttl_days(days as u32);
        }
        self.memory
            .write(entry)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        Ok(ToolResult {
            ok: true,
            content: json!({
                "remembered": content,
                "tags": tags,
            }),
            trace: None,
        })
    }
}

// ───── forget_memory ─────────────────────────────────────────────────────

/// Tool: remove a previously-stored fact by its `id`.
///
/// The `Memory` trait itself doesn't expose deletion (different backends
/// implement it differently — some lookup by id, some by content match).
/// This tool delegates via a runtime check: it tries to recall to confirm
/// the id exists, then asks the backend to delete via downcast. Backends
/// without explicit delete support get a degraded "marked-for-deletion"
/// response so the LLM doesn't keep retrying.
///
/// For typical `FileMemory`-backed setups, wire up `delete_by_id` via the
/// `MemoryDelete` trait (next section) or call `FileMemory::delete_by_id`
/// directly when constructing the tool.
pub struct ForgetMemoryTool {
    deleter: Arc<dyn MemoryDelete>,
    /// Optional recall source. When present, the agent may pass `query` (the
    /// fact in the user's own words) instead of an `id`: the tool resolves it
    /// to the single best-matching entry and deletes that — collapsing the
    /// usual list-then-forget into ONE tool round. Absent → `id` only.
    resolver: Option<Arc<dyn Memory>>,
    schema: ToolSchema,
}

/// Extension trait for memory backends that support row deletion.
///
/// Not part of `harness-core::Memory` because backends differ (file delete
/// is rewrite-all; SQL is `DELETE WHERE id=?`). Apps provide a small
/// adapter that calls the right path.
#[async_trait]
pub trait MemoryDelete: Send + Sync {
    /// Returns `true` if a row was actually removed.
    async fn delete_by_id(&self, id: &str) -> Result<bool, String>;
    /// Returns the number of rows removed (best-effort estimate).
    async fn delete_all(&self) -> Result<u32, String>;
}

/// Adapter so `Arc<FileMemory>` works directly as the deleter for
/// `ForgetMemoryTool`. Most apps will just use:
///
/// ```ignore
/// let fm = Arc::new(FileMemory::open(path)?);
/// let forget = ForgetMemoryTool::new(fm.clone() as Arc<dyn MemoryDelete>);
/// ```
#[async_trait]
impl MemoryDelete for harness_context::FileMemory {
    async fn delete_by_id(&self, id: &str) -> Result<bool, String> {
        harness_context::FileMemory::delete_by_id(self, id).map_err(|e| e.to_string())
    }
    async fn delete_all(&self) -> Result<u32, String> {
        harness_context::FileMemory::delete_all(self).map_err(|e| e.to_string())
    }
}

impl ForgetMemoryTool {
    pub fn new(deleter: Arc<dyn MemoryDelete>) -> Self {
        Self {
            deleter,
            resolver: None,
            schema: Self::build_schema(false),
        }
    }

    /// Attach a recall source so the agent can forget by `query` (the fact
    /// text) in a single call, without first running `list_memories`. The
    /// tool recalls the top match and deletes it. Pass the same `Memory`
    /// the loop uses for recall so the match set is consistent.
    pub fn with_resolver(mut self, memory: Arc<dyn Memory>) -> Self {
        self.resolver = Some(memory);
        self.schema = Self::build_schema(true);
        self
    }

    fn build_schema(with_query: bool) -> ToolSchema {
        if with_query {
            ToolSchema {
                name: "forget_memory".into(),
                description: "Remove a previously-stored fact. Prefer `query`: the fact in the \
                              user's own words (e.g. \"我喜欢的咖啡\" / \"my home address\") — \
                              the single best match is deleted in ONE step, no `list_memories` \
                              needed. Use when the user says \"忘掉 X\" / \"forget that\" / \
                              \"that's wrong\". Pass an exact `id` from list_memories only when \
                              you already have one; if both are given, `id` wins."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The fact to forget, in natural language. Deletes the best match."},
                        "id": {"type": "string", "description": "Exact entry id from list_memories (optional; overrides query)."}
                    }
                }),
            }
        } else {
            ToolSchema {
                name: "forget_memory".into(),
                description: "Remove a previously-stored fact by id. Use when the user says \
                              \"忘掉 X\" / \"forget that\" / \"that's wrong\" about a specific \
                              recalled item. First call `list_memories` to get the id."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Entry id from list_memories."}
                    },
                    "required": ["id"]
                }),
            }
        }
    }
}

#[async_trait]
impl Tool for ForgetMemoryTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Destructive
    }
    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let arg = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        // Resolve to a concrete id. `id` wins; otherwise fall back to `query`
        // (one recall → top hit) when a resolver is wired. This is what lets a
        // delete complete in a single tool round instead of list-then-forget.
        let id = match arg("id") {
            Some(id) => id,
            None => {
                let Some(query) = arg("query") else {
                    return Err(ToolError::InvalidArgs {
                        name: "forget_memory".into(),
                        reason: "provide `id`, or `query` (the fact in natural language)".into(),
                    });
                };
                let Some(resolver) = &self.resolver else {
                    return Err(ToolError::InvalidArgs {
                        name: "forget_memory".into(),
                        reason: "this deployment supports id-only; call list_memories first, then pass id".into(),
                    });
                };
                let hits = resolver
                    .recall(&query, 1)
                    .await
                    .map_err(|e| ToolError::Exec(e.to_string()))?;
                match hits.into_iter().next() {
                    Some(entry) => entry.id,
                    None => {
                        return Ok(ToolResult {
                            ok: false,
                            content: json!({"error": format!("no memory matched query `{query}`")}),
                            trace: None,
                        });
                    }
                }
            }
        };

        let ok = self
            .deleter
            .delete_by_id(&id)
            .await
            .map_err(ToolError::Exec)?;
        Ok(ToolResult {
            ok,
            content: if ok {
                json!({"forgot": id})
            } else {
                json!({"error": format!("no memory with id `{id}`")})
            },
            trace: None,
        })
    }
}

// ───── list_memories ─────────────────────────────────────────────────────

/// Tool: surface what the agent currently knows about the user. Useful
/// when the user asks "what do you remember about me?" — instead of the
/// model guessing from the recall-injected system prompt, it queries
/// explicitly.
pub struct ListMemoriesTool {
    memory: Arc<dyn Memory>,
    schema: ToolSchema,
}

impl ListMemoriesTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            schema: ToolSchema {
                name: "list_memories".into(),
                description: "Recall stored facts. Pass a query string to filter; empty query \
                              returns most-recent first. Use this when the user asks what you \
                              know about them, or before calling `forget_memory`."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "default": "", "description": "Keyword filter; empty returns recent."},
                        "k":     {"type": "integer", "default": 10, "minimum": 1, "maximum": 50}
                    }
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for ListMemoriesTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10).min(50) as usize;
        let hits = self
            .memory
            .recall(&query, k)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        Ok(ToolResult {
            ok: true,
            content: json!({"count": hits.len(), "memories": hits}),
            trace: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::MemoryError;
    use std::sync::Mutex;

    /// Backs both `Memory` (recall/write) and `MemoryDelete` so one instance can
    /// stand in for the FileMemory wiring `ForgetMemoryTool` sees in production.
    #[derive(Default)]
    struct MockMem {
        entries: Mutex<Vec<MemoryEntry>>,
    }

    #[async_trait]
    impl Memory for MockMem {
        async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
            let e = self.entries.lock().unwrap();
            Ok(e.iter()
                .filter(|m| m.content.contains(query))
                .take(k)
                .cloned()
                .collect())
        }
        async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
            self.entries.lock().unwrap().push(entry);
            Ok(())
        }
    }

    #[async_trait]
    impl MemoryDelete for MockMem {
        async fn delete_by_id(&self, id: &str) -> Result<bool, String> {
            let mut e = self.entries.lock().unwrap();
            let before = e.len();
            e.retain(|m| m.id != id);
            Ok(e.len() != before)
        }
        async fn delete_all(&self) -> Result<u32, String> {
            let mut e = self.entries.lock().unwrap();
            let n = e.len() as u32;
            e.clear();
            Ok(n)
        }
    }

    fn entry(id: &str, content: &str) -> MemoryEntry {
        let mut m = MemoryEntry::new(content);
        m.id = id.into();
        m
    }

    fn world() -> World {
        harness_context::with_profile(".", harness_core::UserProfile::default())
    }

    #[tokio::test]
    async fn forget_by_query_resolves_top_match_and_deletes_in_one_call() {
        let mem = Arc::new(MockMem::default());
        mem.entries
            .lock()
            .unwrap()
            .push(entry("a", "我喜欢喝美式咖啡"));
        mem.entries.lock().unwrap().push(entry("b", "我住在上海"));

        let tool = ForgetMemoryTool::new(mem.clone() as Arc<dyn MemoryDelete>)
            .with_resolver(mem.clone() as Arc<dyn Memory>);

        let res = tool
            .invoke(json!({"query": "咖啡"}), &mut world())
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(res.content["forgot"], "a");

        let left = mem.entries.lock().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "b");
    }

    #[tokio::test]
    async fn forget_by_id_still_works_and_wins_over_query() {
        let mem = Arc::new(MockMem::default());
        mem.entries.lock().unwrap().push(entry("a", "咖啡"));
        mem.entries.lock().unwrap().push(entry("b", "茶"));

        let tool = ForgetMemoryTool::new(mem.clone() as Arc<dyn MemoryDelete>)
            .with_resolver(mem.clone() as Arc<dyn Memory>);

        // query would match "a", but an explicit id takes precedence.
        let res = tool
            .invoke(json!({"id": "b", "query": "咖啡"}), &mut world())
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(res.content["forgot"], "b");
    }

    #[tokio::test]
    async fn forget_by_query_reports_miss_without_deleting() {
        let mem = Arc::new(MockMem::default());
        mem.entries.lock().unwrap().push(entry("a", "咖啡"));

        let tool = ForgetMemoryTool::new(mem.clone() as Arc<dyn MemoryDelete>)
            .with_resolver(mem.clone() as Arc<dyn Memory>);

        let res = tool
            .invoke(json!({"query": "不存在的东西"}), &mut world())
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(
            res.content["error"]
                .as_str()
                .unwrap()
                .contains("no memory matched")
        );
        assert_eq!(mem.entries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn query_without_resolver_is_rejected() {
        let mem = Arc::new(MockMem::default());
        let tool = ForgetMemoryTool::new(mem as Arc<dyn MemoryDelete>); // no resolver
        let err = tool
            .invoke(json!({"query": "咖啡"}), &mut world())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
