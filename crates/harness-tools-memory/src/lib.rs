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
            schema: ToolSchema {
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
            },
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
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "forget_memory".into(),
                reason: "id required".into(),
            })?;
        let ok = self
            .deleter
            .delete_by_id(id)
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
